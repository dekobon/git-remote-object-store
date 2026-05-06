//! Packchain `fetch` handler — chain-walk fetch with parallel
//! download / sequential install.
//!
//! Mirrors [`crate::protocol::fetch`] in shape (per-ref tasks bounded
//! by [`crate::protocol::fetch::MAX_FETCH_CONCURRENCY`], session-wide
//! [`FetchedRefs`] dedup, post-batch shallow-boundary writeback) but
//! drives the chain-walk read protocol from issue #64:
//!
//! 1. GET `chain.json` for the requested ref.
//! 2. Walk segments newest → oldest, stopping at the first segment
//!    SHA already present in the local ODB.
//! 3. Download every needed pack in parallel — and the
//!    `<full_at>.bundle` baseline as well, when the walk reached the
//!    chain root without finding a known ancestor.
//! 4. Install **oldest-first** (baseline, then segment[N-1] down to
//!    segment[0]). gix-pack's `TreeAdditionsComparedToAncestor`
//!    packs are self-contained for delta resolution (they include
//!    the ancestor commit + tree alongside the diff blobs), so
//!    install order isn't strictly required for the install step
//!    itself. The reason it matters is **object reachability**:
//!    each segment omits ancestor-only blobs, so a later phase that
//!    walks a tree referenced from a newer segment can only resolve
//!    those blobs if the older segments are already on disk.
//!    Installing oldest-first guarantees that invariant for any
//!    subsequent reader.
//!
//! ## Shallow fetch divergence
//!
//! When `option depth N` is in effect, the parallel-then-install
//! shape above is **not** safe: the user wants the smallest history
//! that satisfies depth=N, which means we should stop downloading
//! after the BFS-from-tip finds a non-empty boundary. Doing parallel
//! downloads up front would defeat the early-termination saving.
//!
//! Phase 3 therefore takes a different shape under `Some(depth)`:
//! download segment[0]'s pack, install it, run
//! [`crate::git::shallow_boundaries`], and stop as soon as the BFS
//! frontier is non-empty. Only walk further segments (and finally
//! the baseline) if shallower depths still leave the BFS empty. A
//! future "speed up packchain shallow fetch" change must NOT
//! re-parallelize: the boundary calculation depends on inspecting the
//! installed objects between segments.
//!
//! ## Stdout discipline
//!
//! Same as the bundle fetch: this handler emits nothing on stdout.
//! The trailing blank-line terminator is the REPL's responsibility
//! (`.claude/rules/protocol-stdout.md`).

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use gix_pack::Find as _;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::debug;

use crate::git::{self, RefName, Sha};
use crate::keys;
use crate::object_store::{GetOpts, ObjectStore, ObjectStoreError};
use crate::protocol::fetch::{
    FetchError, FetchedRefs, MAX_FETCH_CONCURRENCY, ShallowBoundaries, git_dir_for,
    parse_fetch_args,
};

use super::PackchainError;
use super::manifest::load_chain;
use super::schema::{ChainManifest, ChainSegment, Sha40};

/// Drive a batch of `fetch` commands against a packchain bucket.
///
/// Concurrency budget is the shared [`MAX_FETCH_CONCURRENCY`]
/// semaphore; each pack download (and the baseline bundle when
/// applicable) acquires one permit. Refs that resolve to a SHA
/// already in [`FetchedRefs`] short-circuit immediately. After every
/// task drains, accumulated [`ShallowBoundaries`] are merged into
/// `.git/shallow` exactly once.
pub(crate) async fn fetch_batch(
    ctx: &super::super::protocol::BatchCtx,
    cmds: Vec<String>,
    fetched_refs: FetchedRefs,
    depth: Option<NonZeroU32>,
) -> Result<(), FetchError> {
    if cmds.is_empty() {
        return Ok(());
    }
    debug!(
        count = cmds.len(),
        depth = ?depth,
        "fetching packchain refs"
    );

    let semaphore = Arc::new(Semaphore::new(MAX_FETCH_CONCURRENCY));
    let mut tasks: JoinSet<Result<(), FetchError>> = JoinSet::new();
    let prefix = ctx.prefix.clone();
    let boundaries = ShallowBoundaries::new();

    for cmd in cmds {
        let store = Arc::clone(&ctx.store);
        let semaphore = Arc::clone(&semaphore);
        let prefix = prefix.clone();
        let repo_dir = Arc::clone(&ctx.repo_dir);
        let fetched_refs = fetched_refs.clone();
        let boundaries = boundaries.clone();
        tasks.spawn(async move {
            let (sha, ref_name) = parse_fetch_args(&cmd)?;
            fetch_one(FetchOneCtx {
                store,
                semaphore,
                prefix: prefix.as_deref(),
                repo_dir: repo_dir.as_path(),
                sha,
                ref_name: &ref_name,
                fetched_refs: &fetched_refs,
                depth,
                boundaries: &boundaries,
            })
            .await
        });
    }

    // Drain every task before returning so a single failure cannot
    // leave the rest running into a closing helper. First error wins;
    // subsequent errors are logged at debug! so an operator
    // investigating a multi-task failure has the full picture
    // (without bloating the wire-line, which only carries the first).
    let mut first_err: Option<FetchError> = None;
    while let Some(joined) = tasks.join_next().await {
        let res: Result<(), FetchError> = joined.unwrap_or_else(|je| Err(je.into()));
        if let Err(err) = res {
            if first_err.is_none() {
                first_err = Some(err);
            } else {
                debug!(error = %err, "additional packchain fetch task error (first error already captured)");
            }
        }
    }

    if first_err.is_none() && depth.is_some() {
        let collected = boundaries.drain();
        if !collected.is_empty() {
            let git_dir = git_dir_for(ctx.repo_dir.as_path());
            tokio::task::spawn_blocking(move || git::write_shallow_file(&git_dir, &collected))
                .await??;
        }
    }

    first_err.map_or(Ok(()), Err)
}

/// Per-task context for one ref's chain-walk fetch.
struct FetchOneCtx<'a> {
    store: Arc<dyn ObjectStore>,
    semaphore: Arc<Semaphore>,
    prefix: Option<&'a str>,
    repo_dir: &'a Path,
    sha: Sha,
    ref_name: &'a RefName,
    fetched_refs: &'a FetchedRefs,
    depth: Option<NonZeroU32>,
    boundaries: &'a ShallowBoundaries,
}

async fn fetch_one(ctx: FetchOneCtx<'_>) -> Result<(), FetchError> {
    let FetchOneCtx {
        store,
        semaphore,
        prefix,
        repo_dir,
        sha,
        ref_name,
        fetched_refs,
        depth,
        boundaries,
    } = ctx;

    if fetched_refs.contains(&sha) {
        debug!(%sha, ref_name = %ref_name, "skipping fetch: already fetched in this session");
    } else {
        // Load chain.json. None → the bucket has no record of this
        // ref under the packchain engine; surface as a typed
        // PackchainError rather than an opaque NotFound.
        let chain = load_chain(store.as_ref(), prefix, ref_name)
            .await
            .map_err(FetchError::Packchain)?
            .ok_or_else(|| {
                FetchError::Packchain(PackchainError::ChainAbsent {
                    ref_name: ref_name.as_str().to_owned(),
                })
            })?;

        // Walk the chain once here so both the full and shallow paths
        // share the same cut-point analysis. Doing this in
        // spawn_blocking keeps the !Sync `gix::Repository` off any
        // .await in the surrounding task.
        //
        // `??` works because `FetchError: From<JoinError>` (outer
        // task error) and `From<FetchError>` for the inner result is
        // identity — `?` propagates each layer with the appropriate
        // From conversion. The same pattern applies to every
        // spawn_blocking call below.
        let chain_for_walk = chain.clone();
        let repo_dir_owned = repo_dir.to_path_buf();
        let (needed, need_baseline) = tokio::task::spawn_blocking(move || {
            select_needed_segments(&repo_dir_owned, &chain_for_walk)
        })
        .await??;

        let temp_dir = tempfile::Builder::new()
            .prefix("git_remote_object_store_packchain_fetch_")
            .tempdir()?;

        // Resolve the baseline SHA up front. `Sha::from_hex` cannot
        // fail here: `chain.full_at` is a `Sha40`, validated as
        // exactly 40 lowercase-hex bytes at deserialise time
        // (see `schema::Sha40::try_new`). Per
        // `.claude/rules/rust.md`, document the invariant in-place
        // rather than propagating an error path that the type
        // system already rules out.
        let baseline_sha = need_baseline.then(|| {
            Sha::from_hex(chain.full_at.as_str())
                .expect("chain.full_at is a Sha40 — guaranteed 40 lowercase hex bytes")
        });

        if let Some(depth) = depth {
            // Shallow path: sequential newest-first download + install
            // + BFS-after-each. Stops as soon as the boundary set is
            // non-empty, so deeper segments and the baseline never
            // leave the bucket. See the module doc on why this can't
            // be parallelised.
            fetch_shallow(
                store.as_ref(),
                prefix,
                repo_dir,
                temp_dir.path(),
                ref_name,
                sha,
                &needed,
                baseline_sha,
                depth,
            )
            .await?;
        } else {
            // Full path: parallel-download every needed pack
            // (and the baseline when applicable), install
            // oldest-first.
            fetch_full(
                &store,
                &semaphore,
                prefix,
                repo_dir,
                temp_dir.path(),
                ref_name,
                &needed,
                baseline_sha,
            )
            .await?;
        }
        fetched_refs.insert(sha);
    }

    // Shallow boundary collection runs even when the chain was
    // already in this session's `FetchedRefs` — `depth` is set
    // per-batch and the BFS depends on `depth`, not on whether we
    // touched the network. Walk the local objects either way.
    if let Some(depth) = depth {
        let repo_dir = repo_dir.to_path_buf();
        let ids = tokio::task::spawn_blocking(move || {
            let repo = gix::open(&repo_dir).map_err(crate::git::GitError::from)?;
            git::shallow_boundaries(&repo, sha, depth)
        })
        .await??;
        boundaries.extend(ids);
    }
    Ok(())
}

/// Walk the chain newest-first, stopping at the first segment whose
/// SHA is already in the local ODB. Returns the segments that are
/// missing (in the same newest-first order as `chain.segments`) and a
/// flag for whether we walked all the way to the root (i.e. the
/// receiver has no anchor and the baseline must also be installed).
fn select_needed_segments(
    repo_dir: &Path,
    chain: &ChainManifest,
) -> Result<(Vec<ChainSegment>, bool), FetchError> {
    let repo = gix::open(repo_dir).map_err(crate::git::GitError::from)?;
    let odb = repo.objects.clone().into_inner();
    let mut needed: Vec<ChainSegment> = Vec::new();
    for segment in &chain.segments {
        let oid = sha40_to_object_id(&segment.sha);
        if odb.contains(&oid) {
            // We already have this segment's tip; everything older is
            // also present. Stop walking.
            return Ok((needed, false));
        }
        needed.push(segment.clone());
    }
    // Walked the entire chain without finding a known ancestor — the
    // baseline bundle is needed too.
    Ok((needed, true))
}

/// Convert a [`Sha40`] (always 40 lowercase hex) to a
/// [`gix_hash::ObjectId`].
///
/// Infallible by construction: `Sha40::try_new` validates the
/// 40-lowercase-hex shape at deserialise time, so `Sha::from_hex`
/// always succeeds here. Document the invariant in-place per
/// `.claude/rules/rust.md` rather than threading a `Result` that
/// no caller can usefully act on.
fn sha40_to_object_id(sha: &Sha40) -> gix_hash::ObjectId {
    *Sha::from_hex(sha.as_str())
        .expect("Sha40 is 40-lowercase-hex by construction")
        .as_object_id()
}

/// Full-fetch path: parallel download, sequential install. The
/// caller pre-computes `(needed, baseline_sha)` once via
/// [`select_needed_segments`] (`baseline_sha` is `Some` iff the chain
/// walk reached the root without finding a known ancestor).
#[allow(clippy::too_many_arguments)] // bundles every borrow this fn needs; the alternative is a per-fn ctx struct
async fn fetch_full(
    store: &Arc<dyn ObjectStore>,
    semaphore: &Arc<Semaphore>,
    prefix: Option<&str>,
    repo_dir: &Path,
    temp_path: &Path,
    ref_name: &RefName,
    needed: &[ChainSegment],
    baseline_sha: Option<Sha>,
) -> Result<(), FetchError> {
    if needed.is_empty() && baseline_sha.is_none() {
        // Receiver already has the chain.tip — nothing to do beyond
        // marking the SHA in `FetchedRefs` (caller does that).
        debug!(ref_name = %ref_name, "packchain fetch: receiver already up to date");
        return Ok(());
    }

    // Stage all downloads (segment packs + baseline) in parallel.
    // Each download takes one permit so the global concurrency cap
    // applies across refs.
    let mut downloads: JoinSet<Result<DownloadedArtifact, FetchError>> = JoinSet::new();
    for segment in needed {
        let store = Arc::clone(store);
        let permit_pool = Arc::clone(semaphore);
        let key = super::keys::packs_key_with_prefix(prefix, &segment.pack);
        let dest = temp_path.join(format!("{}.pack", segment.sha.as_str()));
        let segment_clone = segment.clone();
        downloads.spawn(async move {
            let _permit = permit_pool
                .acquire_owned()
                .await
                .expect("fetch semaphore is owned by this batch and never closed");
            download_pack(store.as_ref(), &key, &dest).await?;
            Ok(DownloadedArtifact::Segment {
                segment: segment_clone,
                pack_path: dest,
            })
        });
    }
    if let Some(baseline_sha) = baseline_sha {
        let key = keys::bundle_key(prefix, ref_name, baseline_sha);
        let dest = temp_path.join(format!("{baseline_sha}.bundle"));
        let store = Arc::clone(store);
        let permit_pool = Arc::clone(semaphore);
        downloads.spawn(async move {
            let _permit = permit_pool
                .acquire_owned()
                .await
                .expect("fetch semaphore is owned by this batch and never closed");
            download_baseline(store.as_ref(), &key, &dest).await?;
            Ok(DownloadedArtifact::Baseline {
                sha: baseline_sha,
                bundle_path: dest,
            })
        });
    }

    // Drain downloads. Collect into segment-keyed map for ordered
    // install; surface the first error after every task drains.
    // Subsequent errors are logged at debug! so multi-task failures
    // remain visible to operators (the wire only renders the first).
    let mut downloaded_segments: std::collections::HashMap<Sha40, PathBuf> =
        std::collections::HashMap::with_capacity(needed.len());
    let mut downloaded_baseline: Option<(Sha, PathBuf)> = None;
    let mut first_err: Option<FetchError> = None;
    while let Some(joined) = downloads.join_next().await {
        let res: Result<DownloadedArtifact, FetchError> =
            joined.unwrap_or_else(|je| Err(je.into()));
        match res {
            Ok(DownloadedArtifact::Segment { segment, pack_path }) => {
                downloaded_segments.insert(segment.sha, pack_path);
            }
            Ok(DownloadedArtifact::Baseline { sha, bundle_path }) => {
                downloaded_baseline = Some((sha, bundle_path));
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                } else {
                    debug!(error = %e, "additional packchain download error (first error already captured)");
                }
            }
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }

    // Install oldest-first: baseline (if any), then segment[N-1] down
    // to segment[0]. Each install runs in spawn_blocking because
    // gix's pack writers block on disk I/O and aren't `async`.
    if let Some((sha, _bundle_path)) = downloaded_baseline {
        // _bundle_path is rooted in `temp_path`; tempdir cleanup drops
        // the file after this scope. `git::unbundle_at` clones cwd /
        // folder / ref_name internally before its spawn_blocking, so
        // there's no need to pre-own them here.
        git::unbundle_at(repo_dir, temp_path, sha, ref_name).await?;
    }
    for segment in needed.iter().rev() {
        let pack_path = downloaded_segments.remove(&segment.sha).ok_or_else(|| {
            FetchError::Packchain(PackchainError::PackBuild(
                "segment download succeeded but path is missing".to_owned(),
            ))
        })?;
        let repo_dir = repo_dir.to_path_buf();
        tokio::task::spawn_blocking(move || install_pack(&repo_dir, &pack_path)).await??;
    }
    Ok(())
}

/// Shallow-fetch path: sequential newest-first install with
/// BFS-after-each, terminating as soon as a non-empty boundary
/// surfaces. If the chain is exhausted without a boundary, fall
/// through to the baseline (when `baseline_sha` is `Some`).
#[allow(clippy::too_many_arguments)] // mirrors `fetch_full`'s shape; refactoring to a struct would obscure the call site
async fn fetch_shallow(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    repo_dir: &Path,
    temp_path: &Path,
    ref_name: &RefName,
    tip_sha: Sha,
    needed: &[ChainSegment],
    baseline_sha: Option<Sha>,
    depth: NonZeroU32,
) -> Result<(), FetchError> {
    for segment in needed {
        let key = super::keys::packs_key_with_prefix(prefix, &segment.pack);
        let dest = temp_path.join(format!("{}.pack", segment.sha.as_str()));
        download_pack(store, &key, &dest).await?;
        let repo_dir_clone = repo_dir.to_path_buf();
        let pack_path = dest;
        tokio::task::spawn_blocking(move || install_pack(&repo_dir_clone, &pack_path)).await??;

        // BFS from the tip. Empty result → frontier hasn't been
        // reached yet; install the next-older segment. Non-empty →
        // we have enough commits; do not download more.
        let repo_dir_clone = repo_dir.to_path_buf();
        let ids = tokio::task::spawn_blocking(move || {
            let repo = gix::open(&repo_dir_clone).map_err(crate::git::GitError::from)?;
            git::shallow_boundaries(&repo, tip_sha, depth)
        })
        .await??;
        if !ids.is_empty() {
            // The post-batch shallow-write step (in fetch_batch) will
            // re-walk and merge. We don't push these into `boundaries`
            // here because the per-ref shallow walk at the end of
            // `fetch_one` does exactly that work, and double-walking
            // would double-count.
            return Ok(());
        }
    }

    // Chain exhausted without a boundary. If we have a known
    // ancestor inside the chain (`baseline_sha` is None) the local
    // ODB already covered everything older than the chain — depth=N
    // can't see further than what we have. Otherwise install the
    // baseline so the BFS has a complete graph to walk.
    if let Some(baseline_sha) = baseline_sha {
        let key = keys::bundle_key(prefix, ref_name, baseline_sha);
        let dest = temp_path.join(format!("{baseline_sha}.bundle"));
        download_baseline(store, &key, &dest).await?;
        git::unbundle_at(repo_dir, temp_path, baseline_sha, ref_name).await?;
    }
    Ok(())
}

/// One downloaded artefact ready for installation. Ordered installs
/// happen in [`fetch_full`] after all downloads drain.
enum DownloadedArtifact {
    Segment {
        segment: ChainSegment,
        pack_path: PathBuf,
    },
    Baseline {
        sha: Sha,
        bundle_path: PathBuf,
    },
}

/// Stream a pack body to `dest`, mapping `NotFound` to a typed
/// `PackMissing` error so the operator sees which pack the chain
/// pointed at.
async fn download_pack(store: &dyn ObjectStore, key: &str, dest: &Path) -> Result<(), FetchError> {
    match store.get_to_file(key, dest, GetOpts::default()).await {
        Ok(()) => Ok(()),
        Err(ObjectStoreError::NotFound(_)) => {
            Err(FetchError::Packchain(PackchainError::PackMissing {
                key: key.to_owned(),
            }))
        }
        Err(e) => Err(FetchError::Store(e)),
    }
}

/// Stream the baseline bundle, mapping `NotFound` to a typed
/// `BaselineMissing` error.
async fn download_baseline(
    store: &dyn ObjectStore,
    key: &str,
    dest: &Path,
) -> Result<(), FetchError> {
    match store.get_to_file(key, dest, GetOpts::default()).await {
        Ok(()) => Ok(()),
        Err(ObjectStoreError::NotFound(_)) => {
            Err(FetchError::Packchain(PackchainError::BaselineMissing {
                key: key.to_owned(),
            }))
        }
        Err(e) => Err(FetchError::Store(e)),
    }
}

/// Install a packchain pack file into the destination repo's
/// `objects/pack` directory. Mirrors the unbundle path in
/// [`crate::bundle::unbundle`] but operates on a raw PACK file (no
/// bundle v2 header) — packchain packs are bare per the on-bucket
/// schema.
///
/// `pub(crate)` so [`super::compact`] can drive the same install
/// pipeline against its temp repo without going through the
/// helper-protocol fetch path.
pub(crate) fn install_pack(repo_dir: &Path, pack_path: &Path) -> Result<(), FetchError> {
    use std::fs;
    use std::io::BufReader;

    let repo = gix::open(repo_dir).map_err(crate::git::GitError::from)?;
    let pack_dir = repo.git_dir().join("objects/pack");
    fs::create_dir_all(&pack_dir).map_err(FetchError::Io)?;

    let pack_file = fs::File::open(pack_path).map_err(FetchError::Io)?;
    let mut reader = BufReader::new(pack_file);

    // Pass the dst ODB as the thin-pack resolver so deltas in this
    // pack can be resolved against objects from earlier-installed
    // packs (the install order in `fetch_full` / `fetch_shallow`
    // guarantees the bases are already on disk).
    let interrupted = AtomicBool::new(false);
    let outcome = gix_pack::Bundle::write_to_directory(
        &mut reader,
        Some(&pack_dir),
        &mut gix::progress::Discard,
        &interrupted,
        Some(repo.objects.clone().into_inner()),
        gix_pack::bundle::write::Options {
            object_hash: gix_hash::Kind::Sha1,
            ..Default::default()
        },
    )
    .map_err(|e| FetchError::Packchain(PackchainError::PackIndexWrite(Box::new(e))))?;

    // gix writes a `.keep` to prevent git-gc from reaping the new
    // pack before refs land. The remote helper exits before git
    // updates refs, and `git gc --auto` is a synchronous post-fetch
    // step (not a daemon), so we can drop the `.keep` immediately
    // (mirroring src/bundle.rs:332-337).
    if let Some(keep_path) = outcome.keep_path
        && let Err(e) = fs::remove_file(&keep_path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(FetchError::Io(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::mock::MockStore;
    use crate::packchain::keys::chain_key;
    use bytes::Bytes;

    fn ref_main() -> RefName {
        RefName::new("refs/heads/main").expect("ref")
    }

    #[tokio::test]
    async fn select_needed_segments_returns_all_for_empty_repo() {
        // A freshly-init'd repo contains no packchain commits, so
        // every segment is "needed" and the baseline must come too.
        let repo_dir = tempfile::tempdir().unwrap();
        gix::init(repo_dir.path()).unwrap();

        let chain = ChainManifest {
            v: 1,
            tip: Sha40::try_new("0000000000000000000000000000000000000001").unwrap(),
            full_at: Sha40::try_new("0000000000000000000000000000000000000001").unwrap(),
            segments: vec![ChainSegment {
                sha: Sha40::try_new("0000000000000000000000000000000000000001").unwrap(),
                parent_sha: None,
                pack: "packs/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.pack".to_owned(),
                bytes: 1_024,
            }],
        };
        let (needed, need_baseline) =
            select_needed_segments(repo_dir.path(), &chain).expect("walk");
        assert_eq!(needed.len(), 1);
        assert!(need_baseline);
    }

    #[tokio::test]
    async fn select_needed_segments_stops_at_first_known_ancestor() {
        // Build a fixture repo that contains a real commit `c1`, then
        // construct a 2-segment chain where segments[1].sha == c1.
        // The walk MUST iterate segments[0] (not in ODB → push to
        // needed), then segments[1] (in ODB → stop, return false for
        // need_baseline). Pins the partial-walk early-exit branch
        // that no other unit test currently covers.
        use gix::actor::SignatureRef;
        use gix::bstr::BStr;
        use gix_hash::ObjectId;

        let repo_dir = tempfile::tempdir().unwrap();
        let repo = gix::init(repo_dir.path()).unwrap();
        let signature = SignatureRef {
            name: BStr::new("Tester"),
            email: BStr::new("t@example.com"),
            time: "0 +0000",
        };
        let blob = repo.write_blob(b"v1").unwrap().detach();
        let tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![gix::objs::tree::Entry {
                    mode: gix::objs::tree::EntryKind::Blob.into(),
                    filename: "a.txt".into(),
                    oid: blob,
                }],
            })
            .unwrap()
            .detach();
        let c1_oid = repo
            .commit_as(
                signature,
                signature,
                "refs/heads/main",
                "first",
                tree,
                std::iter::empty::<ObjectId>(),
            )
            .unwrap()
            .detach();
        let c1_sha40 = Sha40::try_new(c1_oid.to_string()).unwrap();
        // segments[0] is a fictional newer commit (NOT in ODB).
        let newer = Sha40::try_new("ffffffffffffffffffffffffffffffffffffffff").unwrap();

        let chain = ChainManifest {
            v: 1,
            tip: newer.clone(),
            full_at: c1_sha40.clone(),
            segments: vec![
                ChainSegment {
                    sha: newer.clone(),
                    parent_sha: Some(c1_sha40.clone()),
                    pack: "packs/0000000000000000000000000000000000000001.pack".to_owned(),
                    bytes: 1_024,
                },
                ChainSegment {
                    sha: c1_sha40.clone(),
                    parent_sha: None,
                    pack: "packs/0000000000000000000000000000000000000002.pack".to_owned(),
                    bytes: 2_048,
                },
            ],
        };
        let (needed, need_baseline) =
            select_needed_segments(repo_dir.path(), &chain).expect("walk");
        assert_eq!(
            needed.len(),
            1,
            "walk must stop at the known ancestor; only segments[0] is needed",
        );
        assert_eq!(needed[0].sha, newer);
        assert!(
            !need_baseline,
            "baseline is NOT needed when the walk found a known ancestor mid-chain",
        );
    }

    #[tokio::test]
    async fn fetch_returns_chain_absent_when_chain_missing() {
        // Drive `fetch_one` against a MockStore that has no chain.json
        // for the ref. The error must surface as ChainAbsent (typed),
        // not Store(NotFound).
        let repo_dir = tempfile::tempdir().unwrap();
        gix::init(repo_dir.path()).unwrap();

        let store: Arc<dyn ObjectStore> = Arc::new(MockStore::new());
        let semaphore = Arc::new(Semaphore::new(MAX_FETCH_CONCURRENCY));
        let fetched_refs = FetchedRefs::new();
        let boundaries = ShallowBoundaries::new();
        let ref_name = ref_main();
        let bogus_sha = Sha::from_hex("0000000000000000000000000000000000000001").unwrap();

        let result = fetch_one(FetchOneCtx {
            store,
            semaphore,
            prefix: Some("repo"),
            repo_dir: repo_dir.path(),
            sha: bogus_sha,
            ref_name: &ref_name,
            fetched_refs: &fetched_refs,
            depth: None,
            boundaries: &boundaries,
        })
        .await;
        match result {
            Err(FetchError::Packchain(PackchainError::ChainAbsent { ref_name: r })) => {
                assert_eq!(r, "refs/heads/main");
            }
            other => panic!("expected ChainAbsent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_surfaces_pack_missing_when_chain_references_absent_pack() {
        // Pre-seed chain.json that points at a pack key the bucket
        // doesn't have. The fetch must surface PackMissing — issue
        // #64's regression criterion: "fail loud, not silent
        // zero-byte fetch".
        let repo_dir = tempfile::tempdir().unwrap();
        gix::init(repo_dir.path()).unwrap();

        let store_inner = MockStore::new();
        let chain = ChainManifest {
            v: 1,
            tip: Sha40::try_new("1111111111111111111111111111111111111111").unwrap(),
            full_at: Sha40::try_new("1111111111111111111111111111111111111111").unwrap(),
            segments: vec![ChainSegment {
                sha: Sha40::try_new("1111111111111111111111111111111111111111").unwrap(),
                parent_sha: None,
                pack: "packs/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.pack".to_owned(),
                bytes: 1_024,
            }],
        };
        store_inner.insert(
            chain_key(Some("repo"), ref_main()),
            Bytes::from(chain.to_json_pretty().unwrap()),
        );
        let store: Arc<dyn ObjectStore> = Arc::new(store_inner);

        let semaphore = Arc::new(Semaphore::new(MAX_FETCH_CONCURRENCY));
        let fetched_refs = FetchedRefs::new();
        let boundaries = ShallowBoundaries::new();
        let ref_name = ref_main();
        let tip_sha = Sha::from_hex("1111111111111111111111111111111111111111").unwrap();

        let result = fetch_one(FetchOneCtx {
            store,
            semaphore,
            prefix: Some("repo"),
            repo_dir: repo_dir.path(),
            sha: tip_sha,
            ref_name: &ref_name,
            fetched_refs: &fetched_refs,
            depth: None,
            boundaries: &boundaries,
        })
        .await;
        match result {
            Err(FetchError::Packchain(PackchainError::PackMissing { key })) => {
                assert!(
                    key.contains("packs/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.pack"),
                    "key should name the absent pack, got: {key}",
                );
            }
            other => panic!("expected PackMissing, got {other:?}"),
        }
    }
}

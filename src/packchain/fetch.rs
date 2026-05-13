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
use tracing::{debug, warn};

use crate::git::{self, RefName, Sha};
use crate::keys;
use crate::object_store::{GetOpts, ObjectStore, ObjectStoreError};
use crate::protocol::fetch::{
    FetchError, FetchedRefs, MAX_FETCH_CONCURRENCY, ShallowBoundaries, parse_fetch_args,
};

use super::PackchainError;
use super::manifest::load_chain;
use super::retry::{
    PACK_MISSING_MAX_RETRIES, PACK_MISSING_RETRY_BACKOFFS, chain_references_pack_key,
};
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

    // Run unconditionally on success even with empty `collected`: the
    // rewrite must prune stale pre-existing entries whose parents have
    // just landed in the ODB (a deepen-to-full history fetch shrinks
    // the file to nothing and unlinks it).
    if first_err.is_none() && depth.is_some() {
        let collected = boundaries.drain();
        let repo_dir = ctx.repo_dir.as_path().to_path_buf();
        tokio::task::spawn_blocking(move || git::write_shallow_file(&repo_dir, &collected))
            .await??;
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
        fetch_with_pack_missing_retries(&store, &semaphore, prefix, repo_dir, ref_name, sha, depth)
            .await?;
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

/// Chain-walk-and-fetch driver wrapped in a `PackMissing` retry loop.
///
/// Loads `chain.json`, walks it to pick needed segments, then drives
/// either [`fetch_full`] (no depth) or [`fetch_shallow`] (with depth).
/// On a [`PackchainError::PackMissing`] caused by a concurrent
/// `manage gc sweep` (detected by reloading `chain.json` and observing
/// that the failing pack key is no longer referenced), waits a backoff
/// and retries against the fresh chain. After
/// [`PACK_MISSING_MAX_RETRIES`] retries gives up with
/// [`PackchainError::ConcurrentGcRetriesExhausted`] (issue #148, the
/// fetch-side analogue of issue #136's read-side retry).
///
/// Non-`PackMissing` errors (transport faults, malformed pack,
/// `BaselineMissing`, etc.) pass through immediately — they are not
/// retryable in this fashion and would otherwise wait through the
/// backoff schedule for no reason. A `chain.json` that vanishes
/// mid-fetch (the ref was deleted) surfaces as
/// [`PackchainError::ChainAbsent`], not as `PackMissing`.
///
/// Already-installed packs from a previous attempt are NOT
/// redownloaded under the shallow path: each install lands in the
/// destination repo's ODB, and the next iteration's chain walk stops
/// at the first known ancestor. Under the full path nothing installs
/// until all parallel downloads succeed, so a mid-fetch retry does
/// re-download the survivors — acceptable cost for a rare race that
/// only fires when compact+sweep keeps outpacing the receiver.
#[allow(clippy::too_many_arguments)] // mirrors fetch_one's existing call shape
async fn fetch_with_pack_missing_retries(
    store: &Arc<dyn ObjectStore>,
    semaphore: &Arc<Semaphore>,
    prefix: Option<&str>,
    repo_dir: &Path,
    ref_name: &RefName,
    sha: Sha,
    depth: Option<NonZeroU32>,
) -> Result<(), FetchError> {
    let mut attempt: u32 = 0;
    loop {
        let result = fetch_once(store, semaphore, prefix, repo_dir, ref_name, sha, depth).await;
        let missing_key = match result {
            Ok(()) => return Ok(()),
            Err(FetchError::Packchain(PackchainError::PackMissing { key })) => key,
            Err(e) => return Err(e),
        };
        // PackMissing — distinguish concurrent GC (retryable) from
        // genuine bucket inconsistency (data loss → fail fast).
        // `load_chain` returning `Ok(None)` means the ref vanished
        // mid-fetch — surface as ChainAbsent, NOT as PackMissing.
        let reloaded = load_chain(store.as_ref(), prefix, ref_name)
            .await
            .map_err(FetchError::Packchain)?
            .ok_or_else(|| {
                FetchError::Packchain(PackchainError::ChainAbsent {
                    ref_name: ref_name.as_str().to_owned(),
                })
            })?;
        if chain_references_pack_key(&reloaded, prefix, &missing_key)
            .map_err(FetchError::Packchain)?
        {
            // The reloaded chain still names this pack — the bucket
            // is genuinely missing data the chain still references.
            // Surface the original PackMissing without retrying.
            return Err(FetchError::Packchain(PackchainError::PackMissing {
                key: missing_key,
            }));
        }
        if attempt >= PACK_MISSING_MAX_RETRIES {
            warn!(
                ref_name = %ref_name.as_str(),
                last_missing_key = %missing_key,
                attempts = attempt,
                "fetch: exhausted pack-missing retries against concurrent GC"
            );
            return Err(FetchError::Packchain(
                PackchainError::ConcurrentGcRetriesExhausted {
                    last_missing_key: missing_key,
                    attempts: attempt,
                },
            ));
        }
        debug!(
            ref_name = %ref_name.as_str(),
            missing_key = %missing_key,
            attempt = attempt,
            "fetch: PackMissing on chain no longer references the pack — retrying after GC race"
        );
        tokio::time::sleep(PACK_MISSING_RETRY_BACKOFFS[attempt as usize]).await;
        attempt += 1;
    }
}

/// One attempt at the chain-walk + parallel-or-sequential download +
/// install pipeline. Factored out of `fetch_one` so
/// [`fetch_with_pack_missing_retries`] can iterate it; each attempt
/// re-reads `chain.json`, re-walks the local ODB (so previously
/// installed segments are skipped under the shallow path), and
/// allocates its own temp directory so a failed run leaves nothing on
/// disk.
async fn fetch_once(
    store: &Arc<dyn ObjectStore>,
    semaphore: &Arc<Semaphore>,
    prefix: Option<&str>,
    repo_dir: &Path,
    ref_name: &RefName,
    sha: Sha,
    depth: Option<NonZeroU32>,
) -> Result<(), FetchError> {
    // Load chain.json. None → the bucket has no record of this ref
    // under the packchain engine; surface as a typed PackchainError
    // rather than an opaque NotFound.
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
            store,
            semaphore,
            prefix,
            repo_dir,
            temp_dir.path(),
            ref_name,
            &needed,
            baseline_sha,
        )
        .await?;
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
        // Validate `segment.pack` before deriving a bucket key — a
        // crafted chain.json could otherwise drive
        // `pack_key_from_relative` to request an arbitrary bucket key
        // (issue #120). Shared with read/compact/gc via
        // `segment_pack_sha`.
        let content_sha = super::keys::segment_pack_sha(segment).map_err(FetchError::Packchain)?;
        let store = Arc::clone(store);
        let permit_pool = Arc::clone(semaphore);
        let key = super::keys::pack_key_from_relative(prefix, &segment.pack);
        let dest = temp_path.join(format!("{}.pack", content_sha.as_str()));
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
        // Validate the bucket-relative pack key before composing the
        // GET key; see the matching guard in `fetch_full` and issue #120.
        let content_sha = super::keys::segment_pack_sha(segment).map_err(FetchError::Packchain)?;
        let key = super::keys::pack_key_from_relative(prefix, &segment.pack);
        let dest = temp_path.join(format!("{}.pack", content_sha.as_str()));
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
        let c1_sha40 = Sha40::from_oid(&c1_oid).unwrap();
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

    /// Helper for the malformed-pack-key tests: build a `ChainSegment`
    /// whose `pack` field is `pack`. The other fields use stable dummy
    /// values; the validation under test only inspects `pack`.
    fn segment_with_pack(pack: &str) -> ChainSegment {
        ChainSegment {
            sha: Sha40::try_new("2222222222222222222222222222222222222222").unwrap(),
            parent_sha: None,
            pack: pack.to_owned(),
            bytes: 1_024,
        }
    }

    /// Drive `fetch_full` directly with a single malformed-pack-key
    /// segment and assert the error variant. Centralises the
    /// boilerplate so the three regression cases stay declarative.
    async fn assert_fetch_full_rejects(pack: &str) {
        let repo_dir = tempfile::tempdir().unwrap();
        gix::init(repo_dir.path()).unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(MockStore::new());
        let semaphore = Arc::new(Semaphore::new(MAX_FETCH_CONCURRENCY));
        let needed = vec![segment_with_pack(pack)];

        let result = fetch_full(
            &store,
            &semaphore,
            Some("repo"),
            repo_dir.path(),
            temp_dir.path(),
            &ref_main(),
            &needed,
            None,
        )
        .await;
        match result {
            Err(FetchError::Packchain(PackchainError::MalformedPackEntry {
                offset: 0,
                reason,
            })) => {
                assert!(
                    reason.contains("is not of the form"),
                    "reason should describe the expected shape, got: {reason}",
                );
                if !pack.is_empty() {
                    assert!(
                        reason.contains(pack),
                        "reason should also name the offending key, got: {reason}",
                    );
                }
            }
            other => panic!("expected MalformedPackEntry for pack `{pack}`, got {other:?}"),
        }
    }

    /// Drive `fetch_shallow` directly with a single malformed-pack-key
    /// segment and assert the error variant.
    async fn assert_fetch_shallow_rejects(pack: &str) {
        let repo_dir = tempfile::tempdir().unwrap();
        gix::init(repo_dir.path()).unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let store = MockStore::new();
        let needed = vec![segment_with_pack(pack)];
        let tip_sha = Sha::from_hex("2222222222222222222222222222222222222222").unwrap();
        let depth = NonZeroU32::new(1).unwrap();

        let result = fetch_shallow(
            &store,
            Some("repo"),
            repo_dir.path(),
            temp_dir.path(),
            &ref_main(),
            tip_sha,
            &needed,
            None,
            depth,
        )
        .await;
        match result {
            Err(FetchError::Packchain(PackchainError::MalformedPackEntry {
                offset: 0,
                reason,
            })) => {
                assert!(
                    reason.contains("is not of the form"),
                    "reason should describe the expected shape, got: {reason}",
                );
                if !pack.is_empty() {
                    assert!(
                        reason.contains(pack),
                        "reason should also name the offending key, got: {reason}",
                    );
                }
            }
            other => panic!("expected MalformedPackEntry for pack `{pack}`, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_full_rejects_empty_pack_key() {
        assert_fetch_full_rejects("").await;
    }

    #[tokio::test]
    async fn fetch_full_rejects_unstructured_pack_key() {
        assert_fetch_full_rejects("wrong").await;
    }

    #[tokio::test]
    async fn fetch_full_rejects_non_hex_pack_key() {
        assert_fetch_full_rejects("packs/notahex.pack").await;
    }

    #[tokio::test]
    async fn fetch_shallow_rejects_empty_pack_key() {
        assert_fetch_shallow_rejects("").await;
    }

    #[tokio::test]
    async fn fetch_shallow_rejects_unstructured_pack_key() {
        assert_fetch_shallow_rejects("wrong").await;
    }

    #[tokio::test]
    async fn fetch_shallow_rejects_non_hex_pack_key() {
        assert_fetch_shallow_rejects("packs/notahex.pack").await;
    }

    // -- Pack-missing retry (issue #148) --------------------------

    /// Test-only [`ObjectStore`] decorator: on each `get_bytes` against
    /// `chain_key`, serves the next body from `bodies` (saturating at
    /// the last entry once exhausted) so a fetch retry observes an
    /// evolving on-bucket chain without racing a real concurrent
    /// writer. Mirrors `read::tests::EvolvingChainStore`.
    struct EvolvingChainStore {
        inner: MockStore,
        chain_key: String,
        bodies: std::sync::Mutex<Vec<Bytes>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl EvolvingChainStore {
        fn new(inner: MockStore, chain_key: String, bodies: Vec<Bytes>) -> Self {
            assert!(!bodies.is_empty(), "must supply at least one chain body");
            Self {
                inner,
                chain_key,
                bodies: std::sync::Mutex::new(bodies),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn chain_calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for EvolvingChainStore {
        async fn list(
            &self,
            prefix: &str,
        ) -> Result<Vec<crate::object_store::ObjectMeta>, ObjectStoreError> {
            self.inner.list(prefix).await
        }
        async fn get_to_file(
            &self,
            key: &str,
            dest: &Path,
            opts: GetOpts,
        ) -> Result<(), ObjectStoreError> {
            self.inner.get_to_file(key, dest, opts).await
        }
        async fn get_bytes(&self, key: &str) -> Result<Bytes, ObjectStoreError> {
            if key == self.chain_key {
                let idx = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let guard = self.bodies.lock().unwrap();
                let pick = idx.min(guard.len() - 1);
                return Ok(guard[pick].clone());
            }
            self.inner.get_bytes(key).await
        }
        async fn get_bytes_range(
            &self,
            key: &str,
            range: std::ops::Range<u64>,
        ) -> Result<Bytes, ObjectStoreError> {
            self.inner.get_bytes_range(key, range).await
        }
        async fn put_bytes(
            &self,
            key: &str,
            body: Bytes,
            opts: crate::object_store::PutOpts,
        ) -> Result<(), ObjectStoreError> {
            self.inner.put_bytes(key, body, opts).await
        }
        async fn put_path(
            &self,
            key: &str,
            src: &Path,
            opts: crate::object_store::PutOpts,
        ) -> Result<(), ObjectStoreError> {
            self.inner.put_path(key, src, opts).await
        }
        async fn put_if_absent(&self, key: &str, body: Bytes) -> Result<bool, ObjectStoreError> {
            self.inner.put_if_absent(key, body).await
        }
        async fn head(
            &self,
            key: &str,
        ) -> Result<crate::object_store::ObjectMeta, ObjectStoreError> {
            self.inner.head(key).await
        }
        async fn copy(&self, src: &str, dst: &str) -> Result<(), ObjectStoreError> {
            self.inner.copy(src, dst).await
        }
        async fn delete(&self, key: &str) -> Result<(), ObjectStoreError> {
            self.inner.delete(key).await
        }
    }

    /// Variant of [`EvolvingChainStore`] that returns `NotFound` on the
    /// SECOND `get_bytes(chain_key)` — simulates the ref being deleted
    /// (chain.json vanishes) between the initial load and the retry
    /// reload. The first call serves the seeded body so the fetch path
    /// enters its download attempt before the disappearance.
    struct VanishingChainStore {
        inner: MockStore,
        chain_key: String,
        initial: Bytes,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ObjectStore for VanishingChainStore {
        async fn list(
            &self,
            prefix: &str,
        ) -> Result<Vec<crate::object_store::ObjectMeta>, ObjectStoreError> {
            self.inner.list(prefix).await
        }
        async fn get_to_file(
            &self,
            key: &str,
            dest: &Path,
            opts: GetOpts,
        ) -> Result<(), ObjectStoreError> {
            self.inner.get_to_file(key, dest, opts).await
        }
        async fn get_bytes(&self, key: &str) -> Result<Bytes, ObjectStoreError> {
            if key == self.chain_key {
                let idx = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if idx == 0 {
                    return Ok(self.initial.clone());
                }
                return Err(ObjectStoreError::NotFound(key.to_owned()));
            }
            self.inner.get_bytes(key).await
        }
        async fn get_bytes_range(
            &self,
            key: &str,
            range: std::ops::Range<u64>,
        ) -> Result<Bytes, ObjectStoreError> {
            self.inner.get_bytes_range(key, range).await
        }
        async fn put_bytes(
            &self,
            key: &str,
            body: Bytes,
            opts: crate::object_store::PutOpts,
        ) -> Result<(), ObjectStoreError> {
            self.inner.put_bytes(key, body, opts).await
        }
        async fn put_path(
            &self,
            key: &str,
            src: &Path,
            opts: crate::object_store::PutOpts,
        ) -> Result<(), ObjectStoreError> {
            self.inner.put_path(key, src, opts).await
        }
        async fn put_if_absent(&self, key: &str, body: Bytes) -> Result<bool, ObjectStoreError> {
            self.inner.put_if_absent(key, body).await
        }
        async fn head(
            &self,
            key: &str,
        ) -> Result<crate::object_store::ObjectMeta, ObjectStoreError> {
            self.inner.head(key).await
        }
        async fn copy(&self, src: &str, dst: &str) -> Result<(), ObjectStoreError> {
            self.inner.copy(src, dst).await
        }
        async fn delete(&self, key: &str) -> Result<(), ObjectStoreError> {
            self.inner.delete(key).await
        }
    }

    /// Construct a 1-segment chain referencing pack `pack_sha_hex` at
    /// tip `tip_hex`. The pack is intentionally NOT seeded into the
    /// store so the first download attempt yields `PackMissing`.
    fn one_segment_chain(tip_hex: &str, pack_sha_hex: &str) -> ChainManifest {
        ChainManifest {
            v: 1,
            tip: Sha40::try_new(tip_hex).unwrap(),
            full_at: Sha40::try_new(tip_hex).unwrap(),
            segments: vec![ChainSegment {
                sha: Sha40::try_new(tip_hex).unwrap(),
                parent_sha: None,
                pack: format!("packs/{pack_sha_hex}.pack"),
                bytes: 1_024,
            }],
        }
    }

    /// Seed a real commit in `repo_dir` and return its `Sha40`. Used
    /// by the retry-success tests so the reload chain can reference a
    /// SHA already present in the local ODB — `select_needed_segments`
    /// stops at that segment, returns `(empty, false)`, and the
    /// download/install phase has nothing to do.
    fn seed_commit(repo_dir: &Path) -> Sha40 {
        use gix::actor::SignatureRef;
        use gix::bstr::BStr;
        use gix_hash::ObjectId;
        let repo = gix::open(repo_dir).unwrap();
        let signature = SignatureRef {
            name: BStr::new("Tester"),
            email: BStr::new("t@example.com"),
            time: "0 +0000",
        };
        let blob = repo.write_blob(b"hello").unwrap().detach();
        let tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![gix::objs::tree::Entry {
                    mode: gix::objs::tree::EntryKind::Blob.into(),
                    filename: "f".into(),
                    oid: blob,
                }],
            })
            .unwrap()
            .detach();
        let oid = repo
            .commit_as(
                signature,
                signature,
                "refs/heads/main",
                "seed",
                tree,
                std::iter::empty::<ObjectId>(),
            )
            .unwrap()
            .detach();
        Sha40::from_oid(&oid).unwrap()
    }

    /// Build a 1-segment chain referencing pack `pack_sha_hex` at tip
    /// `tip` whose segment SHA equals `segment_sha` — used by the
    /// retry-success tests to point the reloaded chain at a SHA the
    /// local ODB already contains.
    fn chain_with_known_segment(
        tip: &Sha40,
        segment_sha: &Sha40,
        pack_sha_hex: &str,
    ) -> ChainManifest {
        ChainManifest {
            v: 1,
            tip: tip.clone(),
            full_at: segment_sha.clone(),
            segments: vec![ChainSegment {
                sha: segment_sha.clone(),
                parent_sha: None,
                pack: format!("packs/{pack_sha_hex}.pack"),
                bytes: 1_024,
            }],
        }
    }

    /// Fetch retries once after `PackMissing`: the initial chain
    /// points at a pack absent from the bucket; the reload returns a
    /// chain whose only segment is already in the local ODB (the
    /// receiver's repo has been brought up to date by a sibling
    /// fetch, or the sweep compacted the chain to a point the
    /// receiver already covers). The retry observes no work left and
    /// returns Ok, proving the retry layer recovers from the GC race
    /// without requiring the original pack.
    ///
    /// Without the retry (pre-#148 behaviour), the call would surface
    /// `PackMissing` on the first attempt — the existing
    /// `fetch_surfaces_pack_missing_when_chain_references_absent_pack`
    /// test pins that legacy behaviour for the genuine-corruption case.
    #[tokio::test(start_paused = true)]
    async fn fetch_retries_after_pack_missing_when_reload_drops_segment() {
        // `start_paused = true` lets the runtime auto-advance through
        // the backoff sleep so the test pays nanoseconds, not 100 ms.
        let repo_dir = tempfile::tempdir().unwrap();
        gix::init(repo_dir.path()).unwrap();
        let known_sha = seed_commit(repo_dir.path());

        let initial_pack = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let reload_pack = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let tip = "1111111111111111111111111111111111111111";

        let inner = MockStore::new();
        // Reload presents a chain whose only segment is the
        // local-ODB-known commit (so `select_needed_segments` returns
        // `(empty, false)` and the install step has nothing to do).
        // The reload chain MUST reference a different pack key so the
        // verify-step (`chain_references_pack_key(initial_pack)`)
        // returns false and the retry proceeds instead of failing
        // fast on the genuine-corruption branch.
        let reload_chain =
            chain_with_known_segment(&Sha40::try_new(tip).unwrap(), &known_sha, reload_pack);
        let initial_bytes = Bytes::from(
            one_segment_chain(tip, initial_pack)
                .to_json_pretty()
                .unwrap(),
        );
        let reload_bytes = Bytes::from(reload_chain.to_json_pretty().unwrap());
        let key = chain_key(Some("repo"), ref_main());
        inner.insert(&key, initial_bytes.clone());

        let evolving = Arc::new(EvolvingChainStore::new(
            inner,
            key,
            vec![initial_bytes, reload_bytes],
        ));
        let store: Arc<dyn ObjectStore> = Arc::clone(&evolving) as _;
        let semaphore = Arc::new(Semaphore::new(MAX_FETCH_CONCURRENCY));
        let fetched_refs = FetchedRefs::new();
        let boundaries = ShallowBoundaries::new();
        let ref_name = ref_main();
        let tip_sha = Sha::from_hex(tip).unwrap();

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
        result.expect("retry must succeed once reload drops the missing-pack reference");
        // Pin the retry contract: at least two chain loads — one
        // before the PackMissing, one to verify the missing key was
        // dropped. (The retry attempt that follows itself loads
        // chain.json again, for a total of three on this path.)
        assert!(
            evolving.chain_calls() >= 2,
            "retry must reload chain.json at least once to verify the GC race; \
             observed {} chain calls",
            evolving.chain_calls()
        );
    }

    /// When each reload still references a fresh missing pack, the
    /// retry loop exhausts after `PACK_MISSING_MAX_RETRIES` and
    /// surfaces `ConcurrentGcRetriesExhausted` with the last observed
    /// key.
    ///
    /// The wrapper alternates `fetch_once` (load chain at the start
    /// of each attempt) with the verify-reload that follows the
    /// resulting `PackMissing`. With `PACK_MISSING_MAX_RETRIES=3`, we
    /// run four `fetch_once` attempts (initial + 3 retries) before
    /// the loop trips its exhausted check, so the store must serve
    /// eight distinct chain bodies: indices 0,2,4,6 are the
    /// per-attempt loads (each referencing a unique missing pack),
    /// indices 1,3,5,7 are the verification reloads (each referencing
    /// a DIFFERENT pack so `chain_references_pack_key` returns false
    /// and the loop retries instead of failing fast).
    #[tokio::test(start_paused = true)]
    async fn fetch_surfaces_exhausted_after_max_retries() {
        let repo_dir = tempfile::tempdir().unwrap();
        gix::init(repo_dir.path()).unwrap();

        // Eight distinct pack SHAs, one per chain-body in the
        // evolving sequence. Use the segment index as the dominant
        // nibble so the values stay visually distinct in failure
        // messages (each repeated 40 times to fill `Sha40`).
        let pack_shas: [String; 8] = std::array::from_fn(|i| {
            let nibble = char::from_digit(u32::try_from(i).unwrap(), 16).unwrap();
            std::iter::repeat_n(nibble, 40).collect()
        });
        let tip = "ffffffffffffffffffffffffffffffffffffffff";
        let bodies: Vec<Bytes> = pack_shas
            .iter()
            .map(|p| Bytes::from(one_segment_chain(tip, p).to_json_pretty().unwrap()))
            .collect();

        let inner = MockStore::new();
        let key = chain_key(Some("repo"), ref_main());
        inner.insert(&key, bodies[0].clone());
        let store: Arc<dyn ObjectStore> = Arc::new(EvolvingChainStore::new(inner, key, bodies));
        let semaphore = Arc::new(Semaphore::new(MAX_FETCH_CONCURRENCY));
        let fetched_refs = FetchedRefs::new();
        let boundaries = ShallowBoundaries::new();
        let ref_name = ref_main();
        let tip_sha = Sha::from_hex(tip).unwrap();

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
            Err(FetchError::Packchain(PackchainError::ConcurrentGcRetriesExhausted {
                last_missing_key,
                attempts,
            })) => {
                assert_eq!(attempts, PACK_MISSING_MAX_RETRIES);
                // The 4th and last fetch_once attempt loaded body
                // index 6 (refs pack_shas[6]) and surfaced
                // PackMissing for that pack.
                assert!(
                    last_missing_key.contains(&pack_shas[6]),
                    "last missing key should name pack_shas[6], got {last_missing_key}"
                );
            }
            other => panic!("expected ConcurrentGcRetriesExhausted, got {other:?}"),
        }
    }

    /// chain.json vanishing between the first download and the retry
    /// reload (ref deleted out from under the fetch) must surface as
    /// `ChainAbsent` — NOT as `PackMissing` and NOT as the retries-
    /// exhausted variant.
    #[tokio::test(start_paused = true)]
    async fn fetch_surfaces_chain_absent_when_chain_vanishes_mid_fetch() {
        let repo_dir = tempfile::tempdir().unwrap();
        gix::init(repo_dir.path()).unwrap();

        let pack_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let tip = "1111111111111111111111111111111111111111";
        let inner = MockStore::new();
        let initial = Bytes::from(one_segment_chain(tip, pack_sha).to_json_pretty().unwrap());
        let key = chain_key(Some("repo"), ref_main());
        inner.insert(&key, initial.clone());
        let store: Arc<dyn ObjectStore> = Arc::new(VanishingChainStore {
            inner,
            chain_key: key,
            initial,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let semaphore = Arc::new(Semaphore::new(MAX_FETCH_CONCURRENCY));
        let fetched_refs = FetchedRefs::new();
        let boundaries = ShallowBoundaries::new();
        let ref_name = ref_main();
        let tip_sha = Sha::from_hex(tip).unwrap();

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
            Err(FetchError::Packchain(PackchainError::ChainAbsent { ref_name: r })) => {
                assert_eq!(r, "refs/heads/main");
            }
            other => panic!("expected ChainAbsent, got {other:?}"),
        }
    }

    /// Shallow fetch must take the same retry path as the full fetch:
    /// initial chain references an absent pack, reload presents a
    /// chain whose only segment is already in the local ODB so the
    /// retry returns Ok. Pins that `fetch_shallow` is exercised
    /// through the retry wrapper.
    #[tokio::test(start_paused = true)]
    async fn fetch_shallow_retries_after_pack_missing_when_reload_drops_segment() {
        let repo_dir = tempfile::tempdir().unwrap();
        gix::init(repo_dir.path()).unwrap();
        let known_sha = seed_commit(repo_dir.path());

        let initial_pack = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let reload_pack = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let tip = "1111111111111111111111111111111111111111";

        let inner = MockStore::new();
        let initial_bytes = Bytes::from(
            one_segment_chain(tip, initial_pack)
                .to_json_pretty()
                .unwrap(),
        );
        let reload_bytes = Bytes::from(
            chain_with_known_segment(&Sha40::try_new(tip).unwrap(), &known_sha, reload_pack)
                .to_json_pretty()
                .unwrap(),
        );
        let key = chain_key(Some("repo"), ref_main());
        inner.insert(&key, initial_bytes.clone());
        let store: Arc<dyn ObjectStore> = Arc::new(EvolvingChainStore::new(
            inner,
            key,
            vec![initial_bytes, reload_bytes],
        ));
        let semaphore = Arc::new(Semaphore::new(MAX_FETCH_CONCURRENCY));
        let fetched_refs = FetchedRefs::new();
        let boundaries = ShallowBoundaries::new();
        let ref_name = ref_main();
        let tip_sha = Sha::from_hex(known_sha.as_str()).unwrap();
        let depth = NonZeroU32::new(1);

        let result = fetch_one(FetchOneCtx {
            store,
            semaphore,
            prefix: Some("repo"),
            repo_dir: repo_dir.path(),
            sha: tip_sha,
            ref_name: &ref_name,
            fetched_refs: &fetched_refs,
            depth,
            boundaries: &boundaries,
        })
        .await;
        result.expect("shallow retry must succeed once reload drops the missing-pack reference");
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

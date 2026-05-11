//! Chain compaction (issue #67).
//!
//! `compact` rewrites a packchain ref's `chain.json` to a single-segment
//! chain at the current tip. After a successful compact the ref's
//! on-bucket state mirrors a fresh first push:
//!
//! - `chain.json.segments.len() == 1` with `parent_sha = None`.
//! - `chain.tip == chain.full_at`.
//! - A fresh baseline bundle lives at `<prefix>/<ref>/<full_at>.bundle`.
//! - A fresh `path-index.json` reflects the tip's tree.
//! - Old segment packs are left in `<prefix>/packs/` and become orphans
//!   for [`super::gc`] to reap on the next mark/sweep cycle.
//!
//! ## Concurrency
//!
//! Compact holds the per-ref lock for the full duration so a concurrent
//! push cannot land a chain.json that compact then silently overwrites.
//! The lock TTL is configured by the caller; for large repos it must
//! be set well above the push default (typically 60s).
//!
//! ## Implementation choice
//!
//! Issue #67 enumerates two approaches: local-clone-then-repack vs
//! server-side re-pack. We take **local-clone-then-repack** because it
//! reuses [`super::pack::build_baseline_pack`] and the existing
//! [`super::fetch::install_pack`] pipeline; the alternative would
//! duplicate gix-pack iteration logic that already lives behind the
//! local-repo abstraction. The cost is one full chain install per
//! compact — paid only by an operator who explicitly opts in.

use std::path::Path;

use futures::stream::{StreamExt, TryStreamExt};
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use tracing::{debug, info, warn};

use crate::git::{self, RefName, Sha};
use crate::keys;
use crate::object_store::{GetOpts, ObjectStore, ObjectStoreError, PutOpts};
use crate::protocol::fetch::MAX_FETCH_CONCURRENCY;
use crate::protocol::push::{acquire_lock, lock_key, release_lock};

use super::PackchainError;
use super::audit::{COMPACT_BYTES_THRESHOLD, COMPACT_SEGMENTS_THRESHOLD};
use super::fetch::install_pack;
use super::keys::{pack_idx_key, pack_key, packs_key_with_prefix};
use super::manifest::{load_chain, write_chain, write_path_index};
use super::pack::build_baseline_pack;
use super::schema::{ChainManifest, ChainSegment};

/// Knobs for [`compact`]. Mirrors the shape used by `gc::SweepOpts`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CompactOpts {
    /// When `true`, bypass the "≥ 20 segments OR > 100 MiB" heuristic
    /// and compact unconditionally. Operator-asserted: small chains
    /// don't benefit from the work.
    pub(crate) force: bool,
    /// Lock TTL for the compact's per-ref lock. Compact holds the lock
    /// from chain read through chain.json commit; large repos may
    /// need TTL well above the push default.
    pub(crate) lock_ttl: Duration,
}

/// Per-ref compact result. The `action` field discriminates the four
/// terminal states the compact runner reports to operators.
#[derive(Debug, Clone)]
pub(crate) struct CompactOutcome {
    /// Outcome category — see [`CompactAction`].
    pub(crate) action: CompactAction,
    /// Ref whose chain was (or was not) compacted.
    pub(crate) ref_path: String,
    /// Pre-compact segment count. Reported regardless of action so
    /// operators can see why the heuristic did/didn't trigger.
    pub(crate) prior_segments: usize,
    /// Pre-compact `sum(segment.bytes)`.
    pub(crate) prior_bytes: u64,
    /// Content-SHA of the new baseline pack (only set on
    /// [`CompactAction::Compacted`]).
    pub(crate) new_pack_sha: Option<String>,
    /// Size of the new baseline pack in bytes (only set on
    /// [`CompactAction::Compacted`]).
    pub(crate) new_pack_bytes: u64,
}

/// Terminal state of a [`compact`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactAction {
    /// Chain was rewritten to a single-segment chain at the current tip.
    Compacted,
    /// Heuristic did not trigger and `force` was not set; chain
    /// untouched.
    SkippedUnderThreshold,
    /// Chain was already a single-segment chain at the current tip;
    /// idempotent no-op (no lock acquisition, no I/O beyond the
    /// initial `chain.json` read).
    AlreadyMinimal,
    /// Per-ref lock was held by another client; no work performed.
    LockContended,
}

/// Run compact against `<prefix>/<ref_name>/`.
///
/// Acquires the per-ref lock with `opts.lock_ttl`, downloads every
/// chain pack and the existing baseline bundle into a tempdir-rooted
/// bare git repo, builds a fresh baseline pack via
/// [`build_baseline_pack`], regenerates `path-index.json`, builds a
/// fresh baseline bundle, uploads the new artefacts, and atomically
/// commits the new single-segment `chain.json`. Old segment packs
/// stay on the bucket as orphans for [`super::gc`].
///
/// # Errors
///
/// - [`PackchainError::ChainAbsent`] — the ref has no `chain.json`.
/// - [`PackchainError::Store`] — transport failures during artefact
///   download or upload.
/// - [`PackchainError::PackBuild`] / [`PackchainError::PackIndexWrite`]
///   — gix-pack failures during the fresh baseline construction.
/// - [`PackchainError::Git`] — gix failures during chain install or
///   path-index extraction.
/// - [`PackchainError::Io`] — local I/O failures (tempdir, file
///   read/write).
pub(crate) async fn compact(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    ref_name: &RefName,
    opts: CompactOpts,
) -> Result<CompactOutcome, PackchainError> {
    let prior =
        load_chain(store, prefix, ref_name)
            .await?
            .ok_or_else(|| PackchainError::ChainAbsent {
                ref_name: ref_name.as_str().to_owned(),
            })?;

    let prior_segments = prior.segments.len();
    let prior_bytes = prior
        .segments
        .iter()
        .map(|s| s.bytes)
        .fold(0u64, u64::saturating_add);

    // Idempotent short-circuit: a chain that's already a single
    // segment at the tip needs nothing done and we avoid taking the
    // lock entirely. Concurrent compact-against-the-same-ref runs
    // both observe this state once any one of them completes.
    if prior_segments == 1 && prior.tip == prior.full_at {
        return Ok(CompactOutcome {
            action: CompactAction::AlreadyMinimal,
            ref_path: ref_name.as_str().to_owned(),
            prior_segments,
            prior_bytes,
            new_pack_sha: None,
            new_pack_bytes: 0,
        });
    }

    if !opts.force
        && prior_segments <= COMPACT_SEGMENTS_THRESHOLD
        && prior_bytes <= COMPACT_BYTES_THRESHOLD
    {
        debug!(
            ref_path = %ref_name.as_str(),
            segments = prior_segments,
            bytes = prior_bytes,
            "compact: heuristic did not trigger; skipping",
        );
        return Ok(CompactOutcome {
            action: CompactAction::SkippedUnderThreshold,
            ref_path: ref_name.as_str().to_owned(),
            prior_segments,
            prior_bytes,
            new_pack_sha: None,
            new_pack_bytes: 0,
        });
    }

    let lock = lock_key(prefix, ref_name);
    let now = OffsetDateTime::now_utc();
    if !acquire_lock(store, &lock, opts.lock_ttl, now)
        .await
        .map_err(PackchainError::Store)?
    {
        return Ok(CompactOutcome {
            action: CompactAction::LockContended,
            ref_path: ref_name.as_str().to_owned(),
            prior_segments,
            prior_bytes,
            new_pack_sha: None,
            new_pack_bytes: 0,
        });
    }

    // Mirror push.rs's "try / always release" pattern: run work, drop
    // the lock unconditionally, then surface either error.
    let result = compact_under_lock(store, prefix, ref_name).await;
    let release_result = release_lock(store, &lock).await;

    // Match push.rs:667-687: a genuine work error takes priority,
    // but a release failure on the (Err, Err) path is logged at
    // `warn` so an operator investigating a compact failure also
    // sees the dangling-lock signal. Without this, the (Err, Err)
    // case silently drops the release error.
    match (&result, release_result) {
        (Ok(_), Err(e)) => {
            // Work succeeded but release failed; surface the
            // dangling-lock detail at warn rather than failing the
            // operator-visible result. The lock will age out by
            // `opts.lock_ttl`; subsequent pushes will recover it
            // as stale.
            warn!(key = %lock, error = %e, "compact: failed to release per-ref lock");
        }
        (Err(_), Err(e)) => {
            warn!(
                key = %lock,
                error = %e,
                "compact: lock release failed (compact already errored)",
            );
        }
        _ => {}
    }
    let mut outcome = result?;
    // Fill in pre-lock fields that compact_under_lock didn't have
    // visibility into.
    outcome.prior_segments = prior_segments;
    outcome.prior_bytes = prior_bytes;
    Ok(outcome)
}

/// Lock-protected body of [`compact`]: re-read chain, install
/// locally, build fresh artefacts, upload, commit chain.json.
async fn compact_under_lock(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    ref_name: &RefName,
) -> Result<CompactOutcome, PackchainError> {
    // Re-read chain under lock. A pre-lock observation could be
    // stale (concurrent push between heuristic check and lock).
    let chain =
        load_chain(store, prefix, ref_name)
            .await?
            .ok_or_else(|| PackchainError::ChainAbsent {
                ref_name: ref_name.as_str().to_owned(),
            })?;
    // `Sha40::try_new` already validated 40-hex on deserialise of
    // `chain.tip`, so the `Sha::from_hex` round-trip is provably-OK
    // by construction. Use `expect` per the project's
    // unreachable-defensive-code rule.
    let tip_sha = Sha::from_hex(chain.tip.as_str()).expect("Sha40 always parses as a gix Sha");

    let scratch = TempDir::new().map_err(PackchainError::Io)?;
    let repo_dir = scratch.path().join("repo");
    let download_dir = scratch.path().join("downloads");
    let output_dir = scratch.path().join("output");
    std::fs::create_dir(&repo_dir).map_err(PackchainError::Io)?;
    std::fs::create_dir(&download_dir).map_err(PackchainError::Io)?;
    std::fs::create_dir(&output_dir).map_err(PackchainError::Io)?;

    // Init a bare git repo in `repo_dir`. Bare avoids creating a
    // working tree the install step would need to ignore.
    // `gix::init::Error` is a large variant — box it on the way out
    // so the closure return type stays small (avoids
    // `clippy::result_large_err`); the `?` chain then collapses
    // through `From<JoinError> for PackchainError` and the explicit
    // map for the boxed init error.
    {
        let repo_dir = repo_dir.clone();
        tokio::task::spawn_blocking(move || {
            gix::init_bare(&repo_dir)
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
        })
        .await?
        .map_err(|e| PackchainError::Io(std::io::Error::other(e.to_string())))?;
    }

    // Download every chain pack + the baseline bundle in parallel.
    download_chain_artefacts(store, prefix, ref_name, &chain, &download_dir).await?;

    // Install oldest-first: baseline bundle first, then segments
    // from oldest (last in newest-first list) to newest. Mirrors
    // `fetch::fetch_full`'s install order so deltas resolve against
    // already-installed bases.
    install_chain_into_repo(&repo_dir, &download_dir, &chain, ref_name).await?;

    // Build the fresh baseline pack at the current tip. After this
    // returns, `<output_dir>/<content_sha>.{pack,idx}` exist. `tip_sha`
    // may be unpeeled (tag OID for tag refs, tree/blob OID for
    // bare-tree / bare-blob refs); peel through the tag chain inside
    // the blocking task and dispatch on the leaf kind so the rebuilt
    // baseline carries the right closure. `gix::Repository` is `!Sync`,
    // hence the blocking-task wrap.
    let new_pack = {
        let repo_dir = repo_dir.clone();
        let output_dir = output_dir.clone();
        tokio::task::spawn_blocking(move || -> Result<_, PackchainError> {
            let repo = gix::open(&repo_dir).map_err(crate::git::GitError::from)?;
            let peeled = crate::git::peel_tag_chain(&repo, tip_sha).map_err(PackchainError::Git)?;
            drop(repo);
            build_baseline_pack(&repo_dir, peeled, &output_dir)
        })
        .await??
    };

    // Compute fresh path-index. Returns `None` for blob-tipped
    // chains — compaction skips writing path-index in that case.
    let path_index = {
        let repo_dir = repo_dir.clone();
        tokio::task::spawn_blocking(move || -> Result<_, PackchainError> {
            let repo = gix::open(&repo_dir).map_err(crate::git::GitError::from)?;
            let peeled = crate::git::peel_tag_chain(&repo, tip_sha).map_err(PackchainError::Git)?;
            super::git::extract_path_index(&repo, &peeled, tip_sha)
        })
        .await??
    };

    // Build fresh baseline bundle. The bundle filename uses the tip
    // SHA. The spec must resolve to that commit in the temp repo —
    // and the temp repo only has objects, not refs (unbundle and
    // install_pack do not create refs), so pass the SHA directly
    // rather than the ref name. `bundle_at` clones the spec into a
    // `'static` String internally for `spawn_blocking`, so passing
    // a borrowed `&str` here is sufficient.
    let bundle_path = git::bundle_at(&repo_dir, &output_dir, tip_sha, chain.tip.as_str())
        .await
        .map_err(PackchainError::Git)?;

    // Upload artefacts. Order matters here for crash safety: pack
    // and bundle FIRST (orphan-safe — they become reapable orphans
    // if the chain commit fails), then chain.json, then path-index.
    // The chain.json PUT is the linearisation point — anything
    // before it is best-effort and reapable; anything after is
    // already committed. path-index follows chain.json so a crash
    // between them leaves a stale `path_index.tip` paired with a
    // fresh `chain.tip` (reader-detected, issue #114) rather than
    // the inverse, which would surface as `BlobNotInChain`.
    //
    // In compact specifically the tip is unchanged across the
    // operation, so even an interrupted path-index PUT leaves
    // `path_index.tip == chain.tip` and the mismatch check never
    // trips; readers see consistent state throughout.
    let new_pack_key = pack_key(prefix, &new_pack.content_sha);
    let new_idx_key = pack_idx_key(prefix, &new_pack.content_sha);
    let new_bundle_key = keys::bundle_key(prefix, ref_name, chain.tip.as_str());
    // Capture the pre-rewrite baseline bundle key. Once `write_chain`
    // commits the new manifest, the old `<full_at>.bundle` becomes
    // unreachable — and bundle keys live outside the `packs/`
    // namespace that `gc::list_pack_shas` scans, so without an
    // explicit delete here the file would leak forever (issue #84).
    // We delete only when `full_at` actually moved; if the tip is
    // unchanged (compact collapsing only non-baseline packs), the
    // old key equals the new one and is still live.
    let old_bundle_key = keys::bundle_key(prefix, ref_name, chain.full_at.as_str());
    upload_pack_and_idx(
        store,
        &new_pack_key,
        &new_idx_key,
        &new_pack.pack_path,
        &new_pack.idx_path,
    )
    .await?;
    upload_bundle(store, &new_bundle_key, &bundle_path).await?;

    let new_segment = ChainSegment {
        sha: chain.tip.clone(),
        parent_sha: None,
        // Schema stores the pack key relative to the bucket; reuse
        // the canonical key builder with `None` as the prefix to
        // produce `packs/<sha>.pack`.
        pack: pack_key(None, &new_pack.content_sha),
        bytes: new_pack.pack_bytes,
    };
    let new_chain = ChainManifest {
        v: ChainManifest::SCHEMA_VERSION,
        tip: chain.tip.clone(),
        full_at: chain.tip.clone(),
        segments: vec![new_segment],
    };
    write_chain(store, prefix, ref_name, &new_chain).await?;

    // path-index.json follows chain.json — see the comment block
    // above on the linearisation invariant.
    if let Some(ref index) = path_index {
        write_path_index(store, prefix, ref_name, index).await?;
    }

    // chain.json is now durable — the old baseline bundle is
    // unreachable. See [`delete_prior_baseline_bundle`] for why
    // this delete must run under the same lock and after the
    // chain.json commit. The delete is best-effort: a failure
    // after the chain commit must NOT report compact as failed,
    // because the new chain is already live and a retry would
    // short-circuit through `AlreadyMinimal` without finishing
    // the cleanup (issue #113). The helper logs at `warn` with
    // the orphan key so an operator can clean up manually.
    delete_prior_baseline_bundle(store, ref_name, &old_bundle_key, &new_bundle_key).await;

    info!(
        ref_path = %ref_name.as_str(),
        prior_segments = chain.segments.len(),
        new_pack_sha = %new_pack.content_sha.as_str(),
        new_pack_bytes = new_pack.pack_bytes,
        "compact: chain rewritten to single segment",
    );

    Ok(CompactOutcome {
        action: CompactAction::Compacted,
        ref_path: ref_name.as_str().to_owned(),
        // prior_* are filled in by the public `compact` after this
        // function returns (it has the snapshot from before the
        // lock was taken).
        prior_segments: 0,
        prior_bytes: 0,
        new_pack_sha: Some(new_pack.content_sha.as_str().to_owned()),
        new_pack_bytes: new_pack.pack_bytes,
    })
}

/// Download every chain pack + the baseline bundle into
/// `download_dir`. Pack files land at
/// `<download_dir>/<segment.sha>.pack`; the baseline bundle lands at
/// `<download_dir>/<full_at>.bundle`. Per-artefact downloads run in
/// parallel up to [`MAX_FETCH_CONCURRENCY`].
async fn download_chain_artefacts(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    ref_name: &RefName,
    chain: &ChainManifest,
    download_dir: &Path,
) -> Result<(), PackchainError> {
    // Validate every segment's bucket-relative pack key before
    // composing GET keys. `ChainSegment::pack` deserialises as a
    // plain `String` with no format check, so a crafted chain.json
    // could otherwise drive `packs_key_with_prefix` to request an
    // arbitrary bucket key. Mirror the same guard `gc.rs`, `read.rs`,
    // and `fetch.rs` apply (issue #120). The local destination name
    // keeps `seg.sha` (commit SHA at the segment tip) to match the
    // install step's filename convention — `parse_pack_key_sha`'s
    // output is the pack content SHA, a different identifier.
    let mut tasks: Vec<DownloadTask> = chain
        .segments
        .iter()
        .map(|seg| {
            super::keys::parse_pack_key_sha(&seg.pack).ok_or_else(|| {
                PackchainError::MalformedPackEntry {
                    offset: 0,
                    reason: format!(
                        "chain segment pack key `{}` is not of the form `[<prefix>/]packs/<sha>.pack`",
                        seg.pack,
                    ),
                }
            })?;
            Ok::<_, PackchainError>(DownloadTask {
                key: packs_key_with_prefix(prefix, &seg.pack),
                dest: download_dir.join(format!("{}.pack", seg.sha.as_str())),
                kind: "pack",
            })
        })
        .collect::<Result<_, _>>()?;
    tasks.push(DownloadTask {
        key: keys::bundle_key(prefix, ref_name, chain.full_at.as_str()),
        dest: download_dir.join(format!("{}.bundle", chain.full_at.as_str())),
        kind: "bundle",
    });

    futures::stream::iter(tasks)
        .map(|task| async move { task.run(store).await })
        .buffer_unordered(MAX_FETCH_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(())
}

/// One artefact to download. `kind` is a static label for the
/// `tracing::debug!` line so an operator can see what failed.
#[derive(Debug, Clone)]
struct DownloadTask {
    key: String,
    dest: std::path::PathBuf,
    kind: &'static str,
}

impl DownloadTask {
    async fn run(self, store: &dyn ObjectStore) -> Result<(), PackchainError> {
        // Stream the body straight to disk so a chain with many
        // large packs doesn't hold every body in memory under
        // `buffer_unordered`. Peak RSS becomes
        // `MAX_FETCH_CONCURRENCY × per-chunk-buffer` instead of
        // `MAX_FETCH_CONCURRENCY × largest-pack`.
        store
            .get_to_file(&self.key, &self.dest, GetOpts::default())
            .await
            .map_err(PackchainError::Store)?;
        debug!(key = %self.key, kind = self.kind, "compact: downloaded");
        Ok(())
    }
}

/// Install every chain pack + the baseline bundle into `repo_dir`,
/// in oldest-first order so each install resolves its deltas against
/// already-installed bases.
async fn install_chain_into_repo(
    repo_dir: &Path,
    download_dir: &Path,
    chain: &ChainManifest,
    ref_name: &RefName,
) -> Result<(), PackchainError> {
    // `chain.full_at` has the same Sha40-validated provenance as
    // `chain.tip`; see the matching comment in `compact_under_lock`.
    let baseline_sha =
        Sha::from_hex(chain.full_at.as_str()).expect("Sha40 always parses as a gix Sha");
    // `unbundle_at` resolves the bundle file via `<folder>/<sha>.bundle`.
    git::unbundle_at(repo_dir, download_dir, baseline_sha, ref_name)
        .await
        .map_err(PackchainError::Git)?;

    for segment in chain.segments.iter().rev() {
        let pack_path = download_dir.join(format!("{}.pack", segment.sha.as_str()));
        let repo_dir = repo_dir.to_path_buf();
        tokio::task::spawn_blocking(move || install_pack(&repo_dir, &pack_path))
            .await?
            .map_err(|e| match e {
                crate::protocol::fetch::FetchError::Packchain(p) => p,
                crate::protocol::fetch::FetchError::Git(g) => PackchainError::Git(g),
                crate::protocol::fetch::FetchError::Io(io) => PackchainError::Io(io),
                other => PackchainError::Io(std::io::Error::other(other.to_string())),
            })?;
    }
    Ok(())
}

/// Upload `<pack_path>` and `<idx_path>` to their respective bucket
/// keys in parallel. Pack and idx are independent and share no
/// resource, so `try_join!` saves one full round-trip per compact
/// without orchestration complexity worth flagging.
async fn upload_pack_and_idx(
    store: &dyn ObjectStore,
    pack_key: &str,
    idx_key: &str,
    pack_path: &Path,
    idx_path: &Path,
) -> Result<(), PackchainError> {
    tokio::try_join!(
        store.put_path(pack_key, pack_path, PutOpts::default()),
        store.put_path(idx_key, idx_path, PutOpts::default()),
    )
    .map_err(PackchainError::Store)?;
    Ok(())
}

async fn upload_bundle(
    store: &dyn ObjectStore,
    key: &str,
    bundle_path: &Path,
) -> Result<(), PackchainError> {
    store
        .put_path(key, bundle_path, PutOpts::default())
        .await
        .map_err(PackchainError::Store)?;
    Ok(())
}

/// Best-effort delete of the prior `<full_at>.bundle` once the new
/// chain.json is durable.
///
/// Called from [`compact_under_lock`] AFTER the `write_chain` PUT —
/// any reader after that point follows the new chain.json and never
/// references the old key. Running under the per-ref lock prevents a
/// concurrent push from reusing the same `full_at` SHA between our
/// chain commit and our delete.
///
/// Skips when `old == new` (compact left `full_at` unchanged — the
/// keys alias the same live bundle). Tolerates `NotFound` so a retry
/// after a partial completion is idempotent (issue #84).
///
/// Failure contract (issue #113): the new chain manifest is already
/// durable on the bucket when this function runs, so a transient
/// store error here must NOT propagate as a compact failure. Retrying
/// `compact` would short-circuit through `AlreadyMinimal` and never
/// re-attempt the cleanup, stranding the old baseline indefinitely
/// (`gc::list_pack_shas` does not sweep bundle keys). Log at `warn`
/// with the orphan key and return.
async fn delete_prior_baseline_bundle(
    store: &dyn ObjectStore,
    ref_name: &RefName,
    old_bundle_key: &str,
    new_bundle_key: &str,
) {
    if old_bundle_key == new_bundle_key {
        return;
    }
    match store.delete(old_bundle_key).await {
        Ok(()) => {
            debug!(key = %old_bundle_key, "compact: deleted prior baseline bundle");
        }
        Err(ObjectStoreError::NotFound(_)) => {
            debug!(
                key = %old_bundle_key,
                "compact: prior baseline bundle already absent",
            );
        }
        Err(e) => {
            warn!(
                ref_path = %ref_name.as_str(),
                key = %old_bundle_key,
                error = %e,
                "compact: prior baseline cleanup failed (chain.json already committed); \
                 orphan key left for manual cleanup",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::mock::MockStore;
    use crate::packchain::manifest::write_chain;
    use crate::packchain::pack::{build_baseline_pack, build_incremental_pack};
    use crate::packchain::schema::{ChainSegment, Sha40};
    use bytes::Bytes;
    use gix::actor::SignatureRef;
    use gix::bstr::BStr;
    use gix_hash::ObjectId;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn signature() -> SignatureRef<'static> {
        SignatureRef {
            name: BStr::new("Tester"),
            email: BStr::new("t@example.com"),
            time: "0 +0000",
        }
    }

    fn ref_main() -> RefName {
        RefName::new("refs/heads/main").unwrap()
    }

    fn sha40(s: &str) -> Sha40 {
        Sha40::try_new(s).unwrap()
    }

    fn opts(force: bool) -> CompactOpts {
        CompactOpts {
            force,
            lock_ttl: Duration::seconds(60),
        }
    }

    /// Build a small repo with three commits c1 -> c2 -> c3, each
    /// touching one file. Returns the repo dir and tip sha for c3.
    fn fixture_three_commits() -> (TempDir, Sha, Sha, Sha) {
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();
        let mut prev: Option<ObjectId> = None;
        let mut shas: Vec<ObjectId> = Vec::new();
        for (i, content) in [b"hello-1".as_slice(), b"hello-2", b"hello-3"]
            .iter()
            .enumerate()
        {
            let blob = repo.write_blob(content).unwrap().detach();
            let tree = repo
                .write_object(&gix::objs::Tree {
                    entries: vec![gix::objs::tree::Entry {
                        mode: gix::objs::tree::EntryKind::Blob.into(),
                        filename: format!("f{i}").into(),
                        oid: blob,
                    }],
                })
                .unwrap()
                .detach();
            let parents: Vec<ObjectId> = prev.into_iter().collect();
            let c = repo
                .commit_as(
                    signature(),
                    signature(),
                    "refs/heads/main",
                    format!("commit {i}"),
                    tree,
                    parents,
                )
                .unwrap()
                .detach();
            shas.push(c);
            prev = Some(c);
        }
        (
            tmp,
            Sha::from_object_id(shas[0]),
            Sha::from_object_id(shas[1]),
            Sha::from_object_id(shas[2]),
        )
    }

    /// Lay down a real three-segment packchain in `store` using the
    /// repo from [`fixture_three_commits`]. Returns the chain and
    /// the path to the source repo (for read-back verification).
    async fn lay_down_three_segment_chain(
        store: &MockStore,
        prefix: &str,
    ) -> (TempDir, ChainManifest, Sha, Sha, Sha) {
        let (repo_dir, c1, c2, c3) = fixture_three_commits();

        // Baseline pack at c1, plus incremental packs for c2 and c3.
        let out_dir = TempDir::new().unwrap();
        let baseline = build_baseline_pack(
            repo_dir.path(),
            crate::git::PeeledTip::Commit {
                commit: c1,
                tag_chain: Vec::new(),
            },
            out_dir.path(),
        )
        .unwrap();
        let inc2 = build_incremental_pack(repo_dir.path(), c1, c2, &[], out_dir.path()).unwrap();
        let inc3 = build_incremental_pack(repo_dir.path(), c2, c3, &[], out_dir.path()).unwrap();

        // Upload packs + idx files.
        for built in [&baseline, &inc2, &inc3] {
            let pack_key = format!("{}/packs/{}.pack", prefix, built.content_sha.as_str());
            let idx_key = format!("{}/packs/{}.idx", prefix, built.content_sha.as_str());
            let pack_bytes = std::fs::read(&built.pack_path).unwrap();
            let idx_bytes = std::fs::read(&built.idx_path).unwrap();
            store.insert(pack_key, Bytes::from(pack_bytes));
            store.insert(idx_key, Bytes::from(idx_bytes));
        }

        // Baseline bundle at c1. Pass the commit SHA as the spec so
        // `bundle::create` resolves to c1 specifically (the repo's
        // `refs/heads/main` points at c3 after the third commit).
        let c1_spec = c1.to_string();
        let bundle_path = crate::git::bundle_at(repo_dir.path(), out_dir.path(), c1, &c1_spec)
            .await
            .unwrap();
        let bundle_bytes = std::fs::read(&bundle_path).unwrap();
        let bundle_key = format!("{prefix}/refs/heads/main/{c1}.bundle");
        store.insert(bundle_key, Bytes::from(bundle_bytes));

        // chain.json: newest-first [c3, c2, c1], full_at = c1.
        let chain = ChainManifest {
            v: ChainManifest::SCHEMA_VERSION,
            tip: sha40(&c3.to_string()),
            full_at: sha40(&c1.to_string()),
            segments: vec![
                ChainSegment {
                    sha: sha40(&c3.to_string()),
                    parent_sha: Some(sha40(&c2.to_string())),
                    pack: format!("packs/{}.pack", inc3.content_sha.as_str()),
                    bytes: inc3.pack_bytes,
                },
                ChainSegment {
                    sha: sha40(&c2.to_string()),
                    parent_sha: Some(sha40(&c1.to_string())),
                    pack: format!("packs/{}.pack", inc2.content_sha.as_str()),
                    bytes: inc2.pack_bytes,
                },
                ChainSegment {
                    sha: sha40(&c1.to_string()),
                    parent_sha: None,
                    pack: format!("packs/{}.pack", baseline.content_sha.as_str()),
                    bytes: baseline.pack_bytes,
                },
            ],
        };
        write_chain(store, Some(prefix), &ref_main(), &chain)
            .await
            .unwrap();

        // Path-index at the current tip — the read_blob path requires
        // it. Build it from the source repo before we drop the handle.
        let path_index = {
            let repo = gix::open(repo_dir.path()).unwrap();
            let peeled = crate::git::peel_tag_chain(&repo, c3).unwrap();
            crate::packchain::git::extract_path_index(&repo, &peeled, c3)
                .unwrap()
                .expect("commit-tipped fixture must produce a path-index")
        };
        crate::packchain::manifest::write_path_index(store, Some(prefix), &ref_main(), &path_index)
            .await
            .unwrap();

        (repo_dir, chain, c1, c2, c3)
    }

    #[tokio::test]
    async fn compact_short_circuits_for_single_segment_at_tip() {
        // A chain that is already a single-segment chain at the tip is
        // the post-compact state; running compact again must be a
        // no-op (no lock taken, no I/O beyond the chain.json read).
        let store = MockStore::new();
        let chain = ChainManifest {
            v: 1,
            tip: sha40("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            full_at: sha40("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            segments: vec![ChainSegment {
                sha: sha40("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                parent_sha: None,
                pack: "packs/1111111111111111111111111111111111111111.pack".into(),
                bytes: 100,
            }],
        };
        write_chain(&store, Some("repo"), &ref_main(), &chain)
            .await
            .unwrap();

        let outcome = compact(&store, Some("repo"), &ref_main(), opts(true))
            .await
            .unwrap();
        assert_eq!(outcome.action, CompactAction::AlreadyMinimal);
    }

    #[tokio::test]
    async fn compact_skips_when_under_threshold_without_force() {
        let store = MockStore::new();
        // 2 segments, 1 KiB each — well under both thresholds.
        let chain = ChainManifest {
            v: 1,
            tip: sha40("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            full_at: sha40("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            segments: vec![
                ChainSegment {
                    sha: sha40("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                    parent_sha: Some(sha40("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")),
                    pack: "packs/1111111111111111111111111111111111111111.pack".into(),
                    bytes: 1024,
                },
                ChainSegment {
                    sha: sha40("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                    parent_sha: None,
                    pack: "packs/2222222222222222222222222222222222222222.pack".into(),
                    bytes: 1024,
                },
            ],
        };
        write_chain(&store, Some("repo"), &ref_main(), &chain)
            .await
            .unwrap();

        let outcome = compact(&store, Some("repo"), &ref_main(), opts(false))
            .await
            .unwrap();
        assert_eq!(outcome.action, CompactAction::SkippedUnderThreshold);
        assert_eq!(outcome.prior_segments, 2);
    }

    #[tokio::test]
    async fn compact_returns_lock_contended_when_lock_held_recently() {
        // Pre-place a fresh lock; compact's `acquire_lock` should
        // observe a non-stale lock and return LockContended without
        // doing any work.
        let store = MockStore::new();
        let (_repo, _chain, _c1, _c2, _c3) = lay_down_three_segment_chain(&store, "repo").await;

        let lock_key = lock_key(Some("repo"), &ref_main());
        store.insert(&lock_key, Bytes::new());

        let outcome = compact(&store, Some("repo"), &ref_main(), opts(true))
            .await
            .unwrap();
        assert_eq!(outcome.action, CompactAction::LockContended);
        // Lock object still present.
        assert!(store.contains(&lock_key));
    }

    #[tokio::test]
    async fn compact_chain_absent_returns_chain_absent_error() {
        let store = MockStore::new();
        let err = compact(&store, Some("repo"), &ref_main(), opts(true))
            .await
            .unwrap_err();
        assert!(matches!(err, PackchainError::ChainAbsent { .. }));
    }

    #[tokio::test]
    async fn compact_force_collapses_three_segment_chain_to_one() {
        // The substantive end-to-end test: build a real 3-commit
        // repo, lay down a 3-segment packchain in MockStore, run
        // compact, verify the resulting chain.json is a single
        // segment at tip with full_at = tip and the new pack
        // content-sha is uploaded.
        let store = MockStore::new();
        let (_repo, prior, c1, _c2, c3) = lay_down_three_segment_chain(&store, "repo").await;

        // Delete the fixture's path-index so the post-compact
        // assertion below is observable: compact writing the
        // path-index is the only way for the key to reappear.
        // (Without this delete the assertion would be tautological —
        // the fixture writes the same path-index compact would
        // produce.)
        let path_index_key = "repo/refs/heads/main/path-index.json";
        store.delete(path_index_key).await.unwrap();

        let outcome = compact(&store, Some("repo"), &ref_main(), opts(true))
            .await
            .unwrap();
        assert_eq!(outcome.action, CompactAction::Compacted);
        assert_eq!(outcome.prior_segments, 3);

        // Read the new chain.json and verify shape.
        let new_chain = crate::packchain::manifest::load_chain(&store, Some("repo"), &ref_main())
            .await
            .unwrap()
            .expect("chain present");
        assert_eq!(new_chain.tip, prior.tip);
        assert_eq!(
            new_chain.full_at, new_chain.tip,
            "full_at must equal tip after compact",
        );
        assert_eq!(new_chain.segments.len(), 1);
        assert_eq!(new_chain.segments[0].sha, prior.tip);
        assert_eq!(
            new_chain.segments[0].parent_sha, None,
            "post-compact chain root segment has no parent",
        );

        // The single segment's pack key must exist on the bucket.
        let new_pack_sha = outcome
            .new_pack_sha
            .as_ref()
            .expect("Compacted action carries new pack sha");
        let pack_key = format!("repo/packs/{new_pack_sha}.pack");
        let idx_key = format!("repo/packs/{new_pack_sha}.idx");
        assert!(store.contains(&pack_key), "new pack must be uploaded");
        assert!(store.contains(&idx_key), "new idx must be uploaded");

        // The fresh baseline bundle at the new tip must exist.
        let bundle_key = format!("repo/refs/heads/main/{c3}.bundle");
        assert!(
            store.contains(&bundle_key),
            "fresh baseline bundle at the new tip must be uploaded",
        );

        // The PRIOR baseline bundle (at c1) must have been deleted —
        // it is unreachable from the new chain.json and lives outside
        // the `packs/` namespace that GC scans, so leaking it would
        // accumulate orphan files forever (issue #84).
        let prior_bundle_key = format!("repo/refs/heads/main/{c1}.bundle");
        assert!(
            !store.contains(&prior_bundle_key),
            "compact must delete the prior baseline bundle once \
             chain.json no longer references it",
        );

        // The fresh path-index.json at the ref must exist. We
        // explicitly deleted the fixture's pre-existing one above
        // so this assertion catches a regression where compact
        // drops the `write_path_index` call.
        assert!(
            store.contains(path_index_key),
            "compact must write a fresh path-index.json at the ref",
        );

        // Lock must be released.
        let lock = lock_key(Some("repo"), &ref_main());
        assert!(
            !store.contains(&lock),
            "compact must release the per-ref lock on success",
        );
    }

    #[tokio::test]
    async fn second_compact_after_successful_compact_is_already_minimal() {
        // Idempotency at the public `compact()` entrypoint: a successful
        // compact deletes the prior baseline bundle and rewrites the chain
        // to a single segment at the tip. A second compact against the
        // same ref must observe `AlreadyMinimal` and do nothing.
        //
        // NOTE: this test does NOT cover the `delete_prior_baseline_bundle`
        // `NotFound` branch — the second compact short-circuits at
        // `AlreadyMinimal` before reaching the helper, so removing the
        // `NotFound` tolerance from production code does not make this
        // test fail. The helper's `NotFound` branch is covered directly
        // by `delete_prior_baseline_bundle_tolerates_already_absent`.
        let store = MockStore::new();
        let (_repo, _prior, c1, _c2, _c3) = lay_down_three_segment_chain(&store, "repo").await;

        let prior_bundle_key = format!("repo/refs/heads/main/{c1}.bundle");
        // Fixture invariant: the prior baseline bundle is written; assert
        // up-front so the test fails loudly if the fixture changes shape.
        assert!(store.contains(&prior_bundle_key));

        let outcome = compact(&store, Some("repo"), &ref_main(), opts(true))
            .await
            .unwrap();
        assert_eq!(outcome.action, CompactAction::Compacted);
        assert!(!store.contains(&prior_bundle_key));

        let second = compact(&store, Some("repo"), &ref_main(), opts(true))
            .await
            .unwrap();
        assert_eq!(second.action, CompactAction::AlreadyMinimal);
    }

    #[tokio::test]
    async fn delete_prior_baseline_bundle_tolerates_already_absent() {
        // Direct unit-level coverage of the `NotFound` branch.
        // `second_compact_after_successful_compact_is_already_minimal`
        // exercises idempotency at the public `compact()` entrypoint,
        // but its second run short-circuits via `AlreadyMinimal` before
        // reaching the helper — so it cannot catch a regression that
        // breaks the helper's `NotFound` tolerance. This test calls the
        // helper directly with an old key that does not exist on the
        // store, locking down the tolerated `NotFound` swallow that
        // issue #84's fix relies on.
        let store = MockStore::new();
        // Old key intentionally absent; new key intentionally present.
        // Distinct keys so the early `old == new` short-circuit does
        // not fire — the delete must actually be attempted.
        let old_key = "repo/refs/heads/main/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bundle";
        let new_key = "repo/refs/heads/main/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.bundle";
        store.insert(new_key, Bytes::from_static(b"new"));
        delete_prior_baseline_bundle(&store, &ref_main(), old_key, new_key).await;
        assert!(
            store.contains(new_key),
            "tolerating NotFound on the old key must not touch the new key",
        );
    }

    #[tokio::test]
    async fn delete_prior_baseline_bundle_skips_when_keys_alias() {
        // The `old_key == new_key` short-circuit guards the case where
        // compact left `full_at` unchanged (collapsing only non-baseline
        // segments). A real delete here would erase the live baseline
        // bundle the new chain still points at — exactly the bug the
        // guard prevents.
        let store = MockStore::new();
        let key = "repo/refs/heads/main/cccccccccccccccccccccccccccccccccccccccc.bundle";
        store.insert(key, Bytes::from_static(b"live"));
        delete_prior_baseline_bundle(&store, &ref_main(), key, key).await;
        assert!(
            store.contains(key),
            "aliasing keys must not delete the live baseline bundle",
        );
    }

    #[tokio::test]
    async fn compact_reports_success_when_post_commit_baseline_delete_fails() {
        // Issue #113 regression: a non-NotFound delete error on the
        // prior baseline bundle after `write_chain` has committed
        // must NOT fail the compact. The new chain manifest is
        // already durable; on retry the chain would short-circuit
        // through `AlreadyMinimal` and never re-attempt cleanup, so
        // operators would observe a "failed" compact that actually
        // succeeded. Match the force-push baseline-cleanup contract:
        // log at warn and report success.
        let store = MockStore::new();
        let (_repo, _prior, c1, _c2, _c3) = lay_down_three_segment_chain(&store, "repo").await;

        // Arm a delete-time fault on the prior baseline bundle key.
        // The key is keyed off the fixture's c1 (= `full_at`).
        let prior_bundle_key = format!("repo/refs/heads/main/{c1}.bundle");
        store.arm(crate::object_store::mock::Fault::NetworkOnDelete {
            key: prior_bundle_key.clone(),
        });

        let outcome = compact(&store, Some("repo"), &ref_main(), opts(true))
            .await
            .expect("compact must report success when post-commit cleanup fails");
        assert_eq!(outcome.action, CompactAction::Compacted);

        // The new chain.json must be durable — readers see the new
        // single-segment chain at the new tip.
        let new_chain = crate::packchain::manifest::load_chain(&store, Some("repo"), &ref_main())
            .await
            .unwrap()
            .expect("chain present");
        assert_eq!(new_chain.segments.len(), 1);
        assert_eq!(new_chain.full_at, new_chain.tip);

        // The old baseline bundle is still on the bucket (the
        // delete failed) — the orphan is operator-visible via the
        // warn log. A second compact must observe AlreadyMinimal
        // and not re-attempt the cleanup, locking down the
        // short-circuit behaviour that motivates the best-effort
        // contract.
        assert!(
            store.contains(&prior_bundle_key),
            "delete fault must have left the prior baseline bundle in place",
        );
        let second = compact(&store, Some("repo"), &ref_main(), opts(true))
            .await
            .unwrap();
        assert_eq!(second.action, CompactAction::AlreadyMinimal);

        // Lock must still be released even though the helper logged
        // a warning.
        let lock = lock_key(Some("repo"), &ref_main());
        assert!(
            !store.contains(&lock),
            "compact must release the per-ref lock even on best-effort cleanup failure",
        );
    }

    #[tokio::test]
    async fn compact_round_trips_blobs_via_read_blob() {
        // Bytes-equivalence: every blob reachable from the new tip
        // must be byte-identical pre- and post-compact when read
        // through the public `read_blob` API.
        let store = MockStore::new();
        let (_repo, _prior, _c1, _c2, _c3) = lay_down_three_segment_chain(&store, "repo").await;
        let store_arc: Arc<dyn ObjectStore> = Arc::new(store.clone());

        // Capture pre-compact bytes for path "f2" (the file added at c3).
        let remote = crate::Remote::new_for_test(
            store_arc.clone(),
            "repo",
            crate::url::StorageEngine::Packchain,
        );
        // 1 MiB cache is plenty — test packs are tiny.
        let cache = crate::packchain::PackIndexCache::new(1024 * 1024);
        let pre = crate::packchain::read_blob(&remote, ref_main().as_str(), "f2", &cache)
            .await
            .expect("read_blob pre-compact");

        // Run compact.
        let outcome = compact(&store, Some("repo"), &ref_main(), opts(true))
            .await
            .unwrap();
        assert_eq!(outcome.action, CompactAction::Compacted);

        // Read post-compact through a fresh cache (the old idx is
        // orphan; a stale cached entry would mask a bug).
        let cache2 = crate::packchain::PackIndexCache::new(1024 * 1024);
        let post = crate::packchain::read_blob(&remote, ref_main().as_str(), "f2", &cache2)
            .await
            .expect("read_blob post-compact");
        assert_eq!(pre, post, "blob bytes must round-trip through compact");
    }

    /// Drive `download_chain_artefacts` with a single segment whose
    /// `pack` field is malformed and assert the typed error variant.
    /// Centralises the three regression cases for issue #120.
    async fn assert_download_chain_artefacts_rejects(pack: &str) {
        let store = MockStore::new();
        let download_dir = TempDir::new().unwrap();
        let chain = ChainManifest {
            v: 1,
            tip: sha40("3333333333333333333333333333333333333333"),
            full_at: sha40("3333333333333333333333333333333333333333"),
            segments: vec![ChainSegment {
                sha: sha40("3333333333333333333333333333333333333333"),
                parent_sha: None,
                pack: pack.to_owned(),
                bytes: 1_024,
            }],
        };

        let result = download_chain_artefacts(
            &store,
            Some("repo"),
            &ref_main(),
            &chain,
            download_dir.path(),
        )
        .await;
        match result {
            Err(PackchainError::MalformedPackEntry { offset: 0, reason }) => {
                assert!(
                    reason.contains(pack) || pack.is_empty(),
                    "reason should name the malformed pack key, got: {reason}",
                );
            }
            other => panic!("expected MalformedPackEntry for pack `{pack}`, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn download_chain_artefacts_rejects_empty_pack_key() {
        assert_download_chain_artefacts_rejects("").await;
    }

    #[tokio::test]
    async fn download_chain_artefacts_rejects_unstructured_pack_key() {
        assert_download_chain_artefacts_rejects("wrong").await;
    }

    #[tokio::test]
    async fn download_chain_artefacts_rejects_non_hex_pack_key() {
        assert_download_chain_artefacts_rejects("packs/notahex.pack").await;
    }
}

//! Packchain `push` handler — incremental upload via per-ref locking.
//!
//! Mirrors the bundle engine's [`crate::protocol::push`] in shape
//! (sequential per-ref, batch-driven, per-ref `PushOutcome` lines on
//! the wire) but writes the packchain on-bucket layout described in
//! [`super`]. The two engines share lock primitives, the
//! [`crate::protocol::push::PushOutcome`] type, and the
//! [`crate::protocol::push::NOT_ANCESTOR_TOKEN`] wire token; everything
//! else is independent.
//!
//! Stdout discipline (`.claude/rules/protocol-stdout.md`): the handler
//! returns `PushOutcome` values; the REPL ([`crate::protocol::run`])
//! renders them. `tracing::{debug, info, warn}` is the only diagnostic
//! channel — no `println!` / `eprintln!` / `dbg!`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use time::OffsetDateTime;
use tracing::{debug, info, warn};

use crate::git::{self, RefName, Sha};
use crate::keys;
use crate::object_store::{ObjectStore, ObjectStoreError, PutOpts};
use crate::protocol::push::{
    self as bundle_push, NOT_ANCESTOR_TOKEN, PushError, PushOutcome, PushSpec, acquire_lock,
    bundle_progress_sink, delete_idempotent, head_key, is_protected, lock_key, lock_ttl_from_env,
    parse_push_args, ref_listing_prefix,
};
use crate::url::StorageEngine;

use super::PackchainError;
use super::keys::{chain_key, pack_idx_key, pack_key};
use super::manifest::{load_chain, next_manifest, write_chain, write_path_index};
use super::pack::{BuiltPack, build_baseline_pack, build_incremental_pack};
use super::schema::{ChainManifest, ChainSegment, Sha40};

/// Per-batch configuration carried into [`push_one`] and below.
struct PushConfig {
    engine: StorageEngine,
    ttl: time::Duration,
}

/// Outcome of [`prepare_push`]: either pre-lock work completed and
/// [`push_one`] should proceed to acquire the lock, or the per-ref
/// outcome is already decided (delete, protection, ancestor mismatch,
/// shallow-rejection, idempotent same-SHA).
///
/// The [`ReadyState`] payload is sizeable (~200 bytes — paths, the
/// chain manifest, the temp dir guard); boxing it keeps the
/// `PrepareOutcome` enum compact regardless of variant.
enum PrepareOutcome {
    Ready(Box<ReadyState>),
    Done(PushOutcome),
}

/// All state captured pre-lock and consumed under the lock.
///
/// Pack/idx/baseline uploads happen in `prepare_push` (pre-lock), so
/// by the time this state crosses into `perform_push_under_lock` the
/// only on-bucket residue from a buggy abort is a set of orphan keys
/// (pack at content-SHA, baseline at tip-SHA) that `manage gc` reaps.
/// The under-lock work shrinks to path-index walk + chain.json /
/// FORMAT / HEAD writes — bounded by JSON-PUT latency, not pack size.
struct ReadyState {
    remote_ref: RefName,
    local_sha: Sha,
    local_sha40: Sha40,
    /// Working directory captured at probe time. The under-lock
    /// path-index walker re-opens the repo here without depending on
    /// the surrounding `BatchCtx`'s `repo_dir` (which can be the
    /// `.git/` directory rather than the workdir).
    cwd: PathBuf,
    /// Pre-lock chain snapshot. `None` on first push.
    prior: Option<ChainManifest>,
    /// Pack content SHA + size — used by [`perform_push_under_lock`]
    /// to construct the new chain segment without reopening the pack.
    /// The pack file itself is already uploaded by the time we get
    /// here.
    pack_content_sha: Sha40,
    pack_bytes: u64,
    /// Force-flag from the parsed [`PushSpec`].
    force: bool,
    /// Owns the temp dir that backed the pack/idx/baseline files
    /// during the upload phase. Dropped after `perform_push_under_lock`
    /// returns; the on-bucket copies survive.
    _temp_dir: tempfile::TempDir,
}

/// Recoverable per-push errors discovered during local git work.
#[derive(Debug, Clone, Copy)]
enum GitProbeError {
    LocalRefNotFound,
    NotAncestor,
    Shallow,
}

/// Captured local-git output: resolved SHA + repo working dir.
struct LocalGit {
    local_sha: Sha,
    cwd: PathBuf,
}

/// Drive a batch of `push` commands sequentially against the packchain
/// engine. Each command runs under its own per-ref lock; per-ref
/// failures (lock contention, stale chain, ancestor mismatch, shallow
/// rejection) become [`PushOutcome::Error`] lines so the batch can
/// continue. Catastrophic failures (transport, malformed protocol)
/// abort with [`PushError`].
pub(crate) async fn push_batch(
    ctx: &super::super::protocol::BatchCtx,
    engine: StorageEngine,
    cmds: Vec<String>,
) -> Result<Vec<PushOutcome>, PushError> {
    if cmds.is_empty() {
        return Ok(Vec::new());
    }
    debug!(count = cmds.len(), engine = %engine, "processing packchain push batch");

    let config = PushConfig {
        engine,
        ttl: lock_ttl_from_env(),
    };
    let mut outcomes = Vec::with_capacity(cmds.len());

    for cmd in cmds {
        let spec = parse_push_args(&cmd)?;
        let remote_ref_str = spec.remote_ref.as_str().to_owned();
        let outcome = match push_one(
            ctx.store.as_ref(),
            ctx.prefix.as_deref(),
            ctx.repo_dir.as_path(),
            &config,
            OffsetDateTime::now_utc(),
            spec,
        )
        .await
        {
            Ok(o) => o,
            // Operational failures (transport, local git, local I/O,
            // packchain engine errors) become per-ref `error` lines so
            // the batch can continue. Mirrors bundle's policy at
            // src/protocol/push.rs:365-377.
            Err(e)
                if matches!(
                    e,
                    PushError::Store(_)
                        | PushError::Git(_)
                        | PushError::Io(_)
                        | PushError::Sha(_)
                        | PushError::Packchain(_)
                ) =>
            {
                let chain = full_error_chain(&e);
                warn!(ref_name = %remote_ref_str, error = %chain, "packchain push ref failed");
                PushOutcome::Error {
                    remote_ref: remote_ref_str,
                    message: format!(r#""{chain}"?"#),
                }
            }
            Err(e) => return Err(e),
        };
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// Render a [`PushError`] as a colon-separated chain. Mirrors bundle's
/// helper at `src/protocol/push.rs::full_error_chain` so wire output
/// is uniform across engines.
fn full_error_chain(err: &PushError) -> String {
    let mut msg = err.to_string();
    crate::protocol::append_source_chain(&mut msg, err);
    msg
}

/// Execute one push: prepare, lock, upload, release. Lock release is
/// unconditional; the post-result `match` mirrors bundle's policy at
/// `src/protocol/push.rs:656-676` (lock-release failure overrides a
/// successful push but never masks a push error).
async fn push_one(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    repo_dir: &Path,
    config: &PushConfig,
    now: OffsetDateTime,
    spec: PushSpec,
) -> Result<PushOutcome, PushError> {
    let state = match prepare_push(store, prefix, repo_dir, spec).await? {
        PrepareOutcome::Done(o) => return Ok(o),
        PrepareOutcome::Ready(s) => s,
    };

    let remote_ref_str = state.remote_ref.as_str().to_owned();
    let lock = lock_key(prefix, &state.remote_ref);
    let acquired = acquire_lock(store, &lock, config.ttl, now).await?;
    if !acquired {
        return Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message: format!(
                r#""failed to acquire ref lock at {lock}. Another client may be pushing. If this persists beyond {}s, run git-remote-object-store doctor to inspect and optionally clear stale locks."?"#,
                config.ttl.whole_seconds(),
            ),
        });
    }

    let result = perform_push_under_lock(store, prefix, config.engine, *state).await;
    let release_result = bundle_push::release_lock(store, &lock).await;

    match (&result, release_result) {
        (Ok(PushOutcome::Ok { .. }), Err(e)) => {
            warn!(key = %lock, error = %e, "packchain failed to release lock");
            Ok(PushOutcome::Error {
                remote_ref: remote_ref_str,
                message: format!(
                    r#""failed to release lock. You may need to manually remove the lock {lock} from the server or use git-remote-object-store doctor to fix."?"#,
                ),
            })
        }
        (_, Err(e)) => {
            warn!(key = %lock, error = %e, "packchain lock release failed (push already errored)");
            result
        }
        _ => result,
    }
}

/// All pre-lock work for a push: protect check, local git probe, chain
/// snapshot, idempotency short-circuit, pack build, baseline bundle
/// build (first / force push). The `gix::Repository` handle is dropped
/// inside helper scopes so the surrounding future stays `Send`.
async fn prepare_push(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    repo_dir: &Path,
    spec: PushSpec,
) -> Result<PrepareOutcome, PushError> {
    let PushSpec {
        force,
        local_spec,
        remote_ref,
    } = spec;
    let remote_ref_str = remote_ref.as_str().to_owned();

    // Delete refspec → packchain-specific cleanup (no .bundle counting,
    // no `repo.zip`).
    if local_spec.is_empty() {
        let outcome = delete_remote_ref_packchain(store, prefix, &remote_ref).await?;
        return Ok(PrepareOutcome::Done(outcome));
    }

    // Force push against a `PROTECTED#` marker is rejected before any
    // pack work. Mirror bundle's gate exactly.
    let force_push = if force {
        !is_protected(store, prefix, &remote_ref).await?
    } else {
        false
    };
    debug!(local = %local_spec, remote = %remote_ref, force_push, "packchain push");

    // Pre-lock chain snapshot. Used by the stale-tip guard under the
    // lock and by the prior_tip ancestor / incremental-pack-base.
    let prior = load_chain(store, prefix, &remote_ref)
        .await
        .map_err(PushError::Packchain)?;
    let prior_tip_sha: Option<Sha> = match prior.as_ref() {
        Some(c) => Some(Sha::from_hex(c.tip.as_str()).map_err(PushError::Sha)?),
        None => None,
    };

    // Sync gix work runs in a separate scope so the `!Sync` Repository
    // handle is dropped before any .await.
    let probe = local_git_work_packchain(repo_dir, &local_spec, prior_tip_sha, force_push)?;
    let local = match probe {
        Ok(local) => local,
        Err(probe_err) => {
            return Ok(PrepareOutcome::Done(probe_error_to_outcome(
                probe_err,
                remote_ref_str,
                &local_spec,
            )));
        }
    };

    let local_sha40 = Sha40::try_new(local.local_sha.to_string()).map_err(PushError::Packchain)?;

    // Idempotency short-circuit: same tip means no bucket changes.
    // Bundle engine's `same-bundle no-op` analogue.
    if !force_push && prior.as_ref().map(|c| &c.tip) == Some(&local_sha40) {
        info!(
            ref_name = %remote_ref,
            tip = %local_sha40.as_str(),
            "packchain push: same tip already on bucket, no-op",
        );
        return Ok(PrepareOutcome::Done(PushOutcome::Ok {
            remote_ref: remote_ref_str,
        }));
    }

    let temp_dir = tempfile::Builder::new()
        .prefix("git_remote_object_store_packchain_")
        .tempdir()?;
    let local_sha = local.local_sha;
    // Pack kind encodes the invariant the bool / Option pair only
    // hinted at: a baseline pack carries no prerequisite; an
    // incremental pack carries exactly one. The compiler now enforces
    // it, so `build_pack_and_baseline` no longer needs an `expect`.
    let kind = match (force_push, prior_tip_sha) {
        (true, _) | (false, None) => PackKind::Baseline,
        (false, Some(prior_tip)) => PackKind::Incremental { prior_tip },
    };
    let (pack, baseline_bundle) = build_pack_and_baseline(
        local.cwd.clone(),
        temp_dir.path().to_owned(),
        local_sha,
        kind,
        local_spec.clone(),
    )
    .await?;

    // Pre-lock upload of pack + idx + (optional) baseline bundle.
    // Bounding lock-hold time is the design intent (see `super`'s
    // module doc on linearization): two pushers that race both
    // upload their packs before contending for the lock; the loser
    // sees `stale chain` after re-reading and returns without
    // touching chain.json, leaving its pack as an orphan for `manage gc`
    // GC. A single push uploads each of pack/idx/baseline exactly
    // once.
    upload_pack_idx_baseline(
        store,
        prefix,
        &remote_ref,
        local_sha,
        &pack,
        baseline_bundle.as_deref(),
    )
    .await?;

    Ok(PrepareOutcome::Ready(Box::new(ReadyState {
        remote_ref,
        local_sha,
        local_sha40,
        cwd: local.cwd,
        prior,
        pack_content_sha: pack.content_sha,
        pack_bytes: pack.pack_bytes,
        force: force_push,
        _temp_dir: temp_dir,
    })))
}

/// Convert a [`GitProbeError`] into the per-ref [`PushOutcome::Error`]
/// the wire wants. Pulled out of [`prepare_push`] so the latter stays
/// under clippy's 100-line ceiling.
fn probe_error_to_outcome(
    err: GitProbeError,
    remote_ref_str: String,
    local_spec: &str,
) -> PushOutcome {
    let message = match err {
        GitProbeError::LocalRefNotFound => format!(r#""{local_spec} not found"?"#),
        GitProbeError::NotAncestor => {
            format!(r#""remote ref is {NOT_ANCESTOR_TOKEN} of {local_spec}."?"#)
        }
        GitProbeError::Shallow => {
            r#""cannot push from a shallow clone: rev-walk crosses a shallow boundary"?"#.to_owned()
        }
    };
    PushOutcome::Error {
        remote_ref: remote_ref_str,
        message,
    }
}

/// What kind of pack a push needs to build.
///
/// Encoded as an enum (rather than a `bool` + `Option<Sha>` pair) so
/// the invariant "incremental ⟺ has prerequisite" is enforced by the
/// compiler — no `expect("guarded above")` on the prior-tip access
/// inside the build closure.
#[derive(Debug, Clone, Copy)]
enum PackKind {
    /// Full snapshot from the local tip — first push or force push.
    Baseline,
    /// Thin pack reachable from the local tip but not from
    /// `prior_tip` (the prior chain.tip).
    Incremental { prior_tip: Sha },
}

/// Run the (possibly slow) pack + baseline bundle build off the
/// runtime so the `!Sync` `gix::Repository` never crosses an `.await`.
async fn build_pack_and_baseline(
    cwd: PathBuf,
    temp_path: PathBuf,
    local_sha: Sha,
    kind: PackKind,
    local_spec: String,
) -> Result<(BuiltPack, Option<PathBuf>), PushError> {
    let result = tokio::task::spawn_blocking(move || {
        let (pack, needs_baseline) = match kind {
            PackKind::Baseline => (build_baseline_pack(&cwd, local_sha, &temp_path)?, true),
            PackKind::Incremental { prior_tip } => (
                build_incremental_pack(&cwd, prior_tip, local_sha, &temp_path)?,
                false,
            ),
        };
        let baseline = if needs_baseline {
            // bundle::create reuses the bundle-engine code path verbatim
            // — no drift between the two engines' baseline shapes.
            let bundle_path = crate::bundle::create(&cwd, &temp_path, local_sha, &local_spec)
                .map_err(|e| PackchainError::PackBuild(format!("baseline bundle: {e}")))?;
            Some(bundle_path)
        } else {
            None
        };
        Ok::<_, PackchainError>((pack, baseline))
    })
    .await
    .map_err(|join_err| std::io::Error::other(join_err.to_string()))?;
    result.map_err(PushError::Packchain)
}

/// Local-git probe: resolve the spec, optionally check ancestry, run
/// the shallow-clone rejection. Drops the [`gix::Repository`] before
/// returning so the caller's future stays `Send`.
fn local_git_work_packchain(
    repo_dir: &Path,
    local_spec: &str,
    prior_tip: Option<Sha>,
    force_push: bool,
) -> Result<Result<LocalGit, GitProbeError>, PushError> {
    let repo = gix::open(repo_dir).map_err(|e| PushError::Git(crate::git::GitError::from(e)))?;
    let cwd = repo.workdir().unwrap_or_else(|| repo.git_dir()).to_owned();

    let Ok(local_sha) = git::branch::resolve(&repo, local_spec) else {
        return Ok(Err(GitProbeError::LocalRefNotFound));
    };

    if let (Some(prior), false) = (prior_tip, force_push)
        && !git::is_ancestor(&repo, prior, local_sha).map_err(PushError::Git)?
    {
        return Ok(Err(GitProbeError::NotAncestor));
    }

    if rev_walk_crosses_shallow_boundary(&repo, local_sha).map_err(PushError::Packchain)? {
        return Ok(Err(GitProbeError::Shallow));
    }

    drop(repo);
    Ok(Ok(LocalGit { local_sha, cwd }))
}

/// Returns `true` when `tip` is reachable through a `.git/shallow`
/// boundary commit — i.e. the rev-walk would yield a commit whose
/// parents are missing from the local ODB. Pushing such a tip would
/// produce permanently incomplete history on the server.
///
/// Errors are mapped onto [`PackchainError::PackBuild`] (carries the
/// rendered message) since `GitError` does not have a generic-error
/// variant. We never lose information — the underlying error's
/// `Display` is preserved.
fn rev_walk_crosses_shallow_boundary(
    repo: &gix::Repository,
    tip: Sha,
) -> Result<bool, PackchainError> {
    let Some(commits) = repo
        .shallow_commits()
        .map_err(|e| PackchainError::PackBuild(format!("read .git/shallow: {e}")))?
    else {
        return Ok(false);
    };
    let boundary: HashSet<gix_hash::ObjectId> = commits.iter().copied().collect();
    let walker = repo
        .rev_walk([*tip.as_object_id()])
        .all()
        .map_err(|e| PackchainError::PackBuild(format!("rev-walk for shallow check: {e}")))?;
    for info in walker {
        let info = info.map_err(|e| PackchainError::PackBuild(format!("rev-walk step: {e}")))?;
        if boundary.contains(&info.id) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Under-lock body. By the time this runs, pack/idx/baseline are
/// already on the bucket (uploaded pre-lock in [`prepare_push`]) — the
/// remaining work is the path-index walk + FORMAT/HEAD bootstrap +
/// chain.json commit. Lock-hold time is bounded by JSON-PUT latency,
/// not pack size. See [`super`]'s module doc on the linearization
/// invariant: chain.json is the commit point and must be the LAST
/// referenced-key write.
async fn perform_push_under_lock(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    engine: StorageEngine,
    state: ReadyState,
) -> Result<PushOutcome, PushError> {
    let ReadyState {
        remote_ref,
        local_sha,
        local_sha40,
        cwd,
        prior,
        pack_content_sha,
        pack_bytes,
        force,
        _temp_dir,
    } = state;
    let remote_ref_str = remote_ref.as_str().to_owned();

    // 1. Re-read chain.json under the lock.
    let current = load_chain(store, prefix, &remote_ref)
        .await
        .map_err(PushError::Packchain)?;

    // 2. Stale-tip guard (skipped on force). Pre-lock uploads of pack
    //    + idx (and baseline, when applicable) become orphans for
    //    `manage gc`.
    if !force {
        let pre_tip = prior.as_ref().map(|c| &c.tip);
        let cur_tip = current.as_ref().map(|c| &c.tip);
        if pre_tip != cur_tip {
            return Ok(PushOutcome::Error {
                remote_ref: remote_ref_str,
                message: r#""stale chain. Please fetch and retry."?"#.to_owned(),
            });
        }
    }
    let _ = local_sha; // silences `unused` until/unless re-introduced

    // 3. Re-walk tree to build path-index. Walks from the resolved
    //    local_sha (not local_spec) so a concurrent local ref move
    //    cannot perturb the tree we're writing. Runs in
    //    spawn_blocking so the !Sync repo handle never crosses
    //    `.await`. `cwd` is moved into the closure (it's not used
    //    again after this point); `local_sha: Sha` is `Copy`.
    let path_index = tokio::task::spawn_blocking(move || -> Result<_, PackchainError> {
        let repo = gix::open(&cwd).map_err(crate::git::GitError::from)?;
        super::git::extract_path_index(&repo, local_sha)
    })
    .await
    .map_err(|join_err| std::io::Error::other(join_err.to_string()))?
    .map_err(PushError::Packchain)?;

    // 4. path-index.json — overwrite (must precede chain.json so the
    //    `chain.tip == path_index.commit` invariant holds for any
    //    reader who trusts the chain).
    write_path_index(store, prefix, &remote_ref, &path_index)
        .await
        .map_err(PushError::Packchain)?;

    // 5. FORMAT bootstrap (idempotent — every push past the first is
    //    a no-op).
    let format_key = keys::join(prefix.unwrap_or(""), "FORMAT");
    store
        .put_if_absent(&format_key, Bytes::from_static(engine.as_str().as_bytes()))
        .await?;

    // 6. HEAD bootstrap (idempotent — first ref to push wins).
    let head = head_key(prefix);
    store
        .put_if_absent(
            &head,
            Bytes::copy_from_slice(remote_ref.as_str().as_bytes()),
        )
        .await?;

    // 7. Build new chain manifest. `next_manifest` produces the
    //    correct `parent_sha` itself (None for force / first push,
    //    `prior.tip` for incremental) — we don't precompute it here.
    let new_segment = ChainSegment {
        sha: local_sha40.clone(),
        parent_sha: None, // `next_manifest` fills this in for the incremental path
        // Pack key is bucket-relative (prefix-stripped) for storage in
        // chain.json — readers reapply the prefix at fetch time.
        pack: pack_key(None, &pack_content_sha),
        bytes: pack_bytes,
    };
    let manifest = next_manifest(prior.as_ref(), &local_sha40, new_segment, force);

    // 8. chain.json — THE commit point. After this PUT returns the
    //    push is durable.
    write_chain(store, prefix, &remote_ref, &manifest)
        .await
        .map_err(PushError::Packchain)?;

    // 9. Force-push old-baseline cleanup (best-effort, post-commit).
    if force {
        force_push_baseline_cleanup(store, prefix, &remote_ref, prior.as_ref(), &local_sha40).await;
    }

    Ok(PushOutcome::Ok {
        remote_ref: remote_ref_str,
    })
}

/// Upload pack + idx + (optional) baseline bundle. Each upload uses
/// `put_path` for streaming and a [`bundle_progress_sink`] for stderr
/// progress lines. Pulled out of [`perform_push_under_lock`] so the
/// latter stays under clippy's 100-line ceiling.
async fn upload_pack_idx_baseline(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    remote_ref: &RefName,
    local_sha: Sha,
    pack: &BuiltPack,
    baseline_bundle: Option<&Path>,
) -> Result<(), PushError> {
    let pack_dest = pack_key(prefix, &pack.content_sha);
    upload_with_progress(store, &pack_dest, &pack.pack_path, Some(pack.pack_bytes)).await?;

    let idx_dest = pack_idx_key(prefix, &pack.content_sha);
    upload_with_progress(
        store,
        &idx_dest,
        &pack.idx_path,
        file_len(&pack.idx_path).await,
    )
    .await?;

    if let Some(bundle_path) = baseline_bundle {
        let bundle_dest = keys::bundle_key(prefix, remote_ref, local_sha);
        upload_with_progress(
            store,
            &bundle_dest,
            bundle_path,
            file_len(bundle_path).await,
        )
        .await?;
    }
    Ok(())
}

/// Stat `path` and return its byte length, swallowing errors.
///
/// `bundle_progress_sink` accepts `Option<u64>` and renders "unknown"
/// for `None`; that is the right degradation for a stat failure on a
/// tempdir we just wrote (the upload's own size accounting is
/// independent of this hint).
async fn file_len(path: &Path) -> Option<u64> {
    tokio::fs::metadata(path).await.map(|m| m.len()).ok()
}

/// Stream `src` to `dest_key` with a progress sink wired to the
/// stderr `tracing` channel. `total_hint` is what the progress sink
/// renders for "X / total" lines — it's a hint, not a contract.
async fn upload_with_progress(
    store: &dyn ObjectStore,
    dest_key: &str,
    src: &Path,
    total_hint: Option<u64>,
) -> Result<(), PushError> {
    let opts = PutOpts {
        progress: Some(bundle_progress_sink(dest_key, total_hint)),
        ..PutOpts::default()
    };
    store.put_path(dest_key, src, opts).await?;
    Ok(())
}

/// Best-effort delete of the prior baseline bundle after a force push
/// has already committed (i.e. `chain.json` has been overwritten).
/// Failure here cannot fail the push: we log at `warn` so an operator
/// notices the orphan and `manage gc` sweeps it later.
async fn force_push_baseline_cleanup(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    remote_ref: &RefName,
    prior: Option<&ChainManifest>,
    local_sha40: &Sha40,
) {
    let Some(prior) = prior else {
        return;
    };
    if &prior.full_at == local_sha40 {
        return;
    }
    let prior_full_sha = match Sha::from_hex(prior.full_at.as_str()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "skipping force-push baseline cleanup: invalid prior full_at");
            return;
        }
    };
    let old_baseline_key = keys::bundle_key(prefix, remote_ref, prior_full_sha);
    if let Err(e) = delete_idempotent(store, &old_baseline_key).await {
        warn!(
            key = %old_baseline_key,
            error = %e,
            "force-push baseline cleanup failed (push already committed)",
        );
    }
}

/// Delete a packchain-engine ref: remove `chain.json`, `path-index.json`,
/// and the baseline bundle. Pack files are NOT deleted (they may be
/// referenced by other branches; `manage gc` reaps unreferenced packs).
///
/// Returns `Ok(PushOutcome::Error{ "not found"? })` when no chain.json
/// exists; `Ok(PushOutcome::Ok)` when the chain is removed (other
/// keys are best-effort).
async fn delete_remote_ref_packchain(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    remote_ref: &RefName,
) -> Result<PushOutcome, PushError> {
    let chain = chain_key(prefix, remote_ref);
    let remote_ref_str = remote_ref.as_str().to_owned();

    // Probe via head: NotFound → "not found" wire error.
    match store.head(&chain).await {
        Ok(_) => {}
        Err(ObjectStoreError::NotFound(_)) => {
            return Ok(PushOutcome::Error {
                remote_ref: remote_ref_str,
                message: r#""not found"?"#.to_owned(),
            });
        }
        Err(e) => return Err(PushError::Store(e)),
    }

    // Listing under the ref prefix may include the baseline bundle and
    // other per-ref artifacts; sweep them all. Pack files live under
    // a sibling `packs/` prefix and are intentionally not touched.
    let listing = ref_listing_prefix(prefix, remote_ref);
    let entries = store.list(&listing).await?;
    for entry in &entries {
        delete_idempotent(store, &entry.key).await?;
    }

    Ok(PushOutcome::Ok {
        remote_ref: remote_ref_str,
    })
}

#[cfg(test)]
mod tests {
    use super::super::keys::path_index_key;
    use super::*;
    use crate::object_store::mock::MockStore;

    fn rn(s: &str) -> RefName {
        RefName::new(s).unwrap()
    }

    // --- delete_remote_ref_packchain -----------------------------------

    #[tokio::test]
    async fn delete_returns_not_found_when_chain_absent() {
        let store = MockStore::new();
        let outcome = delete_remote_ref_packchain(&store, None, &rn("refs/heads/main"))
            .await
            .unwrap();
        match outcome {
            PushOutcome::Error { message, .. } => {
                assert!(message.contains("not found"), "got: {message}");
            }
            PushOutcome::Ok { .. } => panic!("expected Error, got {outcome:?}"),
        }
    }

    #[tokio::test]
    async fn delete_sweeps_chain_path_index_and_baseline() {
        let store = MockStore::new();
        let prefix = Some("repo");
        let remote = rn("refs/heads/main");
        let baseline_sha = Sha::from_hex("0000000000000000000000000000000000000001").unwrap();
        let baseline_key = keys::bundle_key(prefix, &remote, baseline_sha);
        // Seed chain + path-index + a baseline bundle.
        store.insert(
            chain_key(prefix, &remote),
            Bytes::from_static(b"{\"v\":1,\"tip\":\"0000000000000000000000000000000000000001\",\"full_at\":\"0000000000000000000000000000000000000001\",\"segments\":[]}"),
        );
        store.insert(path_index_key(prefix, &remote), Bytes::from_static(b"{}"));
        store.insert(&baseline_key, Bytes::from_static(b"PACK"));

        let outcome = delete_remote_ref_packchain(&store, prefix, &remote)
            .await
            .unwrap();
        assert!(matches!(outcome, PushOutcome::Ok { .. }));
        assert!(!store.contains(&chain_key(prefix, &remote)));
        assert!(!store.contains(&path_index_key(prefix, &remote)));
        // The test name asserts the baseline bundle is also swept;
        // without this check, a regression that filtered the listing
        // to chain + path-index only would still pass.
        assert!(
            !store.contains(&baseline_key),
            "baseline bundle at {baseline_key} must also be deleted",
        );
    }
}

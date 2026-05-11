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
use std::sync::Arc;

use bytes::Bytes;
use time::OffsetDateTime;
use tracing::{debug, info, warn};

use crate::git::{self, PeeledTip, RefName, Sha};
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

/// Captured local-git output: resolved SHA + repo working dir + the
/// fully-peeled local tip and prior tip.
///
/// `local_sha` is the ref's actual target — the tag OID for an
/// annotated tag, the commit OID for a branch or lightweight tag, or
/// the tree/blob OID for a bare-tree / bare-blob ref. `peeled` carries
/// the leaf kind plus the tag-object chain encountered while peeling;
/// the pack-build path uses it to decide whether to walk commits, walk
/// a tree closure, or pack a single blob.
///
/// `prior_commit` is the peeled prior chain.tip's commit, needed by the
/// incremental pack's `with_hidden` walk and by the ancestry check.
/// `None` on first push or when either side is non-commit (kind
/// mismatch forces a full segment).
struct LocalGit {
    local_sha: Sha,
    peeled: PeeledTip,
    prior_commit: Option<Sha>,
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
            Arc::clone(&ctx.store),
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
    store: Arc<dyn ObjectStore>,
    prefix: Option<&str>,
    repo_dir: &Path,
    config: &PushConfig,
    now: OffsetDateTime,
    spec: PushSpec,
) -> Result<PushOutcome, PushError> {
    let state = match prepare_push(Arc::clone(&store), prefix, repo_dir, config, now, spec).await? {
        PrepareOutcome::Done(o) => return Ok(o),
        PrepareOutcome::Ready(s) => s,
    };

    let remote_ref_str = state.remote_ref.as_str().to_owned();
    let lock = lock_key(prefix, &state.remote_ref);
    let Some(guard) = acquire_lock(Arc::clone(&store), &lock, config.ttl, now).await? else {
        return Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message: format!(
                r#""failed to acquire ref lock at {lock}. Another client may be pushing. If this persists beyond {}s, run git-remote-object-store doctor to inspect and optionally clear stale locks."?"#,
                config.ttl.whole_seconds(),
            ),
        });
    };

    let result = perform_push_under_lock(store.as_ref(), prefix, config.engine, *state).await;
    let release_result = bundle_push::release_lock(guard).await;

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
    store: Arc<dyn ObjectStore>,
    prefix: Option<&str>,
    repo_dir: &Path,
    config: &PushConfig,
    now: OffsetDateTime,
    spec: PushSpec,
) -> Result<PrepareOutcome, PushError> {
    let PushSpec {
        force,
        local_spec,
        remote_ref,
    } = spec;
    let remote_ref_str = remote_ref.as_str().to_owned();

    // Delete refspec → packchain-specific cleanup (no .bundle counting,
    // no `repo.zip`). Delete takes the per-ref lock so a concurrent push
    // cannot lose mutual exclusion via the sweep removing `LOCK#.lock`
    // (#116).
    if local_spec.is_empty() {
        let outcome = delete_remote_ref_packchain(store, prefix, &remote_ref, config, now).await?;
        return Ok(PrepareOutcome::Done(outcome));
    }

    let store_ref = store.as_ref();
    // Force push against a `PROTECTED#` marker is rejected before any
    // pack work. Mirror bundle's gate exactly.
    let force_push = if force {
        !is_protected(store_ref, prefix, &remote_ref).await?
    } else {
        false
    };
    debug!(local = %local_spec, remote = %remote_ref, force_push, "packchain push");

    // Pre-lock chain snapshot. Used by the stale-tip guard under the
    // lock and by the prior_tip ancestor / incremental-pack-base.
    let prior = load_chain(store_ref, prefix, &remote_ref)
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

    let local_sha40 =
        Sha40::from_oid(local.local_sha.as_object_id()).map_err(PushError::Packchain)?;

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
    //
    // Incremental packs are commit-only: tree/blob-tipped pushes always
    // emit a full segment (no rev-walk to compare). The probe above
    // sets `prior_commit` to `None` whenever the new tip is non-commit,
    // so the match below collapses to `Baseline` for those cases without
    // a separate guard here.
    let kind = match (force_push, local.prior_commit) {
        (true, _) | (false, None) => PackKind::Baseline,
        (false, Some(prior_commit)) => PackKind::Incremental { prior_commit },
    };
    let (pack, baseline_bundle) = build_pack_and_baseline(
        local.cwd.clone(),
        temp_dir.path().to_owned(),
        local_sha,
        local.peeled,
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
        store_ref,
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
    /// Full snapshot from the local tip — first push, force push, or
    /// any push whose tip kind is non-commit.
    Baseline,
    /// Thin pack reachable from the local tip but not from
    /// `prior_commit` (the prior chain.tip, peeled to its commit so
    /// the incremental walk's `with_hidden` sees a commit OID even if
    /// the previous push was for an annotated tag). Only used when
    /// both the new and prior tips peel to a commit.
    Incremental { prior_commit: Sha },
}

/// Run the (possibly slow) pack + baseline bundle build off the
/// runtime so the `!Sync` `gix::Repository` never crosses an `.await`.
async fn build_pack_and_baseline(
    cwd: PathBuf,
    temp_path: PathBuf,
    local_sha: Sha,
    peeled: PeeledTip,
    kind: PackKind,
    local_spec: String,
) -> Result<(BuiltPack, Option<PathBuf>), PushError> {
    let result = tokio::task::spawn_blocking(move || {
        let (pack, needs_baseline) = match kind {
            PackKind::Baseline => (build_baseline_pack(&cwd, peeled, &temp_path)?, true),
            PackKind::Incremental { prior_commit } => {
                // Incremental is only reached when both sides are
                // commit-tipped (push.rs gates this). Destructure to
                // pull the local commit + tag_chain back out of the
                // PeeledTip; any non-Commit variant here is a bug in
                // the gating, surfaced by `expect`.
                let PeeledTip::Commit {
                    commit: local_commit,
                    tag_chain,
                } = peeled
                else {
                    return Err(PackchainError::PackBuild(
                        "incremental pack requires commit-tipped peel; non-commit peel reached \
                         build_pack_and_baseline — push dispatch is buggy"
                            .to_owned(),
                    ));
                };
                (
                    build_incremental_pack(
                        &cwd,
                        prior_commit,
                        local_commit,
                        &tag_chain,
                        &temp_path,
                    )?,
                    false,
                )
            }
        };
        let baseline = if needs_baseline {
            // bundle::create reuses the bundle-engine code path verbatim
            // — no drift between the two engines' baseline shapes. The
            // bundle engine peels and includes the tag chain itself
            // (see src/bundle.rs), so passing the unpeeled tag OID as
            // `local_sha` is correct: the bundle file is named after
            // the ref's actual target, and its pack contents include
            // both the commit-reachable graph and the tag chain.
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

    // Peel the resolved OID through any annotated-tag chain. The leaf
    // kind decides the pack shape: commit-tipped goes through the
    // rev-walk path; tree-tipped and blob-tipped force a full segment
    // and skip ancestry / shallow checks (those are commit-graph
    // concerns).
    let peeled = git::peel_tag_chain(&repo, local_sha).map_err(PushError::Git)?;

    let local_commit = match &peeled {
        PeeledTip::Commit { commit, .. } => Some(*commit),
        PeeledTip::Tree { .. } | PeeledTip::Blob { .. } => None,
    };

    // Compute the peeled prior commit and check ancestry. We pass
    // commits to `is_ancestor` (gix's `merge_base` does not peel tag
    // OIDs internally — see gix-0.83 `repository/revision.rs`), so a
    // non-force tag re-push gets a clean `NotAncestor` rejection rather
    // than a confusing merge-base error.
    //
    // Peeling fails when the prior tip is not in the local ODB — the
    // synthesised remote-only OID in `non_force_push_rejects_when_remote_not_ancestor`
    // and the unrelated-history case in production. Treat
    // `GitError::FindObject` as not-an-ancestor; propagate other peel
    // errors so a corrupted ODB surfaces a real diagnostic instead of
    // being masked as a refusal.
    //
    // If either side is non-commit (tag-of-tree, tag-of-blob, bare-tree,
    // bare-blob), ancestry is undefined and we reject the non-force
    // push. The user must force-push to convert kinds — same contract
    // git itself uses for tag updates that aren't fast-forwards.
    let prior_commit = if !force_push && let Some(prior) = prior_tip {
        match (local_commit, git::peel_tag_chain(&repo, prior)) {
            (Some(local_commit_oid), Ok(PeeledTip::Commit { commit, .. })) => {
                if !git::is_ancestor(&repo, commit, local_commit_oid).map_err(PushError::Git)? {
                    return Ok(Err(GitProbeError::NotAncestor));
                }
                Some(commit)
            }
            // Either side non-commit ⇒ kind mismatch ⇒ NotAncestor.
            // FindObject on the prior tip means it's not in the local
            // ODB (synthesised remote-only OID, unrelated history) —
            // also surface as NotAncestor.
            (None, _)
            | (
                _,
                Ok(PeeledTip::Tree { .. } | PeeledTip::Blob { .. })
                | Err(crate::git::GitError::FindObject(_)),
            ) => {
                return Ok(Err(GitProbeError::NotAncestor));
            }
            (_, Err(e)) => return Err(PushError::Git(e)),
        }
    } else {
        None
    };

    // Shallow-boundary check is a commit-graph property; only meaningful
    // when the local tip peels to a commit.
    if let Some(local_commit_oid) = local_commit
        && rev_walk_crosses_shallow_boundary(&repo, local_commit_oid)
            .map_err(PushError::Packchain)?
    {
        return Ok(Err(GitProbeError::Shallow));
    }

    drop(repo);
    Ok(Ok(LocalGit {
        local_sha,
        peeled,
        prior_commit,
        cwd,
    }))
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
/// remaining work is the path-index walk, the FORMAT/HEAD bootstrap,
/// the chain.json commit, and the post-commit path-index PUT.
/// Lock-hold time is bounded by JSON-PUT latency, not pack size. See
/// [`super`]'s module doc on the linearization invariant: chain.json
/// is the commit point, and `path-index.json` is written AFTER it so
/// the worst observable crash window is a stale `path_index.tip`
/// paired with a fresh `chain.tip` (which readers detect and surface
/// as [`PackchainError::TransientChainPathIndexMismatch`], issue #114).
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
    // 3. Re-walk tree to build path-index. Walks from the resolved
    //    local_sha (not local_spec) so a concurrent local ref move
    //    cannot perturb the tree we're writing. Runs in
    //    spawn_blocking so the !Sync repo handle never crosses
    //    `.await`. `cwd` is moved into the closure (it's not used
    //    again after this point); `local_sha: Sha` is `Copy`.
    //
    //    The PUT is deferred to step 9 (after chain.json) so a crash
    //    between the two leaves the bucket with `chain.tip` ahead of
    //    `path_index.tip` rather than the other way around — see the
    //    module-level "chain.json → path-index.json ordering" doc and
    //    issue #114. Returns `None` for blob-tipped chains; the engine
    //    then omits `path-index.json` entirely.
    let path_index = tokio::task::spawn_blocking(move || -> Result<_, PackchainError> {
        let repo = gix::open(&cwd).map_err(crate::git::GitError::from)?;
        let peeled = git::peel_tag_chain(&repo, local_sha).map_err(PackchainError::Git)?;
        super::git::extract_path_index(&repo, &peeled, local_sha)
    })
    .await
    .map_err(|join_err| std::io::Error::other(join_err.to_string()))?
    .map_err(PushError::Packchain)?;

    // 4. FORMAT bootstrap (idempotent — every push past the first is
    //    a no-op).
    let format_key = keys::join(prefix, "FORMAT");
    store
        .put_if_absent(&format_key, Bytes::from_static(engine.as_str().as_bytes()))
        .await?;

    // 5. HEAD bootstrap (idempotent — first ref to push wins).
    let head = head_key(prefix);
    store
        .put_if_absent(
            &head,
            Bytes::copy_from_slice(remote_ref.as_str().as_bytes()),
        )
        .await?;

    // 6. Build new chain manifest. `next_manifest` produces the
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

    // 7. chain.json — THE commit point. After this PUT returns the
    //    push is durable. A crash here leaves orphan pack/idx/baseline
    //    keys for `manage gc`; the prior chain.json remains visible.
    write_chain(store, prefix, &remote_ref, &manifest)
        .await
        .map_err(PushError::Packchain)?;

    // 8. path-index.json — overwrite AFTER chain.json so a crash in
    //    the window between the two leaves a stale `path_index.tip`
    //    paired with a fresh `chain.tip`. Readers detect the mismatch
    //    via `path_index.tip == chain.tip` and surface
    //    `TransientChainPathIndexMismatch` (issue #114) — far less
    //    confusing than the `BlobNotInChain` the reverse ordering
    //    would produce. Skipped for blob-tipped chains; readers detect
    //    absence and fall back to "no path-index available," the
    //    correct contract for a leaf blob.
    if let Some(ref index) = path_index {
        write_path_index(store, prefix, &remote_ref, index)
            .await
            .map_err(PushError::Packchain)?;
    }

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
///
/// Lock semantics (#116): delete acquires the per-ref `LOCK#.lock` BEFORE
/// listing/deleting so it cannot race a concurrent push. The sweep
/// excludes the lock key during iteration; `release_lock` deletes it
/// last. Without this, the sweep would erase the lock held by a
/// concurrent push, letting a third client's `put_if_absent` succeed
/// and break mutual exclusion.
///
/// Probe order (#125): the `chain.json` existence probe runs INSIDE the
/// lock window, not before it. A pre-lock probe is a TOCTOU race: a
/// concurrent deleter slipping in between the probe and the lock
/// acquire would erase the chain, and we would then sweep nothing and
/// return `Ok` instead of the documented "not found" wire error.
async fn delete_remote_ref_packchain(
    store: Arc<dyn ObjectStore>,
    prefix: Option<&str>,
    remote_ref: &RefName,
    config: &PushConfig,
    now: OffsetDateTime,
) -> Result<PushOutcome, PushError> {
    let chain = chain_key(prefix, remote_ref);
    let remote_ref_str = remote_ref.as_str().to_owned();

    let lock = lock_key(prefix, remote_ref);
    let Some(guard) = acquire_lock(Arc::clone(&store), &lock, config.ttl, now).await? else {
        return Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message: format!(
                r#""failed to acquire ref lock at {lock}. Another client may be pushing or deleting. If this persists beyond {}s, run git-remote-object-store doctor to inspect and optionally clear stale locks."?"#,
                config.ttl.whole_seconds(),
            ),
        });
    };

    // Probe via head INSIDE the lock window: NotFound → "not found"
    // wire error. Release the lock cleanly before returning so we do
    // not leave a stray LOCK#.lock for an absent ref.
    match store.head(&chain).await {
        Ok(_) => {}
        Err(ObjectStoreError::NotFound(_)) => {
            let release_result = bundle_push::release_lock(guard).await;
            if let Err(e) = release_result {
                warn!(
                    key = %lock,
                    error = %e,
                    "packchain delete failed to release lock after not-found probe",
                );
            }
            return Ok(PushOutcome::Error {
                remote_ref: remote_ref_str,
                message: r#""not found"?"#.to_owned(),
            });
        }
        Err(e) => {
            // Best-effort release before surfacing the probe error.
            if let Err(rel_err) = bundle_push::release_lock(guard).await {
                warn!(
                    key = %lock,
                    error = %rel_err,
                    "packchain delete lock release failed (chain.json probe already errored)",
                );
            }
            return Err(PushError::Store(e));
        }
    }

    // Listing under the ref prefix may include the baseline bundle and
    // other per-ref artifacts; sweep them all EXCEPT the lock key.
    // Pack files live under a sibling `packs/` prefix and are
    // intentionally not touched. The lock is released LAST via
    // `release_lock` so concurrent pushes cannot slip into the critical
    // section while we are still sweeping.
    let listing = ref_listing_prefix(prefix, remote_ref);
    let store_ref = store.as_ref();
    let sweep_result: Result<(), PushError> = async {
        let entries = store_ref.list(&listing).await?;
        for entry in &entries {
            if entry.key == lock {
                continue;
            }
            delete_idempotent(store_ref, &entry.key).await?;
        }
        Ok(())
    }
    .await;

    let release_result = bundle_push::release_lock(guard).await;

    match (sweep_result, release_result) {
        (Ok(()), Ok(())) => Ok(PushOutcome::Ok {
            remote_ref: remote_ref_str,
        }),
        (Ok(()), Err(e)) => {
            warn!(key = %lock, error = %e, "packchain delete failed to release lock");
            Ok(PushOutcome::Error {
                remote_ref: remote_ref_str,
                message: format!(
                    r#""failed to release lock. You may need to manually remove the lock {lock} from the server or use git-remote-object-store doctor to fix."?"#,
                ),
            })
        }
        (Err(sweep_err), Err(rel_err)) => {
            warn!(key = %lock, error = %rel_err, "packchain delete lock release failed (sweep already errored)");
            Err(sweep_err)
        }
        (Err(sweep_err), Ok(())) => Err(sweep_err),
    }
}

#[cfg(test)]
mod tests {
    use super::super::keys::path_index_key;
    use super::*;
    use crate::object_store::mock::MockStore;

    fn rn(s: &str) -> RefName {
        RefName::new(s).unwrap()
    }

    fn delete_test_config() -> PushConfig {
        PushConfig {
            engine: StorageEngine::Packchain,
            ttl: time::Duration::seconds(60),
        }
    }

    // --- delete_remote_ref_packchain -----------------------------------

    #[tokio::test]
    async fn delete_returns_not_found_when_chain_absent() {
        let store = Arc::new(MockStore::new());
        let remote = rn("refs/heads/main");
        let config = delete_test_config();
        let outcome = delete_remote_ref_packchain(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            None,
            &remote,
            &config,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        match &outcome {
            PushOutcome::Error { message, .. } => {
                assert_eq!(
                    message, r#""not found"?"#,
                    "wire bytes for not-found delete"
                );
            }
            PushOutcome::Ok { .. } => panic!("expected Error, got {outcome:?}"),
        }
        // #125: the lock acquired around the probe must be released
        // even when the chain is absent, so an absent ref leaves no
        // stray LOCK#.lock behind.
        assert!(
            !store.contains(&lock_key(None, &remote)),
            "lock key must NOT linger after a not-found delete",
        );
    }

    /// Happy-path coverage for the #125 ordering refactor: a
    /// pre-existing `chain.json` is swept successfully when the
    /// probe runs inside the lock window. The actual TOCTOU
    /// regression (probe outside the lock racing a concurrent
    /// deleter) is structurally impossible to construct against a
    /// synchronous mock store — closing the race relies on the
    /// ordering itself, not a runtime check. This test pins the
    /// post-refactor success path so a future regression that
    /// breaks the sweep step is caught here; the "not found" wire
    /// path under the lock is covered by
    /// `delete_returns_not_found_when_chain_absent`.
    #[tokio::test]
    async fn delete_under_lock_completes_when_chain_present() {
        let store = Arc::new(MockStore::new());
        let prefix = Some("repo");
        let remote = rn("refs/heads/main");
        let chain = chain_key(prefix, &remote);
        store.insert(&chain, Bytes::from_static(b"{}"));

        let config = delete_test_config();
        let outcome = delete_remote_ref_packchain(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            prefix,
            &remote,
            &config,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, PushOutcome::Ok { .. }));
        assert!(!store.contains(&chain));
        assert!(!store.contains(&lock_key(prefix, &remote)));
    }

    #[tokio::test]
    async fn delete_sweeps_chain_path_index_and_baseline() {
        let store = Arc::new(MockStore::new());
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

        let config = delete_test_config();
        let outcome = delete_remote_ref_packchain(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            prefix,
            &remote,
            &config,
            OffsetDateTime::now_utc(),
        )
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
        // Lock must also be gone (release_lock deletes it after sweep).
        assert!(
            !store.contains(&lock_key(prefix, &remote)),
            "lock key must be released after a successful delete",
        );
    }

    /// Regression for #116: delete must take the per-ref lock first.
    /// If another writer already holds the lock, delete returns a
    /// contention error and leaves all per-ref keys (including the
    /// foreign-held lock) intact.
    ///
    /// Wire-format pin (#126): the contention message is asserted
    /// byte-for-byte, not by substring. The helper protocol relies on
    /// the `"…"?` envelope; a regression that strips the quotes or
    /// the trailing `?` would silently corrupt the wire encoding, and
    /// a `contains("failed to acquire ref lock")` assertion would not
    /// notice.
    #[tokio::test]
    async fn delete_with_lock_held_reports_contention_and_preserves_keys() {
        let store = Arc::new(MockStore::new());
        let prefix = Some("repo");
        let remote = rn("refs/heads/main");
        let chain = chain_key(prefix, &remote);
        let path_index = path_index_key(prefix, &remote);
        let lock = lock_key(prefix, &remote);

        // Seed chain + path-index, and pre-take the lock as if another
        // writer were mid-push.
        store.insert(&chain, Bytes::from_static(b"{}"));
        store.insert(&path_index, Bytes::from_static(b"{}"));
        store.insert(&lock, Bytes::new());

        let config = delete_test_config();
        // `now` close to the lock's insertion time → not stale, lock
        // acquire returns false.
        let outcome = delete_remote_ref_packchain(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            prefix,
            &remote,
            &config,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        // Pin the exact wire bytes — the `"…"?` envelope and the full
        // message body. The ttl interpolation matches
        // `delete_test_config()`'s 60-second TTL.
        let expected = format!(
            r#""failed to acquire ref lock at {lock}. Another client may be pushing or deleting. If this persists beyond 60s, run git-remote-object-store doctor to inspect and optionally clear stale locks."?"#,
        );
        match &outcome {
            PushOutcome::Error {
                message,
                remote_ref,
            } => {
                assert_eq!(message, &expected, "contention wire message must be exact",);
                assert_eq!(remote_ref, remote.as_str());
            }
            PushOutcome::Ok { .. } => panic!("expected contention Error, got {outcome:?}"),
        }
        // Critical: nothing was deleted. The foreign-held lock is
        // intact, so the other writer still has mutual exclusion.
        assert!(store.contains(&chain), "chain.json must NOT be deleted");
        assert!(
            store.contains(&path_index),
            "path-index.json must NOT be deleted",
        );
        assert!(
            store.contains(&lock),
            "foreign-held LOCK#.lock must NOT be deleted by a contending delete (#116)",
        );
    }

    /// Regression for #116/#126: the sweep must skip the lock key,
    /// so a concurrent `put_if_absent(LOCK#.lock)` between sweep and
    /// release is impossible — the lock we hold is the only
    /// `LOCK#.lock` that ever exists during the delete's critical
    /// section.
    ///
    /// We arm a one-shot `NetworkOnDelete` fault on the lock key so
    /// the lock's deletion is observable. Because the fault only
    /// fires once, the test discriminates:
    ///
    /// - Skip works: sweep deletes every other per-ref key cleanly,
    ///   then `release_lock` trips the fault and the call surfaces a
    ///   "failed to release lock" `PushOutcome::Error`. The lock
    ///   object is still present at the end (the fault blocked
    ///   release's delete) and the fault was consumed exactly once.
    /// - Skip broken (`continue` removed): the sweep deletes the
    ///   lock first, consuming the fault, then continues deleting
    ///   the rest. `release_lock` then sees `NotFound` (treated as
    ///   `Ok`) and the outcome is `PushOutcome::Ok`. The witness:
    ///   the lock is absent at the end. The assertion on the lock
    ///   being present after the call catches that regression.
    ///
    /// The lock is seeded fresh (not stale) so the `acquire_lock`
    /// path is `put_if_absent` and never touches `delete` on the
    /// lock key itself — that keeps the armed fault available for
    /// the sweep/release-stage witness. Stale-recovery coverage
    /// lives in [`delete_recovers_stale_lock_and_completes`].
    #[tokio::test]
    async fn delete_sweep_excludes_lock_key() {
        use crate::object_store::mock::Fault;

        let store = Arc::new(MockStore::new());
        let prefix = Some("repo");
        let remote = rn("refs/heads/main");
        let chain = chain_key(prefix, &remote);
        let path_index = path_index_key(prefix, &remote);
        let baseline_sha = Sha::from_hex("0000000000000000000000000000000000000001").unwrap();
        let baseline = keys::bundle_key(prefix, &remote, baseline_sha);
        let lock = lock_key(prefix, &remote);

        // Seed chain + path-index + a baseline bundle so the sweep
        // has real per-iteration work; a regression that broke
        // iteration would leave these behind and fail the assertions
        // below.
        store.insert(&chain, Bytes::from_static(b"{}"));
        store.insert(&path_index, Bytes::from_static(b"{}"));
        store.insert(&baseline, Bytes::from_static(b"PACK"));

        // Arm a one-shot fault on lock delete. If the sweep ever
        // touches the lock key, it consumes the fault as a sweep
        // error; if the sweep correctly skips, the fault fires from
        // `release_lock` and we observe a release-failure outcome.
        store.arm(Fault::NetworkOnDelete { key: lock.clone() });

        let config = delete_test_config();
        let outcome = delete_remote_ref_packchain(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            prefix,
            &remote,
            &config,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

        // Sweep ran to completion: every non-lock per-ref key is
        // gone. The release-failure path keeps the lock in place
        // (the fault prevented its deletion), giving us a direct
        // witness that the sweep did NOT delete it either.
        assert!(!store.contains(&chain), "chain.json must be swept");
        assert!(
            !store.contains(&path_index),
            "path-index.json must be swept",
        );
        assert!(!store.contains(&baseline), "baseline bundle must be swept");
        assert!(
            store.contains(&lock),
            "lock must survive the sweep — only release_lock may delete it, \
             and the armed fault blocked that delete",
        );

        // The fault was consumed exactly once, by `release_lock`.
        assert_eq!(
            store.pending_faults(),
            0,
            "armed delete-fault must have fired exactly once (via release)",
        );

        // The outcome surfaces the release failure (sweep succeeded,
        // release tripped the armed fault). The exact wire-format
        // envelope is pinned here too.
        let expected = format!(
            r#""failed to release lock. You may need to manually remove the lock {lock} from the server or use git-remote-object-store doctor to fix."?"#,
        );
        match &outcome {
            PushOutcome::Error {
                message,
                remote_ref,
            } => {
                assert_eq!(message, &expected, "release-failure wire bytes");
                assert_eq!(remote_ref, remote.as_str());
            }
            PushOutcome::Ok { .. } => panic!(
                "expected release-failure Error (sweep correctly skipped the lock, \
                 release tripped the armed fault), got {outcome:?}",
            ),
        }
    }

    /// Regression for #126: end-to-end stale-lock recovery in delete.
    /// A pre-existing lock whose `last_modified` is older than `ttl`
    /// is reclaimable; the delete then proceeds, the sweep completes,
    /// and the lock is released at the end. The previous test suite
    /// covered stale recovery only inside the bundle-push acquire
    /// unit tests — not through the packchain delete path.
    #[tokio::test]
    async fn delete_recovers_stale_lock_and_completes() {
        let store = Arc::new(MockStore::new());
        let prefix = Some("repo");
        let remote = rn("refs/heads/main");
        let chain = chain_key(prefix, &remote);
        let path_index = path_index_key(prefix, &remote);
        let lock = lock_key(prefix, &remote);

        store.insert(&chain, Bytes::from_static(b"{}"));
        store.insert(&path_index, Bytes::from_static(b"{}"));

        // Lock pre-existed and is stale (older than the 60-second
        // TTL). `acquire_lock` should reclaim it on the
        // stale-recovery branch.
        let now = OffsetDateTime::now_utc();
        let stale = now - time::Duration::seconds(120);
        store.insert_with(&lock, Bytes::new(), stale, PutOpts::default());

        let config = delete_test_config();
        let outcome = delete_remote_ref_packchain(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            prefix,
            &remote,
            &config,
            now,
        )
        .await
        .unwrap();
        assert!(
            matches!(&outcome, PushOutcome::Ok { remote_ref } if remote_ref == remote.as_str()),
            "stale lock must be recoverable end-to-end, got {outcome:?}",
        );
        assert!(!store.contains(&chain), "chain.json must be swept");
        assert!(
            !store.contains(&path_index),
            "path-index.json must be swept",
        );
        assert!(
            !store.contains(&lock),
            "lock must be released after a successful stale-recovery delete",
        );
    }
}

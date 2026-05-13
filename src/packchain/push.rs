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
    self as bundle_push, DELETE_PROTECTION_MESSAGE, LockGuard, PushError, PushOutcome, PushSpec,
    acquire_lock, bundle_progress_sink, delete_idempotent, head_key, is_protected, lock_key,
    lock_ttl_from_env, not_ancestor_wire_message, parse_push_args, ref_listing_prefix,
};
use crate::url::StorageEngine;

use super::PackchainError;
use super::gc::write_baseline_tombstone_best_effort;
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
    /// Force-flag from the parsed [`PushSpec`]. With issue #129's
    /// fix this is the user's original intent — pre-lock no longer
    /// demotes it against a `PROTECTED#` marker; the protection
    /// check now runs under the lock.
    force: bool,
    /// Whether the pre-lock chain.tip (when one existed) was an
    /// ancestor of the local tip. Always `true` when there was no
    /// prior chain. Used by [`perform_push_under_lock`] to render the
    /// same `NotAncestor` wire error a pre-lock non-force probe would
    /// have produced if a `PROTECTED#` marker lands between the
    /// pre-lock work and the lock acquisition (issue #129).
    prior_was_ancestor: bool,
    /// Original local refspec. Kept so the under-lock `NotAncestor`
    /// arm renders the same wire message a pre-lock non-force probe
    /// would have produced (issue #129).
    local_spec: String,
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
    /// Whether the prior chain.tip (when one existed) was an ancestor
    /// of the local tip. `true` when there was no prior chain. Always
    /// computed (even on force-push) so [`perform_push_under_lock`] can
    /// distinguish FF from non-FF if it discovers the ref became
    /// `PROTECTED#` under the lock (issue #129).
    prior_was_ancestor: bool,
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
    // Issue #129: the `PROTECTED#` check used to run here, before the
    // per-ref lock was acquired. A concurrent `protect` between the
    // check and the lock would let a force-push overwrite a now-
    // protected ref. The check now runs under the lock in
    // [`perform_push_under_lock`]. Pre-lock we just respect the user's
    // intent; ancestry is still computed in `local_git_work_packchain`
    // and stashed on `ReadyState` so the under-lock arm can emit the
    // same NotAncestor wire error a non-force push would have produced.
    let force_push = force;
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
        prior_was_ancestor: local.prior_was_ancestor,
        local_spec,
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
        GitProbeError::NotAncestor => not_ancestor_wire_message(local_spec),
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
    //
    // Issue #129: ancestry is always computed (even on force-push) so
    // the caller can stash `prior_was_ancestor` on `ReadyState` and
    // emit a NotAncestor wire error from under the lock if a concurrent
    // `protect` lands while we hold the pre-lock work. The
    // pack-build kind (`Baseline` vs `Incremental`) still keys off
    // `force_push`: only a non-force push with a peeled commit ancestry
    // produces a usable `prior_commit` for the incremental walker.
    let (prior_commit, prior_was_ancestor) = match prior_tip {
        None => (None, true),
        Some(prior) => match (local_commit, git::peel_tag_chain(&repo, prior)) {
            (Some(local_commit_oid), Ok(PeeledTip::Commit { commit, .. })) => {
                let ancestor =
                    git::is_ancestor(&repo, commit, local_commit_oid).map_err(PushError::Git)?;
                if !force_push && !ancestor {
                    return Ok(Err(GitProbeError::NotAncestor));
                }
                // Incremental pack base only when the push is non-force,
                // commit-tipped, and the prior is reachable from local.
                let prior_commit = (!force_push && ancestor).then_some(commit);
                (prior_commit, ancestor)
            }
            // Either side non-commit ⇒ kind mismatch ⇒ NotAncestor for
            // non-force; force-push gets recorded as not-an-ancestor and
            // proceeds with a Baseline pack. FindObject on the prior tip
            // means it's not in the local ODB (synthesised remote-only
            // OID, unrelated history) — also surface as not-an-ancestor.
            (None, _)
            | (
                _,
                Ok(PeeledTip::Tree { .. } | PeeledTip::Blob { .. })
                | Err(crate::git::GitError::FindObject(_)),
            ) => {
                if !force_push {
                    return Ok(Err(GitProbeError::NotAncestor));
                }
                (None, false)
            }
            (_, Err(e)) => return Err(PushError::Git(e)),
        },
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
        prior_was_ancestor,
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
        prior_was_ancestor,
        local_spec,
        _temp_dir,
    } = state;
    let remote_ref_str = remote_ref.as_str().to_owned();

    // Issue #129: under-lock force-push protection check. The pre-lock
    // arm dropped its `is_protected` call so a concurrent `protect`
    // could no longer race the lock. The historical "protected ref +
    // force" semantic is "demote to non-force": fast-forward pushes go
    // through, non-fast-forward pushes get the standard NotAncestor
    // wire error. `is_protected` uses `head`, not `list`, so this adds
    // one cheap probe and does not duplicate the chain.json re-read
    // below.
    if force && !prior_was_ancestor && is_protected(store, prefix, &remote_ref).await? {
        return Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message: not_ancestor_wire_message(&local_spec),
        });
    }

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

/// Best-effort tombstone of the prior baseline bundle after a force
/// push has already committed (i.e. `chain.json` has been
/// overwritten). The bundle is NOT deleted here: writing the
/// tombstone defers reclamation to [`super::gc::sweep`], which waits
/// for the operator-configured grace window so a concurrent fetch
/// that already read the prior chain.json can still download the
/// bundle it expects (issue #134).
///
/// Failure here cannot fail the push: we log at `warn` so an operator
/// notices the un-tombstoned orphan and `manage gc` sweeps it later
/// via a re-run of this code path or manual cleanup.
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
    write_baseline_tombstone_best_effort(
        store,
        prefix,
        remote_ref,
        &prior.full_at,
        local_sha40,
        "force-push",
    )
    .await;
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
///
/// Protection guard (#130): after listing under the lock, the function
/// checks for a `PROTECTED#` marker segment in any entry's last path
/// segment and refuses the delete with the canonical wire-format
/// protection message. Mirrors the bundle engine's first-guard check in
/// [`crate::protocol::push::delete_remote_ref_under_lock`]. Because
/// `protect` is a lockless `put_if_absent` on the marker key, the
/// concurrent-`protect` TOCTOU window between `acquire_lock` and the
/// listing is closed by running the check against the under-lock
/// listing rather than a pre-lock snapshot.
/// Release `guard` and downgrade any release failure to a structured
/// `warn!`. The `after` argument names the early-exit context the
/// release follows (e.g. "not-found probe", "list failure",
/// "protection rejection"); it appears verbatim at the tail of the log
/// message so each call site keeps its distinct trail in the logs.
///
/// Used by [`delete_remote_ref_packchain`] to consolidate the
/// previously copy-pasted "release-on-error" tails into a single
/// helper. Release failures here are by definition best-effort: the
/// caller is already on an error / wire-error path and will surface
/// the primary outcome regardless of the release result.
async fn release_lock_or_warn(guard: LockGuard, lock: &str, after: &str) {
    if let Err(e) = bundle_push::release_lock(guard).await {
        warn!(
            key = %lock,
            error = %e,
            after,
            "packchain delete failed to release lock",
        );
    }
}

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
            release_lock_or_warn(guard, &lock, "not-found probe").await;
            return Ok(PushOutcome::Error {
                remote_ref: remote_ref_str,
                message: r#""not found"?"#.to_owned(),
            });
        }
        Err(e) => {
            // Best-effort release before surfacing the probe error.
            release_lock_or_warn(guard, &lock, "chain.json probe error").await;
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

    // List under the lock. A list failure releases the lock cleanly
    // before surfacing the error — same shape as the chain.json probe's
    // error branch above.
    let entries = match store_ref.list(&listing).await {
        Ok(es) => es,
        Err(e) => {
            release_lock_or_warn(guard, &lock, "list failure").await;
            return Err(PushError::Store(e));
        }
    };

    // Issue #130: PROTECTED# marker check against the fresh, under-lock
    // listing, BEFORE the sweep. Mirrors the bundle engine's first-guard
    // protection check in `protocol::push::delete_remote_ref_under_lock`.
    if keys::entries_have_protected_marker(&entries) {
        release_lock_or_warn(guard, &lock, "protection rejection").await;
        return Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message: DELETE_PROTECTION_MESSAGE.to_owned(),
        });
    }

    let sweep_result: Result<(), PushError> = async {
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
                assert_eq!(message, &expected, "contention wire message must be exact");
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

    // --- Issue #130: delete must honor the PROTECTED# marker ---

    /// Regression for #130: a `PROTECTED#` marker present alongside the
    /// chain manifest must cause the delete to refuse with the canonical
    /// protection wire message. Both seeded keys (chain.json and the
    /// marker) must survive — a regression that swept either before
    /// noticing the marker would let `git push :<branch>` quietly bypass
    /// the protection.
    ///
    /// Wire-format pin: the message is asserted byte-for-byte, including
    /// the `"…"?` envelope. A regression that dropped the envelope or
    /// drifted the body off the bundle engine's wording would not be
    /// caught by a substring check.
    #[tokio::test]
    async fn delete_rejects_when_protected_marker_present_with_chain() {
        let store = Arc::new(MockStore::new());
        let prefix = Some("repo");
        let remote = rn("refs/heads/main");
        let chain = chain_key(prefix, &remote);
        let protected = "repo/refs/heads/main/PROTECTED#";

        store.insert(&chain, Bytes::from_static(b"{}"));
        store.insert(protected, Bytes::new());

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

        match &outcome {
            PushOutcome::Error {
                message,
                remote_ref,
            } => {
                assert_eq!(
                    message, DELETE_PROTECTION_MESSAGE,
                    "wire bytes for protection rejection must match the bundle engine",
                );
                assert_eq!(remote_ref, remote.as_str());
            }
            PushOutcome::Ok { .. } => {
                panic!("expected protection Error, got {outcome:?}")
            }
        }

        // Critical: NOTHING under the ref prefix was deleted. The
        // protection check must run BEFORE the sweep, so chain.json and
        // the marker both survive.
        assert!(
            store.contains(&chain),
            "chain.json must NOT be swept when PROTECTED# marker is present",
        );
        assert!(
            store.contains(protected),
            "PROTECTED# marker must NOT be swept by a refused delete",
        );
        // The lock is still released cleanly on the protection branch.
        assert!(
            !store.contains(&lock_key(prefix, &remote)),
            "lock must be released after a protection-rejected delete",
        );
    }

    /// Regression for #130: closes the TOCTOU window where a concurrent
    /// `protect` lands the marker between the under-lock `head` probe
    /// of `chain.json` and the under-lock `list` of the ref prefix.
    ///
    /// Uses [`PostHeadHookStore`] to deterministically inject the
    /// PROTECTED# marker between the two calls: the listing is seeded
    /// WITHOUT the marker, the hook fires after `head(chain.json)`
    /// returns successfully, and inserts the marker before the
    /// downstream `list` runs. The under-lock `list` therefore sees
    /// the freshly-arrived marker and the delete is rejected with the
    /// canonical protection message. Mirrors the issue-#144 fix.
    ///
    /// What this pins separately from
    /// [`delete_rejects_when_protected_marker_present_with_chain`]: that
    /// the marker check operates on the under-lock LIST output (not on
    /// the head probe's snapshot), so a marker that arrives strictly
    /// AFTER the head probe is still caught. A regression that
    /// pre-computed `has_protected_marker` from a snapshot read before
    /// the list would let this scenario slip through — without the
    /// hook the listing would never observe the marker and the sweep
    /// would silently complete.
    #[tokio::test]
    async fn delete_rejects_when_protected_marker_lands_after_lock_acquire() {
        let inner = MockStore::new();
        let prefix = Some("repo");
        let remote = rn("refs/heads/main");
        let chain = chain_key(prefix, &remote);
        let protected = "repo/refs/heads/main/PROTECTED#";

        // Chain present so the head probe succeeds; marker is
        // explicitly ABSENT in the initial state — it lands only when
        // the hook fires, after `head(chain.json)` returns.
        inner.insert(&chain, Bytes::from_static(b"{}"));

        let protected_key = protected.to_owned();
        let store = Arc::new(PostHeadHookStore::new(inner, &chain, move |inner| {
            // Simulate the concurrent `protect` landing in the gap
            // between the under-lock `head(chain.json)` and the
            // under-lock `list(ref_prefix)`.
            inner.insert(&protected_key, Bytes::new());
        }));

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

        assert!(
            matches!(
                &outcome,
                PushOutcome::Error { message, .. }
                    if message == DELETE_PROTECTION_MESSAGE
            ),
            "TOCTOU-window protect must reject with the canonical message, got {outcome:?}",
        );

        // Witness: the hook fired exactly once on the chain.json head.
        // If a regression dropped the under-lock head probe (or scoped
        // it to a different key), the hook would never fire and the
        // marker would never land — surfacing here as a non-rejection
        // outcome above OR as a remaining-hook witness below.
        assert!(
            store.hook_fired(),
            "post-head hook must have fired on the chain.json head probe",
        );

        // No keys swept — exhaustive check across every key under the
        // ref prefix (other than the now-released lock). A regression
        // that swept any subset before bailing would leave one of these
        // assertions failing.
        let ref_prefix = "repo/refs/heads/main/";
        let surviving: Vec<String> = store
            .inner
            .keys()
            .into_iter()
            .filter(|k| k.starts_with(ref_prefix))
            .collect();
        assert!(
            surviving.iter().any(|k| k == &chain),
            "chain.json must survive a TOCTOU-window rejection, surviving = {surviving:?}",
        );
        assert!(
            surviving.iter().any(|k| k == protected),
            "PROTECTED# marker must survive a TOCTOU-window rejection, surviving = {surviving:?}",
        );
    }

    /// Regression check: the non-protected path is unaffected by the
    /// #130 guard. An unprotected ref with a chain.json (and other
    /// per-ref artefacts) must still sweep cleanly. A regression that
    /// mis-fired the marker check — e.g., matching any key containing
    /// the substring "PROTECTED" instead of the exact last segment —
    /// would refuse this delete and fail here.
    ///
    /// This complements [`delete_under_lock_completes_when_chain_present`]
    /// (which covers the chain-only happy path) by seeding a richer
    /// listing (chain + path-index + baseline bundle) and asserting all
    /// of them are swept.
    #[tokio::test]
    async fn delete_unprotected_with_chain_sweeps_as_before() {
        let store = Arc::new(MockStore::new());
        let prefix = Some("repo");
        let remote = rn("refs/heads/main");
        let chain = chain_key(prefix, &remote);
        let path_index = path_index_key(prefix, &remote);
        let baseline_sha = Sha::from_hex("0000000000000000000000000000000000000001").unwrap();
        let baseline = keys::bundle_key(prefix, &remote, baseline_sha);

        store.insert(&chain, Bytes::from_static(b"{}"));
        store.insert(&path_index, Bytes::from_static(b"{}"));
        store.insert(&baseline, Bytes::from_static(b"PACK"));
        // Explicitly NO PROTECTED# marker — the guard must not fire.

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

        assert!(
            matches!(&outcome, PushOutcome::Ok { remote_ref } if remote_ref == remote.as_str()),
            "unprotected delete must succeed, got {outcome:?}",
        );
        assert!(!store.contains(&chain), "chain.json must be swept");
        assert!(
            !store.contains(&path_index),
            "path-index.json must be swept",
        );
        assert!(!store.contains(&baseline), "baseline bundle must be swept");
        assert!(
            !store.contains(&lock_key(prefix, &remote)),
            "lock must be released after a successful unprotected delete",
        );
    }

    /// Regression for #130: when the under-lock `list` call itself
    /// errors (transport failure, `AccessDenied`, …), the function must
    /// surface the store error AND release the lock cleanly before
    /// returning. A pre-fix code path that returned `?` directly from
    /// the list call would leak the LOCK#.lock key, blocking every
    /// subsequent push/delete on the ref until TTL recovery.
    #[tokio::test]
    async fn delete_remote_ref_packchain_releases_lock_on_list_failure() {
        use crate::object_store::mock::Fault;

        let store = Arc::new(MockStore::new());
        let prefix = Some("repo");
        let remote = rn("refs/heads/main");
        let chain = chain_key(prefix, &remote);
        let listing = ref_listing_prefix(prefix, &remote);
        let lock = lock_key(prefix, &remote);

        // Seed chain.json so the under-lock `head` probe succeeds and
        // execution reaches the `list` call.
        store.insert(&chain, Bytes::from_static(b"{}"));

        // Arm a one-shot list fault scoped to the ref-listing prefix.
        // The fault fires once and is then consumed, so any subsequent
        // delete in `release_lock` proceeds normally.
        store.arm(Fault::AccessDeniedOnList { prefix: listing });

        let config = delete_test_config();
        let result = delete_remote_ref_packchain(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            prefix,
            &remote,
            &config,
            OffsetDateTime::now_utc(),
        )
        .await;

        // The list error must propagate as a store error — the
        // function must NOT silently return Ok or convert the failure
        // into a per-ref Error outcome.
        match result {
            Err(PushError::Store(_)) => {}
            Err(other) => panic!("expected PushError::Store, got {other:?}"),
            Ok(outcome) => panic!("expected list-failure error, got Ok({outcome:?})"),
        }

        // Witness: the listing fault was consumed exactly once.
        assert_eq!(
            store.pending_faults(),
            0,
            "armed list-fault must have fired exactly once on the under-lock listing",
        );

        // Critical: no orphan lock survives the call. Even though the
        // listing failed, `release_lock` must have run before the
        // function returned.
        assert!(
            !store.contains(&lock),
            "lock key must be released even when the under-lock list call fails",
        );
        // chain.json is untouched on the error path (sweep never ran).
        assert!(
            store.contains(&chain),
            "chain.json must NOT be swept when the listing call failed before the sweep",
        );
    }

    // --- Issue #129: force-push protection check runs under the lock ---

    /// Build a [`ReadyState`] tailored for exercising the early
    /// protection-rejection branch of [`perform_push_under_lock`].
    /// The pre-existing pack/idx have already been uploaded by the
    /// production code path; the test does not seed them because the
    /// new protection check fires *before* the chain.json re-read or
    /// the path-index walk, so the function returns without touching
    /// any of those keys.
    fn ready_state_for_protection_test(force: bool, prior_was_ancestor: bool) -> Box<ReadyState> {
        let local_sha = Sha::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
        let local_sha40 = Sha40::from_oid(local_sha.as_object_id()).unwrap();
        let pack_content_sha = Sha40::try_new("1111111111111111111111111111111111111111").unwrap();
        let temp_dir = tempfile::Builder::new()
            .prefix("test_packchain_push_")
            .tempdir()
            .unwrap();
        Box::new(ReadyState {
            remote_ref: rn("refs/heads/main"),
            local_sha,
            local_sha40,
            cwd: temp_dir.path().to_owned(),
            prior: None,
            pack_content_sha,
            pack_bytes: 0,
            force,
            prior_was_ancestor,
            local_spec: "refs/heads/main".to_owned(),
            _temp_dir: temp_dir,
        })
    }

    /// Regression for issue #129 (packchain). A concurrent `protect`
    /// that lands a `PROTECTED#` marker between the pre-lock work and
    /// the lock acquisition must be observed by the under-lock arm of
    /// `perform_push_under_lock`. With `force=true` and a non-FF push,
    /// the engine must reject with the canonical `NotAncestor` wire
    /// token rather than overwriting the now-protected ref.
    #[tokio::test]
    async fn perform_push_under_lock_rejects_force_when_protected_under_lock_and_not_ff() {
        let store = MockStore::new();
        // Concurrent `protect` lands the marker before we get the lock.
        store.insert("repo/refs/heads/main/PROTECTED#", Bytes::from_static(b""));
        let state = ready_state_for_protection_test(true, false);
        let outcome =
            perform_push_under_lock(&store, Some("repo"), StorageEngine::Packchain, *state)
                .await
                .unwrap();
        assert!(
            matches!(
                &outcome,
                PushOutcome::Error { message, .. }
                    if message == r#""remote ref is not ancestor of refs/heads/main."?"#
            ),
            "expected under-lock NotAncestor refusal, got {outcome:?}",
        );
        // Protection marker survives a refusal.
        assert!(store.contains("repo/refs/heads/main/PROTECTED#"));
        // A refusal path that progressed past the protection check
        // could have uploaded packchain artefacts before bailing.
        // Assert none of those keys exist for this ref — only the
        // `PROTECTED#` marker should be present under the ref prefix.
        // Note: these are bucket keys (case-sensitive by S3/Azure
        // contract), not filesystem paths, so the case-sensitive
        // suffix check is correct.
        #[allow(clippy::case_sensitive_file_extension_comparisons)]
        let is_artefact = |k: &str| {
            k.ends_with(".pack")
                || k.ends_with(".idx")
                || k.ends_with("/chain.json")
                || k.ends_with("/path-index.json")
        };
        let ref_prefix = "repo/refs/heads/main/";
        let stray: Vec<String> = store
            .keys()
            .into_iter()
            .filter(|k| k.starts_with(ref_prefix) && is_artefact(k))
            .collect();
        assert!(
            stray.is_empty(),
            "refused push must not upload packchain artefacts, found: {stray:?}",
        );
    }

    /// Companion to the rejection case: a legitimate force-push (force,
    /// non-FF, no `PROTECTED#` marker) must NOT hit the protection
    /// refusal. This pins the polarity of the AND-clause guarding the
    /// rejection — a regression that dropped the `is_protected` term
    /// would refuse every non-FF force-push, not just those against
    /// protected refs.
    ///
    /// The packchain engine fails downstream against the empty tempdir
    /// (no `.git` → path-index walk errors), so we cannot assert a
    /// successful end-to-end outcome here. What we pin is the absence
    /// of the `NotAncestor` wire token on every outcome arm: that token
    /// would appear if and only if the protection guard mis-fired.
    #[tokio::test]
    async fn perform_push_under_lock_allows_force_when_not_ancestor_and_not_protected() {
        let store = MockStore::new();
        // No PROTECTED# marker seeded.
        let state = ready_state_for_protection_test(true, false);
        let outcome =
            perform_push_under_lock(&store, Some("repo"), StorageEngine::Packchain, *state).await;
        match outcome {
            Ok(PushOutcome::Ok { .. }) => {
                // Passed every stage including the bypassed guard.
            }
            Ok(PushOutcome::Error { message, .. }) => assert!(
                !message.contains("not ancestor"),
                "unprotected force-push must not emit NotAncestor: {message:?}",
            ),
            Err(e) => assert!(
                !e.to_string().contains("not ancestor"),
                "unprotected force-push must not emit NotAncestor: {e}",
            ),
        }
    }

    /// Non-force pushes must not consult `is_protected` under the
    /// lock: by the time control reaches `perform_push_under_lock` a
    /// non-force non-FF push has already been rejected by the
    /// pre-lock ancestry probe, and a non-force FF push is unaffected
    /// by protection. The under-lock check is gated on `force` to
    /// keep the happy path free of an extra HEAD round-trip.
    ///
    /// We trigger an early non-`Ok` outcome from a downstream stage
    /// (the chain.json reload) to verify the function progressed past
    /// the protection check rather than short-circuiting on it.
    #[tokio::test]
    async fn perform_push_under_lock_skips_protection_check_for_non_force() {
        let store = MockStore::new();
        // Marker present, but a non-force push must not even probe for it.
        store.insert("repo/refs/heads/main/PROTECTED#", Bytes::from_static(b""));
        // prior_was_ancestor=false: keep the `!prior_was_ancestor` clause
        // TRUE so the only clause holding the guard
        // `force && !prior_was_ancestor && is_protected(...)` off is `force`
        // itself. Using `prior_was_ancestor=true` would short-circuit on the
        // second clause and silently mask a regression that dropped the
        // leading `force &&` from production.
        let state = ready_state_for_protection_test(false, false);
        // We expect the call to progress past the protection check
        // and fail downstream (chain.json absent → path-index walk
        // against a tempdir without a git repo). What we're pinning is
        // that the failure is NOT the NotAncestor protection refusal —
        // a regression that ran the check for non-force pushes would
        // surface as that exact wire token.
        let outcome =
            perform_push_under_lock(&store, Some("repo"), StorageEngine::Packchain, *state).await;
        // Pin that the protection check is bypassed for non-force pushes,
        // regardless of whether the rest of the push happens to succeed
        // in this test setup. A regression that ran the check for
        // non-force pushes would surface as the `not ancestor` wire token
        // on either the Ok-with-Error or the Err arm.
        match outcome {
            Ok(PushOutcome::Ok { .. }) => {
                // Passed every stage including the bypassed check.
            }
            Ok(PushOutcome::Error { message, .. }) => assert!(
                !message.contains("not ancestor"),
                "non-force push must not hit the protection rejection: {message:?}",
            ),
            Err(e) => assert!(
                !e.to_string().contains("not ancestor"),
                "non-force push must not hit the protection rejection: {e}",
            ),
        }
    }

    // --- mark head→list race injection (issue #144) -------------------

    /// One-shot post-`head` hook used by [`PostHeadHookStore`].
    type PostHeadHook = Box<dyn FnOnce(&MockStore) + Send>;

    /// Test-only [`ObjectStore`] decorator that runs a one-shot
    /// callback the first time `head()` succeeds on `trigger_key`,
    /// *after* the inner head completes but before the call returns
    /// to the consumer. Used to deterministically simulate a
    /// concurrent operation landing in the gap between two
    /// successive calls — e.g. a `protect` that writes the
    /// `PROTECTED#` marker after the under-lock `head(chain.json)`
    /// probe but before the under-lock `list(ref_prefix)`.
    ///
    /// Mirrors the shape of `PostDeleteHookStore` and
    /// `PostListHookStore` in [`super::super::gc`]: every other
    /// trait method forwards to the inner store unchanged, the hook
    /// is consumed exactly once, and the inner [`MockStore`] is
    /// accessible via the `inner` field for test seeding and
    /// post-call assertions.
    ///
    /// The `trigger_key` filter is exact-match (not prefix) because
    /// the head probe in `delete_remote_ref_packchain` shares the
    /// surface with `acquire_lock`, which itself may `head()` the
    /// lock key on contention — scoping by exact key keeps the hook
    /// from misfiring on lock-related probes.
    struct PostHeadHookStore {
        inner: MockStore,
        hook: std::sync::Mutex<Option<PostHeadHook>>,
        trigger_key: String,
    }

    impl PostHeadHookStore {
        fn new(
            inner: MockStore,
            trigger_key: impl Into<String>,
            hook: impl FnOnce(&MockStore) + Send + 'static,
        ) -> Self {
            Self {
                inner,
                hook: std::sync::Mutex::new(Some(Box::new(hook))),
                trigger_key: trigger_key.into(),
            }
        }

        /// `true` once the hook has been consumed — used by tests to
        /// witness that the production code reached the targeted
        /// head call rather than skipping past it on a different
        /// branch.
        fn hook_fired(&self) -> bool {
            self.hook.lock().unwrap().is_none()
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for PostHeadHookStore {
        async fn list(
            &self,
            prefix: &str,
        ) -> Result<Vec<crate::object_store::ObjectMeta>, ObjectStoreError> {
            self.inner.list(prefix).await
        }

        async fn get_to_file(
            &self,
            key: &str,
            dest: &std::path::Path,
            opts: crate::object_store::GetOpts,
        ) -> Result<(), ObjectStoreError> {
            self.inner.get_to_file(key, dest, opts).await
        }

        async fn get_bytes(&self, key: &str) -> Result<Bytes, ObjectStoreError> {
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
            opts: PutOpts,
        ) -> Result<(), ObjectStoreError> {
            self.inner.put_bytes(key, body, opts).await
        }

        async fn put_path(
            &self,
            key: &str,
            src: &std::path::Path,
            opts: PutOpts,
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
            let result = self.inner.head(key).await;
            if result.is_ok()
                && key == self.trigger_key
                && let Some(hook) = self.hook.lock().unwrap().take()
            {
                hook(&self.inner);
            }
            result
        }

        async fn copy(&self, src: &str, dst: &str) -> Result<(), ObjectStoreError> {
            self.inner.copy(src, dst).await
        }

        async fn delete(&self, key: &str) -> Result<(), ObjectStoreError> {
            self.inner.delete(key).await
        }
    }
}

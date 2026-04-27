//! `push` handler with per-ref locking via conditional writes.
//!
//! Mirrors `cmd_push` in `../git-remote-s3/git_remote_s3/remote.py:198-305`
//! and shares its sequential-batch semantics: every `push <refspec>`
//! line in a batch is processed in order under its own per-ref lock,
//! and one outcome line is emitted per push (`ok <ref>\n` or
//! `error <ref> "msg"\n`). `gix::Repository` is `!Sync` so the handler
//! holds the repo handle on a single task — pushes never run in
//! parallel within one client.
//!
//! Stdout discipline: this module returns [`PushOutcome`] values and
//! never writes to the protocol stream itself. The REPL renders each
//! outcome and the trailing blank-line terminator (see
//! `.claude/rules/protocol-stdout.md`).

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use time::{Duration, OffsetDateTime};
use tracing::{debug, warn};

use crate::git::{self, GitError, RefName, RefNameError, Sha, ShaError, is_valid_ref_name};
use crate::object_store::{Error as ObjectStoreError, ObjectMeta, ObjectStore, PutOpts};

/// Default per-ref lock TTL, in seconds. Matches upstream
/// (`DEFAULT_LOCK_TTL_SECONDS = 60`,
/// `../git-remote-s3/git_remote_s3/remote.py:45`).
pub(crate) const DEFAULT_LOCK_TTL_SECONDS: u64 = 60;

/// Environment override for the lock TTL, in seconds. Name is preserved
/// from upstream for cross-implementation parity (see `execution-plan.md`
/// §1.1).
pub(crate) const ENV_LOCK_TTL_SECONDS: &str = "GIT_REMOTE_S3_LOCK_TTL_SECONDS";

/// Errors surfaced by the push path. These abort the helper — per-ref
/// failures (multi-bundle, ancestor mismatch, lock contention, ...) are
/// returned as [`PushOutcome::Error`] without aborting the batch.
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    /// `push <refspec>` line could not be parsed.
    #[error("invalid push command {line:?}: expected `[+]<src>:<dst>`")]
    Parse {
        /// The offending line payload (after the `push ` prefix).
        line: String,
    },

    /// Local rev-spec failed permissive ref-name validation.
    #[error("invalid local ref-spec: {0:?}")]
    InvalidLocalSpec(String),

    /// Remote ref name is malformed.
    #[error("invalid remote ref: {0}")]
    RemoteRef(#[from] RefNameError),

    /// SHA hex extracted from a stored bundle key was malformed.
    #[error("invalid SHA in bundle key: {0}")]
    Sha(#[from] ShaError),

    /// Object-store transport / auth failure.
    #[error("object-store error during push: {0}")]
    Store(#[from] ObjectStoreError),

    /// Local git operation failed (rev-parse, bundle, archive).
    #[error("git error during push: {0}")]
    Git(#[from] GitError),

    /// Local I/O failure (tempdir, file read).
    #[error("local I/O error during push: {0}")]
    Io(#[from] std::io::Error),
}

/// Result of a single push within a batch. Rendered to stdout by the REPL
/// as either `ok <ref>\n` or `error <ref> <msg>\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// Push succeeded. `remote_ref` echoes back to git so it can mark
    /// the local ref as updated.
    Ok {
        /// The remote ref that was pushed (unparsed wire form).
        remote_ref: String,
    },
    /// Push was rejected. `message` is the free-form reason rendered
    /// after the ref name on the wire.
    Error {
        /// The remote ref the rejection applies to.
        remote_ref: String,
        /// Human-readable rejection reason.
        message: String,
    },
}

impl PushOutcome {
    /// Format `self` as the single line emitted on stdout (terminator
    /// included).
    #[must_use]
    pub(crate) fn as_protocol_line(&self) -> String {
        match self {
            PushOutcome::Ok { remote_ref } => format!("ok {remote_ref}\n"),
            PushOutcome::Error {
                remote_ref,
                message,
            } => format!("error {remote_ref} {message}\n"),
        }
    }
}

/// Parsed `push` command line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PushSpec {
    /// `+` was present — the user requested a force push.
    force: bool,
    /// User-supplied local rev-spec. Empty means "delete the remote ref".
    local_spec: String,
    /// Strict, fully-qualified remote ref.
    remote_ref: RefName,
}

/// Parse the payload of a `push <refspec>` line (the bytes after the
/// `push ` prefix have already been stripped by the REPL).
fn parse_push_args(args: &str) -> Result<PushSpec, PushError> {
    let parse_err = || PushError::Parse {
        line: args.to_owned(),
    };
    if args.is_empty() || args.contains(' ') {
        return Err(parse_err());
    }
    let (local, remote) = args.split_once(':').ok_or_else(parse_err)?;
    if remote.is_empty() {
        return Err(parse_err());
    }
    let (force, local) = match local.strip_prefix('+') {
        Some(rest) => (true, rest),
        None => (false, local),
    };
    if !local.is_empty() && !is_valid_ref_name(local) {
        return Err(PushError::InvalidLocalSpec(local.to_owned()));
    }
    let remote_ref = RefName::new(remote)?;
    Ok(PushSpec {
        force,
        local_spec: local.to_owned(),
        remote_ref,
    })
}

/// Build the `<prefix>/<ref>/` listing prefix used by lock and bundle
/// listings. Mirrors the no-prefix special case from
/// [`crate::protocol::fetch::bundle_key`].
fn ref_listing_prefix(prefix: Option<&str>, remote_ref: &RefName) -> String {
    match prefix {
        Some(p) if !p.is_empty() => format!("{p}/{remote_ref}/"),
        _ => format!("{remote_ref}/"),
    }
}

/// Build the bundle key for `<prefix>/<ref>/<sha>.bundle`.
fn bundle_key(prefix: Option<&str>, remote_ref: &RefName, sha: Sha) -> String {
    format!("{}{sha}.bundle", ref_listing_prefix(prefix, remote_ref))
}

/// Build the lock key: `<prefix>/<ref>/LOCK#.lock`.
fn lock_key(prefix: Option<&str>, remote_ref: &RefName) -> String {
    format!("{}LOCK#.lock", ref_listing_prefix(prefix, remote_ref))
}

/// Build the zip-archive key: `<prefix>/<ref>/repo.zip`.
fn archive_key(prefix: Option<&str>, remote_ref: &RefName) -> String {
    format!("{}repo.zip", ref_listing_prefix(prefix, remote_ref))
}

/// Build the HEAD key: `<prefix>/HEAD` (no slash when prefix is absent).
fn head_key(prefix: Option<&str>) -> String {
    match prefix {
        Some(p) if !p.is_empty() => format!("{p}/HEAD"),
        _ => "HEAD".to_owned(),
    }
}

/// Mirror upstream's `get_bundles_for_ref` filter:
/// drop any key containing `PROTECTED#`, `.zip`, `/LOCKS/`, or ending in
/// `.lock` (`../git-remote-s3/git_remote_s3/remote.py:323-344`). The
/// case-sensitive `.lock` suffix is deliberate — bucket keys are
/// case-sensitive and the lock filename is hard-coded.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn is_bundle_candidate(key: &str) -> bool {
    !key.contains("PROTECTED#")
        && !key.contains(".zip")
        && !key.contains("/LOCKS/")
        && !key.ends_with(".lock")
}

/// Returns every bundle object currently stored under `remote_ref`,
/// filtered like upstream's `get_bundles_for_ref`. The store's listing
/// prefix is `<prefix>/<ref>/` so sibling-ref keys don't leak in.
async fn bundles_for_ref(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    remote_ref: &RefName,
) -> Result<Vec<ObjectMeta>, ObjectStoreError> {
    let listing = ref_listing_prefix(prefix, remote_ref);
    let metas = store.list(&listing).await?;
    Ok(metas
        .into_iter()
        .filter(|m| is_bundle_candidate(&m.key))
        .collect())
}

/// Returns `true` iff a `<prefix>/<ref>/PROTECTED#…` marker exists.
async fn is_protected(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    remote_ref: &RefName,
) -> Result<bool, ObjectStoreError> {
    let listing = format!("{}PROTECTED#", ref_listing_prefix(prefix, remote_ref));
    let metas = store.list(&listing).await?;
    Ok(!metas.is_empty())
}

/// Extract the SHA from a `<…>/<sha>.bundle` key. Returns `None` if the
/// trailing segment does not match `[0-9a-f]{40}\.bundle`.
fn parse_remote_sha_from_key(key: &str) -> Option<Sha> {
    let last = key.rsplit('/').next()?;
    let stem = last.strip_suffix(".bundle")?;
    if stem.len() != 40 || !stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    Sha::from_hex(stem).ok()
}

/// Read the lock TTL from `GIT_REMOTE_S3_LOCK_TTL_SECONDS`, falling back
/// to [`DEFAULT_LOCK_TTL_SECONDS`] if the env var is unset or unparseable.
pub(crate) fn lock_ttl_from_env() -> Duration {
    let secs = env::var(ENV_LOCK_TTL_SECONDS)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_LOCK_TTL_SECONDS);
    // i64 cast: 60-ish seconds will never overflow; even MAX would just
    // saturate to ~292 billion years which is fine for a TTL ceiling.
    Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX))
}

/// Try to acquire the per-ref lock. Returns `Ok(true)` when the lock was
/// taken, `Ok(false)` on contention (caller should surface a "lock held"
/// error). On a stale lock (older than `ttl`), the lock is deleted and
/// the conditional `put_if_absent` is retried once.
///
/// The race window between `head` and the retry `put_if_absent` is
/// inherent to non-conditional deletes — another client could acquire
/// the lock between our delete and retry. We accept that race; the
/// retry `put_if_absent` will return `Ok(false)` and the user will
/// retry. Documented in `execution-plan.md` §5.2.
pub(crate) async fn acquire_lock(
    store: &dyn ObjectStore,
    lock_key: &str,
    ttl: Duration,
    now: OffsetDateTime,
) -> Result<bool, ObjectStoreError> {
    if store.put_if_absent(lock_key, Bytes::new()).await? {
        return Ok(true);
    }
    let meta = match store.head(lock_key).await {
        Ok(m) => m,
        // Lock vanished between put_if_absent and head — another client
        // released it. Treat as contention; user retries.
        Err(ObjectStoreError::NotFound(_)) => return Ok(false),
        Err(e) => return Err(e),
    };
    let age = now - meta.last_modified;
    if age <= ttl {
        return Ok(false);
    }
    debug!(key = %lock_key, age_secs = age.whole_seconds(), "deleting stale lock");
    delete_idempotent(store, lock_key).await?;
    store.put_if_absent(lock_key, Bytes::new()).await
}

/// Release a previously acquired per-ref lock. `NotFound` is mapped to
/// `Ok(())` (another client or the TTL may have already cleaned it up);
/// every other delete failure is propagated so the caller can surface
/// it. Mirrors upstream `release_lock`
/// (`../git-remote-s3/git_remote_s3/remote.py:408-416`).
pub(crate) async fn release_lock(
    store: &dyn ObjectStore,
    lock_key: &str,
) -> Result<(), ObjectStoreError> {
    delete_idempotent(store, lock_key).await
}

/// Idempotent delete: treats `NotFound` as success (another client may
/// have raced ahead) but propagates every other error.
async fn delete_idempotent(store: &dyn ObjectStore, key: &str) -> Result<(), ObjectStoreError> {
    match store.delete(key).await {
        Ok(()) | Err(ObjectStoreError::NotFound(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Drive a batch of `push` commands sequentially.
///
/// Each command is parsed, executed under its own per-ref lock, and
/// produces one [`PushOutcome`]. Catastrophic errors (transport,
/// malformed protocol input) abort the batch and bubble out as
/// [`PushError`]; per-ref failures are encoded as
/// [`PushOutcome::Error`] and the batch continues.
pub(crate) async fn push_batch(
    store: Arc<dyn ObjectStore>,
    prefix: Option<String>,
    repo_dir: Arc<PathBuf>,
    zip: bool,
    cmds: Vec<String>,
) -> Result<Vec<PushOutcome>, PushError> {
    if cmds.is_empty() {
        return Ok(Vec::new());
    }
    debug!(count = cmds.len(), "processing push batch");

    let ttl = lock_ttl_from_env();
    let mut outcomes = Vec::with_capacity(cmds.len());

    for cmd in cmds {
        // `parse_push_args` failures are catastrophic: a malformed `push`
        // line means we cannot trust subsequent commands. Abort the batch.
        let spec = parse_push_args(&cmd)?;
        // Capture the ref name before `push_one` consumes the spec so we
        // can still render an `error <ref> ...` line if the call fails.
        let remote_ref_str = spec.remote_ref.as_str().to_owned();
        let outcome = match push_one(
            store.as_ref(),
            prefix.as_deref(),
            repo_dir.as_path(),
            zip,
            ttl,
            OffsetDateTime::now_utc(),
            spec,
        )
        .await
        {
            Ok(o) => o,
            // Per-push operational failures (transport, local git, local I/O,
            // malformed remote bundle SHA) become `error <ref>` lines so the
            // batch can continue, mirroring upstream `cmd_push`'s try/except
            // shape (`../git-remote-s3/git_remote_s3/remote.py:286-296`).
            // Without this, a single 5xx blip in the middle of a multi-ref
            // push would silently drop the outcome lines for already-completed
            // pushes and leave git's local ref-tracking inconsistent with the
            // remote.
            Err(e)
                if matches!(
                    e,
                    PushError::Store(_) | PushError::Git(_) | PushError::Io(_) | PushError::Sha(_)
                ) =>
            {
                PushOutcome::Error {
                    remote_ref: remote_ref_str,
                    message: format!(r#""{e}"?"#),
                }
            }
            Err(e) => return Err(e),
        };
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// Recoverable per-push errors discovered while talking to the local
/// repo. Mapped by the caller into [`PushOutcome::Error`] strings.
enum GitProbeError {
    /// `local_spec` did not resolve in the local repo.
    LocalRefNotFound,
    /// Pre-existing remote bundle is not an ancestor of `local_sha`.
    NotAncestor,
}

/// Local git work that must run synchronously because `gix::Repository`
/// is `!Sync` and cannot cross `.await` points without making the
/// surrounding future `!Send`.
struct LocalGit {
    /// Resolved commit OID for the user's `local_spec`.
    local_sha: Sha,
    /// Working directory for `git bundle create` subprocess calls.
    cwd: PathBuf,
    /// On the zip path: archive on disk + metadata for the upload.
    /// `None` on the regular push path. The `TempDir` keeps the file
    /// alive until the async caller reads its bytes.
    zip_artifacts: Option<ZipArtifacts>,
}

struct ZipArtifacts {
    archive_path: PathBuf,
    short_sha: String,
    commit_msg: String,
    /// Owned tempdir that backs `archive_path`; dropped after upload.
    _tempdir: tempfile::TempDir,
}

/// Open the repo, resolve `local_sha`, optionally check ancestry, and
/// (for the zip variant) build the archive synchronously. The
/// `Repository` handle is dropped before this returns so the caller's
/// `Future` can stay `Send`. Archive bytes are NOT read here — the
/// async caller does that with `tokio::fs::read` to avoid blocking the
/// runtime on file I/O.
fn local_git_work(
    repo_dir: &Path,
    local_spec: &str,
    pre_existing_sha: Option<Sha>,
    force_push: bool,
    zip: bool,
) -> Result<Result<LocalGit, GitProbeError>, GitError> {
    let repo = gix::open(repo_dir)?;
    let cwd = repo.workdir().unwrap_or_else(|| repo.git_dir()).to_owned();

    let Ok(local_sha) = git::rev_parse(&repo, local_spec) else {
        return Ok(Err(GitProbeError::LocalRefNotFound));
    };

    if let (Some(remote_sha), false) = (pre_existing_sha, force_push)
        && !git::is_ancestor(&repo, remote_sha, local_sha)?
    {
        return Ok(Err(GitProbeError::NotAncestor));
    }

    let zip_artifacts = if zip {
        let tempdir = tempfile::Builder::new()
            .prefix("git_remote_object_store_archive_")
            .tempdir()?;
        let archive_path = git::archive(&repo, tempdir.path(), local_spec)?;
        let commit_msg = git::last_commit_message(&repo).unwrap_or_default();
        let sha_hex = local_sha.to_string();
        let short_sha = sha_hex[..8].to_owned();
        Some(ZipArtifacts {
            archive_path,
            short_sha,
            commit_msg,
            _tempdir: tempdir,
        })
    } else {
        None
    };

    drop(repo);
    Ok(Ok(LocalGit {
        local_sha,
        cwd,
        zip_artifacts,
    }))
}

/// Execute one push: lock, validate, upload, release. Mirrors the
/// upstream `cmd_push` body verbatim except where typed errors replace
/// stringy ones.
async fn push_one(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    repo_dir: &Path,
    zip: bool,
    ttl: Duration,
    now: OffsetDateTime,
    spec: PushSpec,
) -> Result<PushOutcome, PushError> {
    let PushSpec {
        force,
        local_spec,
        remote_ref,
    } = spec;
    let remote_ref_str = remote_ref.as_str().to_owned();

    if local_spec.is_empty() {
        return delete_remote_ref(store, prefix, &remote_ref, zip).await;
    }

    let force_push = if force {
        !is_protected(store, prefix, &remote_ref).await?
    } else {
        false
    };
    debug!(local = %local_spec, remote = %remote_ref, force_push, "push");

    let pre_bundles = bundles_for_ref(store, prefix, &remote_ref).await?;
    if pre_bundles.len() > 1 {
        return Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message: r#""multiple bundles exists on server. Run git-remote-object-store doctor to fix."?"#
                .to_owned(),
        });
    }
    let pre_existing = pre_bundles.into_iter().next().map(|m| m.key);

    let pre_existing_sha = match pre_existing.as_deref() {
        Some(key) => match parse_remote_sha_from_key(key) {
            Some(s) => Some(s),
            None => {
                return Ok(PushOutcome::Error {
                    remote_ref: remote_ref_str,
                    message: format!(
                        r#""unable to parse remote bundle key {key:?}; run git-remote-object-store doctor to fix."?"#,
                    ),
                });
            }
        },
        None => None,
    };

    // Sync gix work (rev-parse / ancestor / archive) runs in a
    // dedicated scope so the !Sync `Repository` is dropped before any
    // .await — keeps `push_batch`'s future `Send`.
    let probe = local_git_work(repo_dir, &local_spec, pre_existing_sha, force_push, zip)?;
    let local = match probe {
        Ok(local) => local,
        Err(GitProbeError::LocalRefNotFound) => {
            return Ok(PushOutcome::Error {
                remote_ref: remote_ref_str,
                message: format!(r#""{local_spec} not found"?"#),
            });
        }
        Err(GitProbeError::NotAncestor) => {
            return Ok(PushOutcome::Error {
                remote_ref: remote_ref_str,
                message: format!(r#""remote ref is not ancestor of {local_spec}."?"#,),
            });
        }
    };

    let temp_dir = tempfile::Builder::new()
        .prefix("git_remote_object_store_push_")
        .tempdir()?;
    let bundle_path =
        git::bundle_at(&local.cwd, temp_dir.path(), local.local_sha, &local_spec).await?;

    let lock = lock_key(prefix, &remote_ref);
    let acquired = acquire_lock(store, &lock, ttl, now).await?;
    if !acquired {
        return Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message: format!(
                r#""failed to acquire ref lock at {lock}. Another client may be pushing. If this persists beyond {}s, run git-remote-object-store doctor to inspect and optionally clear stale locks."?"#,
                ttl.whole_seconds(),
            ),
        });
    }

    // Run the lock-protected work, then release the lock unconditionally
    // before propagating the result. Mirrors upstream's `try/finally` so a
    // mid-push error never leaves the lock dangling for the full TTL.
    let result = perform_push_under_lock(
        store,
        prefix,
        &remote_ref,
        local.local_sha,
        pre_existing,
        &bundle_path,
        local.zip_artifacts,
    )
    .await;
    let release_result = release_lock(store, &lock).await;

    // Upstream `cmd_push` (`../git-remote-s3/git_remote_s3/remote.py:297-303`)
    // overrides a successful push with an error when the lock release
    // fails, so the operator is alerted and concurrent pushers are not
    // left hitting a dangling lock for the full TTL. A genuine push
    // error takes priority — do not mask it with the release failure.
    match (&result, release_result) {
        (Ok(PushOutcome::Ok { .. }), Err(e)) => {
            warn!(key = %lock, "failed to release lock: {e}");
            Ok(PushOutcome::Error {
                remote_ref: remote_ref_str,
                message: format!(
                    r#""failed to release lock. You may need to manually remove the lock {lock} from the server or use git-remote-object-store doctor to fix."?"#,
                ),
            })
        }
        // Every other combination: the push's own outcome (whether
        // Ok(Error{..}) or Err(..)) takes priority over the release
        // result — including when both fail. Log the release failure
        // so operators can spot dangling locks that coincide with push
        // errors; the lock TTL will eventually clean up.
        (_, Err(e)) => {
            warn!(key = %lock, "lock release failed (push already errored): {e}");
            result
        }
        _ => result,
    }
}

/// Re-list under the lock, upload the bundle, init HEAD, delete the
/// previous bundle, optionally upload `repo.zip`. Split out so the lock
/// release in the caller is unconditional.
async fn perform_push_under_lock(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    remote_ref: &RefName,
    local_sha: Sha,
    pre_existing: Option<String>,
    bundle_path: &std::path::Path,
    zip_artifacts: Option<ZipArtifacts>,
) -> Result<PushOutcome, PushError> {
    let current = bundles_for_ref(store, prefix, remote_ref).await?;
    if current.len() > 1 {
        return Ok(PushOutcome::Error {
            remote_ref: remote_ref.as_str().to_owned(),
            message: r#""multiple bundles exists for the same ref on server. Run git-remote-object-store doctor to fix."?"#.to_owned(),
        });
    }
    let current_key = current.into_iter().next().map(|m| m.key);
    if let (Some(prev), Some(now_key)) = (pre_existing.as_deref(), current_key.as_deref())
        && prev != now_key
    {
        return Ok(PushOutcome::Error {
            remote_ref: remote_ref.as_str().to_owned(),
            message: r#""stale remote. Please fetch and retry."?"#.to_owned(),
        });
    }

    let bundle_dest = bundle_key(prefix, remote_ref, local_sha);
    store
        .put_path(&bundle_dest, bundle_path, PutOpts::default())
        .await?;

    // HEAD bootstrap: write only if absent. Single round-trip via
    // put_if_absent — we don't care about the boolean (existing HEAD is
    // intentionally preserved).
    let head = head_key(prefix);
    store
        .put_if_absent(
            &head,
            Bytes::copy_from_slice(remote_ref.as_str().as_bytes()),
        )
        .await?;

    if let Some(prev) = current_key
        && prev != bundle_dest
    {
        delete_idempotent(store, &prev).await?;
    }

    if let Some(artifacts) = zip_artifacts {
        let opts = PutOpts {
            content_disposition: Some(format!(
                "attachment; filename=repo-{}.zip",
                artifacts.short_sha
            )),
            user_metadata: vec![(
                "codepipeline-artifact-revision-summary".to_owned(),
                artifacts.commit_msg,
            )],
        };
        let zip_dest = archive_key(prefix, remote_ref);
        store
            .put_path(&zip_dest, &artifacts.archive_path, opts)
            .await?;
    }

    Ok(PushOutcome::Ok {
        remote_ref: remote_ref.as_str().to_owned(),
    })
}

/// Handle a delete refspec (`:<remote_ref>`). Mirrors upstream
/// `remove_remote_ref`: list `<prefix>/<ref>/`, expect 1 (or 2 with zip)
/// keys, delete them all, emit `ok` or the appropriate error.
///
/// The listing is **unfiltered** on purpose — it counts `LOCK#.lock`,
/// `PROTECTED#`, and `repo.zip` against the expected total. Two
/// upstream behaviours fall out:
///
/// 1. A protected ref (`PROTECTED#` marker) cannot be deleted via
///    `git push :ref`: the marker inflates the count past `expected`,
///    triggering the multi-bundle error. Removing the marker first
///    (via the management CLI's `unprotect`) is required.
/// 2. A ref whose only object is a stale `LOCK#.lock` deletes that
///    lock as if it were the bundle and returns `ok`. This matches
///    upstream `remove_remote_ref` (`../git-remote-s3/git_remote_s3/remote.py:172-196`).
async fn delete_remote_ref(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    remote_ref: &RefName,
    zip: bool,
) -> Result<PushOutcome, PushError> {
    let listing = ref_listing_prefix(prefix, remote_ref);
    let entries = store.list(&listing).await?;
    let expected = if zip { 2 } else { 1 };
    let remote_ref_str = remote_ref.as_str().to_owned();
    if entries.len() == expected {
        for entry in &entries {
            delete_idempotent(store, &entry.key).await?;
        }
        Ok(PushOutcome::Ok {
            remote_ref: remote_ref_str,
        })
    } else if entries.is_empty() {
        Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message: "not found".to_owned(),
        })
    } else {
        Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message: r#""multiple bundles exists on server. Run git-remote-object-store doctor to fix."?"#.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::mock::MockStore;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn rn(s: &str) -> RefName {
        RefName::new(s).expect("RefName")
    }

    // --- parse_push_args ----------------------------------------------

    #[test]
    fn parse_push_args_accepts_canonical_form() {
        let spec = parse_push_args("refs/heads/main:refs/heads/main").expect("parse");
        assert!(!spec.force);
        assert_eq!(spec.local_spec, "refs/heads/main");
        assert_eq!(spec.remote_ref.as_str(), "refs/heads/main");
    }

    #[test]
    fn parse_push_args_accepts_force_flag() {
        let spec = parse_push_args("+refs/heads/main:refs/heads/main").expect("parse");
        assert!(spec.force);
        assert_eq!(spec.local_spec, "refs/heads/main");
    }

    #[test]
    fn parse_push_args_accepts_delete_form() {
        let spec = parse_push_args(":refs/heads/main").expect("parse");
        assert!(!spec.force);
        assert!(spec.local_spec.is_empty());
        assert_eq!(spec.remote_ref.as_str(), "refs/heads/main");
    }

    #[test]
    fn parse_push_args_accepts_short_local() {
        let spec = parse_push_args("HEAD:refs/heads/main").expect("parse");
        assert_eq!(spec.local_spec, "HEAD");
    }

    #[test]
    fn parse_push_args_rejects_missing_colon() {
        assert!(matches!(
            parse_push_args("refs/heads/main"),
            Err(PushError::Parse { .. })
        ));
    }

    #[test]
    fn parse_push_args_rejects_empty_remote() {
        assert!(matches!(
            parse_push_args("refs/heads/main:"),
            Err(PushError::Parse { .. })
        ));
    }

    #[test]
    fn parse_push_args_rejects_invalid_remote_ref() {
        assert!(matches!(
            parse_push_args("refs/heads/main:refs/heads/.bad"),
            Err(PushError::RemoteRef(_))
        ));
    }

    #[test]
    fn parse_push_args_rejects_invalid_local_spec() {
        assert!(matches!(
            parse_push_args("refs/heads/.bad:refs/heads/main"),
            Err(PushError::InvalidLocalSpec(_))
        ));
    }

    #[test]
    fn parse_push_args_rejects_embedded_whitespace() {
        assert!(matches!(
            parse_push_args("refs/heads/main:refs/heads/main extra"),
            Err(PushError::Parse { .. })
        ));
    }

    #[test]
    fn parse_push_args_rejects_empty_input() {
        assert!(matches!(parse_push_args(""), Err(PushError::Parse { .. })));
    }

    // --- key formatting -----------------------------------------------

    #[test]
    fn key_formatters_with_prefix() {
        let r = rn("refs/heads/main");
        let sha = Sha::from_hex(SHA).unwrap();
        assert_eq!(
            bundle_key(Some("repo"), &r, sha),
            format!("repo/refs/heads/main/{SHA}.bundle"),
        );
        assert_eq!(
            lock_key(Some("repo"), &r),
            "repo/refs/heads/main/LOCK#.lock"
        );
        assert_eq!(
            archive_key(Some("repo"), &r),
            "repo/refs/heads/main/repo.zip"
        );
        assert_eq!(head_key(Some("repo")), "repo/HEAD");
    }

    #[test]
    fn key_formatters_with_no_prefix() {
        let r = rn("refs/heads/main");
        let sha = Sha::from_hex(SHA).unwrap();
        assert_eq!(
            bundle_key(None, &r, sha),
            format!("refs/heads/main/{SHA}.bundle"),
        );
        assert_eq!(lock_key(None, &r), "refs/heads/main/LOCK#.lock");
        assert_eq!(archive_key(None, &r), "refs/heads/main/repo.zip");
        assert_eq!(head_key(None), "HEAD");
        // Empty-string prefix is treated identically to None.
        assert_eq!(head_key(Some("")), "HEAD");
        assert_eq!(lock_key(Some(""), &r), "refs/heads/main/LOCK#.lock");
    }

    // --- bundle filter ------------------------------------------------

    #[test]
    fn is_bundle_candidate_keeps_real_bundles() {
        assert!(is_bundle_candidate("repo/refs/heads/main/abc.bundle"));
        assert!(is_bundle_candidate("refs/heads/main/abc"));
    }

    #[test]
    fn is_bundle_candidate_rejects_protected_zip_lock() {
        assert!(!is_bundle_candidate("repo/refs/heads/main/PROTECTED#"));
        assert!(!is_bundle_candidate("repo/refs/heads/main/repo.zip"));
        assert!(!is_bundle_candidate("repo/refs/heads/main/LOCK#.lock"));
        assert!(!is_bundle_candidate("repo/refs/heads/main/file.lock"));
        assert!(!is_bundle_candidate("repo/refs/heads/main/LOCKS/x"));
    }

    // --- parse_remote_sha_from_key ------------------------------------

    #[test]
    fn parse_remote_sha_from_key_extracts_lower_hex_40() {
        let sha = parse_remote_sha_from_key(&format!("repo/refs/heads/main/{SHA}.bundle"))
            .expect("parse");
        assert_eq!(sha.to_string(), SHA);
    }

    #[test]
    fn parse_remote_sha_from_key_rejects_uppercase() {
        let upper = SHA.to_uppercase();
        assert!(parse_remote_sha_from_key(&format!("refs/heads/main/{upper}.bundle")).is_none());
    }

    #[test]
    fn parse_remote_sha_from_key_rejects_wrong_length() {
        let short = &SHA[..39];
        assert!(parse_remote_sha_from_key(&format!("refs/heads/main/{short}.bundle")).is_none());
    }

    #[test]
    fn parse_remote_sha_from_key_rejects_missing_extension() {
        assert!(parse_remote_sha_from_key(&format!("refs/heads/main/{SHA}")).is_none());
    }

    // --- bundles_for_ref / is_protected ------------------------------

    #[tokio::test]
    async fn bundles_for_ref_filters_protected_zip_lock() {
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        store.insert(
            format!("repo/refs/heads/main/{SHA}.bundle"),
            Bytes::from_static(b"b"),
        );
        store.insert("repo/refs/heads/main/PROTECTED#", Bytes::from_static(b""));
        store.insert("repo/refs/heads/main/repo.zip", Bytes::from_static(b""));
        store.insert("repo/refs/heads/main/LOCK#.lock", Bytes::from_static(b""));
        let bundles = bundles_for_ref(&store, Some("repo"), &r).await.unwrap();
        assert_eq!(bundles.len(), 1);
        assert!(bundles[0].key.ends_with(".bundle"));
    }

    #[tokio::test]
    async fn is_protected_detects_marker() {
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        assert!(!is_protected(&store, Some("repo"), &r).await.unwrap());
        store.insert("repo/refs/heads/main/PROTECTED#", Bytes::from_static(b""));
        assert!(is_protected(&store, Some("repo"), &r).await.unwrap());
    }

    // --- acquire_lock / release_lock ----------------------------------

    #[tokio::test]
    async fn acquire_lock_succeeds_when_absent() {
        let store = MockStore::new();
        let now = OffsetDateTime::now_utc();
        let acquired = acquire_lock(&store, "k", Duration::seconds(60), now)
            .await
            .unwrap();
        assert!(acquired);
        assert!(store.contains("k"));
    }

    #[tokio::test]
    async fn acquire_lock_returns_false_when_recently_held() {
        let store = MockStore::new();
        let now = OffsetDateTime::now_utc();
        store.insert_with("k", Bytes::new(), now, PutOpts::default());
        let acquired = acquire_lock(&store, "k", Duration::seconds(60), now)
            .await
            .unwrap();
        assert!(!acquired);
    }

    #[tokio::test]
    async fn acquire_lock_recovers_stale_lock() {
        let store = MockStore::new();
        let now = OffsetDateTime::now_utc();
        let stale = now - Duration::seconds(120);
        store.insert_with("k", Bytes::new(), stale, PutOpts::default());
        let acquired = acquire_lock(&store, "k", Duration::seconds(60), now)
            .await
            .unwrap();
        assert!(acquired);
        // Lock still exists (we re-created it with put_if_absent).
        assert!(store.contains("k"));
    }

    #[tokio::test]
    async fn acquire_lock_treats_disappeared_lock_as_contention() {
        // First put_if_absent says "exists", but head returns NotFound
        // (race: another client released between the calls). We must
        // surface contention, not error.
        use crate::object_store::mock::Fault;
        let store = MockStore::new();
        store.insert("k", Bytes::new());
        store.arm(Fault::NotFoundOnHead { key: "k".into() });
        let now = OffsetDateTime::now_utc();
        let acquired = acquire_lock(&store, "k", Duration::seconds(60), now)
            .await
            .unwrap();
        assert!(!acquired);
        // Confirm head() was actually called — a regression that skipped
        // the staleness branch and returned Ok(false) directly would also
        // satisfy `!acquired`. The fault firing proves head ran.
        assert_eq!(store.pending_faults(), 0);
    }

    #[tokio::test]
    async fn release_lock_swallows_not_found() {
        let store = MockStore::new();
        // Releasing an absent lock must map NotFound to Ok(()).
        release_lock(&store, "missing").await.unwrap();
    }

    #[tokio::test]
    async fn release_lock_deletes_existing_key() {
        let store = MockStore::new();
        store.insert("k", Bytes::new());
        release_lock(&store, "k").await.unwrap();
        assert!(!store.contains("k"));
    }

    #[tokio::test]
    async fn release_lock_propagates_non_not_found_errors() {
        use crate::object_store::mock::Fault;
        let store = MockStore::new();
        store.insert("k", Bytes::new());
        store.arm(Fault::NetworkOnDelete { key: "k".into() });
        let err = release_lock(&store, "k").await.unwrap_err();
        assert!(
            matches!(err, ObjectStoreError::Network(_)),
            "expected Network error, got {err:?}",
        );
        // The fault fired exactly once.
        assert_eq!(store.pending_faults(), 0);
        // Key remains because the delete was faulted, not executed.
        assert!(store.contains("k"));
    }

    // --- delete_remote_ref --------------------------------------------

    #[tokio::test]
    async fn delete_remote_ref_removes_single_bundle() {
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        store.insert(
            format!("repo/refs/heads/main/{SHA}.bundle"),
            Bytes::from_static(b"b"),
        );
        let outcome = delete_remote_ref(&store, Some("repo"), &r, false)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            PushOutcome::Ok {
                remote_ref: "refs/heads/main".into()
            }
        );
        assert!(!store.contains(&format!("repo/refs/heads/main/{SHA}.bundle")));
    }

    #[tokio::test]
    async fn delete_remote_ref_returns_not_found_when_empty() {
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        let outcome = delete_remote_ref(&store, Some("repo"), &r, false)
            .await
            .unwrap();
        match outcome {
            PushOutcome::Error { message, .. } => assert!(message.contains("not found")),
            PushOutcome::Ok { .. } => panic!("expected Error outcome"),
        }
    }

    #[tokio::test]
    async fn delete_remote_ref_rejects_protected_marker() {
        // PROTECTED# is unfiltered for the delete-path count, mirroring
        // upstream's bug-as-feature: protected refs cannot be deleted
        // via `git push :ref` because the marker inflates the count.
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        let bundle = format!("repo/refs/heads/main/{SHA}.bundle");
        let protected = "repo/refs/heads/main/PROTECTED#";
        store.insert(&bundle, Bytes::from_static(b"b"));
        store.insert(protected, Bytes::from_static(b""));
        let outcome = delete_remote_ref(&store, Some("repo"), &r, false)
            .await
            .unwrap();
        match outcome {
            PushOutcome::Error { message, .. } => assert!(message.contains("multiple bundles")),
            PushOutcome::Ok { .. } => panic!("expected Error outcome"),
        }
        // Both keys must remain — a regression that deleted on the way
        // to the error branch would still satisfy the message check.
        assert!(store.contains(&bundle));
        assert!(store.contains(protected));
    }

    #[tokio::test]
    async fn delete_remote_ref_zip_mode_expects_two_keys() {
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        let bundle = format!("repo/refs/heads/main/{SHA}.bundle");
        let zip = "repo/refs/heads/main/repo.zip";
        store.insert(&bundle, Bytes::from_static(b"b"));
        store.insert(zip, Bytes::from_static(b""));
        let outcome = delete_remote_ref(&store, Some("repo"), &r, true)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            PushOutcome::Ok {
                remote_ref: "refs/heads/main".into()
            }
        );
        // Verify both keys were actually deleted — without these, a
        // regression that returned Ok without invoking the delete loop
        // would still pass.
        assert!(!store.contains(&bundle));
        assert!(!store.contains(zip));
    }

    // --- PushOutcome rendering ----------------------------------------

    #[test]
    fn push_outcome_renders_ok_line() {
        let line = PushOutcome::Ok {
            remote_ref: "refs/heads/main".into(),
        }
        .as_protocol_line();
        assert_eq!(line, "ok refs/heads/main\n");
    }

    #[test]
    fn push_outcome_renders_error_line() {
        let line = PushOutcome::Error {
            remote_ref: "refs/heads/main".into(),
            message: r#""bad"?"#.into(),
        }
        .as_protocol_line();
        assert_eq!(line, "error refs/heads/main \"bad\"?\n");
    }

    /// Both duplicate-bundle paths (pre-lock at ~line 482 and under-lock
    /// at ~line 600) must produce wire output ending in `"?\n`. The `?`
    /// suffix is the project-wide Rust convention for `error <ref> "..."`
    /// messages — git treats `"..."?` as recoverable and `"..."` as
    /// fatal. Upstream Python omits the `?` on the under-lock branch
    /// (../git-remote-s3/git_remote_s3/remote.py:245); this is a
    /// deliberate normalization documented in `execution-plan.md`.
    #[test]
    fn duplicate_bundle_errors_use_consistent_wire_format() {
        let pre_lock_line = PushOutcome::Error {
            remote_ref: "refs/heads/main".into(),
            message: r#""multiple bundles exists on server. Run git-remote-object-store doctor to fix."?"#.to_owned(),
        }
        .as_protocol_line();
        let under_lock_line = PushOutcome::Error {
            remote_ref: "refs/heads/main".into(),
            message: r#""multiple bundles exists for the same ref on server. Run git-remote-object-store doctor to fix."?"#.to_owned(),
        }
        .as_protocol_line();

        assert_eq!(
            pre_lock_line,
            "error refs/heads/main \"multiple bundles exists on server. \
             Run git-remote-object-store doctor to fix.\"?\n",
        );
        assert_eq!(
            under_lock_line,
            "error refs/heads/main \"multiple bundles exists for the same ref on server. \
             Run git-remote-object-store doctor to fix.\"?\n",
        );
        assert!(pre_lock_line.ends_with("\"?\n"));
        assert!(under_lock_line.ends_with("\"?\n"));
    }

    // --- lock_ttl_from_env --------------------------------------------

    #[test]
    fn lock_ttl_from_env_defaults_when_unset() {
        // Use a guard pattern: clear the env then restore. Keep this
        // single-threaded relative to the other env-touching tests by
        // not parallel-mutating the same key.
        // SAFETY: tests run with `cargo test`'s default thread pool, but
        // this test only reads when the var is unset — which is the
        // normal test environment.
        // No mutation needed: the var is unset by default.
        if env::var(ENV_LOCK_TTL_SECONDS).is_ok() {
            // Skip if a parent harness sets the var.
            return;
        }
        let ttl = lock_ttl_from_env();
        assert_eq!(
            ttl,
            Duration::seconds(i64::try_from(DEFAULT_LOCK_TTL_SECONDS).unwrap()),
        );
    }
}

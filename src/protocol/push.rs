//! `push` handler with per-ref locking via conditional writes.
//!
//! Sequential-batch semantics: every `push <refspec>` line in a batch
//! is processed in order under its own per-ref lock, and one outcome
//! line is emitted per push (`ok <ref>\n` or `error <ref> "msg"\n`).
//! `gix::Repository` is `!Sync` so the handler holds the repo handle
//! on a single task — pushes never run in parallel within one client.
//!
//! Stdout discipline: this module returns [`PushOutcome`] values and
//! never writes to the protocol stream itself. The REPL renders each
//! outcome and the trailing blank-line terminator (see
//! `.claude/rules/protocol-stdout.md`).

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use time::{Duration, OffsetDateTime};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::git::{self, GitError, RefName, RefNameError, Sha, ShaError, is_valid_ref_name};
use crate::keys;
use crate::object_store::{ObjectMeta, ObjectStore, ObjectStoreError, ProgressSink, PutOpts};
use crate::url::{BackendKind, StorageEngine};

/// Default per-ref lock TTL, in seconds.
/// Default lock TTL (seconds) when [`ENV_LOCK_TTL_SECONDS`] is unset
/// or unparseable.
///
/// Widened to `pub` under `cfg(any(test, feature = "test-util"))` so
/// integration tests can pin the wire-format error message without
/// re-deriving the magic number from a parallel literal that could
/// drift from this constant unnoticed.
#[cfg(not(any(test, feature = "test-util")))]
pub(crate) const DEFAULT_LOCK_TTL_SECONDS: u64 = 60;

/// Default lock TTL (seconds) when [`ENV_LOCK_TTL_SECONDS`] is unset
/// or unparseable. Exposed for integration-test wire-format pinning.
#[cfg(any(test, feature = "test-util"))]
pub const DEFAULT_LOCK_TTL_SECONDS: u64 = 60;

/// Push configuration that is constant across an entire batch.
///
/// Bundles the `zip`, `engine`, and `ttl` parameters so that [`push_one`]
/// and [`perform_push_under_lock`] stay within the argument-count budget.
struct PushConfig {
    /// Whether to upload `repo.zip` alongside each bundle.
    zip: bool,
    /// Storage engine to lock into the `FORMAT` key on the first push.
    engine: StorageEngine,
    /// Per-ref lock TTL.
    ttl: Duration,
    /// Backend kind of the destination. Used by the zip-artifact path to
    /// decide whether to attach the `codepipeline-artifact-revision-summary`
    /// user-metadata header — that header is meaningful only on S3
    /// (AWS `CodePipeline` consumes it) and the hyphenated key is invalid on
    /// Azure, where it would otherwise cause the entire zip upload to fail
    /// silently under the issue #127 best-effort swallow contract.
    /// See issue #161.
    kind: BackendKind,
}

/// Environment override for the lock TTL, in seconds.
pub(crate) const ENV_LOCK_TTL_SECONDS: &str = "GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS";

/// Stable substring embedded in the rejection message returned when the
/// remote ref is not an ancestor of the pushed local ref. Treated as a
/// user-facing contract: shellspec suites assert on this token to
/// distinguish the ancestor-mismatch failure mode from unrelated push
/// failures (network, permission, malformed URL, ...). Reword the
/// surrounding sentence freely; do not change this token without
/// updating every call site, including the spec files under
/// `spec/integration/*/force_push_spec.sh` and
/// `spec/live/*/force_push_spec.sh`.
pub(crate) const NOT_ANCESTOR_TOKEN: &str = "not ancestor";

/// Build the canonical wire-format rejection message returned when a
/// push is refused because the remote ref is not an ancestor of
/// `local_spec`. Centralising the template here keeps the bundle and
/// packchain engines in lockstep — they previously inlined byte-identical
/// `format!` calls, and silent drift between them would have produced
/// engine-dependent wire output the spec suites could not match against
/// [`NOT_ANCESTOR_TOKEN`].
pub(crate) fn not_ancestor_wire_message(local_spec: &str) -> String {
    format!(r#""remote ref is {NOT_ANCESTOR_TOKEN} of {local_spec}."?"#)
}

/// Canonical wire-format message returned when a delete is refused
/// because a `PROTECTED#` marker is present under the ref. Shared by
/// the bundle engine ([`delete_remote_ref_under_lock`]) and the
/// packchain engine ([`crate::packchain`]'s `delete_remote_ref_packchain`)
/// so both surface identical bytes to git/clients. A single source of
/// truth here avoids the duplicate-literal drift that a `const _` byte
/// equality guard previously had to defend against.
pub(crate) const DELETE_PROTECTION_MESSAGE: &str = r#""ref is protected. Run git-remote-object-store unprotect <url> <branch> to remove protection before deleting."?"#;

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

    /// Packchain-engine-specific failure surfaced by
    /// [`crate::packchain::push::push_batch`]. Wrapped here so the
    /// per-ref `error <ref>` arm in [`push_batch`] can render a
    /// uniform wire line regardless of which engine produced the
    /// error.
    #[error("packchain engine error during push: {0}")]
    Packchain(#[from] crate::packchain::PackchainError),
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
    pub(crate) fn to_protocol_line(&self) -> String {
        match self {
            Self::Ok { remote_ref } => format!("ok {remote_ref}\n"),
            Self::Error {
                remote_ref,
                message,
            } => format!("error {remote_ref} {message}\n"),
        }
    }
}

/// Parsed `push` command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushSpec {
    /// `+` was present — the user requested a force push.
    pub(crate) force: bool,
    /// User-supplied local rev-spec. Empty means "delete the remote ref".
    pub(crate) local_spec: String,
    /// Strict, fully-qualified remote ref.
    pub(crate) remote_ref: RefName,
}

/// Parse the payload of a `push <refspec>` line (the bytes after the
/// `push ` prefix have already been stripped by the REPL).
pub(crate) fn parse_push_args(args: &str) -> Result<PushSpec, PushError> {
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
/// listings. Empty / absent prefix collapses to a bare `<ref>/`.
///
/// Thin typed wrapper over [`keys::ref_listing_prefix`] that takes the
/// validated `RefName` newtype so helper-protocol call sites can't pass
/// an unvalidated string. The shared helper is the single allocation
/// point — `manage::doctor` and `manage::branch` reach it directly with
/// already-validated `&str` ref paths.
pub(crate) fn ref_listing_prefix(prefix: Option<&str>, remote_ref: &RefName) -> String {
    keys::ref_listing_prefix(prefix, remote_ref.as_str())
}

/// Build the lock key: `<prefix>/<ref>/LOCK#.lock`.
pub(crate) fn lock_key(prefix: Option<&str>, remote_ref: &RefName) -> String {
    format!("{}LOCK#.lock", ref_listing_prefix(prefix, remote_ref))
}

/// Build the zip-archive key: `<prefix>/<ref>/repo.zip`.
fn archive_key(prefix: Option<&str>, remote_ref: &RefName) -> String {
    format!("{}repo.zip", ref_listing_prefix(prefix, remote_ref))
}

/// Build the HEAD key: `<prefix>/HEAD` (no slash when prefix is absent).
pub(crate) fn head_key(prefix: Option<&str>) -> String {
    keys::join(prefix, "HEAD")
}

/// Bundle-candidate filter: positive predicate — the final path segment
/// must be `<sha>.bundle` (40 lower-hex chars + `.bundle`). All other
/// per-ref siblings (`repo.zip`, `LOCK#.lock`, `PROTECTED#`, any
/// hypothetical future artefact) are rejected by construction.
///
/// Earlier revisions filtered by full-key substring (`.zip`, `/LOCKS/`,
/// `.lock`), which false-positively dropped real bundle keys whose ref
/// name happened to contain those substrings — e.g.
/// `refs/heads/v1.zip-rc1/<sha>.bundle` or
/// `refs/heads/LOCKS-feature/x/<sha>.bundle`. A positive check on the
/// final segment cannot misfire on ref-name content.
fn is_bundle_candidate(key: &str) -> bool {
    parse_remote_sha_from_key(key).is_some()
}

/// Returns every bundle object currently stored under `remote_ref`,
/// filtered by [`is_bundle_candidate`]. The store's listing prefix is
/// `<prefix>/<ref>/` so sibling-ref keys don't leak in.
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

/// Returns `true` iff a `<prefix>/<ref>/PROTECTED#` marker exists.
///
/// Uses an exact-key `head` rather than a prefix `list`. `ObjectStore::list`
/// is a byte-prefix match, so a `list("…/PROTECTED#")` would also return any
/// future `PROTECTED#`-prefixed sibling key (e.g. `PROTECTED#audit`). The
/// equality check here matches the semantics of the canonical
/// `is_protected_marker_segment` snapshot-side helper and avoids
/// spuriously blocking pushes or deletes on unprotected refs.
pub(crate) async fn is_protected(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    remote_ref: &RefName,
) -> Result<bool, ObjectStoreError> {
    let key = format!(
        "{}{}",
        ref_listing_prefix(prefix, remote_ref),
        keys::PROTECTED_MARKER_SEGMENT,
    );
    match store.head(&key).await {
        Ok(_) => Ok(true),
        Err(ObjectStoreError::NotFound(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Extract the SHA from a `<…>/<sha>.bundle` key. Returns `None` if the
/// trailing segment does not match `[0-9a-f]{40}\.bundle`.
///
/// Charset/length validation goes through [`keys::is_valid_bundle_stem`]
/// so the doctor's malformed-key detector and this parser agree on
/// what counts as a well-formed stem. The defensive parse-error arm in
/// [`prepare_push`] still calls this — although push's pre-lock listing
/// filters malformed stems via [`is_bundle_candidate`] before reaching
/// the arm, the guard remains in place so a future caller that bypasses
/// the filter cannot silently treat a malformed key as a bundle.
fn parse_remote_sha_from_key(key: &str) -> Option<Sha> {
    let last = key.rsplit('/').next()?;
    let stem = last.strip_suffix(".bundle")?;
    if !keys::is_valid_bundle_stem(stem) {
        return None;
    }
    Sha::from_hex(stem).ok()
}

/// Read the lock TTL from `GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS`,
/// falling back to [`DEFAULT_LOCK_TTL_SECONDS`] if the env var is unset,
/// unparseable, or zero. A zero TTL would make `acquire_lock` treat
/// every held lock as instantly stale and defeat per-ref locking, so
/// we mirror [`crate::packchain::gc::grace_hours_from_env`] and clamp
/// it to the default.
pub(crate) fn lock_ttl_from_env() -> Duration {
    let secs = env::var(ENV_LOCK_TTL_SECONDS)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_LOCK_TTL_SECONDS);
    // i64 cast: 60-ish seconds will never overflow; even MAX would just
    // saturate to ~292 billion years which is fine for a TTL ceiling.
    Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX))
}

/// Handle to an acquired per-ref lock with a live heartbeat task.
///
/// Returned by [`acquire_lock`]. The guard owns a background tokio task
/// that periodically re-PUTs the lock key (overwrite, not conditional)
/// to refresh `last_modified` and keep stale-lock recovery from
/// stealing a still-live lock from under a long-running critical
/// section (issue #118).
///
/// ## Release
///
/// Call [`release_lock`] (or [`Self::release`]) to relinquish: this
/// signals cooperative shutdown to the heartbeat, awaits its exit so
/// any in-flight heartbeat PUT completes (or errors) on the server
/// BEFORE the DELETE is issued, then deletes the lock key. Without
/// the await-then-delete ordering, an in-flight heartbeat PUT could
/// settle on the server after the DELETE and resurrect the lock key
/// as an orphan (issue #150).
///
/// ## Drop
///
/// If a guard is dropped without an explicit release (panic, early
/// return without the post-result match, etc.) the heartbeat is told
/// to stop via the same shutdown signal and the join handle is
/// aborted (we cannot `.await` in `Drop`). The lock key remains on
/// the bucket and will be picked up by the next acquire attempt's
/// stale-recovery path after `ttl` elapses (bounded by
/// `ttl + heartbeat_interval` since the last heartbeat). Callers
/// should still prefer explicit release so the lock is freed
/// immediately.
#[must_use = "lock guards must be released; dropping leaks the lock until TTL"]
pub(crate) struct LockGuard {
    /// Bucket key of the lock. `pub(crate)` so call sites can format
    /// it into error messages (the old API surfaced `&str`).
    lock_key: String,
    /// Store handle, kept around so [`Self::release`] can delete the
    /// key without callers re-passing it. Also given to the heartbeat
    /// task via clone.
    store: Arc<dyn ObjectStore>,
    /// Cooperative shutdown signal. The heartbeat task watches this
    /// receiver via [`tokio::select!`]: a `send(true)` wakes the loop
    /// at any await point inside the `select!` (between ticks) AND is
    /// checked synchronously before issuing each `put_bytes`, so no
    /// PUT can be dispatched after [`Self::release`] has begun.
    shutdown: watch::Sender<bool>,
    /// Heartbeat task handle. `Some` while the guard owns a live
    /// heartbeat; taken by release (awaited) or drop (aborted as a
    /// fallback since `Drop` cannot await).
    heartbeat: Option<JoinHandle<()>>,
}

/// Upper bound on how long [`LockGuard::release`] waits for the
/// heartbeat task to exit cooperatively before falling back to abort.
///
/// The heartbeat task wakes from its `select!` immediately on
/// shutdown and only issues a PUT if it had already passed the
/// shutdown re-check, so the worst-case wait is one in-flight
/// `put_bytes` RTT. Five seconds is well above any healthy RTT but
/// well below any reasonable user-visible push timeout — if the
/// network is wedged hard enough to outlast it, the fallback abort
/// keeps `release` from hanging and the orphan key just regresses to
/// the pre-fix behaviour (stale-lock recovery after TTL).
const HEARTBEAT_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl LockGuard {
    /// Release the lock: cooperatively stop the heartbeat (awaiting
    /// the task's exit so any in-flight PUT settles first), then
    /// delete the lock key. `NotFound` is mapped to `Ok(())` (the
    /// heartbeat may have raced a stale-recovery delete, or an
    /// operator may have cleared the lock manually); every other
    /// delete failure is propagated.
    pub(crate) async fn release(mut self) -> Result<(), ObjectStoreError> {
        // ORDER IS LOAD-BEARING: stop the heartbeat AND wait for its
        // join handle BEFORE deleting the lock key. The previous
        // implementation called `handle.abort()` and then DELETE
        // synchronously, but `abort()` only cancels the future at its
        // next await point — a `put_bytes` already in flight on the
        // server completes regardless of cancellation and can settle
        // AFTER our DELETE, resurrecting the lock key as an orphan
        // (issue #150). Cooperative shutdown via `watch` + join-await
        // makes the worst case "one in-flight PUT completes before
        // DELETE" instead of "one in-flight PUT completes after
        // DELETE".
        self.stop_heartbeat().await;
        delete_idempotent(self.store.as_ref(), &self.lock_key).await
    }

    /// Signal cooperative shutdown to the heartbeat task and await
    /// its exit (bounded by [`HEARTBEAT_JOIN_TIMEOUT`]). On timeout
    /// the handle is `abort()`-ed, restoring the pre-#150 behaviour
    /// for the pathological case of a `put_bytes` hung past the
    /// timeout.
    async fn stop_heartbeat(&mut self) {
        // `send` only fails when there are no receivers — i.e. the
        // heartbeat task has already exited. Either way, there's no
        // PUT we can still prevent, so we ignore the result.
        let _ = self.shutdown.send(true);
        let Some(mut handle) = self.heartbeat.take() else {
            return;
        };
        // Borrow the handle into `timeout` so we can still call
        // `abort()` on it after the wait fails.
        if tokio::time::timeout(HEARTBEAT_JOIN_TIMEOUT, &mut handle)
            .await
            .is_err()
        {
            warn!(
                key = %self.lock_key,
                timeout_secs = HEARTBEAT_JOIN_TIMEOUT.as_secs(),
                "lock heartbeat did not exit within join timeout; \
                 falling back to abort (in-flight PUT may still race \
                 the upcoming DELETE)",
            );
            handle.abort();
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // `Drop` cannot `.await`, so we cannot mirror the release
        // path's "wait for in-flight PUT to settle" guarantee here.
        // The best we can do is (a) signal shutdown so the heartbeat
        // task exits at its next await point, and (b) abort the
        // JoinHandle so a future PUT issued while we were dropping
        // doesn't keep racing forever. The lock key may briefly
        // outlive the holder; the stale-lock recovery path
        // (`acquire_lock`) reclaims it after TTL.
        let _ = self.shutdown.send(true);
        if let Some(handle) = self.heartbeat.take() {
            handle.abort();
        }
    }
}

/// Heartbeat interval: `ttl/3`, floored at one second so a pathological
/// sub-second TTL doesn't busy-loop. We use `ttl/3` rather than `ttl/2`
/// so a single missed heartbeat (transient network blip with
/// `MissedTickBehavior::Delay`) still leaves margin before `age > ttl`:
/// two consecutive misses are needed to push the lock past the
/// staleness threshold (#118 follow-up).
pub(crate) fn heartbeat_interval(ttl: Duration) -> std::time::Duration {
    let secs = ttl.whole_seconds().max(3) / 3;
    // `try_from` rather than `as`: we just clamped to ≥ 1, but
    // `clippy::cast_sign_loss` insists on the explicit fallible cast.
    let secs_u64 =
        u64::try_from(secs).expect("ttl.whole_seconds().max(3) / 3 is always >= 1 (non-negative)");
    std::time::Duration::from_secs(secs_u64)
}

/// Try to acquire the per-ref lock. Returns `Ok(Some(guard))` when the
/// lock was taken (heartbeat is already running) and `Ok(None)` on
/// contention (caller should surface a "lock held" error). On a stale
/// lock (older than `ttl`, with no heartbeat from a live holder), the
/// lock is deleted and the conditional `put_if_absent` is retried
/// once.
///
/// The race window between `head` and the retry `put_if_absent` is
/// inherent to non-conditional deletes — another client could acquire
/// the lock between our delete and retry. We accept that race; the
/// retry `put_if_absent` will return `Ok(false)` and the user will
/// retry.
///
/// Heartbeat semantics (issue #118): a live holder refreshes the lock
/// every `ttl/3` so the staleness check correctly excludes locks held
/// by an in-flight critical section. A long-running [`compact`] no
/// longer races a concurrent writer that wakes up after the original
/// TTL elapses.
///
/// [`compact`]: crate::packchain::compact::compact
pub(crate) async fn acquire_lock(
    store: Arc<dyn ObjectStore>,
    lock_key: &str,
    ttl: Duration,
    now: OffsetDateTime,
) -> Result<Option<LockGuard>, ObjectStoreError> {
    if store.put_if_absent(lock_key, Bytes::new()).await? {
        return Ok(Some(spawn_lock_guard(store, lock_key.to_owned(), ttl)));
    }
    let meta = match store.head(lock_key).await {
        Ok(m) => m,
        // Lock vanished between put_if_absent and head — another client
        // released it. Treat as contention; user retries.
        Err(ObjectStoreError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let age = now - meta.last_modified;
    if age <= ttl {
        return Ok(None);
    }
    debug!(key = %lock_key, age_secs = age.whole_seconds(), "deleting stale lock");
    delete_idempotent(store.as_ref(), lock_key).await?;
    if store.put_if_absent(lock_key, Bytes::new()).await? {
        Ok(Some(spawn_lock_guard(store, lock_key.to_owned(), ttl)))
    } else {
        Ok(None)
    }
}

/// Build a [`LockGuard`] and spawn its heartbeat task. The heartbeat
/// re-PUTs the lock key every `heartbeat_interval(ttl)` (overwrite, not
/// conditional) so the stale-lock recovery branch in [`acquire_lock`]
/// correctly excludes still-held locks. Heartbeat failures are logged
/// at `warn` and retried on the next tick — a transient blip should
/// not surface as a critical-section-aborting error.
///
/// The task watches a [`watch::Receiver<bool>`]: `release` flips it to
/// `true` and the loop exits at the next `select!` await point.
/// Critically, the loop also re-checks the flag synchronously after
/// waking from the tick and BEFORE issuing `put_bytes`, so once
/// `release` has signalled shutdown no further PUT can be dispatched
/// — closing the issue #150 race where an `abort()`-cancelled task
/// had an in-flight `put_bytes` that settled on the server after
/// `release`'s DELETE.
fn spawn_lock_guard(store: Arc<dyn ObjectStore>, lock_key: String, ttl: Duration) -> LockGuard {
    let interval = heartbeat_interval(ttl);
    let task_store = Arc::clone(&store);
    let task_key = lock_key.clone();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // Skip the immediate first tick — the acquire just wrote the
        // key, so a refresh in the same millisecond would be wasted
        // bandwidth.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // immediate
        loop {
            tokio::select! {
                // `biased` so shutdown is checked first on every
                // wake: a tick and a shutdown that fire on the same
                // poll must resolve to shutdown, otherwise the tick
                // arm could win and issue one final PUT after release
                // has begun.
                biased;
                _ = shutdown_rx.changed() => break,
                _ = tick.tick() => {}
            }
            // Re-check the shutdown flag synchronously between the
            // tick and the `put_bytes` await. Without this, a
            // `release` that signals shutdown AFTER `tick.tick()`
            // resolved but BEFORE we issued `put_bytes` would still
            // let the PUT through and race the DELETE — exactly the
            // issue #150 failure mode the `select!` alone does not
            // close.
            if *shutdown_rx.borrow() {
                break;
            }
            match task_store
                .put_bytes(&task_key, Bytes::new(), PutOpts::default())
                .await
            {
                Ok(()) => debug!(key = %task_key, "lock heartbeat refreshed"),
                Err(e) => warn!(
                    key = %task_key,
                    error = %e,
                    "lock heartbeat refresh failed; will retry",
                ),
            }
        }
    });
    LockGuard {
        lock_key,
        store,
        shutdown: shutdown_tx,
        heartbeat: Some(handle),
    }
}

/// Release a previously acquired per-ref lock. `NotFound` is mapped to
/// `Ok(())` (another client or the TTL may have already cleaned it up);
/// every other delete failure is propagated so the caller can surface it.
///
/// Heartbeat semantics: the guard's heartbeat task is aborted before
/// the delete so a heartbeat refresh in flight cannot re-create the
/// key after we've removed it.
pub(crate) async fn release_lock(guard: LockGuard) -> Result<(), ObjectStoreError> {
    guard.release().await
}

/// Idempotent delete: treats `NotFound` as success (another client may
/// have raced ahead) but propagates every other error.
pub(crate) async fn delete_idempotent(
    store: &dyn ObjectStore,
    key: &str,
) -> Result<(), ObjectStoreError> {
    match store.delete(key).await {
        Ok(()) | Err(ObjectStoreError::NotFound(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Best-effort delete of the prior bundle once the new bundle is durable.
///
/// Issue #121: `perform_push_under_lock` writes the new bundle first
/// (the durable commit) and then deletes the previous bundle. A
/// non-`NotFound` error on the delete must NOT fail the push — the new
/// bundle is already on the bucket, so reporting failure to the user
/// is a lie about the remote state. The two-bundle state that results
/// from the orphan trips the under-lock "multiple bundles" guard on
/// the next push, which directs the operator to `doctor`; the warn
/// log gives them the orphan key directly.
///
/// Mirrors `force_push_baseline_cleanup` in `packchain::push`
/// (issue #113).
async fn delete_prior_bundle_best_effort(
    store: &dyn ObjectStore,
    remote_ref: &RefName,
    prior_key: &str,
) {
    if let Err(e) = delete_idempotent(store, prior_key).await {
        warn!(
            ref_path = %remote_ref.as_str(),
            key = %prior_key,
            error = %e,
            "prior-bundle cleanup failed (new bundle already committed); \
             orphan key left for manual cleanup",
        );
    }
}

/// Best-effort upload of the optional zip artifact once the new bundle
/// is durable.
///
/// Issue #127: `perform_push_under_lock` writes the new bundle, `HEAD`,
/// and `FORMAT` (the git-protocol contract for a successful push) and
/// then uploads an optional `repo.zip` CodePipeline-side convenience
/// artifact. A non-`NotFound` error on the zip upload must NOT fail
/// the push — the bundle is already durable, so reporting failure is
/// a lie about the remote state. The next push re-puts the zip at
/// the same key (idempotent), so an operator who notices the warn log
/// or wants the artifact re-uploaded simply re-pushes.
///
/// Mirrors [`delete_prior_bundle_best_effort`] (issue #121) and
/// `force_push_baseline_cleanup` in `packchain::push` (issue #113):
/// log at warn with `ref_path`, `key`, and `error`, and return
/// without propagating.
async fn upload_zip_artifact_best_effort(
    store: &dyn ObjectStore,
    remote_ref: &RefName,
    zip_dest: &str,
    archive_path: &Path,
    opts: PutOpts,
) {
    if let Err(e) = store.put_path(zip_dest, archive_path, opts).await {
        warn!(
            ref_path = %remote_ref.as_str(),
            key = %zip_dest,
            error = %e,
            "zip artifact upload failed (bundle already committed); \
             retry the push to re-upload the zip at the same key",
        );
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
    ctx: &super::BatchCtx,
    kind: BackendKind,
    zip: bool,
    engine: StorageEngine,
    cmds: Vec<String>,
) -> Result<Vec<PushOutcome>, PushError> {
    if cmds.is_empty() {
        return Ok(Vec::new());
    }
    debug!(count = cmds.len(), "processing push batch");

    let config = PushConfig {
        zip,
        engine,
        ttl: lock_ttl_from_env(),
        kind,
    };
    let mut outcomes = Vec::with_capacity(cmds.len());

    for cmd in cmds {
        // `parse_push_args` failures are catastrophic: a malformed `push`
        // line means we cannot trust subsequent commands. Abort the batch.
        let spec = parse_push_args(&cmd)?;
        // Capture the ref name before `push_one` consumes the spec so we
        // can still render an `error <ref> ...` line if the call fails.
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
            // Per-push operational failures (transport, local git, local I/O,
            // malformed remote bundle SHA) become `error <ref>` lines so the
            // batch can continue. Without this, a single 5xx blip in the
            // middle of a multi-ref push would silently drop the outcome
            // lines for already-completed pushes and leave git's local
            // ref-tracking inconsistent with the remote.
            Err(e)
                if matches!(
                    e,
                    PushError::Store(_) | PushError::Git(_) | PushError::Io(_) | PushError::Sha(_)
                ) =>
            {
                let chain = full_error_chain(&e);
                warn!(ref = %remote_ref_str, error = %chain, "push ref failed");
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

/// Render a [`PushError`] as a colon-separated chain so the
/// `error <ref>` wire line and the stderr log carry every level of
/// causal context.
///
/// `PushError`'s `thiserror` formats inline the immediate source via
/// `{0}` / `{source}`, and some of those sources (notably
/// [`ObjectStoreError::Network`], whose own Display embeds its boxed
/// inner error) themselves inline a further level — so a naive
/// chain-walk would produce `"object-store error during push: network
/// error: dns failure: dns failure"`. [`super::append_source_chain`]
/// dedups any level whose text is already at the tail of `msg`, so
/// the rendered chain is uniform across `Store`, `Git`, `Io`, and `Sha`
/// variants without per-variant special-casing.
fn full_error_chain(err: &PushError) -> String {
    let mut msg = err.to_string();
    super::append_source_chain(&mut msg, err);
    msg
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
    /// Working directory passed to `bundle::create`.
    cwd: PathBuf,
    /// On the zip path: archive on disk + metadata for the upload.
    /// `None` on the regular push path. The `TempDir` keeps the file
    /// alive until the async caller reads its bytes.
    zip_artifacts: Option<ZipArtifacts>,
    /// Whether the pre-lock-listed remote bundle's SHA was an ancestor
    /// of `local_sha`. `true` when there was no pre-existing bundle.
    /// Stashed so [`perform_push_under_lock`] can decide, under the
    /// per-ref lock, whether a force-push against a now-`PROTECTED#`
    /// ref is a safe fast-forward (issue #129).
    pre_existing_was_ancestor: bool,
}

struct ZipArtifacts {
    archive_path: PathBuf,
    short_sha: String,
    commit_msg: String,
    /// Owned tempdir that backs `archive_path`; dropped after upload.
    _tempdir: tempfile::TempDir,
}

/// All state computed before the per-ref lock is acquired.
///
/// Passed to [`perform_push_under_lock`] so the argument count stays within
/// the clippy budget and the two phases of [`push_one`] have a clean
/// boundary: pre-lock work (protect check, bundle listing, local git,
/// bundle creation) vs. under-lock work (re-list, upload, HEAD/FORMAT
/// init, old-bundle deletion).
struct PushReadyState {
    remote_ref: RefName,
    local_sha: Sha,
    /// Key of the pre-existing bundle (if any). Checked inside the lock
    /// to detect concurrent pushes (stale-remote guard).
    pre_existing: Option<String>,
    bundle_path: PathBuf,
    zip_artifacts: Option<ZipArtifacts>,
    engine: StorageEngine,
    /// User's `--force` intent. Carried into the lock window so the
    /// `PROTECTED#` check can run *under* the lock and close the TOCTOU
    /// window between the pre-lock check and the lock acquisition
    /// (issue #129).
    force: bool,
    /// Whether the pre-lock-listed bundle's SHA was an ancestor of
    /// `local_sha` (always `true` when there was no pre-existing
    /// bundle). Used together with `force` and the under-lock
    /// protection re-check to decide whether to reject a force-push
    /// that became protected mid-flight.
    pre_existing_was_ancestor: bool,
    /// Original local refspec, kept so the under-lock `NotAncestor` error
    /// message renders the same text the pre-lock arm would have used.
    local_spec: String,
    /// Keeps the temp directory (and `bundle_path`) alive until the bundle
    /// is uploaded inside `perform_push_under_lock`.
    _temp_dir: tempfile::TempDir,
}

/// Outcome of [`prepare_push`]: one of three paths into [`push_one`]'s
/// lock window.
///
/// - [`PrepareOutcome::Ready`] — pre-lock work completed; caller must
///   acquire the per-ref lock and call [`perform_push_under_lock`].
/// - [`PrepareOutcome::Delete`] — delete refspec (`:<ref>`); caller must
///   acquire the per-ref lock and call [`delete_remote_ref_under_lock`].
///   Issue #133: the listing and sweep MUST run under the lock so a
///   concurrent push that lands a new bundle cannot turn the delete
///   into a silent false success.
/// - [`PrepareOutcome::Done`] — outcome already decided pre-lock
///   (multi-bundle corruption, ancestry failure, …); caller returns it
///   directly without taking the lock.
enum PrepareOutcome {
    Ready(PushReadyState),
    Delete { remote_ref: RefName, zip: bool },
    Done(PushOutcome),
}

/// Open the repo, resolve `local_sha`, compute ancestry of the
/// pre-existing remote bundle, and (for the zip variant) build the
/// archive synchronously. The `Repository` handle is dropped before
/// this returns so the caller's `Future` can stay `Send`. Archive
/// bytes are NOT read here — the async caller does that with
/// `tokio::fs::read` to avoid blocking the runtime on file I/O.
///
/// Ancestry is always computed and returned via
/// [`LocalGit::pre_existing_was_ancestor`]; this function only emits
/// [`GitProbeError::NotAncestor`] for the non-force case. Force pushes
/// defer the ancestry-vs-protection decision to
/// [`perform_push_under_lock`] so a concurrent `protect` cannot race
/// the pre-lock check (issue #129).
fn local_git_work(
    repo_dir: &Path,
    local_spec: &str,
    pre_existing_sha: Option<Sha>,
    force_push: bool,
    zip: bool,
) -> Result<Result<LocalGit, GitProbeError>, GitError> {
    let repo = gix::open(repo_dir)?;
    let cwd = repo.workdir().unwrap_or_else(|| repo.git_dir()).to_owned();

    let Ok(local_sha) = git::branch::resolve(&repo, local_spec) else {
        return Ok(Err(GitProbeError::LocalRefNotFound));
    };

    let pre_existing_was_ancestor = match pre_existing_sha {
        None => true,
        Some(remote_sha) => git::is_ancestor(&repo, remote_sha, local_sha)?,
    };
    if !force_push && !pre_existing_was_ancestor {
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
        pre_existing_was_ancestor,
    }))
}

/// Perform all pre-lock work for a push: resolve the local ref, check
/// ancestry, list the remote bundles, and create the local bundle file.
/// Returns [`PrepareOutcome::Done`] for cases that are already resolved
/// (delete-refspec, protection check, multiple-bundle error, ancestry
/// failure) or [`PrepareOutcome::Ready`] when the caller should proceed
/// to acquire the per-ref lock and call [`perform_push_under_lock`].
async fn prepare_push(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    repo_dir: &Path,
    config: &PushConfig,
    spec: PushSpec,
) -> Result<PrepareOutcome, PushError> {
    let PushSpec {
        force,
        local_spec,
        remote_ref,
    } = spec;
    let remote_ref_str = remote_ref.as_str().to_owned();

    if local_spec.is_empty() {
        // Issue #133: do NOT list / sweep here. The bundle engine's
        // delete must run INSIDE the per-ref lock so a concurrent push
        // landing a new bundle between our listing and our deletion
        // cannot produce a silent false success. Defer to
        // [`delete_remote_ref_under_lock`] in [`push_one`].
        return Ok(PrepareOutcome::Delete {
            remote_ref,
            zip: config.zip,
        });
    }

    // Issue #129: do NOT call `is_protected` here. A pre-lock check
    // races against a concurrent `protect`: the check could pass and a
    // `PROTECTED#` marker could land before we acquire the per-ref
    // lock, letting a force-push overwrite a now-protected ref. The
    // check is performed *under* the lock in
    // [`perform_push_under_lock`] instead. Pre-lock we just respect the
    // user's `--force` intent; the `local_git_work` probe still returns
    // ancestry info via [`LocalGit::pre_existing_was_ancestor`] so the
    // under-lock arm can render the same NotAncestor message a
    // protection-demoted non-force push would have produced.
    let force_push = force;
    debug!(local = %local_spec, remote = %remote_ref, force_push, "push");

    let pre_bundles = bundles_for_ref(store, prefix, &remote_ref).await?;
    if pre_bundles.len() > 1 {
        return Ok(PrepareOutcome::Done(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message:
                r#""multiple bundles exist on server. Run git-remote-object-store doctor to fix."?"#
                    .to_owned(),
        }));
    }
    let pre_existing = pre_bundles.into_iter().next().map(|m| m.key);

    let pre_existing_sha = match pre_existing.as_deref() {
        None => None,
        Some(key) => {
            let Some(s) = parse_remote_sha_from_key(key) else {
                return Ok(PrepareOutcome::Done(PushOutcome::Error {
                    remote_ref: remote_ref_str,
                    message: format!(
                        r#""unable to parse remote bundle key {key:?}; run git-remote-object-store doctor to fix."?"#,
                    ),
                }));
            };
            Some(s)
        }
    };

    // Sync gix work (rev-parse / ancestor / archive) runs in a
    // dedicated scope so the !Sync `Repository` is dropped before any
    // .await — keeps `push_batch`'s future `Send`.
    let probe = local_git_work(
        repo_dir,
        &local_spec,
        pre_existing_sha,
        force_push,
        config.zip,
    )?;
    let local = match probe {
        Ok(local) => local,
        Err(GitProbeError::LocalRefNotFound) => {
            return Ok(PrepareOutcome::Done(PushOutcome::Error {
                remote_ref: remote_ref_str,
                message: format!(r#""{local_spec} not found"?"#),
            }));
        }
        Err(GitProbeError::NotAncestor) => {
            return Ok(PrepareOutcome::Done(PushOutcome::Error {
                remote_ref: remote_ref_str,
                message: not_ancestor_wire_message(&local_spec),
            }));
        }
    };

    let temp_dir = tempfile::Builder::new()
        .prefix("git_remote_object_store_push_")
        .tempdir()?;
    let bundle_path =
        git::bundle_at(&local.cwd, temp_dir.path(), local.local_sha, &local_spec).await?;

    Ok(PrepareOutcome::Ready(PushReadyState {
        remote_ref,
        local_sha: local.local_sha,
        pre_existing,
        bundle_path,
        zip_artifacts: local.zip_artifacts,
        engine: config.engine,
        force,
        pre_existing_was_ancestor: local.pre_existing_was_ancestor,
        local_spec,
        _temp_dir: temp_dir,
    }))
}

/// Execute one push or delete: prepare, lock, do work, release.
///
/// Both the upload path ([`perform_push_under_lock`]) and the delete
/// path ([`delete_remote_ref_under_lock`]) run inside the SAME per-ref
/// lock window with identical acquire/release accounting:
///
/// - `acquire_lock` returning `None` → emit the standard
///   "failed to acquire ref lock" wire error without doing work.
/// - `release_lock` failing AFTER successful work → downgrade the
///   outcome to an `Error` so the operator is alerted to a potentially
///   dangling lock.
/// - A genuine work error takes priority over a release failure — the
///   release error is logged but never masks the original failure.
///
/// Issue #133: putting the delete path under this same window closes
/// the race where a concurrent push could land a new bundle between a
/// pre-lock listing and a pre-lock sweep, producing a silent false
/// success.
async fn push_one(
    store: Arc<dyn ObjectStore>,
    prefix: Option<&str>,
    repo_dir: &Path,
    config: &PushConfig,
    now: OffsetDateTime,
    spec: PushSpec,
) -> Result<PushOutcome, PushError> {
    let (remote_ref_str, work): (String, UnderLockWork) =
        match prepare_push(store.as_ref(), prefix, repo_dir, config, spec).await? {
            PrepareOutcome::Done(o) => return Ok(o),
            PrepareOutcome::Ready(state) => (
                state.remote_ref.as_str().to_owned(),
                UnderLockWork::Push(Box::new(state)),
            ),
            PrepareOutcome::Delete { remote_ref, zip } => (
                remote_ref.as_str().to_owned(),
                UnderLockWork::Delete { remote_ref, zip },
            ),
        };

    let lock = match &work {
        UnderLockWork::Push(state) => lock_key(prefix, &state.remote_ref),
        UnderLockWork::Delete { remote_ref, .. } => lock_key(prefix, remote_ref),
    };

    let Some(guard) = acquire_lock(Arc::clone(&store), &lock, config.ttl, now).await? else {
        return Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message: format!(
                r#""failed to acquire ref lock at {lock}. Another client may be pushing. If this persists beyond {}s, run git-remote-object-store doctor to inspect and optionally clear stale locks."?"#,
                config.ttl.whole_seconds(),
            ),
        });
    };

    let result = match work {
        UnderLockWork::Push(state) => {
            perform_push_under_lock(store.as_ref(), prefix, config.kind, *state).await
        }
        UnderLockWork::Delete { remote_ref, zip } => {
            delete_remote_ref_under_lock(store.as_ref(), prefix, &remote_ref, zip, &lock).await
        }
    };
    let release_result = release_lock(guard).await;

    match (&result, release_result) {
        (Ok(PushOutcome::Ok { .. }), Err(e)) => {
            warn!(key = %lock, error = %e, "failed to release lock");
            Ok(PushOutcome::Error {
                remote_ref: remote_ref_str,
                message: format!(
                    r#""failed to release lock. You may need to manually remove the lock {lock} from the server or use git-remote-object-store doctor to fix."?"#,
                ),
            })
        }
        (_, Err(e)) => {
            warn!(key = %lock, error = %e, "lock release failed (work already errored)");
            result
        }
        _ => result,
    }
}

/// The work to perform inside the per-ref lock acquired by [`push_one`].
///
/// `PushReadyState` is significantly larger than the `Delete` variant
/// (paths, the temp dir guard, the captured refspec); boxing it keeps
/// the [`UnderLockWork`] discriminant compact regardless of variant.
enum UnderLockWork {
    Push(Box<PushReadyState>),
    Delete { remote_ref: RefName, zip: bool },
}

/// Re-list under the lock, upload the bundle, init HEAD, write the `FORMAT`
/// key, delete the previous bundle, optionally upload `repo.zip`. Split out
/// so the lock release in the caller is unconditional.
async fn perform_push_under_lock(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    kind: BackendKind,
    state: PushReadyState,
) -> Result<PushOutcome, PushError> {
    let PushReadyState {
        remote_ref,
        local_sha,
        pre_existing,
        bundle_path,
        zip_artifacts,
        engine,
        force,
        pre_existing_was_ancestor,
        local_spec,
        _temp_dir,
    } = state;
    let remote_ref_str = remote_ref.as_str().to_owned();

    // Issue #129: under-lock force-push protection check. A pre-lock
    // check would race against a concurrent `protect`; running here —
    // after `acquire_lock`, before any writes — closes that TOCTOU
    // window. The historical "protected ref + force" semantic is
    // "demote to non-force": if the local SHA would have been a
    // fast-forward against the pre-lock-listed remote (already pinned
    // by the stale-remote guard below), let the push through; if not,
    // emit the same NotAncestor wire error a pre-lock non-force probe
    // would have produced. The `is_protected` helper uses `head`, not
    // `list`, so this adds one cheap key probe and does not duplicate
    // any existing under-lock listing.
    if force && !pre_existing_was_ancestor && is_protected(store, prefix, &remote_ref).await? {
        return Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message: not_ancestor_wire_message(&local_spec),
        });
    }

    let current = bundles_for_ref(store, prefix, &remote_ref).await?;
    if current.len() > 1 {
        return Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message: r#""multiple bundles exist for the same ref on server. Run git-remote-object-store doctor to fix."?"#.to_owned(),
        });
    }
    let current_key = current.into_iter().next().map(|m| m.key);
    // Compare the pre-lock snapshot to the under-lock reality. All five
    // (pre / under) cases must agree:
    //   None / None           — happy path, no bundle existed before or now.
    //   Some(K) / Some(K)     — happy path, same bundle (e.g. force-push that
    //                           re-uploads against the same SHA).
    //   Some(A) / Some(B)     — concurrent push replaced our snapshot's
    //                           bundle. Reject: we'd silently overwrite it.
    //   None / Some           — concurrent push created a bundle after our
    //                           pre-lock list. Reject.
    //   Some / None           — concurrent delete (`git push :ref`) removed
    //                           our snapshot's bundle. Reject.
    // Without this guard a concurrent writer between the pre-lock list and
    // the lock acquisition would be silently overwritten.
    if pre_existing.as_deref() != current_key.as_deref() {
        return Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message: r#""stale remote. Please fetch and retry."?"#.to_owned(),
        });
    }

    let bundle_dest = keys::bundle_key(prefix, &remote_ref, local_sha);
    // Stat once for "X / total" formatting in progress logs. The
    // multipart upload re-stats `bundle_path` to size the part plan;
    // a brief race here would surface as the log showing a stale
    // total — never as wrong bytes — so we tolerate it. On stat
    // failure (vanishingly rare on a tempdir we just wrote) fall
    // back to "unknown" rather than aborting the push.
    let bundle_total = tokio::fs::metadata(&bundle_path)
        .await
        .map(|m| m.len())
        .ok();
    let bundle_opts = PutOpts {
        progress: Some(bundle_progress_sink(&bundle_dest, bundle_total)),
        ..PutOpts::default()
    };
    store
        .put_path(&bundle_dest, &bundle_path, bundle_opts)
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

    // Lock in the storage engine on the first push. `put_if_absent` makes
    // concurrent first-push races safe — both write the same `bundle` value,
    // so the one that loses is a no-op. The boolean result is intentionally
    // ignored: an existing FORMAT key was already validated at connect time by
    // `backend::build`.
    let format_key = keys::join(prefix, "FORMAT");
    store
        .put_if_absent(&format_key, Bytes::from_static(engine.as_str().as_bytes()))
        .await?;

    if let Some(prev) = current_key
        && prev != bundle_dest
    {
        delete_prior_bundle_best_effort(store, &remote_ref, &prev).await;
    }

    if let Some(artifacts) = zip_artifacts {
        let zip_dest = archive_key(prefix, &remote_ref);
        let zip_total = tokio::fs::metadata(&artifacts.archive_path)
            .await
            .map(|m| m.len())
            .ok();
        // Issue #161: the `codepipeline-artifact-revision-summary` user
        // metadata is only meaningful on S3 (AWS CodePipeline consumes it
        // as `x-amz-meta-codepipeline-artifact-revision-summary`). Azure
        // metadata names must be valid C# identifiers (no hyphens), so
        // attaching the header on the Azure path causes the entire blob
        // upload to fail with `InvalidMetadata` — and the issue #127
        // best-effort swallow then hides the failure, leaving every
        // `?zip=1` push silently missing `repo.zip`. Omit the header
        // outside S3 so the zip artifact lands successfully.
        let user_metadata = match kind {
            BackendKind::S3 => vec![(
                "codepipeline-artifact-revision-summary".to_owned(),
                sanitize_metadata_value(&artifacts.commit_msg),
            )],
            BackendKind::Azure => Vec::new(),
        };
        let opts = PutOpts {
            content_disposition: Some(format!(
                "attachment; filename=repo-{}.zip",
                artifacts.short_sha
            )),
            user_metadata,
            progress: Some(bundle_progress_sink(&zip_dest, zip_total)),
        };
        upload_zip_artifact_best_effort(
            store,
            &remote_ref,
            &zip_dest,
            &artifacts.archive_path,
            opts,
        )
        .await;
    }

    Ok(PushOutcome::Ok {
        remote_ref: remote_ref_str,
    })
}

/// Build a [`ProgressSink`] that emits one structured `tracing::info!`
/// line per chunk transferred during a bundle / zip-archive upload.
///
/// Issue #55. Git's helper protocol has no upload-progress channel
/// (the helper-protocol stdout is reserved for protocol traffic per
/// `.claude/rules/protocol-stdout.md`), so the bundle path's only way
/// to inform the user during a multi-GiB upload is `tracing::info!`,
/// which routes to stderr via the tracing-subscriber initialised in
/// `main()`. The LFS path keeps its own sink wiring (one
/// progress-event JSON line per chunk on stdout, governed by the LFS
/// custom-transfer protocol).
///
/// `total` is the bundle size at the moment we stat'd it. A short
/// race against a writer that re-stats during multipart planning
/// would only mis-format the log line — it cannot drive wrong-byte
/// behaviour. `None` renders "unknown" so the call site can
/// gracefully degrade when stat'ing the source fails.
///
/// The granularity of events is whatever the backend's `put_path`
/// provides: one event per completed multipart part / staged block
/// for bodies above [`crate::object_store::multipart::MULTIPART_PUT_THRESHOLD`],
/// one event total for bodies below it. With the default 16 MiB
/// part size and 8-way concurrency, a 1 GiB bundle emits ~64 lines
/// — ample motion to spot a stall, far short of log spam.
pub(crate) fn bundle_progress_sink(key: &str, total: Option<u64>) -> ProgressSink {
    // The closure needs an owned `String` because `ProgressSink` is
    // `'static`; cloning here keeps the call sites' borrow intact so
    // they can pass `&bundle_dest` to `put_path` without juggling
    // ownership.
    let key = key.to_owned();
    let bytes_so_far = Arc::new(AtomicU64::new(0));
    ProgressSink::new(move |bytes_amount| {
        // `Ordering::Relaxed` is enough: we're not synchronising with
        // any other state, just maintaining a monotonic counter for
        // log lines. The worst a re-ordered store could do is print
        // an out-of-order count, which `tracing` already disclaims.
        let so_far = bytes_so_far
            .fetch_add(bytes_amount, Ordering::Relaxed)
            .saturating_add(bytes_amount);
        // Render `total` as a Display value so the field is omitted
        // when stat'ing the source failed at the call site. Tracing's
        // `Option<u64>` rendering would print "None" — uglier than a
        // bare absence of the field.
        if let Some(t) = total {
            info!(
                key = %key,
                bytes_so_far = so_far,
                total = t,
                bytes_chunk = bytes_amount,
                "uploading"
            );
        } else {
            info!(
                key = %key,
                bytes_so_far = so_far,
                bytes_chunk = bytes_amount,
                "uploading"
            );
        }
    })
}

/// Replace ASCII control characters in `s` with spaces so the result
/// is safe to use as an HTTP header value.
///
/// `commit_msg` flows from `git::last_commit_message` (whose summary
/// is "everything before the first blank line" per gix) into the
/// `codepipeline-artifact-revision-summary` user-metadata header on
/// the zip-archive upload. A maliciously-crafted commit could embed
/// `\r\n` in the summary, which would be a CRLF injection on
/// transport — splitting one logical header into two and letting an
/// attacker forge arbitrary user-metadata headers on the uploaded
/// archive. Both backends' SDKs reject CRLF at the transport layer
/// today, but that defense is version-dependent and the resulting
/// error is a cryptic "invalid header" 400; sanitising here surfaces
/// a clean, predictable value at the call site instead.
fn sanitize_metadata_value(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Expected key count for a non-zip ref: one bundle object.
const DELETE_EXPECTED_NO_ZIP: usize = 1;
/// Expected key count for a zip ref: one bundle + one archive object.
const DELETE_EXPECTED_WITH_ZIP: usize = 2;

/// Handle a delete refspec (`:<remote_ref>`) UNDER the per-ref lock
/// acquired by [`push_one`]: list `<prefix>/<ref>/`, expect 1 (or 2
/// with zip) keys after filtering out the lock we hold, delete them
/// all, emit `ok` or the appropriate error.
///
/// Issue #133: this must run inside the lock window. A pre-lock
/// listing-then-sweep races a concurrent push that lands a new bundle
/// between the listing and the deletion, producing a silent false
/// success — the delete reports `ok` to git while the ref survives
/// with a different bundle on the server.
///
/// The lock key (`<prefix>/<ref>/LOCK#.lock`) is filtered from the
/// listing — `release_lock` removes it last, after this function
/// returns. Apart from that one filter, the listing is unfiltered and
/// counts `PROTECTED#` and `repo.zip` against the expected total.
///
/// Issue #128: the `PROTECTED#` marker check is the FIRST guard, run
/// against the fresh under-lock listing BEFORE any count-vs-expected
/// dispatch. The pre-#128 ordering only consulted the marker in the
/// `else if` mismatch branch, so a count-matching listing (e.g.
/// `[bundle.bundle, PROTECTED#]` in zip mode, or `[PROTECTED#]` alone
/// in non-zip mode) would sweep the marker and report `ok`. With the
/// guard at the top, that bypass is closed.
///
/// Four behaviours fall out:
///
/// 1. **Protected ref** — listing (lock filtered out) includes a key
///    whose final segment is the [`keys::PROTECTED_MARKER_SEGMENT`].
///    Emit a protection-specific refusal naming the management CLI's
///    `unprotect` workflow.
/// 2. **Count matches `expected`, no marker** — sweep the entries and
///    report `ok`.
/// 3. **No bundle present** — listing (lock filtered out) is empty.
///    Returns the `"not found"?` wire error. This now includes the case
///    of a ref whose only on-server state was a stale `LOCK#.lock`:
///    `acquire_lock` recovers the stale lock, the post-lock listing
///    contains only our newly-held lock, and the filter renders it
///    empty.
/// 4. **Genuine multi-bundle / corruption** — count exceeds `expected`
///    and no marker is present. Fall through to the doctor message.
///    Example listing:
///    `[ "<prefix>/refs/heads/main/<sha-a>.bundle",
///       "<prefix>/refs/heads/main/<sha-b>.bundle" ]`.
async fn delete_remote_ref_under_lock(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    remote_ref: &RefName,
    zip: bool,
    lock_key: &str,
) -> Result<PushOutcome, PushError> {
    let listing = ref_listing_prefix(prefix, remote_ref);
    let all_entries = store.list(&listing).await?;
    // Issue #133: filter out the lock key we hold. `release_lock`
    // removes it last; the sweep below must not touch it (deleting our
    // own lock mid-critical-section would let concurrent clients
    // acquire it). The remaining count is what the protocol's
    // count-vs-expected dispatch operates on.
    let entries: Vec<&ObjectMeta> = all_entries.iter().filter(|e| e.key != lock_key).collect();
    let expected = if zip {
        DELETE_EXPECTED_WITH_ZIP
    } else {
        DELETE_EXPECTED_NO_ZIP
    };
    let remote_ref_str = remote_ref.as_str().to_owned();
    // Issue #128: the canonical protection guard. Run it FIRST against
    // the fresh, under-lock listing, BEFORE the count-match deletion
    // branch. The pre-#128 ordering only checked for the marker in the
    // `else if` mismatch branch, so a count-matching listing (e.g.
    // `[bundle.bundle, PROTECTED#]` with `expected = 2` in zip mode, or
    // `[PROTECTED#]` alone with `expected = 1`) would sweep the marker
    // and complete the delete silently. `entries_have_protected_marker`
    // matches only the literal `PROTECTED#` last segment — never the
    // `LOCK#.lock` lock key — so scanning the unfiltered `all_entries`
    // here is safe and avoids re-deriving a filtered view solely for
    // this check.
    if keys::entries_have_protected_marker(&all_entries) {
        return Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message: DELETE_PROTECTION_MESSAGE.to_owned(),
        });
    }
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
            message: r#""not found"?"#.to_owned(),
        })
    } else {
        // Genuine multi-bundle / corruption: `entries` has more than
        // `expected` items and no `PROTECTED#` marker (that case
        // returned at the top guard above). Fall through to the doctor
        // message — see issue #128 for the routing rationale.
        Ok(PushOutcome::Error {
            remote_ref: remote_ref_str,
            message:
                r#""multiple bundles exist on server. Run git-remote-object-store doctor to fix."?"#
                    .to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::mock::MockStore;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    /// A second 40-char hex SHA distinct from `SHA`. Used by tests that
    /// need to seed pre-existing state under a SHA different from
    /// `local_sha` so a regression cannot pass through coincidental key
    /// alignment between `pre_existing` and `bundle_dest`.
    const OTHER_SHA: &str = "ffffffffffffffffffffffffffffffffffffffff";

    /// Compile-time guard: replaces the runtime `assert_ne!(other_sha, SHA, …)`
    /// that the constant lift removed. A typo making the two consts equal
    /// fails the build rather than silently letting a stale-remote test
    /// pass for the wrong reason.
    const _: () = {
        let a = SHA.as_bytes();
        let b = OTHER_SHA.as_bytes();
        let mut i = 0;
        let mut differs = a.len() != b.len();
        while i < a.len() && i < b.len() {
            if a[i] != b[i] {
                differs = true;
            }
            i += 1;
        }
        assert!(differs, "OTHER_SHA must differ from SHA");
    };

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
    fn parse_push_args_accepts_force_delete_form() {
        let spec = parse_push_args("+:refs/heads/main").expect("parse");
        assert!(spec.force);
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
            keys::bundle_key(Some("repo"), &r, sha),
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
            keys::bundle_key(None, &r, sha),
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
        assert!(is_bundle_candidate(&format!(
            "repo/refs/heads/main/{SHA}.bundle"
        )));
        assert!(is_bundle_candidate(&format!(
            "refs/heads/main/{SHA}.bundle"
        )));
    }

    #[test]
    fn is_bundle_candidate_rejects_protected_zip_lock() {
        assert!(!is_bundle_candidate("repo/refs/heads/main/PROTECTED#"));
        assert!(!is_bundle_candidate("repo/refs/heads/main/repo.zip"));
        assert!(!is_bundle_candidate("repo/refs/heads/main/LOCK#.lock"));
        assert!(!is_bundle_candidate("repo/refs/heads/main/file.lock"));
        assert!(!is_bundle_candidate("repo/refs/heads/main/LOCKS/x"));
    }

    /// Regression: refs whose names embed `.zip` as a substring must
    /// not be filtered out. Previously the predicate rejected any key
    /// containing `.zip` anywhere in the byte sequence.
    #[test]
    fn is_bundle_candidate_keeps_refs_containing_zip_substring() {
        assert!(is_bundle_candidate(&format!(
            "repo/refs/heads/v1.zip-rc1/{SHA}.bundle"
        )));
        assert!(is_bundle_candidate(&format!(
            "refs/heads/myrelease.zip-v1/{SHA}.bundle"
        )));
    }

    /// Regression: refs whose names embed `LOCKS` as a substring must
    /// not be filtered out. Previously the predicate rejected any key
    /// containing `/LOCKS/` anywhere in the byte sequence.
    #[test]
    fn is_bundle_candidate_keeps_refs_containing_locks_substring() {
        assert!(is_bundle_candidate(&format!(
            "repo/refs/heads/LOCKS-feature/x/{SHA}.bundle"
        )));
        assert!(is_bundle_candidate(&format!(
            "refs/heads/LOCKS/sub/{SHA}.bundle"
        )));
    }

    /// Regression: a ref-name segment ending in `.lock` is permitted by
    /// `gix_validate`; bundle keys under such refs must still match.
    /// The unwanted `.lock` sibling is `<ref>/LOCK#.lock`, where the
    /// final segment is `LOCK#.lock`, not `<sha>.bundle`.
    #[test]
    fn is_bundle_candidate_keeps_refs_containing_lock_substring() {
        assert!(is_bundle_candidate(&format!(
            "refs/heads/feature.lock-rc/{SHA}.bundle"
        )));
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

    /// Regression for #109: a ref whose name contains `.zip` must
    /// not have its bundle silently filtered out.
    #[tokio::test]
    async fn bundles_for_ref_keeps_bundle_when_ref_name_contains_zip() {
        let store = MockStore::new();
        let r = rn("refs/heads/v1.zip-rc1");
        let bundle_key = format!("repo/refs/heads/v1.zip-rc1/{SHA}.bundle");
        store.insert(bundle_key.clone(), Bytes::from_static(b"b"));
        let bundles = bundles_for_ref(&store, Some("repo"), &r).await.unwrap();
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].key, bundle_key);
    }

    /// Regression for #109: a ref whose name contains `LOCKS` must
    /// not have its bundle silently filtered out.
    #[tokio::test]
    async fn bundles_for_ref_keeps_bundle_when_ref_name_contains_locks() {
        let store = MockStore::new();
        let r = rn("refs/heads/LOCKS-feature/x");
        let bundle_key = format!("repo/refs/heads/LOCKS-feature/x/{SHA}.bundle");
        store.insert(bundle_key.clone(), Bytes::from_static(b"b"));
        let bundles = bundles_for_ref(&store, Some("repo"), &r).await.unwrap();
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].key, bundle_key);
    }

    #[tokio::test]
    async fn is_protected_detects_marker() {
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        assert!(!is_protected(&store, Some("repo"), &r).await.unwrap());
        store.insert("repo/refs/heads/main/PROTECTED#", Bytes::from_static(b""));
        assert!(is_protected(&store, Some("repo"), &r).await.unwrap());
    }

    /// Regression for #119: only the exact `PROTECTED#` key counts as a
    /// protection marker. A sibling key that merely starts with
    /// `PROTECTED#` (e.g. `PROTECTED#audit`) must not flip the result.
    #[tokio::test]
    async fn is_protected_ignores_protected_prefixed_sibling() {
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        store.insert(
            "repo/refs/heads/main/PROTECTED#audit",
            Bytes::from_static(b""),
        );
        assert!(!is_protected(&store, Some("repo"), &r).await.unwrap());
    }

    /// Regression for #119: `is_protected` must use `head`, not `list`.
    /// Arm `AccessDeniedOnAnyList`; if the implementation calls `list`
    /// at all, the fault fires and the call returns an error. We assert
    /// success and that the fault is still pending (i.e. unfired).
    #[tokio::test]
    async fn is_protected_uses_head_not_list() {
        use crate::object_store::mock::Fault;
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        store.arm(Fault::AccessDeniedOnAnyList);
        let got = is_protected(&store, Some("repo"), &r).await.unwrap();
        assert!(!got);
        assert_eq!(store.pending_faults(), 1, "is_protected must not call list");
        // And the positive case too — still no list.
        store.insert("repo/refs/heads/main/PROTECTED#", Bytes::from_static(b""));
        let got = is_protected(&store, Some("repo"), &r).await.unwrap();
        assert!(got);
        assert_eq!(store.pending_faults(), 1, "is_protected must not call list");
    }

    // --- acquire_lock / release_lock ----------------------------------
    //
    // Issue #118: `acquire_lock` returns a `LockGuard` instead of a
    // plain bool because the lock now carries a background heartbeat
    // task. Tests construct an `Arc<dyn ObjectStore>` so the heartbeat
    // task can clone the store; `MockStore`'s internal state is
    // already `Arc<Mutex<...>>`-shared, so the `Arc` clone is shape
    // bookkeeping, not extra state.

    #[tokio::test]
    async fn acquire_lock_succeeds_when_absent() {
        let store = Arc::new(MockStore::new());
        let now = OffsetDateTime::now_utc();
        let guard = acquire_lock(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            "k",
            Duration::seconds(60),
            now,
        )
        .await
        .unwrap();
        assert!(guard.is_some(), "expected a fresh guard");
        assert!(store.contains("k"));
        // Drop the guard so the heartbeat task exits before the
        // runtime tears down (also covered explicitly by
        // `lock_guard_drop_aborts_heartbeat`).
        drop(guard);
    }

    #[tokio::test]
    async fn acquire_lock_returns_none_when_recently_held() {
        let store = Arc::new(MockStore::new());
        let now = OffsetDateTime::now_utc();
        store.insert_with("k", Bytes::new(), now, PutOpts::default());
        let guard = acquire_lock(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            "k",
            Duration::seconds(60),
            now,
        )
        .await
        .unwrap();
        assert!(guard.is_none(), "expected contention");
    }

    #[tokio::test]
    async fn acquire_lock_recovers_stale_lock() {
        let store = Arc::new(MockStore::new());
        let now = OffsetDateTime::now_utc();
        let stale = now - Duration::seconds(120);
        store.insert_with("k", Bytes::new(), stale, PutOpts::default());
        let guard = acquire_lock(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            "k",
            Duration::seconds(60),
            now,
        )
        .await
        .unwrap();
        assert!(guard.is_some(), "stale lock must be recoverable");
        // Lock still exists (we re-created it with put_if_absent).
        assert!(store.contains("k"));
        drop(guard);
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
        let arc = Arc::new(store);
        let now = OffsetDateTime::now_utc();
        let guard = acquire_lock(
            Arc::clone(&arc) as Arc<dyn ObjectStore>,
            "k",
            Duration::seconds(60),
            now,
        )
        .await
        .unwrap();
        assert!(guard.is_none(), "expected contention on disappeared lock");
        // Confirm head() was actually called — a regression that skipped
        // the staleness branch and returned None directly would also
        // satisfy the assertion above. The fault firing proves head ran.
        assert_eq!(arc.pending_faults(), 0);
    }

    #[tokio::test]
    async fn release_lock_deletes_existing_key() {
        let store = Arc::new(MockStore::new());
        let now = OffsetDateTime::now_utc();
        let guard = acquire_lock(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            "k",
            Duration::seconds(60),
            now,
        )
        .await
        .unwrap()
        .expect("acquire_lock must succeed on an empty store");
        release_lock(guard).await.unwrap();
        assert!(!store.contains("k"));
    }

    #[tokio::test]
    async fn release_lock_swallows_not_found_when_lock_already_gone() {
        // Acquire a lock, then delete the key out-of-band before
        // release_lock runs. The release must map NotFound → Ok(()).
        let store = Arc::new(MockStore::new());
        let now = OffsetDateTime::now_utc();
        let guard = acquire_lock(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            "k",
            Duration::seconds(60),
            now,
        )
        .await
        .unwrap()
        .expect("acquire_lock must succeed");
        // Cancel the heartbeat first so it cannot race the manual
        // delete and re-create the key.
        guard.heartbeat.as_ref().unwrap().abort();
        // Give the abort a chance to take effect, then remove the key.
        tokio::task::yield_now().await;
        let _ = store.delete("k").await;
        release_lock(guard).await.unwrap();
    }

    #[tokio::test]
    async fn release_lock_propagates_non_not_found_errors() {
        use crate::object_store::mock::Fault;
        let store = Arc::new(MockStore::new());
        let now = OffsetDateTime::now_utc();
        let guard = acquire_lock(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            "k",
            Duration::seconds(60),
            now,
        )
        .await
        .unwrap()
        .expect("acquire_lock must succeed");
        store.arm(Fault::NetworkOnDelete { key: "k".into() });
        let err = release_lock(guard).await.unwrap_err();
        assert!(
            matches!(err, ObjectStoreError::Network(_)),
            "expected Network error, got {err:?}",
        );
        // The fault fired exactly once.
        assert_eq!(store.pending_faults(), 0);
        // Key remains because the delete was faulted, not executed.
        assert!(store.contains("k"));
    }

    /// Issue #118: a long-running critical section must not lose its
    /// lock. The heartbeat refreshes `last_modified` faster than the
    /// TTL expires, so a concurrent acquire after the original TTL
    /// elapses still sees a live lock and returns `None` (contention).
    #[tokio::test(start_paused = true)]
    async fn heartbeat_keeps_lock_alive_past_ttl() {
        let store = Arc::new(MockStore::new());
        let now = OffsetDateTime::now_utc();
        // TTL is 4 s → heartbeat fires every 2 s (see
        // `heartbeat_interval`). Run the test for ~10 s of virtual
        // time so a regression that disabled the heartbeat would have
        // multiple TTLs to expire under.
        let ttl = Duration::seconds(4);
        let guard = acquire_lock(Arc::clone(&store) as Arc<dyn ObjectStore>, "k", ttl, now)
            .await
            .unwrap()
            .expect("acquire must succeed");

        // Advance the clock past several TTLs, letting the heartbeat
        // task fire each time.
        for _ in 0..5 {
            tokio::time::advance(std::time::Duration::from_secs(3)).await;
            // Yield so the spawned heartbeat task can take its turn
            // on the runtime and PUT the lock key.
            tokio::task::yield_now().await;
        }

        // A concurrent acquire would see `last_modified` recent
        // (heartbeat just refreshed it), so it should report
        // contention — not steal the lock as stale. We use a `now`
        // far in the future (matches the wall-clock view a second
        // process would have) but rely on the heartbeat having
        // overwritten `last_modified` to the runtime "now".
        let future = OffsetDateTime::now_utc();
        let other = acquire_lock(Arc::clone(&store) as Arc<dyn ObjectStore>, "k", ttl, future)
            .await
            .unwrap();
        assert!(
            other.is_none(),
            "live lock must not be stealable while the holder's heartbeat runs",
        );

        release_lock(guard).await.unwrap();
        assert!(!store.contains("k"));
    }

    /// Releasing the guard must stop the heartbeat so no further PUTs
    /// hit the lock key after release. We assert by deleting the key
    /// post-release and confirming it stays gone.
    #[tokio::test(start_paused = true)]
    async fn release_lock_stops_heartbeat() {
        let store = Arc::new(MockStore::new());
        let now = OffsetDateTime::now_utc();
        let ttl = Duration::seconds(4);
        let guard = acquire_lock(Arc::clone(&store) as Arc<dyn ObjectStore>, "k", ttl, now)
            .await
            .unwrap()
            .expect("acquire must succeed");
        release_lock(guard).await.unwrap();
        assert!(!store.contains("k"));

        // Advance well past multiple heartbeat intervals. A
        // regression that forgot to abort the task would re-create
        // the key via put_bytes.
        for _ in 0..5 {
            tokio::time::advance(std::time::Duration::from_secs(3)).await;
            tokio::task::yield_now().await;
        }
        assert!(
            !store.contains("k"),
            "heartbeat must not re-create the key after release",
        );
    }

    /// Dropping the guard without calling `release_lock` aborts the
    /// heartbeat (so the lock becomes stealable after TTL) and leaves
    /// the lock key in place (a future caller's stale-recovery path
    /// reclaims it).
    #[tokio::test(start_paused = true)]
    async fn lock_guard_drop_aborts_heartbeat() {
        let store = Arc::new(MockStore::new());
        let now = OffsetDateTime::now_utc();
        let ttl = Duration::seconds(4);
        let guard = acquire_lock(Arc::clone(&store) as Arc<dyn ObjectStore>, "k", ttl, now)
            .await
            .unwrap()
            .expect("acquire must succeed");
        drop(guard);

        // Capture the lock's last_modified right after drop —
        // heartbeats after this point would advance it. `head` is the
        // trait-level path to last_modified and avoids a test-only
        // accessor on MockStore.
        let after_drop = store.head("k").await.expect("lock present").last_modified;

        // Advance time past multiple heartbeat intervals.
        for _ in 0..5 {
            tokio::time::advance(std::time::Duration::from_secs(3)).await;
            tokio::task::yield_now().await;
        }

        let after_advance = store
            .head("k")
            .await
            .expect("lock still present")
            .last_modified;
        assert_eq!(
            after_drop, after_advance,
            "heartbeat must not refresh last_modified after drop",
        );

        // And the lock is now stealable via the stale path: an acquire
        // with a `now` past TTL deletes the orphaned lock and reclaims.
        let future = now + Duration::seconds(120);
        let recovered = acquire_lock(Arc::clone(&store) as Arc<dyn ObjectStore>, "k", ttl, future)
            .await
            .unwrap();
        assert!(recovered.is_some(), "orphaned lock must be reclaimable");
        drop(recovered);
    }

    /// Issue #150 regression: a heartbeat `put_bytes` already in
    /// flight when `release` is called must complete BEFORE the
    /// release issues its DELETE. The pre-fix code did
    /// `handle.abort(); delete`, which only cancelled the future at
    /// its next await point — an in-flight network PUT continued on
    /// the server and could settle AFTER the DELETE, resurrecting the
    /// lock key as an orphan.
    ///
    /// The test wraps the mock store in a barrier-gated `put_bytes`
    /// (the first PUT records its start, then waits on a notify) so
    /// the heartbeat tick lands in a state where the PUT has been
    /// issued but not yet completed when `release` runs. The
    /// post-condition is the sequence of recorded operations: the
    /// in-flight PUT's completion must precede the DELETE's start.
    /// Expected sequence is derived from the invariant in the
    /// `LockGuard::release` doc comment (the spec), not from the
    /// code's current output (lesson #5).
    // The test body inlines a small `ObjectStore` decorator
    // (`GatedPutStore`) so the trait wiring inflates the line count
    // past clippy's default budget. Splitting the decorator into a
    // sibling helper would obscure the test's single behaviour
    // contract, so we accept the lint locally.
    #[allow(clippy::too_many_lines)]
    #[tokio::test(start_paused = true)]
    async fn release_awaits_in_flight_heartbeat_put_before_delete() {
        use std::sync::Mutex;
        use tokio::sync::Notify;

        /// Op-log entry recording the relative order of `put_bytes`
        /// and `delete` events. The release contract requires
        /// `PutEnd` to precede `DeleteStart`.
        #[derive(Debug, PartialEq, Eq, Clone, Copy)]
        enum Op {
            PutStart,
            PutEnd,
            DeleteStart,
            DeleteEnd,
        }

        /// Decorator that gates the FIRST `put_bytes` on a notify
        /// barrier. Subsequent `put_bytes` calls pass through (they
        /// should not happen — once we hold the gate, release should
        /// stop the heartbeat before another tick fires).
        struct GatedPutStore {
            inner: Arc<MockStore>,
            put_gate: Arc<Notify>,
            log: Arc<Mutex<Vec<Op>>>,
            gated_key: String,
            gate_consumed: std::sync::atomic::AtomicBool,
        }

        #[async_trait::async_trait]
        impl ObjectStore for GatedPutStore {
            async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, ObjectStoreError> {
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
                let is_gated = key == self.gated_key
                    && !self
                        .gate_consumed
                        .swap(true, std::sync::atomic::Ordering::SeqCst);
                if is_gated {
                    self.log.lock().unwrap().push(Op::PutStart);
                    self.put_gate.notified().await;
                }
                let result = self.inner.put_bytes(key, body, opts).await;
                if is_gated {
                    self.log.lock().unwrap().push(Op::PutEnd);
                }
                result
            }
            async fn put_path(
                &self,
                key: &str,
                src: &std::path::Path,
                opts: PutOpts,
            ) -> Result<(), ObjectStoreError> {
                self.inner.put_path(key, src, opts).await
            }
            async fn put_if_absent(
                &self,
                key: &str,
                body: Bytes,
            ) -> Result<bool, ObjectStoreError> {
                self.inner.put_if_absent(key, body).await
            }
            async fn head(&self, key: &str) -> Result<ObjectMeta, ObjectStoreError> {
                self.inner.head(key).await
            }
            async fn copy(&self, src: &str, dst: &str) -> Result<(), ObjectStoreError> {
                self.inner.copy(src, dst).await
            }
            async fn delete(&self, key: &str) -> Result<(), ObjectStoreError> {
                self.log.lock().unwrap().push(Op::DeleteStart);
                let result = self.inner.delete(key).await;
                self.log.lock().unwrap().push(Op::DeleteEnd);
                result
            }
        }

        let inner = Arc::new(MockStore::new());
        let put_gate = Arc::new(Notify::new());
        let log = Arc::new(Mutex::new(Vec::<Op>::new()));
        let store = Arc::new(GatedPutStore {
            inner: Arc::clone(&inner),
            put_gate: Arc::clone(&put_gate),
            log: Arc::clone(&log),
            gated_key: "k".to_owned(),
            gate_consumed: std::sync::atomic::AtomicBool::new(false),
        });

        let now = OffsetDateTime::now_utc();
        let ttl = Duration::seconds(4);
        let guard = acquire_lock(Arc::clone(&store) as Arc<dyn ObjectStore>, "k", ttl, now)
            .await
            .unwrap()
            .expect("acquire must succeed");

        // Yield so the heartbeat task is polled and consumes its
        // immediate (zero-duration) first `tick.tick()`. Without this
        // yield, the next `advance` would fire the immediate tick
        // first and then the periodic tick, but the task still
        // wouldn't be polled until the test yields again — we want
        // to be inside the loop body before advancing time.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        // Drive one heartbeat tick. The heartbeat task issues
        // `put_bytes`, which the gate intercepts and parks before the
        // inner write completes. The interval is `ttl/3 = 1s` so any
        // advance >= 1s is sufficient.
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        // Yield repeatedly so the spawned heartbeat task is polled
        // and reaches the gate.
        for _ in 0..16 {
            tokio::task::yield_now().await;
            if !log.lock().unwrap().is_empty() {
                break;
            }
        }
        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[Op::PutStart],
            "heartbeat PUT must be in flight before release fires",
        );

        // Begin release on a separate task so we can observe the
        // ordering: release blocks on `stop_heartbeat`'s join-await,
        // which can only complete after the gated PUT finishes.
        let release_store = Arc::clone(&store);
        let release_handle = tokio::spawn(async move {
            let _ = release_store; // borrow check: keep store alive
            release_lock(guard).await
        });

        // Let the release task get as far as it can — up to the
        // join-await on the heartbeat task.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        // No DELETE may have fired yet: the heartbeat task is still
        // inside the gated `put_bytes`.
        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[Op::PutStart],
            "release must NOT issue DELETE while heartbeat PUT is in flight",
        );

        // Open the gate: the in-flight PUT completes, the heartbeat
        // task exits the loop on its next shutdown re-check, and
        // release proceeds to DELETE.
        put_gate.notify_one();
        let result = release_handle.await.expect("release task panicked");
        result.expect("release_lock");

        let final_log = log.lock().unwrap().clone();
        assert_eq!(
            final_log,
            vec![Op::PutStart, Op::PutEnd, Op::DeleteStart, Op::DeleteEnd],
            "operation order must be: heartbeat PUT completes, THEN release DELETE",
        );
        assert!(
            !inner.contains("k"),
            "lock key must be deleted after release"
        );
    }

    // --- delete_remote_ref_under_lock ---------------------------------

    #[tokio::test]
    async fn delete_remote_ref_removes_single_bundle() {
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        store.insert(
            format!("repo/refs/heads/main/{SHA}.bundle"),
            Bytes::from_static(b"b"),
        );
        let outcome = delete_remote_ref_under_lock(
            &store,
            Some("repo"),
            &r,
            false,
            "repo/refs/heads/main/LOCK#.lock",
        )
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
        let outcome = delete_remote_ref_under_lock(
            &store,
            Some("repo"),
            &r,
            false,
            "repo/refs/heads/main/LOCK#.lock",
        )
        .await
        .unwrap();
        match outcome {
            PushOutcome::Error { message, .. } => {
                assert_eq!(message, r#""not found"?"#);
            }
            PushOutcome::Ok { .. } => panic!("expected Error outcome"),
        }
    }

    #[tokio::test]
    async fn delete_remote_ref_rejects_protected_marker() {
        // PROTECTED# is unfiltered for the delete-path count, but we
        // detect the marker before the generic multi-bundle error and
        // emit a protection-specific refusal that names `unprotect`.
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        let bundle = format!("repo/refs/heads/main/{SHA}.bundle");
        let protected = "repo/refs/heads/main/PROTECTED#";
        store.insert(&bundle, Bytes::from_static(b"b"));
        store.insert(protected, Bytes::from_static(b""));
        let outcome = delete_remote_ref_under_lock(
            &store,
            Some("repo"),
            &r,
            false,
            "repo/refs/heads/main/LOCK#.lock",
        )
        .await
        .unwrap();
        match outcome {
            PushOutcome::Error { message, .. } => {
                assert_eq!(
                    message,
                    r#""ref is protected. Run git-remote-object-store unprotect <url> <branch> to remove protection before deleting."?"#,
                );
            }
            PushOutcome::Ok { .. } => panic!("expected Error outcome"),
        }
        // Both keys must remain — a regression that deleted on the way
        // to the error branch would still satisfy the message check.
        assert!(store.contains(&bundle));
        assert!(store.contains(protected));
    }

    #[tokio::test]
    async fn delete_remote_ref_reports_corruption_without_protected_marker() {
        // Two bundles, no PROTECTED# marker → genuine corruption case
        // still falls through to the doctor message.
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        let bundle_a = format!("repo/refs/heads/main/{SHA}.bundle");
        let bundle_b = format!("repo/refs/heads/main/{OTHER_SHA}.bundle");
        store.insert(&bundle_a, Bytes::from_static(b"a"));
        store.insert(&bundle_b, Bytes::from_static(b"b"));
        let outcome = delete_remote_ref_under_lock(
            &store,
            Some("repo"),
            &r,
            false,
            "repo/refs/heads/main/LOCK#.lock",
        )
        .await
        .unwrap();
        match outcome {
            PushOutcome::Error { message, .. } => {
                assert_eq!(
                    message,
                    r#""multiple bundles exist on server. Run git-remote-object-store doctor to fix."?"#,
                );
            }
            PushOutcome::Ok { .. } => panic!("expected Error outcome"),
        }
        assert!(store.contains(&bundle_a));
        assert!(store.contains(&bundle_b));
    }

    /// Issue #128: the canonical PROTECTED# guard must reject even when
    /// the count happens to match `expected`. Pre-#128 this listing
    /// `[bundle.bundle, PROTECTED#]` matched `expected = 2` in zip mode
    /// and was swept silently. Pin the guard: both keys survive, and
    /// the wire error is the protection-specific message — not the
    /// generic doctor message.
    #[tokio::test]
    async fn delete_remote_ref_rejects_protected_marker_when_count_matches_zip() {
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        let bundle = format!("repo/refs/heads/main/{SHA}.bundle");
        let protected = "repo/refs/heads/main/PROTECTED#";
        store.insert(&bundle, Bytes::from_static(b"b"));
        store.insert(protected, Bytes::from_static(b""));
        // zip = true → expected = 2, and the listing has exactly 2
        // entries (bundle + marker). Pre-#128 this fell through to the
        // count-match deletion branch.
        let outcome = delete_remote_ref_under_lock(
            &store,
            Some("repo"),
            &r,
            true,
            "repo/refs/heads/main/LOCK#.lock",
        )
        .await
        .unwrap();
        match outcome {
            PushOutcome::Error { message, .. } => {
                assert_eq!(
                    message,
                    r#""ref is protected. Run git-remote-object-store unprotect <url> <branch> to remove protection before deleting."?"#,
                );
            }
            PushOutcome::Ok { .. } => panic!("expected protection-refusal Error"),
        }
        assert!(store.contains(&bundle), "bundle must survive");
        assert!(store.contains(protected), "marker must survive");
    }

    /// Issue #128: the protection guard must also fire when the lone
    /// remaining key under the ref prefix is the PROTECTED# marker
    /// itself. Pre-#128, `entries == [PROTECTED#]` matched `expected = 1`
    /// in non-zip mode and the marker was swept — the next push to the
    /// ref would then succeed against an unprotected branch even though
    /// the operator never ran `unprotect`. Pin the guard: the marker
    /// survives and the wire error is the protection-specific message.
    #[tokio::test]
    async fn delete_remote_ref_rejects_protected_marker_when_only_marker_present() {
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        let protected = "repo/refs/heads/main/PROTECTED#";
        store.insert(protected, Bytes::from_static(b""));
        let outcome = delete_remote_ref_under_lock(
            &store,
            Some("repo"),
            &r,
            false,
            "repo/refs/heads/main/LOCK#.lock",
        )
        .await
        .unwrap();
        match outcome {
            PushOutcome::Error { message, .. } => {
                assert_eq!(
                    message,
                    r#""ref is protected. Run git-remote-object-store unprotect <url> <branch> to remove protection before deleting."?"#,
                );
            }
            PushOutcome::Ok { .. } => panic!("expected protection-refusal Error"),
        }
        assert!(store.contains(protected), "marker must survive");
    }

    /// Issue #128: simulate the TOCTOU sequence the bug describes.
    /// Client A starts a delete; between `acquire_lock` and the
    /// under-lock listing, client B writes a PROTECTED# marker
    /// (`protect` takes no per-ref lock — it's a single `put_bytes`).
    /// The fresh under-lock listing reflects the marker, so the
    /// canonical guard rejects the delete. Both the bundle and the
    /// marker survive.
    #[tokio::test]
    async fn delete_remote_ref_rejects_protect_landed_between_acquire_and_list() {
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        let bundle = format!("repo/refs/heads/main/{SHA}.bundle");
        let lock_key = "repo/refs/heads/main/LOCK#.lock";
        let protected = "repo/refs/heads/main/PROTECTED#";
        // Client A acquired the lock; bundle was already present.
        store.insert(&bundle, Bytes::from_static(b"b"));
        store.insert(lock_key, Bytes::from_static(b"held-lock-payload"));
        // Client B's concurrent `protect` lands AFTER our lock-acquire
        // but BEFORE our under-lock listing — exactly the window the
        // pre-#128 ordering left open.
        store.insert(protected, Bytes::from_static(b""));

        let outcome = delete_remote_ref_under_lock(&store, Some("repo"), &r, false, lock_key)
            .await
            .unwrap();

        match outcome {
            PushOutcome::Error { message, .. } => {
                assert_eq!(
                    message,
                    r#""ref is protected. Run git-remote-object-store unprotect <url> <branch> to remove protection before deleting."?"#,
                );
            }
            PushOutcome::Ok { .. } => panic!("expected protection-refusal Error"),
        }
        assert!(store.contains(&bundle), "bundle must survive");
        assert!(store.contains(protected), "marker must survive");
        assert!(store.contains(lock_key), "held lock must survive");
    }

    #[tokio::test]
    async fn delete_remote_ref_zip_mode_expects_two_keys() {
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        let bundle = format!("repo/refs/heads/main/{SHA}.bundle");
        let zip = "repo/refs/heads/main/repo.zip";
        store.insert(&bundle, Bytes::from_static(b"b"));
        store.insert(zip, Bytes::from_static(b""));
        let outcome = delete_remote_ref_under_lock(
            &store,
            Some("repo"),
            &r,
            true,
            "repo/refs/heads/main/LOCK#.lock",
        )
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
        .to_protocol_line();
        assert_eq!(line, "ok refs/heads/main\n");
    }

    #[test]
    fn push_outcome_renders_error_line() {
        let line = PushOutcome::Error {
            remote_ref: "refs/heads/main".into(),
            message: r#""bad"?"#.into(),
        }
        .to_protocol_line();
        assert_eq!(line, "error refs/heads/main \"bad\"?\n");
    }

    /// Both duplicate-bundle paths (pre-lock at ~line 482 and under-lock
    /// at ~line 600) must produce wire output ending in `"?\n`. The `?`
    /// suffix is the project-wide Rust convention for `error <ref> "..."`
    /// messages — git treats `"..."?` as recoverable and `"..."` as
    /// fatal. Both branches normalize to the recoverable form.
    #[test]
    fn duplicate_bundle_errors_use_consistent_wire_format() {
        let pre_lock_line = PushOutcome::Error {
            remote_ref: "refs/heads/main".into(),
            message:
                r#""multiple bundles exist on server. Run git-remote-object-store doctor to fix."?"#
                    .to_owned(),
        }
        .to_protocol_line();
        let under_lock_line = PushOutcome::Error {
            remote_ref: "refs/heads/main".into(),
            message: r#""multiple bundles exist for the same ref on server. Run git-remote-object-store doctor to fix."?"#.to_owned(),
        }
        .to_protocol_line();

        assert_eq!(
            pre_lock_line,
            "error refs/heads/main \"multiple bundles exist on server. \
             Run git-remote-object-store doctor to fix.\"?\n",
        );
        assert_eq!(
            under_lock_line,
            "error refs/heads/main \"multiple bundles exist for the same ref on server. \
             Run git-remote-object-store doctor to fix.\"?\n",
        );
        assert!(pre_lock_line.ends_with("\"?\n"));
        assert!(under_lock_line.ends_with("\"?\n"));
    }

    // --- lock_ttl_from_env --------------------------------------------

    #[test]
    fn lock_ttl_env_override_falls_back_for_unset_invalid_or_zero() {
        // Group all env-var cases in one test fn so they run sequentially
        // — the var is process-global and mutating it from multiple
        // parallel tests would race.
        let default_ttl = Duration::seconds(i64::try_from(DEFAULT_LOCK_TTL_SECONDS).unwrap());
        // Unset returns default.
        unsafe {
            env::remove_var(ENV_LOCK_TTL_SECONDS);
        }
        assert_eq!(lock_ttl_from_env(), default_ttl);
        // Non-numeric falls back.
        unsafe {
            env::set_var(ENV_LOCK_TTL_SECONDS, "not-a-number");
        }
        assert_eq!(lock_ttl_from_env(), default_ttl);
        // Zero falls back (would defeat per-ref locking).
        unsafe {
            env::set_var(ENV_LOCK_TTL_SECONDS, "0");
        }
        assert_eq!(lock_ttl_from_env(), default_ttl);
        // Positive integer wins.
        unsafe {
            env::set_var(ENV_LOCK_TTL_SECONDS, "120");
        }
        assert_eq!(lock_ttl_from_env(), Duration::seconds(120));
        unsafe {
            env::remove_var(ENV_LOCK_TTL_SECONDS);
        }
    }

    // --- FORMAT key write via perform_push_under_lock --------------------

    /// Helper: run `perform_push_under_lock` against a temporary bundle file
    /// so we can assert on the resulting store state.
    async fn push_under_lock_with_bundle(
        store: &MockStore,
        prefix: Option<&str>,
        engine: StorageEngine,
    ) -> PushOutcome {
        let r = rn("refs/heads/main");
        let temp_dir = tempfile::Builder::new()
            .prefix("test_push_")
            .tempdir()
            .unwrap();
        let bundle_path = temp_dir.path().join("bundle");
        std::fs::write(&bundle_path, b"fake bundle").unwrap();

        let state = PushReadyState {
            remote_ref: r,
            local_sha: Sha::from_hex(SHA).unwrap(),
            pre_existing: None,
            bundle_path,
            zip_artifacts: None,
            engine,
            force: false,
            pre_existing_was_ancestor: true,
            local_spec: "refs/heads/main".to_owned(),
            _temp_dir: temp_dir,
        };

        perform_push_under_lock(store, prefix, BackendKind::S3, state)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn perform_push_under_lock_writes_format_key_on_first_push() {
        let store = MockStore::new();
        let outcome =
            push_under_lock_with_bundle(&store, Some("repo"), StorageEngine::Bundle).await;
        assert!(
            matches!(outcome, PushOutcome::Ok { .. }),
            "expected Ok outcome"
        );
        assert!(
            store.contains("repo/FORMAT"),
            "FORMAT key must be written on the first push",
        );
        let content = store.get_bytes("repo/FORMAT").await.unwrap();
        assert_eq!(content.as_ref(), b"bundle");
    }

    #[tokio::test]
    async fn perform_push_under_lock_writes_format_key_without_prefix() {
        let store = MockStore::new();
        let outcome = push_under_lock_with_bundle(&store, None, StorageEngine::Bundle).await;
        assert!(
            matches!(outcome, PushOutcome::Ok { .. }),
            "expected Ok outcome"
        );
        assert!(
            store.contains("FORMAT"),
            "FORMAT key must be written at root when no prefix",
        );
        let content = store.get_bytes("FORMAT").await.unwrap();
        assert_eq!(content.as_ref(), b"bundle");
    }

    #[tokio::test]
    async fn perform_push_under_lock_format_key_is_idempotent() {
        // If FORMAT already exists (second push), put_if_absent is a no-op.
        // Pre-insert with a trailing newline so the original bytes differ from
        // what the push would write — a plain `put` would overwrite to
        // b"bundle", while put_if_absent must preserve b"bundle\n".
        let store = MockStore::new();
        store.insert("repo/FORMAT", Bytes::from_static(b"bundle\n"));
        let outcome =
            push_under_lock_with_bundle(&store, Some("repo"), StorageEngine::Bundle).await;
        assert!(
            matches!(outcome, PushOutcome::Ok { .. }),
            "expected Ok outcome"
        );
        // Original content preserved — put_if_absent did not overwrite.
        let content = store.get_bytes("repo/FORMAT").await.unwrap();
        assert_eq!(content.as_ref(), b"bundle\n");
    }

    // --- stale-remote guard -------------------------------------------

    /// Build a `PushReadyState` with a specific `pre_existing` key and a
    /// matching `bundle_path` on disk. The store is left in whatever state
    /// the caller configured (e.g. with a pre-seeded bundle key); the
    /// caller already knows which prefix it seeded under, so this helper
    /// does not need to take it.
    fn push_state_with_pre_existing(pre_existing: Option<String>) -> PushReadyState {
        let r = rn("refs/heads/main");
        let temp_dir = tempfile::Builder::new()
            .prefix("test_push_")
            .tempdir()
            .unwrap();
        let bundle_path = temp_dir.path().join("bundle");
        std::fs::write(&bundle_path, b"fake bundle").unwrap();
        PushReadyState {
            remote_ref: r,
            local_sha: Sha::from_hex(SHA).unwrap(),
            pre_existing,
            bundle_path,
            zip_artifacts: None,
            engine: StorageEngine::Bundle,
            force: false,
            pre_existing_was_ancestor: true,
            local_spec: "refs/heads/main".to_owned(),
            _temp_dir: temp_dir,
        }
    }

    /// `pre_existing=None`, `current_key=Some(...)`: a concurrent push
    /// created a bundle after our pre-lock list but before we acquired
    /// the lock.
    ///
    /// The seeded `existing_key` deliberately uses a SHA distinct from
    /// the local push's `local_sha` (= `SHA`). If they matched, a
    /// regression that compared `current_key` against `bundle_dest`
    /// (the key derived from `local_sha`) instead of against
    /// `pre_existing` could pass for the wrong reason — the keys
    /// happen to align. With distinct SHAs the only way the test
    /// passes is the correct comparison: `pre_existing(None) !=
    /// current_key(Some)`.
    #[tokio::test]
    async fn perform_push_under_lock_rejects_none_to_some_stale_remote() {
        let store = MockStore::new();
        let existing_key = format!("repo/refs/heads/main/{OTHER_SHA}.bundle");
        store.insert(&existing_key, Bytes::from_static(b"old bundle"));
        let state = push_state_with_pre_existing(None);
        let outcome = perform_push_under_lock(&store, Some("repo"), BackendKind::S3, state)
            .await
            .unwrap();
        assert!(
            matches!(
                &outcome,
                PushOutcome::Error { message, .. }
                    if message == r#""stale remote. Please fetch and retry."?"#
            ),
            "expected stale-remote error, got {outcome:?}",
        );
    }

    /// `pre_existing=Some(key)`, `current_key=None`: the bundle was
    /// deleted between our pre-lock list and the lock acquisition (e.g.
    /// a concurrent `git push :<ref>` delete).
    #[tokio::test]
    async fn perform_push_under_lock_rejects_some_to_none_stale_remote() {
        let store = MockStore::new();
        let old_key = format!("repo/refs/heads/main/{SHA}.bundle");
        let state = push_state_with_pre_existing(Some(old_key.clone()));
        // Store is empty — the previously-seen bundle is gone.
        let outcome = perform_push_under_lock(&store, Some("repo"), BackendKind::S3, state)
            .await
            .unwrap();
        assert!(
            matches!(
                &outcome,
                PushOutcome::Error { message, .. }
                    if message == r#""stale remote. Please fetch and retry."?"#
            ),
            "expected stale-remote error, got {outcome:?}",
        );
    }

    /// `pre_existing=Some(key_a)`, `current_key=Some(key_b)` where
    /// `key_a != key_b`: a concurrent push replaced our bundle.
    #[tokio::test]
    async fn perform_push_under_lock_rejects_replaced_bundle_stale_remote() {
        let store = MockStore::new();
        let old_sha = SHA;
        let new_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let old_key = format!("repo/refs/heads/main/{old_sha}.bundle");
        let new_key = format!("repo/refs/heads/main/{new_sha}.bundle");
        // Under-lock the store shows the *new* key.
        store.insert(&new_key, Bytes::from_static(b"new bundle"));
        let state = push_state_with_pre_existing(Some(old_key));
        let outcome = perform_push_under_lock(&store, Some("repo"), BackendKind::S3, state)
            .await
            .unwrap();
        assert!(
            matches!(
                &outcome,
                PushOutcome::Error { message, .. }
                    if message == r#""stale remote. Please fetch and retry."?"#
            ),
            "expected stale-remote error, got {outcome:?}",
        );
    }

    /// Under-lock re-listing sees two bundles for the same ref (two clients
    /// raced and both uploaded before either acquired the lock). Must return
    /// the under-lock duplicate-bundle error before reaching the stale-remote
    /// guard or the upload.
    #[tokio::test]
    async fn perform_push_under_lock_rejects_two_bundles_seen_under_lock() {
        let store = MockStore::new();
        let sha_a = "1111111111111111111111111111111111111111";
        let sha_b = "2222222222222222222222222222222222222222";
        store.insert(
            format!("repo/refs/heads/main/{sha_a}.bundle"),
            Bytes::from_static(b"bundle_a"),
        );
        store.insert(
            format!("repo/refs/heads/main/{sha_b}.bundle"),
            Bytes::from_static(b"bundle_b"),
        );
        // Pre-lock snapshot saw zero bundles; under-lock sees two.
        let state = push_state_with_pre_existing(None);
        let outcome = perform_push_under_lock(&store, Some("repo"), BackendKind::S3, state)
            .await
            .unwrap();
        assert!(
            matches!(
                &outcome,
                PushOutcome::Error { message, .. }
                    if message == r#""multiple bundles exist for the same ref on server. Run git-remote-object-store doctor to fix."?"#
            ),
            "expected under-lock multi-bundle error, got {outcome:?}",
        );
        // Neither bundle was overwritten or deleted.
        assert!(store.contains(&format!("repo/refs/heads/main/{sha_a}.bundle")));
        assert!(store.contains(&format!("repo/refs/heads/main/{sha_b}.bundle")));
    }

    /// Stale-remote happy path: `pre_existing == current_key`. The four
    /// drift scenarios above all assert that mismatch produces an error.
    /// This case asserts that the *match* path goes through to a normal
    /// `Ok(remote_ref)` push outcome — without it, a regression that
    /// flipped the comparison to always-mismatch would pass every drift
    /// test and silently break every real push.
    ///
    /// `pre_existing` is seeded under SHA distinct from `local_sha`
    /// (mirroring the `rejects_none_to_some` hardening): so a buggy
    /// regression that compared `current_key` against `bundle_dest`
    /// (the key derived from `local_sha`) instead of against
    /// `pre_existing` cannot pass for the wrong reason — the keys
    /// are deliberately different. The push must:
    ///   1. produce `Ok(remote_ref)`
    ///   2. upload the new bundle at `bundle_dest` (SHA = `local_sha`)
    ///   3. delete the old bundle (SHA = `OTHER_SHA`) via the
    ///      post-upload cleanup branch
    ///   4. write `HEAD` and `FORMAT`
    #[tokio::test]
    async fn perform_push_under_lock_passes_through_when_pre_existing_matches_current() {
        let store = MockStore::new();
        let pre_key = format!("repo/refs/heads/main/{OTHER_SHA}.bundle");
        // Seed under `OTHER_SHA` so under-lock list returns this key.
        // The local push's `local_sha` is `SHA`, so `bundle_dest` is a
        // DIFFERENT key. The stale-remote check passes (pre_existing ==
        // current_key, both = pre_key); the function then uploads the
        // new bundle at `bundle_dest` and deletes the old one.
        store.insert(&pre_key, Bytes::from_static(b"old bundle"));
        let state = push_state_with_pre_existing(Some(pre_key.clone()));
        let outcome = perform_push_under_lock(&store, Some("repo"), BackendKind::S3, state)
            .await
            .unwrap();
        assert!(
            matches!(&outcome, PushOutcome::Ok { remote_ref } if remote_ref == "refs/heads/main"),
            "expected Ok(refs/heads/main), got {outcome:?}",
        );
        // The new bundle must land at the bundle_dest derived from
        // local_sha, not at pre_key.
        let bundle_dest = format!("repo/refs/heads/main/{SHA}.bundle");
        let new_bytes = store
            .get_bytes(&bundle_dest)
            .await
            .expect("new bundle must be uploaded at bundle_dest");
        assert_eq!(
            new_bytes.as_ref(),
            b"fake bundle",
            "new bundle must contain the local payload",
        );
        // The old bundle must be deleted by the post-upload cleanup
        // branch (`if prev != bundle_dest { delete_idempotent }`).
        assert!(
            !store.contains(&pre_key),
            "old bundle at pre_key must be deleted",
        );
        assert!(
            store.contains("repo/FORMAT"),
            "FORMAT key must be written by the push",
        );
        assert!(
            store.contains("repo/HEAD"),
            "HEAD key must be written by the push",
        );
    }

    /// Issue #121 regression: a non-NotFound delete error on the prior
    /// bundle after the new bundle has been put must NOT fail the
    /// push. The new bundle is already durable; reporting failure
    /// misrepresents the remote state. Match the `compact` /
    /// `force_push_baseline_cleanup` best-effort contract: log at warn
    /// and report success. The two-bundle state remains on the bucket
    /// so the next push's under-lock multi-bundle guard surfaces it
    /// to the operator.
    #[tokio::test]
    async fn perform_push_under_lock_succeeds_when_prior_delete_fails() {
        use crate::object_store::mock::Fault;
        let store = MockStore::new();
        let pre_key = format!("repo/refs/heads/main/{OTHER_SHA}.bundle");
        store.insert(&pre_key, Bytes::from_static(b"old bundle"));
        store.arm(Fault::NetworkOnDelete {
            key: pre_key.clone(),
        });

        let state = push_state_with_pre_existing(Some(pre_key.clone()));
        let outcome = perform_push_under_lock(&store, Some("repo"), BackendKind::S3, state)
            .await
            .expect("push must succeed even when prior-bundle delete fails");
        assert!(
            matches!(&outcome, PushOutcome::Ok { remote_ref } if remote_ref == "refs/heads/main"),
            "expected Ok(refs/heads/main), got {outcome:?}",
        );

        let bundle_dest = format!("repo/refs/heads/main/{SHA}.bundle");
        assert!(
            store.contains(&bundle_dest),
            "new bundle must be uploaded at bundle_dest",
        );
        // The fault armed against the delete fired exactly once.
        assert_eq!(store.pending_faults(), 0);
        // Orphan remains on the bucket — operator sees the warn log and
        // the next push's multi-bundle guard will direct them to doctor.
        assert!(
            store.contains(&pre_key),
            "delete fault must leave the prior bundle in place",
        );
    }

    /// Issue #127 regression: a non-`NotFound` error on the optional
    /// zip artifact upload after the new bundle has been put must NOT
    /// fail the push. The bundle, `HEAD`, and `FORMAT` are already
    /// durable; reporting failure misrepresents the remote state. The
    /// zip is a CodePipeline-side convenience surface — bundle
    /// availability is what determines whether `git clone`/`fetch`
    /// work. Match the `delete_prior_bundle_best_effort` contract: log
    /// at warn and report success.
    #[tokio::test]
    async fn perform_push_under_lock_succeeds_when_zip_upload_fails() {
        use crate::object_store::mock::Fault;
        let store = MockStore::new();

        // Build a PushReadyState with zip_artifacts present so the
        // zip-upload block runs. The archive file is written to disk
        // so a future refactor that re-orders the fault check vs. the
        // file read still has a real file to act on.
        let r = rn("refs/heads/main");
        let temp_dir = tempfile::Builder::new()
            .prefix("test_push_")
            .tempdir()
            .unwrap();
        let bundle_path = temp_dir.path().join("bundle");
        std::fs::write(&bundle_path, b"fake bundle").unwrap();

        let archive_tempdir = tempfile::Builder::new()
            .prefix("test_zip_")
            .tempdir()
            .unwrap();
        let archive_path = archive_tempdir.path().join("repo.zip");
        std::fs::write(&archive_path, b"fake zip body").unwrap();

        let state = PushReadyState {
            remote_ref: r,
            local_sha: Sha::from_hex(SHA).unwrap(),
            pre_existing: None,
            bundle_path,
            zip_artifacts: Some(ZipArtifacts {
                archive_path,
                short_sha: "deadbeef".to_owned(),
                commit_msg: "test commit".to_owned(),
                _tempdir: archive_tempdir,
            }),
            engine: StorageEngine::Bundle,
            force: false,
            pre_existing_was_ancestor: true,
            local_spec: "refs/heads/main".to_owned(),
            _temp_dir: temp_dir,
        };

        let zip_dest = "repo/refs/heads/main/repo.zip".to_owned();
        store.arm(Fault::NetworkOnPutPath {
            key: zip_dest.clone(),
        });

        let outcome = perform_push_under_lock(&store, Some("repo"), BackendKind::S3, state)
            .await
            .expect("push must succeed even when zip upload fails");
        assert!(
            matches!(&outcome, PushOutcome::Ok { remote_ref } if remote_ref == "refs/heads/main"),
            "expected Ok(refs/heads/main), got {outcome:?}",
        );

        // The bundle reached the bucket — that is the git-protocol
        // contract for a successful push.
        let bundle_dest = format!("repo/refs/heads/main/{SHA}.bundle");
        assert!(
            store.contains(&bundle_dest),
            "new bundle must be uploaded at bundle_dest",
        );
        // The zip fault fired exactly once — proves put_path was
        // attempted and failed.
        assert_eq!(store.pending_faults(), 0);
        // The zip key is absent — proves the failure was not silently
        // swallowed by a retry that masked the regression.
        assert!(
            !store.contains(&zip_dest),
            "zip key must be absent when the upload fault fires",
        );
    }

    /// Issue #161: on S3 the zip-artifact upload must carry the
    /// `codepipeline-artifact-revision-summary` user-metadata header so
    /// AWS `CodePipeline` can consume the commit summary. On Azure the
    /// same hyphenated key is rejected by the service (metadata names
    /// must be valid C# identifiers), and the issue #127 swallow path
    /// then hides the upload failure — silently dropping every zip
    /// artifact. The fix only emits the metadata on S3; this pair of
    /// tests pins both halves of the contract against `MockStore`,
    /// which records `user_metadata` verbatim and lets us inspect it
    /// without standing up a live backend.
    #[tokio::test]
    async fn perform_push_under_lock_emits_codepipeline_metadata_on_s3() {
        let (store, zip_dest) = run_zip_push(BackendKind::S3).await;
        let meta = store.metadata(&zip_dest).expect("zip stored");
        let summary = meta
            .user_metadata
            .iter()
            .find(|(k, _)| k == "codepipeline-artifact-revision-summary")
            .expect("S3 push must attach the CodePipeline revision-summary metadata");
        assert_eq!(summary.1, "test commit");
    }

    #[tokio::test]
    async fn perform_push_under_lock_omits_codepipeline_metadata_on_azure() {
        let (store, zip_dest) = run_zip_push(BackendKind::Azure).await;
        let meta = store.metadata(&zip_dest).expect("zip stored");
        assert!(
            meta.user_metadata.is_empty(),
            "Azure push must not attach hyphenated CodePipeline metadata; \
             got {entries:?}",
            entries = meta.user_metadata,
        );
    }

    /// Drive a single `?zip=1` push through `perform_push_under_lock` for
    /// the given backend kind, returning the store and the zip key so
    /// each test can assert on `user_metadata` independently. Centralised
    /// here so an accidental drift between the S3 and Azure variants
    /// (different `commit_msg`, different prefix, different ref) cannot
    /// hide a regression in the metadata wiring.
    async fn run_zip_push(kind: BackendKind) -> (MockStore, String) {
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        let temp_dir = tempfile::Builder::new()
            .prefix("test_push_")
            .tempdir()
            .unwrap();
        let bundle_path = temp_dir.path().join("bundle");
        std::fs::write(&bundle_path, b"fake bundle").unwrap();

        let archive_tempdir = tempfile::Builder::new()
            .prefix("test_zip_")
            .tempdir()
            .unwrap();
        let archive_path = archive_tempdir.path().join("repo.zip");
        std::fs::write(&archive_path, b"fake zip body").unwrap();

        let state = PushReadyState {
            remote_ref: r,
            local_sha: Sha::from_hex(SHA).unwrap(),
            pre_existing: None,
            bundle_path,
            zip_artifacts: Some(ZipArtifacts {
                archive_path,
                short_sha: "deadbeef".to_owned(),
                commit_msg: "test commit".to_owned(),
                _tempdir: archive_tempdir,
            }),
            engine: StorageEngine::Bundle,
            force: false,
            pre_existing_was_ancestor: true,
            local_spec: "refs/heads/main".to_owned(),
            _temp_dir: temp_dir,
        };

        let outcome = perform_push_under_lock(&store, Some("repo"), kind, state)
            .await
            .expect("push must succeed");
        assert!(matches!(outcome, PushOutcome::Ok { .. }));
        let zip_dest = "repo/refs/heads/main/repo.zip".to_owned();
        assert!(
            store.contains(&zip_dest),
            "zip artifact must land on bucket"
        );
        (store, zip_dest)
    }

    // --- delete_remote_ref_under_lock ---------------------------------

    /// Issue #133: under the lock, a ref whose only remaining object is
    /// the lock key itself reports `"not found"?` rather than the
    /// pre-#133 quirk of treating the lock as a bundle. In production
    /// the lock here is the one [`acquire_lock`] holds across the call;
    /// `release_lock` deletes it after this function returns, so the
    /// caller never sees a dangling lock either way.
    ///
    /// This pins the new contract: the filter on the lock key must
    /// short-circuit to `"not found"?` when the lock is the only
    /// listed entry, NOT delete it as a bundle.
    #[tokio::test]
    async fn delete_remote_ref_under_lock_reports_not_found_when_only_lock_present() {
        let store = MockStore::new();
        let lock_key = "repo/refs/heads/main/LOCK#.lock";
        // Simulate the lock we hold across the call (the production
        // caller has already acquired it via `acquire_lock`).
        store.insert(lock_key, Bytes::from_static(b"held-lock-payload"));
        let r = rn("refs/heads/main");

        let outcome = delete_remote_ref_under_lock(&store, Some("repo"), &r, false, lock_key)
            .await
            .unwrap();

        match outcome {
            PushOutcome::Error { message, .. } => {
                assert_eq!(message, r#""not found"?"#);
            }
            PushOutcome::Ok { .. } => panic!("expected Error, got Ok"),
        }
        // The lock key is NOT swept by `delete_remote_ref_under_lock` —
        // `release_lock` is responsible for removing it. Pin that here
        // so a regression that swept the held lock would fail.
        assert!(
            store.contains(lock_key),
            "delete_remote_ref_under_lock must NOT delete the held lock key",
        );
    }

    /// Issue #133: when a concurrent push lands a NEW bundle between
    /// the lock-acquire and our listing inside the lock, the post-lock
    /// listing reflects the new bundle. The delete proceeds normally
    /// against that bundle (and the lock is filtered out). This pins
    /// the close of the race window the issue describes: the listing
    /// is now under the lock, so the deletion target is whatever the
    /// concurrent writer left behind, not a stale pre-lock snapshot.
    #[tokio::test]
    async fn delete_remote_ref_under_lock_deletes_concurrently_landed_bundle() {
        let store = MockStore::new();
        let r = rn("refs/heads/main");
        let lock_key = "repo/refs/heads/main/LOCK#.lock";
        // Concurrent push landed this bundle; we hold the lock now.
        let bundle = format!("repo/refs/heads/main/{OTHER_SHA}.bundle");
        store.insert(&bundle, Bytes::from_static(b"new"));
        store.insert(lock_key, Bytes::from_static(b"held-lock-payload"));

        let outcome = delete_remote_ref_under_lock(&store, Some("repo"), &r, false, lock_key)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            PushOutcome::Ok {
                remote_ref: "refs/heads/main".into()
            }
        );
        assert!(
            !store.contains(&bundle),
            "concurrently-landed bundle must be swept"
        );
        assert!(
            store.contains(lock_key),
            "held lock must survive the sweep (release_lock removes it)",
        );
    }

    /// Stale lock is deleted but another client re-acquires it before our
    /// retry `put_if_absent`. Must return `Ok(None)` — the caller maps
    /// this to a "lock held" user error, not a hard failure.
    #[tokio::test]
    async fn acquire_lock_stale_retry_loses_second_race() {
        use crate::object_store::mock::Fault;
        let store = MockStore::new();
        let now = OffsetDateTime::now_utc();
        let stale = now - Duration::seconds(120);
        store.insert_with("k", Bytes::new(), stale, PutOpts::default());
        // Another client wins the race between our delete and retry.
        store.arm(Fault::ContendedPutIfAbsent { key: "k".into() });
        let arc = Arc::new(store);
        let guard = acquire_lock(
            Arc::clone(&arc) as Arc<dyn ObjectStore>,
            "k",
            Duration::seconds(60),
            now,
        )
        .await
        .unwrap();
        assert!(guard.is_none(), "expected contention on the retry race");
        // Fault fired — confirms the retry put_if_absent was called.
        assert_eq!(arc.pending_faults(), 0);
        // The stale lock was removed; no key remains.
        assert!(!arc.contains("k"));
    }

    // --- full_error_chain dedup --------------------------------------

    /// Tripwire for the dedup-by-suffix fix: a naive chain-walk on
    /// `PushError::Store(ObjectStoreError::Network(_))` produces a
    /// duplicated tail because both `PushError::Store` and
    /// `ObjectStoreError::Network` inline their immediate source via
    /// `{0}` in the `Display` derive. The shared
    /// `super::append_source_chain` helper skips levels whose text is
    /// already at the tail of the message. A regression that
    /// re-introduced the always-append walk would render
    /// `"…network error: dns failure: dns failure"` (or even longer
    /// for deeper chains), failing this byte-exact assertion.
    #[test]
    fn full_error_chain_deduplicates_inlined_source_text() {
        let inner: crate::object_store::BoxError = Box::new(std::io::Error::other("dns failure"));
        let err = PushError::Store(ObjectStoreError::Network(inner));
        let rendered = full_error_chain(&err);
        assert_eq!(
            rendered, "object-store error during push: network error: dns failure",
            "PushError::Store(Network(_)) must not duplicate the inner source",
        );
    }

    // --- bundle progress sink wiring (issue #55) -----------------------

    /// Decorator around `MockStore` that records, for every `put_path`
    /// call, whether `opts.progress` was `Some`. Used to pin the
    /// "bundle uploads attach a progress sink" contract from issue
    /// #55: a regression that drops the sink from `perform_push_under_lock`
    /// would silently regress the only thing `git push` users have to
    /// watch a multi-GiB transfer.
    ///
    /// The decorator forwards every other method to the inner
    /// `MockStore` unchanged. Wrapping for the assertion-of-interest
    /// alone keeps the test tightly scoped — there is no fault
    /// injection, no chunking knob, just a Vec of "did `put_path` get
    /// a sink?" booleans keyed by the destination key.
    #[derive(Default)]
    struct RecordingPutPathStore {
        inner: MockStore,
        put_path_progress_seen: std::sync::Mutex<Vec<(String, bool)>>,
    }

    impl RecordingPutPathStore {
        fn observed(&self) -> Vec<(String, bool)> {
            self.put_path_progress_seen
                .lock()
                .expect("observation lock")
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for RecordingPutPathStore {
        async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, ObjectStoreError> {
            self.inner.list(prefix).await
        }
        async fn get_to_file(
            &self,
            key: &str,
            dest: &Path,
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
            src: &Path,
            opts: PutOpts,
        ) -> Result<(), ObjectStoreError> {
            self.put_path_progress_seen
                .lock()
                .expect("observation lock")
                .push((key.to_owned(), opts.progress.is_some()));
            self.inner.put_path(key, src, opts).await
        }
        async fn put_if_absent(&self, key: &str, body: Bytes) -> Result<bool, ObjectStoreError> {
            self.inner.put_if_absent(key, body).await
        }
        async fn head(&self, key: &str) -> Result<ObjectMeta, ObjectStoreError> {
            self.inner.head(key).await
        }
        async fn copy(&self, src: &str, dst: &str) -> Result<(), ObjectStoreError> {
            self.inner.copy(src, dst).await
        }
        async fn delete(&self, key: &str) -> Result<(), ObjectStoreError> {
            self.inner.delete(key).await
        }
    }

    /// `perform_push_under_lock` must attach a `ProgressSink` to the
    /// bundle `put_path` so `git push` users see motion during a slow
    /// upload (issue #55). Before the fix, this call passed
    /// `PutOpts::default()` and the user saw nothing for hours on a
    /// 20 GiB push.
    #[tokio::test]
    async fn perform_push_under_lock_attaches_progress_sink_to_bundle_put_path() {
        let store = RecordingPutPathStore::default();
        let r = rn("refs/heads/main");
        let temp_dir = tempfile::Builder::new()
            .prefix("test_push_progress_")
            .tempdir()
            .unwrap();
        let bundle_path = temp_dir.path().join("bundle");
        std::fs::write(&bundle_path, b"fake bundle").unwrap();
        let state = PushReadyState {
            remote_ref: r,
            local_sha: Sha::from_hex(SHA).unwrap(),
            pre_existing: None,
            bundle_path,
            zip_artifacts: None,
            engine: StorageEngine::Bundle,
            force: false,
            pre_existing_was_ancestor: true,
            local_spec: "refs/heads/main".to_owned(),
            _temp_dir: temp_dir,
        };
        let outcome = perform_push_under_lock(&store, Some("repo"), BackendKind::S3, state)
            .await
            .unwrap();
        assert!(
            matches!(outcome, PushOutcome::Ok { .. }),
            "expected Ok outcome",
        );
        let observed = store.observed();
        // Exactly one `put_path` call (no zip artifacts in this fixture).
        // Pinning ≥1 rather than ==1 guards against future test
        // refactors that legitimately add another upload — the contract
        // we care about is that *every* bundle-class upload carries a
        // sink.
        assert!(
            !observed.is_empty(),
            "perform_push_under_lock must call put_path for the bundle",
        );
        for (key, has_sink) in &observed {
            assert!(
                has_sink,
                "put_path for `{key}` must carry a ProgressSink (issue #55)",
            );
        }
    }

    /// `bundle_progress_sink` accepts an arbitrary stream of
    /// `report(amount)` calls without panicking and tolerates `None`
    /// for `total` (the stat-failed fallback). Pins the sink's
    /// public-shape contract: a regression that switched the
    /// `Option<u64>` to a non-optional `u64` would force every caller
    /// to handle stat errors, breaking the "graceful degradation"
    /// note in the helper's doc comment.
    #[test]
    fn bundle_progress_sink_accepts_reports_without_panicking() {
        let with_total = bundle_progress_sink("repo/bundle.bundle", Some(1_024));
        with_total.report(256);
        with_total.report(256);
        with_total.report(512);

        let without_total = bundle_progress_sink("repo/bundle.bundle", None);
        without_total.report(1);
        without_total.report(u64::MAX); // saturates rather than wraps
    }

    /// Pin the not-ancestor wire token at the Rust level. The shellspec
    /// suites (`spec/integration/{s3,az}/force_push_spec.sh`,
    /// `spec/live/s3/force_push_spec.sh`) assert on the literal
    /// substring `"not ancestor"` — but they only run via `make
    /// shellspec-*` targets, not the default `cargo test --workspace`
    /// gate. This test catches a Rust dev who renames the constant
    /// without updating the spec files.
    #[test]
    fn not_ancestor_token_value_is_stable() {
        assert_eq!(
            NOT_ANCESTOR_TOKEN, "not ancestor",
            "spec/{{integration,live}}/*/force_push_spec.sh asserts on this exact substring",
        );
        let formatted = format!(r#""remote ref is {NOT_ANCESTOR_TOKEN} of refs/heads/main."?"#);
        assert!(
            formatted.contains(NOT_ANCESTOR_TOKEN),
            "the not-ancestor PushOutcome::Error message must embed the token literally; got {formatted:?}",
        );
    }

    /// CRLF in a commit-message summary would split the
    /// `x-amz-meta-codepipeline-artifact-revision-summary` (or
    /// `x-ms-meta-…`) header on the wire, letting a forged commit
    /// inject arbitrary user metadata onto the uploaded zip
    /// archive. `sanitize_metadata_value` must collapse every ASCII
    /// control byte (CR, LF, NUL, HTAB, …) to a space so the
    /// resulting header value is single-line and free of injection
    /// payloads.
    #[test]
    fn sanitize_metadata_value_strips_control_chars() {
        assert_eq!(
            sanitize_metadata_value("hello\r\nX-Injected: yes"),
            "hello  X-Injected: yes",
        );
        assert_eq!(sanitize_metadata_value("nul\0byte"), "nul byte",);
        assert_eq!(sanitize_metadata_value("plain text"), "plain text");
        assert_eq!(
            sanitize_metadata_value("café — short summary"),
            "café — short summary",
            "non-ASCII printable characters must pass through unchanged",
        );
        assert_eq!(sanitize_metadata_value(""), "");
    }

    // --- Issue #129: force-push protection check runs under the lock ---

    /// Regression for issue #129. If a concurrent `protect` lands a
    /// `PROTECTED#` marker between the pre-lock work in `prepare_push`
    /// and the lock acquisition in `push_one`, the under-lock arm of
    /// `perform_push_under_lock` must observe it and reject a non-FF
    /// force-push with the same `NotAncestor` wire token the pre-lock
    /// non-force probe would have produced.
    ///
    /// `pre_existing_was_ancestor = false` represents "this force-push
    /// is NOT a fast-forward" — exactly the case the historical
    /// protection-demotion logic rejected. The `PROTECTED#` key is
    /// seeded after the would-be pre-lock check (we simulate that by
    /// constructing the state with `force=true` and seeding the marker
    /// alongside the matching pre-existing bundle).
    #[tokio::test]
    async fn perform_push_under_lock_rejects_force_when_protected_under_lock_and_not_ff() {
        let store = MockStore::new();
        let pre_key = format!("repo/refs/heads/main/{OTHER_SHA}.bundle");
        store.insert(&pre_key, Bytes::from_static(b"old bundle"));
        // Concurrent `protect` lands the marker before we get the lock.
        store.insert("repo/refs/heads/main/PROTECTED#", Bytes::from_static(b""));
        let mut state = push_state_with_pre_existing(Some(pre_key.clone()));
        state.force = true;
        state.pre_existing_was_ancestor = false;
        let outcome = perform_push_under_lock(&store, Some("repo"), BackendKind::S3, state)
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
        // The protection marker must survive — the engine never touches
        // protect/unprotect state. A regression that swept it on the
        // refusal path would silently downgrade a server's protection.
        assert!(store.contains("repo/refs/heads/main/PROTECTED#"));
        // The pre-existing bundle must survive intact: a refusal path
        // that erroneously progressed past the protection check could
        // overwrite or delete it.
        let local_sha = SHA;
        assert!(store.contains(&pre_key));
        assert!(
            !store.contains(&format!("repo/refs/heads/main/{local_sha}.bundle")),
            "refused push must not upload the new bundle",
        );
    }

    /// Companion to the rejection case: when the user's local tip IS a
    /// fast-forward of the pre-existing remote bundle (`pre_existing_was_ancestor
    /// = true`), the historical "protected ref + force" semantic is to
    /// proceed — protection only blocks non-fast-forward force-pushes.
    /// This test pins that branch: a `PROTECTED#` marker under the
    /// lock plus a FF push must still succeed.
    #[tokio::test]
    async fn perform_push_under_lock_allows_force_when_protected_under_lock_but_ff() {
        let store = MockStore::new();
        let pre_key = format!("repo/refs/heads/main/{OTHER_SHA}.bundle");
        store.insert(&pre_key, Bytes::from_static(b"old bundle"));
        store.insert("repo/refs/heads/main/PROTECTED#", Bytes::from_static(b""));
        let mut state = push_state_with_pre_existing(Some(pre_key.clone()));
        state.force = true;
        state.pre_existing_was_ancestor = true; // FF case.
        let outcome = perform_push_under_lock(&store, Some("repo"), BackendKind::S3, state)
            .await
            .unwrap();
        assert!(
            matches!(&outcome, PushOutcome::Ok { remote_ref } if remote_ref == "refs/heads/main"),
            "FF push must pass even when protected, got {outcome:?}",
        );
        // Protection marker still in place after the push.
        assert!(store.contains("repo/refs/heads/main/PROTECTED#"));
    }

    /// Companion to the rejection case: a legitimate force-push (force,
    /// non-FF, no `PROTECTED#` marker) must proceed. This pins the
    /// polarity of the AND-clause guarding the protection rejection — a
    /// regression that dropped the `is_protected` check from the
    /// condition would refuse every non-FF force-push, not just those
    /// against protected refs.
    #[tokio::test]
    async fn perform_push_under_lock_allows_force_when_not_ancestor_and_not_protected() {
        let store = MockStore::new();
        let pre_key = format!("repo/refs/heads/main/{OTHER_SHA}.bundle");
        store.insert(&pre_key, Bytes::from_static(b"old bundle"));
        // No PROTECTED# marker.
        let mut state = push_state_with_pre_existing(Some(pre_key.clone()));
        state.force = true;
        state.pre_existing_was_ancestor = false; // non-FF force-push.
        let outcome = perform_push_under_lock(&store, Some("repo"), BackendKind::S3, state)
            .await
            .unwrap();
        assert!(
            matches!(&outcome, PushOutcome::Ok { remote_ref } if remote_ref == "refs/heads/main"),
            "legitimate non-FF force-push must proceed, got {outcome:?}",
        );
        // A passing push uploads the new bundle keyed by `local_sha`.
        let local_sha = SHA;
        assert!(
            store.contains(&format!("repo/refs/heads/main/{local_sha}.bundle")),
            "new bundle must be uploaded on a successful force-push",
        );
        // Defensive: the NotAncestor wire token must NOT appear anywhere
        // in the outcome — that would mean the guard mis-fired.
        if let PushOutcome::Error { message, .. } = &outcome {
            assert!(
                !message.contains("not ancestor"),
                "force-push without protection must not emit NotAncestor: {message}",
            );
        }
    }

    /// A non-force push (`force=false`) must never consult `is_protected`
    /// under the lock: non-FF non-force pushes were already rejected
    /// pre-lock by the ancestry probe, and FF non-force pushes are
    /// unaffected by protection. The under-lock check is gated on
    /// `force` precisely so this round-trip costs zero extra HEAD calls.
    ///
    /// Test-design note: `pre_existing_was_ancestor` is forced to `false`
    /// here so that ONLY the `force` clause of the under-lock guard
    /// (`force && !pre_existing_was_ancestor && is_protected(...)`)
    /// keeps the protection check off. A regression that drops the
    /// `force &&` clause would flip the guard to true and fail this
    /// test; if `pre_existing_was_ancestor` were left as `true`, the
    /// `!pre_existing_was_ancestor` clause would short-circuit and
    /// hide that regression. The `pre_existing` bundle key is omitted
    /// to keep the FF-vs-non-FF semantics consistent with "no prior
    /// remote SHA, therefore not an ancestor".
    #[tokio::test]
    async fn perform_push_under_lock_skips_protection_check_for_non_force() {
        let store = MockStore::new();
        // Marker present, but a non-force push must not even probe for it.
        store.insert("repo/refs/heads/main/PROTECTED#", Bytes::from_static(b""));
        let mut state = push_state_with_pre_existing(None);
        state.force = false;
        state.pre_existing_was_ancestor = false;
        let outcome = perform_push_under_lock(&store, Some("repo"), BackendKind::S3, state)
            .await
            .unwrap();
        assert!(
            matches!(&outcome, PushOutcome::Ok { remote_ref } if remote_ref == "refs/heads/main"),
            "non-force push must pass regardless of protection: {outcome:?}",
        );
    }
}

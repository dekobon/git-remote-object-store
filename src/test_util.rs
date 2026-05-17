//! Shared test helpers consumed by `tests/` and `cli/tests/`.
//!
//! Gated on `#[cfg(any(test, feature = "test-util"))]` so production
//! builds never compile this module. The lib's own integration tests
//! pick it up via the in-crate `cfg(test)` guard; the `cli` crate's
//! integration tests enable the `test-util` Cargo feature on the path
//! dependency (see `cli/Cargo.toml`).
//!
//! The helpers here are the single source of truth for the git-CLI
//! shellouts, the in-process REPL driver, and the seed-repo factory.
//! Prior to consolidation, `tests/common/mod.rs` and
//! `cli/tests/common/packchain_live.rs` each carried near-identical
//! copies that drifted on docstrings and error wording; moving them
//! here removes the duplication so future call sites cannot diverge.

// These helpers shell out to `git` and drive a duplex channel inside a
// tokio runtime; expressing every panic path in a `# Panics` section
// would balloon the docstrings without telling test authors anything
// they don't already expect from a test fixture. `must_use` is not
// useful for fixture builders whose return value is a tempdir or a
// stdout buffer that callers commonly drop after assertions.
#![allow(clippy::missing_panics_doc, clippy::must_use_candidate)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use tokio::io::AsyncWriteExt;

/// Per-key serialization registry for [`EnvGuard`] and
/// [`env_var_read_lock`]. Each env var name gets its own `RwLock`:
/// writers (env-mutating tests via [`EnvGuard`]) take the write lock for
/// the test's full duration; readers (production code reading the var,
/// gated behind `cfg(any(test, feature = "test-util"))`) take the read
/// lock briefly. The registry map is behind a `Mutex`; we `Box::leak`
/// per-key locks so guards can hold a `'static` reference without a
/// lifetime parameter.
fn env_var_lock(key: &'static str) -> &'static RwLock<()> {
    static REGISTRY: OnceLock<Mutex<HashMap<&'static str, &'static RwLock<()>>>> = OnceLock::new();
    let registry = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = registry.lock().unwrap_or_else(PoisonError::into_inner);
    map.entry(key)
        .or_insert_with(|| Box::leak(Box::new(RwLock::new(()))))
}

thread_local! {
    /// Env-var keys for which the current thread holds an [`EnvGuard`]
    /// write lock. Only ever non-empty inside a test that has an
    /// [`EnvGuard`] live; release builds (where `test-util` is off and
    /// `cfg(test)` is unset) never enter the code paths that touch
    /// this set, so it is always empty for production threads.
    ///
    /// [`env_var_read_lock`] consults this set so a writer on thread T
    /// can recurse into production code that reads the same key from
    /// thread T without re-acquiring the lock — that would deadlock on
    /// the writer-held `RwLock`.
    static WRITER_KEYS: RefCell<HashSet<&'static str>> = RefCell::new(HashSet::new());
}

/// Acquire a read lock on the per-key serialization slot — see
/// [`env_var_lock`] — and return it for the caller to hold while it
/// reads the env var.
///
/// Returns `None` (without acquiring) when the current thread already
/// holds an [`EnvGuard`] write lock for `key`: that means the caller
/// is the mutating test itself recursing into production code, and
/// taking a read lock on top of the same `RwLock` would deadlock.
///
/// Production code paths that read env vars also touched by
/// [`EnvGuard`] should gate this acquisition behind
/// `cfg(any(test, feature = "test-util"))` so release binaries pay no
/// cost.
pub(crate) fn env_var_read_lock(key: &'static str) -> Option<RwLockReadGuard<'static, ()>> {
    let same_thread_writer = WRITER_KEYS.with(|s| s.borrow().contains(key));
    if same_thread_writer {
        None
    } else {
        Some(
            env_var_lock(key)
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

/// RAII guard that mutates a process-global env var and restores its
/// prior value when dropped — including on panic.
///
/// Three correctness properties this gives every test that uses it:
///
/// 1. **Panic-safe cleanup**: the manual `set_var` / `remove_var` pair
///    leaks the env var to subsequent tests when an assertion between
///    the two panics. `Drop` runs on unwind, so the prior value is
///    always restored.
/// 2. **Per-key writer serialization**: two tests touching the same env
///    var across modules would race, with `set_var` from one
///    interleaving with `remove_var` from the other. The guard holds a
///    per-key `RwLock` write lock for its full lifetime, so only one
///    guard for a given key exists at a time.
/// 3. **Reader-vs-writer serialization**: production code paths that
///    read the same env var can opt in to [`env_var_read_lock`], which
///    takes the per-key read lock. A test holding [`EnvGuard`] blocks
///    those reads on other threads until it drops — so parallel push
///    tests reading via gated production code never observe a
///    mutating test's transient value.
///
/// Recursive acquisition on the same thread would deadlock — hold one
/// guard per env var at a time.
///
/// API shape: `set` / `unset` / `take` are the constructors (they
/// acquire the per-key lock); `set_to` / `clear` are the mutation
/// methods you call on an existing guard when a test toggles through
/// multiple values without ever releasing the lock. Rust's inherent-
/// method rules forbid reusing the same name for an associated
/// function and a method, so the constructor/method pair uses
/// `set` / `set_to` and `unset` / `clear` respectively.
///
/// # Example
///
/// ```ignore
/// // Set a var, run assertions, restore prior on drop:
/// let _env = EnvGuard::set("MY_VAR", "value");
/// assert_eq!(std::env::var("MY_VAR").unwrap(), "value");
/// // … drop restores the value `MY_VAR` had before the guard ran.
///
/// // For tests that toggle through several values, `take` acquires
/// // the lock without mutating, and `set_to` / `clear` mutate within
/// // the guarded scope:
/// let env = EnvGuard::take("MY_VAR");
/// env.set_to("first");
/// env.set_to("second");
/// env.clear();
/// // … drop restores the original.
/// ```
pub struct EnvGuard {
    key: &'static str,
    prior: Option<OsString>,
    /// Holds the per-key write lock for the guard's lifetime. The
    /// manual `Drop for EnvGuard` runs before any field is dropped, so
    /// `_lock` is still held while we restore the value — no other
    /// guard or reader for this key can interleave with the restore.
    _lock: RwLockWriteGuard<'static, ()>,
}

impl EnvGuard {
    /// Acquire the per-key write lock and record the env var's current
    /// value. Does not mutate. Pair with [`Self::set_to`] /
    /// [`Self::clear`] for tests that toggle through multiple values.
    pub fn take(key: &'static str) -> Self {
        let lock = env_var_lock(key)
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        WRITER_KEYS.with(|s| s.borrow_mut().insert(key));
        let prior = std::env::var_os(key);
        Self {
            key,
            prior,
            _lock: lock,
        }
    }

    /// Acquire the lock, record the prior value, and set `key` to
    /// `value` for the guard's lifetime.
    pub fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let guard = Self::take(key);
        guard.set_to(value);
        guard
    }

    /// Acquire the lock, record the prior value, and unset `key` for
    /// the guard's lifetime.
    pub fn unset(key: &'static str) -> Self {
        let guard = Self::take(key);
        guard.clear();
        guard
    }

    /// Set the env var to `value`. The caller already holds the
    /// per-key lock via this guard.
    pub fn set_to(&self, value: impl AsRef<OsStr>) {
        // SAFETY: `set_var` is process-global. `self._lock` is the
        // per-key `RwLockWriteGuard`, which guarantees:
        //  - no other [`EnvGuard`] for this key exists (writers
        //    serialise on the same `RwLock`);
        //  - no concurrent reader on another thread can be inside
        //    [`env_var_read_lock`] for this key (the read lock blocks
        //    while the write lock is held).
        // Production reads of this key are either compiled out (release
        // builds, where the `test-util` feature is off and `cfg(test)`
        // is unset) or routed through [`env_var_read_lock`] (test
        // builds), so no thread observes a torn value.
        unsafe {
            std::env::set_var(self.key, value);
        }
    }

    /// Unset the env var. The caller already holds the per-key lock
    /// via this guard.
    pub fn clear(&self) {
        // SAFETY: see [`Self::set_to`].
        unsafe {
            std::env::remove_var(self.key);
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: we still hold the per-key write lock via `_lock`;
        // no other thread can be reading or writing this key
        // concurrently.
        unsafe {
            match &self.prior {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
        WRITER_KEYS.with(|s| s.borrow_mut().remove(self.key));
    }
}

use crate::object_store::ObjectStore;
use crate::protocol::backend;
use crate::protocol::{ProtocolError, run};
use crate::url::RemoteUrl;

/// Check whether the `git` CLI is available on `PATH`. Cached once per
/// test binary so the repeated probe does not dominate test startup.
pub fn git_available() -> bool {
    static AVAIL: OnceLock<bool> = OnceLock::new();
    *AVAIL.get_or_init(|| {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    })
}

/// Run a `git` command and assert it succeeds.
pub fn git(args: &[&str], cwd: &Path) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Run a `git` command, assert it succeeds, and return its stdout.
pub fn git_capture(args: &[&str], cwd: &Path) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("git stdout utf-8")
}

/// Initialise a fresh repo with `n` linear commits on `refs/heads/main`
/// and return the dir + `Vec<sha>` in commit order (oldest first).
///
/// `label` differentiates blob contents across distinct test scenarios
/// so two seeded repos do not produce identical commit SHAs. Two
/// failure modes the label guards against:
///
/// - **Same-second hash collisions in lib tests**: commit time
///   resolution is one second; two repos seeded back-to-back with the
///   same blob bytes can hash-collide and break tests that compare
///   their tip SHAs.
/// - **Shared-bucket pack collisions in live tests**: distinct
///   scenarios sharing a bucket otherwise produce the same content-SHA
///   pack object, and the second push silently no-ops. Within a single
///   scenario, label-equality is fine because bucket isolation per
///   tempdir/container guarantees no on-bucket collisions across
///   parameterised invocations of the same scenario.
pub fn make_seed_repo(n: usize, label: &str) -> (tempfile::TempDir, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    git(&["init", "--quiet", "--initial-branch=main"], dir.path());
    git(&["config", "user.email", "test@example.com"], dir.path());
    git(&["config", "user.name", "Test"], dir.path());
    git(&["config", "commit.gpgsign", "false"], dir.path());

    let mut shas = Vec::with_capacity(n);
    for i in 0..n {
        let body = format!("{label}-{i}\n");
        std::fs::write(dir.path().join(format!("f{i}.txt")), body.as_bytes()).unwrap();
        git(&["add", "."], dir.path());
        git(
            &["commit", "--quiet", "-m", "step", "--no-gpg-sign"],
            dir.path(),
        );
        let sha = git_capture(&["rev-parse", "HEAD"], dir.path())
            .trim()
            .to_owned();
        shas.push(sha);
    }
    (dir, shas)
}

/// Drive [`crate::protocol::run`] in-process via a tokio duplex channel.
///
/// Feeds `script` to the helper's stdin, collects all stdout output,
/// and returns `(stdout_bytes, run_result)`. Used by both the
/// MockStore-driven lib integration tests (where `validate_format` runs
/// against the in-memory mock) and the live-backend cli integration
/// tests (where it runs against a freshly-prepared S3/Azure bucket).
pub async fn drive_in(
    remote: RemoteUrl,
    store: Arc<dyn ObjectStore>,
    script: &str,
    repo_dir: PathBuf,
) -> (Vec<u8>, Result<(), ProtocolError>) {
    let (client_side, helper_side) = tokio::io::duplex(64 * 1024);
    let (helper_in, helper_out) = tokio::io::split(helper_side);
    let (mut client_reader, mut client_writer) = tokio::io::split(client_side);

    let script_bytes = script.as_bytes().to_owned();
    let writer_task = tokio::spawn(async move {
        // Tolerate `BrokenPipe`: a helper that aborts early (e.g.
        // engine-not-implemented for `?engine=packchain`) closes its
        // stdin reader before the full script lands. That is correct
        // helper behaviour, not a test failure.
        let suppress_broken_pipe = |e: std::io::Error| {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                Ok(())
            } else {
                Err(e)
            }
        };
        client_writer
            .write_all(&script_bytes)
            .await
            .or_else(suppress_broken_pipe)
            .unwrap();
        client_writer
            .shutdown()
            .await
            .or_else(suppress_broken_pipe)
            .unwrap();
    });

    let reader_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        client_reader.read_to_end(&mut buf).await.unwrap();
        buf
    });

    // Mirror production wiring: production calls `backend::build` which
    // computes the engine from FORMAT + URL flag. Tests skip `build`
    // (their MockStore needs no probe, and the live bucket is freshly
    // prepared) but still need the same engine resolution so
    // `protocol::run` dispatches correctly.
    let engine = backend::validate_format(
        remote.kind(),
        store.as_ref(),
        remote.prefix().unwrap_or_default(),
        remote.flags().engine,
    )
    .await
    .expect("validate_format must succeed in tests with valid setup");
    let result = run(
        remote,
        store,
        engine,
        tokio::io::BufReader::new(helper_in),
        helper_out,
        None,
        repo_dir,
    )
    .await;

    writer_task.await.unwrap();
    let output = reader_task.await.unwrap();
    (output, result)
}

#[cfg(test)]
mod env_guard_tests {
    use super::EnvGuard;

    // Each test uses a unique key so the cases are independent even if
    // run in parallel. `EnvGuard`'s registry serializes per-key, not
    // globally, so unrelated keys never block each other.

    #[test]
    fn set_then_drop_restores_unset_prior() {
        let key = "GROS_ENV_GUARD_TEST_SET_THEN_UNSET";
        // SAFETY: this key is unique to this test; no other reader exists.
        unsafe {
            std::env::remove_var(key);
        }
        {
            let _g = EnvGuard::set(key, "value");
            assert_eq!(std::env::var(key).as_deref(), Ok("value"));
        }
        assert!(std::env::var_os(key).is_none());
    }

    #[test]
    fn set_then_drop_restores_prior_set_value() {
        let key = "GROS_ENV_GUARD_TEST_SET_THEN_RESET";
        // SAFETY: this key is unique to this test; no other reader exists.
        unsafe {
            std::env::set_var(key, "original");
        }
        {
            let _g = EnvGuard::set(key, "override");
            assert_eq!(std::env::var(key).as_deref(), Ok("override"));
        }
        assert_eq!(std::env::var(key).as_deref(), Ok("original"));
        // SAFETY: cleanup of fixture-set value.
        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn unset_then_drop_restores_prior_value() {
        let key = "GROS_ENV_GUARD_TEST_UNSET_THEN_RESET";
        // SAFETY: this key is unique to this test; no other reader exists.
        unsafe {
            std::env::set_var(key, "original");
        }
        {
            let _g = EnvGuard::unset(key);
            assert!(std::env::var_os(key).is_none());
        }
        assert_eq!(std::env::var(key).as_deref(), Ok("original"));
        // SAFETY: cleanup of fixture-set value.
        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn take_then_multi_toggle_restores_original() {
        let key = "GROS_ENV_GUARD_TEST_MULTI_TOGGLE";
        // SAFETY: this key is unique to this test; no other reader exists.
        unsafe {
            std::env::set_var(key, "first");
        }
        {
            let g = EnvGuard::take(key);
            g.set_to("second");
            assert_eq!(std::env::var(key).as_deref(), Ok("second"));
            g.set_to("third");
            assert_eq!(std::env::var(key).as_deref(), Ok("third"));
            g.clear();
            assert!(std::env::var_os(key).is_none());
        }
        // Drop restores the original "first", not any intermediate value.
        assert_eq!(std::env::var(key).as_deref(), Ok("first"));
        // SAFETY: cleanup of fixture-set value.
        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn env_var_read_lock_succeeds_when_no_writer_active() {
        let key = "GROS_ENV_READ_LOCK_NO_WRITER";
        let guard = super::env_var_read_lock(key);
        assert!(
            guard.is_some(),
            "no writer for this key — read must succeed"
        );
    }

    /// The mutating-test recursion path: a thread holding an
    /// [`EnvGuard`] reads the same key via production code, which
    /// re-enters [`env_var_read_lock`]. Acquiring the read lock on top
    /// of the held write lock would deadlock; the thread-local
    /// [`WRITER_KEYS`] set lets the reader fast-path skip the lock and
    /// return `None`.
    #[test]
    fn env_var_read_lock_skips_when_same_thread_holds_writer() {
        let key = "GROS_ENV_READ_LOCK_SAME_THREAD_RECURSION";
        let _g = EnvGuard::take(key);
        let read = super::env_var_read_lock(key);
        assert!(
            read.is_none(),
            "same-thread writer must be detected so reader skips locking",
        );
    }

    /// Two readers on **different** threads must coexist when no
    /// writer is active. The test uses a `Barrier(2)` so each thread
    /// must hold its guard while the other is also holding one — a
    /// single-reader-at-a-time lock would deadlock the barrier and
    /// hang the test.
    ///
    /// `RwLock::read` documents that recursive same-thread reads
    /// "might panic" depending on the platform (Linux glibc allows it,
    /// macOS/Windows may not), so the cross-thread test is the
    /// portable shape of the "many readers, no writer" assertion.
    #[test]
    fn env_var_read_lock_allows_concurrent_readers_across_threads() {
        use std::sync::Arc;

        let key = "GROS_ENV_READ_LOCK_MULTI_READERS";
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let barrier_for_thread = barrier.clone();
        let reader = std::thread::spawn(move || {
            let guard = super::env_var_read_lock(key);
            assert!(guard.is_some(), "reader on spawned thread must acquire");
            // Hold the guard across the barrier so both threads have
            // a live read lock at the same time.
            barrier_for_thread.wait();
        });

        let guard = super::env_var_read_lock(key);
        assert!(guard.is_some(), "reader on main thread must acquire");
        barrier.wait();

        reader.join().expect("reader thread");
    }

    /// Cross-thread serialization: a writer on thread A holding an
    /// [`EnvGuard`] must block readers on thread B until it drops.
    /// Without the writer set on a different thread, the reader-thread
    /// can't fast-path — it has to wait on the `RwLock`.
    ///
    /// Synchronisation: a 2-thread `Barrier` makes the reader rendezvous
    /// with the main thread before calling [`env_var_read_lock`], so
    /// the timing window does not include reader-thread startup
    /// latency. The 20 ms sleep that follows is the *bug-detection
    /// window*: under a hypothetical regression where the read lock
    /// fails to block, the reader would acquire and set `acquired` to
    /// true within those 20 ms, tripping the `!acquired` assertion.
    /// Shrinking the sleep risks a false pass (the buggy reader hasn't
    /// reached the store yet); inflating it only slows the test. 20 ms
    /// is several orders of magnitude above the post-barrier scheduler
    /// delay we expect on a sane machine.
    #[test]
    fn cross_thread_reader_blocks_until_writer_drops() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let key = "GROS_ENV_READ_LOCK_CROSS_THREAD";
        let guard = EnvGuard::set(key, "during");
        let acquired = Arc::new(AtomicBool::new(false));
        let acquired_for_thread = acquired.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let barrier_for_thread = barrier.clone();
        let reader = std::thread::spawn(move || {
            // Rendezvous with the main thread before calling into the
            // lock — eliminates startup-latency from the timing window.
            barrier_for_thread.wait();
            let _r = super::env_var_read_lock(key);
            acquired_for_thread.store(true, Ordering::SeqCst);
        });

        barrier.wait();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(
            !acquired.load(Ordering::SeqCst),
            "reader on another thread must block while the writer is held",
        );

        // Drop the writer; the reader should now acquire and finish.
        drop(guard);
        reader.join().expect("reader thread");
        assert!(
            acquired.load(Ordering::SeqCst),
            "reader must acquire after writer releases",
        );
    }

    /// Panic inside the guarded scope must still restore the prior
    /// value — this is the core regression issue #220 closes.
    #[test]
    fn panic_inside_guard_still_restores_prior() {
        let key = "GROS_ENV_GUARD_TEST_PANIC_RESTORE";
        // SAFETY: this key is unique to this test; no other reader exists.
        unsafe {
            std::env::set_var(key, "before");
        }
        let outcome = std::panic::catch_unwind(|| {
            let _g = EnvGuard::set(key, "during");
            panic!("simulated test failure between set and remove");
        });
        assert!(outcome.is_err(), "the closure must have panicked");
        assert_eq!(
            std::env::var(key).as_deref(),
            Ok("before"),
            "Drop must restore the prior value on unwind",
        );
        // SAFETY: cleanup of fixture-set value.
        unsafe {
            std::env::remove_var(key);
        }
    }
}

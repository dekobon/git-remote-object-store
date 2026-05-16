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

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use tokio::io::AsyncWriteExt;

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

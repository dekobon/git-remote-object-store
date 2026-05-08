//! Shared helpers for protocol integration tests.
//!
//! Cargo does not treat `tests/common/mod.rs` as a test target, so
//! each `tests/protocol_*.rs` file can `mod common;` to pull these in.

// Each integration-test crate compiles this module independently, so
// helpers used by one test file but not another would trigger warnings.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use git_remote_object_store::object_store::ObjectStore;
use git_remote_object_store::protocol::backend;
use git_remote_object_store::protocol::{ProtocolError, run};
use git_remote_object_store::url::{self, RemoteUrl};
use tokio::io::AsyncWriteExt;

/// Check whether the `git` CLI is available on `PATH`.
pub fn git_available() -> bool {
    static AVAIL: OnceLock<bool> = OnceLock::new();
    *AVAIL.get_or_init(|| {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    })
}

/// Build a test [`RemoteUrl`] pointing at a fake S3 bucket.
pub fn s3_url(prefix: Option<&str>) -> RemoteUrl {
    s3_url_with_zip(prefix, false)
}

/// Build a test [`RemoteUrl`] with an optional `?zip=1` query parameter.
pub fn s3_url_with_zip(prefix: Option<&str>, zip: bool) -> RemoteUrl {
    let mut raw = match prefix {
        Some(p) => format!("s3+https://my-bucket.s3.us-west-2.amazonaws.com/{p}"),
        None => "s3+https://my-bucket.s3.us-west-2.amazonaws.com/".to_string(),
    };
    if zip {
        raw.push_str("?zip=1");
    }
    url::parse(&raw).expect("test URL must parse")
}

/// Drive [`protocol::run`] in-process via a tokio duplex channel.
///
/// Feeds `script` to the helper's stdin, collects all stdout output,
/// and returns `(stdout_bytes, run_result)`.
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
    // (their MockStore needs no probe) but still need the same engine
    // resolution so `protocol::run` dispatches correctly.
    let engine = backend::validate_format(
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
/// and return the dir + Vec<sha> in commit order (oldest first).
///
/// `salt` differentiates blob contents so two repos seeded in the same
/// wall-clock second still produce distinct commit SHAs (commit time
/// resolution is one second; without per-call salt, two seeded repos
/// can hash-collide and break tests that compare their tip SHAs).
pub fn make_seed_repo(n: usize, salt: &str) -> (tempfile::TempDir, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    git(&["init", "--quiet", "--initial-branch=main"], dir.path());
    git(&["config", "user.email", "test@example.com"], dir.path());
    git(&["config", "user.name", "Test"], dir.path());
    git(&["config", "commit.gpgsign", "false"], dir.path());

    let mut shas = Vec::with_capacity(n);
    for i in 0..n {
        let body = format!("{salt}-{i}\n");
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

/// Initialise a fresh repo with one commit and an annotated tag
/// `<tag_name>` pointing at it. Returns `(dir, commit_sha, tag_sha)`.
/// The annotated tag creates a tag-object (not a lightweight tag), so
/// `tag_sha != commit_sha`.
pub fn make_seed_repo_with_annotated_tag(
    salt: &str,
    tag_name: &str,
) -> (tempfile::TempDir, String, String) {
    let (dir, shas) = make_seed_repo(1, salt);
    git(
        &[
            "tag",
            "-a",
            tag_name,
            "-m",
            "release",
            "--no-sign",
            shas[0].as_str(),
        ],
        dir.path(),
    );
    let tag_sha = git_capture(&["rev-parse", tag_name], dir.path())
        .trim()
        .to_owned();
    assert_ne!(
        tag_sha, shas[0],
        "annotated tag must have its own object SHA",
    );
    (dir, shas[0].clone(), tag_sha)
}

/// Initialise a fresh repo with one commit and a tag-of-tag chain:
/// `<outer_name>` (annotated) → `<inner_name>` (annotated) → commit.
/// Returns `(dir, commit_sha, inner_tag_sha, outer_tag_sha)`.
pub fn make_seed_repo_with_tag_of_tag(
    salt: &str,
    inner_name: &str,
    outer_name: &str,
) -> (tempfile::TempDir, String, String, String) {
    let (dir, shas) = make_seed_repo(1, salt);
    git(
        &[
            "tag",
            "-a",
            inner_name,
            "-m",
            "inner",
            "--no-sign",
            shas[0].as_str(),
        ],
        dir.path(),
    );
    let inner_sha = git_capture(&["rev-parse", inner_name], dir.path())
        .trim()
        .to_owned();
    // `git tag -a v1 inner` creates a tag-of-tag (CLI git resolves the
    // arg's OID without peeling).
    git(
        &[
            "tag",
            "-a",
            outer_name,
            "-m",
            "outer",
            "--no-sign",
            inner_sha.as_str(),
        ],
        dir.path(),
    );
    let outer_sha = git_capture(&["rev-parse", outer_name], dir.path())
        .trim()
        .to_owned();
    assert_ne!(inner_sha, outer_sha, "outer must wrap inner");
    (dir, shas[0].clone(), inner_sha, outer_sha)
}

/// Build a `?engine=packchain` URL pointing at a fake S3 bucket.
pub fn s3_url_packchain(prefix: Option<&str>) -> RemoteUrl {
    let raw = match prefix {
        Some(p) => {
            format!("s3+https://my-bucket.s3.us-west-2.amazonaws.com/{p}?engine=packchain")
        }
        None => "s3+https://my-bucket.s3.us-west-2.amazonaws.com/?engine=packchain".to_owned(),
    };
    url::parse(&raw).expect("packchain test URL must parse")
}

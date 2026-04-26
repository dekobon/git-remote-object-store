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
    let (client_reader, mut client_writer) = tokio::io::split(client_side);

    let script_bytes = script.as_bytes().to_owned();
    let writer_task = tokio::spawn(async move {
        client_writer.write_all(&script_bytes).await.unwrap();
        client_writer.shutdown().await.unwrap();
    });

    let reader_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        client_reader
            .take(u64::MAX)
            .read_to_end(&mut buf)
            .await
            .unwrap();
        buf
    });

    let result = run(
        remote,
        store,
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

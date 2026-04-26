//! Phase 7 integration test: drive [`protocol::run`] through a fetch
//! batch against a [`MockStore`] seeded with real git bundles, and
//! verify the bundles end up applied in a destination repository.
//!
//! Ports the assertions from upstream
//! `../git-remote-s3/test/parallel_fetch_test.py` to the Rust REPL:
//! empty batch is a no-op, single fetch round-trips, multiple fetches
//! all complete, duplicate SHAs are deduped without loss, and the
//! `<prefix>=None` URL form omits the leading slash from the bundle key.

#![cfg(feature = "test-util")]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use git_remote_object_store::object_store::ObjectStore;
use git_remote_object_store::object_store::mock::MockStore;
use git_remote_object_store::protocol::{ProtocolError, run};
use git_remote_object_store::url::{self, RemoteUrl};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

fn git_available() -> bool {
    static AVAIL: OnceLock<bool> = OnceLock::new();
    *AVAIL.get_or_init(|| {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    })
}

fn s3_url(prefix: Option<&str>) -> RemoteUrl {
    let raw = match prefix {
        Some(p) => format!("s3+https://my-bucket.s3.us-west-2.amazonaws.com/{p}"),
        None => "s3+https://my-bucket.s3.us-west-2.amazonaws.com/".to_string(),
    };
    url::parse(&raw).expect("test URL must parse")
}

async fn drive_in(
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

/// Initialise a fresh repo, commit a single blob, and return the dir +
/// commit SHA.
fn make_seed_repo() -> (TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    git(&["init", "--quiet", "--initial-branch=main"], dir.path());
    git(&["config", "user.email", "test@example.com"], dir.path());
    git(&["config", "user.name", "Test"], dir.path());
    git(&["config", "commit.gpgsign", "false"], dir.path());
    std::fs::write(dir.path().join("hello.txt"), b"hi\n").unwrap();
    git(&["add", "hello.txt"], dir.path());
    git(
        &["commit", "--quiet", "-m", "seed", "--no-gpg-sign"],
        dir.path(),
    );
    let sha = git_capture(&["rev-parse", "HEAD"], dir.path());
    (dir, sha.trim().to_owned())
}

/// Bundle a ref out of `seed_dir` and return the on-disk bundle bytes.
fn bundle_ref(seed_dir: &Path, sha: &str, ref_name: &str) -> Bytes {
    let bundles = tempfile::tempdir().expect("tempdir");
    let bundle_path = bundles.path().join(format!("{sha}.bundle"));
    git(
        &["bundle", "create", bundle_path.to_str().unwrap(), ref_name],
        seed_dir,
    );
    Bytes::from(std::fs::read(&bundle_path).expect("read bundle"))
}

fn git(args: &[&str], cwd: &Path) {
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

fn git_capture(args: &[&str], cwd: &Path) -> String {
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

fn make_dst_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(&["init", "--quiet"], dir.path());
    dir
}

#[tokio::test]
async fn empty_fetch_batch_then_blank_line_emits_terminator() {
    // No fetch commands ever sent — bare blank line, in idle mode.
    let dst = make_dst_repo();
    let (out, result) = drive_in(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        "\n",
        dst.path().to_path_buf(),
    )
    .await;
    result.expect("blank line should succeed");
    assert_eq!(&out, b"\n");
}

#[tokio::test]
async fn single_fetch_downloads_and_unbundles_into_local_repo() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, sha) = make_seed_repo();
    let bundle = bundle_ref(seed.path(), &sha, "refs/heads/main");

    let store = MockStore::new();
    store.insert(format!("repo/refs/heads/main/{sha}.bundle"), bundle);

    let dst = make_dst_repo();
    let script = format!("fetch {sha} refs/heads/main\n\n");
    let (out, result) = drive_in(
        s3_url(Some("repo")),
        Arc::new(store),
        &script,
        dst.path().to_path_buf(),
    )
    .await;
    result.expect("fetch should succeed");
    assert_eq!(&out, b"\n", "fetch is silent except for terminator");

    let dst_sha = git_capture(&["rev-parse", &sha], dst.path());
    assert_eq!(dst_sha.trim(), sha);
}

#[tokio::test]
async fn fetch_works_with_no_prefix() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, sha) = make_seed_repo();
    let bundle = bundle_ref(seed.path(), &sha, "refs/heads/main");

    let store = MockStore::new();
    // No prefix — bundle key has no leading slash.
    store.insert(format!("refs/heads/main/{sha}.bundle"), bundle);

    let dst = make_dst_repo();
    let script = format!("fetch {sha} refs/heads/main\n\n");
    let (out, result) = drive_in(
        s3_url(None),
        Arc::new(store),
        &script,
        dst.path().to_path_buf(),
    )
    .await;
    result.expect("fetch should succeed");
    assert_eq!(&out, b"\n");
    let dst_sha = git_capture(&["rev-parse", &sha], dst.path());
    assert_eq!(dst_sha.trim(), sha);
}

#[tokio::test]
async fn multiple_fetches_run_to_completion() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    // Build a chain of three commits and bundle each at a distinct ref.
    let seed = tempfile::tempdir().expect("tempdir");
    git(&["init", "--quiet", "--initial-branch=main"], seed.path());
    git(&["config", "user.email", "test@example.com"], seed.path());
    git(&["config", "user.name", "Test"], seed.path());
    git(&["config", "commit.gpgsign", "false"], seed.path());

    let mut shas = Vec::new();
    for i in 0..3 {
        std::fs::write(seed.path().join(format!("f{i}.txt")), b"x\n").unwrap();
        git(&["add", "."], seed.path());
        git(
            &["commit", "--quiet", "-m", "step", "--no-gpg-sign"],
            seed.path(),
        );
        let sha = git_capture(&["rev-parse", "HEAD"], seed.path())
            .trim()
            .to_owned();
        let ref_name = format!("refs/heads/branch-{i}");
        git(&["update-ref", &ref_name, &sha], seed.path());
        shas.push((sha, ref_name));
    }

    let store = MockStore::new();
    for (sha, ref_name) in &shas {
        let bundle = bundle_ref(seed.path(), sha, ref_name);
        store.insert(format!("repo/{ref_name}/{sha}.bundle"), bundle);
    }

    let dst = make_dst_repo();
    let mut script = String::new();
    for (sha, ref_name) in &shas {
        writeln!(script, "fetch {sha} {ref_name}").unwrap();
    }
    script.push('\n');

    let (out, result) = drive_in(
        s3_url(Some("repo")),
        Arc::new(store),
        &script,
        dst.path().to_path_buf(),
    )
    .await;
    result.expect("multi fetch should succeed");
    assert_eq!(&out, b"\n");

    for (sha, _) in &shas {
        let dst_sha = git_capture(&["rev-parse", sha], dst.path());
        assert_eq!(dst_sha.trim(), *sha, "all fetched commits must resolve");
    }
}

#[tokio::test]
async fn duplicate_shas_in_batch_are_handled_safely() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, sha) = make_seed_repo();
    let bundle = bundle_ref(seed.path(), &sha, "refs/heads/main");

    let store = MockStore::new();
    store.insert(format!("repo/refs/heads/main/{sha}.bundle"), bundle);

    let dst = make_dst_repo();
    // 20 copies of the same fetch line — exercises the FetchedRefs lock
    // under concurrency. Mirrors `test_thread_safety_of_fetched_refs`.
    let line = format!("fetch {sha} refs/heads/main\n");
    let script = format!("{}\n", line.repeat(20));

    let (out, result) = drive_in(
        s3_url(Some("repo")),
        Arc::new(store),
        &script,
        dst.path().to_path_buf(),
    )
    .await;
    result.expect("duplicate-SHA batch should succeed");
    assert_eq!(&out, b"\n");
    let dst_sha = git_capture(&["rev-parse", &sha], dst.path());
    assert_eq!(dst_sha.trim(), sha);
}

#[tokio::test]
async fn fetch_missing_bundle_propagates_error() {
    let dst = make_dst_repo();
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let script = format!("fetch {sha} refs/heads/main\n\n");

    let (out, result) = drive_in(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        &script,
        dst.path().to_path_buf(),
    )
    .await;
    match result {
        Err(ProtocolError::Fetch(_)) => {}
        other => panic!("expected Fetch error, got {other:?}"),
    }
    // The handler must not emit the trailing terminator after a failed
    // batch — the helper exits non-zero and leaves stdout untouched.
    assert!(out.is_empty(), "fetch must not write on error: {out:?}");
}

#[tokio::test]
async fn fetch_invalid_sha_returns_error() {
    let dst = make_dst_repo();
    let script = "fetch notahex refs/heads/main\n\n";
    let (_out, result) = drive_in(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        script,
        dst.path().to_path_buf(),
    )
    .await;
    match result {
        Err(ProtocolError::Fetch(_)) => {}
        other => panic!("expected Fetch error, got {other:?}"),
    }
}

#[tokio::test]
async fn fetched_refs_dedupes_across_batches() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, sha) = make_seed_repo();
    let bundle = bundle_ref(seed.path(), &sha, "refs/heads/main");

    let store = MockStore::new();
    store.insert(format!("repo/refs/heads/main/{sha}.bundle"), bundle);
    let store: Arc<dyn ObjectStore> = Arc::new(store);

    let dst = make_dst_repo();

    // Two consecutive fetch batches for the same SHA. After the first
    // batch's unbundle the SHA is in fetched_refs; the second batch's
    // task must short-circuit before download — even if the bundle were
    // gone from the store, the second batch should still succeed.
    let script = format!("fetch {sha} refs/heads/main\n\nfetch {sha} refs/heads/main\n\n",);
    let (out, result) = drive_in(
        s3_url(Some("repo")),
        Arc::clone(&store),
        &script,
        dst.path().to_path_buf(),
    )
    .await;
    result.expect("repeated batches should succeed");
    assert_eq!(&out, b"\n\n", "two fetch batches → two terminators");
}

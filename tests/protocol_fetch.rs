//! Fetch integration test: drive [`protocol::run`] through a fetch
//! batch against a [`MockStore`] seeded with real git bundles, and
//! verify the bundles end up applied in a destination repository.
//!
//! Ports the assertions from upstream
//! `../git-remote-s3/test/parallel_fetch_test.py` to the Rust REPL:
//! empty batch is a no-op, single fetch round-trips, multiple fetches
//! all complete, duplicate SHAs are deduped without loss, and the
//! `<prefix>=None` URL form omits the leading slash from the bundle key.

#![cfg(feature = "test-util")]

mod common;

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use git_remote_object_store::object_store::ObjectStore;
use git_remote_object_store::object_store::mock::MockStore;
use git_remote_object_store::protocol::ProtocolError;
use tempfile::TempDir;

use common::{drive_in, git, git_available, git_capture, s3_url};

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

fn make_dst_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(&["init", "--quiet"], dir.path());
    dir
}

#[tokio::test]
async fn idle_blank_line_with_fetch_wiring_emits_terminator() {
    // Smoke coverage: confirm the `repo_dir` parameter and FetchedRefs
    // session state do not perturb the idle blank-line path. No fetch
    // commands are sent — `mode` stays `None`, so the fetch batch flush
    // in mod.rs is bypassed entirely. The internal empty-cmds
    // short-circuit in `fetch_batch` is covered separately by the unit
    // test in `src/protocol/fetch.rs`.
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
    use git_remote_object_store::protocol::fetch::FetchError;

    let dst = make_dst_repo();
    let script = "fetch notahex refs/heads/main\n\n";
    let (_out, result) = drive_in(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        script,
        dst.path().to_path_buf(),
    )
    .await;
    // Pin the specific inner variant — a regression that misroutes a
    // parse failure into Store / Parse / Ref must fail this assertion.
    match result {
        Err(ProtocolError::Fetch(FetchError::Sha(_))) => {}
        other => panic!("expected Fetch(Sha) error, got {other:?}"),
    }
}

#[tokio::test]
async fn fetched_refs_dedupes_across_batches() {
    use git_remote_object_store::protocol::run;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, sha) = make_seed_repo();
    let bundle = bundle_ref(seed.path(), &sha, "refs/heads/main");

    let store = Arc::new(MockStore::new());
    let key = format!("repo/refs/heads/main/{sha}.bundle");
    store.insert(&key, bundle);

    let dst = make_dst_repo();
    let remote = s3_url(Some("repo"));
    let dst_path = dst.path().to_path_buf();

    // Drive the helper in stages within ONE `run()` call so both batches
    // share the same session-wide `FetchedRefs`. After batch 1 succeeds,
    // delete the bundle from the store. If dedup were broken, batch 2
    // would re-download the missing key and surface NotFound; the test
    // would then fail. Passing therefore proves the SHA was served from
    // `FetchedRefs`, not from a duplicate store call.
    let (client_side, helper_side) = tokio::io::duplex(64 * 1024);
    let (helper_in, helper_out) = tokio::io::split(helper_side);
    let (mut client_reader, mut client_writer) = tokio::io::split(client_side);

    let store_for_run: Arc<dyn ObjectStore> = Arc::clone(&store) as _;
    let run_task = tokio::spawn(async move {
        run(
            remote,
            store_for_run,
            BufReader::new(helper_in),
            helper_out,
            None,
            dst_path,
        )
        .await
    });

    // Batch 1.
    client_writer
        .write_all(format!("fetch {sha} refs/heads/main\n\n").as_bytes())
        .await
        .unwrap();
    let mut buf = [0u8; 1];
    client_reader.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"\n", "batch 1 should emit the terminator");

    // Drop the bundle so any re-fetch will fail.
    store
        .delete(&key)
        .await
        .expect("bundle must be present from setup");

    // Batch 2 — must short-circuit via `FetchedRefs`. If dedup is
    // broken the helper hits the (now-deleted) bundle key, returns
    // `FetchError::Store(NotFound)`, drops `helper_out`, and the
    // `read_exact` below sees EOF before any byte arrives. We wrap the
    // read in a short timeout so the failure mode is explicit (named
    // dedup regression, with the helper's actual error variant) rather
    // than a generic EOF panic that hides which invariant broke.
    client_writer
        .write_all(format!("fetch {sha} refs/heads/main\n\n").as_bytes())
        .await
        .unwrap();
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client_reader.read_exact(&mut buf),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(read_err)) => {
            // Helper aborted before writing batch 2's terminator. Drain
            // `run_task` so the panic message names the underlying
            // FetchError variant (typically `Store(NotFound)`).
            client_writer.shutdown().await.ok();
            let run_outcome = run_task.await;
            panic!(
                "batch 2 emitted no terminator (read error: {read_err}); run() outcome: \
                 {run_outcome:?} — dedup likely broken: helper attempted a forbidden re-fetch \
                 of the deleted bundle"
            );
        }
        Err(elapsed) => {
            panic!("batch 2 read timed out after {elapsed} — helper appears stuck")
        }
    }
    assert_eq!(&buf, b"\n", "batch 2 should emit the terminator");

    // Close stdin so `run()` returns.
    client_writer.shutdown().await.unwrap();
    let result = run_task.await.unwrap();
    result
        .expect("second batch must short-circuit via fetched_refs even though the bundle is gone");
}

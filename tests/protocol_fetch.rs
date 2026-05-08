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
            git_remote_object_store::url::StorageEngine::Bundle,
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

#[tokio::test]
async fn fetch_with_depth_writes_shallow_file_with_boundary() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    // Build a 3-commit linear history so depth=1 produces exactly one
    // boundary (the tip itself — it appears parentless in the shallow clone).
    let seed = tempfile::tempdir().expect("tempdir");
    git(&["init", "--quiet", "--initial-branch=main"], seed.path());
    git(&["config", "user.email", "test@example.com"], seed.path());
    git(&["config", "user.name", "Test"], seed.path());
    git(&["config", "commit.gpgsign", "false"], seed.path());
    for i in 0..3u32 {
        std::fs::write(seed.path().join("hello.txt"), format!("hi {i}\n")).unwrap();
        git(&["add", "hello.txt"], seed.path());
        git(
            &["commit", "--quiet", "-m", &format!("c{i}"), "--no-gpg-sign"],
            seed.path(),
        );
    }
    let tip_sha = git_capture(&["rev-parse", "HEAD"], seed.path())
        .trim()
        .to_owned();
    let bundle = bundle_ref(seed.path(), &tip_sha, "refs/heads/main");

    let store = MockStore::new();
    store.insert(format!("repo/refs/heads/main/{tip_sha}.bundle"), bundle);

    let dst = make_dst_repo();
    // option depth 1 -> ok; fetch tip -> shallow; blank -> flush.
    let script = format!("option depth 1\nfetch {tip_sha} refs/heads/main\n\n");
    let (out, result) = drive_in(
        s3_url(Some("repo")),
        Arc::new(store),
        &script,
        dst.path().to_path_buf(),
    )
    .await;
    result.expect("fetch with depth should succeed");
    // Helper output: `ok\n` for the option ack, then `\n` batch
    // terminator. No other bytes.
    assert_eq!(&out, b"ok\n\n", "expected option ack + terminator");

    let shallow_path = dst.path().join(".git").join("shallow");
    let shallow = std::fs::read_to_string(&shallow_path).expect("shallow file should exist");
    assert_eq!(
        shallow.trim(),
        tip_sha,
        "shallow boundary should be HEAD (tip appears parentless); got {shallow:?}"
    );
}

#[tokio::test]
async fn fetch_without_depth_does_not_write_shallow_file() {
    // Regression guard: a plain `git clone` with no `--depth` must not
    // create `.git/shallow`. This pins the per-batch reset of the depth
    // option in the REPL state.
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
    let (_out, result) = drive_in(
        s3_url(Some("repo")),
        Arc::new(store),
        &script,
        dst.path().to_path_buf(),
    )
    .await;
    result.expect("fetch should succeed");

    let shallow_path = dst.path().join(".git").join("shallow");
    assert!(
        !shallow_path.exists(),
        ".git/shallow unexpectedly created without `option depth`"
    );
}

#[tokio::test]
async fn fetch_with_depth_exceeding_history_does_not_write_shallow_file() {
    // When the requested depth is larger than the total number of commits
    // in the history, shallow_boundaries returns [] — the entire history
    // fits in the shallow clone without a boundary. fetch_batch must NOT
    // create .git/shallow in this case (an empty collected set short-circuits
    // write_shallow_file).
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, sha) = make_seed_repo(); // 1-commit history
    let bundle = bundle_ref(seed.path(), &sha, "refs/heads/main");

    let store = MockStore::new();
    store.insert(format!("repo/refs/heads/main/{sha}.bundle"), bundle);

    let dst = make_dst_repo();
    // depth=10 >> 1-commit history → no boundary → no shallow file
    let script = format!("option depth 10\nfetch {sha} refs/heads/main\n\n");
    let (_out, result) = drive_in(
        s3_url(Some("repo")),
        Arc::new(store),
        &script,
        dst.path().to_path_buf(),
    )
    .await;
    result.expect("fetch with depth exceeding history should succeed");

    let shallow_path = dst.path().join(".git").join("shallow");
    assert!(
        !shallow_path.exists(),
        ".git/shallow must not be created when depth exceeds total history length"
    );
}

#[tokio::test]
async fn depth_resets_between_batches() {
    // After a shallow batch completes, a subsequent batch in the same
    // REPL session must default to non-shallow semantics: depth is
    // per-operation, not session-sticky. We verify this by running
    // batch 1 with depth=1 (writes shallow), then batch 2 without
    // depth on a different ref (must NOT add to .git/shallow beyond
    // batch 1's contribution).
    use git_remote_object_store::protocol::run;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }

    // Seed: linear two-commit history for batch 1; an unrelated
    // single-commit branch for batch 2.
    let seed = tempfile::tempdir().expect("tempdir");
    git(&["init", "--quiet", "--initial-branch=main"], seed.path());
    git(&["config", "user.email", "test@example.com"], seed.path());
    git(&["config", "user.name", "Test"], seed.path());
    git(&["config", "commit.gpgsign", "false"], seed.path());
    std::fs::write(seed.path().join("a.txt"), b"a\n").unwrap();
    git(&["add", "a.txt"], seed.path());
    git(
        &["commit", "--quiet", "-m", "c0", "--no-gpg-sign"],
        seed.path(),
    );
    std::fs::write(seed.path().join("a.txt"), b"a2\n").unwrap();
    git(&["add", "a.txt"], seed.path());
    git(
        &["commit", "--quiet", "-m", "c1", "--no-gpg-sign"],
        seed.path(),
    );
    let tip_sha = git_capture(&["rev-parse", "HEAD"], seed.path())
        .trim()
        .to_owned();
    let bundle_main = bundle_ref(seed.path(), &tip_sha, "refs/heads/main");

    // Side branch with 2 commits so depth=1 from the tip produces a
    // non-empty boundary (side_sha itself). If depth leaked from batch 1,
    // `shallow_boundaries(repo, side_tip, 1)` would return [side_sha],
    // which would be merged into .git/shallow alongside tip_sha —
    // detectable via the final content assertion. An orphan (single-commit)
    // side branch would be invisible to the leak because it has no parents
    // and would produce an empty boundary either way.
    git(&["checkout", "--orphan", "side"], seed.path());
    git(&["rm", "-rf", "."], seed.path());
    std::fs::write(seed.path().join("b.txt"), b"b\n").unwrap();
    git(&["add", "b.txt"], seed.path());
    git(
        &["commit", "--quiet", "-m", "side0", "--no-gpg-sign"],
        seed.path(),
    );
    std::fs::write(seed.path().join("b.txt"), b"b2\n").unwrap();
    git(&["add", "b.txt"], seed.path());
    git(
        &["commit", "--quiet", "-m", "side1", "--no-gpg-sign"],
        seed.path(),
    );
    let side_sha = git_capture(&["rev-parse", "HEAD"], seed.path())
        .trim()
        .to_owned();
    let bundle_side = bundle_ref(seed.path(), &side_sha, "refs/heads/side");

    let store = Arc::new(MockStore::new());
    store.insert(
        format!("repo/refs/heads/main/{tip_sha}.bundle"),
        bundle_main,
    );
    store.insert(
        format!("repo/refs/heads/side/{side_sha}.bundle"),
        bundle_side,
    );

    let dst = make_dst_repo();
    let dst_path = dst.path().to_path_buf();
    let remote = s3_url(Some("repo"));

    let (client_side, helper_side) = tokio::io::duplex(64 * 1024);
    let (helper_in, helper_out) = tokio::io::split(helper_side);
    let (mut client_reader, mut client_writer) = tokio::io::split(client_side);

    let store_for_run: Arc<dyn ObjectStore> = Arc::clone(&store) as _;
    let run_task = tokio::spawn(async move {
        run(
            remote,
            store_for_run,
            git_remote_object_store::url::StorageEngine::Bundle,
            BufReader::new(helper_in),
            helper_out,
            None,
            dst_path,
        )
        .await
    });

    // Batch 1: shallow fetch of main.
    client_writer
        .write_all(format!("option depth 1\nfetch {tip_sha} refs/heads/main\n\n").as_bytes())
        .await
        .unwrap();
    // option ack: `ok\n`; batch terminator: `\n` -> 4 bytes.
    let mut buf = [0u8; 4];
    client_reader.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ok\n\n");

    // Batch 2: plain fetch of side. If depth leaked across batches,
    // .git/shallow would also contain side_sha (depth=1 from side_sha
    // returns [side_sha] as boundary). The reset semantic means batch 2
    // contributes nothing to .git/shallow.
    client_writer
        .write_all(format!("fetch {side_sha} refs/heads/side\n\n").as_bytes())
        .await
        .unwrap();
    let mut term = [0u8; 1];
    client_reader.read_exact(&mut term).await.unwrap();
    assert_eq!(&term, b"\n");

    client_writer.shutdown().await.unwrap();
    let _ = run_task.await.unwrap();

    let shallow_path = dst.path().join(".git").join("shallow");
    let shallow = std::fs::read_to_string(&shallow_path).expect("shallow file should exist");
    // The boundary set must be exactly `tip_sha` from batch 1.
    // Batch 2 contributing entries (or wiping batch 1's) would either
    // add side_sha to the file or remove tip_sha.
    assert_eq!(
        shallow.trim(),
        tip_sha,
        "shallow file polluted by depth-less batch 2: {shallow:?}",
    );
}

#[tokio::test]
async fn shallow_fetch_limits_visible_commit_count() {
    // End-to-end check: after a depth-N fetch, `git log` in the destination
    // repo must show exactly N commits. This verifies that the shallow
    // boundary written to .git/shallow correctly truncates history — not
    // just that the file was created with some content.
    const TOTAL_COMMITS: u32 = 5;
    const DEPTH: u32 = 3;

    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }

    let seed = tempfile::tempdir().expect("tempdir");
    git(&["init", "--quiet", "--initial-branch=main"], seed.path());
    git(&["config", "user.email", "test@example.com"], seed.path());
    git(&["config", "user.name", "Test"], seed.path());
    git(&["config", "commit.gpgsign", "false"], seed.path());
    for i in 0..TOTAL_COMMITS {
        std::fs::write(seed.path().join("f.txt"), format!("{i}\n")).unwrap();
        git(&["add", "f.txt"], seed.path());
        git(
            &["commit", "--quiet", "-m", &format!("c{i}"), "--no-gpg-sign"],
            seed.path(),
        );
    }
    let tip_sha = git_capture(&["rev-parse", "HEAD"], seed.path())
        .trim()
        .to_owned();
    let bundle = bundle_ref(seed.path(), &tip_sha, "refs/heads/main");

    let store = MockStore::new();
    store.insert(format!("repo/refs/heads/main/{tip_sha}.bundle"), bundle);

    let dst = make_dst_repo();
    let script = format!("option depth {DEPTH}\nfetch {tip_sha} refs/heads/main\n\n");
    let (_out, result) = drive_in(
        s3_url(Some("repo")),
        Arc::new(store),
        &script,
        dst.path().to_path_buf(),
    )
    .await;
    result.expect("shallow fetch should succeed");

    // The helper writes objects to the ODB but does not update refs — git
    // does that after the helper exits. Simulate it so `git log` works.
    git(&["update-ref", "refs/heads/main", &tip_sha], dst.path());

    // `git log --oneline` emits one line per visible commit; the count
    // must equal the requested depth.
    let log = git_capture(&["log", "--oneline", "refs/heads/main"], dst.path());
    let visible = log.lines().count();
    assert_eq!(
        visible, DEPTH as usize,
        "expected {DEPTH} visible commits after depth={DEPTH} fetch, got {visible}: {log:?}"
    );
}

// --- Annotated-tag fetch round-trip via bundle engine (issue #79) ---

#[tokio::test]
async fn bundle_fetch_round_trip_of_annotated_tag_resolves_tag_object() {
    // E10 bundle fetch side. Push an annotated tag through the bundle
    // engine, fetch into an empty dst, and confirm `cat-file -t v1`
    // returns `tag`. Without the fix, bundle's pack would only carry
    // commit-reachable objects and the destination's ref-update step
    // would have nothing to point at for the tag-OID.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _commit_sha, tag_sha) = common::make_seed_repo_with_annotated_tag("primary", "v1");

    let store = Arc::new(MockStore::new());
    let (_, push_result) = drive_in(
        s3_url(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/tags/v1:refs/tags/v1\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    push_result.expect("bundle annotated-tag push must succeed");

    let dst = tempfile::tempdir().expect("dst tempdir");
    git(&["init", "--quiet", "--initial-branch=main"], dst.path());
    git(&["config", "user.email", "test@example.com"], dst.path());
    git(&["config", "user.name", "Test"], dst.path());
    git(&["config", "commit.gpgsign", "false"], dst.path());

    let fetch_script = format!("fetch {tag_sha} refs/tags/v1\n\n");
    let (_, fetch_result) = drive_in(
        s3_url(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        &fetch_script,
        dst.path().to_path_buf(),
    )
    .await;
    fetch_result.expect("bundle fetch of tag must succeed");

    let kind = std::process::Command::new("git")
        .args(["cat-file", "-t", &tag_sha])
        .current_dir(dst.path())
        .output()
        .expect("spawn git cat-file");
    assert!(
        kind.status.success(),
        "git cat-file -t failed: {}",
        String::from_utf8_lossy(&kind.stderr),
    );
    assert_eq!(
        String::from_utf8(kind.stdout).unwrap().trim(),
        "tag",
        "tag OID must decode as a tag object after bundle fetch round-trip",
    );
}

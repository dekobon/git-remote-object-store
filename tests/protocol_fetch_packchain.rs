//! Fetch integration test for the packchain engine: drive
//! [`protocol::run`] through `fetch` batches with `?engine=packchain`
//! against a [`MockStore`] that was pre-populated by a real
//! Phase 2 push. Each scenario pushes from a *seed* repo to the
//! mock, then fetches into a *dst* repo and asserts the objects
//! landed (or that the appropriate typed error fires).
//!
//! Mirrors `tests/protocol_fetch.rs`'s structure for the bundle
//! engine. Cross-engine differences (chain.json read, sequential
//! pack install, shallow-fetch divergence) are explicit per scenario.

#![cfg(feature = "test-util")]

mod common;

use std::sync::Arc;

use bytes::Bytes;
use git_remote_object_store::PackchainError;
use git_remote_object_store::object_store::ObjectStore;
use git_remote_object_store::object_store::mock::MockStore;
use git_remote_object_store::protocol::ProtocolError;
use git_remote_object_store::protocol::fetch::FetchError;

use common::{drive_in, git, git_available, git_capture, make_seed_repo, s3_url_packchain};

/// Run a `git cat-file -e <sha>` to confirm the object is reachable
/// in the destination repo's ODB. Returns `true` on success.
fn dst_has_object(dst: &std::path::Path, sha: &str) -> bool {
    let output = std::process::Command::new("git")
        .args(["cat-file", "-e", sha])
        .current_dir(dst)
        .output()
        .expect("spawn git cat-file");
    output.status.success()
}

/// Initialise a fresh empty repo for use as a fetch destination. The
/// helper protocol writes pack files into `.git/objects/pack`; the
/// dst repo must have a `.git/` directory.
fn make_empty_dst() -> tempfile::TempDir {
    let dst = tempfile::tempdir().expect("dst tempdir");
    git(&["init", "--quiet", "--initial-branch=main"], dst.path());
    git(&["config", "user.email", "test@example.com"], dst.path());
    git(&["config", "user.name", "Test"], dst.path());
    git(&["config", "commit.gpgsign", "false"], dst.path());
    dst
}

/// Push `n` commits from a fresh seed repo into `store` via the
/// packchain engine, returning `(seed_dir, vec_of_shas)`. Each push
/// is a separate REPL batch, so the resulting chain.json has `n`
/// segments.
async fn push_n_commits_into(
    store: &Arc<MockStore>,
    n: usize,
    salt: &str,
) -> (tempfile::TempDir, Vec<String>) {
    let (seed, shas) = make_seed_repo(n, salt);
    // make_seed_repo creates n commits in one repo, but they're
    // committed sequentially. To put each commit into its own
    // chain.json segment, push after each commit. Simpler approach:
    // push once at the end (one segment containing all n commits).
    // For tests that need *multiple* segments, the caller pushes
    // again after a follow-up commit. Here we just do the single
    // push so callers compose.
    let (_, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push must succeed");
    (seed, shas)
}

#[tokio::test]
async fn fetch_into_empty_repo_after_first_push_lands_tip_object() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let (_seed, shas) = push_n_commits_into(&store, 1, "primary").await;
    let tip = &shas[0];

    let dst = make_empty_dst();
    let fetch_script = format!("fetch {tip} refs/heads/main\n\n");
    let (_out, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        &fetch_script,
        dst.path().to_path_buf(),
    )
    .await;
    result.expect("packchain fetch into empty dst must succeed");
    assert!(
        dst_has_object(dst.path(), tip),
        "tip {tip} must be reachable in dst after fetch",
    );
}

/// Full clone of a 2-segment chain into an empty dst: walk both
/// segments + baseline and assert every commit lands in the
/// destination ODB. The test name was previously
/// `incremental_fetch_only_downloads_new_segment`, but the body
/// fetches into an EMPTY dst (which forces the full-chain walk —
/// not the chain-walk-dedup path that "incremental" implied).
/// Renamed to match what the body actually verifies; a separate
/// test would be needed to pin the chain-walk-dedup branch
/// (pre-populate dst, fetch new tip, assert older pack is not
/// re-downloaded).
#[tokio::test]
async fn clone_walks_two_segment_chain_and_lands_both_commits() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas1) = make_seed_repo(1, "primary");
    let store = Arc::new(MockStore::new());

    // First push.
    let (_, r1) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r1.expect("first push");
    let tip_1 = &shas1[0];

    // Second commit + push (creates segments[0] in chain.json).
    std::fs::write(seed.path().join("f1.txt"), b"second\n").unwrap();
    git(&["add", "."], seed.path());
    git(
        &["commit", "--quiet", "-m", "step2", "--no-gpg-sign"],
        seed.path(),
    );
    let tip_2 = git_capture(&["rev-parse", "HEAD"], seed.path())
        .trim()
        .to_owned();
    let (_, r2) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r2.expect("second push");

    // Fetch tip_2 from a fresh dst (must walk full chain).
    let dst = make_empty_dst();
    let fetch_script_2 = format!("fetch {tip_2} refs/heads/main\n\n");
    let (_out, fetch_result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        &fetch_script_2,
        dst.path().to_path_buf(),
    )
    .await;
    fetch_result.expect("fetch tip_2 into empty dst");
    assert!(dst_has_object(dst.path(), tip_1), "tip_1 reachable");
    assert!(dst_has_object(dst.path(), &tip_2), "tip_2 reachable");
}

/// Two `fetch` batches in one REPL session must dedup via the
/// session-wide `FetchedRefs` cache: the second batch's
/// `fetch_one` short-circuits on `FetchedRefs::contains(tip)`
/// before it would otherwise re-read `chain.json` and re-download
/// packs.
///
/// Pin this by deleting `chain.json` between batch 1 and batch 2
/// in the SAME REPL run (mirrors the bundle engine's
/// `fetched_refs_dedupes_across_batches` test). If dedup works,
/// the second batch never reads the (now-missing) chain.json —
/// `Ok` is reported. If dedup is broken, batch 2 reads chain.json,
/// hits `NotFound`, surfaces `ChainAbsent`, and the test fails.
///
/// The previous version (`fetch_dedup_within_session_skips_known_tip`)
/// used a single batch with the same SHA twice. That shape races
/// inside `JoinSet` rather than dedup-ing — both tasks check
/// `FetchedRefs` before either inserts. Mutation testing
/// confirmed that version passed even when the
/// `FetchedRefs::contains` check was bypassed.
#[tokio::test]
async fn fetched_refs_dedupes_across_batches() {
    use git_remote_object_store::protocol::run;
    use git_remote_object_store::url::StorageEngine;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let (_seed, shas) = push_n_commits_into(&store, 1, "primary").await;
    let tip = &shas[0];
    let chain_key_owned = "repo/refs/heads/main/chain.json".to_owned();

    let dst = make_empty_dst();
    let dst_path = dst.path().to_path_buf();
    let remote = s3_url_packchain(Some("repo"));
    let store_for_run: Arc<dyn ObjectStore> = Arc::clone(&store) as _;

    let (client_side, helper_side) = tokio::io::duplex(64 * 1024);
    let (helper_in, helper_out) = tokio::io::split(helper_side);
    let (mut client_reader, mut client_writer) = tokio::io::split(client_side);

    let run_task = tokio::spawn(async move {
        run(
            remote,
            store_for_run,
            StorageEngine::Packchain,
            BufReader::new(helper_in),
            helper_out,
            None,
            dst_path,
        )
        .await
    });

    // Batch 1 — happy path.
    client_writer
        .write_all(format!("fetch {tip} refs/heads/main\n\n").as_bytes())
        .await
        .unwrap();
    let mut buf = [0u8; 1];
    client_reader.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"\n", "batch 1 should emit the terminator");

    // Delete chain.json. Any re-read in batch 2 will surface
    // ChainAbsent if FetchedRefs dedup is broken.
    store
        .delete(&chain_key_owned)
        .await
        .expect("chain.json must be present from setup");

    // Batch 2 — must short-circuit via FetchedRefs.
    client_writer
        .write_all(format!("fetch {tip} refs/heads/main\n\n").as_bytes())
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
            client_writer.shutdown().await.ok();
            let run_outcome = run_task.await;
            panic!(
                "batch 2 emitted no terminator (read error: {read_err}); run() outcome: \
                 {run_outcome:?} — dedup likely broken: helper attempted a forbidden \
                 re-read of the deleted chain.json"
            );
        }
        Err(elapsed) => {
            panic!("batch 2 read timed out after {elapsed} — helper appears stuck")
        }
    }
    assert_eq!(&buf, b"\n", "batch 2 should emit the terminator");

    // Close stdin so run() returns; the helper's outcome must be Ok.
    client_writer.shutdown().await.unwrap();
    let result = run_task.await.unwrap();
    result.expect("second batch must short-circuit via FetchedRefs even though chain.json is gone");
    assert!(dst_has_object(dst.path(), tip), "tip must remain reachable");
}

#[tokio::test]
async fn fetch_from_empty_bucket_surfaces_chain_absent_error() {
    // No push happened. The fetch must surface the typed
    // `PackchainError::ChainAbsent` rather than an opaque
    // `Store(NotFound)`. This is the regression-loud-fail criterion
    // from issue #64.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let dst = make_empty_dst();
    let bogus_sha = "1111111111111111111111111111111111111111";
    let fetch_script = format!("fetch {bogus_sha} refs/heads/main\n\n");
    let (_out, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        &fetch_script,
        dst.path().to_path_buf(),
    )
    .await;
    let err = result.expect_err("fetch from empty bucket must error");
    assert!(
        matches!(
            err,
            ProtocolError::Fetch(FetchError::Packchain(PackchainError::ChainAbsent { .. }))
        ),
        "expected ChainAbsent, got {err:?}",
    );
}

#[tokio::test]
async fn fetch_surfaces_pack_missing_when_chain_references_absent_pack() {
    // Pre-seed a chain.json that points at a pack key the bucket
    // doesn't have. Fetch must produce `PackMissing` so an operator
    // sees which artefact is missing — issue #64's "fail loud" claim.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let bogus_tip = "1111111111111111111111111111111111111111";
    let bogus_pack = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let chain_body = serde_json::json!({
        "v": 1,
        "tip": bogus_tip,
        "full_at": bogus_tip,
        "segments": [{
            "sha": bogus_tip,
            "parent_sha": serde_json::Value::Null,
            "pack": format!("packs/{bogus_pack}.pack"),
            "bytes": 1024,
        }],
    });
    store.insert(
        "repo/refs/heads/main/chain.json",
        Bytes::from(serde_json::to_vec(&chain_body).unwrap()),
    );
    store.insert("repo/FORMAT", Bytes::from_static(b"packchain"));

    let dst = make_empty_dst();
    let fetch_script = format!("fetch {bogus_tip} refs/heads/main\n\n");
    let (_out, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        &fetch_script,
        dst.path().to_path_buf(),
    )
    .await;
    let err = result.expect_err("missing pack key must error");
    match err {
        ProtocolError::Fetch(FetchError::Packchain(PackchainError::PackMissing { key })) => {
            assert!(
                key.contains(bogus_pack),
                "error key must name the missing pack, got {key}",
            );
        }
        other => panic!("expected PackMissing, got {other:?}"),
    }
}

#[tokio::test]
async fn shallow_fetch_depth_one_skips_baseline_download() {
    // option depth 1 → after the first segment install + BFS-walk,
    // the boundary set is non-empty (tip itself), so packchain
    // shallow fetch must stop without downloading the baseline.
    // Pin that explicitly with a fault on the baseline key: if the
    // shallow path correctly skips the baseline, the fault stays
    // pending; if a regression always downloads the baseline, the
    // fault fires and the fetch errors.
    //
    // Mutation-tested: forcing the BFS short-circuit off (so the
    // shallow path falls through to the baseline) makes the fault
    // fire and this test fails.
    use git_remote_object_store::object_store::mock::Fault;

    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let (_seed, shas) = push_n_commits_into(&store, 1, "primary").await;
    let tip = &shas[0];
    let baseline_key = format!("repo/refs/heads/main/{tip}.bundle");

    // Arm the fault BEFORE the fetch. PreconditionFailedOnGetToFile
    // fires once when get_to_file is called for the baseline key,
    // surfacing as a Store(PreconditionFailed) error that breaks
    // the fetch. With shallow correctly skipping the baseline, the
    // fault stays armed.
    store.arm(Fault::PreconditionFailedOnGetToFile {
        key: baseline_key.clone(),
    });

    let dst = make_empty_dst();
    // The helper-protocol order: option depth comes BEFORE the fetch
    // batch.
    let fetch_script = format!("option depth 1\nfetch {tip} refs/heads/main\n\n");
    let (_out, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        &fetch_script,
        dst.path().to_path_buf(),
    )
    .await;
    result.expect("shallow fetch must succeed without touching baseline");
    assert!(
        dst_has_object(dst.path(), tip),
        "shallow fetch must land the tip",
    );
    // .git/shallow must exist after a shallow fetch.
    let shallow_path = dst.path().join(".git/shallow");
    assert!(
        shallow_path.exists(),
        ".git/shallow must be created on shallow fetch",
    );
    // The baseline-download fault must remain pending — the shallow
    // path stopped after segment[0] + BFS, never reaching the
    // baseline-download branch.
    assert_eq!(
        store.pending_faults(),
        1,
        "shallow fetch must NOT download the baseline at depth=1",
    );
}

//! Push integration test for the packchain engine: drive
//! [`protocol::run`] through push batches with `?engine=packchain`
//! against a [`MockStore`] and a real local git repo, and verify the
//! Phase 2 on-bucket layout (chain.json, path-index.json, packs/<sha>.{pack,idx},
//! `<tip>.bundle` baseline, `FORMAT`, `HEAD`) byte-for-byte.
//!
//! Mirrors `tests/protocol_push.rs`'s structure for the bundle engine
//! so the wire-output assertions stay parallel; differences in the
//! per-engine artefacts are explicit (no `multiple bundles exist`
//! sibling — packchain's chain.json replaces it).

#![cfg(feature = "test-util")]

mod common;

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use git_remote_object_store::object_store::mock::MockStore;
use git_remote_object_store::object_store::{ObjectStore, PutOpts};
use serde_json::Value;
use time::Duration;
use time::OffsetDateTime;

use common::{drive_in, git, git_available, git_capture, make_seed_repo, s3_url_packchain};

/// Read and parse `<prefix>/refs/heads/main/chain.json` from the mock.
fn read_chain(store: &MockStore, prefix: &str) -> Value {
    let key = format!("{prefix}/refs/heads/main/chain.json");
    let bytes = futures::executor::block_on(store.get_bytes(&key)).expect("chain.json must exist");
    serde_json::from_slice(&bytes).expect("chain.json must be valid JSON")
}

/// Sanity-check that `path-index.json` exists and parses.
fn read_path_index(store: &MockStore, prefix: &str) -> Value {
    let key = format!("{prefix}/refs/heads/main/path-index.json");
    let bytes =
        futures::executor::block_on(store.get_bytes(&key)).expect("path-index.json must exist");
    serde_json::from_slice(&bytes).expect("path-index.json must be valid JSON")
}

#[tokio::test]
async fn first_push_writes_pack_idx_baseline_chain_path_index_format_head() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas) = make_seed_repo(1, "primary");
    let tip = &shas[0];

    let store = Arc::new(MockStore::new());
    let (out, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("packchain push should succeed");
    assert_eq!(
        std::str::from_utf8(&out).unwrap(),
        "ok refs/heads/main\n\n",
        "wire output: ok line + terminator",
    );

    // FORMAT and HEAD seeded.
    let format_body = futures::executor::block_on(store.get_bytes("repo/FORMAT")).unwrap();
    assert_eq!(&format_body[..], b"packchain", "FORMAT body");
    let head_body = futures::executor::block_on(store.get_bytes("repo/HEAD")).unwrap();
    assert_eq!(&head_body[..], b"refs/heads/main", "HEAD body");

    // Baseline bundle keyed by tip SHA exists.
    let baseline_key = format!("repo/refs/heads/main/{tip}.bundle");
    assert!(
        store.contains(&baseline_key),
        "baseline bundle missing at {baseline_key}",
    );

    // chain.json shape: tip, full_at = tip, single segment with parent=null.
    let chain = read_chain(&store, "repo");
    assert_eq!(chain["v"], 1, "chain.json schema version");
    assert_eq!(chain["tip"], *tip);
    assert_eq!(chain["full_at"], *tip, "first push: full_at == tip");
    let segments = chain["segments"].as_array().expect("segments array");
    assert_eq!(segments.len(), 1, "first push: exactly one segment");
    assert_eq!(segments[0]["sha"], *tip);
    assert!(
        segments[0]["parent_sha"].is_null(),
        "first-push segment must have null parent_sha, got {:?}",
        segments[0]["parent_sha"],
    );
    let pack_path = segments[0]["pack"]
        .as_str()
        .expect("segment.pack is a string");
    assert!(pack_path.starts_with("packs/"), "segment.pack: {pack_path}");
    // Case-sensitive `.pack` suffix is the wire-format contract; we
    // *want* lower-case here, not a tolerant comparison.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    {
        assert!(pack_path.ends_with(".pack"), "segment.pack: {pack_path}");
    }

    // The pack itself + its idx exist on the bucket.
    let pack_key = format!("repo/{pack_path}");
    assert!(
        store.contains(&pack_key),
        "pack object missing at {pack_key}"
    );
    let idx_key = format!("repo/{}", pack_path.replace(".pack", ".idx"));
    assert!(store.contains(&idx_key), "idx object missing at {idx_key}");

    // path-index.json reflects the commit. The seed repo wrote `f0.txt`,
    // so path-index.tree must contain that filename — a regression that
    // produced an empty tree (e.g. extract_path_index skipping the
    // walk) would let `tree.is_object()` alone pass vacuously.
    let path_index = read_path_index(&store, "repo");
    assert_eq!(path_index["v"], 1);
    assert_eq!(path_index["commit"], *tip);
    let tree = path_index["tree"]
        .as_object()
        .expect("tree must be a JSON object");
    assert!(
        tree.contains_key("f0.txt"),
        "path-index tree must include the seed file f0.txt, got keys: {:?}",
        tree.keys().collect::<Vec<_>>(),
    );

    // Lock released.
    assert!(!store.contains("repo/refs/heads/main/LOCK#.lock"));
}

#[tokio::test]
async fn incremental_push_appends_segment_newest_first() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _initial_shas) = make_seed_repo(1, "primary");
    let store = Arc::new(MockStore::new());

    // First push: seeds chain.json with one segment.
    let (_, r1) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r1.expect("first push must succeed");
    let chain_after_1 = read_chain(&store, "repo");
    let tip_1 = chain_after_1["tip"].as_str().unwrap().to_owned();
    assert_eq!(chain_after_1["segments"].as_array().unwrap().len(), 1);

    // Add another commit locally and push again.
    std::fs::write(seed.path().join("f1.txt"), b"second\n").unwrap();
    git(&["add", "."], seed.path());
    git(
        &["commit", "--quiet", "-m", "step2", "--no-gpg-sign"],
        seed.path(),
    );
    let tip_2 = git_capture(&["rev-parse", "HEAD"], seed.path())
        .trim()
        .to_owned();
    assert_ne!(tip_1, tip_2);

    let (out2, r2) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r2.expect("incremental push must succeed");
    assert_eq!(
        std::str::from_utf8(&out2).unwrap(),
        "ok refs/heads/main\n\n"
    );

    let chain_after_2 = read_chain(&store, "repo");
    assert_eq!(chain_after_2["tip"], tip_2, "tip moved to new commit");
    assert_eq!(
        chain_after_2["full_at"], tip_1,
        "full_at preserved (no force / first push)",
    );
    let segments = chain_after_2["segments"].as_array().unwrap();
    assert_eq!(segments.len(), 2, "incremental adds one new segment");
    // Newest-first: segments[0] is the new push.
    assert_eq!(segments[0]["sha"], tip_2, "segments[0].sha = new tip");
    assert_eq!(
        segments[0]["parent_sha"], tip_1,
        "segments[0].parent_sha = prior tip",
    );
    assert_eq!(segments[1]["sha"], tip_1, "segments[1] = prior segment");

    // Both packs referenced by chain.json must actually exist on the
    // bucket — without this, a regression that skipped the incremental
    // pack upload would leave chain.json pointing at a missing object
    // and the segment-shape assertions above would still pass.
    for (idx, seg) in segments.iter().enumerate() {
        let pack = seg["pack"].as_str().expect("segment.pack");
        assert!(
            store.contains(&format!("repo/{pack}")),
            "segments[{idx}].pack must exist at repo/{pack}",
        );
    }

    // Path-index reflects new tip.
    let path_index = read_path_index(&store, "repo");
    assert_eq!(path_index["commit"], tip_2);
}

#[tokio::test]
async fn force_push_collapses_segments_and_replaces_baseline() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _shas) = make_seed_repo(2, "primary");
    let tip_1 = git_capture(&["rev-parse", "HEAD~1"], seed.path())
        .trim()
        .to_owned();
    let tip_2 = git_capture(&["rev-parse", "HEAD"], seed.path())
        .trim()
        .to_owned();

    let store = Arc::new(MockStore::new());
    // First push (linear from tip_1 → tip_2).
    let (_, r1) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r1.expect("first push");
    let baseline_key_2 = format!("repo/refs/heads/main/{tip_2}.bundle");
    assert!(store.contains(&baseline_key_2));

    // Reset HEAD to tip_1, add a divergent commit, force-push.
    git(&["reset", "--hard", &tip_1], seed.path());
    std::fs::write(seed.path().join("divergent.txt"), b"x\n").unwrap();
    git(&["add", "."], seed.path());
    git(
        &["commit", "--quiet", "-m", "diverge", "--no-gpg-sign"],
        seed.path(),
    );
    let tip_diverge = git_capture(&["rev-parse", "HEAD"], seed.path())
        .trim()
        .to_owned();
    assert_ne!(tip_diverge, tip_2);

    let (out, r2) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push +refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r2.expect("force push");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/heads/main\n\n");

    let chain = read_chain(&store, "repo");
    assert_eq!(chain["tip"], tip_diverge);
    assert_eq!(
        chain["full_at"], tip_diverge,
        "force push resets full_at to new tip",
    );
    let segments = chain["segments"].as_array().unwrap();
    assert_eq!(segments.len(), 1, "force push collapses to one segment");
    assert_eq!(segments[0]["sha"], tip_diverge);
    assert!(segments[0]["parent_sha"].is_null());

    // The pack referenced by chain.json must actually exist on the
    // bucket. Without this check, a regression that skipped the pack
    // upload on the force-push path would leave chain.json pointing
    // at a missing object, but the test would still pass.
    let segment_pack = segments[0]["pack"].as_str().expect("segment.pack");
    assert!(
        store.contains(&format!("repo/{segment_pack}")),
        "force-push pack referenced by chain.json must exist at repo/{segment_pack}",
    );

    // New baseline at the diverge tip exists; old baseline at tip_2 is gone.
    let new_baseline = format!("repo/refs/heads/main/{tip_diverge}.bundle");
    assert!(store.contains(&new_baseline));
    assert!(
        !store.contains(&baseline_key_2),
        "force push must delete prior baseline at old full_at",
    );
}

#[tokio::test]
async fn non_force_push_rejects_when_remote_not_ancestor() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas) = make_seed_repo(1, "primary");
    let local_tip = &shas[0];

    // Synthesise an unrelated remote tip via a separate seed repo.
    let (other_seed, other_shas) = make_seed_repo(1, "alt");
    let unrelated_tip = &other_shas[0];
    assert_ne!(local_tip, unrelated_tip);
    drop(other_seed);

    // Pre-seed chain.json with the unrelated tip — pretend a prior
    // pusher pushed unrelated history.
    let store = Arc::new(MockStore::new());
    let chain_body = serde_json::json!({
        "v": 1,
        "tip": unrelated_tip,
        "full_at": unrelated_tip,
        "segments": [],
    });
    store.insert(
        "repo/refs/heads/main/chain.json",
        Bytes::from(serde_json::to_vec(&chain_body).unwrap()),
    );
    // FORMAT must agree with the URL's engine query so validate_format
    // does not reject up front.
    store.insert("repo/FORMAT", Bytes::from_static(b"packchain"));

    let (out, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push should refuse, not abort");
    let text = std::str::from_utf8(&out).unwrap();
    assert!(
        text.starts_with("error refs/heads/main "),
        "expected refusal line, got {text:?}",
    );
    assert!(text.contains("not ancestor"), "got {text:?}");
}

#[tokio::test]
async fn lock_contention_returns_error_outcome() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _) = make_seed_repo(1, "primary");
    let store = Arc::new(MockStore::new());
    // Fresh lock held by another client.
    store.insert_with(
        "repo/refs/heads/main/LOCK#.lock",
        Bytes::new(),
        OffsetDateTime::now_utc(),
        PutOpts::default(),
    );

    let (out, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push should refuse, not abort");
    let text = std::str::from_utf8(&out).unwrap();
    assert!(text.contains("failed to acquire ref lock"), "got {text:?}");
    // Lock untouched (not ours to release).
    assert!(store.contains("repo/refs/heads/main/LOCK#.lock"));
}

#[tokio::test]
async fn stale_lock_is_recovered() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _shas) = make_seed_repo(1, "primary");
    let store = Arc::new(MockStore::new());
    // Lock held by a long-dead client (older than default 60s TTL).
    store.insert_with(
        "repo/refs/heads/main/LOCK#.lock",
        Bytes::new(),
        OffsetDateTime::now_utc() - Duration::seconds(120),
        PutOpts::default(),
    );

    let (out, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push should succeed via stale-lock recovery");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/heads/main\n\n");
    assert!(store.contains("repo/refs/heads/main/chain.json"));
}

#[tokio::test]
async fn idempotent_same_sha_push_short_circuits() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _shas) = make_seed_repo(1, "primary");
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
    let chain_v1 = read_chain(&store, "repo");
    let pack_count_v1 = store
        .keys()
        .into_iter()
        .filter(|k| k.starts_with("repo/packs/"))
        .count();

    // Push the same SHA again.
    let (out, r2) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r2.expect("idempotent push");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/heads/main\n\n");

    let chain_v2 = read_chain(&store, "repo");
    assert_eq!(chain_v1, chain_v2, "chain.json unchanged");
    let pack_count_v2 = store
        .keys()
        .into_iter()
        .filter(|k| k.starts_with("repo/packs/"))
        .count();
    assert_eq!(
        pack_count_v1, pack_count_v2,
        "no new pack files written on a same-SHA push",
    );
}

#[tokio::test]
async fn format_mismatch_rejected_at_connect_time() {
    // A bucket whose FORMAT key says `bundle` cannot be pushed to with
    // `?engine=packchain`. `validate_format` must reject before the
    // protocol REPL loop starts. Drive `validate_format` directly here
    // so the assertion is on a typed `BackendError::EngineMismatch`,
    // not on a panic that bubbled out of a test-harness `.expect`.
    use git_remote_object_store::protocol::backend::{self, BackendError};
    use git_remote_object_store::url::StorageEngine;

    let store = Arc::new(MockStore::new());
    store.insert("repo/FORMAT", Bytes::from_static(b"bundle"));

    let err = backend::validate_format(store.as_ref(), "repo", Some(StorageEngine::Packchain))
        .await
        .expect_err("format mismatch must surface as BackendError");
    assert!(
        matches!(err, BackendError::EngineMismatch { .. }),
        "expected EngineMismatch, got {err:?}",
    );
}

/// Drive a delete refspec (`:<remote_ref>`) against an existing
/// packchain branch and assert the chain.json + path-index are gone.
#[tokio::test]
async fn delete_remote_ref_removes_chain_and_path_index() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _) = make_seed_repo(1, "primary");
    let store = Arc::new(MockStore::new());

    // Push first so there's something to delete.
    let (_, r1) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r1.expect("first push");
    assert!(store.contains("repo/refs/heads/main/chain.json"));
    assert!(store.contains("repo/refs/heads/main/path-index.json"));

    // Now delete.
    let (out, r2) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push :refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r2.expect("delete push");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/heads/main\n\n");
    assert!(
        !store.contains("repo/refs/heads/main/chain.json"),
        "chain.json must be deleted",
    );
    assert!(
        !store.contains("repo/refs/heads/main/path-index.json"),
        "path-index.json must be deleted",
    );

    // Pack files are NOT deleted — Phase 5 GC reaps them.
    let pack_keys: Vec<_> = store
        .keys()
        .into_iter()
        .filter(|k| k.starts_with("repo/packs/"))
        .collect();
    assert!(
        !pack_keys.is_empty(),
        "pack files must remain after delete (orphans reaped by Phase 5 GC)",
    );
}

/// Pin that the `?engine=packchain` URL with a fresh bucket persists
/// `FORMAT="packchain"` so a subsequent connect can never silently
/// route a packchain bucket through the bundle engine.
#[tokio::test]
async fn first_push_pins_format_marker() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _) = make_seed_repo(1, "primary");
    let store = Arc::new(MockStore::new());

    let (_, r1) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r1.expect("first push");
    let body = futures::executor::block_on(store.get_bytes("repo/FORMAT")).unwrap();
    assert_eq!(&body[..], b"packchain");
}

/// Force-push old-baseline cleanup is best-effort: the push commits at
/// the chain.json write, and a delete failure on the prior baseline
/// must NOT fail the push (it would leave a confusing wire output for
/// what is already a successful push). The orphan baseline is reaped
/// by Phase 5 GC.
#[tokio::test]
async fn force_push_baseline_cleanup_failure_does_not_fail_push() {
    use git_remote_object_store::object_store::mock::Fault;
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _shas) = make_seed_repo(2, "primary");
    let tip_1 = git_capture(&["rev-parse", "HEAD~1"], seed.path())
        .trim()
        .to_owned();
    let tip_2 = git_capture(&["rev-parse", "HEAD"], seed.path())
        .trim()
        .to_owned();

    let store = Arc::new(MockStore::new());
    let (_, r1) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r1.expect("first push");
    let baseline_key_2 = format!("repo/refs/heads/main/{tip_2}.bundle");
    assert!(store.contains(&baseline_key_2));

    // Diverge and force-push.
    git(&["reset", "--hard", &tip_1], seed.path());
    std::fs::write(seed.path().join("divergent.txt"), b"x\n").unwrap();
    git(&["add", "."], seed.path());
    git(
        &["commit", "--quiet", "-m", "diverge", "--no-gpg-sign"],
        seed.path(),
    );

    // Arm a delete fault on the prior baseline key so the post-commit
    // cleanup at the end of `perform_push_under_lock` fails. The push
    // must still succeed (chain.json has already been written).
    store.arm(Fault::NetworkOnDelete {
        key: baseline_key_2.clone(),
    });

    let (out, r2) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push +refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r2.expect("force push must not fail on baseline-cleanup error");
    assert_eq!(
        std::str::from_utf8(&out).unwrap(),
        "ok refs/heads/main\n\n",
        "wire output must still be `ok` even though baseline cleanup failed",
    );
    // The orphan baseline survives — Phase 5 GC's job to reap it.
    assert!(
        store.contains(&baseline_key_2),
        "old baseline must remain when delete fault prevented cleanup",
    );
    // The fault was consumed (one delete attempt was made and rejected).
    assert_eq!(store.pending_faults(), 0);
}

/// Concurrent pushes to *different* refs in the same bucket: each
/// gets its own per-ref lock and chain.json, but FORMAT and HEAD are
/// shared via `put_if_absent` — first writer wins, second is a no-op.
/// A regression that wrote FORMAT/HEAD with `put_bytes` (overwrite
/// semantics) would not be caught by the single-ref tests because both
/// pushes write the same value; this test pins the `put_if_absent`
/// path by checking HEAD reflects the *first* ref pushed.
#[tokio::test]
async fn concurrent_different_refs_share_format_and_head() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _) = make_seed_repo(1, "primary");
    let store = Arc::new(MockStore::new());

    // First push: refs/heads/main.
    let (_, r1) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r1.expect("first ref push");
    let head_after_main = futures::executor::block_on(store.get_bytes("repo/HEAD")).unwrap();
    assert_eq!(&head_after_main[..], b"refs/heads/main");

    // Create a second branch locally and push it.
    git(&["branch", "dev"], seed.path());
    let (_, r2) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/dev:refs/heads/dev\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r2.expect("second ref push");

    // FORMAT is `packchain` exactly once.
    let format_keys: Vec<_> = store
        .keys()
        .into_iter()
        .filter(|k| k == "repo/FORMAT")
        .collect();
    assert_eq!(format_keys.len(), 1, "FORMAT must be a single key");
    let format_body = futures::executor::block_on(store.get_bytes("repo/FORMAT")).unwrap();
    assert_eq!(&format_body[..], b"packchain");

    // HEAD still reflects the *first* ref pushed (put_if_absent
    // semantics — a regression that switched to put_bytes would
    // overwrite to refs/heads/dev here).
    let head_after_dev = futures::executor::block_on(store.get_bytes("repo/HEAD")).unwrap();
    assert_eq!(
        &head_after_dev[..],
        b"refs/heads/main",
        "HEAD must remain at the first ref pushed (put_if_absent semantics)",
    );

    // Each ref has its own chain.json.
    assert!(store.contains("repo/refs/heads/main/chain.json"));
    assert!(store.contains("repo/refs/heads/dev/chain.json"));
}

// Currently unused but kept for symmetry with the bundle test file's
// helper surface — Phase 3 fetch tests will use it.
#[allow(dead_code)]
fn touch(path: &Path) {
    std::fs::write(path, b"").unwrap();
}

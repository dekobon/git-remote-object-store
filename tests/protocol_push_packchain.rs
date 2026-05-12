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

use common::{
    drive_in, git, git_available, git_capture, make_seed_repo, make_seed_repo_with_annotated_tag,
    make_seed_repo_with_tag_of_tag, s3_url_packchain,
};

/// Read and parse `<prefix>/refs/heads/main/chain.json` from the mock.
fn read_chain(store: &MockStore, prefix: &str) -> Value {
    read_chain_for(store, prefix, "refs/heads/main")
}

/// Read and parse `<prefix>/<ref_name>/chain.json` from the mock —
/// generalisation of [`read_chain`] for tag-ref tests.
fn read_chain_for(store: &MockStore, prefix: &str, ref_name: &str) -> Value {
    let key = format!("{prefix}/{ref_name}/chain.json");
    let bytes = futures::executor::block_on(store.get_bytes(&key)).expect("chain.json must exist");
    serde_json::from_slice(&bytes).expect("chain.json must be valid JSON")
}

/// Sanity-check that `path-index.json` exists and parses.
fn read_path_index(store: &MockStore, prefix: &str) -> Value {
    read_path_index_for(store, prefix, "refs/heads/main")
}

/// Generalisation of [`read_path_index`] for tests that push to refs
/// other than `refs/heads/main` (tag refs, notes refs, ...).
fn read_path_index_for(store: &MockStore, prefix: &str, ref_name: &str) -> Value {
    let key = format!("{prefix}/{ref_name}/path-index.json");
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
    assert_eq!(path_index["v"], 2);
    assert_eq!(path_index["tip"], *tip);
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
    assert_eq!(path_index["tip"], tip_2);
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

    // New baseline at the diverge tip exists; old baseline at tip_2
    // remains in place during the GC grace window (issue #134). A
    // baseline tombstone under `<prefix>/gc/baseline-tomb-*.json`
    // records it for future reclamation.
    let new_baseline = format!("repo/refs/heads/main/{tip_diverge}.bundle");
    assert!(store.contains(&new_baseline));
    assert!(
        store.contains(&baseline_key_2),
        "force push must leave prior baseline in place during grace window",
    );
    let metas = store.list("repo/gc/").await.unwrap();
    let baseline_tomb_count = metas
        .iter()
        .filter(|m| m.key.starts_with("repo/gc/baseline-tomb-"))
        .count();
    assert_eq!(
        baseline_tomb_count, 1,
        "force push must write exactly one baseline tombstone for the prior full_at",
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
    // Pin the exact wire bytes — the trailing `?` matters because git
    // treats `error <ref> "..."?` as recoverable and the inverse as fatal.
    // The packchain engine must surface the same wire format as the
    // bundle engine for non-ancestor refusals.
    assert_eq!(
        text, "error refs/heads/main \"remote ref is not ancestor of refs/heads/main.\"?\n\n",
        "got {text:?}",
    );
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
    // Pin the exact wire bytes — the trailing `?` matters because git
    // treats `error <ref> "..."?` as recoverable and the inverse as fatal.
    // The TTL number is a runtime parameter
    // (`GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS`); the default flows
    // from `DEFAULT_LOCK_TTL_SECONDS` so a future change to the default
    // updates both production code and this test in lockstep.
    let ttl_secs: u64 = std::env::var("GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(git_remote_object_store::protocol::push::DEFAULT_LOCK_TTL_SECONDS);
    let expected = format!(
        "error refs/heads/main \"failed to acquire ref lock at \
         repo/refs/heads/main/LOCK#.lock. Another client may be pushing. \
         If this persists beyond {ttl_secs}s, run git-remote-object-store \
         doctor to inspect and optionally clear stale locks.\"?\n\n",
    );
    assert_eq!(text, expected, "got {text:?}");
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
/// the chain.json write, and a tombstone-write failure on the prior
/// baseline must NOT fail the push (it would leave a confusing wire
/// output for what is already a successful push). Without the
/// tombstone the orphan baseline is left for `manage gc` or a manual
/// operator pass to reap.
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

    // Arm a put-bytes prefix fault on the baseline tombstone path so
    // the post-commit cleanup at the end of `perform_push_under_lock`
    // fails. The push must still succeed (chain.json has already been
    // written). The tombstone key embeds a UUID so we match on the
    // `gc/baseline-tomb-` prefix.
    store.arm(Fault::NetworkOnPutBytesPrefix {
        prefix: "repo/gc/baseline-tomb-".to_owned(),
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
    // The orphan baseline survives — `manage gc` or manual cleanup
    // must reap it.
    assert!(
        store.contains(&baseline_key_2),
        "old baseline must remain when tombstone fault prevented cleanup",
    );
    // The fault was consumed (one tombstone PUT was attempted and rejected).
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

// --- Annotated-tag pushes (issue #79) -------------------------------

/// Read a packchain segment's `.idx` file from the mock and return the
/// set of OIDs it enumerates. Lets tests assert that a specific tag /
/// commit OID is present in the on-bucket pack — the strongest check
/// available short of a fetch round-trip.
fn pack_idx_oids(store: &MockStore, prefix: &str, pack_path_in_chain: &str) -> Vec<String> {
    // pack_path_in_chain is e.g. "packs/<sha>.pack" — derive the .idx
    // sibling and download it from the mock.
    let idx_relative = pack_path_in_chain.replace(".pack", ".idx");
    let key = format!("{prefix}/{idx_relative}");
    let bytes = futures::executor::block_on(store.get_bytes(&key)).expect(".idx must exist");
    let tmp = tempfile::tempdir().expect("tempdir");
    let idx_path = tmp.path().join("scan.idx");
    std::fs::write(&idx_path, &bytes).unwrap();
    let idx = gix_pack::index::File::at(&idx_path, gix_hash::Kind::Sha1).expect("parse idx");
    let mut oids = Vec::with_capacity(idx.num_objects() as usize);
    for entry in idx.iter() {
        oids.push(entry.oid.to_string());
    }
    oids
}

#[tokio::test]
async fn first_push_of_annotated_tag_lands_pack_with_tag_object() {
    // E3 from the plan. Pushing `refs/tags/v1` (annotated) must:
    //   1. succeed with `ok refs/tags/v1`,
    //   2. record `chain.tip == tag_sha` (unpeeled, the ref's actual
    //      target),
    //   3. include the tag-object OID in segment-0's `.idx`.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, commit_sha, tag_sha) = make_seed_repo_with_annotated_tag("primary", "v1");

    let store = Arc::new(MockStore::new());
    let (out, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/tags/v1:refs/tags/v1\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("annotated tag push must succeed");
    assert_eq!(
        std::str::from_utf8(&out).unwrap(),
        "ok refs/tags/v1\n\n",
        "wire output: ok line for tag ref + terminator",
    );

    // chain.tip is the unpeeled tag SHA — the receiver sets the ref to
    // that exact OID.
    let chain = read_chain_for(&store, "repo", "refs/tags/v1");
    assert_eq!(
        chain["tip"], tag_sha,
        "chain.tip must be the tag OID, not the underlying commit",
    );

    // Segment-0 pack contains both the commit AND the tag object. Pin
    // both, since dropping the tag (regression) or dropping the commit
    // (different bug) both break fetch.
    let segments = chain["segments"].as_array().unwrap();
    let pack_path = segments[0]["pack"].as_str().unwrap();
    let oids = pack_idx_oids(&store, "repo", pack_path);
    assert!(
        oids.iter().any(|o| o == &tag_sha),
        "segment-0 pack must include the tag object {tag_sha}; got {oids:?}",
    );
    assert!(
        oids.iter().any(|o| o == &commit_sha),
        "segment-0 pack must include the commit target {commit_sha}; got {oids:?}",
    );
}

#[tokio::test]
async fn first_push_of_tag_of_tag_lands_full_chain_in_pack() {
    // E4: an annotated tag pointing at another annotated tag. Both
    // tag objects must land in segment-0's pack; otherwise a fetch
    // could resolve `outer` only by also having `inner`.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, commit_sha, inner_sha, outer_sha) =
        make_seed_repo_with_tag_of_tag("primary", "inner", "outer");

    let store = Arc::new(MockStore::new());
    let (out, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/tags/outer:refs/tags/outer\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("tag-of-tag push must succeed");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/tags/outer\n\n");

    let chain = read_chain_for(&store, "repo", "refs/tags/outer");
    let pack_path = chain["segments"].as_array().unwrap()[0]["pack"]
        .as_str()
        .unwrap();
    let oids = pack_idx_oids(&store, "repo", pack_path);
    for needed in [&outer_sha, &inner_sha, &commit_sha] {
        assert!(
            oids.iter().any(|o| o == needed),
            "tag-of-tag pack must contain {needed}; got {oids:?}",
        );
    }
}

#[tokio::test]
async fn force_retag_replaces_pack_with_new_tag_object() {
    // E8: push annotated `v1 → commit_a`, then `git tag -af v1 commit_b`
    // and force-push. The new chain.tip must be the new tag SHA, the
    // new segment-0 pack must contain the new tag object, and the old
    // tag's OID must NOT appear in the new pack (the force replaces
    // the chain — the old segment is reapable orphan storage). This
    // pins the force-retag interaction with the tag-chain plumbing
    // added in #79; without it, a regression that dropped tag_chain
    // on the force path would not be caught by the new-tag-push or
    // idempotent-repush tests.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    // commit_a is the old tag's target; the test does not assert anything
    // about it (commit_b descends from commit_a in this fixture, but
    // that's a fixture property, not a force-retag contract).
    let (seed, _commit_a, tag_v1_a) = make_seed_repo_with_annotated_tag("primary", "v1");

    let store = Arc::new(MockStore::new());
    let (_, r1) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/tags/v1:refs/tags/v1\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r1.expect("first tag push must succeed");

    // Add a second commit and retag v1 to it.
    std::fs::write(seed.path().join("f1.txt"), b"second\n").unwrap();
    git(&["add", "."], seed.path());
    git(
        &["commit", "--quiet", "-m", "step2", "--no-gpg-sign"],
        seed.path(),
    );
    let commit_b = git_capture(&["rev-parse", "HEAD"], seed.path())
        .trim()
        .to_owned();
    git(
        &["tag", "-af", "v1", "-m", "release v1 again", &commit_b],
        seed.path(),
    );
    let tag_v1_b = git_capture(&["rev-parse", "v1"], seed.path())
        .trim()
        .to_owned();
    assert_ne!(tag_v1_a, tag_v1_b, "retag must produce a new tag OID");

    // Force-push (the leading `+` flips the spec's force flag).
    let (out, r2) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push +refs/tags/v1:refs/tags/v1\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r2.expect("force-retag must succeed");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/tags/v1\n\n");

    // chain.tip moved to the new tag OID (unpeeled).
    let chain = read_chain_for(&store, "repo", "refs/tags/v1");
    assert_eq!(
        chain["tip"], tag_v1_b,
        "chain.tip must be the new tag OID after force-retag",
    );
    let segments = chain["segments"].as_array().unwrap();
    assert_eq!(
        segments.len(),
        1,
        "force-retag must collapse to a single segment",
    );

    // The new segment-0 pack contains the new tag and the new commit;
    // the old tag's OID is not in this pack.
    let pack_path = segments[0]["pack"].as_str().unwrap();
    let oids = pack_idx_oids(&store, "repo", pack_path);
    assert!(
        oids.iter().any(|o| o == &tag_v1_b),
        "new pack must include the new tag {tag_v1_b}; got {oids:?}",
    );
    assert!(
        oids.iter().any(|o| o == &commit_b),
        "new pack must include the new commit {commit_b}; got {oids:?}",
    );
    assert!(
        !oids.iter().any(|o| o == &tag_v1_a),
        "new pack must NOT include the old tag {tag_v1_a}; got {oids:?}",
    );
}

#[tokio::test]
async fn repushing_same_annotated_tag_is_idempotent() {
    // E7: a second push of the same annotated tag (same OID, no force)
    // must produce identical observable state — same `ok` line, no new
    // pack/idx keys, unchanged chain.json. The production code achieves
    // this via a short-circuit in `prepare_push`, but a full re-execution
    // would also be idempotent (content-addressed packs, deterministic
    // chain.json, idempotent put_if_absent on FORMAT/HEAD), so this test
    // only pins the observable contract — not the short-circuit path
    // specifically.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _commit_sha, _tag_sha) = make_seed_repo_with_annotated_tag("primary", "v1");

    let store = Arc::new(MockStore::new());
    let (_, r1) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/tags/v1:refs/tags/v1\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r1.expect("first push must succeed");
    let chain_1 = read_chain_for(&store, "repo", "refs/tags/v1").to_string();
    let key_count_1 = store.keys().len();

    let (out, r2) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/tags/v1:refs/tags/v1\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    r2.expect("idempotent re-push must succeed");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/tags/v1\n\n");
    let chain_2 = read_chain_for(&store, "repo", "refs/tags/v1").to_string();
    let key_count_2 = store.keys().len();
    assert_eq!(chain_1, chain_2, "chain.json unchanged on idempotent push");
    assert_eq!(
        key_count_1, key_count_2,
        "no new bucket keys created on idempotent push",
    );
}

/// `git mktag` shim: build a raw tag-object body pointing at `target`
/// of `kind`, write it via `git mktag`, and return the tag's OID. The
/// only CLI-friendly way to forge a tag-of-blob (porcelain `git tag -a
/// <name> <blob>` peels through the blob's surrounding tree if any
/// exists; `mktag` writes the bytes verbatim).
fn mktag_pointing_at(seed_dir: &Path, target_oid: &str, kind: &str, tag_name: &str) -> String {
    use std::io::Write as _;
    let body = format!(
        "object {target_oid}\n\
         type {kind}\n\
         tag {tag_name}\n\
         tagger Test <test@example.com> 0 +0000\n\
         \n\
         pointing-at-{kind}\n",
    );
    let mktag = std::process::Command::new("git")
        .args(["mktag"])
        .current_dir(seed_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git mktag");
    mktag
        .stdin
        .as_ref()
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    let out = mktag.wait_with_output().expect("git mktag");
    assert!(
        out.status.success(),
        "git mktag failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn first_push_of_tag_pointing_to_blob_lands_pack_with_tag_and_blob() {
    // #80: tag-of-blob is supported. The push lands the tag and the
    // leaf blob in segment-0's pack and writes chain.tip = tag OID.
    // Path-index is omitted because there is no tree to index.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _shas) = make_seed_repo(1, "primary");
    std::fs::write(seed.path().join("blob-target"), b"data\n").unwrap();
    let blob_oid = git_capture(&["hash-object", "-w", "blob-target"], seed.path())
        .trim()
        .to_owned();
    let tag_sha = mktag_pointing_at(seed.path(), &blob_oid, "blob", "blob-tag");
    git(&["update-ref", "refs/tags/blob-tag", &tag_sha], seed.path());

    let store = Arc::new(MockStore::new());
    let (out, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/tags/blob-tag:refs/tags/blob-tag\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("blob-tag push must succeed");
    assert_eq!(
        std::str::from_utf8(&out).unwrap(),
        "ok refs/tags/blob-tag\n\n",
        "wire output: ok line for blob-tag ref + terminator",
    );

    // chain.tip is the unpeeled tag OID.
    let chain = read_chain_for(&store, "repo", "refs/tags/blob-tag");
    assert_eq!(
        chain["tip"], tag_sha,
        "chain.tip must be the tag OID, not the blob",
    );

    // Segment-0 pack contains the tag + the blob (and nothing else —
    // blob-tipped chains have no commit/tree closure). Pin the exact
    // count so a regression that walked the seed repo's commit graph
    // would be caught.
    let segments = chain["segments"].as_array().unwrap();
    let pack_path = segments[0]["pack"].as_str().unwrap();
    let oids = pack_idx_oids(&store, "repo", pack_path);
    assert_eq!(
        oids.len(),
        2,
        "blob-tipped pack must contain exactly the blob + the tag; got {oids:?}",
    );
    assert!(
        oids.iter().any(|o| o == &tag_sha),
        "pack must include the tag {tag_sha}; got {oids:?}",
    );
    assert!(
        oids.iter().any(|o| o == &blob_oid),
        "pack must include the leaf blob {blob_oid}; got {oids:?}",
    );

    // No path-index for blob-tipped chains.
    let path_index_key = "repo/refs/tags/blob-tag/path-index.json";
    assert!(
        !store.contains(path_index_key),
        "blob-tipped chains must not write a path-index.json",
    );
}

#[tokio::test]
async fn first_push_of_tag_pointing_to_tree_lands_pack_with_tree_closure() {
    // #80: tag-of-tree is supported. The push must land the tag, the
    // leaf tree, and every blob in the tree closure. Path-index is
    // present and indexed under field `tip`.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _shas) = make_seed_repo(1, "primary");
    let tree_oid = git_capture(&["rev-parse", "HEAD^{tree}"], seed.path())
        .trim()
        .to_owned();
    let tag_sha = mktag_pointing_at(seed.path(), &tree_oid, "tree", "tree-tag");
    git(&["update-ref", "refs/tags/tree-tag", &tag_sha], seed.path());

    let store = Arc::new(MockStore::new());
    let (out, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/tags/tree-tag:refs/tags/tree-tag\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("tree-tag push must succeed");
    assert_eq!(
        std::str::from_utf8(&out).unwrap(),
        "ok refs/tags/tree-tag\n\n",
    );

    let chain = read_chain_for(&store, "repo", "refs/tags/tree-tag");
    assert_eq!(chain["tip"], tag_sha);

    // Pack contains tag + tree + every blob the seed repo wrote.
    let segments = chain["segments"].as_array().unwrap();
    let pack_path = segments[0]["pack"].as_str().unwrap();
    let oids = pack_idx_oids(&store, "repo", pack_path);
    assert!(oids.iter().any(|o| o == &tag_sha), "tag must be in pack");
    assert!(oids.iter().any(|o| o == &tree_oid), "tree must be in pack");
    let blob_oid = git_capture(&["rev-parse", "HEAD:f0.txt"], seed.path())
        .trim()
        .to_owned();
    assert!(
        oids.iter().any(|o| o == &blob_oid),
        "tree blob f0.txt {blob_oid} must be in pack; got {oids:?}",
    );

    // Path-index is present and tagged under `tip`, not `commit`.
    let path_index = read_path_index_for(&store, "repo", "refs/tags/tree-tag");
    assert_eq!(path_index["v"], 2);
    assert_eq!(path_index["tip"], tag_sha);
    let tree = path_index["tree"]
        .as_object()
        .expect("path-index tree must be a JSON object");
    assert!(
        tree.contains_key("f0.txt"),
        "tree-tip path-index must include the seed file",
    );
}

#[tokio::test]
async fn first_push_of_tag_of_tag_of_tree_round_trips_full_chain() {
    // Multi-level tag chain ending at a tree. Both tag OIDs land in
    // the pack alongside the tree closure.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _shas) = make_seed_repo(1, "primary");
    let tree_oid = git_capture(&["rev-parse", "HEAD^{tree}"], seed.path())
        .trim()
        .to_owned();
    let inner_tag = mktag_pointing_at(seed.path(), &tree_oid, "tree", "inner");
    let outer_tag = mktag_pointing_at(seed.path(), &inner_tag, "tag", "outer");
    git(&["update-ref", "refs/tags/outer", &outer_tag], seed.path());

    let store = Arc::new(MockStore::new());
    let (out, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/tags/outer:refs/tags/outer\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("tag-of-tag-of-tree push must succeed");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/tags/outer\n\n");

    let chain = read_chain_for(&store, "repo", "refs/tags/outer");
    assert_eq!(chain["tip"], outer_tag);
    let pack_path = chain["segments"].as_array().unwrap()[0]["pack"]
        .as_str()
        .unwrap();
    let oids = pack_idx_oids(&store, "repo", pack_path);
    // Both tags + the leaf tree + every blob in the tree closure. The
    // leaf-blob check pins that the tag-of-tag chain still triggers
    // tree-closure walking — without it, a regression that emitted
    // only the chain (no tree descent) would still pass the three
    // tag/tree-OID checks below.
    let blob_oid = git_capture(&["rev-parse", "HEAD:f0.txt"], seed.path())
        .trim()
        .to_owned();
    for needed in [&outer_tag, &inner_tag, &tree_oid, &blob_oid] {
        assert!(
            oids.iter().any(|o| o == needed),
            "pack must contain {needed}; got {oids:?}",
        );
    }
}

#[tokio::test]
async fn first_push_of_bare_blob_ref_lands_pack_with_blob_only() {
    // A ref pointing directly at a blob (no tag wrapper) is legal.
    // Pack contains exactly the blob; chain.tip is the blob OID.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _shas) = make_seed_repo(1, "primary");
    std::fs::write(seed.path().join("bare-blob"), b"bare\n").unwrap();
    let blob_oid = git_capture(&["hash-object", "-w", "bare-blob"], seed.path())
        .trim()
        .to_owned();
    git(
        &["update-ref", "refs/notes/special", &blob_oid],
        seed.path(),
    );

    let store = Arc::new(MockStore::new());
    let (out, result) = drive_in(
        s3_url_packchain(Some("repo")),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/notes/special:refs/notes/special\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("bare-blob push must succeed");
    assert_eq!(
        std::str::from_utf8(&out).unwrap(),
        "ok refs/notes/special\n\n",
    );
    let chain = read_chain_for(&store, "repo", "refs/notes/special");
    assert_eq!(chain["tip"], blob_oid);
    let pack_path = chain["segments"].as_array().unwrap()[0]["pack"]
        .as_str()
        .unwrap();
    let oids = pack_idx_oids(&store, "repo", pack_path);
    assert_eq!(
        oids.len(),
        1,
        "bare-blob pack must contain exactly the blob; got {oids:?}",
    );
    assert_eq!(oids[0], blob_oid);
    assert!(
        !store.contains("repo/refs/notes/special/path-index.json"),
        "blob-tipped chains must not write path-index.json",
    );
}

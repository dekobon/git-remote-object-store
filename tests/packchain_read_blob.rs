//! Direct file access integration tests for the packchain engine
//! (issue #65). Drives `read_blob` end-to-end against a [`MockStore`]
//! pre-populated by the same Phase 2 push the helper protocol uses,
//! then verifies the returned bytes match what the seed repo had at
//! that path.
//!
//! `read_blob` is a library API — no helper-protocol traffic is
//! involved. The tests construct a [`Remote`] via the test-only
//! constructor against the same `MockStore` the push wrote into.

#![cfg(feature = "test-util")]

mod common;

use std::sync::Arc;

use git_remote_object_store::object_store::mock::{Fault, MockStore};
use git_remote_object_store::object_store::{ObjectStore, ObjectStoreError};
use git_remote_object_store::packchain::{PackIndexCache, read_blob};
use git_remote_object_store::url::StorageEngine;
use git_remote_object_store::{PackchainError, Remote};

use common::{drive_in, git, git_available, git_capture, s3_url_packchain};

/// Push the seed repo into `store` via the packchain engine.
async fn push_seed_into(store: &Arc<MockStore>, seed_dir: &std::path::Path, prefix: Option<&str>) {
    let url = s3_url_packchain(prefix);
    let (_out, result) = drive_in(
        url,
        Arc::clone(store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed_dir.to_path_buf(),
    )
    .await;
    result.expect("packchain push must succeed");
}

/// Initialise a fresh repo with a small directory hierarchy so the
/// path-index has both top-level and nested entries to walk.
fn make_layered_repo() -> (tempfile::TempDir, Vec<(String, Vec<u8>)>) {
    let dir = tempfile::tempdir().expect("seed tempdir");
    git(&["init", "--quiet", "--initial-branch=main"], dir.path());
    git(&["config", "user.email", "test@example.com"], dir.path());
    git(&["config", "user.name", "Test"], dir.path());
    git(&["config", "commit.gpgsign", "false"], dir.path());

    // Three files at three depths. The body content is unique per file
    // so every test assertion is comparing distinct bytes.
    let files: Vec<(String, Vec<u8>)> = vec![
        ("README.md".to_owned(), b"top-level readme\n".to_vec()),
        (
            "src/main.rs".to_owned(),
            b"fn main() {\n    println!(\"hello\");\n}\n".to_vec(),
        ),
        (
            "src/lib/mod.rs".to_owned(),
            b"pub fn hello() -> &'static str { \"world\" }\n".to_vec(),
        ),
    ];
    for (path, body) in &files {
        let full = dir.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("mkdir for fixture");
        }
        std::fs::write(&full, body).expect("write fixture file");
    }
    git(&["add", "."], dir.path());
    git(
        &["commit", "--quiet", "-m", "initial", "--no-gpg-sign"],
        dir.path(),
    );
    (dir, files)
}

#[tokio::test]
async fn read_blob_returns_top_level_file_after_first_push() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let (seed, files) = make_layered_repo();
    push_seed_into(&store, seed.path(), Some("repo")).await;

    let remote = Remote::new_for_test(
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "repo",
        StorageEngine::Packchain,
    );
    let cache = PackIndexCache::default();
    let body = read_blob(&remote, "refs/heads/main", "README.md", &cache)
        .await
        .expect("read_blob must succeed");
    assert_eq!(body.as_ref(), files[0].1.as_slice(), "README.md content");
}

#[tokio::test]
async fn read_blob_returns_nested_file() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let (seed, files) = make_layered_repo();
    push_seed_into(&store, seed.path(), Some("repo")).await;

    let remote = Remote::new_for_test(
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "repo",
        StorageEngine::Packchain,
    );
    let cache = PackIndexCache::default();
    let body = read_blob(&remote, "refs/heads/main", "src/lib/mod.rs", &cache)
        .await
        .expect("read_blob nested path must succeed");
    assert_eq!(body.as_ref(), files[2].1.as_slice());
}

#[tokio::test]
async fn read_blob_walks_chain_to_find_blob_in_older_segment() {
    // The blob lives in the *baseline* segment — the second push only
    // adds a new file. read_blob must walk the chain past the newer
    // segment to reach it. This pins the newest-first chain scan.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let (seed, files) = make_layered_repo();
    push_seed_into(&store, seed.path(), Some("repo")).await;

    // Add a new file to seed and push again — this becomes the second
    // chain segment. The old files all still resolve to blobs that
    // live only in the baseline segment.
    std::fs::write(seed.path().join("NEW.txt"), b"second-segment\n").unwrap();
    git(&["add", "."], seed.path());
    git(
        &["commit", "--quiet", "-m", "second", "--no-gpg-sign"],
        seed.path(),
    );
    push_seed_into(&store, seed.path(), Some("repo")).await;

    // Pin the test premise: the second push must have produced a
    // second chain segment. Without this, a regression that collapsed
    // the second push into a fresh baseline (or made it idempotent)
    // would still pass — there'd be only one segment, and "walking"
    // it doesn't exercise the newest-first scan the test name claims.
    let chain_bytes = store
        .get_bytes("repo/refs/heads/main/chain.json")
        .await
        .expect("chain.json present after two pushes");
    let chain: serde_json::Value = serde_json::from_slice(&chain_bytes).expect("chain.json parses");
    let segments = chain["segments"].as_array().expect("segments is array");
    assert!(
        segments.len() >= 2,
        "two pushes must produce ≥2 chain segments, got {}: {chain}",
        segments.len(),
    );

    let remote = Remote::new_for_test(
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "repo",
        StorageEngine::Packchain,
    );
    let cache = PackIndexCache::default();
    // Pre-existing file: must walk to baseline.
    let main_rs = read_blob(&remote, "refs/heads/main", "src/main.rs", &cache)
        .await
        .expect("pre-existing blob must resolve via chain walk");
    assert_eq!(main_rs.as_ref(), files[1].1.as_slice());
    // New file: lives in newest segment.
    let new_txt = read_blob(&remote, "refs/heads/main", "NEW.txt", &cache)
        .await
        .expect("new blob must resolve from newest segment");
    assert_eq!(new_txt.as_ref(), b"second-segment\n");
}

#[tokio::test]
async fn read_blob_caches_pack_index_across_calls() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let (seed, files) = make_layered_repo();
    push_seed_into(&store, seed.path(), Some("repo")).await;

    let remote = Remote::new_for_test(
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "repo",
        StorageEngine::Packchain,
    );
    let cache = PackIndexCache::default();
    assert_eq!(cache.len(), 0);

    // First call populates the cache. Find the .idx key from the listing
    // so the fault below targets the actual on-bucket key.
    let _ = read_blob(&remote, "refs/heads/main", "README.md", &cache)
        .await
        .expect("first read_blob");
    let bytes_first = cache.resident_bytes();
    assert_eq!(cache.len(), 1, "one segment, one cached idx");
    assert!(bytes_first > 0);
    let metas = store.list("repo/packs/").await.expect("list packs");
    let idx_key = metas
        .into_iter()
        .map(|m| m.key)
        .find(|k| k.as_bytes().ends_with(b".idx"))
        .expect("at least one .idx key");

    // Arm a one-shot fault on the .idx GET. If the cache lookup is
    // bypassed (or broken), the second read_blob will hit `get_bytes`
    // for the .idx and surface a Store(Network) error. If the cache
    // hit fires as designed, `get_bytes` is never called for the idx
    // and the fault stays armed.
    store.arm(Fault::NetworkOnGetBytes {
        key: idx_key.clone(),
    });
    let body = read_blob(&remote, "refs/heads/main", "src/main.rs", &cache)
        .await
        .expect("cache hit must avoid the .idx fetch and the armed fault");
    assert_eq!(body.as_ref(), files[1].1.as_slice());
    assert_eq!(cache.len(), 1, "cache must reuse the existing idx");
    assert_eq!(cache.resident_bytes(), bytes_first);

    // Belt-and-suspenders: confirm the fault is still armed (i.e. it
    // never fired during the second read_blob). A direct `get_bytes`
    // for the same idx_key now consumes the fault.
    let err = store.get_bytes(&idx_key).await.unwrap_err();
    assert!(
        matches!(err, ObjectStoreError::Network(_)),
        "fault should still be armed after a cache-hit-only second read",
    );
}

#[tokio::test]
async fn read_blob_against_bundle_remote_returns_wrong_engine() {
    // No push needed — the engine guardrail short-circuits before any
    // bucket I/O.
    let store = Arc::new(MockStore::new());
    let remote = Remote::new_for_test(store as Arc<dyn ObjectStore>, "repo", StorageEngine::Bundle);
    let cache = PackIndexCache::default();
    let err = read_blob(&remote, "refs/heads/main", "README.md", &cache)
        .await
        .expect_err("bundle remote must reject read_blob");
    assert!(
        matches!(err, PackchainError::WrongEngine { found } if found == StorageEngine::Bundle),
        "expected WrongEngine(Bundle), got {err:?}",
    );
}

#[tokio::test]
async fn read_blob_missing_path_returns_path_not_found() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let (seed, _files) = make_layered_repo();
    push_seed_into(&store, seed.path(), Some("repo")).await;

    let remote = Remote::new_for_test(
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "repo",
        StorageEngine::Packchain,
    );
    let cache = PackIndexCache::default();
    let err = read_blob(&remote, "refs/heads/main", "no/such/path.txt", &cache)
        .await
        .expect_err("missing path must fail");
    assert!(
        matches!(err, PackchainError::PathNotFound { .. }),
        "expected PathNotFound, got {err:?}",
    );
}

#[tokio::test]
async fn read_blob_directory_returns_path_not_a_blob() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let (seed, _files) = make_layered_repo();
    push_seed_into(&store, seed.path(), Some("repo")).await;

    let remote = Remote::new_for_test(
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "repo",
        StorageEngine::Packchain,
    );
    let cache = PackIndexCache::default();
    // `src` is a directory in the seed repo, not a file.
    let err = read_blob(&remote, "refs/heads/main", "src", &cache)
        .await
        .expect_err("directory path must fail");
    assert!(
        matches!(err, PackchainError::PathNotABlob { .. }),
        "expected PathNotABlob, got {err:?}",
    );
}

#[tokio::test]
async fn read_blob_unknown_ref_returns_chain_absent() {
    let store = Arc::new(MockStore::new());
    // No push — empty bucket.
    let remote = Remote::new_for_test(
        store as Arc<dyn ObjectStore>,
        "repo",
        StorageEngine::Packchain,
    );
    let cache = PackIndexCache::default();
    let err = read_blob(&remote, "refs/heads/missing", "README.md", &cache)
        .await
        .expect_err("missing ref must fail");
    assert!(
        matches!(err, PackchainError::ChainAbsent { .. }),
        "expected ChainAbsent, got {err:?}",
    );
}

#[tokio::test]
async fn read_blob_invalid_path_returns_malformed_path() {
    let store = Arc::new(MockStore::new());
    let remote = Remote::new_for_test(
        store as Arc<dyn ObjectStore>,
        "repo",
        StorageEngine::Packchain,
    );
    let cache = PackIndexCache::default();
    // The engine guardrail passes (Packchain), so we hit path validation.
    for bad in ["", "/abs", "src/../etc", "src//main.rs", "./hidden"] {
        let err = read_blob(&remote, "refs/heads/main", bad, &cache)
            .await
            .expect_err(&format!("path `{bad}` must reject"));
        assert!(
            matches!(err, PackchainError::MalformedPath { .. }),
            "expected MalformedPath for `{bad}`, got {err:?}",
        );
    }
}

#[tokio::test]
async fn read_blob_path_index_absent_returns_typed_error() {
    // Hand-craft a state where chain.json exists but path-index.json
    // does not — Phase 2's crash analysis identifies this as a
    // partially-committed first push. read_blob must surface the
    // typed `PathIndexAbsent` rather than a generic NotFound.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let (seed, _files) = make_layered_repo();
    push_seed_into(&store, seed.path(), Some("repo")).await;
    // Drop path-index.json so chain.json is the only remaining manifest.
    let path_index_key = "repo/refs/heads/main/path-index.json";
    store
        .delete(path_index_key)
        .await
        .expect("delete path-index.json");
    // Sanity: confirm it's gone.
    match store.get_bytes(path_index_key).await {
        Err(ObjectStoreError::NotFound(_)) => {}
        other => panic!("expected NotFound after delete, got {other:?}"),
    }

    let remote = Remote::new_for_test(
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "repo",
        StorageEngine::Packchain,
    );
    let cache = PackIndexCache::default();
    let err = read_blob(&remote, "refs/heads/main", "README.md", &cache)
        .await
        .expect_err("missing path-index must fail");
    assert!(
        matches!(err, PackchainError::PathIndexAbsent { .. }),
        "expected PathIndexAbsent, got {err:?}",
    );
}

#[tokio::test]
async fn read_blob_pack_missing_returns_typed_error() {
    // chain.json + path-index.json present, but the pack was deleted.
    // read_blob must surface PackMissing rather than a confusing
    // "blob not in chain".
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let (seed, _files) = make_layered_repo();
    push_seed_into(&store, seed.path(), Some("repo")).await;

    // Find and delete the pack files via the listing. `meta.key` is a
    // bucket key, not a filesystem path — byte-level suffix match keeps
    // clippy's `case_sensitive_file_extension_comparisons` out of the
    // way (S3/Azure keys are byte-exact and lowercase by construction).
    let metas = store.list("repo/packs/").await.expect("list packs");
    let mut deleted = 0;
    for meta in metas {
        if meta.key.as_bytes().ends_with(b".idx") {
            store.delete(&meta.key).await.expect("delete idx");
            deleted += 1;
        }
    }
    assert!(deleted > 0, "must have deleted at least one .idx");

    let remote = Remote::new_for_test(
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "repo",
        StorageEngine::Packchain,
    );
    let cache = PackIndexCache::default();
    let err = read_blob(&remote, "refs/heads/main", "README.md", &cache)
        .await
        .expect_err("missing pack idx must fail");
    // Variant + key suffix together pin both the failure mode AND the
    // specific artefact: a regression that surfaced PackMissing for
    // the wrong key (e.g. the .pack instead of the .idx we deleted)
    // would slip past a variant-only assertion.
    let PackchainError::PackMissing { ref key } = err else {
        panic!("expected PackMissing, got {err:?}");
    };
    assert!(
        key.as_bytes().ends_with(b".idx"),
        "PackMissing key should be the deleted .idx, got {key:?}",
    );
}

#[tokio::test]
async fn read_blob_against_bucket_root_no_prefix_works() {
    // Sanity: read_blob must work with an empty-prefix remote (the
    // bucket-root repository case). Verifies the prefix-handling
    // matches push's empty-prefix shape.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let (seed, files) = make_layered_repo();
    push_seed_into(&store, seed.path(), None).await;

    let remote = Remote::new_for_test(
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "",
        StorageEngine::Packchain,
    );
    let cache = PackIndexCache::default();
    let body = read_blob(&remote, "refs/heads/main", "README.md", &cache)
        .await
        .expect("read_blob must succeed against bucket-root remote");
    assert_eq!(body.as_ref(), files[0].1.as_slice());
}

#[tokio::test]
async fn read_blob_recovers_blob_with_specific_byte_count() {
    // Pin actual bytes (not just length) for a non-trivial blob so a
    // regression that, e.g., off-by-ones the entry-data slice or
    // returns commit/tree bytes instead of the blob payload would
    // be caught by content comparison.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let store = Arc::new(MockStore::new());
    let (seed, files) = make_layered_repo();
    push_seed_into(&store, seed.path(), Some("repo")).await;

    // Cross-reference: the seed repo's git also knows the blob bytes.
    let main_rs_blob_via_git = git_capture(&["cat-file", "-p", "HEAD:src/main.rs"], seed.path());
    let main_rs_via_git_bytes = main_rs_blob_via_git.as_bytes();

    let remote = Remote::new_for_test(
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "repo",
        StorageEngine::Packchain,
    );
    let cache = PackIndexCache::default();
    let body = read_blob(&remote, "refs/heads/main", "src/main.rs", &cache)
        .await
        .expect("read_blob must succeed");

    // Three independent assertions: read_blob result equals the seed
    // file bytes (what was written), equals git's own cat-file output
    // (what git stored), and has the right length (covers a degenerate
    // case where both are equally-zero).
    assert_eq!(body.as_ref(), files[1].1.as_slice());
    assert_eq!(body.as_ref(), main_rs_via_git_bytes);
    assert!(!body.is_empty());
}

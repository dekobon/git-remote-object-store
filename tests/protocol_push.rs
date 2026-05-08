//! Push integration test: drive [`protocol::run`] through push batches
//! against a [`MockStore`] and a real local git repo, and verify the
//! on-bucket layout contract (`<prefix>/<ref>/<sha>.bundle`, `HEAD`,
//! `PROTECTED#`, lock files) is honored byte-for-byte.

#![cfg(feature = "test-util")]

mod common;

use std::sync::Arc;

use bytes::Bytes;
use git_remote_object_store::object_store::mock::MockStore;
use git_remote_object_store::object_store::{ObjectStore, PutOpts};
use time::Duration;
use time::OffsetDateTime;

use common::{
    drive_in, git, git_available, make_seed_repo, make_seed_repo_with_annotated_tag,
    s3_url_with_zip,
};

#[tokio::test]
async fn push_to_empty_remote_uploads_bundle_and_seeds_head() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas) = make_seed_repo(1, "primary");
    let sha = &shas[0];

    let store = Arc::new(MockStore::new());
    let script = "push refs/heads/main:refs/heads/main\n\n";
    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        script,
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push should succeed");
    assert_eq!(
        std::str::from_utf8(&out).unwrap(),
        "ok refs/heads/main\n\n",
        "ok line + terminator",
    );
    assert!(store.contains(&format!("repo/refs/heads/main/{sha}.bundle")));
    let head = store
        .keys()
        .into_iter()
        .find(|k| k == "repo/HEAD")
        .expect("HEAD seeded");
    let head_body = futures::executor::block_on(store.get_bytes(&head)).unwrap();
    assert_eq!(&head_body[..], b"refs/heads/main");
    // Lock released.
    assert!(!store.contains("repo/refs/heads/main/LOCK#.lock"));
}

#[tokio::test]
async fn push_fast_forward_replaces_old_bundle() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas) = make_seed_repo(2, "primary");
    let old_sha = &shas[0];
    let new_sha = &shas[1];

    let store = Arc::new(MockStore::new());
    // Pre-existing bundle for the older commit.
    store.insert(
        format!("repo/refs/heads/main/{old_sha}.bundle"),
        Bytes::from_static(b"old"),
    );

    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push should succeed");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/heads/main\n\n");
    assert!(store.contains(&format!("repo/refs/heads/main/{new_sha}.bundle")));
    assert!(!store.contains(&format!("repo/refs/heads/main/{old_sha}.bundle")));
}

#[tokio::test]
async fn push_non_fast_forward_is_rejected_without_force() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas) = make_seed_repo(1, "primary");

    // Synthesise a remote bundle for an unrelated commit (different repo
    // → unrelated SHA). Easiest: a second seed repo, same parent
    // structure but different content so the SHA differs.
    let (other_seed, other_shas) = make_seed_repo(1, "alt");
    let unrelated_sha = &other_shas[0];
    assert_ne!(&shas[0], unrelated_sha);
    drop(other_seed);

    let store = Arc::new(MockStore::new());
    store.insert(
        format!("repo/refs/heads/main/{unrelated_sha}.bundle"),
        Bytes::from_static(b"x"),
    );

    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push should produce a refusal, not abort");
    let text = std::str::from_utf8(&out).unwrap();
    assert!(
        text.starts_with("error refs/heads/main "),
        "expected error line, got {text:?}",
    );
    assert!(text.contains("not ancestor"), "got {text:?}");
    // The pre-existing unrelated bundle is left untouched.
    assert!(store.contains(&format!("repo/refs/heads/main/{unrelated_sha}.bundle")));
}

#[tokio::test]
async fn force_push_overwrites_unrelated_remote() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas) = make_seed_repo(1, "primary");
    let local_sha = &shas[0];
    let (other_seed, other_shas) = make_seed_repo(1, "alt");
    let unrelated_sha = &other_shas[0];
    drop(other_seed);

    let store = Arc::new(MockStore::new());
    store.insert(
        format!("repo/refs/heads/main/{unrelated_sha}.bundle"),
        Bytes::from_static(b"x"),
    );

    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push +refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("force push should succeed");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/heads/main\n\n");
    assert!(store.contains(&format!("repo/refs/heads/main/{local_sha}.bundle")));
    assert!(!store.contains(&format!("repo/refs/heads/main/{unrelated_sha}.bundle")));
}

#[tokio::test]
async fn force_push_protected_falls_back_to_ancestor_check() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas) = make_seed_repo(1, "primary");
    let (other_seed, other_shas) = make_seed_repo(1, "alt");
    let unrelated_sha = &other_shas[0];
    drop(other_seed);

    let store = Arc::new(MockStore::new());
    store.insert(
        format!("repo/refs/heads/main/{unrelated_sha}.bundle"),
        Bytes::from_static(b"x"),
    );
    // PROTECTED# marker — force flag should be neutralised, ancestor
    // check applies, and a non-ancestor force push is rejected.
    store.insert("repo/refs/heads/main/PROTECTED#", Bytes::from_static(b""));

    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push +refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push should produce a refusal");
    let text = std::str::from_utf8(&out).unwrap();
    assert!(text.contains("not ancestor"), "got {text:?}");
    // Pre-existing unrelated bundle untouched.
    assert!(store.contains(&format!("repo/refs/heads/main/{unrelated_sha}.bundle")));
    let _ = shas;
}

#[tokio::test]
async fn multi_bundle_pre_lock_rejects_push() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas) = make_seed_repo(1, "primary");
    let (other_seed, other_shas) = make_seed_repo(1, "alt");
    let extra_sha = &other_shas[0];
    drop(other_seed);

    let store = Arc::new(MockStore::new());
    let primary_key = format!("repo/refs/heads/main/{}.bundle", &shas[0]);
    let extra_key = format!("repo/refs/heads/main/{extra_sha}.bundle");
    store.insert(&primary_key, Bytes::from_static(b"a"));
    store.insert(&extra_key, Bytes::from_static(b"b"));

    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push should produce a refusal");
    let text = std::str::from_utf8(&out).unwrap();
    // Assert the exact wire bytes — the trailing `?` matters because git
    // treats `error <ref> "..."?` as recoverable and the inverse as fatal.
    // The pre-lock and under-lock duplicate-bundle errors must both end
    // in `?` so operators see a consistent format across branches.
    assert_eq!(
        text,
        "error refs/heads/main \"multiple bundles exist on server. \
         Run git-remote-object-store doctor to fix.\"?\n\n",
        "got {text:?}",
    );
    // Pre-existing bundles must remain — a regression that stealth-deleted
    // before the early return would still satisfy the message check.
    assert!(store.contains(&primary_key));
    assert!(store.contains(&extra_key));
}

#[tokio::test]
async fn push_with_held_lock_returns_contention_error() {
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
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push should produce a refusal");
    let text = std::str::from_utf8(&out).unwrap();
    assert!(text.contains("failed to acquire ref lock"), "got {text:?}");
    // Lock untouched (not ours to release).
    assert!(store.contains("repo/refs/heads/main/LOCK#.lock"));
}

#[tokio::test]
async fn push_recovers_stale_lock() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas) = make_seed_repo(1, "primary");
    let store = Arc::new(MockStore::new());
    // Lock held by a long-dead client (older than default 60s TTL).
    store.insert_with(
        "repo/refs/heads/main/LOCK#.lock",
        Bytes::new(),
        OffsetDateTime::now_utc() - Duration::seconds(120),
        PutOpts::default(),
    );

    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push should succeed via stale-lock recovery");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/heads/main\n\n");
    assert!(store.contains(&format!("repo/refs/heads/main/{}.bundle", &shas[0])));
    // Lock cleared on release.
    assert!(!store.contains("repo/refs/heads/main/LOCK#.lock"));
}

#[tokio::test]
async fn zip_variant_uploads_repo_zip_with_metadata() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas) = make_seed_repo(1, "primary");
    let sha = &shas[0];
    let store = Arc::new(MockStore::new());

    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), true),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("zip push should succeed");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/heads/main\n\n");
    assert!(store.contains(&format!("repo/refs/heads/main/{sha}.bundle")));
    assert!(store.contains("repo/refs/heads/main/repo.zip"));
    let zip_meta = store
        .metadata("repo/refs/heads/main/repo.zip")
        .expect("zip stored");
    let cd = zip_meta
        .content_disposition
        .expect("Content-Disposition set");
    assert!(
        cd.starts_with("attachment; filename=repo-")
            && std::path::Path::new(&cd)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("zip")),
        "unexpected CD {cd:?}",
    );
    let summary = zip_meta
        .user_metadata
        .iter()
        .find(|(k, _)| k == "codepipeline-artifact-revision-summary")
        .expect("revision summary metadata");
    assert!(!summary.1.is_empty());
}

#[tokio::test]
async fn delete_remote_ref_removes_single_bundle() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas) = make_seed_repo(1, "primary");
    let store = Arc::new(MockStore::new());
    store.insert(
        format!("repo/refs/heads/main/{}.bundle", &shas[0]),
        Bytes::from_static(b"x"),
    );

    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push :refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("delete should succeed");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/heads/main\n\n");
    assert!(!store.contains(&format!("repo/refs/heads/main/{}.bundle", &shas[0])));
}

#[tokio::test]
async fn delete_missing_remote_ref_emits_not_found() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _) = make_seed_repo(1, "primary");
    let store = Arc::new(MockStore::new());

    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push :refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("delete should produce a refusal");
    let text = std::str::from_utf8(&out).unwrap();
    assert!(text.contains("not found"), "got {text:?}");
}

#[tokio::test]
async fn batched_pushes_emit_outcome_per_command() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    // Build a seed repo with two refs pointing at the same commit.
    let (seed, shas) = make_seed_repo(1, "primary");
    let sha = &shas[0];
    git(
        &["update-ref", "refs/heads/feature", "refs/heads/main"],
        seed.path(),
    );

    let store = Arc::new(MockStore::new());
    let script = "push refs/heads/main:refs/heads/main\n\
                  push refs/heads/feature:refs/heads/feature\n\n";
    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        script,
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("batched push should succeed");
    let text = std::str::from_utf8(&out).unwrap();
    assert_eq!(
        text, "ok refs/heads/main\nok refs/heads/feature\n\n",
        "two ok lines + single trailing terminator",
    );
    // Both bundles must be uploaded — a regression that emitted `ok` for
    // the second push without invoking the upload would satisfy the wire
    // assertion alone.
    assert!(store.contains(&format!("repo/refs/heads/main/{sha}.bundle")));
    assert!(store.contains(&format!("repo/refs/heads/feature/{sha}.bundle")));
}

#[tokio::test]
async fn nonexistent_local_ref_emits_error() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _) = make_seed_repo(1, "primary");
    let store = Arc::new(MockStore::new());

    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/does-not-exist:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("missing local ref should produce a refusal, not abort");
    let text = std::str::from_utf8(&out).unwrap();
    assert!(text.contains("not found"), "got {text:?}");
}

#[tokio::test]
async fn force_push_protected_with_ancestor_remote_proceeds() {
    // The acceptance branch of force-protected demotion: `+` flag is
    // dropped because PROTECTED# is set, the ancestor check applies,
    // and remote IS an ancestor of local — so the push proceeds.
    // Without this test the protected-fallback code path is only
    // exercised on the rejection side.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas) = make_seed_repo(2, "primary");
    let ancestor = &shas[0];
    let descendant = &shas[1];

    let store = Arc::new(MockStore::new());
    // Pre-existing remote bundle for the ancestor commit.
    store.insert(
        format!("repo/refs/heads/main/{ancestor}.bundle"),
        Bytes::from_static(b"old"),
    );
    // Protect the ref. With `+`, force should be demoted; ancestor
    // check will then accept because ancestor IS an ancestor of
    // descendant.
    store.insert("repo/refs/heads/main/PROTECTED#", Bytes::from_static(b""));

    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push +refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("force-protected fast-forward should succeed");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/heads/main\n\n");
    assert!(store.contains(&format!("repo/refs/heads/main/{descendant}.bundle")));
    // Old bundle replaced.
    assert!(!store.contains(&format!("repo/refs/heads/main/{ancestor}.bundle")));
    // PROTECTED# marker untouched.
    assert!(store.contains("repo/refs/heads/main/PROTECTED#"));
}

#[tokio::test]
async fn batched_push_continues_after_per_push_transport_failure() {
    // Pin the contract that operational failures on push N do not
    // erase the outcome lines for pushes 1..N-1. Without per-push
    // error catching, a 5xx during push #2's bundles_for_ref would
    // abort the whole batch and silently lose push #1's `ok` line.
    use git_remote_object_store::object_store::mock::Fault;
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas) = make_seed_repo(1, "primary");
    let sha = &shas[0];
    git(
        &["update-ref", "refs/heads/feature", "refs/heads/main"],
        seed.path(),
    );

    let store = Arc::new(MockStore::new());
    // Trigger a transport-style failure on push #2's pre-lock listing
    // (the first store.list call in push_one for refs/heads/feature).
    store.arm(Fault::AccessDeniedOnList {
        prefix: "repo/refs/heads/feature/".into(),
    });

    let script = "push refs/heads/main:refs/heads/main\n\
                  push refs/heads/feature:refs/heads/feature\n\n";
    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        script,
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("batch with one failing push should not abort the helper");

    let text = std::str::from_utf8(&out).unwrap();
    let mut lines = text.split_inclusive('\n');
    assert_eq!(lines.next(), Some("ok refs/heads/main\n"));
    let second = lines.next().expect("error line for push #2");
    assert!(
        second.starts_with("error refs/heads/feature "),
        "expected error for second push, got {second:?}",
    );
    assert!(
        second.contains("access denied"),
        "error message must surface the underlying failure: {second:?}",
    );
    assert_eq!(lines.next(), Some("\n"), "trailing batch terminator");
    assert!(lines.next().is_none(), "no extra output: {text:?}");

    // Push #1 actually completed against the store — the upload is durable.
    assert!(store.contains(&format!("repo/refs/heads/main/{sha}.bundle")));
    // Push #2 was rejected before it could upload anything.
    assert!(!store.contains(&format!("repo/refs/heads/feature/{sha}.bundle")));
    // The fault fired exactly once.
    assert_eq!(store.pending_faults(), 0);
}

#[tokio::test]
async fn lock_release_failure_overrides_successful_push() {
    // When the push itself succeeds but the lock cannot be released,
    // the outcome must be `error <ref> ...`, not `ok <ref>`. This
    // matches upstream `cmd_push`'s `finally` block
    // (`../git-remote-s3/git_remote_s3/remote.py:297-303`).
    use git_remote_object_store::object_store::mock::Fault;
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, shas) = make_seed_repo(1, "primary");
    let sha = &shas[0];
    let store = Arc::new(MockStore::new());

    // Arm a network fault that fires when the lock key is deleted
    // (i.e., during release_lock after a successful push).
    let lock_key = "repo/refs/heads/main/LOCK#.lock";
    store.arm(Fault::NetworkOnDelete {
        key: lock_key.into(),
    });

    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push should produce an error outcome, not abort");

    let text = std::str::from_utf8(&out).unwrap();
    assert!(
        text.starts_with("error refs/heads/main "),
        "expected error line, got {text:?}",
    );
    assert!(
        text.contains("failed to release lock"),
        "error message must mention lock release failure: {text:?}",
    );
    assert!(
        text.contains("doctor"),
        "error message must point user at doctor: {text:?}",
    );

    // The bundle was uploaded successfully — the push itself worked.
    assert!(store.contains(&format!("repo/refs/heads/main/{sha}.bundle")));
    // The lock remains because the delete was faulted.
    assert!(store.contains(lock_key));
    // HEAD was seeded.
    assert!(store.contains("repo/HEAD"));
    // Fault consumed.
    assert_eq!(store.pending_faults(), 0);
}

#[tokio::test]
async fn pre_lock_multi_bundle_rejection_surfaces_unchanged() {
    // When the pre-lock `bundles_for_ref` check (push.rs:480-487) finds
    // multiple bundles for the same ref, the push is rejected BEFORE
    // lock acquisition. The multi-bundle error must surface on the wire.
    //
    // Note: the `_ => result` match arm in push_one (push.rs:577) that
    // preserves a push error over a release failure cannot be exercised
    // via this integration test: MockStore's state is static between
    // the pre-lock and under-lock listings, so a multi-bundle condition
    // always fires at the pre-lock check before lock acquisition. The
    // under-lock path is covered structurally (only the `Ok(Ok{..})`
    // arm overrides the result; all others fall through unchanged) and
    // by the unit-level release_lock tests.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _shas) = make_seed_repo(1, "primary");
    let store = Arc::new(MockStore::new());

    // Seed two bundles for the same ref — the pre-lock listing finds >1
    // bundle and returns PushOutcome::Error before lock acquisition.
    // Use realistic 40-hex names so the fixture survives any future
    // tightening of `is_bundle_candidate` to match the parser.
    let sha_a = "1111111111111111111111111111111111111111";
    let sha_b = "2222222222222222222222222222222222222222";
    store.insert(
        format!("repo/refs/heads/main/{sha_a}.bundle"),
        Bytes::from_static(b"a"),
    );
    store.insert(
        format!("repo/refs/heads/main/{sha_b}.bundle"),
        Bytes::from_static(b"b"),
    );

    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push should produce an error outcome, not abort");

    let text = std::str::from_utf8(&out).unwrap();
    // Pin the byte-exact wire format including the trailing `?` — git
    // treats `error <ref> "..."?` as recoverable and the inverse as
    // fatal (#34). A loose `contains("multiple bundles")` would have
    // missed that regression.
    assert_eq!(
        text,
        "error refs/heads/main \"multiple bundles exist on server. \
         Run git-remote-object-store doctor to fix.\"?\n\n",
        "got {text:?}",
    );
    // The lock was never acquired — the pre-lock check returned early.
    assert!(
        !store.contains("repo/refs/heads/main/LOCK#.lock"),
        "lock must not be acquired when the pre-lock check rejects",
    );
}

#[tokio::test]
async fn pre_existing_malformed_bundle_key_surfaces_parse_error() {
    // When the pre-lock listing finds exactly one bundle whose stem is
    // not a 40-hex SHA, `parse_remote_sha_from_key` returns None and
    // push.rs:490-498 emits a `PushOutcome::Error` advising the user to
    // run `doctor`. Exercises the otherwise-untested malformed-key arm.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, _shas) = make_seed_repo(1, "primary");
    let store = Arc::new(MockStore::new());

    // `not-a-valid-sha.bundle` passes `is_bundle_candidate` (no
    // PROTECTED#, no .zip, no /LOCKS/, doesn't end in .lock) but the
    // stem is not 40 hex chars, so `parse_remote_sha_from_key` returns
    // None and the malformed-key error path fires.
    let bad_key = "repo/refs/heads/main/not-a-valid-sha.bundle";
    store.insert(bad_key, Bytes::from_static(b"junk"));

    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push should produce an error outcome, not abort");

    // Wire format: `error <ref> "<msg>"?\n` — see PushOutcome::to_protocol_line
    // and push.rs:494-499. The message embeds `{key:?}`, which renders
    // the key with surrounding literal quote bytes (no escaping needed
    // because the key contains no `"` characters).
    let text = std::str::from_utf8(&out).expect("stdout utf-8");
    let expected = format!(
        "error refs/heads/main \"unable to parse remote bundle key \"{bad_key}\"; \
         run git-remote-object-store doctor to fix.\"?\n\n",
    );
    assert_eq!(text, expected, "exact wire bytes for malformed-key error");

    // The lock was never acquired — the pre-lock check returned early.
    assert!(
        !store.contains("repo/refs/heads/main/LOCK#.lock"),
        "lock must not be acquired when the pre-lock check rejects",
    );
    // The malformed bundle is still present — push did not delete it.
    assert!(
        store.contains(bad_key),
        "malformed bundle must remain untouched (doctor's job to clean up)",
    );
}

// --- Annotated-tag push: bundle's pack must contain the tag (issue #79)

#[tokio::test]
async fn bundle_first_push_of_annotated_tag_lands_bundle_at_tag_sha() {
    // E9 push side. The bundle file is named after the ref's actual
    // target (the tag-OID for an annotated tag). Pin both:
    //   1. Push succeeds (no `Expected object of kind commit` regression
    //      via bundle's path).
    //   2. The bundle file lands at `<tag_sha>.bundle` (not the
    //      underlying-commit SHA).
    // The push-then-fetch round-trip in `protocol_fetch.rs` covers the
    // stronger property — that the bundle's *pack* includes the tag
    // object — so this test only pins the bundle's wire-key shape.
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let (seed, commit_sha, tag_sha) = make_seed_repo_with_annotated_tag("primary", "v1");
    assert_ne!(commit_sha, tag_sha, "fixture must produce distinct OIDs");

    let store = Arc::new(MockStore::new());
    let (out, result) = drive_in(
        s3_url_with_zip(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/tags/v1:refs/tags/v1\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("bundle annotated-tag push must succeed");
    assert_eq!(std::str::from_utf8(&out).unwrap(), "ok refs/tags/v1\n\n");

    let tag_key = format!("repo/refs/tags/v1/{tag_sha}.bundle");
    assert!(
        store.contains(&tag_key),
        "bundle must land at {tag_key} (named after the tag OID)",
    );
    let commit_key = format!("repo/refs/tags/v1/{commit_sha}.bundle");
    assert!(
        !store.contains(&commit_key),
        "bundle must NOT be named after the commit (would mean we peeled before naming)",
    );
}

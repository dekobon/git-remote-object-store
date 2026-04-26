//! Phase 8 integration test: drive [`protocol::run`] through push
//! batches against a [`MockStore`] and a real local git repo, and
//! verify the on-bucket layout matches `execution-plan.md` §1.1.

#![cfg(feature = "test-util")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use git_remote_object_store::object_store::mock::MockStore;
use git_remote_object_store::object_store::{ObjectStore, PutOpts};
use git_remote_object_store::protocol::{ProtocolError, run};
use git_remote_object_store::url::{self, RemoteUrl};
use tempfile::TempDir;
use time::Duration;
use time::OffsetDateTime;
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

fn s3_url(prefix: Option<&str>, zip: bool) -> RemoteUrl {
    let mut raw = match prefix {
        Some(p) => format!("s3+https://my-bucket.s3.us-west-2.amazonaws.com/{p}"),
        None => "s3+https://my-bucket.s3.us-west-2.amazonaws.com/".to_string(),
    };
    if zip {
        raw.push_str("?zip=1");
    }
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

fn git(args: &[&str], cwd: &Path) {
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

/// Initialise a fresh repo with `n` linear commits on `refs/heads/main`
/// and return the dir + Vec<sha> in commit order (oldest first).
///
/// `salt` differentiates blob contents so two repos seeded in the same
/// wall-clock second still produce distinct commit SHAs.
fn make_seed_repo(n: usize, salt: &str) -> (TempDir, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    git(&["init", "--quiet", "--initial-branch=main"], dir.path());
    git(&["config", "user.email", "test@example.com"], dir.path());
    git(&["config", "user.name", "Test"], dir.path());
    git(&["config", "commit.gpgsign", "false"], dir.path());

    let mut shas = Vec::new();
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
        s3_url(Some("repo"), false),
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
        s3_url(Some("repo"), false),
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
        s3_url(Some("repo"), false),
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
        s3_url(Some("repo"), false),
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
        s3_url(Some("repo"), false),
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
        s3_url(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push should produce a refusal");
    let text = std::str::from_utf8(&out).unwrap();
    assert!(text.contains("multiple bundles"), "got {text:?}");
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
        s3_url(Some("repo"), false),
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
        s3_url(Some("repo"), false),
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
        s3_url(Some("repo"), true),
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
        s3_url(Some("repo"), false),
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
        s3_url(Some("repo"), false),
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
        s3_url(Some("repo"), false),
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
        s3_url(Some("repo"), false),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
        "push refs/heads/does-not-exist:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("missing local ref should produce a refusal, not abort");
    let text = std::str::from_utf8(&out).unwrap();
    assert!(text.contains("not found"), "got {text:?}");
}

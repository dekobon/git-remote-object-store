//! Backend-agnostic packchain end-to-end scenarios for issue #69.
//!
//! These scenarios drive the helper-protocol REPL ([`protocol::run`])
//! and the public packchain APIs ([`Remote::open`], [`read_blob`],
//! [`gc::mark`] / [`gc::sweep`]) against a live `Arc<dyn ObjectStore>`
//! constructed by the per-backend caller (`packchain_live_s3.rs` or
//! `packchain_live_azure.rs`). Phases 2 (push), 3 (fetch),
//! 4 (`read_blob`), and 5 (gc) are covered. The tests check the
//! end-to-end behaviour that `MockStore`-only suites cannot — real
//! network round-trips, real HTTP semantics (range/etag/precondition),
//! real list-pagination behaviour — without re-implementing the
//! helper-protocol input loop in each per-backend file.
//!
//! Rationale per issue #69: each phase's live setup overlaps
//! significantly (fresh bucket / container, `Remote::open`, REPL
//! driver), so one shared scenario module beats four near-duplicate
//! pairs of S3/Azure tests.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use git_remote_object_store::object_store::ObjectStore;
use git_remote_object_store::packchain::gc::{MarkOpts, SweepOpts, mark, sweep};
use git_remote_object_store::protocol::{ProtocolError, backend, run};
use git_remote_object_store::url::{RemoteUrl, StorageEngine};
use git_remote_object_store::{PackIndexCache, Remote, read_blob};
use tokio::io::AsyncWriteExt;

// ---------------------------------------------------------------------------
// Generic helpers — git CLI, in-process REPL driver, seed-repo factory.
// ---------------------------------------------------------------------------

/// Check whether the `git` CLI is available on `PATH`. Cached once per
/// test binary so the repeated probe doesn't dominate scenario startup.
pub fn git_available() -> bool {
    static AVAIL: OnceLock<bool> = OnceLock::new();
    *AVAIL.get_or_init(|| {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    })
}

/// Skip the current scenario if the `git` CLI is not on `PATH`.
/// Returns `true` when skipped — caller pattern is
/// `if skip_if_no_git() { return; }`.
fn skip_if_no_git() -> bool {
    if git_available() {
        return false;
    }
    eprintln!("skipping: git not on PATH");
    true
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

/// Initialise a fresh repo with `n` linear commits on `refs/heads/main`
/// and return the dir + Vec<sha> in commit order (oldest first).
///
/// `label` differentiates blob contents *across distinct scenarios* so
/// two scenarios in the same test binary do not produce identical
/// commit SHAs (which would otherwise share a content-SHA pack on the
/// shared bucket). Within a single scenario, label-equality is fine
/// because each scenario uses its own bucket / tempdir — bucket
/// isolation, not the label, is what guarantees no on-bucket collisions
/// across parameterised invocations of the same scenario.
pub fn make_seed_repo(n: usize, label: &str) -> (tempfile::TempDir, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    git(&["init", "--quiet", "--initial-branch=main"], dir.path());
    git(&["config", "user.email", "test@example.com"], dir.path());
    git(&["config", "user.name", "Test"], dir.path());
    git(&["config", "commit.gpgsign", "false"], dir.path());

    let mut shas = Vec::with_capacity(n);
    for i in 0..n {
        let body = format!("{label}-{i}\n");
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

/// Initialise a fresh empty destination repo for fetch tests. The
/// helper-protocol fetch path writes pack files into
/// `dst/.git/objects/pack`; the dst repo must have a `.git/` directory.
pub fn make_empty_dst() -> tempfile::TempDir {
    let dst = tempfile::tempdir().expect("dst tempdir");
    git(&["init", "--quiet", "--initial-branch=main"], dst.path());
    git(&["config", "user.email", "test@example.com"], dst.path());
    git(&["config", "user.name", "Test"], dst.path());
    git(&["config", "commit.gpgsign", "false"], dst.path());
    dst
}

/// `git cat-file -e <sha>` — true if the object is reachable in `dst`.
pub fn dst_has_object(dst: &Path, sha: &str) -> bool {
    std::process::Command::new("git")
        .args(["cat-file", "-e", sha])
        .current_dir(dst)
        .output()
        .expect("spawn git cat-file")
        .status
        .success()
}

/// Drive [`protocol::run`] in-process via a tokio duplex channel.
///
/// Feeds `script` to the helper's stdin, collects all stdout output,
/// and returns `(stdout_bytes, run_result)`. Mirrors the
/// `tests/common/mod.rs::drive_in` helper used by the lib-side
/// protocol tests, but resolves the engine via `validate_format`
/// against the live store rather than the in-memory `MockStore`.
pub async fn drive_in(
    remote: RemoteUrl,
    store: Arc<dyn ObjectStore>,
    script: &str,
    repo_dir: PathBuf,
) -> (Vec<u8>, Result<(), ProtocolError>) {
    let (client_side, helper_side) = tokio::io::duplex(64 * 1024);
    let (helper_in, helper_out) = tokio::io::split(helper_side);
    let (mut client_reader, mut client_writer) = tokio::io::split(client_side);

    let script_bytes = script.as_bytes().to_owned();
    let writer_task = tokio::spawn(async move {
        // Tolerate `BrokenPipe`: a helper that aborts early closes its
        // stdin reader before the full script lands. That is correct
        // helper behaviour, not a test failure.
        let suppress_broken_pipe = |e: std::io::Error| {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                Ok(())
            } else {
                Err(e)
            }
        };
        client_writer
            .write_all(&script_bytes)
            .await
            .or_else(suppress_broken_pipe)
            .unwrap();
        client_writer
            .shutdown()
            .await
            .or_else(suppress_broken_pipe)
            .unwrap();
    });

    let reader_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        client_reader.read_to_end(&mut buf).await.unwrap();
        buf
    });

    let engine = backend::validate_format(
        store.as_ref(),
        remote.prefix().unwrap_or_default(),
        remote.flags().engine,
    )
    .await
    .unwrap_or_else(|e| panic!("validate_format on freshly-prepared bucket: {e}"));
    let result = run(
        remote,
        store,
        engine,
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

// ---------------------------------------------------------------------------
// Backend-agnostic scenarios.
//
// Each scenario takes (store, &remote, prefix). The store is the live
// backend; `remote` is a parsed `RemoteUrl` (borrowed; the scenario
// clones into the per-call `protocol::run` invocations as needed);
// `prefix` is what the URL's `prefix()` resolves to (empty string for
// bucket-root layout). The trio is enough to drive every public surface
// the issue calls out. Scenarios that have no on-bucket invariants to
// check (e.g. fetch-only) drop the `prefix` argument rather than
// accept-and-ignore it.
// ---------------------------------------------------------------------------

/// Phase 2: first push lays down `FORMAT`, `HEAD`, `chain.json`,
/// `path-index.json`, `<tip>.bundle`, and `packs/<sha>.{pack,idx}`.
/// Issue #69 phase 2 / scenario 1.
pub async fn first_push_writes_packchain_layout(
    store: Arc<dyn ObjectStore>,
    remote: &RemoteUrl,
    prefix: &str,
) {
    if skip_if_no_git() {
        return;
    }
    let (seed, shas) = make_seed_repo(1, "primary");
    let tip = &shas[0];

    let (_out, result) = drive_in(
        remote.clone(),
        Arc::clone(&store),
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("packchain push must succeed");

    // FORMAT and HEAD seeded.
    let format_body = store
        .get_bytes(&join(prefix, "FORMAT"))
        .await
        .expect("FORMAT seeded by first push");
    assert_eq!(&format_body[..], b"packchain", "FORMAT body");
    let head_body = store
        .get_bytes(&join(prefix, "HEAD"))
        .await
        .expect("HEAD seeded by first push");
    assert_eq!(&head_body[..], b"refs/heads/main", "HEAD body");

    // Baseline bundle present at <tip>.bundle.
    let baseline = join(prefix, &format!("refs/heads/main/{tip}.bundle"));
    store
        .get_bytes(&baseline)
        .await
        .unwrap_or_else(|e| panic!("baseline bundle missing at {baseline}: {e}"));

    // chain.json shape.
    let chain_key = join(prefix, "refs/heads/main/chain.json");
    let chain_bytes = store.get_bytes(&chain_key).await.expect("chain.json");
    let chain: serde_json::Value = serde_json::from_slice(&chain_bytes).expect("chain.json parses");
    assert_eq!(chain["v"], 1, "chain.json schema version");
    assert_eq!(chain["tip"], *tip);
    assert_eq!(chain["full_at"], *tip, "first push: full_at == tip");
    let segments = chain["segments"].as_array().expect("segments array");
    assert_eq!(segments.len(), 1, "first push: exactly one segment");
    let pack_path = segments[0]["pack"].as_str().expect("segment.pack string");
    assert!(pack_path.starts_with("packs/"), "segment.pack: {pack_path}");

    // The pack and its idx exist on the bucket.
    let pack_key = join(prefix, pack_path);
    store
        .get_bytes(&pack_key)
        .await
        .unwrap_or_else(|e| panic!("pack object missing at {pack_key}: {e}"));
    let idx_key = join(prefix, &pack_path.replace(".pack", ".idx"));
    store
        .get_bytes(&idx_key)
        .await
        .unwrap_or_else(|e| panic!("idx object missing at {idx_key}: {e}"));

    // path-index.json reflects the seed file.
    let pi_key = join(prefix, "refs/heads/main/path-index.json");
    let pi_bytes = store.get_bytes(&pi_key).await.expect("path-index.json");
    let pi: serde_json::Value = serde_json::from_slice(&pi_bytes).expect("path-index parses");
    assert_eq!(pi["v"], 2, "path-index.json schema version");
    assert_eq!(pi["tip"], *tip);
    let tree = pi["tree"].as_object().expect("tree must be JSON object");
    assert!(
        tree.contains_key("f0.txt"),
        "path-index tree must include seed file f0.txt, got {:?}",
        tree.keys().collect::<Vec<_>>(),
    );

    // Lock released.
    let lock = join(prefix, "refs/heads/main/LOCK#.lock");
    let lock_get = store.get_bytes(&lock).await;
    assert!(
        matches!(
            lock_get,
            Err(git_remote_object_store::ObjectStoreError::NotFound(_))
        ),
        "LOCK#.lock must not exist after push, got {lock_get:?}",
    );
}

/// Phase 2: incremental push appends a chain segment newest-first.
/// Issue #69 phase 2 / scenario 2.
pub async fn incremental_push_appends_segment(
    store: Arc<dyn ObjectStore>,
    remote: &RemoteUrl,
    prefix: &str,
) {
    if skip_if_no_git() {
        return;
    }
    let (seed, shas1) = make_seed_repo(1, "incremental");
    let tip_1 = &shas1[0];

    // First push.
    drive_in(
        remote.clone(),
        Arc::clone(&store),
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await
    .1
    .expect("first push");

    // Second commit + push.
    std::fs::write(seed.path().join("f1.txt"), b"second\n").unwrap();
    git(&["add", "."], seed.path());
    git(
        &["commit", "--quiet", "-m", "step2", "--no-gpg-sign"],
        seed.path(),
    );
    let tip_2 = git_capture(&["rev-parse", "HEAD"], seed.path())
        .trim()
        .to_owned();
    assert_ne!(*tip_1, tip_2);

    drive_in(
        remote.clone(),
        Arc::clone(&store),
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await
    .1
    .expect("incremental push");

    // chain.json: tip moved, full_at preserved, two segments newest-first.
    let chain_key = join(prefix, "refs/heads/main/chain.json");
    let chain_bytes = store.get_bytes(&chain_key).await.expect("chain.json");
    let chain: serde_json::Value = serde_json::from_slice(&chain_bytes).expect("chain.json parses");
    assert_eq!(chain["tip"], tip_2);
    assert_eq!(chain["full_at"], *tip_1, "full_at preserved on incremental");
    let segments = chain["segments"].as_array().expect("segments array");
    assert_eq!(segments.len(), 2, "incremental adds one segment");
    assert_eq!(segments[0]["sha"], tip_2, "segments[0] = newest tip");
    assert_eq!(segments[0]["parent_sha"], *tip_1, "parent_sha chain link");
    assert_eq!(segments[1]["sha"], *tip_1, "segments[1] = prior tip");

    // Both packs exist on the bucket.
    for seg in segments {
        let pack_path = seg["pack"].as_str().expect("segment.pack");
        store
            .get_bytes(&join(prefix, pack_path))
            .await
            .unwrap_or_else(|e| panic!("pack {pack_path} missing on bucket: {e}"));
    }
}

/// Phase 2: force push collapses a multi-segment chain into one.
/// Issue #69 phase 2 / scenario 3.
pub async fn force_push_collapses_chain(
    store: Arc<dyn ObjectStore>,
    remote: &RemoteUrl,
    prefix: &str,
) {
    if skip_if_no_git() {
        return;
    }
    let (seed, shas) = make_seed_repo(2, "force");

    // One push of a 2-commit history — produces a 1-segment chain
    // whose pack carries both commits and whose `full_at` points
    // at the 2-commit tip.
    drive_in(
        remote.clone(),
        Arc::clone(&store),
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await
    .1
    .expect("first push");

    // Capture the pre-collapse `full_at` so we can assert it actually
    // changed across the force push. Without this, a regression that
    // left `full_at` pinned at the old tip while rewriting `segments`
    // would still satisfy `chain["full_at"] == new_tip` if `new_tip`
    // happened to equal the original baseline (e.g. a single-commit
    // reset back to the seed commit).
    let chain_key = join(prefix, "refs/heads/main/chain.json");
    let pre_chain: serde_json::Value =
        serde_json::from_slice(&store.get_bytes(&chain_key).await.expect("chain.json"))
            .expect("chain.json parses");
    let pre_full_at = pre_chain["full_at"]
        .as_str()
        .expect("pre-collapse full_at")
        .to_owned();

    // Drop the second commit, force-push HEAD~1: this rewrites history
    // and force is required because the new tip is not a descendant.
    git(&["reset", "--hard", "HEAD~1"], seed.path());
    let new_tip = git_capture(&["rev-parse", "HEAD"], seed.path())
        .trim()
        .to_owned();
    assert_eq!(
        new_tip, shas[0],
        "reset --hard HEAD~1 must land on the first seeded commit",
    );
    assert_ne!(
        pre_full_at, new_tip,
        "test fixture invariant: pre-collapse full_at must differ from \
         the post-reset tip so the assert_ne! below is meaningful",
    );

    drive_in(
        remote.clone(),
        Arc::clone(&store),
        "push +refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await
    .1
    .expect("force push");

    let chain_bytes = store.get_bytes(&chain_key).await.expect("chain.json");
    let chain: serde_json::Value = serde_json::from_slice(&chain_bytes).expect("chain.json parses");
    let segments = chain["segments"].as_array().expect("segments array");
    assert_eq!(
        segments.len(),
        1,
        "force push must collapse chain to a single segment, got {segments:?}",
    );
    assert_eq!(chain["tip"], new_tip);
    assert_eq!(
        chain["full_at"], new_tip,
        "force push: full_at == tip (fresh baseline)",
    );
    assert_ne!(
        chain["full_at"].as_str().unwrap_or_default(),
        pre_full_at,
        "force push must replace full_at, not retain the prior baseline's value",
    );
}

/// Phase 3: full clone into an empty dst repo lands the tip.
/// Issue #69 phase 3 / scenario 1.
pub async fn fetch_into_empty_repo_lands_tip(store: Arc<dyn ObjectStore>, remote: &RemoteUrl) {
    if skip_if_no_git() {
        return;
    }
    let (seed, shas) = make_seed_repo(1, "fetch-empty");
    let tip = &shas[0];

    drive_in(
        remote.clone(),
        Arc::clone(&store),
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await
    .1
    .expect("seed push");

    let dst = make_empty_dst();
    let fetch_script = format!("fetch {tip} refs/heads/main\n\n");
    drive_in(
        remote.clone(),
        Arc::clone(&store),
        &fetch_script,
        dst.path().to_path_buf(),
    )
    .await
    .1
    .expect("fetch into empty dst");
    assert!(
        dst_has_object(dst.path(), tip),
        "tip {tip} must be reachable in dst after fetch",
    );
}

/// Phase 3: clone of a 2-segment chain lands every commit's objects
/// in the dst's ODB (chain-walk fetch).
/// Issue #69 phase 3 / scenario 2.
pub async fn chain_walk_fetch_installs_all_segments(
    store: Arc<dyn ObjectStore>,
    remote: &RemoteUrl,
) {
    if skip_if_no_git() {
        return;
    }
    let (seed, shas1) = make_seed_repo(1, "chainwalk");
    let tip_1 = &shas1[0];

    drive_in(
        remote.clone(),
        Arc::clone(&store),
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await
    .1
    .expect("first push");

    std::fs::write(seed.path().join("step2.txt"), b"second\n").unwrap();
    git(&["add", "."], seed.path());
    git(
        &["commit", "--quiet", "-m", "step2", "--no-gpg-sign"],
        seed.path(),
    );
    let tip_2 = git_capture(&["rev-parse", "HEAD"], seed.path())
        .trim()
        .to_owned();

    drive_in(
        remote.clone(),
        Arc::clone(&store),
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await
    .1
    .expect("second push");

    let dst = make_empty_dst();
    drive_in(
        remote.clone(),
        Arc::clone(&store),
        &format!("fetch {tip_2} refs/heads/main\n\n"),
        dst.path().to_path_buf(),
    )
    .await
    .1
    .expect("chain-walk fetch into empty dst");

    assert!(
        dst_has_object(dst.path(), tip_1),
        "tip_1 {tip_1} must be reachable after chain-walk fetch",
    );
    assert!(
        dst_has_object(dst.path(), &tip_2),
        "tip_2 {tip_2} must be reachable after chain-walk fetch",
    );
}

/// Phase 4: `read_blob` returns byte-equal content; `PackIndexCache`
/// reuse holds across calls — verified by deleting the `.idx` key
/// between the first and second read. If cache reuse works the second
/// read still succeeds via cached indices; if it doesn't, the second
/// read surfaces `PackchainError::PackMissing` because the missing
/// `.idx` cannot be re-loaded from the bucket.
/// Issue #69 phase 4.
pub async fn read_blob_returns_byte_equal_content_and_cache_survives_idx_delete(
    store: Arc<dyn ObjectStore>,
    remote: &RemoteUrl,
    prefix: &str,
) {
    if skip_if_no_git() {
        return;
    }
    let (seed, _shas) = make_seed_repo(1, "blob");
    let body_on_disk = std::fs::read(seed.path().join("f0.txt")).expect("read seed file");

    drive_in(
        remote.clone(),
        Arc::clone(&store),
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await
    .1
    .expect("seed push");

    // Open through the production path (Remote::open → backend::build →
    // validate_format). The issue calls this out explicitly: live tests
    // must not use new_for_test.
    let live_remote = Remote::open(remote).await.expect("Remote::open");
    assert_eq!(live_remote.engine(), StorageEngine::Packchain);
    let cache = PackIndexCache::default();

    // First read primes the cache.
    let bytes_1 = read_blob(&live_remote, "refs/heads/main", "f0.txt", &cache)
        .await
        .expect("read_blob first call");
    assert_eq!(
        &bytes_1[..],
        body_on_disk.as_slice(),
        "read_blob bytes must match the seed file byte-for-byte",
    );

    // Pluck the .idx key from chain.json so we can delete it.
    let chain_key = join(prefix, "refs/heads/main/chain.json");
    let chain_bytes = store.get_bytes(&chain_key).await.expect("chain.json");
    let chain: serde_json::Value = serde_json::from_slice(&chain_bytes).expect("chain.json parses");
    let pack_path = chain["segments"][0]["pack"]
        .as_str()
        .expect("segment.pack string");
    let idx_key = join(prefix, &pack_path.replace(".pack", ".idx"));
    store
        .delete(&idx_key)
        .await
        .expect("delete .idx after first read");

    // Second read must succeed via the cache despite the .idx being
    // gone from the bucket. Without cache reuse this would surface
    // `PackchainError::PackMissing` — `load_index` maps a missing
    // `.idx` GET to that variant rather than passing through the
    // raw `Store(NotFound)`.
    let bytes_2 = read_blob(&live_remote, "refs/heads/main", "f0.txt", &cache)
        .await
        .expect("read_blob second call must reuse cached indices");
    assert_eq!(bytes_2, bytes_1, "second read must return identical bytes");
}

/// Phase 5: force push produces orphan packs; `mark` writes a
/// tombstone naming them; `sweep` with `grace_hours = 0`
/// (zero-grace, normal force=false code path) deletes them. Using
/// zero-grace rather than `force=true` exercises the real
/// age-vs-grace comparison in
/// [`crate::packchain::gc::sweep_one_tombstone`] instead of the
/// operator-asserted bypass.
///
/// A force-push also writes a *baseline tombstone* for the prior
/// baseline bundle (issue #134 / commit 21a9ccd) so that an in-flight
/// fetch reading the prior chain.json can still download the bundle
/// it expects through the same operator-configured grace window.
/// Sweep walks both tombstone namespaces, so the expected outcome is
/// two swept tombstones (orphan-pack + baseline) and three deleted
/// objects (pack + idx + prior baseline bundle).
///
/// Issue #69 phase 5; updated for #164 / #134 baseline-tombstone path.
pub async fn mark_then_sweep_after_grace_deletes_orphans(
    store: Arc<dyn ObjectStore>,
    remote: &RemoteUrl,
    prefix: &str,
) {
    if skip_if_no_git() {
        return;
    }
    let (seed, _) = make_seed_repo(2, "gc");

    drive_in(
        remote.clone(),
        Arc::clone(&store),
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await
    .1
    .expect("first push");

    // Capture the pack key the first push wrote — it's what we want to
    // see deleted by sweep. Also capture the prior `full_at` so we can
    // assert the baseline-tombstone path reclaims the prior baseline
    // bundle the force-push leaves behind (issue #134).
    let chain_key = join(prefix, "refs/heads/main/chain.json");
    let pre_chain: serde_json::Value =
        serde_json::from_slice(&store.get_bytes(&chain_key).await.expect("chain.json"))
            .expect("chain.json parses");
    let orphan_pack_path = pre_chain["segments"][0]["pack"]
        .as_str()
        .expect("segment.pack")
        .to_owned();
    let orphan_pack_key = join(prefix, &orphan_pack_path);
    let orphan_idx_key = join(prefix, &orphan_pack_path.replace(".pack", ".idx"));
    let prior_full_at = pre_chain["full_at"]
        .as_str()
        .expect("chain.full_at")
        .to_owned();
    let prior_baseline_key = join(prefix, &format!("refs/heads/main/{prior_full_at}.bundle"));

    // Drop second commit, force push: leaves first push's pack as
    // an unreferenced orphan (the new full-baseline pack supersedes it)
    // AND leaves the prior baseline bundle covered by a baseline
    // tombstone written by `force_push_baseline_cleanup` (issue #134).
    git(&["reset", "--hard", "HEAD~1"], seed.path());
    drive_in(
        remote.clone(),
        Arc::clone(&store),
        "push +refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await
    .1
    .expect("force push");

    // Pre-condition for sweep: the orphan pack and the now-tombstoned
    // prior baseline bundle must still be on the bucket.
    store
        .get_bytes(&orphan_pack_key)
        .await
        .expect("orphan pack present pre-mark");
    store
        .get_bytes(&prior_baseline_key)
        .await
        .expect("prior baseline bundle present pre-sweep");

    // Mark must record exactly one orphan: the deterministic setup
    // produces one chain segment pre-force-push, so a clean force push
    // leaves exactly one orphan pack. Equality assertion would catch a
    // runaway-mark regression that recorded extra orphans. Mark only
    // covers pack orphans — the baseline tombstone is independently
    // written by the force-push code path, so the count here stays 1.
    let mark_outcome = mark(store.as_ref(), prefix, MarkOpts::default())
        .await
        .expect("mark must succeed");
    assert_eq!(
        mark_outcome.orphan_count, 1,
        "deterministic setup must yield exactly 1 orphan, got {}",
        mark_outcome.orphan_count,
    );

    // Sweep with zero-grace: tombstones were written milliseconds ago,
    // so their age (rounded by `whole_hours`) is 0. With
    // `grace_hours = 0`, the `age < grace` comparison is `0 < 0 ==
    // false`, so neither tombstone is deferred. Sweep walks both
    // tombstone namespaces — the orphan-pack tombstone `mark()` wrote
    // and the baseline tombstone the force-push wrote — so two
    // tombstones are applied and three objects are deleted
    // (pack + idx + prior baseline bundle).
    let sweep_outcome = sweep(
        store.as_ref(),
        prefix,
        SweepOpts {
            grace_hours: 0,
            force: false,
        },
    )
    .await
    .expect("sweep must succeed");
    assert_eq!(
        sweep_outcome.swept_tombstones, 2,
        "sweep must apply exactly 2 tombstones (mark + force-push \
         baseline), got {}",
        sweep_outcome.swept_tombstones,
    );
    // Pack + idx + prior baseline bundle ⇒ exactly three deletions.
    // Equality (vs `>=`) catches a runaway-delete regression.
    assert_eq!(
        sweep_outcome.deleted_objects, 3,
        "sweep must delete pack + idx + prior baseline bundle (3 \
         objects), got {}",
        sweep_outcome.deleted_objects,
    );
    assert_eq!(
        sweep_outcome.deferred_tombstones, 0,
        "tombstones should not be deferred at grace_hours=0",
    );
    assert_eq!(
        sweep_outcome.skipped_repointed_packs, 0,
        "no orphan was re-referenced between mark and sweep, got {}",
        sweep_outcome.skipped_repointed_packs,
    );

    // Post-condition: the orphan pack, its idx, and the prior baseline
    // bundle are all gone — the tombstones themselves are cleaned up
    // by sweep (verified indirectly: a second sweep would see no
    // tombstones to apply).
    assert_not_found(store.as_ref(), &orphan_pack_key, "orphan pack").await;
    assert_not_found(store.as_ref(), &orphan_idx_key, "orphan idx").await;
    assert_not_found(store.as_ref(), &prior_baseline_key, "prior baseline bundle").await;
}

/// Helper: assert a key is absent (`NotFound`) on the bucket. Used by
/// post-sweep verification to keep the calling scenarios under clippy's
/// 100-line per-function ceiling without sacrificing diagnostic clarity
/// in the failure message.
async fn assert_not_found(store: &dyn ObjectStore, key: &str, label: &str) {
    let got = store.get_bytes(key).await;
    assert!(
        matches!(
            got,
            Err(git_remote_object_store::ObjectStoreError::NotFound(_))
        ),
        "{label} ({key}) must be gone after sweep, got {got:?}",
    );
}

// ---------------------------------------------------------------------------
// Local key-join helper. Empty prefix returns the suffix verbatim
// (bucket-root layout); non-empty prefix joins with `/`. Note: this
// does NOT reproduce `crate::keys::join`'s empty-suffix special case
// (which yields `"<prefix>/"` for use as a list prefix). Test scenarios
// only ever pass non-empty suffixes; making `keys::join` `pub` for
// tests would widen the public surface for one helper.
// ---------------------------------------------------------------------------

fn join(prefix: &str, suffix: &str) -> String {
    if prefix.is_empty() {
        suffix.to_owned()
    } else {
        format!("{prefix}/{suffix}")
    }
}

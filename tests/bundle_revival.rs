//! Sequential Bundle revival regression for issue #303. The helper protocol
//! requires `ok <ref>\n\n` to identify a successfully published ref; a fresh
//! advertisement and fetch must then expose that exact requested commit.
//! Native Git freezes every reachable raw object before publication and imports
//! the selected bundle into a separate empty object database as an oracle.

#![cfg(feature = "test-util")]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use git_remote_object_store::object_store::mock::{Fault, MockStore};
use git_remote_object_store::object_store::{
    GetOpts, ObjectMeta, ObjectStore, ObjectStoreError, PutOpts,
};
use git_remote_object_store::protocol::ProtocolError;
use time::OffsetDateTime;

use common::{drive_in, git, git_available, make_seed_repo, s3_url};

type RawGraph = BTreeMap<String, (String, Vec<u8>)>;
type Snapshot = BTreeMap<String, (Bytes, OffsetDateTime, Option<String>)>;

fn object_key(prefix: Option<&str>, suffix: &str) -> String {
    prefix
        .filter(|p| !p.is_empty())
        .map_or_else(|| suffix.to_owned(), |p| format!("{p}/{suffix}"))
}

fn native(repo: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("native Git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn text(repo: &Path, args: &[&str]) -> String {
    String::from_utf8(native(repo, args)).expect("Git object identities are ASCII")
}

fn raw_graph(repo: &Path, tip: &str) -> RawGraph {
    text(repo, &["rev-list", "--objects", "--no-object-names", tip])
        .lines()
        .map(|oid| {
            let kind = text(repo, &["cat-file", "-t", oid]).trim().to_owned();
            let body = native(repo, &["cat-file", &kind, oid]);
            (oid.to_owned(), (kind, body))
        })
        .collect()
}

async fn protocol(
    store: Arc<dyn ObjectStore>,
    prefix: Option<&str>,
    repo: &Path,
    script: &str,
) -> (Vec<u8>, Result<(), ProtocolError>) {
    drive_in(s3_url(prefix), store, script, repo.to_path_buf()).await
}

async fn push(
    store: Arc<dyn ObjectStore>,
    prefix: Option<&str>,
    repo: &Path,
    oid: &str,
    force: bool,
) {
    let sign = if force { "+" } else { "" };
    let script = format!("push {sign}{oid}:refs/heads/main\n\n");
    let (out, result) = protocol(store, prefix, repo, &script).await;
    result.expect("well-formed push protocol must complete");
    assert_eq!(
        out, b"ok refs/heads/main\n\n",
        "requested push must be acknowledged exactly"
    );
}

async fn snapshot(store: &MockStore, keys: impl IntoIterator<Item = String>) -> Snapshot {
    let mut result = Snapshot::new();
    for key in keys {
        let meta = store.head(&key).await.expect("snapshot object exists");
        result.insert(
            key.clone(),
            (
                store.get_bytes(&key).await.unwrap(),
                meta.last_modified,
                meta.etag,
            ),
        );
    }
    result
}

async fn assert_visible_and_readable(
    store: Arc<MockStore>,
    prefix: Option<&str>,
    repo: &Path,
    oid: &str,
    expected: &RawGraph,
) {
    let before = snapshot(&store, store.keys()).await;
    let (out, result) = protocol(store.clone(), prefix, repo, "list\n").await;
    result.expect("fresh list");
    assert_eq!(
        out,
        format!("@refs/heads/main HEAD\n{oid} refs/heads/main\n\n").as_bytes()
    );

    let fetched = tempfile::tempdir().unwrap();
    git(&["init", "--quiet", "--bare"], fetched.path());
    let (out, result) = protocol(
        store.clone(),
        prefix,
        fetched.path(),
        &format!("fetch {oid} refs/heads/main\n\n"),
    )
    .await;
    result.expect("fresh advertised-OID fetch");
    assert_eq!(out, b"\n", "fetch protocol terminator");
    assert_eq!(raw_graph(fetched.path(), oid), *expected);
    git(
        &["fsck", "--full", "--strict", "--no-reflogs", oid],
        fetched.path(),
    );

    let standalone = tempfile::tempdir().unwrap();
    git(&["init", "--quiet", "--bare"], standalone.path());
    let bundle = standalone.path().join("selected.bundle");
    let body_key = object_key(prefix, &format!("refs/heads/main/{oid}.bundle"));
    std::fs::write(&bundle, store.get_bytes(&body_key).await.unwrap()).unwrap();
    git(
        &["bundle", "unbundle", bundle.to_str().unwrap()],
        standalone.path(),
    );
    assert!(!standalone.path().join("objects/info/alternates").exists());
    assert_eq!(raw_graph(standalone.path(), oid), *expected);
    git(
        &["fsck", "--full", "--strict", "--no-reflogs", oid],
        standalone.path(),
    );
    assert_eq!(
        snapshot(&store, store.keys()).await,
        before,
        "readers must not change storage"
    );
}

struct Fixture {
    repo: tempfile::TempDir,
    store: Arc<MockStore>,
    a: String,
    b: String,
    ancestor: String,
    graph_a: RawGraph,
    graph_b: RawGraph,
}

impl Fixture {
    async fn new(prefix: Option<&str>) -> Self {
        let (repo, shas) = make_seed_repo(3, "bundle-revival");
        let a = shas[1].clone();
        let b = shas[2].clone();
        let graph_a = raw_graph(repo.path(), &a);
        let graph_b = raw_graph(repo.path(), &b);
        assert_eq!(text(repo.path(), &["rev-list", "--count", &a]).trim(), "2");
        assert_eq!(text(repo.path(), &["rev-list", "--count", &b]).trim(), "3");
        let store = Arc::new(MockStore::new());
        push(store.clone(), prefix, repo.path(), &a, false).await;
        push(store.clone(), prefix, repo.path(), &b, false).await;
        Self {
            repo,
            store,
            a,
            b,
            ancestor: shas[0].clone(),
            graph_a,
            graph_b,
        }
    }

    async fn matching_marks(&self, prefix: Option<&str>, oid: &str) -> BTreeSet<String> {
        let mut keys = BTreeSet::new();
        for meta in self
            .store
            .list(&object_key(prefix, "gc/baseline-tomb-"))
            .await
            .unwrap()
        {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(
                &self.store.get_bytes(&meta.key).await.unwrap(),
            ) && value["v"] == 1
                && value["ref_name"] == "refs/heads/main"
                && value["sha"] == oid
            {
                keys.insert(meta.key);
            }
        }
        keys
    }

    async fn extra_marks(&self, prefix: Option<&str>) -> (BTreeSet<String>, Snapshot) {
        let original = self.matching_marks(prefix, &self.a).await;
        assert_eq!(original.len(), 1, "ordinary A to B must retire A once");
        let body = self
            .store
            .get_bytes(original.first().unwrap())
            .await
            .unwrap();
        for suffix in [
            "gc/baseline-tomb-revival-copy-1.json",
            "gc/baseline-tomb-revival-copy-2.json",
        ] {
            self.store.insert(object_key(prefix, suffix), body.clone());
        }
        let valid = |ref_name: &str, sha: &str, version: u32| {
            Bytes::from(serde_json::to_vec(&serde_json::json!({
            "v": version, "marked_at": "2026-01-01T00:00:00Z", "ref_name": ref_name, "sha": sha,
        })).unwrap())
        };
        let preserved = [
            (
                "gc/baseline-tomb-other-ref.json",
                valid("refs/heads/other", &self.a, 1),
            ),
            (
                "gc/baseline-tomb-other-sha.json",
                valid("refs/heads/main", &self.ancestor, 1),
            ),
            (
                "gc/baseline-tomb-malformed.json",
                Bytes::from_static(b"not JSON"),
            ),
            (
                "gc/baseline-tomb-other-version.json",
                valid("refs/heads/main", &self.a, 2),
            ),
            (
                "gc/baseline-tomb-invalid-ref.json",
                valid("refs/heads/../invalid", &self.a, 1),
            ),
            (
                "gc/baseline-tomb-invalid-sha.json",
                valid("refs/heads/main", "invalid", 1),
            ),
        ];
        let mut keys = Vec::new();
        for (suffix, body) in preserved {
            let key = object_key(prefix, suffix);
            self.store.insert(&key, body);
            keys.push(key);
        }
        (
            self.matching_marks(prefix, &self.a).await,
            snapshot(&self.store, keys).await,
        )
    }
}

async fn revival(prefix: Option<&str>) {
    let fixture = Fixture::new(prefix).await;
    assert_visible_and_readable(
        fixture.store.clone(),
        prefix,
        fixture.repo.path(),
        &fixture.b,
        &fixture.graph_b,
    )
    .await;
    for _ in 0..2 {
        push(
            fixture.store.clone(),
            prefix,
            fixture.repo.path(),
            &fixture.a,
            true,
        )
        .await;
        assert_visible_and_readable(
            fixture.store.clone(),
            prefix,
            fixture.repo.path(),
            &fixture.a,
            &fixture.graph_a,
        )
        .await;
        assert!(fixture.matching_marks(prefix, &fixture.a).await.is_empty());
        assert!(!fixture.matching_marks(prefix, &fixture.b).await.is_empty());
        push(
            fixture.store.clone(),
            prefix,
            fixture.repo.path(),
            &fixture.b,
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn force_revived_tip_is_advertised_and_fetchable_at_bucket_root() {
    assert!(git_available(), "native Git oracle is required");
    revival(None).await;
}

#[tokio::test]
async fn force_revived_tip_is_advertised_and_fetchable_at_named_prefix() {
    assert!(git_available(), "native Git oracle is required");
    revival(Some("nested/repository")).await;
}

#[tokio::test]
async fn revival_clears_all_exact_marks_and_preserves_unrelated_and_malformed_records() {
    assert!(git_available(), "native Git oracle is required");
    for prefix in [None, Some("nested/repository")] {
        let fixture = Fixture::new(prefix).await;
        let (matching, preserved) = fixture.extra_marks(prefix).await;
        assert_eq!(matching.len(), 3);
        push(
            fixture.store.clone(),
            prefix,
            fixture.repo.path(),
            &fixture.a,
            true,
        )
        .await;
        assert!(fixture.matching_marks(prefix, &fixture.a).await.is_empty());
        assert_eq!(
            snapshot(&fixture.store, preserved.keys().cloned()).await,
            preserved
        );
        assert_visible_and_readable(
            fixture.store.clone(),
            prefix,
            fixture.repo.path(),
            &fixture.a,
            &fixture.graph_a,
        )
        .await;
    }
}

#[derive(Clone, Copy)]
enum DeleteFailure {
    BeforeAcceptance,
    AfterAcceptance,
}

struct DeleteState {
    matching: BTreeSet<String>,
    fail_at: usize,
    failure: DeleteFailure,
    attempts: Vec<(String, bool)>,
}

struct DeleteFaultStore {
    inner: Arc<MockStore>,
    state: Mutex<DeleteState>,
}

#[async_trait]
impl ObjectStore for DeleteFaultStore {
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, ObjectStoreError> {
        self.inner.list(prefix).await
    }
    async fn get_to_file(
        &self,
        key: &str,
        dest: &Path,
        opts: GetOpts,
    ) -> Result<(), ObjectStoreError> {
        self.inner.get_to_file(key, dest, opts).await
    }
    async fn get_bytes(&self, key: &str) -> Result<Bytes, ObjectStoreError> {
        self.inner.get_bytes(key).await
    }
    async fn get_bytes_range(
        &self,
        key: &str,
        range: std::ops::Range<u64>,
    ) -> Result<Bytes, ObjectStoreError> {
        self.inner.get_bytes_range(key, range).await
    }
    async fn put_bytes(
        &self,
        key: &str,
        body: Bytes,
        opts: PutOpts,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_bytes(key, body, opts).await
    }
    async fn put_path(&self, key: &str, src: &Path, opts: PutOpts) -> Result<(), ObjectStoreError> {
        self.inner.put_path(key, src, opts).await
    }
    async fn put_if_absent(&self, key: &str, body: Bytes) -> Result<bool, ObjectStoreError> {
        self.inner.put_if_absent(key, body).await
    }
    async fn head(&self, key: &str) -> Result<ObjectMeta, ObjectStoreError> {
        self.inner.head(key).await
    }
    async fn copy(&self, src: &str, dst: &str) -> Result<(), ObjectStoreError> {
        self.inner.copy(src, dst).await
    }
    async fn delete(&self, key: &str) -> Result<(), ObjectStoreError> {
        let fault = {
            let mut state = self.state.lock().unwrap();
            if state.matching.contains(key) {
                let fail = state.attempts.len() + 1 == state.fail_at;
                state.attempts.push((key.to_owned(), fail));
                fail.then_some(state.failure)
            } else {
                None
            }
        };
        if !matches!(fault, Some(DeleteFailure::BeforeAcceptance)) {
            self.inner.delete(key).await?;
        }
        if fault.is_some() {
            Err(ObjectStoreError::Network(Box::new(std::io::Error::other(
                "synthetic marker delete",
            ))))
        } else {
            Ok(())
        }
    }
}

async fn interrupted_revival(prefix: Option<&str>, failure: DeleteFailure, fail_at: usize) {
    let fixture = Fixture::new(prefix).await;
    let (matching, preserved) = fixture.extra_marks(prefix).await;
    let faulty = Arc::new(DeleteFaultStore {
        inner: fixture.store.clone(),
        state: Mutex::new(DeleteState {
            matching: matching.clone(),
            fail_at,
            failure,
            attempts: Vec::new(),
        }),
    });
    let script = format!("push +{}:refs/heads/main\n\n", fixture.a);
    let (out, result) = protocol(faulty.clone(), prefix, fixture.repo.path(), &script).await;
    result.expect("operational failure is rendered per ref without breaking protocol");
    assert_eq!(out, b"error refs/heads/main \"object-store error during push: network error: synthetic marker delete\"?\n\n");
    let attempts = faulty.state.lock().unwrap().attempts.clone();
    assert_eq!(
        attempts.len(),
        fail_at,
        "selected DELETE fault must actually fire"
    );
    assert!(attempts.last().unwrap().1);
    let removed = fail_at - usize::from(matches!(failure, DeleteFailure::BeforeAcceptance));
    assert_eq!(
        matching
            .iter()
            .filter(|key| !fixture.store.contains(key))
            .count(),
        removed
    );
    assert_eq!(
        snapshot(&fixture.store, preserved.keys().cloned()).await,
        preserved
    );
    let body_key = object_key(prefix, &format!("refs/heads/main/{}.bundle", fixture.a));
    assert!(
        fixture.store.contains(&body_key),
        "requested complete body remains durable"
    );
    assert!(
        !fixture.matching_marks(prefix, &fixture.b).await.is_empty(),
        "predecessor retired before visibility completion"
    );
    assert!(
        !fixture
            .store
            .contains(&object_key(prefix, "refs/heads/main/LOCK#.lock"))
    );
    let (listing, result) =
        protocol(fixture.store.clone(), prefix, fixture.repo.path(), "list\n").await;
    result.unwrap();
    if removed == matching.len() {
        assert_eq!(
            listing,
            format!("@refs/heads/main HEAD\n{} refs/heads/main\n\n", fixture.a).as_bytes()
        );
    } else {
        assert_eq!(
            listing, b"\n",
            "incomplete revival must not advertise the predecessor"
        );
    }
    // A fresh explicit invocation retries the exact intended OID, without repair.
    push(
        fixture.store.clone(),
        prefix,
        fixture.repo.path(),
        &fixture.a,
        true,
    )
    .await;
    assert!(fixture.matching_marks(prefix, &fixture.a).await.is_empty());
    assert_eq!(
        snapshot(&fixture.store, preserved.keys().cloned()).await,
        preserved
    );
    assert_visible_and_readable(
        fixture.store.clone(),
        prefix,
        fixture.repo.path(),
        &fixture.a,
        &fixture.graph_a,
    )
    .await;
    assert_eq!(
        text(fixture.repo.path(), &["rev-parse", "HEAD"]).trim(),
        fixture.b,
        "publishing a literal OID must not reset the caller's HEAD"
    );
}

#[tokio::test]
async fn partial_marker_delete_rejection_reports_failure_and_explicit_retry_converges() {
    assert!(git_available(), "native Git oracle is required");
    for prefix in [None, Some("nested/repository")] {
        interrupted_revival(prefix, DeleteFailure::BeforeAcceptance, 2).await;
    }
}

#[tokio::test]
async fn marker_delete_lost_reply_reports_failure_even_if_visibility_completed() {
    assert!(git_available(), "native Git oracle is required");
    for prefix in [None, Some("nested/repository")] {
        for fail_at in [2, 3] {
            interrupted_revival(prefix, DeleteFailure::AfterAcceptance, fail_at).await;
        }
    }
}

#[tokio::test]
async fn predecessor_retirement_failure_preserves_visible_tip_until_explicit_retry() {
    assert!(git_available(), "native Git oracle is required");
    for prefix in [None, Some("nested/repository")] {
        let fixture = Fixture::new(prefix).await;
        let matching = fixture.matching_marks(prefix, &fixture.a).await;
        assert_eq!(matching.len(), 1);
        let predecessor = object_key(prefix, &format!("refs/heads/main/{}.bundle", fixture.b));
        let mut guarded = matching.clone();
        guarded.insert(predecessor.clone());
        let before = snapshot(&fixture.store, guarded.iter().cloned()).await;
        fixture.store.arm(Fault::NetworkOnPutBytesPrefix {
            prefix: object_key(prefix, "gc/baseline-tomb-"),
        });
        fixture
            .store
            .arm(Fault::NetworkOnDelete { key: predecessor });
        let script = format!("push +{}:refs/heads/main\n\n", fixture.a);
        let (out, result) =
            protocol(fixture.store.clone(), prefix, fixture.repo.path(), &script).await;
        result.expect("operational error is encoded per ref");
        assert_eq!(out, b"error refs/heads/main \"object-store error during push: network error: bundle revival retirement incomplete; retry the same explicit tip\"?\n\n");
        assert_eq!(
            fixture.store.pending_faults(),
            0,
            "both retirement failures must fire"
        );
        assert_eq!(snapshot(&fixture.store, guarded).await, before);
        assert_visible_and_readable(
            fixture.store.clone(),
            prefix,
            fixture.repo.path(),
            &fixture.b,
            &fixture.graph_b,
        )
        .await;
        push(
            fixture.store.clone(),
            prefix,
            fixture.repo.path(),
            &fixture.a,
            true,
        )
        .await;
        assert_visible_and_readable(
            fixture.store.clone(),
            prefix,
            fixture.repo.path(),
            &fixture.a,
            &fixture.graph_a,
        )
        .await;
    }
}

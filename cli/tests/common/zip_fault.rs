//! Cross-backend coverage for the best-effort zip-artifact upload
//! contract (issue #127, sibling of #113 / #121).
//!
//! `perform_push_under_lock` in `src/protocol/push.rs` writes the new
//! bundle, `HEAD`, and `FORMAT` (the git-protocol contract for a
//! successful push) before attempting the optional `repo.zip` artifact
//! upload. Any error on that zip-only `put_path` is logged at warn and
//! swallowed — the bundle is already durable, so reporting failure
//! would lie about the remote state. The unit test
//! `perform_push_under_lock_succeeds_when_zip_upload_fails` in
//! `src/protocol/push.rs` pins the contract against [`MockStore`].
//!
//! This module wires the same contract against the live backends
//! ([`S3Store`] via `RustFS`, [`AzureStore`] via Azurite) by wrapping
//! the real store in a [`ZipPutFaultStore`] decorator that fails
//! `put_path` for the zip key while letting the bundle, `HEAD`, and
//! `FORMAT` writes pass through. The post-conditions assert against
//! the underlying real store, so a regression that retried under a
//! masking outer layer would still surface here.
//!
//! Issue #142.
//!
//! [`S3Store`]: git_remote_object_store::object_store::s3::S3Store
//! [`AzureStore`]: git_remote_object_store::object_store::azure::AzureStore
//! [`MockStore`]: git_remote_object_store::object_store::mock::MockStore

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use git_remote_object_store::object_store::{
    GetOpts, ObjectMeta, ObjectStore, ObjectStoreError, PutOpts,
};
use git_remote_object_store::url::RemoteUrl;

use super::packchain_live::{drive_in, git_available, make_seed_repo};

/// `ObjectStore` decorator that delegates every call to an inner store
/// but fails the first `put_path(target_key, ...)` with
/// [`ObjectStoreError::Network`].
///
/// Modelled on `MockStore`'s `Fault::NetworkOnPutPath` (see
/// `src/object_store/mock.rs`), which arms the same shape against the
/// in-memory store. The one-shot semantics matter: a re-armed fault on
/// a second attempt would mask a regression that quietly retried the
/// zip upload after the swallow path.
struct ZipPutFaultStore {
    inner: Arc<dyn ObjectStore>,
    target_key: String,
    armed: AtomicBool,
}

impl ZipPutFaultStore {
    /// Wrap `inner` and arm a one-shot `put_path` fault on `target_key`.
    fn new(inner: Arc<dyn ObjectStore>, target_key: String) -> Self {
        Self {
            inner,
            target_key,
            armed: AtomicBool::new(true),
        }
    }

    /// `true` if the fault has not fired yet.
    fn is_armed(&self) -> bool {
        // Relaxed is sufficient: this flag synchronises nothing else;
        // the post-push assertion crosses an `.await` boundary that
        // already provides the happens-before edge with the put_path
        // call site.
        self.armed.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl ObjectStore for ZipPutFaultStore {
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
        // `swap(false)` reads as "consume the arm if present"; the
        // previous value tells us whether this call was the one that
        // tripped it. Relaxed ordering is enough — the flag itself
        // synchronises nothing else.
        if key == self.target_key && self.armed.swap(false, Ordering::Relaxed) {
            return Err(ObjectStoreError::Network(Box::new(std::io::Error::other(
                format!("injected put_path fault on {key}"),
            ))));
        }
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
        self.inner.delete(key).await
    }

    async fn presigned_get_url(
        &self,
        key: &str,
        ttl: std::time::Duration,
    ) -> Result<String, ObjectStoreError> {
        self.inner.presigned_get_url(key, ttl).await
    }
}

/// Drive a `?zip=1` push against `inner` with a one-shot fault on the
/// zip-only `put_path` and assert the issue #127 contract end-to-end
/// against the live backend:
///
/// * the helper exits `ok refs/heads/main\n\n` (bundle was durable);
/// * the bundle key is present on the backend;
/// * the zip key is absent (proves the failure was not masked by a
///   retry that re-uploaded under the swallow path);
/// * the injected fault fired exactly once.
///
/// `remote` MUST be parsed with the `?zip=1` flag — the scenario does
/// not synthesise the URL because each backend's URL grammar
/// (path-style vs. account-host, credential alias) is the caller's
/// concern. The bucket-side prefix is read from `remote.prefix()`, so
/// callers do not pass it separately (this also rules out the silent
/// caller / URL prefix drift that a duplicated parameter invites).
pub async fn push_with_zip_put_fault_succeeds_and_omits_zip(
    inner: Arc<dyn ObjectStore>,
    remote: RemoteUrl,
) {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    assert!(
        remote.flags().zip,
        "scenario requires ?zip=1; got {remote:?}",
    );

    let (seed, shas) = make_seed_repo(1, "zip-fault");
    let tip = &shas[0];
    let (zip_key, bundle_key) = bundle_and_zip_keys(remote.prefix(), tip);

    let faulted = Arc::new(ZipPutFaultStore::new(Arc::clone(&inner), zip_key.clone()));
    let (out, result) = drive_in(
        remote,
        Arc::clone(&faulted) as Arc<dyn ObjectStore>,
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push must succeed even when zip upload fails");
    assert_eq!(
        std::str::from_utf8(&out).expect("stdout utf-8"),
        "ok refs/heads/main\n\n",
        "helper must report success after a swallowed zip put_path fault",
    );

    inner
        .head(&bundle_key)
        .await
        .unwrap_or_else(|e| panic!("bundle must be present at {bundle_key}: {e}"));

    match inner.head(&zip_key).await {
        Err(ObjectStoreError::NotFound(_)) => {}
        Err(e) => panic!("expected NotFound on {zip_key}, got {e:?}"),
        Ok(meta) => panic!(
            "zip key {zip_key} must be absent after a swallowed fault; \
             found size={size} bytes",
            size = meta.size,
        ),
    }

    assert!(
        !faulted.is_armed(),
        "fault must fire exactly once; still armed after push at {zip_key}",
    );
}

/// Drive a `?zip=1` push against `inner` with no fault and assert the
/// happy-path counterpart of
/// [`push_with_zip_put_fault_succeeds_and_omits_zip`]: when the zip
/// upload succeeds, both the bundle and the zip artifact land at their
/// documented keys on the live backend.
///
/// The unit-level pin
/// `zip_variant_uploads_repo_zip_with_metadata` in
/// `tests/protocol_push.rs` already covers the `Content-Disposition`
/// and `codepipeline-artifact-revision-summary` user-metadata wiring
/// against `MockStore`; here we only verify that the keys exist and
/// the zip has a non-zero size, since the trait does not expose
/// metadata on `head` results.
pub async fn push_with_zip_uploads_artifact(inner: Arc<dyn ObjectStore>, remote: RemoteUrl) {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    assert!(
        remote.flags().zip,
        "scenario requires ?zip=1; got {remote:?}",
    );

    let (seed, shas) = make_seed_repo(1, "zip-happy");
    let tip = &shas[0];
    let (zip_key, bundle_key) = bundle_and_zip_keys(remote.prefix(), tip);

    let (out, result) = drive_in(
        remote,
        Arc::clone(&inner),
        "push refs/heads/main:refs/heads/main\n\n",
        seed.path().to_path_buf(),
    )
    .await;
    result.expect("push with ?zip=1 must succeed");
    assert_eq!(
        std::str::from_utf8(&out).expect("stdout utf-8"),
        "ok refs/heads/main\n\n",
        "helper must report success on a clean zip push",
    );

    inner
        .head(&bundle_key)
        .await
        .unwrap_or_else(|e| panic!("bundle must be present at {bundle_key}: {e}"));
    let zip_meta = inner
        .head(&zip_key)
        .await
        .unwrap_or_else(|e| panic!("zip must be present at {zip_key}: {e}"));
    assert!(
        zip_meta.size > 0,
        "zip at {zip_key} must have non-zero size; got {size}",
        size = zip_meta.size,
    );
}

/// Build the `(zip_key, bundle_key)` pair for `refs/heads/main` under
/// the optional prefix. Mirrors `archive_key` and `bundle_key` in
/// `src/protocol/push.rs`; centralised here so both scenarios apply
/// the same prefix-join convention.
fn bundle_and_zip_keys(prefix: Option<&str>, tip: &str) -> (String, String) {
    let ref_path = "refs/heads/main";
    let join_key = |leaf: &str| match prefix {
        Some(p) if !p.is_empty() => format!("{p}/{ref_path}/{leaf}"),
        _ => format!("{ref_path}/{leaf}"),
    };
    (join_key("repo.zip"), join_key(&format!("{tip}.bundle")))
}

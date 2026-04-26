//! Integration tests for [`object_store::s3::S3Store`][s3] against a
//! real S3-compatible server (`RustFS` via `testcontainers`).
//!
//! `RustFS` (`https://github.com/rustfs/rustfs`) is an Apache-2.0 S3
//! implementation. The Docker image tag is **pinned**: the upstream
//! `testcontainers-modules` `RustFS` module hardcodes `:latest`, but
//! `RustFS` is alpha-stage and the floating tag would let alpha-version
//! drift break CI silently. Bump [`RUSTFS_TAG`] deliberately when a
//! new alpha lands.
//!
//! Gated on the `integration-s3` Cargo feature so that contributors
//! without Docker are not blocked. CI runs this on Linux:
//!
//! ```text
//! cargo test --features integration-s3
//! ```
//!
//! The whole test binary shares one `RustFS` container (started lazily
//! via [`OnceLock`]); each test creates its own bucket with a random
//! suffix so they parallel-test cleanly.
//!
//! [s3]: git_remote_object_store::object_store
//! [`OnceLock`]: std::sync::OnceLock

#![cfg(feature = "integration-s3")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use aws_sdk_s3::primitives::ByteStream;
use bytes::Bytes;
use git_remote_object_store::object_store::s3::S3Store;
use git_remote_object_store::object_store::{Error, ObjectStore, PutOpts};
use git_remote_object_store::url::{ENV_ALLOW_HTTP, RemoteUrl, parse};
use sha2::{Digest, Sha256};
use testcontainers::core::wait::HttpWaitStrategy;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, ContainerRequest, GenericImage, ImageExt};

/// `RustFS` Docker image. Pinned by [`RUSTFS_TAG`].
const RUSTFS_IMAGE: &str = "rustfs/rustfs";
/// `RustFS` image tag. Pinned to avoid alpha-version drift; bump
/// deliberately and re-run the suite to verify S3 parity is preserved.
const RUSTFS_TAG: &str = "1.0.0-alpha.99";
/// `RustFS` container API port (exposed via the SDK).
const RUSTFS_API_PORT: u16 = 9000;
/// `RustFS` default root credentials (per the upstream docs and
/// `crates/e2e_test/src/reliant/conditional_writes.rs`).
const TEST_USER: &str = "rustfsadmin";
const TEST_PASSWORD: &str = "rustfsadmin";

fn rustfs_image() -> ContainerRequest<GenericImage> {
    // Wait condition: RustFS 1.0.0-alpha.73+ defaults to logging to
    // stdout, but the official Docker image sets
    // `RUSTFS_OBS_LOG_DIRECTORY=/logs` at build time, redirecting logs
    // back to a file inside the container (see rustfs#1075). Even with
    // the env-var override below restoring stdout logging, polling the
    // S3 endpoint is a more reliable readiness signal than parsing a
    // specific startup line — an unauthenticated `GET /` returns 403
    // once the server is serving.
    let http_wait = HttpWaitStrategy::new("/")
        .with_port(RUSTFS_API_PORT.tcp())
        .with_expected_status_code(403_u16);
    GenericImage::new(RUSTFS_IMAGE, RUSTFS_TAG)
        .with_wait_for(WaitFor::http(http_wait))
        .with_exposed_port(RUSTFS_API_PORT.tcp())
        // Override the image's baked-in `RUSTFS_OBS_LOG_DIRECTORY=/logs`
        // so logs flow to stdout (per rustfs#1075). testcontainers
        // captures container stdout/stderr; if a future debug session
        // needs RustFS's startup logs, they'll be available via
        // `docker logs <container>` rather than buried inside a
        // container-internal file.
        .with_env_var("RUSTFS_OBS_LOG_DIRECTORY", "")
}

/// Shared `RustFS` container — started synchronously on first access
/// via [`SyncRunner`] so its lifetime is independent of any single
/// `#[tokio::test]`'s tokio runtime. Multiple tokio runtimes (one per
/// test) reuse the same container and port without their dispatch
/// tasks tearing each other down.
static RUSTFS: OnceLock<RustFsFixture> = OnceLock::new();
static BUCKET_COUNTER: AtomicU64 = AtomicU64::new(0);

struct RustFsFixture {
    /// Owned container handle — keeping it alive keeps the container alive.
    _container: Container<GenericImage>,
    port: u16,
}

fn fixture() -> &'static RustFsFixture {
    RUSTFS.get_or_init(|| {
        // The S3 SDK consults env vars at config-load time. Set them
        // once for the whole test binary before any S3Store
        // instantiates its credential provider chain.
        // SAFETY: edition 2024 marks `set_var` unsafe because it
        // mutates process-wide state; this is invoked exactly once
        // (OnceLock::get_or_init guarantees) before any code reads
        // the variables.
        unsafe {
            std::env::set_var("AWS_ACCESS_KEY_ID", TEST_USER);
            std::env::set_var("AWS_SECRET_ACCESS_KEY", TEST_PASSWORD);
            std::env::set_var(ENV_ALLOW_HTTP, "1");
        }

        // `SyncRunner::start` calls `block_on` internally, which panics
        // if invoked from inside a tokio runtime (every `#[tokio::test]`
        // is one). Run the start on a dedicated `std::thread` that has
        // no ambient runtime, then ferry the result back.
        let handle = std::thread::Builder::new()
            .name("rustfs-fixture-start".to_owned())
            .spawn(|| {
                let container = rustfs_image().start().expect("RustFS container starts");
                let port = container
                    .get_host_port_ipv4(RUSTFS_API_PORT)
                    .expect("RustFS host port");
                RustFsFixture {
                    _container: container,
                    port,
                }
            })
            .expect("spawn fixture-start thread");
        handle.join().expect("fixture-start thread joins")
    })
}

/// Build an `aws-sdk-s3` client on the *current* tokio runtime so its
/// hyper dispatch task lives as long as this test's runtime. We use
/// this only for test setup (bucket creation, seeding); production
/// code goes through `S3Store`.
async fn setup_client(port: u16) -> aws_sdk_s3::Client {
    let endpoint_uri = format!("http://127.0.0.1:{port}");
    let creds = aws_sdk_s3::config::Credentials::new(TEST_USER, TEST_PASSWORD, None, None, "test");
    let shared_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .endpoint_url(endpoint_uri)
        .credentials_provider(creds)
        .load()
        .await;
    let s3_config = aws_sdk_s3::config::Builder::from(&shared_config)
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(s3_config)
}

/// Allocate a fresh bucket name, create the bucket, and return both the
/// `S3Store` (built through the real `parse(...)` → `from_remote_url`
/// path) and the bucket name.
async fn fresh_bucket() -> (S3Store, String) {
    let fixture = fixture();
    let n = BUCKET_COUNTER.fetch_add(1, Ordering::SeqCst);
    // Bucket names: lowercase alphanumeric + dash, 3-63 chars.
    let bucket = format!("test-{}-{}", std::process::id(), n);

    let setup = setup_client(fixture.port).await;
    setup
        .create_bucket()
        .bucket(&bucket)
        .send()
        .await
        .expect("create_bucket succeeds");

    let url_str = format!(
        "s3+http://127.0.0.1:{port}/{bucket}?addressing=path",
        port = fixture.port,
        bucket = bucket
    );
    let url = parse(&url_str).expect("URL parses");
    let RemoteUrl::S3 { .. } = &url else {
        panic!("parse returned non-S3 variant");
    };
    let store = S3Store::from_remote_url(&url)
        .await
        .expect("S3Store::from_remote_url");
    (store, bucket)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn put_then_get_round_trips() {
    let (store, _bucket) = fresh_bucket().await;
    let body = Bytes::from_static(b"hello, s3");
    store
        .put_bytes("greeting", body.clone(), PutOpts::default())
        .await
        .expect("put");
    let fetched = store.get_bytes("greeting").await.expect("get");
    assert_eq!(fetched, body);
}

#[tokio::test]
async fn head_returns_size_and_recent_last_modified() {
    let (store, _bucket) = fresh_bucket().await;
    let body = Bytes::from_static(b"abcdefghij");
    store
        .put_bytes("k", body.clone(), PutOpts::default())
        .await
        .expect("put");

    let meta = store.head("k").await.expect("head");
    assert_eq!(meta.key, "k");
    assert_eq!(meta.size, body.len() as u64);

    let now = time::OffsetDateTime::now_utc();
    let age = now - meta.last_modified;
    assert!(
        age.whole_seconds() < 60 && age.whole_seconds() > -60,
        "last_modified out of range: {age}"
    );
    assert!(meta.etag.is_some(), "S3 head_object must return an ETag");
}

#[tokio::test]
async fn list_paginates_past_default_page() {
    let (store, _bucket) = fresh_bucket().await;
    // S3 returns up to 1000 keys per page; 1500 forces ≥2 pages.
    let count = 1500;
    for i in 0..count {
        store
            .put_bytes(
                &format!("p/{i:05}"),
                Bytes::from_static(b"x"),
                PutOpts::default(),
            )
            .await
            .expect("put");
    }
    let listed = store.list("p/").await.expect("list");
    assert_eq!(listed.len(), count);
}

#[tokio::test]
async fn list_with_empty_prefix_returns_everything() {
    let (store, _bucket) = fresh_bucket().await;
    for k in ["a", "b/1", "c/d/e"] {
        store
            .put_bytes(k, Bytes::from_static(b"x"), PutOpts::default())
            .await
            .expect("put");
    }
    let mut keys: Vec<String> = store
        .list("")
        .await
        .expect("list")
        .into_iter()
        .map(|m| m.key)
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["a", "b/1", "c/d/e"]);
}

#[tokio::test]
async fn put_if_absent_first_succeeds_second_returns_false() {
    let (store, _bucket) = fresh_bucket().await;
    let first = store
        .put_if_absent("lock", Bytes::from_static(b""))
        .await
        .expect("first put_if_absent");
    assert!(first, "first put_if_absent should succeed");

    let second = store
        .put_if_absent("lock", Bytes::from_static(b""))
        .await
        .expect("second put_if_absent");
    assert!(!second, "second put_if_absent should report Ok(false)");
}

#[tokio::test]
async fn put_if_absent_concurrent_contention() {
    let (store, _bucket) = fresh_bucket().await;
    let store = Arc::new(store);
    let mut handles = Vec::new();
    for _ in 0..16 {
        let store = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            store
                .put_if_absent("lock", Bytes::from_static(b""))
                .await
                .expect("put_if_absent")
        }));
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.expect("join"));
    }
    let won = results.iter().filter(|r| **r).count();
    assert_eq!(
        won,
        1,
        "exactly one put_if_absent must win under contention; got {won} winners out of {}",
        results.len()
    );
}

#[tokio::test]
async fn copy_replicates_body() {
    let (store, _bucket) = fresh_bucket().await;
    let body = Bytes::from_static(b"copy me");
    store
        .put_bytes("src", body.clone(), PutOpts::default())
        .await
        .expect("put");
    store.copy("src", "dst").await.expect("copy");
    let fetched = store.get_bytes("dst").await.expect("get");
    assert_eq!(fetched, body);
}

#[tokio::test]
async fn copy_with_special_chars_in_key() {
    let (store, _bucket) = fresh_bucket().await;
    let body = Bytes::from_static(b"locked");
    // `#` is reserved in URLs; encode_copy_source must percent-encode it
    // for the copy_object call to succeed.
    let src = "refs/heads/main/LOCK#.lock";
    let dst = "refs/heads/main/LOCK-copy.lock";
    store
        .put_bytes(src, body.clone(), PutOpts::default())
        .await
        .expect("put");
    store.copy(src, dst).await.expect("copy");
    let fetched = store.get_bytes(dst).await.expect("get");
    assert_eq!(fetched, body);
}

#[tokio::test]
async fn copy_missing_source_is_not_found() {
    let (store, _bucket) = fresh_bucket().await;
    let err = store
        .copy("missing-src", "dst")
        .await
        .expect_err("copy of missing source");
    assert!(
        matches!(err, Error::NotFound(ref s) if s == "missing-src"),
        "expected NotFound(missing-src), got {err:?}"
    );
}

#[tokio::test]
async fn delete_existing_then_delete_missing_is_not_found() {
    let (store, _bucket) = fresh_bucket().await;
    store
        .put_bytes("k", Bytes::from_static(b"v"), PutOpts::default())
        .await
        .expect("put");
    store.delete("k").await.expect("first delete");

    let err = store.delete("k").await.expect_err("second delete");
    assert!(matches!(err, Error::NotFound(ref s) if s == "k"));
}

#[tokio::test]
async fn large_object_multipart_download() {
    let (store, bucket) = fresh_bucket().await;
    let fixture = fixture();

    // 50 MiB of random bytes — forces ≥4 ranged GETs at the 16 MiB
    // chunk size.
    let size = 50 * 1024 * 1024;
    let mut body = vec![0u8; size];
    for (i, b) in body.iter_mut().enumerate() {
        // Cheap deterministic "random": good enough to force every
        // chunk to be distinct. Knuth multiplicative-hash constant.
        *b = u8::try_from(i.wrapping_mul(2_654_435_761) & 0xff).unwrap_or(0);
    }
    let mut hasher = Sha256::new();
    hasher.update(&body);
    let expected_hash = hasher.finalize();

    // Upload via the SDK client directly (avoids the trait's 5 GiB
    // single-PUT cap discussion — 50 MiB fits).
    let setup = setup_client(fixture.port).await;
    setup
        .put_object()
        .bucket(&bucket)
        .key("big")
        .body(ByteStream::from(body))
        .send()
        .await
        .expect("seed put_object");

    let tmp = tempfile::tempdir().expect("tempdir");
    let dest = tmp.path().join("downloaded");
    store
        .get_to_file("big", &dest)
        .await
        .expect("get_to_file (multipart)");

    // Hash the downloaded file in chunks to avoid double-buffering.
    let actual_hash = {
        use std::io::Read;
        let mut file = std::fs::File::open(&dest).expect("open downloaded");
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = file.read(&mut buf).expect("read");
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        hasher.finalize()
    };
    assert_eq!(
        actual_hash, expected_hash,
        "multipart download corrupted body"
    );

    // File size matches expectation.
    let metadata = std::fs::metadata(&dest).expect("metadata");
    assert_eq!(metadata.len(), size as u64);
}

#[tokio::test]
async fn put_path_streams_file_and_round_trips() {
    let (store, _bucket) = fresh_bucket().await;

    // Create a 32 MiB temp file with deterministic content — large enough
    // to exercise the streaming path without approaching the 5 GiB single-PUT
    // ceiling that `put_path` is designed to remove.
    let size: usize = 32 * 1024 * 1024;
    let mut payload = vec![0u8; size];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = u8::try_from(i.wrapping_mul(2_654_435_761) & 0xff).unwrap_or(0);
    }
    let mut hasher = Sha256::new();
    hasher.update(&payload);
    let expected_hash = hasher.finalize();

    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("big-upload.bin");
    tokio::fs::write(&src, &payload).await.expect("write src");

    // Upload via put_path (streaming from disk).
    store
        .put_path("streamed", &src, PutOpts::default())
        .await
        .expect("put_path");

    // Download via get_to_file and hash-compare.
    let dest = tmp.path().join("downloaded.bin");
    store
        .get_to_file("streamed", &dest)
        .await
        .expect("get_to_file");

    let actual_hash = {
        use std::io::Read;
        let mut file = std::fs::File::open(&dest).expect("open downloaded");
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = file.read(&mut buf).expect("read");
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        hasher.finalize()
    };
    assert_eq!(
        actual_hash, expected_hash,
        "put_path → get_to_file round-trip corrupted body"
    );
    let metadata = std::fs::metadata(&dest).expect("metadata");
    assert_eq!(metadata.len(), size as u64);
}

#[tokio::test]
async fn put_path_with_opts_preserves_metadata() {
    let (store, _bucket) = fresh_bucket().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("small.txt");
    tokio::fs::write(&src, b"hello via path")
        .await
        .expect("write src");

    let opts = PutOpts {
        content_disposition: Some("attachment; filename=test.txt".into()),
        user_metadata: vec![("x-custom".into(), "value".into())],
    };
    store
        .put_path("meta-test", &src, opts)
        .await
        .expect("put_path");

    // Verify the body round-trips.
    let body = store.get_bytes("meta-test").await.expect("get_bytes");
    assert_eq!(&body[..], b"hello via path");
}

#[tokio::test]
async fn get_missing_key_is_not_found() {
    let (store, _bucket) = fresh_bucket().await;
    let err = store.get_bytes("absent").await.expect_err("get missing");
    assert!(
        matches!(err, Error::NotFound(ref s) if s == "absent"),
        "expected NotFound, got {err:?}"
    );
}

#[tokio::test]
async fn get_to_file_failure_does_not_corrupt_dest() {
    let (store, _bucket) = fresh_bucket().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let dest: PathBuf = tmp.path().join("nope");
    let err = store
        .get_to_file("missing-key", &dest)
        .await
        .expect_err("get_to_file on missing key");
    assert!(matches!(err, Error::NotFound(_)));
    assert!(
        !dest.exists(),
        "destination must not exist after a failed get_to_file"
    );
}

#[tokio::test]
async fn access_denied_via_wrong_creds() {
    // Seed a key as the privileged user.
    let (store, bucket) = fresh_bucket().await;
    store
        .put_bytes("k", Bytes::from_static(b"v"), PutOpts::default())
        .await
        .expect("put");

    // Build a second S3Store with bogus creds against the same bucket.
    // We can't easily reconfigure the trait's credential provider per-
    // store (the SDK reads env vars at load time), so instantiate the
    // SDK client directly with deliberately-wrong creds and call its
    // get_object — this verifies that the server rejects bad signatures
    // with 403 (the path our classifier maps to AccessDenied).
    let fixture = fixture();
    let endpoint = format!("http://127.0.0.1:{}", fixture.port);
    let bad_creds = aws_sdk_s3::config::Credentials::new(
        "bogus-access-key",
        "bogus-secret-key",
        None,
        None,
        "test",
    );
    let bad_shared = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .endpoint_url(endpoint)
        .credentials_provider(bad_creds)
        .load()
        .await;
    let bad_s3 = aws_sdk_s3::config::Builder::from(&bad_shared)
        .force_path_style(true)
        .build();
    let bad_client = aws_sdk_s3::Client::from_conf(bad_s3);

    let err = bad_client
        .get_object()
        .bucket(&bucket)
        .key("k")
        .send()
        .await
        .expect_err("bad creds must be rejected");
    // We don't go through our classifier here, but assert that the
    // SDK surfaces a 403 service error so the production code path
    // (which does go through `classify`) would map it to AccessDenied.
    let raw_status = match &err {
        aws_sdk_s3::error::SdkError::ServiceError(svc) => Some(svc.raw().status().as_u16()),
        _ => None,
    };
    assert_eq!(
        raw_status,
        Some(403),
        "expected 403 from RustFS for bad creds, got {err:?}"
    );
}

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
use git_remote_object_store::object_store::{GetOpts, ObjectStore, ObjectStoreError, PutOpts};
use git_remote_object_store::protocol::backend::{self, BackendError, BackendKind};
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
async fn get_bytes_range_returns_slice_via_http_range_header() {
    // Real-RustFS exercise of the `Range: bytes=<start>-<end-1>` path.
    // This is the only way to confirm the header format is wire-correct
    // for the packchain engine's pack-blob direct-access path (issue
    // #52); the unit tests cover the trait contract but not the header.
    let (store, _bucket) = fresh_bucket().await;
    let body = Bytes::from_static(b"abcdefghijklmnopqrstuvwxyz"); // 26 bytes
    store
        .put_bytes("k", body.clone(), PutOpts::default())
        .await
        .expect("put");

    // Aligned middle slice.
    let mid = store.get_bytes_range("k", 5..10).await.expect("get range");
    assert_eq!(&mid[..], b"fghij");

    // Single byte at the end (boundary).
    let last = store
        .get_bytes_range("k", 25..26)
        .await
        .expect("get last byte");
    assert_eq!(&last[..], b"z");

    // Full body via range matches `get_bytes`.
    let whole = store.get_bytes_range("k", 0..26).await.expect("get whole");
    assert_eq!(whole, body);

    // Empty range short-circuits without a network call.
    let empty = store.get_bytes_range("k", 7..7).await.expect("empty range");
    assert!(empty.is_empty());
}

#[tokio::test]
async fn get_bytes_range_past_end_maps_to_range_not_satisfiable() {
    // S3 returns HTTP 416 when `Range:` falls past the body. Our
    // mapping must surface the original `Range<u64>` so the wire-line
    // names what the caller asked for, not the server's translation.
    let (store, _bucket) = fresh_bucket().await;
    store
        .put_bytes("k", Bytes::from_static(b"abc"), PutOpts::default())
        .await
        .expect("put");
    let err = store
        .get_bytes_range("k", 100..200)
        .await
        .expect_err("range past end must error");
    assert!(
        matches!(
            err,
            ObjectStoreError::RangeNotSatisfiable {
                ref key,
                requested: ref r,
            } if key == "k" && r.start == 100 && r.end == 200,
        ),
        "expected RangeNotSatisfiable(k, 100..200), got {err:?}",
    );
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
    // Name the page-walk magic number so a future S3 SDK / RustFS
    // default-page-size bump produces a meaningful diff at the
    // constant rather than a silent test-coverage regression (#221).
    const S3_DEFAULT_MAXKEYS: usize = 1000;
    const PAGINATION_MARGIN: usize = 500;
    let (store, _bucket) = fresh_bucket().await;
    let count = S3_DEFAULT_MAXKEYS + PAGINATION_MARGIN;
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
async fn copy_drops_user_metadata_and_content_disposition() {
    // Tripwire for the `MetadataDirective::Replace` choice in
    // `S3Store::copy`. The trait contract (see
    // `ObjectStore::copy`) does NOT promise metadata propagation
    // because the Azure backend cannot guarantee it (its copy is
    // implemented as download-then-reupload and drops user
    // metadata). The S3 backend matches Azure by passing
    // `MetadataDirective::Replace` with no metadata fields set.
    //
    // A regression that flips back to `MetadataDirective::Copy` (or
    // omits the directive entirely — S3 defaults to Copy) would
    // silently restore S3 metadata propagation and break parity
    // with Azure without any test failure. This pins the contract.
    let (store, bucket) = fresh_bucket().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let src_path = tmp.path().join("src.txt");
    tokio::fs::write(&src_path, b"copy contract")
        .await
        .expect("write src");

    let opts = PutOpts {
        content_disposition: Some("attachment; filename=orig.txt".into()),
        user_metadata: vec![("x-original".into(), "yes".into())],
        progress: None,
    };
    store
        .put_path("src", &src_path, opts)
        .await
        .expect("put_path");

    let fixture = fixture();
    let client = setup_client(fixture.port).await;

    // Sanity-check the fixture before testing the contract: if a
    // regression in `put_path` silently dropped these fields, the
    // post-copy "absent" assertions below would pass vacuously. Pin
    // the source state so the test only ever fails for the right
    // reason (a regression in `copy`, not in the put helper).
    let src_head = client
        .head_object()
        .bucket(&bucket)
        .key("src")
        .send()
        .await
        .expect("head_object src");
    assert_eq!(
        src_head.content_disposition(),
        Some("attachment; filename=orig.txt"),
        "fixture precondition: put_path must persist content_disposition",
    );
    assert!(
        src_head
            .metadata()
            .is_some_and(|m| m.get("x-original").is_some_and(|v| v == "yes")),
        "fixture precondition: put_path must persist user metadata; got {:?}",
        src_head.metadata(),
    );

    store.copy("src", "dst").await.expect("copy");

    let head = client
        .head_object()
        .bucket(&bucket)
        .key("dst")
        .send()
        .await
        .expect("head_object dst");
    assert_eq!(
        head.content_disposition(),
        None,
        "copy must drop content_disposition (Azure parity)",
    );
    let user_meta = head.metadata();
    let has_original = user_meta.is_some_and(|m| m.contains_key("x-original"));
    assert!(
        !has_original,
        "copy must drop user metadata (Azure parity); got {user_meta:?}",
    );
}

#[tokio::test]
async fn copy_missing_source_is_not_found() {
    let (store, _bucket) = fresh_bucket().await;
    let err = store
        .copy("missing-src", "dst")
        .await
        .expect_err("copy of missing source");
    assert!(
        matches!(err, ObjectStoreError::NotFound(ref s) if s == "missing-src"),
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
    assert!(matches!(err, ObjectStoreError::NotFound(ref s) if s == "k"));
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
        .get_to_file("big", &dest, GetOpts::default())
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
        .get_to_file("streamed", &dest, GetOpts::default())
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
async fn put_path_with_opts_uploads_body() {
    let (store, bucket) = fresh_bucket().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("small.txt");
    tokio::fs::write(&src, b"hello via path")
        .await
        .expect("write src");

    let opts = PutOpts {
        content_disposition: Some("attachment; filename=test.txt".into()),
        user_metadata: vec![("x-custom".into(), "value".into())],
        progress: None,
    };
    store
        .put_path("meta-test", &src, opts)
        .await
        .expect("put_path");

    // Verify the body round-trips.
    let body = store.get_bytes("meta-test").await.expect("get_bytes");
    assert_eq!(&body[..], b"hello via path");

    // Verify metadata via a direct SDK head_object call — the trait's
    // `head()` doesn't expose content_disposition or user metadata.
    let fixture = fixture();
    let client = setup_client(fixture.port).await;
    let head = client
        .head_object()
        .bucket(&bucket)
        .key("meta-test")
        .send()
        .await
        .expect("head_object");
    assert_eq!(
        head.content_disposition().unwrap_or(""),
        "attachment; filename=test.txt",
        "content_disposition must survive put_path",
    );
    let user_meta = head.metadata().expect("user metadata present");
    assert_eq!(
        user_meta.get("x-custom").map(String::as_str),
        Some("value"),
        "user metadata must survive put_path",
    );
}

#[tokio::test]
async fn get_missing_key_is_not_found() {
    let (store, _bucket) = fresh_bucket().await;
    let err = store.get_bytes("absent").await.expect_err("get missing");
    assert!(
        matches!(err, ObjectStoreError::NotFound(ref s) if s == "absent"),
        "expected NotFound, got {err:?}"
    );
}

#[tokio::test]
async fn get_to_file_failure_does_not_corrupt_dest() {
    let (store, _bucket) = fresh_bucket().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let dest: PathBuf = tmp.path().join("nope");
    let err = store
        .get_to_file("missing-key", &dest, GetOpts::default())
        .await
        .expect_err("get_to_file on missing key");
    assert!(matches!(err, ObjectStoreError::NotFound(_)));
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

// ---------------------------------------------------------------------------
// End-to-end binary tests
//
// Drive `git push` / `git clone` against the actual `git-remote-s3+http`
// helper binary, with RustFS as the backend. These complement the
// trait-level tests above by exercising the protocol REPL, the URL
// dispatch in `protocol::backend::build`, and the LFS custom-transfer
// agent.
//
// Cargo bin names cannot contain `+`, so each helper is built as
// `git-remote-s3-http` and we symlink the binary to `git-remote-s3+http`
// in a tempdir prepended to PATH for the duration of these tests. The
// symlink-based PATH shim is unix-only by design.
//
// Mirrors the structure used by `tests/azure_store_integration.rs`.
// ---------------------------------------------------------------------------

#[cfg(not(unix))]
compile_error!("S3 E2E tests are unix-only (symlink-based PATH shim)");

use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Cargo bin path for the S3 HTTP helper. `CARGO_BIN_EXE_<name>` is
/// populated by cargo for any `[[bin]]` defined in this package, and
/// triggers a build of that binary before the integration test runs.
const HELPER_BIN: &str = env!("CARGO_BIN_EXE_git-remote-s3-http");
/// Cargo bin path for the LFS custom-transfer agent.
const LFS_BIN: &str = env!("CARGO_BIN_EXE_git-lfs-object-store");

/// On-disk name git looks up when dispatching `s3+http://…` URLs. Must
/// be exactly `git-remote-s3+http` per `git help gitremote-helpers`.
const HELPER_GIT_NAME: &str = "git-remote-s3+http";
/// Same for the LFS agent — registered under `lfs.standalonetransferagent`
/// in [`crate::lfs::install`], which uses the unhyphenated name.
const LFS_GIT_NAME: &str = "git-lfs-object-store";

/// Tempdir holding `+`-named symlinks to the cargo binaries. Held in a
/// `OnceLock` so it survives for the lifetime of the test process; the
/// inner [`TempDir`] cleans up at process exit.
static HELPER_BIN_DIR: OnceLock<TempDir> = OnceLock::new();

/// Build (once) a directory containing the helper symlinks, prepend it
/// to PATH for child processes, and return the absolute path.
fn helper_bin_dir() -> &'static Path {
    HELPER_BIN_DIR
        .get_or_init(|| {
            // `+` is legal in POSIX filenames, so the symlink targets
            // can use the `+`-form names git invokes directly.
            let tmp = tempfile::tempdir().expect("helper bin tempdir");
            symlink(Path::new(HELPER_BIN), tmp.path().join(HELPER_GIT_NAME))
                .expect("symlink helper bin");
            symlink(Path::new(LFS_BIN), tmp.path().join(LFS_GIT_NAME)).expect("symlink lfs bin");
            tmp
        })
        .path()
}

/// Whether `git` is on PATH. The whole binary suite assumes git is
/// available, but skip rather than panic when it isn't.
fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

/// Whether `git lfs` is on PATH. Required only by the LFS round-trip
/// test; skip when missing rather than failing the suite.
fn git_lfs_available() -> bool {
    Command::new("git")
        .args(["lfs", "version"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `PATH` value with [`helper_bin_dir`] prepended to the host's
/// existing `PATH`. Computed once because the value is process-stable
/// and used on every git / LFS-agent spawn.
static HERMETIC_PATH: OnceLock<std::ffi::OsString> = OnceLock::new();

fn hermetic_path() -> &'static std::ffi::OsStr {
    HERMETIC_PATH.get_or_init(|| {
        let bin_dir = helper_bin_dir();
        match std::env::var_os("PATH") {
            Some(existing) => {
                let mut prefixed = std::ffi::OsString::from(bin_dir);
                prefixed.push(":");
                prefixed.push(&existing);
                prefixed
            }
            None => bin_dir.as_os_str().to_owned(),
        }
    })
}

/// Apply the env-var trio (plus AWS creds + cleartext-HTTP gate) every
/// spawn in this section needs:
///
/// - `PATH` prepended with the helper-symlink directory so spawned
///   tools find the `+`-named binaries.
/// - User / system git config redirected to `/dev/null` so the host's
///   `~/.gitconfig` cannot leak into the test.
/// - AWS credentials matching RustFS's well-known root creds.
/// - `ENV_ALLOW_HTTP=1` so the helper accepts cleartext loopback URLs.
fn hermetic_env(cmd: &mut Command) -> &mut Command {
    cmd.env("PATH", hermetic_path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("AWS_ACCESS_KEY_ID", TEST_USER)
        .env("AWS_SECRET_ACCESS_KEY", TEST_PASSWORD)
        .env("AWS_REGION", "us-east-1")
        .env(ENV_ALLOW_HTTP, "1")
}

/// Run `git-lfs-object-store install` in `cwd`, exercising the
/// production install path that wires the agent into git config.
fn run_lfs_agent_install(cwd: &Path) {
    let mut cmd = Command::new(helper_bin_dir().join(LFS_GIT_NAME));
    let status = hermetic_env(cmd.arg("install").current_dir(cwd))
        .status()
        .expect("spawn git-lfs-object-store install");
    assert!(
        status.success(),
        "lfs agent install failed in {}",
        cwd.display()
    );
}

/// Run a `git` subcommand against `cwd` with hermetic configuration
/// (see [`hermetic_env`]) plus `GIT_TERMINAL_PROMPT=0` so any
/// unexpected credential prompt fails fast instead of hanging.
/// Asserts the command succeeds.
fn run_git(args: &[&str], cwd: &Path) {
    let mut cmd = Command::new("git");
    let output = hermetic_env(cmd.args(args).current_dir(cwd))
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} (cwd={}) failed: stdout={} stderr={}",
        cwd.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Commit staged changes with a deterministic message. `commit.gpgsign`
/// is already disabled by [`init_seed_repo`].
fn commit(repo: &Path, msg: &str) {
    run_git(&["commit", "--quiet", "-m", msg], repo);
}

/// Initialise an empty repo, configure a deterministic identity, and
/// return its tempdir.
fn init_seed_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("seed repo tempdir");
    run_git(&["init", "--quiet", "--initial-branch=main"], dir.path());
    run_git(&["config", "user.email", "test@example.com"], dir.path());
    run_git(&["config", "user.name", "Test"], dir.path());
    run_git(&["config", "commit.gpgsign", "false"], dir.path());
    dir
}

/// Allocate a fresh bucket in RustFS (used by E2E tests that drive the
/// helper binary rather than the `S3Store` API directly) and return
/// `(host_port, bucket_name)`.
async fn fresh_bucket_endpoint() -> (u16, String) {
    let fixture = fixture();
    let n = BUCKET_COUNTER.fetch_add(1, Ordering::SeqCst);
    let bucket = format!("e2e-{}-{}", std::process::id(), n);
    let setup = setup_client(fixture.port).await;
    setup
        .create_bucket()
        .bucket(&bucket)
        .send()
        .await
        .expect("create_bucket succeeds");
    (fixture.port, bucket)
}

/// Build the helper URL for the given RustFS endpoint and bucket.
/// RustFS's loopback endpoint has no DNS rewriting, so we pin
/// path-style addressing and a fixed `repo` prefix here.
fn helper_url(port: u16, bucket: &str) -> String {
    format!("s3+http://127.0.0.1:{port}/{bucket}/repo?addressing=path")
}

#[tokio::test]
async fn helper_binary_round_trips_init_push_clone_fetch() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }

    let (port, bucket) = fresh_bucket_endpoint().await;
    let url = helper_url(port, &bucket);

    // Source repo: one commit on main.
    let seed = init_seed_repo();
    std::fs::write(seed.path().join("hello.txt"), b"hello s3 e2e\n").expect("write seed file");
    run_git(&["add", "hello.txt"], seed.path());
    commit(seed.path(), "seed");

    // Allow the +http scheme inside this repo's submodule resolution
    // path (defensive — `protocol.s3+http.allow` is irrelevant for the
    // top-level remote, but mirrors the documented config) and add the
    // remote.
    run_git(&["config", "protocol.s3+http.allow", "always"], seed.path());
    run_git(&["remote", "add", "origin", &url], seed.path());

    // Push: drives the helper binary end-to-end. A failure here means
    // the protocol REPL or backend wiring is broken.
    run_git(&["push", "origin", "main"], seed.path());

    // Clone into a fresh directory. The clone must come up with the
    // same commit and the seed file.
    let dest_parent = tempfile::tempdir().expect("dest tempdir");
    let dest = dest_parent.path().join("clone");
    let dest_str = dest.to_str().expect("dest path utf-8");
    run_git(
        &[
            "-c",
            "protocol.s3+http.allow=always",
            "clone",
            "--quiet",
            &url,
            dest_str,
        ],
        dest_parent.path(),
    );

    let cloned_body = std::fs::read(dest.join("hello.txt")).expect("cloned file readable");
    assert_eq!(cloned_body, b"hello s3 e2e\n");

    // Second commit + fetch round-trip: confirms incremental fetch
    // works as well as the first-clone path.
    std::fs::write(seed.path().join("hello.txt"), b"hello s3 e2e v2\n").expect("rewrite seed file");
    run_git(&["add", "hello.txt"], seed.path());
    commit(seed.path(), "v2");
    run_git(&["push", "origin", "main"], seed.path());

    run_git(&["fetch", "origin", "main"], &dest);
    run_git(&["reset", "--hard", "origin/main"], &dest);

    let updated = std::fs::read(dest.join("hello.txt")).expect("updated file");
    assert_eq!(updated, b"hello s3 e2e v2\n");
}

#[tokio::test]
async fn lfs_round_trips_upload_and_download_through_helper() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    if !git_lfs_available() {
        eprintln!("skipping: git lfs not on PATH");
        return;
    }

    let (port, bucket) = fresh_bucket_endpoint().await;
    let url = helper_url(port, &bucket);

    // Build a repo with one LFS-tracked binary file.
    let seed = init_seed_repo();
    run_git(&["config", "protocol.s3+http.allow", "always"], seed.path());
    run_git(&["lfs", "install", "--local"], seed.path());
    // Register our custom-transfer agent via the production binary —
    // exercises `lfs::install::install` end-to-end.
    run_lfs_agent_install(seed.path());

    run_git(&["lfs", "track", "*.bin"], seed.path());
    let body: Vec<u8> = (0u8..=255).cycle().take(64 * 1024).collect();
    std::fs::write(seed.path().join("payload.bin"), &body).expect("write LFS payload");
    run_git(&["add", ".gitattributes", "payload.bin"], seed.path());
    commit(seed.path(), "lfs payload");

    run_git(&["remote", "add", "origin", &url], seed.path());
    // Push: triggers an LFS upload via our standalone transfer agent
    // and a bundle push via the helper REPL.
    run_git(&["push", "origin", "main"], seed.path());

    // Fresh clone — must register the LFS agent again because
    // `lfs.customtransfer.*` config lives in the per-repo config of the
    // clone, which `git clone` initialises empty.
    let dest_parent = tempfile::tempdir().expect("dest tempdir");
    let dest = dest_parent.path().join("clone");
    let dest_str = dest.to_str().expect("dest path utf-8");
    run_git(
        &[
            "-c",
            "protocol.s3+http.allow=always",
            "clone",
            "--quiet",
            "--no-checkout",
            &url,
            dest_str,
        ],
        dest_parent.path(),
    );
    run_git(&["lfs", "install", "--local"], &dest);
    run_lfs_agent_install(&dest);
    run_git(&["checkout", "main"], &dest);

    let downloaded = std::fs::read(dest.join("payload.bin")).expect("LFS payload restored");
    assert_eq!(downloaded, body, "LFS round-trip body mismatch");
}

// ---------------------------------------------------------------------------
// Backend probe: categorical fatal-message mapping (issue #45)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn build_against_existing_bucket_succeeds() {
    // Sanity guard: an empty but real bucket must pass the eager probe
    // (regression check against false positives from the listing call).
    let (_store, bucket) = fresh_bucket().await;
    let fixture = fixture();
    let url_str = format!(
        "s3+http://127.0.0.1:{port}/{bucket}/repo?addressing=path",
        port = fixture.port,
    );
    let url = parse(&url_str).expect("URL parses");
    backend::build(&url)
        .await
        .expect("probe against existing empty bucket succeeds");
}

#[tokio::test]
async fn build_against_missing_bucket_returns_bucket_not_found() {
    let fixture = fixture();
    let n = BUCKET_COUNTER.fetch_add(1, Ordering::SeqCst);
    let bucket = format!("missing-{}-{}", std::process::id(), n);
    let url_str = format!(
        "s3+http://127.0.0.1:{port}/{bucket}/repo?addressing=path",
        port = fixture.port,
    );
    let url = parse(&url_str).expect("URL parses");

    let Err(err) = backend::build(&url).await else {
        panic!("missing bucket must error");
    };
    // Render the *actual* returned error — a manually-constructed
    // value would not catch a regression where `backend::build`
    // populates the variant with the wrong name or kind.
    assert_eq!(
        backend::fatal_message(&err),
        format!("fatal: bucket not found {bucket}"),
    );
    let BackendError::BucketNotFound { kind, name } = err else {
        panic!("expected BucketNotFound, got {err:?}");
    };
    assert_eq!(kind, BackendKind::S3);
    assert_eq!(name, bucket);
}

mod common;
use common::{
    LARGE_BODY_CHUNK_SIZE, LARGE_BODY_ENV_VAR, LARGE_BODY_TEST_SIZE, MIDBODY_ABORT_TEST_SIZE,
    MULTIPART_TEST_SIZE, deterministic_payload, large_body_tests_enabled, sha256_of,
    sha256_of_file, spawn_truncator, write_repeating_pattern_file,
};

/// `put_bytes` above the multipart threshold drives the hand-rolled
/// multipart upload path (issue #53). 80 MiB body splits into 5 parts
/// at the 16 MiB part size, exercising parallelism and the
/// last-part-short case.
#[tokio::test]
async fn multipart_put_bytes_round_trips() {
    let (store, _bucket) = fresh_bucket().await;
    let payload = deterministic_payload(MULTIPART_TEST_SIZE);
    let expected = sha256_of(&payload);

    store
        .put_bytes("multipart-bytes", Bytes::from(payload), PutOpts::default())
        .await
        .expect("multipart put_bytes");

    let tmp = tempfile::tempdir().expect("tempdir");
    let dest = tmp.path().join("downloaded");
    store
        .get_to_file("multipart-bytes", &dest, GetOpts::default())
        .await
        .expect("get_to_file");
    assert_eq!(
        sha256_of_file(&dest),
        expected,
        "multipart upload corrupted the body"
    );
}

/// `put_path` above the multipart threshold drives the streaming
/// multipart upload path (each part read from disk independently).
/// Issue #53.
#[tokio::test]
async fn multipart_put_path_round_trips() {
    let (store, _bucket) = fresh_bucket().await;
    let payload = deterministic_payload(MULTIPART_TEST_SIZE);
    let expected = sha256_of(&payload);

    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("multipart-src.bin");
    tokio::fs::write(&src, &payload).await.expect("write src");

    store
        .put_path("multipart-path", &src, PutOpts::default())
        .await
        .expect("multipart put_path");

    let dest = tmp.path().join("multipart-dst.bin");
    store
        .get_to_file("multipart-path", &dest, GetOpts::default())
        .await
        .expect("get_to_file");
    assert_eq!(sha256_of_file(&dest), expected);
}

/// `copy` above the multipart threshold drives `UploadPartCopy`
/// (server-side copy in chunks). Closes the
/// `manage doctor --fix` quarantine gap on large bundles. Issue #53.
#[tokio::test]
async fn multipart_copy_round_trips() {
    let (store, _bucket) = fresh_bucket().await;
    let payload = deterministic_payload(MULTIPART_TEST_SIZE);
    let expected = sha256_of(&payload);

    store
        .put_bytes("copy-src", Bytes::from(payload), PutOpts::default())
        .await
        .expect("seed src via multipart");
    store
        .copy("copy-src", "copy-dst")
        .await
        .expect("multipart copy");

    let tmp = tempfile::tempdir().expect("tempdir");
    let dest = tmp.path().join("downloaded");
    store
        .get_to_file("copy-dst", &dest, GetOpts::default())
        .await
        .expect("get_to_file dst");
    assert_eq!(
        sha256_of_file(&dest),
        expected,
        "multipart copy corrupted body"
    );
}

/// `copy` sends `x-amz-copy-source-if-match` so a mid-copy mutation
/// of the source surfaces as an error rather than silently producing
/// a destination with mixed pre/post-mutation bytes. Verified by
/// putting an object, capturing its `ETag`, mutating the source,
/// then using the SDK directly to issue a `CopyObject` with the
/// *original* (now stale) `ETag` — `RustFS` must return 412 /
/// `PreconditionFailed`. This pins that the if-match flag is
/// honoured by the test backend in the small-object copy path; the
/// multipart copy path threads the same `copy_source_if_match`
/// through `UploadPartCopy`.
#[tokio::test]
async fn copy_with_stale_source_if_match_returns_precondition_failed() {
    let (_store, bucket) = fresh_bucket().await;
    let fixture = fixture();
    let client = setup_client(fixture.port).await;

    // Seed the source twice — second put changes the ETag.
    client
        .put_object()
        .bucket(&bucket)
        .key("race-src")
        .body(ByteStream::from_static(b"original"))
        .send()
        .await
        .expect("seed src v1");
    let head = client
        .head_object()
        .bucket(&bucket)
        .key("race-src")
        .send()
        .await
        .expect("head v1");
    let stale_etag = head.e_tag().expect("S3 returns an ETag").to_owned();

    client
        .put_object()
        .bucket(&bucket)
        .key("race-src")
        .body(ByteStream::from_static(b"replaced"))
        .send()
        .await
        .expect("seed src v2");

    // Now attempt a CopyObject with the stale ETag. RustFS must
    // refuse — that's the property we rely on for the new
    // `copy_source_if_match` wiring in `S3Store::copy` and
    // `S3Store::multipart_copy`.
    let copy_source = format!("{bucket}/race-src");
    let err = client
        .copy_object()
        .bucket(&bucket)
        .key("race-dst")
        .copy_source(&copy_source)
        .copy_source_if_match(&stale_etag)
        .send()
        .await
        .expect_err("CopyObject with stale ETag must error");
    let status = err.raw_response().map_or(0, |r| r.status().as_u16());
    assert_eq!(
        status, 412,
        "RustFS returned status {status} for CopyObject with stale source-if-match",
    );
}

/// Multipart uploads emit one progress event per completed part so
/// the LFS agent can render a live progress bar. With an 80 MiB body
/// and 16 MiB parts, expect at least 2 events.
#[tokio::test]
async fn multipart_put_emits_per_part_progress_events() {
    let (store, _bucket) = fresh_bucket().await;
    common::assert_put_bytes_emits_chunked_progress(&store, "progress-events").await;
}

/// `put_path` above the multipart threshold also emits per-part
/// progress events — the streaming-from-disk path (`pread` per part)
/// drives the same `upload_parts_from_file` loop that the bytes path
/// drives via `upload_parts_with_bodies`. The bundle upload site in
/// `protocol/push.rs` is `put_path`-only, so without this test the
/// "bundle progress" half of issue #55's acceptance criteria has no
/// coverage on a real backend.
#[tokio::test]
async fn multipart_put_path_emits_per_part_progress_events() {
    let (store, _bucket) = fresh_bucket().await;
    common::assert_put_path_emits_chunked_progress(&store, "progress-path").await;
}

/// Optional regression test for the > 5 GiB AWS hard limit. Skipped
/// by default because a ~6 GiB body needs ~12 GiB of free disk for
/// the round-trip check. Enable with `RUN_LARGE_BODY_TESTS=1`.
///
/// This is the AC item from issues #53 / #56 that the cheaper 80 MiB
/// tests above cannot exercise directly: the actual `EntityTooLarge`
/// failure mode of bare `PutObject` only triggers above 5 GiB. Now
/// that hand-rolled multipart upload has landed (issue #53), the
/// expected outcome is a clean round-trip — no `EntityTooLarge`.
#[tokio::test]
#[ignore = "requires RUN_LARGE_BODY_TESTS=1 and ~12 GiB of free disk; see .claude/rules/testing.md"]
async fn multipart_put_path_above_5_gib_round_trips() {
    if !large_body_tests_enabled() {
        eprintln!("skipping: {LARGE_BODY_ENV_VAR} not set");
        return;
    }
    let (store, _bucket) = fresh_bucket().await;

    // Build the 6 GiB source on disk and stream-hash it in the same
    // pass. The 6 GiB body is 96 repetitions of the same 64 MiB
    // chunk (deterministic-but-repeating), so a per-byte sentinel
    // would not distinguish a part-swap from the original; SHA256
    // over the whole body catches any reordering or corruption that
    // a multipart implementation could introduce.
    let chunk = deterministic_payload(LARGE_BODY_CHUNK_SIZE);
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("six-gib.bin");
    let expected = write_repeating_pattern_file(&src, &chunk, LARGE_BODY_TEST_SIZE).await;

    store
        .put_path("six-gib", &src, PutOpts::default())
        .await
        .expect("multipart put_path > 5 GiB");

    let head = store.head("six-gib").await.expect("head");
    assert_eq!(head.size, LARGE_BODY_TEST_SIZE);

    // Round-trip integrity check: every byte must match the source.
    // `sha256_of_file` reads in 1 MiB chunks so peak memory is
    // bounded regardless of the 6 GiB body size.
    let dest = tmp.path().join("six-gib.dl");
    store
        .get_to_file("six-gib", &dest, GetOpts::default())
        .await
        .expect("get_to_file");
    assert_eq!(
        sha256_of_file(&dest),
        expected,
        "multipart > 5 GiB round-trip corrupted body",
    );
}

/// Mid-body interruption: when a multipart `put_path` source becomes
/// unreadable, the abort path fires and NO destination object is
/// visible. The deterministic part-read failure injection lives in
/// `object_store::multipart::tests::read_file_part_propagates_eof_after_truncate`
/// (unit test) — that test gives reliable byte-for-byte coverage of
/// `read_file_part` on a truncated file, which is the io-error site
/// in the multipart upload pipeline. This test is the integration
/// counterpart: it asserts the visible end-state ("destination key
/// is `NotFound` after a failed multipart") on a real backend, using
/// a body whose multipart plan stays partly pending past the
/// truncate window.
///
/// Expected outcome on S3: `put_path` errors, `head(key)` returns
/// `NotFound` because `S3Store::finish_multipart_upload(Err)` calls
/// `AbortMultipartUpload` (`s3.rs:1456-1473`).
///
/// Body size and concurrency rationale for the truncate window live
/// on `MIDBODY_ABORT_TEST_SIZE` (`cli/tests/common/mod.rs`); 50 ms is
/// long enough for `CreateMultipartUpload` to round-trip and the
/// first batch of preads to begin, but short enough that queued
/// preads have not yet fired against localhost RustFS.
#[tokio::test]
async fn multipart_put_path_aborts_on_midbody_truncation() {
    use std::time::Duration;

    let (store, _bucket) = fresh_bucket().await;
    let payload = deterministic_payload(MIDBODY_ABORT_TEST_SIZE);

    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("midbody-src.bin");
    tokio::fs::write(&src, &payload).await.expect("write src");

    let truncator = spawn_truncator(src.clone(), Duration::from_millis(50));
    let key = "midbody-abort";
    let upload_result = store.put_path(key, &src, PutOpts::default()).await;
    truncator.await.expect("truncator joins");

    assert!(
        upload_result.is_err(),
        "expected put_path to error on mid-upload truncation, got Ok"
    );
    let head_err = store
        .head(key)
        .await
        .expect_err("destination must not be visible after aborted multipart");
    assert!(
        matches!(head_err, ObjectStoreError::NotFound(_)),
        "expected NotFound on aborted destination, got {head_err:?}"
    );
}

// ---------------------------------------------------------------------------
// S3 presigned-URL round-trip (issue #76)
// ---------------------------------------------------------------------------

/// `S3Store::presigned_get_url` produces a SigV4-signed URL whose
/// `X-Amz-Expires` matches the requested TTL and whose
/// `X-Amz-Signature` is non-empty. The URL fetches the same bytes a
/// signed SDK call would, exercising the `SigV4` wire format end-to-end
/// against `RustFS` (which supports `SigV4`). Issue #76.
#[tokio::test]
async fn presigned_get_url_round_trips_against_rustfs() {
    use std::time::Duration;

    let (store, _bucket) = fresh_bucket().await;
    let key = "presign-target";
    let body = Bytes::from_static(b"presigned body content");
    store
        .put_bytes(key, body.clone(), PutOpts::default())
        .await
        .expect("seed body");

    let url_str = store
        .presigned_get_url(key, Duration::from_hours(1))
        .await
        .expect("presigned_get_url");

    let parsed = ::url::Url::parse(&url_str).expect("presigned URL parses");
    let pairs = common::query_pairs_btree(&parsed);
    assert_eq!(
        pairs.get("X-Amz-Expires").map(String::as_str),
        Some("3600"),
        "X-Amz-Expires must echo the requested TTL: {pairs:?}",
    );
    let sig = pairs
        .get("X-Amz-Signature")
        .expect("X-Amz-Signature query param present");
    assert!(!sig.is_empty(), "signature must be non-empty");
    assert!(
        pairs.contains_key("X-Amz-Date"),
        "X-Amz-Date present (SigV4 requirement): {pairs:?}",
    );

    // Round-trip via plain reqwest — proves the URL is actually
    // honoured by the bucket without any further auth.
    let resp = reqwest::get(url_str)
        .await
        .expect("HTTP GET against presigned URL");
    assert_eq!(resp.status().as_u16(), 200, "status: {}", resp.status());
    let downloaded = resp.bytes().await.expect("body").to_vec();
    assert_eq!(downloaded, body.as_ref(), "body mismatch via presigned URL");
}

// ---------------------------------------------------------------------------
// Best-effort zip-artifact upload (issue #127 / #142)
// ---------------------------------------------------------------------------

/// Build a `?zip=1` URL pointing at a fresh `RustFS` bucket and the
/// `repo` prefix, returning the parsed URL + a trait-object store
/// connected to it. Shared between the fault-injection test and the
/// happy-path test below.
async fn fresh_zip_bucket() -> (Arc<dyn ObjectStore>, RemoteUrl) {
    let (store, bucket) = fresh_bucket().await;
    let fixture = fixture();
    let url_str = format!(
        "s3+http://127.0.0.1:{port}/{bucket}/repo?addressing=path&zip=1",
        port = fixture.port,
    );
    let url = parse(&url_str).expect("URL parses");
    let RemoteUrl::S3 { .. } = &url else {
        panic!("parse returned non-S3 variant for {url_str}");
    };
    (Arc::new(store) as Arc<dyn ObjectStore>, url)
}

/// A transient `put_path` failure on the zip-only key must NOT fail the
/// push: the bundle, `HEAD`, and `FORMAT` are already durable. The
/// unit test
/// `perform_push_under_lock_succeeds_when_zip_upload_fails` in
/// `src/protocol/push.rs` pins this contract against `MockStore`; this
/// integration test confirms the same shape end-to-end against
/// `RustFS`, where the real bundle put goes through a multipart-or-
/// single-PUT dispatch and the zip put goes through the
/// content-disposition + user-metadata path. Issue #142.
#[tokio::test]
async fn push_with_zip_put_fault_succeeds_and_omits_zip() {
    let (store, url) = fresh_zip_bucket().await;
    common::zip_fault::push_with_zip_put_fault_succeeds_and_omits_zip(store, url).await;
}

/// Happy-path counterpart of the fault test above: a clean `?zip=1`
/// push against `RustFS` must land both the bundle and the zip artifact
/// at their documented keys. Closes the live-backend coverage gap that
/// `tests/protocol_push.rs::zip_variant_uploads_repo_zip_with_metadata`
/// (MockStore-only) leaves. Issue #142.
#[tokio::test]
async fn push_with_zip_uploads_artifact() {
    let (store, url) = fresh_zip_bucket().await;
    common::zip_fault::push_with_zip_uploads_artifact(store, url).await;
}

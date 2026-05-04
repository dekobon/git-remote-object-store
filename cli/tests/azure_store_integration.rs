//! Integration tests for [`AzureStore`][a] against a real Azure
//! Blob–compatible server (the official Microsoft Azurite emulator
//! via `testcontainers`).
//!
//! Azurite (`mcr.microsoft.com/azure-storage/azurite`) plays the same
//! role for the Azure backend that RustFS plays for S3. The Docker
//! image tag is **pinned**: bump [`AZURITE_TAG`] deliberately when
//! upstream releases a new version and re-run the suite to confirm
//! parity.
//!
//! Gated on the `integration-azure` Cargo feature so contributors
//! without Docker are not blocked. CI runs this on Linux:
//!
//! ```text
//! cargo test --features integration-azure
//! ```
//!
//! The suite shares one Azurite container (started lazily via
//! [`OnceLock`]); each test allocates its own container with a unique
//! suffix so they parallel-test cleanly.
//!
//! [a]: git_remote_object_store::object_store::azure
//! [`OnceLock`]: std::sync::OnceLock

#![cfg(feature = "integration-azure")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use azure_core::http::Method;
use azure_core::http::headers::{HeaderName, Headers};
use bytes::Bytes;
use git_remote_object_store::object_store::azure::AzureStore;
use git_remote_object_store::object_store::{
    GetOpts, ObjectStore, ObjectStoreError, ProgressSink, PutOpts,
};
use git_remote_object_store::protocol::backend::{self, BackendError, BackendKind};
use git_remote_object_store::url::{ENV_ALLOW_HTTP, RemoteUrl, parse};
use sha2::{Digest, Sha256};
use testcontainers::core::wait::HttpWaitStrategy;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, ContainerRequest, GenericImage, ImageExt};

/// Azurite Docker image. Pinned by [`AZURITE_TAG`].
const AZURITE_IMAGE: &str = "mcr.microsoft.com/azure-storage/azurite";
/// Azurite image tag. Bump deliberately and re-run the suite.
const AZURITE_TAG: &str = "3.35.0";
/// Blob service port exposed by Azurite. Queue (10001) and Table
/// (10002) ports are emulated but unused by this backend.
const AZURITE_BLOB_PORT: u16 = 10000;
/// Well-known Azurite account name (hardcoded in the emulator, safe
/// to embed in test code).
const TEST_ACCOUNT: &str = "devstoreaccount1";
/// Well-known Azurite account key. Identical to the legacy Storage
/// Emulator key — safe to embed in test code.
const TEST_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
/// Credential alias used by every test URL: `?credential=AZURITE`
/// resolves to `AZSTORE_AZURITE_KEY` via the Azure backend's env-var
/// lookup.
const CREDENTIAL_ALIAS: &str = "AZURITE";
const KEY_ENV_VAR: &str = "AZSTORE_AZURITE_KEY";

fn azurite_image() -> ContainerRequest<GenericImage> {
    // Wait condition: an unauthenticated `GET /` against Azurite's
    // blob port returns HTTP 400 (Account name required) once the
    // server is serving. That's a more reliable readiness signal
    // than parsing log lines.
    let http_wait = HttpWaitStrategy::new("/")
        .with_port(AZURITE_BLOB_PORT.tcp())
        .with_expected_status_code(400_u16);
    GenericImage::new(AZURITE_IMAGE, AZURITE_TAG)
        .with_wait_for(WaitFor::http(http_wait))
        .with_exposed_port(AZURITE_BLOB_PORT.tcp())
        // `--blobHost 0.0.0.0` so the emulator binds on all
        // interfaces inside the container (required for testcontainers'
        // host-side port mapping). The default binds to 127.0.0.1
        // which is not reachable from outside the container.
        //
        // `--skipApiVersionCheck` because the `azure_storage_blob`
        // crate sends `x-ms-version: 2026-04-06` (latest), which
        // pinned Azurite versions don't yet recognise. The
        // request-shape and response semantics are stable across
        // these revisions, so skipping the strict-equality check is
        // safe for our parity coverage.
        .with_cmd([
            "azurite-blob",
            "--blobHost",
            "0.0.0.0",
            "--blobPort",
            "10000",
            "--skipApiVersionCheck",
        ])
}

static AZURITE: OnceLock<AzuriteFixture> = OnceLock::new();
static CONTAINER_COUNTER: AtomicU64 = AtomicU64::new(0);

struct AzuriteFixture {
    /// Owned container handle — keeping it alive keeps the container
    /// alive.
    _container: Container<GenericImage>,
    port: u16,
}

fn fixture() -> &'static AzuriteFixture {
    AZURITE.get_or_init(|| {
        // The shared-key signing policy reads the alias env var at
        // first use; cleartext-HTTP gating reads `ENV_ALLOW_HTTP`.
        // Set both once for the whole test binary.
        // SAFETY: edition 2024 marks `set_var` unsafe because it
        // mutates process-wide state. `OnceLock::get_or_init`
        // guarantees this runs exactly once before any code reads
        // the variables.
        unsafe {
            std::env::set_var(KEY_ENV_VAR, TEST_KEY);
            std::env::set_var(ENV_ALLOW_HTTP, "1");
        }

        // `SyncRunner::start` calls `block_on` internally, which
        // panics if invoked from inside a tokio runtime (every
        // `#[tokio::test]` is one). Run the start on a dedicated
        // `std::thread` that has no ambient runtime, then ferry the
        // result back.
        let handle = std::thread::Builder::new()
            .name("azurite-fixture-start".to_owned())
            .spawn(|| {
                let container = azurite_image().start().expect("Azurite container starts");
                let port = container
                    .get_host_port_ipv4(AZURITE_BLOB_PORT)
                    .expect("Azurite host port");
                AzuriteFixture {
                    _container: container,
                    port,
                }
            })
            .expect("spawn fixture-start thread");
        handle.join().expect("fixture-start thread joins")
    })
}

/// Allocate a fresh container in Azurite and build an `AzureStore`
/// pointed at it (via the same `parse(...) → from_remote_url` path
/// production code uses).
async fn fresh_container() -> AzureStore {
    let fixture = fixture();
    let n = CONTAINER_COUNTER.fetch_add(1, Ordering::SeqCst);
    // Azure container names: 3-63 chars, lowercase alphanumeric + `-`,
    // no leading/trailing dashes.
    let container = format!("test-{}-{}", std::process::id(), n);

    // Create the container via a separate AzureStore-like client
    // path. We don't currently expose container-creation through the
    // trait, so use a raw azure_storage_blob client just for setup.
    create_container(fixture.port, &container).await;

    let url_str = format!(
        "az+http://127.0.0.1:{port}/{TEST_ACCOUNT}/{container}\
         ?addressing=path&credential={alias}",
        port = fixture.port,
        alias = CREDENTIAL_ALIAS,
    );
    let url = parse(&url_str).expect("URL parses");
    let RemoteUrl::Azure { .. } = &url else {
        panic!("parse returned non-Azure variant");
    };
    AzureStore::from_remote_url(&url)
        .await
        .expect("AzureStore::from_remote_url")
}

/// Create a fresh container in the local Azurite via an authenticated
/// HTTP request signed with the well-known shared key. We avoid
/// reaching for the SDK's `BlobContainerClient::create` here because
/// it requires the same custom-policy plumbing the production code
/// owns; a small ad-hoc signed request keeps the test setup
/// self-contained and verifies the signing function end-to-end.
async fn create_container(port: u16, container: &str) {
    use std::time::Duration;

    let endpoint = format!("http://127.0.0.1:{port}/{TEST_ACCOUNT}/{container}?restype=container");
    let url = ::url::Url::parse(&endpoint).expect("setup URL parses");

    let now = time::OffsetDateTime::now_utc();
    let date = now
        .format(&time::format_description::well_known::Rfc2822)
        .expect("format date")
        .replace("+0000", "GMT");

    // Build the headers we need to sign before sending. Azure
    // requires both `x-ms-version` and `x-ms-date` on every signed
    // request.
    let mut headers = Headers::new();
    headers.insert(HeaderName::from_static("x-ms-version"), "2025-11-05");
    headers.insert(HeaderName::from_static("x-ms-date"), date.clone());
    headers.insert(HeaderName::from_static("content-length"), "0");

    let secret = azure_core::credentials::Secret::new(TEST_KEY.to_owned());
    let auth = git_remote_object_store::object_store::azure::auth::compute_authorization(
        TEST_ACCOUNT,
        &secret,
        Method::Put,
        &url,
        &headers,
        None,
    )
    .expect("signs container-create");

    // Send via reqwest to avoid pulling in the full SDK transport for
    // a single setup PUT.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client");
    let resp = client
        .put(endpoint)
        .header("x-ms-version", "2025-11-05")
        .header("x-ms-date", date)
        .header("authorization", auth)
        .header("content-length", "0")
        .send()
        .await
        .expect("create_container request");
    let status = resp.status().as_u16();
    assert!(
        status == 201 || status == 409,
        "unexpected create_container status {status}: {:?}",
        resp.text().await.ok()
    );
}

/// Issue a signed HEAD request against `<account>/<container>/<blob>`
/// and return the response headers. Used by tests that need to inspect
/// blob properties the [`ObjectStore`] trait does not surface
/// (`content-disposition`, `x-ms-meta-*`, etc.) — analogous to the
/// direct SDK `head_object` call the S3 integration tests use.
async fn head_blob_signed(port: u16, container: &str, blob: &str) -> reqwest::header::HeaderMap {
    use std::time::Duration;

    let endpoint = format!("http://127.0.0.1:{port}/{TEST_ACCOUNT}/{container}/{blob}");
    let url = ::url::Url::parse(&endpoint).expect("HEAD URL parses");

    let now = time::OffsetDateTime::now_utc();
    let date = now
        .format(&time::format_description::well_known::Rfc2822)
        .expect("format date")
        .replace("+0000", "GMT");

    let mut headers = Headers::new();
    headers.insert(HeaderName::from_static("x-ms-version"), "2025-11-05");
    headers.insert(HeaderName::from_static("x-ms-date"), date.clone());

    let secret = azure_core::credentials::Secret::new(TEST_KEY.to_owned());
    let auth = git_remote_object_store::object_store::azure::auth::compute_authorization(
        TEST_ACCOUNT,
        &secret,
        Method::Head,
        &url,
        &headers,
        None,
    )
    .expect("signs HEAD blob");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client");
    let resp = client
        .head(endpoint)
        .header("x-ms-version", "2025-11-05")
        .header("x-ms-date", date)
        .header("authorization", auth)
        .send()
        .await
        .expect("HEAD blob request");
    let status = resp.status().as_u16();
    assert!(
        status == 200,
        "unexpected HEAD blob status {status}: {:?}",
        resp.text().await.ok(),
    );
    resp.headers().clone()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn put_then_get_round_trips() {
    let store = fresh_container().await;
    let body = Bytes::from_static(b"hello, azure");
    store
        .put_bytes("greeting", body.clone(), PutOpts::default())
        .await
        .expect("put");
    let fetched = store.get_bytes("greeting").await.expect("get");
    assert_eq!(fetched, body);
}

#[tokio::test]
async fn head_returns_size_and_recent_last_modified() {
    let store = fresh_container().await;
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
    assert!(
        meta.etag.is_some(),
        "Azure get_properties must return an ETag"
    );
}

#[tokio::test]
async fn list_with_empty_prefix_returns_everything() {
    let store = fresh_container().await;
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
async fn list_with_prefix_filters() {
    let store = fresh_container().await;
    for k in ["a/1", "a/2", "b/1"] {
        store
            .put_bytes(k, Bytes::from_static(b"x"), PutOpts::default())
            .await
            .expect("put");
    }
    let mut keys: Vec<String> = store
        .list("a/")
        .await
        .expect("list")
        .into_iter()
        .map(|m| m.key)
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["a/1", "a/2"]);
}

#[tokio::test]
async fn put_if_absent_first_succeeds_second_returns_false() {
    let store = fresh_container().await;
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
    // 16-racer canary: Azurite implements `If-None-Match: *` on Put
    // Blob, so exactly one of N concurrent put_if_absent calls must
    // succeed. Anything else is a regression in either the SDK, our
    // mapping, or Azurite itself.
    let store = fresh_container().await;
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
    let store = fresh_container().await;
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
async fn copy_streams_large_body_through_tempfile() {
    // Regression test for issue #30: `AzureStore::copy` previously
    // buffered the full source body into a `Bytes` allocation via
    // `get_bytes` + `put_bytes`. `manage doctor`'s duplicate-bundle
    // quarantine path uses `copy()`, so a multi-GiB bundle would force
    // the whole body through RAM. The fix streams `src → tempfile →
    // dst`, bounded by the SDK's per-block partition size. This test
    // exercises a body well past the 4 MiB default partition so the
    // streaming `stage_block` + `commit_block_list` upload route runs
    // end-to-end and round-trips byte-identical.
    let store = fresh_container().await;

    let size: usize = 32 * 1024 * 1024;
    let mut payload = vec![0u8; size];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = u8::try_from(i.wrapping_mul(2_654_435_761) & 0xff).unwrap_or(0);
    }
    let mut hasher = Sha256::new();
    hasher.update(&payload);
    let expected_hash = hasher.finalize();

    let tmp = tempfile::tempdir().expect("tempdir");
    let src_path = tmp.path().join("big-src.bin");
    tokio::fs::write(&src_path, &payload).await.expect("write");

    // Use `put_path` to seed the source so we don't pay for a
    // throwaway 32 MiB allocation in the test.
    store
        .put_path("big-src", &src_path, PutOpts::default())
        .await
        .expect("put_path src");

    store.copy("big-src", "big-dst").await.expect("copy");

    let dest = tmp.path().join("downloaded.bin");
    store
        .get_to_file("big-dst", &dest, GetOpts::default())
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
    assert_eq!(actual_hash, expected_hash, "copy round-trip corrupted body");
    assert_eq!(
        std::fs::metadata(&dest).expect("metadata").len(),
        size as u64,
    );
}

#[tokio::test]
async fn copy_zero_byte_blob_round_trips() {
    // Lock files (the original `copy` consumer) are zero bytes. The
    // streaming path must keep them fast and correct: `get_to_file`
    // short-circuits the GET on `size == 0` and writes an empty
    // tempfile, then `put_path` issues a single zero-byte `Put Blob`.
    let store = fresh_container().await;
    store
        .put_bytes("lock-src", Bytes::new(), PutOpts::default())
        .await
        .expect("put empty src");
    store.copy("lock-src", "lock-dst").await.expect("copy");

    let meta = store.head("lock-dst").await.expect("head dst");
    assert_eq!(meta.size, 0);
    let body = store.get_bytes("lock-dst").await.expect("get dst");
    assert!(body.is_empty(), "expected empty body, got {body:?}");
}

#[tokio::test]
async fn copy_missing_source_is_not_found() {
    let store = fresh_container().await;
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
    let store = fresh_container().await;
    store
        .put_bytes("k", Bytes::from_static(b"v"), PutOpts::default())
        .await
        .expect("put");
    store.delete("k").await.expect("first delete");

    let err = store.delete("k").await.expect_err("second delete");
    assert!(matches!(err, ObjectStoreError::NotFound(ref s) if s == "k"));
}

#[tokio::test]
async fn get_missing_key_is_not_found() {
    let store = fresh_container().await;
    let err = store.get_bytes("absent").await.expect_err("get missing");
    assert!(
        matches!(err, ObjectStoreError::NotFound(ref s) if s == "absent"),
        "expected NotFound, got {err:?}"
    );
}

#[tokio::test]
async fn get_to_file_failure_does_not_corrupt_dest() {
    let store = fresh_container().await;
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
async fn get_to_file_round_trips_streaming() {
    let store = fresh_container().await;

    // 4 MiB body: large enough that the SDK's internal range
    // download will be exercised but small enough to keep the test
    // fast on CI. The byte sequence is deterministic so a sha256
    // mismatch would be obvious.
    let size: usize = 4 * 1024 * 1024;
    let mut body = vec![0u8; size];
    for (i, b) in body.iter_mut().enumerate() {
        *b = u8::try_from(i.wrapping_mul(2_654_435_761) & 0xff).unwrap_or(0);
    }
    let mut hasher = Sha256::new();
    hasher.update(&body);
    let expected = hasher.finalize();

    store
        .put_bytes("big", Bytes::from(body), PutOpts::default())
        .await
        .expect("put");

    let tmp = tempfile::tempdir().expect("tempdir");
    let dest = tmp.path().join("downloaded");
    store
        .get_to_file("big", &dest, GetOpts::default())
        .await
        .expect("get_to_file");

    let actual = {
        use std::io::Read;
        let mut f = std::fs::File::open(&dest).expect("open downloaded");
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf).expect("read");
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        hasher.finalize()
    };
    assert_eq!(actual, expected, "downloaded body differs");
    assert_eq!(
        std::fs::metadata(&dest).expect("metadata").len(),
        size as u64
    );
}

#[tokio::test]
async fn get_to_file_zero_byte_blob_round_trips() {
    let store = fresh_container().await;
    store
        .put_bytes("empty", Bytes::new(), PutOpts::default())
        .await
        .expect("put empty");

    let tmp = tempfile::tempdir().expect("tempdir");
    let dest = tmp.path().join("downloaded");
    store
        .get_to_file("empty", &dest, GetOpts::default())
        .await
        .expect("get_to_file empty");
    assert_eq!(std::fs::metadata(&dest).expect("metadata").len(), 0);
}

#[tokio::test]
async fn put_path_streams_file_and_round_trips() {
    let store = fresh_container().await;

    // 32 MiB payload — well past the SDK's 4 MiB default partition size
    // so this exercises the `stage_block` + `commit_block_list` route
    // rather than the single-shot `Put Blob` path. A regression to the
    // trait's default `read-then-put_bytes` shim would still pass byte
    // identity, but the streaming property is the contract issue #42
    // restores; this test guards round-trip correctness for the
    // streaming code path.
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

    store
        .put_path("streamed", &src, PutOpts::default())
        .await
        .expect("put_path");

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
    assert_eq!(
        std::fs::metadata(&dest).expect("metadata").len(),
        size as u64,
    );
}

#[tokio::test]
async fn put_path_with_opts_uploads_body() {
    // Construct the store via the same URL → from_remote_url path that
    // fresh_container() uses, but hold on to the port + container so
    // the test can verify metadata + content-disposition via a signed
    // HEAD (the trait's `head()` does not surface those properties).
    // Mirrors the structure of the S3 sibling test in
    // tests/s3_store_integration.rs::put_path_with_opts_uploads_body.
    let (port, container) = fresh_container_endpoint().await;
    let url_str = format!(
        "az+http://127.0.0.1:{port}/{TEST_ACCOUNT}/{container}\
         ?addressing=path&credential={CREDENTIAL_ALIAS}"
    );
    let url = parse(&url_str).expect("URL parses");
    let store = AzureStore::from_remote_url(&url)
        .await
        .expect("AzureStore::from_remote_url");

    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("small.txt");
    tokio::fs::write(&src, b"hello via path")
        .await
        .expect("write src");

    // Azure metadata names must be valid C# identifiers (no
    // hyphens), unlike S3's `x-amz-meta-*` style. `customkey` is
    // accepted by Azurite and the production endpoint alike.
    let opts = PutOpts {
        content_disposition: Some("attachment; filename=test.txt".into()),
        user_metadata: vec![("customkey".into(), "value".into())],
        progress: None,
    };
    store
        .put_path("meta-test", &src, opts)
        .await
        .expect("put_path");

    let body = store.get_bytes("meta-test").await.expect("get_bytes");
    assert_eq!(&body[..], b"hello via path");

    // Verify the opts actually made it onto the wire — the body-only
    // assertion above would still pass if `put_path` silently dropped
    // both fields. A signed HEAD reads the underlying blob's response
    // headers (`Content-Disposition`, `x-ms-meta-customkey`).
    let headers = head_blob_signed(port, &container, "meta-test").await;
    assert_eq!(
        headers
            .get("content-disposition")
            .and_then(|v| v.to_str().ok()),
        Some("attachment; filename=test.txt"),
        "content_disposition must survive put_path",
    );
    assert_eq!(
        headers
            .get("x-ms-meta-customkey")
            .and_then(|v| v.to_str().ok()),
        Some("value"),
        "user metadata must survive put_path",
    );
}

#[tokio::test]
async fn put_path_zero_byte_file_round_trips() {
    // Empty file: must not break the streaming path. The SDK's
    // `upload` short-circuits to the oneshot `Put Blob` when content
    // length <= partition size, including length = 0.
    let store = fresh_container().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("empty.bin");
    tokio::fs::write(&src, b"").await.expect("write empty src");

    store
        .put_path("empty-stream", &src, PutOpts::default())
        .await
        .expect("put_path empty");

    let body = store.get_bytes("empty-stream").await.expect("get_bytes");
    assert_eq!(body.len(), 0);
}

// ---------------------------------------------------------------------------
// End-to-end binary tests
//
// Drive `git push` / `git clone` against the actual `git-remote-az+http`
// helper binary, with Azurite as the backend. These complement the
// trait-level tests above by exercising the protocol REPL, the URL
// dispatch in `protocol::backend::build`, and the LFS custom-transfer
// agent.
//
// Cargo bin names cannot contain `+`, so each helper is built as
// `git-remote-az-http` and we symlink the binary to `git-remote-az+http`
// in a tempdir prepended to PATH for the duration of these tests. The
// symlink-based PATH shim is unix-only by design.
// ---------------------------------------------------------------------------

#[cfg(not(unix))]
compile_error!("E2E tests are unix-only (symlink-based PATH shim)");

use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Cargo bin path for the Azure HTTP helper. `CARGO_BIN_EXE_<name>` is
/// populated by cargo for any `[[bin]]` defined in this package, and
/// triggers a build of that binary before the integration test runs.
const HELPER_BIN: &str = env!("CARGO_BIN_EXE_git-remote-az-http");
/// Cargo bin path for the LFS custom-transfer agent.
const LFS_BIN: &str = env!("CARGO_BIN_EXE_git-lfs-object-store");

/// On-disk name git looks up when dispatching `az+http://…` URLs. Must
/// be exactly `git-remote-az+http` per `git help gitremote-helpers`.
const HELPER_GIT_NAME: &str = "git-remote-az+http";
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

/// Apply the env-var trio every spawn in this section needs:
///
/// - `PATH` prepended with the helper-symlink directory so spawned
///   tools find the `+`-named binaries.
/// - User / system git config redirected to `/dev/null` so the host's
///   `~/.gitconfig` cannot leak into the test.
fn hermetic_env(cmd: &mut Command) -> &mut Command {
    cmd.env("PATH", hermetic_path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
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

/// Allocate a fresh container in Azurite (used by E2E tests that drive
/// the helper binary rather than the [`AzureStore`] API directly)
/// and return `(host_port, container_name)`.
async fn fresh_container_endpoint() -> (u16, String) {
    let fixture = fixture();
    let n = CONTAINER_COUNTER.fetch_add(1, Ordering::SeqCst);
    let container = format!("e2e-{}-{}", std::process::id(), n);
    create_container(fixture.port, &container).await;
    (fixture.port, container)
}

/// Build the helper URL for the given Azurite endpoint and container.
/// Azurite's loopback endpoint has no DNS rewriting, so we pin
/// path-style addressing and a fixed `repo` prefix here.
fn helper_url(port: u16, container: &str) -> String {
    format!(
        "az+http://127.0.0.1:{port}/{TEST_ACCOUNT}/{container}/repo\
         ?addressing=path&credential={CREDENTIAL_ALIAS}"
    )
}

#[tokio::test]
async fn helper_binary_round_trips_init_push_clone_fetch() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }

    let (port, container) = fresh_container_endpoint().await;
    let url = helper_url(port, &container);

    // Source repo: one commit on main.
    let seed = init_seed_repo();
    std::fs::write(seed.path().join("hello.txt"), b"hello world\n").expect("write seed file");
    run_git(&["add", "hello.txt"], seed.path());
    commit(seed.path(), "seed");

    // Allow the +http scheme inside this repo's submodule resolution
    // path (defensive — `protocol.az+http.allow` is irrelevant for the
    // top-level remote, but mirrors the documented config) and add the
    // remote.
    run_git(&["config", "protocol.az+http.allow", "always"], seed.path());
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
            "protocol.az+http.allow=always",
            "clone",
            "--quiet",
            &url,
            dest_str,
        ],
        dest_parent.path(),
    );

    let cloned_body = std::fs::read(dest.join("hello.txt")).expect("cloned file readable");
    assert_eq!(cloned_body, b"hello world\n");

    // Second commit + fetch round-trip: confirms incremental fetch
    // works as well as the first-clone path.
    std::fs::write(seed.path().join("hello.txt"), b"hello world v2\n").expect("rewrite seed file");
    run_git(&["add", "hello.txt"], seed.path());
    commit(seed.path(), "v2");
    run_git(&["push", "origin", "main"], seed.path());

    run_git(&["fetch", "origin", "main"], &dest);
    run_git(&["reset", "--hard", "origin/main"], &dest);

    let updated = std::fs::read(dest.join("hello.txt")).expect("updated file");
    assert_eq!(updated, b"hello world v2\n");
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

    let (port, container) = fresh_container_endpoint().await;
    let url = helper_url(port, &container);

    // Build a repo with one LFS-tracked binary file.
    let seed = init_seed_repo();
    run_git(&["config", "protocol.az+http.allow", "always"], seed.path());
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
            "protocol.az+http.allow=always",
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
async fn build_against_existing_container_succeeds() {
    let _store = fresh_container().await;
    let fixture = fixture();
    // Re-derive the URL the same way `fresh_container` does, so the
    // probe runs through the public `backend::build` entrypoint.
    let n = CONTAINER_COUNTER.fetch_add(1, Ordering::SeqCst);
    let container = format!("test-probe-ok-{}-{}", std::process::id(), n);
    create_container(fixture.port, &container).await;
    let url_str = format!(
        "az+http://127.0.0.1:{port}/{TEST_ACCOUNT}/{container}\
         ?addressing=path&credential={alias}",
        port = fixture.port,
        alias = CREDENTIAL_ALIAS,
    );
    let url = parse(&url_str).expect("URL parses");
    backend::build(&url)
        .await
        .expect("probe against existing empty container succeeds");
}

#[tokio::test]
async fn build_against_missing_container_returns_bucket_not_found() {
    let fixture = fixture();
    let n = CONTAINER_COUNTER.fetch_add(1, Ordering::SeqCst);
    let container = format!("missing-{}-{}", std::process::id(), n);
    let url_str = format!(
        "az+http://127.0.0.1:{port}/{TEST_ACCOUNT}/{container}\
         ?addressing=path&credential={alias}",
        port = fixture.port,
        alias = CREDENTIAL_ALIAS,
    );
    let url = parse(&url_str).expect("URL parses");

    let Err(err) = backend::build(&url).await else {
        panic!("missing container must error");
    };
    // Render the *actual* returned error — a manually-constructed
    // value would not catch a regression where `backend::build`
    // populates the variant with the wrong name or kind.
    assert_eq!(
        backend::fatal_message(&err),
        format!("fatal: container not found {container}"),
    );
    let BackendError::BucketNotFound { kind, name } = err else {
        panic!("expected BucketNotFound, got {err:?}");
    };
    assert_eq!(kind, BackendKind::Azure);
    assert_eq!(name, container);
}

mod common;
use common::{
    LARGE_BODY_CHUNK_SIZE, LARGE_BODY_ENV_VAR, LARGE_BODY_TEST_SIZE, MIDBODY_ABORT_TEST_SIZE,
    MULTIPART_TEST_SIZE, deterministic_payload, large_body_tests_enabled, sha256_of,
    sha256_of_file, spawn_truncator, write_repeating_pattern_file,
};

/// `put_bytes` above the multipart threshold drives explicit
/// `stage_block` + `commit_block_list`. Same dispatch criterion as
/// S3 (issue #53). 80 MiB body splits into 5 blocks at 16 MiB.
#[tokio::test]
async fn multipart_put_bytes_round_trips() {
    let store = fresh_container().await;
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
        "multipart upload corrupted body"
    );
}

/// `put_path` above the multipart threshold drives streaming
/// `stage_block` (each block read from disk independently).
#[tokio::test]
async fn multipart_put_path_round_trips() {
    let store = fresh_container().await;
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

/// Multipart uploads emit one progress event per completed block so
/// the LFS agent can drive a live progress bar. With an 80 MiB body
/// and 16 MiB blocks, expect at least 2 events.
#[tokio::test]
async fn multipart_put_emits_per_block_progress_events() {
    let store = fresh_container().await;
    let payload = deterministic_payload(MULTIPART_TEST_SIZE);

    let events: Arc<std::sync::Mutex<Vec<u64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&events);
    let sink = ProgressSink::new(move |bytes| {
        recorded.lock().expect("progress lock").push(bytes);
    });

    let opts = PutOpts {
        progress: Some(sink),
        ..PutOpts::default()
    };
    store
        .put_bytes("progress-events", Bytes::from(payload), opts)
        .await
        .expect("multipart put_bytes with progress");

    let observed = events.lock().expect("progress lock").clone();
    assert!(
        observed.len() >= 2,
        "expected ≥ 2 progress events, got {observed:?}",
    );
    let total: u64 = observed.iter().sum();
    assert_eq!(
        total, MULTIPART_TEST_SIZE as u64,
        "progress events must sum to the body size",
    );
}

/// `put_path` above the multipart threshold also emits per-block
/// progress events — the streaming-from-disk path (`pread` per
/// block) drives the same `stage_blocks_from_file` loop that the
/// bytes path drives via `stage_blocks_with_bodies`. The bundle
/// upload site in `protocol/push.rs` is `put_path`-only, so without
/// this test the "bundle progress" half of issue #55's acceptance
/// criteria has no coverage on a real backend.
#[tokio::test]
async fn multipart_put_path_emits_per_block_progress_events() {
    let store = fresh_container().await;
    let payload = deterministic_payload(MULTIPART_TEST_SIZE);
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("progress-src.bin");
    tokio::fs::write(&src, &payload).await.expect("write src");

    let events: Arc<std::sync::Mutex<Vec<u64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&events);
    let sink = ProgressSink::new(move |bytes| {
        recorded.lock().expect("progress lock").push(bytes);
    });

    let opts = PutOpts {
        progress: Some(sink),
        ..PutOpts::default()
    };
    store
        .put_path("progress-path", &src, opts)
        .await
        .expect("multipart put_path with progress");

    let observed = events.lock().expect("progress lock").clone();
    assert!(
        observed.len() >= 2,
        "expected ≥ 2 progress events from put_path, got {observed:?}",
    );
    let total: u64 = observed.iter().sum();
    assert_eq!(
        total, MULTIPART_TEST_SIZE as u64,
        "put_path progress events must sum to the body size",
    );
}

/// Optional regression test for the > 5 GiB body class. Skipped by
/// default because a ~6 GiB body needs ~12 GiB of free disk for the
/// round-trip check. Enable with `RUN_LARGE_BODY_TESTS=1`.
///
/// Azure has no analogue of S3's 5 GiB single-PUT ceiling — `Put
/// Blob` accepts up to 5 000 MiB and the SDK already chunks larger
/// bodies — but the 50 000-block ceiling and multipart progress
/// behavior in this size class are still worth pinning. With our 16
/// MiB part size, a 6 GiB body splits into 384 blocks; the test
/// asserts the body round-trips byte-identical. Issue #56.
#[tokio::test]
#[ignore = "requires RUN_LARGE_BODY_TESTS=1 and ~12 GiB of free disk; see .claude/rules/testing.md"]
async fn multipart_put_path_above_5_gib_round_trips() {
    if !large_body_tests_enabled() {
        eprintln!("skipping: {LARGE_BODY_ENV_VAR} not set");
        return;
    }
    let store = fresh_container().await;

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
/// unreadable, the abort path fires and NO destination blob is
/// visible. The deterministic part-read failure injection lives in
/// `object_store::multipart::tests::read_file_part_propagates_eof_after_truncate`
/// (unit test). This test is the integration counterpart asserting
/// the visible end-state on a real backend, using a body whose
/// multipart plan stays partly pending past the truncate window.
///
/// Expected outcome on Azure: `put_path` errors. Azure has no client-
/// side abort call (uncommitted blocks Azure already staged simply
/// expire after seven days; `commit_block_list` is never called when
/// `stage_blocks_from_file` returns `Err`), so `head(key)` returns
/// `NotFound` because the blob never became committed.
///
/// Body size and concurrency rationale for the truncate window live
/// on `MIDBODY_ABORT_TEST_SIZE` (`cli/tests/common/mod.rs`); 50 ms is
/// long enough for the first `stage_block` requests to leave the
/// process and short enough that queued preads have not yet fired
/// against localhost Azurite.
#[tokio::test]
async fn multipart_put_path_aborts_on_midbody_truncation() {
    use std::time::Duration;

    let store = fresh_container().await;
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
        .expect_err("destination must not be visible after a failed multipart upload");
    assert!(
        matches!(head_err, ObjectStoreError::NotFound(_)),
        "expected NotFound on aborted destination, got {head_err:?}"
    );
}

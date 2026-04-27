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
// Cargo bin names cannot contain `+` (execution-plan.md §5.6), so each
// helper is built as `git-remote-s3-http` and we symlink the binary to
// `git-remote-s3+http` in a tempdir prepended to PATH for the duration
// of these tests. The symlink-based PATH shim is unix-only by design.
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

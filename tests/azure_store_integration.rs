//! Integration tests for [`AzureBlobStore`][a] against a real Azure
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
use git_remote_object_store::object_store::azure::AzureBlobStore;
use git_remote_object_store::object_store::{Error, ObjectStore, PutOpts};
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

/// Allocate a fresh container in Azurite and build an `AzureBlobStore`
/// pointed at it (via the same `parse(...) → from_remote_url` path
/// production code uses).
async fn fresh_container() -> AzureBlobStore {
    let fixture = fixture();
    let n = CONTAINER_COUNTER.fetch_add(1, Ordering::SeqCst);
    // Azure container names: 3-63 chars, lowercase alphanumeric + `-`,
    // no leading/trailing dashes.
    let container = format!("test-{}-{}", std::process::id(), n);

    // Create the container via a separate AzureBlobStore-like client
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
    AzureBlobStore::from_remote_url(&url)
        .await
        .expect("AzureBlobStore::from_remote_url")
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
    // 16-racer canary: this is the parity test called out by the
    // Phase 11 issue. Azurite implements `If-None-Match: *` on Put
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
async fn copy_missing_source_is_not_found() {
    let store = fresh_container().await;
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
    let store = fresh_container().await;
    store
        .put_bytes("k", Bytes::from_static(b"v"), PutOpts::default())
        .await
        .expect("put");
    store.delete("k").await.expect("first delete");

    let err = store.delete("k").await.expect_err("second delete");
    assert!(matches!(err, Error::NotFound(ref s) if s == "k"));
}

#[tokio::test]
async fn get_missing_key_is_not_found() {
    let store = fresh_container().await;
    let err = store.get_bytes("absent").await.expect_err("get missing");
    assert!(
        matches!(err, Error::NotFound(ref s) if s == "absent"),
        "expected NotFound, got {err:?}"
    );
}

#[tokio::test]
async fn get_to_file_failure_does_not_corrupt_dest() {
    let store = fresh_container().await;
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
    store.get_to_file("big", &dest).await.expect("get_to_file");

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
        .get_to_file("empty", &dest)
        .await
        .expect("get_to_file empty");
    assert_eq!(std::fs::metadata(&dest).expect("metadata").len(), 0);
}

// ---------------------------------------------------------------------------
// End-to-end binary tests (Phase 12)
//
// Drive `git push` / `git clone` against the actual `git-remote-az+http`
// helper binary, with Azurite as the backend. These complement the
// trait-level tests above by exercising the protocol REPL, the URL
// dispatch in `protocol::backend::build`, and the LFS custom-transfer
// agent.
//
// Cargo bin names cannot contain `+` (execution-plan.md §5.6), so each
// helper is built as `git-remote-az-http` and we symlink the binary to
// `git-remote-az+http` in a tempdir prepended to PATH for the duration
// of these tests. The symlink-based PATH shim is unix-only by design.
// ---------------------------------------------------------------------------

#[cfg(not(unix))]
compile_error!("Phase 12 E2E tests are unix-only (symlink-based PATH shim)");

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
/// the helper binary rather than the [`AzureBlobStore`] API directly)
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
    std::fs::write(seed.path().join("hello.txt"), b"hello phase 12\n").expect("write seed file");
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
    assert_eq!(cloned_body, b"hello phase 12\n");

    // Second commit + fetch round-trip: confirms incremental fetch
    // works as well as the first-clone path.
    std::fs::write(seed.path().join("hello.txt"), b"hello phase 12 v2\n")
        .expect("rewrite seed file");
    run_git(&["add", "hello.txt"], seed.path());
    commit(seed.path(), "v2");
    run_git(&["push", "origin", "main"], seed.path());

    run_git(&["fetch", "origin", "main"], &dest);
    run_git(&["reset", "--hard", "origin/main"], &dest);

    let updated = std::fs::read(dest.join("hello.txt")).expect("updated file");
    assert_eq!(updated, b"hello phase 12 v2\n");
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

//! Live packchain integration tests against RustFS — issue #69.
//!
//! Each `#[tokio::test]` allocates a fresh bucket via the shared
//! `RustFsFixture` (lazy-init `OnceLock` + dedicated startup thread,
//! so all the per-bucket runs reuse one container) and drives a
//! backend-agnostic scenario from
//! [`crate::common::packchain_live`]. The S3 layer (path-style
//! URL, `s3+http` scheme, `?addressing=path`) is the only thing
//! these wrappers add — the scenarios themselves only know about
//! `Arc<dyn ObjectStore>` + `RemoteUrl`.
//!
//! Gated on the `integration-s3` Cargo feature, mirroring
//! `s3_store_integration.rs`. CI runs:
//!
//! ```text
//! cargo test --features integration-s3 -- packchain_live_s3
//! ```
//!
//! The fixture, image pin, and credentials infrastructure are
//! intentionally duplicated from `s3_store_integration.rs` rather
//! than extracted to a shared module: each cargo `tests/<name>.rs`
//! file is its own test binary, so a fixture lifted into
//! `cli/tests/common/mod.rs` would duplicate the heavyweight
//! container start across two test binaries instead of one. Sharing
//! by re-running the same `OnceLock` per binary is the cheaper option.

#![cfg(feature = "integration-s3")]

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use git_remote_object_store::object_store::ObjectStore;
use git_remote_object_store::object_store::s3::S3Store;
use git_remote_object_store::url::{ENV_ALLOW_HTTP, RemoteUrl, parse};
use testcontainers::core::wait::HttpWaitStrategy;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, ContainerRequest, GenericImage, ImageExt};

mod common;

const RUSTFS_IMAGE: &str = "rustfs/rustfs";
const RUSTFS_TAG: &str = "1.0.0-alpha.99";
const RUSTFS_API_PORT: u16 = 9000;
const TEST_USER: &str = "rustfsadmin";
const TEST_PASSWORD: &str = "rustfsadmin";

fn rustfs_image() -> ContainerRequest<GenericImage> {
    let http_wait = HttpWaitStrategy::new("/")
        .with_port(RUSTFS_API_PORT.tcp())
        .with_expected_status_code(403_u16);
    GenericImage::new(RUSTFS_IMAGE, RUSTFS_TAG)
        .with_wait_for(WaitFor::http(http_wait))
        .with_exposed_port(RUSTFS_API_PORT.tcp())
        .with_env_var("RUSTFS_OBS_LOG_DIRECTORY", "")
}

static RUSTFS: OnceLock<RustFsFixture> = OnceLock::new();
static BUCKET_COUNTER: AtomicU64 = AtomicU64::new(0);

struct RustFsFixture {
    _container: Container<GenericImage>,
    port: u16,
}

fn fixture() -> &'static RustFsFixture {
    RUSTFS.get_or_init(|| {
        // SAFETY: OnceLock guarantees this runs once before any
        // other code reads these variables.
        unsafe {
            std::env::set_var("AWS_ACCESS_KEY_ID", TEST_USER);
            std::env::set_var("AWS_SECRET_ACCESS_KEY", TEST_PASSWORD);
            std::env::set_var(ENV_ALLOW_HTTP, "1");
        }

        // SyncRunner::start uses block_on; spawn on a dedicated thread
        // so we don't nest tokio runtimes.
        let handle = std::thread::Builder::new()
            .name("rustfs-packchain-fixture-start".to_owned())
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

/// Allocate a fresh bucket and build an `Arc<dyn ObjectStore>` plus
/// the matching `?engine=packchain` URL. Returns the trio scenarios
/// expect: store, parsed URL, prefix string (empty for bucket-root).
async fn fresh_packchain_bucket(prefix: Option<&str>) -> (Arc<dyn ObjectStore>, RemoteUrl, String) {
    let fixture = fixture();
    let n = BUCKET_COUNTER.fetch_add(1, Ordering::SeqCst);
    let bucket = format!("test-pc-{}-{}", std::process::id(), n);

    let setup = setup_client(fixture.port).await;
    setup
        .create_bucket()
        .bucket(&bucket)
        .send()
        .await
        .expect("create_bucket succeeds");

    let path_segment = match prefix {
        Some(p) => format!("{bucket}/{p}"),
        None => bucket.clone(),
    };
    let url_str = format!(
        "s3+http://127.0.0.1:{port}/{path_segment}?addressing=path&engine=packchain",
        port = fixture.port,
    );
    let url = parse(&url_str).expect("URL parses");
    let RemoteUrl::S3 { .. } = &url else {
        panic!("parse returned non-S3 variant for {url_str}");
    };

    let store = S3Store::from_remote_url(&url)
        .await
        .expect("S3Store::from_remote_url");
    (
        Arc::new(store) as Arc<dyn ObjectStore>,
        url,
        prefix.unwrap_or("").to_owned(),
    )
}

// ---------------------------------------------------------------------------
// Phase 2 (push)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn first_push_writes_packchain_layout_at_bucket_root() {
    let (store, url, prefix) = fresh_packchain_bucket(None).await;
    common::packchain_live::first_push_writes_packchain_layout(store, &url, &prefix).await;
}

#[tokio::test]
async fn first_push_writes_packchain_layout_under_repo_prefix() {
    let (store, url, prefix) = fresh_packchain_bucket(Some("my-repo")).await;
    common::packchain_live::first_push_writes_packchain_layout(store, &url, &prefix).await;
}

#[tokio::test]
async fn incremental_push_appends_segment_newest_first() {
    let (store, url, prefix) = fresh_packchain_bucket(Some("repo")).await;
    common::packchain_live::incremental_push_appends_segment(store, &url, &prefix).await;
}

#[tokio::test]
async fn force_push_collapses_chain_to_single_segment() {
    let (store, url, prefix) = fresh_packchain_bucket(Some("repo")).await;
    common::packchain_live::force_push_collapses_chain(store, &url, &prefix).await;
}

// ---------------------------------------------------------------------------
// Phase 3 (fetch)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_into_empty_repo_lands_tip() {
    let (store, url, _prefix) = fresh_packchain_bucket(Some("repo")).await;
    common::packchain_live::fetch_into_empty_repo_lands_tip(store, &url).await;
}

#[tokio::test]
async fn chain_walk_fetch_installs_all_segments() {
    let (store, url, _prefix) = fresh_packchain_bucket(Some("repo")).await;
    common::packchain_live::chain_walk_fetch_installs_all_segments(store, &url).await;
}

// ---------------------------------------------------------------------------
// Phase 4 (read_blob)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_blob_byte_equal_and_pack_index_cache_survives_idx_delete() {
    let (store, url, prefix) = fresh_packchain_bucket(Some("repo")).await;
    common::packchain_live::read_blob_returns_byte_equal_content_and_cache_survives_idx_delete(
        store, &url, &prefix,
    )
    .await;
}

// ---------------------------------------------------------------------------
// Phase 5 (gc)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mark_then_sweep_after_grace_deletes_orphans() {
    let (store, url, prefix) = fresh_packchain_bucket(Some("repo")).await;
    common::packchain_live::mark_then_sweep_after_grace_deletes_orphans(store, &url, &prefix).await;
}

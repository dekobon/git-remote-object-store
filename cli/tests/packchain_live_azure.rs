//! Live packchain integration tests against Azurite — issue #69.
//!
//! Mirrors `packchain_live_s3.rs`'s structure: each `#[tokio::test]`
//! allocates a fresh container against the shared `AzuriteFixture`
//! and drives a backend-agnostic scenario from
//! [`crate::common::packchain_live`]. The Azure layer (path-style
//! URL, `?credential=AZURITE` alias resolved via the
//! `AZSTORE_AZURITE_KEY` env var, well-known emulator account /
//! key) is the only thing these wrappers add — the scenarios
//! themselves only know about `Arc<dyn ObjectStore>` + `RemoteUrl`.
//!
//! Gated on the `integration-azure` Cargo feature, mirroring
//! `azure_store_integration.rs`. CI runs:
//!
//! ```text
//! cargo test --features integration-azure -- packchain_live_azure
//! ```

#![cfg(feature = "integration-azure")]

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use azure_core::http::Method;
use azure_core::http::headers::{HeaderName, Headers};
use git_remote_object_store::object_store::ObjectStore;
use git_remote_object_store::object_store::azure::AzureStore;
use git_remote_object_store::url::{ENV_ALLOW_HTTP, RemoteUrl, parse};
use testcontainers::core::wait::HttpWaitStrategy;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, ContainerRequest, GenericImage, ImageExt};

mod common;

const AZURITE_IMAGE: &str = "mcr.microsoft.com/azure-storage/azurite";
const AZURITE_TAG: &str = "3.35.0";
const AZURITE_BLOB_PORT: u16 = 10000;
const TEST_ACCOUNT: &str = "devstoreaccount1";
const TEST_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
const CREDENTIAL_ALIAS: &str = "AZURITE";
const KEY_ENV_VAR: &str = "AZSTORE_AZURITE_KEY";

fn azurite_image() -> ContainerRequest<GenericImage> {
    let http_wait = HttpWaitStrategy::new("/")
        .with_port(AZURITE_BLOB_PORT.tcp())
        .with_expected_status_code(400_u16);
    GenericImage::new(AZURITE_IMAGE, AZURITE_TAG)
        .with_wait_for(WaitFor::http(http_wait))
        .with_exposed_port(AZURITE_BLOB_PORT.tcp())
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
    _container: Container<GenericImage>,
    port: u16,
}

fn fixture() -> &'static AzuriteFixture {
    AZURITE.get_or_init(|| {
        // SAFETY: OnceLock guarantees this runs once before any code
        // reads the variables.
        unsafe {
            std::env::set_var(KEY_ENV_VAR, TEST_KEY);
            std::env::set_var(ENV_ALLOW_HTTP, "1");
        }

        let handle = std::thread::Builder::new()
            .name("azurite-packchain-fixture-start".to_owned())
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

/// Create a fresh container in Azurite. Mirrors
/// `azure_store_integration::create_container` (signed PUT against
/// `?restype=container`) — the SDK's `BlobContainerClient::create`
/// would need the same custom-policy plumbing production code owns,
/// and a small ad-hoc signed request keeps test setup self-contained.
async fn create_container(port: u16, container: &str) {
    use std::time::Duration;

    let endpoint = format!("http://127.0.0.1:{port}/{TEST_ACCOUNT}/{container}?restype=container");
    let url = ::url::Url::parse(&endpoint).expect("setup URL parses");

    let now = time::OffsetDateTime::now_utc();
    let date = now
        .format(&time::format_description::well_known::Rfc2822)
        .expect("format date")
        .replace("+0000", "GMT");

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

/// Allocate a fresh container in Azurite and build an
/// `Arc<dyn ObjectStore>` plus the matching `?engine=packchain`
/// URL. Returns the trio scenarios expect: store, parsed URL,
/// prefix string (empty for container-root).
async fn fresh_packchain_container(
    prefix: Option<&str>,
) -> (Arc<dyn ObjectStore>, RemoteUrl, String) {
    let fixture = fixture();
    let n = CONTAINER_COUNTER.fetch_add(1, Ordering::SeqCst);
    let container = format!("test-pc-{}-{}", std::process::id(), n);
    create_container(fixture.port, &container).await;

    let account_path = match prefix {
        Some(p) => format!("{TEST_ACCOUNT}/{container}/{p}"),
        None => format!("{TEST_ACCOUNT}/{container}"),
    };
    let url_str = format!(
        "az+http://127.0.0.1:{port}/{account_path}\
         ?addressing=path&credential={alias}&engine=packchain",
        port = fixture.port,
        alias = CREDENTIAL_ALIAS,
    );
    let url = parse(&url_str).expect("URL parses");
    let RemoteUrl::Azure { .. } = &url else {
        panic!("parse returned non-Azure variant for {url_str}");
    };
    let store = AzureStore::from_remote_url(&url)
        .await
        .expect("AzureStore::from_remote_url");
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
async fn first_push_writes_packchain_layout_at_container_root() {
    let (store, url, prefix) = fresh_packchain_container(None).await;
    common::packchain_live::first_push_writes_packchain_layout(store, &url, &prefix).await;
}

#[tokio::test]
async fn first_push_writes_packchain_layout_under_repo_prefix() {
    let (store, url, prefix) = fresh_packchain_container(Some("my-repo")).await;
    common::packchain_live::first_push_writes_packchain_layout(store, &url, &prefix).await;
}

#[tokio::test]
async fn incremental_push_appends_segment_newest_first() {
    let (store, url, prefix) = fresh_packchain_container(Some("repo")).await;
    common::packchain_live::incremental_push_appends_segment(store, &url, &prefix).await;
}

#[tokio::test]
async fn force_push_collapses_chain_to_single_segment() {
    let (store, url, prefix) = fresh_packchain_container(Some("repo")).await;
    common::packchain_live::force_push_collapses_chain(store, &url, &prefix).await;
}

// ---------------------------------------------------------------------------
// Phase 3 (fetch)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_into_empty_repo_lands_tip() {
    let (store, url, _prefix) = fresh_packchain_container(Some("repo")).await;
    common::packchain_live::fetch_into_empty_repo_lands_tip(store, &url).await;
}

#[tokio::test]
async fn chain_walk_fetch_installs_all_segments() {
    let (store, url, _prefix) = fresh_packchain_container(Some("repo")).await;
    common::packchain_live::chain_walk_fetch_installs_all_segments(store, &url).await;
}

// ---------------------------------------------------------------------------
// Phase 4 (read_blob)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_blob_byte_equal_and_pack_index_cache_survives_idx_delete() {
    let (store, url, prefix) = fresh_packchain_container(Some("repo")).await;
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
    let (store, url, prefix) = fresh_packchain_container(Some("repo")).await;
    common::packchain_live::mark_then_sweep_after_grace_deletes_orphans(store, &url, &prefix).await;
}

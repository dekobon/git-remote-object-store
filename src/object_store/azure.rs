//! Azure Blob Storage backend for the [`ObjectStore`][super::ObjectStore]
//! trait.
//!
//! `AzureStore` wraps `azure_storage_blob`. Like the S3 backend, this
//! module owns the URL → SDK config translation, the error-code
//! classifier ([`classify`]), and the credential resolution plumbing.
//! Unlike S3, the SDK already does parallel range downloads inside
//! `BlobClient::download()`, so there is no hand-rolled multipart
//! orchestrator (asymmetric with S3 by design).
//!
//! ## Authentication
//!
//! The official `azure_storage_blob` 0.12 crate currently exposes only
//! `Arc<dyn TokenCredential>` (Entra ID) on its constructors. Azurite
//! does not implement Entra ID without an `--oauth basic` HTTPS setup,
//! and many production accounts still authenticate with shared keys.
//! To bridge both, we install our own [`auth::SharedKeySigningPolicy`]
//! as a per-try [`azure_core::http::policies::Policy`] and pass `None`
//! for the SDK's `credential` parameter. The SDK then forwards every
//! request through our policy, which signs the request using the Azure
//! Storage shared-key v2 scheme. Tracking issue:
//! `Azure/azure-sdk-for-rust#2975`.
//!
//! Resolution order for `?credential=<NAME>` in the URL:
//!
//! 1. `AZSTORE_<NAME>_KEY` — base64 account key → shared-key signing.
//! 2. `AZSTORE_<NAME>_CONNECTION_STRING` — connection string with
//!    `AccountName=` / `AccountKey=` → shared-key signing.
//! 3. `AZSTORE_<NAME>_SAS` — SAS query string appended verbatim to
//!    every outgoing request URL.
//!
//! When no `?credential=` flag is set we fall back to
//! `azure_identity::DeveloperToolsCredential` (env, workload identity,
//! managed identity, Azure CLI, ...).
//!
//! ## Conditional writes
//!
//! [`put_if_absent`][super::ObjectStore::put_if_absent] uses
//! `If-None-Match: "*"` (the SDK's
//! `BlockBlobClientUploadOptions::with_if_not_exists` convenience).
//! Azure returns 409 (`BlobAlreadyExists`) or 412
//! (`ConditionNotMet`) for the contention case; both collapse to
//! `Ok(false)`.
//!
//! ## Atomic `get_to_file`
//!
//! Identical to the S3 path: `head` → tempfile → `download(if_match)` →
//! persist. The SDK's `download()` aggregates parallel range fetches
//! internally, so no per-chunk semaphore here. A single retry with a
//! fresh `ETag` covers the head-then-`GET` race (412 mid-download).
//!
//! ## `copy(src, dst)`
//!
//! `azure_storage_blob` 0.12 does not expose a `BlobClient::copy_from_url`
//! method (only `BlockBlobClient::upload_blob_from_url`, which requires
//! a SAS-tokened source URL or an `x-ms-copy-source-authorization`
//! header — neither integrates cleanly with our credential model). We
//! implement `copy` as a stream-through-tempfile round trip:
//! `get_to_file` writes `src` to a `NamedTempFile`, then `put_path`
//! uploads it to `dst`. Both legs already stream — `get_to_file`
//! consumes the SDK's chunked download into the file without buffering
//! the body, and `put_path` wraps the file in a `SeekableStream` that
//! the SDK uploads via `stage_block` + `commit_block_list` for large
//! bodies. Memory stays bounded by the SDK's per-block partition size
//! (4 MiB by default) regardless of blob size, which matters for
//! `manage doctor`'s duplicate-bundle quarantine path
//! (`Doctor::evict_losing_bundle`) — that path can copy multi-GiB
//! bundles. Zero-byte lock files (the original §5.2 consumer) still
//! round-trip fast: `get_to_file` short-circuits the GET on `size == 0`
//! and `put_path` issues a single zero-byte `Put Blob`. Body is
//! preserved; user metadata is not propagated, mirroring upstream
//! `git-remote-s3`'s S3 `CopyObject` and Python lock-copy paths which
//! similarly only carry body bytes.
//!
//! This is asymmetric with the S3 backend, which uses `CopyObject` for
//! a true server-side copy — Azure's equivalent (`Copy Blob`,
//! `Put Blob From URL`) requires a SAS-signed source URL or an
//! `x-ms-copy-source-authorization` header that the 0.12 SDK does not
//! ergonomically expose. The download+reupload path is the safe
//! correct fallback until the SDK closes that gap.
//!
//! ## A note on `Range` and zero-byte blobs
//!
//! A `Range` request against a zero-byte blob returns HTTP 416. We
//! never issue Range requests directly — `BlobClient::download()`
//! owns that — but the zero-size short-circuit in
//! [`get_to_file`](ObjectStore::get_to_file) also avoids any download
//! SDK call against a known-empty blob, which sidesteps the issue
//! entirely.
//!
//! ## HTTP transport tuning
//!
//! `azure_core` 0.35's default transport keeps idle pooled connections
//! forever and never sets TCP keepalive, so a pooled connection to a
//! rotated VIP would hang an in-flight request until the OS-level TCP
//! retransmit timeout fires (~15 minutes on Linux). [`AzureStore`]
//! installs a custom [`reqwest::Client`] via [`Transport`] on
//! [`ClientOptions::transport`] with four bounds:
//!
//! - [`POOL_IDLE_TIMEOUT`] (30 s) — drops idle pooled connections
//!   before a typical DNS rotation makes them stale.
//! - [`TCP_KEEPALIVE`] (30 s) — detects a dead-but-not-closed TCP
//!   session in seconds rather than the 2-hour Linux default; covers
//!   *hot* pooled connections that pool-idle alone cannot.
//! - [`CONNECT_TIMEOUT`] (10 s) — bounds a fresh-connect attempt to
//!   a dead VIP rather than waiting on the OS connect timeout.
//! - [`READ_TIMEOUT`] (30 s) — per-read timeout that resets after a
//!   successful read, so a stuck transfer fails fast without limiting
//!   total body size.
//!
//! Together these cap a DNS-rotation hang at tens of seconds rather
//! than minutes. The custom transport leaves
//! [`ClientOptions::per_try_policies`] (where the shared-key signing
//! lives) untouched — the SDK pipeline runs per-try policies
//! independently of the transport. Tracking issue: #26.
//!
//! ## Stdout discipline
//!
//! Per `.claude/rules/protocol-stdout.md`, this module never writes to
//! stdout. Diagnostics go through `tracing` (which the helper binaries
//! configure to write to stderr).

pub mod auth;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use azure_core::http::headers::{HeaderName, Headers};
use azure_core::http::request::RequestContent;
use azure_core::http::{ClientOptions, Transport};
use azure_storage_blob::clients::{BlobClient, BlobContainerClient, BlobContainerClientOptions};
use azure_storage_blob::models::method_options::BlockBlobClientUploadOptions;
use azure_storage_blob::models::{
    BlobClientDeleteOptions, BlobClientDownloadOptions, BlobClientGetPropertiesOptions,
    BlobContainerClientListBlobsOptions,
};
use azure_storage_blob::stream::tokio::FileStream;
use bytes::Bytes;
use futures::StreamExt;
use tempfile::NamedTempFile;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;
use url::Url;

use crate::url::{AzureAddressing, RemoteUrl};

use super::error::{network_boxed, other_boxed};
use super::{
    GetOpts, ObjectMeta, ObjectStore, ObjectStoreError, ProgressSink, PutOpts, persist_temp,
};

/// Bound on how long an idle pooled HTTPS connection lingers before
/// the [`reqwest`] connection pool drops it. Short enough that DNS
/// rotation rarely hits a stale pooled connection; long enough that
/// bursty fetch / push batches still benefit from connection reuse.
/// See module-level "HTTP transport tuning" docs and issue #26.
pub(crate) const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// TCP keepalive interval for the custom [`reqwest`] transport.
/// Detects dead-but-not-closed sessions in seconds rather than the
/// 2-hour Linux default. See module-level "HTTP transport tuning"
/// docs and issue #26.
pub(crate) const TCP_KEEPALIVE: Duration = Duration::from_secs(30);

/// Bound on a fresh TCP-connect attempt. `reqwest` defaults to no
/// connect timeout, so an unreachable IP would otherwise wait on the
/// OS-level connect timeout (~75 s on Linux defaults). 10 s is
/// comfortable for an in-region or even cross-region handshake while
/// failing fast on a dead VIP. See module-level "HTTP transport
/// tuning" docs and issue #26.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-read timeout for the custom [`reqwest`] transport. Resets after
/// each successful read, so it caps how long a stuck connection can
/// hold a transfer without limiting total body size. Sized to match
/// [`POOL_IDLE_TIMEOUT`] / [`TCP_KEEPALIVE`] so a single rotation
/// budget covers all three knobs. See module-level "HTTP transport
/// tuning" docs and issue #26.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Production [`ObjectStore`] backed by `azure_storage_blob`.
pub struct AzureStore {
    container: BlobContainerClient,
}

impl std::fmt::Debug for AzureStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `BlobContainerClient` is opaque (private fields, no `Debug`);
        // surface the endpoint instead so error / log lines remain
        // useful.
        f.debug_struct("AzureStore")
            .field("endpoint", &self.container.endpoint().as_str())
            .finish()
    }
}

impl AzureStore {
    /// Build an `AzureStore` from a parsed [`RemoteUrl`].
    ///
    /// Like the S3 backend, the [`RemoteUrl::Azure::prefix`] field is
    /// intentionally **not** consumed here; callers compose it into keys
    /// themselves.
    ///
    /// Marked `async` for symmetry with `S3Store::from_remote_url`,
    /// which awaits the AWS provider chain. The Azure path resolves
    /// credentials synchronously today; the signature stays `async` so
    /// future credential providers (e.g. one that fetches an OIDC
    /// token at construction) can plug in without breaking callers.
    ///
    /// # Errors
    ///
    /// Returns [`ObjectStoreError::Other`] if `url` is not the Azure
    /// variant or if credential resolution fails.
    #[allow(clippy::unused_async)]
    pub async fn from_remote_url(url: &RemoteUrl) -> Result<Self, ObjectStoreError> {
        let RemoteUrl::Azure {
            endpoint,
            account,
            container,
            addressing,
            flags,
            ..
        } = url
        else {
            return Err(ObjectStoreError::Other(
                format!("AzureStore::from_remote_url called with non-Azure URL: {url}").into(),
            ));
        };

        let account_url = build_account_url(endpoint, account, *addressing);
        let resolved = auth::resolve(account, flags)?;

        let client_options = build_client_options(&resolved)?;

        let container_options = BlobContainerClientOptions {
            client_options,
            ..Default::default()
        };

        let container = BlobContainerClient::new(
            &account_url,
            container,
            resolved.token_credential,
            Some(container_options),
        )
        .map_err(other_boxed)?;

        Ok(Self { container })
    }

    /// Construct a [`BlobClient`] for an individual blob.
    fn blob_client(&self, key: &str) -> BlobClient {
        self.container.blob_client(key)
    }

    /// Verify the container is reachable with the configured credentials
    /// by listing one blob (`maxresults=1`) and consuming only the first
    /// page of results. Used by [`crate::protocol::backend::build`] to
    /// fold credential / missing-container / authorization failures into
    /// categorical [`crate::protocol::backend::BackendError`] variants
    /// before the helper REPL runs its first command. Counterpart to
    /// [`crate::object_store::s3::S3Store::probe`].
    pub(crate) async fn probe(&self, prefix: &str) -> Result<(), ObjectStoreError> {
        // Pass `None` for an empty prefix per the same Azurite quirk
        // documented at the top of `list` above: a signed empty prefix
        // returns 403 from Azurite.
        let prefix_opt = (!prefix.is_empty()).then(|| prefix.to_owned());
        let opts = BlobContainerClientListBlobsOptions {
            prefix: prefix_opt,
            maxresults: Some(1),
            ..Default::default()
        };
        let mut pages = self
            .container
            .list_blobs(Some(opts))
            .map_err(|e| classify(e, prefix))?
            .into_pages();
        // Consume only the first page: probing does not need the full
        // listing — we only care that the request succeeded.
        if let Some(page_result) = pages.next().await {
            page_result.map_err(|e| classify(e, prefix))?;
        }
        Ok(())
    }
}

/// Build the [`reqwest::Client`] used by [`AzureStore`]'s custom
/// [`Transport`].
///
/// Bounds the connection pool's idle window, enables TCP keepalive,
/// and sets connect / per-read timeouts so a rotated VIP cannot wedge
/// a long-running session (see [`POOL_IDLE_TIMEOUT`] / [`TCP_KEEPALIVE`]
/// / [`CONNECT_TIMEOUT`] / [`READ_TIMEOUT`] for rationale). Returns
/// [`ObjectStoreError::Other`] if the TLS / DNS resolver layer fails
/// to initialise, which the SDK would otherwise surface as a cryptic
/// per-request error.
pub(crate) fn build_http_client() -> Result<Arc<reqwest::Client>, ObjectStoreError> {
    reqwest::Client::builder()
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .map(Arc::new)
        .map_err(other_boxed)
}

/// Build the [`ClientOptions`] [`AzureStore`] hands to the SDK.
///
/// Installs the custom [`Transport`] (see [`build_http_client`]) and
/// preserves the credential resolver's per-try signing policy. The
/// helper is split out (rather than inlined into [`AzureStore::from_remote_url`])
/// so unit tests can assert that both invariants hold without
/// constructing a real `BlobContainerClient`.
pub(crate) fn build_client_options(
    resolved: &auth::ResolvedCredentials,
) -> Result<ClientOptions, ObjectStoreError> {
    let mut opts = ClientOptions {
        transport: Some(Transport::new(build_http_client()?)),
        ..Default::default()
    };
    if let Some(policy) = &resolved.per_try_policy {
        opts.per_try_policies.push(Arc::clone(policy));
    }
    Ok(opts)
}

/// Construct the account-level endpoint URL the SDK constructors expect.
///
/// The SDK takes a separate `container_name` argument, so we strip the
/// container (and any prefix segments) from the parsed URL. For
/// virtual-hosted addressing the path becomes `/`; for path-style
/// addressing (Azurite, custom endpoints) the path becomes `/<account>`.
pub(crate) fn build_account_url(
    endpoint: &Url,
    account: &str,
    addressing: AzureAddressing,
) -> String {
    let mut rewritten = endpoint.clone();
    rewritten.set_query(None);
    rewritten.set_fragment(None);
    let path = match addressing {
        AzureAddressing::VirtualHosted => "/".to_owned(),
        AzureAddressing::PathStyle => format!("/{account}"),
    };
    rewritten.set_path(&path);
    rewritten.to_string()
}

/// Map an [`azure_core::Error`] into the trait's [`ObjectStoreError`] enum.
///
/// `key` is the operation's key/prefix context; it appears in the
/// resulting [`ObjectStoreError::NotFound`] / [`ObjectStoreError::AccessDenied`] /
/// [`ObjectStoreError::PreconditionFailed`] / [`ObjectStoreError::Conflict`] payload.
fn classify(err: azure_core::Error, key: &str) -> ObjectStoreError {
    if let Some(status) = err.http_status()
        && let Some(mapped) = classify_status(u16::from(status), key)
    {
        return mapped;
    }
    if matches!(err.kind(), azure_core::error::ErrorKind::Io) {
        return network_boxed(err);
    }
    other_boxed(err)
}

/// Pure status-code classifier (key context, no SDK types) so unit
/// tests can exercise every branch without synthesising an SDK error.
fn classify_status(status: u16, key: &str) -> Option<ObjectStoreError> {
    match status {
        404 => Some(ObjectStoreError::NotFound(key.to_owned())),
        403 => Some(ObjectStoreError::AccessDenied(key.to_owned())),
        412 => Some(ObjectStoreError::PreconditionFailed(key.to_owned())),
        409 => Some(ObjectStoreError::Conflict(key.to_owned())),
        _ => None,
    }
}

/// Convert the relevant `Get Blob Properties` headers into the trait's
/// [`ObjectMeta`].
///
/// Extracted so unit tests can drive the missing-content-length and
/// missing-last-modified guard branches without synthesising a full
/// `BlobClientGetPropertiesResultHeaders` value.
///
/// A missing `Content-Length` is an error rather than silent zero: a
/// 0-byte size is semantically meaningful (lock files are intentionally
/// empty) and downstream `head_then_download` takes a fast path on
/// `size == 0` that writes an empty destination file. Treating "header
/// absent" as 0 would silently produce empty bundles instead of
/// surfacing the malformed response.
fn properties_to_meta(
    key: &str,
    content_length: Option<u64>,
    last_modified: Option<OffsetDateTime>,
    etag: Option<&str>,
) -> Result<ObjectMeta, ObjectStoreError> {
    let size = content_length.ok_or_else(|| {
        ObjectStoreError::Other(
            format!("get_properties on `{key}` returned no content-length").into(),
        )
    })?;
    let last_modified = last_modified.ok_or_else(|| {
        ObjectStoreError::Other(
            format!("get_properties on `{key}` returned no last-modified").into(),
        )
    })?;
    Ok(ObjectMeta {
        key: key.to_owned(),
        size,
        last_modified,
        etag: etag.map(str::to_owned),
    })
}

/// Convert a `BlobItem`-shaped record into the trait's [`ObjectMeta`].
///
/// Extracted so unit tests can drive the missing-field guards without
/// synthesising a full `ListBlobsResponse`.
fn item_to_meta(
    name: Option<&str>,
    content_length: Option<u64>,
    last_modified: Option<OffsetDateTime>,
    etag: Option<&str>,
) -> Result<ObjectMeta, ObjectStoreError> {
    let key = name
        .ok_or_else(|| ObjectStoreError::Other("list_blobs returned a blob without a name".into()))?
        .to_owned();
    let size = content_length.unwrap_or(0);
    let last_modified = last_modified.ok_or_else(|| {
        ObjectStoreError::Other(
            format!("list_blobs returned blob `{key}` without last_modified").into(),
        )
    })?;
    Ok(ObjectMeta {
        key,
        size,
        last_modified,
        etag: etag.map(str::to_owned),
    })
}

#[async_trait::async_trait]
impl ObjectStore for AzureStore {
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, ObjectStoreError> {
        // Pass `None` for an empty prefix: Azure list_blobs URL-encodes
        // `prefix=` and Azurite signs an empty value differently than
        // an absent one (treats it as a tampered query and returns
        // 403). Skipping the parameter is the wire-equivalent of "no
        // prefix filter" anyway.
        let prefix_opt = (!prefix.is_empty()).then(|| prefix.to_owned());
        let opts = BlobContainerClientListBlobsOptions {
            prefix: prefix_opt,
            ..Default::default()
        };
        let mut pages = self
            .container
            .list_blobs(Some(opts))
            .map_err(|e| classify(e, prefix))?
            .into_pages();

        let mut out = Vec::new();
        while let Some(page_result) = pages.next().await {
            let response = page_result.map_err(|e| classify(e, prefix))?;
            let body = response
                .into_body()
                .xml::<azure_storage_blob::models::ListBlobsResponse>()
                .map_err(|e| classify(e, prefix))?;
            for item in body.segment.blob_items {
                let props = item.properties.unwrap_or_default();
                let meta = item_to_meta(
                    item.name.as_deref(),
                    props.content_length,
                    props.last_modified,
                    // Listing omits ETag for parity with S3 (avoid
                    // inflating per-object metadata for callers that
                    // only need a key/size enumeration).
                    None,
                )?;
                out.push(meta);
            }
        }
        Ok(out)
    }

    async fn get_to_file(
        &self,
        key: &str,
        dest: &Path,
        opts: GetOpts,
    ) -> Result<(), ObjectStoreError> {
        let parent = dest.parent().ok_or_else(|| {
            ObjectStoreError::Other(
                format!("destination `{}` has no parent directory", dest.display()).into(),
            )
        })?;

        // Mirror S3: try once, retry once on 412 (the head→GET race).
        // After the second attempt any error — including a repeated
        // 412 — propagates.
        let progress = opts.progress.as_ref();
        match self.head_then_download(key, dest, parent, progress).await {
            Err(ObjectStoreError::PreconditionFailed(_)) => {
                tracing::warn!(key, "blob changed between head and GET; retrying");
                self.head_then_download(key, dest, parent, progress).await
            }
            other => other,
        }
    }

    async fn get_bytes(&self, key: &str) -> Result<Bytes, ObjectStoreError> {
        let blob = self.blob_client(key);
        let result = blob.download(None).await.map_err(|e| classify(e, key))?;
        let bytes = result.body.collect().await.map_err(network_boxed)?;
        Ok(bytes)
    }

    async fn put_bytes(
        &self,
        key: &str,
        body: Bytes,
        opts: PutOpts,
    ) -> Result<(), ObjectStoreError> {
        let blob = self.blob_client(key);
        let upload_opts = upload_options_from(opts);
        blob.upload(bytes_to_request_content(body), Some(upload_opts))
            .await
            .map_err(|e| classify(e, key))?;
        Ok(())
    }

    /// Stream a local file to `key` without buffering its full body.
    ///
    /// Mirrors `S3Store::put_path`'s streaming guarantee (issue #21):
    /// memory usage stays bounded by `parallel × partition_size`
    /// (defaults to 4 × 4 MiB = 16 MiB) regardless of file size — the
    /// SDK runs up to `parallel` block uploads concurrently and each
    /// holds a `partition_size`-sized buffer.
    ///
    /// Implementation: wrap `tokio::fs::File` in
    /// [`FileStream`] so the body is delivered as
    /// `Body::SeekableStream`. `BlockBlobClient::upload` then routes
    /// large bodies through `stage_block` + `commit_block_list` (one
    /// request per partition; up to 50000 blocks per blob, ample for
    /// LFS / bundle sizes), while files smaller than one partition
    /// take a single `Put Blob` round trip — same wire shape as
    /// `put_bytes`. Auth (shared-key / SAS / token) is unchanged
    /// because the per-try signing policy reads `request.body().len()`,
    /// which `SeekableStream` reports faithfully via `len()`.
    async fn put_path(&self, key: &str, src: &Path, opts: PutOpts) -> Result<(), ObjectStoreError> {
        let file = tokio::fs::File::open(src).await.map_err(other_boxed)?;
        // `tokio::fs::File::metadata` is the cheap source of truth for
        // file length; the SDK's `FileStream` knows it internally, but
        // does not expose it. We need it to drive a final progress
        // event after the SDK upload completes (the SDK does block-
        // upload internally without exposing per-block hooks — see
        // module-level docs).
        let body_len = file.metadata().await.map_err(other_boxed)?.len();
        let stream = FileStream::builder(file)
            .build()
            .await
            .map_err(other_boxed)?;
        let body: azure_core::http::Body = stream.into();

        let blob = self.blob_client(key);
        let progress = opts.progress.clone();
        let upload_opts = upload_options_from(opts);
        blob.upload(body.into(), Some(upload_opts))
            .await
            .map_err(|e| classify(e, key))?;
        if let Some(sink) = progress
            && body_len > 0
        {
            sink.report(body_len);
        }
        Ok(())
    }

    async fn put_if_absent(&self, key: &str, body: Bytes) -> Result<bool, ObjectStoreError> {
        let blob = self.blob_client(key);
        let upload_opts = BlockBlobClientUploadOptions::default().with_if_not_exists();
        let resp = blob
            .upload(bytes_to_request_content(body), Some(upload_opts))
            .await;
        match resp.map_err(|e| classify(e, key)) {
            Ok(_) => Ok(true),
            Err(ObjectStoreError::PreconditionFailed(_) | ObjectStoreError::Conflict(_)) => {
                Ok(false)
            }
            Err(other) => Err(other),
        }
    }

    async fn head(&self, key: &str) -> Result<ObjectMeta, ObjectStoreError> {
        let blob = self.blob_client(key);
        let resp = blob
            .get_properties(None::<BlobClientGetPropertiesOptions<'_>>)
            .await
            .map_err(|e| classify(e, key))?;
        let headers = resp.headers();
        properties_to_meta(
            key,
            header_u64(headers, &HeaderName::from_static("content-length")),
            header_http_date(headers, &HeaderName::from_static("last-modified")),
            headers.get_optional_str(&HeaderName::from_static("etag")),
        )
    }

    async fn copy(&self, src: &str, dst: &str) -> Result<(), ObjectStoreError> {
        // Server-side copy via `Put Blob From URL` requires a SAS-tokened
        // source URL or `x-ms-copy-source-authorization`, neither of
        // which integrates with our credential model in a clean way
        // for the SDK 0.12 surface. Stream `src` to a temp file via
        // `get_to_file` (chunked download, no body buffer), then
        // `put_path` it back to `dst` (block-uploaded for large
        // bodies). Memory stays bounded by the SDK's partition size
        // regardless of blob size — necessary because
        // `manage doctor`'s duplicate-bundle quarantine path uses
        // `copy()` and bundles can be multi-GiB.
        let temp = NamedTempFile::new().map_err(other_boxed)?;
        // `get_to_file` propagates `NotFound(src)` if the source is
        // absent — exactly the trait contract for `copy`.
        self.get_to_file(src, temp.path(), GetOpts::default())
            .await?;
        // A NotFound on the upload is destination-side — re-shape it
        // so callers don't mistake it for "src absent".
        match self.put_path(dst, temp.path(), PutOpts::default()).await {
            Ok(()) => Ok(()),
            Err(ObjectStoreError::NotFound(_)) => Err(ObjectStoreError::Other(
                format!("copy `{src}` → `{dst}`: upload returned NotFound").into(),
            )),
            Err(other) => Err(other),
        }
    }

    async fn delete(&self, key: &str) -> Result<(), ObjectStoreError> {
        let blob = self.blob_client(key);
        blob.delete(None::<BlobClientDeleteOptions<'_>>)
            .await
            .map_err(|e| classify(e, key))?;
        Ok(())
    }
}

impl AzureStore {
    /// One head→tempfile→download→persist round trip.
    ///
    /// Factored out so [`get_to_file`](ObjectStore::get_to_file) can
    /// invoke it twice: once normally, once more on a 412 retry.
    async fn head_then_download(
        &self,
        key: &str,
        dest: &Path,
        parent: &Path,
        progress: Option<&ProgressSink>,
    ) -> Result<(), ObjectStoreError> {
        let meta = self.head(key).await?;
        let temp = NamedTempFile::new_in(parent).map_err(other_boxed)?;
        if meta.size == 0 {
            // Skip the GET entirely for zero-byte blobs (lock files):
            // `download_streaming` would issue a plain GET for an empty
            // body — correct but a wasted round trip.
            return persist_temp(temp, dest);
        }
        self.download_streaming(key, temp.path(), meta.etag.as_deref(), progress)
            .await?;
        persist_temp(temp, dest)
    }

    /// Stream a blob body to `temp_path` with optional `If-Match`
    /// guarding against mid-download mutation. When `progress` is
    /// `Some`, fires once per SDK body chunk read off the wire.
    async fn download_streaming(
        &self,
        key: &str,
        temp_path: &Path,
        etag: Option<&str>,
        progress: Option<&ProgressSink>,
    ) -> Result<(), ObjectStoreError> {
        let blob = self.blob_client(key);
        let mut opts = BlobClientDownloadOptions::default();
        if let Some(etag) = etag {
            opts.if_match = Some(etag.to_owned());
        }
        let mut result = blob
            .download(Some(opts))
            .await
            .map_err(|e| classify(e, key))?;

        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(temp_path)
            .await
            .map_err(other_boxed)?;

        while let Some(chunk) = result.body.next().await {
            let bytes = chunk.map_err(network_boxed)?;
            let chunk_len = bytes.len() as u64;
            file.write_all(&bytes).await.map_err(other_boxed)?;
            if let Some(sink) = progress
                && chunk_len > 0
            {
                sink.report(chunk_len);
            }
        }
        file.flush().await.map_err(other_boxed)?;
        Ok(())
    }
}

/// Wrap `Bytes` in a `RequestContent` without copying the buffer.
///
/// `RequestContent` has an inherent `from(Vec<u8>)` constructor that
/// shadows the generic `From<Bytes>` trait impl, so a bare
/// `RequestContent::from(body)` resolves to the `Vec<u8>` overload and
/// re-allocates. Going through `Into` instead picks up the trait impl
/// and keeps the `Bytes` payload zero-copy. The return type is left
/// generic so the call site (which pins `Bytes` + `NoFormat` via the
/// `BlobClient::upload` signature) drives type inference.
fn bytes_to_request_content<F>(body: Bytes) -> RequestContent<Bytes, F>
where
    Bytes: Into<RequestContent<Bytes, F>>,
{
    body.into()
}

/// Build a [`BlockBlobClientUploadOptions`] from the trait's [`PutOpts`].
fn upload_options_from(opts: PutOpts) -> BlockBlobClientUploadOptions<'static> {
    let mut out = BlockBlobClientUploadOptions::default();
    if let Some(cd) = opts.content_disposition {
        out.blob_content_disposition = Some(cd);
    }
    if !opts.user_metadata.is_empty() {
        let mut map = std::collections::HashMap::with_capacity(opts.user_metadata.len());
        for (k, v) in opts.user_metadata {
            map.insert(k, v);
        }
        out.metadata = Some(map);
    }
    out
}

fn header_u64(headers: &Headers, name: &HeaderName) -> Option<u64> {
    headers.get_optional_str(name).and_then(|s| s.parse().ok())
}

fn header_http_date(headers: &Headers, name: &HeaderName) -> Option<OffsetDateTime> {
    let raw = headers.get_optional_str(name)?;
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc2822).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url::{AzureAddressing, RemoteFlags};

    fn parse_endpoint(s: &str) -> Url {
        Url::parse(s).expect("test endpoint URL parses")
    }

    fn s3_url() -> RemoteUrl {
        RemoteUrl::S3 {
            endpoint: parse_endpoint("https://my-bucket.s3.us-west-2.amazonaws.com/"),
            bucket: "my-bucket".to_owned(),
            prefix: None,
            addressing: crate::url::S3Addressing::VirtualHosted,
            flags: RemoteFlags::default(),
        }
    }

    // --- build_account_url --------------------------------------------

    #[test]
    fn build_account_url_virtual_hosted_strips_path() {
        let url = parse_endpoint("https://acct.blob.core.windows.net/my-container/some/prefix");
        let out = build_account_url(&url, "acct", AzureAddressing::VirtualHosted);
        assert_eq!(out, "https://acct.blob.core.windows.net/");
    }

    #[test]
    fn build_account_url_path_style_keeps_account() {
        let url = parse_endpoint("http://127.0.0.1:10000/devstoreaccount1/my-container/repo");
        let out = build_account_url(&url, "devstoreaccount1", AzureAddressing::PathStyle);
        assert_eq!(out, "http://127.0.0.1:10000/devstoreaccount1");
    }

    #[test]
    fn build_account_url_strips_query_and_fragment() {
        let url = parse_endpoint("https://acct.blob.core.windows.net/c/r?credential=foo#frag");
        let out = build_account_url(&url, "acct", AzureAddressing::VirtualHosted);
        assert_eq!(out, "https://acct.blob.core.windows.net/");
    }

    // --- classify_status ----------------------------------------------

    #[test]
    fn classify_404_is_not_found() {
        assert!(matches!(
            classify_status(404, "k"),
            Some(ObjectStoreError::NotFound(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_403_is_access_denied() {
        assert!(matches!(
            classify_status(403, "k"),
            Some(ObjectStoreError::AccessDenied(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_412_is_precondition_failed() {
        assert!(matches!(
            classify_status(412, "k"),
            Some(ObjectStoreError::PreconditionFailed(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_409_is_conflict() {
        // 409 covers Azure's `BlobAlreadyExists` (the put-if-absent
        // contention path). Without this branch, `put_if_absent` would
        // surface contention as a hard error instead of `Ok(false)`.
        assert!(matches!(
            classify_status(409, "k"),
            Some(ObjectStoreError::Conflict(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_unrecognised_status_returns_none() {
        assert!(classify_status(500, "k").is_none());
        assert!(classify_status(429, "k").is_none());
    }

    // --- properties_to_meta ------------------------------------------

    #[test]
    fn properties_to_meta_round_trips_well_formed_response() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let meta = properties_to_meta("k", Some(42), Some(now), Some("\"abc\""))
            .expect("conversion succeeds");
        assert_eq!(meta.key, "k");
        assert_eq!(meta.size, 42);
        assert_eq!(meta.last_modified.unix_timestamp(), 1_700_000_000);
        assert_eq!(meta.etag.as_deref(), Some("\"abc\""));
    }

    #[test]
    fn properties_to_meta_preserves_legitimate_zero_size() {
        // Zero-byte lock files are legitimate; a present
        // `Content-Length: 0` header (`Some(0)`) must round-trip as
        // `size == 0`, distinct from the missing-header error.
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let meta =
            properties_to_meta("LOCK", Some(0), Some(now), None).expect("conversion succeeds");
        assert_eq!(meta.size, 0);
    }

    #[test]
    fn properties_to_meta_rejects_missing_content_length() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let err = properties_to_meta("k", None, Some(now), None)
            .expect_err("missing content-length must error");
        match err {
            ObjectStoreError::Other(inner) => {
                let msg = inner.to_string();
                assert!(msg.contains("no content-length"), "names failure: {msg}");
                assert!(msg.contains("`k`"), "includes the key for context: {msg}");
            }
            other => {
                panic!("expected ObjectStoreError::Other for missing content-length, got {other:?}")
            }
        }
    }

    #[test]
    fn properties_to_meta_rejects_missing_last_modified() {
        let err = properties_to_meta("k", Some(0), None, None)
            .expect_err("missing last_modified must error");
        match err {
            ObjectStoreError::Other(inner) => {
                let msg = inner.to_string();
                assert!(msg.contains("no last-modified"), "names failure: {msg}");
                assert!(msg.contains("`k`"), "includes the key for context: {msg}");
            }
            other => {
                panic!("expected ObjectStoreError::Other for missing last_modified, got {other:?}")
            }
        }
    }

    // --- item_to_meta -------------------------------------------------

    #[test]
    fn item_to_meta_round_trips_well_formed_item() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let meta = item_to_meta(Some("k"), Some(42), Some(now), Some("\"abc\"")).unwrap();
        assert_eq!(meta.key, "k");
        assert_eq!(meta.size, 42);
        assert_eq!(meta.last_modified.unix_timestamp(), 1_700_000_000);
        assert_eq!(meta.etag.as_deref(), Some("\"abc\""));
    }

    #[test]
    fn item_to_meta_rejects_missing_name() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let err = item_to_meta(None, Some(0), Some(now), None).unwrap_err();
        match err {
            ObjectStoreError::Other(inner) => {
                assert!(
                    inner.to_string().contains("without a name"),
                    "names failure: {inner}"
                );
            }
            other => panic!("expected ObjectStoreError::Other, got {other:?}"),
        }
    }

    #[test]
    fn item_to_meta_rejects_missing_last_modified() {
        let err = item_to_meta(Some("k"), Some(0), None, None).unwrap_err();
        match err {
            ObjectStoreError::Other(inner) => {
                let msg = inner.to_string();
                assert!(
                    msg.contains("without last_modified"),
                    "names failure: {msg}"
                );
                assert!(msg.contains("`k`"), "includes the key: {msg}");
            }
            other => panic!("expected ObjectStoreError::Other, got {other:?}"),
        }
    }

    #[test]
    fn item_to_meta_treats_missing_size_as_zero() {
        // The Azure SDK types content_length as Option<u64>; missing
        // values default to 0 (rather than `None` propagating through
        // every caller's arithmetic).
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let meta = item_to_meta(Some("k"), None, Some(now), None).unwrap();
        assert_eq!(meta.size, 0);
    }

    // --- upload_options_from ------------------------------------------

    #[test]
    fn upload_options_from_default_is_empty() {
        let out = upload_options_from(PutOpts::default());
        assert!(out.blob_content_disposition.is_none());
        assert!(out.metadata.is_none());
    }

    #[test]
    fn upload_options_from_carries_content_disposition() {
        let opts = PutOpts {
            content_disposition: Some("attachment; filename=x".into()),
            user_metadata: Vec::new(),
            progress: None,
        };
        let out = upload_options_from(opts);
        let cd: String = out
            .blob_content_disposition
            .expect("content_disposition should be set");
        assert!(cd.contains("attachment"));
    }

    #[test]
    fn upload_options_from_collects_metadata() {
        let opts = PutOpts {
            content_disposition: None,
            user_metadata: vec![("x-foo".into(), "1".into()), ("x-bar".into(), "2".into())],
            progress: None,
        };
        let out = upload_options_from(opts);
        let map = out.metadata.expect("metadata set");
        assert_eq!(map.get("x-foo").map(String::as_str), Some("1"));
        assert_eq!(map.get("x-bar").map(String::as_str), Some("2"));
    }

    // --- from_remote_url constructor branch ---------------------------

    #[tokio::test]
    async fn from_remote_url_rejects_s3() {
        let result = AzureStore::from_remote_url(&s3_url()).await;
        match result {
            Err(ObjectStoreError::Other(_)) => {}
            Err(other) => panic!("expected ObjectStoreError::Other, got {other:?}"),
            Ok(_) => panic!("expected S3 URL to be rejected"),
        }
    }

    // --- HTTP transport tuning (#26 / #28) ----------------------------

    /// Pin the timeout values. A future copy-paste mistake (`from_millis`
    /// instead of `from_secs`, an accidental zero) silently disables
    /// the very behaviour these constants exist for; fail fast instead.
    /// If the constants are deliberately changed, update the expected
    /// values on the right-hand side together — the test exists to make
    /// such changes deliberate, not to lock the values forever.
    #[test]
    fn transport_timeout_constants_have_expected_values() {
        assert_eq!(POOL_IDLE_TIMEOUT, Duration::from_secs(30));
        assert_eq!(TCP_KEEPALIVE, Duration::from_secs(30));
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(READ_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn build_http_client_succeeds() {
        build_http_client().expect("reqwest client builds with the configured timeouts");
    }

    /// The meaningful regression check: if a future refactor drops the
    /// `transport = Some(...)` line in `build_client_options`, the
    /// Azure backend silently reverts to `azure_core`'s default
    /// (unbounded) HTTP transport. This test fails when that happens.
    /// Also pins the empty-policies invariant on the no-credential
    /// branch, so a refactor that injects a fallback policy when
    /// `per_try_policy` is `None` is caught.
    #[test]
    fn build_client_options_installs_custom_transport() {
        let resolved = auth::ResolvedCredentials {
            token_credential: None,
            per_try_policy: None,
        };
        let opts = build_client_options(&resolved).expect("client options build");
        assert!(
            opts.transport.is_some(),
            "ClientOptions::transport must be Some so the SDK uses our \
             pool_idle_timeout / tcp_keepalive client (issue #26)",
        );
        assert!(
            opts.per_try_policies.is_empty(),
            "no per-try policy was supplied; the helper must not inject \
             a fallback signer of its own",
        );
    }

    /// Issue #28's Notes section explicitly calls out: the per-try
    /// signing policy must continue to fire after we install a custom
    /// transport. The SDK pipeline runs them independently of the
    /// transport, but a future refactor that confuses the two fields
    /// would silently drop signing — surface that here. The
    /// [`Arc::ptr_eq`] check pins identity so a refactor that
    /// silently *replaces* the caller's policy with a fresh one
    /// (rather than dropping it outright) also fails.
    #[test]
    fn build_client_options_preserves_per_try_policy() {
        // Azurite's published well-known account key — base64-valid
        // and safe to embed.
        const AZURITE_KEY: &str = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
        let policy: Arc<dyn azure_core::http::policies::Policy> = Arc::new(
            auth::SharedKeySigningPolicy::new("devstoreaccount1", AZURITE_KEY)
                .expect("shared-key policy constructs"),
        );
        let resolved = auth::ResolvedCredentials {
            token_credential: None,
            per_try_policy: Some(Arc::clone(&policy)),
        };
        let opts = build_client_options(&resolved).expect("client options build");
        assert!(opts.transport.is_some(), "transport still wired");
        assert_eq!(
            opts.per_try_policies.len(),
            1,
            "exactly one per-try policy is wired",
        );
        assert!(
            Arc::ptr_eq(&policy, &opts.per_try_policies[0]),
            "the policy at index 0 must be the same Arc the caller \
             supplied — not a fresh policy constructed inside the helper",
        );
    }
}

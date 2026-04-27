//! Azure Blob Storage backend for the [`ObjectStore`][super::ObjectStore]
//! trait (Phase 11 of `execution-plan.md`).
//!
//! `AzureBlobStore` wraps `azure_storage_blob`. Like the S3 backend, this
//! module owns the URL → SDK config translation, the error-code
//! classifier ([`classify`]), and the credential resolution plumbing.
//! Unlike S3, the SDK already does parallel range downloads inside
//! `BlobClient::download()`, so there is no hand-rolled multipart
//! orchestrator (asymmetric with S3 by design — see
//! `execution-plan.md` §5.3 / §6).
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
//! `Ok(false)` per `execution-plan.md` §5.1.
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
//! implement `copy` as a download-then-upload round trip. The trait's
//! only consumer is the per-ref locking algorithm (§5.2 of the plan)
//! which copies zero-byte lock files, so the round trip is fast in
//! practice. Body is preserved; user metadata is not propagated for
//! parity with the upstream `git-remote-s3` Python lock-copy path
//! which similarly only carries body bytes.
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
//! ## Stdout discipline
//!
//! Per `.claude/rules/protocol-stdout.md`, this module never writes to
//! stdout. Diagnostics go through `tracing` (which the helper binaries
//! configure to write to stderr).

pub mod auth;

use std::path::Path;

use azure_core::http::ClientOptions;
use azure_core::http::headers::{HeaderName, Headers};
use azure_core::http::request::RequestContent;
use azure_storage_blob::clients::{BlobClient, BlobContainerClient, BlobContainerClientOptions};
use azure_storage_blob::models::method_options::BlockBlobClientUploadOptions;
use azure_storage_blob::models::{
    BlobClientDeleteOptions, BlobClientDownloadOptions, BlobClientGetPropertiesOptions,
    BlobContainerClientListBlobsOptions,
};
use bytes::Bytes;
use futures::StreamExt;
use tempfile::NamedTempFile;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;
use url::Url;

use crate::url::{AzureAddressing, RemoteUrl};

use super::error::other_boxed;
use super::{Error, ObjectMeta, ObjectStore, PutOpts, persist_temp};

/// Production [`ObjectStore`] backed by `azure_storage_blob`.
pub struct AzureBlobStore {
    container: BlobContainerClient,
}

impl std::fmt::Debug for AzureBlobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `BlobContainerClient` is opaque (private fields, no `Debug`);
        // surface the endpoint instead so error / log lines remain
        // useful.
        f.debug_struct("AzureBlobStore")
            .field("endpoint", &self.container.endpoint().as_str())
            .finish()
    }
}

impl AzureBlobStore {
    /// Build an `AzureBlobStore` from a parsed [`RemoteUrl`].
    ///
    /// Returns `Err(Error::Other)` if `url` is not the Azure variant or
    /// if credential resolution fails. Like the S3 backend, the
    /// [`RemoteUrl::Azure::prefix`] field is intentionally **not**
    /// consumed here; callers compose it into keys themselves.
    ///
    /// Marked `async` for symmetry with `S3Store::from_remote_url`,
    /// which awaits the AWS provider chain. The Azure path resolves
    /// credentials synchronously today; the signature stays `async` so
    /// future credential providers (e.g. one that fetches an OIDC
    /// token at construction) can plug in without breaking callers.
    #[allow(clippy::unused_async)]
    pub async fn from_remote_url(url: &RemoteUrl) -> Result<Self, Error> {
        let RemoteUrl::Azure {
            endpoint,
            account,
            container,
            addressing,
            flags,
            ..
        } = url
        else {
            return Err(Error::Other(
                format!("AzureBlobStore::from_remote_url called with non-Azure URL: {url}").into(),
            ));
        };

        let account_url = build_account_url(endpoint, account, *addressing);
        let resolved = auth::resolve(account, flags)?;

        let mut client_options = ClientOptions::default();
        if let Some(policy) = resolved.per_try_policy {
            client_options.per_try_policies.push(policy);
        }

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
}

/// Construct the account-level endpoint URL the SDK constructors expect.
///
/// The SDK takes a separate `container_name` argument, so we strip the
/// container (and any prefix segments) from the parsed URL. For
/// subdomain addressing the path becomes `/`; for path-style addressing
/// (Azurite, custom endpoints) the path becomes `/<account>`.
pub(crate) fn build_account_url(
    endpoint: &Url,
    account: &str,
    addressing: AzureAddressing,
) -> String {
    let mut rewritten = endpoint.clone();
    rewritten.set_query(None);
    rewritten.set_fragment(None);
    let path = match addressing {
        AzureAddressing::Subdomain => "/".to_owned(),
        AzureAddressing::PathStyle => format!("/{account}"),
    };
    rewritten.set_path(&path);
    rewritten.to_string()
}

/// Map an [`azure_core::Error`] into the trait's [`Error`] enum.
///
/// `key` is the operation's key/prefix context; it appears in the
/// resulting [`Error::NotFound`] / [`Error::AccessDenied`] /
/// [`Error::PreconditionFailed`] / [`Error::Conflict`] payload.
fn classify(err: azure_core::Error, key: &str) -> Error {
    if let Some(status) = err.http_status()
        && let Some(mapped) = classify_status(u16::from(status), key)
    {
        return mapped;
    }
    if matches!(err.kind(), azure_core::error::ErrorKind::Io) {
        return Error::Network(Box::new(err));
    }
    Error::Other(Box::new(err))
}

/// Pure status-code classifier (key context, no SDK types) so unit
/// tests can exercise every branch without synthesising an SDK error.
fn classify_status(status: u16, key: &str) -> Option<Error> {
    match status {
        404 => Some(Error::NotFound(key.to_owned())),
        403 => Some(Error::AccessDenied(key.to_owned())),
        412 => Some(Error::PreconditionFailed(key.to_owned())),
        409 => Some(Error::Conflict(key.to_owned())),
        _ => None,
    }
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
) -> Result<ObjectMeta, Error> {
    let key = name
        .ok_or_else(|| Error::Other("list_blobs returned a blob without a name".into()))?
        .to_owned();
    let size = content_length.unwrap_or(0);
    let last_modified = last_modified.ok_or_else(|| {
        Error::Other(format!("list_blobs returned blob `{key}` without last_modified").into())
    })?;
    Ok(ObjectMeta {
        key,
        size,
        last_modified,
        etag: etag.map(str::to_owned),
    })
}

#[async_trait::async_trait]
impl ObjectStore for AzureBlobStore {
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, Error> {
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

    async fn get_to_file(&self, key: &str, dest: &Path) -> Result<(), Error> {
        let parent = dest.parent().ok_or_else(|| {
            Error::Other(format!("destination `{}` has no parent directory", dest.display()).into())
        })?;

        // Mirror S3: try once, retry once on 412 (the head→GET race).
        // After the second attempt any error — including a repeated
        // 412 — propagates.
        match self.head_then_download(key, dest, parent).await {
            Err(Error::PreconditionFailed(_)) => {
                tracing::warn!(key, "blob changed between head and GET; retrying");
                self.head_then_download(key, dest, parent).await
            }
            other => other,
        }
    }

    async fn get_bytes(&self, key: &str) -> Result<Bytes, Error> {
        let blob = self.blob_client(key);
        let result = blob.download(None).await.map_err(|e| classify(e, key))?;
        let bytes = result
            .body
            .collect()
            .await
            .map_err(|e| Error::Network(Box::new(e)))?;
        Ok(bytes)
    }

    async fn put_bytes(&self, key: &str, body: Bytes, opts: PutOpts) -> Result<(), Error> {
        let blob = self.blob_client(key);
        let upload_opts = upload_options_from(opts);
        blob.upload(bytes_to_request_content(body), Some(upload_opts))
            .await
            .map_err(|e| classify(e, key))?;
        Ok(())
    }

    async fn put_if_absent(&self, key: &str, body: Bytes) -> Result<bool, Error> {
        let blob = self.blob_client(key);
        let upload_opts = BlockBlobClientUploadOptions::default().with_if_not_exists();
        let resp = blob
            .upload(bytes_to_request_content(body), Some(upload_opts))
            .await;
        match resp {
            Ok(_) => Ok(true),
            Err(e) => match classify(e, key) {
                Error::PreconditionFailed(_) | Error::Conflict(_) => Ok(false),
                other => Err(other),
            },
        }
    }

    async fn head(&self, key: &str) -> Result<ObjectMeta, Error> {
        let blob = self.blob_client(key);
        let resp = blob
            .get_properties(None::<BlobClientGetPropertiesOptions<'_>>)
            .await
            .map_err(|e| classify(e, key))?;
        let headers = resp.headers();
        let size = header_u64(headers, &HeaderName::from_static("content-length")).unwrap_or(0);
        let last_modified = header_http_date(headers, &HeaderName::from_static("last-modified"))
            .ok_or_else(|| {
                Error::Other(format!("get_properties on `{key}` returned no last-modified").into())
            })?;
        let etag = headers
            .get_optional_str(&HeaderName::from_static("etag"))
            .map(str::to_owned);
        Ok(ObjectMeta {
            key: key.to_owned(),
            size,
            last_modified,
            etag,
        })
    }

    async fn copy(&self, src: &str, dst: &str) -> Result<(), Error> {
        // Server-side copy via PutBlobFromURL requires a SAS-tokened
        // source URL or `x-ms-copy-source-authorization`, neither of
        // which integrates with our credential model in a clean way
        // for the SDK 0.12 surface. Use a download-then-upload round
        // trip; lock files (zero bytes) round-trip in <1 RTT and
        // bundles fit comfortably under the single-PUT 256 MiB ceiling.
        let body = self.get_bytes(src).await?;
        // A NotFound on the upload is destination-side — re-shape it
        // so callers don't mistake it for "src absent".
        match self.put_bytes(dst, body, PutOpts::default()).await {
            Ok(()) => Ok(()),
            Err(Error::NotFound(_)) => Err(Error::Other(
                format!("copy `{src}` → `{dst}`: upload returned NotFound").into(),
            )),
            Err(other) => Err(other),
        }
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        let blob = self.blob_client(key);
        blob.delete(None::<BlobClientDeleteOptions<'_>>)
            .await
            .map_err(|e| classify(e, key))?;
        Ok(())
    }
}

impl AzureBlobStore {
    /// One head→tempfile→download→persist round trip.
    ///
    /// Factored out so [`get_to_file`](ObjectStore::get_to_file) can
    /// invoke it twice: once normally, once more on a 412 retry.
    async fn head_then_download(&self, key: &str, dest: &Path, parent: &Path) -> Result<(), Error> {
        let meta = self.head(key).await?;
        let temp = NamedTempFile::new_in(parent).map_err(other_boxed)?;
        if meta.size == 0 {
            // Skip the GET entirely for zero-byte blobs (lock files).
            // Range fetches against an empty blob return 416.
            return persist_temp(temp, dest);
        }
        self.download_streaming(key, temp.path(), meta.etag.as_deref())
            .await?;
        persist_temp(temp, dest)
    }

    /// Stream a blob body to `temp_path` with optional `If-Match`
    /// guarding against mid-download mutation.
    async fn download_streaming(
        &self,
        key: &str,
        temp_path: &Path,
        etag: Option<&str>,
    ) -> Result<(), Error> {
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
            let bytes = chunk.map_err(|e| Error::Network(Box::new(e)))?;
            file.write_all(&bytes).await.map_err(other_boxed)?;
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
    fn build_account_url_subdomain_strips_path() {
        let url = parse_endpoint("https://acct.blob.core.windows.net/my-container/some/prefix");
        let out = build_account_url(&url, "acct", AzureAddressing::Subdomain);
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
        let out = build_account_url(&url, "acct", AzureAddressing::Subdomain);
        assert_eq!(out, "https://acct.blob.core.windows.net/");
    }

    // --- classify_status ----------------------------------------------

    #[test]
    fn classify_404_is_not_found() {
        assert!(matches!(
            classify_status(404, "k"),
            Some(Error::NotFound(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_403_is_access_denied() {
        assert!(matches!(
            classify_status(403, "k"),
            Some(Error::AccessDenied(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_412_is_precondition_failed() {
        assert!(matches!(
            classify_status(412, "k"),
            Some(Error::PreconditionFailed(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_409_is_conflict() {
        // 409 covers Azure's `BlobAlreadyExists` (the put-if-absent
        // contention path). Without this branch, `put_if_absent` would
        // surface contention as a hard error instead of `Ok(false)`.
        assert!(matches!(
            classify_status(409, "k"),
            Some(Error::Conflict(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_unrecognised_status_returns_none() {
        assert!(classify_status(500, "k").is_none());
        assert!(classify_status(429, "k").is_none());
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
            Error::Other(inner) => {
                assert!(
                    inner.to_string().contains("without a name"),
                    "names failure: {inner}"
                );
            }
            other => panic!("expected Error::Other, got {other:?}"),
        }
    }

    #[test]
    fn item_to_meta_rejects_missing_last_modified() {
        let err = item_to_meta(Some("k"), Some(0), None, None).unwrap_err();
        match err {
            Error::Other(inner) => {
                let msg = inner.to_string();
                assert!(
                    msg.contains("without last_modified"),
                    "names failure: {msg}"
                );
                assert!(msg.contains("`k`"), "includes the key: {msg}");
            }
            other => panic!("expected Error::Other, got {other:?}"),
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
        };
        let out = upload_options_from(opts);
        let map = out.metadata.expect("metadata set");
        assert_eq!(map.get("x-foo").map(String::as_str), Some("1"));
        assert_eq!(map.get("x-bar").map(String::as_str), Some("2"));
    }

    // --- from_remote_url constructor branch ---------------------------

    #[tokio::test]
    async fn from_remote_url_rejects_s3() {
        let result = AzureBlobStore::from_remote_url(&s3_url()).await;
        match result {
            Err(Error::Other(_)) => {}
            Err(other) => panic!("expected Error::Other, got {other:?}"),
            Ok(_) => panic!("expected S3 URL to be rejected"),
        }
    }
}

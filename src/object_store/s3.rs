//! S3 backend for the [`ObjectStore`][super::ObjectStore] trait.
//!
//! `S3Store` wraps `aws-sdk-s3`. The SDK owns `SigV4`, retries, connection
//! pooling, and timeout policy; this module owns the URL → SDK config
//! translation, the error-code classifier ([`classify`]), and the
//! hand-rolled multipart download orchestrator that the SDK does not
//! provide. See `execution-plan.md` §5, §5.1, and §5.3.
//!
//! ## Key composition
//!
//! `S3Store` does **not** auto-prepend the [`RemoteUrl`] `prefix`. Trait
//! keys are byte-prefix per the contract on
//! [`ObjectStore::list`][super::ObjectStore::list]
//! (`mod.rs:65-67`); the URL `prefix` is a repository concern and is
//! composed by Phase 6+ callers that build keys like
//! `<prefix>/refs/.../<sha>.bundle`.
//!
//! ## Conditional writes
//!
//! [`put_if_absent`][super::ObjectStore::put_if_absent] uses
//! `If-None-Match: "*"`. S3 returns either 412 (`PreconditionFailed`)
//! when the key already exists or 409 (`ConditionalRequestConflict`)
//! when two PUTs race. Both collapse to `Ok(false)` per
//! `execution-plan.md` §5.1; only 412 is in upstream Python's path.
//!
//! ## Atomic `get_to_file`
//!
//! Both the small-object and multipart download paths write to a sibling
//! [`tempfile::NamedTempFile`] and rename on success so a partial
//! failure cannot leave a corrupt destination for the Phase 7 unbundle
//! step.
//!
//! Every GET carries `If-Match: <etag>` derived from the preceding
//! `HeadObject` call. If the object is overwritten between `head` and
//! the body download, S3 returns 412 and `get_to_file` retries once
//! with a fresh `head`/`ETag`. After one retry the 412 propagates as
//! [`Error::PreconditionFailed`].
//!
//! ## Stdout discipline
//!
//! Per `.claude/rules/protocol-stdout.md`, this module never writes to
//! stdout. Diagnostics go through `tracing` (which the helper binaries
//! configure to write to stderr).

use std::io::SeekFrom;
use std::path::Path;
use std::sync::Arc;

use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use aws_smithy_types_convert::date_time::DateTimeExt;
use bytes::Bytes;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use tempfile::NamedTempFile;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use url::Url;

use crate::url::{RemoteUrl, S3Addressing};

use super::error::other_boxed;
use super::{Error, ObjectMeta, ObjectStore, PutOpts};

/// Object-size cutoff above which `get_to_file` switches from a single
/// streaming GET to parallel ranged GETs. Matches upstream
/// `boto3.s3.transfer.TransferConfig` (`../git-remote-s3/git_remote_s3/remote.py:143-148`).
pub(crate) const MULTIPART_THRESHOLD: u64 = 25 * 1024 * 1024;
/// Range size for each ranged GET in the multipart download path.
pub(crate) const MULTIPART_CHUNK_SIZE: u64 = 16 * 1024 * 1024;
/// Maximum simultaneous in-flight ranged GETs in the multipart download path.
pub(crate) const MULTIPART_MAX_CONCURRENCY: usize = 8;

/// Percent-encode set used for `x-amz-copy-source` keys: every non-
/// alphanumeric ASCII byte except the path-structural and unreserved
/// characters (`/`, `.`, `-`, `_`, `~`).
const COPY_SOURCE_ENCODE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Production [`ObjectStore`] backed by `aws-sdk-s3`.
#[derive(Debug)]
pub struct S3Store {
    client: aws_sdk_s3::Client,
    bucket: String,
}

/// The decisions extracted from a [`RemoteUrl::S3`] before they are
/// fed into the `aws-sdk-s3` config builder. Factored out so unit
/// tests can assert each decision without going through the SDK
/// (whose getters vary across patch releases).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedS3Config {
    pub(crate) endpoint_url: Url,
    pub(crate) region: Option<String>,
    pub(crate) force_path_style: bool,
    pub(crate) profile: Option<String>,
}

impl ResolvedS3Config {
    pub(crate) fn from_url_parts(
        endpoint: &Url,
        addressing: S3Addressing,
        profile: Option<&str>,
        region_flag: Option<&str>,
    ) -> Result<Self, Error> {
        Ok(Self {
            endpoint_url: normalize_endpoint(endpoint, addressing)?,
            region: resolve_region(endpoint, region_flag),
            force_path_style: matches!(addressing, S3Addressing::PathStyle),
            profile: profile.map(str::to_owned),
        })
    }
}

impl S3Store {
    /// Build an `S3Store` from a parsed [`RemoteUrl`].
    ///
    /// Returns `Err(Error::Other)` if `url` is not the S3 variant or if
    /// the endpoint URL cannot be normalised for virtual-hosted
    /// addressing. The [`RemoteUrl::S3::prefix`] field is intentionally
    /// **not** consumed here; callers compose it into keys themselves
    /// per the module-level docs.
    pub async fn from_remote_url(url: &RemoteUrl) -> Result<Self, Error> {
        let RemoteUrl::S3 {
            endpoint,
            bucket,
            addressing,
            flags,
            ..
        } = url
        else {
            return Err(Error::Other(
                format!("S3Store::from_remote_url called with non-S3 URL: {url}").into(),
            ));
        };

        let resolved = ResolvedS3Config::from_url_parts(
            endpoint,
            *addressing,
            flags.profile.as_deref(),
            flags.region.as_deref(),
        )?;
        let sdk_config = build_s3_config(&resolved).await;
        let client = aws_sdk_s3::Client::from_conf(sdk_config);

        Ok(Self {
            client,
            bucket: bucket.clone(),
        })
    }
}

/// Build the `aws-sdk-s3` config from a [`ResolvedS3Config`].
///
/// 1. Load the AWS SDK provider chain with `BehaviorVersion::latest()`.
/// 2. Apply `endpoint_url`, `profile`, `region` from the resolved decisions.
/// 3. Override `force_path_style` on the resulting `aws_sdk_s3::Config`.
pub(crate) async fn build_s3_config(resolved: &ResolvedS3Config) -> aws_sdk_s3::Config {
    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .endpoint_url(resolved.endpoint_url.as_str());
    if let Some(p) = &resolved.profile {
        loader = loader.profile_name(p);
    }
    if let Some(r) = &resolved.region {
        loader = loader.region(Region::new(r.clone()));
    }
    let sdk_config = loader.load().await;

    aws_sdk_s3::config::Builder::from(&sdk_config)
        .force_path_style(resolved.force_path_style)
        .build()
}

/// Rewrite the parsed endpoint URL into the form `aws-sdk-s3` expects
/// as `endpoint_url`: a base of `scheme://host[:port]` with **no path,
/// query, or fragment**, and with any bucket label stripped from the
/// host for virtual-hosted addressing.
///
/// The SDK rejects an `endpoint_url` that carries a query component
/// (e.g. our `?addressing=...` flag) and adds the bucket itself —
/// either as a path segment (`force_path_style(true)`) or as a host
/// subdomain (`force_path_style(false)`) — so we must strip both
/// before handing the URL off.
pub(crate) fn normalize_endpoint(endpoint: &Url, addressing: S3Addressing) -> Result<Url, Error> {
    let mut rewritten = endpoint.clone();
    rewritten.set_path("");
    rewritten.set_query(None);
    rewritten.set_fragment(None);

    if matches!(addressing, S3Addressing::VirtualHosted) {
        let host = rewritten
            .host_str()
            .ok_or_else(|| Error::Other("endpoint URL has no host".into()))?;
        let regional_host = host
            .split_once('.')
            .map(|(_bucket, rest)| rest)
            .ok_or_else(|| {
                Error::Other(
                    format!("virtual-hosted endpoint host `{host}` has no dot separator").into(),
                )
            })?
            .to_owned();
        rewritten
            .set_host(Some(&regional_host))
            .map_err(other_boxed)?;
    }

    Ok(rewritten)
}

/// Resolve the `SigV4` signing region.
///
/// Order: `?region=` flag → AWS hostname pattern → `us-east-1` default
/// for non-AWS hosts → `None` for legacy AWS hosts that don't carry a
/// region segment (the SDK provider chain takes over).
pub(crate) fn resolve_region(endpoint: &Url, flag: Option<&str>) -> Option<String> {
    if let Some(r) = flag {
        return Some(r.to_owned());
    }
    let host = endpoint.host_str()?;
    if !host.ends_with(".amazonaws.com") && host != "amazonaws.com" {
        return Some("us-east-1".to_owned());
    }
    extract_aws_region(host)
}

/// Try to parse the AWS region out of an `*.amazonaws.com` hostname.
fn extract_aws_region(host: &str) -> Option<String> {
    let trimmed = host.strip_suffix(".amazonaws.com")?;
    // Patterns we accept (in priority order):
    //   <bucket>.s3.<region>      → middle is "s3", trailing label is region
    //   s3.<region>               → leading "s3"
    //   s3-<region>               → legacy hyphenated form
    //   s3                        → legacy us-east-1 (no region segment) → None
    let labels: Vec<&str> = trimmed.split('.').collect();
    match labels.as_slice() {
        ["s3"] => None,
        ["s3", region] => Some((*region).to_owned()),
        [_bucket, "s3", region] => Some((*region).to_owned()),
        [head] if head.starts_with("s3-") => Some(head["s3-".len()..].to_owned()),
        _ => None,
    }
}

/// Plan inclusive RFC 7233 byte ranges for a parallel ranged-GET download.
///
/// `size = 0` → empty vec (caller writes a zero-length file directly).
/// Otherwise: full chunks of `chunk_size` bytes, with the final range
/// covering whatever remainder is left (`(N*chunk, size-1)`).
pub(crate) fn plan_ranges(size: u64, chunk_size: u64) -> Vec<(u64, u64)> {
    if size == 0 || chunk_size == 0 {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    let mut start = 0u64;
    while start < size {
        let end = (start + chunk_size - 1).min(size - 1);
        ranges.push((start, end));
        start = end + 1;
    }
    ranges
}

/// Encode a `<bucket>/<key>` pair for the `x-amz-copy-source` header.
///
/// `aws-sdk-s3` 1.x forwards `copy_source` verbatim; we have to encode
/// reserved characters (notably `#` in `LOCK#.lock`) ourselves.
pub(crate) fn encode_copy_source(bucket: &str, key: &str) -> String {
    let bucket_enc = utf8_percent_encode(bucket, COPY_SOURCE_ENCODE);
    let key_enc = utf8_percent_encode(key, COPY_SOURCE_ENCODE);
    format!("{bucket_enc}/{key_enc}")
}

/// Map a typed `aws-sdk-s3` error into the trait's [`Error`] enum.
///
/// `key` is the operation's key/prefix context — it appears in the
/// resulting [`Error::NotFound`] / [`Error::AccessDenied`] /
/// [`Error::PreconditionFailed`] / [`Error::Conflict`] payload.
///
/// Note that this also covers typed `NotFound` / `NoSuchKey` variants
/// the SDK constructs from 404 responses: those carry HTTP 404 on
/// `svc.raw().status()` and so route through the status-based branch
/// of [`classify_status_and_code`].
fn classify<E>(err: SdkError<E>, key: &str) -> Error
where
    E: std::error::Error + Send + Sync + 'static + ProvideErrorMetadata,
{
    if let SdkError::ServiceError(svc) = &err {
        let status = svc.raw().status().as_u16();
        let code = svc.err().code();
        if let Some(mapped) = classify_status_and_code(status, code, key) {
            return mapped;
        }
    }
    match &err {
        SdkError::DispatchFailure(_) | SdkError::TimeoutError(_) => Error::Network(Box::new(err)),
        _ => Error::Other(Box::new(err)),
    }
}

/// Convert a single [`aws_sdk_s3::types::Object`] from a
/// `ListObjectsV2` page into the trait's [`ObjectMeta`].
///
/// Extracted so unit tests can drive the missing-key and
/// missing-last-modified guard branches via `Object`'s builder
/// without synthesising a full `ListObjectsV2Output`.
pub(crate) fn object_to_meta(obj: &aws_sdk_s3::types::Object) -> Result<ObjectMeta, Error> {
    let key = obj
        .key()
        .ok_or_else(|| Error::Other("list_objects_v2 returned an object without a key".into()))?
        .to_owned();
    let size = u64::try_from(obj.size().unwrap_or(0)).unwrap_or(0);
    let last_modified = obj
        .last_modified()
        .ok_or_else(|| {
            Error::Other(
                format!("list_objects_v2 returned object `{key}` without last_modified").into(),
            )
        })?
        .to_time()
        .map_err(other_boxed)?;
    Ok(ObjectMeta {
        key,
        size,
        last_modified,
        // ListObjectsV2 does return ETags, but they are not consumed
        // by any current caller; keep `None` to avoid inflating the
        // per-object metadata for list results.
        etag: None,
    })
}

/// Pure classifier core (no `SdkError` involvement) so unit tests can
/// exercise every branch without synthesising SDK error types.
fn classify_status_and_code(status: u16, code: Option<&str>, key: &str) -> Option<Error> {
    match status {
        404 => return Some(Error::NotFound(key.to_owned())),
        403 => return Some(Error::AccessDenied(key.to_owned())),
        412 => return Some(Error::PreconditionFailed(key.to_owned())),
        409 => return Some(Error::Conflict(key.to_owned())),
        _ => {}
    }
    match code {
        Some("NoSuchKey" | "NoSuchBucket" | "NotFound") => Some(Error::NotFound(key.to_owned())),
        Some("AccessDenied") => Some(Error::AccessDenied(key.to_owned())),
        Some("PreconditionFailed") => Some(Error::PreconditionFailed(key.to_owned())),
        Some("ConditionalRequestConflict") => Some(Error::Conflict(key.to_owned())),
        _ => None,
    }
}

#[async_trait::async_trait]
impl ObjectStore for S3Store {
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, Error> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let resp = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .set_continuation_token(token.take())
                .send()
                .await
                .map_err(|e| classify(e, prefix))?;

            out.reserve(resp.contents().len());
            for obj in resp.contents() {
                out.push(object_to_meta(obj)?);
            }

            if !resp.is_truncated().unwrap_or(false) {
                break;
            }
            // Defensive: a server that signals truncated but omits the
            // continuation token would loop forever.
            match resp.next_continuation_token() {
                Some(t) => token = Some(t.to_owned()),
                None => break,
            }
        }
        Ok(out)
    }

    async fn get_to_file(&self, key: &str, dest: &Path) -> Result<(), Error> {
        let parent = dest.parent().ok_or_else(|| {
            Error::Other(format!("destination `{}` has no parent directory", dest.display()).into())
        })?;

        // Attempt the head→download cycle up to twice: if the object is
        // mutated between `head` and the body GET, the `If-Match` guard
        // returns 412 and we retry with the new ETag/size. On the second
        // iteration the `attempt == 0` guard is false, so a repeated 412
        // falls through to `Err(e) => return Err(e)` — every path returns.
        for attempt in 0..2 {
            let meta = self.head(key).await?;
            if meta.size == 0 {
                let temp = NamedTempFile::new_in(parent).map_err(other_boxed)?;
                persist_temp(temp, dest)?;
                return Ok(());
            }

            let temp = NamedTempFile::new_in(parent).map_err(other_boxed)?;
            let result = if meta.size <= MULTIPART_THRESHOLD {
                self.download_single(key, temp.path(), meta.etag.as_deref())
                    .await
            } else {
                self.download_multipart(key, temp.path(), meta.size, meta.etag.as_deref())
                    .await
            };

            match result {
                Ok(()) => {
                    persist_temp(temp, dest)?;
                    return Ok(());
                }
                Err(Error::PreconditionFailed(_)) if attempt == 0 => {
                    tracing::warn!(key, "object changed between head and GET; retrying");
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("both loop iterations return from within the match")
    }

    async fn get_bytes(&self, key: &str) -> Result<Bytes, Error> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| classify(e, key))?;
        let aggregated = resp
            .body
            .collect()
            .await
            .map_err(|e| Error::Network(Box::new(e)))?;
        Ok(aggregated.into_bytes())
    }

    async fn put_bytes(&self, key: &str, body: Bytes, opts: PutOpts) -> Result<(), Error> {
        // Note: aws-sdk-s3 1.x rejects PutObject bodies > 5 GiB; larger
        // payloads need multipart upload. Bundle objects in our schema
        // do not approach that limit; LFS-driven needs may extend the
        // trait in a later phase.
        self.put_body(key, ByteStream::from(body), opts).await
    }

    async fn put_path(&self, key: &str, src: &Path, opts: PutOpts) -> Result<(), Error> {
        // `ByteStream::from_path` streams from disk via tokio's async
        // file I/O, avoiding a full in-memory copy. Note: the 5 GiB
        // single-PUT ceiling still applies — `aws-sdk-s3` PutObject
        // does not auto-switch to multipart (that requires the separate
        // `aws-s3-transfer-manager` crate). Bundles well below 5 GiB;
        // LFS phase may need a multipart wrapper.
        let stream = ByteStream::from_path(src).await.map_err(other_boxed)?;
        self.put_body(key, stream, opts).await
    }

    async fn put_if_absent(&self, key: &str, body: Bytes) -> Result<bool, Error> {
        let resp = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .if_none_match("*")
            .body(ByteStream::from(body))
            .send()
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
        let resp = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| classify(e, key))?;
        let size = u64::try_from(resp.content_length().unwrap_or(0)).unwrap_or(0);
        let last_modified = resp
            .last_modified()
            .ok_or_else(|| {
                Error::Other(format!("head_object on `{key}` returned no last_modified").into())
            })?
            .to_time()
            .map_err(other_boxed)?;
        let etag = resp.e_tag().map(str::to_owned);
        Ok(ObjectMeta {
            key: key.to_owned(),
            size,
            last_modified,
            etag,
        })
    }

    async fn copy(&self, src: &str, dst: &str) -> Result<(), Error> {
        let copy_source = encode_copy_source(&self.bucket, src);
        // Pass `src` as the key context so a 404 surfaces as
        // `NotFound(src)` — that's what the trait promises.
        self.client
            .copy_object()
            .bucket(&self.bucket)
            .key(dst)
            .copy_source(copy_source)
            .send()
            .await
            .map_err(|e| classify(e, src))?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        // S3 DeleteObject is idempotent (returns 204 even for missing
        // keys), but the trait contract demands `Err(NotFound)` on a
        // missing key — so HEAD first. Concurrent deletion between this
        // HEAD and the DELETE will return Ok rather than NotFound;
        // semantically acceptable since the key existed at some point
        // during the call.
        self.head(key).await?;
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| classify(e, key))?;
        Ok(())
    }
}

/// Atomically rename a [`NamedTempFile`] to `dest`, mapping the
/// [`tempfile::PersistError`] into [`Error::Other`].
fn persist_temp(temp: NamedTempFile, dest: &Path) -> Result<(), Error> {
    temp.persist(dest)
        .map_err(|e| Error::Other(Box::new(e.error)))?;
    Ok(())
}

impl S3Store {
    /// Common upload helper: sends the given [`ByteStream`] to S3 with
    /// optional `Content-Disposition` and user metadata from [`PutOpts`].
    /// Shared by `put_bytes` (in-memory) and `put_path` (streamed from
    /// disk).
    async fn put_body(&self, key: &str, body: ByteStream, opts: PutOpts) -> Result<(), Error> {
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body);
        if let Some(cd) = &opts.content_disposition {
            req = req.content_disposition(cd);
        }
        for (k, v) in &opts.user_metadata {
            // S3 lowercases user-metadata keys on storage and limits the
            // combined header set to ~2 KB; ASCII only (RFC 2047 encode
            // non-ASCII upstream).
            req = req.metadata(k, v);
        }
        req.send().await.map_err(|e| classify(e, key))?;
        Ok(())
    }

    /// Stream a small (<= [`MULTIPART_THRESHOLD`]) object directly to the
    /// temp-file path. Caller is responsible for `persist`-ing the file.
    ///
    /// When `etag` is `Some`, the request carries `If-Match` so S3
    /// returns 412 if the object was overwritten since the `head` call.
    async fn download_single(
        &self,
        key: &str,
        temp_path: &Path,
        etag: Option<&str>,
    ) -> Result<(), Error> {
        let mut req = self.client.get_object().bucket(&self.bucket).key(key);
        if let Some(etag) = etag {
            req = req.if_match(etag);
        }
        let mut resp = req.send().await.map_err(|e| classify(e, key))?;

        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(temp_path)
            .await
            .map_err(other_boxed)?;

        while let Some(chunk) = resp.body.next().await {
            let bytes = chunk.map_err(|e| Error::Network(Box::new(e)))?;
            file.write_all(&bytes).await.map_err(other_boxed)?;
        }
        file.flush().await.map_err(other_boxed)?;
        Ok(())
    }

    /// Download a large object via parallel ranged GETs, writing each
    /// range at its absolute offset into the pre-allocated temp file.
    ///
    /// When `etag` is `Some`, every ranged GET carries `If-Match` so
    /// S3 returns 412 if the object is overwritten mid-download.
    async fn download_multipart(
        &self,
        key: &str,
        temp_path: &Path,
        size: u64,
        etag: Option<&str>,
    ) -> Result<(), Error> {
        let async_file = tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(false)
            .open(temp_path)
            .await
            .map_err(other_boxed)?;
        async_file.set_len(size).await.map_err(other_boxed)?;

        let file = Arc::new(Mutex::new(async_file));
        let semaphore = Arc::new(Semaphore::new(MULTIPART_MAX_CONCURRENCY));
        let mut tasks: JoinSet<Result<(), Error>> = JoinSet::new();

        let etag_owned = etag.map(str::to_owned);
        for (start, end) in plan_ranges(size, MULTIPART_CHUNK_SIZE) {
            let client = self.client.clone();
            let bucket = self.bucket.clone();
            let key = key.to_owned();
            let etag = etag_owned.clone();
            let file = Arc::clone(&file);
            let semaphore = Arc::clone(&semaphore);
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.map_err(other_boxed)?;
                let mut req = client
                    .get_object()
                    .bucket(&bucket)
                    .key(&key)
                    .range(format!("bytes={start}-{end}"));
                if let Some(etag) = &etag {
                    req = req.if_match(etag);
                }
                let resp = req.send().await.map_err(|e| classify(e, &key))?;
                let bytes = resp
                    .body
                    .collect()
                    .await
                    .map_err(|e| Error::Network(Box::new(e)))?
                    .into_bytes();
                let expected = end - start + 1;
                if bytes.len() as u64 != expected {
                    return Err(Error::Other(
                        format!(
                            "range bytes={start}-{end} returned {} bytes, expected {expected}",
                            bytes.len()
                        )
                        .into(),
                    ));
                }
                let mut f = file.lock().await;
                f.seek(SeekFrom::Start(start)).await.map_err(other_boxed)?;
                f.write_all(&bytes).await.map_err(other_boxed)?;
                Ok(())
            });
        }

        while let Some(joined) = tasks.join_next().await {
            joined.map_err(other_boxed)??;
        }

        // All spawned tasks have been joined above — each task's
        // captured `Arc` clone was dropped when its closure
        // completed, so this is the only outstanding reference.
        let mut file = Arc::try_unwrap(file)
            .expect("file Arc is unique after the JoinSet::join_next loop drains")
            .into_inner();
        file.flush().await.map_err(other_boxed)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url::{AzureAddressing, RemoteFlags};
    use aws_sdk_s3::primitives::DateTime;
    use aws_sdk_s3::types::Object;

    fn parse_endpoint(s: &str) -> Url {
        Url::parse(s).expect("test endpoint URL parses")
    }

    // --- object_to_meta -----------------------------------------------

    #[test]
    fn object_to_meta_round_trips_well_formed_object() {
        let modified = DateTime::from_secs(1_700_000_000);
        let obj = Object::builder()
            .key("refs/heads/main/abc.bundle")
            .size(42)
            .last_modified(modified)
            .build();
        let meta = object_to_meta(&obj).expect("conversion succeeds");
        assert_eq!(meta.key, "refs/heads/main/abc.bundle");
        assert_eq!(meta.size, 42);
        assert_eq!(meta.last_modified.unix_timestamp(), 1_700_000_000);
    }

    #[test]
    fn object_to_meta_rejects_missing_key() {
        let obj = Object::builder()
            .last_modified(DateTime::from_secs(1_700_000_000))
            .build();
        let err = object_to_meta(&obj).expect_err("missing key must error");
        match err {
            Error::Other(inner) => {
                assert!(
                    inner.to_string().contains("without a key"),
                    "error message names the failure: {inner}"
                );
            }
            other => panic!("expected Error::Other for missing key, got {other:?}"),
        }
    }

    #[test]
    fn object_to_meta_rejects_missing_last_modified() {
        let obj = Object::builder().key("k").size(0).build();
        let err = object_to_meta(&obj).expect_err("missing last_modified must error");
        match err {
            Error::Other(inner) => {
                let msg = inner.to_string();
                assert!(
                    msg.contains("without last_modified"),
                    "names failure: {msg}"
                );
                assert!(msg.contains("`k`"), "includes the key for context: {msg}");
            }
            other => panic!("expected Error::Other for missing last_modified, got {other:?}"),
        }
    }

    #[test]
    fn object_to_meta_clamps_negative_size_to_zero() {
        // S3 cannot legally return a negative size, but the SDK types
        // it as `i64`. Defensive default: clamp to 0 rather than
        // sign-extend to a huge u64.
        let obj = Object::builder()
            .key("k")
            .size(-1)
            .last_modified(DateTime::from_secs(1_700_000_000))
            .build();
        let meta = object_to_meta(&obj).expect("conversion succeeds");
        assert_eq!(meta.size, 0);
    }

    // --- plan_ranges --------------------------------------------------

    #[test]
    fn plan_ranges_zero_size_yields_empty_vec() {
        assert!(plan_ranges(0, 16).is_empty());
    }

    #[test]
    fn plan_ranges_zero_chunk_yields_empty_vec() {
        assert!(plan_ranges(100, 0).is_empty());
    }

    #[test]
    fn plan_ranges_size_one_byte() {
        assert_eq!(plan_ranges(1, 16), vec![(0, 0)]);
    }

    #[test]
    fn plan_ranges_size_below_chunk() {
        assert_eq!(plan_ranges(10, 16), vec![(0, 9)]);
    }

    #[test]
    fn plan_ranges_size_equals_chunk() {
        assert_eq!(plan_ranges(16, 16), vec![(0, 15)]);
    }

    #[test]
    fn plan_ranges_size_one_byte_above_chunk() {
        assert_eq!(plan_ranges(17, 16), vec![(0, 15), (16, 16)]);
    }

    #[test]
    fn plan_ranges_exact_multiple_of_chunk() {
        assert_eq!(
            plan_ranges(48, 16),
            vec![(0, 15), (16, 31), (32, 47)],
            "three full chunks, no leftover"
        );
    }

    #[test]
    fn plan_ranges_with_partial_final_chunk() {
        assert_eq!(
            plan_ranges(50, 16),
            vec![(0, 15), (16, 31), (32, 47), (48, 49)]
        );
    }

    #[test]
    fn plan_ranges_handles_huge_size_without_overflow() {
        // 6 GiB at 16 MiB chunks → 384 chunks, all valid u64 arithmetic.
        let size = 6u64 * 1024 * 1024 * 1024;
        let chunk = 16u64 * 1024 * 1024;
        let ranges = plan_ranges(size, chunk);
        assert_eq!(ranges.len(), 384);
        assert_eq!(ranges.first().copied(), Some((0, chunk - 1)));
        assert_eq!(ranges.last().copied(), Some((size - chunk, size - 1)));
    }

    // --- normalize_endpoint -------------------------------------------

    #[test]
    fn normalize_endpoint_path_style_strips_bucket_path() {
        let url = parse_endpoint("https://s3.us-west-2.amazonaws.com/my-bucket");
        let out = normalize_endpoint(&url, S3Addressing::PathStyle).unwrap();
        assert_eq!(out.host_str(), Some("s3.us-west-2.amazonaws.com"));
        assert_eq!(out.path(), "/");
        assert!(out.query().is_none());
    }

    #[test]
    fn normalize_endpoint_strips_query_string() {
        // Our URL parser leaves `?addressing=path` etc. on the endpoint;
        // the SDK rejects any query component.
        let url = parse_endpoint("http://127.0.0.1:9000/my-bucket?addressing=path");
        let out = normalize_endpoint(&url, S3Addressing::PathStyle).unwrap();
        assert!(out.query().is_none(), "query must be stripped: {out}");
        assert_eq!(out.path(), "/");
        assert_eq!(out.host_str(), Some("127.0.0.1"));
        assert_eq!(out.port(), Some(9000));
    }

    #[test]
    fn normalize_endpoint_strips_bucket_label_for_virtual_hosted() {
        let url = parse_endpoint("https://my-bucket.s3.us-west-2.amazonaws.com/");
        let out = normalize_endpoint(&url, S3Addressing::VirtualHosted).unwrap();
        assert_eq!(out.host_str(), Some("s3.us-west-2.amazonaws.com"));
        assert_eq!(out.scheme(), "https");
        assert_eq!(out.path(), "/");
    }

    #[test]
    fn normalize_endpoint_virtual_hosted_preserves_port_and_scheme() {
        let url = parse_endpoint("http://my-bucket.s3.example.com:9000/some/path?x=1");
        let out = normalize_endpoint(&url, S3Addressing::VirtualHosted).unwrap();
        assert_eq!(out.scheme(), "http");
        assert_eq!(out.host_str(), Some("s3.example.com"));
        assert_eq!(out.port(), Some(9000));
        assert_eq!(out.path(), "/");
        assert!(out.query().is_none());
    }

    // --- resolve_region -----------------------------------------------

    #[test]
    fn resolve_region_flag_takes_precedence() {
        let url = parse_endpoint("https://my-bucket.s3.us-west-2.amazonaws.com/");
        assert_eq!(
            resolve_region(&url, Some("eu-central-1")),
            Some("eu-central-1".to_owned())
        );
    }

    #[test]
    fn resolve_region_extracts_from_virtual_hosted_aws_host() {
        let url = parse_endpoint("https://my-bucket.s3.us-west-2.amazonaws.com/");
        assert_eq!(resolve_region(&url, None), Some("us-west-2".to_owned()));
    }

    #[test]
    fn resolve_region_extracts_from_path_style_aws_host() {
        let url = parse_endpoint("https://s3.eu-west-1.amazonaws.com/my-bucket");
        assert_eq!(resolve_region(&url, None), Some("eu-west-1".to_owned()));
    }

    #[test]
    fn resolve_region_handles_legacy_hyphenated_form() {
        let url = parse_endpoint("https://s3-ap-south-1.amazonaws.com/my-bucket");
        assert_eq!(resolve_region(&url, None), Some("ap-south-1".to_owned()));
    }

    #[test]
    fn resolve_region_legacy_no_segment_returns_none() {
        // s3.amazonaws.com (no region segment) — let the SDK's provider
        // chain pick from env/profile.
        let url = parse_endpoint("https://s3.amazonaws.com/my-bucket");
        assert_eq!(resolve_region(&url, None), None);
    }

    #[test]
    fn resolve_region_non_aws_host_defaults_to_us_east_1() {
        let url = parse_endpoint("http://localhost:9000/my-bucket");
        assert_eq!(resolve_region(&url, None), Some("us-east-1".to_owned()));
    }

    #[test]
    fn resolve_region_r2_endpoint_defaults_to_us_east_1() {
        let url = parse_endpoint("https://abc123.r2.cloudflarestorage.com/my-bucket");
        assert_eq!(resolve_region(&url, None), Some("us-east-1".to_owned()));
    }

    // --- encode_copy_source -------------------------------------------

    #[test]
    fn encode_copy_source_preserves_slash_between_bucket_and_key() {
        let out = encode_copy_source("my-bucket", "refs/heads/main/abc.bundle");
        assert_eq!(out, "my-bucket/refs/heads/main/abc.bundle");
    }

    #[test]
    fn encode_copy_source_encodes_hash_in_lock_keys() {
        // LOCK#.lock from upstream's locking scheme — # is reserved.
        let out = encode_copy_source("my-bucket", "refs/heads/main/LOCK#.lock");
        assert_eq!(out, "my-bucket/refs/heads/main/LOCK%23.lock");
    }

    #[test]
    fn encode_copy_source_encodes_spaces_and_query_chars() {
        let out = encode_copy_source("my-bucket", "weird key?with=stuff");
        assert!(out.contains("%20"), "space encoded: {out}");
        assert!(out.contains("%3F"), "? encoded: {out}");
        assert!(out.contains("%3D"), "= encoded: {out}");
    }

    #[test]
    fn encode_copy_source_passes_unreserved_through() {
        let out = encode_copy_source("my.bucket-name_v1~", "abc-def_ghi.txt");
        assert_eq!(out, "my.bucket-name_v1~/abc-def_ghi.txt");
    }

    // --- classify_status_and_code ------------------------------------

    #[test]
    fn classify_404_status_is_not_found() {
        assert!(matches!(
            classify_status_and_code(404, None, "k"),
            Some(Error::NotFound(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_403_status_is_access_denied() {
        assert!(matches!(
            classify_status_and_code(403, None, "k"),
            Some(Error::AccessDenied(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_412_status_is_precondition_failed() {
        assert!(matches!(
            classify_status_and_code(412, None, "k"),
            Some(Error::PreconditionFailed(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_409_status_is_conflict() {
        // The 409 case is critical: AWS S3 returns 409 when two
        // If-None-Match: "*" PUTs race even on a key that did not exist
        // beforehand. Without this branch, put_if_absent would surface
        // racing-write contention as a hard error instead of Ok(false).
        assert!(matches!(
            classify_status_and_code(409, None, "k"),
            Some(Error::Conflict(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_no_such_key_code_falls_back_to_not_found() {
        assert!(matches!(
            classify_status_and_code(500, Some("NoSuchKey"), "k"),
            Some(Error::NotFound(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_conditional_request_conflict_code_is_conflict() {
        assert!(matches!(
            classify_status_and_code(500, Some("ConditionalRequestConflict"), "k"),
            Some(Error::Conflict(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_unrecognised_returns_none() {
        assert!(classify_status_and_code(500, Some("InternalError"), "k").is_none());
        assert!(classify_status_and_code(500, None, "k").is_none());
    }

    // --- from_remote_url constructor branch ---------------------------

    fn azure_url() -> RemoteUrl {
        RemoteUrl::Azure {
            endpoint: parse_endpoint("https://acct.blob.core.windows.net/container"),
            account: "acct".to_owned(),
            container: "container".to_owned(),
            prefix: None,
            addressing: AzureAddressing::Subdomain,
            flags: RemoteFlags::default(),
        }
    }

    #[tokio::test]
    async fn from_remote_url_rejects_azure() {
        let result = S3Store::from_remote_url(&azure_url()).await;
        match result {
            Err(Error::Other(_)) => {}
            Err(other) => panic!("expected Error::Other, got {other:?}"),
            Ok(_) => panic!("expected Azure URL to be rejected"),
        }
    }

    // --- ResolvedS3Config (URL → decisions) ---------------------------

    #[test]
    fn resolved_path_style_minio() {
        let endpoint = parse_endpoint("http://127.0.0.1:9000/my-bucket?addressing=path");
        let resolved =
            ResolvedS3Config::from_url_parts(&endpoint, S3Addressing::PathStyle, None, None)
                .expect("resolves");
        assert!(resolved.force_path_style);
        assert_eq!(resolved.endpoint_url.host_str(), Some("127.0.0.1"));
        assert_eq!(resolved.endpoint_url.port(), Some(9000));
        assert_eq!(resolved.endpoint_url.path(), "/");
        assert!(resolved.endpoint_url.query().is_none());
        assert_eq!(resolved.region.as_deref(), Some("us-east-1"));
        assert!(resolved.profile.is_none());
    }

    #[test]
    fn resolved_virtual_hosted_aws_strips_bucket_and_picks_region() {
        let endpoint = parse_endpoint("https://my-bucket.s3.us-west-2.amazonaws.com/");
        let resolved =
            ResolvedS3Config::from_url_parts(&endpoint, S3Addressing::VirtualHosted, None, None)
                .expect("resolves");
        assert!(!resolved.force_path_style);
        assert_eq!(
            resolved.endpoint_url.host_str(),
            Some("s3.us-west-2.amazonaws.com")
        );
        assert!(
            !resolved.endpoint_url.as_str().contains("my-bucket"),
            "bucket label must be stripped: {}",
            resolved.endpoint_url
        );
        assert_eq!(resolved.region.as_deref(), Some("us-west-2"));
    }

    #[test]
    fn resolved_explicit_flags_propagate() {
        let endpoint = parse_endpoint("http://127.0.0.1:9000/my-bucket");
        let resolved = ResolvedS3Config::from_url_parts(
            &endpoint,
            S3Addressing::PathStyle,
            Some("dev-profile"),
            Some("eu-central-1"),
        )
        .expect("resolves");
        assert_eq!(resolved.region.as_deref(), Some("eu-central-1"));
        assert_eq!(resolved.profile.as_deref(), Some("dev-profile"));
    }

    #[tokio::test]
    async fn build_s3_config_round_trips_resolved_decisions() {
        // We can't peek into aws_sdk_s3::Config getters reliably across
        // SDK 1.x patch releases, so just confirm the build call accepts
        // every decision shape without panicking. The decisions
        // themselves are tested via `ResolvedS3Config` above.
        let endpoint = parse_endpoint("http://127.0.0.1:9000/my-bucket");
        let resolved =
            ResolvedS3Config::from_url_parts(&endpoint, S3Addressing::PathStyle, None, None)
                .expect("resolves");
        let _config = build_s3_config(&resolved).await;
    }
}

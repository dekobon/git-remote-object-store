//! S3 backend for the [`ObjectStore`] trait.
//!
//! `S3Store` wraps `aws-sdk-s3`. The SDK owns `SigV4`, retries, connection
//! pooling, and timeout policy; this module owns the URL → SDK config
//! translation, the error-code classifier ([`classify`]), and the
//! hand-rolled multipart download orchestrator that the SDK does not
//! provide.
//!
//! ## Key composition
//!
//! `S3Store` does **not** auto-prepend the [`RemoteUrl`] `prefix`. Trait
//! keys are byte-prefix per the contract on
//! [`ObjectStore::list`]; the URL `prefix` is
//! a repository concern and is composed by callers that build keys like
//! `<prefix>/refs/.../<sha>.bundle`.
//!
//! ## Conditional writes
//!
//! [`put_if_absent`][super::ObjectStore::put_if_absent] uses
//! `If-None-Match: "*"`. S3 returns either 412 (`PreconditionFailed`)
//! when the key already exists or 409 (`ConditionalRequestConflict`)
//! when two PUTs race. Both collapse to `Ok(false)`.
//!
//! ## Size limits
//!
//! AWS caps a single `PutObject` body at [`SINGLE_PUT_LIMIT_BYTES`]
//! (5 GiB) and a multipart upload at [`S3_MAX_PARTS`] (10 000) parts;
//! the per-object ceiling is 5 TiB. The helper auto-promotes uploads
//! above [`super::multipart::MULTIPART_PUT_THRESHOLD`] onto the
//! multipart path, so callers do not have to reason about the 5 GiB
//! single-PUT cutoff. The upload path is **not resumable** across
//! process death — see the README "Known limitations" section.
//!
//! ## Atomic `get_to_file`
//!
//! Both the small-object and multipart download paths write to a sibling
//! [`tempfile::NamedTempFile`] and rename on success so a partial
//! failure cannot leave a corrupt destination for the unbundle step.
//!
//! Every GET carries `If-Match: <etag>` derived from the preceding
//! `HeadObject` call. If the object is overwritten between `head` and
//! the body download, S3 returns 412 and `get_to_file` retries once
//! with a fresh `head`/`ETag`. After one retry the 412 propagates as
//! [`ObjectStoreError::PreconditionFailed`].
//!
//! ## HTTP transport tuning
//!
//! `aws-sdk-s3`'s default HTTP client keeps idle pooled connections
//! indefinitely, so a pooled connection to a rotated VIP would wedge
//! an in-flight request until the OS-level TCP retransmit timeout
//! fires (~15 minutes on Linux). [`S3Store::from_remote_url`] installs
//! a custom HTTP client built via [`aws_smithy_http_client::Builder`]
//! with [`POOL_IDLE_TIMEOUT`] bounded to 30 s, so a rotation costs at
//! most one short-circuited request rather than minutes of wedged
//! transfer. Tracking issue: #26.
//!
//! Pool-idle alone does not bound a *hot* pooled connection — one that
//! was used within the last 30 s but has since become stuck — and the
//! 412 retry in [`ObjectStore::get_to_file`] is a deliberate-server-
//! response retry, so forcing a fresh connection there does not help.
//! Instead, the SDK's [`aws_config::timeout::TimeoutConfig`] is given
//! a [`READ_TIMEOUT`] so a stuck request fails fast and the SDK's
//! internal retry layer can pick a fresh one. `connect_timeout` is
//! left at the SDK default (3.1 s, already aggressive). Tracking
//! issue: #26.
//!
//! Note: smithy's `read_timeout` resolves the HTTP connector future at
//! "response-headers received." That bounds:
//! - **Uploads** in full — the connector future cannot resolve until
//!   the request body is sent and the response status arrives, so a
//!   stuck upload trips at [`READ_TIMEOUT`]. `put_body` therefore
//!   overrides the timeout per-operation so large bundle uploads are
//!   not cut off at 30 s.
//! - **Downloads** only up to time-to-first-byte. Once response
//!   headers arrive the future resolves; subsequent body-chunk reads
//!   are not bounded by `read_timeout`. A peer that wedges mid-body
//!   on a GET (e.g. a stuck multipart range) is still subject to the
//!   pool-idle / TCP-keepalive layers, but not to `READ_TIMEOUT`.
//!   "Pool-idle timeouts do not bound hot connections" in
//!   `docs/development/lessons_learned.md` covers this.
//!
//! TCP keepalive (the second knob suggested in #27) is **not** wired
//! on the S3 path: `aws-smithy-http-client` 1.1.12's public `Builder`
//! / `ConnectorBuilder` API exposes `pool_idle_timeout` but does not
//! expose `tcp_keepalive`. The dominant DNS-rotation failure in #26 is
//! pool reuse of a dead VIP, which `pool_idle_timeout` already fixes;
//! the gap relative to the Azure backend (which uses `reqwest` and
//! gets keepalive for free) is documented in `CHANGELOG.md`.
//!
//! ## Multipart-upload lifetime safety
//!
//! S3 retains uncompleted multipart uploads indefinitely without an
//! explicit lifecycle rule, so a future dropped between
//! `CreateMultipartUpload` and `CompleteMultipartUpload` orphans the
//! upload-id and bills the caller for the staged parts (issues #169,
//! #171). [`S3Store::start_multipart_upload`] therefore hands back a
//! [`MultipartUploadGuard`] that owns the upload-id and best-effort
//! issues `AbortMultipartUpload` on `Drop`; [`finish_multipart_upload`]
//! is the only call site that may [`disarm`] the guard.
//!
//! Future contributors must **not** introduce an early `?`-return
//! between obtaining the upload-id and constructing the
//! [`MultipartUploadGuard`] inside `start_multipart_upload`, nor
//! between `start_multipart_upload` and the matching
//! `finish_multipart_upload`: a bare upload-id outside the guard
//! reintroduces the leak the guard exists to prevent. (The
//! `ok_or_else` for a missing upload-id field on the SDK response is
//! benign — there is no upload-id to abort.)
//!
//! Azure has no equivalent need — uncommitted blocks auto-expire after
//! seven days (`azure.rs`).
//!
//! [`finish_multipart_upload`]: S3Store::finish_multipart_upload
//! [`disarm`]: MultipartUploadGuard::disarm
//!
//! ## Stdout discipline
//!
//! Per `.claude/rules/protocol-stdout.md`, this module never writes to
//! stdout. Diagnostics go through `tracing` (which the helper binaries
//! configure to write to stderr).

use std::io::SeekFrom;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aws_config::timeout::TimeoutConfig;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::{ByteStream, Length};
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, MetadataDirective};
use aws_smithy_http_client::tls::{Provider as TlsProvider, rustls_provider::CryptoMode};
use aws_smithy_types_convert::date_time::DateTimeExt;
use bytes::Bytes;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use tempfile::NamedTempFile;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use url::Url;

use crate::url::{
    AWS_S3_INFIXES, RemoteUrl, S3Addressing, s3_virtual_hosted_bucket, strip_aws_host_suffix,
};

use super::error::{network_boxed, other_boxed};
use super::multipart::{
    MULTIPART_PUT_MAX_CONCURRENCY, MULTIPART_PUT_PART_SIZE, S3_MAX_PARTS, UploadPart,
    plan_upload_parts, read_file_part, should_use_multipart, slice_bytes_part,
};
use super::{
    GetOpts, ObjectMeta, ObjectStore, ObjectStoreError, ProgressSink, PutOpts, persist_temp,
};

/// Object-size cutoff above which `get_to_file` switches from a single
/// streaming GET to parallel ranged GETs.
pub(crate) const MULTIPART_THRESHOLD: u64 = 25 * 1024 * 1024;
/// Range size for each ranged GET in the multipart download path.
pub(crate) const MULTIPART_CHUNK_SIZE: u64 = 16 * 1024 * 1024;
/// Maximum simultaneous in-flight ranged GETs in the multipart download path.
pub(crate) const MULTIPART_MAX_CONCURRENCY: usize = 8;

/// S3's hard ceiling on a single `PutObject` body. Reported in
/// [`ObjectStoreError::PayloadTooLarge`] when the SDK surfaces
/// `EntityTooLarge` (HTTP 400) or HTTP 413 so the wire-line names the
/// number rather than dumping an opaque SDK chain.
pub(crate) const SINGLE_PUT_LIMIT_BYTES: u64 = 5 * (1 << 30);

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

/// Bound on how long an idle pooled HTTPS connection lingers before
/// the smithy connection pool drops it. Short enough that DNS rotation
/// rarely hits a stale pooled connection; long enough that bursty
/// fetch / push batches still benefit from connection reuse. See the
/// module-level "HTTP transport tuning" docs and issue #26.
pub(crate) const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout applied to every S3 GET, HEAD, LIST, and lock-write request.
/// Catches a hot pooled connection that has gone silent (e.g. mid-LFS
/// session when the server VIP rotates). Sized to match
/// [`POOL_IDLE_TIMEOUT`] — both budgets are "give up and let the SDK
/// retry pick a fresh connection" budgets.
///
/// Note: smithy's `read_timeout` resolves the HTTP connector future at
/// "response-headers received." For uploads that includes the request
/// body (the connector cannot resolve until the response arrives), so
/// [`S3Store::put_body`] overrides the timeout per-operation to keep
/// large bundle uploads from being cut off at 30 s. For downloads it
/// is a time-to-first-byte bound only — body chunks streamed after the
/// headers are not subject to `READ_TIMEOUT`. See the module-level
/// transport docs and "Pool-idle timeouts do not bound hot connections"
/// in `docs/development/lessons_learned.md`.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-operation timeout config applied to every S3 PUT (object upload).
///
/// `disable_read_timeout()` is the entire point of this helper: a
/// regression that returns `TimeoutConfig::builder().build()` (all
/// fields `Unset`) re-introduces issue #26 (large uploads aborted at
/// 30 s). Pinned by `put_body_upload_override_disables_read_timeout`
/// so the fix cannot silently revert.
fn upload_timeout_config() -> TimeoutConfig {
    TimeoutConfig::builder().disable_read_timeout().build()
}

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
    ) -> Result<Self, ObjectStoreError> {
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
    /// The [`RemoteUrl::S3::prefix`] field is intentionally **not**
    /// consumed here; callers compose it into keys themselves per the
    /// module-level docs.
    ///
    /// # Errors
    ///
    /// Returns [`ObjectStoreError::Other`] if `url` is not the S3 variant
    /// or if the endpoint URL cannot be normalised for virtual-hosted
    /// addressing.
    pub async fn from_remote_url(url: &RemoteUrl) -> Result<Self, ObjectStoreError> {
        let RemoteUrl::S3 {
            endpoint,
            bucket,
            addressing,
            flags,
            ..
        } = url
        else {
            return Err(ObjectStoreError::Other(
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

    /// Verify the bucket is reachable with the configured credentials by
    /// issuing a single `ListObjectsV2` with `max_keys=1`. Used by
    /// [`crate::protocol::backend::build`] to fold credential / missing-bucket /
    /// authorization failures into categorical
    /// [`crate::protocol::backend::BackendError`] variants before the
    /// helper REPL runs its first command.
    pub(crate) async fn probe(&self, prefix: &str) -> Result<(), ObjectStoreError> {
        self.client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .max_keys(1)
            .send()
            .await
            .map_err(|e| classify(e, prefix))?;
        Ok(())
    }
}

/// Build the `aws-sdk-s3` config from a [`ResolvedS3Config`].
///
/// 1. Load the AWS SDK provider chain with `BehaviorVersion::latest()`.
/// 2. Install a custom HTTP client with [`POOL_IDLE_TIMEOUT`] so DNS
///    rotation does not wedge long-running sessions (#26).
/// 3. Apply [`READ_TIMEOUT`] so a stuck GET/HEAD/LIST/lock request fails
///    fast instead of waiting for the OS-level TCP retransmit timeout
///    (#26). `connect_timeout` is left at the SDK default (3.1 s).
///    `put_body` overrides this per-operation to allow large uploads.
/// 4. Apply `endpoint_url`, `profile`, `region` from the resolved decisions.
/// 5. Override `force_path_style` on the resulting `aws_sdk_s3::Config`.
pub(crate) async fn build_s3_config(resolved: &ResolvedS3Config) -> aws_sdk_s3::Config {
    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .http_client(
            aws_smithy_http_client::Builder::new()
                .tls_provider(TlsProvider::Rustls(CryptoMode::AwsLc))
                .pool_idle_timeout(POOL_IDLE_TIMEOUT)
                .build_https(),
        )
        .timeout_config(TimeoutConfig::builder().read_timeout(READ_TIMEOUT).build())
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
pub(crate) fn normalize_endpoint(
    endpoint: &Url,
    addressing: S3Addressing,
) -> Result<Url, ObjectStoreError> {
    let mut rewritten = endpoint.clone();
    rewritten.set_path("");
    rewritten.set_query(None);
    rewritten.set_fragment(None);

    if matches!(addressing, S3Addressing::VirtualHosted) {
        let host = rewritten
            .host_str()
            .ok_or_else(|| ObjectStoreError::Other("endpoint URL has no host".into()))?;
        // Use `s3_virtual_hosted_bucket` (rightmost-infix rfind) to find the
        // bucket label length, then strip `bucket.` from the front. This
        // handles dotted bucket names like `bucketname.com.s3.region.amazonaws.com`
        // correctly; a plain `split_once('.')` would stop at the first dot
        // and leave `com.s3.…` as the endpoint instead of `s3.…`.
        // For non-AWS virtual-hosted endpoints without the `.s3.` infix
        // (e.g. MinIO with `bucket.minio.example.com`), fall back to
        // stripping just the leftmost label.
        let regional_host = s3_virtual_hosted_bucket(host)
            // `s3_virtual_hosted_bucket` returns the bucket label (a strict
            // prefix of `host` ending just before the `.s3.` or `.s3-`
            // infix). The byte at `host[bucket.len()]` is always `.` (ASCII
            // 0x2E), so slicing at `+ 1` is always a valid UTF-8 boundary.
            .map(|bucket| host[bucket.len() + 1..].to_owned())
            .or_else(|| host.split_once('.').map(|(_, rest)| rest.to_owned()))
            .ok_or_else(|| {
                ObjectStoreError::Other(
                    format!("virtual-hosted endpoint host `{host}` has no dot separator").into(),
                )
            })?;
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
    // Bare `amazonaws.com` is an AWS host with no leading content (no
    // region segment), so it short-circuits to `None` like `s3.amazonaws.com`
    // does — the SDK provider chain picks the region. Everything else
    // that does not end in an AWS partition suffix is treated as a
    // third-party S3-compatible endpoint and gets the safe `us-east-1`
    // default.
    if host == "amazonaws.com" {
        return None;
    }
    let Some(trimmed) = strip_aws_host_suffix(host) else {
        return Some("us-east-1".to_owned());
    };
    extract_aws_region(trimmed)
}

/// Parse the AWS region out of an AWS hostname's leading portion (the
/// host with its [`AWS_HOST_SUFFIXES`] suffix already stripped).
fn extract_aws_region(trimmed: &str) -> Option<String> {
    // Patterns we accept (in priority order):
    //   s3                        → legacy us-east-1 (no region segment) → None
    //   s3.<region>               → path-style regional
    //   s3-<region>               → legacy hyphenated form
    //   <bucket>.s3.<region>      → simple virtual-hosted (single-label bucket)
    //   <dotted.bucket>.s3.<region>  → dotted-bucket virtual-hosted (4+ labels)
    let labels: Vec<&str> = trimmed.split('.').collect();
    // The explicit arms below match a fixed label count (1, 2, or 3),
    // which guarantees the captured `region` is a single dot-free label.
    // Only the fallback arm operates on the unbounded "4+ labels" shape,
    // where the captured region could in principle still contain a `.`
    // (a malformed host); the dot-filter on that arm rejects those.
    match labels.as_slice() {
        ["s3"] => None,
        ["s3", region] => Some((*region).to_owned()),
        [_bucket, "s3", region] => Some((*region).to_owned()),
        [head] if head.starts_with("s3-") => Some(head["s3-".len()..].to_owned()),
        // Dotted-bucket virtual-hosted: find the rightmost service infix
        // (.s3. or .s3-) and return the segment after it as the region.
        // e.g. "bucketname.com.s3.us-west-2" → "us-west-2"
        // Use max by byte position so we pick the rightmost infix when
        // both `.s3.` and `.s3-` appear in the host.
        _ => AWS_S3_INFIXES
            .iter()
            .filter_map(|infix| {
                trimmed
                    .rfind(infix)
                    .map(|idx| (idx, trimmed[idx + infix.len()..].to_owned()))
            })
            .max_by_key(|(idx, _)| *idx)
            .map(|(_, region)| region)
            .filter(|region| !region.is_empty() && !region.contains('.')),
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

/// Map a typed `aws-sdk-s3` error into the trait's [`ObjectStoreError`] enum.
///
/// `key` is the operation's key/prefix context — it appears in the
/// resulting [`ObjectStoreError::NotFound`] / [`ObjectStoreError::AccessDenied`] /
/// [`ObjectStoreError::PreconditionFailed`] / [`ObjectStoreError::Conflict`] payload.
///
/// Note that this also covers typed `NotFound` / `NoSuchKey` variants
/// the SDK constructs from 404 responses: those carry HTTP 404 on
/// `svc.raw().status()` and so route through the status-based branch
/// of [`classify_status_and_code`].
fn classify<E>(err: SdkError<E>, key: &str) -> ObjectStoreError
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
        SdkError::DispatchFailure(_) | SdkError::TimeoutError(_) => network_boxed(err),
        _ => other_boxed(err),
    }
}

/// Convert a single [`aws_sdk_s3::types::Object`] from a
/// `ListObjectsV2` page into the trait's [`ObjectMeta`].
///
/// Extracted so unit tests can drive the missing-key and
/// missing-last-modified guard branches via `Object`'s builder
/// without synthesising a full `ListObjectsV2Output`.
pub(crate) fn object_to_meta(
    obj: &aws_sdk_s3::types::Object,
) -> Result<ObjectMeta, ObjectStoreError> {
    let key = obj
        .key()
        .ok_or_else(|| {
            ObjectStoreError::Other("list_objects_v2 returned an object without a key".into())
        })?
        .to_owned();
    let size = u64::try_from(obj.size().unwrap_or(0)).unwrap_or(0);
    let last_modified = obj
        .last_modified()
        .ok_or_else(|| {
            ObjectStoreError::Other(
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

/// Convert a [`HeadObject`] response's relevant fields into the trait's
/// [`ObjectMeta`].
///
/// Extracted so unit tests can drive the missing-content-length and
/// missing-last-modified guard branches without standing up a live S3
/// or constructing a full `HeadObjectOutput` (whose builder is not
/// trivially mockable).
///
/// A missing `Content-Length` is an error rather than silent zero: a
/// 0-byte size is semantically meaningful in this codebase (lock
/// files are intentionally empty) and downstream `get_to_file` takes
/// a fast path on `size == 0` that writes an empty destination file.
/// Treating "header absent" as 0 would silently produce empty bundles
/// instead of surfacing the malformed response. Every backend HEAD
/// must yield `Content-Length`.
pub(crate) fn head_output_to_meta(
    key: &str,
    content_length: Option<i64>,
    last_modified: Option<&aws_sdk_s3::primitives::DateTime>,
    etag: Option<&str>,
) -> Result<ObjectMeta, ObjectStoreError> {
    let raw_size = content_length.ok_or_else(|| {
        ObjectStoreError::Other(format!("head_object on `{key}` returned no content-length").into())
    })?;
    // `i64` is the SDK's wire type; clamp a (legally impossible) negative
    // value to 0 rather than wrap to a huge u64. Mirrors `object_to_meta`.
    let size = u64::try_from(raw_size).unwrap_or(0);
    let last_modified = last_modified
        .ok_or_else(|| {
            ObjectStoreError::Other(
                format!("head_object on `{key}` returned no last_modified").into(),
            )
        })?
        .to_time()
        .map_err(other_boxed)?;
    Ok(ObjectMeta {
        key: key.to_owned(),
        size,
        last_modified,
        etag: etag.map(str::to_owned),
    })
}

/// Pure classifier core (no `SdkError` involvement) so unit tests can
/// exercise every branch without synthesising SDK error types.
fn classify_status_and_code(
    status: u16,
    code: Option<&str>,
    key: &str,
) -> Option<ObjectStoreError> {
    match status {
        404 => return Some(ObjectStoreError::NotFound(key.to_owned())),
        403 => return Some(ObjectStoreError::AccessDenied(key.to_owned())),
        412 => return Some(ObjectStoreError::PreconditionFailed(key.to_owned())),
        409 => return Some(ObjectStoreError::Conflict(key.to_owned())),
        // S3 occasionally surfaces HTTP 413 directly (front-door / proxy
        // path); the canonical EntityTooLarge response is HTTP 400, but
        // the status mapping is the same regardless of the SDK code.
        413 => {
            return Some(ObjectStoreError::PayloadTooLarge {
                limit_bytes: SINGLE_PUT_LIMIT_BYTES,
            });
        }
        _ => {}
    }
    match code {
        Some("NoSuchKey" | "NoSuchBucket" | "NotFound") => {
            Some(ObjectStoreError::NotFound(key.to_owned()))
        }
        Some("AccessDenied") => Some(ObjectStoreError::AccessDenied(key.to_owned())),
        Some("PreconditionFailed") => Some(ObjectStoreError::PreconditionFailed(key.to_owned())),
        Some("ConditionalRequestConflict") => Some(ObjectStoreError::Conflict(key.to_owned())),
        // S3 returns HTTP 400 + `EntityTooLarge` when a single-PUT body
        // exceeds 5 GiB. The status alone is too broad to hang
        // `PayloadTooLarge` on, so route via the code.
        Some("EntityTooLarge") => Some(ObjectStoreError::PayloadTooLarge {
            limit_bytes: SINGLE_PUT_LIMIT_BYTES,
        }),
        _ => None,
    }
}

#[async_trait::async_trait]
impl ObjectStore for S3Store {
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, ObjectStoreError> {
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

        // Mirror Azure: try once, retry once on 412 (the head→GET race).
        // After the second attempt any error — including a repeated 412 —
        // propagates. Encoding retry as an explicit second call keeps every
        // control-flow path returning a value, so no `unreachable!` is
        // required.
        let progress = opts.progress.as_ref();
        match self.head_then_download(key, dest, parent, progress).await {
            Err(ObjectStoreError::PreconditionFailed(_)) => {
                tracing::warn!(key, "object changed between head and GET; retrying");
                self.head_then_download(key, dest, parent, progress).await
            }
            other => other,
        }
    }

    async fn get_bytes(&self, key: &str) -> Result<Bytes, ObjectStoreError> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| classify(e, key))?;
        let aggregated = resp.body.collect().await.map_err(network_boxed)?;
        Ok(aggregated.into_bytes())
    }

    /// Issue a `GetObject` with a `Range: bytes=<start>-<end-1>` header.
    /// HTTP 416 maps to [`ObjectStoreError::RangeNotSatisfiable`] with
    /// the original requested range (so the wire-line names what the
    /// caller asked for, not the server's translation). All other
    /// failures route through [`classify`].
    ///
    /// S3 silently truncates a ranged GET to EOF when the requested
    /// range overruns the object — `start < body.len() <= end` returns
    /// `start..body.len()` bytes with HTTP 206 and no error. The
    /// post-flight length check via [`super::verify_range_response_length`]
    /// elevates that mismatch to [`ObjectStoreError::RangeNotSatisfiable`]
    /// so callers (notably the packchain reader) cannot mistake a
    /// truncated slice for the full requested range.
    async fn get_bytes_range(
        &self,
        key: &str,
        range: std::ops::Range<u64>,
    ) -> Result<Bytes, ObjectStoreError> {
        if let Some(empty) = super::precheck_range(key, &range)? {
            return Ok(empty);
        }
        // Inclusive end byte for the HTTP `Range` header (RFC 7233).
        let inclusive_end = range.end - 1;
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .range(format!("bytes={}-{}", range.start, inclusive_end))
            .send()
            .await;
        let resp = match result {
            Ok(resp) => resp,
            Err(err) => {
                if let SdkError::ServiceError(svc) = &err
                    && svc.raw().status().as_u16() == 416
                {
                    return Err(ObjectStoreError::RangeNotSatisfiable {
                        key: key.to_owned(),
                        requested: range,
                    });
                }
                return Err(classify(err, key));
            }
        };
        let aggregated = resp.body.collect().await.map_err(network_boxed)?;
        super::verify_range_response_length(key, &range, aggregated.into_bytes())
    }

    async fn put_bytes(
        &self,
        key: &str,
        body: Bytes,
        opts: PutOpts,
    ) -> Result<(), ObjectStoreError> {
        // PutObject rejects bodies > 5 GiB; above [`MULTIPART_PUT_THRESHOLD`]
        // we hand off to the multipart path which lifts that ceiling and
        // emits per-part progress events. Below the threshold we keep the
        // single round trip (no `CreateMultipartUpload` cost). Issue #53.
        let size = body.len() as u64;
        if should_use_multipart(size) {
            return self.multipart_put_bytes(key, body, size, opts).await;
        }
        let progress = opts.progress.clone();
        self.put_body(key, ByteStream::from(body), opts).await?;
        if let Some(sink) = progress
            && size > 0
        {
            sink.report(size);
        }
        Ok(())
    }

    async fn put_path(&self, key: &str, src: &Path, opts: PutOpts) -> Result<(), ObjectStoreError> {
        // Open the file once and read size from the open handle. This
        // closes the metadata/upload race that would let a concurrent
        // truncate or rename produce a body whose length disagrees
        // with the size we used for multipart planning. The size-based
        // multipart dispatch (issue #53) and the post-transfer
        // progress event both consume the same `body_len`.
        let file = tokio::fs::File::open(src).await.map_err(other_boxed)?;
        let body_len = file.metadata().await.map_err(other_boxed)?.len();
        if should_use_multipart(body_len) {
            return self.multipart_put_path(key, file, body_len, opts).await;
        }
        // Below the threshold: single PutObject. `FsBuilder::file`
        // consumes our already-open handle so the SDK does not
        // re-open by path (which would re-introduce the race).
        let stream = ByteStream::read_from()
            .file(file)
            .length(Length::Exact(body_len))
            .build()
            .await
            .map_err(other_boxed)?;
        let progress = opts.progress.clone();
        self.put_body(key, stream, opts).await?;
        if let Some(sink) = progress
            && body_len > 0
        {
            sink.report(body_len);
        }
        Ok(())
    }

    async fn put_if_absent(&self, key: &str, body: Bytes) -> Result<bool, ObjectStoreError> {
        let resp = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .if_none_match("*")
            .body(ByteStream::from(body))
            .send()
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
        let resp = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| classify(e, key))?;
        head_output_to_meta(
            key,
            resp.content_length(),
            resp.last_modified(),
            resp.e_tag(),
        )
    }

    async fn copy(&self, src: &str, dst: &str) -> Result<(), ObjectStoreError> {
        // `CopyObject` rejects bodies > 5 GiB; above the multipart
        // threshold we HEAD `src` and use `UploadPartCopy` per part
        // (issue #53). The HEAD adds one round trip for small copies,
        // but the only production caller (`Doctor::evict_losing_bundle`)
        // is a quarantine path on bundles that can be multi-GiB —
        // paying one HEAD to learn whether to multipart is worth it.
        //
        // The HEAD also yields the source `ETag`, which we pass as
        // `x-amz-copy-source-if-match` on every subsequent
        // `CopyObject` / `UploadPartCopy`. Without it, a source
        // mutation between HEAD and copy would silently produce a
        // destination whose bytes are a mix of the pre- and post-
        // mutation source. With it, S3 returns 412
        // (`PreconditionFailed`) and the caller can retry. Azure's
        // `copy()` already has this property because it routes
        // through `head_then_download`'s 412 retry.
        let meta = self.head(src).await?;
        if should_use_multipart(meta.size) {
            return self
                .multipart_copy(src, dst, meta.size, meta.etag.as_deref())
                .await;
        }
        let copy_source = encode_copy_source(&self.bucket, src);
        // `MetadataDirective::Replace` makes S3 consistent with the Azure
        // backend (which drops metadata on copy via download-then-reupload):
        // neither backend preserves user metadata, matching the trait
        // contract in `ObjectStore::copy`.
        // Pass `src` as the key context so a 404 surfaces as
        // `NotFound(src)` — that's what the trait promises.
        let mut req = self
            .client
            .copy_object()
            .bucket(&self.bucket)
            .key(dst)
            .copy_source(copy_source)
            .metadata_directive(MetadataDirective::Replace);
        if let Some(etag) = meta.etag.as_deref() {
            req = req.copy_source_if_match(etag);
        }
        req.send().await.map_err(|e| classify(e, src))?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), ObjectStoreError> {
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

    /// Presign a `GetObject` request for `key` valid for `ttl`. Used
    /// by the `bundle-uri` capability (issue #76) to advertise
    /// time-limited download URLs against private buckets. The
    /// returned URL carries an `X-Amz-Signature` query parameter
    /// derived from the SDK's resolved `SigV4` credentials and an
    /// `X-Amz-Expires=<ttl-seconds>` parameter that the operator
    /// can use to verify the requested TTL was honoured.
    ///
    /// # Errors
    ///
    /// Returns [`ObjectStoreError::Other`] when the SDK rejects the
    /// TTL (AWS caps presigned URLs at 7 days) or when the
    /// presigning step fails (e.g. credential provider returned no
    /// credentials).
    async fn presigned_get_url(
        &self,
        key: &str,
        ttl: std::time::Duration,
    ) -> Result<String, ObjectStoreError> {
        let config = aws_sdk_s3::presigning::PresigningConfig::expires_in(ttl).map_err(|e| {
            ObjectStoreError::Other(format!("PresigningConfig::expires_in({ttl:?}): {e}").into())
        })?;
        let presigned = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(config)
            .await
            .map_err(|e| classify(e, key))?;
        Ok(presigned.uri().to_owned())
    }
}

impl S3Store {
    /// One head→tempfile→download→persist round trip.
    ///
    /// Factored out so [`get_to_file`](ObjectStore::get_to_file) can invoke
    /// it twice: once normally, once more on a 412 retry. Mirrors
    /// `AzureStore::head_then_download` so both backends share the same
    /// retry shape.
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
            // Skip the GET entirely for zero-byte objects (lock files):
            // `download_single` would issue a plain GET for an empty body
            // and `download_multipart` would set_len(0) with no ranges —
            // both correct but a wasted round trip and a wasted file
            // open, respectively.
            return persist_temp(temp, dest);
        }

        if meta.size <= MULTIPART_THRESHOLD {
            self.download_single(key, temp.path(), meta.etag.as_deref(), progress)
                .await?;
        } else {
            self.download_multipart(key, temp.path(), meta.size, meta.etag.as_deref(), progress)
                .await?;
        }
        persist_temp(temp, dest)
    }

    /// Common upload helper: sends the given [`ByteStream`] to S3 with
    /// optional `Content-Disposition` and user metadata from [`PutOpts`].
    /// Shared by `put_bytes` (in-memory) and `put_path` (streamed from
    /// disk).
    async fn put_body(
        &self,
        key: &str,
        body: ByteStream,
        opts: PutOpts,
    ) -> Result<(), ObjectStoreError> {
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
        // Disable read_timeout for this operation: smithy's read_timeout
        // resolves the HTTP connector future at "response-headers received,"
        // which for uploads includes the entire request body upload. The
        // global READ_TIMEOUT (30 s) would otherwise abort any bundle
        // upload that takes longer than 30 s. GET/HEAD/LIST operations keep
        // the timeout via the client-level config; uploads opt out here.
        //
        // Caveat: smithy's `MergeTimeoutConfig::merge_iter` treats an
        // override whose `has_timeouts()` is false (no field in `Set` state
        // — only `Disabled` and `Unset` count as "no timeouts") as a no-op
        // and skips inheriting from the client-level config. So this
        // override does NOT inherit `connect_timeout` (or any future
        // client-level timeout) from the SDK's config. That is fine for the
        // current use case — the only timeout we configure at the client
        // level is `read_timeout`, which we explicitly want to disable —
        // but a future client-level `connect_timeout` would have to be
        // duplicated here to take effect on uploads.
        req.customize()
            .config_override(
                aws_sdk_s3::config::Builder::new().timeout_config(upload_timeout_config()),
            )
            .send()
            .await
            .map_err(|e| classify(e, key))?;
        Ok(())
    }

    /// Stream a small (<= [`MULTIPART_THRESHOLD`]) object directly to the
    /// temp-file path. Caller is responsible for `persist`-ing the file.
    ///
    /// When `etag` is `Some`, the request carries `If-Match` so S3
    /// returns 412 if the object was overwritten since the `head` call.
    /// When `progress` is `Some`, fires once per SDK body chunk read
    /// off the wire — chunk sizes follow the SDK's internal aggregation
    /// (typically 1 MiB-ish for HTTPS).
    async fn download_single(
        &self,
        key: &str,
        temp_path: &Path,
        etag: Option<&str>,
        progress: Option<&ProgressSink>,
    ) -> Result<(), ObjectStoreError> {
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

    /// Download a large object via parallel ranged GETs, writing each
    /// range at its absolute offset into the pre-allocated temp file.
    ///
    /// When `etag` is `Some`, every ranged GET carries `If-Match` so
    /// S3 returns 412 if the object is overwritten mid-download. When
    /// `progress` is `Some`, fires once per completed range with the
    /// range's byte count — events arrive out of order, matching the
    /// concurrent-GET schedule, but cumulative bytes equal `size` after
    /// the last event.
    async fn download_multipart(
        &self,
        key: &str,
        temp_path: &Path,
        size: u64,
        etag: Option<&str>,
        progress: Option<&ProgressSink>,
    ) -> Result<(), ObjectStoreError> {
        let async_file = tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(false)
            .open(temp_path)
            .await
            .map_err(other_boxed)?;
        async_file.set_len(size).await.map_err(other_boxed)?;

        let file = Arc::new(Mutex::new(async_file));
        let semaphore = Arc::new(Semaphore::new(MULTIPART_MAX_CONCURRENCY));
        let mut tasks: JoinSet<Result<(), ObjectStoreError>> = JoinSet::new();

        let etag_owned = etag.map(str::to_owned);
        let progress_owned = progress.cloned();
        for (start, end) in plan_ranges(size, MULTIPART_CHUNK_SIZE) {
            let client = self.client.clone();
            let bucket = self.bucket.clone();
            let key = key.to_owned();
            let etag = etag_owned.clone();
            let file = Arc::clone(&file);
            let semaphore = Arc::clone(&semaphore);
            let progress = progress_owned.clone();
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
                    .map_err(network_boxed)?
                    .into_bytes();
                let expected = end - start + 1;
                if bytes.len() as u64 != expected {
                    return Err(ObjectStoreError::Other(
                        format!(
                            "range bytes={start}-{end} returned {} bytes, expected {expected}",
                            bytes.len()
                        )
                        .into(),
                    ));
                }
                let chunk_len = bytes.len() as u64;
                let mut f = file.lock().await;
                f.seek(SeekFrom::Start(start)).await.map_err(other_boxed)?;
                f.write_all(&bytes).await.map_err(other_boxed)?;
                drop(f);
                if let Some(sink) = &progress {
                    sink.report(chunk_len);
                }
                Ok(())
            });
        }

        while let Some(joined) = tasks.join_next().await {
            joined.map_err(other_boxed)??;
        }

        // All spawned tasks have been joined above — each task's
        // captured `Arc` clone was dropped when its closure
        // completed, so this is the only outstanding reference. If
        // some future refactor accidentally leaks a clone, surface a
        // structured error rather than aborting the process: flush via
        // the `Mutex` instead of taking sole ownership.
        match Arc::try_unwrap(file) {
            Ok(mutex) => {
                let mut f = mutex.into_inner();
                f.flush().await.map_err(other_boxed)?;
            }
            Err(shared) => {
                let mut f = shared.lock().await;
                f.flush().await.map_err(other_boxed)?;
            }
        }
        Ok(())
    }

    /// Drive a multipart upload from a fully-buffered `Bytes` body.
    ///
    /// `Bytes::slice` is zero-copy — every part borrows into the same
    /// underlying allocation, so peak memory equals the caller's body
    /// rather than `body × parts`.
    async fn multipart_put_bytes(
        &self,
        key: &str,
        body: Bytes,
        size: u64,
        opts: PutOpts,
    ) -> Result<(), ObjectStoreError> {
        let parts = plan_upload_parts(size, MULTIPART_PUT_PART_SIZE, S3_MAX_PARTS);
        let guard = self.start_multipart_upload(key, &opts).await?;
        let progress = opts.progress.clone();
        let result = self
            .upload_parts_with_bodies(key, guard.upload_id(), &parts, progress, |part| {
                slice_bytes_part(&body, part)
            })
            .await;
        self.finish_multipart_upload(guard, result).await
    }

    /// Drive a multipart upload by streaming a local file part-by-part.
    ///
    /// All tasks share one `Arc<std::fs::File>`; per-task
    /// `read_file_part` uses `pread` so reads are concurrent without
    /// offset contention. Sharing one open file description closes
    /// the metadata/upload race that would otherwise let a
    /// concurrent rename or truncate produce parts with inconsistent
    /// content. With `MULTIPART_PUT_MAX_CONCURRENCY = 8` and
    /// `MULTIPART_PUT_PART_SIZE = 16 MiB`, peak memory is bounded at
    /// 128 MiB regardless of file size — acceptable for LFS uploads.
    async fn multipart_put_path(
        &self,
        key: &str,
        file: tokio::fs::File,
        size: u64,
        opts: PutOpts,
    ) -> Result<(), ObjectStoreError> {
        let parts = plan_upload_parts(size, MULTIPART_PUT_PART_SIZE, S3_MAX_PARTS);
        let guard = self.start_multipart_upload(key, &opts).await?;
        let progress = opts.progress.clone();
        let file: Arc<std::fs::File> = Arc::new(file.into_std().await);
        let result = self
            .upload_parts_from_file(key, guard.upload_id(), file, &parts, progress)
            .await;
        self.finish_multipart_upload(guard, result).await
    }

    /// Drive a multipart server-side copy via `UploadPartCopy`.
    ///
    /// Each part issues an `UploadPartCopy` request with
    /// `x-amz-copy-source` (the bucket+key, percent-encoded) and
    /// `x-amz-copy-source-range: bytes=<start>-<end>` so the copy
    /// runs entirely server-side — no body crosses the wire.
    /// The destination object's metadata starts empty (matching the
    /// trait contract: copy drops user metadata) because
    /// `CreateMultipartUpload` is invoked without any metadata.
    ///
    /// `src_etag`, if `Some`, is forwarded as
    /// `x-amz-copy-source-if-match` on every part so a mid-copy
    /// source mutation surfaces as `PreconditionFailed` rather than
    /// silently producing a destination with mixed pre/post-mutation
    /// bytes.
    async fn multipart_copy(
        &self,
        src: &str,
        dst: &str,
        size: u64,
        src_etag: Option<&str>,
    ) -> Result<(), ObjectStoreError> {
        let parts = plan_upload_parts(size, MULTIPART_PUT_PART_SIZE, S3_MAX_PARTS);
        // `copy()` uses default opts (no progress, no metadata) so the
        // create-call below also carries no metadata. That preserves
        // the "copy drops user metadata" contract.
        let guard = self
            .start_multipart_upload(dst, &PutOpts::default())
            .await?;
        let copy_source = encode_copy_source(&self.bucket, src);
        let result = self
            .upload_parts_via_copy(src, dst, guard.upload_id(), &copy_source, src_etag, &parts)
            .await;
        // The upload_id lives under `dst` even though the bytes come
        // from `src`; the guard already carries `dst` as its key.
        // Errors mid-copy are reported with `src` as context inside
        // the per-part task (matches single-call `copy()` at the trait
        // surface).
        self.finish_multipart_upload(guard, result).await
    }

    /// Begin a multipart upload, returning a guard that owns the
    /// upload-id and aborts the upload on drop.
    ///
    /// `content_disposition` and `user_metadata` from `PutOpts` flow
    /// onto `CreateMultipartUpload` — the destination object inherits
    /// them on `CompleteMultipartUpload` (`UploadPart`/`UploadPartCopy`
    /// have no metadata fields of their own).
    ///
    /// The returned [`MultipartUploadGuard`] keeps the upload-id alive
    /// for the duration of the upload. If the calling future is
    /// dropped (cancelled, panicked, or the caller picks the other arm
    /// of a `select!`) before [`finish_multipart_upload`] runs, the
    /// guard's [`Drop`] best-effort dispatches `AbortMultipartUpload`
    /// so the upload-id is reclaimed and the caller is not billed for
    /// orphaned parts (issues #169, #171). S3 retains uncompleted
    /// multipart uploads indefinitely without an explicit lifecycle
    /// rule; Azure has no equivalent need (uncommitted blocks auto-
    /// expire after seven days).
    ///
    /// [`finish_multipart_upload`]: Self::finish_multipart_upload
    async fn start_multipart_upload(
        &self,
        key: &str,
        opts: &PutOpts,
    ) -> Result<MultipartUploadGuard, ObjectStoreError> {
        let mut req = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key);
        if let Some(cd) = &opts.content_disposition {
            req = req.content_disposition(cd);
        }
        for (k, v) in &opts.user_metadata {
            req = req.metadata(k, v);
        }
        let resp = req.send().await.map_err(|e| classify(e, key))?;
        let upload_id = resp.upload_id().map(str::to_owned).ok_or_else(|| {
            ObjectStoreError::Other(
                format!("CreateMultipartUpload for `{key}` returned no upload-id").into(),
            )
        })?;
        Ok(MultipartUploadGuard::new(
            self.client.clone(),
            self.bucket.clone(),
            key.to_owned(),
            upload_id,
        ))
    }

    /// Spawn parallel `UploadPart` tasks, one per planned part, with
    /// the part body produced by `make_body(part) -> Bytes`. Used by
    /// `multipart_put_bytes` (slice from in-memory `Bytes`).
    async fn upload_parts_with_bodies<F>(
        &self,
        key: &str,
        upload_id: &str,
        parts: &[UploadPart],
        progress: Option<ProgressSink>,
        make_body: F,
    ) -> Result<Vec<CompletedPart>, ObjectStoreError>
    where
        F: Fn(UploadPart) -> Result<Bytes, ObjectStoreError>,
    {
        let semaphore = Arc::new(Semaphore::new(MULTIPART_PUT_MAX_CONCURRENCY));
        let mut tasks: JoinSet<Result<CompletedPart, ObjectStoreError>> = JoinSet::new();
        for (idx, part) in parts.iter().enumerate() {
            let part = *part;
            // S3 caps multipart uploads at S3_MAX_PARTS = 10 000
            // (`plan_upload_parts` enforces this), so `idx + 1` always
            // fits in i32.
            let part_number = i32::try_from(idx + 1)
                .expect("plan_upload_parts caps parts <= S3_MAX_PARTS = 10_000");
            let body = make_body(part)?;
            let client = self.client.clone();
            let bucket = self.bucket.clone();
            let key = key.to_owned();
            let upload_id = upload_id.to_owned();
            let semaphore = Arc::clone(&semaphore);
            let progress = progress.clone();
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.map_err(other_boxed)?;
                let resp = client
                    .upload_part()
                    .bucket(&bucket)
                    .key(&key)
                    .upload_id(&upload_id)
                    .part_number(part_number)
                    .body(ByteStream::from(body))
                    .customize()
                    .config_override(
                        aws_sdk_s3::config::Builder::new().timeout_config(upload_timeout_config()),
                    )
                    .send()
                    .await
                    .map_err(|e| classify(e, &key))?;
                let etag = resp.e_tag().map(str::to_owned).ok_or_else(|| {
                    ObjectStoreError::Other(
                        format!("UploadPart for `{key}` part {part_number} returned no ETag")
                            .into(),
                    )
                })?;
                if let Some(sink) = &progress {
                    sink.report(part.length);
                }
                Ok(CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(etag)
                    .build())
            });
        }
        join_completed_parts(tasks, parts.len()).await
    }

    /// Spawn parallel `UploadPart` tasks that each read their part
    /// from the shared `Arc<std::fs::File>` via `pread`. With
    /// concurrency 8 and 16 MiB parts, peak memory is 128 MiB.
    async fn upload_parts_from_file(
        &self,
        key: &str,
        upload_id: &str,
        file: Arc<std::fs::File>,
        parts: &[UploadPart],
        progress: Option<ProgressSink>,
    ) -> Result<Vec<CompletedPart>, ObjectStoreError> {
        let semaphore = Arc::new(Semaphore::new(MULTIPART_PUT_MAX_CONCURRENCY));
        let mut tasks: JoinSet<Result<CompletedPart, ObjectStoreError>> = JoinSet::new();
        for (idx, part) in parts.iter().enumerate() {
            let part = *part;
            // S3 caps multipart uploads at S3_MAX_PARTS = 10 000
            // (`plan_upload_parts` enforces this), so `idx + 1` always
            // fits in i32.
            let part_number = i32::try_from(idx + 1)
                .expect("plan_upload_parts caps parts <= S3_MAX_PARTS = 10_000");
            let client = self.client.clone();
            let bucket = self.bucket.clone();
            let key = key.to_owned();
            let upload_id = upload_id.to_owned();
            let task_file = Arc::clone(&file);
            let semaphore = Arc::clone(&semaphore);
            let progress = progress.clone();
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.map_err(other_boxed)?;
                let body = read_file_part(task_file, part).await?;
                let resp = client
                    .upload_part()
                    .bucket(&bucket)
                    .key(&key)
                    .upload_id(&upload_id)
                    .part_number(part_number)
                    .body(ByteStream::from(body))
                    .customize()
                    .config_override(
                        aws_sdk_s3::config::Builder::new().timeout_config(upload_timeout_config()),
                    )
                    .send()
                    .await
                    .map_err(|e| classify(e, &key))?;
                let etag = resp.e_tag().map(str::to_owned).ok_or_else(|| {
                    ObjectStoreError::Other(
                        format!("UploadPart for `{key}` part {part_number} returned no ETag")
                            .into(),
                    )
                })?;
                if let Some(sink) = &progress {
                    sink.report(part.length);
                }
                Ok(CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(etag)
                    .build())
            });
        }
        join_completed_parts(tasks, parts.len()).await
    }

    /// Spawn parallel `UploadPartCopy` tasks for a server-side
    /// multipart copy. No body crosses the wire — each task only
    /// sends the source identifier and a byte range header.
    ///
    /// `src_etag`, if `Some`, is set as `x-amz-copy-source-if-match`
    /// on every part. A mid-copy source mutation then surfaces as
    /// `PreconditionFailed` on the offending part rather than
    /// silently producing a mixed destination.
    async fn upload_parts_via_copy(
        &self,
        src: &str,
        dst: &str,
        upload_id: &str,
        copy_source: &str,
        src_etag: Option<&str>,
        parts: &[UploadPart],
    ) -> Result<Vec<CompletedPart>, ObjectStoreError> {
        let semaphore = Arc::new(Semaphore::new(MULTIPART_PUT_MAX_CONCURRENCY));
        let mut tasks: JoinSet<Result<CompletedPart, ObjectStoreError>> = JoinSet::new();
        for (idx, part) in parts.iter().enumerate() {
            let part = *part;
            // S3 caps multipart uploads at S3_MAX_PARTS = 10 000
            // (`plan_upload_parts` enforces this), so `idx + 1` always
            // fits in i32.
            let part_number = i32::try_from(idx + 1)
                .expect("plan_upload_parts caps parts <= S3_MAX_PARTS = 10_000");
            let client = self.client.clone();
            let bucket = self.bucket.clone();
            let dst = dst.to_owned();
            let src_ctx = src.to_owned();
            let upload_id = upload_id.to_owned();
            let copy_source = copy_source.to_owned();
            let src_etag = src_etag.map(str::to_owned);
            let range = format!("bytes={}-{}", part.offset, part.offset + part.length - 1);
            let semaphore = Arc::clone(&semaphore);
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.map_err(other_boxed)?;
                // `UploadPartCopy` failures point at the source (a 404
                // or 403 means the source went away or is now denied)
                // so classify against `src_ctx`, mirroring single-call
                // `copy()` at the trait surface.
                // Disable read_timeout for the same reason `put_body`
                // does — issue #26, and "Pool-idle timeouts do not
                // bound hot connections" in
                // `docs/development/lessons_learned.md`: smithy
                // resolves the connector future at "response-headers
                // received," but `UploadPartCopy` doesn't return until
                // the server-side copy completes — which for a 16 MiB
                // part on a slow region can exceed the 30 s
                // [`READ_TIMEOUT`].
                let mut req = client
                    .upload_part_copy()
                    .bucket(&bucket)
                    .key(&dst)
                    .upload_id(&upload_id)
                    .part_number(part_number)
                    .copy_source(&copy_source)
                    .copy_source_range(&range);
                if let Some(etag) = &src_etag {
                    req = req.copy_source_if_match(etag);
                }
                let resp = req
                    .customize()
                    .config_override(
                        aws_sdk_s3::config::Builder::new().timeout_config(upload_timeout_config()),
                    )
                    .send()
                    .await
                    .map_err(|e| classify(e, &src_ctx))?;
                let etag = resp
                    .copy_part_result()
                    .and_then(|r| r.e_tag())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        ObjectStoreError::Other(
                            format!(
                                "UploadPartCopy for `{src_ctx}` → `{dst}` part {part_number} returned no ETag"
                            )
                            .into(),
                        )
                    })?;
                Ok(CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(etag)
                    .build())
            });
        }
        join_completed_parts(tasks, parts.len()).await
    }

    /// Finalize a multipart upload: complete on success, best-effort
    /// abort on error.
    ///
    /// On success, `complete_multipart_upload` runs and the guard is
    /// disarmed so its [`Drop`] does not fire a redundant abort.
    ///
    /// On a per-part error or join failure, the abort is issued
    /// inline (awaited) so the call returns only after the upload-id
    /// has been released — matching the synchronous error semantics
    /// callers had before the RAII guard was introduced. The guard
    /// is disarmed *after* the inline abort completes; if the inline
    /// abort itself panics or is cancelled, the guard's [`Drop`]
    /// fires the abort again on a detached task. A double-abort is
    /// harmless — S3 returns `NoSuchUpload` on the second call.
    ///
    /// If `complete_multipart_upload` itself fails, the function
    /// returns the classified error via `?` *before* `disarm()`;
    /// the still-armed guard's [`Drop`] then dispatches the abort
    /// on a detached task. This keeps the function short — no
    /// extra inline-abort branch — and folds the rare
    /// "Complete failed" case onto the same Drop path the
    /// future-cancellation case exercises.
    ///
    /// Abort failures are logged via `tracing::warn` but the
    /// original error wins; surfacing the abort error would mask
    /// the cause.
    async fn finish_multipart_upload(
        &self,
        mut guard: MultipartUploadGuard,
        parts: Result<Vec<CompletedPart>, ObjectStoreError>,
    ) -> Result<(), ObjectStoreError> {
        match parts {
            Ok(parts) => {
                let multipart = CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build();
                self.client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(guard.key())
                    .upload_id(guard.upload_id())
                    .multipart_upload(multipart)
                    .send()
                    .await
                    .map_err(|e| classify(e, guard.key()))?;
                guard.disarm();
                Ok(())
            }
            Err(err) => {
                if let Err(abort_err) = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(guard.key())
                    .upload_id(guard.upload_id())
                    .send()
                    .await
                {
                    tracing::warn!(
                        key = %guard.key(),
                        upload_id = %guard.upload_id(),
                        ?abort_err,
                        "AbortMultipartUpload failed; orphan upload may incur storage cost \
                         until lifecycle expiry",
                    );
                }
                guard.disarm();
                Err(err)
            }
        }
    }
}

/// RAII guard for an in-flight S3 multipart upload.
///
/// Owns the inputs needed to issue `AbortMultipartUpload` without
/// re-borrowing the [`S3Store`]. While `armed`, the guard's [`Drop`]
/// best-effort dispatches `AbortMultipartUpload` on a detached
/// `tokio::spawn` task so a future dropped between
/// `CreateMultipartUpload` and `CompleteMultipartUpload` does not
/// orphan the upload-id on the bucket (issues #169, #171).
///
/// Call [`disarm`] once the upload has been completed or its abort
/// has been issued synchronously so [`Drop`] becomes a no-op.
///
/// [`disarm`]: Self::disarm
struct MultipartUploadGuard {
    client: aws_sdk_s3::Client,
    bucket: String,
    key: String,
    upload_id: String,
    armed: bool,
}

impl MultipartUploadGuard {
    fn new(client: aws_sdk_s3::Client, bucket: String, key: String, upload_id: String) -> Self {
        Self {
            client,
            bucket,
            key,
            upload_id,
            armed: true,
        }
    }

    fn upload_id(&self) -> &str {
        &self.upload_id
    }

    fn key(&self) -> &str {
        &self.key
    }

    /// Mark the upload as resolved so [`Drop`] does not fire a
    /// redundant `AbortMultipartUpload`.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for MultipartUploadGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // `Drop` cannot `.await`, so the abort is issued on a
        // detached `tokio::spawn` task. `Handle::try_current()`
        // returns `Err` if Drop runs outside any runtime (e.g. a
        // test that constructs the guard and immediately drops it);
        // in that case the best we can do is warn-log — panicking
        // in Drop is forbidden by project rules.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                key = %self.key,
                upload_id = %self.upload_id,
                "MultipartUploadGuard dropped outside a tokio runtime; \
                 cannot dispatch AbortMultipartUpload (orphan upload may \
                 incur storage cost until S3 lifecycle expiry)",
            );
            return;
        };
        // Move owned fields into the detached task so the abort
        // outlives the dropped future. `Client` clones cheaply
        // (internal `Arc`); the strings move via `mem::take`.
        let client = self.client.clone();
        let bucket = std::mem::take(&mut self.bucket);
        let key = std::mem::take(&mut self.key);
        let upload_id = std::mem::take(&mut self.upload_id);
        handle.spawn(async move {
            if let Err(abort_err) = client
                .abort_multipart_upload()
                .bucket(&bucket)
                .key(&key)
                .upload_id(&upload_id)
                .send()
                .await
            {
                tracing::warn!(
                    key = %key,
                    upload_id = %upload_id,
                    ?abort_err,
                    "AbortMultipartUpload (drop-fire) failed; orphan upload may \
                     incur storage cost until S3 lifecycle expiry",
                );
            }
        });
    }
}

/// Drain a `JoinSet` of part-upload tasks into a `Vec<CompletedPart>`,
/// short-circuiting on the first error and sorting the result by
/// `part_number` so the `CompleteMultipartUpload` request honours
/// S3's "parts in part-number order" requirement.
async fn join_completed_parts(
    mut tasks: JoinSet<Result<CompletedPart, ObjectStoreError>>,
    capacity: usize,
) -> Result<Vec<CompletedPart>, ObjectStoreError> {
    let mut completed = Vec::with_capacity(capacity);
    while let Some(joined) = tasks.join_next().await {
        let part = joined.map_err(other_boxed)??;
        completed.push(part);
    }
    completed.sort_by_key(|p| {
        // Each `CompletedPart` here was built via
        // `CompletedPart::builder().part_number(...).e_tag(...).build()`
        // in the spawn loops above, so `part_number` is always set.
        p.part_number()
            .expect("CompletedPart built with explicit part_number")
    });
    Ok(completed)
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
            ObjectStoreError::Other(inner) => {
                assert!(
                    inner.to_string().contains("without a key"),
                    "error message names the failure: {inner}"
                );
            }
            other => panic!("expected ObjectStoreError::Other for missing key, got {other:?}"),
        }
    }

    #[test]
    fn object_to_meta_rejects_missing_last_modified() {
        let obj = Object::builder().key("k").size(0).build();
        let err = object_to_meta(&obj).expect_err("missing last_modified must error");
        match err {
            ObjectStoreError::Other(inner) => {
                let msg = inner.to_string();
                assert!(
                    msg.contains("without last_modified"),
                    "names failure: {msg}"
                );
                assert!(msg.contains("`k`"), "includes the key for context: {msg}");
            }
            other => {
                panic!("expected ObjectStoreError::Other for missing last_modified, got {other:?}")
            }
        }
    }

    // --- head_output_to_meta -------------------------------------------

    #[test]
    fn head_output_to_meta_round_trips_well_formed_response() {
        let modified = DateTime::from_secs(1_700_000_000);
        let meta = head_output_to_meta("k", Some(42), Some(&modified), Some("\"abc\""))
            .expect("conversion succeeds");
        assert_eq!(meta.key, "k");
        assert_eq!(meta.size, 42);
        assert_eq!(meta.last_modified.unix_timestamp(), 1_700_000_000);
        assert_eq!(meta.etag.as_deref(), Some("\"abc\""));
    }

    #[test]
    fn head_output_to_meta_preserves_legitimate_zero_size() {
        // Zero-byte lock files are legitimate in this codebase; a
        // `Content-Length: 0` header (i.e. `Some(0)`) must round-trip
        // as `size == 0`, distinct from the missing-header error.
        let modified = DateTime::from_secs(1_700_000_000);
        let meta = head_output_to_meta("LOCK", Some(0), Some(&modified), None)
            .expect("conversion succeeds");
        assert_eq!(meta.size, 0);
    }

    #[test]
    fn head_output_to_meta_rejects_missing_content_length() {
        let modified = DateTime::from_secs(1_700_000_000);
        let err = head_output_to_meta("k", None, Some(&modified), None)
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
    fn head_output_to_meta_rejects_missing_last_modified() {
        let err = head_output_to_meta("k", Some(0), None, None)
            .expect_err("missing last_modified must error");
        match err {
            ObjectStoreError::Other(inner) => {
                let msg = inner.to_string();
                assert!(msg.contains("no last_modified"), "names failure: {msg}");
                assert!(msg.contains("`k`"), "includes the key for context: {msg}");
            }
            other => {
                panic!("expected ObjectStoreError::Other for missing last_modified, got {other:?}")
            }
        }
    }

    #[test]
    fn head_output_to_meta_clamps_negative_size_to_zero() {
        // The SDK types content_length as `Option<i64>`; a (legally
        // impossible) negative value clamps to 0 rather than wrapping
        // to a huge u64. Mirrors `object_to_meta` behavior.
        let modified = DateTime::from_secs(1_700_000_000);
        let meta =
            head_output_to_meta("k", Some(-1), Some(&modified), None).expect("conversion succeeds");
        assert_eq!(meta.size, 0);
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

    #[test]
    fn normalize_endpoint_dotted_bucket_virtual_hosted() {
        // Bucket name contains dots (e.g. "bucketname.com"). A plain
        // `split_once('.')` would stop at the first dot and produce
        // "com.s3.us-west-2.amazonaws.com" instead of the correct
        // "s3.us-west-2.amazonaws.com".
        let url = parse_endpoint("https://bucketname.com.s3.us-west-2.amazonaws.com/some/path");
        let out = normalize_endpoint(&url, S3Addressing::VirtualHosted).unwrap();
        assert_eq!(out.host_str(), Some("s3.us-west-2.amazonaws.com"));
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

    #[test]
    fn resolve_region_dotted_bucket_virtual_hosted() {
        // Bucket name contains dots. The host has 4+ labels after stripping
        // `.amazonaws.com`; `resolve_region` must still find the region.
        let url = parse_endpoint("https://bucketname.com.s3.us-west-2.amazonaws.com/some/path");
        assert_eq!(resolve_region(&url, None), Some("us-west-2".to_owned()));
    }

    #[test]
    fn resolve_region_china_partition_virtual_hosted() {
        // China partition (`.amazonaws.com.cn`) — the suffix list in
        // `crate::url::AWS_HOST_SUFFIXES` is the single source of truth
        // for which suffixes count as AWS. This test pins parity between
        // `check_aws_s3_host` (which accepts the suffix) and
        // `resolve_region` (which must extract the region from it).
        let url = parse_endpoint("https://my-bucket.s3.cn-north-1.amazonaws.com.cn/repo");
        assert_eq!(resolve_region(&url, None), Some("cn-north-1".to_owned()));
    }

    #[test]
    fn resolve_region_china_partition_path_style() {
        let url = parse_endpoint("https://s3.cn-northwest-1.amazonaws.com.cn/my-bucket");
        assert_eq!(
            resolve_region(&url, None),
            Some("cn-northwest-1".to_owned())
        );
    }

    // --- encode_copy_source -------------------------------------------

    #[test]
    fn encode_copy_source_preserves_slash_between_bucket_and_key() {
        let out = encode_copy_source("my-bucket", "refs/heads/main/abc.bundle");
        assert_eq!(out, "my-bucket/refs/heads/main/abc.bundle");
    }

    #[test]
    fn encode_copy_source_encodes_hash_in_lock_keys() {
        // LOCK#.lock from the per-ref locking scheme — # is reserved.
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
            Some(ObjectStoreError::NotFound(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_403_status_is_access_denied() {
        assert!(matches!(
            classify_status_and_code(403, None, "k"),
            Some(ObjectStoreError::AccessDenied(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_412_status_is_precondition_failed() {
        assert!(matches!(
            classify_status_and_code(412, None, "k"),
            Some(ObjectStoreError::PreconditionFailed(s)) if s == "k"
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
            Some(ObjectStoreError::Conflict(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_no_such_key_code_falls_back_to_not_found() {
        assert!(matches!(
            classify_status_and_code(500, Some("NoSuchKey"), "k"),
            Some(ObjectStoreError::NotFound(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_conditional_request_conflict_code_is_conflict() {
        assert!(matches!(
            classify_status_and_code(500, Some("ConditionalRequestConflict"), "k"),
            Some(ObjectStoreError::Conflict(s)) if s == "k"
        ));
    }

    #[test]
    fn classify_entity_too_large_code_is_payload_too_large() {
        // S3 returns HTTP 400 + `EntityTooLarge` when a single-PUT body
        // exceeds 5 GiB. Status 400 alone is too broad; route via code.
        assert!(matches!(
            classify_status_and_code(400, Some("EntityTooLarge"), "k"),
            Some(ObjectStoreError::PayloadTooLarge { limit_bytes })
                if limit_bytes == SINGLE_PUT_LIMIT_BYTES
        ));
    }

    #[test]
    fn classify_413_status_is_payload_too_large() {
        // Front-door / proxy paths can surface HTTP 413 directly even
        // when the canonical S3 response is 400; treat 413 the same.
        assert!(matches!(
            classify_status_and_code(413, None, "k"),
            Some(ObjectStoreError::PayloadTooLarge { limit_bytes })
                if limit_bytes == SINGLE_PUT_LIMIT_BYTES
        ));
    }

    #[test]
    fn classify_unrecognised_returns_none() {
        assert!(classify_status_and_code(500, Some("InternalError"), "k").is_none());
        assert!(classify_status_and_code(500, None, "k").is_none());
        // Plain 400 with no recognised code stays in `Other` so callers
        // see the SDK chain rather than a misleading PayloadTooLarge.
        assert!(classify_status_and_code(400, None, "k").is_none());
        assert!(classify_status_and_code(400, Some("MalformedXML"), "k").is_none());
    }

    // --- from_remote_url constructor branch ---------------------------

    fn azure_url() -> RemoteUrl {
        RemoteUrl::Azure {
            endpoint: parse_endpoint("https://acct.blob.core.windows.net/container"),
            account: "acct".to_owned(),
            container: "container".to_owned(),
            prefix: None,
            addressing: AzureAddressing::VirtualHosted,
            flags: RemoteFlags::default(),
        }
    }

    #[tokio::test]
    async fn from_remote_url_rejects_azure() {
        let result = S3Store::from_remote_url(&azure_url()).await;
        match result {
            Err(ObjectStoreError::Other(_)) => {}
            Err(other) => panic!("expected ObjectStoreError::Other, got {other:?}"),
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
        //
        // Coverage scope: this test catches a panic during
        // `Builder::build_https()` construction (e.g. a missing TLS
        // provider feature), but does NOT catch a regression that
        // silently drops `.http_client(...)` from the loader chain —
        // that call is optional, so removing it still compiles and
        // returns a config. The constant-pin test below guards the
        // value; only an integration test against a real server with
        // observable connection-pool timing would catch a regression
        // in the wiring itself.
        let endpoint = parse_endpoint("http://127.0.0.1:9000/my-bucket");
        let resolved =
            ResolvedS3Config::from_url_parts(&endpoint, S3Addressing::PathStyle, None, None)
                .expect("resolves");
        let _config = build_s3_config(&resolved).await;
    }

    /// Pin the timeout values. A future copy-paste mistake
    /// (`from_millis` instead of `from_secs`, an accidental zero)
    /// silently disables the very behaviour the constants exist for;
    /// fail fast instead. If a constant is deliberately changed,
    /// update the expected value on the right-hand side together —
    /// the test exists to make such a change deliberate, not to lock
    /// the value forever. See the matching Azure-side test for the
    /// same rationale.
    #[test]
    fn timeout_constants_have_expected_values() {
        assert_eq!(POOL_IDLE_TIMEOUT, Duration::from_secs(30));
        assert_eq!(READ_TIMEOUT, Duration::from_secs(30));
    }

    /// Pin the `should_use_multipart` predicate at and around the
    /// shared threshold (issue #53).
    ///
    /// `put_bytes`, `put_path`, and `copy` route through this
    /// predicate to decide single-PUT vs multipart. The integration
    /// tests `multipart_put_emits_per_part_progress_events` and the
    /// env-gated `multipart_put_path_above_5_gib_round_trips` cover
    /// the dispatch *call* (only multipart emits >= 2 progress
    /// events; only multipart succeeds above 5 GiB); this unit test
    /// pins the predicate's boundary semantics so the constant can't
    /// be moved out from under those tests without something failing.
    #[test]
    fn should_use_multipart_pins_threshold_boundary() {
        use super::super::multipart::MULTIPART_PUT_THRESHOLD;
        assert!(!should_use_multipart(MULTIPART_PUT_THRESHOLD - 1));
        assert!(should_use_multipart(MULTIPART_PUT_THRESHOLD));
        assert!(should_use_multipart(MULTIPART_PUT_THRESHOLD + 1));
        // A 6 GiB body must take the multipart path: this is the
        // failure mode named in the issue (`EntityTooLarge` on bare
        // `PutObject`).
        assert!(should_use_multipart(6 * (1 << 30)));
    }

    /// Tripwire for the `disable_read_timeout()` fix in commit bfec2f4.
    ///
    /// `put_body` overrides the SDK timeout config per-operation so
    /// large bundle uploads are not aborted at [`READ_TIMEOUT`].
    /// A regression that drops `.disable_read_timeout()` from the
    /// override (e.g. a bare `TimeoutConfig::builder().build()`) would
    /// re-introduce the upload-abort bug silently.
    ///
    /// `TimeoutConfig` does not expose the per-field state via getters
    /// (both `Unset` and `Disabled` return `None`), so the assertion
    /// uses the merge semantics: build a base config that *does* set
    /// `read_timeout`, then verify that merging via `take_defaults_from`
    /// keeps `read_timeout` disabled rather than inheriting the base.
    #[test]
    fn put_body_upload_override_disables_read_timeout() {
        let base = TimeoutConfig::builder()
            .read_timeout(Duration::from_secs(99))
            .build();

        // `upload_timeout_config()` is the function `put_body` calls.
        let mut override_cfg = upload_timeout_config();
        let merged = override_cfg.take_defaults_from(&base);

        // If `disable_read_timeout()` is in place, the merged config
        // returns `None` (Disabled). If a regression dropped it, the
        // merged config would inherit `Some(99s)` from the base.
        assert_eq!(
            merged.read_timeout(),
            None,
            "upload override must disable read_timeout, not just leave it Unset",
        );
    }

    // --- MultipartUploadGuard (#169, #171) ----------------------------

    /// Build a no-network `Client` for guard tests. The Client is
    /// only used to construct guards; no requests are issued from
    /// within these tests (we never let an armed guard's spawned
    /// abort actually reach the SDK).
    fn test_client() -> aws_sdk_s3::Client {
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url("http://127.0.0.1:1/")
            .build();
        aws_sdk_s3::Client::from_conf(conf)
    }

    fn make_guard() -> MultipartUploadGuard {
        MultipartUploadGuard::new(
            test_client(),
            "bkt".to_owned(),
            "k".to_owned(),
            "uid".to_owned(),
        )
    }

    #[test]
    fn multipart_upload_guard_exposes_constructor_fields() {
        // The accessors are load-bearing: `finish_multipart_upload`
        // reads them to address the complete/abort calls.
        let mut guard = make_guard();
        assert_eq!(guard.key(), "k");
        assert_eq!(guard.upload_id(), "uid");
        // Disarm so Drop is a no-op (the constructor-fields test
        // should not exercise the spawn-on-Drop path).
        guard.disarm();
    }

    #[test]
    fn multipart_upload_guard_disarmed_drop_outside_runtime_is_silent() {
        // Observable contract: a disarmed guard must Drop without
        // attempting `Handle::try_current()` or `spawn`. We exercise
        // this outside any tokio runtime — `spawn` would panic there
        // and a `try_current()` lookup would warn-log. Neither must
        // happen on the success path.
        let mut guard = make_guard();
        guard.disarm();
        drop(guard);
    }

    #[test]
    fn multipart_upload_guard_armed_drop_outside_runtime_does_not_panic() {
        // Project rule: Drop must never panic. When dropped armed
        // outside any tokio runtime, the guard logs a warn and
        // returns cleanly rather than panicking on a missing
        // runtime handle.
        let guard = make_guard();
        drop(guard);
    }

    #[tokio::test]
    async fn multipart_upload_guard_armed_drop_inside_runtime_spawns_abort_task() {
        // Production failure mode: a future dropped between
        // `start_multipart_upload` and `finish_multipart_upload`
        // must dispatch `AbortMultipartUpload` on a detached
        // tokio task. This test pins the cheap observable contract:
        //
        // 1. Drop neither panics nor blocks.
        // 2. The spawned abort task makes forward progress (the
        //    unreachable endpoint `127.0.0.1:1` forces a connect
        //    failure inside `send().await`, exercising the spawned
        //    closure's `Err`-arm warn-log).
        //
        // The companion test below
        // (`..._drop_issues_abort_multipart_upload`) goes further
        // and byte-equality-checks the captured HTTP request, so
        // this test only needs to keep the no-panic / forward-
        // progress contract on the warn-log path.
        //
        // Yielding `JoinSet`-style would let us join the task and
        // observe completion, but Drop uses a detached `spawn` by
        // design (a `JoinHandle` would require Drop to hold state
        // for the lifetime of the runtime). Yielding the runtime
        // a handful of times lets the spawned task reach its
        // `send().await` before the test returns and the runtime
        // tears down.
        let guard = make_guard();
        drop(guard);
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }

    /// Build an `aws_sdk_s3::Client` whose HTTP layer is the
    /// smithy `capture_request` handler — every request the SDK
    /// emits is recorded on the returned `CaptureRequestReceiver`
    /// instead of touching the network. Static test credentials
    /// keep `SigV4` happy so the SDK actually reaches the HTTP
    /// layer (an unsigned chain would short-circuit earlier).
    fn capture_client() -> (
        aws_sdk_s3::Client,
        aws_smithy_http_client::test_util::CaptureRequestReceiver,
    ) {
        use aws_sdk_s3::config::Credentials;
        let (http_client, rx) = aws_smithy_http_client::test_util::capture_request(None);
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::new(
                "AKIAIOSFODNN7EXAMPLE",
                "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                None,
                None,
                "test",
            ))
            .http_client(http_client)
            .force_path_style(true)
            .build();
        (aws_sdk_s3::Client::from_conf(conf), rx)
    }

    #[tokio::test]
    async fn multipart_upload_guard_drop_issues_abort_multipart_upload() {
        // Byte-equality contract for issue #173: when an armed
        // `MultipartUploadGuard` is dropped inside a tokio runtime,
        // the detached abort task must issue an
        // `AbortMultipartUpload` request addressing the exact
        // (bucket, key, upload-id) the guard was constructed with.
        //
        // S3's wire form for `AbortMultipartUpload` is
        //   `DELETE /<bucket>/<key>?uploadId=<id>` (path style)
        // so we assert: method=DELETE, the captured URI contains
        // the key, and `uploadId=<id>` appears in the query.
        // We deliberately do NOT couple to the exact host, scheme,
        // or full URL — those are SDK-internal details unrelated
        // to the abort contract.
        let (client, rx) = capture_client();
        let guard = MultipartUploadGuard::new(
            client,
            "test-bucket".to_owned(),
            "test/key.pack".to_owned(),
            "test-upload-id-abc123".to_owned(),
        );
        drop(guard);

        // The detached `tokio::spawn` task must run far enough to
        // submit the request through the capture client. Yielding
        // a handful of times lets the SDK's signing + request
        // pipeline complete; `capture_request` resolves the call
        // synchronously once it sees the request, so no real
        // network wait is needed.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }

        let request = rx.expect_request();
        assert_eq!(
            request.method(),
            "DELETE",
            "AbortMultipartUpload must be DELETE; got {}",
            request.method(),
        );
        let uri = request.uri();
        assert!(
            uri.contains("test/key.pack"),
            "captured URI must address the guard's key; got {uri}",
        );
        assert!(
            uri.contains("uploadId=test-upload-id-abc123"),
            "captured URI must carry the guard's upload-id in the \
             query string; got {uri}",
        );
    }
}

//! Backend-neutral object-store trait shared by the S3 and Azure Blob
//! implementations.
//!
//! The trait, value types, and in-memory mock live here; the concrete
//! S3 and Azure backends are in the sibling modules.
//!
//! Trait dispatch is intended for `Arc<dyn ObjectStore>` so the
//! protocol REPL can drive either backend without monomorphisation.
//! Async methods are routed through [`async_trait`] so
//! `dyn ObjectStore + Send + Sync` composes cleanly — native
//! `async fn`-in-trait would require per-method `Send` bounds that
//! don't survive `dyn`.

pub mod azure;
pub mod error;
pub(crate) mod multipart;
pub mod s3;

#[cfg(any(test, feature = "test-util"))]
pub mod mock;

#[cfg(test)]
pub(crate) mod test_support;

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use tempfile::NamedTempFile;
use time::OffsetDateTime;
use tracing::warn;

use self::error::other_boxed;
pub use self::error::{BoxError, ObjectStoreError};

/// Progress callback invoked by streaming put/get operations.
///
/// `report(bytes_just_transferred)` fires at chunk boundaries — one
/// event per completed part / block on multipart uploads (both S3 and
/// Azure above [`multipart::MULTIPART_PUT_THRESHOLD`]; issue #53), one
/// event per ranged GET on multipart downloads, one event per body
/// chunk on small-object reads, and a single end-of-transfer event on
/// single-PUT uploads. Callers accumulate `bytes_so_far` themselves.
///
/// The callback runs on the backend's task and may be invoked from a
/// spawned worker, so it must be cheap and non-blocking. The LFS agent
/// forwards `report` calls into an `mpsc` channel that the agent drains
/// into protocol `progress` events.
#[derive(Clone)]
pub struct ProgressSink(Arc<dyn Fn(u64) + Send + Sync>);

impl ProgressSink {
    /// Build a sink from any cheap, thread-safe callback.
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        Self(Arc::new(f))
    }

    /// Report `bytes_amount` newly transferred bytes.
    pub fn report(&self, bytes_amount: u64) {
        (self.0)(bytes_amount);
    }
}

impl std::fmt::Debug for ProgressSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressSink").finish_non_exhaustive()
    }
}

/// Atomically rename a [`NamedTempFile`] to `dest`, mapping the
/// [`tempfile::PersistError`] into [`ObjectStoreError::Other`].
///
/// Shared between the S3 and Azure backends — both write `get_to_file`
/// results to a sibling tempfile and persist on success so a partial
/// download cannot leave a corrupt destination for the unbundle step.
pub(crate) fn persist_temp(temp: NamedTempFile, dest: &Path) -> Result<(), ObjectStoreError> {
    temp.persist(dest)
        .map_err(|e| ObjectStoreError::Other(Box::new(e.error)))?;
    Ok(())
}

/// Metadata returned by `list` and `head`.
///
/// `key` is the full backend key (the prefix passed to `list` is included);
/// `last_modified` is the server-side wall clock, used by stale-lock
/// recovery in the push path.
#[derive(Debug, Clone)]
pub struct ObjectMeta {
    /// Full key of the stored object.
    pub key: String,
    /// Body length in bytes.
    pub size: u64,
    /// Server-side last-modified timestamp.
    pub last_modified: OffsetDateTime,
    /// Opaque entity-tag returned by `HEAD` / `GET`. S3 returns a
    /// quoted MD5 (e.g. `"d41d8…"`); Azure returns a similar `ETag`.
    /// `None` when the backend does not expose one (e.g. `list` results
    /// on some backends omit it).
    pub etag: Option<String>,
}

/// Optional `put_bytes` / `put_path` knobs.
///
/// `content_disposition` and `user_metadata` are populated only by the
/// zip-archive push path, which supplies `Content-Disposition` and the
/// `codepipeline-artifact-revision-summary` user metadata. `progress`
/// is populated by the LFS agent so long uploads can drive the
/// `git-lfs` progress bar; left `None` for bundle / lock / HEAD writes
/// where progress reporting is not useful. Defaults to "no extras",
/// which covers every other write.
#[derive(Debug, Clone, Default)]
pub struct PutOpts {
    /// HTTP `Content-Disposition` header to associate with the object.
    pub content_disposition: Option<String>,
    /// Backend user-defined metadata (key/value pairs). Backends should
    /// preserve insertion order; key case-folding is backend-defined.
    pub user_metadata: Vec<(String, String)>,
    /// Optional progress sink invoked at chunk boundaries during the
    /// upload. Backends that do single-shot uploads (small bodies)
    /// emit one `report(size)` call after the transfer completes.
    pub progress: Option<ProgressSink>,
}

/// Optional `get_to_file` knobs.
///
/// `progress` is populated by the LFS agent (the only consumer that
/// needs live download progress); bundle fetches leave it `None`.
#[derive(Debug, Clone, Default)]
pub struct GetOpts {
    /// Optional progress sink invoked at chunk boundaries during the
    /// download. Multipart download paths emit one `report(chunk_size)`
    /// call per completed range; the small-object path emits one
    /// `report(chunk.len())` per body chunk read off the wire.
    pub progress: Option<ProgressSink>,
}

/// Caller-side preflight for [`ObjectStore::get_bytes_range`].
///
/// Centralises the trait contract for degenerate inputs so every
/// backend impl renders the same wire-line and the same short-circuit:
///
/// - `start == end` → `Ok(Some(Bytes::new()))`; the caller returns the
///   empty body without issuing a network request.
/// - `start > end` → `Err(RangeNotSatisfiable)`; the caller propagates.
/// - otherwise → `Ok(None)`; the caller proceeds with the SDK call.
pub(crate) fn precheck_range(
    key: &str,
    range: &std::ops::Range<u64>,
) -> Result<Option<Bytes>, ObjectStoreError> {
    if range.start == range.end {
        return Ok(Some(Bytes::new()));
    }
    if range.start > range.end {
        return Err(ObjectStoreError::RangeNotSatisfiable {
            key: key.to_owned(),
            requested: range.clone(),
        });
    }
    Ok(None)
}

/// Post-flight check for [`ObjectStore::get_bytes_range`] body responses.
///
/// Real S3 and Azure backends silently truncate a ranged GET when
/// `range.start < body.len() <= range.end` — the wire response carries
/// `range.start..body.len()` bytes with HTTP 206 and no error. The
/// packchain reader treats the returned slice as the exact entry it
/// asked for, so a truncated pack file (or a stale `chain.json` whose
/// recorded offsets outrun the on-bucket pack) would propagate as
/// downstream pack-decode garbage rather than as the data-integrity
/// failure it is.
///
/// This helper asserts the SDK gave back exactly `range.end - range.start`
/// bytes and surfaces a mismatch as [`ObjectStoreError::RangeNotSatisfiable`]
/// with the originally requested range. It is the symmetric companion to
/// [`precheck_range`] — the preflight guards against degenerate inputs
/// before the SDK call; this guards against degenerate output after.
///
/// `precheck_range` short-circuits empty and inverted ranges, so by the
/// time this runs `range.start < range.end` is guaranteed. Subtraction
/// therefore cannot underflow.
pub(crate) fn verify_range_response_length(
    key: &str,
    range: &std::ops::Range<u64>,
    body: Bytes,
) -> Result<Bytes, ObjectStoreError> {
    let expected = range.end - range.start;
    let actual = body.len() as u64;
    if actual == expected {
        return Ok(body);
    }
    Err(ObjectStoreError::RangeNotSatisfiable {
        key: key.to_owned(),
        requested: range.clone(),
    })
}

/// Backend-neutral cloud object-store surface.
///
/// Method semantics — every implementation must satisfy these contracts so
/// higher layers can target the trait without backend-specific branching.
///
/// - **`list(prefix)`** — byte-prefix match (matches S3 `Prefix=`
///   semantics; `list("a")` returns `a`, `a/1`, and `aaa`). Returns full
///   keys; ordering is backend-defined.
/// - **`get_to_file(key, dest, opts)`** — caller must ensure `dest`'s
///   parent directory exists. `opts.progress`, if set, fires at chunk
///   boundaries so callers (notably the LFS agent) can render a live
///   progress bar.
/// - **`put_bytes`** — overwrites if the key already exists.
/// - **`put_path`** — streams a local file to the key, overwriting if
///   present. Default reads the file into memory; backends should
///   override for large-file streaming.
/// - **`put_if_absent`** — returns `Ok(true)` on creation, `Ok(false)` if
///   the key already existed. Backends collapse both 412
///   (`PreconditionFailed`) and 409 (`Conflict`) into `Ok(false)`;
///   transport-level failures still surface as `Err`.
/// - **`copy(src, dst)`** — overwrites `dst`; returns `Err(NotFound)` when
///   `src` is absent.
/// - **`delete`** — returns `Err(NotFound)` on missing key. `release_lock`
///   maps `NotFound` to `Ok(())` and propagates other errors.
/// - **`get_bytes_range`** — half-open `[start, end)` range. `start == end`
///   returns `Ok(Bytes::new())` with no network call. `start > end` and
///   server-side 416 both surface as
///   [`ObjectStoreError::RangeNotSatisfiable`]. When the object's body
///   ends inside the requested range
///   (`start < body.len() <= end`) real S3/Azure backends return a
///   silently truncated body (HTTP 206, fewer bytes than requested);
///   this trait elevates that mismatch to
///   [`ObjectStoreError::RangeNotSatisfiable`] so callers never see a
///   short slice masquerading as the full requested range. The mock
///   backend matches this contract.
#[async_trait::async_trait]
pub trait ObjectStore: Send + Sync {
    /// Enumerate every object whose key has `prefix` as a byte prefix.
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, ObjectStoreError>;

    /// Stream the object body to `dest`. The destination's parent
    /// directory must already exist. `opts.progress`, when set, fires
    /// at chunk boundaries with the count of bytes just received.
    async fn get_to_file(
        &self,
        key: &str,
        dest: &Path,
        opts: GetOpts,
    ) -> Result<(), ObjectStoreError>;

    /// Read the entire object body into memory.
    async fn get_bytes(&self, key: &str) -> Result<Bytes, ObjectStoreError>;

    /// Read a half-open byte range `[start, end)` of the object body.
    ///
    /// `start == end` returns `Ok(Bytes::new())` without issuing a
    /// network request. `start > end`, or a server-side 416, surfaces
    /// as [`ObjectStoreError::RangeNotSatisfiable`].
    ///
    /// **Truncation contract**: real S3 and Azure backends return a
    /// silently truncated body (HTTP 206 with fewer bytes than asked)
    /// when the requested range overruns the object's end —
    /// `start < body.len() <= end`. Backends here elevate that mismatch
    /// to [`ObjectStoreError::RangeNotSatisfiable`] so a successful
    /// `Ok(bytes)` always carries exactly `end - start` bytes. The
    /// packchain reader (issue #52) relies on this to surface stale
    /// `chain.json` offsets and truncated pack files as data-integrity
    /// errors instead of pack-decode garbage.
    ///
    /// Used by the packchain engine (issue #52) to read a single blob
    /// out of a larger pack file without downloading the whole pack.
    async fn get_bytes_range(
        &self,
        key: &str,
        range: std::ops::Range<u64>,
    ) -> Result<Bytes, ObjectStoreError>;

    /// Write `body` to `key`, overwriting any existing object.
    async fn put_bytes(
        &self,
        key: &str,
        body: Bytes,
        opts: PutOpts,
    ) -> Result<(), ObjectStoreError>;

    /// Stream a local file to `key`, overwriting any existing object.
    ///
    /// Backends should override this to stream from disk without buffering
    /// the entire file in process memory. The default implementation reads
    /// the file into memory and delegates to [`put_bytes`](Self::put_bytes);
    /// this is correct but defeats the streaming intent for large files.
    async fn put_path(&self, key: &str, src: &Path, opts: PutOpts) -> Result<(), ObjectStoreError> {
        warn!(
            key,
            path = %src.display(),
            "put_path: falling back to read-then-put_bytes; override this method to avoid \
             buffering the entire file in memory"
        );
        let body = tokio::fs::read(src).await.map_err(other_boxed)?;
        // `usize` is at most 64 bits wide, so this cast never truncates.
        let len = body.len() as u64;
        let progress = opts.progress.clone();
        // Strip progress from the inner `put_bytes` call so the sink
        // doesn't fire twice — once during put_bytes' own reporting and
        // again on our final end-of-transfer event below.
        let inner_opts = PutOpts {
            progress: None,
            ..opts
        };
        self.put_bytes(key, Bytes::from(body), inner_opts).await?;
        // Single-shot fallback emits a final progress event with the
        // full body size. Zero-byte bodies produce no progress event.
        if let Some(sink) = progress
            && len > 0
        {
            sink.report(len);
        }
        Ok(())
    }

    /// Create `key` if and only if it does not exist. Returns `Ok(true)`
    /// when the object was created, `Ok(false)` when the key was already
    /// present.
    async fn put_if_absent(&self, key: &str, body: Bytes) -> Result<bool, ObjectStoreError>;

    /// Fetch metadata for an exact key.
    async fn head(&self, key: &str) -> Result<ObjectMeta, ObjectStoreError>;

    /// Copy `src` to `dst`. The body is preserved on every backend;
    /// user metadata is **not** guaranteed to survive — callers must not
    /// rely on metadata round-tripping through `copy`.
    ///
    /// The trait's only in-tree consumer is `Doctor::evict_losing_bundle`,
    /// which carries no user metadata on bundle objects.
    async fn copy(&self, src: &str, dst: &str) -> Result<(), ObjectStoreError>;

    /// Delete `key`. Returns `Err(ObjectStoreError::NotFound)` if the key was
    /// not present.
    async fn delete(&self, key: &str) -> Result<(), ObjectStoreError>;

    /// Build a presigned (short-lived, signed) URL granting GET access
    /// to `key` for `ttl`. Used by the `bundle-uri` capability
    /// ([`crate::protocol::bundle_uri`]) to advertise time-limited
    /// download URLs against private buckets.
    ///
    /// The default impl returns
    /// [`ObjectStoreError::Unsupported`] so backends that have no
    /// presigning model (e.g. `MockStore` in tests, or
    /// `AzureStore` configured with a `TokenCredential` rather than
    /// a shared account key) inherit a clean "not supported" error
    /// without needing a stub.
    ///
    /// # Errors
    ///
    /// Returns [`ObjectStoreError::Unsupported`] when the backend
    /// cannot produce signed URLs (default).
    /// Returns [`ObjectStoreError::Other`] when the backend supports
    /// presigning but the SDK rejects the TTL (e.g. AWS's 7-day
    /// ceiling).
    async fn presigned_get_url(
        &self,
        key: &str,
        ttl: std::time::Duration,
    ) -> Result<String, ObjectStoreError> {
        let _ = (key, ttl);
        Err(ObjectStoreError::Unsupported(
            "presigned URLs are not supported by this backend".to_owned(),
        ))
    }
}

/// Blanket impl so `&Arc<T>` coerces to `&dyn ObjectStore` and so
/// `Arc<T>` is itself usable as an `ObjectStore` value.
///
/// `T: ObjectStore + ?Sized` covers both concrete types (`Arc<S3Store>`,
/// `Arc<MockStore>`) and erased trait objects (`Arc<dyn ObjectStore>`).
/// Every method forwards to the inner `T` through the `Deref` impl.
///
/// Verified non-removable: dropping this impl breaks call sites that
/// pass `&Arc<dyn ObjectStore>` to a function taking `&dyn ObjectStore`.
/// Plain `Arc::deref` lets `arc.list()` work via Rust's auto-deref on
/// method calls, but the *coercion* `&Arc<T>` → `&dyn ObjectStore`
/// requires `Arc<T>` itself to implement the trait — and that is what
/// this impl provides.
#[async_trait::async_trait]
impl<T: ObjectStore + ?Sized> ObjectStore for Arc<T> {
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, ObjectStoreError> {
        (**self).list(prefix).await
    }

    async fn get_to_file(
        &self,
        key: &str,
        dest: &Path,
        opts: GetOpts,
    ) -> Result<(), ObjectStoreError> {
        (**self).get_to_file(key, dest, opts).await
    }

    async fn get_bytes(&self, key: &str) -> Result<Bytes, ObjectStoreError> {
        (**self).get_bytes(key).await
    }

    async fn get_bytes_range(
        &self,
        key: &str,
        range: std::ops::Range<u64>,
    ) -> Result<Bytes, ObjectStoreError> {
        (**self).get_bytes_range(key, range).await
    }

    async fn put_bytes(
        &self,
        key: &str,
        body: Bytes,
        opts: PutOpts,
    ) -> Result<(), ObjectStoreError> {
        (**self).put_bytes(key, body, opts).await
    }

    async fn put_path(&self, key: &str, src: &Path, opts: PutOpts) -> Result<(), ObjectStoreError> {
        (**self).put_path(key, src, opts).await
    }

    async fn put_if_absent(&self, key: &str, body: Bytes) -> Result<bool, ObjectStoreError> {
        (**self).put_if_absent(key, body).await
    }

    async fn head(&self, key: &str) -> Result<ObjectMeta, ObjectStoreError> {
        (**self).head(key).await
    }

    async fn copy(&self, src: &str, dst: &str) -> Result<(), ObjectStoreError> {
        (**self).copy(src, dst).await
    }

    async fn delete(&self, key: &str) -> Result<(), ObjectStoreError> {
        (**self).delete(key).await
    }

    async fn presigned_get_url(
        &self,
        key: &str,
        ttl: std::time::Duration,
    ) -> Result<String, ObjectStoreError> {
        (**self).presigned_get_url(key, ttl).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precheck_range_empty_short_circuits_with_empty_bytes() {
        let out = precheck_range("k", &(5..5)).expect("empty range is valid");
        let bytes = out.expect("empty range short-circuits with Some");
        assert!(bytes.is_empty());
    }

    #[test]
    fn precheck_range_inverted_returns_range_not_satisfiable() {
        let range = std::ops::Range { start: 7, end: 3 };
        let err = precheck_range("k", &range).expect_err("inverted range must error");
        assert!(matches!(
            err,
            ObjectStoreError::RangeNotSatisfiable {
                ref key,
                requested: ref r,
            } if key == "k" && r.start == 7 && r.end == 3
        ));
    }

    #[test]
    fn precheck_range_well_formed_returns_none() {
        let out = precheck_range("k", &(2..6)).expect("valid range");
        assert!(out.is_none(), "well-formed range proceeds to SDK call");
    }

    #[test]
    fn verify_range_response_length_passes_exact_length() {
        let range = 2..6;
        let body = Bytes::from_static(b"abcd"); // 4 bytes, matches range
        let out = verify_range_response_length("k", &range, body.clone())
            .expect("exact-length body must pass");
        assert_eq!(out, body);
    }

    #[test]
    fn verify_range_response_length_rejects_truncated_body() {
        // S3/Azure return start..body.len() when start < body.len() <= end.
        // Caller asked for 4 bytes; SDK returned 2.
        let range = 2..6;
        let body = Bytes::from_static(b"ab");
        let err = verify_range_response_length("pack-key", &range, body)
            .expect_err("short body must be rejected");
        assert!(
            matches!(
                err,
                ObjectStoreError::RangeNotSatisfiable {
                    ref key,
                    requested: ref r,
                } if key == "pack-key" && r.start == 2 && r.end == 6,
            ),
            "expected RangeNotSatisfiable(pack-key, 2..6), got {err:?}"
        );
    }

    #[test]
    fn verify_range_response_length_rejects_overlong_body() {
        // Defensive: a hypothetical SDK bug that returned MORE bytes than
        // requested would equally let downstream callers consume corrupt
        // data. Treat as the same mismatch.
        let range = 0..4;
        let body = Bytes::from_static(b"abcdef");
        let err = verify_range_response_length("k", &range, body)
            .expect_err("overlong body must be rejected");
        assert!(matches!(err, ObjectStoreError::RangeNotSatisfiable { .. }));
    }

    #[test]
    fn verify_range_response_length_single_byte_round_trip() {
        // 1-byte range is the smallest non-degenerate case (precheck
        // already rejected 0-byte and inverted). Locks in that the
        // single-byte path has no off-by-one.
        let range = 7..8;
        let body = Bytes::from_static(b"x");
        let out = verify_range_response_length("k", &range, body.clone())
            .expect("single-byte body must pass");
        assert_eq!(out, body);
    }
}

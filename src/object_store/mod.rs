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
pub mod s3;

#[cfg(any(test, feature = "test-util"))]
pub mod mock;

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
/// `report(bytes_just_transferred)` fires at chunk boundaries — each
/// multipart-upload part for `put_path`, each ranged GET / chunk read
/// for `get_to_file`. Callers accumulate `bytes_so_far` themselves.
/// Matches upstream `ProgressPercentage.__call__` in
/// `../git-remote-s3/git_remote_s3/lfs.py:25-41` (one event per network
/// chunk).
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
/// zip-archive push path (`../git-remote-s3/git_remote_s3/remote.py:275-281`),
/// where upstream supplies `Content-Disposition` and the
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
}

/// Blanket impl so `Arc<T>` is usable wherever `&dyn ObjectStore` is
/// expected, without callers having to dereference explicitly.
///
/// `T: ObjectStore + ?Sized` covers both concrete types (`Arc<S3Store>`)
/// and erased trait objects (`Arc<dyn ObjectStore>`). Every method simply
/// forwards to the inner `T` through the `Deref` impl.
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
}

//! Backend-neutral object-store trait shared by the S3 and Azure Blob
//! implementations.
//!
//! See `execution-plan.md` §2.1 for the trait sketch and §2.3 / §5.1 for
//! the error mapping rationale. Phase 4 lands the trait, value types, and
//! an in-memory mock; Phase 5 fills in `s3.rs` and Phase 11 fills in
//! `azure.rs`.
//!
//! Trait dispatch is intended for `Arc<dyn ObjectStore>` so the protocol
//! REPL (Phase 6) can drive either backend without monomorphisation. Async
//! methods are routed through [`async_trait`] so `dyn ObjectStore + Send +
//! Sync` composes cleanly — native `async fn`-in-trait would require
//! per-method `Send` bounds that don't survive `dyn`.

pub mod azure;
pub mod error;
pub mod s3;

#[cfg(any(test, feature = "test-util"))]
pub mod mock;

use std::path::Path;

use bytes::Bytes;
use tempfile::NamedTempFile;
use time::OffsetDateTime;
use tracing::debug;

use self::error::other_boxed;
pub use self::error::{BoxError, ObjectStoreError};

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
/// `last_modified` is the server-side wall clock, used by Phase 8's
/// stale-lock recovery (`execution-plan.md` §1.1 / §5.2).
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

/// Optional `put_bytes` knobs.
///
/// Both fields are populated only by the zip-archive push path
/// (`../git-remote-s3/git_remote_s3/remote.py:275-281`), where upstream
/// supplies `Content-Disposition` and the
/// `codepipeline-artifact-revision-summary` user metadata. Defaults to
/// "no extras", which covers every other write.
#[derive(Debug, Clone, Default)]
pub struct PutOpts {
    /// HTTP `Content-Disposition` header to associate with the object.
    pub content_disposition: Option<String>,
    /// Backend user-defined metadata (key/value pairs). Backends should
    /// preserve insertion order; key case-folding is backend-defined.
    pub user_metadata: Vec<(String, String)>,
}

/// Backend-neutral cloud object-store surface.
///
/// Method semantics — every implementation must satisfy these contracts so
/// higher layers can target the trait without backend-specific branching.
///
/// - **`list(prefix)`** — byte-prefix match (matches S3 `Prefix=`
///   semantics; `list("a")` returns `a`, `a/1`, and `aaa`). Returns full
///   keys; ordering is backend-defined.
/// - **`get_to_file(key, dest)`** — caller must ensure `dest`'s parent
///   directory exists.
/// - **`put_bytes`** — overwrites if the key already exists.
/// - **`put_path`** — streams a local file to the key, overwriting if
///   present. Default reads the file into memory; backends should
///   override for large-file streaming.
/// - **`put_if_absent`** — returns `Ok(true)` on creation, `Ok(false)` if
///   the key already existed. Backends collapse both 412
///   (`PreconditionFailed`) and 409 (`Conflict`) into `Ok(false)` per
///   `execution-plan.md` §5.1; transport-level failures still surface as
///   `Err`.
/// - **`copy(src, dst)`** — overwrites `dst`; returns `Err(NotFound)` when
///   `src` is absent.
/// - **`delete`** — returns `Err(NotFound)` on missing key. `release_lock`
///   maps `NotFound` to `Ok(())` and propagates other errors.
#[async_trait::async_trait]
pub trait ObjectStore: Send + Sync {
    /// Enumerate every object whose key has `prefix` as a byte prefix.
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, ObjectStoreError>;

    /// Stream the object body to `dest`. The destination's parent
    /// directory must already exist.
    async fn get_to_file(&self, key: &str, dest: &Path) -> Result<(), ObjectStoreError>;

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
        debug!(key, path = %src.display(), "put_path: default read-then-put_bytes fallback");
        let body = tokio::fs::read(src).await.map_err(other_boxed)?;
        self.put_bytes(key, Bytes::from(body), opts).await
    }

    /// Create `key` if and only if it does not exist. Returns `Ok(true)`
    /// when the object was created, `Ok(false)` when the key was already
    /// present.
    async fn put_if_absent(&self, key: &str, body: Bytes) -> Result<bool, ObjectStoreError>;

    /// Fetch metadata for an exact key.
    async fn head(&self, key: &str) -> Result<ObjectMeta, ObjectStoreError>;

    /// Copy `src` to `dst`. The body is preserved on every backend.
    ///
    /// User metadata propagation is **best-effort**: backends that
    /// implement copy as a true server-side operation
    /// (`S3Store::copy` via `CopyObject`) do propagate it, but backends
    /// that emulate copy via download-then-upload (`AzureStore::copy`,
    /// because `azure_storage_blob` 0.12 does not ergonomically expose
    /// `Copy Blob` with shared-key auth) currently drop it. Callers
    /// must not depend on metadata round-tripping through `copy`. The
    /// trait's only in-tree consumer is `Doctor::evict_losing_bundle`,
    /// which carries no user metadata on bundle objects.
    async fn copy(&self, src: &str, dst: &str) -> Result<(), ObjectStoreError>;

    /// Delete `key`. Returns `Err(ObjectStoreError::NotFound)` if the key was
    /// not present.
    async fn delete(&self, key: &str) -> Result<(), ObjectStoreError>;
}

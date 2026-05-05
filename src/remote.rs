//! High-level handle to a git-remote-object-store repository.
//!
//! # Entry point
//!
//! [`Remote`] is the primary library entry point for external consumers.
//! It wraps an [`ObjectStore`] and the repository-level key prefix from the
//! [`RemoteUrl`], so callers never need to track the prefix separately or
//! know the internal key layout.
//!
//! # On-bucket key layout
//!
//! Objects are stored under `<prefix>/<suffix>` (where `<prefix>` is the
//! path component of the URL and may be empty for bucket-root repositories):
//!
//! | Suffix | Purpose |
//! |--------|---------|
//! | `HEAD` | Ref pointer for the default branch |
//! | `refs/heads/<branch>/<sha>.bundle` | Git bundle for a branch commit |
//! | `PROTECTED#` | Branch-protection sentinel |
//! | `LOCK#.lock` | Push-lock file |
//! | `lfs/<oid>` | Git LFS object |
//!
//! Use [`Remote::key`] to build correctly-prefixed keys, then call methods
//! on [`Remote::store`] directly for operations not covered by the helper
//! methods.
//!
//! # Example
//!
//! ```no_run
//! # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use git_remote_object_store::Remote;
//!
//! let remote = Remote::connect("s3+https://my-bucket.s3.us-east-1.amazonaws.com/my-repo").await?;
//!
//! // Read the HEAD ref
//! let head = remote.get_head().await?;
//! println!("{}", String::from_utf8_lossy(&head));
//!
//! // List all objects on a branch
//! let metas = remote.list("refs/heads/main/").await?;
//! for meta in metas {
//!     println!("{} ({} bytes)", meta.key, meta.size);
//! }
//!
//! // Direct store access for advanced operations
//! let data = remote.store().get_bytes(&remote.key("LOCK#.lock")).await?;
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use bytes::Bytes;

use crate::object_store::{ObjectMeta, ObjectStore, ObjectStoreError, PutOpts};
use crate::protocol::backend::{self, BackendError};
use crate::url::{ParseError, RemoteUrl, StorageEngine};

/// A handle to a git-remote-object-store repository in a cloud backend.
///
/// See the [module documentation][self] for the on-bucket key layout and
/// usage examples.
pub struct Remote {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    engine: StorageEngine,
}

impl Remote {
    /// Parse `url_str` and open a connection to the backing cloud store.
    ///
    /// Runs an eager probe (a low-cost listing call) to verify connectivity
    /// and surface auth failures early. Requires a Tokio runtime — call this
    /// inside `#[tokio::main]` or an equivalent async context.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteError::Url`] when the URL is not a recognised scheme,
    /// and [`RemoteError::Backend`] when the backend is unreachable (missing
    /// bucket/container, insufficient permissions, invalid credentials).
    pub async fn connect(url_str: &str) -> Result<Self, RemoteError> {
        let url = url_str.parse::<RemoteUrl>()?;
        Ok(Self::open(&url).await?)
    }

    /// Open a connection from an already-parsed [`RemoteUrl`].
    ///
    /// Prefer [`connect`](Self::connect) when starting from a string.
    /// Use this variant when you need to inspect or route on the URL before
    /// connecting.
    ///
    /// # Errors
    ///
    /// Returns [`BackendError`] when the backend is unreachable.
    pub async fn open(url: &RemoteUrl) -> Result<Self, BackendError> {
        let (store, engine) = backend::build(url).await?;
        let prefix = url.prefix().unwrap_or_default().to_owned();
        Ok(Self {
            store,
            prefix,
            engine,
        })
    }

    /// Compute the storage key for `suffix` within this repository's prefix.
    ///
    /// Use this to construct keys for direct [`store`](Self::store) operations.
    ///
    /// For a repository at `s3+https://bucket/my-repo`:
    /// - `remote.key("HEAD")` → `"my-repo/HEAD"`
    /// - `remote.key("refs/heads/main/")` → `"my-repo/refs/heads/main/"`
    ///
    /// For a repository at `s3+https://bucket` (no prefix):
    /// - `remote.key("HEAD")` → `"HEAD"`
    #[must_use]
    pub fn key(&self, suffix: &str) -> String {
        crate::keys::join(&self.prefix, suffix)
    }

    /// The underlying [`ObjectStore`] for direct get/put operations.
    ///
    /// Combine with [`key`](Self::key) to target the correct storage path:
    ///
    /// ```no_run
    /// # #[tokio::main] async fn main() -> Result<(), git_remote_object_store::ObjectStoreError> {
    /// # use git_remote_object_store::Remote;
    /// # let remote: Remote = todo!();
    /// let metas = remote.store().list(&remote.key("refs/heads/main/")).await?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn store(&self) -> &dyn ObjectStore {
        &*self.store
    }

    /// Test-only constructor that lets integration tests build a
    /// [`Remote`] against an in-memory [`crate::object_store::mock::MockStore`]
    /// without going through `backend::build` (which would attempt a
    /// live probe). Production callers must use [`Self::connect`] /
    /// [`Self::open`].
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn new_for_test(
        store: Arc<dyn ObjectStore>,
        prefix: impl Into<String>,
        engine: StorageEngine,
    ) -> Self {
        Self {
            store,
            prefix: prefix.into(),
            engine,
        }
    }

    /// The repository prefix (empty string for bucket-root repositories).
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The storage engine resolved at [`open`](Self::open) time from the
    /// `FORMAT` key combined with any `?engine=` URL parameter.
    ///
    /// Callers that target engine-specific APIs (notably
    /// [`crate::packchain::read_blob`]) inspect this to fail fast against
    /// a remote of the wrong shape rather than blindly fetching the
    /// engine-specific manifest keys.
    #[must_use]
    pub fn engine(&self) -> StorageEngine {
        self.engine
    }

    /// Read the repository's `HEAD` ref.
    ///
    /// # Errors
    ///
    /// Returns [`ObjectStoreError::NotFound`] when no `HEAD` object exists.
    pub async fn get_head(&self) -> Result<Bytes, ObjectStoreError> {
        self.store.get_bytes(&self.key("HEAD")).await
    }

    /// Write the repository's `HEAD` ref.
    ///
    /// # Errors
    ///
    /// Returns [`ObjectStoreError`] on backend write failure (auth, network, etc.).
    pub async fn put_head(&self, content: Bytes) -> Result<(), ObjectStoreError> {
        self.store
            .put_bytes(&self.key("HEAD"), content, PutOpts::default())
            .await
    }

    /// List all objects whose storage key starts with `<prefix>/<suffix>`.
    ///
    /// Pass `""` to list everything in the repository.
    /// Pass `"refs/heads/main/"` to list all bundles on that branch.
    /// Pass `"refs/"` to list all ref objects.
    ///
    /// # Errors
    ///
    /// Returns [`ObjectStoreError`] on backend list failure (auth, network, etc.).
    pub async fn list(&self, suffix: &str) -> Result<Vec<ObjectMeta>, ObjectStoreError> {
        self.store.list(&self.key(suffix)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_remote(prefix: &str) -> Remote {
        Remote {
            store: Arc::new(crate::object_store::mock::MockStore::new()),
            prefix: prefix.to_owned(),
            engine: StorageEngine::Bundle,
        }
    }

    #[test]
    fn key_with_prefix_joins_with_slash() {
        let remote = make_remote("my-repo");
        assert_eq!(remote.key("HEAD"), "my-repo/HEAD");
        assert_eq!(remote.key("refs/heads/main/"), "my-repo/refs/heads/main/");
    }

    #[test]
    fn key_without_prefix_returns_suffix_only() {
        let remote = make_remote("");
        assert_eq!(remote.key("HEAD"), "HEAD");
        assert_eq!(remote.key("refs/heads/main/"), "refs/heads/main/");
    }

    #[test]
    fn prefix_reflects_construction_value() {
        assert_eq!(make_remote("my-repo").prefix(), "my-repo");
        assert_eq!(make_remote("").prefix(), "");
    }
}

/// Error returned by [`Remote::connect`].
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The URL string was not a recognised scheme or was malformed.
    #[error(transparent)]
    Url(#[from] ParseError),
    /// The backend was unreachable (auth failure, missing bucket/container, etc.).
    #[error(transparent)]
    Backend(#[from] BackendError),
}

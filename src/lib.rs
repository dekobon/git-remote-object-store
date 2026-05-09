//! `git-remote-object-store` — a Rust library and CLI for storing Git
//! repositories in cloud object stores (AWS S3 and Azure Blob Storage).
//!
//! # Library usage
//!
//! This crate provides the [`Remote`] struct as the primary entry point for
//! library consumers. It wraps an [`ObjectStore`] backend together with the
//! repository-level key prefix, so you can read and write objects in the
//! project's on-bucket format without tracking these separately:
//!
//! ```no_run
//! # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use git_remote_object_store::Remote;
//!
//! let remote = Remote::connect("s3+https://my-bucket.s3.us-east-1.amazonaws.com/my-repo").await?;
//! let head = remote.get_head().await?;
//! println!("{}", String::from_utf8_lossy(&head));
//! # Ok(())
//! # }
//! ```
//!
//! See [`remote`] for the full key layout and API documentation.
//!
//! ## Direct file access (packchain remotes only)
//!
//! [`read_blob`] returns the bytes of a single file at a ref's tip
//! without cloning the repo. The companion [`PackIndexCache`]
//! amortises pack-index parses across calls — long-running consumers
//! (CI agents, build systems) keep one cache for the lifetime of the
//! process.
//!
//! ```no_run
//! # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use git_remote_object_store::{PackIndexCache, Remote, read_blob};
//!
//! let remote = Remote::connect("s3+https://bucket/repo?engine=packchain").await?;
//! let cache = PackIndexCache::default();
//! let bytes = read_blob(&remote, "refs/heads/main", "src/main.rs", &cache).await?;
//! println!("{}", String::from_utf8_lossy(&bytes));
//! # Ok(())
//! # }
//! ```
//!
//! # CLI
//!
//! The binaries (`git-remote-s3-https`, `git-remote-az-https`, etc.) are
//! packaged in the companion `git-remote-object-store-cli` crate under
//! `cli/`. Build and install with `cargo install --path cli`.
//!
//! # Architecture
//!
//! This crate ships two backends behind one shared [`ObjectStore`]
//! trait — AWS S3 (and S3-compatible endpoints) and Azure Blob
//! Storage. The on-bucket key layout, locking semantics, helper-
//! protocol behaviour, and management-CLI shape are this project's
//! own decisions; the only external contracts are the git
//! helper-protocol, the LFS protocol, and the cloud-provider HTTP
//! APIs.

pub(crate) mod bundle;
pub mod git;
pub(crate) mod keys;
pub mod lfs;
pub mod manage;
pub mod object_store;
pub mod packchain;
pub mod protocol;
pub mod remote;
pub mod url;

// Re-export the most commonly used types at the crate root so consumers
// do not need three-level import paths.
#[doc(no_inline)]
pub use object_store::{
    BoxError, GetOpts, ObjectMeta, ObjectStore, ObjectStoreError, ProgressSink, PutOpts,
};
/// Re-export of the packchain engine's public API: the error enum
/// surfaced through [`protocol::push::PushError::Packchain`] /
/// [`protocol::fetch::FetchError::Packchain`], plus the Phase 4
/// direct-file-access entry points [`packchain::read_blob`] and
/// [`packchain::PackIndexCache`].
#[doc(no_inline)]
pub use packchain::{PackIndexCache, PackchainError, read_blob};
#[doc(no_inline)]
pub use protocol::backend::{BackendError, BackendKind};
#[doc(no_inline)]
pub use remote::{Remote, RemoteError};
#[doc(no_inline)]
pub use url::{RemoteUrl, StorageEngine};

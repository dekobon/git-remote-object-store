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
//! # CLI
//!
//! The binaries (`git-remote-s3-https`, `git-remote-az-https`, etc.) are
//! packaged in the companion `git-remote-object-store-cli` crate under
//! `cli/`. Build and install with `cargo install --path cli`.
//!
//! # Architecture
//!
//! The high-level design is documented in `execution-plan.md` at the
//! repository root. This crate is a Rust port of
//! [`awslabs/git-remote-s3`][upstream] with an additional Azure Blob Storage
//! backend.
//!
//! [upstream]: https://github.com/awslabs/git-remote-s3

pub(crate) mod bundle;
pub mod git;
pub(crate) mod keys;
pub mod lfs;
pub mod manage;
pub mod object_store;
pub mod protocol;
pub mod remote;
pub mod url;

// Re-export the most commonly used types at the crate root so consumers
// do not need three-level import paths.
#[doc(no_inline)]
pub use object_store::{
    BoxError, GetOpts, ObjectMeta, ObjectStore, ObjectStoreError, ProgressSink, PutOpts,
};
#[doc(no_inline)]
pub use protocol::backend::{BackendError, BackendKind};
#[doc(no_inline)]
pub use remote::{Remote, RemoteError};
#[doc(no_inline)]
pub use url::RemoteUrl;

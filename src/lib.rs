//! `git-remote-object-store` — a git remote helper backed by cloud object
//! stores (S3 and Azure Blob Storage).
//!
//! This crate is a Rust port of [`awslabs/git-remote-s3`][upstream] with an
//! additional Azure Blob Storage backend. The high-level architecture is
//! documented in `execution-plan.md` at the repository root.
//!
//! [upstream]: https://github.com/awslabs/git-remote-s3

pub mod git;
pub(crate) mod keys;
pub mod lfs;
pub mod manage;
pub mod object_store;
pub mod protocol;
pub mod url;

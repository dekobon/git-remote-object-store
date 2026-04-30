//! LFS custom-transfer agent (line-oriented JSON protocol).
//!
//! Implementation of `git-lfs-object-store`, the LFS custom-transfer
//! agent for the S3 and Azure Blob backends. The shape of the agent
//! and the on-bucket layout (`<prefix>/lfs/<oid>`) follow upstream's
//! `../git-remote-s3/git_remote_s3/lfs.py` so existing buckets remain
//! readable.
//!
//! The agent has two modes:
//!
//! - **Subcommands** ([`install`], [`enable_debug`], [`disable_debug`]):
//!   one-shot CLI calls that mutate the local repo's `git config`.
//! - **Helper REPL** ([`run::run`]): newline-delimited JSON over
//!   stdin/stdout, dispatched per LFS event (`init`, `upload`,
//!   `download`, `terminate`).
//!
//! Stdout is the wire protocol — see `.claude/rules/protocol-stdout.md`.
//! Diagnostics use `tracing` configured to write to stderr (or to a
//! debug log file when invoked with the `debug` argv slot).

pub(crate) mod agent;
pub(crate) mod install;
pub(crate) mod oid;
pub(crate) mod protocol;
pub(crate) mod run;

pub use install::{AGENT_NAME, InstallError, disable_debug, enable_debug, install};
pub use run::{GitRemoteResolver, RemoteResolver, RunError, run};

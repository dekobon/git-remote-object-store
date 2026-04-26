//! LFS custom-transfer agent (line-oriented JSON protocol).
//!
//! Implementation of `git-lfs-object-store`, the LFS custom-transfer
//! agent for the S3 and Azure Blob backends. Mirrors
//! `../git-remote-s3/git_remote_s3/lfs.py`; the on-bucket layout is
//! preserved (`<prefix>/lfs/<oid>` per `execution-plan.md` §1.1).
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

pub mod agent;
pub mod install;
pub mod oid;
pub mod protocol;
pub mod run;

pub use install::{AGENT_NAME, InstallError, disable_debug, enable_debug, install};
pub use run::{GitRemoteResolver, RemoteResolver, RunError, run};

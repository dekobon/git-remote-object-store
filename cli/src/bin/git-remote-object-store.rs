//! Thin shim for the `git-remote-object-store` management CLI.
//!
//! The clap definition and dispatch logic live in
//! `git_remote_object_store_cli::management` so `xtask man` can render
//! the manpage from the same `clap::Command`. This binary only
//! installs tracing, starts a tokio runtime, parses argv, and forwards
//! to `management::dispatch`.

// Per `.claude/rules/protocol-stdout.md`, the management binary speaks no
// protocol on stdout and may write human-readable output normally; opt
// out of the workspace-wide `disallowed_macros` lint that targets the
// helper binaries.
#![allow(clippy::disallowed_macros)]

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use git_remote_object_store::protocol::backend::{self, BackendError};
use git_remote_object_store_cli::management::{Cli, dispatch};

fn main() -> ExitCode {
    init_tracing();

    let cli = Cli::parse();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("fatal: failed to start tokio runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(dispatch(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if let Some(be) = err.chain().find_map(|e| e.downcast_ref::<BackendError>()) {
                eprintln!("{}", backend::fatal_message(be));
            } else {
                eprintln!("fatal: {err:#}");
            }
            ExitCode::FAILURE
        }
    }
}

/// Initialise `tracing-subscriber` with stderr output. `git-remote-object-store`
/// is a regular CLI, but logs still belong on stderr so they don't
/// interleave with the doctor's report.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

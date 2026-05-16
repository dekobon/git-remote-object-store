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

use git_remote_object_store::protocol::backend::{self, BackendError};
use git_remote_object_store::protocol::tracing_init;
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

/// Install the shared stderr-only tracing subscriber.
///
/// The management CLI uses the **same** verbosity policy as the
/// helper-protocol binaries (see [`tracing_init::init`]): startup level
/// is `error`, with `GIT_REMOTE_OBJECT_STORE_VERBOSE >= 2` raising it
/// to `info`. `RUST_LOG` is intentionally **not** consulted — one knob,
/// uniform across every binary in the crate. The reload handle returned
/// by `init` is dropped: the management CLI has no protocol REPL that
/// could flip verbosity at runtime.
///
/// `try_init` failures are swallowed — a global subscriber already
/// installed by a test harness or a parent process is benign; we simply
/// log nothing.
fn init_tracing() {
    let _ = tracing_init::init();
}

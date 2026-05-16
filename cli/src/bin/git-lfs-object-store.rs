//! `git-lfs-object-store` — LFS custom-transfer agent for the S3 /
//! Azure backends.
//!
//! Three subcommands (`install`, `enable-debug`, `disable-debug`)
//! mutate the local repo's `git config`; passing `debug` (or no
//! argument at all) starts the LFS REPL on stdin/stdout. Stdout is
//! reserved for protocol traffic — every diagnostic goes to stderr
//! or to `<git-dir>/lfs/tmp/git-lfs-object-store.log` when debug
//! logging is on. The single `print_subcommand_ack` helper below is
//! the only stdout writer that goes through `println!`; everywhere
//! else uses `tracing` (stderr) or the protocol writer.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;

use anyhow::{Context, anyhow};
use tokio::io::BufReader;
use tracing::error;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use git_remote_object_store::lfs::{self, GitRemoteResolver, disable_debug, enable_debug, install};
use git_remote_object_store::protocol::tracing_init;

const DEBUG_LOG_FILENAME: &str = "git-lfs-object-store.log";

#[tokio::main]
async fn main() -> ExitCode {
    // git invokes the agent with at most one positional argument
    // (the optional `debug` slot, set by `enable-debug`), so peeking
    // `argv[1]` is sufficient — no need to collect a `Vec`.
    let subcommand = std::env::args().nth(1);
    match run(subcommand.as_deref()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Tracing may or may not be installed (subcommands run
            // before subscriber init). Fall back to stderr so the
            // user always sees the diagnostic.
            eprintln!("git-lfs-object-store: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(subcommand: Option<&str>) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("failed to read current working directory")?;

    match subcommand {
        Some("install") => {
            install(&cwd).context("install failed")?;
            print_subcommand_ack(&format!("{} installed", lfs::AGENT_NAME));
            Ok(())
        }
        Some("enable-debug") => {
            enable_debug(&cwd).context("enable-debug failed")?;
            print_subcommand_ack("debug enabled");
            Ok(())
        }
        Some("disable-debug") => {
            disable_debug(&cwd).context("disable-debug failed")?;
            print_subcommand_ack("debug disabled");
            Ok(())
        }
        Some("debug") => repl(cwd, true).await,
        None => repl(cwd, false).await,
        Some(other) => Err(anyhow!("unknown subcommand: {other}")),
    }
}

/// Print a one-shot subcommand confirmation. Only invoked from the
/// `install` / `enable-debug` / `disable-debug` branches, which never
/// enter the LFS protocol REPL — so the `println!` cannot collide
/// with wire traffic. The `clippy::disallowed_macros` allow is scoped
/// to this single helper; any future stdout write elsewhere in the
/// binary still trips the lint.
#[allow(clippy::disallowed_macros)]
fn print_subcommand_ack(line: &str) {
    println!("{line}");
}

async fn repl(cwd: PathBuf, debug_logging: bool) -> anyhow::Result<()> {
    let git_dir = git_dir(&cwd)?;
    let tmp_dir = git_dir.join("lfs").join("tmp");

    init_tracing(&tmp_dir, debug_logging)?;

    let resolver = GitRemoteResolver { repo_dir: cwd };
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();

    match lfs::run(stdin, stdout, &resolver, &tmp_dir).await {
        Ok(()) => Ok(()),
        // BrokenPipe on any write (init-ack, progress, complete) means
        // git-lfs closed our stdout — clean shutdown, not a crash.
        // The helper covers both `RunError::Io` (direct stdin reads)
        // and `RunError::Agent(AgentError::Io)` (writes via
        // `agent::write_event`).
        Err(other) if other.is_broken_pipe() => Ok(()),
        Err(other) => {
            error!(error = %other, "LFS REPL exited with error");
            Err(other.into())
        }
    }
}

/// Resolve `<git-dir>` for the repo containing `cwd`. Bare and
/// non-bare repos are both supported.
fn git_dir(cwd: &Path) -> anyhow::Result<PathBuf> {
    let repo = gix::discover(cwd).with_context(|| {
        format!(
            "could not find a git repository at or above {}",
            cwd.display()
        )
    })?;
    Ok(repo.git_dir().to_owned())
}

/// Set up the global tracing subscriber.
///
/// Two mutually exclusive code paths:
///
/// * `debug_logging = true` — set when `enable-debug` has flipped the
///   repo's git config and git invokes us with the `debug` argv slot.
///   Routes lines to `<tmp_dir>/git-lfs-object-store.log` at `debug`
///   level. The level and writer are fixed; `GIT_REMOTE_OBJECT_STORE_VERBOSE`
///   is intentionally **not** consulted here — the file destination and
///   `debug` floor are the contract of `enable-debug`.
/// * `debug_logging = false` — the default REPL path. Delegates to
///   [`tracing_init::init`] so the LFS agent shares the single-knob
///   verbosity policy used by the helper-protocol binaries and the
///   management CLI: `GIT_REMOTE_OBJECT_STORE_VERBOSE >= 2` raises the
///   startup floor from `error` to `info`; output goes to stderr. The
///   reload handle is dropped — the LFS protocol has no `option
///   verbosity` analogue to flip levels at runtime.
///
/// Failure to install a subscriber is non-fatal in both branches — we
/// want to continue serving the LFS protocol even if logging is
/// unavailable.
fn init_tracing(tmp_dir: &Path, debug_logging: bool) -> anyhow::Result<()> {
    if debug_logging {
        std::fs::create_dir_all(tmp_dir)
            .with_context(|| format!("failed to create LFS tmp dir {}", tmp_dir.display()))?;
        let log_path = tmp_dir.join(DEBUG_LOG_FILENAME);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("failed to open LFS log {}", log_path.display()))?;
        let _ = tracing_subscriber::registry()
            .with(EnvFilter::new("debug"))
            .with(
                fmt::layer()
                    .with_writer(Mutex::new(file))
                    .with_target(false),
            )
            .try_init();
    } else {
        let _ = tracing_init::init();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard for #180.
    ///
    /// Before the fix, the LFS agent's non-debug path installed its own
    /// `EnvFilter` pinned to `error`, ignoring
    /// `GIT_REMOTE_OBJECT_STORE_VERBOSE`. The fix routes that path
    /// through [`tracing_init::init`] so the LFS agent shares the same
    /// single-knob verbosity policy as the helper-protocol binaries and
    /// the management CLI.
    ///
    /// The compile-time `use tracing_init` in the bin's preamble and
    /// the `tracing_init::init()` call in the non-debug branch are the
    /// structural delegation. The test below is a value-level pin so a
    /// future rename of `ENV_VERBOSE` doesn't silently break the
    /// contract documented in `docs/environment-variables.md`.
    #[test]
    fn lfs_non_debug_path_honors_verbose_env_var() {
        assert_eq!(
            tracing_init::ENV_VERBOSE,
            "GIT_REMOTE_OBJECT_STORE_VERBOSE",
            "LFS bin's non-debug path delegates to tracing_init::init, \
             which keys verbosity off this exact env var",
        );
    }
}

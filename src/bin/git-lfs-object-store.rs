//! `git-lfs-object-store` — LFS custom-transfer agent for the S3 /
//! Azure backends (Phase 10).
//!
//! Three subcommands (`install`, `enable-debug`, `disable-debug`)
//! mutate the local repo's `git config`; passing `debug` (or no
//! argument at all) starts the LFS REPL on stdin/stdout. Stdout is
//! reserved for protocol traffic — every diagnostic goes to stderr
//! or to `<git-dir>/lfs/tmp/git-lfs-object-store.log` when debug
//! logging is on.
//!
//! `clippy::disallowed_macros` is opted out at file scope: the
//! subcommand branches (`install` / `enable-debug` / `disable-debug`)
//! print human-readable confirmation to stdout, which is safe because
//! those branches never enter the LFS REPL — they exit before the
//! protocol speaker takes over. Protocol traffic itself goes through
//! a `tokio::io::AsyncWrite`, never through `println!`.
#![allow(clippy::disallowed_macros)]

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

use git_remote_object_store::lfs::{
    self, GitRemoteResolver, RunError, disable_debug, enable_debug, install,
};

const DEBUG_LOG_FILENAME: &str = "git-lfs-object-store.log";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args).await {
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

async fn run(args: Vec<String>) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("failed to read current working directory")?;

    match args.first().map(String::as_str) {
        Some("install") => {
            install(&cwd).await.context("install failed")?;
            println!("{} installed", lfs::AGENT_NAME);
            Ok(())
        }
        Some("enable-debug") => {
            enable_debug(&cwd).await.context("enable-debug failed")?;
            println!("debug enabled");
            Ok(())
        }
        Some("disable-debug") => {
            disable_debug(&cwd).await.context("disable-debug failed")?;
            println!("debug disabled");
            Ok(())
        }
        Some("debug") => repl(cwd, true).await,
        None => repl(cwd, false).await,
        Some(other) => Err(anyhow!("unknown subcommand: {other}")),
    }
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
        Err(RunError::Io(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::WriteZero
            ) =>
        {
            Ok(())
        }
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

/// Set up the global tracing subscriber. `debug_logging` (set when
/// `enable-debug` has been run and git invokes us with the `debug`
/// argv slot) routes lines to `<tmp_dir>/git-lfs-object-store.log`
/// at `debug` level; otherwise we log at `error` to stderr.
///
/// Failure to initialise the subscriber is non-fatal — we want to
/// continue serving the LFS protocol even if logging is unavailable.
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
        let _ = tracing_subscriber::registry()
            .with(EnvFilter::default().add_directive(tracing::Level::ERROR.into()))
            .with(fmt::layer().with_writer(std::io::stderr).with_target(false))
            .try_init();
    }
    Ok(())
}

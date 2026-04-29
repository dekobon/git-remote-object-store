//! Shared entry-point for the `git-remote-{s3,az}-{http,https}` shims.
//!
//! Stdout is reserved for the git remote-helper wire protocol — see
//! `.claude/rules/protocol-stdout.md`. All diagnostics go to stderr via
//! `tracing`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, anyhow};
use tokio::io::BufReader;

use git_remote_object_store::RemoteUrl;
use git_remote_object_store::protocol::{self, backend, tracing_init};

/// Shared `main` for every `git-remote-{s3,az}-{http,https}` binary.
///
/// Git always invokes a remote helper as `git-remote-<scheme> <remote-name>
/// <url>` — see `git help gitremote-helpers`. We read the URL from
/// `argv[2]`, matching the upstream Python helper exactly.
///
/// Returns [`ExitCode`] rather than `anyhow::Result` so that
/// credential / missing-bucket / authorization failures from
/// [`backend::build`] can be rendered as a single-line `fatal:` message
/// (matching upstream `git-remote-s3` at
/// `../git-remote-s3/git_remote_s3/remote.py:574-593`) without
/// `anyhow`'s `Display` chain layering on top.
pub async fn run_main() -> ExitCode {
    let remote = match parse_remote_arg(std::env::args()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fatal: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let reload = match tracing_init::init() {
        Ok(handle) => Some(handle),
        Err(e) => {
            // Tracing failed to install (typically: another global subscriber
            // already exists, e.g. in some test harnesses). The protocol can
            // still run — we just lose runtime verbosity flips.
            eprintln!("warning: tracing subscriber install failed: {e}");
            None
        }
    };

    #[cfg(unix)]
    install_sigpipe_mask();

    let store = match backend::build(&remote).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", backend::fatal_message(&e));
            return ExitCode::FAILURE;
        }
    };

    // Resolve the local repository the helper operates against. Modern
    // git (>= ~2.50) invokes remote helpers during `git clone` *before*
    // chdir-ing into the destination, so cwd points at the parent of
    // the new clone and `gix::open(cwd)` fails. `GIT_DIR` is set by git
    // in that path, so prefer it; fall back to cwd for the fetch / push
    // case where git invokes the helper from inside an existing
    // worktree without `GIT_DIR` set. Hand the worktree (not the `.git`
    // dir) to the parallel fetch path so subprocess git tooling that
    // expects a worktree behaves correctly.
    let repo_dir = match resolve_repo_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fatal: {e}");
            return ExitCode::FAILURE;
        }
    };

    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();

    match protocol::run(remote, store, stdin, stdout, reload, repo_dir).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.is_broken_pipe() => {
            tracing::debug!("stdout closed (BrokenPipe); exiting cleanly");
            ExitCode::SUCCESS
        }
        Err(other) => {
            eprintln!("fatal: {other:#}");
            ExitCode::FAILURE
        }
    }
}

/// Extract and parse the remote URL from a process-argv-style iterator.
///
/// Split out from [`run_main`] so the argv contract (slot, error message)
/// is testable without spawning a process or installing a global tracing
/// subscriber.
fn parse_remote_arg<I>(args: I) -> anyhow::Result<RemoteUrl>
where
    I: IntoIterator<Item = String>,
{
    let raw = args
        .into_iter()
        .nth(2)
        .ok_or_else(|| anyhow!("missing remote URL: expected `<remote-name> <url>` on argv"))?;
    raw.parse::<RemoteUrl>()
        .context("failed to parse remote URL")
}

/// Resolve the local repository directory for the remote-helper REPL.
///
/// Returns the **worktree** for non-bare repos, or the git directory
/// for bare repos. The parallel fetch path uses this as the cwd handed
/// to `gix::open`, which refuses to treat a non-repository directory
/// (such as the parent of a `git clone` destination) as a git repo.
///
/// Resolution order (matches git's own `setup_git_directory_gently`):
///
/// 1. `GIT_DIR` env var (set by `git clone` before invoking the helper,
///    when cwd is still the parent of the destination).
/// 2. `gix::discover` from cwd (the fetch / push path: cwd is inside
///    the worktree, no `GIT_DIR` set).
fn resolve_repo_dir() -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current working directory")?;
    let candidate = match std::env::var_os("GIT_DIR") {
        Some(d) => {
            let p = PathBuf::from(d);
            if p.is_absolute() { p } else { cwd.join(p) }
        }
        None => cwd,
    };
    let repo = gix::discover(&candidate).with_context(|| {
        format!(
            "failed to discover git repository at {}",
            candidate.display()
        )
    })?;
    Ok(repo
        .workdir()
        .map_or_else(|| repo.git_dir().to_path_buf(), Path::to_path_buf))
}

#[cfg(unix)]
fn install_sigpipe_mask() {
    // tokio's signal handler installs SIG_IGN-equivalent semantics:
    // SIGPIPE no longer kills the process; instead, the failing write
    // returns EPIPE → ErrorKind::BrokenPipe, which run_main catches
    // above and turns into a clean exit. The returned Signal stream is
    // dropped immediately — we just need the side effect of the installation.
    let _ = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::pipe());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the argv that git actually passes: `argv[0]` is the binary
    /// path, `argv[1]` is the remote name, `argv[2]` is the URL.
    fn argv(extras: &[&str]) -> Vec<String> {
        let mut v = vec!["git-remote-s3-https".to_owned(), "origin".to_owned()];
        v.extend(extras.iter().map(|s| (*s).to_owned()));
        v
    }

    #[test]
    fn parse_remote_arg_reads_argv_slot_two() {
        let url = "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo";
        let remote = parse_remote_arg(argv(&[url])).expect("parse should succeed");
        assert_eq!(remote.prefix(), Some("repo"));
    }

    #[test]
    fn parse_remote_arg_errors_when_url_missing() {
        let err = parse_remote_arg(argv(&[])).expect_err("argv[2] missing should error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("missing remote URL"),
            "error should name the missing slot: {msg}"
        );
    }

    #[test]
    fn parse_remote_arg_errors_on_bad_url() {
        let err = parse_remote_arg(argv(&["not a url"])).expect_err("invalid URL should error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to parse remote URL"),
            "error should preserve context: {msg}"
        );
    }
}

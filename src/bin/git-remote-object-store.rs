//! Management CLI: `doctor`, `delete-branch`, `protect`, `unprotect`.
//!
//! Each subcommand takes a `<remote>` positional that may be a remote URL
//! (`s3+https://…`, `az+https://…`) or the name of a git remote in the
//! current repository (resolved via `git remote get-url`). The parsed URL
//! drives backend selection through
//! [`git_remote_object_store::protocol::backend::build`].

// Per `.claude/rules/protocol-stdout.md`, the management binary speaks no
// protocol on stdout and may write human-readable output normally; opt
// out of the workspace-wide `disallowed_macros` lint that targets the
// helper binaries.
#![allow(clippy::disallowed_macros)]

use std::path::Path;
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use git_remote_object_store::git as git_helpers;
use git_remote_object_store::manage::{
    DEFAULT_LOCK_TTL_SECONDS, DialoguerPrompter,
    branch::ManageBranch,
    doctor::{Doctor, DoctorOpts},
};
use git_remote_object_store::object_store::ObjectStore;
use git_remote_object_store::protocol::backend;
use git_remote_object_store::url::{self as remote_url, RemoteUrl};

#[derive(Debug, Parser)]
#[command(
    name = "git-remote-object-store",
    about = "Manage git remotes backed by S3 or Azure Blob Storage",
    version,
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Analyze a remote and offer to fix duplicate bundles, invalid HEAD,
    /// and stale locks.
    Doctor {
        #[command(flatten)]
        target: Target,

        /// Delete losing bundles outright instead of moving them to a
        /// `<ref>_<uuid8>` quarantine ref.
        #[arg(short = 'd', long)]
        delete_bundle: bool,

        /// Seconds after which a lock is considered stale.
        #[arg(long, default_value_t = DEFAULT_LOCK_TTL_SECONDS, value_name = "SECONDS")]
        lock_ttl: u64,

        /// Delete stale locks found during the scan.
        #[arg(long)]
        delete_stale_locks: bool,
    },
    /// Delete every object under `refs/heads/<branch>/` after a y/N
    /// confirmation.
    DeleteBranch {
        #[command(flatten)]
        target: Target,
        /// Branch name, without the `refs/heads/` prefix.
        branch: String,
    },
    /// Mark a branch as protected by writing the `PROTECTED#` sentinel.
    Protect {
        #[command(flatten)]
        target: Target,
        /// Branch name, without the `refs/heads/` prefix.
        branch: String,
    },
    /// Remove the `PROTECTED#` sentinel from a branch.
    Unprotect {
        #[command(flatten)]
        target: Target,
        /// Branch name, without the `refs/heads/` prefix.
        branch: String,
    },
}

#[derive(Debug, Args)]
struct Target {
    /// A remote URL (`s3+https://…`, `az+https://…`) or the name of a
    /// git remote configured in the current repository.
    remote: String,
}

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
            eprintln!("fatal: {err:#}");
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

async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Doctor {
            target,
            delete_bundle,
            lock_ttl,
            delete_stale_locks,
        } => {
            let (store, prefix) = open_target(&target).await?;
            let opts = DoctorOpts {
                delete_bundle,
                lock_ttl_seconds: lock_ttl,
                delete_stale_locks,
            };
            let prompter = DialoguerPrompter;
            let doctor = Doctor::new(store, prefix, opts, &prompter);
            Ok(doctor.run().await?)
        }
        Command::DeleteBranch { target, branch } => {
            run_branch(&target, &branch, BranchAction::Delete).await
        }
        Command::Protect { target, branch } => {
            run_branch(&target, &branch, BranchAction::Protect).await
        }
        Command::Unprotect { target, branch } => {
            run_branch(&target, &branch, BranchAction::Unprotect).await
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum BranchAction {
    Delete,
    Protect,
    Unprotect,
}

async fn run_branch(target: &Target, branch: &str, action: BranchAction) -> Result<()> {
    let (store, prefix) = open_target(target).await?;
    let prompter = DialoguerPrompter;
    let mb = ManageBranch::open(store, prefix, branch.to_owned(), &prompter).await?;
    let outcome = match action {
        BranchAction::Delete => mb.delete_branch().await,
        BranchAction::Protect => mb.protect_branch().await,
        BranchAction::Unprotect => mb.unprotect_branch().await,
    };
    Ok(outcome?)
}

/// Resolve `target.remote` to an `(ObjectStore, prefix)` pair.
///
/// The returned `prefix` is empty (`""`) when the parsed URL has no
/// repository prefix — i.e. the repo is stored at the bucket/container
/// root. The downstream management surfaces (`Doctor`, `ManageBranch`,
/// `analyze`) all build keys without a leading slash for empty
/// prefixes, matching the protocol REPL's on-bucket layout.
async fn open_target(target: &Target) -> Result<(Arc<dyn ObjectStore>, String)> {
    let url = resolve_remote(&target.remote)?;
    let prefix = url.prefix().unwrap_or_default().to_owned();
    let store = backend::build(&url)
        .await
        .context("failed to build object-store backend")?;
    Ok((store, prefix))
}

/// Try to interpret `input` as a URL first; if that fails, look it up as
/// a git remote name in the repository discovered from cwd.
fn resolve_remote(input: &str) -> Result<RemoteUrl> {
    if let Ok(url) = RemoteUrl::from_str(input) {
        return Ok(url);
    }
    let cwd = std::env::current_dir().context("failed to read current working directory")?;
    let url_string = remote_url_from_named_remote(&cwd, input)?;
    remote_url::parse(&url_string)
        .with_context(|| format!("remote `{input}` URL `{url_string}` is not a recognised scheme"))
}

fn remote_url_from_named_remote(cwd: &Path, name: &str) -> Result<String> {
    let repo = gix::discover(cwd).with_context(|| {
        format!(
            "`{name}` is not a recognised remote URL and no git repository was found at {}",
            cwd.display()
        )
    })?;
    git_helpers::remote_url(&repo, name)
        .map_err(|err| anyhow!(err))
        .with_context(|| format!("failed to read remote `{name}` URL"))
}

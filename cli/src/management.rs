//! Clap definition and dispatch for the `git-remote-object-store`
//! management CLI (`doctor`, `delete-branch`, `protect`, `unprotect`,
//! `gc`, `compact`).
//!
//! The types live in the library (not in `src/bin/git-remote-object-store.rs`)
//! so `xtask man` can build the `clap::Command` and render manpages
//! without duplicating the option surface. The binary entry point is a
//! thin shim — see `cli/src/bin/git-remote-object-store.rs`.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};

use git_remote_object_store::git as git_helpers;
use git_remote_object_store::manage::{
    DEFAULT_LOCK_TTL_SECONDS, DialoguerPrompter,
    branch::ManageBranch,
    compact::{Compact, CompactOpts},
    doctor::{Doctor, DoctorOpts},
    gc::{Gc, GcOpts},
};
use git_remote_object_store::object_store::ObjectStore;
use git_remote_object_store::packchain::gc as packchain_gc;
use git_remote_object_store::protocol::backend;
use git_remote_object_store::url::{self as remote_url, RemoteUrl, StorageEngine};

/// Top-level clap parser for `git-remote-object-store`.
#[derive(Debug, Parser)]
#[command(
    name = "git-remote-object-store",
    about = "Manage git remotes backed by S3 or Azure Blob Storage",
    version,
    propagate_version = true,
    // `xtask man` skips the auto-generated `help` subcommand because no
    // `git-remote-object-store-help.1` page is produced; disable it here
    // so the SUBCOMMANDS list in the parent page does not cross-reference
    // a non-existent man page. Per-subcommand `--help` is unaffected.
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Selected subcommand.
    #[command(subcommand)]
    pub command: Command,
}

/// Management subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Analyze a remote and offer to fix duplicate bundles, invalid HEAD,
    /// and stale locks.
    Doctor {
        /// Remote target (URL or named git remote).
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
        /// Remote target (URL or named git remote).
        #[command(flatten)]
        target: Target,
        /// Branch name, without the `refs/heads/` prefix.
        branch: String,
    },
    /// Mark a branch as protected by writing the `PROTECTED#` sentinel.
    Protect {
        /// Remote target (URL or named git remote).
        #[command(flatten)]
        target: Target,
        /// Branch name, without the `refs/heads/` prefix.
        branch: String,
    },
    /// Remove the `PROTECTED#` sentinel from a branch.
    Unprotect {
        /// Remote target (URL or named git remote).
        #[command(flatten)]
        target: Target,
        /// Branch name, without the `refs/heads/` prefix.
        branch: String,
    },
    /// Two-phase mark-and-sweep garbage collection of orphan packs on a
    /// packchain bucket. Default flow is mark + sweep; `--mark-only` and
    /// `--sweep-only` separate the phases for cron-style scheduling.
    Gc {
        /// Remote target (URL or named git remote).
        #[command(flatten)]
        target: Target,

        /// Run the mark phase only (write a tombstone, do not delete).
        #[arg(long, conflicts_with = "sweep_only")]
        mark_only: bool,

        /// Run the sweep phase only (process pre-existing tombstones).
        #[arg(long, conflicts_with = "mark_only")]
        sweep_only: bool,

        /// Bypass the grace window and the orphan re-check.
        /// Operator-asserted safe (no concurrent reads).
        #[arg(long)]
        force: bool,

        /// Hours a tombstone must age before its packs are eligible
        /// for sweep. Default reads the `GIT_REMOTE_OBJECT_STORE_GC_GRACE_HOURS`
        /// environment variable (falling back to 24).
        #[arg(long, value_name = "HOURS")]
        grace_hours: Option<u64>,
    },
    /// Compact a packchain ref's chain.json down to a single segment
    /// at the current tip. The default scans every ref and prompts
    /// for confirmation; `--ref` targets a single branch. Old segment
    /// packs become orphans for `gc` to reap.
    Compact {
        /// Remote target (URL or named git remote).
        #[command(flatten)]
        target: Target,

        /// Compact only this ref. Accepts a fully-qualified path
        /// (`refs/heads/main`).
        #[arg(long, value_name = "REF")]
        ref_name: Option<String>,

        /// Bypass the segments-/bytes-since-`full_at` heuristic and
        /// compact unconditionally. Useful after a force push when
        /// segments are below threshold but the operator still
        /// wants a baseline rewrite.
        #[arg(long)]
        force: bool,

        /// Run `gc` mark+sweep against the same bucket after a
        /// successful compact. Convenience for one-command cleanup.
        #[arg(long)]
        with_gc: bool,

        /// Lock TTL for compact's per-ref lock, in seconds. Compact
        /// holds the lock from chain read through chain.json commit;
        /// large repos may need a TTL well above the push default.
        /// Default reads `GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS` (falling
        /// back to 60s).
        #[arg(long, value_name = "SECONDS")]
        lock_ttl_seconds: Option<u64>,

        /// Grace hours for the optional `--with-gc` sweep; ignored
        /// without `--with-gc`. Default reads
        /// `GIT_REMOTE_OBJECT_STORE_GC_GRACE_HOURS` (falling back to 24).
        #[arg(long, value_name = "HOURS")]
        gc_grace_hours: Option<u64>,
    },
}

/// Shared positional for every subcommand: the remote URL or named
/// git remote to operate against.
#[derive(Debug, Args)]
pub struct Target {
    /// A remote URL (`s3+https://…`, `az+https://…`) or the name of a
    /// git remote configured in the current repository.
    pub remote: String,
}

/// Run the parsed CLI to completion.
///
/// Returns `Ok(())` on success and propagates any backend / domain
/// error otherwise. The binary `main()` is responsible for installing
/// a tokio runtime, initialising tracing, and rendering errors.
pub async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Doctor {
            target,
            delete_bundle,
            lock_ttl,
            delete_stale_locks,
        } => {
            let (store, prefix, engine) = open_target_with_engine(&target).await?;
            let opts = DoctorOpts {
                delete_bundle,
                lock_ttl_seconds: lock_ttl,
                delete_stale_locks,
                engine,
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
        Command::Gc {
            target,
            mark_only,
            sweep_only,
            force,
            grace_hours,
        } => {
            let (store, prefix) = open_target(&target).await?;
            let opts = GcOpts {
                mark_only,
                sweep_only,
                force,
                grace_hours: grace_hours.unwrap_or_else(packchain_gc::grace_hours_from_env),
            };
            Ok(Gc::new(store, prefix, opts).run().await?)
        }
        Command::Compact {
            target,
            ref_name,
            force,
            with_gc,
            lock_ttl_seconds,
            gc_grace_hours,
        } => {
            let (store, prefix) = open_target(&target).await?;
            let opts = CompactOpts {
                ref_name,
                force,
                with_gc,
                lock_ttl_seconds,
                gc_grace_hours: gc_grace_hours.unwrap_or_else(packchain_gc::grace_hours_from_env),
            };
            let prompter = DialoguerPrompter;
            Ok(Compact::new(store, prefix, opts, &prompter).run().await?)
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
        BranchAction::Delete => mb.delete().await,
        BranchAction::Protect => mb.protect().await,
        BranchAction::Unprotect => mb.unprotect().await,
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
    let (store, prefix, _engine) = open_target_with_engine(target).await?;
    Ok((store, prefix))
}

/// Like [`open_target`] but also returns the resolved engine, for
/// subcommands whose output depends on whether the bucket is bundle-
/// or packchain-shaped (e.g. `doctor`'s engine-aware report).
async fn open_target_with_engine(
    target: &Target,
) -> Result<(Arc<dyn ObjectStore>, String, StorageEngine)> {
    let url = resolve_remote(&target.remote)?;
    let prefix = url.prefix().unwrap_or_default().to_owned();
    let (store, engine) = backend::build(&url).await?;
    Ok((store, prefix, engine))
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

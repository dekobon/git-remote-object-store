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
    DialoguerPrompter,
    branch::ManageBranch,
    compact::{Compact, CompactOpts},
    doctor::{Doctor, DoctorOpts},
    gc::{Gc, GcMode, GcOpts},
};
use git_remote_object_store::object_store::ObjectStore;
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

        /// Seconds after which a lock is considered stale. Default
        /// reads `GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS` (falling
        /// back to 60s) — matching `compact`, `delete-branch`, and
        /// the helper push path so the views of "stale" cannot drift.
        #[arg(long, value_name = "SECONDS")]
        lock_ttl_seconds: Option<u64>,

        /// Delete stale locks found during the scan.
        #[arg(long)]
        delete_stale_locks: bool,
    },
    /// Delete every object under `refs/heads/<branch>/` after a y/N
    /// confirmation. Acquires the ref's per-ref lock for the duration of
    /// the delete; the TTL reads `GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS`
    /// (falling back to 60s).
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
            lock_ttl_seconds,
            delete_stale_locks,
        } => {
            let (store, prefix, engine) = open_target_with_engine(&target).await?;
            let opts = DoctorOpts {
                delete_bundle,
                lock_ttl_seconds,
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
            let mode = gc_mode_from_flags(mark_only, sweep_only)?;
            let (store, prefix) = open_target(&target).await?;
            let opts = GcOpts {
                mode,
                force,
                grace_hours,
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
                gc_grace_hours,
            };
            let prompter = DialoguerPrompter;
            Ok(Compact::new(store, prefix, opts, &prompter).run().await?)
        }
    }
}

/// Translate the `--mark-only` / `--sweep-only` CLI flags into a
/// [`GcMode`]. clap's `conflicts_with` already rejects both flags
/// together (so the `(true, true)` arm is unreachable in practice),
/// but the explicit error keeps the parser contract local to this
/// helper rather than scattered across attribute metadata —
/// matching the "reject the conflicting combination with a clear
/// error message" criterion from finding F-008.
fn gc_mode_from_flags(mark_only: bool, sweep_only: bool) -> Result<GcMode> {
    match (mark_only, sweep_only) {
        (false, false) => Ok(GcMode::Default),
        (true, false) => Ok(GcMode::MarkOnly),
        (false, true) => Ok(GcMode::SweepOnly),
        (true, true) => Err(anyhow!(
            "--mark-only and --sweep-only are mutually exclusive"
        )),
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// F-008: the helper round-trips `--mark-only` and `--sweep-only`
    /// flags into the matching [`GcMode`] variant. The defaults flow
    /// to `GcMode::Default`.
    #[test]
    fn gc_mode_from_flags_round_trips() {
        assert_eq!(
            gc_mode_from_flags(false, false).expect("default"),
            GcMode::Default
        );
        assert_eq!(
            gc_mode_from_flags(true, false).expect("mark only"),
            GcMode::MarkOnly
        );
        assert_eq!(
            gc_mode_from_flags(false, true).expect("sweep only"),
            GcMode::SweepOnly
        );
    }

    /// F-008: passing both flags is a hard error at the boundary,
    /// not a silent no-op. The clap-level `conflicts_with` already
    /// rejects this combination at parse time; the helper enforces
    /// the same contract for programmatic callers and gives the
    /// error a clearer wording than clap's default message.
    #[test]
    fn gc_mode_from_flags_rejects_conflicting_combination() {
        let err = gc_mode_from_flags(true, true).expect_err("conflict");
        let msg = err.to_string();
        assert!(
            msg.contains("--mark-only") && msg.contains("--sweep-only"),
            "error must name both flags: {msg}"
        );
    }

    /// Issue #178: `doctor --lock-ttl-seconds` is optional and defers
    /// to `lock_ttl_from_env` (which honours
    /// `GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS`) when omitted. The
    /// previous spelling (`--lock-ttl` with `default_value_t = 60`)
    /// silently baked the compile-time default and ignored the env
    /// var, making `--delete-stale-locks` unsafe under a tuned TTL.
    /// Pin both shapes: an omitted flag parses to `None`, and the
    /// renamed flag round-trips an explicit value.
    #[test]
    fn cli_doctor_lock_ttl_seconds_defaults_to_none() {
        let cli = Cli::try_parse_from([
            "git-remote-object-store",
            "doctor",
            "s3+https://example.com/bucket",
        ])
        .expect("parse without flag");
        let Command::Doctor {
            lock_ttl_seconds, ..
        } = cli.command
        else {
            panic!("expected Doctor subcommand")
        };
        assert!(
            lock_ttl_seconds.is_none(),
            "omitted flag must parse to None so DoctorOpts can defer to the env var, got {lock_ttl_seconds:?}",
        );
    }

    #[test]
    fn cli_doctor_lock_ttl_seconds_round_trips() {
        let cli = Cli::try_parse_from([
            "git-remote-object-store",
            "doctor",
            "--lock-ttl-seconds",
            "120",
            "s3+https://example.com/bucket",
        ])
        .expect("parse with flag");
        let Command::Doctor {
            lock_ttl_seconds, ..
        } = cli.command
        else {
            panic!("expected Doctor subcommand")
        };
        assert_eq!(lock_ttl_seconds, Some(120));
    }

    /// Issue #183: the old `--lock-ttl` spelling must no longer parse
    /// — it was inconsistent with `compact --lock-ttl-seconds` and
    /// hid a bug under `--delete-stale-locks`. A future attribute
    /// edit that re-adds the alias would silently restore the bug,
    /// so pin the rejection.
    #[test]
    fn cli_doctor_rejects_legacy_lock_ttl_flag() {
        let err = Cli::try_parse_from([
            "git-remote-object-store",
            "doctor",
            "--lock-ttl",
            "60",
            "s3+https://example.com/bucket",
        ])
        .expect_err("legacy --lock-ttl must not parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    /// F-008: clap also rejects the conflicting CLI invocation at
    /// parse time, so the helper never sees `(true, true)` in
    /// production. Pin the parse-time rejection so a future attribute
    /// edit cannot silently drop the `conflicts_with` guard.
    #[test]
    fn cli_rejects_mark_only_and_sweep_only_together() {
        let result = Cli::command().try_get_matches_from([
            "git-remote-object-store",
            "gc",
            "--mark-only",
            "--sweep-only",
            "s3+https://example.com/bucket",
        ]);
        let err = result.expect_err("clap must reject conflicting flags");
        // Pin the rejection mechanism, not just the wording: a future
        // refactor that turns the failure into `MissingRequiredArgument`
        // or any other parser error (instead of an explicit conflict)
        // would silently weaken the guard but still produce a message
        // mentioning "mark" or "sweep".
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        let rendered = err.to_string();
        assert!(
            rendered.contains("--mark-only") && rendered.contains("--sweep-only"),
            "clap error must name both conflicting flags: {rendered}"
        );
    }
}

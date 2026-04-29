//! Implementations of the `install`, `enable-debug`, and
//! `disable-debug` subcommands.
//!
//! Each rewrites the local repository's `.git/config` in-process via
//! [`crate::git::config_add`] / [`crate::git::config_unset`] (which use
//! `gix-config` for parsing and `gix-lock` for the atomic-rename write)
//! to wire the LFS agent into the repository. Mirrors
//! `git_remote_s3/lfs.py:install` and the `enable-debug` /
//! `disable-debug` branches of its `main`.

use std::path::Path;

use thiserror::Error;

use crate::git::{self, GitError};

/// Custom-transfer agent name registered with `git lfs`. The keys
/// `lfs.customtransfer.<name>.*` are namespaced under this; matches
/// the binary name (`git-lfs-object-store`).
pub const AGENT_NAME: &str = "git-lfs-object-store";

const KEY_PATH: &str = "lfs.customtransfer.git-lfs-object-store.path";
const KEY_ARGS: &str = "lfs.customtransfer.git-lfs-object-store.args";
const KEY_STANDALONE: &str = "lfs.standalonetransferagent";

/// Errors surfaced by the install / debug-toggle subcommands.
#[derive(Debug, Error)]
pub enum InstallError {
    /// Underlying `git config` invocation failed.
    #[error(transparent)]
    Git(#[from] GitError),
}

/// Register the agent with `git lfs` in the repository at `cwd`.
///
/// Two writes, batched into a single read / parse / lock / write cycle:
/// - `lfs.customtransfer.git-lfs-object-store.path` → the binary name.
/// - `lfs.standalonetransferagent` → `git-lfs-object-store`, telling
///   LFS to bypass the HTTP transfer queue and call us directly.
///
/// Mirrors `../git-remote-s3/git_remote_s3/lfs.py:install`.
///
/// # Errors
///
/// Returns [`InstallError::Git`] if writing the config entries fails.
pub fn install(cwd: &Path) -> Result<(), InstallError> {
    git::config_add_many(cwd, &[(KEY_PATH, AGENT_NAME), (KEY_STANDALONE, AGENT_NAME)])?;
    Ok(())
}

/// Set `lfs.customtransfer.<agent>.args = debug` so the next time git
/// invokes the agent it forwards the `debug` argv slot, switching the
/// agent's logging from stderr to a file in `<git-dir>/lfs/tmp/`.
///
/// # Errors
///
/// Returns [`InstallError::Git`] if writing the config entry fails.
pub fn enable_debug(cwd: &Path) -> Result<(), InstallError> {
    git::config_add(cwd, KEY_ARGS, "debug")?;
    Ok(())
}

/// Inverse of [`enable_debug`]: clear `lfs.customtransfer.<agent>.args`.
///
/// # Errors
///
/// Returns [`InstallError::Git`] wrapping any [`crate::git::GitError`] from
/// [`crate::git::config_unset`]. The most common case is
/// [`crate::git::GitError::ConfigKeyNotSet`] when the args key is absent;
/// callers that want idempotent behaviour should match on that inner variant.
pub fn disable_debug(cwd: &Path) -> Result<(), InstallError> {
    git::config_unset(cwd, KEY_ARGS)?;
    Ok(())
}

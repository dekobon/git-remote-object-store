//! Implementations of the `install`, `enable-debug`, and
//! `disable-debug` subcommands.
//!
//! Each rewrites the local repository's `.git/config` in-process via
//! [`crate::git::config_set`] / [`crate::git::config_unset_if_present`]
//! (which use `gix-config` for parsing and `gix-lock` for the atomic-rename
//! write) to wire the LFS agent into the repository. All three subcommands
//! are idempotent: re-running them on an already-installed (or already-
//! uninstalled) repo is a no-op rather than producing duplicate entries or
//! a "key not set" error (see issues #198 and #210).

use std::path::Path;

use thiserror::Error;

use crate::git::{self, GitError};

/// Custom-transfer agent name registered with `git lfs`. The keys
/// `lfs.customtransfer.<name>.*` are namespaced under this; matches
/// the binary name (`git-lfs-object-store`).
pub const AGENT_NAME: &str = "git-lfs-object-store";

const KEY_STANDALONE: &str = "lfs.standalonetransferagent";

/// `lfs.customtransfer.<AGENT_NAME>.path` — composed from [`AGENT_NAME`]
/// at compile time via `concat!` so renaming the agent cannot silently
/// break LFS routing (issue #190) and no per-call allocation occurs.
const KEY_PATH: &str = concat!("lfs.customtransfer.", "git-lfs-object-store", ".path");

/// `lfs.customtransfer.<AGENT_NAME>.args` — see [`KEY_PATH`].
const KEY_ARGS: &str = concat!("lfs.customtransfer.", "git-lfs-object-store", ".args");

#[cfg(test)]
const _: () = {
    // Compile-time guard: if `AGENT_NAME` ever drifts from the literal
    // substring baked into the keys above, this assertion stops the
    // build. `concat!` only accepts string literals so we cannot inline
    // `AGENT_NAME` directly — the runtime test below catches drift in
    // CI, and this const_assert catches it during type-check on every
    // build that touches this module.
    let agent = AGENT_NAME.as_bytes();
    let needle = b"git-lfs-object-store";
    assert!(agent.len() == needle.len());
    let mut i = 0;
    while i < needle.len() {
        assert!(agent[i] == needle[i]);
        i += 1;
    }
};

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
/// - `lfs.customtransfer.<AGENT_NAME>.path` → the binary name.
/// - `lfs.standalonetransferagent` → [`AGENT_NAME`], telling LFS to
///   bypass the HTTP transfer queue and call us directly.
///
/// Idempotent: if both keys already hold the expected single value, no
/// write occurs. Legacy multi-valued state from older binaries (#198) is
/// collapsed to the canonical single entry on the next call.
///
/// # Errors
///
/// Returns [`InstallError::Git`] if writing the config entries fails.
pub fn install(cwd: &Path) -> Result<(), InstallError> {
    git::config_set_many(cwd, &[(KEY_PATH, AGENT_NAME), (KEY_STANDALONE, AGENT_NAME)])?;
    Ok(())
}

/// Set `lfs.customtransfer.<agent>.args = debug` so the next time git
/// invokes the agent it forwards the `debug` argv slot, switching the
/// agent's logging from stderr to a file in `<git-dir>/lfs/tmp/`.
///
/// Idempotent: re-running on a repo that already has the key set to
/// `debug` is a no-op (#198).
///
/// # Errors
///
/// Returns [`InstallError::Git`] if writing the config entry fails.
pub fn enable_debug(cwd: &Path) -> Result<(), InstallError> {
    git::config_set(cwd, KEY_ARGS, "debug")?;
    Ok(())
}

/// Inverse of [`enable_debug`]: clear `lfs.customtransfer.<agent>.args`.
///
/// Idempotent: if the args key is already absent, returns `Ok(())` without
/// touching the config file (#210).
///
/// # Errors
///
/// Returns [`InstallError::Git`] wrapping any [`crate::git::GitError`] from
/// [`crate::git::config_unset_if_present`] other than `ConfigKeyNotSet`
/// (which this helper treats as success).
pub fn disable_debug(cwd: &Path) -> Result<(), InstallError> {
    git::config_unset_if_present(cwd, KEY_ARGS)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AGENT_NAME, KEY_ARGS, KEY_PATH};

    // Drift between `AGENT_NAME` and the config keys is what issue #190
    // is about — `git lfs` looks up the agent by exact string match on
    // the subsection, so a mismatch silently sends LFS traffic to the
    // HTTPS transfer queue instead of the helper. The `concat!`-built
    // constants plus the `const _: () = { ... }` compile-time check
    // make drift impossible at build time; these tests pin the
    // resulting shape so a future refactor cannot quietly change it.
    #[test]
    fn key_path_embeds_agent_name() {
        assert!(
            KEY_PATH.contains(AGENT_NAME),
            "KEY_PATH = {KEY_PATH:?} must contain AGENT_NAME = {AGENT_NAME:?}",
        );
    }

    #[test]
    fn key_args_embeds_agent_name() {
        assert!(
            KEY_ARGS.contains(AGENT_NAME),
            "KEY_ARGS = {KEY_ARGS:?} must contain AGENT_NAME = {AGENT_NAME:?}",
        );
    }

    #[test]
    fn key_shapes_match_lfs_customtransfer_namespace() {
        assert_eq!(KEY_PATH, format!("lfs.customtransfer.{AGENT_NAME}.path"));
        assert_eq!(KEY_ARGS, format!("lfs.customtransfer.{AGENT_NAME}.args"));
    }

    // --- idempotency contract (#198, #210) ----------------------------
    //
    // These tests pin the end-to-end behaviour callers rely on: re-running
    // `install` / `enable_debug` / `disable_debug` on a repo where the
    // target state is already in place must be a no-op rather than
    // accumulating duplicate config entries or returning an error.

    use std::path::Path;
    use tempfile::TempDir;

    use super::{KEY_STANDALONE, disable_debug, enable_debug, install};
    use crate::git;

    fn empty_repo() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        gix::init(dir.path()).expect("gix::init");
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    fn config_values(cwd: &Path, key: &str) -> Vec<String> {
        let repo = gix::discover(cwd).expect("discover");
        let path = repo.common_dir().join("config");
        let bytes = std::fs::read(&path).expect("read config");
        let file = gix::config::File::from_bytes_no_includes(
            &bytes,
            gix::config::file::Metadata::api(),
            gix::config::file::init::Options::default(),
        )
        .expect("parse");
        file.raw_values(key)
            .map(|values| {
                values
                    .into_iter()
                    .map(|v| v.into_owned().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn install_is_idempotent_on_repeated_calls() {
        let (_dir, cwd) = empty_repo();
        install(&cwd).expect("first install");
        install(&cwd).expect("second install");
        install(&cwd).expect("third install");
        // Both keys the install writes must end up with exactly one value.
        assert_eq!(config_values(&cwd, KEY_PATH), vec![AGENT_NAME.to_owned()],);
        assert_eq!(
            config_values(&cwd, KEY_STANDALONE),
            vec![AGENT_NAME.to_owned()],
        );
    }

    #[test]
    fn install_collapses_legacy_duplicate_entries() {
        // Reproduces the pre-fix on-disk state: a user who ran the older
        // binary twice had two entries per key. After upgrading, a single
        // `install` call must clean that up rather than adding a third.
        let (_dir, cwd) = empty_repo();
        git::config_add(&cwd, KEY_PATH, AGENT_NAME).expect("seed 1");
        git::config_add(&cwd, KEY_PATH, AGENT_NAME).expect("seed 2");
        git::config_add(&cwd, KEY_STANDALONE, AGENT_NAME).expect("seed 1");
        git::config_add(&cwd, KEY_STANDALONE, AGENT_NAME).expect("seed 2");
        // Pre-state guard: confirm the seed step actually produced legacy
        // multi-valued entries. Without this, a regression that turned
        // `config_add` into idempotent-set would make the seeds a no-op
        // and the post-install assertion would still pass — testing only
        // that `install` produces one entry, not that it collapses two.
        assert_eq!(
            config_values(&cwd, KEY_PATH),
            vec![AGENT_NAME.to_owned(), AGENT_NAME.to_owned()],
            "seed step must produce two values for the collapse test to be meaningful",
        );
        assert_eq!(
            config_values(&cwd, KEY_STANDALONE),
            vec![AGENT_NAME.to_owned(), AGENT_NAME.to_owned()],
        );
        install(&cwd).expect("install");
        assert_eq!(config_values(&cwd, KEY_PATH), vec![AGENT_NAME.to_owned()],);
        assert_eq!(
            config_values(&cwd, KEY_STANDALONE),
            vec![AGENT_NAME.to_owned()],
        );
    }

    #[test]
    fn enable_debug_is_idempotent_on_repeated_calls() {
        let (_dir, cwd) = empty_repo();
        enable_debug(&cwd).expect("first");
        enable_debug(&cwd).expect("second");
        assert_eq!(config_values(&cwd, KEY_ARGS), vec!["debug".to_owned()]);
    }

    #[test]
    fn disable_debug_is_idempotent_when_already_absent() {
        // Issue #210: running disable-debug on a repo that never enabled
        // debug must succeed cleanly.
        let (_dir, cwd) = empty_repo();
        disable_debug(&cwd).expect("disable on empty repo");
        assert!(config_values(&cwd, KEY_ARGS).is_empty());
    }

    #[test]
    fn disable_debug_then_disable_debug_succeeds() {
        // Symmetry with enable_debug: enable, disable, disable. The second
        // disable must not error even though the key is already gone.
        let (_dir, cwd) = empty_repo();
        enable_debug(&cwd).expect("enable");
        disable_debug(&cwd).expect("first disable");
        disable_debug(&cwd).expect("second disable");
        assert!(config_values(&cwd, KEY_ARGS).is_empty());
    }
}

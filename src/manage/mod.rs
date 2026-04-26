//! Management CLI: `doctor`, `delete-branch`, `protect`, `unprotect`.
//!
//! These commands operate against the on-bucket object layout described in
//! `execution-plan.md` §1.1. They mirror the upstream Python
//! `git_remote_s3.manage` module
//! (`../git-remote-s3/git_remote_s3/manage.py`); behavioral parity is the
//! source of truth.
//!
//! The library entry points (`Doctor`, `ManageBranch`, `analyze`) take an
//! [`ObjectStore`][crate::object_store::ObjectStore] and a
//! [`Prompter`], so the binary, mock-backed unit tests, and any future
//! non-interactive frontend share the same code path.

pub mod branch;
pub mod doctor;
pub mod snapshot;

use std::io;

use thiserror::Error;

use crate::object_store::Error as ObjectStoreError;

/// Default lock TTL in seconds, matching the upstream Python value.
///
/// Mirrors `DEFAULT_LOCK_TTL_SECONDS` in
/// `../git-remote-s3/git_remote_s3/remote.py`.
pub const DEFAULT_LOCK_TTL_SECONDS: u64 = 60;

/// `true` iff `key` is a lock-file key. The `.lock` suffix is a
/// wire-format token on a case-sensitive S3/Azure key, not a filesystem
/// extension — clippy's case-insensitive-extension hint is silenced
/// once here so callers don't need to repeat the rationale.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
pub(super) fn is_lock_key(key: &str) -> bool {
    key.ends_with(".lock")
}

/// Errors surfaced by the management surface.
#[derive(Debug, Error)]
pub enum ManageError {
    /// Underlying object-store call failed.
    #[error(transparent)]
    Store(#[from] ObjectStoreError),

    /// `delete-branch` / `protect` / `unprotect` was invoked against a
    /// branch that has no objects under `<prefix>/refs/heads/<branch>/`.
    #[error("branch not found: {0}")]
    BranchNotFound(String),

    /// User cancelled an interactive prompt (Ctrl+C / EOF / explicit
    /// "no" on a destructive confirmation).
    #[error("operation cancelled")]
    Cancelled,

    /// I/O error from `dialoguer` or other non-store sources.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Interactive UI surface used by [`doctor`] and [`branch`].
///
/// Production binaries inject [`DialoguerPrompter`]; tests inject
/// [`ScriptedPrompter`] so prompt-driven flows can be exercised
/// deterministically without spawning the binary.
pub trait Prompter: Send + Sync {
    /// Ask the user to pick one of `options` by index. `prompt` is the
    /// short headline shown above the choices.
    fn select(&self, prompt: &str, options: &[String]) -> Result<usize, ManageError>;

    /// Ask the user a yes/no question. Returns `Ok(true)` for "yes" and
    /// `Ok(false)` for "no"; an EOF or signal returns
    /// [`ManageError::Cancelled`].
    fn confirm(&self, prompt: &str) -> Result<bool, ManageError>;
}

/// Default [`Prompter`] backed by the `dialoguer` crate.
///
/// Each method runs synchronously on the calling thread. Callers driving
/// the prompter from a `tokio::main` runtime should wrap calls in
/// [`tokio::task::spawn_blocking`] when responsiveness matters; the
/// management CLI today drives prompts serially between async I/O calls,
/// so a brief blocking read is acceptable.
#[derive(Debug, Default, Clone, Copy)]
pub struct DialoguerPrompter;

impl Prompter for DialoguerPrompter {
    fn select(&self, prompt: &str, options: &[String]) -> Result<usize, ManageError> {
        dialoguer::Select::new()
            .with_prompt(prompt)
            .items(options)
            .default(0)
            .interact()
            .map_err(into_dialoguer_error)
    }

    fn confirm(&self, prompt: &str) -> Result<bool, ManageError> {
        dialoguer::Confirm::new()
            .with_prompt(prompt)
            .default(false)
            .interact()
            .map_err(into_dialoguer_error)
    }
}

fn into_dialoguer_error(err: dialoguer::Error) -> ManageError {
    match err {
        dialoguer::Error::IO(io_err) if io_err.kind() == io::ErrorKind::Interrupted => {
            ManageError::Cancelled
        }
        dialoguer::Error::IO(io_err) => ManageError::Io(io_err),
    }
}

#[cfg(any(test, feature = "test-util"))]
pub use scripted::ScriptedPrompter;

#[cfg(any(test, feature = "test-util"))]
mod scripted {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::{ManageError, Prompter};

    /// Test-only [`Prompter`] that returns a queued answer for each prompt.
    ///
    /// Construct with [`ScriptedPrompter::new`], then drive one answer per
    /// call. Running out of answers returns [`ManageError::Cancelled`] —
    /// tests should queue exactly the answers they expect, so an unexpected
    /// extra prompt fails loudly.
    pub struct ScriptedPrompter {
        answers: Mutex<VecDeque<Answer>>,
    }

    /// One queued response in a [`ScriptedPrompter`] script.
    #[derive(Debug, Clone)]
    pub enum Answer {
        /// Reply to a `select` prompt with this index.
        Select(usize),
        /// Reply to a `confirm` prompt with this boolean.
        Confirm(bool),
        /// Treat the next prompt as cancelled.
        Cancel,
    }

    impl ScriptedPrompter {
        /// Build a prompter that returns `answers` in order.
        #[must_use]
        pub fn new(answers: impl IntoIterator<Item = Answer>) -> Self {
            Self {
                answers: Mutex::new(answers.into_iter().collect()),
            }
        }

        /// Number of queued answers not yet consumed. Tests assert this is
        /// `0` to catch over-armed scripts.
        ///
        /// # Panics
        ///
        /// Panics if the inner mutex was poisoned by a previous panic
        /// while holding the lock.
        #[must_use]
        pub fn remaining(&self) -> usize {
            self.answers.lock().expect("scripted mutex poisoned").len()
        }

        fn pop(&self) -> Result<Answer, ManageError> {
            self.answers
                .lock()
                .expect("scripted mutex poisoned")
                .pop_front()
                .ok_or(ManageError::Cancelled)
        }
    }

    impl Prompter for ScriptedPrompter {
        fn select(&self, _prompt: &str, _options: &[String]) -> Result<usize, ManageError> {
            match self.pop()? {
                Answer::Select(i) => Ok(i),
                Answer::Cancel => Err(ManageError::Cancelled),
                Answer::Confirm(_) => panic!("expected Select answer, got Confirm"),
            }
        }

        fn confirm(&self, _prompt: &str) -> Result<bool, ManageError> {
            match self.pop()? {
                Answer::Confirm(b) => Ok(b),
                Answer::Cancel => Err(ManageError::Cancelled),
                Answer::Select(_) => panic!("expected Confirm answer, got Select"),
            }
        }
    }
}

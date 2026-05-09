//! Management CLI: `doctor`, `delete-branch`, `protect`, `unprotect`.
//!
//! These commands operate against the same on-bucket object layout as
//! the helper protocol (bundles under `<prefix>/<ref>/`, `PROTECTED#`
//! markers, lock files).
//!
//! The library entry points (`Doctor`, `ManageBranch`) take an
//! [`ObjectStore`][crate::object_store::ObjectStore] and a
//! [`Prompter`], so the binary, mock-backed unit tests, and any future
//! non-interactive frontend share the same code path.

pub mod branch;
pub mod compact;
pub mod doctor;
pub mod gc;
pub(crate) mod snapshot;

use std::io;

use thiserror::Error;

use crate::object_store::ObjectStoreError;

/// Default lock TTL in seconds.
pub const DEFAULT_LOCK_TTL_SECONDS: u64 = 60;

/// `true` iff `key` is a lock-file key. The `.lock` suffix is a
/// wire-format token on a case-sensitive S3/Azure key, not a filesystem
/// extension — clippy's case-insensitive-extension hint is silenced
/// once here so callers don't need to repeat the rationale.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
pub(crate) fn is_lock_key(key: &str) -> bool {
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

    /// Branch name failed `gix-validate`'s strict ref-name check; we
    /// reject these at the management boundary so a value like
    /// `foo/../bar` cannot land as a literal substring of a stored
    /// object key.
    #[error("invalid branch name: {0}")]
    InvalidBranch(String),

    /// User cancelled an interactive prompt via Ctrl+C or EOF. A
    /// deliberate "no" on a confirmation prompt is not an error —
    /// callers (`ManageBranch::delete`, `fix_multiple_bundles`) print
    /// "Aborted" and return `Ok(())`.
    #[error("operation cancelled")]
    Cancelled,

    /// I/O error from `dialoguer` or other non-store sources.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// A defensive invariant inside the management code was violated —
    /// for example a snapshot map lookup that the caller had previously
    /// proven to exist, or a prompter returning an out-of-range index.
    /// These should not happen in practice; surfacing them as a typed
    /// error keeps the helper from aborting the process.
    #[error("internal management error: {0}")]
    Internal(String),

    /// Packchain engine surface error. Surfaced by the `doctor`'s
    /// engine-aware audit path. Carries the typed source so the
    /// `main`-level downcast can recognise transport failures and
    /// emit the categorical `fatal:` line.
    #[error(transparent)]
    Packchain(#[from] crate::packchain::PackchainError),
}

/// Interactive UI surface used by [`doctor`] and [`branch`].
///
/// Production binaries inject [`DialoguerPrompter`]; tests inject
/// [`ScriptedPrompter`] so prompt-driven flows can be exercised
/// deterministically without spawning the binary.
pub trait Prompter: Send + Sync {
    /// Ask the user to pick one of `options` by index. `prompt` is the
    /// short headline shown above the choices.
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::Cancelled`] if the user aborts (Ctrl+C or
    /// EOF), or [`ManageError::Io`] for underlying I/O failures.
    fn select(&self, prompt: &str, options: &[String]) -> Result<usize, ManageError>;

    /// Ask the user a yes/no question. Returns `Ok(true)` for "yes" and
    /// `Ok(false)` for "no".
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::Cancelled`] on EOF or signal, or
    /// [`ManageError::Io`] for underlying I/O failures.
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
        Ok(dialoguer::Select::new()
            .with_prompt(prompt)
            .items(options)
            .default(0)
            .interact()?)
    }

    fn confirm(&self, prompt: &str) -> Result<bool, ManageError> {
        Ok(dialoguer::Confirm::new()
            .with_prompt(prompt)
            .default(false)
            .interact()?)
    }
}

impl From<dialoguer::Error> for ManageError {
    fn from(err: dialoguer::Error) -> Self {
        match err {
            dialoguer::Error::IO(io_err) if io_err.kind() == io::ErrorKind::Interrupted => {
                ManageError::Cancelled
            }
            dialoguer::Error::IO(io_err) => ManageError::Io(io_err),
        }
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

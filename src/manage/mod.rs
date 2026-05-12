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

use std::fmt;
use std::io;

use thiserror::Error;

use crate::keys;
use crate::object_store::{ObjectMeta, ObjectStoreError};

/// `fmt = ...` helper for [`ManageError::PartialDelete`]. Branches on
/// `n_undeleted == 1` so the operator-facing wording reads "1 key" /
/// "<N> keys" instead of always-plural "<N> keys".
///
/// thiserror's `fmt = path` hook calls this with each variant field
/// passed by reference and the formatter as the final argument; we
/// silence `clippy::{ptr_arg, trivially_copy_pass_by_ref}` because
/// thiserror controls the signature, not us.
#[allow(clippy::ptr_arg, clippy::trivially_copy_pass_by_ref)]
fn fmt_partial_delete(
    branch: &String,
    undeleted: &Vec<String>,
    attempted: &usize,
    f: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    let n = undeleted.len();
    let noun = if n == 1 { "key" } else { "keys" };
    write!(
        f,
        "delete-branch {branch} failed: {n} of {attempted} {noun} could not be deleted: {} (retry to converge)",
        undeleted.join(", "),
    )
}

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

/// `true` iff `entries` contains at least one key that represents real
/// branch data — i.e. NOT a lock file and NOT a `PROTECTED#` marker.
///
/// A branch whose only residue is operational metadata (a stale
/// `*.lock` or a previously-written `PROTECTED#` marker) is treated as
/// gone for the purposes of "does the branch still exist on the
/// bucket?" — those keys are coordination state, not user-visible
/// branch data. Both `ManageBranch::protect` (issue #137) and
/// `Doctor::fix_head` (issue #138) consult this helper before writing
/// state that would otherwise re-anchor against a deleted branch.
pub(crate) fn has_branch_data(entries: &[ObjectMeta]) -> bool {
    entries.iter().any(|entry| {
        let last = entry
            .key
            .rsplit_once('/')
            .map_or(entry.key.as_str(), |(_, s)| s);
        !is_lock_key(&entry.key) && !keys::is_protected_marker_segment(last)
    })
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

    /// `delete-branch` was invoked against a branch that has a
    /// `PROTECTED#` marker. Mirrors the refusal the helper-protocol
    /// delete path emits so both surfaces share one wording.
    #[error(
        "ref is protected. Run git-remote-object-store unprotect <url> <branch> to remove protection before deleting."
    )]
    Protected(String),

    /// `delete-branch` swept the fresh listing but one or more per-key
    /// deletes failed with a non-`NotFound` error. The loop continues
    /// past each transient failure so the caller has a complete
    /// inventory of which keys survived; this variant carries that
    /// inventory verbatim.
    ///
    /// Retry on the same branch is naturally idempotent — the re-list
    /// at the start of the next `delete` call will only show the
    /// surviving keys, and the same loop will try to delete them. A
    /// `NotFound` mid-sweep is tolerated and counts as success, so the
    /// `undeleted` field is strictly the set of keys whose deletes
    /// raised something else (Network, `AccessDenied`, etc.).
    #[error(fmt = fmt_partial_delete)]
    PartialDelete {
        /// Branch the sweep ran against.
        branch: String,
        /// Keys whose per-key delete returned a non-`NotFound` error.
        /// Stored verbatim so a retry-by-key tool can target exactly
        /// what survived.
        undeleted: Vec<String>,
        /// Total number of keys the fresh listing yielded, for the
        /// operator-facing "N of M" framing in the error message.
        attempted: usize,
    },

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

    /// `doctor`'s top-of-run snapshot disagreed with a fresh re-check
    /// taken immediately before a mutating write — the on-bucket state
    /// changed under us between the snapshot LIST and the write. The
    /// canonical case (issue #138) is `fix_head` racing against a
    /// concurrent `git push :<branch>` or `manage delete-branch`: the
    /// operator picks a HEAD candidate from the snapshot, but by the
    /// time the prompt returns the chosen branch has been deleted.
    /// Writing HEAD anyway would reproduce the invalid-HEAD condition
    /// the doctor was trying to fix.
    ///
    /// Carries the entity whose presence was re-verified (e.g.
    /// `"refs/heads/main"`) so the operator-facing message names the
    /// branch and tells them to re-run the doctor.
    #[error("doctor snapshot is stale: {0} was deleted between selection and write; re-run doctor")]
    StaleSnapshot(String),

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
/// `ScriptedPrompter` (gated on `test-util`) so prompt-driven flows
/// can be exercised deterministically without spawning the binary.
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

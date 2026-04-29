//! Local-branch primitives.
//!
//! This project never creates / deletes / renames / lists local
//! branches; the helper protocol delegates those to `git` itself. We
//! only need three operations: resolve a rev-spec to an OID, validate
//! a branch ref name, and read which branch HEAD points at. They are
//! grouped here so the surface stays small and explicit.

use std::fmt;

use gix::Repository;
use gix::bstr::BStr;
use thiserror::Error;

use super::{GitError, Sha};

const HEADS_PREFIX: &str = "refs/heads/";

/// Validated local-branch ref name.
///
/// Stores the fully-qualified form (`refs/heads/<name>`); the short
/// form is recovered by stripping the constant prefix.
// `BranchName` and its accessors are introduced ahead of their first
// production consumer (issue #47); the test module exercises them.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BranchName(String);

#[allow(dead_code)]
impl BranchName {
    /// Validate `short` (a name without the `refs/heads/` prefix) and
    /// wrap it.
    ///
    /// # Errors
    ///
    /// Returns [`BranchNameError::Empty`] if `short` is empty,
    /// [`BranchNameError::HasRefsPrefix`] if `short` starts with
    /// `refs/` (likely caller mistake — use [`Self::from_full`]), or
    /// [`BranchNameError::Invalid`] if the resulting full name fails
    /// `gix-validate::reference::name`.
    pub(crate) fn from_short(short: &str) -> Result<Self, BranchNameError> {
        if short.is_empty() {
            return Err(BranchNameError::Empty);
        }
        if short.starts_with("refs/") {
            return Err(BranchNameError::HasRefsPrefix(short.to_owned()));
        }
        let full = format!("{HEADS_PREFIX}{short}");
        match gix_validate::reference::name(BStr::new(&full)) {
            Ok(_) => Ok(BranchName(full)),
            Err(source) => Err(BranchNameError::Invalid { name: full, source }),
        }
    }

    /// Validate `full` (must start with `refs/heads/`) and wrap it.
    ///
    /// # Errors
    ///
    /// Returns [`BranchNameError::Empty`] for an empty input,
    /// [`BranchNameError::NotUnderHeads`] if the input does not begin
    /// with `refs/heads/`, or [`BranchNameError::Invalid`] if
    /// `gix-validate::reference::name` rejects the input.
    pub(crate) fn from_full(full: impl Into<String>) -> Result<Self, BranchNameError> {
        let full = full.into();
        if full.is_empty() {
            return Err(BranchNameError::Empty);
        }
        if !full.starts_with(HEADS_PREFIX) {
            return Err(BranchNameError::NotUnderHeads(full));
        }
        match gix_validate::reference::name(BStr::new(&full)) {
            Ok(_) => Ok(BranchName(full)),
            Err(source) => Err(BranchNameError::Invalid { name: full, source }),
        }
    }

    /// The `<name>` portion (everything after `refs/heads/`).
    pub(crate) fn short(&self) -> &str {
        &self.0[HEADS_PREFIX.len()..]
    }

    /// The fully-qualified form, e.g. `"refs/heads/main"`.
    pub(crate) fn full(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for BranchName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<BranchName> for String {
    fn from(value: BranchName) -> Self {
        value.0
    }
}

/// Error returned by [`BranchName::from_short`] / [`BranchName::from_full`].
#[allow(dead_code)]
#[derive(Debug, Error)]
pub(crate) enum BranchNameError {
    /// Caller passed an empty string.
    #[error("branch name is empty")]
    Empty,
    /// `from_short` was called with input that already starts with `refs/`.
    #[error("branch short name {0:?} starts with `refs/` — use BranchName::from_full")]
    HasRefsPrefix(String),
    /// `from_full` was called with input not under `refs/heads/`.
    #[error("ref {0:?} is not under refs/heads/")]
    NotUnderHeads(String),
    /// `gix-validate` rejected the (full-form) name.
    #[error("invalid branch name {name:?}: {source}")]
    Invalid {
        /// The rejected full-form name.
        name: String,
        /// Underlying gix-validate error.
        #[source]
        source: gix_validate::reference::name::Error,
    },
}

/// Resolve a rev-spec (a ref name, full or short SHA, `HEAD~n`, etc.)
/// to the canonical 40-hex commit OID it points at.
///
/// # Errors
///
/// Returns [`GitError::EmptySpec`] if `spec` is empty, or
/// [`GitError::RevParse`] if the spec cannot be resolved to an object.
pub(crate) fn resolve(repo: &Repository, spec: &str) -> Result<Sha, GitError> {
    if spec.is_empty() {
        return Err(GitError::EmptySpec);
    }
    let id = repo.rev_parse_single(BStr::new(spec))?;
    Ok(Sha::from_object_id(id.detach()))
}

/// Return the branch HEAD points at, or `None` if HEAD is detached,
/// unborn, or pointing outside the `refs/heads/` namespace.
///
/// "Unborn" covers the fresh `git init` state where HEAD symbolically
/// references a branch that does not yet exist (e.g. `refs/heads/main`
/// with no commits). "Detached" covers HEAD storing an OID directly.
/// HEAD pointing at a non-`refs/heads/` ref (rare) is also reported as
/// `None`, since the upstream-tracking helpers this primitive is
/// intended to support are only meaningful for branches.
///
/// # Errors
///
/// Returns [`GitError::HeadLookup`] if the HEAD reference cannot be
/// read or [`GitError::NonUtf8HeadRef`] if the underlying ref name is
/// not valid UTF-8.
#[allow(dead_code)]
pub(crate) fn current(repo: &Repository) -> Result<Option<BranchName>, GitError> {
    // `head_ref()` returns `None` for both detached and unborn HEAD,
    // matching the documented `current()` semantics in one call.
    let Some(reference) = repo.head_ref()? else {
        return Ok(None);
    };
    let name_str = std::str::from_utf8(reference.name().as_bstr())
        .map_err(|source| GitError::NonUtf8HeadRef { source })?;
    if !name_str.starts_with(HEADS_PREFIX) {
        return Ok(None);
    }
    // gix-ref validates ref names on disk via `gix-validate`, so any name
    // surfaced through `head_ref()` is already well-formed. The prefix
    // check above narrows the namespace; `from_full` will only fail here
    // if gix-ref's invariant is violated.
    let branch = BranchName::from_full(name_str).expect("gix-ref validated on-disk ref names");
    Ok(Some(branch))
}

#[cfg(test)]
mod tests {
    use super::*;

    use gix::actor::SignatureRef;
    use gix_hash::ObjectId;
    use tempfile::TempDir;

    fn signature() -> SignatureRef<'static> {
        SignatureRef {
            name: BStr::new("Test"),
            email: BStr::new("test@example.com"),
            time: "0 +0000",
        }
    }

    fn empty_repo() -> (Repository, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let repo = gix::init(dir.path()).expect("gix::init");
        (repo, dir)
    }

    fn add_commit(
        repo: &Repository,
        ref_name: &str,
        parents: &[ObjectId],
        message: &str,
    ) -> ObjectId {
        use gix::objs::tree::{Entry, EntryKind};
        let blob_id = repo.write_blob(b"hello\n").expect("write blob").detach();
        let tree = gix::objs::Tree {
            entries: vec![Entry {
                mode: EntryKind::Blob.into(),
                filename: "marker".into(),
                oid: blob_id,
            }],
        };
        let tree_id = repo.write_object(&tree).expect("write tree").detach();
        repo.commit_as(
            signature(),
            signature(),
            ref_name,
            message,
            tree_id,
            parents.iter().copied(),
        )
        .expect("commit_as")
        .detach()
    }

    // --- BranchName ---------------------------------------------------

    #[test]
    fn from_full_accepts_canonical() {
        let main = BranchName::from_full("refs/heads/main").expect("from_full");
        assert_eq!(main.full(), "refs/heads/main");
        assert_eq!(main.short(), "main");
        let nested = BranchName::from_full("refs/heads/feature/x").expect("from_full");
        assert_eq!(nested.full(), "refs/heads/feature/x");
        assert_eq!(nested.short(), "feature/x");
    }

    #[test]
    fn from_full_rejects_each_invalid_category() {
        let cases: &[&str] = &[
            "refs/heads/.hidden",
            "refs/heads/foo..bar",
            "refs/heads/foo bar",
            "refs/heads/",
            "refs/heads/main.lock",
            "refs/heads/main@{x}",
            "refs/heads//main",
            "refs/heads/main\x01",
            "refs/heads/?bad",
            "refs/heads/[bad]",
            "refs/heads/^bad",
            "refs/heads/~bad",
            "refs/heads/*bad",
            "refs/heads/:bad",
        ];
        for name in cases {
            assert!(
                matches!(
                    BranchName::from_full(*name),
                    Err(BranchNameError::Invalid { .. })
                ),
                "expected Invalid for {name:?}",
            );
        }
    }

    #[test]
    fn from_full_rejects_non_heads_namespace() {
        assert!(matches!(
            BranchName::from_full("refs/tags/v1"),
            Err(BranchNameError::NotUnderHeads(_))
        ));
        assert!(matches!(
            BranchName::from_full("refs/remotes/origin/main"),
            Err(BranchNameError::NotUnderHeads(_))
        ));
    }

    #[test]
    fn from_full_rejects_empty() {
        assert!(matches!(
            BranchName::from_full(""),
            Err(BranchNameError::Empty)
        ));
    }

    #[test]
    fn from_short_accepts_simple_and_slashed() {
        assert_eq!(
            BranchName::from_short("main").expect("from_short").full(),
            "refs/heads/main"
        );
        assert_eq!(
            BranchName::from_short("feature/x")
                .expect("from_short")
                .full(),
            "refs/heads/feature/x"
        );
    }

    #[test]
    fn from_short_rejects_empty() {
        assert!(matches!(
            BranchName::from_short(""),
            Err(BranchNameError::Empty)
        ));
    }

    #[test]
    fn from_short_rejects_refs_prefix() {
        assert!(matches!(
            BranchName::from_short("refs/heads/main"),
            Err(BranchNameError::HasRefsPrefix(_))
        ));
        assert!(matches!(
            BranchName::from_short("refs/tags/v1"),
            Err(BranchNameError::HasRefsPrefix(_))
        ));
    }

    #[test]
    fn from_short_rejects_invalid_resulting_full() {
        for name in &["main..bad", "main.lock", ".hidden"] {
            assert!(
                matches!(
                    BranchName::from_short(name),
                    Err(BranchNameError::Invalid { .. })
                ),
                "expected Invalid for {name:?}",
            );
        }
    }

    #[test]
    fn short_round_trips() {
        let from_full = BranchName::from_full("refs/heads/feature/x").expect("from_full");
        assert_eq!(from_full.short(), "feature/x");
        let from_short = BranchName::from_short("feature/x").expect("from_short");
        assert_eq!(from_short.short(), "feature/x");
        assert_eq!(from_full, from_short);
    }

    #[test]
    fn display_emits_full_form() {
        let b = BranchName::from_short("main").expect("from_short");
        assert_eq!(b.to_string(), "refs/heads/main");
    }

    #[test]
    fn from_branch_name_for_string_emits_full_form() {
        let b = BranchName::from_short("main").expect("from_short");
        let s: String = b.into();
        assert_eq!(s, "refs/heads/main");
    }

    // --- resolve ------------------------------------------------------

    #[test]
    fn resolve_resolves_branch_ref() {
        let (repo, _dir) = empty_repo();
        let oid = add_commit(&repo, "refs/heads/main", &[], "first");
        let sha = resolve(&repo, "refs/heads/main").expect("resolve");
        assert_eq!(sha.as_object_id(), &oid);
    }

    #[test]
    fn resolve_resolves_full_sha() {
        let (repo, _dir) = empty_repo();
        let oid = add_commit(&repo, "refs/heads/main", &[], "first");
        let hex = oid.to_string();
        let sha = resolve(&repo, &hex).expect("resolve");
        assert_eq!(sha.as_object_id(), &oid);
    }

    #[test]
    fn resolve_unknown_returns_error() {
        let (repo, _dir) = empty_repo();
        add_commit(&repo, "refs/heads/main", &[], "first");
        assert!(resolve(&repo, "refs/heads/does-not-exist").is_err());
    }

    #[test]
    fn resolve_empty_returns_empty_spec() {
        let (repo, _dir) = empty_repo();
        add_commit(&repo, "refs/heads/main", &[], "first");
        assert!(matches!(resolve(&repo, ""), Err(GitError::EmptySpec)));
    }

    // --- current ------------------------------------------------------

    #[test]
    fn current_returns_branch_after_first_commit() {
        let (repo, _dir) = empty_repo();
        add_commit(&repo, "refs/heads/main", &[], "first");
        let branch = current(&repo).expect("current").expect("Some(branch)");
        assert_eq!(branch.short(), "main");
        assert_eq!(branch.full(), "refs/heads/main");
    }

    #[test]
    fn current_returns_none_for_unborn_head() {
        let (repo, _dir) = empty_repo();
        assert!(current(&repo).expect("current").is_none());
    }

    #[test]
    fn current_returns_none_for_detached_head() {
        let (repo, _dir) = empty_repo();
        let oid = add_commit(&repo, "refs/heads/main", &[], "first");
        std::fs::write(repo.git_dir().join("HEAD"), format!("{oid}\n")).expect("write HEAD");
        let repo = gix::open(repo.git_dir()).expect("reopen");
        assert!(current(&repo).expect("current").is_none());
    }
}

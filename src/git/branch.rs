//! Local-branch primitives.
//!
//! This project never creates / deletes / renames / lists local
//! branches; the helper protocol delegates those to `git` itself. The
//! only operation we need today is resolving a rev-spec to an OID.

use gix::Repository;
use gix::bstr::BStr;

use super::{GitError, Sha};

/// Resolve a rev-spec (a ref name, full or short SHA, `HEAD~n`, etc.)
/// to the OID of whatever object it points at — commit, annotated tag,
/// tree, or blob. The returned [`Sha`] is the object's own OID, not the
/// commit reached after peeling tags. Callers that need to traverse an
/// annotated-tag chain (the common case for refs that may point at
/// `tag` objects) must invoke [`peel_tag_chain`][crate::git::peel_tag_chain]
/// on the result.
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
}

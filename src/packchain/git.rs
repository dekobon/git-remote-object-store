//! Git-side helpers used by the packchain engine.
//!
//! Sits in the packchain module rather than `crate::git` so the
//! generic gix wrapper does not have to import packchain schema
//! types. Push calls [`extract_path_index`] right before writing
//! `path-index.json`.

use std::collections::BTreeMap;
use std::str;

use gix::Repository;
use gix::object::Kind;
use gix_hash::ObjectId;

use crate::git::Sha;

use super::PackchainError;
use super::schema::{PathIndex, PathNode, Sha40};

/// Walk the tree at `tip`'s commit and build a [`PathIndex`].
///
/// Submodule entries (`EntryKind::Commit` — gitlink mode 160000) are
/// skipped: their target lives in another repository, so there is no
/// local blob to record. Symlinks (`EntryKind::Link`) are recorded as
/// blobs with the link target's blob SHA, matching git's tree
/// representation.
///
/// Filenames must be valid UTF-8. Git allows arbitrary bytes in tree
/// entry names, but the on-bucket JSON layer cannot represent
/// non-UTF-8 keys without a lossy encoding (and lossy encoding for
/// identifiers is banned by `.claude/rules/rust.md`). A non-UTF-8
/// filename surfaces as [`PackchainError::InvalidPath`].
///
/// **Recursion**: this implementation uses native call-stack recursion
/// for tree descent. Real-world git trees are shallow (the Linux
/// kernel sits at ~30 levels) and Rust's default stack (8 MiB) handles
/// thousands of levels comfortably. Revisit if a hostile repository
/// targets the helper with a pathologically deep tree; until then, the
/// simple recursive shape is a deliberate trade-off favouring
/// readability.
///
/// # Errors
///
/// - [`PackchainError::ParseJson`]: never (no JSON parse here);
///   reserved for the push call site.
/// - [`PackchainError::InvalidSha`]: cannot fire — every blob OID we
///   read from gix is a valid 40-hex SHA-1.
/// - [`PackchainError::Git`]: any underlying gix failure (object
///   missing, decode error, walk error).
/// - [`PackchainError::InvalidPath`]: a tree entry's filename is not
///   valid UTF-8.
pub(crate) fn extract_path_index(repo: &Repository, tip: Sha) -> Result<PathIndex, PackchainError> {
    let commit = repo
        .find_object(*tip.as_object_id())
        .map_err(crate::git::GitError::from)?
        .peel_to_kind(Kind::Commit)
        .map_err(crate::git::GitError::from)?
        .into_commit();
    let tree_id = commit
        .tree_id()
        .map_err(crate::git::GitError::from)?
        .detach();
    let mut root: BTreeMap<String, PathNode> = BTreeMap::new();
    walk_tree(repo, tree_id, &mut root)?;
    Ok(PathIndex {
        v: PathIndex::SCHEMA_VERSION,
        commit: Sha40::try_new(tip.to_string())?,
        tree: root,
    })
}

/// Recursive worker. Inserts an entry into `out` for every blob /
/// symlink at this tree level, and recurses into subtrees.
fn walk_tree(
    repo: &Repository,
    tree_id: ObjectId,
    out: &mut BTreeMap<String, PathNode>,
) -> Result<(), PackchainError> {
    use gix::objs::tree::EntryKind;

    let tree = repo
        .find_object(tree_id)
        .map_err(crate::git::GitError::from)?
        .peel_to_kind(Kind::Tree)
        .map_err(crate::git::GitError::from)?
        .into_tree();
    for entry in tree.iter() {
        let entry = entry.map_err(crate::git::GitError::from)?;
        let filename = entry.filename();
        let name = str::from_utf8(filename).map_err(|_| PackchainError::InvalidPath {
            bytes: filename.to_vec(),
        })?;
        // `gix_hash::oid` and `ObjectId` both render as 40-lowercase-hex
        // via `Display`; `Sha40::try_new` accepts that shape directly.
        match entry.kind() {
            EntryKind::Tree => {
                let mut subtree: BTreeMap<String, PathNode> = BTreeMap::new();
                walk_tree(repo, entry.oid().to_owned(), &mut subtree)?;
                out.insert(name.to_owned(), PathNode::Tree(subtree));
            }
            EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => {
                let sha = Sha40::try_new(entry.oid().to_string())?;
                out.insert(name.to_owned(), PathNode::Blob(sha));
            }
            EntryKind::Commit => {
                // Submodule / gitlink. The target lives in another
                // repo; nothing local to record. Skipping is the same
                // contract `git ls-tree` exposes via mode 160000.
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use gix::actor::SignatureRef;
    use gix::bstr::BStr;
    use gix::objs::tree::{Entry, EntryKind};
    use tempfile::TempDir;

    fn signature() -> SignatureRef<'static> {
        SignatureRef {
            name: BStr::new("Tester"),
            email: BStr::new("t@example.com"),
            time: "0 +0000",
        }
    }

    /// Build a fixture repo with this layout:
    ///
    /// ```text
    /// Cargo.toml
    /// src/
    ///   inner/
    ///     deep.rs
    ///   main.rs
    /// ```
    ///
    /// Tree entries are written in lexicographic order per git's tree
    /// canonicalisation rule (gix does not re-sort).
    ///
    /// Returns the repo plus the tip commit's [`Sha`].
    fn fixture_repo() -> (gix::Repository, TempDir, Sha) {
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();

        let cargo = repo.write_blob(b"cargo body").unwrap().detach();
        let main_rs = repo.write_blob(b"fn main(){}").unwrap().detach();
        let deep = repo.write_blob(b"// deep").unwrap().detach();

        let inner_tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![Entry {
                    mode: EntryKind::Blob.into(),
                    filename: "deep.rs".into(),
                    oid: deep,
                }],
            })
            .unwrap()
            .detach();

        let src_tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![
                    Entry {
                        mode: EntryKind::Tree.into(),
                        filename: "inner".into(),
                        oid: inner_tree,
                    },
                    Entry {
                        mode: EntryKind::Blob.into(),
                        filename: "main.rs".into(),
                        oid: main_rs,
                    },
                ],
            })
            .unwrap()
            .detach();

        let root_tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![
                    Entry {
                        mode: EntryKind::Blob.into(),
                        filename: "Cargo.toml".into(),
                        oid: cargo,
                    },
                    Entry {
                        mode: EntryKind::Tree.into(),
                        filename: "src".into(),
                        oid: src_tree,
                    },
                ],
            })
            .unwrap()
            .detach();

        let commit = repo
            .commit_as(
                signature(),
                signature(),
                "refs/heads/main",
                "initial",
                root_tree,
                std::iter::empty::<ObjectId>(),
            )
            .unwrap()
            .detach();
        let tip = Sha::from_object_id(commit);
        (repo, tmp, tip)
    }

    #[test]
    fn extract_path_index_reflects_nested_layout() {
        let (repo, _guard, tip) = fixture_repo();
        let index = extract_path_index(&repo, tip).expect("extract");

        assert_eq!(index.v, PathIndex::SCHEMA_VERSION);
        assert_eq!(index.commit.as_str(), tip.to_string());

        // Root has Cargo.toml (Blob) and src (Tree).
        assert_eq!(index.tree.len(), 2);
        assert!(matches!(
            index.tree.get("Cargo.toml"),
            Some(PathNode::Blob(_))
        ));
        let src = index.tree.get("src").expect("src present");
        let PathNode::Tree(src_children) = src else {
            panic!("expected src to be a Tree, got {src:?}");
        };
        // src has main.rs (Blob) and inner (Tree).
        assert_eq!(src_children.len(), 2);
        assert!(matches!(
            src_children.get("main.rs"),
            Some(PathNode::Blob(_)),
        ));
        let inner = src_children.get("inner").expect("inner present");
        let PathNode::Tree(inner_children) = inner else {
            panic!("expected inner to be a Tree, got {inner:?}");
        };
        // inner has deep.rs (Blob).
        assert_eq!(inner_children.len(), 1);
        assert!(matches!(
            inner_children.get("deep.rs"),
            Some(PathNode::Blob(_)),
        ));
    }

    #[test]
    fn extract_path_index_round_trips_via_json() {
        // Walk → serialise → parse → compare. Pins the contract that
        // anything `extract_path_index` produces is a valid v=1
        // `path-index.json`.
        let (repo, _guard, tip) = fixture_repo();
        let index = extract_path_index(&repo, tip).unwrap();
        let bytes = index.to_json_pretty().unwrap();
        let decoded = PathIndex::from_json_bytes(&bytes).unwrap();
        assert_eq!(decoded, index);
    }

    #[test]
    fn extract_path_index_rejects_non_utf8_filename() {
        // Git allows arbitrary bytes in tree entry names. The
        // on-bucket JSON layer can't represent non-UTF-8 keys without
        // lossy encoding (banned for identifiers), so the walker
        // must surface `PackchainError::InvalidPath`. Verifies the
        // dead-by-default branch in `walk_tree`.
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();
        let blob = repo.write_blob(b"x").unwrap().detach();

        // 0x80 is a UTF-8 continuation byte without a leading byte —
        // never valid UTF-8. Wrap in two ASCII bytes so the corruption
        // is mid-name, mirroring real-world non-UTF-8 filenames from
        // legacy locale-encoded git history.
        let filename = gix::bstr::BString::from(vec![b'a', 0x80, b'b']);

        let tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![Entry {
                    mode: EntryKind::Blob.into(),
                    filename,
                    oid: blob,
                }],
            })
            .unwrap()
            .detach();

        let commit = repo
            .commit_as(
                signature(),
                signature(),
                "refs/heads/bad",
                "non-utf8 filename",
                tree,
                std::iter::empty::<ObjectId>(),
            )
            .unwrap()
            .detach();
        let tip = Sha::from_object_id(commit);

        let err = extract_path_index(&repo, tip).expect_err("non-UTF-8 filename must reject");
        assert!(
            matches!(err, PackchainError::InvalidPath { ref bytes } if bytes == &[b'a', 0x80, b'b']),
            "expected InvalidPath with offending bytes, got {err:?}",
        );
        // Sanity-check the Display rendering uses lossy UTF-8
        // (replacement char) for the diagnostic line, since the
        // original bytes can't be rendered as a clean string.
        let msg = err.to_string();
        assert!(
            msg.starts_with("invalid path: a"),
            "expected lossy-UTF-8 diagnostic, got {msg}",
        );
    }

    #[test]
    fn extract_path_index_keeps_paths_for_existing_repo() {
        // Re-open by `gix::open(path)` to confirm the helper works on
        // a `Repository` that was opened (not just one returned by
        // `gix::init`). Phase 2's push does the open-then-walk dance.
        let (_repo_inmem, guard, tip) = fixture_repo();
        let opened = gix::open(guard.path()).expect("re-open");
        let index = extract_path_index(&opened, tip).expect("extract on opened");
        assert!(index.tree.contains_key("Cargo.toml"));
        assert!(index.tree.contains_key("src"));
    }
}

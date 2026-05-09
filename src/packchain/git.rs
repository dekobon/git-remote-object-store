//! Git-side helpers used by the packchain engine.
//!
//! Sits in the packchain module rather than `crate::git` so the
//! generic gix wrapper does not have to import packchain schema
//! types. Push calls [`extract_path_index`] right before writing
//! `path-index.json`.

use std::collections::{BTreeMap, HashSet};
use std::str;

use gix::Repository;
use gix::object::Kind;
use gix_hash::ObjectId;

use crate::git::{PeeledTip, Sha};

use super::PackchainError;
use super::schema::{PathIndex, PathNode, Sha40};

/// Walk the tree associated with `peeled` and build a [`PathIndex`].
///
/// `unpeeled_tip` is the chain.tip recorded on the bucket — the
/// outermost tag OID for tag refs, the commit OID for branch refs,
/// the tree OID for a bare-tree ref, and (for blob-tipped refs that
/// short-circuit via `Ok(None)` below) the blob OID, though it goes
/// unused in that branch. It is stored verbatim in the
/// [`PathIndex::tip`] field so a reader of `path-index.json` can
/// correlate the index back to the chain entry that produced it.
///
/// Returns `Ok(None)` for blob-tipped chains (annotated tag of blob,
/// or a bare ref pointing at a blob) — there is no tree to index, so
/// the engine omits `path-index.json` entirely.
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
/// - [`PackchainError::TreeCycle`]: a tree references itself directly
///   or transitively on the current descent. Impossible in a healthy
///   ODB (content-addressing rules out cycles); fires on corrupted or
///   adversarial repositories so the walk cannot run unbounded.
pub(crate) fn extract_path_index(
    repo: &Repository,
    peeled: &PeeledTip,
    unpeeled_tip: Sha,
) -> Result<Option<PathIndex>, PackchainError> {
    let tree_id = match peeled {
        PeeledTip::Commit { commit, .. } => repo
            .find_object(*commit.as_object_id())
            .map_err(crate::git::GitError::from)?
            .peel_to_kind(Kind::Commit)
            .map_err(crate::git::GitError::from)?
            .into_commit()
            .tree_id()
            .map_err(crate::git::GitError::from)?
            .detach(),
        PeeledTip::Tree { tree, .. } => *tree,
        PeeledTip::Blob { .. } => return Ok(None),
    };
    let mut root: BTreeMap<String, PathNode> = BTreeMap::new();
    let mut ancestors: HashSet<ObjectId> = HashSet::new();
    walk_tree(repo, tree_id, &mut root, &mut ancestors)?;
    Ok(Some(PathIndex {
        v: PathIndex::SCHEMA_VERSION,
        tip: Sha40::try_new(unpeeled_tip.to_string())?,
        tree: root,
    }))
}

/// Enumerate every distinct OID inside the tree closure rooted at
/// `tree`.
///
/// Returns the tree itself, every reachable subtree, and every blob
/// (regular, executable, or symlink). Submodule entries
/// (`EntryKind::Commit` — gitlink mode 160000) are skipped because
/// their target lives in another repository and is therefore not in
/// this ODB. Order is depth-first stack order — gix-pack does not
/// require any particular ordering for `ObjectExpansion::AsIs`.
///
/// **Deduplication**: each OID is emitted at most once. Real git
/// history dedupes blobs aggressively (two paths with identical
/// content share a blob OID; two parent trees with identical
/// content share a tree OID), so a naive walker would yield the
/// same OID multiple times and produce a malformed pack. The
/// internal `visited` set also breaks cycles defensively, even
/// though content-addressing makes tree cycles impossible in a
/// healthy ODB — a corrupted or adversarial ODB cannot make this
/// loop run forever.
///
/// Used by the pack-build path for tree-tipped refs (annotated tag of
/// tree, bare-tree ref). The resulting `Vec` is fed to
/// `count::objects` with `ObjectExpansion::AsIs` so each OID lands in
/// the pack verbatim — gix-pack's `TreeContents` expansion is
/// documented for commits and tags only and is not relied on for bare
/// trees.
///
/// # Errors
///
/// Returns [`crate::git::GitError`] on any underlying gix failure
/// (object missing, decode error, walk error).
pub(crate) fn enumerate_tree_closure(
    repo: &Repository,
    tree: ObjectId,
) -> Result<Vec<ObjectId>, crate::git::GitError> {
    use gix::objs::tree::EntryKind;

    let mut oids = Vec::new();
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut stack = vec![tree];
    while let Some(current) = stack.pop() {
        // First-pop dedupe: a tree may be pushed onto the stack
        // multiple times by separate parents. Skipping repeats is
        // also the cycle break for adversarial ODBs.
        if !visited.insert(current) {
            continue;
        }
        oids.push(current);
        let object = repo.find_object(current)?.peel_to_kind(Kind::Tree)?;
        for entry in object.into_tree().iter() {
            let entry = entry?;
            match entry.kind() {
                EntryKind::Tree => stack.push(entry.oid().to_owned()),
                EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => {
                    let oid = entry.oid().to_owned();
                    // Blobs are emitted directly (not stack-routed) so
                    // we don't pay an extra heap push for leaves; the
                    // visited-gate keeps shared blobs unique.
                    if visited.insert(oid) {
                        oids.push(oid);
                    }
                }
                EntryKind::Commit => {
                    // Submodule / gitlink. Target lives in another repo;
                    // nothing local to pack. Same contract as `walk_tree`.
                }
            }
        }
    }
    Ok(oids)
}

/// Recursive worker. Inserts an entry into `out` for every blob /
/// symlink at this tree level, and recurses into subtrees.
///
/// `ancestors` is the set of tree OIDs on the current descent path —
/// pushed on entry, popped before return. If `tree_id` is already in
/// the set the descent has hit a cycle and aborts with
/// [`PackchainError::TreeCycle`]. The set is per-descent rather than
/// global because shared subtrees at distinct paths (`src/foo/` and
/// `vendor/foo/` with identical content) are legitimate and must each
/// be walked — only re-entry on the active path is a cycle.
fn walk_tree(
    repo: &Repository,
    tree_id: ObjectId,
    out: &mut BTreeMap<String, PathNode>,
    ancestors: &mut HashSet<ObjectId>,
) -> Result<(), PackchainError> {
    use gix::objs::tree::EntryKind;

    if !ancestors.insert(tree_id) {
        return Err(PackchainError::TreeCycle {
            oid: tree_id.to_string(),
        });
    }
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
                walk_tree(repo, entry.oid().to_owned(), &mut subtree, ancestors)?;
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
    ancestors.remove(&tree_id);
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

    /// Peel a tip OID into a `PeeledTip::Commit`. Used by tests that
    /// build a fixture repo and want to feed `extract_path_index` the
    /// peeled form push would compute.
    fn peeled_commit(repo: &gix::Repository, tip: Sha) -> PeeledTip {
        crate::git::peel_tag_chain(repo, tip).expect("peel commit-tip")
    }

    #[test]
    fn extract_path_index_reflects_nested_layout() {
        let (repo, _guard, tip) = fixture_repo();
        let peeled = peeled_commit(&repo, tip);
        let index = extract_path_index(&repo, &peeled, tip)
            .expect("extract")
            .expect("commit-tip path-index must be present");

        assert_eq!(index.v, PathIndex::SCHEMA_VERSION);
        assert_eq!(index.tip.as_str(), tip.to_string());

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
        // anything `extract_path_index` produces is a valid
        // `path-index.json`.
        let (repo, _guard, tip) = fixture_repo();
        let peeled = peeled_commit(&repo, tip);
        let index = extract_path_index(&repo, &peeled, tip).unwrap().unwrap();
        let bytes = index.to_json_pretty().unwrap();
        let decoded = PathIndex::from_json_bytes(&bytes).unwrap();
        assert_eq!(decoded, index);
    }

    #[test]
    fn extract_path_index_for_tree_tip_walks_tree_directly() {
        // Bare-tree ref or tag-of-tree: walk the leaf tree directly,
        // no commit-peel detour.
        let (repo, _guard, tip) = fixture_repo();
        let root_tree = repo
            .find_object(*tip.as_object_id())
            .unwrap()
            .peel_to_kind(Kind::Commit)
            .unwrap()
            .into_commit()
            .tree_id()
            .unwrap()
            .detach();
        let peeled = PeeledTip::Tree {
            tree: root_tree,
            tag_chain: Vec::new(),
        };
        let unpeeled = Sha::from_object_id(root_tree);
        let index = extract_path_index(&repo, &peeled, unpeeled)
            .unwrap()
            .expect("tree-tip path-index must be present");
        assert_eq!(index.tip.as_str(), unpeeled.to_string());
        assert!(index.tree.contains_key("Cargo.toml"));
        assert!(index.tree.contains_key("src"));
    }

    #[test]
    fn extract_path_index_for_blob_tip_returns_none() {
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();
        let blob = repo.write_blob(b"data").unwrap().detach();
        let peeled = PeeledTip::Blob {
            blob,
            tag_chain: Vec::new(),
        };
        let result = extract_path_index(&repo, &peeled, Sha::from_object_id(blob)).unwrap();
        assert!(result.is_none(), "blob-tipped chains have no tree to index",);
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

        let peeled = peeled_commit(&repo, tip);
        let err =
            extract_path_index(&repo, &peeled, tip).expect_err("non-UTF-8 filename must reject");
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
        let peeled = peeled_commit(&opened, tip);
        let index = extract_path_index(&opened, &peeled, tip)
            .expect("extract on opened")
            .expect("commit-tip path-index must be present");
        assert!(index.tree.contains_key("Cargo.toml"));
        assert!(index.tree.contains_key("src"));
    }

    // --- enumerate_tree_closure ---------------------------------------

    #[test]
    fn enumerate_tree_closure_yields_tree_subtree_and_blob_oids() {
        // Walk every OID inside the fixture's root tree closure. Must
        // include the root tree, the src subtree, the inner subtree,
        // and every blob.
        let (repo, _guard, tip) = fixture_repo();
        let root_tree = repo
            .find_object(*tip.as_object_id())
            .unwrap()
            .peel_to_kind(Kind::Commit)
            .unwrap()
            .into_commit()
            .tree_id()
            .unwrap()
            .detach();
        let oids = enumerate_tree_closure(&repo, root_tree).unwrap();
        // 3 trees (root, src, inner) + 3 blobs (Cargo.toml, main.rs, deep.rs)
        assert_eq!(oids.len(), 6, "fixture has 3 trees + 3 blobs, got {oids:?}",);
        assert!(oids.contains(&root_tree), "root tree must be included");
    }

    #[test]
    fn enumerate_tree_closure_handles_empty_tree() {
        // Empty trees are legal in git (the well-known
        // 4b825dc... tree is empty). Closure is a single-element
        // vector containing just the tree OID.
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();
        let empty_tree = repo
            .write_object(&gix::objs::Tree {
                entries: Vec::new(),
            })
            .unwrap()
            .detach();
        let oids = enumerate_tree_closure(&repo, empty_tree).unwrap();
        assert_eq!(oids, vec![empty_tree]);
    }

    #[test]
    fn enumerate_tree_closure_skips_gitlink_entries() {
        // A tree with a gitlink (submodule) entry. The closure must
        // include the tree itself but NOT the gitlink OID — that
        // commit lives in the submodule's repo, not this ODB.
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();
        // Use an arbitrary 40-hex SHA as the gitlink target. The OID
        // does not need to resolve to anything in this repo — gitlinks
        // are pointers to *another* repository.
        let gitlink_oid = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
        let blob = repo.write_blob(b"x").unwrap().detach();
        let tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![
                    Entry {
                        mode: EntryKind::Commit.into(),
                        filename: "submod".into(),
                        oid: gitlink_oid,
                    },
                    Entry {
                        mode: EntryKind::Blob.into(),
                        filename: "x".into(),
                        oid: blob,
                    },
                ],
            })
            .unwrap()
            .detach();
        let oids = enumerate_tree_closure(&repo, tree).unwrap();
        assert!(oids.contains(&tree), "tree itself must be included");
        assert!(oids.contains(&blob), "blob entry must be included");
        assert!(
            !oids.contains(&gitlink_oid),
            "gitlink OID must be skipped (lives in another repo)",
        );
    }

    #[test]
    fn enumerate_tree_closure_dedupes_shared_blob() {
        // Two tree entries pointing at the same blob OID — common in
        // real git history (identical files at different paths share
        // a blob). The closure must emit the blob exactly once;
        // duplicates would produce a malformed pack downstream.
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();
        let blob = repo.write_blob(b"shared").unwrap().detach();
        let tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![
                    Entry {
                        mode: EntryKind::Blob.into(),
                        filename: "a".into(),
                        oid: blob,
                    },
                    Entry {
                        mode: EntryKind::Blob.into(),
                        filename: "b".into(),
                        oid: blob,
                    },
                ],
            })
            .unwrap()
            .detach();
        let oids = enumerate_tree_closure(&repo, tree).unwrap();
        assert_eq!(
            oids.len(),
            2,
            "shared blob must be emitted exactly once (got {oids:?})",
        );
        assert!(oids.contains(&tree));
        assert!(oids.contains(&blob));
    }

    #[test]
    fn enumerate_tree_closure_dedupes_shared_subtree() {
        // Two tree entries (different filenames) pointing at the same
        // subtree OID. Possible in real history when sibling
        // directories have identical content. Closure must emit the
        // subtree and its blob exactly once each.
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();
        let blob = repo.write_blob(b"leaf").unwrap().detach();
        let subtree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![Entry {
                    mode: EntryKind::Blob.into(),
                    filename: "leaf.txt".into(),
                    oid: blob,
                }],
            })
            .unwrap()
            .detach();
        let root = repo
            .write_object(&gix::objs::Tree {
                entries: vec![
                    Entry {
                        mode: EntryKind::Tree.into(),
                        filename: "left".into(),
                        oid: subtree,
                    },
                    Entry {
                        mode: EntryKind::Tree.into(),
                        filename: "right".into(),
                        oid: subtree,
                    },
                ],
            })
            .unwrap()
            .detach();
        let oids = enumerate_tree_closure(&repo, root).unwrap();
        assert_eq!(
            oids.len(),
            3,
            "root + shared subtree (once) + shared blob (once); got {oids:?}",
        );
        assert!(oids.contains(&root));
        assert!(oids.contains(&subtree));
        assert!(oids.contains(&blob));
    }

    #[test]
    fn enumerate_tree_closure_includes_symlink_blobs() {
        // Symlinks are blobs in git (mode 120000 with target as content).
        // The closure must include them like any other blob.
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();
        let target = repo.write_blob(b"target/path").unwrap().detach();
        let tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![Entry {
                    mode: EntryKind::Link.into(),
                    filename: "alias".into(),
                    oid: target,
                }],
            })
            .unwrap()
            .detach();
        let oids = enumerate_tree_closure(&repo, tree).unwrap();
        assert!(oids.contains(&tree));
        assert!(
            oids.contains(&target),
            "symlink target blob must be included"
        );
    }

    // --- walk_tree cycle / shared-subtree hardening (issue #81) -------

    /// Write a corrupted loose tree object directly under
    /// `.git/objects/`. The filename's hash is `oid`, the contents are
    /// the supplied tree entries — the two need not agree, mirroring an
    /// adversarial or corrupted ODB. gix's loose-object reader does not
    /// verify hash-vs-content on read.
    ///
    /// Returns when the file has been zlib-compressed and persisted.
    fn write_corrupt_loose_tree(
        repo_path: &std::path::Path,
        oid: ObjectId,
        entries: &[(EntryKind, &str, ObjectId)],
    ) {
        use flate2::Compression;
        use flate2::write::ZlibEncoder;
        use std::io::Write as _;

        // Build the raw tree body: each entry is
        //   `<octal-mode> <name>\0<20-byte-binary-oid>`
        // concatenated with no separator.
        let mut body: Vec<u8> = Vec::new();
        for (kind, name, entry_oid) in entries {
            let mode: u32 = match kind {
                EntryKind::Tree => 0o040_000,
                EntryKind::Blob => 0o100_644,
                EntryKind::BlobExecutable => 0o100_755,
                EntryKind::Link => 0o120_000,
                EntryKind::Commit => 0o160_000,
            };
            body.extend_from_slice(format!("{mode:o}").as_bytes());
            body.push(b' ');
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(entry_oid.as_slice());
        }

        // Loose-object format is `tree <decimal-len>\0<body>` then zlib-compressed.
        let mut full: Vec<u8> = Vec::new();
        full.extend_from_slice(format!("tree {}", body.len()).as_bytes());
        full.push(0);
        full.extend_from_slice(&body);

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&full).unwrap();
        let compressed = encoder.finish().unwrap();

        let hex = oid.to_string();
        let dir = repo_path.join(".git/objects").join(&hex[..2]);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(&hex[2..]), compressed).unwrap();
    }

    #[test]
    fn extract_path_index_detects_direct_self_cycle() {
        // A tree T whose only entry references T itself. Impossible in
        // a healthy ODB (content-addressing), but a corrupted loose
        // object can carry it. The walker must abort with `TreeCycle`
        // and surface the offending OID rather than recurse forever.
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();

        // Pick an arbitrary 40-hex OID for the cyclic tree. Its hash
        // need not match its content — the loose-object reader resolves
        // by filename.
        let cyclic = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
        write_corrupt_loose_tree(tmp.path(), cyclic, &[(EntryKind::Tree, "self", cyclic)]);

        let peeled = PeeledTip::Tree {
            tree: cyclic,
            tag_chain: Vec::new(),
        };
        let unpeeled = Sha::from_object_id(cyclic);
        let err = extract_path_index(&repo, &peeled, unpeeled)
            .expect_err("self-referential tree must be rejected as a cycle");
        match err {
            PackchainError::TreeCycle { oid } => {
                assert_eq!(oid, cyclic.to_string());
            }
            other => panic!("expected TreeCycle, got {other:?}"),
        }
    }

    #[test]
    fn extract_path_index_detects_indirect_cycle() {
        // T1 → T2 → T1. Both trees are corrupted loose objects whose
        // referenced OID is the other tree's OID. The walker's ancestor
        // set must catch this on the second descent into T1.
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();

        let t1 = ObjectId::from_hex(b"2222222222222222222222222222222222222222").unwrap();
        let t2 = ObjectId::from_hex(b"3333333333333333333333333333333333333333").unwrap();
        write_corrupt_loose_tree(tmp.path(), t1, &[(EntryKind::Tree, "down", t2)]);
        write_corrupt_loose_tree(tmp.path(), t2, &[(EntryKind::Tree, "back", t1)]);

        let peeled = PeeledTip::Tree {
            tree: t1,
            tag_chain: Vec::new(),
        };
        let unpeeled = Sha::from_object_id(t1);
        let err = extract_path_index(&repo, &peeled, unpeeled)
            .expect_err("indirect tree cycle must be rejected");
        match err {
            PackchainError::TreeCycle { oid } => {
                // The second visit hits T1 again — that's the OID we
                // re-saw in the ancestor set.
                assert_eq!(oid, t1.to_string());
            }
            other => panic!("expected TreeCycle, got {other:?}"),
        }
    }

    #[test]
    fn extract_path_index_walks_shared_subtree_at_distinct_paths() {
        // Regression guard: a flat visited-set would have over-pruned
        // here. The same subtree OID referenced at two distinct paths
        // is NOT a cycle — the walker must descend at both paths and
        // emit identical sub-maps.
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();

        let leaf = repo.write_blob(b"hello").unwrap().detach();
        let shared = repo
            .write_object(&gix::objs::Tree {
                entries: vec![Entry {
                    mode: EntryKind::Blob.into(),
                    filename: "leaf.txt".into(),
                    oid: leaf,
                }],
            })
            .unwrap()
            .detach();
        let root = repo
            .write_object(&gix::objs::Tree {
                entries: vec![
                    Entry {
                        mode: EntryKind::Tree.into(),
                        filename: "src".into(),
                        oid: shared,
                    },
                    Entry {
                        mode: EntryKind::Tree.into(),
                        filename: "vendor".into(),
                        oid: shared,
                    },
                ],
            })
            .unwrap()
            .detach();

        let peeled = PeeledTip::Tree {
            tree: root,
            tag_chain: Vec::new(),
        };
        let unpeeled = Sha::from_object_id(root);
        let index = extract_path_index(&repo, &peeled, unpeeled)
            .expect("shared-subtree walk must succeed")
            .expect("tree-tip path-index must be present");

        // Both paths must be present and must each carry the leaf blob.
        let src = index.tree.get("src").expect("src present");
        let vendor = index.tree.get("vendor").expect("vendor present");
        let PathNode::Tree(src_children) = src else {
            panic!("src must be a Tree, got {src:?}");
        };
        let PathNode::Tree(vendor_children) = vendor else {
            panic!("vendor must be a Tree, got {vendor:?}");
        };
        assert_eq!(
            src_children, vendor_children,
            "shared subtree must yield identical child maps at both paths",
        );
        assert!(matches!(
            src_children.get("leaf.txt"),
            Some(PathNode::Blob(_)),
        ));
    }
}

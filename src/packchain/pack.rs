//! Pack-file emission for the packchain engine.
//!
//! Two entry points:
//!
//! - [`build_baseline_pack`] — full snapshot via
//!   [`ObjectExpansion::TreeContents`]. Used on the first push of a
//!   branch and on every force push.
//! - [`build_incremental_pack`] — incremental (ancestor-aware) pack
//!   via [`ObjectExpansion::TreeAdditionsComparedToAncestor`] over
//!   commits reachable from `local_tip` but not from `prior_tip`
//!   (gix's `rev_walk(local_tip).with_hidden(prior_tip)`). gix-pack's
//!   `TreeAdditionsComparedToAncestor` includes the ancestor commit
//!   and its tree alongside the new commit's tree and the
//!   added/changed blobs — so the resulting pack is **self-contained
//!   for delta resolution** (deltas reference objects within the same
//!   pack). The space saving versus a full snapshot comes from
//!   omitting blobs that were only reachable from the ancestor's tree
//!   (the "ancestor-only blobs"); Phase 3 fetch installs prior chain
//!   packs to make those reachable.
//!
//! Both functions are synchronous and pin a `gix::Repository` for
//! their entire body; callers wrap in [`tokio::task::spawn_blocking`]
//! to keep the surrounding async future `Send`. Outputs are persisted
//! to disk before return — async upload code can stream them via
//! [`crate::object_store::ObjectStore::put_path`] without re-buffering
//! into memory.
//!
//! [`ObjectExpansion::TreeContents`]: gix_pack::data::output::count::objects::ObjectExpansion::TreeContents
//! [`ObjectExpansion::TreeAdditionsComparedToAncestor`]: gix_pack::data::output::count::objects::ObjectExpansion::TreeAdditionsComparedToAncestor

use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use gix_hash::ObjectId;
// `Find` is needed in scope for the `odb.contains(...)` method calls in
// the test module — the trait's methods are only callable when the trait
// is imported.
#[cfg_attr(not(test), allow(unused_imports))]
use gix_pack::Find;
use gix_pack::data::output::bytes::FromEntriesIter;
use gix_pack::data::output::count::objects::ObjectExpansion;
use gix_pack::data::output::{count, entry};
use tempfile::{NamedTempFile, TempDir};

use crate::git::{PeeledTip, Sha};

use super::PackchainError;
use super::schema::Sha40;

/// PACK trailer is the SHA-1 of the pack content (last 20 bytes of
/// the file). The minimum legal pack file is 12-byte header + 20-byte
/// trailer = 32 bytes (a pack with zero entries).
const PACK_TRAILER_LEN: usize = 20;
const PACK_HEADER_LEN: u64 = 12;
const PACK_MIN_LEN: u64 = PACK_HEADER_LEN + PACK_TRAILER_LEN as u64;

/// Output of a successful pack build.
pub(crate) struct BuiltPack {
    /// Path to the persisted `.pack` file (named `<content_sha>.pack`).
    pub(crate) pack_path: PathBuf,
    /// Path to the matching `.idx` file (named `<content_sha>.idx`).
    pub(crate) idx_path: PathBuf,
    /// SHA-1 of the pack content (the trailer; equal to git's pack
    /// content hash). Used as the bucket key under `packs/`.
    pub(crate) content_sha: Sha40,
    /// Pack file size in bytes — recorded in the chain-segment manifest
    /// so Phase 5 GC's "rewrite when segments-since-full > N" heuristic
    /// can sum chain weight without an extra HEAD round trip.
    pub(crate) pack_bytes: u64,
}

/// Build a baseline pack for `peeled`, plus its tag chain.
///
/// Three branches by leaf kind:
///
/// - **Commit**: walk every commit reachable from the leaf commit and
///   expand each via [`ObjectExpansion::TreeContents`] (commits + trees +
///   blobs). The pre-#80 path; emitted pack is byte-identical for
///   commit-tipped refs.
/// - **Tree**: enumerate the leaf tree's full closure (subtrees + blobs,
///   gitlinks skipped) via
///   [`super::git::enumerate_tree_closure`] and feed it with
///   [`ObjectExpansion::AsIs`]. We do not rely on `TreeContents`
///   accepting a bare-tree input — that expansion is documented for
///   commits and tags only.
/// - **Blob**: pack the single leaf blob with
///   [`ObjectExpansion::AsIs`]. No tree to walk.
///
/// In every branch the tag chain is appended verbatim so a fetch-back
/// of `refs/tags/v1` resolves the ref to the unpeeled tag OID.
///
/// `out_dir` must already exist; the resulting `.pack` and `.idx`
/// land directly in it. Callers are responsible for `out_dir`'s
/// lifetime (typically a `tempfile::TempDir` rooted in
/// `prepare_push`'s scratch area).
pub(crate) fn build_baseline_pack(
    repo_dir: &Path,
    peeled: PeeledTip,
    out_dir: &Path,
) -> Result<BuiltPack, PackchainError> {
    let repo = gix::open(repo_dir).map_err(crate::git::GitError::from)?;
    match peeled {
        PeeledTip::Commit { commit, tag_chain } => {
            let commit_ids = collect_commits_baseline(&repo, *commit.as_object_id())?;
            build_pack(
                &repo,
                commit_ids,
                ObjectExpansion::TreeContents,
                &tag_chain,
                out_dir,
            )
        }
        PeeledTip::Tree { tree, tag_chain } => {
            let oids =
                super::git::enumerate_tree_closure(&repo, tree).map_err(PackchainError::Git)?;
            build_pack(&repo, oids, ObjectExpansion::AsIs, &tag_chain, out_dir)
        }
        PeeledTip::Blob { blob, tag_chain } => build_pack(
            &repo,
            vec![blob],
            ObjectExpansion::AsIs,
            &tag_chain,
            out_dir,
        ),
    }
}

/// Build an incremental (ancestor-aware) pack: every commit
/// reachable from `local_tip` but not from `prior_tip`, plus the
/// ancestor commit and its tree, plus blobs added/changed at
/// `local_tip` versus the ancestor tree. Ancestor-only blobs are
/// omitted; the receiver picks them up by installing prior chain
/// packs in order.
///
/// A pack with zero entries is impossible because `prepare_push`
/// short-circuits on `prior_tip == local_tip`, but [`build_pack`]
/// validates `counts.is_empty()` defensively in case a future caller
/// drops the short-circuit.
pub(crate) fn build_incremental_pack(
    repo_dir: &Path,
    prior_commit: Sha,
    local_commit: Sha,
    tag_chain: &[ObjectId],
    out_dir: &Path,
) -> Result<BuiltPack, PackchainError> {
    let repo = gix::open(repo_dir).map_err(crate::git::GitError::from)?;
    let commit_ids = collect_commits_incremental(
        &repo,
        *local_commit.as_object_id(),
        *prior_commit.as_object_id(),
    )?;
    build_pack(
        &repo,
        commit_ids,
        ObjectExpansion::TreeAdditionsComparedToAncestor,
        tag_chain,
        out_dir,
    )
}

/// Walk every commit reachable from `tip`. Used for the baseline pack.
fn collect_commits_baseline(
    repo: &gix::Repository,
    tip: ObjectId,
) -> Result<Vec<ObjectId>, PackchainError> {
    repo.rev_walk([tip])
        .all()
        .map_err(|e| PackchainError::PackBuild(e.to_string()))?
        .map(|info| info.map(|i| i.id))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| PackchainError::PackBuild(e.to_string()))
}

/// Walk commits reachable from `local_tip` but not from `prior_tip`.
fn collect_commits_incremental(
    repo: &gix::Repository,
    local_tip: ObjectId,
    prior_tip: ObjectId,
) -> Result<Vec<ObjectId>, PackchainError> {
    repo.rev_walk([local_tip])
        .with_hidden([prior_tip])
        .all()
        .map_err(|e| PackchainError::PackBuild(e.to_string()))?
        .map(|info| info.map(|i| i.id))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| PackchainError::PackBuild(e.to_string()))
}

/// Shared pack-emission body. Writes a raw PACK (no bundle v2 header)
/// to a temp file, derives the content SHA from the trailer, persists
/// the pack at `<out_dir>/<sha>.pack`, and produces the matching `.idx`
/// at `<out_dir>/<sha>.idx` via a second pass through
/// [`gix_pack::Bundle::write_to_directory`].
///
/// `input_oids` is the seed set for the count phase: commit OIDs for a
/// commit-walk (with `expansion = TreeContents` /
/// `TreeAdditionsComparedToAncestor`), or pre-enumerated raw OIDs for a
/// tree/blob-tipped pack (with `expansion = AsIs`).
fn build_pack(
    repo: &gix::Repository,
    input_oids: Vec<ObjectId>,
    expansion: ObjectExpansion,
    tag_chain: &[ObjectId],
    out_dir: &Path,
) -> Result<BuiltPack, PackchainError> {
    // Strip the Proxy wrapper to expose the gix_pack::Find impl needed
    // by the output pipeline (parity with src/bundle.rs:186-189).
    let mut odb = repo.objects.clone().into_inner();
    odb.prevent_pack_unload();

    let (mut counts, _stats) = count::objects(
        odb.clone(),
        Box::new(
            input_oids
                .into_iter()
                .map(Ok::<_, Box<dyn std::error::Error + Send + Sync + 'static>>),
        ),
        &gix::progress::Discard,
        &AtomicBool::new(false),
        count::objects::Options {
            input_object_expansion: expansion,
            thread_limit: Some(1),
            ..Default::default()
        },
    )
    .map_err(|e| PackchainError::PackBuild(e.to_string()))?;

    // Append annotated-tag objects (and any chain of tag-of-tag objects)
    // verbatim. Without this, a fetch-back of `refs/tags/v1` would fail
    // because the tag-object OID the receiver must point the ref at is
    // not in the ODB. `count_objects_as_is` is shared with `bundle.rs`
    // so both engines stay in lockstep.
    counts.extend(
        crate::bundle::count_objects_as_is(odb.clone(), tag_chain)
            .map_err(|e| PackchainError::PackBuild(e.to_string()))?,
    );

    if counts.is_empty() {
        // A no-op pack would be 32 bytes (header + trailer) — legal,
        // but the caller's idempotency short-circuit should have
        // caught this. Refuse loudly so a bug there can't leak a
        // zero-entry pack into the chain.
        return Err(PackchainError::PackBuild(
            "no objects to pack — caller should have short-circuited on prior.tip == local_tip"
                .into(),
        ));
    }
    let num_entries = u32::try_from(counts.len())
        .map_err(|_| PackchainError::PackBuild("too many objects for a single pack".into()))?;

    let entries_iter = entry::iter_from_counts(
        counts,
        odb,
        Box::new(gix::progress::Discard),
        entry::iter_from_counts::Options {
            thread_limit: Some(1),
            ..Default::default()
        },
    )
    // Strip SequenceId: FromEntriesIter expects Iterator<Item=Result<Vec<Entry>,_>>.
    .map(|r| r.map(|(_, entries)| entries));

    // Stage the pack in a NamedTempFile inside `out_dir` so a crash
    // mid-write leaves no half-pack at the final path.
    let mut pack_tmp = NamedTempFile::new_in(out_dir)?;
    {
        let pack_iter = FromEntriesIter::new(
            entries_iter,
            pack_tmp.as_file_mut(),
            num_entries,
            gix_pack::data::Version::V2,
            gix_hash::Kind::Sha1,
        );
        for r in pack_iter {
            r.map_err(|e| PackchainError::PackBuild(e.to_string()))?;
        }
    }
    pack_tmp.as_file().sync_all()?;

    // PACK trailer = last 20 bytes of file = SHA-1 of pack content.
    let pack_bytes = pack_tmp.as_file().metadata()?.len();
    if pack_bytes < PACK_MIN_LEN {
        return Err(PackchainError::PackTrailer(format!(
            "pack file shorter than {PACK_MIN_LEN} bytes (got {pack_bytes})",
        )));
    }
    let mut trailer = [0u8; PACK_TRAILER_LEN];
    pack_tmp
        .as_file_mut()
        .seek(SeekFrom::Start(pack_bytes - PACK_TRAILER_LEN as u64))?;
    pack_tmp.as_file_mut().read_exact(&mut trailer)?;
    let trailer_oid = ObjectId::from(trailer);
    let content_sha = Sha40::from_oid(&trailer_oid)?;

    // Persist the pack at <out_dir>/<sha>.pack. `NamedTempFile::persist`
    // is atomic (rename(2)).
    let pack_path = out_dir.join(format!("{}.pack", content_sha.as_str()));
    pack_tmp
        .persist(&pack_path)
        .map_err(|e| PackchainError::Io(e.error))?;

    // Derive .idx by streaming the persisted pack through
    // gix_pack::Bundle::write_to_directory into a separate temp dir so
    // gix's preferred naming doesn't collide with our `<sha>.pack`.
    // Then move the derived .idx alongside the pack.
    let idx_path = derive_idx(&pack_path, out_dir, &content_sha)?;

    Ok(BuiltPack {
        pack_path,
        idx_path,
        content_sha,
        pack_bytes,
    })
}

/// Generate `<out_dir>/<content_sha>.idx` by re-reading `pack_path`
/// through [`gix_pack::Bundle::write_to_directory`] (which writes
/// gix-named pack/idx into a scratch directory) and moving its idx
/// into place.
fn derive_idx(
    pack_path: &Path,
    out_dir: &Path,
    content_sha: &Sha40,
) -> Result<PathBuf, PackchainError> {
    // gix's write_to_directory drops a copy of the pack alongside the
    // idx in the target directory — point it at a scratch dir we
    // discard so we don't end up with two pack files in `out_dir`.
    let scratch = TempDir::new_in(out_dir)?;

    let pack_file = File::open(pack_path)?;
    let mut pack_reader = BufReader::new(pack_file);

    let outcome = gix_pack::Bundle::write_to_directory(
        &mut pack_reader,
        Some(scratch.path()),
        &mut gix::progress::Discard,
        &AtomicBool::new(false),
        None::<gix::odb::Handle>,
        gix_pack::bundle::write::Options {
            object_hash: gix_hash::Kind::Sha1,
            ..Default::default()
        },
    )?;

    let derived_idx = outcome
        .index_path
        .ok_or_else(|| PackchainError::PackBuild("gix_pack did not emit an .idx path".into()))?;

    // Move the derived idx alongside the pack at our preferred name.
    let idx_path = out_dir.join(format!("{}.idx", content_sha.as_str()));
    fs::rename(&derived_idx, &idx_path)?;

    // gix writes a .keep alongside the unbundled pack so post-bundle
    // git-gc can't reap it before refs are updated. We're not bundling
    // for unbundle here — the pack lives on the bucket — so the .keep
    // is unnecessary; remove if present (mirror src/bundle.rs:332).
    if let Some(keep_path) = outcome.keep_path {
        let _ = fs::remove_file(keep_path);
    }
    // The scratch tempdir's pack copy is dropped together with `scratch`.
    Ok(idx_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    use gix::actor::SignatureRef;
    use gix::bstr::BStr;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    fn signature() -> SignatureRef<'static> {
        SignatureRef {
            name: BStr::new("Tester"),
            email: BStr::new("t@example.com"),
            time: "0 +0000",
        }
    }

    /// Build the `PeeledTip::Commit` form `build_baseline_pack` now
    /// expects. Tests in this module pre-#80 passed `(commit, tag_chain)`
    /// directly — the helper keeps the change-set localised.
    fn commit_tip(commit: Sha, tag_chain: Vec<ObjectId>) -> PeeledTip {
        PeeledTip::Commit { commit, tag_chain }
    }

    /// Build a fixture repo with two commits on `refs/heads/main`.
    /// Returns `(tempdir keeping the repo alive, c1, c2)`.
    fn fixture_two_commits() -> (TempDir, Sha, Sha) {
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();

        let blob1 = repo.write_blob(b"v1").unwrap().detach();
        let tree1 = repo
            .write_object(&gix::objs::Tree {
                entries: vec![gix::objs::tree::Entry {
                    mode: gix::objs::tree::EntryKind::Blob.into(),
                    filename: "a.txt".into(),
                    oid: blob1,
                }],
            })
            .unwrap()
            .detach();
        let c1 = repo
            .commit_as(
                signature(),
                signature(),
                "refs/heads/main",
                "first",
                tree1,
                std::iter::empty::<ObjectId>(),
            )
            .unwrap()
            .detach();

        let blob2 = repo.write_blob(b"v2 plus more").unwrap().detach();
        let blob_b = repo.write_blob(b"new file b").unwrap().detach();
        let tree2 = repo
            .write_object(&gix::objs::Tree {
                entries: vec![
                    gix::objs::tree::Entry {
                        mode: gix::objs::tree::EntryKind::Blob.into(),
                        filename: "a.txt".into(),
                        oid: blob2,
                    },
                    gix::objs::tree::Entry {
                        mode: gix::objs::tree::EntryKind::Blob.into(),
                        filename: "b.txt".into(),
                        oid: blob_b,
                    },
                ],
            })
            .unwrap()
            .detach();
        let c2 = repo
            .commit_as(
                signature(),
                signature(),
                "refs/heads/main",
                "second",
                tree2,
                std::iter::once(c1),
            )
            .unwrap()
            .detach();

        (tmp, Sha::from_object_id(c1), Sha::from_object_id(c2))
    }

    /// Single-commit fixture for the smallest baseline-pack case.
    fn fixture_single_commit() -> (TempDir, Sha) {
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();
        let blob = repo.write_blob(b"only").unwrap().detach();
        let tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![gix::objs::tree::Entry {
                    mode: gix::objs::tree::EntryKind::Blob.into(),
                    filename: "x".into(),
                    oid: blob,
                }],
            })
            .unwrap()
            .detach();
        let c = repo
            .commit_as(
                signature(),
                signature(),
                "refs/heads/main",
                "only",
                tree,
                std::iter::empty::<ObjectId>(),
            )
            .unwrap()
            .detach();
        (tmp, Sha::from_object_id(c))
    }

    #[test]
    fn build_baseline_pack_handles_single_commit_repo() {
        let (repo_dir, tip) = fixture_single_commit();
        let out = TempDir::new().unwrap();
        let built = build_baseline_pack(repo_dir.path(), commit_tip(tip, vec![]), out.path())
            .expect("build");
        assert!(built.pack_path.exists());
        assert!(built.idx_path.exists());
        assert!(built.pack_bytes >= PACK_MIN_LEN);
        assert_eq!(built.content_sha.as_str().len(), 40);
    }

    #[test]
    fn build_baseline_pack_content_sha_matches_trailer() {
        // Content SHA returned in BuiltPack must equal the last 20
        // bytes of the persisted pack file. Pinning this catches any
        // future refactor that confuses the pack-content hash with
        // (e.g.) the pack's name as gix would have written it.
        let (repo_dir, tip) = fixture_single_commit();
        let out = TempDir::new().unwrap();
        let built = build_baseline_pack(repo_dir.path(), commit_tip(tip, vec![]), out.path())
            .expect("build");
        let pack_bytes = std::fs::read(&built.pack_path).unwrap();
        assert!(pack_bytes.len() >= PACK_TRAILER_LEN);
        let trailer_start = pack_bytes.len() - PACK_TRAILER_LEN;
        let trailer = &pack_bytes[trailer_start..];
        let oid = ObjectId::try_from(trailer).unwrap();
        assert_eq!(built.content_sha.as_str(), oid.to_string());
    }

    #[test]
    fn build_baseline_pack_round_trips_via_bundle_write_to_directory() {
        // Pack everything reachable from the tip, then unbundle into
        // a fresh repo via gix_pack::Bundle::write_to_directory and
        // assert every commit / tree / blob is present.
        let (repo_dir, c1, c2) = fixture_two_commits();
        let out = TempDir::new().unwrap();
        let built = build_baseline_pack(repo_dir.path(), commit_tip(c2, vec![]), out.path())
            .expect("build");

        let dst = TempDir::new().unwrap();
        let dst_repo = gix::init(dst.path()).unwrap();
        let pack_dir = dst_repo.git_dir().join("objects/pack");
        std::fs::create_dir_all(&pack_dir).unwrap();

        let pack_file = File::open(&built.pack_path).unwrap();
        let mut reader = BufReader::new(pack_file);
        gix_pack::Bundle::write_to_directory(
            &mut reader,
            Some(&pack_dir),
            &mut gix::progress::Discard,
            &AtomicBool::new(false),
            None::<gix::odb::Handle>,
            gix_pack::bundle::write::Options {
                object_hash: gix_hash::Kind::Sha1,
                ..Default::default()
            },
        )
        .expect("install pack");

        let odb = dst_repo.objects.clone().into_inner();
        assert!(odb.contains(c1.as_object_id()), "c1 must be reachable");
        assert!(odb.contains(c2.as_object_id()), "c2 must be reachable");
    }

    #[test]
    fn build_incremental_pack_only_emits_new_objects() {
        // Verify by parsing the .idx files: pack byte size is
        // unreliable on micro-fixtures (compression swings it) but
        // object count is stable.
        //
        // Object accounting for the fixture, with rev-walk hiding c1
        // so the input set is `[c2]`:
        //   baseline at c1 (TreeContents):
        //       commit1 + tree1 + blob1                                = 3
        //   incremental c1→c2 (TreeAdditionsComparedToAncestor on [c2]):
        //       commit1 + commit2 + tree1 + tree2 + blob2 + blob_b    = 6
        //       (gix includes the ancestor commit + tree alongside the
        //        added/changed blobs; blob1 is omitted — it lives only
        //        in the baseline.)
        //   incremental c1→c2 (TreeContents on [c2]):
        //       commit2 + tree2 + blob2 + blob_b                       = 4
        //       (no ancestor; receiver must hold c1 + tree1 separately.)
        //
        // Mutation-tested: switching the expansion to `TreeContents`
        // makes incremental's idx report 4 objects, which the strict
        // equality below catches.
        let (repo_dir, c1, c2) = fixture_two_commits();

        let out_baseline = TempDir::new().unwrap();
        let baseline =
            build_baseline_pack(repo_dir.path(), commit_tip(c1, vec![]), out_baseline.path())
                .expect("baseline");

        let out_incr = TempDir::new().unwrap();
        let incr = build_incremental_pack(repo_dir.path(), c1, c2, &[], out_incr.path())
            .expect("incremental");

        let baseline_idx =
            gix_pack::index::File::at(&baseline.idx_path, gix_hash::Kind::Sha1).unwrap();
        let incr_idx = gix_pack::index::File::at(&incr.idx_path, gix_hash::Kind::Sha1).unwrap();

        assert_eq!(
            baseline_idx.num_objects(),
            3,
            "baseline at c1: commit + tree + blob",
        );
        assert_eq!(
            incr_idx.num_objects(),
            6,
            "incremental c1→c2 must omit c1's blob (= 6 objects, not the 7 a TreeContents pack of c2 would produce)",
        );
    }

    #[test]
    fn baseline_plus_incremental_install_reaches_full_history() {
        // Install baseline then incremental (oldest-first, the order
        // the fetch path uses) and confirm every object reachable
        // from c2 lands in the destination ODB — including c1's blob,
        // which lives only in the baseline pack. The previous version
        // of this test only checked `c2.is_reachable`; it would have
        // passed even if the baseline pack contained zero blobs (the
        // commit + tree references would still resolve to the
        // incremental's copies). Mutation-checking the baseline blob
        // closes that gap.
        let (repo_dir, c1, c2) = fixture_two_commits();

        let out_baseline = TempDir::new().unwrap();
        let baseline =
            build_baseline_pack(repo_dir.path(), commit_tip(c1, vec![]), out_baseline.path())
                .expect("baseline");
        let out_incr = TempDir::new().unwrap();
        let incr = build_incremental_pack(repo_dir.path(), c1, c2, &[], out_incr.path())
            .expect("incremental");

        // Walk the source repo to find blob1 (the c1-only blob).
        let src = gix::open(repo_dir.path()).unwrap();
        let c1_tree_id = src
            .find_object(*c1.as_object_id())
            .unwrap()
            .peel_to_kind(gix::object::Kind::Commit)
            .unwrap()
            .into_commit()
            .tree_id()
            .unwrap()
            .detach();
        let blob1 = {
            let tree = src
                .find_object(c1_tree_id)
                .unwrap()
                .peel_to_kind(gix::object::Kind::Tree)
                .unwrap()
                .into_tree();
            // Fixture has exactly one entry at c1 (a.txt → blob1).
            let entry = tree.iter().next().unwrap().unwrap();
            entry.oid().to_owned()
        };

        let dst = TempDir::new().unwrap();
        let dst_repo = gix::init(dst.path()).unwrap();
        let pack_dir = dst_repo.git_dir().join("objects/pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        for pack_path in [&baseline.pack_path, &incr.pack_path] {
            let pf = File::open(pack_path).unwrap();
            let mut r = BufReader::new(pf);
            gix_pack::Bundle::write_to_directory(
                &mut r,
                Some(&pack_dir),
                &mut gix::progress::Discard,
                &AtomicBool::new(false),
                Some(dst_repo.objects.clone().into_inner()),
                gix_pack::bundle::write::Options {
                    object_hash: gix_hash::Kind::Sha1,
                    ..Default::default()
                },
            )
            .unwrap_or_else(|e| panic!("install pack {}: {e}", pack_path.display()));
        }

        let odb = dst_repo.objects.clone().into_inner();
        assert!(odb.contains(c1.as_object_id()), "c1 must be reachable");
        assert!(odb.contains(c2.as_object_id()), "c2 must be reachable");
        assert!(
            odb.contains(&blob1),
            "blob1 (c1-only) must be reachable after installing baseline + incremental",
        );
    }

    // --- tag-chain inclusion ------------------------------------------

    fn write_annotated_tag(
        repo: &gix::Repository,
        target: ObjectId,
        target_kind: gix::object::Kind,
        name: &str,
    ) -> ObjectId {
        let tag = gix::objs::Tag {
            target,
            target_kind,
            name: name.into(),
            tagger: Some(signature().to_owned().expect("static signature is valid")),
            message: "release".into(),
            pgp_signature: None,
        };
        repo.write_object(&tag).unwrap().detach()
    }

    /// Install a pack into a fresh ODB and return the destination repo.
    fn install_into_fresh_repo(pack_path: &Path) -> (TempDir, gix::Repository) {
        let dst = TempDir::new().unwrap();
        let dst_repo = gix::init(dst.path()).unwrap();
        let pack_dir = dst_repo.git_dir().join("objects/pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        let pf = File::open(pack_path).unwrap();
        let mut r = BufReader::new(pf);
        gix_pack::Bundle::write_to_directory(
            &mut r,
            Some(&pack_dir),
            &mut gix::progress::Discard,
            &AtomicBool::new(false),
            None::<gix::odb::Handle>,
            gix_pack::bundle::write::Options {
                object_hash: gix_hash::Kind::Sha1,
                ..Default::default()
            },
        )
        .unwrap();
        (dst, dst_repo)
    }

    #[test]
    fn build_baseline_pack_includes_tag_object_when_tag_chain_nonempty() {
        // E3: annotated tag → commit. The pack must contain the tag
        // object so a fetch-back can update `refs/tags/v1` to the tag
        // OID and resolve `v1^{}`.
        let (repo_dir, _c1, c2) = fixture_two_commits();
        let repo = gix::open(repo_dir.path()).unwrap();
        let tag_oid =
            write_annotated_tag(&repo, *c2.as_object_id(), gix::object::Kind::Commit, "v1");
        drop(repo);

        let out = TempDir::new().unwrap();
        let built = build_baseline_pack(repo_dir.path(), commit_tip(c2, vec![tag_oid]), out.path())
            .expect("build");

        let (_dst_dir, dst_repo) = install_into_fresh_repo(&built.pack_path);
        let odb = dst_repo.objects.clone().into_inner();
        assert!(odb.contains(&tag_oid), "tag object must be in pack");
        assert!(
            odb.contains(c2.as_object_id()),
            "tag's commit target must also be in pack",
        );
        // Decode the tag and pin its target to catch a corruption
        // regression where `tag_oid` is in the pack but the bytes are
        // wrong (would resolve to a different target).
        let tag_obj = dst_repo
            .find_object(tag_oid)
            .expect("find tag")
            .peel_to_kind(gix::object::Kind::Tag)
            .expect("peel to tag");
        let target = tag_obj.into_tag().target_id().expect("decode tag");
        assert_eq!(
            target.detach(),
            *c2.as_object_id(),
            "tag must point at the commit",
        );
    }

    #[test]
    fn build_baseline_pack_includes_full_chain_for_tag_of_tag() {
        // E4: outer → inner → commit. Both tag objects must land in the
        // pack so a receiver can resolve both refs.
        let (repo_dir, _c1, c2) = fixture_two_commits();
        let repo = gix::open(repo_dir.path()).unwrap();
        let inner = write_annotated_tag(
            &repo,
            *c2.as_object_id(),
            gix::object::Kind::Commit,
            "inner",
        );
        let outer = write_annotated_tag(&repo, inner, gix::object::Kind::Tag, "outer");
        drop(repo);

        let out = TempDir::new().unwrap();
        let built = build_baseline_pack(
            repo_dir.path(),
            commit_tip(c2, vec![outer, inner]),
            out.path(),
        )
        .expect("build");

        let (_dst_dir, dst_repo) = install_into_fresh_repo(&built.pack_path);
        let odb = dst_repo.objects.clone().into_inner();
        assert!(odb.contains(&outer), "outer tag must be in pack");
        assert!(odb.contains(&inner), "inner tag must be in pack");
        assert!(
            odb.contains(c2.as_object_id()),
            "commit target must also be in pack",
        );
    }

    #[test]
    fn build_baseline_pack_with_empty_tag_chain_emits_same_object_count_as_today() {
        // E1, E2: regression guard. The AsIs second-pass MUST be gated
        // on `tag_chain.is_empty()` — if it always ran, an empty chain
        // would still produce a (zero-entry) extra count, which would
        // fail the `counts.is_empty()` short-circuit a different way
        // OR (worse) silently inflate `num_entries`. Pin the count to
        // the pre-fix expected value.
        let (repo_dir, _c1, c2) = fixture_two_commits();
        let out = TempDir::new().unwrap();
        let built = build_baseline_pack(repo_dir.path(), commit_tip(c2, vec![]), out.path())
            .expect("build");
        let idx = gix_pack::index::File::at(&built.idx_path, gix_hash::Kind::Sha1).unwrap();
        // 2 commits + 2 trees + 3 blobs = 7 (a-v1, a-v2, b)
        assert_eq!(
            idx.num_objects(),
            7,
            "branch-tip baseline must emit 7 objects (no tag chain to add)",
        );
    }

    #[test]
    fn build_incremental_pack_includes_new_tag_chain() {
        // E8 (partial — incremental shape with a tag): the new tag
        // moves to a descendant commit. The incremental pack carries
        // the tag-of-the-new-tip alongside the commit-walk additions.
        let (repo_dir, c1, c2) = fixture_two_commits();
        let repo = gix::open(repo_dir.path()).unwrap();
        let new_tag =
            write_annotated_tag(&repo, *c2.as_object_id(), gix::object::Kind::Commit, "v2");
        drop(repo);

        let out = TempDir::new().unwrap();
        let built = build_incremental_pack(repo_dir.path(), c1, c2, &[new_tag], out.path())
            .expect("incremental");

        let (_dst_dir, dst_repo) = install_into_fresh_repo(&built.pack_path);
        let odb = dst_repo.objects.clone().into_inner();
        assert!(
            odb.contains(&new_tag),
            "new tag must be in incremental pack"
        );
    }

    // --- non-commit tip pack-build (issue #80) ------------------------

    /// Resolve `c2`'s root tree OID. The fixture has two commits whose
    /// trees differ, so the tree-tip tests pin to `c2`'s tree.
    fn root_tree_of(commit: Sha, repo_dir: &Path) -> ObjectId {
        let repo = gix::open(repo_dir).unwrap();
        repo.find_object(*commit.as_object_id())
            .unwrap()
            .peel_to_kind(gix::object::Kind::Commit)
            .unwrap()
            .into_commit()
            .tree_id()
            .unwrap()
            .detach()
    }

    #[test]
    fn build_baseline_pack_for_tree_tip_includes_tree_closure_and_tag() {
        // Tag-of-tree: peel terminates at a tree, pack must carry the
        // tree, every reachable subtree, every blob, and the tag chain.
        let (repo_dir, _c1, c2) = fixture_two_commits();
        let root_tree = root_tree_of(c2, repo_dir.path());
        let repo = gix::open(repo_dir.path()).unwrap();
        let tag = write_annotated_tag(&repo, root_tree, gix::object::Kind::Tree, "v1-tree");
        drop(repo);

        let out = TempDir::new().unwrap();
        let peeled = PeeledTip::Tree {
            tree: root_tree,
            tag_chain: vec![tag],
        };
        let built = build_baseline_pack(repo_dir.path(), peeled, out.path()).expect("build");
        let (_dst_dir, dst_repo) = install_into_fresh_repo(&built.pack_path);
        let odb = dst_repo.objects.clone().into_inner();
        assert!(odb.contains(&tag), "tag object must be in pack");
        assert!(odb.contains(&root_tree), "leaf tree must be in pack");

        // The fixture's c2 tree has two blobs (a.txt v2, b.txt new) at
        // the root with no subtrees — pin both blob OIDs by walking the
        // tree from the source repo.
        let src = gix::open(repo_dir.path()).unwrap();
        let tree_obj = src.find_object(root_tree).unwrap().into_tree();
        for entry in tree_obj.iter() {
            let entry = entry.unwrap();
            assert!(
                odb.contains(entry.oid()),
                "blob {:?} must be in pack",
                entry.oid(),
            );
        }
    }

    #[test]
    fn build_baseline_pack_for_blob_tip_packs_only_blob_and_tag() {
        // Tag-of-blob: pack must carry the blob and the tag, nothing
        // else. Asserting the .idx object count pins the contract.
        let (repo_dir, _c1, _c2) = fixture_two_commits();
        let repo = gix::open(repo_dir.path()).unwrap();
        let blob = repo.write_blob(b"leaf").unwrap().detach();
        let tag = write_annotated_tag(&repo, blob, gix::object::Kind::Blob, "v1-blob");
        drop(repo);

        let out = TempDir::new().unwrap();
        let peeled = PeeledTip::Blob {
            blob,
            tag_chain: vec![tag],
        };
        let built = build_baseline_pack(repo_dir.path(), peeled, out.path()).expect("build");
        let idx = gix_pack::index::File::at(&built.idx_path, gix_hash::Kind::Sha1).unwrap();
        assert_eq!(
            idx.num_objects(),
            2,
            "blob-tip pack must contain exactly the blob + the tag",
        );

        let (_dst_dir, dst_repo) = install_into_fresh_repo(&built.pack_path);
        let odb = dst_repo.objects.clone().into_inner();
        assert!(odb.contains(&blob));
        assert!(odb.contains(&tag));
    }

    #[test]
    fn build_baseline_pack_for_bare_tree_ref_emits_tree_with_empty_chain() {
        // A ref pointing directly at a tree (no tag wrapper). Empty
        // tag_chain is allowed and must not regress the empty-chain
        // codepath in `count_objects_as_is`. The pack must carry the
        // full tree closure (subtree blobs included) — without the
        // closure check, a regression that emitted only the tree-OID
        // and dropped descent would still pass.
        let (repo_dir, _c1, c2) = fixture_two_commits();
        let root_tree = root_tree_of(c2, repo_dir.path());

        let out = TempDir::new().unwrap();
        let peeled = PeeledTip::Tree {
            tree: root_tree,
            tag_chain: Vec::new(),
        };
        let built = build_baseline_pack(repo_dir.path(), peeled, out.path()).expect("build");

        let (_dst_dir, dst_repo) = install_into_fresh_repo(&built.pack_path);
        let odb = dst_repo.objects.clone().into_inner();
        assert!(odb.contains(&root_tree));
        // Pin every blob the leaf tree references — this catches a
        // regression where `enumerate_tree_closure` stopped descending.
        let src = gix::open(repo_dir.path()).unwrap();
        let tree_obj = src.find_object(root_tree).unwrap().into_tree();
        for entry in tree_obj.iter() {
            let entry = entry.unwrap();
            assert!(
                odb.contains(entry.oid()),
                "tree blob {:?} must be in pack",
                entry.oid(),
            );
        }
    }
}

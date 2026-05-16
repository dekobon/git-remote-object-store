//! Native git bundle v2 read/write on top of `gix-pack`.
//!
//! The git bundle v2 format wraps a standard PACK file with a text header
//! describing the contained refs and any prerequisite commits. See
//! <https://git-scm.com/docs/bundle-format> for the spec.
//!
//! This module implements [`create`] (push path) and [`unbundle`] (fetch path),
//! replacing the former `git bundle create` / `git bundle unbundle` subprocess
//! calls in [`crate::git`].

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use gix::bstr::BStr;
use gix_hash::ObjectId;
use gix_pack::Find as _;
use gix_pack::data::output::bytes::FromEntriesIter;
use gix_pack::data::output::{count, entry};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::git::{PeeledTip, Sha};

/// First line of every git bundle v2 file.
const BUNDLE_V2_MAGIC: &str = "# v2 git bundle";
/// First line of a git bundle v3 file (not supported).
const BUNDLE_V3_MAGIC: &str = "# v3 git bundle";

/// Parsed bundle header as it appears before the PACK payload.
// `version` and `refs` are part of the format and available for callers; not
// all fields are consumed internally.
#[allow(dead_code)]
pub struct BundleHeader {
    /// Always 2 for bundles this module produces or accepts.
    pub version: u8,
    /// SHA-1 OIDs that must be present in the target ODB before unpacking.
    pub prerequisites: Vec<ObjectId>,
    /// `(sha, ref_name)` pairs listed in the header.
    pub refs: Vec<(ObjectId, Vec<u8>)>,
    /// Byte offset within the file where PACK data begins.
    pub pack_offset: u64,
}

impl BundleHeader {
    /// Read and parse the text header from the bundle file at `path`.
    pub fn read(path: &Path) -> Result<Self, BundleError> {
        let mut file = BufReader::new(fs::File::open(path)?);
        let mut line = String::new();

        file.read_line(&mut line)?;
        let magic = line.trim_end_matches(['\n', '\r']);
        if magic == BUNDLE_V3_MAGIC {
            return Err(BundleError::UnsupportedVersion(3));
        }
        if magic != BUNDLE_V2_MAGIC {
            return Err(BundleError::InvalidHeader(format!(
                "expected \"# v2 git bundle\", got {magic:?}",
            )));
        }

        let mut prerequisites = Vec::new();
        let mut refs = Vec::new();

        loop {
            line.clear();
            let n = file.read_line(&mut line)?;
            if n == 0 {
                return Err(BundleError::InvalidHeader(
                    "unexpected end of bundle header".to_owned(),
                ));
            }
            match parse_header_entry(&line)? {
                HeaderEntry::End => break,
                HeaderEntry::Prerequisite(oid) => prerequisites.push(oid),
                HeaderEntry::Ref(oid, name) => refs.push((oid, name)),
            }
        }

        // `pack_offset` captures the position of the PACK magic bytes. The
        // seek in `unbundle` jumps here so the magic is included in the data
        // handed to `gix_pack::Bundle::write_to_directory`.
        let pack_offset = file.stream_position()?;
        verify_pack_magic(&mut file)?;

        Ok(BundleHeader {
            version: 2,
            prerequisites,
            refs,
            pack_offset,
        })
    }
}

/// A single entry from the bundle v2 text header.
#[cfg_attr(test, derive(Debug))]
enum HeaderEntry {
    /// The blank line that terminates the header section.
    End,
    /// A `-<sha40>` prerequisite line.
    Prerequisite(ObjectId),
    /// A `<sha40> <refname>` ref line.
    Ref(ObjectId, Vec<u8>),
}

/// Classify one header line as a prerequisite, ref, or end-of-header.
///
/// The trailing `\n` / `\r\n` is stripped before classification.
fn parse_header_entry(line: &str) -> Result<HeaderEntry, BundleError> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return Ok(HeaderEntry::End);
    }
    if let Some(rest) = trimmed.strip_prefix('-') {
        // Prerequisite line: -<sha40> [optional comment]
        let sha_hex = rest.split_once(' ').map_or(rest, |(s, _)| s);
        let oid = parse_header_oid(sha_hex, "prerequisite")?;
        return Ok(HeaderEntry::Prerequisite(oid));
    }
    // Ref line: <sha40> <refname>
    let mut parts = trimmed.splitn(2, ' ');
    let sha_hex = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| BundleError::InvalidHeader(format!("empty ref line: {trimmed:?}")))?;
    let ref_name = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| BundleError::InvalidHeader(format!("missing ref name: {trimmed:?}")))?;
    let oid = parse_header_oid(sha_hex, "ref")?;
    Ok(HeaderEntry::Ref(oid, ref_name.as_bytes().to_vec()))
}

/// Parse a 40-hex object ID from a bundle header line, returning a
/// [`BundleError::InvalidHeader`] on failure.
///
/// Distinct from `lfs::agent::parse_oid` which validates LFS oid
/// strings (different format and error type) — the suffix `_header`
/// makes the call site unambiguous.
fn parse_header_oid(sha_hex: &str, context: &str) -> Result<ObjectId, BundleError> {
    ObjectId::from_hex(sha_hex.as_bytes())
        .map_err(|_| BundleError::InvalidHeader(format!("bad {context} SHA: {sha_hex:?}")))
}

/// Verify that the next four bytes in `file` are the `PACK` magic.
fn verify_pack_magic<R: Read>(file: &mut R) -> Result<(), BundleError> {
    let mut buf = [0u8; 4];
    // `read_exact` guarantees all 4 bytes are filled or returns an error;
    // `read` may legally return fewer bytes on the first call.
    file.read_exact(&mut buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            BundleError::InvalidHeader("bundle truncated before PACK data".to_owned())
        } else {
            BundleError::Io(e)
        }
    })?;
    if &buf != b"PACK" {
        return Err(BundleError::InvalidHeader(
            "expected PACK magic after bundle header".to_owned(),
        ));
    }
    Ok(())
}

/// Count `object_ids` verbatim — one pack entry per input OID, no
/// expansion. Used by both engines to append annotated-tag objects (and
/// any tag-of-tag chain) to a pack alongside a commit-walk count: the
/// tag objects themselves are leaves of the reachability graph (their
/// commit target is already in the commit count), so `AsIs` is the
/// correct expansion.
///
/// Returns an empty `Vec` for an empty input. Callers concatenate the
/// result onto their own `count::objects` output.
///
/// # Errors
///
/// Returns the underlying [`count::objects::Error`] verbatim. Callers
/// wrap in their engine's error type.
pub(crate) fn count_objects_as_is<F>(
    odb: F,
    object_ids: &[ObjectId],
) -> Result<Vec<gix_pack::data::output::Count>, count::objects::Error>
where
    F: gix_pack::Find + Send + Clone + 'static,
{
    if object_ids.is_empty() {
        return Ok(Vec::new());
    }
    let owned = object_ids.to_vec();
    let (counts, _) = count::objects(
        odb,
        Box::new(
            owned
                .into_iter()
                .map(Ok::<_, Box<dyn std::error::Error + Send + Sync + 'static>>),
        ),
        &gix::progress::Discard,
        &AtomicBool::new(false),
        count::objects::Options {
            input_object_expansion: count::objects::ObjectExpansion::AsIs,
            thread_limit: Some(1),
            ..Default::default()
        },
    )?;
    Ok(counts)
}

/// Create a git bundle v2 file at `<folder>/<sha>.bundle` and return the path.
///
/// `spec` is resolved against the repository at `cwd` (a fully-qualified ref
/// name, a short name, `HEAD`, or a bare commit / tree / blob OID). The
/// bundle's pack carries the leaf object plus everything needed to
/// reconstruct the ref:
///
/// - **Commit-tipped**: every commit reachable from the leaf, expanded
///   to trees + blobs, plus the tag chain.
/// - **Tree-tipped**: the leaf tree plus its full subtree + blob
///   closure (gitlinks skipped), plus the tag chain.
/// - **Blob-tipped**: the leaf blob plus the tag chain.
///
/// The bundle is written atomically via a temp file so partial bundles
/// are never visible to concurrent readers.
pub fn create(cwd: &Path, folder: &Path, sha: Sha, spec: &str) -> Result<PathBuf, BundleError> {
    let repo = gix::open(cwd)?;

    // `sha` names the bundle file and appears in the bundle header ref line.
    // `peeled` carries the leaf kind + tag chain; the seed-set for the count
    // phase depends on the kind.
    let (peeled, ref_name) = resolve_spec_to_ref(&repo, spec)?;

    // Strip the Proxy wrapper to expose the gix_pack::Find impl needed by the
    // output pipeline (gix::OdbHandle = Proxy<Cache<...>> does not implement
    // gix_pack::Find; the inner Cache<...> does).
    let mut odb = repo.objects.clone().into_inner();
    // The parallel pack-generation pipeline accesses `location_by_oid` which
    // panics unless the handle has been pinned against pack unloading.
    odb.prevent_pack_unload();

    // Dispatch on leaf kind: commit-tipped uses TreeContents over the
    // commit walk; tree-tipped enumerates the tree closure and uses
    // AsIs; blob-tipped passes the single blob with AsIs.
    let (input_oids, expansion, tag_chain) = match peeled {
        PeeledTip::Commit { commit, tag_chain } => {
            let ids = collect_commit_ids(&repo, *commit.as_object_id())?;
            (
                ids,
                count::objects::ObjectExpansion::TreeContents,
                tag_chain,
            )
        }
        PeeledTip::Tree { tree, tag_chain } => {
            let ids = crate::packchain::git::enumerate_tree_closure(&repo, tree)
                .map_err(|e| BundleError::Git(Box::new(e)))?;
            (ids, count::objects::ObjectExpansion::AsIs, tag_chain)
        }
        PeeledTip::Blob { blob, tag_chain } => {
            (vec![blob], count::objects::ObjectExpansion::AsIs, tag_chain)
        }
    };

    let (mut counts, _) = count::objects(
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
    )?;

    // For tag-ref pushes, append the annotated-tag objects (and any
    // tag-of-tag chain) verbatim. Without this, the bundle's pack
    // contains the leaf-reachable objects but not the tag object itself,
    // so a fetch-back of the tag ref would fail to update
    // `refs/tags/v1` because the tag-OID isn't in the receiver's ODB.
    counts.extend(count_objects_as_is(odb.clone(), &tag_chain)?);

    let num_entries = u32::try_from(counts.len())
        .map_err(|_| BundleError::PackEntry("too many objects for a single pack".to_owned()))?;

    let entries_iter = entry::iter_from_counts(
        counts,
        odb,
        Box::new(gix::progress::Discard),
        entry::iter_from_counts::Options {
            thread_limit: Some(1),
            ..Default::default()
        },
    )
    // Strip SequenceId — FromEntriesIter expects Iterator<Item = Result<Vec<Entry>, _>>.
    .map(|r| r.map(|(_, entries)| entries));

    let folder = folder.canonicalize()?;
    let bundle_path = folder.join(format!("{sha}.bundle"));
    let mut tmp = NamedTempFile::new_in(&folder)?;

    write_bundle_header(&mut tmp, sha, &ref_name)?;

    let pack_iter = FromEntriesIter::new(
        entries_iter,
        &mut tmp,
        num_entries,
        gix_pack::data::Version::V2,
        gix_hash::Kind::Sha1,
    );
    for result in pack_iter {
        result.map_err(|e| BundleError::PackEntry(e.to_string()))?;
    }

    tmp.persist(&bundle_path)
        .map_err(|e| BundleError::Io(e.error))?;
    Ok(bundle_path)
}

/// Walk all commits reachable from `tip_id` and return their OIDs.
fn collect_commit_ids(
    repo: &gix::Repository,
    tip_id: ObjectId,
) -> Result<Vec<ObjectId>, BundleError> {
    repo.rev_walk([tip_id])
        .all()
        .map_err(|e| BundleError::Walk(Box::new(e)))?
        .map(|info| info.map(|i| i.id))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| BundleError::Walk(Box::new(e)))
}

/// Write the bundle v2 text header (magic line, one ref line, blank separator).
///
/// `ref_name` must be a valid git ref name (gix-validated upstream by
/// `resolve_spec_to_ref`). Taking `&str` rather than `&[u8]` pushes the
/// UTF-8 invariant into the type system so the function body has no
/// `expect()` to fall over on a malformed caller.
fn write_bundle_header<W: Write>(
    writer: &mut W,
    sha: Sha,
    ref_name: &str,
) -> Result<(), BundleError> {
    writeln!(writer, "{BUNDLE_V2_MAGIC}")?;
    writeln!(writer, "{sha} {ref_name}")?;
    writeln!(writer)?;
    Ok(())
}

/// Install the pack from `<folder>/<sha>.bundle` into the repository at `cwd`.
///
/// Objects become immediately available via gix's dynamic store. No ref is
/// created — that is the remote-helper protocol's responsibility (confirmed by
/// the contract documented in [`crate::git::unbundle_at`]).
pub fn unbundle(cwd: &Path, folder: &Path, sha: Sha) -> Result<(), BundleError> {
    let folder = folder.canonicalize()?;
    let bundle_path = folder.join(format!("{sha}.bundle"));

    let header = BundleHeader::read(&bundle_path)?;
    let repo = gix::open(cwd)?;

    // Prerequisite check: all referenced base objects must already be present.
    let odb = repo.objects.clone().into_inner();
    for prereq in &header.prerequisites {
        if !odb.contains(prereq) {
            return Err(BundleError::MissingPrerequisite(*prereq));
        }
    }

    let pack_dir = repo.git_dir().join("objects/pack");
    fs::create_dir_all(&pack_dir)?;

    let mut bundle_file = BufReader::new(fs::File::open(&bundle_path)?);
    bundle_file.seek(io::SeekFrom::Start(header.pack_offset))?;

    let interrupted = AtomicBool::new(false);
    let outcome = gix_pack::Bundle::write_to_directory(
        &mut bundle_file,
        Some(&pack_dir),
        &mut gix::progress::Discard,
        &interrupted,
        None::<gix::odb::Handle>,
        gix_pack::bundle::write::Options {
            object_hash: gix_hash::Kind::Sha1,
            ..Default::default()
        },
    )?;

    // write_to_directory creates a .keep file before installing the pack to
    // prevent git-gc from collecting the new objects before refs point to them.
    // Callers are responsible for removing it once refs are established.
    //
    // We remove it here because git updates refs and invokes any post-fetch
    // GC only after the remote helper exits and the protocol exchange is
    // complete — a point at which the new objects are already reachable.
    // `git gc --auto` is a synchronous post-operation step, not a background
    // daemon, so it cannot run during the window between pack installation
    // (this call) and ref update (performed by git after the helper exits).
    // Leaving .keep files in place permanently would prevent git-repack from
    // consolidating packs, causing lookup performance to degrade linearly with
    // the number of fetches.
    if let Some(keep_path) = outcome.keep_path
        && let Err(e) = fs::remove_file(&keep_path)
        && e.kind() != io::ErrorKind::NotFound
    {
        return Err(BundleError::Io(e));
    }

    Ok(())
}

/// Resolve `spec` in `repo` to `(peeled, canonical_ref_name)`.
///
/// `peeled` is the [`PeeledTip`] produced by walking the resolved OID
/// through any annotated-tag chain — its variant identifies the leaf
/// kind, and `tag_chain()` lists the tag objects encountered. Both are
/// shared with the packchain engine so the two engines agree on tag /
/// tree / blob handling and chain order.
///
/// gix ref names are required to be valid UTF-8 by `gix-validate`, so
/// the conversion below cannot fail in practice; it is wrapped in an
/// explicit `from_utf8` check anyway so the conversion error has a
/// clear cause if a future gix version relaxes the rule.
fn resolve_spec_to_ref(
    repo: &gix::Repository,
    spec: &str,
) -> Result<(PeeledTip, String), BundleError> {
    let resolved = repo.rev_parse_single(BStr::new(spec))?.detach();
    let peeled = crate::git::peel_tag_chain(repo, Sha::from_object_id(resolved))
        .map_err(|e| BundleError::Git(Box::new(e)))?;

    // Follow symrefs one level (HEAD -> refs/heads/main) for the bundle ref line.
    let ref_name = match repo.try_find_reference(spec) {
        Ok(Some(r)) => {
            let bytes = if let Some(Ok(followed)) = r.follow() {
                followed.name().as_bstr().to_vec()
            } else {
                r.name().as_bstr().to_vec()
            };
            String::from_utf8(bytes)
                .map_err(|_| BundleError::InvalidHeader("ref name is not valid UTF-8".to_owned()))?
        }
        // Bare SHA or any unresolvable spec: use spec as-is (already &str).
        _ => spec.to_owned(),
    };

    Ok((peeled, ref_name))
}

/// Errors from [`create`] and [`unbundle`].
#[derive(Debug, Error)]
pub enum BundleError {
    /// Bundle header was malformed.
    #[error("invalid bundle header: {0}")]
    InvalidHeader(String),
    /// Bundle uses a version this module does not support (only v2).
    #[error("unsupported bundle version {0}; only v2 is supported")]
    UnsupportedVersion(u8),
    /// Prerequisite object is not present in the target repository.
    #[error("missing prerequisite {0}")]
    MissingPrerequisite(ObjectId),
    /// `gix::open()` failed.
    #[error("open repository: {0}")]
    Repo(Box<gix::open::Error>),
    /// `rev_parse_single` failed.
    #[error("rev-parse: {0}")]
    RevParse(Box<gix::revision::spec::parse::single::Error>),
    /// Object lookup failed while resolving spec to commit.
    #[error("find object: {0}")]
    FindObject(Box<gix::object::find::existing::Error>),
    /// Object peel to commit kind failed.
    #[error("peel to commit: {0}")]
    PeelToKind(Box<gix::object::peel::to_kind::Error>),
    /// Commit graph traversal failed.
    #[error("object walk: {0}")]
    Walk(Box<dyn std::error::Error + Send + Sync + 'static>),
    /// Object counting phase failed.
    #[error("pack count: {0}")]
    PackCount(Box<count::objects::Error>),
    /// Pack entry serialization failed.
    #[error("pack entry: {0}")]
    PackEntry(String),
    /// `Bundle::write_to_directory` failed.
    #[error("pack write: {0}")]
    PackWrite(Box<gix_pack::bundle::write::Error>),
    /// I/O error.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Underlying git operation failed (peel, find-object, etc.) —
    /// surfaces errors from the shared `peel_tag_chain` helper and
    /// from tree-closure enumeration.
    #[error(transparent)]
    Git(Box<crate::git::GitError>),
}

impl From<gix::open::Error> for BundleError {
    fn from(e: gix::open::Error) -> Self {
        Self::Repo(Box::new(e))
    }
}

impl From<gix::revision::spec::parse::single::Error> for BundleError {
    fn from(e: gix::revision::spec::parse::single::Error) -> Self {
        Self::RevParse(Box::new(e))
    }
}

impl From<gix::object::find::existing::Error> for BundleError {
    fn from(e: gix::object::find::existing::Error) -> Self {
        Self::FindObject(Box::new(e))
    }
}

impl From<gix::object::peel::to_kind::Error> for BundleError {
    fn from(e: gix::object::peel::to_kind::Error) -> Self {
        Self::PeelToKind(Box::new(e))
    }
}

impl From<count::objects::Error> for BundleError {
    fn from(e: count::objects::Error) -> Self {
        Self::PackCount(Box::new(e))
    }
}

impl From<gix_pack::bundle::write::Error> for BundleError {
    fn from(e: gix_pack::bundle::write::Error) -> Self {
        Self::PackWrite(Box::new(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba98";

    // --- parse_header_entry --------------------------------------------

    #[test]
    fn parse_header_entry_recognises_blank_line_as_end() {
        match parse_header_entry("\n").expect("parse") {
            HeaderEntry::End => {}
            other => panic!("expected End, got {other:?}"),
        }
        match parse_header_entry("\r\n").expect("parse") {
            HeaderEntry::End => {}
            other => panic!("expected End, got {other:?}"),
        }
        match parse_header_entry("").expect("parse") {
            HeaderEntry::End => {}
            other => panic!("expected End, got {other:?}"),
        }
    }

    #[test]
    fn parse_header_entry_parses_prerequisite_with_optional_comment() {
        let line = format!("-{SHA}\n");
        let entry = parse_header_entry(&line).expect("parse");
        let HeaderEntry::Prerequisite(oid) = entry else {
            panic!("expected Prerequisite, got {entry:?}");
        };
        assert_eq!(oid.to_hex().to_string(), SHA);

        // Prerequisite with trailing comment is also accepted.
        let with_comment = format!("-{OTHER_SHA} a comment\n");
        let entry = parse_header_entry(&with_comment).expect("parse");
        let HeaderEntry::Prerequisite(oid) = entry else {
            panic!("expected Prerequisite, got {entry:?}");
        };
        assert_eq!(oid.to_hex().to_string(), OTHER_SHA);
    }

    #[test]
    fn parse_header_entry_parses_ref_line() {
        let line = format!("{SHA} refs/heads/main\n");
        let entry = parse_header_entry(&line).expect("parse");
        let HeaderEntry::Ref(oid, name_bytes) = entry else {
            panic!("expected Ref, got {entry:?}");
        };
        assert_eq!(oid.to_hex().to_string(), SHA);
        assert_eq!(name_bytes, b"refs/heads/main");
    }

    #[test]
    fn parse_header_entry_rejects_truncated_ref_line() {
        // SHA but no ref name.
        let line = format!("{SHA}\n");
        match parse_header_entry(&line) {
            Err(BundleError::InvalidHeader(msg)) => {
                assert!(
                    msg.contains("missing ref name"),
                    "expected missing-ref-name wording, got {msg:?}",
                );
            }
            other => panic!("expected InvalidHeader, got {other:?}"),
        }
    }

    #[test]
    fn parse_header_entry_rejects_bad_sha_in_ref_line() {
        // 39 hex chars — off-by-one short of the required 40, the
        // boundary case most likely to slip through a length check.
        let bad = "0123456789abcdef0123456789abcdef0123456";
        assert_eq!(bad.len(), 39);
        let line = format!("{bad} refs/heads/main\n");
        match parse_header_entry(&line) {
            Err(BundleError::InvalidHeader(msg)) => {
                assert!(
                    msg.contains("bad ref SHA"),
                    "expected ref SHA wording, got {msg:?}",
                );
                // The bad input is echoed back so operators can see
                // what was rejected; without this the wording check
                // would pass on any future "bad ref SHA: <unrelated>".
                assert!(
                    msg.contains(bad),
                    "expected echoed SHA in message, got {msg:?}",
                );
            }
            other => panic!("expected InvalidHeader, got {other:?}"),
        }
    }

    #[test]
    fn parse_header_entry_rejects_bad_sha_in_prerequisite_line() {
        let line = "-not-a-sha\n";
        match parse_header_entry(line) {
            Err(BundleError::InvalidHeader(msg)) => {
                assert!(
                    msg.contains("bad prerequisite SHA"),
                    "expected prerequisite SHA wording, got {msg:?}",
                );
            }
            other => panic!("expected InvalidHeader, got {other:?}"),
        }
    }

    // --- parse_header_oid ---------------------------------------------

    #[test]
    fn parse_header_oid_accepts_lowercase_hex() {
        let oid = parse_header_oid(SHA, "test").expect("parse");
        assert_eq!(oid.to_hex().to_string(), SHA);
    }

    #[test]
    fn parse_header_oid_rejects_short_hex_and_names_context() {
        let err = parse_header_oid("abc", "ref").unwrap_err();
        let BundleError::InvalidHeader(msg) = err else {
            panic!("expected InvalidHeader, got {err:?}");
        };
        // Context is interpolated into the error message.
        assert!(msg.contains("bad ref SHA"), "context not in message: {msg}");
    }

    // --- verify_pack_magic --------------------------------------------

    #[test]
    fn verify_pack_magic_accepts_pack_bytes() {
        let mut data: &[u8] = b"PACK extra";
        verify_pack_magic(&mut data).expect("PACK accepted");
        // The slice's `Read` impl advances by exactly the bytes read,
        // so after a successful 4-byte `read_exact` the remainder must
        // be everything past `PACK`. Asserting on the residue catches a
        // regression where a future implementation reads past the
        // magic (e.g. peeks the pack version) without rewinding.
        assert_eq!(data, b" extra");
    }

    #[test]
    fn verify_pack_magic_rejects_non_pack_bytes() {
        let mut data: &[u8] = b"NOPE";
        match verify_pack_magic(&mut data) {
            Err(BundleError::InvalidHeader(msg)) => {
                assert!(msg.contains("expected PACK magic"), "wrong wording: {msg}");
            }
            other => panic!("expected InvalidHeader, got {other:?}"),
        }
    }

    #[test]
    fn verify_pack_magic_rejects_truncated_input_with_specific_error() {
        // Less than 4 bytes — UnexpectedEof must surface as the
        // truncation-specific InvalidHeader, not the generic Io variant.
        let mut data: &[u8] = b"PA";
        match verify_pack_magic(&mut data) {
            Err(BundleError::InvalidHeader(msg)) => {
                assert!(
                    msg.contains("truncated before PACK"),
                    "wrong wording: {msg}",
                );
            }
            other => panic!("expected InvalidHeader for truncation, got {other:?}"),
        }
    }

    // --- create / unbundle round-trips with tag chains ----------------

    use gix::actor::SignatureRef;
    use tempfile::TempDir;

    fn signature() -> SignatureRef<'static> {
        SignatureRef {
            name: BStr::new("Tester"),
            email: BStr::new("t@example.com"),
            time: "0 +0000",
        }
    }

    /// Single-commit fixture; returns `(repo_dir, commit_oid)`.
    fn fixture_commit() -> (TempDir, ObjectId) {
        let tmp = TempDir::new().unwrap();
        let repo = gix::init(tmp.path()).unwrap();
        let blob = repo.write_blob(b"hello").unwrap().detach();
        let tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![gix::objs::tree::Entry {
                    mode: gix::objs::tree::EntryKind::Blob.into(),
                    filename: "a.txt".into(),
                    oid: blob,
                }],
            })
            .unwrap()
            .detach();
        let commit = repo
            .commit_as(
                signature(),
                signature(),
                "refs/heads/main",
                "first",
                tree,
                std::iter::empty::<ObjectId>(),
            )
            .unwrap()
            .detach();
        (tmp, commit)
    }

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

    fn create_tag_ref(repo: &gix::Repository, name: &str, target: ObjectId) {
        repo.reference(
            name,
            target,
            gix::refs::transaction::PreviousValue::MustNotExist,
            "create tag",
        )
        .unwrap();
    }

    /// Install a bundle into a fresh repo and return the destination
    /// repo handle (and its tempdir, which keeps the on-disk state alive).
    fn install_bundle_into_fresh_repo(bundle_path: &Path, sha: Sha) -> (TempDir, gix::Repository) {
        let dst = TempDir::new().unwrap();
        gix::init(dst.path()).unwrap();
        let folder = bundle_path.parent().unwrap().to_owned();
        unbundle(dst.path(), &folder, sha).unwrap();
        let dst_repo = gix::open(dst.path()).unwrap();
        (dst, dst_repo)
    }

    #[test]
    fn bundle_create_round_trips_annotated_tag() {
        // E9: bundle's pack must include the tag object so a fetch-back
        // resolves `refs/tags/v1` to the tag-OID and `v1^{}` finds the
        // commit.
        let (repo_dir, commit) = fixture_commit();
        let repo = gix::open(repo_dir.path()).unwrap();
        let tag_oid = write_annotated_tag(&repo, commit, gix::object::Kind::Commit, "v1");
        create_tag_ref(&repo, "refs/tags/v1", tag_oid);
        drop(repo);

        let folder = TempDir::new().unwrap();
        let tag_sha = Sha::from_object_id(tag_oid);
        let bundle_path =
            create(repo_dir.path(), folder.path(), tag_sha, "refs/tags/v1").expect("create bundle");

        let (_dst_dir, dst_repo) = install_bundle_into_fresh_repo(&bundle_path, tag_sha);
        let odb = dst_repo.objects.clone().into_inner();
        assert!(
            odb.contains(&tag_oid),
            "tag object must be installed by unbundle",
        );
        assert!(
            odb.contains(&commit),
            "commit target must also be installed"
        );
        let tag_obj = dst_repo
            .find_object(tag_oid)
            .unwrap()
            .peel_to_kind(gix::object::Kind::Tag)
            .unwrap();
        assert_eq!(
            tag_obj.into_tag().target_id().unwrap().detach(),
            commit,
            "round-tripped tag must point at the original commit",
        );
    }

    #[test]
    fn bundle_create_with_branch_tip_emits_unchanged_pack() {
        // E1: regression — the second AsIs pass MUST be gated on a
        // non-empty tag chain. Pin the object count for the
        // commit-only case (commit + tree + blob = 3).
        let (repo_dir, commit) = fixture_commit();
        let folder = TempDir::new().unwrap();
        create(
            repo_dir.path(),
            folder.path(),
            Sha::from_object_id(commit),
            "refs/heads/main",
        )
        .expect("create bundle");

        // Install into a fresh repo and count via the .idx that
        // gix-pack derives — `num_objects()` is the wire-stable
        // measure that catches the AsIs second-pass leaking into the
        // empty-tag-chain code path.
        let dst = TempDir::new().unwrap();
        gix::init(dst.path()).unwrap();
        unbundle(dst.path(), folder.path(), Sha::from_object_id(commit)).unwrap();
        let dst_repo = gix::open(dst.path()).unwrap();
        // Find the installed pack and count its entries.
        let pack_dir = dst_repo.git_dir().join("objects/pack");
        let idx_path = std::fs::read_dir(&pack_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|ext| ext == "idx"))
            .expect("idx file must exist");
        let idx = gix_pack::index::File::at(&idx_path, gix_hash::Kind::Sha1).unwrap();
        assert_eq!(
            idx.num_objects(),
            3,
            "branch-tip bundle must contain commit + tree + blob (no tag chain)",
        );
    }

    #[test]
    fn bundle_create_round_trips_tag_pointing_to_blob() {
        // #80: tag-of-blob is now supported. Bundle's pack contains
        // exactly the leaf blob and the tag object — no commit walk,
        // no tree closure.
        let (repo_dir, _commit) = fixture_commit();
        let repo = gix::open(repo_dir.path()).unwrap();
        let blob = repo.write_blob(b"data").unwrap().detach();
        let tag_oid = write_annotated_tag(&repo, blob, gix::object::Kind::Blob, "blob-tag");
        create_tag_ref(&repo, "refs/tags/blob-tag", tag_oid);
        drop(repo);

        let folder = TempDir::new().unwrap();
        let tag_sha = Sha::from_object_id(tag_oid);
        let bundle_path = create(
            repo_dir.path(),
            folder.path(),
            tag_sha,
            "refs/tags/blob-tag",
        )
        .expect("blob-tag bundle must build");

        let (_dst_dir, dst_repo) = install_bundle_into_fresh_repo(&bundle_path, tag_sha);
        let odb = dst_repo.objects.clone().into_inner();
        assert!(odb.contains(&tag_oid), "tag object must land in pack");
        assert!(odb.contains(&blob), "blob target must land in pack");
        // The pack must NOT carry the unrelated commit / tree / blob
        // from the fixture — the leaf's chain is just `tag → blob`.
        // Pin the exact object count so a regression that accidentally
        // walked the fixture's commit graph would be caught.
        let pack_dir = dst_repo.git_dir().join("objects/pack");
        let idx_path = std::fs::read_dir(&pack_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|ext| ext == "idx"))
            .expect("idx file must exist");
        let idx = gix_pack::index::File::at(&idx_path, gix_hash::Kind::Sha1).unwrap();
        assert_eq!(
            idx.num_objects(),
            2,
            "blob-tag bundle must contain exactly the blob + the tag",
        );
        // Decode the tag and pin its target kind.
        let tag_obj = dst_repo
            .find_object(tag_oid)
            .unwrap()
            .peel_to_kind(gix::object::Kind::Tag)
            .unwrap();
        let target_id = tag_obj.into_tag().target_id().unwrap().detach();
        assert_eq!(
            target_id, blob,
            "tag must point at the blob it was created for",
        );
    }

    #[test]
    fn bundle_create_round_trips_tag_pointing_to_tree() {
        // #80: tag-of-tree round-trips through bundle. Pack carries the
        // tag, the leaf tree, and every blob in the tree closure.
        let (repo_dir, commit) = fixture_commit();
        let repo = gix::open(repo_dir.path()).unwrap();
        let tree_id = repo
            .find_object(commit)
            .unwrap()
            .peel_to_kind(gix::object::Kind::Commit)
            .unwrap()
            .into_commit()
            .tree_id()
            .unwrap()
            .detach();
        let tag_oid = write_annotated_tag(&repo, tree_id, gix::object::Kind::Tree, "tree-tag");
        create_tag_ref(&repo, "refs/tags/tree-tag", tag_oid);
        // Capture the blobs the leaf tree references so we can assert
        // they survived the round-trip.
        let tree_blobs: Vec<ObjectId> = {
            let tree_obj = repo.find_object(tree_id).unwrap().into_tree();
            tree_obj
                .iter()
                .map(|e| e.unwrap().oid().to_owned())
                .collect()
        };
        drop(repo);

        let folder = TempDir::new().unwrap();
        let tag_sha = Sha::from_object_id(tag_oid);
        let bundle_path = create(
            repo_dir.path(),
            folder.path(),
            tag_sha,
            "refs/tags/tree-tag",
        )
        .expect("tree-tag bundle must build");

        let (_dst_dir, dst_repo) = install_bundle_into_fresh_repo(&bundle_path, tag_sha);
        let odb = dst_repo.objects.clone().into_inner();
        assert!(odb.contains(&tag_oid), "tag must land in pack");
        assert!(odb.contains(&tree_id), "leaf tree must land in pack");
        for blob in &tree_blobs {
            assert!(odb.contains(blob), "tree blob {blob} must land in pack");
        }
    }
}

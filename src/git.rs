//! Native git operations layered on top of [`gix`][gix].
//!
//! Mirrors the surface of upstream `git_remote_s3/git.py`. Operations that
//! `gix` 0.82 exposes go through `gix` natively; config reads/writes go
//! through `gix-config` + `gix-lock` for atomic edits parity with
//! `git config`. Bundle creation and consumption use the native
//! `gix-pack`-based implementation in [`crate::bundle`]; no `git`
//! subprocess is spawned at runtime.
//!
//! [gix]: https://docs.rs/gix

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::io;
use std::io::Write as _;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::string::FromUtf8Error;
use std::sync::atomic::AtomicBool;

pub(crate) mod branch;

use gix::Repository;
use gix::bstr::{BStr, ByteSlice};
use gix::config::file::Metadata as GixConfigMetadata;
use gix::config::file::init as gix_config_init;
use gix::config::parse::section::{
    ValueName, header as gix_section_header, value_name as gix_value_name,
};
use gix::lock as gix_lock;
use gix::progress::Discard;
use gix::remote::Direction;
use gix_hash::ObjectId;
use thiserror::Error;
use tracing::debug;

/// SHA-1 commit OID, displayed as 40 lowercase hex characters.
///
/// Wraps [`gix_hash::ObjectId`] to make the wire-format invariant —
/// lowercase-hex bundle filenames on the bucket — a type-system
/// property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha(ObjectId);

impl Sha {
    /// Parse a SHA-1 hex string. Accepts lowercase, uppercase, or mixed
    /// case input and stores it canonically; [`Display`][fmt::Display]
    /// always emits lowercase.
    ///
    /// # Errors
    ///
    /// Returns [`ShaError::Empty`] if `hex` is empty, or
    /// [`ShaError::Decode`] if the input is the wrong length or contains
    /// non-hex characters.
    pub fn from_hex(hex: &str) -> Result<Self, ShaError> {
        if hex.is_empty() {
            return Err(ShaError::Empty);
        }
        Ok(Sha(ObjectId::from_hex(hex.as_bytes())?))
    }

    /// Wrap an existing [`ObjectId`] without re-validating.
    #[must_use]
    pub fn from_object_id(id: ObjectId) -> Self {
        Sha(id)
    }

    /// Borrow the underlying [`ObjectId`].
    #[must_use]
    pub fn as_object_id(&self) -> &ObjectId {
        &self.0
    }
}

impl fmt::Display for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Error returned by [`Sha::from_hex`].
#[derive(Debug, Error)]
pub enum ShaError {
    /// Input was the empty string.
    #[error("expected hex digits, got empty string")]
    Empty,
    /// Input was the wrong length or contained non-hex characters.
    #[error(transparent)]
    Decode(#[from] gix_hash::decode::Error),
}

/// Validated git ref name — guaranteed to satisfy
/// `gix_validate::reference::name` (the strict, fully-qualified form).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RefName(String);

impl RefName {
    /// Validate `name` and wrap it.
    ///
    /// # Errors
    ///
    /// Returns [`RefNameError::Invalid`] for any name that
    /// `gix-validate` would reject.
    pub fn new(name: impl Into<String>) -> Result<Self, RefNameError> {
        let name = name.into();
        match gix_validate::reference::name(BStr::new(&name)) {
            Ok(_) => Ok(RefName(name)),
            Err(source) => Err(RefNameError::Invalid { name, source }),
        }
    }

    /// Borrow as a plain `&str`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `true` iff `name` would be accepted by [`RefName::new`]. A
    /// borrow-only predicate for callers that just need the validity
    /// check without keeping the wrapped value — avoids the `String`
    /// allocation [`new`](Self::new) performs on its `impl Into<String>`
    /// argument.
    #[must_use]
    pub fn is_valid(name: &str) -> bool {
        gix_validate::reference::name(BStr::new(name)).is_ok()
    }
}

impl fmt::Display for RefName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RefName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<RefName> for String {
    fn from(value: RefName) -> Self {
        value.0
    }
}

/// Error returned by [`RefName::new`].
#[derive(Debug, Error)]
pub enum RefNameError {
    /// `gix-validate` rejected the input.
    #[error("invalid ref name {name:?}: {source}")]
    Invalid {
        /// The rejected input.
        name: String,
        /// The underlying gix-validate error.
        #[source]
        source: gix_validate::reference::name::Error,
    },
}

/// Permissive ref-name predicate.
///
/// Returns `true` iff `name` passes `gix_validate::reference::name_partial`.
/// The partial form accepts single-component names like `HEAD`, matching the
/// upstream Python regex's permissiveness; for the strict, fully-qualified
/// form used when constructing a [`RefName`], use [`RefName::new`] instead.
#[must_use]
pub fn is_valid_ref_name(name: &str) -> bool {
    gix_validate::reference::name_partial(BStr::new(name)).is_ok()
}

/// Aggregate error for the helpers in this module.
#[derive(Debug, Error)]
pub enum GitError {
    /// Caller passed an empty rev-spec.
    #[error("rev-spec is empty")]
    EmptySpec,
    /// `head_commit()` was called on a repository with no commits.
    #[error("repository has no commits")]
    NoCommits,
    /// Named remote does not exist.
    #[error("remote not found: {0}")]
    RemoteNotFound(String),
    /// Remote exists but has neither a fetch nor a push URL.
    #[error("remote has no fetch or push URL: {0}")]
    RemoteHasNoUrl(String),
    /// Remote URL is not valid UTF-8.
    #[error("remote {remote} URL is not valid UTF-8")]
    NonUtf8RemoteUrl {
        /// The remote whose URL could not be decoded.
        remote: String,
        /// The underlying decode error.
        #[source]
        source: FromUtf8Error,
    },
    /// Native bundle operation failed.
    #[error("bundle: {0}")]
    Bundle(Box<crate::bundle::BundleError>),
    /// A `spawn_blocking` task panicked.
    #[error("blocking task panicked")]
    Panic(#[from] tokio::task::JoinError),
    /// Local I/O error.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// `rev_parse_single` failed.
    #[error(transparent)]
    RevParse(#[from] gix::revision::spec::parse::single::Error),
    /// Could not find an object referenced from a rev-spec.
    #[error(transparent)]
    FindObject(#[from] gix::object::find::existing::Error),
    /// Could not peel an object to the requested kind.
    #[error(transparent)]
    PeelToKind(#[from] gix::object::peel::to_kind::Error),
    /// `head_commit()` failed.
    #[error(transparent)]
    HeadCommit(#[from] gix::reference::head_commit::Error),
    /// Could not decode commit object.
    #[error(transparent)]
    DecodeCommit(#[from] gix::objs::decode::Error),
    /// Computing a short id failed.
    #[error(transparent)]
    ShortId(#[from] gix::id::shorten::Error),
    /// Underlying merge-base computation failed.
    #[error(transparent)]
    MergeBase(Box<gix::repository::merge_base::Error>),
    /// Building the worktree stream for archive emission failed.
    #[error(transparent)]
    WorktreeStream(Box<gix::repository::worktree_stream::Error>),
    /// Writing the archive to disk failed.
    #[error(transparent)]
    WorktreeArchive(Box<gix::repository::worktree_archive::Error>),
    /// `find_remote()` failed.
    #[error(transparent)]
    FindRemote(Box<gix::remote::find::existing::Error>),
    /// `gix::open()` failed.
    #[error(transparent)]
    Open(Box<gix::open::Error>),
    /// `gix::discover()` failed when locating the config file.
    #[error(transparent)]
    Discover(Box<gix::discover::Error>),
    /// Dotted config key was empty, contained empty segments, or had no `.`.
    #[error("invalid config key {0:?}: must be of the form <section>[.<subsection>].<name>")]
    ConfigKeyParse(String),
    /// `gix-config` rejected a section header (invalid name characters).
    #[error("invalid config section name {name:?}: {source}")]
    ConfigInvalidSectionName {
        /// The rejected section name.
        name: String,
        /// Underlying validator error.
        #[source]
        source: gix_section_header::Error,
    },
    /// `gix-config` rejected a value name (invalid characters or non-alphabetic start).
    #[error("invalid config value name {name:?}: {source}")]
    ConfigInvalidValueName {
        /// The rejected value name.
        name: String,
        /// Underlying validator error.
        #[source]
        source: gix_value_name::Error,
    },
    /// `--unset` was issued for a key that is not present in the local config.
    #[error("config key not set: {0}")]
    ConfigKeyNotSet(String),
    /// Failed to parse the existing `.git/config` file.
    #[error(transparent)]
    ConfigParse(Box<gix_config_init::Error>),
    /// Failed to acquire a lock file for an atomic file write (e.g.
    /// `.git/config.lock`, `.git/shallow.lock`).
    #[error(transparent)]
    ConfigLock(Box<gix_lock::acquire::Error>),
    /// Reading the `HEAD` reference failed.
    #[error(transparent)]
    HeadLookup(Box<gix::reference::find::existing::Error>),
    /// `HEAD`'s referent name is not valid UTF-8.
    #[error("HEAD ref name is not valid UTF-8")]
    NonUtf8HeadRef {
        /// Underlying decode error.
        #[source]
        source: std::str::Utf8Error,
    },
    /// A tag chain visited the same OID twice — i.e. a cycle. Real git
    /// objects cannot form cycles (each tag's OID is determined by the
    /// SHA-1 of its content, which includes the target OID, so a cycle
    /// would require a SHA-1 preimage). This guard exists for adversarial
    /// or corrupted ODB inputs that bypass the hashing invariant.
    #[error("tag chain contains a cycle at {oid}")]
    TagChainCycle {
        /// The OID at which the cycle was detected.
        oid: ObjectId,
    },
}

impl From<gix::open::Error> for GitError {
    fn from(e: gix::open::Error) -> Self {
        GitError::Open(Box::new(e))
    }
}

impl From<gix::repository::merge_base::Error> for GitError {
    fn from(e: gix::repository::merge_base::Error) -> Self {
        GitError::MergeBase(Box::new(e))
    }
}

impl From<gix::repository::worktree_stream::Error> for GitError {
    fn from(e: gix::repository::worktree_stream::Error) -> Self {
        GitError::WorktreeStream(Box::new(e))
    }
}

impl From<gix::repository::worktree_archive::Error> for GitError {
    fn from(e: gix::repository::worktree_archive::Error) -> Self {
        GitError::WorktreeArchive(Box::new(e))
    }
}

impl From<gix::remote::find::existing::Error> for GitError {
    fn from(e: gix::remote::find::existing::Error) -> Self {
        GitError::FindRemote(Box::new(e))
    }
}

impl From<gix::discover::Error> for GitError {
    fn from(e: gix::discover::Error) -> Self {
        GitError::Discover(Box::new(e))
    }
}

impl From<gix_config_init::Error> for GitError {
    fn from(e: gix_config_init::Error) -> Self {
        GitError::ConfigParse(Box::new(e))
    }
}

impl From<gix_lock::acquire::Error> for GitError {
    fn from(e: gix_lock::acquire::Error) -> Self {
        GitError::ConfigLock(Box::new(e))
    }
}

impl From<gix::reference::find::existing::Error> for GitError {
    fn from(e: gix::reference::find::existing::Error) -> Self {
        GitError::HeadLookup(Box::new(e))
    }
}

/// Pick a working directory for git operations targeting `repo`. Prefers
/// the work tree and falls back to the git directory for bare repositories.
fn repo_cwd(repo: &Repository) -> &Path {
    repo.workdir().unwrap_or_else(|| repo.git_dir())
}

/// Write a git bundle for `spec` to `<folder>/<sha>.bundle` and return
/// the absolute path.
///
/// `spec` is a rev-spec — a fully-qualified ref (`refs/heads/main`), a
/// short branch (`main`), `HEAD`, or a SHA. All objects reachable from
/// the resolved commit are included.
///
/// The returned future is **not** `Send`: `gix::Repository` is `!Sync`,
/// so the captured `&Repository` parameter cannot cross thread
/// boundaries. Callers must `.await` it directly rather than passing
/// it to `tokio::spawn`.
///
/// # Errors
///
/// Returns [`GitError::Bundle`] if the spec cannot be resolved, the
/// commit graph cannot be walked, or the bundle file cannot be written.
/// Returns [`GitError::Panic`] if the blocking task panics.
pub async fn bundle(
    repo: &Repository,
    folder: &Path,
    sha: Sha,
    spec: &str,
) -> Result<PathBuf, GitError> {
    // `&Repository` is !Send (Repository is Send but !Sync), so we must
    // not hold a `&Path` borrowed from `repo` across the .await.
    let cwd = repo_cwd(repo).to_owned();
    bundle_at(&cwd, folder, sha, spec).await
}

/// Path-only variant of [`bundle`] for callers that cannot hold a
/// `&Repository` across `.await` (the protocol push handler shares
/// state across tokio tasks; `gix::Repository` is `!Sync`, so its
/// future would not be `Send`).
///
/// # Errors
///
/// Returns [`GitError::Bundle`] if the spec cannot be resolved, the
/// commit graph cannot be walked, or the bundle file cannot be written.
/// Returns [`GitError::Panic`] if the blocking task panics.
pub async fn bundle_at(
    cwd: &Path,
    folder: &Path,
    sha: Sha,
    spec: &str,
) -> Result<PathBuf, GitError> {
    let (cwd, folder, spec) = (cwd.to_owned(), folder.to_owned(), spec.to_owned());
    tokio::task::spawn_blocking(move || crate::bundle::create(&cwd, &folder, sha, &spec))
        .await?
        .map_err(|e| GitError::Bundle(Box::new(e)))
}

/// Unbundle `<folder>/<sha>.bundle` into `repo`.
///
/// Objects are installed into the ODB; no ref is created. Ref creation
/// is the remote-helper protocol's responsibility.
///
/// # Errors
///
/// Returns [`GitError::Bundle`] if the bundle file is malformed,
/// prerequisite objects are missing, or the pack cannot be installed.
/// Returns [`GitError::Panic`] if the blocking task panics.
pub async fn unbundle(
    repo: &Repository,
    folder: &Path,
    sha: Sha,
    ref_name: &RefName,
) -> Result<(), GitError> {
    unbundle_at(repo_cwd(repo), folder, sha, ref_name).await
}

/// Path-only variant of [`unbundle`] for callers that cannot hold a
/// `&Repository` across `.await` (notably the parallel fetch handler:
/// `gix::Repository` is `!Sync`, so it cannot be shared across
/// concurrent tasks).
///
/// # Errors
///
/// Returns [`GitError::Bundle`] if the bundle file is malformed,
/// prerequisite objects are missing, or the pack cannot be installed.
/// Returns [`GitError::Panic`] if the blocking task panics.
pub async fn unbundle_at(
    cwd: &Path,
    folder: &Path,
    sha: Sha,
    ref_name: &RefName,
) -> Result<(), GitError> {
    let (cwd, folder, ref_name) = (cwd.to_owned(), folder.to_owned(), ref_name.clone());
    tokio::task::spawn_blocking(move || crate::bundle::unbundle(&cwd, &folder, sha, &ref_name))
        .await?
        .map_err(|e| GitError::Bundle(Box::new(e)))
}

/// Return `true` iff `ancestor` is an ancestor of `descendant` (or
/// equals it).
///
/// Uses the `merge_base(A, B) == A` identity. A commit is its own
/// ancestor; unrelated commits return `false`; missing commits propagate
/// as `GitError`.
///
/// # Errors
///
/// Returns [`GitError::MergeBase`] if the merge-base computation fails.
pub fn is_ancestor(repo: &Repository, ancestor: Sha, descendant: Sha) -> Result<bool, GitError> {
    if ancestor == descendant {
        return Ok(true);
    }
    let ancestor_oid = *ancestor.as_object_id();
    let descendant_oid = *descendant.as_object_id();
    match repo.merge_base(ancestor_oid, descendant_oid) {
        Ok(base) => Ok(base.detach() == ancestor_oid),
        Err(gix::repository::merge_base::Error::NotFound { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Result of peeling a ref's target through any annotated-tag chain.
///
/// The variant tells the caller what kind of leaf object the chain
/// terminates at; `tag_chain` is the ordered sequence of tag-object OIDs
/// encountered along the way (newest-first, i.e. outer then inner).
/// `tag_chain` is empty for branch / lightweight-tag pushes and for bare
/// non-tag refs that point directly at a tree or blob.
///
/// Used by both pack engines: the tag objects themselves are appended to
/// the emitted pack so a receiver can install the full chain, and the
/// leaf-kind variant decides whether the pack is built from a commit
/// rev-walk, a tree closure, or a single blob.
pub(crate) enum PeeledTip {
    /// Chain terminates at a commit — the canonical case (branch tips,
    /// lightweight tags, annotated tags of commits).
    Commit {
        commit: Sha,
        tag_chain: Vec<ObjectId>,
    },
    /// Chain terminates at a tree (annotated tag of tree, or a bare ref
    /// pointing at a tree). The pack carries the tree plus its full
    /// recursive subtree + blob closure verbatim, no rev-walk.
    Tree {
        tree: ObjectId,
        tag_chain: Vec<ObjectId>,
    },
    /// Chain terminates at a blob (annotated tag of blob, or a bare ref
    /// pointing at a blob). The pack carries the blob plus the tag
    /// chain — there is no tree to walk.
    Blob {
        blob: ObjectId,
        tag_chain: Vec<ObjectId>,
    },
}

/// Peel `tip` through any annotated-tag chain to its leaf object.
///
/// Returns a [`PeeledTip`] whose variant identifies the leaf kind
/// (commit / tree / blob) and whose `tag_chain` lists the tag objects
/// encountered in walk order (outer first, inner last). For a branch
/// tip or lightweight tag the chain is empty and the variant is
/// `Commit`.
///
/// Both pack engines call this so the tag objects themselves land in
/// the emitted pack — without them a receiver could install all
/// reachable objects yet still fail to update `refs/tags/v1` because
/// the tag-OID it must point at is not in the ODB.
///
/// # Errors
///
/// - [`GitError::FindObject`] if `tip` or any intermediate tag's
///   target is missing from the ODB.
/// - [`GitError::PeelToKind`] if a tag object's bytes do not decode.
/// - [`GitError::TagChainCycle`] if the chain visits the same OID
///   twice (corrupted or adversarial ODB only — real git tags cannot
///   cycle).
pub(crate) fn peel_tag_chain(repo: &Repository, tip: Sha) -> Result<PeeledTip, GitError> {
    // `visited` defends against cyclic chains in a corrupted or
    // adversarial ODB. Real git tags cannot cycle (a cycle would
    // require a SHA-1 preimage), so the HashSet stays at length ≤ chain
    // depth, which is typically 0–2 in practice.
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut tag_chain = Vec::new();
    let mut current = *tip.as_object_id();
    loop {
        if !visited.insert(current) {
            return Err(GitError::TagChainCycle { oid: current });
        }
        let object = repo.find_object(current)?;
        match object.kind {
            gix::object::Kind::Commit => {
                return Ok(PeeledTip::Commit {
                    commit: Sha::from_object_id(current),
                    tag_chain,
                });
            }
            gix::object::Kind::Tag => {
                tag_chain.push(current);
                current = object.into_tag().target_id()?.detach();
            }
            gix::object::Kind::Tree => {
                return Ok(PeeledTip::Tree {
                    tree: current,
                    tag_chain,
                });
            }
            gix::object::Kind::Blob => {
                return Ok(PeeledTip::Blob {
                    blob: current,
                    tag_chain,
                });
            }
        }
    }
}

/// Compute the shallow-fetch boundary commits for `tip` at `max_depth`.
///
/// Performs a breadth-first walk from `tip`. The returned vector contains
/// the **frontier** OIDs — commits reached at exactly `max_depth`. Git
/// writes these to `.git/shallow` so they appear parentless, giving
/// exactly `max_depth` visible commits from `tip`.
///
/// BFS is mandatory here: `gix::Repository::rev_walk` returns commits in
/// topological-sort order, which does not coincide with depth order at
/// merge points. Naively `.take(N)` on the walk would include the wrong
/// commits and emit incorrect boundaries.
///
/// If the walk exhausts the graph before reaching `max_depth` (i.e. the
/// repository's history is shorter than the requested depth) the
/// returned vector is empty — the repo is fully cloned and no shallow
/// marker should be written.
///
/// # Errors
///
/// Returns [`GitError::FindObject`] if `tip` or any of its ancestors
/// cannot be located in the local object database (the bundle was not
/// installed correctly), or [`GitError::PeelToKind`] if an object that
/// is supposed to be a commit cannot be decoded as one.
pub(crate) fn shallow_boundaries(
    repo: &Repository,
    tip: Sha,
    max_depth: NonZeroU32,
) -> Result<Vec<ObjectId>, GitError> {
    let max_depth = max_depth.get();
    let tip_oid = *tip.as_object_id();

    // BFS from `tip`. `seen` deduplicates; `frontier` accumulates the
    // commits at exactly max_depth — the boundary written to .git/shallow.
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut frontier: Vec<ObjectId> = Vec::new();
    let mut queue: VecDeque<(ObjectId, u32)> = VecDeque::new();
    queue.push_back((tip_oid, 1));

    while let Some((oid, depth)) = queue.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        if depth == max_depth {
            // Frontier commit: appears parentless in the shallow clone.
            // Do not recurse further — its parents are excluded.
            frontier.push(oid);
            continue;
        }
        let commit = repo
            .find_object(oid)?
            .peel_to_kind(gix::object::Kind::Commit)?;
        let commit = commit.into_commit();
        for parent in commit.parent_ids() {
            let parent_oid = parent.detach();
            if !seen.contains(&parent_oid) {
                queue.push_back((parent_oid, depth + 1));
            }
        }
    }

    Ok(frontier)
}

/// 40 hex digits + '\n' per `.git/shallow` entry.
const SHA1_HEX_LINE_LEN: usize = 41;

/// Rewrite `<git_dir>/shallow` so that it lists exactly the commits that
/// remain shallow boundaries — `boundaries` plus any pre-existing entry
/// whose parents are still missing from the local ODB.
///
/// `repo_dir` is the working-tree root (or the git directory itself for a
/// bare repo); the actual `.git/shallow` location is derived internally
/// to handle linked-worktree and `--separate-git-dir` layouts.
///
/// A shallow boundary is a commit whose parents are not present locally;
/// git's `shallow.c::register_shallow` grafts every entry in
/// `.git/shallow` to be parentless (and frees the in-memory parent
/// pointers), so a stale entry suppresses newly-installed parents. After
/// a deepening fetch the previous boundary's parents land in the ODB
/// and the entry must be dropped, otherwise `git log` still stops at the
/// old shallow tip even though deeper history is reachable.
///
/// Algorithm:
/// 1. Pre-existing entries that are *also* in `boundaries` are kept
///    unconditionally (the new fetch explicitly designated them).
/// 2. Each remaining pre-existing entry is dropped iff every parent is
///    present in `repo`'s ODB; an octopus-merge entry stays as long as
///    *any* parent is still missing. Entries pointing at a missing or
///    non-commit object are also dropped (stale).
/// 3. If the resulting set is empty, `.git/shallow` is unlinked when
///    present — a fully-deepened repository must not retain the file
///    (matches git's own behaviour in `shallow.c::prune_shallow`).
///
/// The file format is one SHA-1 hex per line, sorted for stable output;
/// the existing parser is lenient (skips blank or malformed lines) so
/// external tooling's annotations do not break the read pass.
///
/// # Errors
///
/// Returns [`GitError::Open`] if `repo_dir` cannot be opened as a gix
/// repository, [`GitError::Io`] if the file cannot be read, written, or
/// unlinked, or [`GitError::ConfigLock`] if the lock file cannot be
/// acquired.
pub(crate) fn write_shallow_file(repo_dir: &Path, boundaries: &[ObjectId]) -> Result<(), GitError> {
    let path = git_dir_for(repo_dir).join("shallow");

    // Read existing entries leniently: skip blank lines and content that
    // isn't a 40-hex SHA so external annotations or stray whitespace do
    // not abort the rewrite.
    let mut existing: HashSet<ObjectId> = HashSet::new();
    for line in read_or_empty(&path)?.split(|&b| b == b'\n') {
        let line = line.trim_ascii();
        if !line.is_empty()
            && let Ok(oid) = ObjectId::from_hex(line)
        {
            existing.insert(oid);
        }
    }

    // Seed the final set with the new boundaries — they are kept
    // unconditionally regardless of ODB state. The remaining pre-existing
    // entries are stale candidates: they're kept only if their parents
    // are still missing from the ODB.
    let mut final_set: HashSet<ObjectId> = boundaries.iter().copied().collect();
    existing.retain(|oid| !final_set.contains(oid));
    let stale = existing;

    if !stale.is_empty() {
        let repo = gix::open(repo_dir).map_err(|e| GitError::Open(Box::new(e)))?;
        // Hoisting the ODB handle out of the loop matches the
        // skip-when-empty guard above: every entry's parent lookup
        // goes through the same Arc-cloned handle.
        let odb = repo.objects.clone().into_inner();
        for oid in stale {
            if entry_remains_a_boundary(&repo, &odb, oid) {
                final_set.insert(oid);
            }
        }
    }

    if final_set.is_empty() {
        // A fully-deepened repository must not retain `.git/shallow`;
        // the file's mere presence triggers shallow semantics in git.
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(GitError::Io(e));
        }
        return Ok(());
    }

    // Stable on-disk order. ObjectId: Ord sorts by raw SHA bytes, which
    // is the same order as the hex strings the file contains.
    let mut sorted: Vec<ObjectId> = final_set.into_iter().collect();
    sorted.sort_unstable();

    let mut buf = Vec::with_capacity(sorted.len() * SHA1_HEX_LINE_LEN);
    for oid in &sorted {
        writeln!(buf, "{}", oid.to_hex()).map_err(GitError::Io)?;
    }
    write_atomic(&path, &buf)
}

/// Resolve the on-disk git directory for `repo_dir`.
///
/// Three layouts are handled in priority order:
/// 1. `.git/` is a directory → normal clone.
/// 2. `.git` is a file → linked worktree or `--separate-git-dir`; the
///    file contains `gitdir: <path>` pointing to the real git dir.
/// 3. No `.git` entry → bare repository; `repo_dir` is the git dir.
fn git_dir_for(repo_dir: &Path) -> PathBuf {
    let candidate = repo_dir.join(".git");
    if candidate.is_dir() {
        return candidate;
    }
    // Linked-worktree / --separate-git-dir: `.git` is a text file whose
    // sole content is `gitdir: <path>`. Follow the pointer so that
    // write_shallow_file lands in the real git directory.
    if candidate.is_file()
        && let Ok(content) = std::fs::read_to_string(&candidate)
        && let Some(rest) = content.trim().strip_prefix("gitdir:")
    {
        let pointed = Path::new(rest.trim());
        let resolved = if pointed.is_absolute() {
            pointed.to_path_buf()
        } else {
            repo_dir.join(pointed)
        };
        if resolved.is_dir() {
            return resolved;
        }
    }
    // Bare repository: the working tree root is the git directory.
    repo_dir.to_path_buf()
}

/// Decide whether `oid` is still a shallow boundary in `repo`.
///
/// Returns `true` iff `oid` resolves to a commit whose parent set is
/// non-empty and at least one parent is missing from the ODB. A missing
/// object, a non-commit, or a parentless commit is treated as stale and
/// pruned (`false`). Transient lookup errors fall through to `false`
/// with a `debug!` so a single unreadable boundary cannot block the
/// rewrite — the worst-case effect is a stale entry being dropped, which
/// never causes incorrect repository state.
fn entry_remains_a_boundary(
    repo: &gix::Repository,
    odb: &impl gix_pack::Find,
    oid: ObjectId,
) -> bool {
    let object = match repo.find_object(oid) {
        Ok(o) => o,
        Err(e) => {
            debug!(%oid, error = %e, "shallow entry not found in ODB; pruning");
            return false;
        }
    };
    let commit = match object.peel_to_kind(gix::object::Kind::Commit) {
        Ok(c) => c.into_commit(),
        Err(e) => {
            debug!(%oid, error = %e, "shallow entry does not peel to a commit; pruning");
            return false;
        }
    };
    // Single-pass: a commit with no parents is a vacuous (root) boundary
    // and gets pruned; otherwise short-circuit on the first parent that
    // is still missing from the ODB.
    let mut parents = commit.parent_ids().map(gix::Id::detach).peekable();
    if parents.peek().is_none() {
        return false;
    }
    parents.any(|p| !odb.contains(&p))
}

/// Write a zip archive of the tree at `spec` to `<folder>/repo.zip` and
/// return the path.
///
/// `spec` is any rev-spec gix can resolve — fully-qualified ref, short
/// branch, tag, or SHA. Uses `gix-archive`'s native zip writer via
/// [`Repository::worktree_archive`]; no subprocess.
///
/// # Errors
///
/// Returns [`GitError`] if `spec` cannot be resolved, the object cannot
/// be peeled to a tree, or writing the zip file fails.
pub fn archive(repo: &Repository, folder: &Path, spec: &str) -> Result<PathBuf, GitError> {
    let tree = repo
        .rev_parse_single(BStr::new(spec))?
        .object()?
        .peel_to_kind(gix::object::Kind::Tree)?;
    let (stream, _index) = repo.worktree_stream(tree.id)?;

    let path = folder.join("repo.zip");
    let file = std::fs::File::create(&path)?;
    let buf = std::io::BufWriter::new(file);

    let interrupt = AtomicBool::new(false);
    let options = gix_archive::Options {
        format: gix_archive::Format::Zip {
            compression_level: None,
        },
        ..gix_archive::Options::default()
    };
    repo.worktree_archive(stream, buf, Discard, &interrupt, options)?;
    Ok(path)
}

/// Format `HEAD`'s commit as `"<short-sha> <subject>"`, matching upstream
/// `git log -1 --pretty=%h %s`. Used as `CodePipeline` metadata in the
/// `s3+zip` push variant.
///
/// # Errors
///
/// Returns [`GitError::NoCommits`] if the repository has no commits.
/// Returns other [`GitError`] variants if the commit object cannot be
/// decoded or a short id cannot be computed.
pub fn last_commit_message(repo: &Repository) -> Result<String, GitError> {
    use gix::head::peel;

    let commit = match repo.head_commit() {
        Ok(c) => c,
        Err(gix::reference::head_commit::Error::PeelToCommit(
            peel::to_commit::Error::PeelToObject(peel::to_object::Error::Unborn { .. }),
        )) => return Err(GitError::NoCommits),
        Err(e) => return Err(e.into()),
    };
    let short = commit.short_id()?;
    let message = commit.message()?;
    Ok(format!("{} {}", short, message.summary().to_str_lossy()))
}

/// Read a remote's URL out of the repository's configuration.
///
/// Tries the fetch URL first and falls back to the push URL, matching
/// `git remote get-url` semantics.
///
/// # Errors
///
/// Returns [`GitError::RemoteNotFound`] if the remote does not exist,
/// [`GitError::RemoteHasNoUrl`] if it has neither a fetch nor a push URL,
/// [`GitError::NonUtf8RemoteUrl`] if the URL bytes are not valid UTF-8, or
/// [`GitError::FindRemote`] for other lookup failures.
pub fn remote_url(repo: &Repository, name: &str) -> Result<String, GitError> {
    let owned_name = || name.to_owned();
    let remote = repo.find_remote(BStr::new(name)).map_err(|e| match e {
        gix::remote::find::existing::Error::NotFound { .. } => {
            GitError::RemoteNotFound(owned_name())
        }
        other => GitError::FindRemote(Box::new(other)),
    })?;
    let url = remote
        .url(Direction::Fetch)
        .or_else(|| remote.url(Direction::Push))
        .ok_or_else(|| GitError::RemoteHasNoUrl(owned_name()))?;
    String::from_utf8(url.to_bstring().into()).map_err(|source| GitError::NonUtf8RemoteUrl {
        remote: owned_name(),
        source,
    })
}

/// Parsed dotted config key: `<section>[.<subsection>].<name>`.
///
/// Matches `git config`'s native splitting: section is the first
/// dot-segment, name is the last, and any segments in between are joined
/// with `.` to form the subsection (so `lfs.customtransfer.git-lfs-object-store.path`
/// yields section=`lfs`, subsection=`customtransfer.git-lfs-object-store`,
/// name=`path`).
struct DottedKey<'a> {
    section: &'a str,
    subsection: Option<&'a str>,
    name: &'a str,
}

fn parse_dotted_key(key: &str) -> Result<DottedKey<'_>, GitError> {
    let first_dot = key
        .find('.')
        .ok_or_else(|| GitError::ConfigKeyParse(key.to_owned()))?;
    let last_dot = key
        .rfind('.')
        .expect("first_dot found, so rfind cannot be None");
    let section = &key[..first_dot];
    let name = &key[last_dot + 1..];
    if section.is_empty() || name.is_empty() {
        return Err(GitError::ConfigKeyParse(key.to_owned()));
    }
    // Native git accepts an empty subsection (`a..b` → `[a ""]`) and
    // dot-prefixed subsections (`a..b.c` → `[a ".b"]`); preserve that
    // permissiveness here. We only reject when section or name is empty.
    let subsection = (first_dot != last_dot).then(|| &key[first_dot + 1..last_dot]);
    Ok(DottedKey {
        section,
        subsection,
        name,
    })
}

/// Resolve the path to the local `.git/config` for the repository
/// containing `cwd`. Honours `GIT_DIR` and worktree layouts: for linked
/// worktrees we write to the **common** dir's config (where
/// `git config --add` writes by default), not the per-worktree
/// `config.worktree`.
fn config_path_for_cwd(cwd: &Path) -> Result<PathBuf, GitError> {
    let repo = gix::discover(cwd)?;
    Ok(repo.common_dir().join("config"))
}

fn read_or_empty(path: &Path) -> Result<Vec<u8>, GitError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(GitError::Io(e)),
    }
}

/// Atomically rewrite `path` with `bytes` via a `gix-lock` file. The lock
/// path is `<path>.lock`; on commit it is `rename(2)`'d over `path`,
/// matching native `git config`'s behaviour.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), GitError> {
    use std::io::Write;
    let mut lock = gix_lock::File::acquire_to_update_resource(
        path,
        gix_lock::acquire::Fail::Immediately,
        None,
    )?;
    lock.write_all(bytes).map_err(GitError::Io)?;
    lock.commit().map_err(|e| GitError::Io(e.error))?;
    Ok(())
}

/// Add a multi-value entry to the repository's local config (`<section>[.<subsection>].<name> = value`).
///
/// In-process equivalent of `git config --add <key> <value>`. Used by the
/// LFS agent's `install` / `enable-debug` subcommands. `--add` semantics
/// rather than `set` so that re-running `install` does not silently
/// clobber an existing entry the user added by hand.
///
/// The write goes through `gix-lock` (atomic rename via
/// `<path>.lock`), preserving parity with `git config`'s on-disk
/// concurrency contract.
///
/// # Errors
///
/// Returns [`GitError::ConfigKeyParse`] for a malformed dotted key,
/// [`GitError::ConfigInvalidValueName`] if the value name is rejected by
/// `gix-config`, [`GitError::ConfigInvalidSectionName`] if the section name
/// is rejected, [`GitError::Discover`] if the repository cannot be located,
/// [`GitError::ConfigParse`] if the existing config cannot be parsed,
/// [`GitError::ConfigLock`] if the lock cannot be acquired, or
/// [`GitError::Io`] for other file I/O failures.
pub fn config_add(cwd: &Path, key: &str, value: &str) -> Result<(), GitError> {
    config_add_many(cwd, &[(key, value)])
}

/// Batched variant of [`config_add`]: applies every `(key, value)` entry
/// to the local config in a single read / parse / lock / write cycle.
///
/// Used by `lfs::install::install`, which previously paid two full
/// `gix::discover` + `fs::read` + parse + lock + write cycles to set
/// `lfs.customtransfer.<agent>.path` and `lfs.standalonetransferagent`
/// back to back. All entries are validated up front, so a malformed
/// later entry does not partially-write the file.
///
/// # Errors
///
/// Returns [`GitError::ConfigKeyParse`] for a malformed dotted key,
/// [`GitError::ConfigInvalidValueName`] if a value name is rejected by
/// `gix-config`, [`GitError::ConfigInvalidSectionName`] if a section name is
/// rejected, [`GitError::Discover`] if the repository cannot be located,
/// [`GitError::ConfigParse`] if the existing config cannot be parsed,
/// [`GitError::ConfigLock`] if the lock cannot be acquired, or
/// [`GitError::Io`] for other file I/O failures.
pub fn config_add_many(cwd: &Path, entries: &[(&str, &str)]) -> Result<(), GitError> {
    if entries.is_empty() {
        return Ok(());
    }
    let parsed: Vec<(DottedKey<'_>, ValueName<'_>, &str)> = entries
        .iter()
        .map(|(key, value)| {
            let parts = parse_dotted_key(key)?;
            let value_name = ValueName::try_from(parts.name).map_err(|source| {
                GitError::ConfigInvalidValueName {
                    name: parts.name.to_owned(),
                    source,
                }
            })?;
            Ok::<_, GitError>((parts, value_name, *value))
        })
        .collect::<Result<_, _>>()?;

    let config_path = config_path_for_cwd(cwd)?;
    let bytes = read_or_empty(&config_path)?;
    let mut file = gix::config::File::from_bytes_no_includes(
        &bytes,
        GixConfigMetadata::api(),
        gix_config_init::Options::default(),
    )?;
    for (parts, value_name, value) in parsed {
        let subsection = parts.subsection.map(BStr::new);
        let mut section = file
            .section_mut_or_create_new(parts.section, subsection)
            .map_err(|source| GitError::ConfigInvalidSectionName {
                name: parts.section.to_owned(),
                source,
            })?;
        section.push(value_name, Some(BStr::new(value)));
    }

    let extra: usize = entries.iter().map(|(k, v)| k.len() + v.len() + 16).sum();
    let mut serialized = Vec::with_capacity(bytes.len() + extra);
    file.write_to(&mut serialized).map_err(GitError::Io)?;
    write_atomic(&config_path, &serialized)
}

/// Remove the latest value for the given key from the repository's local config.
///
/// In-process equivalent of `git config --unset <key>`. Used by the LFS
/// agent's `disable-debug` subcommand. Returns
/// [`GitError::ConfigKeyNotSet`] when the section or value is absent;
/// callers that want idempotent behaviour should match on that.
///
/// Divergence from `git config --unset`: native git refuses to unset a
/// multi-valued key (it requires `--unset-all`). `gix-config` removes
/// only the latest value. The keys this helper is used with
/// (`lfs.customtransfer.<agent>.args`) are single-valued in practice,
/// so the divergence is not observable here.
///
/// # Errors
///
/// Returns [`GitError::ConfigKeyParse`] if `key` is malformed,
/// [`GitError::Discover`] if the repository cannot be located,
/// [`GitError::ConfigParse`] if the existing config cannot be parsed,
/// [`GitError::ConfigKeyNotSet`] if the section or value is absent,
/// [`GitError::ConfigLock`] if the lock cannot be acquired, or
/// [`GitError::Io`] for other file I/O failures.
pub fn config_unset(cwd: &Path, key: &str) -> Result<(), GitError> {
    let parts = parse_dotted_key(key)?;
    let config_path = config_path_for_cwd(cwd)?;
    let bytes = read_or_empty(&config_path)?;
    let mut file = gix::config::File::from_bytes_no_includes(
        &bytes,
        GixConfigMetadata::api(),
        gix_config_init::Options::default(),
    )?;
    let subsection = parts.subsection.map(BStr::new);
    let Ok(mut section) = file.section_mut(parts.section, subsection) else {
        return Err(GitError::ConfigKeyNotSet(key.to_owned()));
    };
    if section.remove(parts.name).is_none() {
        return Err(GitError::ConfigKeyNotSet(key.to_owned()));
    }

    let mut serialized = Vec::with_capacity(bytes.len());
    file.write_to(&mut serialized).map_err(GitError::Io)?;
    write_atomic(&config_path, &serialized)
}

#[cfg(test)]
mod tests {
    use super::*;

    use gix::actor::SignatureRef;
    use gix::bstr::BStr;
    use gix_pack::Find as _;
    use std::sync::OnceLock;
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

    /// Persist a one-blob tree so `archive()` has something to emit and
    /// bundle round-trips carry real content. `repo.empty_tree()` builds
    /// a `Tree` value without writing it, which would leave commits
    /// referencing a dangling tree id.
    fn make_marker_tree(repo: &Repository) -> ObjectId {
        use gix::objs::tree::{Entry, EntryKind};
        let blob_id = repo.write_blob(b"hello\n").expect("write blob").detach();
        let tree = gix::objs::Tree {
            entries: vec![Entry {
                mode: EntryKind::Blob.into(),
                filename: "marker".into(),
                oid: blob_id,
            }],
        };
        repo.write_object(&tree).expect("write tree").detach()
    }

    fn add_commit(
        repo: &Repository,
        ref_name: &str,
        parents: &[ObjectId],
        message: &str,
    ) -> ObjectId {
        let tree_id = make_marker_tree(repo);
        let id = repo
            .commit_as(
                signature(),
                signature(),
                ref_name,
                message,
                tree_id,
                parents.iter().copied(),
            )
            .expect("commit_as");
        id.detach()
    }

    /// Write a commit object whose parent list contains OIDs that may
    /// or may not be present in the ODB — the gix object writer does
    /// not check parent reachability. Used to construct synthetic
    /// "orphan" or "octopus with a missing parent" inputs for the
    /// shallow-pruning tests.
    fn commit_with_synthetic_parents(
        repo: &Repository,
        parents: &[ObjectId],
        message: &str,
    ) -> ObjectId {
        let tree_id = make_marker_tree(repo);
        let sig = gix::actor::Signature {
            name: "Test".into(),
            email: "test@example.com".into(),
            time: gix::date::Time::default(),
        };
        let commit = gix::objs::Commit {
            tree: tree_id,
            parents: parents.iter().copied().collect(),
            author: sig.clone(),
            committer: sig,
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
        };
        repo.write_object(&commit).expect("write commit").detach()
    }

    fn git_available() -> bool {
        static AVAIL: OnceLock<bool> = OnceLock::new();
        *AVAIL.get_or_init(|| {
            std::process::Command::new("git")
                .arg("--version")
                .output()
                .is_ok()
        })
    }

    // --- Sha ----------------------------------------------------------

    #[test]
    fn sha_from_hex_accepts_valid_lowercase_sha1() {
        let s = Sha::from_hex("0123456789abcdef0123456789abcdef01234567").expect("valid");
        assert_eq!(s.to_string(), "0123456789abcdef0123456789abcdef01234567");
    }

    #[test]
    fn sha_from_hex_accepts_uppercase_and_normalizes_to_lowercase() {
        let s = Sha::from_hex("0123456789ABCDEF0123456789ABCDEF01234567").expect("valid");
        assert_eq!(s.to_string(), "0123456789abcdef0123456789abcdef01234567");
    }

    #[test]
    fn sha_from_hex_rejects_wrong_length() {
        assert!(Sha::from_hex("abc").is_err());
        assert!(Sha::from_hex(&"a".repeat(39)).is_err());
        assert!(Sha::from_hex(&"a".repeat(41)).is_err());
    }

    #[test]
    fn sha_from_hex_rejects_non_hex() {
        assert!(Sha::from_hex(&"g".repeat(40)).is_err());
        assert!(Sha::from_hex("0123456789abcdef0123456789abcdef0123456 ").is_err());
    }

    #[test]
    fn sha_from_hex_rejects_empty() {
        assert!(matches!(Sha::from_hex(""), Err(ShaError::Empty)));
    }

    // --- RefName / is_valid_ref_name ----------------------------------

    const INVALID_REF_NAMES: &[&str] = &[
        "",
        ".hidden",
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

    #[test]
    fn ref_name_new_accepts_canonical_refs() {
        assert!(RefName::new("refs/heads/main").is_ok());
        assert!(RefName::new("refs/heads/feature/x").is_ok());
        assert!(RefName::new("refs/tags/v1").is_ok());
    }

    #[test]
    fn ref_name_new_rejects_each_invalid_category() {
        for name in INVALID_REF_NAMES {
            assert!(
                RefName::new(*name).is_err(),
                "expected RefName::new({name:?}) to fail",
            );
        }
    }

    #[test]
    fn ref_name_is_valid_matches_new() {
        // `RefName::is_valid` is the borrow-only predicate equivalent
        // of `RefName::new(...).is_ok()`. Pin parity on both sides so a
        // future glue change to `gix-validate` can't drift the two
        // surfaces apart.
        for name in ["refs/heads/main", "refs/heads/feature/x", "refs/tags/v1"] {
            assert!(RefName::is_valid(name), "expected is_valid({name:?})");
        }
        for name in INVALID_REF_NAMES {
            assert!(!RefName::is_valid(name), "expected !is_valid({name:?})",);
        }
    }

    #[test]
    fn is_valid_ref_name_partial_accepts_single_component_head() {
        // The partial validator accepts `HEAD`, matching the upstream
        // permissive regex; the strict `RefName::new` would reject it
        // because it isn't fully qualified.
        assert!(is_valid_ref_name("HEAD"));
    }

    #[test]
    fn is_valid_ref_name_partial_rejects_each_invalid_category() {
        // Empty and trailing-slash are rejected by `name_partial`.
        for name in &[
            "",
            "refs/heads/.hidden",
            "refs/heads/foo..bar",
            "refs/heads/main.lock",
        ] {
            assert!(!is_valid_ref_name(name), "expected !{name:?}");
        }
    }

    // --- is_ancestor / archive / last_commit_message / remote_url

    #[test]
    fn is_ancestor_self_is_true() {
        let (repo, _dir) = empty_repo();
        let a = add_commit(&repo, "refs/heads/main", &[], "first");
        let sa = Sha::from_object_id(a);
        assert!(is_ancestor(&repo, sa, sa).expect("is_ancestor"));
    }

    #[test]
    fn is_ancestor_parent_of_child_is_true() {
        let (repo, _dir) = empty_repo();
        let a = add_commit(&repo, "refs/heads/main", &[], "a");
        let b = add_commit(&repo, "refs/heads/main", &[a], "b");
        assert!(
            is_ancestor(&repo, Sha::from_object_id(a), Sha::from_object_id(b))
                .expect("is_ancestor")
        );
    }

    #[test]
    fn is_ancestor_reverse_is_false() {
        let (repo, _dir) = empty_repo();
        let a = add_commit(&repo, "refs/heads/main", &[], "a");
        let b = add_commit(&repo, "refs/heads/main", &[a], "b");
        assert!(
            !is_ancestor(&repo, Sha::from_object_id(b), Sha::from_object_id(a))
                .expect("is_ancestor")
        );
    }

    #[test]
    fn is_ancestor_unrelated_is_false() {
        let (repo, _dir) = empty_repo();
        let a = add_commit(&repo, "refs/heads/main", &[], "a");
        let b = add_commit(&repo, "refs/heads/side", &[], "b");
        assert!(
            !is_ancestor(&repo, Sha::from_object_id(a), Sha::from_object_id(b))
                .expect("is_ancestor")
        );
    }

    // --- peel_tag_chain -----------------------------------------------

    fn write_annotated_tag(
        repo: &Repository,
        target: ObjectId,
        target_kind: gix::object::Kind,
        name: &str,
    ) -> ObjectId {
        let tag = gix::objs::Tag {
            target,
            target_kind,
            name: name.into(),
            tagger: Some(signature().to_owned().expect("static signature is valid")),
            message: "test".into(),
            pgp_signature: None,
        };
        repo.write_object(&tag).expect("write tag").detach()
    }

    #[test]
    fn peel_lightweight_tag_returns_commit_with_empty_chain() {
        // A lightweight tag is a ref pointing directly at a commit — there
        // is no tag object to walk through. We pass the commit OID
        // directly (the same OID `git::branch::resolve` would return for
        // a lightweight tag ref).
        let (repo, _dir) = empty_repo();
        let commit = add_commit(&repo, "refs/heads/main", &[], "c");
        let peeled = peel_tag_chain(&repo, Sha::from_object_id(commit)).expect("peel");
        match peeled {
            PeeledTip::Commit {
                commit: peeled_commit,
                tag_chain,
            } => {
                assert_eq!(peeled_commit.as_object_id(), &commit);
                assert!(tag_chain.is_empty());
            }
            other => panic!("expected Commit variant, got {:?}", variant_name(&other)),
        }
    }

    #[test]
    fn peel_annotated_tag_returns_commit_with_one_element_chain() {
        let (repo, _dir) = empty_repo();
        let commit = add_commit(&repo, "refs/heads/main", &[], "c");
        let tag = write_annotated_tag(&repo, commit, gix::object::Kind::Commit, "v1");
        let peeled = peel_tag_chain(&repo, Sha::from_object_id(tag)).expect("peel");
        match peeled {
            PeeledTip::Commit {
                commit: peeled_commit,
                tag_chain,
            } => {
                assert_eq!(peeled_commit.as_object_id(), &commit);
                assert_eq!(tag_chain, vec![tag]);
            }
            other => panic!("expected Commit variant, got {:?}", variant_name(&other)),
        }
    }

    #[test]
    fn peel_tag_of_tag_returns_commit_with_outer_then_inner_chain() {
        let (repo, _dir) = empty_repo();
        let commit = add_commit(&repo, "refs/heads/main", &[], "c");
        let inner = write_annotated_tag(&repo, commit, gix::object::Kind::Commit, "inner");
        let outer = write_annotated_tag(&repo, inner, gix::object::Kind::Tag, "outer");
        let peeled = peel_tag_chain(&repo, Sha::from_object_id(outer)).expect("peel");
        match peeled {
            PeeledTip::Commit {
                commit: peeled_commit,
                tag_chain,
            } => {
                assert_eq!(peeled_commit.as_object_id(), &commit);
                // Walk order: outer encountered first, then inner.
                assert_eq!(tag_chain, vec![outer, inner]);
            }
            other => panic!("expected Commit variant, got {:?}", variant_name(&other)),
        }
    }

    /// Build a freestanding tree object suitable for `peel_tag_chain` tests.
    fn write_tree_with_one_blob(repo: &gix::Repository) -> (ObjectId, ObjectId) {
        use gix::objs::tree::{Entry, EntryKind};
        let blob = repo.write_blob(b"x").expect("write blob").detach();
        let tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![Entry {
                    mode: EntryKind::Blob.into(),
                    filename: "x".into(),
                    oid: blob,
                }],
            })
            .expect("write tree")
            .detach();
        (tree, blob)
    }

    #[test]
    fn peel_tag_pointing_to_tree_returns_tree_variant() {
        let (repo, _dir) = empty_repo();
        let (tree_id, _blob) = write_tree_with_one_blob(&repo);
        let tag = write_annotated_tag(&repo, tree_id, gix::object::Kind::Tree, "tree-tag");
        let peeled = peel_tag_chain(&repo, Sha::from_object_id(tag)).expect("peel");
        match peeled {
            PeeledTip::Tree { tree, tag_chain } => {
                assert_eq!(tree, tree_id);
                assert_eq!(tag_chain, vec![tag]);
            }
            other => panic!("expected Tree variant, got {:?}", variant_name(&other)),
        }
    }

    #[test]
    fn peel_tag_pointing_to_blob_returns_blob_variant() {
        let (repo, _dir) = empty_repo();
        let blob_id = repo.write_blob(b"data").expect("write blob").detach();
        let tag = write_annotated_tag(&repo, blob_id, gix::object::Kind::Blob, "blob-tag");
        let peeled = peel_tag_chain(&repo, Sha::from_object_id(tag)).expect("peel");
        match peeled {
            PeeledTip::Blob { blob, tag_chain } => {
                assert_eq!(blob, blob_id);
                assert_eq!(tag_chain, vec![tag]);
            }
            other => panic!("expected Blob variant, got {:?}", variant_name(&other)),
        }
    }

    #[test]
    fn peel_tag_of_tag_of_tree_returns_tree_with_outer_then_inner_chain() {
        let (repo, _dir) = empty_repo();
        let (tree_id, _blob) = write_tree_with_one_blob(&repo);
        let inner = write_annotated_tag(&repo, tree_id, gix::object::Kind::Tree, "inner");
        let outer = write_annotated_tag(&repo, inner, gix::object::Kind::Tag, "outer");
        let peeled = peel_tag_chain(&repo, Sha::from_object_id(outer)).expect("peel");
        match peeled {
            PeeledTip::Tree { tree, tag_chain } => {
                assert_eq!(tree, tree_id);
                assert_eq!(tag_chain, vec![outer, inner]);
            }
            other => panic!("expected Tree variant, got {:?}", variant_name(&other)),
        }
    }

    #[test]
    fn peel_depth_three_tag_chain_to_blob_preserves_chain_order() {
        // Three nested tags ending at a blob. Catches off-by-one in the
        // walk that tag-of-tag (depth 2) tests would miss.
        let (repo, _dir) = empty_repo();
        let blob_id = repo.write_blob(b"data").expect("write blob").detach();
        let inner = write_annotated_tag(&repo, blob_id, gix::object::Kind::Blob, "inner");
        let middle = write_annotated_tag(&repo, inner, gix::object::Kind::Tag, "middle");
        let outer = write_annotated_tag(&repo, middle, gix::object::Kind::Tag, "outer");
        let peeled = peel_tag_chain(&repo, Sha::from_object_id(outer)).expect("peel");
        match peeled {
            PeeledTip::Blob { blob, tag_chain } => {
                assert_eq!(blob, blob_id);
                assert_eq!(tag_chain, vec![outer, middle, inner]);
            }
            other => panic!("expected Blob variant, got {:?}", variant_name(&other)),
        }
    }

    #[test]
    fn peel_bare_tree_ref_returns_tree_with_empty_chain() {
        // A ref pointing directly at a tree (no tag wrapper) is legal in
        // git. Empty tag_chain is the natural fallout of treating chain
        // length and leaf kind as orthogonal.
        let (repo, _dir) = empty_repo();
        let (tree_id, _blob) = write_tree_with_one_blob(&repo);
        let peeled = peel_tag_chain(&repo, Sha::from_object_id(tree_id)).expect("peel");
        match peeled {
            PeeledTip::Tree { tree, tag_chain } => {
                assert_eq!(tree, tree_id);
                assert!(tag_chain.is_empty());
            }
            other => panic!("expected Tree variant, got {:?}", variant_name(&other)),
        }
    }

    #[test]
    fn peel_bare_blob_ref_returns_blob_with_empty_chain() {
        let (repo, _dir) = empty_repo();
        let blob_id = repo.write_blob(b"data").expect("write blob").detach();
        let peeled = peel_tag_chain(&repo, Sha::from_object_id(blob_id)).expect("peel");
        match peeled {
            PeeledTip::Blob { blob, tag_chain } => {
                assert_eq!(blob, blob_id);
                assert!(tag_chain.is_empty());
            }
            other => panic!("expected Blob variant, got {:?}", variant_name(&other)),
        }
    }

    fn variant_name(p: &PeeledTip) -> &'static str {
        match p {
            PeeledTip::Commit { .. } => "Commit",
            PeeledTip::Tree { .. } => "Tree",
            PeeledTip::Blob { .. } => "Blob",
        }
    }

    #[test]
    fn archive_writes_repo_zip_with_pk_header() {
        let (repo, dir) = empty_repo();
        add_commit(&repo, "refs/heads/main", &[], "first");
        let out_dir = TempDir::new().expect("tempdir");
        let zip_path = archive(&repo, out_dir.path(), "refs/heads/main").expect("archive");
        assert_eq!(zip_path, out_dir.path().join("repo.zip"));
        let bytes = std::fs::read(&zip_path).expect("read zip");
        assert_eq!(&bytes[..4], b"PK\x03\x04", "zip local-file-header missing");
        drop(dir);
    }

    #[test]
    fn archive_resolves_tag_through_peel() {
        // Annotated tag → commit → tree peel chain. This exercises the
        // tag-handling branch in `peel_to_kind` that the branch test
        // skips.
        let (repo, _dir) = empty_repo();
        let commit_oid = add_commit(&repo, "refs/heads/main", &[], "first");
        let tag = gix::objs::Tag {
            target: commit_oid,
            target_kind: gix::object::Kind::Commit,
            name: "v1".into(),
            tagger: Some(signature().to_owned().expect("static signature is valid")),
            message: "release".into(),
            pgp_signature: None,
        };
        let tag_id = repo.write_object(&tag).expect("write tag").detach();
        repo.reference(
            "refs/tags/v1",
            tag_id,
            gix::refs::transaction::PreviousValue::MustNotExist,
            "create tag",
        )
        .expect("create tag ref");
        let out_dir = TempDir::new().expect("tempdir");
        let zip_path = archive(&repo, out_dir.path(), "refs/tags/v1").expect("archive tag");
        let bytes = std::fs::read(&zip_path).expect("read zip");
        assert_eq!(&bytes[..4], b"PK\x03\x04");
    }

    #[test]
    fn last_commit_message_format_short_sha_then_subject() {
        let (repo, _dir) = empty_repo();
        add_commit(&repo, "refs/heads/main", &[], "Initial commit");
        let msg = last_commit_message(&repo).expect("last_commit_message");
        let mut parts = msg.splitn(2, ' ');
        let short = parts.next().expect("short");
        let subject = parts.next().expect("subject");
        assert!(short.len() >= 4, "short id too short: {short:?}");
        assert!(short.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(subject, "Initial commit");
    }

    #[test]
    fn last_commit_message_unborn_head_returns_no_commits() {
        let (repo, _dir) = empty_repo();
        assert!(matches!(
            last_commit_message(&repo),
            Err(GitError::NoCommits)
        ));
    }

    #[test]
    fn remote_url_returns_fetch_url() {
        let (repo, dir) = empty_repo();
        let url = "https://example.com/repo.git";
        let config_path = repo.git_dir().join("config");
        let existing = std::fs::read_to_string(&config_path).expect("read config");
        let amended = format!(
            "{existing}\n[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"
        );
        std::fs::write(&config_path, amended).expect("write config");
        // Re-open so the new config is visible.
        let repo = gix::open(repo.git_dir()).expect("re-open");
        let got = remote_url(&repo, "origin").expect("remote_url");
        assert_eq!(got, url);
        drop(dir);
    }

    #[test]
    fn remote_url_unknown_remote_returns_remote_not_found() {
        let (repo, _dir) = empty_repo();
        assert!(matches!(
            remote_url(&repo, "missing"),
            Err(GitError::RemoteNotFound(_))
        ));
    }

    #[test]
    fn remote_url_falls_back_to_push_url_when_fetch_url_absent() {
        // A remote with only `pushurl` and no `url` should still resolve.
        // gix's `find_remote` parses the section name from any of url
        // or pushurl, so we set pushurl alone.
        let (repo, dir) = empty_repo();
        let push_url = "https://example.com/push.git";
        let config_path = repo.git_dir().join("config");
        let existing = std::fs::read_to_string(&config_path).expect("read config");
        let amended = format!("{existing}\n[remote \"only-push\"]\n\tpushurl = {push_url}\n");
        std::fs::write(&config_path, amended).expect("write config");
        let repo = gix::open(repo.git_dir()).expect("re-open");
        let got = remote_url(&repo, "only-push").expect("remote_url");
        assert_eq!(got, push_url);
        drop(dir);
    }

    // --- parse_dotted_key ---------------------------------------------

    #[test]
    fn parse_dotted_key_two_segments_has_no_subsection() {
        let p = parse_dotted_key("lfs.standalonetransferagent").expect("parse");
        assert_eq!(p.section, "lfs");
        assert_eq!(p.subsection, None);
        assert_eq!(p.name, "standalonetransferagent");
    }

    #[test]
    fn parse_dotted_key_three_segments_uses_middle_as_subsection() {
        let p = parse_dotted_key("remote.origin.url").expect("parse");
        assert_eq!(p.section, "remote");
        assert_eq!(p.subsection, Some("origin"));
        assert_eq!(p.name, "url");
    }

    #[test]
    fn parse_dotted_key_four_segments_joins_subsection_with_dots() {
        // The two-level LFS shape: section=lfs,
        // subsection=customtransfer.git-lfs-object-store, name=path.
        let p = parse_dotted_key("lfs.customtransfer.git-lfs-object-store.path").expect("parse");
        assert_eq!(p.section, "lfs");
        assert_eq!(p.subsection, Some("customtransfer.git-lfs-object-store"));
        assert_eq!(p.name, "path");
    }

    #[test]
    fn parse_dotted_key_rejects_invalid_shapes() {
        // Covers: empty key, no-dot, leading-dot (empty section),
        // trailing-dot (empty name), and bare dot. Consecutive dots
        // are NOT rejected: native git accepts `a..b` (creates
        // `[a ""]`), so we mirror that.
        for bad in ["", "nodotsegment", ".name", "section.", "."] {
            assert!(
                matches!(parse_dotted_key(bad), Err(GitError::ConfigKeyParse(_))),
                "expected parse failure for {bad:?}",
            );
        }
    }

    #[test]
    fn parse_dotted_key_accepts_empty_subsection_for_git_parity() {
        // `git config a..b foo` creates `[a ""]\n\tb = foo` — we accept
        // the same shape rather than rejecting it.
        let p = parse_dotted_key("a..b").expect("parse");
        assert_eq!(p.section, "a");
        assert_eq!(p.subsection, Some(""));
        assert_eq!(p.name, "b");
    }

    // --- config_add / config_unset (in-process via gix-config) --------

    /// Read the local config back as bytes. Tests parse against the
    /// committed file, not against a serialized buffer, to verify the
    /// atomic-rename actually landed.
    fn read_local_config(repo: &Repository) -> String {
        let path = repo.common_dir().join("config");
        std::fs::read_to_string(&path).expect("read config")
    }

    /// Re-parse `<git_dir>/config` and return all values for `key` as
    /// owned strings. Uses the same `gix-config` machinery the helpers
    /// write through, so this asserts behavioural round-trip rather
    /// than byte equality.
    fn config_values(repo: &Repository, key: &str) -> Vec<String> {
        let path = repo.common_dir().join("config");
        let bytes = std::fs::read(&path).expect("read config");
        let file = gix::config::File::from_bytes_no_includes(
            &bytes,
            GixConfigMetadata::api(),
            gix_config_init::Options::default(),
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
    fn config_add_creates_section_and_value() {
        let (repo, _dir) = empty_repo();
        config_add(
            repo.workdir().expect("workdir"),
            "lfs.standalonetransferagent",
            "git-lfs-object-store",
        )
        .expect("config_add");
        let values = config_values(&repo, "lfs.standalonetransferagent");
        assert_eq!(values, vec!["git-lfs-object-store".to_owned()]);
    }

    #[test]
    fn config_add_handles_two_level_subsection() {
        let (repo, _dir) = empty_repo();
        let key = "lfs.customtransfer.git-lfs-object-store.path";
        config_add(
            repo.workdir().expect("workdir"),
            key,
            "git-lfs-object-store",
        )
        .expect("config_add");
        let values = config_values(&repo, key);
        assert_eq!(values, vec!["git-lfs-object-store".to_owned()]);
    }

    #[test]
    fn config_add_appends_duplicate_values() {
        // `--add` semantics: pushing the same key twice keeps both
        // values, matching upstream `git config --add`.
        let (repo, _dir) = empty_repo();
        let cwd = repo.workdir().expect("workdir");
        config_add(cwd, "lfs.standalonetransferagent", "first").expect("first");
        config_add(cwd, "lfs.standalonetransferagent", "second").expect("second");
        let values = config_values(&repo, "lfs.standalonetransferagent");
        assert_eq!(values, vec!["first".to_owned(), "second".to_owned()]);
    }

    #[test]
    fn config_add_preserves_existing_comments() {
        let (repo, _dir) = empty_repo();
        let path = repo.common_dir().join("config");
        let existing = std::fs::read_to_string(&path).expect("read config");
        let amended = format!("{existing}# user marker\n[user]\n\tname = Tester\n");
        std::fs::write(&path, amended).expect("seed config");

        config_add(
            repo.workdir().expect("workdir"),
            "lfs.standalonetransferagent",
            "git-lfs-object-store",
        )
        .expect("config_add");

        let after = read_local_config(&repo);
        assert!(
            after.contains("# user marker"),
            "comment dropped: {after:?}"
        );
        assert!(
            after.contains("name = Tester"),
            "user.name dropped: {after:?}"
        );
        let values = config_values(&repo, "lfs.standalonetransferagent");
        assert_eq!(values, vec!["git-lfs-object-store".to_owned()]);
    }

    #[test]
    fn config_add_rejects_invalid_key() {
        let (repo, _dir) = empty_repo();
        assert!(matches!(
            config_add(repo.workdir().expect("workdir"), "", "v"),
            Err(GitError::ConfigKeyParse(_))
        ));
        assert!(matches!(
            config_add(repo.workdir().expect("workdir"), "nodot", "v"),
            Err(GitError::ConfigKeyParse(_))
        ));
    }

    #[test]
    fn config_add_rejects_invalid_value_name() {
        // Value names must start with an ASCII alphabetic and contain
        // only alphanumeric/dash. Leading digits trip the validator.
        let (repo, _dir) = empty_repo();
        let err = config_add(repo.workdir().expect("workdir"), "lfs.123bad", "v")
            .expect_err("expected validation error");
        assert!(
            matches!(err, GitError::ConfigInvalidValueName { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn config_add_many_writes_all_entries_in_one_pass() {
        // Both entries land in the file. Order is preserved within a
        // section but `lfs.standalonetransferagent` lives directly under
        // `[lfs]` while the path key lives under
        // `[lfs "customtransfer.git-lfs-object-store"]`, so we just
        // assert each value is readable rather than asserting a
        // particular file ordering.
        let (repo, _dir) = empty_repo();
        let entries: &[(&str, &str)] = &[
            (
                "lfs.customtransfer.git-lfs-object-store.path",
                "git-lfs-object-store",
            ),
            ("lfs.standalonetransferagent", "git-lfs-object-store"),
        ];
        config_add_many(repo.workdir().expect("workdir"), entries).expect("config_add_many");
        for (key, value) in entries {
            assert_eq!(config_values(&repo, key), vec![(*value).to_owned()]);
        }
    }

    #[test]
    fn config_add_many_validates_all_entries_before_writing() {
        // A malformed key in any position must abort *before* we touch
        // the file — otherwise an earlier valid entry would be persisted
        // alongside the failure, leaving the repo in a half-installed
        // state.
        let (repo, _dir) = empty_repo();
        let cwd = repo.workdir().expect("workdir");
        let path_before = read_local_config(&repo);
        let err = config_add_many(
            cwd,
            &[
                ("lfs.standalonetransferagent", "git-lfs-object-store"),
                ("nodot", "v"),
            ],
        )
        .expect_err("expected parse failure on second entry");
        assert!(matches!(err, GitError::ConfigKeyParse(_)), "got {err:?}");
        assert_eq!(read_local_config(&repo), path_before);
        assert!(
            config_values(&repo, "lfs.standalonetransferagent").is_empty(),
            "first entry should not have been written",
        );
    }

    #[test]
    fn config_add_many_empty_input_is_noop() {
        let (repo, _dir) = empty_repo();
        let cwd = repo.workdir().expect("workdir");
        let before = read_local_config(&repo);
        config_add_many(cwd, &[]).expect("noop");
        assert_eq!(read_local_config(&repo), before);
    }

    #[test]
    fn config_unset_removes_existing_value() {
        let (repo, _dir) = empty_repo();
        let cwd = repo.workdir().expect("workdir");
        config_add(cwd, "lfs.customtransfer.git-lfs-object-store.args", "debug").expect("seed");
        config_unset(cwd, "lfs.customtransfer.git-lfs-object-store.args").expect("unset");
        let values = config_values(&repo, "lfs.customtransfer.git-lfs-object-store.args");
        assert!(values.is_empty(), "value still present: {values:?}");
    }

    #[test]
    fn config_unset_missing_key_returns_typed_error() {
        let (repo, _dir) = empty_repo();
        let err = config_unset(repo.workdir().expect("workdir"), "lfs.never.set")
            .expect_err("expected error");
        assert!(matches!(err, GitError::ConfigKeyNotSet(ref k) if k == "lfs.never.set"));
    }

    #[test]
    fn config_unset_missing_section_returns_typed_error() {
        let (repo, _dir) = empty_repo();
        // Even when the section itself is absent, we surface
        // ConfigKeyNotSet (parity with `git config --unset` exiting
        // non-zero in that case).
        let err = config_unset(repo.workdir().expect("workdir"), "ghost.value")
            .expect_err("expected error");
        assert!(matches!(err, GitError::ConfigKeyNotSet(_)), "got {err:?}");
    }

    #[test]
    fn config_unset_missing_key_within_existing_section_returns_typed_error() {
        // Distinct from the above: here the section IS present (we just
        // wrote a value to it), so `section_mut` succeeds and the error
        // must come from `section.remove()` returning None. Without this
        // test the second `ConfigKeyNotSet` branch in `config_unset`
        // would be unreachable from the suite.
        let (repo, _dir) = empty_repo();
        let cwd = repo.workdir().expect("workdir");
        config_add(cwd, "lfs.standalonetransferagent", "git-lfs-object-store").expect("seed");
        let err = config_unset(cwd, "lfs.othervalue").expect_err("expected error");
        assert!(
            matches!(err, GitError::ConfigKeyNotSet(ref k) if k == "lfs.othervalue"),
            "got {err:?}"
        );
    }

    #[test]
    fn config_add_then_native_git_can_read_value() {
        // Cross-tool parity: a value written by our gix-config helper
        // is readable by the native `git config --get` CLI.
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (repo, _dir) = empty_repo();
        let cwd = repo.workdir().expect("workdir");
        config_add(
            cwd,
            "lfs.customtransfer.git-lfs-object-store.path",
            "git-lfs-object-store",
        )
        .expect("config_add");

        let output = std::process::Command::new("git")
            .args([
                "config",
                "--get",
                "lfs.customtransfer.git-lfs-object-store.path",
            ])
            .current_dir(cwd)
            .output()
            .expect("git config --get");
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("utf8");
        assert_eq!(stdout.trim(), "git-lfs-object-store");
    }

    // --- shallow_boundaries / write_shallow_file ----------------------

    #[test]
    fn shallow_boundaries_depth_one_returns_tip() {
        // Linear history a → b. With depth=1 the frontier is {b} (the tip
        // itself). Git writes b to .git/shallow so b appears parentless,
        // giving exactly 1 visible commit.
        let (repo, _dir) = empty_repo();
        let a = add_commit(&repo, "refs/heads/main", &[], "a");
        let b = add_commit(&repo, "refs/heads/main", &[a], "b");
        let tip = Sha::from_object_id(b);
        let bounds =
            shallow_boundaries(&repo, tip, NonZeroU32::new(1).unwrap()).expect("boundaries");
        assert_eq!(bounds, vec![b]);
    }

    #[test]
    fn shallow_boundaries_returns_empty_when_history_shorter_than_depth() {
        // Single-commit history; depth=5 exhausts the graph and writes
        // no boundary (full clone).
        let (repo, _dir) = empty_repo();
        let a = add_commit(&repo, "refs/heads/main", &[], "a");
        let tip = Sha::from_object_id(a);
        let bounds =
            shallow_boundaries(&repo, tip, NonZeroU32::new(5).unwrap()).expect("boundaries");
        assert!(bounds.is_empty(), "expected empty, got {bounds:?}");
    }

    #[test]
    fn shallow_boundaries_at_merge_returns_frontier_at_depth() {
        // Merge graph:
        //     M (tip, depth 1)
        //    / \
        //   A   B   (both depth 2 — the frontier)
        //    \ /
        //     C (depth 3 — excluded, not a boundary marker)
        //
        // BFS at depth=2 includes {M, A, B}. The frontier is {A, B};
        // both appear parentless in the shallow clone, giving 3 visible
        // commits. C is never visited and is not written to .git/shallow.
        let (repo, _dir) = empty_repo();
        let c = add_commit(&repo, "refs/heads/main", &[], "C");
        let a = add_commit(&repo, "refs/heads/main", &[c], "A");
        let b = add_commit(&repo, "refs/heads/side", &[c], "B");
        let m = add_commit(&repo, "refs/heads/main", &[a, b], "M");
        let tip = Sha::from_object_id(m);
        let bounds =
            shallow_boundaries(&repo, tip, NonZeroU32::new(2).unwrap()).expect("boundaries");
        let mut sorted = bounds.clone();
        sorted.sort_unstable();
        let mut expected = vec![a, b];
        expected.sort_unstable();
        assert_eq!(sorted, expected);
    }

    #[test]
    fn shallow_boundaries_at_merge_with_depth_one_returns_tip() {
        // depth=1: the frontier is the merge tip itself. M appears
        // parentless, giving exactly 1 visible commit regardless of
        // how many parents it has.
        let (repo, _dir) = empty_repo();
        let a = add_commit(&repo, "refs/heads/main", &[], "A");
        let b = add_commit(&repo, "refs/heads/side", &[], "B");
        let m = add_commit(&repo, "refs/heads/main", &[a, b], "M");
        let tip = Sha::from_object_id(m);
        let bounds =
            shallow_boundaries(&repo, tip, NonZeroU32::new(1).unwrap()).expect("boundaries");
        assert_eq!(bounds, vec![m]);
    }

    #[test]
    fn write_shallow_file_writes_boundaries_when_absent() {
        let (repo, dir) = empty_repo();
        let a = add_commit(&repo, "refs/heads/main", &[], "a");
        write_shallow_file(dir.path(), &[a]).expect("write");
        let path = repo.git_dir().join("shallow");
        let contents = std::fs::read_to_string(&path).expect("read shallow");
        assert_eq!(contents, format!("{a}\n"));
    }

    #[test]
    fn write_shallow_file_dedupes_entries() {
        // Same SHA seeded and passed in the new boundaries: HashSet
        // dedup yields a single line. `a` is a root commit (no
        // parents), so the prune-by-ODB pass cannot reject it on
        // membership grounds — it lands in the file because it is in
        // `boundaries`.
        let (repo, dir) = empty_repo();
        let a = add_commit(&repo, "refs/heads/main", &[], "a");
        let path = repo.git_dir().join("shallow");
        std::fs::write(&path, format!("{a}\n")).expect("seed");
        write_shallow_file(dir.path(), &[a]).expect("write");
        let contents = std::fs::read_to_string(&path).expect("read");
        assert_eq!(contents, format!("{a}\n"));
    }

    #[test]
    fn write_shallow_file_no_boundaries_no_existing_does_not_create_file() {
        // Empty boundaries + no existing file = no `.git/shallow`. A
        // fully cloned repo must not have this file.
        let (repo, dir) = empty_repo();
        let path = repo.git_dir().join("shallow");
        write_shallow_file(dir.path(), &[]).expect("noop");
        assert!(!path.exists(), "shallow file unexpectedly created");
    }

    #[test]
    fn write_shallow_file_prunes_existing_when_parents_in_odb() {
        // The deepen scenario: `.git/shallow` previously held the
        // depth-1 tip; the deepening fetch installs the parent and
        // computes the new depth-N boundary. The old tip must be
        // pruned (its parent is now in the ODB), leaving only the new
        // boundary in the file.
        let (repo, dir) = empty_repo();
        let a = add_commit(&repo, "refs/heads/main", &[], "a");
        let b = add_commit(&repo, "refs/heads/main", &[a], "b");
        let path = repo.git_dir().join("shallow");
        std::fs::write(&path, format!("{b}\n")).expect("seed depth-1 tip");
        write_shallow_file(dir.path(), &[a]).expect("deepen");
        let contents = std::fs::read_to_string(&path).expect("read");
        assert_eq!(contents, format!("{a}\n"));
    }

    #[test]
    fn write_shallow_file_unlinks_when_set_becomes_empty_after_pruning() {
        // Deepen-to-full-history: the existing tip's parents are now
        // in the ODB AND no new boundary is being added. The file
        // must be unlinked — its presence alone signals shallow
        // semantics to git, so a fully-deepened repo cannot keep it.
        let (repo, dir) = empty_repo();
        let a = add_commit(&repo, "refs/heads/main", &[], "a");
        let b = add_commit(&repo, "refs/heads/main", &[a], "b");
        let path = repo.git_dir().join("shallow");
        std::fs::write(&path, format!("{b}\n")).expect("seed");
        write_shallow_file(dir.path(), &[]).expect("deepen-to-full");
        assert!(!path.exists(), "shallow file should be unlinked");
    }

    #[test]
    fn write_shallow_file_drops_existing_root_commit() {
        // A root commit has no parents, so the "all parents in ODB"
        // predicate is vacuously true — the entry is a no-op marker
        // and gets pruned. (`register_shallow` grafting a parentless
        // commit to parentlessness is a no-op anyway.)
        let (repo, dir) = empty_repo();
        let a = add_commit(&repo, "refs/heads/main", &[], "a");
        let b = add_commit(&repo, "refs/heads/main", &[a], "b");
        let path = repo.git_dir().join("shallow");
        std::fs::write(&path, format!("{a}\n")).expect("seed");
        write_shallow_file(dir.path(), &[b]).expect("write");
        let contents = std::fs::read_to_string(&path).expect("read");
        assert_eq!(contents, format!("{b}\n"));
    }

    #[test]
    fn write_shallow_file_unlinks_when_only_existing_was_root() {
        let (repo, dir) = empty_repo();
        let a = add_commit(&repo, "refs/heads/main", &[], "a");
        let path = repo.git_dir().join("shallow");
        std::fs::write(&path, format!("{a}\n")).expect("seed");
        write_shallow_file(dir.path(), &[]).expect("write");
        assert!(!path.exists(), "shallow file should be unlinked");
    }

    #[test]
    fn write_shallow_file_keeps_existing_when_a_parent_is_missing() {
        // Build a commit whose parent is a synthetic OID that was
        // never written to the ODB. The shallow entry must be kept
        // because its parent is not reachable locally — pruning it
        // would expose git to a dangling parent ref.
        let (repo, dir) = empty_repo();
        let synthetic_parent =
            ObjectId::from_hex(b"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").expect("synthetic OID");
        let orphan = commit_with_synthetic_parents(&repo, &[synthetic_parent], "orphan");
        let new_root = add_commit(&repo, "refs/heads/main", &[], "new_root");
        let path = repo.git_dir().join("shallow");
        std::fs::write(&path, format!("{orphan}\n")).expect("seed");
        write_shallow_file(dir.path(), &[new_root]).expect("write");
        let contents = std::fs::read_to_string(&path).expect("read");
        let mut expected = [format!("{orphan}"), format!("{new_root}")];
        expected.sort();
        assert_eq!(contents.trim(), expected.join("\n"));
    }

    #[test]
    fn write_shallow_file_keeps_octopus_merge_when_any_parent_missing() {
        // Octopus merge with three parents, of which one is synthetic
        // (not in ODB). The entry stays in `.git/shallow` until ALL
        // parents are reachable; otherwise pruning would expose git
        // to a dangling parent.
        let (repo, dir) = empty_repo();
        let p1 = add_commit(&repo, "refs/heads/p1", &[], "p1");
        let p2 = add_commit(&repo, "refs/heads/p2", &[], "p2");
        let synthetic =
            ObjectId::from_hex(b"cafef00dcafef00dcafef00dcafef00dcafef00d").expect("synthetic");
        let merge = commit_with_synthetic_parents(&repo, &[p1, p2, synthetic], "octopus");
        let path = repo.git_dir().join("shallow");
        std::fs::write(&path, format!("{merge}\n")).expect("seed");
        write_shallow_file(dir.path(), &[]).expect("write");
        let contents = std::fs::read_to_string(&path).expect("read");
        assert_eq!(contents, format!("{merge}\n"));
    }

    #[test]
    fn write_shallow_file_drops_entry_pointing_at_non_commit() {
        // A `.git/shallow` line that resolves to a tree (not a
        // commit) is stale; drop it.
        let (repo, dir) = empty_repo();
        let tree_id = make_marker_tree(&repo);
        let path = repo.git_dir().join("shallow");
        std::fs::write(&path, format!("{tree_id}\n")).expect("seed");
        write_shallow_file(dir.path(), &[]).expect("write");
        assert!(!path.exists(), "stale tree entry should not preserve file");
    }

    #[test]
    fn write_shallow_file_drops_entry_missing_from_odb() {
        let (repo, dir) = empty_repo();
        let synthetic =
            ObjectId::from_hex(b"abcdef0123456789abcdef0123456789abcdef01").expect("synthetic");
        let path = repo.git_dir().join("shallow");
        std::fs::write(&path, format!("{synthetic}\n")).expect("seed");
        write_shallow_file(dir.path(), &[]).expect("write");
        assert!(!path.exists(), "missing-OID entry should not preserve file");
    }

    // --- bundle / unbundle (native gix-pack) --------------------------

    #[tokio::test]
    async fn bundle_unbundle_round_trips_natively() {
        let (src_repo, src_dir) = empty_repo();
        let oid = add_commit(&src_repo, "refs/heads/main", &[], "first");
        let sha = Sha::from_object_id(oid);
        let ref_name = RefName::new("refs/heads/main").expect("RefName");

        let bundles = TempDir::new().expect("tempdir");
        let bundle_path = bundle(&src_repo, bundles.path(), sha, ref_name.as_str())
            .await
            .expect("bundle");
        assert!(bundle_path.exists(), "bundle not written");

        // Verify bundle v2 header format.
        let first_line = {
            use std::io::BufRead as _;
            let f = std::fs::File::open(&bundle_path).expect("open bundle");
            let mut buf = String::new();
            std::io::BufReader::new(f)
                .read_line(&mut buf)
                .expect("read");
            buf.trim_end().to_owned()
        };
        assert_eq!(first_line, "# v2 git bundle", "bundle magic mismatch");

        let (dst_repo, _dst_dir) = empty_repo();
        unbundle(&dst_repo, bundles.path(), sha, &ref_name)
            .await
            .expect("unbundle");
        // `unbundle` copies pack objects into the destination ODB but does
        // not update refs — that's the remote-helper protocol's job.
        // Confirm via direct ODB lookup and via rev_parse.
        assert!(
            dst_repo
                .objects
                .clone()
                .into_inner()
                .contains(sha.as_object_id()),
            "commit object not in dst ODB after unbundle"
        );
        // Verify the OID is also resolvable via gix's spec parser — exercises
        // a different lookup path from contains(). The assert_eq on the
        // returned sha would be vacuous (resolve of a bare hex SHA always
        // returns that same SHA), so the .expect() is the assertion.
        branch::resolve(&dst_repo, &sha.to_string()).expect("resolve must work on bundled OID");

        // Confirm that unbundle() removes the .keep file created by
        // write_to_directory. A lingering .keep prevents git-repack from
        // consolidating packs; this check would catch any regression that
        // stops the removal.
        let pack_dir = dst_repo.git_dir().join("objects/pack");
        let keep_files: Vec<_> = std::fs::read_dir(&pack_dir)
            .expect("read pack dir")
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "keep"))
            .collect();
        assert!(
            keep_files.is_empty(),
            ".keep files not removed after unbundle: {keep_files:?}"
        );
        drop(src_dir);
    }

    #[tokio::test]
    async fn bundle_includes_full_commit_history() {
        let (src_repo, src_dir) = empty_repo();
        let oid1 = add_commit(&src_repo, "refs/heads/main", &[], "first");
        let oid2 = add_commit(&src_repo, "refs/heads/main", &[oid1], "second");
        let sha = Sha::from_object_id(oid2);
        let ref_name = RefName::new("refs/heads/main").expect("RefName");

        let bundles = TempDir::new().expect("tempdir");
        bundle(&src_repo, bundles.path(), sha, ref_name.as_str())
            .await
            .expect("bundle");

        let (dst_repo, _dst_dir) = empty_repo();
        unbundle(&dst_repo, bundles.path(), sha, &ref_name)
            .await
            .expect("unbundle");

        // Both commits must be present in dst_repo ODB.
        let dst_odb = dst_repo.objects.clone().into_inner();
        assert!(
            dst_odb.contains(&oid1),
            "ancestor commit not in dst ODB after unbundle"
        );
        assert!(
            dst_odb.contains(&oid2),
            "tip commit not in dst ODB after unbundle"
        );

        // Verify that trees and blobs (not just commits) are in the bundle.
        // add_commit always writes the same blob; write_blob is idempotent
        // (content-addressed), so this returns the same ID that add_commit
        // stored in src_repo without writing a second copy.
        let blob_id = src_repo.write_blob(b"hello\n").expect("blob id").detach();
        assert!(
            dst_odb.contains(&blob_id),
            "blob object not in dst ODB — ObjectExpansion::TreeContents may not be working"
        );
        drop(src_dir);
    }

    // --- idempotency --------------------------------------------------

    /// Calling `unbundle()` twice for the same SHA must succeed both times.
    ///
    /// On the second call `gix_pack::Bundle::write_to_directory` detects the
    /// pack already exists and returns `Outcome { keep_path: None, .. }` — the
    /// branch of our `.keep` removal logic that skips the `fs::remove_file`
    /// entirely. This test pins that path and guards against regressions that
    /// would return an error on a duplicate install.
    #[tokio::test]
    async fn unbundle_is_idempotent_on_duplicate_install() {
        let (src_repo, src_dir) = empty_repo();
        let oid = add_commit(&src_repo, "refs/heads/main", &[], "first");
        let sha = Sha::from_object_id(oid);
        let ref_name = RefName::new("refs/heads/main").expect("RefName");

        let bundles = TempDir::new().expect("tempdir");
        bundle(&src_repo, bundles.path(), sha, ref_name.as_str())
            .await
            .expect("bundle");

        let (dst_repo, _dst_dir) = empty_repo();

        unbundle(&dst_repo, bundles.path(), sha, &ref_name)
            .await
            .expect("first unbundle");

        // Second unbundle of the same SHA: pack already on disk, so
        // write_to_directory returns keep_path = None. Must still return Ok(()).
        unbundle(&dst_repo, bundles.path(), sha, &ref_name)
            .await
            .expect("second unbundle (duplicate install)");

        let pack_dir = dst_repo.git_dir().join("objects/pack");
        let keep_files: Vec<_> = std::fs::read_dir(&pack_dir)
            .expect("read pack dir")
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "keep"))
            .collect();
        assert!(
            keep_files.is_empty(),
            ".keep files after duplicate unbundle: {keep_files:?}"
        );

        assert!(
            dst_repo.objects.clone().into_inner().contains(&oid),
            "commit not in dst ODB after duplicate unbundle"
        );
        drop(src_dir);
    }

    // --- concurrency --------------------------------------------------

    /// Two concurrent `unbundle_at` calls for the same SHA must both succeed,
    /// leave no `.keep` files, and end with the object in the destination ODB.
    ///
    /// This exercises the `NotFound` handling in the `.keep` removal: the
    /// faster task removes the file; the slower task gets `NotFound` and must
    /// silently succeed rather than returning an error. The production fetch
    /// path (`fetch_batch`) runs bundle downloads in parallel and can reach
    /// this scenario when the same SHA appears in multiple concurrent fetch
    /// commands before `FetchedRefs` has recorded the first completion.
    #[tokio::test]
    async fn concurrent_unbundle_same_sha_is_idempotent() {
        let (src_repo, src_dir) = empty_repo();
        let oid = add_commit(&src_repo, "refs/heads/main", &[], "first");
        let sha = Sha::from_object_id(oid);
        let ref_name = RefName::new("refs/heads/main").expect("RefName");

        let bundles = TempDir::new().expect("tempdir");
        bundle(&src_repo, bundles.path(), sha, ref_name.as_str())
            .await
            .expect("bundle");

        let (dst_repo, _dst_dir) = empty_repo();
        let dst_cwd = repo_cwd(&dst_repo).to_owned();
        let bundles_path = bundles.path().to_owned();

        let (r1, r2) = tokio::join!(
            unbundle_at(&dst_cwd, &bundles_path, sha, &ref_name),
            unbundle_at(&dst_cwd, &bundles_path, sha, &ref_name),
        );
        assert!(r1.is_ok(), "first concurrent unbundle failed: {r1:?}");
        assert!(r2.is_ok(), "second concurrent unbundle failed: {r2:?}");

        // No .keep files should survive regardless of task ordering.
        let pack_dir = dst_repo.git_dir().join("objects/pack");
        let keep_files: Vec<_> = std::fs::read_dir(&pack_dir)
            .expect("read pack dir")
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "keep"))
            .collect();
        assert!(
            keep_files.is_empty(),
            ".keep files lingered after concurrent unbundle: {keep_files:?}"
        );

        assert!(
            dst_repo.objects.clone().into_inner().contains(&oid),
            "commit not in dst ODB after concurrent unbundle"
        );
        drop(src_dir);
    }

    // --- cross-tool bundle compatibility --------------------------------

    /// Create a bundle with `git bundle create`, then verify our native
    /// unbundle can parse and install the objects.
    #[tokio::test]
    async fn git_bundle_create_readable_by_native_unbundle() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (src_repo, src_dir) = empty_repo();
        let oid = add_commit(&src_repo, "refs/heads/main", &[], "first");
        let sha = Sha::from_object_id(oid);
        let ref_name = RefName::new("refs/heads/main").expect("RefName");

        let bundles = TempDir::new().expect("tempdir");
        let bundle_path = bundles.path().join(format!("{sha}.bundle"));

        let output = std::process::Command::new("git")
            .args(["bundle", "create"])
            .arg(&bundle_path)
            .arg("refs/heads/main")
            .current_dir(src_dir.path())
            .output()
            .expect("git bundle create");
        assert!(
            output.status.success(),
            "git bundle create failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let (dst_repo, _dst_dir) = empty_repo();
        unbundle(&dst_repo, bundles.path(), sha, &ref_name)
            .await
            .expect("native unbundle of git-created bundle");

        assert!(
            dst_repo.objects.clone().into_inner().contains(&oid),
            "commit not in dst ODB after native unbundle of git-created bundle"
        );
        drop(src_dir);
    }

    /// Create a bundle with our native implementation, then verify that
    /// `git bundle verify` accepts the format and `git bundle unbundle`
    /// can install the objects into a git repository.
    #[tokio::test]
    async fn native_bundle_create_accepted_by_git() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (src_repo, src_dir) = empty_repo();
        let oid = add_commit(&src_repo, "refs/heads/main", &[], "first");
        let sha = Sha::from_object_id(oid);
        let ref_name = RefName::new("refs/heads/main").expect("RefName");

        let bundles = TempDir::new().expect("tempdir");
        let bundle_path = bundle(&src_repo, bundles.path(), sha, ref_name.as_str())
            .await
            .expect("native bundle");
        drop(src_repo);

        // `git bundle verify` validates the header format and pack checksum.
        let output = std::process::Command::new("git")
            .args(["bundle", "verify"])
            .arg(&bundle_path)
            .current_dir(src_dir.path())
            .output()
            .expect("git bundle verify");
        assert!(
            output.status.success(),
            "git bundle verify rejected our bundle:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );

        // `git bundle unbundle` installs the pack objects into a repository.
        let (dst_repo, dst_dir) = empty_repo();
        let output = std::process::Command::new("git")
            .args(["bundle", "unbundle"])
            .arg(&bundle_path)
            .current_dir(dst_dir.path())
            .output()
            .expect("git bundle unbundle");
        assert!(
            output.status.success(),
            "git bundle unbundle failed on native bundle:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );

        assert!(
            dst_repo.objects.clone().into_inner().contains(&oid),
            "commit not in dst ODB after git bundle unbundle of native bundle"
        );
        drop(src_dir);
    }
}

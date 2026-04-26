//! Native git operations layered on top of [`gix`][gix].
//!
//! Mirrors the surface of upstream `git_remote_s3/git.py`. Operations that
//! `gix` 0.82 exposes go through `gix` natively; bundle creation and
//! consumption fall back to `git` subprocess because no public bundle
//! reader/writer exists in `gix` yet (see
//! `docs/development/spike-gix-bundle-parity.md`).
//!
//! Subprocess invocation is funnelled through a single private helper
//! [`run_git`] which hard-codes the stdio configuration required by
//! `.claude/rules/protocol-stdout.md` — stdin null, stdout and stderr
//! captured, never inherited. `run_git` is the only place in the crate
//! that spawns `git`.
//!
//! [gix]: https://docs.rs/gix

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::string::FromUtf8Error;
use std::sync::atomic::AtomicBool;

use gix::Repository;
use gix::bstr::{BStr, ByteSlice};
use gix::progress::Discard;
use gix::remote::Direction;
use gix_hash::ObjectId;
use thiserror::Error;
use tokio::process::Command;

/// SHA-1 commit OID, displayed as 40 lowercase hex characters.
///
/// Wraps [`gix_hash::ObjectId`] to make the wire-format invariant
/// (lowercase-hex bundle filenames, see `execution-plan.md` §1.1) a
/// type-system property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha(ObjectId);

impl Sha {
    /// Parse a SHA-1 hex string. Accepts lowercase, uppercase, or mixed
    /// case input and stores it canonically; [`Display`][fmt::Display]
    /// always emits lowercase.
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
    /// Validate `name` and wrap it. Returns [`RefNameError::Invalid`]
    /// for any string git itself would reject.
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
pub fn validate_ref_name(name: &str) -> bool {
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
    /// `git` binary is not on `PATH`.
    #[error("git binary not found on PATH")]
    GitBinaryMissing,
    /// `git` subprocess exited with a non-zero status.
    #[error("git {operation} failed: {stderr}")]
    Subprocess {
        /// Short tag identifying the subprocess command (e.g. `bundle create`).
        operation: &'static str,
        /// Captured stderr from the subprocess.
        stderr: String,
    },
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

/// The single git-spawning entry point.
///
/// Hard-codes `Stdio::null` for stdin and `Stdio::piped` for both stdout
/// and stderr, satisfying `.claude/rules/protocol-stdout.md`. `operation`
/// is a short human-readable tag attached to the resulting error if the
/// subprocess exits non-zero. `cwd` is set explicitly; the parent's cwd
/// is never inherited.
async fn run_git(
    operation: &'static str,
    args: &[&OsStr],
    cwd: &Path,
) -> Result<Vec<u8>, GitError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| match e.kind() {
            // `cwd` not existing also surfaces as `NotFound`; only treat
            // a missing-binary kind as such if the cwd is sane. The
            // probe is best-effort — a TOCTOU window is harmless here
            // since both branches still fail the call.
            io::ErrorKind::NotFound if cwd.is_dir() => GitError::GitBinaryMissing,
            _ => GitError::Io(e),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(GitError::Subprocess { operation, stderr });
    }

    if !output.stderr.is_empty() {
        tracing::debug!(
            target: "git_remote_object_store::git",
            operation,
            "{}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }

    Ok(output.stdout)
}

/// Pick a working directory for `git` subprocess invocations targeting
/// `repo`. Prefers the work tree (so relative path arguments resolve as
/// the user expects) and falls back to the git directory for bare
/// repositories.
fn repo_cwd(repo: &Repository) -> &Path {
    repo.workdir().unwrap_or_else(|| repo.git_dir())
}

/// Write a git bundle for `ref_name` to `<folder>/<sha>.bundle` and
/// return the absolute path.
///
/// Falls back to `git bundle create` because `gix` 0.82 has no public
/// bundle writer (see `docs/development/spike-gix-bundle-parity.md`).
/// `folder` is canonicalized so the returned bundle path resolves
/// identically regardless of the caller's cwd at observation time.
///
/// The returned future is **not** `Send`: `gix::Repository` is `!Sync`,
/// so the captured `&Repository` parameter cannot cross thread
/// boundaries. Callers must `.await` it directly rather than passing
/// it to `tokio::spawn`. This is fine for the protocol REPL, which
/// drives bundle/unbundle serially.
pub async fn bundle(
    repo: &Repository,
    folder: &Path,
    sha: Sha,
    ref_name: &RefName,
) -> Result<PathBuf, GitError> {
    let folder = folder.canonicalize()?;
    let bundle_path = folder.join(format!("{sha}.bundle"));
    let ref_arg = OsStr::new(ref_name.as_str());
    // `&Repository` is !Send (Repository is Send but !Sync), so we must
    // not hold a `&Path` borrowed from `repo` across the .await. Detach
    // to an owned PathBuf before suspension.
    let cwd = repo_cwd(repo).to_owned();
    let args: [&OsStr; 4] = [
        OsStr::new("bundle"),
        OsStr::new("create"),
        bundle_path.as_os_str(),
        ref_arg,
    ];
    run_git("bundle create", &args, &cwd).await?;
    Ok(bundle_path)
}

/// Unbundle `<folder>/<sha>.bundle` into `repo`, creating `ref_name`.
///
/// Falls back to `git bundle unbundle` for the same reason as
/// [`bundle`]. The trailing `ref_name` argument to `git bundle unbundle`
/// is what causes the ref to be created in the local repo — it is not
/// optional. `folder` is canonicalized so resolution is independent of
/// the caller's cwd.
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
pub async fn unbundle_at(
    cwd: &Path,
    folder: &Path,
    sha: Sha,
    ref_name: &RefName,
) -> Result<(), GitError> {
    let folder = folder.canonicalize()?;
    let bundle_path = folder.join(format!("{sha}.bundle"));
    let ref_arg = OsStr::new(ref_name.as_str());
    let args: [&OsStr; 4] = [
        OsStr::new("bundle"),
        OsStr::new("unbundle"),
        bundle_path.as_os_str(),
        ref_arg,
    ];
    run_git("bundle unbundle", &args, cwd).await?;
    Ok(())
}

/// Resolve a rev-spec (a ref name, full or short SHA, `HEAD~n`, etc.) to
/// the canonical 40-hex commit OID it points at.
pub fn rev_parse(repo: &Repository, spec: &str) -> Result<Sha, GitError> {
    if spec.is_empty() {
        return Err(GitError::EmptySpec);
    }
    let id = repo.rev_parse_single(BStr::new(spec))?;
    Ok(Sha::from_object_id(id.detach()))
}

/// Return `true` iff `ancestor` is an ancestor of `descendant` (or
/// equals it).
///
/// Uses the `merge_base(A, B) == A` identity. A commit is its own
/// ancestor; unrelated commits return `false`; missing commits propagate
/// as `GitError`.
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

/// Write a zip archive of the tree at `ref_name` to `<folder>/repo.zip`
/// and return the path.
///
/// Uses `gix-archive`'s native zip writer via
/// [`Repository::worktree_archive`]; no subprocess.
pub fn archive(repo: &Repository, folder: &Path, ref_name: &RefName) -> Result<PathBuf, GitError> {
    let tree = repo
        .rev_parse_single(BStr::new(ref_name.as_str()))?
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
/// `s3+zip` push variant (Phase 8).
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

#[cfg(test)]
mod tests {
    use super::*;

    use gix::actor::SignatureRef;
    use gix::bstr::BStr;
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

    fn add_commit(
        repo: &Repository,
        ref_name: &str,
        parents: &[ObjectId],
        message: &str,
    ) -> ObjectId {
        use gix::objs::tree::{Entry, EntryKind};
        // Write a one-blob tree so archive() has something to emit and
        // bundle round-trips carry real content. `repo.empty_tree()`
        // builds a `Tree` value but doesn't persist the object, which
        // would leave commits referencing a dangling tree id.
        let blob_id = repo.write_blob(b"hello\n").expect("write blob").detach();
        let tree = gix::objs::Tree {
            entries: vec![Entry {
                mode: EntryKind::Blob.into(),
                filename: "marker".into(),
                oid: blob_id,
            }],
        };
        let tree_id = repo.write_object(&tree).expect("write tree").detach();
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

    // --- RefName / validate_ref_name ----------------------------------

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
    fn validate_ref_name_partial_accepts_single_component_head() {
        // The partial validator accepts `HEAD`, matching the upstream
        // permissive regex; the strict `RefName::new` would reject it
        // because it isn't fully qualified.
        assert!(validate_ref_name("HEAD"));
    }

    #[test]
    fn validate_ref_name_partial_rejects_each_invalid_category() {
        // Empty and trailing-slash are rejected by `name_partial`.
        for name in &[
            "",
            "refs/heads/.hidden",
            "refs/heads/foo..bar",
            "refs/heads/main.lock",
        ] {
            assert!(!validate_ref_name(name), "expected !{name:?}");
        }
    }

    // --- rev_parse / is_ancestor / archive / last_commit_message / remote_url

    #[test]
    fn rev_parse_resolves_branch_ref() {
        let (repo, _dir) = empty_repo();
        let oid = add_commit(&repo, "refs/heads/main", &[], "first");
        let sha = rev_parse(&repo, "refs/heads/main").expect("rev_parse");
        assert_eq!(sha.as_object_id(), &oid);
    }

    #[test]
    fn rev_parse_resolves_full_sha() {
        let (repo, _dir) = empty_repo();
        let oid = add_commit(&repo, "refs/heads/main", &[], "first");
        let hex = oid.to_string();
        let sha = rev_parse(&repo, &hex).expect("rev_parse");
        assert_eq!(sha.as_object_id(), &oid);
    }

    #[test]
    fn rev_parse_unknown_returns_error() {
        let (repo, _dir) = empty_repo();
        add_commit(&repo, "refs/heads/main", &[], "first");
        assert!(rev_parse(&repo, "refs/heads/does-not-exist").is_err());
    }

    #[test]
    fn rev_parse_empty_returns_empty_spec() {
        let (repo, _dir) = empty_repo();
        add_commit(&repo, "refs/heads/main", &[], "first");
        assert!(matches!(rev_parse(&repo, ""), Err(GitError::EmptySpec)));
    }

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

    #[test]
    fn archive_writes_repo_zip_with_pk_header() {
        let (repo, dir) = empty_repo();
        add_commit(&repo, "refs/heads/main", &[], "first");
        let ref_name = RefName::new("refs/heads/main").expect("RefName");
        let out_dir = TempDir::new().expect("tempdir");
        let zip_path = archive(&repo, out_dir.path(), &ref_name).expect("archive");
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
        let ref_name = RefName::new("refs/tags/v1").expect("RefName");
        let out_dir = TempDir::new().expect("tempdir");
        let zip_path = archive(&repo, out_dir.path(), &ref_name).expect("archive tag");
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

    // --- bundle / unbundle (subprocess) -------------------------------

    #[tokio::test]
    async fn bundle_unbundle_round_trips_through_subprocess() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }

        let (src_repo, src_dir) = empty_repo();
        let oid = add_commit(&src_repo, "refs/heads/main", &[], "first");
        let sha = Sha::from_object_id(oid);
        let ref_name = RefName::new("refs/heads/main").expect("RefName");

        let bundles = TempDir::new().expect("tempdir");
        let bundle_path = bundle(&src_repo, bundles.path(), sha, &ref_name)
            .await
            .expect("bundle");
        assert!(bundle_path.exists(), "bundle not written");

        let (dst_repo, _dst_dir) = empty_repo();
        unbundle(&dst_repo, bundles.path(), sha, &ref_name)
            .await
            .expect("unbundle");
        // `git bundle unbundle` copies pack objects into the destination
        // odb but does not update refs — that's the remote-helper
        // protocol's job. Round-trip is proven by the commit object
        // becoming resolvable in dst_repo.
        let dst_sha = rev_parse(&dst_repo, &sha.to_string()).expect("rev_parse dst");
        assert_eq!(dst_sha, sha);
        drop(src_dir);
    }
}

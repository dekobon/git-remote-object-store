//! Parallel `fetch` handler.
//!
//! The remote-helper protocol delivers `fetch` commands as a batch
//! terminated by a blank line (see `gitremote-helpers(1)`). Upstream
//! Python's `process_fetch_cmds` (`../git-remote-s3/git_remote_s3/remote.py:477-496`)
//! services the batch with a `ThreadPoolExecutor`; we mirror that with a
//! [`tokio::task::JoinSet`] bounded by a [`tokio::sync::Semaphore`] of
//! [`MAX_FETCH_CONCURRENCY`] permits — matching upstream's
//! `max_concurrency=8` setting.
//!
//! Per fetch:
//! 1. Download `<prefix>/<ref>/<sha>.bundle` to a private tempdir
//! 2. `git bundle unbundle` it for `<ref>` (subprocess, see [`crate::git`])
//! 3. Mark the SHA as fetched in the session-wide [`FetchedRefs`] set
//!    so a second batch within the same REPL session skips work that
//!    has already happened (parity with upstream's `fetched_refs` list).
//!
//! Stdout discipline: this handler emits nothing on stdout. The trailing
//! blank-line terminator is the REPL's responsibility — see
//! `.claude/rules/protocol-stdout.md`.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gix_hash::ObjectId;
use tokio::sync::Semaphore;
use tokio::task::{JoinError, JoinSet};
use tracing::debug;

use crate::git::{self, GitError, RefName, RefNameError, Sha, ShaError};
use crate::keys;
use crate::object_store::{GetOpts, ObjectStore, ObjectStoreError};

/// Maximum number of in-flight bundle fetches per batch. Matches
/// upstream `boto3.s3.transfer.TransferConfig(max_concurrency=8)` from
/// `../git-remote-s3/git_remote_s3/remote.py:147`.
pub(crate) const MAX_FETCH_CONCURRENCY: usize = 8;

/// Errors surfaced by the fetch path.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// `fetch <sha> <ref>` line could not be parsed.
    #[error("invalid fetch command {line:?}: expected `<sha> <ref>`")]
    Parse {
        /// The offending line payload (after the `fetch ` prefix).
        line: String,
    },

    /// SHA hex was malformed.
    #[error("invalid SHA in fetch command: {0}")]
    Sha(#[from] ShaError),

    /// Ref name was malformed.
    #[error("invalid ref in fetch command: {0}")]
    Ref(#[from] RefNameError),

    /// Object-store call failed (bundle missing, network, auth, ...).
    #[error("object-store error during fetch: {0}")]
    Store(#[from] ObjectStoreError),

    /// Local I/O failure (tempdir creation, etc.).
    #[error("local I/O error during fetch: {0}")]
    Io(#[from] std::io::Error),

    /// `git bundle unbundle` failed.
    #[error("git error during fetch: {0}")]
    Git(#[from] GitError),

    /// A spawned fetch task panicked or was cancelled.
    #[error("fetch task join failed: {0}")]
    Join(#[from] JoinError),
}

/// Session-wide set of SHAs already fetched in this REPL run.
///
/// Cloning is cheap (`Arc` bump). Behaviour mirrors upstream's
/// `fetched_refs` list + `fetched_refs_lock`: lookups and insertions
/// are serialised, but the lock is released around the long-running
/// download/unbundle so concurrent fetches actually run in parallel.
#[derive(Clone, Default)]
pub(crate) struct FetchedRefs {
    inner: Arc<Mutex<HashSet<Sha>>>,
}

impl FetchedRefs {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn contains(&self, sha: &Sha) -> bool {
        // We hold the lock only across `HashSet::contains` / `insert`,
        // both of which cannot leave the set in a half-modified state.
        // If a previous holder panicked, the set is still safe to read,
        // so recover the inner guard rather than escalating to a
        // process-wide abort.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(sha)
    }

    fn insert(&self, sha: Sha) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(sha);
    }

    /// Snapshot of the current set, for tests and assertions.
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> HashSet<Sha> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Per-batch accumulator of shallow boundary OIDs.
///
/// Mirrors [`FetchedRefs`]: each parallel `fetch_one` task pushes the
/// boundary commits it computed, and `fetch_batch` performs a single
/// merged write to `.git/shallow` after all tasks finish. Cloning is
/// cheap (`Arc` bump).
#[derive(Clone, Default)]
struct ShallowBoundaries {
    inner: Arc<Mutex<HashSet<ObjectId>>>,
}

impl ShallowBoundaries {
    fn new() -> Self {
        Self::default()
    }

    fn extend(&self, ids: impl IntoIterator<Item = ObjectId>) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.extend(ids);
    }

    fn drain(&self) -> Vec<ObjectId> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.drain().collect()
    }
}

/// Drive a batch of `fetch` commands to completion.
///
/// Runs at most [`MAX_FETCH_CONCURRENCY`] downloads in parallel and
/// returns the first error after every spawned task has finished — that
/// way no zombie task is left running when the helper exits.
///
/// `depth`, when set, requests a shallow fetch: each ref's installed
/// objects are walked breadth-first from the tip and the boundary
/// commits at depth `N` are merged into `.git/shallow` once all tasks
/// have finished. The walk runs even when a SHA's bundle was skipped
/// (already in `fetched_refs` from an earlier batch in the same REPL
/// session), since the boundary depends on `depth`, not on whether the
/// download happened.
pub(crate) async fn fetch_batch(
    ctx: &super::BatchCtx,
    cmds: Vec<String>,
    fetched_refs: FetchedRefs,
    depth: Option<NonZeroU32>,
) -> Result<(), FetchError> {
    if cmds.is_empty() {
        return Ok(());
    }
    debug!(
        count = cmds.len(),
        depth = ?depth,
        "fetching bundles in parallel"
    );

    let semaphore = Arc::new(Semaphore::new(MAX_FETCH_CONCURRENCY));
    let mut tasks: JoinSet<Result<(), FetchError>> = JoinSet::new();
    // Clone the Arc<str> once; each spawned task gets a cheap reference-count bump.
    let prefix = ctx.prefix.clone();
    let boundaries = ShallowBoundaries::new();

    for cmd in cmds {
        let store = Arc::clone(&ctx.store);
        let semaphore = Arc::clone(&semaphore);
        let prefix = prefix.clone();
        let repo_dir = Arc::clone(&ctx.repo_dir);
        let fetched_refs = fetched_refs.clone();
        let boundaries = boundaries.clone();
        tasks.spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("fetch semaphore is owned by this batch and never closed");
            let (sha, ref_name) = parse_fetch_args(&cmd)?;
            fetch_one(FetchOneCtx {
                store: store.as_ref(),
                prefix: prefix.as_deref(),
                repo_dir: repo_dir.as_path(),
                sha,
                ref_name: &ref_name,
                fetched_refs: &fetched_refs,
                depth,
                boundaries: &boundaries,
            })
            .await
        });
    }

    // Drain every task before returning, so a single failure cannot
    // leave the rest running into a closing helper. First error wins.
    let mut first_err: Option<FetchError> = None;
    while let Some(joined) = tasks.join_next().await {
        // `joined` is `Result<Result<(), FetchError>, JoinError>` — flatten
        // by promoting a join error (panic / cancellation) into a
        // `FetchError::Join` and keeping the inner result otherwise.
        let res: Result<(), FetchError> = joined.unwrap_or_else(|je| Err(je.into()));
        if let Err(err) = res
            && first_err.is_none()
        {
            first_err = Some(err);
        }
    }

    // Apply shallow boundaries only when the batch succeeded. If any
    // task errored, the bundle for that ref may be missing from the
    // ODB, so its BFS results would be incomplete — better to leave
    // `.git/shallow` untouched and let git report the underlying error
    // than to write a half-built boundary set.
    if first_err.is_none() && depth.is_some() {
        let collected = boundaries.drain();
        if !collected.is_empty() {
            let git_dir = git_dir_for(ctx.repo_dir.as_path());
            tokio::task::spawn_blocking(move || git::write_shallow_file(&git_dir, &collected))
                .await
                .map_err(FetchError::from)?
                .map_err(FetchError::from)?;
        }
    }

    first_err.map_or(Ok(()), Err)
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
        let pointed = std::path::Path::new(rest.trim());
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

/// Per-task fetch context: bundles every parameter that varies across
/// `fetch_one` calls into a single struct, keeping the function under
/// the clippy `too_many_arguments` threshold while still owning every
/// borrow it needs.
struct FetchOneCtx<'a> {
    store: &'a dyn ObjectStore,
    prefix: Option<&'a str>,
    repo_dir: &'a Path,
    sha: Sha,
    ref_name: &'a RefName,
    fetched_refs: &'a FetchedRefs,
    depth: Option<NonZeroU32>,
    boundaries: &'a ShallowBoundaries,
}

async fn fetch_one(ctx: FetchOneCtx<'_>) -> Result<(), FetchError> {
    let FetchOneCtx {
        store,
        prefix,
        repo_dir,
        sha,
        ref_name,
        fetched_refs,
        depth,
        boundaries,
    } = ctx;

    if fetched_refs.contains(&sha) {
        debug!(%sha, ref_name = %ref_name, "skipping fetch: already fetched in this session");
    } else {
        let key = keys::bundle_key(prefix, ref_name, sha);
        let temp_dir = tempfile::Builder::new()
            .prefix("git_remote_object_store_fetch_")
            .tempdir()?;
        let bundle_path = temp_dir.path().join(format!("{sha}.bundle"));
        debug!(%sha, ref_name = %ref_name, key = %key, "downloading bundle");
        store
            .get_to_file(&key, &bundle_path, GetOpts::default())
            .await?;
        git::unbundle_at(repo_dir, temp_dir.path(), sha, ref_name).await?;
        fetched_refs.insert(sha);
    }

    // Shallow boundary collection runs even when the bundle was
    // already installed in this session: the per-batch `depth` option
    // is independent of the dedup that skips the download. The walk
    // is cheap (objects are local) and skipping it would silently
    // omit boundaries from the eventual `.git/shallow` write.
    if let Some(depth) = depth {
        let repo_dir = repo_dir.to_path_buf();
        let ids = tokio::task::spawn_blocking(move || {
            let repo = gix::open(&repo_dir).map_err(GitError::from)?;
            git::shallow_boundaries(&repo, sha, depth)
        })
        .await
        .map_err(FetchError::from)?
        .map_err(FetchError::from)?;
        boundaries.extend(ids);
    }
    Ok(())
}

/// Parse the payload of a `fetch <sha> <ref>` line (the bytes after the
/// `fetch ` prefix have already been stripped by the REPL).
fn parse_fetch_args(args: &str) -> Result<(Sha, RefName), FetchError> {
    let parse_err = || FetchError::Parse {
        line: args.to_owned(),
    };
    let (sha, ref_name) = args.split_once(' ').ok_or_else(parse_err)?;
    if sha.is_empty() || ref_name.is_empty() || ref_name.contains(' ') {
        return Err(parse_err());
    }
    Ok((Sha::from_hex(sha)?, RefName::new(ref_name)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn bundle_key_with_prefix_joins_with_slash() {
        let sha = Sha::from_hex(SHA).unwrap();
        let ref_name = RefName::new("refs/heads/main").unwrap();
        assert_eq!(
            keys::bundle_key(Some("repo"), &ref_name, sha),
            format!("repo/refs/heads/main/{SHA}.bundle"),
        );
    }

    #[test]
    fn bundle_key_no_prefix_omits_leading_slash() {
        let sha = Sha::from_hex(SHA).unwrap();
        let ref_name = RefName::new("refs/heads/main").unwrap();
        assert_eq!(
            keys::bundle_key(None, &ref_name, sha),
            format!("refs/heads/main/{SHA}.bundle"),
        );
        // Empty-string prefix is treated identically to None — guards
        // against an accidental `/refs/...` bundle key.
        assert_eq!(
            keys::bundle_key(Some(""), &ref_name, sha),
            format!("refs/heads/main/{SHA}.bundle"),
        );
    }

    #[test]
    fn parse_fetch_args_accepts_canonical_form() {
        let (sha, ref_name) = parse_fetch_args(&format!("{SHA} refs/heads/main")).unwrap();
        assert_eq!(sha.to_string(), SHA);
        assert_eq!(ref_name.as_str(), "refs/heads/main");
    }

    #[test]
    fn parse_fetch_args_rejects_missing_ref() {
        assert!(matches!(
            parse_fetch_args(SHA),
            Err(FetchError::Parse { .. })
        ));
    }

    #[test]
    fn parse_fetch_args_rejects_empty_ref() {
        assert!(matches!(
            parse_fetch_args(&format!("{SHA} ")),
            Err(FetchError::Parse { .. })
        ));
    }

    #[test]
    fn parse_fetch_args_rejects_invalid_sha() {
        assert!(matches!(
            parse_fetch_args("notahex refs/heads/main"),
            Err(FetchError::Sha(_))
        ));
    }

    #[test]
    fn parse_fetch_args_rejects_invalid_ref() {
        assert!(matches!(
            parse_fetch_args(&format!("{SHA} refs/heads/.bad")),
            Err(FetchError::Ref(_))
        ));
    }

    #[test]
    fn parse_fetch_args_rejects_extra_whitespace() {
        // Protocol guarantees a single space; reject obvious garbage so
        // a malformed fetch line never silently splits a ref name.
        assert!(matches!(
            parse_fetch_args(&format!("{SHA} refs/heads/main extra")),
            Err(FetchError::Parse { .. })
        ));
    }

    #[test]
    fn fetched_refs_dedupes_repeated_inserts() {
        // The Mutex<HashSet> is structurally Send + Sync; the dedup
        // semantics we actually rely on are HashSet's. Verify the
        // observable contract: a second insert of the same Sha leaves
        // the set at size 1 and `contains` flips on the first insert.
        let refs = FetchedRefs::new();
        let sha = Sha::from_hex(SHA).unwrap();
        assert!(!refs.contains(&sha));
        refs.insert(sha);
        refs.insert(sha);
        assert!(refs.contains(&sha));
        assert_eq!(refs.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn fetch_batch_empty_cmds_short_circuits() {
        use crate::object_store::mock::MockStore;
        use crate::protocol::BatchCtx;
        // Empty-cmds early return — no store call, no spawn. Covers the
        // internal short-circuit that the integration test cannot reach
        // (the REPL never calls `fetch_batch` with an empty Vec because
        // `BatchState::take_pending` guards on non-empty cmds).
        //
        // The empty store acts as the natural tripwire: any accidental
        // store.get_to_file() call would return NotFound and propagate as
        // Err, which the Ok(()) assertion below would catch.
        let mock = Arc::new(MockStore::new());
        let repo_dir = tempfile::tempdir().expect("tempdir");
        let ctx = BatchCtx {
            store: Arc::clone(&mock) as Arc<dyn ObjectStore>,
            prefix: Some("repo".into()),
            repo_dir: Arc::new(repo_dir.path().to_path_buf()),
        };
        let result = fetch_batch(&ctx, Vec::new(), FetchedRefs::new(), None).await;
        assert!(matches!(result, Ok(())));
    }
}

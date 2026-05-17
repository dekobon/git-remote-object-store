//! `delete-branch`, `protect`, `unprotect` subcommands.
//!
//! Each operation is anchored at `<prefix>/refs/heads/<branch>/`, the same
//! key space the protocol REPL writes bundles into. When the URL has no
//! repository prefix (root-of-bucket repos, `<prefix>` is empty), keys
//! collapse to `refs/heads/<branch>/...` with no leading slash.
//!
//! All operator-visible output goes through a `Write`-bound writer (the
//! `*_into` entry points) so tests can capture and assert on the
//! messages. The public `delete()`, `protect()`, `unprotect()` methods
//! wrap their `*_into` siblings with `std::io::stdout()` for the
//! management CLI.

use std::collections::HashSet;
use std::io::Write;
use std::sync::Arc;

use bytes::Bytes;
use time::OffsetDateTime;
use tracing::{info, warn};

use super::{ManageError, Prompter};
use crate::git::RefName;
use crate::keys;
use crate::object_store::{ObjectMeta, ObjectStore, ObjectStoreError, PutOpts};
use crate::packchain::gc::try_write_baseline_tombstone;
use crate::protocol::push::{
    LockGuard, acquire_lock, lock_key, lock_ttl_from_env, release_lock,
    verify_no_orphan_protected_after_delete,
};

/// Operations on a single branch within a repository.
pub struct ManageBranch<'a> {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    branch: String,
    prompter: &'a dyn Prompter,
}

impl<'a> ManageBranch<'a> {
    /// Open a branch handle, verifying it exists by listing
    /// `<prefix>/refs/heads/<branch>/` (or `refs/heads/<branch>/` when
    /// `prefix` is empty).
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::InvalidBranch`] if `branch` fails
    /// `gix-validate`'s strict ref-name check. Returns
    /// [`ManageError::BranchNotFound`] when no objects exist under the
    /// branch prefix. Returns [`ManageError::Store`] for object-store
    /// failures.
    pub async fn open(
        store: Arc<dyn ObjectStore>,
        prefix: impl Into<String>,
        branch: impl Into<String>,
        prompter: &'a dyn Prompter,
    ) -> Result<Self, ManageError> {
        let branch = branch.into();
        // Reject branch names that git itself would reject. S3 / Azure
        // are case-sensitive byte stores with no path semantics, so a
        // value like `foo/../bar` would be stored verbatim and produce
        // unrecoverable junk under `<prefix>/refs/heads/`. The strict
        // `RefName::is_valid` (delegating to `gix_validate::reference::name`)
        // rejects empties, `..`, control characters, and the rest of
        // git's invalid-ref alphabet. Use the borrow-only predicate
        // here so we don't allocate a wrapped `RefName` we'd discard.
        if !RefName::is_valid(&format!("refs/heads/{branch}")) {
            return Err(ManageError::InvalidBranch(branch));
        }
        let mb = Self {
            store,
            prefix: prefix.into(),
            branch,
            prompter,
        };
        if mb.store.list(&mb.branch_prefix()).await?.is_empty() {
            return Err(ManageError::BranchNotFound(mb.branch));
        }
        Ok(mb)
    }

    fn branch_prefix(&self) -> String {
        keys::ref_listing_prefix(Some(&self.prefix), &format!("refs/heads/{}", self.branch))
    }

    fn protected_key(&self) -> String {
        keys::join(
            Some(&self.prefix),
            &format!(
                "refs/heads/{}/{}",
                self.branch,
                keys::PROTECTED_MARKER_SEGMENT,
            ),
        )
    }

    /// Delete every object under the branch's prefix after a `yes/no`
    /// confirmation. Aborts (returns `Ok(())`) if the user answers no;
    /// the `Cancelled` variant is reserved for prompt I/O failures.
    ///
    /// Refuses outright when a `PROTECTED#` marker is present under the
    /// branch prefix — the operator must run `unprotect` first. This
    /// mirrors the refusal the helper-protocol delete path
    /// (`delete_remote_ref_under_lock`) emits, so a `git push :branch`
    /// against a protected ref and a management-CLI `delete-branch` of
    /// the same ref fail the same way.
    ///
    /// # Per-ref lock (#158)
    ///
    /// After the operator confirms the prompt, `delete-branch` acquires
    /// the same `<prefix>/<ref>/LOCK#.lock` the helper-protocol push
    /// and delete paths take. The lock is held across the fresh re-list,
    /// the baseline tombstone write (#143), and the synchronous sweep.
    /// Without it a concurrent `git push` could land a new bundle after
    /// the post-prompt re-list, the sweep would delete only the stale
    /// snapshot, and the ref would survive with the just-pushed bundle
    /// even though delete-branch reported success.
    ///
    /// Lock acquisition runs AFTER the prompt — the prompt is
    /// interactive and could block indefinitely, and holding the lock
    /// across user input would make every other writer wait on the
    /// operator's keyboard. If the lock is contended at acquisition
    /// time the function returns [`ManageError::LockContended`] and
    /// makes no changes. Release failures are downgraded to a `warn!`
    /// because the lock's TTL guarantees a stale lock is recovered by
    /// the next acquirer; matches the protocol-push pattern.
    ///
    /// The prompt-display and protection-marker check use a first listing
    /// for accuracy of the displayed object count, then a **second
    /// listing is taken under the lock immediately before the deletion
    /// loop**. The fresh listing drives the sweep so that any concurrent
    /// push landing under the branch prefix during the prompt window —
    /// before the lock window opens — is caught and deleted rather than
    /// left as a zombie object (#139). The protection-marker check is
    /// re-evaluated on the fresh listing so a `protect` racing with the
    /// prompt is honoured (#131) — the post-prompt re-check is what
    /// closes the TOCTOU window between the initial marker check and
    /// the deletion loop. If the fresh listing is empty (a concurrent
    /// delete won the race) the function reports it and returns
    /// `Ok(())` rather than silently claiming success.
    ///
    /// `NotFound` errors observed during the sweep are tolerated — they
    /// mean a concurrent deleter swept the key first, which still
    /// satisfies the operator's intent. Other per-key delete errors
    /// (Network, `AccessDenied`, ...) are collected: the loop does NOT
    /// short-circuit, every remaining key is still attempted, and the
    /// function returns [`ManageError::PartialDelete`] with the exact
    /// list of keys that survived so a retry can converge (#122). A
    /// list-call failure still propagates immediately because there is
    /// nothing to recover — without a listing the sweep cannot proceed.
    ///
    /// Packchain refs with a parseable `chain.json` skip immediate
    /// deletion of the baseline bundle (`<full_at>.bundle`): a
    /// baseline tombstone is written first and the bundle is left for
    /// `gc sweep` to reclaim after the grace window (#143). The
    /// synchronous sweep still removes `chain.json`,
    /// `path-index.json`, and any other residue. The deferral protects
    /// an in-flight fetcher that already read the prior `chain.json`
    /// from a `BaselineMissing` range-GET failure; a fresh reader
    /// sees the missing chain and the ref is gone from its
    /// perspective. Bundle-engine refs, refs with an unparseable
    /// chain, and any tombstone PUT failure fall through to immediate
    /// bundle deletion so the operator's "ref is gone" intent is
    /// never blocked on the tombstone path.
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::Protected`] if the branch carries a
    /// `PROTECTED#` marker (checked on both listings),
    /// [`ManageError::LockContended`] if another writer holds the
    /// per-ref lock at acquisition time,
    /// [`ManageError::Cancelled`] if the user cancels the prompt,
    /// [`ManageError::Io`] for prompt or write I/O failures,
    /// [`ManageError::Store`] if a list operation fails, or
    /// [`ManageError::PartialDelete`] when one or more per-key deletes
    /// fail with a non-`NotFound` error after every key in the fresh
    /// listing has been attempted.
    pub async fn delete(&self) -> Result<(), ManageError> {
        self.delete_into(&mut std::io::stdout()).await
    }

    /// Same contract as [`delete`](Self::delete) but writes human-readable
    /// output to `out`. Tests use this to capture the operator messages
    /// (e.g. the "already gone" race notice from #139) so a regression
    /// that drops the message — silently turning a concurrent race into
    /// an apparent success — is caught.
    ///
    /// # Errors
    ///
    /// Same as [`delete`](Self::delete), plus [`ManageError::Io`] if a
    /// write to `out` fails.
    pub(crate) async fn delete_into<W: Write>(&self, out: &mut W) -> Result<(), ManageError> {
        let listing_prefix = self.branch_prefix();
        let initial = self.store.list(&listing_prefix).await?;
        if keys::entries_have_protected_marker(&initial) {
            return Err(ManageError::Protected(self.branch.clone()));
        }
        let prompt = format!("Delete branch {} ({} objects)?", self.branch, initial.len());
        if !self.prompter.confirm(&prompt)? {
            writeln!(out, "Aborted")?;
            return Ok(());
        }

        // Acquire the per-ref lock AFTER the prompt and BEFORE the
        // fresh re-list / tombstone / sweep. Holding the lock across
        // the prompt would block every concurrent writer on the
        // operator's keyboard; the lock window starts only once the
        // operator has confirmed the intent (#158). The protocol push
        // / delete paths and `packchain::compact` use the same lock
        // key, so a concurrent `git push` or `compact` racing this
        // delete is mutually excluded.
        let ref_name = self.validated_ref_name()?;
        let (lock_key, guard) = self.acquire_ref_lock("delete-branch").await?;
        let work = self
            .delete_under_lock(out, &listing_prefix, &lock_key, &initial, &ref_name)
            .await;
        self.release_or_warn(guard, &lock_key, "delete-branch")
            .await;
        work
    }

    /// The lock-held body of [`Self::delete_into`]: fresh re-list,
    /// protection re-check, tombstone write, sweep. Extracted so the
    /// caller's `release_lock` runs unconditionally on every exit
    /// path. The lock key is filtered from the fresh listing so the
    /// sweep does not delete the very lock we hold.
    async fn delete_under_lock<W: Write>(
        &self,
        out: &mut W,
        listing_prefix: &str,
        lock: &str,
        initial: &[ObjectMeta],
        ref_name: &RefName,
    ) -> Result<(), ManageError> {
        // Re-list under the lock so concurrent pushes that landed
        // during the prompt window — before the lock window opened —
        // are included in the deletion set. With the lock now held,
        // no further writes can sneak in between this listing and the
        // sweep. Filter out the lock key itself: we hold it and the
        // release tail removes it; sweeping it mid-critical-section
        // would let another acquirer take the lock under us.
        let fresh: Vec<ObjectMeta> = self
            .store
            .list(listing_prefix)
            .await?
            .into_iter()
            .filter(|m| m.key != lock)
            .collect();
        if fresh.is_empty() {
            writeln!(
                out,
                "Branch {} is already gone (concurrent delete during prompt); nothing to do",
                self.branch,
            )?;
            info!(
                branch = %self.branch,
                "branch already deleted by concurrent operation",
            );
            return Ok(());
        }
        if keys::entries_have_protected_marker(&fresh) {
            return Err(ManageError::Protected(self.branch.clone()));
        }

        let initial_keys: HashSet<&str> = initial.iter().map(|m| m.key.as_str()).collect();
        let concurrent_adds = fresh
            .iter()
            .filter(|m| !initial_keys.contains(m.key.as_str()))
            .count();
        if concurrent_adds > 0 {
            warn!(
                branch = %self.branch,
                added = concurrent_adds,
                "concurrent activity detected during prompt; sweeping fresh listing",
            );
        }

        // Issue #143: if the ref is a packchain ref with a parseable
        // `chain.json`, write a baseline tombstone naming the current
        // `full_at` bundle BEFORE the synchronous sweep, then exclude
        // that bundle key from the delete loop. A concurrent fetcher
        // that read the prior `chain.json` (t₀) and is mid-range-GET
        // on `<full_at>.bundle` then completes against the still-live
        // bundle; `gc sweep` reclaims it after the grace window. The
        // synchronous sweep still removes `chain.json`,
        // `path-index.json`, and every other key — from a fresh
        // reader's perspective the ref is gone the moment those
        // commit. Bundle-engine refs (no `chain.json`) and refs with
        // an unparseable chain fall through to the immediate-delete
        // path: there is nothing for `sweep` to reconcile against, so
        // deferral would just orphan the bundle.
        //
        // The tombstone write runs UNDER the lock (#158): a concurrent
        // push that landed between the tombstone and the chain.json
        // delete would otherwise leave the bucket with a tombstone
        // referencing a SHA no longer in the chain, and `gc sweep`
        // would reclaim a live bundle.
        let deferred_bundle_key = self.try_tombstone_baseline(&fresh).await;
        if let Some(ref key) = deferred_bundle_key {
            info!(
                branch = %self.branch,
                key = %key,
                "delete-branch: deferred baseline bundle delete via tombstone",
            );
        }

        // Collect, don't short-circuit: a transient failure on key #2
        // of a 4-key listing must not leave #3 and #4 standing with no
        // inventory of what survived. NotFound continues to be tolerated
        // (the key is gone — operator intent satisfied). Every other
        // per-key error is logged and recorded; at the end we either
        // declare full success or return PartialDelete naming every
        // surviving key so a retry can converge (#122).
        let mut undeleted: Vec<String> = Vec::new();
        for object in &fresh {
            // The baseline bundle (if any) is left for `gc sweep` —
            // see the tombstone block above. Other keys (chain.json,
            // path-index.json, PROTECTED# is already refused earlier)
            // are deleted synchronously. The lock key was filtered
            // from `fresh` above, so it is not in the iteration.
            if deferred_bundle_key.as_deref() == Some(object.key.as_str()) {
                continue;
            }
            match self.store.delete(&object.key).await {
                Ok(()) | Err(ObjectStoreError::NotFound(_)) => {}
                Err(err) => {
                    warn!(
                        branch = %self.branch,
                        key = %object.key,
                        error = %err,
                        "delete-branch: per-key delete failed; continuing sweep",
                    );
                    undeleted.push(object.key.clone());
                }
            }
        }
        // attempted excludes the deferred bundle (if any): that key was
        // intentionally skipped via tombstone, not "attempted and missing"
        // — the operator-facing count must reflect what was swept, not
        // what was listed.
        let attempted = fresh.len() - usize::from(deferred_bundle_key.is_some());
        if !undeleted.is_empty() {
            return Err(ManageError::PartialDelete {
                branch: self.branch.clone(),
                undeleted,
                attempted,
            });
        }
        // Issue #151 defence-in-depth: post-sweep, with the lock still
        // held, confirm no `PROTECTED#` marker is present for this ref.
        // The primary defence is the per-ref lock — `protect` /
        // `unprotect` both acquire the same `<prefix>/<ref>/LOCK#.lock`
        // per #159, so a marker cannot land between the under-lock
        // listing and the sweep. This `head` probe is belt-and-suspenders
        // surveillance: an orphan marker observed here would indicate a
        // contract violation (lock bypass, bucket inconsistency, or
        // misbehaving sibling tool). The helper logs at `error!` and
        // does NOT change the delete's success outcome — the branch's
        // bundle artefacts are gone, so the operator's intent stands.
        verify_no_orphan_protected_after_delete(self.store.as_ref(), self.prefix_opt(), ref_name)
            .await;
        writeln!(out, "Branch {} has been deleted", self.branch)?;
        info!(branch = %self.branch, count = attempted, "branch deleted");
        Ok(())
    }

    /// Build a validated `RefName` for `refs/heads/<branch>`. `open`
    /// already accepted this value, so the parse is effectively
    /// infallible — but we surface a parse failure as
    /// [`ManageError::InvalidBranch`] rather than panicking so a
    /// future loosening of `open`'s validator cannot turn delete-branch
    /// into a panic surface.
    fn validated_ref_name(&self) -> Result<RefName, ManageError> {
        RefName::new(format!("refs/heads/{}", self.branch))
            .map_err(|_| ManageError::InvalidBranch(self.branch.clone()))
    }

    /// Returns `Some(&prefix)` when a non-empty bucket prefix is
    /// configured, `None` for root-prefixed buckets. Centralises the
    /// `(!self.prefix.is_empty()).then_some(self.prefix.as_str())`
    /// pattern previously duplicated across delete and tombstone paths.
    fn prefix_opt(&self) -> Option<&str> {
        (!self.prefix.is_empty()).then_some(self.prefix.as_str())
    }

    /// Attempt to tombstone the baseline bundle for a packchain ref so
    /// the synchronous delete loop can skip it (issue #143). Returns
    /// the bundle key that was deferred, or `None` if no deferral is
    /// possible. Thin caller-side wrapper that resolves `&self`'s
    /// prefix / ref-name and delegates to the shared
    /// [`try_write_baseline_tombstone`] helper for the actual
    /// load-chain / listing-check / tombstone-write logic (#221).
    async fn try_tombstone_baseline(
        &self,
        fresh: &[crate::object_store::ObjectMeta],
    ) -> Option<String> {
        // `RefName::new` re-runs the same `gix-validate` check `open`
        // already accepted, so this is effectively infallible. Surface
        // a parse failure as "no tombstone" rather than panicking — a
        // future loosening of `open`'s validator must not make
        // delete-branch unsafe.
        let ref_name = RefName::new(format!("refs/heads/{}", self.branch)).ok()?;
        try_write_baseline_tombstone(
            self.store.as_ref(),
            self.prefix_opt(),
            &ref_name,
            fresh,
            "delete-branch",
        )
        .await
    }

    /// Mark the branch as protected by writing the `PROTECTED#` sentinel.
    /// Idempotent — overwrites any existing marker.
    ///
    /// # Per-ref lock (#159)
    ///
    /// `protect` acquires the same `<prefix>/<ref>/LOCK#.lock` the
    /// helper-protocol push, helper-protocol delete, and `delete-branch`
    /// take. Pre-#159, the push path's pre-bundle `is_protected` check
    /// could race a concurrent `protect`: a force-push that observed no
    /// marker would still overwrite the bundle even if `protect` landed
    /// between the under-lock `is_protected` and the bundle upload —
    /// because `protect` was a lockless `put_bytes`. Taking the same
    /// lock serialises protection state changes against the writers
    /// that consult it, closing the entire write window rather than
    /// narrowing it to a second sample.
    ///
    /// If the lock is contended (a push, delete, or compact holds it),
    /// `protect` returns [`ManageError::LockContended`] and makes no
    /// changes. Operators can retry. Stale-lock recovery is inherited
    /// from `acquire_lock` (a previous holder that crashed without
    /// releasing).
    ///
    /// Re-lists the branch prefix under the lock so a concurrent
    /// `delete-branch` (or last-bundle removal) that landed between
    /// [`ManageBranch::open`] and the lock window is caught and the
    /// marker is NOT written for a non-existent branch (#137). Without
    /// this re-check the orphaned `PROTECTED#` would persist with no
    /// automated cleanup and would silently block a future recreation
    /// of the same branch from being force-pushed or deleted. The
    /// re-listing filters out stale lock keys and any pre-existing
    /// `PROTECTED#` marker so a branch whose only residue is operational
    /// metadata is treated as gone.
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::BranchNotFound`] if the under-lock listing
    /// shows the branch was deleted concurrently. Returns
    /// [`ManageError::LockContended`] if another writer holds the
    /// per-ref lock at acquisition time. Returns [`ManageError::Store`]
    /// if a list or put operation fails.
    pub async fn protect(&self) -> Result<(), ManageError> {
        self.protect_into(&mut std::io::stdout()).await
    }

    /// Writer-injecting variant of [`Self::protect`] so tests can
    /// capture the "now protected" operator message. Mirrors the
    /// pattern established by [`Self::delete_into`] (#145) and the
    /// management CLI's other writer-aware entry points.
    ///
    /// # Errors
    ///
    /// Same as [`Self::protect`], plus [`ManageError::Io`] if a write
    /// to `out` fails.
    pub(crate) async fn protect_into<W: Write>(&self, out: &mut W) -> Result<(), ManageError> {
        let (lock_key, guard) = self.acquire_ref_lock("protect").await?;
        let work = self.protect_under_lock(out).await;
        self.release_or_warn(guard, &lock_key, "protect").await;
        work
    }

    /// Lock-held body of [`Self::protect_into`]: re-list under the
    /// lock, reject if the branch has been deleted concurrently,
    /// otherwise write the `PROTECTED#` sentinel. Extracted so the
    /// acquire/release tail in `protect_into` runs unconditionally on
    /// every exit path — including the `BranchNotFound` early return.
    async fn protect_under_lock<W: Write>(&self, out: &mut W) -> Result<(), ManageError> {
        let fresh = self.store.list(&self.branch_prefix()).await?;
        if !super::has_branch_data(&fresh) {
            warn!(
                branch = %self.branch,
                "branch was deleted concurrently between open and protect; refusing to write orphaned marker",
            );
            return Err(ManageError::BranchNotFound(self.branch.clone()));
        }
        self.store
            .put_bytes(&self.protected_key(), Bytes::new(), PutOpts::default())
            .await?;
        writeln!(out, "Branch {} is now protected", self.branch)?;
        Ok(())
    }

    /// Remove the `PROTECTED#` sentinel. A missing marker is treated as
    /// already-unprotected rather than an error.
    ///
    /// # Per-ref lock (#159)
    ///
    /// `unprotect` acquires the same per-ref lock as [`Self::protect`]
    /// so ALL protection state changes serialise against pushes,
    /// deletes, and compactions. Without taking the lock here a
    /// concurrent push observing `is_protected() == true` could
    /// otherwise commit to the protected refusal path just as
    /// `unprotect` landed, leaving the writer's behaviour out of step
    /// with operator intent. Symmetry with `protect` keeps the lock the
    /// single point of serialisation for protection state.
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::LockContended`] if another writer holds
    /// the per-ref lock at acquisition time. Returns
    /// [`ManageError::Store`] for object-store failures other than
    /// `NotFound`.
    pub async fn unprotect(&self) -> Result<(), ManageError> {
        self.unprotect_into(&mut std::io::stdout()).await
    }

    /// Writer-injecting variant of [`Self::unprotect`] so tests can
    /// capture the "now unprotected" operator message.
    ///
    /// # Errors
    ///
    /// Same as [`Self::unprotect`], plus [`ManageError::Io`] if a
    /// write to `out` fails.
    pub(crate) async fn unprotect_into<W: Write>(&self, out: &mut W) -> Result<(), ManageError> {
        let (lock_key, guard) = self.acquire_ref_lock("unprotect").await?;
        let work = self.unprotect_under_lock(out).await;
        self.release_or_warn(guard, &lock_key, "unprotect").await;
        work
    }

    /// Lock-held body of [`Self::unprotect_into`]: delete the
    /// `PROTECTED#` marker, treating `NotFound` as
    /// already-unprotected. The lock scope is mechanical (no listing
    /// or recovery work needed); we still hold it so a concurrent
    /// `protect` cannot land between here and the delete and leave
    /// the operator's "unprotect" intent silently overridden.
    async fn unprotect_under_lock<W: Write>(&self, out: &mut W) -> Result<(), ManageError> {
        match self.store.delete(&self.protected_key()).await {
            Ok(()) | Err(ObjectStoreError::NotFound(_)) => {
                writeln!(out, "Branch {} is now unprotected", self.branch)?;
                Ok(())
            }
            Err(other) => Err(other.into()),
        }
    }

    /// Acquire the per-ref lock for `op` (`delete-branch`, `protect`,
    /// `unprotect`, or any future ref-mutating caller). Returns the
    /// resolved lock object-store key alongside the guard so the
    /// matching `release_or_warn` tail can name the key in its log
    /// line without re-deriving it.
    ///
    /// Contention surfaces as [`ManageError::LockContended`] with the
    /// branch name, lock key, and current TTL — matching the wording
    /// `delete-branch` (#158) uses so operators see one shape of error
    /// across the management surface.
    async fn acquire_ref_lock(&self, op: &'static str) -> Result<(String, LockGuard), ManageError> {
        let ref_name = self.validated_ref_name()?;
        let prefix_opt = self.prefix_opt();
        let lock_key = lock_key(prefix_opt, &ref_name);
        let ttl = lock_ttl_from_env();
        let now = OffsetDateTime::now_utc();
        let Some(guard) = acquire_lock(Arc::clone(&self.store), &lock_key, ttl, now).await? else {
            warn!(
                branch = %self.branch,
                op = op,
                key = %lock_key,
                "{op}: per-ref lock is held by another writer; refusing to race",
            );
            return Err(ManageError::LockContended {
                branch: self.branch.clone(),
                lock: lock_key,
                ttl_seconds: ttl.whole_seconds(),
            });
        };
        Ok((lock_key, guard))
    }

    /// Release a previously acquired lock, downgrading release failures
    /// to a `warn!` so the caller's primary error (or success) is what
    /// surfaces. The lock's TTL recovers a leaked key on the next
    /// acquirer (#150), so the worst case is a delayed retry rather
    /// than a permanently stuck ref.
    async fn release_or_warn(&self, guard: LockGuard, lock_key: &str, op: &'static str) {
        if let Err(e) = release_lock(guard).await {
            warn!(
                branch = %self.branch,
                op = op,
                key = %lock_key,
                error = %e,
                "{op}: failed to release per-ref lock; will age out by TTL",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manage::{Prompter, ScriptedPrompter, scripted::Answer};
    use crate::object_store::mock::MockStore;
    use crate::packchain::gc::baseline_tombstone_listing_prefix;
    use bytes::Bytes;

    fn seed_with_branch(branch: &str) -> MockStore {
        let mock = MockStore::new();
        mock.insert(
            format!("myrepo/refs/heads/{branch}/abc.bundle"),
            Bytes::from("body"),
        );
        mock
    }

    #[tokio::test]
    async fn open_returns_branch_not_found_when_empty() {
        let mock = MockStore::new();
        let store: Arc<dyn ObjectStore> = Arc::new(mock);
        let prompter = ScriptedPrompter::new([]);
        match ManageBranch::open(store, "myrepo", "missing", &prompter).await {
            Err(ManageError::BranchNotFound(name)) => assert_eq!(name, "missing"),
            Err(other) => panic!("expected BranchNotFound, got {other:?}"),
            Ok(_) => panic!("expected open to fail"),
        }
    }

    #[tokio::test]
    async fn delete_removes_every_key_when_confirmed() {
        // No PROTECTED# marker — only the bundle. A confirmed delete
        // must clear it AND release the per-ref lock it acquires
        // (#158), leaving the bucket empty.
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.delete().await.expect("delete");
        assert!(
            mock.keys().is_empty(),
            "all keys removed (including the LOCK#.lock that delete-branch acquired and released): {:?}",
            mock.keys()
        );
        assert_eq!(prompter.remaining(), 0);
    }

    #[tokio::test]
    async fn delete_refuses_when_protected_marker_present() {
        // `protect` then `delete-branch` must refuse — same wording the
        // helper-protocol delete path emits. The prompt is never reached,
        // so the script queues no answer; the marker and bundle survive.
        let mock = seed_with_branch("main");
        mock.insert("myrepo/refs/heads/main/PROTECTED#", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let err = mb
            .delete()
            .await
            .expect_err("delete must refuse when PROTECTED# is present");
        match &err {
            ManageError::Protected(name) => assert_eq!(name, "main"),
            other => panic!("expected ManageError::Protected, got {other:?}"),
        }
        assert!(
            err.to_string()
                .contains("git-remote-object-store unprotect"),
            "error message must point at unprotect, got: {err}",
        );
        assert!(mock.contains("myrepo/refs/heads/main/PROTECTED#"));
        assert!(mock.contains("myrepo/refs/heads/main/abc.bundle"));
        // Prompt must not have been consumed.
        assert_eq!(prompter.remaining(), 0);
    }

    #[tokio::test]
    async fn delete_succeeds_after_unprotect_clears_marker() {
        // Protect, then unprotect, then delete — the canonical recovery
        // path. The final delete must remove every remaining key.
        let mock = seed_with_branch("main");
        mock.insert("myrepo/refs/heads/main/PROTECTED#", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.unprotect().await.expect("unprotect");
        mb.delete().await.expect("delete after unprotect");
        assert!(
            mock.keys().is_empty(),
            "all keys removed after unprotect+delete: {:?}",
            mock.keys()
        );
    }

    #[tokio::test]
    async fn delete_no_keeps_keys() {
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(false)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.delete().await.expect("delete (aborted)");
        assert_eq!(mock.keys().len(), 1, "branch still present");
    }

    #[tokio::test]
    async fn protect_creates_marker_idempotent() {
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.protect().await.expect("protect");
        assert!(mock.contains("myrepo/refs/heads/main/PROTECTED#"));
        // Second call overwrites without error.
        mb.protect().await.expect("protect again");
        assert!(mock.contains("myrepo/refs/heads/main/PROTECTED#"));
    }

    #[tokio::test]
    async fn protect_refuses_when_branch_deleted_between_open_and_protect() {
        // Issue #137: TOCTOU between `open` (which lists to verify the
        // branch exists) and `protect` (which writes the marker). A
        // concurrent `delete-branch` or last-bundle removal lands
        // between the two calls. Pre-fix, `protect` wrote a marker for
        // a non-existent branch — an orphaned `PROTECTED#` that never
        // gets cleaned up and silently blocks a future recreation of
        // the same branch. The fix re-lists immediately before the put
        // and refuses with BranchNotFound if the branch is gone.
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        // Simulate a concurrent delete sweeping every key under the
        // branch prefix after `open` returned but before `protect` runs.
        for key in mock.keys() {
            if key.starts_with("myrepo/refs/heads/main/") {
                let _ = mock.remove_key(&key);
            }
        }
        let err = mb
            .protect()
            .await
            .expect_err("protect must refuse against a concurrently-deleted branch");
        match &err {
            ManageError::BranchNotFound(name) => assert_eq!(name, "main"),
            other => panic!("expected BranchNotFound, got {other:?}"),
        }
        // The orphaned marker must NOT have been written — that is the
        // exact regression #137 fixes.
        assert!(
            !mock.contains("myrepo/refs/heads/main/PROTECTED#"),
            "orphaned PROTECTED# must not be written when branch is gone",
        );
        assert!(
            mock.keys().is_empty(),
            "store remains empty: {:?}",
            mock.keys()
        );
    }

    #[tokio::test]
    async fn protect_refuses_when_only_stale_lock_key_remains() {
        // A `LOCK#.lock` key is operational metadata, not branch data.
        // Treating a lock-only listing as "branch exists" would let a
        // `protect` write a marker for a branch that has no bundles —
        // the same orphan-marker pathology #137 describes, just with a
        // lock as the misleading residue instead of an empty listing.
        //
        // The lock is seeded stale (older than TTL) so #159's lock
        // acquisition recovers it rather than reporting contention —
        // otherwise we would assert the wrong error. The data-presence
        // re-check then runs and refuses the orphan write.
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("body"));
        let stale = OffsetDateTime::now_utc() - time::Duration::days(1);
        mock.insert_with(
            "myrepo/refs/heads/main/LOCK#.lock",
            Bytes::new(),
            stale,
            PutOpts::default(),
        );
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        // Concurrent push-then-delete leaves only the lock behind.
        let _ = mock.remove_key("myrepo/refs/heads/main/abc.bundle");
        let err = mb
            .protect()
            .await
            .expect_err("protect must refuse when only a lock key remains");
        assert!(
            matches!(err, ManageError::BranchNotFound(ref name) if name == "main"),
            "expected BranchNotFound, got {err:?}",
        );
        assert!(!mock.contains("myrepo/refs/heads/main/PROTECTED#"));
        // protect must recover the stale lock AND release the fresh one
        // it acquired. A regression that leaked the lock would still
        // pass the BranchNotFound assertion above.
        assert!(
            !mock.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "stale lock must be recovered and the acquired lock released",
        );
    }

    #[tokio::test]
    async fn protect_remains_idempotent_when_marker_already_present() {
        // The pre-existing marker plus a real bundle means the branch
        // is alive. `protect` must still succeed (idempotent overwrite)
        // — the data-presence check must not regress to "any marker
        // means orphan" and refuse a legitimate re-protect.
        let mock = seed_with_branch("main");
        mock.insert("myrepo/refs/heads/main/PROTECTED#", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.protect()
            .await
            .expect("protect must remain idempotent over an existing marker");
        assert!(mock.contains("myrepo/refs/heads/main/PROTECTED#"));
        assert!(mock.contains("myrepo/refs/heads/main/abc.bundle"));
    }

    #[tokio::test]
    async fn protect_into_writes_operator_message_to_writer() {
        // Mirror the delete_into pattern (#145): the writer-injecting
        // variant must emit the operator-visible message through `out`,
        // not via stdout. A regression that dropped the message — or
        // emitted it on stdout instead of the writer — would slip past
        // any test calling `protect()` because that wraps stdout.
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);
        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let mut out: Vec<u8> = Vec::new();
        mb.protect_into(&mut out).await.expect("protect_into");
        let captured = String::from_utf8(out).expect("utf8");
        assert!(
            captured.contains("Branch main is now protected"),
            "protect_into must emit the operator message; got: {captured:?}",
        );
    }

    #[tokio::test]
    async fn unprotect_into_writes_operator_message_to_writer() {
        let mock = seed_with_branch("main");
        mock.insert("myrepo/refs/heads/main/PROTECTED#", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);
        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let mut out: Vec<u8> = Vec::new();
        mb.unprotect_into(&mut out).await.expect("unprotect_into");
        let captured = String::from_utf8(out).expect("utf8");
        assert!(
            captured.contains("Branch main is now unprotected"),
            "unprotect_into must emit the operator message; got: {captured:?}",
        );
    }

    #[tokio::test]
    async fn unprotect_deletes_marker_when_present() {
        let mock = seed_with_branch("main");
        mock.insert("myrepo/refs/heads/main/PROTECTED#", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.unprotect().await.expect("unprotect");
        assert!(!mock.contains("myrepo/refs/heads/main/PROTECTED#"));
    }

    #[tokio::test]
    async fn unprotect_idempotent_when_marker_absent() {
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock);
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.unprotect()
            .await
            .expect("unprotect should be idempotent");
    }

    #[tokio::test]
    async fn open_rejects_invalid_branch_name() {
        // Attempting `delete-branch foo/../bar` would otherwise build
        // literal `<prefix>/refs/heads/foo/../bar/...` keys on S3.
        let mock = MockStore::new();
        let store: Arc<dyn ObjectStore> = Arc::new(mock);
        let prompter = ScriptedPrompter::new([]);
        match ManageBranch::open(store, "myrepo", "foo/../bar", &prompter).await {
            Err(ManageError::InvalidBranch(name)) => assert_eq!(name, "foo/../bar"),
            Err(other) => panic!("expected InvalidBranch, got {other:?}"),
            Ok(_) => panic!("expected open to reject `foo/../bar`"),
        }
    }

    #[tokio::test]
    async fn open_rejects_branch_with_control_char() {
        let mock = MockStore::new();
        let store: Arc<dyn ObjectStore> = Arc::new(mock);
        let prompter = ScriptedPrompter::new([]);
        match ManageBranch::open(store, "myrepo", "main\nrefs/heads/other", &prompter).await {
            Err(ManageError::InvalidBranch(_)) => {}
            Err(other) => panic!("expected InvalidBranch, got {other:?}"),
            Ok(_) => panic!("expected open to reject control-char branch"),
        }
    }

    #[tokio::test]
    async fn delete_partial_failure_continues_and_returns_structured_error() {
        // Issue #122: pre-fix, `delete` short-circuited on the first
        // per-key error, leaving the later keys untouched and the
        // operator with no inventory of what survived. The fix is to
        // collect failures, continue past each, and return a structured
        // `PartialDelete` naming exactly the keys that remain.
        //
        // `MockStore::list` returns keys in lexicographic (BTreeMap)
        // order. The loop deletes aaa, attempts bbb (armed to fail
        // transiently), and must still attempt ccc. Post-fix: aaa and
        // ccc are gone, bbb remains, the error names bbb explicitly.
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/aaa.bundle", Bytes::from("a"));
        mock.insert("myrepo/refs/heads/main/bbb.bundle", Bytes::from("b"));
        mock.insert("myrepo/refs/heads/main/ccc.bundle", Bytes::from("c"));
        mock.arm(crate::object_store::mock::Fault::NetworkOnDelete {
            key: "myrepo/refs/heads/main/bbb.bundle".to_owned(),
        });
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(
            Arc::clone(&store),
            "myrepo",
            "main",
            &prompter as &dyn Prompter,
        )
        .await
        .expect("open");
        let err = mb
            .delete()
            .await
            .expect_err("partial delete must surface PartialDelete");
        match &err {
            ManageError::PartialDelete {
                branch,
                undeleted,
                attempted,
            } => {
                assert_eq!(branch, "main");
                assert_eq!(*attempted, 3);
                assert_eq!(
                    undeleted.as_slice(),
                    ["myrepo/refs/heads/main/bbb.bundle"],
                    "undeleted list must name exactly the failed key",
                );
            }
            other => panic!("expected PartialDelete, got {other:?}"),
        }
        // The error message must name the failed key so a copy-paste
        // retry tool (or human) can act on it.
        let rendered = err.to_string();
        assert!(
            rendered.contains("myrepo/refs/heads/main/bbb.bundle"),
            "error message must name surviving key, got: {rendered}",
        );
        assert!(
            rendered.contains("retry to converge"),
            "error message must point at the retry path, got: {rendered}",
        );
        assert!(
            rendered.contains("1 of 3"),
            "render should pin the count framing, got: {rendered}",
        );
        // The loop did NOT short-circuit on bbb — aaa AND ccc are
        // both gone, and only bbb survives.
        assert!(!mock.contains("myrepo/refs/heads/main/aaa.bundle"));
        assert!(mock.contains("myrepo/refs/heads/main/bbb.bundle"));
        assert!(!mock.contains("myrepo/refs/heads/main/ccc.bundle"));
        assert_eq!(mock.pending_faults(), 0);

        // Retry-converges: clear nothing extra (the fault is already
        // consumed) and run delete again. The fresh listing inside
        // `delete` will only show bbb; the loop deletes it; the branch
        // is now fully gone.
        let prompter2 = ScriptedPrompter::new([Answer::Confirm(true)]);
        let mb2 = ManageBranch::open(store, "myrepo", "main", &prompter2 as &dyn Prompter)
            .await
            .expect("re-open after partial delete");
        mb2.delete().await.expect("retry must converge to Ok");
        assert!(
            mock.keys().is_empty(),
            "retry must remove the surviving key: {:?}",
            mock.keys(),
        );
    }

    #[tokio::test]
    async fn delete_partial_failure_attempts_every_key_in_listing() {
        // Issue #122 explicit four-key case: a transient failure on
        // key #2 of a 4-key listing must not stop the loop from
        // attempting keys #3 and #4. Pre-fix, this seeded with key
        // names a-d, fault on bbb, would leave bbb/ccc/ddd standing.
        // Post-fix, only bbb survives (the named failure).
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/aaa.bundle", Bytes::from("a"));
        mock.insert("myrepo/refs/heads/main/bbb.bundle", Bytes::from("b"));
        mock.insert("myrepo/refs/heads/main/ccc.bundle", Bytes::from("c"));
        mock.insert("myrepo/refs/heads/main/ddd.bundle", Bytes::from("d"));
        mock.arm(crate::object_store::mock::Fault::NetworkOnDelete {
            key: "myrepo/refs/heads/main/bbb.bundle".to_owned(),
        });
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let err = mb.delete().await.expect_err("partial delete expected");
        match err {
            ManageError::PartialDelete {
                undeleted,
                attempted,
                ..
            } => {
                assert_eq!(attempted, 4, "loop must visit every listed key");
                assert_eq!(undeleted.as_slice(), ["myrepo/refs/heads/main/bbb.bundle"]);
            }
            other => panic!("expected PartialDelete, got {other:?}"),
        }
        // Keys #1, #3, #4 were all attempted and succeeded; only the
        // named failure key survives.
        assert!(!mock.contains("myrepo/refs/heads/main/aaa.bundle"));
        assert!(mock.contains("myrepo/refs/heads/main/bbb.bundle"));
        assert!(!mock.contains("myrepo/refs/heads/main/ccc.bundle"));
        assert!(!mock.contains("myrepo/refs/heads/main/ddd.bundle"));
    }

    #[tokio::test]
    async fn delete_all_keys_fail_returns_full_inventory() {
        // Two faults arm against two of the three keys, plus a third
        // standalone failure. We assert that PartialDelete lists every
        // surviving key in lexicographic order so an operator (or
        // tooling that reads the structured field) gets a complete
        // inventory rather than just the first failure.
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/aaa.bundle", Bytes::from("a"));
        mock.insert("myrepo/refs/heads/main/bbb.bundle", Bytes::from("b"));
        mock.insert("myrepo/refs/heads/main/ccc.bundle", Bytes::from("c"));
        for key in [
            "myrepo/refs/heads/main/aaa.bundle",
            "myrepo/refs/heads/main/bbb.bundle",
            "myrepo/refs/heads/main/ccc.bundle",
        ] {
            mock.arm(crate::object_store::mock::Fault::NetworkOnDelete {
                key: key.to_owned(),
            });
        }
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let err = mb.delete().await.expect_err("all-fail must surface error");
        match err {
            ManageError::PartialDelete {
                undeleted,
                attempted,
                ..
            } => {
                assert_eq!(attempted, 3);
                assert_eq!(
                    undeleted,
                    vec![
                        "myrepo/refs/heads/main/aaa.bundle".to_owned(),
                        "myrepo/refs/heads/main/bbb.bundle".to_owned(),
                        "myrepo/refs/heads/main/ccc.bundle".to_owned(),
                    ],
                    "every surviving key must be reported, in listing order",
                );
            }
            other => panic!("expected PartialDelete, got {other:?}"),
        }
        // All three originals survive — nothing was deleted.
        assert_eq!(mock.keys().len(), 3);
    }

    #[tokio::test]
    async fn delete_mixed_notfound_and_failure_only_lists_real_failures() {
        // NotFound mid-sweep is tolerated (#139). The PartialDelete
        // inventory must NOT include keys that the listing showed but
        // that a concurrent sweeper had already removed — those are
        // success from the operator's POV. Only the genuine network
        // failure on bbb should be in `undeleted`.
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/aaa.bundle", Bytes::from("a"));
        mock.insert("myrepo/refs/heads/main/bbb.bundle", Bytes::from("b"));
        mock.insert("myrepo/refs/heads/main/ccc.bundle", Bytes::from("c"));
        // aaa raced and is gone; bbb is a genuine network failure; ccc
        // succeeds normally.
        mock.arm(crate::object_store::mock::Fault::NotFoundOnDelete {
            key: "myrepo/refs/heads/main/aaa.bundle".to_owned(),
        });
        mock.arm(crate::object_store::mock::Fault::NetworkOnDelete {
            key: "myrepo/refs/heads/main/bbb.bundle".to_owned(),
        });
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let err = mb.delete().await.expect_err("bbb failure must surface");
        match err {
            ManageError::PartialDelete {
                undeleted,
                attempted,
                ..
            } => {
                assert_eq!(attempted, 3);
                assert_eq!(
                    undeleted.as_slice(),
                    ["myrepo/refs/heads/main/bbb.bundle"],
                    "only the genuine non-NotFound failure must appear",
                );
            }
            other => panic!("expected PartialDelete, got {other:?}"),
        }
        // ccc was deleted by the loop. bbb survives. aaa's NotFound
        // fault short-circuited its delete BEFORE the actual removal,
        // so the body is still in the mock — same observable as the
        // pre-existing `delete_tolerates_notfound_mid_sweep` test.
        assert!(!mock.contains("myrepo/refs/heads/main/ccc.bundle"));
        assert!(mock.contains("myrepo/refs/heads/main/bbb.bundle"));
    }

    /// Prompter that performs a side effect against a [`MockStore`]
    /// before replying to `confirm`, simulating a concurrent operation
    /// landing during the user's prompt window. Each call consumes one
    /// queued `(action, answer)` pair; running dry returns
    /// [`ManageError::Cancelled`] so an under-armed script fails loudly.
    struct ConcurrentPrompter {
        store: MockStore,
        actions: std::sync::Mutex<std::collections::VecDeque<(ConcurrentAction, bool)>>,
    }

    enum ConcurrentAction {
        /// Insert `(key, body)` into the store.
        Insert(String, Bytes),
        /// Insert multiple `(key, body)` pairs in one prompt window —
        /// used to model an interleaved `git push` + `protect` race
        /// against a single user prompt (#131).
        InsertMany(Vec<(String, Bytes)>),
        /// Delete every key currently under `prefix` (simulates a
        /// concurrent `delete-branch` winning the race).
        DeleteAllUnder(String),
    }

    impl ConcurrentPrompter {
        fn new(
            store: MockStore,
            actions: impl IntoIterator<Item = (ConcurrentAction, bool)>,
        ) -> Self {
            Self {
                store,
                actions: std::sync::Mutex::new(actions.into_iter().collect()),
            }
        }
    }

    impl Prompter for ConcurrentPrompter {
        fn select(&self, _prompt: &str, _options: &[String]) -> Result<usize, ManageError> {
            panic!("ConcurrentPrompter does not expect select");
        }

        fn confirm(&self, _prompt: &str) -> Result<bool, ManageError> {
            let (action, answer) = self
                .actions
                .lock()
                .expect("concurrent mutex poisoned")
                .pop_front()
                .ok_or(ManageError::Cancelled)?;
            match action {
                ConcurrentAction::Insert(key, body) => self.store.insert(key, body),
                ConcurrentAction::InsertMany(pairs) => {
                    for (key, body) in pairs {
                        self.store.insert(key, body);
                    }
                }
                ConcurrentAction::DeleteAllUnder(prefix) => {
                    for key in self.store.keys() {
                        if key.starts_with(&prefix) {
                            let _ = self.store.remove_key(&key);
                        }
                    }
                }
            }
            Ok(answer)
        }
    }

    #[tokio::test]
    async fn delete_sweeps_objects_added_during_prompt() {
        // Issue #139: a concurrent push lands a new bundle key between
        // the initial LIST and the deletion loop. Pre-fix, that key was
        // not in the captured listing and survived the "successful"
        // delete. The fix re-lists after the prompt, so the new key is
        // included in the sweep.
        let mock = seed_with_branch("main");
        let new_key = "myrepo/refs/heads/main/concurrent.bundle".to_owned();
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ConcurrentPrompter::new(
            mock.clone(),
            [(
                ConcurrentAction::Insert(new_key.clone(), Bytes::from("racing body")),
                true,
            )],
        );

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.delete()
            .await
            .expect("delete must include concurrently-added key");
        assert!(
            mock.keys().is_empty(),
            "fresh listing must drive sweep; zombie keys remaining: {:?}",
            mock.keys(),
        );
        assert!(
            !mock.contains(&new_key),
            "concurrently-added bundle must be deleted, not left as a zombie",
        );
    }

    #[tokio::test]
    async fn delete_refuses_when_marker_lands_during_prompt() {
        // Initial listing has no PROTECTED# marker, so the protection
        // check passes and the prompt fires. A concurrent `protect`
        // lands during the prompt, then the user answers "yes". The
        // fresh-listing protection check must catch the marker and
        // refuse — otherwise the operator silently bulldozes a ref that
        // was just protected.
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ConcurrentPrompter::new(
            mock.clone(),
            [(
                ConcurrentAction::Insert(
                    "myrepo/refs/heads/main/PROTECTED#".to_owned(),
                    Bytes::new(),
                ),
                true,
            )],
        );

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let err = mb
            .delete()
            .await
            .expect_err("delete must refuse marker that landed during prompt");
        assert!(
            matches!(err, ManageError::Protected(ref name) if name == "main"),
            "expected Protected, got {err:?}",
        );
        // Both the marker and the original bundle survive.
        assert!(mock.contains("myrepo/refs/heads/main/PROTECTED#"));
        assert!(mock.contains("myrepo/refs/heads/main/abc.bundle"));
    }

    #[tokio::test]
    async fn issue_131_protect_during_prompt_blocks_delete_even_with_concurrent_push() {
        // Issue #131 regression: TOCTOU between the initial protection
        // check and the deletion loop. This pins the specific scenario
        // where a `protect` lands DURING the user prompt — distinct from
        // #139's pure-push race. The combined push+protect interleaving
        // here proves two things about the post-prompt re-check:
        //
        //   1. The marker check fires on the FRESH listing, not the
        //      stale initial listing (otherwise the marker is missed
        //      because it didn't exist when `delete` started).
        //   2. The marker check takes precedence over the sweep even
        //      when other concurrent activity (a racing push) would
        //      otherwise look "successful" — the operator must not
        //      silently bulldoze a freshly-protected ref just because
        //      the listing also grew.
        //
        // Pre-#139 the marker check was only on the initial listing, so
        // both concurrent writes were ignored and the original bundle
        // was deleted. The fix re-lists after the prompt and re-checks
        // for the marker, refusing the delete entirely.
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ConcurrentPrompter::new(
            mock.clone(),
            [(
                ConcurrentAction::InsertMany(vec![
                    ("myrepo/refs/heads/main/PROTECTED#".to_owned(), Bytes::new()),
                    (
                        "myrepo/refs/heads/main/racing-push.bundle".to_owned(),
                        Bytes::from("pushed during prompt"),
                    ),
                ]),
                true,
            )],
        );

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let err = mb
            .delete()
            .await
            .expect_err("delete must refuse marker even when push also raced");
        assert!(
            matches!(err, ManageError::Protected(ref name) if name == "main"),
            "expected Protected (post-prompt re-check), got {err:?}",
        );
        // The marker, the racing push, and the original bundle all
        // survive — refusal is total, not partial.
        assert!(mock.contains("myrepo/refs/heads/main/PROTECTED#"));
        assert!(mock.contains("myrepo/refs/heads/main/racing-push.bundle"));
        assert!(mock.contains("myrepo/refs/heads/main/abc.bundle"));
    }

    #[tokio::test]
    async fn delete_handles_empty_initial_listing_when_branch_swept_between_open_and_delete() {
        // Distinct from the prompt-window race
        // (`delete_reports_already_gone_on_concurrent_delete_race`):
        // here the branch is swept BETWEEN `open()` succeeding (data
        // existed at open time) and the FIRST listing inside
        // `delete()`. The function must handle the empty-initial-list
        // path without panicking, without spuriously claiming success,
        // and without surfacing an unexpected error variant. The fresh
        // re-listing inside `delete()` is also empty, so the
        // "already gone" branch fires and the function returns Ok(()).
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        // One confirm answer queued: the current implementation does
        // NOT short-circuit on an empty INITIAL listing — it falls
        // through to the prompt (the operator may want to confirm a
        // "0 objects" delete) and only the empty FRESH listing path
        // (post-prompt) returns Ok(()). Queuing a single `true`
        // exercises that exact path.
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open succeeds while branch data is still present");

        // Sweep every key under the branch between open() and delete().
        // Mirrors a concurrent `delete-branch` or last-bundle removal
        // that ran while the caller was still holding the open handle.
        for key in mock.keys() {
            if key.starts_with("myrepo/refs/heads/main/") {
                let _ = mock.remove_key(&key);
            }
        }
        assert!(
            mock.keys().is_empty(),
            "pre-condition: branch must be fully swept before delete()",
        );

        mb.delete()
            .await
            .expect("delete() must handle an empty initial listing without error");
        assert!(
            mock.keys().is_empty(),
            "delete() against an already-empty branch must not resurrect any key",
        );
    }

    #[tokio::test]
    async fn delete_reports_already_gone_on_concurrent_delete_race() {
        // A concurrent `delete-branch` (or last-bundle removal) clears
        // every object under the branch prefix during the prompt
        // window. The fresh listing is empty; the function must report
        // the race and return Ok(()), not claim success without doing
        // anything.
        //
        // The store-state asserts here are intentionally weak: the
        // ConcurrentPrompter side effect already cleared the store
        // before `delete()` resumed from the prompt, so `keys()` is
        // empty regardless of which production branch was taken
        // (#145). The load-bearing assert is the captured-stdout
        // substring — without the "already gone" notice the operator
        // cannot tell a successful delete from a no-op race-loss.
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ConcurrentPrompter::new(
            mock.clone(),
            [(
                ConcurrentAction::DeleteAllUnder("myrepo/refs/heads/main/".to_owned()),
                true,
            )],
        );

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let mut out: Vec<u8> = Vec::new();
        mb.delete_into(&mut out)
            .await
            .expect("empty fresh listing must return Ok, not silent success");
        let captured = String::from_utf8(out).expect("captured output must be UTF-8");
        assert!(
            captured.contains("is already gone"),
            "operator message must announce the concurrent race; got: {captured:?}",
        );
        assert!(
            !captured.contains("has been deleted"),
            "must not claim a successful delete when nothing was swept; got: {captured:?}",
        );
        assert!(mock.keys().is_empty(), "store remains empty");
    }

    #[tokio::test]
    async fn delete_tolerates_notfound_mid_sweep() {
        // A concurrent sweeper races between our fresh listing and a
        // per-key delete: the listing still reports `bbb`, but by the
        // time `delete(bbb)` fires the key is gone. Pre-fix, the
        // ObjectStoreError::NotFound surfaced as ManageError::Store and
        // aborted the sweep mid-flight. The fix tolerates NotFound in
        // the loop so a partial concurrent delete doesn't leave the
        // rest of the branch standing.
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/aaa.bundle", Bytes::from("a"));
        mock.insert("myrepo/refs/heads/main/bbb.bundle", Bytes::from("b"));
        mock.insert("myrepo/refs/heads/main/ccc.bundle", Bytes::from("c"));
        mock.arm(crate::object_store::mock::Fault::NotFoundOnDelete {
            key: "myrepo/refs/heads/main/bbb.bundle".to_owned(),
        });
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);
        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.delete()
            .await
            .expect("NotFound mid-sweep must not abort the loop");
        // aaa and ccc are deleted; the NotFound fault on bbb is
        // tolerated and the fault is consumed (the body remains because
        // the fault fired BEFORE the actual removal).
        assert!(!mock.contains("myrepo/refs/heads/main/aaa.bundle"));
        assert!(!mock.contains("myrepo/refs/heads/main/ccc.bundle"));
        // bbb's body is still present because the fault short-circuited
        // the delete with NotFound before removal. In production the
        // analogous case is a concurrent sweeper that ALREADY removed
        // it — same observable: key gone or not, the loop continues.
        assert_eq!(mock.pending_faults(), 0);
    }

    // --- Root-of-bucket (empty prefix) coverage --------------------------

    #[tokio::test]
    async fn root_prefix_delete_removes_keys_without_leading_slash() {
        // Repo lives at the bucket root: keys have no `<prefix>/`
        // segment. A leading-slash regression here would surface as
        // `BranchNotFound` (the list of `/refs/heads/main/` returns
        // nothing) or as a delete that fails to match the real keys.
        // No PROTECTED# marker is seeded — protected-ref refusal is
        // covered separately by
        // `root_prefix_delete_refuses_when_protected_marker_present`.
        // The `LOCK#.lock` is created and removed by `delete`'s own
        // acquire/release tail (#158) — pre-seeding a fresh lock here
        // would (correctly) surface as `LockContended`.
        let mock = MockStore::new();
        mock.insert("refs/heads/main/abc.bundle", Bytes::from("body"));
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "", "main", &prompter as &dyn Prompter)
            .await
            .expect("open at root");
        mb.delete().await.expect("delete at root");
        assert!(mock.keys().is_empty(), "all root keys removed");
    }

    #[tokio::test]
    async fn root_prefix_delete_refuses_when_protected_marker_present() {
        // Root-of-bucket layout (no `<prefix>/` segment) must use the
        // same final-segment match the helper-protocol delete path uses;
        // a substring-only check could miss the unprefixed marker key.
        let mock = MockStore::new();
        mock.insert("refs/heads/main/abc.bundle", Bytes::from("body"));
        mock.insert("refs/heads/main/PROTECTED#", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "", "main", &prompter as &dyn Prompter)
            .await
            .expect("open at root");
        let err = mb
            .delete()
            .await
            .expect_err("delete at root must refuse PROTECTED#");
        assert!(
            matches!(err, ManageError::Protected(ref name) if name == "main"),
            "expected ManageError::Protected, got {err:?}",
        );
        assert!(mock.contains("refs/heads/main/PROTECTED#"));
        assert!(mock.contains("refs/heads/main/abc.bundle"));
    }

    #[tokio::test]
    async fn root_prefix_protect_writes_marker_at_root_layout() {
        let mock = MockStore::new();
        mock.insert("refs/heads/main/abc.bundle", Bytes::from("body"));
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "", "main", &prompter as &dyn Prompter)
            .await
            .expect("open at root");
        mb.protect().await.expect("protect at root");
        // Root-of-bucket layout: no leading slash, no synthetic prefix.
        assert!(mock.contains("refs/heads/main/PROTECTED#"));
        assert!(!mock.contains("/refs/heads/main/PROTECTED#"));
    }

    #[tokio::test]
    async fn root_prefix_unprotect_removes_marker_at_root_layout() {
        let mock = MockStore::new();
        mock.insert("refs/heads/main/abc.bundle", Bytes::from("body"));
        mock.insert("refs/heads/main/PROTECTED#", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "", "main", &prompter as &dyn Prompter)
            .await
            .expect("open at root");
        mb.unprotect().await.expect("unprotect at root");
        assert!(!mock.contains("refs/heads/main/PROTECTED#"));
        // The bundle alongside the marker must survive — `unprotect` is
        // a marker-only delete and a regression that broadened the
        // delete scope would leave the bundle missing.
        assert!(mock.contains("refs/heads/main/abc.bundle"));
    }

    #[tokio::test]
    async fn root_prefix_open_reports_branch_not_found_for_missing_branch() {
        let mock = MockStore::new();
        let store: Arc<dyn ObjectStore> = Arc::new(mock);
        let prompter = ScriptedPrompter::new([]);
        match ManageBranch::open(store, "", "missing", &prompter).await {
            Err(ManageError::BranchNotFound(name)) => assert_eq!(name, "missing"),
            Err(other) => panic!("expected BranchNotFound, got {other:?}"),
            Ok(_) => panic!("expected open at root to fail with BranchNotFound"),
        }
    }

    // --- Baseline-bundle tombstone on delete-branch (#143) ---------------

    /// SHA used as `<full_at>` in the seeded `chain.json` for the
    /// tombstone tests below. The exact value is irrelevant — the
    /// tests assert that whatever SHA the chain names is the SHA the
    /// tombstone references and the SHA whose `<sha>.bundle` survives
    /// the synchronous sweep.
    const TOMBSTONE_TEST_FULL_AT: &str = "0123456789abcdef0123456789abcdef01234567";

    /// Seed a packchain-style branch at `<prefix>/refs/heads/<branch>/`
    /// with a baseline bundle, a `chain.json` naming that bundle as
    /// `full_at`, and a `path-index.json`. Returns the bundle's
    /// full key so tests can pin survival/deletion against the
    /// exact byte string.
    async fn seed_packchain_branch(
        store: &crate::object_store::mock::MockStore,
        prefix: &str,
        branch: &str,
    ) -> String {
        use crate::packchain::manifest::write_chain;
        use crate::packchain::schema::{ChainManifest, ChainSegment, Sha40};

        let ref_name = RefName::new(format!("refs/heads/{branch}")).unwrap();
        let prefix_opt = (!prefix.is_empty()).then_some(prefix);
        let full_at = Sha40::try_new(TOMBSTONE_TEST_FULL_AT).unwrap();
        let chain = ChainManifest {
            v: 1,
            tip: full_at.clone(),
            full_at: full_at.clone(),
            segments: vec![ChainSegment {
                sha: full_at.clone(),
                parent_sha: None,
                pack: format!("packs/{TOMBSTONE_TEST_FULL_AT}.pack"),
                bytes: 1_024,
            }],
        };
        write_chain(store, prefix_opt, &ref_name, &chain)
            .await
            .unwrap();
        // path-index.json — written verbatim so we can assert the
        // synchronous sweep removes it alongside chain.json.
        let path_index_key = crate::packchain::keys::path_index_key(prefix_opt, &ref_name);
        store.insert(path_index_key, Bytes::from_static(b"{\"v\":1,\"root\":{}}"));
        let bundle_key = keys::bundle_key(prefix_opt, ref_name.as_str(), full_at.as_str());
        store.insert(bundle_key.clone(), Bytes::from_static(b"PACKBUNDLE"));
        bundle_key
    }

    #[tokio::test]
    async fn delete_writes_baseline_tombstone_and_defers_bundle() {
        // Issue #143: a packchain delete-branch must write a
        // `<prefix>/gc/baseline-tomb-*.json` tombstone naming the
        // current `full_at` SHA, and the synchronous sweep must
        // leave that bundle in place. chain.json and path-index.json
        // ARE removed synchronously — from a fresh reader's
        // perspective the ref is gone the moment the chain commits
        // its deletion; the bundle stays only so an in-flight
        // fetcher that already loaded the prior chain can finish.
        let mock = crate::object_store::mock::MockStore::new();
        let bundle_key = seed_packchain_branch(&mock, "repo", "main").await;
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "repo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.delete().await.expect("delete");

        // chain.json and path-index.json are deleted synchronously.
        assert!(
            !mock.contains("repo/refs/heads/main/chain.json"),
            "chain.json must be removed synchronously: {:?}",
            mock.keys(),
        );
        assert!(
            !mock.contains("repo/refs/heads/main/path-index.json"),
            "path-index.json must be removed synchronously: {:?}",
            mock.keys(),
        );
        // The baseline bundle is NOT removed — it is left for `gc sweep`.
        assert!(
            mock.contains(&bundle_key),
            "baseline bundle must survive synchronous delete: {:?}",
            mock.keys(),
        );
        // Exactly one baseline tombstone is written under
        // `<prefix>/gc/baseline-tomb-*.json`, and it names the
        // bundle's SHA. The shape (UUID-named JSON body) belongs to
        // `BaselineTombstone`; this test pins only the listing
        // prefix and the SHA inside, since the UUID is intentionally
        // non-deterministic.
        let tomb_keys: Vec<String> = mock
            .keys()
            .into_iter()
            .filter(|k| k.starts_with(&baseline_tombstone_listing_prefix(Some("repo"))))
            .collect();
        assert_eq!(
            tomb_keys.len(),
            1,
            "exactly one baseline tombstone must exist: {tomb_keys:?}",
        );
        let body = mock
            .get_bytes(&tomb_keys[0])
            .await
            .expect("tombstone body present");
        let parsed: serde_json::Value =
            serde_json::from_slice(&body).expect("tombstone is valid JSON");
        assert_eq!(parsed["v"], 1);
        assert_eq!(parsed["sha"], TOMBSTONE_TEST_FULL_AT);
        assert_eq!(parsed["ref_name"], "refs/heads/main");
    }

    #[tokio::test]
    async fn gc_sweep_after_grace_window_reclaims_deferred_bundle() {
        // Round-trip the #143 contract: write a tombstone via
        // delete-branch, then run `gc sweep --force` (skips the
        // grace window). The bundle must now be gone. This proves
        // the tombstone's body is shaped exactly the way the
        // existing sweep code expects — a regression in the
        // delete-branch tombstone shape would surface as a deferred
        // sweep step rather than a reclaim.
        let mock = crate::object_store::mock::MockStore::new();
        let bundle_key = seed_packchain_branch(&mock, "repo", "main").await;
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(
            Arc::clone(&store),
            "repo",
            "main",
            &prompter as &dyn Prompter,
        )
        .await
        .expect("open");
        mb.delete().await.expect("delete");
        // Pre-condition: bundle still present, tombstone written.
        assert!(mock.contains(&bundle_key));

        // `--force` skips the grace window. The sweep finds a
        // chain.json-less ref (delete-branch removed it) and so
        // proceeds with the bundle delete.
        let outcome = crate::packchain::gc::sweep(
            store.as_ref(),
            "repo",
            crate::packchain::gc::SweepOpts {
                grace_hours: 0,
                force: true,
            },
        )
        .await
        .expect("sweep");
        assert_eq!(
            outcome.swept_tombstones, 1,
            "sweep must reclaim exactly the tombstone delete-branch wrote",
        );
        assert!(
            !mock.contains(&bundle_key),
            "baseline bundle must be deleted by sweep: surviving keys = {:?}",
            mock.keys(),
        );
        // The tombstone itself is also gone after a successful sweep.
        let surviving_tombs: Vec<String> = mock
            .keys()
            .into_iter()
            .filter(|k| k.starts_with(&baseline_tombstone_listing_prefix(Some("repo"))))
            .collect();
        assert!(
            surviving_tombs.is_empty(),
            "tombstone must be deleted by sweep: {surviving_tombs:?}",
        );
    }

    #[tokio::test]
    async fn delete_bundle_engine_ref_with_no_chain_uses_immediate_delete() {
        // Bundle-engine refs (no `chain.json`) have no baseline to
        // tombstone. The function must fall through to the existing
        // immediate-delete path — no tombstone written, every key
        // swept synchronously. This guards against a regression that
        // would write a spurious tombstone naming a non-existent
        // SHA, or that would leave a bundle-engine ref's `.bundle`
        // standing.
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.delete().await.expect("delete");
        assert!(
            mock.keys().is_empty(),
            "bundle-engine ref must be fully swept synchronously: {:?}",
            mock.keys(),
        );
        // No baseline tombstone written.
        let tomb_keys: Vec<String> = mock
            .keys()
            .into_iter()
            .filter(|k| k.contains(crate::packchain::gc::BASELINE_TOMBSTONE_KEY_FRAGMENT))
            .collect();
        assert!(
            tomb_keys.is_empty(),
            "no tombstone must be written for a chain-less ref: {tomb_keys:?}",
        );
    }

    #[tokio::test]
    async fn delete_unparseable_chain_falls_back_to_synchronous_bundle_delete() {
        // A malformed `chain.json` (truncated, wrong schema version,
        // etc.) means `load_chain` fails. The delete path must NOT
        // block on this — the operator already confirmed the delete;
        // the ref is going away. The fallback is the existing
        // immediate-delete behaviour: sweep every key including any
        // bundles. Without the fallback an operator could be stuck
        // unable to delete a corrupted ref.
        let mock = crate::object_store::mock::MockStore::new();
        // Hand-craft an unparseable `chain.json` body (not JSON).
        mock.insert(
            "repo/refs/heads/main/chain.json",
            Bytes::from_static(b"not a json"),
        );
        // Seed a bundle whose name matches what a chain.json MIGHT
        // have pointed at — must still be swept synchronously since
        // we have no tombstone protection.
        let bundle_key = format!("repo/refs/heads/main/{TOMBSTONE_TEST_FULL_AT}.bundle");
        mock.insert(bundle_key.clone(), Bytes::from_static(b"BUNDLE"));
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "repo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.delete().await.expect("delete");

        assert!(
            mock.keys().is_empty(),
            "unparseable chain must fall back to immediate sweep: {:?}",
            mock.keys(),
        );
    }

    #[tokio::test]
    async fn delete_chain_pointing_at_missing_bundle_sweeps_remaining_keys() {
        // Pathological case: `chain.json` parses and names a
        // `full_at`, but the corresponding `<sha>.bundle` is NOT in
        // the fresh listing (already deleted, or never written).
        // `try_tombstone_baseline` returns None on this branch —
        // there is nothing to defer. The synchronous sweep must
        // still remove chain.json and any other residue.
        use crate::packchain::manifest::write_chain;
        use crate::packchain::schema::{ChainManifest, ChainSegment, Sha40};
        let mock = crate::object_store::mock::MockStore::new();
        // Seed chain.json + path-index.json but NOT the bundle.
        let ref_name = RefName::new("refs/heads/main").unwrap();
        let full_at = Sha40::try_new(TOMBSTONE_TEST_FULL_AT).unwrap();
        let chain = ChainManifest {
            v: 1,
            tip: full_at.clone(),
            full_at: full_at.clone(),
            segments: vec![ChainSegment {
                sha: full_at.clone(),
                parent_sha: None,
                pack: format!("packs/{TOMBSTONE_TEST_FULL_AT}.pack"),
                bytes: 1_024,
            }],
        };
        write_chain(&mock, Some("repo"), &ref_name, &chain)
            .await
            .unwrap();

        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "repo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.delete().await.expect("delete");

        // chain.json removed; no bundle was ever there; no
        // tombstone written (deferring nothing is pointless).
        assert!(
            !mock.contains("repo/refs/heads/main/chain.json"),
            "chain.json must be removed: {:?}",
            mock.keys(),
        );
        let tomb_keys: Vec<String> = mock
            .keys()
            .into_iter()
            .filter(|k| k.starts_with(&baseline_tombstone_listing_prefix(Some("repo"))))
            .collect();
        assert!(
            tomb_keys.is_empty(),
            "no tombstone for a chain whose bundle is already absent: {tomb_keys:?}",
        );
    }

    // --- Per-ref lock acquisition / release (#158) ----------------------

    #[tokio::test]
    async fn delete_refuses_when_per_ref_lock_is_held_by_another_writer() {
        // Issue #158: pre-fix, `delete-branch` performed a fresh
        // listing + sweep without taking the per-ref `LOCK#.lock`. A
        // concurrent `git push` that acquired the lock and started
        // uploading a new bundle after the listing would land that
        // bundle AFTER the delete sweep, leaving the ref alive while
        // delete-branch reported success.
        //
        // The fix takes the same lock the helper-protocol push and
        // delete paths take. This test seeds a FRESH lock (matching a
        // concurrent push holding it) and asserts that delete-branch
        // returns `LockContended` and makes NO changes — the bundle
        // and the lock both survive verbatim.
        let mock = seed_with_branch("main");
        // Fresh `last_modified` = now → `acquire_lock` sees
        // `age <= ttl` and reports contention (Ok(None)).
        mock.insert("myrepo/refs/heads/main/LOCK#.lock", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let err = mb
            .delete()
            .await
            .expect_err("delete must refuse to race a fresh lock holder");
        match &err {
            ManageError::LockContended {
                branch,
                lock,
                ttl_seconds,
            } => {
                assert_eq!(branch, "main");
                assert_eq!(lock, "myrepo/refs/heads/main/LOCK#.lock");
                assert!(
                    *ttl_seconds > 0,
                    "ttl_seconds must be positive, got {ttl_seconds}",
                );
            }
            other => panic!("expected LockContended, got {other:?}"),
        }
        // The operator-facing wording must name the lock key (so a
        // doctor invocation can copy it) and surface the TTL.
        let rendered = err.to_string();
        assert!(
            rendered.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "error must name the lock key, got: {rendered}",
        );
        assert!(
            rendered.contains("doctor"),
            "error must point operators at doctor, got: {rendered}",
        );
        // NOTHING was deleted: the bundle, the lock, and the prompt's
        // confirmation are all preserved. Pre-#158 this exact race
        // produced a "success" with the bundle missing.
        assert!(
            mock.contains("myrepo/refs/heads/main/abc.bundle"),
            "bundle must survive a contended-lock refusal",
        );
        assert!(
            mock.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "the racing writer's lock must NOT be deleted",
        );
    }

    #[tokio::test]
    async fn delete_recovers_stale_lock_and_proceeds() {
        // A `LOCK#.lock` older than the TTL means a previous writer
        // crashed before releasing it. `acquire_lock` recovers it by
        // deleting and re-acquiring. The delete must then complete
        // normally — refusing on a stale lock would let a crashed
        // writer block the bucket forever.
        //
        // This pins the staleness boundary by seeding a lock dated
        // well in the past (sufficient for any reasonable TTL up to
        // hours) and asserting both the sweep and the final release
        // ran.
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("body"));
        let stale = OffsetDateTime::now_utc() - time::Duration::days(1);
        mock.insert_with(
            "myrepo/refs/heads/main/LOCK#.lock",
            Bytes::new(),
            stale,
            PutOpts::default(),
        );
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.delete().await.expect("stale lock must be recovered");
        assert!(
            mock.keys().is_empty(),
            "bundle and lock both gone after stale-lock recovery + release: {:?}",
            mock.keys(),
        );
    }

    #[tokio::test]
    async fn delete_releases_lock_after_successful_sweep() {
        // A successful delete must clean up the LOCK#.lock it
        // acquired. Without release, a subsequent operation would
        // see a fresh (just-released) lock and report contention
        // until the TTL elapses — defeating the point of explicit
        // release.
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(
            Arc::clone(&store),
            "myrepo",
            "main",
            &prompter as &dyn Prompter,
        )
        .await
        .expect("open");
        mb.delete().await.expect("delete");
        assert!(
            !mock.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "lock must be released (deleted) after a successful sweep: {:?}",
            mock.keys(),
        );
    }

    #[tokio::test]
    async fn delete_releases_lock_even_when_sweep_returns_partial_delete() {
        // The lock must be released regardless of how the lock-held
        // body returned. A `PartialDelete` error means one per-key
        // delete failed; the lock is still released so a retry isn't
        // gated on TTL recovery.
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/aaa.bundle", Bytes::from("a"));
        mock.insert("myrepo/refs/heads/main/bbb.bundle", Bytes::from("b"));
        mock.arm(crate::object_store::mock::Fault::NetworkOnDelete {
            key: "myrepo/refs/heads/main/bbb.bundle".to_owned(),
        });
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let err = mb
            .delete()
            .await
            .expect_err("partial delete must still surface its error");
        assert!(matches!(err, ManageError::PartialDelete { .. }));
        assert!(
            !mock.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "lock must be released even when sweep returns PartialDelete: {:?}",
            mock.keys(),
        );
    }

    #[tokio::test]
    async fn delete_does_not_iterate_over_its_own_lock_key() {
        // Mirrors `protocol::push::delete_remote_ref_under_lock`'s
        // lock-key filter (#133): the fresh under-lock listing must
        // exclude the lock we hold so the sweep does not delete our
        // own coordination key mid-critical-section. The expression
        // of this guarantee in the test: a stale lock that the
        // acquire path recovered is replaced by OUR fresh lock; the
        // sweep must NOT report `attempted = 2` (the lock + the
        // bundle) but `attempted = 1` (the bundle only).
        //
        // Indirect proof: we arm a fault on the lock key. If the
        // sweep iterates over the lock, the fault fires and the
        // delete returns `PartialDelete { undeleted: [lock] }`. The
        // test asserts the fault is NOT consumed — the sweep skipped
        // the lock as expected.
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("body"));
        // Arm a fault on the lock key; if `delete` iterates over the
        // lock the fault fires.
        mock.arm(crate::object_store::mock::Fault::NetworkOnDelete {
            key: "myrepo/refs/heads/main/LOCK#.lock".to_owned(),
        });
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        // Note: `release_lock`'s delete also goes through the mock
        // and will trip the fault. We accept either outcome here —
        // the load-bearing assertion is that the sweep loop did not
        // iterate over the lock (which would have produced a
        // `PartialDelete`). The release-delete simply returns a
        // warn-logged error and is swallowed; the sweep success path
        // surfaces as `Ok(())`.
        let result = mb.delete().await;
        assert!(
            !matches!(result, Err(ManageError::PartialDelete { .. })),
            "sweep must not iterate over the lock key (no PartialDelete on the lock): {result:?}",
        );
        // The bundle was deleted by the sweep.
        assert!(
            !mock.contains("myrepo/refs/heads/main/abc.bundle"),
            "bundle must still be swept: {:?}",
            mock.keys(),
        );
    }

    #[tokio::test]
    async fn delete_release_failure_does_not_mask_sweep_success() {
        // Issue #158: release failures are downgraded to `warn!` —
        // the operator's "ref is gone" intent is satisfied as soon
        // as the sweep succeeds, and an orphan lock will age out via
        // the next acquirer's TTL recovery. A regression that
        // propagated the release error would surface a spurious
        // failure for a delete that actually succeeded.
        //
        // Arm a fault on the lock-key delete (which is exactly what
        // `release_lock` calls). The sweep is unaffected (bundle is
        // a different key); the delete must return `Ok(())`.
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("body"));
        mock.arm(crate::object_store::mock::Fault::NetworkOnDelete {
            key: "myrepo/refs/heads/main/LOCK#.lock".to_owned(),
        });
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.delete()
            .await
            .expect("release failure must not mask sweep success");
        // The bundle was deleted; only the now-orphan lock survives
        // (the release fault consumed the lock's delete).
        assert!(!mock.contains("myrepo/refs/heads/main/abc.bundle"));
    }

    // -----------------------------------------------------------------
    // Issue #159 — protect / unprotect must acquire the per-ref lock so
    // a concurrent push-in-progress cannot land a force-push between
    // the under-lock `is_protected` sample and the bundle upload.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn protect_refuses_when_per_ref_lock_is_held_by_another_writer() {
        // Issue #159: pre-fix, `protect` was a lockless `put_bytes`. A
        // concurrent `git push` that had taken the per-ref lock and
        // already passed its under-lock `is_protected()` check could
        // overwrite the bundle even if `protect` landed between that
        // check and the bundle upload. The fix takes the same lock the
        // push path takes; this test seeds a fresh lock (matching a
        // push holding it) and asserts `protect` returns
        // `LockContended` and writes NO marker.
        let mock = seed_with_branch("main");
        mock.insert("myrepo/refs/heads/main/LOCK#.lock", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let err = mb
            .protect()
            .await
            .expect_err("protect must refuse to race a fresh lock holder");
        match &err {
            ManageError::LockContended {
                branch,
                lock,
                ttl_seconds,
            } => {
                assert_eq!(branch, "main");
                assert_eq!(lock, "myrepo/refs/heads/main/LOCK#.lock");
                assert!(*ttl_seconds > 0);
            }
            other => panic!("expected LockContended, got {other:?}"),
        }
        // The marker must NOT be written and the racing writer's lock
        // must NOT be deleted. Pre-#159 this exact race let `protect`
        // land its marker AFTER the push's `is_protected` check, with
        // the push completing the force-push anyway.
        assert!(
            !mock.contains("myrepo/refs/heads/main/PROTECTED#"),
            "no marker may be written under a contended lock",
        );
        assert!(
            mock.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "the racing writer's lock must survive a contention refusal",
        );
        assert!(mock.contains("myrepo/refs/heads/main/abc.bundle"));
    }

    #[tokio::test]
    async fn unprotect_refuses_when_per_ref_lock_is_held_by_another_writer() {
        // Symmetry with the protect contention test: `unprotect` must
        // also block on a held lock so protection state changes are
        // serialised against every other writer. Pre-#159, `unprotect`
        // was a lockless `delete`; a concurrent push observing
        // `is_protected() == true` and a racing `unprotect` could land
        // with the push still on the protected-refusal path.
        let mock = seed_with_branch("main");
        mock.insert("myrepo/refs/heads/main/PROTECTED#", Bytes::new());
        mock.insert("myrepo/refs/heads/main/LOCK#.lock", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let err = mb
            .unprotect()
            .await
            .expect_err("unprotect must refuse to race a fresh lock holder");
        assert!(
            matches!(err, ManageError::LockContended { ref branch, .. } if branch == "main"),
            "expected LockContended, got {err:?}",
        );
        // The marker must remain — `unprotect` did not get to remove it.
        assert!(mock.contains("myrepo/refs/heads/main/PROTECTED#"));
        assert!(mock.contains("myrepo/refs/heads/main/LOCK#.lock"));
    }

    #[tokio::test]
    async fn protect_releases_lock_after_successful_write() {
        // A successful protect must release the LOCK#.lock it acquired.
        // Without release, a subsequent push or unprotect would see a
        // fresh lock and report contention until TTL — defeating the
        // point of an explicit release.
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.protect().await.expect("protect");
        assert!(mock.contains("myrepo/refs/heads/main/PROTECTED#"));
        assert!(
            !mock.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "lock must be released after a successful protect: {:?}",
            mock.keys(),
        );
    }

    #[tokio::test]
    async fn unprotect_releases_lock_after_successful_delete() {
        // Mirror of `protect_releases_lock_after_successful_write`: the
        // unprotect path must release its lock on the success branch.
        let mock = seed_with_branch("main");
        mock.insert("myrepo/refs/heads/main/PROTECTED#", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.unprotect().await.expect("unprotect");
        assert!(!mock.contains("myrepo/refs/heads/main/PROTECTED#"));
        assert!(
            !mock.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "lock must be released after a successful unprotect: {:?}",
            mock.keys(),
        );
    }

    #[tokio::test]
    async fn protect_recovers_stale_lock_and_proceeds() {
        // A `LOCK#.lock` older than the TTL means a previous writer
        // crashed before releasing it. `acquire_lock` recovers it by
        // deleting and re-acquiring. The protect must then complete
        // normally — refusing on a stale lock would let a crashed
        // writer block protection state changes forever.
        let mock = seed_with_branch("main");
        let stale = OffsetDateTime::now_utc() - time::Duration::days(1);
        mock.insert_with(
            "myrepo/refs/heads/main/LOCK#.lock",
            Bytes::new(),
            stale,
            PutOpts::default(),
        );
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.protect().await.expect("stale lock must be recovered");
        assert!(mock.contains("myrepo/refs/heads/main/PROTECTED#"));
        assert!(
            !mock.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "stale lock recovered and our fresh lock released: {:?}",
            mock.keys(),
        );
    }

    #[tokio::test]
    async fn protect_release_failure_does_not_mask_marker_write_success() {
        // Issue #159 / #158 symmetry: release failures are downgraded
        // to `warn!`. A regression that propagated the release error
        // would lie to the operator about a `protect` that actually
        // succeeded — the marker is on the bucket; the orphan lock
        // ages out via the next acquirer's TTL recovery.
        let mock = seed_with_branch("main");
        mock.arm(crate::object_store::mock::Fault::NetworkOnDelete {
            key: "myrepo/refs/heads/main/LOCK#.lock".to_owned(),
        });
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.protect()
            .await
            .expect("release failure must not mask marker-write success");
        assert!(
            mock.contains("myrepo/refs/heads/main/PROTECTED#"),
            "marker must be written even when lock release fails",
        );
    }

    #[tokio::test]
    async fn issue_159_protect_cannot_land_during_active_push() {
        // The headline regression test for #159. Models the documented
        // race verbatim:
        //
        //   1. push acquires LOCK#.lock
        //   2. push reads is_protected -> NotFound
        //   3. operator runs `protect`, which (pre-#159) put_bytes the
        //      marker without taking the lock — succeeds
        //   4. push uploads the new bundle, force-overwriting a now
        //      "protected" ref
        //
        // The fix makes step 3 fail with `LockContended`. With the
        // lock still on the bucket from step 1, `protect` cannot run
        // until the push releases — at which point the push has
        // already either committed or refused on its under-lock
        // `is_protected` check, with no half-state in between.
        //
        // The test seeds the lock directly (representing step 1's
        // holder) and asserts step 3 fails. The push's actual upload
        // is not exercised here because it is covered by the
        // helper-protocol push tests; the load-bearing claim is "no
        // mid-push protect can sneak in".
        let mock = seed_with_branch("main");
        mock.insert("myrepo/refs/heads/main/LOCK#.lock", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let err = mb
            .protect()
            .await
            .expect_err("protect must not land during an active push");
        assert!(
            matches!(err, ManageError::LockContended { .. }),
            "expected LockContended, got {err:?}",
        );
        // Marker NOT written: the under-lock push (when it eventually
        // releases) will see no marker, take whichever branch
        // is_protected dictates, and operator intent never crosses
        // streams with the writer's snapshot.
        assert!(!mock.contains("myrepo/refs/heads/main/PROTECTED#"));
        // The racing writer's lock must survive the contention refusal —
        // protect must not have touched LOCK#.lock owned by another
        // operation. Pinning this directly makes the test self-sufficient.
        assert!(
            mock.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "the writer's LOCK#.lock must survive a contended protect attempt",
        );
    }

    // -----------------------------------------------------------------
    // Issue #151 — delete paths must not miss a `PROTECTED#` marker
    // written after the under-lock listing. Closed mechanically by the
    // per-ref lock (#158 for delete-branch, #159 for protect/unprotect):
    // `protect` blocks on the same key the delete acquired, so a marker
    // cannot land between the under-lock listing and the sweep. These
    // tests pin the lock-contract guarantee and the post-sweep
    // defensive verification that surfaces a contract violation if one
    // ever arises.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn issue_151_protect_cannot_inject_marker_during_active_delete() {
        // The headline regression test for #151. Models the documented
        // race verbatim:
        //
        //   1. delete-branch acquires LOCK#.lock and does its
        //      under-lock listing (no marker).
        //   2. operator runs `protect`, which (pre-#159) put_bytes the
        //      marker without taking the lock — would succeed.
        //   3. delete-branch sweeps the listing it took at step 1,
        //      missing the marker entirely; delete reports success
        //      while the marker is orphaned.
        //
        // The fix (#159) makes step 2 fail with `LockContended` because
        // `protect` now serialises through the same per-ref lock
        // delete-branch holds. The test seeds the lock directly
        // (representing the delete-branch holder at step 1) and asserts
        // step 2 fails — proving the race window is mechanically closed.
        let mock = seed_with_branch("main");
        mock.insert("myrepo/refs/heads/main/LOCK#.lock", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        let err = mb
            .protect()
            .await
            .expect_err("protect must not land during an active delete-branch");
        assert!(
            matches!(err, ManageError::LockContended { .. }),
            "expected LockContended (lock held by delete-branch), got {err:?}",
        );
        // No marker landed — the delete's sweep will not encounter a
        // mid-flow PROTECTED# the listing did not see.
        assert!(
            !mock.contains("myrepo/refs/heads/main/PROTECTED#"),
            "no marker may be written while a delete holds the lock",
        );
        // The delete-branch holder's lock survives the contention
        // refusal verbatim.
        assert!(
            mock.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "the delete-branch holder's lock must survive contention",
        );
    }

    #[tokio::test]
    async fn issue_151_post_sweep_verification_passes_on_clean_delete() {
        // The post-sweep `verify_no_orphan_protected_after_delete`
        // probe is belt-and-suspenders telemetry: with the lock
        // contract in place there is no way for a marker to appear
        // post-sweep. The probe must be silent on the happy path so an
        // operator reading logs is not chasing phantoms.
        //
        // This test exercises the success path end-to-end: seed only
        // the bundle, confirm, sweep. The delete must return Ok and
        // the bucket must be empty (including the lock the release
        // step removed). A regression that flipped the post-sweep
        // probe into a hard error (rather than telemetry) would surface
        // here as an unexpected `Err`.
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.delete()
            .await
            .expect("clean delete must pass the post-sweep probe silently");
        assert!(
            mock.keys().is_empty(),
            "bundle + lock both gone after a clean delete-and-release: {:?}",
            mock.keys(),
        );
    }
}

//! `delete-branch`, `protect`, `unprotect` subcommands.
//!
//! Each operation is anchored at `<prefix>/refs/heads/<branch>/`, the same
//! key space the protocol REPL writes bundles into. When the URL has no
//! repository prefix (root-of-bucket repos, `<prefix>` is empty), keys
//! collapse to `refs/heads/<branch>/...` with no leading slash.

// User-facing output is owned by the management CLI; see the matching
// note in `doctor.rs` for the rationale behind the lint exception.
#![allow(clippy::disallowed_macros)]

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;
use tracing::{info, warn};

use super::{ManageError, Prompter};
use crate::git::RefName;
use crate::keys;
use crate::object_store::{ObjectStore, ObjectStoreError, PutOpts};

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
        // `RefName::new` (delegating to `gix_validate::reference::name`)
        // rejects empties, `..`, control characters, and the rest of
        // git's invalid-ref alphabet.
        if RefName::new(format!("refs/heads/{branch}")).is_err() {
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
        keys::join(Some(&self.prefix), &format!("refs/heads/{}/", self.branch))
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
    /// The prompt-display and protection-marker check use a first listing
    /// for accuracy of the displayed object count, then a **second
    /// listing is taken immediately before the deletion loop**. The
    /// fresh listing drives the sweep so that any concurrent push
    /// landing under the branch prefix during the prompt window is
    /// caught and deleted rather than left as a zombie object (#139).
    /// The protection-marker check is re-evaluated on the fresh listing
    /// so a `protect` racing with the prompt is honoured (#131) — the
    /// post-prompt re-check is what closes the TOCTOU window between
    /// the initial marker check and the deletion loop. If the fresh
    /// listing is empty (a concurrent delete won the race) the function
    /// reports it and returns `Ok(())` rather than silently claiming
    /// success.
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
    /// # Errors
    ///
    /// Returns [`ManageError::Protected`] if the branch carries a
    /// `PROTECTED#` marker (checked on both listings),
    /// [`ManageError::Cancelled`] if the user cancels the prompt,
    /// [`ManageError::Io`] for prompt I/O failures,
    /// [`ManageError::Store`] if a list operation fails, or
    /// [`ManageError::PartialDelete`] when one or more per-key deletes
    /// fail with a non-`NotFound` error after every key in the fresh
    /// listing has been attempted.
    pub async fn delete(&self) -> Result<(), ManageError> {
        let prefix = self.branch_prefix();
        let initial = self.store.list(&prefix).await?;
        if keys::entries_have_protected_marker(&initial) {
            return Err(ManageError::Protected(self.branch.clone()));
        }
        let prompt = format!("Delete branch {} ({} objects)?", self.branch, initial.len());
        if !self.prompter.confirm(&prompt)? {
            println!("Aborted");
            return Ok(());
        }

        // Re-list immediately before the sweep so concurrent pushes that
        // landed during the prompt window are included in the deletion
        // set. The first listing can be arbitrarily stale once the user
        // has had time to answer; #139 was filed because pushes during
        // that window left zombie objects.
        let fresh = self.store.list(&prefix).await?;
        if fresh.is_empty() {
            println!(
                "Branch {} is already gone (concurrent delete during prompt); nothing to do",
                self.branch,
            );
            info!(
                branch = %self.branch,
                "branch already deleted by concurrent operation",
            );
            return Ok(());
        }
        if keys::entries_have_protected_marker(&fresh) {
            return Err(ManageError::Protected(self.branch.clone()));
        }

        let initial_keys: BTreeSet<&str> = initial.iter().map(|m| m.key.as_str()).collect();
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

        // Collect, don't short-circuit: a transient failure on key #2
        // of a 4-key listing must not leave #3 and #4 standing with no
        // inventory of what survived. NotFound continues to be tolerated
        // (the key is gone — operator intent satisfied). Every other
        // per-key error is logged and recorded; at the end we either
        // declare full success or return PartialDelete naming every
        // surviving key so a retry can converge (#122).
        let mut undeleted: Vec<String> = Vec::new();
        for object in &fresh {
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
        if !undeleted.is_empty() {
            return Err(ManageError::PartialDelete {
                branch: self.branch.clone(),
                undeleted,
                attempted: fresh.len(),
            });
        }
        println!("Branch {} has been deleted", self.branch);
        info!(branch = %self.branch, count = fresh.len(), "branch deleted");
        Ok(())
    }

    /// Mark the branch as protected by writing the `PROTECTED#` sentinel.
    /// Idempotent — overwrites any existing marker.
    ///
    /// Re-lists the branch prefix immediately before the put so a
    /// concurrent `delete-branch` (or last-bundle removal) that landed
    /// between [`ManageBranch::open`] and this call is caught and the
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
    /// Returns [`ManageError::BranchNotFound`] if the fresh listing
    /// shows the branch was deleted concurrently. Returns
    /// [`ManageError::Store`] if a list or put operation fails.
    pub async fn protect(&self) -> Result<(), ManageError> {
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
        println!("Branch {} is now protected", self.branch);
        Ok(())
    }

    /// Remove the `PROTECTED#` sentinel. A missing marker is treated as
    /// already-unprotected rather than an error.
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::Store`] for object-store failures other than
    /// `NotFound`.
    pub async fn unprotect(&self) -> Result<(), ManageError> {
        match self.store.delete(&self.protected_key()).await {
            Ok(()) | Err(ObjectStoreError::NotFound(_)) => {
                println!("Branch {} is now unprotected", self.branch);
                Ok(())
            }
            Err(other) => Err(other.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manage::{Prompter, ScriptedPrompter, scripted::Answer};
    use crate::object_store::mock::MockStore;
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
        // No PROTECTED# marker — the lock file and bundle are the only
        // residue, and a confirmed delete must clear them.
        let mock = seed_with_branch("main");
        mock.insert("myrepo/refs/heads/main/LOCK#.lock", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.delete().await.expect("delete");
        assert!(
            mock.keys().is_empty(),
            "all keys removed: {:?}",
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
    async fn protect_refuses_when_only_lock_key_remains() {
        // A stale `LOCK#.lock` key is operational metadata, not branch
        // data. Treating a lock-only listing as "branch exists" would
        // let a `protect` write a marker for a branch that has no
        // bundles — the same orphan-marker pathology #137 describes,
        // just with a lock as the misleading residue instead of an
        // empty listing.
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("body"));
        mock.insert("myrepo/refs/heads/main/LOCK#.lock", Bytes::new());
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
    async fn delete_reports_already_gone_on_concurrent_delete_race() {
        // A concurrent `delete-branch` (or last-bundle removal) clears
        // every object under the branch prefix during the prompt
        // window. The fresh listing is empty; the function must report
        // the race and return Ok(()), not claim success without doing
        // anything.
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
        mb.delete()
            .await
            .expect("empty fresh listing must return Ok, not silent success");
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
        let mock = MockStore::new();
        mock.insert("refs/heads/main/abc.bundle", Bytes::from("body"));
        mock.insert("refs/heads/main/LOCK#.lock", Bytes::new());
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
}

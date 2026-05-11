//! `delete-branch`, `protect`, `unprotect` subcommands.
//!
//! Each operation is anchored at `<prefix>/refs/heads/<branch>/`, the same
//! key space the protocol REPL writes bundles into. When the URL has no
//! repository prefix (root-of-bucket repos, `<prefix>` is empty), keys
//! collapse to `refs/heads/<branch>/...` with no leading slash.

// User-facing output is owned by the management CLI; see the matching
// note in `doctor.rs` for the rationale behind the lint exception.
#![allow(clippy::disallowed_macros)]

use std::sync::Arc;

use bytes::Bytes;
use tracing::info;

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
        keys::join(&self.prefix, &format!("refs/heads/{}/", self.branch))
    }

    fn protected_key(&self) -> String {
        keys::join(
            &self.prefix,
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
    /// (`delete_remote_ref`) emits, so a `git push :branch` against a
    /// protected ref and a management-CLI `delete-branch` of the same
    /// ref fail the same way.
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::Protected`] if the branch carries a
    /// `PROTECTED#` marker, [`ManageError::Cancelled`] if the user cancels
    /// the prompt, [`ManageError::Io`] for prompt I/O failures, or
    /// [`ManageError::Store`] if a list or delete operation fails.
    pub async fn delete(&self) -> Result<(), ManageError> {
        let objects = self.store.list(&self.branch_prefix()).await?;
        if objects.iter().any(|entry| {
            entry
                .key
                .rsplit_once('/')
                .is_some_and(|(_, last)| keys::is_protected_marker_segment(last))
        }) {
            return Err(ManageError::Protected(self.branch.clone()));
        }
        let prompt = format!("Delete branch {} ({} objects)?", self.branch, objects.len());
        if !self.prompter.confirm(&prompt)? {
            println!("Aborted");
            return Ok(());
        }
        for object in &objects {
            self.store.delete(&object.key).await?;
        }
        println!("Branch {} has been deleted", self.branch);
        info!(branch = %self.branch, count = objects.len(), "branch deleted");
        Ok(())
    }

    /// Mark the branch as protected by writing the `PROTECTED#` sentinel.
    /// Idempotent — overwrites any existing marker.
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::Store`] if the put operation fails.
    pub async fn protect(&self) -> Result<(), ManageError> {
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
    async fn delete_partial_failure_propagates_error() {
        // `MockStore::list` returns keys in lexicographic (BTreeMap)
        // order, so the loop deletes aaa, then attempts bbb (armed to
        // fail), then ccc. `delete` returns the error
        // immediately on the failed delete; aaa is gone, bbb and ccc
        // remain.
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/aaa.bundle", Bytes::from("a"));
        mock.insert("myrepo/refs/heads/main/bbb.bundle", Bytes::from("b"));
        mock.insert("myrepo/refs/heads/main/ccc.bundle", Bytes::from("c"));
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
            .expect_err("partial delete must propagate");
        assert!(
            matches!(err, ManageError::Store(_)),
            "expected Store error, got {err:?}"
        );
        assert!(!mock.contains("myrepo/refs/heads/main/aaa.bundle"));
        assert!(mock.contains("myrepo/refs/heads/main/bbb.bundle"));
        assert!(mock.contains("myrepo/refs/heads/main/ccc.bundle"));
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

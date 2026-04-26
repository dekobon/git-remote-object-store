//! `delete-branch`, `protect`, `unprotect` subcommands.
//!
//! Each operation is anchored at `<prefix>/refs/heads/<branch>/`, the same
//! key space the protocol REPL writes bundles into. Mirrors upstream
//! `ManageBranch` in `../git-remote-s3/git_remote_s3/manage.py`.

// User-facing output is owned by the management CLI; see the matching
// note in `doctor.rs` for the rationale behind the lint exception.
#![allow(clippy::disallowed_macros)]

use std::sync::Arc;

use bytes::Bytes;
use tracing::info;

use super::{ManageError, Prompter};
use crate::object_store::{Error as ObjectStoreError, ObjectStore, PutOpts};

/// Operations on a single branch within a repository.
pub struct ManageBranch<'a> {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    branch: String,
    prompter: &'a dyn Prompter,
}

impl<'a> ManageBranch<'a> {
    /// Open a branch handle, verifying it exists by listing
    /// `<prefix>/refs/heads/<branch>/`. Returns
    /// [`ManageError::BranchNotFound`] when no objects are present.
    pub async fn open(
        store: Arc<dyn ObjectStore>,
        prefix: impl Into<String>,
        branch: impl Into<String>,
        prompter: &'a dyn Prompter,
    ) -> Result<Self, ManageError> {
        let mb = Self {
            store,
            prefix: prefix.into(),
            branch: branch.into(),
            prompter,
        };
        if mb.store.list(&mb.branch_prefix()).await?.is_empty() {
            return Err(ManageError::BranchNotFound(mb.branch));
        }
        Ok(mb)
    }

    fn branch_prefix(&self) -> String {
        format!("{}/refs/heads/{}/", self.prefix, self.branch)
    }

    fn protected_key(&self) -> String {
        format!("{}/refs/heads/{}/PROTECTED#", self.prefix, self.branch)
    }

    /// Delete every object under the branch's prefix after a `yes/no`
    /// confirmation. Aborts (returns `Ok(())`) if the user answers no;
    /// the `Cancelled` variant is reserved for prompt I/O failures.
    pub async fn delete_branch(&self) -> Result<(), ManageError> {
        let objects = self.store.list(&self.branch_prefix()).await?;
        let prompt = format!("Delete branch {} ({} objects)?", self.branch, objects.len());
        if !self.prompter.confirm(&prompt)? {
            println!("Aborted");
            return Ok(());
        }
        let count = objects.len();
        for object in &objects {
            self.store.delete(&object.key).await?;
        }
        println!("Branch {} has been deleted", self.branch);
        info!(branch = %self.branch, count, "branch deleted");
        Ok(())
    }

    /// Mark the branch as protected by writing the `PROTECTED#` sentinel.
    /// Idempotent — overwrites any existing marker.
    pub async fn protect_branch(&self) -> Result<(), ManageError> {
        self.store
            .put_bytes(&self.protected_key(), Bytes::new(), PutOpts::default())
            .await?;
        println!("Branch {} is now protected", self.branch);
        Ok(())
    }

    /// Remove the `PROTECTED#` sentinel. A missing marker is treated as
    /// already-unprotected rather than an error.
    pub async fn unprotect_branch(&self) -> Result<(), ManageError> {
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
    async fn delete_branch_removes_every_key_when_confirmed() {
        let mock = seed_with_branch("main");
        mock.insert("myrepo/refs/heads/main/PROTECTED#", Bytes::new());
        mock.insert("myrepo/refs/heads/main/LOCK#.lock", Bytes::new());
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(true)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.delete_branch().await.expect("delete");
        assert!(
            mock.keys().is_empty(),
            "all keys removed: {:?}",
            mock.keys()
        );
        assert_eq!(prompter.remaining(), 0);
    }

    #[tokio::test]
    async fn delete_branch_no_keeps_keys() {
        let mock = seed_with_branch("main");
        let store: Arc<dyn ObjectStore> = Arc::new(mock.clone());
        let prompter = ScriptedPrompter::new([Answer::Confirm(false)]);

        let mb = ManageBranch::open(store, "myrepo", "main", &prompter as &dyn Prompter)
            .await
            .expect("open");
        mb.delete_branch().await.expect("delete (aborted)");
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
        mb.protect_branch().await.expect("protect");
        assert!(mock.contains("myrepo/refs/heads/main/PROTECTED#"));
        // Second call overwrites without error.
        mb.protect_branch().await.expect("protect again");
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
        mb.unprotect_branch().await.expect("unprotect");
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
        mb.unprotect_branch()
            .await
            .expect("unprotect should be idempotent");
    }
}

//! `doctor` analyzer + fixers.
//!
//! Mirrors `Doctor` in upstream
//! `../git-remote-s3/git_remote_s3/manage.py`. The flow is:
//! analyze → print report → fix duplicate bundles per ref → fix invalid
//! HEAD → list and (optionally) delete stale locks.
//!
//! The `Doctor` value is constructed once per CLI invocation; all
//! interaction goes through the injected [`Prompter`] so the same code
//! path drives both the binary (via [`DialoguerPrompter`]) and unit
//! tests (via [`ScriptedPrompter`]).
//!
//! [`DialoguerPrompter`]: super::DialoguerPrompter
//! [`ScriptedPrompter`]: super::ScriptedPrompter

// The doctor's report is the management CLI's user-facing output and is
// only reachable from the `git-remote-object-store` binary, which speaks
// no protocol on stdout. Per `.claude/rules/protocol-stdout.md` the
// management binary "may write to stdout normally"; the global
// `disallowed_macros` lint is opted out here so the report can use
// `println!` without per-line escapes.
#![allow(clippy::disallowed_macros)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use time::OffsetDateTime;
use tracing::info;
use uuid::Uuid;

use super::snapshot::{BundleEntry, RepoSnapshot, analyze};
use super::{DEFAULT_LOCK_TTL_SECONDS, ManageError, Prompter};
use crate::object_store::{ObjectStore, PutOpts};

/// Tunables for [`Doctor::run`]. Field names match the equivalent
/// upstream Python `argparse` flags.
#[derive(Debug, Clone, Copy)]
pub struct DoctorOpts {
    /// When `true`, `fix_multiple_bundles` deletes the losing bundles
    /// outright. When `false` (default), they are quarantined to a
    /// fresh `<ref>_<uuid8>` ref so a human can recover them later.
    pub delete_bundle: bool,
    /// Locks older than this TTL are considered stale.
    pub lock_ttl_seconds: u64,
    /// When `true`, scanned stale locks are deleted; otherwise, the
    /// doctor only reports them and recommends re-running with the
    /// flag.
    pub delete_stale_locks: bool,
}

impl Default for DoctorOpts {
    fn default() -> Self {
        Self {
            delete_bundle: false,
            lock_ttl_seconds: DEFAULT_LOCK_TTL_SECONDS,
            delete_stale_locks: false,
        }
    }
}

/// `doctor` runner.
pub struct Doctor<'a> {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    opts: DoctorOpts,
    prompter: &'a dyn Prompter,
}

impl<'a> Doctor<'a> {
    /// Construct a new runner. `prefix` is the parsed remote URL's
    /// repository prefix without a trailing `/`.
    #[must_use]
    pub fn new(
        store: Arc<dyn ObjectStore>,
        prefix: impl Into<String>,
        opts: DoctorOpts,
        prompter: &'a dyn Prompter,
    ) -> Self {
        Self {
            store,
            prefix: prefix.into(),
            opts,
            prompter,
        }
    }

    /// Analyze, report, and fix.
    ///
    /// Errors short-circuit the run — partial fixes are committed
    /// immediately (each `delete` / `copy` / `put` is its own request),
    /// matching upstream's "best-effort" stance.
    pub async fn run(&self) -> Result<(), ManageError> {
        let mut snapshot = analyze(&self.store, &self.prefix).await?;
        self.report(&snapshot);

        // Fix duplicates ref-by-ref. We need owned ref-names because
        // `fix_multiple_bundles` mutates the snapshot under `&mut`.
        let dup_refs: Vec<String> = snapshot
            .refs
            .iter()
            .filter(|(_, r)| r.bundles.len() > 1)
            .map(|(name, _)| name.clone())
            .collect();
        for ref_path in dup_refs {
            self.fix_multiple_bundles(&mut snapshot, &ref_path).await?;
        }

        if !snapshot.is_head_valid() {
            self.fix_head(&mut snapshot).await?;
        }

        self.list_and_handle_stale_locks().await?;
        Ok(())
    }

    fn report(&self, snapshot: &RepoSnapshot) {
        println!("{}:", self.prefix);
        let head = snapshot.head.as_deref();
        let mut head_label = "Invalid";
        for (ref_path, r) in &snapshot.refs {
            if head == Some(ref_path.as_str()) {
                head_label = ref_path.as_str();
            }
            let star = if r.is_protected { "*" } else { "" };
            let status = if r.bundles.len() == 1 {
                "Ok"
            } else if r.bundles.is_empty() {
                "No bundles"
            } else {
                "Multiple bundles"
            };
            println!(" {star} {ref_path}: {status}");
        }
        println!("  HEAD: {head_label}");
    }

    async fn fix_multiple_bundles(
        &self,
        snapshot: &mut RepoSnapshot,
        ref_path: &str,
    ) -> Result<(), ManageError> {
        println!(
            "\nFix multiple bundles for repo {} and ref {ref_path}",
            self.prefix
        );

        // Take ownership of the bundles vec so we can mutate the
        // snapshot for the rest of the run after deciding the keeper.
        let bundles = std::mem::take(
            &mut snapshot
                .refs
                .get_mut(ref_path)
                .expect("ref present per caller's filter")
                .bundles,
        );
        let labels: Vec<String> = bundles
            .iter()
            .map(|b| format!("{} {}", b.sha, b.last_modified))
            .collect();

        let keep_idx = self.prompter.select("Choose the bundle to keep", &labels)?;
        if keep_idx >= bundles.len() {
            return Err(ManageError::Cancelled);
        }
        if !self.prompter.confirm("Confirm and apply changes")? {
            // Restore so a follow-up `doctor` run still sees the duplicates.
            snapshot
                .refs
                .get_mut(ref_path)
                .expect("ref present")
                .bundles = bundles;
            println!("Aborted");
            return Err(ManageError::Cancelled);
        }

        let keeper = &bundles[keep_idx];
        println!("Keeping {}", keeper.sha);
        for (idx, losing) in bundles.iter().enumerate() {
            if idx == keep_idx {
                continue;
            }
            self.evict_losing_bundle(ref_path, losing).await?;
        }

        // Reflect the resolved state in the snapshot so subsequent
        // steps (HEAD validation in particular) see the new layout.
        snapshot
            .refs
            .get_mut(ref_path)
            .expect("ref present")
            .bundles = vec![keeper.clone()];
        Ok(())
    }

    async fn evict_losing_bundle(
        &self,
        ref_path: &str,
        losing: &BundleEntry,
    ) -> Result<(), ManageError> {
        if self.opts.delete_bundle {
            println!("Removing {}", losing.sha);
            self.store.delete(&losing.key).await?;
        } else {
            let suffix: String = Uuid::new_v4().simple().to_string()[..8].to_owned();
            let new_ref = format!("{ref_path}_{suffix}");
            let dst_key = format!("{}/{new_ref}/{}.bundle", self.prefix, losing.sha);
            println!("Moving {} to new branch {new_ref}", losing.sha);
            self.store.copy(&losing.key, &dst_key).await?;
            self.store.delete(&losing.key).await?;
        }
        Ok(())
    }

    async fn fix_head(&self, snapshot: &mut RepoSnapshot) -> Result<(), ManageError> {
        println!("\nFix invalid HEAD for repo {}", self.prefix);

        let candidates: Vec<String> = snapshot
            .refs
            .keys()
            .filter(|k| k.starts_with("refs/heads/"))
            .cloned()
            .collect();
        if candidates.is_empty() {
            println!("No `refs/heads/*` available to assign as HEAD; skipping.");
            return Ok(());
        }

        let labels: Vec<String> = candidates
            .iter()
            .map(|k| short_branch_name(k).to_owned())
            .collect();
        let chosen = self
            .prompter
            .select("Choose the new HEAD branch", &labels)?;
        let new_head = candidates
            .get(chosen)
            .ok_or(ManageError::Cancelled)?
            .clone();

        let head_key = format!("{}/HEAD", self.prefix);
        println!("Setting {new_head} as HEAD");
        self.store
            .put_bytes(&head_key, Bytes::from(new_head.clone()), PutOpts::default())
            .await?;
        snapshot.head = Some(new_head);
        Ok(())
    }

    async fn list_and_handle_stale_locks(&self) -> Result<(), ManageError> {
        println!("\nScanning for stale locks...");
        let prefix = format!("{}/", self.prefix);
        let objects = self.store.list(&prefix).await?;
        let now = OffsetDateTime::now_utc();
        let ttl = Duration::from_secs(self.opts.lock_ttl_seconds);

        let stale: Vec<(String, Duration)> = objects
            .into_iter()
            // `.lock` is a wire-format key suffix, not a filesystem
            // extension — see the matching note in `snapshot.rs`.
            .filter(|o| {
                #[allow(clippy::case_sensitive_file_extension_comparisons)]
                {
                    o.key.ends_with(".lock")
                }
            })
            .filter_map(|o| {
                let elapsed = now - o.last_modified;
                let elapsed = Duration::try_from(elapsed).ok()?;
                (elapsed > ttl).then_some((o.key, elapsed))
            })
            .collect();

        if stale.is_empty() {
            println!("No stale locks found.");
            return Ok(());
        }

        println!("Found stale locks:");
        for (key, age) in &stale {
            println!(" - {key} (age: {}s)", age.as_secs());
        }

        if self.opts.delete_stale_locks {
            println!("\nDeleting stale locks...");
            for (key, _) in &stale {
                match self.store.delete(key).await {
                    Ok(()) => {
                        println!("Deleted {key}");
                        info!(key, "deleted stale lock");
                    }
                    Err(e) => {
                        // Match upstream: report each failure but keep going.
                        println!("Failed to delete {key}: {e}");
                    }
                }
            }
        } else {
            println!("\nRun with --delete-stale-locks to remove them automatically.");
        }
        Ok(())
    }
}

/// Drop the `refs/heads/` prefix when rendering a branch name to the
/// user. Falls back to the full ref path if the prefix is absent.
fn short_branch_name(full: &str) -> &str {
    full.rsplit('/').next().unwrap_or(full)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manage::{ScriptedPrompter, scripted::Answer};
    use crate::object_store::mock::MockStore;
    use bytes::Bytes;
    use time::OffsetDateTime;

    fn store_arc(mock: &MockStore) -> Arc<dyn ObjectStore> {
        Arc::new(mock.clone())
    }

    #[tokio::test]
    async fn no_issues_round_trip_runs_clean() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("b"));
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        doctor.run().await.expect("doctor.run");
        assert_eq!(prompter.remaining(), 0);
    }

    #[tokio::test]
    async fn fix_multiple_bundles_quarantines_losers_by_default() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            "myrepo/refs/heads/main/aaaaaaaa.bundle",
            Bytes::from("body-a"),
        );
        mock.insert(
            "myrepo/refs/heads/main/bbbbbbbb.bundle",
            Bytes::from("body-b"),
        );
        let prompter = ScriptedPrompter::new([Answer::Select(0), Answer::Confirm(true)]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        doctor.run().await.expect("doctor.run");

        // Original bundle for the keeper is still present.
        assert!(mock.contains("myrepo/refs/heads/main/aaaaaaaa.bundle"));
        // Loser was moved off the main ref.
        assert!(!mock.contains("myrepo/refs/heads/main/bbbbbbbb.bundle"));
        // The new quarantine ref has a key with the moved bundle.
        let moved = mock
            .keys()
            .into_iter()
            .find(|k| k.starts_with("myrepo/refs/heads/main_") && k.ends_with("/bbbbbbbb.bundle"))
            .expect("quarantine key created");
        assert!(moved.len() > "myrepo/refs/heads/main_/bbbbbbbb.bundle".len());
    }

    #[tokio::test]
    async fn fix_multiple_bundles_delete_mode_removes_losers() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("myrepo/refs/heads/main/aaa.bundle", Bytes::from("a"));
        mock.insert("myrepo/refs/heads/main/bbb.bundle", Bytes::from("b"));
        let prompter = ScriptedPrompter::new([Answer::Select(1), Answer::Confirm(true)]);
        let opts = DoctorOpts {
            delete_bundle: true,
            ..DoctorOpts::default()
        };
        let doctor = Doctor::new(store_arc(&mock), "myrepo", opts, &prompter);
        doctor.run().await.expect("doctor.run");
        assert!(!mock.contains("myrepo/refs/heads/main/aaa.bundle"));
        assert!(mock.contains("myrepo/refs/heads/main/bbb.bundle"));
    }

    #[tokio::test]
    async fn fix_multiple_bundles_user_aborts_keeps_originals() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("myrepo/refs/heads/main/aaa.bundle", Bytes::from("a"));
        mock.insert("myrepo/refs/heads/main/bbb.bundle", Bytes::from("b"));
        let prompter = ScriptedPrompter::new([Answer::Select(0), Answer::Confirm(false)]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let err = doctor.run().await.expect_err("aborted should propagate");
        assert!(matches!(err, ManageError::Cancelled));
        assert!(mock.contains("myrepo/refs/heads/main/aaa.bundle"));
        assert!(mock.contains("myrepo/refs/heads/main/bbb.bundle"));
    }

    #[tokio::test]
    async fn fix_head_writes_chosen_branch() {
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("b"));
        mock.insert("myrepo/refs/heads/dev/def.bundle", Bytes::from("c"));
        // Refs are surfaced in BTreeMap order (lexicographic), so the
        // candidate list is `[refs/heads/dev, refs/heads/main]` and
        // index 1 is `main`.
        let prompter = ScriptedPrompter::new([Answer::Select(1)]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        doctor.run().await.expect("doctor.run");

        let head_bytes = mock.get_bytes("myrepo/HEAD").await.expect("HEAD written");
        assert_eq!(&head_bytes[..], b"refs/heads/main");
    }

    #[tokio::test]
    async fn stale_lock_listed_but_not_deleted_by_default() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("b"));
        let stale = OffsetDateTime::now_utc() - time::Duration::seconds(120);
        mock.insert_with(
            "myrepo/refs/heads/main/LOCK#.lock",
            Bytes::new(),
            stale,
            PutOpts::default(),
        );
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        doctor.run().await.expect("doctor.run");
        assert!(
            mock.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "lock retained without --delete-stale-locks"
        );
    }

    #[tokio::test]
    async fn stale_lock_deleted_when_flag_set() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("b"));
        let stale = OffsetDateTime::now_utc() - time::Duration::seconds(120);
        mock.insert_with(
            "myrepo/refs/heads/main/LOCK#.lock",
            Bytes::new(),
            stale,
            PutOpts::default(),
        );
        let opts = DoctorOpts {
            delete_stale_locks: true,
            ..DoctorOpts::default()
        };
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", opts, &prompter);
        doctor.run().await.expect("doctor.run");
        assert!(!mock.contains("myrepo/refs/heads/main/LOCK#.lock"));
    }

    #[tokio::test]
    async fn fresh_lock_is_not_flagged_stale() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("b"));
        // Stamped now → not stale.
        mock.insert("myrepo/refs/heads/main/LOCK#.lock", Bytes::new());
        let opts = DoctorOpts {
            delete_stale_locks: true,
            ..DoctorOpts::default()
        };
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", opts, &prompter);
        doctor.run().await.expect("doctor.run");
        assert!(mock.contains("myrepo/refs/heads/main/LOCK#.lock"));
    }
}

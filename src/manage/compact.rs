//! `compact` subcommand for the management CLI (issue #67, Phase 5
//! of #52).
//!
//! Drives [`crate::packchain::compact::compact`] against one ref
//! (when `--ref` is given) or every ref whose chain meets the
//! compaction heuristic (the default — operator confirms the list
//! interactively before any work runs).
//!
//! All output is human-readable on stdout; the management CLI may
//! `println!` per `.claude/rules/protocol-stdout.md`.

#![allow(clippy::disallowed_macros)]

use std::sync::Arc;

use time::Duration;
use tracing::info;

use super::{ManageError, Prompter};
use crate::git::RefName;
use crate::keys;
use crate::object_store::ObjectStore;
use crate::packchain::audit::{self, AuditReport, BranchAuditRow};
use crate::packchain::compact::{self, CompactAction, CompactOpts, CompactOutcome};
use crate::packchain::gc;
use crate::protocol::push::lock_ttl_from_env;

/// Tunables for [`Compact::run`]. Field semantics mirror the CLI flags.
#[derive(Debug, Clone)]
pub struct ManageCompactOpts {
    /// Compact only the named ref. `None` triggers the audit-driven
    /// "every ref meeting the heuristic" mode.
    pub ref_name: Option<String>,
    /// Bypass the heuristic and compact unconditionally.
    pub force: bool,
    /// Run [`crate::packchain::gc`] mark+sweep against the same
    /// bucket after a successful compact.
    pub with_gc: bool,
    /// Lock TTL for compact's per-ref lock. When `None`, falls back
    /// to [`crate::protocol::push::lock_ttl_from_env`] which honours
    /// `GIT_REMOTE_S3_LOCK_TTL_SECONDS`.
    pub lock_ttl_seconds: Option<u64>,
    /// Grace hours forwarded to `gc::sweep` when `with_gc` is set.
    pub gc_grace_hours: u64,
}

impl Default for ManageCompactOpts {
    fn default() -> Self {
        Self {
            ref_name: None,
            force: false,
            with_gc: false,
            lock_ttl_seconds: None,
            gc_grace_hours: gc::DEFAULT_GRACE_HOURS,
        }
    }
}

/// `compact` runner.
pub struct Compact<'a> {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    opts: ManageCompactOpts,
    prompter: &'a dyn Prompter,
}

impl<'a> Compact<'a> {
    /// Construct a runner. `prefix` is the parsed remote URL's
    /// repository prefix without a trailing slash; pass an empty
    /// string for bucket-root repositories.
    #[must_use]
    pub fn new(
        store: Arc<dyn ObjectStore>,
        prefix: impl Into<String>,
        opts: ManageCompactOpts,
        prompter: &'a dyn Prompter,
    ) -> Self {
        Self {
            store,
            prefix: prefix.into(),
            opts,
            prompter,
        }
    }

    /// Execute the configured flow.
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::Store`] for transport failures,
    /// [`ManageError::Packchain`] for engine-level failures during
    /// chain install / repack, [`ManageError::InvalidBranch`] when
    /// `--ref` value fails ref-name validation,
    /// [`ManageError::BranchNotFound`] when a named ref has no
    /// chain.json, and [`ManageError::Cancelled`] when the operator
    /// declines the interactive prompt.
    pub async fn run(&self) -> Result<(), ManageError> {
        let lock_ttl = self
            .opts
            .lock_ttl_seconds
            .map_or_else(lock_ttl_from_env, |s| {
                Duration::seconds(i64::try_from(s).unwrap_or(i64::MAX))
            });
        let compact_opts = CompactOpts {
            force: self.opts.force,
            lock_ttl,
        };

        let targets = self.resolve_targets().await?;
        if targets.is_empty() {
            println!("compact: no refs match the criteria; nothing to do.");
            return Ok(());
        }

        let mut any_compacted = false;
        for ref_name in targets {
            let outcome = compact::compact(
                self.store.as_ref(),
                self.prefix_opt(),
                &ref_name,
                compact_opts,
            )
            .await?;
            print_outcome(&outcome);
            if matches!(outcome.action, CompactAction::Compacted) {
                any_compacted = true;
            }
        }

        if self.opts.with_gc && any_compacted {
            self.run_gc().await?;
        } else if self.opts.with_gc {
            println!("compact: no refs were compacted; skipping gc.");
        }
        Ok(())
    }

    /// Compute the list of ref names to compact. `--ref` short-
    /// circuits to a single ref; otherwise scan via the audit and
    /// prompt the operator to confirm the candidate list.
    async fn resolve_targets(&self) -> Result<Vec<RefName>, ManageError> {
        if let Some(name) = &self.opts.ref_name {
            let ref_name =
                RefName::new(name).map_err(|_| ManageError::InvalidBranch(name.clone()))?;
            return Ok(vec![ref_name]);
        }

        let report = self.audit_for_compaction_candidates().await?;
        let candidates: Vec<&BranchAuditRow> = report
            .branches
            .iter()
            .filter(|r| r.recommend_compact)
            .collect();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        println!("Branches recommended for compaction:");
        for row in &candidates {
            println!(
                "  - {}: {} segment(s), {} byte(s)",
                row.ref_path, row.segments_total, row.bytes_total,
            );
        }
        if !self.prompter.confirm("Compact all of the above?")? {
            println!("Aborted");
            return Err(ManageError::Cancelled);
        }

        let mut out = Vec::with_capacity(candidates.len());
        for row in candidates {
            let ref_name = RefName::new(&row.ref_path)
                .map_err(|_| ManageError::InvalidBranch(row.ref_path.clone()))?;
            out.push(ref_name);
        }
        Ok(out)
    }

    /// Walk the bucket once and derive the audit report we use for
    /// candidate selection. Mirrors the doctor's audit flow.
    async fn audit_for_compaction_candidates(&self) -> Result<AuditReport, ManageError> {
        let list_prefix = keys::join(&self.prefix, "");
        let objects = self.store.list(&list_prefix).await?;
        audit::audit(self.store.as_ref(), &self.prefix, &objects)
            .await
            .map_err(ManageError::from)
    }

    async fn run_gc(&self) -> Result<(), ManageError> {
        let store_ref = self.store.as_ref();
        let mark = gc::mark(store_ref, &self.prefix, gc::MarkOpts::default()).await?;
        if mark.orphan_count == 0 {
            println!("gc mark: no orphan packs.");
        } else {
            println!(
                "gc mark: {} orphan pack(s) tombstoned (run id {}).",
                mark.orphan_count, mark.run_id,
            );
            info!(
                run_id = %mark.run_id,
                key = %mark.tombstone_key,
                "compact --with-gc: mark completed",
            );
        }
        let sweep = gc::sweep(
            store_ref,
            &self.prefix,
            gc::SweepOpts {
                grace_hours: self.opts.gc_grace_hours,
                force: false,
            },
        )
        .await?;
        if sweep.swept_tombstones == 0 && sweep.deferred_tombstones == 0 {
            println!("gc sweep: no tombstones present.");
        } else {
            println!(
                "gc sweep: {} tombstone(s) applied, {} object(s) deleted, {} repointed pack(s) skipped, {} tombstone(s) deferred.",
                sweep.swept_tombstones,
                sweep.deleted_objects,
                sweep.skipped_repointed_packs,
                sweep.deferred_tombstones,
            );
        }
        Ok(())
    }

    fn prefix_opt(&self) -> Option<&str> {
        if self.prefix.is_empty() {
            None
        } else {
            Some(&self.prefix)
        }
    }
}

fn print_outcome(outcome: &CompactOutcome) {
    match outcome.action {
        CompactAction::Compacted => {
            let new_pack = outcome.new_pack_sha.as_deref().unwrap_or("?");
            println!(
                "compact: {} rewritten to single segment (was {} segment(s), {} byte(s); new pack {} at {} byte(s))",
                outcome.ref_path,
                outcome.prior_segments,
                outcome.prior_bytes,
                new_pack,
                outcome.new_pack_bytes,
            );
        }
        CompactAction::SkippedUnderThreshold => {
            println!(
                "compact: {} below heuristic ({} segment(s), {} byte(s)); skipping. Use --force to compact unconditionally.",
                outcome.ref_path, outcome.prior_segments, outcome.prior_bytes,
            );
        }
        CompactAction::AlreadyMinimal => {
            println!(
                "compact: {} already a single-segment chain at the tip; nothing to do.",
                outcome.ref_path,
            );
        }
        CompactAction::LockContended => {
            println!(
                "compact: {} per-ref lock is held by another client; try again later.",
                outcome.ref_path,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manage::ScriptedPrompter;
    use crate::object_store::mock::MockStore;
    use std::sync::Arc;

    fn store_arc(mock: &MockStore) -> Arc<dyn ObjectStore> {
        Arc::new(mock.clone())
    }

    #[tokio::test]
    async fn run_with_named_ref_propagates_invalid_branch() {
        // `--ref ../etc/passwd` must surface a typed
        // `ManageError::InvalidBranch`, not a transport error or a
        // `ChainAbsent`.
        let mock = MockStore::new();
        let prompter = ScriptedPrompter::new([]);
        let runner = Compact::new(
            store_arc(&mock),
            "repo",
            ManageCompactOpts {
                ref_name: Some("refs/heads/../etc/passwd".to_owned()),
                ..ManageCompactOpts::default()
            },
            &prompter,
        );
        let err = runner.run().await.expect_err("invalid ref must error");
        assert!(matches!(err, ManageError::InvalidBranch(_)), "{err:?}");
    }

    #[tokio::test]
    async fn run_default_with_no_candidates_does_nothing() {
        // No chain.json in the bucket → audit returns no candidates →
        // runner prints "nothing to do" and returns Ok without ever
        // prompting.
        let mock = MockStore::new();
        let prompter = ScriptedPrompter::new([]); // no answers queued
        let runner = Compact::new(
            store_arc(&mock),
            "repo",
            ManageCompactOpts::default(),
            &prompter,
        );
        runner.run().await.expect("no-candidate run is Ok");
    }

    #[tokio::test]
    async fn with_gc_skipped_when_no_compaction_happened() {
        // ref_name + force=false against a chain that does not meet
        // the heuristic → SkippedUnderThreshold → with_gc must NOT
        // run gc (no orphans were produced).
        let mock = MockStore::new();
        let chain = crate::packchain::schema::ChainManifest {
            v: 1,
            tip: crate::packchain::schema::Sha40::try_new(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .unwrap(),
            full_at: crate::packchain::schema::Sha40::try_new(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            )
            .unwrap(),
            segments: vec![crate::packchain::schema::ChainSegment {
                sha: crate::packchain::schema::Sha40::try_new(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .unwrap(),
                parent_sha: Some(
                    crate::packchain::schema::Sha40::try_new(
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    )
                    .unwrap(),
                ),
                pack: "packs/1111111111111111111111111111111111111111.pack".to_owned(),
                bytes: 1024,
            }],
        };
        let rn = crate::git::RefName::new("refs/heads/main").unwrap();
        crate::packchain::manifest::write_chain(&mock, Some("repo"), &rn, &chain)
            .await
            .unwrap();
        // Add a stand-alone pack with NO chain reference. This is a
        // real orphan: gc::mark would observe it and write a
        // tombstone if it ran. The original test had no orphans on
        // the bucket, so the "no `gc/` keys" assertion was
        // vacuously true regardless of whether the with_gc gate
        // fired — mutation-verified during /audit-tests
        // (#67-followup).
        mock.insert(
            "repo/packs/9999999999999999999999999999999999999999.pack",
            bytes::Bytes::from_static(b"orphan"),
        );

        let prompter = ScriptedPrompter::new([]);
        let runner = Compact::new(
            store_arc(&mock),
            "repo",
            ManageCompactOpts {
                ref_name: Some("refs/heads/main".to_owned()),
                with_gc: true,
                ..ManageCompactOpts::default()
            },
            &prompter,
        );
        // Compact under-threshold short-circuits to
        // `SkippedUnderThreshold`; with_gc must observe
        // `any_compacted == false` and skip gc. If gc ran, it
        // would tombstone the orphan above and we would see a
        // `repo/gc/tombstones-*.json` key.
        runner.run().await.expect("skip-under-threshold run is Ok");
        let keys = mock.keys();
        assert!(
            !keys.iter().any(|k| k.starts_with("repo/gc/")),
            "with_gc must NOT run gc when nothing was compacted; \
             unexpected gc/ keys: {:?}",
            keys.iter()
                .filter(|k| k.starts_with("repo/gc/"))
                .collect::<Vec<_>>(),
        );
    }
}

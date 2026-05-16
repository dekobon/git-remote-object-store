//! `compact` subcommand for the management CLI (issue #67, Phase 5
//! of #52).
//!
//! Drives [`crate::packchain::compact::compact`] against one ref
//! (when `--ref` is given) or every ref whose chain meets the
//! compaction heuristic (the default — operator confirms the list
//! interactively before any work runs).
//!
//! All output is human-readable on stdout; the management CLI may
//! write to stdout per `.claude/rules/protocol-stdout.md`. The
//! per-ref loop accepts an injected writer so tests can pin the
//! operator-visible output byte-for-byte (issue #160).

use std::io::Write;
use std::sync::Arc;

use time::Duration;
use tracing::{info, warn};

use super::gc_output::{format_mark_outcome, format_sweep_outcome};
use super::{ManageError, Prompter};
use crate::git::RefName;
use crate::keys;
use crate::object_store::ObjectStore;
use crate::packchain::PackchainError;
use crate::packchain::audit::{self, AuditReport, BranchRow};
use crate::packchain::compact::{
    self, CompactAction, CompactOpts as PackchainCompactOpts, CompactOutcome,
};
use crate::packchain::gc;
use crate::protocol::push::resolve_lock_ttl_seconds;

/// Tunables for [`Compact::run`]. Field semantics mirror the CLI flags.
#[derive(Debug, Clone, Default)]
pub struct CompactOpts {
    /// Compact only the named ref. `None` triggers the audit-driven
    /// "every ref meeting the heuristic" mode.
    pub ref_name: Option<String>,
    /// Bypass the heuristic and compact unconditionally.
    pub force: bool,
    /// Run [`crate::packchain::gc`] mark+sweep against the same
    /// bucket after a successful compact.
    pub with_gc: bool,
    /// Lock TTL for compact's per-ref lock. When `None` *or* `Some(0)`,
    /// falls back to [`crate::protocol::push::lock_ttl_from_env`] which
    /// honours `GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS`. A zero TTL
    /// would defeat per-ref locking (issue #208), so the resolver
    /// clamps it through [`crate::protocol::push::resolve_lock_ttl_seconds`].
    pub lock_ttl_seconds: Option<u64>,
    /// Grace hours forwarded to `gc::sweep` when `with_gc` is set.
    /// `None` falls back to [`crate::packchain::gc::grace_hours_from_env`]
    /// which honours `GIT_REMOTE_OBJECT_STORE_GC_GRACE_HOURS` (defaulting
    /// to [`gc::DEFAULT_GRACE_HOURS`] when unset).
    pub gc_grace_hours: Option<u64>,
}

/// `compact` runner.
pub struct Compact<'a> {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    opts: CompactOpts,
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
        opts: CompactOpts,
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
        self.run_into(&mut std::io::stdout()).await
    }

    /// Same contract as [`run`](Self::run) but writes human-readable
    /// output to `out`. Tests use this to capture the operator messages
    /// (e.g. the per-ref "vanished, skipping" notice from #160) so a
    /// regression that silently turns a concurrent ref deletion into a
    /// whole-run abort, or into a silent success, is caught.
    ///
    /// # Errors
    ///
    /// Same as [`run`](Self::run), plus [`ManageError::Io`] if a write
    /// to `out` fails.
    pub(crate) async fn run_into<W: Write>(&self, out: &mut W) -> Result<(), ManageError> {
        // Route Option<u64> through the shared resolver so `Some(0)`
        // cannot defeat per-ref locking (issue #208). `i64::try_from`
        // saturates at `i64::MAX`, which `time::Duration::seconds`
        // accepts as a sentinel ~292-billion-year ceiling.
        let ttl_secs = resolve_lock_ttl_seconds(self.opts.lock_ttl_seconds);
        let lock_ttl = Duration::seconds(i64::try_from(ttl_secs).unwrap_or(i64::MAX));
        let compact_opts = PackchainCompactOpts {
            force: self.opts.force,
            lock_ttl,
        };

        let targets = self.resolve_targets(out).await?;
        if targets.is_empty() {
            writeln!(out, "compact: no refs match the criteria; nothing to do.")?;
            return Ok(());
        }

        let summary = self
            .compact_targets_into(out, &targets, compact_opts)
            .await?;

        if self.opts.with_gc && summary.any_compacted {
            self.run_gc(out).await?;
        } else if self.opts.with_gc {
            writeln!(out, "compact: no refs were compacted; skipping gc.")?;
        }
        Ok(())
    }

    /// Per-ref compaction loop with the #160 "vanished, skipping"
    /// downgrade. Returns the aggregate summary so the caller can
    /// decide whether to run gc and how to phrase the trailing line.
    ///
    /// `ChainAbsent` on any single ref is downgraded to a warn-and-skip
    /// so a concurrent `delete-branch` (or any deletion between the
    /// `resolve_targets` snapshot and a ref's turn in the loop) does
    /// not abort the entire run and rob the operator of the work that
    /// would otherwise complete for the surviving refs. Every other
    /// `PackchainError` variant — and any `ManageError` — still aborts:
    /// a transport failure or auth error signals the bucket itself is
    /// untrustworthy, not just one vanished ref.
    async fn compact_targets_into<W: Write>(
        &self,
        out: &mut W,
        targets: &[RefName],
        compact_opts: PackchainCompactOpts,
    ) -> Result<RunSummary, ManageError> {
        let mut summary = RunSummary::default();
        for ref_name in targets {
            match compact::compact(
                Arc::clone(&self.store),
                self.prefix_opt(),
                ref_name,
                compact_opts,
            )
            .await
            {
                Ok(outcome) => {
                    write_outcome(out, &outcome)?;
                    if matches!(outcome.action, CompactAction::Compacted) {
                        summary.any_compacted = true;
                    }
                }
                Err(PackchainError::ChainAbsent { ref_name: r }) => {
                    // Concurrent `delete-branch` (or any deletion path)
                    // removed this ref's `chain.json` between the
                    // `resolve_targets` snapshot and its turn in the
                    // loop. Without this carve-out, a single vanished
                    // ref would abort the entire compact run and the
                    // surviving refs would never be touched (issue
                    // #160). The skip is operator-visible both on
                    // stdout and via `tracing::warn` so a vanished ref
                    // never disappears silently.
                    summary.skipped_vanished += 1;
                    warn!(
                        ref_path = %r,
                        "compact: chain.json vanished between target selection and per-ref compact \
                         (concurrent delete?); skipping",
                    );
                    writeln!(
                        out,
                        "compact: {r} vanished between selection and compact (concurrent delete?); skipping",
                    )?;
                }
                Err(e) => return Err(ManageError::Packchain(e)),
            }
        }
        if summary.skipped_vanished > 0 {
            writeln!(
                out,
                "compact: skipped {} vanished ref(s) (chain.json removed concurrently).",
                summary.skipped_vanished,
            )?;
        }
        Ok(summary)
    }

    /// Compute the list of ref names to compact. `--ref` short-
    /// circuits to a single ref; otherwise scan via the audit and
    /// prompt the operator to confirm the candidate list.
    async fn resolve_targets<W: Write>(&self, out: &mut W) -> Result<Vec<RefName>, ManageError> {
        if let Some(name) = &self.opts.ref_name {
            let ref_name =
                RefName::new(name).map_err(|_| ManageError::InvalidBranch(name.clone()))?;
            return Ok(vec![ref_name]);
        }

        let report = self.audit_for_compaction_candidates().await?;
        let candidates: Vec<&BranchRow> = report
            .branches
            .iter()
            .filter(|r| r.recommend_compact)
            .collect();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        writeln!(out, "Branches recommended for compaction:")?;
        for row in &candidates {
            writeln!(
                out,
                "  - {}: {} segment(s), {} byte(s)",
                row.ref_path, row.segments_total, row.bytes_total,
            )?;
        }
        if !self.prompter.confirm("Compact all of the above?")? {
            writeln!(out, "Aborted")?;
            return Err(ManageError::Cancelled);
        }

        let mut resolved = Vec::with_capacity(candidates.len());
        for row in candidates {
            let ref_name = RefName::new(&row.ref_path)
                .map_err(|_| ManageError::InvalidBranch(row.ref_path.clone()))?;
            resolved.push(ref_name);
        }
        Ok(resolved)
    }

    /// Walk the bucket once and derive the audit report we use for
    /// candidate selection. Mirrors the doctor's audit flow.
    async fn audit_for_compaction_candidates(&self) -> Result<AuditReport, ManageError> {
        let list_prefix = keys::join(Some(&self.prefix), "");
        let objects = self.store.list(&list_prefix).await?;
        audit::audit(self.store.as_ref(), &self.prefix, &objects)
            .await
            .map_err(ManageError::from)
    }

    async fn run_gc<W: Write>(&self, out: &mut W) -> Result<(), ManageError> {
        let store_ref = self.store.as_ref();
        let mark = gc::mark(store_ref, &self.prefix, gc::MarkOpts::default()).await?;
        format_mark_outcome(out, &mark)?;
        if mark.orphan_count != 0 {
            info!(
                run_id = %mark.run_id,
                key = %mark.tombstone_key,
                "compact --with-gc: mark completed",
            );
        }
        let grace_hours = self
            .opts
            .gc_grace_hours
            .unwrap_or_else(gc::grace_hours_from_env);
        let sweep = gc::sweep(
            store_ref,
            &self.prefix,
            gc::SweepOpts {
                grace_hours,
                force: false,
            },
        )
        .await?;
        format_sweep_outcome(out, &sweep)?;
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

fn write_outcome<W: Write>(out: &mut W, outcome: &CompactOutcome) -> std::io::Result<()> {
    match outcome.action {
        CompactAction::Compacted => {
            let new_pack = outcome.new_pack_sha.as_deref().unwrap_or("?");
            writeln!(
                out,
                "compact: {} rewritten to single segment (was {} segment(s), {} byte(s); new pack {} at {} byte(s))",
                outcome.ref_path,
                outcome.prior_segments,
                outcome.prior_bytes,
                new_pack,
                outcome.new_pack_bytes,
            )
        }
        CompactAction::SkippedUnderThreshold => writeln!(
            out,
            "compact: {} below heuristic ({} segment(s), {} byte(s)); skipping. Use --force to compact unconditionally.",
            outcome.ref_path, outcome.prior_segments, outcome.prior_bytes,
        ),
        CompactAction::AlreadyMinimal => writeln!(
            out,
            "compact: {} already a single-segment chain at the tip; nothing to do.",
            outcome.ref_path,
        ),
        CompactAction::LockContended => writeln!(
            out,
            "compact: {} per-ref lock is held by another client; try again later.",
            outcome.ref_path,
        ),
    }
}

/// Aggregate over the per-ref loop. Drives both the `with_gc` decision
/// and the trailing "skipped N vanished ref(s)" line.
#[derive(Debug, Default)]
struct RunSummary {
    /// At least one ref produced a [`CompactAction::Compacted`]
    /// outcome. Gates the `with_gc` mark/sweep — if nothing changed,
    /// gc has no orphans to reap.
    any_compacted: bool,
    /// Count of refs that vanished between target selection and the
    /// per-ref compact (issue #160). Surfaced in the trailing summary
    /// line so the operator sees the skip count even if the warn line
    /// scrolled off.
    skipped_vanished: usize,
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
            CompactOpts {
                ref_name: Some("refs/heads/../etc/passwd".to_owned()),
                ..CompactOpts::default()
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
        let runner = Compact::new(store_arc(&mock), "repo", CompactOpts::default(), &prompter);
        runner.run().await.expect("no-candidate run is Ok");
    }

    #[tokio::test]
    async fn vanished_ref_is_skipped_and_other_targets_still_compact() {
        // Issue #160 regression: `manage compact` snapshots the target
        // list once and then loops `compact()` per ref. If a ref is
        // deleted between the snapshot and its turn — exactly the
        // window a concurrent `delete-branch` opens — the underlying
        // engine returns `PackchainError::ChainAbsent`. Pre-fix, the
        // `?` on that call aborted the whole run; surviving refs were
        // robbed of work the operator had explicitly confirmed.
        //
        // Fixture: two refs that are already-minimal single-segment
        // chains at the tip. The compact engine short-circuits both
        // through `AlreadyMinimal` without touching packs/bundles —
        // sufficient to prove the per-ref loop continues across the
        // ChainAbsent skip. The dev ref's chain.json is deleted
        // before `compact_targets_into` runs, mirroring the
        // post-snapshot / pre-per-ref deletion window from the bug.
        let mock = MockStore::new();
        let single_segment_chain = |sha: &str| crate::packchain::schema::ChainManifest {
            v: 1,
            tip: crate::packchain::schema::Sha40::try_new(sha).unwrap(),
            full_at: crate::packchain::schema::Sha40::try_new(sha).unwrap(),
            segments: vec![crate::packchain::schema::ChainSegment {
                sha: crate::packchain::schema::Sha40::try_new(sha).unwrap(),
                parent_sha: None,
                pack: format!("packs/{sha}.pack"),
                bytes: 1024,
            }],
        };
        let main = crate::git::RefName::new("refs/heads/main").unwrap();
        let dev = crate::git::RefName::new("refs/heads/dev").unwrap();
        crate::packchain::manifest::write_chain(
            &mock,
            Some("repo"),
            &main,
            &single_segment_chain("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        )
        .await
        .unwrap();
        crate::packchain::manifest::write_chain(
            &mock,
            Some("repo"),
            &dev,
            &single_segment_chain("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        )
        .await
        .unwrap();

        // Simulate a concurrent `delete-branch` removing dev's
        // `chain.json` AFTER the target snapshot but BEFORE its turn
        // in the loop. The fixture sets force=true to bypass the
        // heuristic so an already-minimal chain still gets a real
        // `compact()` call (which short-circuits in the engine).
        assert!(
            mock.remove_key("repo/refs/heads/dev/chain.json"),
            "fixture invariant: dev's chain.json must be present pre-delete",
        );

        let prompter = ScriptedPrompter::new([]);
        let runner = Compact::new(
            store_arc(&mock),
            "repo",
            CompactOpts {
                force: true,
                ..CompactOpts::default()
            },
            &prompter,
        );
        let compact_opts = PackchainCompactOpts {
            force: true,
            lock_ttl: Duration::seconds(60),
        };
        // Drive the per-ref loop directly with a stable target order
        // so the test asserts deterministic output regardless of
        // audit-driven ordering.
        let targets = vec![dev.clone(), main.clone()];
        let mut out: Vec<u8> = Vec::new();
        let summary = runner
            .compact_targets_into(&mut out, &targets, compact_opts)
            .await
            .expect("vanished ref must NOT abort the run");
        let stdout = String::from_utf8(out).expect("operator output is utf-8");

        // Pre-fix: this assertion fails because `compact_targets_into`
        // returns `Err(Packchain(ChainAbsent { … }))` and we never
        // even reach the second target. Post-fix: the run completes
        // with one vanished skip and one AlreadyMinimal.
        assert_eq!(summary.skipped_vanished, 1);
        assert!(
            !summary.any_compacted,
            "AlreadyMinimal must not flip any_compacted",
        );
        assert!(
            stdout.contains(
                "compact: refs/heads/dev vanished between selection and compact \
                 (concurrent delete?); skipping",
            ),
            "operator must see the per-ref vanished line for dev; got: {stdout}",
        );
        assert!(
            stdout.contains(
                "compact: refs/heads/main already a single-segment chain at the tip; nothing to do.",
            ),
            "the surviving ref's outcome must still print after the vanished skip; got: {stdout}",
        );
        assert!(
            stdout.contains("compact: skipped 1 vanished ref(s)"),
            "trailing summary must surface the skip count; got: {stdout}",
        );
    }

    #[tokio::test]
    async fn non_chainabsent_packchain_error_still_aborts_run() {
        // Mirror of the vanished-ref test that proves the carve-out is
        // narrow: only `ChainAbsent` downgrades. Any other
        // `PackchainError` — here, a malformed chain.json that the
        // engine surfaces as `Json` — must still abort the run so a
        // genuine bucket-level fault is not silently swallowed.
        let mock = MockStore::new();
        let main = crate::git::RefName::new("refs/heads/main").unwrap();
        // A non-empty, non-JSON body trips the deserialise path in
        // `load_chain`; the engine surfaces this as
        // `PackchainError::Json`, which the management layer must
        // propagate verbatim.
        mock.insert(
            "repo/refs/heads/main/chain.json",
            bytes::Bytes::from_static(b"not valid json"),
        );

        let prompter = ScriptedPrompter::new([]);
        let runner = Compact::new(
            store_arc(&mock),
            "repo",
            CompactOpts {
                force: true,
                ..CompactOpts::default()
            },
            &prompter,
        );
        let compact_opts = PackchainCompactOpts {
            force: true,
            lock_ttl: Duration::seconds(60),
        };
        let mut out: Vec<u8> = Vec::new();
        let err = runner
            .compact_targets_into(&mut out, std::slice::from_ref(&main), compact_opts)
            .await
            .expect_err("non-ChainAbsent engine error must abort the run");
        assert!(
            matches!(err, ManageError::Packchain(_)),
            "expected ManageError::Packchain, got {err:?}",
        );
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
            CompactOpts {
                ref_name: Some("refs/heads/main".to_owned()),
                with_gc: true,
                ..CompactOpts::default()
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

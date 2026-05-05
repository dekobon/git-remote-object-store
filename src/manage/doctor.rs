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

use super::snapshot::{BundleEntry, RepoSnapshot, analyze_objects};
use super::{DEFAULT_LOCK_TTL_SECONDS, ManageError, Prompter};
use crate::keys;
use crate::object_store::{ObjectMeta, ObjectStore, PutOpts};
use crate::packchain::audit::{self, AuditReport, BranchAuditRow};
use crate::url::StorageEngine;

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
    /// Engine resolved from the bucket's `FORMAT` key. Drives
    /// engine-aware reporting: a `Packchain` value enables the
    /// orphan / tombstone / compaction / dangling-reference section.
    /// Bundle remotes see the existing report unchanged.
    pub engine: StorageEngine,
}

impl Default for DoctorOpts {
    fn default() -> Self {
        Self {
            delete_bundle: false,
            lock_ttl_seconds: DEFAULT_LOCK_TTL_SECONDS,
            delete_stale_locks: false,
            engine: StorageEngine::Bundle,
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
    /// repository prefix without a trailing `/`. Pass an empty string
    /// for repositories stored at the bucket/container root.
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
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::Store`] if an object-store call fails,
    /// [`ManageError::Internal`] if a prompter returns an out-of-range
    /// index, [`ManageError::Cancelled`] if the user cancels an interactive
    /// prompt, or [`ManageError::Io`] for prompt I/O failures.
    pub async fn run(&self) -> Result<(), ManageError> {
        // Share one LIST between snapshot analysis and stale-lock
        // scanning so a doctor run is a single bucket walk regardless
        // of repo size. Empty `prefix` (root-of-bucket repo) collapses
        // to a bucket-wide list.
        let list_prefix = keys::join(&self.prefix, "");
        let objects = self.store.list(&list_prefix).await?;
        let mut snapshot = analyze_objects(&objects, &list_prefix, &self.store).await?;
        print!("{}", self.report(&snapshot));

        // Engine-aware diagnostic. The packchain section is purely
        // read-only — no bucket mutations — so it runs before any of
        // the fixers below to keep the report ordering stable
        // regardless of what fixers do later.
        if matches!(self.opts.engine, StorageEngine::Packchain) {
            let report = audit::audit(&*self.store, &self.prefix).await?;
            print!("{}", render_packchain_section(&report));
        }

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

        self.list_and_handle_stale_locks(&objects).await?;
        Ok(())
    }

    /// Render the snapshot to a human-readable report. Returns the
    /// finished string (with trailing newline) so callers can route
    /// it to stdout, a logger, or a test buffer.
    #[must_use]
    pub(crate) fn report(&self, snapshot: &RepoSnapshot) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let _ = writeln!(out, "{}:", self.report_label());
        for (ref_path, r) in &snapshot.refs {
            let star = if r.is_protected { "*" } else { "" };
            let status = match r.bundles.len() {
                0 => "No bundles",
                1 => "Ok",
                _ => "Multiple bundles",
            };
            let _ = writeln!(out, " {star} {ref_path}: {status}");
        }
        let head_label = snapshot
            .head
            .as_deref()
            .filter(|h| snapshot.refs.contains_key(*h))
            .unwrap_or("Invalid");
        let _ = writeln!(out, "  HEAD: {head_label}");
        out
    }

    /// Human-readable label for the repo in printed output. Empty
    /// `prefix` (root-of-bucket repo) renders as `(root)` so the report
    /// header isn't a bare colon.
    fn report_label(&self) -> &str {
        if self.prefix.is_empty() {
            "(root)"
        } else {
            &self.prefix
        }
    }

    async fn fix_multiple_bundles(
        &self,
        snapshot: &mut RepoSnapshot,
        ref_path: &str,
    ) -> Result<(), ManageError> {
        println!(
            "\nFix multiple bundles for repo {} and ref {ref_path}",
            self.report_label()
        );

        // The caller filtered for refs with `bundles.len() > 1`; if the
        // map shape changed in between, surface a structured internal
        // error rather than aborting the process.
        let ref_entry = snapshot.refs.get_mut(ref_path).ok_or_else(|| {
            ManageError::Internal(format!(
                "fix_multiple_bundles called with ref {ref_path} absent from snapshot"
            ))
        })?;

        let labels: Vec<String> = ref_entry
            .bundles
            .iter()
            .map(|b| format!("{} {}", b.sha, b.last_modified))
            .collect();

        let keep_idx = self.prompter.select("Choose the bundle to keep", &labels)?;
        // `dialoguer::Select` validates the index against the option count
        // before returning, so out-of-range here means a test prompter
        // queued an invalid script. Propagate as a structured internal
        // error so the helper doesn't abort the whole run.
        let keeper_sha = ref_entry
            .bundles
            .get(keep_idx)
            .ok_or_else(|| {
                ManageError::Internal(format!(
                    "prompter returned out-of-range index {keep_idx} for {} bundle(s)",
                    ref_entry.bundles.len()
                ))
            })?
            .sha
            .clone();

        if !self.prompter.confirm("Confirm and apply changes")? {
            // Match `ManageBranch::delete`: an interactive "no" is the user
            // declining this fix, not an abort of the whole run. Doctor
            // continues to the next ref / stale-lock scan with exit 0.
            println!("Aborted");
            return Ok(());
        }

        println!("Keeping {keeper_sha}");
        // Partition into (keep, evict). The snapshot is updated in place
        // so subsequent steps (HEAD validation in particular) see the
        // resolved layout.
        let bundles = std::mem::take(&mut ref_entry.bundles);
        let (keepers, losers): (Vec<_>, Vec<_>) =
            bundles.into_iter().partition(|b| b.sha == keeper_sha);
        ref_entry.bundles = keepers;

        for losing in &losers {
            self.evict_losing_bundle(ref_path, losing).await?;
        }
        Ok(())
    }

    async fn evict_losing_bundle(
        &self,
        ref_path: &str,
        losing: &BundleEntry,
    ) -> Result<(), ManageError> {
        // Both branches end with `self.store.delete(&losing.key)` (after
        // this `if/else`): the bundle is always removed from its losing
        // location. The branches differ only in whether the bundle is
        // first quarantined under a new ref. Adding a "dry-run" or
        // "preserve in place" branch here must NOT fall through to the
        // unconditional delete below — keep the delete inside the
        // appropriate branch when adding new modes.
        if self.opts.delete_bundle {
            println!("Removing {}", losing.sha);
        } else {
            // `Uuid::Simple`'s `Display` impl does NOT honor the
            // precision specifier (`{:.8}`), so encode into a stack
            // buffer and slice to 8 chars — mirroring upstream's
            // `str(uuid.uuid4())[:8]` to produce the `<ref>_<uuid8>`
            // quarantine ref name.
            let mut buf = [0u8; uuid::fmt::Simple::LENGTH];
            let suffix = &Uuid::new_v4().simple().encode_lower(&mut buf)[..8];
            let new_ref = format!("{ref_path}_{suffix}");
            // `keys::bundle_key` accepts an `Option<&str>` prefix and
            // collapses `Some("")` to the no-prefix shape; passing
            // `Some(&self.prefix)` therefore handles both the prefixed
            // and root-of-bucket cases without an explicit branch.
            let dst_key = keys::bundle_key(Some(&self.prefix), &new_ref, &losing.sha);
            println!("Moving {} to new branch {new_ref}", losing.sha);
            self.store.copy(&losing.key, &dst_key).await?;
        }
        self.store.delete(&losing.key).await?;
        Ok(())
    }

    async fn fix_head(&self, snapshot: &mut RepoSnapshot) -> Result<(), ManageError> {
        println!("\nFix invalid HEAD for repo {}", self.report_label());

        let candidates: Vec<&str> = snapshot
            .refs
            .keys()
            .filter(|k| k.starts_with("refs/heads/"))
            .map(String::as_str)
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
        // `dialoguer::Select` cannot return an out-of-range index; an
        // out-of-range answer here is a test-script bug (see
        // `fix_multiple_bundles`). Surface as a structured internal
        // error rather than aborting the process.
        let new_head = candidates
            .get(chosen)
            .copied()
            .ok_or_else(|| {
                ManageError::Internal(format!(
                    "prompter returned out-of-range index {chosen} for {} HEAD candidate(s)",
                    candidates.len()
                ))
            })?
            .to_owned();

        let head_key = keys::join(&self.prefix, "HEAD");
        println!("Setting {new_head} as HEAD");
        self.store
            .put_bytes(&head_key, Bytes::from(new_head.clone()), PutOpts::default())
            .await?;
        snapshot.head = Some(new_head);
        Ok(())
    }

    async fn list_and_handle_stale_locks(&self, objects: &[ObjectMeta]) -> Result<(), ManageError> {
        println!("\nScanning for stale locks...");
        let now = OffsetDateTime::now_utc();
        let ttl = Duration::from_secs(self.opts.lock_ttl_seconds);

        let stale: Vec<(&str, Duration)> = objects
            .iter()
            .filter(|o| super::is_lock_key(&o.key))
            .filter_map(|o| {
                let elapsed = Duration::try_from(now - o.last_modified).ok()?;
                (elapsed > ttl).then_some((o.key.as_str(), elapsed))
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

/// Last `/`-separated segment of a ref path, used as the human label in
/// `fix_head`'s branch picker. Returns the full path if it has no
/// slashes (e.g. a single-component ref).
///
/// `unwrap_or(full)` is the identity fallback: `rsplit` always yields
/// at least one element, so the fallback is unreachable for any
/// non-empty input — but if `rsplit`'s contract ever relaxes (or the
/// input is empty), returning the input verbatim is strictly safer
/// than panicking, since the caller only uses the result as a display
/// label.
fn short_branch_name(full: &str) -> &str {
    full.rsplit('/').next().unwrap_or(full)
}

/// Render the packchain audit section. The output ends with a trailing
/// newline so callers can `print!` it without manual spacing. The shape
/// is the one specified in #68: a header line, then four sub-sections
/// (orphans, tombstones, branches needing compaction, errors).
fn render_packchain_section(report: &AuditReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "\n=== Packchain ===");
    let _ = writeln!(
        out,
        "Orphans: {} pack(s), {}",
        report.orphans.pack_count,
        format_bytes(report.orphans.bytes),
    );

    if report.tombstones.is_empty() {
        let _ = writeln!(out, "Tombstones (pending sweep): none");
    } else {
        let _ = writeln!(out, "Tombstones (pending sweep):");
        for t in &report.tombstones {
            let age = format_age(t.age_hours);
            let _ = writeln!(
                out,
                "  - run id {}, marked {} ({}), {} pack(s)",
                t.run_id, t.marked_at, age, t.orphan_count,
            );
        }
    }

    let candidates: Vec<&BranchAuditRow> = report
        .branches
        .iter()
        .filter(|r| r.recommend_compact)
        .collect();
    if candidates.is_empty() {
        let _ = writeln!(out, "Branches needing compaction: none");
    } else {
        let _ = writeln!(out, "Branches needing compaction:");
        for r in candidates {
            let _ = writeln!(
                out,
                "  - {}: {} segment(s), {} since full_at  [recommend compact]",
                r.ref_path,
                r.segments_total,
                format_bytes(r.bytes_total),
            );
        }
    }

    let has_corrupt = report.branches.iter().any(|r| !r.has_full_at_segment);
    if report.dangling.is_empty() && !has_corrupt {
        let _ = writeln!(out, "ERRORS: none");
    } else {
        let _ = writeln!(out, "ERRORS:");
        for d in &report.dangling {
            let _ = writeln!(
                out,
                "  - {}/chain.json references missing pack {}",
                d.ref_path, d.missing_pack_key,
            );
        }
        for b in report.branches.iter().filter(|r| !r.has_full_at_segment) {
            let _ = writeln!(
                out,
                "  - {}/chain.json full_at not present in segments (corrupt manifest)",
                b.ref_path,
            );
        }
    }
    out
}

/// Human-readable byte total. Reports MiB / GiB above 1 MiB and bare
/// bytes below; rounds to a single decimal for the larger units. The
/// units mirror the numbers operators see in `gc` output and the issue
/// thresholds (100 MiB, etc.) so the report stays diff-comparable.
fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1_024;
    const MIB: u64 = 1_024 * KIB;
    const GIB: u64 = 1_024 * MIB;
    if bytes >= GIB {
        #[allow(clippy::cast_precision_loss)]
        let g = bytes as f64 / GIB as f64;
        format!("{g:.1} GiB")
    } else if bytes >= MIB {
        #[allow(clippy::cast_precision_loss)]
        let m = bytes as f64 / MIB as f64;
        format!("{m:.1} MiB")
    } else if bytes >= KIB {
        #[allow(clippy::cast_precision_loss)]
        let k = bytes as f64 / KIB as f64;
        format!("{k:.1} KiB")
    } else {
        format!("{bytes} B")
    }
}

/// Render an age in whole hours. Negative values (clock skew) are
/// reported as "<1h" so the line stays human-readable without leaking
/// signed-arithmetic semantics into the report.
fn format_age(hours: i64) -> String {
    if hours <= 0 {
        "<1h".to_owned()
    } else if hours < 48 {
        format!("{hours}h ago")
    } else {
        format!("{}d ago", hours / 24)
    }
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
        let initial_keys = mock.keys();
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        doctor.run().await.expect("doctor.run");
        // A clean run must not mutate the bucket — no objects added,
        // moved, or removed. This catches a `Doctor::run` regressed to
        // a no-op as well as one that over-eagerly fixes a non-issue.
        assert_eq!(mock.keys(), initial_keys);
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
        // The new quarantine ref has a key with the moved bundle, and
        // the suffix is exactly 8 lowercase hex characters
        // (`<ref>_<uuid8>`).
        let moved = mock
            .keys()
            .into_iter()
            .find(|k| k.starts_with("myrepo/refs/heads/main_") && k.ends_with("/bbbbbbbb.bundle"))
            .expect("quarantine key created");
        let suffix = moved
            .strip_prefix("myrepo/refs/heads/main_")
            .and_then(|rest| rest.strip_suffix("/bbbbbbbb.bundle"))
            .expect("quarantine key matches `<ref>_<suffix>/<sha>.bundle`");
        assert_eq!(suffix.len(), 8, "expected 8-char suffix, got {suffix:?}");
        assert!(
            suffix
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "expected lowercase hex suffix, got {suffix:?}"
        );
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
        // User-no on the confirmation declines this fix but is not an
        // abort of the whole run — the doctor continues to scan stale
        // locks and exits 0. Both bundles must remain untouched.
        doctor.run().await.expect("user-no should not error");
        assert!(mock.contains("myrepo/refs/heads/main/aaa.bundle"));
        assert!(mock.contains("myrepo/refs/heads/main/bbb.bundle"));
    }

    #[tokio::test]
    async fn fix_multiple_bundles_out_of_range_select_returns_internal_error() {
        // A scripted prompter that returns an out-of-range index used to
        // panic the process via `expect`. The defensive path now returns
        // a structured `ManageError::Internal` so the helper / management
        // CLI can surface the bug without aborting (issue #33).
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("myrepo/refs/heads/main/aaa.bundle", Bytes::from("a"));
        mock.insert("myrepo/refs/heads/main/bbb.bundle", Bytes::from("b"));
        // Two bundles → valid indices are 0 and 1; 99 is out of range.
        let prompter = ScriptedPrompter::new([Answer::Select(99)]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let err = doctor
            .run()
            .await
            .expect_err("out-of-range index propagates");
        assert!(
            matches!(err, ManageError::Internal(ref msg) if msg.contains("out-of-range")),
            "expected ManageError::Internal, got {err:?}",
        );
    }

    #[tokio::test]
    async fn fix_head_out_of_range_select_returns_internal_error() {
        // Sibling guard for the HEAD-candidate prompter path (#33). The
        // doctor reaches `fix_head` when the existing HEAD is invalid (or
        // the bucket has no HEAD object) AND there is at least one
        // `refs/heads/*` candidate to assign. An out-of-range script
        // index from a bad test prompter must surface as
        // `ManageError::Internal`, not panic the process.
        let mock = MockStore::new();
        // No HEAD object → snapshot.is_head_valid() is false.
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("b"));
        mock.insert("myrepo/refs/heads/dev/def.bundle", Bytes::from("c"));
        // Two HEAD candidates → valid indices are 0 and 1; 42 is out of
        // range. No prior bundle-fix prompts because no ref has > 1
        // bundle.
        let prompter = ScriptedPrompter::new([Answer::Select(42)]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let err = doctor
            .run()
            .await
            .expect_err("out-of-range HEAD index propagates");
        assert!(
            matches!(err, ManageError::Internal(ref msg) if msg.contains("HEAD candidate")),
            "expected ManageError::Internal naming HEAD candidate, got {err:?}",
        );
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

    #[tokio::test]
    async fn report_renders_protected_multi_bundle_and_invalid_head() {
        // Build a snapshot covering every report-line shape: a
        // protected ref with one bundle, a duplicate-bundle ref, an
        // empty ref, plus a HEAD body that does not match any ref so
        // the trailing label reads `Invalid`.
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/missing"));
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("b"));
        mock.insert("myrepo/refs/heads/main/PROTECTED#", Bytes::new());
        mock.insert("myrepo/refs/heads/dev/aaa.bundle", Bytes::from("a"));
        mock.insert("myrepo/refs/heads/dev/bbb.bundle", Bytes::from("a"));
        mock.insert("myrepo/refs/heads/empty/PROTECTED#", Bytes::new());

        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let snapshot = super::analyze_objects(
            &mock.list("myrepo/").await.expect("list"),
            "myrepo/",
            &store_arc(&mock),
        )
        .await
        .expect("analyze");

        let report = doctor.report(&snapshot);
        assert_eq!(
            report,
            "myrepo:\n  \
             refs/heads/dev: Multiple bundles\n \
             * refs/heads/empty: No bundles\n \
             * refs/heads/main: Ok\n  \
             HEAD: Invalid\n",
        );
    }

    #[tokio::test]
    async fn report_renders_valid_head_as_ref_label() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("b"));
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let snapshot = super::analyze_objects(
            &mock.list("myrepo/").await.expect("list"),
            "myrepo/",
            &store_arc(&mock),
        )
        .await
        .expect("analyze");

        let report = doctor.report(&snapshot);
        assert_eq!(
            report,
            "myrepo:\n  refs/heads/main: Ok\n  HEAD: refs/heads/main\n",
        );
    }

    // --- Root-of-bucket (empty prefix) coverage --------------------------

    #[tokio::test]
    async fn root_prefix_clean_run_does_not_mutate_bucket() {
        // Repo lives at the bucket root — keys have no `<prefix>/`
        // segment. A regression that re-introduces the leading slash
        // would surface either as "no objects found" (the doctor's
        // listing key would be `/`, which matches nothing) or as the
        // doctor failing to identify the bundle by its relative path.
        let mock = MockStore::new();
        mock.insert("HEAD", Bytes::from("refs/heads/main"));
        mock.insert("refs/heads/main/abc.bundle", Bytes::from("body"));
        let initial_keys = mock.keys();
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "", DoctorOpts::default(), &prompter);
        doctor.run().await.expect("doctor.run at root");
        assert_eq!(mock.keys(), initial_keys);
    }

    #[tokio::test]
    async fn root_prefix_fix_head_writes_to_root_head_key() {
        // No HEAD object → fix_head writes one. The key must be the
        // bare `HEAD`, not `/HEAD`.
        let mock = MockStore::new();
        mock.insert("refs/heads/main/abc.bundle", Bytes::from("b"));
        let prompter = ScriptedPrompter::new([Answer::Select(0)]);
        let doctor = Doctor::new(store_arc(&mock), "", DoctorOpts::default(), &prompter);
        doctor.run().await.expect("doctor.run at root");

        let head_bytes = mock.get_bytes("HEAD").await.expect("HEAD at root");
        assert_eq!(&head_bytes[..], b"refs/heads/main");
        assert!(!mock.contains("/HEAD"), "no leading-slash HEAD key");
    }

    #[tokio::test]
    async fn root_prefix_fix_multiple_bundles_quarantines_at_root() {
        let mock = MockStore::new();
        mock.insert("HEAD", Bytes::from("refs/heads/main"));
        mock.insert("refs/heads/main/aaa.bundle", Bytes::from("a"));
        mock.insert("refs/heads/main/bbb.bundle", Bytes::from("b"));
        let prompter = ScriptedPrompter::new([Answer::Select(0), Answer::Confirm(true)]);
        let doctor = Doctor::new(store_arc(&mock), "", DoctorOpts::default(), &prompter);
        doctor.run().await.expect("doctor.run at root");

        // Loser was moved to a quarantine ref `refs/heads/main_<uuid8>`,
        // and the destination key has no leading slash.
        let moved = mock
            .keys()
            .into_iter()
            .find(|k| k.starts_with("refs/heads/main_") && k.ends_with("/bbb.bundle"))
            .expect("quarantine key created at root");
        assert!(
            !moved.starts_with('/'),
            "quarantine key must not have a leading slash: {moved:?}"
        );
    }

    // --- Packchain section --------------------------------------------

    fn packchain_opts() -> DoctorOpts {
        DoctorOpts {
            engine: StorageEngine::Packchain,
            ..DoctorOpts::default()
        }
    }

    #[tokio::test]
    async fn bundle_engine_does_not_emit_packchain_section() {
        // Default engine is Bundle; no orphan/tombstone scan should
        // trigger. Capture rendering by re-using the pre-/post-fix
        // bundle assertions: the existing `report` already covers
        // bundle-shape output, and a stale-lock-free clean run does
        // not mutate the bucket — we verify the bucket is unchanged
        // (no spurious read of `gc/` or `packs/`).
        let mock = MockStore::new();
        mock.insert("repo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("repo/refs/heads/main/abc.bundle", Bytes::from("b"));
        let initial_keys = mock.keys();
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "repo", DoctorOpts::default(), &prompter);
        doctor.run().await.expect("clean bundle run");
        assert_eq!(mock.keys(), initial_keys);
    }

    #[tokio::test]
    async fn packchain_engine_renders_section_with_orphan_and_tombstone() {
        // Build a packchain shape: one chain.json + one orphan pack +
        // one tombstone. The audit returns all three, and the doctor
        // renders them.
        let mock = MockStore::new();
        // HEAD so the bundle-shape `fix_head` prompt path doesn't fire.
        mock.insert("repo/HEAD", Bytes::from("refs/heads/main"));
        // A bundle keeps the bundle-shape ref entry valid for the
        // legacy snapshot analyser (which doesn't yet understand
        // chain.json as a tip indicator).
        mock.insert(
            "repo/refs/heads/main/0000000000000000000000000000000000000001.bundle",
            Bytes::from("baseline"),
        );
        // chain.json so the audit has a branch row.
        mock.insert(
            "repo/refs/heads/main/chain.json",
            Bytes::from(
                r#"{"v":1,"tip":"0000000000000000000000000000000000000001","full_at":"0000000000000000000000000000000000000001","segments":[{"sha":"0000000000000000000000000000000000000001","parent_sha":null,"pack":"packs/1111111111111111111111111111111111111111.pack","bytes":1024}]}"#,
            ),
        );
        // Live pack referenced by the chain.
        mock.insert(
            "repo/packs/1111111111111111111111111111111111111111.pack",
            Bytes::from_static(b"live"),
        );
        // Orphan pack (not in any chain).
        mock.insert(
            "repo/packs/2222222222222222222222222222222222222222.pack",
            Bytes::from_static(b"orphan-body-len-eq-19"),
        );
        // Tombstone older than 1h.
        let marked_at = (OffsetDateTime::now_utc() - time::Duration::hours(2))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let tombstone_body = format!(
            r#"{{"v":1,"run_id":"abc-1","marked_at":"{marked_at}","orphan_packs":["2222222222222222222222222222222222222222"]}}"#
        );
        let tombstone_key = format!("repo/gc/tombstones-abc-1-{marked_at}.json");
        mock.insert(tombstone_key, Bytes::from(tombstone_body));

        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "repo", packchain_opts(), &prompter);
        doctor.run().await.expect("packchain doctor run");
    }

    #[test]
    fn render_packchain_section_lists_dangling_references_as_errors() {
        // Hand-roll an `AuditReport` so the renderer's behaviour is
        // pinned without going through the live store.
        let report = AuditReport {
            orphans: super::audit::OrphanReport::default(),
            tombstones: Vec::new(),
            branches: Vec::new(),
            dangling: vec![super::audit::DanglingRow {
                ref_path: "refs/heads/dev".to_owned(),
                missing_pack_key: "packs/abcdef0123456789abcdef0123456789abcdef01.pack".to_owned(),
            }],
        };
        let rendered = super::render_packchain_section(&report);
        assert!(rendered.contains("ERRORS:"));
        assert!(rendered.contains("references missing pack"));
        assert!(rendered.contains("refs/heads/dev"));
    }

    #[test]
    fn render_packchain_section_clean_bucket_says_none() {
        let report = AuditReport::default();
        let rendered = super::render_packchain_section(&report);
        assert!(rendered.contains("=== Packchain ==="));
        assert!(rendered.contains("Orphans: 0 pack(s)"));
        assert!(rendered.contains("Tombstones (pending sweep): none"));
        assert!(rendered.contains("Branches needing compaction: none"));
        assert!(rendered.contains("ERRORS: none"));
    }

    #[test]
    fn format_bytes_unit_boundaries() {
        assert_eq!(super::format_bytes(0), "0 B");
        assert_eq!(super::format_bytes(1023), "1023 B");
        assert_eq!(super::format_bytes(1024), "1.0 KiB");
        assert_eq!(super::format_bytes(1024 * 1024 - 1), "1024.0 KiB");
        assert_eq!(super::format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(super::format_bytes(1024 * 1024 * 1024), "1.0 GiB");
    }

    #[test]
    fn format_age_handles_clock_skew_and_rollover() {
        // Negative (future timestamp / clock skew) renders as "<1h".
        assert_eq!(super::format_age(-1), "<1h");
        // Zero hours likewise.
        assert_eq!(super::format_age(0), "<1h");
        // Just under the day rollover stays in hours.
        assert_eq!(super::format_age(1), "1h ago");
        assert_eq!(super::format_age(47), "47h ago");
        // Day rollover (>= 48h reports days).
        assert_eq!(super::format_age(48), "2d ago");
        assert_eq!(super::format_age(72), "3d ago");
    }

    #[test]
    fn render_packchain_section_compaction_candidate_is_flagged() {
        let report = AuditReport {
            orphans: super::audit::OrphanReport::default(),
            tombstones: Vec::new(),
            branches: vec![BranchAuditRow {
                ref_path: "refs/heads/main".to_owned(),
                segments_total: 27,
                bytes_total: 142 * 1024 * 1024,
                recommend_compact: true,
                has_full_at_segment: true,
            }],
            dangling: Vec::new(),
        };
        let rendered = super::render_packchain_section(&report);
        assert!(rendered.contains("refs/heads/main: 27 segment(s)"));
        assert!(rendered.contains("[recommend compact]"));
        assert!(rendered.contains("142.0 MiB"));
    }

    #[tokio::test]
    async fn root_prefix_report_renders_root_label() {
        // The first line of the report uses `(root)` so the empty
        // prefix doesn't produce a bare `:` header.
        let mock = MockStore::new();
        mock.insert("HEAD", Bytes::from("refs/heads/main"));
        mock.insert("refs/heads/main/abc.bundle", Bytes::from("b"));
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "", DoctorOpts::default(), &prompter);
        let snapshot = super::analyze_objects(
            &mock.list("").await.expect("list at root"),
            "",
            &store_arc(&mock),
        )
        .await
        .expect("analyze");

        let report = doctor.report(&snapshot);
        assert_eq!(
            report,
            "(root):\n  refs/heads/main: Ok\n  HEAD: refs/heads/main\n",
        );
    }
}

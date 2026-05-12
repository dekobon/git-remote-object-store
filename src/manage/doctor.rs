//! `doctor` analyzer + fixers.
//!
//! The flow is: analyze → write report → fix duplicate bundles per ref
//! → fix invalid HEAD → list and (optionally) delete stale locks.
//!
//! The `Doctor` value is constructed once per CLI invocation; all
//! interaction goes through the injected [`Prompter`] so the same code
//! path drives both the binary (via [`DialoguerPrompter`]) and unit
//! tests (via `ScriptedPrompter`, gated on `test-util`).
//!
//! All human-readable output flows through [`Doctor::run_into`]'s
//! `impl Write` parameter so tests can capture exact bytes without
//! spawning the management binary. [`Doctor::run`] is the thin
//! public wrapper that passes [`std::io::stdout()`] (per-write
//! locking, keeping the future `Send`).
//!
//! [`DialoguerPrompter`]: super::DialoguerPrompter

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use time::OffsetDateTime;
use tracing::{info, warn};
use uuid::Uuid;

use super::snapshot::{BundleEntry, MalformedBundleKey, RepoSnapshot, analyze_objects};
use super::{DEFAULT_LOCK_TTL_SECONDS, ManageError, Prompter};
use crate::keys;
use crate::object_store::{ObjectMeta, ObjectStore, ObjectStoreError, PutOpts};
use crate::packchain::audit::{self, AuditReport, BranchRow};
use crate::url::StorageEngine;

/// Tunables for [`Doctor::run`].
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

    /// Analyze, report, and fix — writing human-readable output to
    /// stdout.
    ///
    /// Thin wrapper around [`run_into`](Self::run_into) that passes
    /// [`std::io::stdout()`]. Each write acquires the stdout lock
    /// individually, keeping the returned future `Send`. Use
    /// `run_into` directly when you need to capture output (e.g. in
    /// tests).
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::Store`] if an object-store call fails,
    /// [`ManageError::Internal`] if a prompter returns an out-of-range
    /// index, [`ManageError::Cancelled`] if the user cancels an interactive
    /// prompt, or [`ManageError::Io`] for prompt or write I/O failures.
    pub async fn run(&self) -> Result<(), ManageError> {
        self.run_into(&mut std::io::stdout()).await
    }

    /// Analyze, report, and fix — writing human-readable output to
    /// `out`.
    ///
    /// Errors short-circuit the run — partial fixes are committed
    /// immediately (each `delete` / `copy` / `put` is its own request).
    /// This is a "best-effort" stance.
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::Store`] if an object-store call fails,
    /// [`ManageError::Internal`] if a prompter returns an out-of-range
    /// index, [`ManageError::Cancelled`] if the user cancels an interactive
    /// prompt, or [`ManageError::Io`] for prompt or write I/O failures.
    pub(crate) async fn run_into<W: Write>(&self, out: &mut W) -> Result<(), ManageError> {
        // Share one LIST between snapshot analysis, the packchain
        // audit, and stale-lock scanning so a doctor run is a single
        // bucket walk regardless of repo size. Empty `prefix`
        // (root-of-bucket repo) collapses to a bucket-wide list.
        let list_prefix = keys::join(Some(&self.prefix), "");
        let objects = self.store.list(&list_prefix).await?;
        let mut snapshot = analyze_objects(&objects, &list_prefix, &self.store).await?;
        write!(out, "{}", self.report(&snapshot))?;

        // Surface malformed bundle keys that push silently filters
        // (issue #124). Read-only: the doctor never deletes these, it
        // points the operator at the keys and lets them decide. This
        // sits between the snapshot report and the packchain section
        // so a bundle-engine repo (which has no packchain section)
        // still sees the warning right after its ref list.
        if !snapshot.malformed_bundle_keys.is_empty() {
            write!(
                out,
                "{}",
                render_malformed_bundles_section(&snapshot.malformed_bundle_keys),
            )?;
        }

        // Engine-aware diagnostic. The packchain section is purely
        // read-only — no bucket mutations — so it runs before any of
        // the fixers below to keep the report ordering stable
        // regardless of what fixers do later.
        if let Some(section) = self.maybe_render_packchain_section(&objects).await? {
            write!(out, "{section}")?;
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
            self.fix_multiple_bundles(out, &mut snapshot, &ref_path)
                .await?;
        }

        if !snapshot.is_head_valid() {
            self.fix_head(out, &mut snapshot).await?;
        }

        self.list_and_handle_stale_locks(out, &objects).await?;
        Ok(())
    }

    /// Run the packchain audit and render its section, but only when
    /// the configured engine is [`StorageEngine::Packchain`]. Returns
    /// `Ok(None)` for bundle remotes (cheap — no I/O).
    ///
    /// Exposed at `pub(crate)` so the tests can pin the engine gate
    /// without going through `run`'s `print!` side effect.
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::Packchain`] for engine-level failures
    /// during the audit.
    pub(crate) async fn maybe_render_packchain_section(
        &self,
        objects: &[ObjectMeta],
    ) -> Result<Option<String>, ManageError> {
        if !matches!(self.opts.engine, StorageEngine::Packchain) {
            return Ok(None);
        }
        let report = audit::audit(&*self.store, &self.prefix, objects).await?;
        Ok(Some(render_packchain_section(&report)))
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
                0 if r.has_chain => "Ok",
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

    async fn fix_multiple_bundles<W: Write>(
        &self,
        out: &mut W,
        snapshot: &mut RepoSnapshot,
        ref_path: &str,
    ) -> Result<(), ManageError> {
        writeln!(
            out,
            "\nFix multiple bundles for repo {} and ref {ref_path}",
            self.report_label()
        )?;

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
            writeln!(out, "Aborted")?;
            return Ok(());
        }

        writeln!(out, "Keeping {keeper_sha}")?;
        // Partition into (keep, evict). The snapshot is updated in place
        // so subsequent steps (HEAD validation in particular) see the
        // resolved layout.
        let bundles = std::mem::take(&mut ref_entry.bundles);
        let (keepers, losers): (Vec<_>, Vec<_>) =
            bundles.into_iter().partition(|b| b.sha == keeper_sha);
        ref_entry.bundles = keepers;

        for losing in &losers {
            self.evict_losing_bundle(out, ref_path, losing).await?;
        }
        Ok(())
    }

    async fn evict_losing_bundle<W: Write>(
        &self,
        out: &mut W,
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
            writeln!(out, "Removing {}", losing.sha)?;
        } else {
            // `Uuid::Simple`'s `Display` impl does NOT honor the
            // precision specifier (`{:.8}`), so encode into a stack
            // buffer and slice to 8 chars to produce the `<ref>_<uuid8>`
            // quarantine ref name.
            let mut buf = [0u8; uuid::fmt::Simple::LENGTH];
            let suffix = &Uuid::new_v4().simple().encode_lower(&mut buf)[..8];
            let new_ref = format!("{ref_path}_{suffix}");
            // `keys::bundle_key` accepts an `Option<&str>` prefix and
            // collapses `Some("")` to the no-prefix shape; passing
            // `Some(&self.prefix)` therefore handles both the prefixed
            // and root-of-bucket cases without an explicit branch.
            let dst_key = keys::bundle_key(Some(&self.prefix), &new_ref, &losing.sha);
            writeln!(out, "Moving {} to new branch {new_ref}", losing.sha)?;
            self.store.copy(&losing.key, &dst_key).await?;
        }
        self.store.delete(&losing.key).await?;
        Ok(())
    }

    async fn fix_head<W: Write>(
        &self,
        out: &mut W,
        snapshot: &mut RepoSnapshot,
    ) -> Result<(), ManageError> {
        writeln!(out, "\nFix invalid HEAD for repo {}", self.report_label())?;

        let candidates: Vec<&str> = snapshot
            .refs
            .keys()
            .filter(|k| k.starts_with("refs/heads/"))
            .map(String::as_str)
            .collect();
        if candidates.is_empty() {
            writeln!(
                out,
                "No `refs/heads/*` available to assign as HEAD; skipping."
            )?;
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

        // Re-verify the chosen branch is still present on the bucket
        // before writing HEAD. The candidate list above is taken from
        // the top-of-run snapshot, which can be minutes old after the
        // interactive prompt; a concurrent `git push :<branch>` or
        // `manage delete-branch` may have removed the chosen branch
        // since the snapshot was taken. Writing HEAD anyway would
        // re-create the invalid-HEAD condition the doctor was trying
        // to fix (issue #138).
        //
        // We re-list the branch prefix (rather than HEADing a single
        // object) because a packchain branch is healthy with
        // `chain.json` and `packs/*` but no `.bundle` — there is no
        // single object key that uniquely identifies "branch exists"
        // across both engines. `keys::join` with a trailing `/` on the
        // suffix produces a `<prefix>/<ref>/` listing prefix.
        //
        // The "branch exists" predicate uses [`super::has_branch_data`],
        // which excludes `*.lock` keys and the `PROTECTED#` marker.
        // Those keys are operational metadata — a stale lock file or a
        // surviving protection marker — that can outlive the user-data
        // keys (`chain.json`, `packs/*`, `*.bundle`) when a concurrent
        // delete runs partially. Writing HEAD against a branch whose
        // only residue is operational metadata would re-create the
        // invalid-HEAD condition #138 set out to prevent, so we treat
        // that residue as evidence the branch is gone. Sharing the
        // helper with `ManageBranch::protect` keeps the two
        // race-detection paths in lockstep.
        let branch_prefix = keys::join(Some(&self.prefix), &format!("{new_head}/"));
        let recheck = self.store.list(&branch_prefix).await?;
        if !super::has_branch_data(&recheck) {
            // `recheck` may still be non-empty here if it contains only
            // operational metadata (lock files, `PROTECTED#` markers);
            // surface that distinction to the operator so a residual
            // lock or marker doesn't read as "the branch is back".
            let residue_only = !recheck.is_empty();
            if residue_only {
                writeln!(
                    out,
                    "Selected branch {new_head} is considered gone — only operational \
                     metadata (lock files / PROTECTED# marker) remains under its prefix. \
                     Refusing to write stale HEAD. Re-run doctor."
                )?;
            } else {
                writeln!(
                    out,
                    "Selected branch {new_head} was deleted between selection and HEAD write; \
                     refusing to write stale HEAD. Re-run doctor."
                )?;
            }
            warn!(
                branch = %new_head,
                residue_only,
                "doctor fix_head: chosen branch disappeared between snapshot and HEAD write"
            );
            return Err(ManageError::StaleSnapshot(new_head));
        }

        let head_key = keys::join(Some(&self.prefix), "HEAD");
        writeln!(out, "Setting {new_head} as HEAD")?;
        self.store
            .put_bytes(&head_key, Bytes::from(new_head.clone()), PutOpts::default())
            .await?;
        snapshot.head = Some(new_head);
        Ok(())
    }

    async fn list_and_handle_stale_locks<W: Write>(
        &self,
        out: &mut W,
        objects: &[ObjectMeta],
    ) -> Result<(), ManageError> {
        writeln!(out, "\nScanning for stale locks...")?;
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
            writeln!(out, "No stale locks found.")?;
            return Ok(());
        }

        writeln!(out, "Found stale locks:")?;
        for (key, age) in &stale {
            writeln!(out, " - {key} (age: {}s)", age.as_secs())?;
        }

        if !self.opts.delete_stale_locks {
            writeln!(
                out,
                "\nRun with --delete-stale-locks to remove them automatically."
            )?;
            return Ok(());
        }

        writeln!(out, "\nDeleting stale locks...")?;
        let mut deleted = 0usize;
        let mut skipped = 0usize;
        for (key, _) in &stale {
            // Re-HEAD the lock immediately before deleting it. The
            // listing fed into this function originates from the
            // top-of-run `list` call and can be minutes old after the
            // interactive duplicate-bundle / HEAD-fix prompts. Without
            // this re-check the lock at `key` may have been cleaned up
            // and replaced by a fresh, active lock at the same key —
            // and an unconditional `delete` would silently revoke
            // another client's mutual exclusion (issue #132).
            //
            // The `ObjectStore` trait exposes no conditional delete
            // primitive (no `If-Unmodified-Since` / `If-Match`), so a
            // residual HEAD→delete window of a few milliseconds
            // remains. Adding conditional delete is a broader trait
            // change; the HEAD-then-delete approach shrinks the race
            // from minutes to milliseconds and is the trade-off the
            // issue's "suggested fix" accepts.
            let recheck = self.store.head(key).await;
            let recheck_now = OffsetDateTime::now_utc();
            match recheck {
                Ok(meta) => {
                    let still_stale = Duration::try_from(recheck_now - meta.last_modified)
                        .is_ok_and(|elapsed| elapsed > ttl);
                    if !still_stale {
                        writeln!(
                            out,
                            "Skipping {key}: lock no longer stale, refusing to delete"
                        )?;
                        warn!(key, "lock no longer stale, skipping doctor delete");
                        skipped += 1;
                        continue;
                    }
                }
                Err(ObjectStoreError::NotFound(_)) => {
                    writeln!(out, "Skipping {key}: lock disappeared concurrently")?;
                    warn!(key, "lock disappeared between listing and delete, skipping");
                    skipped += 1;
                    continue;
                }
                Err(e) => {
                    // Best-effort sweep: a transient HEAD failure
                    // must not abort the run or delete on a stale
                    // assumption.
                    writeln!(out, "Failed to re-check {key}: {e}")?;
                    warn!(key, error = %e, "head re-check failed; skipping delete");
                    skipped += 1;
                    continue;
                }
            }
            match self.store.delete(key).await {
                Ok(()) => {
                    writeln!(out, "Deleted {key}")?;
                    info!(key, "deleted stale lock");
                    deleted += 1;
                }
                Err(e) => {
                    // Best-effort: report each failure but keep going.
                    writeln!(out, "Failed to delete {key}: {e}")?;
                }
            }
        }
        if skipped > 0 {
            writeln!(
                out,
                "Skipped {skipped} lock(s) that became fresh or disappeared since listing; deleted {deleted}."
            )?;
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

/// Render the malformed-bundle-key section. Caller must check
/// `entries.is_empty()` first — this function always emits a header.
///
/// Doctor does not auto-delete: the safe action is the operator's, not
/// ours. Each row is the full key plus a `aws s3 rm` / `az storage blob
/// delete`-style hint so an operator can act without re-deriving the
/// path from the ref name.
fn render_malformed_bundles_section(entries: &[MalformedBundleKey]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\nMalformed bundle keys (push silently ignores these):"
    );
    for entry in entries {
        let _ = writeln!(out, "  - {} (ref {})", entry.key, entry.ref_path);
    }
    let _ = writeln!(
        out,
        "  Delete each key manually (`aws s3 rm` / `az storage blob delete`) and re-push the ref.",
    );
    out
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

    let candidates: Vec<&BranchRow> = report
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
    // f64 precision loss is acceptable here: the result is rendered
    // to one decimal place for human consumption only.
    #[allow(clippy::cast_precision_loss)]
    let scaled = |unit: u64| bytes as f64 / unit as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", scaled(GIB))
    } else if bytes >= MIB {
        format!("{:.1} MiB", scaled(MIB))
    } else if bytes >= KIB {
        format!("{:.1} KiB", scaled(KIB))
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
    use crate::manage::snapshot::analyze;
    use crate::manage::{ScriptedPrompter, scripted::Answer};
    use crate::object_store::mock::MockStore;
    use bytes::Bytes;
    use time::OffsetDateTime;

    fn store_arc(mock: &MockStore) -> Arc<dyn ObjectStore> {
        Arc::new(mock.clone())
    }

    // Valid 40-lower-hex stems used as bundle-key fixtures. Earlier
    // doctor tests used short stems like "abc"; #124 added stem
    // validation in the snapshot pass, so well-formed-bundle test
    // fixtures must now carry real-length stems.
    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SHA_C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    #[tokio::test]
    async fn no_issues_round_trip_runs_clean() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        let initial_keys = mock.keys();
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        doctor
            .run_into(&mut std::io::sink())
            .await
            .expect("doctor.run");
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
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("body-a"),
        );
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_B}.bundle"),
            Bytes::from("body-b"),
        );
        let prompter = ScriptedPrompter::new([Answer::Select(0), Answer::Confirm(true)]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        doctor
            .run_into(&mut std::io::sink())
            .await
            .expect("doctor.run");

        // Original bundle for the keeper is still present.
        assert!(mock.contains(&format!("myrepo/refs/heads/main/{SHA_A}.bundle")));
        // Loser was moved off the main ref.
        assert!(!mock.contains(&format!("myrepo/refs/heads/main/{SHA_B}.bundle")));
        // The new quarantine ref has a key with the moved bundle, and
        // the suffix is exactly 8 lowercase hex characters
        // (`<ref>_<uuid8>`).
        let loser_tail = format!("/{SHA_B}.bundle");
        let moved = mock
            .keys()
            .into_iter()
            .find(|k| k.starts_with("myrepo/refs/heads/main_") && k.ends_with(&loser_tail))
            .expect("quarantine key created");
        let suffix = moved
            .strip_prefix("myrepo/refs/heads/main_")
            .and_then(|rest| rest.strip_suffix(&loser_tail))
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
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("a"),
        );
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_B}.bundle"),
            Bytes::from("b"),
        );
        let prompter = ScriptedPrompter::new([Answer::Select(1), Answer::Confirm(true)]);
        let opts = DoctorOpts {
            delete_bundle: true,
            ..DoctorOpts::default()
        };
        let doctor = Doctor::new(store_arc(&mock), "myrepo", opts, &prompter);
        doctor
            .run_into(&mut std::io::sink())
            .await
            .expect("doctor.run");
        assert!(!mock.contains(&format!("myrepo/refs/heads/main/{SHA_A}.bundle")));
        assert!(mock.contains(&format!("myrepo/refs/heads/main/{SHA_B}.bundle")));
    }

    #[tokio::test]
    async fn fix_multiple_bundles_user_aborts_keeps_originals() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("a"),
        );
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_B}.bundle"),
            Bytes::from("b"),
        );
        let prompter = ScriptedPrompter::new([Answer::Select(0), Answer::Confirm(false)]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        // User-no on the confirmation declines this fix but is not an
        // abort of the whole run — the doctor continues to scan stale
        // locks and exits 0. Both bundles must remain untouched.
        doctor
            .run_into(&mut std::io::sink())
            .await
            .expect("user-no should not error");
        assert!(mock.contains(&format!("myrepo/refs/heads/main/{SHA_A}.bundle")));
        assert!(mock.contains(&format!("myrepo/refs/heads/main/{SHA_B}.bundle")));
    }

    #[tokio::test]
    async fn fix_multiple_bundles_out_of_range_select_returns_internal_error() {
        // A scripted prompter that returns an out-of-range index used to
        // panic the process via `expect`. The defensive path now returns
        // a structured `ManageError::Internal` so the helper / management
        // CLI can surface the bug without aborting (issue #33).
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("a"),
        );
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_B}.bundle"),
            Bytes::from("b"),
        );
        // Two bundles → valid indices are 0 and 1; 99 is out of range.
        let prompter = ScriptedPrompter::new([Answer::Select(99)]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let err = doctor
            .run_into(&mut std::io::sink())
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
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        mock.insert(
            format!("myrepo/refs/heads/dev/{SHA_C}.bundle"),
            Bytes::from("c"),
        );
        // Two HEAD candidates → valid indices are 0 and 1; 42 is out of
        // range. No prior bundle-fix prompts because no ref has > 1
        // bundle.
        let prompter = ScriptedPrompter::new([Answer::Select(42)]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let err = doctor
            .run_into(&mut std::io::sink())
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
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        mock.insert(
            format!("myrepo/refs/heads/dev/{SHA_C}.bundle"),
            Bytes::from("c"),
        );
        // Refs are surfaced in BTreeMap order (lexicographic), so the
        // candidate list is `[refs/heads/dev, refs/heads/main]` and
        // index 1 is `main`.
        let prompter = ScriptedPrompter::new([Answer::Select(1)]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        doctor
            .run_into(&mut std::io::sink())
            .await
            .expect("doctor.run");

        let head_bytes = mock.get_bytes("myrepo/HEAD").await.expect("HEAD written");
        assert_eq!(&head_bytes[..], b"refs/heads/main");
    }

    /// Prompter that runs a one-shot side effect immediately before
    /// returning its first `select` answer. Used by the issue #138
    /// regression tests to simulate a concurrent
    /// `git push :<branch>` / `manage delete-branch` that fires
    /// **between** the operator's selection and the HEAD write.
    struct DeleteBeforeReturnPrompter {
        index: usize,
        mock: MockStore,
        keys_to_delete: Vec<String>,
        /// Keys to insert (with an empty body) immediately before
        /// returning the answer — lets a race test seed residue
        /// (lock files, `PROTECTED#` markers) into the branch prefix
        /// the doctor is about to re-check.
        keys_to_insert: Vec<String>,
        fired: std::sync::Mutex<bool>,
    }

    impl Prompter for DeleteBeforeReturnPrompter {
        fn select(&self, _prompt: &str, _options: &[String]) -> Result<usize, ManageError> {
            let mut fired = self.fired.lock().expect("fired mutex poisoned");
            if !*fired {
                for key in &self.keys_to_delete {
                    let _ = self.mock.remove_key(key);
                }
                for key in &self.keys_to_insert {
                    self.mock.insert(key.clone(), Bytes::new());
                }
                *fired = true;
            }
            Ok(self.index)
        }

        fn confirm(&self, _prompt: &str) -> Result<bool, ManageError> {
            panic!("DeleteBeforeReturnPrompter does not expect confirm prompts")
        }
    }

    #[tokio::test]
    async fn fix_head_refuses_when_chosen_branch_deleted_between_select_and_write() {
        // Issue #138: `fix_head` presents candidates from the
        // top-of-run snapshot. If a concurrent push / delete-branch
        // removes the chosen branch between the operator's selection
        // and the HEAD write, the doctor must NOT write HEAD pointing
        // at a now-deleted branch. Simulate the race by deleting the
        // chosen branch's only object inside the prompter's `select`
        // implementation (which fires after the candidate list is
        // computed but before `put_bytes(HEAD)` runs).
        let mock = MockStore::new();
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        mock.insert(
            format!("myrepo/refs/heads/dev/{SHA_C}.bundle"),
            Bytes::from("c"),
        );
        // Index 1 == `refs/heads/main` (lexicographic ordering).
        let prompter = DeleteBeforeReturnPrompter {
            index: 1,
            mock: mock.clone(),
            keys_to_delete: vec![format!("myrepo/refs/heads/main/{SHA_A}.bundle")],
            keys_to_insert: vec![],
            fired: std::sync::Mutex::new(false),
        };
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let mut out = Vec::new();
        let err = doctor
            .run_into(&mut out)
            .await
            .expect_err("stale snapshot must surface as an error, not silent success");
        assert!(
            matches!(err, ManageError::StaleSnapshot(ref b) if b == "refs/heads/main"),
            "expected ManageError::StaleSnapshot(refs/heads/main), got {err:?}",
        );

        // HEAD must NOT have been written — the doctor refused to
        // create a HEAD pointing at the deleted branch.
        assert!(
            !mock.contains("myrepo/HEAD"),
            "HEAD was written despite chosen branch being deleted",
        );

        // The operator-facing output names the deleted branch and
        // tells them to re-run.
        let output = String::from_utf8(out).expect("doctor output is utf-8");
        assert!(
            output.contains("refs/heads/main")
                && output.contains("deleted between selection and HEAD write")
                && output.contains("Re-run doctor"),
            "expected race-detection message, got:\n{output}",
        );
        // The "Setting … as HEAD" line MUST NOT appear — we aborted
        // before that write.
        assert!(
            !output.contains("Setting refs/heads/main as HEAD"),
            "doctor printed the HEAD-write confirmation despite aborting:\n{output}",
        );
    }

    #[tokio::test]
    async fn fix_head_succeeds_when_unrelated_branch_deleted_during_prompt() {
        // Companion to the race-detection test above: if a concurrent
        // delete removes a branch the operator did NOT select, the
        // re-verification must still let the HEAD write proceed. This
        // pins the re-check to the chosen branch only — it does not
        // re-list everything and abort on any drift.
        let mock = MockStore::new();
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        mock.insert(
            format!("myrepo/refs/heads/dev/{SHA_C}.bundle"),
            Bytes::from("c"),
        );
        // Index 1 == `refs/heads/main`. Delete `refs/heads/dev` (the
        // non-chosen branch) during the prompt to prove the chosen
        // branch's re-verification is the only thing that matters.
        let prompter = DeleteBeforeReturnPrompter {
            index: 1,
            mock: mock.clone(),
            keys_to_delete: vec![format!("myrepo/refs/heads/dev/{SHA_C}.bundle")],
            keys_to_insert: vec![],
            fired: std::sync::Mutex::new(false),
        };
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        doctor
            .run_into(&mut std::io::sink())
            .await
            .expect("unrelated concurrent delete must not block HEAD write");

        let head_bytes = mock.get_bytes("myrepo/HEAD").await.expect("HEAD written");
        assert_eq!(&head_bytes[..], b"refs/heads/main");
    }

    #[tokio::test]
    async fn fix_head_refuses_when_chosen_branch_left_with_only_lock_or_marker() {
        // Tightens the #138 race-detection check: an empty re-listing is
        // not the only "branch is gone" signal. A concurrent partial
        // delete can leave a stale `LOCK#.lock` and / or a `PROTECTED#`
        // marker behind after every user-data key (bundles, `chain.json`,
        // `packs/*`) has been swept. Those keys are operational
        // metadata, not branch contents — writing HEAD against them would
        // recreate exactly the invalid-HEAD condition the doctor exists
        // to prevent.
        //
        // We drive the race by seeding the branch with one bundle and
        // letting the prompter both delete the bundle AND insert the
        // residue (lock + marker) between candidate selection and the
        // HEAD-write re-check.
        let mock = MockStore::new();
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        mock.insert(
            format!("myrepo/refs/heads/dev/{SHA_C}.bundle"),
            Bytes::from("c"),
        );
        // Index 1 == `refs/heads/main` (lexicographic ordering).
        let prompter = DeleteBeforeReturnPrompter {
            index: 1,
            mock: mock.clone(),
            keys_to_delete: vec![format!("myrepo/refs/heads/main/{SHA_A}.bundle")],
            keys_to_insert: vec![
                "myrepo/refs/heads/main/LOCK#.lock".to_owned(),
                format!("myrepo/refs/heads/main/{}", keys::PROTECTED_MARKER_SEGMENT),
            ],
            fired: std::sync::Mutex::new(false),
        };
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let mut out = Vec::new();
        let err = doctor
            .run_into(&mut out)
            .await
            .expect_err("residue-only branch must surface as stale snapshot");
        assert!(
            matches!(err, ManageError::StaleSnapshot(ref b) if b == "refs/heads/main"),
            "expected ManageError::StaleSnapshot(refs/heads/main), got {err:?}",
        );

        // HEAD must NOT have been written — the doctor refused even
        // though the lock and marker keys make the listing non-empty.
        assert!(
            !mock.contains("myrepo/HEAD"),
            "HEAD was written despite chosen branch having only operational metadata",
        );

        // The operator-facing output names the branch and explains why
        // a non-empty listing was still treated as "gone".
        let output = String::from_utf8(out).expect("doctor output is utf-8");
        assert!(
            output.contains("refs/heads/main"),
            "expected branch name in output:\n{output}",
        );
        assert!(
            output.contains("considered gone"),
            "expected 'considered gone' framing in output:\n{output}",
        );
        assert!(
            output.contains("operational metadata"),
            "expected residue rationale in output:\n{output}",
        );
        assert!(
            !output.contains("Setting refs/heads/main as HEAD"),
            "doctor printed the HEAD-write confirmation despite aborting:\n{output}",
        );
    }

    #[tokio::test]
    async fn stale_lock_listed_but_not_deleted_by_default() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        let stale = OffsetDateTime::now_utc() - time::Duration::seconds(120);
        mock.insert_with(
            "myrepo/refs/heads/main/LOCK#.lock",
            Bytes::new(),
            stale,
            PutOpts::default(),
        );
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        doctor
            .run_into(&mut std::io::sink())
            .await
            .expect("doctor.run");
        assert!(
            mock.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "lock retained without --delete-stale-locks"
        );
    }

    #[tokio::test]
    async fn stale_lock_deleted_when_flag_set() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
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
        doctor
            .run_into(&mut std::io::sink())
            .await
            .expect("doctor.run");
        assert!(!mock.contains("myrepo/refs/heads/main/LOCK#.lock"));
    }

    #[tokio::test]
    async fn fresh_lock_is_not_flagged_stale() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        // Stamped now → not stale.
        mock.insert("myrepo/refs/heads/main/LOCK#.lock", Bytes::new());
        let opts = DoctorOpts {
            delete_stale_locks: true,
            ..DoctorOpts::default()
        };
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", opts, &prompter);
        doctor
            .run_into(&mut std::io::sink())
            .await
            .expect("doctor.run");
        assert!(mock.contains("myrepo/refs/heads/main/LOCK#.lock"));
    }

    // --- Stale-listing race window (#132) ---------------------------------
    //
    // The initial bucket listing is reused for stale-lock detection after
    // interactive duplicate-bundle / HEAD-fix prompts have run, which can
    // take minutes. In that window the stale lock may have been cleaned up
    // and replaced by a fresh, active lock at the same key — and an
    // unconditional delete would silently revoke the new client's mutual
    // exclusion. The fix re-HEADs each lock key immediately before the
    // delete and skips keys that are no longer stale (or vanished).

    /// Lock key was stale at listing time but a fresh lock now sits at the
    /// same key. The re-HEAD must catch it and refuse to delete.
    #[tokio::test]
    async fn stale_listing_with_fresh_head_skips_delete() {
        let mock = MockStore::new();
        // Store the lock with a FRESH timestamp — this is what HEAD will
        // see at delete time, simulating the lock having been refreshed
        // by another client between listing and stale-handling.
        mock.insert("myrepo/refs/heads/main/LOCK#.lock", Bytes::new());

        // Synthesize the stale listing the way `run_into` would see it if
        // the lock had been stale when the initial `list` ran. Calling
        // `list_and_handle_stale_locks` directly lets the test inject the
        // race condition deterministically.
        let stale_ts = OffsetDateTime::now_utc() - time::Duration::seconds(120);
        let synthetic_listing = vec![ObjectMeta {
            key: "myrepo/refs/heads/main/LOCK#.lock".to_owned(),
            size: 0,
            last_modified: stale_ts,
            etag: None,
        }];

        let opts = DoctorOpts {
            delete_stale_locks: true,
            ..DoctorOpts::default()
        };
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", opts, &prompter);
        let mut out = Vec::new();
        doctor
            .list_and_handle_stale_locks(&mut out, &synthetic_listing)
            .await
            .expect("stale-lock handler");
        let captured = String::from_utf8(out).expect("utf-8 output");

        assert!(
            mock.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "fresh lock at stale-listed key must not be deleted",
        );
        assert!(
            captured.contains("no longer stale"),
            "skip reason missing from operator output: {captured:?}",
        );
        assert!(
            captured.contains("Skipped 1 lock(s)"),
            "skipped-count summary missing: {captured:?}",
        );
    }

    /// Lock key was stale at listing time AND still stale at HEAD time —
    /// the re-HEAD must NOT suppress the legitimate delete.
    #[tokio::test]
    async fn stale_listing_with_stale_head_deletes() {
        let mock = MockStore::new();
        let stale_ts = OffsetDateTime::now_utc() - time::Duration::seconds(120);
        mock.insert_with(
            "myrepo/refs/heads/main/LOCK#.lock",
            Bytes::new(),
            stale_ts,
            PutOpts::default(),
        );
        let synthetic_listing = vec![ObjectMeta {
            key: "myrepo/refs/heads/main/LOCK#.lock".to_owned(),
            size: 0,
            last_modified: stale_ts,
            etag: None,
        }];

        let opts = DoctorOpts {
            delete_stale_locks: true,
            ..DoctorOpts::default()
        };
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", opts, &prompter);
        let mut out = Vec::new();
        doctor
            .list_and_handle_stale_locks(&mut out, &synthetic_listing)
            .await
            .expect("stale-lock handler");
        let captured = String::from_utf8(out).expect("utf-8 output");

        assert!(
            !mock.contains("myrepo/refs/heads/main/LOCK#.lock"),
            "stale lock must be deleted when HEAD confirms staleness",
        );
        assert!(
            captured.contains("Deleted myrepo/refs/heads/main/LOCK#.lock"),
            "delete confirmation missing: {captured:?}",
        );
    }

    /// Lock vanished between listing and stale-handling (e.g. another
    /// client's stale-lock recovery already cleaned it up). HEAD returns
    /// `NotFound` — the doctor must skip cleanly without erroring.
    #[tokio::test]
    async fn stale_listing_with_vanished_head_skips_without_error() {
        let mock = MockStore::new();
        // No lock object inserted — HEAD will return NotFound.
        let stale_ts = OffsetDateTime::now_utc() - time::Duration::seconds(120);
        let synthetic_listing = vec![ObjectMeta {
            key: "myrepo/refs/heads/main/LOCK#.lock".to_owned(),
            size: 0,
            last_modified: stale_ts,
            etag: None,
        }];

        let opts = DoctorOpts {
            delete_stale_locks: true,
            ..DoctorOpts::default()
        };
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", opts, &prompter);
        let mut out = Vec::new();
        doctor
            .list_and_handle_stale_locks(&mut out, &synthetic_listing)
            .await
            .expect("vanished lock must not error");
        let captured = String::from_utf8(out).expect("utf-8 output");

        assert!(
            captured.contains("disappeared concurrently"),
            "concurrent-cleanup skip reason missing: {captured:?}",
        );
        assert!(
            !captured.contains("Deleted myrepo/refs/heads/main/LOCK#.lock"),
            "must not log a delete for a vanished key: {captured:?}",
        );
    }

    #[tokio::test]
    async fn report_renders_protected_multi_bundle_and_invalid_head() {
        // Build a snapshot covering every report-line shape: a
        // protected ref with one bundle, a duplicate-bundle ref, an
        // empty ref, plus a HEAD body that does not match any ref so
        // the trailing label reads `Invalid`.
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/missing"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        mock.insert("myrepo/refs/heads/main/PROTECTED#", Bytes::new());
        mock.insert(
            format!("myrepo/refs/heads/dev/{SHA_A}.bundle"),
            Bytes::from("a"),
        );
        mock.insert(
            format!("myrepo/refs/heads/dev/{SHA_B}.bundle"),
            Bytes::from("a"),
        );
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
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
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
        mock.insert(
            format!("refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("body"),
        );
        let initial_keys = mock.keys();
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "", DoctorOpts::default(), &prompter);
        doctor
            .run_into(&mut std::io::sink())
            .await
            .expect("doctor.run at root");
        assert_eq!(mock.keys(), initial_keys);
    }

    #[tokio::test]
    async fn root_prefix_fix_head_writes_to_root_head_key() {
        // No HEAD object → fix_head writes one. The key must be the
        // bare `HEAD`, not `/HEAD`.
        let mock = MockStore::new();
        mock.insert(format!("refs/heads/main/{SHA_A}.bundle"), Bytes::from("b"));
        let prompter = ScriptedPrompter::new([Answer::Select(0)]);
        let doctor = Doctor::new(store_arc(&mock), "", DoctorOpts::default(), &prompter);
        doctor
            .run_into(&mut std::io::sink())
            .await
            .expect("doctor.run at root");

        let head_bytes = mock.get_bytes("HEAD").await.expect("HEAD at root");
        assert_eq!(&head_bytes[..], b"refs/heads/main");
        assert!(!mock.contains("/HEAD"), "no leading-slash HEAD key");
    }

    #[tokio::test]
    async fn root_prefix_fix_multiple_bundles_quarantines_at_root() {
        let mock = MockStore::new();
        mock.insert("HEAD", Bytes::from("refs/heads/main"));
        mock.insert(format!("refs/heads/main/{SHA_A}.bundle"), Bytes::from("a"));
        mock.insert(format!("refs/heads/main/{SHA_B}.bundle"), Bytes::from("b"));
        let prompter = ScriptedPrompter::new([Answer::Select(0), Answer::Confirm(true)]);
        let doctor = Doctor::new(store_arc(&mock), "", DoctorOpts::default(), &prompter);
        doctor
            .run_into(&mut std::io::sink())
            .await
            .expect("doctor.run at root");

        // Loser was moved to a quarantine ref `refs/heads/main_<uuid8>`,
        // and the destination key has no leading slash.
        let moved = mock
            .keys()
            .into_iter()
            .find(|k| k.starts_with("refs/heads/main_") && k.ends_with(&format!("/{SHA_B}.bundle")))
            .expect("quarantine key created at root");
        assert!(
            !moved.starts_with('/'),
            "quarantine key must not have a leading slash: {moved:?}"
        );
    }

    // --- Packchain section --------------------------------------------

    #[tokio::test]
    async fn bundle_engine_clean_run_does_not_mutate_bucket() {
        // Doctor against a bundle-engine remote does not run the
        // packchain audit (the engine gate at run() guards it). This
        // test pins that the run path is non-mutating; the *absence*
        // of the packchain section under bundle is verified
        // separately via `bundle_engine_audit_runs_against_packchain_only`
        // (mutation-tested: inverting the engine gate makes that
        // test fail).
        let mock = MockStore::new();
        mock.insert("repo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            format!("repo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        let initial_keys = mock.keys();
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "repo", DoctorOpts::default(), &prompter);
        doctor
            .run_into(&mut std::io::sink())
            .await
            .expect("clean bundle run");
        assert_eq!(mock.keys(), initial_keys);
    }

    /// Build a small packchain-shape mock used by the engine-gate
    /// tests below: chain.json + live pack + orphan pack + tombstone.
    /// Returns the bucket-wide listing the doctor would compute, so
    /// tests can drive `maybe_render_packchain_section` with the
    /// same `objects` slice the production code receives.
    async fn packchain_mock_with_listing() -> (MockStore, Vec<ObjectMeta>) {
        let mock = MockStore::new();
        mock.insert(
            "repo/refs/heads/main/chain.json",
            Bytes::from(
                r#"{"v":1,"tip":"0000000000000000000000000000000000000001","full_at":"0000000000000000000000000000000000000001","segments":[{"sha":"0000000000000000000000000000000000000001","parent_sha":null,"pack":"packs/1111111111111111111111111111111111111111.pack","bytes":1024}]}"#,
            ),
        );
        mock.insert(
            "repo/packs/1111111111111111111111111111111111111111.pack",
            Bytes::from_static(b"live"),
        );
        mock.insert(
            "repo/packs/2222222222222222222222222222222222222222.pack",
            Bytes::from_static(b"orphan-body-len-eq-19"),
        );
        let marked_at = (OffsetDateTime::now_utc() - time::Duration::hours(2))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let tombstone_body = format!(
            r#"{{"v":1,"run_id":"abc-1","marked_at":"{marked_at}","orphan_packs":["2222222222222222222222222222222222222222"]}}"#
        );
        let tombstone_key = format!("repo/gc/tombstones-abc-1-{marked_at}.json");
        mock.insert(tombstone_key, Bytes::from(tombstone_body));

        let objects = mock.list("repo/").await.expect("list");
        (mock, objects)
    }

    #[tokio::test]
    async fn packchain_engine_renders_section_with_orphan_and_tombstone() {
        // Mutation-tested: stubbing `render_packchain_section` to
        // return an empty string makes this test fail.
        let (mock, objects) = packchain_mock_with_listing().await;
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(
            store_arc(&mock),
            "repo",
            DoctorOpts {
                engine: StorageEngine::Packchain,
                ..DoctorOpts::default()
            },
            &prompter,
        );
        let rendered = doctor
            .maybe_render_packchain_section(&objects)
            .await
            .expect("packchain audit succeeds")
            .expect("packchain engine produces a section");

        // Pin actual content. A regression in either audit
        // classification OR renderer fails one of these.
        assert!(rendered.contains("=== Packchain ==="), "{rendered}");
        assert!(rendered.contains("Orphans: 1 pack(s)"), "{rendered}");
        assert!(rendered.contains("21 B"), "{rendered}");
        assert!(rendered.contains("run id abc-1"), "{rendered}");
        assert!(rendered.contains("1 pack(s)"), "{rendered}");
    }

    #[tokio::test]
    async fn bundle_engine_returns_no_packchain_section() {
        // Mutation-tested: inverting `maybe_render_packchain_section`'s
        // engine gate (`Packchain` → `Bundle`) makes this test fail.
        // The same packchain-shape bucket that fills `packchain_engine_renders_section_with_orphan_and_tombstone`
        // produces None here because the engine is Bundle.
        let (mock, objects) = packchain_mock_with_listing().await;
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "repo", DoctorOpts::default(), &prompter);
        let rendered = doctor
            .maybe_render_packchain_section(&objects)
            .await
            .expect("audit gate skips cleanly under bundle");
        assert!(
            rendered.is_none(),
            "bundle engine must not render a packchain section, got: {rendered:?}",
        );
    }

    #[test]
    fn render_packchain_section_lists_dangling_references_as_errors() {
        // Hand-roll an `AuditReport` so the renderer's behaviour is
        // pinned without going through the live store.
        let report = AuditReport {
            orphans: super::audit::OrphanSummary::default(),
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
            orphans: super::audit::OrphanSummary::default(),
            tombstones: Vec::new(),
            branches: vec![BranchRow {
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

    // --- Packchain report correctness (#75) ------------------------------

    #[tokio::test]
    async fn packchain_report_shows_no_gc_or_packs_lines() {
        // Against a packchain-shape bucket, packs/ and gc/ must NOT appear
        // in the bundle-shape report section. This pins the cleaned output
        // shape specified in #75.
        let mock = MockStore::new();
        mock.insert("repo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("repo/refs/heads/main/chain.json", Bytes::from(r#"{"v":1}"#));
        mock.insert(
            "repo/packs/1111111111111111111111111111111111111111.pack",
            Bytes::from_static(b"live"),
        );
        mock.insert(
            "repo/gc/tombstones-abc-1-2025-01-01T00:00:00Z.json",
            Bytes::from_static(b"{}"),
        );
        mock.insert("repo/lfs/abcdef0123456789", Bytes::from("lfs-body"));
        let prompter = ScriptedPrompter::new([]);
        let store = store_arc(&mock);
        let doctor = Doctor::new(Arc::clone(&store), "repo", DoctorOpts::default(), &prompter);
        let snapshot = analyze(&*store, "repo").await.expect("analyze");

        let report = doctor.report(&snapshot);
        // No "packs", "gc", or "lfs" ref lines.
        assert!(
            !report.contains(" packs:"),
            "packs must not appear in report: {report:?}",
        );
        assert!(
            !report.contains(" gc:"),
            "gc must not appear in report: {report:?}",
        );
        assert!(
            !report.contains(" lfs:"),
            "lfs must not appear in report: {report:?}",
        );
        // chain.json-bearing ref shows "Ok", not "No bundles".
        assert!(
            report.contains("refs/heads/main: Ok"),
            "chain.json ref must show Ok: {report:?}",
        );
        assert!(
            !report.contains("No bundles"),
            "healthy packchain report must have no 'No bundles' lines: {report:?}",
        );
    }

    #[tokio::test]
    async fn packchain_chain_json_ref_reports_ok_not_no_bundles() {
        // A ref with only chain.json (no .bundle) must report "Ok" in
        // the bundle-shape section, because the pack data lives in packs/.
        // This is the per-ref case from #75.
        let mock = MockStore::new();
        mock.insert("repo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("repo/refs/heads/main/chain.json", Bytes::from(r#"{"v":1}"#));
        let prompter = ScriptedPrompter::new([]);
        let store = store_arc(&mock);
        let doctor = Doctor::new(Arc::clone(&store), "repo", DoctorOpts::default(), &prompter);
        let snapshot = analyze(&*store, "repo").await.expect("analyze");

        let report = doctor.report(&snapshot);
        assert_eq!(
            report, "repo:\n  refs/heads/main: Ok\n  HEAD: refs/heads/main\n",
            "chain.json ref must report Ok, got: {report:?}",
        );
    }

    #[tokio::test]
    async fn bundle_engine_report_unchanged_by_chain_json_fix() {
        // A bundle-engine repo with no chain.json files must produce the
        // same output as before the #75 fix: "No bundles" for empty refs,
        // "Ok" for single-bundle refs, "Multiple bundles" for dup refs.
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/missing"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        mock.insert("myrepo/refs/heads/empty/PROTECTED#", Bytes::new());
        let prompter = ScriptedPrompter::new([]);
        let store = store_arc(&mock);
        let doctor = Doctor::new(
            Arc::clone(&store),
            "myrepo",
            DoctorOpts::default(),
            &prompter,
        );
        let snapshot = analyze(&*store, "myrepo").await.expect("analyze");

        let report = doctor.report(&snapshot);
        // "refs/heads/empty" has no bundles and no chain.json → "No bundles".
        assert!(
            report.contains("refs/heads/empty: No bundles"),
            "empty ref without chain.json must still show No bundles: {report:?}",
        );
        // "refs/heads/main" has one bundle → "Ok".
        assert!(
            report.contains("refs/heads/main: Ok"),
            "single-bundle ref must show Ok: {report:?}",
        );
    }

    #[tokio::test]
    async fn packchain_multiple_bundles_still_reports_multiple() {
        // A packchain ref with chain.json AND multiple .bundle files must
        // still report "Multiple bundles" — the has_chain guard only fires
        // when bundles.len() == 0. Prevents a future mistaken edit from
        // suppressing the duplicate-bundle warning for packchain refs.
        let mock = MockStore::new();
        mock.insert("repo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("repo/refs/heads/main/chain.json", Bytes::from(r#"{"v":1}"#));
        mock.insert(
            format!("repo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("a"),
        );
        mock.insert(
            format!("repo/refs/heads/main/{SHA_B}.bundle"),
            Bytes::from("b"),
        );
        let prompter = ScriptedPrompter::new([]);
        let store = store_arc(&mock);
        let doctor = Doctor::new(Arc::clone(&store), "repo", DoctorOpts::default(), &prompter);
        let snapshot = analyze(&*store, "repo").await.expect("analyze");

        let report = doctor.report(&snapshot);
        assert!(
            report.contains("refs/heads/main: Multiple bundles"),
            "packchain ref with multiple bundles must still warn: {report:?}",
        );
    }

    #[tokio::test]
    async fn packchain_chain_json_with_one_bundle_reports_ok() {
        // A packchain ref with chain.json AND exactly one .bundle hits
        // the `1 => "Ok"` arm (not the `0 if has_chain => "Ok"` arm).
        // Pin the rendered status so a future refactor of the match
        // doesn't accidentally regress this case.
        let mock = MockStore::new();
        mock.insert("repo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("repo/refs/heads/main/chain.json", Bytes::from(r#"{"v":1}"#));
        mock.insert(
            format!("repo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        let prompter = ScriptedPrompter::new([]);
        let store = store_arc(&mock);
        let doctor = Doctor::new(Arc::clone(&store), "repo", DoctorOpts::default(), &prompter);
        let snapshot = analyze(&*store, "repo").await.expect("analyze");

        let report = doctor.report(&snapshot);
        assert_eq!(
            report, "repo:\n  refs/heads/main: Ok\n  HEAD: refs/heads/main\n",
            "chain.json + 1 bundle must report Ok, got: {report:?}",
        );
    }

    #[tokio::test]
    async fn packchain_protected_ref_shows_star_and_ok() {
        // A packchain ref that is both protected and chain.json-bearing
        // must render the star prefix and "Ok" status together.
        let mock = MockStore::new();
        mock.insert("repo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("repo/refs/heads/main/chain.json", Bytes::from(r#"{"v":1}"#));
        mock.insert("repo/refs/heads/main/PROTECTED#", Bytes::new());
        let prompter = ScriptedPrompter::new([]);
        let store = store_arc(&mock);
        let doctor = Doctor::new(Arc::clone(&store), "repo", DoctorOpts::default(), &prompter);
        let snapshot = analyze(&*store, "repo").await.expect("analyze");

        let report = doctor.report(&snapshot);
        assert!(
            report.contains("* refs/heads/main: Ok"),
            "protected packchain ref must show star + Ok: {report:?}",
        );
    }

    #[tokio::test]
    async fn root_prefix_report_renders_root_label() {
        // The first line of the report uses `(root)` so the empty
        // prefix doesn't produce a bare `:` header.
        let mock = MockStore::new();
        mock.insert("HEAD", Bytes::from("refs/heads/main"));
        mock.insert(format!("refs/heads/main/{SHA_A}.bundle"), Bytes::from("b"));
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

    // --- run_into output-capture tests -----------------------------------

    /// Helper: run `doctor.run_into` into a `Vec<u8>` and return the
    /// captured output as a `String`.
    async fn capture_run(doctor: &Doctor<'_>) -> (Result<(), ManageError>, String) {
        let mut buf = Vec::new();
        let result = doctor.run_into(&mut buf).await;
        let output = String::from_utf8(buf).expect("doctor output is valid UTF-8");
        (result, output)
    }

    #[tokio::test]
    async fn run_into_bundle_engine_section_order() {
        // A clean bundle-engine run: snapshot report followed by the
        // stale-lock scan. The packchain section must NOT appear.
        // Expected output derives from `report()` (already pinned) plus
        // the stale-lock trailer.
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let (result, output) = capture_run(&doctor).await;
        result.expect("clean bundle run");

        // Section 1: snapshot report
        assert!(
            output.starts_with("myrepo:\n"),
            "output must start with snapshot report header, got: {output:?}",
        );
        // Section 2: stale-lock scan (no packchain section in between)
        assert!(
            output.contains("\nScanning for stale locks...\n"),
            "stale-lock scan missing from output: {output:?}",
        );
        assert!(
            output.contains("No stale locks found.\n"),
            "no-stale-locks trailer missing: {output:?}",
        );
        // Packchain section must NOT appear under bundle engine.
        assert!(
            !output.contains("=== Packchain ==="),
            "packchain section must not appear under bundle engine: {output:?}",
        );

        // Assert ordering: snapshot header appears before stale-lock scan.
        let report_pos = output.find("myrepo:").expect("report header");
        let locks_pos = output
            .find("Scanning for stale locks")
            .expect("stale-lock scan");
        assert!(
            report_pos < locks_pos,
            "snapshot report must precede stale-lock scan"
        );
    }

    #[tokio::test]
    async fn run_into_packchain_engine_section_order() {
        // A packchain-engine run: snapshot report, then packchain
        // section, then stale-lock scan. This test pins the three-part
        // section ordering that is only observable via captured output.
        let mock = MockStore::new();
        mock.insert("repo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            format!("repo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        // A minimal packchain shape: chain.json + live pack.
        mock.insert(
            "repo/refs/heads/main/chain.json",
            Bytes::from(
                r#"{"v":1,"tip":"0000000000000000000000000000000000000001","full_at":"0000000000000000000000000000000000000001","segments":[{"sha":"0000000000000000000000000000000000000001","parent_sha":null,"pack":"packs/1111111111111111111111111111111111111111.pack","bytes":1024}]}"#,
            ),
        );
        mock.insert(
            "repo/packs/1111111111111111111111111111111111111111.pack",
            Bytes::from_static(b"live"),
        );
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(
            store_arc(&mock),
            "repo",
            DoctorOpts {
                engine: StorageEngine::Packchain,
                ..DoctorOpts::default()
            },
            &prompter,
        );
        let (result, output) = capture_run(&doctor).await;
        result.expect("clean packchain run");

        // All three sections must appear, in order.
        let report_pos = output.find("repo:").expect("snapshot report header");
        let packchain_pos = output.find("=== Packchain ===").expect("packchain section");
        let locks_pos = output
            .find("Scanning for stale locks")
            .expect("stale-lock scan");
        assert!(
            report_pos < packchain_pos,
            "snapshot report must precede packchain section"
        );
        assert!(
            packchain_pos < locks_pos,
            "packchain section must precede stale-lock scan"
        );
    }

    #[tokio::test]
    async fn run_into_captures_fix_multiple_bundles_output() {
        // Exercises the duplicate-bundle fixer path through `run_into`
        // and pins the interactive-prompt output in the capture buffer.
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("body-a"),
        );
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_B}.bundle"),
            Bytes::from("body-b"),
        );
        let prompter = ScriptedPrompter::new([Answer::Select(0), Answer::Confirm(true)]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let (result, output) = capture_run(&doctor).await;
        result.expect("fix-multiple run");

        assert!(
            output.contains("Fix multiple bundles for repo myrepo and ref refs/heads/main"),
            "fixer header missing: {output:?}",
        );
        assert!(
            output.contains(&format!("Keeping {SHA_A}")),
            "keeper announcement missing: {output:?}",
        );
        assert!(
            output.contains(&format!("Moving {SHA_B} to new branch")),
            "eviction line missing: {output:?}",
        );
    }

    #[tokio::test]
    async fn run_into_captures_fix_head_output() {
        // Exercises the HEAD-fixer path and pins its output lines.
        let mock = MockStore::new();
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        let prompter = ScriptedPrompter::new([Answer::Select(0)]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let (result, output) = capture_run(&doctor).await;
        result.expect("fix-head run");

        assert!(
            output.contains("Fix invalid HEAD for repo myrepo"),
            "HEAD fixer header missing: {output:?}",
        );
        assert!(
            output.contains("Setting refs/heads/main as HEAD"),
            "HEAD assignment line missing: {output:?}",
        );
    }

    #[tokio::test]
    async fn run_into_captures_stale_lock_output() {
        // Exercises the stale-lock listing + deletion path and pins
        // the report lines in the capture buffer.
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
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
        let (result, output) = capture_run(&doctor).await;
        result.expect("stale-lock-delete run");

        assert!(
            output.contains("Found stale locks:"),
            "stale-lock listing header missing: {output:?}",
        );
        assert!(
            output.contains("Deleting stale locks..."),
            "deletion progress line missing: {output:?}",
        );
        assert!(
            output.contains("Deleted myrepo/refs/heads/main/LOCK#.lock"),
            "individual deletion confirmation missing: {output:?}",
        );
    }

    // --- Malformed bundle keys (#124) ------------------------------------

    #[tokio::test]
    async fn malformed_bundle_key_surfaces_in_doctor_output() {
        // Push silently filters keys whose stem fails the 40-hex
        // check (#109 + #124). The doctor must surface them so an
        // operator can clean them up.
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            "myrepo/refs/heads/main/0123456789abcdef0123456789abcdef01234567.bundle",
            Bytes::from("body"),
        );
        mock.insert(
            "myrepo/refs/heads/main/not-a-valid-sha.bundle",
            Bytes::from("junk"),
        );
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let (result, output) = capture_run(&doctor).await;
        result.expect("doctor.run");

        assert!(
            output.contains("Malformed bundle keys"),
            "section header missing: {output:?}",
        );
        assert!(
            output.contains("myrepo/refs/heads/main/not-a-valid-sha.bundle"),
            "malformed key not listed: {output:?}",
        );
        assert!(
            output.contains("(ref refs/heads/main)"),
            "ref-path context missing: {output:?}",
        );
        assert!(
            output.contains("Delete each key manually"),
            "remediation hint missing: {output:?}",
        );

        // Doctor must NOT delete the malformed key — operator's call.
        assert!(
            mock.contains("myrepo/refs/heads/main/not-a-valid-sha.bundle"),
            "doctor must not auto-delete malformed bundle keys",
        );
        // The valid sibling is untouched as well.
        assert!(mock.contains(
            "myrepo/refs/heads/main/0123456789abcdef0123456789abcdef01234567.bundle",
        ));
    }

    #[tokio::test]
    async fn clean_bucket_emits_no_malformed_section() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            "myrepo/refs/heads/main/0123456789abcdef0123456789abcdef01234567.bundle",
            Bytes::from("body"),
        );
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let (result, output) = capture_run(&doctor).await;
        result.expect("clean run");
        assert!(
            !output.contains("Malformed bundle keys"),
            "no-malformed runs must not emit the section: {output:?}",
        );
    }

    #[tokio::test]
    async fn well_formed_stem_is_not_flagged_as_malformed() {
        // Sanity test mirroring the snapshot-level check: a valid
        // 40-hex stem must not end up in the doctor's malformed
        // section.
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            "myrepo/refs/heads/main/0123456789abcdef0123456789abcdef01234567.bundle",
            Bytes::from("body"),
        );
        let prompter = ScriptedPrompter::new([]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let (result, output) = capture_run(&doctor).await;
        result.expect("clean run");
        assert!(!output.contains("Malformed bundle keys"));
        assert!(!output.contains("0123456789abcdef0123456789abcdef01234567.bundle (ref"));
    }

    #[test]
    fn render_malformed_bundles_section_pins_format() {
        let entries = vec![
            MalformedBundleKey {
                ref_path: "refs/heads/main".to_owned(),
                key: "myrepo/refs/heads/main/not-a-valid-sha.bundle".to_owned(),
            },
            MalformedBundleKey {
                ref_path: "refs/heads/dev".to_owned(),
                key: "myrepo/refs/heads/dev/short.bundle".to_owned(),
            },
        ];
        let rendered = super::render_malformed_bundles_section(&entries);
        assert_eq!(
            rendered,
            "\nMalformed bundle keys (push silently ignores these):\n  \
             - myrepo/refs/heads/main/not-a-valid-sha.bundle (ref refs/heads/main)\n  \
             - myrepo/refs/heads/dev/short.bundle (ref refs/heads/dev)\n  \
             Delete each key manually (`aws s3 rm` / `az storage blob delete`) and re-push the ref.\n",
        );
    }

    #[tokio::test]
    async fn run_into_captures_aborted_output() {
        // Exercises the user-decline path in fix_multiple_bundles and
        // pins the "Aborted" line in the capture buffer. The early
        // return after "Aborted" must prevent any keeper/eviction
        // output from appearing.
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("a"),
        );
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_B}.bundle"),
            Bytes::from("b"),
        );
        let prompter = ScriptedPrompter::new([Answer::Select(0), Answer::Confirm(false)]);
        let doctor = Doctor::new(store_arc(&mock), "myrepo", DoctorOpts::default(), &prompter);
        let (result, output) = capture_run(&doctor).await;
        result.expect("user-abort run");

        assert!(
            output.contains("Fix multiple bundles for repo myrepo"),
            "fixer header missing: {output:?}",
        );
        assert!(
            output.contains("Aborted"),
            "abort message missing: {output:?}",
        );
        // The early return after "Aborted" must prevent the
        // keeper announcement and eviction lines from appearing.
        assert!(
            !output.contains("Keeping"),
            "keeper line must not appear after user abort: {output:?}",
        );
        assert!(
            !output.contains("Moving"),
            "eviction line must not appear after user abort: {output:?}",
        );
    }
}

//! Two-phase mark-and-sweep garbage collection for orphan packs
//! (issue #66, Phase 5 of #52).
//!
//! Orphan packs are pack files in `<prefix>/packs/` that no
//! `chain.json` references. They accumulate from:
//!
//! - **Force push**: replaces a chain's segments; old packs become orphan.
//! - **Lost-race push**: a pre-lock pack upload by the loser of a
//!   concurrent push (Phase 2 design — packs upload pre-lock to keep
//!   the lock window short, and the loser's pack is left orphan).
//! - **Aborted push**: a crash between pack upload and chain.json
//!   commit leaves orphans the next push doesn't reach.
//! - **Branch deletion**: `delete-branch` removes `chain.json` and
//!   `path-index.json` but does not touch `<prefix>/packs/`. The
//!   issue umbrella's "exclusively owned by that branch" claim is
//!   wrong under content-hash dedup; pack keys can be shared across
//!   branches that ever pushed identical object sets.
//! - **Compaction** (when implemented): a chain rewrite leaves the
//!   superseded segment packs orphan.
//! - **Missing `.idx`** (rare): a `.pack` whose sibling `.idx` was
//!   manually deleted is treated as orphan and tombstoned.
//!
//! ## Two-phase mark-and-sweep
//!
//! Naive deletion ("delete every pack older than 24 h") races a
//! concurrent fetch on a freshly-orphaned pack: the pack's
//! `last_modified` reflects upload time, not orphan time. The
//! mark/sweep split fixes this by tombstoning at orphan time and
//! deferring deletion until after a configurable grace window.
//!
//! ### Phase 1 (mark)
//!
//! 1. List `<prefix>/refs/**/chain.json` across every ref namespace
//!    (`refs/heads/`, `refs/tags/`, `refs/notes/`, etc.), parse each,
//!    collect referenced pack content-shas.
//! 2. **Fail closed** on parse error: abort, log the bad key, do not
//!    write tombstones. A corrupt chain could under-report the
//!    referenced set and tombstone live packs.
//! 3. List `<prefix>/packs/`, derive the orphan set.
//! 4. Write `<prefix>/gc/tombstones-<run_id>-<rfc3339>.json`.
//!
//! ### Phase 2 (sweep)
//!
//! 1. List `<prefix>/gc/tombstones-*.json`.
//! 2. For each tombstone past the grace age:
//!    - Re-derive the orphan set from the *current* chain state.
//!      Repeated **per tombstone**, not cached across the sweep: a
//!      concurrent push committing `chain.json` mid-sweep would let
//!      a cached snapshot delete a pack the new chain references,
//!      permanently dangling the reference (issue #140). Force-revert
//!      is the canonical trigger — deterministic gix pack emission
//!      lets the new push reuse the tombstoned pack key without
//!      re-uploading. The cost is one `list("refs/")` per eligible
//!      tombstone vs one per sweep; correctness wins over the linear
//!      overhead for the O(1)-eligible-tombstones common case.
//!    - For each pack still orphan, delete `.pack` + `.idx`
//!      idempotently (a prior partial sweep is fine).
//!    - Delete the tombstone itself.
//! 3. Younger tombstones survive for the next sweep.
//!
//! ### Baseline-bundle tombstones (issue #134)
//!
//! Baseline bundles at `<prefix>/<ref>/<full_at>.bundle` are NOT
//! reapable by the mark/sweep flow above — they live outside
//! `<prefix>/packs/`, so [`list_pack_shas`] never sees them. The
//! compact and force-push code paths instead enqueue a baseline
//! tombstone at `<prefix>/gc/baseline-tomb-<uuid>.json` whenever they
//! supersede a baseline. Sweep processes those alongside pack
//! tombstones: after the grace window expires it re-checks the
//! current `chain.json` for the ref (skipping the delete if a later
//! push re-baselined to the same SHA), then deletes the bundle and
//! the tombstone. The bundle stays in place for the entire grace
//! window, so a concurrent fetch that read the prior `chain.json`
//! before the compact/force-push committed can still download it.
//!
//! ### `--force`
//!
//! Skips ONLY the grace window. The live-pack re-check still runs:
//! a tombstone whose SHA appears in the current chain set is left
//! alone. This closes the race where `mark()` snapshots packs after a
//! concurrent push has uploaded `packs/<sha>.{pack,idx}` but has not
//! yet committed `chain.json` — by sweep time the chain has landed
//! and the pack is live, so the stale tombstone must not delete it.
//! A `tracing::warn!` line records the operator's choice.
//!
//! ## Concurrency
//!
//! Two operators running `gc` simultaneously each get a `UUIDv4` run id
//! → distinct tombstone files, no clobber. Concurrent sweeps tolerate
//! `NotFound` on already-deleted packs. A push landing during mark
//! either uploaded *before* the pack list (orphan candidate, but its
//! chain commit lands before the chain re-list — survives) or *after*
//! the pack list (not in orphan set — survives). The grace window
//! covers a fetch reading an old chain whose packs are about to be
//! swept.

use std::collections::HashSet;

use bytes::Bytes;
use futures::stream::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::git::RefName;
use crate::keys;
use crate::object_store::{ObjectStore, ObjectStoreError, PutOpts};
use crate::protocol::fetch::MAX_FETCH_CONCURRENCY;

use super::PackchainError;
use super::manifest::load_chain;
use super::schema::{ChainManifest, Sha40};

/// Default grace window between mark and sweep (24 hours). A pack
/// tombstoned during mark is only deletable after this duration has
/// elapsed since `marked_at`.
pub const DEFAULT_GRACE_HOURS: u64 = 24;

/// Environment variable that overrides [`DEFAULT_GRACE_HOURS`] when
/// set to a positive integer. Mirrors the shape of
/// `GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS` used by the protocol REPL.
pub const ENV_GC_GRACE_HOURS: &str = "GIT_REMOTE_OBJECT_STORE_GC_GRACE_HOURS";

/// On-bucket schema version this build reads and writes.
pub const TOMBSTONE_SCHEMA_VERSION: u32 = 1;

/// On-bucket tombstone — a record of one mark phase's orphan set.
///
/// Lives at `<prefix>/gc/tombstones-<run_id>-<rfc3339>.json`. The
/// timestamp in the filename is for human inspection; the
/// authoritative `marked_at` is the field inside the JSON body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Tombstone {
    /// Schema version. Always [`TOMBSTONE_SCHEMA_VERSION`] when written.
    pub(crate) v: u32,
    /// `UUIDv4` run identifier. Two concurrent `gc` runs each get a
    /// distinct id, so their tombstone keys don't clobber.
    pub(crate) run_id: String,
    /// RFC 3339 timestamp at which the mark phase produced this set.
    /// Sweep compares this against the grace window.
    pub(crate) marked_at: String,
    /// Content-shas of orphan packs at mark time. Sweep re-checks
    /// each against the current chain state before deleting.
    pub(crate) orphan_packs: Vec<Sha40>,
}

impl Tombstone {
    /// Parse `bytes` as a tombstone JSON, validating the schema
    /// version before returning.
    ///
    /// # Errors
    ///
    /// - [`PackchainError::ParseJson`] for malformed JSON / missing
    ///   fields / `Sha40` validation failures.
    /// - [`PackchainError::UnsupportedSchemaVersion`] when `v` is not
    ///   [`TOMBSTONE_SCHEMA_VERSION`].
    pub(crate) fn from_json_bytes(bytes: &[u8]) -> Result<Self, PackchainError> {
        let parsed: Self = serde_json::from_slice(bytes)?;
        if parsed.v != TOMBSTONE_SCHEMA_VERSION {
            return Err(PackchainError::UnsupportedSchemaVersion {
                found: parsed.v,
                expected: TOMBSTONE_SCHEMA_VERSION,
            });
        }
        Ok(parsed)
    }

    /// Render to pretty-printed JSON bytes.
    ///
    /// # Errors
    ///
    /// `serde_json::to_vec_pretty` is infallible for this schema
    /// today, but the function returns `Result` for forward
    /// compatibility with future fields.
    pub(crate) fn to_json_pretty(&self) -> Result<Vec<u8>, PackchainError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }
}

/// On-bucket tombstone for a superseded baseline bundle (issue #134).
///
/// Lives at `<prefix>/gc/baseline-tomb-<uuid>.json`. Written by
/// [`super::compact`] and [`super::push`] whenever a chain rewrite
/// makes a `<prefix>/<ref>/<sha>.bundle` unreachable. Unlike pack
/// tombstones the body names a specific (ref, sha) — there is exactly
/// one bundle key per record — so [`sweep`] does not need to re-derive
/// an orphan set from listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BaselineTombstone {
    /// Schema version. Always [`TOMBSTONE_SCHEMA_VERSION`] when written.
    pub(crate) v: u32,
    /// RFC 3339 timestamp at which the tombstone was written. Sweep
    /// compares this against the grace window.
    pub(crate) marked_at: String,
    /// Ref the orphaned bundle belonged to (e.g. `refs/heads/main`).
    /// Stored as a raw string for forward compatibility with whatever
    /// `RefName` accepts at sweep time.
    pub(crate) ref_name: String,
    /// Content-SHA of the bundle (matches the `<sha>.bundle` filename).
    /// Sweep skips the delete when the ref's current `chain.full_at`
    /// equals this SHA (a later push re-baselined to the same tip).
    pub(crate) sha: Sha40,
}

impl BaselineTombstone {
    /// Parse `bytes` as a baseline tombstone JSON, validating the
    /// schema version before returning.
    ///
    /// # Errors
    ///
    /// - [`PackchainError::ParseJson`] for malformed JSON / missing
    ///   fields / `Sha40` validation failures.
    /// - [`PackchainError::UnsupportedSchemaVersion`] when `v` is not
    ///   [`TOMBSTONE_SCHEMA_VERSION`].
    pub(crate) fn from_json_bytes(bytes: &[u8]) -> Result<Self, PackchainError> {
        let parsed: Self = serde_json::from_slice(bytes)?;
        if parsed.v != TOMBSTONE_SCHEMA_VERSION {
            return Err(PackchainError::UnsupportedSchemaVersion {
                found: parsed.v,
                expected: TOMBSTONE_SCHEMA_VERSION,
            });
        }
        Ok(parsed)
    }

    /// Render to pretty-printed JSON bytes.
    pub(crate) fn to_json_pretty(&self) -> Result<Vec<u8>, PackchainError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }
}

/// Write a baseline tombstone for the bundle at
/// `<prefix>/<ref_name>/<sha>.bundle` (issue #134).
///
/// Called from [`super::compact`] and [`super::push`] after the new
/// `chain.json` is durable — at that point the bundle has no chain
/// reference and is eligible for deletion, but a fetch that loaded
/// the prior chain may still be about to GET it. The tombstone
/// defers the delete to the next `gc sweep` past the grace window.
///
/// `prior_full_sha` is the SHA of the superseded baseline; `current_full_sha`
/// is the new chain's `full_at`. When they are equal the function
/// returns without writing a tombstone — the keys alias the same live
/// bundle (compact left `full_at` unchanged, or force-push targeted
/// the same tip).
///
/// # Errors
///
/// Returns [`PackchainError::Store`] on a PUT failure. Callers run
/// this AFTER `chain.json` is committed and must treat the failure as
/// best-effort: log a warning and report success, since retrying
/// would short-circuit through `AlreadyMinimal` and never re-attempt
/// the cleanup.
pub(crate) async fn write_baseline_tombstone(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    ref_name: &RefName,
    prior_full_sha: &Sha40,
    current_full_sha: &Sha40,
) -> Result<(), PackchainError> {
    if prior_full_sha == current_full_sha {
        return Ok(());
    }
    let marked_at = OffsetDateTime::now_utc().format(&Rfc3339).map_err(|e| {
        PackchainError::Io(std::io::Error::other(format!("rfc3339 format failed: {e}")))
    })?;
    let tombstone = BaselineTombstone {
        v: TOMBSTONE_SCHEMA_VERSION,
        marked_at,
        ref_name: ref_name.as_str().to_owned(),
        sha: prior_full_sha.clone(),
    };
    let key = baseline_tombstone_key(prefix.unwrap_or(""), &Uuid::new_v4().to_string());
    let body = Bytes::from(tombstone.to_json_pretty()?);
    store.put_bytes(&key, body, PutOpts::default()).await?;
    debug!(
        key = %key,
        ref_path = %ref_name.as_str(),
        sha = %prior_full_sha.as_str(),
        "gc: baseline tombstone written",
    );
    Ok(())
}

/// Outcome of [`mark`].
#[derive(Debug, Clone)]
pub struct MarkOutcome {
    /// `UUIDv4` run id assigned to this mark pass. Embedded in the
    /// tombstone filename and body.
    pub run_id: String,
    /// Number of orphan packs identified.
    pub orphan_count: usize,
    /// Bucket key the tombstone was written to.
    pub tombstone_key: String,
}

/// Outcome of [`sweep`].
#[derive(Debug, Clone, Default)]
pub struct SweepOutcome {
    /// Tombstones whose packs were deleted (and which were themselves
    /// deleted as a result).
    pub swept_tombstones: usize,
    /// Tombstones still inside the grace window — left for the next
    /// sweep.
    pub deferred_tombstones: usize,
    /// Pack file deletions executed (counts both `.pack` and `.idx`
    /// deletions, so two per orphan in the typical case).
    pub deleted_objects: usize,
    /// Tombstoned packs that were no longer orphan at sweep time
    /// (re-referenced between mark and sweep, or deleted by an
    /// earlier sweep). Skipped without error.
    pub skipped_repointed_packs: usize,
}

/// Knobs for [`mark`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MarkOpts {
    /// When `true`, list and report but do not write a tombstone file
    /// or modify the bucket. Used by `doctor` to surface orphan stats.
    pub dry_run: bool,
}

/// Knobs for [`sweep`].
#[derive(Debug, Clone, Copy)]
pub struct SweepOpts {
    /// Grace duration in hours. Tombstones with `marked_at` younger
    /// than this stay deferred. Ignored when `force` is `true`.
    pub grace_hours: u64,
    /// When `true`, skip the grace check. The live-pack re-derive
    /// still runs — a tombstone whose SHA is now referenced by a
    /// committed chain is left alone (closes the mark/commit race
    /// from #117). The grace window is the only safety check this
    /// flag suppresses; concurrent fetches that still hold a SHA in
    /// flight are NOT protected by either path.
    pub force: bool,
}

impl Default for SweepOpts {
    fn default() -> Self {
        Self {
            grace_hours: DEFAULT_GRACE_HOURS,
            force: false,
        }
    }
}

/// Read [`DEFAULT_GRACE_HOURS`] subject to the
/// [`ENV_GC_GRACE_HOURS`] override. Returns the default for unset
/// vars, non-numeric values, or zero (a zero grace would defeat the
/// mark/sweep design's point).
#[must_use]
pub fn grace_hours_from_env() -> u64 {
    std::env::var(ENV_GC_GRACE_HOURS)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|h| *h > 0)
        .unwrap_or(DEFAULT_GRACE_HOURS)
}

/// Run the mark phase: list every chain, list every pack, write a
/// tombstone naming the orphans.
///
/// `prefix` is the repository prefix without leading or trailing
/// slashes — pass an empty string for bucket-root repositories.
///
/// # Errors
///
/// - Any chain.json that fails to parse aborts the mark with
///   [`PackchainError::ParseJson`] / [`PackchainError::InvalidSha`] /
///   [`PackchainError::UnsupportedSchemaVersion`]. The tombstone is
///   not written. Operators must repair the bad chain (or remove it)
///   before re-running.
/// - [`PackchainError::Store`] / [`PackchainError::Io`] for transport
///   or local-I/O failures.
///
/// # Example
///
/// ```no_run
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use git_remote_object_store::Remote;
/// use git_remote_object_store::packchain::gc::{MarkOpts, mark};
///
/// let remote = Remote::connect("s3+https://bucket/repo?engine=packchain").await?;
/// let outcome = mark(remote.store(), remote.prefix(), MarkOpts::default()).await?;
/// println!(
///     "{} orphan pack(s) tombstoned (run id {})",
///     outcome.orphan_count, outcome.run_id,
/// );
/// # Ok(())
/// # }
/// ```
pub async fn mark(
    store: &dyn ObjectStore,
    prefix: &str,
    opts: MarkOpts,
) -> Result<MarkOutcome, PackchainError> {
    let referenced = list_referenced_packs(store, prefix).await?;
    let on_bucket = list_pack_shas(store, prefix).await?;
    let orphans: Vec<Sha40> = on_bucket
        .into_iter()
        .filter(|sha| !referenced.contains(sha))
        .collect();

    let run_id = Uuid::new_v4().to_string();
    let marked_at = OffsetDateTime::now_utc().format(&Rfc3339).map_err(|e| {
        PackchainError::Io(std::io::Error::other(format!("rfc3339 format failed: {e}")))
    })?;
    let tombstone_key = tombstone_key(prefix, &run_id, &marked_at);
    let orphan_count = orphans.len();
    let tombstone = Tombstone {
        v: TOMBSTONE_SCHEMA_VERSION,
        run_id: run_id.clone(),
        marked_at,
        orphan_packs: orphans,
    };
    let outcome = MarkOutcome {
        run_id,
        orphan_count,
        tombstone_key,
    };

    if opts.dry_run {
        debug!(
            run_id = %outcome.run_id,
            orphans = outcome.orphan_count,
            "gc mark: dry-run, not writing tombstone",
        );
        return Ok(outcome);
    }

    if outcome.orphan_count == 0 {
        info!(run_id = %outcome.run_id, "gc mark: no orphans; skipping tombstone");
        return Ok(outcome);
    }

    let body = Bytes::from(tombstone.to_json_pretty()?);
    store
        .put_bytes(&outcome.tombstone_key, body, PutOpts::default())
        .await?;
    info!(
        run_id = %outcome.run_id,
        orphans = outcome.orphan_count,
        key = %outcome.tombstone_key,
        "gc mark: tombstone written",
    );
    Ok(outcome)
}

/// Run the sweep phase: walk tombstones, delete eligible orphans.
///
/// `prefix` and the threading semantics match [`mark`].
///
/// # Errors
///
/// Sweep is best-effort: a single tombstone failure does not abort
/// the run (errors are logged and the next tombstone is tried).
/// Returns [`PackchainError::Store`] only when the initial
/// tombstone-list call fails.
///
/// # Example
///
/// ```no_run
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use git_remote_object_store::Remote;
/// use git_remote_object_store::packchain::gc::{SweepOpts, sweep};
///
/// let remote = Remote::connect("s3+https://bucket/repo?engine=packchain").await?;
/// let outcome = sweep(
///     remote.store(),
///     remote.prefix(),
///     SweepOpts::default(),
/// )
/// .await?;
/// println!(
///     "swept {} tombstone(s), deleted {} object(s), deferred {}",
///     outcome.swept_tombstones,
///     outcome.deleted_objects,
///     outcome.deferred_tombstones,
/// );
/// # Ok(())
/// # }
/// ```
pub async fn sweep(
    store: &dyn ObjectStore,
    prefix: &str,
    opts: SweepOpts,
) -> Result<SweepOutcome, PackchainError> {
    let tombstones_prefix = gc_listing_prefix(prefix);
    let metas = store.list(&tombstones_prefix).await?;
    let mut outcome = SweepOutcome::default();

    if opts.force {
        warn!("gc sweep: --force in effect; skipping grace window");
    }

    for meta in metas {
        if !meta.key.as_bytes().ends_with(b".json") {
            continue;
        }
        let step = if is_tombstone_key(&meta.key, prefix) {
            sweep_one_tombstone(store, prefix, &meta.key, opts).await
        } else if is_baseline_tombstone_key(&meta.key, prefix) {
            sweep_one_baseline_tombstone(store, prefix, &meta.key, opts).await
        } else {
            continue;
        };
        match step {
            Ok(SweepStep::Deferred) => outcome.deferred_tombstones += 1,
            Ok(SweepStep::Swept {
                deleted_objects,
                skipped_repointed_packs,
            }) => {
                outcome.swept_tombstones += 1;
                outcome.deleted_objects += deleted_objects;
                outcome.skipped_repointed_packs += skipped_repointed_packs;
            }
            Err(e) => {
                warn!(key = %meta.key, error = %e, "gc sweep: tombstone failed");
            }
        }
    }
    Ok(outcome)
}

#[derive(Debug)]
enum SweepStep {
    Deferred,
    Swept {
        deleted_objects: usize,
        skipped_repointed_packs: usize,
    },
}

async fn sweep_one_tombstone(
    store: &dyn ObjectStore,
    prefix: &str,
    tombstone_key: &str,
    opts: SweepOpts,
) -> Result<SweepStep, PackchainError> {
    let body = match store.get_bytes(tombstone_key).await {
        Ok(b) => b,
        Err(ObjectStoreError::NotFound(_)) => {
            // Concurrent sweep already cleaned this up.
            return Ok(SweepStep::Swept {
                deleted_objects: 0,
                skipped_repointed_packs: 0,
            });
        }
        Err(e) => return Err(PackchainError::Store(e)),
    };
    let tombstone = Tombstone::from_json_bytes(&body)?;

    if !opts.force {
        let marked_at = OffsetDateTime::parse(&tombstone.marked_at, &Rfc3339).map_err(|e| {
            PackchainError::Io(std::io::Error::other(format!(
                "tombstone marked_at parse failed: {e}"
            )))
        })?;
        let age_hours = (OffsetDateTime::now_utc() - marked_at).whole_hours();
        // Negative age = a tombstone marked in the future (operator
        // clock skew). Treat as "still within grace" rather than
        // sweeping prematurely. The `try_from` is the canonical way
        // to compare an `i64` against an unsigned grace window
        // without a sign-loss cast.
        let age_within_grace = age_hours
            .try_into()
            .map_or(true, |hours: u64| hours < opts.grace_hours);
        if age_within_grace {
            debug!(
                key = %tombstone_key,
                marked_at = %tombstone.marked_at,
                "gc sweep: tombstone within grace window",
            );
            return Ok(SweepStep::Deferred);
        }
    }

    // Re-derive the live referenced set per tombstone, AFTER the
    // grace check passes — never cache across iterations (issue #140).
    // A concurrent push committing chain.json after a sweep-wide
    // snapshot would let sweep delete a pack the new chain references,
    // permanently dangling the chain. Force-revert is the canonical
    // trigger: gix pack emission is deterministic for the same object
    // set, so the new pack key aliases the tombstoned one and the push
    // skips upload, only touching chain.json. Per-tombstone re-listing
    // costs one extra `list("refs/")` + bounded-parallel chain GETs
    // per eligible tombstone vs once per sweep — for typical workloads
    // with O(1) eligible tombstones this is negligible; correctness
    // wins over the linear-in-tombstones overhead. The recompute also
    // runs under --force: that flag suppresses the grace window only,
    // NOT this guard (issue #117).
    let referenced = list_referenced_packs(store, prefix).await?;

    let mut deleted_objects = 0usize;
    let mut skipped_repointed_packs = 0usize;
    for sha in &tombstone.orphan_packs {
        // Always honour the live-pack guard, including under --force.
        // See the recompute comment above and issue #117 for why.
        if referenced.contains(sha) {
            skipped_repointed_packs += 1;
            debug!(
                sha = %sha.as_str(),
                "gc sweep: tombstoned pack re-referenced; skipping",
            );
            continue;
        }
        let pack_key = super::keys::pack_key(Some(prefix), sha);
        let idx_key = super::keys::pack_idx_key(Some(prefix), sha);
        if delete_idempotent(store, &pack_key).await? {
            deleted_objects += 1;
        }
        if delete_idempotent(store, &idx_key).await? {
            deleted_objects += 1;
        }
    }
    // Drop the tombstone last so a sweep crash mid-deletion leaves a
    // tombstone the next sweep can finish.
    delete_idempotent(store, tombstone_key).await?;
    info!(
        key = %tombstone_key,
        deleted = deleted_objects,
        skipped = skipped_repointed_packs,
        "gc sweep: tombstone applied",
    );
    Ok(SweepStep::Swept {
        deleted_objects,
        skipped_repointed_packs,
    })
}

/// Sweep one baseline tombstone (issue #134). Parses the tombstone,
/// honours the grace window, re-checks the ref's current `chain.full_at`
/// to skip a re-baselined-to-same-SHA case, and then idempotently
/// deletes both the bundle and the tombstone.
///
/// The live-state recheck mirrors the pack sweep's
/// `referenced.contains` guard: a tombstone written by a force-push
/// can be invalidated by a subsequent force-push that lands on the
/// same SHA, in which case the bundle is once again live.
async fn sweep_one_baseline_tombstone(
    store: &dyn ObjectStore,
    prefix: &str,
    tombstone_key: &str,
    opts: SweepOpts,
) -> Result<SweepStep, PackchainError> {
    let body = match store.get_bytes(tombstone_key).await {
        Ok(b) => b,
        Err(ObjectStoreError::NotFound(_)) => {
            return Ok(SweepStep::Swept {
                deleted_objects: 0,
                skipped_repointed_packs: 0,
            });
        }
        Err(e) => return Err(PackchainError::Store(e)),
    };
    let tombstone = BaselineTombstone::from_json_bytes(&body)?;

    if !opts.force {
        let marked_at = OffsetDateTime::parse(&tombstone.marked_at, &Rfc3339).map_err(|e| {
            PackchainError::Io(std::io::Error::other(format!(
                "baseline tombstone marked_at parse failed: {e}"
            )))
        })?;
        let age_hours = (OffsetDateTime::now_utc() - marked_at).whole_hours();
        // Same clock-skew handling as `sweep_one_tombstone`: a negative
        // age (tombstone marked in the future) is treated as still
        // inside the grace window.
        let age_within_grace = age_hours
            .try_into()
            .map_or(true, |hours: u64| hours < opts.grace_hours);
        if age_within_grace {
            debug!(
                key = %tombstone_key,
                marked_at = %tombstone.marked_at,
                "gc sweep: baseline tombstone within grace window",
            );
            return Ok(SweepStep::Deferred);
        }
    }

    // Re-check the live chain. A subsequent push that re-baselined to
    // the same SHA (force-push at the same tip, or compact short-cut)
    // makes this bundle live again — leave it alone, drop the now-stale
    // tombstone. A missing ref (chain deleted) means the bundle is also
    // unreachable; proceed with the delete.
    let ref_name = match RefName::new(tombstone.ref_name.clone()) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                key = %tombstone_key,
                ref_name = %tombstone.ref_name,
                error = %e,
                "gc sweep: baseline tombstone names invalid ref; dropping tombstone",
            );
            delete_idempotent(store, tombstone_key).await?;
            return Ok(SweepStep::Swept {
                deleted_objects: 0,
                skipped_repointed_packs: 0,
            });
        }
    };
    let prefix_opt = (!prefix.is_empty()).then_some(prefix);
    let chain = load_chain(store, prefix_opt, &ref_name).await?;
    let mut skipped_repointed_packs = 0usize;
    let mut deleted_objects = 0usize;
    let still_live = chain.as_ref().is_some_and(|c| c.full_at == tombstone.sha);
    if still_live {
        skipped_repointed_packs += 1;
        debug!(
            key = %tombstone_key,
            ref_path = %ref_name.as_str(),
            sha = %tombstone.sha.as_str(),
            "gc sweep: baseline re-referenced; skipping delete",
        );
    } else {
        let bundle_key = keys::bundle_key(prefix_opt, &ref_name, tombstone.sha.as_str());
        if delete_idempotent(store, &bundle_key).await? {
            deleted_objects += 1;
        }
    }
    // Drop the tombstone last so a crash mid-delete leaves it for the
    // next sweep to finish.
    delete_idempotent(store, tombstone_key).await?;
    info!(
        key = %tombstone_key,
        deleted = deleted_objects,
        skipped = skipped_repointed_packs,
        "gc sweep: baseline tombstone applied",
    );
    Ok(SweepStep::Swept {
        deleted_objects,
        skipped_repointed_packs,
    })
}

/// `<prefix>/gc/` prefix for [`ObjectStore::list`]. Empty `prefix`
/// drops the leading slash (matches the project's bucket-root rule).
fn gc_listing_prefix(prefix: &str) -> String {
    keys::join(Some(prefix), "gc/")
}

/// Build a tombstone key. The `marked_at` segment may contain `:`
/// characters; S3 / Azure both accept colons in keys.
fn tombstone_key(prefix: &str, run_id: &str, marked_at: &str) -> String {
    keys::join(
        Some(prefix),
        &format!("gc/tombstones-{run_id}-{marked_at}.json"),
    )
}

/// Build a baseline tombstone key. UUID-keyed so concurrent compacts
/// / force-pushes across different refs never clobber, and the
/// timestamp lives in the body rather than the filename to keep the
/// `is_baseline_tombstone_key` predicate cheap.
fn baseline_tombstone_key(prefix: &str, run_id: &str) -> String {
    keys::join(Some(prefix), &format!("gc/baseline-tomb-{run_id}.json"))
}

/// Robust check that `key` is a tombstone under our prefix. Guards
/// against unrelated `.json` files in `<prefix>/gc/` and against a
/// regression where a future schema rev moves the prefix.
///
/// Root-prefix (`prefix == ""`) case: `expected_prefix` is just
/// `"gc/tombstones-"`, so every `gc/tombstones-*.json` key at the
/// bucket root matches. That is the intended behaviour — a root
/// repo owns the entire `gc/` namespace.
fn is_tombstone_key(key: &str, prefix: &str) -> bool {
    let expected_prefix = keys::join(Some(prefix), "gc/tombstones-");
    key.starts_with(&expected_prefix)
}

/// Robust check that `key` is a baseline tombstone under our prefix
/// (issue #134). Mirrors [`is_tombstone_key`] for the
/// `gc/baseline-tomb-` namespace.
fn is_baseline_tombstone_key(key: &str, prefix: &str) -> bool {
    let expected_prefix = keys::join(Some(prefix), "gc/baseline-tomb-");
    key.starts_with(&expected_prefix)
}

/// List every `<prefix>/refs/**/chain.json` (across every ref
/// namespace — `refs/heads/`, `refs/tags/`, `refs/notes/`, etc.) and
/// union the pack content-shas they reference. Fail closed on parse
/// error.
async fn list_referenced_packs(
    store: &dyn ObjectStore,
    prefix: &str,
) -> Result<HashSet<Sha40>, PackchainError> {
    let refs_prefix = keys::join(Some(prefix), "refs/");
    let metas = store.list(&refs_prefix).await?;

    // Bounded-parallel `get_bytes` per chain.json, parse-as-fetched.
    // Mirrors `list::list_refs` (#89 widened the listing prefix to
    // all `refs/` namespaces, so candidate count scales with branches
    // + tags + notes). `MAX_FETCH_CONCURRENCY` (= 8) is the same bound
    // Phase 3 fetch uses for chain pack downloads. `try_fold` folds
    // each body into the set as soon as `buffer_unordered` yields it,
    // so parse overlaps the next batch's fetch latency and no
    // intermediate `Vec<Bytes>` is held.
    //
    // Fail-closed semantics: a transport failure on any GET, or a
    // parse failure on any chain, aborts the run — the mark phase
    // cannot tombstone live packs because of an under-reporting
    // corrupt chain.
    futures::stream::iter(
        metas
            .into_iter()
            .filter(|m| super::keys::is_chain_json_key(&m.key))
            .map(|m| m.key),
    )
    .map(|key| async move { store.get_bytes(&key).await.map_err(PackchainError::Store) })
    .buffer_unordered(MAX_FETCH_CONCURRENCY)
    .try_fold(HashSet::<Sha40>::new(), |mut acc, body| async move {
        let chain = ChainManifest::from_json_bytes(&body)?;
        for segment in chain.segments {
            // gc fails closed on a malformed pack key — the chain is
            // corrupt and tombstoning live packs based on it would be
            // unsafe. Uses the same `MalformedPackEntry` variant as
            // every other consumer (read, fetch, compact) so error
            // wording stays aligned across the engine.
            let sha = super::keys::segment_pack_sha(&segment)?;
            acc.insert(sha);
        }
        Ok(acc)
    })
    .await
}

/// List every `<prefix>/packs/*.pack` and `*.idx` and return the union
/// of their content-shas. The set is keyed by sha so a pack with a
/// missing-but-tombstoneable idx still counts (and vice versa).
async fn list_pack_shas(
    store: &dyn ObjectStore,
    prefix: &str,
) -> Result<HashSet<Sha40>, PackchainError> {
    let packs_prefix = keys::join(Some(prefix), "packs/");
    let metas = store.list(&packs_prefix).await?;
    let mut shas: HashSet<Sha40> = HashSet::new();
    for meta in metas {
        let basename = meta.key.rsplit('/').next().unwrap_or(meta.key.as_str());
        let candidate = basename
            .strip_suffix(".pack")
            .or_else(|| basename.strip_suffix(".idx"));
        if let Some(sha) = candidate
            && let Ok(parsed) = Sha40::try_new(sha)
        {
            shas.insert(parsed);
        }
    }
    Ok(shas)
}

/// Best-effort delete: returns `Ok(true)` on a real delete, `Ok(false)`
/// when the object was already absent (concurrent sweep raced ahead,
/// or a partial sweep ran earlier).
async fn delete_idempotent(store: &dyn ObjectStore, key: &str) -> Result<bool, PackchainError> {
    match store.delete(key).await {
        Ok(()) => Ok(true),
        Err(ObjectStoreError::NotFound(_)) => Ok(false),
        Err(e) => Err(PackchainError::Store(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RefName;
    use crate::object_store::mock::MockStore;
    use crate::packchain::manifest::write_chain;
    use crate::packchain::schema::ChainSegment;

    const SHA_TIP: &str = "0000000000000000000000000000000000000001";
    const SHA_FULL: &str = "0000000000000000000000000000000000000002";
    const SHA_PACK_LIVE: &str = "1111111111111111111111111111111111111111";
    const SHA_PACK_ORPHAN: &str = "2222222222222222222222222222222222222222";
    const SHA_PACK_ORPHAN_2: &str = "3333333333333333333333333333333333333333";

    fn sha40(s: &str) -> Sha40 {
        Sha40::try_new(s).unwrap()
    }

    fn ref_main() -> RefName {
        RefName::new("refs/heads/main").unwrap()
    }

    fn segment(pack_sha: &str, parent: Option<&str>) -> ChainSegment {
        ChainSegment {
            sha: sha40(SHA_TIP),
            parent_sha: parent.map(sha40),
            pack: format!("packs/{pack_sha}.pack"),
            bytes: 1_024,
        }
    }

    async fn seed_live_chain(store: &MockStore, prefix: Option<&str>) {
        let chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_FULL),
            segments: vec![segment(SHA_PACK_LIVE, None)],
        };
        write_chain(store, prefix, &ref_main(), &chain)
            .await
            .unwrap();
    }

    fn insert_pack_pair(store: &MockStore, prefix: Option<&str>, sha: &str) {
        let pack_key = super::super::keys::pack_key(prefix, &sha40(sha));
        let idx_key = super::super::keys::pack_idx_key(prefix, &sha40(sha));
        store.insert(pack_key, Bytes::from_static(b"PACKDATA"));
        store.insert(idx_key, Bytes::from_static(b"IDXDATA"));
    }

    // --- mark -----------------------------------------------------------

    #[tokio::test]
    async fn mark_with_no_chains_treats_all_packs_as_orphan() {
        let store = MockStore::new();
        insert_pack_pair(&store, Some("repo"), SHA_PACK_ORPHAN);
        let outcome = mark(&store, "repo", MarkOpts::default()).await.unwrap();
        assert_eq!(outcome.orphan_count, 1);
        // Tombstone written to the correct prefix.
        let body = store.get_bytes(&outcome.tombstone_key).await.unwrap();
        let parsed = Tombstone::from_json_bytes(&body).unwrap();
        assert_eq!(parsed.orphan_packs, vec![sha40(SHA_PACK_ORPHAN)]);
    }

    #[tokio::test]
    async fn mark_skips_chain_referenced_packs() {
        let store = MockStore::new();
        seed_live_chain(&store, Some("repo")).await;
        insert_pack_pair(&store, Some("repo"), SHA_PACK_LIVE);
        insert_pack_pair(&store, Some("repo"), SHA_PACK_ORPHAN);
        let outcome = mark(&store, "repo", MarkOpts::default()).await.unwrap();
        assert_eq!(outcome.orphan_count, 1);
        let body = store.get_bytes(&outcome.tombstone_key).await.unwrap();
        let parsed = Tombstone::from_json_bytes(&body).unwrap();
        assert_eq!(parsed.orphan_packs, vec![sha40(SHA_PACK_ORPHAN)]);
    }

    #[tokio::test]
    async fn mark_no_orphans_skips_tombstone_write() {
        let store = MockStore::new();
        seed_live_chain(&store, Some("repo")).await;
        insert_pack_pair(&store, Some("repo"), SHA_PACK_LIVE);
        let outcome = mark(&store, "repo", MarkOpts::default()).await.unwrap();
        assert_eq!(outcome.orphan_count, 0);
        // No tombstone listed.
        let metas = store.list("repo/gc/").await.unwrap();
        assert!(
            metas.is_empty(),
            "tombstone must not exist for empty orphan set"
        );
    }

    #[tokio::test]
    async fn mark_dry_run_does_not_write_tombstone() {
        let store = MockStore::new();
        insert_pack_pair(&store, Some("repo"), SHA_PACK_ORPHAN);
        let outcome = mark(&store, "repo", MarkOpts { dry_run: true })
            .await
            .unwrap();
        assert_eq!(outcome.orphan_count, 1);
        let metas = store.list("repo/gc/").await.unwrap();
        assert!(metas.is_empty(), "dry-run must not write tombstone");
    }

    #[tokio::test]
    async fn mark_treats_tag_chain_referenced_packs_as_live() {
        // A pack referenced only from a chain under refs/tags/ must
        // not be tombstoned. (Regression for issue #89.)
        let store = MockStore::new();
        let chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_FULL),
            segments: vec![segment(SHA_PACK_LIVE, None)],
        };
        let tag_ref = RefName::new("refs/tags/v1").unwrap();
        write_chain(&store, Some("repo"), &tag_ref, &chain)
            .await
            .unwrap();
        insert_pack_pair(&store, Some("repo"), SHA_PACK_LIVE);
        insert_pack_pair(&store, Some("repo"), SHA_PACK_ORPHAN);

        let referenced = list_referenced_packs(&store, "repo").await.unwrap();
        assert!(
            referenced.contains(&sha40(SHA_PACK_LIVE)),
            "pack referenced from refs/tags/ chain must be in the live set",
        );

        let outcome = mark(&store, "repo", MarkOpts::default()).await.unwrap();
        assert_eq!(outcome.orphan_count, 1);
        let body = store.get_bytes(&outcome.tombstone_key).await.unwrap();
        let parsed = Tombstone::from_json_bytes(&body).unwrap();
        assert_eq!(parsed.orphan_packs, vec![sha40(SHA_PACK_ORPHAN)]);
    }

    #[tokio::test]
    async fn mark_treats_notes_chain_referenced_packs_as_live() {
        // refs/notes/commits is the standard git notes ref. A pack
        // referenced only from a notes chain must not be tombstoned.
        let store = MockStore::new();
        let chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_FULL),
            segments: vec![segment(SHA_PACK_LIVE, None)],
        };
        let notes_ref = RefName::new("refs/notes/commits").unwrap();
        write_chain(&store, Some("repo"), &notes_ref, &chain)
            .await
            .unwrap();
        insert_pack_pair(&store, Some("repo"), SHA_PACK_LIVE);

        let referenced = list_referenced_packs(&store, "repo").await.unwrap();
        assert!(
            referenced.contains(&sha40(SHA_PACK_LIVE)),
            "pack referenced from refs/notes/ chain must be in the live set",
        );

        let outcome = mark(&store, "repo", MarkOpts::default()).await.unwrap();
        assert_eq!(outcome.orphan_count, 0);
    }

    #[tokio::test]
    async fn list_referenced_packs_unions_across_namespaces() {
        // A live chain in refs/heads/ AND in refs/tags/ both
        // contribute to the referenced set.
        let store = MockStore::new();
        let head_chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_FULL),
            segments: vec![segment(SHA_PACK_LIVE, None)],
        };
        write_chain(&store, Some("repo"), &ref_main(), &head_chain)
            .await
            .unwrap();
        let tag_chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_FULL),
            segments: vec![segment(SHA_PACK_ORPHAN_2, None)],
        };
        let tag_ref = RefName::new("refs/tags/v1").unwrap();
        write_chain(&store, Some("repo"), &tag_ref, &tag_chain)
            .await
            .unwrap();

        let referenced = list_referenced_packs(&store, "repo").await.unwrap();
        assert!(referenced.contains(&sha40(SHA_PACK_LIVE)));
        assert!(referenced.contains(&sha40(SHA_PACK_ORPHAN_2)));
        assert_eq!(referenced.len(), 2);
    }

    #[tokio::test]
    async fn list_referenced_packs_ignores_sibling_artefacts() {
        // path-index.json, .bundle baselines, and other artefacts
        // under refs/<namespace>/<name>/ must not be parsed as
        // chain.json.
        let store = MockStore::new();
        seed_live_chain(&store, Some("repo")).await;
        // Add sibling artefacts that share the ref directory.
        store.insert(
            "repo/refs/heads/main/path-index.json",
            Bytes::from_static(b"{}"),
        );
        store.insert(
            format!("repo/refs/heads/main/{SHA_TIP}.bundle"),
            Bytes::from_static(b"BUNDLE"),
        );
        // And a tombstone-style key under refs/ that must be filtered.
        store.insert(
            "repo/refs/tags/v1/path-index.json",
            Bytes::from_static(b"{}"),
        );

        let referenced = list_referenced_packs(&store, "repo").await.unwrap();
        assert_eq!(referenced.len(), 1);
        assert!(referenced.contains(&sha40(SHA_PACK_LIVE)));
    }

    #[tokio::test]
    async fn list_referenced_packs_empty_for_no_chains() {
        let store = MockStore::new();
        let referenced = list_referenced_packs(&store, "repo").await.unwrap();
        assert!(referenced.is_empty());
    }

    #[tokio::test]
    async fn list_referenced_packs_unions_many_chains_with_bounded_parallel_fetch() {
        // Regression guard for the buffer_unordered fetch path:
        // exercise more chain.json bodies than MAX_FETCH_CONCURRENCY
        // (= 8) so multiple batches must complete and union without
        // dropping any pack sha. Spans heads, tags, and notes so the
        // listing prefix widening from #89 stays exercised.
        let store = MockStore::new();
        let chain_count = MAX_FETCH_CONCURRENCY * 3 + 1;
        let namespaces = ["refs/heads", "refs/tags", "refs/notes"];
        let mut expected: HashSet<Sha40> = HashSet::new();
        for i in 0..chain_count {
            let pack_sha = format!("{:040x}", 0x1000 + i);
            let pack_sha40 = sha40(&pack_sha);
            let namespace = namespaces[i % namespaces.len()];
            let ref_name = RefName::new(format!("{namespace}/r{i}")).unwrap();
            let chain = ChainManifest {
                v: 1,
                tip: sha40(SHA_TIP),
                full_at: sha40(SHA_FULL),
                segments: vec![ChainSegment {
                    sha: sha40(SHA_TIP),
                    parent_sha: None,
                    pack: format!("packs/{pack_sha}.pack"),
                    bytes: 1_024,
                }],
            };
            write_chain(&store, Some("repo"), &ref_name, &chain)
                .await
                .unwrap();
            expected.insert(pack_sha40);
        }

        let referenced = list_referenced_packs(&store, "repo").await.unwrap();
        assert_eq!(referenced, expected);
    }

    #[tokio::test]
    async fn mark_fails_closed_on_corrupt_chain() {
        let store = MockStore::new();
        // chain.json with malformed JSON.
        store.insert(
            "repo/refs/heads/main/chain.json",
            Bytes::from_static(b"{not valid json"),
        );
        let err = mark(&store, "repo", MarkOpts::default()).await.unwrap_err();
        assert!(matches!(err, PackchainError::ParseJson(_)));
        // No tombstone written.
        let metas = store.list("repo/gc/").await.unwrap();
        assert!(metas.is_empty());
    }

    #[tokio::test]
    async fn mark_fails_closed_on_unsupported_schema_version() {
        let store = MockStore::new();
        store.insert(
            "repo/refs/heads/main/chain.json",
            Bytes::from_static(
                br#"{"v":2,"tip":"0000000000000000000000000000000000000001","full_at":"0000000000000000000000000000000000000002","segments":[]}"#,
            ),
        );
        let err = mark(&store, "repo", MarkOpts::default()).await.unwrap_err();
        assert!(matches!(
            err,
            PackchainError::UnsupportedSchemaVersion { .. }
        ));
    }

    // --- sweep ----------------------------------------------------------

    fn sha_set<I: IntoIterator<Item = &'static str>>(shas: I) -> Vec<Sha40> {
        shas.into_iter().map(sha40).collect()
    }

    fn write_tombstone(
        store: &MockStore,
        prefix: &str,
        marked_at: &str,
        shas: Vec<Sha40>,
    ) -> String {
        let run_id = Uuid::new_v4().to_string();
        let key = tombstone_key(prefix, &run_id, marked_at);
        let body = Tombstone {
            v: 1,
            run_id,
            marked_at: marked_at.to_string(),
            orphan_packs: shas,
        }
        .to_json_pretty()
        .unwrap();
        store.insert(&key, Bytes::from(body));
        key
    }

    #[tokio::test]
    async fn sweep_inside_grace_defers_tombstone() {
        let store = MockStore::new();
        let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
        let tombstone = write_tombstone(&store, "repo", &now, sha_set([SHA_PACK_ORPHAN]));
        insert_pack_pair(&store, Some("repo"), SHA_PACK_ORPHAN);

        let outcome = sweep(
            &store,
            "repo",
            SweepOpts {
                grace_hours: 24,
                force: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.deferred_tombstones, 1);
        assert_eq!(outcome.swept_tombstones, 0);
        // Tombstone and packs survive.
        store.get_bytes(&tombstone).await.unwrap();
        store
            .get_bytes(&format!("repo/packs/{SHA_PACK_ORPHAN}.pack"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn sweep_after_grace_deletes_orphan_packs_and_tombstone() {
        let store = MockStore::new();
        let stale = (OffsetDateTime::now_utc() - time::Duration::hours(48))
            .format(&Rfc3339)
            .unwrap();
        let tombstone = write_tombstone(&store, "repo", &stale, sha_set([SHA_PACK_ORPHAN]));
        insert_pack_pair(&store, Some("repo"), SHA_PACK_ORPHAN);

        let outcome = sweep(&store, "repo", SweepOpts::default()).await.unwrap();
        assert_eq!(outcome.swept_tombstones, 1);
        assert_eq!(outcome.deleted_objects, 2, "pack + idx");
        // Tombstone and packs gone.
        let pack_err = store
            .get_bytes(&format!("repo/packs/{SHA_PACK_ORPHAN}.pack"))
            .await
            .unwrap_err();
        assert!(matches!(pack_err, ObjectStoreError::NotFound(_)));
        let tomb_err = store.get_bytes(&tombstone).await.unwrap_err();
        assert!(matches!(tomb_err, ObjectStoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn sweep_skips_repointed_packs() {
        // A tombstoned pack got re-referenced by a chain rewrite
        // before the grace expired. Sweep must NOT delete it.
        let store = MockStore::new();
        let stale = (OffsetDateTime::now_utc() - time::Duration::hours(48))
            .format(&Rfc3339)
            .unwrap();
        // The tombstone names SHA_PACK_LIVE — but a chain now references it.
        write_tombstone(&store, "repo", &stale, sha_set([SHA_PACK_LIVE]));
        let chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_FULL),
            segments: vec![segment(SHA_PACK_LIVE, None)],
        };
        write_chain(&store, Some("repo"), &ref_main(), &chain)
            .await
            .unwrap();
        insert_pack_pair(&store, Some("repo"), SHA_PACK_LIVE);

        let outcome = sweep(&store, "repo", SweepOpts::default()).await.unwrap();
        assert_eq!(outcome.swept_tombstones, 1);
        assert_eq!(outcome.skipped_repointed_packs, 1);
        assert_eq!(outcome.deleted_objects, 0);
        // Pack still present.
        store
            .get_bytes(&format!("repo/packs/{SHA_PACK_LIVE}.pack"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn sweep_force_bypasses_grace_only_not_live_recheck() {
        // Regression for #117: --force must skip ONLY the grace window,
        // not the live-pack re-check. A fresh tombstone names a pack
        // that has since been referenced by a committed chain — the
        // classic outcome of mark() snapshotting between a concurrent
        // push's pack upload and its chain.json commit. Sweep with
        // --force must NOT delete that pack.
        let store = MockStore::new();
        let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
        write_tombstone(&store, "repo", &now, sha_set([SHA_PACK_LIVE]));
        let chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_FULL),
            segments: vec![segment(SHA_PACK_LIVE, None)],
        };
        write_chain(&store, Some("repo"), &ref_main(), &chain)
            .await
            .unwrap();
        insert_pack_pair(&store, Some("repo"), SHA_PACK_LIVE);

        let outcome = sweep(
            &store,
            "repo",
            SweepOpts {
                grace_hours: 24,
                force: true,
            },
        )
        .await
        .unwrap();
        // Grace was bypassed (fresh tombstone got processed instead of
        // deferred), but the live-pack guard fired and the pack stayed.
        assert_eq!(outcome.swept_tombstones, 1);
        assert_eq!(outcome.deferred_tombstones, 0);
        assert_eq!(outcome.skipped_repointed_packs, 1);
        assert_eq!(outcome.deleted_objects, 0);
        store
            .get_bytes(&format!("repo/packs/{SHA_PACK_LIVE}.pack"))
            .await
            .expect("live pack must survive --force sweep");
        store
            .get_bytes(&format!("repo/packs/{SHA_PACK_LIVE}.idx"))
            .await
            .expect("live idx must survive --force sweep");
    }

    #[tokio::test]
    async fn sweep_force_deletes_truly_orphan_pack_inside_grace() {
        // The happy path for --force: a fresh tombstone naming a pack
        // that is NOT in any chain. Grace is bypassed, the live-pack
        // re-check finds the SHA absent, the pack is deleted.
        let store = MockStore::new();
        let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
        write_tombstone(&store, "repo", &now, sha_set([SHA_PACK_ORPHAN]));
        insert_pack_pair(&store, Some("repo"), SHA_PACK_ORPHAN);

        let outcome = sweep(
            &store,
            "repo",
            SweepOpts {
                grace_hours: 24,
                force: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.swept_tombstones, 1);
        assert_eq!(outcome.deferred_tombstones, 0);
        assert_eq!(outcome.skipped_repointed_packs, 0);
        assert_eq!(outcome.deleted_objects, 2);
        let err = store
            .get_bytes(&format!("repo/packs/{SHA_PACK_ORPHAN}.pack"))
            .await
            .unwrap_err();
        assert!(matches!(err, ObjectStoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn sweep_tolerates_already_deleted_pack() {
        // Tombstone names a pack that no longer exists on the bucket
        // (e.g. a previous partial sweep deleted the .pack but
        // crashed before deleting the .idx). Sweep must complete
        // without error.
        let store = MockStore::new();
        let stale = (OffsetDateTime::now_utc() - time::Duration::hours(48))
            .format(&Rfc3339)
            .unwrap();
        write_tombstone(&store, "repo", &stale, sha_set([SHA_PACK_ORPHAN]));
        // No pack inserted.
        let outcome = sweep(&store, "repo", SweepOpts::default()).await.unwrap();
        assert_eq!(outcome.swept_tombstones, 1);
        assert_eq!(outcome.deleted_objects, 0);
    }

    #[tokio::test]
    async fn sweep_handles_multiple_tombstones_independently() {
        let store = MockStore::new();
        let stale = (OffsetDateTime::now_utc() - time::Duration::hours(48))
            .format(&Rfc3339)
            .unwrap();
        let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
        // One stale tombstone (must sweep) + one fresh (must defer).
        write_tombstone(&store, "repo", &stale, sha_set([SHA_PACK_ORPHAN]));
        write_tombstone(&store, "repo", &now, sha_set([SHA_PACK_ORPHAN_2]));
        insert_pack_pair(&store, Some("repo"), SHA_PACK_ORPHAN);
        insert_pack_pair(&store, Some("repo"), SHA_PACK_ORPHAN_2);

        let outcome = sweep(&store, "repo", SweepOpts::default()).await.unwrap();
        assert_eq!(outcome.swept_tombstones, 1);
        assert_eq!(outcome.deferred_tombstones, 1);
        assert_eq!(outcome.deleted_objects, 2);
    }

    // --- end-to-end ---------------------------------------------------

    #[tokio::test]
    async fn mark_then_force_sweep_round_trips() {
        let store = MockStore::new();
        seed_live_chain(&store, Some("repo")).await;
        insert_pack_pair(&store, Some("repo"), SHA_PACK_LIVE);
        insert_pack_pair(&store, Some("repo"), SHA_PACK_ORPHAN);

        let mark_out = mark(&store, "repo", MarkOpts::default()).await.unwrap();
        assert_eq!(mark_out.orphan_count, 1);

        // Force sweep — bypass grace.
        let sweep_out = sweep(
            &store,
            "repo",
            SweepOpts {
                grace_hours: 24,
                force: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(sweep_out.swept_tombstones, 1);
        assert_eq!(sweep_out.deleted_objects, 2);

        // Live pack survives, orphan pack is gone.
        store
            .get_bytes(&format!("repo/packs/{SHA_PACK_LIVE}.pack"))
            .await
            .unwrap();
        let err = store
            .get_bytes(&format!("repo/packs/{SHA_PACK_ORPHAN}.pack"))
            .await
            .unwrap_err();
        assert!(matches!(err, ObjectStoreError::NotFound(_)));
    }

    // --- baseline tombstones (issue #134) -----------------------------

    fn insert_baseline_bundle(store: &MockStore, prefix: Option<&str>, sha: &str) -> String {
        let key = keys::bundle_key(prefix, ref_main(), sha);
        store.insert(&key, Bytes::from_static(b"BUNDLE"));
        key
    }

    fn write_baseline_tombstone_at(
        store: &MockStore,
        prefix: &str,
        marked_at: &str,
        sha: &str,
    ) -> String {
        let key = baseline_tombstone_key(prefix, &Uuid::new_v4().to_string());
        let body = BaselineTombstone {
            v: TOMBSTONE_SCHEMA_VERSION,
            marked_at: marked_at.to_owned(),
            ref_name: ref_main().as_str().to_owned(),
            sha: sha40(sha),
        }
        .to_json_pretty()
        .unwrap();
        store.insert(&key, Bytes::from(body));
        key
    }

    #[tokio::test]
    async fn write_baseline_tombstone_round_trips() {
        // Writer + parser agree on the on-bucket shape. Regression
        // guard: a future serde tweak that broke the JSON layout would
        // make sweep silently skip every baseline tombstone.
        let store = MockStore::new();
        let prior = sha40(SHA_FULL);
        let current = sha40(SHA_TIP);
        write_baseline_tombstone(&store, Some("repo"), &ref_main(), &prior, &current)
            .await
            .unwrap();
        let metas = store.list("repo/gc/").await.unwrap();
        let tomb_key = metas
            .iter()
            .find(|m| m.key.starts_with("repo/gc/baseline-tomb-"))
            .map(|m| m.key.clone())
            .expect("baseline tombstone written");
        let body = store.get_bytes(&tomb_key).await.unwrap();
        let parsed = BaselineTombstone::from_json_bytes(&body).unwrap();
        assert_eq!(parsed.v, TOMBSTONE_SCHEMA_VERSION);
        assert_eq!(parsed.ref_name, "refs/heads/main");
        assert_eq!(parsed.sha, prior);
    }

    #[tokio::test]
    async fn write_baseline_tombstone_skips_when_prior_equals_current() {
        // No-op when the keys alias: a tombstone in this case would
        // later cause sweep to delete the live baseline bundle.
        let store = MockStore::new();
        let sha = sha40(SHA_FULL);
        write_baseline_tombstone(&store, Some("repo"), &ref_main(), &sha, &sha)
            .await
            .unwrap();
        let metas = store.list("repo/gc/").await.unwrap();
        assert!(
            metas.is_empty(),
            "aliasing prior/current must not write a tombstone",
        );
    }

    #[tokio::test]
    async fn sweep_defers_baseline_tombstone_within_grace_window() {
        // Issue #134: a fetch that started before compact must be able
        // to read the prior baseline within the grace window. Concrete
        // manifestation: a baseline tombstone marked "now" is left
        // alone, and the bundle it names stays on the bucket.
        let store = MockStore::new();
        let bundle_key = insert_baseline_bundle(&store, Some("repo"), SHA_FULL);
        let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
        let tomb_key = write_baseline_tombstone_at(&store, "repo", &now, SHA_FULL);

        let outcome = sweep(
            &store,
            "repo",
            SweepOpts {
                grace_hours: 24,
                force: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.deferred_tombstones, 1);
        assert_eq!(outcome.swept_tombstones, 0);
        assert_eq!(outcome.deleted_objects, 0);
        store
            .get_bytes(&bundle_key)
            .await
            .expect("bundle must survive sweep within grace");
        store
            .get_bytes(&tomb_key)
            .await
            .expect("tombstone must survive sweep within grace");
    }

    #[tokio::test]
    async fn sweep_reclaims_baseline_tombstone_after_grace_window() {
        // Issue #134: past the grace window, sweep deletes the bundle
        // and the tombstone. This is the path that reclaims the
        // orphan baseline left in place by compact / force-push.
        let store = MockStore::new();
        let bundle_key = insert_baseline_bundle(&store, Some("repo"), SHA_FULL);
        let stale = (OffsetDateTime::now_utc() - time::Duration::hours(48))
            .format(&Rfc3339)
            .unwrap();
        let tomb_key = write_baseline_tombstone_at(&store, "repo", &stale, SHA_FULL);

        let outcome = sweep(&store, "repo", SweepOpts::default()).await.unwrap();
        assert_eq!(outcome.swept_tombstones, 1);
        assert_eq!(outcome.deferred_tombstones, 0);
        assert_eq!(outcome.deleted_objects, 1, "bundle delete");
        let bundle_err = store.get_bytes(&bundle_key).await.unwrap_err();
        assert!(matches!(bundle_err, ObjectStoreError::NotFound(_)));
        let tomb_err = store.get_bytes(&tomb_key).await.unwrap_err();
        assert!(matches!(tomb_err, ObjectStoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn sweep_skips_re_baselined_bundle_after_grace() {
        // A later push re-baselined to the SAME SHA the tombstone names
        // (force-push at the same tip, or compact short-cut). Sweep
        // must NOT delete the bundle — it is live again. The
        // now-stale tombstone is dropped.
        let store = MockStore::new();
        let bundle_key = insert_baseline_bundle(&store, Some("repo"), SHA_FULL);
        // Live chain points at the same SHA the tombstone names.
        let chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_FULL),
            segments: vec![segment(SHA_PACK_LIVE, None)],
        };
        write_chain(&store, Some("repo"), &ref_main(), &chain)
            .await
            .unwrap();
        let stale = (OffsetDateTime::now_utc() - time::Duration::hours(48))
            .format(&Rfc3339)
            .unwrap();
        let tomb_key = write_baseline_tombstone_at(&store, "repo", &stale, SHA_FULL);

        let outcome = sweep(&store, "repo", SweepOpts::default()).await.unwrap();
        assert_eq!(outcome.swept_tombstones, 1);
        assert_eq!(outcome.skipped_repointed_packs, 1);
        assert_eq!(outcome.deleted_objects, 0);
        store
            .get_bytes(&bundle_key)
            .await
            .expect("re-baselined bundle must survive");
        let tomb_err = store.get_bytes(&tomb_key).await.unwrap_err();
        assert!(matches!(tomb_err, ObjectStoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn sweep_baseline_tolerates_already_deleted_bundle() {
        // The bundle was deleted out of band (operator cleanup, or a
        // ref deletion that happened to sweep it). Sweep must finish
        // cleanly.
        let store = MockStore::new();
        let stale = (OffsetDateTime::now_utc() - time::Duration::hours(48))
            .format(&Rfc3339)
            .unwrap();
        let tomb_key = write_baseline_tombstone_at(&store, "repo", &stale, SHA_FULL);
        // No bundle inserted.

        let outcome = sweep(&store, "repo", SweepOpts::default()).await.unwrap();
        assert_eq!(outcome.swept_tombstones, 1);
        assert_eq!(outcome.deleted_objects, 0);
        let tomb_err = store.get_bytes(&tomb_key).await.unwrap_err();
        assert!(matches!(tomb_err, ObjectStoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn sweep_baseline_force_bypasses_grace_only_not_live_recheck() {
        // --force on a fresh baseline tombstone whose SHA is now live
        // (re-baselined). Grace is bypassed (tombstone is processed),
        // but the live-state guard fires and the bundle stays.
        let store = MockStore::new();
        let bundle_key = insert_baseline_bundle(&store, Some("repo"), SHA_FULL);
        let chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_FULL),
            segments: vec![segment(SHA_PACK_LIVE, None)],
        };
        write_chain(&store, Some("repo"), &ref_main(), &chain)
            .await
            .unwrap();
        let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
        write_baseline_tombstone_at(&store, "repo", &now, SHA_FULL);

        let outcome = sweep(
            &store,
            "repo",
            SweepOpts {
                grace_hours: 24,
                force: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.swept_tombstones, 1);
        assert_eq!(outcome.deferred_tombstones, 0);
        assert_eq!(outcome.skipped_repointed_packs, 1);
        assert_eq!(outcome.deleted_objects, 0);
        store
            .get_bytes(&bundle_key)
            .await
            .expect("live bundle must survive --force sweep");
    }

    #[tokio::test]
    async fn sweep_processes_pack_and_baseline_tombstones_in_one_pass() {
        // Mixed tombstone types under `<prefix>/gc/`. Sweep must
        // dispatch each to the right handler without mis-counting or
        // skipping.
        let store = MockStore::new();
        let bundle_key = insert_baseline_bundle(&store, Some("repo"), SHA_FULL);
        insert_pack_pair(&store, Some("repo"), SHA_PACK_ORPHAN);
        let stale = (OffsetDateTime::now_utc() - time::Duration::hours(48))
            .format(&Rfc3339)
            .unwrap();
        write_tombstone(&store, "repo", &stale, sha_set([SHA_PACK_ORPHAN]));
        write_baseline_tombstone_at(&store, "repo", &stale, SHA_FULL);

        let outcome = sweep(&store, "repo", SweepOpts::default()).await.unwrap();
        assert_eq!(outcome.swept_tombstones, 2);
        // pack + idx + bundle = 3 deletions
        assert_eq!(outcome.deleted_objects, 3);
        let bundle_err = store.get_bytes(&bundle_key).await.unwrap_err();
        assert!(matches!(bundle_err, ObjectStoreError::NotFound(_)));
        let pack_err = store
            .get_bytes(&format!("repo/packs/{SHA_PACK_ORPHAN}.pack"))
            .await
            .unwrap_err();
        assert!(matches!(pack_err, ObjectStoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn compact_to_sweep_round_trip_simulates_concurrent_fetch_then_gc() {
        // End-to-end issue #134 scenario: compact writes a tombstone
        // (we simulate by hand to avoid pulling in the full compact
        // fixture), an in-flight fetch reads the prior bundle within
        // grace and succeeds, and a later sweep past the grace
        // reclaims it.
        let store = MockStore::new();
        let bundle_key = insert_baseline_bundle(&store, Some("repo"), SHA_FULL);
        // Compact moved the baseline to a new SHA — simulate by
        // writing a chain pointing to SHA_TIP as full_at.
        let chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_TIP),
            segments: vec![segment(SHA_PACK_LIVE, None)],
        };
        write_chain(&store, Some("repo"), &ref_main(), &chain)
            .await
            .unwrap();
        let prior = sha40(SHA_FULL);
        let current = sha40(SHA_TIP);
        write_baseline_tombstone(&store, Some("repo"), &ref_main(), &prior, &current)
            .await
            .unwrap();

        // In-flight fetch: bundle GET within grace MUST succeed.
        let body = store.get_bytes(&bundle_key).await.unwrap();
        assert_eq!(&body[..], b"BUNDLE");
        let in_grace = sweep(
            &store,
            "repo",
            SweepOpts {
                grace_hours: 24,
                force: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(in_grace.deferred_tombstones, 1);
        store
            .get_bytes(&bundle_key)
            .await
            .expect("bundle must survive in-grace sweep");

        // Backdate the tombstone past the grace and re-sweep —
        // bundle is reaped.
        let metas = store.list("repo/gc/").await.unwrap();
        let tomb_key = metas
            .iter()
            .find(|m| m.key.starts_with("repo/gc/baseline-tomb-"))
            .map(|m| m.key.clone())
            .unwrap();
        let stale = (OffsetDateTime::now_utc() - time::Duration::hours(48))
            .format(&Rfc3339)
            .unwrap();
        let body = store.get_bytes(&tomb_key).await.unwrap();
        let mut tomb: BaselineTombstone = serde_json::from_slice(&body).unwrap();
        tomb.marked_at = stale;
        let new_body = serde_json::to_vec_pretty(&tomb).unwrap();
        store.insert(&tomb_key, Bytes::from(new_body));

        let post_grace = sweep(&store, "repo", SweepOpts::default()).await.unwrap();
        assert_eq!(post_grace.swept_tombstones, 1);
        assert_eq!(post_grace.deleted_objects, 1);
        let err = store.get_bytes(&bundle_key).await.unwrap_err();
        assert!(matches!(err, ObjectStoreError::NotFound(_)));
    }

    // --- per-tombstone live-pack recompute (issue #140) --------------

    /// One-shot post-delete hook used by [`PostDeleteHookStore`].
    type PostDeleteHook = Box<dyn FnOnce(&MockStore) + Send>;

    /// Test-only [`ObjectStore`] decorator that runs a one-shot
    /// callback the first time `delete()` succeeds on a key matching
    /// `trigger_prefix`, *after* the inner delete completes. Used to
    /// inject a concurrent push (writing a fresh `chain.json`) between
    /// successive `sweep_one_tombstone` iterations and verify that the
    /// per-tombstone live-pack recompute picks it up.
    ///
    /// Every other trait method forwards to the inner store unchanged.
    struct PostDeleteHookStore {
        inner: MockStore,
        hook: std::sync::Mutex<Option<PostDeleteHook>>,
        /// Key-prefix the hook fires on. The pack-tombstone case
        /// uses `<prefix>/gc/tombstones-`; the test never deletes
        /// other keys before the intended trigger so this stays
        /// unambiguous.
        trigger_prefix: String,
    }

    impl PostDeleteHookStore {
        fn new(
            inner: MockStore,
            trigger_prefix: impl Into<String>,
            hook: impl FnOnce(&MockStore) + Send + 'static,
        ) -> Self {
            Self {
                inner,
                hook: std::sync::Mutex::new(Some(Box::new(hook))),
                trigger_prefix: trigger_prefix.into(),
            }
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for PostDeleteHookStore {
        async fn list(
            &self,
            prefix: &str,
        ) -> Result<Vec<crate::object_store::ObjectMeta>, ObjectStoreError> {
            self.inner.list(prefix).await
        }

        async fn get_to_file(
            &self,
            key: &str,
            dest: &std::path::Path,
            opts: crate::object_store::GetOpts,
        ) -> Result<(), ObjectStoreError> {
            self.inner.get_to_file(key, dest, opts).await
        }

        async fn get_bytes(&self, key: &str) -> Result<Bytes, ObjectStoreError> {
            self.inner.get_bytes(key).await
        }

        async fn get_bytes_range(
            &self,
            key: &str,
            range: std::ops::Range<u64>,
        ) -> Result<Bytes, ObjectStoreError> {
            self.inner.get_bytes_range(key, range).await
        }

        async fn put_bytes(
            &self,
            key: &str,
            body: Bytes,
            opts: PutOpts,
        ) -> Result<(), ObjectStoreError> {
            self.inner.put_bytes(key, body, opts).await
        }

        async fn put_path(
            &self,
            key: &str,
            src: &std::path::Path,
            opts: PutOpts,
        ) -> Result<(), ObjectStoreError> {
            self.inner.put_path(key, src, opts).await
        }

        async fn put_if_absent(&self, key: &str, body: Bytes) -> Result<bool, ObjectStoreError> {
            self.inner.put_if_absent(key, body).await
        }

        async fn head(
            &self,
            key: &str,
        ) -> Result<crate::object_store::ObjectMeta, ObjectStoreError> {
            self.inner.head(key).await
        }

        async fn copy(&self, src: &str, dst: &str) -> Result<(), ObjectStoreError> {
            self.inner.copy(src, dst).await
        }

        async fn delete(&self, key: &str) -> Result<(), ObjectStoreError> {
            let result = self.inner.delete(key).await;
            if result.is_ok()
                && key.starts_with(&self.trigger_prefix)
                && let Some(hook) = self.hook.lock().unwrap().take()
            {
                hook(&self.inner);
            }
            result
        }
    }

    #[tokio::test]
    async fn sweep_re_derives_referenced_set_per_tombstone() {
        // Issue #140 regression: a concurrent push committing
        // chain.json between two `sweep_one_tombstone` iterations
        // must not let sweep delete a pack the new chain references.
        //
        // Layout: two stale tombstones, each naming a distinct pack
        // on its own ref. After the FIRST tombstone is fully
        // processed and deleted, the post-delete hook fires and
        // writes BOTH refs' `chain.json` files — simulating a
        // concurrent push that committed chain.json for the second
        // ref between sweep's two iterations. The second iteration
        // must re-derive the live set and skip the delete.
        //
        // Pre-fix: the once-per-sweep snapshot is empty for both
        // iterations and BOTH packs are deleted (`deleted_objects = 4`).
        // Post-fix: the second iteration's recompute picks up the new
        // chain and the second pack survives
        // (`deleted_objects = 2`, `skipped_repointed_packs = 1`).
        //
        // The hook writes chains for both refs (rather than guessing
        // which tombstone runs first) so the assertions are independent
        // of MockStore iteration order. Writing the first ref's chain
        // is a no-op for that pack — its delete already happened
        // before the hook fired — and the second ref's chain is what
        // protects the still-pending pack.
        let inner = MockStore::new();
        let stale_a = (OffsetDateTime::now_utc() - time::Duration::hours(49))
            .format(&Rfc3339)
            .unwrap();
        let stale_b = (OffsetDateTime::now_utc() - time::Duration::hours(48))
            .format(&Rfc3339)
            .unwrap();
        write_tombstone(&inner, "repo", &stale_a, sha_set([SHA_PACK_ORPHAN]));
        write_tombstone(&inner, "repo", &stale_b, sha_set([SHA_PACK_ORPHAN_2]));
        insert_pack_pair(&inner, Some("repo"), SHA_PACK_ORPHAN);
        insert_pack_pair(&inner, Some("repo"), SHA_PACK_ORPHAN_2);

        // After the FIRST tombstone delete completes, simulate the
        // concurrent push by committing chain.json files for both
        // refs at once.
        let store = PostDeleteHookStore::new(inner, "repo/gc/tombstones-", |inner| {
            for (ref_path, pack_sha) in [
                ("repo/refs/heads/branch_a/chain.json", SHA_PACK_ORPHAN),
                ("repo/refs/heads/branch_b/chain.json", SHA_PACK_ORPHAN_2),
            ] {
                let chain = ChainManifest {
                    v: 1,
                    tip: sha40(SHA_TIP),
                    full_at: sha40(SHA_FULL),
                    segments: vec![segment(pack_sha, None)],
                };
                let body =
                    serde_json::to_vec_pretty(&chain).expect("chain.json serializes for the test");
                inner.insert(ref_path, Bytes::from(body));
            }
        });

        let outcome = sweep(&store, "repo", SweepOpts::default()).await.unwrap();
        // Both tombstones processed.
        assert_eq!(outcome.swept_tombstones, 2);
        // Whichever tombstone ran first deleted its pack pair (2
        // objects). The second iteration's recompute saw the
        // freshly-committed chain and skipped the delete.
        assert_eq!(outcome.deleted_objects, 2);
        assert_eq!(outcome.skipped_repointed_packs, 1);

        // Exactly one of the two packs survives — the one whose
        // tombstone was processed second.
        let first_survives = store
            .inner
            .get_bytes(&format!("repo/packs/{SHA_PACK_ORPHAN}.pack"))
            .await
            .is_ok();
        let second_survives = store
            .inner
            .get_bytes(&format!("repo/packs/{SHA_PACK_ORPHAN_2}.pack"))
            .await
            .is_ok();
        assert!(
            first_survives ^ second_survives,
            "exactly one pack must survive: \
             first_survives={first_survives}, second_survives={second_survives}",
        );
    }

    #[tokio::test]
    async fn sweep_reclaims_genuinely_orphan_pack_with_per_tombstone_recompute() {
        // Sanity: the per-tombstone recompute does NOT regress the
        // normal sweep path. A stale tombstone naming a pack with no
        // chain reference is reclaimed exactly as before.
        let store = MockStore::new();
        let stale = (OffsetDateTime::now_utc() - time::Duration::hours(48))
            .format(&Rfc3339)
            .unwrap();
        write_tombstone(&store, "repo", &stale, sha_set([SHA_PACK_ORPHAN]));
        insert_pack_pair(&store, Some("repo"), SHA_PACK_ORPHAN);
        // No chain.json at all: referenced set is empty for every
        // recompute pass.

        let outcome = sweep(&store, "repo", SweepOpts::default()).await.unwrap();
        assert_eq!(outcome.swept_tombstones, 1);
        assert_eq!(outcome.deleted_objects, 2);
        assert_eq!(outcome.skipped_repointed_packs, 0);
    }

    #[tokio::test]
    async fn sweep_protects_pack_when_concurrent_push_aliases_existing_key() {
        // Issue #140's canonical scenario, framed as the issue
        // describes it: a force-revert republishes a pack with the
        // SAME content SHA as the tombstoned pack (deterministic gix
        // pack emission). The concurrent push only updates
        // chain.json; the pack key is reused. Sweep must observe
        // the new chain reference and leave the pack alone.
        //
        // Modelled at the post-fix invariant level: the chain
        // referencing the tombstoned SHA exists when
        // `sweep_one_tombstone` runs its recompute, and the pack is
        // preserved with `skipped_repointed_packs += 1`.
        let store = MockStore::new();
        let stale = (OffsetDateTime::now_utc() - time::Duration::hours(48))
            .format(&Rfc3339)
            .unwrap();
        write_tombstone(&store, "repo", &stale, sha_set([SHA_PACK_LIVE]));
        // Insert the pack, then commit chain.json referencing it —
        // identical-content SHA path through the engine ends here.
        insert_pack_pair(&store, Some("repo"), SHA_PACK_LIVE);
        let chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_FULL),
            segments: vec![segment(SHA_PACK_LIVE, None)],
        };
        write_chain(&store, Some("repo"), &ref_main(), &chain)
            .await
            .unwrap();

        let outcome = sweep(&store, "repo", SweepOpts::default()).await.unwrap();
        assert_eq!(outcome.swept_tombstones, 1);
        assert_eq!(outcome.skipped_repointed_packs, 1);
        assert_eq!(outcome.deleted_objects, 0);
        store
            .get_bytes(&format!("repo/packs/{SHA_PACK_LIVE}.pack"))
            .await
            .expect("aliased pack must survive sweep");
    }

    #[tokio::test]
    async fn grace_hours_env_override_falls_back_for_unset_or_invalid() {
        // Unset returns default.
        unsafe {
            std::env::remove_var(ENV_GC_GRACE_HOURS);
        }
        assert_eq!(grace_hours_from_env(), DEFAULT_GRACE_HOURS);
        // Non-numeric falls back.
        unsafe {
            std::env::set_var(ENV_GC_GRACE_HOURS, "not-a-number");
        }
        assert_eq!(grace_hours_from_env(), DEFAULT_GRACE_HOURS);
        // Zero falls back (would defeat the design).
        unsafe {
            std::env::set_var(ENV_GC_GRACE_HOURS, "0");
        }
        assert_eq!(grace_hours_from_env(), DEFAULT_GRACE_HOURS);
        // Positive integer wins.
        unsafe {
            std::env::set_var(ENV_GC_GRACE_HOURS, "72");
        }
        assert_eq!(grace_hours_from_env(), 72);
        unsafe {
            std::env::remove_var(ENV_GC_GRACE_HOURS);
        }
    }
}

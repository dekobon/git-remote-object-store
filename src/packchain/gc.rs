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
//! 1. List `<prefix>/refs/heads/*/chain.json`, parse each, collect
//!    referenced pack content-shas.
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
//!    - Re-derive the orphan set from the *current* chain state. A
//!      pack tombstoned by an earlier mark may have become re-referenced
//!      (engine bug edge case, cheap to verify).
//!    - For each pack still orphan, delete `.pack` + `.idx`
//!      idempotently (a prior partial sweep is fine).
//!    - Delete the tombstone itself.
//! 3. Younger tombstones survive for the next sweep.
//!
//! ### `--force`
//!
//! Skips both grace and re-check. Operator-asserted safe. A `tracing::warn!`
//! line records the choice.
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
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::keys;
use crate::object_store::{ObjectStore, ObjectStoreError, PutOpts};

use super::PackchainError;
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
    /// When `true`, skip both the grace check AND the re-derive of
    /// the orphan set. Operator-asserted safe (no concurrent reads).
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
    let now = OffsetDateTime::now_utc();
    let marked_at = now.format(&Rfc3339).map_err(|e| {
        PackchainError::Io(std::io::Error::other(format!("rfc3339 format failed: {e}")))
    })?;
    let tombstone_key = tombstone_key(prefix, &run_id, &marked_at);
    let tombstone = Tombstone {
        v: TOMBSTONE_SCHEMA_VERSION,
        run_id: run_id.clone(),
        marked_at,
        orphan_packs: orphans.clone(),
    };

    let outcome = MarkOutcome {
        run_id: run_id.clone(),
        orphan_count: orphans.len(),
        tombstone_key: tombstone_key.clone(),
    };

    if opts.dry_run {
        debug!(
            run_id = %run_id,
            orphans = orphans.len(),
            "gc mark: dry-run, not writing tombstone",
        );
        return Ok(outcome);
    }

    if orphans.is_empty() {
        info!(run_id = %run_id, "gc mark: no orphans; skipping tombstone");
        return Ok(outcome);
    }

    let body = Bytes::from(tombstone.to_json_pretty()?);
    store
        .put_bytes(&tombstone_key, body, PutOpts::default())
        .await?;
    info!(
        run_id = %run_id,
        orphans = orphans.len(),
        key = %tombstone_key,
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
        warn!("gc sweep: --force in effect; skipping grace and re-check");
    }

    // Re-derive the live referenced set once per sweep, not per
    // tombstone — sweeping many tombstones at once is the dominant
    // case and the chain set doesn't change between iterations of
    // the same `sweep` call.
    let referenced = if opts.force {
        HashSet::new()
    } else {
        list_referenced_packs(store, prefix).await?
    };

    for meta in metas {
        if !meta.key.as_bytes().ends_with(b".json") || !is_tombstone_key(&meta.key, prefix) {
            continue;
        }
        match sweep_one_tombstone(store, prefix, &meta.key, &referenced, opts).await {
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
    referenced: &HashSet<Sha40>,
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

    let mut deleted_objects = 0usize;
    let mut skipped_repointed_packs = 0usize;
    for sha in &tombstone.orphan_packs {
        if !opts.force && referenced.contains(sha) {
            skipped_repointed_packs += 1;
            debug!(
                sha = %sha.as_str(),
                "gc sweep: tombstoned pack re-referenced; skipping",
            );
            continue;
        }
        let pack_key = super::keys::pack_key(super::keys::optional_prefix(prefix), sha);
        let idx_key = super::keys::pack_idx_key(super::keys::optional_prefix(prefix), sha);
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

/// `<prefix>/gc/` prefix for [`ObjectStore::list`]. Empty `prefix`
/// drops the leading slash (matches the project's bucket-root rule).
fn gc_listing_prefix(prefix: &str) -> String {
    keys::join(prefix, "gc/")
}

/// Build a tombstone key. The `marked_at` segment may contain `:`
/// characters; S3 / Azure both accept colons in keys.
fn tombstone_key(prefix: &str, run_id: &str, marked_at: &str) -> String {
    keys::join(prefix, &format!("gc/tombstones-{run_id}-{marked_at}.json"))
}

/// Robust check that `key` is a tombstone under our prefix. Guards
/// against unrelated `.json` files in `<prefix>/gc/` and against a
/// regression where a future schema rev moves the prefix.
fn is_tombstone_key(key: &str, prefix: &str) -> bool {
    let expected_prefix = keys::join(prefix, "gc/tombstones-");
    key.starts_with(&expected_prefix)
}

/// List every `<prefix>/refs/heads/*/chain.json` and union the pack
/// content-shas they reference. Fail closed on parse error.
async fn list_referenced_packs(
    store: &dyn ObjectStore,
    prefix: &str,
) -> Result<HashSet<Sha40>, PackchainError> {
    let refs_prefix = keys::join(prefix, "refs/heads/");
    let metas = store.list(&refs_prefix).await?;
    let mut referenced: HashSet<Sha40> = HashSet::new();
    for meta in metas {
        if !super::keys::is_chain_json_key(&meta.key) {
            continue;
        }
        let body = store.get_bytes(&meta.key).await?;
        let chain = ChainManifest::from_json_bytes(&body)?;
        for segment in chain.segments {
            // gc fails closed on a malformed pack key (vs read.rs's
            // MalformedPackEntry path) — the chain is corrupt and
            // tombstoning live packs based on it would be unsafe.
            let sha = super::keys::parse_pack_key_sha(&segment.pack).ok_or_else(|| {
                PackchainError::ParseJson(serde_json::Error::custom(format!(
                    "chain segment pack key `{}` lacks `.pack` suffix",
                    segment.pack,
                )))
            })?;
            referenced.insert(sha);
        }
    }
    Ok(referenced)
}

/// List every `<prefix>/packs/*.pack` and `*.idx` and return the union
/// of their content-shas. The set is keyed by sha so a pack with a
/// missing-but-tombstoneable idx still counts (and vice versa).
async fn list_pack_shas(
    store: &dyn ObjectStore,
    prefix: &str,
) -> Result<HashSet<Sha40>, PackchainError> {
    let packs_prefix = keys::join(prefix, "packs/");
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

/// `serde_json::Error` constructor — extension trait so we can produce
/// custom-message errors that thread through `PackchainError::ParseJson`.
trait JsonErrorCustom {
    fn custom(msg: String) -> serde_json::Error;
}
impl JsonErrorCustom for serde_json::Error {
    fn custom(msg: String) -> serde_json::Error {
        // Round-trip through serde_json::from_str to manufacture an
        // `Error` with our message — `serde_json` does not expose a
        // public `Error::custom` constructor.
        serde::de::Error::custom(msg)
    }
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
    async fn sweep_force_bypasses_grace_and_recheck() {
        let store = MockStore::new();
        // Fresh tombstone (well within grace) but force ignores it.
        let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
        write_tombstone(&store, "repo", &now, sha_set([SHA_PACK_LIVE]));
        // Live chain references SHA_PACK_LIVE — force ignores re-check.
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
        assert_eq!(outcome.swept_tombstones, 1);
        assert_eq!(outcome.deleted_objects, 2);
        let err = store
            .get_bytes(&format!("repo/packs/{SHA_PACK_LIVE}.pack"))
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

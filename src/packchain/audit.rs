//! Read-only diagnostics for packchain buckets (issue #68).
//!
//! [`audit`] is the data-only counterpart to `gc::mark` / `gc::sweep` and
//! the runtime engine paths: it reports orphan packs, pending tombstones,
//! per-branch compaction candidates, and dangling chain references
//! without acting. The management `doctor` subcommand renders the
//! returned [`AuditReport`].
//!
//! The threshold constants ([`COMPACT_SEGMENTS_THRESHOLD`],
//! [`COMPACT_BYTES_THRESHOLD`]) are exposed so a future `compact`
//! subcommand applies the same heuristic the doctor recommends.

use std::collections::{HashMap, HashSet};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::warn;

use crate::git::RefName;
use crate::keys;
use crate::object_store::{ObjectMeta, ObjectStore};

use super::PackchainError;
use super::gc::Tombstone;
use super::keys::{is_chain_json_key, ref_path_from_chain_key, sha_from_pack_key};
use super::schema::{ChainManifest, Sha40};

/// Segment-count threshold above which a branch is flagged as a
/// compaction candidate. Mirrors the heuristic specified in #67 / #68.
pub(crate) const COMPACT_SEGMENTS_THRESHOLD: usize = 20;

/// Bytes-since-`full_at` threshold above which a branch is flagged as
/// a compaction candidate. Default: 100 MiB.
pub(crate) const COMPACT_BYTES_THRESHOLD: u64 = 100 * 1_024 * 1_024;

/// Aggregate output of [`audit`]. Each field is independently reportable
/// — an empty `Vec` (or zero count) means "nothing to report" rather
/// than "audit failed".
#[derive(Debug, Clone, Default)]
pub(crate) struct AuditReport {
    /// Pack files in `<prefix>/packs/` that no live chain.json
    /// references.
    pub(crate) orphans: OrphanSummary,
    /// Tombstones currently sitting in `<prefix>/gc/`, sorted oldest
    /// first.
    pub(crate) tombstones: Vec<TombstoneRow>,
    /// Per-branch row, sorted by ref path.
    pub(crate) branches: Vec<BranchRow>,
    /// chain.json segment-pack references that point at pack keys
    /// missing from the bucket. Sorted by ref path.
    pub(crate) dangling: Vec<DanglingRow>,
}

/// Orphan-pack summary. `pack_count` counts unique content-shas;
/// `bytes` sums the on-bucket size of each orphan `.pack` file
/// (the matching `.idx` is excluded so the total reflects
/// recoverable storage rather than raw key count).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OrphanSummary {
    /// Number of distinct orphan content-shas.
    pub(crate) pack_count: usize,
    /// Total bytes occupied by orphan `.pack` files.
    pub(crate) bytes: u64,
}

/// One pending tombstone awaiting sweep.
#[derive(Debug, Clone)]
pub(crate) struct TombstoneRow {
    /// `UUIDv4` run id from the tombstone body.
    pub(crate) run_id: String,
    /// RFC 3339 timestamp from the tombstone body.
    pub(crate) marked_at: String,
    /// Whole hours since `marked_at` (negative when the tombstone's
    /// timestamp is in the future, e.g. operator clock skew).
    pub(crate) age_hours: i64,
    /// Number of orphan packs the tombstone names.
    pub(crate) orphan_count: usize,
}

/// Per-branch chain summary used to recommend (or not) a compact run.
///
/// In a healthy chain, the segments slice covers everything since the
/// last baseline bundle — that is, "since `full_at`". Older history
/// lives in the baseline and never appears in `segments`. The fields
/// below therefore reflect both "total" and "since `full_at`"; the
/// distinction only matters in a corrupted chain whose `full_at` does
/// not match any segment's `sha`.
#[derive(Debug, Clone)]
pub(crate) struct BranchRow {
    /// Full ref path (e.g. `refs/heads/main`).
    pub(crate) ref_path: String,
    /// `chain.segments.len()`.
    pub(crate) segments_total: usize,
    /// Sum of `segment.bytes` over `chain.segments`.
    pub(crate) bytes_total: u64,
    /// `true` when either threshold is exceeded.
    pub(crate) recommend_compact: bool,
    /// `true` when `chain.full_at` is present as a segment's `sha`
    /// (the healthy state). `false` flags a corrupted manifest; the
    /// totals above are still computed but the doctor surfaces an
    /// ERROR row.
    pub(crate) has_full_at_segment: bool,
}

/// One chain.json segment that points at a pack key missing from the
/// bucket. Distinct from an orphan: an orphan exists on the bucket
/// without a chain reference; a dangling reference is a chain
/// pointing at a pack that has been deleted.
#[derive(Debug, Clone)]
pub(crate) struct DanglingRow {
    /// Ref whose chain.json references the missing pack.
    pub(crate) ref_path: String,
    /// Pack key the chain.json segment names.
    pub(crate) missing_pack_key: String,
}

/// Walk the supplied object listing and produce an [`AuditReport`].
///
/// `objects` must be a listing that covers everything under
/// `<prefix>/` — typically the same bucket-wide list the doctor
/// already performs for snapshot analysis. Audit filters this
/// listing for the three subsets it cares about (chain.json,
/// `packs/*.pack`, tombstones) so a single network list serves
/// both the bundle-shape report and the packchain audit.
///
/// `store` is used only for `get_bytes` calls on chain.json and
/// tombstone bodies. Per-entry parse failures are logged at `warn`
/// and the entry is skipped rather than aborting the audit —
/// `doctor` is read-only and a corrupt artefact on one branch
/// shouldn't blackhole the rest of the report.
///
/// # Errors
///
/// Returns [`PackchainError::Store`] only for fatal transport
/// errors during artefact body fetches that survive the per-entry
/// warn-and-skip filter (currently none — every body fetch warns
/// and skips on its own).
pub(crate) async fn audit(
    store: &dyn ObjectStore,
    prefix: &str,
    objects: &[ObjectMeta],
) -> Result<AuditReport, PackchainError> {
    let chains = load_chains(store, prefix, objects).await?;
    let pack_metas = pack_metas_from_objects(prefix, objects);
    let tombstones = load_tombstones(store, prefix, objects).await?;

    let referenced: HashSet<Sha40> = chains
        .iter()
        .flat_map(|(_, chain)| chain.segments.iter())
        .filter_map(|s| sha_from_pack_key(&s.pack))
        .collect();

    let orphans = pack_metas
        .iter()
        .filter(|(sha, _)| !referenced.contains(sha))
        .fold(OrphanSummary::default(), |mut acc, (_, meta)| {
            acc.pack_count += 1;
            acc.bytes = acc.bytes.saturating_add(meta.size);
            acc
        });

    let mut branches: Vec<BranchRow> = chains
        .iter()
        .map(|(ref_path, chain)| audit_branch(ref_path, chain))
        .collect();
    branches.sort_by(|a, b| a.ref_path.cmp(&b.ref_path));

    let mut dangling: Vec<DanglingRow> = chains
        .iter()
        .flat_map(|(ref_path, chain)| {
            chain
                .segments
                .iter()
                .filter(|s| !pack_present(&s.pack, &pack_metas))
                .map(move |s| DanglingRow {
                    ref_path: ref_path.clone(),
                    missing_pack_key: s.pack.clone(),
                })
        })
        .collect();
    dangling.sort_by(|a, b| {
        a.ref_path
            .cmp(&b.ref_path)
            .then_with(|| a.missing_pack_key.cmp(&b.missing_pack_key))
    });

    Ok(AuditReport {
        orphans,
        tombstones,
        branches,
        dangling,
    })
}

/// Parse one branch's chain into a [`BranchRow`].
fn audit_branch(ref_path: &str, chain: &ChainManifest) -> BranchRow {
    let segments_total = chain.segments.len();
    let bytes_total = chain
        .segments
        .iter()
        .map(|s| s.bytes)
        .fold(0u64, u64::saturating_add);
    let recommend_compact =
        segments_total > COMPACT_SEGMENTS_THRESHOLD || bytes_total > COMPACT_BYTES_THRESHOLD;
    let has_full_at_segment = chain.segments.iter().any(|s| s.sha == chain.full_at);
    BranchRow {
        ref_path: ref_path.to_owned(),
        segments_total,
        bytes_total,
        recommend_compact,
        has_full_at_segment,
    }
}

/// Resolve a chain segment's `pack` field to its content sha and
/// check presence against the on-bucket pack set. Membership is
/// keyed by the parsed [`Sha40`], not by the full bucket key, so
/// each segment lookup is a single hash probe with no allocation.
fn pack_present(pack_field: &str, pack_metas: &HashMap<Sha40, ObjectMeta>) -> bool {
    sha_from_pack_key(pack_field).is_some_and(|sha| pack_metas.contains_key(&sha))
}

/// Filter the supplied listing for chain.json keys, fetch each
/// body, parse, and return per-branch chain manifests. Per-entry
/// parse failures warn and skip.
async fn load_chains(
    store: &dyn ObjectStore,
    prefix: &str,
    objects: &[ObjectMeta],
) -> Result<Vec<(String, ChainManifest)>, PackchainError> {
    let mut out: Vec<(String, ChainManifest)> = Vec::new();
    for meta in objects.iter().filter(|m| is_chain_json_key(&m.key)) {
        let Some(ref_path) = ref_path_from_chain_key(Some(prefix), &meta.key) else {
            warn!(key = %meta.key, "audit: chain.json key has unexpected shape; skipping");
            continue;
        };
        // Mirror `list_refs`'s defense-in-depth: a maliciously-planted
        // key like `<prefix>/refs/heads/../etc/passwd/chain.json`
        // would otherwise render its derived path verbatim into
        // doctor's stdout.
        if !RefName::is_valid(&ref_path) {
            warn!(
                key = %meta.key,
                ref_path = %ref_path,
                "audit: derived ref path is not a valid ref name; skipping",
            );
            continue;
        }
        // Per-entry transport failures warn-and-skip rather than
        // aborting: doctor is a read-only diagnostic surface and a
        // single transient 503 on one branch should not blackhole the
        // rest of the report.
        let body = match store.get_bytes(&meta.key).await {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    key = %meta.key,
                    error = %e,
                    "audit: chain.json fetch failed; skipping ref",
                );
                continue;
            }
        };
        match ChainManifest::from_json_bytes(&body) {
            Ok(chain) => out.push((ref_path, chain)),
            Err(e) => warn!(
                key = %meta.key,
                error = %e,
                "audit: chain.json failed to parse; skipping ref",
            ),
        }
    }
    Ok(out)
}

/// Filter the supplied listing for `<prefix>/packs/<sha>.pack` keys
/// and pair each with its parsed content sha. Sibling `.idx` files
/// and malformed names are dropped silently. Returns a [`HashMap`]
/// for cheap orphan-set derivation downstream.
fn pack_metas_from_objects(prefix: &str, objects: &[ObjectMeta]) -> HashMap<Sha40, ObjectMeta> {
    let packs_prefix = keys::join(Some(prefix), "packs/");
    let mut out: HashMap<Sha40, ObjectMeta> = HashMap::new();
    for meta in objects {
        if !meta.key.starts_with(&packs_prefix) {
            continue;
        }
        // `rsplit('/').next()` always yields one element for any
        // non-empty input — and `meta.key` cannot be empty in a
        // packs/ listing — so the `expect` documents the invariant
        // rather than papering over an unreachable code path.
        let basename = meta
            .key
            .rsplit('/')
            .next()
            .expect("rsplit yields at least one element");
        let Some(sha_str) = basename.strip_suffix(".pack") else {
            continue;
        };
        let Ok(sha) = Sha40::try_new(sha_str) else {
            continue;
        };
        out.insert(sha, meta.clone());
    }
    out
}

/// Filter the supplied listing for tombstone keys, fetch and parse
/// each body, and return rows sorted oldest-first. Per-entry parse
/// or transport failures warn-and-skip.
async fn load_tombstones(
    store: &dyn ObjectStore,
    prefix: &str,
    objects: &[ObjectMeta],
) -> Result<Vec<TombstoneRow>, PackchainError> {
    let now = OffsetDateTime::now_utc();
    let mut out: Vec<TombstoneRow> = Vec::new();
    for meta in objects {
        if !is_tombstone_key(&meta.key, prefix) {
            continue;
        }
        // Per-entry transport failures warn-and-skip; see `load_chains`.
        let body = match store.get_bytes(&meta.key).await {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    key = %meta.key,
                    error = %e,
                    "audit: tombstone fetch failed; skipping",
                );
                continue;
            }
        };
        let tombstone = match Tombstone::from_json_bytes(&body) {
            Ok(t) => t,
            Err(e) => {
                warn!(
                    key = %meta.key,
                    error = %e,
                    "audit: tombstone failed to parse; skipping",
                );
                continue;
            }
        };
        let age_hours = OffsetDateTime::parse(&tombstone.marked_at, &Rfc3339)
            .map_or(0, |m| (now - m).whole_hours());
        out.push(TombstoneRow {
            run_id: tombstone.run_id,
            marked_at: tombstone.marked_at,
            age_hours,
            orphan_count: tombstone.orphan_packs.len(),
        });
    }
    out.sort_by(|a, b| a.marked_at.cmp(&b.marked_at));
    Ok(out)
}

/// Stricter sibling of `gc::is_tombstone_key`: requires the
/// `<prefix>/gc/tombstones-` prefix AND a `.json` suffix. The gc
/// caller separately filters on `.json` before invoking the prefix
/// check, so the two callers reach the same effective acceptance set.
fn is_tombstone_key(key: &str, prefix: &str) -> bool {
    let expected = keys::join(Some(prefix), "gc/tombstones-");
    key.starts_with(&expected) && key.as_bytes().ends_with(b".json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::PutOpts;
    use crate::object_store::mock::MockStore;
    use crate::packchain::manifest::write_chain;
    use crate::packchain::schema::ChainSegment;
    use bytes::Bytes;

    const SHA_TIP: &str = "0000000000000000000000000000000000000001";
    const SHA_FULL: &str = "0000000000000000000000000000000000000002";
    const SHA_PACK_LIVE: &str = "1111111111111111111111111111111111111111";
    const SHA_PACK_LIVE_2: &str = "4444444444444444444444444444444444444444";
    const SHA_PACK_ORPHAN: &str = "2222222222222222222222222222222222222222";
    const SHA_PACK_DANGLING: &str = "3333333333333333333333333333333333333333";

    fn sha40(s: &str) -> Sha40 {
        Sha40::try_new(s).unwrap()
    }

    fn write_pack(store: &MockStore, prefix: &str, sha: &str, bytes: &[u8]) {
        let key = format!("{prefix}/packs/{sha}.pack");
        store.insert(&key, Bytes::copy_from_slice(bytes));
        // An idx sibling is normally present too; the audit doesn't
        // need it for orphan/byte accounting, but several tests assume
        // both are listed so the orphan-byte total reflects only `.pack`.
        let idx_key = format!("{prefix}/packs/{sha}.idx");
        store.insert(&idx_key, Bytes::from_static(b"idx"));
    }

    async fn write_chain_segment(
        store: &MockStore,
        prefix: &str,
        ref_name: &str,
        tip: &str,
        full_at: &str,
        segments: Vec<(String, u64, &str, Option<&str>)>,
    ) {
        let chain = ChainManifest {
            v: 1,
            tip: sha40(tip),
            full_at: sha40(full_at),
            segments: segments
                .into_iter()
                .map(|(pack, bytes, sha, parent)| ChainSegment {
                    sha: sha40(sha),
                    parent_sha: parent.map(sha40),
                    pack,
                    bytes,
                })
                .collect(),
        };
        let rn = crate::git::RefName::new(ref_name).unwrap();
        write_chain(store, Some(prefix), &rn, &chain).await.unwrap();
    }

    /// List the bucket-wide object set for a test prefix and run
    /// `audit` against it. Mirrors the doctor's "list once, audit
    /// from listing" flow so tests exercise the same code path.
    async fn audit_test(store: &MockStore, prefix: &str) -> AuditReport {
        let list_prefix = crate::keys::join(Some(prefix), "");
        let objects = store.list(&list_prefix).await.unwrap();
        audit(store, prefix, &objects).await.unwrap()
    }

    #[tokio::test]
    async fn empty_bucket_returns_empty_report() {
        let store = MockStore::new();
        let report = audit_test(&store, "repo").await;
        assert_eq!(report.orphans.pack_count, 0);
        assert_eq!(report.orphans.bytes, 0);
        assert!(report.tombstones.is_empty());
        assert!(report.branches.is_empty());
        assert!(report.dangling.is_empty());
    }

    #[tokio::test]
    async fn orphan_pack_is_counted_with_bytes() {
        let store = MockStore::new();
        // Live: referenced by chain.
        write_pack(&store, "repo", SHA_PACK_LIVE, b"live-pack-body");
        // Orphan: not referenced by any chain.
        write_pack(&store, "repo", SHA_PACK_ORPHAN, b"orphan-pack-body-9-extra");
        write_chain_segment(
            &store,
            "repo",
            "refs/heads/main",
            SHA_TIP,
            SHA_TIP,
            vec![(format!("packs/{SHA_PACK_LIVE}.pack"), 14, SHA_TIP, None)],
        )
        .await;

        let report = audit_test(&store, "repo").await;
        assert_eq!(report.orphans.pack_count, 1);
        // Body length is 24; idx file is excluded.
        assert_eq!(
            report.orphans.bytes,
            b"orphan-pack-body-9-extra".len() as u64
        );
    }

    #[tokio::test]
    async fn pending_tombstone_surfaces_with_age() {
        let store = MockStore::new();
        // Tombstone marked 2 hours ago.
        let marked_at = (OffsetDateTime::now_utc() - time::Duration::hours(2))
            .format(&Rfc3339)
            .unwrap();
        let body = format!(
            r#"{{"v":1,"run_id":"abc-1","marked_at":"{marked_at}","orphan_packs":["{SHA_PACK_ORPHAN}"]}}"#
        );
        let key = format!("repo/gc/tombstones-abc-1-{marked_at}.json");
        store
            .put_bytes(&key, Bytes::from(body), PutOpts::default())
            .await
            .unwrap();

        let report = audit_test(&store, "repo").await;
        assert_eq!(report.tombstones.len(), 1);
        let row = &report.tombstones[0];
        assert_eq!(row.run_id, "abc-1");
        assert_eq!(row.orphan_count, 1);
        assert!(
            (1..=3).contains(&row.age_hours),
            "age should be ~2h, got {}",
            row.age_hours,
        );
    }

    #[tokio::test]
    async fn corrupt_tombstone_is_skipped() {
        let store = MockStore::new();
        store.insert(
            "repo/gc/tombstones-bad-2025-01-01T00:00:00Z.json",
            Bytes::from_static(b"{not-json"),
        );
        let report = audit_test(&store, "repo").await;
        assert!(report.tombstones.is_empty());
    }

    #[tokio::test]
    async fn branch_under_threshold_is_not_recommended() {
        let store = MockStore::new();
        write_pack(&store, "repo", SHA_PACK_LIVE, b"x");
        // Two segments, well under both thresholds.
        write_chain_segment(
            &store,
            "repo",
            "refs/heads/main",
            SHA_TIP,
            SHA_FULL,
            vec![
                (
                    format!("packs/{SHA_PACK_LIVE}.pack"),
                    1_024,
                    SHA_TIP,
                    Some(SHA_FULL),
                ),
                (
                    format!("packs/{SHA_PACK_LIVE_2}.pack"),
                    2_048,
                    SHA_FULL,
                    None,
                ),
            ],
        )
        .await;
        write_pack(&store, "repo", SHA_PACK_LIVE_2, b"y");

        let report = audit_test(&store, "repo").await;
        assert_eq!(report.branches.len(), 1);
        let row = &report.branches[0];
        assert_eq!(row.ref_path, "refs/heads/main");
        assert_eq!(row.segments_total, 2);
        assert_eq!(row.bytes_total, 1_024 + 2_048);
        assert!(!row.recommend_compact);
        assert!(row.has_full_at_segment);
    }

    #[tokio::test]
    async fn branch_at_segment_boundary_is_not_recommended() {
        // Exactly COMPACT_SEGMENTS_THRESHOLD segments must NOT trigger;
        // recommendation fires only when *strictly greater than* the
        // threshold.
        let store = MockStore::new();
        let segs: Vec<(String, u64, &str, Option<&str>)> = (0..COMPACT_SEGMENTS_THRESHOLD)
            .map(|i| {
                let pack = format!("packs/{:040x}.pack", 0xa000 + i);
                (pack, 1, SHA_TIP, None)
            })
            .collect();
        write_chain_segment(&store, "repo", "refs/heads/main", SHA_TIP, SHA_TIP, segs).await;
        let report = audit_test(&store, "repo").await;
        let row = report
            .branches
            .iter()
            .find(|r| r.ref_path == "refs/heads/main")
            .unwrap();
        assert_eq!(row.segments_total, COMPACT_SEGMENTS_THRESHOLD);
        assert!(!row.recommend_compact);
    }

    #[tokio::test]
    async fn branch_over_segment_threshold_is_recommended() {
        let store = MockStore::new();
        let segs: Vec<(String, u64, &str, Option<&str>)> = (0..=COMPACT_SEGMENTS_THRESHOLD)
            .map(|i| {
                let pack = format!("packs/{:040x}.pack", 0xb000 + i);
                (pack, 1, SHA_TIP, None)
            })
            .collect();
        write_chain_segment(&store, "repo", "refs/heads/main", SHA_TIP, SHA_TIP, segs).await;
        let report = audit_test(&store, "repo").await;
        let row = report
            .branches
            .iter()
            .find(|r| r.ref_path == "refs/heads/main")
            .unwrap();
        assert_eq!(row.segments_total, COMPACT_SEGMENTS_THRESHOLD + 1);
        assert!(row.recommend_compact);
    }

    #[tokio::test]
    async fn branch_over_byte_threshold_is_recommended() {
        let store = MockStore::new();
        write_chain_segment(
            &store,
            "repo",
            "refs/heads/main",
            SHA_TIP,
            SHA_TIP,
            vec![(
                format!("packs/{SHA_PACK_LIVE}.pack"),
                COMPACT_BYTES_THRESHOLD + 1,
                SHA_TIP,
                None,
            )],
        )
        .await;
        let report = audit_test(&store, "repo").await;
        let row = &report.branches[0];
        assert!(row.recommend_compact);
    }

    #[tokio::test]
    async fn branch_at_byte_boundary_is_not_recommended() {
        // Mirror of the segments boundary test: exactly the byte
        // threshold must NOT recommend; recommendation fires only on
        // strictly greater. Catches a regression that swapped `>` for
        // `>=` on the bytes clause.
        let store = MockStore::new();
        write_chain_segment(
            &store,
            "repo",
            "refs/heads/main",
            SHA_TIP,
            SHA_TIP,
            vec![(
                format!("packs/{SHA_PACK_LIVE}.pack"),
                COMPACT_BYTES_THRESHOLD,
                SHA_TIP,
                None,
            )],
        )
        .await;
        let report = audit_test(&store, "repo").await;
        let row = &report.branches[0];
        assert_eq!(row.bytes_total, COMPACT_BYTES_THRESHOLD);
        assert!(!row.recommend_compact);
    }

    #[tokio::test]
    async fn branch_with_full_at_missing_from_segments_is_flagged() {
        // A corrupted manifest whose `full_at` doesn't match any
        // segment's `sha`. The audit still computes totals but flags
        // `has_full_at_segment = false` so the doctor surfaces an
        // ERROR row.
        let store = MockStore::new();
        // tip + full_at differ; segments contain only a segment whose
        // sha matches `tip`, NOT `full_at`. This is the canary the
        // doctor's ERRORS section watches for.
        let chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_FULL),
            segments: vec![ChainSegment {
                sha: sha40(SHA_TIP),
                parent_sha: None,
                pack: format!("packs/{SHA_PACK_LIVE}.pack"),
                bytes: 1,
            }],
        };
        let rn = crate::git::RefName::new("refs/heads/main").unwrap();
        write_chain(&store, Some("repo"), &rn, &chain)
            .await
            .unwrap();
        write_pack(&store, "repo", SHA_PACK_LIVE, b"x");

        let report = audit_test(&store, "repo").await;
        let row = report
            .branches
            .iter()
            .find(|r| r.ref_path == "refs/heads/main")
            .expect("branch present");
        assert!(
            !row.has_full_at_segment,
            "full_at not in segments must flag the branch row",
        );
    }

    #[tokio::test]
    async fn audit_skips_chain_json_with_path_traversal_in_ref_name() {
        // Defense-in-depth (mirrors `list::list_refs`): a maliciously-
        // planted key like `<prefix>/refs/heads/../etc/passwd/chain.json`
        // would otherwise yield ref path `refs/heads/../etc/passwd` and
        // emit it verbatim into the doctor's stdout.
        let store = MockStore::new();
        write_chain_segment(
            &store,
            "repo",
            "refs/heads/main",
            SHA_TIP,
            SHA_TIP,
            vec![(format!("packs/{SHA_PACK_LIVE}.pack"), 1, SHA_TIP, None)],
        )
        .await;
        write_pack(&store, "repo", SHA_PACK_LIVE, b"x");
        store.insert(
            "repo/refs/heads/../etc/passwd/chain.json",
            Bytes::from(
                format!(r#"{{"v":1,"tip":"{SHA_TIP}","full_at":"{SHA_TIP}","segments":[]}}"#)
                    .into_bytes(),
            ),
        );
        let report = audit_test(&store, "repo").await;
        assert_eq!(report.branches.len(), 1);
        assert_eq!(report.branches[0].ref_path, "refs/heads/main");
        assert!(
            !report.branches.iter().any(|r| r.ref_path.contains("..")),
            "no entry with `..` in ref_path may reach the report",
        );
    }

    #[tokio::test]
    async fn dangling_chain_reference_is_reported() {
        let store = MockStore::new();
        // Chain references a pack key that doesn't exist on the bucket.
        write_chain_segment(
            &store,
            "repo",
            "refs/heads/main",
            SHA_TIP,
            SHA_TIP,
            vec![(
                format!("packs/{SHA_PACK_DANGLING}.pack"),
                1_024,
                SHA_TIP,
                None,
            )],
        )
        .await;
        let report = audit_test(&store, "repo").await;
        assert_eq!(report.dangling.len(), 1);
        let row = &report.dangling[0];
        assert_eq!(row.ref_path, "refs/heads/main");
        assert!(row.missing_pack_key.contains(SHA_PACK_DANGLING));
    }

    #[tokio::test]
    async fn corrupt_chain_json_is_skipped() {
        let store = MockStore::new();
        store.insert(
            "repo/refs/heads/broken/chain.json",
            Bytes::from_static(b"{not valid json"),
        );
        // Add a good ref alongside.
        write_chain_segment(
            &store,
            "repo",
            "refs/heads/main",
            SHA_TIP,
            SHA_TIP,
            vec![(format!("packs/{SHA_PACK_LIVE}.pack"), 1, SHA_TIP, None)],
        )
        .await;
        write_pack(&store, "repo", SHA_PACK_LIVE, b"x");

        let report = audit_test(&store, "repo").await;
        assert_eq!(report.branches.len(), 1, "broken chain must skip");
        assert_eq!(report.branches[0].ref_path, "refs/heads/main");
    }

    #[tokio::test]
    async fn root_prefix_audit_works() {
        // Repo at bucket root — keys have no `<prefix>/` component.
        let store = MockStore::new();
        let chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_TIP),
            segments: vec![ChainSegment {
                sha: sha40(SHA_TIP),
                parent_sha: None,
                pack: format!("packs/{SHA_PACK_LIVE}.pack"),
                bytes: 1,
            }],
        };
        let rn = crate::git::RefName::new("refs/heads/main").unwrap();
        write_chain(&store, None, &rn, &chain).await.unwrap();
        store.insert(
            format!("packs/{SHA_PACK_LIVE}.pack"),
            Bytes::from_static(b"x"),
        );

        let report = audit_test(&store, "").await;
        assert_eq!(report.branches.len(), 1);
        assert_eq!(report.branches[0].ref_path, "refs/heads/main");
        assert_eq!(report.dangling.len(), 0);
    }
}

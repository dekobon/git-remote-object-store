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

use crate::keys;
use crate::object_store::{ObjectMeta, ObjectStore};

use super::PackchainError;
use super::gc::Tombstone;
use super::keys::{is_chain_json_key, parse_pack_key_sha};
use super::schema::{ChainManifest, Sha40};

/// Segment-count threshold above which a branch is flagged as a
/// compaction candidate. Mirrors the heuristic specified in #67 / #68.
pub const COMPACT_SEGMENTS_THRESHOLD: usize = 20;

/// Bytes-since-`full_at` threshold above which a branch is flagged as
/// a compaction candidate. Default: 100 MiB.
pub const COMPACT_BYTES_THRESHOLD: u64 = 100 * 1_024 * 1_024;

/// Aggregate output of [`audit`]. Each field is independently reportable
/// — an empty `Vec` (or zero count) means "nothing to report" rather
/// than "audit failed".
#[derive(Debug, Clone, Default)]
pub struct AuditReport {
    /// Pack files in `<prefix>/packs/` that no live chain.json
    /// references.
    pub orphans: OrphanReport,
    /// Tombstones currently sitting in `<prefix>/gc/`, sorted oldest
    /// first.
    pub tombstones: Vec<TombstoneRow>,
    /// Per-branch row, sorted by ref path.
    pub branches: Vec<BranchAuditRow>,
    /// chain.json segment-pack references that point at pack keys
    /// missing from the bucket. Sorted by ref path.
    pub dangling: Vec<DanglingRow>,
}

/// Orphan-pack summary. `pack_count` counts unique content-shas;
/// `bytes` sums the on-bucket size of each orphan `.pack` file
/// (the matching `.idx` is excluded so the total reflects
/// recoverable storage rather than raw key count).
#[derive(Debug, Clone, Copy, Default)]
pub struct OrphanReport {
    /// Number of distinct orphan content-shas.
    pub pack_count: usize,
    /// Total bytes occupied by orphan `.pack` files.
    pub bytes: u64,
}

/// One pending tombstone awaiting sweep.
#[derive(Debug, Clone)]
pub struct TombstoneRow {
    /// Bucket key of the tombstone JSON file.
    pub key: String,
    /// `UUIDv4` run id from the tombstone body.
    pub run_id: String,
    /// RFC 3339 timestamp from the tombstone body.
    pub marked_at: String,
    /// Whole hours since `marked_at` (negative when the tombstone's
    /// timestamp is in the future, e.g. operator clock skew).
    pub age_hours: i64,
    /// Number of orphan packs the tombstone names.
    pub orphan_count: usize,
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
pub struct BranchAuditRow {
    /// Full ref path (e.g. `refs/heads/main`).
    pub ref_path: String,
    /// `chain.segments.len()`.
    pub segments_total: usize,
    /// Sum of `segment.bytes` over `chain.segments`.
    pub bytes_total: u64,
    /// `true` when either threshold is exceeded.
    pub recommend_compact: bool,
    /// `true` when `chain.full_at` is not present as a segment's
    /// `sha`. A corrupted manifest is reported but does not change
    /// the totals above.
    pub full_at_missing_from_segments: bool,
}

/// One chain.json segment that points at a pack key missing from the
/// bucket. Distinct from an orphan: an orphan exists on the bucket
/// without a chain reference; a dangling reference is a chain
/// pointing at a pack that has been deleted.
#[derive(Debug, Clone)]
pub struct DanglingRow {
    /// Ref whose chain.json references the missing pack.
    pub ref_path: String,
    /// Pack key the chain.json segment names.
    pub missing_pack_key: String,
}

/// Walk the bucket once and produce an [`AuditReport`].
///
/// Performs three list calls (one each for `<prefix>/refs/heads/`,
/// `<prefix>/packs/`, and `<prefix>/gc/`) plus one `get_bytes` per
/// chain.json and per tombstone. Per-entry parse failures are logged
/// at `warn` and the entry is skipped rather than aborting the audit
/// — `doctor` is read-only and a corrupt artefact on one branch
/// shouldn't blackhole the rest of the report.
///
/// # Errors
///
/// Returns [`PackchainError::Store`] for transport failures on any
/// list or get call. JSON-parse failures on per-entry artefacts do
/// not surface as errors — they are logged and the entry is skipped.
pub async fn audit(store: &dyn ObjectStore, prefix: &str) -> Result<AuditReport, PackchainError> {
    let chains = load_chains(store, prefix).await?;
    let pack_metas = list_pack_metas(store, prefix).await?;
    let tombstones = load_tombstones(store, prefix).await?;

    let referenced: HashSet<Sha40> = chains
        .iter()
        .flat_map(|(_, chain)| chain.segments.iter())
        .filter_map(|s| parse_pack_key_sha(&s.pack))
        .collect();

    let orphans = pack_metas
        .iter()
        .filter(|(sha, _)| !referenced.contains(sha))
        .fold(OrphanReport::default(), |mut acc, (_, meta)| {
            acc.pack_count += 1;
            acc.bytes = acc.bytes.saturating_add(meta.size);
            acc
        });

    let pack_keys: HashSet<&str> = pack_metas.values().map(|meta| meta.key.as_str()).collect();

    let mut branches: Vec<BranchAuditRow> = chains
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
                .filter(|s| !pack_present(prefix, &s.pack, &pack_keys))
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

/// Parse one branch's chain into a [`BranchAuditRow`].
fn audit_branch(ref_path: &str, chain: &ChainManifest) -> BranchAuditRow {
    let segments_total = chain.segments.len();
    let bytes_total = chain
        .segments
        .iter()
        .map(|s| s.bytes)
        .fold(0u64, u64::saturating_add);
    let recommend_compact =
        segments_total > COMPACT_SEGMENTS_THRESHOLD || bytes_total > COMPACT_BYTES_THRESHOLD;
    let full_at_missing_from_segments = !chain.segments.iter().any(|s| s.sha == chain.full_at);
    BranchAuditRow {
        ref_path: ref_path.to_owned(),
        segments_total,
        bytes_total,
        recommend_compact,
        full_at_missing_from_segments,
    }
}

/// Resolve a chain segment's `pack` field to an absolute key and check
/// presence against the listed pack keys. The schema stores the pack
/// field with or without the bucket prefix; collapse to the shape the
/// listing returns by re-deriving the canonical absolute key.
fn pack_present(prefix: &str, pack_field: &str, pack_keys: &HashSet<&str>) -> bool {
    let Some(sha) = parse_pack_key_sha(pack_field) else {
        return false;
    };
    let key = super::keys::pack_key(super::keys::optional_prefix(prefix), &sha);
    pack_keys.contains(key.as_str())
}

/// List `<prefix>/refs/heads/`, fetch every chain.json, and parse.
/// Per-entry parse failures warn and skip; transport failures on the
/// initial list call abort.
async fn load_chains(
    store: &dyn ObjectStore,
    prefix: &str,
) -> Result<Vec<(String, ChainManifest)>, PackchainError> {
    let refs_prefix = keys::join(prefix, "refs/heads/");
    let metas = store.list(&refs_prefix).await?;

    let mut out: Vec<(String, ChainManifest)> = Vec::new();
    for meta in metas {
        if !is_chain_json_key(&meta.key) {
            continue;
        }
        let Some(ref_path) = ref_path_from_chain_key(prefix, &meta.key) else {
            warn!(key = %meta.key, "audit: chain.json key has unexpected shape; skipping");
            continue;
        };
        let body = store.get_bytes(&meta.key).await?;
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

/// List `<prefix>/packs/` and pair each `.pack` key with its parsed
/// content sha. Sibling `.idx` files and any malformed names are
/// dropped silently. Returns a [`HashMap`] for cheap orphan-set
/// derivation downstream.
async fn list_pack_metas(
    store: &dyn ObjectStore,
    prefix: &str,
) -> Result<HashMap<Sha40, ObjectMeta>, PackchainError> {
    let packs_prefix = keys::join(prefix, "packs/");
    let metas = store.list(&packs_prefix).await?;
    let mut out: HashMap<Sha40, ObjectMeta> = HashMap::new();
    for meta in metas {
        let basename = meta.key.rsplit('/').next().unwrap_or(meta.key.as_str());
        let Some(sha_str) = basename.strip_suffix(".pack") else {
            continue;
        };
        let Ok(sha) = Sha40::try_new(sha_str) else {
            continue;
        };
        out.insert(sha, meta);
    }
    Ok(out)
}

/// List `<prefix>/gc/` and parse every tombstone JSON. Per-entry parse
/// failures warn-and-skip. Returns the tombstones sorted oldest-first
/// so the doctor's report is easy to read.
async fn load_tombstones(
    store: &dyn ObjectStore,
    prefix: &str,
) -> Result<Vec<TombstoneRow>, PackchainError> {
    let gc_prefix = keys::join(prefix, "gc/");
    let metas = store.list(&gc_prefix).await?;
    let now = OffsetDateTime::now_utc();
    let mut out: Vec<TombstoneRow> = Vec::new();
    for meta in metas {
        if !is_tombstone_key(&meta.key, prefix) {
            continue;
        }
        let body = store.get_bytes(&meta.key).await?;
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
            key: meta.key,
            run_id: tombstone.run_id,
            marked_at: tombstone.marked_at,
            age_hours,
            orphan_count: tombstone.orphan_packs.len(),
        });
    }
    out.sort_by(|a, b| a.marked_at.cmp(&b.marked_at));
    Ok(out)
}

/// Mirror of `gc::is_tombstone_key`. Inlined here so audit doesn't
/// depend on a `gc`-private helper; both reduce to the same prefix
/// check (`<prefix>/gc/tombstones-`).
fn is_tombstone_key(key: &str, prefix: &str) -> bool {
    let expected = keys::join(prefix, "gc/tombstones-");
    key.starts_with(&expected) && key.as_bytes().ends_with(b".json")
}

/// Strip `<prefix>/` and `/chain.json` to derive the ref path. Mirror
/// of the same helper in [`super::list`]; kept private here so the
/// audit is self-contained even if the list helper's signature
/// evolves.
fn ref_path_from_chain_key(prefix: &str, key: &str) -> Option<String> {
    let without_suffix = key.strip_suffix("/chain.json")?;
    if prefix.is_empty() {
        return Some(without_suffix.to_owned());
    }
    without_suffix
        .strip_prefix(prefix)
        .and_then(|s| s.strip_prefix('/'))
        .map(str::to_owned)
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

    #[tokio::test]
    async fn empty_bucket_returns_empty_report() {
        let store = MockStore::new();
        let report = audit(&store, "repo").await.unwrap();
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

        let report = audit(&store, "repo").await.unwrap();
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

        let report = audit(&store, "repo").await.unwrap();
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
    async fn corrupt_tombstone_is_skipped_with_warning() {
        let store = MockStore::new();
        store.insert(
            "repo/gc/tombstones-bad-2025-01-01T00:00:00Z.json",
            Bytes::from_static(b"{not-json"),
        );
        let report = audit(&store, "repo").await.unwrap();
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

        let report = audit(&store, "repo").await.unwrap();
        assert_eq!(report.branches.len(), 1);
        let row = &report.branches[0];
        assert_eq!(row.ref_path, "refs/heads/main");
        assert_eq!(row.segments_total, 2);
        assert_eq!(row.bytes_total, 1_024 + 2_048);
        assert!(!row.recommend_compact);
        assert!(!row.full_at_missing_from_segments);
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
        let report = audit(&store, "repo").await.unwrap();
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
        let report = audit(&store, "repo").await.unwrap();
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
        let report = audit(&store, "repo").await.unwrap();
        let row = &report.branches[0];
        assert!(row.recommend_compact);
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
        let report = audit(&store, "repo").await.unwrap();
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

        let report = audit(&store, "repo").await.unwrap();
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

        let report = audit(&store, "").await.unwrap();
        assert_eq!(report.branches.len(), 1);
        assert_eq!(report.branches[0].ref_path, "refs/heads/main");
        assert_eq!(report.dangling.len(), 0);
    }
}

//! On-bucket [`ChainManifest`] / [`PathIndex`] read / write helpers.
//!
//! Centralises every code path that touches `chain.json` and
//! `path-index.json` so the engine's wire format stays in one place
//! and the schema-version validation in [`ChainManifest::from_json_bytes`]
//! / [`PathIndex::from_json_bytes`] runs everywhere a parsed value
//! enters the engine.

use bytes::Bytes;

use crate::git::RefName;
use crate::object_store::{ObjectStore, ObjectStoreError, PutOpts};

use super::PackchainError;
use super::keys::{chain_key, path_index_key};
use super::schema::{ChainManifest, ChainSegment, PathIndex, Sha40};

/// Read `<prefix>/<ref_name>/chain.json` and parse it.
///
/// Returns `Ok(None)` when the chain key does not exist (first push
/// against the bucket / branch). Every other failure — transport,
/// malformed JSON, invalid sha, unsupported schema version — surfaces
/// as [`PackchainError`] so the caller can decide between
/// [`crate::protocol::push::PushOutcome::Error`] (per-ref) or aborting
/// the batch.
pub(crate) async fn load_chain(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    remote_ref: &RefName,
) -> Result<Option<ChainManifest>, PackchainError> {
    let key = chain_key(prefix, remote_ref);
    match store.get_bytes(&key).await {
        Ok(bytes) => Ok(Some(ChainManifest::from_json_bytes(&bytes)?)),
        Err(ObjectStoreError::NotFound(_)) => Ok(None),
        Err(e) => Err(PackchainError::Store(e)),
    }
}

/// Overwrite `<prefix>/<ref_name>/chain.json` with `manifest`.
///
/// The under-lock caller relies on this PUT being atomic — a partial
/// write would leave the bucket with truncated JSON the next reader
/// fails to parse. `put_bytes` is implemented with a single PUT on
/// every supported backend, which is atomic by definition.
pub(crate) async fn write_chain(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    remote_ref: &RefName,
    manifest: &ChainManifest,
) -> Result<(), PackchainError> {
    let key = chain_key(prefix, remote_ref);
    let body = Bytes::from(manifest.to_json_pretty()?);
    store.put_bytes(&key, body, PutOpts::default()).await?;
    Ok(())
}

/// Read `<prefix>/<ref_name>/path-index.json` and parse it.
///
/// Returns `Ok(None)` when:
///
/// - the key does not exist (the bucket has a chain.json but no
///   path-index, e.g. a partially crashed first push that committed
///   before path-index was rewritten — see the engine's crash-recovery
///   analysis), or
/// - the on-bucket file uses an older schema version (greenfield rule:
///   stale `path-index.json` files in older buckets are treated as
///   absent and the next push re-emits in the current schema). A
///   `tracing::warn!` event is logged so operators see the stale-file
///   event without it surfacing as a hard error to the read path.
///
/// Every other failure (transport, malformed JSON, invalid sha)
/// surfaces as [`PackchainError`].
pub(crate) async fn load_path_index(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    remote_ref: &RefName,
) -> Result<Option<PathIndex>, PackchainError> {
    let key = path_index_key(prefix, remote_ref);
    match store.get_bytes(&key).await {
        Ok(bytes) => match PathIndex::from_json_bytes(&bytes) {
            Ok(parsed) => Ok(Some(parsed)),
            Err(PackchainError::UnsupportedSchemaVersion { found, expected }) => {
                tracing::warn!(
                    key = %key,
                    found_version = found,
                    expected_version = expected,
                    "stale path-index.json schema version; treating as absent — re-push the \
                     ref to regenerate it",
                );
                Ok(None)
            }
            Err(e) => Err(e),
        },
        Err(ObjectStoreError::NotFound(_)) => Ok(None),
        Err(e) => Err(PackchainError::Store(e)),
    }
}

/// Overwrite `<prefix>/<ref_name>/path-index.json` with `index`.
pub(crate) async fn write_path_index(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    remote_ref: &RefName,
    index: &PathIndex,
) -> Result<(), PackchainError> {
    let key = path_index_key(prefix, remote_ref);
    let body = Bytes::from(index.to_json_pretty()?);
    store.put_bytes(&key, body, PutOpts::default()).await?;
    Ok(())
}

/// Build the [`ChainManifest`] this push will write.
///
/// `prior` is the under-lock chain snapshot (`None` for first push,
/// `Some` for incremental). `local_tip` is the new tip. `segment` is
/// the pre-built [`ChainSegment`] for the pack uploaded by this push;
/// the caller leaves `segment.parent_sha` as `None` and lets this
/// function fill it in for the incremental case (so the field's
/// correctness doesn't depend on the caller having the same
/// branching logic the manifest builder already encodes).
///
/// `force` switches to "treat as fresh first push" semantics:
/// segments collapse to one, `full_at` jumps to `local_tip`. For a
/// non-force first push (`prior.is_none()`) and for force push the
/// resulting manifest is wire-identical: a single segment with
/// `parent_sha = None` and `full_at = local_tip`. That gives Phase 5
/// GC and Phase 3 fetch a single shape to reason about regardless of
/// how the chain was reset.
pub(crate) fn next_manifest(
    prior: Option<&ChainManifest>,
    local_tip: &Sha40,
    mut segment: ChainSegment,
    force: bool,
) -> ChainManifest {
    let (full_at, segments) = if let (false, Some(prior)) = (force, prior) {
        // Incremental: parent is the prior chain's tip; prepend the
        // new segment, retain `full_at`.
        segment.parent_sha = Some(prior.tip.clone());
        let mut segments = Vec::with_capacity(prior.segments.len() + 1);
        segments.push(segment);
        segments.extend(prior.segments.iter().cloned());
        (prior.full_at.clone(), segments)
    } else {
        // First push or force push: collapse to a single root segment
        // with no parent. Phase 3 fetch reads `parent_sha: None` as
        // "you have reached the chain root".
        segment.parent_sha = None;
        (local_tip.clone(), vec![segment])
    };
    ChainManifest {
        v: ChainManifest::SCHEMA_VERSION,
        tip: local_tip.clone(),
        full_at,
        segments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::mock::MockStore;

    const SHA_A: &str = "0000000000000000000000000000000000000001";
    const SHA_B: &str = "0000000000000000000000000000000000000002";
    const SHA_C: &str = "0000000000000000000000000000000000000003";
    const SHA_PACK1: &str = "1111111111111111111111111111111111111111";
    const SHA_PACK2: &str = "2222222222222222222222222222222222222222";

    fn sha40(s: &str) -> Sha40 {
        Sha40::try_new(s).unwrap()
    }

    fn ref_main() -> RefName {
        RefName::new("refs/heads/main").unwrap()
    }

    fn segment(sha: &str, parent: Option<&str>, pack_sha: &str, bytes: u64) -> ChainSegment {
        ChainSegment {
            sha: sha40(sha),
            parent_sha: parent.map(sha40),
            pack: format!("packs/{pack_sha}.pack"),
            bytes,
        }
    }

    // --- next_manifest --------------------------------------------------

    #[test]
    fn next_manifest_first_push_writes_single_root_segment() {
        let seg = segment(SHA_A, Some(SHA_B), SHA_PACK1, 1_024);
        let m = next_manifest(None, &sha40(SHA_A), seg, false);
        assert_eq!(m.tip, sha40(SHA_A));
        assert_eq!(m.full_at, sha40(SHA_A));
        assert_eq!(m.segments.len(), 1);
        assert_eq!(m.segments[0].sha, sha40(SHA_A));
        assert_eq!(
            m.segments[0].parent_sha, None,
            "first-push segment must have parent_sha=None even when caller passes a parent",
        );
    }

    #[test]
    fn next_manifest_force_push_collapses_segments_to_one() {
        // Prior chain has two segments; force push must wipe them and
        // produce a single root segment exactly like a first push.
        let prior = ChainManifest {
            v: 1,
            tip: sha40(SHA_B),
            full_at: sha40(SHA_C),
            segments: vec![
                segment(SHA_B, Some(SHA_C), SHA_PACK1, 100),
                segment(SHA_C, None, SHA_PACK2, 200),
            ],
        };
        let seg = segment(SHA_A, Some(SHA_B), SHA_PACK1, 1_024);
        let m = next_manifest(Some(&prior), &sha40(SHA_A), seg, true);
        assert_eq!(m.tip, sha40(SHA_A));
        assert_eq!(m.full_at, sha40(SHA_A));
        assert_eq!(m.segments.len(), 1);
        assert_eq!(m.segments[0].parent_sha, None);
    }

    #[test]
    fn next_manifest_incremental_prepends_newest_segment_preserves_full_at() {
        let prior = ChainManifest {
            v: 1,
            tip: sha40(SHA_B),
            full_at: sha40(SHA_C),
            segments: vec![segment(SHA_B, None, SHA_PACK1, 100)],
        };
        let seg = segment(SHA_A, Some(SHA_B), SHA_PACK2, 200);
        let m = next_manifest(Some(&prior), &sha40(SHA_A), seg, false);
        assert_eq!(m.tip, sha40(SHA_A));
        assert_eq!(
            m.full_at,
            sha40(SHA_C),
            "incremental push must retain prior.full_at",
        );
        assert_eq!(m.segments.len(), 2);
        assert_eq!(m.segments[0].sha, sha40(SHA_A));
        assert_eq!(m.segments[0].parent_sha, Some(sha40(SHA_B)));
        assert_eq!(m.segments[1].sha, sha40(SHA_B));
    }

    // --- load_chain / write_chain --------------------------------------

    #[tokio::test]
    async fn load_chain_returns_none_when_chain_absent() {
        let store = MockStore::new();
        let result = load_chain(&store, None, &ref_main()).await.unwrap();
        assert!(
            result.is_none(),
            "absent chain.json must surface as Ok(None)"
        );
    }

    #[tokio::test]
    async fn load_chain_round_trips_via_write_chain() {
        let store = MockStore::new();
        let chain = ChainManifest {
            v: ChainManifest::SCHEMA_VERSION,
            tip: sha40(SHA_A),
            full_at: sha40(SHA_A),
            segments: vec![segment(SHA_A, None, SHA_PACK1, 100)],
        };
        write_chain(&store, Some("repo"), &ref_main(), &chain)
            .await
            .unwrap();
        let loaded = load_chain(&store, Some("repo"), &ref_main())
            .await
            .unwrap()
            .expect("chain must exist after write");
        assert_eq!(loaded, chain);
    }

    #[tokio::test]
    async fn load_chain_propagates_unsupported_schema_version() {
        let store = MockStore::new();
        // Hand-craft a v=2 chain.json — the parser must refuse without
        // touching disk further.
        store.insert(
            chain_key(None, ref_main()),
            Bytes::from_static(
                br#"{"v":2,"tip":"0000000000000000000000000000000000000001","full_at":"0000000000000000000000000000000000000001","segments":[]}"#,
            ),
        );
        let err = load_chain(&store, None, &ref_main()).await.unwrap_err();
        assert!(
            matches!(
                err,
                PackchainError::UnsupportedSchemaVersion {
                    found: 2,
                    expected: 1,
                },
            ),
            "expected UnsupportedSchemaVersion(2,1), got {err:?}",
        );
    }

    #[tokio::test]
    async fn load_chain_propagates_invalid_sha() {
        let store = MockStore::new();
        store.insert(
            chain_key(None, ref_main()),
            Bytes::from_static(
                br#"{"v":1,"tip":"not-a-sha","full_at":"0000000000000000000000000000000000000001","segments":[]}"#,
            ),
        );
        let err = load_chain(&store, None, &ref_main()).await.unwrap_err();
        assert!(matches!(err, PackchainError::ParseJson(_)));
    }

    #[tokio::test]
    async fn load_path_index_treats_stale_v1_file_as_absent() {
        // Greenfield rule: stale `path-index.json` files written under an
        // older schema version are treated as absent. Surfacing them as
        // a hard read error would break `read_blob` against any bucket
        // that wasn't re-pushed after the schema bump.
        let store = MockStore::new();
        store.insert(
            path_index_key(None, ref_main()),
            Bytes::from_static(
                br#"{"v":1,"commit":"0000000000000000000000000000000000000001","tree":{}}"#,
            ),
        );
        let result = load_path_index(&store, None, &ref_main()).await.unwrap();
        assert!(
            result.is_none(),
            "stale v=1 path-index must be treated as absent; got {result:?}",
        );
    }

    #[tokio::test]
    async fn write_path_index_is_pretty_printed() {
        // Pretty-printing makes operator inspection cheap; assert the
        // serialised body is multi-line so a regression to compact JSON
        // can't slip through.
        let store = MockStore::new();
        let index = PathIndex {
            v: PathIndex::SCHEMA_VERSION,
            tip: sha40(SHA_A),
            tree: std::collections::BTreeMap::new(),
        };
        write_path_index(&store, None, &ref_main(), &index)
            .await
            .unwrap();
        let bytes = store
            .get_bytes(&path_index_key(None, ref_main()))
            .await
            .unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(
            text.contains('\n'),
            "path-index.json must be pretty-printed, got: {text}",
        );
    }
}

//! Read-only view of a repository's on-bucket layout, built by listing
//! `<prefix>/` and grouping the results into refs, bundles, protection
//! markers, and HEAD. Flattened to the single-repo case (one CLI
//! invocation == one prefix).

use std::collections::BTreeMap;

use time::OffsetDateTime;
use tracing::warn;

use super::ManageError;
use crate::object_store::{ObjectMeta, ObjectStore};

/// One bundle object listed under a ref.
#[derive(Debug, Clone)]
pub(crate) struct BundleEntry {
    /// Hex-encoded commit OID extracted from the bundle filename.
    /// Stored as `String` (not `Sha`) for now to avoid an extra
    /// parsing step here; the snapshot classifier guarantees this
    /// value is a 40-lowercase-hex string when populated. Malformed
    /// `<…>.bundle` keys land in [`MalformedBundleKey`] instead.
    pub(crate) sha: String,
    /// Full object key, used directly for `delete` / `copy` calls so the
    /// caller never has to reconstruct it.
    pub(crate) key: String,
    /// Server-side last-modified timestamp.
    pub(crate) last_modified: OffsetDateTime,
}

/// A `<…>.bundle` object whose stem is not a valid 40-lowercase-hex
/// SHA. Push's pre-lock listing silently filters these out (issue #109
/// made `is_bundle_candidate` a positive predicate), so they can
/// accumulate on a bucket with no operator-visible signal. `doctor`
/// surfaces them so an operator can investigate and delete the key
/// manually.
#[derive(Debug, Clone)]
pub(crate) struct MalformedBundleKey {
    /// Ref path under which the malformed bundle was listed (e.g.
    /// `refs/heads/main`). Useful for human-readable rendering — the
    /// operator looks at the ref, not the raw key.
    pub(crate) ref_path: String,
    /// Full object key, ready to feed into `aws s3 rm` / `az storage
    /// blob delete` so the operator never has to reconstruct it.
    pub(crate) key: String,
}

/// Per-ref snapshot — protection state plus every bundle object.
///
/// Constructed by [`analyze_objects`] and consumed only by the doctor.
/// The whole snapshot module is `pub(crate)` because no code outside
/// this crate reads these fields — the `RepoSnapshot` value is built
/// inside `Doctor::run` and the doctor itself owns all rendering.
#[derive(Debug, Clone, Default)]
pub(crate) struct RefSnapshot {
    /// `true` iff a `<ref>/PROTECTED#` marker is present. The marker
    /// is matched by **exact equality** on the final segment via
    /// [`crate::keys::is_protected_marker_segment`]; future
    /// `PROTECTED#`-prefixed keys (e.g. `PROTECTED#audit`) do NOT count.
    pub(crate) is_protected: bool,
    /// Bundle objects under this ref, in listing order. The doctor's
    /// "multiple bundles" check fires when this is longer than one.
    pub(crate) bundles: Vec<BundleEntry>,
    /// `true` iff a `chain.json` manifest is present under this ref.
    /// Indicates a packchain branch; the doctor treats such a ref as
    /// structurally healthy ("Ok") even when no `.bundle` file exists,
    /// because the pack segments are stored under `packs/` rather than
    /// inlined as a bundle.
    pub(crate) has_chain: bool,
}

impl RefSnapshot {
    /// `true` iff this ref carries user-visible branch data — at least
    /// one bundle or a `chain.json` manifest. A ref whose only residue
    /// is a `PROTECTED#` marker returns `false`: the marker is
    /// operational metadata, not branch contents, and a branch that has
    /// shed every bundle / chain is gone for the purposes of HEAD
    /// validity and the doctor's repair path (issue #154). This mirrors
    /// the `key`-level [`super::has_branch_data`] predicate that the
    /// race-detection checks in `Doctor::fix_head` and
    /// `ManageBranch::protect` already use.
    ///
    /// Note: lock-file keys (`*.lock`) are filtered out in
    /// [`classify_into`] before they reach a `RefSnapshot`, so the
    /// in-snapshot view is naturally lock-free. Operational metadata in
    /// the snapshot reduces to `is_protected`.
    #[must_use]
    pub(crate) fn has_branch_data(&self) -> bool {
        !self.bundles.is_empty() || self.has_chain
    }
}

/// Whole-repository snapshot.
#[derive(Debug, Clone, Default)]
pub(crate) struct RepoSnapshot {
    /// Body of `<prefix>/HEAD`, decoded as UTF-8 and trimmed of
    /// surrounding whitespace. `None` when the object is absent or its
    /// body is not valid UTF-8.
    pub(crate) head: Option<String>,
    /// Refs keyed by their full ref-path (e.g. `refs/heads/main`).
    pub(crate) refs: BTreeMap<String, RefSnapshot>,
    /// `<…>.bundle` objects whose stem fails the
    /// [`crate::keys::is_valid_bundle_stem`] check. Push filters these
    /// out before the pre-lock listing reaches `prepare_push`, so they
    /// only surface here in the doctor's snapshot pass.
    pub(crate) malformed_bundle_keys: Vec<MalformedBundleKey>,
}

impl RepoSnapshot {
    /// `true` iff [`head`](Self::head) names a ref in [`refs`](Self::refs)
    /// that carries actual branch data (at least one bundle, or a
    /// `chain.json` manifest).
    ///
    /// A `None` HEAD, a HEAD pointing at an unknown ref, OR a HEAD
    /// pointing at a ref whose only residue is a `PROTECTED#` marker
    /// (issue #154) is "invalid" and triggers `fix_head`. The
    /// `is_protected`-only case matters because `classify_into` creates
    /// a `RefSnapshot` entry for a bare `PROTECTED#` key — a bare entry
    /// alone is operational metadata, not branch contents.
    #[must_use]
    pub(crate) fn is_head_valid(&self) -> bool {
        self.head
            .as_ref()
            .and_then(|h| self.refs.get(h))
            .is_some_and(RefSnapshot::has_branch_data)
    }
}

/// Walk every object under `<prefix>/` and group it into a
/// [`RepoSnapshot`].
///
/// `prefix` must be the full repository prefix from the parsed remote
/// URL (e.g. `acme/myrepo`), without a trailing `/` — this function
/// appends one. An empty `prefix` means "list the entire
/// bucket/container" (root-of-bucket repository) and skips the trailing
/// `/` to avoid emitting a leading-slash list prefix.
///
/// Performs one `list` call. Production callers always already have a
/// listing in hand and use [`analyze_objects`] directly to share the
/// LIST across analysis and stale-lock scanning; this convenience form
/// exists for unit-test ergonomics only.
///
/// # Errors
///
/// Returns [`ManageError::Store`] if the list or HEAD-object get calls
/// fail.
#[cfg(test)]
pub(crate) async fn analyze(
    store: &dyn ObjectStore,
    prefix: &str,
) -> Result<RepoSnapshot, ManageError> {
    let list_prefix = crate::keys::join(Some(prefix), "");
    let objects = store.list(&list_prefix).await?;
    analyze_objects(&objects, &list_prefix, store).await
}

/// Group an already-fetched `<list_prefix>` listing into a
/// [`RepoSnapshot`]. Used by [`analyze`] and by `Doctor::run` to share
/// a single LIST across analysis and stale-lock scanning.
///
/// # Errors
///
/// Returns [`ManageError::Store`] if fetching the `HEAD` object body
/// fails.
pub(crate) async fn analyze_objects(
    objects: &[ObjectMeta],
    list_prefix: &str,
    store: &dyn ObjectStore,
) -> Result<RepoSnapshot, ManageError> {
    let mut snapshot = RepoSnapshot::default();
    for object in objects {
        classify_into(list_prefix, object, &mut snapshot, store).await?;
    }
    Ok(snapshot)
}

/// Slot one listed object into the snapshot. `list_prefix` is the
/// `<prefix>/` form used to strip the leading namespace; everything
/// else is matched against the **relative** path so adding new
/// non-bundle keys (e.g. metadata sidecars) only requires an extra
/// match arm here.
async fn classify_into(
    list_prefix: &str,
    object: &ObjectMeta,
    snapshot: &mut RepoSnapshot,
    store: &dyn ObjectStore,
) -> Result<(), ManageError> {
    let Some(relative) = object.key.strip_prefix(list_prefix) else {
        // Defensive: `list` should only return keys that share the
        // requested prefix. If a backend ever returns a sibling key,
        // skip it rather than misattribute.
        warn!(
            key = %object.key,
            list_prefix = %list_prefix,
            "list returned key outside requested prefix; skipping"
        );
        return Ok(());
    };

    if relative == "HEAD" {
        let body = store.get_bytes(&object.key).await?;
        snapshot.head = std::str::from_utf8(&body)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        return Ok(());
    }

    // Skip known bookkeeping top-level directories that are never ref
    // directories. `packs/` and `gc/` are packchain-internal; `lfs/`
    // holds LFS objects. Including them in `refs` pollutes the doctor
    // report with spurious "No bundles" lines (#75).
    if is_bookkeeping_dir(relative) {
        return Ok(());
    }

    // Every other key is `<ref-path>/<last>`. Anything without a slash
    // (e.g. a sidecar dropped at the prefix root) is unknown and
    // skipped — the doctor never rewrites keys it does not recognise.
    let Some((ref_path, last)) = relative.rsplit_once('/') else {
        return Ok(());
    };

    // `LOCK#.lock` and any future `*.lock` keys are scanned separately
    // by `list_and_handle_stale_locks`; `repo.zip` is the optional
    // `?zip=1` push artefact and is neither a bundle nor a marker.
    if super::is_lock_key(last) || last == "repo.zip" {
        return Ok(());
    }

    if crate::keys::is_protected_marker_segment(last) {
        snapshot
            .refs
            .entry(ref_path.to_owned())
            .or_default()
            .is_protected = true;
    } else if let Some(stem) = last.strip_suffix(".bundle") {
        // Push's `is_bundle_candidate` filters keys where the stem is
        // not 40 lowercase hex chars (#109 + #124). Doctor must do the
        // same split: well-formed stems go into the per-ref bundle
        // set, malformed stems are surfaced to the operator as keys
        // that push will never touch.
        if crate::keys::is_valid_bundle_stem(stem) {
            snapshot
                .refs
                .entry(ref_path.to_owned())
                .or_default()
                .bundles
                .push(BundleEntry {
                    sha: stem.to_owned(),
                    key: object.key.clone(),
                    last_modified: object.last_modified,
                });
        } else {
            snapshot.malformed_bundle_keys.push(MalformedBundleKey {
                ref_path: ref_path.to_owned(),
                key: object.key.clone(),
            });
        }
    } else if last == "chain.json" {
        snapshot
            .refs
            .entry(ref_path.to_owned())
            .or_default()
            .has_chain = true;
    }
    Ok(())
}

/// `true` iff `relative` falls under a known bookkeeping top-level
/// directory that is never a ref directory. These are filtered out
/// before ref-grouping so they don't pollute the doctor report.
///
/// Current set: `packs/` and `gc/` (packchain-internal), `lfs/` (LFS
/// object storage).
///
/// **Maintenance**: if the on-bucket layout adds a new reserved
/// top-level directory, add its `"name/"` prefix here and extend
/// `is_bookkeeping_dir_matches_known_prefixes` in the test suite.
/// The trailing slash is required — `"packs"` (no slash) must NOT
/// match, because a root-level sidecar with that exact name is not
/// a directory.
fn is_bookkeeping_dir(relative: &str) -> bool {
    // Top-level directory prefixes that are never ref directories.
    // Keep sorted alphabetically for scan readability.
    const BOOKKEEPING_PREFIXES: &[&str] = &["gc/", "lfs/", "packs/"];
    BOOKKEEPING_PREFIXES.iter().any(|p| relative.starts_with(p))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::object_store::ObjectStore;
    use crate::object_store::mock::MockStore;
    use bytes::Bytes;

    fn store() -> Arc<dyn ObjectStore> {
        Arc::new(MockStore::new())
    }

    // Valid 40-lower-hex stems used as bundle-key fixtures. Earlier
    // revisions used short stems like "abc"; #124 added stem
    // validation, so the snapshot now distinguishes well-formed
    // bundles from malformed-key keys. Tests that exercise the
    // well-formed path must use real-length stems.
    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[tokio::test]
    async fn empty_listing_yields_empty_snapshot() {
        let s = store();
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert!(snap.head.is_none());
        assert!(snap.refs.is_empty());
        assert!(!snap.is_head_valid());
    }

    #[tokio::test]
    async fn single_ref_one_bundle() {
        let mock = MockStore::new();
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("body"),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        let main = snap.refs.get("refs/heads/main").expect("main present");
        assert_eq!(main.bundles.len(), 1);
        assert_eq!(main.bundles[0].sha, SHA_A);
        assert_eq!(
            main.bundles[0].key,
            format!("myrepo/refs/heads/main/{SHA_A}.bundle")
        );
        assert!(!main.is_protected);
    }

    #[tokio::test]
    async fn protected_marker_exact_match() {
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/PROTECTED#", Bytes::new());
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert!(snap.refs["refs/heads/main"].is_protected);
        assert!(snap.refs["refs/heads/main"].bundles.is_empty());
    }

    #[tokio::test]
    async fn protected_marker_requires_exact_match() {
        // The PROTECTED# marker is matched by exact equality on the
        // final segment — `PROTECTED#`-prefixed keys (e.g. a future
        // `PROTECTED#audit` sidecar) must NOT flip `is_protected`.
        // Pin the strong post-condition: a sidecar that doesn't match
        // any known segment shape leaves the snapshot empty (no phantom
        // ref entry).
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/PROTECTED#audit", Bytes::new());
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        // If the snapshot ever does record this ref (e.g. a future
        // arm recognises `PROTECTED#`-prefixed sidecars), the
        // entry MUST NOT have `is_protected` set.
        if let Some(entry) = snap.refs.get("refs/heads/main") {
            assert!(
                !entry.is_protected,
                "PROTECTED#-prefixed segment must not be classified as the marker",
            );
        }
    }

    #[tokio::test]
    async fn head_object_is_decoded_and_trimmed() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main\n"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("body"),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert_eq!(snap.head.as_deref(), Some("refs/heads/main"));
        assert!(snap.is_head_valid());
    }

    #[tokio::test]
    async fn head_object_invalid_utf8_yields_none() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from(vec![0xff, 0xfe]));
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert!(snap.head.is_none());
        assert!(!snap.is_head_valid());
    }

    #[tokio::test]
    async fn head_pointing_at_unknown_ref_is_invalid() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/missing"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("body"),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert_eq!(snap.head.as_deref(), Some("refs/heads/missing"));
        assert!(!snap.is_head_valid());
    }

    #[tokio::test]
    async fn head_pointing_at_protected_only_ref_is_invalid() {
        // Issue #154: a ref whose only on-bucket residue is a
        // `PROTECTED#` marker has no user-visible branch data. The
        // marker is operational metadata, so HEAD pointing at such a
        // ref must classify as invalid — otherwise the doctor skips the
        // repair path and leaves HEAD silently dangling.
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("myrepo/refs/heads/main/PROTECTED#", Bytes::new());
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        // The ref entry exists (the marker was recognised) but has no
        // branch data — `is_head_valid` must still report false.
        let main = snap.refs.get("refs/heads/main").expect("ref present");
        assert!(main.is_protected);
        assert!(main.bundles.is_empty());
        assert!(!main.has_chain);
        assert!(
            !snap.is_head_valid(),
            "HEAD pointing at a PROTECTED#-only ref must be invalid (#154)",
        );
    }

    #[tokio::test]
    async fn head_pointing_at_protected_ref_with_bundle_is_valid() {
        // Negative control for #154: protection by itself does not
        // invalidate HEAD. A protected branch with at least one bundle
        // (or chain.json) is structurally healthy — the doctor must
        // not offer to rewrite HEAD just because the marker is present.
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert("myrepo/refs/heads/main/PROTECTED#", Bytes::new());
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("body"),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert!(snap.refs["refs/heads/main"].is_protected);
        assert!(snap.is_head_valid());
    }

    #[tokio::test]
    async fn head_pointing_at_chain_only_ref_is_valid() {
        // Negative control for #154: a packchain branch is valid even
        // without a `.bundle` — `chain.json` plus `packs/*` is the
        // canonical packchain on-bucket shape.
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            "myrepo/refs/heads/main/chain.json",
            Bytes::from(r#"{"v":1}"#),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert!(snap.is_head_valid());
    }

    #[tokio::test]
    async fn multiple_bundles_under_one_ref() {
        let mock = MockStore::new();
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("a"),
        );
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_B}.bundle"),
            Bytes::from("b"),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        let shas: std::collections::BTreeSet<&str> = snap.refs["refs/heads/main"]
            .bundles
            .iter()
            .map(|b| b.sha.as_str())
            .collect();
        assert_eq!(shas, [SHA_A, SHA_B].into_iter().collect());
    }

    #[tokio::test]
    async fn lock_files_are_skipped_in_ref_grouping() {
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/LOCK#.lock", Bytes::new());
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert_eq!(snap.refs["refs/heads/main"].bundles.len(), 1);
        assert!(!snap.refs["refs/heads/main"].is_protected);
    }

    #[tokio::test]
    async fn repo_zip_is_skipped_in_ref_grouping() {
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/repo.zip", Bytes::from("zip"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert_eq!(snap.refs["refs/heads/main"].bundles.len(), 1);
    }

    #[tokio::test]
    async fn nested_ref_path_is_preserved() {
        let mock = MockStore::new();
        mock.insert(
            format!("myrepo/refs/heads/feature/x/{SHA_A}.bundle"),
            Bytes::from("a"),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        let entry = snap
            .refs
            .get("refs/heads/feature/x")
            .expect("nested ref recorded");
        assert_eq!(entry.bundles.len(), 1);
        assert_eq!(entry.bundles[0].sha, SHA_A);
        assert_eq!(
            entry.bundles[0].key,
            format!("myrepo/refs/heads/feature/x/{SHA_A}.bundle")
        );
    }

    #[tokio::test]
    async fn root_prefix_lists_bucket_without_leading_slash() {
        // Empty prefix == repository at the bucket root. The on-bucket
        // layout drops the leading `<prefix>/` segment entirely.
        let mock = MockStore::new();
        mock.insert("HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            format!("refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("body"),
        );
        mock.insert("refs/heads/main/PROTECTED#", Bytes::new());
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "").await.expect("analyze at root");
        assert_eq!(snap.head.as_deref(), Some("refs/heads/main"));
        let main = snap.refs.get("refs/heads/main").expect("main present");
        assert_eq!(main.bundles.len(), 1);
        assert_eq!(main.bundles[0].sha, SHA_A);
        assert_eq!(
            main.bundles[0].key,
            format!("refs/heads/main/{SHA_A}.bundle")
        );
        assert!(main.is_protected);
    }

    #[tokio::test]
    async fn empty_head_body_treated_as_missing() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from(""));
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert!(snap.head.is_none());
    }

    // --- Bookkeeping directory filtering (#75) ---------------------------

    #[tokio::test]
    async fn packchain_bookkeeping_dirs_excluded_from_refs() {
        // packs/, gc/, and lfs/ are internal directories that must not
        // appear as ref entries in the snapshot (#75).
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main"));
        mock.insert(
            "myrepo/packs/1111111111111111111111111111111111111111.pack",
            Bytes::from("pack-body"),
        );
        mock.insert(
            "myrepo/gc/tombstones-abc-1-2025-01-01T00:00:00Z.json",
            Bytes::from("{}"),
        );
        mock.insert("myrepo/lfs/abcdef0123456789", Bytes::from("lfs-body"));
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("b"),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");

        // Only the actual ref should be present.
        assert_eq!(
            snap.refs.keys().collect::<Vec<_>>(),
            vec!["refs/heads/main"],
        );
        assert!(!snap.refs.contains_key("packs"));
        assert!(!snap.refs.contains_key("gc"));
        assert!(!snap.refs.contains_key("lfs"));
    }

    #[tokio::test]
    async fn chain_json_sets_has_chain_flag() {
        // A packchain branch has chain.json instead of (or alongside) a
        // .bundle file. The snapshot must record this so the doctor can
        // treat the ref as healthy.
        let mock = MockStore::new();
        mock.insert(
            "myrepo/refs/heads/main/chain.json",
            Bytes::from(r#"{"v":1}"#),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        let main = snap.refs.get("refs/heads/main").expect("main present");
        assert!(main.has_chain, "chain.json must set has_chain");
        assert!(main.bundles.is_empty());
    }

    #[tokio::test]
    async fn chain_json_coexists_with_bundle() {
        // Edge case: a packchain branch that also has a baseline .bundle
        // must record both has_chain and the bundle entry.
        let mock = MockStore::new();
        mock.insert(
            "myrepo/refs/heads/main/chain.json",
            Bytes::from(r#"{"v":1}"#),
        );
        mock.insert(
            format!("myrepo/refs/heads/main/{SHA_A}.bundle"),
            Bytes::from("body"),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        let main = snap.refs.get("refs/heads/main").expect("main present");
        assert!(main.has_chain);
        assert_eq!(main.bundles.len(), 1);
        assert_eq!(main.bundles[0].sha, SHA_A);
    }

    // --- Malformed bundle keys (#124) ------------------------------------

    #[tokio::test]
    async fn malformed_bundle_stem_recorded_separately() {
        // Push silently filters keys whose stem fails the 40-hex check;
        // doctor must surface them. The valid sibling stays under the
        // ref's bundle list, the malformed sibling does not.
        let mock = MockStore::new();
        mock.insert(
            "myrepo/refs/heads/main/0123456789abcdef0123456789abcdef01234567.bundle",
            Bytes::from("good"),
        );
        mock.insert(
            "myrepo/refs/heads/main/not-a-valid-sha.bundle",
            Bytes::from("junk"),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");

        // The valid bundle is the only entry on the ref.
        let main = snap.refs.get("refs/heads/main").expect("ref recorded");
        assert_eq!(main.bundles.len(), 1);
        assert_eq!(
            main.bundles[0].sha,
            "0123456789abcdef0123456789abcdef01234567"
        );

        // The malformed key surfaces in the top-level vec with both
        // the ref path and the full key (so the operator's `aws s3 rm`
        // command is one copy-paste away).
        assert_eq!(snap.malformed_bundle_keys.len(), 1);
        assert_eq!(snap.malformed_bundle_keys[0].ref_path, "refs/heads/main");
        assert_eq!(
            snap.malformed_bundle_keys[0].key,
            "myrepo/refs/heads/main/not-a-valid-sha.bundle"
        );
    }

    #[tokio::test]
    async fn malformed_only_bundle_does_not_create_phantom_ref_entry() {
        // A ref directory whose only `.bundle` key is malformed must
        // not get an empty `RefSnapshot` (which would render as
        // "No bundles" in the doctor report and confuse the operator
        // about whether anything is actually under the ref). The
        // doctor's malformed-key section is the right place to
        // surface it.
        let mock = MockStore::new();
        mock.insert(
            "myrepo/refs/heads/junk/not-a-valid-sha.bundle",
            Bytes::from("junk"),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");

        assert!(
            !snap.refs.contains_key("refs/heads/junk"),
            "malformed-only refs must not appear in snapshot.refs",
        );
        assert_eq!(snap.malformed_bundle_keys.len(), 1);
    }

    #[tokio::test]
    async fn well_formed_stems_are_never_flagged() {
        // Sanity: the canonical 40-hex stem must NOT be flagged.
        let mock = MockStore::new();
        mock.insert(
            "myrepo/refs/heads/main/0123456789abcdef0123456789abcdef01234567.bundle",
            Bytes::from("body"),
        );
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert!(snap.malformed_bundle_keys.is_empty());
    }

    #[test]
    fn is_bookkeeping_dir_matches_known_prefixes() {
        assert!(super::is_bookkeeping_dir("packs/something.pack"));
        assert!(super::is_bookkeeping_dir("gc/tombstones.json"));
        assert!(super::is_bookkeeping_dir("lfs/abcdef"));
        // Actual ref paths must NOT match.
        assert!(!super::is_bookkeeping_dir("refs/heads/main/abc.bundle"));
        assert!(!super::is_bookkeeping_dir("HEAD"));
        // Prefix-only match: "packs" without trailing slash is not a
        // bookkeeping directory (would be a root-level sidecar).
        assert!(!super::is_bookkeeping_dir("packs"));
    }
}

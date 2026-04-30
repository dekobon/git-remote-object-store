//! Read-only view of a repository's on-bucket layout, built by listing
//! `<prefix>/` and grouping the results into refs, bundles, protection
//! markers, and HEAD. Mirrors the `analyze_repo` step in upstream
//! `../git-remote-s3/git_remote_s3/manage.py`, but flattened to the
//! single-repo case (one CLI invocation == one prefix).

use std::collections::BTreeMap;
use std::sync::Arc;

use time::OffsetDateTime;
use tracing::warn;

use super::ManageError;
use crate::keys;
use crate::object_store::{ObjectMeta, ObjectStore};

/// One bundle object listed under a ref.
#[derive(Debug, Clone)]
pub struct BundleEntry {
    /// Hex-encoded commit OID extracted from the bundle filename.
    /// Stored as `String` (not `Sha`) because the doctor must report
    /// even malformed entries — `<sha>.bundle` keys with non-hex names
    /// still need to be displayed and offered for deletion.
    pub sha: String,
    /// Full object key, used directly for `delete` / `copy` calls so the
    /// caller never has to reconstruct it.
    pub key: String,
    /// Server-side last-modified timestamp.
    pub last_modified: OffsetDateTime,
}

/// Per-ref snapshot — protection state plus every bundle object.
#[derive(Debug, Clone, Default)]
pub struct RefSnapshot {
    /// `true` iff at least one `<ref>/PROTECTED#…` marker is present.
    /// The marker is matched by **prefix**, so any key under `<ref>/`
    /// whose final segment starts with `PROTECTED#` counts.
    pub is_protected: bool,
    /// Bundle objects under this ref, in listing order. The doctor's
    /// "multiple bundles" check fires when this is longer than one.
    pub bundles: Vec<BundleEntry>,
}

/// Whole-repository snapshot.
#[derive(Debug, Clone, Default)]
pub struct RepoSnapshot {
    /// Body of `<prefix>/HEAD`, decoded as UTF-8 and trimmed of
    /// surrounding whitespace. `None` when the object is absent or its
    /// body is not valid UTF-8.
    pub head: Option<String>,
    /// Refs keyed by their full ref-path (e.g. `refs/heads/main`).
    pub refs: BTreeMap<String, RefSnapshot>,
}

impl RepoSnapshot {
    /// `true` iff [`head`](Self::head) names a ref that exists in
    /// [`refs`](Self::refs). A `None` HEAD or a HEAD pointing at a ref
    /// with no listed keys is "invalid" and triggers `fix_head`.
    #[must_use]
    pub fn is_head_valid(&self) -> bool {
        self.head
            .as_ref()
            .is_some_and(|h| self.refs.contains_key(h))
    }
}

/// Walk every object under `<prefix>/` and group it into a
/// [`RepoSnapshot`].
///
/// `prefix` must be the full repository prefix from the parsed remote
/// URL (e.g. `acme/myrepo`), without a trailing `/` — this function
/// appends one to match the upstream listing semantics. An empty
/// `prefix` means "list the entire bucket/container" (root-of-bucket
/// repository) and skips the trailing `/` to avoid emitting a
/// leading-slash list prefix.
///
/// Performs one `list` call. Callers that already have a listing of
/// `<prefix>/` should call [`analyze_objects`] instead to avoid a
/// second LIST round-trip.
///
/// # Errors
///
/// Returns [`ManageError::Store`] if the list or HEAD-object get calls
/// fail.
pub async fn analyze(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> Result<RepoSnapshot, ManageError> {
    let list_prefix = keys::join(prefix, "");
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
pub async fn analyze_objects(
    objects: &[ObjectMeta],
    list_prefix: &str,
    store: &Arc<dyn ObjectStore>,
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
    store: &Arc<dyn ObjectStore>,
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

    let entry = snapshot.refs.entry(ref_path.to_owned()).or_default();
    if last.starts_with("PROTECTED#") {
        entry.is_protected = true;
    } else if let Some(sha) = last.strip_suffix(".bundle") {
        entry.bundles.push(BundleEntry {
            sha: sha.to_owned(),
            key: object.key.clone(),
            last_modified: object.last_modified,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::ObjectStore;
    use crate::object_store::mock::MockStore;
    use bytes::Bytes;

    fn store() -> Arc<dyn ObjectStore> {
        Arc::new(MockStore::new())
    }

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
        mock.insert("myrepo/refs/heads/main/abc123.bundle", Bytes::from("body"));
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        let main = snap.refs.get("refs/heads/main").expect("main present");
        assert_eq!(main.bundles.len(), 1);
        assert_eq!(main.bundles[0].sha, "abc123");
        assert_eq!(main.bundles[0].key, "myrepo/refs/heads/main/abc123.bundle");
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
    async fn protected_marker_prefix_match() {
        // §1.1: PROTECTED# is matched by prefix, not exact equality.
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/PROTECTED#tag", Bytes::new());
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert!(snap.refs["refs/heads/main"].is_protected);
    }

    #[tokio::test]
    async fn head_object_is_decoded_and_trimmed() {
        let mock = MockStore::new();
        mock.insert("myrepo/HEAD", Bytes::from("refs/heads/main\n"));
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("body"));
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
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("body"));
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert_eq!(snap.head.as_deref(), Some("refs/heads/missing"));
        assert!(!snap.is_head_valid());
    }

    #[tokio::test]
    async fn multiple_bundles_under_one_ref() {
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/aaa.bundle", Bytes::from("a"));
        mock.insert("myrepo/refs/heads/main/bbb.bundle", Bytes::from("b"));
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        let shas: std::collections::BTreeSet<&str> = snap.refs["refs/heads/main"]
            .bundles
            .iter()
            .map(|b| b.sha.as_str())
            .collect();
        assert_eq!(shas, ["aaa", "bbb"].into_iter().collect());
    }

    #[tokio::test]
    async fn lock_files_are_skipped_in_ref_grouping() {
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/LOCK#.lock", Bytes::new());
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("b"));
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert_eq!(snap.refs["refs/heads/main"].bundles.len(), 1);
        assert!(!snap.refs["refs/heads/main"].is_protected);
    }

    #[tokio::test]
    async fn repo_zip_is_skipped_in_ref_grouping() {
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/main/repo.zip", Bytes::from("zip"));
        mock.insert("myrepo/refs/heads/main/abc.bundle", Bytes::from("b"));
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        assert_eq!(snap.refs["refs/heads/main"].bundles.len(), 1);
    }

    #[tokio::test]
    async fn nested_ref_path_is_preserved() {
        let mock = MockStore::new();
        mock.insert("myrepo/refs/heads/feature/x/aaa.bundle", Bytes::from("a"));
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "myrepo").await.expect("analyze");
        let entry = snap
            .refs
            .get("refs/heads/feature/x")
            .expect("nested ref recorded");
        assert_eq!(entry.bundles.len(), 1);
        assert_eq!(entry.bundles[0].sha, "aaa");
        assert_eq!(
            entry.bundles[0].key,
            "myrepo/refs/heads/feature/x/aaa.bundle"
        );
    }

    #[tokio::test]
    async fn root_prefix_lists_bucket_without_leading_slash() {
        // Empty prefix == repository at the bucket root. The on-bucket
        // layout drops the leading `<prefix>/` segment entirely.
        let mock = MockStore::new();
        mock.insert("HEAD", Bytes::from("refs/heads/main"));
        mock.insert("refs/heads/main/abc.bundle", Bytes::from("body"));
        mock.insert("refs/heads/main/PROTECTED#", Bytes::new());
        let s: Arc<dyn ObjectStore> = Arc::new(mock);
        let snap = analyze(&s, "").await.expect("analyze at root");
        assert_eq!(snap.head.as_deref(), Some("refs/heads/main"));
        let main = snap.refs.get("refs/heads/main").expect("main present");
        assert_eq!(main.bundles.len(), 1);
        assert_eq!(main.bundles[0].sha, "abc");
        assert_eq!(main.bundles[0].key, "refs/heads/main/abc.bundle");
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
}

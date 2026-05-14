//! `list` handler for the packchain engine (issue #72).
//!
//! Bundle's `list` reads `<prefix>/refs/heads/<branch>/<sha>.bundle`
//! keys and reports the bundle filename's SHA as the ref tip. For
//! packchain that filename's SHA is the **baseline tip** (`full_at`),
//! not the **current tip** — after any incremental push, `chain.tip`
//! advances while the baseline bundle stays at `full_at.bundle`. So
//! the bundle handler returns stale tips on packchain remotes,
//! breaking `git ls-remote`, `git fetch`, and `git pull`.
//!
//! This module's [`list_refs`] reads `<prefix>/refs/**/chain.json`
//! across every ref namespace (`refs/heads/`, `refs/tags/`,
//! `refs/notes/`, etc.) and reports the parsed `chain.tip` per ref —
//! the actual current tip of each ref. The wire format the protocol
//! layer emits is unchanged.
//!
//! ## Failure modes
//!
//! Transport / list errors abort with [`PackchainError::Store`]
//! (the protocol layer surfaces this as `ListError`). Per-entry
//! parse failures (corrupt `chain.json`, unsupported schema
//! version, invalid sha) **skip with a `tracing::warn!`** rather
//! than aborting — a single corrupt branch shouldn't blackhole
//! every other branch's ref discovery. Operators see the warning
//! in stderr and can run `doctor` / `gc` to investigate.

use futures::stream::{StreamExt, TryStreamExt};
use time::OffsetDateTime;
use tracing::warn;

use crate::git::RefName;
use crate::keys;
use crate::object_store::{ObjectStore, ObjectStoreError};
use crate::protocol::fetch::MAX_FETCH_CONCURRENCY;

use super::PackchainError;
use super::schema::ChainManifest;

/// One listed ref's parsed parts. Engine-neutral fields the
/// protocol layer renders into `<sha> <ref>\n` lines.
#[derive(Debug, Clone)]
pub(crate) struct ChainRef {
    /// Current tip SHA — `chain.tip`, **not** the baseline
    /// `full_at` filename SHA.
    pub(crate) sha: String,
    /// Full ref path (`refs/heads/<branch>`, `refs/tags/<tag>`,
    /// `refs/notes/<name>`, …).
    pub(crate) ref_path: String,
    /// `chain.json`'s `last_modified`. The protocol layer sorts
    /// newest-first across refs for parity with bundle's
    /// LastModified-desc behaviour.
    pub(crate) last_modified: OffsetDateTime,
}

/// List every packchain ref under `<prefix>/refs/` across every ref
/// namespace (`refs/heads/`, `refs/tags/`, `refs/notes/`, etc.).
///
/// Returns an empty `Vec` for an empty bucket or a bucket that has
/// no `chain.json` files (e.g. a freshly-`FORMAT`ed packchain bucket
/// that nobody has pushed to yet).
///
/// # Errors
///
/// Returns [`PackchainError::Store`] for transport failures on the
/// initial list call or any per-entry chain.json fetch. Per-entry
/// JSON-parse failures are logged at `warn` and the entry is
/// skipped — they do not abort the listing.
pub(crate) async fn list_refs(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
) -> Result<Vec<ChainRef>, PackchainError> {
    let refs_prefix = keys::join(prefix, "refs/");
    let metas = store.list(&refs_prefix).await?;

    // Two-phase: first filter and validate ref names synchronously,
    // then fetch+parse the chain.json bodies in parallel. The
    // validation step rejects keys whose extracted ref path fails
    // `gix-validate`'s ref-name check — a maliciously-planted key
    // like `<prefix>/refs/heads/../etc/passwd/chain.json` would
    // otherwise emit a bogus ref name to git in the list response.
    // The same hardening applies to bundle's `parse_bundle_key`
    // (see #72 review notes); centralising it here keeps the
    // packchain-side scan tight.
    let candidates: Vec<(String, String, OffsetDateTime)> = metas
        .into_iter()
        .filter_map(|m| {
            if !super::keys::is_chain_json_key(&m.key) {
                return None;
            }
            let Some(ref_path) = super::keys::ref_path_from_chain_key(prefix, &m.key) else {
                warn!(key = %m.key, "packchain list: chain.json key has unexpected shape; skipping");
                return None;
            };
            if !RefName::is_valid(&ref_path) {
                warn!(
                    key = %m.key,
                    ref_path = %ref_path,
                    "packchain list: derived ref path is not a valid ref name; skipping",
                );
                return None;
            }
            Some((m.key, ref_path, m.last_modified))
        })
        .collect();

    // Bounded-parallel `get_bytes` per chain.json. `MAX_FETCH_CONCURRENCY`
    // (= 8) matches the limit Phase 3 fetch already uses for chain
    // pack downloads. `buffer_unordered` doesn't preserve order, but
    // we re-sort by `last_modified` desc afterwards anyway.
    //
    // A `NotFound` on any single GET is treated as benign: a concurrent
    // `delete-branch` (or compact/gc sweep) may remove a ref's
    // `chain.json` between the `store.list()` snapshot and the
    // per-entry GET. Without this carve-out, a single missing key
    // would abort discovery for every OTHER ref the operator
    // legitimately holds (issue #149). All other transport / auth
    // failures still abort — a 403/AccessDenied or DNS error means
    // the listing itself is untrustworthy, not just one ref. Parse
    // failures are warn-and-skipped per entry below — a corrupt
    // chain.json on one branch must not blackhole the others.
    let bodies: Vec<(String, String, OffsetDateTime, bytes::Bytes)> =
        futures::stream::iter(candidates)
            .map(|(key, ref_path, last_modified)| async move {
                match store.get_bytes(&key).await {
                    Ok(body) => Ok(Some((key, ref_path, last_modified, body))),
                    Err(ObjectStoreError::NotFound(_)) => {
                        warn!(
                            key = %key,
                            "packchain list: chain.json vanished between list and get \
                             (concurrent delete?); skipping",
                        );
                        Ok(None)
                    }
                    Err(e) => Err(PackchainError::Store(e)),
                }
            })
            .buffer_unordered(MAX_FETCH_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .flatten()
            .collect();

    let mut out: Vec<ChainRef> = bodies
        .into_iter()
        .filter_map(|(key, ref_path, last_modified, body)| {
            match ChainManifest::from_json_bytes(&body) {
                Ok(chain) => Some(ChainRef {
                    // `Sha40: From<Sha40> for String` consumes
                    // without an intermediate `&str` round-trip.
                    sha: chain.tip.into(),
                    ref_path,
                    last_modified,
                }),
                Err(e) => {
                    warn!(
                        key = %key,
                        error = %e,
                        "packchain list: chain.json failed to parse; skipping ref",
                    );
                    None
                }
            }
        })
        .collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.last_modified));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RefName;
    use crate::object_store::mock::MockStore;
    use crate::packchain::manifest::write_chain;
    use crate::packchain::schema::{ChainSegment, Sha40};
    use bytes::Bytes;

    const SHA_TIP: &str = "0000000000000000000000000000000000000001";
    const SHA_FULL: &str = "0000000000000000000000000000000000000002";
    const SHA_PACK: &str = "1111111111111111111111111111111111111111";
    const SHA_TIP_DEV: &str = "0000000000000000000000000000000000000003";

    fn sha40(s: &str) -> Sha40 {
        Sha40::try_new(s).unwrap()
    }

    fn ref_(name: &str) -> RefName {
        RefName::new(name).unwrap()
    }

    async fn write_test_chain(
        store: &MockStore,
        prefix: Option<&str>,
        ref_name: &RefName,
        tip: &str,
        full_at: &str,
    ) {
        let chain = ChainManifest {
            v: 1,
            tip: sha40(tip),
            full_at: sha40(full_at),
            segments: vec![ChainSegment {
                sha: sha40(tip),
                parent_sha: None,
                pack: format!("packs/{SHA_PACK}.pack"),
                bytes: 1_024,
            }],
        };
        write_chain(store, prefix, ref_name, &chain).await.unwrap();
    }

    #[tokio::test]
    async fn list_refs_returns_chain_tip_not_full_at_after_incremental_push() {
        // Pin the bug fix: `tip != full_at` (the post-incremental-push
        // shape) must surface as `tip` — not the baseline filename SHA
        // a bundle-style listing would produce.
        let store = MockStore::new();
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/heads/main"),
            SHA_TIP,
            SHA_FULL,
        )
        .await;
        let entries = list_refs(&store, Some("repo")).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sha, SHA_TIP, "must report chain.tip");
        assert_ne!(entries[0].sha, SHA_FULL, "must NOT report full_at");
        assert_eq!(entries[0].ref_path, "refs/heads/main");
    }

    #[tokio::test]
    async fn list_refs_empty_bucket_returns_empty_vec() {
        let store = MockStore::new();
        let entries = list_refs(&store, Some("repo")).await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn list_refs_collects_multiple_branches() {
        let store = MockStore::new();
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/heads/main"),
            SHA_TIP,
            SHA_FULL,
        )
        .await;
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/heads/dev"),
            SHA_TIP_DEV,
            SHA_TIP_DEV,
        )
        .await;
        let entries = list_refs(&store, Some("repo")).await.unwrap();
        let by_ref: std::collections::HashMap<_, _> = entries
            .iter()
            .map(|e| (e.ref_path.clone(), e.sha.clone()))
            .collect();
        assert_eq!(
            by_ref.get("refs/heads/main").map(String::as_str),
            Some(SHA_TIP)
        );
        assert_eq!(
            by_ref.get("refs/heads/dev").map(String::as_str),
            Some(SHA_TIP_DEV),
        );
    }

    #[tokio::test]
    async fn list_refs_handles_nested_branch_names() {
        // Branches like `refs/heads/feature/foo` produce a key
        // `<prefix>/refs/heads/feature/foo/chain.json`. Both the ref
        // path AND the parsed `chain.tip` must come back intact —
        // checking only the ref path would miss a regression that
        // nested-name keys parsed but extracted the wrong sha.
        let store = MockStore::new();
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/heads/feature/foo"),
            SHA_TIP,
            SHA_TIP,
        )
        .await;
        let entries = list_refs(&store, Some("repo")).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ref_path, "refs/heads/feature/foo");
        assert_eq!(entries[0].sha, SHA_TIP);
    }

    #[tokio::test]
    async fn list_refs_works_at_bucket_root_with_no_prefix() {
        let store = MockStore::new();
        write_test_chain(&store, None, &ref_("refs/heads/main"), SHA_TIP, SHA_TIP).await;
        let entries = list_refs(&store, None).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ref_path, "refs/heads/main");
        assert_eq!(entries[0].sha, SHA_TIP);
    }

    #[tokio::test]
    async fn list_refs_skips_corrupt_chain_json_with_warning() {
        // A corrupt chain.json on one branch must not blackhole
        // discovery for the others.
        let store = MockStore::new();
        // Good branch.
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/heads/main"),
            SHA_TIP,
            SHA_TIP,
        )
        .await;
        // Corrupt branch.
        store.insert(
            "repo/refs/heads/broken/chain.json",
            Bytes::from_static(b"{not valid json"),
        );
        let entries = list_refs(&store, Some("repo")).await.unwrap();
        assert_eq!(entries.len(), 1, "corrupt branch must be skipped");
        assert_eq!(entries[0].ref_path, "refs/heads/main");
    }

    #[tokio::test]
    async fn list_refs_skips_unsupported_schema_version() {
        let store = MockStore::new();
        store.insert(
            "repo/refs/heads/future/chain.json",
            Bytes::from_static(
                br#"{"v":2,"tip":"0000000000000000000000000000000000000001","full_at":"0000000000000000000000000000000000000001","segments":[]}"#,
            ),
        );
        let entries = list_refs(&store, Some("repo")).await.unwrap();
        assert!(
            entries.is_empty(),
            "unsupported schema must skip rather than abort",
        );
    }

    #[tokio::test]
    async fn list_refs_orders_newest_chain_first() {
        // Pin last-modified-desc ordering. MockStore's `list` returns
        // metas in **alphabetical key order** (BTreeMap range), so
        // the chosen ref names must make alphabetical order
        // **disagree** with last-modified-desc — otherwise removing
        // the production `sort_by_key` would not fail this test
        // (the bug the original test had).
        //
        // Insert `dev` first (older `last_modified`), then `main`
        // (newer). Production sorts last-modified desc → `main`
        // leads. Alphabetical order would lead with `dev`. The two
        // disagree, so the assertion now actually pins the sort.
        // Mutation-verified: removing the production `sort_by_key`
        // makes this test fail.
        let store = MockStore::new();
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/heads/dev"),
            SHA_TIP_DEV,
            SHA_TIP_DEV,
        )
        .await;
        // Yield long enough that the wall-clock of the second insert
        // is after the first; MockStore stamps OffsetDateTime::now_utc
        // at insert time.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/heads/main"),
            SHA_TIP,
            SHA_TIP,
        )
        .await;
        let entries = list_refs(&store, Some("repo")).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].ref_path, "refs/heads/main",
            "newest chain.json (last_modified desc) must come first, \
             even though `dev` is alphabetically earlier",
        );
        assert_eq!(entries[1].ref_path, "refs/heads/dev");
    }

    #[test]
    fn ref_path_from_chain_key_strips_prefix_and_suffix() {
        assert_eq!(
            super::super::keys::ref_path_from_chain_key(
                Some("repo"),
                "repo/refs/heads/main/chain.json"
            ),
            Some("refs/heads/main".to_owned()),
        );
    }

    #[test]
    fn ref_path_from_chain_key_handles_no_prefix() {
        assert_eq!(
            super::super::keys::ref_path_from_chain_key(None, "refs/heads/main/chain.json"),
            Some("refs/heads/main".to_owned()),
        );
        assert_eq!(
            super::super::keys::ref_path_from_chain_key(Some(""), "refs/heads/main/chain.json"),
            Some("refs/heads/main".to_owned()),
        );
    }

    #[test]
    fn ref_path_from_chain_key_returns_none_for_unrelated_key() {
        // Sibling-prefix collision: bucket has another repo at
        // `repo-other/`. We should not match its chain.json keys.
        assert_eq!(
            super::super::keys::ref_path_from_chain_key(
                Some("repo"),
                "repo-other/refs/heads/main/chain.json"
            ),
            None,
        );
    }

    #[tokio::test]
    async fn list_refs_skips_chain_json_with_path_traversal_in_ref_name() {
        // Defense-in-depth (review S1): a maliciously-planted key
        // like `<prefix>/refs/heads/../etc/passwd/chain.json` would
        // otherwise yield ref path `refs/heads/../etc/passwd` and
        // emit it to git in the list response. The `RefName::new`
        // filter rejects ref names containing `..` (per
        // `gix-validate`'s rules), so the entry is skipped with
        // a warn rather than included.
        let store = MockStore::new();
        // Good branch alongside the malicious key — pin that the
        // good one is still listed.
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/heads/main"),
            SHA_TIP,
            SHA_TIP,
        )
        .await;
        store.insert(
            "repo/refs/heads/../etc/passwd/chain.json",
            Bytes::from(
                format!(r#"{{"v":1,"tip":"{SHA_TIP}","full_at":"{SHA_TIP}","segments":[]}}"#)
                    .into_bytes(),
            ),
        );
        let entries = list_refs(&store, Some("repo")).await.unwrap();
        assert_eq!(
            entries.len(),
            1,
            "malicious ref path must be filtered before emission",
        );
        assert_eq!(entries[0].ref_path, "refs/heads/main");
        assert!(
            !entries.iter().any(|e| e.ref_path.contains("..")),
            "no entry with `..` in ref_path may reach the list output",
        );
    }

    #[tokio::test]
    async fn list_refs_surfaces_tag_chain() {
        // Regression for issue #82: chains under `refs/tags/` were
        // invisible to the helper-protocol `list` command before the
        // listing prefix was widened from `refs/heads/` to `refs/`.
        let store = MockStore::new();
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/tags/v1"),
            SHA_TIP,
            SHA_TIP,
        )
        .await;
        let entries = list_refs(&store, Some("repo")).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ref_path, "refs/tags/v1");
        assert_eq!(entries[0].sha, SHA_TIP);
    }

    #[tokio::test]
    async fn list_refs_surfaces_notes_chain() {
        // `refs/notes/commits` is the canonical git-notes ref. It
        // must surface alongside heads and tags.
        let store = MockStore::new();
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/notes/commits"),
            SHA_TIP,
            SHA_TIP,
        )
        .await;
        let entries = list_refs(&store, Some("repo")).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].ref_path, "refs/notes/commits");
        assert_eq!(entries[0].sha, SHA_TIP);
    }

    #[tokio::test]
    async fn list_refs_collects_chains_from_mixed_namespaces() {
        // Heads, tags, and notes coexisting under one prefix must
        // all appear in the listing with their correct tips.
        let store = MockStore::new();
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/heads/main"),
            SHA_TIP,
            SHA_FULL,
        )
        .await;
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/tags/v1"),
            SHA_TIP_DEV,
            SHA_TIP_DEV,
        )
        .await;
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/notes/commits"),
            SHA_FULL,
            SHA_FULL,
        )
        .await;
        let entries = list_refs(&store, Some("repo")).await.unwrap();
        let by_ref: std::collections::HashMap<_, _> = entries
            .iter()
            .map(|e| (e.ref_path.clone(), e.sha.clone()))
            .collect();
        assert_eq!(by_ref.len(), 3, "all three namespaces must appear");
        assert_eq!(
            by_ref.get("refs/heads/main").map(String::as_str),
            Some(SHA_TIP),
        );
        assert_eq!(
            by_ref.get("refs/tags/v1").map(String::as_str),
            Some(SHA_TIP_DEV),
        );
        assert_eq!(
            by_ref.get("refs/notes/commits").map(String::as_str),
            Some(SHA_FULL),
        );
    }

    #[tokio::test]
    async fn list_refs_ignores_non_chain_siblings_under_tag_namespace() {
        // The same sibling-artefact discipline that protects
        // `refs/heads/` must apply to every namespace the widened
        // prefix now covers. A `path-index.json` or `<sha>.bundle`
        // sitting under `refs/tags/v1/` must not surface as a ref.
        let store = MockStore::new();
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/tags/v1"),
            SHA_TIP,
            SHA_TIP,
        )
        .await;
        store.insert(
            "repo/refs/tags/v1/path-index.json",
            Bytes::from(format!(r#"{{"v":1,"commit":"{SHA_TIP}","tree":{{}}}}"#).into_bytes()),
        );
        store.insert(
            format!("repo/refs/tags/v1/{SHA_TIP}.bundle"),
            Bytes::from_static(b"baseline"),
        );

        let entries = list_refs(&store, Some("repo")).await.unwrap();
        assert_eq!(entries.len(), 1, "exactly one chain.json processed");
        assert_eq!(entries[0].ref_path, "refs/tags/v1");
        assert_eq!(entries[0].sha, SHA_TIP);
    }

    /// One-shot post-`list` callback used by [`PostListDeleteStore`].
    type PostListHook = Box<dyn FnOnce(&MockStore) + Send>;

    /// One-shot post-`list` decorator that fires after the inner
    /// `list()` returns and before any subsequent `get_bytes`. Used to
    /// simulate a concurrent `delete-branch` removing one ref's
    /// `chain.json` in the gap between `list_refs`' list snapshot and
    /// its parallel GET phase (issue #149).
    struct PostListDeleteStore {
        inner: MockStore,
        hook: std::sync::Mutex<Option<PostListHook>>,
    }

    impl PostListDeleteStore {
        fn new(inner: MockStore, hook: impl FnOnce(&MockStore) + Send + 'static) -> Self {
            Self {
                inner,
                hook: std::sync::Mutex::new(Some(Box::new(hook))),
            }
        }
    }

    crate::delegate_to_inner_impl! {
        impl ObjectStore for PostListDeleteStore {
            forward: get_to_file, get_bytes, get_bytes_range,
                     put_bytes, put_path, put_if_absent,
                     head, copy, delete;

            async fn list(
                &self,
                prefix: &str,
            ) -> Result<Vec<crate::object_store::ObjectMeta>, ObjectStoreError> {
                let result = self.inner.list(prefix).await;
                if result.is_ok()
                    && let Some(hook) = self.hook.lock().unwrap().take()
                {
                    hook(&self.inner);
                }
                result
            }
        }
    }

    #[tokio::test]
    async fn list_refs_skips_chain_json_vanished_between_list_and_get() {
        // Issue #149 regression: a concurrent `delete-branch` that
        // removes a ref's `chain.json` between `store.list()` and the
        // parallel `get_bytes()` phase must not abort discovery for
        // the surviving refs. Pre-fix, `try_collect` propagated the
        // `NotFound` and killed the listing for ALL refs.
        //
        // Layout: two refs (main, dev). The hook fires AFTER `list()`
        // returns both keys but BEFORE the parallel GETs run, deleting
        // `dev`'s chain.json. The fix's `NotFound` carve-out warns and
        // skips `dev`; `main` must still surface with its real tip.
        let inner = MockStore::new();
        write_test_chain(
            &inner,
            Some("repo"),
            &ref_("refs/heads/main"),
            SHA_TIP,
            SHA_TIP,
        )
        .await;
        write_test_chain(
            &inner,
            Some("repo"),
            &ref_("refs/heads/dev"),
            SHA_TIP_DEV,
            SHA_TIP_DEV,
        )
        .await;

        let dev_key = "repo/refs/heads/dev/chain.json";
        let store = PostListDeleteStore::new(inner, move |inner| {
            assert!(
                inner.remove_key(dev_key),
                "concurrent delete must remove the targeted chain.json",
            );
        });

        let entries = list_refs(&store, Some("repo"))
            .await
            .expect("NotFound on one ref must not abort the listing");
        assert_eq!(
            entries.len(),
            1,
            "surviving ref must still surface; vanished ref is silently skipped",
        );
        assert_eq!(
            entries[0].ref_path, "refs/heads/main",
            "main must be the surviving entry — dev's chain.json was deleted mid-list",
        );
        assert_eq!(entries[0].sha, SHA_TIP);
    }

    #[tokio::test]
    async fn list_refs_ignores_path_index_and_baseline_bundle_siblings() {
        // Defensive test (review T1): a real packchain bucket has
        // chain.json sitting alongside `path-index.json` and
        // `<sha>.bundle` keys under the same `refs/heads/<branch>/`
        // directory (Phase 2 push writes all three). The
        // `ends_with(b"/chain.json")` filter must skip the siblings.
        // A regression that broadened the filter (e.g. to
        // `.json`) would parse `path-index.json` as `ChainManifest`
        // and warn-and-skip silently — still passing this test only
        // because we assert the *exact* output shape and count.
        let store = MockStore::new();
        write_test_chain(
            &store,
            Some("repo"),
            &ref_("refs/heads/main"),
            SHA_TIP,
            SHA_TIP,
        )
        .await;
        // Sibling: a plausible path-index.json body.
        store.insert(
            "repo/refs/heads/main/path-index.json",
            Bytes::from(format!(r#"{{"v":1,"commit":"{SHA_TIP}","tree":{{}}}}"#).into_bytes()),
        );
        // Sibling: baseline bundle.
        store.insert(
            format!("repo/refs/heads/main/{SHA_TIP}.bundle"),
            Bytes::from_static(b"baseline"),
        );

        let entries = list_refs(&store, Some("repo")).await.unwrap();
        assert_eq!(entries.len(), 1, "exactly one chain.json processed");
        assert_eq!(entries[0].ref_path, "refs/heads/main");
        assert_eq!(entries[0].sha, SHA_TIP);
    }
}

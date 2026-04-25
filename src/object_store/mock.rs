//! In-memory [`ObjectStore`] used by every higher-layer test in
//! Phases 5–9.
//!
//! Keeps push, fetch, locking, doctor, and LFS logic exercisable without a
//! MinIO/Azurite container. Production builds do not see this module — it
//! is gated on `cfg(any(test, feature = "test-util"))`. Crate-internal unit
//! tests pick it up via `cfg(test)`; integration tests in `tests/` opt in
//! by enabling the `test-util` Cargo feature.
//!
//! The backing store is a `BTreeMap<String, MockObject>` so [`list`]'s
//! iteration is deterministic (lexicographic). Prefix matching mirrors S3
//! `Prefix=` byte-prefix semantics — `list("a")` returns `a`, `a/1`, and
//! `aaa` — see the §1.1 wire-format invariants in `execution-plan.md`.
//!
//! Fault injection is FIFO: tests call [`MockStore::arm`] to queue a
//! [`Fault`]; the next matching operation consumes it and returns the
//! corresponding [`Error`]. This drives Phase 8's stale-lock recovery and
//! similar error-path tests deterministically.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use time::OffsetDateTime;

use super::{Error, ObjectMeta, ObjectStore, PutOpts};

/// Single-call fault recipes consumed FIFO by [`MockStore`].
///
/// Each variant matches one trait method + key/prefix; when the matching
/// call fires, the variant is removed from the queue and the listed error
/// is returned.
#[derive(Debug, Clone)]
pub enum Fault {
    /// Force `put_if_absent(key, _)` to return [`Error::PreconditionFailed`]
    /// without writing.
    PreconditionFailedOnPutIfAbsent {
        /// Key being written.
        key: String,
    },
    /// Force `head(key)` to return [`Error::NotFound`].
    NotFoundOnHead {
        /// Key being inspected.
        key: String,
    },
    /// Force `get_bytes(key)` to return [`Error::Network`].
    NetworkOnGetBytes {
        /// Key being read.
        key: String,
    },
    /// Force `list(prefix)` to return [`Error::AccessDenied`].
    AccessDeniedOnList {
        /// Prefix being listed.
        prefix: String,
    },
}

#[derive(Debug, Clone)]
struct MockObject {
    body: Bytes,
    last_modified: OffsetDateTime,
    content_disposition: Option<String>,
    user_metadata: Vec<(String, String)>,
}

#[derive(Default)]
struct MockState {
    objects: BTreeMap<String, MockObject>,
    faults: Vec<Fault>,
}

/// In-memory [`ObjectStore`] for tests.
///
/// `Clone` is cheap — the backing `Arc<Mutex<…>>` is shared, so all clones
/// observe the same state. Instances are `Send + Sync`.
#[derive(Default, Clone)]
pub struct MockStore {
    inner: Arc<Mutex<MockState>>,
}

impl MockStore {
    /// Build an empty store with no faults armed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue `fault` to fire on the next matching operation.
    pub fn arm(&self, fault: Fault) {
        self.with_state(|s| s.faults.push(fault));
    }

    /// Seed the store with `body` under `key`, stamping `last_modified` to
    /// "now". Existing entries at `key` are overwritten.
    pub fn insert(&self, key: impl Into<String>, body: impl Into<Bytes>) {
        self.insert_with(key, body, OffsetDateTime::now_utc(), PutOpts::default());
    }

    /// Seed with explicit `last_modified` and metadata. Stale-lock recovery
    /// tests use this to back-date a lock object so the staleness check
    /// fires.
    pub fn insert_with(
        &self,
        key: impl Into<String>,
        body: impl Into<Bytes>,
        last_modified: OffsetDateTime,
        opts: PutOpts,
    ) {
        let key = key.into();
        let object = MockObject {
            body: body.into(),
            last_modified,
            content_disposition: opts.content_disposition,
            user_metadata: opts.user_metadata,
        };
        self.with_state(|s| {
            s.objects.insert(key, object);
        });
    }

    /// Snapshot of every key currently stored, sorted lex.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.with_state(|s| s.objects.keys().cloned().collect())
    }

    /// `true` if `key` is present.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.with_state(|s| s.objects.contains_key(key))
    }

    /// Number of armed faults that have not yet fired. Tests assert this
    /// is `0` to catch typos in fault keys.
    #[must_use]
    pub fn pending_faults(&self) -> usize {
        self.with_state(|s| s.faults.len())
    }

    /// Read back the [`PutOpts`] previously stored under `key`. `None` if
    /// the key is absent. Used by tests that round-trip metadata — the
    /// trait does not surface metadata on `head` (yet).
    #[must_use]
    pub fn metadata(&self, key: &str) -> Option<PutOpts> {
        self.with_state(|s| {
            s.objects.get(key).map(|o| PutOpts {
                content_disposition: o.content_disposition.clone(),
                user_metadata: o.user_metadata.clone(),
            })
        })
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut MockState) -> R) -> R {
        let mut guard = self.inner.lock().expect("mock mutex poisoned");
        f(&mut guard)
    }

    /// Pop the first fault for which `map` returns `Some(err)` and bubble
    /// that error out. The closure runs twice on the matching fault — once
    /// to locate it, once to extract the error — but the queue is tiny in
    /// tests, so the duplicated match is cheaper than threading the
    /// destructured payload through the callsite.
    fn check_fault(
        state: &mut MockState,
        map: impl Fn(&Fault) -> Option<Error>,
    ) -> Result<(), Error> {
        let Some(position) = state.faults.iter().position(|f| map(f).is_some()) else {
            return Ok(());
        };
        Err(map(&state.faults.remove(position)).expect("position guarantees match"))
    }
}

/// Lexicographic successor of `prefix`, used as the exclusive upper bound
/// for byte-prefix range queries on a `BTreeMap<String, _>`.
///
/// Returns `Bound::Unbounded` for the empty prefix and for prefixes that
/// are entirely `0xFF` bytes (no successor exists in the byte-string
/// order). For any other prefix, returns `Bound::Excluded(succ)`, where
/// `succ` is `prefix` with the last non-`0xFF` byte incremented and any
/// trailing `0xFF` bytes truncated.
fn next_lex(prefix: &str) -> Bound<String> {
    let bytes = prefix.as_bytes();
    let Some(pivot) = bytes.iter().rposition(|&b| b != 0xFF) else {
        return Bound::Unbounded;
    };
    let mut next = bytes[..=pivot].to_vec();
    next[pivot] += 1;
    // The increment may produce an invalid UTF-8 byte; fall back to an
    // unbounded upper if so. In the wire-format-invariant key space
    // (`<prefix>/<ref>/...`) this never fires because every legal prefix
    // ends with a printable ASCII byte.
    String::from_utf8(next).map_or(Bound::Unbounded, Bound::Excluded)
}

#[async_trait]
impl ObjectStore for MockStore {
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, Error> {
        self.with_state(|s| {
            Self::check_fault(s, |f| match f {
                Fault::AccessDeniedOnList { prefix: p } if p == prefix => {
                    Some(Error::AccessDenied(p.clone()))
                }
                _ => None,
            })?;
            let bounds = (Bound::Included(prefix.to_string()), next_lex(prefix));
            Ok(s.objects
                .range(bounds)
                .map(|(key, object)| ObjectMeta {
                    key: key.clone(),
                    size: object.body.len() as u64,
                    last_modified: object.last_modified,
                })
                .collect())
        })
    }

    async fn get_to_file(&self, key: &str, dest: &Path) -> Result<(), Error> {
        let bytes = self.get_bytes(key).await?;
        tokio::fs::write(dest, &bytes)
            .await
            .map_err(|e| Error::Other(Box::new(e)))
    }

    async fn get_bytes(&self, key: &str) -> Result<Bytes, Error> {
        self.with_state(|s| {
            Self::check_fault(s, |f| match f {
                Fault::NetworkOnGetBytes { key: k } if k == key => Some(Error::Network(Box::new(
                    std::io::Error::other(format!("mock network: {k}")),
                ))),
                _ => None,
            })?;
            s.objects
                .get(key)
                .map(|o| o.body.clone())
                .ok_or_else(|| Error::NotFound(key.to_string()))
        })
    }

    async fn put_bytes(&self, key: &str, body: Bytes, opts: PutOpts) -> Result<(), Error> {
        self.insert_with(key, body, OffsetDateTime::now_utc(), opts);
        Ok(())
    }

    async fn put_if_absent(&self, key: &str, body: Bytes) -> Result<bool, Error> {
        self.with_state(|s| {
            Self::check_fault(s, |f| match f {
                Fault::PreconditionFailedOnPutIfAbsent { key: k } if k == key => {
                    Some(Error::PreconditionFailed(k.clone()))
                }
                _ => None,
            })?;
            if s.objects.contains_key(key) {
                return Ok(false);
            }
            s.objects.insert(
                key.to_string(),
                MockObject {
                    body,
                    last_modified: OffsetDateTime::now_utc(),
                    content_disposition: None,
                    user_metadata: Vec::new(),
                },
            );
            Ok(true)
        })
    }

    async fn head(&self, key: &str) -> Result<ObjectMeta, Error> {
        self.with_state(|s| {
            Self::check_fault(s, |f| match f {
                Fault::NotFoundOnHead { key: k } if k == key => Some(Error::NotFound(k.clone())),
                _ => None,
            })?;
            s.objects
                .get(key)
                .map(|o| ObjectMeta {
                    key: key.to_string(),
                    size: o.body.len() as u64,
                    last_modified: o.last_modified,
                })
                .ok_or_else(|| Error::NotFound(key.to_string()))
        })
    }

    async fn copy(&self, src: &str, dst: &str) -> Result<(), Error> {
        self.with_state(|s| {
            let mut copied = s
                .objects
                .get(src)
                .cloned()
                .ok_or_else(|| Error::NotFound(src.to_string()))?;
            // Copy gets a fresh server-side timestamp, matching S3's
            // copy_object semantics.
            copied.last_modified = OffsetDateTime::now_utc();
            s.objects.insert(dst.to_string(), copied);
            Ok(())
        })
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.with_state(|s| {
            s.objects
                .remove(key)
                .map(|_| ())
                .ok_or_else(|| Error::NotFound(key.to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn body(data: &[u8]) -> Bytes {
        Bytes::copy_from_slice(data)
    }

    #[tokio::test]
    async fn put_then_get_round_trips_bytes_and_size() {
        let store = MockStore::new();
        store
            .put_bytes("k", body(b"hello"), PutOpts::default())
            .await
            .unwrap();

        let got = store.get_bytes("k").await.unwrap();
        assert_eq!(&got[..], b"hello");

        let meta = store.head("k").await.unwrap();
        assert_eq!(meta.key, "k");
        assert_eq!(meta.size, 5);
    }

    #[tokio::test]
    async fn list_uses_byte_prefix_semantics() {
        let store = MockStore::new();
        for key in ["a", "a/1", "a/2", "aaa", "b/1"] {
            store.insert(key, body(b""));
        }

        let under_a = store.list("a").await.unwrap();
        let keys: Vec<&str> = under_a.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, vec!["a", "a/1", "a/2", "aaa"]);

        let under_a_slash = store.list("a/").await.unwrap();
        let keys: Vec<&str> = under_a_slash.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, vec!["a/1", "a/2"]);
    }

    #[tokio::test]
    async fn list_empty_prefix_returns_everything() {
        let store = MockStore::new();
        store.insert("a", body(b""));
        store.insert("z", body(b""));
        let all = store.list("").await.unwrap();
        let keys: Vec<&str> = all.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, vec!["a", "z"]);
    }

    #[tokio::test]
    async fn put_bytes_overwrites_existing_key() {
        let store = MockStore::new();
        store
            .put_bytes("k", body(b"first"), PutOpts::default())
            .await
            .unwrap();
        store
            .put_bytes("k", body(b"second-longer"), PutOpts::default())
            .await
            .unwrap();
        assert_eq!(&store.get_bytes("k").await.unwrap()[..], b"second-longer");
        let meta = store.head("k").await.unwrap();
        assert_eq!(meta.size, b"second-longer".len() as u64);
    }

    #[tokio::test]
    async fn put_if_absent_fault_fires_before_existing_key_check() {
        // Both a fault is armed AND the key is already present. The
        // implementation must consult the fault queue before the
        // contains_key short-circuit, so callers see Err(PreconditionFailed)
        // rather than Ok(false). Locks in the ordering at mock.rs:248-270.
        let store = MockStore::new();
        store.insert("k", body(b"existing"));
        store.arm(Fault::PreconditionFailedOnPutIfAbsent { key: "k".into() });

        let err = store.put_if_absent("k", body(b"x")).await.unwrap_err();
        assert!(matches!(err, Error::PreconditionFailed(ref k) if k == "k"));
        // Body unchanged.
        assert_eq!(&store.get_bytes("k").await.unwrap()[..], b"existing");
        assert_eq!(store.pending_faults(), 0);
    }

    #[tokio::test]
    async fn put_if_absent_supports_zero_byte_lock_objects() {
        let store = MockStore::new();
        let acquired = store.put_if_absent("LOCK", Bytes::new()).await.unwrap();
        assert!(acquired);
        let meta = store.head("LOCK").await.unwrap();
        assert_eq!(meta.size, 0);
    }

    #[tokio::test]
    async fn put_if_absent_returns_false_when_key_exists() {
        let store = MockStore::new();
        assert!(store.put_if_absent("k", body(b"first")).await.unwrap());
        assert!(!store.put_if_absent("k", body(b"second")).await.unwrap());
        // Body unchanged after the rejected second call.
        assert_eq!(&store.get_bytes("k").await.unwrap()[..], b"first");
    }

    #[tokio::test]
    async fn put_if_absent_fault_returns_precondition_and_consumes_once() {
        let store = MockStore::new();
        store.arm(Fault::PreconditionFailedOnPutIfAbsent { key: "k".into() });

        let err = store.put_if_absent("k", body(b"x")).await.unwrap_err();
        assert!(matches!(err, Error::PreconditionFailed(ref k) if k == "k"));
        assert!(!store.contains("k"));
        assert_eq!(store.pending_faults(), 0);

        // Subsequent call without a fault succeeds and inserts.
        assert!(store.put_if_absent("k", body(b"x")).await.unwrap());
    }

    #[tokio::test]
    async fn delete_missing_key_is_not_found() {
        let store = MockStore::new();
        let err = store.delete("missing").await.unwrap_err();
        assert!(matches!(err, Error::NotFound(ref k) if k == "missing"));
    }

    #[tokio::test]
    async fn copy_replicates_body_and_metadata_with_fresh_timestamp() {
        let store = MockStore::new();
        let src_time = OffsetDateTime::now_utc() - Duration::from_secs(60);
        store.insert_with(
            "src",
            body(b"payload"),
            src_time,
            PutOpts {
                content_disposition: Some("attachment; filename=foo".into()),
                user_metadata: vec![("k".into(), "v".into())],
            },
        );

        store.copy("src", "dst").await.unwrap();
        assert_eq!(&store.get_bytes("dst").await.unwrap()[..], b"payload");
        // Source untouched.
        assert!(store.contains("src"));
        let opts = store.metadata("dst").expect("dst exists");
        assert_eq!(
            opts.content_disposition.as_deref(),
            Some("attachment; filename=foo")
        );
        assert_eq!(opts.user_metadata, vec![("k".to_string(), "v".to_string())]);
        // S3's copy_object semantics: dst gets a fresh server-side
        // timestamp, not the back-dated source's.
        let dst_meta = store.head("dst").await.unwrap();
        assert!(
            dst_meta.last_modified > src_time,
            "expected fresh timestamp on dst, got {} ≤ src {src_time}",
            dst_meta.last_modified,
        );
    }

    #[tokio::test]
    async fn copy_missing_source_is_not_found() {
        let store = MockStore::new();
        let err = store.copy("nope", "dst").await.unwrap_err();
        assert!(matches!(err, Error::NotFound(ref k) if k == "nope"));
        assert!(!store.contains("dst"));
    }

    #[tokio::test]
    async fn copy_overwrites_existing_destination() {
        let store = MockStore::new();
        store.insert("src", body(b"new"));
        store.insert("dst", body(b"old"));
        store.copy("src", "dst").await.unwrap();
        assert_eq!(&store.get_bytes("dst").await.unwrap()[..], b"new");
    }

    #[tokio::test]
    async fn get_to_file_writes_body_to_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");
        let store = MockStore::new();
        store.insert("k", body(b"file-bytes"));

        store.get_to_file("k", &path).await.unwrap();
        let read = tokio::fs::read(&path).await.unwrap();
        assert_eq!(read, b"file-bytes");
    }

    #[tokio::test]
    async fn get_to_file_missing_parent_dir_yields_other() {
        let dir = tempfile::tempdir().unwrap();
        // Path under a subdir we deliberately do not create — guarantees
        // ENOENT from the host without coupling to any absolute path.
        let path = dir.path().join("missing-subdir").join("out.bin");
        let store = MockStore::new();
        store.insert("k", body(b"x"));
        let err = store.get_to_file("k", &path).await.unwrap_err();
        assert!(matches!(err, Error::Other(_)));
    }

    #[tokio::test]
    async fn put_opts_round_trip_through_metadata_accessor() {
        let store = MockStore::new();
        let opts = PutOpts {
            content_disposition: Some("inline".into()),
            user_metadata: vec![("a".into(), "1".into()), ("b".into(), "2".into())],
        };
        store.put_bytes("k", body(b""), opts.clone()).await.unwrap();
        let stored = store.metadata("k").expect("k exists");
        assert_eq!(stored.content_disposition, opts.content_disposition);
        assert_eq!(stored.user_metadata, opts.user_metadata);
    }

    #[tokio::test]
    async fn insert_with_back_dates_last_modified() {
        let store = MockStore::new();
        let then = OffsetDateTime::now_utc() - Duration::from_secs(300);
        store.insert_with("LOCK", body(b""), then, PutOpts::default());
        let meta = store.head("LOCK").await.unwrap();
        assert_eq!(meta.last_modified, then);
    }

    #[tokio::test]
    async fn faults_that_never_fire_are_observable() {
        let store = MockStore::new();
        store.arm(Fault::NotFoundOnHead {
            key: "never".into(),
        });
        // Operate on a different key — fault should remain queued.
        store.insert("other", body(b""));
        store.head("other").await.unwrap();
        assert_eq!(store.pending_faults(), 1);
    }

    #[tokio::test]
    async fn list_access_denied_fault_fires_once() {
        let store = MockStore::new();
        store.arm(Fault::AccessDeniedOnList {
            prefix: "secret/".into(),
        });
        let err = store.list("secret/").await.unwrap_err();
        assert!(matches!(err, Error::AccessDenied(ref p) if p == "secret/"));
        // Second call without a queued fault returns empty.
        assert!(store.list("secret/").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn head_not_found_fault_fires_once() {
        let store = MockStore::new();
        store.insert("k", body(b"abc"));
        store.arm(Fault::NotFoundOnHead { key: "k".into() });
        let err = store.head("k").await.unwrap_err();
        assert!(matches!(err, Error::NotFound(ref k) if k == "k"));
        // Without a queued fault, head returns the inserted object's
        // metadata (key + size). Inspecting the payload guards against
        // regressions that swap the returned key or size.
        let meta = store.head("k").await.unwrap();
        assert_eq!(meta.key, "k");
        assert_eq!(meta.size, 3);
    }

    #[tokio::test]
    async fn get_bytes_network_fault_fires_once() {
        let store = MockStore::new();
        store.insert("k", body(b"x"));
        store.arm(Fault::NetworkOnGetBytes { key: "k".into() });
        let err = store.get_bytes("k").await.unwrap_err();
        assert!(matches!(err, Error::Network(_)));
        assert_eq!(&store.get_bytes("k").await.unwrap()[..], b"x");
    }

    #[test]
    fn next_lex_covers_empty_short_and_invalid_utf8_fallback() {
        // Empty input: rposition finds no non-0xFF byte, so the function
        // returns Unbounded.
        assert!(matches!(next_lex(""), Bound::Unbounded));
        // Short ASCII inputs increment the last byte cleanly.
        assert!(matches!(next_lex("a"), Bound::Excluded(s) if s == "b"));
        assert!(matches!(next_lex("ab"), Bound::Excluded(s) if s == "ac"));
        // Unicode max code point U+10FFFF encodes as F4 8F BF BF.
        // Incrementing the trailing 0xBF yields F4 8F BF C0, which is
        // invalid UTF-8 (lone 0xC0 continuation), so the
        // String::from_utf8 fallback path returns Unbounded. This is the
        // only realistic way to exercise that branch through the &str
        // surface.
        assert!(matches!(next_lex("\u{10FFFF}"), Bound::Unbounded));
    }

    #[test]
    fn mock_store_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MockStore>();
    }
}

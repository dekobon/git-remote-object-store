//! Direct file access against a packchain remote (issue #65).
//!
//! [`read_blob`] is the differentiated value-add of the packchain
//! engine: a caller fetches a single file at a ref's tip without
//! cloning, materialising a working tree, or invoking git. The
//! lookup walks the on-bucket artefacts the Phase 2 push wrote:
//!
//! 1. `chain.json` to verify the ref exists.
//! 2. `path-index.json` to resolve `path` → blob SHA at tip.
//! 3. Each segment's `.idx` (newest-first) to locate the blob's pack
//!    entry.
//! 4. A ranged GET against the matching `.pack` to fetch the entry
//!    bytes, zlib-decompressed (and delta-applied, if applicable).
//!
//! The pack-index parses are amortised across calls via
//! [`PackIndexCache`], a byte-bounded LRU keyed by
//! `(prefix, content-sha)`. Single-shot callers can pass
//! `&PackIndexCache::default()` and let the cache GC at drop.
//!
//! ## Delta resolution
//!
//! Pack entries may be deltas against a base elsewhere in the chain.
//! `OFS_DELTA` resolves within the same pack via a relative back-offset;
//! `REF_DELTA` resolves to a SHA which may live in any pack in the
//! chain. The walker recurses, capped at [`MAX_DELTA_DEPTH`] (matching
//! git's own limit) so a corrupted chain with a delta cycle aborts
//! cleanly instead of looping forever.
//!
//! ## What this module does *not* do
//!
//! - **No on-disk cache**: indices live in memory only. CI agents that
//!   want cross-process amortisation should layer their own.
//! - **No directory listings**: [`read_blob`] is single-file. The
//!   nested-tree shape supports listing cleanly, but it's a separate
//!   API and out of scope for issue #65.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use gix_pack::data::entry::Header as EntryHeader;
use tracing::{debug, warn};

use crate::git::RefName;
use crate::object_store::{ObjectStore, ObjectStoreError};
use crate::remote::Remote;
use crate::url::StorageEngine;

use super::PackchainError;
use super::keys::{pack_idx_key, pack_key};
use super::manifest::{load_chain, load_path_index};
use super::retry::{
    PACK_MISSING_MAX_RETRIES, PACK_MISSING_RETRY_BACKOFFS, chain_references_pack_key,
};
use super::schema::{ChainManifest, ChainSegment, PathNode, Sha40};

/// Hard cap on delta-chain depth, matching git's own
/// `pack.deltaCacheLimit`-adjacent recursion limit. A correctly built
/// chain won't approach this; tripping the cap means the pack is
/// corrupted (a cycle) or pathologically deep, and either way
/// stopping is the right call.
pub const MAX_DELTA_DEPTH: u32 = 50;

/// Default in-memory budget for [`PackIndexCache`] (64 MiB), matching
/// the cap the issue #65 plan calls out. Covers a chain of dozens of
/// large packs without thrashing.
pub const DEFAULT_CACHE_CAPACITY_BYTES: u64 = 64 * 1024 * 1024;

/// Upper safety bound on the bytes a single ranged GET may request,
/// covering both the terminal-entry path in [`fetch_entry_bytes`] and
/// the fallback widening in [`inflate_with_retry`]. Past this point
/// we surface a typed error rather than pulling unbounded bytes — a
/// single multi-GiB blob in a code repo is overwhelmingly likely to
/// be a misuse (git-LFS material) rather than a legitimate
/// `read_blob` target.
const MAX_RANGE_BYTES: u64 = 1024 * 1024 * 1024;

/// Hard cap on a single decompressed pack object (1 GiB), enforced
/// against attacker-controlled values from the pack entry header
/// (`decompressed_size`) and the delta dst-size header. A malicious
/// bucket can craft these to claim huge sizes; without a cap, we
/// would `vec![0u8; n]` or `Vec::with_capacity(n)` for that many
/// bytes and either panic or thrash. 1 GiB matches [`MAX_RANGE_BYTES`]
/// and exceeds any realistic source-tree blob; LFS material lives in
/// the LFS path, not [`read_blob`].
const MAX_DECOMPRESSED_BYTES: u64 = 1024 * 1024 * 1024;

/// Maximum number of times the fallback range may expand before the
/// reader gives up with [`PackchainError::MalformedPackEntry`]. Each
/// expansion doubles the range, so 6 retries cover up to ~1 GiB.
const MAX_RANGE_EXPANSIONS: u32 = 6;

/// In-process LRU cache of decoded pack indices keyed by
/// `(prefix, content-sha)`.
///
/// Capacity is bounded by **byte size**, not entry count: a single 1 GB
/// pack carries an .idx file of multiple MiB, so an entry-count cap
/// would either over- or under-budget for realistic chains. Eviction
/// is least-recently-used.
///
/// The cache is `Send + Sync` and shareable across [`read_blob`] calls.
/// Multiple concurrent calls block briefly on the inner mutex during
/// lookup / insert; the inflate / range-GET work happens outside the
/// lock so contention stays bounded.
///
/// ## LRU bookkeeping cost
///
/// `get` and `insert` walk the order [`VecDeque`] via `iter().position`
/// to move the touched key to the back — **O(n) in the cache size**.
/// For typical packchain workloads (single-digit indices in flight),
/// the constant factor dominates and this is faster than a true O(1)
/// linked-list LRU. If a workload starts seeing hundreds of cached
/// indices, this should be revisited (e.g. swap to the `lru` crate or
/// hand-roll a `HashMap` + intrusive doubly-linked list). The simple
/// shape is intentional for now.
///
/// # Example
///
/// ```no_run
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use git_remote_object_store::{packchain::PackIndexCache, Remote};
///
/// let remote = Remote::connect("s3+https://bucket/repo?engine=packchain").await?;
/// let cache = PackIndexCache::default();
/// let bytes = git_remote_object_store::packchain::read_blob(
///     &remote,
///     "refs/heads/main",
///     "src/main.rs",
///     &cache,
/// ).await?;
/// println!("{}", String::from_utf8_lossy(&bytes));
/// # Ok(())
/// # }
/// ```
pub struct PackIndexCache {
    inner: Mutex<CacheInner>,
    capacity_bytes: u64,
}

struct CacheInner {
    /// Owned indices keyed by `(prefix, content-sha)`.
    ///
    /// `Arc` lets [`read_blob`] hold a long-lived reference to the
    /// index while the cache lock is dropped, so the inflate /
    /// range-GET work below doesn't block sibling cache lookups.
    map: HashMap<CacheKey, Arc<CachedIndex>>,
    /// LRU order — front is least-recently-used, back is most-recent.
    order: VecDeque<CacheKey>,
    total_bytes: u64,
}

type CacheKey = (String, Sha40);

struct CachedIndex {
    /// Parsed .idx file owning its bytes (in-memory parse via
    /// [`gix_pack::index::File::from_data`]).
    file: gix_pack::index::File<Vec<u8>>,
    /// Pre-sorted ascending pack offsets. Used to derive the
    /// next-offset upper bound for a ranged GET against the matching
    /// pack file. Computed once at insert.
    sorted_offsets: Vec<u64>,
    /// Approximate resident byte count (the .idx body plus the offsets
    /// vector). Used for the LRU byte-budget bookkeeping.
    bytes: u64,
}

impl PackIndexCache {
    /// Construct a cache with the requested byte budget.
    ///
    /// `capacity_bytes` of zero disables caching (every lookup misses).
    /// Use [`Self::default`] for the standard 64 MiB budget.
    #[must_use]
    pub fn new(capacity_bytes: u64) -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                map: HashMap::new(),
                order: VecDeque::new(),
                total_bytes: 0,
            }),
            capacity_bytes,
        }
    }

    /// Total resident bytes accounted for by the cache.
    ///
    /// # Panics
    ///
    /// Panics only if a previous holder of the inner mutex panicked
    /// while mutating cache state — an invariant violation that would
    /// be unsafe to silently recover from.
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        self.lock().total_bytes
    }

    /// Number of cached entries.
    ///
    /// # Panics
    ///
    /// See [`Self::resident_bytes`].
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    /// Whether the cache currently holds zero entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheInner> {
        self.inner.lock().expect("cache mutex poisoned")
    }

    fn get(&self, key: &CacheKey) -> Option<Arc<CachedIndex>> {
        let mut inner = self.lock();
        let entry = inner.map.get(key).cloned()?;
        // Move to most-recently-used position.
        remove_from_order(&mut inner.order, key);
        inner.order.push_back(key.clone());
        Some(entry)
    }

    fn insert(&self, key: CacheKey, value: Arc<CachedIndex>) {
        let mut inner = self.lock();
        let bytes = value.bytes;
        // Replace existing entry's accounting if present.
        if let Some(prev) = inner.map.remove(&key) {
            inner.total_bytes = inner.total_bytes.saturating_sub(prev.bytes);
            remove_from_order(&mut inner.order, &key);
        }
        // If a single entry exceeds the budget, refuse to cache it
        // (otherwise we'd evict everything and still overshoot).
        if bytes > self.capacity_bytes {
            return;
        }
        // Evict oldest until the new entry fits.
        while inner.total_bytes + bytes > self.capacity_bytes {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            if let Some(removed) = inner.map.remove(&oldest) {
                inner.total_bytes = inner.total_bytes.saturating_sub(removed.bytes);
            }
        }
        inner.total_bytes += bytes;
        inner.order.push_back(key.clone());
        inner.map.insert(key, value);
    }
}

fn remove_from_order(order: &mut VecDeque<CacheKey>, key: &CacheKey) {
    if let Some(pos) = order.iter().position(|k| k == key) {
        order.remove(pos);
    }
}

impl Default for PackIndexCache {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_CAPACITY_BYTES)
    }
}

impl std::fmt::Debug for PackIndexCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Custom impl avoids deadlock-prone `Mutex` Debug while still
        // exposing operationally interesting state: the static budget,
        // current accounting, and current entry count. The `inner`
        // field is *not* surfaced by design — it's an implementation
        // detail and would print the entire cache contents.
        f.debug_struct("PackIndexCache")
            .field("capacity_bytes", &self.capacity_bytes)
            .field("resident_bytes", &self.resident_bytes())
            .field("entries", &self.len())
            .finish_non_exhaustive()
    }
}

/// Read the contents of `path` at `ref_name`'s tip from a packchain
/// remote.
///
/// Walks `chain.json` + `path-index.json` to resolve `path` → blob
/// SHA, then consults each segment's `.idx` newest-first for the
/// blob's pack entry. The matching entry's bytes are fetched via a
/// ranged GET, zlib-decompressed, and (when the entry is a delta)
/// recursively resolved against its base. The entry's eventual blob
/// payload is returned as an owned [`Bytes`].
///
/// `cache` amortises pack-index parsing across calls within the same
/// process. Long-running consumers (CI agents, build systems) should
/// keep one [`PackIndexCache`] for the lifetime of the process so the
/// per-call cost is one or two API calls plus a zlib inflate; one-shot
/// callers can pass `&PackIndexCache::default()` and discard.
///
/// # Errors
///
/// - [`PackchainError::WrongEngine`] when the remote's engine is not
///   [`StorageEngine::Packchain`].
/// - [`PackchainError::ChainAbsent`] when the branch is unknown to
///   the bucket.
/// - [`PackchainError::PathIndexAbsent`] when `chain.json` exists but
///   `path-index.json` does not (a partially crashed first push).
/// - [`PackchainError::TransientChainPathIndexMismatch`] when both
///   exist but `path_index.tip != chain.tip` — a brief window during
///   a concurrent push or compact where `chain.json` has been
///   overwritten ahead of `path-index.json`. Retry-shaped: a
///   subsequent read converges once the writer finishes.
/// - [`PackchainError::MalformedPath`] for `..` segments, leading
///   `/`, empty path, or empty segments (consecutive slashes).
/// - [`PackchainError::PathNotFound`] when the path does not exist
///   in the resolved tree.
/// - [`PackchainError::PathNotABlob`] when the path resolves to a
///   directory rather than a file.
/// - [`PackchainError::BlobNotInChain`] when the path-index named a
///   blob SHA absent from every pack referenced by `chain.json`.
/// - [`PackchainError::DeltaTooDeep`] / [`PackchainError::MalformedDelta`]
///   / [`PackchainError::MalformedPackEntry`] / [`PackchainError::Decompress`]
///   for pack-corruption shapes.
/// - [`PackchainError::PackMissing`], [`PackchainError::Store`], or
///   [`PackchainError::Io`] for transport / I/O failures.
/// - [`PackchainError::ConcurrentGcRetriesExhausted`] when a
///   vigorous concurrent `manage gc sweep` kept deleting packs the
///   reader had just discovered, exhausting the internal retry
///   schedule. Callers should re-invoke `read_blob` after the
///   compaction settles.
pub async fn read_blob(
    remote: &Remote,
    ref_name: &str,
    path: &str,
    cache: &PackIndexCache,
) -> Result<Bytes, PackchainError> {
    if remote.engine() != StorageEngine::Packchain {
        return Err(PackchainError::WrongEngine {
            found: remote.engine(),
        });
    }

    let segments = parse_path(path)?;
    let remote_ref = RefName::new(ref_name).map_err(|_| PackchainError::InvalidRefName {
        name: ref_name.to_owned(),
    })?;
    // `Remote::prefix()` returns `""` for bucket-root remotes; the
    // engine-wide convention (post-#103) is `None` for "no prefix"
    // and `Some(non-empty)` otherwise. Normalise here so cache keys
    // and any downstream `Some(p) if !p.is_empty()` matches agree
    // with the rest of the codebase.
    let prefix_opt = (!remote.prefix().is_empty()).then(|| remote.prefix());

    let chain = load_chain(remote.store(), prefix_opt, &remote_ref)
        .await?
        .ok_or_else(|| PackchainError::ChainAbsent {
            ref_name: ref_name.to_owned(),
        })?;

    let path_index = load_path_index(remote.store(), prefix_opt, &remote_ref)
        .await?
        .ok_or_else(|| PackchainError::PathIndexAbsent {
            ref_name: ref_name.to_owned(),
        })?;

    // Writers PUT chain.json before path-index.json (see the engine
    // module doc on the linearisation point and issue #114). A crash
    // or in-flight push between the two PUTs leaves a stale
    // `path_index.tip` paired with a fresh `chain.tip`; resolving a
    // path through the stale path-index would yield a blob SHA that
    // names a different file than the caller intended (or one absent
    // from the new chain entirely). Refuse to do that — surface the
    // mismatch as a typed transient error so the caller can retry,
    // and so `BlobNotInChain` keeps its honest "bucket corruption"
    // meaning rather than masking a race window.
    if path_index.tip != chain.tip {
        return Err(PackchainError::TransientChainPathIndexMismatch {
            ref_name: ref_name.to_owned(),
            chain_tip: chain.tip.as_str().to_owned(),
            path_index_tip: path_index.tip.as_str().to_owned(),
        });
    }

    let blob_sha = walk_path(&path_index.tree, &segments, ref_name, path)?;

    debug!(
        ref_name = %ref_name,
        path = %path,
        blob = %blob_sha.as_str(),
        segments = chain.segments.len(),
        "read_blob: resolved path to blob, scanning chain"
    );

    let blob_oid = sha40_to_object_id(&blob_sha);
    let result = read_with_pack_missing_retries(
        remote.store(),
        prefix_opt,
        &remote_ref,
        ref_name,
        chain,
        &blob_oid,
        cache,
    )
    .await;
    let blob_not_in_chain = || PackchainError::BlobNotInChain {
        sha: blob_sha.as_str().to_owned(),
        path: path.to_owned(),
    };
    match result {
        Ok(ResolvedObject {
            payload,
            kind: ObjectKind::Blob,
        }) => Ok(Bytes::from(payload)),
        // path-index pointed at a non-blob — bucket inconsistency.
        Ok(_) => Err(blob_not_in_chain()),
        // Inner walker returns BlobNotInChain with an empty path field
        // (it doesn't know the caller's path); replace with one that
        // carries the caller's path for diagnostic clarity. Inner
        // BlobNotInChain values for *other* shas (delta-base lookups)
        // pass through unchanged.
        Err(PackchainError::BlobNotInChain { sha, .. }) if sha == blob_sha.as_str() => {
            Err(blob_not_in_chain())
        }
        Err(e) => Err(e),
    }
}

/// Pack-read driver for [`read_blob`]. Walks the supplied chain for
/// `blob_oid`; on a [`PackchainError::PackMissing`] caused by a
/// concurrent `manage gc sweep` (detected by reloading `chain.json`
/// and observing that the failing key is no longer referenced), waits
/// a backoff and retries against the fresh chain. After
/// [`PACK_MISSING_MAX_RETRIES`] retries gives up with
/// [`PackchainError::ConcurrentGcRetriesExhausted`] (issue #136).
///
/// Path-index is **not** reloaded across retries — the original
/// `blob_oid` represents the snapshot the caller asked about, and
/// compaction preserves blob content addressing so the same SHA
/// resolves in any compatible chain. A new push that overwrote
/// path-index between the initial load and the retry would resolve
/// to a different blob SHA, but the read is still well-defined at
/// the snapshot point (the path-as-of-the-initial-load) and that is
/// the right semantics for a stateless point-in-time read.
///
/// Non-`PackMissing` errors (parse, decompress, transport, etc.)
/// pass through immediately — they are not retryable in this
/// fashion and would otherwise wait through the backoff schedule
/// for no reason. `BlobNotInChain` is also a passthrough so the
/// caller can attach the user-facing `path` field.
async fn read_with_pack_missing_retries(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    remote_ref: &RefName,
    ref_name: &str,
    initial_chain: ChainManifest,
    blob_oid: &gix_hash::ObjectId,
    cache: &PackIndexCache,
) -> Result<ResolvedObject, PackchainError> {
    let mut current_chain = initial_chain;
    let mut attempt: u32 = 0;
    loop {
        let mut depth = 0u32;
        let result = read_object_from_chain(
            store,
            prefix,
            &current_chain.segments,
            blob_oid,
            cache,
            &mut depth,
        )
        .await;
        let missing_key = match result {
            Ok(resolved) => return Ok(resolved),
            Err(PackchainError::PackMissing { key }) => key,
            Err(e) => return Err(e),
        };
        // PackMissing — distinguish concurrent GC (retryable) from
        // genuine bucket inconsistency (data loss → fail fast).
        let reloaded = load_chain(store, prefix, remote_ref)
            .await?
            .ok_or_else(|| PackchainError::ChainAbsent {
                ref_name: ref_name.to_owned(),
            })?;
        if chain_references_pack_key(&reloaded, prefix, &missing_key)? {
            // The reloaded chain still names this pack — the bucket
            // is genuinely missing data the chain still references.
            // Surface the original PackMissing without retrying.
            return Err(PackchainError::PackMissing { key: missing_key });
        }
        if attempt >= PACK_MISSING_MAX_RETRIES {
            warn!(
                ref_name = %ref_name,
                last_missing_key = %missing_key,
                attempts = attempt,
                "read_blob: exhausted pack-missing retries against concurrent GC"
            );
            return Err(PackchainError::ConcurrentGcRetriesExhausted {
                last_missing_key: missing_key,
                attempts: attempt,
            });
        }
        debug!(
            ref_name = %ref_name,
            missing_key = %missing_key,
            attempt = attempt,
            "read_blob: PackMissing on chain no longer references the pack — retrying after GC race"
        );
        tokio::time::sleep(PACK_MISSING_RETRY_BACKOFFS[attempt as usize]).await;
        attempt += 1;
        current_chain = reloaded;
    }
}

/// Decoded pack object — the kind discriminates blobs from other
/// types so [`read_blob`] can refuse to return a tree as a "blob".
#[derive(Debug)]
struct ResolvedObject {
    payload: Vec<u8>,
    kind: ObjectKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjectKind {
    Blob,
    Commit,
    Tree,
    Tag,
}

/// Validate `path` and split it on `/`.
///
/// Rejects shapes that don't map to git tree semantics: empty paths,
/// `/`-prefixed (absolute), `..` segments, and empty segments
/// (consecutive slashes / trailing slashes).
fn parse_path(path: &str) -> Result<Vec<&str>, PackchainError> {
    if path.is_empty() {
        return Err(PackchainError::MalformedPath {
            path: path.to_owned(),
            reason: "empty path",
        });
    }
    if path.starts_with('/') {
        return Err(PackchainError::MalformedPath {
            path: path.to_owned(),
            reason: "absolute paths are not allowed",
        });
    }
    let segments: Vec<&str> = path.split('/').collect();
    for seg in &segments {
        if seg.is_empty() {
            return Err(PackchainError::MalformedPath {
                path: path.to_owned(),
                reason: "empty segment (consecutive or trailing slash)",
            });
        }
        if *seg == ".." {
            return Err(PackchainError::MalformedPath {
                path: path.to_owned(),
                reason: "`..` segments are not allowed",
            });
        }
        if *seg == "." {
            return Err(PackchainError::MalformedPath {
                path: path.to_owned(),
                reason: "`.` segments are not allowed",
            });
        }
    }
    Ok(segments)
}

/// Walk the nested path-index tree following `segments`. Returns the
/// terminal blob's SHA on success.
fn walk_path(
    root: &BTreeMap<String, PathNode>,
    segments: &[&str],
    ref_name: &str,
    path: &str,
) -> Result<Sha40, PackchainError> {
    let path_not_found = || PackchainError::PathNotFound {
        ref_name: ref_name.to_owned(),
        path: path.to_owned(),
    };
    // Splitting up front asserts the invariant `parse_path` guarantees
    // (segments is non-empty) and lets the rest of the function be a
    // straight walk-then-leaf-check with no unreachable fallthrough.
    let (last_seg, prefix_segs) = segments
        .split_last()
        .expect("parse_path guarantees at least one segment");
    let mut current = root;
    for seg in prefix_segs {
        // A mid-path blob (`a/file.txt/extra`) and a missing key both
        // mean the caller's path doesn't resolve in this tree.
        let Some(PathNode::Tree(children)) = current.get(*seg) else {
            return Err(path_not_found());
        };
        current = children;
    }
    match current.get(*last_seg) {
        Some(PathNode::Blob(sha)) => Ok(sha.clone()),
        Some(PathNode::Tree(_)) => Err(PackchainError::PathNotABlob {
            path: path.to_owned(),
        }),
        None => Err(path_not_found()),
    }
}

fn sha40_to_object_id(sha: &Sha40) -> gix_hash::ObjectId {
    // Sha40 invariant: exactly 40 lowercase hex characters. The
    // gix_hash parser accepts that shape unconditionally, so the
    // unwrap-via-expect is documenting the invariant rather than
    // introducing a panic site (see .claude/rules/rust.md).
    gix_hash::ObjectId::from_hex(sha.as_str().as_bytes())
        .expect("Sha40 is always 40 lowercase hex by construction")
}

/// Locate `target_oid` in the chain (newest-first) and decode its
/// pack entry, applying delta resolution as needed.
async fn read_object_from_chain(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    segments: &[ChainSegment],
    target_oid: &gix_hash::ObjectId,
    cache: &PackIndexCache,
    depth: &mut u32,
) -> Result<ResolvedObject, PackchainError> {
    // Note: the delta-depth guard lives in `decode_entry`, the single
    // chokepoint every recursive resolution path traverses. Putting it
    // here would miss the OFS_DELTA branch, which recurses through
    // `decode_entry` directly without coming back via this function
    // (issue #83).
    for segment in segments {
        let content_sha = super::keys::segment_pack_sha(segment)?;
        let idx = load_index(store, prefix, &content_sha, cache).await?;
        let Some(entry_index) = idx.file.lookup(target_oid) else {
            continue;
        };
        let pack_offset = idx.file.pack_offset_at_index(entry_index);
        let bytes = fetch_entry_bytes(store, prefix, &content_sha, pack_offset, &idx).await?;
        let resolved = Box::pin(decode_entry(
            store,
            prefix,
            segments,
            &content_sha,
            pack_offset,
            &bytes,
            cache,
            depth,
        ))
        .await?;
        return Ok(resolved);
    }
    Err(PackchainError::BlobNotInChain {
        // `gix_hash::ObjectId: Display` already produces 40-lowercase-hex
        // (`Display` → `to_hex()` → `HexDisplay`).
        sha: target_oid.to_string(),
        path: String::new(),
    })
}

async fn load_index(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    content_sha: &Sha40,
    cache: &PackIndexCache,
) -> Result<Arc<CachedIndex>, PackchainError> {
    let key = (prefix.unwrap_or("").to_owned(), content_sha.clone());
    if let Some(hit) = cache.get(&key) {
        return Ok(hit);
    }

    let idx_key = pack_idx_key(prefix, content_sha);
    let idx_bytes = match store.get_bytes(&idx_key).await {
        Ok(b) => b,
        Err(ObjectStoreError::NotFound(_)) => {
            return Err(PackchainError::PackMissing { key: idx_key });
        }
        Err(e) => return Err(PackchainError::Store(e)),
    };

    let owned: Vec<u8> = idx_bytes.to_vec();
    let owned_len = owned.len() as u64;
    let path = std::path::PathBuf::from(idx_key);
    let file =
        gix_pack::index::File::from_data(owned, path, gix_hash::Kind::Sha1).map_err(|e| {
            PackchainError::MalformedPackEntry {
                offset: 0,
                reason: format!("idx parse: {e}"),
            }
        })?;
    let sorted_offsets = file.sorted_offsets();
    let offsets_bytes = (sorted_offsets.len() as u64).saturating_mul(8);
    let cached = Arc::new(CachedIndex {
        file,
        sorted_offsets,
        bytes: owned_len.saturating_add(offsets_bytes),
    });
    cache.insert(key, Arc::clone(&cached));
    Ok(cached)
}

/// Range-GET the pack bytes for the entry starting at `pack_offset`.
///
/// Bounds are derived from `idx.sorted_offsets`: the next-greater
/// offset is the entry's end. When `pack_offset` is the highest
/// recorded offset (the last entry), the actual entry end is the
/// trailer position — which we don't know without an extra round
/// trip. Strategy:
///
/// 1. If `next_offset` is known, range-GET `[pack_offset, next_offset)`.
/// 2. Otherwise, `HEAD` the pack to learn its length, then range-GET
///    `[pack_offset, pack_len)`. The HEAD round-trip only fires for
///    the very last entry in a pack — every other entry's bound is
///    already in `sorted_offsets`. Both branches enforce
///    [`MAX_RANGE_BYTES`]; a terminal-entry tail above the cap is
///    rejected as [`PackchainError::MalformedPackEntry`] rather than
///    pulled in full.
async fn fetch_entry_bytes(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    content_sha: &Sha40,
    pack_offset: u64,
    idx: &CachedIndex,
) -> Result<Bytes, PackchainError> {
    let pack = pack_key(prefix, content_sha);
    let next_offset = idx
        .sorted_offsets
        .iter()
        .copied()
        .find(|&o| o > pack_offset);
    let end = if let Some(end) = next_offset {
        end
    } else {
        // Last entry in the pack — learn pack length via HEAD so the
        // range can be bounded the same way as non-terminal entries.
        let meta = match store.head(&pack).await {
            Ok(m) => m,
            Err(ObjectStoreError::NotFound(_)) => {
                return Err(PackchainError::PackMissing { key: pack });
            }
            Err(e) => return Err(PackchainError::Store(e)),
        };
        if pack_offset >= meta.size {
            return Err(PackchainError::MalformedPackEntry {
                offset: pack_offset,
                reason: "entry offset beyond pack EOF".to_owned(),
            });
        }
        meta.size
    };
    let span = end.saturating_sub(pack_offset);
    if span > MAX_RANGE_BYTES {
        return Err(PackchainError::MalformedPackEntry {
            offset: pack_offset,
            reason: format!("entry range {span} bytes exceeds {MAX_RANGE_BYTES}-byte cap"),
        });
    }
    match store.get_bytes_range(&pack, pack_offset..end).await {
        Ok(b) => Ok(b),
        Err(ObjectStoreError::NotFound(_)) => Err(PackchainError::PackMissing { key: pack }),
        Err(e) => Err(PackchainError::Store(e)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn decode_entry(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    chain: &[ChainSegment],
    content_sha: &Sha40,
    pack_offset: u64,
    raw: &[u8],
    cache: &PackIndexCache,
    depth: &mut u32,
) -> Result<ResolvedObject, PackchainError> {
    // Single chokepoint for the delta-depth budget: every recursive
    // delta resolution path — REF_DELTA via `read_object_from_chain`
    // and OFS_DELTA via the direct recursion below — re-enters here.
    // Guarding only `read_object_from_chain` (the previous shape) let
    // a pure-OFS_DELTA chain stack-overflow because OFS_DELTA never
    // re-routed through it (issue #83).
    if *depth > MAX_DELTA_DEPTH {
        return Err(PackchainError::DeltaTooDeep {
            max: MAX_DELTA_DEPTH,
        });
    }
    *depth += 1;

    let entry =
        gix_pack::data::Entry::from_bytes(raw, pack_offset, gix_hash::Kind::Sha1.len_in_bytes())
            .map_err(|e| PackchainError::MalformedPackEntry {
                offset: pack_offset,
                reason: e.to_string(),
            })?;

    // `data_offset` is absolute (pack_offset + header_size). Convert
    // to an index into our locally-fetched buffer. Both casts must
    // succeed: header_size is the number of bytes the entry header
    // consumed (always tiny), and decompressed_size came from the
    // entry header itself (capped by the pack format at u32-ish).
    let header_size: usize = usize::try_from(entry.data_offset - pack_offset).map_err(|_| {
        PackchainError::MalformedPackEntry {
            offset: pack_offset,
            reason: "entry header size exceeds usize".to_owned(),
        }
    })?;
    // Reject pack-header-driven sizes above the hard cap *before*
    // converting to `usize` and allocating. A malicious bucket can
    // claim arbitrary `decompressed_size`; without this guard we'd
    // `vec![0u8; n]` for that many bytes in `inflate_to`.
    if entry.decompressed_size > MAX_DECOMPRESSED_BYTES {
        return Err(PackchainError::MalformedPackEntry {
            offset: pack_offset,
            reason: format!(
                "decompressed object size {} exceeds {}-byte cap",
                entry.decompressed_size, MAX_DECOMPRESSED_BYTES
            ),
        });
    }
    let decompressed_size: usize = usize::try_from(entry.decompressed_size).map_err(|_| {
        PackchainError::MalformedPackEntry {
            offset: pack_offset,
            reason: "decompressed object size exceeds usize".to_owned(),
        }
    })?;

    let inflated = inflate_with_retry(
        store,
        prefix,
        content_sha,
        pack_offset,
        raw,
        header_size,
        decompressed_size,
    )
    .await?;

    match entry.header {
        EntryHeader::Blob => Ok(ResolvedObject {
            payload: inflated,
            kind: ObjectKind::Blob,
        }),
        EntryHeader::Commit => Ok(ResolvedObject {
            payload: inflated,
            kind: ObjectKind::Commit,
        }),
        EntryHeader::Tree => Ok(ResolvedObject {
            payload: inflated,
            kind: ObjectKind::Tree,
        }),
        EntryHeader::Tag => Ok(ResolvedObject {
            payload: inflated,
            kind: ObjectKind::Tag,
        }),
        EntryHeader::OfsDelta { base_distance } => {
            let base_offset = pack_offset.checked_sub(base_distance).ok_or(
                PackchainError::MalformedPackEntry {
                    offset: pack_offset,
                    reason: "ofs-delta base distance underflows pack offset".to_owned(),
                },
            )?;
            let idx = load_index(store, prefix, content_sha, cache).await?;
            let base_bytes =
                fetch_entry_bytes(store, prefix, content_sha, base_offset, &idx).await?;
            let base = Box::pin(decode_entry(
                store,
                prefix,
                chain,
                content_sha,
                base_offset,
                &base_bytes,
                cache,
                depth,
            ))
            .await?;
            apply_delta(&base, &inflated)
        }
        EntryHeader::RefDelta { base_id } => {
            let base = Box::pin(read_object_from_chain(
                store, prefix, chain, &base_id, cache, depth,
            ))
            .await?;
            apply_delta(&base, &inflated)
        }
    }
}

/// Inflate the entry's compressed payload, widening the range and
/// retrying when the locally-fetched buffer is short of the zlib
/// stream end. Only fires for the very last entry in a pack — every
/// other entry's range is bounded by [`CachedIndex::sorted_offsets`].
async fn inflate_with_retry(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    content_sha: &Sha40,
    pack_offset: u64,
    raw: &[u8],
    header_size: usize,
    decompressed_size: usize,
) -> Result<Vec<u8>, PackchainError> {
    // Own the wider buffer as `Bytes` when a retry has fetched more.
    // `Bytes` is Arc-backed so storing it (vs `Vec<u8>`) avoids the
    // `.to_vec()` copy on every retry. The `&buf[header_size..]`
    // re-borrow below auto-derefs through `Bytes`'s `Deref<Target=[u8]>`
    // — we're not constructing a `Bytes::slice`, just indexing into
    // the existing buffer.
    let mut current_buffer: Option<Bytes> = None;
    let mut current_end = pack_offset.saturating_add(raw.len() as u64);
    let mut expansions = 0u32;
    loop {
        let compressed: &[u8] = match &current_buffer {
            Some(buf) => &buf[header_size..],
            None => &raw[header_size..],
        };
        match inflate_to(compressed, decompressed_size) {
            Ok(v) => return Ok(v),
            Err(InflateOutcome::NeedMoreInput) => {
                if expansions >= MAX_RANGE_EXPANSIONS {
                    return Err(PackchainError::MalformedPackEntry {
                        offset: pack_offset,
                        reason: "ran out of compressed bytes after maximum range expansion"
                            .to_owned(),
                    });
                }
                let next_size = ((current_end - pack_offset) * 2).min(MAX_RANGE_BYTES);
                if next_size <= current_end - pack_offset {
                    return Err(PackchainError::MalformedPackEntry {
                        offset: pack_offset,
                        reason: "range expansion hit safety cap".to_owned(),
                    });
                }
                let new_end = pack_offset + next_size;
                let pack = pack_key(prefix, content_sha);
                let bytes = match store.get_bytes_range(&pack, pack_offset..new_end).await {
                    Ok(b) => b,
                    Err(ObjectStoreError::NotFound(_)) => {
                        return Err(PackchainError::PackMissing { key: pack });
                    }
                    Err(ObjectStoreError::RangeNotSatisfiable { .. }) => {
                        return Err(PackchainError::MalformedPackEntry {
                            offset: pack_offset,
                            reason: "zlib stream truncated at pack EOF".to_owned(),
                        });
                    }
                    Err(e) => return Err(PackchainError::Store(e)),
                };
                current_buffer = Some(bytes);
                current_end = new_end;
                expansions += 1;
            }
            Err(InflateOutcome::Failed) => {
                return Err(PackchainError::Decompress {
                    offset: pack_offset,
                });
            }
        }
    }
}

/// One-shot zlib inflate into a buffer of the announced decompressed
/// size. `gix_features::zlib::Inflate` handles the actual decode; the
/// outer return distinguishes "need more input" (caller can widen the
/// range) from "stream is broken".
fn inflate_to(input: &[u8], announced_size: usize) -> Result<Vec<u8>, InflateOutcome> {
    use gix::features::zlib::{FlushDecompress, Status};

    let mut state = gix::features::zlib::Decompress::new();
    let mut out = vec![0u8; announced_size];
    match state.decompress(input, &mut out, FlushDecompress::Finish) {
        Ok(Status::StreamEnd) => {
            let produced =
                usize::try_from(state.total_out()).map_err(|_| InflateOutcome::Failed)?;
            if produced != announced_size {
                return Err(InflateOutcome::Failed);
            }
            Ok(out)
        }
        Ok(Status::Ok | Status::BufError) => Err(InflateOutcome::NeedMoreInput),
        Err(_) => Err(InflateOutcome::Failed),
    }
}

enum InflateOutcome {
    NeedMoreInput,
    Failed,
}

/// Apply a git pack-format delta to `base`, returning the
/// reconstituted object with the same kind as `base`.
fn apply_delta(base: &ResolvedObject, delta: &[u8]) -> Result<ResolvedObject, PackchainError> {
    let mut cursor = 0usize;
    let (src_size, n) = read_size_varint(delta, cursor).ok_or(PackchainError::MalformedDelta {
        reason: "truncated source size header",
    })?;
    cursor += n;
    let (dst_size, n) = read_size_varint(delta, cursor).ok_or(PackchainError::MalformedDelta {
        reason: "truncated destination size header",
    })?;
    cursor += n;
    if src_size != base.payload.len() as u64 {
        return Err(PackchainError::MalformedDelta {
            reason: "delta source size does not match base object size",
        });
    }
    // Cap dst-size before allocating: it comes from the delta's
    // varint header (attacker-controlled in a malicious bucket).
    // Without this guard, `Vec::with_capacity(huge)` would panic
    // or thrash. Same cap as the entry-header path uses.
    if dst_size > MAX_DECOMPRESSED_BYTES {
        return Err(PackchainError::MalformedDelta {
            reason: "delta destination size exceeds 1 GiB cap",
        });
    }
    let dst_size_usize = usize::try_from(dst_size).map_err(|_| PackchainError::MalformedDelta {
        reason: "delta destination size exceeds usize",
    })?;
    let mut out = Vec::with_capacity(dst_size_usize);
    while cursor < delta.len() {
        let op = delta[cursor];
        cursor += 1;
        if op & 0x80 != 0 {
            apply_delta_copy_op(op, delta, &mut cursor, &base.payload, &mut out)?;
        } else if op == 0 {
            return Err(PackchainError::MalformedDelta {
                reason: "reserved zero opcode",
            });
        } else {
            apply_delta_insert_op(op, delta, &mut cursor, &mut out)?;
        }
        // Bound per-op growth so a malicious delta cannot grow `out`
        // without limit between the dst_size header check and the
        // post-loop equality check. Mirrors git's `patch-delta.c`
        // `size -= cp_size` invariant (any op that would push past
        // the announced destination size is rejected immediately).
        if out.len() > dst_size_usize {
            return Err(PackchainError::MalformedDelta {
                reason: "produced object exceeds announced destination size",
            });
        }
    }
    if out.len() as u64 != dst_size {
        return Err(PackchainError::MalformedDelta {
            reason: "produced object does not match announced destination size",
        });
    }
    Ok(ResolvedObject {
        payload: out,
        kind: base.kind,
    })
}

/// Decode a packed-bitfield operand: for each set bit in `bitmask`,
/// consume the next byte of `delta` and OR it in at `bit_index * 8`.
fn read_packed_operand(
    delta: &[u8],
    cursor: &mut usize,
    bitmask: u8,
    bits: u8,
    truncated_reason: &'static str,
) -> Result<u32, PackchainError> {
    let mut value = 0u32;
    for shift in 0..bits {
        if bitmask & (1 << shift) != 0 {
            let byte = *delta.get(*cursor).ok_or(PackchainError::MalformedDelta {
                reason: truncated_reason,
            })?;
            value |= u32::from(byte) << (u32::from(shift) * 8);
            *cursor += 1;
        }
    }
    Ok(value)
}

/// Git's documented default copy size when the delta's size operand is
/// zero (`pack-format.txt`: "if the size is zero, it is assumed to be
/// 0x10000").
const GIT_DELTA_DEFAULT_COPY_SIZE: u32 = 0x1_0000;

/// Handle a copy-from-base opcode (high bit set). Low 4 bits of `op`
/// signal which offset bytes follow; the next 3 bits signal which size
/// bytes follow. A zero size means git's documented default
/// ([`GIT_DELTA_DEFAULT_COPY_SIZE`]).
fn apply_delta_copy_op(
    op: u8,
    delta: &[u8],
    cursor: &mut usize,
    base: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), PackchainError> {
    let copy_offset = read_packed_operand(delta, cursor, op, 4, "truncated delta copy offset")?;
    let mut copy_size =
        read_packed_operand(delta, cursor, op >> 4, 3, "truncated delta copy size")?;
    if copy_size == 0 {
        copy_size = GIT_DELTA_DEFAULT_COPY_SIZE;
    }
    let start = copy_offset as usize;
    let end = start
        .checked_add(copy_size as usize)
        .ok_or(PackchainError::MalformedDelta {
            reason: "copy span overflow",
        })?;
    if end > base.len() {
        return Err(PackchainError::MalformedDelta {
            reason: "copy span exceeds base object",
        });
    }
    out.extend_from_slice(&base[start..end]);
    Ok(())
}

/// Handle an insert opcode. Low 7 bits of `op` are the literal length;
/// that many bytes follow and are copied verbatim into `out`.
fn apply_delta_insert_op(
    op: u8,
    delta: &[u8],
    cursor: &mut usize,
    out: &mut Vec<u8>,
) -> Result<(), PackchainError> {
    let len = op as usize;
    let end = cursor
        .checked_add(len)
        .ok_or(PackchainError::MalformedDelta {
            reason: "insert span overflow",
        })?;
    if end > delta.len() {
        return Err(PackchainError::MalformedDelta {
            reason: "insert span exceeds delta payload",
        });
    }
    out.extend_from_slice(&delta[*cursor..end]);
    *cursor = end;
    Ok(())
}

/// Read the variable-length size encoding used at the head of a delta
/// payload (LEB128-ish: 7 bits per byte, MSB = continuation).
fn read_size_varint(data: &[u8], mut cursor: usize) -> Option<(u64, usize)> {
    let start = cursor;
    let mut value: u64 = 0;
    let mut shift = 0u32;
    loop {
        let byte = *data.get(cursor)?;
        cursor += 1;
        value |= u64::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some((value, cursor - start));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha40(s: &str) -> Sha40 {
        Sha40::try_new(s).expect("test fixture sha is valid")
    }

    #[test]
    fn parse_path_rejects_empty() {
        let err = parse_path("").unwrap_err();
        assert!(matches!(err, PackchainError::MalformedPath { .. }));
    }

    #[test]
    fn parse_path_rejects_absolute() {
        let err = parse_path("/etc/passwd").unwrap_err();
        let PackchainError::MalformedPath { reason, .. } = err else {
            panic!("expected MalformedPath");
        };
        assert!(reason.contains("absolute"));
    }

    #[test]
    fn parse_path_rejects_dotdot() {
        let err = parse_path("src/../etc").unwrap_err();
        assert!(matches!(err, PackchainError::MalformedPath { .. }));
    }

    #[test]
    fn parse_path_rejects_dot() {
        let err = parse_path("./src").unwrap_err();
        assert!(matches!(err, PackchainError::MalformedPath { .. }));
    }

    #[test]
    fn parse_path_rejects_double_slash() {
        let err = parse_path("src//main.rs").unwrap_err();
        assert!(matches!(err, PackchainError::MalformedPath { .. }));
    }

    #[test]
    fn parse_path_rejects_trailing_slash() {
        let err = parse_path("src/main.rs/").unwrap_err();
        assert!(matches!(err, PackchainError::MalformedPath { .. }));
    }

    #[test]
    fn parse_path_accepts_nested() {
        let segs = parse_path("src/lib/mod.rs").unwrap();
        assert_eq!(segs, vec!["src", "lib", "mod.rs"]);
    }

    #[test]
    fn parse_path_accepts_single_segment() {
        let segs = parse_path("Cargo.toml").unwrap();
        assert_eq!(segs, vec!["Cargo.toml"]);
    }

    const SHA_A: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHA_B: &str = "fedcba9876543210fedcba9876543210fedcba98";
    const SHA_C: &str = "1111111111111111111111111111111111111111";

    #[test]
    fn walk_path_finds_top_level_blob() {
        let mut tree = BTreeMap::new();
        tree.insert("Cargo.toml".to_owned(), PathNode::Blob(sha40(SHA_A)));
        let segs = parse_path("Cargo.toml").unwrap();
        let result = walk_path(&tree, &segs, "refs/heads/main", "Cargo.toml").unwrap();
        assert_eq!(result.as_str(), SHA_A);
    }

    #[test]
    fn walk_path_descends_subtree() {
        let mut subtree = BTreeMap::new();
        subtree.insert("main.rs".to_owned(), PathNode::Blob(sha40(SHA_A)));
        let mut tree = BTreeMap::new();
        tree.insert("src".to_owned(), PathNode::Tree(subtree));
        let segs = parse_path("src/main.rs").unwrap();
        let result = walk_path(&tree, &segs, "refs/heads/main", "src/main.rs").unwrap();
        assert_eq!(result.as_str(), SHA_A);
    }

    #[test]
    fn walk_path_missing_returns_path_not_found() {
        let mut tree = BTreeMap::new();
        tree.insert("Cargo.toml".to_owned(), PathNode::Blob(sha40(SHA_A)));
        let segs = parse_path("missing.txt").unwrap();
        let err = walk_path(&tree, &segs, "refs/heads/main", "missing.txt").unwrap_err();
        assert!(matches!(err, PackchainError::PathNotFound { .. }));
    }

    #[test]
    fn walk_path_directory_returns_path_not_a_blob() {
        let mut subtree = BTreeMap::new();
        subtree.insert("main.rs".to_owned(), PathNode::Blob(sha40(SHA_A)));
        let mut tree = BTreeMap::new();
        tree.insert("src".to_owned(), PathNode::Tree(subtree));
        let segs = parse_path("src").unwrap();
        let err = walk_path(&tree, &segs, "refs/heads/main", "src").unwrap_err();
        assert!(matches!(err, PackchainError::PathNotABlob { .. }));
    }

    #[test]
    fn walk_path_through_blob_returns_not_found() {
        let mut tree = BTreeMap::new();
        tree.insert("Cargo.toml".to_owned(), PathNode::Blob(sha40(SHA_A)));
        let segs = parse_path("Cargo.toml/extra").unwrap();
        let err = walk_path(&tree, &segs, "refs/heads/main", "Cargo.toml/extra").unwrap_err();
        assert!(matches!(err, PackchainError::PathNotFound { .. }));
    }

    #[test]
    fn read_size_varint_single_byte() {
        let (v, n) = read_size_varint(&[0x05], 0).unwrap();
        assert_eq!(v, 5);
        assert_eq!(n, 1);
    }

    #[test]
    fn read_size_varint_multi_byte() {
        // 0x83 = 0b10000011 → low 7 bits 3, continuation set.
        // 0x02 = 0b00000010 → low 7 bits 2, no continuation.
        // Decoded: 3 | (2 << 7) = 3 | 256 = 259.
        let (v, n) = read_size_varint(&[0x83, 0x02], 0).unwrap();
        assert_eq!(v, 259);
        assert_eq!(n, 2);
    }

    #[test]
    fn read_size_varint_truncated() {
        // Continuation bit set on last available byte.
        assert!(read_size_varint(&[0x80], 0).is_none());
    }

    #[test]
    fn cache_default_starts_empty() {
        // Capacity (`capacity_bytes`) is not part of the public API
        // surface, so this test only covers what is observable: a
        // freshly-defaulted cache has zero entries and zero resident
        // bytes. The 64 MiB default value itself is checked by the
        // single-entry budget check in `cache_default_rejects_oversize_entry`.
        let cache = PackIndexCache::default();
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
        assert_eq!(cache.resident_bytes(), 0);
    }

    /// Pin the default capacity (`DEFAULT_CACHE_CAPACITY_BYTES`) by
    /// observing the boundary the public API exposes: an entry one
    /// byte over the documented 64 MiB cap is silently rejected, an
    /// entry exactly at the cap is accepted. A regression that
    /// changed the default to a different power of two would flip
    /// one of these two assertions.
    #[test]
    fn cache_default_enforces_64mib_capacity() {
        let cache = PackIndexCache::default();
        // Just over: rejected.
        cache.insert(
            ("p".into(), sha40(SHA_A)),
            Arc::new(make_dummy_index(DEFAULT_CACHE_CAPACITY_BYTES + 1)),
        );
        assert_eq!(cache.len(), 0, "entry over 64 MiB must be rejected");
        // Exactly at: accepted.
        cache.insert(
            ("p".into(), sha40(SHA_B)),
            Arc::new(make_dummy_index(DEFAULT_CACHE_CAPACITY_BYTES)),
        );
        assert_eq!(cache.len(), 1, "entry at 64 MiB must be accepted");
    }

    #[test]
    fn cache_explicit_capacity_zero_disables_caching() {
        let cache = PackIndexCache::new(0);
        // Inserting any non-empty entry must be a no-op (single-entry
        // budget check).
        let dummy = make_dummy_index(1_024);
        cache.insert(("p".into(), sha40(SHA_A)), Arc::new(dummy));
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn cache_evicts_lru_when_over_capacity() {
        let cache = PackIndexCache::new(3_000);
        cache.insert(
            ("p".into(), sha40(SHA_A)),
            Arc::new(make_dummy_index(1_000)),
        );
        cache.insert(
            ("p".into(), sha40(SHA_B)),
            Arc::new(make_dummy_index(1_000)),
        );
        cache.insert(
            ("p".into(), sha40(SHA_C)),
            Arc::new(make_dummy_index(1_000)),
        );
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.resident_bytes(), 3_000);

        // Touch SHA_A so SHA_B becomes LRU. Then insert a fourth entry
        // that pushes us over capacity — SHA_B must be evicted.
        let _ = cache.get(&("p".into(), sha40(SHA_A)));
        cache.insert(
            (
                "p".into(),
                sha40("dddddddddddddddddddddddddddddddddddddddd"),
            ),
            Arc::new(make_dummy_index(1_000)),
        );
        assert_eq!(cache.len(), 3);
        assert!(cache.get(&("p".into(), sha40(SHA_A))).is_some());
        assert!(cache.get(&("p".into(), sha40(SHA_B))).is_none());
    }

    #[test]
    fn cache_repeated_inserts_replace_accounting() {
        let cache = PackIndexCache::new(10_000);
        let key: CacheKey = ("p".into(), sha40(SHA_A));
        cache.insert(key.clone(), Arc::new(make_dummy_index(1_000)));
        cache.insert(key.clone(), Arc::new(make_dummy_index(2_500)));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.resident_bytes(), 2_500);
    }

    /// Construct a [`CachedIndex`] without a real .idx file, only for
    /// exercising the LRU bookkeeping. The `file` field is left
    /// uninitialised by parsing a minimal hand-crafted v2 idx; this is
    /// not used by the cache-mechanics tests.
    fn make_dummy_index(bytes: u64) -> CachedIndex {
        // A minimal v2 idx that gix_pack accepts: signature, version,
        // 256 fan-out entries (all zero — zero objects), and a 20-byte
        // pack-trailer + 20-byte idx-trailer at the end.
        let mut data = Vec::with_capacity(8 + 256 * 4 + 40);
        data.extend_from_slice(b"\xfftOc"); // V2 signature
        data.extend_from_slice(&2u32.to_be_bytes()); // version 2
        for _ in 0..256 {
            data.extend_from_slice(&0u32.to_be_bytes()); // fan-out: 0 objects under each leading byte
        }
        data.extend_from_slice(&[0u8; 20]); // pack trailer placeholder
        data.extend_from_slice(&[0u8; 20]); // idx trailer placeholder
        let file = gix_pack::index::File::from_data(
            data,
            std::path::PathBuf::from("dummy.idx"),
            gix_hash::Kind::Sha1,
        )
        .expect("hand-crafted minimal v2 idx parses");
        CachedIndex {
            file,
            sorted_offsets: Vec::new(),
            bytes,
        }
    }

    #[test]
    fn sha40_to_object_id_roundtrips() {
        let sha = sha40(SHA_A);
        let oid = sha40_to_object_id(&sha);
        assert_eq!(oid.to_string(), SHA_A);
    }

    // --- apply_delta -------------------------------------------------------
    //
    // Hand-craft delta payloads (per the git delta format) and verify
    // [`apply_delta`] reconstructs the right output. Without this,
    // OFS_DELTA / REF_DELTA paths in [`decode_entry`] are not exercised
    // by any test — the integration suite uses small text files that
    // gix-pack does not delta-encode.

    fn base_blob(payload: &[u8]) -> ResolvedObject {
        ResolvedObject {
            payload: payload.to_vec(),
            kind: ObjectKind::Blob,
        }
    }

    /// Encode a single varint per the delta header format (LEB128-ish:
    /// 7 bits per byte, MSB = continuation).
    fn varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    #[test]
    fn apply_delta_insert_only_round_trips() {
        // Empty base, delta is pure-insert. Reconstructed payload
        // must be byte-equal to the literal data the insert opcode
        // carries.
        let base = base_blob(b"");
        let literal = b"Hello, packchain!";
        let mut delta = Vec::new();
        delta.extend_from_slice(&varint(0)); // src_size
        delta.extend_from_slice(&varint(literal.len() as u64)); // dst_size
        // Insert opcode: low 7 bits = literal length. The literal is
        // 17 bytes here, so the cast to u8 is the desired narrow.
        delta.push(u8::try_from(literal.len()).expect("test literal fits in 7 bits"));
        delta.extend_from_slice(literal);
        let out = apply_delta(&base, &delta).expect("insert-only delta applies");
        assert_eq!(out.payload, literal);
        assert_eq!(out.kind, ObjectKind::Blob);
    }

    #[test]
    fn apply_delta_copy_only_round_trips() {
        // Copy first 5 bytes from a 10-byte base.
        let base = base_blob(b"abcdefghij");
        let mut delta = Vec::new();
        delta.extend_from_slice(&varint(10)); // src_size
        delta.extend_from_slice(&varint(5)); // dst_size
        // Copy opcode: MSB=1; bit0 set (1 byte of offset follows);
        // bit4 set (1 byte of size follows).
        delta.push(0b1001_0001);
        delta.push(0); // offset = 0
        delta.push(5); // size = 5
        let out = apply_delta(&base, &delta).expect("copy-only delta applies");
        assert_eq!(out.payload, b"abcde");
    }

    #[test]
    fn apply_delta_mixed_copy_and_insert_round_trips() {
        // Reconstruct "HELLO world" by copying "HELLO" from the base
        // and inserting " world".
        let base = base_blob(b"HELLO!?");
        let mut delta = Vec::new();
        delta.extend_from_slice(&varint(7)); // src_size
        delta.extend_from_slice(&varint(11)); // dst_size: "HELLO world"
        // Copy 5 bytes from offset 0.
        delta.push(0b1001_0001);
        delta.push(0);
        delta.push(5);
        // Insert 6 literal bytes.
        let literal = b" world";
        delta.push(u8::try_from(literal.len()).expect("test literal fits in 7 bits"));
        delta.extend_from_slice(literal);
        let out = apply_delta(&base, &delta).expect("mixed delta applies");
        assert_eq!(out.payload, b"HELLO world");
    }

    #[test]
    fn apply_delta_preserves_base_kind() {
        // A delta against a Tree base must produce a Tree result —
        // delta application doesn't change object kind. Confirms the
        // `kind: base.kind` line at the bottom of `apply_delta`.
        let base = ResolvedObject {
            payload: b"x".to_vec(),
            kind: ObjectKind::Tree,
        };
        let mut delta = Vec::new();
        delta.extend_from_slice(&varint(1));
        delta.extend_from_slice(&varint(1));
        delta.push(0b1001_0001);
        delta.push(0);
        delta.push(1);
        let out = apply_delta(&base, &delta).expect("kind-preserving delta applies");
        assert_eq!(out.kind, ObjectKind::Tree);
    }

    #[test]
    fn apply_delta_rejects_source_size_mismatch() {
        // Delta claims source size 99, base is 1 byte. Must reject
        // before producing output.
        let base = base_blob(b"x");
        let mut delta = Vec::new();
        delta.extend_from_slice(&varint(99));
        delta.extend_from_slice(&varint(1));
        delta.push(1);
        delta.push(b'y');
        let err = apply_delta(&base, &delta).expect_err("size mismatch must fail");
        assert!(
            matches!(err, PackchainError::MalformedDelta { reason } if reason.contains("source size")),
            "expected MalformedDelta source-size mismatch, got {err:?}",
        );
    }

    #[test]
    fn apply_delta_rejects_copy_past_base_end() {
        // Copy opcode asks for bytes [3..8) from a 4-byte base. Bounds
        // check must fire.
        let base = base_blob(b"abcd");
        let mut delta = Vec::new();
        delta.extend_from_slice(&varint(4));
        delta.extend_from_slice(&varint(5));
        delta.push(0b1001_0001);
        delta.push(3); // offset = 3
        delta.push(5); // size = 5 → end = 8 > 4
        let err = apply_delta(&base, &delta).expect_err("out-of-range copy must fail");
        assert!(
            matches!(err, PackchainError::MalformedDelta { reason } if reason.contains("copy span")),
            "expected MalformedDelta copy-span error, got {err:?}",
        );
    }

    #[test]
    fn apply_delta_rejects_dst_size_over_cap() {
        // dst_size header above MAX_DECOMPRESSED_BYTES must reject
        // before allocating.
        let base = base_blob(b"");
        let mut delta = Vec::new();
        delta.extend_from_slice(&varint(0));
        delta.extend_from_slice(&varint(MAX_DECOMPRESSED_BYTES + 1));
        let err = apply_delta(&base, &delta).expect_err("oversize dst must fail");
        assert!(
            matches!(err, PackchainError::MalformedDelta { reason } if reason.contains("1 GiB cap")),
            "expected MalformedDelta cap error, got {err:?}",
        );
    }

    #[test]
    fn apply_delta_rejects_reserved_zero_opcode() {
        // 0x00 is reserved per the git delta format.
        let base = base_blob(b"");
        let mut delta = Vec::new();
        delta.extend_from_slice(&varint(0));
        delta.extend_from_slice(&varint(0));
        delta.push(0); // reserved opcode
        let err = apply_delta(&base, &delta).expect_err("reserved opcode must fail");
        assert!(
            matches!(err, PackchainError::MalformedDelta { reason } if reason.contains("zero opcode")),
            "expected MalformedDelta reserved-opcode error, got {err:?}",
        );
    }

    #[test]
    fn apply_delta_copy_size_zero_substitutes_default() {
        // Copy opcode with NO size operand bytes (high bits 4..6 of op
        // all zero) — the decoded size is zero, which per git's spec
        // must be substituted with `GIT_DELTA_DEFAULT_COPY_SIZE`
        // (0x10000). We can't easily verify the produced length without
        // a >=64 KiB base, so use a small base and check the substitution
        // fires by observing the bounds-check failure: with the default
        // substituted, the span (offset 0, size 0x10000) overshoots the
        // 1-byte base and triggers "copy span exceeds base object". If
        // the substitution were skipped, the span would be empty and
        // the post-loop "destination size" check would fire instead.
        let base = base_blob(b"x");
        let mut delta = Vec::new();
        delta.extend_from_slice(&varint(1));
        delta.extend_from_slice(&varint(2)); // dst_size irrelevant; copy errors first
        // Copy opcode: MSB=1, bit0 set (1 byte of offset follows), all
        // size bits (4..6) cleared.
        delta.push(0b1000_0001);
        delta.push(0); // offset = 0
        let err = apply_delta(&base, &delta)
            .expect_err("default-size substitution must fail bounds check");
        assert!(
            matches!(&err, PackchainError::MalformedDelta { reason } if reason.contains("copy span exceeds base")),
            "expected copy-span-exceeds-base (proves default size was substituted), got {err:?}",
        );
    }

    #[test]
    fn apply_delta_rejects_dst_size_undershoot() {
        // delta finishes (no more opcodes) but produced output is
        // shorter than the announced dst_size. The post-loop check
        // must catch this.
        let base = base_blob(b"abcdef");
        let mut delta = Vec::new();
        delta.extend_from_slice(&varint(6));
        delta.extend_from_slice(&varint(10)); // claim 10
        // ... but only emit 3 bytes via copy.
        delta.push(0b1001_0001);
        delta.push(0);
        delta.push(3);
        let err = apply_delta(&base, &delta).expect_err("undershoot must fail");
        assert!(
            matches!(err, PackchainError::MalformedDelta { reason } if reason.contains("destination size")),
            "expected MalformedDelta undershoot error, got {err:?}",
        );
    }

    #[test]
    fn apply_delta_rejects_overshoot() {
        // Delta announces dst_size=4 but emits 8 bytes via a single
        // copy op. Without the per-op bound, `out` would grow past
        // the announced size and only get caught by the post-loop
        // equality check — a malicious delta with a multi-TiB total
        // could OOM the helper before reaching that point. The
        // per-op bound must reject as soon as `out.len()` exceeds
        // `dst_size_usize`.
        let base = base_blob(b"abcdefgh");
        let mut delta = Vec::new();
        delta.extend_from_slice(&varint(8)); // src_size
        delta.extend_from_slice(&varint(4)); // dst_size — under-claim
        // Copy 8 bytes from offset 0 (overshoots dst_size).
        delta.push(0b1001_0001);
        delta.push(0);
        delta.push(8);
        let err = apply_delta(&base, &delta).expect_err("overshoot must fail");
        assert!(
            matches!(
                err,
                PackchainError::MalformedDelta {
                    reason: "produced object exceeds announced destination size"
                }
            ),
            "expected MalformedDelta overshoot error, got {err:?}",
        );
    }

    #[test]
    fn apply_delta_overshoot_check_fires_after_single_default_size_copy() {
        // Aggressive variant: small base (1 byte, repeated 16 times so
        // copy size 0x1_0000 stays in-bounds against the base), and a
        // copy opcode with size operand bits cleared so it falls back
        // to GIT_DELTA_DEFAULT_COPY_SIZE (0x1_0000 = 64 KiB). A single
        // op therefore emits 64 KiB, which must trip the per-op bound
        // when dst_size is set to 4. This proves the check fires
        // after the FIRST op, not just at end-of-loop — a chain of
        // such ops in a real attack would otherwise blow through
        // memory before the post-loop check ever ran.
        // Heap allocation avoids large_stack_arrays clippy lint (16 KiB cap).
        let base_payload = vec![b'x'; 0x1_0000];
        let base = base_blob(&base_payload);
        let mut delta = Vec::new();
        delta.extend_from_slice(&varint(0x1_0000)); // src_size matches base
        delta.extend_from_slice(&varint(4)); // dst_size — tiny
        // Copy opcode: MSB=1, bit0 set (1 byte of offset follows),
        // size bits (4..6) cleared so default 0x1_0000 substitutes.
        delta.push(0b1000_0001);
        delta.push(0); // offset = 0
        let err = apply_delta(&base, &delta).expect_err("default-size overshoot must fail");
        assert!(
            matches!(
                err,
                PackchainError::MalformedDelta {
                    reason: "produced object exceeds announced destination size"
                }
            ),
            "expected MalformedDelta overshoot error after first op, got {err:?}",
        );
    }

    #[test]
    fn apply_delta_exact_match_does_not_trip_overshoot_check() {
        // Boundary: a delta that exactly fills dst_size must succeed.
        // The per-op bound rejects only `>`, never `==`, so an exact
        // match flows through to the post-loop equality check.
        let base = base_blob(b"abcd");
        let mut delta = Vec::new();
        delta.extend_from_slice(&varint(4));
        delta.extend_from_slice(&varint(4));
        delta.push(0b1001_0001);
        delta.push(0);
        delta.push(4);
        let out = apply_delta(&base, &delta).expect("exact-match delta applies");
        assert_eq!(out.payload, b"abcd");
    }

    // --- delta-depth guard (issue #83) -------------------------------------
    //
    // The fix moves the depth guard from `read_object_from_chain` into
    // `decode_entry`, the single chokepoint every recursive resolution
    // path traverses. These tests exercise both the boundary and the
    // OFS_DELTA bypass that the old shape allowed.
    //
    // The synthesised packs use bounded payloads (small literal byte
    // strings), so the `as usize` / `as u8` casts cannot truncate at
    // runtime. Suppressing the lints here keeps the test setup direct;
    // production code paths use `try_from`.

    use crate::object_store::mock::MockStore;
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;

    /// Encode a pack-entry header per gix-pack's canonical encoding:
    /// 4-bit type tag + 4-bit low size, then 7-bit continuation bytes
    /// for the upper bits of `size`. This is the inverse of
    /// `gix_pack::data::entry::decode::parse_header_info`.
    #[allow(clippy::cast_possible_truncation)]
    fn encode_pack_entry_header(type_id: u8, mut size: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let low4 = (size & 0x0f) as u8;
        size >>= 4;
        let mut byte = (type_id << 4) | low4;
        if size != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        while size != 0 {
            let mut next = (size & 0x7f) as u8;
            size >>= 7;
            if size != 0 {
                next |= 0x80;
            }
            out.push(next);
        }
        out
    }

    /// Encode an `OFS_DELTA` `base_distance` per the gix-pack
    /// `parse_leb64` shape (offset-LEB128, with implicit `+1` between
    /// continuation bytes).
    #[allow(clippy::cast_possible_truncation)]
    fn encode_ofs_delta_distance(distance: u64) -> Vec<u8> {
        // The decoder's invariant: `value = ((((b0 & 0x7f) + 1) << 7) |
        // (b1 & 0x7f) + 1) << 7) | ...` — i.e. each continuation step
        // adds one before shifting. Build the byte sequence by
        // repeatedly subtracting one and shifting right, so the decoder
        // reconstructs the original distance.
        let mut bytes = Vec::new();
        let mut v = distance;
        bytes.push((v & 0x7f) as u8);
        v >>= 7;
        while v != 0 {
            v -= 1;
            bytes.push(((v & 0x7f) as u8) | 0x80);
            v >>= 7;
        }
        bytes.reverse();
        bytes
    }

    fn zlib_compress(data: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).expect("zlib encode");
        e.finish().expect("zlib finish")
    }

    /// Build a delta payload with the canonical varint header and a
    /// single-insert opcode that copies `payload` literally. The result
    /// reconstructs to `payload` regardless of the base content (the
    /// source-size check still runs against the base, so callers must
    /// pass `base_size` matching their base).
    #[allow(clippy::cast_possible_truncation)]
    fn make_insert_delta(base_size: u64, payload: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        // src_size and dst_size as size-varint (low-bit-first, MSB
        // continuation), per `read_size_varint`.
        let put_varint = |mut v: u64, buf: &mut Vec<u8>| loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                buf.push(byte);
                return;
            }
            buf.push(byte | 0x80);
        };
        put_varint(base_size, &mut d);
        put_varint(payload.len() as u64, &mut d);
        // Insert opcode: low 7 bits are length. Tests use small literals.
        assert!(payload.len() < 0x80, "test literal too long for one insert");
        d.push(payload.len() as u8);
        d.extend_from_slice(payload);
        d
    }

    /// Append a complete pack entry (header + zlib-compressed payload)
    /// to `pack` and record the entry's start offset in `offsets`.
    #[allow(clippy::cast_possible_truncation)]
    fn push_pack_entry(
        pack: &mut Vec<u8>,
        offsets: &mut Vec<u64>,
        type_id: u8,
        ofs_delta_distance: Option<u64>,
        decompressed_payload: &[u8],
    ) {
        let start = pack.len() as u64;
        offsets.push(start);
        pack.extend(encode_pack_entry_header(
            type_id,
            decompressed_payload.len() as u64,
        ));
        if let Some(d) = ofs_delta_distance {
            pack.extend(encode_ofs_delta_distance(d));
        }
        pack.extend(zlib_compress(decompressed_payload));
    }

    /// Wire up a `PackIndexCache` with a hand-rolled `CachedIndex`
    /// whose only contract with the test is `sorted_offsets`. The
    /// stored idx file is a zero-entry v2 stub; tests call
    /// `decode_entry` directly so the file-side lookup is never used.
    fn install_cached_index(
        cache: &PackIndexCache,
        prefix: &str,
        content_sha: &Sha40,
        offsets: Vec<u64>,
    ) {
        let cached = CachedIndex {
            file: minimal_v2_idx(),
            sorted_offsets: offsets,
            bytes: 1_024,
        };
        cache.insert((prefix.to_owned(), content_sha.clone()), Arc::new(cached));
    }

    fn minimal_v2_idx() -> gix_pack::index::File<Vec<u8>> {
        let mut data = Vec::with_capacity(8 + 256 * 4 + 40);
        data.extend_from_slice(b"\xfftOc");
        data.extend_from_slice(&2u32.to_be_bytes());
        for _ in 0..256 {
            data.extend_from_slice(&0u32.to_be_bytes());
        }
        data.extend_from_slice(&[0u8; 20]);
        data.extend_from_slice(&[0u8; 20]);
        gix_pack::index::File::from_data(
            data,
            std::path::PathBuf::from("dummy.idx"),
            gix_hash::Kind::Sha1,
        )
        .expect("hand-crafted minimal v2 idx parses")
    }

    /// `decode_entry` is the single chokepoint for the depth budget:
    /// invoking it with `*depth > MAX_DELTA_DEPTH` must fail before
    /// any decode work happens. This is what catches a recursive
    /// caller (`REF_DELTA` via `read_object_from_chain`, or `OFS_DELTA`
    /// directly) blowing past the cap.
    #[tokio::test]
    async fn decode_entry_rejects_when_depth_already_over_cap() {
        let store = MockStore::new();
        let cache = PackIndexCache::default();
        let chain: Vec<ChainSegment> = Vec::new();
        let content_sha = sha40(SHA_A);
        // Any well-formed Blob entry — depth check fires first, so
        // contents are immaterial.
        let mut pack = Vec::new();
        let mut offsets = Vec::new();
        push_pack_entry(&mut pack, &mut offsets, 3 /* BLOB */, None, b"x");

        let mut depth = MAX_DELTA_DEPTH + 1;
        let err = decode_entry(
            &store,
            None,
            &chain,
            &content_sha,
            offsets[0],
            &pack[usize::try_from(offsets[0]).unwrap()..],
            &cache,
            &mut depth,
        )
        .await
        .expect_err("over-cap depth must fail");
        assert!(
            matches!(err, PackchainError::DeltaTooDeep { max } if max == MAX_DELTA_DEPTH),
            "expected DeltaTooDeep, got {err:?}",
        );
    }

    /// At exactly `*depth == MAX_DELTA_DEPTH` and a non-delta entry,
    /// `decode_entry` must succeed: a non-delta base reached at the
    /// boundary is the deepest legal point in the chain. This is the
    /// off-by-one boundary opposite the failing case above.
    #[tokio::test]
    async fn decode_entry_at_cap_with_non_delta_base_succeeds() {
        let store = MockStore::new();
        let cache = PackIndexCache::default();
        let chain: Vec<ChainSegment> = Vec::new();
        let content_sha = sha40(SHA_A);
        let mut pack = Vec::new();
        let mut offsets = Vec::new();
        push_pack_entry(
            &mut pack,
            &mut offsets,
            3, /* BLOB */
            None,
            b"deepest-base",
        );

        let mut depth = MAX_DELTA_DEPTH;
        let resolved = decode_entry(
            &store,
            None,
            &chain,
            &content_sha,
            offsets[0],
            &pack[usize::try_from(offsets[0]).unwrap()..],
            &cache,
            &mut depth,
        )
        .await
        .expect("blob at MAX boundary must decode");
        assert_eq!(resolved.payload, b"deepest-base");
        assert_eq!(resolved.kind, ObjectKind::Blob);
    }

    /// Pre-fix regression: a pure-`OFS_DELTA` chain bypassed the depth
    /// guard because the recursive call hopped through `decode_entry`
    /// directly without re-entering `read_object_from_chain`. With the
    /// guard moved into `decode_entry`, a 2-entry pack (a base blob +
    /// one `OFS_DELTA` pointing at it) entered with
    /// `depth = MAX_DELTA_DEPTH` must fail on the recursive call to the
    /// base, even though the outer entry is itself a single layer.
    ///
    /// Synthesised in-memory pack — does NOT actually approach a real
    /// stack-overflow depth, so this test would behave identically (and
    /// pass) on the unfixed code's `REF_DELTA` path. It catches the
    /// `OFS_DELTA` bypass specifically.
    #[tokio::test]
    async fn ofs_delta_recursion_consumes_depth_budget() {
        let store = MockStore::new();
        let cache = PackIndexCache::default();
        let chain: Vec<ChainSegment> = Vec::new();
        let content_sha = sha40(SHA_A);

        let base_payload = b"base-blob";
        let mut pack = Vec::new();
        let mut offsets = Vec::new();
        // Entry 0: BLOB base.
        push_pack_entry(&mut pack, &mut offsets, 3, None, base_payload);
        // Entry 1: OFS_DELTA pointing back to entry 0. The encoded
        // distance per gix-pack is `entry_offset - base_offset`; entry
        // 1's start is `pack.len()` before the push, so capture it
        // explicitly.
        let delta = make_insert_delta(base_payload.len() as u64, b"reconstructed");
        let entry1_start = pack.len() as u64;
        let distance = entry1_start - offsets[0];
        push_pack_entry(&mut pack, &mut offsets, 6, Some(distance), &delta);

        // Plant the pack body in the store under the canonical key so
        // `fetch_entry_bytes` can range-GET the base. The cache is
        // pre-populated with the offsets so no .idx round-trip is
        // needed.
        store.insert(pack_key(None, &content_sha), Bytes::from(pack.clone()));
        install_cached_index(&cache, "", &content_sha, offsets.clone());

        // Enter at exactly MAX_DELTA_DEPTH so the OFS_DELTA pass bumps
        // the budget to MAX+1 and the recursive base decode trips the
        // guard.
        let mut depth = MAX_DELTA_DEPTH;
        let err = decode_entry(
            &store,
            None,
            &chain,
            &content_sha,
            offsets[1],
            &pack[usize::try_from(offsets[1]).unwrap()..],
            &cache,
            &mut depth,
        )
        .await
        .expect_err("OFS_DELTA recursion must trip the depth guard");
        assert!(
            matches!(err, PackchainError::DeltaTooDeep { max } if max == MAX_DELTA_DEPTH),
            "expected DeltaTooDeep from OFS_DELTA recursion, got {err:?}",
        );
    }

    // --- terminal-entry size cap (issue #115) ------------------------------
    //
    // `fetch_entry_bytes` previously fell back to `get_bytes(&pack)` for
    // the last entry in a pack — unbounded by `MAX_RANGE_BYTES`. The fix
    // routes the terminal entry through a `HEAD` + ranged GET, enforcing
    // the same cap as non-terminal entries.

    /// Delegates everything to an inner `MockStore` except `head`, which
    /// returns a synthetic `size` chosen by the test. Lets us exercise
    /// the `> MAX_RANGE_BYTES` branch without actually allocating a
    /// multi-GiB body.
    struct FakeSizeStore {
        inner: MockStore,
        fake_size: u64,
    }

    #[async_trait::async_trait]
    impl ObjectStore for FakeSizeStore {
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
            opts: crate::object_store::PutOpts,
        ) -> Result<(), ObjectStoreError> {
            self.inner.put_bytes(key, body, opts).await
        }
        async fn put_if_absent(&self, key: &str, body: Bytes) -> Result<bool, ObjectStoreError> {
            self.inner.put_if_absent(key, body).await
        }
        async fn head(
            &self,
            key: &str,
        ) -> Result<crate::object_store::ObjectMeta, ObjectStoreError> {
            // Forward NotFound from the inner store, but report
            // `fake_size` on success regardless of the real body length.
            let meta = self.inner.head(key).await?;
            Ok(crate::object_store::ObjectMeta {
                size: self.fake_size,
                ..meta
            })
        }
        async fn copy(&self, src: &str, dst: &str) -> Result<(), ObjectStoreError> {
            self.inner.copy(src, dst).await
        }
        async fn delete(&self, key: &str) -> Result<(), ObjectStoreError> {
            self.inner.delete(key).await
        }
    }

    /// Happy path: the last entry's span fits under the cap, so
    /// `fetch_entry_bytes` issues a single bounded ranged GET and
    /// returns the tail of the pack starting at `pack_offset`.
    #[tokio::test]
    async fn fetch_entry_bytes_terminal_entry_under_cap_succeeds() {
        let store = MockStore::new();
        let cache = PackIndexCache::default();
        let content_sha = sha40(SHA_A);

        // Plant a tiny pack body. The entry shape doesn't matter — we
        // only assert on the bytes `fetch_entry_bytes` returns.
        let body: &[u8] = b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09";
        store.insert(pack_key(None, &content_sha), Bytes::from(body.to_vec()));
        // Pretend the only entry starts at offset 2 — sorted_offsets
        // contains it and nothing greater, so `next_offset` is `None`
        // and the terminal-entry branch fires.
        install_cached_index(&cache, "", &content_sha, vec![2]);
        let idx = cache
            .get(&(String::new(), content_sha.clone()))
            .expect("cache hit");

        let got = fetch_entry_bytes(&store, None, &content_sha, 2, &idx)
            .await
            .expect("terminal entry under cap must succeed");
        assert_eq!(got.as_ref(), &body[2..]);
    }

    /// Security regression for issue #115: when the implied terminal
    /// range exceeds `MAX_RANGE_BYTES`, the fetcher must fail with a
    /// typed error rather than allocate a multi-GiB buffer.
    #[tokio::test]
    async fn fetch_entry_bytes_terminal_entry_over_cap_rejected() {
        let inner = MockStore::new();
        let cache = PackIndexCache::default();
        let content_sha = sha40(SHA_A);

        // The wrapper's `head` will report `MAX_RANGE_BYTES + 1`, so the
        // implied span `(end - pack_offset)` with `pack_offset = 0`
        // exceeds the cap. The body is a stub — `get_bytes_range` must
        // never be called.
        inner.insert(pack_key(None, &content_sha), Bytes::from_static(b"stub"));
        install_cached_index(&cache, "", &content_sha, vec![0]);
        let idx = cache
            .get(&(String::new(), content_sha.clone()))
            .expect("cache hit");

        let store = FakeSizeStore {
            inner,
            fake_size: MAX_RANGE_BYTES + 1,
        };

        let err = fetch_entry_bytes(&store, None, &content_sha, 0, &idx)
            .await
            .expect_err("terminal entry above cap must be rejected");
        assert!(
            matches!(
                err,
                PackchainError::MalformedPackEntry { offset: 0, ref reason }
                    if reason.contains("exceeds") && reason.contains("cap")
            ),
            "expected MalformedPackEntry size-cap error, got {err:?}",
        );
    }

    /// `pack_offset >= pack_len` on the terminal branch is an
    /// out-of-bounds index. The fetcher must report it as
    /// `MalformedPackEntry` rather than issue a zero-length range GET.
    #[tokio::test]
    async fn fetch_entry_bytes_terminal_entry_offset_past_eof_rejected() {
        let store = MockStore::new();
        let cache = PackIndexCache::default();
        let content_sha = sha40(SHA_A);

        store.insert(pack_key(None, &content_sha), Bytes::from_static(b"abc"));
        // `sorted_offsets` contains an offset at/past the body length,
        // so `next_offset` is `None` and the terminal branch fires.
        install_cached_index(&cache, "", &content_sha, vec![100]);
        let idx = cache
            .get(&(String::new(), content_sha.clone()))
            .expect("cache hit");

        let err = fetch_entry_bytes(&store, None, &content_sha, 100, &idx)
            .await
            .expect_err("offset beyond EOF must be rejected");
        assert!(
            matches!(
                err,
                PackchainError::MalformedPackEntry { offset: 100, ref reason }
                    if reason.contains("beyond pack EOF")
            ),
            "expected MalformedPackEntry EOF error, got {err:?}",
        );
    }

    /// Below the cap, the same `OFS_DELTA` shape decodes cleanly. This
    /// pins the positive case so the boundary test above is meaningful
    /// (without it, a regression that always returned `DeltaTooDeep`
    /// would still pass the over-cap assertion).
    #[tokio::test]
    async fn ofs_delta_below_cap_decodes() {
        let store = MockStore::new();
        let cache = PackIndexCache::default();
        let chain: Vec<ChainSegment> = Vec::new();
        let content_sha = sha40(SHA_A);

        let base_payload = b"base";
        let mut pack = Vec::new();
        let mut offsets = Vec::new();
        push_pack_entry(&mut pack, &mut offsets, 3, None, base_payload);
        let delta = make_insert_delta(base_payload.len() as u64, b"hi");
        let entry1_start = pack.len() as u64;
        let distance = entry1_start - offsets[0];
        push_pack_entry(&mut pack, &mut offsets, 6, Some(distance), &delta);

        store.insert(pack_key(None, &content_sha), Bytes::from(pack.clone()));
        install_cached_index(&cache, "", &content_sha, offsets.clone());

        let mut depth = 0u32;
        let resolved = decode_entry(
            &store,
            None,
            &chain,
            &content_sha,
            offsets[1],
            &pack[usize::try_from(offsets[1]).unwrap()..],
            &cache,
            &mut depth,
        )
        .await
        .expect("OFS_DELTA decodes below cap");
        assert_eq!(resolved.payload, b"hi");
        assert_eq!(resolved.kind, ObjectKind::Blob);
    }

    // --- concurrent-GC retry loop (issue #136) -----------------------------
    //
    // `read_blob` must transparently retry `PackMissing` failures that
    // were caused by a concurrent `manage gc sweep` deleting compacted-
    // away packs. The retry reloads `chain.json` and inspects whether
    // the missing key is still referenced (data loss → fail fast) or
    // gone (GC → retry).

    use crate::packchain::keys::chain_key;
    use crate::packchain::schema::ChainManifest;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_chain_with(tip_hex: &str, pack_sha_hex: &str) -> ChainManifest {
        ChainManifest {
            v: ChainManifest::SCHEMA_VERSION,
            tip: sha40(tip_hex),
            full_at: sha40(tip_hex),
            segments: vec![ChainSegment {
                sha: sha40(tip_hex),
                parent_sha: None,
                pack: format!("packs/{pack_sha_hex}.pack"),
                bytes: 1_024,
            }],
        }
    }

    #[test]
    fn chain_references_pack_key_matches_pack_and_idx_keys() {
        let chain = make_chain_with(SHA_A, SHA_B);
        // Both `.pack` and `.idx` belonging to the same content-sha
        // are considered referenced.
        assert!(chain_references_pack_key(&chain, None, &format!("packs/{SHA_B}.pack")).unwrap());
        assert!(chain_references_pack_key(&chain, None, &format!("packs/{SHA_B}.idx")).unwrap());
    }

    #[test]
    fn chain_references_pack_key_returns_false_for_unreferenced_pack() {
        let chain = make_chain_with(SHA_A, SHA_B);
        assert!(!chain_references_pack_key(&chain, None, &format!("packs/{SHA_C}.pack")).unwrap());
        assert!(!chain_references_pack_key(&chain, None, &format!("packs/{SHA_C}.idx")).unwrap());
    }

    #[test]
    fn chain_references_pack_key_respects_prefix() {
        let chain = make_chain_with(SHA_A, SHA_B);
        // With a prefix, the membership check is against the
        // prefix-joined key — an un-prefixed key for the same
        // content-sha is *not* a match (the join differs).
        assert!(
            chain_references_pack_key(&chain, Some("repo"), &format!("repo/packs/{SHA_B}.pack"))
                .unwrap()
        );
        assert!(
            !chain_references_pack_key(&chain, Some("repo"), &format!("packs/{SHA_B}.pack"))
                .unwrap()
        );
    }

    #[test]
    fn chain_references_pack_key_returns_false_for_malformed_missing_key() {
        // Commit 8fbe693's refactor short-circuits on a missing_key
        // that fails to parse as `[<prefix>/]packs/<sha>.{pack,idx}` —
        // the function returns Ok(false) without iterating segments.
        // This pins that behavior so a future change that re-routes
        // malformed keys through the per-segment path (or that
        // surfaces a parse error to the caller) fails this test.
        let chain = make_chain_with(SHA_A, SHA_B);
        // No `packs/` segment.
        assert!(!chain_references_pack_key(&chain, None, "weird/key").unwrap());
        // `packs/` present but non-hex stem — fails Sha40 validation.
        assert!(!chain_references_pack_key(&chain, None, "packs/not-a-sha.pack").unwrap());
        // Wrong extension.
        assert!(!chain_references_pack_key(&chain, None, &format!("packs/{SHA_B}.bin")).unwrap());
        // Empty key.
        assert!(!chain_references_pack_key(&chain, None, "").unwrap());
    }

    /// `ObjectStore` wrapper that serves a list of `chain.json` bodies
    /// in order: call 0 returns `bodies[0]`, call 1 returns `bodies[1]`,
    /// and so on. The last entry is repeated for any further calls.
    /// All other keys go through to the inner [`MockStore`].
    ///
    /// Lets a test simulate compact+sweep happening *between* the
    /// reader's chain reloads: the reader observes a new chain (a
    /// different segment set) on each retry without the test needing
    /// to race a real concurrent task.
    struct EvolvingChainStore {
        inner: MockStore,
        chain_key: String,
        bodies: Vec<Bytes>,
        calls: AtomicUsize,
        /// Counts `get_bytes` calls whose key ends in
        /// `/path-index.json`. Used by the issue-#136 contract test
        /// (`read_with_pack_missing_retries_does_not_reload_path_index`)
        /// to prove the retry path never re-reads path-index — only
        /// `chain.json` is reloaded.
        path_index_calls: AtomicUsize,
    }

    impl EvolvingChainStore {
        fn new(inner: MockStore, chain_key: String, bodies: Vec<Bytes>) -> Self {
            assert!(!bodies.is_empty(), "must supply at least one chain body");
            Self {
                inner,
                chain_key,
                bodies,
                calls: AtomicUsize::new(0),
                path_index_calls: AtomicUsize::new(0),
            }
        }

        fn chain_calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn path_index_calls(&self) -> usize {
            self.path_index_calls.load(Ordering::SeqCst)
        }
    }

    // Note: `put_path` is intentionally omitted from the forward list
    // to preserve the original behavior (trait default → `Self::put_bytes`
    // → `inner.put_bytes`), which bypasses `MockStore::put_path`. The
    // test never invokes `put_path` on this decorator, so the bypass is
    // a no-op in practice but kept for byte-for-byte parity.
    crate::delegate_to_inner_impl! {
        impl ObjectStore for EvolvingChainStore {
            forward: list, get_to_file, get_bytes_range,
                     put_bytes, put_if_absent,
                     head, copy, delete;

            async fn get_bytes(&self, key: &str) -> Result<Bytes, ObjectStoreError> {
                if key == self.chain_key {
                    let idx = self.calls.fetch_add(1, Ordering::SeqCst);
                    let pick = idx.min(self.bodies.len() - 1);
                    return Ok(self.bodies[pick].clone());
                }
                if key.ends_with("/path-index.json") {
                    self.path_index_calls.fetch_add(1, Ordering::SeqCst);
                }
                self.inner.get_bytes(key).await
            }
        }
    }

    /// Build a real v2 `.idx` carrying a single object: `target_sha` at
    /// pack offset `pack_offset`. `gix_pack`'s parser validates the
    /// fanout and table sizes, so this must match the format exactly.
    fn build_one_object_v2_idx(target_sha: &Sha40, pack_offset: u32) -> Vec<u8> {
        // Decode the 40-hex sha into 20 raw bytes. `gix_hash::ObjectId`
        // already does this for us — re-use it rather than hand-rolling
        // a hex parser.
        let oid = sha40_to_object_id(target_sha);
        let sha_bytes = oid.as_bytes();
        let first_byte = sha_bytes[0];

        let mut data = Vec::with_capacity(8 + 256 * 4 + 20 + 4 + 4 + 20 + 20);
        // V2 magic + version.
        data.extend_from_slice(b"\xfftOc");
        data.extend_from_slice(&2u32.to_be_bytes());
        // Fanout: cumulative count of objects with sha[0] <= i. With a
        // single object whose first byte is `first_byte`, every entry
        // from `first_byte` onwards is 1. `u8::try_from(i)` cannot fail
        // for `i in 0u16..256`.
        for i in 0u16..256 {
            let count = u32::from(u8::try_from(i).expect("0..256 fits in u8") >= first_byte);
            data.extend_from_slice(&count.to_be_bytes());
        }
        // Names table (20 bytes per sha).
        data.extend_from_slice(sha_bytes);
        // CRC32 table (one u32; value is unused by `lookup`).
        data.extend_from_slice(&0u32.to_be_bytes());
        // Offset table (one u32; MSB clear → 32-bit absolute offset).
        data.extend_from_slice(&pack_offset.to_be_bytes());
        // Pack trailer (20-byte sha placeholder) + idx trailer
        // (20-byte sha placeholder). The parser doesn't validate the
        // content of either against the body for `from_data`.
        data.extend_from_slice(&[0u8; 20]);
        data.extend_from_slice(&[0u8; 20]);
        data
    }

    /// Convenience: turn the `tip_hex` and `pack_sha_hex` strings into
    /// a serialised chain.json `Bytes` body the wrapper can return.
    fn chain_json_bytes(tip_hex: &str, pack_sha_hex: &str) -> Bytes {
        let json = make_chain_with(tip_hex, pack_sha_hex)
            .to_json_pretty()
            .expect("chain serialise");
        Bytes::from(json)
    }

    /// GC-race retry succeeds: the initial chain points at pack P1
    /// (absent from the store — simulating "already deleted by a
    /// concurrent sweep"); the reloaded chain points at pack P2 which
    /// is fully present (real one-object `.idx` + zlib-compressed
    /// blob entry). The retry must find the blob via P2 and return
    /// the payload.
    #[tokio::test]
    async fn read_with_pack_missing_retries_succeeds_after_chain_reload() {
        let inner = MockStore::new();
        let cache = PackIndexCache::default();

        let p1_sha = sha40(SHA_A);
        let p2_sha = sha40(SHA_B);
        let blob_payload = b"recovered blob";
        // The blob's git OID — `gix_hash::ObjectId` for "hello blob"
        // shape. We don't compute the real git sha here; we register
        // the same sha in both the .idx names table and in
        // `target_oid` so `gix_pack::index::File::lookup` returns the
        // single object.
        let blob_oid_sha = sha40(SHA_C);
        let blob_oid = sha40_to_object_id(&blob_oid_sha);

        // Build a P2 pack containing one Blob entry at offset 0.
        let mut pack = Vec::new();
        let mut offsets = Vec::new();
        push_pack_entry(
            &mut pack,
            &mut offsets,
            3, /* BLOB */
            None,
            blob_payload,
        );
        inner.insert(pack_key(None, &p2_sha), Bytes::from(pack.clone()));

        // Build the corresponding one-object v2 .idx for P2 and plant
        // it under the canonical idx key. `read_object_from_chain`
        // will load it via `load_index`.
        let idx_bytes = build_one_object_v2_idx(&blob_oid_sha, 0);
        inner.insert(pack_idx_key(None, &p2_sha), Bytes::from(idx_bytes));

        // The reader enters the helper carrying chain v1 (refs P1,
        // absent from the store). When it reloads `chain.json` after
        // the first PackMissing, the wrapper serves v2 (refs P2 with
        // a working idx + pack), simulating a compact+sweep that
        // happened between the reader's two loads.
        let chain_key = chain_key(None, "refs/heads/main");
        let v1 = chain_json_bytes(SHA_A, p1_sha.as_str());
        let v2 = chain_json_bytes(SHA_A, p2_sha.as_str());
        let store = EvolvingChainStore::new(inner, chain_key, vec![v2]);

        let initial = ChainManifest::from_json_bytes(&v1).expect("chain v1 parses");
        let remote_ref = RefName::new("refs/heads/main").expect("ref name valid");

        let resolved = read_with_pack_missing_retries(
            &store,
            None,
            &remote_ref,
            "refs/heads/main",
            initial,
            &blob_oid,
            &cache,
        )
        .await
        .expect("retry must succeed after chain reload");
        assert_eq!(resolved.payload, blob_payload);
        assert_eq!(resolved.kind, ObjectKind::Blob);
        // Exactly one chain reload was needed — the first read against
        // the initial in-memory chain, then one reload that swapped to
        // v2 referencing P2.
        assert_eq!(
            store.chain_calls(),
            1,
            "exactly one chain reload should have fired"
        );
        // Issue #136 contract: the retry path only reloads `chain.json`.
        // Path-index is loaded once by `read_blob` before this helper is
        // entered (the original `blob_oid` represents the snapshot the
        // caller asked about), and compaction preserves blob content
        // addressing — so reloading path-index on retry would silently
        // re-resolve the path against a newer tip's tree, which is the
        // wrong semantics for a stateless point-in-time read.
        // `read_with_pack_missing_retries` itself never touches
        // path-index; pin that with a counter assertion.
        assert_eq!(
            store.path_index_calls(),
            0,
            "retry path must not reload path-index.json",
        );
    }

    /// Companion to `succeeds_after_chain_reload`: pins the
    /// "path-index is NOT reloaded across retries" half of the
    /// `read_with_pack_missing_retries` contract documented at issue
    /// #136. A regression that re-resolved the blob OID via a fresh
    /// path-index load between retries would resolve to a different
    /// blob SHA on a force-pushed tree and surface as a stale-read
    /// bug. We arm a single pack-missing retry, count `get_bytes`
    /// calls against `path-index.json` through the `EvolvingChainStore`
    /// counter, and assert the count stays at zero.
    #[tokio::test]
    async fn read_with_pack_missing_retries_does_not_reload_path_index() {
        let inner = MockStore::new();
        let cache = PackIndexCache::default();

        let p1_sha = sha40(SHA_A);
        let p2_sha = sha40(SHA_B);
        let blob_payload = b"recovered blob";
        let blob_oid_sha = sha40(SHA_C);
        let blob_oid = sha40_to_object_id(&blob_oid_sha);

        // Build the P2 pack + idx so the retry's read finds the blob.
        let mut pack = Vec::new();
        let mut offsets = Vec::new();
        push_pack_entry(
            &mut pack,
            &mut offsets,
            3, /* BLOB */
            None,
            blob_payload,
        );
        inner.insert(pack_key(None, &p2_sha), Bytes::from(pack));
        let idx_bytes = build_one_object_v2_idx(&blob_oid_sha, 0);
        inner.insert(pack_idx_key(None, &p2_sha), Bytes::from(idx_bytes));

        // Pre-write a sentinel `path-index.json` body so any spurious
        // load would consume it (and bump the counter) rather than
        // silently 404 and slip past the assertion.
        inner.insert("refs/heads/main/path-index.json", Bytes::from_static(b"{}"));

        // Initial chain refs P1 (absent), reloaded chain refs P2 (present).
        let chain_key = chain_key(None, "refs/heads/main");
        let v1 = chain_json_bytes(SHA_A, p1_sha.as_str());
        let v2 = chain_json_bytes(SHA_A, p2_sha.as_str());
        let store = EvolvingChainStore::new(inner, chain_key, vec![v2]);

        let initial = ChainManifest::from_json_bytes(&v1).expect("chain v1 parses");
        let remote_ref = RefName::new("refs/heads/main").expect("ref name valid");

        let resolved = read_with_pack_missing_retries(
            &store,
            None,
            &remote_ref,
            "refs/heads/main",
            initial,
            &blob_oid,
            &cache,
        )
        .await
        .expect("retry must succeed");
        assert_eq!(resolved.payload, blob_payload);
        // The retry fired (chain reload count == 1).
        assert_eq!(store.chain_calls(), 1);
        // Critical contract: path-index.json is never re-loaded by the
        // retry path. `read_with_pack_missing_retries` operates on the
        // already-resolved `blob_oid`; reloading path-index would
        // re-resolve the path against a possibly-newer tree.
        assert_eq!(
            store.path_index_calls(),
            0,
            "retry path read path-index.json {} times; must be zero",
            store.path_index_calls(),
        );
    }

    /// `PackMissing` where the reload still references the same pack
    /// means genuine data loss — the bucket is missing a pack
    /// `chain.json` still names. The reader must fail fast with
    /// `PackMissing` rather than retry forever or upgrade to the
    /// concurrent-GC retry-exhausted variant.
    #[tokio::test]
    async fn read_with_pack_missing_retries_fails_fast_when_chain_still_references_missing_pack() {
        let inner = MockStore::new();
        let cache = PackIndexCache::default();

        let p1_sha = sha40(SHA_A);
        let blob_oid = sha40_to_object_id(&sha40(SHA_C));

        // Initial and reloaded chain both reference the same missing
        // pack — the "data loss" scenario, distinct from GC.
        let chain_key = chain_key(None, "refs/heads/main");
        let body = chain_json_bytes(SHA_A, p1_sha.as_str());
        let store = EvolvingChainStore::new(inner, chain_key, vec![body.clone()]);
        let initial = ChainManifest::from_json_bytes(&body).expect("chain parses");
        let remote_ref = RefName::new("refs/heads/main").expect("ref name valid");

        let err = read_with_pack_missing_retries(
            &store,
            None,
            &remote_ref,
            "refs/heads/main",
            initial,
            &blob_oid,
            &cache,
        )
        .await
        .expect_err("missing pack still in chain must fail fast");
        match err {
            PackchainError::PackMissing { key } => {
                assert!(
                    key.contains(&format!("packs/{SHA_A}")),
                    "PackMissing key should name the missing pack, got {key}",
                );
            }
            other => panic!("expected fail-fast PackMissing, got {other:?}"),
        }
        // Exactly one chain reload was issued (to verify the missing
        // key was still referenced) — no further reloads or sleeps.
        assert_eq!(store.chain_calls(), 1);
    }

    /// Retries are exhausted when each reload shows a *different*
    /// missing pack — i.e. compact+sweep keeps outpacing the reader.
    /// After `PACK_MISSING_MAX_RETRIES` retries the call surfaces
    /// `ConcurrentGcRetriesExhausted` with the last observed key.
    #[tokio::test(start_paused = true)]
    async fn read_with_pack_missing_retries_surfaces_exhausted_after_max_retries() {
        // `start_paused` lets the tokio runtime auto-advance time
        // through the backoff sleeps so the test doesn't pay the
        // wall-clock 2.6 s worst case.
        let inner = MockStore::new();
        let cache = PackIndexCache::default();

        let blob_oid = sha40_to_object_id(&sha40(SHA_C));

        // Five distinct chain versions, each referencing a different
        // missing pack. The initial in-memory chain is v1; the
        // wrapper serves v2..v5 on successive reloads. The reader's
        // walk is:
        //   - attempt 0 with v1 → PackMissing(P0); reload → v2 (refs P1)
        //   - attempt 1 with v2 → PackMissing(P1); reload → v3 (refs P2)
        //   - attempt 2 with v3 → PackMissing(P2); reload → v4 (refs P3)
        //   - attempt 3 with v4 → PackMissing(P3); reload → v5 (refs P4)
        //     → attempt >= MAX → exhausted with key for P3.
        let pack_shas = [
            "0000000000000000000000000000000000000000",
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
            "3333333333333333333333333333333333333333",
            "4444444444444444444444444444444444444444",
        ];
        let chain_key = chain_key(None, "refs/heads/main");
        let v1 = chain_json_bytes(SHA_A, pack_shas[0]);
        let reload_bodies: Vec<Bytes> = pack_shas[1..]
            .iter()
            .map(|sha| chain_json_bytes(SHA_A, sha))
            .collect();
        let initial = ChainManifest::from_json_bytes(&v1).expect("chain v1 parses");
        let store = EvolvingChainStore::new(inner, chain_key, reload_bodies);
        let remote_ref = RefName::new("refs/heads/main").expect("ref name valid");

        let err = read_with_pack_missing_retries(
            &store,
            None,
            &remote_ref,
            "refs/heads/main",
            initial,
            &blob_oid,
            &cache,
        )
        .await
        .expect_err("exhausted retries must error");
        match err {
            PackchainError::ConcurrentGcRetriesExhausted {
                last_missing_key,
                attempts,
            } => {
                // The last attempt was against v4 referencing P3
                // (the fourth pack in our table). `attempts` records
                // retries beyond the initial attempt: 3.
                assert_eq!(attempts, PACK_MISSING_MAX_RETRIES);
                assert!(
                    last_missing_key.contains(pack_shas[3]),
                    "last missing key should name pack[3], got {last_missing_key}"
                );
            }
            other => panic!("expected ConcurrentGcRetriesExhausted, got {other:?}"),
        }
        // Exactly MAX_RETRIES + 1 reloads: one verification per
        // attempted read. Each reload showed the missing pack absent
        // from the freshly-loaded chain so the loop kept retrying
        // (or, on the final reload, recorded "exhausted").
        assert_eq!(
            store.chain_calls(),
            usize::try_from(PACK_MISSING_MAX_RETRIES + 1).unwrap()
        );
    }

    /// Non-PackMissing errors are not retried — they pass through
    /// directly. Otherwise a transport error or a malformed pack would
    /// wait through the backoff schedule for no reason.
    #[tokio::test]
    async fn read_with_pack_missing_retries_does_not_retry_on_non_pack_missing_errors() {
        let inner = MockStore::new();
        let cache = PackIndexCache::default();

        // Plant a malformed `.idx` for the chain's pack. `load_index`
        // will parse-fail with `MalformedPackEntry`, *not*
        // `PackMissing`, so the retry path must not fire.
        let p1_sha = sha40(SHA_A);
        inner.insert(
            pack_idx_key(None, &p1_sha),
            Bytes::from_static(b"not a real idx"),
        );

        let chain_key = chain_key(None, "refs/heads/main");
        let body = chain_json_bytes(SHA_A, p1_sha.as_str());
        let store = EvolvingChainStore::new(inner, chain_key, vec![body.clone()]);
        let initial = ChainManifest::from_json_bytes(&body).expect("chain parses");
        let remote_ref = RefName::new("refs/heads/main").expect("ref name valid");
        let blob_oid = sha40_to_object_id(&sha40(SHA_C));

        let err = read_with_pack_missing_retries(
            &store,
            None,
            &remote_ref,
            "refs/heads/main",
            initial,
            &blob_oid,
            &cache,
        )
        .await
        .expect_err("malformed idx must surface immediately");
        assert!(
            matches!(err, PackchainError::MalformedPackEntry { .. }),
            "expected MalformedPackEntry passthrough, got {err:?}"
        );
        // No chain reloads — the error path did not enter the retry
        // branch at all.
        assert_eq!(store.chain_calls(), 0);
    }

    /// Regression for #136: a chain reload that itself errors (transport
    /// failure on `chain.json`) must surface as `PackchainError::Store`
    /// — NOT be converted into `ConcurrentGcRetriesExhausted` and NOT
    /// swallowed back into the original `PackMissing`. The retry loop
    /// uses `?` on `load_chain`, so a network fault during reload
    /// short-circuits to the wrapped store error.
    ///
    /// Setup: initial in-memory chain refs P1; the store has no pack
    /// for P1 (so the first read fails with `PackMissing`), and a
    /// one-shot `NetworkOnGetBytes` fault armed on the chain key
    /// fires when the retry loop calls `load_chain`.
    #[tokio::test]
    async fn read_with_pack_missing_retries_surfaces_chain_reload_error() {
        use crate::object_store::mock::Fault;

        let store = MockStore::new();
        let cache = PackIndexCache::default();

        let p1_sha = sha40(SHA_A);
        let blob_oid = sha40_to_object_id(&sha40(SHA_C));

        // Initial chain refs P1, which is absent from the store. The
        // first pack-read will surface PackMissing, sending the loop
        // into its reload branch.
        let chain_key_str = chain_key(None, "refs/heads/main");
        let body = chain_json_bytes(SHA_A, p1_sha.as_str());
        let initial = ChainManifest::from_json_bytes(&body).expect("chain v1 parses");
        let remote_ref = RefName::new("refs/heads/main").expect("ref name valid");

        // Arm a network fault on the chain key. `load_chain` calls
        // `store.get_bytes(chain_key)`; the fault fires there and the
        // wrapped error must propagate out of the retry helper.
        store.arm(Fault::NetworkOnGetBytes { key: chain_key_str });

        let err = read_with_pack_missing_retries(
            &store,
            None,
            &remote_ref,
            "refs/heads/main",
            initial,
            &blob_oid,
            &cache,
        )
        .await
        .expect_err("chain reload failure must surface as an error");

        assert!(
            matches!(err, PackchainError::Store(_)),
            "expected PackchainError::Store wrapping the chain-reload transport error; \
             a regression that swallowed the reload error would yield \
             ConcurrentGcRetriesExhausted or the original PackMissing instead. got {err:?}"
        );
        // Fault was consumed exactly once by the single reload attempt.
        assert_eq!(
            store.pending_faults(),
            0,
            "armed chain-reload fault must have fired exactly once"
        );
    }
}

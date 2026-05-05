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
use tracing::debug;

use crate::git::RefName;
use crate::object_store::{ObjectStore, ObjectStoreError};
use crate::remote::Remote;
use crate::url::StorageEngine;

use super::PackchainError;
use super::keys::{pack_idx_key, pack_key};
use super::manifest::{load_chain, load_path_index};
use super::schema::{ChainSegment, PathNode, Sha40};

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

/// Upper safety bound when expanding the fallback range for very
/// large blobs. Past this point we surface a typed error rather than
/// pulling unbounded bytes — a single multi-GiB blob in a code repo
/// is overwhelmingly likely to be a misuse (git-LFS material) rather
/// than a legitimate `read_blob` target.
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
    // `Remote::prefix()` borrows from `remote`, which outlives this
    // function — no need to own the bytes.
    let prefix_opt = optional_prefix(remote.prefix());

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

    let blob_sha = walk_path(&path_index.tree, &segments, ref_name, path)?;

    debug!(
        ref_name = %ref_name,
        path = %path,
        blob = %blob_sha.as_str(),
        segments = chain.segments.len(),
        "read_blob: resolved path to blob, scanning chain"
    );

    let blob_oid = sha40_to_object_id(&blob_sha);
    let mut depth = 0u32;
    let result = read_object_from_chain(
        remote.store(),
        prefix_opt,
        &chain.segments,
        &blob_oid,
        cache,
        &mut depth,
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

fn optional_prefix(prefix: &str) -> Option<&str> {
    if prefix.is_empty() {
        None
    } else {
        Some(prefix)
    }
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
    if *depth > MAX_DELTA_DEPTH {
        return Err(PackchainError::DeltaTooDeep {
            max: MAX_DELTA_DEPTH,
        });
    }
    *depth += 1;

    for segment in segments {
        let content_sha = pack_content_sha(segment)?;
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

fn pack_content_sha(segment: &ChainSegment) -> Result<Sha40, PackchainError> {
    // segment.pack is `[<prefix>/]packs/<sha>.pack`. Strip the
    // basename and the .pack suffix.
    let basename = segment
        .pack
        .rsplit('/')
        .next()
        .unwrap_or(segment.pack.as_str());
    let sha_str =
        basename
            .strip_suffix(".pack")
            .ok_or_else(|| PackchainError::MalformedPackEntry {
                offset: 0,
                reason: format!(
                    "chain segment pack key `{}` lacks `.pack` suffix",
                    segment.pack
                ),
            })?;
    Sha40::try_new(sha_str)
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
/// 2. Otherwise, fetch the whole pack and slice from `pack_offset`.
///    Only the last entry in any pack pays this cost; the rest hit
///    the cheap ranged-GET path. For typical packs (<10 MiB) the
///    full-fetch cost is negligible; for large packs the same code
///    path is exercised at most once per pack per process.
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
    if let Some(end) = next_offset {
        let range = pack_offset..end;
        return match store.get_bytes_range(&pack, range).await {
            Ok(b) => Ok(b),
            Err(ObjectStoreError::NotFound(_)) => Err(PackchainError::PackMissing { key: pack }),
            Err(e) => Err(PackchainError::Store(e)),
        };
    }
    // Last entry in the pack — fetch the whole pack and slice. The
    // cost amortises through the index cache: subsequent calls for
    // earlier entries take the cheap ranged-GET path.
    let full = match store.get_bytes(&pack).await {
        Ok(b) => b,
        Err(ObjectStoreError::NotFound(_)) => {
            return Err(PackchainError::PackMissing { key: pack });
        }
        Err(e) => return Err(PackchainError::Store(e)),
    };
    let pack_len = full.len() as u64;
    if pack_offset >= pack_len {
        return Err(PackchainError::MalformedPackEntry {
            offset: pack_offset,
            reason: "entry offset beyond pack EOF".to_owned(),
        });
    }
    let start = usize::try_from(pack_offset).map_err(|_| PackchainError::MalformedPackEntry {
        offset: pack_offset,
        reason: "pack offset exceeds usize".to_owned(),
    })?;
    Ok(full.slice(start..))
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
            // Copy-from-base instruction. The flag byte's low 4 bits
            // signal which offset bytes follow; the next 3 bits signal
            // which size bytes follow.
            let mut copy_offset = 0u32;
            for shift in 0..4 {
                if op & (1 << shift) != 0 {
                    copy_offset |=
                        u32::from(*delta.get(cursor).ok_or(PackchainError::MalformedDelta {
                            reason: "truncated delta copy offset",
                        })?) << (shift * 8);
                    cursor += 1;
                }
            }
            let mut copy_size = 0u32;
            for shift in 0..3 {
                if op & (1 << (4 + shift)) != 0 {
                    copy_size |=
                        u32::from(*delta.get(cursor).ok_or(PackchainError::MalformedDelta {
                            reason: "truncated delta copy size",
                        })?) << (shift * 8);
                    cursor += 1;
                }
            }
            if copy_size == 0 {
                copy_size = 0x10000; // Git's documented default size.
            }
            let start = copy_offset as usize;
            let end =
                start
                    .checked_add(copy_size as usize)
                    .ok_or(PackchainError::MalformedDelta {
                        reason: "copy span overflow",
                    })?;
            if end > base.payload.len() {
                return Err(PackchainError::MalformedDelta {
                    reason: "copy span exceeds base object",
                });
            }
            out.extend_from_slice(&base.payload[start..end]);
        } else if op == 0 {
            return Err(PackchainError::MalformedDelta {
                reason: "reserved zero opcode",
            });
        } else {
            // Insert opcode: low 7 bits are the literal length.
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
            out.extend_from_slice(&delta[cursor..end]);
            cursor = end;
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
    fn pack_content_sha_strips_prefix_and_extension() {
        let segment = ChainSegment {
            sha: sha40(SHA_A),
            parent_sha: None,
            pack: format!("acme/repo/packs/{SHA_C}.pack"),
            bytes: 4_096,
        };
        let sha = pack_content_sha(&segment).unwrap();
        assert_eq!(sha.as_str(), SHA_C);
    }

    #[test]
    fn pack_content_sha_handles_no_prefix() {
        let segment = ChainSegment {
            sha: sha40(SHA_A),
            parent_sha: None,
            pack: format!("packs/{SHA_C}.pack"),
            bytes: 4_096,
        };
        let sha = pack_content_sha(&segment).unwrap();
        assert_eq!(sha.as_str(), SHA_C);
    }

    #[test]
    fn pack_content_sha_rejects_missing_extension() {
        let segment = ChainSegment {
            sha: sha40(SHA_A),
            parent_sha: None,
            pack: format!("packs/{SHA_C}"),
            bytes: 4_096,
        };
        let err = pack_content_sha(&segment).unwrap_err();
        assert!(matches!(err, PackchainError::MalformedPackEntry { .. }));
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
}

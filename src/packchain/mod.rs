//! Incremental pack-chain storage engine (issue #52).
//!
//! Push (#63) writes incremental packs keyed by content SHA, a
//! newest-first [`schema::ChainManifest`], a nested
//! [`schema::PathIndex`] of repo paths to blob SHAs, and a baseline
//! bundle on the first / force push so a fresh clone short-circuits
//! through `bundle-uri`. Fetch (#64), direct file access (#65,
//! `read_blob` library API), compaction (#67), and GC (#66) are all
//! implemented in sibling modules. Push artefacts on the bucket:
//!
//! ```text
//! <prefix>/FORMAT                                "packchain"
//! <prefix>/HEAD                                  "refs/heads/main"
//! <prefix>/refs/heads/<branch>/LOCK#.lock        held during write, released after
//! <prefix>/refs/heads/<branch>/chain.json        newest-first manifest (THE commit point)
//! <prefix>/refs/heads/<branch>/path-index.json   nested tree → blob SHA map
//! <prefix>/refs/heads/<branch>/<tip>.bundle      baseline (first / force push only)
//! <prefix>/packs/<content-sha>.pack              incremental pack
//! <prefix>/packs/<content-sha>.idx               pack index
//! ```
//!
//! Once the push lands, fetch resolves shallow / full clones via
//! sequential pack install (`fetch.rs`), `read_blob` reads single
//! blobs via the path-index without rehydrating the chain
//! (`read.rs`), and the `manage compact` / `manage gc` subcommands
//! reap orphans and collapse the chain (`compact.rs`, `gc.rs`).
//!
//! ## Linearization point
//!
//! `chain.json` is the commit point: pack/idx/baseline upload
//! pre-lock, then under the per-ref lock the push writes
//! FORMAT → HEAD → chain.json → path-index.json. Anything that
//! crashed before the chain.json PUT leaves orphan keys
//! (pack/idx/baseline at content-SHA or tip-SHA names) which
//! `manage gc` reaps. Anything written after chain.json
//! (path-index.json overwrite, force-push baseline cleanup) is
//! post-commit and may be retried by re-running the push or compact.
//!
//! ## chain.json → path-index.json ordering and the reader contract
//!
//! Writing `path-index.json` LAST means a crash between the
//! `chain.json` PUT and the `path-index.json` PUT leaves the bucket
//! with a fresh chain alongside a stale path-index whose `tip` field
//! still names the prior chain.tip. The reader detects this with a
//! single tip-equality check (`path_index.tip == chain.tip`) and
//! surfaces it as
//! [`PackchainError::TransientChainPathIndexMismatch`] — a typed,
//! retry-shaped error — rather than silently returning the wrong
//! blob bytes or failing with the confusing
//! [`PackchainError::BlobNotInChain`] that the old (path-index-first)
//! ordering produced (issue #114).
//!
//! The reverse ordering (path-index before chain.json) is rejected
//! because it lets a stale chain coexist with a fresh path-index
//! whose blob SHAs are NOT yet in any chain pack, surfacing as
//! `BlobNotInChain` — indistinguishable from genuine corruption.
//!
//! ## Lost-race orphan packs
//!
//! Packs upload BEFORE the per-ref lock is acquired so the lock-hold
//! window stays bounded by chain.json + path-index PUT latency. When
//! two pushers race they both upload their packs pre-lock; the loser
//! sees `stale chain` after re-reading `chain.json` under the lock
//! and returns without committing, leaving its pack as an
//! unreferenced orphan that `manage gc` sweeps. The orphan-bandwidth
//! cost is the deliberate trade-off for keeping the lock window
//! short — an in-lock-upload alternative would block sibling pushers
//! for the full duration of a multi-GiB upload.

pub(crate) mod audit;
pub(crate) mod compact;
pub(crate) mod fetch;
pub mod gc;
pub(crate) mod git;
pub(crate) mod keys;
pub(crate) mod list;
pub(crate) mod manifest;
pub(crate) mod pack;
pub(crate) mod push;
// `pub` (parity with `gc`) so `git_remote_object_store::packchain::read`
// is reachable for rustdoc discovery. The convenience re-exports
// below (`packchain::PackIndexCache`, `packchain::read_blob`) remain
// the canonical short paths.
pub mod read;
pub(crate) mod retry;
pub(crate) mod schema;

pub use read::{PackIndexCache, read_blob};

/// Errors surfaced by the packchain engine. `pub` because the
/// [`crate::protocol::push::PushError::Packchain`] variant — which is
/// public — wraps it; making this `pub(crate)` would leak a private
/// type through a public API. The packchain engine itself stays
/// `pub(crate)` (see `pub(crate) mod push` etc.); only `gc` and `read`
/// are `pub` for rustdoc / direct-access reachability.
#[derive(Debug, thiserror::Error)]
pub enum PackchainError {
    /// On-bucket schema declares a version this build cannot read. The
    /// `expected` field is the version this build writes; `found` is
    /// the value parsed from the JSON. Lets a future v=2 reader refuse
    /// v=1 clients (and vice versa) cleanly.
    #[error("packchain schema version {found} unsupported (this build reads v{expected})")]
    UnsupportedSchemaVersion {
        /// Version found in the parsed JSON.
        found: u32,
        /// Version this build expects.
        expected: u32,
    },

    /// A field that should hold a 40-lowercase-hex SHA contained
    /// something else. Validation runs on every [`schema::Sha40`]
    /// deserialise so a malformed `chain.json` or `path-index.json`
    /// cannot leak past the parser into the rest of the engine.
    #[error("invalid 40-hex sha `{found}`: must be 40 lowercase hex characters")]
    InvalidSha {
        /// The rejected string (truncated by `Display`'s default
        /// formatter at the wire level).
        found: String,
    },

    /// Underlying `serde_json` parse error (malformed JSON, missing
    /// fields, type mismatches that aren't caught by [`schema::Sha40`]'s
    /// validator).
    #[error("packchain schema parse error: {0}")]
    ParseJson(#[from] serde_json::Error),

    /// Tree entry filename was not valid UTF-8. Git allows arbitrary
    /// bytes in tree entry names, but the on-bucket JSON layer cannot
    /// represent non-UTF-8 keys without a lossy encoding (banned by
    /// `.claude/rules/rust.md`). Carries the offending bytes verbatim
    /// for diagnostics.
    #[error("invalid path: {} (not valid UTF-8)", String::from_utf8_lossy(bytes))]
    InvalidPath {
        /// The offending bytes from the tree entry's filename.
        bytes: Vec<u8>,
    },

    /// Underlying gix / git error from tree-walking, ref lookups, or
    /// other git-side operations.
    #[error("packchain git error: {0}")]
    Git(#[from] crate::git::GitError),

    /// Local repository is shallow (a `.git/shallow` file exists) and
    /// the rev-walk from the local tip crosses a shallow boundary, so
    /// a complete pack cannot be produced. Pushing from a shallow
    /// clone would leave the server with permanently incomplete
    /// history; better to refuse loudly than to corrupt the remote.
    #[error("cannot push from a shallow clone: rev-walk crosses a shallow boundary")]
    ShallowPushRejected,

    /// `chain.json` is missing for the requested ref. Either the
    /// branch was never pushed under the packchain engine or it was
    /// deleted server-side. Distinct from
    /// [`Self::Store`]`(NotFound)` so the wire-line is explicit
    /// about which artefact is missing.
    #[error("chain.json absent for {ref_name}; the branch is unknown to the bucket")]
    ChainAbsent {
        /// The ref name the fetch asked about.
        ref_name: String,
    },

    /// `chain.json` references a pack that is not present on the
    /// bucket. Pinning this as a typed error so `doctor` can flag it
    /// specifically rather than the operator having to disambiguate a
    /// generic `NotFound` from a transient failure. Issue #64 calls
    /// this out as a regression case to surface loudly rather than
    /// silently zero-byte-fetch.
    #[error("packchain: chain.json references missing pack at {key}")]
    PackMissing {
        /// Bucket-relative pack key recorded in `chain.json`.
        key: String,
    },

    /// Baseline bundle (the `<full_at>.bundle` artefact) is missing.
    /// Surfaces during a clone where the chain walk reached the root
    /// segment but the baseline that should be alongside it is gone.
    #[error("packchain: baseline bundle missing at {key}")]
    BaselineMissing {
        /// Bucket key of the missing `<full_at>.bundle`.
        key: String,
    },

    /// Pack content SHA could not be derived (file shorter than the
    /// 32-byte minimum PACK header + trailer, or an I/O error reading
    /// the trailer).
    #[error("pack content SHA unavailable: {0}")]
    PackTrailer(String),

    /// `gix_pack::data::output::count::objects` or `FromEntriesIter`
    /// failed during pack emission.
    #[error("pack build error: {0}")]
    PackBuild(String),

    /// `gix_pack::Bundle::write_to_directory` failed during the
    /// post-pack `.idx` derivation pass.
    #[error("pack index write error: {0}")]
    PackIndexWrite(Box<gix_pack::bundle::write::Error>),

    /// Underlying object-store transport / auth error.
    #[error("packchain object-store error: {0}")]
    Store(#[from] crate::object_store::ObjectStoreError),

    /// Local I/O failure (tempdir, file read, file persist).
    #[error("packchain I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// [`read::read_blob`] was called against a remote whose resolved
    /// engine is not [`crate::url::StorageEngine::Packchain`]. Surfaces
    /// before any artefact lookup so callers see a typed mismatch
    /// instead of a misleading `chain.json` not-found.
    #[error(
        "read_blob requires the packchain engine; this remote uses `{found}` — \
         check the URL's `?engine=` parameter or the bucket's `FORMAT` key"
    )]
    WrongEngine {
        /// Engine the remote actually resolved to.
        found: crate::url::StorageEngine,
    },

    /// `path-index.json` is missing for the requested ref. Distinct
    /// from [`Self::ChainAbsent`] so an operator sees which artefact is
    /// gone — chain.json being present without path-index indicates a
    /// crashed-mid-push state `manage gc` will reconcile.
    #[error("path-index.json absent for {ref_name}; the branch's path map is unavailable")]
    PathIndexAbsent {
        /// The ref name [`read::read_blob`] was asked about.
        ref_name: String,
    },

    /// Caller passed a `path` that does not exist in this commit's tree.
    #[error("path `{path}` not found in {ref_name}")]
    PathNotFound {
        /// Ref the lookup ran against.
        ref_name: String,
        /// The path the caller asked for, returned verbatim.
        path: String,
    },

    /// Caller passed a malformed path: empty, absolute (`/`-prefixed),
    /// containing a `..` segment, or containing empty segments
    /// (consecutive slashes). These shapes don't map to git tree
    /// semantics; reject before walking.
    #[error("malformed path `{path}`: {reason}")]
    MalformedPath {
        /// The rejected path, returned verbatim.
        path: String,
        /// Human-readable reason (`"empty"`, `"absolute"`, `"contains ..\""`, etc.).
        reason: &'static str,
    },

    /// Path resolved to a tree node, not a blob — the caller asked for
    /// a directory, not a file. Distinct from [`Self::PathNotFound`] so
    /// the caller can distinguish "wrong shape" from "missing".
    #[error("path `{path}` resolves to a directory, not a file")]
    PathNotABlob {
        /// The path the caller asked for.
        path: String,
    },

    /// Blob SHA recorded in `path-index.json` was not present in any
    /// pack referenced by `chain.json`. Indicates a corrupted bucket
    /// (path-index points at a blob the chain doesn't carry); typed
    /// distinctly so `doctor` can flag it specifically.
    #[error("blob {sha} for path `{path}` not present in any chain pack")]
    BlobNotInChain {
        /// The blob SHA the path-index named.
        sha: String,
        /// The path the caller asked for.
        path: String,
    },

    /// Pack entry header could not be decoded (truncated bytes, unknown
    /// type id, non-canonical size encoding, etc.).
    #[error("malformed pack entry at offset {offset}: {reason}")]
    MalformedPackEntry {
        /// Pack-relative offset of the entry that failed to decode.
        offset: u64,
        /// Human-readable reason from the underlying decoder.
        reason: String,
    },

    /// Zlib stream embedded in a pack entry could not be inflated.
    #[error("zlib decompression failure for entry at offset {offset}")]
    Decompress {
        /// Pack-relative offset of the entry whose payload failed.
        offset: u64,
    },

    /// Delta resolution exceeded [`read::MAX_DELTA_DEPTH`]. Mirrors
    /// git's own depth cap — most legitimate chains stay well under it,
    /// so a deep chain is almost certainly a corrupted pack with a
    /// delta cycle.
    #[error("pack delta chain exceeds maximum depth ({max})")]
    DeltaTooDeep {
        /// The depth limit (always [`read::MAX_DELTA_DEPTH`]).
        max: u32,
    },

    /// Delta payload could not be applied (truncated instructions,
    /// out-of-range copy span, source size mismatch).
    #[error("malformed delta payload: {reason}")]
    MalformedDelta {
        /// Human-readable reason from the delta decoder.
        reason: &'static str,
    },

    /// `read_blob` was given a ref name that fails `gix-validate`'s
    /// reference-name rules (empty, control characters, `..`, etc.).
    #[error("invalid ref name `{name}`")]
    InvalidRefName {
        /// The ref name the caller passed.
        name: String,
    },

    /// Tree closure walk encountered a cycle: a tree references itself
    /// directly or transitively via an ancestor on the current descent.
    /// Content-addressing makes cycles impossible in a healthy ODB, so
    /// this surfaces a corrupted or adversarial repository rather than
    /// looping unbounded and exhausting the call stack.
    #[error("tree {oid} forms a cycle in the path-index walk")]
    TreeCycle {
        /// The tree OID whose presence in the ancestor set was detected.
        oid: String,
    },

    /// Reader observed `chain.json` and `path-index.json` with
    /// mismatched tips — a transient state during the brief window
    /// where a push or compact has committed the new `chain.json` but
    /// not yet overwritten `path-index.json` (issue #114). The reader
    /// refuses to resolve a path against an out-of-sync path-index
    /// because the resolved blob SHA may name a different file than
    /// the caller intended; instead it surfaces this typed error so
    /// the caller can retry. Subsequent reads converge once the writer
    /// finishes the path-index PUT.
    #[error(
        "transient chain/path-index mismatch for {ref_name}: \
         chain.tip = {chain_tip}, path_index.tip = {path_index_tip}; retry"
    )]
    TransientChainPathIndexMismatch {
        /// The ref the lookup ran against.
        ref_name: String,
        /// Tip recorded in `chain.json` at read time.
        chain_tip: String,
        /// Tip recorded in `path-index.json` at read time.
        path_index_tip: String,
    },

    /// [`read::read_blob`] retried [`Self::PackMissing`] failures the
    /// configured number of times and gave up. Each retry reloaded
    /// `chain.json` and observed that the failing pack key was no
    /// longer referenced — consistent with a concurrent
    /// `manage gc sweep` deleting compacted-away packs — but a fresh
    /// `PackMissing` showed up on the new chain anyway, suggesting a
    /// vigorous compact+sweep cycle that kept outpacing the reader.
    /// Distinct from [`Self::PackMissing`] so callers can treat it as
    /// "retry the whole `read_blob` call later" rather than as a
    /// permanent bucket inconsistency (issue #136).
    #[error(
        "read_blob exhausted {attempts} retries against concurrent GC: \
         last missing pack `{last_missing_key}`; retry the call"
    )]
    ConcurrentGcRetriesExhausted {
        /// The key whose final `PackMissing` ended the retry loop.
        last_missing_key: String,
        /// Number of retry attempts that were made (excluding the
        /// initial attempt). Pinned in the error so logs and tests can
        /// assert on it.
        attempts: u32,
    },
}

impl From<gix_pack::bundle::write::Error> for PackchainError {
    fn from(value: gix_pack::bundle::write::Error) -> Self {
        Self::PackIndexWrite(Box::new(value))
    }
}

impl From<tokio::task::JoinError> for PackchainError {
    fn from(value: tokio::task::JoinError) -> Self {
        // `JoinError` carries either a panic payload or a cancellation
        // signal; flatten both through the Io variant since neither
        // matches a more specific PackchainError category.
        Self::Io(std::io::Error::other(value.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_schema_version_renders_both_versions() {
        let err = PackchainError::UnsupportedSchemaVersion {
            found: 2,
            expected: 1,
        };
        assert_eq!(
            err.to_string(),
            "packchain schema version 2 unsupported (this build reads v1)"
        );
    }

    #[test]
    fn shallow_push_rejected_includes_actionable_wording() {
        let err = PackchainError::ShallowPushRejected;
        // The wire-line client-facing wording must remain stable for
        // shellspec assertions; pin it here too.
        let msg = err.to_string();
        assert!(
            msg.contains("shallow clone"),
            "shallow rejection wording must mention shallow clone: {msg}",
        );
    }
}

//! Incremental pack-chain storage engine (issue #52).
//!
//! Phase 1 (commit `783a339`) shipped the foundation: schema types,
//! validated [`Sha40`] newtype, key builders, and `extract_path_index`.
//! Phase 2 (issue #63) lights up push: incremental packs keyed by
//! content SHA, a newest-first [`schema::ChainManifest`], a nested
//! [`schema::PathIndex`] of repo paths to blob SHAs, and a baseline
//! bundle on the first / force push so a fresh clone in Phase 3 can
//! short-circuit. Push artefacts on the bucket:
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
//! Fetch (Phase 3), direct file access (Phase 4), and GC / compaction
//! (Phase 5) are out of scope; a packchain bucket written by Phase 2
//! is **write-only** until Phase 3.
//!
//! ## Linearization point
//!
//! `chain.json` is the commit point: pack/idx/baseline upload
//! pre-lock, then under the per-ref lock the push writes
//! path-index → FORMAT → HEAD → chain.json. Anything that crashed
//! before the chain.json PUT leaves orphan keys (pack/idx/baseline at
//! content-SHA or tip-SHA names) which Phase 5 GC reaps. Anything
//! written after chain.json (force-push baseline cleanup) is
//! best-effort and never fails the push.
//!
//! ## Lost-race orphan packs
//!
//! Packs upload BEFORE the per-ref lock is acquired so the lock-hold
//! window stays bounded by chain.json + path-index PUT latency. When
//! two pushers race they both upload their packs pre-lock; the loser
//! sees `stale chain` after re-reading `chain.json` under the lock
//! and returns without committing, leaving its pack as an
//! unreferenced orphan that Phase 5 GC sweeps. The orphan-bandwidth
//! cost is the deliberate trade-off for keeping the lock window
//! short — an in-lock-upload alternative would block sibling pushers
//! for the full duration of a multi-GiB upload.

pub(crate) mod git;
pub(crate) mod keys;
pub(crate) mod manifest;
pub(crate) mod pack;
pub(crate) mod push;
pub(crate) mod schema;

/// Errors surfaced by the packchain engine. `pub` because the
/// [`crate::protocol::push::PushError::Packchain`] variant — which is
/// public — wraps it; making this `pub(crate)` would leak a private
/// type through a public API. The packchain engine itself stays
/// `pub(crate)` (see `pub(crate) mod push` etc.) until Phase 3.
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
}

impl From<gix_pack::bundle::write::Error> for PackchainError {
    fn from(value: gix_pack::bundle::write::Error) -> Self {
        Self::PackIndexWrite(Box::new(value))
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

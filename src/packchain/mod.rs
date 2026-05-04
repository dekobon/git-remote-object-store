//! Incremental pack-chain storage engine (issue #52).
//!
//! Phase 1 ships the foundation only: the [`StorageEngine::Packchain`]
//! variant, on-bucket schema types ([`ChainManifest`], [`PathIndex`]),
//! validated [`Sha40`] newtype, and key builders. Push, fetch, direct
//! file access, compaction, and GC follow in sub-issues filed under
//! #52. Phase 1 dispatch lives in [`crate::protocol::run`] — when the
//! resolved engine is `Packchain` it surfaces
//! [`crate::protocol::ProtocolError::EngineNotImplemented`] before any
//! command runs, so users see a single clear error rather than silent
//! fallback to the bundle engine.
//!
//! [`StorageEngine::Packchain`]: crate::url::StorageEngine::Packchain
//!
//! ## Dead code in Phase 1
//!
//! The schema types and key builders below are only consumed by their
//! own unit tests and by [`engine_unimplemented`]'s call sites. Phase 2
//! (push) and Phase 3 (fetch) will wire them into real engine logic.
//! `#![allow(dead_code)]` suppresses the per-symbol warnings until then
//! — landing them now keeps the Phase 2 PR focused on logic, not on
//! foundation churn that can break wire formats. Remove the allow when
//! Phase 2 lands.

#![allow(dead_code)]

pub(crate) mod git;
pub(crate) mod keys;
pub(crate) mod schema;

/// Errors surfaced by the packchain engine. `pub(crate)` while Phase 2+
/// is in flight; flip to `pub` once the engine has a stable public API.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PackchainError {
    /// The dispatch site reached a packchain code path that has not yet
    /// landed. The string names the operation (`"push"`, `"fetch"`,
    /// ...) so the wire-line is actionable. Replaced by real logic in
    /// the Phase 2-5 sub-issues.
    #[error("packchain engine: `{0}` is not yet implemented (issue #52)")]
    NotImplemented(&'static str),

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
    /// something else. Validation runs on every [`Sha40`] deserialise
    /// so a malformed `chain.json` or `path-index.json` cannot leak
    /// past the parser into the rest of the engine.
    #[error("invalid 40-hex sha `{found}`: must be 40 lowercase hex characters")]
    InvalidSha {
        /// The rejected string (truncated by `Display`'s default
        /// formatter at the wire level).
        found: String,
    },

    /// Underlying `serde_json` parse error (malformed JSON, missing
    /// fields, type mismatches that aren't caught by [`Sha40`]'s
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_implemented_display_includes_operation() {
        let err = PackchainError::NotImplemented("push");
        assert_eq!(
            err.to_string(),
            "packchain engine: `push` is not yet implemented (issue #52)"
        );
    }

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
}

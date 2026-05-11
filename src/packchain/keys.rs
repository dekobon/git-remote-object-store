//! Bucket-key builders **and inspectors** for the packchain engine.
//!
//! Centralised so Phase 2/3 push/fetch and Phase 4 direct-file-access
//! all derive identical keys for a given (prefix, ref, sha) tuple. The
//! key shapes mirror the issue-#52 spec:
//!
//! ```text
//! <prefix>/refs/heads/<branch>/chain.json
//! <prefix>/refs/heads/<branch>/path-index.json
//! <prefix>/packs/<content-sha>.pack
//! <prefix>/packs/<content-sha>.idx
//! ```
//!
//! All builders apply the same empty-prefix rule as
//! [`crate::keys::join`] / [`crate::keys::bundle_key`]: an empty (or
//! `None`) prefix yields a key with no leading slash.
//!
//! Inspectors ([`is_chain_json_key`], [`sha_from_pack_key`]) live
//! here too so callers across the engine (`gc`, `list`, `read`)
//! don't grow drift between independent copies.

use std::fmt;

use super::PackchainError;
use super::schema::{ChainSegment, Sha40};

/// Suffix bytes that mark a [`chain_key`] in a listing. Defined
/// once so `gc::list_referenced_packs` and `list::list_refs` can't
/// drift apart.
pub(crate) const CHAIN_JSON_SUFFIX: &[u8] = b"/chain.json";

/// Returns `true` when `key` ends with [`CHAIN_JSON_SUFFIX`] —
/// i.e. it is a chain manifest key, not a sibling
/// `path-index.json` / `<sha>.bundle` under the same ref directory.
#[must_use]
pub(crate) fn is_chain_json_key(key: &str) -> bool {
    key.as_bytes().ends_with(CHAIN_JSON_SUFFIX)
}

/// Compose the full bucket key for a chain segment's pack from the
/// prefix and the bucket-relative `pack` field stored in `chain.json`.
/// `chain.json` records pack keys as `packs/<sha>.pack` (no leading
/// prefix) so a chain authored with one prefix can be read with
/// another after a `mv`-style rename.
#[must_use]
pub(crate) fn pack_key_from_relative(prefix: Option<&str>, bucket_relative_pack: &str) -> String {
    crate::keys::join(prefix, bucket_relative_pack)
}

/// Strip `<prefix>/` and `/chain.json` to derive the ref path.
///
/// Returns `None` for keys that don't fit the shape — callers
/// upstream filter on [`is_chain_json_key`], so a `None` here
/// signals a deeper inconsistency (an unprefixed key listed under a
/// prefixed bucket, or a sibling-prefix collision like `repo-other/`
/// against `repo`). Centralised so `list::list_refs` and
/// `audit::load_chains` can't drift apart.
#[must_use]
pub(crate) fn ref_path_from_chain_key(prefix: Option<&str>, key: &str) -> Option<String> {
    let without_suffix = key.strip_suffix("/chain.json")?;
    match prefix {
        None | Some("") => Some(without_suffix.to_owned()),
        Some(p) => without_suffix
            .strip_prefix(p)
            .and_then(|s| s.strip_prefix('/'))
            .map(str::to_owned),
    }
}

/// Extract the content SHA from a chain segment's `pack` field.
///
/// `pack` is `[<prefix>/]packs/<sha>.pack` per the chain.json
/// schema. Returns `None` for keys that don't fit the shape; the
/// caller wraps the `None` into its preferred error variant
/// (`MalformedPackEntry` for `read::decode_entry`'s call site,
/// `ParseJson` via `serde_json::Error::custom` for
/// `gc::list_referenced_packs`).
#[must_use]
pub(crate) fn sha_from_pack_key(pack: &str) -> Option<Sha40> {
    let basename = pack.rsplit('/').next().unwrap_or(pack);
    let sha = basename.strip_suffix(".pack")?;
    Sha40::try_new(sha).ok()
}

/// Validate `segment.pack` and return its content SHA, or surface a
/// [`PackchainError::MalformedPackEntry`] when the key is malformed.
/// One helper used by every code path that needs to derive a bucket
/// key (or just validate the format) from a chain segment — keeps the
/// error wording aligned across `fetch`, `compact`, `read`, and `gc`.
pub(crate) fn segment_pack_sha(segment: &ChainSegment) -> Result<Sha40, PackchainError> {
    sha_from_pack_key(&segment.pack).ok_or_else(|| PackchainError::MalformedPackEntry {
        offset: 0,
        reason: format!(
            "chain segment pack key `{}` is not of the form `[<prefix>/]packs/<sha>.pack`",
            segment.pack,
        ),
    })
}

/// `<prefix>/<ref_name>/chain.json` — newest-first chain manifest for
/// `ref_name`.
pub(crate) fn chain_key(prefix: Option<&str>, ref_name: impl fmt::Display) -> String {
    match prefix {
        Some(p) if !p.is_empty() => format!("{p}/{ref_name}/chain.json"),
        _ => format!("{ref_name}/chain.json"),
    }
}

/// `<prefix>/<ref_name>/path-index.json` — nested path→blob map at
/// `ref_name`'s tip commit.
pub(crate) fn path_index_key(prefix: Option<&str>, ref_name: impl fmt::Display) -> String {
    match prefix {
        Some(p) if !p.is_empty() => format!("{p}/{ref_name}/path-index.json"),
        _ => format!("{ref_name}/path-index.json"),
    }
}

/// `<prefix>/packs/<content_sha>.pack` — pack file keyed by its
/// content SHA (the trailing SHA1 appended by git's PACK format).
pub(crate) fn pack_key(prefix: Option<&str>, content_sha: &Sha40) -> String {
    let sha = content_sha.as_str();
    match prefix {
        Some(p) if !p.is_empty() => format!("{p}/packs/{sha}.pack"),
        _ => format!("packs/{sha}.pack"),
    }
}

/// `<prefix>/packs/<content_sha>.idx` — pack index file matching
/// `pack_key(prefix, content_sha)`.
pub(crate) fn pack_idx_key(prefix: Option<&str>, content_sha: &Sha40) -> String {
    let sha = content_sha.as_str();
    match prefix {
        Some(p) if !p.is_empty() => format!("{p}/packs/{sha}.idx"),
        _ => format!("packs/{sha}.idx"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "abcdef0123456789abcdef0123456789abcdef01";
    const REF: &str = "refs/heads/main";

    fn sha40() -> Sha40 {
        Sha40::try_new(SHA).unwrap()
    }

    #[test]
    fn chain_key_with_prefix() {
        assert_eq!(
            chain_key(Some("acme"), REF),
            format!("acme/{REF}/chain.json"),
        );
    }

    #[test]
    fn chain_key_without_prefix() {
        assert_eq!(chain_key(None, REF), format!("{REF}/chain.json"));
    }

    #[test]
    fn chain_key_empty_prefix_matches_none() {
        assert_eq!(chain_key(Some(""), REF), chain_key(None, REF));
    }

    #[test]
    fn path_index_key_with_prefix() {
        assert_eq!(
            path_index_key(Some("acme"), REF),
            format!("acme/{REF}/path-index.json"),
        );
    }

    #[test]
    fn path_index_key_without_prefix() {
        assert_eq!(path_index_key(None, REF), format!("{REF}/path-index.json"));
    }

    #[test]
    fn pack_key_with_prefix() {
        let sha = sha40();
        assert_eq!(
            pack_key(Some("acme"), &sha),
            format!("acme/packs/{SHA}.pack")
        );
    }

    #[test]
    fn pack_key_without_prefix() {
        let sha = sha40();
        assert_eq!(pack_key(None, &sha), format!("packs/{SHA}.pack"));
    }

    #[test]
    fn pack_idx_key_with_prefix() {
        let sha = sha40();
        assert_eq!(
            pack_idx_key(Some("acme"), &sha),
            format!("acme/packs/{SHA}.idx"),
        );
    }

    #[test]
    fn pack_idx_key_without_prefix() {
        let sha = sha40();
        assert_eq!(pack_idx_key(None, &sha), format!("packs/{SHA}.idx"));
    }

    #[test]
    fn pack_and_idx_share_basename() {
        // The two keys must differ only in the `.pack` / `.idx`
        // extension. A regression that decoupled them (e.g. a stray
        // separator in one builder) would orphan the index from its
        // pack on every push.
        let sha = sha40();
        let pack = pack_key(Some("acme"), &sha);
        let idx = pack_idx_key(Some("acme"), &sha);
        assert_eq!(
            pack.strip_suffix(".pack").unwrap(),
            idx.strip_suffix(".idx").unwrap()
        );
    }

    // --- inspectors ----------------------------------------------------

    #[test]
    fn is_chain_json_key_accepts_prefixed_and_unprefixed_keys() {
        assert!(is_chain_json_key("repo/refs/heads/main/chain.json"));
        assert!(is_chain_json_key("refs/heads/main/chain.json"));
        assert!(is_chain_json_key("refs/heads/feature/x/chain.json"));
    }

    #[test]
    fn is_chain_json_key_rejects_siblings() {
        assert!(!is_chain_json_key("repo/refs/heads/main/path-index.json"));
        assert!(!is_chain_json_key(&format!(
            "repo/refs/heads/main/{SHA}.bundle"
        )));
        // A key whose basename starts with `chain.json` but has more
        // bytes after — e.g. `chain.json.bak` — must be rejected.
        assert!(!is_chain_json_key("repo/refs/heads/main/chain.json.bak"));
    }

    #[test]
    fn sha_from_pack_key_handles_prefixed_and_unprefixed() {
        let sha = sha_from_pack_key(&format!("packs/{SHA}.pack")).expect("unprefixed");
        assert_eq!(sha.as_str(), SHA);
        let sha = sha_from_pack_key(&format!("acme/repo/packs/{SHA}.pack")).expect("prefixed");
        assert_eq!(sha.as_str(), SHA);
    }

    #[test]
    fn sha_from_pack_key_returns_none_for_malformed() {
        // Missing `.pack` suffix.
        assert!(sha_from_pack_key(&format!("packs/{SHA}")).is_none());
        // Wrong-length sha (39 hex chars).
        assert!(sha_from_pack_key("packs/abcdef0123456789abcdef0123456789abcdef0.pack").is_none());
        // Non-hex character in sha.
        assert!(sha_from_pack_key("packs/zbcdef0123456789abcdef0123456789abcdef01.pack").is_none());
    }

    #[test]
    fn segment_pack_sha_maps_malformed_to_malformed_pack_entry() {
        let segment = super::super::schema::ChainSegment {
            sha: Sha40::try_new(SHA).unwrap(),
            parent_sha: None,
            pack: format!("packs/{SHA}"),
            bytes: 4_096,
        };
        let err = segment_pack_sha(&segment).unwrap_err();
        assert!(
            matches!(err, PackchainError::MalformedPackEntry { offset: 0, .. }),
            "expected MalformedPackEntry, got {err:?}",
        );
    }
}

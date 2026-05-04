//! Bucket-key builders for the packchain engine.
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

use std::fmt;

use super::schema::Sha40;

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
}

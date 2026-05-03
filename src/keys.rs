//! Helpers for constructing object-store keys.
//!
//! All key-building functions live here so that the "empty prefix means
//! no leading slash" rule and the bundle-key format have exactly one
//! implementation. See Lessons Learned §3.
//!
//! Both the helper protocol (push/fetch/list) and the management CLI
//! (doctor/branch/snapshot) build keys of the form `<prefix>/<suffix>`
//! and have to special-case the empty-prefix (root-of-bucket) case so
//! the resulting key has no leading slash. Centralising the rule here
//! keeps the four module call sites in lockstep and prevents the
//! recurring "leading slash on root-prefix repos" bug (#29, #32).

use std::fmt;

/// Join `prefix` and `suffix` with a single `/`, omitting both the
/// separator and the prefix entirely when `prefix` is empty.
///
/// `suffix` is taken verbatim — pass `""` to obtain a `<prefix>/`
/// listing prefix (or `""` for root), `"HEAD"` for the head object,
/// `"refs/heads/<branch>/"` for a branch listing, and so on.
///
/// Callers who carry the prefix as `Option<&str>` should pass
/// `prefix.unwrap_or("")`. `Some("")` and `None` collapse to the same
/// "no prefix" key shape.
pub(crate) fn join(prefix: &str, suffix: &str) -> String {
    if prefix.is_empty() {
        suffix.to_owned()
    } else if suffix.is_empty() {
        format!("{prefix}/")
    } else {
        format!("{prefix}/{suffix}")
    }
}

/// Build the bundle key `<prefix>/<ref_name>/<sha>.bundle`, applying the
/// same empty-prefix rule as [`join`].
///
/// **Precondition:** `ref_name` must be a valid git ref name and `sha`
/// must be 40 lowercase hex characters. The signature accepts
/// `impl fmt::Display` rather than `&RefName` / `&Sha` so the management
/// CLI (which holds these as raw `String` from listing output) can call
/// the helper without re-parsing; production helper-protocol callers
/// always pass the validated newtypes (`RefName`, `Sha`). Pre-validate
/// untyped strings before reaching here — `bundle_key` performs no
/// charset or length check, and a malformed input produces a key with
/// the same shape that subsequent storage operations will accept and
/// then fail to round-trip.
///
/// Renders into a single allocation regardless of whether `prefix` is
/// present (prior implementations chained two `format!` calls).
pub(crate) fn bundle_key(
    prefix: Option<&str>,
    ref_name: impl fmt::Display,
    sha: impl fmt::Display,
) -> String {
    match prefix {
        Some(p) if !p.is_empty() => format!("{p}/{ref_name}/{sha}.bundle"),
        _ => format!("{ref_name}/{sha}.bundle"),
    }
}

#[cfg(test)]
mod tests {
    use super::{bundle_key, join};

    #[test]
    fn joins_prefix_and_suffix_with_slash() {
        assert_eq!(join("acme", "HEAD"), "acme/HEAD");
        assert_eq!(
            join("acme/repo", "refs/heads/main/"),
            "acme/repo/refs/heads/main/"
        );
    }

    #[test]
    fn empty_prefix_yields_suffix_verbatim() {
        assert_eq!(join("", "HEAD"), "HEAD");
        assert_eq!(join("", "refs/heads/main/"), "refs/heads/main/");
    }

    #[test]
    fn empty_suffix_yields_listing_prefix_with_trailing_slash() {
        assert_eq!(join("acme", ""), "acme/");
    }

    #[test]
    fn empty_prefix_and_suffix_yields_empty_string() {
        // Listing the bucket root with no prefix at all.
        assert_eq!(join("", ""), "");
    }

    #[test]
    fn bundle_key_with_prefix() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            bundle_key(Some("acme"), "refs/heads/main", sha),
            format!("acme/refs/heads/main/{sha}.bundle"),
        );
    }

    #[test]
    fn bundle_key_without_prefix() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            bundle_key(None, "refs/heads/main", sha),
            format!("refs/heads/main/{sha}.bundle"),
        );
    }

    #[test]
    fn bundle_key_empty_prefix_matches_none() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            bundle_key(Some(""), "refs/heads/main", sha),
            bundle_key(None, "refs/heads/main", sha),
        );
    }
}

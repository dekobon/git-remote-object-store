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

use crate::object_store::ObjectMeta;

/// Final-segment name written for protected refs. Shape on bucket:
/// `<prefix>/<ref-path>/PROTECTED#`. The literal `#` keeps it cleanly
/// outside the bundle/lock/zip namespaces and `gix-validate` rejects
/// `#` in ref names, so the marker cannot be confused with a real ref.
///
/// Use [`is_protected_marker_segment`] to test a candidate last
/// segment for the marker — substring matching against the full key
/// is unsafe (the literal could appear elsewhere in a future schema).
pub(crate) const PROTECTED_MARKER_SEGMENT: &str = "PROTECTED#";

/// Returns `true` iff `last_segment` is the protected-marker name.
///
/// Callers that hold the full key should split with `rsplit_once('/')`
/// and pass the trailing segment. Substring matching against the full
/// key is unsafe — see the type-level note on
/// [`PROTECTED_MARKER_SEGMENT`].
pub(crate) fn is_protected_marker_segment(last_segment: &str) -> bool {
    last_segment == PROTECTED_MARKER_SEGMENT
}

/// Returns `true` iff any entry's final path segment is the
/// `PROTECTED#` marker. Centralised here so the bundle engine, the
/// packchain engine, and the management `delete-branch` flow share one
/// canonical predicate against the same byte-exact segment match —
/// substring matching against the full key is unsafe (the literal could
/// appear elsewhere in a future schema).
///
/// [`is_protected_marker_segment`] matches only the exact `PROTECTED#`
/// segment — never the `LOCK#.lock` lock key or a `PROTECTED#`-prefixed
/// sibling — so an unfiltered listing is safe to scan: a held lock key
/// in `entries` cannot be conflated with the marker.
pub(crate) fn entries_have_protected_marker(entries: &[ObjectMeta]) -> bool {
    entries.iter().any(|entry| {
        entry
            .key
            .rsplit_once('/')
            .is_some_and(|(_, last)| is_protected_marker_segment(last))
    })
}

/// Join `prefix` and `suffix` with a single `/`, omitting both the
/// separator and the prefix entirely when `prefix` is absent or empty.
///
/// `suffix` is taken verbatim — pass `""` to obtain a `<prefix>/`
/// listing prefix (or `""` for root), `"HEAD"` for the head object,
/// `"refs/heads/<branch>/"` for a branch listing, and so on.
///
/// `None` and `Some("")` collapse to the same "no prefix" key shape.
/// Callers that hold the prefix as `&str` should pass `Some(prefix)`;
/// the empty-string check inside keeps the bucket-root case working.
pub(crate) fn join(prefix: Option<&str>, suffix: &str) -> String {
    match prefix {
        Some(p) if !p.is_empty() => {
            if suffix.is_empty() {
                format!("{p}/")
            } else {
                format!("{p}/{suffix}")
            }
        }
        _ => suffix.to_owned(),
    }
}

/// Returns `true` iff `stem` is a syntactically valid bundle-file SHA:
/// exactly 40 characters, all lowercase hex (`0-9`, `a-f`).
///
/// Used by the push pre-lock listing to reject malformed `<stem>.bundle`
/// keys ([`is_bundle_candidate`](crate::protocol::push)) and by the
/// `doctor` snapshot pass to flag those same keys for operator
/// attention. Pinning the predicate here keeps the two sites in
/// lockstep — push silently filters keys that this returns `false` for,
/// and doctor surfaces them.
pub(crate) fn is_valid_bundle_stem(stem: &str) -> bool {
    stem.len() == 40 && stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
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
    use super::{bundle_key, is_valid_bundle_stem, join};

    #[test]
    fn joins_prefix_and_suffix_with_slash() {
        assert_eq!(join(Some("acme"), "HEAD"), "acme/HEAD");
        assert_eq!(
            join(Some("acme/repo"), "refs/heads/main/"),
            "acme/repo/refs/heads/main/"
        );
    }

    #[test]
    fn empty_prefix_yields_suffix_verbatim() {
        assert_eq!(join(Some(""), "HEAD"), "HEAD");
        assert_eq!(join(None, "HEAD"), "HEAD");
        assert_eq!(join(Some(""), "refs/heads/main/"), "refs/heads/main/");
        assert_eq!(join(None, "refs/heads/main/"), "refs/heads/main/");
    }

    #[test]
    fn empty_suffix_yields_listing_prefix_with_trailing_slash() {
        assert_eq!(join(Some("acme"), ""), "acme/");
    }

    #[test]
    fn empty_prefix_and_suffix_yields_empty_string() {
        // Listing the bucket root with no prefix at all.
        assert_eq!(join(Some(""), ""), "");
        assert_eq!(join(None, ""), "");
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
    fn is_valid_bundle_stem_accepts_lower_hex_40() {
        assert!(is_valid_bundle_stem(
            "0123456789abcdef0123456789abcdef01234567"
        ));
        assert!(is_valid_bundle_stem(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
    }

    #[test]
    fn is_valid_bundle_stem_rejects_wrong_length_and_charset() {
        // 39 chars
        assert!(!is_valid_bundle_stem(
            "0123456789abcdef0123456789abcdef0123456"
        ));
        // 41 chars
        assert!(!is_valid_bundle_stem(
            "0123456789abcdef0123456789abcdef012345670"
        ));
        // Uppercase hex is not accepted (mirrors `Sha::from_hex`).
        assert!(!is_valid_bundle_stem(
            "0123456789ABCDEF0123456789abcdef01234567"
        ));
        // Non-hex characters
        assert!(!is_valid_bundle_stem(
            "not-a-valid-sha-not-a-valid-sha-not-aval"
        ));
        assert!(!is_valid_bundle_stem(""));
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

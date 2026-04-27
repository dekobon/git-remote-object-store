//! Helpers for constructing object-store keys.
//!
//! Both the helper protocol (push/fetch/list) and the management CLI
//! (doctor/branch/snapshot) build keys of the form `<prefix>/<suffix>`
//! and have to special-case the empty-prefix (root-of-bucket) case so
//! the resulting key has no leading slash. Centralising the rule here
//! keeps the four module call sites in lockstep and prevents the
//! recurring "leading slash on root-prefix repos" bug (#29, #32).

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

#[cfg(test)]
mod tests {
    use super::join;

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
}

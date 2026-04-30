//! Validated Git LFS object id (lowercase 64-char SHA-256 hex).
//!
//! LFS pointer files reference payloads by `<oid>` and the on-bucket
//! layout is `<prefix>/lfs/<oid>`. The OID flows in over an untrusted
//! JSON line on stdin, so we validate it once at the agent boundary
//! and pass [`LfsOid`] downward.

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

/// Length of a SHA-256 hex string. The LFS spec pins SHA-256, and the
/// upstream agent (`../git-remote-s3/git_remote_s3/lfs.py`) uses
/// `event["oid"]` verbatim as a key suffix; we enforce the format up
/// front so a bad event cannot poison the bucket namespace.
const SHA256_HEX_LEN: usize = 64;

/// Validated lowercase SHA-256 hex string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct LfsOid(String);

impl LfsOid {
    /// Borrow as a plain `&str`. Always 64 lowercase hex chars.
    #[must_use]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LfsOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for LfsOid {
    type Err = LfsOidError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != SHA256_HEX_LEN {
            return Err(LfsOidError::WrongLength { actual: s.len() });
        }
        if !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(LfsOidError::NotLowerHex);
        }
        Ok(LfsOid(s.to_owned()))
    }
}

/// Errors returned by [`LfsOid::from_str`].
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum LfsOidError {
    /// Input length differed from the SHA-256 hex length (64).
    #[error("LFS oid must be {SHA256_HEX_LEN} chars, got {actual}")]
    WrongLength {
        /// Observed length.
        actual: usize,
    },
    /// Input contained a character outside `[0-9a-f]`.
    #[error("LFS oid must be lowercase hex (0-9, a-f)")]
    NotLowerHex,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_oid() -> &'static str {
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    }

    #[test]
    fn accepts_valid_lowercase_hex() {
        let oid = LfsOid::from_str(good_oid()).expect("valid oid");
        assert_eq!(oid.as_str(), good_oid());
        assert_eq!(oid.to_string(), good_oid());
    }

    #[test]
    fn rejects_short_input() {
        assert_eq!(
            LfsOid::from_str("abc"),
            Err(LfsOidError::WrongLength { actual: 3 })
        );
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(
            LfsOid::from_str(""),
            Err(LfsOidError::WrongLength { actual: 0 })
        );
    }

    #[test]
    fn rejects_uppercase_hex() {
        let bad = "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(LfsOid::from_str(bad), Err(LfsOidError::NotLowerHex));
    }

    #[test]
    fn rejects_non_hex_chars() {
        let bad = "g123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(LfsOid::from_str(bad), Err(LfsOidError::NotLowerHex));
    }
}

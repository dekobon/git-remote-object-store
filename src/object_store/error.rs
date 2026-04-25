//! Shared error type for every [`ObjectStore`][super::ObjectStore]
//! implementation.
//!
//! Centralises the mapping of backend-specific failure codes onto a small,
//! finite set of variants so higher layers (push, fetch, doctor, LFS) can
//! pattern-match without caring whether the underlying SDK returned an
//! `aws_sdk_s3::error::SdkError` or an `azure_core::error::Error`.
//!
//! The variant set follows `execution-plan.md` §2.3 and the conditional-write
//! note in §5.1: S3 returns 412 (`PreconditionFailed`) *and* 409
//! (`ConditionalRequestConflict`) for the same `If-None-Match: "*"`
//! contention path; both must be available so backends can preserve the
//! distinction in diagnostics, while the `put_if_absent` trait method
//! collapses both into the `Ok(false)` "lock not acquired" return.

use std::error::Error as StdError;

/// Boxed source error used by [`Error::Network`] and [`Error::Other`].
///
/// `Send + Sync + 'static` so the error can cross task boundaries; this
/// matches the bounds `tokio::task::JoinHandle` and friends impose.
pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;

/// Errors returned by every [`ObjectStore`][super::ObjectStore] method.
///
/// The `String` payload on the four key-correlated variants names the key
/// (or, for `list`, the prefix) the operation was attempting, so
/// `tracing::error!` lines remain actionable without the caller adding
/// context.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Object (or, for `list`, every object under the prefix) is absent.
    #[error("object not found: {0}")]
    NotFound(String),

    /// Authentication succeeded but the principal is not allowed to perform
    /// the operation. Maps from S3 `AccessDenied` (HTTP 403) and Azure
    /// `AuthorizationFailure`.
    #[error("access denied for {0}")]
    AccessDenied(String),

    /// Conditional request returned 412 — the precondition (typically
    /// `If-None-Match: "*"`) was not satisfied. See `execution-plan.md`
    /// §5.1; backends `put_if_absent` collapses this into `Ok(false)`, so
    /// callers should rarely observe it directly.
    #[error("precondition failed for {0}")]
    PreconditionFailed(String),

    /// Conditional request returned 409. Treated by `put_if_absent` callers
    /// the same as `PreconditionFailed`, but kept distinct for diagnostics.
    #[error("conflict on {0}")]
    Conflict(String),

    /// Transport-level failure (DNS, TLS, timeout, connection reset).
    /// Carries the original SDK error as `#[source]` so the chain is
    /// preserved.
    #[error("network error")]
    Network(#[source] BoxError),

    /// Any backend failure that does not fit the variants above.
    #[error(transparent)]
    Other(BoxError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed_io(message: &str) -> BoxError {
        Box::new(std::io::Error::other(message.to_string()))
    }

    #[test]
    fn display_names_the_key() {
        assert_eq!(
            Error::NotFound("a/b".into()).to_string(),
            "object not found: a/b"
        );
        assert_eq!(
            Error::AccessDenied("a/b".into()).to_string(),
            "access denied for a/b"
        );
        assert_eq!(
            Error::PreconditionFailed("a/b".into()).to_string(),
            "precondition failed for a/b"
        );
        assert_eq!(Error::Conflict("a/b".into()).to_string(), "conflict on a/b");
    }

    #[test]
    fn network_preserves_source_chain() {
        let err = Error::Network(boxed_io("dns failure"));
        assert_eq!(err.to_string(), "network error");
        let source = err.source().expect("Network exposes its #[source]");
        assert_eq!(source.to_string(), "dns failure");
    }

    #[test]
    fn other_is_transparent() {
        let err = Error::Other(boxed_io("boom"));
        // `transparent` forwards Display to the inner error.
        assert_eq!(err.to_string(), "boom");
    }
}

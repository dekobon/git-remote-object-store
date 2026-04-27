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

/// Boxed source error used by [`ObjectStoreError::Network`] and
/// [`ObjectStoreError::Other`].
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
pub enum ObjectStoreError {
    /// Object (or, for `list`, every object under the prefix) is absent.
    #[error("object not found: {0}")]
    NotFound(String),

    /// Authentication succeeded but the principal is not allowed to perform
    /// the operation. Maps from S3 `AccessDenied` (HTTP 403) and Azure
    /// `AuthorizationFailure`.
    #[error("access denied: {0}")]
    AccessDenied(String),

    /// Conditional request returned 412 — the precondition (typically
    /// `If-None-Match: "*"`) was not satisfied. See `execution-plan.md`
    /// §5.1; backends `put_if_absent` collapses this into `Ok(false)`, so
    /// callers should rarely observe it directly.
    #[error("precondition failed: {0}")]
    PreconditionFailed(String),

    /// Conditional request returned 409. Treated by `put_if_absent` callers
    /// the same as `PreconditionFailed`, but kept distinct for diagnostics.
    #[error("conflict: {0}")]
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

/// Wrap any concrete `std::error::Error` into [`ObjectStoreError::Other`].
///
/// Replaces the open-coded `|e| ObjectStoreError::Other(Box::new(e))` closure
/// that otherwise repeats at every I/O / time-conversion / persist
/// call site.
pub(crate) fn other_boxed<E: StdError + Send + Sync + 'static>(e: E) -> ObjectStoreError {
    ObjectStoreError::Other(Box::new(e))
}

/// Wrap any concrete `std::error::Error` into [`ObjectStoreError::Network`].
///
/// Replaces the open-coded `|e| ObjectStoreError::Network(Box::new(e))`
/// closure used at every body-streaming / multipart-chunk site that
/// surfaces a transport failure.
pub(crate) fn network_boxed<E: StdError + Send + Sync + 'static>(e: E) -> ObjectStoreError {
    ObjectStoreError::Network(Box::new(e))
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
            ObjectStoreError::NotFound("a/b".into()).to_string(),
            "object not found: a/b"
        );
        assert_eq!(
            ObjectStoreError::AccessDenied("a/b".into()).to_string(),
            "access denied: a/b"
        );
        assert_eq!(
            ObjectStoreError::PreconditionFailed("a/b".into()).to_string(),
            "precondition failed: a/b"
        );
        assert_eq!(
            ObjectStoreError::Conflict("a/b".into()).to_string(),
            "conflict: a/b"
        );
    }

    #[test]
    fn network_preserves_source_chain() {
        let err = ObjectStoreError::Network(boxed_io("dns failure"));
        assert_eq!(err.to_string(), "network error");
        let source = err.source().expect("Network exposes its #[source]");
        assert_eq!(source.to_string(), "dns failure");
    }

    #[test]
    fn other_is_transparent() {
        let err = ObjectStoreError::Other(boxed_io("boom"));
        // `transparent` forwards Display to the inner error.
        assert_eq!(err.to_string(), "boom");
    }
}

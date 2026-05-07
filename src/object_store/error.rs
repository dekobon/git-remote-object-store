//! Shared error type for every [`ObjectStore`][super::ObjectStore]
//! implementation.
//!
//! Centralises the mapping of backend-specific failure codes onto a small,
//! finite set of variants so higher layers (push, fetch, doctor, LFS) can
//! pattern-match without caring whether the underlying SDK returned an
//! `aws_sdk_s3::error::SdkError` or an `azure_core::error::Error`.
//!
//! On the conditional-write path, S3 returns 412 (`PreconditionFailed`)
//! *and* 409 (`ConditionalRequestConflict`) for the same
//! `If-None-Match: "*"` contention path; both variants are kept here so
//! backends can preserve the distinction in diagnostics, while the
//! `put_if_absent` trait method collapses both into the `Ok(false)`
//! "lock not acquired" return.

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
    /// `If-None-Match: "*"`) was not satisfied. `put_if_absent`
    /// collapses this into `Ok(false)`, so callers should rarely
    /// observe it directly.
    #[error("precondition failed: {0}")]
    PreconditionFailed(String),

    /// Conditional request returned 409. Treated by `put_if_absent` callers
    /// the same as `PreconditionFailed`, but kept distinct for diagnostics.
    #[error("conflict: {0}")]
    Conflict(String),

    /// Upload body exceeded the backend's size ceiling for the API call
    /// in use. Maps from S3 `EntityTooLarge` (single-PUT > 5 GiB) and
    /// Azure 413 / `RequestBodyTooLarge` (single Put Blob > 5000 MiB).
    /// `limit_bytes` is the backend-specific ceiling at the time the
    /// classifier ran, surfaced so the wire-line message names a concrete
    /// number rather than dumping an opaque SDK error chain.
    #[error("upload exceeds backend size limit ({})", format_byte_limit(*limit_bytes))]
    PayloadTooLarge {
        /// The backend's documented single-call size ceiling, in bytes.
        limit_bytes: u64,
    },

    /// Ranged GET requested a byte range that the backend cannot
    /// satisfy. Maps from HTTP 416 on both S3 and Azure, and surfaces
    /// caller-side range bugs (`start > end`) without issuing a network
    /// call. The `requested` range is the half-open `[start, end)` the
    /// caller passed to `get_bytes_range`.
    #[error("range {}..{} not satisfiable for key `{key}`", requested.start, requested.end)]
    RangeNotSatisfiable {
        /// Key being read.
        key: String,
        /// Range the caller asked for (half-open, end-exclusive).
        requested: std::ops::Range<u64>,
    },

    /// Transport-level failure (DNS, TLS, timeout, connection reset).
    /// Carries the original SDK error as `#[source]` so the chain is
    /// preserved. The inner error is included in the display so the
    /// SDK-level detail (e.g. "operation timed out") surfaces in the
    /// push `error <ref>` wire line without requiring verbose logging.
    #[error("network error: {0}")]
    Network(#[source] BoxError),

    /// Operation is not implemented for the backend in use. Used by
    /// optional [`ObjectStore`](crate::object_store::ObjectStore)
    /// methods such as `presigned_get_url` that not every backend
    /// can satisfy (e.g. `MockStore` in tests, or `AzureStore`
    /// configured with a `TokenCredential` rather than a shared
    /// account key — the latter cannot generate a service-SAS). The
    /// payload is the full operator-facing message; the variant
    /// adds no prefix of its own so chained errors do not double-
    /// say "not supported".
    #[error("{0}")]
    Unsupported(String),

    /// Any backend failure that does not fit the variants above.
    #[error(transparent)]
    Other(BoxError),
}

/// Render a byte ceiling as the largest unit that divides it cleanly
/// (`5 GiB`, `5000 MiB`, otherwise raw bytes). Used in
/// [`ObjectStoreError::PayloadTooLarge`]'s `Display` so the wire-line
/// reads as a familiar quota number rather than "5368709120".
fn format_byte_limit(bytes: u64) -> String {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    if bytes >= GIB && bytes.is_multiple_of(GIB) {
        format!("{} GiB", bytes / GIB)
    } else if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} B")
    }
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
        assert_eq!(err.to_string(), "network error: dns failure");
        let source = err.source().expect("Network exposes its #[source]");
        assert_eq!(source.to_string(), "dns failure");
    }

    #[test]
    fn other_is_transparent() {
        let err = ObjectStoreError::Other(boxed_io("boom"));
        // `transparent` forwards Display to the inner error.
        assert_eq!(err.to_string(), "boom");
    }

    #[test]
    fn payload_too_large_renders_gib_when_exact() {
        let err = ObjectStoreError::PayloadTooLarge {
            limit_bytes: 5 * (1 << 30),
        };
        assert_eq!(err.to_string(), "upload exceeds backend size limit (5 GiB)");
    }

    #[test]
    fn payload_too_large_renders_mib_when_not_a_clean_gib() {
        // Azure single-PUT ceiling is 5000 MiB — close to but not 5 GiB.
        let err = ObjectStoreError::PayloadTooLarge {
            limit_bytes: 5_000 * (1 << 20),
        };
        assert_eq!(
            err.to_string(),
            "upload exceeds backend size limit (5000 MiB)"
        );
    }

    #[test]
    fn payload_too_large_falls_back_to_raw_bytes() {
        let err = ObjectStoreError::PayloadTooLarge { limit_bytes: 1_234 };
        assert_eq!(
            err.to_string(),
            "upload exceeds backend size limit (1234 B)"
        );
    }

    #[test]
    fn range_not_satisfiable_names_key_and_range() {
        let err = ObjectStoreError::RangeNotSatisfiable {
            key: "packs/abc.pack".to_string(),
            requested: 100..200,
        };
        assert_eq!(
            err.to_string(),
            "range 100..200 not satisfiable for key `packs/abc.pack`"
        );
    }
}

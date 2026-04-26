//! Backend factory: turns a parsed [`RemoteUrl`] into an
//! [`Arc<dyn ObjectStore>`] for the protocol REPL to drive.
//!
//! S3 is wired today (Phase 5). Azure URLs return a structured
//! "not yet implemented" error pending Phase 11.

use std::sync::Arc;

use crate::object_store::s3::S3Store;
use crate::object_store::{Error as ObjectStoreError, ObjectStore};
use crate::url::RemoteUrl;

/// Errors surfaced by [`build`].
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// The Azure backend is not yet wired (Phase 11). The four
    /// `git-remote-az-*` bins still build because they share the
    /// protocol REPL with the S3 bins; this error fires only when one
    /// is actually invoked.
    #[error("azure backend not yet implemented (Phase 11)")]
    AzureNotImplemented,

    /// S3 backend construction failed (credential resolution, malformed
    /// endpoint, ...).
    #[error("failed to construct S3 backend: {0}")]
    S3(#[source] ObjectStoreError),
}

/// Construct the right [`ObjectStore`] for `remote`.
pub async fn build(remote: &RemoteUrl) -> Result<Arc<dyn ObjectStore>, BackendError> {
    match remote {
        RemoteUrl::S3 { .. } => {
            let store = S3Store::from_remote_url(remote)
                .await
                .map_err(BackendError::S3)?;
            Ok(Arc::new(store))
        }
        RemoteUrl::Azure { .. } => Err(BackendError::AzureNotImplemented),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url::parse;

    #[tokio::test]
    async fn azure_url_returns_not_implemented() {
        let remote = parse("az+https://acct.blob.core.windows.net/container/repo").unwrap();
        match build(&remote).await {
            Ok(_) => panic!("expected Azure backend to error"),
            Err(BackendError::AzureNotImplemented) => {}
            Err(other) => panic!("unexpected backend error: {other:?}"),
        }
    }
}

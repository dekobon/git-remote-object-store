//! Backend factory: turns a parsed [`RemoteUrl`] into an
//! [`Arc<dyn ObjectStore>`] for the protocol REPL to drive.
//!
//! Both S3 (Phase 5) and Azure Blob (Phase 11) are wired here.

use std::sync::Arc;

use crate::object_store::azure::AzureStore;
use crate::object_store::s3::S3Store;
use crate::object_store::{ObjectStore, ObjectStoreError};
use crate::url::RemoteUrl;

/// Errors surfaced by [`build`].
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// S3 backend construction failed (credential resolution, malformed
    /// endpoint, ...).
    #[error("failed to construct S3 backend: {0}")]
    S3(#[source] ObjectStoreError),

    /// Azure backend construction failed (credential resolution,
    /// malformed endpoint, missing env var for credential alias, ...).
    #[error("failed to construct Azure Blob backend: {0}")]
    Azure(#[source] ObjectStoreError),
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
        RemoteUrl::Azure { .. } => {
            let store = AzureStore::from_remote_url(remote)
                .await
                .map_err(BackendError::Azure)?;
            Ok(Arc::new(store))
        }
    }
}

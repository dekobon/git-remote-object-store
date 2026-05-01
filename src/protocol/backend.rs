//! Backend factory: turns a parsed [`RemoteUrl`] into an
//! [`Arc<dyn ObjectStore>`] for the protocol REPL to drive.
//!
//! Both S3 and Azure Blob are wired here.
//!
//! # Eager probe and categorical error mapping
//!
//! Mirrors upstream's `S3Remote.__init__` (`../git-remote-s3/git_remote_s3/remote.py:78-85`):
//! after constructing the SDK client, [`build`] runs a single low-cost listing
//! call (`max_keys=1` for S3, `maxresults=1` for Azure) and folds well-known
//! failures into one of three categorical [`BackendError`] variants. Helper
//! binaries pattern-match on these variants via [`fatal_message`] to emit
//! single-line `fatal:` diagnostics that match upstream's wording at
//! `remote.py:574-593`.
//!
//! The probe runs **once** at backend construction. Per-call errors during
//! `fetch` / `push` continue to flow through their existing typed paths.

use std::sync::Arc;

use crate::keys;
use crate::object_store::azure::AzureStore;
use crate::object_store::s3::S3Store;
use crate::object_store::{ObjectStore, ObjectStoreError};
use crate::url::{RemoteUrl, StorageEngine};

/// Which backend a [`BackendError`] refers to. Drives the wording in
/// [`fatal_message`] (S3 says "bucket"; Azure says "container").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Amazon S3 (or any S3-compatible) backend.
    S3,
    /// Azure Blob Storage backend.
    Azure,
}

/// Errors surfaced by [`build`]. The three variants line up with the
/// three categorical fatal lines upstream's Python helper emits at
/// `../git-remote-s3/git_remote_s3/remote.py:574-593`.
///
/// The `Display` strings deliberately match upstream's wording (no
/// colons, "user" prefix on `NotAuthorized`) so that
/// [`fatal_message`] is just `format!("fatal: {err}")` — a single
/// source of truth for the operator-facing wording.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// Bucket (S3) or container (Azure) does not exist. Maps from a
    /// 404 / `NoSuchBucket` on the construction-time probe.
    #[error("{} not found {name}", container_word(*kind))]
    BucketNotFound {
        /// Which backend reported the failure.
        kind: BackendKind,
        /// Bucket or container name.
        name: String,
    },

    /// Authentication succeeded but the principal lacks the listed
    /// `action` on the named bucket/container. Maps from a 403 /
    /// `AccessDenied` on the probe.
    #[error("user not authorized to perform {action} on {name}")]
    NotAuthorized {
        /// Which backend reported the failure.
        kind: BackendKind,
        /// SDK call name the principal was denied (e.g. `ListObjectsV2`).
        action: String,
        /// Bucket or container name.
        name: String,
    },

    /// Catch-all for credential acquisition failures (missing AWS
    /// profile, expired creds, missing Azure credential alias, ...) and
    /// transport-level failures during the probe. Mirrors upstream's
    /// `(ClientError, ProfileNotFound, CredentialRetrievalError,
    /// NoCredentialsError, UnknownCredentialError)` arm at `remote.py:586-593`.
    #[error("invalid credentials {source}")]
    InvalidCredentials {
        /// The underlying [`ObjectStoreError`] preserved as `#[source]`.
        #[source]
        source: ObjectStoreError,
    },

    /// The `FORMAT` key records an engine name this binary does not support.
    #[error(
        "bucket uses unknown storage engine `{stored}`; \
         this client only supports `bundle`"
    )]
    UnknownStoredEngine {
        /// The engine name as written in the `FORMAT` key.
        stored: String,
    },

    /// The `?engine=` URL parameter conflicts with the engine stored in the
    /// `FORMAT` key.
    #[error(
        "URL specifies engine `{url_engine}` but this bucket uses `{stored_engine}`; \
         remove the `?engine=` parameter from the remote URL"
    )]
    EngineMismatch {
        /// Engine requested via the `?engine=` URL parameter.
        url_engine: StorageEngine,
        /// Engine stored in the `FORMAT` key.
        stored_engine: StorageEngine,
    },
}

const fn container_word(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::S3 => "bucket",
        BackendKind::Azure => "container",
    }
}

/// Render `err` as the upstream-style single-line `fatal:` diagnostic
/// helper binaries write to stderr.
///
/// The S3 wording matches `../git-remote-s3/git_remote_s3/remote.py:584-593`
/// byte-for-byte; the Azure wording substitutes "container" for "bucket"
/// (no upstream Python equivalent — Azure support is Rust-port-only). The
/// upstream wording lives in [`BackendError`]'s `Display` derive — see the
/// type-level doc comment.
#[must_use]
pub fn fatal_message(err: &BackendError) -> String {
    format!("fatal: {err}")
}

/// Fold an [`ObjectStoreError`] from backend construction or the eager
/// probe into the categorical [`BackendError`] surface used by helper
/// binaries.
fn classify(
    kind: BackendKind,
    name: &str,
    action: &'static str,
    err: ObjectStoreError,
) -> BackendError {
    match err {
        ObjectStoreError::NotFound(_) => BackendError::BucketNotFound {
            kind,
            name: name.to_owned(),
        },
        ObjectStoreError::AccessDenied(_) => BackendError::NotAuthorized {
            kind,
            action: action.to_owned(),
            name: name.to_owned(),
        },
        other => BackendError::InvalidCredentials { source: other },
    }
}

/// Read the `FORMAT` key at `<prefix>/FORMAT` and validate it against the
/// engine declared in the URL. Returns `Ok(())` when:
///
/// - The key does not exist (new bucket — engine will be written on first push).
/// - The stored engine matches the URL engine (or no engine was declared).
///
/// # Errors
///
/// - [`BackendError::UnknownStoredEngine`] when the `FORMAT` content is not a
///   recognised engine name.
/// - [`BackendError::EngineMismatch`] when the URL engine conflicts with the
///   stored engine.
/// - [`BackendError::InvalidCredentials`] for transport / auth failures reading
///   the key.
async fn validate_format(
    store: &dyn ObjectStore,
    prefix: &str,
    url_engine: Option<StorageEngine>,
) -> Result<(), BackendError> {
    let format_key = keys::join(prefix, "FORMAT");
    let bytes = match store.get_bytes(&format_key).await {
        Ok(b) => b,
        // No FORMAT key — this is a new or legacy bucket. The engine will
        // be written on the first push.
        Err(ObjectStoreError::NotFound(_)) => return Ok(()),
        Err(e) => return Err(BackendError::InvalidCredentials { source: e }),
    };

    // Trim ASCII whitespace so a trailing newline in the stored value does
    // not cause a spurious parse failure.
    let stored_name = String::from_utf8_lossy(&bytes);
    let stored_name = stored_name.trim();

    let stored_engine =
        StorageEngine::from_name(stored_name).ok_or_else(|| BackendError::UnknownStoredEngine {
            stored: stored_name.to_owned(),
        })?;

    if let Some(url_engine) = url_engine
        && url_engine != stored_engine
    {
        return Err(BackendError::EngineMismatch {
            url_engine,
            stored_engine,
        });
    }

    Ok(())
}

/// Construct the right [`ObjectStore`] for `remote` and verify it is
/// reachable with a single low-cost list call. After the probe, reads the
/// `FORMAT` key to validate the storage engine declared in `?engine=`.
///
/// # Errors
///
/// Returns [`BackendError`] if the backend cannot be constructed (e.g.
/// invalid credentials or endpoint), the probe list call fails (e.g.
/// bucket/container not found or permission denied), or the `FORMAT` key
/// conflicts with `?engine=`.
pub async fn build(remote: &RemoteUrl) -> Result<Arc<dyn ObjectStore>, BackendError> {
    let prefix = remote.prefix().unwrap_or_default();
    let url_engine = remote.flags().engine;
    let store: Arc<dyn ObjectStore> = match remote {
        RemoteUrl::S3 { bucket, .. } => {
            let store = S3Store::from_remote_url(remote)
                .await
                .map_err(|e| classify(BackendKind::S3, bucket, "ListObjectsV2", e))?;
            store
                .probe(prefix)
                .await
                .map_err(|e| classify(BackendKind::S3, bucket, "ListObjectsV2", e))?;
            Arc::new(store)
        }
        RemoteUrl::Azure { container, .. } => {
            let store = AzureStore::from_remote_url(remote)
                .await
                .map_err(|e| classify(BackendKind::Azure, container, "ListBlobs", e))?;
            store
                .probe(prefix)
                .await
                .map_err(|e| classify(BackendKind::Azure, container, "ListBlobs", e))?;
            Arc::new(store)
        }
    };
    validate_format(store.as_ref(), prefix, url_engine).await?;
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::mock::MockStore;
    use bytes::Bytes;

    fn boxed(message: &str) -> crate::object_store::BoxError {
        Box::new(std::io::Error::other(message.to_string()))
    }

    #[test]
    fn classify_maps_not_found_to_bucket_not_found_for_s3() {
        let err = classify(
            BackendKind::S3,
            "mybucket",
            "ListObjectsV2",
            ObjectStoreError::NotFound("mybucket".into()),
        );
        assert!(matches!(
            err,
            BackendError::BucketNotFound {
                kind: BackendKind::S3,
                ref name
            } if name == "mybucket"
        ));
    }

    #[test]
    fn classify_maps_not_found_to_bucket_not_found_for_azure() {
        let err = classify(
            BackendKind::Azure,
            "mycontainer",
            "ListBlobs",
            ObjectStoreError::NotFound("mycontainer".into()),
        );
        assert!(matches!(
            err,
            BackendError::BucketNotFound {
                kind: BackendKind::Azure,
                ref name
            } if name == "mycontainer"
        ));
    }

    #[test]
    fn classify_maps_access_denied_to_not_authorized() {
        let err = classify(
            BackendKind::S3,
            "mybucket",
            "ListObjectsV2",
            ObjectStoreError::AccessDenied("mybucket".into()),
        );
        let BackendError::NotAuthorized { kind, action, name } = err else {
            panic!("expected NotAuthorized");
        };
        assert_eq!(kind, BackendKind::S3);
        assert_eq!(action, "ListObjectsV2");
        assert_eq!(name, "mybucket");
    }

    #[test]
    fn classify_maps_network_to_invalid_credentials() {
        use std::error::Error as _;
        let err = classify(
            BackendKind::S3,
            "mybucket",
            "ListObjectsV2",
            ObjectStoreError::Network(boxed("dns failure")),
        );
        let BackendError::InvalidCredentials { source } = err else {
            panic!("expected InvalidCredentials, got {err:?}");
        };
        // Source must round-trip the original error so operators see
        // the underlying cause (e.g. "dns failure"), not a placeholder.
        assert!(matches!(source, ObjectStoreError::Network(_)));
        assert!(
            source
                .source()
                .is_some_and(|s| s.to_string() == "dns failure"),
            "Network source must preserve the original error chain",
        );
    }

    #[test]
    fn classify_maps_other_to_invalid_credentials() {
        let err = classify(
            BackendKind::Azure,
            "mycontainer",
            "ListBlobs",
            ObjectStoreError::Other(boxed("missing AZ_CRED env var")),
        );
        let BackendError::InvalidCredentials { source } = err else {
            panic!("expected InvalidCredentials");
        };
        assert_eq!(source.to_string(), "missing AZ_CRED env var");
    }

    #[test]
    fn classify_maps_precondition_failed_to_invalid_credentials() {
        // 412 / 409 are impossible from a list call but should not panic
        // — they fall through to the catch-all arm.
        let err = classify(
            BackendKind::S3,
            "mybucket",
            "ListObjectsV2",
            ObjectStoreError::PreconditionFailed("mybucket".into()),
        );
        assert!(matches!(err, BackendError::InvalidCredentials { .. }));
    }

    #[test]
    fn fatal_message_s3_bucket_not_found_matches_upstream() {
        let err = BackendError::BucketNotFound {
            kind: BackendKind::S3,
            name: "mybucket".into(),
        };
        assert_eq!(fatal_message(&err), "fatal: bucket not found mybucket");
    }

    #[test]
    fn fatal_message_azure_container_not_found() {
        let err = BackendError::BucketNotFound {
            kind: BackendKind::Azure,
            name: "mycontainer".into(),
        };
        assert_eq!(
            fatal_message(&err),
            "fatal: container not found mycontainer"
        );
    }

    #[test]
    fn fatal_message_not_authorized_matches_upstream() {
        let err = BackendError::NotAuthorized {
            kind: BackendKind::S3,
            action: "ListObjectsV2".into(),
            name: "mybucket".into(),
        };
        assert_eq!(
            fatal_message(&err),
            "fatal: user not authorized to perform ListObjectsV2 on mybucket"
        );
    }

    #[test]
    fn fatal_message_invalid_credentials_appends_source() {
        let err = BackendError::InvalidCredentials {
            source: ObjectStoreError::Other(boxed("credential acquisition failed")),
        };
        assert_eq!(
            fatal_message(&err),
            "fatal: invalid credentials credential acquisition failed"
        );
    }

    // --- validate_format --------------------------------------------------

    #[tokio::test]
    async fn validate_format_passes_when_key_absent() {
        let store = MockStore::new();
        // No FORMAT key in the store — should succeed (new bucket).
        validate_format(&store, "", None).await.unwrap();
        validate_format(&store, "my-repo", None).await.unwrap();
    }

    #[tokio::test]
    async fn validate_format_passes_when_stored_engine_matches_url() {
        let store = MockStore::new();
        store.insert("FORMAT", Bytes::from_static(b"bundle"));
        validate_format(&store, "", Some(StorageEngine::Bundle))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn validate_format_passes_when_no_url_engine_declared() {
        let store = MockStore::new();
        store.insert("FORMAT", Bytes::from_static(b"bundle"));
        // No URL engine — stored value is authoritative; no conflict.
        validate_format(&store, "", None).await.unwrap();
    }

    #[tokio::test]
    async fn validate_format_passes_when_key_has_trailing_newline() {
        let store = MockStore::new();
        store.insert("FORMAT", Bytes::from_static(b"bundle\n"));
        validate_format(&store, "", Some(StorageEngine::Bundle))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn validate_format_rejects_unknown_stored_engine() {
        let store = MockStore::new();
        store.insert("FORMAT", Bytes::from_static(b"pack"));
        let err = validate_format(&store, "", None).await.unwrap_err();
        assert!(
            matches!(err, BackendError::UnknownStoredEngine { ref stored } if stored == "pack"),
            "expected UnknownStoredEngine(pack), got {err:?}",
        );
    }

    #[tokio::test]
    async fn validate_format_uses_prefix_for_key_lookup() {
        let store = MockStore::new();
        // Key at prefix/FORMAT, not at bare FORMAT.
        store.insert("my-repo/FORMAT", Bytes::from_static(b"bundle"));
        // Without prefix: FORMAT absent → passes.
        validate_format(&store, "", None).await.unwrap();
        // With prefix: FORMAT found → passes.
        validate_format(&store, "my-repo", None).await.unwrap();
    }

    #[test]
    fn unknown_stored_engine_error_message() {
        let err = BackendError::UnknownStoredEngine {
            stored: "pack".into(),
        };
        assert_eq!(
            fatal_message(&err),
            "fatal: bucket uses unknown storage engine `pack`; this client only supports `bundle`",
        );
    }
}

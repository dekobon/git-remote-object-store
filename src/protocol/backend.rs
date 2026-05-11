//! Backend factory: turns a parsed [`RemoteUrl`] into an
//! [`Arc<dyn ObjectStore>`] for the protocol REPL to drive.
//!
//! Both S3 and Azure Blob are wired here.
//!
//! # Eager probe and categorical error mapping
//!
//! After constructing the SDK client, [`build`] runs a single low-cost
//! listing call (`max_keys=1` for S3, `maxresults=1` for Azure) and folds
//! well-known failures into categorical [`BackendError`] variants. Helper
//! binaries pattern-match on these variants via [`fatal_message`] to emit
//! single-line `fatal:` diagnostics.
//!
//! The probe runs **once** at backend construction. Per-call errors during
//! `fetch` / `push` continue to flow through their existing typed paths.

use std::sync::Arc;

use crate::keys;
use crate::object_store::azure::AzureStore;
use crate::object_store::s3::S3Store;
use crate::object_store::{BoxError, ObjectStore, ObjectStoreError};
use crate::url::{RemoteUrl, StorageEngine};

pub use crate::url::BackendKind;

/// Errors surfaced by [`build`].
///
/// The `Display` strings (no colons, "user" prefix on `NotAuthorized`)
/// are the single source of truth for the operator-facing wording
/// rendered by [`fatal_message`].
///
/// # Invariant for `fatal_message`
///
/// [`fatal_message`] walks the error source chain starting one level
/// past `err.source()`, because any variant with a `#[source]` field
/// already embeds `{source}` in its `Display` format string (making
/// the first level visible without chain-walking). **Every future
/// variant that adds a `#[source]` field must also include `{source}`
/// in its format string.** Omitting `{source}` while keeping
/// `#[source]` causes `fatal_message` to silently drop the first
/// source level from the rendered message.
///
/// # Invariant for `Network` classification
///
/// When [`classify`] or [`validate_format`] encounter
/// [`ObjectStoreError::Network`], they extract the inner [`BoxError`]
/// and store it directly in [`BackendError::Network::source`]. They
/// must **never** wrap the whole `ObjectStoreError::Network` (whose
/// own `Display` is `"network error: <inner>"`) into another
/// `BackendError` variant whose `Display` also includes the source —
/// that produces the redundant `"network error: network error: ..."`
/// rendering [`fatal_message`] is documented to avoid. New
/// `ObjectStoreError` variants that carry transport semantics must
/// either add a dedicated `BackendError::Network`-style classification
/// arm or store the inner cause directly.
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

    /// Transport-level failure during backend construction (probe or FORMAT
    /// key read): DNS resolution failed, connection refused, TLS handshake
    /// error, or request timeout. This indicates a URL or network
    /// configuration problem — not a credentials problem. The inner error is
    /// extracted from [`ObjectStoreError::Network`] and stored directly to
    /// avoid the redundant "network error: network error" display that would
    /// result from wrapping it whole.
    #[error("connection error: {source}")]
    Network {
        /// The underlying transport error preserved for chain-walking.
        #[source]
        source: BoxError,
    },

    /// Catch-all for credential acquisition failures (missing AWS
    /// profile, expired creds, missing Azure credential alias, ...).
    #[error("invalid credentials {source}")]
    InvalidCredentials {
        /// The underlying [`ObjectStoreError`] preserved as `#[source]`.
        #[source]
        source: ObjectStoreError,
    },

    /// The `FORMAT` key records an engine name this binary does not support.
    ///
    /// The supported-engine list is rendered from
    /// [`StorageEngine::supported_list_str`] so adding a new variant
    /// updates this message automatically.
    #[error(
        "bucket uses unknown storage engine `{stored}`; \
         this client supports {}",
        StorageEngine::supported_list_str()
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

/// Render `err` as a single-line `fatal:` diagnostic helper binaries
/// write to stderr.
///
/// The Azure wording substitutes "container" for "bucket". The wording
/// lives in [`BackendError`]'s `Display` derive — see the type-level
/// doc comment.
///
/// Variants like [`BackendError::InvalidCredentials`] and
/// [`BackendError::Network`] inline their immediate source via
/// `{source}`/`{0}` in the format string, sometimes transitively when
/// the source itself wraps another typed error. The chain-walk is done
/// by [`super::append_source_chain`], which dedups any level whose
/// `Display` text is already at the tail of `msg` — so the `fatal:`
/// line surfaces deeper root causes (e.g. the io / DNS error nested
/// inside the SDK dispatch failure) without producing the duplicated
/// "network error: network error: …" rendering that a naive walk
/// would.
#[must_use]
pub fn fatal_message(err: &BackendError) -> String {
    let mut msg = format!("fatal: {err}");
    super::append_source_chain(&mut msg, err);
    msg
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
        ObjectStoreError::Network(inner) => BackendError::Network { source: inner },
        other => BackendError::InvalidCredentials { source: other },
    }
}

/// Read the `FORMAT` key at `<prefix>/FORMAT` and validate it against the
/// engine declared in the URL. Returns `Ok(())` when:
///
/// - The key does not exist (new bucket — engine will be written on first push).
/// - The stored engine matches the URL engine (or no engine was declared).
///
/// Returns the resolved engine in priority order:
///
/// 1. the engine stored in `FORMAT` when present and recognised,
/// 2. the URL engine when `FORMAT` is absent and the URL declared one,
/// 3. [`StorageEngine::Bundle`] otherwise (the default for new buckets).
///
/// One FORMAT read per call. [`build`] surfaces the resolved engine to
/// its caller so [`crate::protocol::run`] can dispatch without a second
/// network round trip.
///
/// # Errors
///
/// - [`BackendError::UnknownStoredEngine`] when the `FORMAT` content is not a
///   recognised engine name.
/// - [`BackendError::EngineMismatch`] when the URL engine conflicts with the
///   stored engine.
/// - [`BackendError::Network`] for transport failures (DNS, TLS, timeout)
///   reading the key.
/// - [`BackendError::InvalidCredentials`] for auth / credential failures
///   reading the key, or non-UTF-8 bytes in the FORMAT body.
pub async fn validate_format(
    store: &dyn ObjectStore,
    prefix: &str,
    url_engine: Option<StorageEngine>,
) -> Result<StorageEngine, BackendError> {
    let format_key = keys::join(Some(prefix), "FORMAT");
    let bytes = match store.get_bytes(&format_key).await {
        Ok(b) => b,
        // No FORMAT key — this is a new or legacy bucket. The engine
        // will be written on the first push; until then, the URL value
        // (or the Bundle default) is authoritative.
        Err(ObjectStoreError::NotFound(_)) => {
            return Ok(url_engine.unwrap_or(StorageEngine::Bundle));
        }
        Err(ObjectStoreError::Network(inner)) => {
            return Err(BackendError::Network { source: inner });
        }
        Err(e) => return Err(BackendError::InvalidCredentials { source: e }),
    };

    // Trim ASCII whitespace so a trailing newline in the stored value does
    // not cause a spurious parse failure. Use `from_utf8` (not lossy) so
    // non-UTF-8 bytes in the FORMAT key surface as an error rather than
    // silently producing a replacement-character engine name that would
    // never match a valid StorageEngine variant.
    let stored_name =
        std::str::from_utf8(&bytes).map_err(|_| BackendError::InvalidCredentials {
            source: ObjectStoreError::Other(Box::new(std::io::Error::other(
                "FORMAT key contains non-UTF-8 bytes",
            ))),
        })?;
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

    Ok(stored_engine)
}

/// Construct the right [`ObjectStore`] for `remote`, verify it is
/// reachable with a single low-cost list call, and resolve the storage
/// engine from the `FORMAT` key in one pass.
///
/// Returns the connected store paired with the resolved
/// [`StorageEngine`]. The engine is computed from `FORMAT` (when
/// present) plus the URL's `?engine=` flag, with [`StorageEngine::Bundle`]
/// as the default for buckets that have no `FORMAT` key yet.
///
/// # Errors
///
/// Returns [`BackendError`] if the backend cannot be constructed (e.g.
/// invalid credentials or endpoint), the probe list call fails (e.g.
/// bucket/container not found or permission denied), or the `FORMAT` key
/// conflicts with `?engine=`.
pub async fn build(
    remote: &RemoteUrl,
) -> Result<(Arc<dyn ObjectStore>, StorageEngine), BackendError> {
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
    let engine = validate_format(store.as_ref(), prefix, url_engine).await?;
    Ok((store, engine))
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
    fn classify_maps_network_to_network_error() {
        let err = classify(
            BackendKind::S3,
            "mybucket",
            "ListObjectsV2",
            ObjectStoreError::Network(boxed("dns failure")),
        );
        let BackendError::Network { source } = err else {
            panic!("expected Network, got {err:?}");
        };
        // The BoxError is extracted from ObjectStoreError::Network directly,
        // so its Display is the inner message, not "network error".
        assert_eq!(source.to_string(), "dns failure");
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
    fn fatal_message_s3_bucket_not_found_renders_expected_wording() {
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
    fn fatal_message_not_authorized_renders_expected_wording() {
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

    #[test]
    fn fatal_message_network_includes_root_cause() {
        // BackendError::Network stores the BoxError directly (not wrapped in
        // ObjectStoreError::Network), so the Display is "connection error: <source>"
        // and fatal_message walks one level deeper from source.
        let err = BackendError::Network {
            source: boxed("dns lookup failed"),
        };
        assert_eq!(
            fatal_message(&err),
            "fatal: connection error: dns lookup failed"
        );
    }

    #[test]
    fn fatal_message_walks_full_chain() {
        use std::error::Error as StdError;
        use std::fmt;

        // A two-level chain: Network { source: mid } where mid itself has a
        // source. Verifies the `while` loop in fatal_message appends every
        // level, not just the first one it reaches.
        #[derive(Debug)]
        struct WrappedError {
            msg: &'static str,
            inner: Box<dyn StdError + Send + Sync + 'static>,
        }
        impl fmt::Display for WrappedError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.msg)
            }
        }
        impl StdError for WrappedError {
            fn source(&self) -> Option<&(dyn StdError + 'static)> {
                Some(self.inner.as_ref())
            }
        }

        let err = BackendError::Network {
            source: Box::new(WrappedError {
                msg: "dispatch failure",
                inner: boxed("connection refused"),
            }),
        };
        assert_eq!(
            fatal_message(&err),
            "fatal: connection error: dispatch failure: connection refused"
        );
    }

    #[test]
    fn fatal_message_engine_mismatch() {
        // Pin the wording with two distinct engines so the assertion is
        // not structurally circular (Lesson #6 — expected values derived
        // from the spec). Picking `Packchain` URL against a `Bundle`
        // bucket exercises the realistic operator-error path: someone
        // adds `?engine=packchain` to a remote that was first pushed as
        // `bundle`.
        let url_engine = StorageEngine::Packchain;
        let stored_engine = StorageEngine::Bundle;
        let err = BackendError::EngineMismatch {
            url_engine,
            stored_engine,
        };
        let expected = "\
            fatal: URL specifies engine `packchain` but this bucket uses `bundle`; \
            remove the `?engine=` parameter from the remote URL";
        assert_eq!(fatal_message(&err), expected);
    }

    // --- validate_format --------------------------------------------------

    #[tokio::test]
    async fn validate_format_passes_when_key_absent() {
        let store = MockStore::new();
        // No FORMAT key in the store — should resolve to Bundle (the
        // default for new buckets) when the URL also omits the engine.
        assert_eq!(
            validate_format(&store, "", None).await.unwrap(),
            StorageEngine::Bundle,
        );
        assert_eq!(
            validate_format(&store, "my-repo", None).await.unwrap(),
            StorageEngine::Bundle,
        );
        // Empty bucket + URL declares an engine → resolve to URL value.
        assert_eq!(
            validate_format(&store, "", Some(StorageEngine::Packchain))
                .await
                .unwrap(),
            StorageEngine::Packchain,
        );
    }

    #[tokio::test]
    async fn validate_format_passes_when_stored_engine_matches_url() {
        let store = MockStore::new();
        store.insert("FORMAT", Bytes::from_static(b"bundle"));
        assert_eq!(
            validate_format(&store, "", Some(StorageEngine::Bundle))
                .await
                .unwrap(),
            StorageEngine::Bundle,
        );
    }

    #[tokio::test]
    async fn validate_format_passes_when_no_url_engine_declared() {
        let store = MockStore::new();
        store.insert("FORMAT", Bytes::from_static(b"bundle"));
        // No URL engine — stored value is authoritative; no conflict.
        assert_eq!(
            validate_format(&store, "", None).await.unwrap(),
            StorageEngine::Bundle,
        );
    }

    #[tokio::test]
    async fn validate_format_passes_when_key_has_trailing_newline() {
        let store = MockStore::new();
        store.insert("FORMAT", Bytes::from_static(b"bundle\n"));
        assert_eq!(
            validate_format(&store, "", Some(StorageEngine::Bundle))
                .await
                .unwrap(),
            StorageEngine::Bundle,
        );
    }

    #[tokio::test]
    async fn validate_format_rejects_url_packchain_against_stored_bundle() {
        // Operator typo: bucket was first pushed as `bundle`, then
        // `?engine=packchain` was added to the remote URL. Stored value
        // is authoritative, so we must reject with a clear mismatch.
        let store = MockStore::new();
        store.insert("FORMAT", Bytes::from_static(b"bundle"));
        let err = validate_format(&store, "", Some(StorageEngine::Packchain))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                BackendError::EngineMismatch {
                    url_engine: StorageEngine::Packchain,
                    stored_engine: StorageEngine::Bundle,
                }
            ),
            "expected EngineMismatch(url=packchain, stored=bundle), got {err:?}",
        );
    }

    #[tokio::test]
    async fn validate_format_rejects_url_bundle_against_stored_packchain() {
        // Symmetric direction: bucket was first pushed as `packchain`,
        // then a stale `?engine=bundle` URL is reused. Same rejection.
        let store = MockStore::new();
        store.insert("FORMAT", Bytes::from_static(b"packchain"));
        let err = validate_format(&store, "", Some(StorageEngine::Bundle))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                BackendError::EngineMismatch {
                    url_engine: StorageEngine::Bundle,
                    stored_engine: StorageEngine::Packchain,
                }
            ),
            "expected EngineMismatch(url=bundle, stored=packchain), got {err:?}",
        );
    }

    #[tokio::test]
    async fn validate_format_passes_stored_packchain_with_no_url_engine() {
        // `FORMAT` already locked to `packchain`; URL omits `?engine=`.
        // Stored value is authoritative; resolution returns it.
        let store = MockStore::new();
        store.insert("FORMAT", Bytes::from_static(b"packchain"));
        assert_eq!(
            validate_format(&store, "", None).await.unwrap(),
            StorageEngine::Packchain,
        );
    }

    #[tokio::test]
    async fn validate_format_passes_stored_packchain_with_matching_url() {
        let store = MockStore::new();
        store.insert("FORMAT", Bytes::from_static(b"packchain"));
        assert_eq!(
            validate_format(&store, "", Some(StorageEngine::Packchain))
                .await
                .unwrap(),
            StorageEngine::Packchain,
        );
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
        // Valid key at the prefixed path.
        store.insert("my-repo/FORMAT", Bytes::from_static(b"bundle"));
        // Conflicting/invalid content at the root path — if the prefix is
        // ignored, the "with prefix" call below would read this and fail.
        // The sentinel value ("INVALID_SENTINEL_NEVER_AN_ENGINE") is
        // structurally impossible to be a valid `StorageEngine` name now
        // or in the future (uppercase, contains underscores), so the
        // assertion holds even if a future engine variant is added.
        store.insert(
            "FORMAT",
            Bytes::from_static(b"INVALID_SENTINEL_NEVER_AN_ENGINE"),
        );
        // Without prefix: reads root FORMAT → must be specifically the
        // `UnknownStoredEngine` variant. A regression that mapped this
        // through `Network` or `InvalidCredentials` would still produce
        // an error but for the wrong reason.
        let err = validate_format(&store, "", None).await.unwrap_err();
        assert!(
            matches!(
                err,
                BackendError::UnknownStoredEngine { ref stored }
                    if stored == "INVALID_SENTINEL_NEVER_AN_ENGINE"
            ),
            "expected UnknownStoredEngine(INVALID_SENTINEL_NEVER_AN_ENGINE), got {err:?}",
        );
        // With prefix "my-repo": reads "my-repo/FORMAT" = "bundle" → Ok.
        validate_format(&store, "my-repo", None).await.unwrap();
    }

    /// T1 tripwire: the `from_utf8` hardening in `validate_format` (vs
    /// the prior `from_utf8_lossy`) must surface non-UTF-8 bytes as
    /// `BackendError::InvalidCredentials` carrying an `io::Error` whose
    /// message names the FORMAT key. A regression that revives
    /// `from_utf8_lossy()` would silently produce a replacement-character
    /// engine name and fail later at `from_name`'s lookup with the wrong
    /// error variant.
    #[tokio::test]
    async fn validate_format_rejects_non_utf8_format_bytes() {
        let store = MockStore::new();
        store.insert("FORMAT", Bytes::from_static(b"\xff\xff\xff"));
        let err = validate_format(&store, "", None).await.unwrap_err();
        let BackendError::InvalidCredentials { source } = &err else {
            panic!("expected InvalidCredentials, got {err:?}");
        };
        let ObjectStoreError::Other(inner) = source else {
            panic!("expected Other inside InvalidCredentials, got {source:?}");
        };
        let msg = inner.to_string();
        // Both substrings together pin the wording the docstring claims:
        // the message must surface the encoding category ("non-UTF-8")
        // AND identify which key carried the bytes ("FORMAT"). Either
        // assertion alone could false-pass on a generic "invalid utf-8"
        // wording or a different-key error.
        assert!(
            msg.contains("non-UTF-8") && msg.contains("FORMAT"),
            "expected message naming the FORMAT key and non-UTF-8 cause, got `{msg}`",
        );
        // The fatal message must surface BOTH the variant prefix
        // ("invalid credentials") AND the inner non-UTF-8 cause through
        // the chain-walk in `fatal_message`. This catches a regression
        // that drops the source level (e.g. by removing `{source}` from
        // the `InvalidCredentials` `#[error(...)]` format).
        let fatal = fatal_message(&err);
        assert!(
            fatal.contains("invalid credentials") && fatal.contains("non-UTF-8"),
            "fatal_message must surface variant + non-UTF-8 source, got `{fatal}`",
        );
    }

    #[test]
    fn unknown_stored_engine_error_message() {
        let err = BackendError::UnknownStoredEngine {
            stored: "pack".into(),
        };
        let fatal = fatal_message(&err);
        assert!(
            fatal.starts_with("fatal: bucket uses unknown storage engine `pack`;"),
            "missing prefix in {fatal}",
        );
        // The supported-engine list is driven by `StorageEngine::ALL`.
        // Asserting against every variant means a new engine that fails
        // to update the diagnostic wording will surface here automatically.
        for engine in StorageEngine::ALL {
            assert!(
                fatal.contains(&format!("`{}`", engine.as_str())),
                "fatal_message for UnknownStoredEngine must mention engine `{}`, got `{fatal}`",
                engine.as_str(),
            );
        }
    }

    #[tokio::test]
    async fn validate_format_returns_network_error_on_transport_failure() {
        use crate::object_store::mock::Fault;
        let store = MockStore::new();
        store.arm(Fault::NetworkOnGetBytes {
            key: "FORMAT".into(),
        });
        let err = validate_format(&store, "", None).await.unwrap_err();
        assert!(
            matches!(err, BackendError::Network { .. }),
            "expected Network, got {err:?}",
        );
    }
}

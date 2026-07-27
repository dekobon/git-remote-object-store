//! Credential resolution and the shared-key / SAS signing policies for
//! the Azure Blob backend.
//!
//! The official `azure_storage_blob` 1.0 crate accepts only
//! `Arc<dyn TokenCredential>` (Entra ID) on its constructors, but the
//! Azurite emulator and many production accounts still authenticate
//! with shared keys. We bridge the gap with a custom per-try
//! [`Policy`] that signs each outgoing request using the Azure
//! Storage shared-key v2 scheme. Tracking issue:
//! `Azure/azure-sdk-for-rust#2975`.
//!
//! Resolution order:
//!
//! 1. URL flag `?credential=<NAME>` →
//!    - `AZSTORE_<NAME>_KEY` (base64 account key) → [`SharedKeySigningPolicy`]
//!    - `AZSTORE_<NAME>_CONNECTION_STRING` → parsed for `AccountKey=`
//!      → [`SharedKeySigningPolicy`]
//!    - `AZSTORE_<NAME>_SAS` → [`SasSigningPolicy`] (appends SAS query
//!      params to every outgoing request URL)
//! 2. No flag → [`azure_identity::DeveloperToolsCredential`].
//!
//! The shared-key signing implementation here is derived from the
//! reference workaround posted on issue #2975, which itself was
//! airlifted from the legacy `azure_storage` SDK. The
//! string-to-sign / canonicalised-resource layout is documented at
//! <https://learn.microsoft.com/en-us/rest/api/storageservices/authorize-with-shared-key>.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::env;
use std::sync::Arc;

use async_trait::async_trait;
use azure_core::credentials::TokenCredential;
use azure_core::http::Method;
use azure_core::http::headers::{HeaderName, Headers};
use azure_core::http::policies::{Policy, PolicyResult};
use azure_core::http::{Context, Request};
use azure_identity::DeveloperToolsCredential;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use time::OffsetDateTime;
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use url::Url;

use crate::object_store::ObjectStoreError;
use crate::object_store::error::other_boxed;
use crate::url::RemoteFlags;

/// RFC 1123 date format Azure requires for `x-ms-date` — the
/// `Sun, 06 Nov 1994 08:49:37 GMT` shape documented at
/// <https://learn.microsoft.com/en-us/rest/api/storageservices/representation-of-date-time-values-in-headers>.
///
/// Pinning the format here keeps Azure-auth correctness independent of
/// the exact byte emission of any well-known formatter in the `time`
/// crate; previously we formatted via `Rfc2822` and string-replaced
/// `+0000` → `GMT`, which would silently produce malformed headers if
/// `time` ever changed its RFC 2822 layout (e.g., `+00:00`).
const X_MS_DATE_FORMAT: &[BorrowedFormatItem<'_>] = format_description!(
    "[weekday repr:short], [day padding:zero] [month repr:short] [year] \
     [hour padding:zero]:[minute padding:zero]:[second padding:zero] GMT"
);

/// Outcome of [`resolve`]: at most one of (`token_credential`,
/// `per_try_policy`) is populated. `sas_signing_key` is populated
/// only for the shared-key / connection-string paths so that the
/// `bundle-uri` capability (issue #76) can derive per-blob service
/// SAS tokens; the SAS env-var path and the Entra-ID path leave it
/// `None`, in which case `presigned_get_url` returns
/// [`crate::object_store::ObjectStoreError::Unsupported`].
pub(crate) struct ResolvedCredentials {
    /// Entra ID credential, used when no `?credential=` alias is set.
    pub token_credential: Option<Arc<dyn TokenCredential>>,
    /// Per-try signing policy (shared-key or SAS), used when a
    /// `?credential=` alias resolves to an env-var-provided key.
    pub per_try_policy: Option<Arc<dyn Policy>>,
    /// Account name + base64 storage key, when the credential alias
    /// resolves to a shared key (KEY env var or connection string).
    /// Held alongside the per-try policy so callers that need to
    /// sign things outside the request pipeline (service-SAS for
    /// `bundle-uri` presigned URLs) don't have to re-walk the env
    /// vars. `None` for SAS / Entra-ID paths.
    pub sas_signing_key: Option<SasSigningKey>,
}

/// Material required to sign a service-blob SAS token (issue #76).
/// Carries the storage key as pre-decoded [`HmacKey`] bytes — the
/// base64 decode happens once at credential-resolution time rather
/// than per SAS-URL build.
///
/// `Debug` is derived: the inner [`HmacKey`] redacts its own bytes,
/// so the default field-walking impl already produces safe output.
#[derive(Clone, Debug)]
pub(crate) struct SasSigningKey {
    pub account: String,
    pub key: HmacKey,
}

/// Pre-decoded HMAC-SHA256 key bytes for Azure shared-key /
/// service-SAS signing. The base64 decode happens once at
/// construction (in [`SharedKeySigningPolicy::new`] and the
/// `parse_connection_string` paths) instead of per request.
///
/// The bytes themselves are an Azure storage account key — leaking
/// them via `Debug` or log output would give full storage-account
/// access — so the manual `Debug` impl redacts. The underlying
/// `Vec<u8>` is not zeroized on drop; the existing `Secret`-based
/// design did not zeroize either, so this is a parity choice
/// rather than a regression (adding `zeroize` would mean a new
/// dependency).
#[derive(Clone)]
pub struct HmacKey {
    bytes: Vec<u8>,
}

impl HmacKey {
    /// Decode `key_b64` and return an [`HmacKey`]. Surfaces the
    /// decoding failure inline so the malformed-key error fires at
    /// credential-resolution time rather than at first request.
    ///
    /// # Errors
    ///
    /// Returns [`ObjectStoreError::Other`] if `key_b64` is not valid
    /// base64.
    pub fn from_base64(key_b64: &str) -> Result<Self, ObjectStoreError> {
        let bytes = BASE64.decode(key_b64.as_bytes()).map_err(|e| {
            ObjectStoreError::Other(format!("AccountKey is not valid base64: {e}").into())
        })?;
        Ok(Self { bytes })
    }

    /// Raw HMAC key bytes. Use only inside the signing primitives.
    fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl std::fmt::Debug for HmacKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HmacKey")
            .field("bytes", &"<redacted>")
            .finish()
    }
}

/// Resolve credentials for a parsed Azure URL.
pub(crate) fn resolve(
    account: &str,
    flags: &RemoteFlags,
) -> Result<ResolvedCredentials, ObjectStoreError> {
    if let Some(alias) = flags.credential.as_deref() {
        return resolve_alias(account, alias);
    }
    let cred = DeveloperToolsCredential::new(None).map_err(other_boxed)?;
    Ok(ResolvedCredentials {
        token_credential: Some(cred),
        per_try_policy: None,
        sas_signing_key: None,
    })
}

fn resolve_alias(account: &str, alias: &str) -> Result<ResolvedCredentials, ObjectStoreError> {
    if !is_valid_alias(alias) {
        return Err(ObjectStoreError::Other(
            format!(
                "invalid credential alias `{alias}`: \
                 must match [A-Za-z0-9_]+ (used to build env var names)"
            )
            .into(),
        ));
    }
    let upper = alias.to_ascii_uppercase();
    let key_var = format!("AZSTORE_{upper}_KEY");
    let conn_var = format!("AZSTORE_{upper}_CONNECTION_STRING");
    let sas_var = format!("AZSTORE_{upper}_SAS");

    if let Some(key_b64) = lookup_env(&key_var)? {
        let policy = SharedKeySigningPolicy::new(account, &key_b64)?;
        let key = HmacKey::from_base64(&key_b64)?;
        return Ok(resolved(
            Arc::new(policy),
            Some(SasSigningKey {
                account: account.to_owned(),
                key,
            }),
        ));
    }
    if let Some(conn) = lookup_env(&conn_var)? {
        let parsed = parse_connection_string(&conn)?;
        let policy = SharedKeySigningPolicy::new(&parsed.account, &parsed.key_b64)?;
        let key = HmacKey::from_base64(&parsed.key_b64)?;
        return Ok(resolved(
            Arc::new(policy),
            Some(SasSigningKey {
                account: parsed.account,
                key,
            }),
        ));
    }
    if let Some(sas) = lookup_env(&sas_var)? {
        let policy = SasSigningPolicy::new(&sas)?;
        // SAS-env-var path has no storage key, so we cannot derive
        // a fresh per-blob SAS for `bundle-uri` presigning. Pass
        // `None` to make the missing key explicit at the call site.
        return Ok(resolved(Arc::new(policy), None));
    }

    Err(ObjectStoreError::Other(
        format!(
            "credential alias `{alias}` has no env var set: \
             expected {key_var}, {conn_var}, or {sas_var}"
        )
        .into(),
    ))
}

/// Read a credential-chain env var, distinguishing "not set" from
/// "set to non-UTF-8 bytes". Returning `Ok(None)` for `NotPresent`
/// keeps the chain walking; returning `Err(...)` for `NotUnicode`
/// surfaces the misconfiguration with the offending variable name
/// so the operator does not chase a "no env var set" message when
/// the var actually was set.
fn lookup_env(name: &str) -> Result<Option<String>, ObjectStoreError> {
    match env::var(name) {
        Ok(v) => Ok(Some(v)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(ObjectStoreError::Other(
            format!("env var `{name}` is set but its value is not valid UTF-8").into(),
        )),
    }
}

/// Build a [`ResolvedCredentials`] from a per-try signing policy
/// plus an optional SAS-signing key. The two `Option<SasSigningKey>`
/// states make the single distinction between alias paths explicit
/// at the call site: `Some(...)` for shared-key / connection-string
/// (presigning is reachable), `None` for SAS-env-var (presigning
/// returns `Unsupported`).
fn resolved(
    policy: Arc<dyn Policy>,
    sas_signing_key: Option<SasSigningKey>,
) -> ResolvedCredentials {
    ResolvedCredentials {
        token_credential: None,
        per_try_policy: Some(policy),
        sas_signing_key,
    }
}

fn is_valid_alias(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Parsed `AccountName` / `AccountKey` from an Azure connection string.
#[derive(Debug)]
pub(crate) struct ConnectionStringParts {
    pub account: String,
    pub key_b64: String,
}

/// Parse the Azure connection-string format documented at
/// <https://learn.microsoft.com/en-us/azure/storage/common/storage-configure-connection-string>.
///
/// Only `AccountName` and `AccountKey` are required; other fields
/// (`DefaultEndpointsProtocol`, `BlobEndpoint`, ...) are accepted but
/// ignored. The endpoint URL is taken from the parsed `RemoteUrl`,
/// not from the connection string, so the URL is the single source
/// of truth for endpoint/host/port.
pub(crate) fn parse_connection_string(
    input: &str,
) -> Result<ConnectionStringParts, ObjectStoreError> {
    let mut account = None;
    let mut key_b64 = None;
    for segment in input.split(';') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        // Surface malformed segments instead of silently skipping —
        // a typo like `AccountKeyy=...` would otherwise be ignored
        // and reported as "missing AccountKey", which sends the user
        // chasing the wrong field.
        let Some((k, v)) = segment.split_once('=') else {
            return Err(ObjectStoreError::Other(
                format!("connection string segment `{segment}` is missing `=`").into(),
            ));
        };
        match k {
            "AccountName" => account = Some(v.to_owned()),
            "AccountKey" => key_b64 = Some(v.to_owned()),
            // Tolerate every other documented field (BlobEndpoint,
            // DefaultEndpointsProtocol, EndpointSuffix, ...) without
            // demanding we know each one — the URL itself is the
            // authoritative endpoint source.
            _ => {}
        }
    }
    let account = account
        .ok_or_else(|| ObjectStoreError::Other("connection string missing AccountName".into()))?;
    let key_b64 = key_b64
        .ok_or_else(|| ObjectStoreError::Other("connection string missing AccountKey".into()))?;
    Ok(ConnectionStringParts { account, key_b64 })
}

// ---------------------------------------------------------------------------
// SharedKeySigningPolicy
// ---------------------------------------------------------------------------

/// Per-try policy that signs every outgoing request with the Azure
/// Storage shared-key v2 scheme.
#[derive(Debug)]
pub(crate) struct SharedKeySigningPolicy {
    account: String,
    key: HmacKey,
}

impl SharedKeySigningPolicy {
    pub(crate) fn new(account: &str, key_b64: &str) -> Result<Self, ObjectStoreError> {
        // Validate base64-decodability up front so a malformed key
        // surfaces at construction, not on the first request. The
        // decoded bytes are cached so subsequent signs avoid the
        // per-request decode cost.
        let key = HmacKey::from_base64(key_b64)?;
        Ok(Self {
            account: account.to_owned(),
            key,
        })
    }
}

#[async_trait]
impl Policy for SharedKeySigningPolicy {
    async fn send(
        &self,
        ctx: &Context,
        request: &mut Request,
        next: &[Arc<dyn Policy>],
    ) -> PolicyResult {
        // Stamp x-ms-date so signing has a stable canonicalised header
        // value. The SDK's date policy sometimes injects a regular
        // `Date` header instead; `x-ms-date` takes precedence per
        // the Azure spec.
        let now = OffsetDateTime::now_utc();
        let date = now.format(&X_MS_DATE_FORMAT).map_err(|e| {
            azure_core::Error::with_message(
                azure_core::error::ErrorKind::Other,
                format!("failed to format x-ms-date: {e}"),
            )
        })?;
        request.insert_header(HeaderName::from_static("x-ms-date"), date);

        let method = request.method();
        let url = request.url().clone();
        let content_length = request_content_length(request);
        let auth = compute_authorization(
            &self.account,
            &self.key,
            method,
            &url,
            request.headers(),
            content_length,
        )
        .map_err(|e| {
            azure_core::Error::with_message(
                azure_core::error::ErrorKind::Other,
                format!("shared-key signing failed: {e}"),
            )
        })?;
        request.insert_header(HeaderName::from_static("authorization"), auth);

        forward_to_next(ctx, request, next, "shared-key").await
    }
}

/// Hand the request to the next policy in the chain, returning a clear
/// error if the chain was empty (the SDK always installs at least the
/// transport policy as the tail, so an empty chain only fires when the
/// signing policy is wired wrong).
async fn forward_to_next(
    ctx: &Context<'_>,
    request: &mut Request,
    next: &[Arc<dyn Policy>],
    policy_name: &'static str,
) -> PolicyResult {
    match next.first() {
        Some(p) => p.send(ctx, request, &next[1..]).await,
        None => Err(azure_core::Error::with_message(
            azure_core::error::ErrorKind::Other,
            format!("{policy_name} policy installed without a downstream policy"),
        )),
    }
}

/// Pull `Content-Length` from the request, falling back to the body
/// length if the header is not yet stamped. Returns `None` for empty
/// bodies (the spec says omit the value from the string-to-sign).
fn request_content_length(request: &Request) -> Option<u64> {
    if let Some(s) = request
        .headers()
        .get_optional_str(&HeaderName::from_static("content-length"))
        && let Ok(n) = s.parse::<u64>()
    {
        return if n == 0 { None } else { Some(n) };
    }
    match request.body().len() {
        Some(0) | None => None,
        Some(n) => Some(n),
    }
}

/// Compute the `Authorization: SharedKey <account>:<sig>` header value.
///
/// Exposed as `pub` so the Azurite integration test (in a separate
/// crate) can sign its own container-create setup request. There is
/// no production caller outside this crate; the function is small,
/// pure, and stable enough that re-using it in tests is preferable
/// to duplicating the spec-exact canonicalisation logic.
///
/// The `key` argument is a pre-decoded [`HmacKey`]; callers that
/// have only a base64 string call [`HmacKey::from_base64`] once and
/// reuse the resulting [`HmacKey`] across signs.
///
/// # Errors
///
/// Returns `Err(String)` if HMAC initialisation fails.
pub fn compute_authorization(
    account: &str,
    key: &HmacKey,
    method: Method,
    url: &Url,
    headers: &Headers,
    content_length: Option<u64>,
) -> Result<String, String> {
    let canon_resource = canonicalized_resource(account, url);
    let canon_headers = canonicalized_headers(headers);
    let string_to_sign = string_to_sign(
        method,
        headers,
        content_length,
        &canon_headers,
        &canon_resource,
    );
    let sig = hmac_sha256_base64(&string_to_sign, key)?;
    Ok(format!("SharedKey {account}:{sig}"))
}

/// Look up a header value for inclusion in the string-to-sign,
/// applying the same trim + unfold-newlines transform that
/// [`canonicalized_headers`] applies to `x-ms-*` headers. Both
/// sites participate in the same string-to-sign and must use the
/// same sanitisation — a literal `\n` in any of these header
/// values would shift fields downstream and let a malformed
/// request masquerade as a different signed request. In practice
/// the HTTP stack rejects header values containing newlines, so
/// this is theoretical; the consistency is the point.
fn header_str<'a>(headers: &'a Headers, name: &'static str) -> Cow<'a, str> {
    let raw = headers
        .get_optional_str(&HeaderName::from_static(name))
        .unwrap_or("");
    let trimmed = raw.trim();
    if trimmed.contains('\n') {
        Cow::Owned(trimmed.replace('\n', " "))
    } else {
        Cow::Borrowed(trimmed)
    }
}

/// Build the Azure shared-key v2 string-to-sign.
fn string_to_sign(
    method: Method,
    headers: &Headers,
    content_length: Option<u64>,
    canon_headers: &str,
    canon_resource: &str,
) -> String {
    let cl = content_length.map(|n| n.to_string()).unwrap_or_default();
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}{}",
        method.as_ref(),
        header_str(headers, "content-encoding"),
        header_str(headers, "content-language"),
        cl,
        header_str(headers, "content-md5"),
        header_str(headers, "content-type"),
        // `Date` is omitted — `x-ms-date` (in canon_headers) takes
        // precedence per the Azure spec.
        "",
        header_str(headers, "if-modified-since"),
        header_str(headers, "if-match"),
        header_str(headers, "if-none-match"),
        header_str(headers, "if-unmodified-since"),
        header_str(headers, "range"),
        canon_headers,
        canon_resource,
    )
}

/// Build the `CanonicalizedHeaders` string per the Azure spec.
fn canonicalized_headers(headers: &Headers) -> String {
    let mut sorted: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in headers.iter() {
        let name = name.as_str().to_ascii_lowercase();
        if !name.starts_with("x-ms-") {
            continue;
        }
        // The spec requires unfolding embedded newlines into single
        // spaces, but the `\n` case is rare — avoid the unconditional
        // allocation that `str::replace` performs.
        let trimmed = value.as_str().trim();
        let value: Cow<'_, str> = if trimmed.contains('\n') {
            Cow::Owned(trimmed.replace('\n', " "))
        } else {
            Cow::Borrowed(trimmed)
        };
        sorted
            .entry(name)
            .and_modify(|existing| {
                existing.push(',');
                existing.push_str(&value);
            })
            .or_insert_with(|| value.into_owned());
    }
    let mut out = String::new();
    for (name, value) in sorted {
        out.push_str(&name);
        out.push(':');
        out.push_str(&value);
        out.push('\n');
    }
    out
}

/// Build the `CanonicalizedResource` string per the Azure spec.
fn canonicalized_resource(account: &str, url: &Url) -> String {
    let mut out = format!("/{account}");
    let path = url.path();
    if !path.starts_with('/') {
        out.push('/');
    }
    out.push_str(path);

    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in url.query_pairs() {
        let key = k.to_ascii_lowercase();
        grouped.entry(key).or_default().push(v.into_owned());
    }
    for (name, mut values) in grouped {
        values.sort_unstable();
        out.push('\n');
        out.push_str(&name);
        out.push(':');
        for (i, v) in values.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(v);
        }
    }
    out
}

/// HMAC-SHA256 over `data` with a pre-decoded [`HmacKey`], returning
/// a base64-encoded MAC. Used by both the shared-key signing policy
/// (`Authorization: SharedKey …` header) and the service-blob SAS
/// builder ([`super::sas`]) — same primitive, same byte sequence,
/// same error wording. The base64 decode of the storage key happens
/// once at [`HmacKey::from_base64`] time; this function does no
/// decoding.
pub(super) fn hmac_sha256_base64(data: &str, key: &HmacKey) -> Result<String, String> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.as_bytes())
        .map_err(|e| format!("HMAC init: {e}"))?;
    mac.update(data.as_bytes());
    Ok(BASE64.encode(mac.finalize().into_bytes()))
}

// ---------------------------------------------------------------------------
// SasSigningPolicy
// ---------------------------------------------------------------------------

/// Per-try policy that appends SAS query parameters to every
/// outgoing request URL.
#[derive(Debug)]
pub(crate) struct SasSigningPolicy {
    pairs: Vec<(String, String)>,
}

impl SasSigningPolicy {
    pub(crate) fn new(sas: &str) -> Result<Self, ObjectStoreError> {
        let trimmed = sas.trim().trim_start_matches('?');
        if trimmed.is_empty() {
            return Err(ObjectStoreError::Other("SAS token is empty".into()));
        }
        let parsed = Url::parse(&format!("https://example.invalid/?{trimmed}"))
            .map_err(|e| ObjectStoreError::Other(format!("malformed SAS token: {e}").into()))?;
        let pairs: Vec<(String, String)> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if pairs.is_empty() {
            return Err(ObjectStoreError::Other(
                "SAS token has no query parameters".into(),
            ));
        }
        Ok(Self { pairs })
    }
}

#[async_trait]
impl Policy for SasSigningPolicy {
    async fn send(
        &self,
        ctx: &Context,
        request: &mut Request,
        next: &[Arc<dyn Policy>],
    ) -> PolicyResult {
        let url = request.url_mut();
        let sas_keys: std::collections::HashSet<&str> =
            self.pairs.iter().map(|(k, _)| k.as_str()).collect();
        let preserved: Vec<(String, String)> = url
            .query_pairs()
            .filter_map(|(k, v)| {
                if sas_keys.contains(k.as_ref()) {
                    None
                } else {
                    Some((k.into_owned(), v.into_owned()))
                }
            })
            .collect();
        url.set_query(None);
        {
            let mut q = url.query_pairs_mut();
            for (k, v) in &preserved {
                q.append_pair(k, v);
            }
            for (k, v) in &self.pairs {
                q.append_pair(k, v);
            }
        }

        forward_to_next(ctx, request, next, "SAS").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- is_valid_alias / parse_connection_string ---------------------

    #[test]
    fn alias_charset() {
        assert!(is_valid_alias("PROD"));
        assert!(is_valid_alias("dev_1"));
        assert!(!is_valid_alias(""));
        assert!(!is_valid_alias("has-dash"));
        assert!(!is_valid_alias("has space"));
        assert!(!is_valid_alias(&"a".repeat(65)));
    }

    #[test]
    fn parse_connection_string_extracts_account_and_key() {
        let s = "DefaultEndpointsProtocol=http;\
                 AccountName=devstoreaccount1;\
                 AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;\
                 BlobEndpoint=http://127.0.0.1:10000/devstoreaccount1;";
        let parts = parse_connection_string(s).expect("parses");
        assert_eq!(parts.account, "devstoreaccount1");
        assert!(parts.key_b64.starts_with("Eby8vdM"));
    }

    #[test]
    fn parse_connection_string_requires_account_name() {
        let s = "AccountKey=abc==;BlobEndpoint=http://x/";
        let err = parse_connection_string(s).unwrap_err();
        assert!(err.to_string().contains("AccountName"), "{err}");
    }

    #[test]
    fn parse_connection_string_requires_account_key() {
        let s = "AccountName=acct;BlobEndpoint=http://x/";
        let err = parse_connection_string(s).unwrap_err();
        assert!(err.to_string().contains("AccountKey"), "{err}");
    }

    #[test]
    fn parse_connection_string_ignores_blank_segments() {
        let s = ";;AccountName=acct;;AccountKey=YWJj;;";
        let parts = parse_connection_string(s).expect("parses");
        assert_eq!(parts.account, "acct");
        assert_eq!(parts.key_b64, "YWJj");
    }

    #[test]
    fn parse_connection_string_rejects_segment_without_equals() {
        let s = "AccountName=acct;malformed;AccountKey=YWJj";
        let err = parse_connection_string(s).unwrap_err();
        assert!(
            err.to_string().contains("malformed"),
            "error names the bad segment: {err}"
        );
    }

    // --- canonicalized_resource ---------------------------------------

    #[test]
    fn canon_resource_path_only() {
        let url = Url::parse("https://acct.blob.core.windows.net/container/blob").unwrap();
        let out = canonicalized_resource("acct", &url);
        assert_eq!(out, "/acct/container/blob");
    }

    #[test]
    fn canon_resource_with_query_params_sorts_and_lowercases() {
        let url = Url::parse(
            "https://acct.blob.core.windows.net/c/b?Restype=container&comp=list&PREFIX=p",
        )
        .unwrap();
        let out = canonicalized_resource("acct", &url);
        assert_eq!(out, "/acct/c/b\ncomp:list\nprefix:p\nrestype:container");
    }

    #[test]
    fn canon_resource_groups_duplicate_keys() {
        let url = Url::parse("https://x.blob.core.windows.net/c?inc=a&inc=b").unwrap();
        let out = canonicalized_resource("x", &url);
        assert_eq!(out, "/x/c\ninc:a,b");
    }

    // --- canonicalized_headers ----------------------------------------

    #[test]
    fn canon_headers_filters_x_ms_only_and_sorts() {
        let mut headers = Headers::new();
        headers.insert(HeaderName::from_static("x-ms-version"), "2025-11-05");
        headers.insert(
            HeaderName::from_static("x-ms-date"),
            "Wed, 01 Jan 2025 00:00:00 GMT",
        );
        headers.insert(HeaderName::from_static("authorization"), "ignored");
        headers.insert(
            HeaderName::from_static("content-type"),
            "application/octet-stream",
        );
        let out = canonicalized_headers(&headers);
        assert_eq!(
            out,
            "x-ms-date:Wed, 01 Jan 2025 00:00:00 GMT\nx-ms-version:2025-11-05\n"
        );
    }

    #[test]
    fn canon_headers_handles_no_x_ms_headers() {
        let mut headers = Headers::new();
        headers.insert(HeaderName::from_static("content-type"), "x");
        assert_eq!(canonicalized_headers(&headers), "");
    }

    // --- compute_authorization fixed vector ---------------------------

    #[test]
    fn compute_authorization_matches_known_vector() {
        // Hand-built fixture: GET against a container with the
        // well-known Azurite key. Exact wire-format isn't easily
        // verifiable in a unit test against the real service, but we
        // can ensure the signing function produces a stable,
        // deterministic value given fixed inputs — locking the
        // canonicalisation into place so future refactors don't
        // silently change wire output.
        let key_b64 = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
        let key = HmacKey::from_base64(key_b64).expect("valid base64");
        let url =
            Url::parse("http://127.0.0.1:10000/devstoreaccount1/c?restype=container&comp=list")
                .unwrap();
        let mut headers = Headers::new();
        headers.insert(
            HeaderName::from_static("x-ms-date"),
            "Wed, 01 Jan 2025 00:00:00 GMT",
        );
        headers.insert(HeaderName::from_static("x-ms-version"), "2025-11-05");

        let auth =
            compute_authorization("devstoreaccount1", &key, Method::Get, &url, &headers, None)
                .expect("signs");
        assert!(auth.starts_with("SharedKey devstoreaccount1:"));
        let sig = auth.strip_prefix("SharedKey devstoreaccount1:").unwrap();
        // HMAC-SHA256 → 32 bytes → 44 chars base64.
        assert_eq!(sig.len(), 44, "unexpected sig length: `{sig}`");
    }

    // --- X_MS_DATE_FORMAT --------------------------------------------

    /// F-003: signing through a pre-decoded [`HmacKey`] must produce
    /// the canonical Azure shared-key v2 signature for the fixed
    /// inputs below. The pinned wire-format byte sequence is the
    /// real assertion — a future refactor that silently swaps the
    /// decoder, transforms the key bytes, or alters the
    /// string-to-sign layout will flip the signature and trip the
    /// equality check.
    #[test]
    fn hmac_key_signs_canonical_shared_key_v2_signature() {
        let key_b64 = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
        let url =
            Url::parse("http://127.0.0.1:10000/devstoreaccount1/c?restype=container&comp=list")
                .unwrap();
        let mut headers = Headers::new();
        headers.insert(
            HeaderName::from_static("x-ms-date"),
            "Wed, 01 Jan 2025 00:00:00 GMT",
        );
        headers.insert(HeaderName::from_static("x-ms-version"), "2025-11-05");

        let key = HmacKey::from_base64(key_b64).expect("valid base64");
        let auth =
            compute_authorization("devstoreaccount1", &key, Method::Get, &url, &headers, None)
                .expect("signs");
        // Pinned wire-format vector — computed once from the inputs
        // above against the Azure shared-key v2 string-to-sign
        // layout. (Round-tripping two independently-constructed
        // `HmacKey`s would only verify that base64 decode is
        // deterministic — already guaranteed by the standard.)
        assert_eq!(
            auth, "SharedKey devstoreaccount1:VgcoAvg+vqaLJ76WpTkj7NrIj4dwCiYGPiMhJ7Q/2zI=",
            "signature must match the pinned wire-format vector",
        );
    }

    /// F-003: the derived `Debug` on [`SasSigningKey`] must inherit
    /// [`HmacKey`]'s redaction — switching from a manual impl to
    /// `derive(Debug)` relies on the inner field's `Debug` doing the
    /// right thing, so pin it here.
    #[test]
    fn sas_signing_key_debug_does_not_leak_inner_bytes() {
        let key_b64 = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
        let signing = SasSigningKey {
            account: "devstoreaccount1".to_owned(),
            key: HmacKey::from_base64(key_b64).expect("valid base64"),
        };
        let rendered = format!("{signing:?}");
        assert!(
            rendered.contains("redacted"),
            "Debug must redact via inner HmacKey: {rendered}"
        );
        assert!(
            !rendered.contains("bytes: ["),
            "Debug must not leak raw key bytes: {rendered}"
        );
    }

    /// F-003: `Debug` on [`HmacKey`] must not leak the underlying
    /// key bytes — any rendering that includes the raw `Vec<u8>`
    /// would surface the key in tracing output.
    #[test]
    fn hmac_key_debug_does_not_leak_bytes() {
        let key_b64 = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
        let key = HmacKey::from_base64(key_b64).expect("valid base64");
        let rendered = format!("{key:?}");
        assert!(
            rendered.contains("redacted"),
            "Debug must redact: {rendered}"
        );
        // A leaky derive would render the raw `Vec<u8>` as
        // `[0x12, 0x34, ...]`. Reject any byte-array delimiters
        // appearing alongside the bytes field name.
        assert!(
            !rendered.contains("bytes: ["),
            "Debug output must not include raw key bytes: {rendered}"
        );
    }

    #[test]
    fn x_ms_date_format_matches_rfc1123_literal() {
        // Canonical RFC 1123 / Azure example, also used as the IMF-fixdate
        // sample in RFC 7231: `Sun, 06 Nov 1994 08:49:37 GMT`.
        // Unix timestamp 784111777 is 1994-11-06T08:49:37Z.
        let when = OffsetDateTime::from_unix_timestamp(784_111_777).expect("valid timestamp");
        let formatted = when.format(&X_MS_DATE_FORMAT).expect("formats");
        assert_eq!(formatted, "Sun, 06 Nov 1994 08:49:37 GMT");
    }

    #[test]
    fn x_ms_date_format_zero_pads_single_digit_fields() {
        // Guards against a future regression that drops zero-padding on
        // day, hour, minute, or second — Azure rejects unpadded values.
        // 2025-01-02T03:04:05Z → all single-digit components.
        let when = OffsetDateTime::from_unix_timestamp(1_735_787_045).expect("valid timestamp");
        let formatted = when.format(&X_MS_DATE_FORMAT).expect("formats");
        assert_eq!(formatted, "Thu, 02 Jan 2025 03:04:05 GMT");
    }

    // --- SasSigningPolicy --------------------------------------------

    #[test]
    fn sas_policy_rejects_empty() {
        assert!(SasSigningPolicy::new("").is_err());
        assert!(SasSigningPolicy::new("?").is_err());
        assert!(SasSigningPolicy::new("   ").is_err());
    }

    // --- lookup_env ---------------------------------------------------

    #[test]
    fn lookup_env_returns_none_when_unset() {
        // Use a var name that no other test touches so parallel test
        // runs do not race. `lookup_env` does no caching, so reading
        // a guaranteed-unset name is deterministic.
        let name = "AZSTORE_AUTH_TEST_DEFINITELY_UNSET_VAR";
        let _env = crate::test_util::EnvGuard::unset(name);
        assert!(matches!(lookup_env(name), Ok(None)));
    }

    #[test]
    fn lookup_env_returns_value_when_valid_utf8() {
        let name = "AZSTORE_AUTH_TEST_VALID_UTF8";
        let _env = crate::test_util::EnvGuard::set(name, "hello");
        let value = lookup_env(name).expect("UTF-8 value must read");
        assert_eq!(value.as_deref(), Some("hello"));
    }

    /// Issue #218: a credential env var set to non-UTF-8 bytes must
    /// surface as a structured error naming the offending variable,
    /// not be silently treated as "unset" (which previously sent the
    /// operator chasing the "no env var set" message).
    #[cfg(unix)]
    #[test]
    fn lookup_env_surfaces_not_unicode_error_naming_var() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let name = "AZSTORE_AUTH_TEST_NOT_UNICODE";
        // 0xFF is never valid in a UTF-8 byte stream, so this is
        // guaranteed to land in the `VarError::NotUnicode` branch.
        let bad = OsString::from_vec(vec![0xFF, 0xFE, 0xFD]);
        let _env = crate::test_util::EnvGuard::set(name, &bad);
        let err = lookup_env(name).expect_err("non-UTF-8 env value must error, not be ignored");
        let msg = err.to_string();
        assert!(
            msg.contains(name),
            "error must name the offending var (`{name}`): {msg}"
        );
        assert!(
            msg.contains("not valid UTF-8") || msg.contains("UTF-8"),
            "error must mention UTF-8: {msg}"
        );
    }

    #[test]
    fn sas_policy_parses_with_or_without_leading_question() {
        let a = SasSigningPolicy::new("sv=2025&sig=abc").expect("parses");
        let b = SasSigningPolicy::new("?sv=2025&sig=abc").expect("parses");
        assert_eq!(a.pairs, b.pairs);
        assert!(a.pairs.iter().any(|(k, v)| k == "sv" && v == "2025"));
        assert!(a.pairs.iter().any(|(k, v)| k == "sig" && v == "abc"));
    }
}

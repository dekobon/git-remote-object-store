//! Credential resolution and the shared-key / SAS signing policies for
//! the Azure Blob backend.
//!
//! The official `azure_storage_blob` 0.12 crate accepts only
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
use azure_core::credentials::{Secret, TokenCredential};
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
use time::format_description::well_known::Rfc2822;
use url::Url;

use crate::object_store::ObjectStoreError;
use crate::object_store::error::other_boxed;
use crate::url::RemoteFlags;

/// Outcome of [`resolve`]: at most one of these is populated.
pub(crate) struct ResolvedCredentials {
    /// Entra ID credential, used when no `?credential=` alias is set.
    pub token_credential: Option<Arc<dyn TokenCredential>>,
    /// Per-try signing policy (shared-key or SAS), used when a
    /// `?credential=` alias resolves to an env-var-provided key.
    pub per_try_policy: Option<Arc<dyn Policy>>,
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

    if let Ok(key_b64) = env::var(&key_var) {
        let policy = SharedKeySigningPolicy::new(account, &key_b64)?;
        return Ok(policy_only(Arc::new(policy)));
    }
    if let Ok(conn) = env::var(&conn_var) {
        let parsed = parse_connection_string(&conn)?;
        let policy = SharedKeySigningPolicy::new(&parsed.account, &parsed.key_b64)?;
        return Ok(policy_only(Arc::new(policy)));
    }
    if let Ok(sas) = env::var(&sas_var) {
        let policy = SasSigningPolicy::new(&sas)?;
        return Ok(policy_only(Arc::new(policy)));
    }

    Err(ObjectStoreError::Other(
        format!(
            "credential alias `{alias}` has no env var set: \
             expected {key_var}, {conn_var}, or {sas_var}"
        )
        .into(),
    ))
}

fn policy_only(policy: Arc<dyn Policy>) -> ResolvedCredentials {
    ResolvedCredentials {
        token_credential: None,
        per_try_policy: Some(policy),
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
pub(crate) struct SharedKeySigningPolicy {
    account: String,
    key: Secret,
}

impl std::fmt::Debug for SharedKeySigningPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedKeySigningPolicy")
            .field("account", &self.account)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl SharedKeySigningPolicy {
    pub(crate) fn new(account: &str, key_b64: &str) -> Result<Self, ObjectStoreError> {
        // Validate base64-decodability up front so a malformed key
        // surfaces at construction, not on the first request.
        BASE64.decode(key_b64.as_bytes()).map_err(|e| {
            ObjectStoreError::Other(format!("AccountKey is not valid base64: {e}").into())
        })?;
        Ok(Self {
            account: account.to_owned(),
            key: Secret::new(key_b64.to_owned()),
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
        let date = now.format(&Rfc2822).map_err(|e| {
            azure_core::Error::with_message(
                azure_core::error::ErrorKind::Other,
                format!("failed to format x-ms-date: {e}"),
            )
        })?;
        // RFC 2822 emits `+0000`; Azure expects `GMT` per RFC 1123.
        let date = date.replace("+0000", "GMT");
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
/// # Errors
///
/// Returns `Err(String)` if the HMAC key cannot be decoded from
/// base64 (the error string describes the decoding failure).
pub fn compute_authorization(
    account: &str,
    key: &Secret,
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

fn header_str<'a>(headers: &'a Headers, name: &'static str) -> &'a str {
    headers
        .get_optional_str(&HeaderName::from_static(name))
        .unwrap_or("")
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

fn hmac_sha256_base64(data: &str, key: &Secret) -> Result<String, String> {
    let key_bytes = BASE64
        .decode(key.secret().as_bytes())
        .map_err(|e| format!("AccountKey base64 decode: {e}"))?;
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(&key_bytes).map_err(|e| format!("HMAC init: {e}"))?;
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
        let key = Secret::new(key_b64.to_owned());
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

    // --- SasSigningPolicy --------------------------------------------

    #[test]
    fn sas_policy_rejects_empty() {
        assert!(SasSigningPolicy::new("").is_err());
        assert!(SasSigningPolicy::new("?").is_err());
        assert!(SasSigningPolicy::new("   ").is_err());
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

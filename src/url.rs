//! Parser for the `s3+https` / `s3+http` / `az+https` / `az+http` URL
//! grammar described in §3 of `execution-plan.md`.
//!
//! The parser strips the backend prefix (`s3+` or `az+`), parses the
//! remainder as an RFC 3986 URL via the [`url`] crate, then layers
//! cleartext-HTTP gating (§3.5), backend-specific name validation,
//! addressing-style detection (§3.4), and query-flag extraction on top.

use std::env;
use std::fmt;
use std::str::FromStr;

use thiserror::Error;
use url::Url;

/// Environment override that allows cleartext `*+http://` URLs against
/// non-loopback hosts. Per §3.5 this is accepted only when set to `1`.
pub const ENV_ALLOW_HTTP: &str = "GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP";

/// A parsed remote URL.
///
/// The `endpoint` field holds the canonical `https://` or `http://`
/// URL that remains after stripping the backend prefix; bucket /
/// account / container / prefix are projections of that URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteUrl {
    /// Amazon S3 (or any S3-compatible) endpoint.
    S3 {
        /// Canonical RFC 3986 endpoint URL (the input minus `s3+`).
        endpoint: Url,
        /// Bucket name.
        bucket: String,
        /// Optional repository prefix within the bucket (no trailing `/`).
        prefix: Option<String>,
        /// Auto-detected or explicitly overridden addressing style.
        addressing: S3Addressing,
        /// Query-string flags.
        flags: RemoteFlags,
    },
    /// Azure Blob Storage endpoint.
    Azure {
        /// Canonical RFC 3986 endpoint URL (the input minus `az+`).
        endpoint: Url,
        /// Storage-account name.
        account: String,
        /// Container name.
        container: String,
        /// Optional repository prefix within the container (no trailing `/`).
        prefix: Option<String>,
        /// Auto-detected or explicitly overridden addressing style.
        addressing: AzureAddressing,
        /// Query-string flags.
        flags: RemoteFlags,
    },
}

/// S3 addressing style (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum S3Addressing {
    /// `<bucket>.s3.<region>.amazonaws.com` — bucket is the leftmost
    /// hostname label.
    VirtualHosted,
    /// `s3.<region>.amazonaws.com/<bucket>` — bucket is the first path
    /// segment.
    PathStyle,
}

/// Azure Blob addressing style (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AzureAddressing {
    /// `<account>.blob.<endpoint-suffix>` — account is the leftmost
    /// hostname label.
    Subdomain,
    /// `<host>/<account>/...` — account is the first path segment
    /// (Azurite, custom endpoints).
    PathStyle,
}

/// Query-string flags described in §3.2 / §3.3.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteFlags {
    /// `?zip=1` — push uploads `repo.zip` alongside each bundle.
    pub zip: bool,
    /// `?profile=...` — selects a named AWS profile (S3 only).
    pub profile: Option<String>,
    /// `?credential=...` — names an Azure credential alias.
    pub credential: Option<String>,
    /// `?region=...` — overrides the SDK-derived region (rare).
    pub region: Option<String>,
}

/// Errors produced by [`parse`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    /// Input was empty or whitespace-only.
    #[error("empty URL")]
    Empty,
    /// Scheme is not one of the four accepted values.
    #[error("unsupported scheme `{0}`; expected `s3+https`, `s3+http`, `az+https`, or `az+http`")]
    UnsupportedScheme(String),
    /// The body after the backend prefix could not be parsed as a URL.
    #[error("malformed URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    /// URL is missing a host component.
    #[error("URL is missing a host")]
    MissingHost,
    /// S3 path-style URL is missing the first path segment (the bucket).
    #[error("URL is missing the bucket segment")]
    MissingBucket,
    /// Azure subdomain URL is missing the first path segment (the
    /// container) — or path-style is missing the second path segment.
    #[error("URL is missing the container segment")]
    MissingContainer,
    /// Azure path-style URL is missing the first path segment (the
    /// account).
    #[error("URL is missing the account segment")]
    MissingAccount,
    /// Bucket name does not match the S3 charset rules in §3.5.
    #[error("invalid bucket name `{0}`")]
    InvalidBucket(String),
    /// Storage-account name does not match the Azure rules in §3.5.
    #[error("invalid storage-account name `{0}`")]
    InvalidAccount(String),
    /// Container name does not match the Azure rules in §3.5.
    #[error("invalid container name `{0}`")]
    InvalidContainer(String),
    /// Cleartext `*+http://` against a non-loopback host without the
    /// override env var.
    #[error(
        "cleartext http:// is forbidden against non-loopback host `{host}`; \
         set {ENV_ALLOW_HTTP}=1 to override"
    )]
    CleartextHttpForbidden {
        /// The non-loopback host that triggered the rejection.
        host: String,
    },
    /// `?addressing=` value other than `path` or `virtual`.
    #[error("unknown addressing override `{0}`; expected `path` or `virtual`")]
    UnknownAddressing(String),
    /// A known flag had a value outside its accepted set.
    #[error("invalid value for flag `{name}`: `{value}`")]
    InvalidFlagValue {
        /// Flag name.
        name: String,
        /// Offending value.
        value: String,
    },
    /// A query parameter is not part of the documented flag set.
    #[error("unknown query flag `{0}`")]
    UnknownFlag(String),
}

/// Parse a remote URL.
pub fn parse(input: &str) -> Result<RemoteUrl, ParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ParseError::Empty);
    }

    let backend = detect_backend(trimmed)?;
    let body = trimmed
        .strip_prefix(backend.scheme_prefix())
        .ok_or_else(|| ParseError::UnsupportedScheme(scheme_of(trimmed)))?;
    let endpoint = Url::parse(body)?;

    let host = endpoint.host_str().ok_or(ParseError::MissingHost)?;
    if endpoint.scheme() == "http" && !is_loopback(&endpoint) && !http_allowed_by_env() {
        return Err(ParseError::CleartextHttpForbidden {
            host: host.to_owned(),
        });
    }

    let (flags, addressing_override) = extract_flags(&endpoint)?;

    match backend {
        Backend::S3 => finish_s3(endpoint, flags, addressing_override),
        Backend::Azure => finish_azure(endpoint, flags, addressing_override),
    }
}

impl FromStr for RemoteUrl {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        parse(s)
    }
}

impl fmt::Display for RemoteUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteUrl::S3 { endpoint, .. } => write!(f, "s3+{endpoint}"),
            RemoteUrl::Azure { endpoint, .. } => write!(f, "az+{endpoint}"),
        }
    }
}

impl RemoteUrl {
    /// Returns the canonical endpoint URL (without the backend prefix).
    #[must_use]
    pub fn endpoint(&self) -> &Url {
        match self {
            RemoteUrl::S3 { endpoint, .. } | RemoteUrl::Azure { endpoint, .. } => endpoint,
        }
    }

    /// Returns the optional repository prefix.
    #[must_use]
    pub fn prefix(&self) -> Option<&str> {
        match self {
            RemoteUrl::S3 { prefix, .. } | RemoteUrl::Azure { prefix, .. } => prefix.as_deref(),
        }
    }

    /// Returns the parsed query flags.
    #[must_use]
    pub fn flags(&self) -> &RemoteFlags {
        match self {
            RemoteUrl::S3 { flags, .. } | RemoteUrl::Azure { flags, .. } => flags,
        }
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    S3,
    Azure,
}

impl Backend {
    fn scheme_prefix(self) -> &'static str {
        match self {
            Backend::S3 => "s3+",
            Backend::Azure => "az+",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddressingOverride {
    Path,
    Virtual,
}

fn detect_backend(input: &str) -> Result<Backend, ParseError> {
    if input.starts_with("s3+https://") || input.starts_with("s3+http://") {
        Ok(Backend::S3)
    } else if input.starts_with("az+https://") || input.starts_with("az+http://") {
        Ok(Backend::Azure)
    } else {
        Err(ParseError::UnsupportedScheme(scheme_of(input)))
    }
}

/// Extract the part of `input` before the first `:` for error messages.
/// Falls back to the whole string when no `:` is present.
fn scheme_of(input: &str) -> String {
    input.split(':').next().unwrap_or(input).to_owned()
}

fn is_loopback(u: &Url) -> bool {
    match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

fn http_allowed_by_env() -> bool {
    matches!(env::var(ENV_ALLOW_HTTP).as_deref(), Ok("1"))
}

/// Pull known flags out of the query string. Unknown keys are an error
/// (fail-fast on typos rather than silently discard configuration).
fn extract_flags(u: &Url) -> Result<(RemoteFlags, Option<AddressingOverride>), ParseError> {
    let mut flags = RemoteFlags::default();
    let mut addressing = None;
    for (key, value) in u.query_pairs() {
        match key.as_ref() {
            "zip" => flags.zip = parse_bool_flag("zip", value.as_ref())?,
            "profile" => flags.profile = Some(value.into_owned()),
            "credential" => flags.credential = Some(value.into_owned()),
            "region" => flags.region = Some(value.into_owned()),
            "addressing" => {
                addressing = Some(match value.as_ref() {
                    "path" => AddressingOverride::Path,
                    "virtual" => AddressingOverride::Virtual,
                    other => return Err(ParseError::UnknownAddressing(other.to_owned())),
                });
            }
            other => return Err(ParseError::UnknownFlag(other.to_owned())),
        }
    }
    Ok((flags, addressing))
}

fn parse_bool_flag(name: &str, value: &str) -> Result<bool, ParseError> {
    match value {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        other => Err(ParseError::InvalidFlagValue {
            name: name.to_owned(),
            value: other.to_owned(),
        }),
    }
}

/// Non-empty path segments. Segments are returned verbatim; bucket /
/// account / container charsets cannot contain percent-encoded bytes,
/// and the prefix is round-tripped as-stored.
fn path_segments(u: &Url) -> Vec<String> {
    u.path_segments()
        .map(|iter| iter.filter(|s| !s.is_empty()).map(str::to_owned).collect())
        .unwrap_or_default()
}

fn join_prefix(segments: &[String]) -> Option<String> {
    if segments.is_empty() {
        None
    } else {
        Some(segments.join("/"))
    }
}

/// Set the URL's path so that [`fmt::Display`] reproduces the canonical
/// form (with trailing `/` stripped).
fn set_canonical_path(u: &mut Url, segments: &[&str]) {
    u.set_path(&format!("/{}", segments.join("/")));
}

// ---------------------------------------------------------------------------
// S3
// ---------------------------------------------------------------------------

fn finish_s3(
    mut endpoint: Url,
    flags: RemoteFlags,
    addressing_override: Option<AddressingOverride>,
) -> Result<RemoteUrl, ParseError> {
    let segments = path_segments(&endpoint);
    let host = endpoint
        .host_str()
        .ok_or(ParseError::MissingHost)?
        .to_owned();
    let addressing = match addressing_override {
        Some(AddressingOverride::Path) => S3Addressing::PathStyle,
        Some(AddressingOverride::Virtual) => S3Addressing::VirtualHosted,
        None => detect_s3_addressing(&host),
    };

    let (bucket, prefix_segments) = match addressing {
        S3Addressing::VirtualHosted => {
            let bucket = leftmost_label(&host).ok_or(ParseError::MissingBucket)?;
            (bucket, segments.as_slice())
        }
        S3Addressing::PathStyle => {
            let (head, tail) = segments.split_first().ok_or(ParseError::MissingBucket)?;
            (head.clone(), tail)
        }
    };

    if !is_valid_bucket(&bucket) {
        return Err(ParseError::InvalidBucket(bucket));
    }
    let prefix = join_prefix(prefix_segments);

    // Re-emit a canonical path so Display round-trips cleanly.
    let canonical: Vec<&str> = match addressing {
        S3Addressing::VirtualHosted => prefix_segments.iter().map(String::as_str).collect(),
        S3Addressing::PathStyle => std::iter::once(bucket.as_str())
            .chain(prefix_segments.iter().map(String::as_str))
            .collect(),
    };
    set_canonical_path(&mut endpoint, &canonical);

    Ok(RemoteUrl::S3 {
        endpoint,
        bucket,
        prefix,
        addressing,
        flags,
    })
}

fn detect_s3_addressing(host: &str) -> S3Addressing {
    // §3.4: virtual-hosted iff the second hostname label is `s3`.
    // Otherwise default to path-style; the `?addressing=` override is
    // available for S3-compatible endpoints that follow a different
    // virtual-hosted convention. Hosts are already lowercased by the
    // `url` crate (RFC 3986), so direct comparison is sufficient.
    if host.split('.').nth(1) == Some("s3") {
        S3Addressing::VirtualHosted
    } else {
        S3Addressing::PathStyle
    }
}

fn leftmost_label(host: &str) -> Option<String> {
    host.split('.')
        .next()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Azure
// ---------------------------------------------------------------------------

fn finish_azure(
    mut endpoint: Url,
    flags: RemoteFlags,
    addressing_override: Option<AddressingOverride>,
) -> Result<RemoteUrl, ParseError> {
    let segments = path_segments(&endpoint);
    let host = endpoint
        .host_str()
        .ok_or(ParseError::MissingHost)?
        .to_owned();
    let addressing = match addressing_override {
        Some(AddressingOverride::Path) => AzureAddressing::PathStyle,
        Some(AddressingOverride::Virtual) => AzureAddressing::Subdomain,
        None => detect_azure_addressing(&host),
    };

    let (account, container, prefix_segments) = match addressing {
        AzureAddressing::Subdomain => {
            let account = leftmost_label(&host).ok_or(ParseError::MissingAccount)?;
            match segments.as_slice() {
                [] => return Err(ParseError::MissingContainer),
                [container, rest @ ..] => (account, container.clone(), rest),
            }
        }
        AzureAddressing::PathStyle => match segments.as_slice() {
            [] => return Err(ParseError::MissingAccount),
            [_] => return Err(ParseError::MissingContainer),
            [account, container, rest @ ..] => (account.clone(), container.clone(), rest),
        },
    };

    if !is_valid_account(&account) {
        return Err(ParseError::InvalidAccount(account));
    }
    if !is_valid_container(&container) {
        return Err(ParseError::InvalidContainer(container));
    }
    let prefix = join_prefix(prefix_segments);

    let canonical: Vec<&str> = match addressing {
        AzureAddressing::Subdomain => std::iter::once(container.as_str())
            .chain(prefix_segments.iter().map(String::as_str))
            .collect(),
        AzureAddressing::PathStyle => std::iter::once(account.as_str())
            .chain(std::iter::once(container.as_str()))
            .chain(prefix_segments.iter().map(String::as_str))
            .collect(),
    };
    set_canonical_path(&mut endpoint, &canonical);

    Ok(RemoteUrl::Azure {
        endpoint,
        account,
        container,
        prefix,
        addressing,
        flags,
    })
}

fn detect_azure_addressing(host: &str) -> AzureAddressing {
    // §3.4: subdomain iff the second hostname label is `blob`. Hosts
    // are already lowercased by the `url` crate (RFC 3986).
    if host.split('.').nth(1) == Some("blob") {
        AzureAddressing::Subdomain
    } else {
        AzureAddressing::PathStyle
    }
}

// ---------------------------------------------------------------------------
// Validation (§3.5)
// ---------------------------------------------------------------------------

/// `[a-z0-9][a-z0-9.\-]{2,62}` — total length 3..=63.
fn is_valid_bucket(s: &str) -> bool {
    let bytes = s.as_bytes();
    if !(3..=63).contains(&bytes.len()) {
        return false;
    }
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };
    let first_ok = first.is_ascii_lowercase() || first.is_ascii_digit();
    let rest_ok = rest
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'.' || *b == b'-');
    first_ok && rest_ok
}

/// `[a-z0-9]{3,24}`.
fn is_valid_account(s: &str) -> bool {
    let bytes = s.as_bytes();
    if !(3..=24).contains(&bytes.len()) {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// `[a-z0-9-]{3,63}`.
fn is_valid_container(s: &str) -> bool {
    let bytes = s.as_bytes();
    if !(3..=63).contains(&bytes.len()) {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty() {
        assert_eq!(parse(""), Err(ParseError::Empty));
        assert_eq!(parse("   "), Err(ParseError::Empty));
    }

    #[test]
    fn rejects_unknown_scheme() {
        let err = parse("https://example.com/bucket").unwrap_err();
        assert!(matches!(err, ParseError::UnsupportedScheme(s) if s == "https"));
    }

    #[test]
    fn validates_bucket_charset() {
        assert!(is_valid_bucket("my-bucket"));
        assert!(is_valid_bucket("a23"));
        assert!(!is_valid_bucket("ab"));
        assert!(!is_valid_bucket("-leading-dash"));
        assert!(!is_valid_bucket("UPPER"));
        assert!(!is_valid_bucket(&"a".repeat(64)));
    }

    #[test]
    fn validates_account_charset() {
        assert!(is_valid_account("myacct1"));
        assert!(!is_valid_account("ab"));
        assert!(!is_valid_account("has-hyphen"));
        assert!(!is_valid_account(&"a".repeat(25)));
    }

    #[test]
    fn validates_container_charset() {
        assert!(is_valid_container("my-container"));
        assert!(!is_valid_container("ab"));
        assert!(!is_valid_container("UPPER"));
        assert!(!is_valid_container(&"a".repeat(64)));
    }

    #[test]
    fn s3_addressing_heuristic() {
        assert_eq!(
            detect_s3_addressing("my-bucket.s3.us-west-2.amazonaws.com"),
            S3Addressing::VirtualHosted
        );
        assert_eq!(
            detect_s3_addressing("s3.us-west-2.amazonaws.com"),
            S3Addressing::PathStyle
        );
        assert_eq!(
            detect_s3_addressing("acc.r2.cloudflarestorage.com"),
            S3Addressing::PathStyle
        );
    }

    #[test]
    fn azure_addressing_heuristic() {
        assert_eq!(
            detect_azure_addressing("my-account.blob.core.windows.net"),
            AzureAddressing::Subdomain
        );
        assert_eq!(
            detect_azure_addressing("127.0.0.1"),
            AzureAddressing::PathStyle
        );
    }
}

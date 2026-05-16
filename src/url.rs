//! Parser for the `s3+https` / `s3+http` / `az+https` / `az+http` URL
//! grammar.
//!
//! The parser strips the backend prefix (`s3+` or `az+`), parses the
//! remainder as an RFC 3986 URL via the [`url`] crate, then layers
//! cleartext-HTTP gating, backend-specific name validation,
//! addressing-style detection, and query-flag extraction on top. The
//! user-facing grammar reference is `docs/getting-started.md`.

use std::env;
use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;

use thiserror::Error;
use url::Url;

/// Environment override that allows cleartext `*+http://` URLs against
/// non-loopback hosts. Accepted only when set to `1`.
pub const ENV_ALLOW_HTTP: &str = "GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP";

/// Maximum accepted value for `?bundle_uri_presign_ttl=<seconds>`: 7
/// days, in seconds. Pinned at the URL boundary so the value cannot
/// reach the backend SDKs as a degenerate input.
///
/// AWS enforces a 7-day ceiling on presigned URLs as part of the
/// `SigV4` specification; passing anything larger to
/// `aws_sdk_s3::presigning::PresigningConfig::expires_in` fails with
/// `expires_in must be less than or equal to 604800 seconds`. Azure
/// service-SAS does not have a comparable spec-mandated cap, but a
/// pathological caller-supplied TTL (e.g. `u64::MAX`) caused a panic
/// in [`crate::object_store::azure::sas::build_blob_sas_url`] via
/// `time::Duration::seconds_f64` overflow. Applying the same 7-day
/// cap to both backends gives consistent behaviour and a clean error
/// at URL-parse time rather than mid-protocol (issue #219).
pub const MAX_BUNDLE_URI_PRESIGN_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;

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
    /// hostname label. Named `VirtualHosted` for symmetry with
    /// [`S3Addressing::VirtualHosted`]; both describe the
    /// "leftmost-hostname-label" pattern.
    VirtualHosted,
    /// `<host>/<account>/...` — account is the first path segment
    /// (Azurite, custom endpoints).
    PathStyle,
}

/// Identifies the on-bucket storage format / serialisation engine.
///
/// `engine` is a bucket-level property: once written to the `FORMAT` key on
/// the first push, it is validated on every subsequent connect. The
/// `?engine=` URL parameter is advisory — it is only meaningful when
/// initialising a new repository. After the first push the stored value is
/// authoritative and the URL parameter is checked for conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageEngine {
    /// Git bundle v2 — a text header followed by a PACK file.
    ///
    /// Key layout: `<prefix>/refs/heads/<branch>/<sha>.bundle`.
    Bundle,
    /// Incremental pack-chain engine (issue #52).
    ///
    /// On-bucket layout: `chain.json` (newest-first manifest) plus
    /// `path-index.json` per ref, with content-addressed packs at
    /// `<prefix>/packs/<sha>.{pack,idx}` and a baseline bundle for
    /// first-push fan-out. Push, fetch, direct file access (`read_blob`
    /// library API), compaction, and GC are all implemented; see
    /// `src/packchain/{push,fetch,read,compact,gc}.rs`.
    Packchain,
}

impl StorageEngine {
    /// Every storage engine this client recognises.
    ///
    /// Single source of truth for diagnostics that need to enumerate
    /// the supported set (see [`Self::supported_list_str`]). When a new
    /// variant is added, append it here and every diagnostic that drives
    /// its wording from this list updates automatically.
    pub(crate) const ALL: &'static [Self] = &[Self::Bundle, Self::Packchain];

    /// Parse an engine from its canonical string name. Returns `None` for
    /// unrecognised names.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|engine| engine.as_str() == name)
    }

    /// The canonical name for this engine, as stored in the `FORMAT` key and
    /// accepted in the `?engine=` URL parameter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bundle => "bundle",
            Self::Packchain => "packchain",
        }
    }

    /// Human-readable comma-separated list of every supported engine name,
    /// each wrapped in backticks (e.g. `` "`bundle`, `packchain`" ``).
    ///
    /// Used by [`ParseError::UnknownEngine`] and
    /// [`crate::protocol::backend::BackendError::UnknownStoredEngine`] so
    /// that diagnostics stay in sync with [`Self::ALL`].
    #[must_use]
    pub(crate) fn supported_list_str() -> String {
        Self::ALL
            .iter()
            .map(|engine| format!("`{}`", engine.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl fmt::Display for StorageEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which backend a URL (or error) refers to.
///
/// Used as a discriminant in [`crate::protocol::backend::BackendError`] to select
/// S3 vs Azure error wording, and internally in `url::parse` to route the
/// URL to the right parsing path.
///
/// Marked `#[non_exhaustive]` so adding a new backend (e.g. GCS) is not
/// a breaking change for downstream `match` arms — they will see a
/// compiler error reminding them to handle the new variant via an
/// explicit wildcard branch rather than silently picking up the wrong
/// behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendKind {
    /// Amazon S3 (or any S3-compatible) backend.
    S3,
    /// Azure Blob Storage backend.
    Azure,
}

impl BackendKind {
    /// The URL scheme prefix for this backend (`"s3+"` or `"az+"`).
    pub(crate) const fn scheme_prefix(self) -> &'static str {
        match self {
            Self::S3 => "s3+",
            Self::Azure => "az+",
        }
    }
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
    /// `?engine=...` — declares the storage engine for a new repository.
    ///
    /// On the first push to an empty bucket this value is written to the
    /// `FORMAT` key. On subsequent connects the stored `FORMAT` value is
    /// authoritative; a conflicting `?engine=` aborts with an error.
    pub engine: Option<StorageEngine>,
    /// `?bundle_uri=1` — opt in to advertising the `bundle-uri` helper
    /// capability so a `git clone` can fetch the packchain baseline
    /// bundle directly (e.g. via a public bucket or CDN) before the
    /// helper protocol negotiates the incremental tail. Only meaningful
    /// for `?engine=packchain` remotes; bundle-engine remotes ignore
    /// the flag because their bundle filenames rotate per push and a
    /// stable URL would race the next push.
    pub bundle_uri: bool,
    /// `?bundle_uri_presign_ttl=<seconds>` — when set on a packchain
    /// remote with `?bundle_uri=1`, the helper presigns each emitted
    /// `bundle.<ref>.uri=<url>` line with an `<seconds>`-TTL signed
    /// URL (S3 `SigV4` or Azure service-SAS). Operators with private
    /// buckets need this; public-read buckets and CDN-fronted
    /// endpoints can leave it unset (the canonical URL works
    /// directly).
    ///
    /// `NonZeroU64` because a zero-second TTL is meaningless (the URL
    /// would expire before any client could observe it). The URL
    /// parser rejects `=0` at the boundary with [`ParseError::InvalidFlagValue`].
    /// Issue #76.
    pub bundle_uri_presign_ttl: Option<NonZeroU64>,
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
    /// Azure virtual-hosted URL is missing the first path segment (the
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
    /// `?engine=` value is not a recognised engine name.
    #[error(
        "unknown engine `{0}`; expected one of {supported}",
        supported = StorageEngine::supported_list_str()
    )]
    UnknownEngine(String),
    /// An `amazonaws.com` hostname that cannot be a valid S3 endpoint.
    ///
    /// Valid patterns are:
    /// - virtual-hosted: `<bucket>.s3[.<region>].amazonaws.com`
    /// - path-style: `s3[.<region>|-<region>].amazonaws.com`
    #[error(
        "hostname `{host}` is not a recognized AWS S3 endpoint; \
         for virtual-hosted use `<bucket>.s3[.<region>].amazonaws.com`, \
         for path-style use `s3[.<region>|-<region>].amazonaws.com`"
    )]
    InvalidAwsS3Endpoint {
        /// The offending hostname.
        host: String,
    },
    /// `?bundle_uri_presign_ttl=<seconds>` exceeded
    /// [`MAX_BUNDLE_URI_PRESIGN_TTL_SECONDS`] (7 days). Rejecting at
    /// the URL boundary prevents a degenerate value from reaching the
    /// AWS SDK (which rejects > 7 days anyway) or the Azure SAS
    /// builder (which previously panicked on `u64::MAX`). Issue #219.
    #[error(
        "bundle_uri_presign_ttl=`{value}` exceeds the 7-day maximum \
         ({max} seconds); presigned URLs cannot be valid for longer"
    )]
    BundleUriPresignTtlTooLarge {
        /// The offending value.
        value: u64,
        /// The maximum accepted value
        /// ([`MAX_BUNDLE_URI_PRESIGN_TTL_SECONDS`]).
        max: u64,
    },
}

/// Parse a remote URL.
///
/// # Errors
///
/// Returns [`ParseError`] if the input is empty, uses an unsupported
/// scheme, contains a malformed URL, is missing required components
/// (host, bucket, container, account), contains invalid component names,
/// uses an `amazonaws.com` hostname that does not match a known S3
/// endpoint pattern, or uses cleartext `http://` against a non-loopback
/// host without the [`ENV_ALLOW_HTTP`] environment override.
pub fn parse(input: &str) -> Result<RemoteUrl, ParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ParseError::Empty);
    }

    let (backend, body) = detect_backend(trimmed)?;
    let endpoint = Url::parse(body)?;

    let host = endpoint
        .host_str()
        .ok_or(ParseError::MissingHost)?
        .to_owned();
    if endpoint.scheme() == "http" && !is_loopback(&endpoint) && !http_allowed_by_env() {
        return Err(ParseError::CleartextHttpForbidden { host });
    }

    let (flags, addressing_override) = extract_flags(&endpoint)?;

    match backend {
        BackendKind::S3 => finish_s3(endpoint, &host, flags, addressing_override),
        BackendKind::Azure => finish_azure(endpoint, &host, flags, addressing_override),
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
            Self::S3 { endpoint, .. } => write!(f, "s3+{endpoint}"),
            Self::Azure { endpoint, .. } => write!(f, "az+{endpoint}"),
        }
    }
}

impl RemoteUrl {
    /// Returns the canonical endpoint URL (without the backend prefix).
    #[must_use]
    pub const fn endpoint(&self) -> &Url {
        match self {
            Self::S3 { endpoint, .. } | Self::Azure { endpoint, .. } => endpoint,
        }
    }

    /// Returns the optional repository prefix.
    #[must_use]
    pub fn prefix(&self) -> Option<&str> {
        match self {
            Self::S3 { prefix, .. } | Self::Azure { prefix, .. } => prefix.as_deref(),
        }
    }

    /// Returns the parsed query flags.
    #[must_use]
    pub const fn flags(&self) -> &RemoteFlags {
        match self {
            Self::S3 { flags, .. } | Self::Azure { flags, .. } => flags,
        }
    }

    /// Returns the backend kind discriminant.
    #[must_use]
    pub const fn kind(&self) -> BackendKind {
        match self {
            Self::S3 { .. } => BackendKind::S3,
            Self::Azure { .. } => BackendKind::Azure,
        }
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddressingOverride {
    Path,
    Virtual,
}

/// Classify the URL by its backend scheme prefix and return both the
/// detected [`BackendKind`] and the body of the URL with the `s3+` /
/// `az+` tag stripped. Folding the classification and the strip into
/// one step keeps `parse()` free of an unreachable fallback for a
/// mismatched prefix.
///
/// Each branch also verifies that the body starts with `https://` or
/// `http://` so the downstream `Url::parse` sees a recognised scheme.
fn detect_backend(input: &str) -> Result<(BackendKind, &str), ParseError> {
    for kind in [BackendKind::S3, BackendKind::Azure] {
        if let Some(body) = input.strip_prefix(kind.scheme_prefix())
            && (body.starts_with("https://") || body.starts_with("http://"))
        {
            return Ok((kind, body));
        }
    }
    Err(ParseError::UnsupportedScheme(scheme_of(input)))
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
            "engine" => {
                flags.engine = Some(
                    StorageEngine::from_name(value.as_ref())
                        .ok_or_else(|| ParseError::UnknownEngine(value.into_owned()))?,
                );
            }
            "bundle_uri" => flags.bundle_uri = parse_bool_flag("bundle_uri", value.as_ref())?,
            "bundle_uri_presign_ttl" => {
                flags.bundle_uri_presign_ttl = Some(parse_bundle_uri_presign_ttl(value.as_ref())?);
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

/// Parse a positive integer flag value into [`NonZeroU64`]. Rejects
/// `0`, negative values, non-numeric junk. Used for `bundle_uri_presign_ttl`
/// (issue #76).
fn parse_nonzero_u64_flag(name: &str, value: &str) -> Result<NonZeroU64, ParseError> {
    let n: u64 = value.parse().map_err(|_| ParseError::InvalidFlagValue {
        name: name.to_owned(),
        value: value.to_owned(),
    })?;
    NonZeroU64::new(n).ok_or_else(|| ParseError::InvalidFlagValue {
        name: name.to_owned(),
        value: value.to_owned(),
    })
}

/// Parse `?bundle_uri_presign_ttl=<seconds>`: positive integer in
/// `1..=MAX_BUNDLE_URI_PRESIGN_TTL_SECONDS`. The upper cap matches
/// AWS's hard 7-day ceiling on presigned URLs and protects the Azure
/// SAS builder from `u64`-overflow inputs (issue #219).
fn parse_bundle_uri_presign_ttl(value: &str) -> Result<NonZeroU64, ParseError> {
    let ttl = parse_nonzero_u64_flag("bundle_uri_presign_ttl", value)?;
    if ttl.get() > MAX_BUNDLE_URI_PRESIGN_TTL_SECONDS {
        return Err(ParseError::BundleUriPresignTtlTooLarge {
            value: ttl.get(),
            max: MAX_BUNDLE_URI_PRESIGN_TTL_SECONDS,
        });
    }
    Ok(ttl)
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

/// AWS partition suffixes that are owned by AWS and therefore subject to
/// `check_aws_s3_host` validation. Hosts ending in any of these must
/// match a recognised S3 endpoint shape; hosts ending in anything else
/// are treated as third-party S3-compatible endpoints (`MinIO`,
/// Cloudflare R2, …) and skip the check entirely.
///
/// Order is irrelevant for correctness: a host that ends in
/// `.amazonaws.com.cn` does not end in `.amazonaws.com` (the trailing
/// `.cn` rules that out), so the two suffixes are mutually exclusive on
/// any given host. The China entry is listed first by convention only.
pub(crate) const AWS_HOST_SUFFIXES: &[&str] = &[".amazonaws.com.cn", ".amazonaws.com"];

/// If `host` ends in one of [`AWS_HOST_SUFFIXES`], return the host with
/// that suffix stripped; otherwise return `None`. Single source of truth
/// for "is this an AWS partition host, and what is the leading portion?"
pub(crate) fn strip_aws_host_suffix(host: &str) -> Option<&str> {
    AWS_HOST_SUFFIXES
        .iter()
        .find_map(|suffix| host.strip_suffix(suffix))
}

/// Reject AWS hostnames (`.amazonaws.com` and `.amazonaws.com.cn`) that
/// cannot be valid S3 endpoints.
///
/// Third-party S3-compatible endpoints (custom hosts, `MinIO`, R2, …)
/// are passed through unconditionally — they do not end in an AWS
/// partition suffix. For AWS hosts, after stripping the partition
/// suffix the remainder must match one of:
///
/// - `s3` (legacy global path-style: `s3.amazonaws.com`)
/// - `s3.<region>` (path-style with region: `s3.us-west-2.amazonaws.com`)
/// - `s3-<region>` (legacy hyphenated path-style:
///   `s3-us-east-1.amazonaws.com`)
/// - end with `.s3` (no-region virtual-hosted:
///   `<bucket>.s3.amazonaws.com`, where the trailing `.s3` label is
///   the AWS service marker for the legacy global form)
/// - contain `.s3.` or `.s3-` (virtual-hosted with region:
///   `<bucket>.s3.<region>.amazonaws.com` /
///   `<bucket>.s3-<region>.amazonaws.com`)
///
/// The common mistake `<bucket>.<region>.amazonaws.com` — missing the
/// `.s3.` service marker — would otherwise silently fall through to
/// path-style addressing with a non-existent endpoint hostname,
/// producing an inscrutable DNS-resolution error at connect time.
///
/// **Policy on `?addressing=` override:** this check runs before the
/// addressing override is applied, so `?addressing=path` (or
/// `=virtual`) on an AWS hostname does not bypass it. AWS owns
/// `.amazonaws.com[.cn]`; any host on those suffixes that is not a
/// recognised S3 endpoint is a typo, and a fast-fail with the helpful
/// `InvalidAwsS3Endpoint` error is preferable to letting the user pick
/// any addressing style they want against a non-existent endpoint.
fn check_aws_s3_host(host: &str) -> Result<(), ParseError> {
    let Some(trimmed) = strip_aws_host_suffix(host) else {
        // Not an AWS host — third-party S3-compatible endpoint, always OK.
        return Ok(());
    };

    // `<bucket>.s3.amazonaws.com` → trimmed is `<bucket>.s3`; the last
    // dot-separated label is "s3" (global virtual-hosted, no region).
    // This is the only branch that catches the no-region virtual-hosted
    // shape — it is NOT redundant with the `.s3.` / `.s3-` infix checks
    // (which require a region segment after the marker).
    let last_label_is_s3 = trimmed.split('.').next_back() == Some("s3");

    let valid = trimmed == "s3"
        || trimmed.starts_with("s3.")
        // Legacy path-style hyphenated form: `s3-<region>.amazonaws.com`.
        // Accepts any `s3-*` prefix without validating the region name, so
        // `s3-mybucket.amazonaws.com` is a known false-negative (passes the
        // check but is not a real S3 endpoint; user sees a DNS error rather
        // than this helpful message). Tightening would require a region
        // allowlist, which is fragile as AWS adds regions.
        || trimmed.starts_with("s3-")
        || last_label_is_s3
        || trimmed.contains(".s3.")
        || trimmed.contains(".s3-");

    if !valid {
        return Err(ParseError::InvalidAwsS3Endpoint {
            host: host.to_owned(),
        });
    }
    Ok(())
}

fn finish_s3(
    mut endpoint: Url,
    host: &str,
    flags: RemoteFlags,
    addressing_override: Option<AddressingOverride>,
) -> Result<RemoteUrl, ParseError> {
    let segments = path_segments(&endpoint);

    check_aws_s3_host(host)?;

    let (addressing, bucket, prefix_segments) =
        resolve_s3_components(host, &segments, addressing_override)?;

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

/// Determine S3 addressing style and extract the bucket name and prefix
/// segments from the URL's host and path.
///
/// Path-style skips the `rfind` scan entirely; virtual-hosted (auto or
/// explicit) runs it once and reuses the result for both detection and
/// extraction.
fn resolve_s3_components<'a>(
    host: &str,
    segments: &'a [String],
    addressing_override: Option<AddressingOverride>,
) -> Result<(S3Addressing, String, &'a [String]), ParseError> {
    // Compute addressing and the AWS bucket prefix together.
    let (addressing, aws_bucket) = match addressing_override {
        Some(AddressingOverride::Path) => (S3Addressing::PathStyle, None),
        Some(AddressingOverride::Virtual) => {
            (S3Addressing::VirtualHosted, s3_virtual_hosted_bucket(host))
        }
        None => {
            let b = s3_virtual_hosted_bucket(host);
            let style = if b.is_some() {
                S3Addressing::VirtualHosted
            } else {
                S3Addressing::PathStyle
            };
            (style, b)
        }
    };

    let (bucket, prefix_segments) = match addressing {
        S3Addressing::VirtualHosted => {
            // `aws_bucket` covers both auto-detected and explicit
            // `?addressing=virtual` for AWS hosts. Falls back to the
            // leftmost label for non-AWS virtual-hosted endpoints, which
            // by convention put the bucket as the leftmost label.
            let bucket = aws_bucket
                .or_else(|| leftmost_label(host))
                .ok_or(ParseError::MissingBucket)?;
            (bucket, segments)
        }
        S3Addressing::PathStyle => {
            let (head, tail) = segments.split_first().ok_or(ParseError::MissingBucket)?;
            (head.clone(), tail)
        }
    };

    Ok((addressing, bucket, prefix_segments))
}

/// AWS virtual-hosted infixes anchored at the start of the
/// `s3[.-]<region>.amazonaws.com` suffix. The scan picks the rightmost
/// occurrence (see `s3_virtual_hosted_bucket`) so a bucket prefix
/// containing dots — or even a literal `.s3.` segment — survives
/// intact and only the AWS service marker before the region is
/// consumed.
pub(crate) const AWS_S3_INFIXES: &[&str] = &[".s3.", ".s3-"];

/// Extract the bucket prefix that precedes the AWS `.s3.` or `.s3-`
/// service infix in `host`. Returns `None` for hosts that don't carry
/// the AWS virtual-hosted shape — callers fall back to `leftmost_label`
/// for non-AWS endpoints reached via `?addressing=virtual`.
///
/// Uses `rfind` (rightmost occurrence) so a bucket name that itself
/// contains `.s3.` or `.s3-` segments (no AWS rule forbids it) is
/// extracted in full instead of being truncated at the first match.
/// The returned string is the entire substring before the chosen
/// infix, so dotted bucket names like `bucketname.com` survive intact.
pub(crate) fn s3_virtual_hosted_bucket(host: &str) -> Option<String> {
    // Both infixes are 4 bytes, so the one whose rfind position is
    // numerically largest is the rightmost match in the string — no need
    // to track which infix won after taking the max.
    AWS_S3_INFIXES
        .iter()
        .filter_map(|infix| host.rfind(infix))
        .max()
        .map(|idx| host[..idx].to_owned())
        .filter(|bucket| !bucket.is_empty())
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
    host: &str,
    flags: RemoteFlags,
    addressing_override: Option<AddressingOverride>,
) -> Result<RemoteUrl, ParseError> {
    let segments = path_segments(&endpoint);

    let addressing = match addressing_override {
        Some(AddressingOverride::Path) => AzureAddressing::PathStyle,
        Some(AddressingOverride::Virtual) => AzureAddressing::VirtualHosted,
        None => detect_azure_addressing(host),
    };

    let (account, container, prefix_segments) =
        resolve_azure_components(addressing, host, &segments)?;

    if !is_valid_account(&account) {
        return Err(ParseError::InvalidAccount(account));
    }
    if !is_valid_container(&container) {
        return Err(ParseError::InvalidContainer(container));
    }
    let prefix = join_prefix(prefix_segments);

    let canonical: Vec<&str> = match addressing {
        AzureAddressing::VirtualHosted => std::iter::once(container.as_str())
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

/// Extract the storage account, container, and prefix segments from the
/// URL's host and path, according to the resolved addressing style.
fn resolve_azure_components<'a>(
    addressing: AzureAddressing,
    host: &str,
    segments: &'a [String],
) -> Result<(String, String, &'a [String]), ParseError> {
    match addressing {
        AzureAddressing::VirtualHosted => {
            let account = leftmost_label(host).ok_or(ParseError::MissingAccount)?;
            match segments {
                [] => Err(ParseError::MissingContainer),
                [container, rest @ ..] => Ok((account, container.clone(), rest)),
            }
        }
        AzureAddressing::PathStyle => match segments {
            [] => Err(ParseError::MissingAccount),
            [_] => Err(ParseError::MissingContainer),
            [account, container, rest @ ..] => Ok((account.clone(), container.clone(), rest)),
        },
    }
}

fn detect_azure_addressing(host: &str) -> AzureAddressing {
    // §3.4: virtual-hosted iff the second hostname label is `blob`.
    // Hosts are already lowercased by the `url` crate (RFC 3986).
    if host.split('.').nth(1) == Some("blob") {
        AzureAddressing::VirtualHosted
    } else {
        AzureAddressing::PathStyle
    }
}

// ---------------------------------------------------------------------------
// Validation (§3.5)
// ---------------------------------------------------------------------------

/// AWS-reserved bucket-name prefixes. See
/// <https://docs.aws.amazon.com/AmazonS3/latest/userguide/bucketnamingrules.html>.
const FORBIDDEN_BUCKET_PREFIXES: &[&str] = &["xn--", "sthree-", "amzn-s3-demo-"];

/// AWS-reserved bucket-name suffixes. See the same AWS doc.
const FORBIDDEN_BUCKET_SUFFIXES: &[&str] =
    &["-s3alias", "--ol-s3", ".mrap", "--x-s3", "--table-s3"];

/// AWS S3 General Purpose bucket-naming rules: 3–63 chars, lowercase
/// alphanumerics plus `.` and `-`, must begin and end with a letter or
/// digit, no consecutive periods, not formatted as an IPv4 address, and
/// none of the AWS reserved prefixes or suffixes.
fn is_valid_bucket(s: &str) -> bool {
    let bytes = s.as_bytes();
    let (Some(&first), Some(&last)) = (bytes.first(), bytes.last()) else {
        return false;
    };
    (3..=63).contains(&bytes.len())
        && is_ascii_alphanum_lower(first)
        && is_ascii_alphanum_lower(last)
        && bytes
            .iter()
            .all(|b| is_ascii_alphanum_lower(*b) || matches!(*b, b'.' | b'-'))
        && !s.contains("..")
        && !is_ipv4_formatted(s)
        && !FORBIDDEN_BUCKET_PREFIXES.iter().any(|p| s.starts_with(p))
        && !FORBIDDEN_BUCKET_SUFFIXES.iter().any(|p| s.ends_with(p))
}

/// `[a-z0-9]{3,24}` — Azure storage-account naming rule.
fn is_valid_account(s: &str) -> bool {
    (3..=24).contains(&s.len()) && s.bytes().all(is_ascii_alphanum_lower)
}

/// Azure container-naming rule: 3–63 chars, lowercase alphanumerics plus
/// `-`, must begin and end with a letter or digit, and no consecutive
/// hyphens. See
/// <https://learn.microsoft.com/en-us/rest/api/storageservices/naming-and-referencing-containers--blobs--and-metadata>.
fn is_valid_container(s: &str) -> bool {
    let bytes = s.as_bytes();
    let (Some(&first), Some(&last)) = (bytes.first(), bytes.last()) else {
        return false;
    };
    (3..=63).contains(&bytes.len())
        && is_ascii_alphanum_lower(first)
        && is_ascii_alphanum_lower(last)
        && bytes
            .iter()
            .all(|b| is_ascii_alphanum_lower(*b) || *b == b'-')
        && !s.contains("--")
}

const fn is_ascii_alphanum_lower(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit()
}

/// True iff `s` looks like a dotted-quad IPv4 address (four non-empty
/// digit-only segments separated by `.`). AWS rejects bucket names with
/// this shape regardless of whether the address is routable.
fn is_ipv4_formatted(s: &str) -> bool {
    let mut parts = 0usize;
    for part in s.split('.') {
        parts += 1;
        if parts > 4 {
            return false;
        }
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    parts == 4
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
    fn rejects_backend_tag_with_unsupported_inner_scheme() {
        // `detect_backend` must check both the `s3+`/`az+` tag and the
        // inner `http(s)://` scheme — otherwise an `s3+ftp://` URL would
        // sneak past classification and surface as a confusing downstream
        // `Url::parse` error.
        for input in [
            "s3+ftp://example.com/b",
            "az+ftp://acct.blob.core.windows.net/c",
        ] {
            let err = parse(input).unwrap_err();
            assert!(
                matches!(&err, ParseError::UnsupportedScheme(_)),
                "expected UnsupportedScheme for {input}, got {err:?}",
            );
        }
    }

    #[test]
    fn validates_bucket_charset() {
        assert!(is_valid_bucket("my-bucket"));
        assert!(is_valid_bucket("a23"));
        assert!(is_valid_bucket("a.b.c"));
        assert!(!is_valid_bucket("ab"));
        assert!(!is_valid_bucket("-leading-dash"));
        assert!(!is_valid_bucket("trailing-dash-"));
        assert!(!is_valid_bucket(".leading-dot"));
        assert!(!is_valid_bucket("trailing-dot."));
        assert!(!is_valid_bucket("UPPER"));
        assert!(!is_valid_bucket(&"a".repeat(64)));
    }

    #[test]
    fn rejects_bucket_with_consecutive_dots() {
        assert!(!is_valid_bucket("ab..cd"));
        assert!(!is_valid_bucket("a..b"));
    }

    #[test]
    fn rejects_bucket_formatted_like_ipv4() {
        assert!(!is_valid_bucket("192.168.1.1"));
        assert!(!is_valid_bucket("1.2.3.4"));
        assert!(!is_valid_bucket("999.999.999.999"));
        // Three or five segments are not IPv4-shaped.
        assert!(is_valid_bucket("1.2.3"));
        assert!(is_valid_bucket("1.2.3.4.5"));
    }

    #[test]
    fn rejects_forbidden_bucket_prefixes() {
        assert!(!is_valid_bucket("xn--abc"));
        assert!(!is_valid_bucket("sthree-foo"));
        assert!(!is_valid_bucket("amzn-s3-demo-bucket"));
    }

    #[test]
    fn rejects_forbidden_bucket_suffixes() {
        assert!(!is_valid_bucket("my-bucket-s3alias"));
        assert!(!is_valid_bucket("my-bucket--ol-s3"));
        assert!(!is_valid_bucket("my-bucket--x-s3"));
        assert!(!is_valid_bucket("my-bucket--table-s3"));
        assert!(!is_valid_bucket("ab.mrap"));
    }

    #[test]
    fn ipv4_formatted_helper() {
        assert!(is_ipv4_formatted("0.0.0.0"));
        assert!(is_ipv4_formatted("10.20.30.40"));
        assert!(!is_ipv4_formatted("a.b.c.d"));
        assert!(!is_ipv4_formatted("1.2.3"));
        assert!(!is_ipv4_formatted("1.2.3.4.5"));
        assert!(!is_ipv4_formatted("1..2.3"));
        assert!(!is_ipv4_formatted(".1.2.3.4"));
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
        assert!(is_valid_container("a-b-c"));
        assert!(!is_valid_container("ab"));
        assert!(!is_valid_container("UPPER"));
        assert!(!is_valid_container(&"a".repeat(64)));
    }

    #[test]
    fn rejects_container_with_dash_at_boundary() {
        assert!(!is_valid_container("-leading"));
        assert!(!is_valid_container("trailing-"));
    }

    #[test]
    fn rejects_container_with_consecutive_dashes() {
        assert!(!is_valid_container("a--b"));
        assert!(!is_valid_container("foo--bar"));
    }

    #[test]
    fn s3_addressing_heuristic() {
        // Auto-detection is now expressed as s3_virtual_hosted_bucket.is_some().
        assert!(s3_virtual_hosted_bucket("my-bucket.s3.us-west-2.amazonaws.com").is_some());
        assert!(s3_virtual_hosted_bucket("s3.us-west-2.amazonaws.com").is_none());
        assert!(s3_virtual_hosted_bucket("acc.r2.cloudflarestorage.com").is_none());
    }

    #[test]
    fn s3_addressing_heuristic_dotted_bucket() {
        // Bucket names with embedded dots stretch the host across more
        // than two labels — auto-detection must still recognise the
        // virtual-hosted shape.
        assert!(s3_virtual_hosted_bucket("bucketname.com.s3.us-west-2.amazonaws.com").is_some());
        assert!(s3_virtual_hosted_bucket("my.dotted.s3.us-west-2.amazonaws.com").is_some());
        // Legacy `s3-<region>` hyphenated form.
        assert!(s3_virtual_hosted_bucket("bucketname.com.s3-us-west-2.amazonaws.com").is_some());
    }

    #[test]
    fn s3_virtual_hosted_bucket_extracts_full_prefix() {
        assert_eq!(
            s3_virtual_hosted_bucket("my-bucket.s3.us-west-2.amazonaws.com"),
            Some("my-bucket".to_owned())
        );
        assert_eq!(
            s3_virtual_hosted_bucket("bucketname.com.s3.us-west-2.amazonaws.com"),
            Some("bucketname.com".to_owned())
        );
        assert_eq!(
            s3_virtual_hosted_bucket("my.dotted.s3.us-west-2.amazonaws.com"),
            Some("my.dotted".to_owned())
        );
        assert_eq!(
            s3_virtual_hosted_bucket("bucketname.com.s3-us-west-2.amazonaws.com"),
            Some("bucketname.com".to_owned())
        );
        // Path-style host has no `.s3.` infix preceded by anything —
        // returns None so the caller falls through.
        assert_eq!(s3_virtual_hosted_bucket("s3.us-west-2.amazonaws.com"), None);
        // Non-AWS host: no infix.
        assert_eq!(
            s3_virtual_hosted_bucket("acc.r2.cloudflarestorage.com"),
            None
        );
        // Pathological: bucket name itself contains `.s3.`. The
        // rightmost infix is the AWS service marker, so the full
        // bucket prefix is recovered.
        assert_eq!(
            s3_virtual_hosted_bucket("my.s3.bucket.s3.us-west-2.amazonaws.com"),
            Some("my.s3.bucket".to_owned())
        );
    }

    #[test]
    fn azure_addressing_heuristic() {
        assert_eq!(
            detect_azure_addressing("my-account.blob.core.windows.net"),
            AzureAddressing::VirtualHosted
        );
        assert_eq!(
            detect_azure_addressing("127.0.0.1"),
            AzureAddressing::PathStyle
        );
    }

    #[test]
    fn azure_path_style_with_account_only_rejects_missing_container() {
        // Path-style: host/account/container/prefix. Exactly one path
        // segment means the container is absent — must be a parse error.
        let err = parse("az+https://127.0.0.1/myaccount").unwrap_err();
        assert!(
            matches!(err, ParseError::MissingContainer),
            "expected MissingContainer, got {err:?}",
        );
    }

    // --- StorageEngine and ?engine= flag ---------------------------------

    #[test]
    fn engine_flag_absent_leaves_none() {
        let url = parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo").unwrap();
        assert_eq!(url.flags().engine, None);
    }

    #[test]
    fn engine_flag_bundle_parses() {
        let url =
            parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=bundle").unwrap();
        assert_eq!(url.flags().engine, Some(StorageEngine::Bundle));
    }

    #[test]
    fn engine_flag_rejects_unknown_value() {
        let err =
            parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=pack").unwrap_err();
        assert!(
            matches!(err, ParseError::UnknownEngine(ref s) if s == "pack"),
            "expected UnknownEngine(pack), got {err:?}",
        );
    }

    #[test]
    fn engine_flag_rejects_empty_value() {
        let err =
            parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=").unwrap_err();
        assert!(
            matches!(err, ParseError::UnknownEngine(ref s) if s.is_empty()),
            "expected UnknownEngine(\"\"), got {err:?}",
        );
    }

    #[test]
    fn unknown_engine_error_message_lists_every_supported_engine() {
        // Iterating over `StorageEngine::ALL` keeps this regression test
        // synchronised with the enum: a new variant whose name is missing
        // from the rendered diagnostic fails this assertion.
        let err =
            parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=pack").unwrap_err();
        let rendered = err.to_string();
        assert!(
            rendered.contains("unknown engine `pack`"),
            "missing rejected-value in `{rendered}`",
        );
        for engine in StorageEngine::ALL {
            assert!(
                rendered.contains(&format!("`{}`", engine.as_str())),
                "UnknownEngine message must mention engine `{}`, got `{rendered}`",
                engine.as_str(),
            );
        }
    }

    #[test]
    fn engine_as_str_roundtrips() {
        assert_eq!(StorageEngine::Bundle.as_str(), "bundle");
        assert_eq!(StorageEngine::Bundle.to_string(), "bundle");
        assert_eq!(StorageEngine::Packchain.as_str(), "packchain");
        assert_eq!(StorageEngine::Packchain.to_string(), "packchain");
    }

    #[test]
    fn engine_from_name_parses_known_and_rejects_unknown() {
        assert_eq!(
            StorageEngine::from_name("bundle"),
            Some(StorageEngine::Bundle)
        );
        assert_eq!(
            StorageEngine::from_name("packchain"),
            Some(StorageEngine::Packchain)
        );
        assert_eq!(StorageEngine::from_name("pack"), None);
        assert_eq!(StorageEngine::from_name(""), None);
        assert_eq!(StorageEngine::from_name("Bundle"), None); // case-sensitive
        assert_eq!(StorageEngine::from_name("Packchain"), None); // case-sensitive
    }

    #[test]
    fn engine_flag_packchain_parses() {
        let url =
            parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain").unwrap();
        assert_eq!(url.flags().engine, Some(StorageEngine::Packchain));
    }

    // --- bundle_uri flag (issue #71) -------------------------------------

    #[test]
    fn bundle_uri_flag_absent_defaults_to_false() {
        let url = parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo").unwrap();
        assert!(!url.flags().bundle_uri);
    }

    #[test]
    fn bundle_uri_flag_one_sets_true() {
        let url = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain&bundle_uri=1",
        )
        .unwrap();
        assert!(url.flags().bundle_uri);
    }

    #[test]
    fn bundle_uri_flag_zero_sets_false() {
        let url = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain&bundle_uri=0",
        )
        .unwrap();
        assert!(!url.flags().bundle_uri);
    }

    // --- bundle_uri_presign_ttl flag (issue #76) -------------------------

    #[test]
    fn bundle_uri_presign_ttl_absent_defaults_to_none() {
        let url = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain&bundle_uri=1",
        )
        .unwrap();
        assert_eq!(url.flags().bundle_uri_presign_ttl, None);
    }

    #[test]
    fn bundle_uri_presign_ttl_positive_int_parses() {
        let url = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
             ?engine=packchain&bundle_uri=1&bundle_uri_presign_ttl=3600",
        )
        .unwrap();
        assert_eq!(
            url.flags().bundle_uri_presign_ttl,
            Some(NonZeroU64::new(3600).expect("3600 is non-zero")),
        );
    }

    #[test]
    fn bundle_uri_presign_ttl_one_second_accepted() {
        // Useless in practice but the type-system contract is "any
        // positive value"; operator's prerogative to choose.
        let url = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
             ?engine=packchain&bundle_uri=1&bundle_uri_presign_ttl=1",
        )
        .unwrap();
        assert_eq!(
            url.flags().bundle_uri_presign_ttl,
            Some(NonZeroU64::new(1).expect("1 is non-zero")),
        );
    }

    #[test]
    fn bundle_uri_presign_ttl_zero_rejected() {
        // Zero-second TTL is meaningless; reject at the boundary
        // rather than letting the bad value flow into the
        // (presigning) backend.
        let err = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
             ?engine=packchain&bundle_uri=1&bundle_uri_presign_ttl=0",
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidFlagValue { ref name, ref value }
                    if name == "bundle_uri_presign_ttl" && value == "0"
            ),
            "expected InvalidFlagValue {{ name: bundle_uri_presign_ttl, value: 0 }}, got {err:?}",
        );
    }

    #[test]
    fn bundle_uri_presign_ttl_non_numeric_rejected() {
        let err = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
             ?engine=packchain&bundle_uri=1&bundle_uri_presign_ttl=abc",
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::InvalidFlagValue { ref name, ref value }
                    if name == "bundle_uri_presign_ttl" && value == "abc"
            ),
            "expected InvalidFlagValue, got {err:?}",
        );
    }

    #[test]
    fn bundle_uri_presign_ttl_negative_rejected() {
        // u64 parser rejects negative input; surface as InvalidFlagValue.
        let err = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
             ?engine=packchain&bundle_uri=1&bundle_uri_presign_ttl=-1",
        )
        .unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidFlagValue { ref name, .. } if name == "bundle_uri_presign_ttl"),
            "expected InvalidFlagValue, got {err:?}",
        );
    }

    /// Issue #219: huge values panic the Azure SAS builder via
    /// `time::Duration::seconds_f64`. The URL boundary caps the flag
    /// at [`MAX_BUNDLE_URI_PRESIGN_TTL_SECONDS`] (7 days) so the bad
    /// value never reaches the helper, matching the AWS SDK's hard
    /// ceiling.
    #[test]
    fn bundle_uri_presign_ttl_above_seven_days_rejected() {
        let err = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
             ?engine=packchain&bundle_uri=1&bundle_uri_presign_ttl=604801",
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::BundleUriPresignTtlTooLarge { value, max }
                    if value == 604_801 && max == MAX_BUNDLE_URI_PRESIGN_TTL_SECONDS
            ),
            "expected BundleUriPresignTtlTooLarge {{ value: 604801, max: {MAX_BUNDLE_URI_PRESIGN_TTL_SECONDS} }}, got {err:?}",
        );
    }

    /// Issue #219: the pathological `u64::MAX`-class value reported
    /// in the bug must be rejected at the URL boundary with a clean
    /// error rather than panicking the helper.
    #[test]
    fn bundle_uri_presign_ttl_huge_value_rejected_not_panic() {
        let err = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
             ?engine=packchain&bundle_uri=1&bundle_uri_presign_ttl=999999999999999999",
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::BundleUriPresignTtlTooLarge { value, .. }
                    if value == 999_999_999_999_999_999
            ),
            "expected BundleUriPresignTtlTooLarge for huge value, got {err:?}",
        );
    }

    /// Issue #219: the 7-day boundary value itself is accepted so
    /// operators can express AWS's spec-mandated maximum.
    #[test]
    fn bundle_uri_presign_ttl_exactly_seven_days_accepted() {
        let url = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
             ?engine=packchain&bundle_uri=1&bundle_uri_presign_ttl=604800",
        )
        .unwrap();
        assert_eq!(
            url.flags().bundle_uri_presign_ttl,
            Some(
                NonZeroU64::new(MAX_BUNDLE_URI_PRESIGN_TTL_SECONDS).expect("7-day cap is non-zero")
            ),
        );
    }

    #[test]
    fn engine_flag_packchain_on_azure_url() {
        let url =
            parse("az+https://myaccount.blob.core.windows.net/my-container/repo?engine=packchain")
                .unwrap();
        assert_eq!(url.flags().engine, Some(StorageEngine::Packchain));
    }

    #[test]
    fn engine_flag_on_azure_url() {
        let url =
            parse("az+https://myaccount.blob.core.windows.net/my-container/repo?engine=bundle")
                .unwrap();
        assert_eq!(url.flags().engine, Some(StorageEngine::Bundle));
    }

    // --- AWS S3 endpoint host validation ------------------------------------

    #[test]
    fn rejects_amazonaws_host_missing_s3_service_marker() {
        // The common mistake: <bucket>.<region>.amazonaws.com — no `.s3.`.
        let err = parse("s3+https://git-test-2224.us-west-2.amazonaws.com/git-remote-object-store")
            .unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidAwsS3Endpoint { ref host } if host == "git-test-2224.us-west-2.amazonaws.com"),
            "expected InvalidAwsS3Endpoint, got {err:?}",
        );
    }

    #[test]
    fn accepts_valid_aws_s3_hosts() {
        // Virtual-hosted with region.
        parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo").unwrap();
        // Virtual-hosted without region (legacy global).
        parse("s3+https://my-bucket.s3.amazonaws.com/repo").unwrap();
        // Virtual-hosted legacy hyphenated region.
        parse("s3+https://my-bucket.s3-us-west-2.amazonaws.com/repo").unwrap();
        // Path-style with region.
        parse("s3+https://s3.us-west-2.amazonaws.com/my-bucket/repo").unwrap();
        // Path-style without region (legacy global).
        parse("s3+https://s3.amazonaws.com/my-bucket/repo").unwrap();
        // Path-style legacy hyphenated region (`s3-<region>.amazonaws.com`).
        parse("s3+https://s3-us-east-1.amazonaws.com/my-bucket/repo").unwrap();
        // China partition (`.amazonaws.com.cn`): both addressing styles.
        parse("s3+https://my-bucket.s3.cn-north-1.amazonaws.com.cn/repo").unwrap();
        parse("s3+https://s3.cn-north-1.amazonaws.com.cn/my-bucket/repo").unwrap();
    }

    #[test]
    fn rejects_china_amazonaws_host_missing_s3_service_marker() {
        // Same typo class as `rejects_amazonaws_host_missing_s3_service_marker`
        // but on the China partition (`.amazonaws.com.cn`). The typo
        // `<bucket>.<region>.amazonaws.com.cn` (no `.s3.` marker) must
        // produce the helpful `InvalidAwsS3Endpoint`, not a silent fall-
        // through to PathStyle and a DNS error at connect time.
        let err = parse("s3+https://git-test.cn-north-1.amazonaws.com.cn/repo").unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidAwsS3Endpoint { ref host } if host == "git-test.cn-north-1.amazonaws.com.cn"),
            "expected InvalidAwsS3Endpoint, got {err:?}",
        );
    }

    #[test]
    fn check_aws_s3_host_runs_before_addressing_override() {
        // Policy: `?addressing=path` (or `=virtual`) on an AWS hostname
        // does NOT bypass the validator. AWS owns `.amazonaws.com[.cn]`,
        // so any host on those suffixes that is not a recognised S3
        // endpoint is a typo. A user who needs path-style addressing on a
        // vanity host should use a domain they own, not `.amazonaws.com`.
        let err =
            parse("s3+https://corp.amazonaws.com/my-bucket/repo?addressing=path").unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidAwsS3Endpoint { ref host } if host == "corp.amazonaws.com"),
            "expected InvalidAwsS3Endpoint, got {err:?}",
        );
        let err =
            parse("s3+https://corp.amazonaws.com/my-bucket/repo?addressing=virtual").unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidAwsS3Endpoint { ref host } if host == "corp.amazonaws.com"),
            "expected InvalidAwsS3Endpoint, got {err:?}",
        );
    }

    #[test]
    fn accepts_s3_prefix_known_false_negative() {
        // `s3-<non-region>.amazonaws.com` passes `check_aws_s3_host` because
        // the `starts_with("s3-")` guard does not validate the region name.
        // Pinned here to document the known false-negative: the parse
        // succeeds, but the user will see a DNS error at connect time rather
        // than the helpful `InvalidAwsS3Endpoint` message. The valid legacy
        // form (`s3-us-east-1`) and this false-negative are accepted by the
        // same branch; a tightening that rejects false-negatives must not
        // break valid legacy inputs.
        parse("s3+https://s3-mybucket.amazonaws.com/my-bucket/repo").unwrap();
    }

    #[test]
    fn accepts_non_aws_s3_compatible_hosts() {
        // MinIO, Cloudflare R2, and other S3-compatible services that do
        // not use `.amazonaws.com` are not subject to the service-marker check.
        parse("s3+https://play.min.io/my-bucket/repo").unwrap();
        parse("s3+https://acc.r2.cloudflarestorage.com/my-bucket/repo").unwrap();
        parse("s3+https://localhost/my-bucket/repo?zip=0").unwrap();
    }
}

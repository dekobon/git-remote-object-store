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
/// non-loopback hosts. Accepted when set to any of the truthy values
/// recognised by [`parse_bool_value`] (`1`, `true`, `yes`, `on`,
/// case-insensitive). Any other value — including the empty string,
/// `0`, `false`, `no`, `off`, or any unrecognised token — is treated
/// as "not set" and the cleartext-HTTP gate stays closed.
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
pub(crate) const MAX_BUNDLE_URI_PRESIGN_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;

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

    /// Human-readable backend name for diagnostics (`"S3"` / `"Azure"`).
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::S3 => "S3",
            Self::Azure => "Azure",
        }
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Query-string flags described in §3.2 / §3.3.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteFlags {
    /// `?zip=1` — push uploads `repo.zip` alongside each bundle.
    pub zip: bool,
    /// `?profile=...` — selects a named AWS profile. S3 only; an Azure
    /// URL carrying this flag is rejected with
    /// [`ParseError::FlagNotApplicable`].
    pub profile: Option<String>,
    /// `?credential=...` — names an Azure credential alias. Azure only;
    /// an S3 URL carrying this flag is rejected with
    /// [`ParseError::FlagNotApplicable`].
    pub credential: Option<String>,
    /// `?region=...` — overrides the SDK-derived region (rare). S3 only;
    /// an Azure URL carrying this flag is rejected with
    /// [`ParseError::FlagNotApplicable`].
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
    /// `?bundle_uri_presign_ttl=<seconds>` — on a packchain remote with
    /// `?bundle_uri=1`, the helper presigns each emitted
    /// `bundle.<ref>.uri=<url>` line with an `<seconds>`-TTL signed
    /// URL (S3 `SigV4` or Azure service-SAS). Operators with private
    /// buckets need this; public-read buckets and CDN-fronted
    /// endpoints can leave it unset (the canonical URL works
    /// directly).
    ///
    /// Meaningful only when bundle-uri advertising is active — the TTL
    /// solely governs presigning of the `bundle.<ref>.uri=` lines, which
    /// are emitted nowhere else. Supplying it without `?bundle_uri=1` is
    /// therefore rejected at the URL boundary with
    /// [`ParseError::BundleUriPresignTtlWithoutBundleUri`] rather than
    /// silently discarded (issue #246). The engine is **not** checked
    /// here: it is resolved from the bucket `FORMAT` at connect time, so a
    /// packchain bucket is validly reconnected with `?bundle_uri=1` and
    /// the TTL but no `?engine=packchain` (a bundle-engine bucket simply
    /// leaves the TTL inert, exactly as it does the `?bundle_uri=1` flag).
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
    /// A flag is a documented flag but does not apply to the selected
    /// backend (e.g. `?profile=` or `?region=` on an Azure URL,
    /// `?credential=` on an S3 URL). Rejected with the same fail-fast
    /// policy as [`UnknownFlag`][Self::UnknownFlag] so a misplaced flag
    /// is never silently discarded.
    #[error("query flag `{flag}` does not apply to the {backend} backend")]
    FlagNotApplicable {
        /// The flag name as it appeared in the query string.
        flag: String,
        /// The backend the URL selected, which does not consume `flag`.
        backend: BackendKind,
    },
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
    /// `?bundle_uri_presign_ttl=<seconds>` was supplied without
    /// `?bundle_uri=1`, the flag that gives it meaning: the TTL only
    /// governs presigning of the `bundle.<ref>.uri=` lines, which are
    /// advertised solely when bundle-uri advertising is opted into, so
    /// accepting it without that opt-in would silently discard caller
    /// intent. The engine is intentionally not part of this check — it is
    /// resolved from the bucket `FORMAT` at connect time, not knowable at
    /// URL-parse time. Issue #246.
    #[error("bundle_uri_presign_ttl requires `?bundle_uri=1`; it has no effect otherwise")]
    BundleUriPresignTtlWithoutBundleUri,
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
    reject_inapplicable_presign_ttl(&flags)?;

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
    // Reuse the same vocabulary the URL boolean flags accept so
    // `ALLOW_HTTP=true` and `ALLOW_HTTP=1` behave identically. Anything
    // we cannot parse as a boolean (unset, empty, junk) leaves the
    // gate closed — fail-safe is "no cleartext".
    env::var(ENV_ALLOW_HTTP)
        .ok()
        .as_deref()
        .and_then(parse_bool_value)
        .unwrap_or(false)
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

/// Reject a flag that parsed globally in [`extract_flags`] but does not
/// apply to the backend that the URL ultimately selected.
///
/// `extract_flags` runs before backend dispatch and therefore cannot
/// know whether `?profile=` belongs to an S3 URL or an Azure one. The
/// backend-specific `finish_*` functions own that context, so the
/// cross-backend pairing check lives here and is shared by both. When
/// `present` is true the flag is reported via
/// [`ParseError::FlagNotApplicable`], matching the fail-fast policy used
/// for unknown flags.
fn reject_inapplicable_flag(
    present: bool,
    flag: &str,
    backend: BackendKind,
) -> Result<(), ParseError> {
    if present {
        return Err(ParseError::FlagNotApplicable {
            flag: flag.to_owned(),
            backend,
        });
    }
    Ok(())
}

/// Reject `?bundle_uri_presign_ttl=` when `?bundle_uri=1` is absent — the
/// TTL governs presigning of the emitted `bundle.<ref>.uri=` lines, which
/// are only advertised when bundle-uri advertising is opted into, so a TTL
/// without `?bundle_uri=1` silently discards the operator's intent.
///
/// The check is deliberately gated on `bundle_uri` alone, **not** the
/// engine. The runtime engine is bucket-authoritative — resolved from the
/// `FORMAT` key in `backend::build`, not the URL `?engine=` flag (see
/// `protocol::run`) — so a packchain bucket is routinely reconnected with
/// `?bundle_uri=1&bundle_uri_presign_ttl=<n>` and no `?engine=packchain`.
/// The URL parser runs before `FORMAT` is read and therefore cannot know
/// the engine; gating on the flag would reject that valid steady-state
/// URL. `bundle_uri` is the only precondition the parser can evaluate
/// soundly, and it mirrors how a `?bundle_uri=1` flag is itself merely
/// inert (not rejected) on a non-packchain bucket.
fn reject_inapplicable_presign_ttl(flags: &RemoteFlags) -> Result<(), ParseError> {
    let has_ttl = flags.bundle_uri_presign_ttl.is_some();
    if has_ttl && !flags.bundle_uri {
        return Err(ParseError::BundleUriPresignTtlWithoutBundleUri);
    }
    Ok(())
}

fn parse_bool_flag(name: &str, value: &str) -> Result<bool, ParseError> {
    parse_bool_value(value).ok_or_else(|| ParseError::InvalidFlagValue {
        name: name.to_owned(),
        value: value.to_owned(),
    })
}

/// Single source of truth for boolean-string parsing across the URL
/// query-flag parser and the helper-runtime env-var reads.
///
/// Accepts the conventional "truthy / falsy" vocabulary used by most
/// shells and config files (`1|true|yes|on` for true; `0|false|no|off`
/// for false), all case-insensitively. Returns `None` for any token
/// outside the accepted set so callers can map the failure mode they
/// need (URL flags surface [`ParseError::InvalidFlagValue`]; env-var
/// reads fall back to "unset/false").
///
/// Centralising the vocabulary here (rather than open-coding `matches!`
/// at each read site) prevents the divergence reported in issue #187,
/// where `?zip=true` worked in the URL but `ALLOW_HTTP=true` did not.
fn parse_bool_value(value: &str) -> Option<bool> {
    // One `to_ascii_lowercase` allocation plus a single match,
    // replacing the previous 8-way `eq_ignore_ascii_case` chain (#221).
    // Not a hot path; clarity outweighs the per-call String alloc.
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
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
    // `credential` names an Azure credential alias and is consumed only
    // by the Azure auth path; it is meaningless on S3.
    reject_inapplicable_flag(flags.credential.is_some(), "credential", BackendKind::S3)?;

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
    // `profile` selects an AWS named profile and `region` overrides the
    // AWS SDK region; both are consumed only on the S3 path and have no
    // meaning for Azure.
    reject_inapplicable_flag(flags.profile.is_some(), "profile", BackendKind::Azure)?;
    reject_inapplicable_flag(flags.region.is_some(), "region", BackendKind::Azure)?;

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
mod tests;

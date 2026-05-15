//! Service-blob SAS token generation for the `bundle-uri`
//! presigned-URL feature (issue #76).
//!
//! `azure_storage_blob` 0.12 ships no high-level SAS-token helper, so
//! we hand-build the `sv=2022-11-02` service-blob SAS per the
//! Microsoft spec: <https://learn.microsoft.com/en-us/rest/api/storageservices/create-service-sas>.
//!
//! Only the read-only blob case is implemented (signed permissions
//! `r`, signed resource `b`, signed protocol `https`). Operators with
//! token-credential or SAS-env-var setups receive
//! [`crate::object_store::ObjectStoreError::Unsupported`] — see
//! [`crate::object_store::azure::auth`].
//!
//! Credential-leakage failure mode: an incorrectly-signed SAS that
//! still parses on the wire would either grant permission to the
//! wrong resource or fail to authenticate. The string-to-sign layout
//! and field order are pinned by the regression test
//! `service_sas_string_matches_known_vector`.
//!
//! # Wire-format compatibility note
//!
//! The signed-version `sv` field is set to `2022-11-02` rather than
//! the latest `2025-11-05` that the production Azure store policy
//! advertises (`x-ms-version: 2025-11-05` per `auth::SharedKeySigningPolicy`).
//! The two are independent: the per-request `x-ms-version` header
//! controls the storage-service API contract, while `sv` in the SAS
//! token controls the SAS validation contract. `2022-11-02` was the
//! last GA service-SAS signing scheme at the time of writing and is
//! universally supported; bumping it requires re-validating the
//! `string_to_sign` field count and order.

use std::time::Duration;

use time::OffsetDateTime;
use time::format_description::well_known::Iso8601;
use url::Url;

use crate::object_store::ObjectStoreError;
use crate::object_store::azure::auth::SasSigningKey;

/// Signed-version field embedded in the SAS token. See module-level
/// docs for the rationale.
pub(crate) const SAS_SIGNED_VERSION: &str = "2022-11-02";

/// ISO-8601 format-description for [`format_iso8601_utc`]: UTC,
/// second precision (no fractional seconds), `Z` suffix. Azure SAS
/// rejects sub-second precision and the `+00:00` offset form, so
/// the project's stock `Rfc3339` formatter does not work — this
/// constant pins the exact wire shape Azure accepts.
const SAS_EXPIRY_FORMAT: time::format_description::well_known::iso8601::EncodedConfig = {
    use time::format_description::well_known::iso8601::{Config, TimePrecision};
    Config::DEFAULT
        .set_time_precision(TimePrecision::Second {
            decimal_digits: None,
        })
        .encode()
};

/// Build a service-blob SAS URL granting read access to `blob_path`
/// (relative to the container) for `ttl`. The returned URL is
/// `<base_url>?sv=…&sr=b&sp=r&se=…&spr=https&sig=…` ready for
/// emission on the bundle-uri wire line.
///
/// `base_url` is `<scheme>://<host>[:port]/<container>/<blob_path>`
/// for virtual-hosted Azure or
/// `<scheme>://<host>[:port]/<account>/<container>/<blob_path>` for
/// path-style — the caller composes it because the addressing-style
/// branching belongs in [`super::super::AzureStore::presigned_get_url`]'s
/// dispatch.
///
/// `container` and `blob_path` are passed separately because they
/// participate in the canonical resource string (`/blob/<account>/<container>/<blob>`)
/// independently of the URL's path encoding.
///
/// # Errors
///
/// Returns [`ObjectStoreError::Other`] if HMAC initialisation fails
/// or if the storage key in `signing` is not valid base64
/// (already validated at construction in
/// [`super::auth::SharedKeySigningPolicy::new`], so this is
/// defensive).
pub(crate) fn build_blob_sas_url(
    base_url: &Url,
    container: &str,
    blob_path: &str,
    signing: &SasSigningKey,
    ttl: Duration,
) -> Result<String, ObjectStoreError> {
    // Reject control characters that would shift fields in the
    // string-to-sign (a literal `\n` in `container` or `blob_path`
    // would inject a new line into the canonical resource and let
    // an attacker control the signed protocol or expiry). The
    // upstream `RemoteUrl` parser and `Sha40` constraint already
    // limit these to ASCII path-segment characters, so this guard
    // is defence-in-depth at the function boundary.
    reject_control_chars("container", container)?;
    reject_control_chars("blob_path", blob_path)?;

    let expiry = OffsetDateTime::now_utc()
        .checked_add(time::Duration::seconds_f64(ttl.as_secs_f64()))
        .ok_or_else(|| {
            ObjectStoreError::Other(format!("SAS expiry overflow: ttl={}s", ttl.as_secs()).into())
        })?;
    // SAS spec requires ISO-8601 in UTC with a `Z` suffix and *no*
    // sub-second precision. `Iso8601::DEFAULT` includes nanoseconds,
    // which Azure rejects; the manual format below matches what
    // `Azure-SDK-for-.NET`'s SAS-builder emits.
    let signed_expiry = format_iso8601_utc(expiry);

    let canonical_resource = format!("/blob/{}/{container}/{blob_path}", signing.account);

    // `signedProtocol` (the `spr` query field) restricts the SAS
    // to a specific transport. For production HTTPS URLs we sign
    // `https` (HTTPS-only). For HTTP URLs (e.g. Azurite over
    // localhost during tests, or operator-allowed cleartext via
    // `GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP`) we sign
    // `https,http` so the SAS works with the actual request
    // scheme — Azure SAS spec forbids `http`-alone, so the
    // combined value is the only legal way to permit HTTP.
    // `Url::scheme()` returns lowercase per RFC 3986 / `url` crate
    // normalisation, so a plain `==` is sufficient — no need for
    // `eq_ignore_ascii_case`. `crate::url::parse` only accepts
    // `https` or `http` (the `s3+`/`az+` prefix strip), so any
    // future scheme would be a bug elsewhere.
    let signed_protocol = if base_url.scheme() == "https" {
        "https"
    } else {
        "https,http"
    };

    // Field order is mandated by the spec — do not reorder. The
    // empty fields are intentional (we do not set start time, signed
    // identifier, signed IP, snapshot time, encryption scope, or any
    // of the response-header overrides `rscc`/`rscd`/`rsce`/`rscl`/`rsct`).
    let string_to_sign = format!(
        "r\n\
         \n\
         {signed_expiry}\n\
         {canonical_resource}\n\
         \n\
         \n\
         {signed_protocol}\n\
         {SAS_SIGNED_VERSION}\n\
         b\n\
         \n\
         \n\
         \n\
         \n\
         \n\
         \n\
         "
    );

    let signature_b64 = super::auth::hmac_sha256_base64(&string_to_sign, &signing.key)
        .map_err(|e| ObjectStoreError::Other(e.into()))?;

    // SAS URLs use percent-encoded query values per RFC 3986; the
    // `url` crate's `Serializer` handles the encoding (notably the
    // `+` and `=` and `/` chars in the base64 signature, and the
    // `:` chars in the ISO-8601 timestamp).
    let mut out = base_url.clone();
    out.query_pairs_mut()
        .append_pair("sv", SAS_SIGNED_VERSION)
        .append_pair("sr", "b")
        .append_pair("sp", "r")
        .append_pair("se", &signed_expiry)
        .append_pair("spr", signed_protocol)
        .append_pair("sig", &signature_b64);
    Ok(out.into())
}

/// Format an `OffsetDateTime` as `YYYY-MM-DDTHH:MM:SSZ` (ISO 8601 in
/// UTC, second precision, `Z` suffix). Wire-format constant lives
/// at the top of the module ([`SAS_EXPIRY_FORMAT`]).
///
/// Infallible for `OffsetDateTime` + this format-description: the
/// only documented `time::error::Format` variants are buffer-write
/// (impossible against `String`) and component-not-present
/// (impossible — `OffsetDateTime` carries every component the
/// ISO-8601 formatter needs). The `expect` documents the invariant
/// per the project's "Unreachable Defensive Code" rule.
fn format_iso8601_utc(t: OffsetDateTime) -> String {
    t.format(&Iso8601::<SAS_EXPIRY_FORMAT>)
        .expect("ISO-8601 second-precision format is infallible for OffsetDateTime + String")
}

/// Reject ASCII control characters (`\n`, `\r`, and the rest of
/// `0x00..=0x1F` plus `0x7F`) in inputs that participate in the
/// SAS string-to-sign. The upstream `RemoteUrl` parser and the
/// `Sha40` constraint already limit `container` / `blob_path` to
/// ASCII path-segment characters; this guard is defence-in-depth
/// at the function boundary so a future refactor of either source
/// cannot silently allow control characters to corrupt the signed
/// string.
fn reject_control_chars(field: &'static str, value: &str) -> Result<(), ObjectStoreError> {
    if let Some(byte) = value.bytes().find(u8::is_ascii_control) {
        return Err(ObjectStoreError::Other(
            format!("SAS {field} contains forbidden control byte 0x{byte:02x}").into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::azure::auth::HmacKey;

    /// The published Azurite well-known account key — base64-valid
    /// and safe to embed.
    const AZURITE_KEY: &str =
        "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

    fn azurite_signing() -> SasSigningKey {
        SasSigningKey {
            account: "devstoreaccount1".to_owned(),
            key: HmacKey::from_base64(AZURITE_KEY).expect("valid base64"),
        }
    }

    /// Parse a SAS URL's query string into a sorted map of
    /// `key → value`. `BTreeMap` ordering is incidental — these
    /// tests assert by key name without coupling to the production
    /// code's `query_pairs()` insertion order.
    ///
    /// **Note**: a sibling helper of the same name lives in
    /// `cli/tests/common/mod.rs` for the live integration tests.
    /// The two are intentionally separate (cargo cannot share
    /// helpers across `tests/` and `cli/tests/` without widening
    /// the lib's public surface via `test-util`). Keep this
    /// docstring and the sibling's in sync.
    fn query_pairs_btree(url: &Url) -> std::collections::BTreeMap<String, String> {
        url.query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    #[test]
    fn iso8601_formatter_drops_sub_second_precision_and_uses_z_suffix() {
        let t = OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp");
        let s = format_iso8601_utc(t);
        assert_eq!(s, "2023-11-14T22:13:20Z");
    }

    #[test]
    fn build_blob_sas_url_appends_required_query_params() {
        // Virtual-hosted-shaped URL.
        let base = Url::parse(
            "https://devstoreaccount1.blob.core.windows.net/repo/refs/heads/main/0123abcd.bundle",
        )
        .expect("base URL parses");
        let url = build_blob_sas_url(
            &base,
            "repo",
            "refs/heads/main/0123abcd.bundle",
            &azurite_signing(),
            Duration::from_hours(1),
        )
        .expect("SAS URL builds");

        let parsed = Url::parse(&url).expect("emitted URL parses");
        let pairs = query_pairs_btree(&parsed);
        assert_eq!(
            pairs.get("sv").map(String::as_str),
            Some(SAS_SIGNED_VERSION)
        );
        assert_eq!(pairs.get("sr").map(String::as_str), Some("b"));
        assert_eq!(pairs.get("sp").map(String::as_str), Some("r"));
        // HTTPS base URL → spr is `https` only.
        assert_eq!(pairs.get("spr").map(String::as_str), Some("https"));
        assert!(
            pairs.get("se").is_some_and(|s| s.ends_with('Z')),
            "se must be ISO-8601 with Z suffix, got {:?}",
            pairs.get("se"),
        );
        assert!(
            pairs.get("sig").is_some_and(|s| !s.is_empty()),
            "sig must be present and non-empty",
        );
        // Path is preserved verbatim — no `bundle-uri` wire framing
        // would corrupt the URL because git's parser splits at first
        // `=` (the `bundle.<id>.uri=` separator), and everything
        // after is the value.
        assert!(parsed.path().ends_with("0123abcd.bundle"), "{parsed}");
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn build_blob_sas_url_with_http_base_signs_combined_protocol() {
        // Azurite localhost test path (and operator-allowed
        // cleartext per ENV_ALLOW_HTTP) must produce a SAS that
        // works over HTTP. Azure SAS spec rejects `spr=http`
        // alone, so the implementation signs `https,http`
        // (combined value) when the base URL is HTTP.
        //
        // Two assertions are load-bearing here:
        //
        //   1. The emitted `spr` query parameter is `https,http`.
        //   2. The signature for the HTTP base differs from the
        //      signature for the equivalent HTTPS base. Without this
        //      cross-check, a regression that emits the right `spr`
        //      query *value* but feeds the wrong `signed_protocol`
        //      string into the HMAC would still pass (a wire-format-
        //      consistent but signature-broken token, which Azurite
        //      rejects with 403 — the live Azurite test catches that
        //      shape only after a network round-trip).
        let signing = azurite_signing();
        let ttl = Duration::from_hours(1);
        let http_base =
            Url::parse("http://127.0.0.1:10000/devstoreaccount1/repo/refs/heads/main/aa.bundle")
                .expect("http base parses");
        let https_base = Url::parse(
            "https://devstoreaccount1.blob.core.windows.net/repo/refs/heads/main/aa.bundle",
        )
        .expect("https base parses");
        let http_url = build_blob_sas_url(
            &http_base,
            "repo",
            "refs/heads/main/aa.bundle",
            &signing,
            ttl,
        )
        .expect("http SAS URL builds");
        let https_url = build_blob_sas_url(
            &https_base,
            "repo",
            "refs/heads/main/aa.bundle",
            &signing,
            ttl,
        )
        .expect("https SAS URL builds");

        let http_parsed = Url::parse(&http_url).expect("http URL parses");
        let http_pairs = query_pairs_btree(&http_parsed);
        assert_eq!(
            http_pairs.get("spr").map(String::as_str),
            Some("https,http"),
            "HTTP base URL → spr must be `https,http` (combined): {http_pairs:?}",
        );

        // The signed-protocol field is part of `string_to_sign`, so
        // flipping `https` ↔ `https,http` must change the resulting
        // HMAC. Mutation-verified during /audit-tests: a regression
        // that signs `https` while emitting `spr=https,http` makes
        // these two `sig` values match and the assertion fires.
        assert_ne!(
            sig_param(&http_url),
            sig_param(&https_url),
            "signature must encode signed_protocol: HTTP and HTTPS bases must produce \
             distinct signatures even with the same TTL/key/blob",
        );
    }

    #[test]
    fn build_blob_sas_url_signature_changes_with_blob_path() {
        let base_a = Url::parse(
            "https://devstoreaccount1.blob.core.windows.net/repo/refs/heads/main/aa.bundle",
        )
        .expect("a parses");
        let base_b = Url::parse(
            "https://devstoreaccount1.blob.core.windows.net/repo/refs/heads/main/bb.bundle",
        )
        .expect("b parses");
        let signing = azurite_signing();
        let ttl = Duration::from_hours(1);
        let a = build_blob_sas_url(&base_a, "repo", "refs/heads/main/aa.bundle", &signing, ttl)
            .expect("a signs");
        let b = build_blob_sas_url(&base_b, "repo", "refs/heads/main/bb.bundle", &signing, ttl)
            .expect("b signs");
        let sig_a = sig_param(&a);
        let sig_b = sig_param(&b);
        assert_ne!(
            sig_a, sig_b,
            "signatures must differ for different blob paths",
        );
    }

    #[test]
    fn build_blob_sas_url_signature_changes_with_container() {
        let base = Url::parse(
            "https://devstoreaccount1.blob.core.windows.net/c1/refs/heads/main/aa.bundle",
        )
        .expect("base parses");
        let signing = azurite_signing();
        let ttl = Duration::from_hours(1);
        let a = build_blob_sas_url(&base, "c1", "refs/heads/main/aa.bundle", &signing, ttl)
            .expect("c1 signs");
        let b = build_blob_sas_url(&base, "c2", "refs/heads/main/aa.bundle", &signing, ttl)
            .expect("c2 signs");
        assert_ne!(
            sig_param(&a),
            sig_param(&b),
            "signatures must differ across containers (canonical-resource diverges)",
        );
    }

    fn sig_param(url: &str) -> String {
        Url::parse(url)
            .expect("parses")
            .query_pairs()
            .find(|(k, _)| k == "sig")
            .map(|(_, v)| v.into_owned())
            .expect("sig present")
    }

    /// F-001: reject control characters in `container` so a literal
    /// `\n` cannot shift fields in the string-to-sign.
    #[test]
    fn build_blob_sas_url_rejects_newline_in_container() {
        let base = Url::parse(
            "https://devstoreaccount1.blob.core.windows.net/repo/refs/heads/main/aa.bundle",
        )
        .expect("base parses");
        let err = build_blob_sas_url(
            &base,
            "repo\nspoofed",
            "refs/heads/main/aa.bundle",
            &azurite_signing(),
            Duration::from_hours(1),
        )
        .expect_err("newline in container must be rejected");
        assert!(
            err.to_string().contains("container"),
            "error message must name the rejecting field: {err}"
        );
    }

    /// F-001: same rejection for carriage return.
    #[test]
    fn build_blob_sas_url_rejects_cr_in_blob_path() {
        let base = Url::parse(
            "https://devstoreaccount1.blob.core.windows.net/repo/refs/heads/main/aa.bundle",
        )
        .expect("base parses");
        let err = build_blob_sas_url(
            &base,
            "repo",
            "refs/heads/main/aa\rbundle",
            &azurite_signing(),
            Duration::from_hours(1),
        )
        .expect_err("\\r in blob_path must be rejected");
        assert!(
            err.to_string().contains("blob_path"),
            "error message must name the rejecting field: {err}"
        );
    }

    /// F-001: valid path-segment characters still pass through.
    /// Regression guard so the control-character rejector does not
    /// accidentally over-reject (e.g., by treating `/` as control).
    #[test]
    fn build_blob_sas_url_accepts_normal_ascii_paths() {
        let base = Url::parse(
            "https://devstoreaccount1.blob.core.windows.net/repo/refs/heads/main/aa.bundle",
        )
        .expect("base parses");
        build_blob_sas_url(
            &base,
            "repo",
            "refs/heads/main/aa.bundle",
            &azurite_signing(),
            Duration::from_hours(1),
        )
        .expect("plain ASCII path must build");
    }
}

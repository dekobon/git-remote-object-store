//! Integration tests for `git_remote_object_store::url::parse`.
//!
//! Covers every concrete URL example in the grammar plus negative
//! cases for the validation rules, the `?addressing=` override, and a
//! `proptest` round-trip on the legal grammar.

use std::env;
use std::sync::Mutex;

use git_remote_object_store::url::{
    AzureAddressing, ENV_ALLOW_HTTP, ParseError, RemoteFlags, RemoteUrl, S3Addressing, parse,
};
use proptest::prelude::*;

// Tests that mutate ENV_ALLOW_HTTP must serialize against each other
// AND against any test that reads it via `parse()`. The mutex covers
// only env-touching tests; the rest of the suite never reads env, so
// it can stay parallel.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn with_allow_http_env<R>(value: Option<&str>, f: impl FnOnce() -> R) -> R {
    let lock = ENV_LOCK.lock().expect("ENV_LOCK poisoned");
    let prev = env::var(ENV_ALLOW_HTTP).ok();
    // SAFETY: tests that read ENV_ALLOW_HTTP via `parse()` all acquire
    // ENV_LOCK before calling parse, so no other thread observes the
    // env var while it is being mutated here.
    unsafe {
        match value {
            Some(v) => env::set_var(ENV_ALLOW_HTTP, v),
            None => env::remove_var(ENV_ALLOW_HTTP),
        }
    }
    let result = f();
    // SAFETY: see above; restore the previous value before releasing
    // the lock.
    unsafe {
        match prev {
            Some(p) => env::set_var(ENV_ALLOW_HTTP, p),
            None => env::remove_var(ENV_ALLOW_HTTP),
        }
    }
    drop(lock);
    result
}

// ---------------------------------------------------------------------------
// Positive cases — every concrete example in §3.1
// ---------------------------------------------------------------------------

#[test]
fn s3_virtual_hosted_aws() {
    let url = parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/my-repo").unwrap();
    let RemoteUrl::S3 {
        bucket,
        prefix,
        addressing,
        flags,
        ..
    } = url
    else {
        panic!("expected S3");
    };
    assert_eq!(bucket, "my-bucket");
    assert_eq!(prefix.as_deref(), Some("my-repo"));
    assert_eq!(addressing, S3Addressing::VirtualHosted);
    assert_eq!(flags, RemoteFlags::default());
}

#[test]
fn s3_path_style_aws() {
    let url = parse("s3+https://s3.us-west-2.amazonaws.com/my-bucket/my-repo").unwrap();
    let RemoteUrl::S3 {
        bucket,
        prefix,
        addressing,
        ..
    } = url
    else {
        panic!("expected S3");
    };
    assert_eq!(bucket, "my-bucket");
    assert_eq!(prefix.as_deref(), Some("my-repo"));
    assert_eq!(addressing, S3Addressing::PathStyle);
}

#[test]
fn s3_local_minio() {
    // No env override needed — loopback is always allowed.
    with_allow_http_env(None, || {
        let url = parse("s3+http://localhost:9000/my-bucket/my-repo").unwrap();
        let RemoteUrl::S3 {
            bucket,
            prefix,
            addressing,
            ..
        } = url
        else {
            panic!("expected S3");
        };
        assert_eq!(bucket, "my-bucket");
        assert_eq!(prefix.as_deref(), Some("my-repo"));
        assert_eq!(addressing, S3Addressing::PathStyle);
    });
}

#[test]
fn s3_cloudflare_r2() {
    let url = parse("s3+https://acc-id1234.r2.cloudflarestorage.com/my-bucket/my-repo").unwrap();
    let RemoteUrl::S3 {
        bucket,
        prefix,
        addressing,
        ..
    } = url
    else {
        panic!("expected S3");
    };
    assert_eq!(bucket, "my-bucket");
    assert_eq!(prefix.as_deref(), Some("my-repo"));
    assert_eq!(addressing, S3Addressing::PathStyle);
}

#[test]
fn s3_backblaze_b2() {
    let url = parse("s3+https://s3.us-west-002.backblazeb2.com/my-bucket/my-repo").unwrap();
    let RemoteUrl::S3 {
        bucket,
        prefix,
        addressing,
        ..
    } = url
    else {
        panic!("expected S3");
    };
    assert_eq!(bucket, "my-bucket");
    assert_eq!(prefix.as_deref(), Some("my-repo"));
    assert_eq!(addressing, S3Addressing::PathStyle);
}

#[test]
fn azure_public_cloud() {
    let url = parse("az+https://myaccount.blob.core.windows.net/my-container/my-repo").unwrap();
    let RemoteUrl::Azure {
        account,
        container,
        prefix,
        addressing,
        ..
    } = url
    else {
        panic!("expected Azure");
    };
    assert_eq!(account, "myaccount");
    assert_eq!(container, "my-container");
    assert_eq!(prefix.as_deref(), Some("my-repo"));
    assert_eq!(addressing, AzureAddressing::VirtualHosted);
}

#[test]
fn azure_us_gov_cloud() {
    let url =
        parse("az+https://myaccount.blob.core.usgovcloudapi.net/my-container/my-repo").unwrap();
    let RemoteUrl::Azure {
        account,
        container,
        prefix,
        addressing,
        ..
    } = url
    else {
        panic!("expected Azure");
    };
    assert_eq!(account, "myaccount");
    assert_eq!(container, "my-container");
    assert_eq!(prefix.as_deref(), Some("my-repo"));
    assert_eq!(addressing, AzureAddressing::VirtualHosted);
}

#[test]
fn azure_azurite_path_style() {
    with_allow_http_env(None, || {
        let url = parse("az+http://127.0.0.1:10000/devstoreaccount1/my-container/my-repo").unwrap();
        let RemoteUrl::Azure {
            account,
            container,
            prefix,
            addressing,
            ..
        } = url
        else {
            panic!("expected Azure");
        };
        assert_eq!(account, "devstoreaccount1");
        assert_eq!(container, "my-container");
        assert_eq!(prefix.as_deref(), Some("my-repo"));
        assert_eq!(addressing, AzureAddressing::PathStyle);
    });
}

#[test]
fn s3_zip_flag() {
    let url = parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/my-repo?zip=1").unwrap();
    assert!(url.flags().zip);
    assert_eq!(url.flags().profile, None);
}

#[test]
fn s3_all_flags() {
    let url = parse(
        "s3+https://my-bucket.s3.us-west-2.amazonaws.com/my-repo\
         ?zip=true&profile=prod&region=us-east-1",
    )
    .unwrap();
    assert!(url.flags().zip);
    assert_eq!(url.flags().profile.as_deref(), Some("prod"));
    assert_eq!(url.flags().region.as_deref(), Some("us-east-1"));
}

#[test]
fn azure_credential_flag() {
    let url = parse(
        "az+https://myaccount.blob.core.windows.net/my-container/repo\
         ?credential=ci-cd",
    )
    .unwrap();
    assert_eq!(url.flags().credential.as_deref(), Some("ci-cd"));
}

#[test]
fn missing_prefix_is_allowed_virtual() {
    let url = parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com").unwrap();
    let RemoteUrl::S3 { bucket, prefix, .. } = url else {
        panic!("expected S3");
    };
    assert_eq!(bucket, "my-bucket");
    assert_eq!(prefix, None);
}

#[test]
fn missing_prefix_is_allowed_path_style() {
    let url = parse("s3+https://s3.us-west-2.amazonaws.com/my-bucket").unwrap();
    let RemoteUrl::S3 { bucket, prefix, .. } = url else {
        panic!("expected S3");
    };
    assert_eq!(bucket, "my-bucket");
    assert_eq!(prefix, None);
}

#[test]
fn trailing_slash_on_prefix_is_stripped() {
    let url = parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/my-repo/").unwrap();
    assert_eq!(url.prefix(), Some("my-repo"));
}

#[test]
fn nested_prefix_is_joined() {
    let url = parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/team/repo").unwrap();
    assert_eq!(url.prefix(), Some("team/repo"));
}

// ---------------------------------------------------------------------------
// Addressing override (§3.4)
// ---------------------------------------------------------------------------

#[test]
fn addressing_override_forces_path_on_virtual_host() {
    // Hostname looks virtual-hosted (`<bucket>.s3.…`) but we override
    // to path-style; the first hostname label is no longer treated as
    // a bucket and the first path segment becomes the bucket.
    let url =
        parse("s3+https://example.s3.us-west-2.amazonaws.com/my-bucket/my-repo?addressing=path")
            .unwrap();
    let RemoteUrl::S3 {
        bucket,
        addressing,
        prefix,
        ..
    } = url
    else {
        panic!("expected S3");
    };
    assert_eq!(addressing, S3Addressing::PathStyle);
    assert_eq!(bucket, "my-bucket");
    assert_eq!(prefix.as_deref(), Some("my-repo"));
}

#[test]
fn addressing_override_forces_virtual_on_path_host() {
    // Use a hostname that doesn't match the `s3` heuristic but the
    // user knows the endpoint follows a virtual-hosted convention.
    let url = parse("s3+https://my-bucket.minio.example.com/my-repo?addressing=virtual").unwrap();
    let RemoteUrl::S3 {
        bucket,
        addressing,
        prefix,
        ..
    } = url
    else {
        panic!("expected S3");
    };
    assert_eq!(addressing, S3Addressing::VirtualHosted);
    assert_eq!(bucket, "my-bucket");
    assert_eq!(prefix.as_deref(), Some("my-repo"));
}

#[test]
fn azure_addressing_override_path() {
    let url = parse(
        "az+https://myaccount.blob.core.windows.net/myacct1/my-container/my-repo\
         ?addressing=path",
    )
    .unwrap();
    let RemoteUrl::Azure {
        account,
        container,
        prefix,
        addressing,
        ..
    } = url
    else {
        panic!("expected Azure");
    };
    assert_eq!(addressing, AzureAddressing::PathStyle);
    assert_eq!(account, "myacct1");
    assert_eq!(container, "my-container");
    assert_eq!(prefix.as_deref(), Some("my-repo"));
}

// ---------------------------------------------------------------------------
// Negative cases — §3.5
// ---------------------------------------------------------------------------

#[test]
fn rejects_https_without_backend_prefix() {
    let err = parse("https://my-bucket.s3.us-west-2.amazonaws.com/my-repo").unwrap_err();
    assert!(matches!(err, ParseError::UnsupportedScheme(s) if s == "https"));
}

#[test]
fn rejects_ftp() {
    let err = parse("ftp://example.com/").unwrap_err();
    assert!(matches!(err, ParseError::UnsupportedScheme(s) if s == "ftp"));
}

#[test]
fn rejects_cleartext_http_to_non_loopback_without_env() {
    with_allow_http_env(None, || {
        let err = parse("s3+http://example.com/my-bucket/my-repo").unwrap_err();
        assert!(matches!(err, ParseError::CleartextHttpForbidden { .. }));
    });
}

#[test]
fn allows_cleartext_http_to_non_loopback_with_env() {
    with_allow_http_env(Some("1"), || {
        let url = parse("s3+http://example.com/my-bucket/my-repo").unwrap();
        let RemoteUrl::S3 { bucket, .. } = url else {
            panic!("expected S3");
        };
        assert_eq!(bucket, "my-bucket");
    });
}

#[test]
fn env_value_other_than_one_does_not_unlock() {
    with_allow_http_env(Some("yes"), || {
        let err = parse("s3+http://example.com/my-bucket/my-repo").unwrap_err();
        assert!(matches!(err, ParseError::CleartextHttpForbidden { .. }));
    });
}

#[test]
fn ipv6_loopback_allows_cleartext() {
    with_allow_http_env(None, || {
        let url = parse("s3+http://[::1]:9000/my-bucket/my-repo").unwrap();
        let RemoteUrl::S3 { bucket, .. } = url else {
            panic!("expected S3");
        };
        assert_eq!(bucket, "my-bucket");
    });
}

#[test]
fn rejects_uppercase_bucket() {
    let err = parse("s3+https://s3.us-west-2.amazonaws.com/MyBucket/repo").unwrap_err();
    assert!(matches!(err, ParseError::InvalidBucket(s) if s == "MyBucket"));
}

#[test]
fn rejects_too_short_bucket() {
    let err = parse("s3+https://s3.us-west-2.amazonaws.com/ab/repo").unwrap_err();
    assert!(matches!(err, ParseError::InvalidBucket(s) if s == "ab"));
}

#[test]
fn rejects_bucket_starting_with_dash() {
    let err = parse("s3+https://s3.us-west-2.amazonaws.com/-bucket/repo").unwrap_err();
    assert!(matches!(err, ParseError::InvalidBucket(s) if s == "-bucket"));
}

#[test]
fn rejects_missing_bucket() {
    let err = parse("s3+https://s3.us-west-2.amazonaws.com/").unwrap_err();
    assert!(matches!(err, ParseError::MissingBucket));
}

#[test]
fn rejects_missing_container() {
    let url = "az+https://myaccount.blob.core.windows.net/";
    let err = parse(url).unwrap_err();
    assert!(matches!(err, ParseError::MissingContainer));
}

#[test]
fn rejects_missing_account_path_style() {
    with_allow_http_env(None, || {
        let err = parse("az+http://127.0.0.1:10000/").unwrap_err();
        assert!(matches!(err, ParseError::MissingAccount));
    });
}

#[test]
fn rejects_invalid_account_charset() {
    let err = parse("az+https://has-hyphen.blob.core.windows.net/my-container/repo").unwrap_err();
    assert!(matches!(err, ParseError::InvalidAccount(s) if s == "has-hyphen"));
}

#[test]
fn rejects_invalid_container_charset() {
    let err = parse("az+https://myaccount.blob.core.windows.net/UPPER/repo").unwrap_err();
    assert!(matches!(err, ParseError::InvalidContainer(s) if s == "UPPER"));
}

#[test]
fn rejects_forbidden_bucket_prefixes_path_style() {
    for bad in ["xn--abcdef", "sthree-foo", "amzn-s3-demo-bucket"] {
        let url = format!("s3+https://s3.us-west-2.amazonaws.com/{bad}/repo");
        let err = parse(&url).unwrap_err();
        assert!(
            matches!(&err, ParseError::InvalidBucket(s) if s == bad),
            "expected InvalidBucket({bad}), got {err:?}"
        );
    }
}

#[test]
fn rejects_forbidden_bucket_suffixes_path_style() {
    for bad in [
        "my-bucket-s3alias",
        "my-bucket--ol-s3",
        "my-bucket--x-s3",
        "my-bucket--table-s3",
        "ab.mrap",
    ] {
        let url = format!("s3+https://s3.us-west-2.amazonaws.com/{bad}/repo");
        let err = parse(&url).unwrap_err();
        assert!(
            matches!(&err, ParseError::InvalidBucket(s) if s == bad),
            "expected InvalidBucket({bad}), got {err:?}"
        );
    }
}

#[test]
fn rejects_bucket_formatted_as_ipv4() {
    let err = parse("s3+https://s3.us-west-2.amazonaws.com/192.168.1.1/repo").unwrap_err();
    assert!(matches!(err, ParseError::InvalidBucket(s) if s == "192.168.1.1"));
}

#[test]
fn rejects_bucket_with_consecutive_periods() {
    let err = parse("s3+https://s3.us-west-2.amazonaws.com/ab..cd/repo").unwrap_err();
    assert!(matches!(err, ParseError::InvalidBucket(s) if s == "ab..cd"));
}

#[test]
fn rejects_bucket_ending_with_dash() {
    let err = parse("s3+https://s3.us-west-2.amazonaws.com/bucket-/repo").unwrap_err();
    assert!(matches!(err, ParseError::InvalidBucket(s) if s == "bucket-"));
}

#[test]
fn rejects_container_with_leading_dash() {
    let err = parse("az+https://myaccount.blob.core.windows.net/-leading/repo").unwrap_err();
    assert!(matches!(err, ParseError::InvalidContainer(s) if s == "-leading"));
}

#[test]
fn rejects_container_with_trailing_dash() {
    let err = parse("az+https://myaccount.blob.core.windows.net/trailing-/repo").unwrap_err();
    assert!(matches!(err, ParseError::InvalidContainer(s) if s == "trailing-"));
}

#[test]
fn rejects_container_with_consecutive_dashes() {
    let err = parse("az+https://myaccount.blob.core.windows.net/foo--bar/repo").unwrap_err();
    assert!(matches!(err, ParseError::InvalidContainer(s) if s == "foo--bar"));
}

#[test]
fn rejects_unknown_flag() {
    let err = parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?bogus=1").unwrap_err();
    assert!(matches!(err, ParseError::UnknownFlag(s) if s == "bogus"));
}

#[test]
fn rejects_invalid_zip_value() {
    let err = parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?zip=yes").unwrap_err();
    assert!(matches!(
        err,
        ParseError::InvalidFlagValue { name, value } if name == "zip" && value == "yes"
    ));
}

#[test]
fn rejects_unknown_addressing() {
    let err =
        parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?addressing=weird").unwrap_err();
    assert!(matches!(err, ParseError::UnknownAddressing(s) if s == "weird"));
}

#[test]
fn rejects_empty_input() {
    assert_eq!(parse(""), Err(ParseError::Empty));
    assert_eq!(parse("   "), Err(ParseError::Empty));
}

// ---------------------------------------------------------------------------
// Display round-trip
// ---------------------------------------------------------------------------

#[test]
fn display_round_trip_concrete() {
    let inputs = [
        "s3+https://my-bucket.s3.us-west-2.amazonaws.com/my-repo",
        "s3+https://s3.us-west-2.amazonaws.com/my-bucket/my-repo",
        "s3+https://my-bucket.s3.us-west-2.amazonaws.com/my-repo?zip=1",
        "az+https://myaccount.blob.core.windows.net/my-container/my-repo",
    ];
    for input in inputs {
        let parsed = parse(input).expect(input);
        let displayed = parsed.to_string();
        let reparsed = parse(&displayed).expect(&displayed);
        assert_eq!(parsed, reparsed, "round-trip mismatch for `{input}`");
    }
}

// ---------------------------------------------------------------------------
// Property-based round-trip
// ---------------------------------------------------------------------------

/// AWS-reserved bucket-name prefixes mirrored from `src/url.rs` so the
/// proptest generator can avoid emitting them. Kept independent of the
/// production constants on purpose: the proptest is meant to exercise
/// the production validator, not borrow its definitions.
const FORBIDDEN_BUCKET_PREFIXES: &[&str] = &["xn--", "sthree-", "amzn-s3-demo-"];

/// AWS-reserved bucket-name suffixes (see comment above).
const FORBIDDEN_BUCKET_SUFFIXES: &[&str] = &["-s3alias", "--ol-s3", "--x-s3", "--table-s3"];

/// S3 bucket strategy that excludes `.` so the bucket is a single
/// hostname label, requires the first and last bytes to be alphanumeric
/// per AWS rules, and filters out the AWS reserved prefixes/suffixes so
/// every generated value is a valid AWS bucket name.
fn arb_bucket() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-z0-9][a-z0-9-]{1,28}[a-z0-9]")
        .expect("valid bucket regex")
        .prop_filter("excludes AWS reserved prefix/suffix", |s| {
            !FORBIDDEN_BUCKET_PREFIXES.iter().any(|p| s.starts_with(p))
                && !FORBIDDEN_BUCKET_SUFFIXES.iter().any(|p| s.ends_with(p))
        })
}

fn arb_account() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-z0-9]{3,24}").expect("valid account regex")
}

/// Azure container strategy: alphanumeric bookends, no consecutive
/// hyphens, lowercase only.
fn arb_container() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-z0-9][a-z0-9-]{1,28}[a-z0-9]")
        .expect("valid container regex")
        .prop_filter("no consecutive hyphens", |s| !s.contains("--"))
}

fn arb_prefix() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        proptest::string::string_regex("[a-z0-9][a-z0-9_-]{0,16}")
            .expect("prefix regex")
            .prop_map(Some),
        (
            proptest::string::string_regex("[a-z0-9]{1,8}").expect("seg regex"),
            proptest::string::string_regex("[a-z0-9]{1,8}").expect("seg regex"),
        )
            .prop_map(|(a, b)| Some(format!("{a}/{b}"))),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn s3_virtual_hosted_round_trip(
        bucket in arb_bucket(),
        prefix in arb_prefix(),
        zip in any::<bool>(),
    ) {
        let prefix_part = prefix.as_deref().map_or(String::new(), |p| format!("/{p}"));
        let zip_part = if zip { "?zip=1" } else { "" };
        let input = format!(
            "s3+https://{bucket}.s3.us-west-2.amazonaws.com{prefix_part}{zip_part}"
        );
        let parsed = parse(&input).expect("valid input");
        let displayed = parsed.to_string();
        let reparsed = parse(&displayed).expect("display output should re-parse");
        prop_assert_eq!(parsed, reparsed);
    }

    #[test]
    fn s3_path_style_round_trip(
        bucket in arb_bucket(),
        prefix in arb_prefix(),
    ) {
        let prefix_part = prefix.as_deref().map_or(String::new(), |p| format!("/{p}"));
        let input = format!("s3+https://s3.us-west-2.amazonaws.com/{bucket}{prefix_part}");
        let parsed = parse(&input).expect("valid input");
        let reparsed = parse(&parsed.to_string()).expect("display output should re-parse");
        prop_assert_eq!(parsed, reparsed);
    }

    #[test]
    fn azure_virtual_hosted_round_trip(
        account in arb_account(),
        container in arb_container(),
        prefix in arb_prefix(),
    ) {
        let prefix_part = prefix.as_deref().map_or(String::new(), |p| format!("/{p}"));
        let input = format!(
            "az+https://{account}.blob.core.windows.net/{container}{prefix_part}"
        );
        let parsed = parse(&input).expect("valid input");
        let reparsed = parse(&parsed.to_string()).expect("display output should re-parse");
        prop_assert_eq!(parsed, reparsed);
    }

    #[test]
    fn azure_path_style_round_trip(
        account in arb_account(),
        container in arb_container(),
        prefix in arb_prefix(),
    ) {
        // Azurite-style: loopback host, path-style addressing. No env
        // mutation needed because 127.0.0.1 is always allowed.
        let prefix_part = prefix.as_deref().map_or(String::new(), |p| format!("/{p}"));
        let input = format!(
            "az+http://127.0.0.1:10000/{account}/{container}{prefix_part}"
        );
        let parsed = parse(&input).expect("valid input");
        let reparsed = parse(&parsed.to_string()).expect("display output should re-parse");
        prop_assert_eq!(parsed, reparsed);
    }
}

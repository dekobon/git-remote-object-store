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
    let url = parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=bundle").unwrap();
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
    let err = parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=").unwrap_err();
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
    let url =
        parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain&bundle_uri=1")
            .unwrap();
    assert!(url.flags().bundle_uri);
}

#[test]
fn bundle_uri_flag_zero_sets_false() {
    let url =
        parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain&bundle_uri=0")
            .unwrap();
    assert!(!url.flags().bundle_uri);
}

// --- bundle_uri_presign_ttl flag (issue #76) -------------------------

#[test]
fn bundle_uri_presign_ttl_absent_defaults_to_none() {
    let url =
        parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain&bundle_uri=1")
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
        Some(NonZeroU64::new(MAX_BUNDLE_URI_PRESIGN_TTL_SECONDS).expect("7-day cap is non-zero")),
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
    let url = parse("az+https://myaccount.blob.core.windows.net/my-container/repo?engine=bundle")
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
    let err = parse("s3+https://corp.amazonaws.com/my-bucket/repo?addressing=path").unwrap_err();
    assert!(
        matches!(err, ParseError::InvalidAwsS3Endpoint { ref host } if host == "corp.amazonaws.com"),
        "expected InvalidAwsS3Endpoint, got {err:?}",
    );
    let err = parse("s3+https://corp.amazonaws.com/my-bucket/repo?addressing=virtual").unwrap_err();
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

// --- Boolean-value vocabulary (issue #187) ----------------------------
//
// The same `parse_bool_value` helper governs both URL query flags
// (`?zip=`, `?bundle_uri=`) and env-var booleans (`ALLOW_HTTP`).
// The matrix below pins the accepted set so the two surfaces stay
// in sync.

#[test]
fn parse_bool_value_accepts_truthy_tokens() {
    for v in ["1", "true", "yes", "on"] {
        assert_eq!(parse_bool_value(v), Some(true), "expected true for `{v}`");
    }
}

#[test]
fn parse_bool_value_accepts_falsy_tokens() {
    for v in ["0", "false", "no", "off"] {
        assert_eq!(parse_bool_value(v), Some(false), "expected false for `{v}`");
    }
}

#[test]
fn parse_bool_value_is_case_insensitive() {
    // Per-value matrix covering common mixed-case spellings users
    // type ad-hoc. The helper must accept every casing for every
    // accepted token; this loop checks the full cross product.
    for (input, expected) in [
        ("TRUE", true),
        ("True", true),
        ("tRuE", true),
        ("YES", true),
        ("Yes", true),
        ("ON", true),
        ("On", true),
        ("FALSE", false),
        ("False", false),
        ("NO", false),
        ("No", false),
        ("OFF", false),
        ("Off", false),
    ] {
        assert_eq!(
            parse_bool_value(input),
            Some(expected),
            "expected {expected} for `{input}`",
        );
    }
}

#[test]
fn parse_bool_value_rejects_unknown_tokens() {
    // Empty string, near-misses, common typos, and arbitrary
    // junk must all fall through to `None` so the URL-flag path
    // can surface `InvalidFlagValue` and the env-var path can
    // fall back to "unset". Picking "y"/"n" as rejected pins the
    // policy: short forms are NOT accepted (issue #187 left this
    // explicit to avoid surprising aliases).
    for v in [
        "", " ", "yep", "nope", "2", "-1", "truee", "y", "n", "enabled",
    ] {
        assert_eq!(parse_bool_value(v), None, "expected None for `{v}`");
    }
}

#[test]
fn parse_bool_flag_propagates_invalid_flag_value_error() {
    // Names propagated into the error must match the flag the
    // user typed so the diagnostic stays useful.
    let err = parse_bool_flag("zip", "maybe").unwrap_err();
    assert!(
        matches!(&err, ParseError::InvalidFlagValue { name, value }
            if name == "zip" && value == "maybe"),
        "expected InvalidFlagValue(zip, maybe), got {err:?}",
    );
}

#[test]
fn url_bool_flags_accept_mixed_case_and_extended_vocabulary() {
    // Per-value coverage at the URL surface: `?zip=` and
    // `?bundle_uri=` must accept every truthy / falsy token the
    // helper recognises. Loopback host keeps this independent of
    // the AWS-endpoint validator.
    for v in ["1", "true", "True", "TRUE", "yes", "Yes", "on", "ON"] {
        let url = parse(&format!("s3+https://localhost/my-bucket/repo?zip={v}")).unwrap();
        assert!(url.flags().zip, "expected zip=true for `{v}`");
    }
    for v in ["0", "false", "False", "FALSE", "no", "No", "off", "OFF"] {
        let url = parse(&format!("s3+https://localhost/my-bucket/repo?zip={v}")).unwrap();
        assert!(!url.flags().zip, "expected zip=false for `{v}`");
    }
}

#[test]
fn url_bool_flags_reject_unknown_value_with_flag_name() {
    let err = parse("s3+https://localhost/my-bucket/repo?zip=maybe").unwrap_err();
    assert!(
        matches!(&err, ParseError::InvalidFlagValue { name, value }
            if name == "zip" && value == "maybe"),
        "expected InvalidFlagValue(zip, maybe), got {err:?}",
    );

    let err = parse("s3+https://localhost/my-bucket/repo?bundle_uri=2").unwrap_err();
    assert!(
        matches!(&err, ParseError::InvalidFlagValue { name, value }
            if name == "bundle_uri" && value == "2"),
        "expected InvalidFlagValue(bundle_uri, 2), got {err:?}",
    );
}

// --- backend-specific flag pairing (issue #245) ----------------------

#[test]
fn azure_url_rejects_s3_only_profile_flag() {
    let err = parse("az+https://myaccount.blob.core.windows.net/my-container/repo?profile=prod")
        .unwrap_err();
    assert!(
        matches!(&err, ParseError::FlagNotApplicable { flag, backend }
            if flag == "profile" && *backend == BackendKind::Azure),
        "expected FlagNotApplicable(profile, Azure), got {err:?}",
    );
}

#[test]
fn azure_url_rejects_s3_only_region_flag() {
    let err =
        parse("az+https://myaccount.blob.core.windows.net/my-container/repo?region=us-east-1")
            .unwrap_err();
    assert!(
        matches!(&err, ParseError::FlagNotApplicable { flag, backend }
            if flag == "region" && *backend == BackendKind::Azure),
        "expected FlagNotApplicable(region, Azure), got {err:?}",
    );
}

#[test]
fn s3_url_rejects_azure_only_credential_flag() {
    let err =
        parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?credential=ci-cd").unwrap_err();
    assert!(
        matches!(&err, ParseError::FlagNotApplicable { flag, backend }
            if flag == "credential" && *backend == BackendKind::S3),
        "expected FlagNotApplicable(credential, S3), got {err:?}",
    );
}

#[test]
fn inapplicable_flag_rejected_even_with_empty_value() {
    // An empty value still records `Some("")`, so the pairing check
    // must fire — fail-fast does not depend on the value being
    // non-empty.
    let err =
        parse("az+https://myaccount.blob.core.windows.net/my-container/repo?profile=").unwrap_err();
    assert!(
        matches!(&err, ParseError::FlagNotApplicable { flag, backend }
            if flag == "profile" && *backend == BackendKind::Azure),
        "expected FlagNotApplicable(profile, Azure), got {err:?}",
    );
}

#[test]
fn flag_not_applicable_message_names_flag_and_backend() {
    let err =
        parse("az+https://myaccount.blob.core.windows.net/my-container/repo?region=us-east-1")
            .unwrap_err();
    let rendered = err.to_string();
    assert!(
        rendered.contains("`region`") && rendered.contains("Azure"),
        "message must name the flag and backend, got `{rendered}`",
    );
}

#[test]
fn valid_backend_flag_pairings_still_parse() {
    // S3 consumes `profile` and `region`; Azure consumes
    // `credential`. The pairing check must leave these untouched.
    let s3 = parse(
        "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
         ?profile=prod&region=us-east-1",
    )
    .unwrap();
    assert_eq!(s3.flags().profile.as_deref(), Some("prod"));
    assert_eq!(s3.flags().region.as_deref(), Some("us-east-1"));
    assert_eq!(s3.flags().credential, None);

    let azure =
        parse("az+https://myaccount.blob.core.windows.net/my-container/repo?credential=ci-cd")
            .unwrap();
    assert_eq!(azure.flags().credential.as_deref(), Some("ci-cd"));
    assert_eq!(azure.flags().profile, None);
    assert_eq!(azure.flags().region, None);
}

// --- bundle_uri_presign_ttl cross-flag validation (issue #246) -------
//
// The TTL only governs presigning of the `bundle.<ref>.uri=` lines,
// which are advertised only when `?bundle_uri=1` is set. Supplying the
// TTL without `?bundle_uri=1` is a no-op and must be rejected at the
// URL boundary rather than stored and silently ignored. The engine is
// NOT gated here: it is resolved from the bucket `FORMAT` at connect
// time, so a URL carrying `?bundle_uri=1` and the TTL must parse
// regardless of the `?engine=` flag (or its absence).

#[test]
fn presign_ttl_without_bundle_uri_rejected() {
    // packchain engine, but bundle_uri is unset (defaults to false).
    let err = parse(
        "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
         ?engine=packchain&bundle_uri_presign_ttl=3600",
    )
    .unwrap_err();
    assert_eq!(err, ParseError::BundleUriPresignTtlWithoutBundleUri);
}

#[test]
fn presign_ttl_with_bundle_uri_disabled_rejected() {
    // packchain engine and the TTL, but bundle_uri is explicitly off.
    let err = parse(
        "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
         ?engine=packchain&bundle_uri=0&bundle_uri_presign_ttl=3600",
    )
    .unwrap_err();
    assert_eq!(err, ParseError::BundleUriPresignTtlWithoutBundleUri);
}

#[test]
fn presign_ttl_with_explicit_bundle_engine_still_parses() {
    // `?engine=bundle` is the URL flag, but the runtime engine is
    // resolved from the bucket `FORMAT`, not the flag — the same
    // bucket may be packchain. The parser cannot know, so as long as
    // `?bundle_uri=1` is present it must accept the TTL (a bundle
    // bucket simply leaves it inert downstream, exactly as it does
    // the `?bundle_uri=1` flag itself). Rejecting here would break a
    // valid packchain reconnect that happens to carry `?engine=bundle`.
    let url = parse(
        "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
         ?engine=bundle&bundle_uri=1&bundle_uri_presign_ttl=3600",
    )
    .unwrap();
    assert_eq!(
        url.flags().bundle_uri_presign_ttl,
        Some(NonZeroU64::new(3600).unwrap())
    );
}

#[test]
fn presign_ttl_without_engine_flag_still_parses() {
    // Regression for the steady-state packchain reconnect: a packchain
    // bucket is routinely connected with `?bundle_uri=1` and the TTL
    // but no `?engine=packchain` (the engine comes from `FORMAT`). The
    // URL parser must not reject this — gating the TTL on the URL
    // engine flag would abort every fetch/push on such a bucket.
    let url = parse(
        "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
         ?bundle_uri=1&bundle_uri_presign_ttl=3600",
    )
    .unwrap();
    assert_eq!(url.flags().engine, None);
    assert!(url.flags().bundle_uri);
    assert_eq!(
        url.flags().bundle_uri_presign_ttl,
        Some(NonZeroU64::new(3600).unwrap())
    );
}

#[test]
fn presign_ttl_with_packchain_and_bundle_uri_parses() {
    // The one valid configuration: packchain + bundle_uri=1 + TTL.
    let url = parse(
        "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
         ?engine=packchain&bundle_uri=1&bundle_uri_presign_ttl=3600",
    )
    .unwrap();
    assert_eq!(url.flags().engine, Some(StorageEngine::Packchain));
    assert!(url.flags().bundle_uri);
    assert_eq!(
        url.flags().bundle_uri_presign_ttl,
        Some(NonZeroU64::new(3600).unwrap())
    );
}

#[test]
fn packchain_and_bundle_uri_without_ttl_parses() {
    // The TTL is optional: the valid pairing must still parse when it
    // is absent, leaving the field None.
    let url = parse(
        "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo\
         ?engine=packchain&bundle_uri=1",
    )
    .unwrap();
    assert_eq!(url.flags().bundle_uri_presign_ttl, None);
}

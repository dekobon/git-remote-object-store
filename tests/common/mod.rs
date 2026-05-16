//! Shared helpers for protocol integration tests.
//!
//! Cargo does not treat `tests/common/mod.rs` as a test target, so
//! each `tests/protocol_*.rs` file can `mod common;` to pull these in.
//!
//! The cross-crate helpers (git CLI shellouts, in-process REPL driver,
//! seed-repo factory) live in
//! [`git_remote_object_store::test_util`] so the cli crate's
//! integration tests share the same source of truth; this module
//! re-exports them alongside test-suite-local helpers (the
//! MockStore-specific URL builders and the annotated-tag / tag-of-tag
//! seed factories) that only the lib tests need.

// Each integration-test crate compiles this module independently, so
// helpers used by one test file but not another would trigger warnings.
// The `pub use` re-export of `test_util` items hits `unused_imports`
// for the same per-target reason — a test crate that uses `drive_in`
// but not `git_available` would otherwise warn on the re-export.
#![allow(dead_code, unused_imports)]

use git_remote_object_store::url::{self, RemoteUrl};

pub use git_remote_object_store::test_util::{
    drive_in, git, git_available, git_capture, make_seed_repo,
};

/// Build a test [`RemoteUrl`] pointing at a fake S3 bucket.
pub fn s3_url(prefix: Option<&str>) -> RemoteUrl {
    s3_url_with_zip(prefix, false)
}

/// Build a test [`RemoteUrl`] with an optional `?zip=1` query parameter.
pub fn s3_url_with_zip(prefix: Option<&str>, zip: bool) -> RemoteUrl {
    let mut raw = match prefix {
        Some(p) => format!("s3+https://my-bucket.s3.us-west-2.amazonaws.com/{p}"),
        None => "s3+https://my-bucket.s3.us-west-2.amazonaws.com/".to_string(),
    };
    if zip {
        raw.push_str("?zip=1");
    }
    url::parse(&raw).expect("test URL must parse")
}

/// Initialise a fresh repo with one commit and an annotated tag
/// `<tag_name>` pointing at it. Returns `(dir, commit_sha, tag_sha)`.
/// The annotated tag creates a tag-object (not a lightweight tag), so
/// `tag_sha != commit_sha`.
pub fn make_seed_repo_with_annotated_tag(
    label: &str,
    tag_name: &str,
) -> (tempfile::TempDir, String, String) {
    let (dir, shas) = make_seed_repo(1, label);
    git(
        &[
            "tag",
            "-a",
            tag_name,
            "-m",
            "release",
            "--no-sign",
            shas[0].as_str(),
        ],
        dir.path(),
    );
    let tag_sha = git_capture(&["rev-parse", tag_name], dir.path())
        .trim()
        .to_owned();
    assert_ne!(
        tag_sha, shas[0],
        "annotated tag must have its own object SHA",
    );
    (dir, shas[0].clone(), tag_sha)
}

/// Initialise a fresh repo with one commit and a tag-of-tag chain:
/// `<outer_name>` (annotated) → `<inner_name>` (annotated) → commit.
/// Returns `(dir, commit_sha, inner_tag_sha, outer_tag_sha)`.
pub fn make_seed_repo_with_tag_of_tag(
    label: &str,
    inner_name: &str,
    outer_name: &str,
) -> (tempfile::TempDir, String, String, String) {
    let (dir, shas) = make_seed_repo(1, label);
    git(
        &[
            "tag",
            "-a",
            inner_name,
            "-m",
            "inner",
            "--no-sign",
            shas[0].as_str(),
        ],
        dir.path(),
    );
    let inner_sha = git_capture(&["rev-parse", inner_name], dir.path())
        .trim()
        .to_owned();
    // `git tag -a v1 inner` creates a tag-of-tag (CLI git resolves the
    // arg's OID without peeling).
    git(
        &[
            "tag",
            "-a",
            outer_name,
            "-m",
            "outer",
            "--no-sign",
            inner_sha.as_str(),
        ],
        dir.path(),
    );
    let outer_sha = git_capture(&["rev-parse", outer_name], dir.path())
        .trim()
        .to_owned();
    assert_ne!(inner_sha, outer_sha, "outer must wrap inner");
    (dir, shas[0].clone(), inner_sha, outer_sha)
}

/// Build a `?engine=packchain` URL pointing at a fake S3 bucket.
pub fn s3_url_packchain(prefix: Option<&str>) -> RemoteUrl {
    let raw = match prefix {
        Some(p) => {
            format!("s3+https://my-bucket.s3.us-west-2.amazonaws.com/{p}?engine=packchain")
        }
        None => "s3+https://my-bucket.s3.us-west-2.amazonaws.com/?engine=packchain".to_owned(),
    };
    url::parse(&raw).expect("packchain test URL must parse")
}

//! Phase 6 smoke test: drive [`protocol::run`] in-process via
//! `tokio::io::duplex`, with a [`MockStore`] standing in for the cloud
//! backend. Every claim from the upstream Python `cmd_list`/`cmd_capabilities`
//! port has a matching assertion here.
//!
//! Binary-spawn integration tests (real `git-remote-s3-https` against
//! RustFS) are deferred until Phase 7's fetch handler exists — there is
//! nothing for git to do today besides `ls-remote`, which the in-process
//! tests below cover deterministically.

#![cfg(feature = "test-util")]

use std::sync::Arc;

use bytes::Bytes;
use git_remote_object_store::object_store::mock::MockStore;
use git_remote_object_store::object_store::{ObjectStore, PutOpts};
use git_remote_object_store::protocol::{ProtocolError, run};
use git_remote_object_store::url::{self, RemoteUrl};
use time::Duration;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;

const SHA_A: &str = "0000000000000000000000000000000000000001";
const SHA_B: &str = "0000000000000000000000000000000000000002";
const SHA_C: &str = "0000000000000000000000000000000000000003";

fn s3_url(prefix: Option<&str>) -> RemoteUrl {
    let raw = match prefix {
        Some(p) => format!("s3+https://my-bucket.s3.us-west-2.amazonaws.com/{p}"),
        None => "s3+https://my-bucket.s3.us-west-2.amazonaws.com/".to_string(),
    };
    url::parse(&raw).expect("test URL must parse")
}

async fn drive(
    remote: RemoteUrl,
    store: Arc<dyn ObjectStore>,
    script: &str,
) -> (Vec<u8>, Result<(), ProtocolError>) {
    let (client_side, helper_side) = tokio::io::duplex(64 * 1024);
    let (helper_in, helper_out) = tokio::io::split(helper_side);
    let (client_reader, mut client_writer) = tokio::io::split(client_side);

    let script_bytes = script.as_bytes().to_owned();
    let writer_task = tokio::spawn(async move {
        client_writer.write_all(&script_bytes).await.unwrap();
        client_writer.shutdown().await.unwrap();
    });

    let reader_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut reader = client_reader;
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).await.unwrap();
        buf
    });

    let result = run(
        remote,
        store,
        tokio::io::BufReader::new(helper_in),
        helper_out,
        None,
    )
    .await;

    writer_task.await.unwrap();
    let output = reader_task.await.unwrap();
    (output, result)
}

#[tokio::test]
async fn capabilities_emits_exact_block() {
    let (out, result) = drive(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        "capabilities\n",
    )
    .await;
    result.expect("capabilities should succeed");
    assert_eq!(&out, b"*push\n*fetch\noption\n\n");
}

#[tokio::test]
async fn list_empty_bucket_emits_terminator() {
    let (out, result) = drive(s3_url(Some("repo")), Arc::new(MockStore::new()), "list\n").await;
    result.expect("list should succeed");
    assert_eq!(&out, b"\n");
}

#[tokio::test]
async fn list_for_push_skips_head_lookup() {
    let store = MockStore::new();
    store.insert(
        format!("repo/refs/heads/main/{SHA_A}.bundle"),
        Bytes::from_static(b"bundle a"),
    );
    store.insert("repo/HEAD", Bytes::from_static(b"refs/heads/main"));

    let (out, result) = drive(s3_url(Some("repo")), Arc::new(store), "list for-push\n").await;
    result.expect("list for-push should succeed");
    let text = std::str::from_utf8(&out).unwrap();
    // Exact-eq subsumes presence of the bundle line, absence of `@<ref> HEAD`,
    // and the trailing-blank-line terminator in a single assertion.
    assert_eq!(text, format!("{SHA_A} refs/heads/main\n\n"));
}

#[tokio::test]
async fn list_emits_head_pointer_when_ref_present() {
    let store = MockStore::new();
    store.insert(
        format!("repo/refs/heads/main/{SHA_A}.bundle"),
        Bytes::from_static(b"bundle"),
    );
    store.insert("repo/HEAD", Bytes::from_static(b"refs/heads/main\n"));

    let (out, result) = drive(s3_url(Some("repo")), Arc::new(store), "list\n").await;
    result.expect("list should succeed");
    let text = std::str::from_utf8(&out).unwrap();
    assert_eq!(
        text,
        format!("@refs/heads/main HEAD\n{SHA_A} refs/heads/main\n\n")
    );
}

#[tokio::test]
async fn list_omits_head_when_pointed_ref_has_no_bundle() {
    let store = MockStore::new();
    store.insert(
        format!("repo/refs/heads/feature/{SHA_A}.bundle"),
        Bytes::from_static(b"bundle"),
    );
    store.insert("repo/HEAD", Bytes::from_static(b"refs/heads/main"));

    let (out, result) = drive(s3_url(Some("repo")), Arc::new(store), "list\n").await;
    result.expect("list should succeed");
    let text = std::str::from_utf8(&out).unwrap();
    // Exact-eq: no `@refs/heads/main HEAD` line (the listed ref does not
    // match the head body) and the bundle line is the only output.
    assert_eq!(text, format!("{SHA_A} refs/heads/feature\n\n"));
}

#[tokio::test]
async fn list_swallows_missing_head_silently() {
    let store = MockStore::new();
    store.insert(
        format!("repo/refs/heads/main/{SHA_A}.bundle"),
        Bytes::from_static(b"bundle"),
    );
    // No HEAD object — must not error.

    let (out, result) = drive(s3_url(Some("repo")), Arc::new(store), "list\n").await;
    result.expect("list should succeed even without HEAD");
    let text = std::str::from_utf8(&out).unwrap();
    assert_eq!(text, format!("{SHA_A} refs/heads/main\n\n"));
}

#[tokio::test]
async fn list_sorts_bundles_by_last_modified_desc() {
    let store = MockStore::new();
    let now = OffsetDateTime::now_utc();
    store.insert_with(
        format!("repo/refs/heads/main/{SHA_A}.bundle"),
        Bytes::from_static(b"old"),
        now - Duration::seconds(60),
        PutOpts::default(),
    );
    store.insert_with(
        format!("repo/refs/heads/main/{SHA_B}.bundle"),
        Bytes::from_static(b"new"),
        now,
        PutOpts::default(),
    );
    store.insert_with(
        format!("repo/refs/heads/main/{SHA_C}.bundle"),
        Bytes::from_static(b"middle"),
        now - Duration::seconds(30),
        PutOpts::default(),
    );

    let (out, result) = drive(s3_url(Some("repo")), Arc::new(store), "list\n").await;
    result.expect("list should succeed");
    let text = std::str::from_utf8(&out).unwrap();
    let expected =
        format!("{SHA_B} refs/heads/main\n{SHA_C} refs/heads/main\n{SHA_A} refs/heads/main\n\n");
    assert_eq!(text, expected);
}

#[tokio::test]
async fn list_filters_non_bundle_keys() {
    let store = MockStore::new();
    // Real bundle.
    store.insert(
        format!("repo/refs/heads/main/{SHA_A}.bundle"),
        Bytes::from_static(b"bundle"),
    );
    // Uppercase SHA — must be filtered out.
    let upper_sha = SHA_A.to_uppercase();
    store.insert(
        format!("repo/refs/heads/main/{upper_sha}.bundle"),
        Bytes::from_static(b"bundle"),
    );
    // Non-refs/ keys.
    store.insert(format!("repo/lfs/{SHA_A}"), Bytes::from_static(b"lfs"));
    // Lock file under refs.
    store.insert(
        "repo/refs/heads/main/LOCK#.lock",
        Bytes::from_static(b"lock"),
    );

    let (out, result) = drive(s3_url(Some("repo")), Arc::new(store), "list for-push\n").await;
    result.expect("list should succeed");
    let text = std::str::from_utf8(&out).unwrap();
    assert_eq!(text, format!("{SHA_A} refs/heads/main\n\n"));
}

#[tokio::test]
async fn list_rejects_sibling_prefix_collision() {
    let store = MockStore::new();
    // Real repo.
    store.insert(
        format!("repo/refs/heads/main/{SHA_A}.bundle"),
        Bytes::from_static(b"bundle"),
    );
    // Sibling-prefix repo that would byte-match `prefix=repo`.
    store.insert(
        format!("repo-other/refs/heads/main/{SHA_B}.bundle"),
        Bytes::from_static(b"bundle"),
    );

    let (out, result) = drive(s3_url(Some("repo")), Arc::new(store), "list for-push\n").await;
    result.expect("list should succeed");
    let text = std::str::from_utf8(&out).unwrap();
    assert_eq!(text, format!("{SHA_A} refs/heads/main\n\n"));
    assert!(!text.contains(SHA_B));
}

#[tokio::test]
async fn list_works_with_no_prefix() {
    let store = MockStore::new();
    store.insert(
        format!("refs/heads/main/{SHA_A}.bundle"),
        Bytes::from_static(b"bundle"),
    );

    let (out, result) = drive(s3_url(None), Arc::new(store), "list for-push\n").await;
    result.expect("list should succeed");
    let text = std::str::from_utf8(&out).unwrap();
    assert_eq!(text, format!("{SHA_A} refs/heads/main\n\n"));
}

#[tokio::test]
async fn option_verbosity_two_responds_ok() {
    let (out, result) = drive(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        "option verbosity 2\n",
    )
    .await;
    result.expect("option should succeed");
    assert_eq!(&out, b"ok\n");
}

#[tokio::test]
async fn option_verbosity_one_responds_unsupported() {
    let (out, result) = drive(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        "option verbosity 1\n",
    )
    .await;
    result.expect("option should succeed");
    assert_eq!(&out, b"unsupported\n");
}

#[tokio::test]
async fn option_unknown_key_responds_unsupported() {
    let (out, result) = drive(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        "option progress true\n",
    )
    .await;
    result.expect("option should succeed");
    assert_eq!(&out, b"unsupported\n");
}

#[tokio::test]
async fn empty_line_emits_terminator_in_idle_mode() {
    let (out, result) = drive(s3_url(Some("repo")), Arc::new(MockStore::new()), "\n").await;
    result.expect("blank line should succeed");
    assert_eq!(&out, b"\n");
}

#[tokio::test]
async fn invalid_command_returns_error() {
    let (_out, result) = drive(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        "nonsense\n",
    )
    .await;
    match result {
        Err(ProtocolError::InvalidCommand(line)) => assert_eq!(line, "nonsense"),
        other => panic!("expected InvalidCommand error, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_command_returns_not_implemented() {
    let (out, result) = drive(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        &format!("fetch {SHA_A} refs/heads/main\n"),
    )
    .await;
    match result {
        Err(ProtocolError::Fetch(_)) => {}
        other => panic!("expected Fetch error, got {other:?}"),
    }
    // The stub bails before writing anything; a regression that emitted
    // partial protocol output before erroring would corrupt git's parser.
    assert!(out.is_empty(), "stub must not write to stdout: {out:?}");
}

#[tokio::test]
async fn push_command_returns_not_implemented() {
    let (out, result) = drive(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        "push refs/heads/main:refs/heads/main\n",
    )
    .await;
    match result {
        Err(ProtocolError::Push(_)) => {}
        other => panic!("expected Push error, got {other:?}"),
    }
    // The stub bails before writing anything; a regression that emitted
    // partial protocol output before erroring would corrupt git's parser.
    assert!(out.is_empty(), "stub must not write to stdout: {out:?}");
}

#[tokio::test]
async fn stdin_eof_exits_cleanly() {
    let (out, result) = drive(s3_url(Some("repo")), Arc::new(MockStore::new()), "").await;
    result.expect("EOF should be a clean exit");
    assert!(out.is_empty());
}

#[tokio::test]
async fn batched_command_then_blank_line_emits_terminator() {
    // capabilities and list each emit their own terminators, but a bare
    // blank line should still produce just `\n`.
    let (out, result) = drive(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        "capabilities\n\n",
    )
    .await;
    result.expect("script should succeed");
    assert_eq!(&out, b"*push\n*fetch\noption\n\n\n");
}

#[tokio::test]
async fn head_with_trailing_whitespace_is_trimmed() {
    let store = MockStore::new();
    store.insert(
        format!("repo/refs/heads/main/{SHA_A}.bundle"),
        Bytes::from_static(b"bundle"),
    );
    store.insert("repo/HEAD", Bytes::from_static(b"  refs/heads/main\n  \n"));

    let (out, result) = drive(s3_url(Some("repo")), Arc::new(store), "list\n").await;
    result.expect("list should succeed");
    let text = std::str::from_utf8(&out).unwrap();
    // Exact-eq: confirms the leading whitespace was stripped (otherwise the
    // ref-match would fail and `@<ref> HEAD` would be omitted) AND that no
    // extra padding leaked through to the protocol output.
    assert_eq!(
        text,
        format!("@refs/heads/main HEAD\n{SHA_A} refs/heads/main\n\n")
    );
}

#[tokio::test]
async fn head_with_empty_body_is_ignored() {
    let store = MockStore::new();
    store.insert(
        format!("repo/refs/heads/main/{SHA_A}.bundle"),
        Bytes::from_static(b"bundle"),
    );
    store.insert("repo/HEAD", Bytes::from_static(b"   \n"));

    let (out, result) = drive(s3_url(Some("repo")), Arc::new(store), "list\n").await;
    result.expect("list should succeed");
    let text = std::str::from_utf8(&out).unwrap();
    assert!(!text.contains("HEAD\n"));
    assert_eq!(text, format!("{SHA_A} refs/heads/main\n\n"));
}

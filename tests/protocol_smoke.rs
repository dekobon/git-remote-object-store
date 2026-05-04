//! Smoke test: drive [`protocol::run`] in-process via
//! `tokio::io::duplex`, with a [`MockStore`] standing in for the cloud
//! backend. Every claim from the `cmd_list` / `cmd_capabilities`
//! handlers has a matching assertion here.
//!
//! Real binary-spawn integration tests (with `git-remote-s3-https`
//! against RustFS) live alongside the fetch and push integration
//! suites; this file's role is the deterministic in-process check.

#![cfg(feature = "test-util")]

mod common;

use std::sync::Arc;

use bytes::Bytes;
use git_remote_object_store::object_store::mock::MockStore;
use git_remote_object_store::object_store::{ObjectStore, PutOpts};
use git_remote_object_store::protocol::ProtocolError;
use git_remote_object_store::url::{RemoteUrl, StorageEngine};
use time::Duration;
use time::OffsetDateTime;

use common::{drive_in, s3_url};

const SHA_A: &str = "0000000000000000000000000000000000000001";
const SHA_B: &str = "0000000000000000000000000000000000000002";
const SHA_C: &str = "0000000000000000000000000000000000000003";

async fn drive(
    remote: RemoteUrl,
    store: Arc<dyn ObjectStore>,
    script: &str,
) -> (Vec<u8>, Result<(), ProtocolError>) {
    drive_in(remote, store, script, std::env::temp_dir()).await
}

#[tokio::test]
async fn packchain_capabilities_succeeds() {
    // Phase 2 (issue #63) lights up packchain push. Engine-agnostic
    // commands like `capabilities` must succeed for the packchain
    // engine just as they do for the bundle engine — pinning this
    // catches a regression that re-introduces a blanket
    // engine-not-implemented gate at REPL entry.
    let raw = "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain";
    let remote = git_remote_object_store::url::parse(raw).expect("URL parses");

    let store: Arc<dyn ObjectStore> = Arc::new(MockStore::new());
    let (out, result) = drive(remote, store, "capabilities\n").await;

    result.expect("capabilities must succeed for packchain");
    assert_eq!(&out, b"*push\n*fetch\noption\n\n");
}

#[tokio::test]
async fn packchain_fetch_aborts_with_engine_not_implemented() {
    // Phase 3 will land packchain fetch; until then a `fetch` batch
    // against a packchain bucket must abort with a clear error rather
    // than silently routing through the bundle code path. Stdout
    // discipline is asserted at the variant level — the abort should
    // happen before any `fetch` reply is written.
    let raw = "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain";
    let remote = git_remote_object_store::url::parse(raw).expect("URL parses");

    let store: Arc<dyn ObjectStore> = Arc::new(MockStore::new());
    let (_out, result) = drive(
        remote,
        store,
        "fetch 0123456789abcdef0123456789abcdef01234567 refs/heads/main\n\n",
    )
    .await;
    let err = result.expect_err("packchain fetch must abort");
    assert!(
        matches!(
            err,
            ProtocolError::EngineNotImplemented(StorageEngine::Packchain)
        ),
        "expected EngineNotImplemented(Packchain), got {err:?}",
    );
}

#[tokio::test]
async fn packchain_format_resolves_engine_even_without_url_flag() {
    // FORMAT is authoritative for the resolved engine. A bucket
    // already locked to `packchain` must dispatch through the
    // packchain code paths even when the URL omits `?engine=` —
    // otherwise a bundle-helper would walk a packchain bucket's keys
    // and either return empty results or overwrite chain.json.
    //
    // Capabilities is engine-agnostic; the meaningful regression is
    // that subsequent fetch / push gets routed by FORMAT, not by URL.
    let store = MockStore::new();
    store.insert("repo/FORMAT", Bytes::from_static(b"packchain"));
    let store: Arc<dyn ObjectStore> = Arc::new(store);

    let (_out, result) = drive(
        s3_url(Some("repo")),
        store,
        "fetch 0123456789abcdef0123456789abcdef01234567 refs/heads/main\n\n",
    )
    .await;
    let err = result.expect_err("packchain fetch must abort even without URL flag");
    assert!(
        matches!(
            err,
            ProtocolError::EngineNotImplemented(StorageEngine::Packchain)
        ),
        "FORMAT must drive engine resolution; got {err:?}",
    );
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
async fn option_verbosity_zero_responds_unsupported() {
    // Explicit "off" — git probes with `option verbosity 0` to silence
    // helpers; we have nothing to say so we must respond `unsupported`.
    let (out, result) = drive(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        "option verbosity 0\n",
    )
    .await;
    result.expect("option should succeed");
    assert_eq!(&out, b"unsupported\n");
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
async fn option_verbosity_three_responds_ok() {
    // Git may send any non-negative integer for `option verbosity`; the
    // handler treats `n >= 2` as the "info" threshold, so 3, 4, … must
    // all behave identically to 2 (`ok\n`). Pinning this prevents a
    // future refactor from accidentally tightening the predicate to
    // `== 2`, which would silently break high-verbosity invocations.
    let (out, result) = drive(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        "option verbosity 3\n",
    )
    .await;
    result.expect("option should succeed");
    assert_eq!(&out, b"ok\n");
}

#[tokio::test]
async fn option_verbosity_four_responds_ok() {
    // Same threshold as `verbosity 3` — covers a second value above the
    // boundary so the test isn't married to the exact number 3.
    let (out, result) = drive(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        "option verbosity 4\n",
    )
    .await;
    result.expect("option should succeed");
    assert_eq!(&out, b"ok\n");
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
async fn push_with_malformed_args_returns_parse_error() {
    use git_remote_object_store::protocol::push::PushError;

    // Drain on the trailing blank line so push_batch is invoked. A
    // malformed refspec aborts the batch with `PushError::Parse` before
    // any stdout traffic — a regression that emitted partial protocol
    // output before erroring would corrupt git's parser.
    let (out, result) = drive(
        s3_url(Some("repo")),
        Arc::new(MockStore::new()),
        "push not-a-refspec\n\n",
    )
    .await;
    match result {
        Err(ProtocolError::Push(PushError::Parse { .. })) => {}
        other => panic!("expected Push(Parse) error, got {other:?}"),
    }
    assert!(
        out.is_empty(),
        "push must not write on parse error: {out:?}"
    );
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

/// Mid-batch fetch → push mode flip discards the buffered fetch.
///
/// Spec-allowed but uncommon: the REPL's `BatchState::accumulate`
/// resets the OTHER mode's accumulator on a switch. A regression that
/// kept the buffered fetch would either:
///   - run the fetch and crash on a missing bundle (since the script
///     never seeds it), turning the script's `Ok(())` into `Err`, or
///   - emit fetch-side stdout bytes before the push outcome.
///
/// This test seeds nothing for the fetch and asserts the script
/// produces ONLY the push outcome line (with no fetch traffic), so the
/// fetch must have been dropped.
#[tokio::test]
async fn fetch_then_push_mode_flip_drops_buffered_fetch() {
    // The push is `:refs/heads/main` (delete a ref). With nothing
    // seeded in the store, this returns the upstream-style
    // `error <ref> "not found"?` outcome — a deterministic byte-exact
    // line we can pin. The fetch line targets a SHA that is NOT in the
    // store; if the fetch ran, the helper would error with
    // ProtocolError::Fetch (NotFound) instead of producing this output.
    let script = format!("fetch {SHA_A} refs/heads/main\npush :refs/heads/main\n\n");
    let (out, result) = drive(s3_url(Some("repo")), Arc::new(MockStore::new()), &script).await;
    result.expect("mode flip script should succeed");
    let text = std::str::from_utf8(&out).expect("stdout utf-8");
    // Byte-exact: the push outcome line, then the trailing blank-line
    // batch terminator. No fetch-side bytes leaked through.
    assert_eq!(
        text, "error refs/heads/main \"not found\"?\n\n",
        "expected only the push outcome line, got {text:?}",
    );
}

//! Wire types for the [Git LFS custom-transfer
//! protocol](https://github.com/git-lfs/git-lfs/blob/main/docs/custom-transfers.md).
//!
//! The protocol is newline-delimited JSON on stdin/stdout. Every event
//! is a single JSON object on its own line; responses (init ack,
//! progress, complete) are also single-line JSON objects.

use serde::{Deserialize, Serialize};

/// One incoming event from `git-lfs`.
///
/// The wire format tags variants on the `event` field; `serde`'s
/// internally-tagged enum representation handles the dispatch.
#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub(crate) enum Event {
    /// First event. Carries the operation (`upload` or `download`),
    /// the local remote name (e.g. `origin`), and a few hints we don't
    /// currently consume.
    Init(InitEvent),
    /// Push a local file to the bucket.
    Upload(UploadEvent),
    /// Fetch a bucket object to a local file.
    Download(DownloadEvent),
    /// Graceful shutdown — the agent should exit cleanly.
    Terminate,
}

/// Fields of the `init` event.
///
/// Only `remote` is captured. Upstream's `operation` /
/// `concurrent` / `concurrenttransfers` fields are accepted on the
/// wire but unused — serde silently drops unknown fields, so they
/// don't need to appear in this struct to keep deserialization
/// happy.
#[derive(Debug, Deserialize)]
pub(crate) struct InitEvent {
    /// Local git remote name (e.g. `origin`). Resolved to a URL via
    /// `git remote get-url`.
    pub(crate) remote: String,
}

/// Fields of an `upload` event.
#[derive(Debug, Deserialize)]
pub(crate) struct UploadEvent {
    /// LFS object id (lowercase SHA-256 hex).
    pub(crate) oid: String,
    /// Body length in bytes (used for the final progress event).
    pub(crate) size: u64,
    /// Local file containing the body to upload.
    pub(crate) path: String,
}

/// Fields of a `download` event.
#[derive(Debug, Deserialize)]
pub(crate) struct DownloadEvent {
    /// LFS object id (lowercase SHA-256 hex).
    pub(crate) oid: String,
    /// Body length in bytes.
    pub(crate) size: u64,
}

/// Outgoing acknowledgement of `init`. The protocol expects either an
/// empty JSON object (`{}`) on success or `{"error":{...}}` on failure.
#[derive(Debug, Serialize)]
pub(crate) struct InitResponse<'a> {
    /// Populated on failure; absent on success (the empty-object form).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<ErrorPayload<'a>>,
}

/// Progress event emitted while an upload/download is running.
#[derive(Debug, Serialize)]
pub(crate) struct ProgressEvent<'a> {
    /// Always the literal `"progress"`.
    pub(crate) event: &'static str,
    /// LFS oid this progress refers to.
    pub(crate) oid: &'a str,
    /// Cumulative bytes transferred so far.
    #[serde(rename = "bytesSoFar")]
    pub(crate) bytes_so_far: u64,
    /// Bytes transferred since the previous progress event.
    #[serde(rename = "bytesSinceLast")]
    pub(crate) bytes_since_last: u64,
}

/// Terminal event for an upload or download.
///
/// On success: `event=complete, oid` (and `path` for downloads). On
/// failure: `event=complete, oid, error={code, message}`.
#[derive(Debug, Serialize)]
pub(crate) struct CompleteEvent<'a> {
    /// Always the literal `"complete"`.
    pub(crate) event: &'static str,
    /// LFS oid this event refers to.
    pub(crate) oid: &'a str,
    /// Path the body was downloaded to. Only present for `download`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<&'a str>,
    /// Error payload. Only present on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<ErrorPayload<'a>>,
}

/// `{code, message}` error object embedded in init / complete events.
#[derive(Debug, Serialize)]
pub(crate) struct ErrorPayload<'a> {
    /// Numeric error code. Mirrors upstream's choice of `2` for
    /// generic agent errors and `32` for malformed init.
    pub(crate) code: u32,
    /// Human-readable description.
    pub(crate) message: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_init_event_and_ignores_unused_fields() {
        // Upstream emits `operation` / `concurrent` /
        // `concurrenttransfers` — serde must silently drop them.
        let json = r#"{"event":"init","operation":"upload","remote":"origin","concurrent":true,"concurrenttransfers":3}"#;
        let evt: Event = serde_json::from_str(json).expect("parse init");
        match evt {
            Event::Init(init) => assert_eq!(init.remote, "origin"),
            other => panic!("expected init, got {other:?}"),
        }
    }

    #[test]
    fn parses_upload_event() {
        let json = r#"{"event":"upload","oid":"abc","size":42,"path":"/tmp/foo"}"#;
        let evt: Event = serde_json::from_str(json).expect("parse upload");
        match evt {
            Event::Upload(u) => {
                assert_eq!(u.oid, "abc");
                assert_eq!(u.size, 42);
                assert_eq!(u.path, "/tmp/foo");
            }
            other => panic!("expected upload, got {other:?}"),
        }
    }

    #[test]
    fn parses_download_event() {
        let json = r#"{"event":"download","oid":"abc","size":42}"#;
        let evt: Event = serde_json::from_str(json).expect("parse download");
        assert!(matches!(evt, Event::Download(_)));
    }

    #[test]
    fn parses_terminate_event() {
        let json = r#"{"event":"terminate"}"#;
        let evt: Event = serde_json::from_str(json).expect("parse terminate");
        assert!(matches!(evt, Event::Terminate));
    }

    #[test]
    fn complete_skips_optional_fields_when_absent() {
        let evt = CompleteEvent {
            event: "complete",
            oid: "abc",
            path: None,
            error: None,
        };
        let out = serde_json::to_string(&evt).expect("serialize");
        assert_eq!(out, r#"{"event":"complete","oid":"abc"}"#);
    }

    #[test]
    fn complete_emits_path_on_download_success() {
        let evt = CompleteEvent {
            event: "complete",
            oid: "abc",
            path: Some("/tmp/abc"),
            error: None,
        };
        let out = serde_json::to_string(&evt).expect("serialize");
        assert_eq!(out, r#"{"event":"complete","oid":"abc","path":"/tmp/abc"}"#);
    }

    #[test]
    fn complete_emits_error_on_failure() {
        let evt = CompleteEvent {
            event: "complete",
            oid: "abc",
            path: None,
            error: Some(ErrorPayload {
                code: 2,
                message: "boom",
            }),
        };
        let out = serde_json::to_string(&evt).expect("serialize");
        assert_eq!(
            out,
            r#"{"event":"complete","oid":"abc","error":{"code":2,"message":"boom"}}"#
        );
    }

    #[test]
    fn init_response_empty_on_success() {
        let resp = InitResponse { error: None };
        let out = serde_json::to_string(&resp).expect("serialize");
        assert_eq!(out, "{}");
    }
}

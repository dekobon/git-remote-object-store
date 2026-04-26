//! Backend-neutral LFS upload/download driver.
//!
//! The agent owns an `Arc<dyn ObjectStore>` and a per-repo prefix.
//! It exposes one method per LFS operation that writes the matching
//! line-oriented JSON events to its writer; transport errors flow up
//! as [`AgentError::Io`] (fatal) while object-store errors are folded
//! into a `complete` event with an `error` payload (recoverable —
//! the LFS client moves on to the next event).

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use thiserror::Error;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::{debug, warn};

use crate::lfs::oid::LfsOid;
use crate::lfs::protocol::{CompleteEvent, EventError, ProgressEvent};
use crate::object_store::{Error as ObjectStoreError, ObjectStore};

/// Generic error code surfaced in `complete` event payloads. Matches
/// upstream `git_remote_s3/lfs.py:write_error_event` (`code=2`).
const ERR_CODE_GENERIC: u32 = 2;

/// Driver for a single LFS session against one remote.
pub(crate) struct Agent {
    store: Arc<dyn ObjectStore>,
    /// Bucket / container prefix derived from the remote URL. Empty
    /// string when the URL had no `<prefix>` segment.
    prefix: String,
    /// `<git-dir>/lfs/tmp` — destination directory for downloads.
    tmp_dir: PathBuf,
}

/// Fatal errors during agent dispatch. Object-store failures are *not*
/// in this enum: those become `complete` events instead of process
/// exits, which is the protocol contract.
///
/// Surfaces through the public [`RunError::Agent`][crate::lfs::RunError]
/// variant, so this type is part of the crate's public API.
#[derive(Debug, Error)]
pub enum AgentError {
    /// stdin/stdout transport failure — fatal.
    #[error("LFS protocol I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization of an outgoing event failed. Should be
    /// unreachable in practice (every type that flows here is owned
    /// by us), but we surface it instead of panicking.
    #[error("LFS event serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
}

impl Agent {
    /// Build an agent. `prefix` is the path-prefix from the parsed
    /// remote URL (no trailing `/`); `tmp_dir` is `<git-dir>/lfs/tmp`.
    pub(crate) fn new(
        store: Arc<dyn ObjectStore>,
        prefix: Option<String>,
        tmp_dir: PathBuf,
    ) -> Self {
        Self {
            store,
            prefix: prefix.unwrap_or_default(),
            tmp_dir,
        }
    }

    /// Destination key for an LFS object: `<prefix>/lfs/<oid>` (or
    /// `lfs/<oid>` when there is no prefix).
    fn key(&self, oid: &LfsOid) -> String {
        if self.prefix.is_empty() {
            format!("lfs/{oid}")
        } else {
            format!("{}/lfs/{oid}", self.prefix)
        }
    }

    /// Handle an `upload` event: skip when the key already exists,
    /// otherwise stream the file body and emit progress + complete.
    pub(crate) async fn upload<W: AsyncWrite + Unpin>(
        &self,
        oid_raw: &str,
        size: u64,
        path: &Path,
        writer: &mut W,
    ) -> Result<(), AgentError> {
        match self.try_upload(oid_raw, path).await {
            Ok(UploadOutcome::AlreadyPresent) => write_complete(writer, oid_raw, None, None).await,
            Ok(UploadOutcome::Uploaded { oid }) => {
                write_progress(writer, oid.as_str(), size, size).await?;
                write_complete(writer, oid.as_str(), None, None).await
            }
            Err(OpError { oid, message }) => {
                write_complete(writer, &oid, None, Some(&message)).await
            }
        }
    }

    /// Handle a `download` event: stream the body to
    /// `<tmp_dir>/<oid>` and emit progress + complete-with-path.
    pub(crate) async fn download<W: AsyncWrite + Unpin>(
        &self,
        oid_raw: &str,
        size: u64,
        writer: &mut W,
    ) -> Result<(), AgentError> {
        match self.try_download(oid_raw).await {
            Ok(DownloadOutcome { oid, dest_str }) => {
                write_progress(writer, oid.as_str(), size, size).await?;
                write_complete(writer, oid.as_str(), Some(&dest_str), None).await
            }
            Err(OpError { oid, message }) => {
                write_complete(writer, &oid, None, Some(&message)).await
            }
        }
    }

    async fn try_upload(&self, oid_raw: &str, path: &Path) -> Result<UploadOutcome, OpError> {
        let oid = parse_oid(oid_raw)?;
        let key = self.key(&oid);
        debug!(oid = %oid, key = %key, "lfs upload");

        match self.store.head(&key).await {
            Ok(_) => {
                debug!(oid = %oid, "object already present; skipping upload");
                return Ok(UploadOutcome::AlreadyPresent);
            }
            Err(ObjectStoreError::NotFound(_)) => {}
            Err(e) => {
                warn!(oid = %oid, error = %e, "head failed during upload");
                return Err(OpError::with_cause(oid.as_str(), &e));
            }
        }

        self.store
            .put_path(&key, path, crate::object_store::PutOpts::default())
            .await
            .map_err(|e| {
                warn!(oid = %oid, error = %e, "upload failed");
                OpError::with_cause(oid.as_str(), &e)
            })?;

        Ok(UploadOutcome::Uploaded { oid })
    }

    async fn try_download(&self, oid_raw: &str) -> Result<DownloadOutcome, OpError> {
        let oid = parse_oid(oid_raw)?;
        let key = self.key(&oid);
        let dest = self.tmp_dir.join(oid.as_str());
        debug!(oid = %oid, key = %key, dest = %dest.display(), "lfs download");

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                warn!(oid = %oid, error = %e, "create_dir_all failed");
                OpError::with_cause(oid.as_str(), &e)
            })?;
        }

        self.store.get_to_file(&key, &dest).await.map_err(|e| {
            warn!(oid = %oid, error = %e, "download failed");
            OpError::with_cause(oid.as_str(), &e)
        })?;

        let dest_str = dest.to_str().map(str::to_owned).ok_or_else(|| {
            OpError::with_cause(oid.as_str(), &"download destination is not valid UTF-8")
        })?;

        Ok(DownloadOutcome { oid, dest_str })
    }
}

enum UploadOutcome {
    AlreadyPresent,
    Uploaded { oid: LfsOid },
}

struct DownloadOutcome {
    oid: LfsOid,
    dest_str: String,
}

/// Recoverable per-event error. `oid` is owned because we may have
/// failed before parsing succeeded (in which case the wire-side oid
/// string from the event is the only handle we have).
struct OpError {
    oid: String,
    message: String,
}

impl OpError {
    /// Build from any `Display`-able cause. Used uniformly for
    /// object-store errors, `std::io::Error`s, and string literals.
    fn with_cause(oid: &str, cause: &dyn fmt::Display) -> Self {
        Self {
            oid: oid.to_owned(),
            message: cause.to_string(),
        }
    }
}

fn parse_oid(oid_raw: &str) -> Result<LfsOid, OpError> {
    LfsOid::from_str(oid_raw)
        .map_err(|e| OpError::with_cause(oid_raw, &format_args!("invalid oid: {e}")))
}

/// Serialize `evt` and write it as one newline-terminated line.
/// Shared with [`crate::lfs::run`] so init-ack and per-event writes
/// go through the same flush discipline.
pub(crate) async fn write_event<W, E>(writer: &mut W, evt: &E) -> Result<(), AgentError>
where
    W: AsyncWrite + Unpin,
    E: serde::Serialize,
{
    let line = serde_json::to_string(evt)?;
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn write_progress<W: AsyncWrite + Unpin>(
    writer: &mut W,
    oid: &str,
    bytes_so_far: u64,
    bytes_since_last: u64,
) -> Result<(), AgentError> {
    write_event(
        writer,
        &ProgressEvent {
            event: "progress",
            oid,
            bytes_so_far,
            bytes_since_last,
        },
    )
    .await
}

/// Emit a terminal `complete` event. `path` carries the local
/// destination on download success; `error_message` carries the
/// failure message on either-direction error.
async fn write_complete<W: AsyncWrite + Unpin>(
    writer: &mut W,
    oid: &str,
    path: Option<&str>,
    error_message: Option<&str>,
) -> Result<(), AgentError> {
    write_event(
        writer,
        &CompleteEvent {
            event: "complete",
            oid,
            path,
            error: error_message.map(|message| EventError {
                code: ERR_CODE_GENERIC,
                message,
            }),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::mock::MockStore;
    use bytes::Bytes;
    use tempfile::TempDir;

    fn good_oid() -> String {
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned()
    }

    fn agent(store: MockStore, prefix: Option<&str>, tmp: &TempDir) -> Agent {
        Agent::new(
            Arc::new(store),
            prefix.map(str::to_owned),
            tmp.path().to_owned(),
        )
    }

    #[tokio::test]
    async fn upload_skips_when_present() {
        let store = MockStore::new();
        let oid = good_oid();
        store.insert(format!("repo/lfs/{oid}"), Bytes::from_static(b"hello"));
        let tmp = TempDir::new().unwrap();
        let a = agent(store.clone(), Some("repo"), &tmp);

        let src = tmp.path().join("body");
        tokio::fs::write(&src, b"hello").await.unwrap();

        let mut out = Vec::new();
        a.upload(&oid, 5, &src, &mut out).await.expect("upload");
        let got = String::from_utf8(out).unwrap();
        // Only one line — complete with no error and no path.
        assert_eq!(
            got,
            format!("{{\"event\":\"complete\",\"oid\":\"{oid}\"}}\n")
        );
    }

    #[tokio::test]
    async fn upload_streams_when_absent_and_emits_progress_then_complete() {
        let store = MockStore::new();
        let tmp = TempDir::new().unwrap();
        let oid = good_oid();
        let a = agent(store.clone(), Some("repo"), &tmp);

        let src = tmp.path().join("body");
        let body = b"the quick brown fox";
        tokio::fs::write(&src, body).await.unwrap();

        let mut out = Vec::new();
        a.upload(&oid, body.len() as u64, &src, &mut out)
            .await
            .expect("upload");
        let got = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = got.lines().collect();
        assert_eq!(lines.len(), 2, "expected progress + complete: {got}");
        assert!(lines[0].contains("\"event\":\"progress\""));
        assert!(lines[0].contains(&format!("\"oid\":\"{oid}\"")));
        assert!(lines[0].contains(&format!("\"bytesSoFar\":{}", body.len())));
        assert_eq!(
            lines[1],
            format!("{{\"event\":\"complete\",\"oid\":\"{oid}\"}}")
        );
        assert!(store.contains(&format!("repo/lfs/{oid}")));
    }

    #[tokio::test]
    async fn upload_rejects_invalid_oid() {
        let store = MockStore::new();
        let tmp = TempDir::new().unwrap();
        let a = agent(store, Some("repo"), &tmp);

        let src = tmp.path().join("body");
        tokio::fs::write(&src, b"x").await.unwrap();

        let mut out = Vec::new();
        a.upload("not-a-real-oid", 1, &src, &mut out)
            .await
            .expect("dispatch ok");
        let got = String::from_utf8(out).unwrap();
        assert!(got.contains("\"error\""));
        assert!(got.contains("invalid oid"));
        assert_eq!(got.lines().count(), 1);
    }

    #[tokio::test]
    async fn download_writes_file_and_emits_progress_then_complete() {
        let store = MockStore::new();
        let oid = good_oid();
        let body = b"payload bytes";
        store.insert(format!("repo/lfs/{oid}"), Bytes::from_static(body));
        let tmp = TempDir::new().unwrap();
        let a = agent(store, Some("repo"), &tmp);

        let mut out = Vec::new();
        a.download(&oid, body.len() as u64, &mut out)
            .await
            .expect("download");
        let got = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = got.lines().collect();
        assert_eq!(lines.len(), 2, "expected progress + complete: {got}");
        assert!(lines[0].contains("\"event\":\"progress\""));
        let dest = tmp.path().join(&oid);
        let dest_str = dest.to_str().unwrap();
        assert!(
            lines[1].contains(&format!("\"path\":\"{dest_str}\"")),
            "complete should include path: {got}"
        );
        let read = tokio::fs::read(&dest).await.unwrap();
        assert_eq!(read, body);
    }

    #[tokio::test]
    async fn download_emits_error_on_missing_object() {
        let store = MockStore::new();
        let oid = good_oid();
        let tmp = TempDir::new().unwrap();
        let a = agent(store, Some("repo"), &tmp);

        let mut out = Vec::new();
        a.download(&oid, 0, &mut out).await.expect("dispatch ok");
        let got = String::from_utf8(out).unwrap();
        assert!(got.contains("\"error\""));
        assert!(got.contains(&format!("\"oid\":\"{oid}\"")));
    }

    #[tokio::test]
    async fn empty_prefix_yields_top_level_lfs_key() {
        let store = MockStore::new();
        let tmp = TempDir::new().unwrap();
        let oid = good_oid();
        let a = agent(store.clone(), None, &tmp);

        let src = tmp.path().join("body");
        tokio::fs::write(&src, b"x").await.unwrap();
        let mut out = Vec::new();
        a.upload(&oid, 1, &src, &mut out).await.expect("upload");
        assert!(store.contains(&format!("lfs/{oid}")));
    }
}

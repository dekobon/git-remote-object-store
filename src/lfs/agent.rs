//! Backend-neutral LFS upload/download driver.
//!
//! The agent owns an `Arc<dyn ObjectStore>` and a per-repo prefix.
//! It exposes one method per LFS operation that writes the matching
//! line-oriented JSON events to its writer; transport errors flow up
//! as [`AgentError::Io`] (fatal) while object-store errors are folded
//! into a `complete` event with an `error` payload (recoverable —
//! the LFS client moves on to the next event).

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
pub struct Agent {
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
    pub fn new(store: Arc<dyn ObjectStore>, prefix: Option<String>, tmp_dir: PathBuf) -> Self {
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
    pub async fn upload<W: AsyncWrite + Unpin>(
        &self,
        oid_raw: &str,
        size: u64,
        path: &Path,
        writer: &mut W,
    ) -> Result<(), AgentError> {
        let oid = match LfsOid::from_str(oid_raw) {
            Ok(o) => o,
            Err(e) => {
                return write_complete_error(writer, oid_raw, &format!("invalid oid: {e}")).await;
            }
        };
        let key = self.key(&oid);
        debug!(oid = %oid, key = %key, "lfs upload");

        match self.store.head(&key).await {
            Ok(_) => {
                debug!(oid = %oid, "object already present; skipping upload");
                return write_complete_success(writer, oid.as_str(), None).await;
            }
            Err(ObjectStoreError::NotFound(_)) => {}
            Err(e) => {
                warn!(oid = %oid, error = %e, "head failed during upload");
                return write_complete_error(writer, oid.as_str(), &e.to_string()).await;
            }
        }

        if let Err(e) = self
            .store
            .put_path(&key, path, crate::object_store::PutOpts::default())
            .await
        {
            warn!(oid = %oid, error = %e, "upload failed");
            return write_complete_error(writer, oid.as_str(), &e.to_string()).await;
        }

        write_progress(writer, oid.as_str(), size, size).await?;
        write_complete_success(writer, oid.as_str(), None).await
    }

    /// Handle a `download` event: stream the body to
    /// `<tmp_dir>/<oid>` and emit progress + complete-with-path.
    pub async fn download<W: AsyncWrite + Unpin>(
        &self,
        oid_raw: &str,
        size: u64,
        writer: &mut W,
    ) -> Result<(), AgentError> {
        let oid = match LfsOid::from_str(oid_raw) {
            Ok(o) => o,
            Err(e) => {
                return write_complete_error(writer, oid_raw, &format!("invalid oid: {e}")).await;
            }
        };
        let key = self.key(&oid);
        let dest = self.tmp_dir.join(oid.as_str());
        debug!(oid = %oid, key = %key, dest = %dest.display(), "lfs download");

        if let Some(parent) = dest.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            warn!(oid = %oid, error = %e, "create_dir_all failed");
            return write_complete_error(writer, oid.as_str(), &e.to_string()).await;
        }

        if let Err(e) = self.store.get_to_file(&key, &dest).await {
            warn!(oid = %oid, error = %e, "download failed");
            return write_complete_error(writer, oid.as_str(), &e.to_string()).await;
        }

        let dest_str = match dest.to_str() {
            Some(s) => s.to_owned(),
            None => {
                return write_complete_error(
                    writer,
                    oid.as_str(),
                    "download destination is not valid UTF-8",
                )
                .await;
            }
        };

        write_progress(writer, oid.as_str(), size, size).await?;
        write_complete_success(writer, oid.as_str(), Some(&dest_str)).await
    }
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> Result<(), AgentError> {
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
    let evt = ProgressEvent {
        event: "progress",
        oid,
        bytes_so_far,
        bytes_since_last,
    };
    let line = serde_json::to_string(&evt)?;
    write_line(writer, &line).await
}

async fn write_complete_success<W: AsyncWrite + Unpin>(
    writer: &mut W,
    oid: &str,
    path: Option<&str>,
) -> Result<(), AgentError> {
    let evt = CompleteEvent {
        event: "complete",
        oid,
        path,
        error: None,
    };
    let line = serde_json::to_string(&evt)?;
    write_line(writer, &line).await
}

async fn write_complete_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    oid: &str,
    message: &str,
) -> Result<(), AgentError> {
    let evt = CompleteEvent {
        event: "complete",
        oid,
        path: None,
        error: Some(EventError {
            code: ERR_CODE_GENERIC,
            message,
        }),
    };
    let line = serde_json::to_string(&evt)?;
    write_line(writer, &line).await
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

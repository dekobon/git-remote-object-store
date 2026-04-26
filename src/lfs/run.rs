//! REPL driver for the LFS custom-transfer protocol.
//!
//! Generic over reader and writer so tests can drive it through
//! `tokio::io::duplex`; the bin entrypoint wires real stdin/stdout.
//!
//! Stdout is the wire protocol — see `.claude/rules/protocol-stdout.md`.
//! Diagnostic output goes through `tracing` (configured to write to
//! stderr or a debug log file by the bin entrypoint).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, error, warn};

use crate::lfs::agent::{Agent, AgentError};
use crate::lfs::protocol::{Event, EventError, InitEvent, InitResponse};
use crate::object_store::ObjectStore;
use crate::protocol::backend;
use crate::url;

/// Errors surfaced by [`run`] that are *fatal* to the agent process.
///
/// Backend / object-store errors that occur after init are not in
/// here — they are folded into per-event `complete` payloads by the
/// [`Agent`].
#[derive(Debug, Error)]
pub enum RunError {
    /// Underlying transport (stdin/stdout) failed.
    #[error("LFS protocol I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Agent dispatch error (transport or serialization).
    #[error(transparent)]
    Agent(#[from] AgentError),
    /// First event was not `init`. The LFS spec requires it.
    #[error("expected init as the first event, got {0:?}")]
    InitNotFirst(String),
    /// Stdin closed before any event was read.
    #[error("stdin closed before init")]
    StdinClosed,
}

/// How to resolve a remote name to an [`ObjectStore`]. Production
/// uses a `gix`-based resolver; tests inject a closure that returns
/// a [`crate::object_store::mock::MockStore`].
#[async_trait::async_trait]
pub trait RemoteResolver: Send + Sync {
    /// Resolve `remote_name` → `(object store, optional bucket prefix)`.
    async fn resolve(
        &self,
        remote_name: &str,
    ) -> Result<(Arc<dyn ObjectStore>, Option<String>), Box<dyn std::error::Error + Send + Sync>>;
}

/// Production resolver: opens the local repo via `gix`, reads the
/// remote URL, parses it, and builds the matching object-store
/// backend.
pub struct GitRemoteResolver {
    /// Working directory of the local repository (cwd at process
    /// start).
    pub repo_dir: PathBuf,
}

#[async_trait::async_trait]
impl RemoteResolver for GitRemoteResolver {
    async fn resolve(
        &self,
        remote_name: &str,
    ) -> Result<(Arc<dyn ObjectStore>, Option<String>), Box<dyn std::error::Error + Send + Sync>>
    {
        type BoxErr = Box<dyn std::error::Error + Send + Sync>;
        let repo = gix::discover(&self.repo_dir).map_err(|e| Box::new(e) as BoxErr)?;
        let raw = crate::git::remote_url(&repo, remote_name).map_err(|e| Box::new(e) as BoxErr)?;
        let parsed = url::parse(&raw).map_err(|e| Box::new(e) as BoxErr)?;
        let prefix = parsed.prefix().map(str::to_owned);
        let store = backend::build(&parsed)
            .await
            .map_err(|e| Box::new(e) as BoxErr)?;
        Ok((store, prefix))
    }
}

/// Drive the LFS REPL until stdin closes or `terminate` arrives.
///
/// `tmp_dir` is the destination directory for downloads
/// (`<git-dir>/lfs/tmp` per `execution-plan.md` §5.5).
pub async fn run<R, W, Res>(
    reader: R,
    mut writer: W,
    resolver: &Res,
    tmp_dir: &Path,
) -> Result<(), RunError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
    Res: RemoteResolver + ?Sized,
{
    let mut lines = reader.lines();

    let Some(first) = lines.next_line().await? else {
        return Err(RunError::StdinClosed);
    };
    let event = parse_event(&first)?;
    let init = match event {
        Event::Init(init) => init,
        Event::Terminate => {
            // Spec doesn't require ack on terminate; mirror upstream's
            // silent exit.
            debug!("received terminate before init; exiting");
            return Ok(());
        }
        other => {
            return Err(RunError::InitNotFirst(format!("{other:?}")));
        }
    };

    let agent = match init_agent(&init, resolver, tmp_dir.to_owned()).await {
        Ok(a) => {
            write_init_ack(&mut writer, None).await?;
            a
        }
        Err(msg) => {
            error!(error = %msg, "init failed");
            write_init_ack(&mut writer, Some(&msg)).await?;
            return Ok(());
        }
    };

    while let Some(line) = lines.next_line().await? {
        debug!(line = %line, "lfs event");
        let event = match parse_event(&line) {
            Ok(e) => e,
            Err(RunError::InitNotFirst(_)) => unreachable!("only set on the init dispatch"),
            Err(other) => return Err(other),
        };
        match event {
            Event::Init(_) => {
                warn!("received second init; ignoring");
            }
            Event::Upload(u) => {
                agent
                    .upload(&u.oid, u.size, Path::new(&u.path), &mut writer)
                    .await?;
            }
            Event::Download(d) => {
                agent.download(&d.oid, d.size, &mut writer).await?;
            }
            Event::Terminate => {
                debug!("received terminate; exiting");
                break;
            }
        }
    }
    Ok(())
}

fn parse_event(line: &str) -> Result<Event, RunError> {
    serde_json::from_str(line).map_err(|e| {
        // Treat malformed JSON as a fatal protocol error — git-lfs
        // never sends garbage on the wire.
        RunError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("malformed LFS event: {e}"),
        ))
    })
}

async fn init_agent<Res>(
    init: &InitEvent,
    resolver: &Res,
    tmp_dir: PathBuf,
) -> Result<Agent, String>
where
    Res: RemoteResolver + ?Sized,
{
    if init.remote.is_empty() {
        return Err("init.remote is empty".to_owned());
    }
    let (store, prefix) = resolver
        .resolve(&init.remote)
        .await
        .map_err(|e| format!("cannot resolve remote \"{}\": {e}", init.remote))?;
    Ok(Agent::new(store, prefix, tmp_dir))
}

async fn write_init_ack<W: AsyncWrite + Unpin>(
    writer: &mut W,
    error_msg: Option<&str>,
) -> Result<(), RunError> {
    let resp = InitResponse {
        error: error_msg.map(|m| EventError {
            code: 32,
            message: m,
        }),
    };
    let line = serde_json::to_string(&resp)
        .map_err(|e| RunError::Io(std::io::Error::other(e.to_string())))?;
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::mock::MockStore;
    use bytes::Bytes;
    use tempfile::TempDir;

    struct StubResolver {
        store: MockStore,
        prefix: Option<String>,
    }

    #[async_trait::async_trait]
    impl RemoteResolver for StubResolver {
        async fn resolve(
            &self,
            _remote_name: &str,
        ) -> Result<(Arc<dyn ObjectStore>, Option<String>), Box<dyn std::error::Error + Send + Sync>>
        {
            Ok((Arc::new(self.store.clone()), self.prefix.clone()))
        }
    }

    fn good_oid() -> String {
        "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210".to_owned()
    }

    async fn drive(
        events: &[String],
        resolver: &dyn RemoteResolver,
        tmp_dir: &Path,
    ) -> (Vec<String>, Result<(), RunError>) {
        let mut input = events.join("\n");
        if !events.is_empty() {
            input.push('\n');
        }
        let reader = tokio::io::BufReader::new(std::io::Cursor::new(input.into_bytes()));
        let mut output: Vec<u8> = Vec::new();
        let res = run(reader, &mut output, resolver, tmp_dir).await;
        let lines = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        (lines, res)
    }

    #[tokio::test]
    async fn full_round_trip_init_upload_download_terminate() {
        let store = MockStore::new();
        let oid = good_oid();
        let body = b"some body";
        // Pre-seed the second oid for download.
        let oid2 = good_oid();
        store.insert(format!("repo/lfs/{oid2}"), Bytes::from_static(body));

        let resolver = StubResolver {
            store: store.clone(),
            prefix: Some("repo".to_owned()),
        };

        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        tokio::fs::write(&src, body).await.unwrap();

        let events = vec![
            r#"{"event":"init","operation":"upload","remote":"origin"}"#.to_owned(),
            format!(
                r#"{{"event":"upload","oid":"{oid}","size":{size},"path":"{path}"}}"#,
                size = body.len(),
                path = src.to_str().unwrap(),
            ),
            format!(
                r#"{{"event":"download","oid":"{oid2}","size":{size}}}"#,
                size = body.len(),
            ),
            r#"{"event":"terminate"}"#.to_owned(),
        ];
        let (lines, res) = drive(&events, &resolver, tmp.path()).await;
        res.expect("run should exit cleanly");

        // Expected: init ack, progress+complete (upload), progress+complete (download).
        assert_eq!(lines[0], "{}", "init ack should be empty object");
        assert!(lines.iter().any(|l| l.contains("\"event\":\"progress\"")));
        let completes: Vec<_> = lines
            .iter()
            .filter(|l| l.contains("\"event\":\"complete\""))
            .collect();
        assert_eq!(completes.len(), 2, "expected two completes: {lines:?}");
        assert!(store.contains(&format!("repo/lfs/{oid}")));
    }

    #[tokio::test]
    async fn init_failure_emits_error_object_and_exits_cleanly() {
        struct FailingResolver;
        #[async_trait::async_trait]
        impl RemoteResolver for FailingResolver {
            async fn resolve(
                &self,
                _remote_name: &str,
            ) -> Result<
                (Arc<dyn ObjectStore>, Option<String>),
                Box<dyn std::error::Error + Send + Sync>,
            > {
                Err("no such remote".into())
            }
        }
        let tmp = TempDir::new().unwrap();
        let events = vec![r#"{"event":"init","remote":"origin"}"#.to_owned()];
        let (lines, res) = drive(&events, &FailingResolver, tmp.path()).await;
        res.expect("init failure is non-fatal");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"error\""));
        assert!(lines[0].contains("\"code\":32"));
    }

    #[tokio::test]
    async fn first_non_init_event_is_fatal() {
        let store = MockStore::new();
        let resolver = StubResolver {
            store,
            prefix: Some("repo".into()),
        };
        let tmp = TempDir::new().unwrap();
        let events = vec![r#"{"event":"upload","oid":"abc","size":1,"path":"/tmp/x"}"#.to_owned()];
        let (_, res) = drive(&events, &resolver, tmp.path()).await;
        let err = res.expect_err("non-init first event must error");
        assert!(matches!(err, RunError::InitNotFirst(_)));
    }

    #[tokio::test]
    async fn malformed_json_is_fatal() {
        let store = MockStore::new();
        let resolver = StubResolver {
            store,
            prefix: None,
        };
        let tmp = TempDir::new().unwrap();
        let events = vec!["not json".to_owned()];
        let (_, res) = drive(&events, &resolver, tmp.path()).await;
        let err = res.expect_err("garbage line must error");
        assert!(matches!(err, RunError::Io(_)));
    }

    #[tokio::test]
    async fn empty_stdin_returns_stdin_closed() {
        let store = MockStore::new();
        let resolver = StubResolver {
            store,
            prefix: None,
        };
        let tmp = TempDir::new().unwrap();
        let (_, res) = drive(&[], &resolver, tmp.path()).await;
        assert!(matches!(res, Err(RunError::StdinClosed)));
    }
}

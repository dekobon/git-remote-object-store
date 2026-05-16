//! REPL driver for the LFS custom-transfer protocol.
//!
//! Generic over reader and writer so tests can drive it through
//! `tokio::io::duplex`; the bin entrypoint wires real stdin/stdout.
//!
//! Stdout is the wire protocol — see `.claude/rules/protocol-stdout.md`.
//! Diagnostic output goes through `tracing` (configured to write to
//! stderr or a debug log file by the bin entrypoint).

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite};
use tracing::{debug, error, warn};

use crate::lfs::agent::{self, Agent, AgentError, ERR_CODE_GENERIC, ERR_CODE_INIT};
use crate::lfs::oid::LfsOid;
use crate::lfs::protocol::{CompleteEvent, ErrorPayload, Event, InitEvent, InitResponse};
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
    /// An incoming line was not valid LFS JSON, or an outgoing event
    /// could not be serialized. Either is fatal — the protocol cannot
    /// continue past a parse mismatch.
    #[error("malformed LFS event: {0}")]
    MalformedEvent(#[from] serde_json::Error),
    /// First event was not `init`. The LFS spec requires it. The
    /// payload is the `Debug` rendering of the offending event,
    /// captured at construction time.
    #[error("expected init as the first event, got {0}")]
    InitNotFirst(String),
    /// Stdin closed before any event was read.
    #[error("stdin closed before init")]
    StdinClosed,
}

impl RunError {
    /// `true` if this error is a `BrokenPipe` / `WriteZero` from
    /// stdout closing — the bin-side REPL turns those into a clean
    /// exit. Walks both the direct `Io` variant and the nested
    /// `Agent(AgentError::Io)` variant produced by writes that flow
    /// through the agent's [`write_event`][crate::lfs::agent::write_event].
    #[must_use]
    pub fn is_broken_pipe(&self) -> bool {
        let io_err = match self {
            Self::Io(e) | Self::Agent(AgentError::Io(e)) => Some(e),
            _ => None,
        };
        io_err.is_some_and(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::WriteZero,
            )
        })
    }
}

/// Init-time failures that the bin-side REPL converts into an LFS
/// `init` error response and a clean exit. Distinct from [`RunError`]
/// because none of these are fatal to the agent — they're reported on
/// the wire as `{"error":{...}}`, then the loop returns `Ok(())`.
#[derive(Debug, Error)]
enum InitError {
    /// `init.remote` was the empty string. Upstream's helper accepts
    /// it and then explodes later; we reject up front.
    #[error("init.remote is empty")]
    EmptyRemote,
    /// `git remote get-url` / URL parsing / backend construction
    /// failed for the named remote.
    #[error("cannot resolve remote \"{remote}\": {source}")]
    Resolve {
        /// Remote name from the init event.
        remote: String,
        /// Underlying resolver failure.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// How to resolve a remote name to an [`ObjectStore`]. Production
/// uses a `gix`-based resolver; tests inject a closure that returns
/// a `MockStore` (the in-memory test backend gated on `test-util`).
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
        // `?` against `Box<dyn Error + Send + Sync>` uses the blanket
        // `From<E: Error + Send + Sync + 'static> for Box<...>`, so no
        // explicit cast is needed at each call site.
        let repo = gix::discover(&self.repo_dir)?;
        let raw = crate::git::remote_url(&repo, remote_name)?;
        let parsed = url::parse(&raw)?;
        let prefix = parsed.prefix().map(str::to_owned);
        // LFS is engine-independent (objects live at `<prefix>/lfs/<oid>`
        // regardless of the bundle/packchain choice); discard the
        // resolved engine.
        let (store, _engine) = backend::build(&parsed).await?;
        Ok((store, prefix))
    }
}

/// Drive the LFS REPL until stdin closes or `terminate` arrives.
///
/// `tmp_dir` is the destination directory for downloads
/// (`<git-dir>/lfs/tmp`).
///
/// # Errors
///
/// Returns [`RunError::StdinClosed`] if stdin closes before the first event,
/// [`RunError::MalformedEvent`] for unparseable JSON, or
/// [`RunError::InitNotFirst`] if the first event is not `init`.
/// Transport or serialisation errors from upload/download operations surface
/// as [`RunError::Io`] or [`RunError::Agent`].
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
        Err(err) => {
            error!(error = %err, "init failed");
            write_init_ack(&mut writer, Some(&err.to_string())).await?;
            return Ok(());
        }
    };

    while let Some(line) = lines.next_line().await? {
        debug!(line = %line, "lfs event");
        let event = parse_event(&line)?;
        match event {
            Event::Init(_) => {
                warn!("received second init; ignoring");
            }
            Event::Upload(u) => {
                if let Some(oid) = validate_oid(&u.oid, &mut writer, "upload").await? {
                    agent
                        .upload(&oid, u.size, Path::new(&u.path), &mut writer)
                        .await?;
                }
            }
            Event::Download(d) => {
                if let Some(oid) = validate_oid(&d.oid, &mut writer, "download").await? {
                    agent.download(&oid, d.size, &mut writer).await?;
                }
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
    // Malformed JSON is fatal — git-lfs never sends garbage on the
    // wire. The `?` operator at call sites turns this into
    // `RunError::MalformedEvent` via the `#[from]` impl.
    Ok(serde_json::from_str(line)?)
}

async fn init_agent<Res>(
    init: &InitEvent,
    resolver: &Res,
    tmp_dir: PathBuf,
) -> Result<Agent, InitError>
where
    Res: RemoteResolver + ?Sized,
{
    if init.remote.is_empty() {
        return Err(InitError::EmptyRemote);
    }
    let (store, prefix) =
        resolver
            .resolve(&init.remote)
            .await
            .map_err(|source| InitError::Resolve {
                remote: init.remote.clone(),
                source,
            })?;
    Ok(Agent::new(store, prefix, tmp_dir))
}

/// Validate `oid_raw` at the run-loop boundary. Returns `Some(oid)`
/// on success (the caller dispatches into the agent), or `None`
/// after emitting a `complete` event with an empty `oid` field and
/// the raw rejected value folded into the `error.message`. The
/// `op` label flows into the warn-log so an operator can correlate
/// the rejection with the source event line.
async fn validate_oid<W: AsyncWrite + Unpin>(
    oid_raw: &str,
    writer: &mut W,
    op: &'static str,
) -> Result<Option<LfsOid>, RunError> {
    match LfsOid::from_str(oid_raw) {
        Ok(oid) => Ok(Some(oid)),
        Err(err) => {
            warn!(oid = %oid_raw, error = %err, op, "rejecting malformed oid");
            let message = format!("invalid oid `{oid_raw}`: {err}");
            let evt = CompleteEvent {
                event: "complete",
                oid: "",
                path: None,
                error: Some(ErrorPayload {
                    code: ERR_CODE_GENERIC,
                    message: &message,
                }),
            };
            agent::write_event(writer, &evt).await?;
            Ok(None)
        }
    }
}

async fn write_init_ack<W: AsyncWrite + Unpin>(
    writer: &mut W,
    error_msg: Option<&str>,
) -> Result<(), RunError> {
    let resp = InitResponse {
        error: error_msg.map(|m| ErrorPayload {
            code: ERR_CODE_INIT,
            message: m,
        }),
    };
    Ok(agent::write_event(writer, &resp).await?)
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
        assert!(lines[0].contains(&format!("\"code\":{ERR_CODE_INIT}")));
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

    #[test]
    fn init_not_first_display_does_not_double_quote_payload() {
        // Regression guard: the variant carries a payload that has
        // already been `Debug`-rendered by the caller, so the error
        // message must use `{0}` (Display) over the wrapped String,
        // not `{0:?}` which would double-quote the Debug form.
        let err = RunError::InitNotFirst("Upload(UploadEvent { oid: \"abc\" })".to_owned());
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("expected init as the first event, got Upload(UploadEvent {"),
            "InitNotFirst should not wrap the payload in extra quotes: {rendered}"
        );
    }

    #[tokio::test]
    async fn empty_remote_in_init_emits_error_object_and_exits_cleanly() {
        // Regression guard for InitError::EmptyRemote — upstream's
        // helper accepts the empty string and explodes later; we
        // reject up front and emit the structured error response.
        struct UnreachableResolver;
        #[async_trait::async_trait]
        impl RemoteResolver for UnreachableResolver {
            async fn resolve(
                &self,
                _remote_name: &str,
            ) -> Result<
                (Arc<dyn ObjectStore>, Option<String>),
                Box<dyn std::error::Error + Send + Sync>,
            > {
                panic!("resolver should not be called when init.remote is empty");
            }
        }
        let tmp = TempDir::new().unwrap();
        let events = vec![r#"{"event":"init","remote":""}"#.to_owned()];
        let (lines, res) = drive(&events, &UnreachableResolver, tmp.path()).await;
        res.expect("empty-remote init failure is non-fatal");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"error\""));
        assert!(lines[0].contains(&format!("\"code\":{ERR_CODE_INIT}")));
        assert!(
            lines[0].contains("init.remote is empty"),
            "ack should include the InitError::EmptyRemote message: {}",
            lines[0]
        );
    }

    #[tokio::test]
    async fn broken_pipe_during_init_ack_is_clean_exit() {
        // Regression guard: if stdout closes mid-init-ack, the bin
        // turns the resulting error into a clean exit. RunError
        // must classify it as `is_broken_pipe()` so the bin's
        // `Err(other) if other.is_broken_pipe()` arm fires.
        use tokio::io::duplex;

        // A writer that returns BrokenPipe immediately. A `duplex`
        // pair where the read half is dropped achieves this.
        let (writer, reader) = duplex(64);
        drop(reader); // force BrokenPipe on the next write

        let store = MockStore::new();
        let resolver = StubResolver {
            store,
            prefix: None,
        };
        let tmp = TempDir::new().unwrap();
        let input = r#"{"event":"init","remote":"origin"}"#;
        let buffered = tokio::io::BufReader::new(std::io::Cursor::new(input.as_bytes().to_vec()));

        let res = run(buffered, writer, &resolver, tmp.path()).await;
        let err = res.expect_err("write to closed duplex must surface as Err");
        assert!(
            err.is_broken_pipe(),
            "init-ack BrokenPipe must be classified as broken-pipe, got: {err:?}"
        );
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
        assert!(matches!(err, RunError::MalformedEvent(_)));
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

    /// F-009: oid validation moved to the run-loop boundary. A
    /// malformed oid on an `upload` event must surface as a
    /// `complete` event with an empty `oid` field and the raw
    /// rejected value folded into the `error.message` so the
    /// operator can correlate the failure with the source event
    /// line.
    #[tokio::test]
    async fn upload_with_invalid_oid_emits_complete_with_empty_oid() {
        let store = MockStore::new();
        let resolver = StubResolver {
            store,
            prefix: Some("repo".to_owned()),
        };
        let tmp = TempDir::new().unwrap();
        let bad_oid = "not-a-real-oid";
        let src = tmp.path().join("body");
        tokio::fs::write(&src, b"x").await.unwrap();
        let events = vec![
            r#"{"event":"init","operation":"upload","remote":"origin"}"#.to_owned(),
            format!(
                r#"{{"event":"upload","oid":"{bad_oid}","size":1,"path":"{path}"}}"#,
                path = src.to_str().unwrap(),
            ),
            r#"{"event":"terminate"}"#.to_owned(),
        ];
        let (lines, res) = drive(&events, &resolver, tmp.path()).await;
        res.expect("run completes despite bad oid");
        // init ack + complete (error) for the upload.
        assert!(
            lines.len() >= 2,
            "expected init ack and complete: {lines:?}"
        );
        let complete_line = lines
            .iter()
            .find(|l| l.contains("\"event\":\"complete\""))
            .expect("complete event present");
        // Byte-exact assertion on the wire format. Per the LFS
        // spec we surface an empty oid field (we never had a
        // validated id) and place the raw rejected string in the
        // error message.
        assert_eq!(
            complete_line.as_str(),
            r#"{"event":"complete","oid":"","error":{"code":2,"message":"invalid oid `not-a-real-oid`: LFS oid must be 64 chars, got 14"}}"#,
        );
    }

    /// F-009: the same shape for `download`. Validation lives in the
    /// run loop, the agent is never reached, and the wire-format
    /// failure event matches the upload case.
    #[tokio::test]
    async fn download_with_invalid_oid_emits_complete_with_empty_oid() {
        let store = MockStore::new();
        let resolver = StubResolver {
            store,
            prefix: Some("repo".to_owned()),
        };
        let tmp = TempDir::new().unwrap();
        let bad_oid = "DEADBEEF";
        let events = vec![
            r#"{"event":"init","operation":"download","remote":"origin"}"#.to_owned(),
            format!(r#"{{"event":"download","oid":"{bad_oid}","size":1}}"#),
            r#"{"event":"terminate"}"#.to_owned(),
        ];
        let (lines, res) = drive(&events, &resolver, tmp.path()).await;
        res.expect("run completes despite bad oid");
        let complete_line = lines
            .iter()
            .find(|l| l.contains("\"event\":\"complete\""))
            .expect("complete event present");
        assert!(
            complete_line.contains(r#""oid":"""#),
            "wire-format oid field must be empty for validation failure: {complete_line}"
        );
        assert!(
            complete_line.contains(&format!("invalid oid `{bad_oid}`")),
            "raw rejected oid must appear in the error message: {complete_line}"
        );
        assert!(
            complete_line.contains(r#""code":2"#),
            "error code must be the generic value 2: {complete_line}"
        );
    }

    /// F-009 sanity check: a valid oid passes the boundary check
    /// and the agent is reached. This is covered by the existing
    /// `full_round_trip_init_upload_download_terminate` test; this
    /// shorter sibling pins the contract more directly.
    #[tokio::test]
    async fn upload_with_valid_oid_reaches_agent() {
        let store = MockStore::new();
        let resolver = StubResolver {
            store: store.clone(),
            prefix: Some("repo".to_owned()),
        };
        let tmp = TempDir::new().unwrap();
        let oid = good_oid();
        let src = tmp.path().join("body");
        let body = b"payload";
        tokio::fs::write(&src, body).await.unwrap();
        let events = vec![
            r#"{"event":"init","operation":"upload","remote":"origin"}"#.to_owned(),
            format!(
                r#"{{"event":"upload","oid":"{oid}","size":{size},"path":"{path}"}}"#,
                size = body.len(),
                path = src.to_str().unwrap(),
            ),
            r#"{"event":"terminate"}"#.to_owned(),
        ];
        let (lines, res) = drive(&events, &resolver, tmp.path()).await;
        res.expect("run completes");
        // The bucket actually received the body — proof the agent
        // was reached after boundary validation.
        assert!(store.contains(&format!("repo/lfs/{oid}")));
        // Wire-side complete has the validated oid (not empty).
        assert!(
            lines
                .iter()
                .any(|l| l.contains(&format!(r#""oid":"{oid}""#))
                    && l.contains("\"event\":\"complete\""))
        );
    }
}

//! Git remote-helper protocol REPL and command dispatcher.
//!
//! [`run`] is generic over its reader and writer so tests can drive it
//! through `tokio::io::duplex`.
//!
//! Stdout is the wire protocol — see `.claude/rules/protocol-stdout.md`.
//! Diagnostics use `tracing` (configured to write to stderr by
//! [`tracing_init::init`]); the only stdout writes happen via the
//! per-command handlers below.

use std::io::ErrorKind;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, error};

use crate::object_store::ObjectStore;
use crate::url::{RemoteUrl, StorageEngine};

pub mod backend;
pub(crate) mod capabilities;
pub mod fetch;
pub(crate) mod list;
pub(crate) mod option;
pub mod push;
pub mod tracing_init;

use self::fetch::{FetchedRefs, fetch_batch};
use self::option::{OptionEffect, handle_option};
use self::push::push_batch;
use self::tracing_init::ReloadHandle;

/// Errors surfaced by the REPL loop.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// Stdin / stdout transport failure.
    #[error("protocol I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Object-store call failed during `list`.
    #[error("list failed: {0}")]
    List(#[from] list::ListError),

    /// `fetch` batch failed.
    #[error("fetch failed: {0}")]
    Fetch(#[from] fetch::FetchError),

    /// `push` batch failed.
    #[error("push failed: {0}")]
    Push(#[from] push::PushError),

    /// An input line did not match any recognised command.
    #[error("invalid command: {0:?}")]
    InvalidCommand(String),
}

impl ProtocolError {
    /// Returns `true` when the error is a broken-pipe or write-zero I/O
    /// failure — both indicate git closed the helper's stdout, which is a
    /// clean exit rather than a crash.
    #[must_use]
    pub fn is_broken_pipe(&self) -> bool {
        matches!(self, Self::Io(e)
            if matches!(e.kind(), ErrorKind::BrokenPipe | ErrorKind::WriteZero))
    }
}

/// Single-line command parsed from stdin.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Capabilities,
    List { for_push: bool },
    Option(String),
    Fetch(String),
    Push(String),
    Empty,
}

/// Which batched command stream is currently being collected.
///
/// Push and fetch are mutually exclusive within a batch — switching
/// between them resets the accumulator (matches upstream's
/// `process_cmd` mode flip in
/// `../git-remote-s3/git_remote_s3/remote.py:498-536`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Fetch,
    Push,
}

/// Session-fixed infrastructure shared by [`fetch_batch`] and [`push_batch`].
///
/// Created once per [`run`] call and passed by shared reference to both
/// batch handlers so the call sites don't repeat the `(store, prefix,
/// repo_dir)` triple.
pub(crate) struct BatchCtx {
    pub(crate) store: Arc<dyn ObjectStore>,
    /// Optional repository prefix within the bucket / container.
    pub(crate) prefix: Option<Arc<str>>,
    pub(crate) repo_dir: Arc<PathBuf>,
}

/// Accumulates `fetch` / `push` commands until a blank line flushes the batch.
///
/// The REPL protocol delivers commands as a batch separated by a blank
/// line.  Mode switches between fetch and push (rare but spec-allowed)
/// reset both accumulators so stale commands from the prior mode are
/// discarded.
struct BatchState {
    mode: Option<Mode>,
    fetch_cmds: Vec<String>,
    push_cmds: Vec<String>,
}

impl BatchState {
    fn new() -> Self {
        Self {
            mode: None,
            fetch_cmds: Vec::new(),
            push_cmds: Vec::new(),
        }
    }

    /// Record one command for `incoming` mode, resetting the other
    /// accumulator if the mode has changed.
    fn accumulate(&mut self, incoming: Mode, cmd: String) {
        if self.mode != Some(incoming) {
            self.fetch_cmds.clear();
            self.push_cmds.clear();
            self.mode = Some(incoming);
        }
        match incoming {
            Mode::Fetch => self.fetch_cmds.push(cmd),
            Mode::Push => self.push_cmds.push(cmd),
        }
    }

    /// Drain the pending batch, returning `(mode, cmds)` when non-empty.
    ///
    /// Returns `None` if there is no current mode or the accumulator is
    /// empty, leaving state unchanged so the REPL can still emit the
    /// mandatory blank-line acknowledgement.
    fn take_pending(&mut self) -> Option<(Mode, Vec<String>)> {
        match self.mode {
            Some(Mode::Fetch) if !self.fetch_cmds.is_empty() => {
                self.mode = None;
                Some((Mode::Fetch, std::mem::take(&mut self.fetch_cmds)))
            }
            Some(Mode::Push) if !self.push_cmds.is_empty() => {
                self.mode = None;
                Some((Mode::Push, std::mem::take(&mut self.push_cmds)))
            }
            _ => None,
        }
    }
}

fn parse_command(line: &str) -> Option<Command> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return Some(Command::Empty);
    }
    if trimmed == "capabilities" {
        return Some(Command::Capabilities);
    }
    // Order matters: "list for-push" must match before "list".
    if trimmed == "list for-push" {
        return Some(Command::List { for_push: true });
    }
    if trimmed == "list" {
        return Some(Command::List { for_push: false });
    }
    if let Some(rest) = trimmed.strip_prefix("option ") {
        return Some(Command::Option(rest.to_owned()));
    }
    if let Some(rest) = trimmed.strip_prefix("fetch ") {
        return Some(Command::Fetch(rest.to_owned()));
    }
    if let Some(rest) = trimmed.strip_prefix("push ") {
        return Some(Command::Push(rest.to_owned()));
    }
    None
}

/// Run the helper REPL until stdin closes (clean exit) or the writer
/// breaks (`BrokenPipe` — also a clean exit, mirroring upstream's
/// `os.dup2(devnull, stdout)` trick).
///
/// `repo_dir` is the local repository the helper is operating against;
/// the parallel fetch path uses it as the cwd for `git bundle unbundle`.
///
/// # Errors
///
/// Returns [`ProtocolError::Io`] on transport failure,
/// [`ProtocolError::InvalidCommand`] for an unrecognised command,
/// [`ProtocolError::List`] / [`ProtocolError::Fetch`] /
/// [`ProtocolError::Push`] for backend errors in the respective
/// operations.
pub async fn run<R, W>(
    remote: RemoteUrl,
    store: Arc<dyn ObjectStore>,
    reader: R,
    mut writer: W,
    reload: Option<ReloadHandle>,
    repo_dir: PathBuf,
) -> Result<(), ProtocolError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = reader.lines();
    let fetched_refs = FetchedRefs::new();
    let mut batch = BatchState::new();
    // Per-operation `option depth <N>` is set immediately before a
    // fetch batch and reset to `None` once that batch drains. Depth is
    // not session-sticky — git re-issues `option depth` for each
    // shallow operation.
    let mut depth: Option<NonZeroU32> = None;
    let zip = remote.flags().zip;
    let engine = remote.flags().engine.unwrap_or(StorageEngine::Bundle);
    let ctx = BatchCtx {
        store,
        prefix: remote.prefix().map(Arc::from),
        repo_dir: Arc::new(repo_dir),
    };

    while let Some(line) = lines.next_line().await? {
        debug!(cmd = %line, "received protocol command");
        let Some(cmd) = parse_command(&line) else {
            error!(cmd = %line, "fatal: invalid command");
            return Err(ProtocolError::InvalidCommand(line));
        };
        match cmd {
            Command::Capabilities => {
                capabilities::handle_capabilities(&mut writer).await?;
            }
            Command::List { for_push } => {
                list::handle_list(
                    ctx.store.as_ref(),
                    ctx.prefix.as_deref(),
                    for_push,
                    &mut writer,
                )
                .await?;
            }
            Command::Option(args) => {
                let effect = handle_option(&args, reload.as_ref(), &mut writer).await?;
                if let OptionEffect::SetDepth(d) = effect {
                    depth = Some(d);
                }
            }
            Command::Fetch(args) => batch.accumulate(Mode::Fetch, args),
            Command::Push(args) => batch.accumulate(Mode::Push, args),
            Command::Empty => {
                if let Some((mode, cmds)) = batch.take_pending() {
                    match mode {
                        Mode::Fetch => {
                            // Take depth so it applies to *this* batch only; a
                            // subsequent fetch without a fresh `option depth`
                            // line must clone fully, matching upstream git's
                            // per-operation depth contract.
                            fetch_batch(&ctx, cmds, fetched_refs.clone(), depth.take()).await?;
                        }
                        Mode::Push => {
                            let outcomes = push_batch(&ctx, zip, engine, cmds).await?;
                            for outcome in &outcomes {
                                writer
                                    .write_all(outcome.to_protocol_line().as_bytes())
                                    .await?;
                            }
                        }
                    }
                }
                writer.write_all(b"\n").await?;
                writer.flush().await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_command_recognises_each_form() {
        assert_eq!(parse_command("capabilities\n"), Some(Command::Capabilities));
        assert_eq!(
            parse_command("list\n"),
            Some(Command::List { for_push: false })
        );
        assert_eq!(
            parse_command("list for-push\n"),
            Some(Command::List { for_push: true })
        );
        assert_eq!(
            parse_command("option verbosity 2\n"),
            Some(Command::Option("verbosity 2".into()))
        );
        assert_eq!(
            parse_command("fetch deadbeef refs/heads/main\n"),
            Some(Command::Fetch("deadbeef refs/heads/main".into()))
        );
        assert_eq!(
            parse_command("push refs/heads/main:refs/heads/main\n"),
            Some(Command::Push("refs/heads/main:refs/heads/main".into()))
        );
        assert_eq!(parse_command("\n"), Some(Command::Empty));
    }

    #[test]
    fn parse_command_handles_crlf() {
        assert_eq!(
            parse_command("list\r\n"),
            Some(Command::List { for_push: false })
        );
        assert_eq!(parse_command("\r\n"), Some(Command::Empty));
    }

    #[test]
    fn parse_command_rejects_garbage() {
        assert_eq!(parse_command("nonsense\n"), None);
        // Whitespace-only is treated as garbage (parity with upstream's
        // strict `cmd == "\n"` blank-line check; only a literal blank
        // line is the batch separator).
        assert_eq!(parse_command("   \n"), None);
    }

    #[test]
    fn is_broken_pipe_matches_kinds() {
        let pipe = ProtocolError::Io(std::io::Error::from(ErrorKind::BrokenPipe));
        assert!(pipe.is_broken_pipe());
        let write_zero = ProtocolError::Io(std::io::Error::from(ErrorKind::WriteZero));
        assert!(write_zero.is_broken_pipe());
        let other = ProtocolError::Io(std::io::Error::from(ErrorKind::Other));
        assert!(!other.is_broken_pipe());
        let not_io = ProtocolError::InvalidCommand("bad".into());
        assert!(!not_io.is_broken_pipe());
    }
}

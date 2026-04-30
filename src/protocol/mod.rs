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
use crate::url::RemoteUrl;

pub mod backend;
pub(crate) mod capabilities;
pub mod fetch;
pub mod list;
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
    let repo_dir = Arc::new(repo_dir);
    let fetched_refs = FetchedRefs::new();
    let mut mode: Option<Mode> = None;
    let mut fetch_cmds: Vec<String> = Vec::new();
    let mut push_cmds: Vec<String> = Vec::new();
    // Per-operation `option depth <N>` is set immediately before a
    // fetch batch and reset to `None` once that batch drains. Depth is
    // not session-sticky — git re-issues `option depth` for each
    // shallow operation.
    let mut depth: Option<NonZeroU32> = None;
    let zip = remote.flags().zip;

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
                list::handle_list(store.as_ref(), remote.prefix(), for_push, &mut writer).await?;
            }
            Command::Option(args) => {
                let effect = handle_option(&args, reload.as_ref(), &mut writer).await?;
                if let OptionEffect::SetDepth(d) = effect {
                    depth = Some(d);
                }
            }
            Command::Fetch(args) => {
                if mode != Some(Mode::Fetch) {
                    fetch_cmds.clear();
                    push_cmds.clear();
                    mode = Some(Mode::Fetch);
                }
                fetch_cmds.push(args);
            }
            Command::Push(args) => {
                if mode != Some(Mode::Push) {
                    push_cmds.clear();
                    fetch_cmds.clear();
                    mode = Some(Mode::Push);
                }
                push_cmds.push(args);
            }
            Command::Empty => {
                if mode == Some(Mode::Fetch) && !fetch_cmds.is_empty() {
                    let drained = std::mem::take(&mut fetch_cmds);
                    // Take depth so it applies to *this* batch only; a
                    // subsequent fetch without a fresh `option depth`
                    // line must clone fully, matching upstream git's
                    // per-operation depth contract.
                    let batch_depth = depth.take();
                    fetch_batch(
                        Arc::clone(&store),
                        remote.prefix().map(str::to_owned),
                        Arc::clone(&repo_dir),
                        drained,
                        fetched_refs.clone(),
                        batch_depth,
                    )
                    .await?;
                    mode = None;
                } else if mode == Some(Mode::Push) && !push_cmds.is_empty() {
                    let drained = std::mem::take(&mut push_cmds);
                    let outcomes = push_batch(
                        Arc::clone(&store),
                        remote.prefix().map(str::to_owned),
                        Arc::clone(&repo_dir),
                        zip,
                        drained,
                    )
                    .await?;
                    for outcome in &outcomes {
                        writer
                            .write_all(outcome.to_protocol_line().as_bytes())
                            .await?;
                    }
                    mode = None;
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

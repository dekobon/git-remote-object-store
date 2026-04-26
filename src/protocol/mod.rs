//! Git remote-helper protocol REPL and command dispatcher.
//!
//! [`run`] is generic over its reader and writer so tests can drive it
//! through `tokio::io::duplex`; [`run_main`] is the binary-side entry
//! that wires real stdin/stdout, parses argv, builds the backend, and
//! installs the tracing subscriber.
//!
//! Stdout is the wire protocol — see `.claude/rules/protocol-stdout.md`.
//! Diagnostics use `tracing` (configured to write to stderr by
//! [`tracing_init::init`]); the only stdout writes happen via the
//! per-command handlers below.

use std::io::ErrorKind;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::{debug, error};

use crate::object_store::ObjectStore;
use crate::url::{self, RemoteUrl};

pub mod backend;
pub mod capabilities;
pub mod fetch;
pub mod list;
pub mod option;
pub mod push;
pub mod tracing_init;

use self::option::handle_option;
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

    /// `fetch` is a Phase 7 deliverable.
    #[error(transparent)]
    Fetch(#[from] fetch::FetchNotImplemented),

    /// `push` is a Phase 8 deliverable.
    #[error(transparent)]
    Push(#[from] push::PushNotImplemented),

    /// An input line did not match any recognised command.
    #[error("invalid command: {0:?}")]
    InvalidCommand(String),
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
pub async fn run<R, W>(
    remote: RemoteUrl,
    store: Arc<dyn ObjectStore>,
    reader: R,
    mut writer: W,
    reload: Option<ReloadHandle>,
) -> Result<(), ProtocolError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = reader.lines();
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
                handle_option(&args, reload.as_ref(), &mut writer).await?;
            }
            Command::Fetch(_) => return Err(fetch::FetchNotImplemented.into()),
            Command::Push(_) => return Err(push::PushNotImplemented.into()),
            Command::Empty => {
                writer.write_all(b"\n").await?;
                writer.flush().await?;
            }
        }
    }
    Ok(())
}

/// Shared `main` for every `git-remote-{s3,az}-{http,https}` binary.
///
/// Reads `argv[2]` (the URL — git always passes remote-name as argv[1]
/// and URL as argv[2]), parses it, builds the backend, installs the
/// tracing subscriber, masks SIGPIPE on Unix, and drives [`run`].
pub async fn run_main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let remote_arg = args
        .get(2)
        .or_else(|| args.get(1))
        .ok_or_else(|| anyhow!("missing remote URL on command line"))?;

    let remote = url::parse(remote_arg).context("failed to parse remote URL")?;
    let reload = tracing_init::init().ok();

    #[cfg(unix)]
    install_sigpipe_mask();

    let store = backend::build(&remote)
        .await
        .context("failed to build object-store backend")?;

    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();

    match run(remote, store, stdin, stdout, reload).await {
        Ok(()) => Ok(()),
        Err(ProtocolError::Io(e)) if is_broken_pipe(&e) => {
            debug!("stdout closed (BrokenPipe); exiting cleanly");
            std::process::exit(0);
        }
        Err(other) => Err(other.into()),
    }
}

fn is_broken_pipe(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::BrokenPipe | ErrorKind::WriteZero)
}

#[cfg(unix)]
fn install_sigpipe_mask() {
    // tokio's signal handler installs `SIG_IGN`-equivalent semantics:
    // SIGPIPE no longer kills the process; instead, the failing write
    // returns `EPIPE` → `ErrorKind::BrokenPipe`, which `run_main` catches
    // above and turns into a clean exit. The returned Signal stream is
    // dropped immediately — we just need the side effect of the
    // installation.
    let _ = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::pipe());
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
        let pipe = std::io::Error::from(ErrorKind::BrokenPipe);
        assert!(is_broken_pipe(&pipe));
        let write_zero = std::io::Error::from(ErrorKind::WriteZero);
        assert!(is_broken_pipe(&write_zero));
        let other = std::io::Error::from(ErrorKind::Other);
        assert!(!is_broken_pipe(&other));
    }
}

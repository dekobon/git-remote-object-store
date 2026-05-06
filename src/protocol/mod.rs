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
pub(crate) mod bundle_uri;
pub(crate) mod capabilities;
pub mod fetch;
pub(crate) mod list;
pub(crate) mod option;
pub mod push;
pub mod tracing_init;

use self::fetch::{FetchedRefs, fetch_batch};
use self::option::{OptionEffect, handle_option};
use self::push::{PushOutcome, push_batch};
use self::tracing_init::ReloadHandle;

/// Write each [`PushOutcome`]'s wire line to `writer` in order.
///
/// Both engines' `push_batch` returns `Vec<PushOutcome>`; the rendering
/// loop is identical, so it lives here. Pulled out so the per-engine
/// `Mode::Push` arms in [`run`] each shrink to a single line.
async fn write_push_outcomes<W>(
    writer: &mut W,
    outcomes: &[PushOutcome],
) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    for outcome in outcomes {
        writer
            .write_all(outcome.to_protocol_line().as_bytes())
            .await?;
    }
    Ok(())
}

/// Walk `err`'s `source()` chain and append each level's `Display` to
/// `msg`, **skipping any level whose text is already at the tail of
/// `msg`**.
///
/// `thiserror`-derived `#[error]` formats often inline `{0}` or
/// `{source}` of the immediate source at the *tail* of the format
/// string — sometimes recursively. For example: `PushError::Store(
/// "object-store error during push: {0}")` where `{0}` is
/// `ObjectStoreError::Network("network error: {0}")`, which itself
/// inlines its boxed source at the tail. A naive chain-walk that
/// always appends produces `"... network error: dns failure: dns
/// failure"` because `dns failure` is already at the tail. The
/// suffix-only dedup handles every variant currently in this crate.
///
/// Caveat: a wrapper that inlines `{source}` *mid*-string (e.g.
/// `"network error: {0} (transient)"`) is **not** deduped — the inner
/// source would be appended a second time. No such wrapper exists
/// today; if one is added, prefer reformulating its `#[error]` to
/// keep `{source}` at the tail (or extend this helper) rather than
/// living with the duplication.
///
/// Used by both [`backend::fatal_message`] (for the operator-facing
/// `fatal:` line) and [`push`] (for the per-ref `error <ref>` wire
/// line). Sharing the helper keeps the two diagnostics in sync.
pub(crate) fn append_source_chain<E: std::error::Error + ?Sized>(msg: &mut String, err: &E) {
    let mut next = err.source();
    while let Some(src) = next {
        // We need the rendered string twice (once for the suffix check,
        // once to append) so format it once and reuse — `write!` would
        // re-format it via the `Display` impl.
        let rendered = src.to_string();
        if !msg.ends_with(&rendered) {
            msg.push_str(": ");
            msg.push_str(&rendered);
        }
        next = src.source();
    }
}

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

    /// The bucket's resolved storage engine is not implemented in this
    /// build. Phase 1 of issue #52 lands the `packchain` plumbing
    /// without push/fetch logic; selecting that engine surfaces here.
    #[error("storage engine `{0}` is not yet implemented (issue #52)")]
    EngineNotImplemented(StorageEngine),

    /// `FORMAT` validation / engine resolution failed during connect.
    #[error("backend resolution failed: {0}")]
    Backend(#[from] backend::BackendError),

    /// `bundle-uri` command handler failed.
    #[error("bundle-uri failed: {0}")]
    BundleUri(#[from] bundle_uri::BundleUriError),
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
    BundleUri,
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

    /// Record one command for `incoming` mode, resetting the OTHER
    /// mode's accumulator if the mode has changed.
    fn accumulate(&mut self, incoming: Mode, cmd: String) {
        if self.mode != Some(incoming) {
            match incoming {
                Mode::Fetch => self.push_cmds.clear(),
                Mode::Push => self.fetch_cmds.clear(),
            }
            self.mode = Some(incoming);
        }
        match incoming {
            Mode::Fetch => {
                // Defense-in-depth: the OTHER-mode accumulator was just
                // cleared on a switch (or was already empty); if a
                // future bug ever leaves it non-empty across a drain,
                // panic in debug rather than silently mixing modes.
                debug_assert!(
                    self.push_cmds.is_empty(),
                    "push_cmds must be empty when accumulating a Fetch command",
                );
                self.fetch_cmds.push(cmd);
            }
            Mode::Push => {
                debug_assert!(
                    self.fetch_cmds.is_empty(),
                    "fetch_cmds must be empty when accumulating a Push command",
                );
                self.push_cmds.push(cmd);
            }
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
    if trimmed == "bundle-uri" {
        return Some(Command::BundleUri);
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
/// operations, and [`ProtocolError::EngineNotImplemented`] when
/// `engine` has no Phase-1 push/fetch logic (issue #52).
///
/// `engine` is the resolved engine returned by [`backend::build`].
/// Threading it through the call chain (rather than re-reading
/// `FORMAT` here) avoids a duplicate round trip per helper invocation.
pub async fn run<R, W>(
    remote: RemoteUrl,
    store: Arc<dyn ObjectStore>,
    engine: StorageEngine,
    reader: R,
    mut writer: W,
    reload: Option<ReloadHandle>,
    repo_dir: PathBuf,
) -> Result<(), ProtocolError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // Phase 2 (packchain push) routes per command/engine pair below;
    // Phase 3 (packchain fetch) is still unimplemented, so a packchain
    // helper that receives a `fetch` command surfaces
    // `EngineNotImplemented` at drain time rather than letting a
    // bundle code path run against on-bucket packchain state.
    let mut lines = reader.lines();
    let fetched_refs = FetchedRefs::new();
    let mut batch = BatchState::new();
    // Per-operation `option depth <N>` is set immediately before a
    // fetch batch and reset to `None` once that batch drains. Depth is
    // not session-sticky — git re-issues `option depth` for each
    // shallow operation.
    let mut depth: Option<NonZeroU32> = None;
    let zip = remote.flags().zip;
    // bundle-uri (issue #71) is gated on engine == Packchain AND the
    // operator opting in via `?bundle_uri=1`. The gate is computed
    // once at session start so a `?bundle_uri=1` flag on a bundle
    // remote is silently inert (the issue puts the bundle engine
    // explicitly out of scope: bundle filenames rotate per push, so
    // a stable URL would race the next push).
    let advertise_bundle_uri =
        matches!(engine, StorageEngine::Packchain) && remote.flags().bundle_uri;
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
                capabilities::handle_capabilities(&mut writer, advertise_bundle_uri).await?;
            }
            Command::BundleUri => {
                let opts = bundle_uri::BundleUriOpts {
                    presign_ttl_seconds: remote.flags().bundle_uri_presign_ttl,
                };
                bundle_uri::handle_bundle_uri(
                    ctx.store.as_ref(),
                    &remote,
                    opts,
                    advertise_bundle_uri,
                    &mut writer,
                )
                .await?;
            }
            Command::List { for_push } => {
                list::handle_list(
                    ctx.store.as_ref(),
                    ctx.prefix.as_deref(),
                    engine,
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
                    match (mode, engine) {
                        (Mode::Fetch, StorageEngine::Bundle) => {
                            // Take depth so it applies to *this* batch only; a
                            // subsequent fetch without a fresh `option depth`
                            // line must clone fully, matching upstream git's
                            // per-operation depth contract.
                            fetch_batch(&ctx, cmds, fetched_refs.clone(), depth.take()).await?;
                        }
                        (Mode::Fetch, StorageEngine::Packchain) => {
                            // Take depth so it applies to *this* batch only,
                            // matching the bundle path. Packchain's shallow
                            // fetch is sequential newest-first with
                            // BFS-after-each (see crate::packchain::fetch's
                            // module doc).
                            crate::packchain::fetch::fetch_batch(
                                &ctx,
                                cmds,
                                fetched_refs.clone(),
                                depth.take(),
                            )
                            .await?;
                        }
                        (Mode::Push, StorageEngine::Bundle) => {
                            let outcomes = push_batch(&ctx, zip, engine, cmds).await?;
                            write_push_outcomes(&mut writer, &outcomes).await?;
                        }
                        (Mode::Push, StorageEngine::Packchain) => {
                            let outcomes =
                                crate::packchain::push::push_batch(&ctx, engine, cmds).await?;
                            write_push_outcomes(&mut writer, &outcomes).await?;
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
        // Double-space inside a recognised command is rejected. Pinning
        // strict byte-exact matching on the protocol verbs against any
        // future "be lenient with whitespace" regression — `"list  for-push"`
        // (two spaces) must NOT collapse to `Command::List { for_push: true }`.
        assert_eq!(parse_command("list  for-push\n"), None);
        // Trailing space after a verb is also rejected.
        assert_eq!(parse_command("list \n"), None);
    }

    /// `parse_command` matches the strip-prefix verbs (`option`, `fetch`,
    /// `push`) on a single space — the rest is passed through verbatim
    /// to the per-verb argument parser. Pin this contract so a
    /// regression that collapses internal whitespace before the strip
    /// (e.g. `trimmed.split_whitespace().collect()`) is caught here
    /// rather than bouncing off the per-verb parser with a confusing
    /// error.
    #[test]
    fn parse_command_passes_strip_prefix_args_verbatim() {
        // Double space after the verb produces a leading-space arg, NOT
        // a no-op collapse. The downstream parser (e.g. parse_fetch_args)
        // is responsible for rejecting bad arg shapes; parse_command's
        // job ends at the verb match.
        assert_eq!(
            parse_command("fetch  abc def\n"),
            Some(Command::Fetch(" abc def".into())),
        );
        assert_eq!(
            parse_command("push  +ref:ref\n"),
            Some(Command::Push(" +ref:ref".into())),
        );
        // Empty args after the verb are also passed through (rejected
        // by parse_fetch_args / parse_push_args, not here).
        assert_eq!(
            parse_command("fetch \n"),
            Some(Command::Fetch(String::new()))
        );
    }

    // --- append_source_chain ----------------------------------------

    /// Layered wrapper for testing the dedup behaviour of
    /// `append_source_chain`. The inner is a `BoxError` so we can stack
    /// arbitrary depth without writing one struct per level.
    #[derive(Debug, thiserror::Error)]
    #[error("layer: {0}")]
    struct LayerError(#[source] crate::object_store::BoxError);

    #[test]
    fn append_source_chain_skips_levels_already_in_display() {
        // BoxError is a leaf (`io::Error::other`'s Display is just the
        // message). LayerError's Display inlines `{0}` recursively so
        // the top-level `to_string()` already contains every level.
        // append_source_chain must NOT duplicate any of them.
        let inner: crate::object_store::BoxError = Box::new(std::io::Error::other("dns failure"));
        let mid: crate::object_store::BoxError = Box::new(LayerError(inner));
        let top = LayerError(mid);

        let mut msg = top.to_string();
        // `top.to_string()` inlines every level via `{0}`:
        // "layer: layer: dns failure"
        assert_eq!(msg, "layer: layer: dns failure");

        append_source_chain(&mut msg, &top);
        // Walk would land on each source's Display — all already at the
        // tail of `msg` — so dedup must skip every level.
        assert_eq!(
            msg, "layer: layer: dns failure",
            "append_source_chain must not duplicate already-inlined sources",
        );
    }

    #[test]
    fn append_source_chain_appends_when_source_text_is_not_in_display() {
        // A wrapper whose Display does NOT inline its source. The chain
        // walk must surface the inner cause.
        #[derive(Debug, thiserror::Error)]
        #[error("opaque wrapper")]
        struct OpaqueWrapper(#[source] crate::object_store::BoxError);

        let inner: crate::object_store::BoxError = Box::new(std::io::Error::other("dns failure"));
        let top = OpaqueWrapper(inner);

        let mut msg = top.to_string();
        assert_eq!(msg, "opaque wrapper");
        append_source_chain(&mut msg, &top);
        assert_eq!(msg, "opaque wrapper: dns failure");
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

    // --- BatchState ---------------------------------------------------

    #[test]
    fn batch_state_empty_take_returns_none() {
        let mut batch = BatchState::new();
        assert!(batch.take_pending().is_none());
    }

    #[test]
    fn batch_state_accumulate_and_take_round_trip() {
        let mut batch = BatchState::new();
        batch.accumulate(Mode::Fetch, "a".to_owned());
        batch.accumulate(Mode::Fetch, "b".to_owned());
        let (mode, cmds) = batch.take_pending().expect("non-empty fetch batch");
        assert_eq!(mode, Mode::Fetch);
        assert_eq!(cmds, ["a", "b"]);
        // Mode is reset after drain; a second take returns None.
        assert!(batch.take_pending().is_none());
    }

    #[test]
    fn batch_state_mode_switch_clears_prior_cmds() {
        let mut batch = BatchState::new();
        // Accumulate fetch commands, then switch to push mid-batch.
        batch.accumulate(Mode::Fetch, "fetch-cmd".to_owned());
        batch.accumulate(Mode::Push, "push-cmd".to_owned());
        // Only the push command survives the mode switch.
        let (mode, cmds) = batch.take_pending().expect("non-empty push batch");
        assert_eq!(mode, Mode::Push);
        assert_eq!(cmds, ["push-cmd"]);
        assert!(batch.take_pending().is_none());
    }

    #[test]
    fn batch_state_accumulate_with_no_cmds_after_mode_set_takes_none() {
        // Verify that take_pending does not return Some for a mode with
        // an empty accumulator (mode is set but no cmds were pushed).
        // This can happen if the mode was set by accumulate and then all
        // cmds were consumed, leaving mode non-None but cmds empty.
        let mut batch = BatchState::new();
        batch.accumulate(Mode::Fetch, "only-cmd".to_owned());
        batch.take_pending(); // drain and reset mode
        // After take, mode == None; a spurious second take must return None.
        assert!(batch.take_pending().is_none());
    }
}

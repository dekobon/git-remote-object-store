//! Handler for the `option` remote-helper command.
//!
//! Mirrors `cmd_option` in `../git-remote-s3/git_remote_s3/remote.py`.
//! Recognised options are `verbosity` and `depth`; everything else (and
//! any malformed `option ...` line) responds `unsupported\n`. Git
//! requires an exact `ok\n` / `unsupported\n` per option line — silence
//! stalls the transfer.

use std::num::NonZeroU32;

use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::tracing_init::{self, ReloadHandle};

/// Side effect that the REPL must observe after `handle_option` returns.
///
/// Verbosity changes are applied here (via the `tracing-subscriber`
/// reload handle) and surface as [`OptionEffect::None`]; depth requests
/// are surfaced so the REPL can thread them into the next fetch batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OptionEffect {
    /// Option had no out-of-band effect (verbosity, unsupported,
    /// malformed — all share the same `None` outcome).
    None,
    /// Caller requested a depth-limited fetch for the next batch.
    SetDepth(NonZeroU32),
}

/// Drive a single `option ...` command.
///
/// `args` is the portion after the literal `option ` token; e.g. for
/// the input line `option verbosity 2\n`, `args` is `verbosity 2`.
///
/// Returns the side-effect the caller must apply (currently only
/// [`OptionEffect::SetDepth`] for `depth <N>`); the `ok\n` /
/// `unsupported\n` protocol response is emitted to `writer` directly.
pub(crate) async fn handle_option<W>(
    args: &str,
    reload: Option<&ReloadHandle>,
    writer: &mut W,
) -> std::io::Result<OptionEffect>
where
    W: AsyncWrite + Unpin,
{
    let parsed = parse_option(args);
    let (response, effect): (&[u8], OptionEffect) = match parsed {
        Some(OptionRequest::Verbosity(n)) if n >= 2 => {
            if let Some(handle) = reload {
                // Reload error is best-effort: if the subscriber's filter
                // can't be flipped (e.g. it was poisoned), we still respond
                // `ok` so git's protocol stream stays well-formed. Losing a
                // verbosity bump is preferable to aborting the session.
                let _ = tracing_init::raise_to_info(handle);
            }
            (b"ok\n", OptionEffect::None)
        }
        Some(OptionRequest::Depth(n)) => (b"ok\n", OptionEffect::SetDepth(n)),
        _ => (b"unsupported\n", OptionEffect::None),
    };
    writer.write_all(response).await?;
    writer.flush().await?;
    Ok(effect)
}

#[derive(Debug, PartialEq, Eq)]
enum OptionRequest {
    Verbosity(i32),
    /// Per the git remote-helper spec, depth must be ≥ 1; modelled as
    /// `NonZeroU32` so depth=0 cannot be constructed and parses as
    /// `unsupported\n` at the protocol layer.
    Depth(NonZeroU32),
}

fn parse_option(args: &str) -> Option<OptionRequest> {
    let mut parts = args.split_whitespace();
    let key = parts.next()?;
    let value = parts.next()?;
    if parts.next().is_some() {
        // Extra tokens — match upstream's `split(" ")[1:]` strictness.
        return None;
    }
    match key {
        "verbosity" => value.parse::<i32>().ok().map(OptionRequest::Verbosity),
        "depth" => value.parse::<NonZeroU32>().ok().map(OptionRequest::Depth),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recognises_verbosity() {
        assert_eq!(
            parse_option("verbosity 2"),
            Some(OptionRequest::Verbosity(2))
        );
        assert_eq!(
            parse_option("verbosity 0"),
            Some(OptionRequest::Verbosity(0))
        );
        assert_eq!(
            parse_option("verbosity -1"),
            Some(OptionRequest::Verbosity(-1))
        );
    }

    #[test]
    fn parse_recognises_depth() {
        assert_eq!(
            parse_option("depth 1"),
            Some(OptionRequest::Depth(NonZeroU32::new(1).unwrap()))
        );
        assert_eq!(
            parse_option("depth 42"),
            Some(OptionRequest::Depth(NonZeroU32::new(42).unwrap()))
        );
    }

    #[test]
    fn parse_rejects_depth_zero_and_negative() {
        // Spec requires depth ≥ 1; NonZeroU32 rejects both at parse time.
        assert_eq!(parse_option("depth 0"), None);
        assert_eq!(parse_option("depth -1"), None);
        assert_eq!(parse_option("depth foo"), None);
    }

    #[test]
    fn parse_rejects_unknown_keys() {
        assert_eq!(parse_option("progress true"), None);
        assert_eq!(parse_option("dry-run true"), None);
    }

    #[test]
    fn parse_rejects_malformed_lines() {
        assert_eq!(parse_option(""), None);
        assert_eq!(parse_option("verbosity"), None);
        assert_eq!(parse_option("verbosity foo"), None);
        assert_eq!(parse_option("verbosity 2 extra"), None);
        assert_eq!(parse_option("depth"), None);
        assert_eq!(parse_option("depth 1 extra"), None);
    }

    #[tokio::test]
    async fn responds_ok_for_verbosity_two() {
        let mut buf: Vec<u8> = Vec::new();
        let effect = handle_option("verbosity 2", None, &mut buf).await.unwrap();
        assert_eq!(&buf, b"ok\n");
        assert_eq!(effect, OptionEffect::None);
    }

    #[tokio::test]
    async fn responds_unsupported_for_low_verbosity() {
        let mut buf: Vec<u8> = Vec::new();
        let effect = handle_option("verbosity 1", None, &mut buf).await.unwrap();
        assert_eq!(&buf, b"unsupported\n");
        assert_eq!(effect, OptionEffect::None);
    }

    #[tokio::test]
    async fn responds_unsupported_for_unknown_option() {
        let mut buf: Vec<u8> = Vec::new();
        let effect = handle_option("progress true", None, &mut buf)
            .await
            .unwrap();
        assert_eq!(&buf, b"unsupported\n");
        assert_eq!(effect, OptionEffect::None);
    }

    #[tokio::test]
    async fn responds_unsupported_for_malformed() {
        let mut buf: Vec<u8> = Vec::new();
        let effect = handle_option("verbosity foo", None, &mut buf)
            .await
            .unwrap();
        assert_eq!(&buf, b"unsupported\n");
        assert_eq!(effect, OptionEffect::None);
    }

    #[tokio::test]
    async fn responds_ok_and_returns_depth_for_valid_depth() {
        let mut buf: Vec<u8> = Vec::new();
        let effect = handle_option("depth 5", None, &mut buf).await.unwrap();
        assert_eq!(&buf, b"ok\n");
        assert_eq!(effect, OptionEffect::SetDepth(NonZeroU32::new(5).unwrap()));
    }

    #[tokio::test]
    async fn responds_unsupported_for_depth_zero() {
        let mut buf: Vec<u8> = Vec::new();
        let effect = handle_option("depth 0", None, &mut buf).await.unwrap();
        assert_eq!(&buf, b"unsupported\n");
        assert_eq!(effect, OptionEffect::None);
    }
}

//! Handler for the `option` remote-helper command.
//!
//! Mirrors `cmd_option` in `../git-remote-s3/git_remote_s3/remote.py`.
//! Only `verbosity` is recognised; everything else (and any malformed
//! `option ...` line) responds `unsupported\n`. Git requires an exact
//! `ok\n` / `unsupported\n` per option line — silence stalls the
//! transfer.

use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::tracing_init::{self, ReloadHandle};

/// Drive a single `option ...` command.
///
/// `args` is the portion after the literal `option ` token; e.g. for
/// the input line `option verbosity 2\n`, `args` is `verbosity 2`.
pub(crate) async fn handle_option<W>(
    args: &str,
    reload: Option<&ReloadHandle>,
    writer: &mut W,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let response: &[u8] = match parse_option(args) {
        Some(OptionRequest::Verbosity(n)) if n >= 2 => {
            if let Some(handle) = reload {
                // Reload error is best-effort: if the subscriber's filter
                // can't be flipped (e.g. it was poisoned), we still respond
                // `ok` so git's protocol stream stays well-formed. Losing a
                // verbosity bump is preferable to aborting the session.
                let _ = tracing_init::raise_to_info(handle);
            }
            b"ok\n"
        }
        _ => b"unsupported\n",
    };
    writer.write_all(response).await?;
    writer.flush().await
}

#[derive(Debug, PartialEq, Eq)]
enum OptionRequest {
    Verbosity(i32),
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
    }

    #[tokio::test]
    async fn responds_ok_for_verbosity_two() {
        let mut buf: Vec<u8> = Vec::new();
        handle_option("verbosity 2", None, &mut buf).await.unwrap();
        assert_eq!(&buf, b"ok\n");
    }

    #[tokio::test]
    async fn responds_unsupported_for_low_verbosity() {
        let mut buf: Vec<u8> = Vec::new();
        handle_option("verbosity 1", None, &mut buf).await.unwrap();
        assert_eq!(&buf, b"unsupported\n");
    }

    #[tokio::test]
    async fn responds_unsupported_for_unknown_option() {
        let mut buf: Vec<u8> = Vec::new();
        handle_option("progress true", None, &mut buf)
            .await
            .unwrap();
        assert_eq!(&buf, b"unsupported\n");
    }

    #[tokio::test]
    async fn responds_unsupported_for_malformed() {
        let mut buf: Vec<u8> = Vec::new();
        handle_option("verbosity foo", None, &mut buf)
            .await
            .unwrap();
        assert_eq!(&buf, b"unsupported\n");
    }
}

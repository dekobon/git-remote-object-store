//! `capabilities` command handler.
//!
//! Emits `*push`, `*fetch`, `option`, optionally `bundle-uri`, then a
//! blank terminator. The `bundle-uri` line is a packchain-only
//! extension (issue #71) that opts an operator into advertising
//! baseline-bundle URLs the client can fetch before the helper
//! protocol negotiates the incremental tail. See the git remote-helper
//! protocol docs (`git help gitremote-helpers`) for the format.

use tokio::io::{AsyncWrite, AsyncWriteExt};

const BASE_CAPABILITIES: &[u8] = b"*push\n*fetch\noption\n";
const BUNDLE_URI_LINE: &[u8] = b"bundle-uri\n";
const TERMINATOR: &[u8] = b"\n";

/// Write the capability list to `writer` and flush.
///
/// `advertise_bundle_uri` is set by the caller when the resolved engine
/// is [`crate::url::StorageEngine::Packchain`] and the URL has
/// `?bundle_uri=1`. Per `.claude/rules/protocol-stdout.md`, every byte
/// emitted here is part of the wire-line; the function performs no
/// other writes.
pub(crate) async fn handle_capabilities<W>(
    writer: &mut W,
    advertise_bundle_uri: bool,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(BASE_CAPABILITIES).await?;
    if advertise_bundle_uri {
        writer.write_all(BUNDLE_URI_LINE).await?;
    }
    writer.write_all(TERMINATOR).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_base_capabilities_block_when_bundle_uri_disabled() {
        let mut buf: Vec<u8> = Vec::new();
        handle_capabilities(&mut buf, false).await.unwrap();
        assert_eq!(&buf, b"*push\n*fetch\noption\n\n");
    }

    #[tokio::test]
    async fn appends_bundle_uri_line_when_enabled() {
        // The bundle-uri line lands BEFORE the trailing blank
        // terminator so git's capability parser still sees the
        // canonical termination.
        let mut buf: Vec<u8> = Vec::new();
        handle_capabilities(&mut buf, true).await.unwrap();
        assert_eq!(&buf, b"*push\n*fetch\noption\nbundle-uri\n\n");
    }
}

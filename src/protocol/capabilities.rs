//! `capabilities` command handler.
//!
//! Mirrors `cmd_capabilities` in `../git-remote-s3/git_remote_s3/remote.py`.
//! Output is exactly four lines: `*push`, `*fetch`, `option`, blank — see
//! the git remote-helper protocol docs (`git help gitremote-helpers`).

use tokio::io::{AsyncWrite, AsyncWriteExt};

/// Capability list announced to git: parallel push, parallel fetch, and
/// the `option` setting protocol. Includes the trailing blank-line
/// terminator.
const CAPABILITIES: &[u8] = b"*push\n*fetch\noption\n\n";

/// Write the capability list to `writer` and flush.
pub(crate) async fn handle_capabilities<W>(writer: &mut W) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(CAPABILITIES).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_exact_capabilities_block() {
        let mut buf: Vec<u8> = Vec::new();
        handle_capabilities(&mut buf).await.unwrap();
        assert_eq!(&buf, b"*push\n*fetch\noption\n\n");
    }
}

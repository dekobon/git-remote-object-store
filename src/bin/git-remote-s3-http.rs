//! `git-remote-s3+http` helper shim (loopback / local-dev only).
//!
//! Thin wrapper around [`git_remote_object_store::protocol::run_main`].

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    git_remote_object_store::protocol::run_main().await
}

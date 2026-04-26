//! `git-remote-az+http` helper shim (loopback / Azurite only).
//!
//! Thin wrapper around [`git_remote_object_store::protocol::run_main`].
//! The Azure backend itself is wired in Phase 11 — until then the REPL
//! exits early with a "not yet implemented" error.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    git_remote_object_store::protocol::run_main().await
}

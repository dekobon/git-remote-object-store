//! `git-remote-az+http` helper shim (Azurite / local-dev only).
//!
//! Thin wrapper around [`git_remote_object_store_cli::run_main`].

#[tokio::main]
async fn main() -> std::process::ExitCode {
    git_remote_object_store_cli::run_main().await
}

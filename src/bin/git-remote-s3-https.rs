//! `git-remote-s3+https` helper shim.
//!
//! Thin wrapper around [`git_remote_object_store::protocol::run_main`].

#[tokio::main]
async fn main() -> std::process::ExitCode {
    git_remote_object_store::protocol::run_main().await
}

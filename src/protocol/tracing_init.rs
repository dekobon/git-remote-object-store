//! Stderr-only `tracing` subscriber for the helper-protocol binaries.
//!
//! The remote-helper binaries speak the git transport protocol on stdout —
//! see `.claude/rules/protocol-stdout.md` — so every log line MUST go to
//! stderr. The subscriber returned here is wired with [`std::io::stderr`]
//! as its writer; the `EnvFilter` is wrapped in a [`reload::Layer`] so
//! the protocol REPL can flip the level at runtime in response to
//! `option verbosity <n>`.
//!
//! Startup level is `error`, with one env-var override honoured:
//!
//! - `GIT_REMOTE_OBJECT_STORE_VERBOSE` — canonical name for this crate.
//!
//! A numeric value `>= 2` bumps the start level to `info`, matching the
//! `option verbosity 2` threshold from the helper protocol.

use std::env;

use tracing::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;

/// Canonical env var read at startup to bump the verbosity floor.
pub const ENV_VERBOSE: &str = "GIT_REMOTE_OBJECT_STORE_VERBOSE";

/// Handle returned by [`init`] so callers can flip the subscriber's filter
/// at runtime. The underlying layer type is intentionally hidden behind a
/// type alias to keep the public surface minimal.
pub type ReloadHandle = reload::Handle<EnvFilter, tracing_subscriber::Registry>;

/// Initialise the global subscriber and return a handle to its filter.
///
/// # Errors
///
/// Returns [`InitError`] if a global subscriber is already set (the
/// subscriber can only be initialised once per process). The helper
/// bins call this exactly once from `run_main`; tests that set up their
/// own subscriber will receive `Err` here.
pub fn init() -> Result<ReloadHandle, InitError> {
    let initial = build_initial_filter();
    let (filter, handle) = reload::Layer::new(initial);

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false),
        )
        .try_init()
        .map_err(|source| InitError {
            source: source.into(),
        })?;

    Ok(handle)
}

/// Flip the subscriber to `info` level. Used by `option verbosity 2+`.
///
/// # Errors
///
/// Returns [`InitError`] if the reload handle has been invalidated (the
/// subscriber was dropped).
pub fn raise_to_info(handle: &ReloadHandle) -> Result<(), InitError> {
    handle
        .modify(|filter| *filter = info_filter())
        .map_err(|source| InitError {
            source: Box::new(source),
        })
}

/// Errors surfaced by [`init`] / [`raise_to_info`]. Boxed so the public
/// surface does not leak `tracing-subscriber`'s error types.
#[derive(Debug, thiserror::Error)]
#[error("failed to initialise tracing subscriber: {source}")]
pub struct InitError {
    #[source]
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
}

fn build_initial_filter() -> EnvFilter {
    if env_verbose_at_least(2) {
        info_filter()
    } else {
        EnvFilter::default().add_directive(LevelFilter::ERROR.into())
    }
}

fn info_filter() -> EnvFilter {
    EnvFilter::default().add_directive(Level::INFO.into())
}

fn env_verbose_at_least(threshold: u32) -> bool {
    read_verbose_env() >= threshold
}

fn read_verbose_env() -> u32 {
    env::var(ENV_VERBOSE)
        .ok()
        .and_then(|v| parse_verbose(&v))
        .unwrap_or(0)
}

fn parse_verbose(value: &str) -> Option<u32> {
    value.trim().parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_verbose_accepts_decimal() {
        assert_eq!(parse_verbose("2"), Some(2));
        assert_eq!(parse_verbose(" 3 "), Some(3));
        assert_eq!(parse_verbose("0"), Some(0));
    }

    #[test]
    fn parse_verbose_rejects_non_decimal() {
        assert_eq!(parse_verbose("foo"), None);
        assert_eq!(parse_verbose(""), None);
        assert_eq!(parse_verbose("-1"), None);
    }
}

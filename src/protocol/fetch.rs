//! Parallel `fetch` handler.
//!
//! Phase 6 ships only a stub: the dispatcher recognises `fetch <sha> <ref>`
//! lines, but actually downloading and unbundling refs is Phase 7's job.
//! Until then the stub returns a structured error so `git fetch` fails
//! fast with a clear reason rather than hanging on an empty response.

/// Error returned by the Phase 6 `fetch` stub.
#[derive(Debug, thiserror::Error)]
#[error("fetch is not yet implemented (Phase 7)")]
pub struct FetchNotImplemented;

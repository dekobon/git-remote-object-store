//! `push` handler with per-ref locking via conditional writes.
//!
//! Phase 6 ships only a stub: the dispatcher recognises `push <refspec>`
//! lines, but actually building bundles and acquiring locks is Phase 8's
//! job. Until then the stub returns a structured error so `git push`
//! fails fast with a clear reason rather than hanging on an empty
//! response.

/// Error returned by the Phase 6 `push` stub.
#[derive(Debug, thiserror::Error)]
#[error("push is not yet implemented (Phase 8)")]
pub struct PushNotImplemented;

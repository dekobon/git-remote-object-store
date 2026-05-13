//! Shared `PackMissing` retry policy for read- and fetch-side races
//! against a concurrent `manage gc sweep`.
//!
//! The packchain engine has two read-shaped surfaces that load
//! `chain.json` once, then issue follow-up GETs against the packs that
//! chain points at:
//!
//! - [`crate::packchain::read::read_blob`] (issue #136 wired the retry
//!   here first).
//! - [`crate::packchain::fetch::fetch_batch`] (issue #148 extended the
//!   same policy here).
//!
//! Both can race a `manage gc sweep` that compacts the chain and
//! deletes packs the original snapshot named. A naive caller surfaces
//! [`PackchainError::PackMissing`] for a key that was perfectly
//! reachable at the moment of the first load. The right shape is:
//! reload `chain.json`, observe that the failing key is no longer
//! referenced, and retry against the fresh chain.
//!
//! This module centralises the retry constants ([`PACK_MISSING_MAX_RETRIES`],
//! [`PACK_MISSING_RETRY_BACKOFFS`]) and the
//! [`chain_references_pack_key`] discriminator so both call sites stay
//! in sync. Genuine bucket inconsistency (the reloaded chain still
//! names the missing key) and non-`PackMissing` errors fail fast —
//! waiting through the backoff schedule when the operation has no
//! chance of succeeding wastes wall-clock for no recovery upside.

use std::time::Duration;

use super::PackchainError;
use super::keys::{pack_sha_from_full_key, segment_pack_sha};
use super::schema::ChainManifest;

/// Maximum number of times a read- or fetch-side caller reloads
/// `chain.json` and retries the failing operation after observing a
/// [`PackchainError::PackMissing`] that the reloaded chain shows is no
/// longer referenced — i.e. a concurrent `manage gc sweep` deleted
/// packs the original chain snapshot pointed at (issue #136 for read,
/// issue #148 for fetch). After this many retries the caller surfaces
/// [`PackchainError::ConcurrentGcRetriesExhausted`].
pub(crate) const PACK_MISSING_MAX_RETRIES: u32 = 3;

/// Backoff schedule (per retry attempt) for the chain reload loop.
/// The schedule must have exactly [`PACK_MISSING_MAX_RETRIES`]
/// entries: index `i` is the sleep before the `i`-th retry. The
/// growing pattern (100 ms → 500 ms → 2 s) gives a vigorous compact
/// cycle time to settle without unnecessarily blocking a quick caller
/// when the race was a one-shot.
pub(crate) const PACK_MISSING_RETRY_BACKOFFS: [Duration; PACK_MISSING_MAX_RETRIES as usize] = [
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(2),
];

/// Compile-time pin: the backoff schedule must have exactly one entry
/// per retry attempt. If the cap and the schedule ever drift apart
/// (e.g. someone bumps the cap without extending the array, or trims
/// the array without lowering the cap), the resulting index into
/// `PACK_MISSING_RETRY_BACKOFFS` at the loop's last attempt would
/// panic at runtime. Catch the desync at build time instead.
const _: () = assert!(
    PACK_MISSING_RETRY_BACKOFFS.len() == PACK_MISSING_MAX_RETRIES as usize,
    "PACK_MISSING_RETRY_BACKOFFS length must equal PACK_MISSING_MAX_RETRIES",
);

/// Whether any segment of `chain` would produce a pack or idx key
/// equal to `missing_key`. Used by the retry loops to distinguish a
/// concurrent `manage gc sweep` (the key is *not* in the reloaded
/// chain → retry is safe) from a genuine bucket inconsistency (the
/// key *is* still referenced → data loss, fail fast). Returns the
/// parse error from [`segment_pack_sha`] if a segment's `pack` field
/// is malformed.
pub(crate) fn chain_references_pack_key(
    chain: &ChainManifest,
    prefix: Option<&str>,
    missing_key: &str,
) -> Result<bool, PackchainError> {
    // Parse the missing key once up front: extract `<sha>` from
    // `[<prefix>/]packs/<sha>.{pack,idx}`. A malformed key (or one whose
    // prefix doesn't match `prefix`) cannot match any segment by
    // construction, so we return `Ok(false)` without iterating. Note
    // that this short-circuits past per-segment `segment_pack_sha`
    // parse errors that the prior per-segment string-eq implementation
    // would have surfaced; on the #136 retry path this is benign
    // because a malformed missing-key is observable evidence of bucket
    // corruption the caller will surface from its original `PackMissing`.
    let Some(missing_sha) = pack_sha_from_full_key(prefix, missing_key) else {
        return Ok(false);
    };
    for segment in &chain.segments {
        if segment_pack_sha(segment)? == missing_sha {
            return Ok(true);
        }
    }
    Ok(false)
}

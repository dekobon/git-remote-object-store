//! Handlers for `list` and `list for-push` remote-helper commands.
//!
//! The wire format is:
//!
//! ```text
//! <sha> <ref>\n          ← one line per bundle, sorted by LastModified desc
//! @<head-ref> HEAD\n     ← only when not for-push and HEAD is present
//! \n                     ← terminator
//! ```
//!
//! The bundle filter is `^refs/.+/.+/[a-f0-9]{40}\.bundle$`. Stripping
//! happens against `<prefix>/` (with a trailing slash) so a sibling-prefix
//! repo (`<prefix>-other/...`) cannot accidentally match.

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::warn;

use crate::git::RefName;
use crate::keys;
use crate::object_store::{ObjectStore, ObjectStoreError};
use crate::packchain::gc::tombstoned_bundle_keys;
use crate::packchain::list as packchain_list;
use crate::url::StorageEngine;

/// Errors specific to the list path that the dispatcher converts into
/// fatal exits.
#[derive(Debug, thiserror::Error)]
pub enum ListError {
    /// Underlying object-store call failed.
    #[error("object-store error during list: {0}")]
    Store(#[from] ObjectStoreError),

    /// Writing to the protocol stream failed (typically `BrokenPipe`).
    #[error("write to protocol stream failed: {0}")]
    Io(#[from] std::io::Error),

    /// Packchain-engine list failed (transport or schema-version
    /// mismatch). Per-entry parse failures are skipped with a warn
    /// and never reach this variant — only fatal transport errors
    /// from the listing call propagate here.
    #[error("packchain list error: {0}")]
    Packchain(#[from] crate::packchain::PackchainError),
}

/// Drive a single `list` (or `list for-push`) command end-to-end.
///
/// `prefix` is the parsed [`crate::url::RemoteUrl::prefix`] — `None`
/// means the repo lives at the bucket root. `for_push` is `true` for the
/// `list for-push` form; in that case the HEAD lookup is skipped.
pub(crate) async fn handle_list<W>(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    engine: StorageEngine,
    for_push: bool,
    writer: &mut W,
) -> Result<(), ListError>
where
    W: AsyncWrite + Unpin,
{
    // Engine-aware dispatch: bundle parses `<sha>.bundle` filenames,
    // packchain reads each ref's `chain.json` and reports `chain.tip`.
    // Both produce engine-neutral [`ListedRef`] values that the wire
    // loop below renders identically. Without this split, packchain
    // remotes return stale `<full_at>` SHAs after any incremental
    // push (issue #72).
    let entries: Vec<ListedRef> = match engine {
        StorageEngine::Bundle => collect_bundles(store, prefix).await?,
        StorageEngine::Packchain => packchain_list::list_refs(store, prefix)
            .await?
            .into_iter()
            .map(|r| ListedRef {
                sha: r.sha,
                ref_path: r.ref_path,
            })
            .collect(),
    };

    // Print `@<ref> HEAD` only when not for-push, HEAD is present, and the
    // listed entries include the head ref.
    if !for_push
        && let Some(head_ref) = read_remote_head(store, prefix).await?
        && entries.iter().any(|e| e.ref_path == head_ref)
    {
        writer
            .write_all(format!("@{head_ref} HEAD\n").as_bytes())
            .await?;
    }

    for entry in &entries {
        writer
            .write_all(format!("{} {}\n", entry.sha, entry.ref_path).as_bytes())
            .await?;
    }

    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

/// One listed ref's parsed parts. Engine-neutral — both bundle and
/// packchain handlers produce these, and the wire loop in
/// [`handle_list`] formats them identically. Internal — never
/// serialised directly.
struct ListedRef {
    sha: String,
    ref_path: String,
}

async fn collect_bundles(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
) -> Result<Vec<ListedRef>, ObjectStoreError> {
    // List with `Prefix=prefix` and no trailing slash. The strip step
    // below disambiguates sibling-prefix collisions.
    let listed = store.list(prefix.unwrap_or("")).await?;

    // Skip the tombstone listing entirely when nothing was found —
    // an empty bucket and packchain-only buckets pay nothing for the
    // bundle-engine hide-set on every `git ls-remote`.
    if listed.is_empty() {
        return Ok(Vec::new());
    }

    // Issue #157: bundles named by a baseline tombstone are pending
    // reclamation by `gc sweep` — the bundle key stays readable so
    // an in-flight fetcher that already advertised it can finish, but
    // `list` must not advertise it again (it would emit a second
    // `<sha> <ref>\n` line for the same ref and confuse git).
    let hidden = tombstoned_bundle_keys(store, prefix).await?;

    // Parse every match exactly once, carrying the timestamp alongside
    // the parsed entry so the sort below doesn't force a re-parse.
    let mut parsed: Vec<(time::OffsetDateTime, ListedRef)> = listed
        .into_iter()
        .filter_map(|m| {
            if hidden.contains(&m.key) {
                return None;
            }
            let rel = relative_key(prefix, &m.key)?;
            let (ref_path, sha) = parse_bundle_key(rel)?;
            Some((
                m.last_modified,
                ListedRef {
                    sha: sha.to_owned(),
                    ref_path: ref_path.to_owned(),
                },
            ))
        })
        .collect();

    // LastModified desc, stable: callers care about freshness ordering
    // when a ref has multiple bundles mid-rotation.
    parsed.sort_by(|(a, _), (b, _)| b.cmp(a));

    Ok(parsed.into_iter().map(|(_, entry)| entry).collect())
}

async fn read_remote_head(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
) -> Result<Option<String>, ObjectStoreError> {
    // `Some("")` collapses to "no prefix" — same shape as `None` — so a
    // root-of-bucket repo lists `HEAD` rather than `/HEAD`.
    let key = keys::join(prefix, "HEAD");
    let body = match store.get_bytes(&key).await {
        Ok(body) => body,
        Err(ObjectStoreError::NotFound(_)) => return Ok(None),
        Err(other) => return Err(other),
    };
    let Ok(text) = std::str::from_utf8(&body) else {
        warn!(key = %key, "remote HEAD body is not UTF-8; ignoring");
        return Ok(None);
    };
    // Mirror Python `.strip()` — leading and trailing whitespace
    // (including `\n`) is stripped, embedded whitespace is kept.
    let trimmed = text.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_owned()))
    }
}

/// Strip `<prefix>/` (or no-op when prefix is `None`) from a full
/// store key, returning `None` when the key does not belong to this
/// repo (e.g. a sibling-prefix collision).
fn relative_key<'a>(prefix: Option<&str>, full_key: &'a str) -> Option<&'a str> {
    match prefix {
        None | Some("") => Some(full_key),
        Some(p) => {
            // Build "<prefix>/" without allocating: check the prefix bytes
            // and then ensure the next byte is `/`.
            let with_slash_len = p.len() + 1;
            if full_key.len() <= p.len() {
                return None;
            }
            if !full_key.starts_with(p) {
                return None;
            }
            if full_key.as_bytes().get(p.len()).copied() != Some(b'/') {
                return None;
            }
            Some(&full_key[with_slash_len..])
        }
    }
}

/// Match `^refs/.+/.+/[a-f0-9]{40}\.bundle$` and return
/// `(ref_path, sha)` on success.
fn parse_bundle_key(rel_key: &str) -> Option<(&str, &str)> {
    let segments: Vec<&str> = rel_key.split('/').collect();
    if segments.len() < 4 {
        return None;
    }
    if segments[0] != "refs" {
        return None;
    }
    let last = segments.last()?;
    let sha = last.strip_suffix(".bundle")?;
    // Delegate the 40-hex stem check to `keys::is_valid_bundle_stem`
    // so push, doctor, and list stay in lockstep — see the doc comment
    // on `is_valid_bundle_stem` for why the predicate is centralised.
    if !keys::is_valid_bundle_stem(sha) {
        return None;
    }
    // ref_path is everything before the trailing "/<sha>.bundle".
    // The `-1` drops the `/` separator between ref_path and last segment.
    let split_at = rel_key.len() - last.len() - 1;
    let ref_path = &rel_key[..split_at];
    // Defense-in-depth: reject ref paths that fail gix-validate's
    // ref-name check (e.g. `..` traversal, control characters).
    // Mirrors the packchain-side hardening added in #72 — see
    // `packchain::list::list_refs`.
    if !RefName::is_valid(ref_path) {
        warn!(
            rel_key = %rel_key,
            ref_path = %ref_path,
            "bundle list: derived ref path is not a valid ref name; skipping",
        );
        return None;
    }
    Some((ref_path, sha))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn parse_bundle_key_accepts_two_segment_ref() {
        let key = format!("refs/heads/main/{SHA}.bundle");
        let (ref_path, sha) = parse_bundle_key(&key).unwrap();
        assert_eq!(ref_path, "refs/heads/main");
        assert_eq!(sha, SHA);
    }

    #[test]
    fn parse_bundle_key_accepts_deeper_ref() {
        let key = format!("refs/heads/feature/x/{SHA}.bundle");
        let (ref_path, sha) = parse_bundle_key(&key).unwrap();
        assert_eq!(ref_path, "refs/heads/feature/x");
        assert_eq!(sha, SHA);
    }

    #[test]
    fn parse_bundle_key_rejects_uppercase_sha() {
        let upper = SHA.to_uppercase();
        assert!(parse_bundle_key(&format!("refs/heads/main/{upper}.bundle")).is_none());
    }

    #[test]
    fn parse_bundle_key_rejects_wrong_length_sha() {
        let short = &SHA[..39];
        assert!(parse_bundle_key(&format!("refs/heads/main/{short}.bundle")).is_none());
        let long = format!("{SHA}a");
        assert!(parse_bundle_key(&format!("refs/heads/main/{long}.bundle")).is_none());
    }

    #[test]
    fn parse_bundle_key_rejects_missing_extension() {
        assert!(parse_bundle_key(&format!("refs/heads/main/{SHA}")).is_none());
        assert!(parse_bundle_key(&format!("refs/heads/main/{SHA}.txt")).is_none());
    }

    #[test]
    fn parse_bundle_key_rejects_non_refs_prefix() {
        assert!(parse_bundle_key(&format!("HEAD/heads/main/{SHA}.bundle")).is_none());
        assert!(parse_bundle_key(&format!("lfs/heads/main/{SHA}.bundle")).is_none());
    }

    #[test]
    fn parse_bundle_key_rejects_too_few_segments() {
        assert!(parse_bundle_key(&format!("refs/main/{SHA}.bundle")).is_none());
        assert!(parse_bundle_key(&format!("refs/{SHA}.bundle")).is_none());
    }

    #[test]
    fn parse_bundle_key_rejects_empty_segment() {
        assert!(parse_bundle_key(&format!("refs/heads//{SHA}.bundle")).is_none());
        assert!(parse_bundle_key(&format!("refs//main/{SHA}.bundle")).is_none());
    }

    #[test]
    fn parse_bundle_key_rejects_dotdot_traversal_in_ref_path() {
        // Defense-in-depth (#73): gix-validate rejects `..` segments.
        for key in [
            format!("refs/heads/../etc/passwd/{SHA}.bundle"),
            format!("refs/heads/feature/../../etc/{SHA}.bundle"),
        ] {
            assert!(
                parse_bundle_key(&key).is_none(),
                "`..` traversal must be rejected: {key:?}",
            );
        }
    }

    #[test]
    fn parse_bundle_key_rejects_control_characters_in_ref_path() {
        // Defense-in-depth (#73): gix-validate rejects control characters.
        for key in [
            format!("refs/heads/main\x07/{SHA}.bundle"),
            format!("refs/heads/main\x00/{SHA}.bundle"),
        ] {
            assert!(
                parse_bundle_key(&key).is_none(),
                "control character in ref path must be rejected: {key:?}",
            );
        }
    }

    #[test]
    fn parse_bundle_key_rejects_dot_lock_suffix() {
        // gix-validate rejects components ending in `.lock`.
        let key = format!("refs/heads/main.lock/{SHA}.bundle");
        assert!(
            parse_bundle_key(&key).is_none(),
            "`.lock` suffix in ref component must be rejected",
        );
    }

    #[test]
    fn relative_key_handles_no_prefix() {
        assert_eq!(
            relative_key(None, "refs/heads/main"),
            Some("refs/heads/main")
        );
        assert_eq!(
            relative_key(Some(""), "refs/heads/main"),
            Some("refs/heads/main")
        );
    }

    #[test]
    fn relative_key_strips_prefix_with_slash() {
        assert_eq!(
            relative_key(Some("repo"), "repo/refs/heads/main"),
            Some("refs/heads/main")
        );
    }

    #[test]
    fn relative_key_rejects_sibling_prefix() {
        assert_eq!(
            relative_key(Some("repo"), "repo-other/refs/heads/main"),
            None
        );
        assert_eq!(
            relative_key(Some("repo"), "repository/refs/heads/main"),
            None
        );
    }

    #[test]
    fn relative_key_rejects_exact_prefix_match() {
        // "<prefix>" alone (no trailing key) is not a child key.
        assert_eq!(relative_key(Some("repo"), "repo"), None);
    }

    /// Issue #157: `collect_bundles` must hide bundles named by a
    /// `<prefix>/gc/baseline-tomb-*.json` record. Without this filter
    /// a force-push that deferred the prior bundle would cause `list`
    /// to advertise TWO `<sha> refs/heads/main\n` lines, confusing git.
    /// The bundle key itself remains readable at its path — only the
    /// listing path hides it.
    #[tokio::test]
    async fn collect_bundles_hides_tombstoned_bundles() {
        use crate::object_store::mock::MockStore;
        use bytes::Bytes;

        const OLD_SHA: &str = "ffffffffffffffffffffffffffffffffffffffff";
        let store = MockStore::new();

        // Seed two bundles for the same ref: a live one (SHA) and a
        // tombstoned one (OLD_SHA). The tombstone references OLD_SHA's
        // bundle key.
        let live_key = format!("repo/refs/heads/main/{SHA}.bundle");
        let old_key = format!("repo/refs/heads/main/{OLD_SHA}.bundle");
        store.insert(&live_key, Bytes::from_static(b"live"));
        store.insert(&old_key, Bytes::from_static(b"old"));

        // Hand-written tombstone body matching the BaselineTombstone
        // schema (v=1, marked_at, ref_name, sha). Pinned literally so a
        // future schema change does not silently break this assertion.
        let tomb_body = serde_json::json!({
            "v": 1,
            "marked_at": "2026-01-01T00:00:00Z",
            "ref_name": "refs/heads/main",
            "sha": OLD_SHA,
        });
        store.insert(
            "repo/gc/baseline-tomb-fixture.json",
            Bytes::from(serde_json::to_vec(&tomb_body).unwrap()),
        );

        let entries = collect_bundles(&store, Some("repo")).await.unwrap();
        assert_eq!(
            entries.len(),
            1,
            "tombstoned bundle must be filtered out of the listing",
        );
        assert_eq!(
            entries[0].sha, SHA,
            "remaining entry must be the live (non-tombstoned) bundle",
        );
        assert_eq!(entries[0].ref_path, "refs/heads/main");
        // The bundle key itself must still be readable on the bucket —
        // fetchers that advertised OLD_SHA via an earlier `list` need
        // the key to remain until `gc sweep` reclaims it.
        assert!(
            store.contains(&old_key),
            "tombstoned bundle must remain on the bucket during the grace window",
        );
    }
}

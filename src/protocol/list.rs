//! Handlers for `list` and `list for-push` remote-helper commands.
//!
//! Mirrors `cmd_list` and `list_refs` in
//! `../git-remote-s3/git_remote_s3/remote.py`. The wire format is:
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

use crate::object_store::{ObjectStore, ObjectStoreError};

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
}

/// Drive a single `list` (or `list for-push`) command end-to-end.
///
/// `prefix` is the parsed [`crate::url::RemoteUrl::prefix`] — `None`
/// means the repo lives at the bucket root. `for_push` is `true` for the
/// `list for-push` form; in that case the HEAD lookup is skipped.
pub(crate) async fn handle_list<W>(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
    for_push: bool,
    writer: &mut W,
) -> Result<(), ListError>
where
    W: AsyncWrite + Unpin,
{
    let entries = collect_bundles(store, prefix).await?;

    // Print `@<ref> HEAD` only when not for-push, HEAD is present, and the
    // listed bundles include the head ref. Mirrors upstream's
    // loop-and-match behaviour in `cmd_list`.
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

/// One listed bundle's parsed parts. Internal — never serialised directly.
struct BundleEntry {
    sha: String,
    ref_path: String,
}

async fn collect_bundles(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
) -> Result<Vec<BundleEntry>, ObjectStoreError> {
    // Match upstream: `list_objects_v2(Prefix=prefix)` with no trailing
    // slash. The strip step below disambiguates sibling-prefix collisions.
    let listed = store.list(prefix.unwrap_or("")).await?;

    // Parse every match exactly once, carrying the timestamp alongside
    // the parsed entry so the sort below doesn't force a re-parse.
    let mut parsed: Vec<(time::OffsetDateTime, BundleEntry)> = listed
        .into_iter()
        .filter_map(|m| {
            let rel = relative_key(prefix, &m.key)?;
            let (ref_path, sha) = parse_bundle_key(rel)?;
            Some((
                m.last_modified,
                BundleEntry {
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
    let key = match prefix {
        Some(p) => format!("{p}/HEAD"),
        None => "HEAD".to_owned(),
    };
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
    if segments.iter().any(|s| s.is_empty()) {
        return None;
    }
    let last = segments.last()?;
    let sha = last.strip_suffix(".bundle")?;
    if sha.len() != 40 || !sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    // ref_path is everything before the trailing "/<sha>.bundle".
    // The `-1` drops the `/` separator between ref_path and last segment.
    let split_at = rel_key.len() - last.len() - 1;
    Some((&rel_key[..split_at], sha))
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
}

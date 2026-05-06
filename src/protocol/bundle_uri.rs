//! `bundle-uri` command handler (issue #71, packchain-only).
//!
//! Git invokes `bundle-uri\n` after capability advertisement when the
//! helper has advertised the `bundle-uri` capability. The helper
//! responds with one entry per ref, each pointing at that ref's
//! baseline bundle on the bucket:
//!
//! ```text
//! bundle.<ref>.uri=<url>
//! bundle.<ref>.creationToken=<full_at>
//! <blank line>
//! ```
//!
//! `creationToken` lets the client cache the bundle across clones
//! until `full_at` advances (force push or [`crate::packchain::compact`]).
//!
//! ## Engine gating
//!
//! Only [`crate::url::StorageEngine::Packchain`] remotes ever reach
//! this handler — [`super::capabilities`] only advertises the
//! capability when both the engine resolves to packchain AND the
//! URL carries `?bundle_uri=1`. The bundle engine's bundle filenames
//! rotate per push, so a stable URL would race the next push; the
//! issue explicitly puts the bundle engine out of scope.
//!
//! ## URL generation (MVP)
//!
//! For the MVP this module emits **canonical bucket URLs** suitable
//! for public-read S3 buckets, S3-compatible CDNs, and Azure blob
//! containers with anonymous-read access. Operator-controlled
//! presigning (S3 `SigV4`) and SAS-token generation (Azure) are
//! deliberate follow-ups — see [`BundleUriOpts::presign_ttl_seconds`]
//! — because the cross-backend signing logic is invasive enough to
//! warrant its own focused review (credential leakage if implemented
//! incorrectly is the failure mode).
//!
//! ## Wire format
//!
//! Per `.claude/rules/protocol-stdout.md`, this handler emits only
//! protocol-formatted bytes on the writer. The trailing blank line
//! is part of the bundle-uri response per the gitprotocol-v2 spec
//! (`bundle list` framing).

use std::num::NonZeroU64;

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::warn;

use crate::keys;
use crate::object_store::ObjectStore;
use crate::packchain::PackchainError;
use crate::packchain::keys::is_chain_json_key;
use crate::packchain::schema::ChainManifest;
use crate::url::{AzureAddressing, RemoteUrl, S3Addressing};

/// Tunables for [`handle_bundle_uri`]. Mirrors the shape used by
/// other manage-style opts (Doctor, Gc, Compact).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BundleUriOpts {
    /// When `Some(N)`, presign emitted URLs with an `N`-second TTL.
    /// `None` (default) emits canonical bucket URLs that only work
    /// against public-read buckets / CDN-fronted endpoints.
    ///
    /// `NonZeroU64` because a zero-second TTL is meaningless (the
    /// URL would expire before any client could observe it); the
    /// type-system check rejects the bad value at the boundary
    /// rather than letting it flow into the (eventually) presigning
    /// code path.
    ///
    /// **Currently unimplemented**: setting this to `Some(_)` causes
    /// the handler to return [`BundleUriError::PresigningUnsupported`]
    /// rather than silently emit a canonical (insecure) URL.
    pub(crate) presign_ttl_seconds: Option<NonZeroU64>,
}

/// Errors specific to the bundle-uri handler. Wrapped by
/// [`super::ProtocolError`] at the dispatch site; `pub` because it
/// is reachable through the public `ProtocolError::BundleUri`
/// variant.
#[derive(Debug, thiserror::Error)]
pub enum BundleUriError {
    /// Object-store transport failure during the chain.json listing
    /// or fetch.
    #[error(transparent)]
    Packchain(#[from] PackchainError),
    /// I/O failure writing the response to the protocol writer.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Operator passed `presign_ttl_seconds: Some(_)` but the
    /// presigning code path is not yet implemented. Today the handler
    /// only emits canonical bucket URLs (sufficient for public-read
    /// buckets and CDN-fronted endpoints).
    #[error(
        "bundle-uri presigned URLs are not yet implemented; \
         drop the presign TTL or use a public-read bucket"
    )]
    PresigningUnsupported,
}

/// Run the `bundle-uri` command.
///
/// `advertised` mirrors the capability gate: when `false` the
/// helper still responds (gitprotocol-v2 framing requires *some*
/// reply) but emits only the trailing blank-line terminator, no
/// entries. Centralising the gate here keeps the always-respond
/// contract in one place rather than splitting it between the
/// REPL dispatch and the handler.
///
/// On `advertised = true` the handler lists
/// `<prefix>/refs/heads/*/chain.json`, parses each, and emits
/// `bundle.<ref>.uri=<url>` + `bundle.<ref>.creationToken=<full_at>`
/// lines for every parsed chain. Per-entry parse failures
/// warn-and-skip; only transport failures on the initial list call
/// surface as errors. The trailing blank line ends the response.
pub(crate) async fn handle_bundle_uri<W>(
    store: &dyn ObjectStore,
    remote: &RemoteUrl,
    opts: BundleUriOpts,
    advertised: bool,
    writer: &mut W,
) -> Result<(), BundleUriError>
where
    W: AsyncWrite + Unpin,
{
    if !advertised {
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        return Ok(());
    }
    if opts.presign_ttl_seconds.is_some() {
        return Err(BundleUriError::PresigningUnsupported);
    }

    let prefix = remote.prefix().unwrap_or_default();
    // Bundle-uri is best-effort hinting per gitprotocol-v2: on a
    // transport failure during the refs listing, log and emit an
    // empty response (terminator only) rather than aborting the
    // helper. The client falls back to the helper-protocol fetch
    // path. This matches the per-entry warn-and-skip policy
    // already applied to chain.json fetches inside
    // [`collect_entries`].
    let entries = match collect_entries(store, prefix).await {
        Ok(entries) => entries,
        Err(e) => {
            warn!(
                error = %e,
                "bundle-uri: refs listing failed; emitting empty response",
            );
            Vec::new()
        }
    };

    for entry in &entries {
        let url = canonical_bundle_url(remote, &entry.ref_path, &entry.full_at);
        let ref_path = &entry.ref_path;
        let token = &entry.full_at;
        let line =
            format!("bundle.{ref_path}.uri={url}\nbundle.{ref_path}.creationToken={token}\n");
        writer.write_all(line.as_bytes()).await?;
    }
    // Blank-line terminator per gitprotocol-v2 bundle-uri framing.
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

/// One parsed chain — the data the handler renders into a
/// `bundle.<ref>.*` block.
#[derive(Debug, Clone)]
struct BundleEntry {
    ref_path: String,
    full_at: String,
}

/// List `<prefix>/refs/heads/*/chain.json` and return the
/// `(ref_path, full_at)` pair for each parseable chain. Per-entry
/// parse failures warn-and-skip; only transport failures abort.
async fn collect_entries(
    store: &dyn ObjectStore,
    prefix: &str,
) -> Result<Vec<BundleEntry>, PackchainError> {
    let refs_prefix = keys::join(prefix, "refs/heads/");
    let metas = store.list(&refs_prefix).await?;

    let prefix_opt = crate::packchain::keys::optional_prefix(prefix);
    // `metas.len()` is a tight upper bound: every kept entry is a
    // subset (chain.json keys after the `is_chain_json_key` filter).
    let mut out: Vec<BundleEntry> = Vec::with_capacity(metas.len());
    for meta in metas {
        if !is_chain_json_key(&meta.key) {
            continue;
        }
        let Some(ref_path) = crate::packchain::keys::ref_path_from_chain_key(prefix_opt, &meta.key)
        else {
            warn!(key = %meta.key, "bundle-uri: chain.json key has unexpected shape; skipping");
            continue;
        };
        // Validate the derived ref-path the same way `list_refs` and
        // the audit do: a maliciously-planted key such as
        // `<prefix>/refs/heads/../etc/passwd/chain.json` must not
        // emit a verbatim `bundle.refs/heads/../etc/passwd.uri=…`
        // line. RefName::new rejects `..`, control chars, and other
        // gix-validate-rejected shapes.
        if crate::git::RefName::new(&ref_path).is_err() {
            warn!(
                key = %meta.key,
                ref_path = %ref_path,
                "bundle-uri: derived ref path is not a valid ref name; skipping",
            );
            continue;
        }
        // Belt-and-suspenders against bundle-uri wire-format
        // injection. The line shape is
        // `bundle.<id>.<key>=<value>\n`; git's parser splits each
        // line at the first `=`. `RefName::new` (via
        // `gix_validate::reference::name`) already bans `\n`, `\r`,
        // ` `, `:`, and the rest of `\0-\x1F`, but it permits `=`.
        // A ref-path containing `=` cannot relocate the URL host —
        // the `:` ban forecloses scheme injection — but it would
        // produce a malformed entry that breaks a clone with
        // `?bundle_uri=1` against a shared-prefix bucket where
        // another tenant has write access. Reject defensively.
        if !is_safe_for_bundle_uri_emission(&ref_path) {
            warn!(
                key = %meta.key,
                ref_path = %ref_path,
                "bundle-uri: derived ref path contains framing-unsafe bytes; skipping",
            );
            continue;
        }
        // Fetch and parse chain.json. Per-ref transport failures
        // warn-and-skip rather than aborting — bundle-uri is
        // best-effort hinting; a missing entry just means the
        // client falls back to the helper-protocol fetch.
        let body = match store.get_bytes(&meta.key).await {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    key = %meta.key,
                    error = %e,
                    "bundle-uri: chain.json fetch failed; skipping",
                );
                continue;
            }
        };
        match ChainManifest::from_json_bytes(&body) {
            Ok(chain) => out.push(BundleEntry {
                ref_path,
                full_at: chain.full_at.as_str().to_owned(),
            }),
            Err(e) => warn!(
                key = %meta.key,
                error = %e,
                "bundle-uri: chain.json failed to parse; skipping",
            ),
        }
    }
    // Stable order: sort by ref path so the wire output is
    // deterministic regardless of the listing's response order.
    out.sort_by(|a, b| a.ref_path.cmp(&b.ref_path));
    Ok(out)
}

/// Build a canonical (unsigned) bucket URL for the baseline bundle
/// at `<prefix>/<ref_path>/<full_at>.bundle`. Works for public-read
/// buckets and CDN-fronted endpoints. Private buckets need
/// presigning, which is a documented follow-up.
fn canonical_bundle_url(remote: &RemoteUrl, ref_path: &str, full_at: &str) -> String {
    let bundle_key = keys::bundle_key(remote.prefix(), ref_path, full_at);
    match remote {
        // Virtual-hosted S3: the parsed `endpoint.host_str()` already
        // includes the bucket as the leftmost label (e.g.
        // `my-bucket.s3.us-west-2.amazonaws.com`), so the URL is
        // `<scheme>://<host>[:port]/<key>` — the bucket name lives
        // in the host, not the path.
        RemoteUrl::S3 {
            endpoint,
            addressing: S3Addressing::VirtualHosted,
            ..
        } => format!("{}/{bundle_key}", host_authority(endpoint)),
        // Path-style S3: the host has no bucket label; insert it as
        // the first path segment.
        RemoteUrl::S3 {
            endpoint,
            bucket,
            addressing: S3Addressing::PathStyle,
            ..
        } => format!("{}/{bucket}/{bundle_key}", host_authority(endpoint)),
        // Virtual-hosted Azure: the host already includes the
        // account (`<account>.blob.<suffix>`); URL is
        // `<host>[:port]/<container>/<key>`.
        RemoteUrl::Azure {
            endpoint,
            container,
            addressing: AzureAddressing::VirtualHosted,
            ..
        } => format!("{}/{container}/{bundle_key}", host_authority(endpoint)),
        // Path-style Azure (Azurite, custom endpoints):
        // `<host>[:port]/<account>/<container>/<key>`.
        RemoteUrl::Azure {
            endpoint,
            account,
            container,
            addressing: AzureAddressing::PathStyle,
            ..
        } => format!(
            "{}/{account}/{container}/{bundle_key}",
            host_authority(endpoint),
        ),
    }
}

/// `true` if `ref_path` is safe to interpolate into the
/// `bundle.<id>.<key>=<value>\n` wire shape after `RefName::new`
/// has already accepted it. Specifically, reject `=`: gix-validate
/// permits it in ref names, but git's `bundle-uri` parser splits at
/// the first `=` so its presence in the id position breaks framing.
/// All other framing-relevant bytes (`\n`, `\r`, ` `, `:`,
/// `\0`-`\x1F`, `\x7F`) are already rejected by gix-validate.
fn is_safe_for_bundle_uri_emission(ref_path: &str) -> bool {
    !ref_path.as_bytes().contains(&b'=')
}

/// Render `<scheme>://<host>[:port]` from a parsed [`url::Url`].
/// `RemoteUrl::parse` rejects URLs without a host, so `host_str()`
/// is provably `Some` here — the `expect` documents the invariant
/// rather than papering over an unreachable code path.
fn host_authority(endpoint: &url::Url) -> String {
    let scheme = endpoint.scheme();
    let host = endpoint
        .host_str()
        .expect("RemoteUrl invariant: parse() rejects URLs without a host");
    match endpoint.port() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::mock::MockStore;
    use crate::packchain::manifest::write_chain;
    use crate::packchain::schema::{ChainManifest, ChainSegment, Sha40};
    use crate::url::parse;
    use bytes::Bytes;

    const SHA_TIP: &str = "0000000000000000000000000000000000000001";
    const SHA_FULL: &str = "0000000000000000000000000000000000000002";
    const SHA_PACK: &str = "1111111111111111111111111111111111111111";

    fn sha40(s: &str) -> Sha40 {
        Sha40::try_new(s).unwrap()
    }

    fn ref_main() -> crate::git::RefName {
        crate::git::RefName::new("refs/heads/main").unwrap()
    }

    async fn write_test_chain(
        store: &MockStore,
        prefix: Option<&str>,
        ref_name: &crate::git::RefName,
        tip: &str,
        full_at: &str,
    ) {
        let chain = ChainManifest {
            v: 1,
            tip: sha40(tip),
            full_at: sha40(full_at),
            segments: vec![ChainSegment {
                sha: sha40(tip),
                parent_sha: None,
                pack: format!("packs/{SHA_PACK}.pack"),
                bytes: 1_024,
            }],
        };
        write_chain(store, prefix, ref_name, &chain).await.unwrap();
    }

    #[tokio::test]
    async fn empty_bucket_emits_just_terminator() {
        let store = MockStore::new();
        let remote =
            parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?bundle_uri=1").unwrap();
        let mut buf: Vec<u8> = Vec::new();
        handle_bundle_uri(&store, &remote, BundleUriOpts::default(), true, &mut buf)
            .await
            .unwrap();
        assert_eq!(&buf, b"\n", "empty bucket must emit only the terminator");
    }

    #[tokio::test]
    async fn emits_one_entry_per_ref_with_canonical_s3_url() {
        let store = MockStore::new();
        write_test_chain(&store, Some("repo"), &ref_main(), SHA_TIP, SHA_FULL).await;

        let remote = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain&bundle_uri=1",
        )
        .unwrap();
        let mut buf: Vec<u8> = Vec::new();
        handle_bundle_uri(&store, &remote, BundleUriOpts::default(), true, &mut buf)
            .await
            .unwrap();
        let text = std::str::from_utf8(&buf).unwrap();
        // Pin exact bytes — bundle-uri is a wire-format contract.
        assert_eq!(
            text,
            format!(
                "bundle.refs/heads/main.uri=https://my-bucket.s3.us-west-2.amazonaws.com/repo/refs/heads/main/{SHA_FULL}.bundle\n\
                 bundle.refs/heads/main.creationToken={SHA_FULL}\n\
                 \n"
            ),
        );
    }

    #[tokio::test]
    async fn s3_path_style_url_uses_bucket_in_path() {
        // `?addressing=path` → URL form is `<host>/<bucket>/<key>`.
        // For a path-style URL the bucket lives in the path, not
        // the hostname, so we use a bare regional endpoint here.
        let store = MockStore::new();
        write_test_chain(&store, Some("repo"), &ref_main(), SHA_TIP, SHA_FULL).await;
        let remote = parse(
            "s3+https://s3.us-west-2.amazonaws.com/my-bucket/repo?addressing=path&engine=packchain&bundle_uri=1",
        )
        .unwrap();
        let mut buf: Vec<u8> = Vec::new();
        handle_bundle_uri(&store, &remote, BundleUriOpts::default(), true, &mut buf)
            .await
            .unwrap();
        let text = std::str::from_utf8(&buf).unwrap();
        assert!(
            text.contains(&format!(
                "uri=https://s3.us-west-2.amazonaws.com/my-bucket/repo/refs/heads/main/{SHA_FULL}.bundle\n",
            )),
            "{text}",
        );
    }

    #[tokio::test]
    async fn azure_virtual_hosted_url_uses_account_subdomain() {
        let store = MockStore::new();
        write_test_chain(&store, Some("repo"), &ref_main(), SHA_TIP, SHA_FULL).await;
        let remote = parse(
            "az+https://myaccount.blob.core.windows.net/my-container/repo?engine=packchain&bundle_uri=1",
        )
        .unwrap();
        let mut buf: Vec<u8> = Vec::new();
        handle_bundle_uri(&store, &remote, BundleUriOpts::default(), true, &mut buf)
            .await
            .unwrap();
        let text = std::str::from_utf8(&buf).unwrap();
        assert!(
            text.contains(&format!(
                "uri=https://myaccount.blob.core.windows.net/my-container/repo/refs/heads/main/{SHA_FULL}.bundle\n",
            )),
            "{text}",
        );
    }

    #[tokio::test]
    async fn presign_ttl_returns_unsupported_error() {
        // Until the presigning implementation lands (see follow-up),
        // setting `presign_ttl_seconds: Some(_)` must error rather
        // than silently emit a canonical (insecure) URL.
        let store = MockStore::new();
        let remote =
            parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?bundle_uri=1").unwrap();
        let mut buf: Vec<u8> = Vec::new();
        let err = handle_bundle_uri(
            &store,
            &remote,
            BundleUriOpts {
                presign_ttl_seconds: Some(NonZeroU64::new(3_600).unwrap()),
            },
            true,
            &mut buf,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BundleUriError::PresigningUnsupported));
        assert!(buf.is_empty(), "no bytes written on error path");
    }

    #[tokio::test]
    async fn skips_chain_json_with_equals_in_ref_name() {
        // Defense-in-depth against bundle-uri wire-format injection:
        // gix-validate permits `=` in ref names, but git's
        // `bundle-uri` parser splits each line at the first `=`. A
        // ref-path containing `=` would produce a malformed entry on
        // the wire. The host-relocation SSRF chain is foreclosed by
        // gix-validate's `:` ban (no scheme injection possible), but
        // we still skip rather than emit a corrupted line.
        //
        // Mutation-verified during /security-review: removing the
        // `is_safe_for_bundle_uri_emission` check at the call site
        // makes this test fail because `bundle.refs/heads/x=evil...`
        // reaches the wire output.
        let store = MockStore::new();
        write_test_chain(&store, Some("repo"), &ref_main(), SHA_TIP, SHA_FULL).await;
        // Plant a chain.json under a ref name that gix-validate
        // accepts (`=` is not in its banned-bytes set) but that
        // would corrupt the bundle-uri wire framing.
        store.insert(
            "repo/refs/heads/x=evil/chain.json",
            Bytes::from(
                format!(r#"{{"v":1,"tip":"{SHA_TIP}","full_at":"{SHA_TIP}","segments":[]}}"#)
                    .into_bytes(),
            ),
        );
        let remote = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain&bundle_uri=1",
        )
        .unwrap();
        let mut buf: Vec<u8> = Vec::new();
        handle_bundle_uri(&store, &remote, BundleUriOpts::default(), true, &mut buf)
            .await
            .unwrap();
        let text = std::str::from_utf8(&buf).unwrap();
        // The malicious entry must not reach the wire output. We
        // anchor on the ref-name fragment `x=evil` rather than `=`
        // alone because legitimate `bundle.<ref>.uri=<url>` lines
        // also contain `=` as the id/value separator.
        assert!(
            !text.contains("x=evil"),
            "no entry containing `=` in the ref-name segment may reach the wire output: {text}",
        );
        // The good ref is still present.
        assert!(text.contains("bundle.refs/heads/main.uri="), "{text}");
    }

    #[test]
    fn is_safe_for_bundle_uri_emission_accepts_typical_ref_paths() {
        for path in &[
            "refs/heads/main",
            "refs/heads/feature/foo-bar.baz",
            "refs/heads/release-1.0.0",
            "refs/tags/v1",
        ] {
            assert!(
                is_safe_for_bundle_uri_emission(path),
                "expected `{path}` to be accepted",
            );
        }
    }

    #[test]
    fn is_safe_for_bundle_uri_emission_rejects_equals() {
        // `=` is the only framing-relevant byte gix-validate
        // permits. Reject it everywhere it appears in the ref-path.
        for path in &[
            "refs/heads/x=y",
            "refs/heads/=",
            "=refs/heads/main",
            "refs/heads/main=",
            "refs/heads/main=evil.attacker",
        ] {
            assert!(
                !is_safe_for_bundle_uri_emission(path),
                "expected `{path}` to be rejected",
            );
        }
    }

    #[tokio::test]
    async fn skips_chain_json_with_path_traversal_in_ref_name() {
        // Defense-in-depth: a maliciously-planted
        // `<prefix>/refs/heads/../etc/passwd/chain.json` would
        // otherwise emit a verbatim
        // `bundle.refs/heads/../etc/passwd.uri=…` line.
        let store = MockStore::new();
        write_test_chain(&store, Some("repo"), &ref_main(), SHA_TIP, SHA_FULL).await;
        store.insert(
            "repo/refs/heads/../etc/passwd/chain.json",
            Bytes::from(
                format!(r#"{{"v":1,"tip":"{SHA_TIP}","full_at":"{SHA_TIP}","segments":[]}}"#)
                    .into_bytes(),
            ),
        );
        let remote = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain&bundle_uri=1",
        )
        .unwrap();
        let mut buf: Vec<u8> = Vec::new();
        handle_bundle_uri(&store, &remote, BundleUriOpts::default(), true, &mut buf)
            .await
            .unwrap();
        let text = std::str::from_utf8(&buf).unwrap();
        assert!(
            !text.contains(".."),
            "no entry containing `..` may reach the wire output: {text}",
        );
        // The good ref is still present.
        assert!(text.contains("bundle.refs/heads/main.uri="), "{text}");
    }

    #[tokio::test]
    async fn corrupt_chain_json_is_skipped() {
        // A corrupt chain.json on one branch must not blackhole the
        // others — bundle-uri is best-effort.
        let store = MockStore::new();
        write_test_chain(&store, Some("repo"), &ref_main(), SHA_TIP, SHA_FULL).await;
        store.insert(
            "repo/refs/heads/broken/chain.json",
            Bytes::from_static(b"{not valid json"),
        );
        let remote = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain&bundle_uri=1",
        )
        .unwrap();
        let mut buf: Vec<u8> = Vec::new();
        handle_bundle_uri(&store, &remote, BundleUriOpts::default(), true, &mut buf)
            .await
            .unwrap();
        let text = std::str::from_utf8(&buf).unwrap();
        assert!(text.contains("bundle.refs/heads/main.uri="), "{text}");
        assert!(!text.contains("bundle.refs/heads/broken"), "{text}");
    }

    #[tokio::test]
    async fn entries_are_sorted_alphabetically_by_ref_path() {
        // The wire output must be deterministic regardless of the
        // listing's response order. Pin the lexical sort by writing
        // chains in reverse alphabetical order and asserting the
        // emitted entries come out in forward order.
        // Mutation-verified: replacing `out.sort_by(...)` with
        // `out.reverse()` makes this test fail.
        let store = MockStore::new();
        // Insert in reverse order so any test reliance on insertion
        // order would visibly fail.
        let zulu = crate::git::RefName::new("refs/heads/zulu").unwrap();
        let main = crate::git::RefName::new("refs/heads/main").unwrap();
        let alpha = crate::git::RefName::new("refs/heads/alpha").unwrap();
        write_test_chain(&store, Some("repo"), &zulu, SHA_TIP, SHA_FULL).await;
        write_test_chain(&store, Some("repo"), &main, SHA_TIP, SHA_FULL).await;
        write_test_chain(&store, Some("repo"), &alpha, SHA_TIP, SHA_FULL).await;

        let remote = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain&bundle_uri=1",
        )
        .unwrap();
        let mut buf: Vec<u8> = Vec::new();
        handle_bundle_uri(&store, &remote, BundleUriOpts::default(), true, &mut buf)
            .await
            .unwrap();
        let text = std::str::from_utf8(&buf).unwrap();

        let alpha_pos = text
            .find("bundle.refs/heads/alpha.uri=")
            .expect("alpha entry present");
        let main_pos = text
            .find("bundle.refs/heads/main.uri=")
            .expect("main entry present");
        let zulu_pos = text
            .find("bundle.refs/heads/zulu.uri=")
            .expect("zulu entry present");
        assert!(
            alpha_pos < main_pos && main_pos < zulu_pos,
            "entries must appear in lexical ref-path order; got\n{text}",
        );
    }

    #[tokio::test]
    async fn root_prefix_emits_bare_bundle_keys() {
        // Empty prefix (root-of-bucket) — the bundle key has no
        // leading prefix segment.
        let store = MockStore::new();
        write_test_chain(&store, None, &ref_main(), SHA_TIP, SHA_FULL).await;
        let remote =
            parse("s3+https://my-bucket.s3.us-west-2.amazonaws.com/?engine=packchain&bundle_uri=1")
                .unwrap();
        let mut buf: Vec<u8> = Vec::new();
        handle_bundle_uri(&store, &remote, BundleUriOpts::default(), true, &mut buf)
            .await
            .unwrap();
        let text = std::str::from_utf8(&buf).unwrap();
        assert!(
            text.contains(&format!(
                "uri=https://my-bucket.s3.us-west-2.amazonaws.com/refs/heads/main/{SHA_FULL}.bundle\n",
            )),
            "{text}",
        );
    }

    /// Test-only `ObjectStore` decorator that fails on `list` and
    /// delegates everything else to an inner `MockStore`. Used to
    /// pin the "best-effort hinting" contract: a transport failure
    /// during `bundle-uri`'s refs listing must not abort the helper.
    /// Methods outside `list` are unreachable from the bundle-uri
    /// path, so they panic loudly if exercised.
    struct FailListStore;

    // Methods other than `list` are unreachable from the bundle-uri
    // path under test; the `unreachable!`s document that contract.
    // Project-level clippy denies `unreachable` by default, but these
    // are intentionally test-only stubs in a `#[cfg(test)]` module.
    #[allow(clippy::unreachable)]
    #[async_trait::async_trait]
    impl crate::object_store::ObjectStore for FailListStore {
        async fn list(
            &self,
            _prefix: &str,
        ) -> Result<Vec<crate::object_store::ObjectMeta>, crate::object_store::ObjectStoreError>
        {
            Err(crate::object_store::ObjectStoreError::Network(Box::new(
                std::io::Error::other("simulated transport failure"),
            )))
        }
        async fn get_to_file(
            &self,
            _key: &str,
            _dest: &std::path::Path,
            _opts: crate::object_store::GetOpts,
        ) -> Result<(), crate::object_store::ObjectStoreError> {
            unreachable!("bundle-uri does not call get_to_file")
        }
        async fn get_bytes(
            &self,
            _key: &str,
        ) -> Result<bytes::Bytes, crate::object_store::ObjectStoreError> {
            unreachable!("bundle-uri does not reach get_bytes when list fails")
        }
        async fn get_bytes_range(
            &self,
            _key: &str,
            _range: std::ops::Range<u64>,
        ) -> Result<bytes::Bytes, crate::object_store::ObjectStoreError> {
            unreachable!("bundle-uri does not call get_bytes_range")
        }
        async fn put_bytes(
            &self,
            _key: &str,
            _body: bytes::Bytes,
            _opts: crate::object_store::PutOpts,
        ) -> Result<(), crate::object_store::ObjectStoreError> {
            unreachable!("bundle-uri does not call put_bytes")
        }
        async fn put_if_absent(
            &self,
            _key: &str,
            _body: bytes::Bytes,
        ) -> Result<bool, crate::object_store::ObjectStoreError> {
            unreachable!("bundle-uri does not call put_if_absent")
        }
        async fn head(
            &self,
            _key: &str,
        ) -> Result<crate::object_store::ObjectMeta, crate::object_store::ObjectStoreError>
        {
            unreachable!("bundle-uri does not call head")
        }
        async fn copy(
            &self,
            _src: &str,
            _dst: &str,
        ) -> Result<(), crate::object_store::ObjectStoreError> {
            unreachable!("bundle-uri does not call copy")
        }
        async fn delete(&self, _key: &str) -> Result<(), crate::object_store::ObjectStoreError> {
            unreachable!("bundle-uri does not call delete")
        }
    }

    #[tokio::test]
    async fn list_failure_emits_empty_response_rather_than_aborting() {
        // Bundle-uri is best-effort hinting per gitprotocol-v2: a
        // transport failure on the refs listing should warn-and-emit-
        // empty so the helper keeps running and the client falls
        // back to the helper-protocol fetch. Mutation-verifiable:
        // changing the `match collect_entries(...)` to `?` makes
        // this test fail with a `BundleUriError::Packchain` error.
        let store = FailListStore;
        let remote = parse(
            "s3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain&bundle_uri=1",
        )
        .unwrap();
        let mut buf: Vec<u8> = Vec::new();
        handle_bundle_uri(&store, &remote, BundleUriOpts::default(), true, &mut buf)
            .await
            .expect("list failure must not surface as a hard error");
        assert_eq!(
            &buf, b"\n",
            "list failure must yield only the trailing terminator",
        );
    }
}

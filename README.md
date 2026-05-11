# git-remote-object-store

**Push, fetch, and clone Git repositories straight against AWS S3 or
Azure Blob Storage. No server. No SaaS. No managed runner. One
static binary, two clouds.**

```bash
git remote add origin 's3+https://my-bucket.s3.us-west-2.amazonaws.com/my-repo'
git push -u origin main
```

Or, with Azure:

```bash
git remote add origin 'az+https://myaccount.blob.core.windows.net/my-container/my-repo?credential=PROD'
git push -u origin main
```

That's it. Your bucket is your remote.

## Why?

You want a private Git remote that is:

- **Owned by you, not a vendor.** No SaaS subscription, no per-seat
  cost, no "the host got breached" risk for your private code. Just
  a bucket or container in an account you already control.
- **Backed by storage you already trust.** Encryption at rest,
  IAM/RBAC at the prefix or container, lifecycle policies, regional
  replication, audit logs — every control your cloud storage gives
  you, with no application server in between.
- **One small binary.** No Python runtime, no Docker image, no
  webhook endpoint to babysit.

Use cases that fit naturally:

- Private repos you do not want on GitHub or GitLab.
- Internal libraries hosted on your team's existing S3 / Azure tenant.
- Repos consumed by AWS CodePipeline (use `?zip=1` to mirror each push
  as `repo.zip` next to the bundle).
- Air-gapped or sovereign-cloud environments where SaaS Git hosts are
  not an option.

## What you get

- **Two backends behind one trait.** AWS S3 and Azure Blob Storage,
  plus any S3-compatible endpoint (MinIO, Cloudflare R2, Wasabi,
  Backblaze B2, RustFS, on-prem appliances).
- **RFC 3986 HTTPS-native URL grammar.** `s3+https://<host>/<bucket>/<prefix>`
  and `az+https://<account>.blob.<endpoint>/<container>/<prefix>`.
  Cleartext `*+http://` is loopback-only by default for MinIO /
  Azurite work.
- **Streaming uploads end-to-end.** No in-memory buffering of bundles,
  no 5 GiB single-PUT ceiling — multipart upload is wired into both
  backends.
- **Hand-rolled parallel ranged GETs**, `If-Match`-guarded against
  concurrent overwrites.
- **Per-ref push-batch error handling.** A single failed ref reports
  a reason without aborting the rest of the batch.
- **Up-front bucket-name validation.** AWS-reserved prefixes/suffixes,
  IPv4 dotted-quads, and the rest of the rule set are checked before
  the SDK can return a cryptic error.
- **Modern TLS stack.** `rustls 0.23`, with deliberate opt-out of the
  AWS SDK's legacy `rustls 0.21` chain.
- **Locking parity across backends.** `If-None-Match: *` on S3,
  mirrored on Azure; same TTL semantics; tested across both.

## Quick install

See [docs/getting-started.md](docs/getting-started.md) for the full
walkthrough — install, credentials for both clouds, your first push,
LFS, submodules, local development against MinIO and Azurite.

The short version:

```bash
cargo xtask install
```

That runs `cargo install --path cli` and creates the four `+`-form
helper symlinks (`git-remote-s3+https`, `git-remote-s3+http`,
`git-remote-az+https`, `git-remote-az+http`) alongside the cargo
binaries, which is what git looks up by URL scheme. Re-runs are
idempotent. Pass `--bin-dir <PATH>` to install into a custom
directory, `--no-install` to refresh the symlinks only, or
`--dry-run` to preview.

## Using as a library

`git-remote-object-store` is also a Rust library crate. Add it to your
`Cargo.toml` and use `Remote` as the entry point to read or write objects in
the on-bucket format:

```rust
use git_remote_object_store::Remote;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let remote = Remote::connect(
        "s3+https://my-bucket.s3.us-east-1.amazonaws.com/my-repo"
    ).await?;

    // Read HEAD
    let head = remote.get_head().await?;
    println!("HEAD: {}", String::from_utf8_lossy(&head));

    // List all bundles on a branch
    let objects = remote.list("refs/heads/main/").await?;
    for obj in objects {
        println!("{} ({} bytes)", obj.key, obj.size);
    }

    // Direct store access for any operation
    let store = remote.store();
    let data = store.get_bytes(&remote.key("LOCK#.lock")).await?;
    Ok(())
}
```

The `ObjectStore` trait and the S3 / Azure backends are also publicly
available for building custom storage integrations.

## Documentation

- [Getting started](docs/getting-started.md) — install, credentials,
  first push, LFS, submodules, local dev with MinIO / Azurite,
  troubleshooting.
- [Changelog](CHANGELOG.md).
- [Lessons learned](docs/development/lessons_learned.md).

## Testing

`make shellspec` runs the fast CLI unit suite. The end-to-end shellspec
suites drive `git push` / `git fetch` / `git clone` through the helper
binaries against real backend containers; they require Docker, the
matching cloud CLI on the host, and `git-lfs` for the LFS scenarios.

```bash
make shellspec-integration-s3       # requires docker + aws-cli + git-lfs
make shellspec-integration-azure    # requires docker + azure-cli + git-lfs
make shellspec-integration          # both
```

## Status

`0.1.0`. The shipping surface includes the URL parser; the
`ObjectStore` trait with S3 and Azure backends; the helper-protocol
REPL; parallel `fetch`; locked `push`; the management CLI (`doctor` /
`delete-branch` / `protect` / `unprotect`); the LFS custom-transfer
agent; and the release pipeline.

Git operations are gitoxide-backed where `gix` has the surface
we need — rev-parse, is-ancestor, ref-name validation, remote-URL
inspection, archive / last-commit-message, ref discovery, object
resolution. Bundle `create` and `unbundle` still shell out to the
user's `git` binary through a single `run_git` helper because `gix`
does not yet expose a public bundle API; the spike notes at
[`docs/development/spike-gix-bundle-parity.md`](docs/development/spike-gix-bundle-parity.md)
record what the gap is and what would need to change upstream to
close it. The fallback is contained: `run_git` is the only place in
the crate that spawns a subprocess, and it enforces the
helper-protocol stdout discipline (stdin closed, stdout/stderr
captured, never inherited).

## Known limitations

A push of a multi-GB monorepo will work today on either backend —
multipart upload is wired into both — but a few sharp edges are worth
knowing about before you start:

- **No resume after a failed upload.** If the helper process dies
  mid-push (network blip, signal, reboot), the next `git push`
  re-uploads the bundle from the beginning. S3 cleans up abandoned
  multipart sessions per the bucket's lifecycle policy; Azure
  uncommitted blocks expire after seven days. Neither backend
  surfaces a "resume from byte N" handle today.
- **Object-size ceilings are the cloud's, not ours.** S3 caps a
  single object at 5 TiB and a multipart upload at 10 000 parts; the
  single-`PutObject` ceiling is still 5 GiB but the helper auto-
  promotes large bodies to multipart well below that. Azure caps a
  block blob at 50 000 committed blocks (~4.75 TiB at the SDK's
  default block size). Repositories whose individual bundles
  approach those limits are outside what either backend can store.

## Verifying releases

Every `v*` tag publishes signed, attested artefacts to
[GitHub Releases](https://github.com/dekobon/git-remote-object-store/releases).

```bash
gh release download vX.Y.Z -p '*x86_64-unknown-linux-musl.tar.gz' \
                          -p SHA256SUMS -p SHA256SUMS.minisig
minisign -Vm SHA256SUMS -p minisign.pub
grep musl SHA256SUMS | sha256sum -c
gh attestation verify git-remote-object-store-X.Y.Z-x86_64-unknown-linux-musl.tar.gz \
                     -R dekobon/git-remote-object-store
```

`SHA256SUMS` is signed with [minisign](https://jedisct1.github.io/minisign/)
against the committed [`minisign.pub`](minisign.pub); each archive
also carries a [SLSA build provenance](https://slsa.dev/) attestation
signed by the runner's GitHub OIDC identity. CycloneDX SBOMs
(`*.cdx.json`) ship in every release for both the library and the
CLI. See [`docs/development/cutting-a-release.md`](docs/development/cutting-a-release.md)
for the full release pipeline and [`SECURITY.md`](SECURITY.md) for
the vulnerability-reporting flow.

## License

Apache-2.0. See [LICENSE](LICENSE).

## Credits

Inspired by [`awslabs/git-remote-s3`](https://github.com/awslabs/git-remote-s3),
which itself draws on
[`bgahagan/git-remote-s3`](https://github.com/bgahagan/git-remote-s3)
and the LFS work in
[`nicolas-graves/lfs-s3`](https://github.com/nicolas-graves/lfs-s3).

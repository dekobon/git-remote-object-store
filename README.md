# git-remote-object-store

[![CI](https://github.com/dekobon/git-remote-object-store/actions/workflows/ci.yml/badge.svg?branch=main&event=push)](https://github.com/dekobon/git-remote-object-store/actions/workflows/ci.yml?query=branch%3Amain+event%3Apush)
[![CodeQL](https://github.com/dekobon/git-remote-object-store/actions/workflows/codeql.yml/badge.svg?branch=main)](https://github.com/dekobon/git-remote-object-store/actions/workflows/codeql.yml?query=branch%3Amain)
[![crates.io](https://img.shields.io/crates/v/git-remote-object-store.svg)](https://crates.io/crates/git-remote-object-store)

**Push, fetch, and clone Git repositories straight against a S3 compatible store or
Azure Blob Storage. No intermediary servers. One static binary, one object store.**

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
- **Two storage engines.** A `bundle` engine (one git bundle per
  push, simple and self-contained) and a `packchain` engine (newest-
  first pack manifest with GC and compaction, smaller fetches on
  active repos). Pick per-remote with `?engine=`; default is
  `bundle`. See [docs/storage-engines.md](docs/storage-engines.md)
  for the comparison and when to choose which.
- **Streaming uploads end-to-end.** No in-memory buffering of bundles,
  no 5 GiB single-PUT ceiling — multipart upload is wired into both
  backends.
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

`git-remote-object-store` is also a Rust library crate. `Remote` is
the entry point for reading and writing objects in the on-bucket
format; the `ObjectStore` trait and the S3 / Azure backends are also
publicly exported for building custom storage integrations. See
[docs/library-usage.md](docs/library-usage.md) for a worked example
and [docs.rs](https://docs.rs/git-remote-object-store) for the full
API.

## Documentation

- [Getting started](docs/getting-started.md) — install, credentials,
  first push, LFS, submodules, local dev with MinIO / Azurite,
  troubleshooting.
- [Storage engines](docs/storage-engines.md) — `bundle` vs
  `packchain`, trade-offs, on-bucket layout, when to choose which.
- [Environment variables](docs/environment-variables.md) — every
  variable the helpers, CLI, and test suites read.
- [Library usage](docs/library-usage.md) — using the crate as a Rust
  library.
- [Verifying releases](docs/verifying-releases.md) — signature,
  attestation, and SBOM verification.
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

`0.1.0`. The shipping surface covers both storage engines
(`bundle` and `packchain`), both backends (S3 and Azure Blob), the
helper-protocol REPL, parallel `fetch`, locked `push`, the
management CLI (`doctor`, `delete-branch`, `protect`, `unprotect`,
`gc`, `compact`), the LFS custom-transfer agent, and the signed
release pipeline.

Git operations are gitoxide-backed end to end — bundle read/write is
native via `gix-pack`, and the crate spawns no `git` subprocess in
production code. The `gix` surface in use covers rev-parse,
is-ancestor, ref-name validation, remote-URL inspection, archive,
last-commit-message, ref discovery, and object resolution.

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

Every `v*` tag publishes signed, attested artefacts (minisign over
`SHA256SUMS`, SLSA build provenance, CycloneDX SBOMs) to
[GitHub Releases](https://github.com/dekobon/git-remote-object-store/releases).
See [docs/verifying-releases.md](docs/verifying-releases.md) for the
verification recipe and [`SECURITY.md`](SECURITY.md) for vulnerability
reporting.

## License

Apache-2.0. See [LICENSE](LICENSE).

## Credits

Inspired by [`awslabs/git-remote-s3`](https://github.com/awslabs/git-remote-s3),
which itself draws on
[`bgahagan/git-remote-s3`](https://github.com/bgahagan/git-remote-s3)
and the LFS work in
[`nicolas-graves/lfs-s3`](https://github.com/nicolas-graves/lfs-s3).

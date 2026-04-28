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

## How does it compare to `awslabs/git-remote-s3`?

This is a Rust rewrite of the upstream Python tool
[`awslabs/git-remote-s3`](https://github.com/awslabs/git-remote-s3).
The on-bucket object layout is preserved byte-for-byte (existing
buckets remain readable in either direction), but the URL grammar,
distribution model, and a number of correctness and performance
details are intentional improvements.

|                                | `awslabs/git-remote-s3`           | `git-remote-object-store`                                                            |
| ------------------------------ | --------------------------------- | ------------------------------------------------------------------------------------ |
| **Backends**                   | S3 only                           | **S3 + Azure Blob Storage**, behind one shared `ObjectStore` trait                   |
| **URL grammar**                | `s3://profile@bucket/key`         | RFC 3986 HTTPS-native: `s3+https://<host>/<bucket>/<prefix>` — works with any host   |
| **S3-compatible endpoints**    | Untested, address-style guessing  | First-class: MinIO, Cloudflare R2, Wasabi, Backblaze B2, RustFS, on-prem appliances  |
| **Distribution**               | `pip install`, Python ≥ 3.9       | One static binary per platform, no runtime                                           |
| **Bundle uploads**             | Buffered in process memory        | **Streamed** end-to-end — no OOM on large repos, no 5 GiB single-PUT ceiling         |
| **Multipart download**         | boto3 `TransferConfig` defaults   | Hand-rolled parallel ranged GETs, **`If-Match`-guarded** against concurrent overwrites |
| **Push-batch error handling**  | First failure aborts the batch    | Per-ref errors continue the batch; failed refs report a reason without losing peers  |
| **Bucket-name validation**     | Surfaces as cryptic SDK errors    | Validates AWS-reserved prefixes/suffixes, IPv4 dotted-quads, etc. up front           |
| **TLS stack**                  | Whatever Python's `ssl` resolves  | Modern `rustls 0.23`; deliberately opts out of AWS SDK's legacy `rustls 0.21` chain  |
| **Locking**                    | `If-None-Match: *` (S3)           | Same on S3, mirrored on Azure; same TTL semantics; tested across both backends       |
| **Cleartext HTTP**             | n/a                               | Loopback-only by default; `s3+http`/`az+http` exist for MinIO and Azurite            |

What we deliberately did **not** carry over from upstream:

- The `s3+zip://` scheme. Use `?zip=1` on the URL — same artefact,
  cleaner grammar.
- The `s3://profile@bucket/path` userinfo form. Use `?profile=NAME`.
- Backwards-compatibility shims for the old URL forms. This is a
  greenfield 0.1.0 — pay the one-time cost of `git remote set-url`.

## Quick install

See [docs/getting-started.md](docs/getting-started.md) for the full
walkthrough — install, credentials for both clouds, your first push,
LFS, submodules, local development against MinIO and Azurite.

The short version:

```bash
cargo install --path .

# Bridge cargo's hyphenated names to the `+`-form git looks up.
mkdir -p ~/.local/bin
for s in s3+https s3+http az+https az+http; do
    ln -sf "$HOME/.cargo/bin/git-remote-${s/+/-}" \
           "$HOME/.local/bin/git-remote-$s"
done
```

## Documentation

- [Getting started](docs/getting-started.md) — install, credentials,
  first push, LFS, submodules, local dev with MinIO / Azurite,
  troubleshooting.
- [Execution plan](execution-plan.md) — design rationale, URL grammar
  spec, on-bucket layout, phase-by-phase implementation notes.
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

`0.1.0`. Phases 1–14 of the [execution plan](execution-plan.md) are
shipped: URL parser; the `ObjectStore` trait with S3 and Azure
backends; the helper-protocol REPL; parallel `fetch`; locked `push`;
the management CLI (`doctor` / `delete-branch` / `protect` /
`unprotect`); the LFS custom-transfer agent; and the release
pipeline.

Git operations are gitoxide-backed where `gix` 0.82 has the surface
we need — rev-parse, is-ancestor, ref-name validation, remote-URL
inspection, archive / last-commit-message, ref discovery, object
resolution. Bundle `create` and `unbundle` still shell out to the
user's `git` binary through a single `run_git` helper because `gix`
does not yet expose a public bundle API; this is documented in
[execution-plan.md §6](execution-plan.md#6-resolved-decisions-and-remaining-open-questions)
and the spike notes at
[`docs/development/spike-gix-bundle-parity.md`](docs/development/spike-gix-bundle-parity.md).
The fallback is contained: `run_git` is the only place in the crate
that spawns a subprocess, and it enforces the helper-protocol stdout
discipline (stdin closed, stdout/stderr captured, never inherited).

## License

Apache-2.0. See [LICENSE](LICENSE).

## Credits

Inspired by [`awslabs/git-remote-s3`](https://github.com/awslabs/git-remote-s3),
which itself draws on
[`bgahagan/git-remote-s3`](https://github.com/bgahagan/git-remote-s3)
and the LFS work in
[`nicolas-graves/lfs-s3`](https://github.com/nicolas-graves/lfs-s3).

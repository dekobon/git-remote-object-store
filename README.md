# git-remote-object-store

A git remote helper backed by cloud object stores. Push, fetch, and
clone against AWS S3 (and S3-compatible) buckets or Azure Blob Storage
containers, using the same on-bucket layout as the upstream
[`awslabs/git-remote-s3`](https://github.com/awslabs/git-remote-s3) so
existing buckets remain readable.

This is a Rust rewrite that adds Azure Blob Storage as a first-class
backend alongside S3. The URL grammar is intentionally different — see
[`execution-plan.md`](execution-plan.md) §3 for the rationale.

## Status

Pre-release. Phases 1–12 of the execution plan have shipped: URL
parser, gitoxide-backed git operations, the `ObjectStore` trait with
S3 and Azure backends, the helper protocol REPL, parallel `fetch`,
locked `push`, the management CLI (`doctor` / `delete-branch` /
`protect` / `unprotect`), the LFS custom-transfer agent, and the
helper-binary shims for both schemes.

Parity QA against the Python upstream (Phase 13) and packaging
(Phase 14) are tracked as separate issues.

## Installing

Cargo rejects `+` in `[[bin]] name`, so the package ships hyphenated
binary names. Git looks helpers up by their `+`-form scheme name, so
each remote-helper binary needs a `+`-named symlink alongside the
cargo-installed file. (A future `xtask` packaging step will automate
this — tracked for Phase 14, see
[`execution-plan.md`](execution-plan.md) §5.6 / §6.)

```bash
cargo install --path .

# Bridge cargo's hyphen names to the `+` form git invokes. Pick any
# directory on PATH for the symlinks; ~/.local/bin is shown here.
mkdir -p ~/.local/bin
for s in s3+https s3+http az+https az+http; do
    ln -sf "$HOME/.cargo/bin/git-remote-${s/+/-}" \
           "$HOME/.local/bin/git-remote-$s"
done
```

The non-helper binaries (`git-remote-object-store` and
`git-lfs-object-store`) are looked up under their literal cargo names
and need no rename.

The full set of installed binaries:

- `git-remote-s3+https` / `git-remote-s3+http` — S3 helpers
  (the `+http` variant is loopback-only by design)
- `git-remote-az+https` / `git-remote-az+http` — Azure Blob helpers
- `git-remote-object-store` — management CLI
- `git-lfs-object-store` — LFS custom-transfer agent

## URL grammar

```text
s3+https://<host>[:port]/<bucket>/<prefix>[?flags]
s3+http://<host>[:port]/<bucket>/<prefix>[?flags]      # local dev only
az+https://<account>.blob.<endpoint-suffix>/<container>/<prefix>[?flags]
az+http://<host>[:port]/<account>/<container>/<prefix>[?flags]   # Azurite
```

Concrete examples and the full validation rules live in
[`execution-plan.md`](execution-plan.md) §3.

## Submodule allowance

Git refuses unknown URL schemes inside submodule URLs by default.
Allow the helper schemes globally so submodule clones do not fail:

```bash
git config --global protocol.s3+https.allow always
git config --global protocol.az+https.allow always
```

The `s3+http` / `az+http` variants are restricted to loopback hosts
(`localhost`, `127.0.0.1`, `::1`) by the helper itself and should not
be needed for submodules. Set
`GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP=1` to lift the loopback gate for
local development against MinIO / Azurite.

## Authentication

### AWS S3

Standard AWS credential resolution applies — the SDK consults
environment variables, the shared credentials file, IMDS, ECS task
metadata, and so on. To pin a named profile per remote, set
`?profile=<NAME>` on the URL:

```bash
git remote add origin \
    's3+https://my-bucket.s3.us-west-2.amazonaws.com/my-repo?profile=prod'
```

### Azure Blob Storage

Three credential shapes are supported, in priority order when
`?credential=<NAME>` is set on the URL:

1. `AZSTORE_<NAME>_KEY` — shared-key (storage account key). The
   helper signs each request with Azure Storage shared-key v2.
2. `AZSTORE_<NAME>_CONNECTION_STRING` — full
   `DefaultEndpointsProtocol=…;AccountName=…;AccountKey=…` connection
   string.
3. `AZSTORE_<NAME>_SAS` — shared-access signature, appended to each
   outgoing URL.

If `?credential=` is not set, the helper falls back to the Azure SDK's
`DeveloperToolsCredential` (Entra ID), which itself walks env vars,
workload identity, managed identity, the Azure CLI, and so on.

```bash
export AZSTORE_PROD_KEY='<base64 storage-account key>'
git remote add origin \
    'az+https://myaccount.blob.core.windows.net/my-container/my-repo?credential=PROD'
```

For Azurite (the local emulator), use the well-known account key:

```bash
export AZSTORE_AZURITE_KEY='Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=='
export GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP=1   # loopback gate; redundant for 127.0.0.1
git remote add origin \
    'az+http://127.0.0.1:10000/devstoreaccount1/my-container/my-repo?addressing=path&credential=AZURITE'
```

## LFS

`git-lfs-object-store` is a custom-transfer agent that uploads and
downloads LFS objects through the same backend as the parent remote.
Register it in a repository with:

```bash
git-lfs-object-store install
```

This sets `lfs.customtransfer.git-lfs-object-store.path` and
`lfs.standalonetransferagent`. Subsequent `git push` / `git lfs pull`
calls route LFS objects to `<prefix>/lfs/<oid>` in the bucket.

For verbose tracing during development:

```bash
git-lfs-object-store enable-debug   # writes to <git-dir>/lfs/tmp/git-lfs-object-store.log
git-lfs-object-store disable-debug
```

## Management

`git-remote-object-store` accepts either a remote URL or the name of a
git remote configured in the current repository. Subcommands:

- `doctor` — analyze the bucket, offer to keep or quarantine
  duplicate bundles per ref, prompt for a replacement when `HEAD` is
  invalid, and clear stale `*.lock` files past the TTL.
- `delete-branch <branch>` — delete every object under
  `refs/heads/<branch>/` after a confirmation.
- `protect <branch>` / `unprotect <branch>` — toggle the
  `PROTECTED#` sentinel that blocks force-pushes.

## License

Apache-2.0. See [`LICENSE`](LICENSE).

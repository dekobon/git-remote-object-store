# Getting started

This walks you from a clean machine to your first push against either
AWS S3 or Azure Blob Storage. Pick the backend section that matches
your cloud — the rest of the workflow is identical.

If you just want to play locally, jump to
[Local development](#4-local-development) for MinIO / Azurite recipes
that skip cloud accounts entirely.

- [1. Install](#1-install)
- [2. AWS S3](#2-aws-s3)
- [3. Azure Blob Storage](#3-azure-blob-storage)
- [4. Local development](#4-local-development)
- [5. URL grammar reference](#5-url-grammar-reference)
- [6. Submodules](#6-submodules)
- [7. Git LFS](#7-git-lfs)
- [8. Management CLI](#8-management-cli)
- [9. Troubleshooting](#9-troubleshooting)

## 1. Install

### Prerequisites

- `git` (any reasonably recent version)
- A Rust toolchain (`rustup` / `cargo`) if you are building from
  source. Stable Rust ≥ 1.94.

### Build and install

```bash
git clone https://github.com/dekobon/git-remote-object-store
cd git-remote-object-store
cargo install --path cli
```

This installs six binaries into `$HOME/.cargo/bin`:

| Binary                       | Purpose                                                      |
| ---------------------------- | ------------------------------------------------------------ |
| `git-remote-s3-https`        | S3 helper (HTTPS)                                            |
| `git-remote-s3-http`         | S3 helper (loopback HTTP only — MinIO and friends)           |
| `git-remote-az-https`        | Azure Blob helper (HTTPS)                                    |
| `git-remote-az-http`         | Azure Blob helper (loopback HTTP only — Azurite)             |
| `git-remote-object-store`    | Management CLI (`doctor`, `delete-branch`, `protect`, …)     |
| `git-lfs-object-store`       | LFS custom-transfer agent                                    |

### Symlink the `+`-form helpers

Cargo does not allow `+` in `[[bin]] name`, so the four helper
binaries above ship hyphenated. Git invokes helpers by scheme name —
i.e. `git-remote-s3+https` for an `s3+https://...` URL — so each
hyphenated binary needs a `+`-named symlink alongside it. Pick any
directory on `PATH`; `~/.local/bin` is shown here:

```bash
mkdir -p ~/.local/bin
for s in s3+https s3+http az+https az+http; do
    ln -sf "$HOME/.cargo/bin/git-remote-${s/+/-}" \
           "$HOME/.local/bin/git-remote-$s"
done
```

`git-remote-object-store` and `git-lfs-object-store` are looked up by
their literal cargo names and need no rename.

### Verify

```bash
git-remote-object-store --help
```

## 2. AWS S3

### Create the bucket and IAM policy

Create a bucket (or reuse one). Attach a policy to your IAM user or
role granting at least:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "ObjectAccess",
      "Effect": "Allow",
      "Action": ["s3:PutObject", "s3:GetObject", "s3:DeleteObject"],
      "Resource": ["arn:aws:s3:::MY-BUCKET/*"]
    },
    {
      "Sid": "ListBucket",
      "Effect": "Allow",
      "Action": ["s3:ListBucket"],
      "Resource": ["arn:aws:s3:::MY-BUCKET"]
    }
  ]
}
```

If the bucket uses SSE-KMS, also grant `kms:Decrypt` and
`kms:GenerateDataKey` on the key.

To host multiple repositories in one bucket and segregate access per
repo, scope `Resource` to `arn:aws:s3:::MY-BUCKET/MY-REPO/*` and add a
`s3:prefix` condition on `s3:ListBucket`.

### Configure credentials

The helper uses the standard AWS credential chain — environment
variables, `~/.aws/credentials`, IMDS, ECS task metadata, SSO, and so
on. The simplest path is the AWS CLI:

```bash
aws configure --profile prod
```

To pin a profile to a single remote, append `?profile=prod` to the
URL. To override the SigV4 region (the helper otherwise infers it
from `*.s3.<region>.amazonaws.com` hostnames and falls back to
`us-east-1` for non-AWS endpoints), append `&region=us-west-2`.

### Push your first repo

```bash
mkdir my-repo && cd my-repo
git init
echo "Hello" > hello.txt
git add -A && git commit -m "first"
git remote add origin \
    's3+https://my-bucket.s3.us-west-2.amazonaws.com/my-repo?profile=prod'
git push -u origin main
```

The remote `HEAD` is set to the first branch you push.

### Clone

```bash
git clone \
    's3+https://my-bucket.s3.us-west-2.amazonaws.com/my-repo?profile=prod' \
    my-repo-clone
```

### S3-compatible endpoints

The same scheme works against any S3-compatible service — MinIO,
Cloudflare R2, Wasabi, Backblaze B2, RustFS, on-prem appliances. Just
point at the right host. R2 example:

```bash
git remote add origin \
    's3+https://<accountid>.r2.cloudflarestorage.com/my-bucket/my-repo?addressing=path&region=auto'
```

If the endpoint does not accept virtual-hosted bucket addressing
(`<bucket>.<host>/...`), pass `addressing=path` to force path-style
(`<host>/<bucket>/...`).

## 3. Azure Blob Storage

### Create the container

Reuse an existing storage account or create one. Then create a
container inside it:

```bash
az storage container create --account-name myaccount --name my-container
```

### Configure credentials

The helper supports three credential shapes, picked in priority order
when `?credential=<NAME>` is set on the URL:

1. **`AZSTORE_<NAME>_KEY`** — base64 storage account key. Signed via
   Azure Storage shared-key v2.
2. **`AZSTORE_<NAME>_CONNECTION_STRING`** — full
   `DefaultEndpointsProtocol=…;AccountName=…;AccountKey=…` form.
3. **`AZSTORE_<NAME>_SAS`** — shared-access signature, appended to
   each outgoing URL.

If `?credential=` is not set, the helper falls back to the Azure SDK's
`DeveloperToolsCredential` (Entra ID), which walks env vars, workload
identity, managed identity, the Azure CLI, and so on.

```bash
export AZSTORE_PROD_KEY='<base64 storage-account key>'
```

### Push your first repo

```bash
mkdir my-repo && cd my-repo
git init
echo "Hello" > hello.txt
git add -A && git commit -m "first"
git remote add origin \
    'az+https://myaccount.blob.core.windows.net/my-container/my-repo?credential=PROD'
git push -u origin main
```

### Clone

```bash
git clone \
    'az+https://myaccount.blob.core.windows.net/my-container/my-repo?credential=PROD' \
    my-repo-clone
```

## 4. Local development

For experimenting without a cloud account.

### MinIO (S3-compatible)

```bash
docker run -d --name minio -p 9000:9000 -p 9001:9001 \
    -e MINIO_ROOT_USER=minioadmin \
    -e MINIO_ROOT_PASSWORD=minioadmin \
    minio/minio server /data --console-address ":9001"

aws --endpoint-url http://127.0.0.1:9000 \
    --region us-east-1 \
    s3 mb s3://my-bucket

export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
export GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP=1   # only needed for non-loopback HTTP

mkdir my-repo && cd my-repo
git init && echo hi > hi.txt && git add -A && git commit -m "first"
git remote add origin \
    's3+http://127.0.0.1:9000/my-bucket/my-repo?addressing=path&region=us-east-1'
git push -u origin main
```

### Azurite (Azure emulator)

```bash
docker run -d --name azurite -p 10000:10000 \
    mcr.microsoft.com/azure-storage/azurite \
    azurite-blob --blobHost 0.0.0.0

# Well-known Azurite account key:
export AZSTORE_AZURITE_KEY='Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=='

# One-time: create the container against Azurite. Any tool that signs
# with the Azurite key works; the Azure CLI is convenient:
az storage container create \
    --name my-container \
    --connection-string "DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;AccountKey=$AZSTORE_AZURITE_KEY;BlobEndpoint=http://127.0.0.1:10000/devstoreaccount1;"

mkdir my-repo && cd my-repo
git init && echo hi > hi.txt && git add -A && git commit -m "first"
git remote add origin \
    'az+http://127.0.0.1:10000/devstoreaccount1/my-container/my-repo?addressing=path&credential=AZURITE'
git push -u origin main
```

The `s3+http` and `az+http` schemes only accept loopback hosts
(`localhost`, `127.0.0.1`, `::1`) by default. To allow plain HTTP
against a non-loopback dev endpoint, set
`GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP=1`. This gate is intentional;
plaintext-on-the-network is not an ergonomic default.

## 5. URL grammar reference

```text
s3+https://<host>[:port]/<bucket>/<prefix>[?flags]
s3+http://<host>[:port]/<bucket>/<prefix>[?flags]                  # loopback only
az+https://<account>.blob.<endpoint-suffix>/<container>/<prefix>[?flags]
az+http://<host>[:port]/<account>/<container>/<prefix>[?flags]     # Azurite
```

Query-string flags:

| Flag                       | Backends | Meaning                                                 |
| -------------------------- | -------- | ------------------------------------------------------- |
| `profile=<NAME>`           | S3       | Pin AWS named profile                                   |
| `credential=<NAME>`        | Azure    | Pick the `AZSTORE_<NAME>_*` env-var bundle              |
| `region=<REGION>`          | S3       | Override SigV4 region                                   |
| `addressing=path\|virtual` | Both     | Force the addressing style (auto-detected by default)   |
| `zip=1`                    | Both     | Mirror each push as `repo.zip` (AWS CodePipeline input) |

The complete grammar lives in the URL parser (`src/url.rs`); the
table above and the scheme outline earlier in this section cover
everything an end-user typically needs.

## 6. Submodules

Git refuses unknown URL schemes inside submodule URLs by default.
Allow the helper schemes globally so submodule clones do not fail:

```bash
git config --global protocol.s3+https.allow always
git config --global protocol.az+https.allow always
```

The `s3+http` / `az+http` variants are restricted to loopback hosts
inside the helper itself and should not be needed for submodules.

## 7. Git LFS

Install Git LFS first (one-time per system) — see
<https://git-lfs.com/> for platform packages.

Then in each repo:

```bash
git lfs install
git-lfs-object-store install     # registers the custom-transfer agent
git lfs track "*.tiff"
git add .gitattributes
git add big.tiff
git commit -m "add binary"
git remote add origin '<your s3+https or az+https URL>'
git push -u origin main
```

`git-lfs-object-store install` writes two keys into the local
`git config`:

```
lfs.customtransfer.git-lfs-object-store.path = git-lfs-object-store
lfs.standalonetransferagent = git-lfs-object-store
```

LFS objects are stored under `<prefix>/lfs/<oid>` in the same bucket
or container as the repo bundles.

### Cloning an LFS repo for the first time

LFS does not yet know about the custom-transfer agent in a fresh
clone, so the smudge filter fails on the first checkout. Re-run the
install and reset:

```bash
git clone '<url>' repo-clone
cd repo-clone
git-lfs-object-store install
git reset --hard
```

### Verbose LFS tracing

```bash
git-lfs-object-store enable-debug    # logs to <git-dir>/lfs/tmp/git-lfs-object-store.log
git-lfs-object-store disable-debug
```

Logs always go to the file or to stderr — never to stdout, which is
reserved for the LFS protocol.

## 8. Management CLI

`git-remote-object-store` accepts either a remote URL or the name of
a configured git remote in the current repo (resolved via
`git remote get-url`). All subcommands take the remote first:

```bash
# Inspect / repair: scans for duplicate bundles, an invalid HEAD, and
# stale locks. Interactive prompts choose what to keep / quarantine.
git-remote-object-store doctor origin

# Drop every object under refs/heads/<branch>/.
git-remote-object-store delete-branch origin feature-branch

# Force-push protection (writes / removes the PROTECTED# sentinel).
git-remote-object-store protect origin main
git-remote-object-store unprotect origin main
```

`doctor` flags worth knowing:

- `--lock-ttl <SECS>` — seconds after which a `*.lock` file is
  considered stale (default `60`). Also configurable via the
  upstream-compat env var `GIT_REMOTE_S3_LOCK_TTL_SECONDS`.
- `--delete-stale-locks` — actually remove stale locks (otherwise
  doctor only reports them).
- `--delete-bundle` — delete losing bundles outright instead of
  moving them to `<ref>_<uuid8>` quarantine refs (the default, which
  is non-destructive — you can `git checkout` the quarantine ref and
  decide what to do).

## 9. Troubleshooting

### Verbose helper output

```bash
GIT_REMOTE_OBJECT_STORE_VERBOSE=2 git push origin main
# upstream-compat alias also works:
GIT_REMOTE_S3_VERBOSE=2 git push origin main
```

Git's own verbosity knob also reaches the helper at runtime:

```bash
git -c transfer.verbosity=2 push origin main
```

All log output goes to stderr — stdout is reserved for the
remote-helper protocol bytes that git is parsing.

### "lock held" on push

Another client is currently pushing to the same ref, or a previous
push aborted without releasing the lock. Wait the TTL (60s default)
and retry — the helper auto-clears stale locks on contention. To
inspect manually:

```bash
git-remote-object-store doctor origin --lock-ttl 60 --delete-stale-locks
```

### "matches more than one" on push

Two bundles exist for the same ref because two pushes raced. Run
`doctor` — by default it offers to keep one and quarantine the other
under `<ref>_<uuid8>`. Pass `--delete-bundle` to drop the loser.

### Cleartext HTTP rejected

`s3+http://` and `az+http://` only accept loopback hosts
(`localhost`, `127.0.0.1`, `::1`) by default. For non-loopback HTTP
(lab MinIO, on-prem object stores), set:

```bash
export GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP=1
```

This is intentional — we don't want to make plaintext-over-the-network
the default ergonomics. Use HTTPS in production.

### Azure: container not found

The helper does not auto-create containers. Create the container
once with the Azure CLI or portal before the first push.

### S3: cryptic SDK error on a fresh bucket

If `git push` returns `AccessDenied` or `NoSuchBucket`, double-check:

- The IAM principal really resolves at runtime
  (`aws sts get-caller-identity` with the same profile).
- The IAM policy includes `s3:ListBucket` on the bucket itself, not
  only `s3:GetObject` / `s3:PutObject` on the objects.
- The bucket is in the region you configured (or is reachable via the
  endpoint you supplied for non-AWS S3-compatible services).

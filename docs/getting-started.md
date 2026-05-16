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
- [9. Maintenance: `gc` and `compact`](#9-maintenance-gc-and-compact)
- [10. Bundle URI — faster `git clone` for large repos](#10-bundle-uri--faster-git-clone-for-large-repos)
- [11. Troubleshooting](#11-troubleshooting)
- See also: [environment-variables.md](environment-variables.md) —
  every env var the helper binaries, CLI, and test suites read.

## 1. Install

### Prerequisites

- `git` (any reasonably recent version)
- A Rust toolchain (`rustup` / `cargo`) if you are building from
  source. Stable Rust ≥ 1.94.

### Build and install

```bash
git clone https://github.com/dekobon/git-remote-object-store
cd git-remote-object-store
cargo xtask install
```

`cargo xtask install` runs `cargo install --path cli` and then creates
the four `+`-form helper symlinks git invokes by URL scheme. Six
binaries land in `$HOME/.cargo/bin`:

| Binary                       | Purpose                                                      |
| ---------------------------- | ------------------------------------------------------------ |
| `git-remote-s3-https`        | S3 helper (HTTPS)                                            |
| `git-remote-s3-http`         | S3 helper (loopback HTTP only — MinIO and friends)           |
| `git-remote-az-https`        | Azure Blob helper (HTTPS)                                    |
| `git-remote-az-http`         | Azure Blob helper (loopback HTTP only — Azurite)             |
| `git-remote-object-store`    | Management CLI (`doctor`, `delete-branch`, `protect`, …)     |
| `git-lfs-object-store`       | LFS custom-transfer agent                                    |

alongside four `+`-form symlinks
(`git-remote-s3+https`, `git-remote-s3+http`, `git-remote-az+https`,
`git-remote-az+http`) that point at the matching hyphenated binary
in the same directory. Re-runs are idempotent.

### Why the symlinks?

Cargo does not allow `+` in `[[bin]] name`, so the four helper
binaries ship hyphenated. Git looks helpers up by URL scheme — i.e.
`git-remote-s3+https` for an `s3+https://...` URL — so each
hyphenated binary needs a `+`-named symlink alongside it.
`cargo xtask install` automates this; the manual equivalent is:

```bash
cargo install --path cli
for s in s3+https s3+http az+https az+http; do
    ln -sf "$HOME/.cargo/bin/git-remote-${s/+/-}" \
           "$HOME/.cargo/bin/git-remote-$s"
done
```

`git-remote-object-store` and `git-lfs-object-store` are looked up by
their literal cargo names and need no rename.

### xtask options

```bash
cargo xtask install --bin-dir ~/.local/bin   # install into a custom dir
cargo xtask install --no-install             # refresh symlinks only
cargo xtask install --dry-run                # preview without writing
```

`--bin-dir` overrides the auto-detected directory (which is
`$CARGO_INSTALL_ROOT/bin`, then `$CARGO_HOME/bin`, then
`$HOME/.cargo/bin`). The xtask refuses to clobber any existing
regular file or directory at a `+`-form path — only its own symlinks
are refreshed.

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
| `engine=bundle\|packchain` | Both     | Storage engine on first push (defaults to `bundle`); see [storage-engines.md](storage-engines.md) |
| `profile=<NAME>`           | S3       | Pin AWS named profile                                   |
| `credential=<NAME>`        | Azure    | Pick the `AZSTORE_<NAME>_*` env-var bundle              |
| `region=<REGION>`          | S3       | Override SigV4 region                                   |
| `addressing=path\|virtual` | Both     | Force the addressing style (auto-detected by default)   |
| `zip=1`                    | Both     | Mirror each push as `repo.zip` (AWS CodePipeline input) |
| `bundle_uri=1`             | Both     | Tell `git clone` to download the baseline pack directly from the bucket/CDN in parallel with the helper, skipping the chain walk (packchain only — see §10) |
| `bundle_uri_presign_ttl=<SECONDS>` | Both | Needed for `bundle_uri=1` to actually work on private buckets: TTL of the presigned per-ref URL the helper emits (see §10) |

The complete grammar lives in the URL parser (`src/url.rs`); the
table above and the scheme outline earlier in this section cover
everything an end-user typically needs.

### Case-sensitivity policy

The case rules below are intentional, not historical accidents.

| Flag class                          | Case                 | Example                                                                                                                                                                                                  |
| ----------------------------------- | -------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Boolean flags (`zip`, `bundle_uri`) | Case-**in**sensitive | `?zip=true`, `?zip=TRUE`, `?zip=Yes`, `?zip=on` all enable the flag; `0`, `false`, `no`, `off` (any casing) disable it.                                                                                  |
| `engine=<name>`                     | Case-**sensitive**   | `?engine=bundle` and `?engine=packchain` are the only accepted spellings. `?engine=Bundle` is rejected.                                                                                                  |
| `addressing=<style>`                | Case-**sensitive**   | `?addressing=path` and `?addressing=virtual` only — not `Path` or `VIRTUAL`.                                                                                                                             |
| `credential=<NAME>`                 | Normalised           | The value is preserved at the URL surface but normalised to ASCII upper case when used to build the Azure credential env-var name (`AZSTORE_<NAME>_KEY`, …). `?credential=prod` and `?credential=PROD` both resolve to `AZSTORE_PROD_KEY`. |
| `profile=<NAME>`, `region=<REGION>` | Verbatim             | Forwarded as-is to the AWS SDK; the SDK's own casing rules apply (profile names are case-sensitive; region names are conventionally lower case).                                                         |

Boolean values share their vocabulary with the
`GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP` env-var gate
([environment-variables.md](environment-variables.md)) — anything the
URL flag accepts, the env var accepts, and vice versa. Engine and
addressing values are deliberately case-sensitive: their accepted set
is small and stable, and accepting variant spellings would just create
ambiguity for anyone reading a URL out of a config file or CI log.

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

The `gc` and `compact` subcommands target `packchain`-engine
bucket maintenance and are covered in §9 below.

`doctor` flags worth knowing:

- `--lock-ttl-seconds <SECS>` — seconds after which a `*.lock` file
  is considered stale. When unset, the default reads
  `GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS` (falling back to 60s) —
  matching `compact`, `delete-branch`, and the helper push path.
- `--delete-stale-locks` — actually remove stale locks (otherwise
  doctor only reports them).
- `--delete-bundle` — delete losing bundles outright instead of
  moving them to `<ref>_<uuid8>` quarantine refs (the default, which
  is non-destructive — you can `git checkout` the quarantine ref and
  decide what to do).

## 9. Maintenance: `gc` and `compact`

Both subcommands target **packchain** remotes only (see
[storage-engines.md](storage-engines.md) for the differences between
the two engines). On a `bundle`-engine remote they exit cleanly with
nothing to do.

### 9.1. Garbage collection (`gc`)

`gc` reclaims pack objects that are no longer referenced by any
`chain.json`. Bundle-engine remotes have no garbage to collect —
every push writes a fresh, self-contained bundle — so `gc` is a
no-op there.

```text
git-remote-object-store gc <remote> [--mark-only] [--sweep-only] [--force] [--grace-hours <HOURS>]
```

#### When to run

Run `gc` after any operation that detaches packs from the chain:

- **Force pushes** — the previous baseline and any segments that
  were rewritten become orphans.
- **Branch deletions** — packs unique to the deleted branch are no
  longer referenced.
- **Compactions** — `compact` rewrites a chain to a single segment;
  every pre-compact segment pack becomes an orphan.
- **On a regular schedule** — for active buckets, a weekly cron is
  the simplest way to keep the bucket tidy without thinking about
  it.

`gc` is read-mostly during the mark phase and only deletes during
sweep. It is safe to run against a live bucket; concurrent pushes
take the per-ref lock and sweep re-checks the orphan set before
deletion.

#### Default flow: mark + sweep in one command

```bash
git-remote-object-store gc origin
```

This invokes both phases:

1. **Mark** — list every pack key, intersect against every
   `chain.json`'s segment set, and write a tombstone at
   `<prefix>/gc/tombstones-<run-id>-<rfc3339>.json` listing the
   orphan packs.
2. **Sweep** — re-list pack keys, re-check each tombstoned pack
   against the latest chains (a concurrent push may have re-pointed
   to a previously-orphan pack via content-hash dedup), and delete
   the packs that are still orphan AND whose tombstone is older
   than the grace window.

Fresh tombstones from this same invocation will not sweep — they
have not yet aged past the grace window. Re-running `gc` after the
grace window applies them.

#### Cron-friendly split

The grace window protects in-flight readers: a clone that started
before the mark phase is allowed to finish even if `gc` decided
the pack was orphan. For that to work, mark and sweep need to run
**at least one grace window apart**.

The simplest schedule is a single weekly job. Each invocation
sweeps last week's tombstones and writes this week's. You do not
need to split mark and sweep into separate jobs to get the grace
behaviour — the grace check inside sweep handles it.

Sample crontab (Sunday 03:00 local time):

```cron
0 3 * * 0  /usr/local/bin/git-remote-object-store gc s3+https://my-bucket.s3.us-west-2.amazonaws.com/my-repo?profile=ops >> /var/log/grobs-gc.log 2>&1
```

Sample GitHub Actions workflow (weekly, manual trigger also
allowed):

```yaml
name: Bucket GC
on:
  schedule:
    - cron: "0 3 * * 0"
  workflow_dispatch:

jobs:
  gc:
    runs-on: ubuntu-latest
    permissions:
      id-token: write   # for OIDC -> AWS
      contents: read
    steps:
      - uses: aws-actions/configure-aws-credentials@v4
        with:
          role-to-assume: arn:aws:iam::123456789012:role/gc-runner
          aws-region: us-west-2
      - run: cargo install --git https://github.com/dekobon/git-remote-object-store git-remote-object-store-cli
      - run: |
          git-remote-object-store gc \
            's3+https://my-bucket.s3.us-west-2.amazonaws.com/my-repo'
```

Operators who want the phases on different schedules — e.g. mark
nightly, sweep weekly — can pass `--mark-only` and `--sweep-only`.
Each `--mark-only` invocation writes a fresh tombstone; each
`--sweep-only` invocation sweeps tombstones that have aged past
the grace window.

#### Tuning the grace window

The grace window is the minimum age a tombstone must reach before
its packs are eligible for sweep. Default is 24 hours.

```bash
# Override per invocation:
git-remote-object-store gc origin --grace-hours 168    # 7 days

# Or via env var:
export GIT_REMOTE_OBJECT_STORE_GC_GRACE_HOURS=168
git-remote-object-store gc origin
```

Recommended values:

- **24h** — typical setup. Long enough that any normal `git clone`
  or `git fetch` finishes within the window.
- **7d** — buckets where multi-day clones are realistic (very
  large repos, slow links, scheduled mirroring jobs).

`--grace-hours 0` and `--force` are independent knobs. The former
removes only the age check; the latter also skips the orphan-set
re-check that protects against a concurrent push reusing the
tombstoned pack via content-hash dedup. For routine maintenance
keep both at their defaults; reach for them only during operator-
asserted-quiet windows.

#### `--force`: skip the grace window and re-check

```bash
git-remote-object-store gc origin --force
```

`--force` tells `gc`:

1. The operator asserts that no concurrent reads against this
   bucket are in flight.
2. Sweep should not require a grace window — apply tombstones
   immediately.
3. Sweep should not re-check orphan packs against the chains —
   delete what the tombstone said.

Use it for one-off cleanup after a known-quiet maintenance window
(release freeze, off-hours sweep). Do **not** wire it into a
recurring schedule — the protections it bypasses exist precisely
to keep clones from breaking under concurrent traffic.

#### Reading `gc` output

The mark phase reports the orphan count or that the bucket is
already clean:

```text
gc mark: N orphan pack(s) tombstoned (run id <uuid>).
gc mark: no orphan packs.
```

The sweep phase reports per-tombstone disposition:

```text
gc sweep: A tombstone(s) applied, B object(s) deleted, C repointed pack(s) skipped, D tombstone(s) deferred.
gc sweep: no tombstones present.
```

Field meanings:

- **applied** (`A`) — tombstones whose grace window has expired
  and whose orphan packs were processed this invocation.
- **deleted** (`B`) — pack keys actually removed from the bucket.
  Each pack contributes both its `.pack` and `.idx` to this count.
- **repointed pack(s) skipped** (`C`) — packs the tombstone listed
  as orphan but that the post-mark re-check found referenced by a
  current chain. A concurrent push reused the content-hashed pack;
  the tombstone correctly defers to the live reference and the
  pack is not deleted.
- **deferred** (`D`) — tombstones whose grace window has not yet
  expired. They remain on the bucket and will be considered on
  the next sweep.

### 9.2. Compaction (`compact`)

`compact` rewrites a ref's `chain.json` into a single baseline
segment at the current tip. Fetches against a long chain pay one
round trip per segment to walk the chain; collapsing the chain
restores fetch latency to the single-segment case. The pre-compact
segment packs become orphans for `gc` to reap on its next sweep.

```text
git-remote-object-store compact <remote> [--ref-name <REF>] [--force] [--with-gc] [--lock-ttl-seconds <SECS>] [--gc-grace-hours <HOURS>]
```

Like `gc`, `compact` applies only to `packchain` remotes; on a
`bundle`-engine remote it exits cleanly with nothing to do.

#### When to run

The default invocation audits every ref and only compacts those
that meet the heuristic — currently **more than 20 segments OR
more than 100 MiB of cumulative segment bytes since the last
baseline**. Compact each candidate ref one at a time; you confirm
the list interactively before any rewrite runs.

Typical schedule:

- **Active monorepos** — pair `compact` with the weekly `gc` cron.
  Pass `--with-gc` so a single invocation rewrites the chains then
  immediately reaps the orphan packs.
- **Long-lived release branches** — run `compact --ref-name
  refs/heads/release/X` after a force-push or large rebase so the
  next clone of that branch picks up a single-segment baseline.
- **Bundle URI consumers** — every `compact` advances the chain's
  `full_at` SHA, which is the `creationToken` clients cache against.
  Schedule compaction during low-traffic windows so cached clients
  rebuild against the new baseline at off-peak.

#### Targeting a single ref

```bash
git-remote-object-store compact origin --ref-name refs/heads/main
```

`--ref-name` accepts the fully-qualified ref path
(`refs/heads/<branch>`). Without it, `compact` scans every ref and
prompts before rewriting anything that meets the heuristic.

#### Bypassing the heuristic

```bash
git-remote-object-store compact origin --ref-name refs/heads/main --force
```

`--force` bypasses the segments-and-bytes check and rewrites the
chain unconditionally. Useful after a force-push when the segment
count is below the threshold but the operator still wants to
collapse the chain to a single baseline.

#### One-command cleanup with `--with-gc`

```bash
git-remote-object-store compact origin --with-gc
```

Runs `gc` mark+sweep against the same bucket after a successful
compact, so the freshly-orphaned segment packs are reaped in the
same invocation. `--gc-grace-hours` forwards to the sweep (default
reads `GIT_REMOTE_OBJECT_STORE_GC_GRACE_HOURS`, falling back to 24);
without `--with-gc` the flag is ignored.

#### Locking

`compact` holds the per-ref `chain.json` lock from chain read
through commit. Large repos can take many seconds to rewrite, so
the lock TTL needs to be high enough to cover the rewrite. The
default reads `GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS` (falling
back to 60 seconds); override with `--lock-ttl-seconds` per
invocation if your repo needs longer.

Concurrent pushes against the same ref will fail to acquire the
lock and surface the standard "ref is locked" error; they should
be retried after `compact` releases.

## 10. Bundle URI — faster `git clone` for large repos

### What it is

`bundle-uri` is a [git protocol capability](https://git-scm.com/docs/bundle-uri):
at the start of a clone, the server can tell git "before you ask me
for objects, download these pre-packaged bundle files from this URL."
Git fetches them in parallel with the normal protocol negotiation,
unpacks them locally, and then asks the server only for whatever the
bundles didn't already cover.

This crate's `packchain` engine stores every push as an immutable
content-addressed pack. Without `bundle-uri`, a fresh `git clone` has
to walk the chain of `chain.json` links through the helper protocol
to discover which packs to download. With `bundle-uri`, the helper
tells git the direct URL of the baseline pack up front, git pulls it
straight from object storage (or a CDN), and the helper protocol is
left to negotiate only the incremental tail since the baseline.

The "URI" in the name is literal: the helper emits one URL per ref
on stdout, and git fetches them.

### When to enable it

Turn it on when **at least one** of these is true:

- **The repo is large enough that the baseline pack is the
  bottleneck.** Pulling hundreds of MB directly from S3 / Azure /
  CDN — in parallel, with HTTP keep-alive, no per-object round
  trip — is typically much faster than walking the chain over the
  helper protocol.
- **You clone often** (CI fleets, ephemeral dev environments). Each
  runner caches the bundle by `creationToken` (the chain's `full_at`
  SHA) and skips re-downloading it until the next force-push or
  `compact` advances the baseline.
- **The bucket is fronted by a CDN.** For public-read buckets the
  helper emits the canonical bucket URL, so a CloudFront / Azure
  Front Door / Fastly cache in front of the bucket transparently
  absorbs the load.

### When to leave it off (the default)

- **Small repos.** The baseline fits in one or two round trips
  anyway; the setup overhead won't pay for itself.
- **`bundle`-engine remotes.** The baseline filename rotates on
  every push, so there is no stable URL to advertise. The flag is
  silently ignored — see [storage-engines.md](storage-engines.md).
- **Private buckets where the helper's stdout could leak.** Enabling
  it on a private bucket means emitting a time-limited presigned
  URL on stdout. Anyone who reads the git transcript (verbose CI
  logs, `git -c transfer.verbosity=2`, a captured `git remote -v`)
  can fetch the baseline until the URL expires. See the security
  notes below.
- **Azure with Entra-ID-only credentials.** Per-blob presigning
  requires a shared account key; the token-credential and
  SAS-env-var paths cannot sign per-blob. The entry is warn-and-
  skipped and the client falls back to the normal helper protocol
  fetch (correct, just not accelerated).

Enabling `bundle_uri=1` and failing to produce a URL is never fatal:
the helper logs a warning, omits that ref's entry, and the client
falls back to the regular helper-protocol fetch path.

### Enabling it

Opt in with `?bundle_uri=1` on a `packchain` remote:

```bash
git clone 's3+https://my-bucket.s3.us-west-2.amazonaws.com/repo?engine=packchain&bundle_uri=1'
```

The helper advertises one entry per ref:

```text
bundle.<ref>.uri=<url>
bundle.<ref>.creationToken=<full_at>
```

`creationToken` is the chain's `full_at` SHA. Clients cache the
fetched bundle and skip the network round trip on a subsequent
clone whenever the token still matches; force-push or `compact`
advances `full_at`, invalidating any cached bundle.

### Public-read vs private buckets

| Bucket layout | URL flag | Notes |
|---|---|---|
| Public-read S3 / CDN-fronted / anonymous-read Azure container | `?bundle_uri=1` | Default; helper emits the canonical bucket URL — no signing. |
| Private S3 / private Azure container | `?bundle_uri=1&bundle_uri_presign_ttl=<seconds>` | Helper emits a per-ref presigned URL (S3 SigV4 / Azure service-blob SAS) that expires after `<seconds>`. |

`bundle_uri_presign_ttl` is parsed as a positive integer of
seconds in the range `1..=604_800` (1 second to 7 days).
`=0` and values above 7 days are rejected at the URL boundary;
the 7-day cap matches AWS's hard ceiling on presigned URLs and
keeps both backends consistent. Choose the TTL to balance
accelerated-clone window vs URL-leakage risk: longer TTLs let
one clone reuse the URL across retries, but the URL grants
time-limited GET access to the bundle key to anyone who reads
it.

```bash
# Private S3 bucket, 1-hour TTL.
git clone 's3+https://acme-private.s3.us-west-2.amazonaws.com/repo?engine=packchain&bundle_uri=1&bundle_uri_presign_ttl=3600'

# Private Azure container with a shared-key credential alias.
AZSTORE_PROD_KEY=<base64-key> \
  git clone 'az+https://acme.blob.core.windows.net/repo?engine=packchain&bundle_uri=1&bundle_uri_presign_ttl=3600&credential=PROD'
```

### Security notes for private buckets

- **URL leakage**: anyone who reads the helper's stdout (e.g.
  `git -c transfer.verbosity=2`, CI log captures, `git remote
  -v` after the clone if the URL is persisted) sees the
  presigned URL. Choose `presign_ttl` shorter than your log
  retention if that matters.
- **No credentials on the wire**: the helper signs the URL itself;
  no credential material is emitted on stdout. The signed URL is
  derived from the credentials but does not contain them.
- **Azure credentials**: presigning requires a shared account
  key (the `AZSTORE_<ALIAS>_KEY` or `AZSTORE_<ALIAS>_CONNECTION_STRING`
  env var). Entra-ID `TokenCredential` and the SAS-env-var path
  cannot derive per-blob SAS — both fall back to
  `ObjectStoreError::Unsupported` at the wire line, the entry is
  warn-and-skipped, and the client falls back to the helper
  protocol fetch path. User-delegation SAS (Entra-ID-backed) is
  filed as a future enhancement.
- **7-day TTL ceiling**: AWS enforces a 7-day maximum on
  presigned URLs as part of the `SigV4` spec; this project
  applies the same cap to Azure for consistency. Asking for
  `bundle_uri_presign_ttl=604801` is rejected at URL-parse time
  with a clear error (`bundle_uri_presign_ttl` too large), so
  the helper never starts and `git clone` reports the bad flag
  immediately.

## 11. Troubleshooting

### Verbose helper output

```bash
GIT_REMOTE_OBJECT_STORE_VERBOSE=2 git push origin main
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
git-remote-object-store doctor origin --lock-ttl-seconds 60 --delete-stale-locks
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

# Live-cloud shellspec tier

This directory hosts shellspec specs that exercise the helper binaries
and the management CLI against **real cloud backends**, not the
container emulators that drive `make shellspec-integration`.

Coverage spans AWS S3 and Azure Blob. The two suites are independent —
operators can run either or both depending on which credentials they
have configured. Each backend is gated behind a per-suite flag (`LIVE_S3=1`
for S3, `LIVE_AZ=1` for Azure) plus the global cost-acknowledgement
guard.

## Why a separate tier

Container emulators diverge from real cloud on:

- SDK provider chains (env vars, profile files, IMDS, SSO).
- Eventual consistency and read-after-write timing.
- Throttling, retries, and error codes.
- Conditional-write semantics (`If-None-Match: *` against real S3
  versus the emulator's looser interpretation).
- LFS HTTPS streaming through real CDN edges.

Bugs that only show up against real AWS / Azure deserve a tier the
emulator suite cannot catch.

## Cost and safety

These tests issue real PUT / GET / LIST / DELETE calls against your
account. They are designed to be cheap (each spec writes a handful of
small objects under a unique per-run prefix and deletes them in
`AfterAll`), but **you pay every byte of storage and every request**
they make.

Two guards prevent accidental invocation:

1. The per-suite flag (`LIVE_S3=1` for AWS, `LIVE_AZ=1` for Azure)
   gates spec inclusion at `Skip if`, so a stray `shellspec spec/`
   invocation does not trigger them.
2. The acknowledgement variable
   `LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY=1` gates the suite at
   `BeforeAll` with a loud failure if unset.

The `make` targets set the per-suite flag; the acknowledgement variable
is yours to export deliberately. Both must be present.

## AWS S3 setup

### Required environment

| Variable | Purpose |
|---|---|
| `LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY=1` | Acknowledgement guard. |
| `LIVE_S3_BUCKET` | Pre-existing bucket you own. |
| `LIVE_S3_REGION` | Bucket region (e.g. `us-east-2`). |

### Optional environment

| Variable | Purpose |
|---|---|
| `LIVE_S3_PROFILE` | Named AWS profile; passed through as `?profile=` on every test URL. Omit to use the default credential chain. |
| `LIVE_ENGINE` | Storage engine (default `bundle`). Plumbed through as `?engine=`. |

You may keep these in `spec/live/.env` (gitignored) for local
convenience. The suite sources that file at startup if present.

### IAM permissions

The credential's policy must allow, scoped to
`arn:aws:s3:::$LIVE_S3_BUCKET` and
`arn:aws:s3:::$LIVE_S3_BUCKET/live-test/*`:

- `s3:ListBucket` (with the `live-test/*` prefix condition)
- `s3:GetObject`
- `s3:PutObject`
- `s3:DeleteObject`

The `BeforeAll` sentinel pre-flight writes, reads, and deletes a
test object under `live-test/<run-id>/.preflight` to validate every
required action before any scenario runs. A missing permission fails
fast with a message naming the failed call.

### Tools

The runner verifies these are on `PATH`:

- `aws` (AWS CLI v2)
- `git` (>= 2.40)
- `git-lfs` (only required for `lfs_spec.sh`)
- `jq`
- `script(1)` (only required for `manage_cli_spec.sh`)

Missing tools fail fast with the missing list (not one-by-one).

### Side effects on your home directory

The live suite preserves the operator's real `HOME` (the integration
suite does not — it isolates `HOME` to a scratch dir). This is required
so the AWS SDK can resolve `~/.aws/credentials`, `~/.aws/config`, and
the SSO cache. Two consequences worth knowing:

- `lfs_spec.sh` runs `git lfs install --skip-repo`, which writes a
  `[filter "lfs"]` section to `~/.gitconfig` if not already present.
  Operators who already have `git lfs install` in their environment
  (which is most LFS users) see no change. Operators who don't can
  remove the section by hand or by re-running `git lfs install --skip-repo`
  with a different config target.
- The repo-local `user.name`, `user.email`, and `commit.gpgsign=false`
  set by `git_scenarios_init` override your global `~/.gitconfig` for
  the per-test repos, so test commits don't pick up your real identity
  or signing key.

## Azure Blob setup

### Required environment

| Variable | Purpose |
|---|---|
| `LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY=1` | Acknowledgement guard. |
| `LIVE_AZ_ACCOUNT` | Storage-account name. |
| `LIVE_AZ_CONTAINER` | Pre-existing blob container you own. |
| `LIVE_AZ_CREDENTIAL_NAME` | Logical alias the helper resolves at runtime; passed through as `?credential=` on every test URL. |

### Optional environment

| Variable | Purpose |
|---|---|
| `LIVE_AZ_ENDPOINT_SUFFIX` | Endpoint suffix (default `blob.core.windows.net`). Override for sovereign-cloud accounts (`blob.core.usgovcloudapi.net`, `blob.core.chinacloudapi.cn`, …). |
| `LIVE_ENGINE` | Storage engine (default `bundle`). Plumbed through as `?engine=`. |

In addition, **one** of the following env vars must be set so that the
helper and the cleanup CLI can resolve the credential alias to actual
secrets. The helper reads these at runtime per
`src/object_store/azure/auth.rs`:

| Env var | Auth scheme |
|---|---|
| `AZSTORE_<ALIAS>_KEY` | Base64 account key (shared-key signing). |
| `AZSTORE_<ALIAS>_CONNECTION_STRING` | Full connection string (parsed for `AccountKey=`). |
| `AZSTORE_<ALIAS>_SAS` | SAS token (appended to every outgoing request). |

`<ALIAS>` is the ASCII-uppercased value of `LIVE_AZ_CREDENTIAL_NAME`.
Resolution order is KEY → CONNECTION_STRING → SAS; the first set
variable wins.

### Note: account key is visible in `ps` output during cleanup

The Azure suite passes the resolved account key to the `az` CLI as
`--account-key <value>` on argv (the only env-based alternative,
`AZURE_STORAGE_KEY`, is process-global and would shadow any other
shell state the operator has). For the lifetime of each `az` call,
the key is readable in `ps aux` / `/proc/<pid>/cmdline` by other
local users on the same host. Treat the live-tier credential as
disposable: scope it to the test container only, and rotate it after
suspicious activity. SAS tokens and connection strings have the same
exposure surface.

### RBAC / role permissions

The credential must allow the following data-plane operations against
the `live-test/*` blob path inside `$LIVE_AZ_CONTAINER`:

- `Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read`
- `Microsoft.Storage/storageAccounts/blobServices/containers/blobs/write`
- `Microsoft.Storage/storageAccounts/blobServices/containers/blobs/delete`

The built-in role **Storage Blob Data Contributor** scoped to the
container is the simplest way to grant these. The pre-flight (write,
read, delete a sentinel under `live-test/<run-id>/.preflight`)
validates the contract before any scenario runs and names the failed
operation if a permission is missing.

### Tools

The runner verifies these are on `PATH`:

- `az` (Azure CLI v2.50+)
- `git` (>= 2.40)
- `git-lfs` (only required for `lfs_spec.sh`)
- `jq`
- `script(1)` (only required for `manage_cli_spec.sh`)

Missing tools fail fast with the missing list (not one-by-one).

## Run

### AWS S3

```bash
export LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY=1
export LIVE_S3_BUCKET=my-test-bucket
export LIVE_S3_REGION=us-east-2
export LIVE_S3_PROFILE=my-test-profile  # optional

make shellspec-live-s3
```

### Azure Blob

```bash
export LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY=1
export LIVE_AZ_ACCOUNT=mystorageaccount
export LIVE_AZ_CONTAINER=git-remote-tests
export LIVE_AZ_CREDENTIAL_NAME=PROD
export AZSTORE_PROD_KEY=$(az storage account keys list ... --query '[0].value' -o tsv)

make shellspec-live-azure
```

### Both backends in one run

```bash
make shellspec-live   # fans out to S3 + Azure if both are configured
```

To pass a different storage engine through to the helper URL:

```bash
make shellspec-live-s3 ENGINE=bundle
make shellspec-live-azure ENGINE=bundle
```

## Cleanup

Every run scopes its writes under a unique prefix:

```text
live-test/<YYYYMMDDTHHMMSSZ>-<pid>-<rand>/
```

`AfterAll` plus an `EXIT`/`INT`/`TERM` signal trap recursively deletes
this prefix at the end of the run (or on `Ctrl-C`). The cleanup
function refuses to run unless its target prefix begins with
`live-test/`, so a buggy refactor that leaves the variable empty
cannot wipe the bucket root.

`SIGKILL` and host-crash leave orphans. The recovery path is:

```bash
# Dry-run: list run-ids older than 24h on every configured backend.
make shellspec-live-sweep

# Override the cutoff.
make shellspec-live-sweep AGE=7d

# Actually delete (not just list).
make shellspec-live-sweep COMMIT=1

# Restrict to a single backend (skips the other regardless of env).
bash utils/live-sweep.sh --backend s3
bash utils/live-sweep.sh --backend az --commit 1
```

Run-ids start with a UTC timestamp so the sweep is a single list call
per backend plus a lexicographic comparison against a synthetic cutoff
string. No clock skew assumptions; no recursive scan. Backends whose
required env vars are not set are skipped with a warning rather than a
hard failure, so an operator who has only S3 (or only Azure) configured
can still run the umbrella sweep.

## What the suite does **not** do

- Create or delete buckets / containers. They must pre-exist; the
  suite never provisions infrastructure.
- Run inside CI. A `workflow_dispatch` workflow with OIDC / federated
  identity is a sensible follow-up but adds infra (cloud accounts,
  repo secrets, IAM trust policies) best landed separately once the
  suite has stabilized locally.
- Test multi-region replication, throughput, large objects (>1 GiB),
  or scheduled / nightly runs.
- Reset, audit, or modify your existing bucket data outside
  `live-test/`.

## Layout

| Path | Role |
|---|---|
| `spec/live/s3/*.sh` | AWS S3 spec mirrors of `spec/integration/s3/`. |
| `spec/live/az/*.sh` | Azure Blob spec mirrors of `spec/integration/az/`. |
| `spec/support/live_common.sh` | Guard, env loader, run-id, prefix-safety, engine helpers. |
| `spec/support/live_s3.sh` | AWS-specific list / get / put / delete / pre-flight / setup / teardown. |
| `spec/support/live_az.sh` | Azure-specific list / get / put / delete / pre-flight / setup / teardown. |
| `utils/live-sweep.sh` | Cross-run prefix sweep across every configured backend. |

The integration-tier files at `spec/integration/{s3,az}/` and the
backend-agnostic helpers at `spec/support/{git_scenarios,bucket_assertions}.sh`
are reused unchanged.

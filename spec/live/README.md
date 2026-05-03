# Live-cloud shellspec tier

This directory hosts shellspec specs that exercise the helper binaries
and the management CLI against **real cloud backends**, not the
container emulators that drive `make shellspec-integration`.

The first cut covers AWS S3 only. Real Azure Blob coverage is a
follow-up; tracking issue [#59].

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

1. The `LIVE_*` per-suite flag (`LIVE_S3=1`) gates spec inclusion at
   `Skip if`, so a stray `shellspec spec/` invocation does not trigger
   them.
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

## Run

```bash
export LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY=1
export LIVE_S3_BUCKET=my-test-bucket
export LIVE_S3_REGION=us-east-2
export LIVE_S3_PROFILE=my-test-profile  # optional

make shellspec-live-s3
```

To pass a different storage engine through to the helper URL:

```bash
make shellspec-live-s3 ENGINE=bundle
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
# Dry-run: list run-ids older than 24h.
make shellspec-live-sweep

# Override the cutoff.
make shellspec-live-sweep AGE=7d

# Actually delete (not just list).
make shellspec-live-sweep COMMIT=1
```

Run-ids start with a UTC timestamp so the sweep is a single
list-objects-v2 call plus a lexicographic comparison against a
synthetic cutoff string. No clock skew assumptions; no recursive scan.

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
| `spec/support/live_common.sh` | Guard, env loader, run-id, prefix-safety, engine helpers. |
| `spec/support/live_s3.sh` | AWS-specific list / get / put / delete / pre-flight / setup / teardown. |
| `utils/live-sweep.sh` | Cross-run prefix sweep (driven by `make shellspec-live-sweep`). |

The integration-tier files at `spec/integration/{s3,az}/` and the
backend-agnostic helpers at `spec/support/{git_scenarios,bucket_assertions}.sh`
are reused unchanged.

[#59]: https://github.com/dekobon/git-remote-object-store/issues/59

# Environment variables

Reference for every environment variable read by `git-remote-object-store` —
the helper binaries (`git-remote-s3+https`, `git-remote-az+https`, …), the
LFS custom-transfer agent (`git-lfs-object-store`), the management CLI
(`git-remote-object-store`), and the test suites.

If you add a new env var, add it here. See
[Adding a new environment variable](#adding-a-new-environment-variable) at
the bottom of this page.

## Contents

- [Helper runtime](#helper-runtime)
- [AWS S3 credentials](#aws-s3-credentials)
- [Azure Blob credentials](#azure-blob-credentials)
- [Git invocation context](#git-invocation-context)
- [Tests and development](#tests-and-development)
- [Not honored](#not-honored)
- [Adding a new environment variable](#adding-a-new-environment-variable)

## Helper runtime

Read by the helper-protocol binaries and (where noted) by the management CLI.
All four are prefixed `GIT_REMOTE_OBJECT_STORE_`.

| Variable | Default | Effect | Read at |
|---|---|---|---|
| `GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP` | unset | Set to a truthy value (`1`, `true`, `yes`, `on` — case-insensitive) to allow `s3+http://` / `az+http://` against non-loopback hosts. Falsy values (`0`, `false`, `no`, `off`), unset, or any unrecognised token leave the gate closed (fail-safe). Loopback (`localhost`, `127.0.0.1`, `::1`) is always allowed. Same boolean vocabulary as URL flags (`?zip=`, `?bundle_uri=`). | `src/url.rs` (`ENV_ALLOW_HTTP`) |
| `GIT_REMOTE_OBJECT_STORE_VERBOSE` | unset | Numeric. `>= 2` raises the startup tracing level from `error` to `info`. Honored by every binary in the crate — the helper-protocol bins (`git-remote-s3+https`, `git-remote-az+https`, …), the management CLI (`git-remote-object-store`), and the LFS custom-transfer agent (`git-lfs-object-store`) in its default REPL mode — so verbosity is set the same way regardless of which entry point is invoked. For the helper protocol, `option verbosity 2+` can raise the level at runtime but cannot lower a level set here. The LFS agent's `enable-debug` subcommand is a separate orthogonal mechanism that routes `debug`-level logs to a file under `<git-dir>/lfs/tmp/`; it overrides this variable while active. | `src/protocol/tracing_init.rs` (`ENV_VERBOSE`) |
| `GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS` | `60` | Per-ref lock TTL in seconds. Locks older than this are considered stale. Honored by the helper push path, `compact --lock-ttl-seconds`, `doctor --lock-ttl-seconds`, `delete-branch`, and library consumers of `DoctorOpts::default()`. | `src/protocol/push.rs` (`ENV_LOCK_TTL_SECONDS`) |
| `GIT_REMOTE_OBJECT_STORE_GC_GRACE_HOURS` | `24` | Minimum age a GC tombstone must reach before its packs are eligible for sweep. Honored by `gc --grace-hours` and `compact --with-gc`. | `src/packchain/gc.rs` (`ENV_GC_GRACE_HOURS`) |

## AWS S3 credentials

The S3 backend hands credential resolution to the AWS SDK's default provider
chain (`aws_config::defaults`, configured in
`src/object_store/s3.rs::build_s3_config`). The chain reads — in priority
order — environment variables, the shared credentials file, IAM Identity
Center / SSO, web-identity (workload identity), and instance metadata.

The env vars below are the most common entry points; the SDK honors more.
See the [AWS SDK provider chain documentation][aws-chain] for the full list.

| Variable | Effect |
|---|---|
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | Static credentials. |
| `AWS_SESSION_TOKEN` | Session token for temporary credentials. |
| `AWS_REGION` (or `AWS_DEFAULT_REGION`) | Region for SigV4 signing. The URL's `?region=` flag overrides this. |
| `AWS_PROFILE` (or `AWS_DEFAULT_PROFILE`) | Named profile in `~/.aws/credentials`. The URL's `?profile=` flag overrides this. |
| `AWS_SHARED_CREDENTIALS_FILE` | Path to the shared credentials file. |
| `AWS_CONFIG_FILE` | Path to the shared config file. |
| `AWS_ENDPOINT_URL` | Override the inferred endpoint (MinIO, RustFS, on-prem S3). The URL host normally supplies this; the env var is a last-resort override. |

[aws-chain]: https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html

## Azure Blob credentials

The Azure backend uses its **own** env-var scheme keyed by a credential alias
selected with the URL flag `?credential=<NAME>`. The alias is uppercased,
then the helper looks for `AZSTORE_<UPPER>_KEY`, then
`AZSTORE_<UPPER>_CONNECTION_STRING`, then `AZSTORE_<UPPER>_SAS`. First match
wins; if none are set, the helper errors with the list of expected names.

| Variable | Effect | Read at |
|---|---|---|
| `AZSTORE_<ALIAS>_KEY` | Base64-encoded storage account key → Azure Storage shared-key v2 signing. Required for presigning `bundle_uri` URLs against private containers. | `src/object_store/azure/auth.rs::resolve_alias` |
| `AZSTORE_<ALIAS>_CONNECTION_STRING` | Full `DefaultEndpointsProtocol=…;AccountName=…;AccountKey=…` form. The `AccountKey=` field is parsed out and used for shared-key signing. | `src/object_store/azure/auth.rs::resolve_alias` |
| `AZSTORE_<ALIAS>_SAS` | Pre-issued SAS token appended verbatim to every outgoing request. Cannot presign `bundle_uri` URLs (no key material). | `src/object_store/azure/auth.rs::resolve_alias` |

`<ALIAS>` must match `[A-Za-z0-9_]+` — the helper validates this before
constructing the env-var name to avoid arbitrary names leaking into the
env lookup.

If `?credential=` is omitted, the helper falls back to the Azure SDK's
`DeveloperToolsCredential` (Entra ID), which walks its own env vars,
workload identity, managed identity, and the Azure CLI in turn. See the
[Azure Identity client library][azure-identity] for the variables that path
honors.

[azure-identity]: https://learn.microsoft.com/en-us/azure/developer/rust/

## Git invocation context

`git clone` sets these before invoking the helper; you do not usually need
to set them yourself.

| Variable | Effect | Read at |
|---|---|---|
| `GIT_DIR` | Path to the repository (or its `.git` directory). When set, the helper uses it as the primary candidate for `gix::discover`, joining it to cwd if relative; when unset, discovery falls back to cwd. Git itself exports `GIT_DIR` before invoking the helper during `git clone`, where the destination worktree is still empty so cwd alone would not discover. | `cli/src/lib.rs::resolve_repo_dir` |

## Tests and development

Read only by the test suites and `cargo xtask` tooling. Operators do not
need to set these unless running the test tier they gate.

### Cost-gated test suites

| Variable | Required by | Effect |
|---|---|---|
| `RUN_LARGE_BODY_TESTS` | `cli/tests/{s3,azure}_store_integration.rs` | Set to any non-empty value to opt into the > 5 GiB body tests (S3 single-PUT ceiling, Azure block-count regimes). Costs ~12 GiB of scratch disk per run. See [`.claude/rules/testing.md`](../.claude/rules/testing.md). |
| `LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY` | `spec/live/**` | Acknowledgement guard for the live-cloud shellspec tier. Without it, the tier fails fast in `BeforeAll`. |
| `LIVE_S3` | `make shellspec-live-s3` | Set to `1` to include the AWS live tier. |
| `LIVE_AZ` | `make shellspec-live-azure` | Set to `1` to include the Azure live tier. |

### Live-tier configuration

Required when the corresponding `LIVE_*` gate is set:

| Variable | Tier | Purpose |
|---|---|---|
| `LIVE_S3_BUCKET` | S3 | Pre-existing bucket you own. |
| `LIVE_S3_REGION` | S3 | Bucket region (e.g. `us-east-2`). |
| `LIVE_S3_PROFILE` | S3 | Optional. Named AWS profile; passed through as `?profile=` on every test URL. |
| `LIVE_AZ_ACCOUNT` | Azure | Storage-account name. |
| `LIVE_AZ_CONTAINER` | Azure | Pre-existing blob container you own. |
| `LIVE_AZ_CREDENTIAL_NAME` | Azure | Logical alias resolved at runtime; passed through as `?credential=`. The corresponding `AZSTORE_<UPPER>_KEY` / `_CONNECTION_STRING` / `_SAS` must also be set. |
| `LIVE_AZ_ENDPOINT_SUFFIX` | Azure | Optional. Endpoint suffix (default `blob.core.windows.net`). Override for sovereign-cloud accounts. |
| `LIVE_ENGINE` | Both | Optional. Storage engine (default `bundle`). Plumbed through as `?engine=`. |
| `ENGINES` | Both | Optional. Space-separated list of engines to fan out across (default: every implemented engine). |

See [`spec/live/README.md`](../spec/live/README.md) for the complete live-tier setup
(IAM permissions, RBAC roles, cost guidance).

### Container emulator helpers

The integration suite drives RustFS (S3) and Azurite (Azure) under Docker.
These are set automatically by `spec/support/rustfs.sh` and
`spec/support/azurite.sh`; they are documented here only because they appear
in the harness's hermetic-env tables.

| Variable | Effect |
|---|---|
| `AZSTORE_AZURITE_KEY` | Well-known Azurite account key; the integration suite exports it via `AZSTORE_<AZURITE_CREDENTIAL_ALIAS>_KEY` so the helper resolves `?credential=AZURITE`. |
| `AZURE_STORAGE_CONNECTION_STRING` | Exported for the `az` CLI used by the harness for one-time container creation. Not read by the helper itself (see [Not honored](#not-honored)). |

## Not honored

These variables look like they should work but are **not** consulted by any
production code in this crate. They are documented here so operators can stop
trying to use them.

| Variable | Why it isn't read |
|---|---|
| `RUST_LOG` | Every binary in the crate (helper bins, LFS agent, management CLI) installs a custom `tracing_subscriber` with a startup level pinned by `GIT_REMOTE_OBJECT_STORE_VERBOSE` (or, for the LFS agent, its `enable-debug` subcommand). None of them call `EnvFilter::try_from_default_env`, so `RUST_LOG` has no effect. One knob per concern — see [`GIT_REMOTE_OBJECT_STORE_VERBOSE`](#helper-runtime). |
| `AZURE_STORAGE_ACCOUNT`, `AZURE_STORAGE_KEY`, `AZURE_STORAGE_SAS_TOKEN`, `AZURE_STORAGE_AUTH_MODE` | Process-global Azure CLI / azure-sdk env vars. The helper uses the `AZSTORE_<ALIAS>_*` scheme above so multiple credentials can coexist in one shell without shadowing each other. The shellspec harness explicitly unsets these (`spec/spec_helper.sh`) to prevent confusion. |
| `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` | These are honored only indirectly, by the Azure SDK's `DeveloperToolsCredential` when `?credential=` is omitted from the URL. They have no effect on the helper's `AZSTORE_<ALIAS>_*` resolution path. |

## Adding a new environment variable

When you introduce a new env var:

1. Define a `pub const ENV_<NAME>: &str = "…"` constant alongside its read
   site (existing examples: `src/url.rs`, `src/protocol/tracing_init.rs`,
   `src/protocol/push.rs`, `src/packchain/gc.rs`).
2. **Add it to this page** in the appropriate section. New project-owned
   variables go in [Helper runtime](#helper-runtime); test-only gates go in
   [Tests and development](#tests-and-development).
3. If the variable is helper-runtime visible, add it to the `ENVIRONMENT`
   section of the relevant man page (`man/git-remote-*.1`).
4. If the variable is operator-facing, surface it where it matters in
   [`docs/getting-started.md`](getting-started.md) — at minimum the
   troubleshooting section.
5. Cover it with a unit test that asserts the read happens via the constant
   (e.g., `tests/url_parsing.rs` style) — never duplicate the literal string
   across code and tests.

`tests/env_var_doc_sync.rs` enforces step 2 mechanically: it scans every
`pub` / `pub(crate)` `const ENV_*` declaration under `src/` and fails if the
string literal is missing from this page. A `cargo test` is enough to catch a
forgotten doc row before it ships.

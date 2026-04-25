# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Phase 5 S3 backend (`src/object_store/s3.rs`): full `ObjectStore`
  implementation against `aws-sdk-s3` 1.x. The SDK owns SigV4, retries,
  and connection pooling; this module owns URL → SDK config translation
  (endpoint normalisation that strips both the bucket label and any
  query string before handing the URL to the SDK; region resolution
  that honours `?region=`, parses AWS hostnames, and falls back to
  `us-east-1` for non-AWS endpoints so SigV4 has a region to sign
  with), error classification (404→`NotFound`, 403→`AccessDenied`,
  412→`PreconditionFailed`, 409→`Conflict`, network/timeout→`Network`),
  and a hand-rolled multipart download orchestrator (HEAD for size,
  then concurrent ranged GETs through a Tokio semaphore, max 8 in
  flight, 16 MiB chunks, 25 MiB threshold) matching the upstream
  `boto3.s3.transfer.TransferConfig` defaults. `put_if_absent` calls
  `put_object().if_none_match("*")` and collapses both 412 and 409 to
  `Ok(false)` so racing `If-None-Match: "*"` PUTs surface as "lock not
  acquired" rather than as hard errors. `get_to_file` writes to a
  sibling `NamedTempFile` and persists on success so a partial failure
  cannot leave a corrupt destination. `delete` HEADs first to honour
  the trait's `Err(NotFound)` contract on missing keys (S3 DELETE is
  idempotent). Copy keys with reserved characters (`#` from
  `LOCK#.lock`) are percent-encoded before being placed in the
  `x-amz-copy-source` header. Integration tests run against MinIO via
  `testcontainers` behind the new `integration-s3` Cargo feature
  (Docker required); these cover round-trip put/get, pagination beyond
  one page, concurrent `put_if_absent` contention, the 50 MiB+
  multipart download path, percent-encoded copy, atomic-fail behaviour
  of `get_to_file`, and `AccessDenied` mapping.
- Phase 4 object-store seam (`src/object_store/`): backend-neutral
  `ObjectStore` async trait (eight methods covering list / head / get /
  put / put-if-absent / copy / delete), shared `Error` enum mapping S3
  and Azure failure codes onto `NotFound` / `AccessDenied` /
  `PreconditionFailed` / `Conflict` / `Network` / `Other`, and the
  `ObjectMeta` / `PutOpts` value types. The trait is dispatched via
  `Arc<dyn ObjectStore>` (`async_trait` macro keeps `dyn + Send + Sync`
  ergonomic). An in-memory `MockStore` lives behind a new `test-util`
  Cargo feature (also active under `cfg(test)`) so unit tests in this
  crate AND integration tests for phases 5–9 can drive push, fetch,
  locking, and doctor logic without MinIO/Azurite. The mock supports
  FIFO fault injection (`PreconditionFailed` on `put_if_absent`,
  `NotFound` on `head`, `Network` on `get_bytes`, `AccessDenied` on
  `list`) so Phase 8's stale-lock retry path is deterministic, and
  `insert_with` back-dates `last_modified` for the staleness check.
- Phase 3 git wrapper (`src/git.rs`): the eight helpers from upstream
  `git_remote_s3/git.py` ported onto `gix` (gitoxide) with two newtypes
  (`Sha`, `RefName`), a `GitError` aggregate, and a single private
  `run_git` helper that funnels every `git` subprocess through one
  stdio-disciplined entry point. `archive` uses `gix-archive`'s native
  zip writer; `bundle`/`unbundle` retain a subprocess fallback because
  `gix` 0.82 has no public bundle API. Spike result captured in
  `docs/development/spike-gix-bundle-parity.md`.
- Phase 1 scaffolding: Cargo manifest with the dependency set called out in
  `execution-plan.md` (tokio, thiserror/anyhow, tracing, time, serde,
  clap v4, url, gix and selected sub-crates, bytes, tempfile).
- Empty module skeleton matching §2 of the execution plan
  (`url`, `git`, `protocol/*`, `object_store/*`, `lfs`, `manage/*`).
- Placeholder `[[bin]]` shims for the four remote-helper schemes plus
  the management and LFS binaries.
- GitHub Actions CI workflow running `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo test`.
- Phase 2 URL parser (`src/url.rs`): `parse(&str) -> Result<RemoteUrl, ParseError>`
  for the `s3+https`, `s3+http`, `az+https`, `az+http` grammar in
  `execution-plan.md` §3.1. Includes addressing-style auto-detection
  (§3.4) with `?addressing=path|virtual` override, query-flag extraction
  (`zip`, `profile`, `credential`, `region`), and cleartext-HTTP gating
  (§3.5) — non-loopback `*+http://` is rejected unless
  `GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP=1` is set.
- Integration tests in `tests/url_parsing.rs` covering every concrete
  example in §3.1 plus negative cases for invalid bucket / account /
  container charsets, missing segments, unknown flags, illegal flag
  values, and cleartext-HTTP rejection. `proptest` round-trip
  (parse → display → parse) for the legal grammar.

### Changed

- Fixed §3.1 Azure example to use `myaccount` rather than `my-account`;
  the previous form contradicted the §3.5 account charset rule
  `[a-z0-9]{3,24}` (no hyphens).

### Changed

- Phase-1 spike result: `cargo` rejects `+` in `[[bin]] name` (it derives
  a crate name from the bin name and `+` is not a legal crate-name
  character). The cargo bins therefore use hyphenated names
  (`git-remote-s3-https`, `git-remote-s3-http`, `git-remote-az-https`,
  `git-remote-az-http`) and a later `xtask` step will rename / hardlink
  them to the `+` form expected by `git` at install time
  (see `execution-plan.md` §5.6 / §6).

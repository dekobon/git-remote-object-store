# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- LFS custom-transfer agent (`git-lfs-object-store`, Phase 10): a single
  binary that serves both backends. Subcommands `install`,
  `enable-debug`, and `disable-debug` mutate the local repo's
  `git config`; passing no argument (or `debug`, set automatically by
  `enable-debug`) starts the LFS REPL. The REPL handles the `init`,
  `upload`, `download`, and `terminate` events of the line-oriented
  JSON protocol: uploads HEAD `<prefix>/lfs/<oid>` and skip on hit,
  otherwise stream the body and emit a final `progress` plus
  `complete`; downloads stream to `<git-dir>/lfs/tmp/<oid>` and emit
  `complete` with the path. Debug logs go to
  `<git-dir>/lfs/tmp/git-lfs-object-store.log` when enabled, never to
  stdout. (#10)
- Management CLI (`git-remote-object-store`) with `doctor`,
  `delete-branch`, `protect`, and `unprotect` subcommands. Each accepts a
  remote URL (`s3+https://…`, `az+https://…`) or the name of a git remote
  configured in the current repository, and dispatches to the right
  backend through the `ObjectStore` trait. The doctor analyzes the
  on-bucket layout, offers to keep or quarantine duplicate bundles per
  ref (`<ref>_<uuid8>` quarantine refs by default; `--delete-bundle`
  switches to outright deletion), prompts for a replacement when `HEAD`
  is invalid, and scans `*.lock` keys against a TTL (`--lock-ttl`,
  defaults to 60 s) with optional `--delete-stale-locks`. Interactive
  prompts go through a `Prompter` trait so unit tests drive the same
  code path with a scripted prompter against `MockStore`. (#9)

### Security

- Disable `aws-sdk-s3`'s default `rustls` feature to drop the legacy
  `rustls 0.21` / `rustls-webpki 0.101.x` dependency chain pulled in by
  `aws-smithy-runtime/tls-rustls`. The crate now uses the modern
  `default-https-client` path (`rustls 0.23` / `rustls-webpki 0.103.x`),
  resolving GHSA-4p46-pwfr-66x6 (high — DoS via panic on malformed CRL
  BIT STRING) and the two webpki name-constraint advisories
  (GHSA-fjxv-7rqg-78g4, GHSA-fhc7-32rr-h57g).

### Fixed

- `release_lock` now propagates non-`NotFound` delete failures instead of
  silently swallowing them. When the push itself succeeds but the lock
  cannot be released, the outcome is replaced with
  `error <ref> "failed to release lock. ..."` matching upstream
  `cmd_push`'s `finally` block. A genuine push error is never masked by
  a release failure. (#18)

- `S3Store::get_to_file` now guards against concurrent object mutation:
  every GET carries `If-Match: <etag>` from the preceding `HeadObject`.
  If the object is overwritten mid-download, S3 returns 412 and the
  operation retries once before propagating `Error::PreconditionFailed`.
  (#20)

- Push batches no longer abort on the first per-push transport, git, or
  local-I/O failure. `push_batch` now catches `PushError::Store`, `Git`,
  `Io`, and `Sha` per-push and converts them to `error <ref> "..."` outcome
  lines so the batch continues, mirroring upstream `cmd_push`'s
  try/except shape (`../git-remote-s3/git_remote_s3/remote.py:286-296`).
  Without this, a single 5xx blip mid-batch would silently drop the
  outcome lines for already-completed pushes and leave git's local
  ref-tracking inconsistent with the remote. `PushError::Parse`,
  `InvalidLocalSpec`, and `RemoteRef` still abort the batch — those mean
  subsequent commands cannot be trusted.

- `url::is_valid_bucket` now rejects the AWS-reserved bucket prefixes
  (`xn--`, `sthree-`, `amzn-s3-demo-`) and suffixes (`-s3alias`,
  `--ol-s3`, `.mrap`, `--x-s3`, `--table-s3`), enforces the
  begin-and-end-with-alphanumeric rule, rejects consecutive periods, and
  rejects names formatted as IPv4 dotted-quads. `url::is_valid_container`
  now enforces the matching Azure rules: alphanumeric bookends and no
  consecutive hyphens. Closes #17.

### Added

- `ObjectStore::put_path` streams local files to the backend without
  buffering in process memory. The push handler now uses it for bundle
  and zip artifact uploads, removing OOM risk for large repos and the
  5 GiB single-PUT ceiling. (#21)

- Shared protocol-test helpers extracted into `tests/common/mod.rs`,
  eliminating ~100 lines of duplicated `git()`, `git_capture()`,
  `s3_url()`, `drive_in()`, and `git_available()` across
  `protocol_smoke.rs`, `protocol_fetch.rs`, and `protocol_push.rs`.
  (#19)

- Phase 8 `push` handler with per-ref locking (`src/protocol/push.rs`): the
  REPL now batches `push <refspec>` lines until a blank line and processes
  them sequentially under per-ref locks at `<prefix>/<ref>/LOCK#.lock`,
  acquired via the trait's `put_if_absent` (S3 `If-None-Match: *` /
  Azure `If-None-Match: *`). On contention the handler `head`s the lock
  and, if its `LastModified` exceeds the TTL (default 60 s, override via
  `GIT_REMOTE_S3_LOCK_TTL_SECONDS` per upstream parity), deletes and
  retries once; otherwise it surfaces a "lock held" error line. After
  acquiring the lock the handler re-lists bundles and rejects the push if
  another client wrote a different bundle ("stale remote") or left the
  ref in a multi-bundle state. Force pushes against a ref carrying a
  `PROTECTED#` marker are demoted to non-force and re-checked against
  `merge-base --is-ancestor`. The `?zip=1` URL flag triggers an
  additional `repo.zip` upload alongside the bundle, with
  `Content-Disposition: attachment; filename=repo-<short-sha>.zip` and
  `codepipeline-artifact-revision-summary` user metadata. Per-push
  outcomes (`ok <ref>` / `error <ref> <reason>`) are written one line per
  command, followed by the protocol's blank-line terminator. Closes #8.
- `git::bundle_at(cwd, …)`: path-only variant of `git::bundle` so the
  push handler does not have to hold `gix::Repository` (which is `!Sync`)
  across `.await`, mirroring the path-only `unbundle_at` Phase 7
  introduced.

- Phase 7 parallel `fetch` handler (`src/protocol/fetch.rs`): the REPL now
  collects `fetch <sha> <ref>` lines until a blank line and dispatches them
  through a `tokio::task::JoinSet` bounded by a `tokio::sync::Semaphore`
  with `MAX_FETCH_CONCURRENCY = 8` permits (parity with upstream's
  `boto3.s3.transfer.TransferConfig(max_concurrency=8)`). Each task
  downloads `<prefix>/<ref>/<sha>.bundle` to a private tempdir, runs
  `git bundle unbundle` against the local repository's working directory,
  and records the SHA in a session-wide `Arc<Mutex<HashSet<Sha>>>` so a
  later batch in the same REPL session skips already-fetched refs. The
  batch driver drains every task before returning so a single failure
  cannot leave zombies running into a closing helper. `protocol::run` now
  takes a `repo_dir: PathBuf` parameter; `run_main` derives it from the
  process cwd (set by git when it invokes the helper).

### Changed

- `protocol::ProtocolError::Push` now wraps a structured `push::PushError`
  enum (`Parse` / `InvalidLocalSpec` / `RemoteRef` / `Sha` / `Store` /
  `Git` / `Io`) instead of the Phase 6 `PushNotImplemented` placeholder.
  The REPL acquired a `Mode::Push` accumulator alongside the existing
  `Mode::Fetch` one; switching modes mid-batch resets the opposite
  accumulator (mirrors upstream `process_cmd`).
- `git::bundle` and `git::archive` now take `spec: &str` (a permissive
  rev-spec) instead of `&RefName`. Storage-key types remain strict; the
  rev-spec passed to git itself is just a string git already validates.
- `protocol::ProtocolError::Fetch` now wraps a structured `fetch::FetchError`
  enum (`Parse` / `Sha` / `Ref` / `Store` / `Io` / `Git` / `Join`) instead
  of the Phase 6 `FetchNotImplemented` placeholder.
- `git::unbundle` is now a thin wrapper over a new
  `git::unbundle_at(cwd, …)` path-only variant. The parallel fetch path
  uses the path variant because `gix::Repository` is `!Sync` and cannot be
  shared across spawned tasks.

- Phase 6 remote-helper protocol skeleton (`src/protocol/`): asynchronous
  REPL (`protocol::run`) generic over its reader/writer so tests can drive
  it via `tokio::io::duplex`, plus a shared `protocol::run_main` entry that
  every `git-remote-{s3,az}-{http,https}` binary now invokes. Implements
  the four Phase-6 commands: `capabilities` (announces `*push`, `*fetch`,
  `option`), `list` and `list for-push` (lists `<sha> <ref>` lines, sorted
  by `LastModified` descending, filtered to
  `^refs/.+/.+/[a-f0-9]{40}\.bundle$`, with `@<ref> HEAD` emitted only when
  not for-push and the head ref appears in the listing), and `option
  verbosity <n>` (responds `ok` and reloads the `tracing` filter to `info`
  for `n >= 2`, `unsupported` otherwise). Stripping happens against
  `<prefix>/` so a sibling-prefix repo cannot match. HEAD body is trimmed
  per upstream `.strip()` semantics; `Error::NotFound` on HEAD is
  swallowed silently. `fetch`/`push` lines are recognised but return a
  structured "not yet implemented" error pending Phases 7/8 — fail-fast
  rather than the upstream silent-queue-then-flush so `git fetch`/`git push`
  surfaces a clear reason. Stdin EOF is a clean exit; stdout `BrokenPipe`
  is caught at the top level and the process exits 0 (mirroring
  upstream's `os.dup2(devnull, stdout)` trick). On Unix, SIGPIPE is masked
  via `tokio::signal::unix::signal(SignalKind::pipe())` so writes return
  EPIPE rather than killing the process.
- Phase 6 backend factory (`protocol::backend::build`) dispatches a parsed
  `RemoteUrl` to `S3Store` (Phase 5) or returns
  `BackendError::AzureNotImplemented` for `RemoteUrl::Azure` until
  Phase 11 lands the Azure backend.
- Phase 6 stderr-only tracing initialiser (`protocol::tracing_init`)
  honours `GIT_REMOTE_OBJECT_STORE_VERBOSE` and the upstream-compat alias
  `GIT_REMOTE_S3_VERBOSE`; a numeric `>= 2` bumps the start level to
  `info`. The filter sits behind `reload::Layer` so the protocol can flip
  verbosity at runtime.
- `clippy.toml` now bans `println!`/`print!`/`dbg!` via `disallowed-macros`
  per execution-plan.md §5.8 / `.claude/rules/protocol-stdout.md`. The
  management CLI and LFS agent will opt out at the file level when they
  start writing to stdout in Phases 9/10.
- Tokio's `io-std` feature is now enabled so the helper binaries can read
  stdin and write stdout asynchronously.
- Smoke test `tests/protocol_smoke.rs` (gated on `feature = "test-util"`)
  drives `protocol::run` end-to-end against `MockStore` via
  `tokio::io::duplex`, asserting exact stdout bytes for capabilities,
  list / list for-push, option verbosity, the `fetch`/`push` stub error
  paths, EOF, blank lines, HEAD trimming, sibling-prefix collisions, and
  bundle-key filter rejections.
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
  `x-amz-copy-source` header. Integration tests run against RustFS
  (Apache-2.0) via `testcontainers` behind the new `integration-s3`
  Cargo feature (Docker required). The fixture pins the RustFS image
  tag explicitly so alpha-version drift cannot break CI silently.
  Tests cover round-trip put/get, pagination beyond one page,
  concurrent `put_if_absent` contention, the 50 MiB+ multipart
  download path, percent-encoded copy, atomic-fail behaviour of
  `get_to_file`, and `AccessDenied` mapping.
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

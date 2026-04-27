# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- `ObjectStore::get_to_file` now takes a `GetOpts` argument; `PutOpts`
  gains an optional `progress` field. Both carry an
  `Option<ProgressSink>` that backends drive at chunk boundaries
  (per-range for the S3 multipart download path, per body chunk for
  the S3 single-PUT and Azure download paths). Bundle / lock / HEAD
  call sites pass `GetOpts::default()` and `progress: None`; the LFS
  agent populates the sink. This is a public-API break for callers of
  `ObjectStore::get_to_file`. (#44)

### Fixed

- The LFS custom-transfer agent now emits `progress` events at each
  network-chunk boundary, mirroring upstream
  `git_remote_s3/lfs.py`'s `ProgressPercentage.__call__` callback.
  Previously the agent emitted a single end-of-transfer event with
  `bytesSoFar == size`, which left long uploads / downloads
  appearing frozen and stripped `git-lfs` of any signal to detect
  stalled transfers. Backends report bytes through a `ProgressSink`;
  the agent forwards them through an `mpsc` channel into live
  `progress` events on stdout. (#44)
- `S3Store::from_remote_url` now installs a custom
  `aws-smithy-http-client` with `pool_idle_timeout(30s)` so DNS
  rotation no longer wedges a long-running LFS session until the
  OS-level TCP timeout fires (~15 minutes on Linux). The same TLS
  provider as the SDK's `default-https-client` (`rustls-aws-lc`) is
  selected explicitly so cargo unifies on a single rustls stack. TCP
  keepalive is **not** wired here: `aws-smithy-http-client` 1.1.12's
  public `Builder` API exposes `pool_idle_timeout` but does not
  expose `tcp_keepalive`; the dominant pool-reuse-of-dead-VIP
  failure is fixed by the idle timeout alone. (#26, #27)
- `AzureStore::from_remote_url` now configures the SDK's HTTP transport
  with `pool_idle_timeout(30s)` and `tcp_keepalive(30s)`. Pooled
  connections to a rotated VIP can no longer wedge a long-running LFS
  session until the OS-level TCP timeout fires (~15 minutes on Linux).
  The custom transport leaves `ClientOptions::per_try_policies`
  untouched, so shared-key / SAS signing continues to fire on every
  request. (#26, #28)
- `push.rs` parse-error message now names the full
  `git-remote-object-store doctor` binary instead of the bare word
  `doctor`, matching the wording of the other doctor-pointing error
  paths. (#22)
- Management CLI (`doctor`, `delete-branch`, `protect`, `unprotect`)
  now accepts root-of-bucket remotes (empty repository prefix)
  end-to-end, building keys like `refs/heads/main/...` and `HEAD`
  without a leading slash. (#29, #32)
- `AzureStore::copy` now streams through a tempfile via
  `get_to_file` + `put_path` instead of buffering the whole body in
  RAM. Memory is bounded by the SDK's per-block partition size
  regardless of blob size, so `Doctor::evict_losing_bundle`'s
  duplicate-bundle quarantine no longer pulls multi-GiB bundles
  through the helper process. (#30)
- Replaced production `expect()` panics in `manage::doctor`,
  `protocol::fetch`, and `object_store::s3` with structured error
  propagation. Snapshot-lookup invariants now surface as
  `ManageError::Internal`; mutex poisoning is recovered via
  `PoisonError::into_inner`; the `JoinSet`/`Arc::try_unwrap` flush
  path falls back to a locked-flush instead of aborting. (#33)
- Under-lock duplicate-bundle push error now ends with the trailing
  `?` suffix used by every other `error <ref> "..."` message in the
  helper, so the wire format is consistent across the pre-lock and
  under-lock branches. Deliberate divergence from upstream Python,
  which omits the `?` on this path. (#34)
- Both `S3Store` and `AzureStore` now error with
  `ObjectStoreError::Other` when a `head_object` response omits
  `Content-Length`, instead of treating the missing header as
  `size = 0` and silently writing an empty file at the destination.
  Mirrors the existing `last_modified` guard. (#43)
- `AzureStore::put_path` streams files from disk via the SDK's
  `FileStream` + `BlockBlobClient::upload` (auto-partitioned
  `stage_block` + `commit_block_list`), restoring the cross-backend
  streaming guarantee from #21 that the Azure side had been silently
  inheriting from the trait's read-then-`put_bytes` default. Memory
  is bounded by `parallel × partition_size` (≈16 MiB by default)
  regardless of file size. (#42)
- `protocol::list::read_remote_head` now treats `Some("")` as a
  no-prefix repository, matching the rest of the helper. The previous
  inline `match` produced a `/HEAD` key for root-of-bucket remotes
  whose prefix parsed as the empty string, which never resolved
  on the wire.

### Changed

- Renamed `crate::object_store::Error` to `ObjectStoreError`. Every
  importer previously aliased it via `use ... as ObjectStoreError`;
  the rename pushes the action prefix into the type so pattern
  matches read `ObjectStoreError::NotFound(_)` natively. Breaking
  for external library consumers (none in-tree besides the helper /
  management binaries). (#37)
- Renamed `PushOutcome::as_protocol_line` to `to_protocol_line`
  (allocates `String` via `format!`, so `to_*` matches Rust API
  Guidelines C-CONV). Replaced the free helper
  `into_dialoguer_error` with `impl From<dialoguer::Error> for
  ManageError`, dropping the `map_err(...)` boilerplate at both
  call sites in favour of `?`. (#38)
- Renamed `ManageBranch::delete_branch`/`protect_branch`/
  `unprotect_branch` to `delete`/`protect`/`unprotect` — the
  receiver type already names the subject; the method-side
  `_branch` was redundant noise. The CLI subcommand names
  (`delete-branch`, `protect`, `unprotect`) are unchanged. (#39)
- Renamed `AzureBlobStore` to `AzureStore` (symmetric with
  `S3Store`); renamed `AzureAddressing::Subdomain` to
  `AzureAddressing::VirtualHosted` (symmetric with
  `S3Addressing::VirtualHosted` and matches AWS-canonical
  terminology); renamed the private `protocol::list::BundleEntry`
  to `ListedBundle` so it no longer collides with the public
  `manage::snapshot::BundleEntry`. (#40)
- Renamed `git::validate_ref_name` to `is_valid_ref_name` so the
  `bool`-returning predicate carries the `is_*` prefix per the
  project naming rules. (#41)
- Hoisted the empty-prefix key builder out of `manage` into a new
  `crate::keys` module so the protocol, LFS, and management layers
  all share one source of truth for `<prefix>/<suffix>` joining.
  Five sites (`push.rs`, `fetch.rs`, `list.rs`, `lfs/agent.rs`, plus
  three management call sites) previously open-coded the same
  empty-prefix `match`. Added `network_boxed` next to `other_boxed`
  in `object_store::error` so the seven open-coded
  `|e| ObjectStoreError::Network(Box::new(e))` closures collapse to
  function pointers.

### Tests

- Tightened protocol-test coverage: dropped the stale
  `bucket = "0.a"` proptest seed (no longer reachable from
  `arb_bucket()`), replaced placeholder `aaaa.bundle` /
  `bbbb.bundle` fixtures with realistic 40-hex SHAs, added a
  regression test for the previously-untested
  `parse_remote_sha_from_key` failure arm in `protocol::push`,
  added end-to-end S3 helper-binary coverage modeled on the
  existing Azure pattern (push / clone / fetch / LFS), and pinned
  `option verbosity` behaviour for `n >= 2`. (#35)
- Strengthened three tests surfaced by the audit-tests pass:
  `pre_lock_multi_bundle_rejection_surfaces_unchanged` now pins the
  byte-exact wire bytes (the loose `contains("multiple bundles")`
  would not have caught the missing `?` that #34 fixed); added
  `fix_head_out_of_range_select_returns_internal_error` to cover
  the HEAD-candidate `ManageError::Internal` branch that was
  structurally identical to the bundle-index branch but lacked
  coverage; and the Azure `put_path_with_opts_uploads_body` test
  now verifies `content_disposition` and `x-ms-meta-*` propagate on
  the wire via a signed HEAD, mirroring its S3 sibling.

### Documentation

- Clarified the `ObjectStore::copy` trait contract: the body is
  preserved on every backend, but user-metadata propagation is
  best-effort. `S3Store::copy` (server-side `CopyObject`) does
  propagate it; `AzureStore::copy` (download-then-upload, since
  `azure_storage_blob` 0.12 does not ergonomically expose `Copy
  Blob` with shared-key auth) currently drops it. Callers must not
  depend on metadata round-tripping through `copy`.
- Removed the stale "Azure backend wired in Phase 11 — until then
  the REPL exits early with a 'not yet implemented' error" note
  from both Azure helper shim binaries; the wrappers now describe
  the current shape symmetrically with the S3 shims. (#31)
- `execution-plan.md` §1.1 ls-remote description now matches the
  actual `cmd_list` wire output: one line per bundle (not per ref),
  sorted by `LastModified` descending, with the `@<head> HEAD` line
  prepended only when not `list for-push` and the head ref appears
  in the listed bundles. (#36)
- `README.md` "Status" section now describes the gitoxide /
  subprocess split honestly: gitoxide is used for rev-parse,
  is-ancestor, ref-name validation, remote-URL inspection, archive,
  last-commit-message, ref discovery, and object resolution; bundle
  `create` and `unbundle` still shell out via the single `run_git`
  helper because `gix` 0.82 has no public bundle API. (#36)

## [0.1.0] - 2026-04-26

Initial release. Phases 1–14 of the [execution plan](execution-plan.md)
are complete: URL parser, gitoxide-backed git operations, the
`ObjectStore` trait with S3 and Azure Blob backends, the helper
protocol REPL, parallel `fetch`, locked `push`, the management CLI
(`doctor` / `delete-branch` / `protect` / `unprotect`), the LFS
custom-transfer agent, the helper-binary shims for both schemes, and
the documentation / packaging / release pipeline.

### Added

- README backend matrix and side-by-side S3/Azure examples covering
  clone, push, and management commands. (#14)
- `cargo install` instructions plus the `+`-form symlink workaround
  for git's helper lookup (xtask automation tracked as a follow-up
  issue). (#14)
- GitHub Actions CI jobs for the `integration-s3` and
  `integration-azure` features (Docker-backed RustFS / Azurite
  fixtures), plus a `markdownlint-cli2` job and an `--all-features`
  clippy pass so feature-gated code paths are linted. (#14)
- Tag-triggered release workflow (`.github/workflows/release.yml`)
  that builds release binaries on Linux x86_64 and macOS arm64,
  splits debug info into separate `.debug` / `.dSYM` artefacts via
  `objcopy --only-keep-debug` / `dsymutil`, strips the primary
  binary, and publishes both tarballs to a GitHub Release per the
  comment in `Cargo.toml`. (#14)
- `README.md` covering install, URL grammar, the
  `protocol.s3+https.allow always` / `protocol.az+https.allow always`
  config required for submodule URLs, AWS credential resolution, the
  Azure `AZSTORE_<NAME>_KEY` / `_CONNECTION_STRING` / `_SAS` aliases,
  and the LFS custom-transfer agent install flow. (#12)
- End-to-end binary tests (Phase 12) in
  `tests/azure_store_integration.rs`: drive `git push` / `git clone` /
  `git fetch` against the real `git-remote-az+http` helper binary
  through Azurite, plus an LFS round-trip exercising
  `git-lfs-object-store install`. The cargo bin name
  (`git-remote-az-http`) is symlinked to the `+`-form git looks up in a
  per-process tempdir prepended to `PATH`. Gated on
  `--features integration-azure` alongside the trait-level coverage.
  (#12)
- Azure Blob Storage backend (`AzureStore`, Phase 11): full
  `ObjectStore` trait implementation against the official
  `azure_storage_blob` 0.12 crate. `list` paginates through
  `BlobContainerClient::list_blobs`; `get_to_file` streams via the
  SDK's parallelised `BlobClient::download` (no hand-rolled multipart
  on Azure, asymmetric with S3 by design); `put_bytes` /
  `put_if_absent` use `BlockBlobClientUploadOptions::with_if_not_exists`
  to surface 409/412 contention as `Ok(false)`. Wired into
  `protocol::backend::build`, so existing `git-remote-az+https` /
  `git-remote-az+http` shims now drive a real backend. (#11)
- Custom shared-key signing policy (`auth::SharedKeySigningPolicy`):
  the SDK does not yet support shared-key authentication
  (`Azure/azure-sdk-for-rust#2975`), so we install our own per-try
  `azure_core::http::policies::Policy` that signs each outgoing
  request with the Azure Storage shared-key v2 scheme. This is the
  only way to authenticate against Azurite without an HTTPS+OAuth
  setup, and unblocks production accounts that still use account
  keys. SAS-token signing (`SasSigningPolicy`) and
  `?credential=<NAME>` env-var resolution
  (`AZSTORE_<NAME>_KEY` / `_CONNECTION_STRING` / `_SAS`) ship in the
  same patch. (#11)
- Azurite-backed integration suite
  (`tests/azure_store_integration.rs`, gated on
  `--features integration-azure`): mirrors the RustFS S3 fixture
  (one shared container, fresh-per-test container allocation, the
  16-racer `put_if_absent` contention canary, and round-trips for
  `head` / `list` / `copy` / `delete` / `get_to_file` zero-byte and
  multi-megabyte). (#11)
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
  `ObjectStore` async trait covering list / head / get / put /
  put-if-absent / copy / delete, shared `Error` enum mapping S3
  and Azure failure codes onto `NotFound` / `AccessDenied` /
  `PreconditionFailed` / `Conflict` / `Network` / `Other`, and the
  `ObjectMeta` / `PutOpts` value types. The trait is dispatched via
  `Arc<dyn ObjectStore>` (`async_trait` macro keeps `dyn + Send + Sync`
  ergonomic). An in-memory `MockStore` lives behind a new `test-util`
  Cargo feature (also active under `cfg(test)`) so unit tests in this
  crate AND integration tests for higher phases can drive push, fetch,
  locking, and doctor logic without MinIO/Azurite. The mock supports
  FIFO fault injection (`PreconditionFailed` on `put_if_absent`,
  `NotFound` on `head`, `Network` on `get_bytes`, `AccessDenied` on
  `list`) so Phase 8's stale-lock retry path is deterministic, and
  `insert_with` back-dates `last_modified` for the staleness check.
- Phase 3 git wrapper (`src/git.rs`): the helpers from upstream
  `git_remote_s3/git.py` ported onto `gix` (gitoxide) with two newtypes
  (`Sha`, `RefName`), a `GitError` aggregate, and a single private
  `run_git` helper that funnels every `git` subprocess through one
  stdio-disciplined entry point. `archive` uses `gix-archive`'s native
  zip writer; `bundle`/`unbundle` retain a subprocess fallback because
  `gix` 0.82 has no public bundle API. Spike result captured in
  `docs/development/spike-gix-bundle-parity.md`.
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
- Phase 1 scaffolding: Cargo manifest with the dependency set called out
  in `execution-plan.md` (tokio, thiserror/anyhow, tracing, time, serde,
  clap v4, url, gix and selected sub-crates, bytes, tempfile).
- Empty module skeleton matching §2 of the execution plan
  (`url`, `git`, `protocol/*`, `object_store/*`, `lfs`, `manage/*`).
- Placeholder `[[bin]]` shims for the remote-helper schemes plus the
  management and LFS binaries.
- GitHub Actions CI workflow running `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo test`.

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
- Fixed §3.1 Azure example to use `myaccount` rather than `my-account`;
  the previous form contradicted the §3.5 account charset rule
  `[a-z0-9]{3,24}` (no hyphens).
- Phase-1 spike result: `cargo` rejects `+` in `[[bin]] name` (it derives
  a crate name from the bin name and `+` is not a legal crate-name
  character). The cargo bins therefore use hyphenated names
  (`git-remote-s3-https`, `git-remote-s3-http`, `git-remote-az-https`,
  `git-remote-az-http`) and a later `xtask` step will rename / hardlink
  them to the `+` form expected by `git` at install time
  (see `execution-plan.md` §5.6 / §6).

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

### Security

- Disable `aws-sdk-s3`'s default `rustls` feature to drop the legacy
  `rustls 0.21` / `rustls-webpki 0.101.x` dependency chain pulled in by
  `aws-smithy-runtime/tls-rustls`. The crate now uses the modern
  `default-https-client` path (`rustls 0.23` / `rustls-webpki 0.103.x`),
  resolving GHSA-4p46-pwfr-66x6 (high — DoS via panic on malformed CRL
  BIT STRING) and the two webpki name-constraint advisories
  (GHSA-fjxv-7rqg-78g4, GHSA-fhc7-32rr-h57g).

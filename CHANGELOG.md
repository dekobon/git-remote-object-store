# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- `parse_bundle_key` now rejects bundle keys whose extracted ref
  path fails `gix-validate`'s ref-name check (`..` traversal,
  control characters, `.lock` suffixes). Mirrors the packchain-side
  hardening from #72 — both engines now validate ref paths before
  emitting them in the `list` response (#73).
- `Doctor::run` now delegates to `run_into<W: Write>` with an
  injectable writer, making the full doctor output (report, fixer
  prompts, stale-lock scan) unit-testable without spawning the
  management binary (#74).
- `make shellspec-live-s3` and `make shellspec-live-azure` now run
  every implemented storage engine in turn (`bundle`, `packchain`)
  instead of bundle only. The Makefile knob `ENGINE=<name>` is
  replaced with `ENGINES="<name> ..."`; pass `ENGINES=bundle` (or
  `ENGINES=packchain`) to scope a run to a single engine. Empty
  `ENGINES` is rejected at the target boundary instead of silently
  no-opping.
- Stale `Phase N / not yet implemented` doc-comments across the
  `packchain` module rewritten to reflect shipped reality: push
  (#63), fetch (#64), `read_blob` (#65), GC (#66), and compaction
  (#67) are described as implemented; references to "Phase 5 GC"
  replaced with `manage gc`.

### Fixed

- `git fetch --depth=N` from a shallow clone now correctly deepens the
  local repository. The helper previously merged new shallow boundaries
  with the prior `.git/shallow`, leaving the original tip in the file;
  git treats every entry in `.git/shallow` as hard parentless via
  `shallow.c::register_shallow` grafts, so the newly-installed parent
  commits stayed hidden and `git log` still showed only the tip. The
  helper now prunes any prior boundary whose parents are present in the
  ODB before writing, and unlinks the file when no boundaries remain
  (matching git's own `prune_shallow` semantics). Affects both bundle
  and packchain engines and all storage backends — the issue was first
  observed against real Azure but reproduces on every tier. New
  shellspec coverage exercises re-shallow, deepen-to-full-history, and
  successive-deepen flows (#78).
- `doctor` bundle-shape report no longer misclassifies packchain
  bookkeeping directories (`packs/`, `gc/`) and LFS storage
  (`lfs/`) as bare refs. Refs with a `chain.json` manifest now
  report "Ok" instead of "No bundles" (#75).
- Pushing an annotated tag now works against both engines. The
  packchain engine previously crashed at push time
  (`Expected object of kind commit but got tag`) because
  `gix::Repository::rev_walk` was called with the unpeeled tag-OID.
  The bundle engine appeared to succeed but emitted a pack
  containing only commit-reachable objects, so a fetch-back of the
  tag could not resolve the ref. Both engines now peel the
  resolved spec to its underlying commit and append the tag chain
  (annotated tag, or tag-of-tag) verbatim into the emitted pack
  via a second `count::objects` pass with
  `ObjectExpansion::AsIs`. Branch and lightweight-tag pushes are
  unaffected. Tag refs whose target is a tree or blob are
  explicitly rejected with `GitError::TagTargetUnsupported`;
  tracked separately as a feature request (#79).

### Removed

- **Breaking** (Rust API): `ProtocolError::EngineNotImplemented`
  variant removed. The variant was a leftover from packchain Phase 1
  scaffolding and was never constructed once push (#63), fetch (#64),
  `read_blob` (#65), GC (#66), and compaction (#67) shipped. Both
  `StorageEngine` variants (`bundle`, `packchain`) cover the full
  protocol surface, so the variant could not fire. Downstream code
  that exhaustively matched `ProtocolError` and had a branch for
  `EngineNotImplemented` will need to drop the branch (the branch was
  dead code anyway).

### Added

- Live-cloud shellspec tier expanded to engine parity. New
  `spec/live_s3_spec.sh` mirrors `spec/live_az_spec.sh`'s structure
  with unit-level coverage of `spec/support/live_s3.sh` (URL grammar,
  `aws` argv composition, `clear_prefix` safety guard) — runs as part
  of the default `make shellspec` suite, no cloud calls. New
  `spec/live/{s3,az}/manage_cli_spec.sh` and
  `spec/live/{s3,az}/shallow_fetch_spec.sh` port the integration-tier
  `manage_cli` and `shallow_fetch` scenarios to the live tier so the
  management CLI and shallow-fetch paths are exercised against real
  AWS / Azure SDK chains.
- `assert_ls_remote_ref_present` and `assert_ls_remote_sha` helpers
  in `spec/support/git_scenarios.sh` provide engine-agnostic pre/
  post-conditions for tests where the bundle-format-only assertions
  (`assert_bundle_count`, `assert_bundle_sha_for_ref`) are gated
  behind `live_engine_is_bundle` and would otherwise pass vacuously
  under packchain. Applied retroactively to `core_spec.sh`
  delete-branch and `force_push_spec.sh` force-push tests for both
  engines.
- `script(1)` added to the live-tier tools list in
  `spec/live/README.md` (only required for `manage_cli_spec.sh`'s
  pty-allocated `delete-branch` confirmation prompt).
- `packchain` `bundle-uri` presigned URLs (issue #76, completes
  the deferred follow-up from #71): a new
  `?bundle_uri_presign_ttl=<seconds>` URL flag asks the helper to
  emit per-ref signed URLs (S3 SigV4 / Azure service-blob SAS)
  instead of canonical bucket URLs, so private-bucket users can
  also benefit from `bundle-uri`-accelerated clones. The TTL
  parses to `Option<NonZeroU64>` so `=0` is rejected at the URL
  boundary. New `ObjectStore::presigned_get_url(key, ttl)` trait
  method drives the presigning per backend; the default impl
  returns `ObjectStoreError::Unsupported` so backends without a
  presigning model (`MockStore` in tests, Azure `TokenCredential`
  / SAS-env-var paths) inherit a clean error without a stub.
  S3 presigning uses `aws-sdk-s3::presigning::PresigningConfig`;
  Azure SAS is a hand-built `sv=2022-11-02` service-blob signature
  in `src/object_store/azure/sas.rs` (storage-key-signed; user-
  delegation SAS is out of scope per #76). Live round-trip tests
  exercise SigV4 against RustFS and SAS against Azurite — both
  emit the expected `X-Amz-Signature` / `sig` query parameters
  and the URL fetches the body via plain `reqwest::get` with no
  further auth. The `BundleUriError::PresigningUnsupported`
  variant is removed.
- `packchain` live integration tests against RustFS and Azurite
  (issue #69, completes the live-coverage gap for Phases 2–5 of
  #52): two new test binaries
  (`cli/tests/packchain_live_s3.rs`,
  `cli/tests/packchain_live_azure.rs`) drive a backend-agnostic
  scenario module (`cli/tests/common/packchain_live.rs`) against
  fresh-per-test buckets / containers. Scenarios cover Phase 2
  (first push lays down `chain.json` + `path-index.json` +
  `<tip>.bundle` + `packs/<sha>.{pack,idx}` + `FORMAT` + `HEAD`;
  incremental push appends a chain segment newest-first; force
  push collapses to a single segment); Phase 3 (fetch into an
  empty repo lands the tip; chain-walk fetch installs every
  segment in dependency order); Phase 4 (`read_blob` returns
  byte-equal content, and the cache survives an `.idx` deletion
  between calls — pinning `PackIndexCache` reuse without
  instrumenting the store); Phase 5 (`mark` writes a tombstone
  for orphan packs, `sweep` with `grace_hours = 0` deletes them
  through the production grace-comparison path). CI runs both
  suites in the existing integration-test jobs.
- `packchain` `bundle-uri` capability (issue #71): packchain remotes
  can now advertise the git remote-helper `bundle-uri` capability,
  letting `git clone` fetch the baseline bundle from a public bucket
  or CDN-fronted endpoint in parallel before the helper protocol
  negotiates only the incremental tail. Opt in with `?bundle_uri=1`
  on a `?engine=packchain` URL; bundle-engine remotes ignore the
  flag (their bundle filenames rotate per push, so a stable URL
  would race the next push). The `bundle-uri` command response
  emits one entry per ref (`bundle.<ref>.uri=<url>` +
  `bundle.<ref>.creationToken=<full_at>`), letting clients cache
  the bundle across clones until `full_at` advances (force push or
  compact). Per-ref parse failures warn-and-skip; a corrupt chain
  on one branch does not blackhole the others. Default emission
  is canonical bucket URLs (works against public-read buckets,
  S3-compatible CDNs, and Azure containers with anonymous-read
  access); private buckets opt in to per-ref presigned URLs via
  the `?bundle_uri_presign_ttl=<seconds>` flag (issue #76).
- `packchain` `compact` subcommand (issue #67, completes Phase 5
  of #52): new `git-remote-object-store compact <remote>` rewrites
  a packchain ref's `chain.json` to a single-segment chain at the
  current tip, with a fresh baseline pack and bundle. Old segment
  packs become orphans for `gc` to reap on the next mark/sweep
  cycle. Flags: `--ref <name>` to target a single branch (default
  scans every ref via the audit and prompts for confirmation),
  `--force` to bypass the segments-/bytes-since-`full_at`
  heuristic, `--with-gc` to chain mark+sweep after a successful
  compact, `--lock-ttl-seconds <N>` to extend the per-ref lock TTL
  for large repos (resolves Open Q4 from #52). Implementation uses
  the local-clone-then-repack approach: downloads the entire chain
  into a tempdir-backed bare repo, runs `build_baseline_pack` at
  the current tip, regenerates `path-index.json`, builds a fresh
  baseline bundle, uploads, and atomically commits the new
  `chain.json`. New `packchain::compact` library API and
  `manage::compact::Compact` runner.
- `packchain` doctor extensions (issue #68): the management
  `doctor` subcommand now emits a `=== Packchain ===` section
  whenever the resolved engine is `packchain`. The section reports
  orphan pack count and bytes, pending tombstones (run id, marked
  timestamp, age, orphan count) sorted oldest-first, per-branch
  segment / byte totals with a `[recommend compact]` flag when
  either threshold is exceeded, and dangling chain references
  (chain.json segments pointing at packs missing from the bucket)
  surfaced as ERRORS. New public `packchain::audit` module with
  `audit`, `AuditReport`, `OrphanReport`, `TombstoneRow`,
  `BranchAuditRow`, `DanglingRow`, and the threshold constants
  `COMPACT_SEGMENTS_THRESHOLD` (>20 segments) and
  `COMPACT_BYTES_THRESHOLD` (>100 MiB). Bundle-engine remotes see
  the existing report unchanged.
- Operator guide for `gc` (issue #70): a "Garbage collection"
  section in `docs/getting-started.md` covers when to run, the
  default mark+sweep flow, a cron-friendly weekly schedule with
  crontab and GitHub Actions samples, `--grace-hours` and
  `GIT_REMOTE_S3_GC_GRACE_HOURS` tuning, the `--force` re-check-
  skip semantics, and how to read the per-phase output.
- `packchain` storage engine — Phase 5 partial (orphan-pack garbage
  collection) of issue #52: new `git-remote-object-store gc <remote>`
  subcommand and `git_remote_object_store::packchain::gc` library
  module. Two-phase mark-and-sweep design: phase 1 lists every
  `<prefix>/refs/heads/*/chain.json`, derives the orphan pack set
  (in `packs/` but not referenced by any chain), and writes a
  tombstone at `<prefix>/gc/tombstones-<run_id>-<rfc3339>.json`.
  Phase 2 walks tombstones older than `--grace-hours` (default 24,
  env override `GIT_REMOTE_S3_GC_GRACE_HOURS`), re-derives the
  current orphan set to skip packs re-referenced between phases,
  deletes `.pack` + `.idx` idempotently, and removes the tombstone.
  Mark fails closed on a corrupt `chain.json` so a parse error never
  tombstones live packs. `--mark-only` and `--sweep-only` separate
  the phases for cron scheduling; `--force` skips both grace and
  re-check (operator-asserted safe). Sources of orphans handled:
  force push, lost-race push, aborted push, branch deletion, and
  (future) compaction. (#66, sub-issue of #52, partial — `compact`
  subcommand and `doctor` orphan-reporting extensions deferred to
  follow-ups.)
- `packchain::gc` public surface: `mark`, `sweep`, `MarkOpts`,
  `MarkOutcome`, `SweepOpts`, `SweepOutcome`, `DEFAULT_GRACE_HOURS`,
  `ENV_GC_GRACE_HOURS`, `grace_hours_from_env` for library consumers
  that drive GC programmatically (CI agents, scheduled lambdas).
- `manage::gc::Gc` runner that the CLI's `gc` subcommand wraps,
  matching the existing `Doctor` / `ManageBranch` shape so a
  non-interactive frontend can drive the same flow.
- `packchain` storage engine — Phase 4 (direct file access) of issue
  #52: new public `read_blob(remote, ref_name, path, &cache)` library
  API fetches a single file at a ref's tip without cloning or running
  git. The lookup walks `chain.json` + `path-index.json` to resolve
  the path to a blob SHA, scans each segment's `.idx` newest-first
  for the entry, and ranged-GETs the blob's pack bytes via
  `ObjectStore::get_bytes_range`, zlib-decompressing and applying
  `OFS_DELTA` / `REF_DELTA` chains up to a fixed depth (`MAX_DELTA_DEPTH
  = 50`, matching git's own cap). Total: 4–5 API calls for a warm
  lookup against a single-segment chain. (#65, sub-issue of #52)
- `PackIndexCache` — byte-bounded LRU keyed by `(prefix, content-sha)`
  that amortises pack-index parses across `read_blob` calls. Default
  capacity is 64 MiB; long-running consumers (CI agents, build
  systems) keep one cache for the lifetime of the process so the
  per-call cost drops to one `chain.json` GET, one `path-index.json`
  GET, and the ranged pack read. Single-shot callers can pass
  `&PackIndexCache::default()` and let it GC at drop.
- Engine guardrail on `Remote`: `Remote::open` now stores the resolved
  `StorageEngine`, exposed via `Remote::engine()`. `read_blob` rejects
  bundle remotes up front with `PackchainError::WrongEngine` rather
  than blindly fetching a non-existent `chain.json`.
- New `PackchainError` variants for Phase 4 failure modes:
  `WrongEngine`, `PathIndexAbsent`, `PathNotFound`, `MalformedPath`,
  `PathNotABlob`, `BlobNotInChain`, `MalformedPackEntry`, `Decompress`,
  `DeltaTooDeep`, `MalformedDelta`, and `InvalidRefName`. Each
  identifies the specific corruption / misuse class so a Phase 5
  `doctor` can flag them individually.
- `packchain` storage engine — Phase 3 (fetch) of issue #52: a
  packchain bucket written by Phase 2 is now clonable and fetchable.
  `git fetch` against `?engine=packchain` reads `chain.json`, walks
  segments newest → oldest until a locally-known ancestor is found,
  downloads the needed packs (and the `<full_at>.bundle` baseline
  when the receiver has no anchor) in parallel up to
  `MAX_FETCH_CONCURRENCY = 8`, and installs each pack
  oldest-first into the local `objects/pack` directory. Cross-batch
  dedup via the existing session-wide `FetchedRefs` cache works
  identically to the bundle engine. `chain.json` references that
  resolve to a missing pack on the bucket surface a typed
  `PackchainError::PackMissing` with the absent key, satisfying
  issue #64's "fail loud, not silent zero-byte fetch" criterion.
  (#64, sub-issue of #52)
- Shallow fetch on the packchain engine: under `option depth N`,
  the engine downloads segments **sequentially** newest-first,
  installs each, and runs `shallow_boundaries` after every install,
  stopping as soon as the boundary set is non-empty. This is a
  deliberate divergence from the bundle engine's parallel-fetch
  shape; the boundary calculation depends on inspecting the
  installed objects between segments, so a future "speed up
  packchain shallow fetch" change must NOT re-parallelise.
- `PackchainError::ChainAbsent`, `PackchainError::PackMissing`, and
  `PackchainError::BaselineMissing` typed variants for fetch-side
  failure modes; surfaced through the new
  `FetchError::Packchain(_)` wrapper. The `PackchainError` type is
  re-exported at the crate root so consumers can match on packchain
  failures without naming the `pub(crate)` engine module.
- `packchain` storage engine — Phase 2 (incremental push) of issue #52:
  pushing to `?engine=packchain` now writes a content-SHA-keyed pack
  under `packs/`, a sibling `.idx`, a newest-first `chain.json`
  manifest, a nested `path-index.json` mapping repo paths to blob
  SHAs at the tip, and (on first / force push) a baseline bundle at
  `<tip>.bundle` so Phase 3 fetch can short-circuit a fresh clone.
  First push is `TreeContents` from the local tip; incremental
  pushes use `TreeAdditionsComparedToAncestor`, which yields a
  self-contained ancestor-aware pack (the ancestor commit and tree
  travel with the new commit; only ancestor-only blobs are omitted,
  to be picked up from prior chain packs at fetch). `chain.json` is
  the linearization point — pack/idx/baseline upload pre-lock to
  keep the per-ref lock window bounded by JSON-PUT latency, and
  under the lock the push writes path-index → FORMAT → HEAD →
  chain.json. Concurrent pushers leave orphan packs on the loser;
  Phase 5 GC reaps them. (#63, sub-issue of #52)
- Force push on the packchain engine collapses the chain to a fresh
  single-segment manifest with `full_at = new tip` and replaces the
  baseline bundle, deleting the prior baseline at the old `full_at`
  best-effort (failure is logged at `warn` and never fails the push,
  since chain.json has already committed).
- Idempotent same-SHA push on the packchain engine: if the local tip
  matches the on-bucket `chain.tip`, push is a wire-level no-op
  (`ok <ref>` with no uploads), parity with the bundle engine's
  same-bundle short-circuit.
- Shallow-clone push rejection on the packchain engine: a local
  repository with a `.git/shallow` boundary that the rev-walk crosses
  surfaces `cannot push from a shallow clone` as a per-ref
  `error <ref>` line rather than producing a permanently incomplete
  remote.
- `packchain` storage engine — Phase 1 (foundation) of issue #52: new
  `Packchain` variant of the `?engine=` URL selector and `FORMAT` key,
  `get_bytes_range(key, Range<u64>)` on `ObjectStore` (S3 + Azure +
  mock, with HTTP 416 mapped to `ObjectStoreError::RangeNotSatisfiable`),
  on-bucket schema types (`chain.json` and nested-tree
  `path-index.json`) with a validating `Sha40` newtype, and a
  `git::extract_path_index` tree walker that builds a path-index from
  a tip commit. Phase 1's blanket-abort dispatch is replaced in this
  release by the per-engine routing introduced for Phase 2: a
  packchain `fetch` still aborts with `EngineNotImplemented` (Phase
  3 will fill it in) but `capabilities`, `list`, and `push` succeed.
  (#52)
- Per-chunk upload progress for `git push`: bundle and zip-archive
  uploads now emit one `tracing::info!` line per completed multipart
  part / staged block (S3 and Azure), routed to stderr to stay within
  helper-protocol stdout discipline. (#55)
- Gated `RUN_LARGE_BODY_TESTS=1` integration tests for >5 GiB upload
  round-trips on both S3 and Azure backends, mid-body abort tests
  that confirm the multipart abort path leaves no destination key
  visible, and a deterministic unit test pinning `read_file_part`'s
  io-error propagation. (#56)
- Hand-rolled multipart upload for S3 and explicit
  `stage_block` + `commit_block_list` for Azure above a shared
  `MULTIPART_PUT_THRESHOLD` (default 64 MiB). On S3 this lifts the
  5 GiB single-`PutObject` ceiling and the 5 GiB single-`CopyObject`
  ceiling — large LFS objects, large bundle pushes, and the
  `manage doctor --fix` quarantine path now succeed for multi-GiB
  objects. On Azure the dispatch criterion is the same so multi-GiB
  transfers no longer rely on the SDK's opaque internal chunking.
  Both backends emit one progress event per completed part / block.
  Below the threshold the existing single-call paths are preserved
  (no `CreateMultipartUpload` round trip for small bundles, lock
  files, or HEAD writes). (#53)
- `ObjectStoreError::PayloadTooLarge { limit_bytes }` variant for
  upload-body-too-big failures. The S3 classifier maps
  `EntityTooLarge` (HTTP 400) and HTTP 413 onto it (limit 5 GiB single
  PUT); the Azure classifier maps HTTP 413 and `RequestBodyTooLarge`
  onto it (limit 5000 MiB single Put Blob). The push wire-line now
  reads `"upload exceeds backend size limit (5 GiB)"` instead of
  dumping an opaque SDK chain when a bundle exceeds the single-PUT
  ceiling. (#54)
- Live-cloud shellspec tier under `spec/live/{s3,az}/` exercising the
  helper binaries against real AWS S3 and real Azure Blob. New make
  targets `shellspec-live-s3`, `shellspec-live-azure`, `shellspec-live`
  (umbrella), and `shellspec-live-sweep` are not invoked by `make ci`,
  `make pre-commit`, `make test`, or `make shellspec-integration`. Each
  suite is gated by its own per-suite flag (`LIVE_S3=1` / `LIVE_AZ=1`,
  set by the make target) plus the global acknowledgement variable
  `LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY=1` (loud-fail at
  `BeforeAll`). Every run scopes writes under `live-test/<run-id>/`;
  `AfterAll` plus an `EXIT`/`INT`/`TERM` trap delete the run prefix;
  the cleanup helpers refuse to run unless the target prefix is
  non-empty and starts with `live-test/`. `BeforeAll` runs a sentinel
  write/read/delete pre-flight to catch missing IAM / RBAC permissions
  before any scenario starts. The Azure suite resolves credentials via
  the existing `?credential=<NAME>` /
  `AZSTORE_<NAME>_KEY|CONNECTION_STRING|SAS` chain, and the
  `shellspec-live-sweep` target now scans both backends (configurable
  via `--backend s3|az|all`). Operator setup, env vars, costs, and
  recovery are documented in `spec/live/README.md`. (#59)
- Storage-engine selector: `?engine=<name>` URL query parameter and
  `<prefix>/FORMAT` bucket-level lock key. The only supported engine is
  `bundle` (the existing git bundle v2 format, also the default when
  `?engine=` is omitted). On the first push the engine is written to
  `FORMAT`; subsequent connects read and validate it. A `?engine=` value
  that conflicts with the stored `FORMAT` aborts with a clear error:
  `"URL specifies engine X but this bucket uses Y; remove the ?engine=
  parameter from the remote URL"`. Existing buckets without a `FORMAT`
  key continue to work — the key is written on the next push. (#51)
- Shallow-fetch support in the helper protocol: `option depth <N>` is
  now recognised and handled end-to-end. Depth is threaded through REPL
  state (reset after each batch so it applies per-operation only) and
  into `fetch_batch`, which runs a BFS from each fetched ref's tip to
  collect the correct boundary commits and writes them atomically to
  `.git/shallow` (read–merge–write so existing entries are preserved).
  BFS is used rather than topological-walk `.take(N)` because topo order
  does not match depth order at merge commits — all parents of the
  included set that lie outside it are boundaries. Phase 1 only: bundles
  are still downloaded in full; depth-limited bundle storage is a
  separate future feature. (#50)
- Shellspec integration suites under `spec/integration/{s3,az}/`
  exercising `git clone` / `git push` / `git fetch` /
  `git push --force` / `git push --delete` against live rustfs and
  Azurite Docker containers. Each backend covers core git ops,
  force-push protection (PROTECTED#), the
  `git-remote-object-store` management CLI (`protect`, `unprotect`,
  `delete-branch`, `doctor --delete-stale-locks`), the LFS round-trip
  via `git-lfs-object-store`, and concurrent / stale-lock contention.
  Three new Makefile targets (`shellspec-integration-s3`,
  `shellspec-integration-azure`, `shellspec-integration`) gate the
  new suites behind Docker + cloud-CLI prerequisites;
  `image-pin-check` guards against image-tag drift between the
  shellspec helpers and the Rust integration tests.
- `protocol::backend::build` now runs an eager probe (single
  `ListObjectsV2` for S3, `ListBlobs` first page for Azure with
  `maxresults=1`) at backend construction. The probe folds well-known
  failures into three categorical `BackendError` variants
  (`BucketNotFound`, `NotAuthorized`, `InvalidCredentials`) so helper
  binaries can emit single-line `fatal:` diagnostics that match
  upstream `git_remote_s3/remote.py:574-593`. The probe runs once per
  helper invocation and is off the per-command hot path. (#45)
- The LFS custom-transfer agent now emits `progress` events at each
  network-chunk boundary, mirroring upstream
  `git_remote_s3/lfs.py`'s `ProgressPercentage.__call__` callback.
  Previously the agent emitted a single end-of-transfer event with
  `bytesSoFar == size`, which left long uploads / downloads
  appearing frozen and stripped `git-lfs` of any signal to detect
  stalled transfers. Backends report bytes through a `ProgressSink`;
  the agent forwards them through an `mpsc` channel into live
  `progress` events on stdout. (#44)
- `Remote` struct as the primary library entry point for external
  consumers. `Remote::connect(url)` parses a URL and opens a verified
  backend connection in one call; `Remote::key(suffix)` computes correct
  prefixed storage keys; `Remote::get_head()`, `Remote::put_head()`, and
  `Remote::list()` cover the most common on-bucket operations; and
  `Remote::store()` exposes the underlying `ObjectStore` (as `&dyn
  ObjectStore`) for advanced use.
- Top-level re-exports for `ObjectStore`, `ObjectMeta`,
  `ObjectStoreError`, `RemoteUrl`, `Remote`, `RemoteError`,
  `BackendError`, and `BackendKind`; consumers no longer need
  three-level module-path imports.
- `ProtocolError::is_broken_pipe()` method; the private
  `is_broken_pipe(err: &io::Error)` helper is removed.

### Changed

- `packchain::list::list_refs` now fetches `chain.json` bodies in
  bounded parallel (`MAX_FETCH_CONCURRENCY = 8`, matching Phase 3
  fetch). Earlier sequential N round trips became a single bounded
  batch — meaningful for buckets with many branches; negligible
  for typical single-digit-branch repos.
- `packchain::list::list_refs` filters extracted ref paths through
  `gix-validate`'s `RefName::new` check before emitting them to
  git. A maliciously-planted key like
  `<prefix>/refs/heads/../etc/passwd/chain.json` would otherwise
  yield ref path `refs/heads/../etc/passwd` in the list response;
  the filter rejects such names with `tracing::warn!` and skips
  the entry. Defense-in-depth against bucket-write attackers.
- `delete-branch` documented as not deleting pack files for the
  packchain engine. Pack keys can be shared across branches under
  content-hash dedup (the umbrella issue's "exclusively owned by
  that branch" claim was incorrect); `delete-branch` removes only
  the branch's `chain.json`, `path-index.json`, baseline bundle, and
  `PROTECTED#` marker. Operators run `gc` afterwards to reclaim
  orphan packs. The behaviour itself is unchanged — `delete-branch`
  always operated under `<prefix>/refs/heads/<branch>/` only — but
  the invariant is now explicit.
- Cross-cutting packchain polish: `is_chain_json_key`,
  `optional_prefix`, and `parse_pack_key_sha` consolidated into
  `src/packchain/keys.rs` so `gc`, `list`, and `read` no longer
  duplicate the same string-shape inspectors. `pub mod read;`
  matches `pub mod gc;` so both submodules are reachable through
  the public rustdoc tree at
  `git_remote_object_store::packchain::{gc, read}`. New crate-level
  doc-test in `src/lib.rs` walks `Remote::connect` →
  `PackIndexCache::default` → `read_blob` using the crate-root
  re-exports. New `# Example` sections on `gc::mark` and `gc::sweep`
  show the canonical `Remote::connect` → `mark|sweep(remote.store(),
  remote.prefix(), Opts::default())` shape for library consumers
  driving GC programmatically.
- Tightened shellspec assertions
  (`spec/integration/s3/`, `spec/live/s3/`, `spec/integration/az/`):
  the `not ancestor` push wording is anchored to the documented
  `NOT_ANCESTOR_TOKEN` constant in `src/protocol/push.rs`;
  `git ls-remote` "ref absent" assertions distinguish empty-output
  success from masked failure via the new
  `assert_ls_remote_ref_absent` helper; the concurrent-push race
  scenario now requires both divergent winners to be observed across
  iterations rather than accepting `A || B`; the LFS spec is split
  into two focused `It`s so each example has exactly one
  load-bearing assertion that depends on the code under test. (#60)
- `S3Store::get_to_file` no longer ends in `unreachable!()`. The
  retry-on-412 (head→GET race) loop is rewritten as an explicit
  `match … { Err(PreconditionFailed) => retry once, other => other }`
  over a new private `head_then_download` helper, mirroring the
  Azure backend's shape. Every control-flow path now returns a
  value, so the panic primitive is gone. `clippy::unreachable` is
  denied at the workspace level to prevent regressions. (#49)
- Extracted local-branch primitives into a new `git::branch` submodule.
  `git::rev_parse` is removed; callers use `git::branch::resolve`
  instead. Added `BranchName` newtype that encapsulates the
  `refs/heads/<name>` invariant and `git::branch::current` reporting
  the branch HEAD points at (returning `None` for detached, unborn,
  and non-`refs/heads/` HEADs). (#47)
- Restructured as a Cargo workspace: the library crate
  (`git-remote-object-store`) stays at the repository root; the six
  binary targets move to a new `cli/` sub-crate
  (`git-remote-object-store-cli`). Install from source with
  `cargo install --path cli`; `cargo build --workspace` is unchanged
  for development builds.
- `protocol::run_main` is no longer part of the library API; it lives
  in the CLI crate. `protocol::capabilities` and `protocol::option` are
  now `pub(crate)`.
- `bundle_at` and `unbundle_at` now use a native `gix-pack 0.69`
  implementation (`src/bundle.rs`) instead of shelling out to
  `git bundle create` / `git bundle unbundle`. The `git` binary is no
  longer required at runtime for bundle operations. The implementation
  walks the commit graph with `rev_walk`, counts objects with
  `count::objects` (using `ObjectExpansion::TreeContents` to include
  trees and blobs), serialises with the `entry::iter_from_counts` →
  `bytes::FromEntriesIter` pipeline, and writes the header + pack
  atomically via `NamedTempFile::persist`. Unbundle parses the v2 header,
  checks prerequisites, and calls `Bundle::write_to_directory`.
- `git::config_add` / `git::config_unset` now write through
  `gix-config` and `gix-lock` instead of spawning `git config --add` /
  `--unset`. The in-process path acquires `.git/config.lock`, parses with
  `File::from_bytes_no_includes`, mutates via `SectionMut::push` /
  `remove`, and atomically renames over `<git-dir>/config`. `--unset` on
  a missing key now returns the typed `GitError::ConfigKeyNotSet` (the
  callers that previously matched on `Subprocess` are updated).
  `git::config_add_many` batches multiple key/value writes into a single
  read / parse / lock / write cycle; `lfs::install::install` uses it to
  set `lfs.customtransfer.<agent>.path` and `lfs.standalonetransferagent`
  in one pass. The LFS agent's `install` / `enable_debug` /
  `disable_debug` subcommands lose their `async` qualifier as a side
  effect. (#46)
- `protocol::run_main` now returns `std::process::ExitCode` instead of
  `anyhow::Result<()>` so the helper binaries
  (`git-remote-{s3,az}-{http,https}`) can render categorical
  `BackendError`s as upstream-style single-line `fatal:` messages
  without `anyhow`'s `Display` chain layering on top. The
  management binary (`git-remote-object-store`) downcasts through the
  anyhow chain to the same effect. (#45)
- `BackendError` lost its `S3` / `Azure` construction-failure variants
  in favour of `BucketNotFound { kind, name }`,
  `NotAuthorized { kind, action, name }`, and
  `InvalidCredentials { source }`. Greenfield project — no compat
  shim. (#45)
- `ObjectStore::get_to_file` now takes a `GetOpts` argument; `PutOpts`
  gains an optional `progress` field. Both carry an
  `Option<ProgressSink>` that backends drive at chunk boundaries
  (per-range for the S3 multipart download path, per body chunk for
  the S3 single-PUT and Azure download paths). Bundle / lock / HEAD
  call sites pass `GetOpts::default()` and `progress: None`; the LFS
  agent populates the sink. This is a public-API break for callers of
  `ObjectStore::get_to_file`. (#44)
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
- Documented backend size limits (AWS / Azure SDK API ceilings),
  lack of resume after upload failure, and the open `git push`
  upload-progress gap (#55) in a new "Known limitations" section in
  `README.md`, with cross-references from the s3 and azure
  module-level docs. (#57)
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
- ls-remote / `cmd_list` wire output documentation now matches the
  actual behaviour: one line per bundle (not per ref), sorted by
  `LastModified` descending, with the `@<head> HEAD` line prepended
  only when not `list for-push` and the head ref appears in the
  listed bundles. (#36)
- `README.md` "Status" section now describes the gitoxide /
  subprocess split honestly: gitoxide is used for rev-parse,
  is-ancestor, ref-name validation, remote-URL inspection, archive,
  last-commit-message, ref discovery, and object resolution; bundle
  `create` and `unbundle` still shell out via the single `run_git`
  helper because `gix` 0.82 has no public bundle API. (#36)

### Removed

- Internal `run_git` helper — was the sole subprocess-spawning point in
  production; removed once `bundle_at` / `unbundle_at` moved to the native
  `gix-pack` path.
- `GitError::GitBinaryMissing` — was only reachable through `run_git`;
  removed along with it.
- `GitError::Subprocess` — likewise only reachable through `run_git`.

### Fixed

- `list` command on packchain remotes now returns `chain.tip`
  rather than the baseline `<full_at>` SHA. The bundle-engine
  `list` handler parsed `<sha>.bundle` filenames; for packchain
  the bundle is the (fixed) baseline, not the moving tip, so
  after any incremental push `git ls-remote` / `git fetch` /
  `git pull` saw stale tips. Fix: engine-aware dispatch in
  `protocol::list::handle_list` — bundle keeps its bundle-key
  parser, packchain reads each ref's `chain.json` and reports
  `chain.tip`. Per-entry `chain.json` parse failures skip with
  a `tracing::warn!` so a single corrupt branch does not
  blackhole the whole listing. (#72)
- Sanitize the commit-message summary that flows from
  `git::last_commit_message` into the
  `codepipeline-artifact-revision-summary` user-metadata header on
  the zip-archive upload. ASCII control bytes (CR, LF, NUL, …) are
  collapsed to spaces so a forged commit summary cannot CRLF-inject
  forged user-metadata headers on the upload. Both backend SDKs
  reject CRLF at the transport layer today, but defending at the
  call site surfaces a clean, predictable header value instead of a
  cryptic 400.
- Dotted S3 bucket names (e.g. `bucketname.com`) in virtual-hosted URLs
  are now parsed correctly. `detect_s3_addressing` scans for the
  rightmost `.s3.` or `.s3-` AWS service infix anywhere in the host
  (instead of only checking the second label), and the virtual-hosted
  bucket extractor returns the full prefix preceding that infix (instead
  of just the leftmost label). Hosts of the shape
  `bucketname.com.s3.<region>.amazonaws.com` and the legacy
  `bucketname.com.s3-<region>.amazonaws.com` form now resolve to the
  correct bucket; the previous behaviour silently routed to the wrong
  bucket or produced a misleading `InvalidBucket` error. (#48)
- Both `S3Store` and `AzureStore` now apply HTTP-layer
  read/connect timeouts so a *hot* pooled connection that has gone
  silent (e.g. mid-LFS push when the server VIP rotates) fails fast
  instead of waiting for the OS-level TCP retransmit timeout
  (~15 minutes on Linux). Pool-idle alone bounds only *idle* pooled
  connections; a connection used within the last 30 s never goes
  idle. S3 sets `read_timeout(30s)` on the SDK's `TimeoutConfig`
  (smithy semantics: time-to-first-byte, not body-transfer); `connect_timeout`
  stays at the SDK default of 3.1 s. Azure sets `connect_timeout(10s)`
  and `read_timeout(30s)` on the custom `reqwest::Client` (per-read
  semantics: resets after each successful read). The third
  remediation checkbox in #26 ("force a fresh connection on
  connection-level retry") is reframed: the existing one-shot retry
  in `get_to_file` is a 412 mutation-race retry where the connection
  is healthy by definition, so forcing a fresh socket there does not
  help — the timeout-then-SDK-retry path covers the actual stuck-
  connection case. (#26)
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

- `packchain` `bundle-uri` (issue #71) now rejects derived
  ref-paths containing `=` before emission. Defense-in-depth
  hardening flagged by /security-review: `gix_validate::reference::name`
  bans `:`, `\n`, `\r`, ` `, control chars, and other framing-
  relevant bytes — but it permits `=`, which git's `bundle-uri`
  parser uses as the id/value split. The pre-existing `:` ban
  forecloses scheme injection (no host-relocation SSRF), but a
  ref-path with `=` could still produce a malformed wire entry on
  shared-prefix deployments where another tenant has bucket-write
  access. The new `is_safe_for_bundle_uri_emission` check warns
  and skips such entries. Mutation-verified
  (`skips_chain_json_with_equals_in_ref_name`).

## [0.1.0] - 2026-04-26

Initial release. The full feature surface is in place: URL parser,
gitoxide-backed git operations, the `ObjectStore` trait with S3 and
Azure Blob backends, the helper protocol REPL, parallel `fetch`,
locked `push`, the management CLI (`doctor` / `delete-branch` /
`protect` / `unprotect`), the LFS custom-transfer agent, the
helper-binary shims for both schemes, and the documentation /
packaging / release pipeline.

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
  per `.claude/rules/protocol-stdout.md`. The management CLI and LFS
  agent opt out at the file level when they need to write to stdout.
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
- URL parser (`src/url.rs`): `parse(&str) -> Result<RemoteUrl, ParseError>`
  for the `s3+https`, `s3+http`, `az+https`, `az+http` grammar.
  Includes addressing-style auto-detection with
  `?addressing=path|virtual` override, query-flag extraction (`zip`,
  `profile`, `credential`, `region`), and cleartext-HTTP gating —
  non-loopback `*+http://` is rejected unless
  `GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP=1` is set.
- Integration tests in `tests/url_parsing.rs` covering every concrete
  URL example in the grammar plus negative cases for invalid bucket /
  account / container charsets, missing segments, unknown flags,
  illegal flag values, and cleartext-HTTP rejection. `proptest`
  round-trip (parse → display → parse) for the legal grammar.
- Cargo manifest with the dependency set used throughout (tokio,
  thiserror/anyhow, tracing, time, serde, clap v4, url, gix and
  selected sub-crates, bytes, tempfile).
- Module skeleton (`url`, `git`, `protocol/*`, `object_store/*`,
  `lfs`, `manage/*`).
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
- Spike result: `cargo` rejects `+` in `[[bin]] name` (it derives a
  crate name from the bin name and `+` is not a legal crate-name
  character). The cargo bins therefore use hyphenated names
  (`git-remote-s3-https`, `git-remote-s3-http`, `git-remote-az-https`,
  `git-remote-az-http`) and a later `xtask` step will rename / hardlink
  them to the `+` form expected by `git` at install time.

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

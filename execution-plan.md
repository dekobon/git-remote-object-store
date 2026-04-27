# Execution Plan: Rust Port of `git-remote-s3` with Azure Blob Support

This document captures a step-by-step plan for rewriting
[`awslabs/git-remote-s3`](https://github.com/awslabs/git-remote-s3)
(Python, ~1.3K LOC of production code) in Rust as
[`git-remote-object-store`](https://github.com/dekobon/git-remote-object-store),
adding Azure Blob Storage as a second backend.

The plan is derived from a read of the upstream sources
(`git_remote_s3/{remote,manage,lfs,git,common,enums,__init__}.py`,
the test suite, and `README.md`) and the Git remote-helper protocol
spec.

## 0. Goals and non-goals

This is a clean-slate rewrite. We are **not** preserving wire- or
URL-level compatibility with `git-remote-s3`. Existing users will
need to re-add their remotes; that cost is paid once in exchange
for a much cleaner URL grammar and feature surface (see §3).

**Goals**

- Cover the same operational surface as `git-remote-s3` for an S3
  bucket:
  - The git remote-helper protocol commands `capabilities`, `list`,
    `list for-push`, `fetch`, `push`, `option`
  - Per-ref locking via S3 conditional writes (`If-None-Match: *`)
    with TTL-based stale-lock recovery
  - Parallel multipart fetch of `<sha>.bundle` objects
  - LFS custom transfer agent over the same bucket/prefix
  - Management CLI: `doctor`, `delete-branch`, `protect`,
    `unprotect`
- Add an Azure Blob Storage backend with the same surface area,
  using Azure's `If-None-Match: *` for locking.
- **HTTPS-native URL grammar** (see §3): the scheme name carries
  backend + transport (`s3+https://`, `az+https://`); the rest is
  a real RFC 3986 URL. Supports any S3-compatible endpoint
  (MinIO, R2, Wasabi, B2, …) and Azure clouds (public, US Gov,
  China, sovereign) with no extra config.
- Single Rust crate, single shared `ObjectStore` trait so backend
  code is the only place where S3 vs Azure diverge.
- Idiomatic Rust: no `unsafe`, no `unwrap`/`expect`/`panic` outside
  tests, conventions in `.claude/rules/rust.md` followed throughout.
- Ship a binary that is meaningfully faster than the Python
  implementation for `git clone`-style workloads (parallel fetch is
  the biggest single win).

**Non-goals (initial release)**

- Backwards compatibility with `git-remote-s3` URL schemes
  (`s3://`, `s3+zip://`) or its `profile@bucket` userinfo form.
  Users re-create remotes against the new grammar.
- Server-side hooks, web UI, or multi-user ACL features.
- Migration tooling for existing repos. On-disk S3/Azure object
  layout (`<prefix>/<ref>/<sha>.bundle`, etc.) is preserved, so a
  bucket pushed to by upstream Python can still be re-pointed at
  by the new client without re-uploading data — only the
  `git remote set-url` form changes.

## 1. Reference: upstream architecture in one screen

```
git_remote_s3/
  common.py        URL parser (regex)                  ~36 LOC
  enums.py         UriScheme {S3, S3_ZIP}              ~10 LOC
  git.py           subprocess wrappers around `git`    ~146 LOC
  remote.py        S3Remote + remote-helper REPL       ~600 LOC
  lfs.py           LFS custom transfer agent           ~221 LOC
  manage.py        Doctor + ManageBranch CLI           ~316 LOC
test/              parse_url_test, parallel_fetch_test, remote_test
```

Object-store schema used on the wire (must be preserved):

```
<prefix>/HEAD                            — pointer to default branch ref
<prefix>/<ref>/<sha>.bundle              — git bundle for that ref
<prefix>/<ref>/repo.zip                  — optional zip (s3+zip scheme)
<prefix>/<ref>/PROTECTED#                — zero-byte marker
<prefix>/<ref>/LOCK#.lock                — zero-byte advisory lock
<prefix>/lfs/<oid>                       — LFS object payloads
```

Protocol-level invariants:

- Push: take per-ref lock with conditional write, write
  `<sha>.bundle`, delete previous bundle, release lock.
- Lock TTL default: 60 s; stale locks may be replaced on contention.
- HEAD bootstrapping: first push to a ref writes `HEAD` if absent.
- `git ls-remote` output: order by `LastModified` desc, filter to
  `<prefix>/refs/.../<sha>.bundle`.

### 1.1 Wire-format invariants (cross-implementation contract)

The on-bucket layout is the only piece of upstream's surface this rewrite
*must* preserve byte-for-byte. Existing buckets created by
`git-remote-s3` must remain readable by this implementation, and buckets
created by this implementation must remain readable by upstream. Every
detail below is grounded in `../git-remote-s3/git_remote_s3/remote.py` —
treat that file as the spec, and re-read it before implementing the
relevant phase. AGENTS.md "Upstream is the source of truth" applies.

**Key paths.** All keys are constructed under `<prefix>/`, where
`<prefix>` is the second path segment of the parsed URL (or empty for
single-bucket repos at the root). The grammar:

| Key | Created by | Body | Notes |
|-----|-----------|------|-------|
| `<prefix>/HEAD` | first push to any ref | the bare ref string (e.g., `refs/heads/main`), no `ref:` prefix, no trailing newline | written via `put_object`; preserved verbatim |
| `<prefix>/<ref>/<sha>.bundle` | push | git bundle bytes (output of `git bundle create`) | `<ref>` is the full ref name including `refs/heads/...`; `<sha>` is the lowercase hex commit OID (40 chars for SHA-1, 64 for SHA-256 if/when gix gains parity) |
| `<prefix>/<ref>/repo.zip` | push when `?zip=1` | output of `git archive --format=zip` | optional; `?zip=1` query flag must be set on the URL |
| `<prefix>/<ref>/PROTECTED#` | doctor / management CLI | zero bytes | sentinel marker; presence is matched by **prefix** (`startswith("PROTECTED#")`), not by exact key, so any key under `<ref>/` starting with `PROTECTED#` is interpreted as the marker |
| `<prefix>/<ref>/LOCK#.lock` | acquire-lock | zero bytes | created via S3 `If-None-Match: *` (conditional write); presence == lock held |
| `<prefix>/lfs/<oid>` | LFS upload | LFS object payload | `<oid>` is the lowercase hex Git LFS OID (full 64-char SHA-256) |

**Listing semantics.** `get_bundles_for_ref(<ref>)` lists keys under
`<prefix>/<ref>/` and filters out:

- any key containing `PROTECTED#` (substring match)
- any key containing `.zip` (substring match — note this is permissive and
  excludes anything zip-related, not just the canonical `repo.zip`)
- any key containing `/LOCKS/` (legacy — kept for back-compat with older
  upstream layouts)
- any key ending with `.lock`

The remaining keys are the bundle objects, sorted by `LastModified`
descending — the most recent bundle is the active tip. Multiple bundles
under one ref is an error state (concurrent-write race) and is reported
as `error <ref> "multiple bundles exists on server"`.

**Locking.** Per-ref lock under `<prefix>/<ref>/LOCK#.lock`:

- Acquire: `put_object` with `IfNoneMatch: "*"`. A `412 PreconditionFailed`
  means the lock is already held.
- Stale-lock handling: on `412`, `head_object` the lock and compare
  `LastModified` against `now()`. If the difference exceeds the configured
  TTL, delete the lock and retry the conditional `put_object`.
- TTL default: **60 seconds**. Override via env
  `GIT_REMOTE_S3_LOCK_TTL_SECONDS` (upstream name preserved for parity).
  This is the only env var in the wire-format-invariant set; do NOT add
  parallel `..._OBJECT_STORE_...` aliases — match upstream exactly.
- Release: `delete_object` on the lock key.

**HEAD bootstrapping.** On push, if `head_object` on `<prefix>/HEAD`
returns 404, write `HEAD` with body = the ref being pushed. Subsequent
pushes do not update `HEAD` (it is the *initial* default ref, not the
*current* tip).

**ls-remote output.** The git remote helper writes **one line per
bundle** to stdout in the format `<sha> <ref>\n`, sorted by
`LastModified` descending. A given ref may appear on multiple
consecutive lines when more than one `<sha>.bundle` is present (e.g.
mid-rotation, before the previous bundle is deleted under lock); the
freshest bundle comes first because of the sort. A `@<head_ref> HEAD\n`
line is prepended only when not `list for-push`, the remote `HEAD`
object is present, and the ref it points at appears in the listed
bundles. Output ends with an empty line. This is the standard `list`
capability of the git remote-helper protocol.

**Encoding.** All keys, ref names, and HEAD bodies are UTF-8 byte
strings. No BOM, no normalization. Git itself constrains ref names to a
known charset (`gix-validate::reference::name`), so non-ASCII in keys
arises only from the user-supplied `<prefix>`, which is preserved
verbatim.

**Byte-for-byte parity is enforced by integration tests** (Phase 8 / 13)
that round-trip a real repository through both implementations against
the same MinIO bucket. Adding a new key path, changing `HEAD` body
encoding, or changing the lock-key suffix would break that test — and
existing user buckets — so any such change requires an explicit
divergence note in §6 and the same enforcement logic on the read side.

## 2. High-level Rust architecture

Single Cargo crate (binary + library), with the binary entry points
selected by `argv[0]` (BusyBox-style multi-call), or — equivalently —
several `[[bin]]` shims that each call into one library function.
Both produce the same on-disk layout once installed; the multi-call
form keeps the install footprint small.

```
src/
  lib.rs                         — public re-exports
  url.rs                         — URI parsing for s3:// / az://
  git.rs                         — wrappers over `git` subprocess
  protocol/
    mod.rs                       — remote-helper REPL, command parser
    capabilities.rs              — `capabilities` response
    list.rs                      — `list`, `list for-push`
    fetch.rs                     — parallel fetch
    push.rs                      — push + locking + zip variant
    option.rs                    — `option verbosity` etc.
  object_store/
    mod.rs                       — `ObjectStore` trait
    error.rs
    s3.rs                        — aws-sdk-s3 implementation
    azure.rs                     — azure_storage_blob implementation
  lfs/
    mod.rs                       — JSON-line custom-transfer agent
  manage/
    mod.rs
    doctor.rs                    — `doctor` analyzer + fixers
    branch.rs                    — delete/protect/unprotect
  bin/
    git-remote-s3+https.rs       — thin: protocol::run(parsed_url)
    git-remote-s3+http.rs        — thin: protocol::run(parsed_url)
    git-remote-az+https.rs       — thin: protocol::run(parsed_url)
    git-remote-az+http.rs        — thin: protocol::run(parsed_url)
    git-remote-object-store.rs   — clap-driven manage CLI: doctor,
                                   delete-branch, protect, unprotect
                                   (single binary, dispatches by URL)
    git-lfs-object-store.rs      — LFS custom-transfer agent
                                   (single binary, backend chosen by URL)
tests/
  url_parsing.rs
  protocol_smoke.rs
  s3_integration.rs              — backed by RustFS docker (Apache-2.0, pinned tag)
  azure_integration.rs           — backed by Azurite docker
```

### 2.1 The `ObjectStore` trait

A small, async, backend-neutral trait. Sketch:

```rust
#[async_trait]
pub(crate) trait ObjectStore: Send + Sync {
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, Error>;
    async fn get_to_file(&self, key: &str, dest: &Path) -> Result<(), Error>;
    async fn get_bytes(&self, key: &str) -> Result<Bytes, Error>;
    async fn put_bytes(&self, key: &str, body: Bytes, opts: PutOpts)
        -> Result<(), Error>;
    async fn put_if_absent(&self, key: &str, body: Bytes)
        -> Result<bool /* acquired */, Error>;
    async fn head(&self, key: &str) -> Result<ObjectMeta, Error>;
    async fn copy(&self, src: &str, dst: &str) -> Result<(), Error>;
    async fn delete(&self, key: &str) -> Result<(), Error>;
}

pub(crate) struct ObjectMeta {
    pub key: String,
    pub size: u64,
    pub last_modified: time::OffsetDateTime,
}

pub(crate) struct PutOpts {
    pub content_disposition: Option<String>,
    pub user_metadata: Vec<(String, String)>,
}
```

Both backends can satisfy this:

- **S3** via `aws-sdk-s3`'s `PutObject.if_none_match("*")`,
  `ListObjectsV2`, `HeadObject`, `CopyObject`, `DeleteObject`,
  and `aws-smithy-types-convert` for time conversions. Multipart
  download via `aws-sdk-s3` or the `aws-sdk-s3-transfer-manager`
  preview crate (final choice in §6).
- **Azure** via `azure_storage_blob` (the actively maintained
  official crate; `azure_storage_blobs` is legacy). Conditional
  put-if-absent maps onto `BlobClient::upload(...)` with the
  `If-None-Match: "*"` access condition.

Key trait-design decision: list returns a flat `Vec` (small page
counts in practice — refs per repo are bounded in the hundreds).
Pagination is hidden inside each backend implementation. If a future
repo ever has tens of thousands of refs we can add a streaming
`list_stream` variant; not needed for parity.

### 2.2 Async runtime

The git remote-helper protocol is a synchronous REPL (line in,
response out) but per-fetch parallelism and the AWS/Azure SDKs are
async. Wrap `main` in `#[tokio::main]` and `block_on` the protocol
loop. This is the same pattern used by `cargo`, `rustup`, and other
async-stack CLIs that are externally synchronous.

### 2.3 Error handling

- `thiserror`-derived `Error` enum at the `object_store` layer with
  variants for: `NotFound`, `AccessDenied`, `PreconditionFailed`,
  `Conflict`, `Network`, `Other(BoxError)`.
- Higher-level layers (push, fetch) translate to the
  remote-helper-protocol error lines (`error <ref> "<msg>"?\n`).
- `anyhow` only at the binary boundary for top-level `main()` errors.

### 2.4 Logging

`tracing` + `tracing-subscriber`. Default level `error`. Honour
`GIT_REMOTE_S3_VERBOSE` / `GIT_REMOTE_OBJECT_STORE_VERBOSE` env var
and the protocol's `option verbosity 2` to bump to `info`. Send to
stderr exclusively — stdout is the wire protocol.

## 3. URL scheme design

We use HTTPS-native URLs with a backend+transport scheme prefix.
Everything after the prefix is a real RFC 3986 URL — host, port,
path, and query all behave normally — so we get region/endpoint
flexibility for free and don't abuse the userinfo slot for
profile names.

### 3.1 Grammar

```
s3+https://<host>[:port]/<bucket>/<prefix>[?flags]
s3+http://<host>[:port]/<bucket>/<prefix>[?flags]      # local dev only
az+https://<account>.blob.<endpoint-suffix>/<container>/<prefix>[?flags]
az+http://<host>[:port]/<account>/<container>/<prefix>[?flags]   # Azurite
```

Concrete examples:

```
# AWS S3, virtual-hosted addressing
s3+https://my-bucket.s3.us-west-2.amazonaws.com/my-repo

# AWS S3, path-style addressing
s3+https://s3.us-west-2.amazonaws.com/my-bucket/my-repo

# Local RustFS / MinIO (CI / dev)
s3+http://localhost:9000/my-bucket/my-repo

# Cloudflare R2
s3+https://<account-id>.r2.cloudflarestorage.com/my-bucket/my-repo

# Backblaze B2
s3+https://s3.us-west-002.backblazeb2.com/my-bucket/my-repo

# Azure public cloud
az+https://myaccount.blob.core.windows.net/my-container/my-repo

# Azure US Government
az+https://myaccount.blob.core.usgovcloudapi.net/my-container/my-repo

# Azurite emulator (account is path-style)
az+http://127.0.0.1:10000/devstoreaccount1/my-container/my-repo

# Zip variant (push uploads repo.zip alongside each bundle)
s3+https://my-bucket.s3.us-west-2.amazonaws.com/my-repo?zip=1
```

### 3.2 Three deliberate departures from upstream

1. **No `profile@` userinfo slot.** Upstream put the AWS profile
   name in the RFC 3986 userinfo field, which is reserved for
   `user[:password]`. We move credential selection to a query
   parameter and/or out-of-band git config:

   - `?profile=prod` selects a named AWS profile for S3.
   - `?credential=foo` names an Azure credential alias.
   - `git config remote.origin.profile prod` (per-remote override)
     wins over the URL.
   - Default with no override: AWS credential provider chain for
     S3, `DefaultAzureCredential` for Azure.

2. **No `*+zip://` scheme.** Zip-archive emission is a feature
   flag, not a transport. Use `?zip=1` in the URL or
   `git config remote.origin.zip true`. One fewer helper binary.

3. **No region in the URL.** AWS region lives in the hostname for
   `*.amazonaws.com`; for custom S3-compatible endpoints, the
   endpoint URL itself carries everything the SDK needs. Override
   only via `?region=us-east-1` if a particular endpoint requires
   it (rare).

### 3.3 Parsed form

```rust
pub enum RemoteUrl {
    S3 {
        endpoint: url::Url,              // canonical https URL minus "s3+"
        bucket: String,
        prefix: Option<String>,
        addressing: S3Addressing,        // VirtualHosted | PathStyle (auto-detected)
        flags: RemoteFlags,
    },
    Azure {
        endpoint: url::Url,
        account: String,
        container: String,
        prefix: Option<String>,
        addressing: AzureAddressing,     // VirtualHosted | PathStyle (auto-detected)
        flags: RemoteFlags,
    },
}

pub struct RemoteFlags {
    pub zip: bool,                      // ?zip=1
    pub profile: Option<String>,        // ?profile=...     (S3)
    pub credential: Option<String>,     // ?credential=...  (Azure)
    pub region: Option<String>,         // ?region=...      (rare)
}
```

### 3.4 Addressing-style detection

- **S3**: if hostname starts with `<bucket>.s3` (or
  `<bucket>.<endpoint>`), it's virtual-hosted and the first path
  segment is the prefix. Otherwise path-style and the first path
  segment is the bucket.
- **Azure**: if hostname starts with `<account>.blob.` (the public
  pattern), it's subdomain-style and the first path segment is the
  container. Otherwise (Azurite, custom endpoints) path-style with
  the first path segment as the account.

The parser exposes an explicit override (`?addressing=path|virtual`)
to disambiguate hostnames that don't follow either convention.

### 3.5 Validation

- Scheme must be exactly `s3+https`, `s3+http`, `az+https`, or
  `az+http`. Anything else → parse error.
- `http` scheme is accepted only against loopback hosts
  (`localhost`, `127.0.0.1`, `::1`) **or** when
  `GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP=1` is set. This prevents
  accidental cleartext credentials against production.
- Bucket name follows the full AWS S3 General Purpose rules: 3–63 chars
  in `[a-z0-9.\-]`, must begin and end with a letter or digit, no
  consecutive periods, not formatted as an IPv4 dotted-quad, and none of
  the AWS reserved prefixes (`xn--`, `sthree-`, `amzn-s3-demo-`) or
  suffixes (`-s3alias`, `--ol-s3`, `.mrap`, `--x-s3`, `--table-s3`).
- Azure account: `[a-z0-9]{3,24}`. Azure container: 3–63 chars in
  `[a-z0-9-]`, must begin and end with a letter or digit, no consecutive
  hyphens.
- Trailing `/` on the prefix is stripped; missing prefix is
  allowed (single-bucket repo at the root).

Unit tests in `tests/url_parsing.rs` will cover every form above
plus negative cases (cleartext-on-non-loopback, malformed bucket
names, missing container for Azure, illegal flag values).

## 4. Phased delivery

Each phase is small, mergeable, and ends with the test suite green.
Cross-references to the phase numbers used here are useful in commit
messages (`refactor(phase-04): ...`).

### Phase 1 — Scaffolding

- Add deps to `Cargo.toml`:
  - Async/runtime: `tokio` (with `rt-multi-thread`, `macros`, `fs`,
    `io-util`, `process`, `signal`, `time`)
  - Errors: `thiserror`, `anyhow`
  - Logging: `tracing`, `tracing-subscriber`
  - Time: `time` with `parsing` + `formatting`
  - JSON (LFS): `serde`, `serde_json`
  - CLI: `clap` v4 with `derive`
  - URL parsing: `url` crate (RFC 3986)
  - Git ops: `gix` (with sub-crates `gix-hash`, `gix-validate`,
    `gix-archive` as needed)
  - Bytes/IO: `bytes`, `tempfile`
- Create the empty module skeleton from §2.
- Wire `cargo fmt`, `cargo clippy --all-targets -D warnings`,
  `cargo test` into CI.
- Add `CHANGELOG.md` (Keep a Changelog format) per
  `.claude/rules/changelog.md`.

Exit criterion: `cargo build`, `cargo test`, `cargo clippy` all pass
on an empty skeleton.

### Phase 2 — URL parser

- Implement `url::parse(&str) -> Result<RemoteUrl, ParseError>`
  for the grammar in §3.1, returning the enum from §3.3.
- Use the `url` crate to do the heavy lifting (RFC 3986 parsing of
  the body after the `s3+` / `az+` prefix), then layer our own
  validation and addressing-style detection on top.
- Cleartext-HTTP gating per §3.5 (loopback only unless env var
  override).
- Tests in `tests/url_parsing.rs` cover every concrete example in
  §3.1 plus negative cases.
- Property test (`proptest`) round-trip: parse → format → parse for
  randomly generated inputs in the legal grammar.

Exit criterion: full positive/negative table passes; cleartext
gating proven.

### Phase 3 — `gix` (gitoxide) wrapper

Native Rust git operations via `gix` instead of shelling out to
the `git` CLI. The wrapper module `src/git.rs` exposes the same
surface the upstream Python had, but each function is implemented
on top of `gix`:

- `bundle(folder, sha, ref) -> PathBuf` — write a git bundle for
  the named ref/sha. Uses `gix` bundle support if stable; falls
  back to `tokio::process::Command` invoking `git bundle create`
  for this one operation if `gix` parity isn't available yet
  (tracked as the open question in §6).
- `unbundle(folder, sha, ref)` — same fallback policy.
- `rev_parse(ref) -> Sha` — `gix::Repository::rev_parse`.
- `is_ancestor(ancestor, descendant) -> bool` —
  `gix::revision::merge_base` / `is_ancestor` helpers.
- `archive(folder, ref) -> PathBuf` (zip archive) —
  `gix-archive` (zip writer).
- `validate_ref_name(name) -> bool` — `gix-validate::reference::name`.
- `last_commit_message() -> String` — `gix::Repository::head_commit`.
- `remote_url(remote_name) -> String` — read from
  `gix::Repository::config_snapshot` / `remote_url`.

Newtype wrappers: `Sha` (use `gix-hash::ObjectId`) and
`RefName(String)` validated through `gix-validate`. Don't reinvent
existing newtypes — re-export the `gix` ones where appropriate.

Unit-test by building tiny repos with `gix` and exercising each
helper in-process — no subprocess, no `tempfile::TempDir` shell
dance.

Phase-3 spike (first task): write a one-page parity check for
`gix bundle` ↔ `git bundle`. If create+consume of bundles
roundtrips both directions across `git`/`gix`, we drop the
subprocess fallback entirely. If not, keep it for bundle ops only
and document the constraint.

### Phase 4 — `ObjectStore` trait + mock backend

- Define the trait and types from §2.1 in `src/object_store/`.
- Add a `mock` backend behind `#[cfg(test)]` (in-memory `BTreeMap`)
  used by every higher-layer test in phases 5–9. This lets push,
  fetch, and doctor logic be exercised without any cloud
  dependency.
- Mock supports configurable failure injection (return
  `PreconditionFailed` on demand) so locking tests don't need a real
  S3.

### Phase 5 — S3 backend

- Implement `object_store::s3::S3Store` using `aws-sdk-s3` and
  `aws-config`.
- Use the parsed `endpoint` from the URL as
  `aws_config::Builder::endpoint_url`. This makes MinIO, R2,
  Wasabi, and B2 work with no extra config beyond the URL itself.
- Honour `?profile=<name>` (and per-remote git config) by passing
  it to the AWS config loader's `profile_name`. Default: standard
  AWS provider chain.
- Honour `?region=<r>` if present; otherwise let the SDK extract
  from the hostname or fall back to `AWS_REGION` / profile config.
- Honour the parsed `addressing` to set `force_path_style` on the
  S3 client.
- `put_if_absent` calls `put_object().if_none_match("*")` and maps
  `412 PreconditionFailed`/`409 ConditionalRequestConflict` to
  `Error::PreconditionFailed`.
- Multipart download for objects > 25 MiB. **Hand-rolled ranged
  GETs**: HEAD for size, then issue concurrent
  `GetObject().range("bytes=N-M")` calls bounded by a Tokio
  semaphore (max concurrency 8, chunk 16 MiB), writing into an
  output `tokio::fs::File` at positioned offsets. The SDK still
  handles SigV4, retries, and connection pooling — we only own
  the orchestration (~100 LOC).
- Integration tests with RustFS (Apache-2.0) via `testcontainers`,
  pinned image tag (Docker required for `cargo test --features integration-s3`).

### Phase 6 — Remote helper protocol skeleton

- Implement the REPL: read stdin line by line, dispatch to handlers,
  write to stdout.
- `capabilities` response: `*push`, `*fetch`, `option`.
- `list` and `list for-push`:
  - `list_refs(prefix)` → strip prefix, sort by `LastModified` desc,
    filter to `^refs/.+/.+/[a-f0-9]{40}.bundle$`.
  - Print `<sha> <ref>` lines, plus `@<ref> HEAD` when not for-push.
- `option verbosity <n>` flips the `tracing` filter.
- Handle `BrokenPipeError` cleanly (Python's
  `os.dup2(devnull, stdout)` trick equivalent: redirect stdout to
  `/dev/null` and `exit(0)`).
- Smoke test: spawn the binary, drive the protocol via a duplex pipe
  against the mock object store, assert outputs.

### Phase 7 — Fetch (parallel)

- Collect `fetch` commands until a blank line, then run them
  concurrently with a `tokio::task::JoinSet` bounded by a semaphore
  (max 8, matching upstream `max_concurrency`).
- Per fetch:
  - Download `<prefix>/<ref>/<sha>.bundle` to a temp file
  - `git bundle unbundle` it for `<ref>`
  - Track `fetched_refs` in an `Arc<Mutex<HashSet<Sha>>>`
- Port `test/parallel_fetch_test.py` (uses thread-safety assertions)
  as a Rust integration test against the mock store.

### Phase 8 — Push (with locking)

- Mirror `cmd_push` from `remote.py`:
  1. Parse `+local_ref:remote_ref`, detect force-push and `protected`
     state.
  2. List existing bundles for the ref; bail if `>1`.
  3. Resolve local SHA via `git rev-parse`.
  4. If a remote bundle exists and not force, require
     `git merge-base --is-ancestor`.
  5. Build the bundle locally via `git bundle create`.
  6. **Acquire lock**: `put_if_absent("<prefix>/<ref>/LOCK#.lock")`.
     On `PreconditionFailed`, `head` the lock; if older than
     `lock_ttl`, delete and retry once.
  7. Re-list bundles; if a different bundle now exists, return
     "stale remote".
  8. `put_object` for `<sha>.bundle`.
  9. Init `<prefix>/HEAD` if absent.
  10. Delete old bundle.
  11. If `s3+zip`, `git archive` and `put_object` for `repo.zip`
      with `Content-Disposition` and the codepipeline metadata.
  12. **Release lock** (best-effort delete).
- Tests: lock acquisition contention, stale-lock recovery, force
  push of protected ref denied, ancestor check rejection, multi-
  bundle rejection, zip variant.

### Phase 9 — Management CLI (`git-remote-object-store`)

- Single binary; `clap` subcommands: `doctor`, `delete-branch`,
  `protect`, `unprotect`. Each accepts a remote URL (or a git
  remote name to be resolved against the current repo) and
  dispatches to the right backend through the `ObjectStore` trait.
- Port `Doctor`:
  - `analyze_repo` builds the `repos[name].refs[ref] = {protected,
    bundles[]}` map plus `HEAD`.
  - `fix_multiple_bundles`: prompt user to choose which bundle to
    keep; either delete others or move them to `<ref>_<uuid8>` —
    use the `uuid` crate.
  - `fix_head`: prompt for new HEAD branch when `HEAD` is invalid.
  - `list_and_handle_stale_locks`: list `*.lock` keys, age via
    `LastModified`, optionally delete with `--delete-stale-locks`.
- `ManageBranch` for the others.
- Interactive prompts via `dialoguer`. Tests run with stdin scripted
  via `assert_cmd` + `predicates`.

### Phase 10 — LFS custom transfer (`git-lfs-object-store`)

- Single binary serving as the LFS custom-transfer agent for
  both backends:
  - `init` event resolves the remote URL via
    `git remote get-url`; the URL scheme picks the backend.
  - `upload` event: HEAD the key
    (`<prefix>/lfs/<oid>`); if exists, emit `complete`; else
    upload with progress events (`{"event":"progress","oid":...,
    "bytesSoFar":...,"bytesSinceLast":...}`).
  - `download` event: download to `.git/lfs/tmp/<oid>`, emit
    `complete` with `path`.
  - Subcommands: `install`, `enable-debug`, `disable-debug`.
- Logging to `.git/lfs/tmp/git-lfs-object-store.log` when debug
  is enabled.
- Tests: line-oriented JSON in/out against the mock store.

### Phase 11 — Azure Blob backend

- Implement `object_store::azure::AzureStore` against
  `azure_storage_blob` 0.12 (the actively maintained official
  crate; still in beta as of 2026-04).
- Map the trait verbatim where the SDK supports it:
  - `list` → `BlobContainerClient::list_blobs` with a prefix and
    `into_pages()` pagination. Empty prefix is sent as `None`
    (Azurite signs `prefix=` differently than an absent param).
  - `get_to_file` → `BlobClient::download()` directly. The SDK
    performs internal parallel range downloads, so no
    hand-rolled chunking is needed (asymmetric with S3 by design
    — see §5.3).
  - `put_bytes` / `put_if_absent` → `BlobClient::upload` with
    `BlockBlobClientUploadOptions::with_if_not_exists()` (sets
    `If-None-Match: "*"`).
  - `head` → `BlobClient::get_properties`.
  - `delete` → `BlobClient::delete`.
- **SDK divergences from the original plan** (the SDK is still in
  beta; these decisions are kept narrow to preserve the trait
  contract while staying compatible with the SDK as it stabilises):
  - `copy` → download-then-upload round trip. The 0.12 crate does
    not expose `BlobClient::copy_from_url`; the only available
    server-side copy is `BlockBlobClient::upload_blob_from_url`,
    which requires a SAS-tokened source URL or
    `x-ms-copy-source-authorization` header — neither integrates
    cleanly with our credential model. Lock files (the only
    `copy` consumer in the trait) are zero bytes, so a
    download-then-upload is one extra round trip on a tiny payload.
  - **Custom shared-key signing policy.** The 0.12 crate accepts
    only `Arc<dyn TokenCredential>` (Entra ID) on its
    constructors. Azurite needs shared-key auth (no Entra ID
    server without an HTTPS+OAuth setup), and many production
    accounts still use account keys. We register an
    `azure_core::http::policies::Policy` that signs each request
    with the Azure Storage shared-key v2 scheme. Tracking
    upstream: `Azure/azure-sdk-for-rust#2975`.
- Note: a `Range` request against a zero-byte blob returns HTTP
  416. Bundles are never zero bytes, and the
  `meta.size == 0 → persist empty tempfile` short-circuit in
  `get_to_file` avoids any download SDK call against an empty
  blob, so the 416 path is unreachable from the trait surface.
- Credential resolution:
  1. If URL has `?credential=<NAME>`, look up env vars
     `AZSTORE_<NAME>_KEY`, `AZSTORE_<NAME>_CONNECTION_STRING`,
     or `AZSTORE_<NAME>_SAS` (in priority order).
  2. Else use `DeveloperToolsCredential` (env, workload identity,
     managed identity, Azure CLI, etc.). The 0.35 `azure_identity`
     crate renamed `DefaultAzureCredential` to
     `DeveloperToolsCredential`; behaviour is the same.
- Integration tests against Azurite via `testcontainers`. The
  Azurite container is started with `--skipApiVersionCheck`
  because the SDK ships a newer `x-ms-version` than the pinned
  Azurite image accepts (semantically a no-op for our request
  shapes).

### Phase 12 — Azure binaries and surface

- Add `git-remote-az+https` and `git-remote-az+http` as `[[bin]]`
  shims that pick the Azure backend based on the parsed URL.
- Wire Azure into the existing `git-remote-object-store` and
  `git-lfs-object-store` binaries (no new binaries needed for
  manage / LFS — the URL scheme dispatches).
- Document Azure auth in `README.md`, including:

  ```bash
  git config --global protocol.s3+https.allow always
  git config --global protocol.az+https.allow always
  ```

  (Required when these schemes appear inside submodule URLs.)
- End-to-end test: `cargo run --bin git-remote-az+https` against
  Azurite performing init → push → clone → fetch → LFS
  upload/download.

### Phase 13 — Parity QA and cross-implementation data interop

URL grammar is intentionally incompatible with upstream, but the
on-bucket object layout is preserved (`<prefix>/<ref>/<sha>.bundle`,
`HEAD`, `PROTECTED#`, lock files, `lfs/<oid>`). The QA matrix
verifies that:

- A bucket pushed to by upstream Python `git-remote-s3` can be
  cloned by `git-remote-s3+https` against the same MinIO endpoint
  (after `git remote set-url` to the new grammar).
- Conversely, a bucket pushed to by the Rust client can be
  cloned by the Python client (with the corresponding `s3://`
  URL).
- Concurrent push contention test: Python and Rust pushing the
  same ref concurrently — locking semantics still hold (only one
  succeeds, the other gets the documented error).

### Phase 14 — Documentation, packaging, release

- README rewrite covering both backends, with side-by-side
  examples and a feature matrix.
- `cargo install` instructions; `Homebrew`/`scoop` packaging
  follow-up tracked as separate issues.
- `CHANGELOG.md` filled in with the actual release notes.
- GitHub Actions workflow for `cargo test` (with and without the
  integration features), `cargo clippy`, `cargo fmt --check`,
  `markdownlint-cli2`, and the release-build pipeline that strips
  symbols (split-debuginfo) per the comment in `Cargo.toml`.
- Tag `v0.1.0`.

## 5. Tricky areas (called out for review during implementation)

### 5.1 Conditional-write semantics

S3 returns 412 (`PreconditionFailed`) if the key already exists, but
*also* 409 (`ConditionalRequestConflict`) if a racing write
"happens during" the PUT. Both must be treated as "lock not
acquired". Azure returns 412 for the analogous `If-None-Match: *`
case. Centralise this mapping in `object_store::error`.

### 5.2 Lock TTL clock skew

Upstream uses S3's `LastModified` (server-side wall clock) and
compares against `now()` on the client. With ±a few seconds of
client skew this is robust at the default 60 s TTL, but a
client whose clock is grossly wrong can either delete healthy
locks (too-fast clock) or fail to clear stale ones (too-slow).
Document this; consider sourcing the "now" from a trusted source
(e.g. `Date:` response header) — but only if it becomes a real
problem.

### 5.3 Multipart download (asymmetric across backends)

The two SDKs have different built-in capabilities, so the trait
implementations diverge:

- **S3** (`aws-sdk-s3`): no built-in parallel multipart download.
  We hand-roll ranged GETs in the trait impl: HEAD for size,
  then issue concurrent `GetObject().range("bytes=N-M")` calls
  via `tokio::task::JoinSet` bounded by a semaphore (max 8,
  chunk 16 MiB), writing positioned offsets to a
  `tokio::fs::File`. The SDK still owns SigV4, retries, and
  pooling — we only own the orchestration. Defaults match the
  Python `TransferConfig` settings (25 MiB threshold).
- **Azure** (`azure_storage_blob`): `BlobClient::download()` does
  parallel range downloads internally. Just call it. Tune
  concurrency/chunk size via the client builder if benchmarks
  later show a reason to.

Both backends therefore satisfy `ObjectStore::get_to_file` with
parallelism, but the implementation strategies differ — that's
intentional, not an oversight.

### 5.4 `validate_ref_name` correctness

The upstream regex at `git_remote_s3/git.py:130` is a partial
implementation of git's ref-name rules. Port it byte-for-byte for
parity, but also add a unit test against the actual cases in
`git/refs.c` `check_refname_component` (component-level forbidden
chars, double-slashes, leading dot, trailing `.lock`, etc.).

### 5.5 LFS storage location

`git-lfs-s3` writes to `.git/lfs/tmp/<oid>` and assumes that
directory exists (it does after `git lfs install`). On Windows
the same path applies; double-check directory separator handling
when porting (use `Path::join`, not string concatenation).

### 5.6 Cargo bin names containing `+`

Git invokes the helper as `git-remote-<scheme>`, so the on-disk
binary names must be exactly `git-remote-s3+https`,
`git-remote-s3+http`, `git-remote-az+https`,
`git-remote-az+http`. The `+` is legal in POSIX file names.
Cargo accepts `+` in `[[bin]] name` since at least edition 2021,
but verify in Phase 1 with a smoke build. Fallback if cargo ever
rejects: name the cargo bins with hyphens
(`git-remote-s3-https`) and rename / hardlink at install time
via a small `xtask` post-install helper.

### 5.7 Submodule allowance for new schemes

Document for users:

```bash
git config --global protocol.s3+https.allow always
git config --global protocol.az+https.allow always
```

(Required when these schemes appear inside submodule URLs. The
`s3+http`/`az+http` schemes are loopback-only by design and
shouldn't be needed for submodules.)

### 5.8 Output discipline

Stdout is the protocol; *every* informational message goes to
stderr via `tracing`. A stray `println!` in the wrong module
silently breaks `git fetch`. Add a lint pass (clippy
`disallowed-macros` config in `clippy.toml`) that bans
`println!`/`print!` in non-`bin` modules.

## 6. Resolved decisions and remaining open questions

**Resolved (locked in):**

- **URL grammar**: HTTPS-native with backend+transport scheme prefix
  (`s3+https://`, `az+https://`, plus loopback-only `+http`
  variants). No `profile@` userinfo; credentials via
  `?profile=` / `?credential=` / git config. No `+zip` scheme;
  zip variant via `?zip=1`. See §3.
- **Backwards compatibility**: none. Existing repos keep their
  bucket layout but users update remote URLs.
- **Binary layout**: per-role `[[bin]]` shims; one binary per
  helper scheme, single shared `git-remote-object-store` for
  management, single shared `git-lfs-object-store` for LFS.

**Resolved (additional):**

- **Git operations backend**: gitoxide (`gix`) for native Rust
  rev-parse/is-ancestor/archive/last-commit-message/remote-url/
  ref-name validation. Phase 3 ports the upstream `git.py` surface
  onto `gix` APIs; the spike result (see
  `docs/development/spike-gix-bundle-parity.md`) is that `gix` 0.82
  has no public bundle API, so `bundle`/`unbundle` retain a
  subprocess fallback funnelled through a single `run_git` helper
  that enforces the helper-protocol stdout discipline.
- **S3 multipart download**: hand-rolled ranged GETs through the
  `aws-sdk-s3` client (SigV4 still handled by the SDK). See §5.3.
- **Azure multipart download**: use `BlobClient::download()` —
  the SDK already does parallel range downloads internally.
- **Cleartext-HTTP gate**: hard-block `s3+http://` / `az+http://`
  to non-loopback hosts unless
  `GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP=1` is set. Loopback
  (`localhost`, `127.0.0.1`, `::1`) is always allowed for local
  dev (MinIO, Azurite). See §3.5.

**Phase-1 spikes (verify, not decide):**

1. `gix` bundle parity — confirm `gix` can create and consume git
   bundles compatible with `git bundle` round-trip. Fallback
   (subprocess for bundle/unbundle only) is ready if not.
2. Cargo `[[bin]]` names containing `+` — confirm cargo accepts
   `git-remote-s3+https` etc. as bin names. Fallback is hyphen
   names plus a post-install rename via `xtask`.

## 7. Tracking

Each phase becomes one or more GitHub issues with the label
`phase-N`. PRs reference both the phase and the upstream Python
file/line they are porting (`Ports git_remote_s3/remote.py:198-305`)
so reviewers can diff against the source of truth.

## References

- Upstream project: <https://github.com/awslabs/git-remote-s3>
- Git remote-helper protocol: <https://git-scm.com/docs/gitremote-helpers>
- S3 conditional writes: <https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html>
- aws-sdk-s3 client docs: <https://docs.rs/aws-sdk-s3>
- Azure SDK for Rust: <https://github.com/Azure/azure-sdk-for-rust>
- `azure_storage_blob` crate: <https://crates.io/crates/azure_storage_blob>

## Resolved Divergences (post-plan)

### #20 — s3: get_to_file If-Match guard (2026-04-26)

- **Upstream behavior**: `boto3.download_file` does not use `If-Match`
  headers; concurrent mutation between `HeadObject` and the body GET
  may produce a silently truncated or corrupted file.
- **New behavior**: Every GET in `get_to_file` carries
  `If-Match: <etag>` from the preceding `HeadObject`. If S3 returns
  412 the operation retries once, then propagates
  `Error::PreconditionFailed`.
- **Rationale**: Silent data corruption is a worse failure mode than
  a structured error. The guard adds one ETag string per request
  (negligible overhead) and catches a real race that can corrupt
  bundle files. This is a strictly defensive improvement — no
  behavioral change to the on-the-wire object layout, locking
  semantics, or LFS transfer protocol.
- **Affected sections**: Relates to §5 (S3 object-store
  implementation); the canonical record lives in this section.

### #34 — push: normalize duplicate-bundle error wire format (2026-04-26)

- **Upstream behavior**: `cmd_push` emits the under-lock duplicate-bundle
  rejection without the trailing `?` suffix
  (`../git-remote-s3/git_remote_s3/remote.py:245`), even though the
  surrounding `error <ref> "..."?` messages do include it.
- **New behavior**: Both duplicate-bundle paths in `src/protocol/push.rs`
  (pre-lock and under-lock) end with `"?\n` so the wire output is
  consistent across branches.
- **Rationale**: Git treats `error <ref> "..."?` as recoverable and
  `error <ref> "..."` as fatal. Mixing the two formats inside the same
  binary is a footgun for operators reading helper output and for
  future code that copies one branch's wording into another. The
  one-character normalization keeps the helper's user-visible error
  surface internally consistent at no behavioral cost.
- **Affected sections**: Relates to §4 (helper protocol); the
  canonical record lives in this section.

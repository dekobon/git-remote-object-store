# Using `git-remote-object-store` as a library

`git-remote-object-store` is published as a Rust library crate in
addition to the helper binaries. The full API is on
[docs.rs](https://docs.rs/git-remote-object-store); this page collects
the orientation a first-time consumer typically wants.

## When to use which entry point

- **[`Remote`](https://docs.rs/git-remote-object-store/latest/git_remote_object_store/struct.Remote.html)** —
  a typed handle that owns the parsed URL, the resolved cloud
  backend, and the repository's key prefix. Use this when you want
  the on-bucket key layout abstracted away. It exposes helper
  methods (`get_head`, `put_head`, `list`, `key`, `prefix`,
  `engine`) and also gives you the underlying store for direct
  access via `Remote::store`.
  The module-level rustdoc on `Remote` has a runnable example.

- **[`ObjectStore` trait](https://docs.rs/git-remote-object-store/latest/git_remote_object_store/object_store/trait.ObjectStore.html)** —
  the storage primitive: `get_bytes`, `get_bytes_range`, `put_bytes`,
  `put_if_absent`, `delete`, `list`, `head`. Implement it to plug in
  a new backend; consume it directly when you want to share
  S3 / Azure machinery (multipart upload, parallel ranged GET,
  `If-Match` guards, `If-None-Match: *` locking) outside the
  repository abstraction.

- **[`RemoteUrl`](https://docs.rs/git-remote-object-store/latest/git_remote_object_store/struct.RemoteUrl.html)** —
  the parser. Useful when you need to inspect or rewrite a URL
  (engine, prefix, flags) before connecting, or when implementing
  a tool that operates on `s3+https://` / `az+https://` URLs without
  ever touching the network.

## On-bucket key layout

`Remote::key(suffix)` joins `suffix` onto the repository prefix.
The conventional suffixes are:

| Suffix | Purpose |
|--------|---------|
| `HEAD` | Ref pointer for the default branch |
| `FORMAT` | Storage-engine marker (`bundle` or `packchain`) |
| `refs/heads/<branch>/<sha>.bundle` | Git bundle for a branch commit (bundle engine) |
| `refs/heads/<branch>/chain.json` | Pack-chain manifest (packchain engine) |
| `refs/heads/<branch>/LOCK#.lock` | Per-ref push-lock file |
| `refs/heads/<branch>/PROTECTED#` | Per-ref branch-protection sentinel |
| `lfs/<oid>` | Git LFS object |

Locks and protection markers are **per-ref**, not repository-wide —
each branch carries its own under its `refs/heads/<branch>/` prefix.
The exact layout depends on which storage engine the bucket uses;
see [storage-engines.md](storage-engines.md) for the full per-engine
key table.

## Async runtime

All `Remote` and `ObjectStore` methods are `async` and require a
Tokio runtime (the AWS and Azure SDKs both depend on Tokio). Drive
them from `#[tokio::main]`, `tokio::runtime::Runtime::block_on`, or
an existing Tokio task.

## Feature flags

- `test-util` — exposes `Remote::new_for_test` and the
  `MockStore` in-memory `ObjectStore`. Off by default; enable from
  `[dev-dependencies]` for downstream tests.

# Storage engines

`git-remote-object-store` ships two storage engines that decide how
git data lands on the bucket:

- **`bundle`** — one self-contained git bundle per ref per push.
- **`packchain`** — a newest-first manifest of incremental packs, with
  garbage collection and on-demand compaction.

Pick per-bucket with `?engine=<name>` in the remote URL. If the flag
is absent on a new bucket, the engine defaults to **`bundle`**. The
choice is then persisted to a `FORMAT` key at the bucket prefix on
the first push and is read on every subsequent connection.

## At a glance

| | `bundle` | `packchain` |
|---|---|---|
| Push wire cost | Full bundle of the ref every push | Incremental pack (only new objects) |
| Fetch wire cost (update) | Full bundle | New segment(s) only — walks chain until a known ancestor is found |
| Storage growth | Bounded per ref (old bundle deleted on each push) | Grows per push until `compact` rewrites the chain; old packs released by `gc` |
| Operational overhead | None | Schedule `gc` (weekly is fine); optionally `compact` |
| Direct single-file read | Not supported | Supported via `packchain::read_blob` |
| Accelerated clone (`bundle-uri`) | Not supported (bundle filename rotates per push) | Supported with `?bundle_uri=1` |
| Cross-push pack dedup | No — each bundle is self-contained | Yes when packs are byte-identical — they share a key under `<prefix>/packs/<sha>.pack` |
| `git bundle verify` of stored artefacts | Yes | Yes, on the baseline bundle only |

LFS, branch protection, per-ref push locking, branch deletion, and the
`doctor` management command work identically on both engines.

## Choosing an engine

Pick **`bundle`** when:

- The repository is archival or rarely updated.
- You want zero operational surface — no GC, no compaction, no
  grace-window tuning.
- Clones are infrequent or the repo is small enough that a full
  bundle download per ref is cheap.
- You want every stored artefact to be inspectable with stock
  `git bundle verify` / `git bundle list-heads`.

Pick **`packchain`** when:

- The repository is actively pushed to by a team and you care about
  push/fetch latency.
- You want incremental fetches — clones after the first one download
  only new packs.
- You want to read individual files at the tip without cloning
  (`packchain::read_blob`).
- You can run a weekly `gc` job (cron, GitHub Actions, or similar).
- You want `git clone` to fetch the baseline bundle in parallel with
  the helper-protocol negotiation (`?bundle_uri=1`).

## How `bundle` works

On every push, the helper builds a git bundle v2 file containing the
ref and uploads it to `<prefix>/refs/heads/<branch>/<sha>.bundle`,
where `<sha>` is the tip OID. The previous bundle for the same ref is
deleted in the same per-ref lock window. Force-pushes follow the same
flow: a new bundle is uploaded and the loser is removed.

On-bucket layout:

| Key | Purpose |
|-----|---------|
| `<prefix>/FORMAT` | Engine marker (`bundle`) |
| `<prefix>/HEAD` | Default-branch pointer |
| `<prefix>/refs/heads/<branch>/<sha>.bundle` | Bundle for the tip commit |
| `<prefix>/refs/heads/<branch>/LOCK#.lock` | Per-ref push lock |
| `<prefix>/refs/heads/<branch>/PROTECTED#` | Branch-protection sentinel (optional) |
| `<prefix>/refs/heads/<branch>/repo.zip` | Source archive (when `?zip=1` is set) |
| `<prefix>/lfs/<oid>` | Git LFS objects (when LFS is in use) |

Fetches download the bundle for the requested ref and unbundle it
locally. There is no incremental fetch — every fetch transfers the
full bundle. This is the simplest model the project ships; if you do
not have a specific reason to pick `packchain`, this is the right
default.

## How `packchain` works

A `chain.json` manifest at `<prefix>/refs/heads/<branch>/chain.json`
records a newest-first list of pack segments. The first push uploads
a baseline bundle plus a single-segment chain; each subsequent push
builds an incremental pack containing only the new objects, uploads
it under `<prefix>/packs/<content-sha>.pack` (plus its `.idx`), and
prepends a new segment to `chain.json`. Pack files are
content-addressed, so two pushes that produce byte-identical packs
deduplicate naturally — including across branches.

Fetches walk segments newest-to-oldest, stop at the first segment
whose tip is already present locally, and download only the segments
above that boundary (in parallel). A first-time clone walks all the
way to the baseline bundle.

On-bucket layout:

| Key | Purpose |
|-----|---------|
| `<prefix>/FORMAT` | Engine marker (`packchain`) |
| `<prefix>/HEAD` | Default-branch pointer |
| `<prefix>/refs/heads/<branch>/chain.json` | Newest-first segment manifest |
| `<prefix>/refs/heads/<branch>/<full_at>.bundle` | Baseline bundle, named by the SHA that `chain.full_at` points at |
| `<prefix>/refs/heads/<branch>/path-index.json` | `path → blob OID` map at the tip (powers `read_blob`) |
| `<prefix>/refs/heads/<branch>/LOCK#.lock` | Per-ref push lock |
| `<prefix>/refs/heads/<branch>/PROTECTED#` | Branch-protection sentinel (optional) |
| `<prefix>/packs/<sha>.pack` + `.idx` | Content-addressed pack segments |
| `<prefix>/gc/tombstones-<run-id>-<timestamp>.json` | GC mark-phase records |
| `<prefix>/lfs/<oid>` | Git LFS objects (when LFS is in use) |

### Garbage collection

Force-pushes, branch deletions, and compactions detach packs from the
chain. `git-remote-object-store gc` removes them through a two-phase
mark-and-sweep with a grace window (default 24 hours; override with
`--grace-hours` or the `GIT_REMOTE_OBJECT_STORE_GC_GRACE_HOURS` env
var). The grace window protects in-flight readers — a clone that
started before the mark phase is allowed to finish even if `gc`
decided the pack was orphan.

A single weekly invocation is the standard schedule: each run sweeps
the previous run's tombstones and writes new ones. See
[Getting started §9](getting-started.md#9-garbage-collection) for
cron and GitHub Actions examples.

### Compaction

Long chains slow down fetches. `git-remote-object-store compact`
rewrites a chain into a single root segment in place. The heuristic
recommends compaction at roughly 20 segments or 100 MiB of cumulative
segment bytes per ref, whichever comes first; `--force` overrides the
heuristic. The compaction holds the per-ref lock for the duration of
the rewrite; after compaction, the orphaned segment packs become GC
candidates.

### Direct single-file reads

The library exposes `packchain::read_blob(&remote, ref, path, &cache)`
for fetching a single file at a ref's tip without cloning. The lookup
goes through `path-index.json`, then walks pack indices
newest-first. The caller-supplied `PackIndexCache` (a byte-bounded
LRU, 64 MiB by default) amortises index downloads across many
`read_blob` calls; on a `bundle`-engine remote the function returns
`PackchainError::WrongEngine` instead.

### Accelerated clones (`bundle-uri`)

`bundle-uri` is a git protocol capability that lets the server tell
`git clone` "fetch this baseline pack directly from <URL>" — git
downloads it from object storage (or a CDN) in parallel with the
helper-protocol negotiation, skipping the chain walk. With
`?bundle_uri=1` set, the helper advertises the baseline pack's URL
per ref; the creation token is the chain's `full_at` SHA, so a cached
baseline is reused across clones until the next force-push or
`compact`. See [Getting started §10](getting-started.md#10-bundle-uri--faster-git-clone-for-large-repos)
for when to enable it, public-bucket vs private-bucket setup, and
the security trade-offs of emitting presigned URLs.

## Switching engines

The `FORMAT` key is written once on first push and read-only after
that. There is no in-place migration:

- A bucket that already has `FORMAT=bundle` cannot be converted to
  `packchain` (or vice versa) without manually creating a new bucket
  prefix and pushing into it.
- Connecting with `?engine=packchain` to a `FORMAT=bundle` bucket
  fails with `BackendError::EngineMismatch` before any data is
  written, and the same in reverse.

To migrate, create a new remote URL with the desired `?engine=`,
push every branch into it, then update consumers.

## Caveats

- **Default may surprise large-repo users.** Active monorepos
  typically want `packchain`; the default of `bundle` optimises for
  zero ops, not throughput. Set `?engine=packchain` explicitly on
  the first push if you know you want it.
- **`?bundle_uri=1` has no effect on `bundle`-engine remotes**;
  there is no stable per-ref URL to advertise. The helper silently
  omits the `bundle.<ref>.uri` lines.
- **`gc` and `compact` are effectively no-ops on `bundle`-engine
  remotes.** `bundle` buckets have no `packs/` namespace and no
  `chain.json` files, so the mark phase finds nothing to tombstone
  and `compact` finds nothing to rewrite. Running them on a
  mixed-engine inventory is safe.
- **Engine choice is per-bucket-prefix, not per-ref.** Different
  prefixes of the same bucket can hold different engines, but all
  refs under one prefix share one engine.

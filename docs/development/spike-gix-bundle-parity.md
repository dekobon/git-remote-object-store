# Spike: `gix` ↔ `git` bundle parity

**Phase:** 3 (`gix` (gitoxide) wrapper)
**Date:** 2026-04-25
**Status:** resolved — native implementation in `src/bundle.rs` via
`gix-pack 0.69`.

## Question

Can [`gix`][gix] 0.82 create and consume git bundle files (`git bundle
create FILE REF` / `git bundle unbundle FILE REF`) so that the upstream
subprocess can be dropped from the Rust port?

## Method

1. Read the public `gix` 0.82 API surface
   ([docs.rs/gix/0.82.0][gix]) — searched for any `bundle`, `Bundle`,
   `pack-bundle`, or `gix-bundle` references.
2. Read the relevant sub-crates: `gix-pack`, `gix-protocol`, `gix-odb`,
   `gix-features`. None expose a bundle reader/writer.
3. Searched the gitoxide issue tracker for open issues or PRs that
   would land bundle support. None found; the closest hit is the
   long-open #104 about `pack-receive`, which is unrelated.

## Initial result (2026-04-25)

`gix` 0.82 had **no public API for creating or consuming git bundle
files**. Decision at the time: keep a subprocess fallback for
`bundle()` and `unbundle()` only.

## Resolution (2026-04-28)

`gix-pack 0.69` (already a transitive dependency through `gix 0.82`)
exposes the two primitives needed to implement the bundle v2 format
natively:

- **`gix_pack::Bundle::write_to_directory`** — receives a raw PACK byte
  stream and writes an indexed `.pack`/`.idx` pair into a directory.
  Used by the unbundle path: parse the git bundle text header, seek to
  the PACK data, and pass the remaining bytes into this function.
- **`gix_pack::data::output` pipeline** — three-stage:
  `count::objects` → `entry::iter_from_counts` →
  `bytes::FromEntriesIter`. Using `ObjectExpansion::TreeContents`,
  passing only commit IDs causes automatic per-commit expansion to trees
  and blobs. Used by the bundle-create path.

The implementation lives in `src/bundle.rs`. The `run_git` subprocess
helper was removed along with `GitError::GitBinaryMissing` and
`GitError::Subprocess`. See `CHANGELOG.md` for the full entry.

[gix]: https://docs.rs/gix/0.82.0/gix/

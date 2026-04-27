# Spike: `gix` ↔ `git` bundle parity

**Phase:** 3 (`gix` (gitoxide) wrapper)
**Date:** 2026-04-25
**Status:** resolved — keep subprocess fallback for `bundle`/`unbundle` only.

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

## Result

`gix` 0.82 has **no public API for creating or consuming git bundle
files**. The bundle wire format is documented in
[git's `bundle-format.txt`][fmt] and could in principle be implemented
on top of the existing `gix-pack` writer/reader, but no such layer
exists today.

Decision: keep a subprocess fallback for `bundle()` and `unbundle()`
only. All other helpers (`rev_parse`, `is_ancestor`, `archive`,
`is_valid_ref_name`, `last_commit_message`, `remote_url`) go through
`gix` natively.

The fallback is implemented through a single private helper,
`run_git()`, that hard-codes `Stdio::null` for stdin and `Stdio::piped`
for both stdout and stderr — protecting the helper-protocol stdout
discipline mandated by `.claude/rules/protocol-stdout.md`. `run_git()`
is the only place in the crate that spawns `git`.

## Re-evaluation triggers

- `gix` ships a public bundle reader/writer (likely as a new
  `gix-bundle` sub-crate, or under `gix::bundle::*`). Watch the
  gitoxide CHANGELOG and the `bundle` label on the issue tracker.
- A standalone `gix-bundle` crate appears with a stable API.

When either triggers, replace the `bundle`/`unbundle` body to call the
new API and drop the `run_git` helper if no other site uses it.

[gix]: https://docs.rs/gix/0.82.0/gix/
[fmt]: https://git-scm.com/docs/bundle-format

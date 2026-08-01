---
globs: "**/*.rs"
---

## Bucket Keys and Destructive Writes

Rules for any code path that builds a bucket key or mutates bucket
state. Each was extracted from a shipped bug; the incidents are in
`docs/development/lessons_learned.md`.

### Key construction goes through `crate::keys`

Never open-code a `<prefix>/<suffix>` join, a trailing-slash
normalisation, or a marker-key literal at a call site. `crate::keys`
owns one canonical implementation of each — route every site through
it, even when the helper looks like trivial one-line glue. The
empty-prefix (root-of-bucket) case is the one open-coded joins get
wrong, and they get it wrong at a different subset of sites each time.

### Marker existence is `head(exact_key)`, never `list(prefix)`

`ObjectStore::list(prefix)` is a **byte-prefix scan**, not a
path-segment match: `list("a/PROTECTED#")` also matches a future
`PROTECTED#v2` or `PROTECTED#audit`. For a singleton marker whose
question is "does this exact key exist":

- **Existence**: `store.head(exact_key)`, mapping `NotFound` to `false`.
- **Segment test**: byte-equality on the final path segment (e.g.
  `keys::is_protected_marker_segment`) — never `starts_with`, never
  `contains`.

That today's layout writes exactly one literal under the segment is not
a defence; it makes the site correct by accident until the marker
becomes a family. `list()` for an existence check also spends a
needless `ListObjectsV2` / `ListBlobs` round-trip.

### Name the durable commit, then order the work around it

Every protocol entry point has one write whose success defines "the
operation happened" — the bundle `put_path`, the `chain.json` commit.
Identify that line first; it partitions the function.

**Before it** — when a logical commit spans several objects, write the
invariant-holder **last** and document the ordering at the call site.
Then walk the read paths: every observable in-flight state must be
either invisible (atomically overwritten) or surfaced as a **typed
transient** error that callers can distinguish from genuine corruption.
Ordering without the typed error leaves readers unable to tell
"crashed mid-write" from "corrupt"; the typed error without the
ordering misclassifies corruption as transient.

**After it** — everything is best-effort: prior-bundle deletes,
baseline cleanup, optional artifacts, observability writes. A `?` past
the durable commit reports "push failed" to the operator while the git
data is already live on the backend. Swallow those errors with a
`warn!` naming the ref path and the orphan key, and return success to
the protocol. Audit every `?` that follows the durable-commit line.

### Re-verify under the lock, immediately before the destructive write

A LIST or HEAD that justifies a delete, overwrite, or marker-put
expires the moment anything happens between it and the write — an
operator prompt, a loop iteration over a snapshot, a lock release,
another client's lock window. Re-read under the per-ref lock, scoped to
the exact key the next mutation touches, with nothing in between, and
surface any divergence from the snapshot rather than papering over it.

When reviewing a snapshot-driven flow, find the LIST/HEAD that drives
the decision and trace forward to the destructive call. If the path
crosses an interactive prompt, a lock release, or a loop boundary, the
invariant has expired and the flow is a TOCTOU bug.

### One flag per named safety

When adding a safety check to a path that already has a `--force`-style
flag, run the new check **unconditionally**. If it genuinely needs a
bypass, give the bypass its own named flag (`--skip-live-recheck`) —
not a reuse of `--force`. Reviewing an existing `!force` guard: does
the flag's *name* tell the operator they are bypassing that specific
check? If not, the guard is wrong.

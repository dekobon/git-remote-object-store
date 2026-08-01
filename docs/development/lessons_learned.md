# Lessons Learned

Hard-won lessons from building `git-remote-object-store`.

Quality bar: each entry must describe a problem that was **genuinely
hard** (cost real debugging time or caused a real bug) **and** is
**likely to recur**. Err on the side of leaving entries out. This is not
a changelog or a diary — that role belongs to `CHANGELOG.md`.

## How this file relates to `.claude/rules/`

The *prescriptions* distilled from these incidents live in
`.claude/rules/`, which is loaded every session. This file holds the
*evidence*: what the bug looked like, why the wrong shape looked right,
and which issues it took to find it. Keep the prescription in one place
— restating it here guarantees the two copies drift.

Most entries close with a `**Rule**:` line naming the section that
carries the prescription. An entry without one is a lesson too
situational to become a standing rule; it keeps its own `**Lesson**:`
paragraph instead.

Cite entries **by title**, never by position. Entries get merged,
reordered, and rewritten; the numbered citations that used to appear in
`src/` and `spec/` had already gone stale by the time they were audited.

Use the `/lessons-learned` skill to review project activity (issues,
commits, CHANGELOG) and draft new entries against this bar.

---

## Trait defaults silently regress per-impl invariants

When a trait method carries an invariant beyond its signature
(streaming, bounded memory, atomicity, progress reporting), a default
implementation that satisfies the *signature* without satisfying the
*invariant* is a trap. New impls inherit the default and the invariant
is silently lost. The type system cannot enforce "this method must not
buffer the body in memory"; only the per-impl override can. When some
backends override and others do not, parity gaps appear that compile,
pass small-fixture tests, and manifest only under production-scale
input.

**Azure `put_path` regressed the streaming guarantee from #21** (#42,
90739f5). #21 added `ObjectStore::put_path` specifically to avoid an
N-byte working-set spike on bundle and LFS uploads. `S3Store` overrode
it with `ByteStream::from_path`; `AzureStore` did not, so it inherited
the trait default `tokio::fs::read(src) → put_bytes`. A 5 GiB LFS push
to Azure allocated 5 GiB in the helper process while the same push to S3
streamed in chunks. The bug shipped, was filed, and required a separate
fix per backend.

**Rule**: "Trait Defaults and Non-Signature Invariants" in
`.claude/rules/rust.md`.

---

## Pool-idle timeouts do not bound hot connections

`pool_idle_timeout` retires connections that have been *idle* for the
configured window. A connection used within the window never goes idle,
so when its peer rotates DNS or the load balancer kills the VIP, that
hot pooled connection wedges until the OS-level TCP retransmit timeout
(~15 minutes on Linux defaults). The bound that matters for
"hot connection to a dead peer" is a layer-7 read or connect timeout —
and SDK semantics differ: smithy's `read_timeout` is time-to-first-byte
(it does not limit body transfer), while reqwest's is per-read (it
resets after each successful read, bounding stuck transfers without
capping total size). TCP keepalive helps but is not always exposed by
SDK builders.

**Three rounds of fixes for DNS-rotation hangs in long LFS sessions**
(#26, #27, #28; 5bd303f, 073e474, 218eff1). #27 and #28 added
`pool_idle_timeout(30s)` and `tcp_keepalive(30s)` where exposed.
Production traces still showed wedged sessions, because a
continuously-used connection never goes idle. The actual fix was
5bd303f: `read_timeout(30s)` plus `connect_timeout(10s)` (Azure), so a
stuck hot connection fails fast and the SDK's retry layer picks a fresh
socket.

**Lesson**: when wiring a new HTTP SDK, configure all four bounds
deliberately — `pool_idle_timeout`, `tcp_keepalive`, `connect_timeout`,
`read_timeout` — and verify that SDK's `read_timeout` semantics (TTFB
vs. per-read) before relying on it. Pool-idle alone is never enough for
a long-lived process; assume hot connections will outlive their peers.
The per-backend consequences are documented at the read sites: see the
transport section of the `object_store::s3` module docs and
`S3Store::put_body`'s override.

---

## Guards and transforms belong at the chokepoint, not at each call site

When a structural condition affects how every call site builds a key, or
when a recursive routine has more than one re-entry edge, open-coding
the check at each site guarantees it will be wrong at some of them. The
bug surface scales with the number of sites and so does the fix surface.
A bound that lives in only one of several re-entry paths is not a bound
at all: the sibling edge consumes none of the budget. The right shape is
one canonical implementation at the point every path funnels through —
even when that implementation looks like trivial one-line glue.

**Empty-prefix root-of-bucket keys were wrong at every site that joined
`<prefix>/<suffix>`** (#29, #32; 05ea704, 194dd55, plus a separate
`read_remote_head` `Some("")` fix). The pattern
`match prefix { Some(p) if !p.is_empty() => format!("{p}/{suffix}"), _ => suffix.to_owned() }`
appeared in `push.rs`, `fetch.rs`, `list.rs`, `lfs/agent.rs`, and
several management call sites. Each was patched as the bug surfaced,
across multiple issues, until 194dd55 consolidated the join into
`crate::keys`.

**`OFS_DELTA` chains bypassed `MAX_DELTA_DEPTH`** (#83, 9383408). The
depth counter was checked-and-incremented in `read_object_from_chain`.
The `REF_DELTA` branch re-entered through that function and stayed
bounded; the `OFS_DELTA` branch called `decode_entry` directly with the
same mutable counter and never checked it, so a long pure-`OFS_DELTA`
chain in a malformed pack could stack-overflow the reader. The fix moved
the guard to the top of `decode_entry` — the one chokepoint every
recursive resolution path traverses — so both delta forms share a
budget.

**Lesson**: write the helper before the third copy, and put recursion
bounds at the dispatcher rather than at any one caller of it. The
unit-test surface collapses from N sites to one, and the next
edge-case-at-the-boundary bug becomes a one-line fix instead of
whack-a-mole. The delta-decoder shape generalises to any tree walker,
interpreter, or expression evaluator with more than one recursive edge.

**Rule**: "Key construction goes through `crate::keys`" in
`.claude/rules/object-store-writes.md`; "Recursion and Cycle Bounds" in
`.claude/rules/rust.md`.

---

## `list()` is a byte-prefix scan, not a path-segment match

Object-store `list(prefix)` returns every key whose *bytes* start with
the prefix — including a hypothetical future `PROTECTED#v2`,
`PROTECTED#audit`, or any sibling artefact sharing those bytes.
Likewise, `last_segment.starts_with(MARKER)` and `key.contains(MARKER)`
are not equivalent to `last_segment == MARKER`. A layout in which
exactly one literal is ever written under the segment makes all three
shapes correct **by accident**, and they flip to wrong the moment the
marker becomes a family.

**Three sites used substring, prefix, or LIST for `PROTECTED#`** (#94,
81028d0; #111, e8fa6c4; #119, 2ed0c1e). `is_protected` in
`src/protocol/push.rs` used `store.list()` to test for the marker — a
byte-prefix match plus a needless `ListObjectsV2` / `ListBlobs`
round-trip on every protected-push attempt. `delete_remote_ref` used a
substring `contains(PROTECTED_MARKER_SEGMENT)`.
`snapshot::push_into_snapshot` used `last.starts_with(...)`. Each was a
future-schema trap that would have silently flipped protection on for
unrelated `PROTECTED#`-prefixed keys. The fixes consolidated onto
`keys::is_protected_marker_segment` and `store.head(exact_key)`.

**Rule**: "Marker existence is `head(exact_key)`, never `list(prefix)`"
in `.claude/rules/object-store-writes.md`.

---

## Name the durable commit; order the work around it

Every protocol entry point has one write whose success defines "the
operation happened." That line partitions the function, and both halves
have their own failure mode. *Before* it, when a logical commit spans
several objects, the durable-write order determines what concurrent and
post-crash readers observe. *After* it, the protocol's contract is
already met, so any `?` that still propagates reports failure to the
operator while the git data is live on the backend.

**`path-index.json` could become newer than `chain.json` across a crash
window** (#114, 7a480e0). Packchain push wrote `path-index.json` before
`chain.json`. A crash between the two left the new path-index pointing
at a tip in the *new* tree while the chain manifest still listed only
the *old* segments. `read_blob` resolved a blob SHA from the new
path-index, failed to find it in the segments named by the old chain,
and surfaced `BlobNotInChain` — indistinguishable from genuine
corruption. The fix flips the order (`chain.json` first) so a mid-flight
crash leaves a state the reader can recognise, and maps that state to a
new typed `TransientChainPathIndexMismatch`.

**Three propagated-error bugs one frame past the durable commit** (#113,
b816ae8; #121, 58d0ed1; #127, dcce5e2). All three sit inside
`perform_push_under_lock` or `compact_under_lock`, after the bundle /
`chain.json` upload: compact's prior-baseline `delete_idempotent`,
force-push's old-bundle `delete_idempotent`, and the optional `?zip=1`
CodePipeline artifact `put_path`. Each propagated a transient store
error as a push failure. The same shape recurred twice more after the
first fix — #121 was found reviewing the #113 fix, #127 three frames
over — because the fix was applied to the line rather than to the
partition.

**Lesson**: the second and third occurrences are the real lesson. When a
bug is "an error propagates where it should not," the fix is to identify
the durable-commit line and audit *every* `?` after it, not to patch the
one that was reported.

**Rule**: "Name the durable commit, then order the work around it" in
`.claude/rules/object-store-writes.md`.

---

## A snapshot that justified a destructive write expires immediately

When a path lists or snapshots remote state at T1 and destroys something
at T2, everything in the elapsed window invalidates the invariant the
original check confirmed. The protection marker that was absent at T1
was written at T1+1s; the lock that was "stale" at T1 was reclaimed at
T1+30s; the branch that existed at T1 was deleted at T1+5s. Separately,
a `--force` flag accumulates meaning: each safety check later added to
the same path either tests `!force` and gets bundled under a switch
whose name never mentioned it, or runs unconditionally and stays a
safety.

**Nine TOCTOU bugs filed in one batch-fix wave** (issues
`#128–#132, #137–#140`; commits a0ed694, 8836fd6, 27cd6d4, 3bc2ba6,
5539cbd, e9f20c6, 9759015, a720cce, 808adbe, 50d9a8b). Several were
labelled `security`. Representative shapes:

- **`delete-branch` listed, prompted the operator, then deleted from the
  stale list** (#131, #139). A concurrent `protect` across the prompt
  left the `PROTECTED#` marker untouched while the bundles were
  destroyed; a concurrent push added bundle keys the deletion loop never
  saw, so the branch survived the "delete" holding the new content.
- **`doctor fix_head` pointed HEAD at a branch deleted across the
  prompt** (#138) — producing exactly the invalid-HEAD condition it
  existed to repair.
- **`doctor` deleted a stale lock that had since been reclaimed** (#132),
  breaking mutual exclusion for the client that now held it.
- **`gc sweep` reused one referenced-set across every tombstone** (#140).
  A concurrent push committing a `chain.json` that referenced a
  tombstoned pack was invisible to later iterations, and sweep deleted a
  live pack — permanent chain corruption.

**`gc sweep --force` deleted live packs from stale tombstones** (#117,
1baa452). `--force` was introduced to skip the grace-window wait. Later,
`sweep()` grew a live-pack re-check against a fresh
`list_referenced_packs()`, guarded by `if !opts.force && ...` — folding
"skip grace" and "skip the live re-check" under one flag. A stale
tombstone plus `--force` could then delete pack objects a committed
`chain.json` still referenced.

**Lesson**: both failures read the same way in a review — "the safety
check ran, but the destructive action no longer satisfies the safety it
claimed to satisfy." One is temporal, one is flag scope. Trace the
driving read forward to the write; trace the flag's name against each
guard it gates.

**Rule**: "Re-verify under the lock" and "One flag per named safety" in
`.claude/rules/object-store-writes.md`.

---

## A test that agrees with the code is not an oracle

A green suite is evidence only if the assertion could have failed. Four
distinct ways it silently cannot, each caught here the hard way — by
audit, or by an older suite whose assertions pre-dated the change.

**Loose assertion form: the `?` suffix dropped from the under-lock
duplicate-bundle error** (#34; 84b1811, c251914). The under-lock branch
emitted `error <ref> "multiple bundles ..."` without the trailing `?`
that every other `error <ref>` line in the helper carries. The test
`pre_lock_multi_bundle_rejection_surfaces_unchanged` asserted
`contains("multiple bundles")` and passed cleanly. Found by audit, not
by the suite.

**Expected value taken from the code: the same bug would have survived a
byte-exact assertion** (#34, c251914). Had that test compared the full
string, the expected string would still have been copied from the code's
actual output — already missing the `?`. The comparison would have
agreed with itself. Contrast the protocol-smoke `list` filter
`^refs/.+/.+/[a-f0-9]{40}\.bundle$`, derived from the on-bucket layout
rules rather than from running the code, which is why it catches
sibling-prefix collisions the implementation shape alone would not
surface.

**Structurally vacuous input: the shallow-boundary depth-reset test**
(#50, d32741c). `depth_resets_between_batches` sent its second fetch
batch at an orphan (root) commit. `shallow_boundaries` returns `[]` for
a parentless commit at *any* depth, so the assertion was unconditionally
true and the test passed even when the depth option did leak between
batches. The fix chose a target that has parents.

**Oracle flipped in the same commit** (#205, c5468b4 reverted; #208,
7dfa5dc). c5468b4 added a tombstone-defer to
`delete_remote_ref_under_lock` and, in the same commit, renamed
`delete_remote_ref_removes_single_bundle` and flipped its assertion from
`!store.contains(<bundle>)` to `store.contains(<bundle>)`, plus matching
flips on the integration and lock-release tests. The unit suite went
green. 7dfa5dc is the subtler variant: it *added* a test pinning
`Some(0)` → env-or-default for `doctor --lock-ttl-seconds 0`, and that
newly-pinned contract was itself the regression. In both cases the
shellspec suite caught it, because its assertions were authored before
the commit under test.

**Lesson**: the two catches came from oracles that pre-dated the change
— an audit pass and an older suite. Neither was the unit test written
alongside the code. Ask of any diff that touches production and test
together: "would this test have failed before this commit?"

**Rule**: "A Test That Agrees With the Code Is Not an Oracle" in
`.claude/rules/testing.md`.

---

## The suite you ran is not the suite you think

Three ways a green run covers less than it appears to: a guard that
fails to fire, a harness missing production glue, and a file that
compiles to nothing.

**A `Skip if` guard silently did not fire** (spec/integration, alongside
the CHANGELOG "Shellspec integration suites" entry).
`Skip if "aws-cli not on PATH" ! command -v aws >/dev/null 2>&1` ran the
spec on a host without `aws`, which then failed inside
`rustfs_make_bucket` with `aws: command not found` — several layers from
the cause. Shellspec parses the condition through its DSL preprocessor,
not bash: a leading `!` is folded into the command name and redirections
are mangled. Dropping the redirection aborted shellspec entirely
(`[reporter: 101]`). The fix is predicate functions in
`spec/spec_helper.sh`.

**The harness skipped an install step the README documents**
(spec/spec_helper.sh symlink shim). `cargo build` produces hyphenated
binaries (`git-remote-s3-http`), but git, given `s3+http://…`, looks up
the literal `git-remote-s3+http` on PATH. The integration suite hit
`git: 'remote-s3+http' is not a git command` because the harness skipped
the one-time symlink loop the README already tells end users to run.
`spec_helper.sh` now creates the `+`-form symlinks in a per-run temp dir
and prepends it to PATH.

**Feature-gated test files ran only via cross-package feature
unification**. Six integration files under `tests/` open with
`#![cfg(feature = "test-util")]`; when the feature is off they compile
as empty translation units, and every identifier resolution, visibility
check (E0603), and type error inside is skipped silently. `test-util` is
not a default feature — yet a workspace-root `cargo test` runs them,
because `cli/Cargo.toml` dev-depends on the lib with
`features = ["test-util"]` and `default-members` includes `cli`, so
resolver-v2 unification turns the gate on. Verified before the fix:

```text
cargo test --test protocol_smoke -- --list                        → 32 tests
cargo test -p git-remote-object-store --test protocol_smoke ...   →  0 tests
```

Nothing in the test targets named the feature they depended on; the
guarantee lived in a dev-dependency line in a *different* package. The
targets now pass `--features git-remote-object-store/test-util`
explicitly.

**What kept this from being worse**: `tests/url_parsing.rs` imports
`test_util::EnvGuard` **ungated**, so dropping the feature is a hard
`E0432` rather than a silent loss of coverage. That is luck, not design
— it is one unrelated file, and it reports the problem as a broken build
in a file that has nothing to do with the six that went quiet.

**Lesson**: the shared failure mode is that all three report success —
or, in the third case, report a failure pointing somewhere else. When a
suite's coverage depends on something outside the test files — a guard,
a PATH entry, a feature flag — name that dependency in the invocation
rather than inheriting it from the environment or the resolver.

**Rule**: "The Harness Must Replicate the Production Install Glue" and
"Feature-Gated Integration Tests" in `.claude/rules/testing.md`;
"Shellspec `Skip if` conditions" in `.claude/rules/bash.md`.

---

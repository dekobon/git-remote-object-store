# Lessons Learned

Hard-won lessons from building `git-remote-object-store`.

Quality bar: each entry must describe a problem that was **genuinely hard**
(cost real debugging time or caused a real bug) **and** is **likely to
recur**. Err on the side of leaving entries out. This is not a changelog
or a diary — that role belongs to `CHANGELOG.md`.

Use the `/lessons-learned` skill to review project activity (issues,
commits, CHANGELOG) and draft new entries against this bar.

---

<!-- Numbered lessons begin here. Append new entries at the end with the
     next sequential number. Format:

## N. Pithy Principle Name

One-paragraph statement of the general lesson (not issue-specific).

**Description of specific instance** (#NN, abc1234). Concrete details.

**Lesson**: actionable takeaway in one or two sentences.

---
-->

## 1. Trait defaults silently regress per-impl invariants

When a trait method carries a non-functional invariant (streaming, bounded
memory, atomicity, progress reporting), a default implementation that
satisfies the *signature* without satisfying the *invariant* is a trap:
new impls inherit the default and the invariant is silently lost. The
type system cannot enforce "this method must not buffer the body in
memory"; only the per-impl override does. When some backends override
and others rely on the default, parity gaps appear that compile, pass
small-fixture tests, and only manifest under production-scale inputs.

**Azure `put_path` regressed the streaming guarantee from #21** (#42,
90739f5). #21 added `ObjectStore::put_path` specifically to
avoid an N-byte working-set spike on bundle and LFS uploads. `S3Store`
overrode it with `ByteStream::from_path`; `AzureStore` did not, so it
inherited the trait default `tokio::fs::read(src) → put_bytes`. A 5 GiB
LFS push to Azure allocated 5 GiB in the helper process while the same
push to S3 streamed in chunks. The bug shipped, was filed, and required
a separate fix per backend.

**Lesson**: For trait methods that carry an invariant beyond the
signature (streaming, atomicity, progress, conditional semantics),
either omit the default and force every impl to provide one, or write
the default to *fail loudly* (e.g. return `Unsupported`) rather than
silently degrade. If a convenience default must exist, document the
invariant it does *not* preserve in the trait doc comment, and keep a
checklist of impls that need an explicit override.

---

## 2. HTTP pool-idle timeouts do not bound hot connections

`pool_idle_timeout` retires connections that have been *idle* for the
configured window. A connection used within the window never goes idle,
so when its peer rotates DNS or the load balancer kills the VIP, that
hot pooled connection wedges until the OS-level TCP retransmit timeout
(~15 minutes on Linux defaults). The bound that matters for the
"hot connection to a dead peer" failure mode is a layer-7 read or
connect timeout — and the SDK semantics differ: smithy's `read_timeout`
is time-to-first-byte (does not limit body transfer), while reqwest's
`read_timeout` is per-read (resets after each successful read, bounding
stuck transfers without limiting total size). TCP keepalive helps but
isn't always exposed by SDK builders (e.g. `aws-smithy-http-client`
1.1.12).

**Three rounds of fixes for DNS-rotation hangs in long LFS sessions**
(#26, #27, #28, 5bd303f, 073e474, 218eff1). #27 and #28 added
`pool_idle_timeout(30s)` (and `tcp_keepalive(30s)` where exposed).
Production traces still showed wedged sessions because a continuously-
used connection never went idle. The actual fix was 5bd303f, which
applied `read_timeout(30s)` plus `connect_timeout(10s)` (Azure) so a
stuck hot connection fails fast and the SDK's retry layer picks a fresh
socket.

**Lesson**: When wiring a new HTTP SDK, configure all four bounds
deliberately — `pool_idle_timeout`, `tcp_keepalive`, `connect_timeout`,
and `read_timeout` — and verify the SDK-specific semantics of
`read_timeout` (TTFB vs. per-read). Pool-idle alone is not enough for
long-lived process designs; assume hot connections will outlive their
peers.

---

## 3. Cross-cutting key/path transforms belong in one helper

When a structural condition affects how every call site builds a key or
path (empty-prefix join, trailing-slash normalization, percent-encoding
of reserved characters), open-coding the transform at each call site
guarantees it will be wrong at some of them. The bug surface scales
with the number of sites; the fix surface scales the same way. The
right shape is a single helper with one canonical implementation, used
everywhere — even when the helper looks like trivial one-line glue.

**Empty-prefix root-of-bucket keys were wrong at every call site that
joined `<prefix>/<suffix>`** (#29, #32, 05ea704, 194dd55, plus a
separate `read_remote_head` `Some("")` fix). The pattern
`match prefix { Some(p) if !p.is_empty() => format!("{p}/{suffix}"), _ => suffix.to_owned() }`
appeared in `push.rs`, `fetch.rs`, `list.rs`, `lfs/agent.rs`, and
several management call sites. Each site had to be patched as the bug
surfaced, across multiple issues. 194dd55 finally consolidated the
join into a `crate::keys` module.

**Lesson**: If a structural condition touches more than one or two call
sites, write the helper before the third copy. The unit-test surface
collapses from N sites to one, and the next "edge case at the boundary"
bug becomes a one-line fix instead of a whack-a-mole.

---

## 4. Pin exact wire bytes in protocol tests, not substrings

`assert!(output.contains("multiple bundles"))` against a line-based
protocol passes for any line that contains the substring — including
lines that drop a trailing punctuation byte the wire format requires.
The git-remote-helper protocol and the LFS JSON protocol are byte-exact
contracts; tests that match loosely on protocol output let
wire-incompatible regressions through. The same anti-pattern shows up
under other names (matching log strings, accepting any nonzero exit
code, `is_ok()` on a `Result<T, E>` whose error variant matters), and
it consistently produces tests that pass while the system is broken.

**`?` suffix dropped from under-lock duplicate-bundle error** (#34,
84b1811, c251914). The under-lock branch of duplicate-bundle rejection
emitted `error <ref> "multiple bundles ..."` without the trailing `?`
used by every other `error <ref>` line in the helper. The existing test
`pre_lock_multi_bundle_rejection_surfaces_unchanged` used a
`contains("multiple bundles")` assertion and passed cleanly. The bug
was found by audit, not by the test suite. The strengthened test now
pins the byte-exact line.

**Shallow-boundary depth-reset test used a root commit that made the
test vacuous** (#50, d32741c). The `depth_resets_between_batches`
integration test sent a second fetch batch targeting an orphan (root)
commit — a commit with no parents. `shallow_boundaries` on a root
commit always returns `[]` for any depth value because there are no
parent edges to walk. The test passed even when the depth option
*did* leak between batches, because the structural choice of input
made the assertion unconditionally true. The fix replaced the orphan
target with a commit that has parents, so depth-leak actually
produces a non-empty boundary that the assertion can catch.

**Lesson**: For any output that is part of a protocol contract (helper
stdout, LFS JSON events, exit codes that downstream parses), assert
byte-exact equality on the relevant span. Reserve `contains` for
human-readable diagnostic strings where exact wording is allowed to
drift. The same principle extends to test *inputs*: verify that the
chosen fixture is capable of falsifying the assertion — a structurally
vacuous input (no parents, empty set, zero length) makes any depth,
presence, or boundary check unconditionally true and hides the very
bug the test was written to catch. The `audit-tests` skill exists for
this category — use it on new protocol tests before merge.

---

## 5. Test expected values must come from the spec, not the code

Writing a test by running the implementation, copying the output
into the assertion, and calling it green pins *whatever the code
does* — correct or not. For wire-format output (helper-protocol
lines, LFS JSON events, on-bucket keys, `error <ref>` strings), the
expected value must come from outside the implementation under test:
the helper-protocol spec, the LFS spec, the cloud-provider API spec,
or a hand-derived value from the on-bucket layout rules. Even a
byte-exact assertion (lesson #4) is worthless if the byte sequence
it compares against was generated by the code under test.

**The `?` suffix bug survived a passing test** (#34, c251914). The
existing `pre_lock_multi_bundle_rejection_surfaces_unchanged` test
used `contains("multiple bundles")` — loose matching is one failure
mode (lesson #4) — but even if it had matched the full string
exactly, the expected string would have been copied from the code's
actual output, which was already missing the `?`. The byte
comparison would have agreed with itself. The fix in the same
commit pins the byte-exact line, *and* derives the expected line by
matching the format every other `error <ref>` site uses.

**Bundle-key filter regex was derived from the on-bucket layout, not
from the implementation's output**. The protocol-smoke tests for
`list` / `list for-push` filter on `^refs/.+/.+/[a-f0-9]{40}\.bundle$`
to reject sibling-prefix collisions. The expected behaviour is
derived from the layout rules, not from running the code with one
input and copying the output — which is why the test catches
sibling-prefix bugs the implementation shape alone would not have
surfaced.

**Lesson**: For any test asserting wire-format output, document in
the test or its module-level comment where the expected value comes
from — protocol spec section, LFS spec, or a hand-derived
calculation from the on-bucket layout. If the answer is "I ran
`cargo test` and copied the output," the assertion is circular. The
`audit-tests` skill flags this category alongside loose substring
matching. Related to lesson #4 (exact bytes): that lesson says match
exactly; this one says the bytes you match against must come from
outside the code under test.

---

## 6. Shellspec `Skip if` cannot parse a leading `!` or shell redirection

`Skip if "<reason>" <condition>` evaluates `<condition>` through
shellspec's DSL preprocessor, not a plain bash interpreter. A leading
`!` is folded into the command name (so the negated test never runs as
intended) and shell redirections (`>/dev/null 2>&1`) get mangled by
shellspec's argument quoting. The failure mode is silent in the worst
way: the spec body runs as if the prerequisite were satisfied, then
falls over inside `BeforeAll` or `setup` with a confusing downstream
error (a missing CLI, a docker invocation, etc.) rather than the
intended `SKIPPED`.

**Integration-suite Skip guards initially didn't fire** (spec/integration
introduced alongside CHANGELOG "Shellspec integration suites" entry).
`Skip if "aws-cli not on PATH" ! command -v aws >/dev/null 2>&1` ran
the spec on a host without `aws`, which then failed in
`rustfs_make_bucket` with `aws: command not found` — a misleading
error several layers removed from the actual cause. The form
`Skip if "..." ! command -v aws` (no redirection) didn't run the spec
but aborted shellspec entirely with `[reporter: 101]`. Wrapping in
`bash -c "! command -v aws >/dev/null 2>&1"` worked but obscures
intent and forks a shell per Skip evaluation.

**Lesson**: In shellspec, `Skip if "<reason>" <cond>` requires
`<cond>` to be a single command (built-in, executable, or function
call) without a leading `!`, without pipelines, and without
redirections. Define small predicate functions in `spec/spec_helper.sh`
that already return the desired exit code — `missing_cmd foo`,
`have_cmd foo`, `flag_unset INTEGRATION_S3` — and call those from
Skip if. The `spec_helper.sh` definitions become the one place to
audit; spec files stay declarative.

---

## 7. The test harness must replicate the production install glue

When the production install instructions include filesystem-level glue
(symlinks, PATH munging, hooks, file-mode tweaks) that lives outside
the binary itself, the test harness must perform the same glue before
exercising the binary end-to-end. Skipping any step because "the
binary is built" leaves the harness dependent on whatever ad-hoc
configuration the host happens to have. The failure mode looks like
"my local works, CI doesn't" or vice versa, and the diagnostic almost
never points at the missing glue — it points at the symptom one layer
deeper.

**Integration shellspec suite hit `git: 'remote-s3+http' is not a git
command`** (spec/integration, spec/spec_helper.sh symlink shim).
`cargo build` produces binaries with hyphenated names
(`git-remote-s3-http`, `git-remote-az-https`, …), but git, given a
URL `s3+http://…`, looks up the literal `git-remote-s3+http` on
PATH. README's install section already documents the one-time symlink
loop end users run; the integration suite reproduced the failure
because the test harness skipped that step. `spec/spec_helper.sh` now
creates the four `+`-form symlinks in a per-run temp directory and
prepends it to PATH, mirroring the README workaround inside the
test session.

**Lesson**: For every step the production install docs spell out
beyond `cargo install` (or `cargo build`), the test harness's
bring-up code must perform the equivalent. When you find yourself
reading `README.md` to debug a test failure, that's the signal —
fold the missing step into `spec_helper.sh` (or the analogous
fixture) so the harness is self-contained. Cross-link from the
fixture comment to the README section so a future reader sees the
production/test parallel.

---

## 8. Feature-gated integration tests hide visibility regressions from `cargo test`

Integration test files in `tests/` that start with
`#![cfg(feature = "...")]` compile as empty translation units when
the feature is not enabled. Every identifier resolution, every
visibility check (E0603), and every type error inside the file is
skipped. A plain `cargo test` (no `--all-features`) reports zero
failures even when the file references items that became inaccessible
— so a visibility regression such as narrowing `pub mod X` to
`pub(crate) mod X` passes `cargo test` cleanly but breaks
`cargo test --all-features` (what `make pre-commit` uses).

**`pub(crate)` narrowing of `fetch` and `push` modules broke
`make pre-commit`** (b691d90, dcfc73a). `b691d90` changed
`pub mod fetch` and `pub mod push` in `src/protocol/mod.rs` to
`pub(crate)`. The integration test files `tests/protocol_fetch.rs`
and `tests/protocol_smoke.rs` reference `protocol::fetch::FetchError`
and `protocol::push::PushError` from outside the crate. Both files
are gated with `#![cfg(feature = "test-util")]`. Plain `cargo test`
silently skipped both files; `make pre-commit` (with `--all-features`)
caught the E0603 visibility errors. The regression shipped in a
commit (b691d90) aimed at tightening visibility and was caught only
when `make pre-commit` ran.

**Lesson**: Treat `#![cfg(feature = "...")]`-gated integration test
files as invisible to `cargo test` — they do not validate visibility
or type-correctness under normal CI unless `--all-features` is
explicitly passed. When narrowing the visibility of a `pub` item,
check whether any gated test file under `tests/` references it;
`make pre-commit` (or an equivalent `--all-features` check) must be
part of the pre-merge gate. If CI runs `cargo test` without
`--all-features`, it cannot be the sole correctness check.

---

## 9. Recursion bounds belong at the chokepoint, not the entry path

A recursion-depth or cycle bound that lives in only one of several
re-entry paths is no bound at all: a sibling edge that bypasses the
guarded function consumes none of the budget and recurses freely.
The fix shape is to move the guard down to the single chokepoint
every recursive path traverses — typically the function that
*dispatches* on the recursive shape — rather than scattering checks
across each call site that re-enters the recursion.

**`OFS_DELTA` chains bypassed `MAX_DELTA_DEPTH`** (#83, 9383408).
`MAX_DELTA_DEPTH` was checked-and-incremented in
`read_object_from_chain`. The `REF_DELTA` branch re-entered through
that function and stayed bounded, but the `OFS_DELTA` branch called
`decode_entry` directly with the same mutable counter, never
checking it. A long pure-`OFS_DELTA` chain in a malformed or
attacker-controlled pack could stack-overflow the reader. The fix
moves the guard to the top of `decode_entry` — the single
chokepoint every recursive resolution path traverses — so both
delta forms share the same budget. The regression test
(`ofs_delta_recursion_consumes_depth_budget`) exercises the
previously bypassed path with a synthetic in-memory pack.

**Lesson**: When a recursive routine has more than one re-entry
edge, put the bound at the dispatcher that every edge funnels
through, not at any one caller of it. If the bound *must* live at
the caller (e.g. to capture call-site context), enumerate every
caller in the same review and add the guard at each — and add a
regression test that exercises the path you suspect of bypassing
the guard. This pattern generalises beyond delta decoders to any
tree walker, interpreter, or expression evaluator with more than
one recursive edge.

---

## 10. Multi-file manifest writes need ordered durability and a typed mismatch error

When a logical "commit" spans more than one bucket object, the
durable-write order determines what concurrent readers and
post-crash readers observe. The invariant-holder — the file whose
presence defines "the commit happened" — must be written *last*,
and the reader path must surface the observable in-flight mismatch
as a typed transient error, not as the misleading downstream
failure that happens to fall out first. Both halves matter:
ordering alone leaves readers without a way to distinguish
"crashed-in-flight" from "genuinely corrupt", and a typed-error
without the ordering means corruption gets misclassified as
transient.

**`path-index.json` could become newer than `chain.json` across a
crash window** (#114, 7a480e0). Packchain push wrote
`path-index.json` before `chain.json`. A crash between the two
left a bucket state where the new path-index pointed at a tip in
the *new* tree while the chain manifest still listed only the
*old* segments. `read_blob` resolved a blob SHA from the new
path-index, then failed to find it in the segments named by the
old chain, and surfaced `BlobNotInChain` — indistinguishable from
genuine corruption. The fix flips the order (`chain.json` first,
`path-index.json` second) so a mid-flight crash leaves the bucket
in a state the reader can recognise: the path-index references a
tip not in `chain.json`. That recognised state now maps to the
new typed `TransientChainPathIndexMismatch`, distinct from
`BlobNotInChain`.

**Lesson**: For any composite on-bucket write, write the
invariant-holder last and document the ordering at the call site.
Then walk the read paths: every observable in-flight state must
either be invisible (overwritten atomically) or surface as a
typed transient error that callers can distinguish from
corruption. Compact, future engines, and any new manifest format
will face the same question — answer it before shipping, not
after the first crash report.

---

## 11. `--force` skips one named safety check, never accidentally bundles others

A `--force` flag is a single token but accumulates meaning over
time: each new safety check added to the surrounding code path
either explicitly tests `!force` (and gets skipped) or
unconditionally runs (and stays a safety). The flag's name
documents what the user thinks they are bypassing, and silently
expanding it bundles independent safeties under a single switch.
The right shape is one flag per named safety, or the new check
runs unconditionally and the flag documents only what its name
says.

**`gc sweep --force` deleted live packs from stale tombstones**
(#117, 1baa452). `--force` was introduced to skip the grace-window
wait so an operator could finalise a sweep without waiting hours.
Later, `sweep()` grew a live-pack re-check that compared the
tombstone list against a *fresh* `list_referenced_packs()` — a
defence against the race where `mark()` snapshotted before a
concurrent push committed `chain.json`. The re-check was guarded
by `if !opts.force && referenced.contains(sha)`, folding "skip
grace" and "skip the live re-check" under the same flag. A stale
tombstone with `--force` could then delete pack/idx objects that
a committed `chain.json` still referenced — data-loss-class. The
fix isolates `--force` to grace-window skip only; the live
re-check always runs.

**Lesson**: When adding a new safety check to a code path that
already has a `--force`-style flag, default to running the check
unconditionally. If the user really might need to bypass it, give
the bypass its own named flag (`--skip-live-recheck`) — not a
reuse of `--force`. Audit each existing `!force` guard during
review: does the flag's *name* tell the user they are bypassing
that specific check? If not, the guard is wrong.

---

## 12. Marker-key existence is HEAD + segment-equality, never LIST or substring

Object-store `list(prefix)` is a *byte-prefix* scan, not a
path-segment match: `list("a/PROTECTED#")` returns every key
whose byte sequence starts with that string, including hypothetical
future `PROTECTED#v2`, `PROTECTED#audit`, or any sibling artefact
that shares the prefix. Likewise, `last_segment.starts_with(MARKER)`
and `key.contains(MARKER)` are not equivalent to
`last_segment == MARKER`. For a *singleton* marker key whose
purpose is "this exact key exists or it doesn't", the only safe
shape is `head(exact_key)` + map `NotFound` to `false`, and the
only safe segment test is byte-equality on the final path segment.
The current bucket layout is no defence — it is correct by
accident the moment the marker becomes a family.

**Three sites used substring/prefix/LIST for `PROTECTED#`**
(#94, 81028d0; #111, e8fa6c4; #119, 2ed0c1e). `is_protected` in
`src/protocol/push.rs` used `store.list()` to test for the marker —
byte-prefix, plus a needlessly expensive `ListObjectsV2`/`ListBlobs`
round-trip on every protected-push attempt. `delete_remote_ref`
used a substring `contains(PROTECTED_MARKER_SEGMENT)`.
`snapshot::push_into_snapshot` used
`last.starts_with(PROTECTED_MARKER_SEGMENT)`. Each happened to be
correct only because exactly one literal (`PROTECTED#`) is ever
written under that segment today; every site was a future-schema
trap that would have silently flipped protection on for unrelated
`PROTECTED#`-prefixed keys. The fixes consolidate on
`keys::is_protected_marker_segment(last)` (equality helper) and
`store.head(exact_key)` (existence check).

**Lesson**: For singleton-marker keys, write one helper that
returns the exact key and one helper that tests segment equality,
and route every call site through them. Reviewing a new
marker-check site: if it calls `list()`, reaches for
`starts_with`, or open-codes `contains(MARKER)`, reject it — the
correct shape is `head(exact)` and `segment == MARKER`. Related to
lesson #3 (one helper for cross-cutting key transforms): #94
consolidated the three sites onto `keys::PROTECTED_MARKER_SEGMENT`
and `is_protected_marker_segment` — #3 covers the
helper-consolidation half; this lesson covers the API-choice half
(HEAD vs LIST, equality vs substring). Also related to lesson #4
(byte-exact protocol output): that lesson covers test assertions
on wire bytes; this one covers production-code existence and
equality checks on bucket keys. Same family of trap, different
surface.

---

## 13. Re-verify state under the lock, immediately before the destructive write

When a code path lists or snapshots remote state at time T1 and
performs a destructive operation (delete, overwrite, marker write)
at time T2, the elapsed window — an operator prompt, an interactive
selection, or another lock's worth of concurrent writes —
invalidates whatever invariant the original check confirmed. The
protection marker that wasn't there at T1 was written at T1 + 1s;
the lock that was "stale" at T1 was reclaimed at T1 + 30s; the
branch that existed at T1 was deleted at T1 + 5s. Every
snapshot-driven destructive path that doesn't re-read under the lock
immediately before the write is a TOCTOU bug waiting to be filed.
The right shape is one re-list/re-HEAD under the per-ref lock,
scoped to the exact key the next mutation touches, with no operator
interaction or unrelated work between the re-read and the write.

**Nine TOCTOU bugs filed in one batch-fix wave**
(#128, #129, #130, #131, #132, #137, #138, #139, #140;
commits a0ed694, 8836fd6, 27cd6d4, 3bc2ba6, 5539cbd, e9f20c6,
9759015, a720cce, 808adbe, 50d9a8b). Several were labelled
`security`. Representative shapes:

- **`delete-branch` listed objects, prompted the user, then deleted
  from the stale list** (#131, #139). A concurrent `protect`
  between LIST and the deletion loop left the `PROTECTED#` marker
  untouched while the bundles were destroyed; a concurrent push
  added new bundle keys the deletion loop never saw, so the branch
  survived the "delete" with the freshly-pushed content. Fix:
  re-list under the per-ref lock after the user confirms.
- **`doctor fix_head` wrote HEAD pointing to a branch that was
  deleted across the operator prompt** (#138). The doctor's
  analysis snapshot drove the candidate list; the selected branch
  could be deleted by a concurrent `git push :main` while the
  operator was reading the prompt. The doctor then "fixed" HEAD by
  pointing it at the freshly-deleted branch — exactly the
  invalid-HEAD condition it was supposed to repair. Fix: re-verify
  branch existence right before the HEAD write.
- **`doctor` stale-lock deletion deleted the active lock at the
  same key** (#132). A lock seen as stale in the initial listing
  was reclaimed by another client (via `acquire_lock` after stale
  recovery) before the doctor reached its deletion phase, minutes
  later. The doctor then unconditionally deleted the fresh lock —
  same key, no HEAD/ETag check — breaking mutual exclusion. Fix:
  re-HEAD each lock key immediately before deleting, comparing
  live `last_modified` against the TTL.
- **`gc sweep` reused a stale referenced-set across all
  tombstones** (#140). `list_referenced_packs` ran once at the top
  of `sweep()`; a concurrent push that committed a new
  `chain.json` referencing a tombstoned pack (deterministic
  baseline packs reproduce the same content SHA on force-push) was
  invisible to subsequent tombstone-processing iterations, and
  `sweep` deleted a live pack — permanent chain corruption. Fix:
  re-derive `referenced` per tombstone inside
  `sweep_one_tombstone`.

**Lesson**: For any destructive write — delete, overwrite, marker
put — the read that justifies it must happen under the same lock
window and immediately before the write, with nothing in between
(no operator prompts, no analysis, no iteration over a snapshot).
When reviewing a snapshot-driven flow, find the LIST/HEAD that
drives the decision and trace forward to the destructive call: if
the path crosses a lock release, an interactive prompt, or a loop
boundary, the invariant has expired. The fix shape is invariant
across callers — re-read under the lock, scope the re-read to the
exact key the next mutation will touch, and surface any divergence
from the snapshot rather than papering over it. Related to
lesson #11 (`--force` flag scope): that lesson is about flag
bundling; this one is about temporal staleness — both manifest as
"the safety check ran, but the destructive action no longer
satisfies the safety it claimed to satisfy."

---

## 14. After the durable commit lands, subsidiary work is best-effort

Once the contractual on-bucket write that defines "the operation
happened" is durable (bundle uploaded, `chain.json` committed), any
subsequent step in the same function — prior-bundle delete,
baseline cleanup, optional artifact upload — is no longer
load-bearing for the protocol's success contract. If those
subsequent steps return errors via `?`, the protocol reports
failure to the user even though git operations against the bucket
would already succeed. The user sees "push failed" with the git
data live on the backend; their retry succeeds because the durable
write is idempotent at its key, but the message they read first
lied about the remote state. The fix shape is to swallow
non-protocol errors with a `warn!` containing the orphan key, log
enough context for an operator to either re-push or run a future
repair workflow, and return success to the protocol.

**Three propagated-error bugs after the durable commit, one wave
apart** (#113, #121, #127; commits b816ae8, 58d0ed1, dcce5e2). All
three sit inside the same `perform_push_under_lock` (or
`compact_under_lock`) function, one frame past the bundle /
`chain.json` upload:

- **Compact's prior baseline delete propagated transient store
  errors** (#113, earlier wave). The new chain was already
  durable; the old baseline delete (`delete_idempotent` on the
  prior `<full_at>.bundle`) returned `?`, surfacing
  `PushError::Store` and reporting compact as failed. The orphan
  baseline survived; the next compact reattempt observed two
  baselines and tripped invariant checks.
- **Force-push old-bundle delete had the same shape, two frames
  over** (#121, 58d0ed1). The same `delete_idempotent(store,
  &prev).await?;` pattern in `perform_push_under_lock`, this time
  for the prior ref-prefix bundle. Found during `/batch-fix` Wave
  2 review of the #113 fix.
- **Optional `?zip=1` CodePipeline artifact upload propagated
  put_path errors** (#127, dcce5e2). The bundle, `HEAD`, and
  `FORMAT` were all durable; the trailing
  `store.put_path(&zip_dest, &artifacts.archive_path, opts).await?`
  for the optional zip artifact propagated, failing the push for
  what is explicitly a CodePipeline-side convenience surface (not
  a git-protocol surface). Same shape, three frames over.

**Lesson**: Identify the single line in each protocol entry
function that constitutes the durable commit (the
`put_path`/`put_bytes` whose success defines "the operation
happened"). Everything after that line is best-effort: cleanup,
optional artifacts, observability writes. Audit the `?` operators
after that point — each one is a potential "succeeded on the
bucket, failed to the user" bug. Replace them with a small
`_best_effort` helper that logs `warn!` with the ref path and
orphan key, and returns success to the protocol. Related to
lesson #10 (multi-file manifest writes need ordered durability +
typed mismatch error): #10 is about *write ordering* — what must
be written first so a crash leaves a recoverable state; this one
is about *error handling on the work that runs after the
contractual write* — what must not fail-loud once the protocol
contract is already met.

---

## 15. A test flipped alongside production code is no longer an oracle

When a single commit changes behavior AND modifies the corresponding
test in lockstep (flipping an assertion `!contains` → `contains` or
`is_empty` → `len > 0`, deleting an old test and writing a new one
with inverted expectations, or adding a new test that pins the new
behavior as the contract), the test stops oraculing the new code —
it becomes a self-consistent mirror.
`cargo test` and `make pre-commit` agree with themselves because the
production change and the new assertion describe the same world. The
contract the test was *supposed* to pin (operator expectation, wire
behavior, on-bucket post-condition) is no longer asserted anywhere
inside the suite. The check-in passes review for the same reason: a
reviewer reading the diff sees production and test moving together and
infers the change is intentional, when the question to ask is whether
the operator-visible contract moved with them.

**Bundle-engine helper-protocol delete left the bundle in place**
(#205, c5468b4 reverted). c5468b4 added a tombstone-defer to
`delete_remote_ref_under_lock` for in-flight-fetcher race safety.
In the same commit, it renamed
`delete_remote_ref_removes_single_bundle` to
`..._defers_..._via_tombstone` and flipped the assertion from
`!store.contains(<bundle>)` to `store.contains(<bundle>)` — plus
matching flips on the integration test and the lock-release test. The
unit suite went green. The live shellspec suite (gated behind
`LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY=1`, authored before c5468b4)
asserted on raw `aws s3 ls` output and caught the regression because
its oracle pre-dated the commit.

**`doctor --lock-ttl-seconds 0` silently clamped to default** (#208,
7dfa5dc → Doctor's call site rerouted to bypass the shared resolver).
7dfa5dc added the test `resolved_lock_ttl_honors_env_explicit_and_zero_clamp`
that pinned `Some(0)` → env-or-default as the contract. The test was
new, not modified — but the operator-facing contract it asserted
("explicit zero clamps") was the regression itself. The lib suite
agreed. The shellspec test `doctor --delete-stale-locks` with
`--lock-ttl-seconds 0` (authored a week earlier in 406c811) caught
it because, again, its assertion ("the seeded lock is gone") predated
7dfa5dc.

**Lesson**: When reviewing a diff that flips a test assertion alongside
a production change, ask explicitly: "would this test have failed
before this commit?" If yes (red → green), healthy. If the test was
added or rewritten in the same commit, it is not an oracle for that
commit — find or add an *independent* assertion that pre-dated the
change. Prefer post-condition assertions on observable contracts
(prefix listings, full stdout, operator-visible bucket state) over
assertions on specific implementation keys: the operator's expectation
moves slowly; specific keys move with every refactor. Related to
lessons #4 (exact bytes) and #5 (expected from spec): lesson #4 is
about assertion form, lesson #5 is about where the expected value
comes from, and this one is about whether the assertion's invariant
pre-dated the production change it oracles.

---

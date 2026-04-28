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

**Lesson**: For any output that is part of a protocol contract (helper
stdout, LFS JSON events, exit codes that downstream parses), assert
byte-exact equality on the relevant span. Reserve `contains` for
human-readable diagnostic strings where exact wording is allowed to
drift. The `audit-tests` skill exists for this category — use it on
new protocol tests before merge.

---

## 5. Upstream is a wire-format contract; classify every divergence

This project is a Rust port of `awslabs/git-remote-s3`, with the Python
implementation checked out at `../git-remote-s3` and designated as
source-of-truth for behaviour, on-bucket object layout, locking
semantics, LFS transfer protocol, and management-CLI command shapes
(`AGENTS.md`). Greenfield URL grammar and CLI surface are deliberate
divergences enumerated in `execution-plan.md` §0 / §3 / §6; the
on-bucket layout (`<prefix>/<ref>/<sha>.bundle`, `HEAD`, `PROTECTED#`,
lock files, `lfs/<oid>`) is a preserved invariant so existing buckets
remain readable. Any other divergence — wire bytes the helper writes
to stdout, key shapes on the bucket, error message phrasing that LFS
or git matches against — is a contract break against either upstream
or shipping users, even when the Rust code compiles and tests pass.

**Trailing `?` dropped from the under-lock duplicate-bundle error**
(#34, 84b1811). The under-lock branch emitted
`error <ref> "multiple bundles ..."` without the trailing `?` that
every other `error <ref>` line in the helper carries. Upstream
Python's wire format included it; the Rust port silently dropped it
on one of two structurally-identical branches. The fix landed as a
deliberate divergence (we keep `?` everywhere; upstream omits it on
this single path), now documented in CHANGELOG.

**Stale-ref cleanup behaviour and bundle-listing semantics**
(repeated CHANGELOG citations of `git_remote_s3/remote.py:286-296`,
`git_remote_s3/remote.py:574-593`, `git_remote_s3/lfs.py`'s
`ProgressPercentage`, etc.). Multiple porting tasks were resolved by
re-reading the Python at the cited line range and matching its
behaviour byte-for-byte; in each case, an "obvious" Rust shape
diverged from upstream until the diff was checked.

**Lesson**: For every behaviour port, read the upstream Python
before writing the Rust, and classify each observed difference as
INTENTIONAL (covered by `execution-plan.md`), IMPROVEMENT (a
deliberate fix that should be added to the divergence list), or BUG
(unintentional drift that must be reverted). Cite the upstream file
and line range in the commit message or CHANGELOG entry so the next
porter can verify. If a divergence isn't already listed in
`execution-plan.md`, stop and confirm before merging — silent
divergence is the failure mode this project is most exposed to.
Related to lesson #4 (exact wire bytes in tests): that lesson says
*how* to assert; this one says *what* to assert against.

---

## 6. Test expected values must come from upstream or the spec, not the code

Writing a test by running the Rust implementation, copying the output
into the assertion, and calling it green pins *whatever the code
does* — correct or not. For wire-format output (helper-protocol
lines, LFS JSON events, on-bucket keys, `error <ref>` strings), the
expected value must come from outside the implementation under test:
the upstream Python's behaviour against the same input, the
helper-protocol spec, the LFS spec, or a hand-derived value from the
on-bucket layout rules. Even a byte-exact assertion (lesson #4) is
worthless if the byte sequence it compares against was generated by
the code under test.

**The `?` suffix bug survived a passing test** (#34, c251914). The
existing `pre_lock_multi_bundle_rejection_surfaces_unchanged` test
used `contains("multiple bundles")` — loose matching is one failure
mode (lesson #4) — but even if it had matched the full string
exactly, the expected string would have been copied from the Rust
code's actual output, which was already missing the `?`. The byte
comparison would have agreed with itself. The fix in the same
commit pins the byte-exact line, *and* derives the expected line by
matching the format every other `error <ref>` site uses.

**Bundle-key filter regex was tightened against upstream behaviour,
not against the Rust output**. The protocol-smoke tests for
`list` / `list for-push` filter on `^refs/.+/.+/[a-f0-9]{40}\.bundle$`
to reject sibling-prefix collisions (CHANGELOG Phase 6). The
expected behaviour is derived from upstream's filter logic, not
from running the Rust code with one input and copying the output —
which is why the test catches sibling-prefix bugs the Rust shape
alone would not have surfaced.

**Lesson**: For any test asserting wire-format output, document in
the test or its module-level comment where the expected value comes
from — upstream Python file/line, protocol spec section, or a
hand-derived calculation from the on-bucket layout. If the answer is
"I ran `cargo test` and copied the output," the assertion is
circular. The `audit-tests` skill flags this category alongside
loose substring matching. Related to lesson #5 (upstream is the
contract): that lesson governs the production code; this one
governs the test fixtures. Related to lesson #4 (exact bytes): that
lesson says match exactly; this one says the bytes you match against
must come from outside the code under test.

---

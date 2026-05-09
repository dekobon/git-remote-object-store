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
test vacuous** (#50, d32741cb). The `depth_resets_between_batches`
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

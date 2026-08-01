---
globs: "**/*"
alwaysApply: false
---

## Build Before Test

Always rebuild (`cargo build`) before running integration tests. Never test against a stale binary.

## No Incidental Coupling in Test Infrastructure

Before encoding any property into test code, verify that the **system under test** actually depends on it. If the code doesn't branch on a property, the tests must not couple to it. Prefer runtime discovery (scanning, globbing) over constructing exact values from environmental or structural assumptions.

Common traps:

- **Host environment** (`cfg(target_arch)`, `std::env::consts::ARCH/OS`): only use if the code under test is architecture/OS-specific
- **Filename structure**: don't parse and reconstruct every segment of a naming convention — couple only to the segments the code needs
- **Directory layout**: don't hardcode paths that reflect organizational choices irrelevant to the logic being tested

## A Test That Agrees With the Code Is Not an Oracle

A green suite is evidence only if the assertion could have failed. Four
ways it silently cannot — check each when writing or reviewing a test:

1. **Loose assertion form.** For output that is part of a contract
   (helper stdout, LFS JSON events, on-bucket keys, exit codes another
   process parses), assert byte-exact equality on the relevant span.
   `contains("multiple bundles")` passes for a line missing the
   trailing `?` the wire format requires. Reserve `contains` for
   human-readable diagnostics whose wording is allowed to drift. The
   same trap wears other names: `is_ok()` on a `Result` whose error
   variant matters, "any nonzero exit code", matching log strings.
2. **Expected value taken from the code.** Run the implementation, copy
   its output into the assertion, and the test pins whatever the code
   does — correct or not. Wire-format expectations must come from
   outside the implementation: the helper-protocol spec, the LFS spec,
   the cloud-provider API spec, or a value hand-derived from the
   on-bucket layout rules. Say where it came from in the test or its
   module comment. "I ran `cargo test` and copied the output" means the
   assertion is circular, and byte-exactness does not save it.
3. **Structurally vacuous input.** Verify the fixture is *capable* of
   falsifying the assertion. A root commit has no parents, so a
   shallow-boundary walk returns `[]` at every depth and a depth-leak
   regression cannot be observed. Empty sets, zero lengths, and
   parentless nodes make presence, depth, and boundary checks
   unconditionally true.
4. **Oracle flipped in the same commit.** A commit that changes
   behaviour *and* rewrites the matching test — flipping `!contains` to
   `contains`, deleting a test and writing an inverted one, or adding a
   new test that pins the new behaviour as the contract — leaves the
   suite self-consistent and the original contract asserted nowhere.
   Ask: "would this test have failed before this commit?" If the test
   was added or rewritten alongside the change, it is not an oracle for
   it; find or add an assertion that pre-dates the change.

Prefer post-condition assertions on observable contracts (prefix
listings, full stdout, operator-visible bucket state) over assertions on
specific implementation keys. The operator's expectation moves slowly;
specific keys move with every refactor.

The `audit-tests` skill exists for this category — run it on new
protocol tests before merge.

## The Harness Must Replicate the Production Install Glue

When the install instructions include filesystem-level glue that lives
outside the binary — symlinks, PATH munging, hooks, file-mode tweaks —
the test harness must perform the same glue before exercising the binary
end-to-end. Skipping a step because "the binary is built" leaves the
harness dependent on whatever the host happens to have configured, and
the resulting diagnostic points one layer deeper than the actual cause.

When you find yourself reading `README.md` to debug a test failure,
that is the signal: fold the missing step into `spec/spec_helper.sh` (or
the analogous fixture) and cross-link from the fixture comment to the
README section, so the production/test parallel is visible to the next
reader.

## Feature-Gated Integration Tests

Integration test files under `tests/` that open with
`#![cfg(feature = "...")]` compile as empty translation units when the
feature is off — every identifier resolution, visibility check (E0603),
and type error inside is skipped, and the run reports success.

The `test-util`-gated files must be run with the feature named
explicitly:

```bash
cargo test --workspace --features git-remote-object-store/test-util --lib --bins --tests
```

`make test` and `make pre-commit` do this (pre-commit also runs the
`--doc` pass). Do **not** reach for `--all-features` instead:
`integration-s3` and `integration-azure` are `cli` features that require
Docker, and enabling them turns a plain test run into a container-backed
one.

A bare `cargo test` at the workspace root currently picks the feature up
anyway, through `cli`'s dev-dependency on the lib and resolver-v2
unification. Do not rely on that — it is incidental, it does not survive
a `-p`-scoped or non-default-member invocation, and it is exactly the
kind of dependency a test target should state rather than inherit. Use
`make test`.

When narrowing the visibility of a `pub` item, check whether any gated
file under `tests/` references it.

## Large-body integration tests (`RUN_LARGE_BODY_TESTS`)

A handful of integration tests upload a body in the > 5 GiB class to
exercise failure modes (S3 single-PUT ceiling, multipart-copy ceiling,
Azure block-count regimes) that the cheap 80 MiB tests cannot reach.
They are gated behind both `#[ignore]` and the `RUN_LARGE_BODY_TESTS`
env var so a default `cargo test` never pays for them.

Local run command:

```bash
RUN_LARGE_BODY_TESTS=1 cargo test --features integration-s3 -- \
    --ignored multipart_put_path_above_5_gib_round_trips
RUN_LARGE_BODY_TESTS=1 cargo test --features integration-azure -- \
    --ignored multipart_put_path_above_5_gib_round_trips
```

Cost per run, per backend:

- **Disk**: ~12 GiB scratch (the source plus a downloaded copy for
  round-trip hashing). The tests use `tempfile::tempdir()`, which
  honors `TMPDIR`; redirect to a roomier filesystem if `/tmp` is small.
- **Time**: dominated by container-startup and the upload/download
  loop. On a developer laptop against the in-Docker emulators this is
  several minutes; against real S3 / Azure it depends on egress
  bandwidth.
- **Egress**: zero against the local containers (the integration
  fixtures use `RustFS` for S3 and Azurite for Azure); against a real
  cloud backend, ~12 GiB of data transfer per run.

The mid-body abort tests (`multipart_put_path_aborts_on_midbody_truncation`)
are NOT gated — they run as part of the normal `integration-s3` /
`integration-azure` suites because they only need the 80 MiB body
the rest of the multipart suite already costs.

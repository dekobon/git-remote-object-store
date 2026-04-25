---
name: audit-tests
description: Audit test suites for tests that pass trivially, mask bugs, or assert the wrong thing. Finds tests designed to pass rather than designed to catch regressions.
---

# Audit Tests

Audit test code for tests that **pass for the wrong reason**. The goal is to
find tests that appear to provide coverage but would not catch a real
regression. This is complementary to `/review` (which checks production code
correctness); this skill focuses exclusively on test quality.

## Scope

Determine what to audit based on `$ARGUMENTS`:

| Argument | Scope |
|----------|-------|
| *(empty)* | Tests in unstaged + staged changes (`git diff HEAD`) |
| `staged` | Tests in staged changes only (`git diff --cached`) |
| `branch` | Tests in all commits on current branch vs `main` (`git diff main...HEAD`) |
| *file path* | Specific test file |
| *directory path* | All test files under the directory (`tests/`, `src/**/mod.rs`, `#[cfg(test)]` modules) |

---

## Step 1: Collect test code

1. Determine scope from `$ARGUMENTS`.
2. For diff-based scopes: extract only the test functions that were added or
   modified. Pre-existing unchanged tests are out of scope.
3. For path scopes: include all test functions reachable through `tests/*.rs`
   integration files and `#[cfg(test)] mod tests { ... }` blocks inside
   `src/`.
4. Read each test file **in full** — context around the test (helpers,
   constants, imports, fixtures) is essential.
5. If `docs/development/lessons_learned.md` exists, read it and note any
   lesson that bears on the tests under audit.

---

## Step 2: Understand what each test claims to verify

For every test in scope, answer three questions before applying the checklist:

1. **What does the test name say it tests?** Parse the name as a specification.
2. **What does the test body actually verify?** Trace from setup through
   assertions.
3. **What would have to break in production for this test to fail?** This is
   the test's real coverage claim.

If questions 1 and 3 have different answers, that is a finding.

---

## Step 3: Audit checklist

Apply every applicable question to every test in scope. Record each finding:

```
FINDING: <short title>
FILE: <path>:<line range>
CHECKLIST: <question number(s)>
EVIDENCE: <what is wrong and why, with code snippet or manual execution output>
SEVERITY: false-pass | weak-assertion | wrong-target | incidental-coupling
EFFORT: trivial | small | medium
```

### Trivially passing tests

These tests pass without exercising the behavior they claim to test.

1. **No-op test inputs**: Does the test use input that does not exercise the
   code path under test? Example: testing `--no-warnings` with a config that
   produces no warnings — the flag has nothing to suppress, so the test is
   identical to one without the flag.
2. **Tautological assertions**: Does the test assert something that is always
   true regardless of the code's behavior? Example: asserting `.is_ok()` on a
   function that never returns `Err` for the given input.
3. **Vacuously true negative assertions**: Does the test assert something is
   absent from output that was never going to be there? Example: asserting a
   field is not in JSON output when the endpoint never includes that field.
4. **Dead assertions**: Is there an assertion that cannot fail because an
   earlier assertion or control flow already guarantees it?

### Weak assertions

These tests exercise the right code path but don't check enough to catch a
real regression.

5. **Missing positive assertion after negative**: Does the test assert an
   original value is absent (redacted, filtered, removed) without asserting
   the replacement is present? If the code deleted the value instead of
   replacing it, the test would still pass.
6. **Empty collection accepted**: Does the test assert a collection is of the
   right type (`.is_array()`, `.is_object()`, `Vec::new()`) without checking
   it is non-empty? An empty result where data is expected is often a bug.
7. **Substring match on structured data**: Does the test use
   `contains("field_name")` on JSON / TOML / YAML output instead of parsing
   and checking structure? Substring matches can false-match on values,
   comments, or unrelated fields.
8. **Disjunctive assertions**: Does the test use `||` to accept multiple
   different outputs? This masks which code path actually ran. Each branch of
   an OR assertion should be its own test with deterministic setup.
9. **Exit code without output check**: Does the test assert only the exit code
   without checking stdout/stderr content? Many different failures produce
   the same exit code.
10. **Output check without exit code**: Does the test assert output content
    without checking the exit code? The output could come from an error path.
11. **`is_ok()` / `is_err()` without value inspection**: Does the test
    `.unwrap()` or `assert!(result.is_ok())` without further asserting on the
    `Ok` payload — or assert `is_err()` without checking the error variant?
    Many bugs change the value or variant inside `Result`, not the wrapper.

### Wrong target

These tests verify something different from what their name implies.

12. **Name-body mismatch**: Does the test name describe a scenario that the
    test body does not actually set up or verify?
13. **Testing the framework, not the code**: Does the test primarily exercise
    the test helper or framework rather than the production code? Example: a
    test that verifies `write_config()` creates a file, not that the CLI
    processes it correctly.
14. **Testing the happy path as error handling**: Does a test named for
    error handling only verify the happy path with valid input that happens
    not to trigger the error?

### Incidental coupling

These tests couple to properties the code does not depend on. (See
`.claude/rules/testing.md`: "No Incidental Coupling in Test Infrastructure".)

15. **Assertion depends on ordering that the code does not guarantee**: Does
    the test index into a collection (e.g., `violations[0]`, `errors.first()`)
    when the production code does not guarantee order? A future change that
    reorders results breaks the test without introducing a bug.
16. **Assertion coupled to exact formatting**: Does the test assert on the
    exact string representation (whitespace, punctuation, capitalization)
    when the contract is semantic, not lexical?
17. **Assertion coupled to implementation detail**: Does the test assert on
    an internal detail (specific error message wording, internal field name,
    intermediate computation) rather than the observable contract?
18. **Coupling to host environment**: Does the test encode `cfg(target_arch)`,
    `std::env::consts::ARCH`, hardcoded directory layouts, or filename
    structure that the production code does not actually branch on?
    (`.claude/rules/testing.md`.)

### Test isolation

19. **Host-dependent test**: Does the test depend on host state (installed
    software, running processes, config files, network) without isolating
    itself? Will it pass on CI and fail locally (or vice versa)?
20. **Shared mutable state**: Do tests share a temp directory, file, env
    variable, or global without isolation? Can parallel test execution cause
    flaky failures? (Example: tests that mutate `std::env` must serialize via
    a `Mutex` and acquire it across all readers as well, not just writers.)

---

## Step 4: Verify findings by execution

For each finding, verify it is real by running the test (and, where
applicable, by running the same logic with a perturbation that should fail):

```bash
# Run a single test by name (full path or partial match)
cargo test --test <integration-file> <test_name>
cargo test --lib <module>::tests::<test_name>

# For a finding that claims a test would still pass under a buggy
# production change, the strongest verification is mutation testing:
# temporarily edit the production code to introduce the bug, rerun the
# test, and confirm it still passes. Revert the production change after.
```

Compare actual behavior against what the test asserts. Discard findings
where execution shows the test is actually correct despite looking
suspicious.

**This step is mandatory.** Do not report findings based solely on code
reading. If you cannot run the test (e.g., requires network or binaries
unavailable in the environment), say so and downgrade the finding to a
suggestion rather than a confirmed defect.

Before running tests, rebuild if production code has changed:

```bash
cargo build
```

(Per `.claude/rules/testing.md`: never test against a stale binary.)

---

## Step 5: Report and fix

### Report format

Print findings grouped by severity:

```
## Test Audit: <scope description>

### False-pass (tests that pass without testing what they claim)
| # | Finding | File:Line | Effort | Evidence |

### Weak assertions (right code path, insufficient checking)
| # | Finding | File:Line | Effort | Evidence |

### Wrong target (testing something other than what the name implies)
| # | Finding | File:Line | Effort | Evidence |

### Incidental coupling (coupled to properties the code doesn't depend on)
| # | Finding | File:Line | Effort | Evidence |

### Summary
- Tests audited: N
- Findings: N (N false-pass, N weak-assertion, N wrong-target, N incidental-coupling)
- Verdict: PASS | FINDINGS TO FIX
```

### Fix

For each finding, fix the test directly:

- **False-pass**: Redesign the test input so the code path is actually
  exercised. For example, if testing `--no-warnings`, use a config that
  produces both warnings and errors.
- **Weak assertion**: Add the missing assertion. For example, if checking a
  value was redacted, also assert the replacement is present. If asserting
  `.is_ok()`, inspect the `Ok` payload.
- **Wrong target**: Rename the test or rewrite the body to match the name.
  Per `.claude/rules/testing.md`, do not rewrite the entire test file —
  modify only the specific tests that need changing.
- **Incidental coupling**: Replace index-based access with `.iter().any()`
  or `.find()`. Replace exact string matches with semantic checks. Decouple
  from host environment unless the production code actually branches on it.

After fixing, run the affected tests to verify they still pass:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

---

## Guardrails

- Do NOT report findings without verifying them by running the test
  (or stating clearly that execution was not possible).
- Do NOT flag tests for style issues (naming conventions, assertion style)
  unless the style causes a false-pass or weak assertion.
- Do NOT flag pre-existing tests outside the scope unless the change made
  them worse.
- Do NOT refactor test helpers or consolidate tests — this skill is about
  correctness, not cleanup.
- Do NOT touch production code. Only test files are in scope.
- Per `.claude/rules/testing.md`: never rewrite an entire test file to fix
  one assertion; modify only the specific tests that need changing.
- Run `cargo test --workspace` after all fixes to verify nothing broke.

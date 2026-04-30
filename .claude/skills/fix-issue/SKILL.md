---
name: fix-issue
description: Complete workflow for fixing GitHub issues including investigation, implementation, review, testing, and documentation. Use when asked to fix a GitHub issue.
---

# Fix GitHub Issue Workflow

1. Read the GitHub issue thoroughly (`gh issue view <number>` plus all comments).
2. If Serena (or another LSP-based code intelligence MCP) is available, activate
   the project (`serena:activate_project`) so symbol-level navigation and editing
   are the default. Per `.claude/rules/tool-choice.md`, LSP tools are preferred
   over text-based search/edit for code; fall back to `rg`/`fd` and the built-in
   Grep/Glob tools only when LSP is unavailable. Never use legacy `grep`/`find`.
3. Re-read the project conventions that govern this fix:
   - `AGENTS.md` — relationship to upstream and greenfield rules.
   - `.claude/rules/rust.md`, `.claude/rules/naming.md`, `.claude/rules/testing.md`,
     `.claude/rules/git-commits.md`, `.claude/rules/worktree-safety.md`.
   Note any rule that is directly relevant so it can be cited in the fix.
4. Investigate the codebase to understand the root cause. For any
   behavior that touches a wire-format surface — the on-bucket object
   layout, helper-protocol stdout bytes, LFS JSON events, or error
   strings that git/git-lfs match against — read the upstream Python
   at `../git-remote-s3` as a reference implementation; it is not
   authoritative, but it is the closest existing reference and saves
   debugging time.
5. Check for the same bug pattern elsewhere in the codebase. If the root cause
   is a repeated pattern, fix all instances — do not leave known-broken siblings
   for a follow-up.
6. **Plan the fix using sequential thinking.** Use the
   `sequential-thinking:sequentialthinking` MCP tool to reason through the
   resolution step by step before writing any code. The sequential thinking
   process MUST:
   - **Start** with `thoughtNumber: 1`, an initial `totalThoughts` estimate
     (typically 5-8), and `nextThoughtNeeded: true`.
   - **Analyze** the root cause — not just the symptom. Trace the data/control
     flow that leads to the bug.
   - **Enumerate approaches** and evaluate trade-offs (simplicity, correctness,
     performance, scope).
   - **Identify edge cases** — empty inputs, boundary values, concurrent access,
     error paths, S3-vs-Azure differences, partial-multipart failures, network
     retries, lock contention. Walk through each edge case and confirm the
     proposed fix handles it.
   - **Cross-check against project rules** — if the fix would introduce a silent
     `unwrap_or_default`, an `unwrap()` in non-test code, an unexplained
     abbreviation, a `to_string_lossy()` on an identifier path, a `splitn` on an
     untrusted empty pattern, or any of the other anti-patterns called out in
     `.claude/rules/rust.md` and `.claude/rules/naming.md`, redesign before
     proceeding.
   - **Verify completeness** — confirm the plan covers implementation, tests,
     and documentation before concluding.
   - **Conclude** with `nextThoughtNeeded: false` and a final plan summary.
   - Adjust `totalThoughts` up or down as understanding evolves. Use
     `isRevision` if earlier reasoning needs correction.
7. **Implement the fix.** Do NOT stop after planning — execute the plan from
   step 6. If the implementation reveals issues the plan missed, revise via
   sequential thinking before proceeding. Before changing any public API, run
   `find_referencing_symbols` (or equivalent) to enumerate every call site.
8. **Write tests.** Sufficient testing is mandatory before review. At minimum:
   - **Unit tests**: for all new or changed public functions. Each edge case
     identified in step 6 should have a corresponding test.
   - **Integration tests**: for end-to-end behavior changes. Always run
     `cargo build` before integration tests so they exercise the new binary —
     never test against a stale binary (per `.claude/rules/testing.md`).
   - **Cross-backend coverage**: if the fix touches storage, exercise both the
     S3 and Azure Blob paths (or document why one is N/A).
   - **Regression check**: `cargo test --workspace` and `cargo clippy
     --workspace --all-targets -- -D warnings` must pass.
   - Tests must actually assert what they claim — no silent fallbacks
     (`unwrap_or_else` that swallows failures), no missing assertions, no
     coupling to incidental host/filesystem details
     (`.claude/rules/testing.md`).
9. **Review the changes** for:
   - **Correctness**: Does the fix actually address the root cause, not just the
     symptom? For wire-format-sensitive code, has the change been
     verified against the upstream Python reference at `../git-remote-s3`?
   - **Performance**: Are algorithms and data structures appropriate? Avoid
     O(n²) when O(n) is feasible. Consider hot paths (push/fetch on large
     histories, multipart transfers) and large inputs.
   - **Simplicity**: Is this the simplest fix that solves the problem? No
     over-engineering, no speculative abstractions, no shims for
     backwards-compat that this greenfield project explicitly forbids.
   - **Completeness**: Are edge cases handled? Are there similar patterns
     elsewhere that need the same fix?
   - **Test coverage**: Do the tests from step 8 cover the root cause, edge
     cases, and regression scenarios?
   - **Conventions**: Does the diff respect `.claude/rules/rust.md` (no
     `unwrap`/`expect`/`panic` outside tests, no `unsafe`, newtypes for
     domain invariants), `.claude/rules/naming.md` (one word per concept,
     positive boolean predicates, `as_`/`to_`/`into_`/`from_` semantics), and
     `.claude/rules/tool-choice.md`?
10. Fix any issues found in review. If the fixes were non-trivial, re-review.
    Do NOT commit with known issues and plan to fix them in a follow-up — that
    is how fix-up chains happen.
11. Run the final gate before committing:
    - `cargo fmt --all -- --check`
    - `cargo clippy --workspace --all-targets -- -D warnings`
    - `cargo test --workspace`
    - `markdownlint-cli2` against any Markdown files touched (per
      `.claude/rules/markdown.md`).
    If any check fails, fix and re-run until clean.
12. Run integration tests against the NEW binary if applicable to the change
    (rebuild first; never test stale).
13. **Update all documentation.** Review and update each of the following as
    applicable:
    - `CHANGELOG.md` — add an entry under the appropriate section (Added,
      Changed, Fixed) per `.claude/rules/changelog.md`.
    - `README.md` — if user-facing behavior, install steps, or supported
      backends changed.
    - Crate-level or module-level doc comments (`//!`) — if module intent or
      architecture changed.
    - Avoid hardcoding stale counts in any doc per
      `.claude/rules/documentation.md` ("all tests passing", not "42 tests
      passing").
    - Re-run `markdownlint-cli2` after editing any Markdown files.
14. If there is a hard-won, globally reusable lesson from this fix, run the
    `/lessons-learned` skill (or propose an entry directly to
    `docs/development/lessons_learned.md`) and prompt the user for approval.
    Keep the bar high — only lessons that cost real debugging time and are
    likely to recur. Project conventions (not lessons) belong in
    `.claude/rules/` or `AGENTS.md`.
15. Commit using Conventional Commits (`.claude/rules/git-commits.md`):
    `<type>(<scope>): <subject>` with `Fixes #NN` in the **body**, not the
    subject line. Keep commits atomic; do not add `Co-Authored-By` lines.
16. Update the GitHub issue body with results AND add a comment with research
    and findings (per `.claude/rules/github-cli.md`: use `--body-file` with a
    temp file for non-trivial bodies).
17. Close the issue with `gh issue close <number>` only when ALL items are
    resolved. If items remain unresolved, do NOT close — instead update the
    issue body to reflect what is done and what is left.

## Worktree Safety Reminder

If this session is running inside a worktree (`git rev-parse --show-toplevel`
returns a path under `.claude/worktrees/`), the bans in
`.claude/rules/worktree-safety.md` apply throughout this workflow: never delete
worktrees, never `cd` to the main repo, never check out a different branch,
never write to files outside your worktree.

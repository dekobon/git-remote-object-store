---
name: batch-fix
description: Fix multiple GitHub issues on an integration branch. Issues touching different modules run in parallel worktrees; issues sharing a module run sequentially. Each goes through fix, simplify, review, and remediation before merging. Use when asked to fix several issues at once.
---

# Batch Fix GitHub Issues

Fix multiple GitHub issues on a single integration branch. Issues are
classified by affected module(s) and triaged for quick-win priority and
cross-issue dependencies, then scheduled into waves where issues touching
different modules run in parallel. Quick wins are front-loaded for fast
feedback. Issues sharing a module are serialized to avoid merge conflicts.
Each issue goes through the full pipeline: investigate, fix, simplify,
review, remediate, validate, commit. Successful fixes are merged to the
integration branch. Failures are logged and skipped.

## Arguments

Parse `$ARGUMENTS` as a space-separated list of issue references and flags:
`#42 #57 #63` or `42 57 63` (with or without `#` prefix).

Optional flags:

- `--sequential`: disable parallel processing, process all issues one at a
  time (original behavior). Use when issues have cross-module dependencies.

Extract the numeric issue numbers. If no issues are provided, abort with:
"Error: provide at least one issue number. Usage: /batch-fix #42 #57 #63"

---

## Step 0: Validate

### 0a: Validate issues exist

For each issue number, run:

```bash
gh issue view <number> --json number,title,state,labels,body,comments --jq '{number, title, state, labels: [.labels[].name], body, comments}'
```

If any issue does not exist or is already closed, warn the user and remove it
from the list. If no valid open issues remain, abort.

Record each issue's number, title, body, labels, and comments for later steps.
This data is reused in Step 2 (classification) and Step 4 (worktree agent
prompts) -- do not re-fetch.

### 0b: Ensure clean working tree

```bash
git status --porcelain
```

If there are uncommitted changes, abort with:
"Error: working tree is dirty. Please commit or stash your changes before
running /batch-fix."

### 0c: Record the base branch

```bash
git rev-parse --abbrev-ref HEAD
```

Record this as `BASE_BRANCH` so we can return to it if needed.

### 0d: Detect isolation mode

```bash
PROJECT_ROOT="$(git rev-parse --show-toplevel)"
if [[ "$PROJECT_ROOT" == *".claude/worktrees/"* ]]; then
  ISOLATION_MODE="worktree"
else
  ISOLATION_MODE="branch"
fi
```

- **Worktree mode**: Agents are launched with `isolation: "worktree"` and run
  in parallel (existing behavior).
- **Branch mode**: Agents are launched WITHOUT `isolation: "worktree"` and run
  sequentially using feature branches. This preserves Serena LSP compatibility.
  All agents in a wave MUST be processed one at a time (they share the working
  directory).

Record `ISOLATION_MODE` for use in Step 4.

---

## Step 1: Create integration branch

Determine a unique branch name. Try `fix/batch-YYYY-MM-DD` first, then
append a sequence number if it already exists:

```bash
DATE=$(date +%Y-%m-%d)
BRANCH="fix/batch-${DATE}"
SEQ=2
while git rev-parse --verify "$BRANCH" >/dev/null 2>&1; do
  BRANCH="fix/batch-${DATE}-${SEQ}"
  SEQ=$((SEQ + 1))
done
git checkout -b "$BRANCH" main
```

Record the branch name as `INTEGRATION_BRANCH`.

---

## Step 2: Classify and triage issues

For each issue, determine which module(s) it affects and assess complexity.
This lightweight triage improves wave scheduling without adding API calls
(all data was cached in Step 0a).

### 2a: Module classification

This crate is single-package; "module" means a top-level module under `src/`.
The current top-level modules are: `protocol`, `object_store`, `lfs`,
`manage`, `bin`, `url`, `git`.

Use these signals in priority order:

1. **Labels**: GitHub labels matching module names (e.g., `protocol`,
   `object_store`, `lfs`, `url`) map directly to modules.
2. **Title/body keywords**: Look for module names, file paths
   (`src/<module>/...`), or distinctive terms:
   - "push", "fetch", "ref", "bundle", "lock", "helper protocol",
     "capabilities", "git-remote-* binary", "stdout discipline" -> `protocol`
   - "S3", "Azure Blob", "multipart", "presigned", "ETag", "credentials",
     "MockStore", "object store", "backend" -> `object_store`
   - "LFS", "custom transfer", "OID", "git-lfs-*" -> `lfs`
   - "management CLI", "git-remote-object-store binary", "subcommand",
     "ls", "rm", "list" -> `manage`
   - "URL", "scheme", "grammar", "host", "bucket", "container", "prefix",
     "userinfo" -> `url`
   - "git plumbing", "gitoxide", "ref discovery", "object resolution",
     "loose object", "bundle write" -> `git`
   - Reserve `bin` only for issues that touch ONLY `src/bin/*.rs` thin
     wrappers — argv parsing, `tracing-subscriber` setup, banner output —
     with no behavior change in the underlying module. Helper-protocol
     binaries route to `protocol`; the management binary routes to
     `manage`; the LFS binary routes to `lfs`.
3. **Ambiguous**: If the module cannot be determined from labels or keywords,
   classify as `unknown`.

**Special case -- `object_store` module**: `object_store` is a shared
dependency of `protocol`, `lfs`, and `manage`. Issues classified as
`object_store` are likely to have cross-module ripple effects. Default to
`cross_module: true` for `object_store` issues unless the issue body makes
it clear the change is internal (e.g., a self-contained bug fix in a single
backend with no public trait or API impact).

### 2b: Quick-win detection

Flag issues as `quick_win: true` if they match **two or more** positive
indicators AND **zero** disqualifiers.

**Positive indicators** (from title, body, and comments):

- References a single specific file path (e.g., `src/url.rs`,
  `src/protocol/push.rs`)
- Contains a clear error message or panic trace
- Mentions a specific function, struct, or constant name
- Has a "good first issue" or "bug" label
- Body is short (< 500 characters) with a clear reproduction case
- Fix is described in the issue itself (e.g., "should use X instead of Y")

**Disqualifiers** (any one prevents quick-win):

- Requires new public API or architectural changes
- Spans multiple modules explicitly ("change X in object_store and update
  protocol")
- Touches the on-the-wire object layout, locking semantics, or LFS transfer
  protocol (these require upstream-Python parity verification)
- Needs external input or design decision ("should we...?", "RFC")
- References an unimplemented phase from `execution-plan.md`
- Has `cross_module: true` from Step 2a

### 2c: Cross-issue dependency detection

Scan each issue's title, body, and comments for references to other issues
in the current batch:

- Patterns: `depends on #<N>`, `blocked by #<N>`, `after #<N>`,
  `requires #<N>`
- Bare `#<N>` references do NOT imply dependency -- issues commonly
  cross-reference each other for context without ordering constraints
- Only consider references to issue numbers that are in the current batch

If issue A references issue B, record: `A depends_on B`. This means B must
be scheduled in an earlier wave than A.

**Cycle detection**: If dependencies form a cycle (A->B->C->A), log a
warning and drop all edges in the cycle -- treat those issues as independent.

### 2d: Print classification

For each issue, record:

- `module`: the primary affected module name, or `unknown`
- `cross_module`: `true` if the issue clearly spans multiple modules,
  `false` otherwise
- `quick_win`: `true` if the issue matches the quick-win criteria above
- `depends_on`: list of issue numbers this issue depends on (empty if none)

Print the classification table:

```
## Issue Classification
| Issue | Title | Module | Cross-module | Quick-win | Depends on |
|-------|-------|--------|--------------|-----------|------------|
```

---

## Step 3: Schedule waves

Group issues into processing waves. The goal: maximize parallelism while
respecting module conflicts, dependencies, and quick-win priority.

### Rules

1. Two issues can run in the same wave only if they affect **different
   modules** (neither is `unknown`, neither is `cross_module`, and their
   module values differ).
2. `unknown` and `cross_module` issues are placed in their own wave
   (one at a time) after all classified issues.
3. If `--sequential` was specified, every issue gets its own wave.
4. **Dependency ordering**: If issue A `depends_on` issue B, B must appear
   in an earlier wave than A. Dependencies take precedence over quick-win
   priority.
5. **Quick-win priority**: Within each module group, quick-win issues are
   scheduled before non-quick-win issues. This front-loads fast fixes into
   early waves, giving rapid feedback and reducing blast radius.
6. User-specified order is preserved as a tiebreaker within each module
   group (after dependency and quick-win sorting).

### Algorithm

```
classified = issues grouped by module (excluding unknown/cross_module)
unclassified = issues marked unknown or cross_module
deps = dependency graph from Step 2c

# Sort each module group: quick_wins first, then user order
for module in classified:
    classified[module].sort(key=lambda i: (not i.quick_win, user_order(i)))

# Build waves from classified issues
waves = []
scheduled = set()  # issue numbers already assigned to a wave
remaining = copy of classified (dict of module -> [issue list])
while remaining is not empty:
    wave = []
    modules_in_wave = set()
    for module in list(remaining.keys()) sorted by most issues first:
        if module not in modules_in_wave:
            # Find the first issue whose dependencies are all scheduled
            candidate = None
            for issue in remaining[module]:
                if all(dep in scheduled for dep in issue.depends_on):
                    candidate = issue
                    break
            if candidate is not None:
                remaining[module].remove(candidate)
                wave.append(candidate)
                modules_in_wave.add(module)
    # Clean up empty module groups after the wave is built
    remaining = {m: issues for m, issues in remaining if issues is not empty}
    # Guard against deadlock from unresolvable dependencies
    if wave is empty and remaining is not empty:
        # Force-schedule one issue to break the deadlock
        module = first key of remaining
        issue = remaining[module].pop(0)
        wave = [issue]
        remaining = {m: issues for m, issues in remaining if issues is not empty}
    for issue in wave:
        scheduled.add(issue.number)
    waves.append(wave)

# Append unclassified issues as single-issue waves (respecting dependencies)
pending = list(unclassified)
while pending:
    for issue in pending:
        if all(dep in scheduled for dep in issue.depends_on):
            waves.append([issue])
            scheduled.add(issue.number)
            pending.remove(issue)
            break
    else:
        # Deadlock -- force-schedule the first pending issue
        issue = pending.pop(0)
        waves.append([issue])
        scheduled.add(issue.number)
```

Print the wave plan:

```
## Processing Plan
Isolation: <worktree (parallel) | branch (sequential, Serena-compatible)>
Wave 1 (parallel): #42 (protocol, quick-win), #57 (url)
Wave 2 (parallel): #63 (protocol), #71 (lfs, quick-win)
Wave 3 (sequential): #80 (object_store, cross-module, depends on #42)
```

In branch mode, also note: "Agents in later waves see changes from earlier
waves (branch mode advantage)."

If all waves are single-issue, note: "All issues serialized (same module or
unclassified)."

---

## Step 4: Process waves

For each wave, in order:

### 4a: Spawn agents

Use the issue data (title, body, comments) cached from Step 0a to populate
each agent's prompt.

Pass each agent the full agent prompt (see below) with `<ISSUE_NUMBER>`,
`<ISSUE_TITLE>`, and `<ISSUE_BODY>` substituted.

#### Worktree mode (`ISOLATION_MODE=worktree`)

**CRITICAL**: Every agent MUST be launched with `isolation: "worktree"`.
This is a required parameter on the Agent tool call, not optional. Agents
launched without worktree isolation will modify the main project directory,
corrupting the integration branch. Double-check that every Agent tool call
includes `isolation: "worktree"` before sending.

For a **single-issue wave**: launch one Agent with `isolation: "worktree"`
and `model: "opus"`.

For a **multi-issue wave**: launch ALL agents in a single message block
(parallel tool calls). Each agent gets `isolation: "worktree"` and
`model: "opus"`. Do NOT use `run_in_background` -- wait for all agents in
the wave to complete before proceeding.

**Known limitation**: Each worktree agent forks from `main`, not from the
integration branch tip. Agents do not see changes from prior waves during
investigation. The merge in Step 4b handles this mechanically. For tightly
coupled issues, use `--sequential` or run them as a single `/fix-issue`.

#### Branch mode (`ISOLATION_MODE=branch`)

All agents in a wave MUST be processed **sequentially** (one at a time).
They share the working directory, so parallel execution is FORBIDDEN.

For each issue in the wave, in order:

1. Create a feature branch from the integration branch:

```bash
BRANCH="fix/issue-${ISSUE_NUMBER}"
if git rev-parse --verify "$BRANCH" >/dev/null 2>&1; then
  git branch -D "$BRANCH"  # stale local branch from prior run
fi
git checkout -b "$BRANCH" "$INTEGRATION_BRANCH"
```

2. Launch ONE Agent with `model: "opus"` (NO `isolation: "worktree"`).

3. On **SUCCESS**:

```bash
git checkout "$INTEGRATION_BRANCH"
git merge "fix/issue-${ISSUE_NUMBER}" --no-edit
git branch -d "fix/issue-${ISSUE_NUMBER}"
```

4. On **FAILED** or **SKIPPED**:

```bash
git checkout -- .
git reset HEAD
git checkout "$INTEGRATION_BRANCH"
git branch -D "fix/issue-${ISSUE_NUMBER}"
```

5. Verify clean state before the next issue:

```bash
DIRTY="$(git status --porcelain)"
if [[ -n "$DIRTY" ]]; then
  echo "WARNING: working tree dirty after agent cleanup, cleaning..."
  git checkout -- .
  git clean -fd
fi
```

**Branch mode advantage**: Each feature branch is created from the integration
branch AFTER prior merges, so later agents see earlier fixes. This is an
improvement over worktree mode where agents fork independently from `main`.

### 4b: Process results (after all agents in wave complete)

> **Branch mode**: Skip this step — results are processed inline in Step 4a.

For each agent result in the wave:

The worktree agent returns one of:

- **SUCCESS**: branch name, commit hash, files changed, summary, changelog entry, divergence note
- **SKIPPED**: reason (issue is invalid, already fixed, or requires no code changes)
- **FAILED**: reason, what was attempted

**On SUCCESS**:

1. Ensure we are on the integration branch:

```bash
git checkout <INTEGRATION_BRANCH>
```

2. Merge the worktree branch:

```bash
git merge <worktree-branch> --no-edit
```

3. If merge conflict occurs, abort and log as FAILED:

```bash
git merge --abort
```

Log: "Issue #N: FAILED -- merge conflict with prior fixes on integration branch"

**On SKIPPED**:

Log the skip reason and continue to the next result. No merge needed.

**On FAILED**:

Log the failure reason and continue to the next result.

### 4c: Wave checkpoint

After merging all successful results from the wave, run a quick compile
check on the integration branch:

```bash
git checkout <INTEGRATION_BRANCH>
cargo check --workspace --all-targets
```

If `cargo check` fails after a multi-issue wave merge, the conflict is
between issues in this wave. Identify the culprit:

1. Record the list of merge commits from this wave (oldest to newest).
2. Reset the integration branch to the state before this wave's merges:

```bash
git reset --hard <pre-wave-commit>
```

3. Re-merge each wave branch one at a time, running `cargo check` after
   each merge. The first merge that causes `cargo check` to fail is the
   culprit.
4. Abort that merge (`git merge --abort`), log the issue as FAILED with
   reason "compilation conflict with parallel fix in same wave".
5. Continue re-merging the remaining (innocent) branches, skipping only
   the culprit.

Proceed to the next wave.

---

## Step 5: Consolidate shared files

After all waves are processed, collect CHANGELOG and execution-plan entries
from all successful agents and apply them in a single commit on the
integration branch. This avoids merge conflicts on shared files.

### 5a: Update CHANGELOG.md

Collect the `CHANGELOG:` entries from all successful agent results. Add them
to the `[Unreleased]` section of `CHANGELOG.md` under the appropriate
subsection (`### Fixed`, `### Added`, `### Changed`) per
`.claude/rules/changelog.md`. Each entry should reference the issue number
(e.g., `- Fix incorrect URL host validation (#42)`).

### 5b: Record divergences (if applicable)

If any agent reported a `DIVERGENCE:` entry other than "none" (a deliberate
divergence from upstream `git-remote-s3` behavior, or a change to a resolved
decision), append it to a single, append-only section at the bottom of
`execution-plan.md` titled `## Resolved Divergences (post-plan)`. Create the
heading if it does not yet exist. Each entry must include the issue number,
the affected upstream behavior, and the rationale. Format:

```markdown
## Resolved Divergences (post-plan)

### #<ISSUE_NUMBER> — <short title> (<YYYY-MM-DD>)

- **Upstream behavior**: <what `../git-remote-s3` does>
- **New behavior**: <what this crate now does>
- **Rationale**: <why the divergence is justified>
- **Affected sections**: <e.g., relates to §3 URL grammar, §6 resolved
  decisions — but the canonical record lives in this section>
```

Do NOT edit §0/§3/§6 inline from this skill — those sections are the
authoritative pre-implementation plan and are easy to corrupt with
parallel divergence notes. The append-only section is the canonical record;
a maintainer can later promote stable entries into §0/§3/§6 by hand.

Per `AGENTS.md`, undocumented divergences from upstream are not allowed --
if an agent flagged a divergence, it MUST be recorded here.

### 5c: Run markdown lint on touched docs

```bash
markdownlint-cli2 CHANGELOG.md execution-plan.md
```

Fix any lint issues before committing.

### 5d: Commit shared file updates

```bash
git add CHANGELOG.md execution-plan.md
git commit -m "$(cat <<'EOF'
docs: consolidate changelog and divergences from batch fix

Update CHANGELOG.md (and execution-plan.md if applicable) with entries
from all successfully merged issue fixes in this batch.
EOF
)"
```

Skip this step if no shared files were changed.

---

## Step 6: Final validation

After all waves are processed, if any merges succeeded:

### 6a: Run the project's pre-commit gate

```bash
git checkout <INTEGRATION_BRANCH>
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

(Run the markdown lint from Step 5c too, if any Markdown files were touched
by an agent.)

### 6b: Handle pre-commit failure

If any check fails, identify and remove the bad merge:

1. List merge commits on the integration branch since `main`:

```bash
git log main..HEAD --merges --reverse --format="%H %s"
```

2. Test each merge point by checking it out (read-only, no destructive ops).
   Stash any local state with a labeled entry, but only if the tree is dirty
   — an unconditional `git stash` on a clean tree creates no entry, and the
   later `git stash pop` would then pop an unrelated prior stash:

```bash
STASH_REF=""
if [[ -n "$(git status --porcelain)" ]]; then
  git stash push -m "batch-fix-bisect-${INTEGRATION_BRANCH}"
  STASH_REF="$(git rev-parse stash@{0})"
fi
# For each merge commit hash, from oldest to newest:
git checkout <merge-commit-hash>
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

3. The first commit where the gate fails is the culprit. Return to the
   integration branch and reset to just before it:

```bash
git checkout <INTEGRATION_BRANCH>
git reset --hard <parent-of-bad-merge>
```

4. Re-apply subsequent good merges by cherry-picking or re-merging their
   source branches (skip the bad one).

5. Log the culprit issue as FAILED with reason "pre-commit failure after
   merge with other fixes".

6. Re-run the gate to confirm the branch is clean.

If the gate fails on the very first merge (no prior good state), reset to
`main`, log that issue as FAILED, and re-merge the remaining successful
branches.

If the gate still fails after removing all suspect merges, something is
fundamentally wrong -- abort and report to the user.

**Recovery from interruption**: If the bisection is interrupted mid-sequence
(timeout, context exhaustion), return to the integration branch and restore
the labeled stash entry — only if one was actually pushed:

```bash
git checkout <INTEGRATION_BRANCH>
if [[ -n "$STASH_REF" ]]; then
  # Resolve the stash by its captured ref to avoid popping an unrelated entry
  git stash pop "$STASH_REF"
fi
```

---

## Step 7: Summary

Print a summary table:

```
## Batch Fix Results
Branch: <INTEGRATION_BRANCH>
Isolation: <worktree | branch>
Mode: <parallel|sequential>

### Processing Plan
<wave plan from Step 3>

### Succeeded
| # | Issue | Title | Module | Quick-win | Wave | Commit | Files Changed |
|---|-------|-------|--------|-----------|------|--------|---------------|

### Skipped
| # | Issue | Title | Reason |
|---|-------|-------|--------|

### Failed
| # | Issue | Title | Reason |
|---|-------|-------|--------|

### Statistics
- Issues attempted: N
- Succeeded: N
- Skipped: N
- Failed: N
- Waves executed: N (M parallel, K sequential)
- Total commits on integration branch: N
```

Remind the user: "Integration branch `<INTEGRATION_BRANCH>` is ready for your
review. Merge to main when satisfied, or push to open a PR."

---

## Agent Prompt

**BEGIN AGENT PROMPT**

You are fixing GitHub issue #<ISSUE_NUMBER>: <ISSUE_TITLE>

Issue body:

```
<ISSUE_BODY>
```

You must complete the full fix lifecycle: investigate, implement, simplify,
review, remediate, validate, commit. Do NOT close the GitHub issue -- only
annotate it. The `Fixes #N` commit trailer will close it on merge.

### Setup — Environment Verification (MANDATORY)

Determine your isolation mode:

```bash
PROJECT_ROOT="$(git rev-parse --show-toplevel)"
if [[ "$PROJECT_ROOT" == *".claude/worktrees/"* ]]; then
  ISOLATION_MODE="worktree"
else
  ISOLATION_MODE="branch"
fi
echo "ISOLATION_MODE=$ISOLATION_MODE PROJECT_ROOT=$PROJECT_ROOT"
```

Verify your branch:

```bash
AGENT_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
echo "AGENT_BRANCH=$AGENT_BRANCH"
```

**HARD GATE**: If `AGENT_BRANCH` is `main`, `master`, or `HEAD` (detached),
abort immediately — do NOT investigate, do NOT edit any files. Return:

```
STATUS: FAILED
REASON: Agent is on disallowed branch. AGENT_BRANCH=<branch>
ATTEMPTED: Setup verification only — no changes made.
```

Record `AGENT_BRANCH` — you will verify it again before committing.

**BRANCH SAFETY**: Do NOT switch branches. Do NOT run `git checkout`,
`git switch`, or `git checkout -b`. All commits must land on this branch.

In worktree mode: ALL file operations must be within `PROJECT_ROOT`. Per
`.claude/rules/worktree-safety.md`, never delete worktrees, never `cd` to
the main repo, never write outside your worktree.

In branch mode: the orchestrator has verified a clean repo before launching you.

If Serena (or another LSP-based code intelligence MCP) is reachable, call
its `activate_project` tool with the `git-remote-object-store` project so
symbol-level navigation and editing are the default. If Serena is
unavailable, use text-based tools (Read, Edit, Grep, Glob). Per
`.claude/rules/tool-choice.md`, never use legacy `grep`/`find` -- use the
Grep/Glob tools, `rg`, or `fd`.

### Phase 1: Investigate and Fix

Follow the `/fix-issue` workflow:

1. Re-read the project conventions that govern this fix:
   - `AGENTS.md` — upstream-as-source-of-truth and greenfield rules.
   - `execution-plan.md` — §0 goals/non-goals, §3 URL grammar, §6 resolved
     decisions.
   - `.claude/rules/rust.md`, `.claude/rules/naming.md`,
     `.claude/rules/testing.md`, `.claude/rules/git-commits.md`,
     `.claude/rules/worktree-safety.md`, `.claude/rules/protocol-stdout.md`.
2. Read `docs/development/lessons_learned.md` -- check whether any lesson
   applies to this issue's domain.
3. Investigate the codebase to understand the root cause. For any behavior
   that touches the on-the-wire object layout, locking semantics, LFS
   transfer protocol, or management-CLI shape, **read the upstream Python at
   `../git-remote-s3` first** — that is the authoritative behavior. Only
   diverge where `execution-plan.md` already documents the divergence;
   otherwise stop and report FAILED with reason "undocumented divergence
   required -- needs user decision".
4. Use Serena LSP tools (`find_symbol`, `get_symbols_overview`,
   `find_referencing_symbols`) for code navigation. Fall back to Read/Grep
   if Serena is unavailable. Before changing any public API, run
   `find_referencing_symbols` to enumerate every call site.
5. Check for the same bug pattern elsewhere in the codebase. If the root
   cause is repeated, fix all instances.
6. **Plan the fix using sequential thinking.** Use the
   `sequential-thinking:sequentialthinking` MCP tool to reason through the
   resolution step by step before writing any code. The sequential thinking
   process MUST:
   - **Start** with `thoughtNumber: 1`, an initial `totalThoughts` estimate
     (typically 5-8), and `nextThoughtNeeded: true`.
   - **Analyze** the root cause — not just the symptom. Trace the
     data/control flow that leads to the bug.
   - **Enumerate approaches** and evaluate trade-offs (simplicity,
     correctness, performance, scope).
   - **Identify edge cases** — empty inputs, boundary values, concurrent
     access, error paths, S3-vs-Azure backend differences, partial-multipart
     failures, network retries, lock contention. Walk through each edge case
     and confirm the proposed fix handles it.
   - **Cross-check against project rules** — if the fix would introduce a
     silent `unwrap_or_default`, an `unwrap()`/`expect()`/`panic!()` in
     non-test code, an unexplained abbreviation, a `to_string_lossy()` on an
     identifier path, a `splitn` on an untrusted empty pattern, an
     `unsafe` block, a `println!`/`dbg!` in a helper binary's protocol
     code path, or any of the other anti-patterns called out in
     `.claude/rules/rust.md`, `.claude/rules/naming.md`, or
     `.claude/rules/protocol-stdout.md`, redesign before proceeding.
   - **Verify completeness** — confirm the plan covers implementation,
     tests, and documentation before concluding.
   - **Conclude** with `nextThoughtNeeded: false` and a final plan summary.
   - Adjust `totalThoughts` up or down as understanding evolves. Use
     `isRevision` if earlier reasoning needs correction.
7. **Implement the fix.** Execute the plan from step 6. If the
   implementation reveals issues the plan missed, revise via sequential
   thinking before proceeding.
8. Self-review the implementation:
   - Correctness: root cause addressed, not just symptom? Match upstream
     Python behavior where required?
   - Performance: appropriate algorithms and data structures? Hot paths
     (push/fetch on large histories, multipart transfers) considered?
   - Simplicity: simplest fix that solves the problem? No speculative
     abstractions, no backwards-compat shims (forbidden in this greenfield
     project per `AGENTS.md`).
   - Completeness: edge cases handled? Similar patterns elsewhere?
   - Test coverage: regression tests added? Assertions specific?
   - Lessons learned: does the fix repeat any known anti-pattern?
9. Fix any issues found in self-review. If fixes were non-trivial, re-review.
10. **Write tests.** Sufficient testing is mandatory before proceeding. At
    minimum:
    - **Unit tests**: for all new or changed public functions. Each edge
      case identified in step 6 should have a corresponding test.
    - **Integration tests**: for end-to-end behavior changes (in `tests/`).
      Always run `cargo build` before integration tests so they exercise
      the new binary — never test against a stale binary
      (`.claude/rules/testing.md`).
    - **Cross-backend coverage**: if the fix touches storage, exercise both
      the S3 and Azure Blob paths via `MockStore`/the trait abstraction (or
      document why one is N/A).
    - **Regression check**: `cargo test --workspace` must pass.
    - Tests must actually assert what they claim — no silent fallbacks
      (`unwrap_or_else` that swallows failures), no missing assertions, no
      coupling to incidental host/filesystem details.
11. **Update agent-local documentation.** Review and update each of the
    following as applicable:
    - `README.md` — if user-facing behavior, install steps, or supported
      backends changed.
    - Crate-level or module-level doc comments (`//!`) — if module intent
      or architecture changed.
    - Avoid hardcoding stale counts in any doc per
      `.claude/rules/documentation.md` ("all tests passing", not "42 tests
      passing").
    - Run `markdownlint-cli2` against any Markdown files touched.
12. Do NOT update `CHANGELOG.md` or `execution-plan.md` -- the
    orchestrator consolidates these shared files after merging to avoid
    merge conflicts between parallel agents. Include the changelog entry
    text and any divergence notes in your Phase 7 result instead.

### Phase 2: Simplify

<!-- Adapted from /simplify-rust -- keep in sync -->

Review the diff (`git diff HEAD`) across three dimensions and apply fixes
directly:

**Reuse**:

- Repeated conversion code that should be a `From`/`TryFrom` impl
- Copy-pasted validation or formatting logic across functions
- Manual error mapping chains replaceable by a single `From` impl
- Identical match arms that can be consolidated

**Clarity**:

- `unwrap()` / `expect()` / `panic!()` / `assert!()` in non-test code --
  must use `?` or `Result`/`Option` (per `.claude/rules/rust.md`)
- Complex nested `if`/`match` that can be flattened with early returns or `?`
- Boolean flags or stringly-typed APIs that should be enums or newtypes
- `pub` items that should be `pub(crate)` (not used outside the crate)
- Functions longer than ~40 lines that mix unrelated concerns
- Redundant type annotations the compiler can infer
- `to_string_lossy()` on paths used as identifiers
- Numeric literals missing underscore separators

**Efficiency**:

- `.clone()` where a borrow suffices
- `String` parameters where `&str` works
- Unnecessary `.collect()` into intermediate `Vec`
- Missing `with_capacity()` for collections built in loops

Do NOT: extract tiny helpers that obscure flow, simplify clear `for` loops
into unreadable iterator chains, or add lifetime annotations the compiler
infers.

Run `cargo check --workspace --all-targets` to verify changes compile after
simplification.

### Phase 3: Review and Remediate

<!-- Adapted from /review -- keep in sync -->

Review the cumulative diff (`git diff HEAD`) against this checklist. Read
each changed file in full for context.

**Correctness**:

- Off-by-one errors in ranges, indexes, slices
- Unreachable match arms or dead branches after the change
- Error cases silently swallowed
- Edge cases: empty input, single element, `None`, non-UTF-8 paths,
  S3-vs-Azure differences
- Changed behavior for existing callers
- Upstream-Python parity preserved for on-the-wire layout, locking, LFS

**Performance**:

- Unnecessary allocations in hot paths (push/fetch, multipart)
- O(n^2) where O(n) is feasible
- Repeated work that should be hoisted out of loops

**Security**:

- Path traversal risk
- `to_string_lossy()` on identifier paths
- Injection risks (URL parsing, ref names, OIDs)
- Stdout discipline in helper binaries (per
  `.claude/rules/protocol-stdout.md`): no `println!`/`dbg!`/banners on
  stdout outside intentional protocol output

**Tests**:

- Every new code path has a corresponding test
- Existing tests still cover their intended scenarios
- Test assertions are specific (not just `is_ok()`)
- Missing negative tests for error paths

For each finding, classify severity and effort:

- **bug** or **security** -> fix immediately
- **performance** or **code-smell** (medium+ effort) -> fix if safe
- **trivial code-smell** or **test-gap** -> fix if trivial, note otherwise

Fix all actionable findings. If fixes were non-trivial, re-review the new
diff. Do NOT proceed with known bugs or security issues.

### Phase 4: Validate

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

If any check fails on code you changed, fix and retry (one attempt).
If it fails again or fails on code you did not change, report as FAILED.

If you touched any Markdown files, also run:

```bash
markdownlint-cli2 <files-touched>
```

### Phase 5: Commit

Before committing, verify you are still on your agent branch:

```bash
CURRENT_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [ "$CURRENT_BRANCH" != "$AGENT_BRANCH" ]; then
  echo "ERROR: Branch drift detected. Expected $AGENT_BRANCH, on $CURRENT_BRANCH"
  git checkout -- .
  git reset HEAD
  # Report FAILED
fi
```

Verify what will be staged:

```bash
git status
git diff HEAD --stat
```

Stage only files you intentionally changed (do NOT use `git add -A`):

```bash
git add <file1> <file2> ...
```

Commit using Conventional Commits (`.claude/rules/git-commits.md`). The
`Fixes #N` trailer is REQUIRED in the **body** (not the subject) — it will
close the issue when the branch is merged. Do not add `Co-Authored-By` lines.

```bash
git commit -m "$(cat <<'EOF'
<type>(<scope>): <subject>

<body explaining what and why, 72-char lines>

Fixes #<ISSUE_NUMBER>
EOF
)"
```

Record the branch name, commit hash, and what changed:

```bash
git rev-parse --abbrev-ref HEAD
git rev-parse --short HEAD
git show --stat HEAD
```

### Phase 6: Annotate GitHub Issue

Update the GitHub issue with research findings and fix details. Do NOT
close the issue -- the `Fixes #N` commit trailer handles closure on merge.

Update BOTH the issue body AND add a comment (per
`.claude/rules/github-cli.md`: use `--body-file` with a temp file for
non-trivial bodies):

```bash
cat > /tmp/issue-comment-<ISSUE_NUMBER>.md <<'COMMENT_EOF'
## Fix Summary

**Root cause**: <what was wrong and why>

**Changes**:
- <file>: <what changed>
- ...

**Tests**: <what test coverage was added>

**Commit**: <hash> on branch <branch-name>

<any additional notes, follow-up items, or related issues>
COMMENT_EOF
gh issue comment <ISSUE_NUMBER> --body-file /tmp/issue-comment-<ISSUE_NUMBER>.md
```

Also update the issue body to reflect the fix status:

```bash
gh issue view <ISSUE_NUMBER> --json body --jq '.body' > /tmp/issue-body-<ISSUE_NUMBER>.md
# Append fix status to the issue body
cat >> /tmp/issue-body-<ISSUE_NUMBER>.md <<'BODY_EOF'

---

## Resolution

**Status**: Fixed (pending merge)
**Commit**: <hash> on branch <branch-name>
**Root cause**: <brief summary>
BODY_EOF
gh issue edit <ISSUE_NUMBER> --body-file /tmp/issue-body-<ISSUE_NUMBER>.md
```

### Phase 7: Report Result

Return EXACTLY one of:

**SUCCESS**:

```
STATUS: SUCCESS
BRANCH: <branch-name>
COMMIT: <short-hash>
FILES: <number of files changed>
SUMMARY: <one-line description of the fix>
CHANGELOG: <changelog entry text, e.g. "Fixed incorrect URL host validation in url::parse">
DIVERGENCE: <divergence note if applicable, or "none" — only set if execution-plan.md needs an update>
LESSON: <hard-won, globally reusable lesson if any, or "none" — the orchestrator collects these and proposes them to the user after all waves complete>
```

**SKIPPED** (issue is invalid, already fixed, or requires no code changes):

```
STATUS: SKIPPED
REASON: <why no changes were needed>
```

**FAILED**:

```
STATUS: FAILED
REASON: <what went wrong>
ATTEMPTED: <what was tried before failure>
```

On FAILED, ensure no uncommitted changes remain in tracked files:

```bash
git checkout -- .
git reset HEAD
```

Do NOT run `git clean -fd` -- the worktree runtime manages untracked files.

**END AGENT PROMPT**

---

## Guardrails

- Do NOT merge the integration branch into `main` -- leave for the user
- Do NOT close GitHub issues -- the `Fixes #N` trailer handles this on merge
- Do NOT use `git push --force` or any destructive git operations
- Do NOT delete worktrees -- only the Claude Code runtime may do that
  (per `.claude/rules/worktree-safety.md`)
- Do NOT skip the review phase -- every fix must be reviewed before commit
- If a worktree agent cannot safely resolve an issue, it must report FAILED
- Parallel agents in the same wave MUST touch different modules -- same-module
  issues are always serialized across waves
- Each worktree agent is fully self-contained -- it does not call /fix-issue,
  /simplify-rust, or /review as skills. The logic is embedded in the prompt.
- Undocumented divergence from upstream `git-remote-s3` is forbidden per
  `AGENTS.md` -- agents that need to diverge must report FAILED instead.

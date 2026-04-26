---
name: audit
description: Audit the git-remote-object-store crate for logic errors, complexity, bugs, security issues, and code smells. Use when asked to audit or review the crate.
---

# Audit Crate

Audit the Rust crate `$ARGUMENTS` for logic errors, unnecessary complexity,
bugs, security issues, incorrect comments, and code smells.

If `$ARGUMENTS` is empty, default to `git-remote-object-store` — this project
is a single-crate workspace, so the crate name is the package name in the root
`Cargo.toml`. Treat the project root (`git rev-parse --show-toplevel`) as the
crate root.

**Resolve `$ARGUMENTS` once at the very start of the run** and use the
resolved value for every subsequent reference (memory keys, `cargo -p`, issue
titles). Never let an unresolved or empty `$ARGUMENTS` reach a template like
`audit-state-$ARGUMENTS` — that would write to a malformed memory key.

## ABSOLUTE CONSTRAINTS

**This skill is READ-ONLY. It MUST NOT leave any trace on the filesystem.**

- **NEVER commit code.** No `git commit`, no `git add`, no staging. Zero commits.
- **NEVER leave uncommitted files.** No new files, no modified files, no temp files
  in the worktree. If you accidentally create or modify a file, revert it
  immediately with `git checkout -- .`.
- **NEVER modify source files.** Not even "harmless" formatting or comment fixes.
- **NEVER push branches.** The isolation branch is disposable and local-only.
- The ONLY side effects of this skill are: GitHub issues filed, Serena memories
  updated, and terminal output printed.

---

## Before Starting

Check existing open GitHub issues (`gh issue list`) to avoid filing duplicates.
Note any relevant open issues as context, but do NOT let them constrain the audit.

---

## Step 0: Launch isolated agent

**This step is MANDATORY and must be the very first action.**

The audit runs in isolation to guarantee the main working tree is never touched.

### Environment detection

Determine the isolation mode:

```bash
PROJECT_ROOT="$(git rev-parse --show-toplevel)"
if [[ "$PROJECT_ROOT" == *".claude/worktrees/"* ]]; then
  ISOLATION_MODE="worktree"
else
  ISOLATION_MODE="branch"
fi
```

- **Worktree mode**: You are already inside a worktree. Keep all existing
  behavior (agent launched with `isolation: "worktree"`).
- **Branch mode**: You are in the main project directory. Agents run without
  worktree isolation. Serena LSP works correctly in this mode.

### Branch mode prerequisites

In branch mode, verify the working tree is completely clean before proceeding:

```bash
if [[ "$ISOLATION_MODE" == "branch" ]]; then
  DIRTY="$(git status --porcelain)"
  if [[ -n "$DIRTY" ]]; then
    echo "Error: branch mode requires a clean repository (no uncommitted or untracked files)." >&2
    echo "$DIRTY" >&2
    exit 1
  fi
fi
```

### Launch the agent

If you are the top-level orchestrator (invoked by the user), immediately launch
a single Agent that executes Steps 1-9. Pass the full crate name and any prior
context. Do NOT perform any audit work directly.

- **Worktree mode**: Launch the Agent with `isolation: "worktree"`. This creates
  an isolated worktree automatically.
- **Branch mode**: Launch the Agent WITHOUT `isolation: "worktree"`. The agent
  runs in the main project directory (safe because the audit is read-only).

**CRITICAL**: In worktree mode, the Agent tool call MUST include
`isolation: "worktree"` as a required parameter. Double-check before sending.
In branch mode, do NOT include `isolation: "worktree"`.

If sub-agents are used (e.g., to audit file groups in parallel), they inherit
the parent context and do NOT need their own `isolation: "worktree"` -- the
audit is read-only, so concurrent reads are safe.

### Agent verification (MANDATORY)

Every agent (top-level or sub-agent) must verify its environment before doing
any work:

```bash
PROJECT_ROOT="$(git rev-parse --show-toplevel)"
if [[ "$PROJECT_ROOT" == *".claude/worktrees/"* ]]; then
  ISOLATION_MODE="worktree"
else
  ISOLATION_MODE="branch"
fi
echo "ISOLATION_MODE=$ISOLATION_MODE PROJECT_ROOT=$PROJECT_ROOT"
```

**In worktree mode**: Confirm `PROJECT_ROOT` contains `.claude/worktrees/`.
If not, abort:
"ABORTED: Agent expected worktree isolation but PROJECT_ROOT=\<path\>"

**In branch mode**: Confirm the working tree is clean (`git status --porcelain`
returns empty output). If dirty, abort:
"ABORTED: Branch mode requires a clean working tree."

In worktree mode, all file operations must be within `PROJECT_ROOT`.

**Worktree cleanup**: Worktrees are automatically cleaned up by the Claude Code
runtime. NEVER run `git worktree remove` or `git worktree prune`.

---

## Step 1: Load audit history

Read the Serena memory `audit-state-$ARGUMENTS` to check for prior audit state.
Invoke the Serena MCP tool `serena:read_memory` directly (the project does not
use a wrapper) with `memory_name: "audit-state-$ARGUMENTS"`. If the Serena MCP
server is not active for this session, call `serena:activate_project` first —
see `.claude/skills/fix-issue/SKILL.md` for the same pattern.

If the memory exists, it contains a per-file coverage table in this format:

```
# Audit State: $ARGUMENTS
last_audit: YYYY-MM-DD
last_model: <model-id>

## File Coverage
<relative_path> | <depth> | <date> | <findings_count> findings | <model-id>
```

Where `<depth>` is one of: `full`, `partial`, `skimmed`, `none`.

Parse the table and use it to **prioritize files** in Step 2:

| Priority | Condition | Action |
|----------|-----------|--------|
| 1 (highest) | `none` — never audited | Deep audit required |
| 2 | `skimmed` — quick scan only | Deep audit required |
| 3 | `partial` AND older than 30 days | Audit uncovered areas |
| 4 | `partial` AND recent | Spot-check, focus on new changes |
| 5 (lowest) | `full` AND recent (< 3 days) | Skip unless file changed since last audit |

New files not in the memory table default to priority 1.

If the memory does not exist, treat all files as priority 1 (first audit).

---

## Step 2: Discover scope

### 2a: Build baseline

Because this project is a single-crate workspace, `-p $ARGUMENTS` is optional —
plain `cargo build` works. Use `-p` for clarity and to make commands portable
if the workspace later grows.

```bash
cargo build -p $ARGUMENTS 2>&1 | tail -5
cargo test  -p $ARGUMENTS 2>&1 | tail -20
cargo clippy -p $ARGUMENTS -- -W clippy::all 2>&1 | tail -20
```

Record: passing test count, existing warnings, existing clippy findings.
Anything already broken is NOT your problem to fix — note it as context only.

### 2b: Collect code metrics

If a code-metrics tool (e.g., `utils/code-metrics.sh`) is available, run it
scoped to the crate directory. This produces cyclomatic/cognitive complexity
hotspots, Halstead defect estimates, function size outliers, and maintainability
index scores — all of which direct the audit to the highest-risk code.

```bash
CRATE_DIR=$(cargo metadata --format-version 1 --no-deps \
  | jq -r --arg name "$ARGUMENTS" \
    '.packages[] | select(.name == $name) | .manifest_path' \
  | xargs dirname)

if [[ -x utils/code-metrics.sh ]]; then
  utils/code-metrics.sh --path "$CRATE_DIR" --top 10
fi
```

Parse the output (when available) and extract:

- **Halstead estimated bugs > 0.5**: These functions have the highest
  probability of containing defects. Audit them at depth `full` regardless
  of prior audit state.
- **Cyclomatic complexity > 10**: Complex branching increases the chance of
  missed edge cases. Cross-reference with checklist questions 1-6 (logic
  and correctness).
- **Cognitive complexity > 15**: Hard-to-understand code is where incorrect
  comments (checklist 15-18) and swallowed errors (checklist 4) hide.
- **Functions > 100 SLOC or > 3 parameters**: Code smell candidates for
  checklist questions 19-24.
- **Maintainability Index < 10**: These files need deep review — changes to
  them carry disproportionate regression risk.

Carry this data forward to Steps 3-4. When applying the audit checklist, start
with the functions flagged by metrics before scanning the rest of the file.

If the metrics tool is unavailable, skip this substep and proceed — the audit
can still run without metrics.

### 2c: Map crate layout

Then locate and map the crate layout. Use `fd` per `.claude/rules/bash.md`
(detect `fdfind` on Debian/Ubuntu):

```bash
FD=$(command -v fd 2>/dev/null || command -v fdfind 2>/dev/null || true)
if [[ -z "$FD" ]]; then
  echo "error: fd (or fdfind) not found." >&2
  exit 1
fi

"$FD" --type f . "$CRATE_DIR/src"
[[ -d "$CRATE_DIR/tests" ]] && "$FD" --type f . "$CRATE_DIR/tests"
```

Group every file into one of these categories before auditing:

| Group | Contents |
|-------|----------|
| A — Library core | `src/lib.rs` and all modules it uses (`src/protocol/`, `src/object_store/`, `src/lfs/`, `src/manage/`, `src/url.rs`, `src/git.rs`) |
| B — Binaries | `src/bin/` entries (helper binaries `git-remote-*`, LFS transfer agent, management CLI) |
| C — Tests | `tests/` directory |
| D — Supporting files | `README.md`, examples, `Cargo.toml`, `.claude/rules/` |

**Use the priority table from Step 1** to order files within each group. Audit
highest-priority files first. For a focused session, you may skip priority-5
files entirely and note them as "skipped (recently audited)".

Read each file **entirely** before answering checklist questions for that group.

**Track your depth**: As you audit each file, note the depth of review:

- **full**: Read every function, applied all applicable checklist questions
- **partial**: Reviewed key public APIs and complex logic, skipped straightforward code
- **skimmed**: Quick scan for obvious issues only (acceptable for low-priority files)

---

## Step 3: Audit checklist

Apply every applicable question to every file in scope. **Start with functions
flagged by code metrics** (Step 2b) — these have the highest defect probability.
For each file, audit metric-flagged functions first, then scan the remainder.

Record each finding as:

```
FINDING: <short title>
FILE: <path>:<line range>
CHECKLIST: <question number(s)>
EVIDENCE: <what is wrong and why, with code snippet>
CATEGORY: bug | security | enhancement | documentation
```

Use exactly these four category names — they are the GitHub labels Step 5
will apply, so the vocabulary must match end-to-end. Mapping rules:

- **bug** — incorrect behavior, off-by-one, swallowed errors, broken contracts
- **security** — anything in the Security checklist (Q9-Q14)
- **enhancement** — code smells, refactors, complexity, dependency hygiene
- **documentation** — incorrect or stale comments, doc-tests, missing docs

### Logic and Correctness

1. Does every code path produce the correct output for its documented contract?
2. Are there off-by-one errors in ranges, indexes, or boundary checks?
3. Are there match arms, if-else branches, or rule bodies that are unreachable or logically dead?
4. Are error cases handled, or silently swallowed (`continue`, `_ => {}`, `Err(_)` arms, ignored `Result`)?
5. Are there false positives on valid inputs, or false negatives on invalid inputs?
6. Are all edge cases handled: empty input, max-size input, non-UTF-8 paths, missing files?
7. Is recursion bounded? Can deeply nested input cause stack overflow?
8. For helper-protocol (`src/bin/git-remote-*.rs`) and LFS-transfer
   (`src/bin/git-lfs-*.rs`) code paths: does any production code write to
   stdout outside the protocol contract (see `.claude/rules/protocol-stdout.md`)?
   Concretely, flag: any `println!`/`print!`/`dbg!` in non-test code; any
   `tracing-subscriber` initializer (in `main()` or a shared init helper) that
   does not call `.with_writer(std::io::stderr)`; any direct `writeln!(stdout(), ...)`
   outside intentional protocol output.

### Security

9. Can a malicious or pathological input cause stack overflow, infinite loop, or OOM?
10. Is there path traversal risk in any directory-walking or file-loading logic?
11. Are symlinks handled safely?
12. Is `to_string_lossy()` safe for all file path handling, or could non-UTF-8 paths cause silent corruption? (See `.claude/rules/rust.md` — never `to_string_lossy()` for paths used as identifiers.)
13. Does any feature or API allow user-supplied content to execute arbitrary code or make network calls?
14. Are there hardcoded secrets, tokens, or environment-specific paths?

### Incorrect Comments and Documentation

15. Do doc comments on public items accurately describe behavior, parameters, return values, and errors?
16. Are inline comments factually correct and not stale (old architectures, wrong line numbers, removed features)?
17. Do doc-test examples compile and run correctly?
18. Are there TODO/FIXME/HACK comments indicating known tech debt?

### Code Smells and Unnecessary Complexity

19. Are there duplicated helper functions across test files that should be in a shared module?
20. Are there overly verbose patterns that could be simplified (unnecessary clones, verbose match arms)?
21. Are there unused imports, functions, or struct fields?
22. Are there functions longer than ~50 lines of logic that should be decomposed?
23. Are there hardcoded strings that should be named constants?
24. Are any hardcoded lists a maintenance hazard (e.g., a list of S3/Azure backend
    error codes that must stay in sync with upstream)?

### Dependency and Build Concerns

25. Are all `Cargo.toml` dependencies necessary? Are any runtime dependencies only used by a binary or optional feature?
26. Are feature flags on dependencies minimal and appropriate?
27. Is `default-features = false` used where the full default feature set is not needed?

### Project-Specific (git-remote-object-store)

28. Does any change diverge from the upstream `git-remote-s3` Python implementation
    in a way **not** documented in `execution-plan.md` §0/§3/§6? Per `AGENTS.md`,
    upstream is the source of truth for behavior, on-the-wire object layout,
    locking semantics, LFS transfer protocol, and management-CLI command shapes.
29. Does any code break the on-bucket object-layout invariant (`<prefix>/<ref>/<sha>.bundle`,
    `HEAD`, `PROTECTED#`, lock files, `lfs/<oid>`)? This is the single
    backwards-compatibility contract the project preserves.
30. Are there `unwrap()`, `expect()`, `assert!()`, or `panic!()` calls in
    non-test code? `expect()`/`assert!()` are acceptable in tests; production
    code must propagate with `?`. (Per `.claude/rules/rust.md`.)

(Helper-binary stdout discipline is covered by Q8 in Logic and Correctness;
do not double-report it here.)

---

## Step 4: Deduplicate and validate

For each finding:

1. Re-read and confirm the evidence is concrete (file + line + reasoning).
2. Cross-check against existing open issues (`gh issue list`). Drop exact duplicates.
3. Confirm the category from Step 3 (`bug`, `security`, `enhancement`, or
   `documentation`) — this is the GitHub label Step 5 will apply.

---

## Step 5: File GitHub issues

### 5a: Ensure category labels exist

Before filing the first issue, ensure every label the audit may use exists in
the repo. `gh issue create` rejects unknown labels. Of the four categories,
`bug`, `documentation`, and `enhancement` already exist; `security` and
`upstream-blocked` need to be created on first use:

```bash
ensure_label() {
  local name="$1" color="$2" desc="$3"
  if ! gh label list --limit 200 | awk '{print $1}' | grep -qx "$name"; then
    gh label create "$name" --color "$color" --description "$desc"
  fi
}

ensure_label security        "ee0701" "Security-relevant finding"
ensure_label upstream-blocked "fbca04" "Cannot be fixed locally; needs upstream coordination"
```

### 5b: Create the issue

For each surviving finding, create one issue. Template:

```markdown
## Summary

<One-sentence description of the problem>

## Location

- `<file path>:<line numbers>`

## Evidence

<Code snippet or reasoning showing the problem>

## Expected Behavior

<What should happen instead>

## Actual Behavior

<What currently happens>

## Impact

<Who is affected and how>
```

Write the body to a temp file and use `gh issue create --body-file`:

```bash
cat > /tmp/issue-body.md <<'EOF'
...body here...
EOF
gh issue create --title "crate: short description" --label "bug" --body-file /tmp/issue-body.md
```

### Upstream-blocked findings

Some findings cannot be fixed in this repository because they require changes
to behavior owned by an upstream project. For `git-remote-object-store`, the
relevant upstream is `awslabs/git-remote-s3` (checked out as a sibling at
`../git-remote-s3`). The on-bucket object layout, locking semantics, and LFS
transfer protocol must remain compatible with upstream so existing buckets
remain readable.

When a finding falls into this category:

1. **Still file the issue** — the problem is real and should be tracked.
2. Add the `upstream-blocked` label alongside the normal category label
   (`bug`, `security`, etc.).
3. In the issue body, add an `## Upstream Ownership` section that names the
   upstream project and explains why a local fix is not possible.
4. Do NOT add a fix plan in Step 6 that assumes a local fix; instead, the
   fix-plan comment should describe the upstream coordination required
   (file an issue against `awslabs/git-remote-s3`, document the divergence
   in `execution-plan.md`, etc.).

Example:

```bash
gh issue create \
  --title "object-store: bundle path naming diverges from upstream" \
  --label "bug" \
  --label "upstream-blocked" \
  --body-file /tmp/issue-body.md
```

Signals that an upstream block applies include:

- Finding would change the on-bucket object layout (`<prefix>/<ref>/<sha>.bundle`,
  `HEAD`, `PROTECTED#`, lock files, `lfs/<oid>`).
- Finding would change the LFS custom-transfer JSON event shape.
- Finding would change locking semantics across cooperating clients.
- Fix would cause behavioral divergence from `awslabs/git-remote-s3` not
  already documented in `execution-plan.md` §0/§3/§6.

---

## Step 6: Add fix-plan comments

For each issue filed, add a comment:

```markdown
## General Plan for Fix

### Root Cause

<Why the problem exists>

### Implementation Steps

1. <Smallest change that fixes the problem>
2. <Next step if needed>

### Tests to Add/Update

- <Test 1>
- <Test 2>

### Verification

- [ ] `cargo test -p $ARGUMENTS` passes
- [ ] `cargo clippy -p $ARGUMENTS` clean
- [ ] Manual verification: <specific scenario>
```

```bash
cat > /tmp/fix-plan.md <<'EOF'
...plan here...
EOF
gh issue comment <NUMBER> --body-file /tmp/fix-plan.md
```

---

## Step 7: Summary table

Print to the terminal:

```
| # | Title | Category | File | Issue |
|---|-------|----------|------|-------|
```

Also list any findings dropped as duplicates of existing issues.

---

## Step 8: Save audit state

After completing the audit, persist the coverage state to Serena memory by
invoking the Serena MCP tool `serena:write_memory` directly with
`memory_name: "audit-state-$ARGUMENTS"` and the merged content as `content`.

Build the memory content by **merging** the previous state (from Step 1) with
the current session's coverage. Rules for merging:

- For files audited in this session: update depth, date, model, and findings count
- For files NOT audited in this session: preserve the existing record unchanged
- For files that no longer exist in the crate: remove them from the table
- For new files discovered but not audited: add them with depth `none`

Format the memory as:

```
# Audit State: $ARGUMENTS
last_audit: YYYY-MM-DD
last_model: <model-id>

## File Coverage
src/lib.rs | full | 2026-04-25 | 3 findings | claude-opus-4-7
src/url.rs | partial | 2026-04-25 | 1 finding | claude-opus-4-7
src/protocol/mod.rs | none | - | - | -
tests/url_parsing.rs | full | 2026-04-25 | 0 findings | claude-sonnet-4-6
```

Each line: `<relative_path> | <depth> | <date> | <findings_count> findings | <model-id>`

Where:

- `<relative_path>` is relative to the crate root
- `<depth>` is `full`, `partial`, `skimmed`, or `none`
- `<date>` is YYYY-MM-DD of last audit (or `-` if never)
- `<findings_count>` is the number of issues filed for that file (or `-` if never audited)
- `<model-id>` is the AI model that performed the audit (or `-` if never audited)

The `last_model` header records the model used in the most recent audit session.
Per-file model IDs record which model last audited each specific file, which may
differ across files if audits were performed across multiple sessions with
different models.

---

## Step 9: Verify clean working tree

**This step is MANDATORY and must be the last action before returning.**

Confirm that the working tree has zero modifications and zero new files:

```bash
git status --porcelain
```

If any output is shown, something went wrong — the audit was supposed to be
read-only.

- For **modifications to tracked files** (porcelain status `M`, staged or
  unstaged), revert with
  `git checkout -- <path>` per file. Do NOT use a blanket `git checkout -- .`
  in worktree mode without first verifying every listed path belongs to the
  audit (other agents may share the worktree per `.claude/rules/worktree-safety.md`).
- For **untracked files** (`??` lines), do NOT delete them automatically.
  Untracked output means the audit wrote a file it should not have. Surface
  the path(s) verbatim in the final summary so the user can decide whether
  to keep or remove them. Never run `git clean` from this skill.

Report the anomaly (which files, which step likely produced them) in your
final summary output.

---

## Guardrails

All ABSOLUTE CONSTRAINTS (top of this document) apply. Additionally:

- **NEVER delete worktrees.** Only the Claude Code runtime may do that.
- Do NOT implement fixes. This is audit-only.
- Do NOT file findings without concrete evidence (file + line + reasoning).
- Do NOT file duplicate issues. Always check `gh issue list` first.
- Use `--body-file` for all `gh issue create` and `gh issue comment` calls.
- In worktree mode, all audit work MUST happen inside the isolated worktree.
- In branch mode, audit work runs in the main directory (read-only, verified clean).
- Sub-agents share the parent context (read-only); they do NOT need separate isolation.

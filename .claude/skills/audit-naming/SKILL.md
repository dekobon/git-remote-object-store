---
name: audit-naming
description: Audit naming quality in the git-remote-object-store crate (or a directory) for misleading, inconsistent, or unclear names. Use when asked to audit or review naming.
---

# Audit Naming Quality

Audit naming quality in `$ARGUMENTS` for misleading, inconsistent, or unclear
names across Rust and Bash code. Distinct from `audit` (logic, security,
complexity) — this focuses exclusively on whether names tell the truth.

If `$ARGUMENTS` is empty, default to `git-remote-object-store` — this project
is a single-crate workspace, so the crate name is the package name in the root
`Cargo.toml`. Treat the project root (`git rev-parse --show-toplevel`) as the
crate root.

**Resolve `$ARGUMENTS` once at the very start of the run** and use the
resolved value for every subsequent reference (memory keys, `cargo -p`, issue
titles). Never let an unresolved or empty `$ARGUMENTS` reach a template like
`naming-audit-state-$ARGUMENTS` — that would write to a malformed memory key.

The authoritative project naming rules live in:

- `.claude/rules/naming.md` — universal + Rust naming conventions
- `.claude/rules/rust.md` — Rust-specific conventions (visibility, conversions, getters)
- `.claude/rules/bash.md` — Bash conventions

Use those files as the standard against which findings are measured. If you
encounter a possible finding that the project rules already permit, drop it.

## Hard constraint: this skill is read-only

The audit runs against a tree the user may be working in, so it must leave no
trace on the filesystem:

- No commits and no staging — no `git commit`, no `git add`.
- No modified or new files, including "harmless" formatting or comment fixes
  and temp files inside the worktree. If you create or modify a file by
  accident, revert it immediately with `git checkout -- .`.
- No pushed branches. The isolation branch is disposable and local-only.

The only side effects are GitHub issues filed, Serena memories updated (when
Serena is available), and terminal output.

---

## Step 0: Launch isolated agent

Do this before any other action. The audit runs in isolation so the main
working tree is never touched.

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
a single Agent that executes Steps 1–8. Pass the resolved scope (crate name or
directory path) and any prior context. Do NOT perform any audit work directly.

- **Worktree mode**: Launch the Agent with `isolation: "worktree"`. This creates
  an isolated worktree automatically.
- **Branch mode**: Launch the Agent WITHOUT `isolation: "worktree"`. The agent
  runs in the main project directory (safe because the audit is read-only).

In worktree mode the Agent tool call must include `isolation: "worktree"`;
in branch mode it must omit it. Getting this backwards either loses the
isolation guarantee or creates a nested worktree.

If sub-agents are used (e.g., to audit file groups in parallel), they inherit
the parent context and do NOT need their own `isolation: "worktree"` — the
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

Read the Serena memory `naming-audit-state-$ARGUMENTS` to check for prior audit
state. Invoke the Serena MCP tool `serena:read_memory` directly with
`memory_name: "naming-audit-state-$ARGUMENTS"`. If the Serena MCP server is not
active for this session, call `serena:activate_project` first — see
`.claude/skills/fix-issue/SKILL.md` for the same pattern.

If the memory exists, it contains a per-file coverage table in this format:

```
# Naming Audit State: $ARGUMENTS
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

## Step 2: Discover scope and run automated pre-checks

### Check for duplicate issues

Check existing open GitHub issues (`gh issue list --label naming`) to avoid
filing duplicates. Note any relevant open issues as context, but do NOT let
them constrain the audit.

### Resolve the target

Try `cargo metadata` first. If no crate matches, check if `$ARGUMENTS` is a
valid directory. If neither, abort with a clear error.

```bash
CRATE_NAME="$ARGUMENTS"
SCOPE_DIR=$(cargo metadata --format-version 1 --no-deps \
  | jq -r --arg name "$CRATE_NAME" \
    '.packages[] | select(.name == $name) | .manifest_path' \
  | xargs dirname 2>/dev/null)

if [[ -z "$SCOPE_DIR" ]]; then
  CRATE_NAME=""
  if [[ -d "$ARGUMENTS" ]]; then
    SCOPE_DIR="$ARGUMENTS"
  else
    echo "ERROR: '$ARGUMENTS' is not a known crate name or valid directory." >&2
    exit 1
  fi
fi
```

`CRATE_NAME` is set when the target is a crate (used for clippy), empty when
the target is a bare directory.

### Enumerate files

Find all `.rs`, `.sh`, and `.bash` files in scope. Use `fd` per
`.claude/rules/bash.md` (detect `fdfind` on Debian/Ubuntu):

```bash
FD=$(command -v fd 2>/dev/null || command -v fdfind 2>/dev/null || true)
if [[ -z "$FD" ]]; then
  echo "error: fd (or fdfind) not found." >&2
  exit 1
fi
"$FD" -e rs -e sh -e bash . "$SCOPE_DIR"
```

### Run automated naming lints first

Run linters to avoid duplicating their work in the manual audit:

```bash
# Rust: naming-related clippy lints (only when target is a crate)
if [[ -n "$CRATE_NAME" ]]; then
  cargo clippy -p "$CRATE_NAME" -- \
    -W clippy::module_name_repetitions \
    -W clippy::enum_variant_names \
    -W clippy::struct_field_names \
    -W clippy::similar_names \
    -W clippy::disallowed_names \
    -W clippy::wrong_self_convention 2>&1 | tail -30
fi

# Bash: shellcheck (only if any shell scripts are present)
if "$FD" -q -e sh -e bash . "$SCOPE_DIR" 2>/dev/null; then
  "$FD" -e sh -e bash . "$SCOPE_DIR" -x shellcheck {} 2>&1 | tail -20
fi
```

Note the output as context. The manual audit in Step 3 **MUST skip issues
already caught by linters** and focus on semantic naming quality that linters
cannot detect.

### Map file groups

Group files by language and role before auditing:

| Group | Contents |
|-------|----------|
| A — Library core | `src/lib.rs` and modules (`src/protocol/`, `src/object_store/`, `src/lfs/`, `src/manage/`, `src/url.rs`, `src/git.rs`, …) |
| B — Binaries | `src/bin/` entries (helper binaries `git-remote-*`, LFS transfer agent, management CLI) |
| C — Rust tests | `tests/**/*.rs` and `#[cfg(test)]` modules |
| D — Bash scripts | `*.sh`, `*.bash` files |

**Use the priority table from Step 1** to order files within each group. Audit
highest-priority files first. For a focused session, you may skip priority-5
files entirely and note them as "skipped (recently audited)".

Read each file **entirely** before applying checklist questions for that file.
For Rust files, prefer Serena LSP tools (`get_symbols_overview`, `find_symbol`,
`find_referencing_symbols`) over text-based reads — see `.claude/rules/tool-choice.md`.

**Track your depth**: As you audit each file, note the depth of review:

- **full**: Read every symbol, applied all applicable checklist questions
- **partial**: Reviewed public APIs and complex logic, skipped straightforward code
- **skimmed**: Quick scan for obvious issues only (acceptable for low-priority files)

---

## Step 3: Naming audit checklist

Apply every applicable question to every file in scope. Record each finding as:

```
FINDING: <short title>
FILE: <path>:<line range>
CHECKLIST: <check ID(s)> (e.g., U2, R1)
CURRENT NAME: <the problematic name>
EVIDENCE: <why the name misleads, with code context>
SUGGESTED NAME: <proposed alternative>
SEVERITY: misleading | inconsistent | unclear | convention
```

Severity definitions:

- **misleading**: Name actively implies wrong behavior, type, or purpose
- **inconsistent**: Same concept named differently across the codebase, or naming
  pattern applied unevenly
- **unclear**: Name requires reading the implementation to understand
- **convention**: Violates language-specific naming conventions — Rust API
  Guidelines and the project rules in `.claude/rules/naming.md`,
  `.claude/rules/rust.md`, and `.claude/rules/bash.md` (note: the project's
  Bash rule mandates uppercase variables for *all* variables, not just
  constants/env vars — do not import Google Shell Style Guide expectations)

### Universal Checks (all languages)

| ID | Check |
|----|-------|
| U1 | Name reveals intent without requiring a comment |
| U2 | Name does not mislead about behavior/type/purpose |
| U3 | No noise words for meaningless distinctions (`Data` vs `Info`) |
| U4 | Same concept uses same word everywhere (no `fetch`/`get`/`retrieve` mix) |
| U5 | Different concepts use different words |
| U6 | Name length proportional to scope (short names for tight scopes, descriptive names for wide scopes) |
| U7 | Boolean names are positive (no double negation like `not_disabled`) |
| U8 | No unexplained abbreviations (domain-standard abbreviations like `fd`, `pid`, `url`, `sha`, `oid`, `ref` are fine; per `.claude/rules/naming.md`) |
| U9 | Naming patterns applied consistently across similar entities |
| U10 | No linguistic antipatterns (name/behavior mismatch per Arnaoudova et al.) |
| U11 | Plural names hold collections; singular names hold single values (per `.claude/rules/naming.md` "Universal") |

### Rust-Specific Checks

Anchored in `.claude/rules/rust.md` and `.claude/rules/naming.md`.

| ID | Check |
|----|-------|
| R1 | Conversion prefixes match semantics: `as_` (free borrow), `to_` (expensive copy), `into_` (consuming), `from_` (constructor) — Rust API Guidelines C-CONV |
| R2 | Getters omit `get_` prefix (Rust API Guidelines C-GETTER) |
| R3 | `into_*` consumes self; `as_*` borrows (signature matches prefix) |
| R4 | Iterator methods follow `iter`/`iter_mut`/`into_iter` convention |
| R5 | Word order follows stdlib patterns (`ParseError` not `ErrorParse`) |
| R6 | `is_*`/`has_*` methods return `bool` |
| R7 | Struct/enum field names match their types semantically (no `count: String`, no `name: Vec<u8>` without justification) |
| R8 | Newtype wrappers used to enforce domain invariants where appropriate (per `.claude/rules/rust.md` "Data Modeling") |
| R9 | Visibility uses `pub(crate)` instead of `pub` unless the symbol is a true downstream API surface |

### Bash-Specific Checks

Anchored in `.claude/rules/bash.md`.

| ID | Check |
|----|-------|
| B1 | Constants/env vars are `UPPERCASE_WITH_UNDERSCORES`; user variables also uppercase per project rule |
| B2 | Function names are descriptive `lowercase_with_underscores` |
| B3 | Loop variables named for their contents (not bare `i` in complex loops) |
| B4 | Variables with `local` scope don't collide with global/env var names |
| B5 | `fd` portability variable is named `FD` (per project rule), not bare `fd` |

### Project-specific naming red flags

These are project-specific traps observed in the `git-remote-object-store`
codebase. Treat each as a finding when you spot it:

- Mixing `fetch` / `get` / `retrieve` / `download` for the same operation across
  modules — pick one verb per concept (`.claude/rules/naming.md`).
- Mixing `parse` / `from_str` / `decode` / `try_from` for the same conversion.
- Using `process` or `handle` as the operation name when something more specific
  applies (e.g., `validate`, `upload`, `bundle`).
- Helper-binary modules using names that imply terminal output but actually
  produce protocol output, or vice versa (see `.claude/rules/protocol-stdout.md`).
- `Error` types without an action prefix (`ParseError`, `UploadError`) — bare
  `Error` collides at use sites and obscures the failing operation.

---

## Step 4: Group findings into issues

**Key difference from `audit`**: Rather than one issue per finding, group
findings into coherent tickets:

1. Group by **file** if multiple findings in the same file
2. Group by **theme** if the same naming pattern repeats across files (e.g.,
   "inconsistent use of `parse` vs `from_str` across 4 files")
3. Group by **severity** only as a last resort

Each grouped issue should have 1–5 findings. Never create an issue with more
than ~8 findings (split if needed).

For each finding:

1. Re-read and confirm the evidence is concrete (file + line + reasoning)
2. Cross-check against existing open issues (`gh issue list --label naming`).
   Drop exact duplicates
3. Verify the finding is NOT already caught by clippy or shellcheck in Step 2
4. Verify the finding is not already permitted by `.claude/rules/naming.md`
   (e.g., domain abbreviations like `oid`, `sha`, `ref`)

**Public-API findings (MANDATORY)**: Before suggesting a rename for any `pub`
or `pub(crate)` Rust symbol, run `find_referencing_symbols` to enumerate all
call sites and include the count + crate names in the issue body. This makes
the blast radius visible to the fixer (per `.claude/rules/tool-choice.md`).

---

## Step 5: File GitHub issues

Issue template (adapted for grouped findings):

```markdown
## Summary

<One-sentence theme of the naming issues in this group>

## Findings

### 1. `<current_name>` in `<file>:<lines>`

**Check**: <ID> | **Severity**: <level>

<Evidence: what the name implies vs what the code does>

**Suggested**: `<proposed_name>`

**Call sites**: <N references> (for public APIs only)

### 2. `<current_name>` in `<file>:<lines>`

...

## References

- Project rules: `.claude/rules/naming.md`, `.claude/rules/rust.md`,
  `.claude/rules/bash.md`
- Checks: <list of check IDs referenced>
- Sources: Rust API Guidelines (C-CONV, C-GETTER), Clean Code (Martin),
  Linguistic Antipatterns (Arnaoudova et al.)
```

Write the body to a temp file and use `gh issue create --body-file` per
`.claude/rules/github-cli.md`:

```bash
cat > /tmp/issue-body.md <<'EOF'
...body here...
EOF
gh issue create --title "$ARGUMENTS: <theme>" --label "naming" --body-file /tmp/issue-body.md
```

If the `naming` label does not yet exist, create it once:

```bash
gh label create naming --description "Naming clarity / consistency findings" 2>/dev/null || true
```

For each issue filed, add a fix-plan comment:

```markdown
## Fix Plan

### Changes

1. Rename `<old>` to `<new>` in `<file>` (use `rename_symbol` via Serena LSP)
2. ...

### Verification

When the audit scope is a crate (`CRATE_NAME` was set in Step 2), use `-p
<crate>` in the cargo commands. When the scope is a bare directory, drop
`-p` and run the cargo commands from the appropriate workspace root, or
substitute repo-equivalent test/lint commands.

- [ ] `cargo build -p <crate>` compiles after renames
- [ ] `cargo test -p <crate>` passes
- [ ] `cargo clippy -p <crate> -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] No remaining references to the old name(s) (`rg` over the scope)
- [ ] If a public API was renamed: `CHANGELOG.md` updated under "Changed"
```

```bash
cat > /tmp/fix-plan.md <<'EOF'
...plan here...
EOF
gh issue comment <NUMBER> --body-file /tmp/fix-plan.md
```

---

## Step 6: Summary table

Print to the terminal:

```
## Naming Audit: $ARGUMENTS

| # | Issue | Theme | Findings | Highest Severity |
|---|-------|-------|----------|------------------|
| 1 | #123  | Misleading conversion prefixes | 3 | misleading |
| 2 | #124  | Inconsistent error naming | 2 | inconsistent |
...

Total: X findings in Y issues
Skipped: Z findings already caught by clippy/shellcheck
```

The **Highest Severity** column shows the most severe finding in the group.
Individual finding severities are detailed in the issue body.

---

## Step 7: Save audit state

After completing the audit, persist the coverage state to Serena memory.
Use the Serena MCP tool `serena:write_memory` to save
`naming-audit-state-$ARGUMENTS`.

Build the memory content by **merging** the previous state (from Step 1) with
the current session's coverage. Rules for merging:

- For files audited in this session: update depth, date, model, and findings count
- For files NOT audited in this session: preserve the existing record unchanged
- For files that no longer exist in scope: remove them from the table
- For new files discovered but not audited: add them with depth `none`

Format the memory as:

```
# Naming Audit State: $ARGUMENTS
last_audit: YYYY-MM-DD
last_model: <model-id>

## File Coverage
src/lib.rs | full | 2026-04-26 | 3 findings | claude-opus-4-7
src/url.rs | partial | 2026-04-26 | 1 finding | claude-opus-4-7
src/git.rs | none | - | - | -
tests/integration.rs | full | 2026-04-26 | 0 findings | claude-opus-4-7
```

Each line: `<relative_path> | <depth> | <date> | <findings_count> findings | <model-id>`

Where:

- `<relative_path>` is relative to the scope root
- `<depth>` is `full`, `partial`, `skimmed`, or `none`
- `<date>` is YYYY-MM-DD of last audit (or `-` if never)
- `<findings_count>` is the number of findings for that file (or `-` if never audited)
- `<model-id>` is the AI model that performed the audit (or `-` if never audited)

---

## Step 8: Verify clean working tree

**This step is MANDATORY and must be the last action before returning.**

Confirm that the working tree has zero modifications:

```bash
git status --porcelain
```

If any output is shown, something went wrong. Revert all changes:

```bash
git checkout -- .
```

Then report the anomaly in your summary output.

---

## Guardrails

All ABSOLUTE CONSTRAINTS (top of this document) apply. Additionally:

- **NEVER delete worktrees.** Only the Claude Code runtime may do that.
- Do NOT implement fixes. This is audit-only.
- Do NOT file findings without concrete evidence (file + line + reasoning).
- Do NOT file duplicate issues. Always check `gh issue list --label naming` first.
- Do NOT file findings already caught by clippy or shellcheck in Step 2.
- Do NOT flag domain-standard abbreviations explicitly allowed by
  `.claude/rules/naming.md` (`fd`, `pid`, `url`, `sha`, `oid`, `ref`).
- Use `--body-file` for all `gh issue create` and `gh issue comment` calls
  (per `.claude/rules/github-cli.md`).
- For public-API renames, include a `find_referencing_symbols` call-site count
  in the issue body (per `.claude/rules/tool-choice.md`).
- In worktree mode, all audit work MUST happen inside the isolated worktree.
- In branch mode, audit work runs in the main directory (read-only, verified clean).
- Sub-agents share the parent context (read-only); they do NOT need separate isolation.

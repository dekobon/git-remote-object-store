# CLAUDE.md

## Shared Project Instructions

@AGENTS.md

## Claude Code-Specific Configuration

Everything in `.claude/rules/` is loaded automatically at session start — read
those files for the full conventions rather than restating them here. The notes
below cover only what is specific to the Claude Code harness.

### Editing

- For code files: prefer LSP / Serena symbol-level editing (`replace_symbol_body`, `insert_before/after_symbol`) over line-based Edit tool calls.
- For non-code files: use targeted Edit tool calls. Avoid `sed` for multi-line edits.

### Two hard bans

- **Worktree deletion and branch escape** (`.claude/rules/worktree-safety.md`)
  — this one overrides normal judgment, because violating it destroys another
  agent's uncommitted work, which no diff can recover.
- **`grep` and `find` via Bash** (`.claude/rules/tool-choice.md`) — use the
  Grep/Glob tools, or `rg`/`fd`. Nothing is destroyed by getting this wrong;
  it is a standing project preference, and it holds without case-by-case
  justification.

### Skills

The skills in `.claude/skills/` are workflows, not reference docs. Invoke one
when the user names it or clearly describes its job; otherwise work directly.
Invoking a skill authorizes what it does, including filing issues and spawning
its isolation agents — see "Skills are already authorized" in `AGENTS.md`.

Read a skill's own opening section before assuming what it touches; the
`audit-*` skills are read-only by contract, but `/audit-tests` deliberately
mutates production code to mutation-test a suspect assertion and reverts it.

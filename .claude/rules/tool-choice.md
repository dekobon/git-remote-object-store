---
globs: "**/*"
alwaysApply: true
---

## Hard ban: `grep` and `find`

Do not run `grep` or `find` via the Bash tool — including `find | grep` and
`find -exec grep`. Use the Grep tool or `rg` for text, and the Glob tool or
`fd` for filenames.

This is a project preference, and it is absolute: it holds in every directory
and every situation, and no local circumstance makes an exception reasonable.
Do not look for one.

## Tool hierarchy

1. **LSP-based code intelligence** (e.g., Serena via MCP) for code operations when available — symbol-level read, search, edit, refactor
2. **ast-grep (`sg`)** for pattern-based code queries
3. **Built-in tools** (Grep/Glob) for text search and file discovery
4. **Bash** with `rg`/`fd` for complex shell operations

The per-task mapping of LSP tools lives in `AGENTS.md` under "Code
Intelligence (LSP / Serena)".

## Code vs. non-code

For non-code files (TOML, YAML, Markdown, JSON, configs), use text-based
tools (Read, Edit, Grep, Glob) — LSP semantics do not apply.

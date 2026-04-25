---
globs: "**/*"
alwaysApply: true
---

## ABSOLUTE BAN: grep and find commands

**NEVER run `grep` or `find` via the Bash tool. This is a hard ban, not a preference.**

- Text search → use the built-in Grep tool, or `rg` (ripgrep) if Bash is required
- File search → use the built-in Glob tool, or `fd` if Bash is required
- NEVER: `grep`, `find`, `find | grep`, `find -exec grep`
- ALWAYS: Grep tool, Glob tool, `rg`, `fd`

## Tool Hierarchy

1. **LSP-based code intelligence** (e.g., Serena via MCP) for code operations when available — symbol-level read, search, edit, refactor
2. **ast-grep (sg)** for pattern-based code queries
3. **Built-in tools** (Grep/Glob) for text search and file discovery
4. **Bash** with `rg`/`fd` for complex shell operations
5. NEVER use legacy `grep`/`find` commands

## Code vs. Non-Code

When LSP code intelligence is available, prefer symbol-level operations
(`find_symbol`, `replace_symbol_body`, `find_referencing_symbols`,
`rename_symbol`) over reading whole files or line-based edits. Call the
references-lookup before changing any public API.

For non-code files (TOML, YAML, Markdown, JSON, configs), use text-based
tools (Read, Edit, Grep, Glob) — LSP semantics don't apply.

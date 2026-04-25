# CLAUDE.md

## Shared Project Instructions

@AGENTS.md

## Claude Code-Specific Configuration

### Editing

- For code files: prefer LSP / Serena symbol-level editing (`replace_symbol_body`, `insert_before/after_symbol`) over line-based Edit tool calls.
- For non-code files: use targeted Edit tool calls. Avoid sed for multi-line edits.

### Tool Choice

- **Code operations**: use Serena LSP tools as the default when available (see `.claude/rules/tool-choice.md`)
- **Non-code text search**: use `rg` (ripgrep) or the built-in Grep tool
- **File search**: use `fd` or the built-in Glob tool
- Never use `grep` or `find`

See `.claude/rules/tool-choice.md` for the full tool hierarchy.

## ABSOLUTE PRIORITIES - READ FIRST

### BANNED: grep and find commands

**NEVER run `grep` or `find` via the Bash tool. This is a hard ban, not a preference.**

- Text search → use the built-in Grep tool, or `rg` (ripgrep) if Bash is required
- File search → use the built-in Glob tool, or `fd` if Bash is required
- NEVER: `grep`, `find`, `find | grep`, `find -exec grep`
- ALWAYS: Grep tool, Glob tool, `rg`, `fd`

### BANNED: Worktree Deletion and Branch Escape

**When running in a worktree, these are hard bans:**

- **NEVER** run `git worktree remove`, `git worktree prune`, or `rm -rf` on any worktree directory
- **NEVER** `cd` to the main repository, `git checkout main`, or write files outside your worktree
- **NEVER** use `/clean_gone` or any command that removes worktrees
- Only the Claude Code runtime or the user may remove worktrees
- Other agents may be using worktrees that look "stale" to you -- leave them alone
- See `.claude/rules/worktree-safety.md` for full details

### LSP-First Code Operations (when available)

- When an LSP-based code intelligence tool (e.g., Serena) is reachable, it is the default for all code operations
- Use `get_symbols_overview` instead of reading entire code files
- Use `find_symbol`, `find_referencing_symbols` instead of text-based code search
- Use `replace_symbol_body`, `insert_before/after_symbol` instead of line-based editing
- Use `rename_symbol` instead of multi-file find-and-replace
- **MANDATORY**: call `find_referencing_symbols` before changing any public API
- See `.claude/rules/tool-choice.md` for the full tool hierarchy

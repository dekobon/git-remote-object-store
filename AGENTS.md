# AGENTS.md

Universal project instructions for AI coding assistants.

## Project Overview

`git-remote-object-store` is a Rust crate that exposes AWS S3 and Azure Blob Storage as git remote backends. It ships the helper-protocol binaries (`git-remote-s3+https`, `git-remote-az+https`, etc.), an LFS custom-transfer agent, and a management CLI (`doctor`, `delete-branch`, `protect`, `unprotect`, `gc`, `compact`).

### Standalone project — no external compatibility contracts

This crate stands on its own. It is **not** a port of, and does not maintain any compatibility contract with, any other project — at the URL, CLI-flag, config-file, on-bucket layout, wire-format, or error-wording level. Behavior, locking semantics, error wording, the URL grammar, the on-bucket key layout, and the management-CLI shape are all this project's own decisions and are free to evolve.

Do not introduce shims, aliases, deprecated-form parsers, `--legacy-*` flags, or "matches X" doc comments aimed at accommodating any external surface. Do not cite outside implementations as authoritative references in code comments — the spec, the source itself, and tests are the contract. The helper-protocol spec, the LFS spec, and the cloud-provider API specs are the only external authorities.

Coding conventions for the project live in `.claude/rules/`:

| File | Topic |
|------|-------|
| `.claude/rules/rust.md` | Rust coding conventions |
| `.claude/rules/naming.md` | Naming conventions across languages |
| `.claude/rules/testing.md` | Test infrastructure principles |
| `.claude/rules/git-commits.md` | Conventional commit format |
| `.claude/rules/markdown.md` | Markdown lint expectations |
| `.claude/rules/tool-choice.md` | Tool precedence (Serena/LSP, ast-grep, Grep/Glob) |
| `.claude/rules/bash.md` | Bash coding conventions |
| `.claude/rules/github-cli.md` | `gh` CLI usage patterns |
| `.claude/rules/worktree-safety.md` | Worktree deletion / branch-escape bans |
| `.claude/rules/changelog.md` | CHANGELOG format |
| `.claude/rules/documentation.md` | No stale counts in docs |
| `.claude/rules/environment-variables.md` | Adding/removing env vars (single index in `docs/environment-variables.md`) |
| `.claude/rules/lessons-learned.md` | Where hard-won lessons live and the quality bar |
| `.claude/rules/protocol-stdout.md` | stdout/stderr discipline for the helper-protocol binaries |

## Git & Workflow

### Worktree Safety (ABSOLUTE PRIORITY)

When running in a git worktree (e.g., via `claude -w` or `isolation: worktree`):

- **NEVER delete worktrees** you did not create. No `git worktree remove`, no `git worktree prune`, no `rm -rf` on worktree directories. Other agents may be using them.
- **NEVER leave your worktree**. Do not `cd` to the main repository, do not `git checkout main`, do not write files outside your worktree directory.
- **NEVER run plugins or commands that clean up worktrees** (e.g., `/clean_gone`). Only the Claude Code runtime or the user may remove worktrees.
- Worktrees that look "stale" may belong to another agent -- leave them alone.
- Before any git operation, verify you are in your worktree: `git rev-parse --show-toplevel` should return a path containing `.claude/worktrees/`.

### Plan Execution Scope

When a plan includes multiple distinct phases (e.g., issue filing, implementation, PR creation), treat each phase boundary as a checkpoint. Complete the first phase, then confirm with the user before proceeding to the next. Do not assume "implement the plan" means "execute every phase."

## Editing Principles

- Never rewrite an entire test file to add/fix tests. Only modify the specific tests/functions that need changing.
- Verify previously passing tests still pass before committing.
- Add useful unit and integration tests when fixing issues.

## Code Intelligence (LSP / Serena)

When an LSP-based code intelligence tool such as Serena is available, use it as the **default** for all code operations -- read, search, edit, refactor.

| Task | LSP tool | NOT this |
|---|---|---|
| Understand a file | `get_symbols_overview` | Reading entire file |
| Find a symbol | `find_symbol` | Text search (Grep/rg) |
| Find callers | `find_referencing_symbols` | Text search for function name |
| Edit a function | `replace_symbol_body` | Line-number-based editing |
| Add code near a symbol | `insert_before/after_symbol` | Line-number-based editing |
| Rename anything | `rename_symbol` | Multi-file find-and-replace |
| Search code patterns | `search_for_pattern` | Text search (Grep/rg) |

**When NOT to use LSP tools** (use text-based tools instead):

- Non-code files: TOML, YAML, Markdown, JSON, configs
- Comments and documentation text (unless searching within code files)
- Creating new files from scratch
- File discovery by name (use Glob/fd)

**MANDATORY before any public API change**: enumerate all call sites with `find_referencing_symbols`. This prevents cross-crate breakage.

## External Documentation (Context7)

When researching external libraries, frameworks, SDKs, or CLI tools, prefer indexed documentation tools (e.g., Context7) over web search:

1. **Context7** -- preferred for indexed libraries
2. **`cargo doc`** -- fallback for crates not in Context7
3. **Web fetch** -- last resort

## GitHub CLI

When using `gh` with complex arguments, write content to a temp file and pass via `--body-file`:

```bash
cat > /tmp/issue-body.md <<'EOF'
Content with $variables, `backticks`, and "quotes"
EOF
gh issue create --title "Title" --label "bug" --body-file /tmp/issue-body.md
```

### GitHub Issue Hygiene

- Only close an issue when ALL items are resolved
- When updating issues, update BOTH the body AND add a comment

## Tone and Behavior

- Criticism is welcome -- point out mistakes, suggest better approaches, cite relevant standards
- Be skeptical and concise

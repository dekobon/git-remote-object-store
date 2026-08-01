# AGENTS.md

Universal project instructions for AI coding assistants.

## Project Overview

`git-remote-object-store` is a Rust crate that exposes AWS S3 and Azure Blob Storage as git remote backends. It ships the helper-protocol binaries (`git-remote-s3+https`, `git-remote-az+https`, etc.), an LFS custom-transfer agent, and a management CLI (`doctor`, `delete-branch`, `protect`, `unprotect`, `gc`, `compact`).

### Standalone project — no external compatibility contracts

This crate stands on its own. It is **not** a port of, and does not maintain any compatibility contract with, any other project — at the URL, CLI-flag, config-file, on-bucket layout, wire-format, or error-wording level. Behavior, locking semantics, error wording, the URL grammar, the on-bucket key layout, and the management-CLI shape are all this project's own decisions and are free to evolve.

Do not introduce shims, aliases, deprecated-form parsers, `--legacy-*` flags, or "matches X" doc comments aimed at accommodating any external surface. Do not cite outside implementations as authoritative references in code comments — the spec, the source itself, and tests are the contract. The helper-protocol spec, the LFS spec, and the cloud-provider API specs are the only external authorities.

What this does **not** waive: buckets written by an already-released version of this crate must stay readable by the next one. That is a promise to our own users, not a compatibility contract with a third party. A change to the on-bucket key layout needs a migration path and explicit sign-off.

## Conventions

Coding conventions for the project live in `.claude/rules/`. Claude Code loads them automatically at session start; other assistants should read them directly.

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
| `.claude/rules/object-store-writes.md` | Bucket-key construction and destructive-write safety |

## Working Agreements

### Task scope

Deliver what was asked, at the scope intended. Make routine judgment calls yourself, and check in only when different readings of the request would lead to materially different work. If the request seems mistaken or a better approach exists, say so in a sentence and continue with the task as asked rather than quietly narrowing, widening, or transforming it. Finish the whole task, and stop short of actions clearly beyond what was asked.

Concretely, for this repo: a bug fix does not need the surrounding module cleaned up, a new helper does not need speculative configurability, and internal call sites do not need defensive validation. Validate at system boundaries (URL parsing, cloud responses, operator input), not between functions you control.

### Plan execution scope

When a plan includes multiple distinct phases (e.g., issue filing, implementation, PR creation), treat each phase boundary as a checkpoint. Complete the first phase, then confirm with the user before proceeding to the next. "Implement the plan" does not mean "execute every phase."

### Delegating to subagents

Delegate only for large, genuinely independent tracks of work — a wide multi-file investigation, or fixing several issues that touch disjoint modules. Do not delegate work you can finish yourself in a handful of tool calls, and do not spawn subagents to verify or double-check your own work. If one subagent can do the job, use one rather than several.

### Destructive and outward-facing actions

Local, reversible actions (editing files, running tests, `cargo` commands) need no confirmation. Ask first for anything hard to reverse or visible to others: `git push`, `git push --force`, `git reset --hard`, deleting branches, amending pushed commits, filing or closing GitHub issues and PRs, and any write against a real (non-emulator) S3 or Azure bucket.

When blocked, do not reach for a destructive shortcut: no `--no-verify`, no deleting unfamiliar files that may be another agent's in-progress work, no relaxing a test to make it pass.

**Skills are already authorized.** Invoking a skill from `.claude/skills/` is the user's approval of what that skill does. When `/audit` files issues, `/fix-issue` closes one, or `/batch-fix` and `/audit` launch their isolation agents, that is the deliverable — carry it out without re-confirming, and without weighing it against the delegation guidance above. The confirmation and delegation rules govern actions you chose on your own initiative. Anything a skill does *not* cover still needs the usual judgment.

Worktree deletion and branch escape are separate hard bans — see `.claude/rules/worktree-safety.md`.

## Editing Principles

- Never rewrite an entire test file to add/fix tests. Only modify the specific tests/functions that need changing.
- Add useful unit and integration tests when fixing issues.
- Run `make pre-commit` before committing. It is the gate — formatting, `clippy --all-features`, the test suite with `test-util` enabled, shellspec, and the doc/lint checks — and it replaces ad-hoc re-reading of your own diff. A bare `cargo test --workspace` relies on incidental feature unification for part of its coverage — see "Feature-Gated Integration Tests" in `.claude/rules/testing.md`.

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

Before changing any public API, enumerate its call sites with `find_referencing_symbols`. Signature changes that compile locally can still break `cli/` or `xtask/`.

## External Documentation (Context7)

When researching external libraries, frameworks, SDKs, or CLI tools, prefer indexed documentation tools (e.g., Context7) over web search:

1. **Context7** -- preferred for indexed libraries
2. **`cargo doc`** -- fallback for crates not in Context7
3. **Web fetch** -- last resort

## GitHub CLI

See `.claude/rules/github-cli.md` for `--body-file` usage and issue hygiene.

## Tone and Behavior

- Criticism is welcome -- point out mistakes, suggest better approaches, cite relevant standards
- Be skeptical and concise
- Keep responses focused and brief. Lead with the outcome: the first sentence should answer "what happened" or "what did you find", with supporting detail after it
- Match written deliverables (issue bodies, CHANGELOG entries, docs) to what the task needs; cover the substance without padding out filler sections or redundant summaries

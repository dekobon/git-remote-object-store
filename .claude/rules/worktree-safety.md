## Hard ban: worktree deletion and branch escape

Several agents may be working in `.claude/worktrees/` at once. Removing a
worktree, or writing outside your own, destroys another agent's in-progress
work — work that is not in any commit and cannot be recovered. Treat these
as bans rather than defaults to weigh.

### Never remove a worktree

Do not run `git worktree remove`, `git worktree prune`, or `rm -rf` against
any worktree directory, and do not invoke commands or plugins that clean
worktrees up (`/clean_gone` and friends). Only the Claude Code runtime that
created a worktree, or the user, may remove it.

A worktree that looks stale is not evidence that it is abandoned. Leave it
alone.

### Stay inside your worktree

If `git rev-parse --show-toplevel` returns a path under `.claude/worktrees/`,
you are in a worktree. For the rest of that session:

- Do not `cd` to the main repository, and do not target it with `git -C`.
- Do not `git checkout` or `git switch` to any branch other than your own.
- Keep every write — Edit, Write, Bash redirection, build output — inside
  your worktree. Reading main-repo files for reference is fine.

If a git operation is about to touch shared state and you are unsure where
you are, run `git rev-parse --show-toplevel` and confirm before proceeding.

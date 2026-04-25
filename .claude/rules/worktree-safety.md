## ABSOLUTE BAN: Worktree Deletion and Branch Escape

These rules are non-negotiable. Violating them destroys other agents' in-progress work.

### Never Delete Worktrees You Did Not Create

- **NEVER** run `git worktree remove` on ANY worktree
- **NEVER** run `git worktree prune`
- **NEVER** run `rm -rf` on a worktree directory
- **NEVER** use the `/clean_gone` command or any plugin that removes worktrees
- The ONLY entity that may remove a worktree is the Claude Code runtime that created it (automatic cleanup on session end)
- If you see stale worktrees, **leave them alone** -- another agent may be using them, or the user will clean them up manually

### Stay In Your Worktree

If you are running inside a worktree (check: `git rev-parse --show-toplevel` returns a path under `.claude/worktrees/`):

- **NEVER** `cd` to the main repository directory
- **NEVER** `git checkout main`, `git switch main`, or check out any branch other than your worktree's branch
- **NEVER** run write operations (Edit, Write, Bash) on files in the main repository -- all writes must be within your worktree
- **NEVER** run `git` commands that affect the main repository's state (e.g., `git -C /path/to/main/repo ...`)
- All your commits, file edits, and builds must happen within your worktree directory
- Reading main repo files for reference is OK, but NEVER write to them

### Pre-Flight Check

Before any git operation in a worktree session, verify you are in the right place:

```bash
# Confirm you're in your worktree, not main
git rev-parse --show-toplevel
```

If the output does NOT contain `.claude/worktrees/`, STOP and re-orient -- you have escaped your worktree.

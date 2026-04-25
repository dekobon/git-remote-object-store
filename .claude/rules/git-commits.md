---
globs: "**/*"
alwaysApply: true
---

## Commit Convention

- **Format**: `<type>(<scope>): <subject>` (conventional commits)
- **Types**: feat, fix, docs, style, refactor, test, chore, perf
- **Subject**: max 50 chars, imperative mood, no period
- **Body**: 72-char lines for complex changes explaining what/why
- Keep commits atomic. Do not add Co-Authored-By lines.

## Closing GitHub Issues

When a commit resolves a GitHub issue, add `Fixes #NNN` in the commit
**body** (not the subject line). This auto-closes the issue on push.

```
fix(s3): retry transient 503s with exponential backoff

Add jittered retry around put_object to handle throttling.

Fixes #42
```

- Use `Fixes #N` in the body (not `(#N)` in the subject — that only creates a link)
- Multiple issues: add one `Fixes #N` line per issue
- GitHub recognizes: `Fixes`, `Closes`, `Resolves` (case-insensitive)

## Amending and Squashing

Amending unpushed commits is fine and often preferred to keep history clean. Use `git commit --amend` or `git reset --soft HEAD~N && git commit` to squash local fixups into their parent commit.

Only avoid amending commits that have already been pushed to a remote branch — that requires a force-push and rewrites shared history.

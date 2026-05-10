---
name: Bug report
about: Report a defect in git-remote-object-store
title: ''
labels: bug
assignees: ''

---

**Describe the bug**
A clear and concise description of what the bug is.

**To reproduce**
Steps to reproduce the behavior. Where possible, include the exact
URL form (`s3+https://…` or `az+https://…`), the git command run,
and any relevant configuration (env vars, AWS profile, Azure
credential alias) — redacted as needed.

```bash
# commands you ran
```

**Expected behavior**
What you expected to happen.

**Observed behavior**
What actually happened. Include the full helper output if possible
(re-run the failing git command with `GIT_TRACE=1
GIT_TRANSFER_TRACE=1`).

**Environment**

- `git-remote-object-store` version (`git-remote-object-store --version`):
- `git --version`:
- OS / arch:
- Backend: S3 / Azure Blob / S3-compatible (which?)
- Install method: cargo install / GitHub Release archive / .deb / .rpm / .apk / Homebrew

**Additional context**
Anything else relevant — bucket layout, repo size class (a few MB vs.
multi-GB monorepo), LFS in use, custom endpoints, etc.

<!--
Thanks for the contribution! A few notes:

- Conventional Commits format for the PR title and each commit:
  `<type>(<scope>): <subject>`. Types: feat, fix, docs, refactor,
  test, chore, perf, ci.
- For bug fixes, link the issue with `Fixes #N` in the body so
  GitHub auto-closes it on merge.
- Add a CHANGELOG entry under `## [Unreleased]` for any user-visible
  change (API addition, behaviour change, bug fix). Refactors,
  docs-only, and CI changes don't need one — the commit message is
  enough.
-->

## Summary

<!-- 1–3 bullets on what changed and why. Focus on the "why". -->

## Related issues

<!-- e.g. Fixes #42, Refs #17 -->

## Test plan

<!--
What you ran locally and what to run in review:
- [ ] cargo test --workspace
- [ ] cargo clippy --workspace --all-targets -- -D warnings
- [ ] cargo test --features integration-s3 --test s3_store_integration  (if backend behaviour changed)
- [ ] cargo test --features integration-azure --test azure_store_integration  (if backend behaviour changed)
- [ ] make ci
-->

## Notes for reviewers

<!-- Anything specific to look at: subtle invariants, deliberate trade-offs, follow-ups deferred. -->

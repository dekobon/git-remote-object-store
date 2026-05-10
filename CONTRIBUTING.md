# Contributing to git-remote-object-store

Thanks for considering a contribution. This document covers the
essentials. Project conventions live under
[`AGENTS.md`](AGENTS.md) and the rule files in
[`.claude/rules/`](.claude/rules/) — start there before making a
non-trivial change.

## Ground rules

- By submitting a pull request, you agree to license your
  contribution under the project's license, **Apache-2.0**
  (see [`LICENSE`](LICENSE)).
- Security-sensitive reports must **not** go in public issues —
  see [`SECURITY.md`](SECURITY.md) for the private reporting flow.

## Getting started

```bash
git clone https://github.com/dekobon/git-remote-object-store
cd git-remote-object-store
cargo build --workspace
cargo test --workspace
```

MSRV is declared in the workspace `Cargo.toml` (`rust-version`).
The current floor is **Rust 1.94**.

The repository ships a `Makefile` that mirrors CI:

```bash
make ci          # everything CI runs (fmt, clippy, lints, tests, shellspec)
make check-tools # verify your local toolchain matches what CI expects
```

For backend-touching changes, the Docker-backed integration suites
exercise real S3 (RustFS) and Azure Blob (Azurite) emulators:

```bash
cargo test --features integration-s3 --test s3_store_integration
cargo test --features integration-azure --test azure_store_integration
```

Optional CLI prerequisites for the `spec/` ShellSpec suite:
[ShellSpec](https://shellspec.info/) (the install script lives in
`make ci`, but you can also install manually).

## Project conventions

The canonical reference is `AGENTS.md` plus the per-topic files in
`.claude/rules/`:

| Topic | File |
|------|------|
| Rust style | [`.claude/rules/rust.md`](.claude/rules/rust.md) |
| Naming | [`.claude/rules/naming.md`](.claude/rules/naming.md) |
| Testing | [`.claude/rules/testing.md`](.claude/rules/testing.md) |
| Conventional commits | [`.claude/rules/git-commits.md`](.claude/rules/git-commits.md) |
| Tool choice (LSP-first) | [`.claude/rules/tool-choice.md`](.claude/rules/tool-choice.md) |
| Bash style | [`.claude/rules/bash.md`](.claude/rules/bash.md) |
| Markdown | [`.claude/rules/markdown.md`](.claude/rules/markdown.md) |
| Helper-binary stdout discipline | [`.claude/rules/protocol-stdout.md`](.claude/rules/protocol-stdout.md) |
| Changelog | [`.claude/rules/changelog.md`](.claude/rules/changelog.md) |
| Documentation hygiene | [`.claude/rules/documentation.md`](.claude/rules/documentation.md) |

Highlights:

- **Rust style**: `cargo fmt`, clippy clean with
  `--all-targets --all-features -- -D warnings`. No `unsafe` code.
  Avoid `unwrap` / `expect` / `panic!` outside tests.
- **Commits**: Conventional Commits — `<type>(<scope>): <subject>`.
  Validated in CI by `commitsar`.
- **Tests**: add a regression test whenever you fix a bug. Assertions
  must be specific (not `is_ok()` without checking the value).
  See [`.claude/rules/testing.md`](.claude/rules/testing.md) for the
  full discipline including the "no incidental coupling" rule.
- **Helper-binary stdout is a contract**: the `git-remote-*` and
  `git-lfs-*` binaries communicate with git over a line-based
  protocol on stdout. Diagnostic output goes to stderr only — never
  `println!`, `dbg!`, or stray banner text on stdout outside
  protocol responses.
- **Docs**: `///` on public items; the crate warns on `missing_docs`.

## Workflow

1. **Open an issue first** for anything beyond a trivial fix. It's
   cheaper to discuss the design than to review a large PR against
   the grain of the project.
2. **Branch** from `main`.
3. **Commit in small, reviewable steps.** Each commit should build
   and pass tests on its own where practical.
4. **Run `make ci`** before pushing.
5. **Open a pull request** against `main`. Fill in the PR template.
   Link the issue with `Fixes #NNN` in the PR body.
6. **Changelog**: add an entry to [`CHANGELOG.md`](CHANGELOG.md)
   under `[Unreleased]` for user-visible changes. Refactors,
   docs-only, and CI-only changes don't need a changelog entry.

## Code review

Criticism is welcome — point out mistakes, suggest better approaches,
cite relevant standards. Be skeptical and concise. Reviews focus on
correctness, API shape, and test coverage before style.

## Questions

Open a [GitHub Discussion](https://github.com/dekobon/git-remote-object-store/discussions)
or a low-priority issue. We'd rather answer a question than review a
PR that went the wrong direction.

# CI/CD Integration

CI/CD setup for `git-remote-object-store`.

## Overview

The CI workflow (`.github/workflows/ci.yml`) runs on every push and pull
request with the following jobs:

1. **Commit Messages** — validates conventional commits with
   [commitsar](https://github.com/aevea/commitsar)
2. **Build & Test** — runs format checks, lints, unit/integration tests,
   build, and shellspec via the `make _ci-*` targets
3. **Integration (S3 / RustFS)** — runs the S3 backend test binary against
   a containerized [RustFS](https://github.com/rustfs/rustfs) instance
   spawned by `testcontainers`
4. **Integration (Azure / Azurite)** — runs the Azure backend test binary
   against a containerized
   [Azurite](https://github.com/Azure/Azurite) emulator

The Build & Test job mirrors what `make ci` does locally, so reproducing a
CI failure on a developer machine only requires `make ci`.

### Target Comparison

| Target | Formats Code | Runs Tests | Use Case |
|---|---|---|---|
| `make fmt` | Yes | No | Format all code |
| `make pre-commit` | Yes | Yes | Pre-commit hook (auto-fix) |
| `make ci` | No | Yes | CI pipelines (validation only) |
| `make all` | No | Yes | Full check + test + release build |

## Quick Start

```bash
# Run all CI checks locally (validation only, no auto-fix)
make ci

# Check formatting only
make fmt-check

# Pre-commit with auto-fix (for local development)
make pre-commit
```

## Commitsar

The CI workflow validates all commit messages against the
[Conventional Commits](https://www.conventionalcommits.org) specification using
[commitsar](https://github.com/aevea/commitsar). A pre-built binary is
downloaded from GitHub Releases and cached, avoiding the ~30-60s Docker-based
Go compilation that the upstream GitHub Action performs on every run. Every
commit in the push or PR is checked.

Required format: `<type>(<scope>): <subject>`

Allowed types: `feat`, `fix`, `docs`, `style`, `refactor`, `test`, `chore`,
`perf`, `ci`, `build`, `revert`.

### Fixing Commit Message Failures

Amend the most recent commit:

```bash
git commit --amend -m "feat(scope): proper message"
git push --force-with-lease
```

Rebase and edit older commits:

```bash
git rebase -i HEAD~5
# Mark commits as 'reword', save, edit messages
git push --force-with-lease
```

## What `make ci` Checks

### Format Validation

Validates formatting without modifying files. Fails with exit code 1 if any
files need reformatting.

- **Rust**: `cargo fmt --all --check`
- **Markdown**: `markdownlint-cli2`
- **Bash**: `shfmt -d`
- **TOML**: `taplo fmt --check`

### Linting

- **Rust**: `cargo clippy --all-targets -- -D warnings`
  (and `--all-features`)
- **Bash**: `shellcheck` on all `.sh` files (excluding shellspec specs)
- **Markdown**: `markdownlint-cli2`
- **TOML**: `taplo lint`
- **Makefile**: `checkmake`
- **Dependencies**: `cargo deny check advisories bans licenses sources`

### Testing

- **Unit and integration tests**: `cargo test --workspace --lib --bins --tests`
- **Doc tests**: `cargo test --workspace --doc`
- **CLI integration tests**: `shellspec --shell bash`
- **Backend integration tests** (separate jobs): `cargo test
  --features integration-s3` and `--features integration-azure`

#### Test Result Reporting

The shellspec stage emits JUnit XML at `reports/results_junit.xml`, which is
consumed by [dorny/test-reporter](https://github.com/dorny/test-reporter)
to surface pass/fail breakdowns and inline annotations on failed lines.
Rust unit tests do not emit JUnit XML today (the Makefile uses plain
`cargo test`); failures are reported via the standard step exit code and
the "Fail on test failures" safety-net step.

### Build Verification

- Debug build: `cargo build --workspace --all-targets`

### Rustdoc Validation

Broken intra-doc links, redundant explicit link targets, and any other
rustdoc lint failures fail the build. Runs as the dedicated `rustdoc`
job in CI (`make _ci-doc-check`) and as part of `make pre-commit` /
`make ci` locally — both routes call `make doc-check`, which expands
to:

```bash
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace \
  --all-features --locked
```

`--all-features` is what activates the `test-util`-gated mock
`ObjectStore` so its doc comments are also checked.

## Tool Installation

The CI workflow installs tools using a hybrid caching strategy.

### Pre-installed on `ubuntu-latest`

- Docker (used by the integration-s3 / integration-azure jobs via
  `testcontainers`)
- jq (also installed explicitly to be defensive against runner image drift)

### Installed by the Workflow

| Tool | Version | Method | Cached |
|---|---|---|---|
| commitsar | 1.0.3 | Static binary download | Yes |
| Rust toolchain | derived from `Cargo.toml` `[workspace.package].rust-version` via `cargo metadata` in the `derive-toolchain` job | `actions-rust-lang/setup-rust-toolchain` | Yes |
| cargo-deny | 0.18.4 | `cargo install --locked` | Yes (via Rust cache) |
| shellspec | 0.28.1 | Install script (version-pinned) | Yes |
| markdownlint-cli2 | 0.21.0 | `npm install -g` | Yes |
| taplo | 0.10.0 | Static binary download (sha256-checked) | Yes |
| shfmt | 3.13.0 | Static binary download (sha256-checked) | Yes |
| checkmake | 0.3.2 | Static binary download (sha256-checked) | Yes |
| shellcheck | apt | `apt install` | No |

### Caching Strategy

Two cache layers handle different tool categories.

**Rust toolchain cache**: `actions-rust-lang/setup-rust-toolchain` wraps
`Swatinem/rust-cache` and caches `~/.cargo/registry`, `~/.cargo/git`, and
`target/` with stale artifact cleanup. The workflow also passes
`cache-bin: "true"` so `~/.cargo/bin` is cached, which restores the
`cargo-deny` binary across runs (and avoids reinstalling on every job).
Keyed on `Cargo.lock` hash. The `cache-key` parameter distinguishes the
main job from the integration-test jobs so they don't fight over the
same cache slot.

**Binary tools cache** (explicit): shellspec, markdownlint-cli2, taplo,
shfmt, and checkmake are cached in `~/.local/bin`,
`~/.local/lib/shellspec`, and `~/.npm-global`. The cache key includes
`runner.os` + `runner.arch` plus every tool version, so updating any
version (or adding a new runner platform) automatically invalidates
the cache.

Do not add manual `actions/cache` steps for cargo artifacts — the built-in
Rust cache already handles this. Combining both creates conflicts.

### Updating Tool Versions

Tool versions and pre-built-binary checksums are pinned in the `env`
section of `.github/workflows/ci.yml`:

```yaml
env:
  COMMITSAR_VERSION: "1.0.3"
  CARGO_DENY_VERSION: "0.18.4"
  SHELLSPEC_VERSION: "0.28.1"
  MARKDOWNLINT_CLI2_VERSION: "0.21.0"
  TAPLO_VERSION: "0.10.0"
  SHFMT_VERSION: "3.13.0"
  CHECKMAKE_VERSION: "0.3.2"
```

The Rust toolchain version is **not** pinned in this list — the
`derive-toolchain` job reads it from `[workspace.package].rust-version`
in the root `Cargo.toml` via `cargo metadata` and exposes it as a job
output that downstream jobs consume. Bumping MSRV is therefore a
single-file edit.

To update a tool version:

1. Edit the version (and the matching `*_SHA256_*` checksum, where
   applicable — fetch the new checksum from the upstream release page).
2. To bump the Rust toolchain, edit `rust-version` in the root
   `[workspace.package]` table of `Cargo.toml`. The CI and release
   workflows pick the new value up automatically.
3. Commit and push. The cache invalidates automatically because the cache
   key includes the version strings.

## Manual Setup

The CI workflow runs entirely on GitHub-hosted runners with no
project-specific secrets. There are, however, a few one-time things a
maintainer should verify.

### Required repository configuration

- **Branch protection on `main`**: require the `commitsar`, `ci`,
  `integration-s3`, and `integration-azure` jobs to pass before merging.
  Set this in *Settings → Branches → Branch protection rules*.
- **Action permissions**: the workflow declares its own `permissions:`
  block (read-only at the workflow level; the `ci` job opts in to
  `checks: write` and `pull-requests: write` so `dorny/test-reporter`
  can post check runs and PR annotations). No additional secrets are
  required.

### Optional: dorny/test-reporter permissions

`dorny/test-reporter` posts a check run with the shellspec results. On a
PR from a fork, GitHub's default permissions only grant the action a
read-only token, which causes the post step to fail silently. If
fork-PR test reporting matters, switch the workflow trigger to
`pull_request_target` or accept that fork PRs will only show stage exit
codes (no inline annotations).

### Updating SHA-pinned actions

Third-party actions (`actions/checkout`, `actions/cache`,
`actions-rust-lang/setup-rust-toolchain`, `dorny/test-reporter`) are
pinned to commit SHAs with a trailing `# v<major>` comment. When
updating:

1. Visit the action's GitHub repo and find the new release tag.
2. Resolve the tag to a commit SHA (`git ls-remote
   https://github.com/<owner>/<repo> refs/tags/v<N>`).
3. Replace the SHA in `.github/workflows/ci.yml` and update the
   `# v<N>` comment to match.

## Troubleshooting

### Tests Pass Locally But Fail in CI

Common causes:

1. **Unformatted code**: `make pre-commit` auto-fixes locally but
   `make ci` rejects unformatted code. Run `make fmt` before committing.
2. **Tool version mismatch**: CI uses pinned tool versions. Run
   `make check-tools` and compare against the `env` section of the
   workflow.
3. **Uncommitted files**: The local working tree has files that are
   not yet committed; CI only sees what is in the commit.

### Reproducing CI Locally

```bash
# Exact same checks as the "Build & Test" job
make ci

# Or step by step
make fmt-check
make lint
make test
make shellspec

# Backend integration jobs (require Docker)
make test-integration-s3
make test-integration-azure
```

## Extending CI

### Adding Makefile Checks

To add a new check to the `make ci` pipeline:

1. Create the check as a standalone Makefile target.
2. Create a `_ci-<name>` target with no prerequisites.
3. Add it to the `ci` and `pre-commit` aggregate targets (and to the
   `.PHONY` list at the top of the Makefile).
4. Add a step that invokes `make _ci-<name>` in
   `.github/workflows/ci.yml`.
5. Update this document.

### Adding CI Tools

To add a new tool dependency:

1. Add a versioned download step in `.github/workflows/ci.yml`.
2. Gate it behind `steps.cache-tools.outputs.cache-hit != 'true'`.
3. Add the version to the `env` section.
4. Add the version to the cache key string.
5. Update `utils/check-tools.sh` with the new tool.

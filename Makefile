MAKE_MAJOR_VER    := $(shell echo $(MAKE_VERSION) | cut -d'.' -f1)

ifneq ($(shell test $(MAKE_MAJOR_VER) -gt 3; echo $$?),0)
$(error Make version $(MAKE_VERSION) is not supported, please install GNU Make 4.x)
endif

# Strict shell and Make settings for robust recipes
SHELL          := bash
.SHELLFLAGS    := -eu -o pipefail -c
.DELETE_ON_ERROR:
MAKEFLAGS      += --warn-undefined-variables
MAKEFLAGS      += --no-builtin-rules
.DEFAULT_GOAL  := help

# Directory path of Makefile
BASE_DIR       := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

# Directories excluded from linting and file-search operations
EXCLUDE_DIRS   := .claude .git target

# File finder: prefer fd/fdfind (fast, .gitignore-aware), fall back to find
FD             := $(shell which fdfind 2>/dev/null || which fd 2>/dev/null)

# Precomputed exclusion flags for fd and find
FD_EXCLUDE     := $(foreach dir,$(EXCLUDE_DIRS),--exclude $(dir))
FIND_EXCLUDE   := $(foreach dir,$(EXCLUDE_DIRS),! -path "./$(dir)/*")

# Find files by extension with fd (preferred) or find (fallback).
# Usage: $(call find-by-ext,EXTENSION[,EXTRA_FD_ARGS[,EXTRA_FIND_ARGS]])
# Always pass all three args (use empty for unused) to avoid
# --warn-undefined-variables warnings, e.g. $(call find-by-ext,md,,).
find-by-ext = $(if $(FD),$(FD) --extension $(1) $(FD_EXCLUDE) $(2),find . -name "*.$(1)" -type f $(FIND_EXCLUDE) $(3))

.PHONY: help check-tools build build-release test test-all test-integration-s3 test-integration-azure fmt fmt-check markdown-fmt markdown-check markdown-lint shellcheck sh-fmt sh-fmt-check toml-fmt toml-fmt-check toml-lint makefile-check lint check deny shellspec shellspec-integration shellspec-integration-s3 shellspec-integration-azure shellspec-live shellspec-live-s3 shellspec-live-azure shellspec-live-sweep image-pin-check clean install install-man man man-check doc doc-open bench all pre-commit ci _pc-fmt _pc-clippy _pc-test _pc-build _pc-shellspec _pc-shellcheck _pc-markdown-lint _pc-toml-lint _pc-makefile-check _pc-deny _pc-man-check _ci-fmt-check _ci-clippy _ci-test _ci-build _ci-shellspec _ci-shellcheck _ci-markdown-check _ci-toml-lint _ci-makefile-check _ci-deny _ci-man-check _ci-cargo-pipeline

# Default target
help:
	@echo "Build and test commands for git-remote-object-store"
	@echo ""
	@echo "Usage: make <target>"
	@echo ""
	@echo "Prerequisites:"
	@echo "  check-tools                          Verify required tools are present"
	@echo ""
	@echo "Build targets:"
	@echo "  build                                Build debug binaries"
	@echo "  build-release                        Build optimized release binaries"
	@echo ""
	@echo "Test targets:"
	@echo "  test                                 Run unit and integration tests (no remote backends)"
	@echo "  test-all                             Run all tests including doc tests"
	@echo "  test-integration-s3                  Run S3 integration tests against local MinIO/RustFS (Docker)"
	@echo "  test-integration-azure               Run Azure integration tests against Azurite (Docker)"
	@echo "  shellspec                            Run shellspec CLI integration tests"
	@echo "  shellspec-live-s3                    Run live AWS S3 shellspec suite (opt-in, costs money)"
	@echo "  shellspec-live-azure                 Run live Azure Blob shellspec suite (opt-in, costs money)"
	@echo "  shellspec-live                       Umbrella for all live-cloud suites (S3 + Azure)"
	@echo "  shellspec-live-sweep                 List/delete stale live-test prefixes (AGE=24h, COMMIT=0)"
	@echo ""
	@echo "Code quality:"
	@echo "  fmt                                  Format Rust + Markdown + TOML + Bash"
	@echo "  fmt-check                            Verify formatting without modifying files"
	@echo "  markdown-fmt                         Auto-fix Markdown with markdownlint-cli2"
	@echo "  markdown-check                       Check Markdown without auto-fix"
	@echo "  markdown-lint                        Lint Markdown with markdownlint-cli2"
	@echo "  shellcheck                           Lint bash scripts (skips spec/*_spec.sh)"
	@echo "  sh-fmt                               Format bash scripts with shfmt"
	@echo "  sh-fmt-check                         Check bash formatting without modifying"
	@echo "  toml-fmt                             Format TOML files with taplo"
	@echo "  toml-fmt-check                       Check TOML formatting without modifying"
	@echo "  toml-lint                            Lint TOML files with taplo"
	@echo "  makefile-check                       Lint Makefile with checkmake"
	@echo "  lint                                 Run all linters"
	@echo "  check                                Run cargo check"
	@echo "  deny                                 Run cargo-deny (advisories, bans, licenses, sources)"
	@echo ""
	@echo "Maintenance:"
	@echo "  clean                                Remove build artifacts"
	@echo "  install                              Build and install binaries"
	@echo "  install-man                          Install manpages under \$$DESTDIR\$$MANDIR/man1/"
	@echo "  man                                  Regenerate manpages (cargo xtask man)"
	@echo "  man-check                            Verify man/ matches the generators"
	@echo ""
	@echo "Documentation:"
	@echo "  doc                                  Generate documentation"
	@echo "  doc-open                             Generate and open documentation"
	@echo ""
	@echo "Performance:"
	@echo "  bench                                Run Rust micro-benchmarks"
	@echo ""
	@echo "Combined targets:"
	@echo "  all                                  Check, test, build release"
	@echo "  pre-commit                           Format, lint, test (recommended before commit)"
	@echo "  ci                                   Validate formatting, lint, test (no auto-fix)"

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------
check-tools:
	@bash $(BASE_DIR)utils/check-tools.sh

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
build:
	cargo build --workspace --all-targets

build-release:
	cargo build --workspace --release

# ---------------------------------------------------------------------------
# Test
# ---------------------------------------------------------------------------
test:
	cargo test --workspace --lib --bins --tests

test-all:
	cargo test --workspace --all-targets
	cargo test --workspace --doc

test-integration-s3:
	cargo test --workspace --features integration-s3 --test s3_store_integration

test-integration-azure:
	cargo test --workspace --features integration-azure --test azure_store_integration

# ---------------------------------------------------------------------------
# Formatting
# ---------------------------------------------------------------------------
fmt:
	@echo "Formatting Rust code..."
	@cargo fmt --all
	@echo "Formatting Markdown files..."
	@$(MAKE) --no-print-directory markdown-fmt
	@echo "Formatting bash scripts..."
	@$(MAKE) --no-print-directory sh-fmt
	@echo "Formatting TOML files..."
	@$(MAKE) --no-print-directory toml-fmt

fmt-check:
	@echo "Checking Rust code formatting..."
	@cargo fmt --all --check || { echo "Rust code is not formatted (run 'make fmt')"; exit 1; }
	@echo "Checking Markdown formatting..."
	@$(MAKE) --no-print-directory markdown-check
	@echo "Checking bash script formatting..."
	@$(MAKE) --no-print-directory sh-fmt-check
	@echo "Checking TOML formatting..."
	@$(MAKE) --no-print-directory toml-fmt-check
	@echo "All formatting checks passed"

markdown-fmt:
	@echo "Auto-fixing Markdown files..."
	@$(call find-by-ext,md,,) | xargs -r markdownlint-cli2 --fix || { echo "markdownlint-cli2 could not auto-fix all issues"; exit 1; }

markdown-check:
	@$(MAKE) --no-print-directory markdown-lint

markdown-lint:
	@echo "Linting Markdown files..."
	@$(call find-by-ext,md,,) | xargs -r markdownlint-cli2 || { echo "markdownlint-cli2 found issues"; exit 1; }

sh-fmt:
	@$(call find-by-ext,sh,--exclude '*_spec.sh',! -path "./spec/*_spec.sh") | xargs -r shfmt -w -i 0 -ci -bn

sh-fmt-check:
	@$(call find-by-ext,sh,--exclude '*_spec.sh',! -path "./spec/*_spec.sh") | xargs -r shfmt -d -i 0 -ci -bn || { echo "Bash scripts are not formatted (run 'make sh-fmt')"; exit 1; }

shellcheck:
	@echo "Linting bash scripts with shellcheck..."
	@$(call find-by-ext,sh,--exclude '*_spec.sh',! -path "./spec/*_spec.sh") | xargs -r shellcheck || { echo "Shellcheck found issues"; exit 1; }

toml-fmt:
	@taplo fmt

toml-fmt-check:
	@taplo fmt --check || { echo "TOML files are not formatted (run 'make toml-fmt')"; exit 1; }

toml-lint:
	@echo "Linting TOML files..."
	@taplo lint || { echo "TOML lint found issues"; exit 1; }

makefile-check:
	@echo "Linting Makefile with checkmake..."
	@checkmake --config $(BASE_DIR).checkmake.ini $(BASE_DIR)Makefile || { echo "checkmake found issues"; exit 1; }

# ---------------------------------------------------------------------------
# Lint aggregate
# ---------------------------------------------------------------------------
lint:
	@echo "Running Rust lints..."
	@cargo clippy --workspace --all-targets -- -D warnings
	@cargo clippy --workspace --all-targets --all-features -- -D warnings
	@echo "Running bash script lints..."
	@$(MAKE) --no-print-directory shellcheck
	@echo "Running Markdown lints..."
	@$(MAKE) --no-print-directory markdown-lint
	@echo "Running TOML lints..."
	@$(MAKE) --no-print-directory toml-lint
	@echo "Running Makefile lints..."
	@$(MAKE) --no-print-directory makefile-check

check:
	cargo check --workspace --all-targets

deny:
	@echo "Running cargo-deny..."
	@cargo deny check advisories bans licenses sources

# ---------------------------------------------------------------------------
# Shellspec CLI integration tests
# ---------------------------------------------------------------------------
shellspec: build
	@echo "Running shellspec CLI + support-module unit tests..."
	@shellspec --shell bash spec/cli_basics_spec.sh spec/live_common_spec.sh spec/live_az_spec.sh spec/live_s3_spec.sh

# End-to-end integration suites that drive `git push` / `git fetch` /
# `git clone` against a real rustfs (S3) or Azurite (Azure Blob)
# container. Require Docker plus the matching cloud CLI on the host
# (`aws` for S3, `az` for Azure) and `git-lfs` for the LFS scenarios.
shellspec-integration-s3: build
	@echo "Running shellspec S3 integration suite..."
	@INTEGRATION_S3=1 shellspec --shell bash \
	  spec/cli_basics_spec.sh spec/integration/s3

shellspec-integration-azure: build
	@echo "Running shellspec Azure integration suite..."
	@INTEGRATION_AZ=1 shellspec --shell bash \
	  spec/cli_basics_spec.sh spec/integration/az

shellspec-integration: shellspec-integration-s3 shellspec-integration-azure

# Live-cloud test tier — opt-in, runs against real AWS S3 and real
# Azure Blob. Never invoked by `make ci`, `make pre-commit`, `make test`,
# `make shellspec-integration`, or `make all`. Requires the explicit
# acknowledgement variable `LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY=1`
# to be exported in the operator's environment; without it the recipe
# refuses to invoke shellspec (the same `live_require_guard` check that
# direct shellspec / `live-sweep.sh` invocations rely on). See
# `spec/live/README.md` for setup, env vars, costs, and cleanup details.
# `ENGINES` defaults to every implemented storage engine, so the live
# suite exercises each one in turn against the same backend. Override
# with `ENGINES=bundle` (or `ENGINES=packchain`) to scope a run to a
# single engine.
ENGINES ?= bundle packchain

# Delegate the cost-acknowledgement check to the canonical bash helper
# so the message and exact semantics live in one place. Without this
# guard at the recipe level, shellspec's per-spec BeforeAll hook still
# aborts — but only after printing one "FAILED" line per test plus a
# buried Examples: block, which scrolls off short terminals.
check_live_guard = bash -c 'source "$(BASE_DIR)spec/support/live_common.sh" && live_require_guard'

shellspec-live-s3: build
	@$(check_live_guard)
	@[ -n "$(strip $(ENGINES))" ] || { echo "ENGINES is empty (set ENGINES=bundle, =packchain, or unset to use the default)" >&2; exit 1; }
	@for engine in $(ENGINES); do \
	  echo "Running shellspec live AWS S3 suite ($$engine)..."; \
	  LIVE_S3=1 LIVE_ENGINE="$$engine" shellspec --shell bash -j 1 \
	    spec/cli_basics_spec.sh spec/live/s3 || exit $$?; \
	done

shellspec-live-azure: build
	@$(check_live_guard)
	@[ -n "$(strip $(ENGINES))" ] || { echo "ENGINES is empty (set ENGINES=bundle, =packchain, or unset to use the default)" >&2; exit 1; }
	@for engine in $(ENGINES); do \
	  echo "Running shellspec live Azure Blob suite ($$engine)..."; \
	  LIVE_AZ=1 LIVE_ENGINE="$$engine" shellspec --shell bash -j 1 \
	    spec/cli_basics_spec.sh spec/live/az || exit $$?; \
	done

# Umbrella target. Fans out to every live-cloud suite. The S3 and Azure
# targets are independent — operators can run either or both depending
# on which credentials they have configured.
shellspec-live: shellspec-live-s3 shellspec-live-azure

# Manual cross-run sweep. Lists prefixes under `live-test/` older than
# `AGE` (default 24 hours). Pass `COMMIT=1` to actually delete; otherwise
# the target prints the keys it would remove and exits. Recovery path
# for SIGKILL-orphaned runs whose `AfterAll` cleanup did not fire.
AGE    ?= 24h
COMMIT ?= 0

shellspec-live-sweep:
	@bash $(BASE_DIR)utils/live-sweep.sh \
	  --age "$(AGE)" \
	  --commit "$(COMMIT)"

# Image-pin guard: the docker image tags in
# `spec/support/images.sh` must match the ones in the Rust
# integration tests so a single backend bug reproduces in both layers.
# `head -1` keeps the comparison single-line if a future contributor
# adds a doc comment or duplicate constant matching the same prefix.
image-pin-check:
	@echo "Verifying shellspec image pins match Rust integration tests..."
	@spec_rust_tag=$$(rg --no-line-number --no-filename "^const RUSTFS_TAG: &str = " tests/s3_store_integration.rs \
	  | head -1 | sed -E 's/.*"([^"]+)".*/\1/') && \
	  spec_shell_tag=$$(rg --no-line-number --no-filename "^RUSTFS_TAG=" spec/support/images.sh \
	  | head -1 | sed -E 's/.*"([^"]+)".*/\1/') && \
	  [ "$$spec_rust_tag" = "$$spec_shell_tag" ] || { \
	    echo "RUSTFS_TAG drift: rust=$$spec_rust_tag shellspec=$$spec_shell_tag" >&2; exit 1; }
	@az_rust_tag=$$(rg --no-line-number --no-filename "^const AZURITE_TAG: &str = " tests/azure_store_integration.rs \
	  | head -1 | sed -E 's/.*"([^"]+)".*/\1/') && \
	  az_shell_tag=$$(rg --no-line-number --no-filename "^AZURITE_TAG=" spec/support/images.sh \
	  | head -1 | sed -E 's/.*"([^"]+)".*/\1/') && \
	  [ "$$az_rust_tag" = "$$az_shell_tag" ] || { \
	    echo "AZURITE_TAG drift: rust=$$az_rust_tag shellspec=$$az_shell_tag" >&2; exit 1; }
	@echo "Image pins are in sync."

# ---------------------------------------------------------------------------
# Maintenance
# ---------------------------------------------------------------------------
clean:
	cargo clean

install: build-release
	cargo install --path cli

# ---------------------------------------------------------------------------
# Manpages
# ---------------------------------------------------------------------------
# `man` regenerates the troff under `man/` (clap-derived pages for the
# management CLI + hand-authored stubs for the helper-protocol binaries).
# `man-check` is the CI gate: fails non-zero when the tree drifts from
# what `xtask man` would emit. Treat any failure as "run `make man` and
# commit the diff."
#
# `install-man` ships the `.1` files under the operator-configured prefix.
# `DESTDIR` is the staged install root (used by packagers); `PREFIX` is the
# final on-disk prefix (typically `/usr/local` for `make install`, `/usr`
# for distro packages). `MANDIR` defaults to `$(PREFIX)/share/man` but can
# be overridden for distros that use `/usr/share/man` regardless of prefix.
PREFIX  ?= /usr/local
MANDIR  ?= $(PREFIX)/share/man

man:
	cargo xtask man

man-check:
	cargo xtask man --check

install-man:
	@install -d "$(DESTDIR)$(MANDIR)/man1"
	@for page in $(BASE_DIR)man/*.1; do \
	  install -m 0644 "$$page" "$(DESTDIR)$(MANDIR)/man1/"; \
	done
	@echo "Installed manpages to $(DESTDIR)$(MANDIR)/man1/"

# ---------------------------------------------------------------------------
# Documentation
# ---------------------------------------------------------------------------
doc:
	cargo doc --no-deps --workspace

doc-open:
	cargo doc --no-deps --workspace --open

# ---------------------------------------------------------------------------
# Performance
# ---------------------------------------------------------------------------
bench:
	cargo bench --workspace

# ---------------------------------------------------------------------------
# Combined workflows
# ---------------------------------------------------------------------------
all: check test build-release

pre-commit:
	$(MAKE) -j --output-sync=target \
	  _pc-test _pc-shellspec \
	  _pc-shellcheck _pc-markdown-lint _pc-toml-lint _pc-makefile-check _pc-deny \
	  _pc-man-check
	@echo "Pre-commit checks passed"

ci:
	$(MAKE) _ci-fmt-check
	$(MAKE) -j --output-sync=target \
	  _ci-cargo-pipeline \
	  _ci-shellcheck _ci-markdown-check _ci-toml-lint _ci-makefile-check _ci-deny \
	  _ci-man-check
	@echo "CI checks passed"

# ---------------------------------------------------------------------------
# Parallel pre-commit DAG
#
# These _pc-* targets express the dependency graph so `make -j` runs
# independent stages concurrently. The `pre-commit` target invokes them
# with `-j --output-sync=target`.
#
# Cargo targets (_pc-clippy → _pc-test → _pc-build) are serialized because
# concurrent cargo commands block on the package cache lock. Non-cargo
# checks run in parallel with the cargo pipeline, gated only on _pc-fmt.
#
# Dependency graph:
#
#   _pc-fmt
#    ├── _pc-clippy → _pc-test → _pc-build
#    │                              └── _pc-shellspec
#    ├── _pc-shellcheck
#    ├── _pc-markdown-lint
#    ├── _pc-toml-lint
#    ├── _pc-makefile-check
#    └── _pc-deny
#
# Do not invoke _pc-* targets directly; use `make pre-commit`.
# ---------------------------------------------------------------------------
_pc-fmt:
	$(MAKE) fmt-check

_pc-clippy: _pc-fmt
	cargo clippy --workspace --all-targets -- -D warnings
	cargo clippy --workspace --all-targets --all-features -- -D warnings

_pc-test: _pc-clippy
	cargo test --workspace --lib --bins --tests
	cargo test --workspace --doc

_pc-build: _pc-test
	cargo build --workspace --all-targets

_pc-shellspec: _pc-build
	shellspec --shell bash spec/cli_basics_spec.sh spec/live_common_spec.sh spec/live_az_spec.sh spec/live_s3_spec.sh

_pc-shellcheck: _pc-fmt
	$(MAKE) shellcheck

_pc-markdown-lint: _pc-fmt
	$(MAKE) markdown-lint

_pc-toml-lint: _pc-fmt
	$(MAKE) toml-lint

_pc-makefile-check: _pc-fmt
	$(MAKE) makefile-check

_pc-deny: _pc-fmt
	$(MAKE) deny

_pc-man-check: _pc-fmt
	$(MAKE) man-check

# ---------------------------------------------------------------------------
# CI validation targets (no auto-formatting)
#
# These _ci-* targets have NO prerequisites — they can be invoked
# individually from GitHub Actions workflow steps or composed via `make ci`
# for local use.
#
# Execution order (enforced by `ci` target + _ci-cargo-pipeline):
#   1. _ci-fmt-check (sequential, must pass before anything else)
#   2. parallel:
#      _ci-cargo-pipeline: clippy → test → build → shellspec
#      _ci-shellcheck, _ci-markdown-check, _ci-toml-lint,
#      _ci-makefile-check, _ci-deny
# ---------------------------------------------------------------------------
_ci-fmt-check:
	$(MAKE) fmt-check

_ci-clippy:
	cargo clippy --workspace --all-targets -- -D warnings
	cargo clippy --workspace --all-targets --all-features -- -D warnings

_ci-test:
	cargo test --workspace --lib --bins --tests
	cargo test --workspace --doc

_ci-build:
	cargo build --workspace --all-targets

_ci-shellspec:
	mkdir -p reports
	shellspec --shell bash --format progress --output junit --reportdir reports \
	  spec/cli_basics_spec.sh spec/live_common_spec.sh spec/live_az_spec.sh spec/live_s3_spec.sh

_ci-shellcheck:
	$(MAKE) shellcheck

_ci-markdown-check:
	$(MAKE) markdown-check

_ci-toml-lint:
	$(MAKE) toml-lint

_ci-makefile-check:
	$(MAKE) makefile-check

_ci-deny:
	$(MAKE) deny

_ci-man-check:
	$(MAKE) man-check

# Sequential cargo pipeline for local `make ci`
_ci-cargo-pipeline:
	$(MAKE) _ci-clippy
	$(MAKE) _ci-test
	$(MAKE) _ci-build
	$(MAKE) _ci-shellspec

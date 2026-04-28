#!/usr/bin/env bash
# Check for required and optional tools used by the Makefile.

set -euo pipefail

echo "Checking required tools..."
echo ""
echo "Core Tools:"

cargo_missing=0
if command -v cargo >/dev/null 2>&1; then
	echo "  ✓ cargo (version: $(cargo --version | cut -d' ' -f2))"
else
	echo "  ✗ cargo (not found)"
	cargo_missing=1
fi

deny_missing=0
if cargo deny --version >/dev/null 2>&1; then
	deny_version=$(cargo deny --version 2>/dev/null | awk 'NR==1{print $2; exit}' || true)
	deny_version=${deny_version:-unknown}
	echo "  ✓ cargo-deny (version: $deny_version)"
else
	echo "  ✗ cargo-deny (not found)"
	deny_missing=1
fi

checkmake_missing=0
if command -v checkmake >/dev/null 2>&1; then
	# checkmake --version: "checkmake v0.3.2 built at ..." when ldflags are
	# applied, or "checkmake  built at ..." when they are not. Scan for the
	# first token that looks like a version rather than assuming position.
	checkmake_version=$(checkmake --version 2>/dev/null \
		| awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^v?[0-9]+\.[0-9]+/) { print $i; exit } }')
	checkmake_version=${checkmake_version:-unknown}
	echo "  ✓ checkmake (version: $checkmake_version)"
else
	echo "  ✗ checkmake (not found)"
	checkmake_missing=1
fi

echo ""
echo "Optional Tools (CLI integration tests):"

shellspec_missing=0
if command -v shellspec >/dev/null 2>&1; then
	echo "  ✓ shellspec (version: $(shellspec --version 2>/dev/null | head -1))"
else
	echo "  ✗ shellspec (not found)"
	shellspec_missing=1
fi

jq_missing=0
if command -v jq >/dev/null 2>&1; then
	echo "  ✓ jq (version: $(jq --version 2>/dev/null))"
else
	echo "  ✗ jq (not found)"
	jq_missing=1
fi

echo ""
echo "Optional Tools (Markdown linting):"

markdownlint_missing=0
if command -v markdownlint-cli2 >/dev/null 2>&1; then
	mdlint_version=$(markdownlint-cli2 --version 2>&1 | awk 'NR==1{print $1" "$2; exit}' || true)
	mdlint_version=${mdlint_version:-unknown}
	echo "  ✓ markdownlint-cli2 (version: $mdlint_version)"
else
	echo "  ✗ markdownlint-cli2 (not found)"
	markdownlint_missing=1
fi

echo ""
echo "Optional Tools (File search):"

fd_missing=0
if command -v fd >/dev/null 2>&1; then
	echo "  ✓ fd (version: $(fd --version 2>/dev/null | head -1))"
elif command -v fdfind >/dev/null 2>&1; then
	echo "  ✓ fdfind (version: $(fdfind --version 2>/dev/null | head -1))"
else
	echo "  ✗ fd/fdfind (not found)"
	fd_missing=1
fi

echo ""
echo "Optional Tools (TOML formatting/linting):"

taplo_missing=0
if command -v taplo >/dev/null 2>&1; then
	echo "  ✓ taplo (version: $(taplo --version 2>/dev/null | head -1))"
else
	echo "  ✗ taplo (not found)"
	taplo_missing=1
fi

echo ""
echo "Optional Tools (Bash linting/formatting):"

shellcheck_missing=0
if command -v shellcheck >/dev/null 2>&1; then
	echo "  ✓ shellcheck (version: $(shellcheck --version 2>/dev/null | awk '/^version:/{print $2; exit}'))"
else
	echo "  ✗ shellcheck (not found)"
	shellcheck_missing=1
fi

shfmt_missing=0
if command -v shfmt >/dev/null 2>&1; then
	echo "  ✓ shfmt (version: $(shfmt --version 2>/dev/null | head -1))"
else
	echo "  ✗ shfmt (not found)"
	shfmt_missing=1
fi

echo ""
echo "Optional Tools (Integration backends — Docker required at test time):"

docker_missing=0
if command -v docker >/dev/null 2>&1; then
	echo "  ✓ docker (version: $(docker --version 2>/dev/null | cut -d' ' -f3 | tr -d ','))"
else
	echo "  ✗ docker (not found)"
	docker_missing=1
fi

echo ""

core_missing=$((cargo_missing + deny_missing + checkmake_missing))
optional_missing=$((shellspec_missing + jq_missing + markdownlint_missing + fd_missing + taplo_missing + shellcheck_missing + shfmt_missing + docker_missing))

if [ "$core_missing" -gt 0 ]; then
	echo "Missing core tools:"
	if [ "$cargo_missing" -eq 1 ]; then
		echo "  - cargo: Install from https://rustup.rs/"
	fi
	if [ "$deny_missing" -eq 1 ]; then
		echo "  - cargo-deny: Install with: cargo install --locked cargo-deny"
	fi
	if [ "$checkmake_missing" -eq 1 ]; then
		echo "  - checkmake: Download from https://github.com/checkmake/checkmake/releases"
	fi
	echo ""
	echo "Error: Required core tools are missing. Please install them before continuing."
	exit 1
fi

if [ "$optional_missing" -gt 0 ]; then
	echo "Missing optional tools:"
	if [ "$shellspec_missing" -eq 1 ]; then
		echo "  - shellspec: Install from https://shellspec.info/ (needed for 'make shellspec')"
	fi
	if [ "$jq_missing" -eq 1 ]; then
		echo "  - jq: Install with: apt install jq (Debian/Ubuntu) or brew install jq (macOS)"
	fi
	if [ "$markdownlint_missing" -eq 1 ]; then
		echo "  - markdownlint-cli2: Install with: npm install -g markdownlint-cli2"
	fi
	if [ "$fd_missing" -eq 1 ]; then
		echo "  - fd: Install with: apt install fd-find (Debian/Ubuntu) or cargo install fd-find"
	fi
	if [ "$taplo_missing" -eq 1 ]; then
		echo "  - taplo: Install with: cargo install taplo-cli --locked --features lsp"
	fi
	if [ "$shellcheck_missing" -eq 1 ]; then
		echo "  - shellcheck: Install with: apt install shellcheck (Debian/Ubuntu) or brew install shellcheck (macOS)"
	fi
	if [ "$shfmt_missing" -eq 1 ]; then
		echo "  - shfmt: Install from https://github.com/mvdan/sh/releases"
	fi
	if [ "$docker_missing" -eq 1 ]; then
		echo "  - docker: Required to run integration-s3 / integration-azure tests"
	fi
	echo ""
	echo "Warning: Optional tools are missing. Some targets will fail."
	echo "All core tools are available - you can still run most targets."
else
	echo "All tools available!"
fi

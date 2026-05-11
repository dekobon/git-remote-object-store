#!/usr/bin/env bash
# Check for required and optional tools used by the Makefile.

set -euo pipefail

echo "Checking required tools..."
echo ""
echo "Core Tools:"

CARGO_MISSING=0
if command -v cargo >/dev/null 2>&1; then
	echo "  ✓ cargo (version: $(cargo --version | cut -d' ' -f2))"
else
	echo "  ✗ cargo (not found)"
	CARGO_MISSING=1
fi

DENY_MISSING=0
if cargo deny --version >/dev/null 2>&1; then
	DENY_VERSION=$(cargo deny --version 2>/dev/null | awk 'NR==1{print $2; exit}' || true)
	DENY_VERSION=${DENY_VERSION:-unknown}
	echo "  ✓ cargo-deny (version: $DENY_VERSION)"
else
	echo "  ✗ cargo-deny (not found)"
	DENY_MISSING=1
fi

CHECKMAKE_MISSING=0
if command -v checkmake >/dev/null 2>&1; then
	# checkmake --version: "checkmake v0.3.2 built at ..." when ldflags are
	# applied, or "checkmake  built at ..." when they are not. Scan for the
	# first token that looks like a version rather than assuming position.
	CHECKMAKE_VERSION=$(checkmake --version 2>/dev/null \
		| awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^v?[0-9]+\.[0-9]+/) { print $i; exit } }')
	CHECKMAKE_VERSION=${CHECKMAKE_VERSION:-unknown}
	echo "  ✓ checkmake (version: $CHECKMAKE_VERSION)"
else
	echo "  ✗ checkmake (not found)"
	CHECKMAKE_MISSING=1
fi

echo ""
echo "Optional Tools (CLI integration tests):"

SHELLSPEC_MISSING=0
if command -v shellspec >/dev/null 2>&1; then
	echo "  ✓ shellspec (version: $(shellspec --version 2>/dev/null | head -1))"
else
	echo "  ✗ shellspec (not found)"
	SHELLSPEC_MISSING=1
fi

JQ_MISSING=0
if command -v jq >/dev/null 2>&1; then
	echo "  ✓ jq (version: $(jq --version 2>/dev/null))"
else
	echo "  ✗ jq (not found)"
	JQ_MISSING=1
fi

echo ""
echo "Optional Tools (Markdown linting):"

MARKDOWNLINT_MISSING=0
if command -v markdownlint-cli2 >/dev/null 2>&1; then
	MDLINT_VERSION=$(markdownlint-cli2 --version 2>&1 | awk 'NR==1{print $1" "$2; exit}' || true)
	MDLINT_VERSION=${MDLINT_VERSION:-unknown}
	echo "  ✓ markdownlint-cli2 (version: $MDLINT_VERSION)"
else
	echo "  ✗ markdownlint-cli2 (not found)"
	MARKDOWNLINT_MISSING=1
fi

echo ""
echo "Optional Tools (File search):"

FD_MISSING=0
if command -v fd >/dev/null 2>&1; then
	echo "  ✓ fd (version: $(fd --version 2>/dev/null | head -1))"
elif command -v fdfind >/dev/null 2>&1; then
	echo "  ✓ fdfind (version: $(fdfind --version 2>/dev/null | head -1))"
else
	echo "  ✗ fd/fdfind (not found)"
	FD_MISSING=1
fi

echo ""
echo "Optional Tools (TOML formatting/linting):"

TAPLO_MISSING=0
if command -v taplo >/dev/null 2>&1; then
	echo "  ✓ taplo (version: $(taplo --version 2>/dev/null | head -1))"
else
	echo "  ✗ taplo (not found)"
	TAPLO_MISSING=1
fi

echo ""
echo "Optional Tools (Bash linting/formatting):"

SHELLCHECK_MISSING=0
if command -v shellcheck >/dev/null 2>&1; then
	echo "  ✓ shellcheck (version: $(shellcheck --version 2>/dev/null | awk '/^version:/{print $2; exit}'))"
else
	echo "  ✗ shellcheck (not found)"
	SHELLCHECK_MISSING=1
fi

SHFMT_MISSING=0
if command -v shfmt >/dev/null 2>&1; then
	echo "  ✓ shfmt (version: $(shfmt --version 2>/dev/null | head -1))"
else
	echo "  ✗ shfmt (not found)"
	SHFMT_MISSING=1
fi

echo ""
echo "Optional Tools (Integration backends — Docker required at test time):"

DOCKER_MISSING=0
if command -v docker >/dev/null 2>&1; then
	echo "  ✓ docker (version: $(docker --version 2>/dev/null | cut -d' ' -f3 | tr -d ','))"
else
	echo "  ✗ docker (not found)"
	DOCKER_MISSING=1
fi

echo ""

CORE_MISSING=$((CARGO_MISSING + DENY_MISSING + CHECKMAKE_MISSING))
OPTIONAL_MISSING=$((SHELLSPEC_MISSING + JQ_MISSING + MARKDOWNLINT_MISSING + FD_MISSING + TAPLO_MISSING + SHELLCHECK_MISSING + SHFMT_MISSING + DOCKER_MISSING))

if [ "$CORE_MISSING" -gt 0 ]; then
	echo "Missing core tools:"
	if [ "$CARGO_MISSING" -eq 1 ]; then
		echo "  - cargo: Install from https://rustup.rs/"
	fi
	if [ "$DENY_MISSING" -eq 1 ]; then
		echo "  - cargo-deny: Install with: cargo install --locked cargo-deny"
	fi
	if [ "$CHECKMAKE_MISSING" -eq 1 ]; then
		echo "  - checkmake: Download from https://github.com/checkmake/checkmake/releases"
	fi
	echo ""
	echo "Error: Required core tools are missing. Please install them before continuing."
	exit 1
fi

if [ "$OPTIONAL_MISSING" -gt 0 ]; then
	echo "Missing optional tools:"
	if [ "$SHELLSPEC_MISSING" -eq 1 ]; then
		echo "  - shellspec: Install from https://shellspec.info/ (needed for 'make shellspec')"
	fi
	if [ "$JQ_MISSING" -eq 1 ]; then
		echo "  - jq: Install with: apt install jq (Debian/Ubuntu) or brew install jq (macOS)"
	fi
	if [ "$MARKDOWNLINT_MISSING" -eq 1 ]; then
		echo "  - markdownlint-cli2: Install with: npm install -g markdownlint-cli2"
	fi
	if [ "$FD_MISSING" -eq 1 ]; then
		echo "  - fd: Install with: apt install fd-find (Debian/Ubuntu) or cargo install fd-find"
	fi
	if [ "$TAPLO_MISSING" -eq 1 ]; then
		echo "  - taplo: Install with: cargo install taplo-cli --locked --features lsp"
	fi
	if [ "$SHELLCHECK_MISSING" -eq 1 ]; then
		echo "  - shellcheck: Install with: apt install shellcheck (Debian/Ubuntu) or brew install shellcheck (macOS)"
	fi
	if [ "$SHFMT_MISSING" -eq 1 ]; then
		echo "  - shfmt: Install from https://github.com/mvdan/sh/releases"
	fi
	if [ "$DOCKER_MISSING" -eq 1 ]; then
		echo "  - docker: Required to run integration-s3 / integration-azure tests"
	fi
	echo ""
	echo "Warning: Optional tools are missing. Some targets will fail."
	echo "All core tools are available - you can still run most targets."
else
	echo "All tools available!"
fi

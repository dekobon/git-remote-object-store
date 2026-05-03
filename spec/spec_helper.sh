# shellcheck shell=bash

# shellspec test helper.
# Provides: binary paths, env vars, common fixtures.
#
# Note: do not use `set -eu` here -- shellspec manages its own shell options
# and `set -u` conflicts with shellspec's internal variable handling.

# SHELLSPEC_PROJECT_ROOT is set by shellspec to the project root.
BASE_DIR="${SHELLSPEC_PROJECT_ROOT:?SHELLSPEC_PROJECT_ROOT must be set}"

# Helper-protocol binaries and the management CLI all land in target/debug.
# shellspec's `run command` uses shellspec_which() which only searches PATH,
# so add the binary directory to PATH for command resolution.
export PATH="${BASE_DIR}/target/debug:${PATH}"

# Public binary names — kept in one place so spec files can refer to them
# symbolically. These are NOT exported because shellspec sources this file
# into the same shell that runs specs.
# shellcheck disable=SC2034 # used by spec files that source this helper
MANAGE_BIN="git-remote-object-store"
# shellcheck disable=SC2034
HELPER_S3_HTTPS="git-remote-s3-https"
# shellcheck disable=SC2034
HELPER_S3_HTTP="git-remote-s3-http"
# shellcheck disable=SC2034
HELPER_AZ_HTTPS="git-remote-az-https"
# shellcheck disable=SC2034
HELPER_AZ_HTTP="git-remote-az-http"
# shellcheck disable=SC2034
LFS_AGENT_BIN="git-lfs-object-store"

# shellcheck disable=SC2034
FIXTURE_DIR="${BASE_DIR}/spec/fixtures"

# Isolate from the host environment so tests cannot leak credentials or
# pick up a developer's real cloud configuration. Skipped when a live
# suite is enabled (`LIVE_S3=1`, future `LIVE_AZ=1`) — those tests need
# the user's real AWS / Azure provider chain (env vars, profile files,
# IMDS) to authenticate against real cloud endpoints.
if [[ "${LIVE_S3:-0}" != "1" ]]; then
	unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN \
		AWS_PROFILE AWS_REGION AWS_DEFAULT_REGION AWS_DEFAULT_PROFILE \
		AWS_CONFIG_FILE AWS_SHARED_CREDENTIALS_FILE AWS_ENDPOINT_URL
	unset AZURE_STORAGE_ACCOUNT AZURE_STORAGE_KEY AZURE_STORAGE_CONNECTION_STRING \
		AZURE_STORAGE_SAS_TOKEN AZURE_STORAGE_AUTH_MODE
	# AZSTORE_<NAME>_{KEY,CONNECTION_STRING,SAS} is the helper's credential
	# alias surface (see src/object_store/azure.rs); strip every variant.
	while IFS= read -r _var; do
		unset "$_var"
	done < <(env | awk -F= '/^AZSTORE_/{print $1}')
	unset _var
fi

# Per-run scratch directory; the runtime cleans this up by way of the OS.
# For live suites, preserve the operator's HOME so `~/.aws/credentials`
# (and `~/.aws/config`) remain visible to the SDK provider chain. The
# scratch dir is still allocated for git's per-test workspace.
SHELLSPEC_TMP_HOME="$(mktemp -d)"
export XDG_CONFIG_HOME="${SHELLSPEC_TMP_HOME}/config"
export XDG_DATA_HOME="${SHELLSPEC_TMP_HOME}/data"
if [[ "${LIVE_S3:-0}" != "1" ]]; then
	export HOME="${SHELLSPEC_TMP_HOME}"
fi
mkdir -p "${XDG_CONFIG_HOME}" "${XDG_DATA_HOME}"

# git looks for `git-remote-s3+http`, `git-remote-az+https`, etc. when
# resolving an `s3+http://…` / `az+https://…` URL — but our binaries are
# named with hyphens (`git-remote-s3-http`). Create symlinks with the
# `+` form so git can find the helpers, mirroring the install-time
# workaround documented in README.md. See
# docs/development/lessons_learned.md §8.
SHELLSPEC_HELPER_BIN="${SHELLSPEC_TMP_HOME}/bin"
mkdir -p "${SHELLSPEC_HELPER_BIN}"
for _scheme in s3+https s3+http az+https az+http; do
	_target="${BASE_DIR}/target/debug/git-remote-${_scheme//+/-}"
	if [[ -x "${_target}" ]]; then
		ln -sf "${_target}" "${SHELLSPEC_HELPER_BIN}/git-remote-${_scheme}"
	fi
done
unset _scheme _target
export PATH="${SHELLSPEC_HELPER_BIN}:${PATH}"

# Helpers used by `Skip if`: shellspec parses the condition expression
# in a way that mishandles a leading `!` and shell redirection. Wrap
# negations in plain functions that already return the desired exit
# code. See docs/development/lessons_learned.md §7.
have_cmd() { command -v "$1" >/dev/null 2>&1; }
missing_cmd() { ! command -v "$1" >/dev/null 2>&1; }
flag_unset() { [[ "${!1:-0}" != "1" ]]; }

# Integration-suite gating. Default off; the matching Makefile target
# (shellspec-integration-{s3,azure}) sets these to 1 before invoking
# shellspec. Each integration spec opens with a `Skip if` guard.
INTEGRATION_S3="${INTEGRATION_S3:-0}"
INTEGRATION_AZ="${INTEGRATION_AZ:-0}"
# Live-cloud-suite gating. Default off; the `shellspec-live-s3` make
# target sets `LIVE_S3=1` before invoking shellspec. The acknowledgement
# variable `LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY=1` is a separate
# loud-fail guard checked inside the suite's `BeforeAll`.
LIVE_S3="${LIVE_S3:-0}"

# Allow the helper binaries to talk to local docker backends over plain
# HTTP. The URL parser already permits cleartext on loopback hosts
# (src/url.rs); this defensive export covers any future code path that
# tightens the rule.
export GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP=1

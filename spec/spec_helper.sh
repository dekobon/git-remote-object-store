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
# pick up a developer's real cloud configuration. Each provider's
# unset block is gated on its own live flag — flipping `LIVE_S3=1`
# must NOT skip Azure isolation (and vice versa), otherwise a developer
# running INTEGRATION_AZ=1 with LIVE_S3=1 left over in the shell would
# leak real Azure credentials into the Azurite tier.
if [[ "${LIVE_S3:-0}" != "1" ]]; then
	unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN \
		AWS_PROFILE AWS_REGION AWS_DEFAULT_REGION AWS_DEFAULT_PROFILE \
		AWS_CONFIG_FILE AWS_SHARED_CREDENTIALS_FILE AWS_ENDPOINT_URL
fi
# A future `LIVE_AZ=1` flag will gate this block; today it is
# unconditional because no Azure live tier exists yet.
if [[ "${LIVE_AZ:-0}" != "1" ]]; then
	unset AZURE_STORAGE_ACCOUNT AZURE_STORAGE_KEY AZURE_STORAGE_CONNECTION_STRING \
		AZURE_STORAGE_SAS_TOKEN AZURE_STORAGE_AUTH_MODE
	# AZSTORE_<NAME>_{KEY,CONNECTION_STRING,SAS} is the helper's credential
	# alias surface (see src/object_store/azure.rs); strip every variant.
	while IFS= read -r VAR; do
		unset "$VAR"
	done < <(env | awk -F= '/^AZSTORE_/{print $1}')
	unset VAR
fi

# Helper-specific tunables that, if left exported by the operator's
# shell, would bend test semantics. Always unset here; specific specs
# that need a non-default value (e.g. `concurrent_push_spec.sh`) set the
# var inline on the command they invoke. Notably, the bundle/packchain
# held-lock tests (#133) seed `LOCK#.lock` and rely on the production
# code seeing the lock as fresh — a host with
# `GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS=1` exported would otherwise
# let the helper reclaim the lock as stale and pass the delete through.
unset GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS

# Per-run scratch directory; the runtime cleans this up by way of the OS.
# `HOME` is preserved when ANY live suite is active so the operator's
# `~/.aws/credentials` / Azure config / SSO cache remain visible to the
# SDK provider chains. The scratch dir is still allocated for git's
# per-test workspace.
SHELLSPEC_TMP_HOME="$(mktemp -d)"
export XDG_CONFIG_HOME="${SHELLSPEC_TMP_HOME}/config"
export XDG_DATA_HOME="${SHELLSPEC_TMP_HOME}/data"
if [[ "${LIVE_S3:-0}" != "1" && "${LIVE_AZ:-0}" != "1" ]]; then
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
for SCHEME in s3+https s3+http az+https az+http; do
	TARGET="${BASE_DIR}/target/debug/git-remote-${SCHEME//+/-}"
	if [[ -x "${TARGET}" ]]; then
		ln -sf "${TARGET}" "${SHELLSPEC_HELPER_BIN}/git-remote-${SCHEME}"
	fi
done
unset SCHEME TARGET
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
# loud-fail guard checked inside the suite's `BeforeAll`. `LIVE_AZ` is
# reserved for the future Azure live tier (#59); declared here so the
# host-isolation gate above can reference it without an undefined-var
# warning.
LIVE_S3="${LIVE_S3:-0}"
LIVE_AZ="${LIVE_AZ:-0}"

# Allow the helper binaries to talk to local docker backends over plain
# HTTP. The URL parser already permits cleartext on loopback hosts
# (src/url.rs); this defensive export covers any future code path that
# tightens the rule.
export GIT_REMOTE_OBJECT_STORE_ALLOW_HTTP=1

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
# pick up a developer's real cloud configuration.
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN AWS_PROFILE AWS_REGION
unset AZURE_STORAGE_ACCOUNT AZURE_STORAGE_KEY AZURE_STORAGE_CONNECTION_STRING

# Per-run scratch directory; the runtime cleans this up by way of the OS.
SHELLSPEC_TMP_HOME="$(mktemp -d)"
export XDG_CONFIG_HOME="${SHELLSPEC_TMP_HOME}/config"
export XDG_DATA_HOME="${SHELLSPEC_TMP_HOME}/data"
export HOME="${SHELLSPEC_TMP_HOME}"
mkdir -p "${XDG_CONFIG_HOME}" "${XDG_DATA_HOME}"

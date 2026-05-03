# shellcheck shell=bash

# Azure Blob Storage backend bindings for the live-cloud test tier.
# Mirrors the public surface of `spec/support/live_s3.sh` (which itself
# mirrors `spec/support/azurite.sh`) so the same scenario helpers
# (`spec/support/git_scenarios.sh`, `spec/support/bucket_assertions.sh`)
# work unchanged.
#
# All blobs land under `${LIVE_RUN_PREFIX}/<spec-id>/...`. The container
# itself (`$LIVE_AZ_CONTAINER`) is operator-owned and pre-existing; this
# module never creates or deletes containers.

# Required env vars for the Azure suite. `LIVE_AZ_CREDENTIAL_NAME` is
# the alias the helper resolves at runtime via `?credential=$NAME` →
# `AZSTORE_<NAME>_KEY` / `AZSTORE_<NAME>_CONNECTION_STRING` /
# `AZSTORE_<NAME>_SAS` (see `src/object_store/azure/auth.rs`).
LIVE_AZ_REQUIRED_VARS=(LIVE_AZ_ACCOUNT LIVE_AZ_CONTAINER LIVE_AZ_CREDENTIAL_NAME)

# Default Azure public-cloud endpoint suffix. Operators on sovereign
# clouds (US Gov, China, Germany) override via `LIVE_AZ_ENDPOINT_SUFFIX`.
LIVE_AZ_DEFAULT_ENDPOINT_SUFFIX="blob.core.windows.net"

# live_az_require_env
# Abort the suite unless the Azure-suite env contract is satisfied.
live_az_require_env() {
	live_require_env "${LIVE_AZ_REQUIRED_VARS[@]}"
}

# live_az_endpoint_suffix
# Print the endpoint suffix (default `blob.core.windows.net`). Forms the
# tail of the URL host after `<account>.`.
live_az_endpoint_suffix() {
	echo "${LIVE_AZ_ENDPOINT_SUFFIX:-$LIVE_AZ_DEFAULT_ENDPOINT_SUFFIX}"
}

# live_az_credential_env_value
# Resolve the credential alias to its env-var value, in the same order
# the helper does: KEY → CONNECTION_STRING → SAS. Echo the value plus a
# tag (`KEY` / `CONN` / `SAS`) so callers know how to pass it to the
# `az` CLI. Empty output (status non-zero) when no env var is set.
live_az_credential_env_value() {
	# `${var^^}` matches the helper's ASCII-uppercase normalisation of the
	# alias (see resolve_alias() in src/object_store/azure/auth.rs).
	# Aliases are constrained to `[A-Za-z0-9_]+` upstream, so locale-
	# sensitive uppercasing differences cannot apply.
	local upper="${LIVE_AZ_CREDENTIAL_NAME^^}"
	local key_var="AZSTORE_${upper}_KEY"
	local conn_var="AZSTORE_${upper}_CONNECTION_STRING"
	local sas_var="AZSTORE_${upper}_SAS"
	if [[ -n "${!key_var:-}" ]]; then
		printf 'KEY\t%s' "${!key_var}"
		return 0
	fi
	if [[ -n "${!conn_var:-}" ]]; then
		printf 'CONN\t%s' "${!conn_var}"
		return 0
	fi
	if [[ -n "${!sas_var:-}" ]]; then
		printf 'SAS\t%s' "${!sas_var}"
		return 0
	fi
	echo "live_az_credential_env_value: none of $key_var / $conn_var / $sas_var is set" >&2
	return 1
}

# live_az
# Wrapper around `az storage <subcommand>` that translates the resolved
# credential alias into the appropriate `--account-name --account-key`
# / `--connection-string` / `--account-name --sas-token` flag set, then
# invokes the requested subcommand. Centralising prevents a stray call
# from inheriting whatever auth state the operator's shell happens to
# have configured (e.g. a previous `az login` that points at a
# different subscription).
#
# Auth flags are placed BEFORE the user's args so that `set -x` traces
# read in the conventional order (`az storage --account-name X
# --account-key Y blob list ...`). Az CLI itself is order-agnostic.
#
# Usage: live_az <storage-subcommand> [args...]
#   e.g. live_az blob list --container-name foo --prefix bar
live_az() {
	local cred
	cred=$(live_az_credential_env_value) || return 1
	local kind="${cred%%$'\t'*}"
	local value="${cred#*$'\t'}"
	local auth_args=()
	case "$kind" in
		KEY)
			auth_args=(--account-name "$LIVE_AZ_ACCOUNT" --account-key "$value")
			;;
		CONN)
			auth_args=(--connection-string "$value")
			;;
		SAS)
			# `--sas-token` accepts the raw token (no leading `?`).
			auth_args=(--account-name "$LIVE_AZ_ACCOUNT" --sas-token "${value#\?}")
			;;
		*)
			echo "live_az: unrecognised credential kind '$kind'" >&2
			return 1
			;;
	esac
	az storage "${auth_args[@]}" "$@"
}

# live_az_url <prefix>
# Print the helper-protocol URL for a (container, prefix) pair on real
# Azure. The host is virtual-hosted (`<account>.<endpoint-suffix>`); the
# path is `<container>/<prefix>` with `?engine=` and `?credential=`
# appended.
#
# NOTE: This signature differs from `azurite_url <container> <prefix>` —
# the live tier always targets the operator-owned, fixed
# `$LIVE_AZ_CONTAINER` set in `BeforeAll`, so accepting a per-call
# container would invite mismatches between cleanup target and test
# target.
live_az_url() {
	local prefix="$1"
	if [[ -z "$prefix" ]]; then
		echo "live_az_url: requires <prefix>" >&2
		return 1
	fi
	local suffix
	suffix=$(live_az_endpoint_suffix)
	local host="${LIVE_AZ_ACCOUNT}.${suffix}"
	local engine
	engine=$(live_engine)
	printf 'az+https://%s/%s/%s?credential=%s&engine=%s' \
		"$host" "$LIVE_AZ_CONTAINER" "$prefix" \
		"$LIVE_AZ_CREDENTIAL_NAME" "$engine"
}

# live_az_unique_prefix
# Allocate a fresh per-spec sub-prefix under the run prefix. Each
# `Describe` block calls this once at `BeforeEach` so its blob keys do
# not overlap with neighbouring blocks. The trailing random suffix
# bypasses any short-window read-after-write quirks across reused
# prefixes within a single run.
live_az_unique_prefix() {
	live_assert_safe_prefix || return 1
	local rand
	rand=$(head -c 3 /dev/urandom | od -An -tx1 | tr -d ' \n') || return 1
	printf '%s/spec-%s-%s' "$LIVE_RUN_PREFIX" "$$" "$rand"
}

# live_az_list <container> <prefix>
# Print every blob name under <prefix> in <container>, one per line.
# Container is the first arg (matching the `bucket_assertions.sh`
# lister contract); the value is always `$LIVE_AZ_CONTAINER`, but
# accepting it keeps the function signature interchangeable with
# `azurite_list`.
live_az_list() {
	local container="$1"
	local prefix="$2"
	if [[ -z "$container" || -z "$prefix" ]]; then
		echo "live_az_list: requires <container> <prefix>" >&2
		return 1
	fi
	live_az blob list --container-name "$container" --prefix "$prefix" \
		--query '[].name' -o tsv 2>/dev/null \
		| tr '\t' '\n' \
		| awk 'NF' \
		|| true
}

# live_az_get_object <container> <blob> <out_file>
# Download <blob> to <out_file>. Used by `assert_head_pointer`.
live_az_get_object() {
	local container="$1"
	local blob="$2"
	local out="$3"
	if [[ -z "$container" || -z "$blob" || -z "$out" ]]; then
		echo "live_az_get_object: requires <container> <blob> <out_file>" >&2
		return 1
	fi
	live_az blob download --container-name "$container" --name "$blob" \
		--file "$out" --no-progress >/dev/null
}

# live_az_put_object <container> <blob> <local_file>
# Upload <local_file> as <blob> in <container>. Used by the sentinel
# pre-flight to verify write permission and by tests that pre-corrupt
# the container (stale-lock scenarios).
live_az_put_object() {
	local container="$1"
	local blob="$2"
	local file="$3"
	if [[ -z "$container" || -z "$blob" || -z "$file" ]]; then
		echo "live_az_put_object: requires <container> <blob> <file>" >&2
		return 1
	fi
	live_az blob upload --container-name "$container" --name "$blob" \
		--file "$file" --overwrite true --no-progress >/dev/null
}

# live_az_delete_object <container> <blob>
# Delete a single blob. 404 / BlobNotFound is treated as success so
# repeated cleanup is idempotent.
live_az_delete_object() {
	local container="$1"
	local blob="$2"
	if [[ -z "$container" || -z "$blob" ]]; then
		echo "live_az_delete_object: requires <container> <blob>" >&2
		return 1
	fi
	live_az blob delete --container-name "$container" --name "$blob" \
		>/dev/null 2>&1 || true
}

# live_az_clear_prefix <container> <prefix>
# Recursively delete every blob under <prefix>. Refuses to run unless
# the prefix lives under `${LIVE_TOP_PREFIX}/` so a buggy caller cannot
# wipe the root of the container. Idempotent — missing-blob errors are
# swallowed.
live_az_clear_prefix() {
	local container="$1"
	local prefix="$2"
	if [[ -z "$container" || -z "$prefix" ]]; then
		echo "live_az_clear_prefix: requires <container> <prefix>" >&2
		return 1
	fi
	if [[ "$prefix" != "${LIVE_TOP_PREFIX}/"* ]]; then
		echo "live_az_clear_prefix: refusing to clear prefix '$prefix' (must start with '${LIVE_TOP_PREFIX}/')" >&2
		return 1
	fi
	# `delete-batch --pattern <prefix>/*` matches every blob under the
	# prefix; the trailing `/*` is a glob, not a regex. Empty match is
	# treated as success on Azure CLI 2.50+.
	live_az blob delete-batch --source "$container" \
		--pattern "${prefix}/*" >/dev/null 2>&1 || true
}

# live_az_preflight
# Sentinel write/read/delete under the run prefix to validate
# write+delete permissions BEFORE any scenario runs. Catches the
# silent-leak failure mode where a credential has put but not delete
# permission — without this, tests would appear to pass while leaving
# data behind. Each step's failure is reported by name so the operator
# knows which RBAC role action is missing.
live_az_preflight() {
	live_assert_safe_prefix || return 1
	local key="${LIVE_RUN_PREFIX}/.preflight"
	local body downloaded
	body=$(mktemp -t live-preflight.XXXXXX) || {
		echo "live_az_preflight: mktemp failed" >&2
		return 1
	}
	downloaded=$(mktemp -t live-preflight-dl.XXXXXX) || {
		rm -f "$body"
		echo "live_az_preflight: mktemp failed" >&2
		return 1
	}
	echo "preflight" >"$body"

	# Pre-flight is the operator's diagnostic surface for auth /
	# permission / endpoint issues. Let az-cli's stderr through verbatim
	# — the "<op> failed" categorization line is a summary, not a
	# substitute for the underlying error message (e.g.
	# "AuthorizationPermissionMismatch", "AuthenticationFailed",
	# "ContainerNotFound").
	if ! live_az_put_object "$LIVE_AZ_CONTAINER" "$key" "$body"; then
		echo "live_az_preflight: PUT failed (Microsoft.Storage/.../blobs/write)" >&2
		rm -f "$body" "$downloaded"
		return 1
	fi
	if ! live_az_get_object "$LIVE_AZ_CONTAINER" "$key" "$downloaded"; then
		echo "live_az_preflight: GET failed (Microsoft.Storage/.../blobs/read)" >&2
		live_az_delete_object "$LIVE_AZ_CONTAINER" "$key"
		rm -f "$body" "$downloaded"
		return 1
	fi
	if ! cmp -s "$body" "$downloaded"; then
		echo "live_az_preflight: round-trip body mismatch" >&2
		live_az_delete_object "$LIVE_AZ_CONTAINER" "$key"
		rm -f "$body" "$downloaded"
		return 1
	fi
	if ! live_az blob delete --container-name "$LIVE_AZ_CONTAINER" \
		--name "$key" >/dev/null; then
		echo "live_az_preflight: DELETE failed (Microsoft.Storage/.../blobs/delete)" >&2
		rm -f "$body" "$downloaded"
		return 1
	fi
	rm -f "$body" "$downloaded"
}

# live_az_setup
# `BeforeAll` entry point: load env file, validate guard / env / tools,
# allocate the run prefix, validate that the operator's chosen
# credential alias resolves to an env var, run the sentinel pre-flight,
# install the cleanup trap.
live_az_setup() {
	live_load_env_file
	live_require_guard || return 1
	live_require_cmd az git jq || return 1
	live_az_require_env || return 1
	live_init_run_prefix || return 1
	# `live_az_preflight` runs `live_az_put_object`, which threads
	# through `live_az` → `live_az_credential_env_value`. A missing
	# alias env var fails there with the named-variable error message,
	# so no separate pre-check is needed.
	if ! live_az_preflight; then
		echo "live_az_setup: pre-flight failed; aborting before any test runs" >&2
		return 1
	fi
	# Trap on EXIT/INT/TERM so a Ctrl-C mid-suite still cleans up.
	# `live_az_teardown` is the one-shot idempotent cleanup helper.
	trap 'live_az_teardown' EXIT INT TERM
}

# live_az_teardown
# `AfterAll` entry point AND the trap target. Idempotent: missing blobs
# are treated as success. The prefix-safety guard inside
# `live_az_clear_prefix` rejects an empty or malformed
# `LIVE_RUN_PREFIX`.
live_az_teardown() {
	# `AfterAll` may run before `BeforeAll` allocated the prefix (e.g.
	# guard rejection in `live_az_setup`). In that case there is
	# nothing to clean up.
	if [[ -z "${LIVE_RUN_PREFIX:-}" ]]; then
		return 0
	fi
	live_az_clear_prefix "$LIVE_AZ_CONTAINER" "$LIVE_RUN_PREFIX"
	# Run the trap once: subsequent EXIT signals are no-ops.
	trap - EXIT INT TERM
}

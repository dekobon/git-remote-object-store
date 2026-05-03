# shellcheck shell=bash

# Live-cloud test support: guard, env loader, run-id, sentinel
# pre-flight, cleanup trap. Backend-agnostic — concrete S3 / Azure
# bindings live in `live_s3.sh` / `live_az.sh`.
#
# Live-tier tests issue real cloud requests against the user's account.
# The verbose acknowledgement variable
# `LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY=1` is the entry gate; absent
# or unset, every live spec aborts at `BeforeAll`.

# Acknowledgement variable name; the verbose form is intentional. Using
# `_THIS_COSTS_MONEY` makes accidental sets in shell history obvious.
LIVE_GUARD_VAR="LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY"

# Top-level prefix under which every live-tier key is written. Cleanup
# refuses to recurse outside this prefix to bound the blast radius of a
# buggy refactor that leaves `LIVE_RUN_PREFIX` empty.
LIVE_TOP_PREFIX="live-test"

# live_load_env_file
# Optionally source `spec/live/.env` for developer convenience. The file
# is gitignored and never referenced from CI. Missing file is not an
# error — env vars may be exported by the shell instead.
live_load_env_file() {
	local env_file="${SHELLSPEC_PROJECT_ROOT:?SHELLSPEC_PROJECT_ROOT must be set}/spec/live/.env"
	if [[ -f "$env_file" ]]; then
		# shellcheck disable=SC1090
		set -a && . "$env_file" && set +a
	fi
}

# live_require_guard
# Abort with a clear message unless the acknowledgement variable is set
# to `1`. The wording names the variable so a CI log makes the fix
# obvious without scrolling.
live_require_guard() {
	if [[ "${!LIVE_GUARD_VAR:-}" != "1" ]]; then
		echo >&2
		echo "Refusing to run live-cloud tests:" >&2
		echo "$LIVE_GUARD_VAR is not set to 1." >&2
		echo >&2
		echo "These tests issue real PUT/GET/LIST/DELETE against your cloud" >&2
		echo "account using your credentials and will incur charges. Set" >&2
		echo "the variable explicitly to acknowledge:" >&2
		echo >&2
		echo "    export $LIVE_GUARD_VAR=1" >&2
		echo >&2
		return 1
	fi
}

# live_require_env <var-name>...
# Abort with the missing list (not one-by-one) when any required var is
# unset or empty. Tests should call this once at BeforeAll with every
# required name; failure mode is a single message, not a chain.
live_require_env() {
	local missing=()
	local name
	for name in "$@"; do
		if [[ -z "${!name:-}" ]]; then
			missing+=("$name")
		fi
	done
	if ((${#missing[@]} > 0)); then
		echo >&2
		echo "Live-cloud tests require these env vars (all unset):" >&2
		printf '  %s\n' "${missing[@]}" >&2
		return 1
	fi
}

# live_require_cmd <command>...
# Abort with the missing-tools list (not one-by-one). Live tests need
# the full toolchain present before any backend call; piecemeal failures
# waste the operator's time.
live_require_cmd() {
	local missing=()
	local cmd
	for cmd in "$@"; do
		if ! command -v "$cmd" >/dev/null 2>&1; then
			missing+=("$cmd")
		fi
	done
	if ((${#missing[@]} > 0)); then
		echo >&2
		echo "Live-cloud tests require these commands on PATH (all missing):" >&2
		printf '  %s\n' "${missing[@]}" >&2
		return 1
	fi
}

# live_run_id
# Print a sortable, unique-per-process run id. Format:
#
#   YYYYMMDDTHHMMSSZ-<pid>-<rand4>
#
# Timestamp-first ordering keeps lexicographic age sweeps cheap (the
# `shellspec-live-sweep` target compares strings). PID + 4 random hex
# bytes guard against same-second collisions across parallel runners.
live_run_id() {
	local stamp pid rand
	stamp=$(date -u +%Y%m%dT%H%M%SZ) || return 1
	pid=$$
	# /dev/urandom is ubiquitous on Linux/macOS; openssl is not strictly
	# required and `od` is part of POSIX. Hex-encode 4 bytes → 8 chars.
	rand=$(head -c 4 /dev/urandom | od -An -tx1 | tr -d ' \n') || return 1
	printf '%s-%s-%s' "$stamp" "$pid" "$rand"
}

# live_init_run_prefix
# Allocate a fresh `LIVE_RUN_PREFIX` and export it. Idempotent within a
# process: subsequent calls return the same prefix so spec helpers can
# rely on a stable value across hooks.
live_init_run_prefix() {
	if [[ -z "${LIVE_RUN_PREFIX:-}" ]]; then
		local id
		id=$(live_run_id) || return 1
		export LIVE_RUN_PREFIX="${LIVE_TOP_PREFIX}/${id}"
	fi
}

# live_engine
# Print the configured storage engine, defaulting to `bundle`. Plumbed
# into every test URL as `?engine=$(live_engine)`. Forward-compatible
# with future engines (#52); bundle-layout assertions are guarded with
# `live_engine_is_bundle`.
live_engine() {
	echo "${LIVE_ENGINE:-bundle}"
}

# live_engine_is_bundle
# True when the configured engine is `bundle`. Use to gate white-box
# bundle-layout assertions so a future engine drops in cleanly.
live_engine_is_bundle() {
	[[ "$(live_engine)" == "bundle" ]]
}

# live_filter_stale_prefixes <cutoff_stamp>
# Read run-prefix candidates (one per line on stdin, e.g.
# `live-test/20260101T000000Z-pid-rand/`) and print those whose leading
# timestamp segment sorts strictly less than `<cutoff_stamp>`. The run-id
# format is `YYYYMMDDTHHMMSSZ-<pid>-<rand>`; the timestamp is everything
# up to the first `-`. Lexicographic comparison is correct because the
# format is fixed-width and timezone-fixed (UTC).
#
# Pure I/O — no side effects, no env reads beyond `${LIVE_TOP_PREFIX}`.
# `utils/live-sweep.sh` calls this once per backend.
live_filter_stale_prefixes() {
	local cutoff_stamp="$1"
	if [[ -z "$cutoff_stamp" ]]; then
		echo "live_filter_stale_prefixes: requires <cutoff_stamp>" >&2
		return 1
	fi
	local stamp id p
	while IFS= read -r p; do
		[[ -z "$p" ]] && continue
		id="${p#"${LIVE_TOP_PREFIX}"/}"
		id="${id%/}"
		stamp="${id%%-*}"
		if [[ "$stamp" < "$cutoff_stamp" ]]; then
			printf '%s\n' "$p"
		fi
	done
}

# live_assert_safe_prefix
# Return non-zero unless `LIVE_RUN_PREFIX` is non-empty AND begins with
# `${LIVE_TOP_PREFIX}/`. Belt-and-suspenders against the empty-variable
# footgun where a buggy refactor turns
# `aws s3 rm --recursive s3://$BUCKET/$LIVE_RUN_PREFIX/` into a recursive
# delete of the entire bucket.
live_assert_safe_prefix() {
	if [[ -z "${LIVE_RUN_PREFIX:-}" ]]; then
		echo "live_assert_safe_prefix: LIVE_RUN_PREFIX is empty" >&2
		return 1
	fi
	if [[ "${LIVE_RUN_PREFIX}" != "${LIVE_TOP_PREFIX}/"* ]]; then
		echo "live_assert_safe_prefix: LIVE_RUN_PREFIX='${LIVE_RUN_PREFIX}' does not start with '${LIVE_TOP_PREFIX}/'" >&2
		return 1
	fi
}

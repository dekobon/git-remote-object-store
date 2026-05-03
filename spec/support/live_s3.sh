# shellcheck shell=bash

# AWS S3 backend bindings for the live-cloud test tier. Mirrors the
# public surface of `spec/support/rustfs.sh` so the same scenario
# helpers (`spec/support/git_scenarios.sh`,
# `spec/support/bucket_assertions.sh`) work unchanged.
#
# All keys land under `${LIVE_RUN_PREFIX}/<spec-id>/...`. The bucket
# itself (`$LIVE_S3_BUCKET`) is operator-owned and pre-existing; this
# module never creates or deletes buckets.

# Required env vars for the AWS suite. `LIVE_S3_PROFILE` is optional —
# absence means "let the SDK / aws-cli use its default credential chain
# resolution".
LIVE_S3_REQUIRED_VARS=(LIVE_S3_BUCKET LIVE_S3_REGION)

# live_s3_require_env
# Abort the suite unless the AWS-suite env contract is satisfied.
live_s3_require_env() {
	live_require_env "${LIVE_S3_REQUIRED_VARS[@]}"
}

# live_s3_aws
# Wrapper around `aws` that always passes `--region $LIVE_S3_REGION` and
# `--profile $LIVE_S3_PROFILE` (when set). Centralising prevents a stray
# call from inheriting a default region / profile and cleaning up the
# wrong account.
live_s3_aws() {
	local args=(--region "$LIVE_S3_REGION")
	if [[ -n "${LIVE_S3_PROFILE:-}" ]]; then
		args+=(--profile "$LIVE_S3_PROFILE")
	fi
	# Stop aws-cli from paging mid-suite.
	AWS_PAGER="" aws "${args[@]}" "$@"
}

# live_s3_url <prefix>
# Print the helper-protocol URL for a (bucket, prefix) pair on real AWS.
# The host follows virtual-hosted style; the path is `<prefix>` with
# `?engine=` appended (and `?profile=` when set, joined with `&`).
#
# NOTE: This signature deliberately differs from `rustfs_url <bucket>
# <prefix>` — the live tier always targets the operator-owned, fixed
# `$LIVE_S3_BUCKET` set in `BeforeAll`, so accepting a per-call bucket
# would invite mismatches between cleanup target and test target.
live_s3_url() {
	local prefix="$1"
	if [[ -z "$prefix" ]]; then
		echo "live_s3_url: requires <prefix>" >&2
		return 1
	fi
	local host="${LIVE_S3_BUCKET}.s3.${LIVE_S3_REGION}.amazonaws.com"
	local engine
	engine=$(live_engine)
	local query="engine=${engine}"
	if [[ -n "${LIVE_S3_PROFILE:-}" ]]; then
		query+="&profile=${LIVE_S3_PROFILE}"
	fi
	printf 's3+https://%s/%s?%s' "$host" "$prefix" "$query"
}

# live_s3_unique_prefix
# Allocate a fresh per-spec sub-prefix under the run prefix. Each
# `Describe` block calls this once at `BeforeEach` so its bundle keys do
# not overlap with neighbouring blocks. The trailing random suffix
# bypasses any S3 short-window read-after-write quirks across reused
# prefixes within a single run.
live_s3_unique_prefix() {
	live_assert_safe_prefix || return 1
	local rand
	rand=$(head -c 3 /dev/urandom | od -An -tx1 | tr -d ' \n') || return 1
	printf '%s/spec-%s-%s' "$LIVE_RUN_PREFIX" "$$" "$rand"
}

# live_s3_list <bucket> <prefix>
# Print every key under <prefix> in <bucket>, one per line. Bucket is
# the first arg (matching the `bucket_assertions.sh` lister contract);
# the value is always `$LIVE_S3_BUCKET`, but accepting it keeps the
# function signature interchangeable with `rustfs_list`.
live_s3_list() {
	local bucket="$1"
	local prefix="$2"
	if [[ -z "$bucket" || -z "$prefix" ]]; then
		echo "live_s3_list: requires <bucket> <prefix>" >&2
		return 1
	fi
	# `--query 'Contents[].Key'` plus `--output text` prints tab-separated
	# keys; convert to one-per-line and strip the literal "None" that
	# `--output text` emits when Contents is empty.
	live_s3_aws s3api list-objects-v2 \
		--bucket "$bucket" --prefix "$prefix" \
		--query 'Contents[].Key' --output text 2>/dev/null \
		| tr '\t' '\n' \
		| awk 'NF && $0 != "None"' \
		|| true
}

# live_s3_get_object <bucket> <key> <out_file>
# Download <key> to <out_file>. Used by `assert_head_pointer`.
live_s3_get_object() {
	local bucket="$1"
	local key="$2"
	local out="$3"
	if [[ -z "$bucket" || -z "$key" || -z "$out" ]]; then
		echo "live_s3_get_object: requires <bucket> <key> <out_file>" >&2
		return 1
	fi
	live_s3_aws s3api get-object \
		--bucket "$bucket" --key "$key" "$out" >/dev/null
}

# live_s3_put_object <bucket> <key> <local_file>
# Upload <local_file> as <key> in <bucket>. Used by the sentinel
# pre-flight to verify write permission and by tests that pre-corrupt
# the bucket (stale-lock scenarios).
live_s3_put_object() {
	local bucket="$1"
	local key="$2"
	local file="$3"
	if [[ -z "$bucket" || -z "$key" || -z "$file" ]]; then
		echo "live_s3_put_object: requires <bucket> <key> <file>" >&2
		return 1
	fi
	live_s3_aws s3api put-object \
		--bucket "$bucket" --key "$key" --body "$file" >/dev/null
}

# live_s3_delete_object <bucket> <key>
# Delete a single object. 404 / NoSuchKey is treated as success so
# repeated cleanup is idempotent.
live_s3_delete_object() {
	local bucket="$1"
	local key="$2"
	if [[ -z "$bucket" || -z "$key" ]]; then
		echo "live_s3_delete_object: requires <bucket> <key>" >&2
		return 1
	fi
	live_s3_aws s3api delete-object \
		--bucket "$bucket" --key "$key" >/dev/null 2>&1 || true
}

# live_s3_clear_prefix <bucket> <prefix>
# Recursively delete every key under <prefix>. Refuses to run unless the
# prefix lives under `${LIVE_TOP_PREFIX}/` so a buggy caller cannot wipe
# the root of the bucket. Idempotent — missing-key errors are swallowed.
live_s3_clear_prefix() {
	local bucket="$1"
	local prefix="$2"
	if [[ -z "$bucket" || -z "$prefix" ]]; then
		echo "live_s3_clear_prefix: requires <bucket> <prefix>" >&2
		return 1
	fi
	if [[ "$prefix" != "${LIVE_TOP_PREFIX}/"* ]]; then
		echo "live_s3_clear_prefix: refusing to clear prefix '$prefix' (must start with '${LIVE_TOP_PREFIX}/')" >&2
		return 1
	fi
	# `aws s3 rm --recursive` on a non-existent prefix is already a
	# no-op success on AWS, so no extra masking is needed.
	live_s3_aws s3 rm "s3://${bucket}/${prefix}/" --recursive >/dev/null 2>&1 || true
}

# live_s3_preflight
# Sentinel write/read/delete under the run prefix to validate
# write+delete permissions BEFORE any scenario runs. Catches the
# silent-leak failure mode where a credential has put but not delete
# permission — without this, tests would appear to pass while leaving
# data behind. Each step's failure is reported by name so the operator
# knows which IAM action is missing.
live_s3_preflight() {
	live_assert_safe_prefix || return 1
	local key="${LIVE_RUN_PREFIX}/.preflight"
	local body downloaded
	body=$(mktemp -t live-preflight.XXXXXX) || {
		echo "live_s3_preflight: mktemp failed" >&2
		return 1
	}
	downloaded=$(mktemp -t live-preflight-dl.XXXXXX) || {
		rm -f "$body"
		echo "live_s3_preflight: mktemp failed" >&2
		return 1
	}
	echo "preflight" >"$body"

	if ! live_s3_put_object "$LIVE_S3_BUCKET" "$key" "$body" 2>/dev/null; then
		echo "live_s3_preflight: PUT failed (s3:PutObject)" >&2
		rm -f "$body" "$downloaded"
		return 1
	fi
	if ! live_s3_get_object "$LIVE_S3_BUCKET" "$key" "$downloaded" 2>/dev/null; then
		echo "live_s3_preflight: GET failed (s3:GetObject)" >&2
		live_s3_delete_object "$LIVE_S3_BUCKET" "$key"
		rm -f "$body" "$downloaded"
		return 1
	fi
	if ! cmp -s "$body" "$downloaded"; then
		echo "live_s3_preflight: round-trip body mismatch" >&2
		live_s3_delete_object "$LIVE_S3_BUCKET" "$key"
		rm -f "$body" "$downloaded"
		return 1
	fi
	if ! live_s3_aws s3api delete-object \
		--bucket "$LIVE_S3_BUCKET" --key "$key" >/dev/null 2>&1; then
		echo "live_s3_preflight: DELETE failed (s3:DeleteObject)" >&2
		rm -f "$body" "$downloaded"
		return 1
	fi
	rm -f "$body" "$downloaded"
}

# live_s3_setup
# `BeforeAll` entry point: load env file, validate guard / env / tools,
# allocate the run prefix, run the sentinel pre-flight, install the
# cleanup trap. Exports AWS env vars the spec helpers and the helper
# binary inherit (the helper uses the standard SDK chain).
live_s3_setup() {
	live_load_env_file
	live_require_guard || return 1
	live_require_cmd aws git jq || return 1
	live_s3_require_env || return 1
	live_init_run_prefix || return 1
	if ! live_s3_preflight; then
		echo "live_s3_setup: pre-flight failed; aborting before any test runs" >&2
		return 1
	fi
	# Re-export the region so the helper binary picks it up via
	# `AWS_REGION`. Profile is plumbed through the URL itself, not env.
	export AWS_REGION="$LIVE_S3_REGION"
	export AWS_PAGER=""
	# Trap on EXIT/INT/TERM so a Ctrl-C mid-suite still cleans up.
	# `live_s3_teardown` is the one-shot idempotent cleanup helper.
	trap 'live_s3_teardown' EXIT INT TERM
}

# live_s3_teardown
# `AfterAll` entry point AND the trap target. Idempotent: missing keys
# are treated as success. The prefix-safety guard inside
# `live_s3_clear_prefix` rejects an empty or malformed
# `LIVE_RUN_PREFIX`.
live_s3_teardown() {
	# `AfterAll` may run before `BeforeAll` allocated the prefix (e.g.
	# guard rejection in `live_s3_setup`). In that case there is
	# nothing to clean up.
	if [[ -z "${LIVE_RUN_PREFIX:-}" ]]; then
		return 0
	fi
	live_s3_clear_prefix "$LIVE_S3_BUCKET" "$LIVE_RUN_PREFIX"
	# Run the trap once: subsequent EXIT signals are no-ops.
	trap - EXIT INT TERM
}

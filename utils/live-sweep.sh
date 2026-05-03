#!/usr/bin/env bash
# Sweep stale `live-test/<run-id>/` prefixes from the live-cloud test
# bucket. Recovery path for SIGKILL-orphaned runs whose `AfterAll`
# cleanup did not fire.
#
# Run-ids encode a UTC timestamp as the first segment
# (`YYYYMMDDTHHMMSSZ-<pid>-<rand>`), so age comparison is a cheap
# lexicographic test against a synthetic cutoff string.
#
# Required env: same as the live S3 suite (LIVE_S3_BUCKET, LIVE_S3_REGION,
# optionally LIVE_S3_PROFILE), plus the
# `LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY=1` acknowledgement guard.

set -euo pipefail

AGE="24h"
COMMIT="0"

while [[ $# -gt 0 ]]; do
	case "$1" in
		--age)
			AGE="$2"
			shift 2
			;;
		--commit)
			COMMIT="$2"
			shift 2
			;;
		*)
			echo "live-sweep: unknown arg '$1'" >&2
			exit 2
			;;
	esac
done

# Source the same support modules the spec suite uses, so guard / env /
# helper functions stay in one place.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
export SHELLSPEC_PROJECT_ROOT="$PROJECT_ROOT"

# shellcheck disable=SC1091
. "$PROJECT_ROOT/spec/support/live_common.sh"
# shellcheck disable=SC1091
. "$PROJECT_ROOT/spec/support/live_s3.sh"

live_load_env_file
live_require_guard
live_require_cmd aws date awk
live_s3_require_env

# Convert a duration like `24h` / `7d` / `30m` to seconds. Accepts the
# `[smhd]` suffix; bare integer is treated as seconds.
duration_to_seconds() {
	local v="$1"
	case "$v" in
		*s) echo $((${v%s})) ;;
		*m) echo $((${v%m} * 60)) ;;
		*h) echo $((${v%h} * 3600)) ;;
		*d) echo $((${v%d} * 86400)) ;;
		*) echo "$v" ;;
	esac
}

cutoff_seconds=$(duration_to_seconds "$AGE")
# `date -u -d @<epoch>` formats a UTC timestamp matching the run-id's
# leading segment. The cutoff string is what every prefix older than
# AGE will sort before.
cutoff_epoch=$(($(date -u +%s) - cutoff_seconds))
cutoff_stamp=$(date -u -d "@${cutoff_epoch}" +%Y%m%dT%H%M%SZ)

echo "live-sweep: bucket=$LIVE_S3_BUCKET region=$LIVE_S3_REGION"
echo "live-sweep: cutoff=$cutoff_stamp (anything strictly less is a candidate)"

# List immediate sub-prefixes of `live-test/`. `--delimiter /` makes
# `CommonPrefixes` the per-run-id list; one entry per run.
mapfile -t run_prefixes < <(
	live_s3_aws s3api list-objects-v2 \
		--bucket "$LIVE_S3_BUCKET" \
		--prefix "${LIVE_TOP_PREFIX}/" \
		--delimiter "/" \
		--query 'CommonPrefixes[].Prefix' --output text 2>/dev/null \
		| tr '\t' '\n' \
		| awk 'NF && $0 != "None"' \
		|| true
)

if [[ ${#run_prefixes[@]} -eq 0 ]]; then
	echo "live-sweep: no run prefixes found under ${LIVE_TOP_PREFIX}/"
	exit 0
fi

stale=()
for p in "${run_prefixes[@]}"; do
	# Strip leading `live-test/` and trailing `/` to get the run-id.
	id="${p#"${LIVE_TOP_PREFIX}"/}"
	id="${id%/}"
	# The run-id's first segment up to `-` is the timestamp.
	stamp="${id%%-*}"
	if [[ "$stamp" < "$cutoff_stamp" ]]; then
		stale+=("$p")
	fi
done

if [[ ${#stale[@]} -eq 0 ]]; then
	echo "live-sweep: no stale prefixes (none older than $AGE)"
	exit 0
fi

echo "live-sweep: ${#stale[@]} stale prefix(es):"
printf '  %s\n' "${stale[@]}"

if [[ "$COMMIT" != "1" ]]; then
	echo
	echo "live-sweep: dry run — pass COMMIT=1 to delete."
	exit 0
fi

for p in "${stale[@]}"; do
	echo "live-sweep: deleting $p ..."
	# `live_s3_clear_prefix` enforces the live-test/ prefix safety guard.
	# Strip the trailing slash that list-objects-v2 includes.
	live_s3_clear_prefix "$LIVE_S3_BUCKET" "${p%/}"
done
echo "live-sweep: done."

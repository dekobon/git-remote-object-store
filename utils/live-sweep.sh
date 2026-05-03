#!/usr/bin/env bash
# Sweep stale `live-test/<run-id>/` prefixes from the live-cloud test
# backends. Recovery path for SIGKILL-orphaned runs whose `AfterAll`
# cleanup did not fire.
#
# Run-ids encode a UTC timestamp as the first segment
# (`YYYYMMDDTHHMMSSZ-<pid>-<rand>`), so age comparison is a cheap
# lexicographic test against a synthetic cutoff string.
#
# Backend selection (`--backend`):
#   - `s3`  — sweep AWS S3 only; requires LIVE_S3_BUCKET / LIVE_S3_REGION.
#   - `az`  — sweep Azure Blob only; requires LIVE_AZ_ACCOUNT /
#             LIVE_AZ_CONTAINER / LIVE_AZ_CREDENTIAL_NAME.
#   - `all` (default) — sweep every backend whose required env vars are
#             present; backends with incomplete env are skipped with a
#             warning, not a hard failure. The acknowledgement guard
#             still applies regardless of which backends are scanned.

set -euo pipefail

AGE="24h"
COMMIT="0"
BACKEND="all"

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
		--backend)
			BACKEND="$2"
			shift 2
			;;
		*)
			echo "live-sweep: unknown arg '$1'" >&2
			exit 2
			;;
	esac
done

case "$BACKEND" in
	s3 | az | all) ;;
	*)
		echo "live-sweep: --backend must be 's3', 'az', or 'all' (got '$BACKEND')" >&2
		exit 2
		;;
esac

# Source the same support modules the spec suite uses, so guard / env /
# helper functions stay in one place.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
export SHELLSPEC_PROJECT_ROOT="$PROJECT_ROOT"

# shellcheck disable=SC1091
. "$PROJECT_ROOT/spec/support/live_common.sh"
# shellcheck disable=SC1091
. "$PROJECT_ROOT/spec/support/live_s3.sh"
# shellcheck disable=SC1091
. "$PROJECT_ROOT/spec/support/live_az.sh"

live_load_env_file
live_require_guard
live_require_cmd date awk

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

echo "live-sweep: cutoff=$cutoff_stamp (anything strictly less is a candidate)"

# Print stale prefixes from the supplied newline-delimited list. The
# run-id's first `-`-separated segment is the timestamp; older than
# cutoff = stale.
filter_stale() {
	local stamp id
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

# sweep_s3 — list, report, optionally delete stale S3 run-prefixes.
sweep_s3() {
	if ! command -v aws >/dev/null 2>&1; then
		echo "live-sweep: skipping S3 — aws CLI not on PATH" >&2
		return 0
	fi
	if [[ -z "${LIVE_S3_BUCKET:-}" || -z "${LIVE_S3_REGION:-}" ]]; then
		echo "live-sweep: skipping S3 — LIVE_S3_BUCKET / LIVE_S3_REGION not set" >&2
		return 0
	fi
	echo "live-sweep[s3]: bucket=$LIVE_S3_BUCKET region=$LIVE_S3_REGION"

	# `--delimiter /` makes `CommonPrefixes` the per-run-id list.
	local listing
	listing=$(
		live_s3_aws s3api list-objects-v2 \
			--bucket "$LIVE_S3_BUCKET" \
			--prefix "${LIVE_TOP_PREFIX}/" \
			--delimiter "/" \
			--query 'CommonPrefixes[].Prefix' --output text 2>/dev/null \
			| tr '\t' '\n' \
			| awk 'NF && $0 != "None"' \
			|| true
	)
	if [[ -z "$listing" ]]; then
		echo "live-sweep[s3]: no run prefixes found under ${LIVE_TOP_PREFIX}/"
		return 0
	fi
	mapfile -t stale < <(printf '%s\n' "$listing" | filter_stale)
	if [[ ${#stale[@]} -eq 0 ]]; then
		echo "live-sweep[s3]: no stale prefixes (none older than $AGE)"
		return 0
	fi
	echo "live-sweep[s3]: ${#stale[@]} stale prefix(es):"
	printf '  %s\n' "${stale[@]}"
	[[ "$COMMIT" != "1" ]] && return 0
	local p
	for p in "${stale[@]}"; do
		echo "live-sweep[s3]: deleting $p ..."
		live_s3_clear_prefix "$LIVE_S3_BUCKET" "${p%/}"
	done
}

# sweep_az — list, report, optionally delete stale Azure run-prefixes.
sweep_az() {
	if ! command -v az >/dev/null 2>&1; then
		echo "live-sweep: skipping Azure — az CLI not on PATH" >&2
		return 0
	fi
	if [[ -z "${LIVE_AZ_ACCOUNT:-}" || -z "${LIVE_AZ_CONTAINER:-}" ||
		-z "${LIVE_AZ_CREDENTIAL_NAME:-}" ]]; then
		echo "live-sweep: skipping Azure — LIVE_AZ_ACCOUNT / LIVE_AZ_CONTAINER / LIVE_AZ_CREDENTIAL_NAME not set" >&2
		return 0
	fi
	if ! live_az_credential_env_value >/dev/null; then
		echo "live-sweep: skipping Azure — credential alias '$LIVE_AZ_CREDENTIAL_NAME' has no env var set" >&2
		return 0
	fi
	echo "live-sweep[az]: account=$LIVE_AZ_ACCOUNT container=$LIVE_AZ_CONTAINER"

	# Plain blob containers do not expose virtual-directory listings
	# through `--query '[].name'` (JMESPath only sees blob entries, not
	# the `BlobPrefix` collection that `--delimiter` populates). The
	# robust portable approach: list every blob under `live-test/` and
	# dedupe by the first two path segments (`live-test/<run-id>`).
	# `az storage fs directory list` would be cleaner but only works on
	# ADLS Gen2 accounts.
	local listing
	listing=$(
		live_az blob list --container-name "$LIVE_AZ_CONTAINER" \
			--prefix "${LIVE_TOP_PREFIX}/" \
			--query '[].name' -o tsv 2>/dev/null \
			| tr '\t' '\n' \
			| awk -F/ 'NF >= 2 { print $1 "/" $2 "/" }' \
			| sort -u \
			|| true
	)
	if [[ -z "$listing" ]]; then
		echo "live-sweep[az]: no run prefixes found under ${LIVE_TOP_PREFIX}/"
		return 0
	fi
	mapfile -t stale < <(printf '%s\n' "$listing" | filter_stale)
	if [[ ${#stale[@]} -eq 0 ]]; then
		echo "live-sweep[az]: no stale prefixes (none older than $AGE)"
		return 0
	fi
	echo "live-sweep[az]: ${#stale[@]} stale prefix(es):"
	printf '  %s\n' "${stale[@]}"
	[[ "$COMMIT" != "1" ]] && return 0
	local p
	for p in "${stale[@]}"; do
		echo "live-sweep[az]: deleting $p ..."
		live_az_clear_prefix "$LIVE_AZ_CONTAINER" "${p%/}"
	done
}

case "$BACKEND" in
	s3) sweep_s3 ;;
	az) sweep_az ;;
	all)
		sweep_s3
		sweep_az
		;;
esac

if [[ "$COMMIT" != "1" ]]; then
	echo
	echo "live-sweep: dry run — pass COMMIT=1 to delete."
fi
echo "live-sweep: done."

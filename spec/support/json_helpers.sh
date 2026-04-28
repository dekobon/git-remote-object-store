# shellcheck shell=bash

# JSON helpers for shellspec. Source from spec files that need to assert on
# JSON output (e.g. management-CLI subcommands that emit `--format json`).
# Requires `jq` on PATH; specs that use these helpers should declare jq as
# a prerequisite via Skip if ! command -v jq.

# json_field <json> <jq-path>
# Print the string value at <jq-path> in <json>, or empty string if absent.
json_field() {
	local json="$1"
	local path="$2"
	echo "$json" | jq -r "$path // empty"
}

# json_has <json> <jq-path>
# Return 0 if <jq-path> exists and is non-null, 1 otherwise.
json_has() {
	local json="$1"
	local path="$2"
	[[ "$(echo "$json" | jq "$path != null")" == "true" ]]
}

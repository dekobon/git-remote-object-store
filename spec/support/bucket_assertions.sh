# shellcheck shell=bash

# On-bucket layout assertions. Every helper takes a "lister" function
# name as its first argument so the same assertions work against rustfs
# and Azurite. The lister is invoked as `<lister> <bucket> <prefix>`
# and prints one key per line.

# tombstoned_bundle_keys <lister> <getter> <bucket> <prefix>
# Print the set of bundle keys (full path) that have a matching baseline
# tombstone under <prefix>/gc/. Mirrors `tombstoned_bundle_keys` in
# `src/packchain/gc.rs`: the bundle engine defers prior-bundle deletion
# (issue #157) so the on-bucket listing temporarily contains both the
# live bundle and any tombstoned predecessors during the gc grace
# window. White-box assertions on "logical" bucket state need to
# subtract the tombstoned set to recover the pre-#157 semantics.
#
# Requires `jq` on PATH (already a hard prereq of the live + integration
# suites). Each tombstone body is JSON with `.ref_name` and `.sha`
# fields (see `BaselineTombstone` in `src/packchain/gc.rs`); the bundle
# key is reconstructed via `<prefix>/<ref_name>/<sha>.bundle` to match
# the canonical `keys::bundle_key` shape.
tombstoned_bundle_keys() {
	local lister="$1"
	local getter="$2"
	local bucket="$3"
	local prefix="$4"
	local tomb_prefix="${prefix}/gc/baseline-tomb-"
	local tomb_keys
	tomb_keys=$("$lister" "$bucket" "$prefix" \
		| awk -v t="$tomb_prefix" 'index($0, t) == 1 && /\.json$/')
	if [[ -z "$tomb_keys" ]]; then
		return 0
	fi
	local tmp key body ref sha
	tmp=$(mktemp -t baseline-tomb.XXXXXX) || {
		echo "tombstoned_bundle_keys: mktemp failed" >&2
		return 1
	}
	# `IFS=` + `-r` preserves the literal key, then we trim by hand.
	while IFS= read -r key; do
		[[ -z "$key" ]] && continue
		if ! "$getter" "$bucket" "$key" "$tmp" >/dev/null 2>&1; then
			rm -f "$tmp"
			echo "tombstoned_bundle_keys: failed to download $key" >&2
			return 1
		fi
		body=$(<"$tmp")
		ref=$(jq -r '.ref_name // empty' <<<"$body")
		sha=$(jq -r '.sha // empty' <<<"$body")
		if [[ -z "$ref" || -z "$sha" ]]; then
			rm -f "$tmp"
			echo "tombstoned_bundle_keys: malformed tombstone at $key" >&2
			return 1
		fi
		printf '%s/%s/%s.bundle\n' "$prefix" "$ref" "$sha"
	done <<<"$tomb_keys"
	rm -f "$tmp"
}

# bundle_keys <lister> <bucket> <prefix> <ref> [getter]
# Print the bundle keys (full path) under <prefix>/<ref>/.
# `index($0, target) == 1` is a literal-substring match (no regex), so
# `$prefix` / `$ref` containing `.`, `+`, `*`, `[` cannot turn into
# wildcards. Only the trailing `<sha>.bundle` shape is matched as a
# regex.
#
# When the optional 5th arg <getter> is provided, the result is filtered
# to exclude bundle keys that have a matching baseline tombstone (see
# `tombstoned_bundle_keys`). Tests that exercise A→B transitions on the
# bundle engine must pass a getter so the assertion reflects logical
# (live) state, not the raw on-bucket listing that includes
# soon-to-be-reclaimed predecessors.
bundle_keys() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	local getter="${5:-}"
	local target="${prefix}/${ref}/"
	local all_keys
	all_keys=$("$lister" "$bucket" "$prefix" \
		| awk -v t="$target" 'index($0, t) == 1 && /\/[0-9a-f]+\.bundle$/')
	local tombstoned=""
	if [[ -n "$getter" ]]; then
		tombstoned=$(tombstoned_bundle_keys "$lister" "$getter" "$bucket" "$prefix") || return 1
	fi
	if [[ -z "$tombstoned" ]]; then
		# An empty `all_keys` is a valid result (zero bundles under
		# the ref — e.g. immediately after a delete) so the helper
		# must return 0; an explicit `return 0` avoids inheriting the
		# `[[ -n ]] &&` short-circuit's exit-1 status.
		if [[ -n "$all_keys" ]]; then
			echo "$all_keys"
		fi
		return 0
	fi
	# `grep -Fxvf <tombstoned> <all_keys>` returns 1 when every input
	# line is filtered out (an empty result). That is a valid outcome
	# here — every observed bundle was tombstoned — so the `|| true`
	# pin keeps the helper from failing.
	grep -Fxvf <(echo "$tombstoned") <(echo "$all_keys") || true
}

# assert_bundle_count <lister> <bucket> <prefix> <ref> <expected> [getter]
# Pass <getter> to filter tombstoned bundles from the count; required on
# bundle-engine tests that exercise A→B transitions (see `bundle_keys`).
assert_bundle_count() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	local expected="$5"
	local getter="${6:-}"
	# Split the pipe so `bundle_keys`'s non-zero exit (e.g. a
	# `tombstoned_bundle_keys` failure on getter / mktemp / malformed
	# JSON) propagates up instead of being masked by `grep -c`'s
	# zero-input "count == 0" result.
	local keys actual
	keys=$(bundle_keys "$lister" "$bucket" "$prefix" "$ref" "$getter") || return 1
	actual=$(echo "$keys" | grep -c . || true)
	if [[ "$actual" != "$expected" ]]; then
		echo "assert_bundle_count: $prefix/$ref/ has $actual bundle(s), expected $expected" >&2
		"$lister" "$bucket" "$prefix" >&2 || true
		return 1
	fi
}

# assert_bundle_sha_for_ref <lister> <bucket> <prefix> <ref> <sha> [getter]
# Fail unless exactly one bundle exists under <prefix>/<ref>/ and its
# basename is <sha>.bundle. Pass <getter> to filter tombstoned bundles
# (see `bundle_keys`).
assert_bundle_sha_for_ref() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	local sha="$5"
	local getter="${6:-}"
	local keys
	# Propagate `bundle_keys` errors directly rather than letting them
	# surface as "found 0 bundles" with the original cause hidden on
	# stderr — see `assert_bundle_count` for the same fix.
	keys=$(bundle_keys "$lister" "$bucket" "$prefix" "$ref" "$getter") || return 1
	local count
	count=$(echo "$keys" | grep -c . || true)
	if [[ "$count" != "1" ]]; then
		echo "assert_bundle_sha_for_ref: expected 1 bundle, found $count" >&2
		echo "$keys" >&2
		return 1
	fi
	local expected="${prefix}/${ref}/${sha}.bundle"
	if [[ "$keys" != "$expected" ]]; then
		echo "assert_bundle_sha_for_ref: bundle is '$keys', expected '$expected'" >&2
		return 1
	fi
}

# assert_protected_marker <lister> <bucket> <prefix> <ref>
# Fail unless <prefix>/<ref>/PROTECTED# is present.
assert_protected_marker() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	local key="${prefix}/${ref}/PROTECTED#"
	if ! "$lister" "$bucket" "$prefix" | grep -Fxq "$key"; then
		echo "assert_protected_marker: $key not found" >&2
		"$lister" "$bucket" "$prefix" >&2 || true
		return 1
	fi
}

# assert_no_protected_marker <lister> <bucket> <prefix> <ref>
assert_no_protected_marker() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	local key="${prefix}/${ref}/PROTECTED#"
	if "$lister" "$bucket" "$prefix" | grep -Fxq "$key"; then
		echo "assert_no_protected_marker: $key still present" >&2
		return 1
	fi
}

# lock_keys <lister> <bucket> <prefix> <ref>
# Print every *.lock key under <prefix>/<ref>/. Mirrors `bundle_keys`
# in using a literal-substring prefix match so regex metacharacters in
# `$prefix` / `$ref` cannot turn into wildcards.
lock_keys() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	local target="${prefix}/${ref}/"
	"$lister" "$bucket" "$prefix" \
		| awk -v t="$target" 'index($0, t) == 1 && /\.lock$/'
}

# assert_lock_present <lister> <bucket> <prefix> <ref>
# Fail unless a *.lock key exists under <prefix>/<ref>/.
assert_lock_present() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	if [[ -z "$(lock_keys "$lister" "$bucket" "$prefix" "$ref")" ]]; then
		echo "assert_lock_present: no *.lock under $prefix/$ref/" >&2
		"$lister" "$bucket" "$prefix" >&2 || true
		return 1
	fi
}

# assert_lock_absent <lister> <bucket> <prefix> <ref>
assert_lock_absent() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	if [[ -n "$(lock_keys "$lister" "$bucket" "$prefix" "$ref")" ]]; then
		echo "assert_lock_absent: stray *.lock under $prefix/$ref/" >&2
		"$lister" "$bucket" "$prefix" >&2 || true
		return 1
	fi
}

# assert_chain_present <lister> <bucket> <prefix> <ref>
# Fail unless <prefix>/<ref>/chain.json is present. Packchain engine's
# per-ref manifest — the "engine equivalent" of the bundle key under
# `<prefix>/<ref>/<sha>.bundle` for white-box assertions on packchain
# spec runs.
assert_chain_present() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	local key="${prefix}/${ref}/chain.json"
	if ! "$lister" "$bucket" "$prefix" | grep -Fxq "$key"; then
		echo "assert_chain_present: $key not found" >&2
		"$lister" "$bucket" "$prefix" >&2 || true
		return 1
	fi
}

# assert_chain_absent <lister> <bucket> <prefix> <ref>
# Symmetric to `assert_chain_present`.
assert_chain_absent() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	local key="${prefix}/${ref}/chain.json"
	if "$lister" "$bucket" "$prefix" | grep -Fxq "$key"; then
		echo "assert_chain_absent: $key unexpectedly present" >&2
		return 1
	fi
}

# assert_path_index_present <lister> <bucket> <prefix> <ref>
# Fail unless <prefix>/<ref>/path-index.json is present. Companion to
# `assert_chain_present` — packchain writes chain.json AND
# path-index.json side-by-side, and a successful delete sweeps both
# together (see tests/protocol_push_packchain.rs::delete_remote_ref_removes_chain_and_path_index).
# Asserting both on refusal pins that a partial-sweep regression
# (e.g. one key swept but not the other) is caught.
assert_path_index_present() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	local key="${prefix}/${ref}/path-index.json"
	if ! "$lister" "$bucket" "$prefix" | grep -Fxq "$key"; then
		echo "assert_path_index_present: $key not found" >&2
		"$lister" "$bucket" "$prefix" >&2 || true
		return 1
	fi
}

# assert_lfs_object_exists <lister> <bucket> <prefix> <oid>
# Fail unless <prefix>/lfs/<oid> is present.
assert_lfs_object_exists() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local oid="$4"
	local key="${prefix}/lfs/${oid}"
	if ! "$lister" "$bucket" "$prefix" | grep -Fxq "$key"; then
		echo "assert_lfs_object_exists: $key not found" >&2
		"$lister" "$bucket" "$prefix" >&2 || true
		return 1
	fi
}

# assert_head_pointer <getter> <bucket> <prefix> <ref>
# Download <prefix>/HEAD and assert its content equals <ref>. <getter>
# is a function name like `rustfs_get_object` / `azurite_get_object`.
assert_head_pointer() {
	local getter="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	local tmp
	# `mktemp` (not `$$`) so parallel shellspec runs don't share the
	# same path and clobber each other's HEAD body.
	tmp=$(mktemp -t HEAD.XXXXXX) || {
		echo "assert_head_pointer: mktemp failed" >&2
		return 1
	}
	if ! "$getter" "$bucket" "${prefix}/HEAD" "$tmp"; then
		echo "assert_head_pointer: failed to download $prefix/HEAD" >&2
		rm -f "$tmp"
		return 1
	fi
	local actual
	actual=$(<"$tmp")
	rm -f "$tmp"
	# `HEAD` body is the bare ref name (e.g. `refs/heads/main`).
	if [[ "$actual" != "$ref" ]]; then
		echo "assert_head_pointer: HEAD is '$actual', expected '$ref'" >&2
		return 1
	fi
}

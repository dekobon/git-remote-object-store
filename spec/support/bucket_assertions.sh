# shellcheck shell=bash

# On-bucket layout assertions. Every helper takes a "lister" function
# name as its first argument so the same assertions work against rustfs
# and Azurite. The lister is invoked as `<lister> <bucket> <prefix>`
# and prints one key per line.

# bundle_keys <lister> <bucket> <prefix> <ref>
# Print the bundle keys (full path) under <prefix>/<ref>/.
# `index($0, target) == 1` is a literal-substring match (no regex), so
# `$prefix` / `$ref` containing `.`, `+`, `*`, `[` cannot turn into
# wildcards. Only the trailing `<sha>.bundle` shape is matched as a
# regex.
bundle_keys() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	local target="${prefix}/${ref}/"
	"$lister" "$bucket" "$prefix" \
		| awk -v t="$target" 'index($0, t) == 1 && /\/[0-9a-f]+\.bundle$/'
}

# assert_bundle_count <lister> <bucket> <prefix> <ref> <expected>
assert_bundle_count() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	local expected="$5"
	local actual
	actual=$(bundle_keys "$lister" "$bucket" "$prefix" "$ref" | wc -l | tr -d ' ')
	if [[ "$actual" != "$expected" ]]; then
		echo "assert_bundle_count: $prefix/$ref/ has $actual bundle(s), expected $expected" >&2
		"$lister" "$bucket" "$prefix" >&2 || true
		return 1
	fi
}

# assert_bundle_sha_for_ref <lister> <bucket> <prefix> <ref> <sha>
# Fail unless exactly one bundle exists under <prefix>/<ref>/ and its
# basename is <sha>.bundle.
assert_bundle_sha_for_ref() {
	local lister="$1"
	local bucket="$2"
	local prefix="$3"
	local ref="$4"
	local sha="$5"
	local keys
	keys=$(bundle_keys "$lister" "$bucket" "$prefix" "$ref")
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
	# `HEAD` body is the bare ref name per upstream layout
	# (`git_remote_s3/remote.py` writes `refs/heads/<branch>`).
	if [[ "$actual" != "$ref" ]]; then
		echo "assert_head_pointer: HEAD is '$actual', expected '$ref'" >&2
		return 1
	fi
}

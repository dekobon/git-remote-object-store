# shellcheck shell=bash

# Pure-shell scenario helpers. No backend awareness: every function
# takes explicit args. Both backends call the same scenario steps
# from their respective spec files.
#
# The push/clone/fetch helpers expose two globals so shellspec's `When
# call` and `The status should equal` can introspect:
#   LAST_GIT_OUTPUT  — combined stdout/stderr of the last git command
#   LAST_GIT_STATUS  — exit code of the last git command

LAST_GIT_OUTPUT=""
LAST_GIT_STATUS=0

# git_scenarios_init <repo_dir> <name> <email>
# Configure repo-local git identity (no global writes).
git_scenarios_init() {
	local dir="$1"
	local name="${2:-shellspec}"
	local email="${3:-shellspec@example.invalid}"
	git -C "$dir" config user.name "$name"
	git -C "$dir" config user.email "$email"
	# Speed: avoid GPG signing in tests even if the host configured it.
	git -C "$dir" config commit.gpgsign false
	git -C "$dir" config tag.gpgsign false
}

# mk_local_repo <dir>
# Initialise a fresh repo at <dir> with the default branch `main`.
mk_local_repo() {
	local dir="$1"
	if [[ -z "$dir" ]]; then
		echo "mk_local_repo: missing <dir>" >&2
		return 1
	fi
	mkdir -p "$dir"
	git -C "$dir" init -q -b main
	git_scenarios_init "$dir"
}

# commit_in_repo <dir> <file> <content> <msg>
# Write <content> to <dir>/<file>, add, commit. Echoes the new HEAD SHA.
# Each step is chained with `&&` so a failure (e.g. empty diff,
# pre-commit hook reject) propagates instead of silently returning the
# old HEAD as if a fresh commit had landed.
commit_in_repo() {
	local dir="$1"
	local file="$2"
	local content="$3"
	local msg="$4"
	if [[ -z "$dir" || -z "$file" || -z "$msg" ]]; then
		echo "commit_in_repo: requires <dir> <file> <content> <msg>" >&2
		return 1
	fi
	mkdir -p "$(dirname "$dir/$file")" \
		&& printf '%s' "$content" >"$dir/$file" \
		&& git -C "$dir" add "$file" \
		&& git -C "$dir" commit -q -m "$msg" \
		&& git -C "$dir" rev-parse HEAD
}

# tag_in_repo <dir> <name> [-m msg]
# Create an annotated tag (lightweight if -m not provided).
tag_in_repo() {
	local dir="$1"
	local name="$2"
	if [[ -z "$dir" || -z "$name" ]]; then
		echo "tag_in_repo: requires <dir> <name>" >&2
		return 1
	fi
	shift 2
	if (($# == 0)); then
		git -C "$dir" tag "$name"
	else
		git -C "$dir" tag -a "$name" "$@"
	fi
}

# add_remote <dir> <name> <url>
# Add a remote named <name> with URL <url> to the repo at <dir>.
add_remote() {
	local dir="$1"
	local name="$2"
	local url="$3"
	if [[ -z "$dir" || -z "$name" || -z "$url" ]]; then
		echo "add_remote: requires <dir> <name> <url>" >&2
		return 1
	fi
	git -C "$dir" remote add "$name" "$url"
}

# capture_git <dir> <git-args...>
# Run `git -C <dir> <args>` capturing combined stdout/stderr into
# LAST_GIT_OUTPUT and exit code into LAST_GIT_STATUS. Returns
# LAST_GIT_STATUS so `When call` can pick it up.
capture_git() {
	local dir="$1"
	shift
	LAST_GIT_OUTPUT=$(git -C "$dir" "$@" 2>&1)
	LAST_GIT_STATUS=$?
	if ((LAST_GIT_STATUS != 0)); then
		echo "$LAST_GIT_OUTPUT" >&2
	fi
	return "$LAST_GIT_STATUS"
}

# push_branch <dir> <remote> <refspec> [extra-args...]
# `git push` wrapper. <refspec> is the bare refspec (e.g.
# `refs/heads/main:refs/heads/main` or `:refs/heads/feature` for
# delete). Pass `+refs/heads/x:refs/heads/x` for force-push.
push_branch() {
	local dir="$1"
	local remote="$2"
	local refspec="$3"
	shift 3
	capture_git "$dir" push "$remote" "$refspec" "$@"
}

# shallow_clone_remote <depth> <url> <dir>
# `git clone --depth <depth>` wrapper. Sets LAST_GIT_OUTPUT / LAST_GIT_STATUS.
shallow_clone_remote() {
	local depth="$1"
	local url="$2"
	local dir="$3"
	if [[ -z "$depth" || -z "$url" || -z "$dir" ]]; then
		echo "shallow_clone_remote: requires <depth> <url> <dir>" >&2
		return 1
	fi
	LAST_GIT_OUTPUT=$(git clone --depth "$depth" "$url" "$dir" 2>&1)
	LAST_GIT_STATUS=$?
	if ((LAST_GIT_STATUS != 0)); then
		echo "$LAST_GIT_OUTPUT" >&2
	fi
	return "$LAST_GIT_STATUS"
}

# clone_remote <url> <dir>
# `git clone` wrapper. Returns the exit code; LAST_GIT_OUTPUT has the
# combined output.
clone_remote() {
	local url="$1"
	local dir="$2"
	if [[ -z "$url" || -z "$dir" ]]; then
		echo "clone_remote: requires <url> <dir>" >&2
		return 1
	fi
	LAST_GIT_OUTPUT=$(git clone "$url" "$dir" 2>&1)
	LAST_GIT_STATUS=$?
	if ((LAST_GIT_STATUS != 0)); then
		echo "$LAST_GIT_OUTPUT" >&2
	fi
	return "$LAST_GIT_STATUS"
}

# fetch_remote <dir> <remote> [extra-args...]
# `git fetch` wrapper.
fetch_remote() {
	local dir="$1"
	local remote="$2"
	shift 2
	capture_git "$dir" fetch "$remote" "$@"
}

# ls_remote <url>
# `git ls-remote` against the URL; output goes to stdout (caller
# consumes). Sets LAST_GIT_STATUS.
ls_remote() {
	local url="$1"
	LAST_GIT_OUTPUT=$(git ls-remote "$url" 2>&1)
	LAST_GIT_STATUS=$?
	if ((LAST_GIT_STATUS != 0)); then
		echo "$LAST_GIT_OUTPUT" >&2
		return "$LAST_GIT_STATUS"
	fi
	echo "$LAST_GIT_OUTPUT"
}

# resolve_sha <dir> <rev>
# Print the SHA of <rev> in <dir>.
resolve_sha() {
	local dir="$1"
	local rev="$2"
	git -C "$dir" rev-parse "$rev"
}

# assert_git_sha_equals <dir> <rev> <expected_sha>
# Fail (return non-zero) with a diagnostic if <rev> in <dir> does not
# resolve to <expected_sha>.
assert_git_sha_equals() {
	local dir="$1"
	local rev="$2"
	local expected="$3"
	local actual
	actual=$(git -C "$dir" rev-parse "$rev" 2>/dev/null)
	if [[ "$actual" != "$expected" ]]; then
		echo "assert_git_sha_equals: $rev in $dir is $actual, expected $expected" >&2
		return 1
	fi
}

# commit_count <dir>
# Print the number of commits reachable from HEAD (respects .git/shallow).
commit_count() {
	local dir="$1"
	git -C "$dir" log --oneline | wc -l | tr -d ' '
}

# assert_shallow_file_exists <dir>
# Fail unless <dir>/.git/shallow is present (non-empty — an all-grafted
# shallow clone always has at least one boundary entry).
assert_shallow_file_exists() {
	local dir="$1"
	if [[ ! -s "${dir}/.git/shallow" ]]; then
		echo "assert_shallow_file_exists: ${dir}/.git/shallow missing or empty" >&2
		return 1
	fi
}

# build_linear_history <src> <url> <n>
# Initialise a repo at <src>, push <n> sequential commits to <url>, and
# echo the tip SHA. Suitable for both S3 and Azure backends (the URL
# grammar is the only backend-specific detail).
build_linear_history() {
	local src="$1"
	local url="$2"
	local n="$3"
	local i sha
	if [[ -z "$src" || -z "$url" || -z "$n" ]]; then
		echo "build_linear_history: requires <src> <url> <n>" >&2
		return 1
	fi
	mk_local_repo "$src"
	add_remote "$src" origin "$url"
	for ((i = 1; i <= n; i++)); do
		sha=$(commit_in_repo "$src" "file${i}.txt" "content ${i}" "commit ${i}")
	done
	push_branch "$src" origin refs/heads/main:refs/heads/main
	echo "$sha"
}

# assert_commit_count <dir> <expected>
# Fail unless git log reports exactly <expected> commits from HEAD.
assert_commit_count() {
	local dir="$1"
	local expected="$2"
	local actual
	actual=$(commit_count "$dir")
	if [[ "$actual" != "$expected" ]]; then
		echo "assert_commit_count: $dir has $actual commit(s), expected $expected" >&2
		git -C "$dir" log --oneline >&2 || true
		return 1
	fi
}

# shellcheck shell=bash
# shellcheck disable=SC2154 # variables defined by shellspec hooks

Describe "Azure helper: shallow fetch"
	Include spec/support/images.sh
	Include spec/support/docker_backend.sh
	Include spec/support/azurite.sh
	Include spec/support/git_scenarios.sh

	Skip if "set INTEGRATION_AZ=1 to enable" flag_unset INTEGRATION_AZ
	Skip if "docker not on PATH" missing_cmd docker
	Skip if "az-cli not on PATH" missing_cmd az
	Skip if "git not on PATH" missing_cmd git

	BeforeAll 'azurite_start'
	AfterAll 'azurite_stop'

	# build_linear_history <src> <url> <n>
	# Push <n> commits to <url> from <src>. Echoes the tip SHA.
	build_linear_history() {
		local src="$1"
		local url="$2"
		local n="$3"
		local i sha
		mk_local_repo "$src"
		add_remote "$src" origin "$url"
		for ((i = 1; i <= n; i++)); do
			sha=$(commit_in_repo "$src" "file${i}.txt" "content ${i}" "commit ${i}")
		done
		push_branch "$src" origin refs/heads/main:refs/heads/main
		echo "$sha"
	}

	Describe "git clone --depth 1 on a 5-commit history"
		# Split setup/It so the clone is the observable action. If setup
		# silently failed to push, both depth and full clone would produce
		# zero commits — the depth constraint would pass vacuously.
		setup() {
			CONTAINER=$(azurite_unique_container)
			PREFIX="myrepo"
			azurite_make_container "$CONTAINER"
			URL=$(azurite_url "$CONTAINER" "$PREFIX")
			SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
			DST="$SHELLSPEC_TMPDIR/dst-$$-$RANDOM"
			TIP_SHA=$(build_linear_history "$SRC" "$URL" 5)
		}
		BeforeEach 'setup'

		do_clone_depth_1() { shallow_clone_remote 1 "$URL" "$DST"; }

		It "writes .git/shallow and limits git log to 1 commit"
			When call do_clone_depth_1
			The status should equal 0
			assert_shallow_file_exists "$DST"
			assert_git_sha_equals "$DST" HEAD "$TIP_SHA"
			assert_commit_count "$DST" 1
		End
	End

	Describe "git clone --depth 3 on a 5-commit history"
		setup() {
			CONTAINER=$(azurite_unique_container)
			PREFIX="myrepo"
			azurite_make_container "$CONTAINER"
			URL=$(azurite_url "$CONTAINER" "$PREFIX")
			SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
			DST="$SHELLSPEC_TMPDIR/dst-$$-$RANDOM"
			TIP_SHA=$(build_linear_history "$SRC" "$URL" 5)
		}
		BeforeEach 'setup'

		do_clone_depth_3() { shallow_clone_remote 3 "$URL" "$DST"; }

		It "writes .git/shallow and limits git log to 3 commits"
			When call do_clone_depth_3
			The status should equal 0
			assert_shallow_file_exists "$DST"
			assert_git_sha_equals "$DST" HEAD "$TIP_SHA"
			assert_commit_count "$DST" 3
		End
	End

	Describe "git fetch --depth 3 deepens a depth-1 shallow clone"
		# Setup clones at depth=1 so the deepen is the observable
		# action in the It body. If depth-1 clone silently fetched all
		# 5 commits, the fetch --depth 3 would still show 5 and the
		# assert_commit_count check would not catch it.
		setup() {
			CONTAINER=$(azurite_unique_container)
			PREFIX="myrepo"
			azurite_make_container "$CONTAINER"
			URL=$(azurite_url "$CONTAINER" "$PREFIX")
			SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
			DST="$SHELLSPEC_TMPDIR/dst-$$-$RANDOM"
			TIP_SHA=$(build_linear_history "$SRC" "$URL" 5)
			shallow_clone_remote 1 "$URL" "$DST" >/dev/null
			assert_commit_count "$DST" 1
		}
		BeforeEach 'setup'

		do_deepen() { fetch_remote "$DST" origin --depth=3; }

		It "shows 3 commits in git log after deepening"
			When call do_deepen
			The status should equal 0
			assert_commit_count "$DST" 3
		End
	End
End

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

	Describe "git fetch --depth 1 from a depth-3 clone re-shallows"
		# The reverse of deepen: existing shallow boundary at depth-3
		# must be pruned in favour of the depth-1 boundary. The
		# stale-pruning logic is symmetric across depth changes.
		setup() {
			CONTAINER=$(azurite_unique_container)
			PREFIX="myrepo"
			azurite_make_container "$CONTAINER"
			URL=$(azurite_url "$CONTAINER" "$PREFIX")
			SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
			DST="$SHELLSPEC_TMPDIR/dst-$$-$RANDOM"
			TIP_SHA=$(build_linear_history "$SRC" "$URL" 5)
			shallow_clone_remote 3 "$URL" "$DST" >/dev/null
			assert_commit_count "$DST" 3
		}
		BeforeEach 'setup'

		do_re_shallow() { fetch_remote "$DST" origin --depth=1; }

		It "shows 1 commit after re-shallowing"
			When call do_re_shallow
			The status should equal 0
			assert_shallow_file_exists "$DST"
			assert_commit_count "$DST" 1
		End
	End

	Describe "git fetch --depth N (N >= history) removes the shallow file"
		# When the requested depth exceeds the available history, no
		# new boundary is needed. The pre-existing depth-1 entry must
		# also be pruned because its parents are now in the ODB. The
		# file must be unlinked — its presence alone signals shallow
		# semantics to git.
		setup() {
			CONTAINER=$(azurite_unique_container)
			PREFIX="myrepo"
			azurite_make_container "$CONTAINER"
			URL=$(azurite_url "$CONTAINER" "$PREFIX")
			SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
			DST="$SHELLSPEC_TMPDIR/dst-$$-$RANDOM"
			TIP_SHA=$(build_linear_history "$SRC" "$URL" 3)
			shallow_clone_remote 1 "$URL" "$DST" >/dev/null
			assert_commit_count "$DST" 1
		}
		BeforeEach 'setup'

		do_deepen_to_full() { fetch_remote "$DST" origin --depth=10; }

		It "unlinks .git/shallow and exposes full history"
			When call do_deepen_to_full
			The status should equal 0
			assert_shallow_file_absent "$DST"
			assert_commit_count "$DST" 3
		End
	End

	Describe "successive deepening 1 -> 2 -> 3"
		# Multiple consecutive deepens must each prune the prior
		# boundary. Catches a regression where pruning only handled
		# the depth-1 -> depth-N transition and not arbitrary chains.
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

		do_deepen_chain() {
			fetch_remote "$DST" origin --depth=2 >/dev/null || return $?
			assert_commit_count "$DST" 2 || return 1
			fetch_remote "$DST" origin --depth=3
		}

		It "shows 3 commits after the chained deepens"
			When call do_deepen_chain
			The status should equal 0
			assert_commit_count "$DST" 3
		End
	End
End

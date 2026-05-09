# shellcheck shell=bash
# shellcheck disable=SC2154 # variables defined by shellspec hooks

Describe "Azure helper (live Azure Blob): shallow fetch"
	Include spec/support/live_common.sh
	Include spec/support/live_az.sh
	Include spec/support/git_scenarios.sh

	Skip if "set LIVE_AZ=1 to enable" flag_unset LIVE_AZ

	BeforeAll 'live_az_setup'
	AfterAll 'live_az_teardown'

	Describe "git clone --depth 1 on a 5-commit history"
		# Split setup/It so the clone is the observable action. If setup
		# silently failed to push, both depth and full clone would produce
		# zero commits — the depth constraint would pass vacuously.
		setup() {
			PREFIX=$(live_az_unique_prefix)
			URL=$(live_az_url "$PREFIX")
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
			PREFIX=$(live_az_unique_prefix)
			URL=$(live_az_url "$PREFIX")
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
			PREFIX=$(live_az_unique_prefix)
			URL=$(live_az_url "$PREFIX")
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
		setup() {
			PREFIX=$(live_az_unique_prefix)
			URL=$(live_az_url "$PREFIX")
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
		setup() {
			PREFIX=$(live_az_unique_prefix)
			URL=$(live_az_url "$PREFIX")
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
		setup() {
			PREFIX=$(live_az_unique_prefix)
			URL=$(live_az_url "$PREFIX")
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

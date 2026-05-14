# shellcheck shell=bash
# shellcheck disable=SC2154

Describe "Azure helper (live Azure Blob): concurrent push contention"
	Include spec/support/live_common.sh
	Include spec/support/live_az.sh
	Include spec/support/git_scenarios.sh
	Include spec/support/bucket_assertions.sh

	Skip if "set LIVE_AZ=1 to enable" flag_unset LIVE_AZ

	BeforeAll 'live_az_setup'
	AfterAll 'live_az_teardown'

	Describe "two parallel force-pushes leave a single bundle"
		# Both clones make divergent commits and force-push concurrently.
		# Whichever acquires the lock first writes its bundle; the other
		# either waits for the lock to release (then writes its own,
		# replacing the first) or rejects with a lock-related error.
		# Either way the final container state must contain exactly one
		# bundle for the ref — the lock contract.
		#
		# Compared to the Azurite version this exercises real Azure Blob
		# conditional-write semantics (`If-None-Match: *` via
		# `put_if_absent`); the emulator has historically been forgiving
		# about returning the wrong status code for that header.
		setup_two_clones() {
			CONTAINER="$LIVE_AZ_CONTAINER"
			PREFIX=$(live_az_unique_prefix)
			URL=$(live_az_url "$PREFIX")
			SRC_A="$SHELLSPEC_TMPDIR/srcA-$$-$RANDOM"
			SRC_B="$SHELLSPEC_TMPDIR/srcB-$$-$RANDOM"
			mk_local_repo "$SRC_A"
			commit_in_repo "$SRC_A" hello.txt "base" "base commit" >/dev/null
			add_remote "$SRC_A" origin "$URL"
			push_branch "$SRC_A" origin refs/heads/main:refs/heads/main

			clone_remote "$URL" "$SRC_B"
			git_scenarios_init "$SRC_B"

			echo "from A" >"$SRC_A/hello.txt"
			git -C "$SRC_A" add hello.txt
			GIT_COMMITTER_DATE='2026-01-01T00:00:00Z' \
				GIT_AUTHOR_DATE='2026-01-01T00:00:00Z' \
				git -C "$SRC_A" commit -q -m "from A"
			echo "from B" >"$SRC_B/hello.txt"
			git -C "$SRC_B" add hello.txt
			GIT_COMMITTER_DATE='2026-02-02T00:00:00Z' \
				GIT_AUTHOR_DATE='2026-02-02T00:00:00Z' \
				git -C "$SRC_B" commit -q -m "from B"
		}
		BeforeEach 'setup_two_clones'

		race_pushes() {
			local result_dir
			result_dir=$(mktemp -d -t race.XXXXXX)
			# Coin-flip fork order to match the integration tier; keeps
			# every concurrent-push spec on the same randomization
			# discipline regardless of how strict the backend's
			# conditional-write semantics are.
			if (( RANDOM % 2 == 0 )); then
				race_force_pushes "$result_dir" refs/heads/main \
					A "$SRC_A" B "$SRC_B"
			else
				race_force_pushes "$result_dir" refs/heads/main \
					B "$SRC_B" A "$SRC_A"
			fi

			# At least one push must have succeeded — the race must
			# actually run to completion. Without this check the
			# bundle-count assertion below would pass vacuously if
			# both pushes failed (the container would still hold the
			# base-commit bundle from setup, count == 1).
			local a_exit b_exit
			a_exit=$(cat "$result_dir/A.exit" 2>/dev/null || echo "missing")
			b_exit=$(cat "$result_dir/B.exit" 2>/dev/null || echo "missing")
			if [[ "$a_exit" != "0" && "$b_exit" != "0" ]]; then
				echo "race_pushes: neither push succeeded (A=$a_exit B=$b_exit)" >&2
				echo "--- A.log ---" >&2
				cat "$result_dir/A.log" >&2 2>/dev/null || true
				echo "--- B.log ---" >&2
				cat "$result_dir/B.log" >&2 2>/dev/null || true
				rm -rf "$result_dir"
				return 1
			fi
			rm -rf "$result_dir"
		}

		It "ends with exactly one bundle for refs/heads/main"
			When call race_pushes
			The status should equal 0

			SHA_A=$(git -C "$SRC_A" rev-parse HEAD)
			SHA_B=$(git -C "$SRC_B" rev-parse HEAD)

			if live_engine_is_bundle; then
				# Issue #157: every successful force-push tombstones
				# the prior baseline, so the raw listing contains the
				# base-commit bundle (tombstoned) AND the winner's
				# bundle. Pass the getter so the assertion filters out
				# the tombstoned predecessor and verifies the LOGICAL
				# state (one live bundle for the ref).
				assert_bundle_count live_az_list "$CONTAINER" "$PREFIX" \
					refs/heads/main 1 live_az_get_object
				keys=$(bundle_keys live_az_list "$CONTAINER" "$PREFIX" \
					refs/heads/main live_az_get_object)
				[[ "$keys" == *"/${SHA_A}.bundle"* || "$keys" == *"/${SHA_B}.bundle"* ]]
			else
				# Engine-agnostic black-box check: ls-remote tip must
				# match one of the two divergent SHAs.
				tip=$(git ls-remote "$URL" refs/heads/main | awk '{print $1}')
				[[ "$tip" == "$SHA_A" || "$tip" == "$SHA_B" ]]
			fi
		End
	End
End

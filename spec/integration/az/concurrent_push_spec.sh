# shellcheck shell=bash
# shellcheck disable=SC2154

Describe "Azure helper: concurrent push and stale-lock recovery"
	Include spec/support/images.sh
	Include spec/support/docker_backend.sh
	Include spec/support/azurite.sh
	Include spec/support/git_scenarios.sh
	Include spec/support/bucket_assertions.sh

	Skip if "set INTEGRATION_AZ=1 to enable" flag_unset INTEGRATION_AZ
	Skip if "docker not on PATH" missing_cmd docker
	Skip if "az-cli not on PATH" missing_cmd az
	Skip if "git not on PATH" missing_cmd git

	BeforeAll 'azurite_start'
	AfterAll 'azurite_stop'

	Describe "two parallel force-pushes leave a single bundle"
		# To detect a regression that silently broke contention so that
		# the same side always wins (a disjunctive `A || B` assertion
		# would mask this), `race_one_iteration` runs the race against
		# a fresh container and `race_observes_both_winners` repeats it
		# until both winner SHAs are observed (capped by RACE_MAX_TRIES
		# to bound CI cost; the cap is generous enough that a fair race
		# almost always finishes well under it).

		# Number of iterations to spin while waiting to observe both
		# winners. With a fair (~50/50) race the expected number of
		# iterations to see both is ~3 and the loop exits early; the
		# cap only matters when a regression has fully stuck the race.
		# Cap is generous (vs. the live tier's absence of this check)
		# because azurite's loose conditional-write semantics leave a
		# significant ordering bias even after fork-order randomization.
		# Mirrors the integration s3 tier.
		RACE_MAX_TRIES=60

		race_one_iteration() {
			local container prefix url src_a src_b sha_a sha_b
			container=$(azurite_unique_container)
			prefix="myrepo"
			azurite_make_container "$container"
			url=$(azurite_url "$container" "$prefix")
			src_a="$SHELLSPEC_TMPDIR/srcA-$$-$RANDOM"
			src_b="$SHELLSPEC_TMPDIR/srcB-$$-$RANDOM"
			mk_local_repo "$src_a"
			commit_in_repo "$src_a" hello.txt "base" "base commit" >/dev/null
			add_remote "$src_a" origin "$url"
			push_branch "$src_a" origin refs/heads/main:refs/heads/main

			clone_remote "$url" "$src_b"
			git_scenarios_init "$src_b"

			echo "from A" >"$src_a/hello.txt"
			git -C "$src_a" add hello.txt
			GIT_COMMITTER_DATE='2026-01-01T00:00:00Z' \
				GIT_AUTHOR_DATE='2026-01-01T00:00:00Z' \
				git -C "$src_a" commit -q -m "from A"
			echo "from B" >"$src_b/hello.txt"
			git -C "$src_b" add hello.txt
			GIT_COMMITTER_DATE='2026-02-02T00:00:00Z' \
				GIT_AUTHOR_DATE='2026-02-02T00:00:00Z' \
				git -C "$src_b" commit -q -m "from B"

			sha_a=$(git -C "$src_a" rev-parse HEAD)
			sha_b=$(git -C "$src_b" rev-parse HEAD)

			local result_dir a_exit b_exit
			result_dir=$(mktemp -d -t race.XXXXXX)
			# Randomize which side is started first. Bash's fork
			# ordering plus azurite's loose conditional-write semantics
			# give the second-started side a near-deterministic edge,
			# starving the bias check. A coin flip restores the ~50/50
			# distribution the test design (commit 00fc355) assumed.
			if (( RANDOM % 2 == 0 )); then
				race_force_pushes "$result_dir" refs/heads/main \
					A "$src_a" B "$src_b"
			else
				race_force_pushes "$result_dir" refs/heads/main \
					B "$src_b" A "$src_a"
			fi

			a_exit=$(cat "$result_dir/A.exit" 2>/dev/null || echo "missing")
			b_exit=$(cat "$result_dir/B.exit" 2>/dev/null || echo "missing")
			if [[ "$a_exit" != "0" && "$b_exit" != "0" ]]; then
				echo "race_one_iteration: neither push succeeded (A=$a_exit B=$b_exit)" >&2
				echo "--- A.log ---" >&2
				cat "$result_dir/A.log" >&2 2>/dev/null || true
				echo "--- B.log ---" >&2
				cat "$result_dir/B.log" >&2 2>/dev/null || true
				rm -rf "$result_dir"
				return 1
			fi
			rm -rf "$result_dir"

			# Issue #157: every successful force-push tombstones the
			# prior baseline, so the raw listing contains the
			# base-commit bundle (tombstoned) AND the winner's bundle.
			# Pass `azurite_get_object` to filter the tombstoned
			# predecessor.
			assert_bundle_count azurite_list "$container" "$prefix" \
				refs/heads/main 1 azurite_get_object || return 1
			local keys winner=""
			keys=$(bundle_keys azurite_list "$container" "$prefix" \
				refs/heads/main azurite_get_object)
			if [[ "$keys" == *"/${sha_a}.bundle"* ]]; then
				winner="A"
			elif [[ "$keys" == *"/${sha_b}.bundle"* ]]; then
				winner="B"
			else
				echo "race_one_iteration: surviving bundle matches neither divergent SHA" >&2
				echo "$keys" >&2
				return 1
			fi
			printf '%s\n' "$winner"
		}

		race_observes_both_winners() {
			local saw_a=0 saw_b=0 i winner
			for ((i = 1; i <= RACE_MAX_TRIES; i++)); do
				winner=$(race_one_iteration) || return 1
				case "$winner" in
					A) saw_a=1 ;;
					B) saw_b=1 ;;
				esac
				if ((saw_a == 1 && saw_b == 1)); then
					return 0
				fi
			done
			echo "race_observes_both_winners: after $RACE_MAX_TRIES iterations only one side ever won (A=$saw_a B=$saw_b) — contention may be broken" >&2
			return 1
		}

		It "lets either divergent push win across repeated races"
			# Strengthens the prior `A || B` disjunctive assertion: if a
			# regression made one side always win, that test would still
			# pass; this one requires both winners to be observed.
			When call race_observes_both_winners
			The status should equal 0
		End
	End

	Describe "stale lock is reclaimed after TTL"
		setup_with_stale_lock() {
			CONTAINER=$(azurite_unique_container)
			PREFIX="myrepo"
			azurite_make_container "$CONTAINER"
			URL=$(azurite_url "$CONTAINER" "$PREFIX")
			SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
			mk_local_repo "$SRC"
			SHA1=$(commit_in_repo "$SRC" hello.txt "first" "commit 1")
			add_remote "$SRC" origin "$URL"
			push_branch "$SRC" origin refs/heads/main:refs/heads/main

			LOCK_FILE="$SHELLSPEC_TMPDIR/lockbody.$$"
			: >"$LOCK_FILE"
			azurite_put_object "$CONTAINER" \
				"$PREFIX/refs/heads/main/LOCK#.lock" "$LOCK_FILE"
			sleep 3

			SHA2=$(commit_in_repo "$SRC" hello.txt "second" "commit 2")
		}
		BeforeEach 'setup_with_stale_lock'

		quiet_push() {
			GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS=2 \
				git -C "$SRC" push origin refs/heads/main:refs/heads/main \
				>/dev/null 2>&1
		}

		It "completes the push and replaces the bundle"
			When call quiet_push
			The status should equal 0

			# Issue #157: SHA1's bundle survives the fast-forward push
			# as a tombstoned predecessor — pass the getter to filter.
			assert_bundle_count azurite_list "$CONTAINER" "$PREFIX" \
				refs/heads/main 1 azurite_get_object
			assert_bundle_sha_for_ref azurite_list "$CONTAINER" "$PREFIX" \
				refs/heads/main "$SHA2" azurite_get_object
		End
	End
End

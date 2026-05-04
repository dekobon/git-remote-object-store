# shellcheck shell=bash
# shellcheck disable=SC2154

Describe "S3 helper (live AWS): concurrent push contention"
	Include spec/support/live_common.sh
	Include spec/support/live_s3.sh
	Include spec/support/git_scenarios.sh
	Include spec/support/bucket_assertions.sh

	Skip if "set LIVE_S3=1 to enable" flag_unset LIVE_S3

	BeforeAll 'live_s3_setup'
	AfterAll 'live_s3_teardown'

	Describe "two parallel force-pushes leave a single bundle"
		# Both clones make divergent commits and force-push concurrently.
		# Whichever acquires the lock first writes its bundle; the other
		# either waits for the lock to release (then writes its own,
		# replacing the first) or rejects with a lock-related error.
		# Either way the final bucket state must contain exactly one
		# bundle for the ref — the lock contract.
		#
		# Compared to the rustfs version this exercises real S3
		# conditional-write semantics (`If-None-Match: *` via
		# `put_if_absent`); the emulator has historically been forgiving
		# about returning the wrong status code for that header.
		#
		# To detect a regression that silently broke contention so that
		# the same side always wins (a disjunctive `A || B` assertion
		# would mask this), `race_one_iteration` runs the race against
		# a fresh prefix and `race_observes_both_winners` repeats it
		# until both winner SHAs are observed (capped by RACE_MAX_TRIES
		# to bound CI cost; the cap is generous enough that a fair race
		# almost always finishes well under it).

		# Number of iterations to spin while waiting to observe both
		# winners. With a fair (~50/50) race the expected number of
		# iterations to see both is ~3; the cap lets a moderately
		# biased race still pass while flagging a fully-stuck race.
		# The live tier uses a smaller cap because each iteration
		# issues real S3 calls and a stuck race should fail fast.
		RACE_MAX_TRIES=6

		race_one_iteration() {
			local bucket prefix url src_a src_b sha_a sha_b
			bucket="$LIVE_S3_BUCKET"
			prefix=$(live_s3_unique_prefix)
			url=$(live_s3_url "$prefix")
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
			(
				git -C "$src_a" push origin '+refs/heads/main:refs/heads/main' \
					>"$result_dir/A.log" 2>&1
				echo $? >"$result_dir/A.exit"
			) &
			local a_pid=$!
			(
				git -C "$src_b" push origin '+refs/heads/main:refs/heads/main' \
					>"$result_dir/B.log" 2>&1
				echo $? >"$result_dir/B.exit"
			) &
			local b_pid=$!
			wait "$a_pid" "$b_pid"

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

			# Identify the winner: bundle path under bundle-engine,
			# `ls-remote` tip otherwise (engine-agnostic).
			local winner=""
			if live_engine_is_bundle; then
				assert_bundle_count live_s3_list "$bucket" "$prefix" \
					refs/heads/main 1 || return 1
				local keys
				keys=$(live_s3_list "$bucket" "$prefix")
				if [[ "$keys" == *"/${sha_a}.bundle"* ]]; then
					winner="A"
				elif [[ "$keys" == *"/${sha_b}.bundle"* ]]; then
					winner="B"
				fi
			else
				local tip
				tip=$(git ls-remote "$url" refs/heads/main | awk '{print $1}')
				if [[ "$tip" == "$sha_a" ]]; then
					winner="A"
				elif [[ "$tip" == "$sha_b" ]]; then
					winner="B"
				fi
			fi
			if [[ -z "$winner" ]]; then
				echo "race_one_iteration: surviving state matches neither divergent SHA" >&2
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
End

# shellcheck shell=bash
# shellcheck disable=SC2154 # variables defined by shellspec hooks

Describe "Azure helper (live Azure Blob): management CLI"
	Include spec/support/live_common.sh
	Include spec/support/live_az.sh
	Include spec/support/git_scenarios.sh
	Include spec/support/bucket_assertions.sh

	Skip if "set LIVE_AZ=1 to enable" flag_unset LIVE_AZ
	Skip if "script(1) not on PATH" missing_cmd script

	BeforeAll 'live_az_setup'
	AfterAll 'live_az_teardown'

	setup_prefix_with_main() {
		CONTAINER="$LIVE_AZ_CONTAINER"
		PREFIX=$(live_az_unique_prefix)
		URL=$(live_az_url "$PREFIX")
		SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
		mk_local_repo "$SRC"
		SHA_MAIN=$(commit_in_repo "$SRC" hello.txt "hi" "first commit")
		add_remote "$SRC" origin "$URL"
		push_branch "$SRC" origin refs/heads/main:refs/heads/main
	}

	Describe "protect then unprotect"
		BeforeEach 'setup_prefix_with_main'

		quiet_protect() {
			git-remote-object-store protect "$URL" main >/dev/null
		}
		quiet_unprotect() {
			git-remote-object-store unprotect "$URL" main >/dev/null
		}

		It "writes the PROTECTED# marker on protect and removes it on unprotect"
			When call quiet_protect
			The status should equal 0
			assert_protected_marker live_az_list "$CONTAINER" "$PREFIX" \
				refs/heads/main

			quiet_unprotect
			assert_no_protected_marker live_az_list "$CONTAINER" "$PREFIX" \
				refs/heads/main
		End
	End

	Describe "delete-branch"
		setup_with_feature() {
			setup_prefix_with_main
			git -C "$SRC" checkout -q -b feature
			SHA_FEATURE=$(commit_in_repo "$SRC" feature.txt "ff" "feature")
			push_branch "$SRC" origin refs/heads/feature:refs/heads/feature
		}
		BeforeEach 'setup_with_feature'

		# `git-remote-object-store delete-branch` calls
		# `dialoguer::Confirm::interact()`, which requires a TTY and
		# fails with `fatal: not a terminal` when fed via a pipe. Use
		# `script -qec` to allocate a pty so the prompt machinery is
		# satisfied; feed `y\n` through stdin to confirm.
		delete_feature_via_pty() {
			script -qec "git-remote-object-store delete-branch \"$URL\" feature" /dev/null <<<'y' >/dev/null
		}

		It "removes every bundle under the branch after a y/N prompt"
			# Pre-condition: feature is on the remote. `assert_bundle_count`
			# is bundle-format-specific; the engine-agnostic
			# `assert_ls_remote_ref_present` keeps the precondition
			# meaningful under packchain (where bundle-count skips and a
			# silent setup failure would otherwise let the post-condition
			# `refs_listed == ""` pass vacuously).
			assert_ls_remote_ref_present "$URL" refs/heads/feature
			if live_engine_is_bundle; then
				assert_bundle_count live_az_list "$CONTAINER" "$PREFIX" \
					refs/heads/feature 1
			fi

			When call delete_feature_via_pty
			The status should equal 0

			if live_engine_is_bundle; then
				assert_bundle_count live_az_list "$CONTAINER" "$PREFIX" \
					refs/heads/feature 0
			fi
			refs_listed=$(git ls-remote "$URL" refs/heads/feature 2>/dev/null || true)
			The variable refs_listed should equal ""
		End
	End

	Describe "doctor --delete-stale-locks"
		setup_with_stale_lock() {
			setup_prefix_with_main
			LOCK_FILE="$SHELLSPEC_TMPDIR/lockbody.$$"
			: >"$LOCK_FILE"
			live_az_put_object "$CONTAINER" \
				"$PREFIX/refs/heads/main/LOCK#.lock" "$LOCK_FILE"
		}
		BeforeEach 'setup_with_stale_lock'

		quiet_doctor() {
			git-remote-object-store doctor "$URL" \
				--lock-ttl-seconds 0 --delete-stale-locks >/dev/null
		}

		It "removes the stale lock from the ref directory"
			assert_lock_present live_az_list "$CONTAINER" "$PREFIX" \
				refs/heads/main

			When call quiet_doctor
			The status should equal 0

			assert_lock_absent live_az_list "$CONTAINER" "$PREFIX" \
				refs/heads/main
		End
	End
End

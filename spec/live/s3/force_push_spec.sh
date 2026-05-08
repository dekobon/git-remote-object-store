# shellcheck shell=bash
# shellcheck disable=SC2154

Describe "S3 helper (live AWS): force-push and protected refs"
	Include spec/support/live_common.sh
	Include spec/support/live_s3.sh
	Include spec/support/git_scenarios.sh
	Include spec/support/bucket_assertions.sh

	Skip if "set LIVE_S3=1 to enable" flag_unset LIVE_S3

	BeforeAll 'live_s3_setup'
	AfterAll 'live_s3_teardown'

	# Setup: push commit A to the remote, then rewrite the local main to
	# a divergent commit B (different SHA, no ancestry to A). A second
	# `+refs/heads/main:refs/heads/main` push is the force-push under
	# test.
	setup_divergent() {
		BUCKET="$LIVE_S3_BUCKET"
		PREFIX=$(live_s3_unique_prefix)
		URL=$(live_s3_url "$PREFIX")
		SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
		mk_local_repo "$SRC"
		SHA_A=$(commit_in_repo "$SRC" hello.txt "first" "commit A")
		add_remote "$SRC" origin "$URL"
		push_branch "$SRC" origin refs/heads/main:refs/heads/main
		# Amend the root commit with different content + timestamp to
		# get a different SHA. `--amend` on a root commit produces a
		# fresh root commit that is not an ancestor of A.
		echo "second" >"$SRC/hello.txt"
		git -C "$SRC" add hello.txt
		GIT_COMMITTER_DATE='2026-01-01T00:00:00Z' \
		GIT_AUTHOR_DATE='2026-01-01T00:00:00Z' \
			git -C "$SRC" commit --amend --quiet -m "commit B"
		SHA_B=$(git -C "$SRC" rev-parse HEAD)
		[[ "$SHA_A" != "$SHA_B" ]]
	}

	Describe "force-push allowed when ref is not protected"
		BeforeEach 'setup_divergent'

		It "replaces the bundle and exits 0"
			# Pre-condition: SHA_A is the tip of main. Without this
			# check, a setup that silently dropped the first push would
			# make the post-condition (SHA_B at main) indistinguishable
			# from a fresh push of SHA_B. `assert_ls_remote_sha` is
			# engine-agnostic; `assert_bundle_sha_for_ref` adds
			# bundle-format-specific detail.
			assert_ls_remote_sha "$URL" refs/heads/main "$SHA_A"
			if live_engine_is_bundle; then
				assert_bundle_sha_for_ref live_s3_list "$BUCKET" "$PREFIX" \
					refs/heads/main "$SHA_A"
			fi

			When call push_branch "$SRC" origin "+refs/heads/main:refs/heads/main"
			The status should equal 0

			assert_ls_remote_sha "$URL" refs/heads/main "$SHA_B"
			if live_engine_is_bundle; then
				assert_bundle_sha_for_ref live_s3_list "$BUCKET" "$PREFIX" \
					refs/heads/main "$SHA_B"
			fi
		End
	End

	Describe "force-push silently degraded when PROTECTED# present"
		# The helper strips the leading `+` from the refspec when a
		# PROTECTED# marker exists for the target ref (see
		# src/protocol/push.rs — `force_push` is forced to false). A
		# divergent commit then fails the ancestor check; the helper
		# emits `error <ref> "remote ref is not ancestor of <local>."`
		# (NOT_ANCESTOR_TOKEN in src/protocol/push.rs). Exit code is
		# non-zero; the bundle on the remote is unchanged.
		BeforeEach 'setup_divergent'
		BeforeEach 'git-remote-object-store protect "$URL" main >/dev/null 2>&1'

		quiet_push_force() {
			push_branch "$SRC" origin "+refs/heads/main:refs/heads/main" >/dev/null 2>&1
		}

		It "rejects the push and leaves the bundle SHA unchanged"
			When call quiet_push_force
			The status should not equal 0
			# "not ancestor" is the documented contract token
			# (src/protocol/push.rs:NOT_ANCESTOR_TOKEN). Asserted here
			# to distinguish the ancestor-mismatch failure mode from
			# unrelated push failures (network, permission, ...).
			The variable LAST_GIT_OUTPUT should include "not ancestor"

			# Engine-agnostic post-condition: refs/heads/main still
			# resolves to SHA_A. Catches a regression where the rejected
			# push corrupts on-bucket state under packchain, where the
			# bundle-format check below would skip.
			assert_ls_remote_sha "$URL" refs/heads/main "$SHA_A"
			if live_engine_is_bundle; then
				assert_bundle_count live_s3_list "$BUCKET" "$PREFIX" \
					refs/heads/main 1
				assert_bundle_sha_for_ref live_s3_list "$BUCKET" "$PREFIX" \
					refs/heads/main "$SHA_A"
			fi
		End
	End
End

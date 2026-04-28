# shellcheck shell=bash
# shellcheck disable=SC2154

Describe "S3 helper: force-push and protected refs"
	Include spec/support/images.sh
	Include spec/support/docker_backend.sh
	Include spec/support/rustfs.sh
	Include spec/support/git_scenarios.sh
	Include spec/support/bucket_assertions.sh

	Skip if "set INTEGRATION_S3=1 to enable" flag_unset INTEGRATION_S3
	Skip if "docker not on PATH" missing_cmd docker
	Skip if "aws-cli not on PATH" missing_cmd aws
	Skip if "git not on PATH" missing_cmd git

	BeforeAll 'rustfs_start'
	AfterAll 'rustfs_stop'

	# Setup: push commit A to the remote, then rewrite the local main to
	# a divergent commit B (different SHA, no ancestry to A). A second
	# `+refs/heads/main:refs/heads/main` push is the force-push under
	# test.
	setup_divergent() {
		BUCKET=$(rustfs_unique_bucket)
		PREFIX="myrepo"
		rustfs_make_bucket "$BUCKET"
		URL=$(rustfs_url "$BUCKET" "$PREFIX")
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
			# Pre-condition: SHA_A is the bundle under main. Without
			# this check, a setup that silently dropped the first push
			# would make the post-condition (one bundle of SHA_B)
			# indistinguishable from a fresh push of SHA_B.
			assert_bundle_sha_for_ref rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main "$SHA_A"

			When call push_branch "$SRC" origin "+refs/heads/main:refs/heads/main"
			The status should equal 0

			assert_bundle_sha_for_ref rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main "$SHA_B"
		End
	End

	Describe "force-push silently degraded when PROTECTED# present"
		# The helper strips the leading `+` from the refspec when a
		# PROTECTED# marker exists for the target ref (see
		# src/protocol/push.rs:467 — `force_push` is forced to false).
		# A divergent commit then fails the ancestor check with
		# `"remote ref is not ancestor of <local>."`. Exit code is
		# non-zero; the bundle on the remote is unchanged.
		BeforeEach 'setup_divergent'
		BeforeEach 'git-remote-object-store protect "$URL" main >/dev/null 2>&1'

		quiet_push_force() {
			push_branch "$SRC" origin "+refs/heads/main:refs/heads/main" >/dev/null 2>&1
		}

		It "rejects the push and leaves the bundle SHA unchanged"
			When call quiet_push_force
			The status should not equal 0
			The variable LAST_GIT_OUTPUT should include "not ancestor"

			assert_bundle_count rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main 1
			assert_bundle_sha_for_ref rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main "$SHA_A"
		End
	End
End

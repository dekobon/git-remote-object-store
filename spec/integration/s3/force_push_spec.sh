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

			assert_bundle_count rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main 1
			assert_bundle_sha_for_ref rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main "$SHA_A"
		End
	End

	# Issue #141: cross-backend coverage for the delete-path protection
	# guard (#128) and the under-lock delete serialization (#133). The
	# MockStore unit tests in src/protocol/push.rs pin the byte-exact wire
	# output; these tests confirm the same guards fire against a real S3
	# backend's listing semantics, where read-after-write and listing
	# ordering quirks are observable only here.
	setup_with_feature() {
		BUCKET=$(rustfs_unique_bucket)
		PREFIX="myrepo"
		rustfs_make_bucket "$BUCKET"
		URL=$(rustfs_url "$BUCKET" "$PREFIX")
		SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
		mk_local_repo "$SRC"
		commit_in_repo "$SRC" hello.txt "hi" "first" >/dev/null
		add_remote "$SRC" origin "$URL"
		push_branch "$SRC" origin refs/heads/main:refs/heads/main
		git -C "$SRC" checkout -q -b feature
		SHA_FEATURE=$(commit_in_repo "$SRC" feature.txt "ff" "feature")
		push_branch "$SRC" origin refs/heads/feature:refs/heads/feature
	}

	Describe "bundle delete refused when PROTECTED# present (#128)"
		BeforeEach 'setup_with_feature'
		BeforeEach 'git-remote-object-store protect "$URL" feature >/dev/null 2>&1'

		quiet_delete() {
			push_branch "$SRC" origin ":refs/heads/feature" >/dev/null 2>&1
		}

		It "rejects the delete and leaves the bundle untouched"
			# Pre-condition: bundle is present (the precondition fails
			# loudly if the setup push silently produced no bundle, so a
			# vacuous "bundle still present" post-condition can't pass).
			assert_bundle_sha_for_ref rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature "$SHA_FEATURE"
			assert_protected_marker rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature

			When call quiet_delete
			The status should not equal 0
			# "protected" is the contract substring from
			# DELETE_PROTECTION_MESSAGE in src/protocol/push.rs. Asserted
			# to distinguish this rejection from unrelated push failures.
			The variable LAST_GIT_OUTPUT should include "protected"

			assert_bundle_sha_for_ref rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature "$SHA_FEATURE"
			assert_protected_marker rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature
			# Mirrors the unit test `delete_rejects_when_protected_marker_present_with_chain`
			# in src/packchain/push.rs and `release_lock` semantics in
			# src/protocol/push.rs:push_one: the lock is released on the
			# refusal arm, so no `LOCK#.lock` should survive. A regression
			# that returned from the protection branch without releasing
			# would leave a stray lock here.
			assert_lock_absent rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature
		End
	End

	Describe "bundle delete refused when LOCK#.lock held (#133)"
		# Pre-seed a fresh LOCK#.lock directly via aws-cli (same pattern
		# as doctor --delete-stale-locks). With the post-#133 fix the
		# bundle delete must acquire this lock before sweeping; a held
		# fresh lock therefore yields a "failed to acquire ref lock"
		# refusal and the bundle survives. A regression that swept under
		# a stale pre-lock listing would delete the bundle here.
		BeforeEach 'setup_with_feature'
		setup_held_lock() {
			LOCK_FILE="$SHELLSPEC_TMPDIR/lockbody.$$"
			: >"$LOCK_FILE"
			rustfs_put_object "$BUCKET" \
				"$PREFIX/refs/heads/feature/LOCK#.lock" "$LOCK_FILE"
		}
		BeforeEach 'setup_held_lock'

		quiet_delete() {
			push_branch "$SRC" origin ":refs/heads/feature" >/dev/null 2>&1
		}

		It "rejects the delete and leaves the bundle and lock in place"
			assert_bundle_sha_for_ref rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature "$SHA_FEATURE"
			assert_lock_present rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature

			When call quiet_delete
			The status should not equal 0
			# Contract token from acquire_lock contention path
			# (tests/protocol_push.rs:delete_with_held_lock_returns_contention_error).
			The variable LAST_GIT_OUTPUT should include "failed to acquire ref lock"

			assert_bundle_sha_for_ref rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature "$SHA_FEATURE"
			assert_lock_present rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature
		End
	End
End

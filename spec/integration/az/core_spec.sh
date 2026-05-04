# shellcheck shell=bash
# shellcheck disable=SC2154 # variables defined by shellspec hooks

Describe "Azure helper: core git operations against Azurite"
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

	Describe "push first commit to empty remote"
		setup() {
			CONTAINER=$(azurite_unique_container)
			PREFIX="myrepo"
			azurite_make_container "$CONTAINER"
			WORK="$SHELLSPEC_TMPDIR/repo-$$-$RANDOM"
			mk_local_repo "$WORK"
			SHA=$(commit_in_repo "$WORK" hello.txt "hi" "first commit")
			add_remote "$WORK" origin "$(azurite_url "$CONTAINER" "$PREFIX")"
		}
		BeforeEach 'setup'

		It "creates exactly one bundle and a HEAD pointer matching the ref"
			When call push_branch "$WORK" origin refs/heads/main:refs/heads/main
			The status should equal 0

			assert_bundle_count azurite_list "$CONTAINER" "$PREFIX" \
				refs/heads/main 1
			assert_bundle_sha_for_ref azurite_list "$CONTAINER" "$PREFIX" \
				refs/heads/main "$SHA"
			assert_head_pointer azurite_get_object "$CONTAINER" "$PREFIX" \
				refs/heads/main
		End
	End

	Describe "clone an existing remote"
		setup() {
			CONTAINER=$(azurite_unique_container)
			PREFIX="myrepo"
			azurite_make_container "$CONTAINER"
			URL=$(azurite_url "$CONTAINER" "$PREFIX")
			SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
			DST="$SHELLSPEC_TMPDIR/dst-$$-$RANDOM"
			mk_local_repo "$SRC"
			SHA=$(commit_in_repo "$SRC" hello.txt "hi" "first commit")
			add_remote "$SRC" origin "$URL"
			push_branch "$SRC" origin refs/heads/main:refs/heads/main
		}
		BeforeEach 'setup'

		It "fetches the bundle and reproduces HEAD"
			When call clone_remote "$URL" "$DST"
			The status should equal 0
			assert_git_sha_equals "$DST" HEAD "$SHA"
			assert_git_sha_equals "$DST" refs/remotes/origin/main "$SHA"
		End
	End

	Describe "fast-forward push then fetch"
		# Setup pushes SHA1 and clones to DST; the It performs the SHA2
		# push itself so the A→B transition is observable. Without this
		# split, a setup that silently produced no bundle would let the
		# It body pass vacuously (one bundle of SHA2 is indistinguishable
		# from a fresh push of SHA2).
		setup() {
			CONTAINER=$(azurite_unique_container)
			PREFIX="myrepo"
			azurite_make_container "$CONTAINER"
			URL=$(azurite_url "$CONTAINER" "$PREFIX")
			SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
			DST="$SHELLSPEC_TMPDIR/dst-$$-$RANDOM"
			mk_local_repo "$SRC"
			SHA1=$(commit_in_repo "$SRC" hello.txt "hi" "first commit")
			add_remote "$SRC" origin "$URL"
			push_branch "$SRC" origin refs/heads/main:refs/heads/main
			clone_remote "$URL" "$DST"
			SHA2=$(commit_in_repo "$SRC" hello.txt "hi again" "second commit")
			[[ "$SHA1" != "$SHA2" ]]
		}
		BeforeEach 'setup'

		It "replaces the bundle and lets a peer fetch the new tip"
			# Pre-condition: container contains SHA1's bundle.
			assert_bundle_sha_for_ref azurite_list "$CONTAINER" "$PREFIX" \
				refs/heads/main "$SHA1"

			When call push_branch "$SRC" origin refs/heads/main:refs/heads/main
			The status should equal 0

			# Post-condition: SHA1 replaced by SHA2.
			assert_bundle_sha_for_ref azurite_list "$CONTAINER" "$PREFIX" \
				refs/heads/main "$SHA2"

			fetch_remote "$DST" origin
			assert_git_sha_equals "$DST" refs/remotes/origin/main "$SHA2"
		End
	End

	Describe "second branch and tag"
		setup() {
			CONTAINER=$(azurite_unique_container)
			PREFIX="myrepo"
			azurite_make_container "$CONTAINER"
			URL=$(azurite_url "$CONTAINER" "$PREFIX")
			SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
			mk_local_repo "$SRC"
			commit_in_repo "$SRC" hello.txt "hi" "first" >/dev/null
			add_remote "$SRC" origin "$URL"
			push_branch "$SRC" origin refs/heads/main:refs/heads/main

			git -C "$SRC" checkout -q -b feature
			SHA_FEATURE=$(commit_in_repo "$SRC" feature.txt "ff" "feature commit")
			push_branch "$SRC" origin refs/heads/feature:refs/heads/feature
			tag_in_repo "$SRC" v1 -m "release v1"
			SHA_TAG=$(git -C "$SRC" rev-parse v1)
			push_branch "$SRC" origin refs/tags/v1:refs/tags/v1
		}
		BeforeEach 'setup'

		verify_branch_and_tag() {
			assert_bundle_count azurite_list "$CONTAINER" "$PREFIX" \
				refs/heads/feature 1 \
				&& assert_bundle_sha_for_ref azurite_list "$CONTAINER" "$PREFIX" \
				refs/heads/feature "$SHA_FEATURE" \
				&& assert_bundle_count azurite_list "$CONTAINER" "$PREFIX" \
				refs/tags/v1 1 \
				&& assert_bundle_sha_for_ref azurite_list "$CONTAINER" "$PREFIX" \
				refs/tags/v1 "$SHA_TAG"
		}

		It "lands a bundle for each new ref"
			When call verify_branch_and_tag
			The status should equal 0
		End
	End

	Describe "delete branch via empty-source push"
		setup() {
			CONTAINER=$(azurite_unique_container)
			PREFIX="myrepo"
			azurite_make_container "$CONTAINER"
			URL=$(azurite_url "$CONTAINER" "$PREFIX")
			SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
			mk_local_repo "$SRC"
			commit_in_repo "$SRC" hello.txt "hi" "first" >/dev/null
			add_remote "$SRC" origin "$URL"
			push_branch "$SRC" origin refs/heads/main:refs/heads/main
			git -C "$SRC" checkout -q -b feature
			commit_in_repo "$SRC" feature.txt "ff" "feature" >/dev/null
			push_branch "$SRC" origin refs/heads/feature:refs/heads/feature
		}
		BeforeEach 'setup'

		It "removes every bundle under the deleted ref"
			# Pre-condition: feature actually exists on the remote.
			# Without this check, a setup that silently produced no
			# bundle would let the post-condition (count == 0) pass
			# vacuously since the helper's delete path returns Ok on a
			# missing ref (src/protocol/push.rs delete_remote_ref).
			assert_bundle_count azurite_list "$CONTAINER" "$PREFIX" \
				refs/heads/feature 1

			When call push_branch "$SRC" origin :refs/heads/feature
			The status should equal 0

			assert_bundle_count azurite_list "$CONTAINER" "$PREFIX" \
				refs/heads/feature 0

			# Distinguish "ref absent" (ls-remote exit==0, empty output)
			# from "ls-remote failed" (the latter would also produce empty
			# stdout under `2>/dev/null || true`). See
			# `assert_ls_remote_ref_absent` in spec/support/git_scenarios.sh.
			assert_ls_remote_ref_absent "$URL" refs/heads/feature
		End
	End
End

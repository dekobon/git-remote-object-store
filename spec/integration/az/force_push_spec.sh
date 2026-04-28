# shellcheck shell=bash
# shellcheck disable=SC2154

Describe "Azure helper: force-push and protected refs"
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

	setup_divergent() {
		CONTAINER=$(azurite_unique_container)
		PREFIX="myrepo"
		azurite_make_container "$CONTAINER"
		URL=$(azurite_url "$CONTAINER" "$PREFIX")
		SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
		mk_local_repo "$SRC"
		SHA_A=$(commit_in_repo "$SRC" hello.txt "first" "commit A")
		add_remote "$SRC" origin "$URL"
		push_branch "$SRC" origin refs/heads/main:refs/heads/main
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
			assert_bundle_sha_for_ref azurite_list "$CONTAINER" "$PREFIX" \
				refs/heads/main "$SHA_A"

			When call push_branch "$SRC" origin "+refs/heads/main:refs/heads/main"
			The status should equal 0

			assert_bundle_sha_for_ref azurite_list "$CONTAINER" "$PREFIX" \
				refs/heads/main "$SHA_B"
		End
	End

	Describe "force-push silently degraded when PROTECTED# present"
		BeforeEach 'setup_divergent'
		BeforeEach 'git-remote-object-store protect "$URL" main >/dev/null 2>&1'

		quiet_push_force() {
			push_branch "$SRC" origin "+refs/heads/main:refs/heads/main" >/dev/null 2>&1
		}

		It "rejects the push and leaves the bundle SHA unchanged"
			When call quiet_push_force
			The status should not equal 0
			The variable LAST_GIT_OUTPUT should include "not ancestor"

			assert_bundle_count azurite_list "$CONTAINER" "$PREFIX" \
				refs/heads/main 1
			assert_bundle_sha_for_ref azurite_list "$CONTAINER" "$PREFIX" \
				refs/heads/main "$SHA_A"
		End
	End
End

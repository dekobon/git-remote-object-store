# shellcheck shell=bash
# shellcheck disable=SC2154 # variables defined by shellspec hooks

# Issue #141: cross-backend coverage for the packchain engine's
# delete-with-PROTECTED# guard (#130) and the under-lock force-push
# protection (#129). Mirrors the bundle-engine coverage in
# `force_push_spec.sh`. The unit tests in `src/packchain/push.rs` pin
# the byte-exact wire output against MockStore; these tests run the
# packchain engine end-to-end via `?engine=packchain` against rustfs so
# any real S3 listing-semantics regression surfaces here.
Describe "S3 helper (packchain): protected-ref guards against rustfs"
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

	# packchain_url <bucket> <prefix>
	# Wrap `rustfs_url` with the `?engine=packchain` query-string. Kept
	# inline here (not in spec/support/rustfs.sh) because no other spec
	# in this directory needs it; lift it out the second a second caller
	# appears.
	packchain_url() {
		printf '%s?engine=packchain' "$(rustfs_url "$1" "$2")"
	}

	# Push main and feature under `?engine=packchain`; the bucket's
	# FORMAT marker is then locked to "packchain" so subsequent CLI calls
	# (protect, the delete-push under test) route through the packchain
	# engine.
	setup_packchain_feature() {
		BUCKET=$(rustfs_unique_bucket)
		PREFIX="myrepo"
		rustfs_make_bucket "$BUCKET"
		URL=$(packchain_url "$BUCKET" "$PREFIX")
		SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
		mk_local_repo "$SRC"
		commit_in_repo "$SRC" hello.txt "hi" "first" >/dev/null
		add_remote "$SRC" origin "$URL"
		push_branch "$SRC" origin refs/heads/main:refs/heads/main
		git -C "$SRC" checkout -q -b feature
		commit_in_repo "$SRC" feature.txt "ff" "feature" >/dev/null
		push_branch "$SRC" origin refs/heads/feature:refs/heads/feature
	}

	Describe "packchain delete refused when PROTECTED# present (#130)"
		BeforeEach 'setup_packchain_feature'
		BeforeEach 'git-remote-object-store protect "$URL" feature >/dev/null 2>&1'

		quiet_delete() {
			push_branch "$SRC" origin ":refs/heads/feature" >/dev/null 2>&1
		}

		It "rejects the delete and leaves the chain manifest in place"
			# Pre-conditions: feature actually exists as a packchain ref
			# AND is marked PROTECTED#. Without these, a setup that
			# silently produced no chain would let the post-conditions
			# pass vacuously.
			assert_chain_present rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature
			assert_path_index_present rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature
			assert_protected_marker rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature

			When call quiet_delete
			The status should not equal 0
			# "protected" is the contract substring from
			# DELETE_PROTECTION_MESSAGE in src/protocol/push.rs (shared
			# by both engines per the byte-eq assert in
			# src/packchain/push.rs).
			The variable LAST_GIT_OUTPUT should include "protected"

			# Both manifests must survive: a partial-sweep regression
			# (e.g. chain.json kept but path-index.json swept) would slip
			# past a chain-only check.
			assert_chain_present rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature
			assert_path_index_present rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature
			assert_protected_marker rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature
			# Lock release on the refusal arm — see the unit-test
			# parallel `delete_rejects_when_protected_marker_present_with_chain`
			# in src/packchain/push.rs.
			assert_lock_absent rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature
		End
	End

	Describe "packchain force-push refused when PROTECTED# present (#129)"
		# Setup: push commit A to main on packchain, then rewrite main
		# to a divergent commit B. A second `+refs/heads/main:refs/heads/main`
		# push is the force-push under test, with main protected. The
		# helper demotes `force` to false under the marker; the
		# divergent commit then fails the ancestor check and the chain
		# tip is unchanged on the bucket.
		setup_divergent_packchain() {
			BUCKET=$(rustfs_unique_bucket)
			PREFIX="myrepo"
			rustfs_make_bucket "$BUCKET"
			URL=$(packchain_url "$BUCKET" "$PREFIX")
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
		BeforeEach 'setup_divergent_packchain'
		BeforeEach 'git-remote-object-store protect "$URL" main >/dev/null 2>&1'

		quiet_push_force() {
			push_branch "$SRC" origin "+refs/heads/main:refs/heads/main" >/dev/null 2>&1
		}

		It "rejects the push and leaves the chain tip pointing at SHA_A"
			# Pre-condition: both packchain manifests exist for main and
			# `ls-remote` reports SHA_A as the tip.
			assert_chain_present rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main
			assert_path_index_present rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main
			assert_ls_remote_sha "$URL" refs/heads/main "$SHA_A"

			When call quiet_push_force
			The status should not equal 0
			# Contract token from src/protocol/push.rs:NOT_ANCESTOR_TOKEN,
			# re-rendered under-lock by the packchain engine per #129.
			The variable LAST_GIT_OUTPUT should include "not ancestor"

			# Post-condition: chain tip is still SHA_A and both manifests
			# survive. A regression that selectively swept one manifest on
			# the refusal arm would slip past a chain-only check.
			assert_ls_remote_sha "$URL" refs/heads/main "$SHA_A"
			assert_chain_present rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main
			assert_path_index_present rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main
			assert_protected_marker rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main
			# Lock release on the under-lock refusal arm — a regression
			# that bailed out of the protected branch without releasing
			# would leave a stray lock here.
			assert_lock_absent rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main
		End
	End
End

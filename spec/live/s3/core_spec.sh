# shellcheck shell=bash
# shellcheck disable=SC2154 # variables defined by shellspec hooks

Describe "S3 helper (live AWS): core git operations"
	Include spec/support/live_common.sh
	Include spec/support/live_s3.sh
	Include spec/support/git_scenarios.sh
	Include spec/support/bucket_assertions.sh

	Skip if "set LIVE_S3=1 to enable" flag_unset LIVE_S3

	BeforeAll 'live_s3_setup'
	AfterAll 'live_s3_teardown'

	Describe "push first commit to empty remote"
		setup() {
			BUCKET="$LIVE_S3_BUCKET"
			PREFIX=$(live_s3_unique_prefix)
			WORK="$SHELLSPEC_TMPDIR/repo-$$-$RANDOM"
			mk_local_repo "$WORK"
			SHA=$(commit_in_repo "$WORK" hello.txt "hi" "first commit")
			add_remote "$WORK" origin "$(live_s3_url "$PREFIX")"
		}
		BeforeEach 'setup'

		It "creates exactly one bundle and a HEAD pointer matching the ref"
			When call push_branch "$WORK" origin refs/heads/main:refs/heads/main
			The status should equal 0

			if live_engine_is_bundle; then
				assert_bundle_count live_s3_list "$BUCKET" "$PREFIX" \
					refs/heads/main 1
				assert_bundle_sha_for_ref live_s3_list "$BUCKET" "$PREFIX" \
					refs/heads/main "$SHA"
				assert_head_pointer live_s3_get_object "$BUCKET" "$PREFIX" \
					refs/heads/main
			fi
		End
	End

	Describe "clone an existing remote"
		setup() {
			BUCKET="$LIVE_S3_BUCKET"
			PREFIX=$(live_s3_unique_prefix)
			URL=$(live_s3_url "$PREFIX")
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
			BUCKET="$LIVE_S3_BUCKET"
			PREFIX=$(live_s3_unique_prefix)
			URL=$(live_s3_url "$PREFIX")
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
			if live_engine_is_bundle; then
				assert_bundle_sha_for_ref live_s3_list "$BUCKET" "$PREFIX" \
					refs/heads/main "$SHA1"
			fi

			When call push_branch "$SRC" origin refs/heads/main:refs/heads/main
			The status should equal 0

			if live_engine_is_bundle; then
				assert_bundle_sha_for_ref live_s3_list "$BUCKET" "$PREFIX" \
					refs/heads/main "$SHA2"
			fi

			fetch_remote "$DST" origin
			assert_git_sha_equals "$DST" refs/remotes/origin/main "$SHA2"
		End
	End

	Describe "second branch and tag"
		setup() {
			BUCKET="$LIVE_S3_BUCKET"
			PREFIX=$(live_s3_unique_prefix)
			URL=$(live_s3_url "$PREFIX")
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
			if ! live_engine_is_bundle; then
				# Black-box: the tip SHA must be reachable via ls-remote.
				git ls-remote "$URL" refs/heads/feature \
					| grep -Fq "$SHA_FEATURE" \
					&& git ls-remote "$URL" refs/tags/v1 \
					| grep -Fq "$SHA_TAG"
				return $?
			fi
			assert_bundle_count live_s3_list "$BUCKET" "$PREFIX" \
				refs/heads/feature 1 \
				&& assert_bundle_sha_for_ref live_s3_list "$BUCKET" "$PREFIX" \
				refs/heads/feature "$SHA_FEATURE" \
				&& assert_bundle_count live_s3_list "$BUCKET" "$PREFIX" \
				refs/tags/v1 1 \
				&& assert_bundle_sha_for_ref live_s3_list "$BUCKET" "$PREFIX" \
				refs/tags/v1 "$SHA_TAG"
		}

		# Fetch round-trip — engine-agnostic. The on-bucket pack must
		# contain the tag object so `git fetch` can update
		# `refs/tags/v1` to the tag-OID. This pins the regression in
		# issue #79: packchain previously crashed at push, and bundle
		# silently emitted a pack without the tag.
		verify_tag_round_trips_via_fetch() {
			local DST="$SHELLSPEC_TMPDIR/dst-$$-$RANDOM"
			git clone -q "$URL" "$DST" || return 1
			local kind
			kind=$(git -C "$DST" cat-file -t v1 2>/dev/null) || return 1
			[[ "$kind" == "tag" ]] || {
				echo "expected tag, got $kind" >&2
				return 1
			}
			git -C "$DST" cat-file -p v1 | grep -q "release v1" || {
				echo "annotation message did not round-trip" >&2
				return 1
			}
		}

		It "lands a bundle for each new ref"
			When call verify_branch_and_tag
			The status should equal 0
		End

		It "round-trips the annotated tag through fetch"
			When call verify_tag_round_trips_via_fetch
			The status should equal 0
		End
	End

	Describe "delete branch via empty-source push"
		setup() {
			BUCKET="$LIVE_S3_BUCKET"
			PREFIX=$(live_s3_unique_prefix)
			URL=$(live_s3_url "$PREFIX")
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
			# Pre-condition: the feature ref actually exists on the
			# remote. Without this check, a setup that silently produced
			# no on-bucket state would let the post-condition pass
			# vacuously since the helper's delete path returns Ok on a
			# missing ref (src/protocol/push.rs delete_remote_ref).
			# `assert_ls_remote_ref_present` is engine-agnostic;
			# `assert_bundle_count` adds bundle-format-specific detail.
			assert_ls_remote_ref_present "$URL" refs/heads/feature
			if live_engine_is_bundle; then
				assert_bundle_count live_s3_list "$BUCKET" "$PREFIX" \
					refs/heads/feature 1
			fi

			When call push_branch "$SRC" origin :refs/heads/feature
			The status should equal 0

			if live_engine_is_bundle; then
				assert_bundle_count live_s3_list "$BUCKET" "$PREFIX" \
					refs/heads/feature 0
			fi

			# Distinguish "ref absent" (ls-remote exit==0, empty output)
			# from "ls-remote failed" (the latter would also produce empty
			# stdout under `2>/dev/null || true`). See
			# `assert_ls_remote_ref_absent` in spec/support/git_scenarios.sh.
			assert_ls_remote_ref_absent "$URL" refs/heads/feature
		End
	End
End

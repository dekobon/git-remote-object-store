# shellcheck shell=bash
# shellcheck disable=SC2154 # variables defined by shellspec hooks

Describe "S3 helper: core git operations against rustfs"
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

	Describe "push first commit to empty remote"
		setup() {
			BUCKET=$(rustfs_unique_bucket)
			PREFIX="myrepo"
			rustfs_make_bucket "$BUCKET"
			WORK="$SHELLSPEC_TMPDIR/repo-$$-$RANDOM"
			mk_local_repo "$WORK"
			SHA=$(commit_in_repo "$WORK" hello.txt "hi" "first commit")
			add_remote "$WORK" origin "$(rustfs_url "$BUCKET" "$PREFIX")"
		}
		BeforeEach 'setup'

		It "creates exactly one bundle and a HEAD pointer matching the ref"
			When call push_branch "$WORK" origin refs/heads/main:refs/heads/main
			The status should equal 0

			assert_bundle_count rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main 1
			assert_bundle_sha_for_ref rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main "$SHA"
			assert_head_pointer rustfs_get_object "$BUCKET" "$PREFIX" \
				refs/heads/main
		End
	End

	Describe "clone an existing remote"
		setup() {
			BUCKET=$(rustfs_unique_bucket)
			PREFIX="myrepo"
			rustfs_make_bucket "$BUCKET"
			URL=$(rustfs_url "$BUCKET" "$PREFIX")
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
			BUCKET=$(rustfs_unique_bucket)
			PREFIX="myrepo"
			rustfs_make_bucket "$BUCKET"
			URL=$(rustfs_url "$BUCKET" "$PREFIX")
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
			# Pre-condition: bucket contains SHA1's bundle.
			assert_bundle_sha_for_ref rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main "$SHA1"

			When call push_branch "$SRC" origin refs/heads/main:refs/heads/main
			The status should equal 0

			# Post-condition: SHA1 replaced by SHA2 logically. Under
			# issue #157 the bundle engine defers prior-bundle deletion
			# via a baseline tombstone, so SHA1's bundle survives on
			# the bucket during the gc grace window. Pass the getter so
			# the assertion subtracts the tombstoned key set and the
			# "exactly one bundle, named SHA2" invariant still holds
			# against the logical (live) state.
			assert_bundle_sha_for_ref rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main "$SHA2" rustfs_get_object

			fetch_remote "$DST" origin
			assert_git_sha_equals "$DST" refs/remotes/origin/main "$SHA2"
		End
	End

	Describe "second branch and tag"
		setup() {
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
			SHA_FEATURE=$(commit_in_repo "$SRC" feature.txt "ff" "feature commit")
			push_branch "$SRC" origin refs/heads/feature:refs/heads/feature
			tag_in_repo "$SRC" v1 -m "release v1"
			SHA_TAG=$(git -C "$SRC" rev-parse v1)
			push_branch "$SRC" origin refs/tags/v1:refs/tags/v1
		}
		BeforeEach 'setup'

		verify_branch_and_tag() {
			assert_bundle_count rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature 1 \
				&& assert_bundle_sha_for_ref rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature "$SHA_FEATURE" \
				&& assert_bundle_count rustfs_list "$BUCKET" "$PREFIX" \
				refs/tags/v1 1 \
				&& assert_bundle_sha_for_ref rustfs_list "$BUCKET" "$PREFIX" \
				refs/tags/v1 "$SHA_TAG"
		}

		# Fetch round-trip — the on-bucket pack must contain the tag
		# object, otherwise `git fetch` lands the commits but cannot
		# update `refs/tags/v1` to the tag-OID.
		verify_tag_round_trips_via_fetch() {
			local DST="$SHELLSPEC_TMPDIR/dst-$$-$RANDOM"
			git clone -q "$URL" "$DST" || return 1
			# `git cat-file -t v1` must print "tag" — if the tag object
			# isn't in the receiver's ODB, this fails or prints "commit".
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

	Describe "annotated tag pointing at a tree (#80)"
		setup() {
			BUCKET=$(rustfs_unique_bucket)
			PREFIX="myrepo"
			rustfs_make_bucket "$BUCKET"
			URL=$(rustfs_url "$BUCKET" "$PREFIX")
			SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
			DST="$SHELLSPEC_TMPDIR/dst-$$-$RANDOM"
			mk_local_repo "$SRC"
			commit_in_repo "$SRC" hello.txt "hi" "first" >/dev/null
			SHA_TREE=$(git -C "$SRC" rev-parse "HEAD^{tree}")
			SHA_TREE_TAG=$(mktag_in_repo "$SRC" refs/tags/tree-tag "$SHA_TREE" tree)
			add_remote "$SRC" origin "$URL"
			# Push the branch first (so the bucket is initialised), then
			# the tag. The tag push is the change-under-test.
			push_branch "$SRC" origin refs/heads/main:refs/heads/main
		}
		BeforeEach 'setup'

		It "round-trips a tag whose target is a tree"
			When call push_branch "$SRC" origin refs/tags/tree-tag:refs/tags/tree-tag
			The status should equal 0

			# Fetch into a fresh clone and confirm the tag resolves to a
			# tag object whose ^{} peels to a tree.
			git clone -q "$URL" "$DST"
			git -C "$DST" fetch -q origin "refs/tags/tree-tag:refs/tags/tree-tag"
			local kind peel_kind
			kind=$(git -C "$DST" cat-file -t tree-tag)
			[[ "$kind" == "tag" ]] || {
				echo "expected tag, got $kind" >&2
				return 1
			}
			peel_kind=$(git -C "$DST" cat-file -t 'tree-tag^{}')
			[[ "$peel_kind" == "tree" ]] || {
				echo "expected tree-tag^{} to peel to a tree, got $peel_kind" >&2
				return 1
			}
		End
	End

	Describe "annotated tag pointing at a blob (#80)"
		setup() {
			BUCKET=$(rustfs_unique_bucket)
			PREFIX="myrepo"
			rustfs_make_bucket "$BUCKET"
			URL=$(rustfs_url "$BUCKET" "$PREFIX")
			SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
			DST="$SHELLSPEC_TMPDIR/dst-$$-$RANDOM"
			mk_local_repo "$SRC"
			commit_in_repo "$SRC" hello.txt "hi" "first" >/dev/null
			# Write a free-standing blob (not committed) and tag it.
			echo "leaf-blob" > "$SRC/leaf"
			SHA_BLOB=$(git -C "$SRC" hash-object -w "$SRC/leaf")
			SHA_BLOB_TAG=$(mktag_in_repo "$SRC" refs/tags/blob-tag "$SHA_BLOB" blob)
			add_remote "$SRC" origin "$URL"
			push_branch "$SRC" origin refs/heads/main:refs/heads/main
		}
		BeforeEach 'setup'

		It "round-trips a tag whose target is a blob"
			When call push_branch "$SRC" origin refs/tags/blob-tag:refs/tags/blob-tag
			The status should equal 0

			git clone -q "$URL" "$DST"
			git -C "$DST" fetch -q origin "refs/tags/blob-tag:refs/tags/blob-tag"
			local kind peel_kind
			kind=$(git -C "$DST" cat-file -t blob-tag)
			[[ "$kind" == "tag" ]] || {
				echo "expected tag, got $kind" >&2
				return 1
			}
			peel_kind=$(git -C "$DST" cat-file -t 'blob-tag^{}')
			[[ "$peel_kind" == "blob" ]] || {
				echo "expected blob-tag^{} to peel to a blob, got $peel_kind" >&2
				return 1
			}
		End
	End

	Describe "delete branch via empty-source push"
		setup() {
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
			assert_bundle_count rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature 1

			When call push_branch "$SRC" origin :refs/heads/feature
			The status should equal 0

			assert_bundle_count rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature 0

			# Distinguish "ref absent" (ls-remote exit==0, empty output)
			# from "ls-remote failed" (the latter would also produce empty
			# stdout under `2>/dev/null || true`). See
			# `assert_ls_remote_ref_absent` in spec/support/git_scenarios.sh.
			assert_ls_remote_ref_absent "$URL" refs/heads/feature
		End
	End
End

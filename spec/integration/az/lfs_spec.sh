# shellcheck shell=bash
# shellcheck disable=SC2154

Describe "Azure helper: LFS round-trip via git-lfs-object-store"
	Include spec/support/images.sh
	Include spec/support/docker_backend.sh
	Include spec/support/azurite.sh
	Include spec/support/git_scenarios.sh
	Include spec/support/bucket_assertions.sh

	Skip if "set INTEGRATION_AZ=1 to enable" flag_unset INTEGRATION_AZ
	Skip if "docker not on PATH" missing_cmd docker
	Skip if "az-cli not on PATH" missing_cmd az
	Skip if "git not on PATH" missing_cmd git
	Skip if "git-lfs not on PATH" missing_cmd git-lfs
	Skip if "sha256sum not on PATH" missing_cmd sha256sum

	BeforeAll 'azurite_start'
	AfterAll 'azurite_stop'

	make_fixture() {
		local out="$1"
		printf '\xff%.0s' {1..4096} >"$out"
	}

	lfs_oid() {
		local file="$1"
		sha256sum "$file" | awk '{print $1}'
	}

	# Stage the repo through the LFS-tracked commit but stop *before*
	# the final push. Used by both Its: the upload-contract It does
	# the push inside its `When call` so the load-bearing assertion
	# (`assert_lfs_object_exists`) depends on the code under test, not
	# on setup. The round-trip It calls `setup_lfs_repo_pushed` to
	# additionally perform the push as part of its setup.
	setup_lfs_repo_unpushed() {
		CONTAINER=$(azurite_unique_container)
		PREFIX="myrepo"
		azurite_make_container "$CONTAINER"
		URL=$(azurite_url "$CONTAINER" "$PREFIX")
		SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
		DST="$SHELLSPEC_TMPDIR/dst-$$-$RANDOM"
		FIXTURE="$SHELLSPEC_TMPDIR/big-$$-$RANDOM.bin"
		make_fixture "$FIXTURE"
		OID=$(lfs_oid "$FIXTURE")

		mk_local_repo "$SRC"
		commit_in_repo "$SRC" README.md "hi" "initial commit" >/dev/null
		add_remote "$SRC" origin "$URL"
		push_branch "$SRC" origin refs/heads/main:refs/heads/main

		git lfs install --skip-repo >/dev/null
		( cd "$SRC" && git-lfs-object-store install >/dev/null )
		git -C "$SRC" lfs track '*.bin' >/dev/null

		cp "$FIXTURE" "$SRC/big.bin"
		git -C "$SRC" add .gitattributes big.bin
		git -C "$SRC" commit -q -m "add LFS-tracked binary"
	}

	setup_lfs_repo_pushed() {
		setup_lfs_repo_unpushed
		push_branch "$SRC" origin refs/heads/main:refs/heads/main
	}

	push_lfs_main() {
		push_branch "$SRC" origin refs/heads/main:refs/heads/main
	}

	Describe "push uploads the LFS object"
		# Push runs inside the `It` — `assert_lfs_object_exists` is the
		# load-bearing assertion and depends on the code under test.
		BeforeEach 'setup_lfs_repo_unpushed'

		It "places the object at <prefix>/lfs/<oid>"
			When call push_lfs_main
			The status should equal 0
			assert_lfs_object_exists azurite_list "$CONTAINER" "$PREFIX" "$OID"
		End
	End

	Describe "clone round-trips the LFS-tracked file"
		# Push happens in BeforeEach; the It exercises the clone + pull
		# path so `cmp` is the load-bearing assertion.
		BeforeEach 'setup_lfs_repo_pushed'

		It "clone + lfs pull reproduces the working-tree bytes"
			GIT_LFS_SKIP_SMUDGE=1 git clone "$URL" "$DST" >/dev/null 2>&1
			( cd "$DST" && git-lfs-object-store install >/dev/null )
			When call git -C "$DST" lfs pull
			The status should equal 0

			cmp "$FIXTURE" "$DST/big.bin"
		End
	End
End

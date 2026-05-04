# shellcheck shell=bash
# shellcheck disable=SC2154

Describe "S3 helper: LFS round-trip via git-lfs-object-store"
	Include spec/support/images.sh
	Include spec/support/docker_backend.sh
	Include spec/support/rustfs.sh
	Include spec/support/git_scenarios.sh
	Include spec/support/bucket_assertions.sh

	Skip if "set INTEGRATION_S3=1 to enable" flag_unset INTEGRATION_S3
	Skip if "docker not on PATH" missing_cmd docker
	Skip if "aws-cli not on PATH" missing_cmd aws
	Skip if "git not on PATH" missing_cmd git
	Skip if "git-lfs not on PATH" missing_cmd git-lfs
	Skip if "sha256sum not on PATH" missing_cmd sha256sum

	BeforeAll 'rustfs_start'
	AfterAll 'rustfs_stop'

	# A deterministic 4 KiB binary fixture of 0xFF bytes. The OID
	# (SHA-256 of file contents) is stable across runs so test
	# assertions can name the exact bucket key without recomputing.
	make_fixture() {
		local out="$1"
		# `printf '\xff%.0s' {1..4096}` writes 4096 0xFF bytes via the
		# shell's brace-expansion + format-reuse trick.
		printf '\xff%.0s' {1..4096} >"$out"
	}

	# Compute the LFS OID for a file (sha256 of contents, hex).
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
		BUCKET=$(rustfs_unique_bucket)
		PREFIX="myrepo"
		rustfs_make_bucket "$BUCKET"
		URL=$(rustfs_url "$BUCKET" "$PREFIX")
		SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
		DST="$SHELLSPEC_TMPDIR/dst-$$-$RANDOM"
		FIXTURE="$SHELLSPEC_TMPDIR/big-$$-$RANDOM.bin"
		make_fixture "$FIXTURE"
		OID=$(lfs_oid "$FIXTURE")

		mk_local_repo "$SRC"
		# Pre-seed an ordinary commit so the bundle for `main` exists
		# before the LFS push. Pushing only LFS objects without any git
		# refs is not the supported flow.
		commit_in_repo "$SRC" README.md "hi" "initial commit" >/dev/null
		add_remote "$SRC" origin "$URL"
		push_branch "$SRC" origin refs/heads/main:refs/heads/main

		# Wire LFS into the repo:
		#   1. `git lfs install --skip-repo` registers the LFS hook
		#      machinery without touching the working repo's config
		#      (we manage that via our own `install` subcommand).
		#   2. `git-lfs-object-store install` writes the customtransfer
		#      config (lfs.customtransfer.git-lfs-object-store.path /
		#      .standalonetransferagent — see src/lfs/install.rs).
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
			assert_lfs_object_exists rustfs_list "$BUCKET" "$PREFIX" "$OID"
		End
	End

	Describe "clone round-trips the LFS-tracked file"
		# Push happens in BeforeEach; the It exercises the clone + pull
		# path so `cmp` is the load-bearing assertion.
		BeforeEach 'setup_lfs_repo_pushed'

		It "clone + lfs pull reproduces the working-tree bytes"
			# Clone with smudge disabled, install the agent, then pull.
			# Without the customtransfer config, the smudge filter has
			# no transport and the working-tree file would remain a
			# pointer stub.
			GIT_LFS_SKIP_SMUDGE=1 git clone "$URL" "$DST" >/dev/null 2>&1
			( cd "$DST" && git-lfs-object-store install >/dev/null )
			When call git -C "$DST" lfs pull
			The status should equal 0

			# Working-tree byte compare.
			cmp "$FIXTURE" "$DST/big.bin"
		End
	End
End

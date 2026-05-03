# shellcheck shell=bash
# shellcheck disable=SC2154

Describe "S3 helper (live AWS): LFS round-trip via git-lfs-object-store"
	Include spec/support/live_common.sh
	Include spec/support/live_s3.sh
	Include spec/support/git_scenarios.sh
	Include spec/support/bucket_assertions.sh

	Skip if "set LIVE_S3=1 to enable" flag_unset LIVE_S3
	Skip if "git-lfs not on PATH" missing_cmd git-lfs
	Skip if "sha256sum not on PATH" missing_cmd sha256sum

	BeforeAll 'live_s3_setup'
	AfterAll 'live_s3_teardown'

	# A deterministic 4 KiB binary fixture of 0xFF bytes. The OID
	# (SHA-256 of file contents) is stable across runs so test
	# assertions can name the exact bucket key without recomputing.
	make_fixture() {
		local out="$1"
		printf '\xff%.0s' {1..4096} >"$out"
	}

	lfs_oid() {
		local file="$1"
		sha256sum "$file" | awk '{print $1}'
	}

	setup_lfs_repo() {
		BUCKET="$LIVE_S3_BUCKET"
		PREFIX=$(live_s3_unique_prefix)
		URL=$(live_s3_url "$PREFIX")
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

		git lfs install --skip-repo >/dev/null
		( cd "$SRC" && git-lfs-object-store install >/dev/null )
		git -C "$SRC" lfs track '*.bin' >/dev/null

		cp "$FIXTURE" "$SRC/big.bin"
		git -C "$SRC" add .gitattributes big.bin
		git -C "$SRC" commit -q -m "add LFS-tracked binary"
		push_branch "$SRC" origin refs/heads/main:refs/heads/main
	}

	Describe "push then clone"
		BeforeEach 'setup_lfs_repo'

		It "uploads the object to <prefix>/lfs/<oid> and round-trips on clone"
			if live_engine_is_bundle; then
				assert_lfs_object_exists live_s3_list "$BUCKET" "$PREFIX" "$OID"
			fi

			# Clone with smudge disabled, install the agent, then pull.
			# Without the customtransfer config, the smudge filter has
			# no transport and the working-tree file would remain a
			# pointer stub.
			GIT_LFS_SKIP_SMUDGE=1 git clone "$URL" "$DST" >/dev/null 2>&1
			( cd "$DST" && git-lfs-object-store install >/dev/null )
			When call git -C "$DST" lfs pull
			The status should equal 0

			cmp "$FIXTURE" "$DST/big.bin"
		End
	End
End

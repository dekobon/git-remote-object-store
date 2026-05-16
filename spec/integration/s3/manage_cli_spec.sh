# shellcheck shell=bash
# shellcheck disable=SC2154

Describe "S3 helper: management CLI against rustfs"
	Include spec/support/images.sh
	Include spec/support/docker_backend.sh
	Include spec/support/rustfs.sh
	Include spec/support/git_scenarios.sh
	Include spec/support/bucket_assertions.sh

	Skip if "set INTEGRATION_S3=1 to enable" flag_unset INTEGRATION_S3
	Skip if "docker not on PATH" missing_cmd docker
	Skip if "aws-cli not on PATH" missing_cmd aws
	Skip if "git not on PATH" missing_cmd git
	Skip if "script(1) not on PATH" missing_cmd script

	BeforeAll 'rustfs_start'
	AfterAll 'rustfs_stop'

	# Common setup: create a bucket and push a repo with one commit.
	# Tests can then run protect/unprotect/delete-branch/doctor against
	# the populated bucket.
	setup_bucket_with_main() {
		BUCKET=$(rustfs_unique_bucket)
		PREFIX="myrepo"
		rustfs_make_bucket "$BUCKET"
		URL=$(rustfs_url "$BUCKET" "$PREFIX")
		SRC="$SHELLSPEC_TMPDIR/src-$$-$RANDOM"
		mk_local_repo "$SRC"
		SHA_MAIN=$(commit_in_repo "$SRC" hello.txt "hi" "first commit")
		add_remote "$SRC" origin "$URL"
		push_branch "$SRC" origin refs/heads/main:refs/heads/main
	}

	Describe "protect then unprotect"
		BeforeEach 'setup_bucket_with_main'

		quiet_protect() {
			git-remote-object-store protect "$URL" main >/dev/null
		}
		quiet_unprotect() {
			git-remote-object-store unprotect "$URL" main >/dev/null
		}

		It "writes the PROTECTED# marker on protect and removes it on unprotect"
			When call quiet_protect
			The status should equal 0
			assert_protected_marker rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main

			quiet_unprotect
			assert_no_protected_marker rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main
		End
	End

	Describe "delete-branch"
		setup_with_feature() {
			setup_bucket_with_main
			git -C "$SRC" checkout -q -b feature
			SHA_FEATURE=$(commit_in_repo "$SRC" feature.txt "ff" "feature")
			push_branch "$SRC" origin refs/heads/feature:refs/heads/feature
		}
		BeforeEach 'setup_with_feature'

		# `git-remote-object-store delete-branch` calls
		# `dialoguer::Confirm::interact()`, which requires a TTY and
		# fails with `fatal: not a terminal` when fed via a pipe. Use
		# `script -qec` to allocate a pty so the prompt machinery is
		# satisfied; feed `y\n` through stdin to confirm.
		delete_feature_via_pty() {
			script -qec "git-remote-object-store delete-branch \"$URL\" feature" /dev/null <<<'y' >/dev/null
		}

		It "removes every bundle under the branch after a y/N prompt"
			# Pre-condition: feature is on the remote.
			assert_bundle_count rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature 1

			When call delete_feature_via_pty
			The status should equal 0

			assert_bundle_count rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/feature 0
			refs_listed=$(git ls-remote "$URL" refs/heads/feature 2>/dev/null || true)
			The variable refs_listed should equal ""
		End
	End

	Describe "doctor --delete-stale-locks"
		# Pre-seed a `LOCK#.lock` object directly via aws-cli, then run
		# doctor with a zero TTL so every existing lock is stale. The
		# `--delete-stale-locks` flag is non-interactive (no `Confirm`
		# prompts; see src/manage/doctor.rs:320). No `select` calls are
		# triggered because there are no duplicate bundles and HEAD is
		# valid, so a TTY is not required.
		setup_with_stale_lock() {
			setup_bucket_with_main
			LOCK_FILE="$SHELLSPEC_TMPDIR/lockbody.$$"
			: >"$LOCK_FILE"
			rustfs_put_object "$BUCKET" \
				"$PREFIX/refs/heads/main/LOCK#.lock" "$LOCK_FILE"
		}
		BeforeEach 'setup_with_stale_lock'

		quiet_doctor() {
			git-remote-object-store doctor "$URL" \
				--lock-ttl-seconds 0 --delete-stale-locks >/dev/null
		}

		It "removes the stale lock from the ref directory"
			assert_lock_present rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main

			When call quiet_doctor
			The status should equal 0

			assert_lock_absent rustfs_list "$BUCKET" "$PREFIX" \
				refs/heads/main
		End
	End
End

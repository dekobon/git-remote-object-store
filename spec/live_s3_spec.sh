# shellcheck shell=bash
# shellcheck disable=SC2154

# Unit-level coverage for `spec/support/live_s3.sh`. Locks the URL
# grammar, the `aws` argv composition (region/profile flags), and the
# prefix-safety guard the live-cloud cleanup path leans on — no
# live-cloud dependency, no aws-cli required, runs as part of the
# default shellspec suite.

Describe "live_s3.sh: live_s3_url"
	Include spec/support/live_common.sh
	Include spec/support/live_s3.sh

	It "constructs an s3+https URL with engine=bundle by default"
		export LIVE_S3_BUCKET=mybucket
		export LIVE_S3_REGION=us-east-2
		unset LIVE_S3_PROFILE LIVE_ENGINE
		When call live_s3_url myrepo/prefix
		The status should equal 0
		The output should equal "s3+https://mybucket.s3.us-east-2.amazonaws.com/myrepo/prefix?engine=bundle"
	End

	It "appends &profile= when LIVE_S3_PROFILE is set"
		export LIVE_S3_BUCKET=mybucket
		export LIVE_S3_REGION=us-east-2
		export LIVE_S3_PROFILE=myprofile
		unset LIVE_ENGINE
		When call live_s3_url myrepo
		The status should equal 0
		The output should equal "s3+https://mybucket.s3.us-east-2.amazonaws.com/myrepo?engine=bundle&profile=myprofile"
	End

	It "plumbs LIVE_ENGINE through as ?engine="
		export LIVE_S3_BUCKET=mybucket
		export LIVE_S3_REGION=us-east-2
		export LIVE_ENGINE=future-engine
		unset LIVE_S3_PROFILE
		When call live_s3_url myrepo
		The output should equal "s3+https://mybucket.s3.us-east-2.amazonaws.com/myrepo?engine=future-engine"
	End

	It "joins engine and profile with &"
		# Pin the order so a refactor that reverses query-param order
		# is caught immediately.
		export LIVE_S3_BUCKET=mybucket
		export LIVE_S3_REGION=us-east-2
		export LIVE_S3_PROFILE=myprofile
		export LIVE_ENGINE=future-engine
		When call live_s3_url myrepo
		The output should equal "s3+https://mybucket.s3.us-east-2.amazonaws.com/myrepo?engine=future-engine&profile=myprofile"
	End

	It "rejects an empty prefix"
		export LIVE_S3_BUCKET=mybucket
		export LIVE_S3_REGION=us-east-2
		When call live_s3_url ""
		The status should not equal 0
		The stderr should include "requires <prefix>"
	End
End

Describe "live_s3.sh: live_s3 aws argv composition"
	Include spec/support/live_common.sh
	Include spec/support/live_s3.sh

	# Source-of-truth for the asserted argv ordering: aws-CLI accepts
	# global flags (`--region`, `--profile`) before the subcommand path,
	# so the wrapper at spec/support/live_s3.sh:28-35 prepends them via
	# `aws "${args[@]}" "$@"`. This pins that order — opposite to the
	# az wrapper, which appends auth flags after the user args because
	# the az subcommand path is parsed positionally.
	#
	# Mock the `aws` binary by defining a shell function with the same
	# name. Bash function definitions take precedence over PATH lookups,
	# so every call to `aws ...` inside this Describe block prints its
	# argv on stdout instead of touching the network.
	aws() {
		printf '%s\n' "$@"
	}

	It "passes --region and forwards user args after"
		export LIVE_S3_REGION=us-east-2
		unset LIVE_S3_PROFILE
		When call live_s3_aws s3 ls
		The status should equal 0
		The line 1 of output should equal "--region"
		The line 2 of output should equal "us-east-2"
		The line 3 of output should equal "s3"
		The line 4 of output should equal "ls"
	End

	It "passes --region followed by --profile when LIVE_S3_PROFILE is set"
		export LIVE_S3_REGION=us-east-2
		export LIVE_S3_PROFILE=myprofile
		When call live_s3_aws s3 ls
		The status should equal 0
		The line 1 of output should equal "--region"
		The line 2 of output should equal "us-east-2"
		The line 3 of output should equal "--profile"
		The line 4 of output should equal "myprofile"
		The line 5 of output should equal "s3"
		The line 6 of output should equal "ls"
	End

	It "treats empty LIVE_S3_PROFILE as unset"
		# Covers the `[[ -n "${LIVE_S3_PROFILE:-}" ]]` branch — the
		# wrapper must not emit `--profile` for an empty-string profile.
		export LIVE_S3_REGION=us-east-2
		export LIVE_S3_PROFILE=""
		When call live_s3_aws s3 ls
		The status should equal 0
		The line 1 of output should equal "--region"
		The line 2 of output should equal "us-east-2"
		The line 3 of output should equal "s3"
		The line 4 of output should equal "ls"
	End
End

Describe "live_s3.sh: live_s3_clear_prefix safety guard"
	Include spec/support/live_common.sh
	Include spec/support/live_s3.sh

	# Belt-and-suspenders test: even if cleanup is invoked with a prefix
	# that would otherwise wipe the bucket root, the guard refuses. No
	# `aws` CLI needed because the function returns before invoking it.
	It "refuses to clear a prefix outside live-test/"
		When call live_s3_clear_prefix some-bucket other-prefix/foo
		The status should not equal 0
		The stderr should include "must start with 'live-test/'"
	End

	It "refuses an empty prefix"
		When call live_s3_clear_prefix some-bucket ""
		The status should not equal 0
		The stderr should include "requires <bucket> <prefix>"
	End
End

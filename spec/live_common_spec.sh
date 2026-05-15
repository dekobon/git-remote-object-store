# shellcheck shell=bash
# shellcheck disable=SC2154

# Unit-level coverage for `spec/support/live_common.sh`. Locks the
# prefix-safety contract that the live-cloud cleanup path leans on —
# no live-cloud dependency, no aws-cli required, runs as part of the
# default shellspec suite.

Describe "live_common.sh: live_assert_safe_prefix"
	Include spec/support/live_common.sh

	Describe "rejects empty input"
		It "fails with non-zero status when LIVE_RUN_PREFIX is unset"
			unset LIVE_RUN_PREFIX
			When call live_assert_safe_prefix
			The status should not equal 0
			The stderr should include "is empty"
		End

		It "fails with non-zero status when LIVE_RUN_PREFIX is the empty string"
			LIVE_RUN_PREFIX=""
			When call live_assert_safe_prefix
			The status should not equal 0
			The stderr should include "is empty"
		End
	End

	Describe "rejects malformed prefixes"
		# A prefix that does not start with `live-test/` would, if passed
		# to `aws s3 rm --recursive s3://$BUCKET/$LIVE_RUN_PREFIX/`,
		# delete data outside the test boundary. The guard must reject.
		It "fails when prefix is bare 'live-test' without trailing segment"
			LIVE_RUN_PREFIX="live-test"
			When call live_assert_safe_prefix
			The status should not equal 0
			The stderr should include "does not start with 'live-test/'"
		End

		It "fails when prefix points outside the live-test root"
			LIVE_RUN_PREFIX="other-prefix/foo"
			When call live_assert_safe_prefix
			The status should not equal 0
			The stderr should include "does not start with 'live-test/'"
		End

		It "fails when prefix has a leading slash"
			LIVE_RUN_PREFIX="/live-test/foo"
			When call live_assert_safe_prefix
			The status should not equal 0
			The stderr should include "does not start with 'live-test/'"
		End
	End

	Describe "accepts well-formed prefixes"
		It "succeeds for a prefix matching the run-id shape"
			LIVE_RUN_PREFIX="live-test/20260503T120000Z-12345-abcd1234"
			When call live_assert_safe_prefix
			The status should equal 0
		End

		It "succeeds for a prefix with a sub-segment"
			LIVE_RUN_PREFIX="live-test/20260503T120000Z-12345-abcd1234/spec-12345-ab"
			When call live_assert_safe_prefix
			The status should equal 0
		End
	End
End

Describe "live_common.sh: live_require_guard"
	Include spec/support/live_common.sh

	# This guard is the single source of truth for the cost-acknowledgement
	# check; both the Makefile recipes (`shellspec-live-*`) and the bash
	# entry points (`live_s3.sh`, `live_az.sh`, `utils/live-sweep.sh`) call
	# it. The test pins the wording and exit semantics that both layers
	# depend on.
	It "fails when LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY is unset"
		unset LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY
		When call live_require_guard
		The status should not equal 0
		The stderr should include "LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY is not set to 1"
		The stderr should include "export LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY=1"
	End

	It "fails when the variable is set to any value other than 1"
		LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY="yes"
		When call live_require_guard
		The status should not equal 0
		The stderr should include "is not set to 1"
	End

	It "fails when the variable is set to 0"
		LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY="0"
		When call live_require_guard
		The status should not equal 0
		The stderr should include "is not set to 1"
	End

	It "succeeds when the variable is set to exactly 1"
		LIVE_TESTS_I_UNDERSTAND_THIS_COSTS_MONEY="1"
		When call live_require_guard
		The status should equal 0
	End
End

Describe "Makefile: shellspec-live-* guard wiring"
	# Regression cover for the recipe-level guard. `make -n` prints the
	# expanded recipe without executing it, so the test never invokes
	# shellspec or contacts a cloud backend. If a future edit drops the
	# `$(check_live_guard)` line from a recipe, these tests fail before
	# the misconfigured target ever hits CI or a developer's terminal.
	It "shellspec-live-s3 delegates to live_require_guard"
		When run command make -n shellspec-live-s3
		The status should equal 0
		The output should include "live_require_guard"
	End

	It "shellspec-live-azure delegates to live_require_guard"
		When run command make -n shellspec-live-azure
		The status should equal 0
		The output should include "live_require_guard"
	End
End

Describe "live_common.sh: live_run_id"
	Include spec/support/live_common.sh

	It "produces a sortable timestamp-prefixed id"
		When call live_run_id
		The status should equal 0
		# YYYYMMDDTHHMMSSZ-<pid>-<8 hex chars>: the leading 16-char
		# timestamp keeps lexicographic ordering aligned with chronology.
		The output should match pattern '20[0-9][0-9][0-1][0-9][0-3][0-9]T[0-2][0-9][0-5][0-9][0-5][0-9]Z-*-*'
	End
End

Describe "live_common.sh: live_engine"
	Include spec/support/live_common.sh

	It "defaults to bundle when LIVE_ENGINE is unset"
		unset LIVE_ENGINE
		When call live_engine
		The output should equal "bundle"
	End

	It "echoes the operator-supplied LIVE_ENGINE value"
		LIVE_ENGINE="future-engine"
		When call live_engine
		The output should equal "future-engine"
	End
End

Describe "live_common.sh: live_filter_stale_prefixes"
	Include spec/support/live_common.sh

	# The function is the lexicographic-cutoff core of `live-sweep.sh`.
	# Run-id format is `YYYYMMDDTHHMMSSZ-<pid>-<rand>`, so the leading
	# 16-char timestamp is compared as a string.
	It "passes through prefixes whose timestamp is strictly older than the cutoff"
		Data
			#|live-test/20260101T000000Z-100-aaaa/
			#|live-test/20260601T000000Z-200-bbbb/
		End
		When call live_filter_stale_prefixes 20260301T000000Z
		The status should equal 0
		The output should equal "live-test/20260101T000000Z-100-aaaa/"
	End

	It "treats the cutoff itself as not stale (strict less-than)"
		Data
			#|live-test/20260301T000000Z-100-aaaa/
		End
		When call live_filter_stale_prefixes 20260301T000000Z
		The status should equal 0
		The output should equal ""
	End

	It "ignores empty input lines"
		Data
			#|
			#|live-test/20260101T000000Z-1-aa/
			#|
		End
		When call live_filter_stale_prefixes 20260601T000000Z
		The output should equal "live-test/20260101T000000Z-1-aa/"
	End

	It "tolerates prefixes without a trailing slash"
		Data
			#|live-test/20260101T000000Z-1-aa
		End
		When call live_filter_stale_prefixes 20260601T000000Z
		The output should equal "live-test/20260101T000000Z-1-aa"
	End

	It "fails fast when cutoff is missing"
		When call live_filter_stale_prefixes ""
		The status should not equal 0
		The stderr should include "requires <cutoff_stamp>"
	End

	It "emits nothing when input is empty"
		Data ""
		When call live_filter_stale_prefixes 20260301T000000Z
		The status should equal 0
		The output should equal ""
	End
End

Describe "live_common.sh: live_engine_is_bundle"
	Include spec/support/live_common.sh

	It "is true for the default engine"
		unset LIVE_ENGINE
		When call live_engine_is_bundle
		The status should equal 0
	End

	It "is true for explicit bundle"
		LIVE_ENGINE="bundle"
		When call live_engine_is_bundle
		The status should equal 0
	End

	It "is false for any other engine"
		LIVE_ENGINE="future-engine"
		When call live_engine_is_bundle
		The status should not equal 0
	End
End

# shellcheck shell=bash
# shellcheck disable=SC2154

# Unit-level coverage for `spec/support/live_az.sh`. Locks the
# credential-alias resolution contract (priority order, alias
# normalisation) that the live-cloud Azure cleanup path leans on —
# no live-cloud dependency, no az-cli required, runs as part of the
# default shellspec suite.

Describe "live_az.sh: live_az_credential_env_value priority"
	Include spec/support/live_common.sh
	Include spec/support/live_az.sh

	# Mirrors `resolve_alias` in src/object_store/azure/auth.rs:
	# KEY → CONNECTION_STRING → SAS, first hit wins.
	clear_creds() {
		unset AZSTORE_PROD_KEY AZSTORE_PROD_CONNECTION_STRING AZSTORE_PROD_SAS
	}

	It "prefers KEY when all three env vars are set"
		LIVE_AZ_CREDENTIAL_NAME=PROD
		clear_creds
		export AZSTORE_PROD_KEY=key-value
		export AZSTORE_PROD_CONNECTION_STRING=conn-value
		export AZSTORE_PROD_SAS=sas-value
		When call live_az_credential_env_value
		The status should equal 0
		The output should equal "$(printf 'KEY\tkey-value')"
	End

	It "falls back to CONNECTION_STRING when KEY is absent"
		LIVE_AZ_CREDENTIAL_NAME=PROD
		clear_creds
		export AZSTORE_PROD_CONNECTION_STRING=conn-value
		export AZSTORE_PROD_SAS=sas-value
		When call live_az_credential_env_value
		The status should equal 0
		The output should equal "$(printf 'CONN\tconn-value')"
	End

	It "falls back to SAS when KEY and CONNECTION_STRING are absent"
		LIVE_AZ_CREDENTIAL_NAME=PROD
		clear_creds
		export AZSTORE_PROD_SAS=sas-value
		When call live_az_credential_env_value
		The status should equal 0
		The output should equal "$(printf 'SAS\tsas-value')"
	End

	It "returns non-zero with a clear message when none are set"
		LIVE_AZ_CREDENTIAL_NAME=PROD
		clear_creds
		When call live_az_credential_env_value
		The status should not equal 0
		The stderr should include "AZSTORE_PROD_KEY"
		The stderr should include "AZSTORE_PROD_CONNECTION_STRING"
		The stderr should include "AZSTORE_PROD_SAS"
	End

	It "uppercases the alias to match the helper's env-var lookup"
		# `resolve_alias` in src/object_store/azure/auth.rs ASCII-uppercases
		# the alias before building the env-var name, so a lowercase
		# alias must resolve the same way here.
		LIVE_AZ_CREDENTIAL_NAME=prod
		clear_creds
		unset AZSTORE_prod_KEY
		export AZSTORE_PROD_KEY=upper-key
		When call live_az_credential_env_value
		The status should equal 0
		The output should equal "$(printf 'KEY\tupper-key')"
	End
End

Describe "live_az.sh: live_az_url"
	Include spec/support/live_common.sh
	Include spec/support/live_az.sh

	It "constructs a virtual-hosted az+https URL with credential and engine"
		export LIVE_AZ_ACCOUNT=myacct
		export LIVE_AZ_CONTAINER=mycontainer
		export LIVE_AZ_CREDENTIAL_NAME=PROD
		unset LIVE_AZ_ENDPOINT_SUFFIX LIVE_ENGINE
		When call live_az_url myrepo/prefix
		The status should equal 0
		The output should equal "az+https://myacct.blob.core.windows.net/mycontainer/myrepo/prefix?credential=PROD&engine=bundle"
	End

	It "honours LIVE_AZ_ENDPOINT_SUFFIX for sovereign clouds"
		export LIVE_AZ_ACCOUNT=myacct
		export LIVE_AZ_CONTAINER=mycontainer
		export LIVE_AZ_CREDENTIAL_NAME=PROD
		export LIVE_AZ_ENDPOINT_SUFFIX=blob.core.usgovcloudapi.net
		unset LIVE_ENGINE
		When call live_az_url repo
		The status should equal 0
		The output should equal "az+https://myacct.blob.core.usgovcloudapi.net/mycontainer/repo?credential=PROD&engine=bundle"
	End

	It "plumbs LIVE_ENGINE through as ?engine="
		export LIVE_AZ_ACCOUNT=myacct
		export LIVE_AZ_CONTAINER=mycontainer
		export LIVE_AZ_CREDENTIAL_NAME=PROD
		export LIVE_ENGINE=future-engine
		unset LIVE_AZ_ENDPOINT_SUFFIX
		When call live_az_url repo
		The output should equal "az+https://myacct.blob.core.windows.net/mycontainer/repo?credential=PROD&engine=future-engine"
	End

	It "rejects an empty prefix"
		export LIVE_AZ_ACCOUNT=myacct
		export LIVE_AZ_CONTAINER=mycontainer
		export LIVE_AZ_CREDENTIAL_NAME=PROD
		When call live_az_url ""
		The status should not equal 0
		The stderr should include "requires <prefix>"
	End
End

Describe "live_az.sh: live_az_clear_prefix safety guard"
	Include spec/support/live_common.sh
	Include spec/support/live_az.sh

	# Belt-and-suspenders test: even if cleanup is invoked with a prefix
	# that would otherwise wipe the container root, the guard refuses.
	# No `az` CLI needed because the function returns before invoking it.
	It "refuses to clear a prefix outside live-test/"
		When call live_az_clear_prefix some-container other-prefix/foo
		The status should not equal 0
		The stderr should include "must start with 'live-test/'"
	End

	It "refuses an empty prefix"
		When call live_az_clear_prefix some-container ""
		The status should not equal 0
		The stderr should include "requires <container> <prefix>"
	End
End

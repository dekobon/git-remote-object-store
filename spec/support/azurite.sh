# shellcheck shell=bash

# Azurite (Azure Blob emulator) backend lifecycle for the shellspec
# integration suite. Mirrors `tests/azure_store_integration.rs`.

# Well-known Azurite account name and key. Hardcoded by the emulator and
# safe to embed in test code (matches `tests/azure_store_integration.rs`).
AZURITE_ACCOUNT="devstoreaccount1"
AZURITE_KEY="Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="
# Credential alias used by every test URL: `?credential=AZURITE`
# resolves `AZSTORE_AZURITE_KEY` via the helper's env-var lookup
# (see `src/object_store/azure.rs`).
AZURITE_CREDENTIAL_ALIAS="AZURITE"
# Container-internal blob port; host port allocated by docker -P.
AZURITE_CONTAINER_PORT="10000"

# azurite_start
# Pull the pinned image, run a detached container with the blob-only
# CMD override (matches the testcontainers fixture), wait for readiness,
# and export:
#   AZURITE_CONTAINER, AZURITE_PORT, AZURITE_BLOB_ENDPOINT,
#   AZURE_STORAGE_CONNECTION_STRING (for az-cli),
#   AZSTORE_AZURITE_KEY (for the helper binary).
azurite_start() {
	docker_pull "$AZURITE_IMAGE:$AZURITE_TAG" || return 1

	# Override the image's default CMD (the multiplexer `azurite`) to
	# run the blob service only; bind on 0.0.0.0 so the host-port
	# mapping is reachable; skip the strict API-version check because
	# the helper sends `x-ms-version: 2026-04-06` which Azurite 3.35.0
	# does not yet recognise.
	local cid
	cid=$(docker_run_detached "$AZURITE_IMAGE:$AZURITE_TAG" \
		-- \
		azurite-blob \
		--blobHost 0.0.0.0 \
		--blobPort "$AZURITE_CONTAINER_PORT" \
		--skipApiVersionCheck) || return 1
	cid="${cid//$'\n'/}"
	export AZURITE_CONTAINER="$cid"

	local port
	local tries=0
	while ((tries < 50)); do
		if port=$(docker_host_port "$cid" "$AZURITE_CONTAINER_PORT" 2>/dev/null); then
			break
		fi
		sleep 0.1
		tries=$((tries + 1))
	done
	if [[ -z "${port:-}" ]]; then
		echo "azurite_start: failed to discover host port for $cid" >&2
		docker_stop_rm "$cid"
		return 1
	fi
	export AZURITE_PORT="$port"
	export AZURITE_BLOB_ENDPOINT="http://127.0.0.1:${AZURITE_PORT}/${AZURITE_ACCOUNT}"

	# Readiness: a bare `GET /` against Azurite returns 400 (Account
	# name required) once it is serving.
	if ! docker_wait_http 127.0.0.1 "$AZURITE_PORT" 400 15 "$cid"; then
		docker_stop_rm "$cid"
		return 1
	fi

	export AZURE_STORAGE_CONNECTION_STRING="DefaultEndpointsProtocol=http;AccountName=${AZURITE_ACCOUNT};AccountKey=${AZURITE_KEY};BlobEndpoint=${AZURITE_BLOB_ENDPOINT};"
	# Helper binary picks this up via the credential-alias resolver.
	export "AZSTORE_${AZURITE_CREDENTIAL_ALIAS}_KEY=$AZURITE_KEY"
}

# azurite_stop
# Stop and remove the container; capture logs for failure forensics.
azurite_stop() {
	if [[ -z "${AZURITE_CONTAINER:-}" ]]; then
		return 0
	fi
	local log_dir="${SHELLSPEC_TMPBASE:-${TMPDIR:-/tmp}}/logs"
	docker_stop_rm "$AZURITE_CONTAINER" "$log_dir/azurite.log"
	unset AZURITE_CONTAINER AZURITE_PORT AZURITE_BLOB_ENDPOINT
	unset AZURE_STORAGE_CONNECTION_STRING
	unset "AZSTORE_${AZURITE_CREDENTIAL_ALIAS}_KEY"
}

# azurite_make_container <name>
# Create a blob container against the running Azurite.
azurite_make_container() {
	local name="$1"
	if [[ -z "$name" ]]; then
		echo "azurite_make_container: missing <name>" >&2
		return 1
	fi
	az storage container create --name "$name" --public-access off >/dev/null
}

# azurite_url <container> <prefix>
# Print the helper-protocol URL for a (container, prefix) pair.
azurite_url() {
	local container="$1"
	local prefix="$2"
	if [[ -z "$container" || -z "$prefix" ]]; then
		echo "azurite_url: requires <container> <prefix>" >&2
		return 1
	fi
	echo "az+http://127.0.0.1:${AZURITE_PORT}/${AZURITE_ACCOUNT}/${container}/${prefix}?credential=${AZURITE_CREDENTIAL_ALIAS}"
}

# azurite_list <container> <prefix>
# List every blob under <prefix> in <container>, one blob name per line.
azurite_list() {
	local container="$1"
	local prefix="$2"
	if [[ -z "$container" || -z "$prefix" ]]; then
		echo "azurite_list: requires <container> <prefix>" >&2
		return 1
	fi
	az storage blob list --container-name "$container" --prefix "$prefix" \
		--query '[].name' -o tsv 2>/dev/null \
		| tr '\t' '\n' \
		| grep -v '^$' \
		|| true
}

# azurite_put_object <container> <blob> <local_file>
# Upload <local_file> as <blob> in <container>. Used for pre-corruption.
azurite_put_object() {
	local container="$1"
	local blob="$2"
	local file="$3"
	if [[ -z "$container" || -z "$blob" || -z "$file" ]]; then
		echo "azurite_put_object: requires <container> <blob> <file>" >&2
		return 1
	fi
	az storage blob upload --container-name "$container" --name "$blob" \
		--file "$file" --overwrite true >/dev/null
}

# azurite_get_object <container> <blob> <out_file>
# Download <blob> from <container> into <out_file>. Used for HEAD-pointer
# assertions.
azurite_get_object() {
	local container="$1"
	local blob="$2"
	local out="$3"
	if [[ -z "$container" || -z "$blob" || -z "$out" ]]; then
		echo "azurite_get_object: requires <container> <blob> <out_file>" >&2
		return 1
	fi
	az storage blob download --container-name "$container" --name "$blob" \
		--file "$out" --no-progress >/dev/null
}

# azurite_unique_container
# Print a fresh lowercase container name (3–63 chars, alphanumeric+dash,
# no leading/trailing dash).
azurite_unique_container() {
	printf 'testctr-%s-%s' "$$" "${RANDOM}${RANDOM}" | tr '[:upper:]' '[:lower:]'
}

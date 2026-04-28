# shellcheck shell=bash

# rustfs (S3) backend lifecycle for the shellspec integration suite.
# Mirrors `tests/s3_store_integration.rs` so the Rust integration tests
# and shellspec tests exercise the same backend version.

# Default credentials. rustfs ships with `rustfsadmin` / `rustfsadmin`
# baked in by the install script and Docker image; tests use them too.
RUSTFS_USER="rustfsadmin"
RUSTFS_PASSWORD="rustfsadmin"
# Container-internal API port. The host port is allocated by docker -P
# and discovered via `docker_host_port`.
RUSTFS_CONTAINER_PORT="9000"

# rustfs_start
# Pull the pinned image, run a detached container, wait for readiness,
# and export the env vars subsequent helpers / aws-cli need:
#   RUSTFS_CONTAINER  — docker container ID
#   RUSTFS_PORT       — host port mapped to RUSTFS_CONTAINER_PORT
#   RUSTFS_ENDPOINT   — http://127.0.0.1:$RUSTFS_PORT (for aws --endpoint-url)
#   AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_REGION
rustfs_start() {
	docker_pull "$RUSTFS_IMAGE:$RUSTFS_TAG" || return 1

	local cid
	cid=$(docker_run_detached "$RUSTFS_IMAGE:$RUSTFS_TAG" \
		-e "RUSTFS_ROOT_USER=$RUSTFS_USER" \
		-e "RUSTFS_ROOT_PASSWORD=$RUSTFS_PASSWORD" \
		-e "RUSTFS_ACCESS_KEY=$RUSTFS_USER" \
		-e "RUSTFS_SECRET_KEY=$RUSTFS_PASSWORD" \
		-e "RUSTFS_ADDRESS=:$RUSTFS_CONTAINER_PORT" \
		-e "RUSTFS_CONSOLE_ENABLE=false" \
		-e "RUSTFS_OBS_LOG_DIRECTORY=") || return 1
	cid="${cid//$'\n'/}"
	export RUSTFS_CONTAINER="$cid"

	local port
	# rustfs's first cold start can take a couple of seconds before the
	# port mapping is published; retry briefly.
	local tries=0
	while ((tries < 50)); do
		if port=$(docker_host_port "$cid" "$RUSTFS_CONTAINER_PORT" 2>/dev/null); then
			break
		fi
		sleep 0.1
		tries=$((tries + 1))
	done
	if [[ -z "${port:-}" ]]; then
		echo "rustfs_start: failed to discover host port for $cid" >&2
		docker_stop_rm "$cid"
		return 1
	fi
	export RUSTFS_PORT="$port"
	export RUSTFS_ENDPOINT="http://127.0.0.1:${RUSTFS_PORT}"

	# Readiness: an unauthenticated `GET /` returns 403 once rustfs is
	# serving (matches `tests/s3_store_integration.rs` wait strategy).
	if ! docker_wait_http 127.0.0.1 "$RUSTFS_PORT" 403 30 "$cid"; then
		docker_stop_rm "$cid"
		return 1
	fi

	# Standard AWS env contract; the helper binary picks these up via the
	# default credential provider chain.
	export AWS_ACCESS_KEY_ID="$RUSTFS_USER"
	export AWS_SECRET_ACCESS_KEY="$RUSTFS_PASSWORD"
	export AWS_REGION="us-east-1"
	# Stop aws-cli from hitting our terminal pager mid-suite.
	export AWS_PAGER=""
}

# rustfs_stop
# Stop and remove the container; capture logs to
# $SHELLSPEC_TMPBASE/logs/rustfs.log for failure forensics.
rustfs_stop() {
	if [[ -z "${RUSTFS_CONTAINER:-}" ]]; then
		return 0
	fi
	local log_dir="${SHELLSPEC_TMPBASE:-${TMPDIR:-/tmp}}/logs"
	docker_stop_rm "$RUSTFS_CONTAINER" "$log_dir/rustfs.log"
	unset RUSTFS_CONTAINER RUSTFS_PORT RUSTFS_ENDPOINT
}

# rustfs_make_bucket <name>
# Create an S3 bucket via aws-cli against the running rustfs.
rustfs_make_bucket() {
	local bucket="$1"
	if [[ -z "$bucket" ]]; then
		echo "rustfs_make_bucket: missing <name>" >&2
		return 1
	fi
	aws --endpoint-url "$RUSTFS_ENDPOINT" s3 mb "s3://${bucket}" >/dev/null
}

# rustfs_url <bucket> <prefix>
# Print the helper-protocol URL for a (bucket, prefix) pair on this
# rustfs instance.
rustfs_url() {
	local bucket="$1"
	local prefix="$2"
	if [[ -z "$bucket" || -z "$prefix" ]]; then
		echo "rustfs_url: requires <bucket> <prefix>" >&2
		return 1
	fi
	echo "s3+http://127.0.0.1:${RUSTFS_PORT}/${bucket}/${prefix}"
}

# rustfs_list <bucket> <prefix>
# List every key under <prefix> in <bucket>, one key per line. Output
# is the bare key (no size/date columns).
rustfs_list() {
	local bucket="$1"
	local prefix="$2"
	if [[ -z "$bucket" || -z "$prefix" ]]; then
		echo "rustfs_list: requires <bucket> <prefix>" >&2
		return 1
	fi
	aws --endpoint-url "$RUSTFS_ENDPOINT" s3api list-objects-v2 \
		--bucket "$bucket" --prefix "$prefix" \
		--query 'Contents[].Key' --output text 2>/dev/null \
		| tr '\t' '\n' \
		| grep -v '^None$' \
		| grep -v '^$' \
		|| true
}

# rustfs_put_object <bucket> <key> <local_file>
# Upload <local_file> as <key> in <bucket>. Used to pre-corrupt the
# bucket for doctor / stale-lock scenarios.
rustfs_put_object() {
	local bucket="$1"
	local key="$2"
	local file="$3"
	if [[ -z "$bucket" || -z "$key" || -z "$file" ]]; then
		echo "rustfs_put_object: requires <bucket> <key> <file>" >&2
		return 1
	fi
	aws --endpoint-url "$RUSTFS_ENDPOINT" s3api put-object \
		--bucket "$bucket" --key "$key" --body "$file" >/dev/null
}

# rustfs_get_object <bucket> <key> <out_file>
# Download <key> from <bucket> to <out_file>. Used by HEAD-pointer
# assertions.
rustfs_get_object() {
	local bucket="$1"
	local key="$2"
	local out="$3"
	if [[ -z "$bucket" || -z "$key" || -z "$out" ]]; then
		echo "rustfs_get_object: requires <bucket> <key> <out_file>" >&2
		return 1
	fi
	aws --endpoint-url "$RUSTFS_ENDPOINT" s3api get-object \
		--bucket "$bucket" --key "$key" "$out" >/dev/null
}

# rustfs_unique_bucket
# Print a fresh lowercase bucket name (3–63 chars, alphanumeric+dash).
rustfs_unique_bucket() {
	printf 'testbkt-%s-%s' "$$" "${RANDOM}${RANDOM}" | tr '[:upper:]' '[:lower:]'
}

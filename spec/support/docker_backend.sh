# shellcheck shell=bash

# Generic docker lifecycle helpers shared by rustfs.sh and azurite.sh.
# No backend-specific knowledge; all functions take explicit args.
#
# Implementation note: cannot use `set -euo pipefail` (shellspec manages
# its own shell options; see spec/spec_helper.sh). Each helper checks
# return codes explicitly.

# docker_pull <image:tag>
# Idempotent pull. Suppresses progress output; surfaces errors on stderr.
docker_pull() {
	local ref="$1"
	if [[ -z "$ref" ]]; then
		echo "docker_pull: missing image:tag argument" >&2
		return 1
	fi
	docker pull --quiet "$ref" >/dev/null 2>&1
	local status=$?
	if ((status != 0)); then
		# Re-run without --quiet so the user sees the actual error.
		docker pull "$ref" >&2
		return 1
	fi
}

# docker_run_detached <image:tag> [docker-args...] -- [container-cmd...]
# Runs `docker run -d -P` with extra args. The optional `--` separator
# splits docker arguments from the container command. Echoes the container
# ID (the value docker prints on stdout).
docker_run_detached() {
	local ref="$1"
	shift
	if [[ -z "$ref" ]]; then
		echo "docker_run_detached: missing image:tag argument" >&2
		return 1
	fi

	local docker_args=()
	local container_cmd=()
	local saw_separator=0
	local arg
	for arg in "$@"; do
		if [[ "$saw_separator" == "0" && "$arg" == "--" ]]; then
			saw_separator=1
			continue
		fi
		if ((saw_separator)); then
			container_cmd+=("$arg")
		else
			docker_args+=("$arg")
		fi
	done

	docker run -d -P "${docker_args[@]}" "$ref" "${container_cmd[@]}"
}

# docker_host_port <container_id> <container_port>
# Print the host port mapped to <container_port>/tcp inside <container_id>.
# Returns non-zero if the container has no such mapping yet.
docker_host_port() {
	local cid="$1"
	local cport="$2"
	if [[ -z "$cid" || -z "$cport" ]]; then
		echo "docker_host_port: requires <container_id> <container_port>" >&2
		return 1
	fi
	local mapping
	mapping=$(docker port "$cid" "$cport/tcp" 2>/dev/null | head -n1)
	if [[ -z "$mapping" ]]; then
		echo "docker_host_port: no mapping for $cport/tcp on $cid" >&2
		return 1
	fi
	# `docker port` prints `0.0.0.0:NNNN` (or `[::]:NNNN`); take the
	# trailing port digits.
	echo "${mapping##*:}"
}

# docker_wait_http <host> <port> <expected_code> [timeout_seconds]
# Poll the given HTTP endpoint until it returns the expected status code.
# Default timeout 30 seconds. Returns non-zero if the timeout elapses or
# the container exits before readiness.
docker_wait_http() {
	local host="$1"
	local port="$2"
	local expected="$3"
	local timeout="${4:-30}"
	local cid="${5:-}"
	local start now code
	start=$SECONDS
	while :; do
		code=$(curl -s -o /dev/null -w '%{http_code}' "http://${host}:${port}/" 2>/dev/null || echo "000")
		if [[ "$code" == "$expected" ]]; then
			return 0
		fi
		now=$SECONDS
		if ((now - start > timeout)); then
			echo "docker_wait_http: timed out after ${timeout}s waiting for ${host}:${port} to return ${expected} (last=${code})" >&2
			return 1
		fi
		if [[ -n "$cid" ]] && ! docker inspect -f '{{.State.Running}}' "$cid" 2>/dev/null | grep -q true; then
			echo "docker_wait_http: container $cid exited before readiness" >&2
			docker logs "$cid" >&2 2>/dev/null || true
			return 1
		fi
		sleep 0.2
	done
}

# docker_dump_logs <container_id> <log_path>
# Append `docker logs <container_id>` to <log_path>. Idempotent; safe to
# call from teardown even if the container has already been removed.
docker_dump_logs() {
	local cid="$1"
	local path="$2"
	if [[ -z "$cid" || -z "$path" ]]; then
		return 0
	fi
	mkdir -p "$(dirname "$path")"
	docker logs "$cid" >"$path" 2>&1 || true
}

# docker_stop_rm <container_id> [log_path]
# Stop and remove the container. If <log_path> is given, capture
# `docker logs` to that path before removal.
docker_stop_rm() {
	local cid="$1"
	local path="${2:-}"
	if [[ -z "$cid" ]]; then
		return 0
	fi
	if [[ -n "$path" ]]; then
		docker_dump_logs "$cid" "$path"
	fi
	docker rm -f "$cid" >/dev/null 2>&1 || true
}

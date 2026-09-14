#!/usr/bin/env bash
# Verify a scratch Juicity server image without requiring a shell in that image.
# Usage: smoke-test.sh IMAGE PLATFORM EXPECTED_VERSION
set -Eeuo pipefail

if [[ $# -ne 3 || -z ${1:-} || -z ${2:-} || -z ${3:-} ]]; then
  echo "usage: $0 IMAGE PLATFORM EXPECTED_VERSION" >&2
  exit 64
fi

image=$1
platform=$2
expected_version=$3
helper_image=${SMOKE_HELPER_IMAGE:-busybox:1.37.0}
container="juicity-smoke-$$-${RANDOM}"
workdir=$(mktemp -d "${TMPDIR:-/tmp}/juicity-docker-smoke.XXXXXX")
started=false
negative_container=

cleanup() {
  local status=$?
  trap - EXIT
  if [[ $status -ne 0 && $started == true ]]; then
    echo "--- $container logs (failure diagnostics) ---" >&2
    docker logs "$container" >&2 || true
    echo "--- socket table (failure diagnostics) ---" >&2
    socket_table >&2 || true
  fi
  if [[ -n $negative_container ]]; then
    docker rm -f "$negative_container" >/dev/null 2>&1 || true
  fi
  docker rm -f "$container" >/dev/null 2>&1 || true
  rm -rf "$workdir"
  exit "$status"
}
trap cleanup EXIT

die() {
  echo "smoke test failed: $*" >&2
  exit 1
}

socket_table() {
  docker run --rm \
    --network "container:$container" --pid "container:$container" \
    "$helper_image" sh -ec 'cat /proc/net/udp /proc/net/udp6 2>/dev/null || true'
}

wait_for_udp_listener() {
  local table
  for _ in {1..30}; do
    if [[ $(docker inspect -f '{{.State.Running}}' "$container" 2>/dev/null || true) != true ]]; then
      return 1
    fi
    table=$(socket_table 2>/dev/null || true)
    # 23182 decimal is 5A8E hexadecimal in /proc/net/{udp,udp6}.
    if [[ $table == *":5A8E"* ]]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

expect_startup_failure() {
  local description=$1
  local expected_error=$2
  local exit_code logs
  shift 2
  negative_container="${container}-negative-${RANDOM}"

  docker run -d --name "$negative_container" --platform "$platform" "$@" "$image" >/dev/null \
    || die "$description could not start its test container"
  for _ in {1..10}; do
    if [[ $(docker inspect -f '{{.State.Running}}' "$negative_container") != true ]]; then
      break
    fi
    sleep 1
  done
  if [[ $(docker inspect -f '{{.State.Running}}' "$negative_container") == true ]]; then
    docker logs "$negative_container" >&2 || true
    die "$description did not fail within 10 seconds"
  fi
  exit_code=$(docker inspect -f '{{.State.ExitCode}}' "$negative_container")
  logs=$(docker logs "$negative_container" 2>&1 || true)
  docker rm "$negative_container" >/dev/null
  negative_container=
  [[ $exit_code != 0 && $exit_code != 125 && $exit_code != 126 && $exit_code != 127 && $exit_code != 137 ]] \
    || die "$description exited with Docker/runtime status $exit_code instead of an application error"
  [[ $logs == *"$expected_error"* ]] \
    || die "$description did not report expected error '$expected_error': $logs"
}

echo "Checking $image for $platform"
version_output=$(docker run --rm --platform "$platform" "$image" --version 2>&1) \
  || die "--version exited non-zero"
printf '%s\n' "$version_output"
[[ $version_output == *"$expected_version"* ]] \
  || die "--version output does not contain expected version '$expected_version'"

# The image must reject the default config path when no configuration is mounted.
expect_startup_failure "missing configuration" "No such file or directory" \
  --read-only --cap-drop ALL --security-opt no-new-privileges

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=juicity-smoke-test' \
  -keyout "$workdir/key.pem" -out "$workdir/cert.pem" >/dev/null 2>&1
chmod 755 "$workdir"
chmod 644 "$workdir/cert.pem" "$workdir/key.pem"

printf '%s\n' \
  '{' \
  '  "listen": ":23182",' \
  '  "users": { "00000000-0000-0000-0000-000000000000": "smoke-test-password" },' \
  '  "certificate": "/etc/juicity/cert.pem",' \
  '  "private_key": "/etc/juicity/key.pem",' \
  '  "congestion_control": "bbr",' \
  '  "log_level": "info"' \
  '}' > "$workdir/server.json"
chmod 644 "$workdir/server.json"

expect_startup_failure "missing certificate" "certificate file '/etc/juicity/cert.pem' not found" \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  -v "$workdir/server.json:/etc/juicity/server.json:ro" \
  -v "$workdir/key.pem:/etc/juicity/key.pem:ro"
expect_startup_failure "missing private key" "private key file '/etc/juicity/key.pem' not found" \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  -v "$workdir/server.json:/etc/juicity/server.json:ro" \
  -v "$workdir/cert.pem:/etc/juicity/cert.pem:ro"

docker run -d --name "$container" --platform "$platform" \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  -v "$workdir/server.json:/etc/juicity/server.json:ro" \
  -v "$workdir/cert.pem:/etc/juicity/cert.pem:ro" \
  -v "$workdir/key.pem:/etc/juicity/key.pem:ro" \
  "$image" >/dev/null
started=true

wait_for_udp_listener || die "server did not create a UDP listener on port 23182"
echo "Verified UDP listener on 23182"

docker stop -t 10 "$container" >/dev/null || die "docker stop failed"
exit_code=$(docker inspect -f '{{.State.ExitCode}}' "$container")
[[ $exit_code == 0 ]] || die "SIGTERM shutdown exit code was $exit_code (expected 0, never 137)"
started=false
docker rm "$container" >/dev/null
echo "Verified graceful SIGTERM shutdown"

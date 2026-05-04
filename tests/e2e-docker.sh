#!/usr/bin/env bash
#
# clickfs end-to-end test, dockerized
# -----------------------------------
# Build a Linux image (rust:slim + libfuse) and run tests/e2e.sh inside
# it. The container reaches the host's ClickHouse via host.docker.internal
# (mapped to the host gateway). Default bridge network is used because
# OrbStack's host-network mode breaks outbound TLS to crates.io, blocking
# the in-container `cargo build`.
#
# Linux build artifacts go to ./target-linux/ to avoid clashing with any
# host (macOS) cargo build output under ./target/.
#
# See tests/README.md for full docs.

set -u
set -o pipefail

: "${CH_URL:=http://host.docker.internal:8123}"
: "${CH_USER:=default}"
: "${IMAGE_TAG:=clickfs-e2e:latest}"
: "${MOUNTPOINT_IN_CONTAINER:=/mnt/ch-e2e}"
: "${REBUILD_IMAGE:=0}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  cat <<EOF
Usage: CLICKFS_PASSWORD='...' $0 [OPTIONS] [-- e2e-args...]

Builds (or reuses) a Linux test image and runs tests/e2e.sh inside it.
Reaches ClickHouse via host.docker.internal:8123 (host-gateway).

Options:
  --rebuild         Rebuild the test docker image even if it exists.
  --image TAG       Test image tag (default: $IMAGE_TAG).
  --url URL         ClickHouse URL as seen from the container
                    (default: $CH_URL).
  --user NAME       ClickHouse user (default: $CH_USER).
  -h, --help        Show this help.

Required env:
  CLICKFS_PASSWORD  ClickHouse password.

Optional env:
  RUST_LOG          Forwarded to clickfs (default: clickfs=info).
  CARGO_HOME_HOST   Override host path mounted as /usr/local/cargo/registry.
  CARGO_GIT_HOST    Override host path mounted as /usr/local/cargo/git.

Anything after \`--\` is passed through to tests/e2e.sh.
EOF
}

EXTRA_ARGS=()
while [[ $# -gt 0 ]]; do
  case $1 in
    --rebuild)   REBUILD_IMAGE=1; shift ;;
    --image)     IMAGE_TAG="$2"; shift 2 ;;
    --url)       CH_URL="$2"; shift 2 ;;
    --user)      CH_USER="$2"; shift 2 ;;
    -h|--help)   usage; exit 0 ;;
    --)          shift; EXTRA_ARGS=("$@"); break ;;
    *) printf 'unknown option: %s\n' "$1" >&2; usage; exit 2 ;;
  esac
done

if [[ -z "${CLICKFS_PASSWORD:-}" ]]; then
  printf 'error: CLICKFS_PASSWORD must be set\n' >&2
  exit 2
fi

# --- Build image if needed -------------------------------------------------
if [[ "$REBUILD_IMAGE" == "1" ]] || ! docker image inspect "$IMAGE_TAG" >/dev/null 2>&1; then
  printf '[i] building docker image %s\n' "$IMAGE_TAG"
  docker build -t "$IMAGE_TAG" \
    -f "$REPO_ROOT/tests/Dockerfile.e2e" \
    "$REPO_ROOT/tests"
fi

# --- Cargo cache mounts (default to repo-local .cache to survive flaky net) -
: "${CARGO_HOME_HOST:=$REPO_ROOT/.cache/cargo-registry}"
: "${CARGO_GIT_HOST:=$REPO_ROOT/.cache/cargo-git}"
mkdir -p "$CARGO_HOME_HOST" "$CARGO_GIT_HOST"
CACHE_ARGS=(
  -v "$CARGO_HOME_HOST:/usr/local/cargo/registry"
  -v "$CARGO_GIT_HOST:/usr/local/cargo/git"
)

# --- Run -------------------------------------------------------------------
# Pass --build to e2e.sh so it compiles the binary inside the container,
# isolated under target-linux/ to keep host (macOS) target/ untouched.
printf '[i] running e2e in container (image=%s, ch=%s)\n' "$IMAGE_TAG" "$CH_URL"

exec docker run --rm -t \
  --add-host=host.docker.internal:host-gateway \
  --device /dev/fuse \
  --cap-add SYS_ADMIN \
  --security-opt apparmor=unconfined \
  -v "$REPO_ROOT:/work" \
  -e CH_URL="$CH_URL" \
  -e CH_USER="$CH_USER" \
  -e CLICKFS_PASSWORD="$CLICKFS_PASSWORD" \
  -e MOUNTPOINT="$MOUNTPOINT_IN_CONTAINER" \
  -e CARGO_TARGET_DIR=/work/target-linux \
  -e CLICKFS_BIN=/work/target-linux/release/clickfs \
  -e BUILD=1 \
  -e RUST_LOG="${RUST_LOG:-clickfs=info}" \
  "${CACHE_ARGS[@]+"${CACHE_ARGS[@]}"}" \
  "$IMAGE_TAG" \
  "${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}"

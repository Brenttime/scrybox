#!/bin/sh
# Build the Node-free single-binary image (Dockerfile.rust).
#
#   ./server/docker/build-rust-image.sh [tag] [docker-build args...]
#        -> scrybox-rust:<tag> (default tag: local)
#
# Thin wrapper: Dockerfile.rust is self-contained (ORT 1.27.0, static OpenCV
# 4.10, libscryjpeg and scrybox-server are all built/fetched with pinned
# sha256s inside builder stages; the PP-OCRv6 recogniser is operator-fetched
# by the server at first use since iter-64 — see the `ocr` stage comment), so
# this is just
#   docker build -f Dockerfile.rust -t scrybox-rust:<tag> .
# from a clean checkout. Extra args go to `docker build` (--no-cache,
# --platform linux/arm64, --progress=plain, ...).
#
# Optional fast path: SCRYBOX_BUILD_HOST=brenttime@brain.local ships the
# tracked files (what a clean checkout has, at their working-tree state) to
# that host, builds there with the same Dockerfile, and streams the image
# back via docker save | docker load. Same inputs, just more cores.
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TAG=${1:-local}
[ $# -gt 0 ] && shift
IMG="scrybox-rust:$TAG"
export DOCKER_BUILDKIT=1

if [ -n "${SCRYBOX_BUILD_HOST:-}" ]; then
  SSH="ssh -F /dev/null -o BatchMode=yes -i $HOME/.ssh/id_ed25519 $SCRYBOX_BUILD_HOST"
  DIR=rustbuild/image-ctx
  echo "[build-rust-image] building $IMG on $SCRYBOX_BUILD_HOST" >&2
  ( cd "$ROOT" && git ls-files -z | tar --null -T - -czf - ) \
    | $SSH "rm -rf ~/$DIR && mkdir -p ~/$DIR && tar xzf - -C ~/$DIR"
  # shellcheck disable=SC2029  # args are expanded locally on purpose
  $SSH "cd ~/$DIR && if docker info >/dev/null 2>&1; then D=docker; else D='sudo docker'; fi; \
        DOCKER_BUILDKIT=1 \$D build -f Dockerfile.rust -t $IMG $* . >&2 && \$D save $IMG" \
    | docker load
else
  cd "$ROOT"
  docker build -f Dockerfile.rust -t "$IMG" "$@" .
fi
docker image inspect "$IMG" \
  --format 'IMAGE SIZE: {{.Size}} bytes (Gate D ceiling 150 MB = 157286400)'
# Container smoke (/tmp 1777 + uid-1000 writable, server uid, health):
# SCRYBOX_SMOKE=1 runs it right after the build.
if [ "${SCRYBOX_SMOKE:-0}" = 1 ]; then
  "$ROOT/server/docker/smoke-rust-image.sh" "$IMG"
fi

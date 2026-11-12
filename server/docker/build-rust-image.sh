#!/bin/sh
# Stage the Node-free image inputs and build Dockerfile.rust (LANE B unit 7).
#
#   ./server/docker/build-rust-image.sh [tag]        # tag: scrybox-rust:<tag>, default local
#
# Staging (into server/docker/{bin,lib}/, rebuilt every run):
#   scrybox-server       — cargo build --release (remote-cargo => brain.local;
#                          SCRYBOX_LOCAL_CARGO=1 forces local)
#   libonnxruntime.so.1  — the MIT-licensed ORT dylib the Node image ships
#                          inside onnxruntime-node. Rust dlopens it
#                          (ort load-dynamic + ORT_DYLIB_PATH), so the image
#                          needs the file itself, not the npm package.
#   libscryjpeg.so       — mozjpeg-4.1.5 static shim (native/build.sh),
#                          bit-identical JPEG decode for the scan tensor path.
set -e
ROOT=$(cd "$(dirname "$0")/../.." && pwd)   # repo root (wt-b)
STAGE="$ROOT/server/docker"
TAG=${1:-local}

export PATH="$HOME/projects/scrybox-rusttest/tools/remote-cargo:$PATH"
if [ "${SCRYBOX_LOCAL_CARGO:-}" = "1" ]; then
  unset -f cargo 2>/dev/null || true
  PATH=$(echo "$PATH" | sed 's|[^:]*remote-cargo||')
fi
( cd "$ROOT/server" && cargo build --release -q )
BIN="$ROOT/server/target/release/scrybox-server"
[ -x "$BIN" ] || { echo "no release binary at $BIN"; exit 1; }

ORT=${ORT_DYLIB_PATH:-/home/brenttime/projects/bindarr/backend/node_modules/onnxruntime-node/bin/napi-v6/linux/x64/libonnxruntime.so.1}
[ -f "$ORT" ] || { echo "ORT dylib not found: $ORT (set ORT_DYLIB_PATH)"; exit 1; }
JPEG=${SCRYBOX_JPEG_LIB:-$ROOT/../data/lib/libscryjpeg.so}
[ -f "$JPEG" ] || { echo "scryjpeg shim not found: $JPEG (set SCRYBOX_JPEG_LIB)"; exit 1; }

rm -rf "$STAGE/bin" "$STAGE/lib"
mkdir -p "$STAGE/bin" "$STAGE/lib"
cp "$BIN" "$STAGE/bin/scrybox-server"
cp "$ORT" "$STAGE/lib/libonnxruntime.so.1"
cp "$JPEG" "$STAGE/lib/libscryjpeg.so"

cd "$ROOT"
docker build -f Dockerfile.rust -t "scrybox-rust:$TAG" .
docker image inspect "scrybox-rust:$TAG" \
  --format 'IMAGE SIZE: {{.Size}} bytes (Gate D ceiling 150 MB = 157286400)'

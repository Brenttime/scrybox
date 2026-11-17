#!/bin/sh
# Container smoke check for the Node-free image (Dockerfile.rust).
#
#   ./server/docker/smoke-rust-image.sh [image]   (default scrybox-rust:local)
#
# Boots the image on a throwaway empty volume and checks what a static
# inspection cannot:
#   - /tmp is 1777 and writable by the server's uid 1000 (cutfix BUG-1:
#     SQLite temp spills -> "disk I/O error" on a root 0755 /tmp);
#   - the server process really runs as uid 1000;
#   - /api/health answers.
set -eu
IMG=${1:-scrybox-rust:local}
NAME=scrybox-smoke-$$
VOL=$(mktemp -d)
cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; docker run --rm -v "$VOL":/v --entrypoint /bin/busybox "$IMG" rm -rf /v/* >/dev/null 2>&1 || true; rm -rf "$VOL"; }
trap cleanup EXIT
fail() { echo "SMOKE FAIL: $*" >&2; docker logs --tail 30 "$NAME" >&2 || true; exit 1; }

docker run -d --name "$NAME" -e RUST_JOBS= -v "$VOL":/app/database "$IMG" >/dev/null
mode=$(docker exec "$NAME" /bin/busybox stat -c %a /tmp)
[ "$mode" = 1777 ] || fail "/tmp mode is $mode, want 1777"
docker exec -u 1000 "$NAME" /bin/busybox sh -c 'echo x > /tmp/.smoke && rm /tmp/.smoke' \
  || fail "/tmp not writable by uid 1000"
i=0
until docker exec "$NAME" /bin/busybox wget -qO- http://localhost:3001/api/health >/dev/null 2>&1; do
  i=$((i + 1)); [ $i -lt 60 ] || fail "/api/health not up after 60 s"; sleep 1
done
uid=$(docker exec "$NAME" /bin/busybox sh -c 'for p in /proc/[0-9]*; do [ "$(cat $p/comm 2>/dev/null)" = scrybox-server ] && /bin/busybox stat -c %u $p && break; done')
[ "$uid" = 1000 ] || fail "scrybox-server runs as uid '$uid', want 1000"
echo "SMOKE OK: $IMG (/tmp 1777 + uid-1000 writable, server uid 1000, health up)"

#!/bin/sh
# Node-free image entrypoint (LANE B unit 7). Same contract as entrypoint.sh:
# run as root, hand any foreign-owned volume at /app/database to the
# unprivileged uid-1000 user, then gosu into the server. See entrypoint.sh's
# long comment for why the chown walk covers the whole tree, not just the top
# directory.
set -e

mkdir -p /app/database

if [ -n "$(find /app/database ! -user scrybox -print -quit)" ]; then
  echo "entrypoint: taking ownership of /app/database for scrybox user..."
  # exec-sync L5: chown -h — plain chown follows symlinks, so a planted
  # /app/database/x -> /etc/shadow (or -> the curl-impersonate binary)
  # would be re-owned to uid-1000 by root on the next boot. -h changes the
  # link's own ownership, never its target. (Node's entrypoint.sh has the
  # same pattern; per policy we do not change Node.)
  find /app/database ! -user scrybox -exec chown -h scrybox:scrybox {} +
fi

exec gosu scrybox "$@"

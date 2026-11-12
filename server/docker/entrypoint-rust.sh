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
  find /app/database ! -user scrybox -exec chown scrybox:scrybox {} +
fi

exec gosu scrybox "$@"

#!/bin/sh
# Seed the per-user parity session into the DB COPY (never prod).
# bench/parity.mjs's stats_user1 case authenticates as users.id=1 (admin) so
# the per-user stats body is compared for a user the bench token is not.
# Run this if the DB copy is refreshed from prod and parity starts failing
# stats_user1 with 401 on the Rust side and Node side alike.
set -e
DB="$(cd "$(dirname "$0")/../../.." && pwd)/data/bindarr.db"
node -e '
const { DatabaseSync } = require("node:sqlite");
const db = new DatabaseSync(process.argv[1]);
const t = "39381d59b2d9874a99c9dfe050a40942ff8bf2fe2338346d7c539def20e5c353";
db.prepare("DELETE FROM sessions WHERE token = ?").run(t);
db.prepare("INSERT INTO sessions (token, user_id, expires_at) VALUES (?, 1, ?)").run(t, "2030-01-01 00:00:00");
console.log(JSON.stringify(db.prepare("SELECT token, user_id, expires_at FROM sessions WHERE token = ?").get(t)));
' "$DB" 2>&1 | grep -v Warning

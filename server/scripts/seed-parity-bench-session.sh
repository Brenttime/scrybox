#!/bin/sh
# Re-seed the bench harness session (scen-rust.json's Authorization token)
# into the DB COPY (never prod). Sessions live 7 days (authHelpers
# generateSession); when it lapses, every authed bench/parity case drifts
# toward 401==401 false-passes — re-run this and the harness is whole again.
# Same channel as seed-parity-user1-session.sh. usage:
#   scripts/seed-parity-bench-session.sh   (BENCH_USER=brenttime by default)
set -e
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
DB="$ROOT/data/bindarr.db"
SCEN="$ROOT/scen-rust.json"
node --input-type=module -e '
import { DatabaseSync } from "node:sqlite";
import { readFileSync } from "node:fs";
const [dbPath, scenPath, user] = process.argv.slice(1);
const scen = JSON.parse(readFileSync(scenPath, "utf8"));
const auth = scen.scenarios.find((x) => x.headers?.Authorization)?.headers?.Authorization;
if (!auth) { console.error("no Authorization header in scenarios"); process.exit(1); }
const token = auth.split(" ")[1];
const db = new DatabaseSync(dbPath);
const u = db.prepare("SELECT id FROM users WHERE username = ?").get(user);
if (!u) { console.error(`user ${user} missing in DB copy`); process.exit(1); }
db.prepare("DELETE FROM sessions WHERE token = ?").run(token);
db.prepare("INSERT INTO sessions (token, user_id, expires_at) VALUES (?, ?, ?)").run(token, u.id, "2030-01-01 00:00:00");
console.log(JSON.stringify({ reseeded: true, user_id: u.id, username: user }));
' "$DB" "$SCEN" "${BENCH_USER:-brenttime}" 2>&1 | grep -v Warning

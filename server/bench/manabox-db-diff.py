#!/usr/bin/env python3
"""Lane B manabox-sync DB-diff harness (BRIEF-B: "run the Node job vs the Rust
job on two copies of the DB, then diff the affected tables").

The parity battery can only exercise the apply PRE-write arm (mb_apply_stale_
409) because a full apply is non-idempotent across the two sides sharing one
DB; the full write path was cargo-fixture-only (tests/manabox_sync.rs). This
harness is the DB-diff twin of jobs-db-diff.py for that path: it drives the
REAL POST /api/manabox-sync/apply route on both engines against isolated DB
copies.

  Node side: a throwaway container from the SAME image (scrybox:3e91010-
  headbtn) on 127.0.0.1:3313, DB_PATH=/app/database/mbx_node.db. The shared
  scrybox-rusttest-node container on :3312 is never touched.
  Rust side: target/iter/scrybox-server on 127.0.0.1:3398,
  SCRYBOX_DB_PATH=<data>/mbx_rust.db, RUST_ROUTES=manabox_backup so the apply
  is native (no proxy), job timers off.

Both copies are seeded from the shared DB (online backup) with one probe user
carrying a fixture collection + lists that drive EVERY plan arm:
  add      — CSV owns 2x (Normal/Near Mint/English), user has 1 manabox row
  redate   — that kept row's added_at differs from the ManaBox date
  remove   — (Holofoil/Damaged/Japanese) manabox row, absent from the CSV
  hand-keep— (Normal/Lightly Played/English) source=NULL row, absent too
             (must survive; also proves handAdded counting)
  list create — CSV list 'ThirdList' (no existing list of that name)
  list update — existing manabox list 'SyncTestList' with different contents
  list skipped— existing hand list 'NewList' sharing a CSV list name
Seed rows all carry quantity 1: the app invariant after server.js's boot
splitStackedEntries is one copy per row (iter-28 discovered the Rust boot
twin was missing that split — ported into migrations.rs, covered by a cargo
test there; seeding stacked rows here would only race the boot split's
CURRENT_TIMESTAMP siblings against the collection.added_at diff).
All Scryfall IDs point at the cached fixture card (trk 15,
Emergency Medical Hologram) so no upstream leg fires. Sessions are seeded
directly (fixed tokens) — no login, so no wall-clock token on either side.

Diff: collection, card_lists, list_cards (all filtered to the probe user),
manabox_sync_runs (user-filtered; created_at excluded by design, report
compared with the wall-clock header minute scrubbed like parity's MB_TS).
The HTTP responses are ALSO compared: summary must be deep-equal AND equal to
the harness's own expected summary (so a regression that "matches" by both
sides being wrong still fails), report must be byte-equal post-scrub.

Usage: python3 manabox-db-diff.py [--keep]
Exit 0 = byte-identical affected tables + responses; 1 = diffs; 2 = setup fail.
"""
import argparse
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))  # scrybox-rusttest/
DATA = os.path.join(ROOT, "data")
SRC_DB = os.path.join(DATA, "bindarr.db")
NODE_IMAGE = "scrybox:3e91010-headbtn"
NODE_CONTAINER = "scrybox-laneb-mbx-node"   # throwaway; NOT scrybox-rusttest-node
NODE_PORT = 3313
RUST_PORT = 3398
NETWORK = "bindarr-host_default"            # same network as the main containers

CARD = "mtg-dda6f965-cbdf-4dbc-b0b1-00c27e32f5f2"  # cached fixture (trk 15)
UUID = CARD[len("mtg-"):]
USER = "mbx_probe"
# 64-hex fixed session tokens (shape parity-mjs asserts for issued tokens).
TOK_NODE, TOK_RUST = "a1" * 32, "b2" * 32
SESSION_EXP = "2030-01-01 00:00:00"

MB_HDR = ("Binder Name,Binder Type,Name,Set code,Collector number,Foil,"
          "Quantity,Scryfall ID,Condition,Language,Added")
MB_CSV = "\n".join([
    MB_HDR,
    f"SyncTest,binder,Emergency Medical Hologram,trk,15,normal,1,{UUID},mint,en,2024-01-02T03:04:05.000Z",
    f"SyncTest2,binder,Emergency Medical Hologram,trk,15,normal,1,{UUID},mint,en,2024-01-03T04:05:06.000Z",
    f"SyncTestList,list,Emergency Medical Hologram,trk,15,normal,1,{UUID},mint,en,2024-01-01T00:00:00.000Z",
    f"NewList,list,Emergency Medical Hologram,trk,15,normal,2,{UUID},mint,en,2024-01-01T00:00:00.000Z",
    f"ThirdList,list,Emergency Medical Hologram,trk,15,foil,3,{UUID},mint,en,2024-01-01T00:00:00.000Z",
]) + "\n"

# What the plan MUST look like (arms above). A match-both-sides-wrong run
# still fails on this.
EXPECTED_SUMMARY = {
    "csvRows": 5, "owned": 2, "before": 3, "after": 3,
    "add": 1, "remove": 1, "redate": 1, "handAdded": 1,
    "listsCreate": 1, "listsUpdate": 1, "listsSkipped": ["NewList"],
    "rejected": 0,
}

# Wall-clock columns excluded by design; report scrub mirrors parity MB_TS.
SKIP = {
    "manabox_sync_runs": {"created_at"},
    "card_lists": {"created_at"},
}
SCRUB = {
    "manabox_sync_runs": {"report": lambda s: re.sub(
        r"# ManaBox sync \d{4}-\d{2}-\d{2} \d{2}:\d{2} UTC",
        "# ManaBox sync TS UTC", s or "")},
}
RESP_SCRUB = lambda s: re.sub(r"# ManaBox sync \d{4}-\d{2}-\d{2} \d{2}:\d{2} UTC",
                              "# ManaBox sync TS UTC", s or "")


def sh(*cmd, env=None, check=True):
    r = subprocess.run(cmd, capture_output=True, text=True, env=env)
    if check and r.returncode != 0:
        print(f"FAIL: {' '.join(cmd)}\n{r.stdout}\n{r.stderr}", file=sys.stderr)
        sys.exit(2)
    return r.stdout


def http_json(port, method, path, token=None, body=None, timeout=30):
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", method=method)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        req.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(req, data=data, timeout=timeout) as r:
            return r.status, json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, {"raw": raw}


def wait_ready(port, token, label, deadline=90):
    """Poll the NATIVE runs route — proves both listen AND the manabox module."""
    t0 = time.time()
    while time.time() - t0 < deadline:
        try:
            st, js = http_json(port, "GET", "/api/manabox-sync/runs", token=token, timeout=5)
            if st == 200 and isinstance(js, list):
                return
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            pass
        time.sleep(1)
    print(f"{label} did not become ready on :{port} within {deadline}s", file=sys.stderr)
    sys.exit(2)


def seed(path):
    src = sqlite3.connect(SRC_DB)
    dst = sqlite3.connect(path)
    src.backup(dst)
    src.close()
    c = dst
    c.execute("PRAGMA journal_mode=WAL")
    if not c.execute("SELECT 1 FROM card_cache WHERE id=?", (CARD,)).fetchone():
        print(f"fixture card {CARD} not in card_cache — seed DB unusable", file=sys.stderr)
        sys.exit(2)
    c.execute("DELETE FROM users WHERE username = ?", (USER,))
    # Deterministic baseline hash (same on both copies; never logged in with —
    # sessions are seeded directly).
    import hashlib
    salt = "5273745061726974792d30303030"
    h = hashlib.pbkdf2_hmac("sha512", b"MbxProbe!0000", bytes.fromhex(salt), 210000, 64).hex()
    # Dedicated deterministic share_token (rusttest_parity owns the classic
    # fixture token; users.share_token is UNIQUE).
    c.execute("INSERT INTO users (username, password_hash, role, share_token, share_enabled, api_key) "
              "VALUES (?, ?, 'member', ?, 0, NULL)", (USER, f"210000:{salt}:{h}", "4d425850524f42450000000000000000"))
    uid = c.execute("SELECT id FROM users WHERE username = ?", (USER,)).fetchone()[0]
    c.execute("DELETE FROM collection WHERE user_id = ?", (uid,))
    # (printing, condition, language, qty, source, added_at) — qty 1 per row
    # (app invariant after boot splitStackedEntries; see header).
    for pr, cd, lg, qty, src_, ad in [
        ("Normal", "Near Mint", "English", 1, "manabox", "2023-05-01 00:00:00"),   # keep+redate
        ("Holofoil", "Damaged", "Japanese", 1, "manabox", "2022-02-02 00:00:00"),  # remove
        ("Normal", "Lightly Played", "English", 1, None, "2021-03-03 00:00:00"),   # hand-keep
    ]:
        c.execute("INSERT INTO collection (card_id, quantity, condition, printing, language, "
                  "purchase_price, added_at, user_id, source) VALUES (?,?,?,?,?,?,?,?,"
                  "CASE ? WHEN 'manabox' THEN 'manabox' ELSE NULL END)",
                  (CARD, qty, cd, pr, lg, None, ad, uid, src_))
    c.execute("DELETE FROM list_cards WHERE list_id IN (SELECT id FROM card_lists WHERE user_id = ?)", (uid,))
    c.execute("DELETE FROM card_lists WHERE user_id = ?", (uid,))
    c.execute("INSERT INTO card_lists (user_id, name, description, source) VALUES (?, 'SyncTestList', 'Synced from ManaBox', 'manabox')", (uid,))
    lid_up = c.execute("SELECT last_insert_rowid()").fetchone()[0]
    c.execute("INSERT INTO card_lists (user_id, name, description, source) VALUES (?, 'NewList', 'Hand list', NULL)", (uid,))
    lid_skip = c.execute("SELECT last_insert_rowid()").fetchone()[0]
    c.execute("INSERT INTO list_cards (list_id, card_id, quantity) VALUES (?,?,9)", (lid_up, CARD))
    c.execute("INSERT INTO list_cards (list_id, card_id, quantity) VALUES (?,?,5)", (lid_skip, CARD))
    c.execute("DELETE FROM manabox_sync_runs WHERE user_id = ?", (uid,))
    c.execute("INSERT INTO sessions (token, user_id, expires_at) VALUES (?,?,?)",
              (TOK_NODE, uid, SESSION_EXP))
    c.execute("INSERT INTO sessions (token, user_id, expires_at) VALUES (?,?,?)",
              (TOK_RUST, uid, SESSION_EXP))
    c.commit()
    c.close()


def start_node(db_rel):
    sh("docker", "rm", "-f", NODE_CONTAINER, check=False)
    sh("docker", "run", "-d", "--name", NODE_CONTAINER, "--network", NETWORK,
       "-p", f"127.0.0.1:{NODE_PORT}:3001", "-v", f"{DATA}:/app/database",
       "-e", "NODE_ENV=production", "-e", "PORT=3001",
       "-e", f"DB_PATH=/app/database/{db_rel}",
       "-e", "CATALOG_AUTO_UPDATE=0",
       "-e", "SCRYBOX_DISABLE_SETS_REFRESH=true",
       "-e", "SCRYBOX_DISABLE_PRICE_SWEEP=true",
       "-e", "SCRYBOX_SKIP_DB_INIT=true",
       "-e", "LOCAL_SCRYFALL_ENABLED=true",
       "-e", "LOCAL_SCRYFALL_URL=http://scryfall-local:8420",
       NODE_IMAGE)


def rust_binary():
    for cand in (os.path.join(HERE, "..", "target", "iter", "scrybox-server"),
                 os.path.join(HERE, "..", "target", "release", "scrybox-server")):
        if os.path.exists(cand):
            return os.path.abspath(cand)
    print("no scrybox-server binary (build --profile iter or --release first)", file=sys.stderr)
    sys.exit(2)


def start_rust(db_abs, sidecar_url):
    env = dict(os.environ,
               SCRYBOX_BIND=f"127.0.0.1:{RUST_PORT}",
               SCRYBOX_NODE_UPSTREAM="http://127.0.0.1:3312",
               SCRYBOX_DB_PATH=db_abs,
               RUST_ROUTES="manabox_backup",
               SCRYBOX_DISABLE_MOXFIELD_SCHEDULER="1",
               SCRYBOX_DISABLE_AUTO_BACKUP="1",
               SCRYBOX_STATS_PREWARM="0",
               LOCAL_SCRYFALL_ENABLED="true",
               LOCAL_SCRYFALL_URL=sidecar_url)
    env.pop("SCRYBOX_STATIC_DIR", None)
    env.pop("RUST_JOBS", None)
    log = open(os.path.join(DATA, "mbx_rust_server.log"), "w")
    return subprocess.Popen([rust_binary()], env=env, stdout=log, stderr=subprocess.STDOUT)


def sidecar_url():
    url = os.environ.get("SCRY_HARNESS_SIDECAR_URL", "").strip()
    if url:
        return url
    out = sh("docker", "inspect", "-f",
             "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
             "scryfall-local").strip().splitlines()
    if not out or not out[0]:
        print("scryfall-local container not found; set SCRY_HARNESS_SIDECAR_URL", file=sys.stderr)
        sys.exit(2)
    return f"http://{out[0]}:8420"


def quote(x):
    return '"' + x.replace('"', '""') + '"'


def user_rows(path, table, uid):
    c = sqlite3.connect(path)
    cols = [r[1] for r in c.execute(f"PRAGMA table_info({table})")]
    keep_cols = [x for x in cols if x not in SKIP.get(table, set())]
    scrub = SCRUB.get(table, {})
    if table == "list_cards":
        where = (f"WHERE list_id IN (SELECT id FROM card_lists WHERE user_id={uid})")
    else:
        where = f"WHERE user_id={uid}"
    sql = f"SELECT {','.join(quote(x) for x in keep_cols)} FROM {table} {where}"
    rows = []
    for r in c.execute(sql):
        r = list(r)
        for i, col in enumerate(keep_cols):
            if col in scrub and isinstance(r[i], str):
                r[i] = scrub[col](r[i])
        rows.append(tuple(r))
    c.close()
    rows.sort(key=lambda r: tuple(("" if v is None else str(v)) for v in r))
    return keep_cols, rows


def diff(a, b, table, uid, limit=5):
    ca, ra = user_rows(a, table, uid)
    cb, rb = user_rows(b, table, uid)
    out = []
    if ca != cb:
        return [f"column set differs: {ca} vs {cb}"]
    shown = 0
    for i in range(max(len(ra), len(rb))):
        x = ra[i] if i < len(ra) else None
        y = rb[i] if i < len(rb) else None
        if x != y:
            shown += 1
            if shown <= limit:
                out.append(f"row {i}: {x}  VS  {y}")
    if shown > limit:
        out.append(f"... and {shown - limit} more differing rows")
    if len(ra) != len(rb):
        out.insert(0, f"row count {len(ra)} vs {len(rb)}")
    return out


def apply(port, token):
    st, js = http_json(port, "POST", "/api/manabox-sync/apply", token=token,
                       body={"csv": MB_CSV, "fileName": "harness.csv"})
    if st != 200:
        print(f"apply on :{port} -> HTTP {st}: {js}", file=sys.stderr)
        sys.exit(2)
    return js


def cleanup(keep, procs):
    for p in procs:
        try:
            p.terminate()
            p.wait(timeout=10)
        except Exception:
            try:
                p.kill()
            except Exception:
                pass
    if not keep:
        sh("docker", "rm", "-f", NODE_CONTAINER, check=False)
        for name in ("mbxseed.db", "mbx_node.db", "mbx_rust.db", "mbx_rust_server.log"):
            for suf in ("", "-wal", "-shm"):
                try:
                    os.remove(os.path.join(DATA, name + suf))
                except FileNotFoundError:
                    pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", action="store_true", help="keep DB copies + server log")
    ap.add_argument("--no-negative-control", action="store_true",
                    help="skip the poison-the-Rust-copy self-check (NOT for gate runs)")
    args = ap.parse_args()

    procs = []
    for name in ("mbxseed.db", "mbx_node.db", "mbx_rust.db"):
        for suf in ("", "-wal", "-shm"):
            try:
                os.remove(os.path.join(DATA, name + suf))
            except FileNotFoundError:
                pass
    seed_path = os.path.join(DATA, "mbxseed.db")
    ndb = os.path.join(DATA, "mbx_node.db")
    rdb = os.path.join(DATA, "mbx_rust.db")

    try:
        seed(seed_path)
        shutil.copy(seed_path, ndb)
        shutil.copy(seed_path, rdb)
        uid = sqlite3.connect(seed_path).execute(
            "SELECT id FROM users WHERE username=?", (USER,)).fetchone()[0]
        print(f"seed: probe user id={uid}, collection 3 rows, lists 2, runs 0")

        start_node("mbx_node.db")
        wait_ready(NODE_PORT, TOK_NODE, "node (throwaway)")
        rproc = start_rust(rdb, sidecar_url())
        procs.append(rproc)
        wait_ready(RUST_PORT, TOK_RUST, "rust (throwaway)")

        # Back-to-back on purpose: nothing here should depend on upstream
        # (fixture card is cached), but keep the ordering discipline.
        nres = apply(NODE_PORT, TOK_NODE)
        rres = apply(RUST_PORT, TOK_RUST)

        ok = True
        # 1. Response equality + arm coverage vs the harness's own expectation.
        for side, res in (("node", nres), ("rust", rres)):
            got = res.get("summary")
            if got != EXPECTED_SUMMARY:
                ok = False
                print(f"{side}: summary {got} != expected {EXPECTED_SUMMARY}")
        if RESP_SCRUB(nres.get("report")) != RESP_SCRUB(rres.get("report")):
            ok = False
            print("report differs post-scrub:\nNODE:\n", nres.get("report"),
                  "\nRUST:\n", rres.get("report"))
        print("responses:", "MATCH (and match expected summary)" if ok else "DIFF")

        # 2. Affected-table equality (user-filtered).
        for t in ("collection", "card_lists", "list_cards", "manabox_sync_runs"):
            d = diff(ndb, rdb, t, uid)
            print(f"{t}: {'MATCH' if not d else 'DIFF'}")
            for line in d:
                ok = False
                print("   ", line)

        # 3. Both sides actually stamped a run row (the diff would MATCH on 0=0).
        for side, p in (("node", ndb), ("rust", rdb)):
            conn = sqlite3.connect(p)
            n = conn.execute(
                "SELECT COUNT(*) FROM manabox_sync_runs WHERE user_id=?", (uid,)).fetchone()[0]
            # Positive-write guards (iter-29 review gate): require each plan
            # arm to have ACTUALLY written — summary numbers alone could in
            # principle be echoed by a no-op engine; the tables must show it.
            n_coll = conn.execute(
                "SELECT COUNT(*) FROM collection WHERE user_id=?", (uid,)).fetchone()[0]
            n_holo = conn.execute(
                "SELECT COUNT(*) FROM collection WHERE user_id=? AND printing='Holofoil'",
                (uid,)).fetchone()[0]
            hand = conn.execute(
                "SELECT quantity FROM collection WHERE user_id=? AND condition='Lightly Played'",
                (uid,)).fetchone()
            redated = conn.execute(
                "SELECT added_at FROM collection WHERE user_id=? AND condition='Near Mint'",
                (uid,)).fetchone()
            n_lists = conn.execute(
                "SELECT COUNT(*) FROM card_lists WHERE user_id=?", (uid,)).fetchone()[0]
            n_lc = conn.execute(
                "SELECT COUNT(*) FROM list_cards WHERE list_id IN "
                "(SELECT id FROM card_lists WHERE user_id=?)", (uid,)).fetchone()[0]
            conn.close()
            print(f"{side}: runs={n} collection={n_coll} holofoil-removed={1 - n_holo} "
                  f"hand={hand} redated={redated[0] if redated else None} "
                  f"lists={n_lists} list_cards={n_lc}")
            if n != 1:
                ok = False
            if n_coll != 3 or n_holo != 0 or hand != (1,) or n_lists != 3 or n_lc != 3:
                ok = False
            if not redated or redated[0] == "2023-05-01 00:00:00":
                ok = False  # redate arm did not move the kept row's date

        # 4. Negative controls (iter-29 review gate): poison the Rust copy
        # with one targeted mutation per compared table and require EVERY
        # compared table to report a DIFF. Proves the table_rows/diff
        # machinery can actually fail on each write path — a harness that
        # cannot fail gates nothing. (The response-vs-EXPECTED arm is
        # self-controlled: EXPECTED_SUMMARY is harness-owned, so both-sides-
        # wrong already fails.)
        if not args.no_negative_control:
            lc_poison = (
                "UPDATE list_cards SET quantity = quantity + 1 WHERE card_id=? AND list_id = "
                "(SELECT id FROM card_lists WHERE user_id=? AND name='ThirdList')", (CARD, uid))
            poisons = [
                ("collection", "UPDATE collection SET quantity = quantity + 5 "
                               "WHERE user_id=? AND condition='Near Mint'", (uid,)),
                ("list_cards", *lc_poison),
                ("card_lists", "UPDATE card_lists SET name = name || '!' "
                               "WHERE user_id=? AND name='ThirdList'", (uid,)),
                ("manabox_sync_runs", "UPDATE manabox_sync_runs SET report = report || 'X' "
                                      "WHERE user_id=?", (uid,)),
            ]
            pc = sqlite3.connect(rdb)
            for _, sql, prm in poisons:
                pc.execute(sql, prm)
            pc.commit()
            pc.close()
            for t, _, _ in poisons:
                d = diff(ndb, rdb, t, uid)
                fired = bool(d)
                print(f"negative-control {t}: {'DETECTED' if fired else 'UNDETECTED'}")
                if not fired:
                    ok = False

        if args.keep:
            print(f"kept: {ndb} {rdb} {os.path.join(DATA, 'mbx_rust_server.log')}")
        print("VERDICT:", "PASS" if ok else "FAIL")
        sys.exit(0 if ok else 1)
    finally:
        cleanup(args.keep, procs)


if __name__ == "__main__":
    main()

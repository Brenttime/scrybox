#!/usr/bin/env python3
"""Lane B jobs DB-diff harness (BRIEF-B: "run the Node job vs the Rust job on
two copies of the DB, then diff the affected tables").

Flow:
  1. Online-backup the shared DB copy (../../data/bindarr.db) to a seed, then
     prune it to a deterministic sweep target set: keep collection rows for the
     first N distinct owned cards, drop the rest of the collection and all
     deck_cards (the sweep query unions in deck cards), and empty `sets` and
     `price_history` so both jobs execute their FULL write paths.
  2. Copy the seed twice. Node side: docker exec into scrybox-rusttest-node
     (sidecar env already in the container) and drive the real
     backend/src/scryfallApi.js fetchAndCacheSets(true) + updateCollectionPrices(true)
     against /app/database/<node>.db.
     Rust side: target/release/jobs (scrybox-jobs) sets-sync --force and
     price-sweep --force against the host-visible path. Same sidecar
     (LOCAL_SCRYFALL_*), same upstream /cards/collection leg, back-to-back
     runs so live Scryfall cannot change price mid-comparison.
  3. Diff the affected tables, excluding the by-design wall-clock columns
     (card_cache.last_updated, price_history.recorded_at, and
     app_settings.mtg_prices_swept_at — the latter is separately asserted to
     have been written on BOTH sides).

Review-gate hardening (lane-B iter-29, orchestrator 13:03 ruling):
  * prune now SEEDS a deterministic deck (decks + deck_cards rows) instead of
    dropping all deck_cards. The sweep query (scry_jobs.rs SWEEP_SQL and
    refresh_price_estimates' unpriced check, scryfallApi.js twin) unions in
    every cached printing of a logical card used by a deck; with deck_cards
    empty that arm was dead on both sides — a twin that dropped the UNION
    would have PASSed forever. Deck names are chosen cache-wide with <=2
    printings and NOT owned, so the deck arm must add sweep rows beyond the
    collection arm (asserted by the deck-arm-rows guard).
  * Positive-write guards: both copies must end with sets > 0,
    price_history > 0 and price_history rows for cards nobody owns
    (deck-arm-only rows). Empty-vs-empty MATCHes are no longer a PASS.
  * Negative controls (--no-negative-control to skip): after the honest PASS,
    poison the Rust copy (one targeted mutation per compared table) and
    require EVERY compared table to report a DIFF. A harness that cannot
    fail cannot gate. Exit 0 requires verdict PASS AND every control fired.

Usage: python3 jobs-db-diff.py [--keep] [--cards 225] [--no-negative-control]
Exit 0 = byte-identical affected tables AND negative controls all detected;
1 = diffs (samples printed) or a control went undetected; 2 = setup failure.
"""
import argparse
import json
import os
import shutil
import sqlite3
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))  # scrybox-rusttest/
DATA = os.path.join(ROOT, "data")
SRC_DB = os.path.join(DATA, "bindarr.db")
JOBS_BIN = os.path.join(HERE, "..", "target", "release", "jobs")
NODE_CONTAINER = "scrybox-rusttest-node"

# Wall-clock / run-time columns excluded from the diff by design.
SKIP = {
    "card_cache": {"last_updated"},
    "price_history": {"recorded_at"},
    "app_settings": {"mtg_prices_swept_at"},
}
TABLES = ["sets", "card_cache", "price_history", "app_settings"]


def sh(*cmd, env=None, check=True):
    r = subprocess.run(cmd, capture_output=True, text=True, env=env)
    if check and r.returncode != 0:
        print(f"FAIL: {' '.join(cmd)}\n{r.stdout}\n{r.stderr}", file=sys.stderr)
        sys.exit(2)
    return r.stdout


def backup(dst):
    src = sqlite3.connect(SRC_DB)
    dstc = sqlite3.connect(dst)
    src.backup(dstc)
    src.close()
    dstc.close()


def prune(path, keep_cards):
    c = sqlite3.connect(path)
    c.execute("PRAGMA journal_mode=WAL")
    keep = [r[0] for r in c.execute(
        "SELECT DISTINCT card_id FROM collection WHERE quantity > 0 ORDER BY card_id LIMIT ?",
        (keep_cards,))]
    if len(keep) < 1:
        print("no owned cards — nothing to sweep", file=sys.stderr)
        sys.exit(2)
    qmarks = ",".join("?" * len(keep))
    c.execute(f"DELETE FROM collection WHERE card_id NOT IN ({qmarks})", keep)
    # Review-gate fix (lane-B iter-29): do NOT leave deck_cards empty. The
    # sweep query unions in every cached printing whose logical key matches a
    # DECK-used card (scry_jobs.rs SWEEP_SQL, scryfallApi.js twin), and
    # refresh_price_estimates adds `UNION SELECT card_id FROM deck_cards
    # WHERE quantity > 0`. With deck_cards pruned to zero that arm was dead
    # on BOTH sides — a twin dropping the UNION would PASS forever. Seed one
    # deterministic deck: names with 2..4 cached printings, none owned, so
    # the deck arm must sweep sibling printings nobody owns (asserted by the
    # deck-only-rows guard in main).
    c.execute("DELETE FROM deck_cards")
    c.execute("DELETE FROM decks")
    names = [r[0] for r in c.execute(
        """SELECT name FROM card_cache
            WHERE name IS NOT NULL
              AND LOWER(TRIM(name)) NOT IN (
                SELECT DISTINCT LOWER(TRIM(cc.name)) FROM collection col
                JOIN card_cache cc ON cc.id = col.card_id
                WHERE col.quantity > 0 AND cc.name IS NOT NULL)
            GROUP BY LOWER(TRIM(name))
            HAVING COUNT(*) BETWEEN 2 AND 4
            ORDER BY LOWER(TRIM(name)) LIMIT 8""")]
    deck_rows = 0
    if names:
        c.execute("INSERT INTO decks (id, user_id, name, source) VALUES (90042, 42, 'dbdiff-deck', 'manual')")
        for i, nm in enumerate(names):
            row = c.execute(
                "SELECT id FROM card_cache WHERE LOWER(TRIM(name))=LOWER(TRIM(?)) ORDER BY id LIMIT 1",
                (nm,)).fetchone()
            if row:
                c.execute("INSERT OR IGNORE INTO deck_cards (deck_id, card_id, quantity) VALUES (90042, ?, ?)",
                          (row[0], i + 1))
                deck_rows += 1
    if deck_rows == 0:
        print("prune: no eligible deck cards seeded — deck-arm coverage lost", file=sys.stderr)
        sys.exit(2)
    c.execute("DELETE FROM sets")
    c.execute("DELETE FROM price_history")
    # Deterministic sweep gate: interval 1 day + NULL swept_at = due now.
    c.execute("UPDATE app_settings SET price_refresh_days = 1, mtg_prices_swept_at = NULL")
    c.commit()
    # Owned card_cache rows must exist (sweep joins them); report if not.
    missing = c.execute(
        "SELECT COUNT(*) FROM collection c LEFT JOIN card_cache cc ON cc.id=c.card_id WHERE cc.id IS NULL AND c.quantity>0 AND c.card_id LIKE 'mtg-%'",
    ).fetchone()[0]
    rows = c.execute(f"SELECT COUNT(*) FROM collection WHERE quantity>0").fetchone()[0]
    c.close()
    return len(keep), rows, missing


def run_node(db_rel, job):
    js = {
        "sets": "a.fetchAndCacheSets(true).then(n=>{console.log(JSON.stringify({job:'sets',sets:n??'ok'}));process.exit(0)}).catch(e=>{console.error(String(e));process.exit(1)})",
        # Full price_sweep job, not just the collection leg: Node's boot
        # catch-up (server.js ~352-366) runs updateCollectionPrices() and then
        # await refreshPriceEstimates(), and the Rust jobs binary chains
        # refresh_price_estimates inside price-sweep (scry_jobs.rs). Running
        # only updateCollectionPrices left the estimates arms (fill_missing_
        # floors + refresh_all_estimates -> card_cache.price_estimated etc)
        # compared Rust-wrote vs Node-unrun. lane-B iter-28.
        "sweep": "a.updateCollectionPrices(true).then(()=>a.refreshPriceEstimates()).then(()=>{console.log(JSON.stringify({job:'sweep',done:true}));process.exit(0)}).catch(e=>{console.error(String(e));process.exit(1)})",
    }[job]
    out = sh("docker", "exec", "-e", f"DB_PATH=/app/database/{db_rel}", NODE_CONTAINER,
             "node", "-e", f"const a=require('/app/backend/src/scryfallApi'); {js}")
    return out.strip().splitlines()[-1] if out.strip() else "{}"


def sidecar_url():
    """Same resolution as restart-rust-b.sh: explicit override, else docker inspect."""
    url = os.environ.get("SCRY_HARNESS_SIDECAR_URL", "").strip()
    if url:
        return url
    out = sh("docker", "inspect", "-f",
             "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
             "scryfall-local").strip().splitlines()
    if not out or not out[0]:
        print("scryfall-local container not found; set SCRY_HARNESS_SIDECAR_URL",
              file=sys.stderr)
        sys.exit(2)
    return f"http://{out[0]}:8420"


def run_rust(db_abs, job):
    env = dict(os.environ,
               SCRYBOX_DB_PATH=db_abs,
               LOCAL_SCRYFALL_ENABLED="true",
               LOCAL_SCRYFALL_URL=sidecar_url())
    args = [JOBS_BIN] + (["sets-sync", "--force"] if job == "sets" else ["price-sweep", "--force"])
    return sh(*args, env=env).strip().splitlines()[-1]


def table_rows(path, table):
    c = sqlite3.connect(path)
    cols = [r[1] for r in c.execute(f"PRAGMA table_info({table})")]
    keep_cols = [x for x in cols if x not in SKIP.get(table, set())]
    sql = f"SELECT {','.join(quote(x) for x in keep_cols)} FROM {table}"
    # Sort in Python (multiset compare) — recorded_at-style ties between
    # rows with equal compared columns stay deterministic this way.
    rows = sorted(c.execute(sql).fetchall(), key=lambda r: tuple(("" if v is None else str(v)) for v in r))
    c.close()
    return keep_cols, rows


def quote(x):
    return '"' + x.replace('"', '""') + '"'


def diff(a, b, table, limit=5):
    ca, ra = table_rows(a, table)
    cb, rb = table_rows(b, table)
    out = []
    if ca != cb:
        out.append(f"column set differs: {ca} vs {cb}")
        return out
    n = max(len(ra), len(rb))
    shown = 0
    for i in range(n):
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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", action="store_true", help="keep the DB copies for inspection")
    ap.add_argument("--cards", type=int, default=225, help="distinct owned cards to sweep (75/batch)")
    ap.add_argument("--no-negative-control", action="store_true",
                    help="skip the poison-the-Rust-copy self-check (NOT for gate runs)")
    args = ap.parse_args()

    if not os.path.exists(JOBS_BIN):
        print(f"build the harness binary first: (cd server && cargo build --release) -> {JOBS_BIN}", file=sys.stderr)
        sys.exit(2)

    seed = os.path.join(DATA, "jobseed.db")
    ndb, rdb = os.path.join(DATA, "jobnode.db"), os.path.join(DATA, "jobrust.db")
    for p in (seed, ndb, rdb):
        for suf in ("", "-wal", "-shm"):
            try: os.remove(p + suf)
            except FileNotFoundError: pass
    backup(seed)
    n_cards, n_rows, n_missing = prune(seed, args.cards)
    print(f"seed: {n_cards} distinct owned cards, {n_rows} collection rows "
          f"({n_missing} owned ids missing from card_cache)")
    shutil.copy(seed, ndb)
    shutil.copy(seed, rdb)

    print("sets-sync:", run_node("jobnode.db", "sets"))
    print("sets-sync:", run_rust(rdb, "sets"))
    # Back-to-back so live Scryfall prices cannot change between the two runs.
    print("sweep    :", run_node("jobnode.db", "sweep"))
    print("sweep    :", run_rust(rdb, "sweep"))

    ok = True
    for t in TABLES:
        d = diff(ndb, rdb, t)
        print(f"{t}: {'MATCH' if not d else 'DIFF'}")
        for line in d:
            ok = False
            print("   ", line)
    # Both sides must have stamped the sweep checkpoint (the excluded column).
    for side, p in (("node", ndb), ("rust", rdb)):
        conn = sqlite3.connect(p)
        v = conn.execute(
            "SELECT mtg_prices_swept_at FROM app_settings WHERE id=1").fetchone()[0]
        # Positive-write guards (iter-29 review gate): an emptied-and-refetched
        # table that stayed empty on BOTH sides would diff as MATCH. Require
        # the full write paths to have produced rows: sets repopulated,
        # price_history written, and at least one price_history row for a
        # card nobody owns — proof the deck-union arm of the sweep ran and
        # priced sibling printings (see the deck seeding in prune()).
        n_sets = conn.execute("SELECT COUNT(*) FROM sets").fetchone()[0]
        n_ph = conn.execute("SELECT COUNT(*) FROM price_history").fetchone()[0]
        n_deck = conn.execute(
            "SELECT COUNT(*) FROM price_history WHERE card_id NOT IN "
            "(SELECT card_id FROM collection)").fetchone()[0]
        conn.close()
        print(f"{side}: mtg_prices_swept_at = {v!r}, sets={n_sets}, "
              f"price_history={n_ph}, deck-only-priced={n_deck}")
        if not v or n_sets == 0 or n_ph == 0 or n_deck == 0:
            ok = False

    # Negative controls (iter-29 review gate): poison the Rust copy with one
    # targeted mutation per compared table; EVERY table must then report a
    # DIFF. Proves the diff machinery can fail — a harness that never fails
    # gates nothing. Skippable only for debugging via --no-negative-control.
    if ok and not args.no_negative_control:
        poisons = {
            "sets": "UPDATE sets SET name = name || '!' WHERE set_code = "
                    "(SELECT set_code FROM sets ORDER BY id LIMIT 1)",
            "card_cache": "UPDATE card_cache SET price_normal = COALESCE(price_normal, 0) + 1 WHERE id = "
                          "(SELECT card_id FROM collection ORDER BY card_id LIMIT 1)",
            "price_history": "UPDATE price_history SET price = price + 1 WHERE card_id = "
                             "(SELECT card_id FROM price_history ORDER BY card_id LIMIT 1)",
            "app_settings": "UPDATE app_settings SET price_refresh_days = "
                            "COALESCE(price_refresh_days, 0) + 1 WHERE id = 1",
        }
        pc = sqlite3.connect(rdb)
        for t, sql in poisons.items():
            pc.execute(sql)
        pc.commit()
        pc.close()
        for t in TABLES:
            d = diff(ndb, rdb, t, limit=1)
            fired = bool(d)
            print(f"negative-control {t}: {'DETECTED' if fired else 'UNDETECTED'}")
            if not fired:
                ok = False

    if not args.keep:
        for p in (seed, ndb, rdb):
            for suf in ("", "-wal", "-shm"):
                try: os.remove(p + suf)
                except FileNotFoundError: pass
    else:
        print(f"kept: {ndb} {rdb}")
    print("VERDICT:", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()

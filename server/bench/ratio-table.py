#!/usr/bin/env python3
"""Ratio table generator (loop tooling, iter 14).

Reads one or more http-bench.mjs JSON outputs and prints the PROGRESS.md
table: rps / p50 / p99 per route against the Node baseline, with x-factors.
The 10x gate is rps AND p50; the 20x stretch gate is the same at 20. p99,
p999 and max are printed for honesty, never gated.

usage: ratio-table.py [run.json ...]     (reads the `results` array; hot and
       realistic files both work — route names are matched by prefix)
"""
import json, sys

BASE = {  # baseline (old Node) rps / p50 / p99 — BRIEF.md
    "collection_page": ("collection_page100", 49.5, 312, 487),
    "collection_full": ("collection_full_27MB", 0.8, 2378, 2535),
    "stats": ("stats", 1054, 14, 24),
    "search_coll_t": ("search_coll_t_creature_c_g", 40, 386, 504),
    "search_coll_sol": ("search_coll_sol_ring", 207, 75, 120),
    "search_db": ("search_db_name_sol_ring", 11, 1386, 1636),
}

def baseline(name):
    # longest-prefix wins so search_coll_t_creature... does not match search_coll_sol
    for pref in sorted(BASE, key=len, reverse=True):
        if name.startswith(pref):
            return BASE[pref]
    return None

def main(files):
    for f in files:
        d = json.load(open(f))
        print(f"## {f}")
        if d.get("write_mix"):
            print(f"write-mix: every {d['write_mix']['every_ms']} ms, "
                  f"ticks={d['write_mix']['ticks']}, errors={d['write_mix']['errors']}")
        print("| route | baseline rps/p50/p99 | rust rps/p50/p99 | x(rps)/x(p50)/x(p99) |")
        print("|---|---|---|---|")
        worst_rps, worst_p50 = 9e9, 9e9
        for r in d["results"]:
            b = baseline(r["name"])
            extra = f" p999={r.get('p999_ms')} max={r.get('max_ms')}" if r.get("p999_ms") else ""
            rss = f" rss={r.get('peak_rss_mb')}" if r.get("peak_rss_mb") else ""
            if not b:
                print(f"| {r['name']} | — | {r['rps']} / {r['p50_ms']} / {r['p99_ms']}{extra}{rss} | — |")
                continue
            _, br, bp, bq = b
            xr, xp, xq = r["rps"] / br, bp / r["p50_ms"], bq / r["p99_ms"]
            worst_rps, worst_p50 = min(worst_rps, xr), min(worst_p50, xp)
            print(f"| {r['name']} | {br} / {bp} / {bq} | {r['rps']} / {r['p50_ms']} / {r['p99_ms']}{extra}{rss} "
                  f"| {xr:.1f}x / {xp:.1f}x / {xq:.1f}x |")
        if worst_rps < 9e9:
            gate10 = "PASS" if worst_rps >= 10 and worst_p50 >= 10 else "FAIL"
            gate20 = "PASS" if worst_rps >= 20 and worst_p50 >= 20 else "FAIL"
            print(f"gate: worst rps {worst_rps:.1f}x, worst p50 {worst_p50:.1f}x → 10x {gate10}, 20x {gate20}")
        print()

if __name__ == "__main__":
    main(sys.argv[1:])

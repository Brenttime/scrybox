#!/usr/bin/env python3
"""Best-of-N official table generator (iter 19, orchestrator bench ruling).

Reads >=4 http-bench.mjs JSON outputs for ONE table (all realistic, or all
hot) and prints, per route: BEST and MEDIAN across windows for rps (max),
p50/p99 (min = best, median across windows), with x-factors vs the BRIEF
baseline. The 10x/20x gates are evaluated on the MEDIAN table (ruling: a
gain counts only if the median beats the table of record). Window-to-window
rps spread is printed so the "gain > spread" comparison is auditable.

usage: official-table.py run-1.json run-2.json run-3.json run-4.json [...]
"""
import importlib.util, json, statistics, sys, os

_spec = importlib.util.spec_from_file_location(
    "ratio_table", os.path.join(os.path.dirname(__file__), "ratio-table.py"))
assert _spec is not None and _spec.loader is not None
_ratio = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_ratio)
baseline = _ratio.baseline

def main(files):
    if len(files) < 4:
        sys.exit("ruling requires >=4 windows per table; got %d" % len(files))
    runs = [json.load(open(f)) for f in files]
    names = [r["name"] for r in runs[0]["results"]]
    wm = [r for r in runs if r.get("write_mix")]
    print(f"windows={len(files)}  ({', '.join(os.path.basename(f) for f in files)})")
    if wm:
        w = wm[0]["write_mix"]
        print(f"write-mix: every {w['every_ms']} ms; ticks/errors per window: "
              + ", ".join(f"{r['write_mix']['ticks']}/{r['write_mix']['errors']}" for r in wm))
    rss = [r.get("peak_rss_mb") for run in runs for r in run["results"] if r.get("peak_rss_mb")]
    print("| route | baseline rps/p50/p99 | best rps/p50/p99 | median rps/p50/p99 | x(rps)/x(p50) MEDIAN | rps spread |")
    print("|---|---|---|---|---|---|")
    worst_rps, worst_p50 = 9e9, 9e9
    for name in names:
        rs = [next(r for r in run["results"] if r["name"] == name) for run in runs]
        rps = [r["rps"] for r in rs]
        p50 = [r["p50_ms"] for r in rs]
        p99 = [r["p99_ms"] for r in rs]
        best = (max(rps), min(p50), min(p99))
        med = (statistics.median(rps), statistics.median(p50), statistics.median(p99))
        b = baseline(name)
        if not b:
            print(f"| {name} | — | {best[0]:.1f} / {best[1]} / {best[2]} "
                  f"| {med[0]:.1f} / {med[1]:.2f} / {med[2]:.2f} | — | "
                  f"{max(rps)-min(rps):.0f} |")
            continue
        _, br, bp, bq = b
        xr, xp = med[0] / br, bp / med[1]
        worst_rps, worst_p50 = min(worst_rps, xr), min(worst_p50, xp)
        print(f"| {name} | {br} / {bp} / {bq} | {best[0]:.1f} / {best[1]} / {best[2]} "
              f"| {med[0]:.1f} / {med[1]:.2f} / {med[2]:.2f} "
              f"| {xr:.1f}x / {xp:.1f}x | {max(rps)-min(rps):.0f} |")
    if worst_rps < 9e9:
        g10 = "PASS" if worst_rps >= 10 and worst_p50 >= 10 else "FAIL"
        g20 = "PASS" if worst_rps >= 20 and worst_p50 >= 20 else "FAIL"
        print(f"gate (MEDIAN table): worst rps {worst_rps:.1f}x, worst p50 {worst_p50:.1f}x "
              f"-> 10x {g10}, 20x {g20}")
    if rss:
        print(f"peak RSS across windows: max {max(rss)} MB")

if __name__ == "__main__":
    main(sys.argv[1:])

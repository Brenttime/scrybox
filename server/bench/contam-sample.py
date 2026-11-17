#!/usr/bin/env python3
"""Contamination sampler for bench recording windows (iter-14 loop tool).

Usage: contam-sample.py <out.csv> [max_seconds]
Every 2 s appends: epoch, load1, and any process (outside a known-good
allowlist of bench/server/systemd) with CPU% > 40 — the signature of a
foreign workload on this 4-core box. The recorder audits this CSV after the
run: rows with a foreign process or load1 >= 2 during the window mean the
recording must be discarded (BRIEF honest-benchmark requirement).
"""
import csv, subprocess, sys, time

OUT = sys.argv[1]
MAX = int(sys.argv[2]) if len(sys.argv) > 2 else 3600
ALLOW = ("http-bench.mjs", "scrybox-server", "coldstart-probe.mjs", "contam-sample.py")

def foreign():
    try:
        out = subprocess.run(["ps", "-eo", "pid,pcpu,comm,args", "--sort=-pcpu"],
                             capture_output=True, text=True, timeout=5).stdout
    except Exception:
        return []
    rows = []
    for line in out.splitlines()[1:]:
        parts = line.split(None, 3)
        if len(parts) < 4:
            continue
        pid, pcpu, comm, args = parts
        try:
            cpu = float(pcpu)
        except ValueError:
            continue
        if cpu < 40:
            break
        if any(a in args for a in ALLOW):
            continue
        if "ps -eo" in args:
            continue
        rows.append((pid, cpu, comm))
    return rows

t0 = time.time()
with open(OUT, "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["epoch", "load1", "foreign"])
    while time.time() - t0 < MAX:
        load1 = open("/proc/loadavg").read().split()[0]
        fr = foreign()
        w.writerow([round(time.time(), 1), load1, ";".join(f"{c}:{comm}" for _, c, comm in fr[:3])])
        f.flush()
        time.sleep(2)

#!/bin/bash
# Official recording runner — orchestrator bench policy (BRIEF.md "Orchestrator
# rulings", Totoro 2026-11-10, FINAL):
#   - best-of-N is official: run >=4 windows per table, report BEST and MEDIAN;
#   - record load average + co-resident processes alongside (evidence only);
#   - the zero-foreign-process audit is DROPPED (it was unsatisfiable on this
#     shared 4-core box and rejected every window for 6 straight iterations);
#   - do NOT pause other sessions; never touch prod.
# A gain counts only if the MEDIAN beats the previous table of record by more
# than the window-to-window spread (compare-tables.py prints that verdict).
# Usage: ITER=19 ./record-window.sh   (or TAG=rust-iter-19 ./record-window.sh)
# Window: cold-start + realistic x4 (varied + write-mix 2s) + hot x4, all
# under the contamination sampler (logged, never a rejection reason).
set -u
BENCH="$(cd "$(dirname "$0")" && pwd)"
TAG="${TAG:-rust-iter-${ITER:-19}}"
cd "$BENCH"
LOG=results/record-$TAG.log
: > "$LOG"
rm -f "results/$TAG"-realistic-*.json "results/$TAG"-hot-*.json

echo "recording $TAG start $(date) — orchestrator policy: best-of-N official, load recorded, no window rejection" >> "$LOG"
python3 contam-sample.py "results/contam-$TAG.csv" 1200 & SAMPLER=$!
sleep 1
L0=$(cut -d' ' -f1 /proc/loadavg)
echo "window start load1=$L0" >> "$LOG"

./coldstart.sh >> "$LOG" 2>&1
PID=$(systemctl --user show -p MainPID --value scrybox-rusttest)
echo "server pid=$PID" >> "$LOG"
for i in 1 2 3 4; do
  echo "realistic window $i $(date +%T) load1=$(cut -d' ' -f1 /proc/loadavg)" >> "$LOG"
  node http-bench.mjs scen-varied.json --pid $PID --duration 10 --write-mix 2000 \
    > "results/$TAG-realistic-$i.json" 2>> "$LOG"
done
for i in 1 2 3 4; do
  echo "hot window $i $(date +%T) load1=$(cut -d' ' -f1 /proc/loadavg)" >> "$LOG"
  node http-bench.mjs ../../../scen-rust.json --pid $PID --duration 10 \
    > "results/$TAG-hot-$i.json" 2>> "$LOG"
done
kill $SAMPLER 2>/dev/null; sleep 1  # sampler records until the last window ends

python3 - "results/contam-$TAG.csv" <<'EOF' >> "$LOG"
import csv, sys
rows = list(csv.DictReader(open(sys.argv[1])))
fr = [r for r in rows if r['foreign'].strip()]
print(f"ENV {sys.argv[1]}: rows={len(rows)} foreign_rows={len(fr)} "
      f"start_load={rows[0]['load1'] if rows else '?'} "
      f"max_load={max(float(r['load1']) for r in rows) if rows else '?'} (recorded, not a gate)")
EOF
if ls results/$TAG-realistic-4.json results/$TAG-hot-4.json >/dev/null 2>&1; then
  echo "RECORDED $(date +%T)" >> "$LOG"; exit 0
fi
echo "RECORD_INCOMPLETE" >> "$LOG"; exit 1

#!/bin/bash
# Official recording runner with contamination gate (introduced iter 15).
# Usage: ITER=16 ./record-window.sh   (or TAG=rust-iter-16 ./record-window.sh)
# Preflight: require load1 < 1.5 and zero foreign >40%-CPU procs (the
# contam-sample.py allowlist) for 3 consecutive 2s samples before recording.
# Window: cold-start + realistic x2 + hot x1, all under the sampler.
# Audit: zero rows with a foreign process, and the window starts load1 < 1.5.
# Bench-induced load1 rise is EXPECTED (closed-loop conc 16 on 4 cores) and
# is logged, not a rejection reason — foreign procs are the discriminator
# (iter-13/14 policy).
set -u
BENCH="$(cd "$(dirname "$0")" && pwd)"
TAG="${TAG:-rust-iter-${ITER:-15}}"
cd "$BENCH"
LOG=results/record-$TAG.log
: > "$LOG"
for attempt in 1 2 3 4; do
  echo "attempt $attempt preflight $(date +%T)" >> "$LOG"
  # preflight: up to 12 min for a quiet start
  ok=0; tries=0
  while [ $tries -lt 144 ]; do
    tries=$((tries+1))
    L=$(cut -d' ' -f1 /proc/loadavg)
    F=$(ps -eo pid,pcpu,comm,args --sort=-pcpu | awk 'NR>1 && $2>=40' \
        | grep -vE "http-bench.mjs|scrybox-server|coldstart-probe|contam-sample|record-window|ps -eo" | wc -l)
    if python3 -c "exit(0 if float('$L')<1.5 else 1)" && [ "$F" -eq 0 ]; then
      ok=$((ok+1)); [ $ok -ge 3 ] && break
    else
      ok=0
    fi
    sleep 2
  done
  if [ $ok -lt 3 ]; then echo "attempt $attempt: no quiet start within 12 min — abort" >> "$LOG"; break; fi
  echo "attempt $attempt quiet, recording $(date +%T) load=$L" >> "$LOG"
  python3 contam-sample.py "results/contam-$TAG-$attempt.csv" 300 & SAMPLER=$!
  sleep 1
  ./coldstart.sh >> "$LOG" 2>&1
  PID=$(systemctl --user show -p MainPID --value scrybox-rusttest)
  echo "server pid=$PID" >> "$LOG"
  node http-bench.mjs scen-varied.json --pid $PID --duration 10 --write-mix 2000 > "results/$TAG-realistic-1.json" 2>> "$LOG"
  node http-bench.mjs scen-varied.json --pid $PID --duration 10 --write-mix 2000 > "results/$TAG-realistic-2.json" 2>> "$LOG"
  node http-bench.mjs ../../../scen-rust.json --pid $PID --duration 10 > "results/$TAG-hot.json" 2>> "$LOG"
  wait $SAMPLER 2>/dev/null
  # audit
  python3 - "results/contam-$TAG-$attempt.csv" <<'EOF' >> "$LOG"
import csv, sys
rows = list(csv.DictReader(open(sys.argv[1])))
fr = [r for r in rows if r['foreign'].strip()]
start_ok = rows and float(rows[0]['load1']) < 1.5
print(f"AUDIT {sys.argv[1]}: rows={len(rows)} foreign_rows={len(fr)} start_load={rows[0]['load1'] if rows else '?'} start_ok={start_ok} max_load={max(float(r['load1']) for r in rows)}")
sys.exit(0 if not fr and start_ok else 1)
EOF
  if [ $? -eq 0 ]; then echo "attempt $attempt CLEAN $(date +%T)" >> "$LOG"; exit 0; fi
  echo "attempt $attempt contaminated — discard, retry after cooldown" >> "$LOG"
  rm -f "results/$TAG-realistic-1.json" "results/$TAG-realistic-2.json" "results/$TAG-hot.json"
  sleep 60
done
echo "RECORD_FAILED" >> "$LOG"
exit 1

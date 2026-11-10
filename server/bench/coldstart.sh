#!/bin/sh
# Cold-start recorder (stretch-goal metric). Restarts the Rust front with the
# probe already hammering every hot-scenario route, then prints first-200 ms
# per route measured from the process's own start. Run from wt/server/bench.
# Output: results/coldstart-<ts>.json
set -e
cd "$(dirname "$0")"
TS=$(date +%H%M%S)
node coldstart-probe.mjs --scenario ../../../scen-rust.json --duration-ms 4000 --out "results/coldstart-$TS.json" &
PROBE=$!
sleep 0.3
../../restart-rust.sh
wait $PROBE
echo "wrote results/coldstart-$TS.json"

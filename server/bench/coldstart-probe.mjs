#!/usr/bin/env node
// Cold-start probe (stretch goal: "process start -> first 200 on each route").
//
// Start this BEFORE restarting the server (see coldstart.sh). It hammers
// every hot-scenario route every 10 ms until --duration-ms elapses, then
// resolves the server's MainPID via systemctl and prints, per route, the
// wall time from the Rust process's own start (starttime in /proc/<pid>/stat,
// clock ticks since boot) to the FIRST 200 the server sent for that route —
// i.e. what the very first real client paid on a cold process, not a
// warm-cache replay. Requests fired before the process binds fail with ECONN-
// REFUSED and are silently retried; the first-200 timestamps are the point.
//
//   node bench/coldstart-probe.mjs --scenario ../../../scen-rust.json \
//        --duration-ms 4000 --out results/coldstart.json

import fs from 'node:fs';
import http from 'node:http';
import { execSync } from 'node:child_process';

const opt = (k, d) => { const i = process.argv.indexOf(k); return i > -1 ? process.argv[i + 1] : d; };
const scenPath = opt('--scenario', '../../../scen-rust.json');
const outPath = opt('--out', 'coldstart.json');
const durationMs = Number(opt('--duration-ms', 4000));
const HZ = 100; // USER_HZ on Linux; stable since 2.6

const scen = JSON.parse(fs.readFileSync(scenPath, 'utf8'));
const base = new URL(scen.base);
const scenarios = scen.scenarios.map((s) => ({
  name: s.name,
  path: (s.paths ? s.paths[0] : s.path) || '/api/health',
  headers: { ...(s.headers || {}), connection: 'close' },
}));

const uptimeSec = () => Number(fs.readFileSync('/proc/uptime', 'utf8').split(' ')[0]);
const startSec = (pid) => {
  const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
  // comm may contain spaces/parens: starttime is field 22 counting after ')'.
  const after = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
  return Number(after[19]) / HZ;
};
const mainPid = () => {
  try {
    return Number(execSync('systemctl --user show -p MainPID --value scrybox-rusttest', { encoding: 'utf8' }).trim());
  } catch { return 0; }
};

const first200 = new Map();

function fire(s) {
  const req = http.request({ host: base.hostname, port: base.port, path: s.path, method: 'GET', headers: s.headers }, (res) => {
    res.resume();
    if (res.statusCode === 200 && !first200.has(s.name)) first200.set(s.name, uptimeSec());
  });
  req.on('error', () => {});
  req.end();
}

const t0 = Date.now();
const tick = setInterval(() => {
  for (const s of scenarios) fire(s);
  if (Date.now() - t0 > durationMs) {
    clearInterval(tick);
    // Let in-flight responses land their callbacks before reading.
    setTimeout(() => {
      const pid = mainPid();
      const out = { pid, probed_at: new Date().toISOString(), routes: {} };
      if (pid && fs.existsSync(`/proc/${pid}/stat`)) {
        const pStart = startSec(pid);
        for (const s of scenarios) {
          const t = first200.get(s.name);
          out.routes[s.name] = t === undefined ? null : Math.round((t - pStart) * 1000);
        }
      }
      fs.writeFileSync(outPath, JSON.stringify(out, null, 1));
      console.log(JSON.stringify(out.routes));
      process.exit(0);
    }, 150);
  }
}, 10);

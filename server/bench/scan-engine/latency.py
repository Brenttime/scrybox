#!/usr/bin/env python3
"""Interleaved latency: sidecar (HTTP) vs in-process engine (scan_engine_eval)
on the same requests, alternating rounds so host load hits both equally.
usage: latency.py <diff_dir> <base_url> <eval_bin> [n_requests] [rounds]
(eval env vars as in README)."""
import json, os, statistics, subprocess, sys, time, urllib.error, urllib.request

D, BASE, BIN = sys.argv[1:4]
N = int(sys.argv[4]) if len(sys.argv) > 4 else 60
ROUNDS = int(sys.argv[5]) if len(sys.argv) > 5 else 2
B = D + '-lat'
os.makedirs(B, exist_ok=True)
reqs = [json.loads(l) for l in open(D + '/requests.jsonl')]
sub = [r for r in reqs if r['path'] != '/api/scene-complete' and 'X-Scan-Session' not in r['headers']][:N]
with open(B + '/requests.jsonl', 'w') as f:
    for r in sub:
        f.write(json.dumps(r) + '\n')
if not os.path.exists(B + '/bodies'):
    os.symlink(os.path.abspath(D + '/bodies'), B + '/bodies')


def py_run():
    out = []
    for r in sub:
        body = open(D + '/' + r['body'], 'rb').read()
        q = urllib.request.Request(BASE + r['path'], data=body, headers=r['headers'], method='POST')
        t = time.perf_counter()
        try:
            urllib.request.urlopen(q, timeout=120).read()
        except urllib.error.HTTPError as e:
            e.read()
        out.append((r['path'], (time.perf_counter() - t) * 1000))
    return out


def rs_run():
    p = subprocess.run([BIN, B], capture_output=True, text=True)
    return [(sub[i]['path'], json.loads(l)['ms']) for i, l in enumerate(p.stdout.splitlines())]


res = {'python': [], 'rust': []}
for _ in range(ROUNDS):
    res['python'] += py_run()
    res['rust'] += rs_run()
for who, v in res.items():
    for ep in sorted({p for p, _ in v}):
        xs = sorted(m for p, m in v if p == ep)
        print(f'{who:6} {ep:18} n={len(xs):3} p50 {xs[len(xs)//2]:6.0f} p90 {xs[int(len(xs)*.9)]:6.0f} mean {statistics.mean(xs):6.0f} ms')
print('loadavg', os.getloadavg())

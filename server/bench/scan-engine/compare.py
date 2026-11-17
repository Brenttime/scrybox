#!/usr/bin/env python3
"""Compare py.jsonl (sidecar) vs rust.jsonl (in-process) per endpoint.

usage: compare.py <diff_dir>   (needs requests.jsonl, py.jsonl, rust.jsonl)"""
import json, sys
from collections import Counter, defaultdict

D = sys.argv[1]
reqs = {r['id']: r for r in map(json.loads, open(f'{D}/requests.jsonl'))}
py = {r['id']: r for r in map(json.loads, open(f'{D}/py.jsonl'))}
rs = {r['id']: r for r in map(json.loads, open(f'{D}/rust.jsonl'))}
VOLATILE = {'timings', 'request_ms', 'history', 'file', 'ts'}


def scrub(v):
    if isinstance(v, dict):
        return {k: scrub(x) for k, x in v.items() if k not in VOLATILE}
    if isinstance(v, list):
        return [scrub(x) for x in v]
    return v


def first_diff(a, b, path=''):
    if type(a) != type(b) and not (isinstance(a, (int, float)) and isinstance(b, (int, float))):
        return f'{path}: {str(a)[:80]} != {str(b)[:80]}'
    if isinstance(a, dict):
        if list(a) != list(b):
            return f'{path}: keys {list(a)} != {list(b)}'
        for k in a:
            d = first_diff(a[k], b[k], f'{path}.{k}')
            if d:
                return d
        return None
    if isinstance(a, list):
        if len(a) != len(b):
            return f'{path}: len {len(a)} != {len(b)}'
        for i, (x, y) in enumerate(zip(a, b)):
            d = first_diff(x, y, f'{path}[{i}]')
            if d:
                return d
        return None
    return None if a == b else f'{path}: {a!r} != {b!r}'


def ident(r):
    c = r.get('card') or {}
    return (r.get('scene_number'), bool(r.get('ok')), r.get('title'), c.get('id'), c.get('set'), c.get('num'),
            (r.get('footer_ocr') or {}).get('resolved_by'), r.get('error'))


stats = defaultdict(Counter)
lat = defaultdict(lambda: ([], []))
examples = defaultdict(list)
for i, rq in reqs.items():
    ep = rq['path']
    p, r = py[i], rs.get(i)
    if r is None:
        stats[ep]['missing'] += 1
        continue
    stats[ep]['requests'] += 1
    lat[ep][0].append(p['ms'])
    lat[ep][1].append(r['ms'])
    pj, rj = scrub(p['json']), scrub(r['json'])
    if p['status'] == r['status']:
        stats[ep]['status_same'] += 1
    d = first_diff(pj, rj)
    if d is None:
        stats[ep]['json_identical'] += 1
    else:
        examples[ep].append((i, d))
    pc, rc = pj.get('candidates'), rj.get('candidates')
    if pc is not None or rc is not None:
        pc, rc = pc or [], rc or []
        stats[ep]['cand_py'] += len(pc)
        stats[ep]['cand_rs'] += len(rc)
        if [c.get('box') for c in pc] == [c.get('box') for c in rc]:
            stats[ep]['frames_boxes_same'] += 1
        if [c.get('quad') for c in pc] == [c.get('quad') for c in rc]:
            stats[ep]['frames_quads_same'] += 1
        if [(c.get('eligible'), c.get('status')) for c in pc] == [(c.get('eligible'), c.get('status')) for c in rc]:
            stats[ep]['frames_eligibility_same'] += 1
        stats[ep]['frames_with_candidates'] += 1
    pr, rr = pj.get('results'), rj.get('results')
    if pr is not None or rr is not None:
        pi = sorted(map(ident, pr or []), key=str)
        ri = sorted(map(ident, rr or []), key=str)
        stats[ep]['results_py'] += len(pi)
        stats[ep]['ok_py'] += sum(1 for x in pi if x[1])
        stats[ep]['ok_rs'] += sum(1 for x in ri if x[1])
        same = Counter(pi) & Counter(ri)
        stats[ep]['results_identity_same'] += sum(same.values())
        for x in (Counter(pi) - Counter(ri)):
            examples[ep + ' identity'].append((i, 'py ' + str(x)))
        for x in (Counter(ri) - Counter(pi)):
            examples[ep + ' identity'].append((i, 'rs ' + str(x)))


def pct(v, q):
    v = sorted(v)
    return v[min(len(v) - 1, int(q * len(v)))] if v else 0


for ep, s in stats.items():
    print(f'== {ep}')
    for k, v in s.items():
        print(f'   {k}: {v}')
    a, b = lat[ep]
    print(f'   latency ms py p50/p90 {pct(a, .5):.0f}/{pct(a, .9):.0f}  rust p50/p90 {pct(b, .5):.0f}/{pct(b, .9):.0f}')
for ep, ex in examples.items():
    print(f'-- {ep}: {len(ex)} diffs')
    for i, d in ex[:12]:
        print(f'   #{i} {d}')

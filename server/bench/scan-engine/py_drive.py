#!/usr/bin/env python3
"""Differential driver: replay the corpus against the Python sidecar
(throwaway container) and record requests + responses.

usage: py_drive.py <corpus_dir> <base_url> <out_dir>
Writes out_dir/requests.jsonl (endpoint, headers, body file) and
out_dir/py.jsonl (status, json, wall ms)."""
import base64, json, os, sys, time, urllib.error, urllib.request

C, BASE, OUT = sys.argv[1], sys.argv[2].rstrip('/'), sys.argv[3]
os.makedirs(OUT + '/bodies', exist_ok=True)
files = json.load(open(C + '/files.json'))
reqs = open(OUT + '/requests.jsonl', 'w')
res = open(OUT + '/py.jsonl', 'w')
n = 0


def post(path, body, headers):
    global n
    n += 1
    bf = f'bodies/{n:05d}.bin'
    open(f'{OUT}/{bf}', 'wb').write(body)
    rid = n
    reqs.write(json.dumps({'id': rid, 'path': path, 'headers': headers, 'body': bf}) + '\n')
    req = urllib.request.Request(BASE + path, data=body, headers=headers, method='POST')
    t = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            status, data = r.status, r.read()
    except urllib.error.HTTPError as e:
        status, data = e.code, e.read()
    ms = (time.perf_counter() - t) * 1000
    j = json.loads(data)
    res.write(json.dumps({'id': rid, 'status': status, 'ms': ms, 'json': j}) + '\n')
    return j


health = json.load(urllib.request.urlopen(BASE + '/api/health'))
for f in files['frames']:
    raw = open(f'{C}/{f}', 'rb').read()
    det = post('/api/detect-cards', raw, {'Content-Type': 'image/jpeg', 'X-Scan-Source': 'browser-camera'})
    post('/api/scan-frame', raw, {'Content-Type': 'image/jpeg', 'X-Scan-Client': 'diff'})
    cards = []
    for c in det.get('candidates') or []:
        if not c.get('eligible'):
            continue
        item = {'image': base64.b64encode(raw).decode(), 'number': c['number']}
        if c.get('quad'):
            item['quad'] = c['quad']
        else:
            item['box'] = c['box']
        cards.append(item)
    if cards:
        post('/api/scan-scene', json.dumps({'cards': cards[:8]}).encode(), {'Content-Type': 'application/json', 'X-Scene-Protocol': '3'})
for f in files['cards']:
    raw = open(f'{C}/{f}', 'rb').read()
    import struct
    w, h = struct.unpack('>II', raw[16:24])  # PNG IHDR
    post('/api/scan-scene', json.dumps({'cards': [{'image': base64.b64encode(raw).decode(), 'box': [0, 0, w, h], 'number': 1}]}).encode(),
         {'Content-Type': 'application/json', 'X-Scene-Protocol': '3'})
# session sequence: same frame thrice with a session (identity cache)
f0 = files['frames'][-1]
raw = open(f'{C}/{f0}', 'rb').read()
for _ in range(3):
    post('/api/scan-frame', raw, {'Content-Type': 'image/jpeg', 'X-Scan-Session': 'difftest'})
post('/api/scan-scene', b'{}', {'Content-Type': 'application/json', 'X-Scene-Protocol': '2'})
post('/api/scan-scene', b'{"cards": []}', {'Content-Type': 'application/json', 'X-Scene-Protocol': '3'})
post('/api/scene-complete', b'{}', {'Content-Type': 'application/json'})
post('/api/detect-cards', b'not an image', {'Content-Type': 'image/jpeg'})
print('requests', n, 'health', health)

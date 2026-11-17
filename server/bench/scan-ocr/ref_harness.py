import json, os, sys, time, io
sys.path.insert(0, "/app")
import numpy as np
from PIL import Image, ImageOps
import server as S

S.load_oracle()
assert S.init_ocr()
C = "/corpus"
man = json.load(open(f"{C}/manifest.json"))
os.makedirs("/out/strips", exist_ok=True)
res = []
strip_idx = 0
for fi, item in enumerate(man):
    im = Image.open(f"{C}/{item['frame']}"); im.load(); im = im.convert("RGB")
    crops = []
    for (x, y, w, h) in item["boxes"]:
        crops.append(im.crop((x, y, x + w, y + h)).convert("RGB"))
    # per-crop variant-0 strip, saved for byte-exact preprocessing checks
    strips = []
    for ci, c in enumerate(crops):
        x0, x1, y0, y1, th = (0.030, 0.82, 0.025, 0.100, 96)
        s = c.crop((int(c.width*x0), int(c.height*y0), int(c.width*x1), int(c.height*y1)))
        s = ImageOps.autocontrast(s.convert("RGB"))
        tw = max(64, round(s.width*th/s.height))
        s = s.resize((tw, th), S.LANCZOS)
        p = f"strips/{fi}_{ci}.png"; s.save(f"/out/{p}")
        a = np.asarray(s)
        strips.append(a)
    t0 = time.perf_counter()
    txts, scores = S._recognize_scene_strips(strips)
    t1 = time.perf_counter()
    single = [S._recognize_scene_strips([a]) for a in strips]
    t2 = time.perf_counter()
    titles = S._batch_scene_titles(crops)
    t3 = time.perf_counter()
    res.append({
        "frame": item["frame"],
        "batch_strip": [[t, float(s)] for t, s in zip(txts, scores)],
        "single_strip": [[t[0][0], float(t[1][0])] for t in single],
        "match": [list(S.find_card_by_ocr(t)) for t in txts],
        "titles": [[n, float(sc), r, a] for (n, sc, r, a) in titles],
        "ms": {"batch_rec": (t1-t0)*1000, "titles": (t3-t2)*1000},
    })
json.dump(res, open("/out/ref.json", "w"), indent=0)
print("done", len(res))

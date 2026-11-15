import json, sys, os
S = os.path.expanduser("~/.hermes/cache/scratch/mono-scan")
py = json.load(open(f"{S}/ref/ref.json"))
rs = json.load(open(sys.argv[1] if len(sys.argv) > 1 else f"{S}/rust.json"))
assert len(py) == len(rs)
c = dict(strip_txt=0, strip_txt_n=0, conf_absmax=0.0, single_txt=0, title_eq=0, title_n=0,
         py_named=0, rs_named=0, name_eq=0, score_eq=0)
diffs = []
for p, r in zip(py, rs):
    for (pt, ps), (rt, rsc) in zip(p["batch_strip"], r["batch_strip"]):
        c["strip_txt_n"] += 1; c["strip_txt"] += pt == rt
        if pt == rt: c["conf_absmax"] = max(c["conf_absmax"], abs(ps - rsc))
    for (pt, _), (rt, _) in zip(p["single_strip"], r["single_strip"]):
        c["single_txt"] += pt == rt
    for i, (pt, rt) in enumerate(zip(p["titles"], r["titles"])):
        c["title_n"] += 1
        c["py_named"] += bool(pt[0]); c["rs_named"] += bool(rt[0])
        c["name_eq"] += pt[0] == rt[0]
        c["score_eq"] += pt[0] == rt[0] and abs(pt[1] - rt[1]) < 1e-9
        same = pt[0] == rt[0] and pt[2] == rt[2] and pt[3] == rt[3] and abs(pt[1] - rt[1]) < 1e-9
        c["title_eq"] += same
        if not same: diffs.append((p["frame"], i, pt, rt))
pm = sorted(x["ms"]["titles"] for x in py); rm = sorted(x["ms"]["titles"] for x in rs)
c["py_titles_ms_p50"] = round(pm[len(pm)//2], 1); c["rs_titles_ms_p50"] = round(rm[len(rm)//2], 1)
c["py_titles_ms_mean"] = round(sum(pm)/len(pm), 1); c["rs_titles_ms_mean"] = round(sum(rm)/len(rm), 1)
print(json.dumps(c, indent=1))
for d in diffs[:20]: print("DIFF", d)

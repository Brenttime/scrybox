#!/usr/bin/env python3
"""One-time (build-time) vendoring of Scryfall set metadata for the local clone.

Fetches https://api.scryfall.com/sets (the full list; has_more is false) and
derives the two files the server loads at startup:

  data/blocks.json     — Scryfall /blocks-shaped JSON ({"object":"list","blocks":
                         [{"id","name","sets":[{"code","name","id"}]}]}), SYNTHESIZED
                         from the `block` field of the set list, because this API
                         deployment 404s on /blocks. Blocks are ordered by the
                         earliest release date among their sets.
  data/sets-meta.json  — {code: {id, scryfall_uri, block, parent_set, icon_svg_uri,
                         digital, foil_only, nonfoil_only, printed_size,
                         mtgo_code, arena_code}} for merging into /api/sets rows
                         and the /api/set/{code} detail envelope.

Provenance: data comes from api.scryfall.com/sets, retrieved 2026-10-24.
Re-run (rarely) to refresh:
  curl -s -H 'User-Agent: curl/8.5.0' -H 'Accept: */*' https://api.scryfall.com/sets -o /tmp/sets.json
  python3 data/gen_sets_meta.py /tmp/sets.json
"""
import json
import sys
import uuid
import collections
from pathlib import Path

SRC = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).parent / "scryfall-sets-raw.json"
OUT = Path(sys.argv[2]) if len(sys.argv) > 2 else Path(__file__).parent
OUT.mkdir(parents=True, exist_ok=True)

d = json.loads(SRC.read_text())
assert d.get("object") == "list" and not d.get("has_more"), "expected a single-page full /sets dump"
sets = d["data"]

meta = {}
for s in sets:
    meta[s["code"]] = {
        "id": s.get("id"),
        "scryfall_uri": s.get("scryfall_uri"),
        "block": s.get("block"),
        # Scryfall's set objects spell this `parent_set_code` — reading
        # `parent_set` here silently nulled every parent, which disabled the
        # release-group merge in index.rs.
        "parent_set": s.get("parent_set_code"),
        "icon_svg_uri": s.get("icon_svg_uri"),
        "digital": s.get("digital"),
        "foil_only": s.get("foil_only"),
        "nonfoil_only": s.get("nonfoil_only"),
        "printed_size": s.get("printed_size"),
        "mtgo_code": s.get("mtgo_code"),
        "arena_code": s.get("arena_code"),
    }

# group sets by block name, ordered by earliest release date in the block
groups = collections.defaultdict(list)
released = {s["code"]: (s.get("released_at") or "9999-12-31") for s in sets}
name_by_code = {s["code"]: s["name"] for s in sets}
id_by_code = {s["code"]: s.get("id") for s in sets}
for s in sets:
    b = s.get("block")
    if b:
        groups[b].append(s["code"])

def key(item):
    name, codes = item
    return (min(released.get(c, "9999-12-31") for c in codes), name)

blocks = []
for name, codes in sorted(groups.items(), key=key):
    codes = sorted(codes, key=lambda c: (released.get(c, "9999-12-31"), name_by_code.get(c, "")))
    blocks.append({
        "id": id_by_code.get(codes[0]) or str(uuid.uuid4()),
        "name": name,
        "sets": [{"code": c, "name": name_by_code.get(c, c), "id": id_by_code.get(c)} for c in codes],
    })

(OUT / "sets-meta.json").write_text(json.dumps(meta, ensure_ascii=False, sort_keys=True))
(OUT / "blocks.json").write_text(json.dumps({
    "object": "list",
    "uri": "(derived from api.scryfall.com/sets `block` field; /blocks 404s on this deployment)",
    "blocks": blocks,
}, ensure_ascii=False, indent=1))
print(f"blocks: {len(blocks)} | sets with block: {sum(len(b['sets']) for b in blocks)} | meta: {len(meta)}")

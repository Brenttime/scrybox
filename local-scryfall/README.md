# local-scryfall — vendored Scryfall search engine (Scrybox sidecar)

Self-contained Rust reimplementation of the Scryfall search API: one static
binary (axum + tokio, **no SQLite**, no external deps at runtime). Loads the
Scryfall bulk dump (119,158 default-listing prints) into a memory-resident
index at startup (~5.5 s, ~490 MB RSS) and serves search, suggest, set
browsing, and a card portal UI from RAM. Roaring-bitmap inverted indexes +
rayon parallel scans; each card's JSON is pre-serialized at load, so a search
response is a zero-parse string join.

Vendored from `~/projects/local-scryfall/scryfall-api` so the Scrybox
`docker-compose` stack can build it as the `scryfall-local` sidecar. With
`LOCAL_SCRYFALL_ENABLED=true` in the Scrybox backend, hot-path Scryfall calls
(card search, set enumeration) hit this server instead of api.scryfall.com —
zero rate-limit budget spent, and those paths work with no internet at all.

**Provenance & license:** built from the Scryfall bulk files
(`default-cards`, `oracle-tags`) at `https://data.scryfall.io/` (live URLs
resolved via `https://api.scryfall.com/bulk-data`) plus `api.scryfall.com/sets`
for set metadata — see `fetch-data.sh` and `data/gen_sets_meta.py`.
Card data & images © Scryfall (https://scryfall.com). This indexes Scryfall's
public bulk data for a single self-hosted install; it is not a redistribution
service, and the compose stack gives it no published ports.

## Run standalone

```sh
SCRYFALL_DATA_DIR=./data ./fetch-data.sh   # first time: pulls bulk files (~86 MB)
cargo build --release
./target/release/scryfall-api              # binds 0.0.0.0:8420
./tests.sh http://127.0.0.1:8420           # regression suite, server must be up
```

## Run in Docker

```sh
docker build -t scryfall-local local-scryfall/
docker run -d --name scryfall-local -v scryfall-data:/data -p 8420:8420 scryfall-local
```

The entrypoint runs `fetch-data.sh` first (network-tolerant: warns and
continues on failure — the image ships the small vendored `data/` files, so it
boots offline once the bulk files are on the volume) and then `exec`s the
server. The Scrybox compose stack runs this service with **no published
ports** (backend talks to it over the compose network) — see docker-compose.yml.

## Environment variables

| Var | Default | Meaning |
| --- | --- | --- |
| `PORT` | `8420` | listen port (on 0.0.0.0) |
| `BIND` | — | full `host:port`, wins over `PORT` |
| `SCRYFALL_DATA_DIR` | `./data` | dir with `default-cards.jsonl.gz`, `oracle-tags.jsonl.gz`, `sets-meta.json`, `blocks.json`, `land_cycles.json` (Docker: `/data`) |
| `CARDS_JSONL_GZ` | — | explicit cards file (escape hatch over `SCRYFALL_DATA_DIR`) |
| `ORACLE_TAGS_JSONL_GZ` | — | explicit oracle-tags file |
| `SETS_META_DIR` | — | explicit dir for `blocks.json` + `sets-meta.json` |
| `STATIC_DIR` | `./static` | portal assets served at `/` |

Missing `oracle-tags.jsonl.gz` degrades gracefully (`otag:` warns and returns
0 results); missing `data/` files only disable block/icon/parent fields —
startup logs what it found.

## API contract

### `GET /cards/search` (Scryfall alias) ≡ `GET /api/search` ≡ `GET /api/cards/search`
Params: `q`, `limit` (default 75, max 500; aliases `page_size`/`per_page`),
`page` (1-based; or `offset`), `order` = `name|cmc|rarity|set|released`,
`dir` = `asc|desc`.

Response envelope (Scryfall-compatible — the `object`/`type`/`context`
markers and `self`/`next_page` links let a client switch its baseURL from
`https://api.scryfall.com` to this server and keep paging unchanged):
```json
{ "object": "list", "type": "card", "context": "search",
  "data": [ …card objects… ], "cards": [ …same array, alias… ],
  "total_cards": 62, "total": 62, "page": 1, "page_size": 60, "per_page": 60,
  "offset": 0, "has_more": true, "self": "http://host/cards/search?…",
  "next_page": "http://host/cards/search?…page=2", "took_ms": 1.2, "warnings": [] }
```

Card object fields (flat, stable): `id, oracle_id, name, set, set_name,
collector_number, type_line, mana_cost, cmc, colors[], color_identity[],
rarity, oracle_text, keywords[], power, toughness, loyalty, layout,
image_uris{small,normal}, image_url, legal, legalities{}, scryfall_uri,
artist, released_at`. `legal` is a comma-joined string of formats whose
legality is `legal` (e.g. `"modern,legacy,pauper"`). `legalities` covers these
14 formats: standard, future, historic, pioneer, modern, legacy, pauper,
vintage, commander, brawl, paupercommander, oathbreaker, alchemy, timeless.
(Note: bulk-derived rows carry no `prices` — the Scrybox backend keeps its
background price/oracle-tag enrichment on upstream Scryfall for that reason.)

### `GET /api/suggest?q=…&limit=…`
```json
{ "suggestions": [ {"name","set","type_line","image_small","id"}, … ],
  "tokens": [ "is:legendary", … ] }
```
Prefix-matched names (reprint-deduped) with Scryfall-style ≤1-edit fuzzy
fallback ("darc" → Dark…), plus query-token completions.

### `GET /api/card/{id}` — by Scryfall uuid, `set/collector` (`lea/1`), or exact name.
Returns the same card object; `404 {"status_code":404,"details":…}` otherwise.

### `GET /sets` (Scryfall alias) / `GET /api/sets`
Startup-cached set aggregation. `q` = name/code substring, `type` =
comma-separated set_type list (multi allowed), `order` = `date` (default,
desc) | `set` | `block` | `cards`. Rows keep `code,name,set_type,released_at,
card_count,sample_image` and add `block,parent_set,parent_set_code,
icon_svg_uri,printed_size,digital,foil_only,nonfoil_only` from the vendored
metadata (see `data/`). `GET /sets` content-negotiates on `Accept`: JSON
clients get Scryfall's `/sets` envelope (`object:"list"`, `data[]` with
per-record `object:"set"`, `has_more:false`); browsers get the 301 below.

### Set pages (frontend)
`/sets` → 301 → `/sets.html` (Scryfall-style set list: text filter +
sorted-by/of-type dropdowns with `?q=&type=&order=` URL sync, zebra
checklist rows with 20px set icons, spoiler pill for future `released_at`).
`/sets/{code}` serves `static/set.html` (SPA: purple set header band, meta
sublines, client-generated Download CSV, card grid fetched with
`q=s:CODE include:extras` so tokens/variants show, empty-state tiles for
cards without art, back link to /sets.html).
Documented divergences: English-only (lang dropdown hidden, non-en ⇒ 0),
text filter added to the controls row, no price subline (type counts
instead), "spoiler" badge added (Scryfall shows none in the list).

`include:` — `include:extras` (and all/legacy/shadow/spotlight/future/
spoiled/nonlegal/hasboostercrypto/hascontentwarnings) parse as neutral
visibility modifiers: no warning, no filtering. Without `include:` the
search behavior is byte-identical to the pre-sets revision (verified
against the running old binary; the parity battery never sends include:).

### `data/` — vendored Scryfall set metadata
`blocks.json` + `sets-meta.json` + `land_cycles.json`, generated by
`data/gen_sets_meta.py` from an `api.scryfall.com/sets` fetch (this
deployment 404s on `/blocks`, so blocks are derived from the set list's
`block` field; `parent_set_code` on set objects feeds the release-group
merge). Re-run rarely to refresh — see the script docstring. Missing files
only disable block/icon/parent fields; startup logs it.

### `GET /api/health` / `GET /api/healthz`
`{"status":"ok","cards":119158,"index_build_ms":…,"uptime_s":…,"rss_mb":…,…}`

## Query syntax (Scryfall subset)

- free text (fuzzy name matching via trigram index + token index over name/type/oracle), `"quoted phrases"`
- `and` / `or` / `not` / `!` / `-`, `(grouping)`
- `c:`/`ci:` with `>` (contains), `<` (subset), `=` (exact), `!` (exclude), `x` (colorless); colon-less `c>rw`
- `t:` `ot:` type tokens, `o:` oracle substring, `n:` name substring, `m:` mana symbols, `a:` artist
- numeric: `cmc`/`mv`, `p`/`pow`, `tou`, `lov` with `= != > < >= <=` and ranges (`cmc=3-5`), colon-less `cmc>=4`
- `r:` rarity (c/u/r/m/full), `s:`/`e:` set (code, code prefix, or set-name substring), `f:`/`legal:` format (+ aliases edh, explorer, ob)
- `k:` keyword, `layout:`, `is:` legendary|spell|permanent|creature|instant|sorcery|artifact|enchantment|planeswalker|battle|basic|snow|vanilla|multicolored|colorless|dfc|fullart|textless|digital|promo|oversized|token|emblem|scheme|plane|phenomenon|vanguard|art_series|saga|adventure|class|leveler|mutate|prototype|case
- `otag:` Oracle Tags (needs `oracle-tags.jsonl.gz`), `id:` oracle uuid
- unknown keys produce a `warnings[]` entry and fall back to free-text.

## Layout

- `src/index.rs` — loader, interned string table, compact `Card` struct, roaring inverted indexes, trigram fuzzy index, precomputed sort orders, per-card pre-serialized JSON
- `src/query.rs` — hand-written tokenizer + recursive-descent parser → AST
- `src/eval.rs` — AST → RoaringBitmap (AND/OR/NOT set algebra, rayon scans with bitmap pre-filtering)
- `src/api.rs` — axum handlers, string-assembled JSON responses
- `src/main.rs` — router (incl. Scryfall-compatible aliases), compression, ServeDir fallback, 0.0.0.0:8420
- `fetch-data.sh` — first-boot bulk-data pull for the container
- `Dockerfile` — multi-stage build for the compose sidecar

## Measured latency (release build, localhost, 600 samples)

| scenario | p50 | p99 |
|---|---|---|
| text `dark ritual` | 1.2 ms | 1.9 ms |
| filters `c:rg t:creature cmc=3 f:commander` | 2.4 ms | 3.8 ms |
| phrase `"black lotus" f:vintage` | 1.9 ms | 2.1 ms |
| `o:"draw a card" cmc<=3` | 4.0 ms | 4.5 ms |
| limit=500 page | 3.1 ms | 4.4 ms |
| suggest (fuzzy/prefix) | 0.3–0.7 ms | ≤1.1 ms |
| card / healthz | 0.3 ms | 0.5 ms |

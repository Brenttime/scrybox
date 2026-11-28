# Scrybox — Architecture & Developer Guide

Developer-facing reference for the codebase. For install/run/deploy and end-user
features, see [README.md](README.md); this document explains **how the system is
built and why**.

Scrybox is a self-hosted trading-card collection manager for **Magic:
The Gathering**. It identifies cards from a phone photo (no typing),
values the collection over time, and helps you pull the cards for a deck back
out again.

- **Backend**: Rust (axum + tokio), SQLite (single file, rusqlite), served together with the built frontend from one container (`Dockerfile.rust`).
- **Frontend**: React + Vite SPA.
- **Auth**: opaque session tokens in a server-side `sessions` table, sent as a `Bearer` header.
- **Card data**: Scryfall, cached locally in `card_cache`.
- **Image ID**: two small ONNX models — `cornelius` finds the card's corners, `milo` embeds the dewarped card as a 128-d unit vector — then a brute-force cosine sweep over a prebuilt catalog of every cached card's artwork. Corner detection also runs in the browser, so the outline on screen is the crop that gets matched.

Stack: React + Vite + Recharts on the front, axum + `rusqlite` with hand-written
security-header and IP rate-limit layers on the back (`secheaders.rs`,
`ip_limit.rs`), the ONNX Runtime C API (`ort`) plus native image codecs for
images on the server and `onnxruntime-web` in a worker on the client, Docker + GitHub Actions
to ship.

---

## Repository layout

```
server/
  crates/
    scrybox-server/        Rust axum server: auth, per-route modules (see Route map),
                           scan engine (scan_engine/), OCR (scan_ocr/), migrations, SPA serving
    local-scryfall/        Local Scryfall engine crate (parser/evaluator/index), linked in-process; also builds the standalone `scryfall-api` bin
  native/                  Native image codec builds (mozjpeg wrapper)
  docker/                  Rust image build + entrypoint scripts
  bench/                   HTTP benchmarks
  scripts/                 Dev/parity helper scripts
frontend/
  src/
    main.jsx, App.jsx      Entry + root: auth state, fetch wrapper (injects Bearer), tab routing, code-split views
    components/            One component per screen/widget (see Frontend section)
    utils/                 Pure helpers: sorting, pricing, printing/rarity styling, language, shuffle
shared/                    Code used by both the server and the frontend: scryfallQuery.js, cardOrder.json,
                           sortSchemes.json, cardListText.js, clientScan/, imgproc.mjs
local-scryfall/            Portal static assets, vendored set meta, fetch-data.sh, standalone Dockerfile (engine source is in server/crates)
Dockerfile.rust, .github/workflows/docker-build.yml   Rust container build + CI publish to GHCR
```

Regenerable/large artifacts live under `CV_MODEL_DIR` (the two ONNX files and
the built catalogs, `CV_MODEL_DIR`) and the SQLite DB — both gitignored. A catalog
is ~5 MB per 10k cards, two orders of magnitude smaller than the per-set ORB
indexes this replaced (~2.6 GB), but it is still build output, so the container
points `CV_MODEL_DIR` at `/app/database/models` on the mounted volume — otherwise
an image update discards every catalog an admin built.

The two models are in neither the repository nor the image, and that is a
licensing decision rather than an omission: they are AGPL-3.0 while Scrybox is MIT,
so the operator fetches them into `CV_MODEL_DIR` as a deliberate step
(the Models section under **Settings → Scan catalogs** downloads them, optionally
with the published catalog fallbacks). Startup says so when they are absent, because that is the ordinary
state of a fresh install, and `/api/scan-match` answers `503 notBuilt` rather than
failing obscurely at session creation.

---

## Backend

### Request lifecycle

The server (`main.rs` / `lib.rs`) wires the security headers (with a Report-Only CSP that allow-lists the card-image
hosts), JSON body limits, the API routers, then serves the
built SPA and a SPA fallback. `GET /api/health` is unauthenticated and backs the
Docker `HEALTHCHECK`. An empty `users` table stays empty unless
`DEFAULT_ADMIN_PASSWORD` is set, which seeds the `admin` account at startup;
otherwise the first browser visit sets that account's password through
`POST /api/auth/bootstrap`. Both paths name it `admin` — the bootstrap route ignores
any username posted to it, because the name has to be knowable to whoever set
`DEFAULT_ADMIN_PASSWORD` and is what `db.adoptOrphanRows` is reached through. No
password is ever logged.

Startup also probes `CV_MODEL_DIR` with a real write — `fs.access(W_OK)` reports
permission bits, which is not the same question as whether the filesystem will
accept a file — and warms the two ONNX sessions and the default catalog, so the
first scan of a session does not pay for the load. An unwritable model directory
is reported and survived: everything except catalog builds still works.

### Auth

Authentication is DB-backed session tokens, not JWTs:

- `POST /api/auth/login` verifies a PBKDF2 password hash and inserts a row into `sessions` (`user_id`, `token`, `expires_at`).
- The auth gate (`auth.rs`, the port of Node's `authenticateToken`) reads the `Bearer` token, looks it up in `sessions` where `expires_at > now`, and sets the request user `{ id, username, role, share_token, share_enabled, api_key, ... }`.
- `requireAdmin` gates admin-only routes on `req.user.role === 'admin'`.
- A bearer token that matches no session is then checked against `users.api_key` — a long-lived read-only credential for external scripts (issue #33). It sets `req.user.via_api_key`, which makes `authenticateToken` refuse any non-GET (403) and `requireAdmin` refuse it outright, and makes `/auth/me` return only the read-only key rather than the account's other fields. Read-only is the whole reason a non-expiring credential is acceptable here; anything that weakens it has to replace it with something scoped.
- Rate limiting (`ip_limit.rs`, the port of Node's `authLimiter`) protects login and expensive endpoints.

The collection routes sit behind the auth gate from the front (route groups are
registered through the auth-checked stack in `lib.rs`), so every
collection/deck-adjacent route requires a valid session.

### Route map

Routes are registered per group in `server/crates/scrybox-server/src/lib.rs`;
each group lives in its own module:

| Mount | Module(s) | Responsibility |
|-------|-----------|----------------|
| `/api/auth` | auth_routes.rs | `register`, `login`, `logout`, `me`, `PUT /settings` (per-user), `POST/DELETE /api-key` (read-only external key) |
| `/api` | search.rs, collection.rs / collection_writes.rs, cvscan.rs, stats.rs / stats_subs.rs, export.rs, cards_meta.rs | Card `search`, `scan-match`; `collection` CRUD + `bulk`; `stats`, `stats/history`, `stats/networth`, `export`, `import`; `cards/:id/price-history` |
| `/api/decks` | decks_reads.rs / decks_writes.rs / decks_combos.rs | Deck CRUD, `:id/cards`, `:id/checkout`, `:id/return`, `:id/locations` (checkout/check-in coverage payload) |
| `/api/sets` | sets.rs | Set catalog for dividers and scan scoping |
| `/api/settings` | settings.rs | App-wide settings (read any; write requires admin) |
| `/api/shared` | shared.rs | Public, read-only collection view by `share_token` (no auth) |
| `/api/admin` | admin_users.rs, seed_cards.rs, catalog_build.rs, model_assets.rs, backup.rs | User management, card cache seeding, `catalogs` (list / `build` / `stop` / `progress`), model downloads, DB backups (admin) |

### Card data sources

The Scryfall client (`scryfall.rs`, with the `card_cache` upserts in
`card_cache.rs`) normalizes provider cards into one shape and upserts into
`card_cache`. Every card carries a `language` recording which printing the
row is (a non-English printing is its own card, not a display variant: it has
its own provider id, its own art and its own name).

#### Two names per card, and which is which

`card_cache.name` is the **searchable** name and `printed_name` is the name **on the
card**. Display reads `printed_name || name` (`utils/languages.displayName`,
`langHelper.getCardDisplayName`); search reads both columns (`utils/cardSearchSql`,
`CollectionList`'s filter); logic that must not split a card across languages — the
four-copy deck rule, CSV export, marketplace links — reads `name` only.

Scryfall hands over both for free: `name` is the card's English name and
`printed_name` the name as printed on that specific printing.

A copy's language is chosen separately from the card that was picked — Quick Add's
dropdown, or a scan the English catalog answered — so `cardApi.printingInLanguage`
swaps the row for that language's printing inside `addCardToCollection`, which every
add path routes through. Resolution is by set + collector number, which is
language-invariant: the same physical card has the same number in every language.
Null means keep what was picked: a card never printed in that language.

### Image identification pipeline

Image-only, no OCR. Two ONNX models find and embed the card; only the catalog
differs per language.

The browser does the first half. `utils/detectWorker.js` runs **cornelius**
(384×384, ~4.2 MB, fetched once from `GET /models/cornelius.onnx`) through
`onnxruntime-web` on a worker thread to draw the live outline, then
`CameraScanner.localDewarp` perspective-warps the captured frame to a 448×448
square using the shared `shared/imgproc.mjs` and uploads only that. Two reasons:
the previous version posted a JPEG per preview frame (~2.7 MB per minute of
pointing the camera at a card), and the outline on screen is now *by
construction* the crop that gets matched. Detection is ~80 ms per frame on the
wasm EP — name the EP explicitly, WebGPU measured 1075 ms for this model.

Server side, `cvScan.match(buffer, 'mtg', topK, opts)`:

1. **Dewarp.** An already-rectified upload (`cropped: true`) is only resized to
   448 — re-running cornelius on a crop that already *is* the card would find the
   same square again for the price of a decode and a forward pass. A whole frame
   goes through `detectAndDewarp`: cornelius on a 384 copy, then a homography onto
   a 448 square sampled from a 1200px decode. Below a sharpness of 0.02 the corner
   peaks are flat — nothing card-like in frame — and the raw frame is matched
   instead, which still recovers most of those.
2. **Embed.** **milo** turns the 448 square into a 128-d L2-normalised vector.
   One crop, one forward pass, reused by every catalog swept.
3. **Sweep.** A brute-force dot product against each catalog (both sides are unit
   vectors, so the dot product *is* the cosine). 21,775 rows costs ~6 ms, which is
   why there is no ANN index here: building one would cost more than it saves.
4. **Rank.** Hits from every catalog merge into one list sorted by score —
   comparable because it is the same model and the same normalisation — deduped by
   id, and cut to `topK`.

The route (`POST /api/scan-match`) hydrates each candidate from `card_cache`,
re-expresses it in the scanned language, and `CameraScanner` gates the result:
auto-add above the confidence bar, otherwise the candidate list for a manual pick.

#### Language is a fallback chain, not a filter

Artwork is identical across languages, so any catalog can answer *which card this
is*; only the printing differs. `loadAll` therefore sweeps the catalog for the
scanned language **and** the English one, and the route re-expresses the winner
(`getPrintingInLang` — by set + collector number, which is language-invariant)
before it reaches the picker.

That second sweep is not a nicety. A non-English catalog is only as complete as
its provider: Scryfall serves card records for only part of each language's sets,
so a catalog holds fewer cards than the language actually has. A cosine sweep
never returns nothing, so every card outside those sets would come back as the
nearest of the cards that *are* there, sometimes confidently. The English catalog
has a row for nearly all of them. The right card in the wrong language beats a
wrong card in the right one.

#### Set scoping is a filter, and it is per catalog

Passing set ids skips every row that does not belong *before* scoring: the whole
point of scoping is that a runner-up from an unwanted set can no longer outrank
the right card. There is nothing to build — the ORB path needed a per-set index
first, which was the client's old "preparing set" wait.

The filter is evaluated per catalog, because set ids do not survive a language:
`SV4a` names no row in the English catalog. A catalog with no rows in scope is
**dropped** from the sweep rather than searched unscoped, since searching it
unscoped would reintroduce exactly the wrong-card answer the scope exists to
prevent. If no catalog has rows in scope the filter is ignored entirely — "no
match" for a card that is plainly there is the worse failure.

#### "Nothing here is your card"

A sweep always returns its nearest row, so a card the catalog has never heard of
arrives in the same shape as one it has. The gate is how far the winner stands
above **its own catalog's** ranks 2–11 (`GAP_FLOOR`, default 0.10, env
`CV_SCAN_GAP`) — not its absolute cosine, because absolute cosine tracks photo
quality and the gap does not. Measured by the scan-floor harness over 60
cards per catalog, each searched with its own row masked out so that it *is* a
missing card:

| strangers accepted | reference-quality input | blurred / tilted / dim input |
| --- | --- | --- |
| absolute cosine ≥ 0.65 | 31–41 of 60 | 15–19 of 60 |
| gap ≥ 0.10 | 12 of 60 | 11–12 of 60 |

One threshold, same behaviour on a good photo and a bad one, across catalogs of
very different sizes, and no correct answer was rejected in any of the runs. The
gap is measured within a single catalog on purpose: the same card sits in the
English catalog and in its own language's one, and its own twin a rank down in a
merged sweep would flatten the neighbourhood and make every correct answer look
like a stranger.

When it trips, the response carries `notInCatalog: true` **and** the candidates.
The client refuses to auto-add and says the card is not in the catalog, but still
shows the list: a bad photo and a missing card look identical from here, and one
of the candidates is right often enough to be worth the glance. The strangers
that do get through are cards whose *artwork* is reprinted elsewhere — right art,
wrong printing, which no similarity gate can separate and which the client's
same-name check already routes to the picker.

### Why embeddings replaced the ORB stack

The previous pipeline was a 64-bit dHash sweep plus a bag-of-visual-words lookup
for recall, then ORB descriptors with a RANSAC homography to verify. Measured
against CollectorVision on the same 100-card noisy MTG sample:

| pipeline | exact printing | right card | latency |
| --- | --- | --- | --- |
| hash 250 + BoVW 10 + ORB verify | **78.0%** | 88.0% | 1187 ms |
| cornelius + milo | 76.0% | **90.0%** | 310 ms |

Two points of exact printing for 3.8× the speed — and the reason to switch is
what went with it. ~2.6 GB of per-set ORB indexes plus two collection-wide rollups
became two ONNX files and one catalog per language at ~5 MB per 10k
cards. There is no index build in the scan path at all, so set-scoped scanning
needs no preparation and a scan has no geometric verification stage to be slow in.

Both models are AGPL-3.0 ([milo](https://huggingface.co/HanClinto/milo),
[cornelius](https://huggingface.co/HanClinto/cornelius)) and Scrybox is MIT.
Shipping them enabled is a licensing decision, not only a technical one.

Test-time augmentation (two extra dewarps at 0.92×/1.08× crop tightness, averaged
as unit vectors) took exact printing from 76% to 81% with right-card unmoved, for
two more forward passes — ~100 ms of a ~255 ms scan. Removed for latency; the git
history has it if that trade ever looks different.

### Catalog builds

A catalog is one language, and building it has two phases:

1. **Cache** — walk every set Scryfall lists for that language and pull its
   cards into `card_cache` (`cardSets.cacheSetCards`).
2. **Embed** — run every cached card's artwork through milo and write the
   embedding table the scanner sweeps (`milo-mtg[-<lang>]-local.bin`, plus a
   `.json` carrying ids, dimensions and source urls).

They are one job rather than two buttons because phase 2 can only ever be as
complete as phase 1 — and phase 1 is the half that was missing for years.
Caching used to happen only as a side effect of building a scan index, so a set
nobody indexed, searched or browsed simply was not there.

Both phases resume. Phase 1 is idempotent; phase 2 keeps every embedding whose
**embedded** source url is unchanged, and a cancelled build still writes what it
has, because a partial catalog is valid and resuming reuses all of it. A set with
no data in the chosen language raises an *absent* error rather than a failure —
per-language provider coverage is patchy enough that counting gaps as failures
would abort every non-English build partway through.

Settings → Scan catalogs drives it (`/api/admin/catalogs`, admin-only) and lists what
exists **with a denominator**, because "built, 9,604 cards" reads as complete and
is not. The English total is counted against the `sets` table (Scryfall-derived,
so it matches `card_cache`); a non-English catalog is only as complete as
Scryfall's own data for that language. A catalog can be perfectly built and still
cover only part of that language's available printings.

The scanner matches card **art**, so a build embeds the image the provider
serves (`card_cache.image_url`). Scryfall's urls are already full-size renders,
so there is nothing to swap at embed time. Because resume keys on the url
actually embedded, a re-uploaded image invalidates the old vector instead of
silently reusing it.

Locally built catalogs are keyed by `card_cache.id`, so every hit resolves by
construction. The published fallback catalog (`milo-mtg.npz`, read by
`npz.rs`) is keyed by the same provider ids, so it also resolves — it is a
dated snapshot rather than this install's cache, and a local build wins when one
is present.

### Measuring the scan gate

`GAP_FLOOR` decides whether an answer is presented as an answer at all, so it is
measured rather than guessed. The scan-floor harness (the Node
`measure-scan-floor.js` script) samples catalog rows evenly (not the first N — ids are ordered by set, so the
first N would measure one set's internal confusability), degrades each card's own
art two ways, and reports two distributions per regime: **genuine**, searched
against the whole catalog, and **impostor**, the same image with its own row
masked out, which is exactly the missing-card case. It then sweeps candidate
thresholds for both the absolute cosine and the gap, printing what each would cost
in strangers accepted and correct answers rejected.

Read the two regimes against each other rather than in isolation: a threshold
whose columns move between them is measuring photo quality, not card identity.
Both are optimistic — neither models glare or a shadow across the art — so prefer
a gate that behaves the same in both over one tuned to either.

### Prices

`utils/priceHelpers.resolveCardPrice(row)` is the single answer to "what is this
worth". It reads the price column matching the row's `printing` — `Holofoil`
or `Normal` — falling back to `price_trend` when that column is empty. Any
query that wants a card's value must therefore select the `cc.price_*` columns
the resolver reads, not just `price_trend`, or a foil silently reads as its
non-foil price in that one view — the failure is invisible, it just reads low.

Where the provider price itself comes from depends on the language, because it
depends on which marketplace sells that printing. Scryfall quotes two:
`prices.usd` (TCGplayer's number) and `prices.eur` (Cardmarket's).

| Rows | Source | `price_source` | Currency |
|------|--------|----------------|----------|
| any language, when Scryfall carries a USD price | `prices.usd` | `scryfall` | USD |
| a printing with no USD price (most non-English printings) | `prices.eur` | `scryfall` | EUR |

Two rules hold that together. **A row is never mixed**: when a printing has no
USD price, the EUR normal *and* foil prices are used together, because a USD normal
next to a EUR foil is a pair nothing can compare. And **nothing is converted** — an
exchange rate is a live number this app has no source for, and a stale hardcoded one
misprices a collection silently — so `price_currency` travels with the row and the UI
prints the matching symbol (`utils/formatPrice.priceText`). Collection totals sum the
currencies as-is; `/api/stats/networth` reports `currencies` so a consumer can tell.

Coverage is a function of the sweep's scope, not of the cache: the daily sweep
(the price-sweep timers over the Scryfall client) runs over the cards the user owns or runs
in decks, one `/cards/collection` request per 75 identifiers, so a browsed-but-
unowned set reads 0.00 until a card from it is added.

`frontend/src/utils/resolveCardPrice.js` mirrors the same per-printing order for
cards not yet saved, so the displayed price matches the one that will be recorded
when the card is added.

### Card ordering

`frontend/src/utils/cardSort.js` is the "how do I order cards" logic (the old storage half — compartments, capacity, slot recommendation,
location rules — was removed with the physical-location feature):

- Sort schemes are structured (name / set-number / price / type-color / language), optionally foil-aware. The scheme list and category orderings live in `shared/sortSchemes.json` and `shared/cardOrder.json`; `frontend/src/utils/cardSort.js` consumes them so list order never drifts between the screens that sort.

---

## Data model (SQLite)

| Table | Purpose / key columns |
|-------|-----------------------|
| `users` | `id`, `username`, `password_hash` (PBKDF2, iterations embedded), `role`, `share_token`, `share_enabled`, `api_key` (read-only external credential) |
| `sessions` | `user_id`, `token`, `expires_at` — Bearer-token auth |
| `card_cache` | Normalized card metadata keyed by provider `id`: `name` (searchable) and `printed_name` (as printed), `language`, `set_id`/`set_name`, `number`, `image_url`, `types`/`subtypes`/`supertype`, `rarity`, `cmc`, `color_identity`, `price_*` with `price_source`/`price_currency`, `tcgplayer_product_id`, `tcgplayer_url`/`cardmarket_url`, `last_updated`. Written only through `utils/cardCache.cacheNormalizedCards`, which upserts — `INSERT OR REPLACE` re-created the row and reset every column outside the provider's own list |
| `collection` | One row per owned stack: `id` (entry_id), `user_id`, `card_id`→card_cache, `quantity`, `condition`, `printing`, `language`, `purchase_price`, `list_type` (`collection`/`trade`), `is_trade`, `notes`, `added_at` |
| `decks` | `user_id`, `name`, `description`, `checked_out`, `checked_out_at`, `created_at` |
| `deck_cards` | Deck contents: `deck_id`, `card_id`, `quantity` |
| `price_history` | Per-card price points over time, powering trend charts |
| `sets` | Set catalog (names/ordering) for dividers and set-scoped scan |
| `app_settings` | App-wide key/value settings (e.g. registration toggle) |

**Entry identity**: a `collection.id` (`entry_id`) uniquely identifies one
owned stack. Features that track individual copies (deck checkout coverage) key
on `entry_id`, never on `card_id` alone.

---

## Frontend

`App.jsx` holds auth state (`token`/`user` in `localStorage` under
`scrybox_*`), installs a `fetch` wrapper that injects the `Bearer` header on
`/api/*` calls and dispatches a logout event on `401`, and tab-routes between
code-split view components. `/share/:token` renders the public view without auth.

| Component | Role |
|-----------|------|
| `Login` | Auth screen (login/register) |
| `Dashboard` | Collection value, net-worth trends, distributions, milestones |
| `AddCards` | Wrapper toggling **CameraScanner** vs **CardSearch** |
| `CameraScanner` | Camera capture, in-browser corner detection + dewarp, POST `/api/scan-match`, confidence gate + manual pick |
| `CardSearch` | Name/number text search against the card API |
| `CardInspectorModal` | Card detail: pricing, types, printing/rarity |
| `CollectionList` | Browse/filter/sort the collection; bulk actions |
| `DeckBuilder` | Deck CRUD, composition charts, draw simulator, checkout/return |
| `CheckoutWizardModal` | Checkout **and** check-in coverage checklist (mode prop): owned vs required vs in-use-elsewhere, with a missing count |
| `CatalogPanel` | Scan catalogs: what is built per language, coverage against the provider's own totals, build/stop with live progress |
| `Settings`, `AdminPanel`, `SharedCollection`, `PriceHistoryChart` | Preferences, user admin, public view, price charts |

Client utils (`utils/`): `cardSort` (shared sort comparators + `sortCardsByOrder`),
`resolveCardPrice`/`formatPrice` (pricing display), `cardPrinting`/`cardRarity`
(badge styling), `langHelper` (Japanese name handling), `cardOptions`
(condition/printing/language enums), `shuffle` (draw sim), `i18n`/`translate` (UI
language, React context rather than a library). Scanning adds
`cardDetector`/`detectWorker` (cornelius on a worker thread), `sharpness` (is the
frame worth capturing) and `autoCapture` (the steady-hand cadence); the geometry
they share with the server lives in `shared/imgproc.mjs`, with the detector itself
in `shared/cardDetectPure.mjs`.

---

## Deck checkout / check-in

Reserving a deck's cards. **Checkout and check-in never move cards in the DB** —
only `decks.checked_out` changes; "available" means owned minus copies already
locked by other checked-out decks.

- `PUT /api/decks/:id/checkout` validates availability and sets the flag.
- `GET /api/decks/:id/locations` returns, per card, `owned_qty`, `required_qty`, `available` (owned minus copies locked elsewhere) and `missing`.
- `GET /api/collection` annotates each entry with `checked_out_qty` (`checkedOutAllocation` greedily allocates checked-out decks' requirements onto owned entries), so the collection marks those copies with an "In Play" badge.
- `CheckoutWizardModal` renders that payload as a per-card coverage checklist; `PUT /api/decks/:id/return` flips the flag and reopens the same modal in reverse (`mode="checkin"`).

---

## Conventions & gotchas

- **The Rust server has no auto-reload**: rebuild and restart `cargo run` after backend changes so new routes/data load. Frontend uses Vite HMR.
- **SQLite runs in WAL mode** — checkpoint/stop before file-level backups so `-wal`/`-shm` are flushed.
- **New card fields must be threaded through the Scryfall client normalization** (`scryfall.rs`) and added to the `card_cache` upsert list — the upserts in `card_cache.rs` only write the columns they name, so a field left out is silently dropped.
- **Scanning needs a catalog**: the two ONNX models identify nothing on their own, and there is no second matcher to fall back to. `/api/scan-match` answers `503 notBuilt` with the fix in the message rather than an empty candidate list, which reads to the user as "your card could not be identified". A catalog is per language and only as complete as the provider's data for that language.
- **Frontend lint is strict**: CI runs `eslint --max-warnings 0`, so unused vars/imports and empty blocks fail the Docker build.

---

## Build, run, test

Setup and Docker deployment: see [README.md](README.md). Quick reference:

- Backend: `cd server && cargo run -p scrybox-server`; port `3001` (`SCRYBOX_BIND` to override). The native deps (clang/libclang, OpenCV, mozjpeg) are the ones `Dockerfile.rust` builds — CI tests in Docker.
- Frontend: `cd frontend && npm run dev` (Vite, port `5173`, proxies `/api` → `3001`).
- Tests: `cd server && cargo test` runs the Rust suites; `cd frontend && npm test` runs the frontend util suites (`node --test` + locale check).
- Lint (matches CI): `cd frontend && npm run lint`.

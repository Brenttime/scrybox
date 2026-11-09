# Scrybox Backend -> Rust Migration Plan

Status: DRAFT (Totoro). Sparks worker review pending.
Scope: `backend/` (Node 22 / Express 4, ~18.5k LOC src, 167 route handlers, 20 route modules)
Already Rust: `local-scryfall/` (axum 0.8, tokio, rayon, roaring) on :8420.

---

## Staging policy (owner directive: STAGED, NOT MERGED)

- All migration work lives on a long-lived branch `rust-migration` (phase branches
  `rust-p0`..`rust-p6` branch off it). **Nothing merges to `main`** and nothing
  deploys to prod (bindarr-host deploys from `main`) without Brent's explicit go.
- Rust server is exercised only in a staging compose stack against a COPY of the DB.
- No PRs opened against `main`; draft PRs into `rust-migration` only.
- Each phase is "done" when staged + tested, not when merged.

## 0. TL;DR

- Most of the workload is I/O-bound: SQLite queries, upstream calls to Scryfall, Moxfield and EDHREC, and static files.
  The CPU-heavy work already runs in native code (sharp/libvips, onnxruntime).
  **Expect 1.5-3x throughput on JSON/SQL routes, 5-10x less RSS, ~10x faster
  cold start and flatter p99 latency. Do not expect large speedups on card scanning.**
- The migration is still worthwhile for: one static binary, a much smaller image, no
  node-gyp/glibc issues with sqlite3, typed SQL, background jobs that cannot
  block request handling, and a single language shared with local-scryfall.
- Strategy: a **strangler fig**. The Rust server takes over the front port and
  proxies every route it has not ported yet to the Node backend. Routes move over one module at a time.
  Contract tests must pass before each cutover, and a feature flag allows rollback per route.
- Rough effort: **10-14 focused weeks**. Six phases, and each one ships on its own.

---

## 1. Will it be faster? Honest assessment

| Path | Where the time goes today | Rust gain |
|---|---|---|
| Collection/deck/list/stats CRUD | SQLite via `sqlite3` (async callback bridge, one connection) + JSON serialization | **Medium.** rusqlite + a connection pool removes the callback bridge and enables parallel reads under WAL. Expect 1.5-3x req/s and lower p99. |
| `stats.js`, `deckPricing`, `cardSort`, `collectionHelpers` | JS loops over large result sets | **Medium-High** for large collections (~20k cards). Pure CPU work that currently blocks the event loop. |
| Card search / `rawQuerySql` / `cardSearchSql` | Query compiler + SQLite | Medium. SQL build cost is small; the gain comes from row decoding. |
| `scryfallApi.js` (1.5k LOC), price sync, set sync | Upstream HTTP plus rate limiting | **~None.** Bound by the network and rate limits. |
| `moxfieldApi.js` | Shells out to `curl-impersonate` for the Chrome TLS fingerprint | None. **Keep the subprocess** (or use `rquest`/`wreq` boring-ssl impersonation later). |
| `cvScan.js` / `catalog.js` | **Pure-JS numeric loops** (`warpRgb` ~600k-sample bilinear warp, `toTensor`, brute-force `searchTopK` cosine over 110k x 128 = ~14M FMAs/scan) + native sharp/ORT. Scan ~255-310 ms; Node serializes concurrent scans on one thread | **High.** 2-5x on kernels via rayon/SIMD, scales with cores under concurrent scans; catalog build 2-5x. ORT itself ~parity. (Corrected by Sparks review.) |
| Auth (`pbkdf2Sync`) | **Synchronous PBKDF2-SHA512 on the event loop** | Real p99 improvement: it moves onto `spawn_blocking`, so a login no longer stalls other requests. (Node could fix this today with async `pbkdf2`.) |
| Static frontend / scan-assets | express.static + compression | Medium (tower-http with precompressed br/gz). |
| Cold start | Node boot + migrations + set sync kick-off | High (ms instead of seconds). |

**Cheap fixes to do first, in Node, before any rewrite:** async pbkdf2, a read connection
pool (or better-sqlite3 in a worker), and moving stats aggregation into SQL. If those
are enough, the remaining case for Rust rests on simpler operations, not speed.

---

## 2. Target architecture

```
                 :3001 / :3443 (TLS)
                        |
              +---------v----------+
              |  scrybox-server    |  axum + tower + rustls
              |  (Rust, new)       |
              |  ported routes ----+--> SQLite (rusqlite, r2d2/deadpool pool, WAL)
              |  fallback proxy ---+--> node backend :3002 (internal, shrinking)
              |  jobs (tokio)      |
              +---------+----------+
                        | HTTP (compose net)
              +---------v----------+
              | local-scryfall     |  unchanged sidecar :8420
              +--------------------+
```

### Workspace layout (new `server/` Cargo workspace; local-scryfall stays separate for now)

```
server/
  Cargo.toml                (workspace)
  crates/
    scrybox-server/         bin: router, TLS, static, proxy fallback, config
    scrybox-db/             pool, PRAGMAs, migrations, typed queries
    scrybox-auth/           sessions, api keys, pbkdf2, OIDC, rate limits
    scrybox-domain/         collection, decks, lists, stats, pricing, sort
    scrybox-upstream/       scryfall, moxfield (curl-impersonate), edhrec, manabox
    scrybox-scan/           ort + image preprocessing + npz index
    scrybox-jobs/           price sweep, set sync, oracle tags, moxfield tick, backup
    scrybox-contract/       shared request/response types (serde) + ts-rs export
```

### Crate choices

| Concern | Pick | Why |
|---|---|---|
| HTTP | `axum 0.8`, `tower-http` (cors, compression, fs, trace, limit) | Same stack as local-scryfall |
| TLS + self-signed | `axum-server` + `rustls`, `rcgen` | Replaces `selfsigned` and the https listener |
| Security headers | custom `tower` layer copying helmet's current CSP/COEP output exactly | Parity, see §6 |
| Rate limit | `tower_governor` | Matches `authLimiter` (20 / 15 min) |
| SQLite | **`rusqlite` (bundled) + `deadpool-sqlite`** | Code relies on PRAGMAs, `PRAGMA table_info` checks and dynamic SQL (`rawQuerySql`), which suits rusqlite better than sqlx compile-time checks. Use 1 writer + N reader connections. |
| HTTP client | `reqwest` (rustls) | Scryfall, EDHREC, OIDC |
| Moxfield | `tokio::process` -> `/usr/local/bin/curl-impersonate` | Keep the proven bypass |
| OIDC | `openidconnect` | Replaces `utils/oidc.js` (495 LOC) |
| Password hashing | `pbkdf2` + `sha2` + `subtle` | **Must verify existing hashes byte-for-byte** (same iterations, 64-byte key, sha512) |
| Image | `fast_image_resize` + `image`/`zune-jpeg` | Avoids a libvips dependency. **The resize kernel must match sharp's output, or the embeddings drift.** Fallback: `libvips` bindings. |
| ONNX | `ort 2.x` (load-dynamic, CPU EP) | Same models from `CV_MODEL_DIR` |
| NPZ | `ndarray-npy` | Replaces `utils/npz.js` |
| Jobs | `tokio::time::interval` + a small scheduler | Behaviour is just setInterval plus daily gates; `tokio-cron-scheduler` is not needed |
| Logging | `tracing` + `tracing-subscriber` | |
| Errors | `thiserror` + an `AppError: IntoResponse` mapping to `{ error: "..." }` | Matches the current JSON error shape |
| TS types | `ts-rs` export to `frontend/src/api/types` | Keeps the frontend honest |

### Should it merge into local-scryfall?
**Not at first.** local-scryfall deliberately runs standalone, with no published ports, and
can be used without Scrybox. Share code through a workspace crate if needed. Revisit
in-process search (to drop an HTTP hop) after phase 6, and only if benchmarks show the hop matters.

---

## 3. Phased strangler plan

Mechanics: Rust binds 3001/3443. Node moves to `127.0.0.1:3002` inside the same
container (or a second compose service). The Rust router mounts its ported routes, and
`fallback` reverse-proxies everything else to Node unchanged (headers, body,
streaming). Per-route kill switch: `RUST_ROUTES=collection,decks,...` (config file
value, not env sprawl). Unlisted routes go to Node.

**Database ownership during the transition:** both processes open the same SQLite
file in WAL mode with busy_timeout = 5000. **Schema migrations stay in Node until phase 6**,
because there must be exactly one migrator. Rust checks `user_version` / table_info at boot and refuses to
start on an unknown schema.

| Phase | Scope | Why this order | Exit criteria |
|---|---|---|---|
| **P0 Foundations** (1.5w) | Workspace, config, TLS, security headers, CORS, compression, static frontend + `/scan-assets`, `/api/health`, proxy fallback, tracing. Port **nothing else**. | Proves proxy transparency with zero risk | Full e2e suite passes **through** the Rust proxy; header diff shows nothing |
| **P1 Auth core** (1.5w) | `middleware/auth` (session + read-only API key + admin), pbkdf2 verify, `/api/auth/*` minus OIDC, rate limiter | Every other route depends on it | auth/authme/bootstrapowner tests pass; existing passwords log in |
| **P2 Read-heavy hot paths** (2.5w) | `collection` GET, `stats`, `sets`, `search-history`, `shared`, card search (`cardSearchSql`, `rawQuerySql`, `localrawrouting`), ETag behaviour, `statsCache` invalidation | Largest perf win, low write risk | p50/p99 bench beats Node; collectionetag, cardsearchsql, collectioncatalogquery pass |
| **P3 Writes + domain** (2.5w) | collection writes, `decks` (1k LOC), `lists`, `precons`, `secret-lair`, `limited`, `rules`, `settings`, `importExport`, `marketplace` | Core business logic; port it after the read model is trusted | All deck*/list*/marketplace* tests pass; golden import/export files match byte-for-byte |
| **P4 Upstream + jobs** (2w) | `scryfallApi` (rate-limited client, price sweep, set sync, card_cache), `oracleTags`, `catalog`, `cardSets`, `cardArt`, `moxfield*` + scheduler, `manaboxSync`, `backup`, OIDC | Background jobs move last because they hold the most state | Jobs run in Rust only (**disable the Node schedulers by flag first**, or both run jobs twice) |
| **P5 Card scan** (1.5w; *may be pulled forward as an internal `scrybox-cv` sidecar behind `SCRYBOX_CV_ENABLED` with in-process Node fallback -- biggest pure-CPU win, lowest coupling*) | `cvScan`, `cardscan`, `scan-match`, model asset versioning | Highest numeric-parity risk | Recognition eval (`scripts/eval-global-index.mjs` fixtures) has **top-1 accuracy equal to or above Node** on the same image set |
| **P6 Decommission** (1w) | Move migrations into `scrybox-db` (versioned, `user_version`), remove Node from the runtime image, remove proxy | | Fresh install + upgrade from the current prod DB both boot; image < 60 MB |

Per-route cutover checklist:
1. Port with the same paths, status codes, JSON shape and field order where the frontend depends on it.
2. Run the contract suite against both backends (§4) and require zero diffs.
3. Enable in staging with `RUST_ROUTES`, then soak for 48h on a real collection.
4. Enable in prod. Keep Node's handler alive until the next phase ends (instant rollback).

---

## 4. Contract parity testing

- **Reuse the existing suites against a base URL.** Make `test/e2e/*.test.js` and the
  HTTP-level unit tests read `SCRYBOX_BASE_URL`. Run every suite twice, once against Node
  and once against Rust-fronted. These remain the source of truth throughout the migration.
- **Differential shadow testing.** Add a dev-only tower layer that replays each GET to Node
  and diffs the JSON (normalising volatile fields: timestamps, tokens). Log mismatches.
  Run it over a recorded HAR of a real session (collection browsing, deck editing, search).
- **Golden files** for import/export (CSV / Moxfield / Manabox formats) and `cardListText`.
- **Port pure-function tests to Rust unit tests:** cardSort, priceHelpers,
  deckPricing, mfxPayload, cardSearchSql, scryfallQuery (shared/). Test against the same fixture JSON in
  `backend/test/fixtures`.
- **`shared/` stays the single source** for sortSchemes/cardOrder/languages JSON. Rust
  loads them with `include_str!` at build time.
- **Scan:** a fixed image corpus produces identical top-k IDs, plus an embedding cosine check of 0.999 or better against Node.

---

## 5. Database

- Keep the **same file and same schema** (`DB_PATH=/app/database/bindarr.db`), with no data migration.
- Copy the PRAGMAs exactly: foreign_keys, WAL, busy_timeout 5000, synchronous NORMAL,
  cache_size -65536, mmap 256MB, temp_store MEMORY, analysis_limit, optimize 0x10002.
- Pool: 1 dedicated writer connection behind a mutex/channel and 4-8 readers.
  Run writes in `spawn_blocking`. Never hold a pool connection across `.await` on HTTP.
- Migrations today are idempotent `CREATE IF NOT EXISTS` statements plus `table_info` probes with `ALTER`.
  In P6, freeze the current schema as migration `0001_baseline` and gate it on `PRAGMA user_version`.
  The baseline step must detect and accept every historical shape (including the legacy
  `pokemon_cards.db` rename and `dbrename` / `legacydepokemon` / `legacyartcleanup` paths).
- Keep `price_history` and `collection_query_cache` semantics identical, since stats depend on them.
- Back up before every phase deploy (`backup.js` behaviour, then a manual snapshot).

---

## 6. Auth and security parity (non-negotiable)

- Sessions table lookup with an `expires_at > now()` check. API key fallback is **GET-only**,
  and API keys are **never admin**. Error messages and status codes stay identical (401 vs 403).
- PBKDF2 verify must accept existing stored hashes. Write a test against a real hash from a
  test DB. Use `subtle::ConstantTimeEq`.
- Generate tokens with `rand::rngs::OsRng` (16-byte hex share tokens, `bnd_` plus a 24-byte API key).
- OIDC: same `oidc_locked:` sentinel, same sub-linking rules (oidc.test.js).
- Rate limiters (four fixed windows in `middleware/auth.js`: auth 20/15 min, search 300/15 min, import 20/h, ...) -- match fixed-window math exactly. Honour `TRUST_PROXY` for the client IP.
- Headers: snapshot helmet + `coep.js` output from Node and assert byte-equality from Rust.
  COEP/COOP matter for scanning (SharedArrayBuffer / secure context).
- Body limits per route: bigJson routes, manabox 40mb, default small.
- CORS: `PUBLIC_BASE_URL` is auto-allowed, same origin list logic.
- Run `cargo audit` + `cargo deny` in CI.

---

## 7. Docker, deploy, rollback

- **During P0-P5:** one image with a Rust binary and Node. A tiny supervisor
  (`entrypoint.sh`) starts both, and Rust is PID-primary. The healthcheck hits Rust
  `/api/health`, which also checks that the Node upstream is alive.
- **After P6:** a `rust:1-bookworm` builder producing a `debian:bookworm-slim` runtime
  (still needs glibc for ORT + curl-impersonate) that contains the binary, frontend/dist,
  shared/, curl-impersonate and gosu. Target under 60 MB compared with the current ~400 MB+.
- Frontend build stage unchanged.
- Leave the volume layout (`/app/database`, `ssl/`, `models/`) unchanged.
- **Rollback levels:** (1) remove the route from `RUST_ROUTES`, (2) pin the previous image
  tag, (3) restore the DB snapshot (only needed after P6 migrations).
- Deploy from `main` as today (bindarr-host). Tag each phase `rust-p0` .. `rust-p6`.

---

## 8. Benchmarks before committing (do these first, ~2 days)

Run against a copy of the real prod DB (~20k cards) on the deploy host:

1. `oha`/`k6`: GET collection (paged + full), stats dashboard, deck detail with
   pricing, card search (5 representative queries), and a login burst while browsing.
   Record req/s, p50, p99 and RSS.
2. Spike: port **only** `GET /api/stats` + collection list to an axum prototype
   (1-2 days) and compare.
3. Scan: 50-image corpus end-to-end latency and top-1 accuracy.
4. Cold start to healthy.
5. Repeat after the §1 cheap Node fixes. **If cheap Node fixes get within 30% on
   p99, deprioritise the full rewrite.**

Go/no-go: proceed past P0 only if the spike shows 2x or better on p99 for stats/collection, or if
operational wins (image size, native build pain) are reason enough on their own.

---

## 9. Effort and risks

| Phase | Est. |
|---|---|
| Benchmarks + spike | 0.5w |
| P0 | 1.5w |
| P1 | 1.5w |
| P2 | 2.5w |
| P3 | 2.5w |
| P4 | 2w |
| P5 | 1.5w |
| P6 | 1w |
| **Total** | **~13w** (single dev; parallel agents can compress P2/P3) |

| Risk | Mitigation |
|---|---|
| Behaviour drift in 1k-LOC route files (decks, collection) | Differential shadowing + golden files; port tests first |
| Two writers on one SQLite during transition | busy_timeout, short transactions, jobs owned by exactly one side via flag |
| Image preprocessing drift breaks scan accuracy | Eval corpus gate; fall back to libvips bindings |
| e2e Scryfall mock patches axios in-process, can't stub Rust's outbound calls | Lift it to an HTTP fixture server both backends point at |
| Rust `panic = "abort"` profile (as in local-scryfall) aborts on a bad tensor | Use `panic = "unwind"` for the server crate + `catch_unwind` around scan; compose restart |
| Moxfield Cloudflare fingerprint | Keep the curl-impersonate subprocess unchanged |
| Hidden coupling via `statsCache.invalidateOnWrite` | Port it as a tower layer in P2; while both sides write, invalidate via a DB row/version counter rather than in-memory |
| Scope creep / never-finished migration | Each phase is shippable; the proxy means stopping halfway is a valid end state |
| AGPL models | Unchanged, still operator-fetched |

---

## 9b. Sparks (worker) review -- dissent noted

Sparks independently recommends a **targeted** port only (CV sidecar ~3-6 wk; full rewrite 5-9 eng-months, ~0 gain on ~90% of routes). Kept this plan as the full-migration path per owner request, but adopted its corrections: CV is the real CPU win, HTTP-level test mocks, panic profile, rate-limiter detail. Raw review: [rust-migration-sparks-review.md](rust-migration-sparks-review.md).

## 10. Recommendation

1. **Week 1:** apply the cheap Node fixes (async pbkdf2, read pool, SQL aggregation for stats)
   and run the benchmarks in §8. This step is cheap and benefits the app no matter what happens next.
2. **If the spike justifies it:** go strangler-fig with P0-P2 first. Those phases capture
   about 80% of the perceived speed (browse, stats, search), and the work can stop there with Node
   still serving the long tail through the proxy.
3. Continue to P3-P6 only if the single-binary/ops win is wanted. A full rewrite
   in one go is **not** recommended.

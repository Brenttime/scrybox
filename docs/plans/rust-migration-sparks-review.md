# Scrybox Backend → Rust Migration Strategy

*Read-only analysis of `/home/brenttime/projects/bindarr`. No files modified. All claims below are grounded in specific files read during this analysis; production data scales are taken from code comments (the checked-in `backend/database/bindarr.db` is a near-empty dev fixture — card_cache=19, collection=0, and 16,726 of 33,909 pages are freelist/dead, so its 139 MB is bloat, not workload).*

## 0. What the codebase actually is

- **~18.5k LOC** in `backend/src` (~31k incl. tests). `server.js:1-617` wires **18 route modules, ~120 endpoints**, an auth gate, a stats-cache invalidator, four body-size tiers (1 MB / 15 MB / 40 MB), `helmet` + `cors` + `compression`, static SPA serving, a `/scryfall-portal/*` proxy, and a second HTTPS listener for camera scanning.
- **Two hot subsystems, very different shapes:**
  - **CPU-bound CV path** — `cvScan.js` (`/scan`, `/search-match`): pure-JS `warpRgb` (bilinear warp over 448×448×3 ≈ 600k samples), `toTensor` (3×448×448 normalize), and brute-force `searchTopK` cosine over a **110k×128 Float32Array ≈ 14M FMAs per scan** (code comments: cosine sweep ~25–60 ms, dewarp ~100 ms, whole scan ~255–310 ms). ONNX inference (`cornelius.onnx` corners 192→1484, `milo.onnx` 448→128) and JPEG decode/resize via `sharp` (libvips) are **already native C**. `catalog.js` runs the same embedding loops over ~100k cards as a batch job.
  - **I/O-bound orchestration** — `scryfallApi.js` (1535 lines): a **serialized async queue paced at 2 req/sec** to Scryfall, retries, a dual-layer (memory + persistent) cache, and a 404-circuit-breaker state machine; plus `moxfieldSync.js`/`moxfieldApi.js` (spawns `curl-impersonate` to defeat Cloudflare JA3), `manaboxSync`, marketplace orders. These are bottlenecked on **external network and deliberate rate pacing**, not JS.
- **Decisive structural fact:** `local-scryfall/` **already reimplements the read-heavy search surface in Rust** — `src/api.rs` serves `/api/search`, `/api/card`, `/api/collection`, `/api/suggest`, `/api/sets`, `/api/set`, `/api/healthz` from an in-memory index (axum + tokio + rayon + roaring, no SQLite at query time, hand-rolled JSON via `push_str`), already running in production behind `LOCAL_SCRYFALL_ENABLED`. **The single biggest "move to Rust" opportunity is already done, via a clean strangler-fig sidecar.** This reframes the whole recommendation.

---

## 1. Will Rust actually be faster here? Honest assessment

| Path | Bound by | Node today | Rust gain | Verdict |
|---|---|---|---|---|
| `cvScan` `warpRgb`/`toTensor`/`searchTopK`/`normalize` | CPU numeric loops, **no SIMD, no threads** | 255–310 ms/scan | **2–5×** (rayon + auto-vectorize) | ✅ real win |
| Concurrent `/scan` throughput | Node's single thread **serializes** the JS loops | 1 scan at a time | **scales with cores** | ✅ headline win |
| `catalog.js` embed phase (100k cards) | same JS loops, batch | minutes | 2–5× | ✅ win |
| ONNX inference | native ORT (`onnxruntime-node`) | native C | ≈0 (same lib via `ort`) | ➖ parity only |
| JPEG decode/resize | `sharp`/libvips | native C | **regress risk** if you use the `image` crate | ⚠️ keep libvips or `turbojpeg` |
| `scryfallApi` search/proxy/paging | external HTTP + **intentional 2/sec pacing** | I/O | ≈0 | ❌ keep Node |
| Moxfield/Manabox/marketplace sync | external HTTP + `curl-impersonate` JA3 | I/O + shell-out | ≈0, and **no Rust JA3 equivalent** | ❌ keep Node |
| Auth middleware | one indexed session query | I/O | ≈0 | ❌ keep Node |
| `stats`/`collection` reads | **SQLite does the aggregation** (30 SQL `SUM/COUNT/GROUP BY` vs 20 light JS shaping loops in `stats.js`) | SQLite C engine | marginal per-request; **concurrency win only** | ➖ see below |

**Two nuances that keep this honest:**

1. **The node `sqlite3` module serializes every statement through one connection's background thread.** Under many *concurrent* users this is a genuine ceiling; `rusqlite`+pool or `sqlx` removes it. But that is a **multi-user concurrency** argument, not a latency argument — and Scrybox is a self-hosted homelab app where one user is the common case. Do not port for this until a concurrency ceiling is actually measured (§8).
2. **The CV path that *would* benefit is already 255–310 ms and "well within the 1-second budget"** (its own comment). Rust optimizes an already-acceptable path — it does not fix a broken one. It also lowers RSS (56 MB catalog + embeddings held once) and lets concurrent scans scale, which are the more defensible wins than p50 latency.

**Bottom line:** Rust is a genuine, measurable win for **CV/scan + catalog build** and *maybe* for **multi-user DB reads**. For the other ~90% of routes (16.5k of 18.5k src lines — scryfall/moxfield/manabox/auth/CRUD/stats) the gain is ~0 and the parity risk is real.

---

## 2. Target architecture

**Do not merge into `local-scryfall`.** They have deliberately opposite data models: `local-scryfall/src/index.rs:1` and `main.rs:1` state "no SQLite, no disk at query time" — it is a *public-data, memory-only, unauthenticated* search engine. The Scrybox backend is *SQLite + auth + private user data*. Merging would couple public card search to user auth/collection writes and violate the sidecar's clean invariant. Instead **create a sibling Rust crate** that copies the proven `local-scryfall` deployment pattern.

```
scrybox-rs/                      # NEW Cargo workspace (repo root), separate binaries
  Cargo.toml                      # [workspace] members = ["cv", ...]; shares lockfile + utils
  cv/                             # scrybox-cv — the ONLY port with real payoff
    axum, tokio                    # reuse sidecar's stack/version pins
    ort = "2"                       # ONNX Runtime (same native lib onnxruntime-node wraps); CPU EP only
    turbojpeg                       # libjpeg-turbo SIMD decode (do NOT use the pure `image` crate resize)
    rayon                           # parallelize warpRgb/toTensor/searchTopK
    npz reading (zip + .npy)        # port utils/npz.js
    NO auth, NO SQLite writes       # internal-network only, read-mostly
```

| Concern | Choice | Why |
|---|---|---|
| Web | **axum** + **tokio** | matches the existing sidecar; version pins already proven here |
| ONNX | **`ort`** (pyke) | wraps the same ONNX Runtime as `onnxruntime-node`; CPUExecutionProvider; `download-binaries` or system lib — same glibc caveat the Node Dockerfile documents |
| Image decode | **`turbojpeg`** (libjpeg-turbo) | competitive with libvips on decode; `image` crate is *slower* — using it would regress the sharp path |
| Numeric kernels | **`rayon`** + `#[repr(C)]` f32 slices | the actual win (warp/tensor/cosine) |
| DB (if ever read-ported) | **`rusqlite` + r2d2 pool** | WAL lets Node stay the single writer while Rust opens many read conns; sync API fits the numeric code. Choose `sqlx` only if the whole app server is ever ported |
| Cron | **not needed** for CV (admin-triggered, not cron) | if I/O syncs were ever ported: `tokio-cron-scheduler` |
| JSON | **`serde_json`** + hand-built hot responses | mirrors `local-scryfall`'s `push_str` fast paths |

**Deployment shape:** `scrybox-cv` is an internal-network-only compose service (no published port), healthcheck `/healthz`, env-gated `SCRYBOX_CV_ENABLED`/`SCRYBOX_CV_URL` — **the exact pattern `docker-compose.yml:36-56` already uses for `scryfall-local`**. Node authenticates, then forwards `POST /api/cardscan/detect|scan` and `POST /api/scan-match` to it, and **falls back to in-process `cvScan` when the flag is unset** (instant rollback, mirrors the LOCAL_SCRYFALL fallback at `server.js:410`).

---

## 3. Strangler-fig phased plan (route-by-route, value/risk ordered)

Node remains the **only front door and the only SQLite writer** throughout. Rust services are internal, feature-flagged, and always behind a Node fallback.

| Phase | Routes / units | Value | Risk | Action |
|---|---|---|---|---|
| **0. Spike + benchmarks** | none shipped | de-risk | none | Stand up `scrybox-cv`, run §8 benchmarks + equivalence scripts on fixed images. **Gate: only proceed if searchTopK+warpRgb ≥2× and decode doesn't regress.** |
| **1. CV scan** (the payoff) | `POST /api/cardscan/detect`, `POST /api/cardscan/scan`, `POST /api/scan-match` (`cvScan.js`, `routes/cardscan.js`, `utils/npz.js`, `coep.js`, `modelAssets.js`) | High (CPU + concurrency + RSS) | Med | Port + Node proxy + flag + in-process fallback. Canary via dual-run (§4). |
| **2. Catalog build** | `POST /api/admin/catalogs/build` (`catalog.js`) | Med (batch CPU) | Low | Same crate, admin-gated, restartable. Proves the embedding path on 100k cards. |
| **3. Pooled SQLite reads** (conditional) | `GET /api/stats`, `GET /api/collection`, price reads | Only if multi-user | Med | **Do only if §8 shows the node-sqlite3 single-conn ceiling is hit.** rusqlire read-pool, Node still writes. |
| **4. ❌ Do NOT port** | `scryfallApi` search/proxy/paging; `moxfield*`, `manabox*`, `marketplace*`; `auth`/`oidc`/`admin` users; CRUD `decks`/`lists`/`collection` writes; static/SPA/portal/TLS | ≈0 | High | **Keep in Node.** Negative-value rewrites. Moxfield's JA3 curl-impersonate has no Rust equivalent. |

Ordering is by **value-per-risk**: Phase 1–2 (CV) carry the entire benefit and are self-contained; Phase 3 is gated on evidence; Phase 4 is explicitly a "do not do," documented because it's tempting.

---

## 4. API contract parity testing (reusing existing e2e)

**The one thing that breaks naive reuse:** the e2e Scryfall mock (`test/e2e/scryfall-mock.js`) monkey-patches **`axios` inside the Node process** — it can only intercept Node's outbound calls, so it **cannot stub Scryfall for a Rust backend**.

**Fix:** lift the Scryfall stub to an **HTTP-level fixture server** that serves the same `test/fixtures/manapool/*.json` and Scryfall-shaped responses on localhost. Point **both** backends at it (`LOCAL_SCRYFALL_URL`/`SCRYFALL_API_URL`), so the same assertions run against Node and Rust.

Reuse strategy:
- **e2e runner as a dual-backend gate.** `test/e2e/run.js:24` spawns the server via env (`PORT`, `DB_PATH`, `HTTPS_PORT`, `DEFAULT_ADMIN_PASSWORD`). Extend it to spawn **either** `node server.js` **or** the Rust binary; the same `F1-TCn` files then certify both.
- **Unit tests as the golden spec for ported pure logic.** `test/*.test.js` are plain-`assert` in-process scripts (one child each, `test/run.js`). For each ported kernel (npz dtypes, warp math, cosine top-K), port the *same input fixtures* and assert Rust output matches within float tolerance.
- **CV equivalence harness already exists** — `backend/scripts/scan-equivalence.mjs`, `lanczos-equivalence.mjs`, `measure-scan-floor.js`, `client-scan-replay.mjs`. Feed fixed test images through Node and Rust; assert embeddings and top-K **oracle-id lists match** (tolerance on scores, exact on identity).
- **Shadow / dual-run canary.** Before flipping the flag: Node serves the real response, asynchronously mirrors the request to Rust, and **diffs status + normalized JSON** (float-jitter tolerance for embeddings) with divergence logged + metric'd. Ship Phase 1 to a canary user; flip `SCRYBOX_CV_ENABLED` only when divergence < threshold.
- **Header/security parity assertions** in e2e: assert byte-identical `helmet`/CSP/COOP/COEP, `Cache-Control`, and CORS behavior for the proxied routes (see §6).

---

## 5. DB / schema / migrations

- `db.js:44-...` `initDb` is **imperative JS**: a big `CREATE TABLE IF NOT EXISTS` block plus **dozens of `try{ALTER TABLE…}catch{}` idempotent migrations** + indexes + FTS5 virtual tables. This soup is *not* reproducibly ported to Rust versioned migrations — trying to dual-own it causes drift.
- **Keep Node schema-authoritative.** `scrybox-cv` opens the SQLite volume **read-only** (WAL permits one writer + many readers) and **runs zero migrations**. It never writes. This sidesteps schema drift entirely and is the safe default for a CV sidecar that mostly needs model assets, not mutable user rows.
- **If Phase 3 (read-pool) is later justified:** still read-only rusqlite against WAL; no migrations.
- **Only if the whole app server were ever ported** (not recommended): adopt `sqlx::migrate!` / `rusqlite_migration`, seed from a **VACUUMed snapshot** of the current schema (reclaiming the 49% freelist), and retire the `try/catch` ALTERs behind explicit versioned migrations — a one-time, reviewed schema freeze.

---

## 6. Auth / security parity

If the CV service is **internal-network-only, no published port** (the `scryfall-local` model, `docker-compose.yml:32-35`), **it needs no auth at all** — Node authenticates and forwards. That is the right move and shrinks the security-port surface to zero. The `try/catch` migration and auth concerns stay wholly in Node.

Only if some Rust service is ever *exposed* must it replicate, exactly:
- **`helmet` header set + the COOP/COEP nuance** — `server.js:104-149` (COOP same-origin, CSP, `upgradeInsecureRequests` gated off for self-signed TLS so Chrome keeps its "proceed anyway" bypass) and the **portal's selective header-stripping** (`server.js:512-515` removes CSP/COEP only for the portal subtree). Byte-exact header parity is the trap; the frontend needs SAB (SharedArrayBuffer) headers for camera scanning.
- **CORS** allowlist + `PUBLIC_BASE_URL` auto-allow (`server.js:191`).
- **Four `express-rate-limit` fixed windows** (`middleware/auth.js:75-102`: auth 20/15 min, search 300/15 min, import 20/h) + **`TRUST_PROXY`**/`X-Forwarded-For` so `req.ip` is the real client. Fixed-window boundary semantics differ across Rust limiter crates — match the window math and cover with tests.
- **Body-size tiers** 1 MB / 15 MB (`bigJson`) / 40 MB (`manabox`) — mirror with axum `DefaultBodyLimit` per-route.
- **Credential crypto:** PBKDF2-SHA512 at the same iteration count with `timingSafeEqual` (`authHelpers.js:21-23`) → Rust `pbkdf2`/`ring` + `subtle::ConstantTimeEq`; `randomBytes` tokens (`authHelpers.js:27`, API key `bnd_`+24B at `auth.js:339`, share token 16B); **API-keys-GET-only** and **no-API-key-for-admin** rules (`middleware/auth.js:31,64`). Self-signed cert generation (`tls.js`, `selfsigned`) → Rust `rcgen`, keeping the 397-day Safari cap and SAN-from-interfaces logic.

Recommendation: **keep all of this in Node** by keeping the Rust services internal-only.

---

## 7. Docker / deploy / rollback

Clone the `scryfall-local` template verbatim (`local-scryfall/Dockerfile`, `docker-compose.yml:36-56`):
- **Dockerfile:** `rust:1-slim` builder → `debian-slim` runtime (**glibc, not musl** — `ort`/libvips hit the exact landmine the Node Dockerfile documents at `Dockerfile:22-27`). Bake ONNX Runtime, libjpeg-turbo (+libvips if kept). Note the existing `Cargo.toml` uses `panic = "abort"` — a bad tensor **aborts the process**, so run under a supervisor / per-request catch and let compose `restart: unless-stopped` recover.
- **Compose:** new `scrybox-cv` service, **internal network only, no published ports**, `healthcheck` `/healthz`, `start_period` sized to model/catalog cold-load (the Node sidecar uses 90 s for a ~10 s index load — the 56 MB catalog load needs measuring, §8).
- **Models on the persisted volume** (`CV_MODEL_DIR`), AGPL note preserved (models fetched by operator, not baked — `Dockerfile:68-71`).
- **Release/rollback:** feature flag `SCRYBOX_CV_ENABLED`. ON → Node proxies to Rust; OFF/unset → Node uses in-process `cvScan`. **Rollback = unset the env var**, no data migration (no schema change), no image revert. Canary via §4 shadow mode; blue/green is just both scan paths running and being diffed.

---

## 8. Benchmarks to run *before* committing (gates)

Use `criterion` for kernels, `k6`/`vegeta`/`hey` for HTTP. Commit only if all pass.

1. **Kernel micro-bench** (`criterion`): `searchTopK` (110k×128 cosine), `warpRgb`, `toTensor` — **require ≥2× Rust vs Node**.
2. **Concurrent `/scan` throughput** — the headline: Node serializes its JS loops on one thread, so scan/s should plateau at ~1; Rust+rayon should scale with cores. This is the strongest justification — measure it.
3. **ONNX parity** — `ort` vs `onnxruntime-node` session throughput on `cornelius` + `milo` (same native lib ⇒ expect ≈equal; must not regress).
4. **Decode** — `turbojpeg` vs `sharp` on a ~12 MP camera JPEG (must not regress; if it does, keep decode in Node or use libvips binding).
5. **RSS** — Rust CV resident set vs Node with 56 MB catalog + 100k×128 embeddings loaded (should be lower; confirm it fits).
6. **Cold start** — model + catalog load time (drives `start_period`).
7. **DB concurrency (only if Phase 3 is on the table)** — hammer `GET /api/collection` + `GET /api/stats` with N readers; confirm the node-sqlite3 single-connection serialization is actually the ceiling *before* investing in the DB port.
8. **End-to-end scan latency p50/p95/p99** via existing `measure-scan-floor.js` — confirm the 255–310 ms budget improves and the 1 s ceiling is never approached.

---

## 9. Effort estimate and risks

**Effort (one experienced Rust dev):**
- **Full rewrite** (all 18.5k src + 12.7k test LOC): **~5–9 engineer-months**, high regression risk across ~120 endpoints, ~0 perf gain on ~90% of routes. **Not justified.**
- **CV sidecar (recommended):** ~**3–6 engineer-weeks** — ~1.5k LOC of real logic (`cvScan` 586 + `catalog` 524 + `npz` 130 + coep/model glue) + ONNX/image/turbojpeg wiring + Docker + parity harness + canary.
- **Conditional DB read-pool:** ~1–3 weeks, only if §8 shows a real ceiling.

**Risks (with mitigations):**
- **Float divergence changing scan matches** — the highest-value risk. Mitigate with golden images + the equivalence scripts + identity-exact / score-tolerant diffing + shadow canary (§4).
- **Native-lib landmines** (ORT, libvips, glibc, musl) — already documented in `Dockerfile:22-93` for Node; Rust hits the same. Mitigate: glibc base, pin versions to the sidecar's, build in CI.
- **`panic=abort`** in the existing release profile — a malformed tensor aborts the process. Mitigate: per-request `catch_unwind`/supervisor + compose restart + healthcheck.
- **`image` crate regression** — do not use it for resize; use `turbojpeg`/libvips.
- **Parity drift** as two codebases evolve — mitigate with a shared fixture/contract suite run against both backends in CI.
- **`curl-impersonate`/Cloudflare JA3** (Moxfield) has **no Rust equivalent** — an explicit reason *not* to port the syncs.
- **MIGRATION SOUP** (`try/catch` ALTERs) — keep Node schema-authoritative, Rust read-only.

---

## 10. Recommendation

**Targeted port, not a rewrite.** Keep **Node as the application server** — it is correct and optimal for the I/O-bound (`scryfallApi` 2/sec pacing, Moxfield/Manabox/marketplace sync), SQLite-bound (stats/collection aggregates run in SQLite's C engine), auth, and CRUD surface: ~90% of routes and ~16.5k of 18.5k src lines. Node's event loop already saturates those paths, and several depend on Node/shell specifics (`curl-impersonate` JA3, the 404-circuit-breaker, the serialized rate queue) that a Rust rewrite would regress for zero benefit.

**Capture the only genuine Rust win the way the repo already proved works:** continue the `local-scryfall` strangler-fig pattern and extract the **CV/scan + catalog-build** CPU path — the pure-JS `warpRgb`/`toTensor`/`searchTopK` and 110k×128 embedding sweep — into a **sibling Rust crate `scrybox-cv`** (axum + tokio + `ort` + `turbojpeg` + `rayon`), deployed internal-network-only behind a feature flag with an in-process Node fallback, exactly like `scryfall-local`. That yields **2–5× on the numeric hot path, core-scaling under concurrent scans, and lower RSS** — at roughly **⅛ the effort and risk** of a full rewrite, while never touching the paths where Node is already optimal.

Do the SQLite read-pool **only if** a measured multi-user concurrency ceiling justifies it. Explicitly reject the scryfall/moxfield/manabox/auth/CRUD rewrites as negative-value.

**One-line answer to your core question:** Rust is real here — but *only* for the CV hot path and (maybe) concurrent SQLite reads; for everything else it's a rewrite tax with no speedup. Port CV as a sidecar, keep Node in front.

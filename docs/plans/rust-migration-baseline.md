# Rust migration -- Node baseline (plan section 8, step 1)

Measured 2026-11-09 by the kickoff job. **Staged on `rust-p0`; not merged, not deployed.**

## Setup

- Host: ThousandSunny (AMD Ryzen 3 3200U, 4 threads). It is shared with the live prod
  container, so treat the numbers as a relative baseline, not absolute capacity.
- DB: **copy** of `bindarr-host/predeploy-42028e6.db` (2026-10-24 snapshot of prod, 24,718
  collection rows for one user). In the copy only, the bench user's password was reset so a
  login could be scripted. The prod DB was never opened.
- Node: `backend/` at origin/main `064ddac`, Node v22.23.4,
  `PORT=3911 TRUST_PROXY=true CATALOG_AUTO_UPDATE=0`, no HTTPS listener and no local-scryfall sidecar.
- Load: `server/bench/http-bench.mjs` (closed-loop, keep-alive, 10 s per scenario, 3 warm-up
  requests). Each request rotates `X-Forwarded-For` so the express-rate-limit windows do not
  turn the run into a 429 benchmark. `oha` and `k6` are not installed on this host.
- Run on a fully booted, idle server. An earlier run, made during boot jobs (Moxfield boot sync and
  writers), got `SQLITE_BUSY` 500s on login and roughly half the throughput. It was discarded.
  That gap is itself evidence of the single-connection ceiling.
- Raw JSON: `server/bench/results/2026-11-09-*.json`.

## Node baseline (direct, :3911)

| Scenario | Conc. | req/s | p50 ms | p99 ms | Peak RSS MB |
|---|---|---|---|---|---|
| `GET /api/health` | 16 | 2020.4 | 7.18 | 18.10 | 924 |
| `GET /api/collection?limit=100` | 16 | 29.6 | 533.4 | 666.1 | 946 |
| `GET /api/collection` (full, 27 MB JSON) | 2 | 0.9 | 2189 | 2595 | 1315 |
| `GET /api/stats` (statsCache warm) | 16 | 1149.1 | 13.32 | 21.57 | 1175 |
| search `scope=collection q="t:creature c:g"` | 16 | 13.6 | 1094 | 1901 | 1154 |
| search `scope=collection q="sol ring"` | 16 | 14.9 | 1026 | 1510 | 1161 |
| search `scope=database name="Sol Ring"` (card_cache) | 16 | 10.9 | 1425 | 1967 | 1165 |
| `POST /api/auth/login` burst (pbkdf2 210k) | 4 | 14.5 | 269.5 | 358.5 | 1172 |

- Idle RSS after boot was ~660 MB. RSS stays at ~1.4 GB after the run (V8 heap growth from the full-collection payload).
- Cold start (process spawn to `/api/health` 200): **~1.5 s**.
- Takeaways: paged collection runs at ~30 req/s, and collection-scope search at ~14 req/s with a p50 above 1 s
  at 16 concurrent. These are the real hot spots. Stats is cheap only because statsCache is warm.

## P0 Rust front proxy (:3912 -> Node :3911), same scenarios

| Scenario | req/s | p50 ms | p99 ms | Rust RSS MB |
|---|---|---|---|---|
| health (proxied to Node) | 1827.2 | 7.80 | 19.98 | 6.5 |
| collection page 100 | 28.8 | 541.2 | 719.5 | 9.4 |
| collection full | 0.9 | 2319 | 3045 | 11.7 |
| stats | 1095.6 | 13.9 | 22.41 | 10.3 |
| search t:creature c:g | 13.7 | 1124 | 1568 | 11.3 |
| search sol ring | 14.3 | 1081 | 1698 | 12.2 |
| search db Sol Ring | 10.1 | 1533 | 1857 | 11.3 |
| login burst | 13.8 | 286.9 | 352.9 | 9.8 |

The proxy hop costs about 0.5 ms at p50 on cheap routes and is within run-to-run noise on heavy routes. The Rust process stays at
about 10 MB RSS. Header parity was checked with case-insensitive, sorted diffs of Node direct vs proxied responses for `/api/health`,
`/api/stats`, `/api/collection?limit=100`, `/`, `/collection`, search, and an unknown `/api/nope`. The only differences are
the hop-by-hop `connection`/`keep-alive` headers and the shared `ratelimit-remaining` counter. Bodies are byte-identical.

## Not measured (blockers)

- **Scan latency:** not measured. Prod `CV_MODEL_DIR` has `cornelius.onnx` but **no `milo.onnx`
  or milo catalog**, so the server-side `cvScan` path is not built on this install. Scanning currently
  runs client-side. Measuring it needs `scripts/fetch-models.mjs` and a catalog build in a staging
  volume, which was out of scope for an unattended run.
- Deck detail with pricing, and the 50-image scan corpus (section 8 items 1 and 3): to do.

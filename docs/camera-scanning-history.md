# Scrybox camera scanning: history, results, lessons

Last updated: 2026-09-27. Live image: `scrybox:8a84d94-scansettle`
(branch `perf/scan-settle`, not merged to main).

## Goal and rules
- Scan a card in under 1 s from placing it to adding it.
- Exact printing only: name + set + collector number must uniquely
  prove the printing. Never guess. Art similarity may narrow the options
  but must never confirm a printing.
- Clients: Windows desktop, iOS, Android, all in the browser (WASM/WebGL,
  no native app). The client does most of the work; the server is the
  fallback.
- No extra request per frame, no auto-zoom, no stillness gate that blocks
  the user (the settle window below is short and automatic).
- A speed change ships only if the saved-frame replay gives identical
  matches and no wrong cards. gpt-6-astra does a read-only review of
  every change. Nothing goes live without approval.

## Architecture timeline
1. Upstream Bindarr: ONNX scan pipeline (two models) in
   `CameraScanner.jsx`, server-side. Big component, slow and fragile.
2. `cardscan` sidecar (repo `~/projects/cardscan`): Python OCR service.
   - Staged footer OCR (~45% faster), one-shot `/api/scan-frame`
     (detect + warp + OCR from one decode), identity cache, retro-frame
     collector numbers, DFT collector line, wide footer fallback.
3. Scan Cards v1 (`FastScanner.jsx`, PR #42): sidecar integration;
   photo upload removed.
4. On-device scanning (PR #46): in-browser title + collector OCR
   (cornelius corner model + PP-OCR rec in a Web Worker, ORT WASM),
   with server fallback.
5. Speed/UX (PR #48, #49): Single/Auto modes, hedged server read, blur
   gate, merged footer stage, cross-frame footer evidence (pooled reads,
   padded-number vote), tap to change printing, tray qty/foil, undo,
   "bring closer" hint, no zoom.
6. Audit fixes (PR #52): exact-printing safety, worker recovery and
   timeouts, loop generation tokens, abort in-flight fetches on stop,
   USD-only prices, sidecar cancel, repeated-failure backoff. The old
   `CameraScanner` was removed.
7. Telemetry (`49fba49`): per-pass client timings, raw title OCR
   (including rejected reads), gate state and outcome, stored in
   `database/scan-telemetry/*.jsonl`.
8. Settle + speed + corner fix (`7878bbf`..`8a84d94`): see below.

## Measurements
- 61 saved Windows frames (debug dir): baseline client 11, server 9,
  neither 47. Client and server both answered on 6 frames and agreed on
  all of them.
- Worst reported case (~3 s): the client read failed (~1.3 s), then the
  server read the same frame one after the other (~1.65 s).
- 39 of the 47 frames neither side identified never produced a title.
  Webcam motion blur passed the phone-tuned blur check.
- Old-frame collector check cost ~380 ms on every failed read, even
  for FRA cards.
- PC telemetry, 23 cards (25 adds; Apex Witchstalker and Arcane
  Amphisbaena added twice, cause unconfirmed):
  - Card appears to added: median 1.3 s, 90th percentile 2.5 s, max
    4.9 s. Only 9 of 25 under 1 s.
  - The PC made every identification; the server went 0 for 14.
  - A read that succeeds takes ~0.29 s (title only) or ~1.0 s (title +
    collector).
  - About 2/3 of the wait is spent before the successful read: the
    first "still" frame is still smeared, the PC wastes ~0.52 s on it,
    then the server wastes ~0.35 s more.
  - The PC ran OCR on 1 thread (16 available). The collector-number
    read alone took ~0.43 s.

## What worked
- On-device OCR as the main path. The server never won on PC frames.
- Telemetry that logs raw rejected title OCR. It exposed "ley Reception"
  and the smeared-first-frame problem.
- Settle window: 3 fresh steady, sharp frames before OCR. A blurry or
  clipped frame resets the count. It still works when frames arrive
  slowly.
- Auto skips the server when the title is junk. The server gets one try
  after 4 junk reads in a row, and the shutter always uses the server.
- Deferring the deep footer stages to the next frame on the first look
  (never twice in a row, and never settling a printing before them).
- Exact-title fast path: skip the second title read when every read is
  the same exact, confident match. Reprints still need the collector
  number.
- 2 ORT threads on desktop through
  `Cross-Origin-Embedder-Policy: credentialless`. Browser-verified:
  isolated, SIMD on, Scryfall images and Google Fonts still load.
  Browsers without support fall back to 1 thread.
- **Lanczos corner refinement (the big one).** The browser's canvas
  `drawImage` 384 px downscale moved the cornelius corners a few px,
  which broke footer proofs: 4 of 11 printings in browser conditions
  versus 11 of 11 in the Node/sharp replay. Re-detecting on a
  `resizeLanczos3` copy before OCR restored 11 of 11 in the live
  browser worker. Cost ~140-170 ms, only on frames that get OCR'd.
  - Bounded: the refined corners must stay within `STILL_DRIFT` of the
    gated ones, off the frame edge and sharp, or the gated corners are
    kept.
  - A tracked card returns from the cache before refinement, and its
    art signature is taken from the region OCR reads.
- Showing the confirmed card before hydrating details/prices (one fewer
  round trip in the perceived time).
- Pooled footer evidence across frames and the padded-number vote.

## What did not work / rejected
- Stepwise canvas halving and area-average downscale as the corner fix:
  3 of 11 and 7 of 11. Only Lanczos matched sharp.
- Lowering the global name-match threshold for "ley Reception": it
  could pick Deception. Only targeted I/l and c/e confusion handling is
  acceptable (still TODO).
- Tightening the blur threshold alone: it rejects readable cards. It
  was replaced by the settle window plus the title-sharpness floor.
- Running server and client reads in sequence: that caused the ~3 s
  worst case.
- Racing client and server in parallel: rejected until the server can
  cancel abandoned work (queued frames from disconnected clients are
  skipped now, but running OCR is not interrupted).
- Auto-zoom, per-frame server requests, a hard stillness gate: excluded
  by UX rules.
- Letting art matching confirm a printing: never; shortlist only.
- Photo upload in Scan Cards: removed.
- Legacy `CameraScanner.jsx` (2.8k lines, stale closures,
  `willReadFrequently` mismatch, worker hangs, re-render storm):
  removed.

## 63-card desktop session (scansettle build) and follow-up
- waited p50 769 ms, p90 1784, max 8020; 38/63 < 1 s; client answered all.
- Final successful read p50 246 ms: the wait is gating, not inference.
- Edge veto was the tail: card top at y=6-8 of 1080 inside the 1% band;
  161 passes, the 8.0/5.6/5.2 s adds. 14 junk-title reads ~430 ms each.
- Shipped (`b570248-scanedge`, Astra 3 rounds, SHIP): guarded near-edge
  admission (finite convex outline, 70-110 deg corners, side/aspect
  checks, 0.2% margin, refined quad validated too, every strip of a stage
  must be in frame or the frame is unresolved, fresh read only); settle
  window counts real time (>= 90 ms between observations) so a 25 ms
  gate-pass loop on >= 8-core devices cannot shorten it; Lanczos weight
  cache; telemetry: title batch index, near_edge, partial stage/strips.
- Rejected by review: dropping off-frame footer strips and resolving
  from the rest (can drop the competing identity -> wrong printing);
  side-ratio-only plausibility (collapsed outline passed).
- Planned, measured experiments (Astra plan `.hermes/audits/astra-63cards.md`):
  title crop selection by yield, quality-aware 2-obs settle, thread
  benchmark 2/4/8 on desktop, speculative footer worker, WebGPU EP for
  rec (+13 MB wasm, lazy), hydration off the commit path.

## Known open items
- Measure the new build on the PC and a phone (target < 1 s).
- Targeted OCR-confusion handling (Icy Reception). The frame still
  fails.
- Skip the old-frame collector check for confidently modern cards
  (Astra replay: same 11 matches, slow failed reads 1.16 s -> 0.77 s).
  Needs broader old-border coverage.
- Confirm or fix the double adds.
- Phone cost of the Lanczos refinement (untested on iOS/Android).
- Server-side cancellation of running OCR, then optional client/server
  racing.
- Later: collector reads with fewer strips first, a WebGL/WebGPU
  execution provider, art shortlist experiments.

## Testing and tools
- Offline replay:
  `CV_MODEL_DIR=/tmp/client-assets node backend/scripts/client-scan-replay.mjs <assets> <frames> --json out.json`
  (baseline `.hermes/audits/replay-base-20260927.json`).
- Browser-equivalent downscale A/B: `.hermes/audits/downscale-ab.mjs`.
  Always check preprocessing with browser-like input, not only sharp.
- Live-browser worker harness: load
  `/assets/clientScanWorker-<hash>.js`, serve frames with CORS and
  `Cross-Origin-Resource-Policy: cross-origin`. `reset` sends no
  reply; do not await it. Keep runs under ~12 frames.
- Telemetry: `docker cp bindarr:/app/database/scan-telemetry/<date>.jsonl`.
- Reviews: `.hermes/audits/*astra*.md`, `scan-latency-astra.md`,
  `telemetry-astra.md`.
- Rollback:
  `cd ~/projects/bindarr-host && cp docker-compose.yml.bak-scansettle docker-compose.yml && docker compose up -d bindarr`

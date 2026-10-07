# Scrybox camera scanning: history, results, lessons

Last updated: 2026-09-27. Live image: `scrybox:4196f28-scanfix`
(branch `perf/scan-speedups`, PR open).

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


## Slow-card fixes (scanfix build, 2026-09-27)
Live image `scrybox:4196f28-scanfix` (branch `perf/scan-speedups`, on top
of the proxy work). Driven by Astra's blind investigation of the 4-8 s
cards (`.hermes/audits/astra-slowscan-0927.md`): the waits were crop
geometry and repeated failed reads, not inference or thread count.

What shipped:
1. First modern footer batch reads y=0.84 too (6 strips, still one
   recognizer call). FRA collector lines ("U 0138") sit there on loose
   outlines; neither narrow nor wide sweep read it (Konstrari Charm,
   Marwyn, Last Gasp).
2. Auto backoff: only a real server attempt starts/extends the failure
   streak; settling and local misses keep it unchanged (a settling pass
   used to clear it, so failed fallbacks repeated). Near-edge partials
   (a stage would read past the frame) stay on-device with a new
   `fastscan.hintEdge` hint; after 8 same-place partials the server gets
   one try; movement or any other outcome resets the run.
3. Near-edge stage geometry: when a stage's padded strips leave the frame
   the WHOLE stage is re-projected with that side's padding shrunk
   (0.75/0.5/0.25/0), accepted only if the new strips still cover every
   on-card part of every original strip (tolerance 0.6% of the card).
   Otherwise the stage abstains as before. Never a partial stage.
   The first skipped stage is the one reported.
4. Settle window is elapsed time: 3 still observations spanning >= 180 ms
   real time (was: 3 observations each >= 90 ms apart, which aliased a
   65 ms loop to 260 ms). A read with zero recognizer calls no longer
   counts as OCR for the post-read gap rule.

Replay (Node, same assets as live):
- Astra's 71 fallback frames: client matches 3 -> 49, agree with server
  10, wrong 0, title conflicts 0, lost 0. All 39 client-only printings
  equal what the live session eventually added.
- 1281 saved debug frames: 462 -> 564 matches, 0 lost, 0 changed; wrong
  stays at the 2 pre-existing Flicker/Garruk frames (unchanged).
- Slow cards on their saved frames, before -> after: Roiling Canopy 0/4 ->
  4/4 (title re-projected), Solarium Sentry 0/7 -> 7/7 (footer
  re-projected), Heartstring Puller 0/10 -> 10/10, Konstrari Charm 0/2 ->
  2/2 (0.84 row). Live-browser worker: 8/8 correct.

Rejected in review: re-projection without a coverage check (Astra R1:
synthetic card slid the footer rows onto text-box digits and proved the
wrong printing); local deferrals renewing the backoff (server never
asked again); moving passes preserving the edge run.

Next: a Windows scan round to measure waited_ms on the new build; then
the Flicker/Garruk wrong-title frames, OCR confusions (Icy Reception).
Reviews: `.hermes/audits/astra-scanfix-r1.md`, `astra-scanfix-r2.md`.
Rollback:
`cd ~/projects/bindarr-host && cp docker-compose.yml.bak-scanfix docker-compose.yml && docker compose up -d bindarr`

## Round-2 speedups (scanfix2 build, 2026-09-28)
Based on Astra's round-2 audit (`.hermes/audits/astra-perf-round2.md`) of the
541-card Windows session (adds p50 807 ms, p90 1566 ms). Branch
`perf/scan-speedups-2` off tag v1. One commit per item.
- Shipped, on by default:
  - Footer rescue stage (#2). One 6-strip batch: the wide 0.84 row plus
    0.035-tall modern rows. It runs after every v1 stage, so the v1 stages and
    their recognizer calls are unchanged. Its reads are added to the earlier
    ones, never used in their place. Once rescue has read, no exit (stage,
    retro or pooled vote) may prove a printing that another strong number or
    set+number read contradicts.
  - The 1286-frame replay caught a wrong printing on the first attempt:
    Damn read `089/59%` and was proven DRC 89, but the card is MH2 80.
    This rule fixed it.
  - Resets (#8): `lastDeferred` is cleared on reset and on no-card frames.
    The no-title run is cleared on auto start.
  - Backoff (#10): a titleless server failure is signed with the same-frame
    client title and position.
  - Equivalent rewrites: fuzzy lookup memo (#11: 5940 lookups, 0
    mismatches), recognizer LUT and buffers (#5: bit-identical), and
    flattened-tap Lanczos (#4: 0 differing bytes on real frames).
  - Frames (#6): rVFC frame ids, with a pixel fingerprint when rVFC is
    missing or stale. A repeated frame never counts as settle evidence and
    is never read. Switching between the two id sources restarts the window.
  - Presentation epoch and a presence-aware dedupe (#0): no re-add without a
    lift, slide, other card, or sustained motion. Build stamp added to
    telemetry.
  - Directional edge hint and dashed safe region (#9). This is guidance
    only; admission is unchanged.
  - Memoized tray, and no busy re-render per auto pass (#12).
  - Background hydration (#13). Rows stay pending and unsendable until
    hydrated. A failure keeps the row and retries it.
- Shipped behind flags, off by default: `localStorage scan.fastSettle=1`
  (120 ms settle window on clean, distinct rVFC frames, #1) and
  `scan.titleGate=1` (skip title rescue when every first crop is flat, #3/#7;
  crop contrast is always logged as `timings.title_range`). Both need
  recorded-video or telemetry calibration before they can be turned on.
- Threads (#14): the default stays at 2. `scan.threads` (existing) is now
  reported in device info.
- Not done (#15): WebGPU/WebNN. The worker imports the wasm-only ORT, jsep
  assets are not shipped, and no GPU adapter or navigator.ml was available
  to test on. Revisit once Brent's Windows Chrome reports an adapter.
- Replay (baseline v1 -> candidate):
  - 71 old fallback frames: 49 -> 55.
  - 5 new fallback frames: 0 -> 5.
  - Harness: 8 -> 8.
  - 1286 debug frames: 564 -> 621 matched, server agreements 347 -> 363,
    wrong 2 -> 2 (the same two Flicker frames as before).
  - 0 lost, 0 changed.
- Live-worker browser check on the deployed bundle agrees: Oath, Helm,
  Masamune and H.E.R.B.I.E. resolve via rescue, and Damn abstains.
- Review: Astra, 5 rounds (`.hermes/audits/astra-r2impl-review1..5.md`).
  Round 5 gave SHIP once the full replay showed 0 lost and 0 new wrong,
  which it did.
  - The review caught: second copies suppressed by dedupe, frozen frames
    counted as settle evidence (including a switch between id sources),
    ambiguity escaping through retro and pooled votes, rescue pre-empting
    v1 stages, hydration failures dropping rows, and title-timing sources.
- Next: a Windows scan round on this build (telemetry build stamp
  `1.8.5+20260928T0206`), a threads A/B (`scan.threads`), then calibrating
  the fastSettle and titleGate flags from the new `dup`, `fast_settle` and
  `title_range` telemetry.

## Set-agnostic identification cascade (scancascade build, 2026-10-07)
Trigger: Windows Chrome webcam session `17lonf8t` (build
`1.8.5+20260930T2351`). Twisted Image (SOM 50/249, 2003 frame, List
symbol) looped on "exact printing not resolved" for 30+ s. Diabolic Tutor
was ADDED as ODY 129, but the card is The List ODY-129. Brent: "durable
solution that is agnostic to set ... iterate down ways to scan ... fastest
way wins". Never guess.
- Root causes:
  - The footer resolver compared Scryfall numbers literally. The List / MB2
    (`plst`) numbers like `ODY-129` never matched a printed `129`, so
    `ody 129` looked unique.
  - Old-frame (2003-2010) copyright lines end with `N/T` at y ~0.865-0.88
    on a padded webcam outline. The retro rows (0.845/0.855) never read it,
    and the old trailing-number rule returned T (the set total).
- Shipped (client `shared/clientScan/*`, sidecar `server.py`, both
  data-driven):
  - Physical footer key per printing: a number `<SET>-<N>` whose prefix
    is ANOTHER known set code prints that set's footer. 5568 of 5588 plst
    printings map; the other 20 are online-only originals. pmei `2020-1`
    and sld `IFIYW-3` keep their own set. No per-set code.
  - Resolvers return the whole collision group (`resolveFooterGroup`,
    `_resolve_footer_group`). Exactly one printing = added; several =
    the cascade stops with `printing needs a choice` plus `choices`. A
    unique printed alias with a twin is not a proof. The visual matcher may
    not pick between twins (Scryfall's plst images are the original scans).
  - Retro stage: 4 rows (0.855, 0.845, 0.865, 0.875) in one recognizer
    call. `N/T` -> N, with the total checked against the set's highest
    number. Years are rejected (also `2003/2010`). A bare `N/T` line needs
    T >= 40. The added rows feed only this parser, never general footer
    evidence. Validated numbers stay strong constraints for every later
    stage and for pooled frames (expiring with their frame). Unreconciled
    ones settle the card on the server (no per-card fallback).
  - UI: a choice becomes a tray row "Choose printing" with
    "<set> or <set>?". It opens the existing printing sheet with only the
    candidates (images) and is never sent until picked. Auto dedupes it by
    a stable key. Backend `POST /api/cardscan/choices`; /frame passes
    `footer_ocr.choices` through.
- Not done: a corner-symbol discriminator stage. Footer twins abstain to
  the picker instead. A future stage would slot in before the picker,
  without changing the contract.
- Replay, 1364 saved frames:
  - Client: matched 655 -> 601, wrong 2 -> 2 (pre-existing), 0 lost,
    0 changed.
  - Client gained 4: Tide Shaper MH2 394 from "2021 Wizards of the Coast
    394".
  - Client former hits that became choices (intended, every one a real
    plst twin): 58 frames, from Chainer MH2 289 (23), Refuse to Yield
    SNC 27 (17), Specimen Collector MH2 64 (10), Timeless Dragon MH2 35
    (5) and Lonis MH2 204 (3).
  - Client misses that became choices: 10 Twisted Image frames
    (SOM 50 / plst SOM-50).
  - Server (sidecar, serial): hits 524 -> 466. The 58 former hits all
    became choices (the same five plst twins), 0 lost, 0 changed. 6
    Twisted Image misses became choices. p50/p90 385/1053 -> 397/1289 ms.
- Review: Astra, 4 rounds (`.hermes/audits/astra-r1..r4.md`). Ten
  blockers were found and fixed, each with a regression test that fails
  on the previous code. R4 verdict: SHIP.
  - Caught: alias shortcut, retro choice overwritten by a later sweep, P/T
    and rules digits from the new rows, `(c) 2003/2010`, a set-total
    bypass, a stale picker request, retro constraints lost across stages,
    frames and the per-card fallback, and shared-promise cancellation.
- Suspect rows (added 2026-10-07 for printings that have a plst twin;
  NOT changed): see the deploy report.
- Next: a Windows scan round with List cards and originals mixed.
  Measure how often the picker appears, and consider a bottom-left
  corner-symbol stage if it is frequent.

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

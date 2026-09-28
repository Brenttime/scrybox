╭─ ⚕ Hermes ───────────────────────────────────────────────────────────────────╮
Read-only investigation complete. No repository files changed.

Confirmed: 541 added events, p50 807 ms, p90 1,566 ms, eight above 2.5 seconds, five server fallbacks.

Important correction: waited_ms is not a reliable physical-card appearance timer. Twelve windows contain an earlier, different recognized title. The 8,430 ms Galactus event includes 3,518 ms reading Helm of the Host unsuccessfully, followed by 3,486 ms of edge rejection whose card identity is unknown.

Ranking below weighs typical latency, tail impact, and strength of evidence. Savings ranges are engineering estimates for Windows validation, not measured production improvements. They overlap and must not be summed. “Affected tail” means the particular slow cards, not the entire session’s p90.

File references are against origin/perf/scan-speedups at 92c88c6.

Ranked FIXES

- 1. Make the settle window quality-adaptive, rather than charging every readable card the same 180 ms.

  Expected saving: p50 30–80 ms; p90 50–150 ms. Additional opportunity around 100–130 ms per unnecessary post-OCR re-settle.

  Evidence: reconstructed add windows contain 134.1 seconds of settling; per-card settling p50 is 235 ms, p90 374 ms. Of that total, 15.5 seconds follows OCR without intervening observed movement. The actual reader admits at 195 ms with 65 ms observations, but at 267 ms with 89 ms observations. After a long OCR gap, my probe required another 130 ms.

  Trial: a shorter, approximately 120 ms path for demonstrably clean, distinct frames; retain the existing conservative path for uncertain frames. Do not remove continuity invalidation or blindly eliminate the window.

  Risk: medium/high. The existing 180 ms window still sometimes admits smeared frames. Earlier admission can make scanning slower if it triggers failed OCR.

  Validation: replay recorded video, including swaps and motion, with decoded-frame timestamps. Still-image replay cannot validate this change.

  Files: shared/clientScan/pipeline.mjs:53–80,301–342; frontend/src/components/FastScanner.jsx:379–391.

- 2. Repair footer strip height and the missing wide .84 row. This is the strongest directly reproduced tail fix.

  Expected saving: population p50 roughly 0–10 ms; p90 approximately 50–200 ms if broader misses behave similarly. Affected tail: approximately 0.4–1.5 seconds, including avoided fallback and retry work.

  Three different failures are present:

  - Helm of the Host: height .025 reads “R.0209”; height .035 at the same .88 start reads “R 0200”, confidence .926/.934. The index contains MSC 200 and 437—not 209.
  - H.E.R.B.I.E.: height .025 reads “2196”; height .035 at .86 reads “R 0106”, confidence .972. This is vertical crop damage, not a missing printing.
  - Oath of Eorl: narrow .84 reads “R.006”; wide .84 reads “R 0064”. The current wide sweep omits .84, so it cannot recover that final digit.
  - The Masamune: the original .94 strip correctly reads 0264 once, without FIN. A taller .92 strip supplies another correct read, satisfying the existing setless evidence rule. Gloin/LTR and Palantir/HOC show analogous single-number/no-code misses.

  Scratch full-pipeline rescue results:
    New fallback frames: 0/5 -> 5/5 matches.
    Previous fallback corpus: 49/71 -> 55/71.
    All original successful IDs retained.
    Eleven exact server agreements across both corpora; zero compared conflicts.

  Risk: medium/high. Only Oath has same-frame exact server truth among the five new frames. Taller crops can include rules text; a universal extra batch also makes remaining misses slower. Use bounded rescue, preserve conflicting evidence, and do not globally replace the crop geometry based on five images.

  Validation: mixed layouts, promo suffixes, off-card text, competing valid collector numbers, and the previous reprojection adversarial tests. Never “correct” 0209 to the closest indexed number.

  Files: shared/clientScan/pipeline.mjs:130–147,400–433,476–530; shared/clientScan/text.mjs:234–289.

- 3. Stop spending full title-rescue work on visibly unusable frames—but do not simply skip rescue when the first batch is empty.

  Expected saving: p50 0–40 ms; p90 approximately 80–200 ms. Affected tail: roughly 200–500 ms per avoided bad read and its subsequent settling.

  Evidence: 125 “no confident card title” passes consume 56.3 seconds inside add windows. Many raw reads are empty, “一”, isolated characters, or visibly mutilated names—not nearly correct strings being rejected by an overly strict matcher.

  However, 105 failed passes lack a usable-length Latin string in title batch one, and 46 successful final reads also start that way. Therefore “empty first batch -> skip title2” sacrifices real successes.

  Fix the decision using crop-level text contrast, directional blur, clipping, and fresh-frame quality. A short local retry can be better than running all rescue crops against the same smeared image.

  Risk: medium. Do not lower title confidence or fuzzy-match thresholds as a speed fix. My raw-text probe found no substantial hidden population of low-confidence correct titles; relaxing the floor even admits the one-letter title “X” from a noisy Pyre Rhymer frame.

  Validation: save rejected frames locally in an opt-in ring buffer; compare first-frame recovery and false titles, not just OCR duration.

  Files: shared/clientScan/pipeline.mjs:435–461; shared/clientScan/imaging.mjs:149–170; shared/clientScan/text.mjs:135–160.

- 4. Accelerate refinement without removing its validated resampling.

  Expected target: p50 20–40 ms; p90 20–50 ms. Measured total budget: refinement p50 64 ms, p90 68 ms.

  Every final add pays refinement. It performs a full-frame separable Lanczos resize and another Cornelius inference after the initial canvas-based probe.

  Prefer a numerically checked WASM/SIMD resizer, fused normalization, or a validated single high-quality detector input. The latter trades more work on gate passes against removing duplicate detection on reads; benchmark whole-session throughput, not isolated successful reads.

  Risk: low/medium for equivalent arithmetic, high for replacing Lanczos with a cheaper filter or deleting refinement. Existing evidence shows the canvas/model-input mismatch affects printing recovery.

  Validation: output-pixel comparison, corner deltas, exact IDs, and full Windows session CPU usage.

  Files: shared/clientScan/imaging.mjs:173–207; shared/clientScan/pipeline.mjs:344–374; frontend/src/utils/clientScan.js:132–142.

- 5. Optimize recognizer tensor work, batch shapes, and output decoding. One ONNX call is not constant cost.

  Expected target: p50 5–20 ms for equivalent preprocessing/decoding work. Larger inference savings remain unmeasured; do not budget them as guaranteed.

  Actual browser tensors:
    First title: 2 x 3 x 48 x 401.
    Title rescue: 3 x 3 x 48 x 358.
    First footer: 6 x 3 x 48 x 320.
    Wide footer: 4 x 3 x 48 x 411.
    Retro: 2 x 3 x 48 x 823.

  The six-strip footer is materially more expensive than the two-strip title despite both being one call. Title stage two appears on 95 final adds and helps explain title p90 324 ms versus p50 119 ms.

  Safe experiments: reusable shape-keyed buffers, normalization LUTs, fused sampling/tensor packing, and an ONNX output head returning argmax plus original maximum probability. The current 18,710-class outputs are approximately 7.5 MB for title batch one and 18 MB for footer batch one.

  Risk: low for equivalent operations; medium/high for removing strips, changing widths, or pruning character classes. Preserve CTC tie handling and confidence exactly. Do not remove non-Latin classes merely because the collection is English.

  Validation: token/confidence equivalence, ambiguous titles, and per-shape browser timings.

  Files: shared/clientScan/pipeline.mjs:207–238; shared/clientScan/imaging.mjs:72–119; shared/clientScan/text.mjs:25–37.

- 6. Schedule against actual decoded frames and deadlines, not only setTimeout gaps.

  Expected target: p50 0–30 ms; p90 20–80 ms. Faster discovery after “no card” could save more, but that delay is largely absent from waited_ms.

  Measured post-pass gaps are consistently about 26 ms after gates and 63 ms after failed reads. There is little unexplained scheduler delay. The avoidable cost is mostly the configured delay.

  More importantly, 2,240 adjacent pairs have identical sharpness and box. That suggests repeated decoded frames, although telemetry cannot prove camera FPS.

  Use requestVideoFrameCallback/mediaTime to avoid duplicate observations, select the freshest frame, and wake near a settle deadline. Reduce the post-failure 60 ms delay when new pixels already exist. Reconsider 350 ms empty-desk polling for desktop use.

  Risk: low/medium. Preserve one read in flight and same-frame correspondence between detection, OCR, and fallback JPEG. Counting duplicate frames as independent evidence is unsafe.

  Files: frontend/src/components/FastScanner.jsx:22–29,379–391; frontend/src/utils/clientScan.js:97–144; shared/clientScan/pipeline.mjs:303–321.

- 7. Recalibrate blur and motion using OCR outcomes, not a globally lower blur threshold or larger drift tolerance.

  Expected saving: not independently estimable from this dataset; it overlaps fixes 1 and 3. Population p50 may improve modestly or regress. Affected tail budget is hundreds of milliseconds.

  The present sharpness statistic sparsely samples a 64 x 12 band; it is not a conventional neighboring-source-pixel blur measurement. Background/border structure can remain “sharp” while title characters are unreadable.

  Threshold sweep illustrates the tradeoff:
    Floor 1500 rejects 48 failed-title frames—but also 9 successful final frames.
    Floor 2500 rejects 86 failures—but also 48 successes.

  No supplied frame below the current 500 floor was actually OCR-tested in this session. Lowering it has no demonstrated benefit. Motion tolerance is normalized to frame diagonal, not elapsed time or card size.

  Risk: medium/high. Separate “may attempt fresh OCR” from “may retain previous printing/evidence.”

  Validation: shadow decisions against labeled video, including sharp moving cards, exposure smear, sleeves, small cards, and slow translation.

  Files: shared/clientScan/imaging.mjs:149–170; shared/clientScan/pipeline.mjs:48–68,301–341.

- 8. Reset all presentation-specific retry state. There is a concrete remaining bug.

  Expected saving: p50 likely 0; affected tail approximately one unnecessary wide-plus-retro read, around 470 ms at current Windows timings. Frequency is not recoverable from existing telemetry.

  Reproduced using the real reader with synthetic state fixtures:
    Fresh presentation: 2 recognizer calls, deferred footer.
    After reader.reset(): 4 calls, no deferral.

  reset() and the no-card probe path fail to clear lastDeferred. A previous card’s deferral can therefore make the next presentation immediately pay the expensive deep stages. Reset no-title counters at the corresponding UI run boundary too.

  Risk: low if reset is limited to genuine presentation/run resets. Do not reset repeatedly while an unresolved card remains present; that could prevent deep rescue forever.

  Validation: deferred -> stop/start, deferred -> no-card -> new card, and uninterrupted unresolved-card progress.

  Files: shared/clientScan/pipeline.mjs:248–249,381–395,535–541; frontend/src/components/FastScanner.jsx:290–291,394–401.

- 9. Treat the remaining edge tail as a framing/detection problem, not permission to relax exactness.

  Expected saving: population p50 approximately 0, p90 small; affected tail potentially 1–3 seconds through earlier correction. No recognition saving is proven on these frames.

  All 201 remaining edge-rejected passes have their box at the top boundary. This is no longer the old y=6–8 px, padded-strip rejection problem. The session contains no near_edge_partial failures.

  Give an immediate directional “move card down” guide and a visible safe capture region. A bounded high-quality detector recheck may distinguish coarse-detector error from actual clipping, but must still prove complete readable geometry.

  Risk: low for guidance; high for accepting y=0 outlines or clamped pixels. No images were retained for these local-only rejected passes, so a relaxed edge threshold cannot be signed off.

  Files: shared/clientScan/pipeline.mjs:294–299,329–342,400–433; frontend/src/components/FastScanner.jsx:327–332.

- 10. Establish fallback backoff from the same-frame local title when the server fails without a title.

  Expected saving: population p50/p90 approximately 0; affected tail roughly 400 ms per redundant Helm-style request.

  Both Helm server responses say “no confident card title,” while the client already knows Helm. nextFailStreak therefore cannot establish a signature. The next full local failure sends another request.

  Preserve the client’s same-frame title/position when recording that actual failed server attempt. Continue to release on a different card and keep bounded rescue. Never renew the deadline merely because another local deferral occurred.

  Risk: low/medium. Avoid suppressing multi-card or genuinely changed scenes.

  Validation: actual Helm response shapes, changed-title bypass, timed release, and unchanged request ceiling. With only five fallbacks now, this is no longer a leading median optimization.

  Files: frontend/src/utils/fastScan.js:79–126; frontend/src/components/FastScanner.jsx:304–316.

- 11. Reduce repeated fuzzy-search work without changing matching semantics.

  Expected target: population p50 0–5 ms; noisy-title tail 5–30 ms.

  Non-exact titles can scan 35,542 names for multiple query variants. Exact names already have an O(1) lookup. Use bounded memoization keyed by normalized OCR text and index version, plus provably equivalent length/candidate pruning.

  Cache text lookup results—not printing identity across frames. Preserve runner-up margins, aliases, exclusions, and tie order.

  Risk: low with equivalence tests. Lowering cutoffs is not an optimization.

  Validation: compare every recorded title_raw lookup and an adversarial near-name corpus.

  Files: shared/clientScan/text.mjs:87–106,135–160; shared/clientScan/pipeline.mjs:439–455.

- 12. Keep worker/transfer and UI changes proportional to their measured cost.

  Expected saving: worker transport p50 approximately 0 ms, p90 at most a few milliseconds. Capture draw offers a separate approximately 14 ms budget.

  All 7,799 wait_ms values are zero. Worker round-trip minus pipeline time is p50 0, p90 1 ms. Buffers are already transferred, not cloned. Replacing this with SharedArrayBuffer is not a major latency fix.

  Worth testing: worker-owned VideoFrame/OffscreenCanvas capture, releasing unused returned buffers, memoizing the growing tray, and avoiding busy-state rerenders every gate pass. However, ordinary scheduling gaps remain stable; there is no demonstrated large tray-render stall.

  Risk: low/medium; lifecycle and stale-frame handling matter more than transport mechanics.

  Validation: long-session main-thread traces and memory/GC, with the full tray populated.

  Files: frontend/src/utils/clientScan.js:51–57,118–144; frontend/src/utils/clientScanWorker.js:105–132; frontend/src/components/FastScanner.jsx:242,349–366.

- 13. Decouple hydration from continued scanning, preserving a pending/unusable tray state until hydration finishes.

  Expected budget: p50 9 ms, p90 11 ms; isolated tails 58–456 ms. Actual benefit depends on whether next-card capture overlaps it.

  Hydration is small normally but occasionally stalls an already proven ID. Continue capture while the same existing hydration request completes; deduplicate in-flight IDs and do not allow incomplete entries to be sent.

  Risk: medium due to ordering, cancellation, and duplicate handling. Do not label an incomplete card as fully added merely to improve telemetry.

  Validation: delayed hydration, stop/start, repeated IDs, failures, and pending-row send behavior.

  Files: frontend/src/utils/clientScan.js:154–192; frontend/src/components/FastScanner.jsx:295–299,349–354.

- 14. Benchmark four threads on Brent’s actual machine; do not ship a core-count-based increase yet.

  Expected Windows saving: unknown; zero demonstrated.

  I ran a real isolated Chromium/WASM 2 -> 4 -> 2 experiment on nine saved frames. This host has four cores. Read medians were 1,453 -> 1,833 -> 1,423 ms: four threads was slower here.

  That does not settle the 16-core Windows question. Use the existing scan.threads override, reload the worker, randomize A/B order, and measure actual appear-to-proof latency plus thermal stability—not just inference throughput.

  Risk: low accuracy risk, meaningful performance/battery regression risk.

  Files: frontend/src/utils/clientScanWorker.js:22–29,73–83,89–92; frontend/src/utils/clientScan.js:63–65.

- 15. WebGPU/WebNN and alternative recognizers are big bets, not measured recommendations.

  Expected saving: presently unquantifiable. Relevant live budgets are title 119/324 ms and footer approximately 270 ms on footer-requiring cards.

  The shipped worker explicitly imports the WASM-only entry point. Hardware EP use requires different runtime assets, verified operator coverage, shape handling, warmup, and CPU fallback. My browser exposed navigator.gpu but returned no adapter; navigator.ml was absent. I therefore have no hardware acceleration result to report.

  WebGPU becomes more attractive if large CTC output readbacks are reduced. Quantization or a Latin-specialized model may save more compute, but changes recognition behavior and demands a broader accuracy program.

  Risk: medium for EP numerical differences; high for model/quantization changes.

  Files: frontend/src/utils/clientScanWorker.js:12–16,73–80; shared/clientScan/pipeline.mjs:233–236; shared/clientScan/text.mjs:25–37.

Measurement fix required alongside these trials

- Add a presentation epoch and distinguish capture time, first candidate, first confident title, exact proof, hydration, and tray commit. Log mediaTime, strip identity/confidence, and rejection geometry using the existing batched telemetry—not extra frame requests.

  Also address the four-second dedupe expiration: The Masamune is added, repeated, then added again after a long failure interval at nearly the same box. That can happen without a proven physical-card replacement.

  This saves no inference time directly, but prevents mixed-card timers and repeat additions from producing misleading performance conclusions.

  Files: frontend/src/components/FastScanner.jsx:324–347; frontend/src/utils/scanTelemetry.js:79–105; shared/clientScan/pipeline.mjs:439–442,514.

Quick wins versus big bets

- Best first hands-on trial: missing wide .84 coverage plus bounded taller-footer rescue. It has real pixel-level and full-pipeline evidence.
- Low-risk cleanup: reset lastDeferred at presentation boundaries; fix titleless-server backoff; instrument decoded-frame timing and presentation identity.
- Next optimization trials: equivalent refinement acceleration, capture deadline scheduling, crop-quality-driven title retry.
- Bigger accuracy-sensitive work: shorter adaptive settling, learned/localized title/footer regions, fewer OCR strips.
- Hardware bets: target-Windows thread A/B, then WebGPU/WebNN. Do not assume more threads or a GPU EP is automatically faster.

Verification and evidence

- 49 existing targeted tests passed.
- Real replay used assets copied from bindarr:/app/database/models/client-scan.
- Scratch candidate: 60/76 matches versus 49/76 shipped-pipeline matches; all baseline successful IDs retained; eleven exact server agreements, zero compared conflicts.
- This is not an accuracy sign-off: most candidate successes lack same-frame exact ground truth, and only five images exist from the new session.
- No repository modifications or production deployment.

Full evidence, reconstructed timelines, probes, browser results, and scratch experiments:
  /tmp/astra-after-0927/README.txt
╰──────────────────────────────────────────────────────────────────────────────╯

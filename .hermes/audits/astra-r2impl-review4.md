╭─ ⚕ Hermes ───────────────────────────────────────────────────────────────────╮
Reviewed 0a6c640. No repository edits made. Report and probe outputs: /tmp/astra-r2rev4/REVIEW.txt

References below are relative to /tmp/bd-s2.

BLOCKERS

No newly reproduced recognition, frozen-frame, or acknowledged-row-loss blocker. The required measurement criterion remains incomplete below.

SHOULD-FIX

R3-S1 — NOT FIXED in full.
  frontend/src/components/FastScanner.jsx:378-391,434,464-468
  frontend/src/utils/fastScan.js:161-184

  Original timing defects are fixed:
    - 50 ms title + 750 ms footer now reports title_ms=50.
    - Capture/refinement spans are included.
    - Capture=100, local error=120, sequential server completion=1000 now reports title_ms=900, not 20.

  Expanded probes reproduce two remaining defects:

    A. Early server hedge gets the later local-completion time.
       Capture=100; server completes with the title at 200; local fails at 900.
       Actual title_ms=800; expected server-completion metric=100.
       serverRead never retains its completion timestamp; line 467 samples performance.now() after awaiting local.

    B. Successful server title is omitted when local has a candidate but no title.
       At the auto title-escape threshold, server fallback succeeds at 1000 and adds the row, but pres.title_ms is absent.
       useLocal checks candidate presence, so nextPresentation receives the title-less local object despite selecting the server-time fallback.

  Retain server completion time and select the title and timestamp together, separately from local presentation geometry.

  Probe: /tmp/astra-r2rev4/timing-probes.mjs

  Enqueue labeling — FIXED.
    frontend/src/components/FastScanner.jsx:333-334,495-507

    insert_enqueue_ms / usable_enqueue_ms explicitly describe approximations. The deferred-state probe confirms usable_enqueue_ms=800 while the row remains pending until the queued update is applied. No React commit/paint claim remains.

R3-S2 — FIXED.
  shared/clientScan/pipeline.mjs:588-606,609-626
  frontend/src/utils/scanAudit.test.js:586-598

  Adapted the deferred probe to assert correct v1 behavior:

    Original stages:               id-lea, batches [2,6,4,2]
    Candidate, blank rescue:       id-lea, batches [2,6,4,2]
    Candidate, conflicting rescue: id-lea, batches [2,6,4,2]

  All preserve stage 2, footer evidence, and multi-frame proof method. Rescue never runs on this pooled v1 success.

  Regression check: an unresolved deferred pool still reaches rescue with [2,6,4,2,6] and resolves the fixture. Existing wide/retro preservation and rescue-conflict abstention probes also pass.

  Probe: /tmp/astra-r2rev4/deferred-stage-probe.mjs

NITS

  R2-N3 — Device validation still outstanding; cadence disclosure remains fixed.
    frontend/src/components/FastScanner.jsx:27,545-563
    frontend/src/utils/fastScan.js:236-242

    Modeled fallback tests pass, but no physical-phone battery, thermal, low-light, or throughput validation. “Well under 1 ms” remains an unqualified hash-cost claim.

  R4-N1 — Stale timing comment.
    frontend/src/utils/fastScan.js:168-170

    Still describes titleAt as the end of the entire on-device read.

  Carried benchmark disclosure, waiter cleanup, and failed-row animation fixes remain intact:
    backend/scripts/lanczos-equivalence.mjs:1-4,18-22
    frontend/src/utils/clientScan.js:147-154
    frontend/src/index.css:1906-1908

PER-ITEM VERDICTS

   0  DROP      Title-source timing remains incomplete.
   1  FLAG-OFF  fastSettle remains experimental/default-off.
   2  SHIP      Deferred pooled boundary fixed; corpus gate remains.
   3  FLAG-OFF  Title rejection threshold remains uncalibrated.
   4  SHIP      Lanczos pixel equality verified; benchmark qualified.
   5  SHIP      LUT/reused-buffer equivalence tests pass.
   6  SHIP      Frame-source transition and live-resumption probes pass.
   7  FLAG-OFF  Same experimental titleGate as item 3.
   8  SHIP      Reset/no-card deferral regressions pass.
   9  SHIP      Guidance and whole-stage geometry protections retained.
  10  SHIP      Failure-signature/deadline regressions pass.
  11  SHIP      Bounded index-scoped text memo retained.
  12  SHIP      Memoized tray and stable callbacks retained.
  13  SHIP      Failed rows retained; retry lifecycle remains bounded.
  14  SHIP      Thread reporting retained; no default increase.
  15  SHIP      Intentional document-only deferral accepted; WASM unchanged.

Per-item source locations are included in REVIEW.txt.

VERIFICATION

  - npm test: 151 passed, 0 failed; locale checks passed.
  - Targeted ESLint and git diff --check passed.
  - Current-code scanner, UI, supplemental, and adapted deferred probes passed their regression assertions. Timing probes reproduced the residual defects above.
  - Five real-image Lanczos comparisons: zero differing bytes.
  - cand4: 49/71 -> 55/71; 0/5 -> 5/5; harness 8/8 -> 8/8.
  - Independently checked replay counts and coverage: zero lost/changed successful IDs, zero new compared wrong printings, zero title conflicts. Baseline-success stages, call counts, and footer evidence match.
  - cand4 is from c112adc; per-frame resets do not exercise the changed deferred path or UI timing. It is not a fresh full replay of 0a6c640.
  - cand4-debug.json remains absent. The 1,286-frame replay is not signed off.

OVERALL: NOT-SHIP. R3-S2 is fixed, but R3-S1 still fails the required measurement semantics. The full replay must additionally show 0 lost / 0 new wrong; it is not the only outstanding condition.
╰──────────────────────────────────────────────────────────────────────────────╯

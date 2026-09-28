╭─ ⚕ Hermes ───────────────────────────────────────────────────────────────────╮
Reviewed through c112adc. No repository edits made. Full report and probes: /tmp/astra-r2rev3/REVIEW.txt

References below are relative to /tmp/bd-s2.

BLOCKERS

No remaining reproduced frozen-frame or acknowledged-scan-loss blocker. Two required R2 criteria remain unmet below, so the complete batch is NOT-SHIP.

  R2-B2 — FIXED.
    shared/clientScan/pipeline.mjs:389-425
    frontend/src/utils/clientScan.js:188-203

    The exact 7 -> 8 -> repeated 8 -> fingerprint sequence now resets stillRun to 1 at 701 ms, with zero OCR calls. Reverse and repeated source switches also reset. A resumed live stream still progresses to admission.

  Carried R1-B1 — FIXED.
    frontend/src/utils/fastScan.js:198-220
    frontend/src/components/FastScanner.jsx:477-478

    Lift/repeat probe produces exactly one second addition at 5,000 ms. Suppressed repeats preserve the break; two motion passes do not break presence, three do.

  Carried R1-B3 — FIXED for the reported rescue ambiguity escape.
    shared/clientScan/pipeline.mjs:179-191,497-498,592-602,635-640

    Conflicting rescue identities and the previous pooled-conflict probe abstain. Real current-code replay of the Damn frame preserves both 080/505 and 089/59% evidence, sets rescue_conflict=1, and abstains.

  Carried R1-B4 — FIXED.
    frontend/src/components/FastScanner.jsx:315-347,498-500,600-603

    Failed hydration retains the proven, unsendable row. Automatic retry hydrates that same row without rereading; quantity survives. Send excludes pending/failed rows.

SHOULD-FIX

  R2-S1 — NOT FIXED in full.
    frontend/src/utils/fastScan.js:171-186
    frontend/src/components/FastScanner.jsx:333-334,398-400,461,485-499

    Fixed: sustained-motion epochs, lift/repeat continuity, capture-start anchoring, and renaming commit_ms to insert_ms.

    Remaining defects reproduced through the actual extracted handlers:

    - A 50 ms title stage followed by 750 ms of footer work reports pres.title_ms=800. localDoneAt measures the entire read, not title completion.
    - Capture starts at 100; local reading fails at 120; fallback completes at 1000. The server title receives localDoneAt=120, reporting title_ms=20 despite there being no local title.
    - usable_ms is emitted after enqueueing setResults, not after the row becomes usable in React. A deferred-state probe receives the hydrated event while the row remains pending.

    Use the timestamp belonging to the selected title source. Distinguish title completion, hydration completion, and usable commit—or name the metrics explicitly as enqueue/completion approximations.

    Probe: /tmp/astra-r2rev3/timing-probes.mjs

  R2-S2 — NOT FIXED in full.
    shared/clientScan/pipeline.mjs:171-174,588-602,620-640

    Single-frame wide/retro preemption is fixed. Both original probes now preserve v1 batches, and all 66 real baseline retro successes retain their IDs, footer reads, and four recognizer calls.

    Deferred multi-frame proof still changes:

      Index: LEA 161 and 2X2 161, same title.
      First eligible frame: “lea”; footer deferred.
      Next frame: narrow/wide “161”; retro blank.

      V1:       id-lea, batches [2,6,4,2]
      Candidate: id-lea, batches [2,6,4,2,6]

      If that extra rescue reads “2x2 161”:
      Candidate: id-2x2 instead of v1’s id-lea.

    lastStage includes rescue, so prior.deferred suppresses the original retro-stage pooled exit until rescue runs. This can change the selected printing, not merely add a call.

    This is synthetic control-flow evidence, not an observed real-image wrong printing. Normal replay resets between frames and cannot exercise it.

    Preserve the original deferred-proof boundary, or explicitly gate and validate this policy change rather than claiming all v1 stages/calls are unchanged.

    Probe: /tmp/astra-r2rev3/deferred-stage-probe.mjs

  R2-S3 — FIXED.
    frontend/src/components/FastScanner.jsx:315-322,336-346

    Initial failure plus four failed manual retries leaves one live timer, not five. Busy double taps issue no additional request; subsequent backoff retains one timer. Dismissed rows do not retry.

NITS

  R2-N1 — FIXED as disclosure.
    backend/scripts/lanczos-equivalence.mjs:1-4,18-22

    The script labels the reference uncached and disclaims a production A/B. Five real images again produce zero differing bytes. This does not independently measure corner deltas or isolate production speedup.

  Carried R1-N2 — FIXED.
    frontend/src/utils/clientScan.js:147-154

    100 timed-out waits retain zero waiters; a live callback wakes and drains its waiter.

  R2-N3 — NOT FIXED as device validation; cadence comment FIXED.
    frontend/src/components/FastScanner.jsx:27,40,545-554
    frontend/src/utils/fastScan.js:236-242

    The comment now acknowledges that some phones take the faster branch. Modeled fallback timing passes. Physical-phone battery, thermal, low-light, and throughput validation remains absent. “Well under 1 ms” remains an unqualified hash-cost claim.

  R2-N4 — FIXED.
    frontend/src/index.css:1906-1908

    The failed+pending selector now overrides the pending animation.

PER-ITEM VERDICTS

  0  DROP      Dedupe/epochs fixed; title and usable-commit metrics incomplete.
  1  FLAG-OFF  fastSettle remains default off; decoded-video validation absent.
  2  DROP      Deferred pooled-proof preemption remains.
  3  FLAG-OFF  Title rejection threshold remains uncalibrated.
  4  SHIP      Lanczos pixel equality verified; benchmark qualified.
  5  SHIP      LUT/reused-buffer equivalence tests pass.
  6  SHIP      Frame-source transition blocker fixed.
  7  FLAG-OFF  Same experimental titleGate as item 3.
  8  SHIP      Reset/no-card deferral regressions pass.
  9  SHIP      Guidance and whole-stage geometry protections retained.
 10  SHIP      Failure-signature/deadline regressions pass.
 11  SHIP      Bounded index-scoped text memo unchanged; tests pass.
 12  SHIP      Memoized tray and stable callbacks retained.
 13  SHIP      Failed rows preserved; retry lifecycle now bounded per row.
 14  SHIP      Thread settings reported; no default increase.
 15  SHIP      Intentional document-only deferral is acceptable.

Item 15 does not require acceleration to exist. Retaining the WASM-only path without an unvalidated hardware-provider change is appropriate: frontend/src/utils/clientScanWorker.js:12,75.

VERIFICATION

  - npm test: 150 passed, 0 failed; locale checks passed.
  - Targeted ESLint and git diff --check passed.
  - Original round-2 probes rerun; adapted scanner, UI, and supplemental probes pass. New probes reproduce the remaining S1/S2 defects.
  - cand4: 49/71 -> 55/71; 0/5 -> 5/5; harness 8/8 -> 8/8. No baseline successful IDs lost or changed; their stages, call counts, and footer reads also match. Zero compared printing/title conflicts.
  - Independent 67-frame replay: all 66 baseline retro successes retained with four recognizer calls; Damn abstains. There are 23 exact server agreements; 43 matches lack same-frame exact server truth.
  - cand4-debug.json remained absent at final check. The full 1,286-frame corpus is not signed off; earlier candidate results are not substituted for it.
  - No physical-phone or end-to-end speedup claim.

OVERALL: NOT-SHIP. Revise items 0 and 2; retain experimental flags off. Item 15’s intentional deferral is not a release objection.
╰──────────────────────────────────────────────────────────────────────────────╯

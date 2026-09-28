╭─ ⚕ Hermes ───────────────────────────────────────────────────────────────────╮
Reviewed through 26305f4. No repository files modified. References below are relative to /tmp/bd-s2.

BLOCKERS

R1-B2 — NOT FIXED: switching identity sources still creates false settle evidence.
  frontend/src/utils/clientScan.js:188-203
  shared/clientScan/pipeline.mjs:389-421

The original one-frame freeze now abstains. However, this sequence still admits:

  100 ms: decoded frame 7, stillRun=1
  200 ms: decoded frame 8, stillRun=2
  400 ms: repeated frame 8, stillRun=2
  701 ms: unchanged pixels, clock expires, fingerprint ID replaces 8
          stillRun=3; ready/ok; OCR runs

Changing identifier namespaces is not a third decoded observation. Preserve comparable freshness across the transition, or restart settling when changing identity sources. Protect the reverse transition too.

Resolved R1 blockers:

  R1-B1 — FIXED.
    frontend/src/utils/fastScan.js:190-212
    frontend/src/components/FastScanner.jsx:459-461

    Suppressed repeats preserve the break and original timestamp. The adapted lift/repeat probe produces exactly one second addition at 5,000 ms. Two motion passes do not break presence; three do.

  R1-B3 — FIXED for the reported ambiguity escape.
    shared/clientScan/pipeline.mjs:179-191,493-494,588-614,632-634

    Conflicting rescue/retro evidence now abstains. The additional pooled-evidence probe also abstains. Real replay of 20260924-010129-557.jpg retains both 080/505 and 089/59% evidence, sets rescue_conflict=1, and abstains instead of returning DRC 89.

  R1-B4 — FIXED.
    frontend/src/components/FastScanner.jsx:311-330,479-481

    Failed hydration retains the proven row, unsendable and marked failed. Automatic retry hydrates that same row without rereading the card; quantity survives. Send excludes failed/pending rows. Retry scheduling has a separate issue below.

SHOULD-FIX

R1-S1 — NOT FIXED; partially improved.
  frontend/src/utils/fastScan.js:166-177
  frontend/src/components/FastScanner.jsx:322-326,444-446,464-480

Epochs now increase across lifts, repeats no longer clear presentation state, and hydration events correlate with rows/presentations. Remaining problems:

  - Three same-place motion passes break dedupe presence but leave the presentation epoch unchanged. A second accepted copy can retain epoch 1.
  - nextPresentation receives end-of-pass time, not separate candidate/title-completion timestamps. First-pass success reports effectively zero presentation age/title delay.
  - pending_rows distinguishes pending additions, but outcome remains 'added'; commit_ms is emitted before setResults, not after a usable React commit.

R1-S2 — NOT FIXED in full: wide preemption fixed; retro preemption remains.
  shared/clientScan/pipeline.mjs:171-174,600-634

The original wide probe now matches v1. But rescue still precedes retro:

  First/wide: unreadable
  Rescue:     lea 161
  Retro:      wizards 117

  Candidate: id-lea, batches [2,6,4,6]
  V1 order:  id-2x2, batches [2,6,4,2]

This is synthetic control-flow evidence, not an observed real-image false positive. It disproves the comment claiming preservation of every original stage.

The guard can also change an old retro success into abstention even when rescue contributes no useful text. That conservative behavior can be appropriate, but it is not old-order equivalence.

Independent real-data check: all 66 baseline successful retro frames retained their IDs, with zero losses or changed IDs. However, every one grew from four to five recognizer calls. Preserve original retro consultation or explicitly gate and validate this ordering change.

S3 — NEW: manual retries multiply persistent retry chains.
  frontend/src/components/FastScanner.jsx:86-87,311-330

Initial hydration failure plus four failed manual retries leaves five live retry timers. Executing and failing them again leaves five timers in the next backoff. In-flight request dedupe merges overlapping requests, not these retry chains.

Use one tracked timer/in-flight lifecycle per row. Dismissal already prevents subsequent requests. This is nonblocking to the restored no-data-loss behavior, but retry scheduling is not bounded per row.

NITS

R1-N1 — NOT FIXED.
  backend/scripts/lanczos-equivalence.mjs:16-17

The benchmark still compares uncached reference work against cached candidate work and does not perform its advertised corner comparison. Five real images again produced zero differing bytes; that establishes pixel equality, not a clean production speedup A/B.

R1-N2 — FIXED.
  frontend/src/utils/clientScan.js:147-154

100 timed-out newFrameAfter calls retain zero completed waiters. A live callback also wakes and drains its waiter.

N3 — Phone cost/cadence claims remain unverified.
  frontend/src/utils/fastScan.js:228-234
  frontend/src/components/FastScanner.jsx:27,40,525-535

pixelPrint performs 117,965 sampled-byte steps. Warm host-Node measurements: p50 0.185 ms, p95 0.264 ms, max 2.077 ms—not phone measurements.

An unsampled-byte change reproduces a collision. Such collisions conservatively skip evidence; they do not manufacture a printing. The blocker is the identity-source transition.

Extracted autoLoop tests confirm bounded missing/stale-rVFC timer fallback for modeled iOS/Android inputs. Any phone reporting eight logical cores nevertheless takes the 8 ms live-rVFC branch, contradicting the blanket “phones keep their cadence” comment. No physical-phone battery, thermal, or low-light validation was performed.

N4 — Failed rows still animate.
  frontend/src/index.css:1906-1907

The later is-pending animation rule has equal specificity and overrides is-failed for rows carrying both classes.

PER-ITEM VERDICTS

  0  DROP      Dedupe fixed; presentation measurement incomplete.
  1  FLAG-OFF  fastSettle default off; decoded-video validation absent.
  2  DROP      Ambiguity escape fixed; retro ordering still changed.
  3  FLAG-OFF  Title rejection threshold remains uncalibrated.
  4  SHIP      Pixel equality retained; qualify benchmark claims.
  5  SHIP      LUT/reused-buffer equivalence regressions pass.
  6  DROP      Identity-source transition admits frozen pixels.
  7  FLAG-OFF  Same experimental titleGate gate as item 3.
  8  SHIP      Reset/no-card deferral tests pass.
  9  SHIP      Guidance and whole-stage coverage checks retained.
 10  SHIP      Failure-signature/deadline regressions pass.
 11  SHIP      Bounded text/index memo unchanged; suite passes.
 12  SHIP      Memoized tray and stable callbacks retained.
 13  SHIP      Failed scans retained/retryable; coalesce retry timers.
 14  SHIP      Thread settings reported; no default increase.
 15  DROP      Hardware acceleration remains deferred.

VERIFICATION

  - npm test: 149 passed, 0 failed; locale checks passed.
  - Targeted ESLint and git diff --check passed.
  - Both original R1 scripts rerun. The original scanner probe stops at the now-fixed wide-order assertion; the original UI probe lacks the new HYDRATE_RETRY_MS dependency.
  - Adapted scanner, UI, and supplemental probes completed, asserting both fixes and remaining failure cases.
  - Supplied cand3 results: 49/71 -> 55/71; 0/5 -> 5/5; harness 8/8 -> 8/8. No baseline successful IDs lost or changed; zero compared printing/title conflicts.
  - Independent 67-frame replay: all 66 baseline retro successes retained, plus abstention on the Damn conflict. There were 23 exact server-printing agreements; 43 matches lack exact same-frame server truth.
  - cand3-debug.json remained absent at the final check; its replay was progressing. The full 1,286-frame corpus is NOT signed off. Its baseline already has two printing conflicts.
  - No end-to-end speedup or physical iOS/Android validation claimed.

Full report and reproducer artifacts:
  /tmp/astra-r2rev2/REVIEW.txt
  /tmp/astra-r2rev2/probes.mjs
  /tmp/astra-r2rev2/ui-probes.mjs
  /tmp/astra-r2rev2/supplemental.mjs

OVERALL: NOT-SHIP
╰──────────────────────────────────────────────────────────────────────────────╯

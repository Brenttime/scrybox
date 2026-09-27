╭─ ⚕ Hermes ───────────────────────────────────────────────────────────────────╮
BLOCKERS

B1. Whole-stage reprojection can prove the wrong printing.
    shared/clientScan/pipeline.mjs:386-400, 479-483

    The fit check guarantees in-frame pixels, not that those pixels still belong to the footer. Shrinking bottom padding moves every footer row upward and can exclude the actual collector line.

    Reproduced with real PP-OCRv6 inference and the unmodified pipeline, using an explicitly synthetic card and two-printing index:
    - Centered: reads the actual footer, “2X2 147”; returns actual-2x2.
    - Same card translated toward the bottom edge: selects ['footer0','b',0.25], reads unrelated text “LEA 161”; returns wrong-lea with ok:true.
    - Reprojection disabled: correctly abstains at footer0.

    All six strips were retained. Their combined vertical extent changed from 90.06%-104.00% of the true card height to 85.08%-98.28%, admitting text-box content while missing the low footer. The ambiguity guard cannot reject evidence it never sees.

    This is a demonstrated geometry failure, not an observed wrong identification of a real production card. The batch-size-only fake recognizer at frontend/src/utils/scanAudit.test.js:310 cannot detect it.

    Repro:
      node /tmp/astra-r1/geometry.mjs

    Require footer-region validation or abstain when reprojection cannot preserve the relevant evidence. Merely retaining the strip count is insufficient.

B2. Preserved backoff can starve server rescue indefinitely.
    frontend/src/utils/fastScan.js:97-106
    frontend/src/components/FastScanner.jsx:294-315
    shared/clientScan/pipeline.mjs:349-359, 491-493

    nextFailStreak counts local deferred results as failures and advances the deadline. Near-edge frames correctly discard pooled evidence, so unresolved reads alternate between deferred and full reads. Each deferred read renews the deadline; each full read is held. Settling passes now preserve this cycle instead of breaking it.

    Reproduced using createReader plus the actual extracted scan callback, with scripted OCR and a controlled clock modeling 600 ms reads:
    - Current helper: zero server requests over 22,600 ms.
    - Original nextFailStreak, otherwise identical execution: requests at 1,500 ms, 2,900 ms, and subsequent intervals.
    - The cycle has no eventual-release condition while these outcomes continue.

    Thus the nominal three-second cap is not a cap on waiting. A same-title replacement needing server-only footer rescue can remain blocked indefinitely. Different-title or sufficiently moved full-read observations bypass the old streak, but deferred observations can establish this starvation cycle themselves.

    Repro:
      node /tmp/astra-r1/probes.mjs

    Advance the failure budget only for actual failed server attempts, or otherwise prevent local deferrals from renewing its deadline.

SHOULD-FIX

S1. Moving observations do not reset the edge run.
    frontend/src/components/FastScanner.jsx:292-293
    frontend/src/utils/fastScan.js:129-135

    The component explicitly skips nextEdgeRun for “moving,” overriding the helper’s reset behavior.

    Probe: seven edge partials, a moving observation far away, then a partial at the original position sends a server request immediately. The count remained seven throughout movement. That is not eight consecutive same-place partials.

    Preserve settling only when appropriate; invalidate the run on observed movement.

NITS

N1. The universal faster-cadence claim is false.
    shared/clientScan/pipeline.mjs:69-74
    frontend/src/utils/scanAudit.test.js:363-373

    Probed admission:
    - 65 ms cadence: 195 ms; 105 ms cadence: 210 ms — intended improvement confirmed.
    - 89 ms cadence: 267 ms; 90 ms cadence: 180 ms — faster can still admit later due to sampling phase.

    Describe this as removing the old observation-spacing aliasing, not guaranteeing monotonic admission across all cadences.

VERIFICATION

- Change 1 confirmed: y=.84 is in the six-strip first modern batch, with one recognizer call for that batch (pipeline.mjs:76,110,181-205).
- Change 2 partially confirmed: settling preserves backoff; edge partials stay local below eight; the edge hint exists. B2 and S1 refute the claimed bounded/reset behavior.
- Change 3 structurally confirmed: whole stages, specified shrink factors/sides, and first-skipped-stage reporting (pipeline.mjs:375-400). B1 refutes printing safety.
- Change 4 confirmed for initial admission and detected movement: no OCR through 179 ms, admission at 180 ms; movement at 250 ms prevents OCR until 430 ms. env.clock controls admission, and readSince requires recognizer calls (pipeline.mjs:271-289,353).

- Near-edge tracking/pooling remains disabled (pipeline.mjs:309,359); contradictory exact set+number matches still return null (text.mjs:278-281).
- Titleless edge probes sent once per eight partials, not every frame. No extra per-frame server request was introduced.
- Required tests: 32 passed, zero failed. Additional clientScanText tests: 14 passed.
- Real 71-frame replay: 49 client matches, ten agreements with server printing, zero compared printing/title conflicts. The other 39 matches lack exact server ground truth.
- No separate top-title wrong-printing reproduction was found; bottom-footer reprojection alone demonstrates the safety failure.
- Worktree unchanged. All probe artifacts are under /tmp/astra-r1/.

VERDICT: NO-SHIP
╰──────────────────────────────────────────────────────────────────────────────╯

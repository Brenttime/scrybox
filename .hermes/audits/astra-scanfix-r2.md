╭─ ⚕ Hermes ───────────────────────────────────────────────────────────────────╮
BLOCKERS

None remaining under the stated 0.006 coverage tolerance.

B1 — FIXED within that tolerance.
  shared/clientScan/pipeline.mjs:103-120,419-428

  The real-OCR adversarial probe now abstains at footer0 instead of returning wrong-lea. Centered, the same synthetic card still returns actual-2x2.

  Independently checked all 100 default stage/side/shrink combinations using 2-D rectangle-union containment: 42 accepted, none loses coverage beyond 0.006. This includes both title stages, narrow/wide/retro footers, and lr. Unsafe lr title/retro fits are rejected; accepted lr footer fits preserve their on-card coverage.

B2 — FIXED.
  frontend/src/utils/fastScan.js:100-108
  frontend/src/components/FastScanner.jsx:299,315-316

  Local deferrals preserve the existing deadline; only server outcomes start/extend it. Hits and no-card outcomes clear it. Re-running the extracted callback with stateful per-pass telemetry produced seven server requests over the modeled 22,600 ms, versus zero in R1. Different-title and moved-card observations still bypass backoff.

SHOULD-FIX

None remaining.

S1 — FIXED.
  frontend/src/components/FastScanner.jsx:292-294

  Seven edge partials followed by movement now clear the run. Returning to the original position does not immediately request the server. Settling still preserves the run; uninterrupted titleless partials still request once per eight observations.

NITS

N1 — NOT FIXED; acceptable as a nonblocking documentation nit.
  shared/clientScan/pipeline.mjs:69-74
  frontend/src/utils/scanAudit.test.js:363-373

  The universal faster-cadence claim remains false: 89 ms cadence admits at 267 ms, while 90 ms admits at 180 ms. Describe removal of observation-spacing aliasing, not monotonic admission across cadences.

N2 — “Never on-card rows” requires the tolerance qualification.
  shared/clientScan/pipeline.mjs:95-100,117

  Literal background-only removal is not guaranteed. Accepted title1/t/0.75 loses the top 0.065% of card height; footer1/b/0.75 loses the bottom 0.098875%. Both are within the declared tolerance. The tested guarantee is coverage preservation within 0.006, not zero on-card loss.

VERIFICATION

  Real replay: 71 frames, 49 client matches versus baseline 3; all baseline hits retained with unchanged IDs. Ten exact server-printing agreements, zero compared printing/title conflicts. The other 39 matches lack exact server-printing ground truth.

  Reprojection remains useful: 36 frames use it, and 33 match. Roiling Canopy uses title1/t/0.75; Solarium Sentry uses footer0/b/0.5, with one frame also using footer1/b/0.75. These frames abstained in the supplied baseline. No lr reprojection occurred in this replay; lr coverage was checked geometrically.

  All 49 targeted tests passed. Both adapted R1 probes passed. HEAD’s diff matches /tmp/scanfix-r2.diff. No repository files modified; artifacts are under /tmp/astra-r2/, including cand.json, geometry.json, probes.json, and coverage.json.

VERDICT: SHIP
╰──────────────────────────────────────────────────────────────────────────────╯

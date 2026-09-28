╭─ ⚕ Hermes ───────────────────────────────────────────────────────────────────╮
Reviewed b1256ac. No repository edits; scratch and report:
  /tmp/astra-r2rev5/REVIEW.txt

Source paths below are relative to /tmp/bd-s2.

BLOCKERS

None reproduced. The full replay remains outstanding: /tmp/s2rep/cand4-debug.json was still absent; its process was observed running.

SHOULD-FIX

No outstanding code-level should-fix reproduced.

R4-S1 A — FIXED
  frontend/src/components/FastScanner.jsx:388-396,467-473

  Reran the actual extracted scan handler:
  Capture=100, hedged server title=200, local failure=900.
  Now title_ms=100, previously 800.
  Sequential fallback and server-only cases still report 900 correctly.

R4-S1 B — FIXED
  frontend/src/utils/fastScan.js:165-174
  frontend/src/components/FastScanner.jsx:467-473

  Title-less local candidate plus successful server fallback now reports title_ms=900, previously absent, and adds the row.
  Expanded probes confirm local geometry survives, including server card.name-only fallback. Early hedge plus title-less local reports 100. Local titles retain their own timestamp.

NITS

R4-N1 — FIXED
  frontend/src/utils/fastScan.js:179-182
  Comment correctly describes title-stage/server-response completion.

R2-N3 cost disclosure — FIXED
  frontend/src/utils/fastScan.js:248-250
  Hash cost is explicitly host-only, not phone-measured.

Physical-device validation — OUTSTANDING, nonblocking with experimental flags off.
  frontend/src/components/FastScanner.jsx:27,550-568
  No physical-phone thermal, battery, low-light or throughput validation.

PER-ITEM VERDICTS 0-15

 0 SHIP — Title timing/input fixed.
   frontend/src/components/FastScanner.jsx:467-473
 1 FLAG-OFF — Experimental fastSettle.
   frontend/src/utils/clientScan.js:66-72
 2 SHIP — Deferred pooled boundary preserved; replay gate applies.
   shared/clientScan/pipeline.mjs:588-626
 3 FLAG-OFF — Uncalibrated title gate.
   shared/clientScan/pipeline.mjs:559-563
 4 SHIP — Lanczos equality; benchmark qualified.
   backend/scripts/lanczos-equivalence.mjs:1-4,18-22
 5 SHIP — LUT/buffer equivalence passes.
   shared/clientScan/pipeline.mjs:257-287
 6 SHIP — Freeze/source-transition/resumption probes pass.
   shared/clientScan/pipeline.mjs:386-425
 7 FLAG-OFF — Same title gate as item 3.
   shared/clientScan/pipeline.mjs:559-563
 8 SHIP — Reset/no-card deferral passes.
   shared/clientScan/pipeline.mjs:661-669
 9 SHIP — Guidance/geometry protections retained.
   shared/clientScan/pipeline.mjs:469-475,513-536
10 SHIP — Failure-signature/deadline tests pass.
   frontend/src/utils/fastScan.js:101-135
11 SHIP — Bounded index-scoped memo retained.
   shared/clientScan/text.mjs:135-150
12 SHIP — Memoized tray/stable callbacks retained.
   frontend/src/components/FastScanner.jsx:77,609-610,680
13 SHIP — Failed rows retained; retries bounded per row.
   frontend/src/components/FastScanner.jsx:315-347
14 SHIP — Thread reporting retained; no default increase.
   frontend/src/utils/clientScanWorker.js:22-29,85
15 SHIP — Document-only deferral; WASM unchanged.
   frontend/src/utils/clientScanWorker.js:12,75

VERIFIED

- npm test: 152 passed, 0 failed; locale checks passed.
- Targeted ESLint and git diff --check passed.
- Original timing acceptance cases and expanded timing/failure probes passed.
- Scanner, UI, supplemental and deferred-stage regression probes passed.
- Five real-image Lanczos comparisons: zero differing bytes.
- Available cand4 smaller corpora retain zero lost/changed successful IDs and zero new compared wrong printings. These do not replace the full replay.

OVERALL: SHIP conditional on the full replay showing 0 lost / 0 new wrong vs /tmp/s2rep/base-debug.json. Keep items 1, 3 and 7 FLAG-OFF. The full replay is the only remaining release condition from this review.
╰──────────────────────────────────────────────────────────────────────────────╯

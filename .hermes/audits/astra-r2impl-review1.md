╭─ ⚕ Hermes ───────────────────────────────────────────────────────────────────╮
Reviewed through c83c662, including the frame-clock follow-up added during review. No repository files modified. References below are relative to /tmp/bd-s2.

BLOCKERS

B1. A genuine second copy can be suppressed indefinitely.
frontend/src/utils/fastScan.js:186-203
frontend/src/components/FastScanner.jsx:453-454

dedupeFresh requires both a four-second gap and broken=true. notePresence then overwrites the entry for every recognized hit—including suppressed repeats—clearing the lift marker and renewing the timestamp.

Reproduced: add at 0; observed lift at 500 ms; second copy recognized at 1,000 ms and repeatedly through 20,000 ms. No second addition. This is not merely a four-second delay.

Consume the presentation break only when a new copy is actually committed; a suppressed read must not erase it.

B2. The stale-rVFC fallback counts frozen pixels as settle evidence.
frontend/src/utils/clientScan.js:183-187
shared/clientScan/pipeline.mjs:368-399

c83c662 changes frameId to null when callbacks are 500 ms old. That bypasses duplicate detection and resumes stillRun increments.

Reproduced with the actual frame-ID expression and real reader: one decoded frame, ID 7, at t=100; duplicate observations at 200/400; null IDs at 601/701. At 701 the reader returns ready/ok and runs OCR despite receiving no new decoded frame.

The no-rVFC path likewise admits identical frames at 0/100/200 ms. Keep timer scheduling, but establish freshness separately. A stopped callback must not turn known duplicate pixels into independent evidence.

B3. Conflicting footer identities can escape through the retro resolver.
shared/clientScan/pipeline.mjs:573-599, especially 584-586

Reproduced: rescue reads “lea 161 117” against two same-title LEA printings. That is ambiguous. Later “wizards 117” resolves printing 117 because the retro branch considers only its own numbers—even though footer_ocr still contains both identities.

The retro exit is pre-existing, not a newly introduced line. Nevertheless, the new rescue does not uphold its claimed conflicting-evidence guarantee. Ambiguity must survive every resolution exit.

B4. Background hydration failure silently discards an acknowledged scan.
frontend/src/components/FastScanner.jsx:306-322,467-473

The scanner flashes/vibrates and inserts a pending row, then deletes that row if hydration fails. Recovery requires a future physical reread. If the user has already moved on, the proven card disappears without a retry row or visible failure notification.

Executing the actual hydrateRow handler with a delayed rejection removed the only row. Preserve the proven ID in an unsendable failed/pending state and support hydration retry without needing the card again.

SHOULD-FIX

S1. Presentation telemetry still cannot identify presentations reliably.
frontend/src/utils/fastScan.js:164-173
frontend/src/components/FastScanner.jsx:438-466

No-card returns null, so the next presentation starts at epoch 1 again. Successful and repeated hits also clear presRef. The probe reports epoch 1 before and after a lift.

Additionally:
  - titleAt uses pass start, not title completion.
  - Pending rows already receive outcome='added'.
  - commit_ms precedes the React commit.
  - Hydration events lack presentation/row correlation.

Keep a monotonic run-scoped epoch independently of current presence, and distinguish proof, pending insertion, hydration, and usable commit.

S2. Rescue now bypasses previously consulted disambiguating stages.
shared/clientScan/pipeline.mjs:171,594-599

Scripted probe:
  - First footer: unreadable.
  - Rescue: “lea 161”.
  - Original wide stage: “2x2 117”.

Candidate returns id-lea after batches [2,6,6]; the original stage sequence returns id-2x2 after [2,6,4].

This demonstrates control flow, not an observed real-image false positive. Preserving earlier OCR does not preserve conflicting evidence from stages no longer executed. The requested competing-number/mixed-layout validation remains necessary before enabling this unflagged insertion broadly.

NITS

N1. Lanczos timing is not a clean production A/B.
backend/scripts/lanczos-equivalence.mjs:16-17
shared/clientScan/imaging.mjs:220,248

The reference recomputes weights and allocates scratch; the candidate caches them. The old production implementation already cached both. Pixel equality is useful evidence, but this benchmark cannot isolate the optimization’s production savings. It also does not directly run the advertised corner comparison.

N2. Completed frame waiters remain registered.
frontend/src/utils/clientScan.js:150-153

100 direct timed-out newFrameAfter calls leave 100 completed waiters until another callback fires. Remove waiters on timeout. The latest stale-clock guard limits accumulation through the current auto caller; this is not evidence of unlimited normal-session growth.

PER-ITEM VERDICTS

DROP means revise and re-review, not abandon the idea.

  0    DROP      Second-copy suppression; incomplete presentation telemetry.
  1    FLAG-OFF  Default OFF verified; requires decoded-video validation.
  2    DROP      Footer ambiguity escape and rescue-stage preemption.
  3    FLAG-OFF  Contrast measured; rejection threshold not calibrated.
  4    SHIP      Pixel equivalence passed; performance claim needs cleaner A/B.
  5    SHIP      LUT, reused-buffer and padding equivalence tests pass.
  6    DROP      Frozen frames become settle evidence after clock expiry.
  7    FLAG-OFF  Same experimental gate as item 3.
  8    SHIP      Reset/no-card deferral regressions pass; UI counter reset present.
  9    SHIP      Guidance only; whole-stage reprojection/coversStage preserved.
 10    SHIP      Same-frame failure signature and bounded deadline tests pass.
 11    SHIP      Bounded text/index memo; reference equivalence passed.
 12    SHIP      Stable memoized tray props; no auto busy=true toggles.
 13    DROP      Hydration failure loses scans; pending Send exclusion itself passes.
 14    SHIP      Reports existing override/effective threads; no default increase.
 15    DROP      Deferred hardware work; no justified acceleration claim.

VERIFICATION

  - npm test: 143 passed, 0 failed; locale checks passed.
  - Targeted ESLint and git diff --check passed.
  - Five real images: Lanczos produced zero differing bytes.
  - Title equivalence: 5,940 comparisons, zero mismatches.
  - Independent five-image replay: 5/5 matches, one exact server agreement, zero compared conflicts. Four images lack same-frame server truth.
  - Completed supplied replay pairs: 49/71 -> 55/71; 0/5 -> 5/5; harness 8/8 -> 8/8. No baseline successful ID changed.
  - Actual extracted Send handler sent only the hydrated row. Pending rows remained unsendable. Concurrent same-ID hydration shared one request.
  - The 1,286-frame debug candidate remained incomplete; its baseline already reports two printing conflicts. That corpus is not signed off.
  - No iOS/Android device trial or labeled-video admission validation performed. No end-to-end speedup claimed.

Reproducers and full report:
  /tmp/astra-r2rev/probes.mjs
  /tmp/astra-r2rev/ui-probes.mjs
  /tmp/astra-r2rev/REVIEW.txt

OCR and network probes use explicit synthetic fixtures to demonstrate control-flow bugs, not real-image error rates.

OVERALL: NOT-SHIP
╰──────────────────────────────────────────────────────────────────────────────╯

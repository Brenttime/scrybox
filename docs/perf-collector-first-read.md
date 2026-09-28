# Collector-number first-read: replay report

Branch: perf/collector-first-read (from main 7d78227). Offline only; not deployed.

## Change
The first footer batch (rows 0.84-0.94, x 0-0.22) is now read 0.030 tall instead of 0.025 (the new constant `FIRST_H` in shared/clientScan/pipeline.mjs).
iPhone footers were being clipped at the digits' lower edge, which caused misreads like "8.0244" for R 0044, "6.0139" for C 0129, and ".9278" for U 0278.
The 'tall' stage (same rows) moves from 0.030 to 0.025, so the two batches never sample identical crops. All other stages are unchanged.

## Replay
Command: `client-scan-replay.mjs`, run on every saved frame in /tmp/dbg (1,286-1,288 frames).

| variant | matches | first-read proofs | wrong | rec calls | vs base |
|---|---|---|---|---|---|
| base (0.025) | 652 | 321 | 2 | 3987 | - |
| first 0.030, tall 0.030 (rejected by review) | 658 | 357 | 2 | 3933 | +6, but 4 of those are identical-crop duplicate reads counted as corroboration |
| first 0.028 | 656 | 351 | 3 | 3946 | 1 new wrong, 2 lost |
| first 0.032 | 657 | 359 | 2 | 3929 | 1 lost |
| 0.030 + tall 0.033 | 658 | 357 | 2 | 3930 | 2 lost |
| 0.030 + tall 0.035 | 658 | 357 | 2 | 3929 | 3 lost |
| **first 0.030, tall 0.025 (chosen)** | **654** | **357** | **2** | **3945** | **+2 matches, +36 first-read proofs, 0 lost/changed/new wrong** |

- Determinism: base and 0.030 were each rerun, and the reruns gave identical hits and printings.
- 50 frames move from a later stage to a first-read proof; 16 move the other way (they are still matched).
- The 2 "wrong" answers are pre-existing and identical in both runs.

## Review
gpt-6-astra (read-only): BLOCKER on first=tall=0.030 (strongNumbers counts one crop read twice as corroboration); VERDICT: SHIP B (first 0.030, tall 0.025). Full text: ~/projects/bindarr/.hermes/audits/astra-collector-first-read.md. A regression test now asserts the two heights differ.

## Tests
- Frontend: 161 pass, 0 fail, plus the locale check.
- Backend: `npm test` passed.

## Not verified
Not tested on a real iPhone yet. Nothing was deployed and main was not touched.

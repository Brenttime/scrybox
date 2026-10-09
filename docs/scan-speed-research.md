# Scan speed research (2026-10-07)

Goal: old-frame cards and The List / Mystery Booster 2 reprints in 1 s
(2 s at most) from appearance to added, without touching the modern
footer path that already works.

## What the 2026-10-07 Windows session showed

Telemetry `scan-telemetry/2026-10-07.jsonl` (sessions `17lonf8t`,
`uyjzrbje`, 22:41-22:45Z, Chrome on Windows, 2560x1440 capture, 1920x1080
frames, 2 ORT threads). Each pass was attributed by wall clock, and every
debug frame was checked by eye.

| Card | Wall time | Where the time went |
|---|---|---|
| Smokestack (MB2 232), presented 6 times | 14-16.5 s each, never added | **Title never matched.** OCR read `Smokestack` at 0.91-1.00 conf on every pass. The client index listed the title as *excluded*. Scryfall's `oracle-cards` bulk keeps one representative printing per card; Smokestack's is VMA (digital), so `build_oracle_index` dropped the title. 29-30 `no confident card title` passes per presentation, plus a server fallback every 4th pass (7 per presentation) that failed the same way. |
| Hatchet Bully (plst EVE-54) | 17.4 s, never added | Title fine. The 2008 footer (`Paul Bonner`, `1993-2008`, `54/180` at the far right) never gave a number. 8 server fallbacks of about 1.9 s each. |
| Chain of Smog (plst ONS-132), 3 presentations | 15.1 s / 12.6 s / 6.3 s | Retro footer read `192 350` / `132 350` intermittently. Then the twin picker (`ons 132 | plst ONS-132`). 2-4 server fallbacks of 2.2 s each, plus `held` backoff. |
| Woodland Changeling (plst LRW-242) | 11.6 s | Footer `&1993-2007`, `inkel272`: the number arrived only after 7 passes and 3 server fallbacks (about 2.5 s each), then the picker. |
| Whip Silk / Terrain Generator (plst) | 2.1 s each | Retro line read on the 2nd frame, then the picker. |
| Hullbreacher (MB2 124) | 2.0 s, and a 16 s re-presentation | Footer `3/2` only. Server fallbacks. |
| All-Star Kicker, Werewhat (MB2) | 0.40-0.46 s | Unique title: the fast path, unchanged. |

Root causes of the 10+ s:
1. **A data bug, not OCR** (Smokestack, and 1,060 paper titles in total,
   e.g. Survival of the Fittest, Taiga, Contagion, Cataclysm, Zombie
   Master, Mox Jet): titles whose Oracle representative is digital were
   unmatchable on both the client and the server.
2. **List cards wait on the footer.** The only physical difference between
   a List card and its original is the bottom-left stamp. Even a good
   footer read therefore ends at the picker, and on 1997-2008 frames the
   footer number is the slowest and least reliable read (5-8 passes plus
   server fallbacks at 2-2.5 s each).
3. Server fallbacks cost 1.9-2.5 s each on these frames (client read, then
   the sidecar's full sweep, in sequence). Backoff then adds `held` gaps.

## What others do (sources)

- **Perceptual hashing of the whole card or the art** is the classic
  open-source approach: tmikonen/magic_card_detector
  (<https://github.com/tmikonen/magic_card_detector>,
  <https://tmikonen.github.io/quantitatively/2020-01-01-magic-card-detector/>)
  and hj3yoo/mtg_card_detector
  (<https://github.com/hj3yoo/mtg_card_detector>: YOLO plus pHash,
  20-25 fps detection; a pHash scan of ~10k cards takes 23-66 ms). A hash
  finds the *art*, not the printing: reprints share art, so it can only
  shortlist (as the never-guess rule already says).
- **Commercial scanners read the whole card image, including the set
  symbol and frame**, to pick the printing. Text-only (name-first)
  scanners are known for the "wrong set" problem: Lotus Scan FAQ,
  <https://www.scanyourmtg.com/faq/wrong-set-detection/>. ManaBox asks for
  a plain, contrasting background and glare-free light for border
  detection: <https://manabox.app/guides/scanner/getting-started/>.
  OCR-only pipelines (fortierq/MTGScan,
  <https://github.com/fortierq/mtgscan>) stop at the name.
- **The List / MB2 stamp**: List cards are "printed as they previously
  appeared (including art, card frame, expansion symbol and collector
  number) with the exception that they'll have a small planeswalker stamp
  in their lower-left corner" (<https://mtg.wiki/page/The_List>). So the
  stamp is the only physical discriminator, and it is the same mark for
  every set. That makes a single template work set-agnostically.
- **Higher-resolution stills**: `ImageCapture.takePhoto()` gives a
  full-sensor still (<https://developer.mozilla.org/en-US/docs/Web/API/ImageCapture/takePhoto>),
  but Safari only gained ImageCapture recently and support is incomplete
  (<https://github.com/mdn/browser-compat-data/issues/27078>). A takePhoto
  costs a few hundred ms of shutter, and the stream is already
  2560x1440 -> 1920. Not worth it for the target.
- **Super-resolution for small text** improves OCR on low-res inputs
  (<https://ar5iv.labs.arxiv.org/html/1506.02211>), but it adds a model
  run per strip. Our footer misses on these frames were mostly
  missing/garbled digits on old frames, which the stamp makes
  unnecessary for List cards.

## Ranked approaches (ms saved per accuracy risk)

1. **Index every paper title** (server `load_printings`, flows into the
   client index). This removes the Smokestack-class 14-16 s loops outright
   (never added -> one modern footer read). Risk: new titles could attract
   fuzzy matches. They are real, printed paper card names, the same class
   as the rest of the index; the replay diff below checks this.
2. **List-stamp template check** (client `shared/clientScan/stamp.mjs`,
   about 5-12 ms, run only when the first footer batch has NOT resolved a
   title that has a reprint-sheet printing, so already-fast cards never
   pay for it). Only a strong *present* stamp counts as evidence:
   - footer twins with a present stamp resolve to the sheet printing
     without the picker;
   - title + present stamp + exactly one `<SET>-<N>` sheet printing (and no
     printing from a set that stamps under its own numbering: cmb1, cmb2,
     ulst, mb2, mbc) resolves to it after one footer batch, unless any
     number read so far, on this frame or a pooled one, or any validated
     copyright-line number points elsewhere.
   An absent stamp never selects the original. Astra R1 showed that a
   covered or glared stamp reads as "absent", so the original still needs
   its footer or the picker.
3. Rejected in review: moving the copyright-line (retro) stage ahead of tall/wide on a digit-free first batch (Astra R1-B4: it could settle a modern card before tall/wide read a conflicting number).
4. Not shipped: an art-hash shortlist (it does not separate twins, and
   the title already shortlists), takePhoto / higher resolution (iOS
   support, shutter cost), and super-resolution (cost per strip, not
   needed once 2 is in).

## Stamp detector evidence

- Template: the mean |plst - original| difference over 67 random Scryfall
  original/plst pairs, card space 500x700; the stamp is at x 12-32,
  y 655-676. The detector searches +-12 px.
- Two measures must agree: NCC against the template (present >= 0.60,
  absent < 0.50) and masked stamp-vs-ring contrast (present >= 0.90,
  absent < 0.80). `absent` also needs crop sharpness >= 25. The
  footer-free answer needs NCC >= 0.66 and contrast >= 1.10.
- Scryfall references, Gaussian blur 0-6: an unstamped card was never
  called present. Before the sharpness floor, 3/67 stamped cards were
  called absent at blur 6; with the floor, none were.
- Real frames: today's webcam session gave 12 present, all of them List
  cards (Hatchet Bully, Chain of Smog, Woodland Changeling), and 35
  absent, all of them unstamped (Smokestack MB2, Hullbreacher MB2,
  others). The one unknown was a hand-blurred Chain of Smog. In the older
  corpus (675 sampled frames, `--every 2`) there were 7 present: 6 were
  List cards (verified by eye), and 1 was a blurred, hand-covered quad
  that scored below the footer-free margin. Frames whose labelled
  printing has a List twin (MH2, SNC originals) all scored absent.

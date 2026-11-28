# On-device Scan Cards

Scan Cards can read a card on the phone itself. The phone proves the
printing, the backend only turns the Scryfall id into a card row (prices, art),
and the cardscan sidecar is asked only for what the phone cannot prove.

## How a frame is handled

1. `FastScanner` grabs the frame (same 1920 px ceiling as the server upload)
   plus a 384×384 copy, and hands both to `utils/clientScanWorker.js`.
2. The worker runs `shared/clientScan/pipeline.mjs`:
   - **cornelius** (already used by the scanner, `/models/cornelius.onnx`) predicts
     the card corners. The quad is then padded to the sidecar's crop margins, so
     all of its strip fractions carry over unchanged.
   - **Gates:** a card touching the frame edge (1%), a blurry title band, or (in
     Auto) a card still moving between passes is not read. The next pass retries.
   - **Title:** the sidecar's title strips go through the PP-OCRv6 recognizer
     (RapidOCR preprocessing: height 48, `(x/255-0.5)/0.5`, BGR, batches of 6, CTC
     greedy decode). The read is then fuzzy-matched with the same rules as
     `find_card_by_ocr`, including rapidfuzz `ratio`.
   - **Printing:** a title with one printing, or a unique printed alias, resolves
     right away. Otherwise the footer is read in stages, stopping at the first
     proof: 0.90/0.92, then the retro copyright line, then 0.86-0.94, then
     0.76-0.84. The result must be **exactly one** printing. It is never the
     likeliest printing of a name.
   - A number **without a set code** counts only when it is printed like a
     modern footer (`080/303`) or agrees across two strips. This is stricter
     than the sidecar, and it was added after the first replay produced two
     wrong printings from noisy digit runs.
3. A proven result goes to `POST /api/cardscan/cards`, which hydrates it the
   same way `/frame` does. Everything else goes up as the same frame to
   `POST /api/cardscan/frame`: an unproven card, no card on a shutter press,
   or a worker/model failure. The fallback rules are in `needsServer()`
   (`utils/fastScan.js`, unit-tested).

## Assets

Served from `/scan-assets/` out of `CLIENT_SCAN_DIR` (default
`<CV_MODEL_DIR>/client-scan`). If the directory is missing, the scanner stays on
the server path.

| file | size |
|---|---|
| `PP-OCRv6_rec_small.<hash>.onnx` | 21.2 MB |
| `scan-index.<hash>.json.gz` | 3.9 MB |
| `rec-dict.<hash>.txt` | 75 KB |
| `manifest.json` | tiny, `no-cache` |

The hashed files are served `immutable` and are also kept in the Cache API
(`scrybox-scan-assets`), so a returning phone downloads nothing.
cornelius (3.2 MB) is already cached by the live detector. The download
starts when the camera is started, not when the tab opens.

Generate the assets with the cardscan repo (it reuses the sidecar's own loaders,
so exclusions, aliases and printing keys cannot drift):

```sh
cd ~/projects/cardscan
.venv/bin/python tools/build_client_index.py <CLIENT_SCAN_DIR>
```

Re-run it after the sidecar's printing index is refreshed for new sets.

## Validating

The Node replay harness (`client-scan-replay.mjs`) runs the same pipeline module under
onnxruntime-node against saved phone frames (sidecar `CARDSCAN_DEBUG_DIR`
output). It scores each frame against the sidecar's saved answer and exits
non-zero on any wrong printing:

```sh
node client-scan-replay.mjs <CLIENT_SCAN_DIR> ~/projects/scrybox-host/cardscan/debug
```

## Limits

- One card per frame (cornelius predicts one quad). Multi-card scenes fall back
  to the server.
- The wasm EP is single-threaded (there is no COOP/COEP), so a phone runs ~4
  recognizer batches per new card.

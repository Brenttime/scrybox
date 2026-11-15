# In-process cardscan OCR differential (TOTORO mono scan)

Reference = the sidecar's own Python in a THROWAWAY container from the same
image (`cardscan:local-scanspeed`), never the production `cardscan` container.

1. Corpus: sample `bindarr-host/cardscan/debug/*.json` live frames with
   candidate boxes -> `$S/corpus/{manifest.json,*.jpg}` (manifest: frame, boxes).
2. Python: `docker run --rm --entrypoint sh -e PYTHONHASHSEED=0 -e CARDSCAN_OCR_THREADS=2
   -v $S/data:/app/data -v $S/appout:/app/out -v $S/apphist:/app/history
   -v $S/corpus:/corpus:ro -v $S/ref:/out -v ref_harness.py:/h.py:ro
   cardscan:local-scanspeed -c 'cd /tmp && python /h.py'` (+ cv_harness.py).
3. Rust: `cargo build --release --bin scan_ocr_eval`, then
   `ORT_DYLIB_PATH=.. SCRYBOX_JPEG_LIB=.. SCRYBOX_OCR_MODEL_DIR=<dir with
   PP-OCRv6_rec_small.onnx> SCRYBOX_CARDSCAN_DATA=<oracle_titles.json dir>
   CARDSCAN_OCR_THREADS=2 target/release/scan_ocr_eval $S/corpus $S/ref > rust.json`
4. `python3 compare.py rust.json`.
golden.py regenerates crates/scrybox-server/tests/fixtures/scan_ocr.

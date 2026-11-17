# In-process cardscan engine differential (TOTORO mono scan2)

Reference = Python sidecar in a THROWAWAY container from `cardscan:local-scanspeed`
(cv2 5.0.0), never the production `cardscan` container. Rust = `scan_engine`
(opencv crate 0.95 on a static OpenCV 4.10 core+imgproc; the two 4.10→5.0
behaviour changes that mattered are ported: `minAreaRect` angle convention
and the new bicubic `warpPerspective` kernel — see detect.rs).

```
S=~/.hermes/cache/scratch/mono-scan2      # data/ = copy of the sidecar data dir
docker run -d --name cardscan-ref-scan2 -p 127.0.0.1:18321:8321 \
  -e CARDSCAN_KEEP_CAPTURES=0 -e CARDSCAN_SCENE_FALLBACK=0 -e CARDSCAN_TITLE_ROTATE=0 \
  -v $S/data:/app/data -v $S/hist:/app/history --entrypoint python \
  cardscan:local-scanspeed server.py 8321 --host 0.0.0.0
python3 py_drive.py $S/corpus http://127.0.0.1:18321 $S/diff   # requests + py.jsonl
cargo build --release --bin scan_engine_eval
ORT_DYLIB_PATH=.. SCRYBOX_JPEG_LIB=.. SCRYBOX_OCR_MODEL_DIR=.. SCRYBOX_CARDSCAN_DATA=$S/data \
  CARDSCAN_KEEP_CAPTURES=0 CARDSCAN_SCENE_FALLBACK=0 CARDSCAN_TITLE_ROTATE=0 \
  target/release/scan_engine_eval $S/diff > $S/diff/rust.jsonl
python3 compare.py $S/diff                 # per-endpoint parity
python3 latency.py $S/diff http://127.0.0.1:18321 target/release/scan_engine_eval 90 2
```

Corpus `$S/corpus/files.json`: 120 evenly sampled `bindarr-host/cardscan/debug`
frames, `cardscan/tests/fixtures/*.jpg`, `scan-eval-corpus` (full frame + card
crops). Goldens for `tests/scan_engine_golden.rs` and the detect.rs cv5 parity
tests live in `crates/scrybox-server/tests/fixtures/scan_engine`.

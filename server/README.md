# scrybox-server (Rust)

The Scrybox backend. Production runs this binary as the **only** process in one
container (`docker build -f Dockerfile.rust`, image `scrybox-rust:*` /
`ghcr.io/brenttime/scrybox`). The legacy Node backend was removed (PR #170); the
strangler-fig proxy that once forwarded unported routes to Node is now inert —
the image sets `SCRYBOX_NODE_UPSTREAM=` (empty), which declares a Node-free
deployment.

What runs in-process:
- All API routes (`crates/scrybox-server/src/*`), SQLite (`rusqlite`, `SCRYBOX_DB_PATH`,
  default in the image `/app/database/bindarr.db`), migrations, background jobs.
- The built frontend (`SCRYBOX_STATIC_DIR`) with SPA fallback.
- HTTP on `$PORT`/`SCRYBOX_BIND` (3001) and HTTPS on `HTTPS_PORT` (3443, self-signed unless `SSL_CERT_PATH`/`SSL_KEY_PATH`).
- The local Scryfall engine (`crates/local-scryfall`) when `SCRYBOX_LSF_DATA_DIR` is set,
  including the card portal at `/scryfall-portal/`. `LOCAL_SCRYFALL_URL` / `SCRYBOX_SIDECAR_URL`
  stay unset in production; setting either re-enables an HTTP path to an external engine (fallback only).
- cardscan: corner detection, embeddings and OCR via the ONNX Runtime (`ort`), see
  `cardscan.rs`, `cvscan.rs`, `scan_engine/`, `scan_ocr/`.

Full env defaults: the final stage of `Dockerfile.rust`.

## Build and test

Build and test **inside Docker** — the host build needs clang/libclang, static OpenCV and
mozjpeg, which `Dockerfile.rust` provides:

```sh
docker build -f Dockerfile.rust --target rust-build -t rbuild .
# then run `cargo test --release --locked --workspace` in a container FROM rbuild
```

`cargo run -p scrybox-server` works on a host that has those native deps. Do not run
`cargo fmt --all`: the tree is not rustfmt-clean. Code standards: see `AGENTS.md`.

Benchmarks and parity tools live in `bench/` (e.g. `bench/http-bench.mjs`,
`bench/scenarios.example.json`). Run them only against a DB copy.

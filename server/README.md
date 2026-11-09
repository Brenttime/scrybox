# scrybox-server (Rust, phase P0)

Strangler-fig front server for the backend migration. The plan is in `docs/plans/rust-migration.md`.
**Staged only. Do not deploy, because nothing in compose or the Dockerfile references this yet.**

P0 scope:
- `GET /api/health` returns Node's own health response, so headers stay identical. It returns 503 if Node is down or unhealthy.
- Every other request is reverse-proxied to Node (`SCRYBOX_NODE_UPSTREAM`) with streaming bodies.
  Hop-by-hop headers are stripped, and the client IP is appended to `X-Forwarded-For`. Run Node with
  `TRUST_PROXY=loopback` so rate limiters still see real clients.
- Optional static serving: set `SCRYBOX_STATIC_DIR=frontend/dist`. Misses fall through to Node.
- `RUST_ROUTES` is parsed for the later per-route kill switch. It has no effect in P0.

```sh
cd server
cargo build --release && cargo test
SCRYBOX_BIND=0.0.0.0:3001 SCRYBOX_NODE_UPSTREAM=http://127.0.0.1:3002 ./target/release/scrybox-server
```

Not yet in P0: TLS (:3443), compression, rustls, or a native security-header layer. Those are still Node's
job behind the proxy, and they come before Rust takes over the front port.

Benchmarks: `bench/http-bench.mjs` and `bench/scenarios.example.json`. Run them only against a DB copy.

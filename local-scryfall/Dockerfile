# syntax=docker/dockerfile:1
# Local Scryfall search engine — Scrybox sidecar.
#
# Multi-stage: the builder only exists to produce one static-ish binary; the
# runtime never sees cargo or the 500 MB target/. Data files (default-cards /
# oracle-tags) are NOT baked in — fetch-data.sh pulls them into the mounted
# /data volume on first boot, and the vendored data/ (sets-meta, blocks) is
# copied so the server boots offline.

FROM rust:1-slim-bookworm AS builder
# The engine is now the `local-scryfall` lib+bin crate inside the server
# workspace (server/crates/local-scryfall); build context is the REPO ROOT.
WORKDIR /build
COPY server/ /build/server/
RUN cd server && cargo build --release -p local-scryfall --bin scryfall-api \
    && strip target/release/scryfall-api && cp target/release/scryfall-api /build/scryfall-api

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl python3 \
    && rm -rf /var/lib/apt/lists/*
# Unprivileged: nothing here binds <1024 or writes outside /data.
RUN useradd -r -u 10001 -m scryfall
WORKDIR /app
COPY --from=builder /build/scryfall-api /app/scryfall-api
COPY local-scryfall/static/ /app/static/
COPY local-scryfall/data/ /app/data/
COPY local-scryfall/fetch-data.sh /app/fetch-data.sh
RUN chmod +x /app/fetch-data.sh && chown -R scryfall:scryfall /app
VOLUME /data
ENV SCRYFALL_DATA_DIR=/data \
    STATIC_DIR=/app/static \
    PORT=8420
USER scryfall
EXPOSE 8420
# Entrypoint keeps the network optional: fetch-data.sh warns and continues on
# any failure, then exec's the server so it stays PID 1 for signals.
ENTRYPOINT ["/bin/bash", "-c", "/app/fetch-data.sh; exec /app/scryfall-api"]

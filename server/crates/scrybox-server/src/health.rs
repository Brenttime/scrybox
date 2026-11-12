//! `/api/health` server-side cache (iter 17).
//!
//! Node's own handler is a `SELECT 1` plus `{ "status": "ok" }` with static
//! helmet/X-App-Name headers — the whole per-request cost of the old handler
//! was the proxy round-trip to Node (bench p50 7.6–11 ms, ~1.5k rps: the
//! last server-slow route). A background prober keeps the latest *healthy*
//! Node response in memory; the handler serves a copy of it while fresh
//! (default TTL 1 s) and any probe failure clears the entry, so a wedged DB
//! or dead Node degrades to the original live-proxy behaviour (including the
//! 503 bodies) after at most one probe tick + TTL of stale "ok".
//!
//! Only 2xx responses with a small body (<= 4 KiB) are cached; everything
//! else — non-2xx, timeouts, oversized or unreadable bodies — goes through
//! the live path exactly as before. Ops kill switch: `SCRYBOX_HEALTH_CACHE=0`
//! restores the pure per-request proxy (iter-16 behaviour exactly). Tunables:
//! `SCRYBOX_HEALTH_TICK_MS`, `SCRYBOX_HEALTH_TTL_MS`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::{json_error, strip_hop_by_hop, AppState};

pub const DEFAULT_PROBE_TICK_MS: u64 = 500;
pub const DEFAULT_TTL_MS: u64 = 1_000;
/// Healthy bodies beyond this size are served but never cached (Node's is
/// ~15 bytes; the cap is a guard, not a real limit).
const MAX_CACHED_BODY: usize = 4096;
/// Same timeout the pre-cache handler used for its Node ping.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

struct Cached {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    at: Instant,
}

pub struct HealthCache {
    enabled: AtomicBool,
    tick: Mutex<Duration>,
    ttl: Mutex<Duration>,
    entry: Mutex<Option<Cached>>,
}

impl HealthCache {
    pub fn new_from_env() -> Self {
        let ms = |k: &str, d: u64| {
            std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok()).filter(|v| *v > 0).unwrap_or(d)
        };
        Self::new(
            std::env::var("SCRYBOX_HEALTH_CACHE").map(|v| v != "0").unwrap_or(true),
            Duration::from_millis(ms("SCRYBOX_HEALTH_TICK_MS", DEFAULT_PROBE_TICK_MS)),
            Duration::from_millis(ms("SCRYBOX_HEALTH_TTL_MS", DEFAULT_TTL_MS)),
        )
    }

    pub fn new(enabled: bool, tick: Duration, ttl: Duration) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            tick: Mutex::new(tick),
            ttl: Mutex::new(ttl),
            entry: Mutex::new(None),
        }
    }

    /// Runtime override for the kill switch (tests; ops uses the env var).
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Test/ops hook: retune the prober/TTL at any time (the ticker re-reads
    /// the tick each cycle; the TTL is consulted per request).
    pub fn set_intervals(&self, tick: Duration, ttl: Duration) {
        *self.tick.lock().unwrap() = tick;
        *self.ttl.lock().unwrap() = ttl;
    }

    fn tick_dur(&self) -> Duration {
        *self.tick.lock().unwrap()
    }

    fn ttl_dur(&self) -> Duration {
        *self.ttl.lock().unwrap()
    }

    fn store(&self, status: StatusCode, headers: HeaderMap, body: Bytes) {
        // Per-request header classes must NOT be cached. The health cache is
        // shared across all requests (and refreshed by a prober with a bare
        // client UA), so a captured Cross-Origin-Embedder-Policy is frozen to
        // whichever UA produced it — a Safari client would be replayed
        // `credentialless` where Node answers `require-corp`, and a captured
        // Access-Control-Allow-Origin would leak onto Origin-less replays
        // where Node sends none. secheaders::guard recomputes exactly these
        // per request (existing-wins only sees what survives this filter);
        // every OTHER header of Node's health reply (helmet set, x-app-name,
        // content-type, etag, vary tokens…) is UA/Origin-invariant and
        // replays byte-faithfully.
        const PER_REQUEST: [&str; 3] = [
            "cross-origin-embedder-policy",
            "access-control-allow-origin",
            "access-control-allow-credentials",
        ];
        let headers: HeaderMap = headers
            .into_iter()
            .filter_map(|(name, value)| {
                let name = name.filter(|n| !PER_REQUEST.contains(&n.as_str()))?;
                Some((name, value))
            })
            .collect();
        *self.entry.lock().unwrap() = Some(Cached { status, headers, body, at: Instant::now() });
    }

    fn clear(&self) {
        *self.entry.lock().unwrap() = None;
    }

    /// A clone of the cached response while it is younger than the TTL.
    pub fn fresh(&self) -> Option<Response> {
        let entry = self.entry.lock().unwrap();
        let c = entry.as_ref()?;
        if c.at.elapsed() >= self.ttl_dur() {
            return None;
        }
        let mut res = Response::new(Body::from(c.body.clone()));
        *res.status_mut() = c.status;
        *res.headers_mut() = c.headers.clone();
        Some(res)
    }
}

/// Live ping of Node's `/api/health`. On success with `store`, caches the
/// (tiny) response; any failure clears the cache so the next request — and
/// every request after a wedged DB — rides the live path and sees the truth.
async fn probe(st: &AppState, store: bool) -> Response {
    let uri: Uri = match format!("{}/api/health", st.cfg.node_upstream).parse() {
        Ok(u) => u,
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "bad upstream"),
    };
    let req = Request::get(uri).body(Body::empty()).expect("static request");
    let res = tokio::time::timeout(PROBE_TIMEOUT, st.client.request(req)).await;
    match res {
        Ok(Ok(r)) if r.status().is_success() => {
            let (mut parts, body) = r.into_parts();
            strip_hop_by_hop(&mut parts.headers);
            if !store {
                return Response::from_parts(parts, Body::new(body));
            }
            match to_bytes(Body::new(body), MAX_CACHED_BODY + 1).await {
                Ok(bytes) => {
                    if bytes.len() <= MAX_CACHED_BODY {
                        st.health_cache.store(parts.status, parts.headers.clone(), bytes.clone());
                    }
                    Response::from_parts(parts, Body::from(bytes))
                }
                Err(_) => {
                    st.health_cache.clear();
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        axum::Json(serde_json::json!({ "status": "error", "upstream": "node", "error": "unreachable" })),
                    )
                        .into_response()
                }
            }
        }
        Ok(Ok(r)) => {
            st.health_cache.clear();
            (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({ "status": "error", "upstream": "node", "upstream_status": r.status().as_u16() })),
            )
                .into_response()
        }
        _ => {
            st.health_cache.clear();
            (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({ "status": "error", "upstream": "node", "error": "unreachable" })),
            )
                .into_response()
        }
    }
}

/// Node's own /api/health handler (server.js:411) for the Node-free image:
/// X-App-Name + SELECT 1 against the read pool. helmet/COEP headers ride the
/// shared secheaders::guard layer, and the express JSON ETag is the 'etag'
/// package's content-based weak form (W/"{base64 sha1}"), pinned per body.
/// Node's dbReady gate maps to "no SCRYBOX_DB_PATH / pool": the boot ladder
/// runs before listen, so a missing pool is the only uninitialized state.
fn native_health(st: &AppState) -> Response {
    let (status, body): (StatusCode, &'static str) = match &st.db {
        Some(pool) => match pool.get().ok().and_then(|c| c.query_row("SELECT 1", [], |_| Ok(())).ok()) {
            Some(()) => (StatusCode::OK, "{\"status\":\"ok\"}"),
            None => (StatusCode::SERVICE_UNAVAILABLE, "{\"status\":\"db_unavailable\"}"),
        },
        None => (StatusCode::SERVICE_UNAVAILABLE, "{\"status\":\"db_initializing\"}"),
    };
    // express etag twin: W/"{bodyLen as hex}-{base64(sha1(body))}" — values
    // captured from the live Node :3312 /api/health.
    let etag: &'static str = match body {
        "{\"status\":\"ok\"}" => "W/\"f-VaSQ4oDUiZblZNAEkkN+sX+q3Sg\"",
        "{\"status\":\"db_unavailable\"}" => "W/\"1b-SnIv4TQbcNfatKpq+uRY3kDEnnI\"",
        _ => "W/\"1c-jWxaDve2uVH4xAyM2GutLQRGSJc\"",
    };
    let mut headers = HeaderMap::new();
    headers.insert("x-app-name", "Scrybox".parse().unwrap());
    headers.insert(axum::http::header::ETAG, etag.parse().unwrap());
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "application/json; charset=utf-8".parse().unwrap(),
    );
    (status, headers, body).into_response()
}

pub async fn handle(State(st): State<AppState>) -> Response {
    if st.cfg.node_upstream.is_empty() {
        return native_health(&st);
    }
    if st.health_cache.is_enabled() {
        if let Some(res) = st.health_cache.fresh() {
            return res;
        }
        return probe(&st, true).await;
    }
    probe(&st, false).await
}

/// Background prober: refreshes the cached healthy response every tick so
/// request serving never blocks on Node while the entry is fresh. Spawned
/// from `app()` next to the route, same as the stats/collection refreshers.
pub fn spawn_prober(st: &AppState) {
    // Node-free standalone: /api/health is answered locally; there is no
    // peer to probe and no cache to keep warm.
    if st.cfg.node_upstream.is_empty() {
        return;
    }
    if !st.health_cache.is_enabled() {
        return;
    }
    let st = st.clone();
    tokio::spawn(async move {
        // Sleep BEFORE each probe and read the tick per cycle. The old
        // `interval(tick_dur())` captured the tick at task-start — racing
        // any set_intervals() retune (tests park the tick after app()
        // spawns this) — and its immediate first tick let a background
        // probe land mid-burst (cached_burst_hits_node_once counted 2).
        // Boot warmup loses at most one tick; the first request probes
        // live either way (empty cache), so nothing observable regresses.
        loop {
            tokio::time::sleep(st.health_cache.tick_dur()).await;
            // Failures clear the cache inside probe(); serving then falls
            // back to the live path on its own.
            let _ = probe(&st, true).await;
        }
    });
}

#[cfg(test)]
mod store_tests {
    use super::*;

    #[test]
    fn fresh_none_when_empty_and_after_clear() {
        let c = HealthCache::new(true, Duration::from_secs(60), Duration::from_secs(60));
        assert!(c.fresh().is_none());
        c.store(StatusCode::OK, HeaderMap::new(), Bytes::from_static(b"{\"status\":\"ok\"}"));
        assert!(c.fresh().is_some());
        c.clear();
        assert!(c.fresh().is_none());
    }

    #[test]
    fn fresh_none_after_ttl_expiry() {
        let c = HealthCache::new(true, Duration::from_secs(60), Duration::from_millis(20));
        c.store(StatusCode::OK, HeaderMap::new(), Bytes::from_static(b"x"));
        assert!(c.fresh().is_some());
        std::thread::sleep(Duration::from_millis(40));
        assert!(c.fresh().is_none());
    }

    #[test]
    fn fresh_preserves_status_and_headers() {
        let c = HealthCache::new(true, Duration::from_secs(60), Duration::from_secs(60));
        let mut h = HeaderMap::new();
        h.insert("x-app-name", "Scrybox".parse().unwrap());
        c.store(StatusCode::OK, h, Bytes::from_static(b"x"));
        let res = c.fresh().unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get("x-app-name").unwrap(), "Scrybox");
    }

    /// iter-2 live catch: the cache is shared across all UAs/Origins and the
    /// prober refreshes it with a bare client UA, so the per-request header
    /// classes (COEP, ACAO, ACAC) must be dropped at store time and left to
    /// secheaders::guard to recompute — otherwise a Safari client got the
    /// prober's `credentialless` replayed where Node answers `require-corp`.
    #[test]
    fn store_strips_per_request_headers_keeps_the_rest() {
        let c = HealthCache::new(true, Duration::from_secs(60), Duration::from_secs(60));
        let mut h = HeaderMap::new();
        h.insert("cross-origin-embedder-policy", "credentialless".parse().unwrap());
        h.insert("access-control-allow-origin", "http://localhost:5173".parse().unwrap());
        h.insert("access-control-allow-credentials", "true".parse().unwrap());
        h.insert("cross-origin-opener-policy", "same-origin".parse().unwrap());
        h.insert("vary", "User-Agent, Origin, Accept-Encoding".parse().unwrap());
        h.insert("x-app-name", "Scrybox".parse().unwrap());
        h.insert("etag", "W/\"f-x\"".parse().unwrap());
        c.store(StatusCode::OK, h, Bytes::from_static(b"x"));
        let res = c.fresh().unwrap();
        for dropped in ["cross-origin-embedder-policy", "access-control-allow-origin", "access-control-allow-credentials"] {
            assert!(res.headers().get(dropped).is_none(), "{dropped} must not be cached");
        }
        assert_eq!(res.headers()["cross-origin-opener-policy"], "same-origin");
        assert_eq!(res.headers()["vary"], "User-Agent, Origin, Accept-Encoding");
        assert_eq!(res.headers()["x-app-name"], "Scrybox");
        assert_eq!(res.headers()["etag"], "W/\"f-x\"");
    }

    #[test]
    fn set_intervals_retones_ttl() {
        let c = HealthCache::new(true, Duration::from_secs(60), Duration::from_secs(60));
        c.store(StatusCode::OK, HeaderMap::new(), Bytes::from_static(b"x"));
        c.set_intervals(Duration::from_secs(60), Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(10));
        assert!(c.fresh().is_none());
    }

    /// The env kill switch + tunables. Safe to touch process env here: no
    /// other unit test in this binary constructs a HealthCache from env.
    #[test]
    fn new_from_env_honours_kill_switch_and_tunables() {
        std::env::set_var("SCRYBOX_HEALTH_CACHE", "0");
        std::env::set_var("SCRYBOX_HEALTH_TICK_MS", "250");
        std::env::set_var("SCRYBOX_HEALTH_TTL_MS", "4000");
        let c = HealthCache::new_from_env();
        assert!(!c.is_enabled());
        assert_eq!(c.tick_dur(), Duration::from_millis(250));
        assert_eq!(c.ttl_dur(), Duration::from_millis(4000));
        std::env::set_var("SCRYBOX_HEALTH_CACHE", "1");
        std::env::remove_var("SCRYBOX_HEALTH_TICK_MS");
        std::env::remove_var("SCRYBOX_HEALTH_TTL_MS");
        let c = HealthCache::new_from_env();
        assert!(c.is_enabled());
        assert_eq!(c.tick_dur(), Duration::from_millis(DEFAULT_PROBE_TICK_MS));
        assert_eq!(c.ttl_dur(), Duration::from_millis(DEFAULT_TTL_MS));
        // Garbage values fall back to defaults, not to zero/panic.
        std::env::set_var("SCRYBOX_HEALTH_TICK_MS", "abc");
        std::env::set_var("SCRYBOX_HEALTH_TTL_MS", "0");
        let c = HealthCache::new_from_env();
        assert_eq!(c.tick_dur(), Duration::from_millis(DEFAULT_PROBE_TICK_MS));
        assert_eq!(c.ttl_dur(), Duration::from_millis(DEFAULT_TTL_MS));
        std::env::remove_var("SCRYBOX_HEALTH_CACHE");
        std::env::remove_var("SCRYBOX_HEALTH_TICK_MS");
        std::env::remove_var("SCRYBOX_HEALTH_TTL_MS");
    }
}

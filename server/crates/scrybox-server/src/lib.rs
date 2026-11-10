//! Scrybox front server (strangler fig).
//!
//! `/api/health` is answered natively (backed by a Node health ping). Routes
//! listed in `RUST_ROUTES` are served by their ported Rust handlers; every
//! other request is reverse-proxied to the Node backend unchanged, so
//! behaviour (headers, status codes, bodies) stays exactly what Node
//! produces. Static frontend serving from Rust is opt-in
//! (`SCRYBOX_STATIC_DIR`); misses fall through to Node.

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{any, get},
    Router,
};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use tower_http::services::ServeDir;

pub mod auth;
pub mod collq;
pub mod collection;
pub mod db;
pub mod name_index;
pub mod search;
pub mod sidecar;
pub mod stats;
mod stats_sql;

/// Runtime configuration. Read from env in `main`; built directly in tests.
#[derive(Clone, Debug)]
pub struct Config {
    /// Address the Rust server binds (front door).
    pub bind: SocketAddr,
    /// Base URL of the Node backend, e.g. `http://127.0.0.1:3002` (no trailing slash).
    pub node_upstream: String,
    /// Optional built frontend (`frontend/dist`). None = Node serves static too.
    pub static_dir: Option<PathBuf>,
    /// Routes Rust owns (per-route kill switch, plan section 3). Empty in P0.
    pub rust_routes: BTreeSet<String>,
    /// The SQLite copy shared with Node (`SCRYBOX_DB_PATH`). Ported routes
    /// read through it; without it they stay disabled and everything proxies.
    pub db_path: Option<PathBuf>,
    /// scryfall-local sidecar base URL (`SCRYBOX_SIDECAR_URL`, e.g.
    /// `http://172.29.0.2:8420`). The `search_sidecar` leg pages it directly
    /// (same base the Node container uses via LOCAL_SCRYFALL_URL); unset
    /// disables that leg — those requests keep proxying to Node.
    pub sidecar_url: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let bind = std::env::var("SCRYBOX_BIND")
            .unwrap_or_else(|_| "0.0.0.0:3001".into())
            .parse()
            .map_err(|e| format!("SCRYBOX_BIND: {e}"))?;
        let node_upstream = std::env::var("SCRYBOX_NODE_UPSTREAM")
            .unwrap_or_else(|_| "http://127.0.0.1:3002".into())
            .trim_end_matches('/')
            .to_string();
        if !node_upstream.starts_with("http://") {
            return Err("SCRYBOX_NODE_UPSTREAM must be an http:// URL".into());
        }
        let static_dir = std::env::var_os("SCRYBOX_STATIC_DIR").map(PathBuf::from);
        let rust_routes = std::env::var("RUST_ROUTES")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Ok(Self { bind, node_upstream, static_dir, rust_routes, db_path: std::env::var_os("SCRYBOX_DB_PATH").map(PathBuf::from), sidecar_url: std::env::var("SCRYBOX_SIDECAR_URL").ok().map(|s| s.trim_end_matches('/').to_string()).filter(|s| !s.is_empty()) })
    }
}

type HttpClient = Client<HttpConnector, Body>;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Config,
    pub(crate) client: HttpClient,
    /// Read pool over the shared DB copy; `None` when SCRYBOX_DB_PATH unset.
    pub db: Option<Arc<db::DbPool>>,
    pub collection_cache: Arc<collection::CollectionCache>,
    /// Response cache for the ported search route, keyed by
    /// (params, user, collection_read_revision).
    pub search_cache: Arc<search::SearchCache>,
    /// In-memory name index speeding the search route's LIKE filter
    /// (name_index.rs). Process-wide so invalidation rebuilds once.
    pub name_index: Arc<name_index::NameIndex>,
    /// Per-user owned-qty snapshots (name_index::OwnedCache) replacing the
    /// per-request owned CTE re-materialization in the search route.
    pub owned_index: Arc<name_index::OwnedCache>,
    /// Response cache for the native /stats route (stats.rs mirrors
    /// backend/src/utils/statsCache.js); disabled without a DB, same rule.
    pub stats_cache: Arc<stats::StatsCache>,
    /// In-process half of the shared `collection_query_cache` (match lists
    /// for the collection-scope q leg, collq.rs) + walk single-flight.
    pub collq_cache: Arc<collq::CollQCache>,
}

impl AppState {
    pub fn new(cfg: Config) -> Self {
        let mut conn = HttpConnector::new();
        conn.set_nodelay(true);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .build(conn);
        let db = cfg.db_path.as_ref().and_then(|path| {
            match db::DbPool::open(path, 16) {
                Ok(pool) => {
                    // Hit-path checkouts (auth + revision) run inline on
                    // runtime workers; pre-fill so they never pay connect().
                    pool.warm(16);
                    Some(Arc::new(pool))
                }
                Err(e) => {
                    tracing::error!(error = %e, path = %path.display(), "cannot open DB copy; Rust routes disabled");
                    None
                }
            }
        });
        Self { cfg, client, db, collection_cache: Arc::new(collection::CollectionCache::new()), search_cache: Arc::new(search::SearchCache::new()), name_index: Arc::new(name_index::NameIndex::new()), owned_index: Arc::new(name_index::OwnedCache::new()), stats_cache: Arc::new(stats::StatsCache::new()), collq_cache: Arc::new(collq::CollQCache::new()) }
    }

    /// Kill-switch decision for a route name (`collection`, `stats`, ...).
    pub fn owns(&self, route: &str) -> bool {
        self.db.is_some() && (self.cfg.rust_routes.contains("*") || self.cfg.rust_routes.contains(route))
    }
}

/// Hop-by-hop headers (RFC 9110 section 7.6.1) are connection-scoped and must not be forwarded.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn strip_hop_by_hop(h: &mut HeaderMap) {
    // Headers named in `Connection:` are hop-by-hop too.
    let named: Vec<HeaderName> = h
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|n| HeaderName::from_bytes(n.trim().as_bytes()).ok())
        .collect();
    for n in named {
        h.remove(n);
    }
    for n in HOP_BY_HOP {
        h.remove(*n);
    }
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(serde_json::json!({ "error": msg }))).into_response()
}

/// Reverse-proxy one request to Node, streaming both bodies.
pub async fn proxy(State(st): State<AppState>, mut req: Request) -> Response {
    // statsCache.invalidateOnWrite twin: an authenticated non-GET /api write
    // bumps that user's stats generation so their cached /stats body is
    // dropped (best-effort; never delays or fails the proxied request).
    stats::on_proxied_write(&st, req.method(), req.uri().path(), req.headers());
    let pq = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let uri: Uri = match format!("{}{}", st.cfg.node_upstream, pq).parse() {
        Ok(u) => u,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "Bad request URI"),
    };
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
    *req.uri_mut() = uri;
    *req.version_mut() = hyper::Version::HTTP_11;
    let h = req.headers_mut();
    strip_hop_by_hop(h);
    // Append the real client to X-Forwarded-For so Node (TRUST_PROXY=loopback)
    // keys its rate limiters on the client, not on 127.0.0.1.
    if let Some(ip) = peer {
        let xff = match h.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            Some(prev) => format!("{prev}, {ip}"),
            None => ip.to_string(),
        };
        if let Ok(v) = HeaderValue::from_str(&xff) {
            h.insert("x-forwarded-for", v);
        }
    }
    match st.client.request(req).await {
        Ok(res) => {
            let (mut parts, body) = res.into_parts();
            strip_hop_by_hop(&mut parts.headers);
            Response::from_parts(parts, Body::new(body))
        }
        Err(e) => {
            tracing::warn!(error = %e, "node upstream request failed");
            json_error(StatusCode::BAD_GATEWAY, "Upstream unavailable")
        }
    }
}

/// `/api/health`: healthy only when the Node upstream (and therefore its DB
/// ping) is healthy. Body shape matches Node's `{ "status": "ok" }`.
pub async fn health(State(st): State<AppState>) -> Response {
    let uri: Uri = match format!("{}/api/health", st.cfg.node_upstream).parse() {
        Ok(u) => u,
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "bad upstream"),
    };
    let req = Request::get(uri).body(Body::empty()).expect("static request");
    let res = tokio::time::timeout(Duration::from_secs(3), st.client.request(req)).await;
    match res {
        // Healthy: hand back Node's own response so security headers
        // (helmet/COEP) stay byte-identical to Node's.
        Ok(Ok(r)) if r.status().is_success() => {
            let (mut parts, body) = r.into_parts();
            strip_hop_by_hop(&mut parts.headers);
            Response::from_parts(parts, Body::new(body))
        }
        Ok(Ok(r)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({ "status": "error", "upstream": "node", "upstream_status": r.status().as_u16() })),
        )
            .into_response(),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({ "status": "error", "upstream": "node", "error": "unreachable" })),
        )
            .into_response(),
    }
}

/// Build the router. Ported routes are mounted explicitly; everything else
/// falls back to Node (optionally after a static-file lookup).
pub fn app(state: AppState) -> Router {
    let router = Router::new().route("/api/health", get(health));
    // Ported API routes behind the per-route kill switch. Non-GET methods on
    // an owned path still proxy to Node (the handler dispatches internally).
    let router = if state.owns("collection") {
        // Background pre-warm (iter 14, owner hint): pre-builds post-bump
        // bodies for recently-requested shapes through the shared
        // single-flight slot, so the first request after a write never
        // rides the blocking build. Bodies come from the identical
        // build path — strict parity untouched. Ops kill switch:
        // SCRYBOX_COLLECTION_PREWARM=0 restores iter-12 behavior exactly.
        if std::env::var("SCRYBOX_COLLECTION_PREWARM").map(|v| v != "0").unwrap_or(true) {
            collection::spawn_refresher(&state);
        }
        router.route("/api/collection", any(collection::handle))
    } else {
        router
    };
    let router = if state.owns("search") {
        router.route("/api/search", any(search::handle))
    } else {
        router
    };
    let router = if state.owns("stats") {
        // Background refresher (iter 13): polls the revision counter and
        // pre-builds post-bump bodies for recently-active users. Lives with
        // the router (same runtime); tests that build a Router get it too,
        // which is the point — the bounded-staleness tests exercise it.
        stats::spawn_refresher(&state);
        router.route("/api/stats", any(stats::handle))
    } else {
        router
    };
    let router = match state.cfg.static_dir.clone() {
        Some(dir) => {
            // Static hits are served by Rust; misses (API, SPA deep links,
            // /scan-assets, portal) go to Node unchanged.
            let fallback = axum::routing::any(proxy).with_state(state.clone());
            router.fallback_service(ServeDir::new(dir).fallback(fallback))
        }
        None => router.fallback(proxy),
    };
    router
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

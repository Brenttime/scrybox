//! Scrybox front server (strangler fig).
//!
//! `/api/health` is answered natively (backed by a cached Node health ping,
//! health.rs). Routes
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
    routing::{any, get, post},
    Router,
};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use tower_http::services::ServeDir;

pub mod auth;
pub mod auth_routes;
pub mod auth_support;
pub mod admin_users;
pub mod backup;
pub mod cards_meta;
pub mod card_cache;
pub mod card_sets;
pub mod catalog_build;
pub mod card_norm;
pub mod cardscan;
pub mod card_art;
pub mod cards_tags;
pub mod catalog_read;
pub mod collq;
pub mod cvscan;
pub mod ip_limit;
pub mod oidc;
pub mod collection;
pub mod collection_writes;
pub mod db;
pub mod decks_writes;
pub mod decks_reads;
pub mod export;
pub mod npz;
pub mod settings;
pub mod lists_body_writes;
pub mod lists_reads;
pub mod lists_writes;
pub mod health;
pub mod manabox;
pub mod marketplace;
pub mod migrations;
pub mod mfx_payload;
pub mod moxfield;
pub mod moxfield_api;
pub mod model_assets;
pub mod name_index;
pub mod precons;
pub mod rules;
pub mod scryfall;
pub mod secheaders;
pub mod scry_jobs;
pub mod search;
pub mod secret_lair;
pub mod sets;
pub mod sidecar;
pub mod stats;
mod stats_sql;
pub mod stats_subs;
pub mod search_history;
pub mod shared;
pub mod spa;
pub mod static_assets;
pub mod tls;

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
        let static_dir = std::env::var_os("SCRYBOX_STATIC_DIR")
            .map(PathBuf::from)
            // An empty-but-set value must not mount ServeDir: its fallback
            // never answers non-GET requests, so every POST to a
            // Rust-unregistered API path would die 405 (allow: GET,HEAD)
            // instead of reaching the proxy.
            .filter(|p| !p.as_os_str().is_empty());
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
    /// Pre-warm cost gate (collection.rs, iter 15): shapes whose last build
    /// was cheaper than this never fan out. `SCRYBOX_PREWARM_MIN_COST_MS`
    /// overrides; tests set the field directly.
    pub prewarm_min_cost_ms: u64,
    /// `/api/health` response cache + prober (iter 17). Read from env.
    pub health_cache: Arc<health::HealthCache>,
    /// TRUST_PROXY twin for the auth limiter's client-IP decision (iter 20).
    /// Tests set this directly; `new` parses the same env Node's server.js
    /// reads, with the same ladder.
    pub trust_proxy: ip_limit::Trust,
    /// Shared Scryfall client (Lane B): rate-limited queue + 404 breaker.
    /// One per process so the queue is global, like Node's module state.
    pub scry: Arc<scryfall::Scry>,
    /// Moxfield API client (Lane B unit 3): execs curl/curl-impersonate,
    /// GET only. `from_env` at boot (MOXFIELD_API_BASE / MOXFIELD_CURL_BIN /
    /// curl-impersonate probe), like Node's require-time constants; tests
    /// point it at a mock by setting the field.
    pub mfx: Arc<crate::moxfield_api::MfxApi>,
    /// cardSets.js getScryfallSets memo (tree=1 folds, 6 h TTL).
    pub scryfall_sets: sets::ScryfallSetsMemo,
    /// ManaPool order client (marketplace.rs httpGet twin). Trait-typed so
    /// route tests inject a stub, exactly like the Node unit tests inject
    /// `httpGet` into utils/marketplaceOrders.
    pub mp_http: Arc<dyn marketplace::MpHttp>,
    /// authLimiter twin (middleware/auth.js): one fixed window per client IP
    /// shared by login/register/bootstrap/api-key/settings.
    pub auth_limiter: Arc<ip_limit::FixedWindowLimiter>,
    /// Repo `shared/` directory (SCRYBOX_SHARED_DIR): the layers
    /// `cardArt.BUNDLED_DIR` and `utils/languages.js` read. None makes the
    /// catalog_art file-dependent handlers proxy instead of guessing.
    pub shared_dir: Option<std::path::PathBuf>,
    /// CV_MODEL_DIR — catalog meta/model dir catalog.js/cvScan.js stat.
    /// None => catalog-read handlers proxy (no silent empty answers).
    pub model_dir: Option<std::path::PathBuf>,
    /// utils/deckListCache.js `store` twin (decks_reads): userId ->
    /// (deck_list_revision, built body bytes). Per process like Node's
    /// module-level Map; per AppState in tests keeps fixtures isolated.
    pub deck_list_cache: Arc<std::sync::Mutex<std::collections::HashMap<i64, (i64, Vec<u8>)>>>,
    /// preconData.js `indexFile` — the shared MTGJSON mirror
    /// (dirname(DB)/precons.json), the SAME file the Node container reads
    /// through its /app/database bind mount. Tests point it at a temp dir.
    pub precons_index_path: std::path::PathBuf,
    /// preconData.MTGJSON_BASE; SCRYBOX_MTGJSON_BASE overrides for tests
    /// only (snapshot at boot, like SettingsEnv — an env read per request
    /// would race across parallel integration tests).
    pub mtgjson_base: String,
    /// rules.js CACHE — the shared rules doc (dirname(DB)/mtg-rules.json,
    /// same bind-mounted file the Node container reads). RULES_CACHE env
    /// override like the Node twin.
    pub rules_cache_path: std::path::PathBuf,
    /// rules.js hard-coded upstream URLs (test hooks; production keeps the
    /// JS constants — same snapshot pattern as `mtgjson_base`).
    pub rules_index_url: String,
    pub rules_commander_url: String,
    pub rules_cr_base: String,
    /// rules.js module-level `mem` + `kwMem` (per-process), + the `inflight`
    /// build-dedup twin. Per AppState so fixtures stay isolated.
    pub rules_mem: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    pub kw_mem: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    pub rules_build: Arc<tokio::sync::Mutex<()>>,
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
        let precons_index_path = crate::precons::default_index_path(cfg.db_path.as_ref());
        let mtgjson_base = std::env::var("SCRYBOX_MTGJSON_BASE")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| crate::precons::MTGJSON.to_string());
        let rules_cache_path = std::env::var("RULES_CACHE")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::rules::default_cache_path(cfg.db_path.as_ref()));
        Self { cfg, client, db, collection_cache: Arc::new(collection::CollectionCache::new()), search_cache: Arc::new(search::SearchCache::new()), name_index: Arc::new(name_index::NameIndex::new()), owned_index: Arc::new(name_index::OwnedCache::new()), stats_cache: Arc::new(stats::StatsCache::new()), collq_cache: Arc::new(collq::CollQCache::new()), prewarm_min_cost_ms: std::env::var("SCRYBOX_PREWARM_MIN_COST_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(collection::PREWARM_MIN_COST_MS_DEFAULT), health_cache: Arc::new(health::HealthCache::new_from_env()), trust_proxy: std::env::var("TRUST_PROXY").map(|v| ip_limit::Trust::from_env_value(&v)).unwrap_or(ip_limit::Trust::None), auth_limiter: Arc::new(ip_limit::FixedWindowLimiter::new(20, 15 * 60 * 1000)), scry: Arc::new(scryfall::Scry::new(scryfall::ScryEnv::from_env())), mfx: std::sync::Arc::new(crate::moxfield_api::MfxApi::from_env()), scryfall_sets: std::sync::Arc::new(std::sync::Mutex::new(None)), mp_http: std::sync::Arc::new(crate::marketplace::RealHttp), shared_dir: std::env::var_os("SCRYBOX_SHARED_DIR").map(std::path::PathBuf::from), model_dir: std::env::var_os("CV_MODEL_DIR").map(std::path::PathBuf::from), deck_list_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())), precons_index_path, mtgjson_base, rules_cache_path, rules_index_url: crate::rules::DEFAULT_INDEX_URL.to_string(), rules_commander_url: crate::rules::DEFAULT_COMMANDER_URL.to_string(), rules_cr_base: crate::rules::DEFAULT_CR_BASE.to_string(), rules_mem: Arc::new(std::sync::Mutex::new(None)), kw_mem: Arc::new(std::sync::Mutex::new(None)), rules_build: Arc::new(tokio::sync::Mutex::new(())) }
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

/// Build the router. Ported routes are mounted explicitly; everything else
/// falls back to Node (optionally after a static-file lookup).
pub fn app(state: AppState) -> Router {
    // Health cache prober (iter 17): keeps the last healthy Node response
    // warm so the handler answers from memory while fresh. SCRYBOX_HEALTH_CACHE=0
    // disables both the prober and the cache lookup (iter-16 live proxy).
    health::spawn_prober(&state);
    let router = Router::new().route("/api/health", get(health::handle));
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
    // Collection WRITE group (cardlist / bulk / PUT+DELETE :id). The
    // handlers dispatch internally: methods the port does not own, and the
    // PUT card_id branch whose target printing is not in card_cache (Node
    // would hydrate it from Scryfall), are proxied to Node untouched.
    let router = if state.owns("collection_writes") {
        router
            .route("/api/collection/cardlist", any(collection_writes::handle_cardlist))
            .route("/api/collection/bulk", any(collection_writes::handle_bulk))
            .route("/api/collection/{id}", any(collection_writes::handle_entry))
    } else {
        router
    };
    // Collection ADD group (iter 24): POST /api/collection and POST
    // /api/collection/bulk-add. Registered as a `post`-only MethodRouter on
    // /api/collection so it overrides the `collection` group's `any` fallback
    // for POST only; GET keeps answering. Any add whose card (or localized
    // printing) is not in card_cache — Node would fetch it from Scryfall —
    // forwards to Node untouched, so the Rust port never needs a Scryfall
    // client (that's lane B's scryfallApi territory).
    let router = if state.owns("collection_adds") {
        // With the `collection` group mounted, its `any()` fallback answers
        // every non-POST method and the merged POST entry wins for POST.
        // Alone, this group must NOT swallow GET with a 405 — a kill-switch
        // combination has to look like the port doesn't exist, so the
        // MethodRouter falls through to the Node proxy for other methods.
        let adds = if state.owns("collection") {
            post(collection_writes::handle_add)
        } else {
            post(collection_writes::handle_add).fallback(crate::proxy)
        };
        router
            .route("/api/collection", adds)
            .route("/api/collection/bulk-add", any(collection_writes::handle_bulk_add))
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
    let router = if state.owns("stats_subs") {
        // GET /api/stats/history, /api/stats/networth, /api/stats/top-mover
        // (iter 25). commander-of-the-day stays on Node (Scryfall leg is
        // Lane B surface — forwarding it whole-request would not make it
        // native). Exact paths only: '/api/stats/' trailing-slash variants
        // keep Express's loose-match answer through the proxy, same policy
        // as the /stats mount, and mounting these never changes what
        // /api/stats itself resolves to — the groups stay independent.
        router
            .route("/api/stats/history", any(stats_subs::handle_history))
            .route("/api/stats/networth", any(stats_subs::handle_networth))
            .route("/api/stats/top-mover", any(stats_subs::handle_top_mover))
    } else {
        router
    };
    // Auth route group (iter 20, FULL PORT P1): everything under /api/auth
    // not in the known-route table is proxied by the handler itself, so
    // Express keeps authoring the 404 pages.
    let router = if state.owns("auth") {
        router
            .route("/api/auth", any(auth_routes::handle))
            .route("/api/auth/{*rest}", any(auth_routes::handle))
    } else {
        router
    };
    // Sets route group (Lane B unit 1): native GET /api/sets (flat + tree).
    let router = if state.owns("sets") {
        router.route("/api/sets", any(sets::handle))
    } else {
        router
    };
    // Cards tags group (Lane B unit 2a): Tagger oracle views. Methods/paths
    // the port does not own proxy to Node untouched, so Express still
    // authors everything else under /api/cards (e.g. /cards/highlight).
    let router = if state.owns("cards_tags") {
        router
            .route("/api/cards/{id}/oracle", any(cards_tags::handle_oracle))
            .route("/api/cards/{id}/oracle-tags", any(cards_tags::handle_oracle_tags))
            .route("/api/cards/role-tags", any(cards_tags::handle_role_tags))
            .route("/api/cards/tag-stats", any(cards_tags::handle_tag_stats))
    } else {
        router
    };
    // Search-history + shared group (iter 27): the "stats/sets/search-history/
    // shared" P2 cluster's remainder. search-history owns exactly
    // /api/search-history (trailing-slash variants keep Express's
    // loose-match answer through the proxy, per the iter-25 ruling); shared
    // owns the PUBLIC GET /api/shared/:token (Node mounts it above the auth
    // gate — Rust answers it without any session too). Methods/paths the
    // port does not own proxy to Node untouched.
    let router = if state.owns("search_history") {
        router.route("/api/search-history", any(search_history::handle))
    } else {
        router
    };
    let router = if state.owns("shared") {
        router.route("/api/shared/{token}", any(shared::handle))
    } else {
        router
    };
    // Lane B unit 2b: catalog_art = the stateless card-art reads and the
    // two DB/FS-only catalog views. Mutating card-art routes and the
    // build-control routes stay unregistered so the catch-all proxies them.
    let router = if state.owns("catalog_art") {
        router
            .route("/api/card-art/index", any(card_art::handle_index))
            // matchit has no `{param}.png` suffix segments — Node's route is
            // `/:cardId.png`; the handler enforces the literal suffix and
            // proxies anything else (Node 404s those too).
            .route("/api/card-art/{cardId}", any(card_art::handle_png))
            .route("/api/admin/catalogs/languages", any(catalog_read::handle_catalog_languages))
            .route("/api/scan-sets", any(catalog_read::handle_scan_sets))
    } else {
        router
    };
    // Lane B unit 6c: catalog_build = the admin build-control group +
    // modelAssets (RUST_ROUTES=catalog_build). The catch-all proxies these
    // paths while the group is off, so Node keeps its own engine there.
    let router = if state.owns("catalog_build") {
        router
            .route("/api/admin/catalogs", any(catalog_build::handle_catalogs))
            .route("/api/admin/catalogs/progress", any(catalog_build::handle_catalogs))
            .route("/api/admin/catalogs/build", any(catalog_build::handle_catalogs))
            .route("/api/admin/catalogs/stop", any(catalog_build::handle_catalogs))
            .route("/api/admin/models", any(model_assets::handle_models))
            .route("/api/admin/models/download", any(model_assets::handle_models))
    } else {
        router
    };
    // Lane A iter-28/29/30: deck writes (PUT/DELETE + POST/checkout/return,
    // `decks_writes`) and deck reads (GET list / detail / locations,
    // `decks_reads`). Both groups share the /api/decks and /api/decks/{id}
    // shapes, so one dispatcher route owns each: GET goes to decks_reads
    // when that group is on, POST/PUT/DELETE to decks_writes, and everything
    // else (or groups that are off) falls through to the catch-all proxy —
    // never 405 here.
    let router = if state.owns("decks_reads") {
        router.route("/api/decks/{id}/locations", any(decks_reads::handle_locations))
    } else {
        router
    };
    let router = if state.owns("decks_reads") && state.owns("decks_writes") {
        router.route("/api/decks", any(decks_writes::route_collection))
    } else if state.owns("decks_reads") {
        router.route("/api/decks", any(decks_reads::handle_list))
    } else if state.owns("decks_writes") {
        router.route("/api/decks", any(decks_writes::handle_create))
    } else {
        router
    };
    let router = if state.owns("decks_reads") || state.owns("decks_writes") {
        router.route("/api/decks/{id}", any(decks_reads::route_entry))
    } else {
        router
    };
    let router = if state.owns("decks_writes") {
        router
            .route("/api/decks/{id}/cards/{card_id}", any(decks_writes::handle_card))
            .route("/api/decks/{id}/cards", any(decks_writes::handle_add_card))
            .route("/api/decks/{id}/register-collection", any(decks_writes::handle_register))
            .route("/api/decks/{id}/checkout", any(decks_writes::handle_checkout))
            .route("/api/decks/{id}/return", any(decks_writes::handle_return))
    } else {
        router
    };
    // Lane A iter-34: settings group (GET/PUT /api/settings,
    // GET /api/settings/version) under `settings`. Other methods ride the
    // catch-all proxy — never 405.
    let router = if state.owns("settings") {
        router
            .route("/api/settings", any(settings::route_settings))
            .route("/api/settings/version", any(settings::route_version))
    } else {
        router
    };
    // Lane A iter-35: precons group (GET /api/precons, POST
    // /api/precons/import) under `precons`. Handlers self-check
    // group+method and forward anything else — never 405/404 locally.
    let router = if state.owns("precons") {
        router
            .route("/api/precons", any(precons::route_precons))
            .route("/api/precons/import", any(precons::route_import))
    } else {
        router
    };
    // Lane A iter-36: rules group (GET /api/rules, GET /api/rules/keywords)
    // under `rules`. Handlers self-check group+method and forward anything
    // else — never 405/404 locally.
    let router = if state.owns("rules") {
        router
            .route("/api/rules", any(rules::route_rules))
            .route("/api/rules/keywords", any(rules::route_keywords))
    } else {
        router
    };
    // Lane A iter-37: secret_lair group (GET /api/secret-lair,
    // GET /api/secret-lair/preview, POST /api/secret-lair/add) under
    // `secret_lair`. Handlers self-check group+method and forward anything
    // else — never 405/404 locally.
    let router = if state.owns("secret_lair") {
        router
            .route("/api/secret-lair", any(secret_lair::route_search))
            .route("/api/secret-lair/preview", any(secret_lair::route_preview))
            .route("/api/secret-lair/add", any(secret_lair::route_add))
    } else {
        router
    };
    // Lane A iter-38: export group (routes/importExport.js) under `export`:
    // GET /api/export, POST /api/export (view subset), POST /api/import.
    // Handlers self-check owns+method and proxy everything else.
    let router = if state.owns("export") {
        router
            .route("/api/export", any(export::route_export).post(export::route_export_post))
            .route("/api/import", any(export::route_import))
    } else {
        router
    };
    // Lane A iter-39: cards-meta group (stats.js card routes + the
    // collection.js suggest) under `cards_meta`: POST /api/cards/highlight,
    // GET /api/cards/{id}/decks, GET /api/cards/{id}/price-history,
    // GET /api/stats/commander-of-the-day, GET /api/suggest.
    // Handlers self-check owns+method and proxy everything else.
    let router = if state.owns("cards_meta") {
        router
            .route("/api/cards/highlight", any(cards_meta::handle_highlight))
            .route("/api/cards/{id}/decks", any(cards_meta::handle_card_decks))
            .route("/api/cards/{id}/price-history", any(cards_meta::handle_price_history))
            .route("/api/stats/commander-of-the-day", any(cards_meta::handle_cotd))
            .route("/api/suggest", any(cards_meta::handle_suggest))
    } else {
        router
    };
    // Lane A iter-40: admin USERS group (GET/POST /api/admin/users,
    // PUT/DELETE /api/admin/users/:id) under `admin_users`. The rest of
    // admin.js (seed-cards, catalogs, models, backups) is Lane B's unit-4
    // surface; everything unmatched here rides the catch-all proxy —
    // never a local 404/405.
    let router = if state.owns("admin_users") {
        router
            .route("/api/admin/users", any(admin_users::route_users))
            .route("/api/admin/users/{id}", any(admin_users::route_user))
    } else {
        router
    };
    // Lane A iter-41: MARKETPLACE slice 1 (GET/PUT /api/marketplace/accounts,
    // GET /api/marketplace/recent/:source) under `marketplace`. POST
    // /preview and POST /add (orderFromRequest + parseOrderPayload + the
    // bulkAddToCollection core) are slice 2 and stay proxied; unmatched
    // methods ride the catch-all proxy, never a local 404/405.
    let router = if state.owns("marketplace") {
        router
            .route("/api/marketplace/accounts", any(marketplace::route_accounts))
            .route("/api/marketplace/recent/{source}", any(marketplace::route_recent))
            .route("/api/marketplace/preview", any(marketplace::route_preview))
            .route("/api/marketplace/add", any(marketplace::route_add))
    } else {
        router
    };
    // Lane A iter-31: card-list READS (GET /, /card-matches, /:id,
    // /:id/cardlist) under `lists_reads`. The write routes (POST/PUT/DELETE,
    // /order, cheapest-printings, printing, bulk*) stay proxied: the
    // /api/lists/{id} dispatcher answers GET natively and forwards every
    // other method, so shapes are never 405'd here.
    let router = if state.owns("lists_reads") {
        let router = router
            .route("/api/lists/card-matches", any(lists_reads::handle_card_matches))
            .route("/api/lists/{id}", any(lists_reads::route_entry))
            .route("/api/lists/{id}/cardlist", any(lists_reads::handle_cardlist));
        if state.owns("lists_writes") {
            // iter-32: /api/lists POST + /order are native under lists_writes;
            // the collection dispatcher keeps GET with lists_reads.
            router
                .route("/api/lists", any(lists_writes::route_collection))
                .route("/api/lists/order", any(lists_writes::handle_order))
        } else {
            router.route("/api/lists", any(lists_reads::handle_collection))
        }
    } else if state.owns("lists_writes") {
        router
            .route("/api/lists", any(lists_writes::route_collection))
            .route("/api/lists/order", any(lists_writes::handle_order))
            // The /:id dispatcher answers PUT/DELETE natively here; its GET
            // leg self-checks lists_reads and proxies when that group is off.
            .route("/api/lists/{id}", any(lists_reads::route_entry))
    } else {
        router
    };
    // Lane A iter-33: card-list CARD-BODY writes (add card, bulk add/remove,
    // remove-from-collection, cheapest-printings, change printing, remove
    // card) ride the same `lists_writes` kill switch. Handlers self-check the
    // group and the method, forwarding anything else to Node (never 405).
    let router = if state.owns("lists_writes") {
        router
            .route("/api/lists/{id}/cards", any(lists_body_writes::handle_add_card))
            .route("/api/lists/{id}/cards/bulk", any(lists_body_writes::handle_bulk_add))
            .route("/api/lists/{id}/cards/bulk-remove", any(lists_body_writes::handle_bulk_remove))
            .route("/api/lists/{id}/remove-from-collection", any(lists_body_writes::handle_remove_from_collection))
            .route("/api/lists/{id}/cheapest-printings", any(lists_body_writes::handle_cheapest_printings))
            .route("/api/lists/{id}/cards/{card_id}/printing", any(lists_body_writes::handle_printing))
            .route("/api/lists/{id}/cards/{card_id}", any(lists_body_writes::handle_remove_card))
    } else {
        router
    };
    // Lane B unit 3a: Moxfield sync — status, author CRUD, forced syncs,
    // per-deck enable switch, all under RUST_ROUTES=moxfield. The 20-second
    // scheduler + boot catch-up ride the same gate (Node's twin is disabled
    // with SCRYBOX_DISABLE_MOXFIELD_SCHEDULER, server.js).
    let router = if state.owns("moxfield") {
        // Scheduler twin of Node's server.js kill switch: the harness runs
        // the test instance with SCRYBOX_DISABLE_MOXFIELD_SCHEDULER=1 (and
        // the Node container's flag file marks its tick off) so the shared
        // DB copy stays quiescent for parity — the tick/boot-catch-up
        // semantics are proven by cargo tests, like the sets/price jobs are
        // by the jobs binary + DB-diff. Flip this env off (and remove the
        // container flag file) to hand the live tick to Rust.
        let sched_off = std::env::var("SCRYBOX_DISABLE_MOXFIELD_SCHEDULER")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        if sched_off {
            tracing::info!("Moxfield scheduler disabled (SCRYBOX_DISABLE_MOXFIELD_SCHEDULER) — no tick runs on this instance");
        } else {
            moxfield::spawn_scheduler(&state);
        }
        router
            .route("/api/moxfield", any(moxfield::handle_status))
            .route("/api/moxfield/authors", any(moxfield::handle_add_author))
            .route("/api/moxfield/authors/{id}", any(moxfield::handle_delete_author))
            .route("/api/moxfield/authors/{id}/sync-decklist", any(moxfield::handle_sync_decklist))
            .route("/api/moxfield/authors/{id}/sync-contents", any(moxfield::handle_sync_contents))
            .route("/api/moxfield/decks/{publicId}/sync", any(moxfield::handle_deck_sync))
            .route("/api/moxfield/decks/{publicId}", any(moxfield::handle_deck_update))
    } else {
        router
    };
    // Lane B unit 3b: ManaBox sync + backup job, both under
    // RUST_ROUTES=manabox_backup. Node's twin auto-backup timer is switched
    // off with SCRYBOX_DISABLE_AUTO_BACKUP (server.js); the parity harness
    // sets it on both instances so the shared DB copy stays quiescent.
    let router = if state.owns("manabox_backup") {
        let backup_off = std::env::var("SCRYBOX_DISABLE_AUTO_BACKUP")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        if backup_off {
            tracing::info!("Auto-backup disabled (SCRYBOX_DISABLE_AUTO_BACKUP) — no timer runs on this instance");
        } else {
            backup::spawn_auto_backup(&state);
        }
        router
            .route("/api/manabox-sync/preview", any(manabox::handle_preview))
            .route("/api/manabox-sync/apply", any(manabox::handle_apply))
            .route("/api/manabox-sync/runs", any(manabox::handle_runs))
            .route("/api/manabox-sync/runs/{id}/report", any(manabox::handle_report))
            .route("/api/admin/backups", any(backup::handle_backups_collection))
            .route("/api/admin/backups/{file}/download", any(backup::handle_backup_download))
    } else {
        router
    };
    // Lane B unit 5b: the static trio (see static_assets.rs). Kill-switch
    // group `static_assets`: off => everything proxies to Node's own
    // handlers. The portal leg additionally mirrors Node's registration
    // gate: the routes only exist when LOCAL_WEB_PORTAL_ENABLED === 'true'.
    let router = if state.owns("static_assets") {
        router
            .route("/scan-assets", any(static_assets::handle_scan_assets))
            // Explicit trailing-slash exacts: matchit's catch-all does NOT
            // match an empty tail, and under static_spa the SPA wildcard
            // would otherwise swallow these (live :3312-vs-:3320 diff).
            .route("/scan-assets/", any(static_assets::handle_scan_assets))
            .route("/scan-assets/{*rest}", any(static_assets::handle_scan_assets))
            .route("/models/cornelius.onnx", any(static_assets::handle_model))
    } else {
        router
    };
    let router = if state.owns("static_assets")
        && std::env::var("LOCAL_WEB_PORTAL_ENABLED").as_deref() == Ok("true")
    {
        router
            .route("/scryfall-portal", any(static_assets::handle_portal))
            .route("/scryfall-portal/", any(static_assets::handle_portal))
            .route("/scryfall-portal/{*rest}", any(static_assets::handle_portal))
    } else {
        router
    };
    // Lane B unit 5c: the SPA mount itself (see spa.rs). Registered as a
    // WILDCARD ROUTE, not the fallback: Node's catch-all sits behind every
    // API router, so unmatched /api/* must still reach the router-level
    // proxy below — the gate is Node's exact `req.path.startsWith('/api')`,
    // so even '/apifoo' proxies (on Node it next()s to finalhandler). Kill-
    // switch group `static_spa`; without a build dir the group is inert
    // (Node serves the SPA, exactly like a dist-less Node install).
    let router = if state.owns("static_spa") && state.cfg.static_dir.is_some() {
        router
            .route("/", any(spa::handle))
            .route("/{*rest}", any(spa::handle))
    } else {
        router
    };
    // Lane B unit 6a: cardscan sidecar-proxy group (see cardscan.rs). All
    // eleven router.js routes; unowned methods/paths fall through to the
    // router-level proxy, so a kill-switched group is invisible. Merged as a
    // sub-router so the compression() twin wraps ONLY this group's replies
    // (axum .layer covers every route registered on that Router at the
    // time — a direct .layer here would also buffer other lanes' bodies).
    let router = if state.owns("cardscan") {
        let cs = axum::Router::new()
            .route("/api/cardscan/status", any(cardscan::handle_status))
            .route("/api/cardscan/detect", any(cardscan::handle_detect))
            .route("/api/cardscan/frame", any(cardscan::handle_frame))
            .route("/api/cardscan/choices", any(cardscan::handle_choices))
            .route("/api/cardscan/cards", any(cardscan::handle_cards))
            .route("/api/cardscan/telemetry", any(cardscan::handle_telemetry))
            .route("/api/cardscan", any(cardscan::handle_scan))
            .route("/api/cardscan/scan", any(cardscan::handle_scan))
            .layer(axum::middleware::from_fn(cardscan::compress));
        router.merge(cs)
    } else {
        router
    };
    // Lane B unit 6b: cvScan.js twin + POST /api/scan-match, behind
    // RUST_ROUTES=scan_match. Same compression() twin treatment as the
    // cardscan group (Node's global compression() gzips this JSON too).
    let router = if state.owns("scan_match") {
        let sm = axum::Router::new()
            .route("/api/scan-match", any(cvscan::handle_scan_match))
            .layer(axum::middleware::from_fn(cardscan::compress));
        router.merge(sm)
    } else {
        router
    };
    // Lane B unit 5a: security-header stack (COEP/helmet-8/cors twins) over
    // the NATIVE routes only. Attached before the fallback is registered so
    // proxied traffic keeps Node's own header output untouched — axum
    // applies .layer() only to the routes added above it.
    let router = router.layer(axum::middleware::from_fn(crate::secheaders::guard));
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

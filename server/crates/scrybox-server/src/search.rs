//! Port of the local-cache fast path of `GET /api/search`
//! (`backend/src/routes/collection.js` `searchCards` → `scryfallApi.js`
//! `runSearch` field-filter branch → `cardSearchSql.js` `localCacheQuery` →
//! `priceHelpers.parseCardRow` + `collection.js` `attachOwnedQty`).
//!
//! Scope of the port (mirrors Node's own short-circuit exactly): only
//! requests Node itself would answer from `card_cache` are served natively —
//! empty raw query, scope not `collection`/`internet`, no `prints=1`, no scan
//! `image` — and only when the local query returns rows. Zero rows (or any
//! query error) means Node would fall through to the Scryfall/sidecar path,
//! so the request is proxied instead and Node answers it verbatim. That
//! keeps the parity contract intact: Rust never invents an answer Node would
//! not give from the cache.
//!
//! Response shape (verified against live Node): bare JSON array of
//! `parseCardRow`-shaped rows with `owned_qty` appended, plus the headers
//! `X-Source: cache` and `Access-Control-Expose-Headers: X-Total-Count,
//! X-Source`, and Express's default body ETag (replays as 304).
//!
//! Cache: serialized raw+gzip keyed by (user, normalized params) x
//! `collection_read_revision`, whose triggers cover every card_cache and
//! collection write Node can make.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use rusqlite::{Connection, Row};
use serde_json::{Map, Value};

use crate::{
    auth,
    collection::{body_etag, etag_matches, gzip_bytes, parse_array_col, sqlite_value, truthy, wants_encoding},
    db::DbPool,
    name_index::{self, NameIndex, OwnedCache, Txn},
    sidecar, AppState,
};

// ---------------------------------------------------------------------------
// Language resolution — port of utils/languages.js resolve()/toName().
// Same table, same fallback order (code, name, provider), English default.
// ---------------------------------------------------------------------------

fn langs() -> &'static Vec<(String, String, String)> {
    static L: OnceLock<Vec<(String, String, String)>> = OnceLock::new();
    L.get_or_init(|| {
        let v: Vec<Value> = serde_json::from_str(include_str!("../../../../shared/languages.json"))
            .expect("shared/languages.json");
        v.iter()
            .map(|l| {
                (
                    l["code"].as_str().unwrap_or("").to_string(),
                    l["name"].as_str().unwrap_or("").to_string(),
                    l["scryfall"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    })
}

/// `languages.toName(input)` — display name ('English'). `None`/empty is the
/// JS falsy path → DEFAULT (LANGUAGES[0], English).
pub(crate) fn to_name(input: Option<&str>) -> String {
    let default = || langs()[0].1.clone();
    let Some(input) = input.filter(|s| !s.is_empty()) else { return default() };
    let lower = input.trim().to_lowercase();
    for (code, name, scry) in langs() {
        if lower == *code
            || lower == name.to_lowercase()
            || (!scry.is_empty() && lower == *scry)
        {
            return name.clone();
        }
    }
    default()
}

// ---------------------------------------------------------------------------
// Query parsing (express req.query semantics for this route)
// ---------------------------------------------------------------------------

/// The normalized parameters the native path consumes. `PartialEq`/`Hash` via
/// the manual `cache_key`.
#[derive(Clone, Debug)]
pub struct SearchQuery {
    user_id: i64,
    name: String,
    number: String,
    set: String,
    lang_name: String,
    page: i64,
    limit: i64,
    /// Zero-row local results may be answered by the sidecar leg instead of
    /// proxying: the name-only English field-filter branch (no number/set —
    /// the specific-query and set-browse legs stay on Node).
    walkable: bool,
}

impl SearchQuery {
    fn cache_key(&self) -> String {
        format!(
            "s|{}|{}|{}|{}|{}|{}|{}",
            self.user_id, self.name, self.number, self.set, self.lang_name, self.page, self.limit
        )
    }
}

/// JS `parseInt(x, 10)` (same port as collection.rs).
pub(crate) fn js_parse_int(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r') {
        i += 1;
    }
    let neg = match bytes.get(i) {
        Some(b'+') => {
            i += 1;
            false
        }
        Some(b'-') => {
            i += 1;
            true
        }
        _ => false,
    };
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    s[start..i].parse::<i64>().ok().map(|v| if neg { -v } else { v }).or(if neg { Some(i64::MIN) } else { Some(i64::MAX) })
}

/// JS `(numberQuery || '').trim().replace(/^#/, '').split('/')[0].trim()`.
fn clean_number(raw: &str) -> String {
    let t = raw.trim().trim_start_matches('#').to_string();
    t.split('/').next().unwrap_or("").trim().to_string()
}

/// Outcome of the native-gate decision.
pub enum Plan {
    Native(SearchQuery),
    Proxy,
}

/// Decide whether Rust may answer this GET, mirroring the branches of
/// `searchCards`/`runSearch` that Node answers from `card_cache`. Anything
/// that could reach Scryfall/the sidecar (raw `q`, `prints`, `image`, other
/// scopes) or that uses query-param shapes too odd to model (repeated keys
/// → qs produces arrays) is left to Node.
pub fn plan(user_id: i64, raw_query: Option<&str>) -> Plan {
    let pairs: Vec<(String, String)> = raw_query
        .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
        .unwrap_or_default();
    let first = |k: &str| pairs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    for k in ["name", "number", "set", "q", "scope", "lang", "prints", "image", "page", "limit"] {
        if pairs.iter().filter(|(kk, _)| kk == k).count() > 1 {
            return Plan::Proxy;
        }
    }
    // `q` present and non-blank: Node takes the raw-query path (parser +
    // sidecar). A blank q is trimmed to '' and behaves as absent — same here.
    if !first("q").unwrap_or_default().trim().is_empty() {
        return Plan::Proxy;
    }
    // Destructure default only fires when the key is absent; `scope=` (empty)
    // stays '' and is neither collection nor internet — same as Node.
    let scope = first("scope").unwrap_or_else(|| "database".to_string());
    if scope == "collection" || scope == "internet" {
        return Plan::Proxy;
    }
    if first("prints").as_deref() == Some("1") {
        return Plan::Proxy; // allPrints can reach the live printing walk
    }
    if first("image").map(|s| !s.is_empty()).unwrap_or(false) {
        return Plan::Proxy; // cvScan ranking lives in Node
    }

    // page = Math.max(1, parseInt(page, 10) || 1); limit = min(250, max(1,
    // parseInt(limit,10) || 60)) — maxLimit is 250 for every scope this gate
    // admits (2000 belongs to the proxied collection+q path).
    let page = first("page").and_then(|v| js_parse_int(&v)).unwrap_or(0);
    let page = if page == 0 { 1 } else { page }.max(1);
    let limit = first("limit").and_then(|v| js_parse_int(&v)).unwrap_or(0);
    let limit = if limit == 0 { 60 } else { limit }.clamp(1, 250);

    Plan::Native(SearchQuery {
        user_id,
        name: first("name").unwrap_or_default().trim().to_string(),
        number: clean_number(first("number").as_deref().unwrap_or("")),
        set: first("set").unwrap_or_default(),
        lang_name: to_name(first("lang").as_deref()),
        page,
        limit,
        // The walked leg exists only for the name-only English branch; an
        // empty name has no query to walk (Node answers [] from the cache
        // path with no sidecar call — the build path's proxy covers it).
        walkable: !first("name").unwrap_or_default().trim().is_empty()
            && clean_number(first("number").as_deref().unwrap_or("")).is_empty()
            && first("set").unwrap_or_default().is_empty()
            && to_name(first("lang").as_deref()) == "English",
    })
}

// ---------------------------------------------------------------------------
// localCacheQuery + parseCardRow + attachOwnedQty
// ---------------------------------------------------------------------------

/// `cardSearchSql.localCacheQuery` — same SQL text, same parameter order.
/// `candidates_json` (from the name index, when honoured) is a JSON array of
/// rowids: the predicate only *narrows the scan* — the LIKE clause stays, so
/// results are identical for a correct candidate set — and `ORDER BY rowid`
/// pins the output to the rowid-ascending order the plain full scan (the
/// proven plan: `SCAN card_cache`, no usable index) returns rows in.
fn local_cache_query(p: &SearchQuery, candidates_json: Option<&str>) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value as Sql;
    let mut sql = String::from("SELECT * FROM card_cache WHERE language = ?");
    let mut params = vec![Sql::Text(p.lang_name.clone())];
    if !p.name.is_empty() {
        sql.push_str(" AND (name LIKE ? OR printed_name LIKE ?)");
        let like = format!("%{}%", p.name);
        params.push(Sql::Text(like.clone()));
        params.push(Sql::Text(like));
    }
    if !p.number.is_empty() {
        sql.push_str(" AND number = ?");
        params.push(Sql::Text(p.number.clone()));
    }
    // parseSetList + setSqlFilter: any of the comma-separated sets, by
    // set_name LIKE or exact set_id.
    let set_list: Vec<String> = p.set.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if !set_list.is_empty() {
        let clause = set_list.iter().map(|_| "(set_name LIKE ? OR set_id = ?)").collect::<Vec<_>>().join(" OR ");
        sql.push_str(&format!(" AND ({clause})"));
        for s in &set_list {
            params.push(Sql::Text(format!("%{s}%")));
            params.push(Sql::Text(s.clone()));
        }
    }
    if let Some(cj) = candidates_json {
        sql.push_str(" AND rowid IN (SELECT value FROM json_each(?))");
        params.push(Sql::Text(cj.to_string()));
        sql.push_str(" ORDER BY rowid");
    }
    // offset = (page-1)*limit, saturating: JS computes it as a float and the
    // driver clamps; i64 saturating math keeps the same "past the end"
    // answer (empty rows) instead of wrapping.
    let offset = (p.page - 1).saturating_mul(p.limit);
    sql.push_str(" LIMIT ? OFFSET ?");
    params.push(Sql::Integer(p.limit));
    params.push(Sql::Integer(offset));
    (sql, params)
}

/// Raw rows → the `parseCardRow` object shape: every column in table order
/// (`SELECT *`, so the statement's own column list is the contract), with
/// the JSON array columns parsed; a missing array column is appended, which
/// is what the JS object spread does.
fn row_to_object(row: &Row) -> Result<Map<String, Value>, String> {
    let ncols = row.as_ref().column_count();
    let names: Vec<String> =
        (0..ncols).map(|i| row.as_ref().column_name(i).unwrap_or("").to_string()).collect();
    let mut obj = Map::with_capacity(ncols + 3);
    for (i, name) in names.iter().enumerate() {
        let v = sqlite_value(row, i).map_err(|e| e.to_string())?;
        let v = match name.as_str() {
            "subtypes" | "types" | "color_identity" => parse_array_col(&v)?,
            _ => v,
        };
        obj.insert(name.clone(), v);
    }
    for name in ["subtypes", "types", "color_identity"] {
        if !obj.contains_key(name) {
            obj.insert(name.to_string(), parse_array_col(&Value::Null)?);
        }
    }
    Ok(obj)
}

/// `attachOwnedQty` from routes/collection.js — verbatim SQL with the
/// `sqlCardKey` helper expanded (`LOWER(TRIM(COALESCE(alias.name, '')))`).
/// Assigns `owned_qty` on each card (appended key, JS assignment).
fn attach_owned_qty(conn: &Connection, cards: &mut [Value], user_id: i64) -> Result<(), String> {
    if cards.is_empty() {
        return Ok(());
    }
    // targets: card_id = card.card_id || card.id, name = card.name (JS ||).
    let targets: Vec<Map<String, Value>> = cards
        .iter()
        .map(|c| {
            let o = c.as_object();
            let card_id = o
                .and_then(|m| m.get("card_id"))
                .filter(|v| truthy(v))
                .cloned()
                .or_else(|| o.and_then(|m| m.get("id")).cloned())
                .unwrap_or(Value::Null);
            let name = o
                .and_then(|m| m.get("name"))
                .filter(|v| truthy(v))
                .cloned()
                .unwrap_or(Value::Null);
            Map::from_iter([("card_id".to_string(), card_id), ("name".to_string(), name)])
        })
        .collect();
    let targets_json = serde_json::to_string(&targets).map_err(|e| e.to_string())?;

    let sql = "
     WITH target_cards AS (
       SELECT
         CAST(key AS INTEGER) AS target_index,
         json_extract(value, '$.card_id') AS card_id,
         json_extract(value, '$.name') AS name
       FROM json_each(?)
     ),
     target_identity AS (
       SELECT
         target_cards.target_index,
         COALESCE(
           NULLIF(LOWER(TRIM(COALESCE(cached_target.name, ''))), ''),
           NULLIF(LOWER(TRIM(target_cards.name)), '')
         ) AS card_key
       FROM target_cards
       LEFT JOIN card_cache cached_target
         ON cached_target.id = target_cards.card_id
     ),
     owned AS MATERIALIZED (
       SELECT LOWER(TRIM(COALESCE(owned_cc.name, ''))) AS card_key, SUM(c.quantity) AS qty
       FROM collection c
       JOIN card_cache owned_cc ON owned_cc.id = c.card_id
       WHERE c.user_id = ? AND c.quantity > 0
       GROUP BY LOWER(TRIM(COALESCE(owned_cc.name, '')))
     )
     SELECT target_identity.target_index, COALESCE(owned.qty, 0) AS qty
     FROM target_identity
     LEFT JOIN owned ON owned.card_key = target_identity.card_key
     ORDER BY target_identity.target_index";
    let mut stmt = conn.prepare_cached(sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query(rusqlite::params![targets_json, user_id]).map_err(|e| e.to_string())?;
    let mut owned: HashMap<i64, Value> = HashMap::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let idx = row.get::<_, i64>(0).map_err(|e| e.to_string())?;
        let qty = sqlite_value(row, 1).map_err(|e| e.to_string())?;
        owned.insert(idx, qty);
    }
    for (i, card) in cards.iter_mut().enumerate() {
        // `owned.get(i) || 0` — falsy (absent, 0, null) collapses to 0.
        let qty = owned.get(&(i as i64)).filter(|v| truthy(v)).cloned().unwrap_or(Value::from(0));
        if let Some(obj) = card.as_object_mut() {
            obj.insert("owned_qty".to_string(), qty);
        }
    }
    Ok(())
}

/// One native cache miss: run the local query (rows found is a precondition
/// checked by the caller via the same call), shape the cards, attach owned
/// quantities, serialize. `Ok(None)` means "Node would not answer from the
/// cache" — the caller must proxy. Any local-cache *query* failure also maps
/// to `Ok(None)` because `runSearch` swallows it into an empty result and
/// falls through to the live path; only post-cache (attachOwnedQty/serialize)
/// errors mirror Node's 500, surfaced as `Err`.
fn build_body(pool: &DbPool, idx: &NameIndex, owned: &OwnedCache, p: &SearchQuery) -> Result<Option<(Vec<u8>, Option<u64>)>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    // Pin one read transaction over probe + card query + owned attach: WAL
    // fixes every statement's view at the probe, so the revision we compare
    // snapshot revs against is the state the response is built from
    // (name_index.rs documents why that equality is sufficient). BEGIN
    // failure proxies (same as a query error).
    let txn = match Txn::begin(&conn) {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(error = %e, "search build: BEGIN failed → proxy");
            return Ok(None);
        }
    };
    let now = crate::db::current_revision(&conn).ok().flatten();
    // Name-index fast path: any miss (cold index, metacharacter pattern,
    // revision moved mid-build) falls through to today's plain scan.
    let mut candidates_json: Option<String> = None;
    if !p.name.is_empty() {
        if let Some(pat) = name_index::indexable_pattern(&p.name) {
            if let Some(r) = now {
                idx.ensure_fresh(pool, r);
                if let Some(snap) = idx.snapshot_for(r) {
                    let ids = snap.candidate_rowids(&pat);
                    tracing::debug!(name = %p.name, candidates = ids.len(), "search build: name index");
                    if ids.is_empty() {
                        return Ok(None); // LIKE finds nothing either → Node's live path
                    }
                    let mut cj = String::with_capacity(ids.len() * 8 + 2);
                    cj.push('[');
                    for (i, id) in ids.iter().enumerate() {
                        if i > 0 {
                            cj.push(',');
                        }
                        cj.push_str(&id.to_string());
                    }
                    cj.push(']');
                    candidates_json = Some(cj);
                }
            }
        }
    }
    let (sql, params) = local_cache_query(p, candidates_json.as_deref());
    let mut cards: Vec<Value> = {
        let mut stmt = conn.prepare_cached(&sql).map_err(|e| format!("prepare: {e}"))?;
        let mut rows = match stmt.query(rusqlite::params_from_iter(params.iter())) {
            Ok(r) => r,
            // runSearch: `catch (e) { localResults = []; }` → live path.
            Err(e) => {
                tracing::debug!(error = %e, sql = %sql, "search build: cache query failed → proxy");
                return Ok(None);
            }
        };
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|_| "cache scan failed".to_string())? {
            out.push(Value::Object(row_to_object(row)?));
        }
        out
    };
    if cards.is_empty() {
        return Ok(None); // Node keeps going: specific/broad Scryfall query.
    }
    // Owned quantities from the per-rev snapshot when it holds; any
    // deviation (row shape, non-integer SUM, build race) → original SQL.
    let mut attached = false;
    if let Some(r) = now {
        if let Some(map) = owned.owned_map(pool, p.user_id, r) {
            attached = name_index::attach_owned_from_snapshot(&mut cards, &map);
        }
    }
    if !attached {
        attach_owned_qty(&conn, &mut cards[..], p.user_id)?;
    }
    txn.commit().map_err(|e| e.to_string())?;
    let body = serde_json::to_vec(&cards).map_err(|e| e.to_string())?;
    // This path never sets meta.total (only the live paths do) → no
    // X-Total-Count header on the native response.
    Ok(Some((body, None)))
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

pub struct SearchEntry {
    rev: i64,
    raw: Bytes,
    gz: Bytes,
    etag: String,
    /// Walked entries carry the broad query's total (X-Total-Count) and the
    /// Node source label; cache-path entries are (None, "cache").
    total: Option<u64>,
    source: &'static str,
}

/// Serialized native search responses keyed by (params, revision). Bounded
/// the same way the collection cache is.
#[derive(Default)]
pub struct SearchCache {
    entries: Mutex<HashMap<String, Arc<SearchEntry>>>,
}

impl SearchCache {
    pub fn new() -> Self {
        Self::default()
    }
    fn get(&self, key: &str, rev: i64) -> Option<Arc<SearchEntry>> {
        let e = self.entries.lock().unwrap();
        e.get(key).filter(|e| e.rev == rev).cloned()
    }
    fn insert(&self, key: String, entry: SearchEntry) -> Arc<SearchEntry> {
        let mut e = self.entries.lock().unwrap();
        if e.len() > 64 {
            e.clear();
        }
        let entry = Arc::new(entry);
        e.insert(key, entry.clone());
        entry
    }
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

pub async fn handle(method: Method, State(st): State<AppState>, req: Request) -> Response {
    // Only GET has a native path; POST /api/search reads a scan image body.
    if method != Method::GET {
        return crate::proxy(State(st.clone()), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st.clone()), req).await;
    };
    let headers = req.headers();
    let uri = req.uri().clone();

    // authenticateToken runs before the route in Node; same 401/403 bodies.
    // µs-cached SQLite, inline on the worker (iter 6).
    let token = auth::bearer_token(headers).map(|s| s.to_string());
    let user = match auth::authenticate(&pool, token.as_deref(), true) {
        Ok(u) => u,
        Err(res) => return res,
    };

    let raw_query = uri.query().map(|s| s.to_string());
    let p = match plan(user.id, raw_query.as_deref()) {
        Plan::Native(p) => p,
        Plan::Proxy => return crate::proxy(State(st.clone()), req).await,
    };

    let inm = headers.get("if-none-match").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).map(|s| s.to_string());

    let rev = pool
        .get()
        .ok()
        .and_then(|c| crate::db::current_revision(&c).ok().flatten());
    let rev_i = rev.unwrap_or(-1);

    let key = p.cache_key();
    let entry: Arc<SearchEntry> = match st.search_cache.get(&key, rev_i) {
        Some(e) => e,
        None => {
            let build_pool = Arc::clone(&pool);
            let build_idx = Arc::clone(&st.name_index);
            let build_owned = Arc::clone(&st.owned_index);
            let p2 = p.clone();
            let built = tokio::task::spawn_blocking(move || {
                let Some((body, _total)) = build_body(&build_pool, &build_idx, &build_owned, &p2)? else {
                    return Ok::<_, String>(None);
                };
                let body = bytes::Bytes::from(body);
                let etag = body_etag(&body);
                let gz = gzip_bytes(&body);
                Ok(Some((body, gz, etag, None, "cache")))
            })
            .await
            .unwrap_or_else(|e| Err(format!("build task panicked: {e}")));
            let (body, gz, etag, total, source) = match built {
                // The local cache had nothing. Node's next leg is the
                // sidecar walk; for the branch we port, run it ourselves —
                // anything else (or any sidecar failure) proxies and Node
                // answers verbatim, upserts/503s and all.
                Ok(None) => {
                    let Some(base) = st
                        .cfg
                        .sidecar_url
                        .as_deref()
                        .filter(|_| p.walkable && st.owns("search_sidecar"))
                    else {
                        return crate::proxy(State(st.clone()), req).await;
                    };
                    let offset = (p.page - 1).saturating_mul(p.limit);
                    let outcome = match sidecar::walk_name(&st, base, &p.name, &p.lang_name, offset, p.limit).await {
                        Ok(o) => o,
                        Err(_) => {
                            tracing::debug!(name = %p.name, "sidecar walk failed → proxy");
                            return crate::proxy(State(st.clone()), req).await;
                        }
                    };
                    let walk_pool = Arc::clone(&pool);
                    let walk_owned = Arc::clone(&st.owned_index);
                    let p3 = p.clone();
                    let walked = tokio::task::spawn_blocking(move || -> Result<_, String> {
                        let mut cards = outcome.cards;
                        let conn = walk_pool.get().map_err(|e| e.to_string())?;
                        let mut attached = false;
                        if rev_i >= 0 {
                            if let Some(map) = walk_owned.owned_map(&walk_pool, p3.user_id, rev_i) {
                                attached = name_index::attach_owned_from_snapshot(&mut cards, &map);
                            }
                        }
                        if !attached {
                            attach_owned_qty(&conn, &mut cards[..], p3.user_id)?;
                        }
                        let body = Bytes::from(serde_json::to_vec(&cards).map_err(|e| e.to_string())?);
                        Ok::<_, String>((body, outcome.total, outcome.source))
                    })
                    .await;
                    let walked = match walked {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::error!(error = %e, "sidecar walk task panicked");
                            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Search failed");
                        }
                    };
                    let (body, total, source) = match walked {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::error!(error = %e, "sidecar walk build failed");
                            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Search failed");
                        }
                    };
                    let etag = body_etag(&body);
                    let gz = gzip_bytes(&body);
                    (body, gz, etag, total, source)
                }
                Ok(Some(v)) => v,
                Err(err) => {
                    tracing::error!(error = %err, "search build failed");
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Search failed");
                }
            };
            st.search_cache.insert(
                key,
                SearchEntry { rev: rev_i, raw: body, gz, etag, total, source },
            )
        }
    };

    // Express's default body ETag on res.json(cards): a matching conditional
    // GET replays as an empty 304, with the headers set before send().
    let tag = entry.etag.clone();
    let src = HeaderValue::from_str(entry.source).unwrap();
    let total_hdr = entry.total.map(|t| (header::HeaderName::from_static("x-total-count"), HeaderValue::from_str(&t.to_string()).unwrap()));
    if etag_matches(inm.as_deref(), &tag) {
        let mut b = Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, HeaderValue::from_str(&tag).unwrap())
            .header("x-source", src);
        if let Some((k, v)) = total_hdr {
            b = b.header(k, v);
        }
        return b
            .header("access-control-expose-headers", HeaderValue::from_static("X-Total-Count, X-Source"))
            .body(Body::empty())
            .unwrap();
    }

    let use_gzip = wants_encoding(ae.as_deref(), "gzip", entry.raw.len());
    let (body, encoding) = if use_gzip { (entry.gz.clone(), Some("gzip")) } else { (entry.raw.clone(), None) };

    let mut b = Response::builder().status(StatusCode::OK).header(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    if let Some(enc) = encoding {
        b = b.header(header::CONTENT_ENCODING, HeaderValue::from_static(enc))
            .header(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    b = b
        .header(header::ETAG, HeaderValue::from_str(&tag).unwrap())
        .header("x-source", src);
    if let Some((k, v)) = total_hdr {
        b = b.header(k, v);
    }
    b.header("access-control-expose-headers", HeaderValue::from_static("X-Total-Count, X-Source"))
        .body(Body::from(body))
        .unwrap()
}

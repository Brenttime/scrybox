//! Port of `GET /api/collection` from `backend/src/routes/collection.js`.
//!
//! Parity contract (BRIEF rule 1): same status, same JSON after parsing —
//! same keys, values, key order, and array order as Node. The row shaping
//! mirrors `parseCardRow` + `applyProxyRarity` + `resolveCardPrice` from
//! `backend/src/utils/priceHelpers.js`, and the deck-allocation pass mirrors
//! `checkedOutAllocation` in `backend/src/utils/collectionHelpers.js`.
//!
//! Performance model: the serialized response (raw + gzip) is cached keyed by
//! the `collection_read_revision` trigger counter — Node owns writes and the
//! triggers bump the counter on every collection/card_cache change, so a
//! cached entry is dropped as soon as any write lands.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use rusqlite::{types::ValueRef, Row};
use serde_json::{Map, Number, Value};

use crate::{auth, db::DbPool, AppState};

/// Process boot id, formatted the way Node's `Date.now().toString(36)`.
fn boot_id() -> &'static str {
    static BOOT: OnceLock<String> = OnceLock::new();
    BOOT.get_or_init(|| {
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        to_base36(ms)
    })
}

fn to_base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// W/"col-{boot}-{rev}-{user}-{qh}" — same shape as utils/collectionEtag.js.
/// `qh_input` is what Node hashes: the originalUrl sliced after the first
/// '?' (the whole URL when there is no '?').
fn collection_etag(rev: i64, user_id: i64, qh_input: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(qh_input.as_bytes());
    let enc = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, h.finalize());
    let qh = &enc[..16.min(enc.len())];
    format!("W/\"col-{}-{rev}-{user_id}-{qh}\"", boot_id())
}

/// Express's *default* ETag, i.e. what `res.json()` stamps on every response
/// the route does not tag itself (the `etag` npm package, weak form):
/// `W/"{byteLength:hex}-{base64(sha1(body))[0..27]}"`. Reproduced exactly so a
/// conditional GET on an untagged path (notably the ~27 MB unpaginated read)
/// can be answered 304 from the cache instead of shipping the body.
pub fn body_etag(raw: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(raw);
    let digest = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, h.finalize());
    format!("W/\"{:x}-{}\"", raw.len(), &digest[..27.min(digest.len())])
}

/// Node: `inm.split(',').some(t => t.trim() === tag || t.trim() === '*')`.
pub(crate) fn etag_matches(inm: Option<&str>, tag: &str) -> bool {
    let Some(inm) = inm else { return false };
    inm.split(',').any(|t| {
        let t = t.trim();
        t == tag || t == "*"
    })
}

// ---------------------------------------------------------------------------
// Query parsing (mirrors express req.query handling for this route)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CollectionParams {
    user_id: i64,
    /// `req.query.is_trade !== undefined` + the 'true'/'1' coercion.
    is_trade: Option<i64>,
    /// `if (req.query.card_id)` — only when present and non-empty.
    card_id: Option<String>,
    paginated: bool,
    limit: i64,
    offset: i64,
    alloc: bool,
    count_zero: bool,
    /// Raw (percent-encoded) query string, as the client sent it — hashed for
    /// the ETag exactly like Node hashes req.originalUrl.
    qs: String,
}

/// JS `parseInt(x, 10)`: leading whitespace, optional sign, ASCII digits,
/// stop at the first non-digit; None for NaN.
fn js_parse_int(s: &str) -> Option<i64> {
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

pub fn parse_params(user_id: i64, raw_query: Option<&str>) -> CollectionParams {
    let pairs: Vec<(String, String)> = raw_query
        .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
        .unwrap_or_default();
    let first = |k: &str| pairs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());

    let is_trade = first("is_trade").map(|v| if v == "true" || v == "1" { 1 } else { 0 });
    let card_id = first("card_id").filter(|v| !v.is_empty());
    let paginated = pairs.iter().any(|(k, _)| k == "limit" || k == "offset");
    // Math.min(2000, Math.max(1, parseInt(limit,10) || 100))
    let limit_raw = first("limit").and_then(|v| js_parse_int(&v)).unwrap_or(0);
    let limit = if limit_raw == 0 { 100 } else { limit_raw }.clamp(1, 2000);
    // Math.max(0, parseInt(offset,10) || 0)
    let offset = first("offset").and_then(|v| js_parse_int(&v)).unwrap_or(0).max(0);
    let alloc = first("include_allocation").as_deref() == Some("1");
    let count_zero = first("count").as_deref() == Some("0");
    let qs = raw_query.unwrap_or("").to_string();
    CollectionParams { user_id, is_trade, card_id, paginated, limit, offset, alloc, count_zero, qs }
}

impl CollectionParams {
    fn cache_key(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}|{}",
            self.user_id,
            self.is_trade.map_or('-'.to_string(), |v| v.to_string()),
            self.card_id.as_deref().unwrap_or(""),
            self.alloc,
            self.count_zero,
            self.paginated,
            self.limit,
            self.offset,
        )
    }
}

// ---------------------------------------------------------------------------
// Row building — parity-critical JSON shaping
// ---------------------------------------------------------------------------

/// The SELECT list, in Node's exact order (collection part).
const COLLECT_COLS: &[&str] = &[
    "entry_id", "card_id", "quantity", "condition", "printing", "language", "purchase_price",
    "added_at", "is_trade", "notes", "is_proxy",
];
/// The SELECT list, in Node's exact order (card_cache part).
const CARD_COLS: &[&str] = &[
    "name", "printed_name", "supertype", "subtypes", "types", "cmc", "color_identity", "rarity",
    "set_id", "set_name", "number", "image_url", "price_trend", "price_normal", "price_holofoil",
    "price_currency", "price_source", "price_estimated", "tcgplayer_url", "cardmarket_url",
    "tcgplayer_product_id",
];
const ALL_COLS: &[&str] = &{
    let mut cols: [&str; COLLECT_COLS.len() + CARD_COLS.len()] = [""; COLLECT_COLS.len() + CARD_COLS.len()];
    let mut i = 0;
    while i < COLLECT_COLS.len() {
        cols[i] = COLLECT_COLS[i];
        i += 1;
    }
    let mut j = 0;
    while j < CARD_COLS.len() {
        cols[COLLECT_COLS.len() + j] = CARD_COLS[j];
        j += 1;
    }
    cols
};

pub(crate) fn sqlite_value(row: &Row, idx: usize) -> rusqlite::Result<Value> {
    Ok(match row.get_ref(idx)? {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => json_num(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        // No selected column is a BLOB; render Buffer JSON the way Node's
        // sqlite3 driver + JSON.stringify would, rather than silently 500.
        ValueRef::Blob(b) => Value::Object(
            Map::from_iter([
                ("type".to_string(), Value::String("Buffer".into())),
                ("data".to_string(), Value::Array(b.iter().map(|&x| Value::from(x as u64)).collect())),
            ]),
        ),
        })
}

/// JSON.stringify formatting for doubles: integral values print without a
/// fraction ("-0" prints as "0", NaN/Infinity as null — like Node).
pub(crate) fn json_num(f: f64) -> Value {
    if f.is_nan() || f.is_infinite() {
        return Value::Null;
    }
    if f == 0.0 {
        return Value::from(0);
    }
    if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
        return Value::from(f as i64);
    }
    Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
}

/// JS truthiness for the raw SQLite values this route touches.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i != 0
            } else if let Some(u) = n.as_u64() {
                u != 0
            } else if let Some(f) = n.as_f64() {
                f != 0.0 && !f.is_nan()
            } else {
                true
            }
        }
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// JS `Number(x) > 0` style comparison used by resolveCardPrice.
pub(crate) fn gt_zero(v: &Value) -> bool {
    match v {
        Value::Number(n) => n.as_f64().map(|f| f > 0.0 && !f.is_nan()).unwrap_or(true),
        Value::String(s) => !s.trim().is_empty() && s.trim().parse::<f64>().map(|f| f > 0.0).unwrap_or(false),
        _ => false,
    }
}

/// resolveCardPrice(row) from utils/priceHelpers.js, over raw row values.
pub(crate) fn resolve_card_price(is_proxy: &Value, printing: &Value, pt: &Value, pn: &Value, ph: &Value) -> Value {
    if truthy(is_proxy) {
        return Value::from(0);
    }
    if printing.as_str() == Some("Holofoil") && !ph.is_null() && gt_zero(ph) {
        return ph.clone();
    }
    if printing.as_str() == Some("Normal") && !pn.is_null() && gt_zero(pn) {
        return pn.clone();
    }
    if truthy(pt) {
        pt.clone()
    } else {
        Value::from(0)
    }
}

/// JSON.parse(row.col || '[]') from parseCardRow.
pub(crate) fn parse_array_col(v: &Value) -> Result<Value, String> {
    if v.is_null() {
        return Ok(Value::Array(Vec::new()));
    }
    let text = match v {
        Value::String(s) if s.is_empty() => return Ok(Value::Array(Vec::new())),
        Value::String(s) => s.clone(),
        other => other.to_string(), // JSON.parse coerces non-strings via String()
    };
    serde_json::from_str(&text).map_err(|e| format!("bad array column: {e}"))
}

const SELECT_BODY: &str = "
      SELECT
        c.id as entry_id,
        c.card_id,
        c.quantity,
        c.condition,
        c.printing,
        c.language,
        c.purchase_price,
        c.added_at,
        c.is_trade,
        c.notes,
        c.is_proxy,
        cc.name,
        -- The localized name for a non-English printing, so every view that
        -- renders a collection card can show it as the card actually reads.
        cc.printed_name,
        cc.supertype,
        cc.subtypes,
        cc.types,
        cc.cmc,
        cc.color_identity,
        cc.rarity,
        cc.set_id,
        cc.set_name,
        cc.number,
        cc.image_url,
        cc.price_trend,
        cc.price_normal,
        cc.price_holofoil,
        cc.price_currency,
        cc.price_source,
        cc.price_estimated,
        cc.tcgplayer_url,
        cc.cardmarket_url,
        cc.tcgplayer_product_id
      FROM collection c
      JOIN card_cache cc ON c.card_id = cc.id";

fn filter_clause(p: &CollectionParams) -> (String, Vec<rusqlite::types::Value>) {
    let mut sql = String::from("WHERE c.user_id = ?");
    let mut params = vec![rusqlite::types::Value::Integer(p.user_id)];
    if let Some(t) = p.is_trade {
        sql.push_str(" AND c.is_trade = ?");
        params.push(rusqlite::types::Value::Integer(t));
    }
    if let Some(cid) = &p.card_id {
        sql.push_str(" AND c.card_id = ?");
        params.push(rusqlite::types::Value::Text(cid.clone()));
    }
    (sql, params)
}

/// checkedOutAllocation from utils/collectionHelpers.js (SQL expanded from
/// sqlCardKey/sqlIsBasicLand verbatim).
fn checked_out_allocation(conn: &rusqlite::Connection, user_id: i64) -> rusqlite::Result<HashMap<i64, f64>> {
    let card_key = |a: &str| format!("LOWER(TRIM(COALESCE({a}.name, '')))");
    let required_sql = format!(
        "
    SELECT {key} AS card_key, SUM(dc.quantity) AS req
    FROM deck_cards dc
    JOIN card_cache needed_cc ON needed_cc.id = dc.card_id
    JOIN decks d ON dc.deck_id = d.id
    WHERE d.user_id = ?
      AND d.checked_out = 1
      AND dc.quantity > 0
      AND NOT ((LOWER(COALESCE(needed_cc.supertype, '')) = 'land' OR INSTR(LOWER(COALESCE(needed_cc.subtypes, '')), '\"land\"') > 0) AND ({key} IN ('plains', 'island', 'swamp', 'mountain', 'forest', 'wastes') OR INSTR(LOWER(COALESCE(needed_cc.subtypes, '')), '\"basic\"') > 0))
    GROUP BY {key}
  ",
        key = card_key("needed_cc")
    );
    let mut required: Vec<(String, f64)> = Vec::new();
    {
        let mut stmt = conn.prepare_cached(&required_sql)?;
        let rows = stmt.query_map([user_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<f64>>(1)?.unwrap_or(0.0)))
        })?;
        for row in rows {
            required.push(row?);
        }
    }
    if required.is_empty() {
        return Ok(HashMap::new());
    }

    let keys_json = serde_json::to_string(&required.iter().map(|(k, _)| k).collect::<Vec<_>>()).unwrap();
    let entries_sql = format!(
        "
    SELECT
      c.id AS entry_id,
      c.quantity,
      {key} AS card_key
    FROM card_cache owned_cc
    CROSS JOIN collection c
    WHERE c.card_id = owned_cc.id
      AND c.user_id = ?
      AND c.quantity > 0
      AND {key} IN (SELECT value FROM json_each(?))
    ORDER BY c.added_at DESC, c.id DESC
  ",
        key = card_key("owned_cc")
    );
    let mut remaining: HashMap<String, f64> = required.into_iter().collect();
    let mut alloc = HashMap::new();
    {
        let mut stmt = conn.prepare_cached(&entries_sql)?;
        let rows = stmt.query_map(rusqlite::params![user_id, keys_json], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<f64>>(1)?.unwrap_or(0.0), r.get::<_, String>(2)?))
        })?;
        for row in rows {
            let (entry_id, qty, key) = row?;
            let need = remaining.get(&key).copied().unwrap_or(0.0);
            if need <= 0.0 {
                continue;
            }
            let take = qty.min(need);
            if take <= 0.0 {
                continue;
            }
            remaining.insert(key, need - take);
            alloc.insert(entry_id, take);
        }
    }
    Ok(alloc)
}

/// Run the full query + shaping for one cache miss. Errors surface as the
/// route's 500 body, matching Node's catch block.
fn build_body(
    pool: &DbPool,
    p: &CollectionParams,
) -> Result<(Vec<u8>, Option<u64>), String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    let (filter_sql, mut params) = filter_clause(p);

    let total = if p.paginated && !p.count_zero {
        let sql = format!(
            "SELECT COUNT(*) AS count
         FROM collection c
         JOIN card_cache cc ON c.card_id = cc.id
         {filter_sql}"
        );
        let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
        let n: i64 = stmt
            .query_row(rusqlite::params_from_iter(params.iter()), |r| r.get(0))
            .map_err(|e| e.to_string())?;
        Some(n as u64)
    } else {
        None
    };

    let mut sql = format!("{SELECT_BODY}\n      {filter_sql}\n      ORDER BY c.added_at DESC, c.id DESC");
    if p.paginated {
        sql.push_str("\n      LIMIT ? OFFSET ?");
        params.push(rusqlite::types::Value::Integer(p.limit));
        params.push(rusqlite::types::Value::Integer(p.offset));
    }

    let alloc = if !p.paginated || p.alloc {
        Some(checked_out_allocation(&conn, p.user_id).map_err(|e| e.to_string())?)
    } else {
        None
    };

    let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter())).map_err(|e| e.to_string())?;
    let mut out: Vec<Value> = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        // Raw values first (resolveCardPrice/applyProxyRarity read the raw row).
        let mut raw = Vec::with_capacity(ALL_COLS.len());
        for i in 0..ALL_COLS.len() {
            raw.push(sqlite_value(row, i).map_err(|e| e.to_string())?);
        }
        let idx = |name: &str| ALL_COLS.iter().position(|c| *c == name).unwrap();
        let is_proxy = &raw[idx("is_proxy")];
        let printing = &raw[idx("printing")];
        let rarity_raw = &raw[idx("rarity")];

        let mut obj = Map::with_capacity(ALL_COLS.len() + 2);
        for (i, name) in ALL_COLS.iter().enumerate() {
            let v = match *name {
                "is_proxy" => Value::from(if truthy(is_proxy) { 1 } else { 0 }),
                "rarity" => {
                    if truthy(is_proxy) {
                        Value::String("Proxy".into())
                    } else {
                        rarity_raw.clone()
                    }
                }
                "subtypes" | "types" | "color_identity" => parse_array_col(&raw[i])?,
                "price_trend" => resolve_card_price(
                    is_proxy,
                    printing,
                    &raw[idx("price_trend")],
                    &raw[idx("price_normal")],
                    &raw[idx("price_holofoil")],
                ),
                _ => raw[i].clone(),
            };
            obj.insert((*name).to_string(), v);
        }
        // applyProxyRarity appends base_rarity as a brand-new key (the SELECT
        // has no base_rarity column), landing it after the row keys.
        obj.insert("base_rarity".to_string(), rarity_raw.clone());
        if let Some(alloc) = &alloc {
            obj.insert(
                "checked_out_qty".to_string(),
                alloc.get(&raw[idx("entry_id")].as_i64().unwrap_or(-1)).map(|q| json_num(*q)).unwrap_or(Value::from(0)),
            );
        }
        out.push(Value::Object(obj));
    }

    let body = serde_json::to_vec(&out).map_err(|e| e.to_string())?;
    Ok((body, total))
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

pub struct CacheEntry {
    rev: i64,
    raw: Bytes,
    gz: Bytes,
    total: Option<u64>,
    /// Express-default body ETag, computed once with the body.
    etag: String,
}

/// Serialized responses keyed by (params, collection_read_revision).
/// Node's write triggers bump the revision, which invalidates every entry.
#[derive(Default)]
pub struct CollectionCache {
    entries: Mutex<HashMap<String, Arc<CacheEntry>>>,
}

impl CollectionCache {
    pub fn new() -> Self {
        Self::default()
    }
    fn get(&self, key: &str, rev: i64) -> Option<Arc<CacheEntry>> {
        let e = self.entries.lock().unwrap();
        e.get(key).filter(|e| e.rev == rev).cloned()
    }
    fn insert(&self, key: String, entry: CacheEntry) -> Arc<CacheEntry> {
        let mut e = self.entries.lock().unwrap();
        if e.len() > 64 {
            e.clear();
        }
        let entry = Arc::new(entry);
        e.insert(key, entry.clone());
        entry
    }
}

pub(crate) fn gzip_bytes(raw: &[u8]) -> Bytes {
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(raw).expect("vec write");
    Bytes::from(enc.finish().expect("gzip"))
}

/// compression@1.x behaviour: gzip when Accept-Encoding offers it and the
/// body clears the 1 KB threshold.
pub(crate) fn wants_encoding(ae: Option<&str>, needle: &str, len: usize) -> bool {
    len >= 1024 && ae.map(|a| a.to_ascii_lowercase().contains(needle)).unwrap_or(false)
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
    // The port only owns GET; POST/PUT/DELETE keep their Node home.
    if method != Method::GET {
        return crate::proxy(State(st.clone()), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st.clone()), req).await;
    };
    let headers = req.headers().clone();
    let uri = req.uri().clone();

    // authenticate (blocking DB call offlined from the runtime)
    let auth_pool = Arc::clone(&pool);
    let auth_headers = headers.clone();
    let authed = tokio::task::spawn_blocking(move || auth::authenticate(&auth_pool, &auth_headers, true)).await;
    let user = match authed {
        Ok(Ok(u)) => u,
        Ok(Err(res)) => return res,
        Err(e) => {
            tracing::error!(error = %e, "auth task panicked");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Authentication error");
        }
    };

    let raw_query = uri.query().map(|s| s.to_string());
    let p = parse_params(user.id, raw_query.as_deref());
    let inm = headers.get("if-none-match").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).map(|s| s.to_string());

    // Revision probe: single indexed read; also the cache invalidator.
    let probe_pool = Arc::clone(&pool);
    let rev = tokio::task::spawn_blocking(move || crate::db::current_revision(&probe_pool.get().unwrap()))
        .await
        .unwrap_or(Ok(None))
        .unwrap_or(None);

    let mut extra_headers: Vec<(header::HeaderName, HeaderValue)> = Vec::new();
    // True when the route's own revision tag was emitted; Express's default
    // body tag must not shadow it (Node sets ETag once, route-first).
    let mut route_tagged = false;
    if p.paginated && !p.alloc {
        if let Some(rev) = rev {
            route_tagged = true;
            let qh_input = match &raw_query {
                Some(qs) => qs.clone(),
                None => uri.path().to_string(),
            };
            let tag = collection_etag(rev, user.id, &qh_input);
            extra_headers.push((header::ETAG, HeaderValue::from_str(&tag).unwrap()));
            extra_headers.push((
                header::CACHE_CONTROL,
                HeaderValue::from_str("private, no-cache").unwrap(),
            ));
            if etag_matches(inm.as_deref(), &tag) {
                let mut b = Response::builder().status(StatusCode::NOT_MODIFIED);
                for (k, v) in extra_headers {
                    b = b.header(k, v);
                }
                return b.body(Body::empty()).unwrap();
            }
        }
    }

    let key = p.cache_key();
    let rev_i = rev.unwrap_or(-1);
    let entry: Arc<CacheEntry> = match st.collection_cache.get(&key, rev_i) {
        Some(e) => e,
        None => {
            let build_pool = Arc::clone(&pool);
            let p2 = p.clone();
            // Query + shaping + gzip all run off the async runtime: the full
            // response is ~27 MB raw.
            let built = tokio::task::spawn_blocking(move || {
                let (body, total) = build_body(&build_pool, &p2)?;
                let etag = body_etag(&body);
                let gz = gzip_bytes(&body);
                Ok::<_, String>((body, gz, total, etag))
            })
            .await
            .unwrap_or_else(|e| Err(format!("build task panicked: {e}")));
            match built {
                Ok((body, gz, total, etag)) => st.collection_cache.insert(
                    key,
                    CacheEntry { rev: rev_i, raw: Bytes::from(body), gz, total, etag },
                ),
                Err(err) => {
                    tracing::error!(error = %err, "collection build failed");
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch collection");
                }
            }
        }
    };

    // Untagged paths still get Express's default body ETag, so a conditional
    // GET replays as a 304 from cache instead of resending (for the full
    // collection, ~27 MB). Node's `res.json()` does the same, minus the cost.
    if !route_tagged {
        let tag = entry.etag.clone();
        extra_headers.push((header::ETAG, HeaderValue::from_str(&tag).unwrap()));
        if etag_matches(inm.as_deref(), &tag) {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, HeaderValue::from_str(&tag).unwrap())
                .body(Body::empty())
                .unwrap();
        }
    }

    let use_gzip = wants_encoding(ae.as_deref(), "gzip", entry.raw.len());
    let (body, encoding) = if use_gzip {
        (entry.gz.clone(), Some("gzip"))
    } else {
        (entry.raw.clone(), None)
    };

    let mut b = Response::builder().status(StatusCode::OK).header(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    if let Some(enc) = encoding {
        b = b.header(header::CONTENT_ENCODING, HeaderValue::from_static(enc))
            .header(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    for (k, v) in extra_headers {
        b = b.header(k, v);
    }
    if p.paginated && !p.count_zero {
        if let Some(total) = entry.total {
            b = b
                .header("x-total-count", HeaderValue::from_str(&total.to_string()).unwrap())
                .header("access-control-expose-headers", HeaderValue::from_static("X-Total-Count"));
        }
    }
    b.body(Body::from(body)).unwrap()
}

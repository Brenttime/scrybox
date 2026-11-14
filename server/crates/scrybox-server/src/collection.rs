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
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tokio::sync::{OnceCell, Semaphore};

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
    /// Shapes whose body embeds `checked_out_qty`: the full/unpaginated
    /// shape always, the paginated shape only with include_allocation=1.
    /// These cache entries carry a second validity key
    /// (`deck_list_revision`) — review 2026-11-13 C1.
    fn needs_alloc(&self) -> bool {
        !self.paginated || self.alloc
    }

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
///
/// Sequential reference implementation — the parallel fast path
/// (`build_body_parallel`) must stay byte-identical to this; that identity is
/// what the collection test suite pins.
pub fn build_body(
    pool: &DbPool,
    p: &CollectionParams,
) -> Result<(Vec<u8>, Option<u64>), String> {
    let (values, total) = build_values(pool, p)?;
    let body = serde_json::to_vec(&values).map_err(|e| e.to_string())?;
    Ok((body, total))
}

/// Query + parity-critical row shaping only (no serialization) — split out of
/// `build_body` so the perf probe can time the stages independently.
pub fn build_values(
    pool: &DbPool,
    p: &CollectionParams,
) -> Result<(Vec<Value>, Option<u64>), String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    let (filter_sql, params) = filter_clause(p);
    let total = count_total(&conn, p, &filter_sql, &params)?;
    let alloc = if !p.paginated || p.alloc {
        Some(checked_out_allocation(&conn, p.user_id).map_err(|e| e.to_string())?)
    } else {
        None
    };
    let raw = collect_raw_rows(&conn, p, &filter_sql, &params)?;
    let out = (0..raw.len())
        .map(|i| shape_row(raw.row(i), alloc.as_ref()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((out, total))
}

/// The parallel fast path used by the handler on cache miss.
///
/// The SQLite cursor stays on one connection (one consistent snapshot) for
/// row collection; `checked_out_allocation` overlaps on a second pool
/// connection; shaping + per-row serialization run on scoped worker threads.
/// Each worker serializes its contiguous row range into ONE pooled chunk
/// buffer (iter-18 chunk arena) and the groups are stitched by hand —
/// byte-identical to `serde_json::to_vec` over the whole `Vec<Value>`
/// (`to_writer` shares the element formatter; comma joining is the same code
/// path), pinned by a test against the sequential oracle.
pub fn build_body_parallel(
    pool: &Arc<DbPool>,
    p: &CollectionParams,
) -> Result<(Vec<u8>, Option<u64>), String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    let (filter_sql, params) = filter_clause(p);
    let total = count_total(&conn, p, &filter_sql, &params)?;
    let need_alloc = !p.paginated || p.alloc;

    // Overlap the allocation queries with row collection: second connection,
    // same WAL snapshot semantics as running them sequentially (they were
    // already separate autocommit read statements).
    let alloc_pool = Arc::clone(pool);
    let user_id = p.user_id;
    let (raw_rows, alloc) = if need_alloc {
        std::thread::scope(|s| -> Result<(RowStore, Option<HashMap<i64, f64>>), String> {
            let alloc_handle = s.spawn(move || -> Result<HashMap<i64, f64>, String> {
                let conn = alloc_pool.get().map_err(|e| e.to_string())?;
                checked_out_allocation(&conn, user_id).map_err(|e| e.to_string())
            });
            let raw = collect_raw_rows(&conn, p, &filter_sql, &params)?;
            let a = alloc_handle.join().map_err(|_| "alloc thread panicked".to_string())??;
            Ok::<_, String>((raw, Some(a)))
        })?
    } else {
        (collect_raw_rows(&conn, p, &filter_sql, &params)?, None)
    };

    let nthreads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 4);
    let n = raw_rows.len();
    // Each group buffer holds `row,row,...` for one contiguous row range.
    let groups: Vec<Vec<u8>> = if n < 4000 || nthreads == 1 {
        let mut buf = take_chunk(n.saturating_mul(CHUNK_ROW_EST_BYTES) + 64);
        for r in 0..n {
            if r > 0 {
                buf.push(b',');
            }
            let v = match shape_row(raw_rows.row(r), alloc.as_ref()) {
                Ok(v) => v,
                Err(e) => {
                    give_chunk(buf);
                    return Err(e);
                }
            };
            if let Err(e) = serde_json::to_writer(&mut buf, &v) {
                give_chunk(buf);
                return Err(format!("serialize row {r}: {e}"));
            }
        }
        vec![buf]
    } else {
        let step = n.div_ceil(nthreads);
        let alloc_ref = alloc.as_ref();
        let store = &raw_rows;
        let results = std::thread::scope(|s| {
            let handles: Vec<_> = (0..nthreads)
                .map(|t| {
                    let lo = t * step;
                    let hi = (lo + step).min(n);
                    s.spawn(move || -> Result<Vec<u8>, String> {
                        if lo >= hi {
                            return Ok(Vec::new());
                        }
                        let mut buf = take_chunk((hi - lo) * CHUNK_ROW_EST_BYTES + 64);
                        for r in lo..hi {
                            if r > lo {
                                buf.push(b',');
                            }
                            let v = match shape_row(store.row(r), alloc_ref) {
                                Ok(v) => v,
                                Err(e) => {
                                    give_chunk(buf);
                                    return Err(e);
                                }
                            };
                            if let Err(e) = serde_json::to_writer(&mut buf, &v) {
                                give_chunk(buf);
                                return Err(format!("serialize row {r}: {e}"));
                            }
                        }
                        Ok(buf)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().map_err(|_| "shape thread panicked".to_string())?)
                .collect::<Vec<Result<Vec<u8>, String>>>()
        });
        let mut groups: Vec<Vec<u8>> = Vec::with_capacity(results.len());
        for g in results {
            match g {
                Ok(g) => groups.push(g),
                Err(e) => {
                    for g in groups {
                        give_chunk(g);
                    }
                    return Err(e);
                }
            }
        }
        groups
    };

    // Stitch the array: serde_json writes elements back-to-back separated by
    // ',' inside the brackets, so concatenating the group buffers (with ','
    // between non-empty groups) reproduces the whole-array serialization byte
    // for byte. The group buffers then return to the arena instead of being
    // freed to glibc — that churn is the collection_full p99 tail.
    let mut body = Vec::with_capacity(groups.iter().map(|g| g.len() + 1).sum::<usize>() + 2);
    body.push(b'[');
    let mut first = true;
    for g in &groups {
        if g.is_empty() {
            continue;
        }
        if !first {
            body.push(b',');
        }
        body.extend_from_slice(g);
        first = false;
    }
    body.push(b']');
    for g in groups {
        give_chunk(g);
    }
    Ok((body, total))
}

/// COUNT(*) companion for paginated shapes (same SQL as Node's count query).
fn count_total(
    conn: &rusqlite::Connection,
    p: &CollectionParams,
    filter_sql: &str,
    params: &[rusqlite::types::Value],
) -> Result<Option<u64>, String> {
    if !p.paginated || p.count_zero {
        return Ok(None);
    }
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
    Ok(Some(n as u64))
}

// ---------------------------------------------------------------------------
// Row store + chunk arena (iter 18)
// ---------------------------------------------------------------------------

/// Row-major strided store of the raw (unshaped) column values, one row of
/// `ALL_COLS.len()` values per stride. The old `Vec<Vec<Value>>` cost one
/// heap block per row (~27k blocks per full-collection build, all freed back
/// to glibc on every invalidation); the stride makes it one. `Arc` keeps the
/// shared-build `Clone` cheap.
#[derive(Clone)]
struct RowStore {
    vals: Arc<Vec<Value>>,
    width: usize,
}

impl RowStore {
    fn len(&self) -> usize {
        self.vals.len() / self.width
    }
    fn row(&self, r: usize) -> &[Value] {
        let off = r * self.width;
        &self.vals[off..off + self.width]
    }
}

/// Chunk arena (iter 18): a full-collection build serializes ~27k rows. It
/// used to allocate one `Vec<u8>` per row part; instead each worker
/// serializes its whole row range into ONE buffer borrowed from this pool,
/// which survives across builds. That kills the ~54k short-lived allocations
/// per invalidation behind the collection_full write-mix p99 tail (656–977 ms
/// at iter 17) and feeds the RSS ratchet (iter 15/16) far less: only the
/// final stitched body the cache keeps is a fresh allocation.
const CHUNK_KEEP_MAX_CAP: usize = 32 << 20;
const CHUNK_POOL_MAX_BUFFERS: usize = 8; // ≤ 2 concurrent builds × 4 workers
const CHUNK_POOL_MAX_BYTES: u64 = 128 << 20;
/// Per-row serialization-size estimate used to pre-size buffers (full
/// collection runs ≈1 KB/row).
const CHUNK_ROW_EST_BYTES: usize = 1152;

static CHUNK_POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
static CHUNK_POOL_BYTES: AtomicU64 = AtomicU64::new(0);

/// Borrow a serialize buffer with at least `desired` capacity, preferring the
/// smallest pooled buffer that fits (best fit) so the big full-build buffers
/// stay reserved for full-build shapes.
fn take_chunk(desired: usize) -> Vec<u8> {
    let mut pool = CHUNK_POOL.lock().unwrap_or_else(|e| e.into_inner());
    let mut best: Option<usize> = None;
    for (i, b) in pool.iter().enumerate() {
        if b.capacity() >= desired && best.map_or(true, |j| b.capacity() < pool[j].capacity()) {
            best = Some(i);
        }
    }
    match best {
        Some(i) => {
            let mut b = pool.swap_remove(i);
            CHUNK_POOL_BYTES.fetch_sub(b.capacity() as u64, Ordering::Relaxed);
            b.clear();
            b
        }
        None => Vec::with_capacity(desired),
    }
}

/// Return a serialize buffer for a later build to reuse. Oversized buffers
/// and retention beyond the pool budget are dropped to the allocator instead
/// of being hoarded.
fn give_chunk(mut buf: Vec<u8>) {
    let cap = buf.capacity();
    if cap == 0 || cap > CHUNK_KEEP_MAX_CAP {
        return;
    }
    if CHUNK_POOL_BYTES.fetch_add(cap as u64, Ordering::Relaxed) + cap as u64 > CHUNK_POOL_MAX_BYTES {
        CHUNK_POOL_BYTES.fetch_sub(cap as u64, Ordering::Relaxed);
        return;
    }
    buf.clear();
    let mut pool = CHUNK_POOL.lock().unwrap_or_else(|e| e.into_inner());
    if pool.len() >= CHUNK_POOL_MAX_BUFFERS {
        drop(pool);
        CHUNK_POOL_BYTES.fetch_sub(cap as u64, Ordering::Relaxed);
        return;
    }
    pool.push(buf);
}

#[cfg(test)]
fn chunk_pool_len() -> usize {
    CHUNK_POOL.lock().unwrap().len()
}

/// Step the main SELECT, returning the raw (unshaped) column values per row.
fn collect_raw_rows(
    conn: &rusqlite::Connection,
    p: &CollectionParams,
    filter_sql: &str,
    params: &[rusqlite::types::Value],
) -> Result<RowStore, String> {
    let mut sql = format!("{SELECT_BODY}\n      {filter_sql}\n      ORDER BY c.added_at DESC, c.id DESC");
    let mut params: Vec<rusqlite::types::Value> = params.to_vec();
    if p.paginated {
        sql.push_str("\n      LIMIT ? OFFSET ?");
        params.push(rusqlite::types::Value::Integer(p.limit));
        params.push(rusqlite::types::Value::Integer(p.offset));
    }
    let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter())).map_err(|e| e.to_string())?;
    let mut vals: Vec<Value> = Vec::with_capacity(ALL_COLS.len() * 512);
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        for i in 0..ALL_COLS.len() {
            vals.push(sqlite_value(row, i).map_err(|e| e.to_string())?);
        }
    }
    Ok(RowStore { vals: Arc::new(vals), width: ALL_COLS.len() })
}

/// Parity-critical single-row shaping (parseCardRow + applyProxyRarity +
/// resolveCardPrice + checkedOutAllocation). Shared by the sequential and
/// parallel paths so the shaping logic lives in exactly one place.
fn shape_row(raw: &[Value], alloc: Option<&HashMap<i64, f64>>) -> Result<Value, String> {
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
    Ok(Value::Object(obj))
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

/// Background pre-warm (iter 14, owner hint "collection_full write-mix
/// rebuild could be incremental/pre-warmed"): poll the revision counter and
/// pre-build the post-bump body for recently-requested shapes, so the first
/// request after an invalidation never rides the ~400 ms full-body compute
/// path. Purely an optimization: the request-path miss build is untouched
/// and the refresher produces the identical body through the same code, so
/// strict parity (BRIEF rule 1) is unaffected.
const REFRESH_TICK: Duration = Duration::from_millis(250);
/// Only shapes requested within this window are pre-built; a shape nobody
/// asks for must not cost background query work (Node does no unrequested
/// work either).
const REFRESH_ACTIVE: Duration = Duration::from_secs(30);
/// Debounce: the revision must be observed stable across two consecutive
/// ticks before the fan-out — keeps in-flight edit+revert write pairs from
/// spawning builds for the intermediate state.
const REFRESH_DEBOUNCE_TICKS: u8 = 2;
/// After a failed pre-build, back off per key instead of retrying every tick.
const REFRESH_COOLDOWN: Duration = Duration::from_secs(5);
/// Soft cap on the recently-requested-shape map (params variety × users).
const RECENT_MAX: usize = 256;

/// Byte budget for stored bodies (raw + lazy gz). The 64-shape cap alone is
/// not a memory bound: six ~27 MB full-collection shapes plus allocator-
/// retained freed blocks ratcheted RSS to ~3 GB over a realistic run
/// (iter-15). Above this budget `put` LRU-evicts (least-recently-served
/// first) until the stored bytes fit.
///
/// Gate D iter-22: the 1 GiB default was chosen to stop the 3 GB runaway,
/// not as a target — per-leg bench attribution showed this budget alone
/// (+379 MB VmHWM on collection_full_varied) overshoots the idle-66/
/// peak-110 Gate D envelope. Default now 256 MiB, env-tunable
/// (SCRYBOX_COLLECTION_CACHE_MAX_MB) for hosts that want the old headroom.
const STORE_MAX_BYTES_DEFAULT: u64 = 256 << 20; // 256 MiB

fn store_max_bytes() -> u64 {
    std::env::var("SCRYBOX_COLLECTION_CACHE_MAX_MB").ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .map(|mb| mb << 20)
        .unwrap_or(STORE_MAX_BYTES_DEFAULT)
}

/// Freed bytes (pruned/evicted) in one `put` before the refresher tick is
/// asked to return heap to the OS (`malloc_trim`). Small churn is left for
/// the allocator to reuse; the refresher swaps this flag off the request
/// path so the trim never lands on a serving thread.
pub(crate) const TRIM_DIRTY_BYTES: u64 = 64 << 20; // 64 MiB
/// Cost gate for the pre-warm fan-out (iter 15). Unbudgeted fan-out over
/// every recent shape — cheap paged builds included — kept 2 of the 4 cores
/// busy on every write-mix tick, starved the search_db request path
/// (realistic rps collapsed 5136 → 32–186) and grew RSS to ~3.5 GB vs
/// iter-12's 1147 MB. Only shapes whose LAST successful build cost at least
/// this many ms are worth pre-warming; cheap shapes rebuild lazily for free.
/// Overridable per-process via `AppState::prewarm_min_cost_ms`
/// (`SCRYBOX_PREWARM_MIN_COST_MS`).
pub(crate) const PREWARM_MIN_COST_MS_DEFAULT: u64 = 40;
/// Per-generation estimate-time budget (iter 15): a settled revision fans out
/// at most this much estimated build time (summed from last-build costs),
/// most-recently-requested first. Bounds the blast radius when many expensive
/// shapes are all recent at once.
const PREWARM_GEN_BUDGET_MS: u64 = 2_000;
/// Recent entries older than this are dropped when the reaper runs.
const RECENT_WINDOW: Duration = Duration::from_secs(120);

/// Cache validity key. `coll` is `collection_read_revision` (bumped by
/// collection/card_cache triggers); `deck` is `deck_list_revision` (bumped
/// by decks/deck_cards triggers) and is only meaningful for shapes with
/// `needs_alloc()` — every other shape stores and compares `NO_DECK` so a
/// deck edit never invalidates plain pages (review 2026-11-13 C1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevPair {
    coll: i64,
    deck: i64,
}

/// Deck-revision value for shapes that do not embed checked_out_qty.
pub const NO_DECK: i64 = 0;
/// Deck-revision value when the counter row is missing on an alloc shape —
/// distinct from every real revision so the entry self-heals once the
/// counter exists (mirrors rev_i = -1 for a missing collection counter).
pub const DECK_MISSING: i64 = i64::MIN;

impl RevPair {
    /// Validity key for one shape. Non-alloc shapes ignore the deck
    /// counter entirely; alloc shapes must rebuild when EITHER counter moves.
    pub fn for_shape(p: &CollectionParams, coll: Option<i64>, deck: Option<i64>) -> RevPair {
        RevPair {
            coll: coll.unwrap_or(-1),
            deck: if p.needs_alloc() { deck.unwrap_or(DECK_MISSING) } else { NO_DECK },
        }
    }
}

impl From<i64> for RevPair {
    /// Plain-collection-revision construction (non-alloc shapes, tests).
    fn from(coll: i64) -> Self {
        RevPair { coll, deck: NO_DECK }
    }
}

/// Single-flight slot key: cache key + validity pair (review 2026-11-13 H1,
/// same pattern as search.rs's `format!("{key}\u{0}{rev_i}")`). Without the
/// revision in the slot key, a build started at R1 owns the coalescing cell;
/// a request that already probed R2 calls `slot(key)`, gets handed the R1
/// cell by `get_or_try_init`, and can serve the stale R1 body under the
/// route ETag computed from R2 — sticky in the browser cache until the next
/// write. Keying the slot by the pair makes each revision a separate
/// coalescing group; stale in-flight builds simply lose the race.
fn slot_key(key: &str, rp: RevPair) -> String {
    format!("{key}\u{0}{}\u{0}{}", rp.coll, rp.deck)
}

pub struct CacheEntry {
    rev: i64,
    deck_rev: i64,
    raw: Bytes,
    total: Option<u64>,
    /// Express-default body ETag, computed once with the body.
    etag: String,
    /// gzip is filled lazily on the first gzip-demanding hit. Node pays for
    /// compression inside each response that asks for it; precomputing at
    /// build time charged ~300 ms per rebuild even on clients (and the bench)
    /// that never send Accept-Encoding.
    gz: OnceCell<Bytes>,
    /// Monotonic LRU clock value: stamped at `put`, refreshed on every cache
    /// hit in `get`. Eviction under `STORE_MAX_BYTES` drops the smallest.
    seen: AtomicU64,
}

impl CacheEntry {
    /// Heap held by this entry (raw + gz if it was ever built).
    fn bytes(&self) -> u64 {
        self.raw.len() as u64 + self.gz.get().map_or(0, |g| g.len() as u64)
    }
}

/// Single-flight slot, same shape as the stats cache (stats.rs): every
/// concurrent miss on one key shares a single query+shape+serialize. Without
/// it, a write-mix bench stampede runs duplicate ~26 MB builds per shape.
type ComputeSlot = OnceCell<Result<Arc<CacheEntry>, String>>;

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Arc<CacheEntry>>,
    inflight: HashMap<String, Arc<ComputeSlot>>,
    /// Shapes requested recently, with their parsed params — the candidate
    /// set for the background refresher (see `spawn_refresher`).
    recent: HashMap<String, (CollectionParams, Instant)>,
    /// Last successful build time (ms) per shape key — the pre-warm cost
    /// gate's input (iter 15). Absent = never built, so never pre-warmed.
    cost: HashMap<String, u64>,
    /// Per-key backoff after a failed pre-build (refresher only).
    cooldown: HashMap<String, Instant>,
}

/// Serialized responses keyed by (params, collection_read_revision).
/// Node's write triggers bump the revision, which invalidates every entry.
pub struct CollectionCache {
    inner: Mutex<Inner>,
    /// Number of full bodies actually computed (single-flight test + ops).
    computes: AtomicU64,
    /// Cross-key cap on concurrent full builds (a full shape is ~26 MB raw;
    /// unbounded builds under stampede drove RSS to ~4 GB in iter 4's
    /// realistic run).
    builds: Arc<Semaphore>,
    /// Monotonic LRU clock stamped into `CacheEntry::seen` (a clock, not a
    /// wall clock: tests get deterministic eviction order).
    clock: AtomicU64,
    /// Byte budget for `Inner::entries` bodies; settable in tests.
    max_bytes: AtomicU64,
    /// Freed-bytes threshold that arms `trim_dirty`; settable in tests.
    trim_dirty_bytes: AtomicU64,
    /// True once a `put` freed at least `trim_dirty_bytes` and the heap has
    /// not been returned yet (the refresher tick swaps it off).
    trim_dirty: AtomicBool,
    /// Highest revision any entry was built for. Entries from older
    /// generations are dead weight (`get` filters by exact rev) and get
    /// pruned on the next `put`.
    max_rev: AtomicI64,
}

impl Default for CollectionCache {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            computes: AtomicU64::new(0),
            builds: Arc::new(Semaphore::new(2)),
            clock: AtomicU64::new(0),
            max_bytes: AtomicU64::new(store_max_bytes()),
            trim_dirty_bytes: AtomicU64::new(TRIM_DIRTY_BYTES),
            trim_dirty: AtomicBool::new(false),
            max_rev: AtomicI64::new(i64::MIN),
        }
    }
}

impl CollectionCache {
    pub fn new() -> Self {
        Self::default()
    }
    fn get(&self, key: &str, rev: impl Into<RevPair>) -> Option<Arc<CacheEntry>> {
        let rp = rev.into();
        let e = self.inner.lock().unwrap();
        let hit = e
            .entries
            .get(key)
            .filter(|e| e.rev == rp.coll && e.deck_rev == rp.deck)
            .cloned();
        if let Some(entry) = &hit {
            // Serve = most-recently-used; the byte-budget eviction reads
            // this clock to pick its victim.
            entry.seen.store(self.clock.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
        }
        hit
    }
    /// LRU-evict until stored bytes (raw + any gz copy) fit `max_bytes`;
    /// never evicts the last entry. Returns bytes freed.
    fn evict_over_budget(&self, e: &mut Inner) -> u64 {
        let mut freed: u64 = 0;
        let budget = self.max_bytes.load(Ordering::Relaxed);
        let mut total_bytes: u64 = e.entries.values().map(|v| v.bytes()).sum();
        while total_bytes > budget && e.entries.len() > 1 {
            let victim = e
                .entries
                .iter()
                .min_by_key(|(_, v)| v.seen.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    if let Some(v) = e.entries.remove(&k) {
                        total_bytes -= v.bytes();
                        freed += v.bytes();
                    }
                }
                None => break,
            }
        }
        freed
    }
    /// Review M3: the gz copy is filled lazily after `put`'s budget check, so
    /// re-run eviction once it lands to keep raw+gz inside `max_bytes`.
    fn account_encoded(&self) {
        let freed = {
            let mut e = self.inner.lock().unwrap();
            self.evict_over_budget(&mut e)
        };
        if freed >= self.trim_dirty_bytes.load(Ordering::Relaxed) && freed > 0 {
            self.trim_dirty.store(true, Ordering::Relaxed);
        }
    }
    /// Store a freshly built entry and return it. `cost` is the measured
    /// build time (permit acquired → body complete), recorded for the
    /// pre-warm cost gate (iter 15).
    ///
    /// Memory hygiene (iter 16), all under the entries lock:
    /// - bodies from superseded generations are dropped (`get` filters by
    ///   exact rev, so they are pure RSS weight);
    /// - total stored bytes are held under `max_bytes` by evicting the
    ///   least-recently-served entries first;
    /// - a big enough drop arms `trim_dirty` so the refresher tick returns
    ///   the freed heap to the OS off the request path (glibc keeps large
    ///   freed blocks in its arenas otherwise).
    fn put(&self, key: String, rev: impl Into<RevPair>, raw: Vec<u8>, total: Option<u64>, cost: Duration) -> Arc<CacheEntry> {
        let rp = rev.into();
        let etag = body_etag(&raw);
        let seen = self.clock.fetch_add(1, Ordering::Relaxed);
        let entry = Arc::new(CacheEntry {
            rev: rp.coll,
            deck_rev: rp.deck,
            raw: Bytes::from(raw),
            total,
            etag,
            gz: OnceCell::new(),
            seen: AtomicU64::new(seen),
        });
        self.max_rev.fetch_max(rp.coll, Ordering::Relaxed);
        let live_rev = self.max_rev.load(Ordering::Relaxed);
        let mut freed: u64 = 0;
        {
            let mut e = self.inner.lock().unwrap();
            if e.entries.len() > 64 {
                freed += e.entries.values().map(|v| v.bytes()).sum::<u64>();
                e.entries.clear();
            }
            e.cost.insert(key.clone(), cost.as_millis() as u64);
            e.entries.insert(key, entry.clone());
            e.entries.retain(|_, v| {
                if v.rev != live_rev {
                    freed += v.bytes();
                    false
                } else {
                    true
                }
            });
            freed += self.evict_over_budget(&mut e);
        }
        if freed >= self.trim_dirty_bytes.load(Ordering::Relaxed) {
            self.trim_dirty.store(true, Ordering::Relaxed);
        }
        self.computes.fetch_add(1, Ordering::Relaxed);
        entry
    }
    /// Swap off the "heap was freed" flag (refresher tick → malloc_trim).
    fn take_trim(&self) -> bool {
        self.trim_dirty.swap(false, Ordering::Relaxed)
    }
    /// The coalescing slot for a key (created on first miss).
    fn slot(&self, key: &str) -> Arc<ComputeSlot> {
        self.inner.lock().unwrap().inflight.entry(key.to_string()).or_default().clone()
    }
    /// Drop a finished slot so the NEXT miss starts a fresh computation.
    fn finish(&self, key: &str) {
        self.inner.lock().unwrap().inflight.remove(key);
    }
    /// Record a request for this params shape (refresher candidate).
    fn touch(&self, p: &CollectionParams) {
        let key = p.cache_key();
        let mut e = self.inner.lock().unwrap();
        if e.recent.len() > RECENT_MAX && !e.recent.contains_key(&key) {
            e.recent.retain(|_, (_, seen)| seen.elapsed() < RECENT_WINDOW);
            e.cooldown.retain(|_, t| t.elapsed() < REFRESH_COOLDOWN);
            let alive: std::collections::HashSet<String> = e.recent.keys().cloned().collect();
            e.cost.retain(|k, _| alive.contains(k));
        }
        e.recent.insert(key, (p.clone(), Instant::now()));
    }
    /// The refresher's fan-out set for one generation (iter 15): recent
    /// shapes that (a) were built at least once, (b) whose last build cost
    /// ≥ `min_cost_ms` (cheap shapes are cheaper to rebuild lazily than to
    /// pre-build on every bump), taken most-recently-requested first until
    /// `budget_ms` of estimated build time is spent. Never-built shapes are
    /// excluded — the request path pays their first build and records the
    /// cost for a later generation.
    fn prewarm_keys(&self, window: Duration, min_cost_ms: u64, budget_ms: u64) -> Vec<(String, CollectionParams)> {
        let e = self.inner.lock().unwrap();
        let mut v: Vec<_> = e
            .recent
            .iter()
            .filter(|(_, (_, seen))| seen.elapsed() < window)
            .filter_map(|(k, (p, seen))| e.cost.get(k).map(|c| (k.clone(), p.clone(), *seen, *c)))
            .collect();
        v.sort_by_key(|(_, _, seen, _)| std::cmp::Reverse(*seen));
        let mut out = Vec::new();
        let mut spent: u64 = 0;
        for (k, p, _, c) in v {
            if c < min_cost_ms {
                continue;
            }
            if !out.is_empty() && spent + c > budget_ms {
                break;
            }
            spent += c;
            out.push((k, p));
        }
        out
    }
    /// True when the stored entry for `key` already carries the current rev.
    fn is_current(&self, key: &str, rev: impl Into<RevPair>) -> bool {
        let rp = rev.into();
        let e = self.inner.lock().unwrap();
        e.entries
            .get(key)
            .map_or(false, |e| e.rev == rp.coll && e.deck_rev == rp.deck)
    }
    /// A build already in flight for this key (request-path or refresher) —
    /// the poller skips it this tick; the slot's owner will land the body.
    fn inflight_contains(&self, key: &str) -> bool {
        self.inner.lock().unwrap().inflight.contains_key(key)
    }
    fn on_cooldown(&self, key: &str) -> bool {
        let e = self.inner.lock().unwrap();
        match e.cooldown.get(key) {
            Some(t) => t.elapsed() < REFRESH_COOLDOWN,
            None => false,
        }
    }
    fn refresh_failed(&self, key: &str) {
        self.inner.lock().unwrap().cooldown.insert(key.to_string(), Instant::now());
    }
    /// Observability for the single-flight test and ops dashboards.
    pub fn computes(&self) -> u64 {
        self.computes.load(Ordering::Relaxed)
    }
    async fn build_permit(&self) -> tokio::sync::OwnedSemaphorePermit {
        // acquire() only fails on close(), which never happens here.
        Arc::clone(&self.builds).acquire_owned().await.expect("build semaphore")
    }
}

/// glibc holds freed multi-MB blocks in its arenas (dynamic mmap threshold
/// climbs to 32 MB, and `free` only trims the heap top), so a run that
/// repeatedly rebuilds the ~27 MB full-collection body ratchets RSS upward
/// even when the cache map holds only a few live bodies (iter-15: ~3 GB
/// against a live set well under 1 GB). Two levers, set once at boot:
/// allocations ≥ 64 MB always go through mmap (munmapped on free — covers
/// gz members concatenated and future bigger bodies), and the trim threshold
/// is pinned so `malloc_trim` can return the rest.
pub(crate) fn tune_allocator() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 64 << 20);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 1 << 20);
    }
}

/// Return free heap pages to the OS. Runs on the refresher timer only
/// (never a serving thread); armed by `put` freeing ≥ `TRIM_DIRTY_BYTES`.
pub(crate) fn trim_heap() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Background pre-warm (iter 14, owner hint): poll the `collection_read_revision`
/// counter and, after a bump settles, pre-build the current body for every
/// recently-requested shape through the SAME single-flight slot and build
/// semaphore as the request path. Requests landing after an invalidation are
/// then answered from the cache instead of waiting on a ~400 ms (full shape)
/// blocking build. Bodies are produced by the identical `build_body_parallel`
/// path, so this cannot change any response — strict parity is preserved
/// (unlike the stats route's bounded-SWR exception, this route needs none).
///
/// Costs are bounded: only shapes requested within REFRESH_ACTIVE, only
/// shapes whose last build cost ≥ the cost gate (cheap shapes rebuild
/// lazily — unbudgeted fan-out of 50+ cheap shapes per write-mix tick
/// starved the request path and ballooned RSS, iter-15 fix), at most
/// PREWARM_GEN_BUDGET_MS of estimated build time per settled revision, only
/// after the revision has been observed stable for REFRESH_DEBOUNCE_TICKS
/// ticks (edit+revert write-mix pairs must not fan out builds for
/// intermediate states), a failed pre-build backs off per key for
/// REFRESH_COOLDOWN, and concurrent builds never exceed the cross-key
/// semaphore the request path already enforces.
pub fn spawn_refresher(state: &AppState) {
    let Some(pool) = state.db.clone() else { return };
    let cache = Arc::clone(&state.collection_cache);
    let min_cost_ms = state.prewarm_min_cost_ms;
    tune_allocator();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(REFRESH_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_observed: Option<(i64, i64)> = None;
        let mut stable_ticks: u8 = 0;
        let mut built_rev: Option<(i64, i64)> = None;
        loop {
            tick.tick().await;
            // Heap hygiene: a big prune/eviction above arms the flag; the
            // trim runs here on the timer, never on a serving thread.
            if cache.take_trim() {
                trim_heap();
            }
            let active = cache.prewarm_keys(REFRESH_ACTIVE, min_cost_ms, PREWARM_GEN_BUDGET_MS);
            if active.is_empty() {
                continue;
            }
            // One revision read per tick (both counters on one connection),
            // kept off the runtime workers.
            let pool2 = Arc::clone(&pool);
            let (rev, deck_rev) = match tokio::task::spawn_blocking(move || {
                pool2.get().ok().map(|c| {
                    (
                        crate::db::current_revision(&c).ok().flatten(),
                        crate::db::current_deck_revision(&c).ok().flatten(),
                    )
                })
            })
            .await
            {
                Ok(Some(pair)) => pair,
                Ok(None) => (None, None),
                Err(_) => return, // runtime shutting down
            };
            let Some(rev) = rev else {
                last_observed = None;
                stable_ticks = 0;
                continue;
            };
            let observed = (rev, deck_rev.unwrap_or(DECK_MISSING));
            if built_rev == Some(observed) {
                continue;
            }
            if last_observed == Some(observed) {
                stable_ticks = stable_ticks.saturating_add(1);
            } else {
                last_observed = Some(observed);
                stable_ticks = 1;
            }
            if stable_ticks < REFRESH_DEBOUNCE_TICKS {
                continue;
            }
            let mut retry_next_tick = false;
            for (key, p) in active {
                let rp = RevPair::for_shape(&p, Some(rev), deck_rev);
                if cache.is_current(&key, rp) || cache.on_cooldown(&key) {
                    continue;
                }
                let skey = slot_key(&key, rp);
                if cache.inflight_contains(&skey) {
                    // A build for THIS revision is already in flight (request
                    // path or an earlier tick); it lands shortly and the
                    // is_current check skips it next tick (or we fire then).
                    // Old-revision in-flights have their own slot key and no
                    // longer mask this one (H1).
                    retry_next_tick = true;
                    continue;
                }
                let slot = cache.slot(&skey);
                let cache2 = Arc::clone(&cache);
                let pool3 = Arc::clone(&pool);
                let key2 = key.clone();
                let skey2 = skey.clone();
                tokio::spawn(async move {
                    let init_cache = Arc::clone(&cache2);
                    let init_pool = Arc::clone(&pool3);
                    let init_key = key2.clone();
                    let init = move || async move {
                        let _permit = init_cache.build_permit().await;
                        let t0 = Instant::now();
                        let r = tokio::task::spawn_blocking(move || -> Result<Arc<CacheEntry>, String> {
                            let (body, total) = build_body_parallel(&init_pool, &p)?;
                            Ok(init_cache.put(init_key, rp, body, total, t0.elapsed()))
                        })
                        .await
                        .map_err(|e| format!("collection refresh join: {e}"))?;
                        Ok(r)
                    };
                    // Shared with any concurrent request-path miss on the
                    // same key: whoever gets here first computes, everyone
                    // else awaits the same OnceCell. (Type annotation pins
                    // the nested Result like the request-path call below.)
                    let res: Result<&Result<Arc<CacheEntry>, String>, String> =
                        slot.get_or_try_init(init).await;
                    match res {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "collection refresher build failed");
                            cache2.refresh_failed(&key2);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "collection refresher build failed");
                            cache2.refresh_failed(&key2);
                        }
                    }
                    cache2.finish(&skey2);
                });
            }
            if !retry_next_tick {
                built_rev = Some(observed);
            }
        }
    });
}

pub(crate) fn gzip_bytes(raw: &[u8]) -> Bytes {
    Bytes::from(gzip_at(raw, 6))
}

fn gzip_at(raw: &[u8], level: u32) -> Vec<u8> {
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::new(level));
    enc.write_all(raw).expect("vec write");
    enc.finish().expect("gzip")
}

/// Multi-member gzip (the pigz technique): independently compressed
/// members concatenated. Any conformant gunzip (browsers, curl, undici)
/// transparently decompresses the concatenation, and the members compress
/// in parallel — 26 MB of full-collection JSON drops from ~310 ms to ~150 ms
/// on 4 cores while keeping the same deflate level (6) as Node's zlib default.
fn gzip_members(raw: &[u8], level: u32) -> Vec<u8> {
    const MEMBER: usize = 4 << 20; // 4 MB per member
    if raw.len() <= MEMBER {
        return gzip_at(raw, level);
    }
    let nthreads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    let chunks: Vec<&[u8]> = if raw.len() / nthreads >= MEMBER {
        raw.chunks(MEMBER).collect()
    } else {
        raw.chunks(raw.len().div_ceil(nthreads)).collect()
    };
    let members: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = chunks.iter().map(|c| s.spawn(move || gzip_at(c, level))).collect();
        handles.into_iter().map(|h| h.join().expect("gzip member thread")).collect()
    });
    let total: usize = members.iter().map(|m| m.len()).sum();
    let mut out = Vec::with_capacity(total);
    for m in members {
        out.extend_from_slice(&m);
    }
    out
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
    let headers = req.headers();
    let uri = req.uri().clone();

    // authenticate (REVIEW M5: async checkout — idle pop on the worker,
    // blocking-pool hop when none is idle; never pins the worker).
    let token = auth::bearer_token(headers).map(|s| s.to_string());
    let user = match auth::authenticate_async(&pool, token.as_deref(), true).await {
        Ok(u) => u,
        Err(res) => return res,
    };

    let raw_query = uri.query().map(|s| s.to_string());
    let p = parse_params(user.id, raw_query.as_deref());
    let inm = headers.get("if-none-match").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).map(|s| s.to_string());

    // Revision probe: single indexed read; also the cache invalidator.
    // Alloc shapes (full, include_allocation=1) additionally probe the deck
    // counter on the SAME connection — their checked_out_qty comes from
    // decks/deck_cards, which bump deck_list_revision only (C1).
    // REVIEW M5: async checkout so the probe never parks the worker.
    let (rev, deck_rev) = match pool.get_async().await {
        Ok(c) => (
            crate::db::current_revision(&c).ok().flatten(),
            p.needs_alloc()
                .then(|| crate::db::current_deck_revision(&c).ok().flatten())
                .flatten(),
        ),
        Err(_) => (None, None),
    };

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
    // Refresher candidate bookkeeping (see `spawn_refresher`).
    st.collection_cache.touch(&p);
    let rp = RevPair::for_shape(&p, rev, deck_rev);
    let entry: Arc<CacheEntry> = match st.collection_cache.get(&key, rp) {
        Some(e) => e,
        None => {
            // Coalesced regeneration (stats.rs pattern): every concurrent
            // miss on this key awaits ONE build_body_parallel through a
            // shared OnceCell slot, gated by the cross-key build semaphore.
            let cache = Arc::clone(&st.collection_cache);
            let skey = slot_key(&key, rp);
            let slot = cache.slot(&skey);
            let init_cache = Arc::clone(&cache);
            let init_pool = Arc::clone(&pool);
            let init_key = key.clone();
            let init_p = p.clone();
            let res: Result<&Result<Arc<CacheEntry>, String>, String> = slot
                .get_or_try_init(move || async move {
                    let _permit = init_cache.build_permit().await;
                    let t0 = Instant::now();
                    let r = tokio::task::spawn_blocking(move || -> Result<Arc<CacheEntry>, String> {
                        let (body, total) = build_body_parallel(&init_pool, &init_p)?;
                        Ok(init_cache.put(init_key, rp, body, total, t0.elapsed()))
                    })
                    .await
                    .map_err(|e| format!("collection build task panicked: {e}"))?;
                    Ok(r)
                })
                .await;
            cache.finish(&skey);
            // get_or_try_init nests: outer Err carries a clone of the init
            // error, outer Ok wraps the stored Result<Arc<CacheEntry>, String>.
            match res {
                Ok(Ok(e)) => e.clone(),
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "collection build failed");
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch collection");
                }
                Err(e) => {
                    tracing::error!(error = %e, "collection build failed");
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
        // Compressed on first demand, then shared by every later hit; members
        // compress in parallel (see gzip_members).
        let gz = entry
            .gz
            .get_or_try_init(|| {
                let raw = entry.raw.clone();
                async move {
                    tokio::task::spawn_blocking(move || Ok::<_, std::io::Error>(Bytes::from(gzip_members(
                        &raw, 6,
                    ))))
                    .await
                    .map_err(|e| {
                        std::io::Error::other(format!("gzip task panicked: {e}"))
                    })?
                }
            })
            .await;
        if gz.is_ok() {
            st.collection_cache.account_encoded();
        }
        match gz {
            Ok(gz) => (gz.clone(), Some("gzip")),
            Err(e) => {
                tracing::error!(error = %e, "collection gzip failed; serving raw");
                (entry.raw.clone(), None)
            }
        }
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

#[cfg(test)]
mod perf_probe {
    //! Ignored stage-timing probe for the full (unpaginated) collection build
    //! against the real DB copy: `SCRYBOX_DB_PATH=... cargo test --lib --
    //! --ignored --nocapture perf_probe`. Not part of `cargo test` default run.
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore = "perf probe: needs SCRYBOX_DB_PATH pointing at the prod DB copy"]
    fn full_build_stage_timings() {
        let Ok(path) = std::env::var("SCRYBOX_DB_PATH") else {
            eprintln!("skip: SCRYBOX_DB_PATH unset");
            return;
        };
        let pool = DbPool::open(&path, 4).expect("open pool");
        let user_id = std::env::var("SCRYBOX_PROBE_USER").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
        let p = CollectionParams {
            user_id,
            is_trade: None,
            card_id: None,
            paginated: false,
            limit: 100,
            offset: 0,
            alloc: false,
            count_zero: false,
            qs: String::new(),
        };
        for round in 0..3 {
            let t0 = Instant::now();
            let (vals, _total) = build_values(&pool, &p).unwrap();
            let t1 = Instant::now();
            let body = serde_json::to_vec(&vals).unwrap();
            let t2 = Instant::now();
            let _etag = body_etag(&body);
            let t3 = Instant::now();
            let gz6 = gzip_at(&body, 6);
            let t4 = Instant::now();
            let gz3 = gzip_at(&body, 3);
            let t5 = Instant::now();
            let gz1 = gzip_at(&body, 1);
            let t6 = Instant::now();
            let par = {
                let pool = Arc::new(DbPool::open(&path, 4).unwrap());
                let (b, _t) = build_body_parallel(&pool, &p).unwrap();
                assert_eq!(b, body, "parallel build must be byte-identical");
                b.len()
            };
            let t7 = Instant::now();
            println!(
                "round {round}: rows={} raw={}MB | values={:?} to_vec={:?} etag={:?} gz6={:?}({}MB) gz3={:?}({}MB) gz1={:?}({}MB) total={:?} | parallel_build={:?}",
                vals.len(),
                body.len() / 1_048_576,
                t1 - t0,
                t2 - t1,
                t3 - t2,
                t4 - t3,
                gz6.len() / 1_048_576,
                t5 - t4,
                gz3.len() / 1_048_576,
                t6 - t5,
                gz1.len() / 1_048_576,
                t6 - t0,
                t7 - t6,
            );
            let _ = par;
        }
    }

    #[test]
    #[ignore = "perf probe: needs SCRYBOX_DB_PATH pointing at the prod DB copy"]
    fn full_build_substage_timings() {
        let Ok(path) = std::env::var("SCRYBOX_DB_PATH") else { eprintln!("skip"); return };
        let pool = DbPool::open(&path, 8).expect("open pool");
        let user_id = std::env::var("SCRYBOX_PROBE_USER").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
        let p = CollectionParams { user_id, is_trade: None, card_id: None, paginated: false,
            limit: 100, offset: 0, alloc: false, count_zero: false, qs: String::new() };
        for round in 0..3 {
            let t0 = Instant::now();
            let (filter_sql, params) = filter_clause(&p);
            let conn = pool.get().unwrap();
            let mut sql = format!("{SELECT_BODY}\n      {filter_sql}\n      ORDER BY c.added_at DESC, c.id DESC");
            if p.paginated { sql.push_str("\n      LIMIT ? OFFSET ?"); }
            let mut stmt = conn.prepare_cached(&sql).unwrap();
            let mut raw_rows: Vec<Vec<Value>> = Vec::new();
            {
                let mut rows = stmt.query(rusqlite::params_from_iter(params.iter())).unwrap();
                while let Some(row) = rows.next().unwrap() {
                    let mut raw = Vec::with_capacity(ALL_COLS.len());
                    for i in 0..ALL_COLS.len() { raw.push(sqlite_value(row, i).unwrap()); }
                    raw_rows.push(raw);
                }
            }
            drop(stmt);
            let t1 = Instant::now();
            let _alloc = checked_out_allocation(&conn, p.user_id).unwrap();
            let t2 = Instant::now();
            // shape sequentially (copy of build_values loop)
            let idx = |name: &str| ALL_COLS.iter().position(|c| *c == name).unwrap();
            let shape_row = |raw: &Vec<Value>| -> Value {
                let is_proxy = &raw[idx("is_proxy")];
                let printing = &raw[idx("printing")];
                let rarity_raw = &raw[idx("rarity")];
                let mut obj = Map::with_capacity(ALL_COLS.len() + 2);
                for (i, name) in ALL_COLS.iter().enumerate() {
                    let v = match *name {
                        "is_proxy" => Value::from(if truthy(is_proxy) { 1 } else { 0 }),
                        "rarity" => if truthy(is_proxy) { Value::String("Proxy".into()) } else { rarity_raw.clone() },
                        "subtypes" | "types" | "color_identity" => parse_array_col(&raw[i]).unwrap(),
                        "price_trend" => resolve_card_price(is_proxy, printing, &raw[idx("price_trend")], &raw[idx("price_normal")], &raw[idx("price_holofoil")]),
                        _ => raw[i].clone(),
                    };
                    obj.insert((*name).to_string(), v);
                }
                obj.insert("base_rarity".to_string(), rarity_raw.clone());
                Value::Object(obj)
            };
            let vals: Vec<Value> = raw_rows.iter().map(shape_row).collect();
            let t3 = Instant::now();
            // parallel shape+serialize via thread::scope chunks
            let nthreads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(4);
            let chunk = raw_rows.len().div_ceil(nthreads);
            let parts: Vec<Vec<u8>> = std::thread::scope(|s| {
                let handles: Vec<_> = raw_rows.chunks(chunk).map(|chunk_rows| {
                    s.spawn(move || chunk_rows.iter().map(shape_row).map(|v| serde_json::to_vec(&v).unwrap()).collect::<Vec<_>>())
                }).collect();
                handles.into_iter().map(|h| h.join().unwrap()).flatten().collect()
            });
            let t4 = Instant::now();
            let mut par_body = Vec::new();
            par_body.push(b'[');
            for (i, part) in parts.iter().enumerate() {
                if i > 0 { par_body.push(b','); }
                par_body.extend_from_slice(part);
            }
            par_body.push(b']');
            let par_bytes = par_body;
            let seq_body = serde_json::to_vec(&vals).unwrap();
            let t5 = Instant::now();
            // parallel gzip members over seq_body
            let gz_par = {
                let chunk = seq_body.len().div_ceil(nthreads);
                let members: Vec<Vec<u8>> = std::thread::scope(|s| {
                    let handles: Vec<_> = seq_body.chunks(chunk).map(|c| s.spawn(move || gzip_at(c, 6))).collect();
                    handles.into_iter().map(|h| h.join().unwrap()).collect()
                });
                members.concat()
            };
            let t6 = Instant::now();
            assert_eq!(seq_body, par_bytes, "parallel body must be byte-identical");
            println!(
                "round {round}: raw_rows={:?} alloc={:?} shape_seq={:?} par_shape_tovec={:?} stitch_ser={:?} gzmulti={:?} (gz {}MB -> {}MB)",
                t1 - t0, t2 - t1, t3 - t2, t4 - t3, t5 - t4, t6 - t5,
                seq_body.len() / 1_048_576, gz_par.len() / 1_048_576,
            );
        }
    }
}

#[cfg(test)]
mod prewarm_gate {
    //! Iter-15 cost gate + per-generation budget for the refresher fan-out
    //! (unbudgeted fan-out of cheap shapes starved the request path).
    use super::*;

    fn params(offset: i64) -> CollectionParams {
        CollectionParams {
            user_id: 1,
            is_trade: None,
            card_id: None,
            paginated: true,
            limit: 100,
            offset,
            alloc: false,
            count_zero: false,
            qs: String::new(),
        }
    }

    fn seed(cache: &CollectionCache, p: &CollectionParams, cost_ms: u64) {
        cache.touch(p);
        let _ = cache.put(p.cache_key(), 1, b"{}".to_vec(), None, Duration::from_millis(cost_ms));
    }

    #[test]
    fn cheap_and_never_built_shapes_never_fan_out() {
        let cache = CollectionCache::new();
        seed(&cache, &params(0), 2); // below the 40 ms gate
        cache.touch(&params(100)); // touched but never built
        let keys = cache.prewarm_keys(REFRESH_ACTIVE, PREWARM_MIN_COST_MS_DEFAULT, PREWARM_GEN_BUDGET_MS);
        assert!(keys.is_empty(), "cheap or never-built shapes must not fan out: {keys:?}");
    }

    #[test]
    fn expensive_shapes_fan_out_mru_first_until_budget_exhausts() {
        let cache = CollectionCache::new();
        // 2000 ms budget vs three 900 ms shapes: the third must not fit.
        seed(&cache, &params(0), 900);
        seed(&cache, &params(100), 900);
        seed(&cache, &params(200), 900);
        seed(&cache, &params(300), 5); // cheapest but freshest: must be skipped
        let keys = cache.prewarm_keys(REFRESH_ACTIVE, PREWARM_MIN_COST_MS_DEFAULT, PREWARM_GEN_BUDGET_MS);
        assert_eq!(keys.len(), 2, "budget must cut the third 900 ms shape: {keys:?}");
        assert_eq!(keys[0].1.offset, 200, "fan-out must be MRU-first");
        assert_eq!(keys[1].1.offset, 100);
    }
}

#[cfg(test)]
mod chunk_arena {
    //! Iter-18 chunk arena: pooled serialize buffers for `build_body_parallel`.

    use super::*;

    #[test]
    fn pooled_buffers_are_reused_empty_and_capped() {
        // The arena relies on `to_writer(&mut Vec<u8>)` producing the exact
        // bytes `to_vec` produces (the sequential oracle uses `to_vec`).
        let vs = vec![
            serde_json::json!(1.5),
            serde_json::json!(-0.125),
            serde_json::json!(1234.5678),
            serde_json::json!({"a": [1, null, true], "b": 0.0, "c": -0.0}),
            serde_json::json!("Ünïcode & \"quoted\" <c> — 東京"),
        ];
        for v in &vs {
            let mut buf = Vec::new();
            serde_json::to_writer(&mut buf, v).unwrap();
            assert_eq!(buf, serde_json::to_vec(v).unwrap(), "to_writer bytes must match to_vec");
        }
        // Returned buffers come back cleared with capacity intact (reuse),
        // best-fit preferring the smallest sufficient buffer.
        let big = take_chunk(1 << 20);
        let cap = big.capacity();
        let mut big = big;
        big.extend_from_slice(b"dirt-dirt-dirt");
        give_chunk(big);
        let reused = take_chunk(1024);
        assert!(reused.is_empty(), "returned buffers arrive cleared");
        assert!(reused.capacity() >= cap, "best-fit must hand back the pooled capacity");
        give_chunk(reused);
        // Retention stops at the buffer cap; extras drop to the allocator.
        for _ in 0..(CHUNK_POOL_MAX_BUFFERS + 4) {
            give_chunk(Vec::with_capacity(64));
        }
        assert!(
            chunk_pool_len() <= CHUNK_POOL_MAX_BUFFERS,
            "pool must stop at the buffer cap: {} > {}",
            chunk_pool_len(),
            CHUNK_POOL_MAX_BUFFERS
        );
    }
}

#[cfg(test)]
mod store_budget {
    //! Iter-16 store RSS hygiene: superseded-generation bodies are dropped
    //! on the next put, stored bytes are held under a byte budget by
    //! least-recently-served eviction, and a big drop arms the deferred
    //! malloc_trim for the refresher tick.

    use super::*;

    fn params(offset: i64) -> CollectionParams {
        CollectionParams {
            user_id: 1,
            is_trade: None,
            card_id: None,
            paginated: true,
            limit: 100,
            offset,
            alloc: false,
            count_zero: false,
            qs: String::new(),
        }
    }

    #[test]
    fn superseded_generation_bodies_are_dropped() {
        let cache = CollectionCache::new();
        // Two bodies from generation 1.
        cache.put(params(0).cache_key(), 1, vec![1u8; 1000], None, Duration::from_millis(50));
        cache.put(params(100).cache_key(), 1, vec![2u8; 1000], None, Duration::from_millis(50));
        assert_eq!(cache.inner.lock().unwrap().entries.len(), 2);
        // A generation-2 body lands: the generation-1 bodies are dead weight
        // (get filters by exact rev) and must be dropped, not just shadowed.
        cache.put(params(200).cache_key(), 2, vec![3u8; 32], None, Duration::from_millis(50));
        let inner = cache.inner.lock().unwrap();
        assert_eq!(inner.entries.len(), 1, "superseded-generation bodies must be dropped");
        assert_eq!(inner.entries.values().next().unwrap().rev, 2);
    }

    #[test]
    fn late_stale_put_does_not_resurrect_old_generation() {
        let cache = CollectionCache::new();
        cache.put(params(0).cache_key(), 2, vec![1u8; 64], None, Duration::from_millis(50));
        // A slow generation-1 build lands after generation 2 settled: its
        // body is served to its own requester (returned Arc) but never
        // stored.
        let stale =
            cache.put(params(100).cache_key(), 1, vec![2u8; 1000], None, Duration::from_millis(50));
        assert_eq!(stale.rev, 1, "the caller still gets its own body");
        let inner = cache.inner.lock().unwrap();
        assert_eq!(inner.entries.len(), 1);
        assert_eq!(inner.entries.values().next().unwrap().rev, 2);
    }

    #[test]
    fn byte_budget_evicts_least_recently_served_first() {
        let cache = CollectionCache::new();
        // Budget fits exactly three 1000-byte bodies; the fourth forces one
        // eviction and the LRU clock picks the victim.
        cache.max_bytes.store(3000, Ordering::Relaxed);
        let body = vec![9u8; 1000];
        cache.put(params(0).cache_key(), 1, body.clone(), None, Duration::from_millis(50));
        cache.put(params(100).cache_key(), 1, body.clone(), None, Duration::from_millis(50));
        cache.put(params(200).cache_key(), 1, body.clone(), None, Duration::from_millis(50));
        // Serving refreshes the LRU clock, so offset-0 must survive eviction
        // even though it was inserted first.
        assert!(cache.get(&params(0).cache_key(), 1).is_some());
        cache.put(params(300).cache_key(), 1, body, None, Duration::from_millis(50));
        let inner = cache.inner.lock().unwrap();
        let stored: usize = inner.entries.values().map(|e| e.raw.len()).sum();
        assert!(stored <= 3000, "stored bytes must fit the budget: {stored}");
        assert!(inner.entries.contains_key(&params(0).cache_key()), "recently served survives");
        assert!(inner.entries.contains_key(&params(300).cache_key()), "the new entry survives");
        assert!(
            !inner.entries.contains_key(&params(100).cache_key()),
            "oldest serve (unrefreshed) is the victim"
        );
    }

    #[test]
    fn big_evictions_arm_the_deferred_heap_trim() {
        let cache = CollectionCache::new();
        cache.max_bytes.store(2500, Ordering::Relaxed);
        cache.trim_dirty_bytes.store(1000, Ordering::Relaxed);
        let body = vec![7u8; 1000];
        cache.put(params(0).cache_key(), 1, body.clone(), None, Duration::from_millis(50));
        cache.put(params(100).cache_key(), 1, body.clone(), None, Duration::from_millis(50));
        assert!(!cache.take_trim(), "no big drop yet");
        // Third body forces one eviction (>= trim_dirty_bytes freed).
        cache.put(params(200).cache_key(), 1, body, None, Duration::from_millis(50));
        assert!(cache.take_trim(), "evicting >= trim_dirty_bytes must arm the trim");
        assert!(!cache.take_trim(), "take_trim swaps the flag off");
    }

    #[test]
    fn lazily_built_gz_copy_counts_toward_budget() {
        let cache = CollectionCache::new();
        cache.max_bytes.store(2500, Ordering::Relaxed);
        let a = cache.put(params(0).cache_key(), 1, vec![1u8; 1000], None, Duration::from_millis(50));
        cache.put(params(100).cache_key(), 1, vec![2u8; 1000], None, Duration::from_millis(50));
        // gz for offset-0 lands after put (as in the serve path): 2000 + 800 > 2500.
        a.gz.set(Bytes::from(vec![0u8; 800])).unwrap();
        cache.account_encoded();
        let inner = cache.inner.lock().unwrap();
        let stored: u64 = inner.entries.values().map(|e| e.bytes()).sum();
        assert!(stored <= 2500, "raw+gz must fit the budget: {stored}");
    }

    #[test]
    fn single_oversized_entry_is_never_self_evicted() {
        let cache = CollectionCache::new();
        cache.max_bytes.store(500, Ordering::Relaxed);
        cache.put(params(0).cache_key(), 1, vec![3u8; 4000], None, Duration::from_millis(50));
        let inner = cache.inner.lock().unwrap();
        assert!(
            inner.entries.contains_key(&params(0).cache_key()),
            "the only entry survives even over budget (hot full shape)"
        );
    }

    // --- review 2026-11-13 C1: deck counter keys allocation shapes only -----

    #[test]
    fn rev_pair_keys_deck_counter_only_for_alloc_shapes() {
        // Alloc shape: EITHER counter moving changes the validity key.
        let a = CollectionParams { alloc: true, ..params(0) };
        assert_ne!(
            RevPair::for_shape(&a, Some(1), Some(1)),
            RevPair::for_shape(&a, Some(1), Some(2)),
            "deck edit must invalidate allocation shapes (C1)"
        );
        assert_ne!(
            RevPair::for_shape(&a, Some(1), Some(1)),
            RevPair::for_shape(&a, Some(2), Some(1)),
        );
        // Missing deck counter is distinct from every real revision.
        assert_ne!(
            RevPair::for_shape(&a, Some(1), None),
            RevPair::for_shape(&a, Some(1), Some(0)),
        );
        // Plain paginated shape: deck counter ignored entirely.
        let page = params(0); // paginated=true, alloc=false
        assert_eq!(
            RevPair::for_shape(&page, Some(1), Some(1)),
            RevPair::for_shape(&page, Some(1), Some(999)),
            "deck edits must not invalidate plain pages"
        );
        // Full (unpaginated) shape needs alloc keying even without the flag.
        let full = CollectionParams { paginated: false, ..params(0) };
        assert_ne!(
            RevPair::for_shape(&full, Some(1), Some(1)),
            RevPair::for_shape(&full, Some(1), Some(2)),
        );
    }

    #[test]
    fn deck_revision_bump_misses_alloc_entries_only() {
        let cache = CollectionCache::new();
        let ka = CollectionParams { alloc: true, ..params(0) }.cache_key();
        let page = params(0);
        let kp = page.cache_key();
        cache.put(ka.clone(), RevPair { coll: 1, deck: 1 }, b"a".to_vec(), None, Duration::from_millis(50));
        // Stored exactly the way the handler stores a plain page: NO_DECK.
        cache.put(kp.clone(), RevPair::for_shape(&page, Some(1), Some(1)), b"p".to_vec(), None, Duration::from_millis(50));
        let a = CollectionParams { alloc: true, ..params(0) };
        assert!(
            cache.get(&ka, RevPair::for_shape(&a, Some(1), Some(2))).is_none(),
            "stale allocation body must not survive a deck edit"
        );
        assert!(
            cache.get(&kp, RevPair::for_shape(&page, Some(1), Some(2))).is_some(),
            "plain page survives deck traffic"
        );
        // Unmoved counter still hits.
        assert!(cache.get(&ka, RevPair::for_shape(&a, Some(1), Some(1))).is_some());
    }

    // --- review 2026-11-13 H1: single-flight slots keyed by revision --------

    #[test]
    fn in_flight_slot_is_keyed_by_revision() {
        let cache = CollectionCache::new();
        let p = params(0);
        let key = p.cache_key();
        let rp1 = RevPair::for_shape(&p, Some(1), Some(7));
        let rp2 = RevPair::for_shape(&p, Some(2), Some(7));
        let old_build = cache.slot(&slot_key(&key, rp1));
        // The refresher's guard: a build in flight for a DIFFERENT revision
        // must not mask this revision (checked before slot() creates the cell).
        assert!(
            !cache.inflight_contains(&slot_key(&key, rp2)),
            "old-revision in-flight must not mask a new-revision build (H1)"
        );
        assert!(cache.inflight_contains(&slot_key(&key, rp1)));
        // A write bumped coll 1->2 while the R1 build owns the coalescing
        // cell: the new-revision probe must NOT be handed the R1 cell by
        // get_or_try_init (that serves the R1 body under the R2 ETag).
        let fresh = cache.slot(&slot_key(&key, rp2));
        assert!(
            !Arc::ptr_eq(&old_build, &fresh),
            "stale-revision in-flight must not be shared with a new-revision request (H1)"
        );
        // Same revision still coalesces onto one cell.
        let same = cache.slot(&slot_key(&key, rp1));
        assert!(Arc::ptr_eq(&old_build, &same), "same-revision misses must coalesce");
    }
}

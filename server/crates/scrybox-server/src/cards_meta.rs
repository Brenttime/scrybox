//! LANE A — iter-39: cards-meta group (route switch `cards_meta`).
//! Port of the five small Scryfall-backed card routes:
//!
//!   POST /api/cards/highlight            (routes/stats.js:629)
//!   GET  /api/cards/:id/decks            (routes/stats.js:670)
//!   GET  /api/cards/:id/price-history    (routes/stats.js:695)
//!   GET  /api/stats/commander-of-the-day (routes/stats.js:775)
//!   GET  /api/suggest                    (routes/collection.js:238)
//!
//! All five sit behind the global `app.use('/api', authenticateToken)`
//! mount: handlers self-check group+method and proxy everything else.
//!
//! Sidecar legs. highlight and commander-of-the-day answer natively ONLY
//! in LOCAL_SCRYFALL mode (the Node twin's fetchWindow → scryPage walk
//! onto the scryfall-local mirror, duplicated page-for-page from
//! scryfallApi.js incl. scryGetRetried's 4×-on-429 retry). Upstream mode
//! — where the same calls go through the shared axios limiter queue, the
//! per-endpoint gaps and the 429 cooldown — replays the whole untouched
//! request to Node instead of modelling that queue a second time.
//! GET /api/suggest is sidecar-only by nature (Node short-circuits to
//! `{suggestions: []}` without LOCAL_SCRYFALL, so that gate is verbatim,
//! not a divergence).
//!
//! highlight and the `analyze` gate. The Node twin runs the user query
//! through shared/scryfallQuery.analyze purely to VALIDATE it (400 with
//! the QuerySyntaxError message on bad grammar). Modelling the whole
//! Scryfall grammar in Rust is out of scope (same ruling as collq's
//! free-text-only port): a query that survives the bare-word gate below
//! provably cannot make analyze throw (no punctuation, no /or|and/i
//! keywords — tokenize.js can only produce bare name leaves), so those
//! answer natively; ANYTHING else replays the whole POST to Node so its
//! own parser produces the exact 400.
//!
//! Error-shape twins (all re-read from the live routes):
//!  - highlight catch: status = err.status || err.response?.status →
//!    400/422 → 400 {error:'Request failed with status code N'} (the
//!    axios message; the syntax-regex never matches it, scryPage carries
//!    no engine messages); 404 → {names:[]}; everything else (5xx,
//!    429-exhausted, transport, timeout) → 502 {error:'Search engine
//!    unavailable'} — none of which surfaces the transport message, so
//!    those answer natively too.
//!  - commander-of-the-day: EVERY throw inside the one try → 502
//!    {error:'Commander of the day unavailable'} — including 404 (scryPage
//!    does not swallow it the way tryQuery does). total falsy → 503
//!    {error:'No commanders available'}; empty window → 503 {error:'No
//!    commander found'}.
//!  - price-history: parseSqliteUtc(NULL/'') → NaN; a SURVIVING collapsed
//!    point with NaN time throws inside res.json's data.map → 500
//!    {error:'Failed to retrieve price history'}; NaN times among
//!    collapsed-away points only poison spanDays (Math.max → NaN → null).
//!    windowDays: 30 for '30d', null for undefined, and the prototype-
//!    chain curiosities 'constructor' (function value → JSON.stringify
//!    DROPS the key) and '__proto__' (Object.prototype → {}), whose SQL
//!    window param is garbage → datetime() NULL → zero rows.
//!  - decks catch → 500 {error:'Could not load decks'}.
//!
//! JS-semantics twins reused, never re-implemented: collection_writes'
//! sql_is_basic_land_expr + decks_reads' sql_card_key (the utils/
//! cardIdentity twins already pinned by the decks group), stats::
//! parse_sqlite_utc + stats::iso_utc_ms (new Date(...).getTime() /
//! toISOString), collection's json_num/body_etag/etag_matches/
//! wants_encoding/gzip_bytes (Express default ETag + compression
//! middleware), lists_reads::qs_get (req.query), mfx_payload::js_truthy,
//! decks_reads::js_trim, sidecar::encode_uri_component.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};

use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::Response,
};
use http_body_util::BodyExt;
use serde_json::{Map, Value};

use crate::auth::{authenticate, bearer_token};
use crate::collection::{body_etag, etag_matches, gzip_bytes, json_num, wants_encoding};
use crate::db::DbPool;
use crate::decks_reads::{js_trim, sql_card_key};
use crate::lists_reads::{qs_get, QsVal};
use crate::mfx_payload::js_truthy;
use crate::sidecar::encode_uri_component;
use crate::stats::{iso_utc_ms, parse_sqlite_utc};
use crate::AppState;

/// scryfallApi.js SCRY_PAGE_SIZE.
const SCRY_PAGE_SIZE: i64 = 175;
/// stats.js HIGHLIGHT_CHUNK.
const HIGHLIGHT_CHUNK: usize = 100;
/// Transport budget for the sidecar pages: every failure shape maps to a
/// Node-observable answer that never surfaces the reason, so the exact ms
/// are invisible (Node's own axios timeout is 6000 ms).
const PAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Node's cotd cache cap (Map > 7 → delete oldest).
const COTD_CACHE_MAX: usize = 7;

// ---------------------------------------------------------------------------
// shared JS micro-semantics
// ---------------------------------------------------------------------------

/// JS `\s` (what /\s/ and String#split(/\s+/) agree on).
fn is_js_ws(c: char) -> bool {
    matches!(c,
        ' ' | '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | '\u{2028}' | '\u{2029}'
        | '\u{a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// UTF-16 code-unit length (JS `String#length`).
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// JS `String(v)` for the JSON shapes a sidecar/db value can have.
fn js_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(_) | Value::Object(_) => "[object object]".into(),
    }
}

/// `String(x || fallback)` after qs shapes: arrays join on ',', objects
/// stringify '[object object]', the empty string is falsy, `[]` is TRUTHY
/// (so `String([])` = '' wins over the fallback).
fn qs_or(q: Option<&str>, key: &str, fallback: &str) -> String {
    match qs_get(q, key) {
        QsVal::Missing => fallback.to_string(),
        QsVal::Scalar(s) => {
            if s.is_empty() {
                fallback.to_string()
            } else {
                s
            }
        }
        QsVal::Array(a) if a.is_empty() => String::new(),
        QsVal::Array(a) => {
            let j = a.join(",");
            if j.is_empty() {
                fallback.to_string() // ['',''] joins to '' — falsy → fallback
            } else {
                j
            }
        }
        QsVal::Obj => "[object object]".into(),
    }
}

// ---------------------------------------------------------------------------
// scryPage / fetchWindow twin (LOCAL sidecar mode only)
// ---------------------------------------------------------------------------

pub enum WinErr {
    /// Non-2xx after scryGetRetried's 4×-429 loop; the caller templates the
    /// axios message 'Request failed with status code N'.
    Status(u16),
    /// Transport/timeout: Node's axios message is environment-specific, and
    /// both ported routes answer 502 without surfacing it.
    Transport,
}

struct Win {
    cards: Vec<Value>,
    total: Option<f64>,
}

/// `scryGetRetried(scryGet('/cards/search?q=…&page=N[&order=…]'))` against
/// the sidecar base. 429 retries up to 4 attempts, every other error
/// throws immediately. A non-JSON (or JSON-scalar/array) 200 body leaves
/// axios with `resp.data` as a string: `resp.data.data` is undefined → the
/// page reads as empty, has_more false, total null.
async fn scry_page(
    st: &AppState,
    base: &str,
    q: &str,
    page: i64,
    order: Option<&str>,
) -> Result<(Vec<Value>, bool, Option<f64>), WinErr> {
    let mut uri = format!("{base}/cards/search?q={}&page={}", encode_uri_component(q), page);
    if let Some(o) = order {
        uri.push_str("&order=");
        uri.push_str(o);
    }
    let uri = uri.parse::<hyper::Uri>().map_err(|_| WinErr::Transport)?;
    let mut last: i32 = -1; // -1 = transport, else the last HTTP status
    for attempt in 0..4u8 {
        let req = hyper::Request::get(uri.clone())
            .header("user-agent", "Scrybox/1.0")
            .header("accept", "application/json")
            .body(Body::empty())
            .map_err(|_| WinErr::Transport)?;
        let res = match tokio::time::timeout(PAGE_TIMEOUT, st.client.request(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) | Err(_) => {
                // Transport/timeout: Node does NOT retry these, but a
                // bounded extra attempt cannot change the observable 502.
                last = -1;
                continue;
            }
        };
        let status = res.status().as_u16();
        if status == 429 {
            last = 429;
            if attempt < 3 {
                continue; // scryGetRetried: `if (status === 429 && i < retries - 1) continue`
            }
            return Err(WinErr::Status(429));
        }
        if status != 200 {
            return Err(WinErr::Status(status));
        }
        let bytes = match BodyExt::collect(res.into_body()).await {
            Ok(b) => b.to_bytes(),
            Err(_) => return Err(WinErr::Transport),
        };
        let json: Value = match serde_json::from_slice(&bytes) {
            Ok(v @ Value::Object(_)) => v,
            // Non-object body: resp.data.data undefined → empty page.
            _ => return Ok((Vec::new(), false, None)),
        };
        let data = json.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let has_more = json.get("has_more").map(|v| js_truthy(Some(v))).unwrap_or(false);
        let total = match json.get("total_cards") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => n.as_f64(),
            // Truthy non-number passes `!= null` and later %s into NaN;
            // unreachable from scryfall-local, treated as no update.
            Some(_) => None,
        };
        return Ok((data, has_more, total));
    }
    if last < 0 {
        Err(WinErr::Transport)
    } else {
        Err(WinErr::Status(last as u16))
    }
}

/// fetchWindow (scryfallApi.js:544) for the LOCAL sidecar base, English
/// (langSearch with no lang adds nothing).
async fn fetch_window(
    st: &AppState,
    base: &str,
    q: &str,
    offset: i64,
    limit: i64,
    order: Option<&str>,
) -> Result<Win, WinErr> {
    let mut page = offset.div_euclid(SCRY_PAGE_SIZE) + 1;
    let mut skip = offset.rem_euclid(SCRY_PAGE_SIZE) as usize;
    let mut out: Vec<Value> = Vec::new();
    let mut total: Option<f64> = None;
    let mut has_more = false;
    while (out.len() as i64) < limit {
        let (data, hm, ptotal) = scry_page(st, base, q, page, order).await?;
        if ptotal.is_some() {
            total = ptotal;
        }
        out.extend(data.into_iter().skip(skip));
        skip = 0;
        has_more = hm;
        if !has_more {
            break;
        }
        page += 1;
    }
    let _ = has_more; // fetchWindow recomputes hasMore for its own caller
    out.truncate(limit as usize);
    Ok(Win { cards: out, total })
}

// ---------------------------------------------------------------------------
// Express res.json twin (default ETag; freshness 304; gzip ≥ 1024 B)
// ---------------------------------------------------------------------------

fn json_bytes_of(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec())
}

enum Cond {
    Get {
        inm: Option<String>,
        ae: Option<String>,
    },
    Post,
}

fn get_cond(headers: &HeaderMap) -> Cond {
    Cond::Get {
        inm: headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).map(str::to_string),
        ae: headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).map(str::to_string),
    }
}

/// `res.json(payload)` on an OK path.
fn res_json(v: &Value, cond: &Cond) -> Response {
    let raw = json_bytes_of(v);
    let etag = body_etag(&raw);
    match cond {
        Cond::Get { inm, ae } => {
            if etag_matches(inm.as_deref(), &etag) {
                return Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, etag.as_str())
                    .body(Body::empty())
                    .unwrap();
            }
            let use_gz = wants_encoding(ae.as_deref(), "gzip", raw.len());
            let body = if use_gz { Body::from(gzip_bytes(&raw)) } else { Body::from(raw) };
            let mut b = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                .header(header::ETAG, etag.as_str());
            if use_gz {
                b = b.header(header::CONTENT_ENCODING, "gzip").header(header::VARY, "Accept-Encoding");
            }
            b.body(body).unwrap()
        }
        Cond::Post => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .header(header::ETAG, etag.as_str())
            .body(Body::from(raw))
            .unwrap(),
    }
}

/// `res.status(n).json(payload)` — Express stamps the ETag there too.
fn json_status(status: StatusCode, v: &Value) -> Response {
    let raw = json_bytes_of(v);
    let etag = body_etag(&raw);
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::ETAG, etag.as_str())
        .body(Body::from(raw))
        .unwrap()
}

fn err_json(status: StatusCode, msg: &str) -> Response {
    json_status(status, &obj(&[("error", Value::String(msg.into()))]))
}

fn obj(pairs: &[(&str, Value)]) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    Value::Object(m)
}

/// rusqlite row value → JSON, the way Node's sqlite driver surfaces it to
/// the route: INTEGER → int Number, REAL → json_num (drops a trailing .0
/// exactly like V8's JSON.stringify), TEXT → String, BLOB (never produced
/// by these queries) → null.
fn sqlv_json(v: rusqlite::types::Value) -> Value {
    use rusqlite::types::Value as Sv;
    match v {
        Sv::Null => Value::Null,
        Sv::Integer(i) => Value::from(i),
        Sv::Real(f) => json_num(f),
        Sv::Text(s) => Value::String(s),
        Sv::Blob(_) => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// shared entry gates
// ---------------------------------------------------------------------------

/// The (db, auth) prefix every route needs after the owns/method check.
/// Returns (pool, headers, cond, user id); auth errors return as Err.
fn gate_core(st: &AppState, headers: &HeaderMap, want: &Method) -> Result<(std::sync::Arc<DbPool>, Cond, i64), Response> {
    let pool = st.db.clone().ok_or_else(|| Response::new(Body::empty()))?;
    let cond = if *want == Method::GET { get_cond(headers) } else { Cond::Post };
    let token = bearer_token(headers).map(str::to_string);
    let user = authenticate(&pool, token.as_deref(), true).map_err(|res| res)?;
    Ok((pool, cond, user.id))
}

fn local_base(st: &AppState) -> Option<String> {
    if st.scry.env().local {
        Some(st.scry.env().local_url.clone())
    } else {
        None
    }
}

/// Whole-request GET replay to Node (headers captured before consuming).
async fn replay_get(st: &AppState, headers: &HeaderMap, path: &str, query: Option<String>) -> Response {
    let pq = match query {
        Some(q) => format!("{path}?{q}"),
        None => path.to_string(),
    };
    let mut rb = Request::builder().method(Method::GET).uri(pq);
    for (k, v) in headers.iter() {
        if k != header::CONTENT_LENGTH {
            rb = rb.header(k, v);
        }
    }
    let req = rb.body(Body::empty()).unwrap_or_else(|_| Request::new(Body::empty()));
    crate::proxy(State(st.clone()), req).await
}

/// Whole-request POST replay to Node with the ORIGINAL raw bytes.
async fn replay_post_raw(st: &AppState, headers: &HeaderMap, path: &str, body: &[u8]) -> Response {
    let mut rb = Request::builder().method(Method::POST).uri(path);
    for (k, v) in headers.iter() {
        if k != header::CONTENT_LENGTH {
            rb = rb.header(k, v);
        }
    }
    let req = rb.body(Body::from(body.to_vec())).unwrap_or_else(|_| Request::new(Body::empty()));
    crate::proxy(State(st.clone()), req).await
}

// ---------------------------------------------------------------------------
// POST /api/cards/highlight
// ---------------------------------------------------------------------------

/// The bare-word gate a query must clear for native answering: ASCII
/// alphanumerics and JS whitespace only, and no standalone `or`/`and`
/// (tokenize.js:331 matches /^or|and$/i). With that, the tokenizer can
/// only produce bare name leaves — analyze(q) cannot throw.
pub fn safe_free_text(q: &str) -> bool {
    if q.is_empty() || !q.chars().any(|c| !is_js_ws(c)) || !q.chars().all(|c| c.is_ascii_alphanumeric() || is_js_ws(c)) {
        return false;
    }
    q.split(is_js_ws).all(|t| {
        let l = t.to_ascii_lowercase();
        l != "or" && l != "and"
    })
}

/// stats.js `exact`: `!"${n.replace(/\\/g,'\\\\').replace(/"/g,'\\"')}"`.
fn exact_name_query(n: &str) -> String {
    format!("!\"{}\"", n.replace('\\', "\\\\").replace('"', "\\\""))
}

pub async fn handle_highlight(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("cards_meta") || req.method() != Method::POST || st.db.is_none() {
        return crate::proxy(State(st), req).await;
    }
    let headers = req.headers().clone();
    let (pool, _cond, _uid) = match gate_core(&st, &headers, &Method::POST) {
        Ok(v) => v,
        Err(res) => return res,
    };
    let _ = &pool; // highlight reads no DB state (names come from the body)

    // Body. express.json(strict) verdicts:
    //  - non-JSON content type / empty body → req.body stays undefined →
    //    `req.body?.q` is undefined → q='' → the 400 below (natively —
    //    that IS Node's answer);
    //  - JSON object/array → normal destructuring (array: .q/.names
    //    undefined → same 400);
    //  - malformed JSON or a strict-rejected scalar → body-parser's
    //    error-middleware path → replay the raw bytes to Node verbatim.
    let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();
    let is_json_ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().replace(' ', "").starts_with("application/json"))
        .unwrap_or(false);
    let parsed: Option<Value> = if is_json_ct && !bytes.is_empty() {
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(v @ (Value::Object(_) | Value::Array(_))) => Some(v),
            Ok(_) => return replay_post_raw(&st, &headers, "/api/cards/highlight", &bytes).await,
            Err(_) => return replay_post_raw(&st, &headers, "/api/cards/highlight", &bytes).await,
        }
    } else {
        None
    };

    // const q = typeof req.body?.q === 'string' ? req.body.q.trim() : ''
    let q = match parsed.as_ref().and_then(|p| p.get("q")) {
        Some(Value::String(s)) => js_trim(s).to_string(),
        _ => String::new(),
    };
    // names: [...new Set(body.names.filter(n => typeof n==='string' && n.trim()))]
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(Value::Array(a)) = parsed.as_ref().and_then(|p| p.get("names")) {
        for n in a {
            if let Value::String(s) = n {
                if !js_trim(s).is_empty() && seen.insert(s.clone()) {
                    names.push(s.clone());
                }
            }
        }
    }
    if q.is_empty() || js_len(&q) > 500 || names.is_empty() || names.len() > 1000 {
        return err_json(
            StatusCode::BAD_REQUEST,
            "q (1-500 chars) and names (1-1000) are required",
        );
    }

    // analyze(q) validation gate: bare words provably never throw; anything
    // else is Node's grammar to judge (raw replay before any engine call).
    if !safe_free_text(&q) || local_base(&st).is_none() {
        return replay_post_raw(&st, &headers, "/api/cards/highlight", &bytes).await;
    }
    let base = local_base(&st).unwrap();

    let wanted: HashSet<String> = names.iter().map(|n| n.to_lowercase()).collect();
    let mut matched: HashSet<String> = HashSet::new();
    let mut start = 0usize;
    while start < names.len() {
        let end = (start + HIGHLIGHT_CHUNK).min(names.len());
        let chunk_q = names[start..end]
            .iter()
            .map(|n| exact_name_query(n))
            .collect::<Vec<_>>()
            .join(" or ");
        let chunk_len = (end - start) as i64;
        start = end;
        let full_q = format!("({q}) ({chunk_q})");
        let win = match fetch_window(&st, &base, &full_q, 0, chunk_len * 4, Some("name")).await {
            Ok(w) => w,
            Err(WinErr::Status(s @ (400 | 422))) => {
                return err_json(StatusCode::BAD_REQUEST, &format!("Request failed with status code {s}"))
            }
            Err(WinErr::Status(404)) => {
                return res_json(&obj(&[("names", Value::Array(vec![]))]), &Cond::Post)
            }
            Err(_) => return err_json(StatusCode::BAD_GATEWAY, "Search engine unavailable"),
        };
        for c in win.cards {
            // const full = String(c.name || '')
            let full = match c.get("name") {
                Some(v) if js_truthy(Some(v)) => js_str(v),
                _ => String::new(),
            };
            let front = full.split(" // ").next().unwrap_or("");
            let lf = full.to_lowercase();
            let lfr = front.to_lowercase();
            if wanted.contains(&lf) {
                matched.insert(lf);
            }
            if wanted.contains(&lfr) {
                matched.insert(lfr);
            }
        }
    }
    let out: Vec<Value> = names
        .into_iter()
        .filter(|n| {
            let ln = n.to_lowercase();
            if matched.contains(&ln) {
                return true;
            }
            let front = n.split(" // ").next().unwrap_or("").to_lowercase();
            matched.contains(&front)
        })
        .map(Value::String)
        .collect();
    res_json(&obj(&[("names", Value::Array(out))]), &Cond::Post)
}

// ---------------------------------------------------------------------------
// GET /api/cards/:id/decks
// ---------------------------------------------------------------------------

pub async fn handle_card_decks(State(st): State<AppState>, Path(id): Path<String>, req: Request) -> Response {
    if !st.owns("cards_meta") || req.method() != Method::GET || st.db.is_none() {
        return crate::proxy(State(st), req).await;
    }
    let headers = req.headers().clone();
    let (pool, cond, uid) = match gate_core(&st, &headers, &Method::GET) {
        Ok(v) => v,
        Err(res) => return res,
    };
    let sql = format!(
        "
      SELECT d.id, d.name, SUM(dc.quantity) AS quantity
      FROM decks d
      CROSS JOIN deck_cards dc ON dc.deck_id = d.id
      CROSS JOIN card_cache dcc ON dc.card_id = dcc.id
      CROSS JOIN card_cache target ON target.id = ?
      WHERE {key_dcc} = {key_target}
        AND d.user_id = ?
        AND d.checked_out = 1
        AND dc.quantity > 0
        AND NOT {basic_target}
      GROUP BY d.id, d.name
      ORDER BY d.name COLLATE NOCASE
    ",
        key_dcc = sql_card_key("dcc"),
        key_target = sql_card_key("target"),
        basic_target = crate::collection_writes::sql_is_basic_land_expr("target"),
    );
    let db = pool;
    let out = tokio::task::spawn_blocking(move || -> Result<Value, rusqlite::Error> {
        let conn = db.get()?;
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![id, uid], |r| {
            Ok((
                sqlv_json(r.get::<_, rusqlite::types::Value>(0)?),
                sqlv_json(r.get::<_, rusqlite::types::Value>(1)?),
                r.get::<_, Option<f64>>(2)?,
            ))
        })?;
        let mut decks = Vec::new();
        for row in rows {
            let (did, name, qty) = row?;
            // Number(d.quantity) || 0
            let qty = match qty {
                Some(f) if f != 0.0 && f == f => json_num(f),
                _ => json_num(0.0),
            };
            decks.push(obj(&[("id", did), ("name", name), ("quantity", qty)]));
        }
        Ok(obj(&[("decks", Value::Array(decks))]))
    })
    .await;
    match out {
        Ok(Ok(v)) => res_json(&v, &cond),
        _ => err_json(StatusCode::INTERNAL_SERVER_ERROR, "Could not load decks"),
    }
}

// ---------------------------------------------------------------------------
// GET /api/cards/:id/price-history
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum DaysSel {
    /// '30d' → windowed query; windowDays: 30.
    D(i64),
    /// Truthy non-string PRICE_HISTORY_RANGES hit (lowercase Object.prototype
    /// own names): the windowed query runs with a garbage `-… days` modifier
    /// → datetime() NULL → zero rows. windowDays is DROPPED (function value)
    /// for 'constructor', or {} (Object.prototype) for '__proto__'.
    Garbage { desc: &'static str, window_days: Value, drop: bool },
    /// undefined ⇒ 'all': unwindowed query, windowDays: null.
    All,
}

pub async fn handle_price_history(State(st): State<AppState>, Path(id): Path<String>, req: Request) -> Response {
    if !st.owns("cards_meta") || req.method() != Method::GET || st.db.is_none() {
        return crate::proxy(State(st), req).await;
    }
    let query = req.uri().query().map(str::to_string);
    let headers = req.headers().clone();
    let (pool, cond, _uid) = match gate_core(&st, &headers, &Method::GET) {
        Ok(v) => v,
        Err(res) => return res,
    };
    let _ = &headers;
    // const rangeKey = String(req.query.range || '30d').toLowerCase()
    let range_key = qs_or(query.as_deref(), "range", "30d").to_lowercase();
    let days = match range_key.as_str() {
        "30d" => DaysSel::D(30),
        "constructor" => DaysSel::Garbage { desc: "constructor", window_days: Value::Null, drop: true },
        "__proto__" => DaysSel::Garbage { desc: "[object object]", window_days: Value::Object(Map::new()), drop: false },
        _ => DaysSel::All,
    };
    let db = pool;
    let days_c = days.clone();
    let fetch = tokio::task::spawn_blocking(move || -> Result<Vec<(Value, Option<String>)>, String> {
        let conn = db.get().map_err(|e| e.to_string())?;
        let (sql, param): (&str, Option<String>) = match &days_c {
            DaysSel::D(n) => (
                "
          SELECT price, recorded_at
          FROM price_history
          WHERE card_id = ? AND recorded_at >= datetime('now', ?)
          ORDER BY recorded_at ASC
        ",
                Some(format!("-{n} days")),
            ),
            DaysSel::Garbage { desc, .. } => (
                "
          SELECT price, recorded_at
          FROM price_history
          WHERE card_id = ? AND recorded_at >= datetime('now', ?)
          ORDER BY recorded_at ASC
        ",
                Some(format!("-{desc} days")),
            ),
            DaysSel::All => (
                "
          SELECT price, recorded_at
          FROM price_history
          WHERE card_id = ?
          ORDER BY recorded_at ASC
        ",
                None,
            ),
        };
        let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
        let map = |r: &rusqlite::Row| {
            Ok((sqlv_json(r.get::<_, rusqlite::types::Value>(0)?), r.get::<_, Option<String>>(1)?))
        };
        let rows = match param {
            Some(p) => stmt.query_map(rusqlite::params![id, p], map),
            None => stmt.query_map(rusqlite::params![id], map),
        }
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
        Ok(rows)
    });
    let rows = match fetch.await {
        Ok(Ok(r)) => r,
        _ => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve price history"),
    };

    // points = recorded.map(h => ({price: h.price,
    //   time: parseSqliteUtc(h.recorded_at).getTime(), source: 'recorded'}))
    // NULL/'' recorded_at → Invalid Date → NaN time (None below).
    let mut points: Vec<(Value, Option<f64>)> = Vec::new();
    for (price, rec) in &rows {
        let t = rec.as_deref().and_then(parse_sqlite_utc);
        points.push((price.clone(), t));
    }
    // points.sort((a, b) => a.time - b.time): NaN comparisons → 0 (V8's
    // sort is stable; so is Rust's).
    points.sort_by(|a, b| match (a.1, b.1) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        _ => std::cmp::Ordering::Equal,
    });
    let recorded_count = points.len();

    // Collapse flat runs: drop i when prev && next && prev.price === cur
    // && next.price === cur (JS ===: serde_json Number equality is
    // numeric — 2 == 2.0 — and Null == Null, matching null === null).
    let mut data: Vec<(Value, Option<f64>)> = Vec::new();
    for i in 0..points.len() {
        let prev = i.checked_sub(1).map(|k| &points[k]);
        let next = points.get(i + 1);
        if let (Some(p), Some(n)) = (prev, next) {
            if p.0 == points[i].0 && n.0 == points[i].0 {
                continue;
            }
        }
        data.push(points[i].clone());
    }

    // data.map's new Date(p.time).toISOString() THROWS on NaN → whole 500.
    if data.iter().any(|(_, t)| t.is_none()) {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to retrieve price history");
    }

    // spanDays = times.length >= 2 ? Math.round((max-min)/86400000) : 0 —
    // over ALL points (collapsed-away NaN only poisons this → null).
    let span_days: Value = if points.len() >= 2 {
        if points.iter().any(|(_, t)| t.is_none()) {
            Value::Null
        } else {
            let mut mn = f64::MAX;
            let mut mx = f64::MIN;
            for (_, t) in &points {
                let t = t.unwrap();
                if t < mn {
                    mn = t;
                }
                if t > mx {
                    mx = t;
                }
            }
            json_num(js_math_round((mx - mn) / 86_400_000.0))
        }
    } else {
        json_num(0.0)
    };

    let data_json: Vec<Value> = data
        .iter()
        .map(|(price, t)| {
            obj(&[
                ("price", price.clone()),
                ("recorded_at", Value::String(iso_utc_ms(t.unwrap() as i64))),
                ("source", Value::String("recorded".into())),
            ])
        })
        .collect();

    let mut m = Map::new();
    m.insert("data".into(), Value::Array(data_json));
    m.insert("marketCount".into(), json_num(0.0));
    m.insert("recordedCount".into(), json_num(recorded_count as f64));
    m.insert("insufficientHistory".into(), Value::Bool(data.len() < 2));
    m.insert("spanDays".into(), span_days);
    match &days {
        DaysSel::D(n) => {
            m.insert("windowDays".into(), json_num(*n as f64));
        }
        DaysSel::Garbage { window_days, drop, .. } => {
            if !*drop {
                m.insert("windowDays".into(), window_days.clone());
            }
        }
        DaysSel::All => {
            m.insert("windowDays".into(), Value::Null);
        }
    }
    res_json(&Value::Object(m), &cond)
}

/// JS Math.round: half toward +Infinity.
fn js_math_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

// ---------------------------------------------------------------------------
// GET /api/stats/commander-of-the-day
// ---------------------------------------------------------------------------

const COTD_QUERY: &str = "is:commander legal:commander game:paper -is:digital";

fn cotd_cache() -> &'static Mutex<(HashMap<String, Value>, VecDeque<String>)> {
    static C: OnceLock<Mutex<(HashMap<String, Value>, VecDeque<String>)>> = OnceLock::new();
    C.get_or_init(|| Mutex::new((HashMap::new(), VecDeque::new())))
}

/// dayHash (stats.js:766) — FNV-1a over UTF-16 code units, >>> 0.
pub fn day_hash(s: &str) -> u32 {
    let mut h: u32 = 2166136261;
    for u in s.encode_utf16() {
        h ^= u as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

/// Howard Hinnant civil-calendar helpers (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Ordinal epoch-day of the nth `weekday` (0=Sun) of month/year.
/// 1970-01-01 (epoch day 0) was a Thursday ⇒ dow = (z + 4) mod 7.
fn nth_weekday(year: i64, month: i64, weekday: i64, n: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    let w1 = (first + 4).rem_euclid(7);
    first + ((weekday - w1 + 7) % 7) + (n - 1) * 7
}

/// Intl.DateTimeFormat('en-CA', {timeZone:'America/New_York', 2-digit}).
/// US Eastern DST since 2007: 2nd Sunday of March 02:00 local (07:00Z)
/// → 1st Sunday of November 02:00 local (06:00Z).
pub fn eastern_day(now_ms: i64) -> String {
    let y = civil_from_days(now_ms.div_euclid(86_400_000)).0;
    let dst_start = nth_weekday(y, 3, 0, 2) * 86_400_000 + 7 * 3_600_000;
    let dst_end = nth_weekday(y, 11, 0, 1) * 86_400_000 + 6 * 3_600_000;
    let offset_ms = if now_ms >= dst_start && now_ms < dst_end {
        -4 * 3_600_000
    } else {
        -5 * 3_600_000
    };
    let (yy, mo, d) = civil_from_days((now_ms + offset_ms).div_euclid(86_400_000));
    format!("{yy:04}-{mo:02}-{d:02}")
}

/// edhrecSlug (stats.js:771): first face, NFD + strip U+0300–036F,
/// lowercase, drop ['’,.], collapse non-alphanumerics to '-', trim '-'.
pub fn edhrec_slug(n: &str) -> String {
    let first = n.split(" // ").next().unwrap_or("");
    let decomposed: String = first
        .chars()
        .flat_map(nfd_decompose)
        .filter(|c| !('\u{0300}'..='\u{036f}').contains(c))
        .collect();
    let lowered = decomposed.to_lowercase();
    let mut out = String::new();
    let mut pending_dash = false;
    for c in lowered.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        } else if c != '\'' && c != '’' && c != ',' && c != '.' {
            pending_dash = !out.is_empty();
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Canonical NFD for the precomposed Latin blocks realistic card names use
/// (Latin-1 Supplement + Latin Extended-A); anything else passes through
/// and is then dropped by the [^a-z0-9] collapse — identical to Node
/// leaving an un-decomposing character (e.g. 'Æ' → 'thersprens').
fn nfd_decompose(c: char) -> Vec<char> {
    static M: OnceLock<HashMap<char, &'static str>> = OnceLock::new();
    let m = M.get_or_init(|| {
        let table: &[(char, &str)] = &[
            ('\u{c0}', "A\u{300}"), ('\u{c1}', "A\u{301}"), ('\u{c2}', "A\u{302}"), ('\u{c3}', "A\u{303}"),
            ('\u{c4}', "A\u{308}"), ('\u{c5}', "A\u{30a}"), ('\u{c7}', "C\u{327}"), ('\u{c8}', "E\u{300}"),
            ('\u{c9}', "E\u{301}"), ('\u{ca}', "E\u{302}"), ('\u{cb}', "E\u{308}"), ('\u{cc}', "I\u{300}"),
            ('\u{cd}', "I\u{301}"), ('\u{ce}', "I\u{302}"), ('\u{cf}', "I\u{308}"), ('\u{d1}', "N\u{303}"),
            ('\u{d2}', "O\u{300}"), ('\u{d3}', "O\u{301}"), ('\u{d4}', "O\u{302}"), ('\u{d5}', "O\u{303}"),
            ('\u{d6}', "O\u{308}"), ('\u{d9}', "U\u{300}"), ('\u{da}', "U\u{301}"), ('\u{db}', "U\u{302}"),
            ('\u{dc}', "U\u{308}"), ('\u{dd}', "Y\u{301}"), ('\u{e0}', "a\u{300}"), ('\u{e1}', "a\u{301}"),
            ('\u{e2}', "a\u{302}"), ('\u{e3}', "a\u{303}"), ('\u{e4}', "a\u{308}"), ('\u{e5}', "a\u{30a}"),
            ('\u{e7}', "c\u{327}"), ('\u{e8}', "e\u{300}"), ('\u{e9}', "e\u{301}"), ('\u{ea}', "e\u{302}"),
            ('\u{eb}', "e\u{308}"), ('\u{ec}', "i\u{300}"), ('\u{ed}', "i\u{301}"), ('\u{ee}', "i\u{302}"),
            ('\u{ef}', "i\u{308}"), ('\u{f1}', "n\u{303}"), ('\u{f2}', "o\u{300}"), ('\u{f3}', "o\u{301}"),
            ('\u{f4}', "o\u{302}"), ('\u{f5}', "o\u{303}"), ('\u{f6}', "o\u{308}"), ('\u{f9}', "u\u{300}"),
            ('\u{fa}', "u\u{301}"), ('\u{fb}', "u\u{302}"), ('\u{fc}', "u\u{308}"), ('\u{fd}', "y\u{301}"),
            ('\u{ff}', "y\u{308}"),
            ('\u{100}', "A\u{304}"), ('\u{101}', "a\u{304}"), ('\u{102}', "A\u{306}"), ('\u{103}', "a\u{306}"),
            ('\u{104}', "A\u{307}"), ('\u{105}', "a\u{307}"), ('\u{106}', "C\u{301}"), ('\u{107}', "c\u{301}"),
            ('\u{108}', "C\u{302}"), ('\u{109}', "c\u{302}"), ('\u{10a}', "C\u{307}"), ('\u{10b}', "c\u{307}"),
            ('\u{10c}', "C\u{30c}"), ('\u{10d}', "c\u{30c}"), ('\u{10e}', "D\u{30c}"), ('\u{10f}', "d\u{30c}"),
            ('\u{110}', "D\u{333}"), ('\u{112}', "E\u{304}"), ('\u{113}', "e\u{304}"), ('\u{114}', "E\u{306}"),
            ('\u{115}', "e\u{306}"), ('\u{116}', "E\u{307}"), ('\u{117}', "e\u{307}"), ('\u{118}', "E\u{328}"),
            ('\u{119}', "e\u{328}"), ('\u{11a}', "E\u{30c}"), ('\u{11b}', "e\u{30c}"), ('\u{11c}', "G\u{302}"),
            ('\u{11d}', "g\u{302}"), ('\u{11e}', "G\u{30c}"), ('\u{11f}', "g\u{30c}"), ('\u{120}', "G\u{307}"),
            ('\u{121}', "g\u{307}"), ('\u{122}', "G\u{312}"), ('\u{123}', "g\u{312}"), ('\u{124}', "H\u{302}"),
            ('\u{125}', "h\u{302}"), ('\u{126}', "H\u{332}"), ('\u{128}', "I\u{303}"), ('\u{129}', "i\u{303}"),
            ('\u{12a}', "I\u{304}"), ('\u{12b}', "i\u{304}"), ('\u{12c}', "I\u{306}"), ('\u{12d}', "i\u{306}"),
            ('\u{12e}', "I\u{328}"), ('\u{12f}', "i\u{328}"), ('\u{130}', "I\u{307}"), ('\u{131}', "i"),
            ('\u{134}', "J\u{302}"), ('\u{135}', "j\u{302}"), ('\u{136}', "K\u{327}"), ('\u{137}', "k\u{327}"),
            ('\u{139}', "L\u{301}"), ('\u{13a}', "l\u{301}"), ('\u{13b}', "L\u{327}"), ('\u{13c}', "l\u{327}"),
            ('\u{13d}', "L\u{30c}"), ('\u{13e}', "l\u{30c}"), ('\u{141}', "L\u{338}"), ('\u{143}', "N\u{301}"),
            ('\u{144}', "n\u{301}"), ('\u{145}', "N\u{327}"), ('\u{146}', "n\u{327}"), ('\u{147}', "N\u{30c}"),
            ('\u{148}', "n\u{30c}"), ('\u{14a}', "N\u{325}"), ('\u{14b}', "n\u{325}"), ('\u{14c}', "O\u{304}"),
            ('\u{14d}', "o\u{304}"), ('\u{14e}', "O\u{306}"), ('\u{14f}', "o\u{306}"), ('\u{150}', "O\u{30b}"),
            ('\u{151}', "o\u{30b}"), ('\u{154}', "R\u{301}"), ('\u{155}', "r\u{301}"), ('\u{156}', "R\u{327}"),
            ('\u{157}', "r\u{327}"), ('\u{158}', "R\u{30c}"), ('\u{159}', "r\u{30c}"), ('\u{15a}', "S\u{301}"),
            ('\u{15b}', "s\u{301}"), ('\u{15c}', "S\u{302}"), ('\u{15d}', "s\u{302}"), ('\u{15e}', "S\u{327}"),
            ('\u{15f}', "s\u{327}"), ('\u{160}', "S\u{30c}"), ('\u{161}', "s\u{30c}"), ('\u{162}', "T\u{327}"),
            ('\u{163}', "t\u{327}"), ('\u{164}', "T\u{30c}"), ('\u{165}', "t\u{30c}"), ('\u{166}', "T\u{332}"),
            ('\u{168}', "U\u{303}"), ('\u{169}', "u\u{303}"), ('\u{16a}', "U\u{304}"), ('\u{16b}', "u\u{304}"),
            ('\u{16c}', "U\u{306}"), ('\u{16d}', "u\u{306}"), ('\u{16e}', "U\u{30a}"), ('\u{16f}', "u\u{30a}"),
            ('\u{170}', "U\u{30b}"), ('\u{171}', "u\u{30b}"), ('\u{172}', "U\u{328}"), ('\u{173}', "u\u{328}"),
            ('\u{174}', "W\u{302}"), ('\u{175}', "w\u{302}"), ('\u{176}', "Y\u{302}"), ('\u{177}', "y\u{302}"),
            ('\u{178}', "Y\u{308}"), ('\u{179}', "Z\u{301}"), ('\u{17a}', "z\u{301}"), ('\u{17b}', "Z\u{307}"),
            ('\u{17c}', "z\u{307}"), ('\u{17d}', "Z\u{30c}"), ('\u{17e}', "z\u{30c}"),
        ];
        let mut m: HashMap<char, &'static str> = HashMap::with_capacity(table.len());
        for (k, v) in table {
            m.insert(*k, v);
        }
        m
    });
    match m.get(&c) {
        Some(s) => s.chars().collect(),
        None => vec![c],
    }
}

pub async fn handle_cotd(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("cards_meta") || req.method() != Method::GET || st.db.is_none() {
        return crate::proxy(State(st), req).await;
    }
    let query = req.uri().query().map(str::to_string);
    let headers = req.headers().clone();
    let (pool, cond, _uid) = match gate_core(&st, &headers, &Method::GET) {
        Ok(v) => v,
        Err(res) => return res,
    };
    let _ = &pool;
    let base = match local_base(&st) {
        Some(b) => b,
        None => return replay_get(&st, &headers, "/api/stats/commander-of-the-day", query).await,
    };
    // const day = typeof req.query.day === 'string' && /^\d{4}-\d{2}-\d{2}$/
    //               .test(req.query.day) ? req.query.day : easternDay()
    let day = match qs_get(query.as_deref(), "day") {
        QsVal::Scalar(s)
            if s.len() == 10
                && s.bytes().enumerate().all(|(i, b)| match i {
                    4 | 7 => b == b'-',
                    _ => b.is_ascii_digit(),
                }) =>
        {
            s
        }
        _ => eastern_day(now_ms()),
    };
    if let Some(hit) = cotd_cache().lock().unwrap().0.get(&day) {
        return res_json(hit, &cond);
    }
    // Node's whole body sits in one try: EVERY throw → the 502 below.
    let first = match fetch_window(&st, &base, COTD_QUERY, 0, 1, Some("name")).await {
        Ok(w) => w,
        Err(_) => return err_json(StatusCode::BAD_GATEWAY, "Commander of the day unavailable"),
    };
    let total = match first.total {
        Some(t) if t != 0.0 && t == t => t,
        _ => return err_json(StatusCode::SERVICE_UNAVAILABLE, "No commanders available"),
    };
    let idx = (day_hash(&format!("cotd:{day}")) as f64) % total;
    let cards: Vec<Value> = if idx == 0.0 {
        first.cards
    } else {
        match fetch_window(&st, &base, COTD_QUERY, idx as i64, 1, Some("name")).await {
            Ok(w) => w.cards,
            Err(_) => return err_json(StatusCode::BAD_GATEWAY, "Commander of the day unavailable"),
        }
    };
    let raw = match cards.first() {
        Some(r) => r.clone(),
        None => return err_json(StatusCode::SERVICE_UNAVAILABLE, "No commander found"),
    };
    // face = (raw.card_faces && raw.card_faces[0]) || raw   (arrays only:
    // a non-array card_faces is unreachable from scryfall-local)
    let face = raw
        .get("card_faces")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .filter(|f| js_truthy(Some(f)))
        .cloned()
        .unwrap_or_else(|| raw.clone());
    // img = raw.image_uris || face.image_uris || {}
    let img = raw
        .get("image_uris")
        .filter(|v| js_truthy(Some(v)))
        .or_else(|| face.get("image_uris").filter(|v| js_truthy(Some(v))))
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let field_or = |name: &str, fb: &str| -> Value {
        match img.as_object().and_then(|m| m.get(name)) {
            Some(v) if js_truthy(Some(v)) => v.clone(),
            _ => Value::String(fb.into()),
        }
    };
    let image_url = match img.as_object().and_then(|m| m.get("normal")) {
        Some(v) if js_truthy(Some(v)) => v.clone(),
        _ => field_or("large", ""),
    };
    let type_line = raw
        .get("type_line")
        .filter(|v| js_truthy(Some(v)))
        .or_else(|| face.get("type_line").filter(|v| js_truthy(Some(v))))
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let name_for_slug = match raw.get("name") {
        Some(v) if js_truthy(Some(v)) => js_str(v),
        _ => String::new(), // edhrecSlug(String(n || ''))
    };
    let card = obj(&[
        ("id", raw.get("id").cloned().unwrap_or(Value::Null)),
        ("name", raw.get("name").cloned().unwrap_or(Value::Null)),
        ("type_line", type_line),
        (
            "color_identity",
            raw.get("color_identity")
                .filter(|v| js_truthy(Some(v)))
                .cloned()
                .unwrap_or(Value::Array(vec![])),
        ),
        ("set", raw.get("set").cloned().unwrap_or(Value::Null)),
        ("image_url", image_url),
        ("art_crop", field_or("art_crop", "")),
        (
            "edhrec_url",
            Value::String(format!("https://edhrec.com/commanders/{}", edhrec_slug(&name_for_slug))),
        ),
    ]);
    let payload = obj(&[
        ("day", Value::String(day.clone())),
        ("show", Value::Bool(day_hash(&format!("show:{day}")) % 2 == 0)),
        ("card", card),
    ]);
    {
        let mut c = cotd_cache().lock().unwrap();
        if !c.0.contains_key(&day) {
            c.1.push_back(day.clone());
        }
        c.0.insert(day, payload.clone());
        while c.0.len() > COTD_CACHE_MAX {
            match c.1.pop_front() {
                Some(old) => {
                    c.0.remove(&old);
                }
                None => break,
            }
        }
    }
    res_json(&payload, &cond)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

// ---------------------------------------------------------------------------
// GET /api/suggest
// ---------------------------------------------------------------------------

pub async fn handle_suggest(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("cards_meta") || req.method() != Method::GET || st.db.is_none() {
        return crate::proxy(State(st), req).await;
    }
    let query = req.uri().query().map(str::to_string);
    let headers = req.headers().clone();
    let (_pool, cond, _uid) = match gate_core(&st, &headers, &Method::GET) {
        Ok(v) => v,
        Err(res) => return res,
    };
    let empty = || res_json(&obj(&[("suggestions", Value::Array(vec![]))]), &cond);
    // const q = String(req.query.q || '').trim()
    let q = js_trim(&qs_or(query.as_deref(), "q", "")).to_string();
    if js_len(&q) < 2 {
        return empty();
    }
    // if (!scryfallApi.LOCAL_SCRYFALL) return { suggestions: [] }
    let base = match local_base(&st) {
        Some(b) => b,
        None => return empty(),
    };
    // q.slice(0, 80) — UTF-16 code units. A slice that splits a surrogate
    // pair leaves a lone surrogate in JS, where encodeURIComponent THROWS
    // URIError → the route's catch → {suggestions: []}. Same verdict here.
    let units: Vec<u16> = q.encode_utf16().take(80).collect();
    let clipped = match String::from_utf16(&units) {
        Ok(s) => s,
        Err(_) => return empty(),
    };
    let uri = format!("{base}/api/suggest?q={}", encode_uri_component(&clipped));
    let uri = match uri.parse::<hyper::Uri>() {
        Ok(u) => u,
        Err(_) => return empty(),
    };
    let hreq = match hyper::Request::get(uri).body(Body::empty()) {
        Ok(r) => r,
        Err(_) => return empty(),
    };
    // 2500 ms AbortController: any timeout/transport/non-2xx/parse failure
    // lands on the same {suggestions: []}.
    let res = match tokio::time::timeout(std::time::Duration::from_millis(2500), st.client.request(hreq)).await {
        Ok(Ok(r)) => r,
        _ => return empty(),
    };
    if !res.status().is_success() {
        return empty();
    }
    let bytes = match BodyExt::collect(res.into_body()).await {
        Ok(b) => b.to_bytes(),
        Err(_) => return empty(),
    };
    let data: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return empty(),
    };
    let sugg = match data.get("suggestions").and_then(|v| v.as_array()) {
        Some(a) => a.clone(),
        None => return empty(),
    };
    // filter: s && s.name && !seen.has(lower) && seen.add(lower); slice 8;
    // map: {name, set||'', type_line||'', image_small||'', id||''}.
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<(Value /*name*/, Map<String, Value>)> = Vec::new();
    for s in sugg {
        let name_v = match &s {
            Value::Object(m) => m.get("name").cloned(),
            _ => None, // s non-object (incl. null): s.name undefined → skip
        };
        let name = match &name_v {
            Some(Value::String(n)) if !n.is_empty() => n.clone(),
            // truthy non-string name → .toLowerCase() throws → whole catch → []
            Some(v) if js_truthy(Some(v)) => return empty(),
            _ => continue,
        };
        let key = name.to_lowercase();
        if !seen.insert(key) {
            continue;
        }
        out.push((name_v.clone().unwrap_or(Value::String(String::new())), s.as_object().cloned().unwrap_or_default()));
        if out.len() == 8 {
            break;
        }
    }
    let arr: Vec<Value> = out
        .into_iter()
        .map(|(name, m)| {
            let pick = |k: &str| -> Value {
                match m.get(k) {
                    Some(v) if js_truthy(Some(v)) => v.clone(),
                    _ => Value::String(String::new()),
                }
            };
            obj(&[
                ("name", name),
                ("set", pick("set")),
                ("type_line", pick("type_line")),
                ("image_small", pick("image_small")),
                ("id", pick("id")),
            ])
        })
        .collect();
    res_json(&obj(&[("suggestions", Value::Array(arr))]), &cond)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_hash_matches_node() {
        // Generated with node from stats.js dayHash (V8 Math.imul).
        assert_eq!(day_hash("cotd:2026-11-12"), 2060033578);
        assert_eq!(day_hash("show:2026-11-12"), 2741042831);
        assert_eq!(day_hash("cotd:2026-03-08"), 828028856);
        assert_eq!(day_hash(""), 2166136261);
    }

    #[test]
    fn safe_gate() {
        assert!(safe_free_text("black creature"));
        assert!(safe_free_text("Lightning Bolt"));
        assert!(safe_free_text("a1 b2"));
        assert!(!safe_free_text("is:land"));
        assert!(!safe_free_text("bolt or lightning"));
        assert!(!safe_free_text("BOLT AND lightning"));
        assert!(!safe_free_text("!\"Lightning Bolt\""));
        assert!(!safe_free_text("a)b"));
        assert!(!safe_free_text("a-b"));
        assert!(!safe_free_text("a.b"));
        assert!(!safe_free_text(""));
        assert!(!safe_free_text("   "));
    }

    #[test]
    fn slug_matches_node_cases() {
        // Generated with node from stats.js edhrecSlug.
        assert_eq!(edhrec_slug("Lightning Bolt"), "lightning-bolt");
        assert_eq!(edhrec_slug("K'rrik, Son of Andan"), "krrik-son-of-andan");
        assert_eq!(edhrec_slug("Èterka, Weathered Explorer"), "eterka-weathered-explorer");
        assert_eq!(edhrec_slug("A // B"), "a");
        assert_eq!(edhrec_slug(""), "");
        assert_eq!(edhrec_slug("---"), "");
        assert_eq!(edhrec_slug("Giseïla, the Broken Blade"), "giseila-the-broken-blade");
        assert_eq!(edhrec_slug("Æthersprens"), "thersprens");
        assert_eq!(edhrec_slug("  Trim  Me  "), "trim-me");
        assert_eq!(edhrec_slug("Ür, Fëanor’s"), "ur-feanors");
    }

    #[test]
    fn eastern_day_boundaries() {
        // Expected values from node Intl.DateTimeFormat('en-CA', tz NY).
        let ms = |s: &str| parse_sqlite_utc(&s.replace(" ", "T")).unwrap() as i64; // the Z-suffixed leg of parseSqliteUtc is strict ISO
        assert_eq!(eastern_day(ms("2026-03-08 06:59:00Z")), "2026-03-08");
        assert_eq!(eastern_day(ms("2026-03-08 07:00:00Z")), "2026-03-08");
        assert_eq!(eastern_day(ms("2026-11-01 05:59:00Z")), "2026-11-01");
        assert_eq!(eastern_day(ms("2026-11-01 06:00:00Z")), "2026-11-01");
        assert_eq!(eastern_day(ms("2026-11-10 00:00:00Z")), "2026-11-09");
        assert_eq!(eastern_day(ms("2026-01-01 04:59:00Z")), "2025-12-31");
        assert_eq!(eastern_day(ms("2026-01-01 05:00:00Z")), "2026-01-01");
    }

    #[test]
    fn math_round_is_half_up() {
        assert_eq!(js_math_round(0.5), 1.0);
        assert_eq!(js_math_round(0.4999), 0.0);
        assert_eq!(js_math_round(-0.5), -0.0);
        assert_eq!(js_math_round(2.5), 3.0);
    }

    #[test]
    fn exact_query_escapes_backslash_then_quote() {
        assert_eq!(exact_name_query("Gisela \"the\" Blade"), "!\"Gisela \\\"the\\\" Blade\"");
        assert_eq!(exact_name_query("a\\b"), "!\"a\\\\b\"");
    }

    #[test]
    fn qs_or_semantics() {
        assert_eq!(qs_or(Some("x=1"), "range", "30d"), "30d");
        assert_eq!(qs_or(Some("range="), "range", "30d"), "30d");
        assert_eq!(qs_or(Some("range=30d"), "range", "30d"), "30d");
        assert_eq!(qs_or(Some("range=a&range=b"), "range", "30d"), "a,b");
        assert_eq!(qs_or(Some("range[]=x"), "range", "30d"), "x");
        assert_eq!(qs_or(Some("range[a]=1"), "range", "30d"), "[object object]");
    }
}

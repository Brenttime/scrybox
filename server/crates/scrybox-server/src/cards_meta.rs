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
use crate::poison::LockExt;
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
/// `want` drives Node's GET-only API-key rule (middleware/auth.js:24-26:
/// `req.method !== 'GET'` + API key → 403 'API keys are read-only') —
/// REVIEW-auth.md H2.
fn gate_core(st: &AppState, headers: &HeaderMap, want: &Method) -> Result<(std::sync::Arc<DbPool>, Cond, i64), Response> {
    let pool = st.db.clone().ok_or_else(|| Response::new(Body::empty()))?;
    let cond = if *want == Method::GET { get_cond(headers) } else { Cond::Post };
    let token = bearer_token(headers).map(str::to_string);
    let user = authenticate(&pool, token.as_deref(), *want == Method::GET).map_err(|res| res)?;
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
    let (_parts, bytes, oversize) =
        crate::collection_writes::read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    let is_json_ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().replace(' ', "").starts_with("application/json"))
        .unwrap_or(false);
    if oversize && is_json_ct {
        // normalJson (1mb) ceiling exceeded: body-parser's entity.too.large
        // error-middleware answer — the same 413 the raw-bytes replay would
        // have produced from Node, minus buffering the whole stream first
        // (REVIEW-auth.md H1). Non-JSON content-types skip the parser: the
        // abandoned (empty) body parses as `req.body === undefined` below.
        return crate::collection_writes::too_large();
    }
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

/// edhrecSlug (stats.js:771): first face, lowercase, NFD, strip
/// \p{Diacritic}, drop ['’,.], collapse non-alphanumerics to '-', trim '-'.
/// JS order is toLowerCase() *before* normalize('NFD') (edhrecSynergy.js:10-12
/// semantics via stats.js edhrecSlug) — mirrored here: to_lowercase first,
/// then the generated NFD table (keyed by lowercase codepoint), then the
/// generated \p{Diacritic} strip.
pub fn edhrec_slug(n: &str) -> String {
    let first = n.split(" // ").next().unwrap_or("");
    let decomposed: String = first
        .chars()
        .flat_map(|c| c.to_lowercase())
        .flat_map(nfd_decompose)
        .filter(|c| !is_diacritic(*c))
        .collect();
    let lowered = decomposed;
    let mut out = String::new();
    let mut pending_dash = false;
    for c in lowered.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        } else if c != '\'' && c != '’' {
            // Node drops ONLY ['’] outright (edhrecSynergy.js:12), then every
            // other non-[a-z0-9\s] run — commas and dots included — becomes a
            // single separator ('C.A.M.P.' → 'c-a-m-p').
            pending_dash = !out.is_empty();
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Canonical NFD for the slug twin, GENERATED from the Node runtime that
/// answers :3312 (Unicode 17.0): every codepoint whose
/// String(c).toLowerCase().normalize('NFD') keeps an ASCII [a-z0-9] base
/// after the \p{Diacritic} strip — the only codepoints that can change a
/// slug; anything else passes through unchanged and hits the [^a-z0-9]
/// collapse, exactly like Node leaving an un-decomposing character
/// ('Æ' → 'thersprens', 'Tıbir Tamı' → 't-bir-tam' — ı/Đđ/Ħħ/Łł/Ŋŋ/Ŧŧ have
/// NO canonical decomposition and are not \p{Diacritic}, so they become
/// separators; the old hand table's fake stroke decompositions mapped them
/// to base letters and built the wrong EDHREC URLs). Keys are LOWERCASE
/// codepoints: edhrec_slug lowercases first, JS-style, and the only
/// multi-char lowercase with a decomposition (U+0130 → i + U+0307) is
/// reproduced identically by char::to_lowercase before the lookup.
/// Regenerate with the node snippet in PROGRESS-B (iter-11) if the pin set
/// changes; do not hand-edit.
fn nfd_decompose(c: char) -> Vec<char> {
    static TABLE: &[(u32, &[u32])] = &[
        (0xe0, &[0x61, 0x300]), (0xe1, &[0x61, 0x301]), (0xe2, &[0x61, 0x302]),
        (0xe3, &[0x61, 0x303]), (0xe4, &[0x61, 0x308]), (0xe5, &[0x61, 0x30a]),
        (0xe7, &[0x63, 0x327]), (0xe8, &[0x65, 0x300]), (0xe9, &[0x65, 0x301]),
        (0xea, &[0x65, 0x302]), (0xeb, &[0x65, 0x308]), (0xec, &[0x69, 0x300]),
        (0xed, &[0x69, 0x301]), (0xee, &[0x69, 0x302]), (0xef, &[0x69, 0x308]),
        (0xf1, &[0x6e, 0x303]), (0xf2, &[0x6f, 0x300]), (0xf3, &[0x6f, 0x301]),
        (0xf4, &[0x6f, 0x302]), (0xf5, &[0x6f, 0x303]), (0xf6, &[0x6f, 0x308]),
        (0xf9, &[0x75, 0x300]), (0xfa, &[0x75, 0x301]), (0xfb, &[0x75, 0x302]),
        (0xfc, &[0x75, 0x308]), (0xfd, &[0x79, 0x301]), (0xff, &[0x79, 0x308]),
        (0x101, &[0x61, 0x304]), (0x103, &[0x61, 0x306]), (0x105, &[0x61, 0x328]),
        (0x107, &[0x63, 0x301]), (0x109, &[0x63, 0x302]), (0x10b, &[0x63, 0x307]),
        (0x10d, &[0x63, 0x30c]), (0x10f, &[0x64, 0x30c]), (0x113, &[0x65, 0x304]),
        (0x115, &[0x65, 0x306]), (0x117, &[0x65, 0x307]), (0x119, &[0x65, 0x328]),
        (0x11b, &[0x65, 0x30c]), (0x11d, &[0x67, 0x302]), (0x11f, &[0x67, 0x306]),
        (0x121, &[0x67, 0x307]), (0x123, &[0x67, 0x327]), (0x125, &[0x68, 0x302]),
        (0x129, &[0x69, 0x303]), (0x12b, &[0x69, 0x304]), (0x12d, &[0x69, 0x306]),
        (0x12f, &[0x69, 0x328]), (0x135, &[0x6a, 0x302]), (0x137, &[0x6b, 0x327]),
        (0x13a, &[0x6c, 0x301]), (0x13c, &[0x6c, 0x327]), (0x13e, &[0x6c, 0x30c]),
        (0x144, &[0x6e, 0x301]), (0x146, &[0x6e, 0x327]), (0x148, &[0x6e, 0x30c]),
        (0x14d, &[0x6f, 0x304]), (0x14f, &[0x6f, 0x306]), (0x151, &[0x6f, 0x30b]),
        (0x155, &[0x72, 0x301]), (0x157, &[0x72, 0x327]), (0x159, &[0x72, 0x30c]),
        (0x15b, &[0x73, 0x301]), (0x15d, &[0x73, 0x302]), (0x15f, &[0x73, 0x327]),
        (0x161, &[0x73, 0x30c]), (0x163, &[0x74, 0x327]), (0x165, &[0x74, 0x30c]),
        (0x169, &[0x75, 0x303]), (0x16b, &[0x75, 0x304]), (0x16d, &[0x75, 0x306]),
        (0x16f, &[0x75, 0x30a]), (0x171, &[0x75, 0x30b]), (0x173, &[0x75, 0x328]),
        (0x175, &[0x77, 0x302]), (0x177, &[0x79, 0x302]), (0x17a, &[0x7a, 0x301]),
        (0x17c, &[0x7a, 0x307]), (0x17e, &[0x7a, 0x30c]), (0x1a1, &[0x6f, 0x31b]),
        (0x1b0, &[0x75, 0x31b]), (0x1ce, &[0x61, 0x30c]), (0x1d0, &[0x69, 0x30c]),
        (0x1d2, &[0x6f, 0x30c]), (0x1d4, &[0x75, 0x30c]), (0x1d6, &[0x75, 0x308, 0x304]),
        (0x1d8, &[0x75, 0x308, 0x301]), (0x1da, &[0x75, 0x308, 0x30c]), (0x1dc, &[0x75, 0x308, 0x300]),
        (0x1df, &[0x61, 0x308, 0x304]), (0x1e1, &[0x61, 0x307, 0x304]), (0x1e7, &[0x67, 0x30c]),
        (0x1e9, &[0x6b, 0x30c]), (0x1eb, &[0x6f, 0x328]), (0x1ed, &[0x6f, 0x328, 0x304]),
        (0x1f0, &[0x6a, 0x30c]), (0x1f5, &[0x67, 0x301]), (0x1f9, &[0x6e, 0x300]),
        (0x1fb, &[0x61, 0x30a, 0x301]), (0x201, &[0x61, 0x30f]), (0x203, &[0x61, 0x311]),
        (0x205, &[0x65, 0x30f]), (0x207, &[0x65, 0x311]), (0x209, &[0x69, 0x30f]),
        (0x20b, &[0x69, 0x311]), (0x20d, &[0x6f, 0x30f]), (0x20f, &[0x6f, 0x311]),
        (0x211, &[0x72, 0x30f]), (0x213, &[0x72, 0x311]), (0x215, &[0x75, 0x30f]),
        (0x217, &[0x75, 0x311]), (0x219, &[0x73, 0x326]), (0x21b, &[0x74, 0x326]),
        (0x21f, &[0x68, 0x30c]), (0x227, &[0x61, 0x307]), (0x229, &[0x65, 0x327]),
        (0x22b, &[0x6f, 0x308, 0x304]), (0x22d, &[0x6f, 0x303, 0x304]), (0x22f, &[0x6f, 0x307]),
        (0x231, &[0x6f, 0x307, 0x304]), (0x233, &[0x79, 0x304]), (0x1e01, &[0x61, 0x325]),
        (0x1e03, &[0x62, 0x307]), (0x1e05, &[0x62, 0x323]), (0x1e07, &[0x62, 0x331]),
        (0x1e09, &[0x63, 0x327, 0x301]), (0x1e0b, &[0x64, 0x307]), (0x1e0d, &[0x64, 0x323]),
        (0x1e0f, &[0x64, 0x331]), (0x1e11, &[0x64, 0x327]), (0x1e13, &[0x64, 0x32d]),
        (0x1e15, &[0x65, 0x304, 0x300]), (0x1e17, &[0x65, 0x304, 0x301]), (0x1e19, &[0x65, 0x32d]),
        (0x1e1b, &[0x65, 0x330]), (0x1e1d, &[0x65, 0x327, 0x306]), (0x1e1f, &[0x66, 0x307]),
        (0x1e21, &[0x67, 0x304]), (0x1e23, &[0x68, 0x307]), (0x1e25, &[0x68, 0x323]),
        (0x1e27, &[0x68, 0x308]), (0x1e29, &[0x68, 0x327]), (0x1e2b, &[0x68, 0x32e]),
        (0x1e2d, &[0x69, 0x330]), (0x1e2f, &[0x69, 0x308, 0x301]), (0x1e31, &[0x6b, 0x301]),
        (0x1e33, &[0x6b, 0x323]), (0x1e35, &[0x6b, 0x331]), (0x1e37, &[0x6c, 0x323]),
        (0x1e39, &[0x6c, 0x323, 0x304]), (0x1e3b, &[0x6c, 0x331]), (0x1e3d, &[0x6c, 0x32d]),
        (0x1e3f, &[0x6d, 0x301]), (0x1e41, &[0x6d, 0x307]), (0x1e43, &[0x6d, 0x323]),
        (0x1e45, &[0x6e, 0x307]), (0x1e47, &[0x6e, 0x323]), (0x1e49, &[0x6e, 0x331]),
        (0x1e4b, &[0x6e, 0x32d]), (0x1e4d, &[0x6f, 0x303, 0x301]), (0x1e4f, &[0x6f, 0x303, 0x308]),
        (0x1e51, &[0x6f, 0x304, 0x300]), (0x1e53, &[0x6f, 0x304, 0x301]), (0x1e55, &[0x70, 0x301]),
        (0x1e57, &[0x70, 0x307]), (0x1e59, &[0x72, 0x307]), (0x1e5b, &[0x72, 0x323]),
        (0x1e5d, &[0x72, 0x323, 0x304]), (0x1e5f, &[0x72, 0x331]), (0x1e61, &[0x73, 0x307]),
        (0x1e63, &[0x73, 0x323]), (0x1e65, &[0x73, 0x301, 0x307]), (0x1e67, &[0x73, 0x30c, 0x307]),
        (0x1e69, &[0x73, 0x323, 0x307]), (0x1e6b, &[0x74, 0x307]), (0x1e6d, &[0x74, 0x323]),
        (0x1e6f, &[0x74, 0x331]), (0x1e71, &[0x74, 0x32d]), (0x1e73, &[0x75, 0x324]),
        (0x1e75, &[0x75, 0x330]), (0x1e77, &[0x75, 0x32d]), (0x1e79, &[0x75, 0x303, 0x301]),
        (0x1e7b, &[0x75, 0x304, 0x308]), (0x1e7d, &[0x76, 0x303]), (0x1e7f, &[0x76, 0x323]),
        (0x1e81, &[0x77, 0x300]), (0x1e83, &[0x77, 0x301]), (0x1e85, &[0x77, 0x308]),
        (0x1e87, &[0x77, 0x307]), (0x1e89, &[0x77, 0x323]), (0x1e8b, &[0x78, 0x307]),
        (0x1e8d, &[0x78, 0x308]), (0x1e8f, &[0x79, 0x307]), (0x1e91, &[0x7a, 0x302]),
        (0x1e93, &[0x7a, 0x323]), (0x1e95, &[0x7a, 0x331]), (0x1e96, &[0x68, 0x331]),
        (0x1e97, &[0x74, 0x308]), (0x1e98, &[0x77, 0x30a]), (0x1e99, &[0x79, 0x30a]),
        (0x1ea1, &[0x61, 0x323]), (0x1ea3, &[0x61, 0x309]), (0x1ea5, &[0x61, 0x302, 0x301]),
        (0x1ea7, &[0x61, 0x302, 0x300]), (0x1ea9, &[0x61, 0x302, 0x309]), (0x1eab, &[0x61, 0x302, 0x303]),
        (0x1ead, &[0x61, 0x323, 0x302]), (0x1eaf, &[0x61, 0x306, 0x301]), (0x1eb1, &[0x61, 0x306, 0x300]),
        (0x1eb3, &[0x61, 0x306, 0x309]), (0x1eb5, &[0x61, 0x306, 0x303]), (0x1eb7, &[0x61, 0x323, 0x306]),
        (0x1eb9, &[0x65, 0x323]), (0x1ebb, &[0x65, 0x309]), (0x1ebd, &[0x65, 0x303]),
        (0x1ebf, &[0x65, 0x302, 0x301]), (0x1ec1, &[0x65, 0x302, 0x300]), (0x1ec3, &[0x65, 0x302, 0x309]),
        (0x1ec5, &[0x65, 0x302, 0x303]), (0x1ec7, &[0x65, 0x323, 0x302]), (0x1ec9, &[0x69, 0x309]),
        (0x1ecb, &[0x69, 0x323]), (0x1ecd, &[0x6f, 0x323]), (0x1ecf, &[0x6f, 0x309]),
        (0x1ed1, &[0x6f, 0x302, 0x301]), (0x1ed3, &[0x6f, 0x302, 0x300]), (0x1ed5, &[0x6f, 0x302, 0x309]),
        (0x1ed7, &[0x6f, 0x302, 0x303]), (0x1ed9, &[0x6f, 0x323, 0x302]), (0x1edb, &[0x6f, 0x31b, 0x301]),
        (0x1edd, &[0x6f, 0x31b, 0x300]), (0x1edf, &[0x6f, 0x31b, 0x309]), (0x1ee1, &[0x6f, 0x31b, 0x303]),
        (0x1ee3, &[0x6f, 0x31b, 0x323]), (0x1ee5, &[0x75, 0x323]), (0x1ee7, &[0x75, 0x309]),
        (0x1ee9, &[0x75, 0x31b, 0x301]), (0x1eeb, &[0x75, 0x31b, 0x300]), (0x1eed, &[0x75, 0x31b, 0x309]),
        (0x1eef, &[0x75, 0x31b, 0x303]), (0x1ef1, &[0x75, 0x31b, 0x323]), (0x1ef3, &[0x79, 0x300]),
        (0x1ef5, &[0x79, 0x323]), (0x1ef7, &[0x79, 0x309]), (0x1ef9, &[0x79, 0x303]),
    ];
    match TABLE.binary_search_by(|(k, _)| (*k as i64).cmp(&(c as i64))) {
        Ok(i) => TABLE[i].1.iter().map(|&x| char::from_u32(x).unwrap_or(x as u8 as char)).collect(),
        Err(_) => vec![c],
    }
}

/// \p{Diacritic} (Unicode 17.0, as implemented by the Node runtime —
/// includes the spacing-marks oddities like U+00B7/U+005E that JS strips to
/// NOTHING, not a separator, and excludes in-range non-Diacritics such as
/// U+034F/U+0363–036F that survive to become separators). Sorted ranges,
/// generated from the same Node run as the NFD table.
fn is_diacritic(c: char) -> bool {
    static RANGES: &[(u32, u32)] = &[
        (0x5e, 0x5e), (0x60, 0x60), (0xa8, 0xa8), (0xaf, 0xaf),
        (0xb4, 0xb4), (0xb7, 0xb8), (0x2b0, 0x34e), (0x350, 0x357),
        (0x35d, 0x362), (0x374, 0x375), (0x37a, 0x37a), (0x384, 0x385),
        (0x483, 0x487), (0x559, 0x559), (0x591, 0x5bd), (0x5bf, 0x5bf),
        (0x5c1, 0x5c2), (0x5c4, 0x5c5), (0x5c7, 0x5c7), (0x64b, 0x652),
        (0x657, 0x658), (0x6df, 0x6e0), (0x6e5, 0x6e6), (0x6ea, 0x6ec),
        (0x730, 0x74a), (0x7a6, 0x7b0), (0x7eb, 0x7f5), (0x818, 0x819),
        (0x898, 0x89f), (0x8c9, 0x8d2), (0x8e3, 0x8fe), (0x93c, 0x93c),
        (0x94d, 0x94d), (0x951, 0x954), (0x971, 0x971), (0x9bc, 0x9bc),
        (0x9cd, 0x9cd), (0xa3c, 0xa3c), (0xa4d, 0xa4d), (0xabc, 0xabc),
        (0xacd, 0xacd), (0xafd, 0xaff), (0xb3c, 0xb3c), (0xb4d, 0xb4d),
        (0xb55, 0xb55), (0xbcd, 0xbcd), (0xc3c, 0xc3c), (0xc4d, 0xc4d),
        (0xcbc, 0xcbc), (0xccd, 0xccd), (0xd3b, 0xd3c), (0xd4d, 0xd4d),
        (0xdca, 0xdca), (0xe3a, 0xe3a), (0xe47, 0xe4c), (0xe4e, 0xe4e),
        (0xeba, 0xeba), (0xec8, 0xecc), (0xf18, 0xf19), (0xf35, 0xf35),
        (0xf37, 0xf37), (0xf39, 0xf39), (0xf3e, 0xf3f), (0xf82, 0xf84),
        (0xf86, 0xf87), (0xfc6, 0xfc6), (0x1037, 0x1037), (0x1039, 0x103a),
        (0x1063, 0x1064), (0x1069, 0x106d), (0x1087, 0x108d), (0x108f, 0x108f),
        (0x109a, 0x109b), (0x135d, 0x135f), (0x1714, 0x1715), (0x1734, 0x1734),
        (0x17c9, 0x17d3), (0x17dd, 0x17dd), (0x1939, 0x193b), (0x1a60, 0x1a60),
        (0x1a75, 0x1a7c), (0x1a7f, 0x1a7f), (0x1ab0, 0x1abe), (0x1ac1, 0x1acb),
        (0x1acf, 0x1add), (0x1ae0, 0x1aeb), (0x1b34, 0x1b34), (0x1b44, 0x1b44),
        (0x1b6b, 0x1b73), (0x1baa, 0x1bab), (0x1be6, 0x1be6), (0x1bf2, 0x1bf3),
        (0x1c36, 0x1c37), (0x1c78, 0x1c7d), (0x1cd0, 0x1ce8), (0x1ced, 0x1ced),
        (0x1cf4, 0x1cf4), (0x1cf7, 0x1cf9), (0x1d2c, 0x1d6a), (0x1d9b, 0x1dbe),
        (0x1dc4, 0x1dcf), (0x1df5, 0x1dff), (0x1fbd, 0x1fbd), (0x1fbf, 0x1fc1),
        (0x1fcd, 0x1fcf), (0x1fdd, 0x1fdf), (0x1fed, 0x1fef), (0x1ffd, 0x1ffe),
        (0x2cef, 0x2cf1), (0x2e2f, 0x2e2f), (0x302a, 0x302f), (0x3099, 0x309c),
        (0x30fc, 0x30fc), (0xa66f, 0xa66f), (0xa67c, 0xa67d), (0xa67f, 0xa67f),
        (0xa69c, 0xa69d), (0xa6f0, 0xa6f1), (0xa700, 0xa721), (0xa788, 0xa78a),
        (0xa7f1, 0xa7f1), (0xa7f8, 0xa7f9), (0xa806, 0xa806), (0xa82c, 0xa82c),
        (0xa8c4, 0xa8c4), (0xa8e0, 0xa8f1), (0xa92b, 0xa92e), (0xa953, 0xa953),
        (0xa9b3, 0xa9b3), (0xa9c0, 0xa9c0), (0xa9e5, 0xa9e5), (0xaa7b, 0xaa7d),
        (0xaabf, 0xaac2), (0xaaf6, 0xaaf6), (0xab5b, 0xab5f), (0xab69, 0xab6b),
        (0xabec, 0xabed), (0xfb1e, 0xfb1e), (0xfe20, 0xfe2f), (0xff3e, 0xff3e),
        (0xff40, 0xff40), (0xff70, 0xff70), (0xff9e, 0xff9f), (0xffe3, 0xffe3),
        (0x102e0, 0x102e0), (0x10780, 0x10785), (0x10787, 0x107b0), (0x107b2, 0x107ba),
        (0x10a38, 0x10a3a), (0x10a3f, 0x10a3f), (0x10ae5, 0x10ae6), (0x10d22, 0x10d27),
        (0x10d4e, 0x10d4e), (0x10d69, 0x10d6d), (0x10efa, 0x10efa), (0x10efd, 0x10eff),
        (0x10f46, 0x10f50), (0x10f82, 0x10f85), (0x11046, 0x11046), (0x11070, 0x11070),
        (0x110b9, 0x110ba), (0x11133, 0x11134), (0x11173, 0x11173), (0x111c0, 0x111c0),
        (0x111ca, 0x111cc), (0x11235, 0x11236), (0x112e9, 0x112ea), (0x1133b, 0x1133c),
        (0x1134d, 0x1134d), (0x11366, 0x1136c), (0x11370, 0x11374), (0x113ce, 0x113d0),
        (0x113d2, 0x113d3), (0x113e1, 0x113e2), (0x11442, 0x11442), (0x11446, 0x11446),
        (0x114c2, 0x114c3), (0x115bf, 0x115c0), (0x1163f, 0x1163f), (0x116b6, 0x116b7),
        (0x1172b, 0x1172b), (0x11839, 0x1183a), (0x1193d, 0x1193e), (0x11943, 0x11943),
        (0x119e0, 0x119e0), (0x11a34, 0x11a34), (0x11a47, 0x11a47), (0x11a99, 0x11a99),
        (0x11c3f, 0x11c3f), (0x11d42, 0x11d42), (0x11d44, 0x11d45), (0x11d97, 0x11d97),
        (0x11dd9, 0x11dd9), (0x11f41, 0x11f42), (0x11f5a, 0x11f5a), (0x13447, 0x13455),
        (0x1612f, 0x1612f), (0x16af0, 0x16af4), (0x16b30, 0x16b36), (0x16d6b, 0x16d6c),
        (0x16f8f, 0x16f9f), (0x16ff0, 0x16ff1), (0x1aff0, 0x1aff3), (0x1aff5, 0x1affb),
        (0x1affd, 0x1affe), (0x1cf00, 0x1cf2d), (0x1cf30, 0x1cf46), (0x1d167, 0x1d169),
        (0x1d16d, 0x1d172), (0x1d17b, 0x1d182), (0x1d185, 0x1d18b), (0x1d1aa, 0x1d1ad),
        (0x1e030, 0x1e06d), (0x1e130, 0x1e136), (0x1e2ae, 0x1e2ae), (0x1e2ec, 0x1e2ef),
        (0x1e5ee, 0x1e5ef), (0x1e8d0, 0x1e8d6), (0x1e944, 0x1e946), (0x1e948, 0x1e94a),
    ];
    let cp = c as u32;
    match RANGES.partition_point(|&(_, hi)| hi < cp) {
        i if i < RANGES.len() && RANGES[i].0 <= cp => true,
        _ => false,
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
    if let Some(hit) = cotd_cache().guard().0.get(&day) {
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
        let mut c = cotd_cache().guard();
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

use crate::util::now_ms;

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
    fn slug_matches_node_full_corpus() {
        // Node-pinned corpus: every non-ASCII card name in the e2e DB copy
        // plus adversarial shapes (stroke letters with NO canonical
        // decomposition — ı Đđ Ħħ Łł Ŋŋ Ŧŧ — which Node drops to a
        // separator; generated with node against
        // backend/src/utils/edhrecSynergy.js commanderSlug). The earlier
        // 10-case vector let cards_meta's NFD table ship fake decompositions
        // for the stroke letters (U+0131 → "i" etc.); these pin the fix.
        let vectors: &[(&str, &str)] = &[
            ("Adéwalé, Breaker of Chains", "adewale-breaker-of-chains"),
            ("Adéwalé, Breaker of Chains // Adéwalé, Breaker of Chains", "adewale-breaker-of-chains"),
            ("Altaïr Ibn-La'Ahad", "altair-ibn-laahad"),
            ("Andúril, Flame of the West", "anduril-flame-of-the-west"),
            ("Andúril, Narsil Reforged", "anduril-narsil-reforged"),
            ("Araña, Heart of the Spider", "arana-heart-of-the-spider"),
            ("Arna Kennerüd, Skycaptain", "arna-kennerud-skycaptain"),
            ("Arwen Undómiel", "arwen-undomiel"),
            ("Aveline de Grandpré", "aveline-de-grandpre"),
            ("Barad-dûr", "barad-dur"),
            ("Bartolomé del Presidio", "bartolome-del-presidio"),
            ("Bespoke Bō", "bespoke-bo"),
            ("Bösium Strip", "bosium-strip"),
            ("Chicken à la King", "chicken-a-la-king"),
            ("Clavileño, First of the Blessed", "clavileno-first-of-the-blessed"),
            ("Círdan the Shipwright", "cirdan-the-shipwright"),
            ("Dandân", "dandan"),
            ("Dáin Ironfoot", "dain-ironfoot"),
            ("Dáin of the Ancient Halls", "dain-of-the-ancient-halls"),
            ("Dáin's Company", "dains-company"),
            ("Dáin, Lord of the Iron Hills", "dain-lord-of-the-iron-hills"),
            ("Déjà Vu", "deja-vu"),
            ("Dúnedain Blade", "dunedain-blade"),
            ("Dúnedain Rangers", "dunedain-rangers"),
            ("El-Hajjâj", "el-hajjaj"),
            ("Everett K. Ross, Hapless Attaché", "everett-k-ross-hapless-attache"),
            ("Fíli and Kíli, Joyous", "fili-and-kili-joyous"),
            ("Fíli the Pathfinder", "fili-the-pathfinder"),
            ("Galadriel of Lothlórien", "galadriel-of-lothlorien"),
            ("Gilraen, Dúnedain Protector", "gilraen-dunedain-protector"),
            ("Glóin the Mighty // Easy Pickings", "gloin-the-mighty"),
            ("Glóin, Dwarf Emissary", "gloin-dwarf-emissary"),
            ("Grishnákh, Brash Instigator", "grishnakh-brash-instigator"),
            ("Gríma, Saruman's Footman", "grima-sarumans-footman"),
            ("Hamato Ninpō", "hamato-ninpo"),
            ("Ifh-Bíff Efreet", "ifh-biff-efreet"),
            ("Khârn the Betrayer", "kharn-the-betrayer"),
            ("Kíli the Resourceful", "kili-the-resourceful"),
            ("Lim-Dûl the Necromancer", "lim-dul-the-necromancer"),
            ("Lim-Dûl's Hex", "lim-duls-hex"),
            ("Lim-Dûl's Paladin", "lim-duls-paladin"),
            ("Lim-Dûl's Vault", "lim-duls-vault"),
            ("Lord of the Nazgûl", "lord-of-the-nazgul"),
            ("Lothlórien Blade", "lothlorien-blade"),
            ("Lórien Revealed", "lorien-revealed"),
            ("Mauhúr, Uruk-hai Captain", "mauhur-uruk-hai-captain"),
            ("Mists of Lórien", "mists-of-lorien"),
            ("Mjölnir's Might", "mjolnirs-might"),
            ("Mjölnir, Hammer of Thor", "mjolnir-hammer-of-thor"),
            ("Nazgûl", "nazgul"),
            ("Nazgûl Battle-Mace", "nazgul-battle-mace"),
            ("Oath of Lim-Dûl", "oath-of-lim-dul"),
            ("Olórin's Searing Light", "olorins-searing-light"),
            ("Palantír of Orthanc", "palantir-of-orthanc"),
            ("Rasaad, Monk of Selûne", "rasaad-monk-of-selune"),
            ("Ratonhnhaké꞉ton", "ratonhnhaketon"),
            ("Robo-Piñata", "robo-pinata"),
            ("Sauté", "saute"),
            ("Sméagol, Helpful Guide", "smeagol-helpful-guide"),
            ("Song of Eärendil", "song-of-earendil"),
            ("Soothing of Sméagol", "soothing-of-smeagol"),
            ("Surtr, Fiery Jötun", "surtr-fiery-jotun"),
            ("Séance Board", "seance-board"),
            ("The Balrog, Flame of Udûn", "the-balrog-flame-of-udun"),
            ("The Ultimate Nightmare of Wizards of the Coast® Customer Service", "the-ultimate-nightmare-of-wizards-of-the-coast-customer-service"),
            ("Thrór's Map", "thrors-map"),
            ("Théoden, King of Rohan", "theoden-king-of-rohan"),
            ("Troll of Khazad-dûm", "troll-of-khazad-dum"),
            ("Uglúk of the White Hand", "ugluk-of-the-white-hand"),
            ("Võx, Child of the Collective", "vox-child-of-the-collective"),
            ("Éomer of the Riddermark", "eomer-of-the-riddermark"),
            ("Éomer, King of Rohan", "eomer-king-of-rohan"),
            ("Éomer, Marshal of Rohan", "eomer-marshal-of-rohan"),
            ("Éowyn, Fearless Knight", "eowyn-fearless-knight"),
            ("Éowyn, Lady of Rohan", "eowyn-lady-of-rohan"),
            ("Éowyn, Shieldmaiden", "eowyn-shieldmaiden"),
            ("Óin the Brave", "oin-the-brave"),
            ("Tıbir Tamı", "t-bir-tam"),
            ("Đavid", "avid"),
            ("đ", ""),
            ("Ħama", "ama"),
            ("ħ", ""),
            ("Łegend", "egend"),
            ("ł", ""),
            ("Ŋakira", "akira"),
            ("ŋ", ""),
            ("Ŧor", "or"),
            ("ŧ", ""),
            ("İstanbul", "istanbul"),
            ("Æthersprens", "thersprens"),
            ("ß", ""),
            ("Giseïla, the Broken Blade", "giseila-the-broken-blade"),
            ("Ür, Fëanor’s", "ur-feanors"),
            ("Mox ± Diamond", "mox-diamond"),
            ("Serra Ângel ™", "serra-angel"),
            ("A // B", "a"),
            ("  Trim  Me  ", "trim-me"),
            ("---", ""),
            ("", ""),
            ("K’rrík, Son of Àndàn", "krrik-son-of-andan"),
            ("Zoë, Player One", "zoe-player-one"),
            ("Llanowar Auger", "llanowar-auger"),
            ("Crucible of Worlds", "crucible-of-worlds"),
            ("Ωmega", "mega"),
            ("Ω", ""),
            ("Naïve", "naive"),
            ("Bosh, Iron Golem", "bosh-iron-golem"),
            ("Léon", "leon"),
            ("ÁB", "ab"),
            ("a̧", "a"),
            ("M’uıdart", "mu-dart"),
            ("War™", "war"),
            ("C.A.M.P.", "c-a-m-p"), // dots/commas are separators — only ['’] is dropped outright
            ("R.A.X.O.S.", "r-a-x-o-s"),
        ];
        for (name, want) in vectors {
            assert_eq!(&edhrec_slug(name), want, "edhrec_slug({name:?})");
        }
    }

    /// Full-DB differential harness (lane-B iter-11): SCRYBOX_SLUG_CORPUS
    /// points at a TSV of `name\texpected` generated with node from
    /// backend/src/utils/edhrecSynergy.js commanderSlug over EVERY distinct
    /// card_cache.name in a DB copy. Skipped unless the env var is set, so
    /// the committed suite stays hermetic; run:
    ///   node -e '…dump TSV…' && SCRYBOX_SLUG_CORPUS=/path cargo test --lib slug_matches_node_db_corpus_full
    #[test]
    fn slug_matches_node_db_corpus_full() {
        let Ok(path) = std::env::var("SCRYBOX_SLUG_CORPUS") else {
            eprintln!("SCRYBOX_SLUG_CORPUS unset — skipping full-DB slug diff");
            return;
        };
        let data = std::fs::read_to_string(&path).expect("corpus file");
        let mut n = 0;
        for line in data.lines() {
            let (name, want) = line.split_once('\t').expect("TSV line");
            assert_eq!(&edhrec_slug(name), want, "edhrec_slug({name:?})");
            n += 1;
        }
        assert!(n > 1000, "corpus too small ({n}) — wrong file?");
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

//! LANE B unit 6a — cardscan group (route switch `cardscan`).
//! Twin of `backend/src/routes/cardscan.js`: the reverse proxy onto the
//! OCR sidecar (CARDSCAN_URL, axios instance: timeout 20s,
//! validateStatus-always, 40MB caps) plus the card_cache hydration legs.
//!
//! Node order per route: auth (global mount) → bigJson outer parser (the
//! cardscan paths are in largeJsonPaths, so the OUTER express.json skips
//! non-JSON content-types) → route-level parser (express.raw image/jpeg|
//! octet-stream 15mb on detect/frame; express.json 512kb json|text/plain on
//! telemetry; the plain JSON arms read the outer-parsed req.body) → handler.
//!
//! Consequences reproduced here:
//!   - detect/frame with a JSON content-type never reaches the raw parser
//!     (the outer express.json already consumed the stream): 400
//!     {ok:false,error:'Missing image'} regardless of size;
//!   - telemetry only parses json/text/plain; any other type arrives with
//!     req.body = {} → 400 'No records' (env-gated 204 first if SCAN_TELEMETRY=0);
//!   - every upstream axios failure maps through upstreamError: ECONNREFUSED
//!     /ENOTFOUND/EAI_AGAIN → 503 {ok:false,unavailable:true,error:'Scanner
//!     service is not running'}, anything else (timeout, bad gateway JSON)
//!     → 502 {ok:false,unavailable:false,error:'Scanner service error'}.
//!
//! The frame leg's AbortController-on-client-close is a documented
//! divergence: axum gives no writable-side close signal here, so Rust lets
//! the sidecar finish the frame (bytes identical, only wasted sidecar work
//! on an abandoned phone differs).
//!
//! Hydration: `getCardById` is the shared card_cache read (mtg- prefixed
//! ids). A row the DB does not hold would send Node to the Scryfall fetch
//! lane, so the WHOLE request is forwarded to Node instead of half-answering
//! (Forward never touches the sidecar — the decision is made on the already-
//! fetched sidecar reply, and Node re-runs the leg itself).

use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use rusqlite::{types::Value as SqlValue, Connection};
use serde_json::{Map, Value};

use crate::auth::{authenticate, bearer_token};
use crate::cards_tags::{json_error, read_body};
use crate::collection::{body_etag, etag_matches};
use crate::collection_writes::{js_to_string, node_exact};
use crate::decks_reads::parse_card_row;
use crate::AppState;

/// `const BASE = (process.env.CARDSCAN_URL || 'http://cardscan:8321')
/// .replace(/\/+$/, '')` — module-load snapshot, same here.
fn base_url() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| {
        let raw = std::env::var("CARDSCAN_URL")
            .ok()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "http://cardscan:8321".to_string());
        // Node: .replace(/\/+$/, '') — strip AT MOST one trailing slash.
        raw.strip_suffix('/').map(|s| s.to_string()).unwrap_or(raw)
    })
    .as_str()
}

const TIMEOUT: Duration = Duration::from_secs(20); // axios instance timeout

type JsonClient = Client<HttpConnector, String>;
type BytesClient = Client<HttpConnector, http_body_util::Full<axum::body::Bytes>>;

fn json_client() -> &'static JsonClient {
    static C: OnceLock<Arc<JsonClient>> = OnceLock::new();
    C.get_or_init(|| Arc::new(Client::builder(hyper_util::rt::TokioExecutor::new()).build_http::<String>()))
        .as_ref()
}

fn bytes_client() -> &'static BytesClient {
    static C: OnceLock<Arc<BytesClient>> = OnceLock::new();
    C.get_or_init(|| Arc::new(Client::builder(hyper_util::rt::TokioExecutor::new()).build_http::<http_body_util::Full<axum::body::Bytes>>()))
        .as_ref()
}

// ---------------------------------------------------------------------------
// axios twins
// ---------------------------------------------------------------------------

/// Outcome of an upstream call. `Down` = the ECONNREFUSED/ENOTFOUND/EAI_AGAIN
/// family (503 arm); `Failed` = every other axios rejection (502 arm);
/// `Resp` = a completed exchange, status carried through (validateStatus
/// always-true). JSON bodies are parsed the way axios auto-parses
/// application/json; a parse failure is an axios SyntaxError → `Failed`.
enum Up {
    Resp { status: u16, json: Option<Value> },
    Down,
    Failed,
}

/// Node maps axios codes ECONNREFUSED/ENOTFOUND/EAI_AGAIN to the 503 'not
/// running' arm. hyper surfaces the same conditions as a connect error
/// whose IO source says 'Connection refused' or a DNS name-lookup failure.
fn is_down_error_text(t: &str) -> bool {
    let l = t.to_ascii_lowercase();
    l.contains("connection refused") || l.contains("failed to lookup") || l.contains("name or service not known")
        || l.contains("temporary failure in name resolution") || l.contains("no such host")
        || l.contains("econnrefused") || l.contains("enotfound") || l.contains("eai_again")
}

async fn call_json(
    path: &str,
    body: String,
    content_type: &str,
    extra: &[(&str, String)],
    timeout: Duration,
) -> Up {
    let mut builder = hyper::Request::post(format!("{}{}", base_url(), path))
        .header(header::CONTENT_TYPE, content_type);
    for (k, v) in extra {
        if let Ok(hv) = HeaderValue::from_str(v) {
            builder = builder.header(*k, hv);
        }
    }
    let req = match builder.body(body) {
        Ok(r) => r,
        Err(_) => return Up::Failed,
    };
    match tokio::time::timeout(timeout, json_client().request(req)).await {
        Ok(Ok(res)) => finish(res).await,
        Ok(Err(e)) => map_err(&e),
        Err(_) => Up::Failed, // axios timeout → 'timeout of 20000ms exceeded'
    }
}

async fn call_bytes(path: &str, body: Vec<u8>, content_type: &str, extra: &[(&str, String)]) -> Up {
    let mut builder = hyper::Request::post(format!("{}{}", base_url(), path)).header(header::CONTENT_TYPE, content_type);
    for (k, v) in extra {
        if let Ok(hv) = HeaderValue::from_str(v) {
            builder = builder.header(*k, hv);
        }
    }
    let req = match builder.body(http_body_util::Full::from(axum::body::Bytes::from(body))) {
        Ok(r) => r,
        Err(_) => return Up::Failed,
    };
    match tokio::time::timeout(TIMEOUT, bytes_client().request(req)).await {
        Ok(Ok(res)) => finish(res).await,
        Ok(Err(e)) => map_err(&e),
        Err(_) => Up::Failed,
    }
}

async fn call_get(path: &str, timeout: Duration) -> Up {
    let req = match hyper::Request::get(format!("{}{}", base_url(), path)).body(String::new()) {
        Ok(r) => r,
        Err(_) => return Up::Failed,
    };
    match tokio::time::timeout(timeout, json_client().request(req)).await {
        Ok(Ok(res)) => finish(res).await,
        Ok(Err(e)) => map_err(&e),
        Err(_) => Up::Failed,
    }
}

async fn finish(res: hyper::Response<hyper::body::Incoming>) -> Up {
    let status = res.status().as_u16();
    let ctype = res.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let raw = match BodyExt::collect(res.into_body()).await {
        Ok(b) => b.to_bytes().to_vec(),
        Err(_) => return Up::Failed,
    };
    if raw.len() > 40 * 1024 * 1024 {
        return Up::Failed; // maxContentLength
    }
    // axios tryParse() attempts JSON.parse on ANY stringy body (content-type
    // is not consulted); a parse throw surfaces as an axios error → 502 arm.
    let _ = ctype;
    match serde_json::from_slice::<Value>(&raw) {
        Ok(v) => Up::Resp { status, json: Some(v) },
        Err(_) => Up::Failed,
    }
}

fn map_err(e: &hyper_util::client::legacy::Error) -> Up {
    // Walk the source chain: legacy::Error displays as 'client error' and
    // nests the connect::Error -> hyper_util::Error -> io::Error.
    let mut chain = format!("{e}");
    let mut src = std::error::Error::source(e);
    while let Some(s0) = src {
        chain.push_str(": ");
        chain.push_str(&s0.to_string());
        src = s0.source();
    }
    if is_down_error_text(&chain) {
        Up::Down
    } else {
        Up::Failed
    }
}

/// `upstreamError(res, e)` twin.
fn upstream_error(up: &Up) -> Response {
    match up {
        Up::Down => res_json(
            StatusCode::SERVICE_UNAVAILABLE,
            &serde_json::json!({"ok":false,"unavailable":true,"error":"Scanner service is not running"}),
        ),
        _ => res_json(
            StatusCode::BAD_GATEWAY,
            &serde_json::json!({"ok":false,"unavailable":false,"error":"Scanner service error"}),
        ),
    }
}

/// `res.status(r.status).json(r.data)` — res.json re-stringifies the value
/// axios parsed (see res_json); an unparsed body never reaches here
/// (finish() maps a JSON.parse throw to Failed → the 502 arm).
fn passthrough_json(up: &Up) -> Response {
    match up {
        Up::Resp { status, json: Some(v), .. } => res_json(StatusCode::from_u16(*status).unwrap_or(StatusCode::OK), v),
        up => upstream_error(up),
    }
}

// ---------------------------------------------------------------------------
// compression() twin
// ---------------------------------------------------------------------------

/// app-level `compression()` (threshold 1kb, filter compressible('json')) as
/// a route-scoped layer. Node's compression runs OUTSIDE the route: it only
/// touches replies that reach it with a compressible Content-Type and an
/// uncompressed body ≥ threshold; on gzip it adds Content-Encoding +
/// Vary: Accept-Encoding and DELETES Content-Length (node http then chunks —
/// same strip-CL-via-data-stream trick as spa.rs, live-pinned on :3312).
/// ETag stays over the plain entity bytes (express set it before the
/// compressor ran) — verified against the gzipped cards reply on :3312.
pub(crate) async fn compress(req: Request, next: axum::middleware::Next) -> Response {
    let ae = req
        .headers()
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let resp = next.run(req).await;
    let (mut parts, body) = resp.into_parts();
    if parts.headers.contains_key(header::CONTENT_ENCODING) {
        return Response::from_parts(parts, body); // already encoded upstream
    }
    let compressible = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"));
    if !compressible || !ae.as_deref().is_some_and(|a| a.to_ascii_lowercase().contains("gzip")) {
        return Response::from_parts(parts, body);
    }
    let raw = match http_body_util::BodyExt::collect(body).await {
        Ok(c) => c.to_bytes(),
        Err(_) => return Response::from_parts(parts, Body::empty()),
    };
    if raw.len() < 1024 {
        return Response::from_parts(parts, Body::from(raw)); // below threshold, identity
    }
    let gz = crate::collection::gzip_bytes(&raw);
    parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    parts.headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    parts.headers.remove(header::CONTENT_LENGTH);
    let b = Body::from(gz);
    Response::from_parts(parts, Body::from_stream(b.into_data_stream()))
}

// ---------------------------------------------------------------------------
// JS primitives
// ---------------------------------------------------------------------------

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) => true, // JS [] is truthy
        Some(Value::Object(_)) => true,
    }
}

fn scry_id_ok(v: &Value) -> bool {
    // /^[0-9a-f]{8}-...{12}$/i against String(x). String(id||'') semantics:
    // falsy → '' fails the regex anyway.
    let s = js_to_string(Some(v));
    let bytes = s.as_bytes();
    let groups = [8usize, 4, 4, 4, 12];
    let mut i = 0usize;
    for (gi, g) in groups.iter().enumerate() {
        if bytes.len() != 36 {
            return false;
        }
        let start = i;
        for b in &bytes[start..start + g] {
            if !b.is_ascii_hexdigit() {
                return false;
            }
        }
        i += g;
        if gi < 4 {
            if bytes.get(i) != Some(&b'-') {
                return false;
            }
            i += 1;
        }
    }
    i == 36
}

fn sanitize(s: &str, pattern: &dyn Fn(char) -> bool, max: usize) -> String {
    // String(x||'').slice(0,max).replace(/[^\w...]/g,'') — slice BEFORE the
    // filter: the filter can only SHRINK the result, never cap at max.
    let cut: String = s.chars().take(max).collect();
    cut.chars().filter(|c| pattern(*c)).collect()
}

fn is_word_or(c: char, extra: &[char]) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || extra.contains(&c)
}

// ---------------------------------------------------------------------------
// Body parsing twins
// ---------------------------------------------------------------------------

fn content_type(headers: &HeaderMap) -> String {
    headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

fn ct_is(headers: &HeaderMap, ty: &str) -> bool {
    content_type(headers).split(';').next().unwrap_or("").trim().eq_ignore_ascii_case(ty)
}

// Body-parse failures never produce a locally-authored error: Node's own
// express parsers are the authority (entity.too.large is rewritten by the
// global handler at server.js:638 to the 'Upload too large...' 413; strict
// JSON SyntaxErrors render as express's HTML 400 page). Every parse failure
// here therefore FORWARDS the untouched request, byte-for-byte, so Node
// answers exactly as it would if it owned the route.

/// Outer bigJson for JSON content-types (auth already ran). A failure is
/// Forward: let Node run the SAME express parser over the untouched stream
/// — its error body (the entity.too.large rewrite, the strict-mode HTML
/// SyntaxError page) is authoritative and we must not re-author it.
pub(crate) fn parse_outer_json(headers: &HeaderMap, bytes: &[u8]) -> Result<Value, ()> {
    if !ct_is(headers, "application/json") {
        return Ok(Value::Object(Map::new())); // outer skipped; handlers read {}
    }
    if bytes.len() > 15 * 1024 * 1024 {
        return Err(());
    }
    if bytes.is_empty() {
        // body-parser hands an empty body through as {}.
        return Ok(Value::Object(Map::new()));
    }
    // strict:true: only objects/arrays are bodies. Scalars, bare `null` and
    // whitespace-only payloads are SyntaxErrors Node renders itself (HTML
    // 400 page) — verified against the container's body-parser. Forward and
    // let Node answer.
    let v = serde_json::from_slice::<Value>(bytes).map_err(|_| ())?;
    if !v.is_object() && !v.is_array() {
        return Err(());
    }
    Ok(v)
}

/// express.raw({type:['image/jpeg','application/octet-stream'],limit:'15mb'})
/// — returns Some(bytes) only for a RAW-eligible content-type.
fn parse_raw_image(headers: &HeaderMap, bytes: &[u8]) -> Result<Option<Vec<u8>>, ()> {
    let ct = content_type(headers);
    let first = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if first != "image/jpeg" && first != "application/octet-stream" {
        return Ok(None); // outer express.json ate it → req.body not a Buffer
    }
    if bytes.len() > 15 * 1024 * 1024 {
        // raw's limit error has NO type:'entity.too.large', so Node's global
        // handler falls through to 500 {error:'Internal server error'} —
        // Forward, Node renders it.
        return Err(());
    }
    Ok(Some(bytes.to_vec()))
}

/// express.json({limit:'512kb', type:['application/json','text/plain']}).
fn parse_telemetry_body(headers: &HeaderMap, bytes: &[u8]) -> Result<Value, ()> {
    if !ct_is(headers, "application/json") && !ct_is(headers, "text/plain") {
        return Ok(Value::Object(Map::new()));
    }
    if bytes.len() > 512 * 1024 {
        return Err(()); // entity.too.large → global 413 rewrite, Node's job
    }
    if bytes.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    // strict:true — scalars/whitespace-only are Node's own parse failures.
    let v = serde_json::from_slice::<Value>(bytes).map_err(|_| ())?;
    if !v.is_object() && !v.is_array() {
        return Err(());
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Hydration — hydrate() / hydrateChoices()
// ---------------------------------------------------------------------------

/// `fetch_on_miss`: scan/frame must Forward the whole request when the
/// card row is absent (Node's getCardById would resolve it through the
/// Scryfall lane); the /cards arm passes false — there Node's own code
/// reaches no fetch and the 'not in the card database yet' row is exact.
fn get_card_by_id(conn: &Connection, card_id: &str) -> Result<Option<Map<String, Value>>, rusqlite::Error> {
    let row = crate::collection_writes::get_row_map(
        conn,
        "SELECT * FROM card_cache WHERE id = ?1",
        &[SqlValue::Text(card_id.to_string())],
    )
    .map_err(|_| rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(1), None))?;
    Ok(row.and_then(|r| parse_card_row(&r).ok()))
}

/// Any card row this install's DB cannot answer would send Node to the
/// Scryfall fetch lane (and its cacheCards write): whole-request Forward.
/// Throw mirrors a JS TypeError inside hydrate (null element property
/// access), which the route's try/catch turns into upstreamError's 502 arm.
enum Hydrate {
    Done(Map<String, Value>),
    Forward,
    Throw,
}

/// `(x || [])` then `.map(...)` — a truthy non-array makes .map throw a
/// TypeError inside the route try → 502 (upstream_error's generic arm).
fn js_or_array(data: &Value, key: &str) -> Result<Vec<Value>, ()> {
    match data.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => Ok(a.clone()),
        Some(v) if !truthy(Some(v)) => Ok(Vec::new()),
        _ => Err(()),
    }
}

/// `fetch_on_miss`: scan/frame must Forward the whole request when the
/// card row is absent (Node's getCardById would resolve it through the
/// Scryfall lane); the /cards arm passes false — there Node's own code
/// reaches no fetch and the 'not in the card database yet' row is exact.
fn hydrate(conn: &Connection, result: &Value, fetch_on_miss: bool) -> Result<Hydrate, rusqlite::Error> {
    if result.is_null() {
        // Node: result.scene_number on null throws TypeError → route catch
        // → upstreamError (502). Non-object non-null values are harmless:
        // every property read yields undefined and the out row is all
        // drops/nulls, which the regular path below reproduces.
        return Ok(Hydrate::Throw);
    }
    let g = |k: &str| result.get(k);
    let ok_flag = truthy(g("ok"));
    // JS key order: number, ok, error, title, via, card, choices — but
    // JSON.stringify DROPS undefined values: out.number exists only when
    // the source carries scene_number, out.error only when !ok and the
    // source carries error (the miss-branch below assigns a string, so it
    // still lands at its literal position).
    let number = g("scene_number").cloned();
    let mut ok_out = ok_flag;
    let mut error = if ok_flag { None } else { g("error").cloned() };
    let title = match g("title") {
        Some(v) if truthy(Some(v)) => v.clone(),
        _ => result
            .get("identified")
            .and_then(|i| i.get("name"))
            .cloned()
            .filter(|v| truthy(Some(v)))
            .unwrap_or(Value::Null),
    };
    let via = result
        .get("footer_ocr")
        .and_then(|f| f.get("resolved_by"))
        .or_else(|| result.get("identified").and_then(|i| i.get("via")))
        .cloned()
        .filter(|v| truthy(Some(v)))
        .unwrap_or(Value::Null);

    // const sid = result.ok && result.card?.id
    let sid = match (ok_flag, result.get("card").and_then(|c| c.get("id"))) {
        (true, Some(v)) if truthy(Some(v)) => Some(js_to_string(Some(v))),
        _ => None,
    };
    if let Some(sid) = sid {
        match get_card_by_id(conn, &format!("mtg-{sid}"))? {
            Some(card) => {
                let mut out = finish_hydrate(number, ok_out, error.clone(), title.clone(), via.clone(), None, None);
                out.insert("card".into(), Value::Object(card));
                return Ok(Hydrate::Done(out));
            }
            None => {
                if fetch_on_miss {
                    return Ok(Hydrate::Forward);
                }
                ok_out = false;
                error = Some(Value::String("printing not in the card database yet".into()));
            }
        }
    }
    // footer twins → choices
    let choices_arr: Option<Vec<Value>> = if !ok_flag {
        result
            .get("footer_ocr")
            .and_then(|f| f.get("choices"))
            .cloned()
            .filter(|c| c.is_array())
            .map(|c| c.as_array().unwrap().clone())
    } else {
        None
    };
    let mut cards: Option<Vec<Value>> = None;
    if let Some(choices) = choices_arr {
        if choices.len() > 1 {
            if choices.iter().any(|c| c.is_null()) {
                // Node: c.id on null throws TypeError → 502 arm.
                return Ok(Hydrate::Throw);
            }
            let ids: Vec<String> = choices
                .iter()
                .map(|c| {
                    // c.id || c.scryfallId — TRUTHY-first, not present-first.
                    match c.get("id") {
                        Some(v) if truthy(Some(v)) => v.clone(),
                        _ => c.get("scryfallId").cloned().unwrap_or(Value::Null),
                    }
                })
                .map(|v| js_to_string(Some(&v)))
                .collect();
            let need = hydrate_cards_forward(conn, &ids)?;
            match need {
                Some(cs) => cards = Some(cs),
                None if fetch_on_miss => {
                    // Node's getCardById could resolve an id we cannot: it owns
                    // the request.
                    if ids.len() >= 2 && ids.len() <= 8 && ids.iter().all(|id| scry_id_ok(&Value::String(id.clone()))) {
                        return Ok(Hydrate::Forward);
                    }
                }
                None => {}
            }
        }
    }
    Ok(Hydrate::Done(finish_hydrate(number, ok_out, error, title, via, None, cards)))
}

/// Assemble the out map in the object-literal's key order, omitting the
/// undefined (absent) slots exactly like JSON.stringify.
fn finish_hydrate(
    number: Option<Value>,
    ok: bool,
    error: Option<Value>,
    title: Value,
    via: Value,
    card: Option<Value>,
    choices: Option<Vec<Value>>,
) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(n) = number {
        out.insert("number".into(), n);
    }
    out.insert("ok".into(), Value::Bool(ok));
    if let Some(e) = error {
        out.insert("error".into(), e);
    }
    out.insert("title".into(), title);
    out.insert("via".into(), via);
    if let Some(c) = card {
        out.insert("card".into(), c);
    }
    if let Some(c) = choices {
        out.insert("choices".into(), Value::Array(c));
    }
    out
}

/// hydrateChoices twin: null (None) on validation failure or any id the
/// Scryfall lane would have to fetch. The fetch-needed case surfaces as
/// Forward via the caller — represented here with Err(true)-style Option.
enum Choices {
    Cards(Vec<Value>),
    Null,
    NeedsFetch,
}

fn hydrate_choices(conn: &Connection, ids: &[String]) -> Result<Choices, rusqlite::Error> {
    if ids.len() < 2 || ids.len() > 8 || ids.iter().any(|id| !scry_id_ok(&Value::String(id.clone()))) {
        return Ok(Choices::Null);
    }
    let mut cards = Vec::with_capacity(ids.len());
    for id in ids {
        match get_card_by_id(conn, &format!("mtg-{id}"))? {
            Some(c) => cards.push(Value::Object(c)),
            None => return Ok(Choices::NeedsFetch),
        }
    }
    Ok(Choices::Cards(cards))
}

/// hydrateChoices semantics for the in-hydrate choices leg: Some(cards)
/// when every id resolves from the DB, None on validation failure OR any
/// id needing the fetch lane (the caller decides via fetch_on_miss).
fn hydrate_cards_forward(conn: &Connection, ids: &[String]) -> Result<Option<Vec<Value>>, rusqlite::Error> {
    if ids.len() < 2 || ids.len() > 8 || ids.iter().any(|id| !scry_id_ok(&Value::String(id.clone()))) {
        return Ok(None);
    }
    let mut cards = Vec::with_capacity(ids.len());
    for id in ids {
        match get_card_by_id(conn, &format!("mtg-{id}"))? {
            Some(c) => cards.push(Value::Object(c)),
            None => return Ok(None),
        }
    }
    Ok(Some(cards))
}

fn owns(st: &AppState) -> bool {
    st.owns("cardscan")
}

fn authed(st: &AppState, headers: &HeaderMap, is_get: bool) -> Result<i64, Response> {
    let pool = st.db.as_ref().ok_or_else(|| json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;
    let token = bearer_token(headers).map(|s| s.to_string());
    authenticate(pool, token.as_deref(), is_get).map(|u| u.id).map_err(|res| res)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// res.json twin (etag package, weak form)
// ---------------------------------------------------------------------------

/// `res.status(N).json(r.data)` — res.json runs JSON.stringify on the value
/// (production express: no `json spaces`, so compact) and res.send stamps
/// the weak ETag over the shipped bytes. The body is therefore the
/// re-serialised form, NOT the sidecar's original bytes: JSON.stringify
/// canonicalises numbers (1.0 → 1, 1e3 → 1000), which is what the
/// js_stringify below reproduces.
pub(crate) fn res_json(status: StatusCode, v: &Value) -> Response {
    let body = js_stringify(v);
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::ETAG, body_etag(body.as_bytes()))
        .body(Body::from(body))
        .unwrap()
}

/// `fresh` twin (shared with rules.rs): a request Cache-Control carrying the
/// standalone `no-cache` directive disables the ETag 304.
fn fresh_allows_304(headers: &HeaderMap) -> bool {
    match headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()) {
        Some(cc) => !cc.split(',').any(|d| d.trim() == "no-cache"),
        None => true,
    }
}

/// res.send's conditional-GET arm (GET/HEAD only): matching If-None-Match →
/// 304 with the ETag and no entity headers.
fn not_modified(tag: &str) -> Response {
    Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header(header::ETAG, tag)
        .body(Body::empty())
        .unwrap()
}

/// JSON.stringify twin over the JSON value space: serde_json keeps the
/// ORIGINAL literal for non-canonical floats ("1.0", "1e3"), JS always
/// re-emits the shortest round-trip form (integer-valued doubles print
/// without a fraction: 1.0 → "1"). Canonicalise every float through
/// Number::from_f64 (and integral doubles through the integer path) so the
/// bytes match what JSON.stringify ships. Exponent-form extremes
/// (|f| ≥ 1e21 / < 1e-6) remain a serde-vs-JS formatting edge — the
/// sidecar's timings/confidence values never reach it.
fn js_stringify(v: &Value) -> String {
    serde_json::to_string(&canonical(v)).unwrap_or_else(|_| "{}".into())
}

fn canonical(v: &Value) -> Value {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Value::Number(i.into());
            }
            if let Some(u) = n.as_u64() {
                return Value::Number(u.into());
            }
            // Integer-valued DOUBLEs (1.0, 1e3) print without a fraction in
            // JS, but serde keeps them on the float path (as_i64 → None).
            if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.is_finite() {
                    if f >= 0.0 && f <= u64::MAX as f64 {
                        return Value::Number(Value::from(f as u64).as_u64().unwrap().into());
                    }
                    if f >= i64::MIN as f64 && f <= i64::MAX as f64 {
                        return Value::Number(Value::from(f as i64).as_i64().unwrap().into());
                    }
                }
                return n
                    .as_f64()
                    .and_then(serde_json::Number::from_f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null);
            }
            Value::Null
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        Value::Object(o) => {
            let mut m = Map::with_capacity(o.len());
            for (k, x) in o {
                m.insert(k.clone(), canonical(x));
            }
            Value::Object(m)
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// GET /api/cardscan/status — http.get('/api/health', {timeout:2500}).
pub async fn handle_status(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::GET || !owns(&st) {
        return crate::proxy(State(st), req).await;
    }
    let (parts, _body) = req.into_parts();
    if let Err(res) = authed(&st, &parts.headers, true) {
        return node_exact(res).await;
    }
    match call_get("/api/health", Duration::from_millis(2500)).await {
        Up::Resp { status, json, .. } => {
            let mut body = Map::new();
            body.insert("ok".into(), Value::Bool(status == 200));
            match json {
                // Node: `...(typeof r.data === 'object' ? r.data : {})` —
                // typeof [] === 'object', so array health bodies spread
                // their entries under numeric-string keys.
                Some(Value::Object(o)) => {
                    for (k, v) in o {
                        body.insert(k, v);
                    }
                }
                Some(Value::Array(a)) => {
                    for (i, v) in a.iter().enumerate() {
                        body.insert(i.to_string(), v.clone());
                    }
                }
                _ => {}
            }
            let v = Value::Object(body);
            let tag = body_etag(js_stringify(&v).as_bytes());
            let inm = parts.headers.get(header::IF_NONE_MATCH).and_then(|x| x.to_str().ok());
            if fresh_allows_304(&parts.headers) && etag_matches(inm, &tag) {
                return not_modified(&tag);
            }
            res_json(
                if status == 200 { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE },
                &v,
            )
        }
        up => upstream_error(&up),
    }
}

/// POST /api/cardscan/detect — raw image → /api/detect-cards passthrough.
pub async fn handle_detect(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::POST || !owns(&st) {
        return crate::proxy(State(st), req).await;
    }
    if let Err(res) = authed(&st, req.headers(), false) {
        return node_exact(res).await;
    }
    let (parts, bytes) = read_body(req).await;
    let headers = parts.headers.clone();
    let raw = match parse_raw_image(&parts.headers, &bytes) {
        Ok(Some(b)) if b.len() >= 100 => b,
        Ok(_) => return res_json(StatusCode::BAD_REQUEST, &serde_json::json!({"ok":false,"error":"Missing image"})),
        Err(_) => return crate::proxy(State(st.clone()), replay(parts, bytes, headers)).await,
    };
    // Node pins BOTH Content-Type and X-Scan-Source on the detect leg.
    match call_bytes("/api/detect-cards", raw, "image/jpeg", &[("x-scan-source", "browser-camera".into())]).await {
        up @ Up::Resp { .. } => passthrough_json(&up),
        up => upstream_error(&up),
    }
}

/// POST /api/cardscan/frame — raw image → /api/scan-frame + hydration.
pub async fn handle_frame(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::POST || !owns(&st) {
        return crate::proxy(State(st), req).await;
    }
    let uid = match authed(&st, req.headers(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    let headers = req.headers().clone();
    let (parts, bytes) = read_body(req).await;
    let raw = match parse_raw_image(&parts.headers, &bytes) {
        Ok(Some(b)) if b.len() >= 100 => b,
        // req.body not a Buffer → Missing image — but only when the outer
        // bigJson actually parsed it: a json-CT body that bigJson chokes on
        // (binary junk) is Node's own 500, so Forward and let it answer.
        Ok(None) if !ct_is(&parts.headers, "application/json")
            || bytes.is_empty()
            || serde_json::from_slice::<Value>(&bytes).is_ok() => {
            return res_json(StatusCode::BAD_REQUEST, &serde_json::json!({"ok":false,"error":"Missing image"}))
        }
        _ => return crate::proxy(State(st.clone()), replay(parts, bytes, headers.clone())).await,
    };
    let via = sanitize(
        headers.get("x-scan-client").and_then(|v| v.to_str().ok()).unwrap_or(""),
        &|c| is_word_or(c, &[' ', '.', ':', '=', ',', '/', '-']),
        120,
    );
    let session_raw = headers.get("x-scan-session").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let session = sanitize(&session_raw, &|c| is_word_or(c, &[]), 40);
    let mut extra: Vec<(&str, String)> = Vec::new();
    if !via.is_empty() {
        extra.push(("x-scan-client", via.clone()));
    }
    if !session.is_empty() {
        extra.push(("x-scan-session", format!("{uid}-{session}")));
    }
    let t0 = now_ms();
    match call_bytes("/api/scan-frame", raw, "image/jpeg", &extra).await {
        Up::Resp { status: 200, json: Some(v), .. } if truthy(v.get("ok")) => {
            let data = match &v {
                Value::Object(m) => m.clone(),
                // Node: r.data?.ok truthy on a non-object then .map on
                // undefined → TypeError → catch → 502 arm.
                _ => return upstream_error(&Up::Failed),
            };
            let data_v = Value::Object(data.clone());
            let results_in = match js_or_array(&data_v, "results") {
                Ok(a) => a,
                Err(()) => return upstream_error(&Up::Failed),
            };
            let mut results: Vec<Value> = Vec::new();
            let mut forward = false;
            {
                let conn = match st.db.as_ref().unwrap().get() {
                    Ok(c) => c,
                    Err(_) => return upstream_error(&Up::Failed),
                };
                for x in &results_in {
                    match hydrate(&conn, x, true) {
                        Ok(Hydrate::Done(mut m)) => {
                            m.insert("cached".into(), Value::Bool(truthy(x.get("cached"))));
                            results.push(Value::Object(m));
                        }
                        Ok(Hydrate::Forward) | Err(_) => {
                            forward = true;
                            break;
                        }
                        Ok(Hydrate::Throw) => return upstream_error(&Up::Failed),
                    }
                }
            }
            if forward {
                return crate::proxy(State(st.clone()), replay(parts, bytes, headers)).await;
            }
            let cands = match js_or_array(&data_v, "candidates") {
                Ok(a) => a,
                Err(()) => return upstream_error(&Up::Failed),
            };
            let mut candidates: Vec<Value> = Vec::with_capacity(cands.len());
            for c in &cands {
                if c.is_null() {
                    // c.number on null → TypeError → catch → 502 arm.
                    return upstream_error(&Up::Failed);
                }
                let mut m = Map::new();
                for k in ["number", "box", "quad", "eligible", "status"] {
                    if let Some(x) = c.get(k) {
                        m.insert(k.into(), x.clone());
                    }
                }
                candidates.push(Value::Object(m));
            }
            let mut timings = match data.get("timings") {
                Some(Value::Object(t)) => t.clone(),
                _ => Map::new(),
            };
            timings.insert("proxy_ms".into(), Value::from(now_ms() as i64 - t0 as i64));
            let mut body = Map::new();
            body.insert("ok".into(), Value::Bool(true));
            // JSON.stringify drops undefined: frame rides only when present.
            if let Some(f) = data.get("frame") {
                body.insert("frame".into(), f.clone());
            }
            body.insert("candidates".into(), Value::Array(candidates));
            body.insert("results".into(), Value::Array(results));
            body.insert("timings".into(), Value::Object(timings));
            res_json(StatusCode::OK, &Value::Object(body))
        }
        Up::Resp { status, json, .. } => {
            let out_status = if status == 200 { StatusCode::UNPROCESSABLE_ENTITY } else { StatusCode::from_u16(status).unwrap_or(StatusCode::OK) };
            match json {
                Some(v) => res_json(out_status, &v),
                None => upstream_error(&Up::Failed),
            }
        }
        up => upstream_error(&up),
    }
}

fn replay(parts: axum::http::request::Parts, bytes: Vec<u8>, headers: HeaderMap) -> Request {
    let mut req = Request::from_parts(parts, Body::from(bytes));
    *req.headers_mut() = headers;
    req
}

/// POST /api/cardscan/cards — client-read results, card_cache only.
pub async fn handle_cards(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::POST || !owns(&st) {
        return crate::proxy(State(st), req).await;
    }
    if let Err(res) = authed(&st, req.headers(), false) {
        return node_exact(res).await;
    }
    let (parts, bytes) = read_body(req).await;
    let headers = parts.headers.clone();
    let body = match parse_outer_json(&parts.headers, &bytes) {
        Ok(v) => v,
        Err(_) => return crate::proxy(State(st.clone()), replay(parts, bytes, headers)).await,
    };
    let items: Vec<Value> = match body.get("results") {
        Some(Value::Array(a)) if !a.is_empty() && a.len() <= 8 => a.clone(),
        _ => {
            return res_json(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"ok":false,"error":"Expected 1-8 results"}),
            )
        }
    };
    let mut ids = Vec::with_capacity(items.len());
    for x in &items {
        let idv = x.as_object().and_then(|o| o.get("scryfallId")).cloned().unwrap_or(Value::Null);
        if x.is_null() || !scry_id_ok(&idv) {
            return res_json(StatusCode::BAD_REQUEST, &serde_json::json!({"ok":false,"error":"Invalid card id"}));
        }
        ids.push(js_to_string(Some(&idv)));
    }
    let headers = parts.headers.clone();
    let mut results: Vec<Value> = Vec::with_capacity(ids.len());
    {
        let conn = match st.db.as_ref().unwrap().get() {
            Ok(c) => c,
            Err(_) => return crate::proxy(State(st.clone()), replay(parts, bytes, headers)).await,
        };
        // Node maps each item through the SAME hydrate() with a synthetic
        // result: {ok:true, scene_number:x.number, title:x.title,
        // card:{id:x.scryfallId}, footer_ocr:{resolved_by:x.via}} — and
        // getCardById is NOT a plain cache read: a miss enters the Scryfall
        // fetch lane (scryfallApi.getCardById -> scryGet + cacheCards),
        // whose outcome is observable (resolved card vs the 'not in the
        // card database yet' row). fetch_on_miss = true: any miss hands the
        // whole request to Node, which runs that fetch itself.
        for item in &items {
            // scene_number must be ABSENT (not null) when the client omitted
            // number — hydrate's JSON.stringify then drops out.number, like
            // Node's `number: result.scene_number` with an undefined source.
            let mut synth = Map::new();
            synth.insert("ok".into(), Value::Bool(true));
            if let Some(n) = item.get("number") {
                synth.insert("scene_number".into(), n.clone());
            }
            synth.insert("title".into(), item.get("title").cloned().unwrap_or(Value::Null));
            synth.insert("card".into(), serde_json::json!({ "id": item.get("scryfallId").cloned().unwrap_or(Value::Null) }));
            synth.insert("footer_ocr".into(), serde_json::json!({ "resolved_by": item.get("via").cloned().unwrap_or(Value::Null) }));
            match hydrate(&conn, &Value::Object(synth), true) {
                Ok(Hydrate::Done(m)) => results.push(Value::Object(m)),
                Ok(Hydrate::Forward) | Ok(Hydrate::Throw) | Err(_) => {
                    drop(conn);
                    return crate::proxy(State(st.clone()), replay(parts, bytes, headers)).await;
                }
            }
        }
    }
    let mut body = Map::new();
    body.insert("ok".into(), Value::Bool(true));
    body.insert("results".into(), Value::Array(results));
    res_json(StatusCode::OK, &Value::Object(body))
}

/// POST /api/cardscan/choices — printing picker rows.
pub async fn handle_choices(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::POST || !owns(&st) {
        return crate::proxy(State(st), req).await;
    }
    if let Err(res) = authed(&st, req.headers(), false) {
        return node_exact(res).await;
    }
    let (parts, bytes) = read_body(req).await;
    let headers = parts.headers.clone();
    let body = match parse_outer_json(&parts.headers, &bytes) {
        Ok(v) => v,
        Err(_) => return crate::proxy(State(st.clone()), replay(parts, bytes, headers)).await,
    };
    let ids: Vec<String> = match body.get("ids") {
        Some(Value::Array(a)) => a.iter().map(|v| js_to_string(Some(v))).collect(),
        _ => vec![],
    };
    let headers = parts.headers.clone();
    let outcome = {
        let conn = match st.db.as_ref().unwrap().get() {
            Ok(c) => c,
            Err(_) => return crate::proxy(State(st.clone()), replay(parts, bytes, headers)).await,
        };
        hydrate_choices(&conn, &ids)
    };
    match outcome {
        Ok(Choices::Cards(cards)) => {
            let mut m = Map::new();
            m.insert("ok".into(), Value::Bool(true));
            m.insert("cards".into(), Value::Array(cards));
            res_json(StatusCode::OK, &Value::Object(m))
        }
        Ok(Choices::Null) => res_json(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({"ok":false,"error":"Expected 2-8 card ids that exist"}),
        ),
        Ok(Choices::NeedsFetch) | Err(_) => {
            crate::proxy(State(st.clone()), replay(parts, bytes, headers)).await
        }
    }
}

/// POST /api/cardscan — {cards:[...]} → /api/scan-scene + hydration.
pub async fn handle_scan(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::POST || !owns(&st) {
        return crate::proxy(State(st), req).await;
    }
    if let Err(res) = authed(&st, req.headers(), false) {
        return node_exact(res).await;
    }
    let (parts, bytes) = read_body(req).await;
    let headers = parts.headers.clone();
    let body = match parse_outer_json(&parts.headers, &bytes) {
        Ok(v) => v,
        Err(_) => return crate::proxy(State(st.clone()), replay(parts, bytes, headers)).await,
    };
    let cards = match body.get("cards") {
        Some(Value::Array(a)) if !a.is_empty() && a.len() <= 8 => a.clone(),
        _ => {
            return res_json(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"ok":false,"error":"Expected 1-8 cards"}),
            )
        }
    };
    let headers = parts.headers.clone();
    // axios JSON.stringify's the object it POSTs; the client body was
    // parsed through strict express.json, so numbers are in JS-canonical
    // form on the Node leg too — js_stringify reproduces that exactly.
    let payload = js_stringify(&serde_json::json!({ "cards": cards }));
    match call_json("/api/scan-scene", payload, "application/json", &[("x-scene-protocol", "3".into())], TIMEOUT).await {
        Up::Resp { status: 200, json: Some(v), .. } if truthy(v.get("ok")) => {
            let data = match &v {
                Value::Object(m) => m.clone(),
                _ => return upstream_error(&Up::Failed),
            };
            let results_in = match js_or_array(&Value::Object(data.clone()), "results") {
                Ok(a) => a,
                Err(()) => return upstream_error(&Up::Failed),
            };
            let mut results: Vec<Value> = Vec::new();
            let mut forward = false;
            {
                let conn = match st.db.as_ref().unwrap().get() {
                    Ok(c) => c,
                    Err(_) => return upstream_error(&Up::Failed),
                };
                for x in &results_in {
                    match hydrate(&conn, x, true) {
                        Ok(Hydrate::Done(m)) => results.push(Value::Object(m)),
                        Ok(Hydrate::Forward) | Err(_) => {
                            forward = true;
                            break;
                        }
                        Ok(Hydrate::Throw) => return upstream_error(&Up::Failed),
                    }
                }
            }
            if forward {
                return crate::proxy(State(st.clone()), replay(parts, bytes, headers)).await;
            }
            let mut body = Map::new();
            body.insert("ok".into(), Value::Bool(true));
            body.insert("results".into(), Value::Array(results));
            // res.json drops the undefined timings slot entirely.
            if let Some(t) = data.get("timings") {
                body.insert("timings".into(), t.clone());
            }
            // fire-and-forget scene-complete (Node .catch(() => {}))
            tokio::spawn(async move {
                let _ = call_json("/api/scene-complete", "{}".into(), "application/json", &[], TIMEOUT).await;
            });
            res_json(StatusCode::OK, &Value::Object(body))
        }
        Up::Resp { status, json, .. } => {
            let out_status = if status == 200 { StatusCode::UNPROCESSABLE_ENTITY } else { StatusCode::from_u16(status).unwrap_or(StatusCode::OK) };
            match json {
                Some(v) => res_json(out_status, &v),
                None => upstream_error(&Up::Failed),
            }
        }
        up => upstream_error(&up),
    }
}

/// POST /api/cardscan/telemetry — local JSONL sink (NO sidecar leg).
pub async fn handle_telemetry(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::POST || !owns(&st) {
        return crate::proxy(State(st), req).await;
    }
    let uid = match authed(&st, req.headers(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    let (parts, bytes) = read_body(req).await;
    let body = match parse_telemetry_body(&parts.headers, &bytes) {
        Ok(v) => v,
        Err(_) => {
            let headers = parts.headers.clone();
            return crate::proxy(State(st.clone()), replay(parts, bytes, headers)).await;
        }
    };
    if std::env::var("SCAN_TELEMETRY").map(|v| v == "0").unwrap_or(false) {
        return StatusCode::NO_CONTENT.into_response();
    }
    let records: Vec<Value> = match body.get("records") {
        Some(Value::Array(a)) => a.iter().take(200).cloned().collect(),
        _ => vec![],
    };
    if records.is_empty() {
        return res_json(StatusCode::BAD_REQUEST, &serde_json::json!({"ok":false,"error":"No records"}));
    }
    let day = chrono_day();
    let dir = telemetry_dir(&st);
    let ua = parts
        .headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect::<String>();
    let session = match body.get("session") {
        Some(v) if truthy(Some(v)) => js_to_string(Some(v)).chars().take(20).collect(),
        _ => String::new(),
    };
    let mut lines = String::new();
    for r in &records {
        let mut m = Map::new();
        m.insert("user".into(), serde_json::json!(uid));
        m.insert("session".into(), Value::String(session.clone()));
        // JSON.stringify drops undefined: only present keys ride.
        if let Some(v) = body.get("platform") {
            m.insert("platform".into(), v.clone());
        }
        if let Some(v) = body.get("device") {
            m.insert("device".into(), v.clone());
        }
        m.insert("ua".into(), Value::String(ua.clone()));
        if let Value::Object(o) = r {
            for (k, v) in o {
                m.insert(k.clone(), v.clone());
            }
        }
        m.insert("recv".into(), Value::String(chrono_iso())); // fresh Date per record
        // JSON.stringify({...meta, ...r, recv}) — canonical numbers, and
        // undefined slots (absent keys) never enter the object.
        lines.push_str(&js_stringify(&Value::Object(m)));
        lines.push('\n');
    }
    let file = dir.join(format!("{day}.jsonl"));
    let cap = 50 * 1024 * 1024usize;
    let (dir_c, file_c, lines_c) = (dir.clone(), file, lines.clone());
    // Node's promise chain: mkdir → stat(size) → append (skipped over cap)
    // → 204; any failure → 500 {}. A skip-over-cap still 204s.
    let ok = tokio::task::spawn_blocking(move || {
        if std::fs::create_dir_all(&dir_c).is_err() {
            return false;
        }
        let size = std::fs::metadata(&file_c).map(|m| m.len() as usize).unwrap_or(0);
        if size + lines_c.len() > cap {
            return true;
        }
        use std::io::Write;
        match std::fs::OpenOptions::new().create(true).append(true).open(&file_c) {
            Ok(mut f) => f.write_all(lines_c.as_bytes()).is_ok(),
            Err(_) => false,
        }
    })
    .await
    .unwrap_or(false);
    if ok {
        StatusCode::NO_CONTENT.into_response()
    } else {
        res_json(StatusCode::INTERNAL_SERVER_ERROR, &serde_json::json!({"ok":false}))
    }
}

fn iso_utc(ms: u64) -> String {
    // Date.now() -> new Date().toISOString() (civil-date from days-since-epoch).
    let days = (ms / 86_400_000) as i64;
    let rem = (ms % 86_400_000) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        mth, d,
        rem / 3_600_000,
        (rem / 60_000) % 60,
        (rem / 1000) % 60,
        rem % 1000
    )
}

fn chrono_day() -> String {
    iso_utc(now_ms()).chars().take(10).collect()
}

/// Public day getter (YYYY-MM-DD, UTC) for the telemetry filename pin.
pub fn cardscan_day() -> String {
    chrono_day()
}

fn chrono_iso() -> String {
    iso_utc(now_ms())
}

fn telemetry_dir(st: &AppState) -> std::path::PathBuf {
    // SCAN_TELEMETRY_DIR || <repo>/database/scan-telemetry — module-load
    // snapshot like Node's top-level const; for the container that is the
    // DB dir, so mirror the rules-cache convention (dirname(DB)).
    static DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    if let Some(d) = DIR.get_or_init(|| std::env::var("SCAN_TELEMETRY_DIR").ok().filter(|d| !d.is_empty()).map(std::path::PathBuf::from)).as_ref() {
        return d.clone();
    }
    st.cfg
        .db_path
        .as_ref()
        .and_then(|p| p.parent().map(|d| d.join("scan-telemetry")))
        .unwrap_or_else(|| std::path::PathBuf::from("scan-telemetry"))
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn js_stringify_canonicalises_floats_like_json_stringify() {
        // serde_json preserves the source literal ("1.0", "1e3"); JS emits
        // the shortest round-trip decimal (1, 1000).
        let v: Value = serde_json::from_str(r#"{"a":1.0,"b":1e3,"c":0.5,"d":2.50}"#).unwrap();
        assert_eq!(js_stringify(&v), r#"{"a":1,"b":1000,"c":0.5,"d":2.5}"#);
        // nested + arrays too
        let v: Value = serde_json::from_str(r#"{"t":{"ms":12.0},"xs":[3.0,"7"]}"#).unwrap();
        assert_eq!(js_stringify(&v), r#"{"t":{"ms":12},"xs":[3,"7"]}"#);
    }

    #[test]
    fn truthy_table_matches_js() {
        assert!(!truthy(Some(&json!(""))));
        assert!(truthy(Some(&json!([])))); // JS [] is truthy
        assert!(truthy(Some(&json!({}))));
        assert!(!truthy(Some(&json!(0))));
        assert!(!truthy(Some(&json!(null))));
        assert!(!truthy(None));
    }

    #[test]
    fn finish_hydrate_drops_undefined_slots_in_literal_order() {
        let m = finish_hydrate(None, true, None, Value::Null, Value::Null, None, None);
        let keys: Vec<&String> = m.keys().collect();
        assert_eq!(keys, vec!["ok", "title", "via"]);
        // number present + error assigned (miss branch) keeps literal slots:
        let m = finish_hydrate(
            Some(json!("7")),
            false,
            Some(json!("printing not in the card database yet")),
            Value::Null,
            Value::Null,
            None,
            None,
        );
        let keys: Vec<&String> = m.keys().collect();
        assert_eq!(keys, vec!["number", "ok", "error", "title", "via"]);
    }

    #[test]
    fn js_or_array_semantics() {
        let d = json!({});
        assert_eq!(js_or_array(&d, "results").unwrap(), Vec::<Value>::new());
        let d = json!({"results": null});
        assert_eq!(js_or_array(&d, "results").unwrap(), Vec::<Value>::new());
        let d = json!({"results": 0}); // falsy → || []
        assert_eq!(js_or_array(&d, "results").unwrap(), Vec::<Value>::new());
        let d = json!({"results": [1, 2]});
        assert_eq!(js_or_array(&d, "results").unwrap(), vec![json!(1), json!(2)]);
        let d = json!({"results": {"a": 1}}); // truthy non-array → .map TypeError
        assert!(js_or_array(&d, "results").is_err());
        let d = json!({"results": "abc"});
        assert!(js_or_array(&d, "results").is_err());
    }

    #[test]
    fn res_json_etag_is_hex_len_and_unpadded_b64() {
        let res = res_json(StatusCode::OK, &json!({"ok":true}));
        let et = res.headers()[header::ETAG].to_str().unwrap().to_string();
        assert_eq!(et, body_etag(br#"{"ok":true}"#).as_str());
        // 11 bytes → hex "b", never decimal 11 in the wrong base; digest
        // has no '=' padding.
        assert!(et.starts_with("W/\"b-") && !et.contains('='), "{et}");
    }
}

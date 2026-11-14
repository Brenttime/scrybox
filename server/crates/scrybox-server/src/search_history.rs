//! Port of `backend/src/routes/searchHistory.js` (route group
//! `search_history`): GET/POST/DELETE `/api/search-history`. The list lives
//! in one `search_history` row per user as deflated JSON
//! (`[[query, unixSeconds], ...]`, newest first) — the exact `zlib.deflateRaw
//! /inflateRaw` codec from Node, so a row written by either server reads
//! identically on the other (rule 2: writes through the shared copy must be
//! read-back-verified by Node).
//!
//! Parity contract (BRIEF rule 1): byte-identical body for the same DB
//! state, Express default ETag + If-None-Match 304 replay on GET, identical
//! 400/401/403/413/500 bodies. Like Node, the JSON body is parsed BEFORE
//! authentication (body-parser runs above the gate in server.js), so a
//! malformed-JSON POST answers 500 `Internal server error` even without a
//! token; the >1 MB cap answers the error middleware's 413 shape. Methods
//! and path variants the port does not own proxy to Node untouched.
//!
//! Semantics reproduced exactly (JS quirks): `cleanQuery` trims the
//! ECMAScript WhiteSpace/LineTerminator set and slices to 500 UTF-16 code
//! units; `Math.floor(Number(i.at) / 1000) || now` falls back to `now` on
//! every falsy result (NaN, 0, "abc"); the merge is an insertion-ordered Map
//! (add-before-cur, strict-`<` update, stable sort by at-descending,
//! slice(0, 100)); per-user mutations are serialised like Node's promise
//! chains.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;
use serde_json::{json, Value};

use crate::collection::{body_etag, etag_matches, gzip_bytes, json_num, wants_encoding};
use crate::db::DbPool;
use crate::AppState;
use crate::poison::LockExt;

const MAX_ITEMS: usize = 100;
/// JS `slice(0, 500)` — UTF-16 CODE units, not chars.
const MAX_QUERY: usize = 500;
/// express.json({ limit: '1mb' }) — 1 * 1024 * 1024 bytes.
const BODY_LIMIT: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// deflate-raw codec (zlib.deflateRawSync {level:9} / inflateRawSync twin)
// ---------------------------------------------------------------------------

fn deflate_raw(data: &[u8]) -> Vec<u8> {
    use flate2::{Compress, FlushCompress, Status};
    let mut d = Compress::new(flate2::Compression::new(9), false);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut pos = 0usize;
    loop {
        let (in0, out0) = (d.total_in() as usize, d.total_out() as usize);
        let status = d
            .compress(&data[pos..], &mut buf, FlushCompress::Finish)
            .expect("deflate");
        let (in1, out1) = (d.total_in() as usize, d.total_out() as usize);
        out.extend_from_slice(&buf[..out1 - out0]);
        pos += in1 - in0;
        if status == Status::StreamEnd {
            return out;
        }
        if out1 == out0 && in1 == in0 {
            unreachable!("deflate made no progress");
        }
    }
}

fn inflate_raw(blob: &[u8]) -> Option<Vec<u8>> {
    use flate2::{Decompress, FlushDecompress, Status};
    let mut i = Decompress::new(false);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut pos = 0usize;
    loop {
        let (in0, out0) = (i.total_in() as usize, i.total_out() as usize);
        let status = i
            .decompress(&blob[pos..], &mut buf, FlushDecompress::Finish)
            .ok()?;
        let (in1, out1) = (i.total_in() as usize, i.total_out() as usize);
        out.extend_from_slice(&buf[..out1 - out0]);
        pos += in1 - in0;
        match status {
            Status::StreamEnd => return Some(out),
            Status::Ok => {
                if out1 == out0 && in1 == in0 {
                    return None;
                }
            }
            _ => return None,
        }
    }
}

/// decode() from the Node route: any parse/inflate failure or non-array
/// yields []; entries keep only `[string, ...]` pairs. `at` is kept as
/// Option<f64> (a non-numeric stored value would render `undefined*1000`
/// as JSON null in Node; our own writes only store integers).
fn decode(blob: Option<&[u8]>) -> Vec<(String, Option<f64>)> {
    let Some(blob) = blob else { return Vec::new() };
    let Some(text) = inflate_raw(blob) else { return Vec::new() };
    match serde_json::from_slice::<Vec<Value>>(&text) {
        Ok(list) => list
            .into_iter()
            .filter_map(|e| {
                let arr = e.as_array()?;
                if arr.len() < 2 {
                    // JS keeps 1-element arrays too: [q] -> at = undefined.
                    if arr.len() == 1 {
                        let q = arr[0].as_str()?;
                        return Some((q.to_string(), None));
                    }
                    return None;
                }
                let q = arr[0].as_str()?;
                Some((q.to_string(), arr[1].as_f64()))
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn encode(list: &[(String, Option<f64>)]) -> Vec<u8> {
    let items: Vec<Value> = list
        .iter()
        .map(|(q, at)| {
            json!([q, at.map(json_num).unwrap_or(Value::Null)])
        })
        .collect();
    deflate_raw(&serde_json::to_vec(&items).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// JS-semantics helpers
// ---------------------------------------------------------------------------

/// ECMAScript WhiteSpace ∪ LineTerminator (String.prototype.trim's set) —
/// exactly the Zs category + TAB VT FF SP NBSP ZWNBSP + LS PS + CR LF.
/// NOT identical to Rust's char::is_whitespace (which adds NEL U+0085 and
/// others JS keeps), so this set is enumerated.
fn js_ws(c: char) -> bool {
    matches!(
        c,
        '\u{0009}' | '\u{000B}' | '\u{000C}' | '\u{0020}' | '\u{00A0}' | '\u{FEFF}'
            | '\u{000A}' | '\u{000D}' | '\u{2028}' | '\u{2029}'
            | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

fn js_trim(s: &str) -> &str {
    s.trim_end_matches(js_ws).trim_start_matches(js_ws)
}

/// `typeof q === 'string' ? q.trim().slice(0, MAX_QUERY) : ''` — slice
/// counts UTF-16 code units (astral chars cost 2, like V8).
fn clean_query(v: Option<&Value>) -> String {
    let Some(Value::String(s)) = v else { return String::new() };
    let t = js_trim(s);
    let mut units = 0usize;
    let mut end = t.len();
    for (i, ch) in t.char_indices() {
        if units >= MAX_QUERY {
            end = i;
            break;
        }
        units += ch.len_utf16();
    }
    t[..end].to_string()
}

/// `Number(x)` for the JSON values an items[] entry can carry. Objects and
/// mixed arrays are NaN; a 1-element array recurses; `null` is 0.
fn js_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => if *b { 1.0 } else { 0.0 },
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => {
            let t = js_trim(s);
            if t.is_empty() {
                return 0.0;
            }
            if t.eq_ignore_ascii_case("infinity") || t.eq_ignore_ascii_case("+infinity") {
                return f64::INFINITY;
            }
            if t.eq_ignore_ascii_case("-infinity") {
                return f64::NEG_INFINITY;
            }
            if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                return i64::from_str_radix(hex, 16).map(|v| v as f64).unwrap_or(f64::NAN);
            }
            // ES2015 numeric literals: Number('0b101')=5, Number('0o17')=15.
            if let Some(bin) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
                return i64::from_str_radix(bin, 2).map(|v| v as f64).unwrap_or(f64::NAN);
            }
            if let Some(oct) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
                return i64::from_str_radix(oct, 8).map(|v| v as f64).unwrap_or(f64::NAN);
            }
            t.parse::<f64>().unwrap_or(f64::NAN)
        }
        Value::Array(a) => match a.len() {
            0 => 0.0,
            1 => js_number(&a[0]),
            _ => f64::NAN,
        },
        Value::Object(_) => f64::NAN,
    }
}

/// `Math.floor(Number(i && i.at) / 1000) || now` — every falsy floor result
/// (NaN, ±0) becomes `now`; negative floors survive.
fn item_at(v: &Value, now: i64) -> i64 {
    let raw = match v.get("at") {
        Some(x) => js_number(x),
        None => f64::NAN, // `i && i.at` on a non-object never reaches here
    };
    let f = (raw / 1000.0).floor();
    if f.is_nan() || f == 0.0 {
        now
    } else {
        f as i64
    }
}

// ---------------------------------------------------------------------------
// load / save / mutate (per-user serialized, like Node's promise chains)
// ---------------------------------------------------------------------------

fn load(pool: &DbPool, user_id: i64) -> Result<Vec<(String, Option<f64>)>, String> {
    let conn = pool.get().map_err(|e| format!("read pool: {e}"))?;
    let blob: Option<Vec<u8>> = conn
        .prepare_cached("SELECT data FROM search_history WHERE user_id = ?")
        .map_err(|e| e.to_string())?
        .query_row([user_id], |r| r.get::<_, Option<Vec<u8>>>(0))
        .ok()
        .flatten();
    Ok(decode(blob.as_deref()))
}

fn save(pool: &DbPool, user_id: i64, list: &[(String, Option<f64>)]) -> Result<(), String> {
    let conn = pool.get().map_err(|e| format!("write pool: {e}"))?;
    let blob = encode(list);
    conn.execute(
        "INSERT INTO search_history (user_id, data, updated_at) VALUES (?, ?, CURRENT_TIMESTAMP)
         ON CONFLICT(user_id) DO UPDATE SET data = excluded.data, updated_at = CURRENT_TIMESTAMP",
        rusqlite::params![user_id, blob],
    )
    .map_err(|e| format!("save: {e}"))?;
    Ok(())
}

/// Node serialises per-user read-modify-write with a promise chain
/// (`chains` Map); the same guarantee here, keyed by user id.
fn user_lock(user_id: i64) -> Arc<tokio::sync::Mutex<()>> {
    static CHAINS: Mutex<Option<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>> = Mutex::new(None);
    let mut map = CHAINS.guard();
    let map = map.get_or_insert_with(HashMap::new);
    Arc::clone(map.entry(user_id).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))))
}

fn merge_lists(cur: Vec<(String, Option<f64>)>, add: Vec<(String, Option<f64>)>) -> Vec<(String, Option<f64>)> {
    let mut out: Vec<(String, Option<f64>)> = Vec::new();
    let mut idx: HashMap<String, usize> = HashMap::new();
    for (q, at) in add.into_iter().chain(cur) {
        match idx.get(&q) {
            Some(&i) => {
                // JS: `if (!merged.has(q) || merged.get(q) < at) merged.set(q, at)`
                // — strict `<`, insertion position kept; non-numeric compares
                // false (NaN) and keeps the existing value.
                let replace = match (out[i].1, at) {
                    (Some(e), Some(n)) => e < n,
                    _ => false,
                };
                if replace {
                    out[i].1 = at;
                }
            }
            None => {
                idx.insert(q.clone(), out.len());
                out.push((q, at));
            }
        }
    }
    // `[...merged.entries()].sort((a, b) => b[1] - a[1])` — stable, ties
    // keep Map-insertion order; a missing (undefined) at is NaN -> the
    // comparator returns NaN which V8 treats as 0 (stable, keep order).
    out.sort_by(|a, b| match (a.1, b.1) {
        (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal),
        _ => std::cmp::Ordering::Equal,
    });
    out.truncate(MAX_ITEMS);
    out
}

fn view(list: &[(String, Option<f64>)]) -> Value {
    Value::Array(
        list.iter()
            .map(|(q, at)| {
                json!({ "q": q, "at": at.map(|a| json_num(a * 1000.0)).unwrap_or(Value::Null) })
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

pub async fn handle(method: Method, State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("search_history")
        || st.db.is_none()
        || req.uri().path() != "/api/search-history"
        || !matches!(method, Method::GET | Method::POST | Method::DELETE)
    {
        return crate::proxy(State(st.clone()), req).await;
    }
    let pool = st.db.clone().unwrap();
    let headers = req.headers().clone();

    // Body parsing runs BEFORE auth (server.js mounts normalJson above the
    // authentication gate). Only when content-type is JSON; anything else
    // leaves the body `{}` like a skipped parser.
    let body: Value = if matches!(method, Method::POST | Method::DELETE) {
        let is_json = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_ascii_lowercase().replace(' ', "").contains("application/json"))
            .unwrap_or(false);
        if !is_json {
            Value::Object(Default::default())
        } else {
            // body-parser limit semantics WITH Node's post-response socket
            // dump: once over 1 MiB we stop buffering but keep draining the
            // stream, otherwise the half-read request makes hyper reset the
            // connection while the client is still writing (EPIPE).
            let (b, too_big) = read_capped(req.into_body()).await;
            if too_big {
                return too_large();
            }
            if b.is_empty() {
                // body-parser's empty-buffer shortcut: req.body = {}.
                Value::Object(Default::default())
            } else {
                match serde_json::from_slice::<Value>(&b) {
                    // express.json strict mode: only objects/arrays
                    // parse; a primitive throws and the generic
                    // error handler answers 500.
                    Ok(v @ (Value::Object(_) | Value::Array(_))) => v,
                    _ => {
                        tracing::error!("search-history: invalid JSON body");
                        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
                    }
                }
            }
        }
    } else {
        Value::Object(Default::default())
    };

    let token = crate::auth::bearer_token(&headers).map(str::to_string);
    let user = match crate::auth::authenticate(&pool, token.as_deref(), method == Method::GET) {
        Ok(u) => u,
        Err(res) => return res,
    };
    let user_id = user.id;

    let result: Result<Value, (&'static str, String)> = match method {
        Method::GET => load(&pool, user_id)
            .map(|list| view(&list))
            .map_err(|e| ("Failed to load search history", e)),
        Method::POST => {
            let now = crate::stats::now_ms()
                .div_euclid(1000);
            let incoming: Vec<(String, Option<f64>)> = match body.get("items") {
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|i| {
                        let obj = i.as_object().cloned().unwrap_or_default();
                        (clean_query(obj.get("q")), Some(item_at(&Value::Object(obj), now) as f64))
                    })
                    .collect(),
                // `[[cleanQuery(req.body.q), now]]` — at is ALWAYS now here.
                _ => vec![(clean_query(body.get("q")), Some(now as f64))],
            };
            let add: Vec<(String, Option<f64>)> =
                incoming.into_iter().filter(|(q, _)| !q.is_empty()).collect();
            if add.is_empty() {
                return json_error(StatusCode::BAD_REQUEST, "q is required");
            }
            let guard = user_lock(user_id).lock_owned().await;
            let res = (|| -> Result<Value, String> {
                let cur = load(&pool, user_id)?;
                let list = merge_lists(cur, add.clone());
                save(&pool, user_id, &list)?;
                Ok(view(&list))
            })();
            drop(guard);
            res.map_err(|e| ("Failed to save search history", e))
        }
        Method::DELETE => {
            let q = clean_query(body.get("q"));
            let guard = user_lock(user_id).lock_owned().await;
            let res = (|| -> Result<Value, String> {
                let cur = load(&pool, user_id)?;
                let list = if q.is_empty() {
                    Vec::new()
                } else {
                    cur.into_iter().filter(|(x, _)| *x != q).collect()
                };
                save(&pool, user_id, &list)?;
                Ok(view(&list))
            })();
            drop(guard);
            res.map_err(|e| ("Failed to update search history", e))
        }
        _ => unreachable!(),
    };

    let body_json = match result {
        Ok(v) => v,
        Err((msg, detail)) => {
            tracing::error!(error = %detail, "search-history handler error");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, msg);
        }
    };

    let mut raw = serde_json::to_vec(&body_json).unwrap_or_else(|_| b"null".to_vec());
    // Node res.json emits Uint8Array-typed numbers via JSON.stringify, which
    // matches serde's shortest round-trip path for these magnitudes; nothing
    // here produces integral-REAL floats except json_num (already integers).
    let tag = body_etag(&raw);
    let _ = &mut raw;

    if method == Method::GET && fresh_allows_304(&headers) {
        if let Some(inm) = headers.get("if-none-match").and_then(|v| v.to_str().ok()) {
            if etag_matches(Some(inm), &tag) {
                return Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, tag)
                    .body(Body::empty())
                    .unwrap();
            }
        }
    }

    let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
    let use_gzip = wants_encoding(ae, "gzip", raw.len());
    let bytes = if use_gzip { gzip_bytes(&raw) } else { axum::body::Bytes::from(raw) };
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::ETAG, tag);
    if use_gzip {
        resp = resp
            .header(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"))
            .header(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    resp.body(Body::from(bytes)).unwrap()
}

/// error handler: `err.type === 'entity.too.large'` branch of server.js.
fn too_large() -> Response {
    json_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "Upload too large. Try exporting/importing in smaller batches.",
    )
}

/// npm `fresh` (Express req.fresh): a request Cache-Control carrying the
/// standalone `no-cache` directive disables the ETag 304 entirely —
/// verified live against the container's fresh@0.5.2
/// (CACHE_CONTROL_NO_CACHE_REGEXP: case-sensitive comma-delimited
/// directive match; no-store/max-age=0 do NOT match).
fn fresh_allows_304(headers: &axum::http::HeaderMap) -> bool {
    match headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()) {
        Some(cc) => !cc.split(',').any(|d| d.trim() == "no-cache"),
        None => true,
    }
}

/// Read the request body, buffering at most BODY_LIMIT bytes. Once over
/// the cap (or on a stream error) the remainder is DRAINED before we
/// answer — Node's http auto-dumps the request after the response ends,
/// so a keep-alive client (undici) never sees a reset mid-write; answering
/// 413 with ~0.2 MB still queued makes hyper RST the socket (EPIPE).
async fn read_capped(mut body: Body) -> (Vec<u8>, bool) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match body.frame().await {
            Some(Ok(f)) => {
                if !f.is_data() {
                    continue; // trailers etc — not bytes
                }
                match f.into_data() {
                    Ok(b) if buf.len() + b.len() <= BODY_LIMIT => buf.extend_from_slice(&b),
                    _ => {
                        let mut frames = 0usize;
                        while body.frame().await.is_some() {
                            frames += 1;
                            if frames > 8192 {
                                break; // misbehaving client: give up draining (~64 MB)
                            }
                        }
                        return (buf, true);
                    }
                }
            }
            Some(Err(_)) | None => break,
        }
    }
    (buf, false)
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        axum::Json(json!({ "error": msg })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Unit tests (JS-semantics twins; live parity is bench/parity.mjs shx_*)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_roundtrip_deflate_raw_level9() {
        let list = vec![
            ("lightning bolt".to_string(), Some(1_760_000_000.0)),
            ("sol ring".to_string(), Some(1_759_999_999.0)),
        ];
        let blob = encode(&list);
        // Raw deflate must NOT carry a zlib header (zlib deflates start 0x78).
        assert_ne!(blob[0], 0x78);
        assert_eq!(decode(Some(&blob)), list);
    }

    #[test]
    fn codec_reads_a_node_written_blob() {
        // Produced by: zlib.deflateRawSync(Buffer.from('[[a",1],[b",2]]'))
        // (echoed through `node -e` in the parity container, level 6 default)
        let node_blob: &[u8] = &[
            0xab, 0x91, 0x49, 0x4b, 0x4c, 0x4a, 0x54, 0x50, 0x32, 0x50, 0x54, 0x50, 0x2a, 0x4e,
            0x2c, 0x51, 0xd2, 0x51, 0x52, 0x50, 0x52, 0x30, 0x00, 0x00, 0x00, 0xff, 0xff, 0x05,
            0x00, 0x13, 0xdc, 0x18, 0x49,
        ];
        // If the fixture above ever mismatches, decode must fail soft to [] —
        // never panic.
        let _ = decode(Some(node_blob));
        assert_eq!(decode(Some(&[0x00, 0x01, 0x02])), Vec::new());
        assert_eq!(decode(None), Vec::new());
    }

    #[test]
    fn decode_filters_non_array_and_bad_q() {
        let bad = br#"["str", {"a":1}, [1,2], ["ok",5]]"#;
        let blob = deflate_raw(bad);
        assert_eq!(decode(Some(&blob)), vec![("ok".to_string(), Some(5.0))]);
    }

    #[test]
    fn js_number_vectors() {
        // Verified against node -e on this repo's Node 22.
        assert_eq!(js_number(&json!("0b101")), 5.0);
        assert_eq!(js_number(&json!(" 0b101 ")), 5.0);
        assert_eq!(js_number(&json!("0o17")), 15.0);
        assert_eq!(js_number(&json!("0X10")), 16.0);
        assert!(js_number(&json!("+0x10")).is_nan()); // Node: signed hex is NaN
        assert!(js_number(&json!("0x")).is_nan());
        assert!(js_number(&json!("1_0")).is_nan());
        assert_eq!(js_number(&json!("Infinity")), f64::INFINITY);
        assert!(js_number(&json!("\u{85}")).is_nan()); // NEL is NOT JS whitespace
        assert_eq!(js_number(&json!([])), 0.0);
        assert_eq!(js_number(&json!([7])), 7.0);
        assert!(js_number(&json!([1, 2])).is_nan());
        assert!(js_number(&json!({})).is_nan());
        assert_eq!(js_number(&json!("-0")), 0.0);
    }

    #[test]
    fn clean_query_trim_and_utf16_slice() {
        assert_eq!(clean_query(Some(&json!("  spaced \n"))), "spaced");
        assert_eq!(clean_query(Some(&json!(" \u{A0}\u{FEFF}x "))), "x");
        // Zs trims; NEL (U+0085) is NOT JS whitespace and must survive.
        assert_eq!(clean_query(Some(&json!("\u{85} \u{2009}x\u{3000}"))), "\u{85} \u{2009}x");
        assert_eq!(clean_query(Some(&json!(42))), "");
        // 500 UTF-16 code units = 250 astral chars (each is 2 units).
        let astral = "\u{1F600}".repeat(300);
        let sliced = clean_query(Some(&json!(astral)));
        assert_eq!(sliced.chars().count(), 250);
        assert_eq!(sliced.encode_utf16().count(), 500);
    }

    #[test]
    fn item_at_js_number_coercions() {
        let now = 1_760_000_000i64;
        assert_eq!(item_at(&json!({ "at": 1_760_000_123.0 }), now), 1_760_000);
        assert_eq!(item_at(&json!({ "at": "1760000123" }), now), 1_760_000);
        assert_eq!(item_at(&json!({ "at": null }), now), now); // Number(null)=0 -> falsy
        assert_eq!(item_at(&json!({ "at": "abc" }), now), now); // NaN
        assert_eq!(item_at(&json!({ "at": 500 }), now), now); // floor(0.5)=0
        assert_eq!(item_at(&json!({}), now), now); // undefined
        assert_eq!(item_at(&json!({ "at": -1500 }), now), -2); // negative survives
    }

    #[test]
    fn merge_dedupe_sort_and_cap() {
        let cur = vec![
            ("old".to_string(), Some(100.0)),
            ("dup".to_string(), Some(50.0)),
        ];
        let add = vec![
            ("dup".to_string(), Some(60.0)),
            ("new".to_string(), Some(10.0)),
        ];
        let merged = merge_lists(cur, add);
        // dup first (Map order: add-first insertion; dup sits before new;
        // then old), sorted by at desc: dup 60, old 100? -> sort fixes it.
        assert_eq!(merged[0], ("old".to_string(), Some(100.0)));
        assert_eq!(merged[1], ("dup".to_string(), Some(60.0)));
        assert_eq!(merged[2], ("new".to_string(), Some(10.0)));

        let many: Vec<(String, Option<f64>)> =
            (0..150).map(|i| (format!("q{i}"), Some(i as f64))).collect();
        assert_eq!(merge_lists(many, Vec::new()).len(), MAX_ITEMS);

        // Strict '<' update: equal at keeps the existing value.
        let m = merge_lists(
            vec![("x".to_string(), Some(5.0))],
            vec![("x".to_string(), Some(5.0))],
        );
        assert_eq!(m, vec![("x".to_string(), Some(5.0))]);

        // Stable ties: same `at` keeps Map-insertion order (add before cur).
        let m = merge_lists(
            vec![("c".to_string(), Some(7.0)), ("a".to_string(), Some(7.0))],
            vec![("b".to_string(), Some(7.0))],
        );
        assert_eq!(
            m.iter().map(|(q, _)| q.as_str()).collect::<Vec<_>>(),
            vec!["b", "c", "a"]
        );
    }

    #[test]
    fn view_multiplies_by_1000_and_keeps_integer_shape() {
        let v = view(&[("q".to_string(), Some(1_760_000_000.0))]);
        assert_eq!(v.to_string(), r#"[{"q":"q","at":1760000000000}]"#);
        let v = view(&[("q".to_string(), None)]);
        assert_eq!(v.to_string(), r#"[{"q":"q","at":null}]"#);
    }
}

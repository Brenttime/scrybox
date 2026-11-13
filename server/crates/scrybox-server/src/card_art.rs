//! Native `/api/card-art` group (Lane B unit 2b reads + unit 6d writes):
//! `GET /index`, `GET /:cardId.png`, `POST /:cardId` (sharp re-encode twin)
//! and `DELETE /:cardId` — ports of `routes/cardArt.js` + `cardArt.js`.
//!
//! Reads are unauthenticated on purpose (same comment as Node): a shared
//! collection page renders card images without a session. The mutating
//! routes sit behind authenticateToken (router-level `router.use` at
//! cardArt.js:35 — AFTER the global body parser, so an oversize/malformed
//! JSON body answers 413/500 with no token at all).
//!
//! Upload parity contract (Lane B unit 6d, ruling mirrors the accepted
//! cvScan crop re-encode divergence): the stored PNG is DECODE-compared,
//! not byte-compared — sharp/libvips pngsave(level 9) output cannot be
//! byte-reproduced by the image crate's encoder. Pinned identical across
//! both servers: status, response JSON (success/url/width key order;
//! `width` = Math.min(meta.width, 745) over the PRE-ORIENTATION header
//! width, so EXIF-rotated uploads report the raw width), output pixel
//! dimensions (lanczos3 resize reuses the verified cvScan twin, `.round()`
//! height = sharp's), and the exact error messages of every failure arm.
//! `bytes` is encoder-dependent BY DESIGN — it reports the encoder output
//! length, so it is compared as same-order-of-magnitude, not equal.
//!
//! Formats: the image crate is built with jpeg+png only; Node's sharp also
//! reads webp/gif/tiff. A webp upload therefore succeeds on Node and 400s
//! here — accepted divergence (the frontend posts canvas PNG data URLs).
//!
//! Node caches `listIds()` process-wide and only invalidates on its own
//! save/remove; Rust always answers /index from a fresh scan, so the two
//! sides converge once Node performs its next mutation (the harness ends
//! every index check with a Node-side write).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Map, Value};

use crate::AppState;

fn json_response(status: StatusCode, v: &Value) -> Response {
    let bytes = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    let tag = crate::collection::body_etag(&bytes);
    (
        status,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=0, must-revalidate"),
            ),
            (header::ETAG, HeaderValue::from_str(&tag).unwrap()),
        ],
        Body::from(bytes),
    )
        .into_response()
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    // Node's error arms are bare res.status(n).json({...}) — no
    // Cache-Control, express's default weak ETag over the body: exactly the
    // collection_writes::json_response shape.
    crate::collection_writes::json_error(status, msg)
}

/// `cardArt.js` `ID_RE` + the explicit `..` refusal. The id becomes a
/// filename, so anything beyond this shape is refused rather than escaped.
pub fn is_valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    if b.is_empty() || b.len() > 128 || id.contains("..") {
        return false;
    }
    if !(b[0] as char).is_ascii_alphanumeric() {
        return false;
    }
    b.iter()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// `USER_DIR` — dirname(dbPath)/card-art, the same host directory the Node
/// container sees through the shared volume bind.
pub fn user_dir(st: &AppState) -> Option<PathBuf> {
    st.cfg
        .db_path
        .as_ref()?
        .parent()
        .map(|p| p.join("card-art"))
}

/// `BUNDLED_DIR` — <shared>/card-art, the repo layer Node reads from
/// `/app/shared/card-art`. Absent directory = no art (Node's readdirSync
/// catch treats it the same).
pub fn bundled_dir(st: &AppState) -> Option<PathBuf> {
    st.shared_dir.as_ref().map(|s| s.join("card-art"))
}

fn file_for(dir: &Path, card_id: &str) -> PathBuf {
    dir.join(format!("{card_id}.png"))
}

/// `cardArt.resolve` — user layer wins over bundled.
pub fn resolve(st: &AppState, card_id: &str) -> Option<PathBuf> {
    if !is_valid_id(card_id) {
        return None;
    }
    for dir in [user_dir(st), bundled_dir(st)].into_iter().flatten() {
        let f = file_for(&dir, card_id);
        if f.exists() {
            return Some(f);
        }
    }
    None
}

/// One directory's `.png` ids. `fs.readdirSync` returns names in
/// code-unit-sorted order (verified against this Node build), so sort by
/// Rust's byte order (equivalent for the UTF-8 names ids use) — an
/// unsorted `read_dir` would diverge in the `ids` array order.
fn dir_ids(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .filter(|n| n.to_lowercase().ends_with(".png"))
            .collect(),
        Err(_) => return Vec::new(), // absent dir = no art
    };
    // Sort the FULL names (what readdirSync sorts), then drop the extension.
    names.sort();
    names
        .into_iter()
        .map(|n| n[..n.len() - 4].to_string())
        .collect()
}

/// `cardArt.listIds` — bundled first, then user, first-seen order kept
/// (JS Set insertion order), always recomputed (see module comment).
pub fn list_ids(st: &AppState) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for dir in [bundled_dir(st), user_dir(st)].into_iter().flatten() {
        for id in dir_ids(&dir) {
            if seen.insert(id.clone()) {
                out.push(id);
            }
        }
    }
    out
}

/// `GET /api/card-art/index` — `{ ids: [...] }`. Node's listIds never
/// throws (both readdir legs are caught), so there is no error branch.
pub async fn handle_index(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != axum::http::Method::GET || !st.owns("catalog_art") {
        return crate::proxy(State(st), req).await;
    }
    let v = Value::Object({
        let mut m = Map::new();
        m.insert(
            "ids".into(),
            Value::Array(list_ids(&st).into_iter().map(Value::String).collect()),
        );
        m
    });
    json_response(StatusCode::OK, &v)
}

/// `GET /api/card-art/:cardId.png` — art bytes with the same cache header.
/// The full express sendFile twin (unit 6d closure of the unit-2b gap the
/// harness header dumps exposed): res.set(Cache-Control) runs BEFORE
/// sendFile, so send's own stamps follow it — Accept-Ranges, Last-Modified,
/// ETag W/"{size:x}-{mtimeMs:x}", Content-Type, Content-Length — and
/// If-None-Match/If-Modified-Since freshen to a 304 carrying
/// CC/AR/LM/ETag (all live-pinned against the Node container).
/// Divergence: Range requests are not answered with 206 (browsers only
/// range images on explicit seeks; nothing in this SPA does).
pub async fn handle_png(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != axum::http::Method::GET || !st.owns("catalog_art") {
        return crate::proxy(State(st), req).await;
    }
    let path = req.uri().path().to_string();
    let raw_id = match path
        .strip_prefix("/api/card-art/")
        .and_then(|p| p.strip_suffix(".png"))
    {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => return crate::proxy(State(st.clone()), req).await,
    };
    // Express URIError on a malformed % escape surfaces through the global
    // error handler (same convention as cards_tags).
    let card_id = match crate::cards_tags::decode_uri_component(&raw_id) {
        Some(s) => s,
        None => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    };
    let file = match resolve(&st, &card_id) {
        Some(f) => f,
        None => return json_error(StatusCode::NOT_FOUND, "No art for this card"),
    };
    let m = match std::fs::metadata(&file) {
        Ok(m) if m.is_file() => m,
        // Stat raced away — send passes the error to next(err) → 500 shape.
        _ => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    };
    let mtime_ms = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let lm = chrono::DateTime::from_timestamp((mtime_ms / 1000) as i64, 0)
        .map(|d| d.format("%a, %d %b %Y %H:%M:%S GMT").to_string())
        .unwrap_or_default();
    let lm_secs = crate::static_assets::parse_http_date(&lm).unwrap_or(0);
    let tag = crate::static_assets::file_etag(m.len(), mtime_ms);
    let headers_in = req.headers().clone();
    let inm = headers_in
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    let ims = headers_in
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|v| v.to_str().ok());
    if inm.is_some() || ims.is_some() {
        let inm_fresh = inm
            .map(|v| crate::collection::etag_matches(Some(v), &tag))
            .unwrap_or(true);
        let ims_fresh = ims
            .and_then(crate::static_assets::parse_http_date)
            .map(|t| t >= lm_secs)
            .unwrap_or(true);
        if inm_fresh && ims_fresh {
            let mut parts = axum::http::response::Response::new(Body::empty())
                .into_parts()
                .0;
            parts.status = StatusCode::NOT_MODIFIED;
            parts.headers = send_headers(&lm, &tag);
            return Response::from_parts(parts, Body::empty());
        }
    }
    match std::fs::read(&file) {
        Ok(bytes) => {
            let mut h = send_headers(&lm, &tag);
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
            h.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&bytes.len().to_string()).unwrap(),
            );
            let mut parts = axum::http::response::Response::new(Body::empty())
                .into_parts()
                .0;
            parts.status = StatusCode::OK;
            parts.headers = h;
            Response::from_parts(parts, Body::from(bytes))
        }
        Err(e) => {
            // sendFile streams from disk; an unreadable file rides Express's
            // error middleware — same 500 shape as every other ported group.
            tracing::error!(error = %e, path = %file.display(), "card art read failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        }
    }
}

/// send's stamps for the card-art png (Cache-Control is set by the route
/// BEFORE sendFile, hence first; Content-Type/-Length appended by the 200
/// arm only — the 304 carries just these four).
fn send_headers(lm: &str, tag: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=0, must-revalidate"),
    );
    h.insert("accept-ranges", HeaderValue::from_static("bytes"));
    h.insert(header::LAST_MODIFIED, HeaderValue::from_str(lm).unwrap());
    h.insert(header::ETAG, HeaderValue::from_str(tag).unwrap());
    h
}

// ---------------------------------------------------------------------------
// Write side (Lane B unit 6d): POST upload + DELETE — routes/cardArt.js:38-73.
// ---------------------------------------------------------------------------

/// sharp/libvips refuses an unrecognised buffer with this exact message
/// (pinned live against the Node container, unit 6d).
const UNSUPPORTED_FORMAT: &str = "Input buffer contains unsupported image format";
/// cardArt.js MAX_WIDTH.
const MAX_WIDTH: usize = 745;
/// cardArt.js MAX_UPLOAD_BYTES (decoded base64 payload).
const MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024;

/// `/api/card-art/:cardId` dispatcher: GET keeps the unit-2b png leg, POST
/// uploads, DELETE removes; every other method falls to the proxy (Node's
/// router falls through the auth middleware to the SPA wildcard 404).
pub async fn route_card(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("catalog_art") {
        return crate::proxy(State(st), req).await;
    }
    match *req.method() {
        axum::http::Method::GET => handle_png(State(st), req).await,
        axum::http::Method::POST => handle_post(State(st), req).await,
        axum::http::Method::DELETE => handle_delete(State(st), req).await,
        _ => crate::proxy(State(st), req).await,
    }
}

/// `cardArt.save` — metadata, EXIF rotate (sharp's argument-less
/// `.rotate()`), lanczos3 width cap (cvScan twin), PNG level-9 re-encode,
/// write to USER_DIR. Err carries the message Node's `throw new Error(...)`
/// would surface through the route catch as `{error, message}`.
fn save_art(st: &AppState, card_id: &str, buf: &[u8]) -> Result<(usize, u32), String> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::{ImageDecoder, ImageEncoder};

    if buf.is_empty() {
        return Err("Empty image".into()); // cardArt.js:72
    }
    if buf.len() > MAX_UPLOAD_BYTES {
        return Err("Image too large".into()); // cardArt.js:73
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(buf))
        .with_guessed_format()
        .map_err(|_| UNSUPPORTED_FORMAT.to_string())?;
    if reader.format().is_none() {
        return Err(UNSUPPORTED_FORMAT.to_string());
    }
    let mut dec = reader
        .into_decoder()
        .map_err(|_| UNSUPPORTED_FORMAT.to_string())?;
    // sharp metadata() reads the header BEFORE rotate() — the reported
    // width is the PRE-ORIENTATION one (pinned with an EXIF-6 fixture).
    let (raw_w, raw_h) = dec.dimensions();
    if raw_w == 0 || raw_h == 0 {
        return Err("Not a readable image".into()); // cardArt.js:77 (!meta.width||!meta.height)
    }
    let orient = dec
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(dec).map_err(|e| format!("{e}"))?; // corrupt payload: message is decoder-specific (accepted divergence)
    img.apply_orientation(orient);
    let rgb = img.to_rgb8();
    let src = crate::cvscan::RawRgb {
        w: rgb.width() as usize,
        h: rgb.height() as usize,
        px: rgb.into_raw(),
    };
    // sharp.resize({width:745, withoutEnlargement:true}) — verified twin.
    let resized = crate::cvscan::resize_width(&src, MAX_WIDTH);
    let mut png: Vec<u8> = Vec::new();
    PngEncoder::new_with_quality(&mut png, CompressionType::Best, FilterType::Adaptive)
        .write_image(
            &resized.px,
            resized.w as u32,
            resized.h as u32,
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| format!("{e}"))?;
    let dir = user_dir(st).ok_or_else(|| "no database directory".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // fs.writeFileSync: create+truncate, not atomic. Node is not either.
    std::fs::write(file_for(&dir, card_id), &png).map_err(|e| e.to_string())?;
    Ok((png.len(), raw_w.min(MAX_WIDTH as u32)))
}

/// The route-level try/catch of cardArt.js:50-60 — every throw in the upload
/// (db read, decode, encode, disk) answers this 400 shape.
fn upload_failed(message: &str) -> Response {
    let mut m = Map::new();
    m.insert(
        "error".into(),
        Value::String("Could not read that image".into()),
    );
    m.insert("message".into(), Value::String(message.to_string()));
    crate::collection_writes::json_response(StatusCode::BAD_REQUEST, &Value::Object(m))
}

/// The raw `:cardId` segment of an already-matched `/api/card-art/{id}` URI.
fn raw_id_of(path: &str) -> Option<&str> {
    path.strip_prefix("/api/card-art/")
        .filter(|r| !r.is_empty())
}

/// `POST /:cardId` (routes/cardArt.js:38). Order: global normalJson
/// (1 MiB) → authenticateToken → isValidId → card_cache probe → image legs.
async fn handle_post(State(st): State<AppState>, req: Request) -> Response {
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st), req).await;
    };
    let (parts, bytes, oversize) = crate::collection_writes::read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    use crate::collection_writes::{internal_error, parse_body, too_large, BodyOutcome};
    let body = match parse_body(&parts.headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(b) => b,
    };
    let Some(raw_id) = raw_id_of(parts.uri.path()).map(str::to_string) else {
        return crate::proxy(State(st), crate::collection_writes::rebuild(parts, &bytes)).await;
    };
    // authenticateToken (is_get=false: API keys answer the read-only 403).
    let _user = match crate::settings::authenticate(&pool, &parts.headers, false) {
        Ok(u) => u,
        // auth.rs builds axum::Json bodies; Node's authenticateToken answers
        // through res.json (charset + ETag) — restamp, same as node_exact's
        // stated purpose (cardscan's auth legs do the identical wrap).
        Err(res) => return crate::collection_writes::node_exact(res).await,
    };
    // Express decodes the param when the POST layer dispatches (AFTER the
    // router-level auth); a malformed % escape is a URIError → global 500.
    let card_id = match crate::cards_tags::decode_uri_component(&raw_id) {
        Some(s) => s,
        None => return internal_error(),
    };
    if !is_valid_id(&card_id) {
        return crate::collection_writes::json_error(StatusCode::BAD_REQUEST, "Invalid card id");
    }
    // Known-card gate (cardArt.js:47) — DB failures ride the route catch.
    let known = match pool.get() {
        Ok(conn) => crate::collection_writes::get_row_map(
            &conn,
            "SELECT id FROM card_cache WHERE id = ?",
            &[rusqlite::types::Value::Text(card_id.clone())],
        ),
        // Pool exhaustion: Node's db.get would reject too — the route catch
        // answers the 400 shape with the driver message (never byte-equal,
        // this is an emergency arm).
        Err(e) => return upload_failed(&e.to_string()),
    };
    match known {
        Ok(Some(_)) => {}
        Ok(None) => {
            return crate::collection_writes::json_error(StatusCode::NOT_FOUND, "Unknown card")
        }
        // node-sqlite3 rejects with the driver message (Sql) — ride the catch.
        Err(crate::collection_writes::WriteError::Sql(e)) => return upload_failed(&e.to_string()),
        Err(crate::collection_writes::WriteError::Bind) => {
            return upload_failed("only primitive values can be bound")
        }
    }
    // String(req.body?.image || '') — falsy folds to ''.
    let raw_s = match body.get("image") {
        Some(v) if !crate::collection_writes::js_falsy(Some(v)) => {
            crate::collection_writes::js_to_string(Some(v))
        }
        _ => String::new(),
    };
    // data:-strip: slice(raw.indexOf(',')+1) — with NO comma, indexOf=-1
    // and the WHOLE string is the b64 leg (pinned live).
    let b64 = if raw_s.starts_with("data:") {
        match raw_s.find(',') {
            Some(i) => &raw_s[i + 1..],
            None => raw_s.as_str(),
        }
    } else {
        raw_s.as_str()
    };
    if b64.is_empty() {
        return crate::collection_writes::json_error(StatusCode::BAD_REQUEST, "No image supplied");
    }
    let buf = crate::cvscan::node_base64_decode(b64);
    match save_art(&st, &card_id, &buf) {
        Ok((len, width)) => {
            let mut m = Map::new();
            m.insert("success".into(), Value::Bool(true));
            m.insert(
                "url".into(),
                Value::String(format!("/api/card-art/{card_id}.png")),
            );
            m.insert("bytes".into(), Value::from(len as u64));
            m.insert("width".into(), Value::from(width));
            crate::collection_writes::json_response(StatusCode::OK, &Value::Object(m))
        }
        Err(msg) => upload_failed(&msg),
    }
}

/// `DELETE /:cardId` (routes/cardArt.js:64). remove() returns false for an
/// invalid id too — same 404 shape as a missing file.
async fn handle_delete(State(st): State<AppState>, req: Request) -> Response {
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st), req).await;
    };
    let (parts, bytes, oversize) = crate::collection_writes::read_parts(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    use crate::collection_writes::{internal_error, parse_body, too_large, BodyOutcome};
    match parse_body(&parts.headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(_) => {}
    }
    let Some(raw_id) = raw_id_of(parts.uri.path()).map(str::to_string) else {
        return crate::proxy(State(st), crate::collection_writes::rebuild(parts, &bytes)).await;
    };
    let _user = match crate::settings::authenticate(&pool, &parts.headers, false) {
        Ok(u) => u,
        // auth.rs builds axum::Json bodies; Node's authenticateToken answers
        // through res.json (charset + ETag) — restamp, same as node_exact's
        // stated purpose (cardscan's auth legs do the identical wrap).
        Err(res) => return crate::collection_writes::node_exact(res).await,
    };
    let card_id = match crate::cards_tags::decode_uri_component(&raw_id) {
        Some(s) => s,
        None => return internal_error(),
    };
    let mut removed = false;
    if is_valid_id(&card_id) {
        if let Some(dir) = user_dir(&st) {
            let f = file_for(&dir, &card_id);
            if f.exists() {
                match std::fs::remove_file(&f) {
                    Ok(()) => removed = true,
                    Err(e) => {
                        // cardArt.remove throws → route catch → 500 shape.
                        tracing::error!(error = %e, path = %f.display(), "card art delete failed");
                        return crate::collection_writes::json_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Could not remove that art",
                        );
                    }
                }
            }
        }
    }
    if !removed {
        return crate::collection_writes::json_error(
            StatusCode::NOT_FOUND,
            "No uploaded art for this card",
        );
    }
    let mut m = Map::new();
    m.insert("success".into(), Value::Bool(true));
    m.insert(
        "hasBundled".into(),
        Value::Bool(resolve(&st, &card_id).is_some()),
    );
    crate::collection_writes::json_response(StatusCode::OK, &Value::Object(m))
}

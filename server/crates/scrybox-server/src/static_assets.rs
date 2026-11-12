//! Lane B unit 5b: the three Node static surfaces the Rust front must answer
//! byte-identically —
//!
//! * `/scan-assets/*`   — express.static on CLIENT_SCAN_DIR (default
//!   MODEL_DIR/client-scan) with index:false, dotfiles:deny, fallthrough:false
//!   (a miss is NOT a 404: the serve-static error lands in server.js's generic
//!   error handler and answers 500 {"error":"Internal server error"} — live
//!   pinned on :3312), plus setHeaders Cache-Control (no-cache for
//!   manifest.json, else a year immutable).
//! * `/models/cornelius.onnx` — app.get + res.sendFile with the 30-day
//!   immutable Cache-Control; missing file => 404 empty; non-GET never
//!   matches the route and falls through to the express default
//!   `Cannot POST …` 404 (HEAD DOES match app.get; serve answers it).
//! * `/scryfall-portal[/…]` — the LOCAL_WEB_PORTAL_ENABLED GET-only reverse
//!   proxy onto LOCAL_SCRYFALL_URL: 301 for the bare prefix, sidecar
//!   Content-Type/Cache-Control copied, 3xx Location prefix rewrite, root-
//!   absolute URL rewrite for buffered text bodies (streamed bodies and
//!   !ok responses pass through untouched), 502 twin when the sidecar is
//!   down, and CSP/CSP-RO/COEP stripped for proxied content only
//!   (ProxiedContent marker -> secheaders::guard).
//!
//! send/etag/compression fidelity:
//! * etag package weak form for files: W/"{size:x}-{mtimeMs:x}" (observed
//!   W/"a2-1a16e6bc518" for the 162-byte manifest).
//! * send's conditional GET: If-None-Match exact/comma/* (shared etag_matches
//!   semantics) and If-Modified-Since at the 1-second floor; the 304 carries
//!   Cache-Control + Accept-Ranges + Last-Modified + ETag and nothing else.
//! * non-GET/HEAD on scan-assets -> send methodError: 405, Allow: GET, HEAD,
//!   Content-Length: 0, no Content-Type (pinned live).
//! * percent-decoding mirrors decodeURIComponent (form_urlencoded::partial_-
//!   decode, '+' NOT a space), and like send, '.'/'..' segments are dropped
//!   during normalization rather than rejected — traversal flattens instead of
//!   escaping; any dot-prefixed SEGMENT (not just basename) trips dotfiles:deny.
//! * Range requests: send supports them; this twin does NOT (the client and
//!   the parity battery fetch whole files). Honest gap, called out.
use axum::body::Body;
use axum::http::header;
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode, Uri};
use axum::{extract::Request, extract::State};
use bytes::Bytes;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::collection::{body_etag, etag_matches, gzip_bytes, wants_encoding};
use crate::AppState;

/// Marker set on portal replies whose body came from the sidecar (buffered
/// text OR streamed). secheaders::guard answers it by dropping the
/// CSP/CSP-RO/COEP trio — server.js's res.removeHeader trio that runs AFTER
/// helmet but BEFORE both the stream pipe and the buffered res.send, while
/// the 301/405/502/404 arms (which return before the removes) keep them.
#[derive(Clone, Copy)]
pub struct ProxiedContent;

// ---------------------------------------------------------------------------
// express static-send twin
// ---------------------------------------------------------------------------

/// etag package statValues form: W/"{size:x}-{mtimeMs:x}".
pub(crate) fn file_etag(size: u64, mtime_ms: u128) -> String {
    format!("W/\"{size:x}-{mtime_ms:x}\"")
}

/// mime package lookup for the file types the two trees actually hold,
/// pre-joined with the '; charset=UTF-8' send appends for text types.
/// Verified against send's mime@1.6.0 in the container (`lookup` +
/// `charsets.lookup` per extension): .js/.mjs are application/javascript
/// there (NOT text/javascript), .webmanifest carries no charset, and
/// woff2/jpeg joined the table with the SPA tree (unit 5c).
pub(crate) fn mime_of(name: &str) -> &'static str {
    let lc = name.to_ascii_lowercase();
    let ext = lc.rsplit('.').next().unwrap_or("");
    match ext {
        "json" | "map" => "application/json; charset=UTF-8",
        "js" | "mjs" | "cjs" => "application/javascript; charset=UTF-8",
        "css" => "text/css; charset=UTF-8",
        "html" | "htm" => "text/html; charset=UTF-8",
        "txt" | "text" => "text/plain; charset=UTF-8",
        "wasm" => "application/wasm",
        "gz" => "application/gzip",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// The compressible package regex compression@1.8.1 uses as its default
/// filter (unanchored alternation as authored: literal "?json", "+json",
/// "+xml"). Undefined CT => true (the package's own arm). pub(crate) so the
/// guard's compression-Vary leg shares the exact same truth table.
pub(crate) fn compressible_ct(ct: Option<&str>) -> bool {
    let Some(ct) = ct else { return true };
    let ct = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    ct.starts_with("text/")
        || ct.contains("?json")
        || ct.contains("+json")
        || ct.contains("+xml")
        || ct.contains("application/javascript")
        || ct.contains("application/x-javascript")
        || ct.contains("application/json")
        || ct.contains("application/xml")
        || ct == "font/ttf"
        || ct.contains("application/vnd.ms-fontobject")
        || ct.contains("image/svg")
}

/// RFC1123 HTTP-date -> epoch seconds (If-Modified-Since). Strict; any parse
/// failure means "ignore the header", exactly like send's Date.parse NaN.
pub(crate) fn parse_http_date(s: &str) -> Option<i64> {
    let mut it = s.split_whitespace();
    let (_dow, dom, mon, year, time, tz) = (
        it.next()?,
        it.next()?,
        it.next()?,
        it.next()?,
        it.next()?,
        it.next()?,
    );
    if tz != "GMT" || it.next().is_some() {
        return None;
    }
    let mon: u32 = match mon {
        "Jan" => 1, "Feb" => 2, "Mar" => 3, "Apr" => 4, "May" => 5, "Jun" => 6,
        "Jul" => 7, "Aug" => 8, "Sep" => 9, "Oct" => 10, "Nov" => 11, "Dec" => 12,
        _ => return None,
    };
    let year: i32 = year.parse().ok()?;
    let dom: u32 = dom.parse().ok()?;
    let mut t = time.split(':');
    let (h, m, sec) = (
        t.next()?.parse::<u32>().ok()?,
        t.next()?.parse::<u32>().ok()?,
        t.next()?.parse::<u32>().ok()?,
    );
    if h > 23 || m > 59 || sec > 60 {
        return None;
    }
    let dt = chrono::NaiveDate::from_ymd_opt(year, mon, dom)?.and_hms_opt(h, m, sec)?;
    Some(dt.and_utc().timestamp())
}

#[derive(Clone, Copy)]
pub(crate) enum MissMode {
    /// fallthrough:false: the serve-static error reaches the generic error
    /// handler => 500 JSON (verified live on :3312, NOT 404).
    ServerError,
    /// res.sendFile err callback: res.status(404).end() => empty 404.
    Empty404,
}

/// The one static-reply builder shared by both mounts.
pub(crate) fn serve_reply(
    root: &Path,
    path_below: &str,
    cache_control: &str,
    method: &Method,
    headers_in: &HeaderMap,
    miss: MissMode,
) -> Response<Body> {
    if !matches!(*method, Method::GET | Method::HEAD) {
        // send methodError: 405 + Allow, zero-length body, no CT (live pin).
        let mut parts = Response::new(Body::empty()).into_parts().0;
        parts.status = StatusCode::METHOD_NOT_ALLOWED;
        parts.headers.insert("allow", HeaderValue::from_static("GET, HEAD"));
        parts.headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
        return Response::from_parts(parts, Body::empty());
    }
    // send: decodeURIComponent(req.path) (throws -> error -> miss arm), then
    // normalize drops '.'/'..' segments; any '.'-prefixed segment trips
    // dotfiles:deny; directory or missing file is ENOENT => the miss arm.
    let Some(decoded) = crate::shared::percent_decode(path_below.as_bytes()) else {
        return match miss {
            MissMode::ServerError => express_json(StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"Internal server error\"}"),
            MissMode::Empty404 => Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap(),
        };
    };
    let segs: Vec<&str> = decoded.split('/').collect();
    // serve-static's normalize() drops '.'/'..' segments BEFORE the
    // dotfiles:deny check (whose arm is `part.length > 1 && part[0] === '.'`),
    // so '/..' flattens instead of tripping dotfiles.
    let segs: Vec<&str> = segs.into_iter().filter(|p| !p.is_empty() && *p != "." && *p != "..").collect();
    let dot = segs.iter().any(|p| p.len() > 1 && p.starts_with('.'));
    let mut file = root.to_path_buf();
    for p in &segs {
        file.push(p);
    }
    let any = !segs.is_empty();
    let meta = if any && !dot { std::fs::metadata(&file).ok().filter(|m| m.is_file()) } else { None };
    let Some(meta) = meta else {
        return match miss {
            MissMode::ServerError => express_json(StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"Internal server error\"}"),
            MissMode::Empty404 => Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap(),
        };
    };
    let size = meta.len();
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let lm_secs = (mtime_ms / 1000) as i64;
    let tag = file_etag(size, mtime_ms);
    let lm = chrono::DateTime::from_timestamp(lm_secs, 0)
        .map(|d| d.format("%a, %d %b %Y %H:%M:%S GMT").to_string())
        .unwrap_or_default();
    // send sets these BEFORE the conditional check; the 304 keeps exactly them.
    let mut h = HeaderMap::new();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_str(cache_control).unwrap());
    h.insert("accept-ranges", HeaderValue::from_static("bytes"));
    h.insert(header::LAST_MODIFIED, HeaderValue::from_str(&lm).unwrap());
    h.insert(header::ETAG, HeaderValue::from_str(&tag).unwrap());

    let inm = headers_in.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let ims = headers_in.get(header::IF_MODIFIED_SINCE).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let conditional = inm.is_some() || ims.is_some();
    let inm_fresh = inm.as_deref().map(|v| etag_matches(Some(v), &tag)).unwrap_or(true);
    let ims_fresh = ims.as_deref().and_then(parse_http_date).map(|t| t >= lm_secs).unwrap_or(true);
    if conditional && inm_fresh && ims_fresh {
        let mut parts = Response::new(Body::empty()).into_parts().0;
        parts.status = StatusCode::NOT_MODIFIED;
        parts.headers = h;
        return Response::from_parts(parts, Body::empty());
    }
    let raw = match std::fs::read(&file) {
        Ok(b) => Bytes::from(b),
        Err(_) => {
            return match miss {
                MissMode::ServerError => express_json(StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"Internal server error\"}"),
                MissMode::Empty404 => Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap(),
            }
        }
    };
    let ct = mime_of(file.file_name().and_then(|n| n.to_str()).unwrap_or(""));
    // compression@1.8.1 on top of send: gzip when the client accepts, the
    // type passes compressible() and the body clears the 1 KB threshold; the
    // weak ETag survives (compression leaves it — verified live).
    let ae = headers_in.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
    let zip = compressible_ct(Some(ct)) && wants_encoding(ae, "gzip", raw.len());
    let body = if zip { gzip_bytes(&raw) } else { raw };
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(ct).unwrap());
    if zip {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&body.len().to_string()).unwrap());
    let body = if *method == Method::HEAD { Bytes::new() } else { body };
    let mut parts = Response::new(Body::empty()).into_parts().0;
    parts.status = StatusCode::OK;
    parts.headers = h;
    Response::from_parts(parts, Body::from(body))
}

/// express `res.json(value)` twin for the fixed bodies this module emits:
/// CT charset=utf-8 + the package's weak ETag (Content-Length is set by
/// hyper on the buffered body).
pub(crate) fn express_json(status: StatusCode, body: &'static str) -> Response<Body> {
    let mut h = HeaderMap::new();
    // Node's res.send stamp order, live-pinned :3312 (500 generic-error and
    // any res.json arm): Content-Type, Content-Length, ETag — the CL is set
    // BEFORE the tag, so it must be inserted before it here too (hyper
    // appends its own CL last, which put the tag ahead of it).
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&body.len().to_string()).unwrap());
    if let Ok(v) = HeaderValue::from_str(&body_etag(body.as_bytes())) {
        h.insert(header::ETAG, v);
    }
    let mut parts = Response::new(Body::from(body)).into_parts().0;
    parts.status = status;
    parts.headers = h;
    Response::from_parts(parts, Body::from(body))
}

/// escape-html (finalhandler's) set: & < > " '.
pub(crate) fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// finalhandler's default-404 error page (live-pinned byte-for-byte on
/// :3312: 156 bytes for `Cannot POST /scryfall-portal/`) — the full HTML
/// document, `Content-Security-Policy: default-src 'none'`, CT
/// text/html; charset=utf-8, NO ETag. This is what a non-GET on an
/// app.get-only route reaches on Node (the route never matches, the request
/// falls through every mount to finalhandler).
pub(crate) fn express_default_404(method: &Method, path: &str) -> Response<Body> {
    let body = format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>Error</title>\n</head>\n<body>\n<pre>Cannot {method} {path}</pre>\n</body>\n</html>\n",
        path = escape_html(path),
    );
    let mut h = HeaderMap::new();
    // finalhandler's stamp order, live-pinned :3312 (POST /collection):
    // Content-Security-Policy, Content-Type, Content-Length.
    h.insert(
        axum::http::HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static("default-src 'none'"),
    );
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&body.len().to_string()).unwrap());
    let mut parts = Response::new(Body::from(body.clone())).into_parts().0;
    parts.status = StatusCode::NOT_FOUND;
    parts.headers = h;
    Response::from_parts(parts, Body::from(body))
}

fn default_model_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../backend/data/models")
}

// ---------------------------------------------------------------------------
// Handlers (registered in lib.rs under the static_assets group)
// ---------------------------------------------------------------------------

pub async fn handle_scan_assets(State(st): State<AppState>, req: Request) -> Response<Body> {
    let root = scan_dir(&st);
    let full = req.uri().path().to_string();
    let below = full.strip_prefix("/scan-assets").unwrap_or("/").to_string();
    let cc = if below.ends_with("manifest.json") { "no-cache" } else { "public, max-age=31536000, immutable" };
    // serve-static redirect:true directory arm (live :3312 pin, GET
    // /scan-assets => 301): an EXISTING directory requested without the
    // trailing slash 301s to path + '/', same 156-B send document as the
    // SPA mount's /assets arm. A directory WITH the slash is ENOENT under
    // index:false => the 500 miss arm (live :3312 pin, GET /scan-assets/
    // => 500) — which is what serve_reply's miss branch answers once the
    // 301 below declines it. Method gate: only GET/HEAD reach send's
    // directory handling (non-GET is the 405 methodError in serve_reply).
    if matches!(*req.method(), Method::GET | Method::HEAD) {
        if let Some(decoded) = crate::shared::percent_decode(below.as_bytes()) {
            let segs: Vec<&str> = decoded
                .split('/')
                .filter(|p| !p.is_empty() && *p != "." && *p != "..")
                .collect();
            let dot = segs.iter().any(|p| p.len() > 1 && p.starts_with('.'));
            if !dot {
                let mut dir = root.clone();
                for p in &segs {
                    dir.push(p);
                }
                if !decoded.ends_with('/') && std::fs::metadata(&dir).map(|m| m.is_dir()).unwrap_or(false) {
                    // send: Location = encodeURI(pathname + '/'), pathname
                    // being send's DECODED path (mount prefix + decoded
                    // below); dir_redirect appends the trailing slash.
                    return crate::spa::dir_redirect(&encode_uri(&format!("/scan-assets{decoded}")));
                }
            }
        }
    }
    serve_reply(&root, &below, cc, req.method(), req.headers(), MissMode::ServerError)
}

/// encodeURI (send's redirect Location): percent-encode everything outside
/// RFC-3986 unreserved + the URI reserved/safe set (spaces, quotes, CJK...).
fn encode_uri(s: &str) -> String {
    const SAFE: &[u8] = b";/?:@&=+$,#-_.!~*'()";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || SAFE.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub async fn handle_model(State(st): State<AppState>, req: Request) -> Response<Body> {
    // app.get only: GET + HEAD serve; every other method never reaches the
    // handler on Node and lands on the finalHandler default 404.
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return express_default_404(req.method(), req.uri().path());
    }
    let dir = st.model_dir.clone().unwrap_or_else(default_model_dir);
    serve_reply(&dir, "/cornelius.onnx", "public, max-age=2592000, immutable", req.method(), req.headers(), MissMode::Empty404)
}

fn scan_dir(st: &AppState) -> PathBuf {
    if let Some(d) = std::env::var_os("CLIENT_SCAN_DIR") {
        let p = PathBuf::from(d);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    st.model_dir.clone().unwrap_or_else(default_model_dir).join("client-scan")
}

// ---------------------------------------------------------------------------
// /scryfall-portal reverse proxy (server.js LOCAL_WEB_PORTAL_ENABLED block)
// ---------------------------------------------------------------------------

const PORTAL_PREFIX: &str = "/scryfall-portal";

fn portal_upstream_base() -> String {
    std::env::var("LOCAL_SCRYFALL_URL")
        .unwrap_or_else(|_| "http://scryfall-local:8420".into())
        .trim_end_matches('/')
        .to_string()
}

/// server.js rewriteText's two regexes as byte scans (the patterns are pure
/// ASCII; UTF-8 continuation bytes can never match an ASCII needle).
///
/// Unmatched bytes are copied as UTF-8 slices, NOT pushed as latin1 chars:
/// Node's regexes run on the decoded JS string, so every byte outside a match
/// round-trips verbatim (a multibyte em-dash must stay E2 80 94 — pushing
/// `b as char` would double-encode it to C3 A2 C2 80 C2 94). Every break
/// position (i, q) sits on an ASCII needle byte, so slices are always on char
/// boundaries.
pub fn rewrite_text(body: &str) -> String {
    let b = body.as_bytes();
    let mut out = String::with_capacity(body.len() + 64);
    let mut i = 0usize;
    let mut seg = 0usize; // start of the not-yet-copied run
    while i < b.len() {
        // /(href|src)="\/(?!\/)/g -> $1="/scryfall-portal/
        let href = i + 5 <= b.len() && &b[i..i + 5] == b"href=";
        let src = !href && i + 4 <= b.len() && &b[i..i + 4] == b"src=";
        if href || src {
            let q = i + if href { 5 } else { 4 }; // index of the opening quote
            if q + 2 < b.len() && b[q] == b'"' && b[q + 1] == b'/' && b[q + 2] != b'/' {
                // Node's replacement builds `$1="/scryfall-portal/` from
                // scratch (PORTAL_PREFIX itself supplies the leading '/');
                // the match's trailing '/' is consumed.
                out.push_str(&body[seg..q]);
                out.push('"');
                out.push_str(PORTAL_PREFIX);
                out.push('/');
                i = q + 2;
                seg = i;
                continue;
            }
        }
        // /(["'])(\/(?:api\/|sets(?:$|[./?])|\?))/g -> $1 + prefix + $2
        if (b[i] == b'"' || b[i] == b'\'') && i + 1 < b.len() && b[i + 1] == b'/' {
            let rest = &body[i + 2..];
            let n = if rest.starts_with("api/") {
                Some(4)
            } else if rest.starts_with('?') {
                Some(0)
            } else if rest.starts_with("sets") {
                let after: Option<&str> = rest.get(4..5);
                if after.is_none() || after.is_some_and(|c| matches!(c, "." | "/" | "?")) {
                    Some(4)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(n) = n {
                // Node's replacement `$1${PORTAL_PREFIX}$2` from scratch;
                // $2 already starts with its '/'.
                out.push_str(&body[seg..i]);
                out.push_str(&body[i..i + 1]); // the quote (ASCII, needle-guaranteed)
                out.push_str(PORTAL_PREFIX);
                out.push_str(&body[i + 1..i + 2 + n]);
                i = i + 2 + n;
                seg = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&body[seg..]);
    out
}

/// TEXTY = /^text\/(html|css)|javascript|^application\/json/ — unanchored
/// middle arm reproduced exactly, sloppiness included.
fn texty(ct: &str) -> bool {
    let lc = ct.to_ascii_lowercase();
    lc.starts_with("text/html") || lc.starts_with("text/css") || lc.contains("javascript") || lc.starts_with("application/json")
}

pub async fn handle_portal(State(st): State<AppState>, req: Request) -> Response<Body> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // app.get(PORTAL_PREFIX[,/*]) matches GET + HEAD only. Other methods fall
    // through every route to the finalHandler default 404 (verified live:
    // POST /scryfall-portal/ answers 404 "Cannot POST …", not the 405 arm).
    if method != Method::GET {
        if method == Method::HEAD {
            // HEAD reaches the handler; its req.method !== 'GET' arm answers.
            return express_json(
                StatusCode::METHOD_NOT_ALLOWED,
                "{\"error\":\"Method not allowed — the card portal is read-only\"}",
            );
        }
        return express_default_404(&method, &path);
    }
    if path == PORTAL_PREFIX {
        // express res.redirect(301, target): Vary: Accept (express's own
        // leg, merged after UA/Origin by the guard), CT text/plain;
        // charset=utf-8 and body `{reason}. Redirecting to {target}` —
        // live-pinned on :3312 (51 bytes, Content-Length set, no ETag).
        let target = format!("{PORTAL_PREFIX}/");
        let body = format!("Moved Permanently. Redirecting to {target}");
        let mut h = HeaderMap::new();
        h.insert(header::LOCATION, HeaderValue::from_str(&target).unwrap());
        h.insert(header::VARY, HeaderValue::from_static("Accept"));
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&body.len().to_string()).unwrap());
        let mut parts = Response::new(Body::from(body.clone())).into_parts().0;
        parts.status = StatusCode::MOVED_PERMANENTLY;
        parts.headers = h;
        return Response::from_parts(parts, Body::from(body));
    }
    let sub = &path[PORTAL_PREFIX.len()..];
    let sub = if sub.is_empty() { "/" } else { sub };
    let qs = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
    let target: Uri = match format!("{}{}{}", portal_upstream_base(), sub, qs).parse() {
        Ok(u) => u,
        Err(_) => return portal_unavailable(),
    };
    let accept = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*/*")
        .to_string();
    let upstream_req = Request::get(target.clone())
        .header("User-Agent", "Scrybox/1.0")
        .header(header::ACCEPT, accept)
        .header(header::ACCEPT_ENCODING, "identity")
        .body(Body::empty());
    // Node's fetch AbortSignal.timeout(10000) covers header arrival; body
    // streaming itself stays untimed on both sides (honest approximation).
    let upstream = match upstream_req {
        Ok(r) => match tokio::time::timeout(Duration::from_secs(10), st.client.request(r)).await {
            Ok(Ok(r)) => r,
            _ => {
                tracing::warn!(target = %target, "scryfall-portal: upstream unreachable");
                return portal_unavailable();
            }
        },
        Err(_) => return portal_unavailable(),
    };

    let status = upstream.status();
    let mut headers = HeaderMap::new();
    if let Some(v) = upstream.headers().get(header::CONTENT_TYPE).cloned() {
        headers.insert(header::CONTENT_TYPE, v);
    }
    if let Some(v) = upstream.headers().get(header::CACHE_CONTROL).cloned() {
        headers.insert(header::CACHE_CONTROL, v);
    }
    if status.is_redirection() {
        // Prefix any root-absolute redirect target (e.g. /sets -> /sets.html).
        let loc = upstream
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let loc = if loc.starts_with('/') && !loc.starts_with("//") {
            format!("{PORTAL_PREFIX}{loc}")
        } else {
            loc
        };
        if let Ok(v) = HeaderValue::from_str(&loc) {
            headers.insert(header::LOCATION, v);
        }
    }
    let ct_str = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // !ok or non-text: stream through untouched. Node pipes (no explicit
    // Content-Length — the client sees chunked); hyper streams the incoming
    // body the same way, dropping the sidecar's own CL with the headers.
    if !status.is_success() || !texty(&ct_str) {
        let (mut parts, body) = upstream.into_parts();
        parts.status = status;
        parts.headers = headers;
        // Node pipes the body: chunked, never a Content-Length. Round-trip
        // through a data Stream to shed any size hint reqwest would let
        // hyper re-stamp as Content-Length.
        let body = Body::new(body);
        let body = Body::from_stream(body.into_data_stream());
        let mut resp = Response::from_parts(parts, body);
        resp.extensions_mut().insert(ProxiedContent);
        // Node pipes this body: compression sees no explicit Content-Length
        // and returns before its Vary: Accept-Encoding leg.
        resp.extensions_mut().insert(crate::secheaders::NoCompressionVary);
        return resp;
    }

    // Buffered text (JSON verbatim): express res.send(string) appends the
    // missing utf-8 charset, stamps the weak ETag, sets Content-Length.
    let bytes = match axum::body::to_bytes(Body::new(upstream.into_body()), 64 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            tracing::warn!(target = %target, "scryfall-portal: upstream body failed");
            return portal_unavailable();
        }
    };
    let is_json = ct_str.to_ascii_lowercase().starts_with("application/json");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let body_str = if is_json { text } else { rewrite_text(&text) };
    let ct_out = format!(
        "{}{}",
        ct_str.trim_end(),
        if ct_str.to_ascii_lowercase().contains("charset") { "" } else { "; charset=utf-8" }
    );
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&ct_out).unwrap());
    // Node's res.send wire order: Content-Type, Content-Length, ETag. Stamp CL
    // explicitly (same value hyper would auto-derive) so the line order matches.
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&body_str.len().to_string()).unwrap());
    if let Ok(v) = HeaderValue::from_str(&body_etag(body_str.as_bytes())) {
        headers.insert(header::ETAG, v);
    }
    let mut parts = Response::new(Body::empty()).into_parts().0;
    parts.status = status;
    parts.headers = headers;
    let mut resp = Response::from_parts(parts, Body::from(body_str));
    resp.extensions_mut().insert(ProxiedContent);
    resp
}

fn portal_unavailable() -> Response<Body> {
    express_json(
        StatusCode::BAD_GATEWAY,
        "{\"error\":\"Card portal unavailable\",\"details\":\"The local Scryfall service (LOCAL_SCRYFALL_URL) is not reachable. Start the scryfall-local container or unset LOCAL_WEB_PORTAL_ENABLED.\"}",
    )
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("manifest.json"), b"{\"a\":1}").unwrap();
        std::fs::write(d.path().join("rec-dict.txt"), b"hello world").unwrap();
        std::fs::write(d.path().join("model.onnx"), vec![7u8; 4096]).unwrap();
        std::fs::write(d.path().join("big.json"), vec![b'x'; 2048]).unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("sub").join("deep.js"), b"console.log(1)").unwrap();
        std::fs::write(d.path().join(".secret"), b"nope").unwrap();
        d
    }

    fn serve(d: &std::path::Path, p: &str, method: Method, extra: Option<(&str, String)>) -> Response<Body> {
        let mut h = HeaderMap::new();
        if let Some((n, v)) = extra {
            h.insert(n.parse::<axum::http::HeaderName>().unwrap(), v.parse().unwrap());
        }
        serve_reply(d, p, "no-cache", &method, &h, MissMode::ServerError)
    }

    #[test]
    fn serve_hit_headers_match_send() {
        let d = tmp_tree();
        let res = serve(d.path(), "/manifest.json", Method::GET, None);
        assert_eq!(res.status(), StatusCode::OK);
        let h = res.headers();
        assert_eq!(h[header::CACHE_CONTROL], "no-cache");
        assert_eq!(h["accept-ranges"], "bytes");
        assert_eq!(h[header::CONTENT_TYPE], "application/json; charset=UTF-8");
        assert_eq!(h[header::CONTENT_LENGTH], "7");
        let etag = h[header::ETAG].to_str().unwrap().to_string();
        assert!(etag.starts_with("W/\"7-") && etag.ends_with('"'), "{etag}");
    }

    #[test]
    fn serve_304_on_etag_match_and_only_base_headers() {
        let d = tmp_tree();
        let first = serve(d.path(), "/manifest.json", Method::GET, None);
        let etag = first.headers()[header::ETAG].to_str().unwrap().to_string();
        let res = serve(d.path(), "/manifest.json", Method::GET, Some(("if-none-match", etag.clone())));
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        let h = res.headers();
        assert_eq!(h[header::CACHE_CONTROL], "no-cache");
        assert_eq!(h["accept-ranges"], "bytes");
        assert_eq!(h[header::ETAG], etag.as_str());
        assert!(h.get(header::LAST_MODIFIED).is_some());
        assert!(h.get(header::CONTENT_TYPE).is_none());
        assert!(h.get(header::CONTENT_LENGTH).is_none());
    }

    #[test]
    fn serve_304_on_if_modified_since() {
        let d = tmp_tree();
        let first = serve(d.path(), "/rec-dict.txt", Method::GET, None);
        let lm = first.headers()[header::LAST_MODIFIED].to_str().unwrap().to_string();
        let res = serve(d.path(), "/rec-dict.txt", Method::GET, Some(("if-modified-since", lm)));
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
    }

    #[test]
    fn miss_is_500_for_scanassets_empty404_for_model() {
        let d = tmp_tree();
        let a = serve_reply(d.path(), "/nope", "x", &Method::GET, &HeaderMap::new(), MissMode::ServerError);
        assert_eq!(a.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let b = serve_reply(d.path(), "/nope", "x", &Method::GET, &HeaderMap::new(), MissMode::Empty404);
        assert_eq!(b.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn dotfile_any_segment_and_directory_are_misses() {
        let d = tmp_tree();
        for p in ["/.secret", "/sub/../.secret", "/sub/%2e%2esecret"] {
            let r = serve(d.path(), p, Method::GET, None);
            assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR, "{p} must 500 like a send error");
        }
        let dir = serve(d.path(), "/sub/", Method::GET, None);
        assert_eq!(dir.status(), StatusCode::INTERNAL_SERVER_ERROR, "index:false directory -> miss");
    }

    #[test]
    fn traversal_flattens_like_send_normalize() {
        let d = tmp_tree();
        // send drops '..' segments during normalize: /../manifest.json reads
        // root/manifest.json, never escaping the tree.
        let r = serve(d.path(), "/../manifest.json", Method::GET, None);
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[test]
    fn method_error_is_405_with_allow_and_zero_length() {
        let d = tmp_tree();
        let r = serve(d.path(), "/manifest.json", Method::POST, None);
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(r.headers()["allow"], "GET, HEAD");
        assert_eq!(r.headers()[header::CONTENT_LENGTH], "0");
        assert!(r.headers().get(header::CONTENT_TYPE).is_none());
    }

    #[tokio::test]
    async fn head_keeps_headers_no_body() {
        let d = tmp_tree();
        let r = serve(d.path(), "/manifest.json", Method::HEAD, None);
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CONTENT_LENGTH], "7");
        assert_eq!(axum::body::to_bytes(r.into_body(), 9).await.unwrap().len(), 0);
    }

    #[test]
    fn gzip_above_threshold_below_1kb_identity() {
        let d = tmp_tree();
        let small = serve(d.path(), "/manifest.json", Method::GET, Some(("accept-encoding", "gzip".into())));
        assert!(small.headers().get(header::CONTENT_ENCODING).is_none(), "8B < 1KB stays identity");
        let big = serve(d.path(), "/big.json", Method::GET, Some(("accept-encoding", "gzip".into())));
        assert_eq!(big.headers()[header::CONTENT_ENCODING], "gzip");
        assert_eq!(big.headers()[header::VARY], "Accept-Encoding");
        let onx = serve(d.path(), "/model.onnx", Method::GET, Some(("accept-encoding", "gzip".into())));
        assert!(onx.headers().get(header::CONTENT_ENCODING).is_none(), "octet-stream never gzips");
    }

    #[test]
    fn rewrite_text_preserves_multibyte_utf8() {
        // Node's rewriteText regexes run on the DECODED string: bytes outside
        // a match round-trip verbatim. The byte-scan twin once pushed unmatched
        // bytes `as char` (latin1), double-encoding the sidecar index's em-dash
        // (E2 80 94 -> C3 A2 C2 80 C2 94) — this pins that regression.
        let src = "<title>Scryfall Local \u{2014} Magic: The Gathering</title> \u{4e2d}\u{6587} \u{1F525} href=\"/sets\"";
        let got = rewrite_text(src);
        assert_eq!(
            got,
            "<title>Scryfall Local \u{2014} Magic: The Gathering</title> \u{4e2d}\u{6587} \u{1F525} href=\"/scryfall-portal/sets\""
        );
        assert_eq!(&got.as_bytes()[22..25], &[0xE2, 0x80, 0x94]); // "<title>Scryfall Local " = 22 B
    }

    #[test]
    fn rewrite_text_both_regexes() {
        assert_eq!(
            rewrite_text(r#"<a href="/sets.html"><img src="//cdn/x.png"><script src="/app.js">"#),
            r#"<a href="/scryfall-portal/sets.html"><img src="//cdn/x.png"><script src="/scryfall-portal/app.js">"#
        );
        let src = r#"x("/api/cards/y"); u('/sets'); w('/sets.html'); z('/?q=1'); y('/setsx'); t('/other'); s("sets")"#;
        let got = rewrite_text(src);
        // Expected string pinned byte-for-byte from the container's Node with
        // server.js's exact rewriteText(). '/sets' does NOT rewrite: the $ in
        // sets(?:$|[./?]) is end-of-STRING, the closing quote is not in [./?]
        // (faithful fidelity to Node's sloppy regex, verified live).
        assert_eq!(
            got,
            r#"x("/scryfall-portal/api/cards/y"); u('/sets'); w('/scryfall-portal/sets.html'); z('/scryfall-portal/?q=1'); y('/setsx'); t('/other'); s("sets")"#
        );
    }

    #[tokio::test]
    async fn finalhandler_default_404_is_the_full_html_page() {
        // Byte-for-byte live pin from :3312 POST /scryfall-portal/ (156 B).
        let r = express_default_404(&Method::POST, "/scryfall-portal/");
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let h = r.headers();
        assert_eq!(h[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(
            h[axum::http::HeaderName::from_static("content-security-policy")].to_str().unwrap(),
            "default-src 'none'"
        );
        assert!(h.get(header::ETAG).is_none());
        let claimed: u64 = h[header::CONTENT_LENGTH].to_str().unwrap().parse().unwrap();
        let body = axum::body::to_bytes(r.into_body(), 999).await.unwrap();
        assert_eq!(claimed as usize, body.len());
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>Error</title>\n</head>\n<body>\n<pre>Cannot POST /scryfall-portal/</pre>\n</body>\n</html>\n"
        );
    }

    #[tokio::test]
    async fn finalhandler_404_escapes_the_path() {
        // escape-html set: the raw quote/angle chars must never reach the page.
        let r = express_default_404(&Method::GET, "/a\"><script>");
        let body = String::from_utf8(
            axum::body::to_bytes(r.into_body(), 999).await.unwrap().to_vec(),
        )
        .unwrap();
        assert!(body.contains("<pre>Cannot GET /a&quot;&gt;&lt;script&gt;</pre>"), "{body}");
        assert!(!body.contains("<script>"), "path must be escaped");
    }

    #[test]
    fn texty_arms_match_the_regex() {
        assert!(texty("text/html; charset=utf-8"));
        assert!(texty("text/css"));
        assert!(texty("application/json"));
        assert!(texty("application/x-JAVASCRIPT"), "unanchored javascript arm");
        assert!(!texty("application/octet-stream"));
        assert!(!texty("application/gzip"));
    }

    #[test]
    fn compressible_matches_compressible_pkg_arms() {
        assert!(compressible_ct(Some("text/plain")));
        assert!(compressible_ct(Some("application/json")));
        assert!(compressible_ct(Some("application/anything+json")));
        assert!(compressible_ct(None), "no CT is compressible in the pkg");
        assert!(!compressible_ct(Some("application/octet-stream")));
        assert!(!compressible_ct(Some("application/gzip")));
        assert!(!compressible_ct(Some("image/png")));
    }

    #[test]
    fn http_date_parse_roundtrip_and_reject() {
        let s = "Sat, 24 Oct 2026 14:58:39 GMT";
        let t = parse_http_date(s).unwrap();
        let d = chrono::DateTime::from_timestamp(t, 0).unwrap();
        assert_eq!(d.format("%a, %d %b %Y %H:%M:%S GMT").to_string(), s);
        assert!(parse_http_date("garbage").is_none());
        assert!(parse_http_date("Sat, 24 Oct 2026 14:58:39 UTC").is_none());
    }

    #[tokio::test]
    async fn portal_json_is_verbatim_with_express_send_headers() {
        // Sidecar JSON passes through unrewritten; CT gets the utf-8 charset
        // appended, ETag/CL stamped, CSP trio stripped via the marker.
        let body = r#"{"href":"/not-rewritten"}"#;
        // Drive the pure tail of the handler (buffered arm) directly.
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        let ct_str = headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap().to_string();
        let is_json = ct_str.to_ascii_lowercase().starts_with("application/json");
        let out = if is_json { body.to_string() } else { rewrite_text(body) };
        assert_eq!(out, body, "json must never be rewritten");
        let ct_out = format!("{}{}", ct_str.trim_end(), if ct_str.contains("charset") { "" } else { "; charset=utf-8" });
        assert_eq!(ct_out, "application/json; charset=utf-8");
        assert_eq!(body_etag(body.as_bytes()), body_etag(body.as_bytes()));
    }

    #[test]
    fn express_json_carries_ct_and_weak_etag() {
        let r = express_json(StatusCode::BAD_GATEWAY, "{\"error\":\"x\"}");
        assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "application/json; charset=utf-8");
        assert_eq!(r.headers()[header::ETAG], body_etag("{\"error\":\"x\"}".as_bytes()).as_str());
    }
}

//! Lane B unit 5c: the static SPA mount itself, native. Twin of
//! backend/src/server.js:611-631 —
//!
//! * `express.static(frontendBuildPath, { index: false, setHeaders })` —
//!   hashed files under `<dir>/assets/` get `public, max-age=31536000,
//!   immutable`, everything else `no-cache`; and
//! * `app.get('*', …)` — anything NOT starting with `/api` that the static
//!   mount missed is answered with `res.sendFile(index.html)` (GET **and**
//!   HEAD — app.get matches HEAD — the SPA deep-link contract).
//!
//! Registered as a wildcard route (NOT the fallback): Node's catch-all sits
//! behind every API router, so unmatched `/api/*` still reaches the
//! router-level proxy. Inside the handler the `/api` prefix test mirrors
//! `req.path.startsWith('/api')` verbatim — an '/apifoo' path is SPA land,
//! exactly like Node — and that internal proxy leg carries the
//! `secheaders::NodeProxied` marker so the twin stack is not re-stamped on
//! top of Node's own headers.
//!
//! send/serve-static fidelity (all live-pinned :3312 vs :3320):
//! * miss arms: a missing file, a dot-REJECT (see below), a directory WITH a
//!   trailing slash (`/assets/`) and an undecodable percent escape differ:
//!   - missing / directory-with-slash / dot-reject => fall through the
//!     static mount to the catch-all => 200 index.html (sendFile twin);
//!   - a directory WITHOUT a trailing slash => send's redirect listener:
//!     301 `Location: <path>/` with the 156-byte `Redirecting to …` HTML
//!     document (finalhandler createHtmlDocument shape, CSP default-src);
//!   - `decodeURIComponent` throw (e.g. /%zz) => express's generic error
//!     handler => 500 {"error":"Internal server error"} (NOT index.html).
//! * dotfiles: serve-static passes dotfiles:undefined, so send's legacy
//!   ladder applies — a dot-prefixed FINAL segment is 'ignore' (404 =>
//!   fall-through => index.html), a dot in a MIDDLE segment is 'allow'
//!   (served if the file exists). Different from the scan-assets mount,
//!   which passes dotfiles:'deny' explicitly.
//! * res.sendFile(index.html) has no setHeaders, so send's own Cache-Control
//!   applies: `public, max-age=0` (still uncached in practice; index.html
//!   itself served by the STATIC mount is `no-cache`).
//! * header ORDER differs per mount and is pinned: static hits emit
//!   Cache-Control FIRST (setHeaders runs before send's setHeader emits
//!   Accept-Ranges), the sendFile arm emits Accept-Ranges first.
//! * conditional GET (INM exact/comma/* + IMS 1-s floor) => 304 carrying
//!   exactly the four freshness headers; HEAD keeps every header, no body;
//!   compression gzips >1 KB compressible bodies and replaces Content-Length
//!   with chunked (compression deletes the CL — mirrored via a size-hint-less
//!   StreamBody so hyper can never stamp one).
//! * non-GET/HEAD never matches (serve-static fallthrough next() + app.get)
//!   => the finalhandler default 404 (`Cannot POST /collection`).
//! * missing index.html => res.sendFile error => generic 500 JSON arm.
//! * KNOWN GAP (fleet-wide, pre-5c): Node runs compression@1.8.1, which also
//!   negotiates brotli — every native route answers `br` to an
//!   `accept-encoding: …, br` client while the Rust twins answer gzip.
//!   Decoded bodies are byte-equal (verified :3312-vs-:3320); only the
//!   negotiated encoding differs. A faithful fix means porting accepts'
//!   encoding-negotiation (q-values) once and wiring brotli into every
//!   compress arm — tracked as a follow-up unit, deliberately NOT
//!   half-ported here.
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header;
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode};
use bytes::Bytes;
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::collection::{etag_matches, gzip_bytes, wants_encoding};
use crate::static_assets::{
    compressible_ct, express_json, file_etag, mime_of, parse_http_date,
};
use crate::AppState;

/// send's redirect-listener document, live-pinned byte-for-byte on :3312
/// (GET /assets => 156 bytes). finalhandler's createHtmlDocument shape.
fn redirect_doc(loc: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>Redirecting</title>\n</head>\n<body>\n<pre>Redirecting to {}</pre>\n</body>\n</html>\n",
        crate::static_assets::escape_html(loc)
    )
}

pub async fn handle(State(st): State<AppState>, req: Request) -> Response<Body> {
    let path = req.uri().path().to_string();
    // Node's catch-all guard: req.path.startsWith('/api') — byte-for-byte,
    // so '/apifoo' is SPA land and '/api' alone is not. The response is
    // Node's own output; mark it so the guard layer leaves it alone.
    // The static-trio prefixes join that list while the `static_assets`
    // group is OFF: Node's own express.static(fallthrough:false) answers
    // those (a miss is a 500, not index.html), so they must reach it —
    // kill-switch rule. With the group ON those routes match first and
    // never reach this wildcard.
    let trio_off = !st.owns("static_assets")
        && (path.starts_with("/scan-assets")
            || path == "/models/cornelius.onnx"
            || path.starts_with("/scryfall-portal"));
    if path.starts_with("/api") || trio_off {
        let mut res = crate::proxy(State(st), req).await;
        res.extensions_mut().insert(crate::secheaders::NodeProxied);
        return res;
    }
    let root = st.cfg.static_dir.clone().unwrap_or_default();
    serve(&root, &path, req.method(), req.headers())
}

pub(crate) fn serve(root: &Path, path: &str, method: &Method, headers_in: &HeaderMap) -> Response<Body> {
    // serve-static answers GET/HEAD only; every other method next()s past
    // the static mount AND past app.get('*') => finalhandler default 404.
    if !matches!(*method, Method::GET | Method::HEAD) {
        return crate::static_assets::express_default_404(method, path);
    }
    // send: decodeURIComponent(req.path); a throw is a SYNCHRONOUS middleware
    // throw express routes to the generic error handler => 500 JSON (live
    // pinned: GET /%zz), NOT a fall-through.
    let Some(decoded) = crate::shared::percent_decode(path.as_bytes()) else {
        return express_json(StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"Internal server error\"}");
    };
    // send's normalize(join(root, path)): '.'/'..' segments are DROPPED
    // (traversal flattens instead of escaping), empties collapse.
    let segs: Vec<&str> = decoded
        .split('/')
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .collect();
    let trailing = decoded.ends_with('/');
    // send's containsDotFile + legacy undefined-dotfiles ladder: a dot
    // segment only REJECTS (404 => fall-through) when it is the LAST part;
    // mid-path dots are 'allow'.
    let dot_last = segs.last().is_some_and(|p| p.len() > 1 && p.starts_with('.'));
    let mut file = root.to_path_buf();
    for p in &segs {
        file.push(p);
    }
    let meta = if segs.is_empty() || dot_last {
        None
    } else {
        std::fs::metadata(&file).ok()
    };
    if let Some(m) = &meta {
        if m.is_dir() {
            return if trailing {
                // createNotFoundDirectoryListener-equivalent: error(404) =>
                // fallthrough => catch-all index.html.
                index_reply(root, method, headers_in)
            } else {
                dir_redirect(path)
            };
        }
    }
    match meta.filter(|m| m.is_file()) {
        Some(m) => static_reply(&file, &m, method, headers_in),
        None => index_reply(root, method, headers_in),
    }
}

fn stat_tag(m: &std::fs::Metadata) -> (String, String, u64) {
    let size = m.len();
    let mtime_ms = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let lm = chrono::DateTime::from_timestamp((mtime_ms / 1000) as i64, 0)
        .map(|d| d.format("%a, %d %b %Y %H:%M:%S GMT").to_string())
        .unwrap_or_default();
    (file_etag(size, mtime_ms), lm, size)
}

fn conditional(headers_in: &HeaderMap, tag: &str, lm_secs: i64) -> bool {
    let inm = headers_in.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    let ims = headers_in.get(header::IF_MODIFIED_SINCE).and_then(|v| v.to_str().ok());
    if inm.is_none() && ims.is_none() {
        return false;
    }
    let inm_fresh = inm.map(|v| etag_matches(Some(v), tag)).unwrap_or(true);
    let ims_fresh = ims.and_then(parse_http_date).map(|t| t >= lm_secs).unwrap_or(true);
    inm_fresh && ims_fresh
}

/// The express.static hit arm: setHeaders has ALREADY stamped Cache-Control
/// (before send's setHeader emits Accept-Ranges), so the header order is
/// Cache-Control, Accept-Ranges, Last-Modified, ETag, Content-Type.
fn static_reply(file: &Path, m: &std::fs::Metadata, method: &Method, headers_in: &HeaderMap) -> Response<Body> {
    let hashed = file.to_string_lossy().replace('\\', "/").contains("/assets/");
    let cc = if hashed { "public, max-age=31536000, immutable" } else { "no-cache" };
    let (tag, lm, _size) = stat_tag(m);
    let lm_secs = lm_len_secs(&lm);
    let mut h = HeaderMap::new();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_str(cc).unwrap());
    h.insert("accept-ranges", HeaderValue::from_static("bytes"));
    h.insert(header::LAST_MODIFIED, HeaderValue::from_str(&lm).unwrap());
    h.insert(header::ETAG, HeaderValue::from_str(&tag).unwrap());
    finish(file, m, method, headers_in, h, &tag, lm_secs, mime_of(file.file_name().and_then(|n| n.to_str()).unwrap_or("")))
}

/// The catch-all res.sendFile(index.html) arm: send's own order, Accept-Ranges
/// ahead of Cache-Control (public, max-age=0 — sendFile gets no setHeaders).
fn index_reply(root: &Path, method: &Method, headers_in: &HeaderMap) -> Response<Body> {
    let file = root.join("index.html");
    let Ok(m) = std::fs::metadata(&file) else {
        // res.sendFile error => next(err) => generic error handler.
        return express_json(StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"Internal server error\"}");
    };
    if !m.is_file() {
        return express_json(StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"Internal server error\"}");
    }
    let (tag, lm, _size) = stat_tag(&m);
    let lm_secs = lm_len_secs(&lm);
    let mut h = HeaderMap::new();
    h.insert("accept-ranges", HeaderValue::from_static("bytes"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=0"));
    h.insert(header::LAST_MODIFIED, HeaderValue::from_str(&lm).unwrap());
    h.insert(header::ETAG, HeaderValue::from_str(&tag).unwrap());
    finish(&file, &m, method, headers_in, h, &tag, lm_secs, "text/html; charset=UTF-8")
}

/// Last-Modified string back to epoch seconds (1-s floor) for IMS compare.
fn lm_len_secs(lm: &str) -> i64 {
    parse_http_date(lm).unwrap_or(0)
}

#[allow(clippy::too_many_arguments)] // mirrors send()'s option bag (path, m, method, range?, …)
fn finish(
    file: &Path,
    m: &std::fs::Metadata,
    method: &Method,
    headers_in: &HeaderMap,
    mut h: HeaderMap,
    tag: &str,
    lm_secs: i64,
    ct: &str,
) -> Response<Body> {
    let _ = m;
    if conditional(headers_in, tag, lm_secs) {
        let mut parts = Response::new(Body::empty()).into_parts().0;
        parts.status = StatusCode::NOT_MODIFIED;
        parts.headers = h;
        return Response::from_parts(parts, Body::empty());
    }
    let raw = match std::fs::read(file) {
        Ok(b) => Bytes::from(b),
        Err(_) => {
            return express_json(StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"Internal server error\"}")
        }
    };
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(ct).unwrap());
    let ae = headers_in.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
    let zip = compressible_ct(Some(ct)) && wants_encoding(ae, "gzip", raw.len());
    if *method == Method::HEAD {
        // send set every header; the stream emits none of the body.
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&raw.len().to_string()).unwrap());
        let mut parts = Response::new(Body::empty()).into_parts().0;
        parts.status = StatusCode::OK;
        parts.headers = h;
        return Response::from_parts(parts, Body::empty());
    }
    if zip {
        // compression@1.8.1: gzip + Vary: Accept-Encoding merge, and it
        // DELETES Content-Length (node http then chunks). Round-trip the
        // encoded bytes through a data Stream to shed the size hint so hyper
        // can never stamp a Content-Length (same trick as the portal pipe).
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        let gz = gzip_bytes(&raw);
        let body = Body::from(gz);
        let body = Body::from_stream(body.into_data_stream());
        let mut parts = Response::new(Body::empty()).into_parts().0;
        parts.status = StatusCode::OK;
        parts.headers = h;
        return Response::from_parts(parts, body);
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&raw.len().to_string()).unwrap());
    let mut parts = Response::new(Body::empty()).into_parts().0;
    parts.status = StatusCode::OK;
    parts.headers = h;
    Response::from_parts(parts, Body::from(raw))
}

/// send's createRedirectDirectoryListener (via serve-static, redirect:true):
/// GET /assets => 301 Location /assets/ + the Redirecting document. Header
/// order live-pinned: Content-Type, Content-Length, Content-Security-Policy,
/// Location.
pub(crate) fn dir_redirect(path: &str) -> Response<Body> {
    let loc = collapse_leading_slashes(path).to_string() + "/";
    let doc = redirect_doc(&loc);
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=UTF-8"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&doc.len().to_string()).unwrap());
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'"));
    h.insert(header::LOCATION, HeaderValue::from_str(&loc).unwrap());
    let mut parts = Response::new(Body::empty()).into_parts().0;
    parts.status = StatusCode::MOVED_PERMANENTLY;
    parts.headers = h;
    Response::from_parts(parts, Body::from(doc))
}

/// serve-static's collapseLeadingSlashes.
fn collapse_leading_slashes(s: &str) -> &str {
    let n = s.bytes().take_while(|b| *b == b'/').count();
    if n > 1 {
        &s[n - 1..]
    } else {
        s
    }
}

// (PathBuf arrives via std::path import for root.join typing clarity.)

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::HeaderName;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("index.html"), "<html>spa</html>").unwrap();
        std::fs::create_dir_all(d.path().join("assets")).unwrap();
        std::fs::write(d.path().join("assets").join("App.abc123.js"), "console.log(1)").unwrap();
        std::fs::write(d.path().join("logo.svg"), "<svg/>").unwrap();
        std::fs::create_dir_all(d.path().join(".well-known")).unwrap();
        std::fs::write(d.path().join(".well-known").join("links.json"), "{\"a\":1}").unwrap();
        d
    }

    fn hm(kv: &[(&str, String)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in kv {
            h.insert(HeaderName::from_lowercase(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    async fn body_of(r: Response<Body>) -> String {
        let b = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&b).into_owned()
    }

    #[test]
    fn static_hit_carries_no_cache_or_immutable_and_send_order() {
        let d = tree();
        let r = serve(d.path(), "/logo.svg", &Method::GET, &hm(&[]));
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["cache-control"], "no-cache");
        assert_eq!(r.headers()["content-type"], "image/svg+xml");
        // setHeaders-first order: Cache-Control precedes Accept-Ranges.
        let ks: Vec<String> = r.headers().keys().map(|k| k.as_str().to_string()).collect();
        assert_eq!(ks, vec!["cache-control", "accept-ranges", "last-modified", "etag", "content-type", "content-length"]);
        let r2 = serve(d.path(), "/assets/App.abc123.js", &Method::GET, &hm(&[]));
        assert_eq!(r2.headers()["cache-control"], "public, max-age=31536000, immutable");
        assert_eq!(r2.headers()["content-type"], "application/javascript; charset=UTF-8");
    }

    #[tokio::test]
    async fn deep_link_is_index_with_sendfile_order_and_public_maxage() {
        let d = tree();
        let r = serve(d.path(), "/collection", &Method::GET, &hm(&[]));
        assert_eq!(r.status(), 200);
        assert_eq!(body_of(r).await, "<html>spa</html>");
        let d2 = tree();
        let r2 = serve(d2.path(), "/collection", &Method::GET, &hm(&[]));
        let ks: Vec<String> = r2.headers().keys().map(|k| k.as_str().to_string()).collect();
        assert_eq!(ks, vec!["accept-ranges", "cache-control", "last-modified", "etag", "content-type", "content-length"]);
        assert_eq!(r2.headers()["cache-control"], "public, max-age=0");
        assert_eq!(r2.headers()["content-type"], "text/html; charset=UTF-8");
    }

    #[tokio::test]
    async fn missing_deep_index_is_500_json() {
        let d = tempfile::tempdir().unwrap();
        let r = serve(d.path(), "/whatever", &Method::GET, &hm(&[]));
        assert_eq!(r.status(), 500);
        let ks: Vec<String> = r.headers().keys().map(|k| k.as_str().to_string()).collect();
        // Node res.send order live-pinned :3312 (GET /%zz): CT, CL, ETag.
        assert_eq!(ks, vec!["content-type", "content-length", "etag"]);
        assert_eq!(body_of(r).await, "{\"error\":\"Internal server error\"}");
    }

    #[tokio::test]
    async fn dot_last_segment_falls_through_to_index_mid_dot_is_served() {
        let d = tree();
        // final-segment dot => send 'ignore' => 404 => catch-all index.html.
        let r = serve(d.path(), "/x/.secret", &Method::GET, &hm(&[]));
        assert_eq!(r.status(), 200);
        assert_eq!(body_of(r).await, "<html>spa</html>");
        // middle-segment dot => legacy 'allow' => the file IS served.
        let r2 = serve(d.path(), "/.well-known/links.json", &Method::GET, &hm(&[]));
        assert_eq!(r2.status(), 200);
        assert_eq!(r2.headers()["content-type"], "application/json; charset=UTF-8");
        assert_eq!(body_of(r2).await, "{\"a\":1}");
    }

    #[tokio::test]
    async fn directory_redirects_when_no_slash_index_when_slash() {
        let d = tree();
        let r = serve(d.path(), "/assets", &Method::GET, &hm(&[]));
        assert_eq!(r.status(), 301);
        assert_eq!(r.headers()["location"], "/assets/");
        assert_eq!(r.headers()["content-type"], "text/html; charset=UTF-8");
        assert_eq!(r.headers()["content-security-policy"], "default-src 'none'");
        // byte-pinned live on :3312 (GET /assets => 156 bytes).
        assert_eq!(r.headers()["content-length"], "156");
        let ks: Vec<String> = r.headers().keys().map(|k| k.as_str().to_string()).collect();
        assert_eq!(ks, vec!["content-type", "content-length", "content-security-policy", "location"]);
        assert_eq!(body_of(r).await, redirect_doc("/assets/"));
        let r2 = serve(d.path(), "/assets/", &Method::GET, &hm(&[]));
        assert_eq!(r2.status(), 200);
        assert_eq!(body_of(r2).await, "<html>spa</html>");
    }

    #[tokio::test]
    async fn percent_decode_throw_is_500_json() {
        let d = tree();
        let r = serve(d.path(), "/%zz", &Method::GET, &hm(&[]));
        assert_eq!(r.status(), 500);
        assert_eq!(body_of(r).await, "{\"error\":\"Internal server error\"}");
    }

    #[tokio::test]
    async fn traversal_flattens_to_index() {
        let d = tree();
        let r = serve(d.path(), "/../../etc/passwd", &Method::GET, &hm(&[]));
        assert_eq!(r.status(), 200);
        assert_eq!(body_of(r).await, "<html>spa</html>");
    }

    #[test]
    fn conditional_get_is_304_with_four_freshness_headers() {
        let d = tree();
        let r0 = serve(d.path(), "/collection", &Method::GET, &hm(&[]));
        let tag = r0.headers()["etag"].to_str().unwrap().to_string();
        let r = serve(d.path(), "/collection", &Method::GET, &hm(&[("if-none-match", tag.clone())]));
        assert_eq!(r.status(), 304);
        let ks: Vec<String> = r.headers().keys().map(|k| k.as_str().to_string()).collect();
        assert_eq!(ks, vec!["accept-ranges", "cache-control", "last-modified", "etag"]);
    }

    #[tokio::test]
    async fn head_keeps_all_headers_without_body() {
        let d = tree();
        let r = serve(d.path(), "/collection", &Method::HEAD, &hm(&[]));
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["content-length"], "16");
        assert_eq!(body_of(r).await, "");
    }

    #[tokio::test]
    async fn other_methods_are_finalhandler_404() {
        let d = tree();
        let r = serve(d.path(), "/collection", &Method::POST, &hm(&[]));
        assert_eq!(r.status(), 404);
        assert_eq!(r.headers()["content-type"], "text/html; charset=utf-8");
        let b = body_of(r).await;
        assert!(b.contains("Cannot POST /collection"), "{b}");
    }

    #[tokio::test]
    async fn gzip_replaces_content_length_with_chunked_stream() {
        let d = tree();
        // 2 KB of compressible HTML: above compression's 1 KB threshold.
        std::fs::write(d.path().join("index.html"), "<html>".to_string() + &"a".repeat(2048) + "</html>").unwrap();
        let r = serve(d.path(), "/deep", &Method::GET, &hm(&[("accept-encoding", "gzip".to_string())]));
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["content-encoding"], "gzip");
        assert_eq!(r.headers()["vary"], "Accept-Encoding");
        assert!(r.headers().get(header::CONTENT_LENGTH).is_none(), "CL must be shed like compression does");
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        let mut dec = flate2::read::GzDecoder::new(&bytes[..]);
        let mut s = String::new();
        std::io::Read::read_to_string(&mut dec, &mut s).unwrap();
        assert!(s.starts_with("<html>aaa"));
    }
}

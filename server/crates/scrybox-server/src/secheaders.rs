//! Lane B unit 5a: byte-parity twin of the Node security-header stack from
//! backend/src/server.js — the COEP middleware (server.js:113-117 +
//! src/coep.js), helmet 8 with the exact directives from server.js:119-152,
//! and the cors@2.8.5 origin ladder + preflight from server.js:154-204 plus
//! the "Not allowed by CORS" arm of the error handler (server.js:634-637).
//!
//! Scope: responses the Rust front ANSWERS itself (native handlers, their
//! internal forwards, and this module's own CORS rejections/preflights).
//! Proxied responses pass Node's own headers through untouched — Node ran
//! the identical stack for them. Mounted in lib.rs as a layer over the
//! native-route Router BEFORE the fallback is attached, so the proxy path
//! never sees it.
//!
//! Node-order fidelity: every twin header (COEP, Vary, CSP-RO, COOP, CORP,
//! Origin-Agent-Cluster, Referrer-Policy, HSTS, X-Content-Type-Options,
//! X-DNS-Prefetch-Control, X-Download-Options, X-Frame-Options,
//! X-Permitted-Cross-Domain-Policies, X-XSS-Protection) is (re)emitted in
//! the exact order Node emits them, ahead of content-type/etag, so
//! `curl -D` output matches field-for-field and in order, not just as a set.

use axum::{
    body::Body,
    extract::Request,
    http::response::Parts,
    http::{header, HeaderMap, HeaderName, HeaderValue, Response, StatusCode},
    middleware::Next,
};

use crate::collection::body_etag;

/// helmet 8 output for the server.js CSP config, captured verbatim from the
/// Node instance (Content-Security-Policy-Report-Only value). helmet-csp
/// joins directives with ';' (no space) and merges the user directives with
/// its own defaults (base-uri/form-action/frame-ancestors/script-src-attr);
/// upgradeInsecureRequests: null drops that directive entirely.
pub const CSP_REPORT_ONLY: &str = "default-src 'self';script-src 'self' 'wasm-unsafe-eval';connect-src 'self';img-src 'self' data: blob: https://cards.scryfall.io https://c1.scryfall.com https://img.scryfall.com;style-src 'self' 'unsafe-inline' https://fonts.googleapis.com;font-src 'self' data: https://fonts.gstatic.com;object-src 'none';base-uri 'self';form-action 'self';frame-ancestors 'self';script-src-attr 'none'";

/// The twin of `selfSignedTls()` in backend/src/utils/tls.js:103-105.
/// With the self-signed HTTPS listener active (and no operator-provided
/// cert / trusted proxy), Node turns HSTS off because a self-signed warning
/// plus HSTS would lock the user out of the HTTP port.
pub fn self_signed_tls() -> bool {
    let https_port = std::env::var("HTTPS_PORT").unwrap_or_default();
    !https_port.is_empty()
        && std::env::var("SSL_CERT_PATH").unwrap_or_default().is_empty()
        && std::env::var("TRUST_PROXY").unwrap_or_default().is_empty()
}

fn has_digit_or_dot_first(s: &str) -> bool {
    s.as_bytes().first().is_some_and(|c| c.is_ascii_digit() || *c == b'.')
}

/// Scan every "Version/" occurrence for the /Version\/[\d.]+.*Safari\// shape.
/// JS `.` does not match \n; header values cannot carry \n (forbidden
/// characters), so plain substring matching is faithful.
fn version_safari(ua: &str) -> bool {
    let mut from = 0;
    while let Some(i) = ua[from..].find("Version/") {
        let after = &ua[from + i + "Version/".len()..];
        if has_digit_or_dot_first(after) && after.contains("Safari/") {
            return true;
        }
        from = from + i + 1;
    }
    false
}

/// Twin of isWebKitOnly() in backend/src/coep.js (every iOS browser plus
/// desktop Safari; Blink-based UAs excluded). Hand-rolled twins of the JS
/// regexes /AppleWebKit\//, /iPhone|iPad|iPod/,
/// /(Chrome|Chromium|Edg|OPR|Android)\// and /Version\/[\d.]+.*Safari\//.
pub fn is_webkit_only(ua: &str) -> bool {
    if !ua.contains("AppleWebKit/") {
        return false;
    }
    if ua.contains("iPhone") || ua.contains("iPad") || ua.contains("iPod") {
        return true;
    }
    if ["Chrome/", "Chromium/", "Edg/", "OPR/", "Android/"].iter().any(|t| ua.contains(t)) {
        return false;
    }
    version_safari(ua)
}

/// Twin of coepFor() in backend/src/coep.js, reading the env vars Node
/// reads at request time (SCAN_ISOLATION / SCAN_ISOLATION_WEBKIT === '0').
pub fn coep_for(ua: &str) -> Option<&'static str> {
    coep_for_env(
        ua,
        &std::env::var("SCAN_ISOLATION").unwrap_or_default(),
        &std::env::var("SCAN_ISOLATION_WEBKIT").unwrap_or_default(),
    )
}

pub fn coep_for_env(ua: &str, scan_isolation: &str, scan_isolation_webkit: &str) -> Option<&'static str> {
    if scan_isolation == "0" {
        return None;
    }
    if is_webkit_only(ua) {
        if scan_isolation_webkit == "0" {
            return None;
        }
        return Some("require-corp");
    }
    Some("credentialless")
}

/// Twin of the PRIVATE_ORIGIN regex (server.js:159):
/// ^https?:\/\/(localhost|127\..+|.+\.local|.+\.localhost|
///   (10|172\.(1[6-9]|2\d|3[01])|192\.168)(\..+)?)(:\d+)?$
pub fn is_private_origin(origin: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    // (:\d+)? suffix — strip one trailing :<digits> at the very end.
    let host = match rest.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && !h.contains(':') => h,
        _ => rest,
    };
    if host == "localhost" {
        return true;
    }
    // 127\..+ — at least one char after the dot.
    if host.len() > "127.".len() && host.starts_with("127.") {
        return true;
    }
    // .+\.local / .+\.localhost — non-empty prefix before the suffix.
    if let Some(prefix) = host.strip_suffix(".local") {
        if !prefix.is_empty() {
            return true;
        }
    }
    if let Some(prefix) = host.strip_suffix(".localhost") {
        if !prefix.is_empty() {
            return true;
        }
    }
    // (10|192\.168)(\..+)? — bare or dotted suffix.
    if host == "10" || (host.len() > "10.".len() && host.starts_with("10.")) {
        return true;
    }
    if host == "192.168" || (host.len() > "192.168.".len() && host.starts_with("192.168.")) {
        return true;
    }
    // 172\.(1[6-9]|2\d|3[01])(\..+)?
    if let Some(tail) = host.strip_prefix("172.") {
        let second = tail.split('.').next().unwrap_or("");
        let octet_ok = matches!(
            second.as_bytes(),
            [b'1', b'6'..=b'9'] | [b'2', b'0'..=b'9'] | [b'3', b'0' | b'1']
        );
        if !octet_ok {
            return false;
        }
        // Bare "172.<octet>" (optional group absent) or "<octet>." + >=1 char.
        return tail == second || tail.len() > second.len() + 1;
    }
    false
}

/// Origin of a URL per WHATWG (scheme://host[:port], default port dropped,
/// userinfo stripped), enough for the PUBLIC_URL CORS arm (server.js:171-176).
pub fn url_origin(raw: &str) -> Option<String> {
    let (scheme, rest) = raw.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && !h.contains(':') => (h, Some(p)),
        _ => (authority, None),
    };
    if host.is_empty() {
        return None;
    }
    let mut out = String::from(scheme);
    out.push_str("://");
    out.push_str(host);
    if let Some(p) = port {
        let default = (scheme == "http" && p == "80") || (scheme == "https" && p == "443");
        if !default {
            out.push(':');
            out.push_str(p);
        }
    }
    Some(out)
}

/// Twin of the cors origin callback (server.js:192-199) combined with the
/// explicit-origin list built at server.js:163-176. Env is read per call to
/// keep the twin testable; Node fixes the list at boot, which is equivalent
/// since the container env is immutable.
pub fn origin_allowed(origin: &str) -> bool {
    let explicit: Vec<String> = std::env::var("CORS_ORIGIN")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.starts_with("http://") || s.starts_with("https://") {
                s.to_string()
            } else {
                format!("https://{s}")
            }
        })
        .collect();
    let capacitor = std::env::var("PUBLIC_URL").unwrap_or_default().trim().to_string();
    let capacitor_origin = url_origin(&capacitor);
    origin_allowed_with(origin, &explicit, capacitor_origin.as_deref())
}

pub fn origin_allowed_with(origin: &str, explicit: &[String], capacitor_origin: Option<&str>) -> bool {
    is_private_origin(origin)
        || capacitor_origin == Some(origin)
        || explicit.iter().any(|o| o == origin)
}

fn coep_name() -> HeaderName {
    HeaderName::from_static("cross-origin-embedder-policy")
}

fn default_parts() -> Parts {
    Response::new(Body::empty()).into_parts().0
}

/// helmet 8's default header set for the server.js config, in emission
/// order, split around HSTS (Node emits it between Referrer-Policy and
/// X-Content-Type-Options; it is gated on !selfSignedTls()).
fn helmet_pre() -> Vec<(&'static str, &'static str)> {
    vec![
        ("content-security-policy-report-only", CSP_REPORT_ONLY),
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-resource-policy", "same-origin"),
        ("origin-agent-cluster", "?1"),
        ("referrer-policy", "no-referrer"),
    ]
}

fn helmet_post() -> Vec<(&'static str, &'static str)> {
    vec![
        ("x-content-type-options", "nosniff"),
        ("x-dns-prefetch-control", "off"),
        ("x-download-options", "noopen"),
        ("x-frame-options", "SAMEORIGIN"),
        ("x-permitted-cross-domain-policies", "none"),
        ("x-xss-protection", "0"),
    ]
}

/// Compose a twin'd response header map. Emission order matches Node
/// exactly: COEP, Vary, helmet (…, HSTS when on), ACAO, then everything the
/// handler/compression added. Fidelity rule: where the handler response
/// already carries one of the twin headers — a Node-forwarded reply, or the
/// health cache replaying the headers captured from Node — the existing
/// value WINS (Node's own stack produced it; its /api/health cache proves
/// the replay overrides the per-request COEP middleware). The twin value is
/// only a fallback for pure-native replies. Vary is always merged: Node's
/// stack leads with User-Agent/Origin and compression appends downstream.
fn compose_headers(
    mut existing: HeaderMap,
    coep: Option<&str>,
    vary: &[&str],
    hsts: bool,
    acao: Option<&str>,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    // The caller merged every existing Vary token into `vary` already; drop
    // the raw header so the tail-append below cannot replay it as a second
    // Vary line (caught live: the health-cache replay emitted vary twice —
    // HeaderMap::index returns only the first value, so an equality assert
    // alone cannot see the duplicate).
    existing.remove(header::VARY);
    let slot = |out: &mut HeaderMap, existing: &mut HeaderMap, name: HeaderName, computed: Option<HeaderValue>| {
        if let Some(prev) = existing.remove(&name) {
            out.append(name, prev);
        } else if let Some(v) = computed {
            out.append(name, v);
        }
    };
    slot(&mut out, &mut existing, coep_name(), coep.and_then(|c| HeaderValue::from_str(c).ok()));
    if !vary.is_empty() {
        out.insert(header::VARY, HeaderValue::from_str(&vary.join(", ")).unwrap());
    }
    for (n, val) in helmet_pre() {
        slot(&mut out, &mut existing, HeaderName::from_bytes(n.as_bytes()).unwrap(), Some(HeaderValue::from_str(val).unwrap()));
    }
    if hsts {
        slot(
            &mut out,
            &mut existing,
            HeaderName::from_static("strict-transport-security"),
            Some(HeaderValue::from_static("max-age=31536000; includeSubDomains")),
        );
    }
    for (n, val) in helmet_post() {
        slot(&mut out, &mut existing, HeaderName::from_bytes(n.as_bytes()).unwrap(), Some(HeaderValue::from_str(val).unwrap()));
    }
    if let Some(o) = acao {
        if let Ok(v) = HeaderValue::from_str(o) {
            slot(&mut out, &mut existing, header::ACCESS_CONTROL_ALLOW_ORIGIN, Some(v));
        }
    }
    for (name, value) in existing {
        if let Some(name) = name {
            out.append(name, value);
        }
    }
    out
}

// express res.json twin helper is inlined at the reject arm above.

/// The layer, mounted over the native-route Router (lib.rs). Mirrors the
/// Node middleware order: COEP+vary → helmet → cors → handler/error-handler.
pub async fn guard(req: Request, next: Next) -> Response<Body> {
    let ua = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let preflight = req.method() == axum::http::Method::OPTIONS
        && req.headers().contains_key("access-control-request-method");
    let coep = coep_for(&ua);
    // Node's helmet config turns HSTS OFF exactly when selfSignedTls() is
    // true (a self-signed HTTPS listener would make the HTTP port
    // unreachable behind a pinned HSTS).
    let hsts = !self_signed_tls();

    // cors reject arm (server.js:196 callback(new Error('Not allowed by
    // CORS'))) surfacing through the error handler's 403 twin. COEP +
    // Vary:User-Agent and the full helmet set are already on the reply
    // (they run before cors); NO vary:Origin — the throw happens before
    // cors adds it (verified against the Node instance).
    if let Some(o) = &origin {
        if !origin_allowed(o) {
            let (mut parts, body) = {
                let mut p = default_parts();
                p.status = StatusCode::FORBIDDEN;
                p.headers.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json; charset=utf-8"),
                );
                let body = "{\"error\":\"Origin not allowed\"}".to_string();
                if let Ok(v) = HeaderValue::from_str(&body_etag(body.as_bytes())) {
                    p.headers.insert(header::ETAG, v);
                }
                (p, Body::from(body))
            };
            parts.headers = compose_headers(std::mem::take(&mut parts.headers), coep, &["User-Agent"], hsts, None);
            return Response::from_parts(parts, body);
        }
    }

    // cors preflight arm: 204 with ACAO echo + ACA-Methods (cors@2.8.5
    // defaults: GET,HEAD,PUT,PATCH,POST,DELETE), full stack,
    // Vary: User-Agent, Origin, Access-Control-Request-Headers. A preflight
    // with NO Origin header still gets 204 but no ACAO (cors never calls the
    // origin callback — verified: 204 on the Node instance).
    if preflight {
        let mut parts = default_parts();
        parts.status = StatusCode::NO_CONTENT;
        parts.headers = compose_headers(
            HeaderMap::new(),
            coep,
            &["User-Agent", "Origin", "Access-Control-Request-Headers"],
            hsts,
            origin.as_deref().filter(|o| origin_allowed(o)),
        );
        // cors@2.8.5 preflight only answers when the callback allowed the
        // origin; ACA-Methods is the package default method list.
        if origin.as_deref().is_some_and(origin_allowed) {
            parts.headers.insert(
                HeaderName::from_static("access-control-allow-methods"),
                HeaderValue::from_static("GET,HEAD,PUT,PATCH,POST,DELETE"),
            );
        }
        return Response::from_parts(parts, Body::empty());
    }

    // Normal path: run the native handler, then stack the twin ahead of the
    // handler's own headers. Idempotent for Node-forwarded replies: their
    // identical twin headers are dropped and re-emitted in Node's order.
    let resp = next.run(req).await;
    let (mut parts, body) = resp.into_parts();
    // Vary merge: Node's order is User-Agent (COEP mw), Origin (cors,
    // unconditional for non-rejected requests), then anything else the
    // response already carried (e.g. Accept-Encoding).
    let mut vary: Vec<String> = vec!["User-Agent".to_string(), "Origin".to_string()];
    for existing in parts.headers.get_all(header::VARY).iter().filter_map(|v| v.to_str().ok()) {
        for tok in existing.split(',') {
            let tok = tok.trim();
            if !tok.is_empty()
                && !tok.eq_ignore_ascii_case("user-agent")
                && !tok.eq_ignore_ascii_case("origin")
                && !vary.iter().any(|v| v.eq_ignore_ascii_case(tok))
            {
                vary.push(tok.to_string());
            }
        }
    }
    // compression() runs downstream of cors on Node and appends its own
    // Vary: Accept-Encoding to every response that reaches the routes
    // (rejections and preflights short-circuit at cors and never see it —
    // verified against the Node instance).
    if !vary.iter().any(|v| v.eq_ignore_ascii_case("accept-encoding")) {
        vary.push("Accept-Encoding".to_string());
    }
    let vary_refs: Vec<&str> = vary.iter().map(|s| s.as_str()).collect();
    parts.headers = compose_headers(
        std::mem::take(&mut parts.headers),
        coep,
        &vary_refs,
        hsts,
        origin.as_deref().filter(|o| origin_allowed(o)),
    );
    Response::from_parts(parts, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webkit_matrix() {
        // UAs cross-checked against backend/src/coep.js under Node.
        let w = |u: &str| is_webkit_only(u);
        assert!(!w(""));
        assert!(!w("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36"));
        assert!(w("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1"));
        assert!(w("Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/120.0 Mobile/15E148 Safari/604.1"));
        assert!(!w("Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Mobile Safari/537.36"));
        assert!(w("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15"));
        // Version/ with no digit must NOT match.
        assert!(!w("Mozilla/5.0 (Macintosh) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/Safari/605.1.15"));
        // Edg/ (Edge) excludes; Edge's UA also carries Chrome/ — either wins.
        assert!(!w("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0"));
        // FxiOS is WebKit but the iOS-device arm catches it via iPhone/iPad;
        // a hypothetical Version/-only WebKit without Blink tokens matches.
        assert!(w("Mozilla/5.0 (iPhone) AppleWebKit/605.1.15 (KHTML, like Gecko) FxiOS/120.0 Mobile/15E148 Safari/605.1.15"));
        // No AppleWebKit token at all (curl).
        assert!(!w("curl/8.5.0"));
    }

    #[test]
    fn coep_env_arms() {
        let ios = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";
        let desk = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";
        assert_eq!(coep_for_env(desk, "", ""), Some("credentialless"));
        assert_eq!(coep_for_env(ios, "", ""), Some("require-corp"));
        assert_eq!(coep_for_env(ios, "", "0"), None);
        assert_eq!(coep_for_env(desk, "0", "0"), None);
        assert_eq!(coep_for_env(ios, "0", ""), None);
        // Only the exact string '0' disables.
        assert_eq!(coep_for_env(desk, "false", ""), Some("credentialless"));
    }

    #[test]
    fn private_origin_ladder() {
        let ok = |o: &str| is_private_origin(o);
        assert!(ok("http://localhost:3000"));
        assert!(ok("https://localhost"));
        assert!(ok("http://127.0.0.1:5173"));
        assert!(ok("http://scrybox.local:3001"));
        assert!(ok("http://box.localhost"));
        assert!(ok("http://10.1.2.3:80"));
        assert!(ok("http://192.168.4.37:3002"));
        assert!(ok("https://172.16.0.1"));
        assert!(ok("https://172.31.255.255:443"));
        // Bare (10|172\..|192\.168) hosts match — (\..+)? is optional in the
        // JS regex, so these weirdos are allowed by Node too.
        assert!(ok("http://192.168"));
        assert!(ok("https://172.16"));
        assert!(ok("http://10"));
        assert!(!ok("https://172.15.0.1"));
        assert!(!ok("https://172.32.0.1"));
        assert!(!ok("https://172.160.1.1"));
        assert!(!ok("https://172.16.")); // (\..+)? needs >=1 char after the dot
        assert!(!ok("https://evil.example.com"));
        assert!(!ok("http://localhost.evil.com"));
        assert!(!ok("ftp://localhost"));
        assert!(!ok("http://localhost:abc"));
        assert!(!ok("localhost:3000"));
        assert!(!ok("http://.local")); // .+\.local needs a non-empty prefix
        assert!(!ok("http://.localhost"));
        assert!(!ok("http://127.")); // 127\..+ needs a char after the dot
        assert!(!ok("http://192.168."));
    }

    #[test]
    fn explicit_and_capacitor_origins() {
        let explicit = vec!["https://scry.example.com".to_string(), "https://other.example.com".to_string()];
        assert!(origin_allowed_with("https://scry.example.com", &explicit, None));
        assert!(!origin_allowed_with("https://nope.example.com", &explicit, None));
        assert!(origin_allowed_with("https://app.example", &explicit, Some("https://app.example")));
        // Capacitor arm only ever yields scheme://host[:port] — no path.
        assert_eq!(url_origin("https://app.example.com:8443/index.html#/decks"), Some("https://app.example.com:8443".into()));
        assert_eq!(url_origin("https://app.example.com:443"), Some("https://app.example.com".into()));
        assert_eq!(url_origin("http://app.example.com:80/x"), Some("http://app.example.com".into()));
        assert_eq!(url_origin("https://user:pass@auth.example.com"), Some("https://auth.example.com".into()));
        assert_eq!(url_origin("not-a-url"), None);
        assert_eq!(url_origin("ftp://x.com"), None);
    }

    #[test]
    fn helmet_string_exact() {
        // Byte-exact vs the Node instance (curl -D /api/health, captured
        // 2026-11-11): ';' join, no spaces between directives, helmet
        // defaults merged after the user directives.
        assert!(CSP_REPORT_ONLY.starts_with("default-src 'self';script-src 'self' 'wasm-unsafe-eval';"));
        assert!(CSP_REPORT_ONLY.ends_with(";frame-ancestors 'self';script-src-attr 'none'"));
        assert!(!CSP_REPORT_ONLY.contains("upgrade-insecure-requests"));
        assert!(!CSP_REPORT_ONLY.contains("; "));
    }

    #[tokio::test]
    async fn guard_decorates_native_responses_in_node_order() {
        use axum::routing::get;
        use axum::Router;
        use tower::ServiceExt;

        let app = Router::new()
            .route("/api/thing", get(|| async { (StatusCode::OK, "{\"a\":1}") }))
            .layer(axum::middleware::from_fn(guard));

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/thing")
                    .header("user-agent", "curl/8.5.0")
                    .header("origin", "https://192.168.4.37:3002")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let names: Vec<String> = res
            .headers()
            .keys()
            .map(|k| k.as_str().to_string())
            .collect();
        // Twin headers strictly ahead of the handler's content-type, in
        // Node's order.
        let pos = |n: &str| names.iter().position(|k| k == n).unwrap();
        assert_eq!(pos("cross-origin-embedder-policy"), 0);
        assert_eq!(pos("vary"), 1);
        assert_eq!(pos("content-security-policy-report-only"), 2);
        assert!(pos("x-xss-protection") < pos("content-type"));
        assert_eq!(res.headers()["vary"], "User-Agent, Origin, Accept-Encoding");
        assert_eq!(res.headers()["access-control-allow-origin"], "https://192.168.4.37:3002");
        assert_eq!(res.headers()["x-xss-protection"], "0");
        assert_eq!(res.headers()["x-permitted-cross-domain-policies"], "none");
        assert_eq!(res.headers()["strict-transport-security"], "max-age=31536000; includeSubDomains");

        // Rejected origin: 403 JSON twin with the stack but NO vary:Origin.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/thing")
                    .header("user-agent", "curl/8.5.0")
                    .header("origin", "https://evil.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(res.headers()["vary"], "User-Agent");
        assert!(res.headers().get("access-control-allow-origin").is_none());
        assert_eq!(
            res.headers()["content-security-policy-report-only"],
            CSP_REPORT_ONLY
        );

        // Preflight: 204 + ACA-Methods, vary triple.
        let res = app
            .oneshot(
                Request::builder()
                    .method(axum::http::Method::OPTIONS)
                    .uri("/api/thing")
                    .header("user-agent", "curl/8.5.0")
                    .header("origin", "http://localhost:5173")
                    .header("access-control-request-method", "POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(res.headers()["vary"], "User-Agent, Origin, Access-Control-Request-Headers");
        assert_eq!(res.headers()["access-control-allow-origin"], "http://localhost:5173");
        assert_eq!(res.headers()["access-control-allow-methods"], "GET,HEAD,PUT,PATCH,POST,DELETE");
    }

    #[tokio::test]
    async fn guard_dedups_forwarded_node_headers_and_preserves_vary() {
        use axum::routing::get;
        use axum::Router;
        use tower::ServiceExt;

        // Simulate a Node-forwarded reply: twin headers already present
        // (including vary with Accept-Encoding) plus content-type/etag.
        let app = Router::new()
            .route(
                "/api/fwd",
                get(|| async {
                    (
                        StatusCode::OK,
                        [
                            ("cross-origin-embedder-policy", "credentialless"),
                            ("vary", "User-Agent, Origin, Accept-Encoding"),
                            ("content-security-policy-report-only", CSP_REPORT_ONLY),
                            ("x-xss-protection", "0"),
                            ("content-type", "application/json; charset=utf-8"),
                            ("etag", "W/\"7-abcdef\""),
                        ],
                    )
                }),
            )
            .layer(axum::middleware::from_fn(guard));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/fwd")
                    .header("user-agent", "curl/8.5.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let coep_count = res.headers().get_all("cross-origin-embedder-policy").iter().count();
        assert_eq!(coep_count, 1, "forwarded twin headers must not double");
        let vary_count = res.headers().get_all(header::VARY).iter().count();
        assert_eq!(vary_count, 1, "vary must merge into ONE line, not replay the forwarded one");
        assert_eq!(res.headers()["vary"], "User-Agent, Origin, Accept-Encoding");
        let names: Vec<String> = res.headers().keys().map(|k| k.as_str().to_string()).collect();
        let pos = |n: &str| names.iter().position(|k| k == n).unwrap();
        assert!(pos("content-security-policy-report-only") < pos("content-type"));
        assert!(pos("x-xss-protection") < pos("content-type"));
    }
}

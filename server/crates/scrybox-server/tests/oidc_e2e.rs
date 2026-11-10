//! OIDC end-to-end against a fake IdP (iter 20). Exercises the full ported
//! ladder: authorization-URL construction (PKCE/S256, sealed state token),
//! code exchange (client_secret_basic), claim validation, provisioning,
//! username linking, and the front-end redirect encoding.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    extract::Request,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use serde_json::{json, Value};

use scrybox_server::auth_support::{b64url, b64url_decode, hmac_sha256, sha256};

#[path = "./auth_common.rs"]
mod auth_common;
use auth_common::{build_router, peer, request};

const SECRET: &str = "e2e-state-secret";
const CLIENT_ID: &str = "cid";
const CLIENT_SECRET: &str = "sec";

#[derive(Clone)]
struct IdpCfg {
    sub: String,
    /// extra claims merged into the issued id_token
    claims: Value,
    /// expected values asserted at the token endpoint
    expect_cv: String,
    expect_nonce: String,
    token_status: u16,
    token_error_body: String,
}

fn make_jwt(claims: &Value) -> String {
    let header = b64url(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = b64url(serde_json::to_string(claims).unwrap().as_bytes());
    format!("{header}.{payload}.{}", b64url(b"signature-ignored-under-tls-trust"))
}

async fn spawn_idp(cfg: Arc<Mutex<IdpCfg>>) -> (SocketAddr, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let issuer = format!("http://{addr}");
    let discovery_issuer = issuer.clone();
    let token_issuer = issuer.clone();
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || {
                let iss = discovery_issuer.clone();
                async move {
                    axum::Json(json!({
                        "issuer": iss,
                        "authorization_endpoint": format!("{iss}/authorize"),
                        "token_endpoint": format!("{iss}/token"),
                    }))
                }
            }),
        )
        .route(
            "/token",
            post(move |req: Request| {
                let cfg = cfg.clone();
                let iss = token_issuer.clone();
                async move {
                    let (parts, body) = req.into_parts();
                    let raw = String::from_utf8_lossy(
                        &axum::body::to_bytes(body, usize::MAX).await.unwrap(),
                    )
                    .into_owned();
                    let params: HashMap<String, String> = form_urlencoded::parse(raw.as_bytes())
                        .into_owned()
                        .collect();
                    let c = cfg.lock().unwrap().clone();
                    // Assert exactly what oidc.js sends for client_secret_basic.
                    let auth = parts
                        .headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    assert_eq!(
                        auth,
                        format!(
                            "Basic {}",
                            base64_std(format!("{CLIENT_ID}:{CLIENT_SECRET}").as_bytes())
                        ),
                        "token endpoint basic auth"
                    );
                    assert_eq!(params.get("grant_type").map(String::as_str), Some("authorization_code"));
                    assert_eq!(params.get("code").map(String::as_str), Some("the-code"));
                    assert_eq!(params.get("code_verifier").map(String::as_str), Some(c.expect_cv.as_str()));
                    assert_eq!(
                        params.get("redirect_uri").map(String::as_str),
                        Some("http://app.example/api/auth/oidc/callback")
                    );
                    assert!(params.get("client_id").is_none(), "basic auth must not put client_id in the body");
                    if c.token_status != 200 {
                        return (
                            StatusCode::from_u16(c.token_status).unwrap(),
                            axum::body::Body::from(c.token_error_body.clone()),
                        )
                            .into_response();
                    }
                    let mut claims = c.claims.as_object().unwrap().clone();
                    claims.insert("iss".into(), json!(iss));
                    claims.insert("aud".into(), json!(CLIENT_ID));
                    claims.insert("sub".into(), json!(c.sub));
                    claims.insert(
                        "exp".into(),
                        json!((std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs()
                            + 3600) as f64),
                    );
                    claims.insert("nonce".into(), json!(c.expect_nonce));
                    axum::Json(json!({
                        "access_token": "fake-access",
                        "id_token": make_jwt(&Value::Object(claims)),
                    }))
                        .into_response()
                }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, issuer)
}

// The IdP signs id_tokens with this issuer; OIDC_ISSUER_URL is pinned to it.

fn base64_std(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize % 64] as char);
        out.push(T[(n >> 12) as usize % 64] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize % 64] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize % 64] as char } else { '=' });
    }
    out
}

/// Percent-decode a query component the way `URLSearchParams.get` does on
/// Node's side (both `URLSearchParams` and the Rust builder emit `%3A`/`%2F`
/// for `:`/`/` and `+` for spaces, so the test must decode before comparing
/// semantic values).
fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(q: &str) -> HashMap<String, String> {
    // WHATWG `url.searchParams` percent-encodes values (oidc.js:310-317), so
    // the test must decode them the way the IdP endpoint would.
    form_urlencoded::parse(q.as_bytes()).into_owned().collect()
}

async fn start_flow(
    r: &axum::Router,
) -> (String /*state*/, String /*cv*/, String /*nonce*/, String /*challenge*/) {
    let headers = vec![
        ("x-forwarded-proto", "http".to_string()),
        ("x-forwarded-host", "app.example".to_string()),
    ];
    let (_status, _body) = {
        let _ = ();
        (StatusCode::OK, String::new())
    };
    let (status, hdrs, _) = request(r, peer(), "GET", "/api/auth/oidc/login", &headers, None).await;
    assert_eq!(status, StatusCode::FOUND);
    let loc = hdrs["location"].to_str().unwrap().to_string();
    let (base, q) = loc.split_once('?').unwrap();
    assert!(base.ends_with("/authorize"), "{loc}");
    let p = parse_query(q);
    assert_eq!(p["response_type"], "code");
    assert_eq!(p["client_id"], CLIENT_ID);
    assert_eq!(p["redirect_uri"], "http://app.example/api/auth/oidc/callback");
    assert_eq!(p["scope"], "openid profile email");
    assert_eq!(p["code_challenge_method"], "S256");
    let state = p["state"].clone();
    let payload = b64url_decode(state.split('.').next().unwrap()).unwrap();
    let payload: Value = serde_json::from_slice(&payload).unwrap();
    let cv = payload["cv"].as_str().unwrap().to_string();
    let nonce = payload["n"].as_str().unwrap().to_string();
    assert_eq!(payload["ru"], "http://app.example/api/auth/oidc/callback");
    // sealed challenge must equal S256 of the sealed verifier (RFC 7636)
    assert_eq!(p["code_challenge"], b64url(&sha256(cv.as_bytes())));
    // state signature verifies under the sealed HMAC
    let parts: Vec<&str> = state.split('.').collect();
    assert_eq!(parts.len(), 2);
    assert_eq!(
        b64url(&hmac_sha256(SECRET.as_bytes(), parts[0].as_bytes())),
        parts[1]
    );
    (state, cv, nonce, p["code_challenge"].clone())
}

#[tokio::test]
async fn full_oidc_ladder() {
    let cfg = Arc::new(Mutex::new(IdpCfg {
        sub: "sub-1".into(),
        claims: json!({"preferred_username": "First.User"}),
        expect_cv: String::new(),
        expect_nonce: String::new(),
        token_status: 200,
        token_error_body: String::new(),
    }));
    let (_addr, issuer) = spawn_idp(cfg.clone()).await;
    std::env::set_var("OIDC_ENABLED", "true");
    std::env::set_var("OIDC_ISSUER_URL", &issuer);
    std::env::set_var("OIDC_CLIENT_ID", CLIENT_ID);
    std::env::set_var("OIDC_CLIENT_SECRET", CLIENT_SECRET);
    std::env::set_var("OIDC_SESSION_SECRET", SECRET);
    std::env::remove_var("OIDC_AUTO_PROVISION");
    std::env::remove_var("OIDC_ALLOW_USERNAME_LINK");
    std::env::remove_var("ALLOW_REGISTRATION");
    std::env::remove_var("OIDC_REDIRECT_URI");
    std::env::remove_var("PUBLIC_BASE_URL");

    // ---- first user on an empty DB bootstraps as admin -------------------
    let (r, pool) = build_router(&["auth"]);
    let (state, cv, nonce, _) = start_flow(&r).await;
    {
        let mut c = cfg.lock().unwrap();
        c.expect_cv = cv;
        c.expect_nonce = nonce;
        c.sub = "sub-first".into();
    }
    let (status, hdrs, _) = request(
        &r,
        peer(),
        "GET",
        &format!("/api/auth/oidc/callback?code=the-code&state={state}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FOUND);
    let loc = hdrs["location"].to_str().unwrap().to_string();
    let (base, q) = loc.split_once('?').unwrap();
    assert_eq!(base, "/");
    let p = parse_query(q);
    let token = p.get("oidc_token").expect(&loc).clone();
    assert_eq!(token.len(), 64);
    let row: (String, String, String, i64) = pool
        .get()
        .unwrap()
        .query_row(
            "SELECT username, role, password_hash, share_enabled FROM users WHERE oidc_sub='sub-first'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("admin provisioned");
    assert_eq!(row.0, "admin");
    assert_eq!(row.1, "admin");
    assert!(row.2.starts_with("oidc_locked:"));
    assert_eq!(row.2.len(), "oidc_locked:".len() + 32);
    assert_eq!(row.3, 0);
    // the issued token authenticates /me
    let (status, _, body) =
        request(&r, peer(), "GET", "/api/auth/me", &auth_common::bearer(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["user"]["username"], "admin");

    // ---- username link (disabled by default → operator error) ------------
    let (r, pool) = build_router(&["auth"]);
    pool.get()
        .unwrap()
        .execute(
            "INSERT INTO users (username, role, password_hash, share_token) VALUES ('jane-doe','member','oidc_locked:unused','tok-jane')",
            [],
        )
        .unwrap();
    let (state, cv, nonce, _) = start_flow(&r).await;
    {
        let mut c = cfg.lock().unwrap();
        c.expect_cv = cv;
        c.expect_nonce = nonce;
        c.sub = "sub-jane".into();
        c.claims = json!({"preferred_username": "Jane.Doe"});
    }
    let (status, hdrs, _) = request(
        &r,
        peer(),
        "GET",
        &format!("/api/auth/oidc/callback?code=the-code&state={state}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FOUND);
    let loc = hdrs["location"].to_str().unwrap().to_string();
    assert!(
        loc.contains("oidc_error=A+Scrybox+account+with+this+username+already+exists"),
        "{loc}"
    );

    // enable linking → same flow links the identity instead
    std::env::set_var("OIDC_ALLOW_USERNAME_LINK", "true");
    let (state, cv, nonce, _) = start_flow(&r).await;
    {
        let mut c = cfg.lock().unwrap();
        c.expect_cv = cv;
        c.expect_nonce = nonce;
    }
    let (status, hdrs, _) = request(
        &r,
        peer(),
        "GET",
        &format!("/api/auth/oidc/callback?code=the-code&state={state}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FOUND);
    let loc = hdrs["location"].to_str().unwrap().to_string();
    assert!(loc.starts_with("/?oidc_token="), "{loc}");
    let sub: Option<String> = pool
        .get()
        .unwrap()
        .query_row("SELECT oidc_sub FROM users WHERE username='jane-doe'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sub.as_deref(), Some("sub-jane"));
    std::env::remove_var("OIDC_ALLOW_USERNAME_LINK");

    // ---- unknown user, provisioning OFF → canonical message --------------
    // Node's default is ON (oidc.js:19: only 'false'/'0' disable it), so the
    // off case must set the env explicitly.
    std::env::set_var("OIDC_AUTO_PROVISION", "false");
    let (state, cv, nonce, _) = start_flow(&r).await;
    {
        let mut c = cfg.lock().unwrap();
        c.expect_cv = cv;
        c.expect_nonce = nonce;
        c.sub = "sub-stranger".into();
        c.claims = json!({"preferred_username": "Stranger"});
    }
    let (status, hdrs, _) = request(
        &r,
        peer(),
        "GET",
        &format!("/api/auth/oidc/callback?code=the-code&state={state}"),
        &[],
        None,
    )
    .await;
    let loc = hdrs["location"].to_str().unwrap().to_string();
    assert!(loc.contains("No+matching+Scrybox+account+found"), "{loc}");

    // ---- auto-provision member -------------------------------------------
    std::env::set_var("OIDC_AUTO_PROVISION", "true");
    let (state, cv, nonce, _) = start_flow(&r).await;
    {
        let mut c = cfg.lock().unwrap();
        c.expect_cv = cv;
        c.expect_nonce = nonce;
        c.sub = "sub-newguy".into();
        c.claims = json!({"preferred_username": "New.Guy"});
    }
    let (status, hdrs, _) = request(
        &r,
        peer(),
        "GET",
        &format!("/api/auth/oidc/callback?code=the-code&state={state}"),
        &[],
        None,
    )
    .await;
    assert!(hdrs["location"].to_str().unwrap().starts_with("/?oidc_token="));
    let row: (String, String) = pool
        .get()
        .unwrap()
        .query_row(
            "SELECT username, role FROM users WHERE oidc_sub='sub-newguy'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("provisioned");
    assert_eq!(row, ("new-guy".into(), "member".into()));
    std::env::remove_var("OIDC_AUTO_PROVISION");

    // ---- IdP-side error params -------------------------------------------
    let (status, hdrs, _) = request(
        &r,
        peer(),
        "GET",
        "/api/auth/oidc/callback?error=access_denied&error_description=User+said+no",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(hdrs["location"], "/?oidc_error=User+said+no");
    let (_, hdrs, _) = request(
        &r,
        peer(),
        "GET",
        "/api/auth/oidc/callback?error=access_denied",
        &[],
        None,
    )
    .await;
    assert_eq!(hdrs["location"], "/?oidc_error=access_denied");

    // missing code/state
    let (_, hdrs, _) =
        request(&r, peer(), "GET", "/api/auth/oidc/callback", &[], None).await;
    assert_eq!(
        hdrs["location"],
        "/?oidc_error=Missing+authorization+code+or+state+from+identity+provider."
    );

    // ---- tampered state ----------------------------------------------------
    let (state, _, _, _) = start_flow(&r).await;
    let (_, hdrs, _) = request(
        &r,
        peer(),
        "GET",
        &format!("/api/auth/oidc/callback?code=the-code&state={state}XX"),
        &[],
        None,
    )
    .await;
    assert!(
        hdrs["location"]
            .to_str()
            .unwrap()
            .contains("Invalid+or+expired+OIDC+state+token"),
        "{:?}",
        hdrs["location"]
    );

    // ---- nonce mismatch (replayed state, attacker minted token) -----------
    let (state, cv, nonce, _) = start_flow(&r).await;
    {
        let mut c = cfg.lock().unwrap();
        c.expect_cv = cv;
        c.expect_nonce = "different-nonce".into(); // IdP seals the wrong nonce
        c.sub = "sub-x".into();
    }
    let (_, hdrs, _) = request(
        &r,
        peer(),
        "GET",
        &format!("/api/auth/oidc/callback?code=the-code&state={state}"),
        &[],
        None,
    )
    .await;
    assert!(
        hdrs["location"]
            .to_str()
            .unwrap()
            .contains("nonce+does+not+match+this+login+attempt"),
        "{:?}",
        hdrs["location"]
    );
    let _ = nonce;

    // ---- token endpoint 400 surfaces Node's exchange error text -----------
    {
        let mut c = cfg.lock().unwrap();
        c.token_status = 400;
        c.token_error_body = "{\"error\":\"invalid_grant\"}".into();
    }
    let (state, cv, nonce, _) = start_flow(&r).await;
    {
        let mut c = cfg.lock().unwrap();
        c.expect_cv = cv;
        c.expect_nonce = nonce;
        c.sub = "sub-x".into();
    }
    let (_, hdrs, _) = request(
        &r,
        peer(),
        "GET",
        &format!("/api/auth/oidc/callback?code=the-code&state={state}"),
        &[],
        None,
    )
    .await;
    let loc = hdrs["location"].to_str().unwrap().to_string();
    assert!(loc.contains("OIDC+token+exchange+failed+"), "{loc}");
    // URLSearchParams form-encodes ( ) : { } " (routes/auth.js:59) — only
    // spaces become '+', punctuation becomes %XX.
    assert!(loc.contains("%28HTTP+400%29"), "{loc}");
    assert!(loc.contains("invalid_grant"), "{loc}");

    // ---- rate limiter covers the oidc routes ------------------------------
    // The ladder above rebuilds the router per section (fresh limiter each
    // time), so counting its bucket is meaningless. Node's contract is
    // 20/15min shared-bucket per IP with draft-7 headers on the oidc routes
    // (middleware/auth.js authLimiter, mounted at routes/auth.js:35/50) —
    // assert exactly that on a dedicated router.
    let (rl, _rp) = build_router(&["auth"]);
    for i in 1..=20usize {
        let (status, hdrs, _) =
            request(&rl, peer(), "GET", "/api/auth/oidc/login", &[], None).await;
        assert_ne!(status, StatusCode::TOO_MANY_REQUESTS, "hit {i} must pass");
        assert_eq!(hdrs["ratelimit-limit"], "20");
        assert_eq!(hdrs["ratelimit-remaining"], (20 - i).to_string());
    }
    let (status, hdrs, body) =
        request(&rl, peer(), "GET", "/api/auth/oidc/login", &[], None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "body {body}");
    assert_eq!(hdrs["ratelimit-limit"], "20");
    assert_eq!(body, "{\"error\":\"Too many attempts. Please try again later.\"}");
}

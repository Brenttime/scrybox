//! Node-parity tests for the native /api/auth port (iter 20).
//! Every expectation was transcribed from backend/src/routes/auth.js and
//! backend/src/utils/oidc.js source and cross-checked against the live Node
//! instance on :3312. Register-success lives in tests/auth_register.rs
//! (env-var lifetimes are per test binary).

use std::net::SocketAddr;

use axum::http::{HeaderMap, StatusCode};
use serde_json::{json, Value};

use scrybox_server::{
    auth_support::{b64url, b64url_decode, encode_uri_component, hex_lower, hmac_sha256, pbkdf2_sha512, random_bytes, sha256, timing_safe_equal, uuid_v4},
    ip_limit::{client_ip_key, FixedWindowLimiter, Trust},
    oidc::{extract_user_identity, validate_id_token_claims},
};

#[path = "./auth_common.rs"]
mod auth_common;
use auth_common::{build_router, bearer, json_ct, merge, peer, request};

/// hashlib.pbkdf2_hmac('sha512') vectors (python), both iteration tiers.
fn seed_password_users(pool: &scrybox_server::db::DbPool) {
    pool.get()
        .unwrap()
        .execute_batch(
            "INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key) VALUES
              (1,'user1','oidc_locked:unused','admin','tok-1',1,'tok-api'),
              (2,'user2','210000:0123456789abcdef:8380d69e1f4e0f9bd1f5c206cea28b3d8fb14be3f5a310ef8d182043d1ad792b827e554af044f01937052a0ff6ee3ccae2a03eb5648da39b5ec5ad7857011511','member','tok-2',0,NULL),
              (3,'user3','10000:0123456789abcdef:725b920e3fbc3b4e902d65d5ad631559d2c6a789b459b70c0c3d1262d7ce493de420d2ee023c1e53fab08770458623691d70b9adece2a61ae871c0b595d3f76c','member','tok-3',0,NULL);",
        )
        .unwrap();
}

// ===========================================================================
// Pure-logic vectors (python-generated ground truth)
// ===========================================================================

#[test]
fn crypto_primitives_match_node() {
    // RFC 4231 test case 2 (hmac-sha256)
    assert_eq!(
        hex_lower(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    assert_eq!(
        hex_lower(&sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    // hashlib.pbkdf2_hmac('sha512', dklen=64) vector
    assert_eq!(
        hex_lower(&pbkdf2_sha512(b"supersecret1", b"0123456789abcdef", 210000)),
        "8380d69e1f4e0f9bd1f5c206cea28b3d8fb14be3f5a310ef8d182043d1ad792b827e554af044f01937052a0ff6ee3ccae2a03eb5648da39b5ec5ad7857011511"
    );
    // base64url (Buffer.toString('base64url') — no padding, -_ alphabet);
    // vectors regenerated from python base64.urlsafe_b64encode (iter-20 audit:
    // the original hand-written ones were wrong — 4 bytes encode to 6 chars).
    assert_eq!(b64url(b"\xfb\xff\xbe\xef"), "-_--7w");
    assert_eq!(b64url_decode("-__-").unwrap(), vec![0xfb, 0xff, 0xfe]);
    // encodeURIComponent semantics (URLSearchParams form encoding aside)
    assert_eq!(encode_uri_component("a b*c~d/e?&="), "a%20b*c~d%2Fe%3F%26%3D");
    assert_eq!(encode_uri_component("héllo"), "h%C3%A9llo");
    assert!(timing_safe_equal(b"abc", b"abc"));
    assert!(!timing_safe_equal(b"abc", b"abd"));
    assert!(!timing_safe_equal(b"abc", b"ab"));
    // uuid_v4 shape + version/variant bits
    let u = uuid_v4();
    assert_eq!(u.len(), 36);
    assert_eq!(&u[14..15], "4");
    assert!("89ab".contains(&u[19..20]));
    assert_eq!(random_bytes(4).len(), 4);
}

#[test]
fn password_verifier_matches_node_hash_format() {
    // Both tiers verify through the port's verifier (hashlib vectors).
    assert!(scrybox_server::auth_support::verify_password(
        "supersecret1",
        "210000:0123456789abcdef:8380d69e1f4e0f9bd1f5c206cea28b3d8fb14be3f5a310ef8d182043d1ad792b827e554af044f01937052a0ff6ee3ccae2a03eb5648da39b5ec5ad7857011511"
    ));
    assert!(scrybox_server::auth_support::verify_password(
        "legacypw",
        "10000:0123456789abcdef:725b920e3fbc3b4e902d65d5ad631559d2c6a789b459b70c0c3d1262d7ce493de420d2ee023c1e53fab08770458623691d70b9adece2a61ae871c0b595d3f76c"
    ));
    assert!(!scrybox_server::auth_support::verify_password("nope", &scrybox_server::auth_support::hash_password("supersecret1")));
    assert!(!scrybox_server::auth_support::verify_password("x", ""));
    assert!(!scrybox_server::auth_support::verify_password("x", "oidc_locked:deadbeef"));
    let h = scrybox_server::auth_support::hash_password("whatever123");
    let parts: Vec<&str> = h.split(':').collect();
    assert_eq!(parts[0], "210000");
    assert_eq!(parts[1].len(), 32); // db.js: randomBytes(16).toString('hex')
    assert_eq!(parts[2].len(), 128);
}

#[test]
fn session_expiry_matches_js_date_format() {
    // python: datetime.fromtimestamp(1762790400123/1000 + 7*86400, utc)
    assert_eq!(scrybox_server::auth_routes::session_expiry_str(1762790400123), "2025-11-17 16:00:00");
}

#[test]
fn client_ip_key_trust_matrix() {
    let mut h = HeaderMap::new();
    h.insert("x-forwarded-for", "203.0.113.5, 70.41.3.18".parse().unwrap());
    let peer: SocketAddr = "127.0.0.1:1".parse().unwrap();
    assert_eq!(client_ip_key(Some(peer.ip()), &h, &Trust::None), "127.0.0.1");
    // express trust proxy: hops=1 → rightmost XFF entry
    assert_eq!(client_ip_key(Some(peer.ip()), &h, &Trust::Hops(1)), "70.41.3.18");
    assert_eq!(client_ip_key(Some(peer.ip()), &h, &Trust::Hops(2)), "203.0.113.5");
    let mut h2 = HeaderMap::new();
    h2.insert("x-forwarded-for", "not, an, ip".parse().unwrap());
    assert_eq!(client_ip_key(Some(peer.ip()), &h2, &Trust::Hops(1)), "127.0.0.1");
}

#[test]
fn client_ip_key_rules_gate_xff_on_socket_trust() {
    // proxy-addr forwarded(): the XFF chain is consulted only when the
    // socket is trusted. This is the parity.mjs battery shape — unique
    // 198.18.x.y XFFs from the docker/loopback socket, which Node resolves
    // per-request (untrusted XFF value wins) while an untrusted socket must
    // ignore the header outright.
    let trust = Trust::from_env_value("loopback, uniquelocal");
    let mut h = HeaderMap::new();
    h.insert("x-forwarded-for", "198.18.0.7".parse().unwrap());
    let peer: SocketAddr = "127.0.0.1:1".parse().unwrap();
    assert_eq!(client_ip_key(Some(peer.ip()), &h, &trust), "198.18.0.7");
    let docker: SocketAddr = "172.18.0.1:1".parse().unwrap(); // bridge = uniquelocal
    assert_eq!(client_ip_key(Some(docker.ip()), &h, &trust), "198.18.0.7");
    // Untrusted socket: spoofed XFF must NOT mint a fresh bucket.
    let public: SocketAddr = "203.0.113.9:1".parse().unwrap();
    assert_eq!(client_ip_key(Some(public.ip()), &h, &trust), "203.0.113.9");
    // Trusted right-hand hop is walked; the untrusted client stays.
    h.insert("x-forwarded-for", "198.18.0.7, 172.18.0.2".parse().unwrap());
    assert_eq!(client_ip_key(Some(peer.ip()), &h, &trust), "198.18.0.7");
    // All hops trusted → proxy-addr's walk finds no untrusted hop and falls
    // back to the socket (every actor in the chain is a proxy we trust).
    h.insert("x-forwarded-for", "10.0.0.5, 127.0.0.1".parse().unwrap());
    assert_eq!(client_ip_key(Some(peer.ip()), &h, &trust), "127.0.0.1");
    // No XFF at all → socket.
    let h3 = HeaderMap::new();
    assert_eq!(client_ip_key(Some(peer.ip()), &h3, &trust), "127.0.0.1");
}

#[test]
fn fixed_window_limiter_matches_express_rate_limit() {
    let lim = FixedWindowLimiter::new(3, 60_000);
    let t0 = 1_000_000u64;
    for i in 0..3u64 {
        let v = lim.hit("1.2.3.4", t0);
        assert!(!v.blocked, "request {i} must pass");
        assert_eq!(v.remaining, 2 - i);
        assert_eq!(v.reset_secs, 60);
    }
    assert!(lim.hit("1.2.3.4", t0).blocked);
    assert!(!lim.hit("9.9.9.9", t0).blocked);
    assert!(!lim.hit("1.2.3.4", t0 + 60_001).blocked);
}

#[test]
fn oidc_state_token_signs_verifies_and_expires() {
    std::env::set_var("OIDC_SESSION_SECRET", "unit-test-secret");
    let secret = b"unit-test-secret";
    let mk = |age_ms: u64| -> (String, String, String) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let payload = json!({"cv": "verifier", "n": "nonce1", "ru": "http://x/cb", "t": now - age_ms, "r": "aabbccdd"});
        let payload_b64 = b64url(serde_json::to_string(&payload).unwrap().as_bytes());
        let sig = b64url(&hmac_sha256(secret, payload_b64.as_bytes()));
        (format!("{payload_b64}.{sig}"), "verifier".into(), "nonce1".into())
    };
    let (state, cv, nonce) = mk(0);
    let parsed = scrybox_server::oidc::verify_state_token(&state).expect("valid");
    assert_eq!(parsed["cv"], cv);
    assert_eq!(parsed["n"], nonce);
    // tampered signature rejected
    assert!(scrybox_server::oidc::verify_state_token(&format!("{state}x")).is_none());
    let (expired, _, _) = mk(11 * 60 * 1000);
    assert!(scrybox_server::oidc::verify_state_token(&expired).is_none());
    // signed with a different secret → rejected (HMAC key mismatch)
    let payload = json!({"cv":"v","n":"n","ru":"r","t":1,"r":"x"});
    let pb = b64url(serde_json::to_string(&payload).unwrap().as_bytes());
    let bad = format!("{pb}.{}", b64url(&hmac_sha256(b"other-secret", pb.as_bytes())));
    assert!(scrybox_server::oidc::verify_state_token(&bad).is_none());
}

#[test]
fn oidc_claim_validation_matches_node() {
    let now_s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as f64;
    let base = |extra: Value| -> Value {
        let mut m = serde_json::Map::new();
        if let Value::Object(o) = extra {
            for (k, v) in o {
                m.insert(k, v);
            }
        }
        m.entry("iss".to_string()).or_insert_with(|| json!("https://idp.example"));
        m.entry("aud".to_string()).or_insert_with(|| json!("cid"));
        m.entry("exp".to_string()).or_insert_with(|| json!(now_s + 3600.0));
        m.entry("nonce".to_string()).or_insert_with(|| json!("n1"));
        Value::Object(m)
    };
    assert!(validate_id_token_claims(&base(json!({})), "https://idp.example", "cid", "n1").is_ok());
    // trailing slashes normalised on both sides
    assert!(validate_id_token_claims(&base(json!({"iss": "https://idp.example/"})), "https://idp.example/", "cid", "n1").is_ok());
    assert_eq!(
        validate_id_token_claims(&base(json!({"iss": "https://evil"})), "https://idp.example", "cid", "n1").unwrap_err(),
        "OIDC ID token was issued by \"https://evil\", not by the configured issuer."
    );
    assert_eq!(
        validate_id_token_claims(&base(json!({"aud": "other"})), "https://idp.example", "cid", "n1").unwrap_err(),
        "OIDC ID token was not issued for this client."
    );
    // array aud needs azp naming us
    assert_eq!(
        validate_id_token_claims(&base(json!({"aud": ["cid","x"]})), "https://idp.example", "cid", "n1").unwrap_err(),
        "OIDC ID token has multiple audiences but no azp claim, so it cannot be tied to a single client."
    );
    assert!(validate_id_token_claims(&base(json!({"aud": ["cid","x"], "azp": "cid"})), "https://idp.example", "cid", "n1").is_ok());
    assert_eq!(
        validate_id_token_claims(&base(json!({"aud": ["cid","x"], "azp": "x"})), "https://idp.example", "cid", "n1").unwrap_err(),
        "OIDC ID token was authorized for a different client."
    );
    // exp within 60s skew passes, beyond fails
    assert!(validate_id_token_claims(&base(json!({"exp": now_s - 30.0})), "https://idp.example", "cid", "n1").is_ok());
    assert_eq!(
        validate_id_token_claims(&base(json!({"exp": now_s - 3601.0})), "https://idp.example", "cid", "n1").unwrap_err(),
        "OIDC ID token has expired. Please try logging in again."
    );
    assert_eq!(
        validate_id_token_claims(&base(json!({"nonce": "other"})), "https://idp.example", "cid", "n1").unwrap_err(),
        "OIDC ID token nonce does not match this login attempt."
    );
}

#[test]
fn oidc_username_extraction_matches_node_ladder() {
    // claim precedence, lowercasing, sanitiser, and the user-<stem>-<hex6> fallback
    assert_eq!(extract_user_identity(&json!({"sub": "s", "preferred_username": "Jane.Doe"})).unwrap().1, "jane-doe");
    assert_eq!(extract_user_identity(&json!({"sub": "s", "nickname": "  x y  "})).unwrap().1, "x-y");
    assert_eq!(extract_user_identity(&json!({"sub": "s", "name": "Ab"})).unwrap().1.len(), "user-ab-".len() + 6, "short names get the random fallback");
    assert!(extract_user_identity(&json!({"sub": "s", "name": "Ab"})).unwrap().1.starts_with("user-ab-"));
    assert_eq!(extract_user_identity(&json!({"sub": "s", "email": "a.b+tag@d.io"})).unwrap().1, "a-b-tag");
    // sub alone sanitises; long names slice at 32
    let long = "a".repeat(40);
    assert_eq!(extract_user_identity(&json!({"sub": long.clone()})).unwrap().1.len(), 32);
    // sub required
    assert_eq!(
        extract_user_identity(&json!({})).unwrap_err(),
        "OIDC response is missing the required \"sub\" claim."
    );
    // configured claim name wins
    std::env::set_var("OIDC_USER_CLAIM", "custom_claim");
    assert_eq!(extract_user_identity(&json!({"sub": "s", "custom_claim": "Custom_Name", "preferred_username": "p"})).unwrap().1, "custom_name");
    std::env::remove_var("OIDC_USER_CLAIM");
}

// ===========================================================================
// HTTP parity against routes/auth.js
// ===========================================================================

#[tokio::test]
async fn config_reports_flags_with_weak_etag() {
    let (r, _p) = build_router(&["auth"]);
    let (status, headers, body) = request(&r, peer(), "GET", "/api/auth/config", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/json; charset=utf-8");
    assert!(headers["etag"].to_str().unwrap().starts_with("W/\""));
    assert_eq!(
        body,
        "{\"registrationEnabled\":false,\"setupRequired\":true,\"oidcEnabled\":false,\"oidcProviderName\":\"Single Sign-On\"}"
    );
    // Express conditional-GET: matching weak ETag → 304, empty body.
    let etag = headers["etag"].to_str().unwrap().to_string();
    let (status, _, body) =
        request(&r, peer(), "GET", "/api/auth/config", &[("if-none-match", etag.clone())], None)
            .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
}

#[tokio::test]
async fn me_requires_token_and_serialises_req_user() {
    let (r, pool) = build_router(&["auth"]);
    pool.get()
        .unwrap()
        .execute_batch(
            "INSERT INTO users (id, username, password_hash, role, share_token, share_enabled, api_key) VALUES (1,'user1','oidc_locked:unused','admin','tok-1',1,'tok-api');
             INSERT INTO sessions (token, user_id, expires_at) VALUES ('tok-u1',1,'2999-01-01 00:00:00');")
        .unwrap();

    let (status, _, body) = request(&r, peer(), "GET", "/api/auth/me", &[], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, "{\"error\":\"Access denied. No token provided.\"}");

    let (status, _, body) = request(&r, peer(), "GET", "/api/auth/me", &bearer("nope"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, "{\"error\":\"Invalid or expired session token.\"}");

    // req.user serialisation: exact middleware key order and defaults
    let (status, _, body) = request(&r, peer(), "GET", "/api/auth/me", &bearer("tok-u1"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        "{\"user\":{\"id\":1,\"username\":\"user1\",\"role\":\"admin\",\"share_token\":\"tok-1\",\"share_enabled\":1,\"api_key\":\"tok-api\",\"oidc_sub\":null,\"via_api_key\":false}}"
    );

    // API-key bearer authenticates reads with via_api_key=true
    let (status, _, body) = request(&r, peer(), "GET", "/api/auth/me", &bearer("tok-api"), None).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["user"]["via_api_key"], true);

    // Node splits the header on single spaces: 'xyz' (no Bearer) has no [1]
    // segment → treated as missing.
    let mut h = vec![("authorization", "xyz".to_string())];
    let (status, _, _) = request(&r, peer(), "GET", "/api/auth/me", &h, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    h[0] = ("authorization", "Bearer".to_string());
    let (status, _, _) = request(&r, peer(), "GET", "/api/auth/me", &h, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_success_legacy_and_parser_quirks() {
    let (r, pool) = build_router(&["auth"]);
    seed_password_users(&pool);

    let (status, headers, body) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/login",
        &json_ct(),
        Some("{\"username\":\"User2\",\"password\":\"supersecret1\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["ratelimit-limit"], "20");
    assert_eq!(headers["ratelimit-remaining"], "19");
    assert_eq!(headers["ratelimit-reset"], "900");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["message"], "Login successful");
    assert_eq!(v["user"]["username"], "user2"); // trimmed+lowered lookup, DB echo
    assert_eq!(v["user"]["role"], "member");
    assert_eq!(v["user"]["api_key"], ""); // NULL → ''
    let token = v["token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 64);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

    // legacy 10000-iteration hash verifies
    let (status, _, _) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/login",
        &json_ct(),
        Some("{\"username\":\"user3\",\"password\":\"legacypw\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // wrong password and unknown user share one 401
    let (status, _, body) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/login",
        &json_ct(),
        Some("{\"username\":\"user2\",\"password\":\"wrong\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, "{\"error\":\"Invalid username or password\"}");
    let (status, _, body) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/login",
        &json_ct(),
        Some("{\"username\":\"ghost\",\"password\":\"whatever1\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, "{\"error\":\"Invalid username or password\"}");

    // missing fields → 400
    let (status, _, body) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/login",
        &json_ct(),
        Some("{\"username\":\"user2\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "{\"error\":\"Username and password are required\"}");

    // the fresh session authenticates immediately
    let (status, _, _) = request(&r, peer(), "GET", "/api/auth/me", &bearer(&token), None).await;
    assert_eq!(status, StatusCode::OK);

    // express.json({strict:true}): scalar bodies are parse failures → generic 500
    for bad in ["5", "\"str\"", "true", "{bad"] {
        let (status, _, body) =
            request(&r, peer(), "POST", "/api/auth/login", &json_ct(), Some(bad.into())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "body {bad}");
        assert_eq!(body, "{\"error\":\"Internal server error\"}");
    }

    // oversized body → 413 with the export/import text
    let big = format!("{{\"username\":\"a\",\"password\":\"{}\"}}", "x".repeat(1024 * 1024));
    let (status, _, body) =
        request(&r, peer(), "POST", "/api/auth/login", &json_ct(), Some(big)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body, "{\"error\":\"Upload too large. Try exporting/importing in smaller batches.\"}");
}

#[tokio::test]
async fn register_is_disabled_by_default() {
    let (r, _p) = build_router(&["auth"]);
    let (status, _, body) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/register",
        &json_ct(),
        Some("{\"username\":\"someone\",\"password\":\"goodpass123\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body,
        "{\"error\":\"Registration is disabled. Ask an administrator for an account.\"}"
    );
}

#[tokio::test]
async fn bootstrap_creates_owner_and_adopts_orphans() {
    let (r, pool) = build_router(&["auth"]);
    // Node runs every writer with PRAGMA foreign_keys=ON (db.js:56), so a
    // real orphan row is user_id IS NULL with a card_id that EXISTS in
    // card_cache — mint the parent card row first, exactly as the live DB
    // would have it.
    pool.get()
        .unwrap()
        .execute_batch(
            "INSERT INTO card_cache (id, name) VALUES ('orphan-1','Orphan Probe');
             INSERT INTO collection (card_id, quantity, user_id) VALUES ('orphan-1',1,NULL);",
        )
        .unwrap();

    let (status, _, body) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/bootstrap",
        &json_ct(),
        Some("{\"password\":\"ownerpass123\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["message"], "Setup complete");
    let keys: Vec<String> = v["user"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["username", "role", "share_token", "share_enabled", "api_key"]);
    assert_eq!(v["user"]["username"], "admin");
    assert_eq!(v["user"]["role"], "admin");
    assert_eq!(v["user"]["share_enabled"], 0);
    assert_eq!(v["user"]["api_key"], "");
    let token = v["token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 64);

    // orphans adopted by the new owner
    let uid: i64 = pool
        .get()
        .unwrap()
        .query_row("SELECT user_id FROM collection WHERE card_id='orphan-1'", [], |r| r.get(0))
        .unwrap();
    let admin_id: i64 = pool
        .get()
        .unwrap()
        .query_row("SELECT id FROM users WHERE username='admin'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(uid, admin_id);

    // second attempt → 409
    let (status, _, body) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/bootstrap",
        &json_ct(),
        Some("{\"password\":\"ownerpass123\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body, "{\"error\":\"This instance is already set up. Log in instead.\"}");

    // validations run BEFORE the count check (password first in source order)
    let (status, _, body) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/bootstrap",
        &json_ct(),
        Some("{\"password\":\"short\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "{\"error\":\"Password must be at least 8 characters\"}");
    let (status, _, body) =
        request(&r, peer(), "POST", "/api/auth/bootstrap", &json_ct(), Some("{}".into())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "{\"error\":\"A password is required\"}");

    // the owner can log in and read /me
    let (status, _, _) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/login",
        &json_ct(),
        Some("{\"username\":\"ADMIN\",\"password\":\"ownerpass123\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = request(&r, peer(), "GET", "/api/auth/me", &bearer(&token), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn logout_and_api_key_round_trip() {
    let (r, pool) = build_router(&["auth"]);
    seed_password_users(&pool);
    let (status, _, body) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/login",
        &json_ct(),
        Some("{\"username\":\"user2\",\"password\":\"supersecret1\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = serde_json::from_str::<Value>(&body).unwrap()["token"].as_str().unwrap().to_string();

    // authenticateToken precedes the logout handler
    let (status, _, body) = request(&r, peer(), "POST", "/api/auth/logout", &[], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, "{\"error\":\"Access denied. No token provided.\"}");

    // API keys are read-only for writes
    let (status, _, body) = request(&r, peer(), "POST", "/api/auth/logout", &bearer("tok-api"), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, "{\"error\":\"API keys are read-only. Use a login session for this request.\"}");

    // mint an API key (no limiter on this route in Node)
    let (status, headers, body) =
        request(&r, peer(), "POST", "/api/auth/api-key", &bearer(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get("ratelimit-limit").is_none());
    let key = serde_json::from_str::<Value>(&body).unwrap()["api_key"].as_str().unwrap().to_string();
    assert!(key.starts_with("bnd_"));
    assert_eq!(key.len(), 4 + 48);

    // the key works for reads
    let (status, _, body) = request(&r, peer(), "GET", "/api/auth/me", &bearer(&key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["user"]["via_api_key"], true);

    // revoke → key dead
    let (status, _, body) =
        request(&r, peer(), "DELETE", "/api/auth/api-key", &bearer(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "{\"message\":\"API key revoked\"}");
    let (status, _, _) = request(&r, peer(), "GET", "/api/auth/me", &bearer(&key), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // logout kills the session (token taken from the Authorization header)
    let (status, _, body) =
        request(&r, peer(), "POST", "/api/auth/logout", &bearer(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "{\"message\":\"Logged out successfully\"}");
    let (status, _, _) = request(&r, peer(), "GET", "/api/auth/me", &bearer(&token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn settings_password_share_and_readonly_gating() {
    let (r, pool) = build_router(&["auth"]);
    seed_password_users(&pool);
    let (status, _, body) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/login",
        &json_ct(),
        Some("{\"username\":\"user2\",\"password\":\"supersecret1\"}".into()),
    )
    .await;
    let token = serde_json::from_str::<Value>(&body).unwrap()["token"].as_str().unwrap().to_string();

    // write with an API key → read-only 403 (no limiter, authenticateToken only)
    let (status, _, _) = request(
        &r,
        peer(),
        "PUT",
        "/api/auth/settings",
        &merge(&bearer("tok-api"), &json_ct()),
        Some("{\"share_enabled\":true}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // wrong current password
    let (status, _, body) = request(
        &r,
        peer(),
        "PUT",
        "/api/auth/settings",
        &merge(&bearer(&token), &json_ct()),
        Some("{\"password\":\"brandnewpass1\",\"current_password\":\"nope\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, "{\"error\":\"Current password is incorrect\"}");

    // short new password
    let (status, _, body) = request(
        &r,
        peer(),
        "PUT",
        "/api/auth/settings",
        &merge(&bearer(&token), &json_ct()),
        Some("{\"password\":\"short\",\"current_password\":\"supersecret1\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "{\"error\":\"Password must be at least 8 characters\"}");

    // successful change
    let (status, _, body) = request(
        &r,
        peer(),
        "PUT",
        "/api/auth/settings",
        &merge(&bearer(&token), &json_ct()),
        Some("{\"password\":\"brandnewpass1\",\"current_password\":\"supersecret1\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["message"], "Settings updated successfully");
    let keys: Vec<String> = v["user"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["username", "role", "share_token", "share_enabled", "api_key"]);
    assert_eq!(v["user"]["username"], "user2");

    // the OLD session survives (Node does not invalidate sessions)
    let (status, _, _) = request(&r, peer(), "GET", "/api/auth/me", &bearer(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    // new password works, old one does not
    let (status, _, _) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/login",
        &json_ct(),
        Some("{\"username\":\"user2\",\"password\":\"brandnewpass1\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/login",
        &json_ct(),
        Some("{\"username\":\"user2\",\"password\":\"supersecret1\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // share_enabled coerces by JS truthiness; regenerate rotates the token
    let (status, _, body) = request(
        &r,
        peer(),
        "PUT",
        "/api/auth/settings",
        &merge(&bearer(&token), &json_ct()),
        Some("{\"share_enabled\":\"yes\",\"regenerate_share_token\":1}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["user"]["share_enabled"], 1);
    assert_ne!(v["user"]["share_token"], "tok-2");
    assert_eq!(v["user"]["share_token"].as_str().unwrap().len(), 32);

    // falsy share_enabled → 0
    let (status, _, body) = request(
        &r,
        peer(),
        "PUT",
        "/api/auth/settings",
        &merge(&bearer(&token), &json_ct()),
        Some("{\"share_enabled\":0}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["user"]["share_enabled"], 0);
}

#[tokio::test]
async fn rate_limit_shared_bucket_per_ip() {
    let (r, _p) = build_router(&["auth"]);
    for i in 0..20 {
        let (status, headers, _) = request(
            &r,
            peer(),
            "POST",
            "/api/auth/login",
            &json_ct(),
            Some("{\"username\":\"x\",\"password\":\"y\"}".into()),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {i}");
        assert_eq!(headers["ratelimit-remaining"].to_str().unwrap(), (19 - i).to_string());
        assert_eq!(headers["ratelimit-policy"], "20;w=900");
    }
    let (status, headers, body) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/login",
        &json_ct(),
        Some("{\"username\":\"x\",\"password\":\"y\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body, "{\"error\":\"Too many attempts. Please try again later.\"}");
    assert_eq!(headers["ratelimit-remaining"], "0");
    assert_eq!(headers["retry-after"], headers["ratelimit-reset"]);

    // one shared bucket per IP: register is spent too
    let (status, _, _) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/register",
        &json_ct(),
        Some("{\"username\":\"zzz\",\"password\":\"goodpass123\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // a different socket IP is unaffected
    let other: SocketAddr = "198.51.100.7:1234".parse().unwrap();
    let (status, _, _) = request(
        &r,
        other,
        "POST",
        "/api/auth/login",
        &json_ct(),
        Some("{\"username\":\"x\",\"password\":\"y\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // unlimited routes keep working even with the peer's bucket burned
    let (status, _, _) = request(&r, peer(), "GET", "/api/auth/config", &[], None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn trust_proxy_changes_bucket_key() {
    std::env::set_var("TRUST_PROXY", "1");
    let (r, _p) = build_router(&["auth"]);
    std::env::remove_var("TRUST_PROXY");
    let mut headers = json_ct();
    headers.push(("x-forwarded-for", "198.51.100.55".into()));
    for _ in 0..20 {
        let (status, _, _) = request(
            &r,
            peer(),
            "POST",
            "/api/auth/login",
            &headers,
            Some("{\"username\":\"x\",\"password\":\"y\"}".into()),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, _, _) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/login",
        &headers,
        Some("{\"username\":\"x\",\"password\":\"y\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    // same socket, no XFF → different key (the socket IP)
    let (status, _, _) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/login",
        &json_ct(),
        Some("{\"username\":\"x\",\"password\":\"y\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_auth_paths_still_run_the_global_parser_then_proxy() {
    let (r, _p) = build_router(&["auth"]);
    // unknown path/method → proxy to Node (unreachable here → 502, no panic)
    let (status, _, _) = request(&r, peer(), "GET", "/api/auth/whatever", &[], None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let (status, _, _) = request(&r, peer(), "PUT", "/api/auth/login", &[], None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    // but express.json ran first, so parser errors outrank the proxy hop
    let (status, _, body) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/whatever",
        &json_ct(),
        Some("{bad".into()),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, "{\"error\":\"Internal server error\"}");
}

#[tokio::test]
async fn trailing_slash_matches_like_express4() {
    let (r, _p) = build_router(&["auth"]);
    let (status, _, _) = request(&r, peer(), "GET", "/api/auth/config/", &[], None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn oidc_disabled_answers_like_node() {
    let (r, _p) = build_router(&["auth"]);
    for path in ["/api/auth/oidc/login", "/api/auth/oidc/callback"] {
        let (status, _, body) = request(&r, peer(), "GET", path, &[], None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
        assert_eq!(body, "{\"error\":\"OIDC authentication is not enabled.\"}");
    }
}

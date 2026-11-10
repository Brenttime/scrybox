//! ALLOW_REGISTRATION=true lives in its own binary so the env var can be
//! pinned for the whole process (Node reads it once at module load).

use serde_json::Value;
use axum::http::StatusCode;

#[path = "./auth_common.rs"]
mod auth_common;

use auth_common::{build_router, json_ct, peer, request};

fn enable() {
    static ON: std::sync::Once = std::sync::Once::new();
    ON.call_once(|| std::env::set_var("ALLOW_REGISTRATION", "true"));
}

#[tokio::test]
async fn register_success_flow() {
    enable();
    let (r, pool) = build_router(&["auth"]);
    let (status, headers, body) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/register",
        &json_ct(),
        Some("{\"username\":\" NewUser \",\"password\":\"goodpass123\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(headers["etag"].to_str().unwrap().starts_with("W/\""));
    let v: Value = serde_json::from_str(&body).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert_eq!(keys, ["message", "token", "user"]);
    assert_eq!(v["message"], "Registration successful");
    let ukeys: Vec<&str> = v["user"].as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert_eq!(ukeys, ["username", "role", "share_token", "share_enabled"]);
    assert_eq!(v["user"]["username"], "newuser"); // trimmed + lowercased
    assert_eq!(v["user"]["role"], "member");
    assert_eq!(v["user"]["share_enabled"], 0);
    let st = v["user"]["share_token"].as_str().unwrap();
    assert_eq!(st.len(), 32);
    assert!(st.chars().all(|c| c.is_ascii_hexdigit()));
    let token = v["token"].as_str().unwrap();
    assert_eq!(token.len(), 64);

    // the returned session immediately authenticates
    let (status, _, body) = request(
        &r,
        peer(),
        "GET",
        "/api/auth/me",
        &auth_common::bearer(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let me: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(me["user"]["username"], "newuser");
    assert_eq!(me["user"]["role"], "member");
    assert_eq!(me["user"]["api_key"], "");

    // duplicate is case-insensitive on the cleaned name
    let (status, _, body) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/register",
        &json_ct(),
        Some("{\"username\":\"NEWUSER\",\"password\":\"goodpass123\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "{\"error\":\"Username is already taken\"}");

    // validation ladder
    let cases: Vec<(&str, u16, &str)> = vec![
        ("{}", 400, "Username and password are required"),
        // JS truthiness: "  " is truthy, so it passes the required-gate and
        // trips the trimmed-length check (routes/auth.js:227-233).
        ("{\"username\":\"  \",\"password\":\"goodpass123\"}", 400, "Username must be at least 3 characters"),
        ("{\"username\":\"ab\",\"password\":\"goodpass123\"}", 400, "Username must be at least 3 characters"),
        ("{\"username\":\"valid\",\"password\":\"short\"}", 400, "Password must be at least 8 characters"),
    ];
    for (body_in, code, msg) in cases {
        let (status, _, body) =
            request(&r, peer(), "POST", "/api/auth/register", &json_ct(), Some(body_in.into()))
                .await;
        assert_eq!(status.as_u16(), code, "case {body_in}: {body}");
        assert_eq!(body, format!("{{\"error\":\"{msg}\"}}"));
    }

    // password really stored as PBKDF2-SHA512 210000 tier
    let hash: String = pool
        .get()
        .unwrap()
        .query_row("SELECT password_hash FROM users WHERE username='newuser'", [], |r| r.get(0))
        .unwrap();
    assert!(hash.starts_with("210000:"));
    let parts: Vec<&str> = hash.split(':').collect();
    assert_eq!(parts[1].len(), 32); // db.js: randomBytes(16).toString('hex')
    assert_eq!(parts[2].len(), 128);
    assert!(scrybox_server::auth_support::verify_password("goodpass123", &hash));

    // the new member can log in
    let (status, _, _) = request(
        &r,
        peer(),
        "POST",
        "/api/auth/login",
        &json_ct(),
        Some("{\"username\":\"newuser\",\"password\":\"goodpass123\"}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

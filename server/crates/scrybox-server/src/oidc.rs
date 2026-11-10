//! Port of `backend/src/utils/oidc.js`. Same trust model (TLS to the token
//! endpoint instead of signature verification), same env knobs, same state
//! token format (`base64url(json).base64url(hmac-sha256)`), same claim
//! checks, same identity extraction. Discovery is cached 1 h per process
//! like Node.

use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use http_body_util::BodyExt;
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Map, Value};

use crate::auth_support::*;

const STATE_TTL_MS: u64 = 10 * 60 * 1000;
const CLOCK_SKEW_SECONDS: u64 = 60;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_default()
}

pub fn is_oidc_enabled() -> bool {
    let v = env("OIDC_ENABLED");
    v == "true" || v == "1"
}

pub fn get_provider_name() -> String {
    let v = env("OIDC_PROVIDER_NAME");
    let t = v.trim();
    if t.is_empty() { "Single Sign-On".to_string() } else { t.to_string() }
}

pub fn is_auto_provision_enabled() -> bool {
    let v = env("OIDC_AUTO_PROVISION");
    !(v == "false" || v == "0")
}

pub fn is_username_link_enabled() -> bool {
    let v = env("OIDC_ALLOW_USERNAME_LINK");
    v == "true" || v == "1"
}

pub fn get_default_role() -> &'static str {
    if env("OIDC_DEFAULT_ROLE") == "admin" { "admin" } else { "member" }
}

pub fn get_user_claim_name() -> String {
    let v = env("OIDC_USER_CLAIM");
    let t = v.trim();
    if t.is_empty() { "preferred_username".to_string() } else { t.to_string() }
}

fn get_scopes() -> String {
    let v = env("OIDC_SCOPES");
    let t = v.trim();
    if t.is_empty() { "openid profile email".to_string() } else { t.to_string() }
}

fn get_client_id() -> String {
    env("OIDC_CLIENT_ID").trim().to_string()
}

fn get_client_secret() -> String {
    env("OIDC_CLIENT_SECRET").trim().to_string()
}

pub fn get_issuer_url() -> String {
    trim_slashes(env("OIDC_ISSUER_URL").trim())
}

fn trim_slashes(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}

/// See oidc.js: signature is taken on trust via TLS, so the transport is
/// the security boundary — https, or http only on loopback.
pub fn assert_issuer_transport(issuer: &str) -> Result<(), String> {
    let url = url2(issuer).ok_or_else(|| format!("OIDC_ISSUER_URL is not a valid URL: {issuer}"))?;
    if url.scheme == "https" {
        return Ok(());
    }
    if url.scheme == "http"
        && matches!(url.hostname.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1")
    {
        return Ok(());
    }
    Err(format!(
        "OIDC_ISSUER_URL must be https (got {}://{}). Identity is only as trustworthy as the connection it arrives over.",
        url.scheme, url.hostname
    ))
}

// ---- minimal URL split (we only ever parse ours/the IdP's http(s) URLs) ---

pub struct Url2 {
    pub scheme: String,
    pub hostname: String,
}

pub fn url2(s: &str) -> Option<Url2> {
    let (scheme, rest) = s.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let hostname = if authority.starts_with('[') {
        authority.split(']').next().unwrap_or(authority)
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    if hostname.is_empty() || scheme.is_empty() {
        return None;
    }
    Some(Url2 { scheme: scheme.to_ascii_lowercase(), hostname: hostname.to_ascii_lowercase() })
}

// ---- HTTP client -----------------------------------------------------------

type Http = Client<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>, String>;

fn http_client() -> &'static Http {
    static CLIENT: OnceLock<Http> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("no native root certificates found")
            .https_or_http()
            .enable_http1()
            .build();
        Client::builder(TokioExecutor::new()).build(connector)
    })
}

async fn send(
    method: hyper::Method,
    url: &str,
    headers: Vec<(&str, String)>,
    body: Option<String>,
) -> Result<(u16, Bytes), String> {
    let u = url2(url).ok_or_else(|| format!("invalid URL {url}"))?;
    if u.scheme != "https" && u.scheme != "http" {
        return Err(format!("unsupported URL scheme {}", u.scheme));
    }
    let req = hyper::Request::builder()
        .method(method.clone())
        .uri(url)
        .header(hyper::header::ACCEPT, "application/json");
    let mut b = req;
    for (k, v) in headers {
        b = b.header(k, v);
    }
    let req = match body {
        Some(bd) => b
            .header(hyper::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(bd)
            .map_err(|e| e.to_string())?,
        None => b.body(String::new()).map_err(|e| e.to_string())?,
    };
    let fut = http_client().request(req);
    let res = match method {
        hyper::Method::GET => tokio::time::timeout(std::time::Duration::from_secs(10), fut)
            .await
            .map_err(|_| "request timed out".to_string())?,
        _ => tokio::time::timeout(std::time::Duration::from_secs(15), fut)
            .await
            .map_err(|_| "request timed out".to_string())?,
    }
    .map_err(|e| format!("request failed: {e}"))?;
    let status = res.status().as_u16();
    let bytes = res
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("body read failed: {e}"))?
        .to_bytes();
    Ok((status, bytes))
}

async fn get_json(url: &str) -> Result<Value, String> {
    let (status, body) = send(hyper::Method::GET, url, vec![], None).await?;
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    serde_json::from_slice(&body).map_err(|e| format!("invalid JSON: {e}"))
}

// ---- discovery --------------------------------------------------------------

struct DiscCache {
    doc: Value,
    expires_ms: u64,
}

fn disc_cache() -> &'static Mutex<Option<DiscCache>> {
    static C: OnceLock<Mutex<Option<DiscCache>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

pub async fn get_discovery(force_refresh: bool) -> Result<Value, String> {
    let issuer = get_issuer_url();
    if issuer.is_empty() {
        return Err("OIDC_ISSUER_URL is not configured".into());
    }
    assert_issuer_transport(&issuer)?;
    let now = now_ms();
    if !force_refresh {
        if let Some(c) = disc_cache().lock().unwrap().as_ref() {
            if c.expires_ms > now {
                return Ok(c.doc.clone());
            }
        }
    }
    let discovery_url = format!("{issuer}/.well-known/openid-configuration");
    let doc = get_json(&discovery_url)
        .await
        .map_err(|e| format!("Failed to fetch OIDC discovery from {discovery_url} ({e})"))?;
    let has = |k: &str| doc.get(k).and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false);
    if !has("authorization_endpoint") || !has("token_endpoint") {
        return Err(format!(
            "Invalid OIDC discovery response from {discovery_url}: missing endpoints"
        ));
    }
    // OIDC Discovery 4.3: the document must name the issuer we configured.
    let doc_issuer = doc.get("issuer").map(|v| v.as_str().unwrap_or_default()).unwrap_or("");
    if trim_slashes(doc_issuer) != issuer {
        return Err(format!(
            "OIDC discovery at {discovery_url} declares issuer \"{doc_issuer}\", which is not {issuer}."
        ));
    }
    disc_cache()
        .lock()
        .unwrap()
        .replace(DiscCache { doc: doc.clone(), expires_ms: now + 60 * 60 * 1000 });
    Ok(doc)
}

#[cfg(test)]
pub fn reset_discovery_cache() {
    *disc_cache().lock().unwrap() = None;
}

// ---- state token ------------------------------------------------------------

fn state_secret() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| {
        let s = env("OIDC_SESSION_SECRET");
        let s = if !s.is_empty() { s } else { env("SESSION_SECRET") };
        if !s.is_empty() { s } else { random_hex(32) }
    })
}

/// `{...data, t: Date.now(), r: randomBytes(8).hex}` — key order preserved
/// (the HMAC covers the serialized bytes).
pub fn create_state_token(data: [(&str, String); 3]) -> String {
    let mut payload = Map::new();
    for (k, v) in data {
        payload.insert(k.into(), Value::String(v));
    }
    payload.insert("t".into(), Value::from(now_ms()));
    payload.insert("r".into(), Value::String(random_hex(8)));
    let json = serde_json::to_string(&Value::Object(payload)).unwrap();
    let payload_b64 = b64url(json.as_bytes());
    let sig = b64url(&hmac_sha256(state_secret().as_bytes(), payload_b64.as_bytes()));
    format!("{payload_b64}.{sig}")
}

pub fn verify_state_token(token: &str) -> Option<Map<String, Value>> {
    let (payload_b64, sig) = token.split_once('.')?;
    if payload_b64.is_empty() || sig.contains('.') {
        return None;
    }
    let expected = b64url(&hmac_sha256(state_secret().as_bytes(), payload_b64.as_bytes()));
    if !timing_safe_equal(sig.as_bytes(), expected.as_bytes()) {
        return None;
    }
    let json = b64url_decode(payload_b64)?;
    let payload: Map<String, Value> = serde_json::from_slice(&json).ok()?;
    let t = payload.get("t").and_then(Value::as_u64)?;
    let now = now_ms();
    if now.saturating_sub(t) > STATE_TTL_MS {
        return None;
    }
    Some(payload)
}

// ---- claims -----------------------------------------------------------------

pub fn parse_jwt_payload(token: &str) -> Value {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return Value::Object(Map::new());
    }
    match b64url_decode(parts[1]).and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
        Some(v) if v.is_object() => v,
        _ => Value::Object(Map::new()),
    }
}

/// Node's `validateIdTokenClaims` — see oidc.js for why each check exists.
pub fn validate_id_token_claims(
    claims: &Value,
    issuer: &str,
    client_id: &str,
    nonce: &str,
) -> Result<(), String> {
    if !claims.is_object() || claims.as_object().map(|m| m.is_empty()).unwrap_or(true) {
        return Err(
            "OIDC provider returned no ID token. The authorization code flow requires one."
                .into(),
        );
    }
    if !issuer.is_empty() && trim_slashes(claims.get("iss").map(|v| v.as_str().unwrap_or("")).unwrap_or("")) != trim_slashes(issuer) {
        let iss = claims.get("iss").map(|v| v.as_str().unwrap_or("")).unwrap_or("");
        return Err(format!(
            "OIDC ID token was issued by \"{iss}\", not by the configured issuer."
        ));
    }
    let audiences: Vec<String> = match claims.get("aud") {
        Some(Value::Array(a)) => a.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect(),
        Some(v) => vec![v.as_str().unwrap_or("").to_string()],
        None => vec![String::new()],
    };
    if !client_id.is_empty() && !audiences.iter().any(|a| a == client_id) {
        return Err("OIDC ID token was not issued for this client.".into());
    }
    if !client_id.is_empty() && audiences.len() > 1 {
        let azp = claims.get("azp").and_then(Value::as_str).unwrap_or("");
        if azp.is_empty() {
            return Err("OIDC ID token has multiple audiences but no azp claim, so it cannot be tied to a single client.".into());
        }
        if azp != client_id {
            return Err("OIDC ID token was authorized for a different client.".into());
        }
    }
    let exp = match claims.get("exp") {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.parse::<f64>().ok(),
        _ => None,
    };
    let exp = match exp {
        Some(e) if e.is_finite() => e,
        _ => return Err("OIDC ID token has no expiry.".into()),
    };
    if exp * 1000.0 + (CLOCK_SKEW_SECONDS as f64 * 1000.0) < now_ms() as f64 {
        return Err("OIDC ID token has expired. Please try logging in again.".into());
    }
    if !nonce.is_empty() {
        let cn = claims.get("nonce").map(|v| v.as_str().unwrap_or("")).unwrap_or("");
        if cn != nonce {
            return Err("OIDC ID token nonce does not match this login attempt.".into());
        }
    }
    Ok(())
}

// ---- authorization URL / code exchange --------------------------------------

fn resolve_redirect_uri(proto: Option<&str>, host: Option<&str>) -> String {
    let v = env("OIDC_REDIRECT_URI");
    if !v.trim().is_empty() {
        return v.trim().to_string();
    }
    let base = env("PUBLIC_BASE_URL");
    let base = base.trim();
    if !base.is_empty() {
        return format!("{}/api/auth/oidc/callback", trim_slashes(base));
    }
    if let Some(h) = host {
        let p = proto.unwrap_or("http");
        return format!("{p}://{h}/api/auth/oidc/callback");
    }
    "http://localhost:3001/api/auth/oidc/callback".to_string()
}

fn token_endpoint_auth_method() -> Result<String, String> {
    let m = env("OIDC_TOKEN_ENDPOINT_AUTH_METHOD");
    let m = m.trim();
    let m = if m.is_empty() { "client_secret_basic".to_string() } else { m.to_string() };
    if !["client_secret_basic", "client_secret_post", "none"].contains(&m.as_str()) {
        return Err(format!(
            "Unsupported OIDC_TOKEN_ENDPOINT_AUTH_METHOD: {m}. Supported values: client_secret_basic, client_secret_post, none"
        ));
    }
    Ok(m)
}

/// Build the IdP authorization URL. Params are appended in Node's literal
/// order with URLSearchParams encoding (space → '+').
pub async fn build_authorization_url(
    proto: Option<&str>,
    host: Option<&str>,
) -> Result<String, String> {
    let discovery = get_discovery(false).await?;
    let client_id = get_client_id();
    if client_id.is_empty() {
        return Err("OIDC_CLIENT_ID is not configured".into());
    }
    let code_verifier = b64url(&random_bytes(32));
    let code_challenge = b64url(&sha256(code_verifier.as_bytes()));
    let nonce = random_hex(16);
    let redirect_uri = resolve_redirect_uri(proto, host);
    let state = create_state_token([
        ("cv", code_verifier),
        ("n", nonce.clone()),
        ("ru", redirect_uri.clone()),
    ]);
    let endpoint = discovery.get("authorization_endpoint").and_then(Value::as_str).unwrap_or("");
    let params = [
        ("response_type", "code".to_string()),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("scope", get_scopes()),
        ("state", state),
        ("nonce", nonce),
        ("code_challenge", code_challenge),
        ("code_challenge_method", "S256".to_string()),
    ];
    let qs = params
        .iter()
        .map(|(k, v)| format!("{}={}", form_urlencode(k), form_urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let sep = if endpoint.contains('?') { '&' } else { '?' };
    Ok(format!("{endpoint}{sep}{qs}"))
}

pub struct ExchangeOk {
    pub claims: Value,
    pub username: String,
    pub sub: String,
}

pub async fn exchange_code(
    code: &str,
    state_token: &str,
    proto: Option<&str>,
    host: Option<&str>,
) -> Result<ExchangeOk, String> {
    let state = verify_state_token(state_token)
        .ok_or("Invalid or expired OIDC state token. Please try logging in again.")?;
    let discovery = get_discovery(false).await?;
    let client_id = get_client_id();
    let client_secret = get_client_secret();
    let auth_method = token_endpoint_auth_method()?;
    let redirect_uri = state
        .get("ru")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| resolve_redirect_uri(proto, host));
    if client_id.is_empty() {
        return Err("OIDC_CLIENT_ID is not configured".into());
    }
    if (auth_method == "client_secret_basic" || auth_method == "client_secret_post")
        && client_secret.is_empty()
    {
        return Err(format!("OIDC_CLIENT_SECRET is required when using {auth_method}"));
    }

    let mut params: Vec<(String, String)> = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), code.into()),
        ("redirect_uri".into(), redirect_uri),
        (
            "code_verifier".into(),
            state.get("cv").and_then(Value::as_str).unwrap_or("").into(),
        ),
    ];
    let mut headers: Vec<(&str, String)> = vec![];
    match auth_method.as_str() {
        "client_secret_basic" => {
            let pair = format!("{client_id}:{client_secret}");
            headers.push((
                "Authorization",
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(pair.as_bytes())
                ),
            ));
        }
        "client_secret_post" => {
            params.push(("client_id".into(), client_id.clone()));
            params.push(("client_secret".into(), client_secret));
        }
        _ => {
            params.push(("client_id".into(), client_id.clone()));
        }
    }
    let body = params
        .iter()
        .map(|(k, v)| format!("{}={}", form_urlencode(k), form_urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let token_endpoint = discovery.get("token_endpoint").and_then(Value::as_str).unwrap_or("");
    let (status, body_bytes) =
        send(hyper::Method::POST, token_endpoint, headers, Some(body)).await.map_err(|e| {
            format!("OIDC token exchange failed at {token_endpoint}: {e}")
        })?;
    if !(200..300).contains(&status) {
        let text = String::from_utf8_lossy(&body_bytes);
        return Err(format!("OIDC token exchange failed (HTTP {status}): {text}"));
    }
    let tokens: Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| format!("OIDC token exchange returned invalid JSON: {e}"))?;

    let id_token = tokens.get("id_token").and_then(Value::as_str).unwrap_or("");
    let mut claims = parse_jwt_payload(id_token);
    validate_id_token_claims(
        &claims,
        discovery.get("issuer").and_then(Value::as_str).unwrap_or(""),
        &client_id,
        state.get("n").and_then(Value::as_str).unwrap_or(""),
    )?;

    // userinfo (best-effort, mirrors Node): claims merge over the ID token.
    if let (Some(ui), Some(at)) = (
        discovery.get("userinfo_endpoint").and_then(Value::as_str),
        tokens.get("access_token").and_then(Value::as_str),
    ) {
        if !ui.is_empty() && !at.is_empty() {
            match send(hyper::Method::GET, ui, vec![("Authorization", format!("Bearer {at}"))], None).await {
                Ok((200..=299, b)) => {
                    if let Ok(ui_claims) = serde_json::from_slice::<Value>(&b) {
                        if let (Some(base), Some(extra)) =
                            (claims.as_object_mut(), ui_claims.as_object())
                        {
                            for (k, v) in extra {
                                base.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
                _ => {
                    tracing::warn!("OIDC userinfo fetch failed, falling back to ID token claims");
                }
            }
        }
    }

    let (sub, username) = extract_user_identity(&claims)?;
    Ok(ExchangeOk { claims, username, sub })
}

/// `extractUserIdentity` from oidc.js — claim precedence, lowercase,
/// sanitize to [a-z0-9_-], collapse dashes, trim, 3..32 length rules.
pub fn extract_user_identity(claims: &Value) -> Result<(String, String), String> {
    let sub = claims.get("sub").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if sub.is_empty() {
        return Err("OIDC response is missing the required \"sub\" claim.".into());
    }
    let claim_name = get_user_claim_name();
    let get = |k: &str| claims.get(k).and_then(Value::as_str).map(|s| s.to_string());
    let email_prefix = claims
        .get("email")
        .and_then(Value::as_str)
        .map(|e| e.split('@').next().unwrap_or("").to_string());
    let raw = get(&claim_name)
        .or_else(|| get("preferred_username"))
        .or_else(|| get("nickname"))
        .or_else(|| get("name"))
        .or(email_prefix)
        .unwrap_or_else(|| sub.clone());
    let raw = raw.trim().to_lowercase();
    let mut clean = String::new();
    let mut prev_dash = false;
    for c in raw.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
            clean.push(c);
            prev_dash = c == '-';
        } else if !prev_dash {
            clean.push('-');
            prev_dash = true;
        }
    }
    let clean = clean.trim_matches('-').to_string();
    let clean = if clean.chars().count() < 3 {
        let suffix = random_hex(3);
        let stem = if clean.is_empty() { "oidc".to_string() } else { clean.clone() };
        format!("user-{stem}-{suffix}")
    } else {
        clean.chars().take(32).collect::<String>()
    };
    Ok((sub, clean))
}

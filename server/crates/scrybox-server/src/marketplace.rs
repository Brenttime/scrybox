//! Port of the MARKETPLACE group from `backend/src/routes/marketplaceOrders.js`
//! + `backend/src/utils/marketplaceOrders.js` (Lane A iter-41,
//! RUST_ROUTES=`marketplace`):
//!   GET  /api/marketplace/accounts        -> masked per-provider status
//!   PUT  /api/marketplace/accounts        -> save/clear one provider's credentials
//!   GET  /api/marketplace/recent/:source  -> picker list (ManaPool)
//!
//! POST /preview and POST /add (slice 2, iter-43): orderFromRequest (source
//! → shape → row → creds gate → provider fetch → parseOrderPayload) then
//! previewOrder / addOrderToCollection on shared legs (precons bulk
//! resolver, card_cache, collection_writes plan_add/execute_add). Scryfall
//! leg, cacheCards DB failure and plan_add `Forward` replay the WHOLE POST
//! to Node — the proxied answers are exact, nothing local was written.
//!
//! Middleware order mirrored from server.js (:475 mount): express.json above
//! the router (malformed-JSON PUT answers the generic 500 / oversize the 413
//! BEFORE any credential is touched), then the router-level
//! `router.use(authenticateToken)` twin (settings::authenticate — API keys
//! pass on GET only), then handler. There is NO admin gate on this router:
//! members manage their own credentials.
//!
//! Faithfulness notes:
//! - The live ManaPool API is probed-verified (utils header comment): the
//!   buyer list GET /buyer/orders IGNORES paging and returns the whole set
//!   oldest-first (so the SORT lives here), the detail is keyed by the row's
//!   UUID `id` not the human order_number (a human number resolves through
//!   the list first), and card counts must come from the detail's nested
//!   order_seller_details[].items — the list rows carry none.
//! - `MANAPOOL_API_BASE` is read PER CALL here (Node reads it once at
//!   require time). Only tests change it; a live process never does.
//! - The HTTP leg mimics `httpGet`: GET with User-Agent 'Scrybox' + Accept
//!   application/json, validateStatus:true (status never throws), JSON-or-
//!   {raw} body, MARKETPLACE_TIMEOUT_MS = `Number(process.env.X || 16000)`
//!   (0 = no timeout, matching axios). Deviations (documented, excluded from
//!   parity per the moxfield-leg ruling): axios follows up to 3 redirects,
//!   hyper does not (ManaPool API paths never redirect); axios network/
//!   timeout messages ("connect ECONNREFUSED …", "timeout of 16000ms
//!   exceeded") cannot be byte-matched from Rust — provider arms are proven
//!   here against the injected stub client, not against live manapool.com.
//! - String()/truthiness/Number() run through the shared js_to_string/
//!   js_falsy/js_number doubles; .slice/.length math is UTF-16 code-unit
//!   math, matching V8. `stampOf` ordering uses the shared root icu collator
//!   (Node localeCompare); Array#sort is stable on both sides.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use http_body_util::BodyExt;
use hyper::header::{ACCEPT, USER_AGENT};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Map, Value};

use crate::card_cache::cache_normalized_cards;
use crate::collection_writes::{
    execute_add, internal_error, js_falsy, js_to_string, json_error, json_response, parse_body,
    plan_add, read_parts, too_large, AddPlan, BodyOutcome,
};
use crate::db::DbPool;
use crate::decks_reads::{js_number, js_trim, root_collator, rows_map};
use crate::precons::bulk_fetch_by_identifier;
use crate::search::js_parse_int;
use crate::settings::authenticate;
use crate::stats;
use crate::AppState;

const USER_AGENT_NAME: &str = "Scrybox";
const MANAPOOL_DEFAULT_BASE: &str = "https://manapool.com/api/v1";
const MANAPOOL_LIST_PATH: &str = "/buyer/orders";
const RECENT_LIMIT: usize = 3;
const MP_CRED_UNSET_MESSAGE: &str = "ManaPool credentials are not configured";
pub const MP_UNAUTH_MESSAGE: &str = "ManaPool rejected the saved credentials. Check the account email and the access token under ManaPool \u{2192} account \u{2192} Integration settings, then save them again.";
pub const MP_RATE_LIMIT_MESSAGE: &str = "ManaPool is rate-limiting this account right now (too many requests or too much traffic). Wait a minute and try again.";

// ---------------------------------------------------------------------------
// Outbound leg (httpGet twin). Trait so tests inject a stub client exactly
// like the Node unit tests inject httpGet.
// ---------------------------------------------------------------------------

pub struct MpResponse {
    pub status: u16,
    pub body: Value,
}

#[derive(Debug)]
pub struct MpError {
    /// JS `err.status` (route fold uses `err.status ? : 502` — 0 behaves as
    /// absent, hence Option).
    pub status: Option<u16>,
    pub message: String,
    pub list_unavailable: bool,
    /// err.observedKeys (only the detail-not-found and parse arms set it;
    /// never reaches the slice-1 responses — the recent route never sees it).
    #[allow(dead_code)]
    pub observed_keys: Option<Vec<Value>>,
}

impl MpError {
    fn err(status: u16, message: impl Into<String>, list_unavailable: bool) -> Self {
        Self { status: Some(status), message: message.into(), list_unavailable, observed_keys: None }
    }
}

pub trait MpHttp: Send + Sync {
    fn get(
        &self,
        url: String,
        extra_headers: Vec<(String, String)>,
    ) -> Pin<Box<dyn Future<Output = Result<MpResponse, MpError>> + Send + 'static>>;
}

type MpClient = Client<HttpsConnector<HttpConnector>, String>;

pub(crate) fn mp_client() -> &'static MpClient {
    static CLIENT: OnceLock<MpClient> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let https = HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("native roots")
            .https_or_http()
            .enable_http1()
            .build();
        Client::builder(hyper_util::rt::TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .build(https)
    })
}

/// `Number(process.env.MARKETPLACE_TIMEOUT_MS || 16000)` — the `||` runs on
/// the RAW string (unset/'' take the default); Number('0')=0 keeps axios'
/// "no timeout" (modelled with u64::MAX), a NaN timeout arms an instant
/// axios timer (clamped to 1 ms; tokio rejects 0 with the same observable
/// instant failure).
fn http_timeout_ms() -> u64 {
    match std::env::var("MARKETPLACE_TIMEOUT_MS") {
        Ok(raw) if !raw.is_empty() => {
            let n = js_number(Some(&Value::String(raw)));
            if n.is_nan() {
                1
            } else if n <= 0.0 {
                u64::MAX / 2
            } else {
                (n as u64).max(1)
            }
        }
        _ => 16000,
    }
}

fn net_fail(err: impl std::fmt::Display) -> MpError {
    MpError::err(502, format!("Marketplace request failed: {err}"), false)
}

pub struct RealHttp;

impl MpHttp for RealHttp {
    fn get(
        &self,
        url: String,
        extra_headers: Vec<(String, String)>,
    ) -> Pin<Box<dyn Future<Output = Result<MpResponse, MpError>> + Send + 'static>> {
        Box::pin(async move {
            let mut b = hyper::Request::builder()
                .method(hyper::Method::GET)
                .uri(&url)
                .header(USER_AGENT, USER_AGENT_NAME)
                .header(ACCEPT, "application/json");
            for (k, v) in extra_headers {
                b = b.header(k, v);
            }
            let req = match b.body(String::new()) {
                Ok(r) => r,
                Err(e) => return Err(net_fail(e)),
            };
            let timeout = http_timeout_ms();
            let res = tokio::time::timeout(Duration::from_millis(timeout), mp_client().request(req))
                .await
                .map_err(|_| net_fail(format!("timeout of {timeout}ms exceeded")))?
                .map_err(net_fail)?;
            let status = res.status().as_u16();
            let bytes = res
                .into_body()
                .collect()
                .await
                .map(|c| c.to_bytes())
                .map_err(net_fail)?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            // axios: a JSON body arrives parsed; a string body that will not
            // parse folds to {raw} for the callers.
            let body =
                serde_json::from_str::<Value>(&text).unwrap_or_else(|_| json!({ "raw": text }));
            Ok(MpResponse { status, body })
        })
    }
}

fn manapool_base() -> String {
    std::env::var("MANAPOOL_API_BASE").unwrap_or_else(|_| MANAPOOL_DEFAULT_BASE.to_string())
}

fn mp_headers(creds: &Creds) -> Vec<(String, String)> {
    vec![
        ("X-ManaPool-Email".to_string(), creds.email.clone()),
        ("X-ManaPool-Access-Token".to_string(), creds.token.clone()),
    ]
}

/// JS encodeURIComponent (unreserved: A-Za-z0-9 - _ . ! ~ * ' ( )).
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric()
            || matches!(c, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')')
        {
            out.push(c);
        } else {
            for b in c.to_string().as_bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// JS math/string helpers (UTF-16 code-unit semantics, matching V8)
// ---------------------------------------------------------------------------

/// JS number -> JSON: whole f64s serialise like JS (2, not 2.0).
fn js_num_json(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() <= 9.007_199_254_740_991e15 {
        Value::Number(serde_json::Number::from(x as i64))
    } else {
        Value::from(x)
    }
}

fn u16s(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn js_slice(s: &str, start: usize, end: usize) -> String {
    let u = u16s(s);
    let hi = end.min(u.len());
    if start >= hi {
        return String::new();
    }
    String::from_utf16_lossy(&u[start..hi])
}

fn js_len(s: &str) -> usize {
    u16s(s).len()
}

/// `String(x || '')` — falsy takes the empty string.
fn to_str_or_empty(v: Option<&Value>) -> String {
    if v.is_none() || js_falsy(v) {
        String::new()
    } else {
        js_to_string(v)
    }
}

/// settingsViews... (utils/marketplaceOrders.js) maskSecret twin; '•' and
/// '…' are one UTF-16 code unit each.
pub fn mask_secret(secret: Option<&Value>) -> String {
    let s = to_str_or_empty(secret);
    if s.is_empty() {
        return String::new();
    }
    let n = js_len(&s);
    if n <= 8 {
        return "\u{2022}\u{2022}\u{2022}\u{2022}".to_string();
    }
    format!(
        "{}\u{2026}{} ({} chars)",
        js_slice(&s, 0, 3),
        js_slice(&s, n - 4, n),
        n
    )
}

/// maskEmail twin: keep 2 local chars + stable-width bullets + full domain;
/// anything without a usable '@' keeps first char + '…' (never blanked).
pub fn mask_email(email: Option<&Value>) -> String {
    let s = to_str_or_empty(email);
    if s.is_empty() {
        return String::new();
    }
    let at = u16s(&s).iter().position(|c| *c == u16::from(b'@'));
    match at {
        None | Some(0) => format!("{}\u{2026}", js_slice(&s, 0, 1)),
        Some(at) => {
            let user = js_slice(&s, 0, at);
            let domain = js_slice(&s, at + 1, usize::MAX);
            let n = ((js_len(&user) as i64) - 2).clamp(2, 8);
            format!("{}{}@{}", js_slice(&user, 0, 2), "\u{2022}".repeat(n as usize), domain)
        }
    }
}

// ---------------------------------------------------------------------------
// Pure folds (utils/marketplaceOrders.js twins)
// ---------------------------------------------------------------------------

fn is_js_ws(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\u{000b}' | '\u{000c}' | '\r' | '\n' | '\u{00a0}' | '\u{feff}'
            | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// /^[^@\s]+@[^@\s]+\.[^@\s]+$/ with JS's exact \s class (JS \s excludes
/// U+0085 and includes U+FEFF — not Rust's \p{White_Space}).
fn email_valid(email: &str) -> bool {
    let in_class = |c: char| c != '@' && !is_js_ws(c);
    let chars: Vec<char> = email.chars().collect();
    let at = match chars.iter().position(|c| *c == '@') {
        Some(i) => i,
        None => return false,
    };
    // Both segments forbid '@', so a second '@' anywhere fails both ways.
    if at < 1 || !chars[..at].iter().all(|c| in_class(*c)) {
        return false;
    }
    let rest = &chars[at + 1..];
    if rest.is_empty() || !rest.iter().all(|c| in_class(*c)) {
        return false;
    }
    // The '\.' needs a dot at rest-index >= 1 with >= 1 class char after it.
    match rest.iter().position(|c| *c == '.') {
        Some(dot) => dot >= 1 && dot < rest.len() - 1,
        None => false,
    }
}

/// /^[A-Za-z0-9_-]{1,64}$/
fn order_number_shape_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn mp_uuid(num: &str) -> bool {
    // /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i
    let parts: Vec<&str> = num.split('-').collect();
    let lens = [8usize, 4, 4, 4, 12];
    parts.len() == 5
        && parts
            .iter()
            .zip(lens)
            .all(|(p, l)| p.len() == l && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// utils pick(): first name where `obj && obj[n] != null && obj[n] !== ''`.
/// (obj falsy — null/0/''/false — reads nothing at all.)
fn pick(obj: Option<&Value>, names: &[&str]) -> Option<Value> {
    let o = obj.filter(|v| !js_falsy(Some(v)))?;
    for n in names {
        if let Some(v) = o.get(n) {
            let ok = !v.is_null() && v.as_str().map(|s| !s.is_empty()).unwrap_or(true);
            if ok {
                return Some(v.clone());
            }
        }
    }
    None
}

fn order_array(body: &Value) -> Vec<Value> {
    if let Value::Array(a) = body {
        return a.clone();
    }
    if !body.is_object() {
        return Vec::new();
    }
    for k in ["data", "orders", "results", "items", "list", "records", "orderResults"] {
        if let Some(v) = body.get(k) {
            if let Value::Array(a) = v {
                if !a.is_empty() && a.iter().all(|o| o.is_object()) {
                    return a.clone();
                }
            }
            if v.is_object() {
                for ik in ["data", "orders", "results"] {
                    if let Some(Value::Array(inner)) = v.get(ik) {
                        if !inner.is_empty() {
                            return inner.clone();
                        }
                    }
                }
            }
        }
    }
    Vec::new()
}

fn sum_item_counts(order: &Value) -> f64 {
    // sellers.reduce((n, s) => n + (Number(s && s.item_count) || 0), 0)
    match order.get("order_seller_details") {
        Some(Value::Array(sellers)) => sellers
            .iter()
            .map(|s| {
                let x = match s {
                    v if js_falsy(Some(v)) => js_number(Some(&Value::Null)), // Number(falsy s)
                    v => js_number(v.get("item_count")),
                };
                if js_falsy(Some(&Value::from(x))) {
                    0.0
                } else {
                    x
                }
            })
            .sum(),
        _ => 0.0,
    }
}

/// `Number(x) || 0`, Math.trunc'd by the caller where JS truncates.
fn number_or_zero(v: Option<&Value>) -> f64 {
    let x = js_number(v);
    if js_falsy(Some(&Value::from(x))) {
        0.0
    } else {
        x
    }
}

fn unwrap_manapool_order(body: &Value) -> Option<&Value> {
    if !(body.is_object() || body.is_array()) {
        return None; // !body || typeof !== 'object' (arrays are 'object')
    }
    if let Some(o) = body.get("order") {
        if o.is_object() {
            return Some(o);
        }
    }
    Some(body)
}

fn manapool_nested_items(order: &Value) -> Vec<Value> {
    match order.get("order_seller_details") {
        Some(Value::Array(sellers)) => sellers
            .iter()
            .filter_map(|s| match s.get("items") {
                Some(Value::Array(items)) if !items.is_empty() => Some(items.clone()),
                _ => None,
            })
            .flatten()
            .collect(),
        _ => Vec::new(),
    }
}

fn is_manapool_sealed(product: Option<&Value>) -> bool {
    let p = match product {
        Some(p) if p.is_object() => p,
        _ => return false,
    };
    let type_s = to_str_or_empty(p.get("product_type")).to_lowercase();
    if !type_s.is_empty() && type_s != "mtg_single" {
        return true;
    }
    p.get("sealed").map(|x| !js_falsy(Some(x))).unwrap_or(false) && p.get("single").is_none()
}

const LINE_KEYS: [&str; 8] =
    ["items", "lineItems", "line_items", "products", "orderItems", "cards", "contents", "entries"];
const SEALED_KINDS: [&str; 8] =
    ["sealed", "bundle", "box", "case", "display", "accessory", "accessories", "supplies"];

/// eachProductLines: returns (product-to-read-identity, wider-row).
fn each_product_lines(item: &Value) -> (Value, Value) {
    if let Some(p) = item.get("product") {
        if p.is_object() {
            // `p.single || p.sealed || p.product || p` — first TRUTHY (every
            // object is truthy; primitives need truthiness).
            let s = ["single", "sealed", "product"]
                .iter()
                .filter_map(|k| p.get(k))
                .find(|v| !js_falsy(Some(v)))
                .cloned()
                .unwrap_or_else(|| p.clone());
            if s.is_object() || s.is_array() {
                return (s, p.clone());
            }
        }
    }
    for k in ["card", "single", "sku", "printing"] {
        if let Some(v) = item.get(k) {
            if v.is_object() {
                return (v.clone(), item.clone());
            }
        }
    }
    (item.clone(), item.clone())
}

macro_rules! static_re {
    ($name:ident, $pattern:literal) => {
        fn $name() -> &'static regex::Regex {
            static RE: OnceLock<regex::Regex> = OnceLock::new();
            RE.get_or_init(|| regex::Regex::new($pattern).unwrap())
        }
    };
}

static_re!(re_sealed_kind_word, r"\b(sealed|booster|bundle|display|case|brick)\b");
static_re!(
    re_box_accessory,
    r"(?i)\bbox(es)?\b|binder|sleeves|protector|playmat|deck box|storage"
);
static_re!(
    re_foil_finish,
    r"holofoil|etched|showcase|textless|overlay|borderless"
);
static_re!(re_foil_word, r"\bfoil\b");
static_re!(re_nonfoil_word, r"non-?foil");
static_re!(re_digits, r"^\d+$");

fn is_sealed_line(prod: &Value, item: &Value) -> bool {
    let kind = pick(
        Some(prod),
        &["kind", "product_type", "productType", "type", "category"],
    )
    .or_else(|| pick(Some(item), &["kind", "product_type", "productType", "type", "category"]))
    .map(|v| js_to_string(Some(&v)).to_lowercase())
    .unwrap_or_default();
    let sealed_flag = pick(Some(prod), &["is_sealed", "isSealed", "sealed"])
        .or_else(|| pick(Some(item), &["is_sealed", "isSealed", "sealed"]));
    let flag_hit = match sealed_flag.as_ref() {
        Some(Value::Bool(true)) => true,
        Some(Value::Number(n)) => n.as_f64() == Some(1.0),
        Some(Value::String(s)) => s == "1",
        _ => false,
    };
    if flag_hit {
        return true;
    }
    if SEALED_KINDS.contains(&kind.as_str()) {
        return true;
    }
    let name = pick(Some(prod), &["name", "title"])
        .or_else(|| pick(Some(item), &["name", "productName"]))
        .map(|v| js_to_string(Some(&v)).to_lowercase())
        .unwrap_or_default();
    if re_sealed_kind_word().is_match(&name) {
        return true;
    }
    if re_box_accessory().is_match(&name)
        && pick(Some(prod), &["scryfallId", "scryfall_id", "tcgplayerProductId", "product_id"])
            .is_none()
        && pick(Some(prod), &["set", "setCode", "set_code"])
            .is_none()
    {
        return true;
    }
    false
}

fn is_foilish(item: &Value) -> bool {
    // `item.isFoil ?? item.foil ?? item.is_foil` — ?? skips ONLY null/undefined.
    let flag = ["isFoil", "foil", "is_foil"]
        .iter()
        .filter_map(|k| item.get(k))
        .find(|v| !v.is_null())
        .cloned();
    match flag.as_ref() {
        Some(Value::Bool(true)) => return true,
        Some(Value::Number(n)) if n.as_f64() == Some(1.0) => return true,
        Some(Value::String(s)) if s == "1" => return true,
        _ => {}
    }
    let fid = {
        let raw = ["finish_id", "finishId"]
            .iter()
            .filter_map(|k| item.get(k))
            .find(|v| !v.is_null())
            .map(|v| js_to_string(Some(v)))
            .unwrap_or_default();
        js_trim(&raw).to_uppercase()
    };
    if fid == "FO" || fid == "FOIL" {
        return true;
    }
    let finish = ["finish", "printing", "edge"]
        .iter()
        .map(|k| to_str_or_empty(item.get(k)))
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .to_lowercase();
    let name = ["name", "productName"]
        .iter()
        .map(|k| to_str_or_empty(item.get(k)))
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .to_lowercase();
    if re_foil_finish().is_match(&format!("{finish} {name}")) {
        return true;
    }
    if re_foil_word().is_match(&name) && !re_nonfoil_word().is_match(&name) {
        return true;
    }
    if finish.contains("foil") && !finish.contains("non") {
        return true;
    }
    false
}

fn unit_price_cents(item: &Value) -> Option<f64> {
    let cents = ["price_cents", "priceCents", "unit_price_cents"]
        .iter()
        .filter_map(|k| item.get(k))
        .find(|v| !v.is_null());
    if let Some(c) = cents {
        let n = js_number(Some(c));
        if n.is_finite() && n >= 0.0 {
            return Some(n.round());
        }
    }
    let dollars = ["price", "unitPrice", "pricePaid", "unit_price"]
        .iter()
        .filter_map(|k| item.get(k))
        .find(|v| !v.is_null());
    let d = match dollars {
        None => return None,
        Some(v) => {
            let d = js_number(Some(v));
            if !d.is_finite() || d < 0.0 {
                return None;
            }
            d
        }
    };
    Some(if d > 500.0 { d.round() } else { (d * 100.0).round() })
}

/// mapCondition(raw, fallback='Near Mint'): CONDITION_MAP exact, then
/// uppercased, else fallback. ''/null take the fallback directly.
pub fn map_condition(raw: Option<&Value>) -> String {
    map_condition_fb(raw, "Near Mint")
}

fn map_condition_fb(raw: Option<&Value>, fallback: &str) -> String {
    let is_blank = raw.map(|v| v.is_null() || v.as_str() == Some("")).unwrap_or(true);
    if is_blank {
        return fallback.to_string();
    }
    let s = js_trim(&js_to_string(raw)).to_string();
    const TABLE: &[(&str, &str)] = &[
        ("NM", "Near Mint"),
        ("LP", "Lightly Played"),
        ("MP", "Moderately Played"),
        ("HP", "Heavily Played"),
        ("DAM", "Damaged"),
        ("MT", "Damaged"),
        ("GP", "Lightly Played"),
        ("EXC", "Near Mint"),
        ("VG", "Moderately Played"),
        ("G", "Heavily Played"),
        ("P", "Damaged"),
        ("Near Mint", "Near Mint"),
        ("Lightly Played", "Lightly Played"),
        ("Moderately Played", "Moderately Played"),
        ("Heavily Played", "Heavily Played"),
        ("Damaged", "Damaged"),
    ];
    let lookup = |k: &str| -> Option<&'static str> {
        TABLE.iter().find(|(from, _)| *from == k).map(|(_, to)| *to)
    };
    lookup(&s)
        .or_else(|| lookup(&s.to_uppercase()))
        .unwrap_or(fallback)
        .to_string()
}

#[derive(Clone, Debug)]
pub struct OrderLine {
    pub name: String,
    pub set_code: Value,
    pub number: Value,
    pub scryfall_id: Value,
    pub tcgplayer_product_id: Value,
    pub quantity: f64,
    pub price_cents: Value,
    pub is_foil: bool,
    pub condition: String,
    pub year: Value,
    pub sealed: bool,
    pub junk: bool,
}

#[derive(Debug)]
pub struct OrderLines {
    pub lines: Vec<OrderLine>,
    pub card_copies: f64,
    pub card_lines: usize,
    /// `out.__extras` — every sealed and every junk line, counted whether the
    /// line was kept (includeExtras) or dropped. The preview/add totals read
    /// it as `Number(lines.__extras || 0)`.
    pub extras: usize,
}

impl OrderLine {
    /// The JS line object (key order of the literal; slice-2 parity uses it).
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), Value::String(self.name.clone()));
        m.insert("set_code".into(), self.set_code.clone());
        m.insert("number".into(), self.number.clone());
        m.insert("scryfall_id".into(), self.scryfall_id.clone());
        m.insert("tcgplayer_product_id".into(), self.tcgplayer_product_id.clone());
        m.insert("quantity".into(), js_num_json(self.quantity));
        m.insert("price_cents".into(), self.price_cents.clone());
        m.insert("is_foil".into(), json!(self.is_foil));
        m.insert("condition".into(), Value::String(self.condition.clone()));
        m.insert("year".into(), self.year.clone());
        m.insert("sealed".into(), json!(self.sealed));
        m.insert("junk".into(), json!(self.junk));
        Value::Object(m)
    }
}

/// orderLines twin (utils/marketplaceOrders.js): LINE_KEYS selection,
// per-item product flattening, sealed/junk classification, and the
    // __cardCopies / __cardLines fold the recent picker reads.
pub fn order_lines(raw: &Value, include_extras: bool) -> OrderLines {
    let src = if raw.is_object() || raw.is_array() { raw.clone() } else { Value::Null };
    let mut lines: Vec<Value> = Vec::new();
    for k in LINE_KEYS {
        if let Some(Value::Array(a)) = src.get(k) {
            if !a.is_empty() && a.iter().all(|x| x.is_object()) {
                lines = a.clone();
                break;
            }
        }
    }
    if lines.is_empty() {
        if let Some(Value::Array(a)) = src.get("orderItems") {
            lines = a.clone();
        }
    }
    if lines.is_empty() {
        lines = manapool_nested_items(&src);
    }
    let mut out: Vec<OrderLine> = Vec::new();
    let mut extras: usize = 0;
    for item in lines.iter() {
        let (prod, _ctx) = each_product_lines(item);
        let sealed_line =
            is_sealed_line(&prod, item) || is_manapool_sealed(item.get("product"));
        if sealed_line {
            extras += 1;
            if !include_extras {
                continue;
            }
        }
        let name = js_trim(
            &pick(Some(&prod), &["name", "cardName", "title"])
                .filter(|v| !js_falsy(Some(v)))
                .or_else(|| {
                    pick(Some(item), &["name", "productName", "description"])
                        .filter(|v| !js_falsy(Some(v)))
                })
                .map(|v| js_to_string(Some(&v)))
                .unwrap_or_default(),
        )
        .to_string();
        let set = pick(Some(&prod), &["set", "setCode", "set_code", "expansion", "expansionName"])
            .filter(|v| !js_falsy(Some(v)))
            .or_else(|| pick(Some(item), &["set", "setCode"]).filter(|v| !js_falsy(Some(v))));
        let number = pick(Some(&prod), &["number", "collectorNumber", "cardNumber", "collector_number"])
            .or_else(|| pick(Some(item), &["number", "collectorNumber"]));
        let qty = {
            let q = ["quantity", "qty", "count", "quantityFulfilled"]
                .iter()
                .filter_map(|k| item.get(k))
                .find(|v| !v.is_null())
                .cloned()
                .or_else(|| prod.get("quantity").filter(|v| !v.is_null()).cloned())
                .unwrap_or(json!(1));
            js_number(Some(&q))
        };
        let cents = unit_price_cents(item).or_else(|| unit_price_cents(&prod));
        let scryfall = pick(Some(&prod), &["scryfallId", "scryfall_id"])
            .filter(|v| !js_falsy(Some(v)))
            .or_else(|| pick(Some(item), &["scryfallId"]).filter(|v| !js_falsy(Some(v))));
        let tcg_raw = ["tcgplayerProductId", "tcgplayer_product_id", "product_id", "id"]
            .iter()
            .filter_map(|k| prod.get(k))
            .find(|v| !v.is_null())
            .cloned()
            .or_else(|| {
                ["tcgplayerProductId", "product_id"]
                    .iter()
                    .filter_map(|k| item.get(k))
                    .find(|v| !v.is_null())
                    .cloned()
            });
        let tcg_digits = tcg_raw
            .as_ref()
            .map(|v| re_digits().is_match(&js_to_string(Some(v))))
            .unwrap_or(false);
        let mut junk_line = false;
        let set_number = set.is_some() && number.is_some();
        if name.is_empty()
            && scryfall.is_none()
            && !set_number
            && !(tcg_raw.is_some() && tcg_digits)
        {
            junk_line = true;
            extras += 1;
            if !include_extras {
                continue;
            }
        }
        let quantity = if qty.is_finite() && qty > 0.0 { qty.floor() } else { 1.0 };
        let condition = {
            let raw_c = ["condition", "conditionCode", "grade", "conditionText", "condition_id"]
                .iter()
                .filter_map(|k| item.get(k))
                .find(|v| !v.is_null())
                .cloned()
                .or_else(|| {
                    ["condition", "conditionCode", "condition_id"]
                        .iter()
                        .filter_map(|k| prod.get(k))
                        .find(|v| !v.is_null())
                        .cloned()
                });
            map_condition(raw_c.as_ref())
        };
        let set_code = match set {
            Some(v) => {
                let t = js_trim(&js_to_string(Some(&v))).to_lowercase();
                Value::String(t)
            }
            None => Value::Null,
        };
        let number_json = match number {
            Some(v) if !(v.is_null() || v.as_str() == Some("")) => {
                Value::String(js_trim(&js_to_string(Some(&v))).to_string())
            }
            _ => Value::Null,
        };
        out.push(OrderLine {
            name,
            set_code,
            number: number_json,
            scryfall_id: scryfall.unwrap_or(Value::Null),
            tcgplayer_product_id: if tcg_raw.is_some() && tcg_digits {
                Value::String(js_to_string(tcg_raw.as_ref()))
            } else {
                Value::Null
            },
            quantity,
            price_cents: cents.map(js_num_json).unwrap_or(Value::Null),
            is_foil: is_foilish(item) || is_foilish(&prod),
            condition,
            year: pick(Some(&prod), &["year", "printed_year", "printedYear"])
                .unwrap_or(Value::Null),
            sealed: sealed_line,
            junk: junk_line,
        });
    }
    let card_copies = out.iter().fold(0.0f64, |n, l| {
        n + if l.sealed || l.junk {
            0.0
        } else if js_falsy(Some(&Value::from(l.quantity))) {
            1.0
        } else {
            l.quantity
        }
    });
    let card_lines = out.iter().filter(|l| !l.sealed && !l.junk).count();
    OrderLines { lines: out, card_copies, card_lines, extras }
}

fn stamp_of(row: &Value) -> String {
    // String(pick(o, [...]) || '')
    pick(Some(row), &["created_at", "createdAt", "created", "date", "placedAt", "orderDate"])
        .filter(|v| !js_falsy(Some(v)))
        .map(|v| js_to_string(Some(&v)))
        .unwrap_or_default()
}

fn order_number_matches(order: &Value, want: &str) -> bool {
    if want.is_empty() {
        return true;
    }
    let mut cands: Vec<String> = Vec::new();
    for k in [
        "number",
        "orderNumber",
        "order_number",
        "friendly_id",
        "friendlyId",
        "name",
        "id",
        "orderId",
        "order_id",
        "key",
        "reference",
    ] {
        if let Some(v) = order.get(k) {
            if !v.is_null() {
                cands.push(js_to_string(Some(v)));
            }
        }
    }
    let norm_raw = want.strip_prefix('#').unwrap_or(want);
    let norm = js_trim(norm_raw).to_lowercase();
    cands.iter().map(|c| c.to_lowercase()).any(|c| {
        c == norm || c == format!("#{norm}") || c == format!("order-{norm}")
    })
}

fn body_keys(body: &Value) -> Vec<Value> {
    if !body.is_object() && !body.is_array() {
        return Vec::new();
    }
    // Object.keys order: insertion for parsed JSON (serde_json preserves it
    // with preserve_order; without it, key order only decorates errors).
    let keys: Vec<Value> = match body {
        Value::Object(m) => m.keys().map(|k| Value::String(k.clone())).collect(),
        Value::Array(a) => (0..a.len()).map(|i| Value::String(i.to_string())).collect(),
        _ => return Vec::new(),
    };
    if keys.len() <= 25 {
        keys
    } else {
        let mut out = keys[..25].to_vec();
        out.push(Value::String(format!("(+{} more)", keys.len() - 25)));
        out
    }
}

// ---------------------------------------------------------------------------
// ManaPool client folds
// ---------------------------------------------------------------------------

pub struct Creds {
    pub email: String,
    pub token: String,
}

/// mpGuard twin: auth/rate rejections are AUTH problems; otherwise returns
/// the 2xx verdict (the `what` parameter is unused in Node today).
fn mp_guard(status: u16) -> Result<bool, MpError> {
    if status == 401 || status == 403 {
        return Err(MpError::err(401, MP_UNAUTH_MESSAGE, false));
    }
    if status == 429 {
        return Err(MpError::err(429, MP_RATE_LIMIT_MESSAGE, false));
    }
    Ok((200..=299).contains(&status))
}

/// fetchManapoolOrderList twin. NOTE: the live API ignores paging, so the
/// URL carries NO query params and all sorting/slicing lives in the caller.
pub async fn fetch_manapool_order_list(
    http: &dyn MpHttp,
    creds: &Creds,
) -> Result<Vec<Value>, MpError> {
    if creds.email.is_empty() || creds.token.is_empty() {
        return Err(MpError::err(400, MP_CRED_UNSET_MESSAGE, false));
    }
    let url = format!("{}{}", manapool_base(), MANAPOOL_LIST_PATH);
    let r = http.get(url, mp_headers(creds)).await?;
    if !mp_guard(r.status)? {
        return Err(MpError {
            status: Some(502),
            message: format!("ManaPool did not return an order list (status {}).", r.status),
            list_unavailable: true,
            observed_keys: None,
        });
    }
    Ok(order_array(&r.body))
}

/// fetchManapoolOrder twin: a human number resolves through the list first
/// (the detail route is UUID-keyed); the detail GET then rides through any
/// remaining status for the caller to interpret.
pub async fn fetch_manapool_order(
    http: &dyn MpHttp,
    creds: &Creds,
    order_number: &str,
) -> Result<MpResponse, MpError> {
    if creds.email.is_empty() || creds.token.is_empty() {
        return Err(MpError::err(400, MP_CRED_UNSET_MESSAGE, false));
    }
    let num = js_trim(order_number).to_string();
    if !order_number_shape_ok(&num) {
        return Err(MpError::err(400, "Order number is not valid", false));
    }
    let headers = mp_headers(creds);
    let mut id = num.clone();
    if !mp_uuid(&num) {
        let rows = fetch_manapool_order_list(http, creds).await.unwrap_or_default();
        if let Some(hit) = rows.iter().find(|o| order_number_matches(o, &num)) {
            if let Some(cand) = pick(Some(hit), &["id", "order_id", "orderId", "uuid"]) {
                id = js_to_string(Some(&cand));
            }
        }
    }
    let url = format!("{}/buyer/orders/{}", manapool_base(), encode_uri_component(&id));
    let r = http.get(url, headers).await?;
    let is2xx = mp_guard(r.status)?;
    if !is2xx {
        let status = if r.status == 404 || r.status == 400 { 404 } else { 502 };
        return Err(MpError {
            status: Some(status),
            message: format!("ManaPool has no order {num} for this account. Check the number on ManaPool \u{2192} account \u{2192} Orders."),
            list_unavailable: false,
            observed_keys: Some(body_keys(&r.body)),
        });
    }
    Ok(r)
}

/// manapoolRecentOrderSummaries twin: whole-list fetch, filter rows with a
/// non-null number, sort NEWEST-first by stampOf (localeCompare, stable),
/// slice from the offset, then upgrade each shown row's counts from the
/// UUID-keyed detail (a detail failure keeps the list numbers).
pub async fn manapool_recent_order_summaries(
    http: &dyn MpHttp,
    creds: &Creds,
    limit: usize,
    offset: f64,
) -> Result<(Vec<Value>, Option<i64>), MpError> {
    let rows = fetch_manapool_order_list(http, creds).await?;
    let mut numbered: Vec<Value> = rows
        .into_iter()
        .filter(|r| pick(Some(r), &["order_number", "orderNumber", "number", "id"]).is_some())
        .collect();
    numbered.sort_by(|a, b| root_collator().compare(&stamp_of(b), &stamp_of(a)));
    let total = Some(numbered.len() as i64);
    let start = {
        let n = if js_falsy(Some(&Value::from(offset))) { 0.0 } else { offset };
        (n.floor().max(0.0)) as usize
    };
    let mut out: Vec<Value> = Vec::new();
    for row in numbered.into_iter().skip(start) {
        if out.len() >= limit {
            break;
        }
        let number = match pick(Some(&row), &["order_number", "orderNumber", "number", "id"]) {
            Some(v) => v,
            None => continue,
        };
        let stamp = stamp_of(&row);
        let mut summary = Map::new();
        summary.insert("number".into(), Value::String(js_to_string(Some(&number))));
        summary.insert(
            "placedAt".into(),
            if stamp.is_empty() { Value::Null } else { Value::String(stamp) },
        );
        summary.insert(
            "status".into(),
            pick(Some(&row), &["status", "orderStatus", "fulfillmentStatus", "fulfillment_status"])
                .filter(|v| !js_falsy(Some(v)))
                .unwrap_or(Value::Null),
        );
        summary.insert("cardCount".into(), js_num_json(sum_item_counts(&row)));
        summary.insert("lineCount".into(), json!(0));
        summary.insert(
            "total_cents".into(),
            pick(Some(&row), &["total_cents", "totalCents", "total", "orderTotal"])
                .unwrap_or(Value::Null),
        );
        if let Some(id) = pick(Some(&row), &["id", "order_id", "orderId", "uuid"]) {
            let id_s = js_to_string(Some(&id));
            if mp_uuid(&id_s) {
                match fetch_manapool_order(http, creds, &id_s).await {
                    Ok(detail) => {
                        if let Some(order) = unwrap_manapool_order(&detail.body) {
                            let lines = order_lines(order, false);
                            summary.insert("cardCount".into(), js_num_json(lines.card_copies));
                            summary.insert("lineCount".into(), json!(lines.card_lines));
                        }
                    }
                    Err(_) => {} // keep the list's own count
                }
            }
        }
        out.push(Value::Object(summary));
    }
    Ok((out, total))
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

fn load_row(
    conn: &rusqlite::Connection,
    user_id: i64,
) -> Result<Option<Map<String, Value>>, rusqlite::Error> {
    let rows = rows_map(
        conn,
        "SELECT manapool_email, manapool_token, manapool_enabled, manapool_saved_at FROM users WHERE id = ?",
        &[SqlValue::Integer(user_id)],
    )?;
    Ok(rows.into_iter().next())
}

fn account_view(row: &Map<String, Value>) -> Value {
    // Key order is the Node accountView literal's — compared byte-wise in
    // parity, so it is load-bearing.
    let mut mp = Map::new();
    mp.insert(
        "configured".into(),
        json!(!js_falsy(row.get("manapool_email")) && !js_falsy(row.get("manapool_token"))),
    );
    mp.insert("enabled".into(), json!(!js_falsy(row.get("manapool_enabled"))));
    mp.insert("email".into(), json!(mask_email(row.get("manapool_email"))));
    mp.insert("token".into(), json!(mask_secret(row.get("manapool_token"))));
    let saved = row.get("manapool_saved_at");
    mp.insert(
        "savedAt".into(),
        match saved {
            Some(v) if !js_falsy(Some(v)) => v.clone(),
            _ => Value::Null,
        },
    );
    let mut root = Map::new();
    root.insert("manapool".into(), Value::Object(mp));
    Value::Object(root)
}

pub async fn route_accounts(State(st): State<AppState>, req: Request) -> Response {
    match *req.method() {
        Method::GET => handle_accounts_get(State(st), req).await,
        Method::PUT => handle_accounts_put(State(st), req).await,
        _ => crate::proxy(State(st), req).await,
    }
}

pub async fn route_recent(
    State(st): State<AppState>,
    Path(source): Path<String>,
    req: Request,
) -> Response {
    if *req.method() == Method::GET {
        handle_recent(State(st), source, req).await
    } else {
        crate::proxy(State(st), req).await
    }
}

async fn handle_accounts_get(State(st): State<AppState>, req: Request) -> Response {
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let headers = req.headers().clone();
    let user = match authenticate(&pool, &headers, true) {
        Ok(u) => u,
        Err(res) => return res,
    };
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error=?e, "marketplace read pool");
            return internal_error();
        }
    };
    match load_row(&conn, user.id) {
        Ok(Some(row)) => json_response(StatusCode::OK, &account_view(&row)),
        Ok(None) => json_error(StatusCode::NOT_FOUND, "User not found"),
        Err(e) => {
            tracing::error!(error=?e, "marketplace accounts load");
            internal_error()
        }
    }
}

async fn handle_accounts_put(State(st): State<AppState>, req: Request) -> Response {
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    // express.json sits above the router: malformed JSON answers the generic
    // 500 and oversize the 413 BEFORE auth (collection_writes convention).
    let (parts, bytes) = read_parts(req).await;
    let headers = parts.headers.clone();
    let body = match parse_body(&headers, &bytes) {
        BodyOutcome::Parsed(b) => b,
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::TooLarge => return too_large(),
    };
    let user = match authenticate(&pool, &headers, false) {
        Ok(u) => u,
        Err(res) => return res,
    };
    // `const { manapool } = req.body || {}` — key absent is the 400 branch;
    // an explicit null RUNS the clear arm.
    let mp = match body.get("manapool") {
        Some(v) => v.clone(),
        None => return json_error(StatusCode::BAD_REQUEST, "Nothing to save"),
    };
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error=?e, "marketplace write pool");
            return internal_error();
        }
    };
    let row = match load_row(&conn, user.id) {
        Ok(Some(r)) => r,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "User not found"),
        Err(e) => {
            tracing::error!(error=?e, "marketplace accounts load");
            return internal_error();
        }
    };

    let mut sets: Vec<String> = Vec::new();
    let mut params: Vec<SqlValue> = Vec::new();
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    // `manapool === null || manapool.clear` — property reads on primitives
    // answer undefined (Value::get -> None).
    if mp.is_null() || !js_falsy(mp.get("clear")) {
        sets.push("manapool_email = NULL".into());
        sets.push("manapool_token = NULL".into());
        sets.push("manapool_enabled = 0".into());
        sets.push("manapool_saved_at = NULL".into());
    } else {
        let email_s = to_str_or_empty(mp.get("email"));
        let token_s = to_str_or_empty(mp.get("token"));
        let email = js_trim(&email_s).to_string();
        let token = js_trim(&token_s).to_string();
        let row_configured =
            !js_falsy(row.get("manapool_email")) && !js_falsy(row.get("manapool_token"));
        let enabled_off = matches!(mp.get("enabled"), Some(Value::Bool(false)));
        if email.is_empty() && token.is_empty() && row_configured {
            // Enabled-only: keep the credential and its savedAt stamp.
            sets.push("manapool_enabled = ?".into());
            params.push(SqlValue::Integer(if enabled_off { 0 } else { 1 }));
        } else if !email_valid(&email) {
            return json_error(StatusCode::BAD_REQUEST, "ManaPool email is not valid");
        } else if token.is_empty() || js_len(&token) > 4096 {
            return json_error(StatusCode::BAD_REQUEST, "A ManaPool access token is required");
        } else {
            sets.push("manapool_email = ?".into());
            sets.push("manapool_token = ?".into());
            sets.push("manapool_enabled = ?".into());
            sets.push("manapool_saved_at = ?".into());
            params.push(SqlValue::Text(email));
            params.push(SqlValue::Text(token));
            params.push(SqlValue::Integer(if enabled_off { 0 } else { 1 }));
            params.push(SqlValue::Text(now));
        }
    }

    // Node awaits a single db.run; every branch above pushed at least one
    // SET, so the statement can never be empty.
    let sql = format!("UPDATE users SET {} WHERE id = ?", sets.join(", "));
    let mut all = params;
    all.push(SqlValue::Integer(user.id));
    if let Err(e) = conn.execute(&sql, rusqlite::params_from_iter(all.iter())) {
        tracing::error!(error=?e, "marketplace accounts update failed");
        return internal_error();
    }
    let fresh = match load_row(&conn, user.id) {
        Ok(Some(r)) => r,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "User not found"),
        Err(e) => {
            tracing::error!(error=?e, "marketplace accounts reload");
            return internal_error();
        }
    };
    let mut out = Map::new();
    out.insert("ok".into(), json!(true));
    out.insert("accounts".into(), account_view(&fresh));
    json_response(StatusCode::OK, &Value::Object(out))
}

async fn handle_recent(State(st): State<AppState>, source: String, req: Request) -> Response {
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let headers = req.headers().clone();
    let query = req.uri().query().unwrap_or("").to_string();
    // authenticateToken is the router's middleware: it answers BEFORE the
    // source gate.
    let user = match authenticate(&pool, &headers, true) {
        Ok(u) => u,
        Err(res) => return res,
    };
    let source = source.to_lowercase();
    if source != "manapool" {
        return json_error(StatusCode::BAD_REQUEST, "unknown source");
    }
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error=?e, "marketplace read pool");
            return internal_error();
        }
    };
    let row = match load_row(&conn, user.id) {
        Ok(Some(r)) => r,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "User not found"),
        Err(e) => {
            tracing::error!(error=?e, "marketplace recent load");
            return internal_error();
        }
    };
    if js_falsy(row.get("manapool_email"))
        || js_falsy(row.get("manapool_token"))
        || js_falsy(row.get("manapool_enabled"))
    {
        return json_error(
            StatusCode::BAD_REQUEST,
            "ManaPool is not configured or is turned off",
        );
    }
    // `Math.max(0, parseInt(req.query.offset, 10) || 0)` — first query value;
    // a repeated key joins with ',' in qs, which parseInt reads as the first
    // number (or NaN) either way.
    let offset_raw = form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "offset")
        .map(|(_, v)| v.into_owned());
    let offset = offset_raw.as_deref().and_then(js_parse_int).unwrap_or(0).max(0);

    let creds = Creds {
        email: to_str_or_empty(row.get("manapool_email")),
        token: to_str_or_empty(row.get("manapool_token")),
    };
    let http = st.mp_http.clone();
    match manapool_recent_order_summaries(http.as_ref(), &creds, RECENT_LIMIT, offset as f64).await
    {
        Ok((orders, total)) => {
            let mut out = Map::new();
            out.insert("source".into(), json!(source));
            out.insert("orders".into(), Value::Array(orders));
            out.insert("offset".into(), json!(offset));
            out.insert("total".into(), match total {
                Some(t) => json!(t),
                None => Value::Null,
            });
            let has_more = match total {
                Some(t) => offset + (RECENT_LIMIT as i64) < t,
                None => false,
            };
            out.insert("hasMore".into(), json!(has_more));
            json_response(StatusCode::OK, &Value::Object(out))
        }
        Err(e) => {
            let status = match e.status {
                Some(s) if s != 0 => s,
                _ => 502,
            };
            let list_unavailable = e.list_unavailable || status == 502;
            let message = if e.message.is_empty() {
                "Recent orders could not be read".to_string()
            } else {
                e.message
            };
            let mut out = Map::new();
            out.insert("error".into(), Value::String(message));
            out.insert("list_unavailable".into(), json!(list_unavailable));
            json_response(
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                &Value::Object(out),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Slice 2 — POST /api/marketplace/preview and POST /api/marketplace/add
// (routes/marketplaceOrders.js orderFromRequest/preview/add + the
// utils/marketplaceOrders.js parseOrderPayload / previewOrder /
// addOrderToCollection twins).
//
// Middleware order mirrors slice 1: express.json answers badjson 500 /
// oversize 413 BEFORE auth; authenticate (API keys GET-only → 403 here)
// then the handler arms. Node's global invalidateOnWrite bumps the stats
// generation for every authenticated non-GET after auth, so the twin calls
// stats::on_proxied_write once auth passed — before the arm checks, exactly
// like the middleware, and regardless of how the handler answers.
//
// Provider legs: the Scryfall bulk resolver, cacheCards and plan_add
// `Forward` all replay the WHOLE untouched POST to Node while nothing local
// has been written (precons/secret-lair policy) — Node owns the axios
// message shapes and the mid-add hydration path.
// ---------------------------------------------------------------------------

const MP_SOURCES: [&str; 1] = ["manapool"];
/// utils VALID_CONDITIONS — the add path's OWN grade whitelist (the per-line
/// grade must pass it too, else the order-level default wins).
const MP_VALID_CONDITIONS: [&str; 5] = [
    "Near Mint",
    "Lightly Played",
    "Moderately Played",
    "Heavily Played",
    "Damaged",
];
const MP_NO_USABLE_LINES_MSG: &str =
    "The order response did not include any usable line items.";
const MP_NOT_CONNECTED_MSG: &str = "ManaPool is not configured or is turned off";

/// parseOrderPayload output (key order = the Node object literal).
#[derive(Debug)]
pub struct ParsedOrder {
    pub number: Value,
    pub placed_at: Value,
    pub status: Value,
    pub currency: Value,
    pub total_cents: Value,
    pub lines: OrderLines,
}

impl ParsedOrder {
    /// The `{ ...out.parsed }` half of the preview response (and the
    /// `order:{number,lineCount}` pick for add).
    fn to_json(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("number".into(), self.number.clone());
        m.insert("placedAt".into(), self.placed_at.clone());
        m.insert("status".into(), self.status.clone());
        m.insert("currency".into(), self.currency.clone());
        m.insert("total_cents".into(), self.total_cents.clone());
        m.insert(
            "lines".into(),
            Value::Array(self.lines.lines.iter().map(|l| l.to_json()).collect()),
        );
        m.insert("lineCount".into(), json!(self.lines.lines.len() as i64));
        m
    }
}

/// parseOrderPayload twin. Error = the 422 'no usable line items' throw
/// (status + observedKeys ride on MpError exactly like the unit tests read
/// err.status / err.observedKeys).
pub fn parse_order_payload(
    body: &Value,
    want_number: &str,
    include_extras: bool,
) -> Result<ParsedOrder, MpError> {
    let list = order_array(body);
    // direct: a plain object carrying a NON-EMPTY line array under any
    // LINE_KEYS name (no every-object gate here — orderLines applies that).
    let direct = body.is_object()
        && LINE_KEYS.iter().any(|k| matches!(body.get(k), Some(Value::Array(a)) if !a.is_empty()));
    let mut order: Option<Value> = None;
    if direct {
        order = Some(body.clone());
    } else if body.is_object() {
        // body.order && typeof body.order === 'object' — arrays are objects.
        if let Some(o) = body.get("order") {
            if !o.is_null() && (o.is_object() || o.is_array()) {
                order = Some(o.clone());
            }
        }
    }
    if order.is_none() && !list.is_empty() {
        let num = js_trim(want_number);
        order = list
            .iter()
            .find(|o| order_number_matches(o, num))
            .cloned()
            .or_else(|| if num.is_empty() { list.first().cloned() } else { None });
    }
    let Some(order) = order else {
        return Err(MpError {
            status: Some(422),
            message: MP_NO_USABLE_LINES_MSG.to_string(),
            list_unavailable: false,
            observed_keys: Some(body_keys(body)),
        });
    };
    let lines = order_lines(&order, include_extras);
    // number: String(pick(order, [...]) ?? wantNumber ?? '').trim() || null
    let number_s = js_trim(
        &pick(Some(&order), &["number", "orderNumber", "order_number", "id", "orderId"])
            .map(|v| js_to_string(Some(&v)))
            .unwrap_or_else(|| want_number.to_string()),
    )
    .to_string();
    let falsy_pick = |names: &[&str]| -> Value {
        pick(Some(&order), names).filter(|v| !js_falsy(Some(v))).unwrap_or(Value::Null)
    };
    let placed_at =
        falsy_pick(&["created", "createdAt", "created_at", "date", "placedAt", "orderDate"]);
    let status =
        falsy_pick(&["status", "orderStatus", "fulfillmentStatus", "fulfillment_status"]);
    let currency = falsy_pick(&["currency", "currencySymbol"]);
    let total_cents = pick(Some(&order), &["total_cents", "totalCents", "total", "orderTotal"])
        .or_else(|| {
            order
                .get("payment")
                .and_then(|p| pick(Some(p), &["total_cents", "subtotal_cents"]))
        })
        .unwrap_or(Value::Null);
    Ok(ParsedOrder {
        number: if number_s.is_empty() { Value::Null } else { Value::String(number_s) },
        placed_at,
        status,
        currency,
        total_cents,
        lines,
    })
}

/// The card-line uniq fold shared by previewOrder and addOrderToCollection:
/// sealed/junk lines never enter, first cardKey wins, dups ADD quantity.
/// The add path's key builder calls `.toLowerCase()` directly on a truthy
/// scryfall_id — a NUMBER id is a V8 TypeError in Node (500 through the
/// route catch); the message is returned in `Err` so the route can replay
/// byte-exactly. previewOrder wraps String() first and never hits it.
fn build_uniq(lines: &OrderLines, add_path: bool) -> Result<Vec<OrderLine>, String> {
    let mut uniq: Vec<OrderLine> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    for l in &lines.lines {
        if l.sealed || l.junk {
            continue;
        }
        let key = if !js_falsy(Some(&l.scryfall_id)) {
            if add_path && !l.scryfall_id.is_string() {
                return Err("l.scryfall_id.toLowerCase is not a function".to_string());
            }
            format!("i:{}", js_to_string(Some(&l.scryfall_id)).to_lowercase())
        } else {
            format!(
                "n:{}|{}|{}|{}",
                l.name.to_lowercase(),
                to_str_or_empty(Some(&l.set_code)).to_lowercase(),
                to_str_or_empty(Some(&l.number)).to_lowercase(),
                if l.is_foil { "f" } else { "n" }
            )
        };
        match keys.iter().position(|k| *k == key) {
            Some(i) => uniq[i].quantity += l.quantity,
            None => {
                keys.push(key);
                uniq.push(l.clone());
            }
        }
    }
    Ok(uniq)
}

/// The bulkFetchByIdentifier request rows (the _line back-pointer stays in
/// Rust as the uniq index the resolver echoes back through pairs.0).
fn bulk_rows(uniq: &[OrderLine]) -> Vec<Map<String, Value>> {
    uniq.iter()
        .map(|l| {
            let mut m = Map::new();
            if !js_falsy(Some(&l.scryfall_id)) {
                m.insert("id".into(), l.scryfall_id.clone());
            }
            if !js_falsy(Some(&l.set_code)) {
                m.insert("set_id".into(), l.set_code.clone());
            }
            if !js_falsy(Some(&l.number)) {
                m.insert("number".into(), l.number.clone());
            }
            if !l.name.is_empty() {
                m.insert("name".into(), Value::String(l.name.clone()));
            }
            m
        })
        .collect()
}

fn blank_line() -> OrderLine {
    // `(p.row && p.row._line) || {}` — unreachable here (pairs indexes always
    // hit uniq) but the fold shape must not panic if the resolver drifts.
    OrderLine {
        name: String::new(),
        set_code: Value::Null,
        number: Value::Null,
        scryfall_id: Value::Null,
        tcgplayer_product_id: Value::Null,
        quantity: 0.0,
        price_cents: Value::Null,
        is_foil: false,
        condition: String::new(),
        year: Value::Null,
        sealed: false,
        junk: false,
    }
}

fn js_num_truthy(x: f64) -> Value {
    if js_falsy(Some(&Value::from(x))) {
        js_num_json(1.0)
    } else {
        js_num_json(x)
    }
}

/// JS `v || undefined` on a Value: falsy (null, '', 0, false) reads nothing.
fn val_or_none(v: &Value) -> Option<Value> {
    if js_falsy(Some(v)) {
        None
    } else {
        Some(v.clone())
    }
}

/// previewOrder twin. Err(()) = replay the whole POST to Node (scryfall
/// client failure, cacheCards DB failure, or the owned-counts DB failure).
/// The connection is opened only AFTER the scryfall await (rusqlite guards
/// are not Send — same discipline the slice-1 handlers follow).
async fn preview_fold(
    st: &AppState,
    pool: &DbPool,
    user_id: i64,
    lines: &OrderLines,
    include_extras: bool,
) -> Result<Map<String, Value>, ()> {
    let mut m = Map::new();
    if lines.lines.is_empty() {
        // Node's early answer — deliberately MISSING totalCopies,
        // extrasIncluded and price_cents (the object literal stops at
        // sealedCount; the route spread never adds them back).
        m.insert("cards".into(), json!([]));
        m.insert("totalListed".into(), json!(0));
        m.insert("resolvedCount".into(), json!(0));
        m.insert("unresolved".into(), json!(0));
        m.insert("extras".into(), json!(0));
        m.insert("sealedCount".into(), json!(0));
        return Ok(m);
    }
    let uniq = build_uniq(lines, false).map_err(|_| ())?;
    let rows = bulk_rows(&uniq);
    let bulk = if rows.is_empty() {
        crate::precons::BulkOut { cards: Vec::new(), pairs: Vec::new(), not_found: 0 }
    } else {
        match bulk_fetch_by_identifier(st, &rows).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("marketplace preview: scryfall bulk fetch failed ({e}); forwarding to Node");
                return Err(());
            }
        }
    };
    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => {
            tracing::error!("marketplace preview: cannot open connection");
            return Err(());
        }
    };
    if !bulk.cards.is_empty() {
        let fresh: Vec<Map<String, Value>> =
            bulk.cards.iter().filter_map(|c| c.as_object().cloned()).collect();
        if let Err(e) = cache_normalized_cards(&conn, &fresh) {
            tracing::error!("marketplace preview: cacheCards failed: {e}; forwarding to Node");
            return Err(());
        }
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut resolved: Vec<Map<String, Value>> = Vec::new();
    for (idx, card) in &bulk.pairs {
        let idv = card.get("id");
        // if (!card || !card.id || seen.has(card.id)) continue
        if idv.map(|v| js_falsy(Some(v))).unwrap_or(true) {
            continue;
        }
        if !seen.insert(js_to_string(idv)) {
            continue;
        }
        let def = blank_line();
        let line = uniq.get(*idx).unwrap_or(&def);
        let mut r = Map::new();
        r.insert("card_id".into(), idv.cloned().unwrap_or(Value::Null));
        r.insert(
            "name".into(),
            card.get("name")
                .filter(|v| !js_falsy(Some(v)))
                .cloned()
                .or_else(|| {
                    if line.name.is_empty() {
                        None
                    } else {
                        Some(Value::String(line.name.clone()))
                    }
                })
                .unwrap_or_else(|| Value::String(String::new())),
        );
        r.insert(
            "image_url".into(),
            card.get("image_url").filter(|v| !js_falsy(Some(v))).cloned().unwrap_or(Value::Null),
        );
        r.insert(
            "set_code".into(),
            card
                .get("set_id")
                .and_then(val_or_none)
                .or_else(|| val_or_none(&line.set_code))
                .unwrap_or(Value::Null),
        );
        r.insert(
            "number".into(),
            match card.get("number") {
                Some(v) if !v.is_null() => Value::String(js_to_string(Some(v))),
                _ => val_or_none(&line.number).unwrap_or(Value::Null),
            },
        );
        r.insert("quantity".into(), js_num_truthy(line.quantity));
        r.insert("is_foil".into(), json!(line.is_foil));
        r.insert(
            "condition".into(),
            if line.condition.is_empty() {
                Value::Null
            } else {
                Value::String(line.condition.clone())
            },
        );
        r.insert(
            "price_cents".into(),
            if line.price_cents.is_null() { Value::Null } else { line.price_cents.clone() },
        );
        r.insert("matched".into(), json!(true));
        resolved.push(r);
    }
    let resolved_count = resolved.len();
    // ownedCountForCards: SUM(quantity) grouped per card_id, THIS user only;
    // case-insensitive key Map, Number(qty) || 0.
    if user_id != 0 && !resolved.is_empty() {
        let ids: Vec<String> = resolved
            .iter()
            .map(|r| r.get("card_id").cloned().unwrap_or(Value::Null))
            .filter(|v| !js_falsy(Some(v)))
            .map(|v| js_to_string(Some(&v)))
            .collect();
        if !ids.is_empty() {
            let placeholders: Vec<String> =
                (1..=ids.len()).map(|i| format!("?{}", i + 1)).collect();
            let sql = format!(
                "SELECT card_id, SUM(quantity) AS qty FROM collection WHERE user_id = ? AND card_id IN ({}) GROUP BY card_id",
                placeholders.join(",")
            );
            let mut params: Vec<SqlValue> = vec![SqlValue::Integer(user_id)];
            params.extend(ids.iter().map(|s| SqlValue::Text(s.clone())));
            let owned_rows = match rows_map(&conn, &sql, &params) {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::error!("marketplace preview: owned-count query failed: {e}; forwarding to Node");
                    return Err(());
                }
            };
            let mut owned: Vec<(String, f64)> = Vec::new();
            for r in owned_rows {
                let key = js_to_string(r.get("card_id")).to_lowercase();
                let q = number_or_zero(r.get("qty"));
                match owned.iter_mut().find(|(k, _)| *k == key) {
                    Some(slot) => slot.1 = q,
                    None => owned.push((key, q)),
                }
            }
            for r in resolved.iter_mut() {
                let key = js_to_string(r.get("card_id")).to_lowercase();
                let o = owned.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).unwrap_or(0.0);
                r.insert("owned".into(), js_num_json(o));
            }
        }
    }
    let extras_held = lines.extras as f64;
    let total_copies: f64 =
        uniq.iter().fold(0.0, |n, l| n + (if js_falsy(Some(&Value::from(l.quantity))) { 1.0 } else { l.quantity }));
    let cents: f64 = lines.lines.iter().fold(0.0, |n, l| {
        if l.price_cents.is_null() {
            n
        } else {
            n + js_number(Some(&l.price_cents))
                * (if js_falsy(Some(&Value::from(l.quantity))) { 1.0 } else { l.quantity })
        }
    });
    m.insert(
        "cards".into(),
        Value::Array(resolved.into_iter().map(Value::Object).collect()),
    );
    m.insert("totalListed".into(), js_num_json(uniq.len() as f64 + extras_held));
    m.insert("totalCopies".into(), js_num_json(total_copies));
    m.insert("resolvedCount".into(), json!(resolved_count as i64));
    m.insert("unresolved".into(), json!(bulk.not_found));
    m.insert("extras".into(), js_num_json(extras_held));
    m.insert("sealedCount".into(), js_num_json(extras_held));
    m.insert("extrasIncluded".into(), json!(include_extras));
    m.insert(
        "price_cents".into(),
        if cents == 0.0 || cents.is_nan() { Value::Null } else { js_num_json(cents) },
    );
    Ok(m)
}

/// Number((Number(cents)/100).toFixed(2)) — toFixed rounds HALF-AWAY-FROM-
/// ZERO on the exact double value; {:.2} is banker's rounding and would
/// diverge on exact x.xx5 doubles (which DO occur: 12.5 cents = 0.125).
pub fn js_tofixed2_number(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x >= 0.0 {
        (x * 100.0 + 0.5).floor() / 100.0
    } else {
        -((-x * 100.0 + 0.5).floor() / 100.0)
    }
}

#[derive(Clone)]
struct AddPlanEntry {
    card_id: Value,
    quantity: f64,
    printing: String,
    purchase_price: Option<f64>,
    condition: String,
}

struct AddResult {
    added: f64,
    failed: Vec<Value>,
    resolved: usize,
    total_listed: f64,
    unresolved: i64,
}

enum AddErr {
    /// Whole-request replay (scryfall leg, cache DB failure, plan Forward).
    Replay,
    /// addOrderToCollection threw — route answers status||500 with this
    /// message (the numeric-scryfall_id TypeError today).
    Boom(String),
}

/// addOrderToCollection twin. Groups are filed through the shared
/// plan_add/execute_add pair exactly like the Secret Lair path, with the
/// marketplace per-card fields (quantity, purchase_price, condition,
/// printing) overriding the shared defaults; `added` counts COPIES via the
/// per-card qty map, not rows.
async fn add_fold(
    st: &AppState,
    pool: &DbPool,
    user_id: i64,
    lines: &OrderLines,
    body: &Map<String, Value>,
) -> Result<AddResult, AddErr> {
    if lines.lines.is_empty() {
        // { added: 0, failed: [], resolved: 0, totalListed: 0, unresolved: 0 }
        return Ok(AddResult {
            added: 0.0,
            failed: Vec::new(),
            resolved: 0,
            total_listed: 0.0,
            unresolved: 0,
        });
    }
    let cond = match body.get("condition") {
        Some(Value::String(s)) if MP_VALID_CONDITIONS.contains(&s.as_str()) => s.clone(),
        _ => "Near Mint".to_string(),
    };
    let copies_raw = js_number(body.get("copies"));
    // Number(copies) || 1 — NaN/0 fall to 1, Infinity stays Infinity and
    // clamps to 500 through Math.min (is_finite would wrongly clamp it to 1).
    let copies_base = if copies_raw.is_nan() || copies_raw == 0.0 { 1.0 } else { copies_raw };
    let mult = copies_base.min(500.0).max(1.0);
    let uniq = build_uniq(lines, true).map_err(AddErr::Boom)?;
    let rows = bulk_rows(&uniq);
    let bulk = if rows.is_empty() {
        crate::precons::BulkOut { cards: Vec::new(), pairs: Vec::new(), not_found: 0 }
    } else {
        match bulk_fetch_by_identifier(st, &rows).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("marketplace add: scryfall bulk fetch failed ({e}); forwarding to Node");
                return Err(AddErr::Replay);
            }
        }
    };
    // rusqlite guards are not Send: open only after the last await point.
    let conn = match pool.get() {
        Ok(c) => c,
        Err(_) => {
            tracing::error!("marketplace add: cannot open connection");
            return Err(AddErr::Replay);
        }
    };
    if !bulk.cards.is_empty() {
        let fresh: Vec<Map<String, Value>> =
            bulk.cards.iter().filter_map(|c| c.as_object().cloned()).collect();
        if let Err(e) = cache_normalized_cards(&conn, &fresh) {
            tracing::error!("marketplace add: cacheCards failed: {e}; forwarding to Node");
            return Err(AddErr::Replay);
        }
    }
    let mode = match body.get("printing_mode") {
        Some(Value::String(s)) if s == "foil" => "foil",
        Some(Value::String(s)) if s == "nonfoil" => "nonfoil",
        _ => "auto",
    };
    let language = body.get("language").cloned().unwrap_or_else(|| json!("English"));

    let mut seen: HashSet<String> = HashSet::new();
    let mut plan: Vec<AddPlanEntry> = Vec::new();
    for (idx, card) in &bulk.pairs {
        let idv = card.get("id");
        if idv.map(|v| js_falsy(Some(v))).unwrap_or(true) {
            continue;
        }
        if !seen.insert(js_to_string(idv)) {
            continue;
        }
        let def = blank_line();
        let line = uniq.get(*idx).unwrap_or(&def);
        let base = if js_falsy(Some(&Value::from(line.quantity))) { 1.0 } else { line.quantity };
        let quantity = (base * mult).round().max(1.0);
        let printing = match mode {
            "foil" => "Holofoil",
            "nonfoil" => "Normal",
            _ => {
                if line.is_foil {
                    "Holofoil"
                } else {
                    "Normal"
                }
            }
        }
        .to_string();
        let purchase_price = if line.price_cents.is_null() {
            None
        } else {
            Some(js_tofixed2_number(js_number(Some(&line.price_cents)) / 100.0))
        };
        let condition = if !line.condition.is_empty()
            && MP_VALID_CONDITIONS.contains(&line.condition.as_str())
        {
            line.condition.clone()
        } else {
            cond.clone()
        };
        plan.push(AddPlanEntry {
            card_id: idv.cloned().unwrap_or(Value::Null),
            quantity,
            printing,
            purchase_price,
            condition,
        });
    }
    // qtyById: Map keyed by card_id, quantity || 1 — `added` counts COPIES.
    let qty_by_id: Vec<(String, f64)> = plan
        .iter()
        .map(|p| {
            (
                js_to_string(Some(&p.card_id)),
                if js_falsy(Some(&Value::from(p.quantity))) { 1.0 } else { p.quantity },
            )
        })
        .collect();

    let mut added: f64 = 0.0;
    let mut failed: Vec<Value> = Vec::new();
    // Two bulkAddToCollection calls, Normal group first, index restarting per
    // call — exactly the Secret Lair two-pass shape (all reads before any
    // write so a mid-add Forward can replay clean).
    for group in ["Normal", "Holofoil"] {
        let members: Vec<(usize, &AddPlanEntry)> =
            plan.iter().enumerate().filter(|(_, p)| p.printing == group).collect();
        if members.is_empty() {
            continue;
        }
        let mut shared = Map::new();
        shared.insert("condition".into(), Value::String(cond.clone()));
        shared.insert("printing".into(), Value::String(group.to_string()));
        shared.insert("language".into(), language.clone());
        shared.insert("stackable".into(), json!(true));
        enum Planned {
            Ready(AddPlan),
            Reject(String),
        }
        let mut planned: Vec<(usize, AddPlanEntry, Planned)> = Vec::new();
        for (index, p) in members {
            let mut merged = shared.clone();
            // bulkAddToCollection perCard overrides: ride only when neither
            // null nor '' (js_to_string rule, same convention as secret_lair).
            let overrides: [(&str, Option<Value>); 4] = [
                ("quantity", Some(js_num_json(p.quantity))),
                ("purchase_price", p.purchase_price.map(js_num_json)),
                ("condition", Some(Value::String(p.condition.clone()))),
                ("printing", Some(Value::String(p.printing.clone()))),
            ];
            for (k, v) in overrides {
                if let Some(v) = v {
                    if !v.is_null() && js_to_string(Some(&v)) != "" {
                        merged.insert(k.into(), v);
                    }
                }
            }
            merged.insert("card_id".into(), p.card_id.clone());
            match plan_add(&conn, &merged) {
                Ok(pl @ AddPlan::Ready { .. }) => planned.push((index, (*p).clone(), Planned::Ready(pl))),
                Ok(AddPlan::Reject(msg)) => planned.push((index, (*p).clone(), Planned::Reject(msg))),
                Ok(AddPlan::Forward) => {
                    tracing::debug!("marketplace add: hydration-needing card, forwarding to Node");
                    return Err(AddErr::Replay);
                }
                // Non-AddCardError: Node console.errors and reports the
                // generic per-card message.
                Err(e) => {
                    tracing::error!(error = ?e, "marketplace add: plan failed");
                    planned.push((index, (*p).clone(), Planned::Reject("Failed to add card".into())));
                }
            }
        }
        for (index, p, pl) in planned {
            let mut fail = |error: String, failed: &mut Vec<Value>| {
                let mut fm = Map::new();
                fm.insert("card_id".into(), p.card_id.clone());
                fm.insert("error".into(), Value::String(error));
                fm.insert("index".into(), json!(index as i64));
                failed.push(Value::Object(fm));
            };
            match pl {
                Planned::Ready(pp) => match execute_add(&conn, user_id, &pp) {
                    Ok(_) => {
                        // Node's copy-count: qtyById is keyed by the ENTRY
                        // card_id, but bulkAdd reports the INSERTED card_id —
                        // which addCardToCollection may have swapped to the
                        // localized printing. Map miss => || 1, so a
                        // localized filing counts 1 copy per row, not qty.
                        let AddPlan::Ready { card_id, .. } = &pp else { unreachable!() };
                        // A non-TEXT bind could only be a JS number/null —
                        // never === any string entry key (JS Map strictness),
                        // so any sentinel that cannot collide is faithful.
                        let key = match card_id {
                            SqlValue::Text(s) => s.clone(),
                            _ => "\u{0}non-string-card-id".to_string(),
                        };
                        added += qty_by_id
                            .iter()
                            .find(|(k, _)| *k == key)
                            .map(|(_, v)| *v)
                            .unwrap_or(1.0);
                    }
                    Err(e) => {
                        tracing::error!(error = ?e, "marketplace add: card failed");
                        fail("Failed to add card".to_string(), &mut failed);
                    }
                },
                Planned::Reject(msg) => fail(msg, &mut failed),
            }
        }
    }
    let extras_held = lines.extras as f64;
    Ok(AddResult {
        added,
        failed,
        resolved: plan.len(),
        total_listed: uniq.len() as f64 + extras_held,
        unresolved: bulk.not_found,
    })
}

// ---------------------------------------------------------------------------
// Slice-2 handlers
// ---------------------------------------------------------------------------

/// orderFromRequest twin: gates run in Node's exact order (source → shape →
/// row → creds/enabled → provider fetch → parse). `Err(Response)` means the
/// helper already answered (the `fail()` contract that keeps the caller from
/// sending a second response).
async fn order_from_request(
    st: &AppState,
    pool: &DbPool,
    body: &Value,
    user_id: i64,
) -> Result<(ParsedOrder, bool), Response> {
    let source = to_str_or_empty(body.get("source")).to_lowercase();
    if !MP_SOURCES.contains(&source.as_str()) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            &format!("source must be one of: {}", MP_SOURCES.join(", ")),
        ));
    }
    let number = js_trim(&to_str_or_empty(body.get("order_number"))).to_string();
    if !order_number_shape_ok(&number) {
        return Err(json_error(StatusCode::BAD_REQUEST, "order_number is required"));
    }
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "marketplace: cannot open read connection");
            return Err(internal_error());
        }
    };
    let row = match load_row(&conn, user_id) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "marketplace: credential lookup failed");
            return Err(internal_error());
        }
    };
    drop(conn);
    let Some(row) = row else {
        return Err(json_error(StatusCode::NOT_FOUND, "User not found"));
    };
    // The fetchManapoolOrder enabled-gate has to live here too (route
    // comment): this lookup happens first, so a Settings switch-off would be
    // cosmetic for preview/add without it.
    if js_falsy(row.get("manapool_email"))
        || js_falsy(row.get("manapool_token"))
        || js_falsy(row.get("manapool_enabled"))
    {
        return Err(json_error(StatusCode::BAD_REQUEST, MP_NOT_CONNECTED_MSG));
    }
    let creds = Creds {
        email: js_to_string(row.get("manapool_email")),
        token: js_to_string(row.get("manapool_token")),
    };
    let fetched = match fetch_manapool_order(st.mp_http.as_ref(), &creds, &number).await {
        Ok(f) => f,
        Err(e) => {
            let status = match e.status {
                Some(s) if s != 0 => s,
                _ => 502,
            };
            let mut out = Map::new();
            out.insert(
                "error".into(),
                Value::String(if e.message.is_empty() {
                    "Order fetch failed".to_string()
                } else {
                    e.message
                }),
            );
            return Err(json_response(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                &Value::Object(out),
            ));
        }
    };
    let include_extras = matches!(body.get("include_extras"), Some(Value::Bool(true)));
    parse_order_payload(&fetched.body, &number, include_extras).map(|p| (p, include_extras)).map_err(|e| {
        let status = match e.status {
            Some(s) if s != 0 => s,
            _ => 502,
        };
        let mut out = Map::new();
        out.insert(
            "error".into(),
            Value::String(if e.message.is_empty() {
                "Order could not be read".to_string()
            } else {
                e.message
            }),
        );
        if let Some(keys) = e.observed_keys {
            out.insert("observedKeys".into(), Value::Array(keys));
        }
        json_response(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            &Value::Object(out),
        )
    })
}

fn mp_error_json(message: String) -> Response {
    let mut out = Map::new();
    out.insert("error".into(), Value::String(message));
    json_response(StatusCode::INTERNAL_SERVER_ERROR, &Value::Object(out))
}

pub async fn route_preview(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    handle_preview(st, req).await
}

pub async fn route_add(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    handle_add(st, req).await
}

async fn handle_preview(st: AppState, req: Request) -> Response {
    let headers: HeaderMap = req.headers().clone();
    let (parts, bytes) = read_parts(req).await;
    let _ = parts;
    let body = match parse_body(&headers, &bytes) {
        BodyOutcome::Parsed(b) => b,
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::TooLarge => return too_large(),
    };
    let Some(pool) = st.db.clone() else { return internal_error() };
    let user = match authenticate(&pool, &headers, false) {
        Ok(u) => u,
        Err(res) => return res,
    };
    // Node's invalidateOnWrite ran right after the global auth gate, for
    // every authenticated non-GET, whatever the handler then answered.
    stats::on_proxied_write(&st, &Method::POST, "/api/marketplace/preview", &headers);
    let (parsed, include_extras) = match order_from_request(&st, &pool, &body, user.id).await {
        Ok(v) => v,
        Err(res) => return res,
    };
    let mut out = parsed.to_json();
    let preview = match preview_fold(&st, &pool, user.id, &parsed.lines, include_extras).await {
        Ok(m) => m,
        Err(()) => {
            return replay_post(st, &headers, "/api/marketplace/preview", &body).await;
        }
    };
    for (k, v) in preview {
        out.insert(k, v);
    }
    json_response(StatusCode::OK, &Value::Object(out))
}

async fn handle_add(st: AppState, req: Request) -> Response {
    let headers: HeaderMap = req.headers().clone();
    let (parts, bytes) = read_parts(req).await;
    let _ = parts;
    let body = match parse_body(&headers, &bytes) {
        BodyOutcome::Parsed(b) => b,
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::TooLarge => return too_large(),
    };
    let Some(pool) = st.db.clone() else { return internal_error() };
    let user = match authenticate(&pool, &headers, false) {
        Ok(u) => u,
        Err(res) => return res,
    };
    stats::on_proxied_write(&st, &Method::POST, "/api/marketplace/add", &headers);
    let (parsed, _include_extras) = match order_from_request(&st, &pool, &body, user.id).await {
        Ok(v) => v,
        Err(res) => return res,
    };
    let body_obj = body.as_object().cloned().unwrap_or_default();
    let result = match add_fold(&st, &pool, user.id, &parsed.lines, &body_obj).await {
        Ok(r) => r,
        Err(AddErr::Replay) => return replay_post(st, &headers, "/api/marketplace/add", &body).await,
        Err(AddErr::Boom(msg)) => return mp_error_json(msg),
    };
    let mut out = Map::new();
    let mut order = Map::new();
    order.insert("number".into(), parsed.number.clone());
    order.insert("lineCount".into(), json!(parsed.lines.lines.len() as i64));
    out.insert("order".into(), Value::Object(order));
    out.insert("added".into(), js_num_json(result.added));
    out.insert("resolved".into(), json!(result.resolved as i64));
    out.insert("totalListed".into(), js_num_json(result.total_listed));
    out.insert("unresolved".into(), json!(result.unresolved));
    out.insert("failed".into(), Value::Array(result.failed));
    let message = if js_falsy(Some(&Value::from(result.added))) {
        "Nothing to add — no cards in the order resolved to catalogue cards.".to_string()
    } else {
        format!(
            "Added {} card{} from order {}.",
            js_to_string(Some(&js_num_json(result.added))),
            if result.added == 1.0 { "" } else { "s" },
            if js_falsy(Some(&parsed.number)) {
                "?".to_string()
            } else {
                js_to_string(Some(&parsed.number))
            }
        )
    };
    out.insert("message".into(), Value::String(message));
    json_response(StatusCode::OK, &Value::Object(out))
}

/// Whole untouched POST → Node (nothing local was written when the fold
/// decides to forward; same helper shape as the Secret Lair twins).
async fn replay_post(st: AppState, headers: &HeaderMap, path: &str, body: &Value) -> Response {
    let mut rb = Request::builder().method(Method::POST).uri(path);
    for (k, v) in headers.iter() {
        if k != axum::http::header::CONTENT_LENGTH {
            rb = rb.header(k, v);
        }
    }
    let req = rb.body(Body::from(body.to_string())).unwrap_or_else(|_| Request::new(Body::empty()));
    crate::proxy(State(st), req).await
}

//! Native `GET /api/sets` — port of `backend/src/routes/sets.js` (Lane B
//! unit 1). Flat list straight from the shared `sets` table (refreshed by
//! the Rust set-sync job / Node's startup sync), plus the `?tree=1` family
//! view which folds Scryfall child sets into their parents.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Map, Value};

use crate::collection::{body_etag, etag_matches, gzip_bytes, wants_encoding};
use crate::scryfall::{Scry, ScryOpts};
use crate::AppState;
use crate::poison::LockExt;

/// Node's getScryfallSets memo TTL (cardSets.js SCRYFALL_SETS_TTL_MS).
pub const SCRYFALL_SETS_TTL_MS: i64 = 6 * 60 * 60 * 1000;

/// Process memo for Scryfall's /sets list: Some((fetched_at_ms, data)).
/// A failed fetch keeps the previous answer (empty when there is none)
/// without stamping the clock — exactly like the Node memo.
pub type ScryfallSetsMemo = Arc<Mutex<Option<(i64, Vec<Value>)>>>;

/// Fetch (or reuse) Scryfall's /sets list through the shared client.
pub async fn scryfall_sets(scry: &Scry, memo: &ScryfallSetsMemo) -> Vec<Value> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    {
        let guard = memo.guard();
        if let Some((at, list)) = guard.as_ref() {
            if now - *at < SCRYFALL_SETS_TTL_MS {
                return list.clone();
            }
        }
    }
    let url = scry.scry_url("https://api.scryfall.com/sets");
    let fetched = match scry.get_retried(&url, &ScryOpts::default(), 4).await {
        Ok(v) => v
            .as_ref()
            .and_then(|v| v.get("data"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mut guard = memo.guard();
    if !fetched.is_empty() {
        *guard = Some((now, fetched.clone()));
    } else if guard.is_none() {
        *guard = Some((0, Vec::new()));
    }
    fetched
}

#[derive(Clone, Copy)]
pub struct Exclusions {
    pub tokens: bool,
    pub art_cards: bool,
    pub jumpstart: bool,
    pub promos: bool,
}

pub(crate) fn scan_exclusions(conn: &rusqlite::Connection) -> Exclusions {
    conn.query_row(
        "SELECT scan_exclude_tokens, scan_exclude_art_cards, scan_exclude_jumpstart, scan_exclude_promos FROM app_settings WHERE id = 1",
        [],
        |r| {
            Ok(Exclusions {
                tokens: r.get::<_, i64>(0).unwrap_or(0) != 0,
                art_cards: r.get::<_, i64>(1).unwrap_or(0) != 0,
                jumpstart: r.get::<_, i64>(2).unwrap_or(0) != 0,
                promos: r.get::<_, i64>(3).unwrap_or(0) != 0,
            })
        },
    )
    .unwrap_or(Exclusions { tokens: false, art_cards: false, jumpstart: false, promos: false })
}

/// cardSets.js `childAllowed`.
fn child_allowed(set_type: &str, e: &Exclusions) -> bool {
    !(e.tokens && set_type == "token")
        && !(e.art_cards && set_type == "memorabilia")
        && !(e.jumpstart && set_type == "starter")
        && !(e.promos && set_type == "promo")
}

/// cardSets.js `norm`: lowercase, strip everything outside [a-z0-9].
fn norm(set: &str) -> String {
    set.to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

/// getMtgChildSetMap: normalized parent-code -> childBrief[].
pub fn build_child_map(sets: &[Value], exclusions: &Exclusions) -> HashMap<String, Vec<Value>> {
    let mut map: HashMap<String, Vec<Value>> = HashMap::new();
    for s in sets {
        let parent = match s.get("parent_set_code").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => continue,
        };
        if s.get("digital").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        let set_type = s.get("set_type").and_then(|v| v.as_str()).unwrap_or("");
        if !child_allowed(set_type, exclusions) {
            continue;
        }
        let mut brief = Map::new();
        brief.insert(
            "code".into(),
            Value::String(s.get("code").and_then(|v| v.as_str()).unwrap_or("").to_string()),
        );
        brief.insert(
            "name".into(),
            Value::String(s.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string()),
        );
        brief.insert(
            "cardCount".into(),
            s.get("card_count").and_then(|v| v.as_i64()).map(Value::from).unwrap_or(Value::from(0)),
        );
        brief.insert(
            "type".into(),
            Value::String(if set_type.is_empty() { "subset".to_string() } else { set_type.to_string() }),
        );
        map.entry(norm(&parent)).or_default().push(Value::Object(brief));
    }
    map
}

/// routes/sets.js `code(s)`.
fn set_code(s: &Map<String, Value>) -> String {
    if let Some(c) = s.get("set_code").and_then(|v| v.as_str()) {
        if !c.is_empty() {
            return c.to_string();
        }
    }
    let id = s.get("id").and_then(|v| v.as_str()).unwrap_or("");
    id.strip_prefix("mtg-").unwrap_or(id).to_string()
}

fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    let q = query?;
    form_urlencoded::parse(q.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

const SELECT_COLS: [&str; 9] = [
    "id",
    "name",
    "series",
    "printed_total",
    "total",
    "release_date",
    "set_code",
    "symbol_url",
    "logo_url",
];

pub async fn handle(
    method: axum::http::Method,
    State(st): State<AppState>,
    req: Request,
) -> Response {
    // Node mounts the router at '/api/sets' behind authenticateToken and
    // only declares GET '/'; anything else (POST, sub-paths, trailing
    // slash) keeps its Node home via the proxy — a proxied response is
    // byte-perfect by construction.
    if method != axum::http::Method::GET || req.uri().path() != "/api/sets" || !st.owns("sets") {
        return crate::proxy(State(st.clone()), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st.clone()), req).await;
    };
    let headers = req.headers().clone();
    let query = req.uri().query().map(|s| s.to_string());

    // authenticateToken: Bearer session; API keys are GET-only (allowed).
    let token = crate::auth::bearer_token(&headers).map(|s| s.to_string());
    if let Err(res) = crate::auth::authenticate(&pool, token.as_deref(), true) {
        return res;
    }

    let tree = query_param(query.as_deref(), "tree").as_deref() == Some("1");

    let read = (|| -> Result<Vec<Map<String, Value>>, String> {
        let conn = pool.get().map_err(|e| format!("read pool: {e}"))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, name, series, printed_total, total, release_date, set_code, symbol_url, logo_url
                 FROM sets
                 ORDER BY release_date ASC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                let mut m = Map::new();
                for (i, name) in SELECT_COLS.iter().enumerate() {
                    let v: rusqlite::types::Value = row.get(i)?;
                    m.insert(
                        (*name).to_string(),
                        match v {
                            rusqlite::types::Value::Null => Value::Null,
                            rusqlite::types::Value::Integer(n) => Value::from(n),
                            rusqlite::types::Value::Real(f) => Value::from(f),
                            rusqlite::types::Value::Text(s) => Value::String(s),
                            rusqlite::types::Value::Blob(_) => Value::Null,
                        },
                    );
                }
                Ok(m)
            })
            .map_err(|e| e.to_string())?;
        let mut sets = Vec::new();
        for r in rows {
            sets.push(r.map_err(|e| e.to_string())?);
        }
        Ok(sets)
    })();

    let mut sets = match read {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Error fetching sets: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [
                    (header::CONTENT_TYPE, "application/json; charset=utf-8"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                axum::Json(serde_json::json!({ "error": "Failed to retrieve sets" })),
            )
                .into_response();
        }
    };

    if tree {
        // The scanner's set filter needs the family (tokens, art series,
        // promos…) offered by the parent, separable so a token-less user
        // can drop them.
        let scryfall_list = scryfall_sets(&st.scry, &st.scryfall_sets).await;
        let exclusions = pool
            .get()
            .ok()
            .map(|c| scan_exclusions(&c))
            .unwrap_or(Exclusions { tokens: false, art_cards: false, jumpstart: false, promos: false });
        let child_map = build_child_map(&scryfall_list, &exclusions);
        let child_codes: Vec<String> = child_map
            .values()
            .flat_map(|cs| {
                cs.iter().filter_map(|c| c.get("code").and_then(|v| v.as_str()).map(String::from))
            })
            .collect();
        sets = sets
            .into_iter()
            .filter(|s| {
                let code = set_code(s);
                !child_codes.iter().any(|c| *c == code)
            })
            .map(|mut s| {
                let key = norm(&set_code(&s));
                let children = child_map.get(&key).cloned().unwrap_or_default();
                s.insert("children".into(), Value::Array(children));
                s
            })
            .collect();
    }

    // The set list changes only when Scryfall releases a set (refreshed
    // every few hours server-side); let the browser reuse it for an hour.
    // private: responses sit behind auth.
    let raw = Value::Array(sets.into_iter().map(Value::Object).collect()).to_string().into_bytes();
    let tag = body_etag(&raw);
    let inm = headers.get("if-none-match").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let builder = Response::builder()
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .header(header::ETAG, HeaderValue::from_str(&tag).unwrap());
    if etag_matches(inm.as_deref(), &tag) {
        return builder.status(StatusCode::NOT_MODIFIED).body(Body::empty()).unwrap();
    }
    let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    if wants_encoding(ae.as_deref(), "gzip", raw.len()) {
        let gz = gzip_bytes(&raw);
        builder
            .status(StatusCode::OK)
            .header(header::CONTENT_ENCODING, "gzip")
            .header(header::VARY, "Accept-Encoding")
            .body(Body::from(gz))
            .unwrap()
    } else {
        builder.status(StatusCode::OK).body(Body::from(raw)).unwrap()
    }
}

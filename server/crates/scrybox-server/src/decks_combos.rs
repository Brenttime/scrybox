//! Native port of the two deck PROVIDER-READ routes from
//! `backend/src/routes/decks.js`: `GET /:id/combos` (Commander Spellbook
//! find-my-combos proxy) and `GET /:id/synergy` (EDHREC commander-page
//! synergy map). Mounted under the `decks_combos` name in RUST_ROUTES;
//! everything else on /api/decks stays owned by decks_reads / decks_writes
//! or proxied to Node untouched.
//!
//! Every status, body and message was written against the actual Node source
//! (routes/decks.js:213-332, utils/edhrecSynergy.js) — see PROGRESS.md
//! lane-A iter-5.
//!
//! Provider-leg ruling (mirrors the moxfield/marketplace precedent): the
//! Spellbook and EDHREC URLs are module-level constants in Node with no env
//! override, and the parity harness never recreates the Node container, so
//! the real-provider legs are EXCLUDED from the parity matrix. Both legs are
//! stub-proven here instead: the fetches go through the injectable
//! `DecksProviders` trait on AppState (`deck_http`), exactly like the
//! marketplace `mp_http` twin, and the unit tests script provider answers to
//! pin the trim/has/round/sort arms, the 404/403 null-cache arm, the
//! stale-disk fallback, the `!ok` 502s and the JSON-parse throw arms.
//!
//! Key JS semantics reproduced deliberately:
//!   * `res.json`'s Express-default weak ETag + If-None-Match/304 (shared
//!     `json_read_response` twin from decks_reads).
//!   * Body-parse-before-auth: the global express.json runs ahead of the
//!     /api authenticateToken gate, so an unparseable JSON body answers 500
//!     before auth (same twin as the decks_reads guard).
//!   * The route-wide `try/catch`: ANY throw (DB failure, network, timeout,
//!     `data.results` on a null body, `.map` on a truthy non-array) answers
//!     the route's single 502 string — not a 500.
//!   * `trimCombo`'s object-literal key order incl. the dropped-key rule for
//!     `undefined` (JSON.stringify) and the `||`-vs-`??` split
//!     (`popularity ?? null` keeps 0; `bracketTag || null` collapses it).
//!   * `synergyCards`' parseFloat-on-strings / Number.isFinite gate,
//!     max-per-name dedup, Math.round(x*100)/100, and the
//!     synergy-desc || name.localeCompare sort (root-ICU collator twin).
//!   * EDHREC 24 h two-tier cache (module Map + `${EDHREC_CACHE_DIR:-
//!     <db-dir>}/edhrec-cache/${slug}.json`), 404/403 negative-memo (mem
//!     only, never disk), stale-disk fallback on fetch error, best-effort
//!     disk write. The mem map is capped (MEM_CACHE_CAP, oldest-t evicted)
//!     — Node's Map is unbounded; per the owner ruling this port never
//!     deepens a cache hole, and 512 commander pages is far past the
//!     realistic working set.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::extract::Request;
use serde_json::{Map, Value};

use crate::auth::{authenticate, bearer_token};
use crate::collection_writes::{
    internal_error, json_error, node_exact, parse_body, read_parts, too_large, BodyOutcome,
};
use crate::db::DbPool;
use crate::decks_reads::{js_number, json_read_response, root_collator};
use crate::AppState;

/// utils/edhrecSynergy.js SYNERGY_THRESHOLD.
pub const SYNERGY_THRESHOLD: f64 = 0.35;
/// routes/decks.js SPELLBOOK_URL (module constant in Node; no env override).
pub const SPELLBOOK_URL: &str = "https://backend.commanderspellbook.com/find-my-combos";
/// routes/decks.js EDHREC_URL (module constant in Node; no env override).
pub const EDHREC_URL: &str = "https://json.edhrec.com/pages/commanders/";
/// The fetch User-Agent both routes send.
const USER_AGENT_VALUE: &str = "Scrybox/1.0";
/// AbortSignal.timeout(30000) on the Spellbook fetch.
const SPELLBOOK_TIMEOUT_MS: u64 = 30_000;
/// AbortSignal.timeout(20000) on the EDHREC fetch.
const EDHREC_TIMEOUT_MS: u64 = 20_000;
/// EDHREC_TTL_MS = 24 * 3600e3.
const EDHREC_TTL_MS: i64 = 24 * 3600_000;
/// Bounded twin of Node's unbounded module-level `edhrecMem` Map.
pub const MEM_CACHE_CAP: usize = 512;

// ---------------------------------------------------------------------------
// Injectable provider client (mp_http precedent)
// ---------------------------------------------------------------------------

pub struct FetchResp {
    pub status: u16,
    pub bytes: Vec<u8>,
}

/// Raw fetch both provider legs share: Node uses global fetch (undici) for
/// both; Err(()) == the fetch/timeout threw (axios-style network failure).
pub trait DecksProviders: Send + Sync {
    fn fetch(
        &self,
        method: &'static str,
        url: String,
        body: Option<String>,
        extra_headers: Vec<(String, String)>,
        timeout_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<FetchResp, ()>> + Send + 'static>>;
}

pub struct RealDecksProviders;

impl DecksProviders for RealDecksProviders {
    fn fetch(
        &self,
        method: &'static str,
        url: String,
        body: Option<String>,
        extra_headers: Vec<(String, String)>,
        timeout_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<FetchResp, ()>> + Send + 'static>> {
        Box::pin(async move {
            use http_body_util::BodyExt;
            let m = if method == "POST" { hyper::Method::POST } else { hyper::Method::GET };
            let mut b = hyper::Request::builder()
                .method(m)
                .uri(&url)
                .header("user-agent", USER_AGENT_VALUE);
            for (k, v) in extra_headers {
                b = b.header(k, v);
            }
            let req = match b.body(body.unwrap_or_default()) {
                Ok(r) => r,
                Err(_) => return Err(()),
            };
            let resp = tokio::time::timeout(
                std::time::Duration::from_millis(timeout_ms),
                crate::marketplace::mp_client().request(req),
            )
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
            let status = resp.status().as_u16();
            let bytes = resp
                .into_body()
                .collect()
                .await
                .map_err(|_| ())?
                .to_bytes();
            Ok(FetchResp { status, bytes: bytes.to_vec() })
        })
    }
}

// ---------------------------------------------------------------------------
// Shared guard (decks_reads::guard_get twin; also hands the query string to
// the synergy handler for req.query.commander).
// ---------------------------------------------------------------------------

pub(crate) struct Ctx {
    pool: Arc<DbPool>,
    headers: HeaderMap,
    user_id: i64,
    query: Option<String>,
}

async fn guard(st: &AppState, req: Request, group: &str) -> Result<Ctx, Response> {
    if !st.owns(group) || st.db.is_none() || req.method() != Method::GET {
        return Err(crate::proxy(State(st.clone()), req).await);
    }
    let pool = st.db.clone().unwrap();
    let (parts, bytes) = read_parts(req).await;
    // express.json runs globally ahead of the /api auth gate: a GET carrying
    // an unparseable JSON body answers 500 before auth (iter-28 live pin).
    match parse_body(&parts.headers, &bytes) {
        BodyOutcome::TooLarge => return Err(too_large()),
        BodyOutcome::Invalid => return Err(internal_error()),
        BodyOutcome::Parsed(_) => {}
    }
    let token = bearer_token(&parts.headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), true) {
        Ok(u) => u,
        Err(res) => return Err(node_exact(res).await),
    };
    Ok(Ctx { pool, headers: parts.headers, user_id: user.id, query: parts.uri.query().map(str::to_string) })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// GET /api/decks/:id/combos — Commander Spellbook find-my-combos proxy
// ---------------------------------------------------------------------------

pub async fn handle_combos(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    let ctx = match guard(&st, req, "decks_combos").await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let pool = ctx.pool.clone();
    let id2 = id.clone();
    let uid = ctx.user_id;
    let loaded = tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(|_| ())?;
        load_combo_rows(&conn, &id2, uid)
    })
    .await;
    // The route's try/catch turns ANY pre-response throw (incl. DB failure)
    // into the single Spellbook-unreachable 502.
    let rows = match loaded {
        Ok(Ok(Some(rows))) => rows,
        Ok(Ok(None)) => return json_error(StatusCode::NOT_FOUND, "Deck not found or unauthorized"),
        _ => return combos_unreachable(),
    };
    if rows.is_empty() {
        let mut out = Map::new();
        out.insert("identity".into(), Value::String(String::new()));
        out.insert("included".into(), Value::Array(vec![]));
        out.insert("almostIncluded".into(), Value::Array(vec![]));
        return json_read_response(&ctx.headers, serde_json::to_vec(&Value::Object(out)).unwrap(), Vec::<(String, String)>::new());
    }

    // Body: { commanders: [{card, quantity: 1}], main: [{card, quantity}] }
    let mut commanders = Vec::new();
    let mut main = Vec::new();
    // inDeck: lowercased names + front faces ("A // B" split, lowercased).
    let mut indeck: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in &rows {
        indeck.insert(r.name.to_lowercase());
        indeck.insert(front_face(&r.name).to_lowercase());
        let mut o = Map::new();
        o.insert("card".into(), Value::String(r.name.clone()));
        if r.is_commander {
            o.insert("quantity".into(), Value::from(1));
            commanders.push(Value::Object(o));
        } else {
            // Number(r.quantity) || 1
            let q = js_number(Some(&r.quantity));
            let q = if q == 0.0 || q.is_nan() { Value::from(1) } else { number_value(q) };
            o.insert("quantity".into(), q);
            main.push(Value::Object(o));
        }
    }
    let indeck_has = |n: &str| -> bool {
        let l = n.to_lowercase();
        indeck.contains(&l) || indeck.contains(&front_face(n).to_lowercase())
    };

    let mut body = Map::new();
    body.insert("commanders".into(), Value::Array(commanders));
    body.insert("main".into(), Value::Array(main));
    let resp = st
        .deck_http
        .fetch(
            "POST",
            SPELLBOOK_URL.to_string(),
            Some(serde_json::to_string(&Value::Object(body)).unwrap()),
            vec![
                ("content-type".into(), "application/json".into()),
                ("accept".into(), "application/json".into()),
            ],
            SPELLBOOK_TIMEOUT_MS,
        )
        .await;
    let data = match resp {
        Err(()) => return combos_unreachable(),
        Ok(f) if !(200..300).contains(&f.status) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                &format!("Commander Spellbook answered {}", f.status),
            )
        }
        Ok(f) => match serde_json::from_slice::<Value>(&f.bytes) {
            // `await upstream.json()` throwing lands in the same catch.
            Err(_) => return combos_unreachable(),
            Ok(v) => v,
        },
    };
    // `data.results` on a null body is a TypeError in JS -> the catch.
    // (serde get() on Null yields None, so the null case is pinned here.)
    if data.is_null() {
        return combos_unreachable();
    }
    let r = match data.get("results") {
        Some(v) if !v.is_null() && js_truthy(v) => v.clone(),
        _ => Value::Object(Map::new()),
    };
    let included = match trim_list(r.get("included"), &indeck_has) {
        Ok(v) => v,
        Err(()) => return combos_unreachable(),
    };
    let almost_included = match trim_list(r.get("almostIncluded"), &indeck_has) {
        Ok(v) => v,
        Err(()) => return combos_unreachable(),
    };
    let mut out = Map::new();
    out.insert("identity".into(), or_default(r.get("identity"), || Value::String(String::new())));
    out.insert("included".into(), included);
    out.insert("almostIncluded".into(), almost_included);
    json_read_response(&ctx.headers, serde_json::to_vec(&Value::Object(out)).unwrap(), Vec::<(String, String)>::new())
}

fn combos_unreachable() -> Response {
    json_error(StatusCode::BAD_GATEWAY, "Commander Spellbook is unreachable right now")
}

struct ComboRow {
    name: String,
    is_commander: bool,
    quantity: Value,
}

/// The two deck queries of the combos route (routes/decks.js:235-243).
fn load_combo_rows(
    conn: &rusqlite::Connection,
    id: &str,
    user_id: i64,
) -> Result<Option<Vec<ComboRow>>, ()> {
    use rusqlite::OptionalExtension;
    let deck: Option<i64> = conn
        .query_row(
            "SELECT id FROM decks WHERE id = ? AND user_id = ?",
            rusqlite::params![id, user_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(|_| ())?;
    let deck_id = match deck {
        Some(d) => d,
        None => return Ok(None),
    };
    let mut stmt = conn
        .prepare(
            "SELECT cc.name, MAX(dc.is_commander) AS is_commander, SUM(dc.quantity) AS quantity
             FROM deck_cards dc JOIN card_cache cc ON cc.id = dc.card_id
             WHERE dc.deck_id = ? AND dc.quantity > 0
             GROUP BY cc.name",
        )
        .map_err(|_| ())?;
    let rows = stmt
        .query_map(rusqlite::params![deck_id], |r| {
            let name: String = r.get::<_, String>(0).unwrap_or_default();
            let cmd: Option<i64> = r.get::<_, Option<i64>>(1).unwrap_or(None);
            let qty: Value = match r.get::<_, rusqlite::types::Value>(2) {
                Ok(rusqlite::types::Value::Null) => Value::Null,
                Ok(rusqlite::types::Value::Integer(i)) => Value::from(i),
                Ok(rusqlite::types::Value::Real(f)) => number_value(f),
                Ok(rusqlite::types::Value::Text(t)) => Value::String(t),
                _ => Value::Null,
            };
            Ok(ComboRow { name, is_commander: cmd.unwrap_or(0) != 0, quantity: qty })
        })
        .map_err(|_| ())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ())?;
    Ok(Some(rows))
}

fn front_face(n: &str) -> &str {
    n.split(" // ").next().unwrap_or("")
}

/// `commanderSlug` (utils/edhrecSynergy.js) — ported locally rather than
/// reusing the cards_meta `edhrec_slug` twin: that table maps U+0131
/// (`ı`) to `"i"`, but JS `normalize('NFD')` leaves U+0131 undecomposed
/// (no canonical decomposition), so Node's commanderSlug keeps it
/// non-alphanumeric ("Tıbir Tamı" -> "t-bir-tam", pinned from node). The
/// cards_meta entry is a pre-existing lane-B twin deviation (its own
/// vectors never covered U+0131) — flagged for lane B, not touched here.
pub fn commander_slug(name: &str) -> String {
    let first = name.split(" // ").next().unwrap_or("");
    let lowered = first.to_lowercase();
    let stripped: String = lowered
        .chars()
        .flat_map(nfd_decompose_small)
        .filter(|c| !('\u{0300}'..='\u{036f}').contains(c))
        .collect();
    // replace(/['’]/g, '') drops quotes outright; replace(/[^a-z0-9\s]/g,
    // ' ') turns every other non-alnum into a space; then \s+ collapse +
    // trim + ' ' -> '-'.
    let spaced: String = stripped
        .chars()
        .filter(|c| *c != '\'' && *c != '\u{2019}')
        .map(|c| {
            if c.is_ascii_alphanumeric() || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join("-")
}

/// Minimal NFD for the letters card names actually carry. Everything
/// without a canonical decomposition (U+0131, U+00D7/÷, symbols) passes
/// through unchanged, exactly like JS normalize('NFD').
fn nfd_decompose_small(c: char) -> Vec<char> {
    let t: &[(char, &str)] = &[
        ('\u{c0}', "A\u{300}"), ('\u{c1}', "A\u{301}"), ('\u{c2}', "A\u{302}"), ('\u{c3}', "A\u{303}"),
        ('\u{c4}', "A\u{308}"), ('\u{c5}', "A\u{30a}"), ('\u{c7}', "C\u{327}"), ('\u{c8}', "E\u{300}"),
        ('\u{c9}', "E\u{301}"), ('\u{ca}', "E\u{302}"), ('\u{cb}', "E\u{308}"), ('\u{cc}', "I\u{300}"),
        ('\u{cd}', "I\u{301}"), ('\u{ce}', "I\u{302}"), ('\u{cf}', "I\u{308}"), ('\u{d1}', "N\u{303}"),
        ('\u{d2}', "O\u{300}"), ('\u{d3}', "O\u{301}"), ('\u{d4}', "O\u{302}"), ('\u{d5}', "O\u{303}"),
        ('\u{d6}', "O\u{308}"), ('\u{d9}', "U\u{300}"), ('\u{da}', "U\u{301}"), ('\u{db}', "U\u{302}"),
        ('\u{dc}', "U\u{308}"), ('\u{dd}', "Y\u{301}"), ('\u{e0}', "a\u{300}"), ('\u{e1}', "a\u{301}"),
        ('\u{e2}', "a\u{302}"), ('\u{e3}', "a\u{303}"), ('\u{e4}', "a\u{308}"), ('\u{e5}', "a\u{30a}"),
        ('\u{e7}', "c\u{327}"), ('\u{e8}', "e\u{300}"), ('\u{e9}', "e\u{301}"), ('\u{ea}', "e\u{302}"),
        ('\u{eb}', "e\u{308}"), ('\u{ec}', "i\u{300}"), ('\u{ed}', "i\u{301}"), ('\u{ee}', "i\u{302}"),
        ('\u{ef}', "i\u{308}"), ('\u{f1}', "n\u{303}"), ('\u{f2}', "o\u{300}"), ('\u{f3}', "o\u{301}"),
        ('\u{f4}', "o\u{302}"), ('\u{f5}', "o\u{303}"), ('\u{f6}', "o\u{308}"), ('\u{f9}', "u\u{300}"),
        ('\u{fa}', "u\u{301}"), ('\u{fb}', "u\u{302}"), ('\u{fc}', "u\u{308}"), ('\u{fd}', "y\u{301}"),
        ('\u{ff}', "y\u{308}"), ('\u{130}', "I\u{307}"),
    ];
    match t.iter().find(|(k, _)| *k == c) {
        Some((_, s)) => s.chars().collect(),
        None => vec![c],
    }
}

fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            f != 0.0 && !f.is_nan()
        }
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// serde_json cannot express every JS number; integers and finite doubles
/// round-trip identically through JS JSON.stringify shortest-repr.
fn number_value(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() < 9.007199254740992e15 {
        Value::from(f as i64)
    } else {
        serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
    }
}

/// `x || d()` — falsy takes the default, truthy keeps the original value
/// (type and all, matching JSON.stringify of the original).
fn or_default<F: FnOnce() -> Value>(v: Option<&Value>, d: F) -> Value {
    match v {
        Some(v) if js_truthy(v) => v.clone(),
        _ => d(),
    }
}

/// `(combo.uses || [])` — falsy -> []; truthy non-array makes `.map` throw
/// (the route's catch).
fn or_array<'a>(v: Option<&'a Value>) -> Result<&'a [Value], ()> {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(&[]),
        Some(Value::Array(a)) => Ok(a),
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => Ok(&[]),
        Some(Value::String(s)) if s.is_empty() => Ok(&[]),
        Some(_) => Err(()),
    }
}

/// `String(v)` for the truthy values that reach it.
fn js_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "null".into(),
        Value::Array(a) => a
            .iter()
            .map(|e| match e {
                Value::Null => String::new(),
                other => js_to_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

fn trim_list<F: Fn(&str) -> bool>(v: Option<&Value>, indeck_has: &F) -> Result<Value, ()> {
    let arr = or_array(v)?;
    let mut out = Vec::with_capacity(arr.len());
    for c in arr.iter() {
        out.push(trim_combo(c, indeck_has)?);
    }
    Ok(Value::Array(out))
}

fn trim_combo<F: Fn(&str) -> bool>(combo: &Value, indeck_has: &F) -> Result<Value, ()> {
    let mut o = Map::new();
    // id: combo.id — only `undefined` (key absent) drops the key; null is
    // stringified as null (serde get() distinguishes None from Some(Null)).
    if let Some(id) = combo.get("id") {
        o.insert("id".into(), id.clone());
    }
    // cards: uses.map -> { name, image, missing }
    let mut cards = Vec::new();
    for u in or_array(combo.get("uses"))? {
        let name_v = u.get("card").and_then(|c| c.get("name"));
        let mut card_o = Map::new();
        card_o.insert(
            "name".into(),
            match name_v {
                Some(nv) if js_truthy(nv) => nv.clone(),
                _ => Value::String(String::new()),
            },
        );
        let card = u.get("card");
        let img = card.and_then(|c| c.get("imageUriFrontSmall"));
        let img = match img {
            Some(iv) if js_truthy(iv) => iv.clone(),
            _ => match card.and_then(|c| c.get("imageUriFrontNormal")) {
                Some(iv) if js_truthy(iv) => iv.clone(),
                _ => Value::Null,
            },
        };
        card_o.insert("image".into(), img);
        let name_str = match name_v {
            Some(nv) if js_truthy(nv) => js_to_string(nv),
            _ => String::new(),
        };
        card_o.insert("missing".into(), Value::Bool(!indeck_has(&name_str)));
        cards.push(Value::Object(card_o));
    }
    o.insert("cards".into(), Value::Array(cards));
    // templates: requires.map(r => r.template?.name).filter(Boolean)
    o.insert("templates".into(), filter_bool_map(combo.get("requires"), &|r| {
        r.get("template").and_then(|t| t.get("name"))
    })?);
    // produces: produces.map(p => p.feature?.name).filter(Boolean)
    o.insert("produces".into(), filter_bool_map(combo.get("produces"), &|p| {
        p.get("feature").and_then(|f| f.get("name"))
    })?);
    o.insert("manaNeeded".into(), or_default(combo.get("manaNeeded"), || Value::String(String::new())));
    // prerequisites: [easy, notable].filter(Boolean).join('\n')
    let pre: Vec<String> = [
        combo.get("easyPrerequisites"),
        combo.get("notablePrerequisites"),
    ]
    .into_iter()
    .flatten()
    .filter(|v| js_truthy(v))
    .map(|v| js_to_string(v))
    .collect();
    o.insert("prerequisites".into(), Value::String(pre.join("\n")));
    o.insert("description".into(), or_default(combo.get("description"), || Value::String(String::new())));
    o.insert("bracketTag".into(), or_default(combo.get("bracketTag"), || Value::Null));
    // popularity ?? null — only null/undefined collapse (0 stays 0).
    o.insert(
        "popularity".into(),
        match combo.get("popularity") {
            None | Some(Value::Null) => Value::Null,
            Some(v) => v.clone(),
        },
    );
    o.insert("identity".into(), or_default(combo.get("identity"), || Value::String(String::new())));
    Ok(Value::Object(o))
}

fn filter_bool_map<F: Fn(&Value) -> Option<&Value>>(arr_key: Option<&Value>, pick: &F) -> Result<Value, ()> {
    let mut out = Vec::new();
    for item in or_array(arr_key)? {
        if let Some(v) = pick(item) {
            if js_truthy(v) {
                out.push(v.clone());
            }
        }
    }
    Ok(Value::Array(out))
}

// ---------------------------------------------------------------------------
// GET /api/decks/:id/synergy — EDHREC commander-page synergy map
// ---------------------------------------------------------------------------

pub async fn handle_synergy(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    let ctx = match guard(&st, req, "decks_combos").await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let pool = ctx.pool.clone();
    let id2 = id.clone();
    let uid = ctx.user_id;
    let loaded = tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(|_| ())?;
        load_commander_names(&conn, &id2, uid)
    })
    .await;
    // Route catch: any throw -> the EDHREC-unreachable 502.
    let mut names: Vec<String> = match loaded {
        Ok(Ok(Some(names))) => names,
        Ok(Ok(None)) => return json_error(StatusCode::NOT_FOUND, "Deck not found or unauthorized"),
        _ => return synergy_unreachable(),
    };
    // typeof req.query.commander === 'string' ? .slice(0, 200) : ''
    let hint = query_commander(ctx.query.as_deref());
    if names.is_empty() && !hint.is_empty() {
        names = vec![hint];
    }
    if names.is_empty() {
        // res.json({ commander: null, threshold, cards: [] }) — no slug key.
        let mut out = Map::new();
        out.insert("commander".into(), Value::Null);
        out.insert("threshold".into(), number_value(SYNERGY_THRESHOLD));
        out.insert("cards".into(), Value::Array(vec![]));
        return json_read_response(&ctx.headers, serde_json::to_vec(&Value::Object(out)).unwrap(), Vec::<(String, String)>::new());
    }
    // Partner pairs live on a combined page (slug-sorted join); then the
    // first commander's own page.
    let mut slugs: Vec<String> = Vec::new();
    if names.len() > 1 {
        let mut parts: Vec<String> = names.iter().map(|n| commander_slug(n)).collect();
        parts.sort();
        slugs.push(parts.join("-"));
    }
    slugs.push(commander_slug(&names[0]));
    for slug in slugs {
        // if (!slug) continue;
        if slug.is_empty() {
            continue;
        }
        match edhrec_synergy_for(&st, &slug).await {
            Ok(Some(cards)) => {
                if js_truthy(&cards) {
                    return synergy_json(&ctx.headers, Some(slug), &names, cards);
                }
                // cached null (404/403 memo or stale-disk null) -> next slug
            }
            Ok(None) => {}
            Err(()) => return synergy_unreachable(),
        }
    }
    synergy_json(&ctx.headers, None, &names, Value::Array(vec![]))
}

fn synergy_unreachable() -> Response {
    json_error(StatusCode::BAD_GATEWAY, "EDHREC is unreachable right now")
}

fn synergy_json(headers: &HeaderMap, slug: Option<String>, names: &[String], cards: Value) -> Response {
    let mut out = Map::new();
    out.insert(
        "commander".into(),
        if names.is_empty() { Value::Null } else { Value::String(names.join(" + ")) },
    );
    out.insert("slug".into(), match slug {
        Some(s) => Value::String(s),
        None => Value::Null,
    });
    out.insert("threshold".into(), number_value(SYNERGY_THRESHOLD));
    out.insert("cards".into(), cards);
    json_read_response(headers, serde_json::to_vec(&Value::Object(out)).unwrap(), Vec::<(String, String)>::new())
}

/// `typeof req.query.commander === 'string' ? req.query.commander.slice(0, 200) : ''`
/// — repeated keys parse to an array (not a string) in qs, `?commander` is
/// the empty string, and slice counts UTF-16 code units.
fn query_commander(query: Option<&str>) -> String {
    let q = match query {
        Some(q) => q,
        None => return String::new(),
    };
    let mut found: Option<String> = None;
    let mut seen = 0usize;
    for pair in q.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if percent_decode(k) == "commander" {
            seen += 1;
            if seen == 1 {
                found = Some(v.replace('+', " "));
            }
        }
    }
    // Repeated key -> qs gives an array -> typeof !== 'string' -> ''.
    match (seen, found) {
        (1, Some(v)) => {
            let s = percent_decode(&v);
            // JS slice(0, 200) counts UTF-16 code units.
            let units: Vec<u16> = s.encode_utf16().take(200).collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::new(),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
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
    // qs percent-decodes to UTF-8 bytes; invalid sequences fall back to lossy.
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `SELECT DISTINCT cc.name ... is_commander = 1 ORDER BY cc.name`
/// (routes/decks.js:312-315); None == deck not found/unauthorized.
fn load_commander_names(
    conn: &rusqlite::Connection,
    id: &str,
    user_id: i64,
) -> Result<Option<Vec<String>>, ()> {
    use rusqlite::OptionalExtension;
    let deck: Option<i64> = conn
        .query_row(
            "SELECT id FROM decks WHERE id = ? AND user_id = ?",
            rusqlite::params![id, user_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(|_| ())?;
    let deck_id = match deck {
        Some(d) => d,
        None => return Ok(None),
    };
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT cc.name FROM deck_cards dc JOIN card_cache cc ON cc.id = dc.card_id
             WHERE dc.deck_id = ? AND dc.is_commander = 1 ORDER BY cc.name",
        )
        .map_err(|_| ())?;
    let names = stmt
        .query_map(rusqlite::params![deck_id], |r| r.get::<_, String>(0))
        .map_err(|_| ())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ())?;
    Ok(Some(names))
}

// ---------------------------------------------------------------------------
// EDHREC two-tier cache (edhrecMem Map + ${EDHREC_CACHE_DIR}/<slug>.json)
// ---------------------------------------------------------------------------

/// One `edhrecMem` entry: `{ t, cards }`. `cards` is the raw trimmed-cards
/// value; Null mirrors Node's negative memo (`cards: null`).
#[derive(Clone)]
pub struct EdhrecEntry {
    pub t: i64,
    pub cards: Option<Value>,
}

pub fn mem_insert(mem: &Mutex<HashMap<String, EdhrecEntry>>, slug: &str, entry: EdhrecEntry) {
    let mut m = mem.lock().unwrap();
    m.insert(slug.to_string(), entry);
    // Bounded twin of Node's unbounded Map: evict the oldest entry.
    while m.len() > MEM_CACHE_CAP {
        let oldest = m
            .iter()
            .min_by_key(|(_, e)| e.t)
            .map(|(k, _)| k.clone());
        match oldest {
            Some(k) => m.remove(&k),
            None => break,
        };
    }
}

fn edhrec_disk_path(st: &AppState, slug: &str) -> Option<std::path::PathBuf> {
    let dir = st.edhrec_cache_dir.clone()?;
    Some(dir.join(format!("{slug}.json")))
}

/// `edhrecSynergyFor(slug)` twin. Ok(Some(cards)) == return this page (the
/// caller truthiness-gates it), Ok(None) == falsy (negative memo/stale null),
/// Err == rethrow (route catch -> 502).
async fn edhrec_synergy_for(st: &AppState, slug: &str) -> Result<Option<Value>, ()> {
    let now = now_ms();
    if let Some(e) = st.edhrec_mem.lock().unwrap().get(slug) {
        if now - e.t < EDHREC_TTL_MS {
            return Ok(e.cards.clone());
        }
    }
    // let disk = null; try { disk = JSON.parse(readFileSync(file)) } catch { disk = null }
    let disk: Option<Value> = match edhrec_disk_path(st, slug) {
        Some(p) => std::fs::read(&p)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok()),
        None => None,
    };
    // if (disk && Date.now() - disk.t < TTL) { mem.set; return disk.cards }
    if let Some(d) = &disk {
        if js_truthy(d) {
            let fresh = d.get("t").and_then(|t| t.as_f64()).map(|t| (now - t as i64) < EDHREC_TTL_MS).unwrap_or(false);
            if fresh {
                let cards = d.get("cards").cloned();
                mem_insert(&st.edhrec_mem, slug, EdhrecEntry { t: d.get("t").and_then(|t| t.as_f64()).map(|t| t as i64).unwrap_or(now), cards: cards.clone() });
                return Ok(cards);
            }
        }
    }
    let url = format!("{}{}.json", EDHREC_URL, &encodeURIComponent(slug));
    let fetched = st
        .deck_http
        .fetch(
            "GET",
            url,
            None,
            vec![("accept".into(), "application/json".into())],
            EDHREC_TIMEOUT_MS,
        )
        .await;
    match fetched {
        // 404/403: negative memo (mem only), return null.
        Ok(f) if f.status == 404 || f.status == 403 => {
            mem_insert(&st.edhrec_mem, slug, EdhrecEntry { t: now, cards: Some(Value::Null) });
            Ok(Some(Value::Null))
        }
        Ok(f) if !(200..300).contains(&f.status) => Err(()), // throw -> stale-disk catch
        Ok(f) => match serde_json::from_slice::<Value>(&f.bytes) {
            Ok(v) => {
                let cards = Value::Array(synergy_cards(&v, SYNERGY_THRESHOLD));
                mem_insert(&st.edhrec_mem, slug, EdhrecEntry { t: now, cards: Some(cards.clone()) });
                // best-effort mirror to the shared cache volume.
                if let Some(p) = edhrec_disk_path(st, slug) {
                    if let Some(dir) = p.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let mut e = Map::new();
                    e.insert("t".into(), Value::from(now));
                    e.insert("cards".into(), cards.clone());
                    let _ = std::fs::write(&p, serde_json::to_vec(&Value::Object(e)).unwrap());
                }
                Ok(Some(cards))
            }
            Err(_) => Err(()),
        },
        Err(()) => Err(()),
    }
    .or_else(|_| {
        // catch (err) { if (disk) return disk.cards; throw err }
        match &disk {
            Some(d) if js_truthy(d) => Ok(d.get("cards").cloned()),
            _ => Err(()),
        }
    })
}

/// encodeURIComponent on an already-slugish value; kept correct for
/// arbitrary strings (all unreserved chars pass through).
#[allow(non_snake_case)]
fn encodeURIComponent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `synergyCards(data, threshold)` (utils/edhrecSynergy.js).
fn synergy_cards(data: &Value, threshold: f64) -> Vec<Value> {
    let lists = match data.get("container").and_then(|c| c.get("json_dict")).and_then(|j| j.get("cardlists")) {
        Some(Value::Array(l)) => l,
        _ => return Vec::new(),
    };
    // Map<string, f64> with insertion order preserved for the first-wins
    // tie rule before the sort (sort is total, but Node's Map order pins it).
    let mut best: Vec<(String, f64)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for list in lists {
        let cardviews = match list.get("cardviews") {
            Some(Value::Array(cv)) => cv,
            _ => continue,
        };
        for c in cardviews {
            // if (!c || typeof c !== 'object' || !c.name) continue
            if !matches!(c, Value::Object(_) | Value::Array(_)) || c.is_null() {
                continue;
            }
            let name = match c.get("name") {
                Some(n) if js_truthy(n) => n.clone(),
                _ => continue,
            };
            let name_s = js_to_string(&name);
            let syn: f64 = match c.get("synergy") {
                Some(Value::String(s)) => js_parse_float(s),
                Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
                _ => continue, // typeof !== 'number'
            };
            if !syn.is_finite() || syn < threshold {
                continue;
            }
            match index.get(&name_s) {
                Some(&i) => {
                    if syn > best[i].1 {
                        best[i].1 = syn;
                    }
                }
                None => {
                    index.insert(name_s.clone(), best.len());
                    best.push((name_s, syn));
                }
            }
        }
    }
    // Math.round(synergy * 100) / 100; sort synergy-desc, tie name localeCompare
    let mut rounded: Vec<(String, f64)> = best
        .into_iter()
        .map(|(n, s)| (n, js_math_round(s * 100.0) / 100.0))
        .collect();
    let collator = root_collator();
    rounded.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| collator.compare(&a.0, &b.0))
    });
    rounded
        .into_iter()
        .map(|(n, s)| {
            let mut o = Map::new();
            o.insert("name".into(), Value::String(n));
            o.insert("synergy".into(), number_value(s));
            Value::Object(o)
        })
        .collect()
}

/// JS Math.round (floor(x + 0.5) semantics for the finite, non-negative
/// values this call site sees).
fn js_math_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// JS parseFloat on a string: leading whitespace, optional sign, the
/// longest valid numeric prefix (decimal; "0x.." parses the leading 0,
/// "Infinity" parses, trailing junk ignored).
fn js_parse_float(s: &str) -> f64 {
    let trimmed = s.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let (sign, rest) = match trimmed.strip_prefix('-') {
        Some(r) => (-1.0, r),
        None => (1.0, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if rest.starts_with("Infinity") {
        return sign * f64::INFINITY;
    }
    if rest.starts_with("NaN") {
        return f64::NAN;
    }
    // Longest prefix parse — Rust's f64 parse requires the whole string, so
    // walk the longest prefix that parses.
    let bytes = rest.as_bytes();
    let mut end = 0usize;
    let mut i = 0usize;
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.' || bytes[i] == b'e' || bytes[i] == b'E') {
        if bytes[i] == b'e' || bytes[i] == b'E' {
            let mut j = i + 1;
            if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
                j += 1;
            }
            if j < bytes.len() && bytes[j].is_ascii_digit() {
                i = j;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                end = i;
                break;
            } else {
                break;
            }
        } else {
            i += 1;
            end = i;
        }
    }
    // Only one '.' allowed in the prefix; trim at the second.
    let prefix = &rest[..end.min(rest.len())];
    let prefix = match prefix.find('.') {
        Some(p) => match prefix[p + 1..].find('.') {
            Some(_) => &prefix[..p + 1 + prefix[p + 1..].find('.').unwrap()],
            None => prefix,
        },
        None => prefix,
    };
    if prefix.is_empty() || prefix == "." || prefix == "-." || prefix == "+." {
        return f64::NAN;
    }
    sign * prefix.parse::<f64>().unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsfloat_twin_matches_js_semantics() {
        assert_eq!(js_parse_float("0.42"), 0.42);
        assert_eq!(js_parse_float(" 0.5abc"), 0.5);
        assert!(js_parse_float("abc").is_nan());
        assert_eq!(js_parse_float("1e3"), 1000.0);
        assert_eq!(js_parse_float("1e"), 1.0);
        assert_eq!(js_parse_float("0x10"), 0.0);
        assert!(js_parse_float("Infinity").is_infinite());
        assert_eq!(js_parse_float("-.5"), -0.5);
        assert_eq!(js_parse_float("3.5.7"), 3.5);
    }

    #[test]
    fn round_ties_match_node() {
        assert_eq!(js_math_round(0.35 * 100.0), 35.0);
        assert_eq!(js_math_round(35.5), 36.0);
        assert_eq!(js_math_round(35.499999999999996), 35.0);
    }

    #[test]
    fn commanderslug_matches_node_vectors() {
        // Vectors pinned from node: require('backend/src/utils/edhrecSynergy').commanderSlug
        // (incl. the U+0131 case that the cards_meta twin would fail).
        assert_eq!(commander_slug("Atraxa, Praetors' Voice"), "atraxa-praetors-voice");
        assert_eq!(commander_slug("K'rrik, Son of Andan"), "krrik-son-of-andan");
        assert_eq!(commander_slug("Èterka, Weathered Explorer"), "eterka-weathered-explorer");
        assert_eq!(commander_slug("A // B"), "a");
        assert_eq!(commander_slug("!!!"), "");
        // Vectors pinned from `node -e commanderSlug(...)` (iter-5 diff of
        // utils/edhrecSynergy.js commanderSlug vs the cards_meta twin).
        assert_eq!(commander_slug("Atraxa, Praetors\u{2019} Voice"), "atraxa-praetors-voice");
        assert_eq!(commander_slug("Jodah, Archmage \u{c6}ther\u{2122}"), "jodah-archmage-ther");
        assert_eq!(commander_slug("T\u{131}bir Tam\u{131}"), "t-bir-tam");
        assert_eq!(commander_slug("Mox \u{b1} Diamond"), "mox-diamond");
        assert_eq!(commander_slug("Nashi, Moon\u{2019}s Bargain"), "nashi-moons-bargain");
        assert_eq!(commander_slug("  spaces  here  "), "spaces-here");
        assert_eq!(commander_slug(""), "");
    }

    #[test]
    fn synergy_cards_filters_rounds_and_sorts() {
        let data = serde_json::json!({
            "container": { "json_dict": { "cardlists": [
                { "cardviews": [
                    { "name": "Zombie", "synergy": "0.812" },
                    { "name": "Goblin", "synergy": 0.8 },
                    { "name": "Goblin", "synergy": 0.9 },
                    { "name": "Miserable", "synergy": "0.34" },
                    { "name": "Nany", "synergy": "nope" },
                    { "name": "", "synergy": 1.0 },
                    { "synergy": 1.0 },
                    "not-an-object",
                    null
                ] }
            ] } }
        });
        let cards = synergy_cards(&data, SYNERGY_THRESHOLD);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0]["name"], "Goblin");
        assert_eq!(cards[0]["synergy"], 0.9);
        assert_eq!(cards[1]["name"], "Zombie");
        assert_eq!(cards[1]["synergy"], 0.81);
        // No container at all -> [].
        assert!(synergy_cards(&Value::Null, SYNERGY_THRESHOLD).is_empty());
    }

    #[test]
    fn query_commander_matches_qs_slice_rules() {
        assert_eq!(query_commander(Some("commander=Raffine")), "Raffine");
        assert_eq!(query_commander(Some("commander=a&commander=b")), "");
        assert_eq!(query_commander(Some("commander")), "");
        assert_eq!(query_commander(Some("other=1")), "");
        assert_eq!(query_commander(Some("commander=a+b%20c")), "a b c");
        let long = "x".repeat(300);
        assert_eq!(query_commander(Some(&format!("commander={long}"))).len(), 200);
    }

    #[test]
    fn edhrec_mem_cap_evicts_oldest() {
        let mem = std::sync::Mutex::new(std::collections::HashMap::new());
        for i in 0..(MEM_CACHE_CAP as i64 + 5) {
            mem_insert(&mem, &format!("slug-{i}"), EdhrecEntry { t: 1000 + i, cards: Some(Value::Null) });
        }
        let m = mem.lock().unwrap();
        assert_eq!(m.len(), MEM_CACHE_CAP);
        assert!(m.get("slug-0").is_none());
        assert!(m.get("slug-4").is_none());
        assert!(m.get(&format!("slug-{}", MEM_CACHE_CAP + 4)).is_some());
    }

    #[test]
    fn trim_combo_pins_key_order_and_defaults() {
        let indeck = |n: &str| n.to_lowercase() == "lightning bolt";
        let combo = serde_json::json!({
            "id": 42,
            "uses": [{ "card": { "name": "Lightning Bolt", "imageUriFrontSmall": "s" } }, { "card": null }],
            "requires": [{ "template": { "name": "Creature" } }, { "template": {} }, {}],
            "produces": [{ "feature": { "name": "Draw" } }, { "feature": { "name": "" } }],
            "manaNeeded": 0,
            "easyPrerequisites": "easy",
            "notablePrerequisites": "",
            "popularity": 0
        });
        let t = trim_combo(&combo, &indeck).unwrap();
        let keys: Vec<&str> = t.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            vec!["id", "cards", "templates", "produces", "manaNeeded", "prerequisites",
                 "description", "bracketTag", "popularity", "identity"]
        );
        // 0 is falsy -> '' (|| collapse), unlike `?? null` on popularity.
        assert_eq!(t["manaNeeded"], "");
        assert_eq!(t["popularity"], 0); // ?? null keeps 0
        assert_eq!(t["prerequisites"], "easy");
        assert_eq!(t["bracketTag"], Value::Null);
        assert_eq!(t["description"], "");
        assert_eq!(t["cards"][0]["missing"], false);
        assert_eq!(t["cards"][1]["name"], "");
        assert_eq!(t["cards"][1]["image"], Value::Null);
        assert_eq!(t["cards"][1]["missing"], true);
        assert_eq!(t["templates"], serde_json::json!(["Creature"]));
        assert_eq!(t["produces"], serde_json::json!(["Draw"]));
        // No id key -> key dropped entirely.
        let t2 = trim_combo(&serde_json::json!({ "uses": [] }), &indeck).unwrap();
        assert!(t2.get("id").is_none());
        // Truthy non-array uses -> throw arm.
        assert!(trim_combo(&serde_json::json!({ "uses": 5 }), &indeck).is_err());
    }
}

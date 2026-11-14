//! LANE A — iter-37: secret_lair group (route switch `secret_lair`).
//! Port of `backend/src/routes/secretLair.js` + `backend/src/utils/secretLair.js`.
//!
//!   GET  /api/secret-lair?q=...          ranked search over the (cached)
//!                                        MTGJSON Secret Lair drop index
//!   GET  /api/secret-lair/preview        resolve one product file's card
//!        ?fileName=<file>                  list to real, ownable card_cache rows
//!   POST /api/secret-lair/add            file the whole product into the
//!                                        collection at the chosen printing
//!
//! Everything heavy is SHARED code, not a re-implementation: the (cached)
//! MTGJSON index, the deck-file fetch and the ranked-score ladder come from
//! precons.rs (getPreconIndex / getPreconCardList / rankPrecons — the same
//! mirror file, the same 24 h freshness window), the Scryfall resolver is
//! precons' bulkFetchByIdentifier twin on the shared Scry client, the card
//! cache is card_cache::cache_normalized_cards, and the writes reuse
//! collection_writes' plan_add/execute_add pair — exactly the shape the
//! bulk-add route pinned, mirroring the Node service (`bulkAddToCollection`)
//! the Secret Lair importer injects.
//!
//! Byte-parity surface (BRIEF rule 1): `res.json` body shape + key ORDER.
//!   search  {results, total, stale, mtgjsonDate}
//!   preview {name, code, type, cards, unresolved, totalListed, resolvedCount,
//!            fileName, resolvedFoil}
//!   add     {added, failed, name, totalCards, resolvedCount, unresolved,
//!            fileName, resolvedFoil, message}
//!           (the early "No cards to add" shape keeps `message` in its
//!            3rd position — the route spread does not move existing keys)
//!
//! Data-driven quirks pinned here (they are the mirror's, not ours):
//!   * getPreconCardList rows carry only {setCode, number, count, scryfallId,
//!     section} — never `name`/`isFoil`. previewSecretLairDrop therefore
//!     ALWAYS sees `src.name === ''` and `_isFoil === false`; the resolved
//!     card's display fields come from Scryfall, and every planned entry is
//!     'Normal' until MTGJSON starts shipping those fields. Ported verbatim.
//!   * searchSecretLair folds by trimmed-lowercased NAME and tests the
//!     /foiledition/i regex against the NAME, while resolveFoilFileName tests
//!     the same regex against the FILE NAME. Both kept distinct.
//!   * rankPrecons has no 50-cap here (that cap is the precons route's):
//!     search slices by `limit` (1..100, default 25).
//!
//! Forward policy (mirrors precons import): any state where Node's answer
//! cannot be byte-guaranteed locally — a Scryfall client error, or a card
//! cache/owned-count DB failure, or a plan_add `Forward` (a card the Node
//! twin would hydrate from Scryfall mid-add) — replays the WHOLE untouched
//! request to Node BEFORE any local collection write, so Node itself
//! performs the writes and produces the response. MTGJSON-side errors are
//! answered natively because fetch_json already pins the axios error shape.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
};
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value};

use crate::auth::{authenticate, bearer_token};
use crate::card_cache::cache_normalized_cards;
use crate::collection::truthy;
use crate::collection_writes::{execute_add, js_to_string, plan_add, AddPlan};
use crate::decks_reads::rows_map;
use crate::lists_reads::{qs_equals, qs_has, qs_string};
use crate::precons::{
    bulk_fetch_by_identifier, get_precon_card_list, get_precon_index, js_number, js_str_or_empty,
    js_string, js_trim, json_error, json_response, num_out, rank_precons, PreconError, BODY_LIMIT,
};
use crate::stats;
use crate::AppState;

/// secretLair.js SL_TYPE.
const SL_TYPE: &str = "Secret Lair Drop";

/// secretLair.js VALID_CONDITIONS — the util's OWN grade whitelist, applied
/// BEFORE the collection core's defaults.
const VALID_CONDITIONS: [&str; 5] = [
    "Near Mint",
    "Lightly Played",
    "Moderately Played",
    "Heavily Played",
    "Damaged",
];

// ---------------------------------------------------------------------------
// JS-string helpers local to this route pair
// ---------------------------------------------------------------------------

fn foil_tail_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)\s+foil\s+edition\s*$").unwrap())
}

/// `Boolean(v)` on a possibly-missing key.
fn truthy_opt(v: Option<&Value>) -> bool {
    v.map(truthy).unwrap_or(false)
}

/// secretLair.js `rep.code || ''` / `rep.setName || ''`: falsy
/// (undefined/null/''/0/false) collapses to '', truthy passes through
//  unchanged (Node keeps the original JSON value, e.g. a number).
fn nonnull_or_empty(v: Option<&Value>) -> Value {
    match v {
        Some(v) if truthy(v) => v.clone(),
        _ => Value::String(String::new()),
    }
}

/// secretLair.js `baseName`: strip a trailing " Foil Edition" (case-
/// insensitive), after String(name || ''), then trim.
fn base_name(name: Option<&Value>) -> String {
    let s = js_str_or_empty(name);
    js_trim(&foil_tail_re().replace(&s, "")).to_string()
}

/// secretLair.js `nameKey`: baseName, lowercased, every non-word RUN
/// collapsed to a single space (JS /[^\w]+/g with the ASCII [A-Za-z0-9_]
/// word set), trimmed. Run-collapse matters: "Animar,  Friends" keys to
/// "animar friends" in Node, not "animar   friends".
fn name_key(name: Option<&Value>) -> String {
    let s = base_name(name).to_lowercase();
    let mut collapsed = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            collapsed.push(c);
            in_run = false;
        } else if !in_run {
            collapsed.push(' ');
            in_run = true;
        }
    }
    js_trim(&collapsed).to_string()
}

/// secretLair.js `isFoilRow(row)`: strict `row.isFoil === true`, or a
/// trailing " Foil Edition" on the row NAME. (Distinct from the route's
/// /foiledition/i test on the FILE NAME — the two regexes differ on
/// purpose.)
fn is_foil_row(row: &Value) -> bool {
    if matches!(row.get("isFoil"), Some(Value::Bool(true))) {
        return true;
    }
    let s = js_str_or_empty(row.get("name"));
    foil_tail_re().is_match(&s)
}

/// secretLair.js `isFoilFileName` (route twin): /foiledition/i on
/// String(fileName || '') — the contiguous MTGJSON file-name convention.
#[allow(non_snake_case)]
fn is_foil_file_name(fileName: Option<&Value>) -> bool {
    let s = js_str_or_empty(fileName);
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)foiledition").unwrap())
        .is_match(&s)
}

// ---------------------------------------------------------------------------
// Number(x) on a raw qs string
// ---------------------------------------------------------------------------

/// `Number(req.query[key])` with the missing-key case preserved
/// (undefined → None; Node's `Number('')` → Some(0)).
fn qs_number(query: Option<&str>, key: &str) -> Option<f64> {
    if !qs_has(query, key) {
        return None;
    }
    js_number(&Value::String(qs_string(query, key)))
}

// ---------------------------------------------------------------------------
// getPreconIndex slice shared with the search route + foil-twin lookup
// ---------------------------------------------------------------------------

async fn secret_lair_index(st: &AppState) -> Result<crate::precons::IndexOutcome, PreconError> {
    // Node filters AFTER the shared cached index; the cache is the SAME file
    // and freshness window the precons group uses.
    get_precon_index(&st.mtgjson_base, &st.precons_index_path).await
}

fn secret_lair_drops(decks: &[Value]) -> Vec<Value> {
    decks
        .iter()
        .filter(|d| js_str_or_empty(d.get("type")) == SL_TYPE)
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// resolveFoilFileName (routes/secretLair.js)
// ---------------------------------------------------------------------------

/// Returns the resolved fileName plus the `resolvedFoil` flag
/// (`wantFoil && r.fileName !== fileName`, exact JS !==). The twin-missing
/// 422 is a ROUTE-level answer (`{error}` verbatim, no catch prefix), while a
/// thrown index failure rides the route's catch (`<verb> failed: ` prefix) —
/// two distinct shapes, hence two variants.
enum FoilErr {
    TwinMissing(String),
    Index(PreconError),
}

async fn resolve_foil_file_name(
    st: &AppState,
    file_name: &str,
    want_foil: bool,
) -> Result<(String, bool), FoilErr> {
    if !want_foil || is_foil_file_name(Some(&Value::String(file_name.to_string()))) {
        return Ok((file_name.to_string(), false));
    }
    let index = match secret_lair_index(st).await {
        Ok(i) => i,
        Err(e) => return Err(FoilErr::Index(e)),
    };
    let drops = secret_lair_drops(&index.decks);
    let lowered = file_name.to_lowercase();
    let hit = drops
        .iter()
        .find(|d| js_string(d.get("fileName").unwrap_or(&Value::Null)).to_lowercase() == lowered)
        .or_else(|| {
            drops
                .iter()
                .find(|d| js_string(d.get("name").unwrap_or(&Value::Null)).to_lowercase() == lowered)
        });
    let Some(hit) = hit else {
        // not a known drop: let the card-list fetch answer honestly
        return Ok((file_name.to_string(), false));
    };
    let needle = base_name(hit.get("name")).to_lowercase();
    let twin = drops.iter().find(|d| {
        is_foil_file_name(d.get("fileName"))
            && base_name(d.get("name")).to_lowercase() == needle
    });
    match twin {
        Some(t) => {
            let resolved = js_string(t.get("fileName").unwrap_or(&Value::Null));
            Ok((resolved.clone(), resolved != file_name))
        }
        None => Err(FoilErr::TwinMissing(
            "This Secret Lair drop has no foil edition available.".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// previewSecretLairDrop (utils/secretLair.js)
// ---------------------------------------------------------------------------

struct Preview {
    name: Value,
    code: Value,
    type_: Value,
    cards: Vec<Map<String, Value>>,
    unresolved: Value,
    total_listed: Value,
    resolved_count: Value,
}

/// Forward-trigger errors: states where Node's own run is the only
/// byte-guaranteed answer (Scryfall client error, card-cache DB failure,
/// owned-count DB failure). MTGJSON-side throws stay Err(PreconError).
enum PreviewFail {
    Answer(PreconError),
    Forward,
}

async fn preview_secret_lair_drop(
    st: &AppState,
    file_name: &str,
    user_id: i64,
) -> Result<Preview, PreviewFail> {
    if file_name.is_empty() {
        return Err(PreviewFail::Answer(PreconError {
            status: 400,
            message: "fileName required".into(),
        }));
    }
    let list = match get_precon_card_list(&st.mtgjson_base, file_name).await {
        Ok(v) => v,
        Err(e) => return Err(PreviewFail::Answer(e)),
    };

    // entries fold (utils/secretLair.js, verbatim).
    let mut uniq: Vec<Map<String, Value>> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    for c in list.get("cards").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        let scryfall_id = c.get("scryfallId").filter(|v| truthy(v)).map(js_string);
        let card_id = match &scryfall_id {
            Some(s) => s.clone(),
            None => format!(
                "name:{}|set:{}|num:{}",
                js_str_or_empty(c.get("name")),
                js_str_or_empty(c.get("setCode")),
                js_str_or_empty(c.get("number"))
            ),
        };
        if seen_keys.contains(&card_id) {
            continue;
        }
        seen_keys.insert(card_id.clone());
        // const n = Number(count); const c = Number.isFinite(n) && n >= 1 ? Math.floor(n) : 1;
        let n = c.get("count").and_then(js_number);
        let count = match n {
            Some(f) if f.is_finite() && f >= 1.0 => f.floor(),
            _ => 1.0,
        };
        let mut row = Map::new();
        row.insert("id".into(), Value::String(card_id));
        row.insert(
            "set_id".into(),
            c.get("setCode").filter(|v| truthy(v)).cloned().unwrap_or(Value::Null),
        );
        row.insert(
            "number".into(),
            c.get("number").filter(|v| truthy(v)).cloned().unwrap_or(Value::Null),
        );
        row.insert("name".into(), Value::String(js_str_or_empty(c.get("name"))));
        row.insert("_count".into(), num_out(count));
        row.insert("_isFoil".into(), Value::Bool(truthy_opt(c.get("isFoil"))));
        uniq.push(row);
    }

    // Bulk resolver rows: {id, set_id, number, name} (the _count/_isFoil
    // fields ride on the uniq rows, NOT the request rows).
    let rows: Vec<Map<String, Value>> = uniq
        .iter()
        .map(|r| {
            let mut m = Map::new();
            m.insert("id".into(), r.get("id").cloned().unwrap_or(Value::Null));
            m.insert(
                "set_id".into(),
                r.get("set_id").filter(|v| truthy(v)).cloned().unwrap_or(Value::Null),
            );
            m.insert(
                "number".into(),
                r.get("number").filter(|v| truthy(v)).cloned().unwrap_or(Value::Null),
            );
            m.insert("name".into(), Value::String(js_str_or_empty(r.get("name"))));
            m
        })
        .collect();

    let bulk = match bulk_fetch_by_identifier(st, &rows).await {
        Ok(b) => b,
        Err(e) => {
            // Node lets the axios client error bubble to the route catch;
            // Rust replays the WHOLE request so Node produces the exact
            // axios-shaped message (precons-import policy).
            tracing::warn!("secret-lair preview: scryfall bulk fetch failed ({e}); forwarding to Node");
            return Err(PreviewFail::Forward);
        }
    };

    let pool = st.db.clone().ok_or(PreviewFail::Forward)?;
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "secret-lair preview: cannot open read connection");
            return Err(PreviewFail::Forward);
        }
    };

    if !bulk.cards.is_empty() {
        let fresh: Vec<Map<String, Value>> =
            bulk.cards.iter().filter_map(|c| c.as_object().cloned()).collect();
        if let Err(e) = cache_normalized_cards(&conn, &fresh) {
            tracing::error!("secret-lair preview: cacheCards failed: {e}; forwarding to Node");
            return Err(PreviewFail::Forward);
        }
    }

    // resolved fold: first pair per card id wins; count/isFoil come from the
    // REQUEST row (p.row), everything else from the normalized card.
    let mut seen_id: HashSet<String> = HashSet::new();
    let mut resolved: Vec<Map<String, Value>> = Vec::new();
    for (i, card) in &bulk.pairs {
        let cid = js_string(card.get("id").unwrap_or(&Value::Null));
        if !seen_id.insert(cid.clone()) {
            continue;
        }
        let src = &uniq[*i];
        let mut r = Map::new();
        r.insert("card_id".into(), card.get("id").cloned().unwrap_or(Value::Null));
        r.insert(
            "name".into(),
            card.get("name")
                .filter(|v| truthy(v))
                .cloned()
                .or_else(|| src.get("name").filter(|v| truthy(v)).cloned())
                .unwrap_or_else(|| Value::String(String::new())),
        );
        r.insert(
            "image_url".into(),
            card.get("image_url").filter(|v| truthy(v)).cloned().unwrap_or(Value::Null),
        );
        // card.set_id || src.set_code || null — the request rows have no
        // set_code key (getPreconCardList never produced one), so this is
        // exactly `card.set_id || null`.
        r.insert(
            "set_code".into(),
            card.get("set_id").filter(|v| truthy(v)).cloned().unwrap_or(Value::Null),
        );
        r.insert(
            "number".into(),
            match card.get("number") {
                Some(v) if !v.is_null() => Value::String(js_string(v)),
                _ => Value::Null,
            },
        );
        r.insert(
            "count".into(),
            src.get("_count").filter(|v| truthy(v)).cloned().unwrap_or(Value::from(1)),
        );
        r.insert("isFoil".into(), Value::Bool(truthy_opt(src.get("_isFoil"))));
        resolved.push(r);
    }

    // ownedCountForCards: case-insensitive key Map, Number(qty) || 0.
    if !resolved.is_empty() {
        let card_ids: Vec<String> =
            resolved.iter().map(|r| js_string(r.get("card_id").unwrap_or(&Value::Null))).collect();
        let placeholders: Vec<String> =
            (1..=card_ids.len()).map(|i| format!("?{}", i + 1)).collect();
        let sql = format!(
            "SELECT card_id, SUM(quantity) qty FROM collection WHERE user_id = ?1 AND card_id IN ({}) GROUP BY card_id",
            placeholders.join(",")
        );
        let mut params: Vec<SqlValue> = vec![SqlValue::Integer(user_id)];
        params.extend(card_ids.iter().map(|s| SqlValue::Text(s.clone())));
        let rows = match rows_map(&conn, &sql, &params) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!("secret-lair preview: owned-count query failed: {e}; forwarding to Node");
                return Err(PreviewFail::Forward);
            }
        };
        let mut owned_lower: HashMap<String, f64> = HashMap::new();
        for row in rows {
            let key = js_string(row.get("card_id").unwrap_or(&Value::Null)).to_lowercase();
            let qty = js_number(row.get("qty").unwrap_or(&Value::Null)).unwrap_or(f64::NAN);
            owned_lower.insert(key, if truthy(&Value::from(qty)) { qty } else { 0.0 });
        }
        for r in resolved.iter_mut() {
            let k = js_string(r.get("card_id").unwrap_or(&Value::Null)).to_lowercase();
            let owned = owned_lower.get(&k).copied().unwrap_or(0.0);
            r.insert("owned".into(), num_out(owned));
        }
    }

    Ok(Preview {
        name: list.get("name").filter(|v| truthy(v)).cloned().unwrap_or_else(|| {
            Value::String(base_name(Some(&Value::String(file_name.to_string()))))
        }),
        code: list.get("code").filter(|v| truthy(v)).cloned().unwrap_or(Value::Null),
        type_: list.get("type").filter(|v| truthy(v)).cloned().unwrap_or(Value::Null),
        cards: resolved.clone(),
        unresolved: num_out(bulk.not_found as f64),
        total_listed: Value::from(uniq.len() as i64),
        resolved_count: Value::from(resolved.len() as i64),
    })
}

fn preview_out_map(p: &Preview) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("name".into(), p.name.clone());
    out.insert("code".into(), p.code.clone());
    out.insert("type".into(), p.type_.clone());
    out.insert("cards".into(), Value::Array(p.cards.iter().cloned().map(Value::Object).collect()));
    out.insert("unresolved".into(), p.unresolved.clone());
    out.insert("totalListed".into(), p.total_listed.clone());
    out.insert("resolvedCount".into(), p.resolved_count.clone());
    out
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/secret-lair — routes/secretLair.js GET / + searchSecretLair.
pub async fn route_search(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("secret_lair") || req.method() != Method::GET {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let headers: HeaderMap = req.headers().clone();
    let token = bearer_token(&headers).map(str::to_string);
    if let Err(res) = authenticate(&pool, token.as_deref(), true) {
        return res;
    }
    let query = req.uri().query().map(str::to_string);
    let q = js_trim(&qs_string(query.as_deref(), "q")).to_string();
    // const limit = Number(req.query.limit) || undefined;  then in the util:
    // Math.min(Math.max(Number(opts.limit) || 25, 1), 100), sliced as an
    // integer (Array.prototype.slice truncates).
    let route_limit = qs_number(query.as_deref(), "limit").filter(|n| *n != 0.0);
    let limit_f = route_limit.unwrap_or(25.0).max(1.0).min(100.0);
    let limit = limit_f.max(0.0) as usize;

    match secret_lair_index(&st).await {
        Ok(index) => {
            let drops = secret_lair_drops(&index.decks);
            // searchSecretLair byKey fold: slot keyed by nameKey(NAME), empty
            // key skips the row; isFoilRow (isFoil===true || trailing
            // " Foil Edition" on NAME) fills the foil slot, else the base
            // slot — each first-wins. Slots keep Map insertion order.
            let mut slots: Vec<(String, (Option<Value>, Option<Value>))> = Vec::new();
            let mut key_at: HashMap<String, usize> = HashMap::new();
            for d in &drops {
                let key = name_key(d.get("name"));
                if key.is_empty() {
                    continue;
                }
                let at = match key_at.get(&key) {
                    Some(&i) => i,
                    None => {
                        key_at.insert(key.clone(), slots.len());
                        slots.push((key, (None, None)));
                        slots.len() - 1
                    }
                };
                let slot = &mut slots[at].1;
                if is_foil_row(d) {
                    if slot.1.is_none() {
                        slot.1 = Some(d.clone());
                    }
                } else if slot.0.is_none() {
                    slot.0 = Some(d.clone());
                }
            }
            // merged row shape is the util's own literal — key, name,
            // fileName, code, setName, releaseDate, hasFoil, foilOnly, foil —
            // NOT the index row's keys.
            let mut merged: Vec<Value> = Vec::new();
            for (key, (base, foil)) in slots {
                let rep = match (base.as_ref(), foil.as_ref()) {
                    // Node prefers the non-foil row when both exist; binding
                    // it by match removes the checked-then-unwrapped edge
                    // (errors-maint L1).
                    (Some(b), _) => b,
                    (None, Some(f)) => f,
                    (None, None) => continue,
                };
                let stripped = base_name(rep.get("name"));
                let mut m = Map::new();
                m.insert("key".into(), Value::String(key));
                m.insert(
                    "name".into(),
                    if stripped.is_empty() {
                        rep.get("name").cloned().unwrap_or(Value::String(String::new()))
                    } else {
                        Value::String(stripped)
                    },
                );
                m.insert(
                    "fileName".into(),
                    base.as_ref()
                        .or(foil.as_ref())
                        .and_then(|v| v.get("fileName"))
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                m.insert(
                    "code".into(),
                    nonnull_or_empty(rep.get("code")),
                );
                m.insert(
                    "setName".into(),
                    nonnull_or_empty(rep.get("setName")),
                );
                m.insert(
                    "releaseDate".into(),
                    match rep.get("releaseDate") {
                        Some(v) if truthy(v) => v.clone(),
                        _ => Value::Null,
                    },
                );
                m.insert("hasFoil".into(), Value::Bool(foil.is_some()));
                m.insert("foilOnly".into(), Value::Bool(base.is_none() && foil.is_some()));
                m.insert(
                    "foil".into(),
                    match &foil {
                        Some(f) => {
                            let mut fo = Map::new();
                            fo.insert(
                                "fileName".into(),
                                f.get("fileName").cloned().unwrap_or(Value::Null),
                            );
                            fo.insert(
                                "name".into(),
                                f.get("name").cloned().unwrap_or(Value::Null),
                            );
                            fo.insert(
                                "releaseDate".into(),
                                match f.get("releaseDate") {
                                    Some(v) if truthy(v) => v.clone(),
                                    _ => Value::Null,
                                },
                            );
                            Value::Object(fo)
                        }
                        None => Value::Null,
                    },
                );
                merged.push(Value::Object(m));
            }
            let total = merged.len();
            let results: Vec<Value> = rank_precons(&merged, &q)
                .into_iter()
                .take(limit)
                .map(|d| {
                    let mut obj = d.as_object().cloned().unwrap_or_default();
                    obj.remove("_score");
                    obj.remove("_at");
                    Value::Object(obj)
                })
                .collect();
            let mut out = Map::new();
            out.insert("results".into(), Value::Array(results));
            out.insert("total".into(), Value::from(total as i64));
            out.insert("stale".into(), Value::Bool(index.source == "stale"));
            out.insert(
                "mtgjsonDate".into(),
                if truthy(&index.mtgjson_date) { index.mtgjson_date } else { Value::String(String::new()) },
            );
            json_response(StatusCode::OK, &out)
        }
        Err(e) => {
            let status =
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            json_error(status, &format!("Secret Lair search failed: {}", e.message))
        }
    }
}

/// GET /api/secret-lair/preview — routes/secretLair.js GET /preview.
pub async fn route_preview(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("secret_lair") || req.method() != Method::GET {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let headers: HeaderMap = req.headers().clone();
    let token = bearer_token(&headers).map(str::to_string);
    let user = match authenticate(&pool, token.as_deref(), true) {
        Ok(u) => u,
        Err(res) => return res,
    };
    let query = req.uri().query().map(str::to_string);
    let file_name = js_trim(&qs_string(query.as_deref(), "fileName")).to_string();
    if file_name.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "fileName is required");
    }
    let want_foil = qs_equals(query.as_deref(), "foil", "1")
        || qs_equals(query.as_deref(), "foil", "true");

    let (resolved_name, resolved_foil) = match resolve_foil_file_name(&st, &file_name, want_foil)
        .await
    {
        Ok(r) => r,
        Err(FoilErr::TwinMissing(m)) => {
            return json_error(StatusCode::UNPROCESSABLE_ENTITY, &m);
        }
        Err(FoilErr::Index(e)) => {
            let status =
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            return json_error(status, &format!("Secret Lair preview failed: {}", e.message));
        }
    };

    match preview_secret_lair_drop(&st, &resolved_name, user.id).await {
        Ok(p) => {
            let mut out = preview_out_map(&p);
            out.insert("fileName".into(), Value::String(resolved_name));
            out.insert("resolvedFoil".into(), Value::Bool(resolved_foil));
            json_response(StatusCode::OK, &out)
        }
        Err(PreviewFail::Answer(e)) => {
            let status =
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            json_error(status, &format!("Secret Lair preview failed: {}", e.message))
        }
        Err(PreviewFail::Forward) => replay_get(st, &headers, "/api/secret-lair/preview", query).await,
    }
}

/// POST /api/secret-lair/add — routes/secretLair.js POST /add +
/// addSecretLairToCollection.
pub async fn route_add(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("secret_lair") || req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let headers: HeaderMap = req.headers().clone();
    // normalJson is mounted above authenticateToken (server.js), so
    // malformed/oversized bodies answer BEFORE the auth gate.
    let (body_req, early) = crate::precons::read_json_body(req, BODY_LIMIT).await;
    if let Some(res) = early {
        return res;
    }
    let token = bearer_token(&headers).map(str::to_string);
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return res,
    };
    let body_obj = body_req.as_object().cloned().unwrap_or_default();

    // const fileName = String(body.fileName || '').trim();
    let file_name =
        js_trim(&js_str_or_empty(body_obj.get("fileName"))).to_string();
    if file_name.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "fileName is required");
    }
    // const wantFoil = body.foil === true || body.foil === 'true' || body.foil === 1;
    let want_foil = match body_obj.get("foil") {
        Some(Value::Bool(true)) => true,
        Some(Value::String(s)) if s == "true" => true,
        Some(Value::Number(n)) => n.as_i64() == Some(1) || n.as_f64() == Some(1.0),
        _ => false,
    };

    let (resolved_name, resolved_foil) =
        match resolve_foil_file_name(&st, &file_name, want_foil).await {
            Ok(r) => r,
            Err(FoilErr::TwinMissing(m)) => {
                return json_error(StatusCode::UNPROCESSABLE_ENTITY, &m);
            }
            Err(FoilErr::Index(e)) => {
                let status = StatusCode::from_u16(e.status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return json_error(status, &format!("Secret Lair add failed: {}", e.message));
            }
        };

    let preview = match preview_secret_lair_drop(&st, &resolved_name, user.id).await {
        Ok(p) => p,
        Err(PreviewFail::Answer(e)) => {
            let status =
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            return json_error(status, &format!("Secret Lair add failed: {}", e.message));
        }
        Err(PreviewFail::Forward) => {
            return replay_post(st, &headers, "/api/secret-lair/add", &body_req).await;
        }
    };

    // Early shape: nothing resolved is a bad answer, not a server fault.
    // Key order follows the Node object literal — `message` sits THIRD here
    // and the route spread does not move it.
    if preview.cards.is_empty() {
        let mut out = Map::new();
        out.insert("added".into(), Value::from(0));
        out.insert("failed".into(), Value::Array(vec![]));
        out.insert("message".into(), Value::String("No cards to add".into()));
        out.insert("name".into(), preview.name.clone());
        out.insert("totalCards".into(), preview.total_listed.clone());
        out.insert("unresolved".into(), preview.unresolved.clone());
        out.insert("fileName".into(), Value::String(resolved_name));
        out.insert("resolvedFoil".into(), Value::Bool(resolved_foil));
        return json_response(StatusCode::UNPROCESSABLE_ENTITY, &out);
    }

    // Plan (utils/secretLair.js addSecretLairToCollection).
    // const mult = Math.max(1, Math.min(Number(copies) || 1, 500));
    // '2x' → Number = NaN → falsy → 1 (NOT parseInt's 2).
    let mult_raw = js_number(body_obj.get("copies").unwrap_or(&Value::Null))
        .filter(|n| n.is_finite() && *n != 0.0)
        .unwrap_or(1.0);
    let mult = mult_raw.min(500.0).max(1.0);
    // const mode = ['auto','foil','nonfoil'].includes(String(printingMode).toLowerCase())
    //   ? String(printingMode).toLowerCase() : 'auto';  (route passes body.printingMode || 'auto')
    let mode_raw = match body_obj.get("printingMode").filter(|v| truthy(v)) {
        Some(v) => js_string(v).to_lowercase(),
        None => "auto".to_string(),
    };
    let mode = match mode_raw.as_str() {
        "auto" | "foil" | "nonfoil" => mode_raw,
        _ => "auto".to_string(),
    };
    // const condition2 = VALID_CONDITIONS.includes(condition) ? condition : 'Near Mint';
    // (the util's OWN whitelist runs BEFORE the core's defaults — an invalid
    // grade is rewritten to 'Near Mint', not stored raw).
    let condition = match body_obj.get("condition") {
        Some(v @ Value::String(_)) if VALID_CONDITIONS.contains(&js_to_string(Some(v)).as_str()) => {
            v.clone()
        }
        _ => Value::String("Near Mint".into()),
    };
    // language = 'English' is a PARAM DEFAULT: only an absent body key falls
    // back; an explicit null rides through to the row exactly like Node.
    let language = body_obj
        .get("language")
        .cloned()
        .unwrap_or(Value::String("English".into()));

    let mut normal: Vec<Map<String, Value>> = Vec::new();
    let mut holo: Vec<Map<String, Value>> = Vec::new();
    for card in &preview.cards {
        // quantity: Math.max(1, Math.round((c.count || 1) * mult))
        let base = js_number(card.get("count").unwrap_or(&Value::Null))
            .filter(|n| n.is_finite() && *n != 0.0)
            .unwrap_or(1.0);
        let total = (base * mult).round().max(1.0);
        // printing: mode forces; 'auto' honours the product's own isFoil flag.
        let printing = match mode.as_str() {
            "foil" => "Holofoil",
            "nonfoil" => "Normal",
            _ => {
                if truthy_opt(card.get("isFoil")) {
                    "Holofoil"
                } else {
                    "Normal"
                }
            }
        };
        let mut entry = Map::new();
        entry
            .insert("card_id".into(), card.get("card_id").cloned().unwrap_or(Value::Null));
        entry.insert("quantity".into(), num_out(total));
        entry.insert("printing".into(), Value::String(printing.to_string()));
        if printing == "Holofoil" {
            holo.push(entry);
        } else {
            normal.push(entry);
        }
    }

    // shared = { condition: condition2, printing: <group>, language, stackable: true }
    let mut shared = Map::new();
    shared.insert("condition".into(), condition);
    shared.insert("printing".into(), Value::String(mode));
    shared.insert("language".into(), language);
    shared.insert("stackable".into(), Value::Bool(true));

    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "secret-lair add: cannot open write connection");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
        }
    };

    // Pass 1 — pure reads over BOTH groups before any write (the same shape
    // handle_bulk_add pinned): Node answers per-card failures INSIDE
    // bulkAddToCollection, and a card Node would hydrate mid-add (a
    // plan_add `Forward`) has no partial-write escape: the WHOLE request is
    // replayed to Node while nothing local has been written yet.
    enum Planned {
        Ready(AddPlan),
        Reject(String),
    }
    let groups: [&Vec<Map<String, Value>>; 2] = [&normal, &holo];
    let mut planned: Vec<(usize, Map<String, Value>, Planned)> = Vec::new();
    for group in groups {
        for (index, entry) in group.iter().enumerate() {
            let mut merged = shared.clone();
            // perCard overrides (bulkAddToCollection): quantity/printing ride
            // when neither null nor ''.
            for k in ["quantity", "printing"] {
                if let Some(v) = entry.get(k) {
                    if !v.is_null() && js_to_string(Some(v)) != "" {
                        merged.insert(k.into(), v.clone());
                    }
                }
            }
            merged.insert("card_id".into(), entry.get("card_id").cloned().unwrap_or(Value::Null));
            match plan_add(&conn, &merged) {
                Ok(p @ AddPlan::Ready { .. }) => {
                    planned.push((index, entry.clone(), Planned::Ready(p)));
                }
                Ok(AddPlan::Reject(msg)) => {
                    planned.push((index, entry.clone(), Planned::Reject(msg)));
                }
                Ok(AddPlan::Forward) | Ok(AddPlan::FetchCard(_)) => {
                    tracing::debug!("secret-lair add: hydration-needing card, forwarding to Node");
                    return replay_post(st, &headers, "/api/secret-lair/add", &body_req).await;
                }
                // Non-AddCardError: Node console.errors and reports the
                // generic per-card message.
                Err(e) => {
                    tracing::error!(error = ?e, "secret-lair add: plan failed");
                    planned.push((index, entry.clone(), Planned::Reject("Failed to add card".into())));
                }
            }
        }
    }

    // Pass 2 — sequential writes in group order (Normal, then Holofoil);
    // `index` restarts per bulkAddToCollection call, exactly like Node.
    let mut added_count: i64 = 0;
    let mut failed: Vec<Value> = Vec::new();
    for (index, entry, plan) in planned {
        let card_id = entry.get("card_id").cloned().unwrap_or(Value::Null);
        let fail = |error: String, failed: &mut Vec<Value>| {
            let mut m = Map::new();
            m.insert("card_id".into(), card_id.clone());
            m.insert("error".into(), Value::String(error));
            m.insert("index".into(), Value::from(index as i64));
            failed.push(Value::Object(m));
        };
        match plan {
            Planned::Ready(p) => match execute_add(&conn, user.id, &p) {
                Ok(_) => added_count += 1,
                Err(e) => {
                    tracing::error!(error = ?e, "secret-lair add: card failed");
                    fail("Failed to add card".into(), &mut failed);
                }
            },
            Planned::Reject(msg) => fail(msg, &mut failed),
        }
    }

    let mut out = Map::new();
    out.insert("added".into(), Value::from(added_count));
    out.insert("failed".into(), Value::Array(failed));
    out.insert("name".into(), preview.name.clone());
    out.insert("totalCards".into(), preview.total_listed.clone());
    out.insert("resolvedCount".into(), preview.resolved_count.clone());
    out.insert("unresolved".into(), preview.unresolved.clone());
    out.insert("fileName".into(), Value::String(resolved_name));
    out.insert("resolvedFoil".into(), Value::Bool(resolved_foil));

    let resolved_count =
        js_number(&preview.resolved_count).unwrap_or(0.0) > 0.0 || added_count > 0;
    let message = if added_count > 0 {
        format!(
            "Added {} card{} from {}.",
            added_count,
            if added_count == 1 { "" } else { "s" },
            js_string(&preview.name)
        )
    } else if resolved_count {
        let u = js_number(&preview.unresolved).unwrap_or(f64::NAN);
        let shown = if truthy(&Value::from(u)) { u } else { 0.0 };
        format!(
            "Nothing filed — {} card(s) could not be matched to your catalog.",
            js_string(&num_out(shown))
        )
    } else {
        "No cards could be read from this product.".to_string()
    };
    out.insert("message".into(), Value::String(message));

    let resp = json_response(
        if resolved_count { StatusCode::OK } else { StatusCode::UNPROCESSABLE_ENTITY },
        &out,
    );
    if resp.status().is_success() {
        stats::on_proxied_write(&st, &Method::POST, "/api/secret-lair/add", &headers);
    }
    resp
}

// ---------------------------------------------------------------------------
// Replay helpers (whole untouched request → Node, nothing written yet)
// ---------------------------------------------------------------------------

async fn replay_get(st: AppState, headers: &HeaderMap, path: &str, query: Option<String>) -> Response {
    let uri = match query {
        Some(q) if !q.is_empty() => format!("{path}?{q}"),
        _ => path.to_string(),
    };
    let mut rb = Request::builder().method(Method::GET).uri(uri);
    for (k, v) in headers.iter() {
        rb = rb.header(k, v);
    }
    let req = rb.body(Body::empty()).unwrap_or_else(|_| Request::new(Body::empty()));
    crate::proxy(State(st), req).await
}

async fn replay_post(st: AppState, headers: &HeaderMap, path: &str, body: &Value) -> Response {
    let mut rb = Request::builder().method(Method::POST).uri(path);
    for (k, v) in headers.iter() {
        if k != axum::http::header::CONTENT_LENGTH {
            rb = rb.header(k, v);
        }
    }
    let req = rb
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Request::new(Body::empty()));
    crate::proxy(State(st), req).await
}

//! Port of the local-cache fast path of `GET /api/search`
//! (`backend/src/routes/collection.js` `searchCards` → `scryfallApi.js`
//! `runSearch` field-filter branch → `cardSearchSql.js` `localCacheQuery` →
//! `priceHelpers.parseCardRow` + `collection.js` `attachOwnedQty`).
//!
//! Scope of the port (mirrors Node's own short-circuit exactly): only
//! requests Node itself would answer from `card_cache` are served natively —
//! empty raw query, scope not `collection`/`internet`, no `prints=1`, no scan
//! `image` — and only when the local query returns rows. Zero rows (or any
//! query error) means Node would fall through to the Scryfall/sidecar path,
//! so the request is proxied instead and Node answers it verbatim. That
//! keeps the parity contract intact: Rust never invents an answer Node would
//! not give from the cache.
//!
//! Response shape (verified against live Node): bare JSON array of
//! `parseCardRow`-shaped rows with `owned_qty` appended, plus the headers
//! `X-Source: cache` and `Access-Control-Expose-Headers: X-Total-Count,
//! X-Source`, and Express's default body ETag (replays as 304).
//!
//! Cache: serialized raw+gzip keyed by (user, normalized params) x
//! `collection_read_revision`, whose triggers cover every card_cache and
//! collection write Node can make.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
};
use tokio::sync::{OnceCell, Semaphore};

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use rusqlite::{Connection, Row};
use serde_json::{json, Map, Value};

use crate::poison::LockExt;
use crate::{
    auth, collq,
    collection::{body_etag, etag_matches, gzip_bytes, parse_array_col, sqlite_value, truthy, wants_encoding},
    db::DbPool,
    ip_limit,
    name_index::{self, NameIndex, OwnedCache, Txn},
    sidecar, AppState,
};

// ---------------------------------------------------------------------------
// Language resolution — port of utils/languages.js resolve()/toName().
// Same table, same fallback order (code, name, provider), English default.
// ---------------------------------------------------------------------------

fn langs() -> &'static Vec<(String, String, String)> {
    static L: OnceLock<Vec<(String, String, String)>> = OnceLock::new();
    L.get_or_init(|| {
        let v: Vec<Value> = serde_json::from_str(include_str!("../../../../shared/languages.json"))
            .expect("shared/languages.json");
        v.iter()
            .map(|l| {
                (
                    l["code"].as_str().unwrap_or("").to_string(),
                    l["name"].as_str().unwrap_or("").to_string(),
                    l["scryfall"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    })
}

/// `languages.toName(input)` — display name ('English'). `None`/empty is the
/// JS falsy path → DEFAULT (LANGUAGES[0], English).
pub(crate) fn to_name(input: Option<&str>) -> String {
    let default = || langs()[0].1.clone();
    let Some(input) = input.filter(|s| !s.is_empty()) else { return default() };
    let lower = input.trim().to_lowercase();
    for (code, name, scry) in langs() {
        if lower == *code
            || lower == name.to_lowercase()
            || (!scry.is_empty() && lower == *scry)
        {
            return name.clone();
        }
    }
    default()
}

// ---------------------------------------------------------------------------
// Query parsing (express req.query semantics for this route)
// ---------------------------------------------------------------------------

/// The `scope=collection` field-filter leg (`cardSearchSql.collectionQuery`,
/// reached from `runSearch` line 732): pure SQL over the user's OWN rows —
/// no Scryfall, no sidecar, no language filter (an owner sees every language
/// they own). An empty result IS the answer: Node returns `[]`, it never
/// falls through to an API. Prints/lang are inert on this leg (Node's
/// allPrints branch is gated `scope !== 'collection'`); `image` is not — the
/// route cvScan-ranks the rows after the search, so that stays on Node.
#[derive(Clone, Debug)]
pub struct CollectionOwned {
    pub user_id: i64,
    pub name: String,
    pub number: String,
    pub set: String,
    pub page: i64,
    pub limit: i64,
}

impl CollectionOwned {
    fn cache_key(&self) -> String {
        format!(
            "o|{}|{}|{}|{}|{}|{}",
            self.user_id, self.name, self.number, self.set, self.page, self.limit
        )
    }
}

/// The normalized parameters the native path consumes. `PartialEq`/`Hash` via
/// the manual `cache_key`.
#[derive(Clone, Debug)]
pub struct SearchQuery {
    user_id: i64,
    name: String,
    number: String,
    set: String,
    lang_name: String,
    page: i64,
    limit: i64,
    /// Zero-row local results may be answered by the sidecar leg instead of
    /// proxying: the name-only English field-filter branch (no number/set —
    /// the specific-query and set-browse legs stay on Node).
    walkable: bool,
    /// Raw `lang` query param as Node's merge block keeps it: `langSearch`'s
    /// resolve input and the rawPageCache key's language element
    /// (`lang || ''`). `None` = param absent (Node's destructured null).
    lang: Option<String>,
    /// scope=internet: Node skips queryLocal (upstream first), the route
    /// mergeSearchPrices the page before send, and the merge catch re-queries
    /// the cache. The response-cache key must separate the two scopes.
    internet: bool,
}

impl SearchQuery {
    fn cache_key(&self) -> String {
        format!(
            "s|{}|{}|{}|{}|{}|{}|{}|{}",
            self.user_id,
            self.name,
            self.number,
            self.set,
            self.lang_name,
            self.page,
            self.limit,
            if self.internet { "i" } else { "d" }
        )
    }
}

/// JS `parseInt(x, 10)` (same port as collection.rs).
pub(crate) fn js_parse_int(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r') {
        i += 1;
    }
    let neg = match bytes.get(i) {
        Some(b'+') => {
            i += 1;
            false
        }
        Some(b'-') => {
            i += 1;
            true
        }
        _ => false,
    };
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    s[start..i].parse::<i64>().ok().map(|v| if neg { -v } else { v }).or(if neg { Some(i64::MIN) } else { Some(i64::MAX) })
}

/// JS `(numberQuery || '').trim().replace(/^#/, '').split('/')[0].trim()`.
fn clean_number(raw: &str) -> String {
    let t = raw.trim().trim_start_matches('#').to_string();
    t.split('/').next().unwrap_or("").trim().to_string()
}

/// Outcome of the native-gate decision.
pub enum Plan {
    Native(SearchQuery),
    /// Collection-scope free-text `q` leg (collq.rs).
    CollectionQ(Arc<collq::CollectionQ>),
    /// Collection-scope field-filter leg (this file, collection_owned_sql).
    CollectionOwned(Box<CollectionOwned>),
    /// resolveCollectionQuery's answerLocally (mode 'local', upstream mode
    /// only): every term is data-backed, the owned rows ARE the answer.
    CollectionLocalRows(Arc<collq::LocalQ>),
    /// supportsLocalCollectionQuery: otag + LOCAL_COLLECTION_OPERATORS only,
    /// answered from the tag index when isReady holds — `walk` is the walk
    /// path Node falls through to when the readiness gate fails.
    CollectionLocalTags { tags: Arc<collq::LocalQ>, walk: Arc<collq::CollectionQ> },
    /// Upstream mode only: a query Node's own parser provably rejects
    /// (route: classifyQuery throw → 400 INVALID_QUERY).
    InvalidQuery,
    Proxy,
}

/// `languages.resolve(input)` → (code, scryfall) — the two codes the q-leg
/// needs separately: `.code` keys the durable walk cache, `.scryfall` drives
/// langSearch's ` lang:xx` scoping and the non-English parenthesisation.
pub(crate) fn lang_codes(input: Option<&str>) -> (String, String) {
    let default = || (langs()[0].0.clone(), langs()[0].2.clone());
    let Some(input) = input.filter(|s| !s.is_empty()) else { return default() };
    let lower = input.trim().to_lowercase();
    for (code, name, scry) in langs() {
        if lower == *code
            || lower == name.to_lowercase()
            || (!scry.is_empty() && lower == *scry)
        {
            return (code.clone(), scry.clone());
        }
    }
    default()
}

/// Decide whether Rust may answer this GET, mirroring the branches of
/// `searchCards`/`runSearch` that Node answers from `card_cache`. Anything
/// that could reach Scryfall/the sidecar (raw `q`, `prints`, `image`, other
/// scopes) or that uses query-param shapes too odd to model (repeated keys
/// → qs produces arrays) is left to Node.
pub fn plan(user_id: i64, raw_query: Option<&str>, local_scrayfall: bool) -> Plan {
    let pairs: Vec<(String, String)> = raw_query
        .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
        .unwrap_or_default();
    let first = |k: &str| pairs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    for k in ["name", "number", "set", "q", "scope", "lang", "prints", "image", "page", "limit"] {
        if pairs.iter().filter(|(kk, _)| kk == k).count() > 1 {
            return Plan::Proxy;
        }
    }
    // `q` present and non-blank: Node takes the raw-query path (parser +
    // sidecar). A blank q is trimmed to '' and behaves as absent — same here.
    if let Some(qraw) = first("q").filter(|v| !v.trim().is_empty()) {
        // Collection scope routes to resolveCollectionQuery BEFORE the
        // field-filter path; the name-only branch is ported in collq.rs,
        // everything else (any operator syntax, other scopes) proxies.
        let scope0 = first("scope").unwrap_or_else(|| "database".to_string());
        if scope0 == "collection" {
            // Route order: classifyQuery pre-reject → answerLocally (mode
            // 'local', upstream mode only) → supportsLocalCollectionQuery →
            // walk. All three Rust-served legs live in collq.rs; anything
            // outside their accepted subsets keeps the walk/proxy contract.
            if !local_scrayfall && collq::definitely_invalid(&qraw) {
                return Plan::InvalidQuery;
            }
            let dup_count = pairs.iter().filter(|(kk, _)| kk == "count").count() > 1;
            if !dup_count {
                // maxLimit for the collection+q path is 2000 (route line 91).
                let page = first("page").and_then(|v| js_parse_int(&v)).unwrap_or(0);
                let page = if page == 0 { 1 } else { page }.max(1);
                let limit = first("limit").and_then(|v| js_parse_int(&v)).unwrap_or(0);
                let limit = if limit == 0 { 60 } else { limit }.clamp(1, 2000);
                let has_window =
                    pairs.iter().any(|(kk, _)| kk == "page" || kk == "limit");
                let include_total = first("count").as_deref() != Some("0");
                // Route line 140: `typeof query.snapshot === 'string' ?
                // query.snapshot : null` — a duplicated key parses as an
                // array in qs, so Node performs NO snapshot check anywhere
                // (and does not proxy on it).
                let dup_snapshot =
                    pairs.iter().filter(|(kk, _)| kk == "snapshot").count() > 1;
                let expected_snapshot =
                    if dup_snapshot { None } else { first("snapshot") };
                if !local_scrayfall {
                    if let Some(lq) = collq::local_plan(
                        user_id, &qraw, has_window, page, limit, include_total,
                        expected_snapshot.clone(), false,
                    ) {
                        return Plan::CollectionLocalRows(Arc::new(lq));
                    }
                }
                if let Some(walk) =
                    plan_collection_q(user_id, &pairs, &qraw, first, expected_snapshot.clone())
                {
                    if let Some(tags) = collq::local_plan(
                        user_id, &qraw, has_window, page, limit, include_total,
                        expected_snapshot, true,
                    ) {
                        return Plan::CollectionLocalTags {
                            tags: Arc::new(tags),
                            walk: Arc::new(walk),
                        };
                    }
                    return Plan::CollectionQ(Arc::new(walk));
                }
            }
        }
        return Plan::Proxy;
    }
    // Destructure default only fires when the key is absent; `scope=` (empty)
    // stays '' and is neither collection nor internet — same as Node.
    let scope = first("scope").unwrap_or_else(|| "database".to_string());
    if scope == "collection" {
        // F8-TC5: the field-filter collection leg. Node answers it with ONE
        // pure-SQL cardSearchSql.collectionQuery (runSearch line 732): owned
        // rows joined across every language the user owns them in, with
        // owned_qty from the logical-key SUM. No Scryfall, no sidecar, no
        // lang filter, and empty rows are the answer (not a fall-through).
        // `image` still gets cvScan ranking in the ROUTE after the search
        // (routes/collection.js), so that stays on Node.
        if first("image").map(|s| !s.is_empty()).unwrap_or(false) {
            return Plan::Proxy;
        }
        // Route-level parse; maxLimit stays 250 (2000 belongs to the q path).
        let page = first("page").and_then(|v| js_parse_int(&v)).unwrap_or(0);
        let page = if page == 0 { 1 } else { page }.max(1);
        let limit = first("limit").and_then(|v| js_parse_int(&v)).unwrap_or(0);
        let limit = if limit == 0 { 60 } else { limit }.clamp(1, 250);
        return Plan::CollectionOwned(Box::new(CollectionOwned {
            user_id,
            name: first("name").unwrap_or_default().trim().to_string(),
            number: clean_number(first("number").as_deref().unwrap_or("")),
            set: first("set").unwrap_or_default(),
            page,
            limit,
        }));
    }
    // scope=internet skips queryLocal (Node's `if (scope !== 'internet')`
    // gate at scryfallApi.js:752) and answers from the live merge block
    // below — lane-B iter-10 ported that leg, so internet rides Native with
    // the internet flag set. prints/image stay proxied for both scopes.
    let internet = scope == "internet";
    if first("prints").as_deref() == Some("1") {
        return Plan::Proxy; // allPrints can reach the live printing walk
    }
    if first("image").map(|s| !s.is_empty()).unwrap_or(false) {
        return Plan::Proxy; // cvScan ranking lives in Node
    }

    // page = Math.max(1, parseInt(page, 10) || 1); limit = min(250, max(1,
    // parseInt(limit,10) || 60)) — maxLimit is 250 for every scope this gate
    // admits (2000 belongs to the proxied collection+q path).
    let page = first("page").and_then(|v| js_parse_int(&v)).unwrap_or(0);
    let page = if page == 0 { 1 } else { page }.max(1);
    let limit = first("limit").and_then(|v| js_parse_int(&v)).unwrap_or(0);
    let limit = if limit == 0 { 60 } else { limit }.clamp(1, 250);

    Plan::Native(SearchQuery {
        user_id,
        name: first("name").unwrap_or_default().trim().to_string(),
        number: clean_number(first("number").as_deref().unwrap_or("")),
        set: first("set").unwrap_or_default(),
        lang_name: to_name(first("lang").as_deref()),
        page,
        limit,
        // The walked leg exists only for the name-only English branch; an
        // empty name has no query to walk (Node answers [] from the cache
        // path with no sidecar call — the build path's proxy covers it).
        walkable: !first("name").unwrap_or_default().trim().is_empty()
            && clean_number(first("number").as_deref().unwrap_or("")).is_empty()
            && first("set").unwrap_or_default().is_empty()
            && to_name(first("lang").as_deref()) == "English",
        lang: first("lang"),
        internet,
    })
}

// ---------------------------------------------------------------------------
// POST /api/search — bigJson twin + the `{ ...req.query, ...req.body }` merge
// ---------------------------------------------------------------------------

/// body-parser limit for `express.json({ limit: '15mb' })` (bytes npm: mb =
/// 1024^2). server.js:204/462.
const BIG_JSON_LIMIT: usize = 15 * 1024 * 1024;

/// body-parser `type` check for express.json's default 'json': the mime must
/// be application/json or an application/*+json suffix; anything else makes
/// the parser skip the body entirely (req.body stays undefined — the route's
/// `|| {}` turns that into no keys).
fn body_parser_is_json(ct: Option<&HeaderValue>) -> bool {
    ct.and_then(|v| v.to_str().ok())
        .map(|ct| {
            let main = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            main == "application/json"
                || (main.starts_with("application/") && main.ends_with("+json"))
        })
        .unwrap_or(false)
}

/// Outcome of the bigJson parse, pre-gate (order mirrors body-parser:
/// content-type first — a non-JSON 200 MB body is SKIPPED, not 413'd).
enum BigBody {
    /// Some(map) = JSON object properties (spread into the query);
    /// None = parser skipped / empty / array body — contributes no keys.
    Parsed(Option<serde_json::Map<String, Value>>),
    /// entity.parse.failed (incl. strict-mode scalars) → global 500.
    Invalid,
    /// entity.too.large → global 413.
    TooLarge,
}

fn parse_big_json(headers: &HeaderMap, bytes: &[u8]) -> BigBody {
    if !body_parser_is_json(headers.get(header::CONTENT_TYPE)) {
        return BigBody::Parsed(None);
    }
    if bytes.len() > BIG_JSON_LIMIT {
        return BigBody::TooLarge;
    }
    if bytes.is_empty() {
        // body-parser hands back undefined for an empty body; the route's
        // `|| {}` makes that identical to {}.
        return BigBody::Parsed(Some(serde_json::Map::new()));
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(m)) => BigBody::Parsed(Some(m)),
        // strict:true allows arrays; spreading one yields only numeric keys,
        // and searchCards reads none — same as no body.
        Ok(Value::Array(_)) => BigBody::Parsed(Some(serde_json::Map::new())),
        // scalars (incl. null) violate strict mode → SyntaxError → global 500.
        _ => BigBody::Invalid,
    }
}

/// `String(n)` for JSON numbers, JS-faithful on the shapes the native gate
/// can consume. Returns None for values deliberately left to Node
/// (0/NaN-as-falsy callers check truthiness first; |n| >= 1e21 switches to
/// JS exponential notation we do not model).
fn js_number_to_string(n: &serde_json::Number) -> Option<Option<String>> {
    // JS truthiness first: +0 and -0 are falsy, so the `if (x)` / `|| ''`
    // gates never let String() see them (checked BEFORE as_i64, which would
    // otherwise hand "0" to the LIKE param and answer a `%0%` search Node
    // runs as no-name-at-all — live probe {"name":0} vs :3312).
    if n.as_f64() == Some(0.0) {
        return Some(None); // falsy → String() never gates a clause
    }
    if let Some(i) = n.as_i64() {
        return Some(Some(i.to_string()));
    }
    let f = n.as_f64()?;
    if f.is_nan() {
        return Some(None); // falsy → String() never gates a clause
    }
    if !f.is_finite() {
        return Some(Some("Infinity".to_string())); // parseInt → NaN → defaults
    }
    if f.abs() >= 1e21 {
        return None; // "1e+21" exponential form — exotic, leave to Node
    }
    if f.fract() == 0.0 {
        return Some(Some(format!("{}", f as i64)));
    }
    Some(Some(format!("{f}"))) // shortest round-trip, same as JS
}

/// Merge lookup for POST: a JSON-object body property wins over the FIRST
/// query value for the key (`{...query, ...body}`), otherwise the qs
/// first-value rule applies. Returns Err for value shapes deliberately left
/// to Node (arrays/objects on stringified keys, |n| >= 1e21).
enum Merged<'a> {
    Query(&'a str),
    Str(String),
    /// falsy JSON value (null / false / 0 / NaN) — JS-gated keys treat it
    /// exactly like an absent/empty one.
    Falsy,
    Exotic,
}

/// JS-semantics gate for the searchCards field-filter path:
/// `if (value)` truthiness plus the `String(value)` SQL parameter.
/// Returns the parameter string ('' = no clause). Err = exotic shape.
fn merged_param(m: Option<Merged<'_>>) -> Result<String, ()> {
    match m {
        None => Ok(String::new()),
        Some(Merged::Query(s)) => Ok(s.to_string()),
        Some(Merged::Str(s)) => Ok(s),
        // falsy (null/false/0/NaN) or absent: `if (x)` gate never opens, and
        // the query-string empty case is gated by the caller's is_empty().
        Some(Merged::Falsy) => Ok(String::new()),
        Some(Merged::Exotic) => Err(()),
    }
}

/// Decide whether Rust may answer a POST, mirroring `searchCards`'
/// `query = { ...req.query, ...req.body }` merge on top of the GET gate.
/// Deliberate exclusions (proxy): a non-blank `q` — the collq/sidecar q-leg
/// twin is pinned for GET only; `image` strings (cvScan ranking lives in
/// Node); `prints=1`; collection/internet scopes; duplicate query keys the
/// body does not override; and exotic body shapes (arrays/objects, huge
/// floats) on the stringified keys.
pub fn plan_post(
    user_id: i64,
    raw_query: Option<&str>,
    body: Option<&serde_json::Map<String, Value>>,
) -> Plan {
    let pairs: Vec<(String, String)> = raw_query
        .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
        .unwrap_or_default();
    let bq = |k: &str| body.and_then(|b| b.get(k));
    let qq = |k: &str| pairs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.as_str());
    let mv = |k: &str| -> Option<Merged<'_>> {
        match bq(k) {
            Some(Value::String(s)) => Some(Merged::Str(s.clone())),
            Some(Value::Null) | Some(Value::Bool(false)) => Some(Merged::Falsy),
            Some(Value::Bool(true)) => Some(Merged::Str("true".to_string())),
            Some(v @ Value::Number(_)) => {
                match js_number_to_string(v.as_number().unwrap()) {
                    Some(Some(s)) => Some(Merged::Str(s)),
                    Some(None) => Some(Merged::Falsy),
                    None => Some(Merged::Exotic),
                }
            }
            Some(_) => Some(Merged::Exotic), // array / object
            None => qq(k).map(Merged::Query),
        }
    };
    for k in ["name", "number", "set", "q", "scope", "lang", "prints", "image", "page", "limit"] {
        // A body property replaces the whole key, so repeated query keys
        // stop mattering once the body defines it (the spread wins).
        if pairs.iter().filter(|(kk, _)| kk == k).count() > 1 && bq(k).is_none() {
            return Plan::Proxy;
        }
    }

    // q: `typeof query.q === 'string' ? query.q.trim() : ''`. A non-blank q
    // sends Node down the parser/sidecar path — pinned to GET, so proxy.
    match mv("q") {
        Some(Merged::Str(s)) if !s.trim().is_empty() => return Plan::Proxy,
        Some(Merged::Query(s)) if !s.trim().is_empty() => return Plan::Proxy,
        _ => {}
    }

    // scope: only the raw === 'collection' / 'internet' comparisons gate;
    // any other value (non-strings included) behaves like 'database'.
    let scope_is = |t: &str| match bq("scope") {
        Some(Value::String(s)) => s == t,
        Some(_) => false,
        None => qq("scope") == Some(t),
    };
    if scope_is("collection") || scope_is("internet") {
        return Plan::Proxy;
    }
    let prints_is_1 = match bq("prints") {
        Some(Value::String(s)) => s == "1",
        Some(_) => false,
        None => qq("prints") == Some("1"),
    };
    if prints_is_1 {
        return Plan::Proxy;
    }
    // image: `image && typeof image === 'string' && image.length > 0`.
    let image_leg = match bq("image") {
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => false,
        None => qq("image").map(|s| !s.is_empty()).unwrap_or(false),
    };
    if image_leg {
        return Plan::Proxy;
    }

    // Stringified-with-truthiness-gate keys: name/number/set are consumed by
    // runSearch as `(x || '').trim()` — any TRUTHY non-string (number, true,
    // array, object) throws `.trim is not a function` inside the route's try
    // and the catch answers the generic 500 {"error":"Search failed"}. Rust
    // must not answer those natively: whole-request forward lets Node's own
    // 500 ride verbatim. Falsy non-strings (0/-0/false/null) still take the
    // `|| ''` leg; page/limit parseInt ToString's numbers without crashing;
    // languages.resolve String()'s lang (utils/languages.js:28).
    let node_crashes = |k: &str| match bq(k) {
        Some(Value::String(_)) | Some(Value::Null) | None => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map_or(true, |f| f != 0.0),
        Some(_) => true, // array / object
    };
    if ["name", "number", "set"].iter().any(|k| node_crashes(k)) {
        return Plan::Proxy;
    }

    // page = Math.max(1, parseInt(page, 10) || 1); limit = min(250, max(1,
    // parseInt(limit, 10) || 60)) — parseInt is ToString-first, so numbers
    // stringify exactly like JS before the digit scan.
    let pi = |k: &str| -> Option<Option<i64>> {
        match mv(k) {
            None => Some(None), // absent key: parseInt(undefined) → NaN → ||-default
            Some(Merged::Query(s)) => Some(js_parse_int(s)),
            Some(Merged::Str(s)) => Some(js_parse_int(&s)),
            Some(Merged::Falsy) => Some(None), // parseInt("null"/"false"/"0"…) gates to ||-default
            Some(Merged::Exotic) => None,      // parseInt([5]) etc. — exotic
        }
    };
    let page = match pi("page") {
        Some(v) => v.unwrap_or(0),
        None => return Plan::Proxy,
    };
    let page = if page == 0 { 1 } else { page }.max(1);
    let limit = match pi("limit") {
        Some(v) => v.unwrap_or(0),
        None => return Plan::Proxy,
    };
    let limit = if limit == 0 { 60 } else { limit }.clamp(1, 250);

    let name = match merged_param(mv("name")) {
        Ok(s) => s,
        Err(()) => return Plan::Proxy,
    };
    let number_raw = match merged_param(mv("number")) {
        Ok(s) => s,
        Err(()) => return Plan::Proxy,
    };
    let set = match merged_param(mv("set")) {
        Ok(s) => s,
        Err(()) => return Plan::Proxy,
    };
    // languages.resolve(): falsy → DEFAULT; else String(input).trim().lower.
    let lang_name = match mv("lang") {
        None => to_name(None),
        Some(Merged::Falsy) => to_name(None),
        Some(Merged::Query(s)) => to_name(Some(s)),
        Some(Merged::Str(s)) => to_name(Some(&s)),
        Some(Merged::Exotic) => return Plan::Proxy,
    };

    // Walk leg: strings only (numbers stringify into a LIKE param but Node's
    // sidecar leg for them is unproven — conservative: never walk those).
    let all_strings = ["name", "number", "set", "lang"]
        .iter()
        .all(|k| matches!(mv(k), None | Some(Merged::Query(_)) | Some(Merged::Str(_)) | Some(Merged::Falsy)));
    let walkable = all_strings
        && !name.trim().is_empty()
        && clean_number(&number_raw).is_empty()
        && set.is_empty()
        && lang_name == "English";

    Plan::Native(SearchQuery {
        user_id,
        name: name.trim().to_string(),
        number: clean_number(&number_raw),
        set,
        lang_name,
        page,
        limit,
        walkable,
        // plan_post proxies scope=collection/internet wholesale, so the
        // merge leg here is always the database scope. Raw lang for the
        // langSearch resolve + rawPageCache key (`lang || ''`): Falsy shapes
        // key as '' (== absent), Exotic already returned Plan::Proxy above.
        internet: false,
        lang: match mv("lang") {
            Some(Merged::Query(s)) => Some(s.to_string()),
            Some(Merged::Str(s)) => Some(s),
            _ => None,
        },
    })
}

/// Gate + normalization for the collection-scope q leg
/// (`searchCards`' `scope === 'collection' && q.trim()` branch). Returns
/// None (→ proxy) whenever anything outside the ported name + operator-
/// conjunction subset is involved (duplicate `count` keys carry qs array
/// semantics). The `snapshot` parameter is carried natively since iter-16:
/// the string lands in `expected_snapshot` (a duplicated key is a qs array,
/// which Node reads as `null` → no check).
fn plan_collection_q(
    user_id: i64,
    pairs: &[(String, String)],
    qraw: &str,
    first: impl Fn(&str) -> Option<String>,
    expected_snapshot: Option<String>,
) -> Option<collq::CollectionQ> {
    if pairs.iter().filter(|(k, _)| k == "count").count() > 1 {
        return None;
    }
    let walk_base = collq::walk_base(qraw)?;
    let (lang_code, scry_code) = lang_codes(first("lang").as_deref());
    // Route-level parsing, collection+q variant: maxLimit is 2000 here.
    let page = first("page").and_then(|v| js_parse_int(&v)).unwrap_or(0);
    let page = if page == 0 { 1 } else { page }.max(1);
    let limit = first("limit").and_then(|v| js_parse_int(&v)).unwrap_or(0);
    let limit = if limit == 0 { 60 } else { limit }.clamp(1, 2000);
    let has_window = pairs.iter().any(|(k, _)| k == "page" || k == "limit");
    let include_total = first("count").as_deref() != Some("0");
    Some(collq::CollectionQ { user_id, walk_base, lang_code, scry_code, has_window, page, limit, include_total, expected_snapshot })
}

// ---------------------------------------------------------------------------
// localCacheQuery + parseCardRow + attachOwnedQty
// ---------------------------------------------------------------------------

/// `cardSearchSql.localCacheQuery` — same SQL text, same parameter order.
/// `candidates_json` (from the name index, when honoured) is a JSON array of
/// rowids: the predicate only *narrows the scan* — the LIKE clause stays, so
/// results are identical for a correct candidate set — and `ORDER BY rowid`
/// pins the output to the rowid-ascending order the plain full scan (the
/// proven plan: `SCAN card_cache`, no usable index) returns rows in.
fn local_cache_query(p: &SearchQuery, candidates_json: Option<&str>) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value as Sql;
    let mut sql = String::from("SELECT * FROM card_cache WHERE language = ?");
    let mut params = vec![Sql::Text(p.lang_name.clone())];
    if !p.name.is_empty() {
        sql.push_str(" AND (name LIKE ? OR printed_name LIKE ?)");
        let like = format!("%{}%", p.name);
        params.push(Sql::Text(like.clone()));
        params.push(Sql::Text(like));
    }
    if !p.number.is_empty() {
        sql.push_str(" AND number = ?");
        params.push(Sql::Text(p.number.clone()));
    }
    // parseSetList + setSqlFilter: any of the comma-separated sets, by
    // set_name LIKE or exact set_id.
    let set_list: Vec<String> = p.set.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if !set_list.is_empty() {
        let clause = set_list.iter().map(|_| "(set_name LIKE ? OR set_id = ?)").collect::<Vec<_>>().join(" OR ");
        sql.push_str(&format!(" AND ({clause})"));
        for s in &set_list {
            params.push(Sql::Text(format!("%{s}%")));
            params.push(Sql::Text(s.clone()));
        }
    }
    if let Some(cj) = candidates_json {
        sql.push_str(" AND rowid IN (SELECT value FROM json_each(?))");
        params.push(Sql::Text(cj.to_string()));
        sql.push_str(" ORDER BY rowid");
    }
    // offset = (page-1)*limit, saturating: JS computes it as a float and the
    // driver clamps; i64 saturating math keeps the same "past the end"
    // answer (empty rows) instead of wrapping.
    let offset = (p.page - 1).saturating_mul(p.limit);
    sql.push_str(" LIMIT ? OFFSET ?");
    params.push(Sql::Integer(p.limit));
    params.push(Sql::Integer(offset));
    (sql, params)
}

// ---------------------------------------------------------------------------
// collectionQuery (cardSearchSql.js) — the scope=collection field-filter leg
// ---------------------------------------------------------------------------

/// JS `\s` exactly (regex-crate `\s` also matches U+0085, JS's does not, and
/// JS's includes U+FEFF, Unicode White_Space does not).
const JS_WS_CLASS: &str = r"[\t\n\x0b\x0c\r\x20\x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]";

fn number_re() -> &'static regex::Regex {
    static R: OnceLock<Option<regex::Regex>> = OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(&format!(
            r"(?i)^#?([A-Z0-9★-]+)(?:{JS_WS_CLASS}*/{JS_WS_CLASS}*)?$"
        ))
        .ok()
    })
    .as_ref()
    .unwrap()
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `cardSearchSql.numberClause` — same accept regex (JS `/i`), the same
/// leading-zero strip, and the same OR-list of exact / cleaned / stripped /
/// CAST-numeric terms. Operates on the already-normalized number (runSearch's
/// clean_number ran first), so its own `String(number || '').trim()` is the
/// no-op it is in JS.
fn number_clause(column: &str, number: &str) -> Option<(String, Vec<String>)> {
    let exact = number.trim().to_string();
    if exact.is_empty() {
        return None;
    }
    let clean = number_re()
        .captures(&exact)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| exact.clone());
    let stripped = clean.trim_start_matches('0').to_string();
    let mut terms = vec![format!("{column} = ?")];
    let mut params = vec![exact.clone()];
    if clean != exact {
        terms.push(format!("{column} = ?"));
        params.push(clean.clone());
    }
    if stripped != clean && !stripped.is_empty() && stripped != exact {
        terms.push(format!("{column} = ?"));
        params.push(stripped);
    }
    if all_digits(&clean) || all_digits(&exact) {
        terms.push(format!("CAST({column} AS INTEGER) = CAST(? AS INTEGER)"));
        params.push(clean);
    }
    Some((format!("({})", terms.join(" OR ")), params))
}

/// `cardSearchSql.collectionQuery` — same SQL text, same filter order
/// (name, number, sets) and parameter order. `sqlCardKey('owned_cc')` is
/// expanded inline (`LOWER(TRIM(COALESCE(owned_cc.name, '')))`), mirroring
/// the JS helper call.
fn collection_owned_sql(o: &CollectionOwned) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value as Sql;
    let mut filters: Vec<(String, Vec<String>)> = Vec::new();
    // nameClause('owned.', name)
    if !o.name.is_empty() {
        let like = format!("%{}%", o.name);
        filters.push((
            "(owned.name LIKE ? OR owned.printed_name LIKE ?)".to_string(),
            vec![like.clone(), like],
        ));
    }
    // numberClause('owned.number', number)
    if let Some((clause, params)) = number_clause("owned.number", &o.number) {
        filters.push((clause, params));
    }
    // parseSetList + setSqlFilter(setList, 'owned')
    let set_list: Vec<String> =
        o.set.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if !set_list.is_empty() {
        let inner = set_list
            .iter()
            .map(|_| "(owned.set_name LIKE ? OR owned.set_id = ?)")
            .collect::<Vec<_>>()
            .join(" OR ");
        let params = set_list.iter().flat_map(|s| [format!("%{s}%"), s.clone()]).collect();
        filters.push((format!("({inner})"), params));
    }
    let matching_filter = if filters.is_empty() {
        String::new()
    } else {
        format!(
            " FILTER (WHERE {})",
            filters.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>().join(" AND ")
        )
    };
    let sql = format!("
    WITH owned_rows AS (
      SELECT
        c.card_id,
        c.quantity,
        LOWER(TRIM(COALESCE(owned_cc.name, ''))) AS card_key,
        owned_cc.name,
        owned_cc.printed_name,
        owned_cc.number,
        owned_cc.set_name,
        owned_cc.set_id
      FROM collection c
      JOIN card_cache owned_cc ON c.card_id = owned_cc.id
      WHERE c.user_id = ? AND c.quantity > 0
    ),
    logical_owned AS (
      SELECT
        card_key,
        SUM(quantity) AS owned_qty,
        json_group_array(DISTINCT card_id){matching_filter} AS matching_card_ids
      FROM owned_rows owned
      GROUP BY card_key
    ),
    matched AS (
      SELECT owned_printing.value AS card_id, logical_owned.owned_qty
      FROM logical_owned
      JOIN json_each(logical_owned.matching_card_ids) owned_printing
      ORDER BY owned_printing.value
      LIMIT ? OFFSET ?
    )
    SELECT cc.*, matched.owned_qty
    FROM matched
    JOIN card_cache cc ON matched.card_id = cc.id
    ORDER BY matched.card_id
  ");
    let mut params = vec![Sql::Integer(o.user_id)];
    params.extend(filters.iter().flat_map(|(_, p)| p.iter()).map(|p| Sql::Text(p.clone())));
    params.push(Sql::Integer(o.limit));
    params.push(Sql::Integer((o.page - 1).saturating_mul(o.limit)));
    (sql, params)
}

/// One collection-leg build: run the query, shape rows with the parseCardRow
/// twin. Node wraps the WHOLE route in try/catch → any SQL error is a 500
/// 'Search failed' (the collection branch returns before runSearch's
/// live-path try, so there is no fall-through to swallow it).
fn build_owned_body(pool: &DbPool, o: &CollectionOwned) -> Result<Vec<u8>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    let (sql, params) = collection_owned_sql(o);
    let mut stmt = conn.prepare_cached(&sql).map_err(|e| format!("prepare: {e}"))?;
    let mut rows = stmt
        .query(rusqlite::params_from_iter(params.iter()))
        .map_err(|e| format!("collection query: {e}"))?;
    let mut out: Vec<Value> = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        out.push(Value::Object(row_to_object(row)?));
    }
    serde_json::to_vec(&out).map_err(|e| e.to_string())
}

/// Raw rows → the `parseCardRow` object shape: every column in table order
/// (`SELECT *`, so the statement's own column list is the contract), with
/// the JSON array columns parsed; a missing array column is appended, which
/// is what the JS object spread does.
fn row_to_object(row: &Row) -> Result<Map<String, Value>, String> {
    let ncols = row.as_ref().column_count();
    let names: Vec<String> =
        (0..ncols).map(|i| row.as_ref().column_name(i).unwrap_or("").to_string()).collect();
    let mut obj = Map::with_capacity(ncols + 3);
    for (i, name) in names.iter().enumerate() {
        let v = sqlite_value(row, i).map_err(|e| e.to_string())?;
        let v = match name.as_str() {
            "subtypes" | "types" | "color_identity" => parse_array_col(&v)?,
            _ => v,
        };
        obj.insert(name.clone(), v);
    }
    for name in ["subtypes", "types", "color_identity"] {
        if !obj.contains_key(name) {
            obj.insert(name.to_string(), parse_array_col(&Value::Null)?);
        }
    }
    Ok(obj)
}

/// `attachOwnedQty` from routes/collection.js — verbatim SQL with the
/// `sqlCardKey` helper expanded (`LOWER(TRIM(COALESCE(alias.name, '')))`).
/// Assigns `owned_qty` on each card (appended key, JS assignment).
fn attach_owned_qty(conn: &Connection, cards: &mut [Value], user_id: i64) -> Result<(), String> {
    if cards.is_empty() {
        return Ok(());
    }
    // targets: card_id = card.card_id || card.id, name = card.name (JS ||).
    let targets: Vec<Map<String, Value>> = cards
        .iter()
        .map(|c| {
            let o = c.as_object();
            let card_id = o
                .and_then(|m| m.get("card_id"))
                .filter(|v| truthy(v))
                .cloned()
                .or_else(|| o.and_then(|m| m.get("id")).cloned())
                .unwrap_or(Value::Null);
            let name = o
                .and_then(|m| m.get("name"))
                .filter(|v| truthy(v))
                .cloned()
                .unwrap_or(Value::Null);
            Map::from_iter([("card_id".to_string(), card_id), ("name".to_string(), name)])
        })
        .collect();
    let targets_json = serde_json::to_string(&targets).map_err(|e| e.to_string())?;

    let sql = "
     WITH target_cards AS (
       SELECT
         CAST(key AS INTEGER) AS target_index,
         json_extract(value, '$.card_id') AS card_id,
         json_extract(value, '$.name') AS name
       FROM json_each(?)
     ),
     target_identity AS (
       SELECT
         target_cards.target_index,
         COALESCE(
           NULLIF(LOWER(TRIM(COALESCE(cached_target.name, ''))), ''),
           NULLIF(LOWER(TRIM(target_cards.name)), '')
         ) AS card_key
       FROM target_cards
       LEFT JOIN card_cache cached_target
         ON cached_target.id = target_cards.card_id
     ),
     owned AS MATERIALIZED (
       SELECT LOWER(TRIM(COALESCE(owned_cc.name, ''))) AS card_key, SUM(c.quantity) AS qty
       FROM collection c
       JOIN card_cache owned_cc ON owned_cc.id = c.card_id
       WHERE c.user_id = ? AND c.quantity > 0
       GROUP BY LOWER(TRIM(COALESCE(owned_cc.name, '')))
     )
     SELECT target_identity.target_index, COALESCE(owned.qty, 0) AS qty
     FROM target_identity
     LEFT JOIN owned ON owned.card_key = target_identity.card_key
     ORDER BY target_identity.target_index";
    let mut stmt = conn.prepare_cached(sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query(rusqlite::params![targets_json, user_id]).map_err(|e| e.to_string())?;
    let mut owned: HashMap<i64, Value> = HashMap::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let idx = row.get::<_, i64>(0).map_err(|e| e.to_string())?;
        let qty = sqlite_value(row, 1).map_err(|e| e.to_string())?;
        owned.insert(idx, qty);
    }
    for (i, card) in cards.iter_mut().enumerate() {
        // `owned.get(i) || 0` — falsy (absent, 0, null) collapses to 0.
        let qty = owned.get(&(i as i64)).filter(|v| truthy(v)).cloned().unwrap_or(Value::from(0));
        if let Some(obj) = card.as_object_mut() {
            obj.insert("owned_qty".to_string(), qty);
        }
    }
    Ok(())
}

/// One native cache miss: run the local query (rows found is a precondition
/// checked by the caller via the same call), shape the cards, attach owned
/// quantities, serialize. `Ok(None)` means "Node would not answer from the
/// cache" — the caller must proxy. Any local-cache *query* failure also maps
/// to `Ok(None)` because `runSearch` swallows it into an empty result and
/// falls through to the live path; only post-cache (attachOwnedQty/serialize)
/// errors mirror Node's 500, surfaced as `Err`.
fn build_body(
    pool: &DbPool,
    idx: &NameIndex,
    owned: &OwnedCache,
    p: &SearchQuery,
) -> Result<Option<(Vec<u8>, Option<u64>, Vec<Value>)>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    // Pin one read transaction over probe + card query + owned attach: WAL
    // fixes every statement's view at the probe, so the revision we compare
    // snapshot revs against is the state the response is built from
    // (name_index.rs documents why that equality is sufficient). BEGIN
    // failure proxies (same as a query error).
    let txn = match Txn::begin(&conn) {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(error = %e, "search build: BEGIN failed → proxy");
            return Ok(None);
        }
    };
    let now = crate::db::current_revision(&conn).ok().flatten();
    // Name-index fast path: any miss (cold index, metacharacter pattern,
    // revision moved mid-build) falls through to today's plain scan.
    let mut candidates_json: Option<String> = None;
    if !p.name.is_empty() {
        if let Some(pat) = name_index::indexable_pattern(&p.name) {
            if let Some(r) = now {
                idx.ensure_fresh(pool, r);
                if let Some(snap) = idx.snapshot_for(r) {
                    let ids = snap.candidate_rowids(&pat);
                    tracing::debug!(name = %p.name, candidates = ids.len(), "search build: name index");
                    if ids.is_empty() {
                        return Ok(None); // LIKE finds nothing either → Node's live path
                    }
                    let mut cj = String::with_capacity(ids.len() * 8 + 2);
                    cj.push('[');
                    for (i, id) in ids.iter().enumerate() {
                        if i > 0 {
                            cj.push(',');
                        }
                        cj.push_str(&id.to_string());
                    }
                    cj.push(']');
                    candidates_json = Some(cj);
                }
            }
        }
    }
    let (sql, params) = local_cache_query(p, candidates_json.as_deref());
    let mut cards: Vec<Value> = {
        let mut stmt = conn.prepare_cached(&sql).map_err(|e| format!("prepare: {e}"))?;
        let mut rows = match stmt.query(rusqlite::params_from_iter(params.iter())) {
            Ok(r) => r,
            // runSearch: `catch (e) { localResults = []; }` → live path.
            Err(e) => {
                tracing::debug!(error = %e, sql = %sql, "search build: cache query failed → proxy");
                return Ok(None);
            }
        };
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|_| "cache scan failed".to_string())? {
            out.push(Value::Object(row_to_object(row)?));
        }
        out
    };
    if cards.is_empty() {
        return Ok(None); // Node keeps going: specific/broad Scryfall query.
    }
    // Stale background refresh (scryfallApi.js queryLocal → stale filter +
    // floating bulkFetchByIdentifier, CACHE_AGE_LIMIT_MS = 3 days): rows
    // older than 3 days get re-fetched AFTER the cached answer is served.
    // Unparseable/NULL last_updated is NaN in Node's comparison → never
    // stale, hence unwrap_or(false). The identifier rows ride to `handle`,
    // which spawns the fetch — Node re-fires per request while rows stay
    // stale; the response cache serves this entry only until the refresh's
    // card_cache write bumps the revision (trigger) and the entry rebuilds
    // on fresh rows, which bounds the divergence to the failing-fetch case.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let stale: Vec<Value> = cards
        .iter()
        .filter(|c| {
            c.get("last_updated")
                .and_then(|v| v.as_str())
                .and_then(crate::card_cache::parse_sqlite_utc)
                .map(|at| now_ms - at.timestamp_millis() > 3 * 24 * 60 * 60 * 1000)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    // Owned quantities from the per-rev snapshot when it holds; any
    // deviation (row shape, non-integer SUM, build race) → original SQL.
    let mut attached = false;
    if let Some(r) = now {
        if let Some(map) = owned.owned_map(pool, p.user_id, r) {
            attached = name_index::attach_owned_from_snapshot(&mut cards, &map);
        }
    }
    if !attached {
        attach_owned_qty(&conn, &mut cards[..], p.user_id)?;
    }
    txn.commit().map_err(|e| e.to_string())?;
    let body = serde_json::to_vec(&cards).map_err(|e| e.to_string())?;
    // This path never sets meta.total (only the live paths do) → no
    // X-Total-Count header on the native response.
    Ok(Some((body, None, stale)))
}

/// `bulkFetchByIdentifier` (scryfallApi.js) as the queryLocal background
/// refresh uses it: 75-identifier batches POSTed to `/cards/collection`
/// through the shared Scry client (sidecar swap, paced upstream), then
/// normalizeCard + cacheCards. Identifier pick mirrors Node: scryfallUuid
/// → {id}, else set_id+number → {set, collector_number}, else {name}.
/// `pairs`/`not_found` are unused on this leg and dropped. A transport
/// failure after the retries is Node's caught error — log (its message
/// verbatim) and abandon the whole refresh.
async fn bulk_refresh_stale(st: &AppState, rows: &[Value]) {
    const COLLECTION_BATCH: usize = 75;
    let Some(pool) = st.db.as_ref().map(Arc::clone) else { return };
    let lang_name = to_name(None);
    for chunk in rows.chunks(COLLECTION_BATCH) {
        let identifiers: Vec<Value> = chunk
            .iter()
            .map(|row| {
                let id = row
                    .get("id")
                    .and_then(|v| v.as_str())
                    .or_else(|| row.get("card_id").and_then(|v| v.as_str()));
                if let Some(uuid) = crate::card_norm::scryfall_uuid(id) {
                    return json!({ "id": uuid });
                }
                let set_id = match row.get("set_id") {
                    Some(v) if !v.is_null() => crate::mfx_payload::js_to_string(v).to_lowercase(),
                    _ => String::new(),
                };
                let num = match row.get("number") {
                    Some(v) if !v.is_null() => crate::mfx_payload::js_to_string(v),
                    _ => String::new(),
                };
                if !set_id.is_empty() && !num.is_empty() {
                    json!({ "set": set_id, "collector_number": num })
                } else {
                    json!({ "name": row.get("name").and_then(|v| v.as_str()).unwrap_or("") })
                }
            })
            .collect();
        let mut body = serde_json::Map::new();
        body.insert("identifiers".into(), Value::Array(identifiers));
        let resp = st
            .scry
            .post_retried(
                "/cards/collection",
                Value::Object(body),
                &crate::scryfall::ScryOpts::default(),
                4,
            )
            .await;
        let data = match resp {
            Ok(Some(v)) => v
                .get("data")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
            // Node guards `resp.data` (a non-JSON answer carries no cards for
            // this batch); a retry-exhausted transport is the caught-error.
            Ok(None) => Vec::new(),
            Err(e) => {
                tracing::error!("MTG background refresh failed: {e}");
                return;
            }
        };
        if data.is_empty() {
            continue;
        }
        let maps: Vec<serde_json::Map<String, Value>> =
            data.iter().map(|raw| crate::sidecar::normalize_card(raw, &lang_name)).collect();
        let pool_w = Arc::clone(&pool);
        let wrote = tokio::task::spawn_blocking(move || -> Result<(), String> {
            let conn = pool_w.get().map_err(|e| e.to_string())?;
            crate::card_cache::cache_normalized_cards(&conn, &maps).map_err(|e| e.to_string())
        })
        .await;
        if let Err(e) = wrote {
            tracing::error!("MTG background refresh failed: {e:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

/// The q-leg's X-Catalog-* surface (route lines 142–165 of
/// backend/src/routes/collection.js), frozen at build time.
pub(crate) struct CatalogMeta {
    /// X-Total-Count when include_total; None omits the header.
    total: Option<String>,
    complete: bool,
    upstream_total: Option<String>,
    status: &'static str,
    snapshot: String,
}

pub struct SearchEntry {
    rev: i64,
    raw: Bytes,
    gz: Bytes,
    etag: String,
    /// Walked entries carry the broad query's total (X-Total-Count) and the
    /// Node source label; cache-path entries are (None, "cache").
    total: Option<u64>,
    source: &'static str,
    /// False on the collection field-filter leg: Node's route sets NEITHER
    /// X-Source NOR Access-Control-Expose-Headers there (runSearch leaves
    /// meta.source unset and meta.total null), so the response carries only
    /// the Express-default ETag + Content-Type.
    x_source: bool,
    /// Set only on the collection-scope q leg (collq.rs): switches the
    /// response to the resolveCollectionQuery header contract.
    catalog: Option<CatalogMeta>,
    /// Cache-path entries built while some rows were >3 days old carry their
    /// identifier rows so EVERY request served from the entry (hit or miss)
    /// re-fires the background refresh, matching Node's per-request stale
    /// filter while the fetch has not yet landed (queryLocal, scryfallApi.js).
    stale: Arc<Vec<Value>>,
}

/// Single-flight slot, same shape as the collection/stats caches: every
/// concurrent miss on one key shares a single build (and sidecar walk).
/// Without it, the every-2s write-mix invalidation stampede re-runs the
/// query — and re-pages the ~30 ms/page sidecar — once per in-flight request
/// on the same key (iter-7 realistic p99 sat at ~1 s because of this).
/// `Ok(Some(entry))` = serve; `Ok(None)` = Node owns this leg, every waiter
/// proxies; `Err` = 500.
type ComputeSlot = OnceCell<Result<Option<Arc<SearchEntry>>, String>>;

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Arc<SearchEntry>>,
    inflight: HashMap<String, Arc<ComputeSlot>>,
    /// LRU clock per key + the per-key stored-bytes estimate; eviction
    /// (byte budget, Gate D iter-22) picks victims from these.
    seen: HashMap<String, u64>,
    bytes: HashMap<String, u64>,
    total_bytes: u64,
    /// Highest revision any entry was stored at — entries from older
    /// generations are dead weight (`get` filters by exact rev) and get
    /// pruned on the next insert, mirroring the collection cache.
    max_rev: i64,
}

/// Default byte budget for the serialized search bodies. The count cap
/// alone (>64 → clear) never bounded BYTES: an entry is raw+gz body plus
/// stale identifier rows, and the realistic bench (64 varied queries)
/// ratcheted the process VmHWM +694 MB through this one cache
/// (lane-B iter-22 per-leg attribution). Env-tunable like the DB pools.
const SEARCH_STORE_MAX_BYTES_DEFAULT: u64 = 128 << 20; // 128 MiB
/// Heap estimate per stale identifier row (small objects; errs generous).
const STALE_ROW_BYTES: u64 = 1024;

fn search_store_max_bytes() -> u64 {
    std::env::var("SCRYBOX_SEARCH_CACHE_MAX_MB").ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .map(|mb| mb << 20)
        .unwrap_or(SEARCH_STORE_MAX_BYTES_DEFAULT)
}

/// Serialized native search responses keyed by (params, revision). Bounded
/// by BOTH the 64-entry count cap and a global byte budget (LRU eviction,
/// same shape as the collection cache — the old "bounded the same way"
/// comment promised more than the code delivered).
pub struct SearchCache {
    inner: Mutex<Inner>,
    /// Number of full bodies actually computed (single-flight test + ops).
    computes: AtomicU64,
    clock: AtomicU64,
    max_bytes: AtomicU64,
    /// Freed bytes (pruned/evicted) in one `insert` before the trim timer is
    /// asked to return heap to the OS — same shape as the collection cache's
    /// `trim_dirty` (Gate D iter-24: the search legs were the only big
    /// churning caches without a trim path; iter-22 held RSS ~312 MB above
    /// the retained store after the search_db leg).
    trim_dirty_bytes: AtomicU64,
    trim_dirty: AtomicBool,
    /// Gate D iter-25: cold-recompute budget, twin of the collection
    /// cache's `builds` (iter 15). Single-flight coalesces ONE compute per
    /// key, but N distinct cold keys (boot warm, rev-bump fan-out, varied
    /// realistic mix) each spawned an unbounded blocking build. The permit
    /// is acquired INSIDE the OnceCell init, around the heavy leg only —
    /// same-key waiters never consume one, and the sidecar-walk leg
    /// (network-bound) stays ungated so walks never queue behind CPU.
    builds: Arc<Semaphore>,
}

/// Concurrent cold search computes allowed across all search legs
/// (database build, collection-owned, collq). Matches the collection
/// cache's build budget.
const BUILD_BUDGET: usize = 2;

impl Default for SearchCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            computes: AtomicU64::new(0),
            clock: AtomicU64::new(0),
            max_bytes: AtomicU64::new(search_store_max_bytes()),
            trim_dirty_bytes: AtomicU64::new(crate::collection::TRIM_DIRTY_BYTES),
            trim_dirty: AtomicBool::new(false),
            builds: Arc::new(Semaphore::new(BUILD_BUDGET)),
        }
    }
    fn get(&self, key: &str, rev: i64) -> Option<Arc<SearchEntry>> {
        let mut e = self.inner.guard();
        let hit = e.entries.get(key).filter(|x| x.rev == rev).cloned();
        if hit.is_some() {
            // Serve = most-recently-used; byte-budget eviction picks its
            // victim from this clock.
            let seen = self.clock.fetch_add(1, Ordering::Relaxed);
            e.seen.insert(key.to_string(), seen);
        }
        hit
    }
    fn insert(&self, key: String, entry: SearchEntry) -> Arc<SearchEntry> {
        let entry = Arc::new(entry);
        let bytes = entry.raw.len() as u64
            + entry.gz.len() as u64
            + entry.stale.len() as u64 * STALE_ROW_BYTES;
        let seen = self.clock.fetch_add(1, Ordering::Relaxed);
        let mut freed: u64 = 0;
        {
            let mut e = self.inner.guard();
            if e.entries.len() > 64 {
                freed += e.total_bytes;
                e.entries.clear();
                e.seen.clear();
                e.bytes.clear();
                e.total_bytes = 0;
            }
            // Superseded-generation entries are never served (get filters by
            // exact rev) — drop their bodies instead of counting them.
            if entry.rev > e.max_rev {
                e.max_rev = entry.rev;
            }
            e.total_bytes += bytes;
            e.bytes.insert(key.clone(), bytes);
            e.seen.insert(key.clone(), seen);
            e.entries.insert(key.clone(), entry.clone());
            if e.entries.len() > 1 {
                let live = e.max_rev;
                let dead: Vec<String> = e.entries.iter()
                    .filter(|(_, v)| v.rev != live)
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in dead {
                    e.entries.remove(&k);
                    if let Some(b) = e.bytes.remove(&k) {
                        freed += b;
                        e.total_bytes = e.total_bytes.saturating_sub(b);
                    }
                    e.seen.remove(&k);
                }
            }
            // Byte budget: evict least-recently-served until under budget,
            // always keeping the entry just inserted.
            let max = self.max_bytes.load(Ordering::Relaxed);
            while e.total_bytes > max && e.entries.len() > 1 {
                let victim = e.seen.iter()
                    .filter(|(k, _)| e.entries.contains_key(*k) && **k != key)
                    .min_by_key(|(_, s)| **s)
                    .map(|(k, _)| k.clone());
                match victim {
                    Some(k) => {
                        e.entries.remove(&k);
                        if let Some(b) = e.bytes.remove(&k) {
                            freed += b;
                            e.total_bytes = e.total_bytes.saturating_sub(b);
                        }
                        e.seen.remove(&k);
                    }
                    None => break,
                }
            }
        }
        // Big drops (generation prune, budget eviction, count-cap clear)
        // leave multi-MB blocks free in glibc's arenas; arm the deferred
        // malloc_trim so the timer — never a serving thread — hands them
        // back. Small churn is left for the allocator to reuse.
        if freed >= self.trim_dirty_bytes.load(Ordering::Relaxed) {
            self.trim_dirty.store(true, Ordering::Relaxed);
        }
        self.computes.fetch_add(1, Ordering::Relaxed);
        entry
    }
    /// The coalescing slot for a key (created on first miss).
    fn slot(&self, key: &str) -> Arc<ComputeSlot> {
        self.inner.guard().inflight.entry(key.to_string()).or_default().clone()
    }
    /// Drop a finished slot so the NEXT miss starts a fresh computation.
    fn finish(&self, key: &str) {
        self.inner.guard().inflight.remove(key);
    }
    /// Observability for the single-flight test and ops dashboards.
    pub fn computes(&self) -> u64 {
        self.computes.load(Ordering::Relaxed)
    }
    /// Swap off the "big heap drop happened" flag (trim timer → malloc_trim).
    fn take_trim(&self) -> bool {
        self.trim_dirty.swap(false, Ordering::Relaxed)
    }
    /// One compute slot (Gate D iter-25). Held INSIDE the OnceCell init,
    /// around the heavy leg — waiters on the same key never take one.
    pub async fn build_permit(&self) -> tokio::sync::OwnedSemaphorePermit {
        // acquire() only fails on close(), which never happens here.
        Arc::clone(&self.builds).acquire_owned().await.expect("build semaphore")
    }
    /// Observability + budget guard tests.
    pub fn build_permits_available(&self) -> usize {
        self.builds.available_permits()
    }
}

/// The inflight half of the single-flight (see [`crate::flight`]): the
/// explicit finish only ran on the linear path; the paired
/// [`crate::flight::SlotGuard`] reclaims the (revision-stamped) slot key on
/// the panic path too.
impl crate::flight::FlightCache for SearchCache {
    type Slot = ComputeSlot;
    fn flight_slot(&self, key: &str) -> Arc<Self::Slot> {
        self.slot(key)
    }
    fn flight_remove(&self, key: &str, slot: &Arc<Self::Slot>) {
        if self.inner.lock().unwrap().inflight.get(key).is_some_and(|c| Arc::ptr_eq(c, slot)) {
            self.finish(key);
        }
    }
    #[cfg(test)]
    fn flight_len(&self, key: &str) -> usize {
        self.inner.lock().unwrap().inflight.contains_key(key) as usize
    }
}

/// Heap hygiene for the search legs (Gate D iter-24): the collection cache
/// got its deferred `malloc_trim` on the refresher tick in iter 15; the
/// search caches (iter-22 byte budget + rev pruning) churn the same
/// multi-MB bodies with no trim path, and iter-22 measured ~180 MB of freed
/// heap retained above the live store after the search_db leg. This timer
/// consumes the same arm-flag shape: `insert` arms it on a big drop, the
/// tick (never a serving thread) trims. 2 s cadence — a full trim costs
/// well under a ms at these heap sizes, and the flag gates it when idle.
pub fn spawn_trim_timer(state: &AppState) {
    let cache = Arc::clone(&state.search_cache);
    crate::collection::tune_allocator(); // idempotent; owns("collection") may not run
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if cache.take_trim() {
                crate::collection::trim_heap();
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Live specific/broad/fallback merge leg (runSearch scryfallApi.js:776-881,
// lane-B iter-10): the field-filter branch once queryLocal came back empty
// (scope=database) or without ever consulting it (scope=internet). Same
// helpers Node uses — langSearch, the shared scryGetRetried client (sidecar
// swap + pacing + 429-requeue inherited from src/scryfall.rs), the
// 175-card page windows with the process-wide 5-min rawPageCache, the
// exact/broad/fallback merge with the numberPinnedIt short-circuit, the
// stable exact-first/cn sort, and the awaited cacheCards write — so "Rust
// answers this request" means the bytes Node would have produced.
// ---------------------------------------------------------------------------

/// `SCRY_PAGE_SIZE` — Scryfall pages are a fixed 175 cards.
const SCRY_PAGE_SIZE: i64 = 175;
/// `RAW_PAGE_TTL_MS` — repeat searches cost zero upstream budget.
const RAW_PAGE_TTL_MS: i64 = 5 * 60 * 1000;
/// `PRINTS` — every printing, no MTGO/Arena-only rebalances.
const LIVE_PRINTS: &str = " unique:prints -is:digital";

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct RawPage {
    data: Vec<Value>,
    total: Option<u64>,
    has_more: bool,
}

/// Module-level `rawPageCache`: (scoped q, lang, page, order) → page,
/// 5-min TTL, reaped past 400. Failures are never cached — a retry pays
/// the wire again, exactly like Node.
fn raw_pages() -> &'static Mutex<HashMap<String, (i64, RawPage)>> {
    static C: OnceLock<Mutex<HashMap<String, (i64, RawPage)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The merge block's failure mode: a real upstream error (transport,
/// post-retry 429, non-404/422 status). Node routes it through the big
/// catch; Rust answers the scope=internet cache fallback itself and proxies
/// everything else (Node owns that answer).
struct LiveErr;

/// `langSearch(q, lang)`: English adds nothing (the old query string stays
/// byte-identical); anything else appends ` lang:code` and requires
/// include_multilingual.
fn lang_search(q: &str, code: &str) -> (String, bool) {
    if code == "en" {
        (q.to_string(), false)
    } else {
        (format!("{q} lang:{code}"), true)
    }
}

enum PageFetch {
    Page(RawPage),
    /// tryQuery's 404/422 catch: the window is empty, meta.total untouched.
    Mismatch,
    Failed,
}

/// `scryPage` — one /cards/search page through the shared Scry client.
/// Node builds `/cards/search?q=…&page=N[&include_multilingual=true][&order=…]`
/// relative to SCRYFALL_BASE_URL and lets scryUrl swap to the sidecar; the
/// Scry client's `resolve` performs the same swap for a relative path.
async fn scry_page_live(
    st: &AppState,
    scoped_q: &str,
    lang_key: &str,
    multilingual: bool,
    page: i64,
    order: Option<&str>,
) -> PageFetch {
    let key = format!("{scoped_q}\u{0}{lang_key}\u{0}{page}\u{0}{}", order.unwrap_or(""));
    if let Some((exp, p)) = raw_pages().guard().get(&key) {
        if *exp > now_ms() {
            return PageFetch::Page(RawPage { data: p.data.clone(), total: p.total, has_more: p.has_more });
        }
    }
    let mut url = format!(
        "/cards/search?q={}&page={}{}",
        sidecar::encode_uri_component(scoped_q),
        page,
        if multilingual { "&include_multilingual=true" } else { "" }
    );
    if let Some(o) = order {
        url.push_str(&format!("&order={o}"));
    }
    match st.scry.get_retried(&url, &crate::scryfall::ScryOpts::default(), 4).await {
        Ok(v) => {
            let value = RawPage {
                data: v
                    .as_ref()
                    .and_then(|v| v.get("data"))
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
                total: v
                    .as_ref()
                    .and_then(|v| v.get("total_cards"))
                    .filter(|v| !v.is_null())
                    .and_then(|v| v.as_u64()),
                has_more: v
                    .as_ref()
                    .and_then(|v| v.get("has_more"))
                    .map(|v| truthy(v))
                    .unwrap_or(false),
            };
            let mut c = raw_pages().guard();
            c.insert(
                key,
                (now_ms() + RAW_PAGE_TTL_MS, RawPage { data: value.data.clone(), total: value.total, has_more: value.has_more }),
            );
            if c.len() > 400 {
                let now = now_ms();
                c.retain(|_, (exp, _)| *exp > now);
            }
            PageFetch::Page(value)
        }
        Err(e) if e.status == Some(404) || e.status == Some(422) => PageFetch::Mismatch,
        Err(_) => PageFetch::Failed,
    }
}

/// `fetchWindow` — the [offset, offset+limit) window over the 175-card
/// pages, RAW cards (tryQuery normalizes on the way out).
async fn fetch_window(
    st: &AppState,
    q: &str,
    lang_key: &str,
    code: &str,
    offset: i64,
    limit: i64,
    order: Option<&str>,
) -> Result<(Vec<Value>, Option<u64>), LiveErr> {
    let (scoped, multilingual) = lang_search(q, code);
    let mut page = offset.div_euclid(SCRY_PAGE_SIZE) + 1;
    let mut skip = offset.rem_euclid(SCRY_PAGE_SIZE) as usize;
    let mut out: Vec<Value> = Vec::new();
    let mut total: Option<u64> = None;
    while (out.len() as i64) < limit {
        let p = match scry_page_live(st, &scoped, lang_key, multilingual, page, order).await {
            PageFetch::Page(p) => p,
            PageFetch::Mismatch => return Ok((Vec::new(), None)),
            PageFetch::Failed => return Err(LiveErr),
        };
        if p.total.is_some() {
            total = p.total;
        }
        out.extend(p.data.into_iter().skip(skip));
        skip = 0;
        if !p.has_more {
            break;
        }
        page += 1;
    }
    out.truncate(limit as usize);
    Ok((out, total))
}

/// The merge block's `tryQuery`: a window, normalized, with the catch
/// rules. `meta` is meta.total — only the broad query's non-null total
/// reaches it (the specific set+cn probe would report its own tiny count),
/// and an errored/404 window leaves it untouched.
#[allow(clippy::too_many_arguments)]
async fn try_query_live(
    st: &AppState,
    q: &str,
    lang_key: &str,
    code: &str,
    lang_name: &str,
    off: i64,
    limit: i64,
    order: Option<&str>,
    broad_q: &Option<String>,
    meta: &mut Option<u64>,
) -> Result<Vec<Map<String, Value>>, LiveErr> {
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let (raw, total) = fetch_window(st, q, lang_key, code, off, limit, order).await?;
    if broad_q.as_deref() == Some(q) && total.is_some() {
        *meta = total;
    }
    Ok(raw.iter().map(|r| sidecar::normalize_card(r, lang_name)).collect())
}

/// id of a normalized card, as the JS dedup/sort sees it (`c.id`).
fn card_id_of(c: &Map<String, Value>) -> String {
    c.get("id").map(|v| crate::precons::js_string(v)).unwrap_or_default()
}

pub struct LiveOutcome {
    pub cards: Vec<Value>,
    pub total: Option<u64>,
    pub source: &'static str,
    /// Whether the caller must cacheCards() the subset: Node writes on the
    /// success path only — never for the merged-empty cache answer nor for
    /// the catch fallback's rows.
    pub cache: bool,
}

/// The merge block itself. `meta` (meta.total) is threaded through so the
/// caller's error path still sees the total a successful broad fetch
/// reported before a later window threw, like Node's shared `meta`.
async fn live_merge(
    st: &AppState,
    p: &SearchQuery,
    meta: &mut Option<u64>,
) -> Result<LiveOutcome, LiveErr> {
    let set_list: Vec<String> =
        p.set.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let scry_set = if set_list.len() == 1 {
        format!("set:{}", set_list[0])
    } else {
        format!("({})", set_list.iter().map(|s| format!("set:{s}")).collect::<Vec<_>>().join(" or "))
    };
    // `cleanNumber.replace(/^0+/, '') || cleanNumber`
    let stripped = {
        let t = p.number.trim_start_matches('0').to_string();
        if t.is_empty() {
            p.number.clone()
        } else {
            t
        }
    };
    let specific: Option<String> = if !set_list.is_empty() && !stripped.is_empty() {
        Some(format!("{scry_set} cn:{stripped}"))
    } else if !p.name.is_empty() && !stripped.is_empty() {
        Some(format!("{} cn:{stripped}", p.name))
    } else {
        None
    };
    let set_constraint = if !set_list.is_empty() { format!(" {scry_set}") } else { String::new() };
    let broad: Option<String> = if !p.name.is_empty() {
        Some(format!("{}{set_constraint}{LIVE_PRINTS}", p.name))
    } else if !set_list.is_empty() {
        Some(format!("{scry_set}{LIVE_PRINTS}"))
    } else {
        None
    };
    // Last resort: first word only ("Adamant" from "Adamant Will").
    let first_word = sidecar::first_word(&p.name).to_string();
    let fallback: Option<String> = if !first_word.is_empty() && first_word != p.name {
        Some(format!("{first_word}{set_constraint}{LIVE_PRINTS}"))
    } else {
        None
    };
    // A set browse pages by collector number; a name search keeps Scryfall's
    // relevance order so the typed card stays on page 1.
    let order: Option<&str> =
        if p.name.is_empty() && !set_list.is_empty() { Some("set") } else { None };
    let code = lang_codes(p.lang.as_deref()).1;
    let lang_key = p.lang.as_deref().unwrap_or("");
    let offset = (p.page - 1).saturating_mul(p.limit);

    let mut exact: Vec<Map<String, Value>> = Vec::new();
    if p.page == 1 {
        if let Some(sq) = &specific {
            exact =
                try_query_live(st, sq, lang_key, &code, &p.lang_name, 0, p.limit, order, &broad, meta)
                    .await?;
        }
    }
    // A set + collector number identifies ONE card: pairing it with the
    // broad set browse would bury it, so the browse only runs as a fallback.
    let number_pinned = p.name.is_empty() && !stripped.is_empty() && !exact.is_empty();
    let mut broad_cards: Vec<Map<String, Value>> = Vec::new();
    if !number_pinned {
        if let Some(bq) = &broad {
            if specific.as_deref() != Some(bq.as_str()) {
                broad_cards = try_query_live(
                    st, bq, lang_key, &code, &p.lang_name, offset, p.limit, order, &broad, meta,
                )
                .await?;
            }
        }
    }
    if number_pinned {
        *meta = Some(exact.len() as u64);
    }
    if exact.is_empty() && broad_cards.is_empty() {
        if let Some(fq) = &fallback {
            broad_cards = try_query_live(
                st, fq, lang_key, &code, &p.lang_name, offset, p.limit, order, &broad, meta,
            )
            .await?;
        }
    }
    // `merged = [...exact, ...broad.filter(c => !seen.has(c.id))]` — `seen`
    // is built from exact ONLY and never extended during the filter (broad
    // rows duplicating each other survive; Node's filter closes over the
    // exact set).
    let seen: std::collections::HashSet<String> = exact.iter().map(card_id_of).collect();
    let mut merged: Vec<Map<String, Value>> = exact.clone();
    merged.extend(broad_cards.into_iter().filter(|c| !seen.contains(&card_id_of(c))));
    if merged.is_empty() {
        // Node returns `localResults.map(parseCardRow)` — [] for both scopes
        // that can reach this leg (database answered nothing from the cache,
        // internet never queried it). An empty result IS the answer.
        return Ok(LiveOutcome { cards: Vec::new(), total: *meta, source: "cache", cache: false });
    }
    let mut cards: Vec<Value> =
        merged.into_iter().take(p.limit as usize).map(Value::Object).collect();
    if !p.name.is_empty() || !stripped.is_empty() {
        // Stable sort (V8's sort is stable; Rust's sort_by is too): exact
        // matches first, then alternatives by `parseInt(number, 10) || 0`.
        let exact_ids: std::collections::HashSet<String> = exact.iter().map(card_id_of).collect();
        cards.sort_by(|a, b| {
            let rank = |c: &Value| {
                if c.as_object().map(card_id_of).map(|id| exact_ids.contains(&id)).unwrap_or(false)
                {
                    0
                } else {
                    1
                }
            };
            let num = |c: &Value| {
                c.as_object()
                    .and_then(|m| m.get("number"))
                    .and_then(|v| v.as_str())
                    .and_then(js_parse_int)
                    .unwrap_or(0)
            };
            rank(a).cmp(&rank(b)).then(num(a).cmp(&num(b)))
        });
    }
    Ok(LiveOutcome { cards, total: *meta, source: "scryfall", cache: true })
}

/// The merge catch's scope=internet fallback: `await queryLocal()` rows in
/// parseCardRow shape (owned_qty is attached by the caller, as the route
/// does for every non-collection response).
fn query_local_rows(pool: &DbPool, p: &SearchQuery) -> Result<Vec<Value>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    let (sql, params) = local_cache_query(p, None);
    let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
    let mut rows =
        stmt.query(rusqlite::params_from_iter(params.iter())).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        out.push(Value::Object(row_to_object(row)?));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// mergeSearchPrices (utils/searchPrices.js) — the scope=internet route leg.
// The sidecar answers price-less; fill each price-less printing from
// card_cache (the last real quote), and only for printings never priced ask
// the REAL Scryfall API through bulkFetchByIdentifier({upstream: true}).
// ---------------------------------------------------------------------------

const PRICE_FIELDS: [&str; 7] = [
    "price_trend",
    "price_normal",
    "price_holofoil",
    "price_etched",
    "price_currency",
    "price_source",
    "price_estimated",
];
const PRICE_UPSTREAM_CAP: usize = 150;
/// A cached row can carry price_source with no number (quoted before the
/// market had one). Still "never priced" — but don't re-ask every page view.
const EMPTY_RETRY_MS: i64 = 6 * 60 * 60 * 1000;
/// How long a search page may wait for upstream quotes before it is
/// returned with what card_cache had; the fetch keeps running for next time.
const UPSTREAM_WAIT_MS: u64 = 1500;

fn empty_checked_at() -> &'static Mutex<HashMap<String, i64>> {
    static M: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `Number(v) > 0` for the price fields (null/undefined → 0/NaN → false).
fn num_gt0(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Number(n)) => n.as_f64().map(|f| f > 0.0).unwrap_or(false),
        Some(Value::String(s)) => sidecar::js_parse_float(s).map(|f| f > 0.0).unwrap_or(false),
        _ => false,
    }
}

/// `hasQuote`: an estimate (a sibling's quote) is not this printing's quote.
fn has_quote(r: &Map<String, Value>) -> bool {
    let est_truthy = match r.get("price_estimated") {
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => sidecar::js_parse_float(s).map(|f| f != 0.0).unwrap_or(false),
        _ => false,
    };
    let some_quote = ["price_normal", "price_holofoil", "price_etched"]
        .iter()
        .any(|k| num_gt0(r.get(*k)));
    !est_truthy && (some_quote || num_gt0(r.get("price_trend")))
}

/// `copyPrices`: `if (src[f] !== undefined)` over the seven price fields.
/// Driver rows carry all seven (NULL → null, which JS copies too).
fn copy_prices(target: &mut Map<String, Value>, src: &Map<String, Value>) {
    for f in PRICE_FIELDS {
        if let Some(v) = src.get(f) {
            target.insert(f.to_string(), v.clone());
        }
    }
}

async fn merge_search_prices(st: &AppState, pool: &Arc<DbPool>, cards: &mut [Value]) {
    if cards.is_empty() {
        return;
    }
    // `need = cards.filter(c => c && c.id && c.price_source == null)` —
    // == null covers both absent and JSON null.
    let need_ids: Vec<(usize, String)> = cards
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let m = c.as_object()?;
            let id = m.get("id").filter(|v| truthy(v))?;
            let ps_absent = matches!(m.get("price_source"), None | Some(Value::Null));
            if ps_absent {
                Some((i, crate::precons::js_string(id)))
            } else {
                None
            }
        })
        .collect();
    if need_ids.is_empty() {
        return;
    }
    // `[...new Set(need.map(c => c.id))]` — first-seen order.
    let mut ids: Vec<String> = Vec::new();
    for (_, id) in &need_ids {
        if !ids.iter().any(|x| x == id) {
            ids.push(id.clone());
        }
    }
    let rows: Vec<Map<String, Value>> = {
        let Ok(conn) = pool.get_async().await else { return };
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT id, {} FROM card_cache WHERE id IN ({placeholders}) AND price_source IS NOT NULL",
            PRICE_FIELDS.join(", ")
        );
        let mut stmt = match conn.prepare_cached(&sql) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut qres = match stmt
            .query(rusqlite::params_from_iter(
                ids.iter().map(|s| rusqlite::types::Value::Text(s.clone())),
            ))
            .map_err(|e| e.to_string())
        {
            Ok(r) => r,
            Err(_) => return,
        };
        let mut out: Vec<Map<String, Value>> = Vec::new();
        while let Ok(Some(row)) = qres.next() {
            if let Ok(m) = row_to_object(row) {
                out.push(m);
            }
        }
        out
    };
    let by_id: HashMap<String, Map<String, Value>> =
        rows.into_iter().map(|r| (card_id_of(&r), r)).collect();
    let now = now_ms();
    let mut missing_idx: Vec<usize> = Vec::new();
    for (i, id) in &need_ids {
        let r = by_id.get(id);
        if let Some(r) = r {
            if has_quote(r) {
                if let Some(m) = cards[*i].as_object_mut() {
                    copy_prices(m, r);
                }
                continue;
            }
            // Show the empty quote unless upstream has a real one.
            if let Some(m) = cards[*i].as_object_mut() {
                copy_prices(m, r);
            }
            let checked = empty_checked_at().guard().get(id).copied();
            if let Some(checked) = checked {
                if now - checked < EMPTY_RETRY_MS {
                    continue;
                }
            }
        }
        missing_idx.push(*i);
    }
    if missing_idx.is_empty() {
        return;
    }
    // `[...new Map(missing.map(c => [c.id, c])).values()].slice(0, 150)`
    // — last entry wins per id, then cap.
    let mut uniq: Vec<(String, usize)> = Vec::new();
    for i in &missing_idx {
        // let-else: missing_idx entries always come from need_ids above, so
        // the find hits; the guard keeps the panic edge out of the hot path
        // (errors-maint L1).
        let Some(id) = need_ids.iter().find(|(mi, _)| mi == i).map(|(_, id)| id.clone()) else { continue };
        match uniq.iter().position(|(u, _)| *u == id) {
            Some(pos) => uniq[pos].1 = *i,
            None => uniq.push((id, *i)),
        }
    }
    uniq.truncate(PRICE_UPSTREAM_CAP);
    let bulk_rows: Vec<Map<String, Value>> = uniq
        .iter()
        // Node hands bulkFetchByIdentifier the CARD rows themselves — the
        // identifier builder picks set/collector_number/name off them.
        .filter_map(|(_, i)| cards[*i].as_object().cloned())
        .collect();
    // Scryfall rate-limited us: the queue is paused (60 s), so waiting
    // would only stall the page. Answer now; the queued fetch prices the
    // next view.
    let cooling = st.scry.cooldown_until().await.map(|u| u > now).unwrap_or(false);
    let st_bg = st.clone();
    let uniq_bg = uniq.clone();
    let work = tokio::spawn(async move {
        let out = crate::precons::bulk_fetch_by_identifier_opts(
            &st_bg,
            &bulk_rows,
            &crate::scryfall::ScryOpts { upstream: true, breaker: false },
        )
        .await
        .ok()?;
        let priced: Vec<Map<String, Value>> = out
            .cards
            .into_iter()
            .filter_map(|v| v.as_object().cloned())
            .filter(|f| !matches!(f.get("price_source"), None | Some(Value::Null)) && has_quote(f))
            .collect();
        let priced_ids: std::collections::HashSet<String> = priced.iter().map(card_id_of).collect();
        {
            let mut m = empty_checked_at().guard();
            for (id, _) in &uniq_bg {
                if !priced_ids.contains(id) {
                    m.insert(id.clone(), now);
                }
            }
        }
        if !priced.is_empty() {
            let pool_bg = st_bg.db.clone();
            let maps = priced.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let Some(pool_bg) = pool_bg else { return };
                let Ok(conn) = pool_bg.get() else { return };
                if let Err(e) = crate::card_cache::cache_normalized_cards(&conn, &maps) {
                    tracing::error!("search price cache write failed: {e}");
                }
            })
            .await;
        }
        Some(priced)
    });
    if cooling {
        return; // detached work keeps running; the page answers from cache.
    }
    let priced = match tokio::time::timeout(std::time::Duration::from_millis(UPSTREAM_WAIT_MS), work).await
    {
        Ok(Ok(Some(p))) => p,
        // Timed out (or the fetch failed): the spawned task keeps running
        // so the next page view is priced from card_cache.
        _ => return,
    };
    let fresh_by_id: HashMap<String, Map<String, Value>> =
        priced.into_iter().map(|f| (card_id_of(&f), f)).collect();
    for i in &missing_idx {
        // Same let-else binding as the dedup loop above (errors-maint L1).
        let Some(id) = need_ids.iter().find(|(mi, _)| mi == i).map(|(_, id)| id.clone()) else { continue };
        if let Some(f) = fresh_by_id.get(&id) {
            if let Some(m) = cards[*i].as_object_mut() {
                copy_prices(m, f);
            }
        }
    }
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        axum::Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

pub async fn handle(
    method: Method,
    State(st): State<AppState>,
    cc: ConnectInfo<std::net::SocketAddr>,
    req: Request,
) -> Response {
    // GET and POST share the native path (Node mounts the same searchCards
    // on both, collection.js:233-234; POST is how the scanner client carries
    // its JSON body). Every other method proxies verbatim — Express's
    // router.post/router.get would not match either, and Node answers.
    let is_post = method == Method::POST;
    if method != Method::GET && !is_post {
        return crate::proxy(State(st.clone()), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st.clone()), req).await;
    };
    // Express runs authenticateToken (server.js:454) BEFORE bigJson parses
    // the body on /api/search (server.js:462): the 401/403 arms must not
    // depend on body validity, and a malformed body must not reach the
    // limiter. POST bytes are buffered here (read_parts/rebuild precedent
    // from collection_writes) but only INTERPRETED after auth; every proxy
    // leg below re-attaches the original bytes untouched.
    let (parts, bytes, oversize) =
        crate::collection_writes::read_parts(req, crate::collection_writes::BODY_BIG).await;
    let headers = parts.headers.clone();
    let uri = parts.uri.clone();
    let to_node = |st: &AppState, parts: axum::http::request::Parts, bytes: Vec<u8>| {
        let st = st.clone();
        async move {
            crate::proxy(State(st), crate::collection_writes::rebuild(parts, &bytes)).await
        }
    };

    // authenticateToken runs before the route in Node; same 401/403 bodies.
    // µs-cached SQLite, inline on the worker (iter 6). API keys are GET-only
    // reads (authenticate's is_get), so a POST with an api key gets Node's
    // 403 before any body parse — same as Node's gate.
    let token = auth::bearer_token(&headers).map(|s| s.to_string());
    let user = match auth::authenticate_async(&pool, token.as_deref(), !is_post).await {
        Ok(u) => u,
        Err(res) => return res,
    };

    // bigJson (15mb) mounts AFTER authenticateToken (server.js:462): an
    // oversize JSON body answers the parser's 413 only once auth passed.
    // Non-JSON content-types skip the parser — the empty (abandoned) body
    // is the equivalent and the proxy legs forward it body-less.
    if oversize && crate::collection_writes::is_json_ct(&headers) {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Upload too large. Try exporting/importing in smaller batches.",
        );
    }

    // express.json({limit:'15mb'}) twin (bigJson, server.js:204/462). Parse
    // failures answer the GLOBAL handler's 500 (server.js:634-642: only
    // entity.too.large gets a bespoke body; everything else — including
    // strict-mode SyntaxError — logs and 500s), and happen BEFORE the
    // limiter counts the request.
    let body_map: Option<serde_json::Map<String, Value>> = if is_post {
        match parse_big_json(&headers, &bytes) {
            BigBody::Parsed(m) => m,
            BigBody::Invalid => {
                tracing::error!("search: invalid JSON body (express.json strict)");
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
            }
            BigBody::TooLarge => {
                return json_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Upload too large. Try exporting/importing in smaller batches.",
                )
            }
        }
    } else {
        None
    };

    // searchLimiter (middleware/auth.js:86, mounted ahead of searchCards on
    // both GET and POST): 300 / 15 min / client IP, AFTER auth. The bucket is
    // hit exactly once per authenticated request so it advances in lockstep
    // with the Node limiter's bucket (which counts every request, including
    // the ones Rust re-serves through the proxy).
    let rl_key = ip_limit::client_ip_key(Some(cc.0.ip()), &headers, &st.trust_proxy);
    let rl = crate::cvscan::search_limiter().hit(&rl_key, crate::cvscan::now_ms());
    let with_rl = |res: Response| -> Response {
        let mut res = res;
        let h = res.headers_mut();
        let _ = h.try_insert("ratelimit-policy", HeaderValue::from_static("300;w=900"));
        let _ = h.try_insert("ratelimit-limit", HeaderValue::from_static("300"));
        let _ = h.try_insert(
            "ratelimit-remaining",
            HeaderValue::from_str(&rl.remaining.to_string()).unwrap(),
        );
        let _ = h.try_insert(
            "ratelimit-reset",
            HeaderValue::from_str(&rl.reset_secs.to_string()).unwrap(),
        );
        if rl.blocked {
            let _ = h.try_insert(
                "retry-after",
                HeaderValue::from_str(&rl.reset_secs.to_string()).unwrap(),
            );
        }
        res
    };
    if rl.blocked {
        return with_rl(json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many searches. Please slow down and try again shortly.",
        ));
    }

    let raw_query = uri.query().map(|s| s.to_string());
    let p = match if is_post {
        plan_post(user.id, raw_query.as_deref(), body_map.as_ref())
    } else {
        plan(user.id, raw_query.as_deref(), st.local_scrayfall)
    } {
        Plan::Native(p) => p,
        Plan::CollectionQ(cq) => {
            return with_rl(serve_collq(st, crate::collection_writes::rebuild(parts, &bytes), pool, cq).await);
        }
        Plan::CollectionOwned(o) => {
            let inm = headers.get("if-none-match").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
            let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
            return with_rl(serve_owned(st, pool, o, inm, ae).await);
        }
        Plan::CollectionLocalRows(lq) => {
            let inm = headers.get("if-none-match").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
            let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
            return with_rl(serve_local(st, pool, lq, inm, ae, None).await);
        }
        Plan::CollectionLocalTags { tags, walk } => {
            let inm = headers.get("if-none-match").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
            let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
            let req = crate::collection_writes::rebuild(parts, &bytes);
            return with_rl(serve_local(st, pool, tags, inm, ae, Some((req, walk))).await);
        }
        Plan::InvalidQuery => {
            return with_rl(json_error(StatusCode::BAD_REQUEST, "INVALID_QUERY"));
        }
        Plan::Proxy => return to_node(&st, parts, bytes).await,
    };

    // Express's res.json sets the body ETag on POST too, but conditional
    // revalidation only applies to GET/HEAD — a POST never replays as 304.
    let inm = if is_post {
        None
    } else {
        headers.get("if-none-match").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
    };
    let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).map(|s| s.to_string());

    let rev = pool
        .get()
        .ok()
        .and_then(|c| crate::db::current_revision(&c).ok().flatten());
    let rev_i = rev.unwrap_or(-1);

    let key = p.cache_key();
    let entry: Arc<SearchEntry> = match st.search_cache.get(&key, rev_i) {
        Some(e) => e,
        None => {
            // Coalesced regeneration (CollectionCache iter-5 pattern): every
            // concurrent miss on this key awaits ONE build+walk through a
            // shared OnceCell slot. The revision rides in the slot key so a
            // request that observed a newer revision never shares a slot
            // built under an older one; the entry itself is still rev-gated
            // in the cache.
            //
            // Walk entries are stamped with the revision read AFTER the
            // build: the 7c leg's card_cache write fires Node's twin
            // collection_read_rev_* triggers, and stamping the pre-build
            // rev would make every walk invalidate its own entry (each
            // repeat request re-walked the sidecar forever — a regression
            // Node does not have, since Node has no response cache at
            // all). A write from elsewhere during the build still bumps
            // the counter past the post-build read, so real invalidation
            // is preserved. The cache-path branch reads card_cache itself
            // and keeps the pre-build stamp.
            let cache = Arc::clone(&st.search_cache);
            let slot_key = format!("{key}\u{0}{rev_i}");
            let (slot, _slot_guard) = crate::flight::slot_guarded(&cache, &slot_key);
            let init_cache = Arc::clone(&cache);
            let init_st = st.clone();
            let init_pool = Arc::clone(&pool);
            let init_key = key.clone();
            let init_p = p.clone();
            let res: Result<&Result<Option<Arc<SearchEntry>>, String>, String> = slot
                .get_or_try_init(move || async move {
                    let build_pool = Arc::clone(&init_pool);
                    let build_idx = Arc::clone(&init_st.name_index);
                    let build_owned = Arc::clone(&init_st.owned_index);
                    let p2 = init_p.clone();
                    // scope=internet skips queryLocal entirely (Node's
                    // `if (scope !== 'internet')` gate at scryfallApi.js:752)
                    // — the live merge below is its first leg.
                    let built = if init_p.internet {
                        None
                    } else {
                        // Budget-gated CPU leg (iter 25): held across the
                        // blocking build+gzip, dropped before the walk leg
                        // so sidecar walks never queue behind builds.
                        let _permit = init_cache.build_permit().await;
                        let built = tokio::task::spawn_blocking(move || {
                        let Some((body, _total, stale)) =
                            build_body(&build_pool, &build_idx, &build_owned, &p2)?
                        else {
                            return Ok::<_, String>(None);
                        };
                        let body = bytes::Bytes::from(body);
                        let etag = body_etag(&body);
                        let gz = gzip_bytes(&body);
                        Ok(Some((body, gz, etag, None, "cache", rev_i, stale)))
                    })
                    .await
                    .unwrap_or_else(|e| Err(format!("build task panicked: {e}")))?;
                        built
                    };

                    let (body, gz, etag, total, source, entry_rev, stale) = match built {
                        Some(v) => v,
                        // The local cache had nothing. Node's next leg is the
                        // sidecar walk; for the branch we port, run it
                        // ourselves — anything else (or any sidecar failure)
                        // answers Ok(None): every waiter then proxies and
                        // Node answers verbatim, upserts/503s and all.
                        None => {
                            let offset = (init_p.page - 1).saturating_mul(init_p.limit);
                            let walked = match init_st
                                .cfg
                                .sidecar_url
                                .as_deref()
                                .filter(|_| init_p.walkable && init_st.owns("search_sidecar"))
                            {
                                Some(base) => {
                                    match sidecar::walk_name(
                                        &init_st,
                                        base,
                                        &init_p.name,
                                        &init_p.lang_name,
                                        offset,
                                        init_p.limit,
                                    )
                                    .await
                                    {
                                        Ok(o) => Some(o),
                                        // Node's merge catch is the fall-
                                        // through when a window throws; for
                                        // this branch the walk IS that merge,
                                        // so a failed walk lands in the same
                                        // next leg below, not on the proxy.
                                        Err(_) => {
                                            tracing::debug!(
                                                name = %init_p.name,
                                                "sidecar walk failed → live merge"
                                            );
                                            None
                                        }
                                    }
                                }
                                None => None,
                            };
                            if let Some(outcome) = walked {
                            let walk_pool = Arc::clone(&init_pool);
                            let walk_owned = Arc::clone(&init_st.owned_index);
                            let p3 = init_p.clone();
                            let walked = tokio::task::spawn_blocking(move || -> Result<_, String> {
                                let mut cards = outcome.cards;
                                let conn = walk_pool.get().map_err(|e| e.to_string())?;
                                // Node's runSearch `await cacheCards(cards)`
                                // (unit 7c): a non-empty walk upserts the
                                // normalized rows into the shared card_cache
                                // BEFORE the answer is sent — that write is
                                // observable (F3-TC2 reads the table right
                                // after the response) and it is what keeps
                                // the card findable from the cache leg when
                                // upstream goes dark (F3-TC3). Same
                                // cacheNormalizedCards twin every writer
                                // uses; failure logs and continues, exactly
                                // like Node's awaited-but-tolerant call.
                                if outcome.source == "scryfall" {
                                    let maps: Vec<serde_json::Map<String, Value>> = cards
                                        .iter()
                                        .filter_map(|c| c.as_object().cloned())
                                        .collect();
                                    if let Err(e) =
                                        crate::card_cache::cache_normalized_cards(&conn, &maps)
                                    {
                                        tracing::error!("search walk cache write failed: {e}");
                                    }
                                }
                                let mut attached = false;
                                if rev_i >= 0 {
                                    if let Some(map) =
                                        walk_owned.owned_map(&walk_pool, p3.user_id, rev_i)
                                    {
                                        attached = name_index::attach_owned_from_snapshot(
                                            &mut cards, &map,
                                        );
                                    }
                                }
                                if !attached {
                                    attach_owned_qty(&conn, &mut cards[..], p3.user_id)?;
                                }
                                let body = Bytes::from(
                                    serde_json::to_vec(&cards).map_err(|e| e.to_string())?,
                                );
                                Ok::<_, String>((body, outcome.total, outcome.source))
                            })
                            .await
                            .map_err(|e| format!("sidecar walk task panicked: {e}"))??;
                            let (body, total, source) = walked;
                            let etag = body_etag(&body);
                            let gz = gzip_bytes(&body);
                            // Post-write rev stamp (see above): re-read AFTER
                            // our own cacheCards upsert so the entry survives
                            // its own write; any OTHER write that raced the
                            // build already pushed the counter higher.
                            let post_rev = init_pool
                                .get()
                                .ok()
                                .and_then(|c| crate::db::current_revision(&c).ok().flatten())
                                .unwrap_or(rev_i);
                            (body, gz, etag, total, source, post_rev, Vec::new())
                            } else {
                                // ---- live merge leg (iter-10) ----
                                // Node's runSearch specific/broad/fallback
                                // merge: database scope arrives here with an
                                // EMPTY queryLocal (that's why built is
                                // None), internet scope skipped queryLocal
                                // entirely. Upstream errors that Node answers
                                // itself (503/429) keep proxying; the one
                                // catch outcome Rust can reproduce exactly —
                                // scope=internet served from queryLocal rows
                                // — is served natively.
                                let mut meta: Option<u64> = None;
                                let outcome = match live_merge(&init_st, &init_p, &mut meta).await {
                                    Ok(o) => o,
                                    Err(LiveErr) => {
                                        if !init_p.internet {
                                            tracing::debug!(
                                                name = %init_p.name,
                                                "live merge upstream error → proxy"
                                            );
                                            return Ok(Ok(None));
                                        }
                                        let rows_pool = Arc::clone(&init_pool);
                                        let rows_p = init_p.clone();
                                        let rows = tokio::task::spawn_blocking(move || {
                                            query_local_rows(&rows_pool, &rows_p)
                                        })
                                        .await
                                        .map_err(|e| {
                                            format!("local rows task panicked: {e}")
                                        })??;
                                        if rows.is_empty() {
                                            tracing::debug!(
                                                name = %init_p.name,
                                                "live merge failed, no cache rows → proxy"
                                            );
                                            return Ok(Ok(None));
                                        }
                                        LiveOutcome {
                                            cards: rows,
                                            total: meta,
                                            source: "cache",
                                            cache: false,
                                        }
                                    }
                                };
                                let LiveOutcome {
                                    cards: input_cards,
                                    total: live_total,
                                    source: live_source,
                                    cache: do_cache,
                                } = outcome;
                                let pool3 = Arc::clone(&init_pool);
                                let owned3 = Arc::clone(&init_st.owned_index);
                                let p3 = init_p.clone();
                                let internet = init_p.internet;
                                let st4 = init_st.clone();
                                let pool4 = Arc::clone(&init_pool);
                                let mut cards = tokio::task::spawn_blocking(
                                    move || -> Result<Vec<Value>, String> {
                                        let mut cards = input_cards;
                                        let conn = pool3.get().map_err(|e| e.to_string())?;
                                        if do_cache {
                                            // Node's runSearch awaits
                                            // cacheCards(cards) before the
                                            // merge block returns — the same
                                            // twin every writer uses.
                                            let maps: Vec<Map<String, Value>> = cards
                                                .iter()
                                                .filter_map(|c| c.as_object().cloned())
                                                .collect();
                                            if let Err(e) =
                                                crate::card_cache::cache_normalized_cards(
                                                    &conn, &maps,
                                                )
                                            {
                                                tracing::error!(
                                                    "search merge cache write failed: {e}"
                                                );
                                            }
                                        }
                                        // The route attaches owned_qty to
                                        // every non-collection answer.
                                        let mut attached = false;
                                        if rev_i >= 0 {
                                            if let Some(map) =
                                                owned3.owned_map(&pool3, p3.user_id, rev_i)
                                            {
                                                attached =
                                                    name_index::attach_owned_from_snapshot(
                                                        &mut cards, &map,
                                                    );
                                            }
                                        }
                                        if !attached {
                                            attach_owned_qty(&conn, &mut cards[..], p3.user_id)?;
                                        }
                                        Ok(cards)
                                    },
                                )
                                .await
                                .map_err(|e| format!("merge attach task panicked: {e}"))??;
                                // The route mergeSearchPrices on
                                // scope=internet: the sidecar answers
                                // price-less, card_cache holds the last real
                                // quote, upstream (mocked in tests) fills
                                // printings never priced.
                                if internet {
                                    merge_search_prices(&st4, &pool4, &mut cards).await;
                                }
                                let body = Bytes::from(
                                    serde_json::to_vec(&cards).map_err(|e| e.to_string())?,
                                );
                                let etag = body_etag(&body);
                                let gz = gzip_bytes(&body);
                                // Post-write rev stamp (see the walk arm):
                                // re-read AFTER the merge's own cacheCards /
                                // price writes so the entry survives them.
                                let post_rev = init_pool
                                    .get()
                                    .ok()
                                    .and_then(|c| crate::db::current_revision(&c).ok().flatten())
                                    .unwrap_or(rev_i);
                                (body, gz, etag, live_total, live_source, post_rev, Vec::new())
                            }
                        }
                    };
                    // Stamp with the POST-build revision: the walk's own
                    // cacheCards upserts bump the (unconditional, db.js:1252)
                    // collection_read_revision mid-build, and stamping with
                    // the pre-build rev_i would self-invalidate the entry —
                    // every later request would re-walk. Anything written by
                    // someone else before this read still invalidates as
                    // intended; the build itself performed no other writes.
                    let store_rev = init_pool
                        .get()
                        .ok()
                        .and_then(|c| crate::db::current_revision(&c).ok().flatten())
                        .unwrap_or(rev_i);
                    Ok(Ok(Some(init_cache.insert(
                        init_key,
                        SearchEntry { rev: store_rev, raw: body, gz, etag, total, source, x_source: true, catalog: None, stale: Arc::new(stale) },
                    ))))
                })
                .await;
            drop(_slot_guard);
            // get_or_try_init nests: outer Err carries a clone of the init
            // error, outer Ok wraps the stored Result.
            match res {
                Ok(Ok(Some(e))) => e.clone(),
                Ok(Ok(None)) => return to_node(&st, parts.clone(), bytes.clone()).await,
                Ok(Err(ref e)) | Err(ref e) => {
                    tracing::error!(error = %e, "search build failed");
                    return with_rl(json_error(StatusCode::INTERNAL_SERVER_ERROR, "Search failed"));
                }
            }
        }
    };

    // Stale background refresh (queryLocal's floating bulkFetchByIdentifier):
    // while the entry was built on >3-day-old rows, every served request
    // re-fires the fetch until the refreshed write bumps the revision and a
    // fresh entry replaces it. Fire-and-forget, exactly like Node's floating
    // promise — the cached answer goes out untouched either way.
    if st.stale_refresh && !entry.stale.is_empty() {
        let st_bg = st.clone();
        let rows = Arc::clone(&entry.stale);
        tokio::spawn(async move { bulk_refresh_stale(&st_bg, &rows).await });
    }

    // Express's default body ETag on res.json(cards): a matching conditional
    // GET replays as an empty 304, with the headers set before send().
    with_rl(respond_entry(&entry, inm.as_deref(), ae.as_deref(), None))
}

/// Emit a cached native search response, honoring the entry's header
/// contract (plain X-Source for the field-filter legs; the full
/// X-Catalog-* surface for the q leg).
fn respond_entry(entry: &Arc<SearchEntry>, inm: Option<&str>, ae: Option<&str>, dyn_: Option<(&'static str, &'static str)>) -> Response {
    // (X-Catalog-Cache, X-Source) for this request; Node recomputes both per
    // request from the match-list entry's age, so the q leg passes them in
    // rather than replaying baked bytes. Catalog-derived strings bind via
    // .ok(): an unencodable value skips the header instead of panicking the
    // search hot path (errors-maint L2).
    let (cat_status, src) = match dyn_ {
        Some((s, src)) => (s, Some(HeaderValue::from_static(src))),
        None => (
            entry.catalog.as_ref().map(|c| c.status).unwrap_or("cache"),
            HeaderValue::from_str(entry.source).ok(),
        ),
    };
    let tag = entry.etag.clone();
    let total_hdr = entry
        .total
        .map(|t| (header::HeaderName::from_static("x-total-count"), HeaderValue::from_str(&t.to_string()).unwrap()));

    // The q-leg header set (route lines 142–165): X-Total-Count when
    // requested, complete/upstream/cache/snapshot, X-Source (always
    // 'sidecar' here — the only value Node emits in sidecar mode), and the
    // matching expose list. Express keeps res.set() headers on 304s, so the
    // conditional path sends the same set.
    let catalog: Vec<(header::HeaderName, HeaderValue)> = match &entry.catalog {
        None => Vec::new(),
        Some(c) => {
            let mut v: Vec<(header::HeaderName, HeaderValue)> = Vec::with_capacity(5);
            if let Some(t) = &c.total {
                if let Ok(h) = HeaderValue::from_str(t) {
                    v.push((header::HeaderName::from_static("x-total-count"), h));
                }
            }
            v.push((
                header::HeaderName::from_static("x-catalog-complete"),
                HeaderValue::from_static(if c.complete { "1" } else { "0" }),
            ));
            if let Some(u) = &c.upstream_total {
                if let Ok(h) = HeaderValue::from_str(u) {
                    v.push((header::HeaderName::from_static("x-catalog-upstream-total"), h));
                }
            }
            v.push((header::HeaderName::from_static("x-catalog-cache"), HeaderValue::from_static(cat_status)));
            if let Ok(h) = HeaderValue::from_str(&c.snapshot) {
                v.push((header::HeaderName::from_static("x-catalog-snapshot"), h));
            }
            v
        }
    };
    let expose: Option<&'static str> = if entry.catalog.is_some() {
        if entry.x_source {
            Some("X-Total-Count, X-Catalog-Complete, X-Catalog-Upstream-Total, X-Catalog-Cache, X-Catalog-Snapshot, X-Source")
        } else {
            // The local answer legs (answerLocally / local Oracle Tags):
            // Node's route only appends ', X-Source' when source is
            // 'sidecar'/'local-fallback' — 'local' responses stay without it.
            Some("X-Total-Count, X-Catalog-Complete, X-Catalog-Upstream-Total, X-Catalog-Cache, X-Catalog-Snapshot")
        }
    } else if entry.x_source {
        Some("X-Total-Count, X-Source")
    } else {
        // Collection field-filter leg: the route set neither header.
        None
    };

    if etag_matches(inm, &tag) {
        let mut b = Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, HeaderValue::from_str(&tag).unwrap());
        if entry.x_source {
            if let Some(h) = src.clone() {
                b = b.header("x-source", h);
            }
        }
        if let Some((k, v)) = total_hdr {
            b = b.header(k, v);
        }
        for (k, v) in catalog {
            b = b.header(k, v);
        }
        if let Some(expose) = expose {
            b = b.header("access-control-expose-headers", HeaderValue::from_static(expose));
        }
        return b.body(Body::empty()).unwrap();
    }

    let use_gzip = wants_encoding(ae, "gzip", entry.raw.len());
    let (body, encoding) = if use_gzip { (entry.gz.clone(), Some("gzip")) } else { (entry.raw.clone(), None) };

    let mut b = Response::builder().status(StatusCode::OK).header(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    if let Some(enc) = encoding {
        b = b.header(header::CONTENT_ENCODING, HeaderValue::from_static(enc))
            .header(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    b = b.header(header::ETAG, HeaderValue::from_str(&tag).unwrap());
    if entry.x_source {
        if let Some(h) = src {
            b = b.header("x-source", h);
        }
    }
    if let Some((k, v)) = total_hdr {
        b = b.header(k, v);
    }
    for (k, v) in catalog {
        b = b.header(k, v);
    }
    if let Some(expose) = expose {
        b = b.header("access-control-expose-headers", HeaderValue::from_static(expose));
    }
    b.body(Body::from(body)).unwrap()
}

/// The collection field-filter leg for one planned request: rev-gated
/// response cache over (params, revision), one pure-SQL build. Nothing here
/// ever proxies: Node answers the same query from the same tables, and empty
/// rows are Node's `[]` too. A SQL failure mirrors Node's route-catch 500
/// 'Search failed'.
async fn serve_owned(st: AppState, pool: Arc<DbPool>, o: Box<CollectionOwned>, inm: Option<String>, ae: Option<String>) -> Response {
    let rev_i = pool
        .get()
        .ok()
        .and_then(|c| crate::db::current_revision(&c).ok().flatten())
        .unwrap_or(-1);

    let key = o.cache_key();
    if let Some(e) = st.search_cache.get(&key, rev_i) {
        return respond_entry(&e, inm.as_deref(), ae.as_deref(), None);
    }

    // Single-flight like the other legs: the stampede on one cold key shares
    // one query. `Ok(None)` (never produced here) would mean "proxy".
    let cache = Arc::clone(&st.search_cache);
    let slot_key = format!("{key}\u{0}{rev_i}");
    let (slot, _slot_guard) = crate::flight::slot_guarded(&cache, &slot_key);
    let init_cache = Arc::clone(&cache);
    let init_key = key.clone();
    let build_pool = Arc::clone(&pool);
    let build_o = (*o).clone();
    let res: Result<&Result<Option<Arc<SearchEntry>>, String>, String> = slot
        .get_or_try_init(move || async move {
            // Budget-gated compute (iter 25), twin of the collection cache.
            let _permit = init_cache.build_permit().await;
            let built = tokio::task::spawn_blocking(move || build_owned_body(&build_pool, &build_o))
                .await
                .unwrap_or_else(|e| Err(format!("collection-owned build task panicked: {e}")))?;
            let body = bytes::Bytes::from(built);
            let etag = body_etag(&body);
            let gz = gzip_bytes(&body);
            Ok(Ok(Some(init_cache.insert(
                init_key,
                SearchEntry {
                    rev: rev_i,
                    raw: body,
                    gz,
                    etag,
                    total: None,
                    source: "cache",
                    x_source: false,
                    catalog: None,
                    stale: Arc::new(Vec::new()),
                },
            ))))
        })
        .await;
    drop(_slot_guard);
    match res {
        Ok(Ok(Some(e))) => respond_entry(e, inm.as_deref(), ae.as_deref(), None),
        Ok(Ok(None)) => unreachable!("collection-owned build never proxies"),
        Ok(Err(ref e)) | Err(ref e) => {
            tracing::error!(error = %e, "collection-owned build failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Search failed")
        }
    }
}

/// The collection-scope q leg for one planned request: rev-gated response
/// cache over (params, revision), names from the shared collectionQueryCache
/// (single-flight walk inside collq::get_names), owned intersect + shaping
/// in one pinned read transaction. Node-owned outcomes proxy verbatim.
async fn serve_collq(st: AppState, req: Request, pool: Arc<DbPool>, cq: Arc<collq::CollectionQ>) -> Response {
    let headers = req.headers();
    let inm = headers.get("if-none-match").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).map(|s| s.to_string());

    let rev_i = pool
        .get()
        .ok()
        .and_then(|c| crate::db::current_revision(&c).ok().flatten())
        .unwrap_or(-1);

    let key = cq.cache_key();

    // Match list FIRST: Node derives X-Catalog-Cache/X-Source from the match
    // list entry's age per request, so even cache replays need it fresh.
    // Shared in-flight walk, durable-row reuse, and the deterministic engine
    // verdicts (404 empty / 400-422 invalid) resolve here; anything Node
    // must answer itself comes back as Proxy.
    let (entry, resolved_now) = match collq::get_names(st.clone(), &cq).await {
        collq::NamesOutcome::Ready(e, r) => (e, r),
        collq::NamesOutcome::Proxy => return crate::proxy(State(st.clone()), req).await,
        collq::NamesOutcome::InvalidQuery => return json_error(StatusCode::BAD_REQUEST, "INVALID_QUERY"),
    };
    let status: &'static str = if resolved_now { "resolved" } else { collq::status_of(&entry) };
    // resolveCollectionQuery returns source: LIVE_SOURCE on every
    // sidecar-mode path (lines 1456/1489/1525) — age only drives
    // X-Catalog-Cache, never X-Source. 'local-fallback' belongs to Node.
    let source: &'static str = "sidecar";

    // readCatalogSnapshot's remote-mode check (iter-16): Node verifies the
    // caller-supplied snapshot AFTER the match list resolves (an engine 400
    // outranks the 409) and BEFORE answering, on a per-request pinned read
    // txn. The response cache never keys on the snapshot — every replay,
    // fresh or cached, passes this check against the current revision.
    if let Some(exp) = cq.expected_snapshot.as_deref() {
        if let Err(actual) = collq::check_remote_snapshot(&pool, &entry.names, exp) {
            tracing::debug!(expected = %exp, actual = %actual, "catalog snapshot mismatch");
            return json_error(StatusCode::CONFLICT, "CATALOG_SNAPSHOT_CHANGED");
        }
    }

    if let Some(e) = st.search_cache.get(&key, rev_i) {
        return respond_entry(&e, inm.as_deref(), ae.as_deref(), Some((status, source)));
    }

    let cache = Arc::clone(&st.search_cache);
    let slot_key = format!("{key}\u{0}{rev_i}");
    let (slot, _slot_guard) = crate::flight::slot_guarded(&cache, &slot_key);
    let init_cache = Arc::clone(&cache);
    let init_key = key.clone();
    let build_pool = Arc::clone(&pool);
    let build_cq = (*cq).clone();
    let res: Result<&Result<Option<Arc<SearchEntry>>, String>, String> = slot
        .get_or_try_init(move || async move {
            // Budget-gated compute (iter 25), twin of the collection cache.
            let _permit = init_cache.build_permit().await;
            let built = tokio::task::spawn_blocking(move || collq::build(&build_pool, &build_cq, &entry))
                .await
                .unwrap_or_else(|e| collq::BuildOutcome::Failed(format!("collq build task panicked: {e}")));
            let built = match built {
                collq::BuildOutcome::Built(b) => b,
                collq::BuildOutcome::Proxy => return Ok(Ok(None)),
                collq::BuildOutcome::Failed(e) => return Err(e),
            };
            let body = bytes::Bytes::from(built.raw);
            let etag = body_etag(&body);
            let gz = gzip_bytes(&body);
            Ok(Ok(Some(init_cache.insert(
                init_key,
                SearchEntry {
                    rev: rev_i,
                    raw: body,
                    gz,
                    etag,
                    total: None,
                    source,
                    x_source: true,
                    catalog: Some(CatalogMeta {
                        total: built.total_header,
                        complete: built.complete,
                        upstream_total: built.upstream_total,
                        status,
                        snapshot: built.snapshot,
                    }),
                    stale: Arc::new(Vec::new()),
                },
            ))))
        })
        .await;
    drop(_slot_guard);
    match res {
        Ok(Ok(Some(e))) => respond_entry(e, inm.as_deref(), ae.as_deref(), Some((status, source))),
        Ok(Ok(None)) => crate::proxy(State(st.clone()), req).await,
        Ok(Err(ref e)) | Err(ref e) => {
            tracing::error!(error = %e, "collq build failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Search failed")
        }
    }
}

/// One of Node's two SQLite answer legs (collq::build_local), served per
/// request without touching the response cache: Node runs a dedicated read
/// transaction every time, so snapshot/pinning semantics are per-request
/// there. `fallback` carries the rebuilt request + walk plan for the tag
/// leg's isReady fall-through (Node's `if (local) return local;` else walk).
async fn serve_local(
    st: AppState,
    pool: Arc<DbPool>,
    lq: Arc<collq::LocalQ>,
    inm: Option<String>,
    ae: Option<String>,
    fallback: Option<(Request, Arc<collq::CollectionQ>)>,
) -> Response {
    let outcome = {
        let pool = Arc::clone(&pool);
        let lq = Arc::clone(&lq);
        match tokio::task::spawn_blocking(move || collq::build_local(&pool, &lq)).await {
            Ok(o) => o,
            Err(e) => collq::LocalOutcome::Failed(format!("local build task panicked: {e}")),
        }
    };
    let (cards, total, snapshot) = match outcome {
        collq::LocalOutcome::Built { cards, total, snapshot } => (cards, total, snapshot),
        collq::LocalOutcome::NotReady => {
            // supportsLocalCollectionQuery passed but isReady failed — Node
            // continues to the walk below resolveCollectionQuery's local
            // branch and pays for the engine.
            match fallback {
                Some((req, cq)) => {
                    // The walk leg carries its own snapshot check
                    // (serve_collq → collq::check_remote_snapshot, iter-16):
                    // a snapshot-carrying request is answered natively —
                    // rows or 409 — instead of proxying.
                    return serve_collq(st, req, pool, cq).await;
                }
                None => {
                    tracing::error!("local tag leg NotReady without a walk fallback");
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Search failed");
                }
            }
        }
        collq::LocalOutcome::Mismatch { actual } => {
            // readCatalogSnapshot throw → route 409 twin (Node logs the
            // expected/actual pair on the error object, not the body).
            tracing::debug!(expected = %lq.expected_snapshot.as_deref().unwrap_or(""), actual = %actual, "catalog snapshot mismatch");
            return json_error(StatusCode::CONFLICT, "CATALOG_SNAPSHOT_CHANGED");
        }
        collq::LocalOutcome::Failed(e) => {
            tracing::error!(error = %e, "local answer build failed");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Search failed");
        }
    };
    // res.json(cards): compact JSON body, ETag over it, the catalog header
    // set with cacheStatus 'local' and no X-Source (route line 158 only
    // adds it for 'sidecar'/'local-fallback').
    let raw = match serde_json::to_vec(&cards) {
        Ok(v) => bytes::Bytes::from(v),
        Err(e) => {
            tracing::error!(error = %e, "local answer serialize failed");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Search failed");
        }
    };
    let etag = body_etag(&raw);
    let gz = gzip_bytes(&raw);
    let entry = Arc::new(SearchEntry {
        rev: 0,
        raw,
        gz,
        etag,
        total: None,
        source: "local",
        x_source: false,
        catalog: Some(CatalogMeta {
            total,
            complete: true,
            upstream_total: None,
            status: "local",
            snapshot,
        }),
        stale: Arc::new(Vec::new()),
    });
    respond_entry(&entry, inm.as_deref(), ae.as_deref(), Some(("local", "local")))
}

#[cfg(test)]
mod cache_budget_tests {
    use super::*;

    fn entry(rev: i64, body: &str) -> SearchEntry {
        let raw = Bytes::from(body.as_bytes().to_vec());
        SearchEntry {
            rev,
            raw,
            gz: Bytes::from(Vec::new()),
            etag: String::new(),
            total: None,
            source: "cache",
            x_source: true,
            catalog: None,
            stale: Arc::new(Vec::new()),
        }
    }

    fn budget_cache(max_bytes: u64) -> SearchCache {
        let c = SearchCache::new();
        c.max_bytes.store(max_bytes, Ordering::Relaxed);
        c
    }

    #[test]
    fn byte_budget_evicts_least_recently_served() {
        // Three ~1 KiB entries under a 2.5 KiB budget: the two oldest are
        // gone unless a serve refreshed them.
        let c = budget_cache(2560);
        let body = "x".repeat(1024);
        c.insert("a".into(), entry(1, &body));
        c.insert("b".into(), entry(1, &body));
        assert!(c.get("a", 1).is_some()); // refresh a -> b is now the LRU
        c.insert("c".into(), entry(1, &body));
        assert!(c.get("a", 1).is_some(), "recently-served entry must survive");
        assert!(c.get("b", 1).is_none(), "LRU victim must be evicted");
        assert!(c.get("c", 1).is_some());
        let e = c.inner.guard();
        assert!(e.total_bytes <= 2560, "stored bytes must fit the budget");
        assert!(e.bytes.values().sum::<u64>() == e.total_bytes);
    }

    #[test]
    fn superseded_generation_entries_are_pruned() {
        let c = budget_cache(u64::MAX);
        c.insert("a".into(), entry(1, "old generation"));
        c.insert("b".into(), entry(2, "new generation"));
        // get filters by exact rev even pre-pruning; post-pruning the dead
        // body must not count against the store at all.
        assert!(c.get("a", 1).is_none());
        let e = c.inner.guard();
        assert!(!e.entries.contains_key("a"), "dead-rev entry must be dropped");
        assert!(!e.bytes.contains_key("a"), "dead-rev bytes must be uncounted");
    }

    #[test]
    fn count_cap_still_clears_and_resets_counters() {
        let c = budget_cache(u64::MAX);
        for i in 0..66 {
            c.insert(format!("k{i}"), entry(1, "x"));
        }
        let e = c.inner.guard();
        assert_eq!(e.entries.len(), 1);
        assert_eq!(e.total_bytes, 1); // the single "x" body, counters reset on clear
    }

    #[test]
    fn env_overrides_default_budget() {
        std::env::set_var("SCRYBOX_SEARCH_CACHE_MAX_MB", "7");
        assert_eq!(search_store_max_bytes(), 7 << 20);
        std::env::set_var("SCRYBOX_SEARCH_CACHE_MAX_MB", "garbage");
        assert_eq!(search_store_max_bytes(), SEARCH_STORE_MAX_BYTES_DEFAULT);
        std::env::remove_var("SCRYBOX_SEARCH_CACHE_MAX_MB");
    }

    fn trim_cache(threshold: u64) -> SearchCache {
        let c = SearchCache::new();
        c.trim_dirty_bytes.store(threshold, Ordering::Relaxed);
        c
    }

    #[test]
    fn big_eviction_arms_trim_small_ones_do_not() {
        // Budget eviction freeing >= the threshold arms the deferred trim;
        // eviction below it stays silent (allocator reuse is enough).
        let c = trim_cache(2000);
        let body = "x".repeat(1024);
        c.insert("a".into(), entry(1, &body));
        assert!(!c.take_trim(), "first insert frees nothing");
        c.insert("b".into(), entry(1, &body)); // fits the default budget, frees 0
        assert!(!c.take_trim(), "plain inserts free nothing");
        let c2 = trim_cache(2000);
        c2.max_bytes.store(1500, Ordering::Relaxed);
        c2.insert("a".into(), entry(1, &body));
        c2.insert("b".into(), entry(1, &body)); // evicts a (1024 freed < 2000)
        assert!(!c2.take_trim(), "small eviction must not arm trim");
        // Now cross the threshold: two 2 KiB entries, budget 3 KiB, and one
        // 4 KiB insert evicting both.
        let big = "y".repeat(2048);
        let c3 = trim_cache(2000);
        c3.max_bytes.store(3072, Ordering::Relaxed);
        c3.insert("a".into(), entry(1, &big));
        c3.insert("b".into(), entry(1, &big));
        assert!(c3.take_trim(), "two 2 KiB frees (4096) cross the threshold");
        assert!(!c3.take_trim(), "take_trim swaps the flag off");
    }

    #[test]
    fn generation_prune_arms_trim() {
        let c = trim_cache(2000);
        c.insert("a".into(), entry(1, &"x".repeat(4096)));
        assert!(!c.take_trim());
        c.insert("b".into(), entry(2, "new generation")); // prunes 4096 bytes
        assert!(c.take_trim(), "dead-generation prune >= threshold must arm");
    }

    #[test]
    fn count_cap_clear_arms_trim() {
        let c = trim_cache(2000);
        let body = "x".repeat(1024);
        for i in 0..65 {
            c.insert(format!("k{i}"), entry(1, &body));
        }
        assert!(!c.take_trim(), "no big drop while under the count cap");
        c.insert("cap".into(), entry(1, &body)); // 66th sees len>64 -> clears the store
        assert!(c.take_trim(), "count-cap clear frees the store -> arms trim");
    }
}

#[cfg(test)]
mod poison_m2 {
    //! REVIEW-errors-maint M2 (lane-A remainder): search-store lock sites go
    //! through `crate::poison::LockExt` — one panic under the guard must not
    //! poison every later get/insert (see poison.rs module docs).
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    fn entry(rev: i64, body: &str) -> SearchEntry {
        SearchEntry {
            rev,
            raw: Bytes::from(body.as_bytes().to_vec()),
            gz: Bytes::from(Vec::new()),
            etag: String::new(),
            total: None,
            source: "cache",
            x_source: true,
            catalog: None,
            stale: Arc::new(Vec::new()),
        }
    }

    #[test]
    fn poisoned_store_still_serves_and_stores() {
        let c = SearchCache::new();
        c.insert("a".into(), entry(1, "body-A"));
        let poisoned = catch_unwind(AssertUnwindSafe(|| {
            std::thread::scope(|s| {
                s.spawn(|| {
                    let _g = c.inner.lock().unwrap();
                    panic!("simulated panic under the search store guard");
                });
            });
        }));
        assert!(poisoned.is_err(), "precondition: the poisoning thread must panic");
        // Pre-fix (`.lock().unwrap()`) BOTH of these panicked with "mutex is poisoned".
        assert!(c.get("a", 1).is_some(), "reads must survive a poisoned store");
        c.insert("b".into(), entry(1, "body-B"));
        assert!(c.get("b", 1).is_some(), "writes must survive a poisoned store");
    }
}

#[cfg(test)]
mod headers_l2 {
    //! REVIEW-errors-maint L2 (lane-A): respond_entry binds the four
    //! catalog-derived header values through HeaderValue::from_str(..).ok().
    //! Pre-fix these were .unwrap() — a control character in any of them
    //! panicked the search hot path. Red before the fix (the poisoning
    //! values panic the handler); green after (header skipped, response
    //! intact). Valid values must still produce the full header set — the
    //! parity-relevant control.
    use super::*;

    fn entry_with_catalog(snapshot: &str, total: Option<&str>, upstream: Option<&str>) -> SearchEntry {
        SearchEntry {
            rev: 1,
            raw: Bytes::from_static(br#"{"cards":[]}"#),
            gz: Bytes::from_static(br#""#),
            etag: "W/\"l2\"".to_string(),
            total: None,
            source: "sidecar",
            x_source: true,
            catalog: Some(CatalogMeta {
                total: total.map(|t| t.to_string()),
                complete: true,
                upstream_total: upstream.map(|u| u.to_string()),
                status: "hit",
                snapshot: snapshot.to_string(),
            }),
            stale: Arc::new(Vec::new()),
        }
    }

    fn serve(e: SearchEntry) -> Response {
        respond_entry(&Arc::new(e), None, None, None)
    }

    #[test]
    fn valid_catalog_values_serve_the_full_header_set() {
        let r = serve(entry_with_catalog("snap-2026-11-13", Some("123"), Some("456")));
        assert_eq!(r.status(), StatusCode::OK);
        let h = r.headers();
        assert_eq!(h.get("x-catalog-snapshot").and_then(|v| v.to_str().ok()), Some("snap-2026-11-13"));
        assert_eq!(h.get("x-total-count").and_then(|v| v.to_str().ok()), Some("123"));
        assert_eq!(h.get("x-catalog-upstream-total").and_then(|v| v.to_str().ok()), Some("456"));
        assert_eq!(h.get("x-source").and_then(|v| v.to_str().ok()), Some("sidecar"));
        assert_eq!(h.get("x-catalog-cache").and_then(|v| v.to_str().ok()), Some("hit"));
    }

    #[test]
    fn control_char_in_snapshot_skips_the_header_instead_of_panicking() {
        let r = serve(entry_with_catalog("snap\nX-Evil: 1", None, None));
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r.headers().get("x-catalog-snapshot").is_none(), "invalid snapshot must skip the header, not panic");
        // The rest of the contract survives.
        assert_eq!(r.headers().get("x-catalog-cache").and_then(|v| v.to_str().ok()), Some("hit"));
    }

    #[test]
    fn control_char_in_total_and_upstream_skips_only_their_headers() {
        let r = serve(entry_with_catalog("ok", Some("1 2\n"), Some("x\r\n")));
        assert_eq!(r.status(), StatusCode::OK);
        let h = r.headers();
        assert!(h.get("x-total-count").is_none(), "invalid c.total must skip the header");
        assert!(h.get("x-catalog-upstream-total").is_none(), "invalid upstream_total must skip the header");
        assert_eq!(h.get("x-catalog-snapshot").and_then(|v| v.to_str().ok()), Some("ok"));
    }

    #[test]
    fn control_char_in_source_skips_x_source_only() {
        let mut e = entry_with_catalog("ok", None, None);
        e.source = "bad\nvalue";
        let r = serve(e);
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r.headers().get("x-source").is_none(), "invalid source must skip x-source, not panic");
        assert_eq!(r.headers().get("x-catalog-snapshot").and_then(|v| v.to_str().ok()), Some("ok"));
    }
}

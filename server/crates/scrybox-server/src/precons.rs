//! LANE A — iter-35: precons group (route switch `precons`).
//! Port of `backend/src/routes/precons.js` + `backend/src/utils/preconData.js`
//! (+ the `scryfallApi.bulkFetchByIdentifier` leg the route injects as
//! `cardById`, run through the shared Scry client exactly like moxfield.rs —
//! the lane-B client is reused, never re-implemented).
//!
//!   GET  /api/precons?q=...   ranked search over the (cached) MTGJSON index
//!   POST /api/precons/import  fetch the deck list, resolve printings, create
//!                             the deck
//!
//! Both sit behind the global `app.use('/api', authenticateToken)` mount
//! (server.js:454) — EVERY /api/precons path (including unknown subpaths)
//! answers the auth 401 before Express would fall through to the static 404;
//! the two registered shapes therefore self-check group+method and forward
//! everything else, so nothing 405s and unknown shapes reach Node unchanged.
//!
//! Byte-parity surface (BRIEF rule 1): `res.json` body shape + key ORDER.
//! GET answers `{results, stale, mtgjsonDate}` where every result keeps the
//! mirror's 6-key writeIndex shape (name, code, setName, type, releaseDate,
//! fileName — the map is built here with serde_json's preserve_order). POST
//! answers `{message, id, cards, notFound}` / `{error, notFound}`.
//!
//! Mirror-file sharing: getPreconIndex reads/writes data/precons.json — the
//! SAME host directory the Node container bind-mounts at /app/database, so
//! both processes share one mirror exactly like two Node processes would
//! (fixed `.tmp` name + rename, like Node's writeFileSync + renameSync).
//!
//! Hydration policy: any state where the Node twin would produce an answer
//! Rust cannot byte-guarantee locally (Scryfall client error, an unmatched
//! uniq row — Node answers that state as a PARTIAL deck with notFound>0)
//! forwards the WHOLE untouched request to Node before any local write, so
//! Node itself produces the response and performs the writes. When Rust can
//! reproduce Node's local-success shape exactly, it answers locally.
//!
//! rankPrecons' tertiary `a.name.localeCompare(b.name)` runs the SAME root
//! collator the decks_reads localeCompare twin pinned (decks_reads::
//! root_collator), so tie order is byte-faithful, not code-point.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;

use crate::auth::{authenticate, bearer_token};
use crate::card_cache::cache_normalized_cards;
use crate::card_norm::normalize_card;
use crate::collection::truthy;
use crate::decks_reads::root_collator;
use crate::scryfall::ScryOpts;
use crate::AppState;

/// preconData.MTGJSON.
pub const MTGJSON: &str = "https://mtgjson.com/api/v5";
/// preconData.FETCH_TIMEOUT — the module-local axios instance timeout.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// preconData.INDEX_MAX_AGE_MS — re-download once Date.now()-fetchedAt >= it.
const INDEX_MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000;
/// express.json default limit (normalJson), the POST import body cap.
pub(crate) const BODY_LIMIT: usize = 1024 * 1024;

/// `String.prototype.trim` twin (WhiteSpace + LineTerminator set) — same
/// helper the settings port pinned.
pub(crate) fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// JS `String(v)` over JSON values.
pub(crate) fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `String(v || '')` — falsy (undefined/null/''/0/false) collapses to ''.
pub(crate) fn js_str_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(v) if truthy(v) => js_string(v),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Type whitelist (precons.js, verbatim)
// ---------------------------------------------------------------------------

/// COMMANDER_TYPES — Node's Set literal repeats 'Commander Deck'; a Set
/// dedupes, this is its content.
const COMMANDER_TYPES: &[&str] = &["Commander Deck", "MTGO Commander Deck", "Oathbreaker Deck"];

/// PRECON_TYPES — verbatim from precons.js (also repeats 'Commander Deck'
/// in source; deduped).
const PRECON_TYPES: &[&str] = &[
    "Commander Deck", "Theme Deck", "Intro Pack", "Planeswalker Deck", "Duel Deck",
    "Challenger Deck", "Pioneer Challenger Deck", "Event Deck", "Modern Event Deck",
    "Starter Deck", "Starter Kit", "Spellslinger Starter Kit", "Welcome Deck",
    "Game Night Deck", "Clash Pack", "Brawl Deck", "Guild Kit", "Archenemy Deck",
    "Planechase Deck", "Premium Deck", "Enhanced Deck", "Advanced Deck",
    "Challenge Deck", "Box Set",
];

/// `isPrecon(d)` — Set membership on d.type (exact string; a non-string
/// type simply never matches).
pub fn is_precon(d: &Value) -> bool {
    d.get("type").and_then(|v| v.as_str()).map(|t| PRECON_TYPES.contains(&t)).unwrap_or(false)
}

/// defaultShape(type): (format, accentColor, targetSize); category is
/// always null in both branches (and `category || shape.category` keeps it
/// null for every falsy body value).
fn default_shape(type_: &str) -> (&'static str, &'static str, i64) {
    if COMMANDER_TYPES.contains(&type_) {
        ("Commander / EDH", "#8b5cf6", 100)
    } else {
        ("Standard", "#eab308", 60)
    }
}

// ---------------------------------------------------------------------------
// rankPrecons (preconData.js, verbatim semantics)
// ---------------------------------------------------------------------------

/// JS `String.prototype.indexOf` twin returning the UTF-16 code-unit
/// position Node's `_at` carries.
fn index_of(hay: &str, needle: &str) -> i64 {
    if needle.is_empty() {
        return 0;
    }
    match hay.find(needle) {
        Some(byte) => hay[..byte].chars().map(|c| if u32::from(c) > 0xFFFF { 2 } else { 1 }).sum(),
        None => -1,
    }
}

/// `for (const ch of needle)` iterates CODE POINTS; this is that iterator
/// over an already-lowercased needle.
fn code_points(s: &str) -> Vec<String> {
    s.chars().map(|c| c.to_string()).collect()
}

/// rankPrecons(decks, q): score fold (0 name-substring, 1 hay-substring,
/// 2 name-subsequence, drop otherwise) + the 3-key ascending sort
/// (_score, _at, name localeCompare). Returns clones with `_score`/`_at`
/// attached in Node's key order (they were appended last by the spread).
pub fn rank_precons(decks: &[Value], q: &str) -> Vec<Value> {
    let needle = q.to_lowercase();
    let needle_pts = code_points(&needle);
    let subseq = |hay: &str| -> bool {
        let mut i = 0usize;
        for ch in &needle_pts {
            match hay[i..].find(ch.as_str()) {
                Some(at) => i += at + ch.len(),
                None => return false,
            }
        }
        true
    };
    let mut scored: Vec<(i64, i64, String, Value)> = Vec::new();
    for d in decks {
        let name = js_str_or_empty(d.get("name")).to_lowercase();
        let hay = format!(
            "{} {} {} {}",
            name,
            js_str_or_empty(d.get("code")).to_lowercase(),
            js_str_or_empty(d.get("setName")).to_lowercase(),
            js_str_or_empty(d.get("type")).to_lowercase()
        );
        // Node's ladder (preconData.js, verbatim): name substring → 0,
        // hay substring → 1, name-subsequence → 2, else dropped.
        let score = if name.contains(&needle) {
            0
        } else if hay.contains(&needle) {
            1
        } else if subseq(&name) {
            2
        } else {
            -1
        };
        if score >= 0 {
            let at = index_of(&name, &needle);
            let mut v = d.clone();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("_score".into(), Value::Number(score.into()));
                obj.insert("_at".into(), Value::Number(at.into()));
            }
            scored.push((score, at, js_str_or_empty(d.get("name")), v));
        }
    }
    // V8's sort and Rust's sort_by are both stable; the comparator is
    // Node's `a._score - b._score || a._at - b._at ||
    // a.name.localeCompare(b.name)` — note localeCompare runs on the RAW
    // (not lowercased) name, through the shared ICU collator twin.
    let coll = root_collator();
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then_with(|| coll.compare(a.2.as_str(), b.2.as_str()))
    });
    scored.into_iter().map(|(_, _, _, v)| v).collect()
}

// ---------------------------------------------------------------------------
// MTGJSON fetch (preconData.js `client = axios.create({ timeout: 60000,
// headers: { 'User-Agent': 'Scrybox/1.0' } })` twin)
// ---------------------------------------------------------------------------

type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, String>;

fn https_client() -> &'static HttpsClient {
    static C: OnceLock<HttpsClient> = OnceLock::new();
    C.get_or_init(|| {
        let mut https = HttpConnector::new();
        https.set_nodelay(true);
        https.enforce_http(false);
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .unwrap()
            .https_or_http()
            .enable_http1()
            .build();
        Client::builder(TokioExecutor::new()).build(https)
    })
}

/// `client.get(url)` twin. The Err string is the axios rejection message
/// the route templates into its own error text (`err.message`).
pub async fn fetch_json(url: &str) -> Result<Value, (String, Option<u16>)> {
    let req = hyper::Request::get(url)
        .header("user-agent", HeaderValue::from_static("Scrybox/1.0"))
        .body(String::new())
        .map_err(|e| (format!("{url}: {e}"), None))?;
    let res = tokio::time::timeout(FETCH_TIMEOUT, https_client().request(req))
        .await
        // AxiosError message for the instance timeout (no response → no
        // AxiosError.status → the route's `err.status ? err.status : 500`).
        .map_err(|_| ("timeout of 60000ms exceeded".to_string(), None))?
        .map_err(|e| (format!("{url}: {e}"), None))?;
    let status = res.status().as_u16();
    if !(200..300).contains(&status) {
        // AxiosError with the default validateStatus carries BOTH: the
        // message verbatim AND .status = response.status (axios 1.18 probe:
        // `e.status` is the numeric code) — the route's catch forwards it.
        return Err((format!("Request failed with status code {status}"), Some(status)));
    }
    let body = BodyExt::collect(res.into_body())
        .await
        .map_err(|e| (format!("{url}: {e}"), None))?
        .to_bytes();
    // JSON parse failure: axios (json default) would reject with a SyntaxError
    // — no .status → 500. Rare; the message differs but the STATUS matches.
    serde_json::from_slice::<Value>(&body).map_err(|e| (format!("{url}: invalid JSON response ({e})"), None))
}

// ---------------------------------------------------------------------------
// getPreconIndex (preconData.js)
// ---------------------------------------------------------------------------

pub struct IndexOutcome {
    /// The mirror/writeIndex deck objects (6 keys, Node order).
    pub decks: Vec<Value>,
    /// 'cache' | 'fresh' | 'stale' — the route derives `stale` from this.
    pub source: &'static str,
    /// `(payload.meta && payload.meta.date) || ''` (fresh/write path) or the
    /// mirror's field (cache/stale paths); the route answers `|| ''` on
    /// falsy, so anything non-truthy collapses to ''.
    pub mtgjson_date: Value,
}

/// Error carrying the Node `err.status` the route reads (else 500).
pub struct PreconError {
    pub status: u16,
    pub message: String,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `new Date().toISOString()` twin.
fn now_iso() -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms())
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// `Date.parse(s)` twin for the mirror's fetchedAt (ISO 8601 written by
/// toISOString; anything else parses to NaN → age check false).
fn date_parse(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .and_then(|ndt| ndt.and_utc().timestamp_millis().try_into().ok())
        })
}

fn read_index(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    let raw: Value = serde_json::from_slice(&bytes).ok()?;
    // `raw && Array.isArray(raw.decks) && raw.decks.length`
    if raw.get("decks").and_then(|v| v.as_array()).map(|a| !a.is_empty()).unwrap_or(false) {
        Some(raw)
    } else {
        None
    }
}

async fn write_index(path: &Path, payload: &Value, set_names: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    // writeIndex: map payload.data to the 6-key shape, setName from the
    // (UPPER-cased code) SetList map, keep only rows with name && fileName.
    let rows = payload.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut decks: Vec<Value> = Vec::new();
    for d in &rows {
        let obj: Map<String, Value> = [
            ("name", js_str_or_empty(d.get("name"))),
            ("code", js_str_or_empty(d.get("code"))),
            (
                "setName",
                set_names.get(&js_str_or_empty(d.get("code")).to_uppercase()).map(js_string).unwrap_or_default(),
            ),
            ("type", js_str_or_empty(d.get("type"))),
            ("releaseDate", js_str_or_empty(d.get("releaseDate"))),
            ("fileName", js_str_or_empty(d.get("fileName"))),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), Value::String(v)))
        .collect();
        if truthy(obj.get("name").unwrap()) && truthy(obj.get("fileName").unwrap()) {
            decks.push(Value::Object(obj));
        }
    }
    if decks.is_empty() {
        return Err("DeckList payload carried no decks".into());
    }
    let mut doc = Map::new();
    doc.insert("fetchedAt".into(), Value::String(now_iso()));
    let mtgjson_date = payload
        .get("meta")
        .and_then(|m| m.get("date"))
        .filter(|v| truthy(v))
        .cloned()
        .unwrap_or(Value::String(String::new()));
    doc.insert("mtgjsonDate".into(), mtgjson_date);
    doc.insert("decks".into(), Value::Array(decks));
    // `${indexPath}.tmp` + rename — the SAME fixed temp name Node uses, so
    // the two processes also share the tmp path exactly.
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("precons.json")
    ));
    let bytes = serde_json::to_vec(&Value::Object(doc.clone())).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let mut f = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
    f.write_all(&bytes).await.map_err(|e| e.to_string())?;
    f.flush().await.map_err(|e| e.to_string())?;
    drop(f);
    tokio::fs::rename(&tmp, path).await.map_err(|e| e.to_string())?;
    Ok(doc)
}

/// setNameMap: `payload.data` rows with code && name → { UPPER(code): name }.
fn set_name_map(payload: Option<&Value>) -> Map<String, Value> {
    let mut map = Map::new();
    for s in payload.and_then(|p| p.get("data")).and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        let code = s.get("code");
        let name = s.get("name");
        if code.map(truthy).unwrap_or(false) && name.map(truthy).unwrap_or(false) {
            map.insert(js_string(code.unwrap()).to_uppercase(), name.unwrap().clone());
        }
    }
    map
}

fn cached_fields(cached: &Value) -> (Vec<Value>, Value) {
    let decks = cached.get("decks").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mtgjson_date = cached.get("mtgjsonDate").cloned().unwrap_or(Value::Null);
    (decks, mtgjson_date)
}

/// getPreconIndex: serve a fresh mirror; else download DeckList.json +
/// SetList.json in parallel (`Promise.all`, SetList best-effort), rewrite
/// the mirror, fall back to the mirror on ANY failure, else 502.
pub async fn get_precon_index(base: &str, index_path: &Path) -> Result<IndexOutcome, PreconError> {
    let cached = read_index(index_path);
    // `hasSetNames` + the 24 h window decide the re-download; an
    // unparseable fetchedAt is Date.parse → NaN → the age comparison is
    // false → a set-name-carrying mirror serves INDEFINITELY, Node alike.
    let needs_fetch = match &cached {
        None => true,
        Some(c) => {
            let has_set_names = c
                .get("decks")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().any(|d| d.get("setName").map(truthy).unwrap_or(false)))
                .unwrap_or(false);
            if !has_set_names {
                true
            } else {
                match c.get("fetchedAt").and_then(|v| v.as_str()).and_then(date_parse) {
                    Some(ms) => now_ms() - ms >= INDEX_MAX_AGE_MS,
                    None => false,
                }
            }
        }
    };
    if !needs_fetch {
        let (decks, mtgjson_date) = cached_fields(cached.as_ref().unwrap());
        return Ok(IndexOutcome { decks, source: "cache", mtgjson_date });
    }
    // Promise.all([client.get(INDEX_URL), client.get(SETLIST_URL).catch(→null)])
    let index_url = format!("{base}/DeckList.json");
    let setlist_url = format!("{base}/SetList.json");
    let (deck_list, set_list) = tokio::join!(fetch_json(&index_url), fetch_json(&setlist_url));
    let set_names = match set_list {
        Ok(payload) => set_name_map(Some(&payload)),
        Err(_) => Map::new(), // .catch(() => null) → setNameMap skipped
    };
    let outcome = match deck_list {
        Ok(payload) => match write_index(index_path, &payload, &set_names).await {
            Ok(doc) => Ok(IndexOutcome {
                decks: doc.get("decks").and_then(|v| v.as_array()).cloned().unwrap_or_default(),
                source: "fresh",
                mtgjson_date: doc.get("mtgjsonDate").cloned().unwrap_or(Value::Null),
            }),
            Err(message) => Err(message),
        },
        Err((message, _status)) => Err(message), // catch wraps with status 502 anyway
    };
    outcome.or_else(|message| match &cached {
        // catch { if (cached) return {...cached, source:'stale'} }
        Some(c) => {
            let (decks, mtgjson_date) = cached_fields(c);
            Ok(IndexOutcome { decks, source: "stale", mtgjson_date })
        }
        // No mirror and no network: nothing to search.
        None => Err(PreconError { status: 502, message: format!("Precon index unavailable ({message})") }),
    })
}

// ---------------------------------------------------------------------------
// getPreconCardList (preconData.js)
// ---------------------------------------------------------------------------

/// cleanFileName: `String(s || '').replace(/[^A-Za-z0-9_]+/g, '')` — strips
/// EVERY run of foreign characters (does not basename-split, does not
/// reject: 'etc/passwd.json' sanitises to 'etcpasswdjson').
pub fn clean_file_name(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect()
}

/// getPreconCardList: clean the name, fetch `${DECKS_URL}${safe}.json`
/// (encodeURIComponent is the identity on the cleaned charset), require
/// `data.mainBoard`, and fold the mainBoard/commander sections.
pub async fn get_precon_card_list(base: &str, file_name: &str) -> Result<Value, PreconError> {
    let safe = clean_file_name(file_name);
    if safe.is_empty() {
        return Err(PreconError { status: 400, message: "Invalid precon file name".into() });
    }
    // AxiosError rejection: .status = the upstream HTTP status when there IS
    // a response (the route's `err.status ? err.status : 500` forwards it);
    // network/timeout rejections have none → 500 with the axios message.
    let payload = fetch_json(&format!("{base}/decks/{safe}.json"))
        .await
        .map_err(|(message, status)| PreconError { status: status.unwrap_or(500), message })?;
    let data = match payload.get("data").filter(|v| !v.is_null()) {
        Some(d) if d.get("mainBoard").and_then(|v| v.as_array()).is_some() => d.clone(),
        _ => return Err(PreconError { status: 502, message: "Malformed MTGJSON deck file".into() }),
    };
    let mut cards: Vec<Value> = Vec::new();
    for (key, section) in [("mainBoard", "mainboard"), ("commander", "commander")] {
        for c in data.get(key).and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            let count = match c.get("count").and_then(|v| v.as_f64()) {
                Some(f) if f.is_finite() => c.get("count").cloned().unwrap(),
                _ => Value::from(1),
            };
            let scryfall_id = c
                .get("identifiers")
                .and_then(|i| i.get("scryfallId"))
                .filter(|v| truthy(v))
                .map(js_string)
                .unwrap_or_default();
            let mut row = Map::new();
            row.insert("setCode".into(), Value::String(js_str_or_empty(c.get("setCode"))));
            row.insert("number".into(), Value::String(js_str_or_empty(c.get("number"))));
            row.insert("count".into(), count);
            row.insert("scryfallId".into(), Value::String(scryfall_id));
            row.insert("section".into(), Value::String(section.to_string()));
            cards.push(Value::Object(row));
        }
    }
    let nullable = |v: Option<&Value>| -> Value {
        match v {
            Some(v) if truthy(v) => v.clone(),
            _ => Value::Null,
        }
    };
    let mut out = Map::new();
    out.insert("name".into(), nullable(data.get("name")));
    out.insert("code".into(), nullable(data.get("code")));
    out.insert("type".into(), nullable(data.get("type")));
    out.insert("releaseDate".into(), nullable(data.get("releaseDate")));
    out.insert("cards".into(), Value::Array(cards));
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------------------
// bulkFetchByIdentifier (scryfallApi.js) twin on the shared Scry client
// ---------------------------------------------------------------------------

/// `scryfallUuid`: strip a leading `mtg-`, then the 8-4-4-4-12 hex shape.
fn scryfall_uuid(v: Option<&Value>) -> Option<String> {
    let raw = match v {
        Some(v) if !v.is_null() => js_string(v),
        _ => String::new(),
    };
    let raw = raw.strip_prefix("mtg-").unwrap_or(&raw).to_string();
    let parts: Vec<&str> = raw.split('-').collect();
    const LENS: [usize; 5] = [8, 4, 4, 4, 12];
    if parts.len() != 5 {
        return None;
    }
    for (p, l) in parts.iter().zip(LENS) {
        if p.len() != l || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
    }
    Some(raw)
}

pub(crate) struct BulkOut {
    pub(crate) cards: Vec<Value>,
    /// (uniq-row index, normalized card) in RESPONSE order, Node's pairs.
    pub(crate) pairs: Vec<(usize, Map<String, Value>)>,
    pub(crate) not_found: i64,
}

/// scryfallApi.bulkFetchByIdentifier twin, DEFAULT config (sidecar-eligible
/// via the shared Scry client — NOT the upstream-pinned sweep job): 75-row
/// batches, byKey Map (last set wins), scryPostRetried's 4 attempts.
pub(crate) async fn bulk_fetch_by_identifier(st: &AppState, rows: &[Map<String, Value>]) -> Result<BulkOut, String> {
    bulk_fetch_by_identifier_opts(st, rows, &ScryOpts::default()).await
}

/// The same twin with caller-chosen ScryOpts — mergeSearchPrices pins
/// `{upstream: true}` (TCGplayer quotes only exist on the real API; the
/// sidecar answers price-less).
pub(crate) async fn bulk_fetch_by_identifier_opts(
    st: &AppState,
    rows: &[Map<String, Value>],
    opts: &ScryOpts,
) -> Result<BulkOut, String> {
    let mut out = BulkOut { cards: Vec::new(), pairs: Vec::new(), not_found: 0 };
    for (ci, chunk) in rows.chunks(75).enumerate() {
        let chunk_start = ci * 75;
        let mut identifiers: Vec<Value> = Vec::with_capacity(chunk.len());
        let mut by_key: Vec<(String, usize)> = Vec::with_capacity(chunk.len());
        for (i, row) in chunk.iter().enumerate() {
            if let Some(uuid) = scryfall_uuid(
                // `scryfallUuid(row.id || row.card_id)` — JS || falls through
                // any FALSY id (missing, null, ''), not just missing.
                row.get("id").filter(|v| truthy(v)).or_else(|| row.get("card_id").filter(|v| truthy(v))),
            ) {
                by_key_push(&mut by_key, format!("id:{}", uuid.to_lowercase()), i);
                identifiers.push(serde_json::json!({ "id": uuid }));
                continue;
            }
            let set_opt = row.get("set_id");
            let num_opt = row.get("number");
            let set_present = set_opt.map(|v| !v.is_null()).unwrap_or(false);
            let num_present = num_opt.map(|v| !v.is_null()).unwrap_or(false);
            if set_present && num_present {
                let set_id = js_string(set_opt.unwrap()).to_lowercase();
                let num = js_string(num_opt.unwrap());
                if !set_id.is_empty() && !num.is_empty() {
                    by_key_push(&mut by_key, format!("sn:{}|{}", set_id, num.to_lowercase()), i);
                    let mut idm = Map::new();
                    idm.insert("set".into(), Value::String(set_id));
                    idm.insert("collector_number".into(), Value::String(num));
                    identifiers.push(Value::Object(idm));
                    continue;
                }
            }
            let name = js_str_or_empty(row.get("name"));
            by_key_push(&mut by_key, format!("n:{}", name.to_lowercase()), i);
            identifiers.push(serde_json::json!({ "name": name }));
        }
        let mut body = Map::new();
        body.insert("identifiers".into(), Value::Array(identifiers));
        let resp = st
            .scry
            .post_retried("/cards/collection", Value::Object(body), opts, 4)
            .await
            .map_err(|e| e.to_string())?;
        // `(resp.data && resp.data.not_found) || []`
        out.not_found += resp
            .as_ref()
            .and_then(|v| v.get("not_found"))
            .and_then(|v| v.as_array())
            .map(|a| a.len() as i64)
            .unwrap_or(0);
        for raw in resp.as_ref().and_then(|v| v.get("data")).and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            let norm = normalize_card(&raw, None);
            out.cards.push(norm.clone());
            let normm = norm.as_object().cloned().unwrap_or_default();
            let row_i = by_key
                .iter()
                .find(|(k, _)| {
                    *k == format!("id:{}", js_string(&raw.get("id").cloned().unwrap_or(Value::Null)).to_lowercase())
                })
                .or_else(|| {
                    by_key.iter().find(|(k, _)| {
                        *k == format!(
                            "sn:{}|{}",
                            js_str_or_empty(normm.get("set_id")).to_lowercase(),
                            js_string(&normm.get("number").cloned().unwrap_or(Value::String(String::new()))).to_lowercase()
                        )
                    })
                })
                .or_else(|| {
                    by_key
                        .iter()
                        .find(|(k, _)| *k == format!("n:{}", js_str_or_empty(normm.get("name")).to_lowercase()))
                })
                .map(|(_, i)| *i);
            if let Some(i) = row_i {
                out.pairs.push((chunk_start + i, normm));
            }
        }
    }
    Ok(out)
}

/// JS `Map.set` on the Vec-backed map: overwrite the value, keep position.
fn by_key_push(map: &mut Vec<(String, usize)>, key: String, i: usize) {
    if let Some(slot) = map.iter_mut().find(|(k, _)| *k == key) {
        slot.1 = i;
    } else {
        map.push((key, i));
    }
}

// ---------------------------------------------------------------------------
// Query-string semantics for req.query.q — served by the shared live-pinned
// qs twin (lists_reads::qs_string), incl. repeated-key array join, `q[]`/
// `q[<n>]` array forms, other-bracket -> '[object Object]', and qs's
// swallow-the-URIError decoder ('%zz' stays literal, request still 200s —
// container qs probe).
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub(crate) fn json_response(status: StatusCode, map: &Map<String, Value>) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        axum::Json(Value::Object(map.clone())),
    )
        .into_response()
}

pub(crate) fn json_error(status: StatusCode, msg: &str) -> Response {
    let mut m = Map::new();
    m.insert("error".into(), Value::String(msg.to_string()));
    json_response(status, &m)
}

fn internal_error() -> Response {
    json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
}

async fn read_capped(mut body: Body, limit: usize) -> (Vec<u8>, bool) {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = body.frame().await {
        match chunk {
            Ok(f) => {
                if let Some(data) = f.into_data().ok() {
                    if buf.len() + data.len() > limit {
                        return (buf, true);
                    }
                    buf.extend_from_slice(&data);
                }
            }
            Err(_) => return (buf, false),
        }
    }
    (buf, false)
}

/// express.json (default limit 1mb, strict) twin for the POST import:
/// non-JSON content-types leave req.body === {} (body-parser skips, the
/// route's `req.body || {}` keeps {}), malformed JSON answers the generic
/// 500 (global errorHandler), >1 MiB the 413 shape.
pub(crate) async fn read_json_body(req: Request, limit: usize) -> (Value, Option<Response>) {
    let is_json = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().replace(' ', "").contains("application/json"))
        .unwrap_or(false);
    if !is_json {
        return (Value::Object(Default::default()), None);
    }
    let (b, too_big) = read_capped(req.into_body(), limit).await;
    if too_big {
        return (Value::Null, Some(json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Upload too large. Try exporting/importing in smaller batches.",
        )));
    }
    if b.is_empty() {
        return (Value::Object(Default::default()), None);
    }
    match serde_json::from_slice::<Value>(&b) {
        Ok(v @ (Value::Object(_) | Value::Array(_))) => (v, None),
        _ => {
            tracing::error!("precons: invalid JSON body");
            (Value::Null, Some(internal_error()))
        }
    }
}

/// GET /api/precons — routes/precons.js GET /.
pub async fn route_precons(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("precons") || req.method() != Method::GET {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let headers: HeaderMap = req.headers().clone();
    let token = bearer_token(&headers).map(str::to_string);
    if let Err(res) = authenticate(&pool, token.as_deref(), true) {
        return res;
    }
    let q = js_trim(&crate::lists_reads::qs_string(req.uri().query(), "q")).to_string();
    if q.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "q is required");
    }
    let base = st.mtgjson_base.clone();
    let path = st.precons_index_path.clone();
    match get_precon_index(&base, &path).await {
        Ok(index) => {
            let filtered: Vec<Value> = index.decks.iter().filter(|d| is_precon(d)).cloned().collect();
            let results: Vec<Value> = rank_precons(&filtered, &q)
                .into_iter()
                .take(50)
                .map(|d| {
                    let mut obj = d.as_object().cloned().unwrap_or_default();
                    obj.remove("_score");
                    obj.remove("_at");
                    Value::Object(obj)
                })
                .collect();
            let mut out = Map::new();
            out.insert("results".into(), Value::Array(results));
            out.insert("stale".into(), Value::Bool(index.source == "stale"));
            out.insert(
                "mtgjsonDate".into(),
                if truthy(&index.mtgjson_date) { index.mtgjson_date } else { Value::String(String::new()) },
            );
            json_response(StatusCode::OK, &out)
        }
        Err(e) => {
            let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            json_error(status, &format!("Precon search failed: {}", e.message))
        }
    }
}

/// POST /api/precons/import — routes/precons.js POST /import plus
/// importPreconCardsIntoDeck (preconData.js).
pub async fn route_import(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("precons") || req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let headers: HeaderMap = req.headers().clone();
    // normalJson is mounted above authenticateToken (server.js), so
    // malformed/oversized bodies answer BEFORE the auth gate.
    let (body_req, early) = read_json_body(req, BODY_LIMIT).await;
    if let Some(res) = early {
        return res;
    }
    let token = bearer_token(&headers).map(str::to_string);
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return res,
    };
    let body_obj = body_req.as_object().cloned().unwrap_or_default();
    match body_obj.get("fileName") {
        Some(Value::String(s)) if !s.is_empty() => {}
        _ => return json_error(StatusCode::BAD_REQUEST, "fileName is required"),
    }
    let file_name = body_obj["fileName"].as_str().unwrap_or("").to_string();
    let base = st.mtgjson_base.clone();

    let list = match get_precon_card_list(&base, &file_name).await {
        Ok(v) => v,
        Err(e) => {
            let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            return import_failed(status, &e.message, Value::from(0));
        }
    };

    // `includeCommander = true` destructuring default fires only on
    // undefined; an explicit null is falsy.
    let include_commander = match body_obj.get("includeCommander") {
        None => true,
        Some(v) => truthy(v),
    };
    let rows_all: Vec<Map<String, Value>> = list
        .get("cards")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| {
            let section = c.get("section").and_then(|v| v.as_str()).unwrap_or("");
            (include_commander && section == "commander") || section == "mainboard"
        })
        .map(|c| {
            let mut r = Map::new();
            r.insert("set_id".into(), c.get("setCode").cloned().unwrap_or(Value::String(String::new())));
            r.insert("number".into(), c.get("number").cloned().unwrap_or(Value::String(String::new())));
            r.insert("quantity".into(), c.get("count").cloned().unwrap_or(Value::from(1)));
            r.insert("commander".into(), Value::Bool(c.get("section").and_then(|v| v.as_str()) == Some("commander")));
            r
        })
        .collect();

    // uniq fold — `u.quantity += r.quantity; u.commander = u.commander ||
    // !!r.commander` on the FIRST row's object; later rows with the same key
    // only feed the accumulator.
    let mut uniq: Vec<Map<String, Value>> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for r in rows_all {
        let key = format!(
            "{}|{}",
            js_string(r.get("set_id").unwrap_or(&Value::Null)).to_lowercase(),
            js_string(r.get("number").unwrap_or(&Value::Null)).to_lowercase()
        );
        match index.get(&key) {
            Some(&i) => {
                let lead = &mut uniq[i];
                let summed = js_num(lead.get("quantity")) + js_num(r.get("quantity"));
                lead.insert("quantity".into(), num_out(summed));
                let merged = truthy(lead.get("commander").unwrap_or(&Value::Bool(false)))
                    || truthy(r.get("commander").unwrap_or(&Value::Bool(false)));
                lead.insert("commander".into(), Value::Bool(merged));
            }
            None => {
                index.insert(key, uniq.len());
                uniq.push(r);
            }
        }
    }

    // Resolve every printing against Scryfall (the route injects
    // bulkFetchByIdentifier as cardById). Scryfall client errors and
    // unmatched uniq rows are states where Node answers PARTIALLY
    // (notFound>0) or with its own axios message — Rust replays the WHOLE
    // untouched request to Node instead of approximating; nothing local has
    // been written yet, so the replay is clean.
    let bulk = match bulk_fetch_by_identifier(&st, &uniq).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("precons import: scryfall bulk fetch failed ({e}); forwarding to Node");
            return forward_import(st, &headers, &body_req).await;
        }
    };
    let matched: HashMap<usize, ()> = bulk.pairs.iter().map(|(i, _)| (*i, ())).collect();
    if uniq.iter().enumerate().any(|(i, _)| !matched.contains_key(&i)) {
        tracing::debug!("precons import: unresolvable printing, forwarding to Node");
        return forward_import(st, &headers, &body_req).await;
    }

    // cacheCards BEFORE any deck write (importPreconCardsIntoDeck order),
    // deduped by id, mtg- ids only.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let fresh: Vec<Map<String, Value>> = bulk
        .cards
        .iter()
        .filter_map(|c| c.as_object().cloned())
        .filter(|c| {
            c.get("id").map(|v| js_string(v)).unwrap_or_default().starts_with("mtg-")
                && seen.insert(c.get("id").map(js_string).unwrap_or_default())
        })
        .collect();

    // cardIdsByName over pairs IN RESPONSE ORDER: first printing per
    // canonical name is the representative; reprints fold quantity and OR
    // the commander flag; blank keys drop the pair.
    let mut by_name: Vec<(String, String, f64, bool)> = Vec::new(); // (key, cardId, quantity, commander)
    for (row_i, card) in &bulk.pairs {
        let id = js_string(card.get("id").unwrap_or(&Value::Null));
        if !id.starts_with("mtg-") {
            continue;
        }
        let key = js_trim(&js_str_or_empty(card.get("name"))).to_lowercase();
        if key.is_empty() {
            continue;
        }
        let row = &uniq[*row_i];
        let row_quantity = js_num(row.get("quantity"));
        let row_commander = truthy(row.get("commander").unwrap_or(&Value::Bool(false)));
        if let Some(entry) = by_name.iter_mut().find(|(k, _, _, _)| *k == key) {
            entry.2 += row_quantity;
            entry.3 = entry.3 || row_commander;
        } else {
            by_name.push((key, id, row_quantity, row_commander));
        }
    }
    if by_name.is_empty() {
        return import_failed(
            StatusCode::UNPROCESSABLE_ENTITY,
            "None of this preconstructed deck's cards could be resolved to card printings",
            num_out(bulk.not_found as f64),
        );
    }

    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "precons: cannot open write connection");
            return import_failed(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), Value::from(0));
        }
    };
    if !fresh.is_empty() {
        if let Err(e) = cache_normalized_cards(&conn, &fresh) {
            tracing::error!("precons import: cacheCards failed: {e}");
            return import_failed(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), Value::from(0));
        }
    }

    // Route-side shape (runs AFTER the resolver in Node's try block).
    let list_name = list.get("name").cloned().unwrap_or(Value::Null);
    let list_code = list.get("code").cloned().unwrap_or(Value::Null);
    let list_type = list.get("type").cloned().unwrap_or(Value::Null);
    let (shape_format, shape_accent, shape_size) = default_shape(
        list_type.as_str().unwrap_or("\u{0}never-matches"), // COMMANDER_TYPES.has(non-string) === false
    );

    let name_v = body_obj.get("name").cloned().unwrap_or(Value::Null);
    let name = if truthy(&name_v) {
        js_string(&name_v)
    } else if truthy(&list_name) {
        js_string(&list_name)
    } else {
        "Preconstructed Deck".into()
    };
    let description = if truthy(&list_code) {
        format!("Preconstructed: {} ({})", js_string(&list_name), js_string(&list_code))
    } else {
        format!("Preconstructed: {}", js_string(&list_name))
    };
    let format_v = body_obj.get("format").cloned().unwrap_or(Value::Null);
    let format = if truthy(&format_v) { js_string(&format_v) } else { shape_format.to_string() };
    // `category || shape.category` — shape.category is null, but a TRUTHY
    // body category flows through (the route passes it verbatim).
    let category_v = body_obj.get("category").cloned().unwrap_or(Value::Null);
    let category: Option<String> = if truthy(&category_v) {
        Some(js_string(&category_v))
    } else {
        None
    };
    let accent_v = body_obj.get("accentColor").cloned().unwrap_or(Value::Null);
    let accent = if truthy(&accent_v) { js_string(&accent_v) } else { shape_accent.to_string() };
    // Number(targetSize): an absent key is UNDEFINED (NaN → shape default),
    // an explicit null is Number(null) === 0 — a finite override.
    let target_size = match body_obj.get("targetSize") {
        Some(v) => match js_number(v) {
            Some(f) if f.is_finite() => f,
            _ => shape_size as f64,
        },
        None => shape_size as f64,
    };

    // INSERT INTO decks — db.run autocommits, no transaction in Node.
    // JS-number binds mirror node-sqlite3: integer-valued numbers store as
    // INTEGER, only true fractions become REAL (a Rust-authored 100.0 vs
    // Node's 100 would diverge on read-back through GET /api/decks).
    let ts_bind = int_or_f64(target_size);
    let deck_id = conn.execute(
        "INSERT INTO decks (name, description, format, category, accent_color, target_size, user_id, source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![name, description, format, category, accent, &*ts_bind, user.id, "precon"],
    );
    let deck_id = match deck_id {
        Ok(_) => conn.last_insert_rowid(),
        Err(e) => {
            tracing::error!("precons import: deck insert failed: {e}");
            return import_failed(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), Value::from(0));
        }
    };
    for (_, card_id, quantity, commander) in &by_name {
        let qty_bind = int_or_f64(*quantity);
        if let Err(e) = conn.execute(
            "INSERT INTO deck_cards (deck_id, card_id, quantity, is_commander) VALUES (?1,?2,?3,?4)
             ON CONFLICT(deck_id, card_id) DO UPDATE SET
               quantity = quantity + EXCLUDED.quantity,
               is_commander = MAX(is_commander, EXCLUDED.is_commander)",
            rusqlite::params![deck_id, card_id, &*qty_bind, if *commander { 1 } else { 0 }],
        ) {
            tracing::error!("precons import: deck_cards insert failed: {e}");
            return import_failed(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), Value::from(0));
        }
    }

    let mut out = Map::new();
    out.insert("message".into(), Value::String(format!("Precon imported: {} card types in the deck", by_name.len())));
    out.insert("id".into(), Value::from(deck_id));
    out.insert("cards".into(), Value::from(by_name.len()));
    out.insert("notFound".into(), num_out(bulk.not_found as f64));
    json_response(StatusCode::CREATED, &out)
}

fn import_failed(status: StatusCode, message: &str, not_found: Value) -> Response {
    let mut m = Map::new();
    m.insert("error".into(), Value::String(format!("Precon import failed: {message}")));
    // `err.notFound || 0` — non-truthy (0/undefined/NaN) answers 0.
    m.insert("notFound".into(), if truthy(&not_found) { not_found } else { Value::from(0) });
    json_response(status, &m)
}

// small JS numeric helpers ---------------------------------------------------

/// `Number(v)` for JSON values (deck `count`/`number` arrive as JSON
/// scalars; strings use JS String→Number semantics).
pub(crate) fn js_number(v: &Value) -> Option<f64> {
    match v {
        Value::Null => Some(0.0), // Number(null) === 0
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = js_trim(s);
            if t.is_empty() {
                Some(0.0)
            } else {
                t.parse::<f64>().ok()
            }
        }
        Value::Array(a) if a.is_empty() => Some(0.0),
        _ => None, // undefined / {} / [1] → NaN
    }
}

/// `a + b` over row quantities: every value reaching the fold is a JSON
/// number (getPreconCardList guarantees `count`), non-numbers answer NaN and
/// poison the sum exactly like JS (JSON.stringify(NaN) === 'null').
fn js_num(v: Option<&Value>) -> f64 {
    match v {
        Some(v) => js_number(v).unwrap_or(f64::NAN),
        None => f64::NAN,
    }
}

pub(crate) fn num_out(v: f64) -> Value {
    if v.is_finite() && v == v.trunc() && v.abs() < 9.007199254740992e15 {
        Value::from(v as i64)
    } else if v.is_finite() {
        serde_json::Number::from_f64(v).map(Value::Number).unwrap_or(Value::Null)
    } else {
        Value::Null
    }
}

/// node-sqlite3 number binding: JS numbers with no fractional part bind as
/// INTEGER; only true fractions bind REAL.
fn int_or_f64(v: f64) -> Box<dyn rusqlite::ToSql> {
    if v.fract() == 0.0 && v.abs() < 9.007199254740992e15 {
        Box::new(v as i64)
    } else {
        Box::new(v)
    }
}

/// Re-issue the WHOLE untouched POST import to Node: same method, path,
/// headers (minus content-length, which the body re-derives) and JSON body.
/// Node replays auth against the shared DB and completes the import itself.
async fn forward_import(st: AppState, headers: &HeaderMap, body: &Value) -> Response {
    let mut rb = Request::builder().method(Method::POST).uri("/api/precons/import");
    for (k, v) in headers.iter() {
        if k != header::CONTENT_LENGTH {
            rb = rb.header(k, v);
        }
    }
    let req = rb
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Request::new(Body::empty()));
    crate::proxy(State(st), req).await
}

/// PathBuf re-export for AppState wiring without importing PathBuf here.
pub fn default_index_path(db_path: Option<&PathBuf>) -> PathBuf {
    db_path
        .and_then(|p| p.parent())
        .map(|d| d.join("precons.json"))
        .unwrap_or_else(|| PathBuf::from("precons.json"))
}

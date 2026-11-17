//! Lane B unit 6c: cardSets.js twin — set discovery + per-set card caching,
//! the phase-1 engine the catalog build calls (`listAllSets`, `cacheSetCards`).
//!
//! Faithful port of backend/src/cardSets.js:
//!
//!   * `getScryfallSets` is the shared 6 h memo (`st.scryfall_sets`, the same
//!     one `/api/sets` uses through `sets::scryfall_sets`);
//!   * `mtgSetFamilyQuery` folds non-digital children (tokens, promos, art
//!     series) into the parent under the user's scan exclusions, then builds
//!     the Scryfall query string byte-for-byte (`(set:a or set:b)
//!     include:extras unique:prints -is:digital[ lang:<x>]`);
//!   * `fetchMtgSet` pages with `scryGetRetried` (4 attempts, like Node's
//!     default) and a 120 ms sleep between pages, yielding one entry per
//!     scannable face (DFCs yield two, same name/number);
//!   * `cacheSetCards` throws the `absent` FLAG (not a message match) on zero
//!     cards — catalog.js distinguishes expected per-language gaps from real
//!     failures on the flag, so `CacheErr::Absent` carries it.
//!
//! The `cacheFetchedCards` leg dedupes by raw.id (first wins), normalises
//! with `normalizeCard(raw, langCode)` and swallows cache errors like Node's
//! `console.warn`.

use serde_json::{Map, Value};

use crate::card_cache::cache_normalized_cards;
use crate::card_norm::normalize_card;
use crate::catalog_read::{to_code, LangEntry};
use crate::scryfall::{Scry, ScryError, ScryOpts};
use crate::sets::{scryfall_sets, Exclusions, ScryfallSetsMemo};

/// cardSets.js `absent(message)` — an Error with `.absent = true`.
#[derive(Debug)]
pub enum CacheErr {
    /// e.absent — the provider has no cards for this set in this language.
    Absent(String),
    /// A real failure (transport, HTTP, or a cache-side error Node also
    /// swallows — those never surface here).
    Failed(String),
}

impl CacheErr {
    pub fn message(&self) -> &str {
        match self {
            CacheErr::Absent(m) | CacheErr::Failed(m) => m,
        }
    }
}

/// cardSets.js `norm` — `(set || '').toLowerCase().replace(/[^a-z0-9]/g, '')`.
pub fn norm_set(set: &str) -> String {
    set.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn card_count(v: Option<&Value>) -> i64 {
    v.and_then(|v| v.as_f64()).map(|f| f as i64).unwrap_or(0)
}

/// cardSets.js `childAllowed(s, exclusions, excludeChildCodes)`.
pub fn child_allowed(s: &Value, ex: &Exclusions, exclude_child_codes: &[String]) -> bool {
    let set_type = s.get("set_type").and_then(Value::as_str).unwrap_or("");
    if ex.tokens && set_type == "token" {
        return false;
    }
    if ex.art_cards && set_type == "memorabilia" {
        return false;
    }
    if ex.jumpstart && set_type == "starter" {
        return false;
    }
    if ex.promos && set_type == "promo" {
        return false;
    }
    !exclude_child_codes.iter().any(|c| Some(c.as_str()) == s.get("code").and_then(Value::as_str))
}

/// cardSets.js `mtgSetFamilyQuery(set, lang, {excludeChildCodes})` — the
/// query string only; the caller wraps it in the search URL.
///
/// `sets` is the already-memoised Scryfall /sets list; `ex` the scan
/// exclusions (Node re-queries app_settings per call; same answer).
pub fn mtg_set_family_query(
    set: &str,
    lang: &str,
    sets: &[Value],
    ex: &Exclusions,
    exclude_child_codes: &[String],
    langs: &[LangEntry],
) -> String {
    let code = norm_set(set);
    let mut codes: Vec<String> = vec![code.clone()];
    for s in sets {
        let parent = s.get("parent_set_code").and_then(Value::as_str);
        let Some(parent) = parent.filter(|p| !p.is_empty()) else { continue };
        if norm_set(parent) != code {
            continue;
        }
        if truthy(s.get("digital")) {
            continue;
        }
        if !child_allowed(s, ex, exclude_child_codes) {
            continue;
        }
        if let Some(c) = s.get("code").and_then(Value::as_str) {
            if !codes.iter().any(|x| x == c) {
                codes.push(c.to_string());
            }
        }
    }
    let sets_clause = codes
        .iter()
        .map(|c| format!("set:{c}"))
        .collect::<Vec<_>>()
        .join(" or ");
    // languages.resolve(lang).scryfall — `lang` here is already a code from
    // cacheSetCards (langOf), but resolve() accepts code/name/provider alike.
    let scry_lang = crate::catalog_read::resolve_lang(langs, lang)
        .scryfall
        .clone()
        .unwrap_or_else(|| "en".into());
    let lang_term = if scry_lang == "en" { String::new() } else { format!(" lang:{scry_lang}") };
    format!("({sets_clause}) include:extras unique:prints -is:digital{lang_term}")
}

/// `mtgSearchUrl(query, lang, extra)` — the absolute URL BEFORE the scryUrl
/// swap (the Scry client applies its own base rewrite; passing this path form
/// through `scry.get` mirrors `scryGet(scryUrl(abs))`).
fn mtg_search_path(query: &str, lang: &str, extra: &str, langs: &[LangEntry]) -> String {
    let multilingual = if crate::catalog_read::resolve_lang(langs, lang).scryfall.as_deref() == Some("en") {
        String::new()
    } else {
        "&include_multilingual=true".to_string()
    };
    format!(
        "https://api.scryfall.com/cards/search?q={}&{}{}",
        encode_uri_component(query),
        extra.trim_start_matches('&'),
        multilingual
    )
}

/// `encodeURIComponent` (unreserved set A-Za-z0-9-_.!~*'()).
fn encode_uri_component(s: &str) -> String {
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

/// cardSets.js `listAllSets(lang)` — the lang parameter is unread upstream.
pub async fn list_all_sets(scry: &Scry, memo: &ScryfallSetsMemo) -> Vec<String> {
    let sets = scryfall_sets(scry, memo).await;
    sets.into_iter()
        .filter(|s| {
            truthy(s.get("code"))
                && !truthy(s.get("digital"))
                && !truthy(s.get("parent_set_code"))
                && card_count(s.get("card_count")) > 0
        })
        .map(|s| s.get("code").and_then(Value::as_str).unwrap_or_default().to_string())
        .collect()
}

/// cardSets.js `mtgCardImages(c)` — scannable faces of one raw card.
fn mtg_card_images(c: &Value) -> Vec<String> {
    if let Some(u) = c.get("image_uris").and_then(|i| i.get("normal")).and_then(Value::as_str) {
        return vec![u.to_string()];
    }
    c.get("card_faces")
        .and_then(Value::as_array)
        .map(|faces| {
            faces
                .iter()
                .filter_map(|f| {
                    f.get("image_uris").and_then(|i| i.get("normal")).and_then(Value::as_str).map(String::from)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One entry of `fetchMtgSet`'s result — only `raw` and `set` are read by
/// cacheSetCards/cacheFetchedCards today; the shape mirrors Node.
pub struct FetchedCard {
    pub name: String,
    pub set: String,
    pub number: String,
    pub img: String,
    pub raw: Value,
}

/// cardSets.js `fetchMtgSet(set, lang, {excludeChildCodes})`. The catalog
/// job passes the exclusions it read from its own write connection.
pub async fn fetch_mtg_set_with_ex(
    scry: &Scry,
    memo: &ScryfallSetsMemo,
    set: &str,
    lang: &str,
    exclude_child_codes: &[String],
    langs: &[LangEntry],
    ex: Exclusions,
) -> Result<Vec<FetchedCard>, String> {
    let sets_list = scryfall_sets(scry, memo).await;
    let q = mtg_set_family_query(set, lang, &sets_list, &ex, exclude_child_codes, langs);
    let mut url = mtg_search_path(&q, lang, "&order=set", langs);
    let mut cards: Vec<FetchedCard> = Vec::new();
    let opts = ScryOpts::default();
    loop {
        let r = scry
            .get_retried(&url, &opts, 4)
            .await
            .map_err(|e: ScryError| e.to_string())?
            .ok_or_else(|| "non-json scryfall response".to_string())?;
        if let Some(data) = r.get("data").and_then(Value::as_array) {
            for c in data {
                for img in mtg_card_images(c) {
                    cards.push(FetchedCard {
                        name: c
                            .get("printed_name")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .or_else(|| c.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()))
                            .unwrap_or("")
                            .to_string(),
                        set: c
                            .get("set")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(set)
                            .to_string(),
                        number: c.get("collector_number").and_then(Value::as_str).unwrap_or("").to_string(),
                        img,
                        raw: c.clone(),
                    });
                }
            }
        }
        let has_more = r.get("has_more").and_then(Value::as_bool).unwrap_or(false);
        let next = r.get("next_page").and_then(Value::as_str).unwrap_or("").to_string();
        if !has_more || next.is_empty() {
            break;
        }
        url = next;
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
    Ok(cards)
}

/// cardSets.js `cacheSetCards(set, lang)` (excludeChildCodes is always the
/// default [] from catalog.js) split in two so the job never holds a
/// rusqlite Connection across an await: fetch (async, absent-flag on empty)
/// then the sync cache write. Returns the fetched cards so the caller can
/// count them.
pub async fn fetch_set_cards(
    scry: &Scry,
    memo: &ScryfallSetsMemo,
    set: &str,
    lang_display: &str,
    langs: &[LangEntry],
    ex: Exclusions,
) -> Result<(Vec<FetchedCard>, String), CacheErr> {
    let code = to_code(langs, lang_display);
    let cards = fetch_mtg_set_with_ex(scry, memo, set, &code, &[], langs, ex)
        .await
        .map_err(CacheErr::Failed)?;
    if cards.is_empty() {
        return Err(CacheErr::Absent(format!("no cards for set {set}")));
    }
    Ok((cards, code))
}

/// The synchronous cache leg of cacheSetCards (see `cache_fetched_cards`).
pub fn cache_set_rows(conn: &rusqlite::Connection, cards: &[FetchedCard], code: &str) {
    cache_fetched_cards(conn, cards, code);
}

/// cardSets.js `cacheFetchedCards(cards, code)` — dedupe by raw.id (first
/// wins), normalise, INSERT OR REPLACE; cache-side errors are swallowed
/// exactly like Node's try/catch console.warn.
fn cache_fetched_cards(conn: &rusqlite::Connection, cards: &[FetchedCard], code: &str) {
    let mut seen: Vec<String> = Vec::new();
    let mut rows: Vec<Map<String, Value>> = Vec::new();
    for c in cards {
        let Some(id) = c.raw.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            continue;
        };
        if seen.iter().any(|s| s == id) {
            continue;
        }
        seen.push(id.to_string());
        if let Some(m) = normalize_card(&c.raw, Some(code)).as_object().cloned() {
            rows.push(m);
        }
    }
    if let Err(e) = cache_normalized_cards(conn, &rows) {
        tracing::warn!("cardSets: caching cards failed: {e}");
    }
}

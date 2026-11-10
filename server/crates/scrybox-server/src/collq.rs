//! Port of the collection-scope `q` leg of `GET /api/search`
//! (`backend/src/routes/collection.js` searchCards collection branch →
//! `scryfallApi.js` `resolveCollectionQuery` → `fetchCollectionNames`/
//! `runWalk` → `cardSearchSql.ownedByNames` → `priceHelpers.parseCardRow`
//! + `resolveCardPrice`).
//!
//! Scope of the port (iter 9): the NAME-ONLY free-text branch. A query whose
//! every token is a bare word is what Node's parser turns into an AND of
//! lowercased name leaves, and in sidecar mode that AST is re-serialised by
//! `toScryfall` back to the tokens joined by single spaces — a normalization
//! small enough to reproduce byte-exactly. Anything with operator syntax,
//! quoting, grouping or parser-visible structure (`t:`, `!`, `|`, `or`,
//! keywords, comparators…) is LEFT TO NODE: plan() returns Proxy and Node
//! answers verbatim, so the parity contract never depends on Rust modelling
//! the whole Scryfall grammar.
//!
//! Node answers this leg by walking the scryfall-local engine for the MATCH
//! LIST (canonical lowercased game-card names, DFC faces included), caching it
//! in `collection_query_cache` (the durable half of its in-process
//! `collectionQueryCache`), then intersecting that list against the LIVE
//! collection with `ownedByNames`. We mirror all three halves on the SAME
//! durable table (shared key → shared rows → shared headers), so a warm Node
//! entry is warm here and vice versa.
//!
//! Deliberate divergences (design notes):
//!  - No card_cache backfill of walked cards (same call as iter-7's walk):
//!    owned rows already join cached cards; the backfill never affects a
//!    response, only warming.
//!  - No background revalidation of stale (>6 h) entries. The durable
//!    `resolved_at` drives X-Catalog-Cache identically on both servers, and
//!    parity never depends on Rust refreshing the row — Node's own requests
//!    revalidate it on their side when they age.
//!  - Transport-level walk failures proxy (Node's local-fallback/503 legs are
//!    Node's to answer); deterministic engine verdicts answer natively:
//!    404 → the durable empty answer, 400/422 → 400 INVALID_QUERY (uncached,
//!    exactly like Node).

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::OnceCell;

use rusqlite::{types::Value as Sql, Connection};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::{
    collection::{parse_array_col, resolve_card_price, sqlite_value, truthy},
    db::DbPool,
    name_index::Txn,
    sidecar::{self, NamesWalkErr},
    AppState,
};

/// scryfallApi.js CQC_SWR_MAX_AGE_MS / CQC_STALE_MS / CQC_DEFAULT_WALK_LIMIT.
const FRESH_MS: i64 = 6 * 60 * 60 * 1000;
const STALE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const WALK_LIMIT: i64 = 20000;

// ---------------------------------------------------------------------------
// plan: the name-only gate + request normalization
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CollectionQ {
    pub user_id: i64,
    /// `toScryfall(stripDirectives(ast))` for the accepted subset: the
    /// whitespace-split, lowercased, single-space-joined tokens.
    pub walk_base: String,
    /// `languages.toCode(lang)` — the durable-cache `lang` column.
    pub lang_code: String,
    /// `languages.resolve(lang).scryfall` — the ` lang:xx` keyword scope
    /// (langSearch) and the parenthesisation rule (resolveCollectionQuery).
    pub scry_code: String,
    /// `query.page != null || query.limit != null` — explicit pagination
    /// opts into the bounded row window; otherwise the complete owned list.
    pub has_window: bool,
    pub page: i64,
    pub limit: i64,
    /// `query.count !== '0'`.
    pub include_total: bool,
}

impl CollectionQ {
    /// SearchCache key. Distinct prefix ('q|') from the field-filter leg's
    /// keys so the two legs can never collide on one entry.
    pub fn cache_key(&self) -> String {
        format!(
            "q|{}|{}|{}|{}|{}|{}|{}",
            self.user_id, self.walk_base, self.lang_code, self.page, self.limit, self.has_window, self.include_total
        )
    }
    /// `fetchCollectionNames` key: `${q}\u{0}${langCode}` where q is the
    /// CLEAN scoped query (parenthesised for non-English, NO unique: suffix
    /// — the suffix is added only at the engine call).
    pub fn names_key(&self) -> String {
        format!("{}\u{0}{}", self.cqc_q(), self.lang_code)
    }
    fn cqc_q(&self) -> String {
        if self.scry_code == "en" {
            self.walk_base.clone()
        } else {
            format!("({})", self.walk_base)
        }
    }
    /// The engine query: `runWalk` fetches `${q} unique:cards` (byPrinting is
    /// always false on this branch — printing operators fail the gate).
    pub fn walk_q(&self) -> String {
        format!("{} unique:cards", self.cqc_q())
    }
    /// (limit, offset) when the caller asked for a window; None = unbounded.
    pub fn row_window(&self) -> Option<(i64, i64)> {
        self.has_window.then(|| (self.limit, (self.page - 1).saturating_mul(self.limit)))
    }
}

/// JS `\s` — the tokenizer's whitespace class (includes U+FEFF, which Rust's
/// char::is_whitespace does NOT; U+0000 is not JS whitespace but IS ASCII
/// control — tokenizer splits on /\s/ only).
fn is_js_ws(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// The tokenizer's operator regex `^([a-z_]+)(!=|<=|>=|:|=|<|>)` (case-
/// insensitive), mirrored as `(keyword_len, comparator_len)`: the ASCII
/// `[a-z_]` run followed by the FIRST comparator the alternation accepts
/// (the two-char `!=`/`<=`/`>=` are tried before the one-char `:`/`=`/`<`/
/// `>`). `(0, 0)` when the token takes the bare-name path instead.
fn op_shape(tok: &str) -> (usize, usize) {
    let b = tok.as_bytes();
    let mut i = 0;
    while i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'_') {
        i += 1;
    }
    if i == 0 {
        return (0, 0); // regex needs at least one [a-z_] before the comparator
    }
    let rest = &tok[i..];
    let cmp = if rest.starts_with("!=") || rest.starts_with("<=") || rest.starts_with(">=") {
        2
    } else if rest.starts_with([':', '=', '<', '>']) {
        1
    } else {
        0
    };
    (i, cmp)
}

/// `cardSearchSql.parseColorValue`'s accept verdict: digit count, the m/c
/// family, (mono-)color words, or a `[wubrg]` letter run. Node THROWS on
/// anything else (→ parsed=null → the sidecar gets the query VERBATIM,
/// un-normalized), so the gate must reject too rather than walk a
/// re-serialized string Node never sends.
fn color_value_ok(value: &str) -> bool {
    let v = value.trim_matches(is_js_ws).to_lowercase();
    if v.is_empty() {
        return false;
    }
    if v.bytes().all(|b| b.is_ascii_digit()) {
        return true; // /^[0-9]+$/
    }
    if matches!(v.as_str(), "m" | "multicolor" | "multicolored" | "c" | "colorless") {
        return true;
    }
    const WORDS: [&str; 5] = ["white", "blue", "black", "red", "green"];
    let lw = v.replace('-', " ");
    let parts: Vec<&str> = lw.trim_matches(is_js_ws).split(is_js_ws).filter(|s| !s.is_empty()).collect();
    // JS keeps [''] for an empty trim result — mirror by requiring non-empty.
    if !parts.is_empty() && parts.iter().all(|w| WORDS.contains(w)) {
        return true;
    }
    v.chars().all(|c| matches!(c, 'w' | 'u' | 'b' | 'r' | 'g')) // /^[wubrg]+$/
}

/// The accepted operator keywords, by lowercased spelling. `is:` is excluded
/// (makeLeaf rewrites it to `t:` with hyphen→space quoting), `oracle:`/
/// `name:`/… stay proxied — only the two data operators the bench and UI
/// conjunctions need are proven here.
fn allowed_operator_kw(kw_lower: &str) -> bool {
    matches!(kw_lower, "t" | "type" | "c" | "color" | "colour")
}

/// The free-text + operator-conjunction gate + `toScryfall` normalization.
/// Returns the canonical walk base when Node's parser would produce an AND
/// of bare name leaves and/or simple `t:`/`c:` operator leaves and the
/// serializer would emit exactly the mirror's join. Acceptance rules
/// (everything else → caller proxies, Node answers verbatim):
///
///  * bare name tokens — lowercased (makeLeaf lowercases name values;
///    fmtValue only quotes on `[(\s"]`, excluded here by construction);
///  * `t:`/`type:`/`c:`/`color:`/`colour:` leaves with the `:` comparator
///    and a bare, non-empty, unquoted, non-regex value — re-serialized
///    from the LOWERCASED keyword with the value verbatim (leafToScryfall
///    uses `raw`+`cmp`+`fmtValue(value,false)`; color values pass through
///    the `parseColorValue` accept-verdict first);
///  * `and` keyword tokens strictly BETWEEN two terms — parseAnd consumes
///    them; at a boundary (or doubled) it THROWS, which under sidecar mode
///    hands the sidecar the raw query verbatim, a different walk key.
///
/// Refused outright: quotes, negation, exact `!`, bare regex `/`, grouping,
/// `|`, `or`/`not`, any other keyword/comparator, empty values, and the
/// bare word `colorless` — makeLeaf rewrites THAT to a `c:c` color leaf, so
/// echoing it would walk a query Node never sends (iter-9 parity hole,
/// fixed here).
pub fn walk_base(raw: &str) -> Option<String> {
    let toks: Vec<&str> = raw.split(is_js_ws).filter(|t| !t.is_empty()).collect();
    let mut out: Vec<String> = Vec::new();
    for (idx, tok) in toks.iter().enumerate() {
        let first = tok.chars().next().unwrap();
        // '"' starts a quoted token; '!' an exact name; '-' negation;
        // '/' the bare-regex form. None of them serialize as-is.
        if matches!(first, '"' | '!' | '-' | '/' | '(' | ')') {
            return None;
        }
        // '(' ')' are tokenizer structure; '|' and '"' inside a token would
        // change tokenization/serialization; refuse rather than model them.
        if tok.contains(['"', '(', ')', '|']) {
            return None;
        }
        let lower = tok.to_lowercase();
        if lower == "or" || lower == "not" {
            return None;
        }
        if lower == "and" {
            // parseAnd consumes the keyword only between two terms. A
            // neighbouring 'and' (double keyword) or a boundary position
            // throws → Node walks the RAW string instead. Proxy.
            let term_ok = |t: &str| {
                let l = t.to_lowercase();
                l != "and" && l != "or" && l != "not"
            };
            let prev_ok = idx > 0 && term_ok(toks[idx - 1]);
            let next_ok = toks.get(idx + 1).is_some_and(|n| term_ok(n));
            if !prev_ok || !next_ok {
                return None;
            }
            continue;
        }
        let (kw_len, cmp_len) = op_shape(tok);
        if cmp_len == 0 {
            // Bare name leaf. makeLeaf's one word-special case: a bare
            // `colorless` becomes the color leaf `c:c`, NOT a name.
            if lower == "colorless" {
                return None;
            }
            out.push(lower);
            continue;
        }
        // Operator leaf: only the `:` comparator of an allowed keyword on a
        // bare value survives the byte-exactness proof.
        if cmp_len != 1 || tok.as_bytes()[kw_len] != b':' {
            return None;
        }
        let kw_lower = tok[..kw_len].to_ascii_lowercase();
        let value = &tok[kw_len + 1..];
        if value.is_empty() || value.starts_with('"') || value.starts_with('/') {
            return None; // Node throws (needs a value) or enters quoted/regex mode
        }
        if !allowed_operator_kw(&kw_lower) {
            return None;
        }
        if matches!(kw_lower.as_str(), "c" | "color" | "colour") && !color_value_ok(value) {
            return None;
        }
        // leafToScryfall: `${raw}${cmp}${fmtValue(value, false)}` — raw is
        // the lowercased keyword, cmp the verbatim ':', value untouched
        // (fmtValue only quotes on [(\s"], all excluded above).
        out.push(format!("{kw_lower}:{value}"));
    }
    if out.is_empty() {
        return None;
    }
    Some(out.join(" "))
}

// ---------------------------------------------------------------------------
// names cache (in-process half of collectionQueryCache) + single-flight walks
// ---------------------------------------------------------------------------

/// One `cqcEntry` without the `fetched` leg (we never backfill from walks).
pub struct QEntry {
    pub names: Vec<String>,
    pub complete: bool,
    /// upstream total_cards, kept as JSON so the durable round-trip and the
    /// X-Catalog-Upstream-Total String() stay byte-faithful.
    pub total: Option<Value>,
    pub fetched_count: u64,
    /// epoch ms of the last successful walk.
    pub resolved_at: i64,
}

pub enum NamesOutcome {
    /// Match list available; bool = "this request performed the walk"
    /// (Node's `resolved` status belongs to exactly those requests).
    Ready(Arc<QEntry>, bool),
    /// Node owns this leg (no sidecar configured / kill switch off /
    /// transport failure): every caller proxies.
    Proxy,
    /// The engine answered 400/422 for this exact query: Node's contract is
    /// a 400 {"error":"INVALID_QUERY"}, uncached.
    InvalidQuery,
}

type WalkSlot = OnceCell<Result<(Arc<QEntry>, bool), NamesWalkErr>>;

#[derive(Default)]
struct CollQInner {
    entries: HashMap<String, Arc<QEntry>>,
    inflight: HashMap<String, Arc<WalkSlot>>,
}

/// Process-wide walk cache mirroring Node's `collectionQueryCache` map +
/// `inflightCollectionNames`. Kept separate from SearchCache because its
/// lifetime is rev-INDEPENDENT (the match list does not depend on the
/// collection; only the intersect does).
#[derive(Default)]
pub struct CollQCache {
    inner: Mutex<CollQInner>,
    walks: AtomicU64,
}

impl CollQCache {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn walks(&self) -> u64 {
        self.walks.load(Ordering::Relaxed)
    }
    fn mem_get(&self, key: &str) -> Option<Arc<QEntry>> {
        self.inner.lock().unwrap().entries.get(key).cloned()
    }
    fn mem_put(&self, key: String, e: Arc<QEntry>) {
        let mut g = self.inner.lock().unwrap();
        if g.entries.len() > 256 {
            g.entries.clear();
        }
        g.entries.insert(key, e);
    }
    fn slot(&self, key: &str) -> Arc<WalkSlot> {
        self.inner.lock().unwrap().inflight.entry(key.to_string()).or_default().clone()
    }
    fn finish(&self, key: &str) {
        self.inner.lock().unwrap().inflight.remove(key);
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// fetchCollectionNames' fresh/stale bound (a synchronously-resolved walk
/// reports 'resolved' at its call site, not here).
pub(crate) fn status_of(e: &QEntry) -> &'static str {
    if !e.complete {
        "stale"
    } else if e.resolved_at == 0 {
        "resolved"
    } else if now_ms() - e.resolved_at <= FRESH_MS {
        "fresh"
    } else {
        "stale"
    }
}

/// Durable read of one `collection_query_cache` row. A parse/read failure is
/// a miss (Node's catch falls through to the walk on read errors).
fn durable_read(pool: &DbPool, cq: &CollectionQ) -> Option<Arc<QEntry>> {
    let conn = pool.get().ok()?;
    let mut stmt = conn
        .prepare_cached(
            "SELECT names, upstream_total, fetched_count, complete, resolved_at FROM collection_query_cache WHERE query = ? AND lang = ?",
        )
        .ok()?;
    let mut rows = stmt.query(rusqlite::params![cq.cqc_q(), cq.lang_code]).ok()?;
    let row = rows.next().ok().flatten()?;
    let names_raw = row.get::<_, String>(0).ok()?;
    let names: Vec<String> = serde_json::from_str(&names_raw).ok()?;
    let total = sqlite_value(&row, 1).ok().filter(|v| !v.is_null());
    let fetched_count = sqlite_value(&row, 2).ok().and_then(|v| v.as_u64()).unwrap_or(0);
    let complete = sqlite_value(&row, 3).ok().map(|v| truthy(&v)).unwrap_or(false);
    let resolved_at = sqlite_value(&row, 4).ok().and_then(|v| v.as_i64()).unwrap_or(0);
    Some(Arc::new(QEntry { names, complete, total, fetched_count, resolved_at }))
}

/// Best-effort persist of a walked entry, matching Node's upsert column-for-
/// column. Cache durability must never break a search, so errors are logged.
fn durable_write(pool: &DbPool, cq: &CollectionQ, e: &QEntry) {
    let total_sql = match &e.total {
        None => Sql::Null,
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) => Sql::Integer(i),
            None => Sql::Real(n.as_f64().unwrap_or(0.0)),
        },
        Some(other) => Sql::Real(other.as_f64().unwrap_or(0.0)),
    };
    let names_json = serde_json::to_string(&e.names).unwrap_or_else(|_| "[]".into());
    let res = pool.get().and_then(|conn| {
        conn.prepare_cached(
            "INSERT INTO collection_query_cache (query, lang, names, upstream_total, fetched_count, complete, resolved_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(query, lang) DO UPDATE SET
               names = excluded.names,
               upstream_total = excluded.upstream_total,
               fetched_count = excluded.fetched_count,
               complete = excluded.complete,
               resolved_at = excluded.resolved_at,
               expires_at = excluded.expires_at",
        )?.execute(rusqlite::params![
                cq.cqc_q(),
                cq.lang_code,
                names_json,
                total_sql,
                e.fetched_count as i64,
                if e.complete { 1i64 } else { 0i64 },
                e.resolved_at,
                now_ms() + STALE_MS,
            ],
        )
    });
    if let Err(err) = res {
        tracing::warn!(error = %err, "collection_query_cache write failed");
    }
}

/// `fetchCollectionNames` (minus background revalidation — see module note):
/// in-memory entry, else the durable row when still usable, else a shared
/// synchronous walk. Statuses ride the same age rule Node uses.
pub async fn get_names(st: AppState, cq: &Arc<CollectionQ>) -> NamesOutcome {
    let key = cq.names_key();
    if let Some(e) = st.collq_cache.mem_get(&key) {
        return NamesOutcome::Ready(e, false);
    }
    let Some(pool) = st.db.as_ref() else {
        return NamesOutcome::Proxy;
    };
    let pool_h = Arc::clone(pool);
    let cq_d = Arc::clone(cq);
    if let Some(e) = tokio::task::spawn_blocking(move || durable_read(&pool_h, &cq_d)).await.unwrap_or(None) {
        let usable = e.complete || now_ms() - e.resolved_at <= STALE_MS;
        if usable {
            st.collq_cache.mem_put(key, e.clone());
            return NamesOutcome::Ready(e, false);
        }
    }
    // Synchronous walk. No sidecar to ask → Node owns it (its walk has the
    // local-fallback/503 semantics we deliberately do not reproduce).
    let Some(base) = st
        .cfg
        .sidecar_url
        .as_deref()
        .filter(|_| st.owns("search_sidecar"))
    else {
        return NamesOutcome::Proxy;
    };
    let slot = st.collq_cache.slot(&key);
    let init_st = st.clone();
    let init_cq = Arc::clone(cq);
    let init_key = key.clone();
    let init_base = base.to_string();
    let res: Result<&Result<(Arc<QEntry>, bool), NamesWalkErr>, ()> = slot
        .get_or_try_init(move || async move {
            // Late joiners: whoever set the in-memory/durable entry while this
            // slot was being claimed answers without a second walk.
            if let Some(e) = init_st.collq_cache.mem_get(&init_key) {
                return Ok(Ok((e, false)));
            }
            init_st.collq_cache.walks.fetch_add(1, Ordering::Relaxed);
            let walk = match sidecar::walk_collection_names(&init_st, &init_base, &init_cq.walk_q(), &init_cq.lang_code, WALK_LIMIT).await {
                Ok(w) => w,
                Err(e) => return Ok(Err(e)),
            };
            let entry = Arc::new(QEntry {
                names: walk.names,
                complete: walk.complete,
                total: walk.total,
                fetched_count: walk.fetched_count,
                resolved_at: now_ms(),
            });
            init_st.collq_cache.mem_put(init_key.clone(), entry.clone());
            let pool_w = Arc::clone(init_st.db.as_ref().expect("db present above"));
            let cq_w = Arc::clone(&init_cq);
            let e_w = Arc::clone(&entry);
            let _ = tokio::task::spawn_blocking(move || durable_write(&pool_w, &cq_w, &e_w)).await;
            Ok(Ok((entry, true)))
        })
        .await;
    st.collq_cache.finish(&key);
    match res {
        Ok(Ok((e, resolved_now))) => NamesOutcome::Ready(e.clone(), *resolved_now),
        Ok(Err(NamesWalkErr::Invalid)) => NamesOutcome::InvalidQuery,
        Ok(Err(NamesWalkErr::Transport)) | Err(_) => NamesOutcome::Proxy,
    }
}

// ---------------------------------------------------------------------------
// ownedByNames intersect + response build
// ---------------------------------------------------------------------------

/// ownedByNames projection column order — the q-leg's row shape (differs
/// from /api/collection's SELECT by carrying oracle_id and price_etched).
const Q_COLS: &[&str] = &[
    "entry_id", "card_id", "quantity", "condition", "printing", "language", "purchase_price",
    "added_at", "is_trade", "notes", "is_proxy", "oracle_id", "name", "printed_name", "supertype",
    "subtypes", "types", "cmc", "color_identity", "rarity", "set_id", "set_name", "number",
    "image_url", "price_trend", "price_normal", "price_holofoil", "price_etched", "price_currency",
    "price_source", "price_estimated", "tcgplayer_url", "cardmarket_url", "tcgplayer_product_id",
];

const OWNED_BASE: &str = "
    FROM collection c
    JOIN card_cache cc ON c.card_id = cc.id
    WHERE c.user_id = ? AND c.quantity > 0
      AND LOWER(TRIM(COALESCE(cc.name, ''))) IN (SELECT value FROM json_each(?))";

/// The fully-built native response for the q leg.
pub struct Built {
    pub raw: Vec<u8>,
    /// X-Total-Count when include_total (String(JS number)); None omits it.
    pub total_header: Option<String>,
    pub complete: bool,
    pub upstream_total: Option<String>,
    pub snapshot: String,
}

pub enum BuildOutcome {
    Built(Built),
    /// BEGIN/connection trouble: proxy (same contract as the field-filter leg).
    Proxy,
    /// Genuine build fault: Node would 500 'Search failed'.
    Failed(String),
}

/// `catalogMembershipVersion`: sha256 over the sorted names joined by NUL,
/// first 20 hex chars. JS sort is UTF-16 code-unit order; Rust's byte order
/// agrees for all valid (non-lone-surrogate) text.
fn membership_version(names: &[String]) -> String {
    let mut sorted: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    sorted.sort_unstable();
    let joined = sorted.join("\u{0}");
    let digest = Sha256::digest(joined.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..20].to_string()
}

/// JS `String(value)` for the upstream-total header (integer-valued numbers
/// print without a decimal point, as JSON.stringify/Number#toString do).
fn js_string(v: &Value) -> String {
    match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `resolveCollectionQuery`'s post-walk half + the route's header values:
/// names → ownedByNames rows (parseCardRow + resolveCardPrice) → JSON.
pub fn build(pool: &DbPool, cq: &CollectionQ, entry: &Arc<QEntry>) -> BuildOutcome {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return BuildOutcome::Failed(format!("pool: {e}")),
    };
    let txn = match Txn::begin(&conn) {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(error = %e, "collq build: BEGIN failed → proxy");
            return BuildOutcome::Proxy;
        }
    };
    let mv = membership_version(&entry.names);
    let snapshot = match catalog_snapshot(&conn) {
        Ok(s) => s,
        Err(e) => return BuildOutcome::Failed(e),
    };
    let snapshot = format!("remote:{mv}:{snapshot}");

    // Dedup/clean like ownedByNames' `clean` (names arrive lowercased and
    // trimmed from the walk; the Set is defensive against hand-edited rows).
    let mut seen: HashSet<String> = HashSet::new();
    let clean: Vec<String> = entry
        .names
        .iter()
        .map(|n| n.trim().to_lowercase())
        .filter(|n| !n.is_empty())
        .filter(|n| seen.insert(n.clone()))
        .collect();

    let (cards, total_header) = if clean.is_empty() {
        (Vec::new(), cq.include_total.then(|| "0".to_string()))
    } else {
        let membership = match serde_json::to_string(&clean) {
            Ok(m) => m,
            Err(e) => return BuildOutcome::Failed(format!("membership: {e}")),
        };
        match owned_rows(&conn, cq, &membership) {
            Ok(v) => (v, None),
            Err(e) => return BuildOutcome::Failed(e),
        }
    };
    // count runs on the SAME pinned view, only when the caller wants the
    // header; empty-name answers already pinned total to 0 above.
    let total_header = if clean.is_empty() {
        total_header
    } else if cq.include_total {
        let membership = serde_json::to_string(&clean).unwrap_or_else(|_| "[]".into());
        match owned_count(&conn, cq.user_id, &membership) {
            Ok(n) => Some(n.to_string()),
            Err(e) => return BuildOutcome::Failed(e),
        }
    } else {
        None
    };

    if let Err(e) = txn.commit() {
        return BuildOutcome::Failed(format!("commit: {e}"));
    }
    let body = match serde_json::to_vec(&cards) {
        Ok(b) => b,
        Err(e) => return BuildOutcome::Failed(format!("serialize: {e}")),
    };
    BuildOutcome::Built(Built {
        raw: body,
        total_header,
        complete: entry.complete,
        upstream_total: entry.total.as_ref().map(js_string),
        snapshot,
    })
}

/// `readCatalogSnapshot(readDb, 'remote', membershipVersion)` — token parts
/// only; the caller composes `remote:{mv}:{revision}`.
fn catalog_snapshot(conn: &Connection) -> Result<i64, String> {
    let mut stmt = conn
        .prepare_cached("SELECT revision FROM collection_snapshot WHERE id = 1")
        .map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let rev = match rows.next().map_err(|e| e.to_string())? {
        Some(row) => sqlite_value(row, 0)
            .ok()
            .and_then(|v| v.as_f64())
            .filter(|f| *f != 0.0 && !f.is_nan())
            .map(|f| f as i64)
            .unwrap_or(0), // `Number(row?.revision) || 0`
        None => 0,
    };
    Ok(rev)
}

fn owned_rows(conn: &Connection, cq: &CollectionQ, membership: &str) -> Result<Vec<Value>, String> {
    let window = cq.row_window();
    let mut sql = String::from(
        "SELECT
      c.id AS entry_id,
      c.card_id,
      c.quantity,
      c.condition,
      c.printing,
      c.language,
      c.purchase_price,
      c.added_at,
      c.is_trade,
      c.notes,
      c.is_proxy,
      cc.oracle_id,
      cc.name,
      cc.printed_name,
      cc.supertype,
      cc.subtypes,
      cc.types,
      cc.cmc,
      cc.color_identity,
      cc.rarity,
      cc.set_id,
      cc.set_name,
      cc.number,
      cc.image_url,
      cc.price_trend,
      cc.price_normal,
      cc.price_holofoil,
      cc.price_etched,
      cc.price_currency,
      cc.price_source,
      cc.price_estimated,
      cc.tcgplayer_url,
      cc.cardmarket_url,
      cc.tcgplayer_product_id
    ",
    );
    sql.push_str(OWNED_BASE);
    let mut params: Vec<Sql> = vec![Sql::Integer(cq.user_id), Sql::Text(membership.to_string())];
    if let Some((limit, offset)) = window {
        sql.push_str("\n    ORDER BY c.added_at DESC, c.id DESC LIMIT ? OFFSET ?");
        params.push(Sql::Integer(limit));
        params.push(Sql::Integer(offset));
    } else {
        sql.push_str("\n    ORDER BY c.added_at DESC, c.id DESC");
    }
    let mut stmt = conn.prepare_cached(&sql).map_err(|e| format!("prepare rows: {e}"))?;
    let mut rows = stmt
        .query(rusqlite::params_from_iter(params.iter()))
        .map_err(|e| format!("rows query: {e}"))?;
    let mut out: Vec<Value> = Vec::new();
    while let Some(row) = rows.next().map_err(|e| format!("row scan: {e}"))? {
        let mut raw = Vec::with_capacity(Q_COLS.len());
        for i in 0..Q_COLS.len() {
            raw.push(sqlite_value(row, i).map_err(|e| e.to_string())?);
        }
        out.push(Value::Object(shape_q_row(raw)?));
    }
    Ok(out)
}

fn owned_count(conn: &Connection, user_id: i64, membership: &str) -> Result<i64, String> {
    let sql = format!("SELECT COUNT(*) AS n {OWNED_BASE}");
    let mut stmt = conn.prepare_cached(&sql).map_err(|e| format!("prepare count: {e}"))?;
    let mut rows = stmt
        .query(rusqlite::params![user_id, membership])
        .map_err(|e| format!("count query: {e}"))?;
    let row = rows.next().map_err(|e| e.to_string())?;
    let n = match row {
        Some(r) => sqlite_value(r, 0).ok().and_then(|v| v.as_i64()).unwrap_or(0),
        None => 0,
    };
    // `Number(count && count.n) || 0`
    Ok(n)
}

/// `{...parseCardRow(row), price_trend: resolveCardPrice(row)}` on the
/// ownedByNames projection — the same applyProxyRarity/array-column contract
/// collection.rs's shape_row proved against Node, on this leg's column set.
fn shape_q_row(raw: Vec<Value>) -> Result<Map<String, Value>, String> {
    let idx = |name: &str| Q_COLS.iter().position(|c| *c == name).unwrap();
    let is_proxy = &raw[idx("is_proxy")];
    let printing = &raw[idx("printing")];
    let rarity_raw = &raw[idx("rarity")];
    let mut obj = Map::with_capacity(Q_COLS.len() + 1);
    for (i, name) in Q_COLS.iter().enumerate() {
        let v = match *name {
            "is_proxy" => Value::from(if truthy(is_proxy) { 1 } else { 0 }),
            "rarity" => {
                if truthy(is_proxy) {
                    Value::String("Proxy".into())
                } else {
                    rarity_raw.clone()
                }
            }
            "subtypes" | "types" | "color_identity" => parse_array_col(&raw[i])?,
            "price_trend" => resolve_card_price(
                is_proxy,
                printing,
                &raw[idx("price_trend")],
                &raw[idx("price_normal")],
                &raw[idx("price_holofoil")],
            ),
            _ => raw[i].clone(),
        };
        obj.insert((*name).to_string(), v);
    }
    // applyProxyRarity appends base_rarity after the row keys.
    obj.insert("base_rarity".to_string(), rarity_raw.clone());
    Ok(obj)
}

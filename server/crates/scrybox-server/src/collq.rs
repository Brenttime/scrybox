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
use crate::poison::LockExt;

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
    /// Route line 140: `typeof query.snapshot === 'string' ? query.snapshot
    /// : null` — a duplicated key is an array, so no check happens anywhere
    /// in Node (plan_collection_q passes None). The walk leg verifies it
    /// against the fresh `remote:{membershipVersion}:{revision}` token per
    /// request (`check_remote_snapshot`).
    pub expected_snapshot: Option<String>,
    /// `analyze(q).printing` — printing-level terms walk `unique:prints`
    /// and intersect by exact card id (ownedByCardIds). Standalone only.
    pub by_printing: bool,
    /// Standalone sidecar mode: the parsed AST when `analyze().mode` is
    /// 'local' — answerLocally's input for the engine-outage fallback.
    pub fallback_ast: Option<crate::scryq::Node>,
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
        let base = if self.scry_code == "en" {
            self.walk_base.clone()
        } else {
            format!("({})", self.walk_base)
        };
        if self.by_printing { format!("{base} unique:prints") } else { base }
    }
    /// The engine query: `runWalk` fetches `${q} unique:cards` (byPrinting is
    /// always false on this branch — printing operators fail the gate).
    pub fn walk_q(&self) -> String {
        if self.by_printing {
            return self.cqc_q();
        }
        format!("{} unique:cards", self.cqc_q())
    }
    /// (limit, offset) when the caller asked for a window; None = unbounded.
    pub fn row_window(&self) -> Option<(i64, i64)> {
        self.has_window.then(|| (self.limit, (self.page - 1).saturating_mul(self.limit)))
    }
}

/// The hybrid-mode subsets refuse any query containing these characters
/// outright (quoted phrases, grouping — invisible in the AST once parsed —
/// and `|`): a conservative byte-exactness guard, not a tokenizer.
fn subset_chars_ok(raw: &str) -> bool {
    !raw.contains(['"', '(', ')', '|'])
}

/// The flat AND of leaves local-scryfall's parser (via scryq) builds for
/// `raw`, when the query is inside the hybrid subsets' structural shape:
/// no grouping/quotes, parses cleanly, top level is a leaf or an AND of
/// leaves (any OR / negation refuses).
fn subset_leaves(raw: &str) -> Option<(crate::scryq::Node, Vec<crate::scryq::Node>)> {
    use crate::scryq::Node;
    if !subset_chars_ok(raw) {
        return None;
    }
    let ast = crate::scryq::parse(raw).ok()?;
    let leaves = match &ast {
        Node::And(ts) => ts.clone(),
        n => vec![n.clone()],
    };
    if leaves.iter().any(|l| matches!(l, Node::And(_) | Node::Or(_) | Node::Not(_))) {
        return None;
    }
    Some((ast, leaves))
}

/// Bare (non-exact, unquoted) name leaf the subsets admit: the typed token
/// does not start with `!`/`-`/`/` and is not the word `not`.
fn plain_name(n: &crate::scryq::Node) -> bool {
    match n {
        crate::scryq::Node::Name { value, exact: false, quoted: false } => {
            !value.starts_with(['!', '-', '/']) && value != "not" && value != "or"
        }
        _ => false,
    }
}

/// An operator leaf typed with the bare `:` comparator on a plain value.
fn colon_op(n: &crate::scryq::Node) -> Option<&crate::scryq::OpLeaf> {
    match n {
        crate::scryq::Node::Op(l) if l.cmp == ":" && !l.quoted && !l.regex && !l.value.is_empty() => Some(l),
        _ => None,
    }
}

/// The free-text + operator-conjunction gate + `toScryfall` normalization,
/// decided on local-scryfall's parse (scryq::parse) — no tokenizer here.
/// Returns the canonical walk base when the query is an AND of:
///
///  * bare name leaves (lowercased by the parser);
///  * `t:`/`type:`/`c:`/`color:`/`colour:`/`otag:`/`oracletag:`/`function:`
///    leaves with the `:` comparator on a bare value (re-emitted from the
///    lowercased keyword, value verbatim);
///  * `is:` leaves (`is:<TYPE_WORD>` → `t:`, others verbatim);
///
/// and `toScryfall` would emit exactly that join. Refused (→ caller
/// proxies, Node answers verbatim): parse errors, quotes, grouping, `|`,
/// negation, `or`, exact `!`, regex, other keywords/comparators, and the
/// bare word `colorless` (the parser rewrites it to `c:c`, a walk Node
/// never sends verbatim).
pub fn walk_base(raw: &str) -> Option<String> {
    if raw.split(is_js_ws).any(|t| t.eq_ignore_ascii_case("colorless")) {
        return None;
    }
    let (ast, leaves) = subset_leaves(raw)?;
    for l in &leaves {
        if plain_name(l) {
            continue;
        }
        let op = colon_op(l)?;
        if !matches!(op.raw.as_str(), "is" | "t" | "type" | "c" | "color" | "colour" | "otag" | "oracletag" | "function") {
            return None;
        }
    }
    Some(crate::scryq::to_scryfall(&ast))
}

use crate::scryq::is_js_ws;

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
        self.inner.guard().entries.get(key).cloned()
    }
    fn mem_put(&self, key: String, e: Arc<QEntry>) {
        let mut g = self.inner.guard();
        if g.entries.len() > 256 {
            g.entries.clear();
        }
        g.entries.insert(key, e);
    }
    fn slot(&self, key: &str) -> Arc<WalkSlot> {
        self.inner.guard().inflight.entry(key.to_string()).or_default().clone()
    }
    fn finish(&self, key: &str) {
        self.inner.guard().inflight.remove(key);
    }
}

/// The inflight half of the single-flight (see [`crate::flight`]): the walk's
/// explicit finish only ran on the linear path; the paired guard reclaims
/// the slot key on the panic path too.
impl crate::flight::FlightCache for CollQCache {
    type Slot = WalkSlot;
    fn flight_slot(&self, key: &str) -> Arc<Self::Slot> {
        self.slot(key)
    }
    fn flight_remove(&self, key: &str, slot: &Arc<Self::Slot>) {
        if self.inner.guard().inflight.get(key).is_some_and(|c| Arc::ptr_eq(c, slot)) {
            self.finish(key);
        }
    }
    #[cfg(test)]
    fn flight_len(&self, key: &str) -> usize {
        self.inner.guard().inflight.contains_key(key) as usize
    }
}

use crate::util::now_ms;

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
    let (slot, _slot_guard) = crate::flight::slot_guarded(&st.collq_cache, &key);
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
            let mut walk = match sidecar::walk_collection_names(&init_st, &init_base, &init_cq.walk_q(), &init_cq.lang_code, WALK_LIMIT, init_cq.by_printing).await {
                Ok(w) => w,
                Err(e) => return Ok(Err(e)),
            };
            // resolveCollectionQuery's bounded backfill (unit 7c): the first
            // 500 RAW cards of the window, normalized and upserted through
            // the shared card_cache writer WITHOUT blocking the answer —
            // Node runs it on a floating promise for exactly this reason.
            // `fetched` is only non-empty on a real walk (never on an
            // entry reuse), matching Node's `{ entry, fetched }` contract.
            if !walk.fetched.is_empty() {
                let backfill_raw = std::mem::take(&mut walk.fetched);
                let lang_name = crate::search::to_name(Some(init_cq.scry_code.as_str()));
                let backfill_pool = Arc::clone(init_st.db.as_ref().expect("db present above"));
                tokio::spawn(async move {
                    let res = tokio::task::spawn_blocking(move || {
                        let maps: Vec<serde_json::Map<String, Value>> = backfill_raw
                            .iter()
                            .map(|raw| sidecar::normalize_card(raw, &lang_name))
                            .collect();
                        let conn = backfill_pool.get().map_err(|e| e.to_string())?;
                        crate::card_cache::cache_normalized_cards(&conn, &maps)
                            .map_err(|e| e.to_string())
                    })
                    .await;
                    match res {
                        Err(j) => tracing::error!("Scryfall collection query caching failed: {j}"),
                        Ok(Err(s)) => tracing::error!("Scryfall collection query caching failed: {s}"),
                        Ok(Ok(())) => {}
                    }
                });
            }
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
    drop(_slot_guard);
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

/// ownedByCardIds' swap of the membership predicate.
const OWNED_BASE_IDS: &str = "
    FROM collection c
    JOIN card_cache cc ON c.card_id = cc.id
    WHERE c.user_id = ? AND c.quantity > 0
      AND c.card_id IN (SELECT value FROM json_each(?))";

fn owned_base(by_printing: bool) -> &'static str {
    if by_printing { OWNED_BASE_IDS } else { OWNED_BASE }
}

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
        .map(|n| if cq.by_printing { n.clone() } else { n.trim().to_lowercase() })
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
        match owned_count(&conn, cq.user_id, &membership, cq.by_printing) {
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

/// `readCatalogSnapshot(readDb, 'remote', membershipVersion, expected)` —
/// the walk leg's per-request snapshot check (iter-16). Node computes
/// `remote:{mv}:{revision}` inside the same dedicated read transaction that
/// reads the owned rows and throws CATALOG_SNAPSHOT_CHANGED on mismatch
/// (route → 409 `{error}`). Here the response cache is rev-gated and the
/// header token build() minted came from a pinned txn of the same revision,
/// so the check re-reads the revision itself on its own pinned txn per
/// request — a cached replay never skips it. Falsy `expected` (empty string,
/// absent, duplicated key → array → None) skips the check exactly like
/// Node's `if (expectedSnapshot && expectedSnapshot !== token)`.
/// Ok(()) = serve; Err(actual token) = 409 CATALOG_SNAPSHOT_CHANGED.
pub fn check_remote_snapshot(
    pool: &DbPool,
    names: &[String],
    expected: &str,
) -> Result<(), String> {
    if expected.is_empty() {
        return Ok(());
    }
    let conn = pool.get().map_err(|e| e.to_string())?;
    let _txn = Txn::begin(&conn).map_err(|e| e.to_string())?;
    let rev = catalog_snapshot(&conn)?;
    let token = format!("remote:{}:{rev}", membership_version(names));
    if token != expected {
        return Err(token);
    }
    Ok(())
}

fn owned_rows(conn: &Connection, cq: &CollectionQ, membership: &str) -> Result<Vec<Value>, String> {
    let window = cq.row_window();
    let mut sql = format!("SELECT {PROJECTION}");
    sql.push_str(owned_base(cq.by_printing));
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

fn owned_count(conn: &Connection, user_id: i64, membership: &str, by_printing: bool) -> Result<i64, String> {
    let sql = format!("SELECT COUNT(*) AS n {}", owned_base(by_printing));
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

// ---------------------------------------------------------------------------
// Node's two LOCAL answer legs (resolveCollectionQuery, scryfallApi.js
// 1371-1440). The walk path above mirrors sidecar mode; these mirror the
// SQLite answers Node runs WITHOUT the engine:
//   * answerLocally — `parsed.mode === 'local' && !LOCAL_SCRYFALL`: every
//     term is data-backed, the owned rows ARE the answer (no API, no tag
//     index, no eligibility gate; eligibleOnly:false).
//   * the local Oracle Tags path — supportsLocalCollectionQuery: operators
//     include `otag:` and every operator is LOCAL_COLLECTION_OPERATORS
//     (color/identity/rarity/lang/mv/otag + directives); fires in BOTH modes
//     once a generation is active and the user's owned rows are backfilled,
//     with eligibleOnly:true (compileCollectionQuery's default).
// Both share rawQuerySql.compileCollectionQuery; only the accepted-subset
// gate differs. Anything outside the subset (or/not/or grouping, rarity,
// mv, set, number, lang, quoted terms, …) stays on the walk/proxy contract.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct LocalQ {
    pub user_id: i64,
    /// true = the Oracle Tags leg (eligibleOnly:true, isReady gate,
    /// snapshot mode 'local':{genId}); false = answerLocally ('rows':0).
    pub tag_leg: bool,
    pub has_window: bool,
    pub page: i64,
    pub limit: i64,
    pub include_total: bool,
    pub expected_snapshot: Option<String>,
    /// The query AST (local-scryfall's parse, lowered by scryq);
    /// compileWhere runs over it.
    pub ast: crate::scryq::Node,
    /// answerLocally reached through the engine-outage catch: Node relabels
    /// it cacheStatus/source 'local-fallback' (X-Source emitted).
    pub outage_fallback: bool,
}

/// Leaf kinds the hybrid local legs admit (decided on the parsed AST).
#[derive(PartialEq)]
enum LocalKind {
    Name,
    Type,
    Color,
    Otag,
    Directive,
}

fn local_kind(n: &crate::scryq::Node) -> Option<LocalKind> {
    if plain_name(n) {
        return Some(LocalKind::Name);
    }
    let l = colon_op(n)?;
    Some(match l.op.as_str() {
        "is" => LocalKind::Type, // is:<TYPE_WORD> (isflag refuses)
        "type" => LocalKind::Type,
        "color" | "identity" if matches!(l.raw.as_str(), "c" | "color" | "colour" | "id" | "ci" | "commander" | "cmd") => LocalKind::Color,
        "otag" => LocalKind::Otag,
        op if crate::scryq::DIRECTIVES.contains(&op) => LocalKind::Directive,
        _ => return None,
    })
}

/// Gate for both local legs, on local-scryfall's parse. The caller
/// (search::plan) owns the mode gate: rows leg only when `!LOCAL_SCRYFALL`;
/// tag leg in both modes. `tag_leg` restricts leaves to
/// LOCAL_COLLECTION_OPERATORS (otag + color family + directives) with at
/// least one otag leaf; the rows leg must have none (an otag leaf makes
/// analyze() return 'catalog').
pub fn local_plan(
    user_id: i64,
    qraw: &str,
    has_window: bool,
    page: i64,
    limit: i64,
    include_total: bool,
    expected_snapshot: Option<String>,
    tag_leg: bool,
) -> Option<LocalQ> {
    let (ast, leaves) = subset_leaves(qraw)?;
    let mut has_otag = false;
    for l in &leaves {
        let k = local_kind(l)?;
        if tag_leg && matches!(k, LocalKind::Name | LocalKind::Type) {
            return None;
        }
        has_otag |= k == LocalKind::Otag;
    }
    if tag_leg != has_otag {
        return None;
    }
    Some(LocalQ { user_id, tag_leg, has_window, page, limit, include_total, expected_snapshot, ast, outage_fallback: false })
}

/// Node's QuerySyntaxError verdict for the upstream-mode pre-reject
/// (`classifyQuery` throw → route 400 INVALID_QUERY), decided by
/// local-scryfall's parser (via scryq). Returns the error message.
pub fn syntax_reject(q: &str) -> Option<String> {
    crate::scryq::parse(q).err().map(|e| e.0)
}

pub fn definitely_invalid(q: &str) -> bool {
    syntax_reject(q).is_some()
}

/// compileCollectionQuery's projection == Q_COLS (the walk leg's row order).
const LOCAL_BASE: &str = "
    FROM collection c
    JOIN card_cache cc ON cc.id = c.card_id
    WHERE c.user_id = ? AND c.quantity > 0";

/// ownedByNames projection (shared by the walk leg's owned_rows and the
/// local legs — identical Q_COLS column order).
const PROJECTION: &str = "
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
    ";

pub enum LocalOutcome {
    Built { cards: Vec<Value>, total: Option<String>, snapshot: String },
    /// expectedSnapshot mismatch — Node throws CATALOG_SNAPSHOT_CHANGED and
    /// the route answers 409; carries the ACTUAL token for the log.
    Mismatch { actual: String },
    /// tag leg, isReady gate failed → Node continues to the walk.
    NotReady,
    Failed(String),
}

pub fn build_local(pool: &DbPool, lq: &LocalQ) -> LocalOutcome {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return LocalOutcome::Failed(format!("pool: {e}")),
    };
    // withDedicatedReadTransaction twin: readiness, generation, snapshot and
    // rows/count pin to ONE WAL view (the coverage-gate comment in
    // scryfallApi.js — a collection mutation cannot slip between them).
    let txn = match Txn::begin(&conn) {
        Ok(t) => t,
        Err(e) => return LocalOutcome::Failed(format!("BEGIN: {e:?}")),
    };

    let mut gen_id: i64 = 0;
    if lq.tag_leg {
        // oracleTags.supportsLocalCollectionQuery fired at plan time; here
        // isReady(database, userId) decides, exactly like Node:
        // active generation + no owned mtg-% row missing oracle_id /
        // scryfall_search_eligible. Failure answers `null` → the walk runs.
        let active: Option<i64> = match conn.query_row(
            "SELECT id FROM oracle_tag_generations WHERE active = 1 LIMIT 1",
            [],
            |r| r.get::<_, Option<i64>>(0),
        ) {
            Ok(v) => v,
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return LocalOutcome::Failed(format!("generation: {e}")),
        };
        let Some(id) = active else {
            // Txn::Drop issues the ROLLBACK.
            return LocalOutcome::NotReady;
        };
        gen_id = id;
        let missing: Option<i64> = match conn.query_row(
            "SELECT 1 AS missing FROM card_cache
             WHERE id LIKE 'mtg-%'
               AND (oracle_id IS NULL OR scryfall_search_eligible IS NULL)
               AND EXISTS (
                 SELECT 1 FROM collection c
                 WHERE c.card_id = card_cache.id AND c.user_id = ? AND c.quantity > 0
               ) LIMIT 1",
            rusqlite::params![lq.user_id],
            |r| r.get::<_, Option<i64>>(0),
        ) {
            Ok(v) => v,
            // No row = nothing missing (Node's `get` returns undefined).
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return LocalOutcome::Failed(format!("readiness: {e}")),
        };
        if missing.is_some() {
            return LocalOutcome::NotReady;
        }
    }

    let rev = match catalog_snapshot(&conn) {
        Ok(s) => s,
        Err(e) => return LocalOutcome::Failed(e),
    };
    // readCatalogSnapshot(client, mode, sourceVersion, expected):
    // `${mode}:${sourceVersion}:${Number(row?.revision) || 0}`.
    let mode = if lq.tag_leg { "local" } else { "rows" };
    let token = format!("{mode}:{}:{rev}", if lq.tag_leg { gen_id } else { 0 });
    if let Some(exp) = &lq.expected_snapshot {
        if !exp.is_empty() && exp != &token {
            return LocalOutcome::Mismatch { actual: token };
        }
    }

    let mut params: Vec<Sql> = vec![Sql::Integer(lq.user_id)];
    let where_sql = match crate::scryq::compile_where(&lq.ast, &mut params) {
        Ok(s) => s,
        Err(e) => return LocalOutcome::Failed(e),
    };
    let eligible = if lq.tag_leg { "\n      AND cc.scryfall_search_eligible = 1" } else { "" };
    let base = format!("{LOCAL_BASE}{eligible}");

    let rows_sql = format!(
        "SELECT {PROJECTION} {base} {where_sql}\n      ORDER BY c.added_at DESC, c.id DESC{}",
        if lq.has_window { " LIMIT ? OFFSET ?" } else { "" }
    );
    let mut row_params = params.clone();
    if let Some((limit, offset)) = lq.row_window() {
        row_params.push(Sql::Integer(limit));
        row_params.push(Sql::Integer(offset));
    }
    let cards = (|| -> Result<Vec<Value>, String> {
        let mut stmt = conn.prepare_cached(&rows_sql).map_err(|e| format!("prepare rows: {e}"))?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(row_params.iter()))
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
    })();
    let cards = match cards {
        Ok(v) => v,
        Err(e) => return LocalOutcome::Failed(e),
    };
    let total = if lq.include_total {
        let count_sql = format!("SELECT COUNT(*) AS n {base} {where_sql}");
        let n: i64 = match conn.query_row(
            &count_sql,
            rusqlite::params_from_iter(params.iter()),
            |r| r.get::<_, Option<i64>>(0),
        ) {
            Ok(v) => v.unwrap_or(0),
            Err(e) => return LocalOutcome::Failed(format!("count: {e}")),
        };
        // `Number(count && count.n) || 0`
        Some(n.to_string())
    } else {
        None
    };
    if txn.commit().is_err() {
        return LocalOutcome::Failed("COMMIT".to_string());
    }
    LocalOutcome::Built { cards, total, snapshot: token }
}

impl LocalQ {
    fn row_window(&self) -> Option<(i64, i64)> {
        self.has_window.then(|| (self.limit, (self.page - 1).saturating_mul(self.limit)))
    }
}

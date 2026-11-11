//! Port of `GET /api/stats` from `backend/src/routes/stats.js` (the
//! `router.get('/stats', cached(() => 'stats', ...))` handler — the other
//! /stats/* routes stay on Node).
//!
//! Parity contract (BRIEF rule 1): byte-identical body for the same DB
//! state, so the Express default ETag matches and If-None-Match replays 304.
//! The body is assembled as an insertion-ordered serde_json Map mirroring
//! stats.js's object-literal key order, and every JS numeric step
//! (`Number.prototype.toFixed` → `parseFloat`, `Math.round`, `Math.min`,
//! integer-valued serialization) is reproduced with JS semantics, not Rust's
//! (Rust's `{:.2}` is half-to-even; V8 rounds the exact binary value half
//! away from zero — see the node-generated vectors in tests/stats.rs).
//!
//! Cache (utils/statsCache.js twin): per-user entries keyed
//! `{uid}|{gen}|{rev}|stats`, 5-minute TTL, 500 entries max evicting the
//! oldest inserted, `gen` bumped by `on_proxied_write` for authenticated
//! non-GET /api traffic (invalidateOnWrite's job, since Rust — not Node —
//! sees the request first). `rev` is the three trigger-maintained counters
//! rendered exactly like statsCache.dbRevision.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use rusqlite::{types::Value as SqlValue, Connection, Row};
use serde_json::{Map, Value};
use tokio::sync::OnceCell;

use crate::collection::{
    body_etag, etag_matches, gzip_bytes, json_num, resolve_card_price, sqlite_value, truthy,
    wants_encoding,
};
use crate::db::DbPool;
use crate::stats_sql::{AGGREGATE_SQL, DECK_ROWS_SQL, OWNERSHIP_SQL, RECENT_ADDED_SQL, TOP_VALUABLE_SQL};
use crate::AppState;

const TTL: Duration = Duration::from_secs(5 * 60); // statsCache.js TTL_MS
const MAX_ENTRIES: usize = 500; // statsCache.js MAX_ENTRIES
/// Background refresher (iter 13, owner ruling #1): poll the revision
/// counter and pre-build the post-bump body for recently-active users so a
/// request never rides the blocking compute path after an invalidation.
/// Pre-building only ever makes the served body FRESHER — the 5 s SWR bound
/// and the request-path semantics are untouched.
const REFRESH_TICK: Duration = Duration::from_millis(250);
/// Only users who requested stats within this window are pre-built; a user
/// who stopped asking must not cost background query work (Node does no
/// unrequested work either).
const REFRESH_ACTIVE: Duration = Duration::from_secs(30);
/// Debounce: a (gen, rev) must be observed stable across two consecutive
/// ticks before it is pre-built. Keeps the write itself in flight (edit +
/// revert pairs bump twice) out of the build fan-out, and keeps the
/// post-write first-request still a deterministic `miss` — the same window
/// Node's own cache would answer `miss` in.
const REFRESH_DEBOUNCE_TICKS: u8 = 2;
/// After a failed pre-build, back off per user instead of retrying per tick.
const REFRESH_COOLDOWN: Duration = Duration::from_secs(5);
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

// ---------------------------------------------------------------------------
// Response cache (utils/statsCache.js mirror)
// ---------------------------------------------------------------------------

struct StoreEntry {
    at: Instant,
    seq: u64, // insertion order (Map iteration order in Node)
    entry: Arc<StatsEntry>,
}

/// How long a superseded body may keep serving while its replacement is
/// rebuilt in the background (iter 12, stale-while-revalidate). OWNER RULING
/// (Brent, 2026-11-10): the /api/stats exception to hard rule 1 is bounded at
/// a MAX of 5 seconds of staleness — past that a request must see the exact
/// post-write body, so the entry is pinned to exactly 5 s (see the
/// `stale_grace_matches_owner_bound` test; raising this silently breaks rule
/// 1). A cold cache or an entry past this grace blocks and computes, exactly
/// like the old path.
const STALE_GRACE: Duration = Duration::from_secs(5);

/// Single-flight slot: concurrent misses on one cache key share a single
/// `build_body`. Node's statsCache has no such coalescing, but it does not
/// need one to be MATCHED — it just loses every stampede: one invalidation
/// (price sweep, collection edit) makes every in-flight dashboard request
/// run the full ~0.6 s computation on Node's single event loop. Serving N
/// waiters from one computation is what keeps the realistic (varied+writes)
/// bench honest instead of a regeneration storm.
type ComputeSlot = OnceCell<Result<Arc<StatsEntry>, String>>;

#[derive(Default)]
struct Inner {
    gens: HashMap<i64, u64>,
    store: HashMap<String, StoreEntry>,
    /// Most recently COMPUTED body per user (any key/rev). Backs the
    /// stale-while-revalidate path: a rev churn (write) never leaves the
    /// request path waiting on the ~0.6 s aggregate as long as a recent
    /// body exists. Insertion time drives the STALE_GRACE bound.
    latest: HashMap<i64, (Arc<StatsEntry>, Instant)>,
    seq: u64,
    inflight: HashMap<String, Arc<ComputeSlot>>,
    /// Background refresher bookkeeping (iter 13): last stats request per
    /// user — gates which users get a post-bump body pre-built.
    seen: HashMap<i64, Instant>,
    /// The (gen, rev) the current `latest` body was computed for; compared
    /// against the live counters to decide whether the SWR slot still
    /// represents the current DB state.
    latest_key: HashMap<i64, (u64, Option<String>)>,
    /// Debounce state: (gen, rev) under observation per user and the number
    /// of consecutive ticks it has been seen stable.
    observed: HashMap<i64, (u64, Option<String>, u8)>,
    /// Per-user backoff instant after a failed background pre-build.
    cooldown: HashMap<i64, Instant>,
}

pub(crate) struct StatsEntry {
    raw: Bytes,
    /// Pre-gzipped body for clients that accept it; None when the raw body
    /// sits under the compression middleware's 1024-byte threshold.
    gz: Option<Bytes>,
    etag: String,
}

impl StatsEntry {
    pub(crate) fn new(raw: Bytes) -> Self {
        let gz = wants_encoding(Some("gzip"), "gzip", raw.len()).then(|| gzip_bytes(&raw));
        let etag = body_etag(&raw);
        Self { raw, gz, etag }
    }
    pub(crate) fn raw(&self) -> &Bytes {
        &self.raw
    }
    pub(crate) fn gz(&self) -> &Option<Bytes> {
        &self.gz
    }
    pub(crate) fn etag(&self) -> &str {
        &self.etag
    }
}

pub struct StatsCache {
    inner: Mutex<Inner>,
    /// Number of full bodies actually computed (test + observability).
    computes: AtomicU64,
}

impl Default for StatsCache {
    fn default() -> Self {
        Self { inner: Mutex::new(Inner::default()), computes: AtomicU64::new(0) }
    }
}

impl StatsCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The coalescing slot for a key (created on first miss).
    fn slot(&self, key: &str) -> Arc<ComputeSlot> {
        self.inner.lock().unwrap().inflight.entry(key.to_string()).or_default().clone()
    }

    /// Drop a finished slot so the NEXT miss starts a fresh computation
    /// (the completed entry now lives in the store).
    fn finish(&self, key: &str) {
        self.inner.lock().unwrap().inflight.remove(key);
    }

    /// Observability for the single-flight test and ops dashboards.
    pub fn computes(&self) -> u64 {
        self.computes.load(Ordering::Relaxed)
    }

    /// statsCache.bump(userId) — invalidate everything computed for a user.
    pub fn bump(&self, user_id: i64) {
        let mut inner = self.inner.lock().unwrap();
        *inner.gens.entry(user_id).or_insert(0) += 1;
    }

    /// statsCache.keyFor: `${userId}|${gen || 0}|${rev ?? ''}|${name}`.
    fn key_for(user_id: i64, gen: u64, rev: Option<&str>) -> String {
        StatsCache::key_named(user_id, gen, rev, "stats")
    }

    /// keyFor with an explicit cache name — the shape statsCache.js uses for
    /// every `cached(nameFn, ...)` route (`history:30d`, `networth`,
    /// `top-mover`). Same store, same gen: a bump invalidates every name for
    /// that user, exactly like the single shared Map in statsCache.js.
    pub(crate) fn key_named(user_id: i64, gen: u64, rev: Option<&str>, name: &str) -> String {
        format!("{user_id}|{gen}|{}|{name}", rev.unwrap_or(""))
    }

    /// Current gen for a user WITHOUT marking them active (the stats
    /// refresher pre-builds the `/stats` body only; a /stats/history request
    /// must not enroll a user in that background work).
    pub(crate) fn gen_of(&self, user_id: i64) -> u64 {
        *self.inner.lock().unwrap().gens.get(&user_id).unwrap_or(&0)
    }

    /// statsCache.set for a NON-stats name: same store (Node's cache is one
    /// shared Map across all cached names), but never touches the
    /// stale-while-revalidate `latest` slot — that slot backs the `/stats`
    /// SWR path only; a sub-route body must never be served for `/stats`.
    pub(crate) fn set_named(&self, user_id: i64, key: String, entry: Arc<StatsEntry>) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.store.contains_key(&key) && inner.store.len() >= MAX_ENTRIES {
            let oldest = inner.store.values().min_by_key(|e| e.seq).map(|e| e.seq);
            if let Some(seq) = oldest {
                inner.store.retain(|_, v| v.seq != seq);
            }
        }
        inner.seq += 1;
        let seq = inner.seq;
        let _ = user_id;
        inner.store.insert(key, StoreEntry { at: Instant::now(), seq, entry });
    }

    pub(crate) fn get_entry(&self, key: &str) -> Option<Arc<StatsEntry>> {
        self.get(key)
    }

    pub(crate) fn slot_for(&self, key: &str) -> Arc<ComputeSlot> {
        self.slot(key)
    }

    pub(crate) fn finish_key(&self, key: &str) {
        self.finish(key)
    }

    /// The live data revision (statsCache.dataRevision), read off the pool.
    pub(crate) fn revision(pool: &DbPool) -> Option<String> {
        pool.get().ok().and_then(|c| StatsCache::db_revision(&c))
    }

    /// db_revision on an already-held connection.
    pub(crate) fn revision_of(conn: &Connection) -> Option<String> {
        StatsCache::db_revision(conn)
    }

    // --- background refresher support (iter 13) -----------------------------

    /// Record a stats request for `user_id` and return its current gen.
    /// (The request path's key resolution is the only thing that marks a
    /// user "active" for the refresher — mirrors Node doing work only for
    /// users who asked.)
    fn touch(&self, user_id: i64) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.seen.insert(user_id, Instant::now());
        inner.gens.get(&user_id).copied().unwrap_or(0)
    }

    /// Users whose last stats request is within `window`.
    fn active_users(&self, window: Duration) -> Vec<i64> {
        let now = Instant::now();
        self.inner
            .lock()
            .unwrap()
            .seen
            .iter()
            .filter(|(_, at)| now.duration_since(**at) <= window)
            .map(|(u, _)| *u)
            .collect()
    }

    /// Decide whether `user_id` needs a background pre-build for the CURRENT
    /// (gen, rev), applying the stability debounce. Returns the cache key to
    /// build when it is due. The debounce requires the same (gen, rev) on
    /// REFRESH_DEBOUNCE_TICKS consecutive observations, so a request that
    /// follows a write immediately still sees `miss` (Node answers `miss`
    /// in that window too) and edit+revert pairs do not fan out builds.
    fn refresh_due(&self, user_id: i64, rev: Option<&str>) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        if inner
            .cooldown
            .get(&user_id)
            .is_some_and(|at| Instant::now().duration_since(*at) < REFRESH_COOLDOWN)
        {
            return None;
        }
        let gen = inner.gens.get(&user_id).copied().unwrap_or(0);
        let key = StatsCache::key_for(user_id, gen, rev);
        if inner
            .store
            .get(&key)
            .is_some_and(|e| Instant::now().duration_since(e.at) <= TTL)
        {
            inner.observed.remove(&user_id);
            return None;
        }
        let cur = (gen, rev.map(|s| s.to_string()));
        // The SWR slot already holds the CURRENT body and is inside the
        // grace: request-path SWR (with its slot-shared background build)
        // covers this case; pre-building would duplicate the work.
        let swr_covers = inner
            .latest_key
            .get(&user_id)
            .zip(inner.latest.get(&user_id).map(|(_, at)| *at))
            .is_some_and(|((g, r), at)| *g == cur.0 && *r == cur.1 && Instant::now().duration_since(at) <= STALE_GRACE);
        if swr_covers {
            inner.observed.remove(&user_id);
            return None;
        }
        let mut due = false;
        let prev_n = match inner.observed.get(&user_id) {
            Some((g, r, n)) if *g == cur.0 && *r == cur.1 => {
                due = *n + 1 >= REFRESH_DEBOUNCE_TICKS;
                *n
            }
            _ => 0,
        };
        inner.observed.insert(user_id, (cur.0, cur.1, prev_n + 1));
        if !due || inner.inflight.contains_key(&key) {
            return None;
        }
        Some(key)
    }

    fn refresh_failed(&self, user_id: i64) {
        self.inner.lock().unwrap().cooldown.insert(user_id, Instant::now());
    }

    /// statsCache.get: TTL-bound hit, expired entries dropped on probe.
    fn get(&self, key: &str) -> Option<Arc<StatsEntry>> {
        let mut inner = self.inner.lock().unwrap();
        let hit = inner.store.get(key)?;
        if Instant::now().duration_since(hit.at) > TTL {
            inner.store.remove(key);
            return None;
        }
        Some(hit.entry.clone())
    }

    /// statsCache.set: evict the oldest-inserted entry when full. Re-setting
    /// a live key keeps its original insertion slot (JS Map semantics), which
    /// the stored `seq` preserves. Also records the entry as the user's
    /// newest computed body for the stale-while-revalidate path.
    fn set(&self, user_id: i64, key: String, entry: Arc<StatsEntry>) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.store.contains_key(&key) && inner.store.len() >= MAX_ENTRIES {
            let oldest = inner.store.values().min_by_key(|e| e.seq).map(|e| e.seq);
            if let Some(seq) = oldest {
                inner.store.retain(|_, v| v.seq != seq);
            }
        }
        inner.seq += 1;
        let seq = inner.seq;
        inner.latest.insert(user_id, (Arc::clone(&entry), Instant::now()));
        inner
            .latest_key
            .insert(user_id, parse_key_parts(&key));
        inner.store.insert(key, StoreEntry { at: Instant::now(), seq, entry });
    }

    /// Stale-while-revalidate lookup (iter 12): the user's most recently
    /// computed body, if computed within STALE_GRACE. Returned while a
    /// background rebuild of the current key is in flight, so writes never
    /// put the ~0.6 s aggregate on a request's critical path.
    pub(crate) fn stale_for(&self, user_id: i64) -> Option<Arc<StatsEntry>> {
        let inner = self.inner.lock().unwrap();
        let (entry, at) = inner.latest.get(&user_id)?;
        (at.elapsed() <= STALE_GRACE).then(|| entry.clone())
    }

    /// statsCache.dbRevision: one read of the three counters, rendered
    /// `${c ?? '-'}.${d ?? '-'}.${a ?? '-'}`; None when the row is missing or
    /// the query errors (Node's `catch { return null }`) — the key then uses
    /// the empty revision segment.
    fn db_revision(conn: &Connection) -> Option<String> {
        let mut stmt = conn
            .prepare_cached(
                "SELECT (SELECT revision FROM collection_read_revision WHERE id = 1) AS c,\n             (SELECT revision FROM deck_list_revision WHERE id = 1) AS d,\n             (SELECT revision FROM stats_aux_revision WHERE id = 1) AS a",
            )
            .ok()?;
        let mut rows = stmt.query([]).ok()?;
        let row = rows.next().ok()??;
        let part = |i: usize| match row.get_ref(i).ok()? {
            rusqlite::types::ValueRef::Null => Some("-".to_string()),
            rusqlite::types::ValueRef::Integer(v) => Some(v.to_string()),
            rusqlite::types::ValueRef::Real(f) => Some(js_number_string(f)),
            _ => None,
        };
        Some(format!("{}.{}.{}", part(0)?, part(1)?, part(2)?))
    }
}

/// statsCache.invalidateOnWrite twin: Node bumps a user's generation on any
/// authenticated non-GET /api request *after* auth. Rust fronts those writes
/// and proxies them on, so it runs the same auth lookup here and bumps on
/// success. API-key writes never bump (auth rejects them before Node's
/// invalidateOnWrite would ever run), and HEAD is excluded exactly like Node.
pub fn on_proxied_write(st: &AppState, method: &Method, path: &str, headers: &HeaderMap) {
    // stats_subs shares the very same gens map (Node keeps ONE gen map across
    // all cached names), so it needs the bump too when it owns the sub-routes.
    if !(st.owns("stats") || st.owns("stats_subs")) {
        return;
    }
    if *method == Method::GET || *method == Method::HEAD || !path.starts_with("/api/") {
        return;
    }
    let Some(pool) = st.db.clone() else { return };
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string());
    let cache = Arc::clone(&st.stats_cache);
    // Fire-and-forget: the proxied response must not wait on our bookkeeping.
    tokio::spawn(async move {
        // ~5 us sync lookup; no need for a blocking hop (see auth.rs).
        match crate::auth::authenticate(&pool, token.as_deref(), false) {
            Ok(user) => cache.bump(user.id),
            Err(_) => {} // unauth / API-key write: Node never bumps either
        }
    });
}

// ---------------------------------------------------------------------------
// JS numeric semantics (byte-parity traps; vectors in tests/stats.rs)
// ---------------------------------------------------------------------------

/// `Number.prototype.toFixed(n)` for the finite doubles this route produces.
///
/// V8 rounds the EXACT binary value at the cut digit and breaks ties away
/// from zero ((0.125).toFixed(2) === "0.13", (2.675).toFixed(2) === "2.67"
/// because 2.675 is really 2.67499…). Rust's `format!("{:.2}")` rounds
/// half-to-even on ties, so it is only used at an expanded precision to get
/// the exact decimal digits; the rounding is done on the digit string.
/// A `>= '5'` cut is correct for both the strict-greater case and the tie
/// case, and an exact decimal expansion can never be ambiguous at this depth
/// (binary fractions terminate, so a '5' followed by all-zeros IS the tie).
/// `x >= 2^53` would need V8's ToString fallback; unreachable for prices.
fn js_to_fixed(x: f64, n: usize) -> String {
    if x == 0.0 {
        // Includes -0: (-0).toFixed(2) === "0.00" (no sign).
        return if n == 0 { "0".into() } else { format!("0.{}", "0".repeat(n)) };
    }
    if !x.is_finite() {
        return if x.is_nan() { "NaN".into() } else if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let neg = x < 0.0; // false for -0 (handled above)
    let expanded = format!("{:.80}", x.abs()); // correctly rounded, 80 fractional digits
    let (int_part, frac_part) = expanded.split_once('.').unwrap_or((&expanded, ""));
    let mut int_digits: Vec<u8> = int_part.bytes().collect();
    let mut frac_digits: Vec<u8> = frac_part.bytes().collect();
    // Cut at the (n+1)-th fractional digit; ties go away from zero → up.
    if frac_digits.get(n).copied().unwrap_or(b'0') >= b'5' {
        let mut carry = true;
        let mut i = n as isize - 1;
        while carry {
            if i >= 0 {
                let d = &mut frac_digits[i as usize];
                if *d == b'9' {
                    *d = b'0';
                } else {
                    *d += 1;
                    carry = false;
                }
                i -= 1;
            } else {
                // carry into the integer part
                let mut j = int_digits.len() as isize - 1;
                loop {
                    if j < 0 {
                        int_digits.insert(0, b'1');
                        break;
                    }
                    let d = &mut int_digits[j as usize];
                    if *d == b'9' {
                        *d = b'0';
                    } else {
                        *d += 1;
                        break;
                    }
                    j -= 1;
                }
                carry = false;
            }
        }
    }
    frac_digits.truncate(n);
    let int_str = String::from_utf8(int_digits).expect("ascii digits");
    let frac_str = String::from_utf8(frac_digits).expect("ascii digits");
    let mut out = String::with_capacity(1 + int_str.len() + n + 1);
    if neg {
        out.push('-');
    }
    out.push_str(&int_str);
    if n > 0 {
        out.push('.');
        out.push_str(&frac_str);
    }
    out
}

/// `parseFloat(x.toFixed(n))`: round through the toFixed string, then parse
/// it back to the nearest double (Rust's from_str is correctly rounded, same
/// as V8's parseFloat). -0 (e.g. from "-0.00") becomes plain 0 in JSON via
/// json_num, matching JSON.stringify(-0) === "0".
pub(crate) fn js_fixed_parsed(x: f64, n: usize) -> Value {
    json_num(js_parse_float(&js_to_fixed(x, n)))
}

/// `parseFloat(s)` for strings we produced ourselves.
fn js_parse_float(s: &str) -> f64 {
    s.parse::<f64>().unwrap_or(f64::NAN)
}

/// `Math.round(x)`: half toward +Infinity. Implemented as floor + (frac >=
/// 0.5) — NOT `floor(x + 0.5)`, which mis-rounds 0.49999999999999994.
pub(crate) fn js_math_round(x: f64) -> f64 {
    if x.is_nan() || x == 0.0 || x.is_infinite() {
        return x;
    }
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

/// `Math.min(a, 100)` — Rust's f64::min swallows NaN, JS's Math.min does not.
fn js_math_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || a < b {
        a
    } else {
        b
    }
}

/// Shortest round-trip rendering of an f64 the way JSON.stringify does it.
/// Only used for the rare non-integer revision counter; prices serialize via
/// the serde_json/ryu path which matches ECMAScript Number::toString for the
/// magnitudes this route produces (search.rs relies on the same property).
fn js_number_string(f: f64) -> String {
    if f == 0.0 {
        return "0".into();
    }
    if f.fract() == 0.0 && f.abs() < 9.007199254740992e15 {
        return (f as i64).to_string();
    }
    let s = serde_json::to_string(&Value::from(f)).unwrap_or_else(|_| "0".into());
    s.trim_matches('"').to_string()
}

/// Raw driver number as JS sees it: node-sqlite3 hands back JS numbers for
/// INTEGER/REAL and null for NULL. `as_f64` mirrors the Number coercion the
/// handler's arithmetic performs; Null becomes None (comparisons then behave
/// like JS's `null > 0` === false, and the toFixed paths would throw — which
/// the handler maps to the same 500 Node's catch produces).
fn js_num_opt(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// `Number(x)` in the arithmetic paths that JS does not throw on (division,
/// subtraction, `|| 0` guards): null/undefined-like → 0, bool → 0/1, numeric
/// string → parsed, anything else → NaN (which JSON.stringify renders null,
/// matching NaN propagation through toFixed/round).
pub(crate) fn js_to_num(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::Null => 0.0,
        Value::Bool(b) => if *b { 1.0 } else { 0.0 },
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else {
                t.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        _ => f64::NAN,
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

/// `JSON.parse(x || '[]')` plus a normalization pass: SQLite's json_*
/// functions print integer-valued REALs as `12.0` while Node's
/// JSON.parse → res.json cycle would emit `12`. serde_json keeps the 12.0
/// form, so integral floats are normalized to integers to reproduce Node's
/// re-serialized bytes (types/rarities are SUMs — integers — but color_values
/// runs through ROUND() and sets.value is a SUM of REALs).
fn js_parse_json_array(v: &Value, name: &str) -> Result<Value, String> {
    let text = match v {
        Value::Null => "[]".to_string(),
        Value::String(s) if s.is_empty() => "[]".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(), // JS `|| '[]'` only fires on falsy; a number would hit JSON.parse(number) → that number. SQL gives text/null only.
    };
    let mut parsed: Value = serde_json::from_str(&text).map_err(|e| format!("{name} json: {e}"))?;
    normalize_integral_floats(&mut parsed);
    if !parsed.is_array() {
        return Err(format!("{name} is not an array"));
    }
    Ok(parsed)
}

fn normalize_integral_floats(v: &mut Value) {
    match v {
        Value::Array(items) => items.iter_mut().for_each(normalize_integral_floats),
        Value::Object(map) => map.values_mut().for_each(normalize_integral_floats),
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.is_finite() && f.abs() < 9.007199254740992e15 {
                    *v = Value::from(f as i64);
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Row shaping (parseCardRow-lite: raw driver values, column order preserved)
// ---------------------------------------------------------------------------

pub(crate) fn row_to_object(row: &Row) -> rusqlite::Result<Map<String, Value>> {
    let ncols = row.as_ref().column_count();
    let mut obj = Map::with_capacity(ncols);
    for i in 0..ncols {
        let name = row.as_ref().column_name(i).unwrap_or("").to_owned();
        obj.insert(name, sqlite_value(row, i)?);
    }
    Ok(obj)
}

/// applyProxyRarity from utils/priceHelpers.js. Every column the handler
/// selects exists, so the `row.is_proxy === undefined` early-return never
/// fires here (kept for the row-missing parity). Spread semantics: existing
/// keys keep their SELECT position; base_rarity is brand-new for these
/// queries and appends at the end (insertion-ordered Map).
fn apply_proxy_rarity(mut row: Map<String, Value>) -> Map<String, Value> {
    let Some(_is_proxy_raw) = row.get("is_proxy").cloned() else { return row };
    let base = match row.get("base_rarity") {
        Some(v) => v.clone(),
        None => row.get("rarity").cloned().unwrap_or(Value::Null),
    };
    let is_proxy = truthy(row.get("is_proxy").unwrap_or(&Value::Null));
    row.insert("is_proxy".into(), Value::from(if is_proxy { 1 } else { 0 }));
    row.insert("base_rarity".into(), base.clone());
    row.insert(
        "rarity".into(),
        if is_proxy { Value::String("Proxy".into()) } else { base },
    );
    row
}

/// `{ ...applyProxyRarity(row), price_trend: resolveCardPrice(row) }` — the
/// two-step the handler applies to topValuable and recentAdditions rows.
/// resolveCardPrice reads the RAW row (pre-applyProxyRarity); price_trend
/// already exists as a selected column, so the literal overwrite keeps its
/// position.
fn shape_price_row(row: Map<String, Value>) -> Map<String, Value> {
    let price = resolve_card_price(
        row.get("is_proxy").unwrap_or(&Value::Null),
        row.get("printing").unwrap_or(&Value::Null),
        row.get("price_trend").unwrap_or(&Value::Null),
        row.get("price_normal").unwrap_or(&Value::Null),
        row.get("price_holofoil").unwrap_or(&Value::Null),
    );
    let mut out = apply_proxy_rarity(row);
    out.insert("price_trend".into(), price);
    out
}

fn all_rows(conn: &Connection, sql: &str, params: &[SqlValue]) -> Result<Vec<Map<String, Value>>, String> {
    let t = std::time::Instant::now();
    let mut stmt = conn.prepare_cached(sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter())).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        out.push(row_to_object(row).map_err(|e| e.to_string())?);
    }
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    // Slow-query breadcrumb for build-cost forensics (stats rebuild audit,
    // iter 12). Thresholded so hot cache-miss paths can't turn this into a
    // log storm.
    if ms >= 5.0 {
        tracing::info!(elapsed_ms = ms, rows = out.len(), sql = &sql[..sql.len().min(60)], "slow all_rows");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

/// `new Date(ms).toISOString()` — UTC, ms precision: YYYY-MM-DDTHH:MM:SS.sssZ
/// (civil-from-days algorithm; no calendar dependency needed for a fixed
/// proleptic-Gregorian conversion).
pub(crate) fn iso_utc_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (h, rem) = (rem / 3_600_000, rem % 3_600_000);
    let (m, rem) = (rem / 60_000, rem % 60_000);
    let (s, milli) = (rem / 1000, rem % 1000);
    // Days since 1970-01-01 → y-m-d (Howard Hinnant's civil algorithm).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{milli:03}Z")
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// priceHelpers.parseSqliteUtc, ported for the formats SQLite produces. The
/// /stats body itself never runs added_at through it (rows go out raw — see
/// stats.js line 252, the map only applies applyProxyRarity + resolveCardPrice),
/// but /stats/history/networth do and the unit tests pin the semantics:
/// falsy → invalid date; Z / ±HH:MM suffix → absolute instant; bare
/// 'YYYY-MM-DD HH:MM:SS' → UTC (the function itself appends 'Z', so it is TZ
/// independent — the container is TZ=UTC anyway).
pub fn parse_sqlite_utc(s: &str) -> Option<f64> {
    if s.is_empty() {
        return None; // new Date(NaN)
    }
    let ends_tz = s.ends_with('Z')
        || (s.len() >= 6 && {
            let t = &s[s.len() - 6..];
            matches!(t.as_bytes()[0], b'+' | b'-') && t.as_bytes()[3] == b':'
        });
    if ends_tz {
        // new Date(str) on an ISO string with an explicit zone.
        return chrono_free_parse_iso(s);
    }
    // new Date(str.replace(' ', 'T') + 'Z'): naive → UTC.
    let replaced = s.replacen(' ', "T", 1);
    chrono_free_parse_iso(&format!("{replaced}Z"))
}

/// Minimal ISO-8601 parser for the shapes above: YYYY-MM-DDTHH:MM:SS(.sss)?Z
/// or with a ±HH:MM suffix. Returns epoch ms as f64, None on NaN (JS's
/// Invalid Date). Deliberately strict like `new Date()` on these inputs.
fn chrono_free_parse_iso(s: &str) -> Option<f64> {
    let (body, offset_ms) = if let Some(stripped) = s.strip_suffix('Z') {
        (stripped, 0i64)
    } else if s.len() >= 6
        && matches!(s.as_bytes()[s.len() - 6], b'+' | b'-')
        && s.as_bytes()[s.len() - 3] == b':'
    {
        let (b, tz) = s.split_at(s.len() - 6);
        let sign: i64 = if tz.as_bytes()[0] == b'-' { -1 } else { 1 };
        let hh: i64 = tz[1..3].parse().ok()?;
        let mm: i64 = tz[4..6].parse().ok()?;
        (b, sign * (hh * 3_600_000 + mm * 60_000))
    } else {
        return None;
    };
    let bytes = body.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let num = |a: usize, b: usize| body[a..b].parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    // Out-of-range fields make `new Date()` Invalid (NaN) — reject likewise.
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    let mut milli = 0i64;
    if bytes.len() > 19 {
        if bytes[19] != b'.' {
            return None;
        }
        let digits: String = body[20..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() || digits.len() + 20 != body.len() {
            return None;
        }
        milli = digits.parse::<i64>().ok()?;
        match digits.len() {
            1 => milli *= 100,
            2 => milli *= 10,
            3 => {}
            _ => {
                // new Date keeps sub-ms digits in the ms field as a fraction;
                // this route's data never has them.
                milli += digits[3..].parse::<f64>().ok()? as i64 / 10f64.powi((digits.len() - 3) as i32 - 1).max(1.0) as i64;
            }
        }
    }
    // days from civil (inverse of iso_utc_ms' algorithm).
    let y0 = if mo <= 2 { y - 1 } else { y };
    let era = if y0 >= 0 { y0 } else { y0 - 399 } / 400;
    let yoe = y0 - era * 400;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days as f64 * 86_400_000.0 + (h * 3_600_000 + mi * 60_000 + se * 1000 + milli - offset_ms) as f64)
}

/// One cache entry computed on a blocking thread: run the five queries,
/// render the body, stamp the ETag, pre-gzip for the Accept-Encoding branch
/// (the compression middleware's 1024-byte threshold lives in
/// wants_encoding), store under `key` + record as the user's newest body.
fn compute_and_store(
    cache: &Arc<StatsCache>,
    pool: &DbPool,
    user_id: i64,
    key: String,
) -> Result<Arc<StatsEntry>, String> {
    let t_build = std::time::Instant::now();
    let raw = build_body(pool, user_id)?;
    let raw = Bytes::from(raw);
    let etag = body_etag(&raw);
    let gz = wants_encoding(Some("gzip"), "gzip", raw.len()).then(|| gzip_bytes(&raw));
    let entry = Arc::new(StatsEntry { raw, gz, etag });
    cache.set(user_id, key, Arc::clone(&entry));
    cache.computes.fetch_add(1, Ordering::Relaxed);
    tracing::info!(user_id, elapsed_ms = t_build.elapsed().as_secs_f64() * 1000.0, "stats build");
    Ok(entry)
}

/// Inverse of key_for: the (gen, rev) encoded in a cache key. The rev
/// itself contains '|' (it is the `c|d|a` triple), so only the leading
/// fields and the trailing route name are fixed-position.
fn parse_key_parts(key: &str) -> (u64, Option<String>) {
    let mut it = key.split('|');
    let _uid = it.next();
    let gen = it.next().and_then(|g| g.parse().ok()).unwrap_or(0);
    let mut rev_parts: Vec<&str> = it.collect();
    rev_parts.pop(); // trailing "stats"
    let rev = rev_parts.join("|");
    (gen, if rev.is_empty() { None } else { Some(rev) })
}

/// Background refresher (owner ruling, iter 12 note #1 / iter 13): poll the
/// revision counter and pre-build the current body for recently-active
/// users after a bump, so requests landing in the post-write window are
/// either answered from the store or from a fresh SWR slot — never from the
/// blocking compute path. This only ever makes served bodies FRESHER: the
/// store receives an exact-for-current-rev entry earlier than the
/// request-path build would; the 5 s stale-while-revalidate bound is
/// untouched, staleness is never widened.
///
/// Costs are bounded the way Node's own work is: only users who requested
/// stats within REFRESH_ACTIVE get a build, only after the (gen, rev) has
/// been observed stable for REFRESH_DEBOUNCE_TICKS ticks (so in-flight edit
/// +revert pairs do not fan out builds and the immediate post-write request
/// stays a deterministic `miss`, matching what Node answers in that window),
/// and a failed pre-build backs off for REFRESH_COOLDOWN. The build runs
/// through the SAME single-flight slot as the request path, so a bump that
/// is already being rebuilt by a request never double-computes.
pub fn spawn_refresher(state: &AppState) {
    let Some(pool) = state.db.clone() else { return };
    let cache = Arc::clone(&state.stats_cache);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(REFRESH_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let users = cache.active_users(REFRESH_ACTIVE);
            if users.is_empty() {
                continue;
            }
            // One revision read per tick, kept off the runtime workers.
            let pool2 = Arc::clone(&pool);
            let rev = match tokio::task::spawn_blocking(move || {
                pool2.get().ok().and_then(|c| StatsCache::db_revision(&c))
            })
            .await
            {
                Ok(r) => r,
                Err(_) => continue, // runtime shutting down
            };
            for user_id in users {
                let Some(key) = cache.refresh_due(user_id, rev.as_deref()) else { continue };
                let slot = cache.slot(&key);
                let cache2 = Arc::clone(&cache);
                let pool3 = Arc::clone(&pool);
                let key2 = key.clone();
                tokio::spawn(async move {
                    let init_key = key2.clone();
                    let init_cache = Arc::clone(&cache2);
                    let init_pool = Arc::clone(&pool3);
                    let init = move || async move {
                        let c = Arc::clone(&init_cache);
                        let p = Arc::clone(&init_pool);
                        let k = init_key.clone();
                        // get_or_try_init nests: the stored value is the
                        // compute Result; the outer Err carries a JoinError
                        // (same shape as the request-path init below).
                        tokio::task::spawn_blocking(move || {
                            compute_and_store(&c, &p, user_id, k)
                        })
                        .await
                        .map_err(|e| format!("refresh join: {e}"))
                    };
                    // A compute failure lands as Ok(Err(..)) (the error is
                    // the stored value) or Err(JoinError); both must engage
                    // the backoff or the poller retries every tick.
                    match slot.get_or_try_init(init).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => {
                            tracing::warn!(user_id, error = %e, "stats refresher build failed");
                            cache2.refresh_failed(user_id);
                        }
                        Err(e) => {
                            tracing::warn!(user_id, error = %e, "stats refresher build failed");
                            cache2.refresh_failed(user_id);
                        }
                    }
                    cache2.finish(&key2);
                });
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// Vintage list from utils/priceHelpers.js isVintageSet (verbatim).
fn is_vintage_set(id: &str) -> bool {
    const VINTAGE: [&str; 31] = [
        "lea", "leb", "2ed", "arn", "atq", "3ed", "leg", "drk", "fem", "4bb", "4ed", "ice", "bchr", "chr", "ren",
        "rin", "hml", "all", "mir", "vis", "5ed", "wth", "tmp", "sth", "exo", "ugl", "usg", "ulg", "6ed", "uds",
        "mmq",
    ];
    VINTAGE.contains(&id.to_lowercase().as_str())
}

/// The five queries stats.js issues (aggregate, topValuable, recentAdditions,
/// decks, ownership — ownership only when the user has decks). Node issues
/// them sequentially on one connection, but each is an independent autocommit
/// read and there is no shared snapshot across them either way, so issuing
/// them concurrently on separate pool connections yields the same
/// consistency class: every query is internally consistent at its own start.
/// aggregate (~490 ms) dominates; top/recent/decks→ownership (< 100 ms
/// combined) overlap it. Assembly below consumes the fetched rows unchanged.
struct QueryRows {
    aggregate: Map<String, Value>,
    top: Vec<Map<String, Value>>,
    recent: Vec<Map<String, Value>>,
    deck_rows: Vec<Map<String, Value>>,
    ownership: Vec<Map<String, Value>>,
}

fn fetch_all(pool: &DbPool, user_id: i64) -> Result<QueryRows, String> {
    let now = now_ms();
    let cutoff7 = iso_utc_ms(now - 7 * DAY_MS);
    let cutoff30 = iso_utc_ms(now - 30 * DAY_MS);
    type Rows = Result<Vec<Map<String, Value>>, String>;
    let (agg, top, recent, decks_own) = std::thread::scope(|s| {
        let h_agg: std::thread::ScopedJoinHandle<'_, Rows> = s.spawn(|| {
            let conn = pool.get().map_err(|e| format!("read pool: {e}"))?;
            all_rows(
                &conn,
                AGGREGATE_SQL,
                &[
                    SqlValue::Integer(user_id),
                    SqlValue::Text(cutoff7.clone()),
                    SqlValue::Text(cutoff30.clone()),
                    SqlValue::Text(cutoff7),
                    SqlValue::Text(cutoff30),
                ],
            )
        });
        let h_top: std::thread::ScopedJoinHandle<'_, Rows> = s.spawn(|| {
            let conn = pool.get().map_err(|e| format!("read pool: {e}"))?;
            all_rows(&conn, TOP_VALUABLE_SQL, &[SqlValue::Integer(user_id)])
        });
        let h_recent: std::thread::ScopedJoinHandle<'_, Rows> = s.spawn(|| {
            let conn = pool.get().map_err(|e| format!("read pool: {e}"))?;
            all_rows(&conn, RECENT_ADDED_SQL, &[SqlValue::Integer(user_id)])
        });
        let h_decks: std::thread::ScopedJoinHandle<'_, Result<(Vec<Map<String, Value>>, Vec<Map<String, Value>>), String>> = s.spawn(|| {
            let conn = pool.get().map_err(|e| format!("read pool: {e}"))?;
            let decks = all_rows(&conn, DECK_ROWS_SQL, &[SqlValue::Integer(user_id)])?;
            // Same gating as Node: ownership only for users who have decks.
            let own = if !decks.is_empty() {
                all_rows(&conn, OWNERSHIP_SQL, &[SqlValue::Integer(user_id), SqlValue::Integer(user_id)])?
            } else {
                Vec::new()
            };
            Ok((decks, own))
        });
        (
            h_agg.join().map_err(|_| "aggregate thread panicked".to_string()),
            h_top.join().map_err(|_| "topValuable thread panicked".to_string()),
            h_recent.join().map_err(|_| "recentAdditions thread panicked".to_string()),
            h_decks.join().map_err(|_| "decks thread panicked".to_string()),
        )
    });
    let (deck_rows, ownership) = decks_own??;
    let aggregate = agg??.into_iter().next().ok_or("aggregate query returned no rows")?;
    Ok(QueryRows { aggregate, top: top??, recent: recent??, deck_rows, ownership })
}

/// The response assembly from stats.js lines 167-313 over the fetched rows.
fn build_body(pool: &DbPool, user_id: i64) -> Result<Vec<u8>, String> {
    let q = fetch_all(pool, user_id)?;
    let aggregate = q.aggregate;

    let get_col = |k: &str| -> Value { aggregate.get(k).cloned().unwrap_or(Value::Null) };
    // Numbers the handler uses in comparisons; JS treats null as 0/false in
    // these `> 0` guards, and would throw (→ 500) on the toFixed paths.
    let num = |k: &str| -> Result<f64, String> {
        js_num_opt(&get_col(k)).ok_or_else(|| format!("aggregate.{k} is not numeric"))
    };

    let total_cards = get_col("total_cards");
    let unique_cards = get_col("unique_cards");
    let total_value = num("total_value")?;
    let total_spent = num("total_spent")?;
    let near_mint_count = get_col("near_mint_count");

    let types = js_parse_json_array(&get_col("types"), "types")?;
    let color_values = js_parse_json_array(&get_col("color_values"), "color_values")?;
    let rarities = js_parse_json_array(&get_col("rarities"), "rarities")?;
    let all_sets = js_parse_json_array(&get_col("sets"), "sets")?;

    let sets_arr = all_sets.as_array().ok_or("sets not an array")?;
    let mut vintage_count: f64 = 0.0;
    for set in sets_arr {
        let id = set.get("id").and_then(Value::as_str).unwrap_or("");
        if is_vintage_set(id) {
            // JS: sum + (isVintageSet(set.id) ? set.count : 0)
            vintage_count += js_to_num(set.get("count").unwrap_or(&Value::Null));
        }
    }

    // The change aggregates feed comparisons and subtraction only (no
    // method calls), so JS coercion applies: null → 0, non-numeric → NaN.
    let value7_ago = js_to_num(&get_col("value7_ago"));
    let value7_now = js_to_num(&get_col("value7_now"));
    let value30_ago = js_to_num(&get_col("value30_ago"));
    let value30_now = js_to_num(&get_col("value30_now"));

    let top_valuable: Vec<Value> = q.top.into_iter().map(|r| Value::Object(shape_price_row(r))).collect();

    // setProgress (stats.js 220-234): size truthy rows only, percent clamped,
    // stable sort by percent DESC then setId string ASC.
    let mut progress: Vec<Map<String, Value>> = Vec::new();
    for set in sets_arr {
        let size_v = set.get("size").cloned().unwrap_or(Value::Null);
        if !truthy(&size_v) {
            continue;
        }
        let owned = js_to_num(set.get("ownedUnique").unwrap_or(&Value::Null));
        let size = js_to_num(&size_v);
        let percent = js_math_min(js_math_round(owned / size * 100.0), 100.0);
        let mut p = Map::new();
        p.insert("setId".into(), set.get("id").cloned().unwrap_or(Value::Null));
        p.insert("setName".into(), set.get("name").cloned().unwrap_or(Value::Null));
        p.insert("ownedUnique".into(), set.get("ownedUnique").cloned().unwrap_or(Value::Null));
        p.insert("totalCards".into(), size_v);
        p.insert("percent".into(), json_num(percent));
        progress.push(p);
    }
    // Stable two-pass sort mirroring Node's single comparator
    // (percent DESC, then setId string ASC as the tie-breaker).
    progress.sort_by(|a, b| {
        let sa = a.get("setId").and_then(Value::as_str).unwrap_or("");
        let sb = b.get("setId").and_then(Value::as_str).unwrap_or("");
        sa.cmp(sb)
    });
    progress.sort_by(|a, b| {
        let pa = js_to_num(a.get("percent").unwrap_or(&Value::Null));
        let pb = js_to_num(b.get("percent").unwrap_or(&Value::Null));
        pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
    });
    progress.truncate(4);

    let tc = js_to_num(&total_cards);
    let mint_rate = if tc > 0.0 {
        js_fixed_parsed(js_to_num(&near_mint_count) / tc * 100.0, 1)
    } else {
        json_num(0.0)
    };
    let vintage_ratio = if tc > 0.0 { js_fixed_parsed(vintage_count / tc * 100.0, 1) } else { json_num(0.0) };

    let recent_additions: Vec<Value> = q.recent.into_iter().map(|r| Value::Object(shape_price_row(r))).collect();

    let deck_rows = q.deck_rows;
    let mut ownership: HashMap<i64, f64> = HashMap::new();
    if !deck_rows.is_empty() {
        for r in q.ownership {
            let id = js_to_num(r.get("deck_id").unwrap_or(&Value::Null));
            if !id.is_finite() {
                continue; // Number(undefined-ish) → never matches a deck id
            }
            // Number(r.missing_copies) || 0
            let missing = {
                let n = js_to_num(r.get("missing_copies").unwrap_or(&Value::Null));
                if n == 0.0 || n.is_nan() {
                    0.0
                } else {
                    n
                }
            };
            ownership.insert(id as i64, missing);
        }
    }
    let built = deck_rows.iter().filter(|d| truthy(d.get("checked_out").unwrap_or(&Value::Null))).count();
    let need_cards = deck_rows
        .iter()
        .filter(|d| {
            !truthy(d.get("checked_out").unwrap_or(&Value::Null))
                && *ownership
                    .get(&{
                        let n = js_to_num(d.get("id").unwrap_or(&Value::Null));
                        if n.is_finite() { n as i64 } else { i64::MIN }
                    })
                    .unwrap_or(&0.0)
                    > 0.0
        })
        .count();

    // summary (stats.js 273-299) — key order is part of the byte contract.
    let gain_abs = total_value - total_spent;
    let mut roi = Map::new();
    roi.insert("abs".into(), js_fixed_parsed(gain_abs, 2));
    roi.insert("pct".into(), if total_spent > 0.0 { js_fixed_parsed(gain_abs / total_spent * 100.0, 1) } else { Value::Null });
    let avg_card_value = if tc > 0.0 { js_fixed_parsed(total_value / tc, 2) } else { json_num(0.0) };

    let dup = {
        let a = js_to_num(&total_cards);
        let b = js_to_num(&unique_cards);
        if a.is_nan() || b.is_nan() {
            Value::Null // Math.max(NaN, 0) === NaN → null in JSON
        } else {
            json_num((a - b).max(0.0))
        }
    };

    let change = |ago: f64, now_v: f64| -> Value {
        if ago > 0.0 {
            let mut c = Map::new();
            c.insert("available".into(), Value::Bool(true));
            c.insert("abs".into(), js_fixed_parsed(now_v - ago, 2));
            c.insert("pct".into(), js_fixed_parsed((now_v - ago) / ago * 100.0, 1));
            Value::Object(c)
        } else {
            let mut c = Map::new();
            c.insert("available".into(), Value::Bool(false));
            c.insert("abs".into(), Value::Null);
            c.insert("pct".into(), Value::Null);
            Value::Object(c)
        }
    };

    let mut summary = Map::new();
    summary.insert("totalCards".into(), total_cards);
    summary.insert("uniqueCards".into(), unique_cards);
    summary.insert("totalValue".into(), js_fixed_parsed(total_value, 2));
    summary.insert("estimatedCards".into(), get_col("estimated_cards"));
    summary.insert("totalSpent".into(), js_fixed_parsed(total_spent, 2));
    summary.insert("roi".into(), Value::Object(roi));
    summary.insert("avgCardValue".into(), avg_card_value);
    summary.insert("duplicateCopies".into(), dup);
    summary.insert("mintRate".into(), mint_rate);
    summary.insert("vintageRatio".into(), vintage_ratio);
    summary.insert("change7d".into(), change(value7_ago, value7_now));
    summary.insert("change30d".into(), change(value30_ago, value30_now));
    for k in ["change1y", "change5y"] {
        let mut c = Map::new();
        c.insert("available".into(), Value::Bool(false));
        c.insert("abs".into(), Value::Null);
        c.insert("pct".into(), Value::Null);
        summary.insert(k.into(), Value::Object(c));
    }

    // sets top-8 (stats.js 303-308): value DESC, stable (Array.sort is stable
    // in V8 since ES2019; Rust's sort_by is stable too — input order is the
    // json_group_array's first_order sequence on both sides).
    let mut top_sets: Vec<Map<String, Value>> = Vec::new();
    for set in sets_arr {
        let mut s = Map::new();
        s.insert("id".into(), set.get("id").cloned().unwrap_or(Value::Null));
        s.insert("name".into(), set.get("name").cloned().unwrap_or(Value::Null));
        s.insert("count".into(), set.get("count").cloned().unwrap_or(Value::Null));
        let raw = js_num_opt(set.get("value").unwrap_or(&Value::Null)).ok_or("set.value is not numeric")?;
        s.insert("value".into(), js_fixed_parsed(raw, 2));
        top_sets.push(s);
    }
    top_sets.sort_by(|a, b| {
        let av = js_num_opt(a.get("value").unwrap_or(&Value::Null)).unwrap_or(f64::NAN);
        let bv = js_num_opt(b.get("value").unwrap_or(&Value::Null)).unwrap_or(f64::NAN);
        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
    });
    top_sets.truncate(8);

    let mut decks = Map::new();
    decks.insert("count".into(), Value::from(deck_rows.len() as i64));
    decks.insert("built".into(), Value::from(built as i64));
    decks.insert("needCards".into(), Value::from(need_cards as i64));

    let mut body = Map::new();
    body.insert("summary".into(), Value::Object(summary));
    body.insert("types".into(), types);
    body.insert("colorValues".into(), color_values);
    body.insert("rarities".into(), rarities);
    body.insert("sets".into(), Value::Array(top_sets.into_iter().map(Value::Object).collect()));
    body.insert("topValuable".into(), Value::Array(top_valuable));
    body.insert("recentAdditions".into(), Value::Array(recent_additions));
    body.insert("setProgress".into(), Value::Array(progress.into_iter().map(Value::Object).collect()));
    body.insert("decks".into(), Value::Object(decks));

    serde_json::to_vec(&Value::Object(body)).map_err(|e| format!("serialize: {e}"))
}

pub async fn handle(method: Method, State(st): State<AppState>, req: Request) -> Response {
    // Express answers GET (and HEAD) on exactly '/stats' — the handler reads
    // no query parameters (the cache name is the constant 'stats'). Anything
    // else this mount caught — HEAD, POST, '/stats/', the /stats/* sub-routes
    // — goes to Node untouched; a proxied response is byte-perfect by
    // construction.
    if method != Method::GET || req.uri().path() != "/api/stats" || !st.owns("stats") {
        return crate::proxy(State(st.clone()), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st.clone()), req).await;
    };
    let headers = req.headers();

    // authenticateToken runs before the route in Node (Bearer session token;
    // API keys are GET-only, which this path is). Same 401/403 bodies. The
    // lookup is ~5 us of cached SQLite — run inline on the worker, no
    // blocking-pool hop (iter 6: that hop, not the SQL, was the hot-path cost).
    let token = crate::auth::bearer_token(headers).map(|s| s.to_string());
    let user = match crate::auth::authenticate(&pool, token.as_deref(), true) {
        Ok(u) => u,
        Err(res) => return res,
    };

    let inm = headers.get("if-none-match").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).map(|s| s.to_string());

    let compute_pool = Arc::clone(&pool);
    let cache = Arc::clone(&st.stats_cache);
    let user_id = user.id;
    // Phase 1: resolve the cache key and probe. Node computes (gen, rev) at
    // request start; a bump racing the computation lands on the NEXT
    // request's key, exactly like Node's cached() wrapper.
    let outcome = async move {
        let (hit, key) = {
            let conn = pool
                .get()
                .map_err(|e| format!("read pool: {e}"))?;
            let gen = cache.touch(user_id);
            let rev = StatsCache::db_revision(&conn);
            let key = StatsCache::key_for(user_id, gen, rev.as_deref());
            (cache.get(&key), key)
        };
        if let Some(entry) = hit {
            return Ok((true, entry));
        }
        // Phase 2: coalesced regeneration — every concurrent miss on this
        // key awaits ONE build_body via the shared OnceCell slot.
        //
        // Stale-while-revalidate (iter 12): if the user has a body computed
        // within STALE_GRACE (the common case — a write just bumped the rev
        // counter), serve it immediately and rebuild this key in the
        // background. The ~0.6 s dashboard aggregate never lands on a
        // request's critical path again until the grace window expires or
        // the cache is cold, at which point we block and compute exactly as
        // before. Nobody observes data older than Node itself would have
        // spent computing it; steady-state (no writes) bodies are unchanged
        // and byte-identical, and the X-Stats-Cache header keeps Node's
        // answer for this instant ('miss' — Node had no entry either).
        let slot = cache.slot(&key);
        let init_cache = Arc::clone(&cache);
        let init_pool = Arc::clone(&compute_pool);
        let init_key = key.clone();
        let init = move || async move {
            let c = Arc::clone(&init_cache);
            let p = Arc::clone(&init_pool);
            let k = init_key.clone();
            let r = tokio::task::spawn_blocking(move || compute_and_store(&c, &p, user_id, k))
                .await
                .map_err(|e| format!("stats task panicked: {e}"))?;
            Ok(r)
        };
        if let Some(stale) = cache.stale_for(user_id) {
            let bg_slot = Arc::clone(&slot);
            let bg_cache = Arc::clone(&cache);
            let bg_key = key.clone();
            tokio::spawn(async move {
                match bg_slot.get_or_try_init(init).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => tracing::error!(error = %e, "stats background build failed"),
                    Err(e) => tracing::error!(error = %e, "stats background build failed"),
                }
                bg_cache.finish(&bg_key);
            });
            return Ok((false, stale));
        }
        let res: Result<&Result<Arc<StatsEntry>, String>, String> = slot.get_or_try_init(init).await;
        cache.finish(&key);
        // get_or_try_init nests: outer Err carries a clone of the init error
        // (E: Clone), outer Ok wraps the stored Result<Arc<StatsEntry>, String>.
        let entry = match res {
            Ok(Ok(e)) => e.clone(),
            Ok(Err(e)) => return Err(e.clone()),
            Err(e) => return Err(e),
        };
        Ok((false, entry))
    }
    .await;

    let (hit, entry) = match outcome {
        Ok(v) => v,
        Err(e) => {
            // stats.js catch: console.error + 500 { error: 'Failed to compute statistics' }
            tracing::error!(error = %e, "stats computation failed");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to compute statistics");
        }
    };

    let cache_header: HeaderValue =
        HeaderValue::from_static(if hit { "hit" } else { "miss" });

    // Express: res.json stamps the ETag, then the freshness check short-
    // circuits to 304 (X-Stats-Cache stays on the response — Node sets it
    // before res.json runs).
    if let Some(tag) = inm.as_deref() {
        if etag_matches(Some(tag), &entry.etag) {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, entry.etag.as_str())
                .header("x-stats-cache", cache_header)
                .body(Body::empty())
                .unwrap();
        }
    }

    let raw_len = entry.raw.len();
    let use_gzip = wants_encoding(ae.as_deref(), "gzip", raw_len) && entry.gz.is_some();
    let body_bytes: Bytes = if use_gzip {
        entry.gz.clone().unwrap()
    } else {
        entry.raw.clone()
    };

    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::ETAG, entry.etag.as_str())
        .header("x-stats-cache", cache_header);
    if use_gzip {
        resp = resp.header(header::CONTENT_ENCODING, "gzip").header(header::VARY, "Accept-Encoding");
    }
    resp.body(Body::from(body_bytes)).unwrap()
}

// ---------------------------------------------------------------------------
// Unit tests for the JS-numeric core. Every expected value below was
// GENERATED with `~/.hermes/node/bin/node -e` (V8), never computed by hand:
//
//   (0.125).toFixed(2)   === "0.13"   (exact tie → away from zero)
//   (-0.125).toFixed(2)  === "-0.13"
//   (0.375).toFixed(2)   === "0.38"
//   (1.005).toFixed(2)   === "1.00"   (binary 1.00499…, not a tie)
//   (2.675).toFixed(2)   === "2.67"   (binary 2.67499…)
//   (8.005).toFixed(2)   === "8.01"   (binary 8.00500…044)
//   (-2.675).toFixed(2)  === "-2.67"
//   (-1.005).toFixed(2)  === "-1.00"
//   (0.145).toFixed(2)   === "0.14"   (binary 0.14499…)
//   (9.495).toFixed(2)   === "9.49"
//   (0.005).toFixed(2)   === "0.01"
//   (0.015).toFixed(2)   === "0.01"
//   (0.025).toFixed(2)   === "0.03"
//   (21.0).toFixed(2)    === "21.00"  parseFloat → JSON "21"
//   (-0.001).toFixed(2)  === "-0.00"  parseFloat → -0 → JSON "0"
//   (-0.0).toFixed(2)    === "0.00"
//   (123.456).toFixed(2) === "123.46" (-123.456 → "-123.46")
//   (28074.695).toFixed(2) === "28074.69"
//   (0.25).toFixed(1)    === "0.3"    (exact tie → away)
//   (0.35).toFixed(1)    === "0.3"    (binary 0.3499…)
//   (0.45).toFixed(1)    === "0.5"
//   (1.15).toFixed(1)    === "1.1"
//   (99.75).toFixed(1)   === "99.8"
//   (0.05).toFixed(1)    === "0.1"    (-0.05 → "-0.1")
//   (18.35).toFixed(1)   === "18.4"
//   (99.85).toFixed(1)   === "99.8"
//   (-0.04).toFixed(1)   === "-0.0"   parseFloat → -0 → JSON "0"
//   (0.5).toFixed(0)     === "1"      (-0.5 → "-1")
//   (2.5).toFixed(0)     === "3"      (-2.5 → "-3")
//   (3.5).toFixed(0)     === "4"; (1.5).toFixed(0) === "2"; (-1.5) → "-2"
//   Math.round(0.5) === 1; Math.round(-0.5) === -0 (JSON "0");
//   Math.round(2.5) === 3; Math.round(-2.5) === -2;
//   Math.round(0.49999999999999994) === 0 (floor(x+0.5) would give 1);
//   (1/3)*100 → 33; (2/3)*100 → 67; 12.5 → 13; -12.5 → -12;
//   (1/8)*100 → 13; 99.5 → 100; 98.5 → 99; Math.round(-0.4) === -0.
//   JSON.stringify(-0) === "0"; JSON.stringify(21.0) === "21".
// ---------------------------------------------------------------------------

#[cfg(test)]
mod js_number_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_fixed_matches_v8_vectors() {
        let cases: &[(f64, usize, &str)] = &[
            (0.125, 2, "0.13"),
            (-0.125, 2, "-0.13"),
            (0.375, 2, "0.38"),
            (1.005, 2, "1.00"),
            (2.675, 2, "2.67"),
            (8.005, 2, "8.01"),
            (-2.675, 2, "-2.67"),
            (-1.005, 2, "-1.00"),
            (0.145, 2, "0.14"),
            (9.495, 2, "9.49"),
            (0.005, 2, "0.01"),
            (0.015, 2, "0.01"),
            (0.025, 2, "0.03"),
            (21.0, 2, "21.00"),
            (-0.001, 2, "-0.00"),
            (-0.0, 2, "0.00"),
            (0.0, 2, "0.00"),
            (123.456, 2, "123.46"),
            (-123.456, 2, "-123.46"),
            (28074.695, 2, "28074.69"),
            (1.0000000000000002, 2, "1.00"),
            (-0.145, 2, "-0.14"),
            (0.25, 1, "0.3"),
            (0.35, 1, "0.3"),
            (0.45, 1, "0.5"),
            (1.15, 1, "1.1"),
            (99.75, 1, "99.8"),
            (0.05, 1, "0.1"),
            (-0.05, 1, "-0.1"),
            (18.35, 1, "18.4"),
            (99.85, 1, "99.8"),
            (-0.04, 1, "-0.0"),
            (0.5, 0, "1"),
            (-0.5, 0, "-1"),
            (2.5, 0, "3"),
            (-2.5, 0, "-3"),
            (3.5, 0, "4"),
            (1.5, 0, "2"),
            (-1.5, 0, "-2"),
        ];
        for (x, n, want) in cases {
            assert_eq!(&js_to_fixed(*x, *n), want, "({x}).toFixed({n})");
        }
    }

    #[test]
    fn to_fixed_carry_cases() {
        assert_eq!(js_to_fixed(99.999, 2), "100.00");
        assert_eq!(js_to_fixed(0.999, 2), "1.00");
        assert_eq!(js_to_fixed(-9.9999, 2), "-10.00");
        assert_eq!(js_to_fixed(9.5, 0), "10");
        assert_eq!(js_to_fixed(-0.4, 0), "-0"); // sign kept, magnitude rounds to 0
    }

    #[test]
    fn fixed_parsed_values() {
        // parseFloat(x.toFixed(n)) end-to-end, then JSON via json_num.
        let v = |x: f64, n: usize| serde_json::to_string(&js_fixed_parsed(x, n)).unwrap();
        assert_eq!(v(1.005, 2), "1"); // "1.00" → 1 → integer → "1" (JS: JSON.stringify(parseFloat("1.00")) === "1")
        assert_eq!(v(-1.005, 2), "-1");
        assert_eq!(v(-0.001, 2), "0"); // "-0.00" → -0 → "0" (JSON.stringify(-0))
        assert_eq!(v(21.0, 2), "21");
        assert_eq!(v(2.675, 2), "2.67");
        assert_eq!(v(0.125, 2), "0.13");
        assert_eq!(v(28074.695, 2), "28074.69");
        assert_eq!(v(99.85, 1), "99.8");
    }

    #[test]
    fn math_round_half_toward_infinity() {
        assert_eq!(js_math_round(0.5), 1.0);
        assert_eq!(js_math_round(-0.5), 0.0);
        assert_eq!(js_math_round(2.5), 3.0);
        assert_eq!(js_math_round(-2.5), -2.0);
        assert_eq!(js_math_round(0.49999999999999994), 0.0);
        assert_eq!(js_math_round(1.0 / 3.0 * 100.0), 33.0);
        assert_eq!(js_math_round(2.0 / 3.0 * 100.0), 67.0);
        assert_eq!(js_math_round(12.5), 13.0);
        assert_eq!(js_math_round(-12.5), -12.0);
        assert_eq!(js_math_round(1.0 / 8.0 * 100.0), 13.0);
        assert_eq!(js_math_round(99.5), 100.0);
        assert_eq!(js_math_round(98.5), 99.0);
        assert_eq!(js_math_round(-0.4), 0.0);
    }

    #[test]
    fn math_min_propagates_nan_like_js() {
        assert!(js_math_min(f64::NAN, 100.0).is_nan());
        assert_eq!(js_math_min(150.0, 100.0), 100.0);
        assert_eq!(js_math_min(1.0, 100.0), 1.0);
    }

    #[test]
    fn integral_floats_serialize_without_fraction() {
        // The Node byte contract: JSON.stringify(21.0) === "21", and
        // non-integral values keep shortest round-trip digits.
        assert_eq!(serde_json::to_string(&json_num(21.0)).unwrap(), "21");
        assert_eq!(serde_json::to_string(&json_num(4.01)).unwrap(), "4.01");
        assert_eq!(serde_json::to_string(&json_num(-0.0)).unwrap(), "0");
        assert_eq!(serde_json::to_string(&json_num(0.0)).unwrap(), "0");
        assert_eq!(serde_json::to_string(&json_num(12.34)).unwrap(), "12.34");
        assert_eq!(serde_json::to_string(&json_num(36.13)).unwrap(), "36.13");
        // NaN → null, like JSON.stringify(NaN).
        assert_eq!(serde_json::to_string(&json_num(f64::NAN)).unwrap(), "null");
    }

    #[test]
    fn parse_sqlite_utc_formats() {
        // Z-suffixed ISO (the toISOString shape).
        assert_eq!(parse_sqlite_utc("2026-01-01T00:00:00.000Z"), Some(1_767_225_600_000.0));
        // SQLite space-separated naive → treated as UTC (parseSqliteUtc
        // appends 'Z' itself, so container TZ is irrelevant).
        assert_eq!(parse_sqlite_utc("2026-01-01 00:00:00"), Some(1_767_225_600_000.0));
        // Fractional seconds + offset suffix.
        assert_eq!(parse_sqlite_utc("2026-01-01T01:00:00.000+01:00"), Some(1_767_225_600_000.0));
        assert_eq!(parse_sqlite_utc("2026-01-01T00:00:00.500Z"), Some(1_767_225_600_500.0));
        // Falsy → invalid date (new Date(NaN)).
        assert_eq!(parse_sqlite_utc(""), None);
        // Garbage → invalid date.
        assert_eq!(parse_sqlite_utc("not a date"), None);
        assert_eq!(parse_sqlite_utc("2026-13-45T99:99:99Z"), None);
    }

    #[test]
    fn iso_utc_ms_round_trips_todate_isostring() {
        assert_eq!(iso_utc_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_utc_ms(1_767_225_600_000), "2026-01-01T00:00:00.000Z");
        assert_eq!(iso_utc_ms(1_767_225_600_123), "2026-01-01T00:00:00.123Z");
        // Leap-day and pre-epoch (Node: new Date(-1).toISOString()).
        assert_eq!(iso_utc_ms(1_709_172_000_000), "2024-02-29T02:00:00.000Z");
        assert_eq!(iso_utc_ms(-1), "1969-12-31T23:59:59.999Z");
        // Fixed offset sanity (new Date(Date.now() - 7*864e5) shape).
        assert_eq!(iso_utc_ms(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn normalized_json_parses_sqlite_float_integers() {
        // SQLite json emits 12.0 for integer REALs; Node's parse→stringify
        // cycle emits 12 — normalization reproduces Node's bytes.
        let v = js_parse_json_array(&Value::String(r#"[{"name":"X","value":12.0,"count":3.0}]"#.into()), "x").unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), r#"[{"name":"X","value":12,"count":3}]"#);
        // null / empty-string → the `|| '[]'` fallback.
        let v = js_parse_json_array(&Value::Null, "x").unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), "[]");
        let v = js_parse_json_array(&Value::String(String::new()), "x").unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), "[]");
        // Non-numeric value 24.68 survives unchanged (shortest round-trip).
        let v = js_parse_json_array(&Value::String(r#"[{"value":24.68}]"#.into()), "x").unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), r#"[{"value":24.68}]"#);
    }

    #[test]
    fn js_to_num_coercions() {
        assert_eq!(js_to_num(&Value::Null), 0.0);
        assert_eq!(js_to_num(&json!(3)), 3.0);
        assert_eq!(js_to_num(&json!(2.5)), 2.5);
        assert_eq!(js_to_num(&json!(true)), 1.0);
        assert_eq!(js_to_num(&json!("7.5")), 7.5);
        assert_eq!(js_to_num(&json!("")), 0.0);
        assert!(js_to_num(&json!("abc")).is_nan());
    }

    #[test]
    fn vintage_list_membership() {
        assert!(is_vintage_set("LEA"));
        assert!(is_vintage_set("2ed"));
        assert!(!is_vintage_set("null"));
        assert!(!is_vintage_set("m11"));
    }

    fn entry(tag: &str) -> Arc<StatsEntry> {
        Arc::new(StatsEntry { raw: Bytes::from(tag.to_string()), gz: None, etag: format!("W/\"{tag}\"") })
    }

    #[test]
    fn stale_grace_matches_owner_bound() {
        // Owner ruling (Brent, 2026-11-10): the stats SWR exception to
        // hard rule 1 is bounded at 5 s max staleness. Any other value is a
        // rule-1 violation; this test is the tripwire.
        assert_eq!(STALE_GRACE, Duration::from_secs(5));
    }

    #[test]
    fn stale_for_returns_the_newest_computed_body_after_a_key_change() {
        let cache = StatsCache::new();
        let e1 = entry("b1");
        cache.set(2, StatsCache::key_for(2, 0, Some("1.1.1")), Arc::clone(&e1));
        // A write bumps the rev counter → the NEXT request's key misses…
        assert!(cache.get(&StatsCache::key_for(2, 0, Some("1.2.1"))).is_none());
        // …but SWR still has the recent body to serve while it rebuilds.
        let stale = cache.stale_for(2).expect("recent body available");
        assert!(Arc::ptr_eq(&stale, &e1));
    }

    #[test]
    fn stale_for_is_none_when_cold_or_past_grace() {
        let cache = StatsCache::new();
        assert!(cache.stale_for(2).is_none(), "cold cache must block-and-compute");
        cache.set(2, StatsCache::key_for(2, 0, Some("1.1.1")), entry("b1"));
        assert!(cache.stale_for(2).is_some());
        // Age the newest body past the grace window: stale is no longer
        // servable (Node would never be staler than its own rebuild).
        cache.inner.lock().unwrap().latest.insert(
            2,
            (entry("b1"), Instant::now() - STALE_GRACE - Duration::from_secs(1)),
        );
        assert!(cache.stale_for(2).is_none());
    }

    #[test]
    fn stale_body_survives_store_eviction_within_grace() {
        // write-mix churn fills the store with dead rev keys; the newest
        // body must stay servable regardless (bounded by STALE_GRACE).
        let cache = StatsCache::new();
        let keep = entry("b1");
        cache.set(2, "u|0|old|stats".into(), Arc::clone(&keep));
        for i in 0..(MAX_ENTRIES + 10) {
            cache.set(2, format!("u|0|{i}|stats"), entry("filler"));
        }
        assert!(cache.get("u|0|old|stats").is_none(), "old key evicted from the store");
        // The last set replaced `latest` with filler — that is the entry SWR
        // serves (it is the newest COMPUTED body, matching the DB at its
        // compute time); the point is it is Some, not None.
        assert!(cache.stale_for(2).is_some());
    }
}

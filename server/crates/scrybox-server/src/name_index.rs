//! In-memory substring index over `card_cache` names, backing the native
//! `/api/search` name filter.
//!
//! Why: `localCacheQuery`'s hot predicate is `(name LIKE '%q%' OR printed_name
//! LIKE '%q%')` over all ~37k card_cache rows. With no usable index that is a
//! full table scan per distinct query; under the honest bench (52 rotating
//! names, every ~2 s write bumping `collection_read_revision` and invalidating
//! every cached body) the 16-connection read pool stays backlogged on those
//! scans. This index turns each regeneration into an in-memory substring
//! filter plus a handful of primary-key lookups.
//!
//! Correctness contract (checked exhaustively in `tests/search.rs`):
//! - `Snapshot::candidate_rowids(pat)` returns **exactly** the rowids SQLite's
//!   ASCII-case-insensitive `LIKE '%' || pat || '%'` matches against `name`
//!   or `printed_name`, for patterns containing no LIKE metacharacters
//!   (`%`, `_`). SQLite folds only ASCII case, which is what
//!   `to_ascii_lowercase` does. Even so, the route keeps the original LIKE
//!   predicate in the SQL — the candidate set only *narrows the scan*, it can
//!   never add or reorder rows — so a mismatch could at worst be a missing
//!   row, which the equality tests prove does not happen.
//! - A snapshot is only honoured when its build-time `collection_read_revision`
//!   equals the revision the caller reads in the *same* pinned read
//!   transaction. db.js attaches unconditional AFTER INSERT/UPDATE/DELETE
//!   triggers on card_cache bumping that counter, so equal revision ⟺ the
//!   rows the snapshot scanned are exactly the rows the query can see. A
//!   snapshot for any other revision is `None` → plain scan.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, Mutex, RwLock,
    },
};

use rusqlite::Connection;
use serde_json::Value;

use crate::db::{self, DbPool};
use crate::poison::{self, LockExt};

/// Wrap a `BEGIN` so an early `?`/panic still issues `ROLLBACK` before the
/// pooled connection goes back to the pool.
pub(crate) struct Txn<'a> {
    conn: &'a Connection,
    done: bool,
}

impl<'a> Txn<'a> {
    pub(crate) fn begin(conn: &'a Connection) -> rusqlite::Result<Self> {
        conn.execute_batch("BEGIN")?;
        Ok(Self { conn, done: false })
    }
    pub(crate) fn commit(mut self) -> rusqlite::Result<()> {
        self.conn.execute_batch("COMMIT")?;
        self.done = true;
        Ok(())
    }
}

impl Drop for Txn<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

/// One row's indexed columns, pre-lowercased.
struct Entry {
    rowid: i64,
    name: String,
    printed: String,
}

/// A point-in-time scan of every card_cache row.
pub struct Snapshot {
    entries: Vec<Entry>,
}

impl Snapshot {
    /// Rowids whose `name` or `printed_name` contains `pat` (caller must pass
    /// it `to_ascii_lowercase`d; stored haystacks are pre-lowercased).
    /// Rowids come out in ascending order (the scan order), which is the
    /// order a plain `SCAN card_cache` would return rows in.
    pub fn candidate_rowids(&self, pat: &str) -> Vec<i64> {
        if pat.is_empty() {
            return Vec::new();
        }
        self.entries
            .iter()
            .filter(|e| e.name.contains(pat) || e.printed.contains(pat))
            .map(|e| e.rowid)
            .collect()
    }
}

/// Shared index with single-flight rebuilds.
pub struct NameIndex {
    snapshot: RwLock<Option<Arc<Snapshot>>>,
    /// Serialize rebuilds so an invalidation storm builds once, not once per
    /// in-flight request.
    build: Mutex<()>,
    /// Revision the current snapshot reflects; never matches while disabled
    /// or before the first successful build.
    rev: AtomicI64,
    disabled: bool,
}

impl NameIndex {
    pub fn new() -> Self {
        Self {
            snapshot: RwLock::new(None),
            build: Mutex::new(()),
            rev: AtomicI64::new(i64::MIN),
            disabled: false,
        }
    }

    /// Test hook: an index that never serves a snapshot (forces the plain
    /// scan path through the rest of the route for A/B comparison).
    pub fn disabled() -> Self {
        Self { disabled: true, ..Self::new() }
    }

    /// The cached snapshot, but only when it exactly reflects `rev`.
    pub fn snapshot_for(&self, rev: i64) -> Option<Arc<Snapshot>> {
        if self.rev.load(Ordering::Acquire) != rev {
            return None;
        }
        self.snapshot.guard().clone()
    }

    /// (Re)build so a snapshot for `rev` exists (~30 ms for 37k rows);
    /// failures only log — callers keep the plain scan path, so this can
    /// never break correctness.
    ///
    /// Non-blocking under contention: a background writer (Node's sidecar
    /// cache upserts) can advance the revision ~30×/s, so build attempts
    /// overlap constantly. Readers must never queue behind a builder — the
    /// plain 16 ms scan beats a 40 ms mutex wait — so a second concurrent
    /// attempt returns immediately instead of blocking. Single-flight is
    /// still exact: at most one builder is ever in flight.
    pub fn ensure_fresh(&self, pool: &DbPool, rev: i64) {
        if self.disabled || self.rev.load(Ordering::Acquire) == rev {
            return;
        }
        let Ok(_guard) = self.build.try_lock() else { return };
        if self.rev.load(Ordering::Acquire) == rev {
            return;
        }
        match self.build_snapshot(pool, rev) {
            Ok(snap) => {
                *poison::write(&self.snapshot) = Some(Arc::new(snap));
                self.rev.store(rev, Ordering::Release);
            }
            Err(e) => tracing::debug!(error = %e, rev, "name index rebuild skipped"),
        }
    }

    fn build_snapshot(&self, pool: &DbPool, want_rev: i64) -> Result<Snapshot, String> {
        let started = std::time::Instant::now();
        let conn = pool.get().map_err(|e| e.to_string())?;
        // The revision probe is the transaction's first read, so WAL pins the
        // whole snapshot to the state where revision == want_rev; the trigger
        // invariant (every card_cache write bumps the counter atomically with
        // the write) makes that state unique.
        let txn = Txn::begin(&conn).map_err(|e| e.to_string())?;
        let rev_now = db::current_revision(&conn).map_err(|e| e.to_string())?;
        if rev_now != Some(want_rev) {
            return Err(format!("revision moved during build (wanted {want_rev}, saw {rev_now:?})"));
        }
        let mut stmt = conn
            .prepare_cached("SELECT rowid, name, printed_name FROM card_cache")
            .map_err(|e| e.to_string())?;
        let entries = stmt
            .query_map([], |r| {
                let rowid: i64 = r.get(0)?;
                let name: String = r
                    .get::<_, Option<String>>(1)?
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let printed: String = r
                    .get::<_, Option<String>>(2)?
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                Ok(Entry { rowid, name, printed })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        drop(stmt);
        txn.commit().map_err(|e| e.to_string())?;
        tracing::debug!(rows = entries.len(), rev = want_rev, ms = started.elapsed().as_millis(), "name index rebuilt");
        Ok(Snapshot { entries })
    }
}

impl Default for NameIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// A name pattern this index may serve: non-empty, no LIKE metacharacters
/// (`%`, `_` — `_` matches any single character in LIKE, which a substring
/// filter cannot emulate), returned ASCII-lowercased. `None` ⇒ plain scan.
pub fn indexable_pattern(q: &str) -> Option<String> {
    if q.is_empty() || q.contains(['%', '_']) {
        return None;
    }
    Some(q.to_ascii_lowercase())
}

// ---------------------------------------------------------------------------
// Per-user owned-qty snapshot
// ---------------------------------------------------------------------------

/// SQLite's `TRIM(X)` strips **spaces only** (unlike Rust's `trim`), and
/// `LOWER(X)` folds ASCII only — mirror both for the card-side key. (The
/// owned side of the join is built by SQL itself, so only the target side
/// needs this.)
fn sql_lower_trim(s: &str) -> String {
    s.trim_matches(' ').to_ascii_lowercase()
}

/// One user's owned-card key→qty aggregate, materialized once per revision
/// instead of once per search regeneration. `attachOwnedQty`'s owned CTE
/// re-scans/re-groups the user's entire collection (~25k rows on the bench
/// DB ⇒ ~106 ms) on every cache miss; under the write-mix bench that
/// dominated regeneration. Same SQL predicates as the CTE, so the map is
/// row-equivalent to the LEFT JOIN it replaces (one row per key by GROUP BY).
pub struct OwnedCache {
    slots: RwLock<HashMap<i64, Arc<OwnedSlot>>>,
    build: Mutex<()>,
    disabled: bool,
}

struct OwnedSlot {
    rev: i64,
    qty_by_key: Arc<HashMap<String, i64>>,
}

impl Default for OwnedCache {
    fn default() -> Self {
        Self::new()
    }
}

impl OwnedCache {
    pub fn new() -> Self {
        Self { slots: RwLock::new(HashMap::new()), build: Mutex::new(()), disabled: false }
    }

    /// Test hook: never serves a snapshot (forces the SQL attach path for
    /// A/B comparison against the in-memory attach).
    pub fn disabled() -> Self {
        Self { disabled: true, ..Self::new() }
    }

    /// The user's owned map when it reflects `rev`; builds single-flight when
    /// missing. `None` ⇒ caller must run the original SQL attach (cold build
    /// raced a write, the aggregate didn't come back as integers, or another
    /// builder is already in flight — same non-blocking rationale as
    /// `NameIndex::ensure_fresh`).
    pub fn owned_map(&self, pool: &DbPool, user_id: i64, rev: i64) -> Option<Arc<HashMap<String, i64>>> {
        if self.disabled {
            return None;
        }
        if let Some(slot) = self.slots.guard().get(&user_id).cloned() {
            if slot.rev == rev {
                return Some(slot.qty_by_key.clone());
            }
        }
        let Ok(_guard) = self.build.try_lock() else { return None };
        // Re-check: a parallel request may have built it while we waited.
        if let Some(slot) = self.slots.guard().get(&user_id).cloned() {
            if slot.rev == rev {
                return Some(slot.qty_by_key.clone());
            }
        }
        match self.build_slot(pool, user_id, rev) {
            Ok(slot) => {
                let map = slot.qty_by_key.clone();
                poison::write(&self.slots).insert(user_id, Arc::new(slot));
                Some(map)
            }
            Err(e) => {
                tracing::debug!(error = %e, user_id, rev, "owned snapshot skipped");
                None
            }
        }
    }

    fn build_slot(&self, pool: &DbPool, user_id: i64, want_rev: i64) -> Result<OwnedSlot, String> {
        let conn = pool.get().map_err(|e| e.to_string())?;
        let txn = Txn::begin(&conn).map_err(|e| e.to_string())?;
        // Same pinned-snapshot discipline as the name index: probe first,
        // every write to collection/card_cache bumps the counter atomically.
        let now = db::current_revision(&conn).map_err(|e| e.to_string())?;
        if now != Some(want_rev) {
            return Err(format!("revision moved during owned build (wanted {want_rev}, saw {now:?})"));
        }
        let mut stmt = conn
            .prepare_cached(
                "SELECT LOWER(TRIM(COALESCE(owned_cc.name, ''))) AS card_key, SUM(c.quantity) AS qty
                 FROM collection c
                 JOIN card_cache owned_cc ON owned_cc.id = c.card_id
                 WHERE c.user_id = ?1 AND c.quantity > 0
                 GROUP BY LOWER(TRIM(COALESCE(owned_cc.name, '')))",
            )
            .map_err(|e| e.to_string())?;
        let mut qty_by_key = HashMap::new();
        let mut rows = stmt
            .query_map([user_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| e.to_string())?;
        while let Some(row) = rows.next() {
            let (k, q) = row.map_err(|e| e.to_string())?;
            qty_by_key.insert(k, q);
        }
        drop(rows);
        drop(stmt);
        txn.commit().map_err(|e| e.to_string())?;
        Ok(OwnedSlot { rev: want_rev, qty_by_key: Arc::new(qty_by_key) })
    }
}

/// Attach owned quantities from the in-memory snapshot, replicating the
/// LEFT JOIN of `attach_owned_qty` exactly: the target key is
/// `LOWER(TRIM(cached_target.name))` (the card_cache row for the target's
/// `card_id || id` — always the card's own row on this route, since rows
/// come from `SELECT * FROM card_cache`) coalesced with
/// `LOWER(TRIM(target.name))` (same value); empty key ⇒ no match ⇒ 0.
/// Returns false when a row deviates from that shape (e.g. carries a
/// foreign `card_id`), so the caller can fall back to the SQL attach.
pub fn attach_owned_from_snapshot(cards: &mut [Value], map: &HashMap<String, i64>) -> bool {
    for card in cards.iter_mut() {
        let Some(obj) = card.as_object() else { return false };
        // targets: card_id = card.card_id || card.id (JS ||), then the
        // card_cache row for it; only id-as-key is the shape this route
        // produces, everything else falls back to SQL.
        if let Some(cid) = obj.get("card_id").filter(|v| truthy_card(v)) {
            if cid.as_str() != obj.get("id").and_then(Value::as_str) {
                return false;
            }
        }
        let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
        let key = sql_lower_trim(name);
        let qty: i64 = if key.is_empty() { 0 } else { map.get(&key).copied().unwrap_or(0) };
        let Some(obj) = card.as_object_mut() else { return false };
        obj.insert("owned_qty".to_string(), Value::from(qty));
    }
    true
}

fn truthy_card(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

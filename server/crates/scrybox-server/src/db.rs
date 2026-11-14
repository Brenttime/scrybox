//! SQLite connection pool shared by the ported Rust routes.
//!
//! Pooled connections are READERS AND WRITERS: Rust owns many write routes
//! (collection/deck/list writes, settings, auth sessions, backups, price
//! sweeps, migrations) and Node may still write the same DB file for
//! unported routes. Independent connections let SQLite serve reads
//! concurrently out of WAL mode; writers serialize on SQLite's lock (and the
//! route-level locks). Do NOT open the pool READ_ONLY / `query_only`.
//! `foreign_keys` is OFF on pooled connections, so any ON DELETE CASCADE
//! Node relies on must be performed explicitly by the Rust write path. The PRAGMAs mirror
//! `backend/src/db.js` so readers get the same page-cache/mmap behaviour
//! Node configured.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, PoisonError,
    },
};

use rusqlite::Connection;
use crate::poison::LockExt;

/// Per-pool SQLite tuning (Gate D final-phase DB tuning).
///
/// `cache_kb` is the PER-CONNECTION page-cache budget in KiB (negative
/// PRAGMA cache_size semantics). The pool applies a GLOBAL budget
/// (`SCRYBOX_DB_CACHE_BUDGET_MB`, default 16 MB — the a2full measured
/// config; 64 MB was Node's single-connection value, kept as an explicit
/// override) divided across `live_max`
/// connections, not `max`: a burst past the idle target opens connections
/// up to `live_max` (default 2*max), and when the budget was split by
/// `max` the worst case was live_max × (budget/max) = 2× the "global"
/// budget (review/MEMORY.md #7: POOL_MAX=4 + budget 16 MB was part of the
/// 500→360 MB drop; the 2× burst leak is what made the cap nominal).
/// Splitting by `live_max` keeps the whole pool inside the budget at any
/// checkout level (subject only to the 2 MB/conn floor). `mmap_kb` (default 4 MB, was 256 MB then
/// 32 MB) is the per-connection mmap window; the DB copy is 256 MB and
/// the hot working set is far smaller, so a small window caps how much
/// file-backed page cache can be faulted in per reader. 0 disables mmap
/// entirely (SCRYBOX_DB_MMAP_MB=0 is honoured, not filtered to the default).
#[derive(Clone, Copy, Debug)]
pub struct PoolConfig {
    pub max: usize,
    pub cache_kb: i64,
    pub mmap_kb: i64,
    /// Hard cap on LIVE (checked-out) connections — SCRYBOX_DB_LIVE_MAX.
    /// `max` only bounds how many connections are kept IDLE, so `get()`
    /// opened a fresh connection on every checkout past the idle list: an
    /// unbounded burst (queued writers, build fan-out) opened one SQLite
    /// connection per request, each with its own page cache + mmap window
    /// (REVIEW-concurrency H3 / REVIEW-writes M3 memory lever). Default
    /// 2*max: headroom above the idle target so the multi-connection
    /// consumers (collection parallel build holds 2, capped by the `builds`
    /// semaphore) can never deadlock against single-connection waiters.
    /// Clamped to >= max so the warm() idle target stays reachable.
    pub live_max: usize,
}

impl PoolConfig {
    /// Env-tunable defaults: SCRYBOX_DB_POOL_MAX, SCRYBOX_DB_CACHE_BUDGET_MB
    /// (global, split across live_max connections), SCRYBOX_DB_MMAP_MB (per conn),
    /// SCRYBOX_DB_LIVE_MAX (live checkout cap, default 2*max).
    pub fn from_env(default_max: usize) -> Self {
        let env_i64 = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<i64>().ok()).filter(|v| *v > 0);
        let max = env_i64("SCRYBOX_DB_POOL_MAX").unwrap_or(default_max as i64).max(1) as usize;
        let live_max = env_i64("SCRYBOX_DB_LIVE_MAX")
            .unwrap_or(max as i64 * 2)
            .max(max as i64)
            .max(1) as usize;
        // MEMORY.md #7: default budget 16 MB (a2full measured: POOL_MAX=4 +
        // budget 16 was part of the 500→360 drop) split across live_max, so
        // even a full burst (live_max live connections, each carrying its
        // own page cache) cannot exceed the global budget — splitting by
        // `max` let the 2×max burst reach 2× the budget. The 2 MB/conn
        // floor can exceed the budget on wide pools; SQLite would clamp
        // sub-page caches anyway, and wide pools are an operator choice.
        let budget_mb = env_i64("SCRYBOX_DB_CACHE_BUDGET_MB").unwrap_or(16);
        let cache_kb = (budget_mb * 1024 / live_max as i64).max(2048); // never below 2 MB/conn
        // SCRYBOX_DB_MMAP_MB accepts 0 (mmap OFF). The shared env_i64 above
        // filters `> 0`, which silently turned `0` into the default — a
        // deliberate "disable mmap" setting was a no-op (review/MEMORY.md #3).
        // 0 is a legal value here only: each pooled connection maps its own
        // window of the same file, so N × 32 MB windows dominated RssFile
        // (111 MB measured); MMAP_MB=1 cut it to 17 MB with no measured
        // speed loss. Default 4 MB (owner-ruling bound 0-4 MB).
        let mmap_mb = std::env::var("SCRYBOX_DB_MMAP_MB")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v >= 0)
            .unwrap_or(4);
        let mmap_kb = mmap_mb * 1024;
        Self { max, cache_kb, mmap_kb, live_max }
    }
}

/// Pool of ready-to-use read connections. Idle connections are kept up to
/// `max`; checkouts beyond the idle list open fresh connections only while
/// fewer than `live_max` are LIVE — past that, `get()` waits on a Condvar
/// for a returned connection (`try_get()` takes the same path without
/// waiting). Total open connections therefore never exceed `live_max`.
pub struct DbPool {
    path: PathBuf,
    /// `idle` holds returned connections; `live` counts checked-out ones.
    /// One mutex for both so a checkout (idle pop + live++) and a return
    /// (idle push + live--) are atomic — the invariant `live + idle <=
    /// live_max` is asserted only at these two transitions.
    state: Mutex<PoolState>,
    /// Signalled on every connection return (put) and failed open.
    live_cv: std::sync::Condvar,
    max: usize,
    live_max: usize,
    cfg: PoolConfig,
    /// M5: number of `get_async()` calls that found no idle connection and
    /// had to hop to the blocking pool (observability + the cap tests).
    blocking_fallbacks: AtomicUsize,
}

struct PoolState {
    idle: Vec<Connection>,
    live: usize,
}

impl DbPool {
    pub fn open(path: impl Into<PathBuf>, max: usize) -> rusqlite::Result<Self> {
        let max = max.max(1);
        Self::open_with(path, PoolConfig { max, cache_kb: 65536, mmap_kb: 262144, live_max: max * 2 })
    }

    pub fn open_with(path: impl Into<PathBuf>, cfg: PoolConfig) -> rusqlite::Result<Self> {
        let path = path.into();
        // Validate the path once eagerly so a bad SCRYBOX_DB_PATH fails at boot.
        let conn = Self::connect(&path, cfg)?;
        drop(conn);
        let max = cfg.max.max(1);
        // live_max is honoured as given (from_env clamps it to >= max for
        // boot configs; explicit configs may set a tighter cap on purpose).
        let live_max = cfg.live_max.max(1);
        Ok(Self {
            path,
            state: Mutex::new(PoolState { idle: Vec::new(), live: 0 }),
            live_cv: std::sync::Condvar::new(),
            max,
            live_max,
            cfg,
            blocking_fallbacks: AtomicUsize::new(0),
        })
    }

    fn connect(path: &PathBuf, cfg: PoolConfig) -> rusqlite::Result<Connection> {
        // READ_WRITE (not READ_ONLY): WAL readers need the -shm file and
        // busy_timeout must cover the brief shared-lock window. Pooled
        // connections also serve Rust's write routes.
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // Same set as backend/src/db.js on open (foreign_keys stays OFF —
        // cascades are explicit in Rust write paths; optimize/analysis_limit are writer-side planner hints),
        // with the page-cache/mmap sizes taken from the pool budget instead
        // of Node's single-connection values.
        conn.execute_batch(
            &format!(
                "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -{};
             PRAGMA mmap_size = {};
             PRAGMA temp_store = MEMORY;",
                cfg.cache_kb,
                cfg.mmap_kb * 1024
            ),
        )?;
        Ok(conn)
    }

    /// Check out a connection, opening a fresh one when the idle list is
    /// empty — but at most `live_max` at a time. Past the cap this BLOCKS
    /// on the Condvar until another connection is returned, so callers on
    /// async-runtime workers should prefer `try_get` + spawn_blocking
    /// fallback (REVIEW M5); today's inline call sites already block in
    /// `connect()`, so the wait adds no new class of blocking.
    pub fn get(&self) -> rusqlite::Result<PooledConn<'_>> {
        Ok(PooledConn { pool: self, conn: Some(self.get_conn()?) })
    }

    /// Core of `get()`: the idle-pop / permit-reserve / Condvar-wait loop
    /// returning the raw connection. Shared by the borrowed (`PooledConn`)
    /// and owned (`OwnedConn`, async) checkout paths.
    fn get_conn(&self) -> rusqlite::Result<Connection> {
        let mut st = self.state.guard();
        loop {
            if let Some(c) = st.idle.pop() {
                st.live += 1;
                return Ok(c);
            }
            if st.live < self.live_max {
                // Reserve the permit before releasing the mutex so the
                // open-connection total never exceeds live_max, then pay
                // the open+PRAGMA cost outside the lock.
                st.live += 1;
                drop(st);
                let opened = Self::connect(&self.path, self.cfg);
                st = self.state.guard();
                match opened {
                    Ok(c) => return Ok(c),
                    Err(e) => {
                        st.live -= 1;
                        self.live_cv.notify_all();
                        return Err(e);
                    }
                }
            }
            // Recovered (not panicked) on a poisoned mutex: the panic that
            // poisoned it may have skipped its live_cv.notify, so re-check
            // the predicate here rather than trusting a "wake".
            st = self.live_cv.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Idle-only pop for the async fast path (REVIEW M5): never opens a
    /// connection, never waits on the cap — the two operations that can
    /// pin a runtime worker for up to `busy_timeout`.
    fn take_idle(&self) -> Option<Connection> {
        let mut st = self.state.guard();
        let c = st.idle.pop();
        if c.is_some() {
            st.live += 1;
        }
        c
    }

    /// Non-blocking `get()`: returns `Ok(None)` when a checkout would have
    /// to WAIT on the live cap (an idle conn is available or a permit can
    /// be reserved → same path as `get()`). For async-inline probe paths
    /// that must never park a runtime worker; fall back to spawn_blocking.
    pub fn try_get(&self) -> rusqlite::Result<Option<PooledConn<'_>>> {
        let mut st = self.state.guard();
        if let Some(c) = st.idle.pop() {
            st.live += 1;
            return Ok(Some(PooledConn { pool: self, conn: Some(c) }));
        }
        if st.live < self.live_max {
            st.live += 1;
            drop(st);
            let opened = Self::connect(&self.path, self.cfg);
            match opened {
                Ok(c) => Ok(Some(PooledConn { pool: self, conn: Some(c) })),
                Err(e) => {
                    self.state.guard().live -= 1;
                    self.live_cv.notify_all();
                    Err(e)
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Owned twin of [`get`](Self::get) for callers that must move the
    /// checkout across threads (or into `spawn_blocking`): the borrowed
    /// [`PooledConn`] cannot outlive the pool reference.
    pub fn get_owned(self: &Arc<Self>) -> rusqlite::Result<OwnedConn> {
        Ok(OwnedConn { pool: self.clone(), conn: Some(self.get_conn()?) })
    }

    /// Async-safe checkout for code running ON a tokio runtime worker
    /// (REVIEW M5): hot handlers, the auth gate, revision probes. The fast
    /// path pops an IDLE connection only — never an inline `connect()` and
    /// never a Condvar park, the two operations that can pin a worker for
    /// up to `busy_timeout` under a checkpoint stall. On a miss the full
    /// `get()` (open / wait-for-permit) runs on the blocking pool.
    pub async fn get_async(self: &Arc<Self>) -> rusqlite::Result<OwnedConn> {
        if let Some(c) = self.take_idle() {
            return Ok(OwnedConn { pool: self.clone(), conn: Some(c) });
        }
        self.blocking_fallbacks.fetch_add(1, Ordering::Relaxed);
        let pool = self.clone();
        let conn = tokio::task::spawn_blocking(move || pool.get_conn())
            .await
            .map_err(|e| rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERNAL),
                Some(format!("db pool blocking hop: {e}")),
            ))??;
        Ok(OwnedConn { pool: self.clone(), conn: Some(conn) })
    }

    /// White-box: how many `get_async()` calls found no idle connection and
    /// had to hop to the blocking pool (M5 observability + cap tests).
    pub fn blocking_fallbacks(&self) -> usize {
        self.blocking_fallbacks.load(Ordering::Relaxed)
    }

    /// White-box: connections currently checked out / currently open
    /// (idle + live). Used by the pool-cap tests.
    pub fn live_count(&self) -> usize {
        self.state.guard().live
    }

    pub fn open_count(&self) -> usize {
        let st = self.state.guard();
        st.live + st.idle.len()
    }

    /// Pre-create idle connections so steady-state `get()` never pays the
    /// SQLite open+PRAGMA cost on a runtime worker (the hit path checks out
    /// connections inline since iter 6). Only fills up to `max` idle, and
    /// never past `live_max` total open.
    pub fn warm(&self, n: usize) {
        let want = {
            let st = self.state.guard();
            let target = n.min(self.max).min(self.live_max);
            target.saturating_sub(st.live + st.idle.len())
        };
        for _ in 0..want {
            // Reserve a permit around the open so concurrent `get()`s
            // cannot push the total past live_max while we connect.
            {
                let mut st = self.state.guard();
                if st.live + st.idle.len() >= self.live_max {
                    break;
                }
                st.live += 1;
            }
            let opened = Self::connect(&self.path, self.cfg);
            let mut st = self.state.guard();
            st.live -= 1;
            match opened {
                Ok(c) if st.idle.len() < self.max => st.idle.push(c),
                Ok(_) => {} // idle list filled concurrently; drop the extra
                Err(_) => break,
            }
        }
    }

    fn put(&self, conn: Connection) {
        let mut st = self.state.guard();
        if st.idle.len() < self.max {
            st.idle.push(conn);
        } else {
            // Over the idle target: close it. Drop outside the lock so
            // SQLite teardown IO never holds the pool mutex.
            drop(st);
            drop(conn);
            st = self.state.guard();
        }
        st.live = st.live.saturating_sub(1);
        // notify_all (not notify_one): each return frees exactly one
        // checkout, and a woken waiter can have its idle conn raced away
        // by a fresh caller — a missed notify would strand it until the
        // next return anyway.
        self.live_cv.notify_all();
    }

    /// One-shot twin of this pool with the mmap window OFF (iter-64 boot-RSS
    /// attribution): for background/boot scans whose touched pages must not
    /// pin the SERVING pool's mmap windows in RSS. SQLite `mmap_size` pages,
    /// once faulted, stay mapped (and in RSS) as long as the mapping lives;
    /// a boot stats warm pre-faulting the pool's 32 MB windows cost ~85 MB
    /// of file-backed RSS at idle (144 MB boot vs 14 MB warm-off). Reads
    /// through this twin go the read() path — kernel page cache keeps them
    /// cheap and the pages leave RSS when the twin (and its conns) drop.
    /// Same path + per-conn page-cache budget; max 1 connection.
    pub fn scratch(&self) -> rusqlite::Result<DbPool> {
        Self::open_with(
            &self.path,
            PoolConfig { max: 1, cache_kb: self.cfg.cache_kb, mmap_kb: 0, live_max: 1 },
        )
    }
}

pub struct PooledConn<'a> {
    pool: &'a DbPool,
    conn: Option<Connection>,
}

impl std::ops::Deref for PooledConn<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("conn present until drop")
    }
}

impl Drop for PooledConn<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.put(conn);
        }
    }
}

/// Owned twin of [`PooledConn`] for async callers: holds the pool by `Arc`
/// so the checkout can be produced inside `spawn_blocking` (`'static`) and
/// moved back into the awaiting task. Derefs to `Connection`, returns the
/// connection to the pool on drop — same accounting as `PooledConn`.
pub struct OwnedConn {
    pool: Arc<DbPool>,
    conn: Option<Connection>,
}

impl std::ops::Deref for OwnedConn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("conn present until drop")
    }
}

impl Drop for OwnedConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.put(conn);
        }
    }
}

/// The `collection_read_revision` counter, bumped by DB triggers on every
/// collection/card_cache write (the same value Node's ETag uses). `None`
/// when the row does not exist. Each call runs a fresh autocommit read, so
/// in WAL mode it sees every commit Node has made.
pub fn current_revision(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    let mut stmt = match conn.prepare_cached("SELECT revision FROM collection_read_revision WHERE id = 1") {
        Ok(s) => s,
        Err(_) => return Ok(None), // table missing: behave like Node's `if (!row) return null`
    };
    let mut rows = stmt.query([])?;
    Ok(rows.next()?.map(|r| r.get::<_, i64>(0)).transpose()?)
}

/// The `deck_list_revision` counter, bumped by DB triggers on every
/// decks/deck_cards write. Not used by Node's collection ETag (Node
/// recomputes the allocation views fresh per request) — the Rust
/// collection cache uses it as a SECOND validity key for the shapes that
/// embed `checked_out_qty` (review 2026-11-13 C1). `None` when the row
/// does not exist.
pub fn current_deck_revision(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    let mut stmt = match conn.prepare_cached("SELECT revision FROM deck_list_revision WHERE id = 1") {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let mut rows = stmt.query([])?;
    Ok(rows.next()?.map(|r| r.get::<_, i64>(0)).transpose()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scrybox-dbtest-{}-{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.db");
        // DbPool opens READ_WRITE without CREATE — the file must exist.
        Connection::open(&p).unwrap();
        p
    }

    fn pragmas(conn: &Connection, name: &str) -> i64 {
        conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, i64>(0)).unwrap()
    }

    #[test]
    fn scratch_twin_disables_mmap_and_keeps_page_cache_budget() {
        // Iter-64 boot-warm gate: the scratch twin must never open an mmap
        // window (that's the whole RSS point) and must keep the serving
        // pool's per-conn page-cache budget so boot scans stay fast.
        let p = temp_db("scratch");
        let pool = DbPool::open_with(&p, PoolConfig { max: 4, cache_kb: 8192, mmap_kb: 32768, live_max: 8 }).unwrap();
        let scratch = pool.scratch().unwrap();
        let conn = scratch.get().unwrap();
        assert_eq!(pragmas(&conn, "mmap_size"), 0);
        assert_eq!(pragmas(&conn, "cache_size"), -8192);
        // Same DB, readable through the twin.
        assert_eq!(conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0)).unwrap(), "ok");
        drop(conn);
        // Serving pool untouched.
        let serving = pool.get().unwrap();
        assert_eq!(pragmas(&serving, "mmap_size"), 32768 * 1024);
    }

    #[test]
    fn pool_applies_global_budget_split_across_connections() {
        let p = temp_db("budget");
        let pool = DbPool::open_with(&p, PoolConfig { max: 8, cache_kb: 8192, mmap_kb: 32768, live_max: 16 }).unwrap();
        let conn = pool.get().unwrap();
        // Negative cache_size PRAGMA reads back as the negative KiB budget.
        assert_eq!(pragmas(&conn, "cache_size"), -8192);
        assert_eq!(pragmas(&conn, "mmap_size"), 32768 * 1024);
        drop(conn);
        // Warmed connections carry the same config as on-demand ones.
        pool.warm(3);
        let conn = pool.get().unwrap();
        assert_eq!(pragmas(&conn, "cache_size"), -8192);
        assert_eq!(pragmas(&conn, "mmap_size"), 32768 * 1024);
    }

    #[test]
    fn legacy_open_keeps_node_pragma_values() {
        // tests/jobs/bin callers that use open(path, max) keep the old
        // db.js-parity pragmas untouched.
        let p = temp_db("legacy");
        let pool = DbPool::open(&p, 4).unwrap();
        let conn = pool.get().unwrap();
        assert_eq!(pragmas(&conn, "cache_size"), -65536);
        assert_eq!(pragmas(&conn, "mmap_size"), 268435456);
    }

    #[test]
    fn from_env_splits_budget_floors_cache_and_honours_mmap_zero() {
        std::env::set_var("SCRYBOX_DB_POOL_MAX", "8");
        std::env::set_var("SCRYBOX_DB_CACHE_BUDGET_MB", "64");
        std::env::set_var("SCRYBOX_DB_MMAP_MB", "16");
        let c = PoolConfig::from_env(4);
        assert_eq!(c.max, 8);
        // MEMORY.md #7: the budget splits across live_max (2*max by default),
        // so a full burst (live_max live connections each with its own page
        // cache) stays inside the global budget — not 2× it as when the
        // split used `max`.
        assert_eq!(c.cache_kb, 64 * 1024 / 16);
        assert_eq!(c.mmap_kb, 16 * 1024);
        // Live cap default: 2x the idle target.
        assert_eq!(c.live_max, 16);
        // Tiny budget still floors at 2 MB/conn rather than 0.
        std::env::set_var("SCRYBOX_DB_CACHE_BUDGET_MB", "1");
        let c = PoolConfig::from_env(4);
        assert_eq!(c.cache_kb, 2048);
        // Explicit live cap, clamped up to `max` so warm() stays reachable.
        std::env::set_var("SCRYBOX_DB_LIVE_MAX", "2");
        let c = PoolConfig::from_env(4);
        assert_eq!(c.live_max, 8);
        std::env::set_var("SCRYBOX_DB_LIVE_MAX", "32");
        let c = PoolConfig::from_env(4);
        assert_eq!(c.live_max, 32);
        for k in ["SCRYBOX_DB_POOL_MAX", "SCRYBOX_DB_CACHE_BUDGET_MB", "SCRYBOX_DB_MMAP_MB", "SCRYBOX_DB_LIVE_MAX"] {
            std::env::remove_var(k);
        }
        // Defaults: budget 16 MB (MEMORY.md #7, split across live_max),
        // mmap 4 MB (Gate-D memory fix), caller's max.
        let c = PoolConfig::from_env(6);
        assert_eq!(c.max, 6);
        assert_eq!(c.live_max, 12);
        // 16 MB / 12 conns = 1365 KB -> the 2 MB/conn floor wins.
        assert_eq!(c.cache_kb, 2048);
        assert_eq!(c.mmap_kb, 4 * 1024);
        // Narrow pool shows the live_max split without the floor: max=1,
        // live=2 -> 8 MB/conn on the 16 MB default budget.
        let c = PoolConfig::from_env(1);
        assert_eq!(c.live_max, 2);
        assert_eq!(c.cache_kb, 16 * 1024 / 2);
        // MEMORY.md #3: SCRYBOX_DB_MMAP_MB=0 must mean "mmap OFF". The old
        // shared env_i64 filtered `> 0`, so 0 silently became the default
        // (a deliberate disable was a no-op).
        std::env::set_var("SCRYBOX_DB_MMAP_MB", "0");
        let c = PoolConfig::from_env(4);
        assert_eq!(c.mmap_kb, 0, "MMAP_MB=0 must produce mmap_kb 0, not the default");
        // The PRAGMA actually lands at 0 on a real pooled connection.
        let p = temp_db("mmap0");
        let pool = DbPool::open_with(&p, c).unwrap();
        let conn = pool.get().unwrap();
        assert_eq!(pragmas(&conn, "mmap_size"), 0);
        drop(conn);
        drop(pool);
        // restart-rust.sh passes an EMPTY string for unset knobs: empty and
        // garbage must fall back to the 4 MB default, not panic or 0-out.
        std::env::set_var("SCRYBOX_DB_MMAP_MB", "");
        assert_eq!(PoolConfig::from_env(4).mmap_kb, 4 * 1024);
        std::env::set_var("SCRYBOX_DB_MMAP_MB", "abc");
        assert_eq!(PoolConfig::from_env(4).mmap_kb, 4 * 1024);
        std::env::set_var("SCRYBOX_DB_MMAP_MB", "-1");
        assert_eq!(PoolConfig::from_env(4).mmap_kb, 4 * 1024);
        // Other knobs keep rejecting 0 via the shared filter (0 is nonsense
        // for a pool size / budget / live cap).
        std::env::set_var("SCRYBOX_DB_POOL_MAX", "0");
        std::env::set_var("SCRYBOX_DB_CACHE_BUDGET_MB", "0");
        let c = PoolConfig::from_env(5);
        assert_eq!(c.max, 5);
        // 0 rejected -> default budget 16 MB / live_max 10 = 1638 KB -> floor.
        assert_eq!(c.cache_kb, 2048);
        std::env::remove_var("SCRYBOX_DB_MMAP_MB");
        std::env::remove_var("SCRYBOX_DB_POOL_MAX");
        std::env::remove_var("SCRYBOX_DB_CACHE_BUDGET_MB");
    }

    // ---- H3/M3: LIVE connection cap (REVIEW-concurrency H3, REVIEW-writes M3) ----

    #[test]
    fn from_env_page_cache_burst_stays_inside_global_budget() {
        // MEMORY.md #7: live_max=2*max burst must not double the "global"
        // page-cache budget. The invariant: live_max * cache_kb == budget
        // (while above the 2 MB/conn floor). Red-before-fix: with the split
        // by `max`, live_max*cache_kb = 2*budget -> first assert fails
        // (8*2048 KiB = 16 MiB != 32... pre-fix cache_kb was budget/max=
        // 4096 KiB, product 32 MiB).
        std::env::set_var("SCRYBOX_DB_POOL_MAX", "4");
        std::env::set_var("SCRYBOX_DB_CACHE_BUDGET_MB", "16");
        let c = PoolConfig::from_env(4);
        assert_eq!(c.live_max, 8);
        assert_eq!(c.cache_kb * c.live_max as i64, 16 * 1024, "full burst must equal the global budget");
        std::env::set_var("SCRYBOX_DB_CACHE_BUDGET_MB", "64");
        let c = PoolConfig::from_env(4);
        assert_eq!(c.cache_kb * c.live_max as i64, 64 * 1024);
        // Explicit live cap re-divides the budget to match.
        std::env::set_var("SCRYBOX_DB_LIVE_MAX", "4");
        let c = PoolConfig::from_env(4);
        assert_eq!(c.cache_kb * c.live_max as i64, 64 * 1024);
        std::env::remove_var("SCRYBOX_DB_POOL_MAX");
        std::env::remove_var("SCRYBOX_DB_CACHE_BUDGET_MB");
        std::env::remove_var("SCRYBOX_DB_LIVE_MAX");
    }

    fn capped_pool(tag: &str, max: usize, live_max: usize) -> (PathBuf, DbPool) {
        let p = temp_db(tag);
        let pool = DbPool::open_with(&p, PoolConfig { max, cache_kb: 2048, mmap_kb: 0, live_max }).unwrap();
        (p, pool)
    }

    #[test]
    fn get_blocks_at_live_cap_and_wakes_on_return() {
        // H3 red-before-fix: pre-fix get() opened the 3rd connection
        // immediately — the recv_timeout assert below would have received.
        let (_p, pool) = capped_pool("livecap", 1, 2);
        let a = pool.get().unwrap();
        let _b = pool.get().unwrap();
        assert_eq!(pool.live_count(), 2);
        assert_eq!(pool.open_count(), 2);
        // Saturated: try_get must not open (or block for) a 3rd connection.
        assert!(pool.try_get().unwrap().is_none());
        assert_eq!(pool.open_count(), 2);

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                let _c = pool.get().unwrap();
                tx.send(()).unwrap();
                // Hold the checkout until the test's asserts are done.
                std::thread::sleep(std::time::Duration::from_millis(150));
            });
            assert!(
                rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(),
                "H3: get() opened a connection past live_max instead of waiting"
            );
            drop(a);
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .expect("blocked get() was not woken by a returned connection");
            assert!(pool.open_count() <= 2, "open connections exceeded live_max");
        });
    }

    #[test]
    fn try_get_serves_under_cap_without_waiting() {
        // Positive control: under the cap, try_get behaves like get().
        let (_p, pool) = capped_pool("tryget", 1, 2);
        let a = pool.try_get().unwrap().expect("permit under cap");
        let b = pool.try_get().unwrap().expect("permit under cap");
        assert_eq!(pool.open_count(), 2);
        assert!(pool.try_get().unwrap().is_none());
        drop(a);
        // Returned conn is reused from idle, no new open.
        let c = pool.try_get().unwrap().expect("idle conn available");
        assert_eq!(pool.open_count(), 2);
        drop((b, c));
    }

    #[test]
    fn burst_never_opens_past_live_cap() {
        // H3 memory lever: a stampede of checkouts must not open one
        // SQLite connection each. Red-before-fix: high water hit 12.
        let (_p, pool) = capped_pool("burst", 1, 3);
        let pool = std::sync::Arc::new(pool);
        let high = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        std::thread::scope(|s| {
            for _ in 0..12 {
                let pool = std::sync::Arc::clone(&pool);
                let high = std::sync::Arc::clone(&high);
                s.spawn(move || {
                    let _c = pool.get().unwrap();
                    high.fetch_max(pool.open_count(), std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                });
            }
        });
        assert!(
            high.load(std::sync::atomic::Ordering::SeqCst) <= 3,
            "H3: {} connections open simultaneously under a live cap of 3",
            high.load(std::sync::atomic::Ordering::SeqCst)
        );
    }

    #[test]
    fn warm_respects_live_cap_but_fills_idle_when_free() {
        // H3: warm() must not stack idle connections on top of live ones.
        // Red-before-fix (live half): pre-fix warm() ignored live count and
        // opened 4 more, total 6 > live_max 2.
        let (_p, pool) = capped_pool("warm", 4, 2);
        let _a = pool.get().unwrap();
        let _b = pool.get().unwrap();
        pool.warm(4);
        assert_eq!(pool.open_count(), 2, "warm() exceeded the live cap");
        drop((_a, _b));
        // Positive control: with nothing live, warm fills the idle target.
        let pool = DbPool::open_with(
            _p.clone(),
            PoolConfig { max: 4, cache_kb: 2048, mmap_kb: 0, live_max: 8 },
        )
        .unwrap();
        pool.warm(4);
        assert_eq!(pool.open_count(), 4);
        let _c = pool.get().unwrap();
        assert_eq!(pool.open_count(), 4, "hit-path checkout must reuse an idle conn");
    }

    // ---- M5: async checkout on runtime workers (REVIEW-concurrency M5) ----

    #[test]
    fn get_async_serves_idle_inline_and_hops_when_none_idle() {
        // Hit path = idle pop on the worker (no blocking hop, no open).
        // Miss path (no idle conn) = full get() on the blocking pool —
        // the open+PRAGMA cost is never paid on the worker thread.
        let (_p, pool) = capped_pool("async-idle", 2, 4);
        let pool = std::sync::Arc::new(pool);
        pool.warm(1);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();

        let p1 = std::sync::Arc::clone(&pool);
        let a = rt.block_on(async move { p1.get_async().await.unwrap() });
        assert_eq!(pool.blocking_fallbacks(), 0, "idle checkout must not hop to the blocking pool");
        assert_eq!(pool.live_count(), 1);

        // `a` still held: idle is empty, so this checkout must HOP.
        let p2 = std::sync::Arc::clone(&pool);
        let b = rt.block_on(async move { p2.get_async().await.unwrap() });
        assert_eq!(pool.blocking_fallbacks(), 1, "no-idle checkout must hop to the blocking pool");
        assert_eq!(pool.open_count(), 2, "the fresh connection was opened off-worker");

        drop((a, b));
        // Returned connections are idle: async checkouts go inline again.
        let p3 = std::sync::Arc::clone(&pool);
        let c = rt.block_on(async move { p3.get_async().await.unwrap() });
        assert_eq!(pool.blocking_fallbacks(), 1, "idle checkout after a return must not hop");
        drop(c);
        assert_eq!(pool.live_count(), 0);
    }

    #[test]
    fn get_async_never_parks_a_runtime_worker() {
        // M5 red-before-fix: neuter get_async() to the pre-M5 shape (pay
        // the checkout inline on the worker — get_conn() instead of the
        // spawn_blocking hop) and the single worker parks on the Condvar
        // behind the live cap, the timer task starves, `pinged` stays
        // false at the 300 ms assert. With the hop in place the worker
        // stays free throughout. The held connection is released by a
        // helper thread at 1.2 s so a red run FAILS (the parked worker
        // wakes, rt drop joins) instead of deadlocking the test binary.
        let (_p, pool) = capped_pool("async-cap", 1, 1);
        let pool = std::sync::Arc::new(pool);
        let held = pool.get_owned().unwrap(); // live = live_max = 1, idle empty
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(held).unwrap();
        std::thread::spawn(move || {
            let h = rx.recv().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1200));
            drop(h);
        });

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let p1 = std::sync::Arc::clone(&pool);
        let waiter = rt.spawn(async move { p1.get_async().await });
        let pinged = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&pinged);
        rt.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            pinged.load(std::sync::atomic::Ordering::SeqCst),
            "M5: the runtime worker was parked by an inline checkout at the live cap"
        );
        let got = rt.block_on(waiter).unwrap().unwrap();
        assert_eq!(pool.blocking_fallbacks(), 1, "the capped checkout must have used the hop");
        drop(got);
        assert_eq!(pool.live_count(), 0);
        assert!(pool.open_count() <= 1, "async path opened more than live_max connections");
    }

    // --- REVIEW-errors-maint M2: the pool's state mutex must survive a
    // poisoning panic (pre-fix, state was `lock().unwrap()` at the old
    // db.rs:95/108/117: one panic under the guard took down every
    // DB-backed route in the process forever). ---

    fn poison_pool_state(pool: &DbPool) {
        // Poison while HELD, WITHOUT mutating: poisoning is a property of
        // the mutex, not of PoolState. Exactly what a real panic inside a
        // route/job would leave behind.
        let h = std::thread::scope(|s| {
            s.spawn(|| {
                let _st = pool.state.lock().unwrap();
                panic!("simulated panic while holding the pool state lock");
            })
            .join()
        });
        assert!(h.is_err(), "precondition: the poisoning thread must panic");
    }

    #[test]
    fn pool_survives_poisoned_state_mutex() {
        let (_p, pool) = capped_pool("poison", 1, 2);
        poison_pool_state(&pool);
        // Pre-fix the next lines panicked with "mutex is poisoned".
        let c = pool.get().expect("M2: get() must survive a poisoned state mutex");
        assert_eq!(pool.live_count(), 1, "live accounting must keep working");
        let t = pool
            .try_get()
            .expect("M2: try_get() must survive a poisoned state mutex")
            .expect("a permit must be available");
        assert_eq!(pool.live_count(), 2);
        drop((c, t));
        assert_eq!(pool.live_count(), 0, "put() path must keep working");
    }

    #[test]
    fn poisoned_pool_get_still_returns_a_connection() {
        // Poison while a checkout is live, then checkout + return must
        // both keep working through the poisoned mutex (the pass that pops
        // idle / reserves live is the hot path).
        let (_p, pool) = capped_pool("poison2", 1, 2);
        let _c = pool.get().unwrap();
        poison_pool_state(&pool);
        let c2 = pool
            .get()
            .expect("M2: a checkout after poisoning must not panic on the mutex");
        assert_eq!(pool.live_count(), 2);
        drop((c2, _c)); // put() path runs under the poisoned mutex
        assert_eq!(pool.live_count(), 0);
    }

    #[test]
    fn poisoned_pool_try_get_returns_err_not_panic_when_connect_fails() {
        // The connect-error arm (live -= 1 + notify_all under the mutex)
        // must answer Err, not a poison panic.
        let (_p, pool) = capped_pool("poison3", 1, 1); // eager connect ok
        poison_pool_state(&pool);
        std::fs::remove_file(&_p).unwrap(); // every NEW connect now fails;
        // the idle list is empty (open_with opens lazily per checkout), so
        // try_get takes the connect path under the poisoned mutex.
        let r = pool.try_get();
        assert!(r.is_err(), "connect failure must surface as Err, not a poison panic");
        assert_eq!(pool.live_count(), 0, "the rollback path ran under the poisoned mutex");
    }
}

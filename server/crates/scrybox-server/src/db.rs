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

use std::{path::PathBuf, sync::Mutex};

use rusqlite::Connection;

/// Per-pool SQLite tuning (Gate D final-phase DB tuning).
///
/// `cache_kb` is the PER-CONNECTION page-cache budget in KiB (negative
/// PRAGMA cache_size semantics). The pool applies a GLOBAL budget
/// (`SCRYBOX_DB_CACHE_BUDGET_MB`, default 64 MB — Node's single-connection
/// size) divided across `max` connections, so 16 pooled readers no longer
/// each get their own 64 MB cache. `mmap_kb` (default 32 MB, was a fixed
/// 256 MB) is the per-connection mmap window; the DB copy is 256 MB and
/// the hot working set is far smaller, so a smaller window caps how much
/// file-backed page cache can be faulted in per reader.
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
    /// (global, split across the pool), SCRYBOX_DB_MMAP_MB (per conn),
    /// SCRYBOX_DB_LIVE_MAX (live checkout cap, default 2*max).
    pub fn from_env(default_max: usize) -> Self {
        let env_i64 = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<i64>().ok()).filter(|v| *v > 0);
        let max = env_i64("SCRYBOX_DB_POOL_MAX").unwrap_or(default_max as i64).max(1) as usize;
        let budget_mb = env_i64("SCRYBOX_DB_CACHE_BUDGET_MB").unwrap_or(64);
        let cache_kb = (budget_mb * 1024 / max as i64).max(2048); // never below 2 MB/conn
        let mmap_kb = env_i64("SCRYBOX_DB_MMAP_MB").unwrap_or(32) * 1024;
        let live_max = env_i64("SCRYBOX_DB_LIVE_MAX")
            .unwrap_or(max as i64 * 2)
            .max(max as i64)
            .max(1) as usize;
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
        let mut st = self.state.lock().unwrap();
        loop {
            if let Some(c) = st.idle.pop() {
                st.live += 1;
                return Ok(PooledConn { pool: self, conn: Some(c) });
            }
            if st.live < self.live_max {
                // Reserve the permit before releasing the mutex so the
                // open-connection total never exceeds live_max, then pay
                // the open+PRAGMA cost outside the lock.
                st.live += 1;
                drop(st);
                let opened = Self::connect(&self.path, self.cfg);
                st = self.state.lock().unwrap();
                match opened {
                    Ok(c) => return Ok(PooledConn { pool: self, conn: Some(c) }),
                    Err(e) => {
                        st.live -= 1;
                        self.live_cv.notify_all();
                        return Err(e);
                    }
                }
            }
            st = self.live_cv.wait(st).unwrap();
        }
    }

    /// Non-blocking `get()`: returns `Ok(None)` when a checkout would have
    /// to WAIT on the live cap (an idle conn is available or a permit can
    /// be reserved → same path as `get()`). For async-inline probe paths
    /// that must never park a runtime worker; fall back to spawn_blocking.
    pub fn try_get(&self) -> rusqlite::Result<Option<PooledConn<'_>>> {
        let mut st = self.state.lock().unwrap();
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
                    self.state.lock().unwrap().live -= 1;
                    self.live_cv.notify_all();
                    Err(e)
                }
            }
        } else {
            Ok(None)
        }
    }

    /// White-box: connections currently checked out / currently open
    /// (idle + live). Used by the pool-cap tests.
    pub fn live_count(&self) -> usize {
        self.state.lock().unwrap().live
    }

    pub fn open_count(&self) -> usize {
        let st = self.state.lock().unwrap();
        st.live + st.idle.len()
    }

    /// Pre-create idle connections so steady-state `get()` never pays the
    /// SQLite open+PRAGMA cost on a runtime worker (the hit path checks out
    /// connections inline since iter 6). Only fills up to `max` idle, and
    /// never past `live_max` total open.
    pub fn warm(&self, n: usize) {
        let want = {
            let st = self.state.lock().unwrap();
            let target = n.min(self.max).min(self.live_max);
            target.saturating_sub(st.live + st.idle.len())
        };
        for _ in 0..want {
            // Reserve a permit around the open so concurrent `get()`s
            // cannot push the total past live_max while we connect.
            {
                let mut st = self.state.lock().unwrap();
                if st.live + st.idle.len() >= self.live_max {
                    break;
                }
                st.live += 1;
            }
            let opened = Self::connect(&self.path, self.cfg);
            let mut st = self.state.lock().unwrap();
            st.live -= 1;
            match opened {
                Ok(c) if st.idle.len() < self.max => st.idle.push(c),
                Ok(_) => {} // idle list filled concurrently; drop the extra
                Err(_) => break,
            }
        }
    }

    fn put(&self, conn: Connection) {
        let mut st = self.state.lock().unwrap();
        if st.idle.len() < self.max {
            st.idle.push(conn);
        } else {
            // Over the idle target: close it. Drop outside the lock so
            // SQLite teardown IO never holds the pool mutex.
            drop(st);
            drop(conn);
            st = self.state.lock().unwrap();
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
    fn from_env_splits_budget_and_floors_per_conn_cache() {
        std::env::set_var("SCRYBOX_DB_POOL_MAX", "8");
        std::env::set_var("SCRYBOX_DB_CACHE_BUDGET_MB", "64");
        std::env::set_var("SCRYBOX_DB_MMAP_MB", "16");
        let c = PoolConfig::from_env(4);
        assert_eq!(c.max, 8);
        assert_eq!(c.cache_kb, 64 * 1024 / 8);
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
        // Defaults: budget 64 MB, mmap 32 MB, caller's max.
        let c = PoolConfig::from_env(6);
        assert_eq!(c.max, 6);
        assert_eq!(c.cache_kb, 64 * 1024 / 6);
        assert_eq!(c.mmap_kb, 32 * 1024);
    }

    // ---- H3/M3: LIVE connection cap (REVIEW-concurrency H3, REVIEW-writes M3) ----

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
}

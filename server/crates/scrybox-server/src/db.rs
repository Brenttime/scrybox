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
}

impl PoolConfig {
    /// Env-tunable defaults: SCRYBOX_DB_POOL_MAX, SCRYBOX_DB_CACHE_BUDGET_MB
    /// (global, split across the pool), SCRYBOX_DB_MMAP_MB (per conn).
    pub fn from_env(default_max: usize) -> Self {
        let env_i64 = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<i64>().ok()).filter(|v| *v > 0);
        let max = env_i64("SCRYBOX_DB_POOL_MAX").unwrap_or(default_max as i64).max(1) as usize;
        let budget_mb = env_i64("SCRYBOX_DB_CACHE_BUDGET_MB").unwrap_or(64);
        let cache_kb = (budget_mb * 1024 / max as i64).max(2048); // never below 2 MB/conn
        let mmap_kb = env_i64("SCRYBOX_DB_MMAP_MB").unwrap_or(32) * 1024;
        Self { max, cache_kb, mmap_kb }
    }
}

/// Pool of ready-to-use read connections. Connections are created on demand
/// up to `max`; returned connections are kept for the next request, extras
/// are dropped.
pub struct DbPool {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
    max: usize,
    cfg: PoolConfig,
}

impl DbPool {
    pub fn open(path: impl Into<PathBuf>, max: usize) -> rusqlite::Result<Self> {
        Self::open_with(path, PoolConfig { max: max.max(1), cache_kb: 65536, mmap_kb: 262144 })
    }

    pub fn open_with(path: impl Into<PathBuf>, cfg: PoolConfig) -> rusqlite::Result<Self> {
        let path = path.into();
        // Validate the path once eagerly so a bad SCRYBOX_DB_PATH fails at boot.
        let conn = Self::connect(&path, cfg)?;
        drop(conn);
        Ok(Self { path, idle: Mutex::new(Vec::new()), max: cfg.max.max(1), cfg })
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

    pub fn get(&self) -> rusqlite::Result<PooledConn<'_>> {
        let idle = self.idle.lock().unwrap().pop();
        let conn = match idle {
            Some(c) => c,
            None => Self::connect(&self.path, self.cfg)?,
        };
        Ok(PooledConn { pool: self, conn: Some(conn) })
    }

    /// Pre-create idle connections so steady-state `get()` never pays the
    /// SQLite open+PRAGMA cost on a runtime worker (the hit path checks out
    /// connections inline since iter 6). Only fills up to `max`.
    pub fn warm(&self, n: usize) {
        let want = n.min(self.max);
        let have = self.idle.lock().unwrap().len();
        for _ in have..want {
            if let Ok(conn) = Self::connect(&self.path, self.cfg) {
                self.put(conn);
            }
        }
    }

    fn put(&self, conn: Connection) {
        let mut idle = self.idle.lock().unwrap();
        if idle.len() < self.max {
            idle.push(conn);
        }
        // else: drop the connection
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
            PoolConfig { max: 1, cache_kb: self.cfg.cache_kb, mmap_kb: 0 },
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
        let pool = DbPool::open_with(&p, PoolConfig { max: 4, cache_kb: 8192, mmap_kb: 32768 }).unwrap();
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
        let pool = DbPool::open_with(&p, PoolConfig { max: 8, cache_kb: 8192, mmap_kb: 32768 }).unwrap();
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
        // Tiny budget still floors at 2 MB/conn rather than 0.
        std::env::set_var("SCRYBOX_DB_CACHE_BUDGET_MB", "1");
        let c = PoolConfig::from_env(4);
        assert_eq!(c.cache_kb, 2048);
        for k in ["SCRYBOX_DB_POOL_MAX", "SCRYBOX_DB_CACHE_BUDGET_MB", "SCRYBOX_DB_MMAP_MB"] {
            std::env::remove_var(k);
        }
        // Defaults: budget 64 MB, mmap 32 MB, caller's max.
        let c = PoolConfig::from_env(6);
        assert_eq!(c.max, 6);
        assert_eq!(c.cache_kb, 64 * 1024 / 6);
        assert_eq!(c.mmap_kb, 32 * 1024);
    }
}

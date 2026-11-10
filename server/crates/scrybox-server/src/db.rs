//! Read-only SQLite access shared by the ported Rust routes.
//!
//! Node owns writes (it is still the backend for everything unported and it
//! also writes to this same DB copy). Rust therefore never issues a write;
//! it reads through a small pool of independent connections so SQLite can
//! serve them concurrently out of WAL mode. The PRAGMAs mirror
//! `backend/src/db.js` so readers get the same page-cache/mmap behaviour
//! Node configured.

use std::{path::PathBuf, sync::Mutex};

use rusqlite::Connection;

/// Pool of ready-to-use read connections. Connections are created on demand
/// up to `max`; returned connections are kept for the next request, extras
/// are dropped.
pub struct DbPool {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
    max: usize,
}

impl DbPool {
    pub fn open(path: impl Into<PathBuf>, max: usize) -> rusqlite::Result<Self> {
        let path = path.into();
        // Validate the path once eagerly so a bad SCRYBOX_DB_PATH fails at boot.
        let conn = Self::connect(&path)?;
        drop(conn);
        Ok(Self { path, idle: Mutex::new(Vec::new()), max: max.max(1) })
    }

    fn connect(path: &PathBuf) -> rusqlite::Result<Connection> {
        // READ_WRITE (not READ_ONLY): WAL readers need the -shm file and
        // busy_timeout must cover the brief shared-lock window. We still
        // never issue writes from Rust.
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // Same set as backend/src/db.js on open (foreign_keys is irrelevant
        // for reads; optimize/analysis_limit are writer-side planner hints).
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -65536;
             PRAGMA mmap_size = 268435456;
             PRAGMA temp_store = MEMORY;",
        )?;
        Ok(conn)
    }

    pub fn get(&self) -> rusqlite::Result<PooledConn<'_>> {
        let idle = self.idle.lock().unwrap().pop();
        let conn = match idle {
            Some(c) => c,
            None => Self::connect(&self.path)?,
        };
        Ok(PooledConn { pool: self, conn: Some(conn) })
    }

    fn put(&self, conn: Connection) {
        let mut idle = self.idle.lock().unwrap();
        if idle.len() < self.max {
            idle.push(conn);
        }
        // else: drop the connection
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

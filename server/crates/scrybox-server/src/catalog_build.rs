//! Lane B unit 6c: catalog.js twin — the local scan-catalog build engine
//! (cache phase via cardSets, embed phase via milo on ort) plus the four
//! admin build-control routes and the weekly auto-update job behind
//! `RUST_ROUTES=catalog_build`:
//!
//!   GET  /api/admin/catalogs          list + progress + last
//!   GET  /api/admin/catalogs/progress in-memory counters only
//!   POST /api/admin/catalogs/build    start (409 'already running'/unknown game)
//!   POST /api/admin/catalogs/stop     cancel flag
//!
//! `current`/`last` are process-global exactly like Node's module state: one
//! build at a time, `state()` returns a snapshot copy, `stop()` only flips
//! the `cancelled` flag (a cancelled build still writes what it has).
//!
//! Byte-parity notes:
//!   * meta JSON key order `dim, ids, srcs, game, lang, model, builtAt` and
//!     `srcs` insertion order (prev.srcs order, existing keys updated in
//!     place, new ids appended) ride the preserve_order Map — the
//!     JSON.stringify shape matched;
//!   * ids order = reused rows (queue order, pushed synchronously by pump)
//!     interleaved with embed completions (Promise.race order →
//!     JoinSet completion order) then the scoped-merge kept rows;
//!   * images fetch with UA `Scrybox/1.8.5 (+github)` + Accept: image/*,
//!     8 in flight, redirect-follow, 30 s AbortSignal twin;
//!   * the POST /build response is the pre-spawn snapshot, like Node's
//!     synchronous `state()` return from `start()`;
//!   * normalJson (express.json 1 mb, strict) runs BEFORE the admin auth
//!     gate (server.js app chain) — over-limit/strict-parse failures
//!     Forward the untouched request, exactly like the cardscan lane.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Mutex,
};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode, Uri},
    response::Response,
};
use http_body_util::BodyExt;
use serde_json::{Map, Value};

use crate::auth::{authenticate, bearer_token};
use crate::card_sets::{list_all_sets, CacheErr};
use crate::cards_tags::json_response;
use crate::cardscan::parse_outer_json;
use crate::catalog_read::{to_name, LangEntry};
use crate::collection_writes::node_exact;
use crate::cvscan;
use crate::sets::{scan_exclusions, Exclusions, ScryfallSetsMemo};
use crate::scryfall::Scry;
use crate::AppState;

/// catalog.js `UA` — backend/package.json version read at require time.
/// Pinned against backend/package.json by the `scrybox_ua_version` test;
/// bump both together.
pub const SCRYBOX_UA: &str = "Scrybox/1.8.5 (+https://github.com/Brenttime/scrybox)";

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Job state (catalog.js `current` / `last`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Job {
    pub game: String,
    pub lang: String,
    pub sets: Vec<String>,
    pub phase: String,
    pub done: usize,
    pub total: usize,
    pub message: String,
    pub started_at: i64,
    pub cancelled: bool,
}

impl Job {
    /// `{...job}` in the job-literal key order Node serialises.
    fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("game".into(), self.game.clone().into());
        m.insert("lang".into(), self.lang.clone().into());
        m.insert("sets".into(), self.sets.iter().map(|s| Value::String(s.clone())).collect::<Vec<_>>().into());
        m.insert("phase".into(), self.phase.clone().into());
        m.insert("done".into(), (self.done as u64).into());
        m.insert("total".into(), (self.total as u64).into());
        m.insert("message".into(), self.message.clone().into());
        m.insert("startedAt".into(), (self.started_at as u64).into());
        m.insert("cancelled".into(), self.cancelled.into());
        Value::Object(m)
    }
}

static CURRENT: Mutex<Option<Job>> = Mutex::new(None);
static LAST: Mutex<Option<Value>> = Mutex::new(None);

fn with_current<F: FnOnce(&mut Job)>(f: F) {
    if let Some(j) = CURRENT.lock().unwrap().as_mut() {
        f(j);
    }
}

fn cancelled() -> bool {
    CURRENT.lock().unwrap().as_ref().map(|j| j.cancelled).unwrap_or(false)
}

/// catalog.js `state()` — snapshot copy or null.
pub fn state() -> Value {
    CURRENT.lock().unwrap().as_ref().map(|j| j.to_value()).unwrap_or(Value::Null)
}

/// catalog.js `lastResult()` — the state() snapshot taken in `finally`.
pub fn last_result() -> Value {
    LAST.lock().unwrap().clone().unwrap_or(Value::Null)
}

/// catalog.js `stop()` — flip the flag on the running build.
pub fn stop() -> bool {
    match CURRENT.lock().unwrap().as_mut() {
        Some(j) => {
            j.cancelled = true;
            true
        }
        None => false,
    }
}

#[cfg(test)]
pub fn reset_for_test() {
    *CURRENT.lock().unwrap() = None;
    *LAST.lock().unwrap() = None;
}

// ---------------------------------------------------------------------------
// catalog.js file naming (the cvScan local-catalog pair)
// ---------------------------------------------------------------------------

fn meta_path(dir: &Path, game: &str, lang: &str) -> PathBuf {
    dir.join(format!("milo-{game}{}-local.json", cvscan::suffix_lang(lang)))
}
fn bin_path(dir: &Path, game: &str, lang: &str) -> PathBuf {
    dir.join(format!("milo-{game}{}-local.bin", cvscan::suffix_lang(lang)))
}

// ---------------------------------------------------------------------------
// list() / newSetCount()
// ---------------------------------------------------------------------------

/// catalog.js `newSetCount(game, lang)` — printable sets still missing from
/// card_cache for this language, minus recorded gaps. Query failure → None.
pub fn new_set_count(conn: &rusqlite::Connection, game: &str, lang: &str) -> Option<usize> {
    if game != "mtg" {
        return Some(0);
    }
    let mut gaps: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let mut st = conn.prepare("SELECT set_id FROM set_data_gaps WHERE language = ?").ok()?;
        let rows = st.query_map([lang], |r| r.get::<_, String>(0)).ok()?;
        for s in rows.flatten() {
            gaps.insert(s.to_lowercase());
        }
    }
    let mut cached: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let mut st = conn
            .prepare("SELECT DISTINCT LOWER(set_id) FROM card_cache WHERE language = ?")
            .ok()?;
        let rows = st.query_map([lang], |r| r.get::<_, Option<String>>(0)).ok()?;
        for row in rows {
            if let Ok(Some(s)) = row {
                cached.insert(s);
            }
        }
    }
    let mut n = 0usize;
    let mut st = conn.prepare("SELECT id FROM sets WHERE COALESCE(total, 0) > 0").ok()?;
    let rows = st.query_map([], |r| r.get::<_, String>(0)).ok()?;
    for row in rows {
        let Ok(id) = row else { continue };
        let mut s = id.to_lowercase();
        if let Some(stripped) = s.strip_prefix("mtg-") {
            s = stripped.to_string();
        }
        if !gaps.contains(&s) && !cached.contains(&s) {
            n += 1;
        }
    }
    Some(n)
}

/// One catalog.js list() row. Key order matches the object literal;
/// `claimed` is appended by the caller for grouped rows only.
fn lang_entry(
    conn: &rusqlite::Connection,
    model: Option<&Path>,
    game: &str,
    lang: &str,
    cached: u64,
    with_art: u64,
) -> Result<Map<String, Value>, String> {
    let mut e = Map::new();
    e.insert("game".into(), game.into());
    e.insert("lang".into(), lang.into());
    let mut built = Value::Null;
    if let Some(model) = model {
        if meta_path(model, game, lang).exists() {
            let raw = std::fs::read_to_string(meta_path(model, game, lang)).map_err(|e| e.to_string())?;
            let meta: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
            let rows = meta.get("ids").and_then(Value::as_array).ok_or("meta.ids")?.len();
            let bytes = std::fs::metadata(bin_path(model, game, lang)).map_err(|e| e.to_string())?.len();
            let mut b = Map::new();
            b.insert("rows".into(), (rows as u64).into());
            b.insert("builtAt".into(), meta.get("builtAt").cloned().unwrap_or(Value::Null));
            b.insert("bytes".into(), bytes.into());
            built = Value::Object(b);
        }
    }
    e.insert("built".into(), built.clone());
    e.insert("cached".into(), cached.into());
    e.insert("withArt".into(), with_art.into());
    let new_sets = match &built {
        Value::Null => Value::Null,
        _ => new_set_count(conn, game, lang).map(|n| Value::from(n as u64)).unwrap_or(Value::Null),
    };
    e.insert("newSets".into(), new_sets);
    e.insert("published".into(), (built.is_null() && cvscan::is_built(game, lang)).into());
    Ok(e)
}

/// catalog.js `list()`.
pub fn list(conn: &rusqlite::Connection, model: Option<&Path>, game: &str) -> Result<Value, String> {
    let mut out: Vec<Value> = Vec::new();
    let langs = {
        let mut st = conn
            .prepare("SELECT language, COUNT(*) cached, SUM(CASE WHEN image_url IS NOT NULL AND image_url != '' THEN 1 ELSE 0 END) withArt FROM card_cache GROUP BY language")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| {
                Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?))
            })
            .map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        for row in rows {
            let (lang, cached, with_art) = row.map_err(|e| e.to_string())?;
            v.push((lang.unwrap_or_else(|| "English".into()), cached as u64, with_art.map(|w| w as u64)));
        }
        v
    };
    if !langs.iter().any(|(l, _, _)| l == "English") {
        // Synthetic English row: NO claimed key, withArt 0 (object literal).
        let e = lang_entry(conn, model, game, "English", 0, 0)?;
        out.push(Value::Object(e));
    }
    let claimed_t: i64 = conn
        .query_row("SELECT SUM(total) t FROM sets", [], |r| r.get::<_, Option<i64>>(0))
        .map_err(|e| e.to_string())?
        .unwrap_or(0);
    for (lang, cached, with_art) in langs {
        let mut e = lang_entry(conn, model, game, &lang, cached, with_art.unwrap_or(0))?;
        if lang == "English" {
            // Node adds `claimed` to grouped English rows after the literal.
            e.insert("claimed".into(), (claimed_t.max(0) as u64).into());
        }
        out.push(Value::Object(e));
    }
    Ok(Value::Array(out))
}

// ---------------------------------------------------------------------------
// keptFromPrev + prev loading (exported for the resume lib tests)
// ---------------------------------------------------------------------------

pub struct Prev {
    pub vecs: Map<String, Value>,
    pub srcs: Map<String, Value>,
}

/// catalog.js `keptFromPrev(prev, freshIds)` — prev entries untouched by
/// this build, in prev.vecs (meta.ids) order.
pub fn kept_from_prev(prev: &Prev, fresh_ids: &[String]) -> Vec<(String, Vec<f32>, Option<String>)> {
    let mut out = Vec::new();
    for (id, v) in prev.vecs.iter() {
        if fresh_ids.iter().any(|f| f == id) {
            continue;
        }
        if let Some(arr) = v.as_array() {
            let vec: Vec<f32> = arr.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
            let src = prev.srcs.get(id).and_then(Value::as_str).map(String::from);
            out.push((id.clone(), vec, src));
        }
    }
    out
}

fn load_prev(model: &Path, game: &str, lang: &str) -> Option<Prev> {
    let raw = std::fs::read_to_string(meta_path(model, game, lang)).ok()?;
    let meta: Value = serde_json::from_str(&raw).ok()?;
    let ids: Vec<String> =
        meta.get("ids")?.as_array()?.iter().map(|v| v.as_str().unwrap_or_default().to_string()).collect();
    let dim = meta.get("dim")?.as_u64()? as usize;
    let srcs = meta.get("srcs").and_then(Value::as_object).cloned().unwrap_or_default();
    let bin = std::fs::read(bin_path(model, game, lang)).ok()?;
    let mut vecs = Map::new();
    for (i, id) in ids.iter().enumerate() {
        let off = i * dim * 4;
        if off + dim * 4 > bin.len() {
            break;
        }
        let v: Vec<Value> = bin[off..off + dim * 4]
            .chunks_exact(4)
            .map(|c| Value::from(f32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64))
            .collect();
        vecs.insert(id.clone(), Value::Array(v));
    }
    Some(Prev { vecs, srcs })
}

// ---------------------------------------------------------------------------
// start() + the job body
// ---------------------------------------------------------------------------

/// JS ToString for interpolated JSON values ('unknown game ${game}').
pub(crate) fn js_to_string_pub(v: Option<&Value>) -> String {
    js_to_string(v)
}

fn js_to_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                if f == f.trunc() && f.is_finite() {
                    format!("{f:.0}")
                } else {
                    n.to_string()
                }
            }
        }
        Some(_) => "[object Object]".into(),
    }
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

pub struct StartOpts {
    pub skip_cache: bool,
    pub keep_going: bool,
    pub sets: Vec<String>,
}

/// Everything the spawned job needs — built from AppState by the route, or
/// directly by the weekly job thread (which owns its own Scry client like
/// the other scry_jobs timers).
pub struct JobCore {
    pub db: PathBuf,
    pub model: Option<PathBuf>,
    pub scry: std::sync::Arc<Scry>,
    pub memo: ScryfallSetsMemo,
}

/// catalog.js `start(game, lang, opts)` — Ok(pre-spawn state snapshot) or
/// Err(message) for the route's 409.
pub fn start(core: JobCore, langs: &[LangEntry], game_v: Option<&Value>, lang_v: Option<&Value>, opts: StartOpts) -> Result<Value, String> {
    // Node checks `current` first, then the game — order is observable.
    let mut cur = CURRENT.lock().unwrap();
    if cur.is_some() {
        return Err("a catalog build is already running".into());
    }
    let game_owned;
    let game: &str = match game_v {
        Some(Value::String(s)) => s.as_str(),
        _ => {
            game_owned = js_to_string(game_v);
            &game_owned
        }
    };
    if game != "mtg" {
        return Err(format!("unknown game {game}"));
    }
    let lang = {
        let n = to_name(langs, lang_v.and_then(Value::as_str));
        if n.is_empty() { "English".to_string() } else { n }
    };
    let job = Job {
        game: game.to_string(),
        lang,
        sets: opts.sets.iter().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect(),
        phase: "starting".into(),
        done: 0,
        total: 0,
        message: String::new(),
        started_at: now_ms(),
        cancelled: false,
    };
    // Reply snapshot taken BEFORE publishing current — Node's start()
    // returns synchronously while the IIFE has not reached its first await.
    let reply = job.to_value();
    *cur = Some(job);
    drop(cur);

    let JobCore { db, model, scry, memo } = core;
    let langs = langs.to_vec();
    let skip_cache = opts.skip_cache;
    let keep_going = opts.keep_going;
    tokio::spawn(async move {
        run_job(db, model, scry, memo, langs, skip_cache, keep_going).await;
    });
    Ok(reply)
}

/// server.js `autoUpdateCatalogs` twin — runs at the weekly sets-refresh tick
/// (scry_jobs::weekly_loop); one behind-catalog kick per tick.
pub fn auto_update_tick(db: &Path, scry: std::sync::Arc<Scry>, memo: ScryfallSetsMemo) {
    if std::env::var("CATALOG_AUTO_UPDATE").ok().as_deref() == Some("0") {
        return;
    }
    if state().is_object() {
        return; // a build is already running
    }
    let shared = std::env::var_os("SCRYBOX_SHARED_DIR").map(std::path::PathBuf::from);
    let Some(langs) = crate::catalog_read::load_languages_dir(shared.as_deref()) else {
        return;
    };
    let conn = match rusqlite::Connection::open(db) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Catalog auto-update check failed: {e}");
            return;
        }
    };
    let model = cvscan::model_dir_pub();
    let rows = match list(&conn, model.as_deref(), "mtg") {
        Ok(Value::Array(rows)) => rows,
        Ok(_) => return,
        Err(e) => {
            tracing::error!("Catalog auto-update check failed: {e}");
            return;
        }
    };
    for row in rows {
        let built = row.get("built").filter(|b| !b.is_null()).is_some();
        let new_sets = row.get("newSets").and_then(Value::as_u64).unwrap_or(0);
        if !built || new_sets == 0 {
            continue;
        }
        let game = row.get("game").and_then(Value::as_str).unwrap_or("mtg").to_string();
        let lang = row.get("lang").and_then(Value::as_str).unwrap_or("English").to_string();
        tracing::info!("catalog: {game}/{lang} is {new_sets} set(s) behind — updating (CATALOG_AUTO_UPDATE=0 to disable)");
        let game_v = Value::String(game.clone());
        let lang_v = Value::String(lang.clone());
        let _ = start(
            JobCore { db: db.to_path_buf(), model, scry, memo },
            &langs,
            Some(&game_v),
            Some(&lang_v),
            StartOpts { skip_cache: false, keep_going: false, sets: vec![] },
        );
        return; // one at a time; the next tick picks up the next one
    }
}

struct JobCtx {
    game: String,
    lang: String,
    sets: Vec<String>,
    model: PathBuf,
    scry: std::sync::Arc<Scry>,
    memo: ScryfallSetsMemo,
    langs: Vec<LangEntry>,
}

async fn run_job(
    db: PathBuf,
    model: Option<PathBuf>,
    scry: std::sync::Arc<Scry>,
    memo: ScryfallSetsMemo,
    langs: Vec<LangEntry>,
    skip_cache: bool,
    keep_going: bool,
) {
    let result: Result<String, String> = async {
        let model = model.ok_or("no CV_MODEL_DIR")?;
        let job = CURRENT.lock().unwrap().clone().ok_or("job vanished")?;
        let ctx = JobCtx { game: job.game, lang: job.lang, sets: job.sets, model, scry, memo, langs };

        let cached = if skip_cache {
            CacheSummary { cards: 0, gaps: None, failed: 0 }
        } else {
            cache_phase(&db, &ctx).await
        };
        let embedded = if cancelled() && !keep_going {
            None
        } else {
            Some(embed_phase(&db, &ctx).await?)
        };
        let skipped = match cached.gaps {
            Some(g) if g > 0 => format!(", {g} set(s) skipped — no card data upstream"),
            _ => String::new(),
        };
        Ok(match &embedded {
            Some(e) => format!(
                "{} cards embedded ({} new, {} reused, {} failed){skipped}",
                e.rows.unwrap_or(0),
                e.built,
                e.reused,
                e.failed
            ),
            None => format!("cached {} cards{skipped}", cached.cards),
        })
    }
    .await;

    match result {
        Ok(msg) => with_current(|j| {
            j.phase = if j.cancelled { "cancelled".into() } else { "done".into() };
            j.message = msg;
        }),
        Err(e) => {
            tracing::error!("catalog: {} job failed: {e}", "mtg");
            with_current(|j| {
                j.phase = "error".into();
                j.message = e;
            });
        }
    }
    // finally: last = state(); current = null.
    let snap = state();
    let mut cur = CURRENT.lock().unwrap();
    *LAST.lock().unwrap() = Some(snap);
    *cur = None;
}

struct CacheSummary {
    cards: usize,
    gaps: Option<usize>,
    #[allow(dead_code)]
    failed: usize,
}

/// catalog.js `cachePhase` — provider fill of card_cache for every set.
/// The Connection is opened per synchronous leg, never held across an
/// await (rusqlite is !Send).
async fn cache_phase(db: &Path, ctx: &JobCtx) -> CacheSummary {
    let sets: Vec<String> = if !ctx.sets.is_empty() {
        ctx.sets.clone()
    } else {
        list_all_sets(&ctx.scry, &ctx.memo).await
    };
    with_current(|j| {
        j.phase = "cache".into();
        j.total = sets.len();
        j.done = 0;
    });
    let mut cards = 0usize;
    let mut gaps = 0usize;
    let mut failed = 0usize;
    let ex: Exclusions = {
        match rusqlite::Connection::open(db) {
            Ok(c) => scan_exclusions(&c),
            Err(_) => Exclusions { tokens: false, art_cards: false, jumpstart: false, promos: false },
        }
    };
    for set in &sets {
        if cancelled() {
            return CacheSummary { cards, gaps: Some(gaps), failed };
        }
        with_current(|j| j.message = set.clone());
        match crate::card_sets::fetch_set_cards(&ctx.scry, &ctx.memo, set, &ctx.lang, &ctx.langs, ex).await {
            Ok((fetched, code)) => {
                cards += fetched.len();
                if let Ok(conn) = rusqlite::Connection::open(db) {
                    let _ = conn.busy_timeout(std::time::Duration::from_millis(5000));
                    crate::card_sets::cache_set_rows(&conn, &fetched, &code);
                    let _ = conn.execute(
                        "DELETE FROM set_data_gaps WHERE language = ?1 AND set_id = ?2",
                        rusqlite::params![ctx.lang, set.to_lowercase()],
                    );
                }
            }
            Err(CacheErr::Absent(msg)) => {
                gaps += 1;
                let reason: String = msg.chars().take(200).collect();
                if let Ok(conn) = rusqlite::Connection::open(db) {
                    let _ = conn.busy_timeout(std::time::Duration::from_millis(5000));
                    let _ = conn.execute(
                        "INSERT OR REPLACE INTO set_data_gaps (language, set_id, reason) VALUES (?1, ?2, ?3)",
                        rusqlite::params![ctx.lang, set.to_lowercase(), reason],
                    );
                }
            }
            Err(CacheErr::Failed(_)) => failed += 1,
        }
        with_current(|j| j.done += 1);
    }
    CacheSummary { cards, gaps: Some(gaps), failed }
}

struct EmbedSummary {
    built: usize,
    reused: usize,
    failed: usize,
    #[allow(dead_code)]
    wrote: bool,
    rows: Option<usize>,
}

#[derive(Clone)]
struct ImgRow {
    id: String,
    image_url: String,
}

/// catalog.js `embedPhase` — including the finally-reload placement: session
/// creation sits OUTSIDE Node's try (builder failure skips the reload), the
/// early empty-rows return sits before it (no reload either).
async fn embed_phase(db: &Path, ctx: &JobCtx) -> Result<EmbedSummary, String> {
    let scoped = !ctx.sets.is_empty();
    let rows: Vec<ImgRow> = {
        let conn = rusqlite::Connection::open(db).map_err(|e| e.to_string())?;
        let _ = conn.busy_timeout(std::time::Duration::from_millis(5000));
        let sql = if scoped {
            let marks = ctx.sets.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            format!("SELECT id, image_url FROM card_cache WHERE language = ?1 AND image_url IS NOT NULL AND image_url != '' AND LOWER(set_id) IN ({marks}) ORDER BY id")
        } else {
            "SELECT id, image_url FROM card_cache WHERE language = ?1 AND image_url IS NOT NULL AND image_url != '' ORDER BY id".to_string()
        };
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let binds: Vec<String> = if scoped {
            let mut b = vec![ctx.lang.clone()];
            b.extend(ctx.sets.iter().cloned());
            b
        } else {
            vec![ctx.lang.clone()]
        };
        let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let mapped = stmt
            .query_map(refs.as_slice(), |r| {
                Ok(ImgRow { id: r.get::<_, String>(0)?, image_url: r.get::<_, String>(1)? })
            })
            .map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        for row in mapped {
            v.push(row.map_err(|e| e.to_string())?);
        }
        v
    };
    with_current(|j| {
        j.phase = "embed".into();
        j.total = rows.len();
        j.done = 0;
    });
    if rows.is_empty() {
        return Ok(EmbedSummary { built: 0, reused: 0, failed: 0, wrote: false, rows: None });
    }

    let session = cvscan::build_milo_session()?;

    let reload_tail = || cvscan::reload(&ctx.game, &ctx.lang);

    let prev = load_prev(&ctx.model, &ctx.game, &ctx.lang);
    let mut ids: Vec<String> = Vec::new();
    let mut vecs: Vec<Vec<f32>> = Vec::new();
    let mut srcs: Map<String, Value> = prev.as_ref().map(|p| p.srcs.clone()).unwrap_or_default();
    let mut built = 0usize;
    let mut reused = 0usize;
    let mut failed = 0usize;
    let total = rows.len();

    // queue / inflight / pump — Node's 8-wide Promise.race.
    let mut queue: VecDeque<ImgRow> = rows.iter().cloned().collect();
    let mut inflight: tokio::task::JoinSet<(String, String, Result<Vec<u8>, String>)> =
        tokio::task::JoinSet::new();
    macro_rules! pump {
        () => {
            while !cancelled() && inflight.len() < 8 {
                let Some(row) = queue.pop_front() else { break };
                let embed_url = row.image_url.clone();
                let hit = prev.as_ref().and_then(|p| {
                    p.vecs.get(&row.id).and_then(Value::as_array).filter(|_| {
                        p.srcs.get(&row.id).and_then(Value::as_str).map(|s| s == embed_url).unwrap_or(false)
                    })
                });
                if let Some(arr) = hit {
                    ids.push(row.id.clone());
                    vecs.push(arr.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect());
                    srcs.insert(row.id.clone(), Value::String(embed_url));
                    reused += 1;
                    with_current(|j| j.done += 1);
                    with_current(|j| j.message = format!("{}/{total}", built + reused));
                    continue;
                }
                let id = row.id.clone();
                let url = embed_url.clone();
                inflight.spawn(async move { (id, url.clone(), fetch_image(url).await) });
            }
        };
    }
    pump!();
    while let Some(settled) = inflight.join_next().await {
        let (id, url, res) = match settled {
            Ok(v) => v,
            Err(_) => (String::new(), String::new(), Err("join error".into())),
        };
        match res {
            Err(_) => failed += 1,
            Ok(buf) => match embed_one(&session, &buf) {
                Err(_) => failed += 1,
                Ok(v) => {
                    ids.push(id.clone());
                    vecs.push(v);
                    srcs.insert(id, Value::String(url));
                    built += 1;
                }
            },
        }
        with_current(|j| j.done += 1);
        with_current(|j| j.message = format!("{}/{total}", built + reused));
        pump!();
    }

    if scoped {
        if let Some(p) = prev.as_ref() {
            for (id, v, src) in kept_from_prev(p, &ids) {
                ids.push(id.clone());
                vecs.push(v);
                if let Some(s) = src {
                    srcs.insert(id, Value::String(s));
                }
                reused += 1;
            }
        }
    }

    if vecs.is_empty() {
        reload_tail();
        return Ok(EmbedSummary { built, reused, failed, wrote: false, rows: None });
    }
    let dim = vecs[0].len();
    let mut bin = Vec::with_capacity(vecs.len() * dim * 4);
    for v in &vecs {
        for f in v {
            bin.extend_from_slice(&f.to_le_bytes());
        }
    }
    let _ = std::fs::create_dir_all(&ctx.model);
    let mut meta = Map::new();
    meta.insert("dim".into(), (dim as u64).into());
    meta.insert("ids".into(), ids.iter().map(|s| Value::String(s.clone())).collect::<Vec<_>>().into());
    meta.insert("srcs".into(), Value::Object(srcs));
    meta.insert("game".into(), ctx.game.clone().into());
    meta.insert("lang".into(), ctx.lang.clone().into());
    meta.insert("model".into(), "milo1".into());
    meta.insert("builtAt".into(), iso_now().into());
    let bin_p = bin_path(&ctx.model, &ctx.game, &ctx.lang);
    let meta_p = meta_path(&ctx.model, &ctx.game, &ctx.lang);
    std::fs::write(format!("{}.tmp", bin_p.display()), &bin).map_err(|e| e.to_string())?;
    std::fs::write(format!("{}.tmp", meta_p.display()), serde_json::to_vec(&Value::Object(meta)).unwrap())
        .map_err(|e| e.to_string())?;
    std::fs::rename(format!("{}.tmp", bin_p.display()), &bin_p).map_err(|e| e.to_string())?;
    std::fs::rename(format!("{}.tmp", meta_p.display()), &meta_p).map_err(|e| e.to_string())?;
    reload_tail();
    Ok(EmbedSummary { built, reused, failed, wrote: true, rows: Some(ids.len()) })
}

fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// ---------------------------------------------------------------------------
// image fetch (catalog.js fetch: UA + Accept image/*, follow redirect, 30 s)
// ---------------------------------------------------------------------------

type ImgClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    String,
>;

fn image_client() -> &'static ImgClient {
    use std::sync::OnceLock;
    static C: OnceLock<ImgClient> = OnceLock::new();
    C.get_or_init(|| {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("native roots")
            .https_or_http()
            .enable_http1()
            .build();
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .build(https)
    })
}

async fn fetch_image(url: String) -> Result<Vec<u8>, String> {
    let fut = async {
        let client = image_client();
        let mut url = url;
        for _ in 0..10 {
            let uri = url.parse::<Uri>().map_err(|e| e.to_string())?;
            let req = hyper::Request::builder()
                .method(hyper::Method::GET)
                .uri(uri)
                .header(header::USER_AGENT, HeaderValue::from_static(SCRYBOX_UA))
                .header(header::ACCEPT, HeaderValue::from_static("image/*"))
                .body(String::new())
                .map_err(|e| e.to_string())?;
            let res = client.request(req).await.map_err(|e| e.to_string())?;
            let status = res.status();
            if status.is_redirection() {
                if let Some(loc) = res.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()) {
                    url = join_uri(&url, loc);
                    continue;
                }
            }
            if !status.is_success() {
                return Err(format!("HTTP {status}"));
            }
            let bytes = res.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
            return Ok(bytes.to_vec());
        }
        Err("too many redirects".to_string())
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), fut)
        .await
        .map_err(|_| "timeout".to_string())?
}

/// Resolve a Location header against the request URL (fetch's
/// redirect:'follow' does this for free; hyper does not).
pub(crate) fn join_uri(base: &str, loc: &str) -> String {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    let Some(rest) = base.find("://") else { return loc.to_string() };
    let scheme = &base[..rest + 3];
    let after = &base[rest + 3..];
    let auth = after.find('/').map(|i| &after[..i]).unwrap_or(after);
    if loc.starts_with('/') {
        return format!("{scheme}{auth}{loc}");
    }
    let path = &after[auth.len()..];
    let dir = path.rfind('/').map(|i| &path[..i + 1]).unwrap_or("");
    format!("{scheme}{auth}{dir}{loc}")
}

/// sharp(buf).resize(448,448,{fit:'fill'}).removeAlpha().raw() → embed.
fn embed_one(session: &std::sync::Mutex<ort::session::Session>, buf: &[u8]) -> Result<Vec<f32>, String> {
    let rgb = cvscan::decode_rgb(buf)?;
    let filled = cvscan::resize_fill(&rgb, 448, 448);
    cvscan::embed_raw(session, &filled.px)
}

// ---------------------------------------------------------------------------
// Routes (admin.js catalogs block)
// ---------------------------------------------------------------------------

fn json_error(status: StatusCode, msg: &str) -> Response {
    let mut m = Map::new();
    m.insert("error".into(), msg.into());
    json_response(status, &Value::Object(m))
}

/// GET /api/admin/catalogs + /progress + POST /build + POST /stop.
pub async fn handle_catalogs(State(st): State<AppState>, mut req: Request) -> Response {
    if !(st.db.is_some() && (st.cfg.rust_routes.contains("*") || st.cfg.rust_routes.contains("catalog_build"))) {
        return crate::proxy(State(st), req).await;
    }
    let path = req.uri().path().to_string();
    let is_get = req.method() == axum::http::Method::GET;
    let is_post_build = req.method() == axum::http::Method::POST && path == "/api/admin/catalogs/build";

    // server.js chain: normalJson (1 mb strict) runs BEFORE the admin
    // auth gate — parse first, Forward the untouched request on a parse
    // failure (Node's SyntaxError/413 page is authoritative).
    let body_val: Value = if is_post_build {
        let (mut parts, body) = req.into_parts();
        // Capped at normalJson's 1mb (REVIEW-auth.md H1): content-length
        // above the cap answers the parser's 413 here for JSON (Node's
        // normalJson runs before the auth gate); non-JSON skips the parser
        // (empty body, cleared content-length); a chunked stream error
        // past the cap forwards body-less for Node's own verdict.
        let cl_over_cap = parts
            .headers
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
            .map(|n| n > crate::collection_writes::BODY_NORMAL)
            .unwrap_or(false);
        let bytes = match axum::body::to_bytes(body, crate::collection_writes::BODY_NORMAL + 1).await {
            Ok(b) => b,
            Err(_) if cl_over_cap && crate::collection_writes::is_json_ct(&parts.headers) => {
                return crate::collection_writes::too_large();
            }
            Err(_) if cl_over_cap => {
                parts.headers.remove(axum::http::header::CONTENT_LENGTH);
                axum::body::Bytes::new()
            }
            Err(_) => {
                parts.headers.remove(axum::http::header::CONTENT_LENGTH);
                return crate::proxy(State(st), Request::from_parts(parts, Body::empty())).await;
            }
        };
        match parse_outer_json(&parts.headers, &bytes) {
            Ok(v) => {
                req = Request::from_parts(parts, Body::from(bytes.to_vec()));
                v
            }
            Err(()) => {
                let fwd = Request::from_parts(parts, Body::from(bytes.to_vec()));
                return crate::proxy(State(st), fwd).await;
            }
        }
    } else if req.method() == axum::http::Method::POST || req.method() == axum::http::Method::PUT {
        // POST/PUT to stop (or anything else): Node's router ignores the
        // body there (express.json still ran — empty parse), forward-safe.
        // Capped read (REVIEW-auth.md H1) — same 413/skip arms as above;
        // Node's parser runs BEFORE the router, so even body-ignoring
        // routes 413 on oversize JSON.
        let (mut parts, body) = req.into_parts();
        let cl_over_cap = parts
            .headers
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
            .map(|n| n > crate::collection_writes::BODY_NORMAL)
            .unwrap_or(false);
        let bytes = match axum::body::to_bytes(body, crate::collection_writes::BODY_NORMAL + 1).await {
            Ok(b) => b,
            Err(_) if cl_over_cap && crate::collection_writes::is_json_ct(&parts.headers) => {
                return crate::collection_writes::too_large();
            }
            Err(_) if cl_over_cap => {
                parts.headers.remove(axum::http::header::CONTENT_LENGTH);
                axum::body::Bytes::new()
            }
            Err(_) => {
                parts.headers.remove(axum::http::header::CONTENT_LENGTH);
                return crate::proxy(State(st), Request::from_parts(parts, Body::empty())).await;
            }
        };
        match parse_outer_json(&parts.headers, &bytes) {
            Ok(v) => {
                req = Request::from_parts(parts, Body::from(bytes.to_vec()));
                v
            }
            Err(()) => {
                let fwd = Request::from_parts(parts, Body::from(bytes.to_vec()));
                return crate::proxy(State(st), fwd).await;
            }
        }
    } else {
        Value::Object(Map::new())
    };

    // router.use(authenticateToken, requireAdmin) — 401/403 before route
    // matching, so before the method dispatch too.
    let pool = st.db.clone().unwrap();
    let headers = req.headers().clone();
    let token = bearer_token(&headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), is_get) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    if user.via_api_key {
        return json_error(
            StatusCode::FORBIDDEN,
            "API keys cannot access admin endpoints. Use a login session.",
        );
    }
    if user.role.as_deref() != Some("admin") {
        return json_error(StatusCode::FORBIDDEN, "Access denied. Administrator privileges required.");
    }

    let langs = crate::catalog_read::load_languages(&st);

    match (req.method().clone(), path.as_str()) {
        (axum::http::Method::GET, "/api/admin/catalogs") => {
            let conn = match open_conn(&st) {
                Ok(c) => c,
                Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not list catalogs"),
            };
            let model = st.model_dir.clone().or_else(cvscan::model_dir_pub);
            match list(&conn, model.as_deref(), "mtg") {
                Ok(catalogs) => {
                    let mut m = Map::new();
                    m.insert("catalogs".into(), catalogs);
                    m.insert("progress".into(), state());
                    m.insert("last".into(), last_result());
                    json_response(StatusCode::OK, &Value::Object(m))
                }
                Err(e) => {
                    tracing::error!("catalog list failed: {e}");
                    json_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not list catalogs")
                }
            }
        }
        (axum::http::Method::GET, "/api/admin/catalogs/progress") => {
            let mut m = Map::new();
            m.insert("progress".into(), state());
            m.insert("last".into(), last_result());
            json_response(StatusCode::OK, &Value::Object(m))
        }
        (axum::http::Method::POST, "/api/admin/catalogs/build") => {
            let Some(langs) = langs else {
                let (parts, _) = req.into_parts();
                return crate::proxy(State(st), Request::from_parts(parts, Body::empty())).await;
            };
            let sets = body_val
                .get("sets")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|v| js_to_string(Some(v))).collect())
                .unwrap_or_default();
            let opts = StartOpts { skip_cache: truthy(body_val.get("skipCache")), keep_going: false, sets };
            let core = JobCore {
                db: st.cfg.db_path.clone().unwrap(),
                model: st.model_dir.clone().or_else(cvscan::model_dir_pub),
                scry: st.scry.clone(),
                memo: st.scryfall_sets.clone(),
            };
            match start(core, &langs, body_val.get("game"), body_val.get("lang"), opts) {
                Ok(progress) => {
                    let mut m = Map::new();
                    m.insert("progress".into(), progress);
                    json_response(StatusCode::OK, &Value::Object(m))
                }
                Err(e) => {
                    let mut m = Map::new();
                    m.insert("error".into(), e.into());
                    m.insert("progress".into(), state());
                    json_response(StatusCode::CONFLICT, &Value::Object(m))
                }
            }
        }
        (axum::http::Method::POST, "/api/admin/catalogs/stop") => {
            let mut m = Map::new();
            m.insert("stopped".into(), stop().into());
            m.insert("progress".into(), state());
            json_response(StatusCode::OK, &Value::Object(m))
        }
        _ => {
            // Any other method/path on this prefix: Node's router owns the 404.
            let (parts, _) = req.into_parts();
            crate::proxy(State(st), Request::from_parts(parts, Body::empty())).await
        }
    }
}

fn open_conn(st: &AppState) -> Result<rusqlite::Connection, String> {
    let p = st.cfg.db_path.as_ref().ok_or("no db")?;
    let conn = rusqlite::Connection::open(p).map_err(|e| e.to_string())?;
    conn.busy_timeout(std::time::Duration::from_millis(5000)).map_err(|e| e.to_string())?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// unit pins (Lane B unit 6c)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seed_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE card_cache (id TEXT, set_id TEXT, language TEXT, image_url TEXT);
             CREATE TABLE sets (id TEXT, total INTEGER);
             CREATE TABLE set_data_gaps (language TEXT, set_id TEXT, reason TEXT);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn js_to_string_matches_js_semantics() {
        assert_eq!(js_to_string(None), "undefined");
        assert_eq!(js_to_string(Some(&Value::Null)), "null");
        assert_eq!(js_to_string(Some(&json!("x"))), "x");
        assert_eq!(js_to_string(Some(&json!(true))), "true");
        assert_eq!(js_to_string(Some(&json!(7))), "7");
        assert_eq!(js_to_string(Some(&json!(7.0))), "7");
        assert_eq!(js_to_string(Some(&json!(7.5))), "7.5");
        assert_eq!(js_to_string(Some(&json!({"a":1}))), "[object Object]");
    }

    #[test]
    fn truthy_matches_js() {
        assert!(!truthy(None));
        assert!(!truthy(Some(&Value::Null)));
        assert!(!truthy(Some(&json!(0))));
        assert!(!truthy(Some(&json!(""))));
        assert!(truthy(Some(&json!("0"))));
        assert!(truthy(Some(&json!([]))));
        assert!(truthy(Some(&json!({}))));
    }

    #[test]
    fn join_uri_resolves_like_fetch() {
        assert_eq!(join_uri("https://hf.co/a/b", "https://cdn.x/c"), "https://cdn.x/c");
        assert_eq!(join_uri("https://hf.co/a/b", "/c/d"), "https://hf.co/c/d");
        assert_eq!(join_uri("https://hf.co/a/b", "e"), "https://hf.co/a/e");
    }

    #[test]
    fn kept_from_prev_keeps_prev_order_and_drops_fresh() {
        let mut vecs = Map::new();
        vecs.insert("a".into(), json!([1.0, 2.0]));
        vecs.insert("b".into(), json!([3.0, 4.0]));
        vecs.insert("c".into(), json!([5.0, 6.0]));
        let mut srcs = Map::new();
        srcs.insert("a".into(), json!("ua"));
        srcs.insert("c".into(), json!("uc"));
        let prev = Prev { vecs, srcs };
        let kept = kept_from_prev(&prev, &["b".to_string()]);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].0, "a");
        assert_eq!(kept[0].1, vec![1.0, 2.0]);
        assert_eq!(kept[0].2.as_deref(), Some("ua"));
        assert_eq!(kept[1].0, "c");
    }

    #[test]
    fn new_set_count_skips_cached_and_gapped_and_strips_mtg_prefix() {
        let conn = seed_conn();
        conn.execute_batch(
            "INSERT INTO sets (id, total) VALUES ('mtg-xyz', 10), ('abc', 5), ('zero', 0);
             INSERT INTO card_cache (id, set_id, language, image_url) VALUES ('x1', 'xyz', 'English', 'u');
             INSERT INTO set_data_gaps (language, set_id, reason) VALUES ('English', 'abc', 'none');",
        )
        .unwrap();
        assert_eq!(new_set_count(&conn, "mtg", "English"), Some(0));
        // The gap no longer applies once the gap row is gone (fresh conn so
        // the recorded gap does not reappear).
        let conn2 = seed_conn();
        conn2.execute_batch(
            "INSERT INTO sets (id, total) VALUES ('mtg-xyz', 10), ('abc', 5), ('zero', 0);
             INSERT INTO card_cache (id, set_id, language, image_url) VALUES ('x1', 'xyz', 'English', 'u');",
        )
        .unwrap();
        assert_eq!(new_set_count(&conn2, "mtg", "English"), Some(1));
        assert_eq!(new_set_count(&conn, "pokemon", "English"), Some(0));
    }

    #[test]
    fn list_rows_key_order_and_claimed_position() {
        let conn = seed_conn();
        conn.execute(
            "INSERT INTO card_cache (id, set_id, language, image_url) VALUES ('j1', 'xyz', 'Japanese', 'u')",
            [],
        )
        .unwrap();
        let v = list(&conn, None, "mtg").unwrap();
        let rows = v.as_array().unwrap();
        assert_eq!(rows.len(), 2);
        let keys: Vec<&str> = rows[0]
            .as_object()
            .unwrap()
            .keys()
            .map(|s| s.as_str())
            .collect();
        assert_eq!(keys, ["game", "lang", "built", "cached", "withArt", "newSets", "published"]);
        assert_eq!(rows[0]["lang"], "English");
        assert!(rows[0].get("claimed").is_none());
        assert_eq!(rows[1]["lang"], "Japanese");

        // English group row gets claimed appended LAST.
        conn.execute(
            "INSERT INTO card_cache (id, set_id, language, image_url) VALUES ('e1', 'xyz', 'English', NULL)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO sets (id, total) VALUES ('xyz', 3)", []).unwrap();
        let v = list(&conn, None, "mtg").unwrap();
        let rows = v.as_array().unwrap();
        let en = &rows[0];
        assert_eq!(en["lang"], "English");
        assert_eq!(en["cached"], 1);
        assert_eq!(en["withArt"], 0);
        let keys: Vec<&str> = en.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        assert_eq!(keys.last().copied(), Some("claimed"));
        assert_eq!(en["claimed"], 3);
    }

    #[test]
    fn state_and_stop_when_idle() {
        reset_for_test();
        assert_eq!(state(), Value::Null);
        assert_eq!(last_result(), Value::Null);
        assert!(!stop());
    }
}

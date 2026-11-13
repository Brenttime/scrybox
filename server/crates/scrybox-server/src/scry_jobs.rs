//! Rust ports of the two Scryfall background jobs (Lane B unit 1):
//! `fetchAndCacheSets` (set sync) and `updateCollectionPrices` (price
//! sweep) from `backend/src/scryfallApi.js`, plus the server.js timer
//! schedule. Gated by `RUST_JOBS=scryfall`; when a job is enabled here the
//! Node twin in the test container is disabled via
//! SCRYBOX_DISABLE_SETS_REFRESH / SCRYBOX_DISABLE_PRICE_SWEEP (server.js).

use rusqlite::{params, Connection};
use serde_json::{Map, Value};

use crate::card_cache::{
    cache_normalized_cards, mark_prices_swept, record_price, refresh_all_estimates,
    should_sweep_prices, UNPRICED_SQL,
};
use crate::card_norm::{normalize_card, scryfall_uuid};
use crate::scryfall::{Scry, ScryEnv, ScryOpts};

/// Scryfall's bulk lookup takes at most 75 identifiers per request.
const COLLECTION_BATCH: usize = 75;

/// `fetchAndCacheSets(force)`: pull the Scryfall /sets list into the shared
/// `sets` table. Skips when already populated unless forced. Set ids carry
/// the `mtg-` prefix so a Scryfall code is unambiguous on the PK.
pub async fn fetch_and_cache_sets(conn: &Connection, scry: &Scry, force: bool) -> Result<usize, String> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sets", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if !force && count > 0 {
        tracing::info!("MTG sets already populated ({count} sets). Skipping fetch.");
        return Ok(0);
    }
    tracing::info!("Fetching sets from Scryfall...");
    let resp = scry.get("/sets", &ScryOpts::default()).await.map_err(|e| e.to_string())?;
    let sets = resp
        .as_ref()
        .and_then(|v| v.get("data"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for s in &sets {
        let g = |k: &str| s.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let count_num = s.get("card_count").and_then(|v| v.as_i64()).unwrap_or(0);
        let code = g("code");
        conn.execute(
            "INSERT OR REPLACE INTO sets (id, name, series, printed_total, total, release_date, set_code, symbol_url, logo_url)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                format!("mtg-{code}"),
                g("name"),
                g("set_type"),
                count_num,
                count_num,
                g("released_at"),
                code,
                g("icon_svg_uri"),
                g("icon_svg_uri")
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    tracing::info!("Cached {} MTG sets.", sets.len());
    Ok(sets.len())
}

/// A sweep row: the card_id being priced plus its identity columns.
#[derive(Debug, Clone)]
pub struct SweepRow {
    pub card_id: String,
    pub set_id: String,
    pub number: String,
    pub name: String,
}

const SWEEP_SQL: &str = r#"
  WITH deck_card_keys AS (
    SELECT DISTINCT LOWER(TRIM(COALESCE(deck_cc.name, ''))) AS card_key
    FROM deck_cards dc
    JOIN card_cache deck_cc ON deck_cc.id = dc.card_id
    WHERE dc.quantity > 0
  )
  SELECT DISTINCT c.card_id, cc.set_id, cc.number, cc.name
  FROM collection c
  JOIN card_cache cc ON c.card_id = cc.id
  WHERE c.quantity > 0
  UNION
  SELECT DISTINCT cc.id AS card_id, cc.set_id, cc.number, cc.name
  FROM card_cache cc INDEXED BY idx_card_cache_logical_key
  JOIN deck_card_keys deck_key ON deck_key.card_key = LOWER(TRIM(COALESCE(cc.name, '')))
"#;

/// The sweep row set (exposed for tests; same query the job runs).
pub fn sweep_rows(conn: &Connection) -> rusqlite::Result<Vec<SweepRow>> {
    let mut stmt = conn.prepare(SWEEP_SQL)?;
    let rows = stmt.query_map([], |r| {
        Ok(SweepRow {
            card_id: r.get::<_, String>(0)?,
            set_id: r.get::<_, Option<String>>(1).ok().flatten().unwrap_or_default(),
            number: r.get::<_, Option<String>>(2).ok().flatten().unwrap_or_default(),
            name: r.get::<_, Option<String>>(3).ok().flatten().unwrap_or_default(),
        })
    })?;
    rows.collect()
}

/// Outcome of a sweep, for logs and the DB-diff harness.
#[derive(Debug, Default)]
pub struct SweepOutcome {
    pub swept: bool,
    pub asked: usize,
    pub priced: usize,
    pub not_found: usize,
    pub estimates_changed: usize,
    pub floors_filled: usize,
}

/// `bulkFetchByIdentifier(rows)` — POST /cards/collection in batches of 75
/// against the REAL API (sidecar or not: only Scryfall carries live
/// TCGplayer prices). Match by `id:<uuid>`, `sn:<set>:<num>`, `n:<name>`
/// exactly like Node, so a row never lands on another printing's price.
async fn bulk_fetch_by_identifier(
    scry: &Scry,
    rows: &[SweepRow],
) -> Result<(Vec<Map<String, Value>>, Vec<(SweepRow, Map<String, Value>)>, usize), String> {
    let mut cards: Vec<Map<String, Value>> = Vec::new();
    let mut pairs: Vec<(SweepRow, Map<String, Value>)> = Vec::new();
    let mut not_found = 0usize;
    for chunk in rows.chunks(COLLECTION_BATCH) {
        // Node's exact contract: identifiers are OBJECTS — {id} | {set,
        // collector_number} | {name}. Strings are rejected by the real API,
        // so the mock mirrors that too (see tests/mock_scryfall.rs).
        let identifiers: Vec<Value> = chunk
            .iter()
            .map(|r| {
                if let Some(u) = scryfall_uuid(Some(&r.card_id)) {
                    serde_json::json!({ "id": u })
                } else if !r.set_id.is_empty() && !r.number.is_empty() {
                    serde_json::json!({ "set": r.set_id.to_lowercase(), "collector_number": r.number })
                } else {
                    serde_json::json!({ "name": r.name })
                }
            })
            .collect();
        let resp = scry
            .post_retried(
                "/cards/collection",
                Value::Object({
                    let mut m = Map::new();
                    m.insert("identifiers".into(), Value::Array(identifiers));
                    m
                }),
                &ScryOpts { upstream: true, ..Default::default() },
                // Node's scryPostRetried default retries = 4: re-queue on 429,
                // each retry lining up behind the shared cooldown.
                4,
            )
            .await
            .map_err(|e| e.to_string())?;
        let data = resp
            .as_ref()
            .and_then(|v| v.get("data"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let fresh: Vec<Map<String, Value>> =
            data.iter().map(|raw| normalize_card(raw, None)).filter_map(|v| v.as_object().cloned()).collect();
        cards.extend(fresh.iter().cloned());

        // Node's pairing: byKey is keyed from the REQUEST rows (id:/sn:/n:),
        // and each response card resolves its row through the same fallback
        // chain — id:, then sn: (norm set|number), then n: (norm name).
        let mut by_key: Vec<(String, &SweepRow)> = Vec::new();
        for row in chunk {
            if let Some(u) = scryfall_uuid(Some(&row.card_id)) {
                by_key.push((format!("id:{}", u.to_lowercase()), row));
            }
            if !row.set_id.is_empty() && !row.number.is_empty() {
                by_key.push((format!("sn:{}|{}", row.set_id.to_lowercase(), row.number.to_lowercase()), row));
            }
            by_key.push((format!("n:{}", row.name.to_lowercase()), row));
        }
        for (raw, norm) in data.iter().zip(fresh.iter()) {
            let key_id = raw.get("id").and_then(|v| v.as_str()).map(|s| format!("id:{}", s.to_lowercase()));
            let key_sn = format!(
                "sn:{}|{}",
                norm.get("set_id").and_then(|v| v.as_str()).unwrap_or("").to_lowercase(),
                norm.get("number").and_then(|v| v.as_str()).unwrap_or("").to_lowercase()
            );
            let key_n = format!("n:{}", norm.get("name").and_then(|v| v.as_str()).unwrap_or("").to_lowercase());
            let row = key_id
                .as_deref()
                .and_then(|k| by_key.iter().find(|(bk, _)| bk == k))
                .or_else(|| by_key.iter().find(|(bk, _)| bk == &key_sn))
                .or_else(|| by_key.iter().find(|(bk, _)| bk == &key_n))
                .map(|(_, row)| (*row).clone());
            if let Some(row) = row {
                pairs.push((row, norm.clone()));
            }
        }
        not_found += resp
            .as_ref()
            .and_then(|v| v.get("not_found"))
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
    }
    Ok((cards, pairs, not_found))
}

/// `updateCollectionPrices(force)`: refresh every owned printing plus every
/// cached printing of a logical card used by a deck (deck minimums compare
/// alternate printings). The hourly tick asks UNFORCED — `shouldSweepPrices`
/// decides from the admin's price_refresh_days.
pub async fn update_collection_prices(
    conn: &Connection,
    scry: &Scry,
    force: bool,
) -> Result<SweepOutcome, String> {
    let cards = sweep_rows(conn).map_err(|e| e.to_string())?;
    if cards.is_empty() {
        return Ok(SweepOutcome::default());
    }
    if !force && !should_sweep_prices(conn, "mtg") {
        tracing::info!("Skipping MTG price update: already swept within the interval (Scryfall updates prices daily).");
        return Ok(SweepOutcome::default());
    }
    tracing::info!("Starting MTG price update for {} unique cards...", cards.len());
    // Verified /cards/collection contract: { data, not_found }.
    let (fresh, pairs, not_found) = match bulk_fetch_by_identifier(scry, &cards).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("MTG price update failed: {e}");
            return Err(e);
        }
    };
    if !fresh.is_empty() {
        cache_normalized_cards(conn, &fresh).map_err(|e| e.to_string())?;
    }
    for (row, card) in &pairs {
        let price = card.get("price_trend").and_then(|v| v.as_f64()).unwrap_or(0.0);
        record_price(conn, &row.card_id, price).map_err(|e| e.to_string())?;
    }
    mark_prices_swept(conn, "mtg").map_err(|e| e.to_string())?;
    tracing::info!("MTG price update complete: {} priced, {not_found} not found on Scryfall.", pairs.len());
    let outcome = refresh_price_estimates(conn, scry).await?;
    Ok(SweepOutcome {
        swept: true,
        asked: cards.len(),
        priced: pairs.len(),
        not_found,
        estimates_changed: outcome.estimates_changed,
        floors_filled: outcome.floors_filled,
    })
}

/// `refreshPriceEstimates()`: fill floorless owned cards from the real API,
/// then re-estimate the whole cache.
pub async fn refresh_price_estimates(conn: &Connection, scry: &Scry) -> Result<SweepOutcome, String> {
    let floors_filled = fill_missing_floors(conn, scry).await?;
    let estimates_changed = refresh_all_estimates(conn).map_err(|e| e.to_string())?;
    tracing::info!("Price estimates: {floors_filled} cards looked up upstream, {estimates_changed} rows estimated.");
    Ok(SweepOutcome { floors_filled, estimates_changed, ..Default::default() })
}

/// `cardsMissingFloor(db, 150)`: printings the user holds that are unpriced
/// AND have no priced sibling cached.
fn cards_missing_floor(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(&format!(
        r#"WITH used AS (
             SELECT card_id FROM collection WHERE quantity > 0
             UNION SELECT card_id FROM deck_cards WHERE quantity > 0
             UNION SELECT card_id FROM list_cards WHERE quantity > 0
           )
           SELECT DISTINCT cc.oracle_id, LOWER(TRIM(COALESCE(cc.name, ''))) AS card_key
           FROM used JOIN card_cache cc ON cc.id = used.card_id
           WHERE cc.oracle_id IS NOT NULL AND {UNPRICED_SQL}
             AND COALESCE(cc.price_estimated, 0) = 0
           LIMIT ?1"#
    ))?;
    let rows = stmt.query_map(params![limit], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    rows.collect()
}

/// `fillMissingFloors`: pull every printing of each floorless card from the
/// real API (sidecar serves no prices), cache, then estimate. Paced by the
/// shared queue; 1500 ms between lookups like Node's `gapMs`. Remembered per
/// process (askedOracle) so a never-priced card is not re-asked per sweep.
async fn fill_missing_floors(conn: &Connection, scry: &Scry) -> Result<usize, String> {
    let missing: Vec<(String, String)> = cards_missing_floor(conn, 150)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|(oid, _)| !scry.asked_oracle_taken(oid))
        .collect();
    if missing.is_empty() {
        return Ok(0);
    }
    let mut asked = 0usize;
    let gap_ms = if scry.env().local { 0u64 } else { 1500 };
    for (oid, _key) in &missing {
        let mut url = format!(
            "/cards/search?q={}&unique=prints&include_extras=true",
            urlencode(&format!("oracleid:{oid}"))
        );
        let mut page = 0;
        let mut failed = false;
        while !url.is_empty() && page < 5 {
            if gap_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(gap_ms)).await;
            }
            match scry.get(&url, &ScryOpts { upstream: true, ..Default::default() }).await {
                Ok(resp) => {
                    let data = resp
                        .as_ref()
                        .and_then(|v| v.get("data"))
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let cards: Vec<Map<String, Value>> = data
                        .iter()
                        .map(|raw| normalize_card(raw, None))
                        .filter_map(|v| v.as_object().cloned())
                        .filter(|c| matches!(c.get("price_source"), Some(Value::String(_))))
                        .collect();
                    if !cards.is_empty() {
                        cache_normalized_cards(conn, &cards).map_err(|e| e.to_string())?;
                    }
                    let has_more = resp
                        .as_ref()
                        .and_then(|v| v.get("has_more"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    url = if has_more {
                        resp.as_ref()
                            .and_then(|v| v.get("next_page"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string()
                    } else {
                        String::new()
                    };
                }
                Err(e) if e.is_status(404) => {
                    scry.mark_asked_oracle(oid);
                    failed = true;
                    break;
                }
                Err(e) => {
                    // Rate-limited or down: stop and let the next sweep pick
                    // up the rest (Node breaks the whole loop, not the card).
                    tracing::error!("price floor lookup stopped at {oid}: {e}");
                    return Ok(asked);
                }
            }
            page += 1;
        }
        if !failed {
            asked += 1;
            scry.mark_asked_oracle(oid);
        }
    }
    let keys: Vec<String> = missing.iter().map(|(_, k)| k.clone()).collect();
    crate::card_cache::estimate_keys(conn, &keys).map_err(|e| e.to_string())
}

/// Minimal percent-encoding for the Scryfall query parameter.
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Scheduler (server.js timer twin). Started from main only when
// RUST_JOBS=scryfall; Node's twin timers are disabled via the
// SCRYBOX_DISABLE_* env flags in the test container.
// ---------------------------------------------------------------------------

/// Spawn the job timers: weekly sets refresh, hourly (gated) price sweep,
/// and the boot catch-up 30 s after start. `db_path` is the same
/// SCRYBOX_DB_PATH copy the routes read. The jobs hold a rusqlite
/// Connection across awaits (Scryfall fetches in the middle of a write
/// transaction sequence), so they live on a dedicated thread with a
/// current-thread runtime + LocalSet — the future is !Send by design and
/// never crosses threads.
pub fn start_job_timers(db_path: std::path::PathBuf) {
    std::thread::Builder::new()
        .name("scry-jobs".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("jobs runtime");
            let local = tokio::task::LocalSet::new();
            local.spawn_local(boot_sets_sync(db_path.clone()));
            local.spawn_local(weekly_loop(db_path.clone()));
            local.spawn_local(hourly_loop(db_path.clone()));
            local.spawn_local(boot_catchup(db_path));
            rt.block_on(local);
        })
        .expect("spawn scry-jobs thread");
}

fn open_conn(db: &std::path::Path) -> Result<Connection, String> {
    let conn = Connection::open(db).map_err(|e| e.to_string())?;
    conn.busy_timeout(std::time::Duration::from_millis(5000)).map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Node's startup `await fetchAndCacheSets()` (unforced): a fresh install
/// gets its sets without waiting for the weekly tick. Node paid this before
/// listen; here it is a scheduled job so bind never waits on Scryfall.
async fn boot_sets_sync(db: std::path::PathBuf) {
    let scry = Scry::new(ScryEnv::from_env());
    match open_conn(&db) {
        Ok(conn) => {
            if let Err(e) = fetch_and_cache_sets(&conn, &scry, false).await {
                tracing::error!("Boot sets sync failed: {e}");
            }
        }
        Err(e) => tracing::error!("Boot sets sync: db open failed: {e}"),
    }
}

async fn weekly_loop(db: std::path::PathBuf) {
    // Weekly sets refresh (picks up newly released sets) + the
    // autoUpdateCatalogs twin (Lane B unit 6c: one behind-catalog build kick
    // per tick; the Node weekly timer is switched off for this leg with
    // CATALOG_AUTO_UPDATE=0 on the test container).
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(7 * 24 * 3600)).await;
        let scry = Scry::new(ScryEnv::from_env());
        match open_conn(&db) {
            Ok(conn) => {
                if let Err(e) = fetch_and_cache_sets(&conn, &scry, true).await {
                    tracing::error!("Weekly sets refresh failed: {e}");
                }
            }
            Err(e) => tracing::error!("Weekly sets refresh: db open failed: {e}"),
        }
        crate::catalog_build::auto_update_tick(
            &db,
            std::sync::Arc::new(Scry::new(ScryEnv::from_env())),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
        );
    }
}

async fn sweep_once(db: &std::path::Path, force: bool) {
    let scry = Scry::new(ScryEnv::from_env());
    match open_conn(db) {
        Ok(conn) => {
            if let Err(e) = update_collection_prices(&conn, &scry, force).await {
                tracing::error!("MTG price update failed: {e}");
            }
        }
        Err(e) => tracing::error!("MTG price update: db open failed: {e}"),
    }
}

async fn hourly_loop(db: std::path::PathBuf) {
    // Hourly price sweep tick, NOT forced: shouldSweepPrices decides from the
    // admin's price_refresh_days (the daily timer this replaced made that
    // setting dead code — server.js comment, ported verbatim).
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        sweep_once(&db, false).await;
    }
}

async fn boot_catchup(db: std::path::PathBuf) {
    // Boot catch-up 30 s in (NOT forced, so a sweep inside the interval
    // costs one indexed read). Node runs oracleTags.runMaintenance first —
    // that job belongs to the catalog unit and still runs on Node.
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    sweep_once(&db, false).await;
}

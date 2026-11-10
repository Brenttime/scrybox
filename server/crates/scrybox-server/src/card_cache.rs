//! Ports of `utils/cardCache.js` (cacheNormalizedCards), the price parts of
//! `utils/priceHelpers.js` (recordPrice, shouldSweepPrices, markPricesSwept,
//! priceRefreshDays) and `utils/priceEstimates.js` (planEstimates,
//! estimateKeys, refreshEstimatesForIds, refreshAllEstimates).

use rusqlite::{params, Connection};
use serde_json::{Map, Value};

/// cardCache.js COLUMNS — the one place that knows card_cache's column list.
/// Upsert order; `last_updated` is appended as a SQL keyword, never a bind.
const COLUMNS: [&str; 27] = [
    "id",
    "oracle_id",
    "scryfall_search_eligible",
    "name",
    "supertype",
    "subtypes",
    "types",
    "rarity",
    "set_id",
    "set_name",
    "number",
    "image_url",
    "price_trend",
    "price_normal",
    "price_holofoil",
    "price_etched",
    "cmc",
    "color_identity",
    "language",
    "printed_name",
    "tcgplayer_url",
    "cardmarket_url",
    "tcgplayer_product_id",
    "price_currency",
    "price_source",
    "oracle_text",
    "oracle_faces",
];

const PRICE_COLUMNS: [&str; 6] = [
    "price_trend",
    "price_normal",
    "price_holofoil",
    "price_etched",
    "price_currency",
    "price_source",
];

/// cardCache.js NAME_CLAUSE (verbatim): a localized name may never clobber
/// an English one learned from another printing.
const NAME_CLAUSE: &str = r#"name = CASE
    WHEN excluded.name = excluded.printed_name AND card_cache.name <> card_cache.printed_name
    THEN card_cache.name ELSE excluded.name END"#;

/// cardCache.js ESTIMATE_CLAUSE: a priced write replaces any estimate with
/// its real quote; a price-less write leaves the estimate and its flag.
const ESTIMATE_CLAUSE: &str =
    "price_estimated = CASE WHEN excluded.price_source IS NULL THEN card_cache.price_estimated ELSE 0 END";

fn set_clause() -> String {
    COLUMNS
        .iter()
        .filter(|c| **c != "id" && **c != "name")
        .map(|c| {
            if matches!(
                *c,
                "oracle_id" | "scryfall_search_eligible" | "oracle_text" | "oracle_faces"
            ) {
                // Partial set/catalog rows may not carry identity or layout
                // eligibility — never erase durable values.
                format!("{c} = COALESCE(excluded.{c}, card_cache.{c})")
            } else if PRICE_COLUMNS.contains(c) {
                // A row with no price_source came from a price-less source:
                // keep the last real quote rather than zero it.
                format!("{c} = CASE WHEN excluded.price_source IS NULL THEN card_cache.{c} ELSE excluded.{c} END")
            } else {
                format!("{c} = excluded.{c}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// cardCache.js `num(v)`: `v == null || v === '' || NaN → null`, else Number.
fn num(v: Option<&Value>) -> Option<f64> {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => None,
        Some(Value::Bool(true)) => Some(1.0),
        Some(Value::Number(n)) => n.as_f64().filter(|f| !f.is_nan()),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                t.parse::<f64>().ok().filter(|f| !f.is_nan())
            }
        }
        Some(_) => None,
    }
}

fn bind_text_or(card: &Map<String, Value>, key: &str, default: &str) -> rusqlite::types::Value {
    match card.get(key) {
        Some(Value::String(s)) if !s.is_empty() => rusqlite::types::Value::Text(s.clone()),
        Some(Value::Number(n)) => rusqlite::types::Value::Text(n.to_string()),
        Some(Value::Bool(true)) => rusqlite::types::Value::Text("true".into()),
        _ => rusqlite::types::Value::Text(default.to_string()),
    }
}

fn bind_opt_text(card: &Map<String, Value>, key: &str) -> rusqlite::types::Value {
    match card.get(key) {
        Some(Value::String(s)) if !s.is_empty() => rusqlite::types::Value::Text(s.clone()),
        Some(Value::Number(n)) => rusqlite::types::Value::Text(n.to_string()),
        _ => rusqlite::types::Value::Null,
    }
}

/// `JSON.stringify(x || [])` — a falsy/missing value stringifies to `[]`.
fn bind_json_array(card: &Map<String, Value>, key: &str) -> rusqlite::types::Value {
    match card.get(key) {
        None | Some(Value::Null) | Some(Value::Bool(false)) => rusqlite::types::Value::Text("[]".into()),
        Some(Value::String(s)) if s.is_empty() => rusqlite::types::Value::Text("[]".into()),
        Some(v) => rusqlite::types::Value::Text(v.to_string()),
    }
}

fn bind_num(card: &Map<String, Value>, key: &str) -> rusqlite::types::Value {
    match num(card.get(key)) {
        None => rusqlite::types::Value::Null,
        // Integral values bind as INTEGER so boolean-ish columns
        // (scryfall_search_eligible) compare as 0/1 everywhere.
        Some(f) if f.fract() == 0.0 && f.abs() < i64::MAX as f64 => {
            rusqlite::types::Value::Integer(f as i64)
        }
        Some(f) => rusqlite::types::Value::Real(f),
    }
}

/// One normalized card bound in COLUMNS order (the values cacheNormalizedCards
/// pushes per row).
fn bind_row(c: &Map<String, Value>) -> Vec<rusqlite::types::Value> {
    let mut row = Vec::with_capacity(COLUMNS.len());
    row.push(rusqlite::types::Value::Text(
        c.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    ));
    row.push(bind_opt_text(c, "oracle_id"));
    row.push(bind_num(c, "scryfall_search_eligible"));
    row.push(bind_text_or(c, "name", ""));
    row.push(bind_text_or(c, "supertype", ""));
    row.push(bind_json_array(c, "subtypes"));
    row.push(bind_json_array(c, "types"));
    row.push(bind_text_or(c, "rarity", "Common"));
    row.push(bind_text_or(c, "set_id", ""));
    row.push(bind_text_or(c, "set_name", ""));
    row.push(bind_text_or(c, "number", ""));
    row.push(bind_text_or(c, "image_url", ""));
    row.push(bind_num(c, "price_trend"));
    row.push(bind_num(c, "price_normal"));
    row.push(bind_num(c, "price_holofoil"));
    row.push(bind_num(c, "price_etched"));
    row.push(bind_num(c, "cmc"));
    row.push(bind_json_array(c, "color_identity"));
    row.push(bind_text_or(c, "language", "English"));
    row.push(bind_opt_text(c, "printed_name"));
    row.push(bind_opt_text(c, "tcgplayer_url"));
    row.push(bind_opt_text(c, "cardmarket_url"));
    row.push(bind_num(c, "tcgplayer_product_id"));
    row.push(bind_text_or(c, "price_currency", "USD"));
    row.push(bind_opt_text(c, "price_source"));
    row.push(match c.get("oracle_text") {
        None | Some(Value::Null) => rusqlite::types::Value::Null,
        Some(Value::String(s)) => rusqlite::types::Value::Text(s.clone()),
        Some(v) => rusqlite::types::Value::Text(v.to_string()),
    });
    row.push(match c.get("oracle_faces") {
        None | Some(Value::Null) => rusqlite::types::Value::Null,
        Some(Value::String(s)) => rusqlite::types::Value::Text(s.clone()),
        Some(v) => rusqlite::types::Value::Text(v.to_string()),
    });
    row
}

/// A page of results can be 250 cards, so rows go in batched multi-row
/// UPSERTs — chunked so bound-parameter count stays well inside SQLite's
/// limit (cardCache.js CHUNK = 50).
const CHUNK: usize = 50;

/// `cacheNormalizedCards(cards)` — the ONLY writer of card rows (faithful
/// port): an UPSERT, never INSERT OR REPLACE and never DELETE — columns this
/// list does not name (and durable identity columns) are preserved by the
/// ON CONFLICT clause, and a price-less source cannot stamp $0 over a real
/// quote.
pub fn cache_normalized_cards(conn: &Connection, cards: &[Map<String, Value>]) -> rusqlite::Result<()> {
    let rows: Vec<&Map<String, Value>> = cards
        .iter()
        .filter(|c| {
            c.get("id")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        })
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    let set_clause = set_clause();
    let cols = COLUMNS.join(", ");
    let row_sql = format!(
        "({}, CURRENT_TIMESTAMP)",
        COLUMNS.iter().map(|_| "?").collect::<Vec<_>>().join(", ")
    );
    for chunk in rows.chunks(CHUNK) {
        let mut params: Vec<rusqlite::types::Value> =
            Vec::with_capacity(chunk.len() * COLUMNS.len());
        for c in chunk {
            params.extend(bind_row(c));
        }
        let values = vec![row_sql.as_str(); chunk.len()].join(", ");
        let ps: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|v| v as &dyn rusqlite::types::ToSql).collect();
        conn.execute(
            &format!(
                "INSERT INTO card_cache ({cols}, last_updated)
                 VALUES {values}
                 ON CONFLICT(id) DO UPDATE SET {NAME_CLAUSE}, {set_clause}, {ESTIMATE_CLAUSE}, last_updated = CURRENT_TIMESTAMP"
            ),
            ps.as_slice(),
        )?;
    }
    // Re-estimate the touched logical cards (Node logs and continues).
    let ids: Vec<String> = rows
        .iter()
        .filter_map(|c| c.get("id").and_then(|v| v.as_str()).map(String::from))
        .collect();
    if let Err(e) = refresh_estimates_for_ids(conn, &ids) {
        tracing::error!("price estimate refresh failed: {e}");
    }
    Ok(())
}

/// `recordPrice(cardId, price)` — insert ONLY on movement (the sweep runs on
/// a timer and would otherwise append an identical row every tick).
pub fn record_price(conn: &Connection, card_id: &str, price: f64) -> rusqlite::Result<bool> {
    if card_id.is_empty() || !price.is_finite() || price <= 0.0 {
        return Ok(false);
    }
    let latest: Option<String> = conn
        .query_row(
            "SELECT recorded_at FROM price_history WHERE card_id = ?1 ORDER BY recorded_at DESC LIMIT 1",
            params![card_id],
            |row| row.get(0),
        )
        .ok();
    if let Some(at) = &latest {
        if let Some(prev) = conn
            .query_row(
                "SELECT price FROM price_history WHERE card_id = ?1 AND recorded_at = ?2",
                params![card_id, at],
                |row| row.get::<_, f64>(0),
            )
            .ok()
        {
            if prev == price {
                return Ok(false);
            }
        }
    }
    // recorded_at is the PK: equal-second inserts must still be distinct.
    let mut recorded_at = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    if let Some(at) = &latest {
        if at.as_str() >= recorded_at.as_str() {
            // Parse "YYYY-MM-DD HH:MM:SS.mmm" and step past the latest row.
            if let Ok(naive) =
                chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S%.3f")
            {
                recorded_at = (naive + chrono::Duration::milliseconds(1))
                    .format("%Y-%m-%d %H:%M:%S%.3f")
                    .to_string();
            }
        }
    }
    conn.execute(
        "INSERT INTO price_history (card_id, price, recorded_at) VALUES (?1, ?2, ?3)",
        params![card_id, price, recorded_at],
    )?;
    Ok(true)
}

fn price_refresh_days(conn: &Connection) -> f64 {
    let days: Option<String> = conn
        .query_row(
            &format!("SELECT price_refresh_days FROM app_settings WHERE id = 1"),
            [],
            |row| row.get(0),
        )
        .ok();
    // Number(v ?? 1): NULL/'' → 0/NaN → treated like 1 unless explicitly 0.
    match days {
        None => 1.0,
        Some(s) => {
            let n: f64 = s.trim().parse().unwrap_or(0.0);
            if n == 0.0 {
                1.0
            } else {
                n
            }
        }
    }
}

/// `shouldSweepPrices(game)`: automatic (hourly) sweeps only.
pub fn should_sweep_prices(conn: &Connection, game: &str) -> bool {
    let days = price_refresh_days(conn);
    if days <= 0.0 {
        return false;
    }
    let swept_at: Option<String> = match conn.query_row(
        &format!("SELECT {game}_prices_swept_at FROM app_settings WHERE id = 1"),
        [],
        |row| row.get(0),
    ) {
        Ok(v) => v,
        // Unknown game (no such column): Node's db.get rejects and the
        // caller never sweeps — same net effect here.
        Err(_) => return false,
    };
    match swept_at {
        None => true,
        Some(s) => match parse_sqlite_utc(&s) {
            None => true,
            Some(at) => {
                let age_days = (chrono::Utc::now() - at).num_seconds() as f64 / 86_400.0;
                age_days >= days
            }
        },
    }
}

/// SQLite CURRENT_TIMESTAMP is naive UTC; JS-side writes are ISO with an
/// explicit offset. Parse both, treating the naive form as UTC.
pub fn parse_sqlite_utc(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(t) {
        return Some(dt.with_timezone(&Utc));
    }
    let naive = NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f"))
        .ok()?;
    Some(Utc.from_utc_datetime(&naive))
}

/// `markPricesSwept(game)` — record a manual sweep so the hourly tick skips
/// until the user's interval elapses.
pub fn mark_prices_swept(conn: &Connection, game: &str) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "UPDATE app_settings SET {game}_prices_swept_at = CURRENT_TIMESTAMP WHERE id = 1"
        ),
        [],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// priceEstimates.js
// ---------------------------------------------------------------------------

const FINISH_FIELDS: [&str; 4] = ["price_normal", "price_holofoil", "price_etched", "price_trend"];

fn js_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn js_number(v: Option<&Value>) -> f64 {
    match v {
        None | Some(Value::Null) => 0.0,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(f64::NAN),
        Some(Value::Bool(true)) => 1.0,
        Some(Value::Bool(false)) => 0.0,
        Some(_) => f64::NAN,
    }
}

fn is_usd(row: &Map<String, Value>) -> bool {
    match row.get("price_currency") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.to_uppercase() == "USD",
        Some(v) => v.to_string().to_uppercase() == "USD",
    }
}

/// `realFloor(row)`: cheapest positive USD price on a row carrying a REAL
/// quote (an estimated row is never a floor for a sibling).
pub fn real_floor(row: &Map<String, Value>) -> Option<f64> {
    if js_truthy(row.get("price_estimated")) || !is_usd(row) {
        return None;
    }
    let mut best: Option<f64> = None;
    for f in FINISH_FIELDS {
        let v = js_number(row.get(f));
        if v.is_finite() && v > 0.0 {
            best = Some(best.map_or(v, |b: f64| b.min(v)));
        }
    }
    best
}

/// `planEstimates(rows)` (pure): each row is read with its `card_key`.
struct EstUpdate {
    id: String,
    price_trend: f64,
    price_estimated: i64,
}

fn plan_estimates(rows: &[Map<String, Value>]) -> Vec<EstUpdate> {
    let mut floor_by_key: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for r in rows {
        let Some(p) = real_floor(r) else { continue };
        let key = r.get("card_key").and_then(|v| v.as_str()).unwrap_or("").to_string();
        match floor_by_key.get(&key) {
            None => {
                floor_by_key.insert(key, p);
            }
            Some(&c) if p < c => {
                floor_by_key.insert(key, p);
            }
            _ => {}
        }
    }
    let mut updates = Vec::new();
    for r in rows {
        if real_floor(r).is_some() {
            continue; // hasRealQuote
        }
        let id = match r.get("id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let key = r.get("card_key").and_then(|v| v.as_str()).unwrap_or("").to_string();
        match floor_by_key.get(&key) {
            Some(&floor) => {
                if !js_truthy(r.get("price_estimated")) || js_number(r.get("price_trend")) != floor {
                    updates.push(EstUpdate { id, price_trend: floor, price_estimated: 1 });
                }
            }
            // The printing the estimate came from is gone or lost its quote.
            None => {
                if js_truthy(r.get("price_estimated")) {
                    updates.push(EstUpdate { id, price_trend: 0.0, price_estimated: 0 });
                }
            }
        }
    }
    updates
}

/// estimateKeys.js KEY_CHUNK.
const KEY_CHUNK: usize = 400;

/// Row shape estimateKeys reads per sibling (Node's select list, in order).
const ESTIMATE_COLS: [&str; 8] = [
    "card_key",
    "id",
    "price_trend",
    "price_normal",
    "price_holofoil",
    "price_etched",
    "price_currency",
    "price_estimated",
];

fn rows_to_maps(
    conn: &Connection,
    sql: &str,
    ps: &[&dyn rusqlite::types::ToSql],
    cols: &[&'static str],
) -> rusqlite::Result<Vec<Map<String, Value>>> {
    let mut stmt = conn.prepare(sql)?;
    let mut out = Vec::new();
    let mut rows = stmt.query(ps)?;
    while let Some(row) = rows.next()? {
        let mut m = Map::new();
        for (i, c) in cols.iter().enumerate() {
            let v: rusqlite::types::Value = row.get(i)?;
            m.insert(
                (*c).into(),
                match v {
                    rusqlite::types::Value::Null => Value::Null,
                    rusqlite::types::Value::Integer(n) => Value::from(n),
                    rusqlite::types::Value::Real(f) => Value::from(f),
                    rusqlite::types::Value::Text(s) => Value::String(s),
                    rusqlite::types::Value::Blob(_) => Value::Null,
                },
            );
        }
        out.push(m);
    }
    Ok(out)
}

/// `estimateKeys(keys)` — per logical card: borrow the cheapest real quote
/// or clear a stranded estimate.
pub fn estimate_keys(conn: &Connection, keys: &[String]) -> rusqlite::Result<usize> {
    let mut unique: Vec<String> = Vec::new();
    for k in keys {
        if !k.is_empty() && !unique.iter().any(|u| u == k) {
            unique.push(k.clone());
        }
    }
    let mut changed = 0usize;
    for chunk in unique.chunks(KEY_CHUNK) {
        let ps: Vec<&dyn rusqlite::types::ToSql> =
            chunk.iter().map(|k| k as &dyn rusqlite::types::ToSql).collect();
        // INDEXED BY (Node verbatim): drives the lookup off the expression
        // index; without it SQLite full-scans card_cache per batch.
        let sql = format!(
            "WITH k(card_key) AS (VALUES {values})
             SELECT {key_expr} AS card_key, cc.id, cc.price_trend, cc.price_normal,
                    cc.price_holofoil, cc.price_etched, cc.price_currency, cc.price_estimated
             FROM k CROSS JOIN card_cache cc INDEXED BY idx_card_cache_logical_key
             WHERE {key_expr} = k.card_key",
            values = chunk.iter().map(|_| "(?)").collect::<Vec<_>>().join(","),
            key_expr = card_key_sql("cc"),
        );
        let rows = rows_to_maps(conn, &sql, &ps, &ESTIMATE_COLS)?;
        for u in plan_estimates(&rows) {
            // Node's UPDATE verbatim: only price_trend moves; the currency
            // becomes USD only when the row becomes an estimate.
            conn.execute(
                "UPDATE card_cache SET price_trend = ?1, price_estimated = ?2,
                   price_currency = CASE WHEN ?2 = 1 THEN 'USD' ELSE price_currency END
                 WHERE id = ?3",
                params![u.price_trend, u.price_estimated, u.id],
            )?;
            changed += 1;
        }
    }
    Ok(changed)
}

/// `sqlCardKey(alias)` without an alias: the logical-card key.
pub fn card_key_sql(col: &str) -> String {
    format!("LOWER(TRIM(COALESCE({col}.name, '')))")
}

/// `refreshEstimatesForIds(ids)` — re-estimate the logical cards behind a
/// batch of cache-write ids.
pub fn refresh_estimates_for_ids(conn: &Connection, ids: &[String]) -> rusqlite::Result<usize> {
    let mut list: Vec<String> = Vec::new();
    for id in ids {
        if !id.is_empty() && !list.iter().any(|u| u == id) {
            list.push(id.clone());
        }
    }
    if list.is_empty() {
        return Ok(0);
    }
    let mut keys: Vec<String> = Vec::new();
    for chunk in list.chunks(KEY_CHUNK) {
        let ps: Vec<&dyn rusqlite::types::ToSql> =
            chunk.iter().map(|k| k as &dyn rusqlite::types::ToSql).collect();
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT {key_expr} AS card_key FROM card_cache cc
             WHERE cc.id IN ({placeholders})",
            key_expr = card_key_sql("cc"),
            placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",")
        ))?;
        let rows = stmt.query_map(ps.as_slice(), |row| row.get::<_, String>(0))?;
        for r in rows {
            keys.push(r?);
        }
    }
    estimate_keys(conn, &keys)
}

/// priceEstimates.js UNPRICED_SQL (cc alias, verbatim).
pub const UNPRICED_SQL: &str = "(COALESCE(cc.price_estimated, 0) = 1 OR UPPER(COALESCE(cc.price_currency, 'USD')) <> 'USD' OR (
  COALESCE(cc.price_trend, 0) <= 0 AND COALESCE(cc.price_normal, 0) <= 0
  AND COALESCE(cc.price_holofoil, 0) <= 0 AND COALESCE(cc.price_etched, 0) <= 0))";

/// `refreshAllEstimates()` — every logical card with an unpriced or
/// estimated printing (boot catch-up after a restart).
pub fn refresh_all_estimates(conn: &Connection) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare(&format!(
        "SELECT DISTINCT {key_expr} AS card_key FROM card_cache cc WHERE {UNPRICED_SQL}",
        key_expr = card_key_sql("cc")
    ))?;
    let keys = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    estimate_keys(conn, &keys)
}

//! LANE A — iter-38: export group (route switch `export`).
//! Port of `backend/src/routes/importExport.js` (+ its utils/csvExporters,
//! utils/csvMappers, utils/priceHelpers legs).
//!
//!   GET  /api/export?format=&ecosystem=     whole collection as CSV/JSON
//!   POST /api/export {entry_ids,...}        "current view" subset
//!   POST /api/import {format,data}          CSV/JSON payload → collection
//!
//! Byte-parity surface (BRIEF rule 1): the CSV/JSON attachment bytes
//! themselves, `Content-Type` (express `res.send` appends charset=utf-8 to
//! the header sendExport sets), `Content-Disposition`, Express's default
//! body ETag (+ 304 replay) and the JSON key ORDER of the /import replies.
//!
//! Node quirks ported VERBATIM (they are the contract; all were re-derived
//! from the running container, not from the file's comments):
//!   * exportRows drops `entry_id`, `price_trend`, `price_normal` and
//!     `price_holofoil` from every row (destructure-rest, order PRESERVED —
//!     serde_json's Map::remove is swap_remove, so we filter-construct);
//!     `market_price` is appended LAST.
//!   * resolveCardPrice (utils/priceHelpers.js) is proxy-first: `is_proxy`
//!     TRUTHY → 0 regardless of prices; else Holofoil/Normal pick their
//!     per-printing price when `!== null && > 0`; else `price_trend || 0`.
//!     It NEVER returns undefined — the JSON export always carries
//!     market_price. The internal CSV strategy reads
//!     `price_trend || market_price || 0` on the destructured rows, so the
//!     resolved number wins there too.
//!   * Import validation reads fields straight off the parsed item — for
//!     CSV that is the ONCE-strategy-mapped row (parseThirdPartyCSV runs at
//!     parse time only; there is NO second mapping pass). Any single
//!     rejected row ABORTS the whole import (`No rows were imported.`) —
//!     all-or-nothing. Uncached rows must resolve through Scryfall; a row
//!     with no usable identifier is rejected with
//!     `"card has no MTG identifier Scryfall can validate"`. Only
//!     bulk-resolved matches go through cacheCards — a cached row is used
//!     verbatim and NEVER rewritten.
//!   * `item.game`/`item.printing`/every INSERT column is read from the
//!     parsed item (`item.quantity || 1`, …) — for JSON that is the raw
//!     body item; for CSV the once-mapped strategy row.
//!   * CSV parser (route-local): `"` toggles quote mode and is CONSUMED
//!     (never emitted), `,` splits outside quotes, every field is trimmed,
//!     then each header/value runs `replace(/^"|"$/g,'')` — a no-op after
//!     quote consumption but ported anyway.
//!   * `lookup` (the Scryfall identifier) reads `item.set_code||item.set_id`
//!     and `item.collector_number||item.number` — snake_case only.
//!
//! Forward policy: any answer whose bytes Node could not produce identically
//! from the Rust side replays the WHOLE untouched request to Node BEFORE any
//! local write — a SQLite error on the card_cache probe/tx (the sqlite3
//! error text is driver-specific), a Scryfall client error from the shared
//! bulk resolver, a body value Node's driver would reject at bind time, and
//! the JSON-mode `JSON.parse(data)` failure (V8's message text is
//! unreproducible from serde). V8-deterministic throws (null item,
//! `format.toLowerCase`) are answered natively with the pinned message.

use std::collections::HashMap;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
};
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value};

use crate::auth::{authenticate, bearer_token};
use crate::card_cache::cache_normalized_cards;
use crate::collection::{body_etag, etag_matches, gzip_bytes, truthy, wants_encoding};
use crate::collection_writes::json_to_sql_value;
use crate::decks_reads::{fresh_allows_304, rows_map};
use crate::lists_reads::{qs_get, QsVal};
use crate::precons::{bulk_fetch_by_identifier, js_string, js_trim, num_out};
use crate::stats;
use crate::AppState;

/// bigJson (server.js) — mounted for /api/import AFTER the auth gate.
const BIG_BODY_LIMIT: usize = 15 * 1024 * 1024;
/// normalJson — mounted before the gate, so /api/export's POST cap.
const NORMAL_BODY_LIMIT: usize = 1024 * 1024;

/// routes/importExport.js EXPORT_SELECT — kept verbatim (same plan ⇒ same
/// unordered result on the same DB for the full-collection read).
const EXPORT_SELECT: &str = "
      SELECT
        c.id as entry_id,
        c.quantity,
        c.condition,
        c.printing,
        c.language,
        c.purchase_price,
        c.added_at,
        c.is_proxy,
        cc.id as card_id,
        cc.name as name,
        cc.supertype,
        cc.types,
        cc.rarity,
        cc.set_id as set_code,
        cc.set_name,
        cc.number as collector_number,
        cc.image_url,
        cc.price_trend,
        cc.price_normal,
        cc.price_holofoil
      FROM collection c
      JOIN card_cache cc ON c.card_id = cc.id
      WHERE c.user_id = ?";

// ---------------------------------------------------------------------------
// JS value helpers local to this group
// ---------------------------------------------------------------------------

/// First truthy value among `keys`, else `fallback` (`a || b || c`).
fn orf(item: &Map<String, Value>, keys: &[&str], fallback: Value) -> Value {
    for k in keys {
        if let Some(v) = item.get(*k) {
            if truthy(v) {
                return v.clone();
            }
        }
    }
    fallback
}

/// `String(v)` where a MISSING property is 'undefined' (js_string renders
/// the present-value forms; undefined has no Value).
fn js_to_string_opt(v: Option<&Value>) -> String {
    match v {
        Some(v) => js_string(v),
        None => "undefined".into(),
    }
}

/// `String(v || '')`.
fn str_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(v) if truthy(v) => js_string(v),
        _ => String::new(),
    }
}

/// `parseInt(s, 10)` over `String(v)`; None = NaN (overflow included —
/// `crate::util`'s non-saturating arm, matching the old local copy).
fn js_parse_int_str(v: &Value) -> Option<i64> {
    crate::util::js_parse_int_nonsat(&js_string(v))
}

/// `parseFloat(s) || 0` for the (unused-in-the-write-path) CSV strategy.
fn js_parse_float(v: &str) -> Option<f64> {
    let t = v.trim_start();
    let b = t.as_bytes();
    let mut i = 0usize;
    let neg = i < b.len() && b[i] == b'-';
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    if t[i..].starts_with("Infinity") {
        return Some(if neg { f64::NEG_INFINITY } else { f64::INFINITY });
    }
    let d0 = i;
    let mut end = i;
    while end < b.len() && (b[end] as char).is_ascii_digit() {
        end += 1;
    }
    if end < b.len() && b[end] == b'.' {
        let dot = end;
        end += 1;
        let f0 = end;
        while end < b.len() && (b[end] as char).is_ascii_digit() {
            end += 1;
        }
        if end == f0 && end == dot + 1 && dot == d0 {
            end = dot;
        }
    }
    if end == d0 && (end == d0) {
        // no digits after optional sign
        return None;
    }
    if end < b.len() && (b[end] | 0x20) == b'e' {
        let mut e = end + 1;
        if e < b.len() && (b[e] == b'+' || b[e] == b'-') {
            e += 1;
        }
        let e0 = e;
        while e < b.len() && (b[e] as char).is_ascii_digit() {
            e += 1;
        }
        if e > e0 {
            end = e;
        }
    }
    if end == 0 {
        return None;
    }
    // V8 accepts the '.5' / '5.' / '5.e3' StrUnsignedDecimalLiteral shapes
    // Rust's parse rejects — normalise before handing it over.
    let mut s = t[..end].to_string();
    if s.starts_with('.') {
        s.insert(0, '0');
    }
    if s.contains(".e") || s.contains(".E") {
        s = s.replace(".e", ".0e").replace(".E", ".0E");
    }
    if let Some(stripped) = s.strip_suffix('.') {
        s = stripped.to_string();
    }
    s.parse::<f64>().ok()
}

/// `Number(x || 0) || 0` — the proxy branch of resolveCardPrice.
fn num_or_zero(v: Option<&Value>) -> Value {
    let n = match v {
        Some(v) if truthy(v) => match v {
            Value::Number(n) => n.as_f64(),
            _ => js_string(v).trim().parse::<f64>().ok(),
        },
        _ => Some(0.0),
    };
    match n {
        Some(n) if n != 0.0 && n.is_finite() => num_out(n),
        _ => Value::from(0),
    }
}

// ---------------------------------------------------------------------------
// utils/csvExporters.js — generateExportCSV
// ---------------------------------------------------------------------------

fn reverse_condition(format: &str, condition: Option<&Value>) -> Option<&'static str> {
    let c = condition?.as_str()?;
    let table: &[(&str, &str)] = if format == "dragonshield" {
        &[
            ("Near Mint", "NM"),
            ("Lightly Played", "LP"),
            ("Moderately Played", "MP"),
            ("Heavily Played", "HP"),
            ("Damaged", "POOR"),
        ]
    } else {
        &[
            ("Near Mint", "near_mint"),
            ("Lightly Played", "lightly_played"),
            ("Moderately Played", "moderately_played"),
            ("Heavily Played", "heavily_played"),
            ("Damaged", "damaged"),
        ]
    };
    table.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
}

/// One EXPORT_STRATEGIES row: (headers, values) in literal key order.
fn export_strategy_row(format_key: &str, item: &Map<String, Value>) -> Vec<(&'static str, Value)> {
    match format_key {
        "tcgplayer" => vec![
            ("Card Name", orf(item, &["name", "card_name"], Value::String(String::new()))),
            ("Set Code", orf(item, &["set_code", "set_id"], Value::String(String::new()))),
            (
                "Number",
                orf(
                    item,
                    &["collector_number", "number", "card_number"],
                    Value::String(String::new()),
                ),
            ),
            ("Quantity", orf(item, &["quantity"], Value::from(1))),
            ("Condition", orf(item, &["condition"], Value::String("Near Mint".into()))),
            (
                "Printing",
                Value::String(
                    if item.get("printing").and_then(|v| v.as_str()) == Some("Holofoil") {
                        "Foil"
                    } else {
                        "Normal"
                    }
                    .into(),
                ),
            ),
        ],
        "dragonshield" => vec![
            ("Card Name", orf(item, &["name", "card_name"], Value::String(String::new()))),
            ("Set Code", orf(item, &["set_code", "set_id"], Value::String(String::new()))),
            (
                "Card Number",
                orf(
                    item,
                    &["collector_number", "number", "card_number"],
                    Value::String(String::new()),
                ),
            ),
            ("Quantity", orf(item, &["quantity"], Value::from(1))),
            (
                "Condition",
                Value::String(
                    reverse_condition("dragonshield", item.get("condition")).unwrap_or("NM").into(),
                ),
            ),
            (
                "Printing",
                Value::String(
                    if item.get("printing").and_then(|v| v.as_str()) == Some("Holofoil") {
                        "Foil"
                    } else {
                        "Normal"
                    }
                    .into(),
                ),
            ),
        ],
        "manabox" => vec![
            ("Name", orf(item, &["name", "card_name"], Value::String(String::new()))),
            ("Set code", orf(item, &["set_code", "set_id"], Value::String(String::new()))),
            (
                "Card number",
                orf(
                    item,
                    &["collector_number", "number", "card_number"],
                    Value::String(String::new()),
                ),
            ),
            ("Quantity", orf(item, &["quantity"], Value::from(1))),
            (
                "Condition",
                Value::String(
                    reverse_condition("manabox", item.get("condition"))
                        .unwrap_or("near_mint")
                        .into(),
                ),
            ),
            (
                "Foil",
                Value::String(
                    if item.get("printing").and_then(|v| v.as_str()) == Some("Holofoil") {
                        "true"
                    } else {
                        "false"
                    }
                    .into(),
                ),
            ),
        ],
        _ => vec![
            ("Card ID", orf(item, &["card_id"], Value::String(String::new()))),
            ("Name", orf(item, &["name", "card_name"], Value::String(String::new()))),
            ("Set Name", orf(item, &["set_name"], Value::String(String::new()))),
            ("Set ID", orf(item, &["set_id", "set_code"], Value::String(String::new()))),
            (
                "Card Number",
                orf(
                    item,
                    &["collector_number", "number", "card_number"],
                    Value::String(String::new()),
                ),
            ),
            ("Rarity", orf(item, &["rarity"], Value::String("Common".into()))),
            ("Quantity", orf(item, &["quantity"], Value::from(1))),
            ("Condition", orf(item, &["condition"], Value::String("Near Mint".into()))),
            ("Printing", orf(item, &["printing"], Value::String("Normal".into()))),
            ("Language", orf(item, &["language"], Value::String("English".into()))),
            ("Purchase Price", orf(item, &["purchase_price"], Value::from(0))),
            ("Market Price", orf(item, &["price_trend", "market_price"], Value::from(0))),
            ("Added At", orf(item, &["added_at"], Value::String(String::new()))),
        ],
    }
}

/// csvExporters.generateExportCSV, verbatim: headers joined bare, every cell
/// `"…"`-quoted with `"` doubled, rows joined with '\n', '' for no rows.
pub(crate) fn generate_export_csv(rows: &[Map<String, Value>], format_type: &str) -> String {
    let format_key = format_type.to_lowercase();
    if rows.is_empty() {
        return String::new();
    }
    let first = export_strategy_row(&format_key, &rows[0]);
    let headers: Vec<&str> = first.iter().map(|(k, _)| *k).collect();
    let mut out: Vec<String> = Vec::with_capacity(rows.len() + 1);
    out.push(headers.join(","));
    for row in rows {
        let mapped = export_strategy_row(&format_key, row);
        let cells: Vec<String> = headers
            .iter()
            .map(|h| {
                let v = mapped
                    .iter()
                    .find(|(k, _)| k == h)
                    .map(|(_, v)| v)
                    .filter(|v| !v.is_null());
                let s = match v {
                    Some(v) => js_string(v),
                    None => String::new(),
                };
                format!("\"{}\"", s.replace('"', "\"\""))
            })
            .collect();
        out.push(cells.join(","));
    }
    out.join("\n")
}

// ---------------------------------------------------------------------------
// utils/csvMappers.js — parseThirdPartyCSV (note: Node calls this TWICE per
// CSV row; the per-row cardData pass sees an already-mapped row — see the
// module header). Only `name` is load-bearing, but the whole strategy is
// ported so a future caller inherits it faithfully.
// ---------------------------------------------------------------------------

const CONDITION_MAP: &[(&str, &str)] = &[
    ("near mint", "Near Mint"),
    ("nm", "Near Mint"),
    ("near_mint", "Near Mint"),
    ("lightly played", "Lightly Played"),
    ("lp", "Lightly Played"),
    ("lightly_played", "Lightly Played"),
    ("moderately played", "Moderately Played"),
    ("mp", "Moderately Played"),
    ("heavily played", "Heavily Played"),
    ("hp", "Heavily Played"),
    ("damaged", "Damaged"),
    ("dmg", "Damaged"),
    ("poor", "Damaged"),
];

fn map_condition(row: &Map<String, Value>) -> Value {
    let raw = str_or_empty(row.get("Condition")).to_lowercase();
    Value::String(
        CONDITION_MAP
            .iter()
            .find(|(k, _)| *k == raw.as_str())
            .map(|(_, v)| *v)
            .unwrap_or("Near Mint")
            .into(),
    )
}

fn mapped_quantity(row: &Map<String, Value>, keys: &[&str]) -> Value {
    // `parseInt(row['Quantity'] || row['quantity'], 10) || 1`
    for k in keys {
        if let Some(v) = row.get(*k) {
            if truthy(v) {
                if let Some(n) = js_parse_int_str(v) {
                    return Value::from(n);
                }
                break;
            }
        }
    }
    Value::from(1)
}

fn csv_strategy(format_key: &str, row: &Map<String, Value>) -> Map<String, Value> {
    let mut m = Map::new();
    match format_key {
        "tcgplayer" => {
            m.insert("name".into(), orf(row, &["Card Name", "Name"], Value::Null));
            m.insert("set_code".into(), orf(row, &["Set Code", "Set"], Value::Null));
            m.insert(
                "collector_number".into(),
                orf(row, &["Number", "Card Number"], Value::Null),
            );
            m.insert("quantity".into(), mapped_quantity(row, &["Quantity"]));
            m.insert("condition".into(), map_condition(row));
            let p = row.get("Printing").and_then(|v| v.as_str());
            m.insert(
                "printing".into(),
                Value::String(
                    if p == Some("Foil") || p == Some("Holofoil") { "Holofoil" } else { "Normal" }
                        .into(),
                ),
            );
        }
        "dragonshield" => {
            m.insert("name".into(), orf(row, &["Card Name", "Name"], Value::Null));
            m.insert("set_code".into(), orf(row, &["Set Code", "Set"], Value::Null));
            m.insert(
                "collector_number".into(),
                orf(row, &["Card Number", "Number"], Value::Null),
            );
            m.insert("quantity".into(), mapped_quantity(row, &["Quantity"]));
            m.insert("condition".into(), map_condition(row));
            let p = row.get("Printing").and_then(|v| v.as_str());
            m.insert(
                "printing".into(),
                Value::String(
                    if p == Some("Foil") || p == Some("Holofoil") { "Holofoil" } else { "Normal" }
                        .into(),
                ),
            );
        }
        "manabox" => {
            m.insert("name".into(), orf(row, &["Name", "Card Name"], Value::Null));
            m.insert(
                "set_code".into(),
                orf(row, &["Set code", "Set Code", "Set"], Value::Null),
            );
            m.insert(
                "collector_number".into(),
                orf(row, &["Card number", "Number"], Value::Null),
            );
            m.insert("quantity".into(), mapped_quantity(row, &["Quantity"]));
            m.insert("condition".into(), map_condition(row));
            let f = match row.get("Foil") {
                Some(Value::String(s)) => s == "true" || s == "1",
                Some(Value::Bool(true)) => true,
                _ => false,
            };
            m.insert(
                "printing".into(),
                Value::String(if f { "Holofoil" } else { "Normal" }.into()),
            );
        }
        _ => {
            // internal. Falsy name keeps the key absent (=== undefined).
            if let Some(v) = orf(row, &["Name", "card_name", "Name"], Value::Null).as_option_none()
            {
                m.insert("name".into(), v);
            }
            if let Some(v) =
                orf(row, &["Set ID", "set_code", "Set Code"], Value::Null).as_option_none()
            {
                m.insert("set_code".into(), v);
            }
            if let Some(v) = orf(row, &["Set Name", "set_name"], Value::Null).as_option_none() {
                m.insert("set_name".into(), v);
            }
            if let Some(v) =
                orf(row, &["Card Number", "card_number", "number"], Value::Null).as_option_none()
            {
                m.insert("collector_number".into(), v);
            }
            if let Some(v) = orf(row, &["Card ID", "card_id"], Value::Null).as_option_none() {
                m.insert("card_id".into(), v);
            }
            m.insert(
                "quantity".into(),
                mapped_quantity(row, &["Quantity", "quantity"]),
            );
            m.insert("condition".into(), map_condition(row));
            m.insert("printing".into(), orf(row, &["Printing"], Value::String("Normal".into())));
            m.insert("language".into(), orf(row, &["Language"], Value::String("English".into())));
            // parseFloat(row['Purchase Price'] || row['purchase_price']) || 0
            let pp = orf(row, &["Purchase Price", "purchase_price"], Value::Null);
            let f = match &pp {
                Value::Null => None,
                v => js_parse_float(&js_string(v)),
            };
            m.insert(
                "purchase_price".into(),
                match f {
                    Some(f) if f != 0.0 => num_out(f),
                    _ => Value::from(0),
                },
            );
        }
    }
    m
}

/// Helper so the internal strategy keeps 'undefined' (missing key) distinct
/// from null: `orf` folds falsy to Null, truthy passes through.
trait AsOptionNone {
    fn as_option_none(self) -> Option<Value>;
}
impl AsOptionNone for Value {
    fn as_option_none(self) -> Option<Value> {
        if self.is_null() {
            None
        } else {
            Some(self)
        }
    }
}

/// parseThirdPartyCSV(rows, formatType) — the bulk pass.
fn parse_third_party_csv(rows: &[Map<String, Value>], format_type: &Value) -> Vec<Value> {
    let fk = match format_type {
        v if truthy(v) => js_string(v).to_lowercase(),
        _ => "internal".into(),
    };
    rows.iter().map(|r| Value::Object(csv_strategy(&fk, r))).collect()
}

// ---------------------------------------------------------------------------
// utils/priceHelpers.js — resolveCardPrice on the EXPORT call shape
// ---------------------------------------------------------------------------

/// utils/priceHelpers.resolveCardPrice, verbatim twin (the call site spreads
/// `{ price_trend, price_normal, price_holofoil, ...keep }`, so all three
/// price fields are present — possibly null from SQLite). ALWAYS returns a
/// value: market_price is never undefined in the export row.
fn resolve_card_price_for_export(card: &Map<String, Value>) -> Value {
    // if (card.is_proxy) return 0;
    if card.get("is_proxy").map(truthy).unwrap_or(false) {
        return num_out(0.0);
    }
    // if (card.printing === 'Holofoil' && card.price_holofoil !== null && card.price_holofoil > 0)
    if card.get("printing").and_then(|v| v.as_str()) == Some("Holofoil") {
        if let Some(h) = card.get("price_holofoil") {
            if !h.is_null() && js_number_as_f64(h).map(|f| f > 0.0).unwrap_or(false) {
                return h.clone();
            }
        }
    }
    // if (card.printing === 'Normal' && card.price_normal !== null && card.price_normal > 0)
    if card.get("printing").and_then(|v| v.as_str()) == Some("Normal") {
        if let Some(n) = card.get("price_normal") {
            if !n.is_null() && js_number_as_f64(n).map(|f| f > 0.0).unwrap_or(false) {
                return n.clone();
            }
        }
    }
    // return card.price_trend || 0;
    match card.get("price_trend") {
        Some(v) if truthy(v) => v.clone(),
        _ => num_out(0.0),
    }
}

// ---------------------------------------------------------------------------
// exportRows
// ---------------------------------------------------------------------------

fn export_rows_blocking(
    conn: &rusqlite::Connection,
    user_id: i64,
    entry_ids: Option<&[Value]>,
) -> Result<Vec<Map<String, Value>>, rusqlite::Error> {
    let raw: Vec<Map<String, Value>> = match entry_ids {
        None => rows_map(conn, EXPORT_SELECT, &[SqlValue::Integer(user_id)])?,
        Some(ids) => {
            let mut raw = Vec::new();
            for chunk in ids.chunks(500) {
                let placeholders = vec!["?"; chunk.len()].join(",");
                let sql =
                    format!("{EXPORT_SELECT} AND c.id IN ({placeholders})");
                let mut params = vec![SqlValue::Integer(user_id)];
                params.extend(chunk.iter().map(json_to_sql_value));
                raw.extend(rows_map(conn, &sql, &params)?);
            }
            // order = first-seen position of String(id) in entry_ids
            let mut order: HashMap<String, usize> = HashMap::new();
            for (i, id) in ids.iter().enumerate() {
                order.entry(js_string(id)).or_insert(i);
            }
            raw.sort_by_key(|r| {
                order
                    .get(&js_to_string_opt(r.get("entry_id")))
                    .copied()
                    .unwrap_or(0)
            });
            raw
        }
    };
    Ok(raw
        .into_iter()
        .map(|row| {
            let price_trend = row.get("price_trend").cloned();
            let price_normal = row.get("price_normal").cloned();
            let price_holofoil = row.get("price_holofoil").cloned();
            // `{ entry_id, price_trend, price_normal, price_holofoil, ...keep }`
            // — destructure-rest KEEPS the original column order. Build it by
            // filtering: serde_json's preserve_order Map::remove is
            // swap_remove-based and would shuffle the surviving keys.
            let mut resolved = Map::new();
            for (k, v) in row.iter() {
                if matches!(
                    k.as_str(),
                    "entry_id" | "price_trend" | "price_normal" | "price_holofoil"
                ) {
                    continue;
                }
                resolved.insert(k.clone(), v.clone());
            }
            // resolveCardPrice input: {price_trend, price_normal, price_holofoil, ...keep}
            let mut card_input = Map::new();
            card_input
                .insert("price_trend".into(), price_trend.unwrap_or(Value::Null));
            card_input
                .insert("price_normal".into(), price_normal.unwrap_or(Value::Null));
            card_input
                .insert("price_holofoil".into(), price_holofoil.unwrap_or(Value::Null));
            for (k, v) in resolved.iter() {
                card_input.insert(k.clone(), v.clone());
            }
            let mut out = resolved;
            out.insert("market_price".into(), resolve_card_price_for_export(&card_input));
            out
        })
        .collect())
}

// ---------------------------------------------------------------------------
// sendExport — res.set + res.json/res.send twin (Express default ETag,
// 304 replay, charset=utf-8, global compression()).
// ---------------------------------------------------------------------------

fn send_export_bytes(
    headers: &HeaderMap,
    raw: Vec<u8>,
    content_type: &str,
    disposition: String,
) -> Response {
    let tag = body_etag(&raw);
    if fresh_allows_304(headers) {
        if let Some(inm) = headers.get("if-none-match").and_then(|v| v.to_str().ok()) {
            if etag_matches(Some(inm), &tag) {
                // res.send's 304 strip removes Content-Type/Length but keeps
                // the headers sendExport set + the ETag.
                return Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(axum::http::header::ETAG, &tag)
                    .header("content-disposition", &disposition)
                    .body(Body::empty())
                    .unwrap();
            }
        }
    }
    let ae = headers.get(axum::http::header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
    let use_gzip = wants_encoding(ae, "gzip", raw.len());
    let bytes = if use_gzip { Body::from(gzip_bytes(&raw)) } else { Body::from(raw) };
    let mut b = Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header("content-disposition", disposition)
        .header(axum::http::header::ETAG, tag);
    if use_gzip {
        b = b
            .header(axum::http::header::CONTENT_ENCODING, "gzip")
            .header(axum::http::header::VARY, "Accept-Encoding");
    }
    b.body(bytes).unwrap()
}

fn send_export(
    headers: &HeaderMap,
    rows: &[Map<String, Value>],
    is_json: bool,
    target_format: &str,
    base: &str,
) -> Response {
    if is_json {
        let body = serde_json::to_vec(&Value::Array(
            rows.iter().cloned().map(Value::Object).collect(),
        ))
        .unwrap();
        send_export_bytes(
            headers,
            body,
            "application/json; charset=utf-8",
            format!("attachment; filename={base}_{target_format}.json"),
        )
    } else {
        let csv = generate_export_csv(rows, target_format);
        send_export_bytes(
            headers,
            csv.into_bytes(),
            "text/csv; charset=utf-8",
            format!("attachment; filename={base}_{target_format}.csv"),
        )
    }
}

// ---------------------------------------------------------------------------
// Forward helpers (whole-request replay to Node, pre-write)
// ---------------------------------------------------------------------------

async fn replay_get(st: AppState, headers: &HeaderMap, path: &str, query: Option<String>) -> Response {
    let uri = match query {
        Some(q) => format!("{path}?{q}"),
        None => path.to_string(),
    };
    let mut rb = Request::builder().method(Method::GET).uri(uri);
    for (k, v) in headers.iter() {
        if k != axum::http::header::CONTENT_LENGTH {
            rb = rb.header(k, v);
        }
    }
    let req = rb.body(Body::empty()).unwrap_or_else(|_| Request::new(Body::empty()));
    crate::proxy(State(st), req).await
}

async fn replay_post(
    st: AppState,
    headers: &HeaderMap,
    path: &str,
    body: &Value,
) -> Response {
    let mut rb = Request::builder().method(Method::POST).uri(path);
    for (k, v) in headers.iter() {
        if k != axum::http::header::CONTENT_LENGTH {
            rb = rb.header(k, v);
        }
    }
    let req = rb
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Request::new(Body::empty()));
    crate::proxy(State(st), req).await
}

// ---------------------------------------------------------------------------
// GET /api/export
// ---------------------------------------------------------------------------

/// GET /api/export — routes/importExport.js GET /export.
pub async fn route_export(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("export") || req.method() != Method::GET {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let headers: HeaderMap = req.headers().clone();
    let token = bearer_token(&headers).map(str::to_string);
    let user = match authenticate(&pool, token.as_deref(), true) {
        Ok(u) => u,
        Err(res) => return res,
    };
    let query = req.uri().query().map(str::to_string);

    // const { format = 'csv', ecosystem = 'internal' } = req.query
    // — qs arrays/objects on the SELECTED value would hit a bare
    // .toLowerCase() BEFORE the try (uncaught → Node process exit); the
    // honest answer for those is Node's own.
    let eco_val = match qs_get(query.as_deref(), "ecosystem") {
        QsVal::Missing => QsVal::Scalar("internal".into()),
        v => v,
    };
    let fmt_val = match qs_get(query.as_deref(), "format") {
        QsVal::Missing => QsVal::Scalar("csv".into()),
        v => v,
    };
    let eco_selected = match &eco_val {
        QsVal::Scalar(s) => !s.is_empty(),
        QsVal::Array(_) | QsVal::Obj => true, // [] / {} are truthy in JS
        QsVal::Missing => true,                // destructuring default 'internal'
    };
    if !eco_selected {
        // targetFormat falls through to `format || 'internal'` — an array/
        // object format would crash the same way.
        match &fmt_val {
            QsVal::Array(_) | QsVal::Obj => {
                return replay_get(st, &headers, "/api/export", query).await
            }
            _ => {}
        }
    } else if matches!(eco_val, QsVal::Array(_) | QsVal::Obj) {
        return replay_get(st, &headers, "/api/export", query).await;
    }
    let target_format = if eco_selected {
        match &eco_val {
            QsVal::Scalar(s) => s.to_lowercase(),
            _ => "internal".into(),
        }
    } else {
        match &fmt_val {
            QsVal::Scalar(s) if !s.is_empty() => s.to_lowercase(),
            _ => "internal".into(),
        }
    };
    // sendExport's branch test is String(format).toLowerCase() === 'json'.
    let is_json = match &fmt_val {
        QsVal::Scalar(s) => s.to_lowercase() == "json",
        QsVal::Array(a) => a.join(",").to_lowercase() == "json",
        QsVal::Obj => false, // '[object object]'
        QsVal::Missing => false,
    };

    let db = pool;
    let uid = user.id;
    let rows = match tokio::task::spawn_blocking(move || -> Result<_, rusqlite::Error> {
        let conn = db.get()?;
        export_rows_blocking(&conn, uid, None)
    })
    .await
    {
        Ok(Ok(rows)) => rows,
        // Node's catch answers {error:'Export failed', message:<sqlite
        // text>}; the text is driver-specific → Node itself answers.
        _ => return replay_get(st, &headers, "/api/export", query).await,
    };
    send_export(&headers, &rows, is_json, &target_format, "scrybox_collection")
}

// ---------------------------------------------------------------------------
// POST /api/export
// ---------------------------------------------------------------------------

/// POST /api/export — routes/importExport.js POST /export. normalJson is
//  mounted above the auth gate, so body outcomes answer BEFORE auth.
pub async fn route_export_post(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("export") || req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let headers: HeaderMap = req.headers().clone();
    let (body_req, early) =
        crate::precons::read_json_body(req, NORMAL_BODY_LIMIT).await;
    if let Some(res) = early {
        return res;
    }
    let token = bearer_token(&headers).map(str::to_string);
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return res,
    };
    // `req.body || {}` — a top-level array destructures to undefined fields.
    let body_obj: Map<String, Value> = body_req.as_object().cloned().unwrap_or_default();

    // if (!Array.isArray(body.entry_ids) || body.entry_ids.length === 0 ||
    //     body.entry_ids.length > 200000 || !Number.isInteger(body.entry_ids.length))
    let ids_early = match body_obj.get("entry_ids") {
        Some(Value::Array(a)) if !a.is_empty() && a.len() <= 200000 => Some(a.clone()),
        _ => None,
    };
    let ids = match ids_early {
        Some(a) => a,
        None => {
            let mut m = Map::new();
            m.insert("error".into(), Value::String("entry_ids must be a non-empty array".into()));
            return crate::precons::json_response(StatusCode::BAD_REQUEST, &m);
        }
    };
    // entryIds = [...new Set(ids.map(Number).filter(Number.isInteger))]
    let mut entry_ids: Vec<Value> = Vec::new();
    let mut seen: HashMap<String, ()> = HashMap::new();
    for id in &ids {
        let n = js_number_as_f64(id);
        let Some(n) = n else { continue };
        if !n.is_finite() || n.fract() != 0.0 {
            continue; // Number.isInteger
        }
        let key = js_string(&num_out(n));
        if seen.insert(key, ()).is_none() {
            entry_ids.push(num_out(n));
        }
    }
    if entry_ids.is_empty() {
        let mut m = Map::new();
        m.insert("error".into(), Value::String("entry_ids must be integers".into()));
        return crate::precons::json_response(StatusCode::BAD_REQUEST, &m);
    }

    // format default 'csv'; targetFormat = String(ecosystem || 'internal')
    let fmt = body_obj.get("format").cloned().unwrap_or(Value::String("csv".into()));
    let is_json = js_string(&fmt).to_lowercase() == "json";
    let target_format = match body_obj.get("ecosystem") {
        Some(v) if truthy(v) => js_string(v).to_lowercase(),
        _ => "internal".into(),
    };

    let db = pool;
    let uid = user.id;
    let rows = match tokio::task::spawn_blocking(move || -> Result<_, rusqlite::Error> {
        let conn = db.get()?;
        export_rows_blocking(&conn, uid, Some(&entry_ids))
    })
    .await
    {
        Ok(Ok(rows)) => rows,
        _ => return replay_post(st, &headers, "/api/export", &body_req).await,
    };
    send_export(&headers, &rows, is_json, &target_format, "scrybox_view")
}

/// Number(x) for the JS map(Number) step: strings trim+parse, '' → 0,
/// booleans 1/0, null → 0, arrays of one scalar → its Number, else NaN.
fn js_number_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Null => Some(0.0),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = js_trim(s);
            if t.is_empty() {
                Some(0.0)
            } else if t.len() > 2 && (t.starts_with("0x") || t.starts_with("0X")) {
                // Number('0x1f') === 31 — hex literal support.
                i64::from_str_radix(&t[2..], 16).map(|i| i as f64).ok()
            } else {
                t.parse::<f64>().ok()
            }
        }
        Value::Array(a) => match a.as_slice() {
            [] => Some(0.0),
            [x] => js_number_as_f64(x),
            _ => None,
        },
        Value::Object(_) => None,
    }
}

// ---------------------------------------------------------------------------
// POST /api/import
// ---------------------------------------------------------------------------

/// route-local parseCSVLine (quote chars consumed, fields trimmed).
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in line.chars() {
        if c == '"' {
            in_quotes = !in_quotes;
        } else if c == ',' && !in_quotes {
            out.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    out.push(cur.trim().to_string());
    out
}

/// `.replace(/^"|"$/g, '')` — at most one leading and one trailing quote.
fn strip_outer_quotes(s: &str) -> String {
    let mut t = s;
    if let Some(rest) = t.strip_prefix('"') {
        t = rest;
    }
    if let Some(rest) = t.strip_suffix('"') {
        t = rest;
    }
    t.to_string()
}

/// POST /api/import — routes/importExport.js POST /import. bigJson mounts
/// AFTER the auth gate (server.js), so auth answers before body parsing.
pub async fn route_import(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("export") || req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let headers: HeaderMap = req.headers().clone();
    let token = bearer_token(&headers).map(str::to_string);
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return res,
    };
    let (body_req, early) =
        crate::precons::read_json_body(req, BIG_BODY_LIMIT).await;
    if let Some(res) = early {
        return res;
    }
    let body_obj: Map<String, Value> = body_req.as_object().cloned().unwrap_or_default();

    // const { format = 'internal', data } = req.body
    let format: Option<Value> = body_obj.get("format").cloned();
    let data = body_obj.get("data").cloned();
    if !data.as_ref().map(truthy).unwrap_or(false) {
        return crate::precons::json_error(StatusCode::BAD_REQUEST, "No data provided");
    }
    // const isJson = format.toLowerCase() === 'json'
    let format_key = match &format {
        None => "internal".to_string(),
        Some(Value::String(s)) => s.to_lowercase(),
        Some(Value::Null) => {
            // V8: Cannot read properties of null (reading 'toLowerCase')
            return import_failed("Cannot read properties of null (reading 'toLowerCase')");
        }
        Some(_) => {
            return import_failed("format.toLowerCase is not a function");
        }
    };
    let is_json_mode = format_key == "json";
    let raw_items: Vec<Value> = if is_json_mode {
        let parsed = match &data {
            Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
                // V8's JSON.parse failure text is unreproducible → Node.
                Err(_) => return replay_post(st, &headers, "/api/import", &body_req).await,
                Ok(v) => v,
            },
            Some(v) => v.clone(),
            None => Value::Null,
        };
        match parsed {
            Value::Array(a) => a,
            _ => {
                return crate::precons::json_error(
                    StatusCode::BAD_REQUEST,
                    "Invalid data payload",
                )
            }
        }
    } else {
        let data_str = match &data {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(), // lines = String(data).split('\n') — objects etc. never yield a usable CSV
        };
        let lines: Vec<String> = data_str
            .split('\n')
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if lines.len() <= 1 {
            return crate::precons::json_error(
                StatusCode::BAD_REQUEST,
                "CSV file is empty or missing headers",
            );
        }
        let headers: Vec<String> =
            parse_csv_line(&lines[0]).iter().map(|h| strip_outer_quotes(h)).collect();
        let mut parsed_rows: Vec<Map<String, Value>> = Vec::new();
        for line in &lines[1..] {
            let values: Vec<String> =
                parse_csv_line(line).iter().map(|v| strip_outer_quotes(v)).collect();
            if values.len() < headers.len() {
                continue;
            }
            let mut row_obj = Map::new();
            for (idx, h) in headers.iter().enumerate() {
                row_obj.insert(h.clone(), Value::String(values[idx].clone()));
            }
            parsed_rows.push(row_obj);
        }
        parse_third_party_csv(&parsed_rows, &format.clone().unwrap_or(Value::String("internal".into())))
    };

    // Per-row validation — fields read straight off rawItems[i]. For CSV that
    // IS the strategy-mapped row (parseThirdPartyCSV runs exactly ONCE, at
    // parse time); for JSON it is the raw body item. There is no second
    // mapping pass and no separate cardData.
    const ALLOWED_PRINTINGS: [&str; 2] = ["Normal", "Holofoil"];
    let mut rejected: Vec<Value> = Vec::new();
    let mut prepared: Vec<(Map<String, Value>, Value)> = Vec::new(); // (item, cardId)
    let mut authoritative: Vec<(String, Map<String, Value>)> = Vec::new(); // Map set: value overwritten, position kept
    let mut auth_pos: HashMap<String, usize> = HashMap::new();

    for (i, item) in raw_items.iter().enumerate() {
        // String(item.game ?? '') — only a NULL item throws at property
        // access (V8 text pinned verbatim; undefined is folded by ??).
        let empty = Map::new();
        let item_obj: &Map<String, Value> = match item {
            Value::Object(m) => m,
            Value::Null => {
                return import_failed("Cannot read properties of null (reading 'game')");
            }
            _ => &empty,
        };

        let game = str_or_empty_coalesce(item_obj.get("game")).to_lowercase();
        if !game.is_empty() && game != "mtg" {
            rejected.push(rejected_row(
                i,
                &format!(
                    "game '{}' is not supported (this install is MTG-only)",
                    js_to_string_opt(item_obj.get("game"))
                ),
            ));
            continue;
        }
        // item.printing !== undefined && item.printing !== '' &&
        // !['Normal','Holofoil'].includes(item.printing)   (includes === /)
        // — null IS rejected (null !== undefined), only the exact '' skips.
        if let Some(p) = item_obj.get("printing") {
            let skip = matches!(p, Value::String(s) if s.is_empty());
            let allowed = matches!(p, Value::String(s) if ALLOWED_PRINTINGS.contains(&s.as_str()));
            if !skip && !allowed {
                rejected.push(rejected_row(
                    i,
                    &format!(
                        "printing '{}' is not supported",
                        js_to_string_opt(Some(p))
                    ),
                ));
                continue;
            }
        }

        // const suppliedId = item.card_id || item.id || null
        let supplied = orf(item_obj, &["card_id", "id"], Value::Null);
        // node-sqlite3 rejects object/array/boolean binds at the probe too
        // (reject → outer catch → 500 driver text); nothing has been
        // written yet — Node answers those.
        if matches!(supplied, Value::Object(_) | Value::Array(_) | Value::Bool(_)) {
            return replay_post(st, &headers, "/api/import", &body_req).await;
        }
        let lookup_present = truthy(&supplied);

        let mut cached: Option<Map<String, Value>> = None;
        if lookup_present {
            let db = pool.clone();
            let sid = supplied.clone();
            let got = tokio::task::spawn_blocking(move || -> Result<Option<Map<String, Value>>, rusqlite::Error> {
                let conn = db.get()?;
                rows_map(&conn, "SELECT id FROM card_cache WHERE id = ?", &[json_to_sql_value(&sid)])
                    .map(|mut r| r.pop())
            })
            .await
            .map_err(|_| ())
            .and_then(|r| r.map_err(|_| ()));
            match got {
                Ok(row) => cached = row,
                // node-sqlite3 rejects → outer catch → 500 with driver text.
                Err(()) => return replay_post(st, &headers, "/api/import", &body_req).await,
            }
        }
        if let Some(c) = &cached {
            // A cached id has already crossed the Scryfall boundary: use it
            // verbatim. NO cacheCards for this row — Node only caches
            // bulk-resolved matches.
            let id = c.get("id").cloned().unwrap_or(Value::Null);
            if js_to_string_opt(Some(&id)).starts_with("mtg-") {
                prepared.push((item_obj.clone(), id));
            } else {
                rejected.push(rejected_row(i, "cached card id is not an MTG printing"));
            }
            continue;
        }

        // Uncached: resolve through Scryfall — the payload's name/set/number
        // are not evidence a card is Magic.
        let lookup_id = if lookup_present { supplied.clone() } else { Value::Null };
        let lookup_set = orf(item_obj, &["set_code", "set_id"], Value::String(String::new()));
        let lookup_num = orf(item_obj, &["collector_number", "number"], Value::String(String::new()));
        let lookup_name = orf(item_obj, &["name"], Value::String(String::new()));
        let has_setnum = truthy(&lookup_set) && truthy(&lookup_num);
        if !truthy(&lookup_id) && !has_setnum && !truthy(&lookup_name) {
            rejected.push(rejected_row(i, "card has no MTG identifier Scryfall can validate"));
            continue;
        }
        let mut lookup = Map::new();
        lookup.insert("id".into(), lookup_id);
        lookup.insert("set_id".into(), lookup_set);
        lookup.insert("number".into(), lookup_num);
        lookup.insert("name".into(), lookup_name);

        let bulk = match bulk_fetch_by_identifier(&st, std::slice::from_ref(&lookup)).await {
            Ok(b) => b,
            // axios/scry client failure text is client-specific → Node.
            Err(_) => return replay_post(st, &headers, "/api/import", &body_req).await,
        };
        let matched = bulk.pairs.first().map(|(_, c)| c.clone());
        match matched {
            Some(card) => {
                let id = card.get("id").cloned().unwrap_or(Value::Null);
                let id_s = js_to_string_opt(Some(&id));
                if id_s.starts_with("mtg-") {
                    prepared.push((item_obj.clone(), id));
                    match auth_pos.get(&id_s) {
                        Some(&p) => authoritative[p].1 = card,
                        None => {
                            auth_pos.insert(id_s.clone(), authoritative.len());
                            authoritative.push((id_s, card));
                        }
                    }
                } else {
                    rejected.push(rejected_row(
                        i,
                        "card was not recognized by Scryfall as an MTG printing",
                    ));
                }
            }
            None => {
                rejected.push(rejected_row(
                    i,
                    "card was not recognized by Scryfall as an MTG printing",
                ));
            }
        }
    }

    if !rejected.is_empty() {
        let message = format!(
            "Import rejected: {} of {} row(s) are not valid MTG printings. No rows were imported.",
            rejected.len(),
            raw_items.len()
        );
        let mut m = Map::new();
        m.insert("error".into(), Value::String(message));
        m.insert("rejected".into(), Value::Array(rejected));
        return crate::precons::json_response(StatusCode::BAD_REQUEST, &m);
    }

    // Bind-time fidelity: node-sqlite3 throws on object/array/boolean binds
    // ("Invalid data type") and the whole request 500s with driver text →
    // Node answers those; non-finite numbers ride the same forward (Node
    // binds Infinity/NaN as REAL, the driver text for rejects differs).
    for (item, card_id) in &prepared {
        let binds = [
            card_id.clone(),
            orf(item, &["quantity"], Value::from(1)),
            orf(item, &["condition"], Value::String("Near Mint".into())),
            orf(item, &["printing"], Value::String("Normal".into())),
            orf(item, &["language"], Value::String("English".into())),
            orf(item, &["purchase_price"], Value::from(0)),
        ];
        for b in binds {
            match &b {
                Value::Object(_) | Value::Array(_) | Value::Bool(_) => {
                    return replay_post(st, &headers, "/api/import", &body_req).await
                }
                Value::Number(n) => {
                    if !n.as_f64().map(|f| f.is_finite()).unwrap_or(true) {
                        return replay_post(st, &headers, "/api/import", &body_req).await;
                    }
                }
                _ => {}
            }
        }
    }

    let count = prepared.len() as i64;
    let db = pool.clone();
    let uid = user.id;
    let cards: Vec<Map<String, Value>> = authoritative.into_iter().map(|(_, c)| c).collect();
    let rows: Vec<Vec<SqlValue>> = prepared
        .iter()
        .map(|(item, card_id)| {
            vec![
                json_to_sql_value(card_id),
                json_to_sql_value(&orf(item, &["quantity"], Value::from(1))),
                json_to_sql_value(&orf(item, &["condition"], Value::String("Near Mint".into()))),
                json_to_sql_value(&orf(item, &["printing"], Value::String("Normal".into()))),
                json_to_sql_value(&orf(item, &["language"], Value::String("English".into()))),
                json_to_sql_value(&orf(
                    item,
                    &["purchase_price"],
                    Value::from(0),
                )),
                SqlValue::Integer(uid),
            ]
        })
        .collect();
    let tx_ok = tokio::task::spawn_blocking(move || import_transaction(&db, &cards, &rows, None))
    .await;
    if !matches!(tx_ok, Ok(Ok(()))) {
        // Node's withTransaction rolls back and the catch 500s with the
        // sqlite3 error text — nothing persisted, let Node answer for real.
        return replay_post(st, &headers, "/api/import", &body_req).await;
    }

    let mut out = Map::new();
    out.insert("success".into(), Value::Bool(true));
    out.insert("count".into(), Value::from(count));
    out.insert(
        "message".into(),
        Value::String(format!("Successfully imported {count} items.")),
    );
    let bytes = serde_json::to_vec(&Value::Object(out.clone())).unwrap();
    // statsCache.invalidateOnWrite twin on the native success response
    // (same established pattern as the other write groups).
    stats::on_proxied_write(&st, &Method::POST, "/api/import", &headers);
    crate::decks_reads::json_read_response(&headers, bytes, Vec::<(String, String)>::new())
}

/// The /import write transaction, extracted from the spawn_blocking leg so
/// the COMMIT-failure rollback is unit-testable. Node's withTransaction
/// rolls back on a failing statement AND on a failing COMMIT; a pooled
/// connection returned to the pool inside an open transaction holds the WAL
/// write lock against every later writer (same class as the admin_users
/// DELETE pool-leak note).
///
/// `after_begin` exists only for the unit tests: it runs inside the opened
/// transaction (PRAGMA defer_foreign_keys is transaction-scoped, so a
/// COMMIT-time failure injection has to land after the BEGIN). Production
/// passes `None`.
fn import_transaction(
    db: &crate::db::DbPool,
    cards: &[Map<String, Value>],
    rows: &[Vec<SqlValue>],
    after_begin: Option<&dyn Fn(&rusqlite::Connection) -> rusqlite::Result<()>>,
) -> Result<(), ()> {
    let conn = db.get().map_err(|_| ())?;
    conn.execute_batch("BEGIN IMMEDIATE TRANSACTION").map_err(|_| ())?;
    let result = (|| -> rusqlite::Result<()> {
        if let Some(f) = after_begin {
            f(&conn)?;
        }
        cache_normalized_cards(&conn, cards)?;
        let mut stmt = conn.prepare_cached(
            "INSERT INTO collection (card_id, quantity, condition, printing, language, purchase_price, user_id, added_at) VALUES (?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)",
        )?;
        for r in rows {
            stmt.execute(rusqlite::params_from_iter(r.iter()))?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => match conn.execute_batch("COMMIT") {
            Ok(()) => Ok(()),
            Err(_) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(())
            }
        },
        Err(_) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(())
        }
    }
}

/// `String(item.game ?? '')` — ?? folds null AND undefined to ''.
fn str_or_empty_coalesce(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(v) => js_string(v),
    }
}

fn rejected_row(index: usize, reason: &str) -> Value {
    let mut m = Map::new();
    m.insert("index".into(), Value::from(index as i64));
    m.insert("reason".into(), Value::String(reason.to_string()));
    Value::Object(m)
}

fn import_failed(message: &str) -> Response {
    let mut m = Map::new();
    m.insert("error".into(), Value::String("Import failed".into()));
    m.insert("message".into(), Value::String(message.to_string()));
    crate::precons::json_response(StatusCode::INTERNAL_SERVER_ERROR, &m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn parse_csv_line_quotes_and_commas() {
        // quote chars are CONSUMED, commas inside quotes do not split,
        // every field is trimmed.
        assert_eq!(parse_csv_line("a, b ,\"c,d\""), vec!["a", "b", "c,d"]);
        // `""` toggles twice — the doubled quotes leave inQuotes TRUE, so
        // the comma after them is ABSORBED (naive-parser quirk, pinned).
        assert_eq!(parse_csv_line("\"He said \"\"hi\"\"\",x"), vec!["He said hi", "x"]);
        assert_eq!(parse_csv_line(""), vec![""]);
        // an unbalanced quote keeps quote mode ON: the comma never splits.
        assert_eq!(parse_csv_line("a\"b,c"), vec!["ab,c"]);
        assert_eq!(strip_outer_quotes("\"x\""), "x");
        assert_eq!(strip_outer_quotes("\"\"x\"\""), "\"x\"");
    }

    #[test]
    fn csv_internal_headers_quoting_and_market_price_fold() {
        let rows = vec![m(&[
            ("card_id", Value::String("mtg-1".into())),
            ("name", Value::String("Ab\"C".into())),
            ("set_name", Value::String("Set".into())),
            ("set_code", Value::String("10e".into())),
            ("collector_number", Value::String("1".into())),
            ("rarity", Value::String("Common".into())),
            ("quantity", Value::from(2)),
            ("condition", Value::String("Near Mint".into())),
            ("printing", Value::String("Holofoil".into())),
            ("language", Value::String("English".into())),
            ("purchase_price", Value::from(1.5)),
            // market_price absent (non-proxy resolveCardPrice → undefined):
            // the strategy folds price_trend||market_price||0 → 0.
            ("added_at", Value::String("2026-01-01 00:00:00".into())),
        ])];
        let csv = generate_export_csv(&rows, "internal");
        let lines: Vec<&str> = csv.split('\n').collect();
        assert_eq!(
            lines[0],
            "Card ID,Name,Set Name,Set ID,Card Number,Rarity,Quantity,Condition,Printing,Language,Purchase Price,Market Price,Added At"
        );
        assert_eq!(
            lines[1],
            "\"mtg-1\",\"Ab\"\"C\",\"Set\",\"10e\",\"1\",\"Common\",\"2\",\"Near Mint\",\"Holofoil\",\"English\",\"1.5\",\"0\",\"2026-01-01 00:00:00\""
        );
        assert_eq!(generate_export_csv(&[], "internal"), "");
    }

    #[test]
    fn csv_reverse_condition_maps_and_foil_flags() {
        let row = m(&[
            ("name", Value::String("N".into())),
            ("set_code", Value::String("s".into())),
            ("collector_number", Value::String("7".into())),
            ("quantity", Value::from(3)),
            ("condition", Value::String("Lightly Played".into())),
            ("printing", Value::String("Holofoil".into())),
        ]);
        assert_eq!(
            generate_export_csv(std::slice::from_ref(&row), "manabox"),
            "Name,Set code,Card number,Quantity,Condition,Foil\n\"N\",\"s\",\"7\",\"3\",\"lightly_played\",\"true\""
        );
        assert_eq!(
            generate_export_csv(std::slice::from_ref(&row), "dragonshield"),
            "Card Name,Set Code,Card Number,Quantity,Condition,Printing\n\"N\",\"s\",\"7\",\"3\",\"LP\",\"Foil\""
        );
        assert_eq!(
            generate_export_csv(std::slice::from_ref(&row), "tcgplayer"),
            "Card Name,Set Code,Number,Quantity,Condition,Printing\n\"N\",\"s\",\"7\",\"3\",\"Lightly Played\",\"Foil\""
        );
        // unknown format → internal strategy (lower-cased key)
        assert!(generate_export_csv(std::slice::from_ref(&row), "NONSENSE").starts_with("Card ID,"));
    }

    #[test]
    fn csv_strategy_maps_once() {
        // parseThirdPartyCSV runs EXACTLY ONCE at parse time — validation
        // then reads the mapped (lowercase-key) row directly.
        let raw = m(&[
            ("Card Name", Value::String("Lightning Bolt".into())),
            ("Set Code", Value::String("m10".into())),
            ("Number", Value::String("146".into())),
        ]);
        let once = csv_strategy("tcgplayer", &raw);
        assert_eq!(once["name"], Value::String("Lightning Bolt".into()));
        assert_eq!(once["set_code"], Value::String("m10".into()));
        assert_eq!(once["collector_number"], Value::String("146".into()));
        assert_eq!(once["printing"], Value::String("Normal".into()));
        let raw_i = m(&[("Name", Value::String("X".into()))]);
        let once_i = csv_strategy("internal", &raw_i);
        assert_eq!(once_i["name"], Value::String("X".into()));
        assert_eq!(once_i["quantity"], Value::from(1));
        assert_eq!(once_i["condition"], Value::String("Near Mint".into()));
        // parseInt trap: '2x' → 2, 'x' → 1 (NaN || 1)
        let q = m(&[("Quantity", Value::String("2x".into()))]);
        assert_eq!(csv_strategy("internal", &q)["quantity"], Value::from(2));
        let q = m(&[("Quantity", Value::String("x".into()))]);
        assert_eq!(csv_strategy("internal", &q)["quantity"], Value::from(1));
        // manabox Foil truth set
        let f = m(&[("Foil", Value::String("1".into()))]);
        assert_eq!(csv_strategy("manabox", &f)["printing"], Value::String("Holofoil".into()));
        let f = m(&[("Foil", Value::Bool(true))]);
        assert_eq!(csv_strategy("manabox", &f)["printing"], Value::String("Holofoil".into()));
        // CONDITION_MAP folds the reverse-mapped export conditions back.
        let c = m(&[("Condition", Value::String("lightly_played".into()))]);
        assert_eq!(
            csv_strategy("manabox", &c)["condition"],
            Value::String("Lightly Played".into())
        );
    }

    #[test]
    fn resolve_price_export_shape() {
        // verbatim priceHelpers.resolveCardPrice on the export call shape.
        // Proxy wins first: is_proxy TRUTHY → 0 (not its own price!).
        let p = m(&[
            ("is_proxy", Value::from(1)),
            ("price_trend", Value::from(1.25)),
            ("printing", Value::String("Normal".into())),
            ("price_normal", Value::from(9.0)),
        ]);
        assert_eq!(resolve_card_price_for_export(&p), Value::from(0));
        // is_proxy 2 is truthy too.
        let p2 = m(&[("is_proxy", Value::from(2)), ("price_trend", Value::from(3.0))]);
        assert_eq!(resolve_card_price_for_export(&p2), Value::from(0));
        // Holofoil + price_holofoil > 0 → price_holofoil.
        let h = m(&[
            ("is_proxy", Value::from(0)),
            ("printing", Value::String("Holofoil".into())),
            ("price_holofoil", Value::from(3.0)),
            ("price_trend", Value::from(1.0)),
        ]);
        assert_eq!(resolve_card_price_for_export(&h), Value::from(3.0));
        // holofoil price null/0 → falls through to price_trend || 0.
        let h0 = m(&[
            ("is_proxy", Value::from(0)),
            ("printing", Value::String("Holofoil".into())),
            ("price_holofoil", Value::Null),
            ("price_trend", Value::from(2.25)),
        ]);
        assert_eq!(resolve_card_price_for_export(&h0), Value::from(2.25));
        // Normal + price_normal > 0 → price_normal (RAW value cloned, JS
        // style; sqlite_value's INTEGER affinity makes real rows Number(2)).
        let n = m(&[
            ("is_proxy", Value::from(0)),
            ("printing", Value::String("Normal".into())),
            ("price_normal", Value::from(2.0)),
            ("price_trend", Value::from(7.0)),
        ]);
        assert_eq!(resolve_card_price_for_export(&n), Value::from(2.0));
        let n2 = m(&[
            ("is_proxy", Value::from(0)),
            ("printing", Value::String("Normal".into())),
            ("price_normal", Value::from(2)),
            ("price_trend", Value::from(7.0)),
        ]);
        assert_eq!(resolve_card_price_for_export(&n2), Value::from(2));
        // no printing match, price_trend null → 0. market_price ALWAYS present.
        let z = m(&[
            ("is_proxy", Value::from(0)),
            ("printing", Value::String("".into())),
            ("price_trend", Value::Null),
        ]);
        assert_eq!(resolve_card_price_for_export(&z), Value::from(0));
        // string price > 0 coerces, and the RAW string is returned (JS).
        let s = m(&[
            ("is_proxy", Value::from(0)),
            ("printing", Value::String("Normal".into())),
            ("price_normal", Value::String("4".into())),
        ]);
        assert_eq!(resolve_card_price_for_export(&s), Value::String("4".into()));
    }

    #[test]
    fn js_number_edge_forms() {
        assert_eq!(js_number_as_f64(&Value::String("0x1f".into())), Some(31.0));
        assert_eq!(js_number_as_f64(&Value::String(" 5 ".into())), Some(5.0));
        assert_eq!(js_number_as_f64(&Value::String("".into())), Some(0.0));
        assert_eq!(js_number_as_f64(&Value::String("a".into())), None);
        assert_eq!(js_number_as_f64(&Value::Array(vec![Value::from(7)])), Some(7.0));
        assert_eq!(js_number_as_f64(&Value::Array(vec![])), Some(0.0));
        assert_eq!(js_number_as_f64(&Value::Bool(true)), Some(1.0));
        assert_eq!(js_number_as_f64(&Value::Null), Some(0.0));
    }

    #[test]
    fn js_int_and_float_edge_forms() {
        assert_eq!(js_parse_int_str(&Value::String("2x".into())), Some(2));
        assert_eq!(js_parse_int_str(&Value::String(" -12abc".into())), Some(-12));
        assert_eq!(js_parse_int_str(&Value::String("x".into())), None);
        assert_eq!(js_parse_float("2.5abc"), Some(2.5));
        assert_eq!(js_parse_float(""), None);
        assert_eq!(js_parse_float(".5"), Some(0.5));
    }
}

#[cfg(test)]
mod import_tx_tests {
    //! LANE-B review H1/H2-writes (export.rs leg): the /import transaction
    //! must ROLLBACK when the COMMIT statement itself fails. Red before the
    //! fix: `Ok(()) => conn.execute_batch("COMMIT").map_err(|_| ())` left
    //! the pooled connection inside an open transaction, holding the WAL
    //! write lock against every later writer.

    use super::*;
    use crate::db::DbPool;

    fn pool_with_schema(tag: &str) -> (tempfile::TempDir, DbPool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{tag}.db"));
        // DbPool opens READ_WRITE without CREATE — the file must exist
        // (same convention as db.rs temp_db).
        rusqlite::Connection::open(&path).unwrap();
        let pool = DbPool::open(path, 1).unwrap();
        {
            let conn = pool.get().unwrap();
            conn.execute_batch(
                "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, username TEXT UNIQUE NOT NULL, password_hash TEXT NOT NULL);
                 CREATE TABLE card_cache (
                   id TEXT PRIMARY KEY, oracle_id TEXT, scryfall_search_eligible INTEGER,
                   name TEXT, supertype TEXT, subtypes TEXT, types TEXT, rarity TEXT,
                   set_id TEXT, set_name TEXT, number TEXT, image_url TEXT,
                   price_trend REAL, price_normal REAL, price_holofoil REAL, price_etched REAL,
                   cmc REAL, color_identity TEXT, language TEXT, printed_name TEXT,
                   tcgplayer_url TEXT, cardmarket_url TEXT, tcgplayer_product_id INTEGER,
                   price_currency TEXT, price_source TEXT, oracle_text TEXT, oracle_faces TEXT,
                   price_estimated INTEGER NOT NULL DEFAULT 0,
                   last_updated DATETIME DEFAULT CURRENT_TIMESTAMP
                 );
                 CREATE TABLE collection (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   card_id TEXT NOT NULL,
                   quantity INTEGER DEFAULT 1,
                   condition TEXT, printing TEXT, language TEXT,
                   purchase_price REAL,
                   user_id INTEGER REFERENCES users(id),
                   added_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                   FOREIGN KEY(card_id) REFERENCES card_cache(id)
                 );
                 INSERT INTO users (id, username, password_hash) VALUES (1, 'u', 'x');",
            )
            .unwrap();
        }
        (dir, pool)
    }

    fn import_row(card_id: &str) -> Vec<SqlValue> {
        vec![
            SqlValue::Text(card_id.into()),
            SqlValue::Integer(1),
            SqlValue::Text("Near Mint".into()),
            SqlValue::Text("Normal".into()),
            SqlValue::Text("English".into()),
            SqlValue::Real(0.0),
            SqlValue::Integer(1),
        ]
    }

    /// Failure injection (not a parity surface): PRAGMA foreign_keys is a
    /// no-op inside a transaction, and PRAGMA defer_foreign_keys is
    /// transaction-scoped (read at COMMIT time) — so BOTH are set on the
    /// pooled connection while it is idle, BEFORE import_transaction opens
    /// its BEGIN. With FK checks deferred, the dangling card_id insert
    /// succeeds and the violation surfaces from the COMMIT statement
    /// itself, leaving the transaction open — exactly like a BUSY/IO
    /// commit failure. (Verified against the same rusqlite 0.37/bundled
    /// build: COMMIT → SQLITE_CONSTRAINT(787), is_autocommit stays false.)
    fn fail_the_commit(pool: &DbPool) {
        {
            let conn = pool.get().unwrap();
            conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        }
        // cards EMPTY on purpose: cache_normalized_cards would otherwise
        // insert 'dangling' into card_cache and the FK would be satisfied.
        let cards: Vec<Map<String, Value>> = Vec::new();
        let defer = |c: &rusqlite::Connection| c.execute_batch("PRAGMA defer_foreign_keys = ON;");
        assert!(
            import_transaction(pool, &cards, &[import_row("dangling")], Some(&defer)).is_err(),
            "deferred FK must fail the COMMIT"
        );
    }

    #[test]
    fn commit_failure_rolls_the_pooled_connection_back() {
        let (_d, pool) = pool_with_schema("commitfail");
        fail_the_commit(&pool);
        let conn = pool.get().unwrap();
        assert!(conn.is_autocommit(), "pooled conn still inside a transaction after failed COMMIT");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM collection", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "failed import must persist nothing");
        // Connection must be immediately usable for the next writer.
        conn.execute_batch("BEGIN IMMEDIATE; INSERT INTO users (username, password_hash) VALUES ('v','x'); COMMIT;")
            .unwrap();
    }

    #[test]
    fn positive_control_successful_import_commits_and_stays_clean() {
        // Guards the failure test above from passing vacuously: same rig,
        // FK satisfied → Ok, row persisted, card cached, autocommit ON.
        let (_d, pool) = pool_with_schema("importok");
        {
            let conn = pool.get().unwrap();
            conn.execute_batch("PRAGMA foreign_keys = ON; INSERT INTO card_cache (id, name) VALUES ('ok-1','Card');")
                .unwrap();
        }
        let cards: Vec<Map<String, Value>> = vec![Map::from_iter([(
            "id".to_string(),
            Value::String("ok-1".into()),
        )])];
        import_transaction(&pool, &cards, &[import_row("ok-1")], None).unwrap();
        let conn = pool.get().unwrap();
        assert!(conn.is_autocommit());
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM collection WHERE card_id = 'ok-1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "successful import must persist the row");
    }
}

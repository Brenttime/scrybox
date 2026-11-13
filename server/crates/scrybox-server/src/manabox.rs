//! Rust port of the ManaBox sync (Lane B unit 3b): `backend/src/utils/manaboxSync.js`
//! (pure planning) and `backend/src/routes/manaboxSync.js` (the four
//! /api/manabox-sync endpoints). Gated by `RUST_ROUTES=manabox_backup`.
//!
//! Planning rules (docs/manabox-sync.md, mirrored in the Node comments):
//! - Owned = rows whose Binder Type is binder or deck. Lists are never owned;
//!   they become Scrybox lists instead.
//! - Identity = exact printing (mtg-<Scryfall ID>) + printing + condition +
//!   language. The CSV's set code, collector number and name must agree with
//!   the card Scryfall returns for that id, or the row is rejected.
//! - Only collection rows that came from ManaBox (collection.source =
//!   'manabox') can be removed. Cards added in Scrybox itself are never
//!   removed, and count as owned, so nothing is added twice.
//! - ManaBox's "Added" date becomes the row's added_at.
//! - Re-running the same file is a no-op.
//!
//! Apply re-plans from the same CSV against the collection as it is now, so a
//! preview that went stale can never be applied blindly. All writes ride one
//! BEGIN IMMEDIATE transaction, exactly like Node's db.withTransaction.

use std::collections::HashMap;

use axum::{
    extract::{Path, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::auth::{authenticate, bearer_token};
use crate::card_cache::cache_normalized_cards;
use crate::card_norm::{normalize_card, scryfall_uuid};
use crate::collection_writes::{node_exact, Tx};
use crate::mfx_payload::{js_parse_int, js_to_string, js_truthy};
use crate::scryfall::ScryOpts;
use crate::stats;
use crate::AppState;

use super::cards_tags::{json_error, json_response, read_body};

// ---------------------------------------------------------------------------
// Tables of utils/manaboxSync.js
// ---------------------------------------------------------------------------

fn printing_map() -> HashMap<&'static str, &'static str> {
    [("normal", "Normal"), ("foil", "Holofoil"), ("etched", "Holofoil")]
        .into_iter()
        .collect()
}

fn condition_map() -> HashMap<&'static str, &'static str> {
    [
        ("mint", "Near Mint"),
        ("near_mint", "Near Mint"),
        ("excellent", "Lightly Played"),
        ("good", "Moderately Played"),
        ("light_played", "Lightly Played"),
        ("played", "Heavily Played"),
        ("poor", "Damaged"),
    ]
    .into_iter()
    .collect()
}

fn language_map() -> HashMap<&'static str, &'static str> {
    [
        ("en", "English"),
        ("ja", "Japanese"),
        ("de", "German"),
        ("fr", "French"),
        ("it", "Italian"),
        ("es", "Spanish"),
        ("pt", "Portuguese"),
        ("ko", "Korean"),
        ("ru", "Russian"),
        ("zhs", "Chinese Simplified"),
        ("zht", "Chinese Traditional"),
        ("ph", "Phyrexian"),
    ]
    .into_iter()
    .collect()
}

const REQUIRED: [&str; 11] = [
    "Binder Name",
    "Binder Type",
    "Name",
    "Set code",
    "Collector number",
    "Foil",
    "Quantity",
    "Scryfall ID",
    "Condition",
    "Language",
    "Added",
];

// ---------------------------------------------------------------------------
// CSV parsing — parseCsv verbatim (RFC 4180 with ManaBox quirks: a bare `"`
// anywhere outside quotes opens quoted mode, a lone \r breaks the line).
// Returns header-mapped row objects; empty text / header-only => empty vec.
// ---------------------------------------------------------------------------

pub fn parse_csv(text: &str) -> Vec<HashMap<String, String>> {
    let t = parse_csv_table(text);
    t.rows
        .iter()
        .map(|r| t.cols.iter().map(|(h, &i)| (h.clone(), r.get(i).cloned().unwrap_or_default())).collect())
        .collect()
}

/// Header-indexed CSV (review exec-sync M2): rows stay `Vec<String>`, the
/// header maps to its LAST column index (Object.fromEntries: last dup wins).
pub struct CsvTable {
    cols: HashMap<String, usize>,
    rows: Vec<Vec<String>>,
}

impl CsvTable {
    fn has(&self, k: &str) -> bool {
        self.cols.contains_key(k)
    }
    fn cell(&self, r: usize, k: &str) -> Option<String> {
        self.cols.get(k).map(|&i| self.rows[r].get(i).cloned().unwrap_or_default())
    }
}

/// Single linear pass over the bytes. Every delimiter is ASCII, so fields
/// split only on char boundaries and each field is valid UTF-8.
fn parse_csv_table(text: &str) -> CsvTable {
    let b = text.strip_prefix('\u{FEFF}').unwrap_or(text).as_bytes();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field: Vec<u8> = Vec::new();
    let take = |f: &mut Vec<u8>| String::from_utf8(std::mem::take(f)).unwrap_or_default();
    let mut q = false;
    let mut i = 0usize;
    while i < b.len() {
        let ch = b[i];
        if q {
            if ch == b'"' {
                if i + 1 < b.len() && b[i + 1] == b'"' {
                    field.push(b'"');
                    i += 2;
                    continue;
                }
                q = false;
            } else {
                field.push(ch);
            }
        } else if ch == b'"' {
            q = true;
        } else if ch == b',' {
            row.push(take(&mut field));
        } else if ch == b'\n' || ch == b'\r' {
            if ch == b'\r' && i + 1 < b.len() && b[i + 1] == b'\n' {
                i += 1;
            }
            row.push(take(&mut field));
            if row.len() > 1 || row[0] != "" {
                rows.push(std::mem::take(&mut row));
            }
            row.clear();
        } else {
            field.push(ch);
        }
        i += 1;
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(take(&mut field));
        rows.push(row);
    }
    if rows.is_empty() {
        return CsvTable { cols: HashMap::new(), rows: Vec::new() };
    }
    let mut cols = HashMap::new();
    for (i, h) in rows[0].iter().enumerate() {
        cols.insert(h.trim().to_string(), i);
    }
    rows.remove(0);
    CsvTable { cols, rows }
}

fn to_sql_date(iso: Option<&String>) -> Option<String> {
    let s = match iso {
        Some(v) if !v.is_empty() => v.as_str(),
        _ => "",
    };
    let b = s.as_bytes();
    let digit = |i: usize| b.get(i).copied().map_or(false, |c| c.is_ascii_digit());
    let ok = b.len() >= 19
        && (0..4).all(digit)
        && b[4] == b'-'
        && (5..7).all(digit)
        && b[7] == b'-'
        && (8..10).all(digit)
        && b[10] == b'T'
        && (11..13).all(digit)
        && b[13] == b':'
        && (14..16).all(digit)
        && b[16] == b':'
        && (17..19).all(digit);
    if !ok {
        return None;
    }
    Some(format!("{} {}", &s[0..10], &s[11..19]))
}

// ---------------------------------------------------------------------------
// parseManaboxExport
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Expectation {
    pub set: String,
    pub number: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct OwnedRow {
    pub key: String,
    pub card_id: String,
    pub printing: String,
    pub condition: String,
    pub language: String,
    pub dates: Vec<Option<String>>,
    pub quantity: i64,
    pub purchase_price: Option<f64>,
    pub expect: Expectation,
}

#[derive(Clone, Debug)]
pub struct ListEntry {
    pub card_id: String,
    pub quantity: i64,
    pub expect: Expectation,
}

#[derive(Clone, Debug)]
pub struct Rejected {
    pub line: Option<i64>,
    pub name: String,
    pub reason: String,
    #[allow(dead_code)]
    pub quantity: Option<i64>,
}

#[derive(Debug)]
pub struct Parsed {
    pub rows: usize,
    pub owned: Vec<OwnedRow>,
    /// Insertion-ordered list name -> insertion-ordered cardId -> entry.
    pub lists: Vec<(String, Vec<(String, ListEntry)>)>,
    pub errors: Vec<Rejected>,
}

fn js_parse_float(v: Option<&String>) -> f64 {
    // JS parseFloat over a CSV cell: leading whitespace, optional sign, then
    // the longest StrUnsignedDecimalLiteral prefix — NOT hex (parseFloat
    // stops at 'x'), NOT a bare 'inf'/'nan' word. 'Infinity' after the sign
    // is the one word it accepts.
    let Some(s) = v else { return f64::NAN };
    let t = s.trim_start();
    let b = t.as_bytes();
    let mut i = 0usize;
    let neg = i < b.len() && b[i] == b'-';
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    if t[i..].starts_with("Infinity") {
        return if neg { f64::NEG_INFINITY } else { f64::INFINITY };
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
            // '.' with no digits on either side ('.', '.e5') -> NaN.
            end = dot;
        }
    }
    if end < b.len() && (b[end] | 0x20) == b'e' {
        let mut k = end + 1;
        if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
            k += 1;
        }
        let e0 = k;
        while k < b.len() && (b[k] as char).is_ascii_digit() {
            k += 1;
        }
        if k > e0 {
            end = k;
        }
    }
    if end == d0 {
        return f64::NAN;
    }
    t[..end].parse::<f64>().unwrap_or(f64::NAN)
}

fn sid_ok(sid: &str) -> bool {
    sid.len() == 36
        && sid
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c) || c == b'-')
}

pub fn parse_manabox_export(text: &str) -> Result<Parsed, String> {
    let table = parse_csv_table(text);
    if table.rows.is_empty() {
        return Err("The file is empty.".to_string());
    }
    let missing: Vec<&str> = REQUIRED.iter().copied().filter(|h| !table.has(h)).collect();
    if !missing.is_empty() {
        return Err(format!(
            "This is not a ManaBox collection export (missing {}).",
            missing.join(", ")
        ));
    }
    let printing = printing_map();
    let condition = condition_map();
    let language = language_map();
    let mut owned: Vec<OwnedRow> = Vec::new();
    let mut owned_idx: HashMap<String, usize> = HashMap::new();
    let mut lists: Vec<(String, Vec<(String, ListEntry)>)> = Vec::new();
    // First-seen order lives in the Vecs; these maps only index into them.
    let mut list_idx: HashMap<String, usize> = HashMap::new();
    let mut entry_idx: Vec<HashMap<String, usize>> = Vec::new();
    let mut errors: Vec<Rejected> = Vec::new();
    for i in 0..table.rows.len() {
        let line = i as i64 + 2;
        let cell = |k: &str| table.cell(i, k);
        let sid = cell("Scryfall ID").unwrap_or_default().trim().to_lowercase();
        let qty = js_parse_int(Some(&Value::String(cell("Quantity").unwrap_or_default())));
        if !sid_ok(&sid) {
            errors.push(Rejected {
                line: Some(line),
                name: cell("Name").unwrap_or_default(),
                reason: "missing Scryfall ID".to_string(),
                quantity: qty,
            });
            continue;
        }
        if !(qty.unwrap_or(i64::MIN) > 0) {
            errors.push(Rejected {
                line: Some(line),
                name: cell("Name").unwrap_or_default(),
                reason: "bad quantity".to_string(),
                quantity: qty,
            });
            continue;
        }
        let qty = qty.unwrap();
        let expect = Expectation {
            set: cell("Set code").unwrap_or_default().to_lowercase(),
            number: cell("Collector number").unwrap_or_default(),
            name: cell("Name").unwrap_or_default(),
        };
        let card_id = format!("mtg-{sid}");
        if cell("Binder Type").as_deref() == Some("list") {
            let name = match cell("Binder Name") {
                Some(v) if !v.is_empty() => v,
                _ => "ManaBox list".to_string(),
            };
            let li = match list_idx.get(&name) {
                Some(x) => *x,
                None => {
                    lists.push((name.clone(), Vec::new()));
                    entry_idx.push(HashMap::new());
                    list_idx.insert(name, lists.len() - 1);
                    lists.len() - 1
                }
            };
            let entries = &mut lists[li].1;
            match entry_idx[li].get(&card_id) {
                Some(&ei) => entries[ei].1.quantity += qty,
                None => {
                    entry_idx[li].insert(card_id.clone(), entries.len());
                    entries.push((
                        card_id.clone(),
                        ListEntry { card_id: card_id.clone(), quantity: qty, expect: expect.clone() },
                    ))
                }
            }
            continue;
        }
        let foil_cell = cell("Foil");
        let cond_cell = cell("Condition");
        let lang_cell = cell("Language");
        let p = foil_cell.as_ref().and_then(|v| printing.get(v.as_str())).copied();
        let c = cond_cell.as_ref().and_then(|v| condition.get(v.as_str())).copied();
        let l = lang_cell.as_ref().and_then(|v| language.get(v.as_str())).copied();
        if p.is_none() || c.is_none() || l.is_none() {
            let (which, val) = match (p, c, l) {
                (None, _, _) => ("foil", foil_cell.unwrap_or_default()),
                (Some(_), None, _) => ("condition", cond_cell.unwrap_or_default()),
                _ => ("language", lang_cell.unwrap_or_default()),
            };
            errors.push(Rejected {
                line: Some(line),
                name: cell("Name").unwrap_or_default(),
                reason: format!("unknown {which} \"{val}\""),
                quantity: Some(qty),
            });
            continue;
        }
        let (p, c, l) = (p.unwrap(), c.unwrap(), l.unwrap());
        let key = format!("{card_id}|{p}|{c}|{l}");
        let d = to_sql_date(cell("Added").as_ref());
        let price = js_parse_float(cell("Purchase price").as_ref());
        let oi = match owned_idx.get(&key) {
            Some(x) => *x,
            None => {
                owned.push(OwnedRow {
                    key: key.clone(),
                    card_id: card_id.clone(),
                    printing: p.to_string(),
                    condition: c.to_string(),
                    language: l.to_string(),
                    dates: Vec::new(),
                    quantity: 0,
                    purchase_price: None,
                    expect: expect.clone(),
                });
                owned_idx.insert(key.clone(), owned.len() - 1);
                owned.len() - 1
            }
        };
        let o = &mut owned[oi];
        o.quantity += qty;
        for _ in 0..qty {
            o.dates.push(d.clone());
        }
        if price > 0.0 && o.purchase_price.is_none() {
            o.purchase_price = if price.is_finite() { Some(price) } else { Some(price) };
        }
    }
    for o in owned.iter_mut() {
        // sort by String(a || '') — ASCII dates, code-point order == localeCompare.
        o.dates.sort_by(|a, b| {
            let ka = a.clone().unwrap_or_default();
            let kb = b.clone().unwrap_or_default();
            ka.cmp(&kb)
        });
    }
    Ok(Parsed { rows: table.rows.len(), owned, lists, errors })
}

// ---------------------------------------------------------------------------
// identityMismatch
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CardRow {
    pub id: String,
    pub name: Option<String>,
    pub printed_name: Option<String>,
    pub set_id: Option<String>,
    pub number: Option<String>,
}

fn face(s: Option<&String>) -> String {
    let v = s.cloned().unwrap_or_default();
    v.split(" // ").next().unwrap_or("").trim().to_lowercase()
}

pub fn identity_mismatch(expect: &Expectation, card: Option<&CardRow>) -> Option<String> {
    // `if (!card) return 'Scryfall has no card with this ID'` — a missing
    // card is a REJECT, not a pass.
    let Some(card) = card else {
        return Some("Scryfall has no card with this ID".to_string());
    };
    // String(card.set_id || '') — a non-empty string is kept, then mtg- stripped.
    let set = match &card.set_id {
        Some(v) if !v.is_empty() => {
            let t = v.strip_prefix("mtg-").unwrap_or(v);
            t.to_lowercase()
        }
        _ => String::new(),
    };
    if !expect.set.is_empty() && set != expect.set {
        return Some(format!("set {} vs {}", expect.set.to_uppercase(), set.to_uppercase()));
    }
    if !expect.number.is_empty() {
        let cn = card.number.clone().unwrap_or_else(|| "null".to_string());
        if cn != expect.number {
            return Some(format!("collector number {} vs {}", expect.number, cn));
        }
    }
    if !expect.name.is_empty()
        && face(card.name.as_ref()) != face(Some(&expect.name))
        && face(card.printed_name.as_ref()) != face(Some(&expect.name))
    {
        return Some(format!(
            "name \"{}\" vs \"{}\"",
            expect.name,
            card.name.clone().unwrap_or_else(|| "null".to_string())
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// planCollection
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CollRow {
    pub id: i64,
    pub card_id: String,
    pub quantity: i64,
    pub printing: Option<String>,
    pub condition: Option<String>,
    pub language: Option<String>,
    pub added_at: Option<String>,
    pub source: Option<String>,
}

fn js_str(o: &Option<String>) -> String {
    // `${v}` over a node-sqlite3 column value: null -> "null".
    o.clone().unwrap_or_else(|| "null".to_string())
}

fn key_of_row(c: &CollRow) -> String {
    format!("{}|{}|{}|{}", c.card_id, js_str(&c.printing), js_str(&c.condition), js_str(&c.language))
}

#[derive(Clone, Debug, Default)]
pub struct AddRow {
    pub card_id: String,
    pub printing: String,
    pub condition: String,
    pub language: String,
    pub purchase_price: Option<f64>,
    pub added_at: Option<String>,
}

#[derive(Debug, Default)]
pub struct CollPlan {
    pub remove: Vec<CollRow>,
    pub redate: Vec<(i64, String)>,
    pub add: Vec<AddRow>,
}

pub fn plan_collection(rows: &[CollRow], owned: &[OwnedRow]) -> CollPlan {
    let mut want: HashMap<String, usize> = HashMap::new();
    for (i, o) in owned.iter().enumerate() {
        want.insert(o.key.clone(), i);
    }
    let mut by_key: Vec<(String, Vec<usize>, i64, i64)> = Vec::new(); // key, manabox row-idxs, hand, total
    let mut by_idx: HashMap<String, usize> = HashMap::new();
    for (i, c) in rows.iter().enumerate() {
        let k = key_of_row(c);
        let gi = match by_idx.get(&k) {
            Some(x) => *x,
            None => {
                by_key.push((k.clone(), Vec::new(), 0, 0));
                by_idx.insert(k.clone(), by_key.len() - 1);
                by_key.len() - 1
            }
        };
        let g = &mut by_key[gi];
        g.3 += c.quantity;
        if c.source.as_deref() == Some("manabox") {
            g.1.push(i);
        } else {
            g.2 += c.quantity;
        }
    }
    let mut keys: Vec<String> = by_key.iter().map(|(k, ..)| k.clone()).collect();
    let key_set: std::collections::HashSet<String> = keys.iter().cloned().collect();
    for o in owned {
        if !key_set.contains(&o.key) {
            keys.push(o.key.clone());
        }
    }
    let mut plan = CollPlan::default();
    for k in keys {
        let gi = by_idx.get(&k).copied();
        let manabox: Vec<usize> = gi.map(|x| by_key[x].1.clone()).unwrap_or_default();
        let total = gi.map(|x| by_key[x].3).unwrap_or(0);
        let o_idx = want.get(&k).copied();
        let owned_qty = o_idx.map(|x| owned[x].quantity).unwrap_or(0);
        let mut dates: Vec<Option<String>> = o_idx.map(|x| owned[x].dates.clone()).unwrap_or_default();
        let mut keep = owned_qty;
        let mut sorted = manabox.clone();
        sorted.sort_by_key(|&i| rows[i].id); // (a,b) => a.id - b.id, stable
        let mut removed_qty_this_key: i64 = 0;
        for &i in &sorted {
            let c = &rows[i];
            if keep >= c.quantity {
                keep -= c.quantity;
                let take = (c.quantity as usize).min(dates.len());
                let d = dates.first().cloned().flatten();
                dates.drain(..take);
                if let Some(d) = d {
                    if Some(&d) != c.added_at.as_ref() {
                        plan.redate.push((c.id, d));
                    }
                }
            } else {
                removed_qty_this_key += c.quantity;
                plan.remove.push(c.clone());
            }
        }
        let kept = total - removed_qty_this_key;
        if let Some(oi) = o_idx {
            let o = &owned[oi];
            let mut n = kept;
            while n < owned_qty {
                plan.add.push(AddRow {
                    card_id: o.card_id.clone(),
                    printing: o.printing.clone(),
                    condition: o.condition.clone(),
                    language: o.language.clone(),
                    purchase_price: o.purchase_price,
                    added_at: if dates.is_empty() { None } else { dates.remove(0).filter(|s| !s.is_empty()) },
                });
                n += 1;
            }
        }
    }
    plan
}

// ---------------------------------------------------------------------------
// planLists
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ExistingList {
    pub id: i64,
    pub name: String,
    pub source: Option<String>,
    pub cards: Vec<(String, i64)>,
}

#[derive(Debug, Default)]
pub struct ListsPlan {
    pub create: Vec<(String, Vec<ListEntry>)>,
    pub update: Vec<(i64, String, Vec<ListEntry>)>,
    pub skipped: Vec<String>,
}

pub fn plan_lists(existing: &[ExistingList], csv_lists: &[(String, Vec<(String, ListEntry)>)]) -> ListsPlan {
    let mut out = ListsPlan::default();
    for (name, cards) in csv_lists {
        let entries: Vec<ListEntry> = cards.iter().map(|(_, e)| e.clone()).collect();
        let ex = existing.iter().find(|l| l.name == *name);
        let Some(ex) = ex else {
            out.create.push((name.clone(), entries));
            continue;
        };
        if ex.source.as_deref() != Some("manabox") {
            out.skipped.push(name.clone());
            continue;
        }
        let same = ex.cards.len() == entries.len()
            && entries.iter().all(|e| ex.cards.iter().find(|(cid, _)| cid == &e.card_id).map(|(_, q)| *q) == Some(e.quantity));
        if !same {
            out.update.push((ex.id, name.clone(), entries));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// cardsById — cache probe + rate-limited bulk fetch on the shared paced queue
// ---------------------------------------------------------------------------

const CACHE_CHUNK: usize = 500;
const COLLECTION_BATCH: usize = 75;

fn q(list: &[SqlValue]) -> impl rusqlite::Params + '_ {
    rusqlite::params_from_iter(list.iter().map(|p| p as &dyn rusqlite::ToSql).collect::<Vec<_>>())
}

fn text_of(m: &Map<String, Value>, k: &str) -> Option<String> {
    m.get(k).and_then(|v| v.as_str()).map(|s| s.to_string())
}

pub async fn cards_by_id(st: &AppState, ids: &[String]) -> Result<HashMap<String, CardRow>, String> {
    let mut list: Vec<String> = Vec::new();
    let mut seen: HashMap<&str, ()> = HashMap::new();
    for id in ids {
        if seen.insert(id.as_str(), ()).is_none() {
            list.push(id.clone());
        }
    }
    let mut out: HashMap<String, CardRow> = HashMap::new();
    let pool = st.db.as_ref().ok_or("no db")?;
    for chunk in list.chunks(CACHE_CHUNK) {
        let conn = pool.get().map_err(|e| e.to_string())?;
        let ph = vec!["?"; chunk.len()].join(",");
        let params: Vec<SqlValue> = chunk.iter().map(|s| SqlValue::Text(s.clone())).collect();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id, name, printed_name, set_id, number FROM card_cache WHERE id IN ({ph})"
            ))
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query_map(q(&params), |r| {
            Ok(CardRow {
                id: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                name: r.get::<_, Option<String>>(1)?,
                printed_name: r.get::<_, Option<String>>(2)?,
                set_id: r.get::<_, Option<String>>(3)?,
                number: r.get::<_, Option<String>>(4)?,
            })
        }).map_err(|e| e.to_string())?;
        while let Some(r) = rows.next() {
            let r = r.map_err(|e| e.to_string())?;
            out.insert(r.id.clone(), r);
        }
    }
    let missing: Vec<String> = list.iter().filter(|id| !out.contains_key(*id)).cloned().collect();
    if !missing.is_empty() {
        let mut fresh: Vec<Map<String, Value>> = Vec::new();
        for chunk in missing.chunks(COLLECTION_BATCH) {
            let identifiers: Vec<Value> = chunk
                .iter()
                .map(|id| match scryfall_uuid(Some(id)) {
                    Some(uuid) => serde_json::json!({ "id": uuid }),
                    None => serde_json::json!({ "name": "" }),
                })
                .collect();
            let mut body = Map::new();
            body.insert("identifiers".into(), Value::Array(identifiers));
            let resp = st
                .scry
                // Node: scryPostRetried default retries = 4 attempts on 429.
                .post_retried("/cards/collection", Value::Object(body), &ScryOpts::default(), 4)
                .await
                .map_err(|e| e.to_string())?;
            if let Some(data) = resp.as_ref().and_then(|v| v.get("data")).and_then(|v| v.as_array()) {
                for raw in data {
                    if let Some(norm) = normalize_card(raw, None).as_object().cloned() {
                        fresh.push(norm);
                    }
                }
            }
        }
        if !fresh.is_empty() {
            let conn = pool.get().map_err(|e| e.to_string())?;
            cache_normalized_cards(&conn, &fresh).map_err(|e| e.to_string())?;
            for c in &fresh {
                let id = text_of(c, "id").unwrap_or_default();
                out.insert(
                    id.clone(),
                    CardRow {
                        id,
                        name: text_of(c, "name"),
                        printed_name: text_of(c, "printed_name"),
                        set_id: text_of(c, "set_id"),
                        number: text_of(c, "number"),
                    },
                );
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// buildPlan + summary + report
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Summary {
    pub csv_rows: i64,
    pub owned: i64,
    pub before: i64,
    pub after: i64,
    pub add: i64,
    pub remove: i64,
    pub redate: i64,
    pub hand_added: i64,
    pub lists_create: i64,
    pub lists_update: i64,
    pub lists_skipped: Vec<String>,
    pub rejected: i64,
}

impl Summary {
    pub fn value(&self) -> Value {
        let mut m = Map::new();
        m.insert("csvRows".into(), Value::from(self.csv_rows));
        m.insert("owned".into(), Value::from(self.owned));
        m.insert("before".into(), Value::from(self.before));
        m.insert("after".into(), Value::from(self.after));
        m.insert("add".into(), Value::from(self.add));
        m.insert("remove".into(), Value::from(self.remove));
        m.insert("redate".into(), Value::from(self.redate));
        m.insert("handAdded".into(), Value::from(self.hand_added));
        m.insert("listsCreate".into(), Value::from(self.lists_create));
        m.insert("listsUpdate".into(), Value::from(self.lists_update));
        m.insert("listsSkipped".into(), Value::Array(self.lists_skipped.iter().map(|s| Value::String(s.clone())).collect()));
        m.insert("rejected".into(), Value::from(self.rejected));
        Value::Object(m)
    }
}

#[derive(Debug)]
pub struct Plan {
    pub summary: Summary,
    pub coll: CollPlan,
    pub lists: ListsPlan,
    pub rejected: Vec<Rejected>,
    pub cards: HashMap<String, CardRow>,
}

pub enum BuildOutcome {
    Plan(Plan),
    /// parseManaboxExport's `error` field — the route answers 400 {error}.
    BadRequest(String),
    /// A thrown leg (Scryfall fetch, DB) — the route's catch arm.
    ServerError(String),
}

pub async fn build_plan(st: &AppState, user_id: i64, csv: Value, include_lists: bool) -> BuildOutcome {
    // req.body?.csv — parseCsv does String(text): a missing key stringifies
    // undefined/null to 'undefined'/'null' — a one-cell CSV whose rows.slice(1)
    // is empty, so every non-string csv ends up 'The file is empty.'. js_to_string
    // mirrors JS String() for the exotic shapes.
    let csv_text = match csv {
        Value::String(s) => s,
        other => js_to_string(&other),
    };
    // Review exec-sync M2: a 40 MB parse must not pin a runtime worker.
    let parsed = match tokio::task::spawn_blocking(move || parse_manabox_export(&csv_text)).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return BuildOutcome::BadRequest(e),
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    };
    let mut entry_ids: Vec<String> = parsed.owned.iter().map(|o| o.card_id.clone()).collect();
    for (_, m) in &parsed.lists {
        for (cid, _) in m {
            entry_ids.push(cid.clone());
        }
    }
    let cards = match cards_by_id(st, &entry_ids).await {
        Ok(c) => c,
        Err(e) => return BuildOutcome::ServerError(e),
    };
    // Every printing must agree with the CSV's set, collector number and name.
    let mut rejected = parsed.errors.clone();
    let owned: Vec<OwnedRow> = parsed
        .owned
        .iter()
        .filter(|o| match identity_mismatch(&o.expect, cards.get(&o.card_id)) {
            Some(why) => {
                rejected.push(Rejected {
                    line: None,
                    name: o.expect.name.clone(),
                    reason: why,
                    quantity: Some(o.quantity),
                });
                false
            }
            None => true,
        })
        .cloned()
        .collect();
    // List-entry rejects delete from the (name, entries) vecs.
    let mut lists: Vec<(String, Vec<(String, ListEntry)>)> = Vec::new();
    for (name, m) in &parsed.lists {
        let mut kept_entries = Vec::new();
        for (cid, e) in m {
            match identity_mismatch(&e.expect, cards.get(cid)) {
                Some(why) => rejected.push(Rejected {
                    line: None,
                    name: e.expect.name.clone(),
                    reason: format!("{why} (list {name})"),
                    quantity: Some(e.quantity),
                }),
                None => kept_entries.push((cid.clone(), e.clone())),
            }
        }
        lists.push((name.clone(), kept_entries));
    }

    let pool = match st.db.as_ref() {
        Some(p) => p,
        None => return BuildOutcome::ServerError("no db".into()),
    };
    let rows: Vec<CollRow> = {
        let conn = match pool.get() {
            Ok(c) => c,
            Err(e) => return BuildOutcome::ServerError(e.to_string()),
        };
        let mut stmt = match conn.prepare(
            "SELECT id, card_id, quantity, printing, condition, language, added_at, source FROM collection WHERE user_id = ?",
        ) {
            Ok(s) => s,
            Err(e) => return BuildOutcome::ServerError(e.to_string()),
        };
        let params = vec![SqlValue::Integer(user_id)];
        let r = stmt.query_map(q(&params), |r| {
            Ok(CollRow {
                id: r.get(0)?,
                card_id: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                quantity: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                printing: r.get::<_, Option<String>>(3)?,
                condition: r.get::<_, Option<String>>(4)?,
                language: r.get::<_, Option<String>>(5)?,
                added_at: r.get::<_, Option<String>>(6)?,
                source: r.get::<_, Option<String>>(7)?,
            })
        });
        match r {
            Ok(rows) => match rows.collect::<Result<Vec<_>, _>>() {
                Ok(v) => v,
                Err(e) => return BuildOutcome::ServerError(e.to_string()),
            },
            Err(e) => return BuildOutcome::ServerError(e.to_string()),
        }
    };
    let coll = plan_collection(&rows, &owned);

    let lists_plan = if include_lists {
        let conn = match pool.get() {
            Ok(c) => c,
            Err(e) => return BuildOutcome::ServerError(e.to_string()),
        };
        let ls: Vec<(i64, String, Option<String>)> = {
            let mut stmt = match conn.prepare("SELECT id, name, source FROM card_lists WHERE user_id = ?") {
                Ok(s) => s,
                Err(e) => return BuildOutcome::ServerError(e.to_string()),
            };
            let params = vec![SqlValue::Integer(user_id)];
            let r = stmt.query_map(q(&params), |r| {
                Ok((r.get(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get::<_, Option<String>>(2)?))
            });
            match r {
                Ok(rows) => match rows.collect::<Result<Vec<_>, _>>() {
                    Ok(v) => v,
                    Err(e) => return BuildOutcome::ServerError(e.to_string()),
                },
                Err(e) => return BuildOutcome::ServerError(e.to_string()),
            }
        };
        let existing: Option<Vec<ExistingList>> = ls
            .into_iter()
            .map(|(id, name, source)| {
                let mut stmt = conn.prepare("SELECT card_id, quantity FROM list_cards WHERE list_id = ?").ok()?;
                let params = vec![SqlValue::Integer(id)];
                let cards = stmt
                    .query_map(q(&params), |r| {
                        Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get::<_, Option<i64>>(1)?.unwrap_or(0)))
                    })
                    .ok()?
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?;
                Some(ExistingList { id, name, source, cards })
            })
            .collect();
        let Some(existing) = existing else {
            return BuildOutcome::ServerError("list query failed".into());
        };
        plan_lists(&existing, &lists)
    } else {
        ListsPlan::default()
    };

    let before: i64 = rows.iter().map(|r| r.quantity).sum();
    let removed: i64 = coll.remove.iter().map(|r| r.quantity).sum();
    let summary = Summary {
        csv_rows: parsed.rows as i64,
        owned: owned.iter().map(|o| o.quantity).sum(),
        before,
        after: before - removed + coll.add.len() as i64,
        add: coll.add.len() as i64,
        remove: removed,
        redate: coll.redate.len() as i64,
        hand_added: rows.iter().filter(|r| r.source.as_deref() != Some("manabox")).count() as i64,
        lists_create: lists_plan.create.len() as i64,
        lists_update: lists_plan.update.len() as i64,
        lists_skipped: lists_plan.skipped.clone(),
        rejected: rejected.len() as i64,
    };
    BuildOutcome::Plan(Plan { summary, coll, lists: lists_plan, rejected, cards })
}

fn now_stamp() -> String {
    // new Date().toISOString().slice(0, 16).replace('T', ' ')
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "1970-01-01 00:00".to_string())
}

fn js_slice10(s: &str) -> String {
    s.chars().take(10).collect()
}

pub fn report(plan: &Plan, applied: bool, file_name: Option<&Value>) -> String {
    let s = &plan.summary;
    let nm = |id: &str| -> String {
        match plan.cards.get(id) {
            Some(c) => format!(
                "{} | {} {}",
                c.name.clone().unwrap_or_else(|| "null".to_string()),
                c.set_id.clone().unwrap_or_else(|| "null".to_string()).to_uppercase(),
                c.number.clone().unwrap_or_else(|| "null".to_string())
            ),
            None => format!("{id} | ?"),
        }
    };
    let file_label = match file_name {
        Some(v) if js_truthy(Some(v)) => js_to_string(v),
        _ => "ManaBox export".to_string(),
    };
    let skipped = if s.lists_skipped.is_empty() {
        String::new()
    } else {
        format!(", skipped (a Scrybox list already has the name): {}", s.lists_skipped.join(", "))
    };
    let mut l: Vec<String> = Vec::new();
    l.push(format!(
        "# ManaBox sync {} UTC ({})",
        now_stamp(),
        if applied { "APPLIED" } else { "PREVIEW - nothing changed" }
    ));
    l.push(String::new());
    l.push(format!("- File: {file_label} ({} rows, {} owned copies)", s.csv_rows, s.owned));
    l.push(format!("- Collection: {} -> {} copies", s.before, s.after));
    l.push(format!("- Added: **{}**, removed: **{}**, dates set from ManaBox: {}", s.add, s.remove, s.redate));
    l.push(format!("- Kept untouched: {} rows added in Scrybox itself", s.hand_added));
    l.push(format!(
        "- Lists created: {}, updated: {}{skipped}",
        s.lists_create, s.lists_update
    ));
    l.push(format!(
        "- Rejected rows (set/number/name did not match the Scryfall ID, or unreadable): {}",
        s.rejected
    ));
    l.push(String::new());
    l.push("## Removed".into());
    l.push(String::new());
    l.push("Row | Card | Set # | Printing | Condition | Language | Added".into());
    l.push("---|---|---|---|---|---|---".into());
    for r in &plan.coll.remove {
        l.push(format!(
            "{} | {} | {} | {} | {} | {}",
            r.id,
            nm(&r.card_id),
            js_str(&r.printing),
            js_str(&r.condition),
            js_str(&r.language),
            js_slice10(&r.added_at.clone().unwrap_or_else(|| "null".to_string()))
        ));
    }
    l.push(String::new());
    l.push("## Added".into());
    l.push(String::new());
    l.push("Card | Set # | Printing | Condition | Language | ManaBox added".into());
    l.push("---|---|---|---|---|---".into());
    for a in &plan.coll.add {
        l.push(format!(
            "{} | {} | {} | {} | {}",
            nm(&a.card_id),
            a.printing,
            a.condition,
            a.language,
            js_slice10(&a.added_at.clone().unwrap_or_default())
        ));
    }
    l.push(String::new());
    l.push("## Lists".into());
    l.push(String::new());
    for (name, cards) in &plan.lists.create {
        let qty: i64 = cards.iter().map(|c| c.quantity).sum();
        l.push(format!("- Created **{name}** ({qty} cards)"));
    }
    for (_, name, cards) in &plan.lists.update {
        let qty: i64 = cards.iter().map(|c| c.quantity).sum();
        l.push(format!("- Updated **{name}** to {qty} cards"));
    }
    l.push(String::new());
    l.push("## Rejected".into());
    l.push(String::new());
    for r in &plan.rejected {
        let prefix = match r.line {
            Some(n) if n != 0 => format!("line {n}: "),
            _ => String::new(),
        };
        l.push(format!("- {prefix}{}: {}", r.name, r.reason));
    }
    l.join("\n") + "\n"
}

// ---------------------------------------------------------------------------
// Route plumbing
// ---------------------------------------------------------------------------

/// Test-visible JS Number() with the NaN collapsed to None.
pub fn js_number_pub(v: &Value) -> Option<f64> {
    let f = js_number(v);
    if f.is_nan() {
        None
    } else {
        Some(f)
    }
}

/// `req.body?.expectBefore != null && Number(req.body.expectBefore) !== before`
fn js_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => *b as i64 as f64,
        Value::Number(n) => n.as_f64().or_else(|| n.as_i64().map(|i| i as f64)).unwrap_or(f64::NAN),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                return 0.0;
            }
            let sign = if t.starts_with('+') || t.starts_with('-') { &t[..1] } else { "" };
            let body = if sign.is_empty() { t } else { &t[1..] };
            if body == "Infinity" {
                return if sign == "-" { f64::NEG_INFINITY } else { f64::INFINITY };
            }
            if body.starts_with("0x") || body.starts_with("0X") {
                return i64::from_str_radix(&body[2..], 16).map(|v| (if sign == "-" { -v } else { v }) as f64).unwrap_or(f64::NAN);
            }
            // JS StringNumericLiteral is strict about the WHOLE string.
            let ok = !body.is_empty()
                && !body.eq_ignore_ascii_case("inf")
                && !body.eq_ignore_ascii_case("infinity")
                && !body.eq_ignore_ascii_case("nan");
            if ok {
                t.parse::<f64>().unwrap_or(f64::NAN)
            } else {
                f64::NAN
            }
        }
        Value::Array(items) => match items.len() {
            0 => 0.0,
            1 => js_number(&items[0]),
            _ => f64::NAN,
        },
        Value::Object(_) => f64::NAN,
    }
}

/// The router mounts only the global authenticateToken (server.js
/// `app.use('/api', authenticateToken)`) — /api/manabox-sync has NO
/// requireAdmin: any logged-in session may sync their own collection.
fn authed(st: &AppState, headers: &HeaderMap, is_get: bool) -> Result<crate::auth::AuthUser, Response> {
    let pool = st.db.as_ref().ok_or_else(|| json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;
    let token = bearer_token(headers).map(|s| s.to_string());
    authenticate(pool, token.as_deref(), is_get).map_err(|res| res)
}

/// The `express.json({ limit: '40mb' })` mounted on /api/manabox-sync AFTER
/// the auth gate: size/parse errors outrank requireAdmin, non-JSON bodies
/// leave req.body undefined (=> csv undefined => 'The file is empty.').
enum BigOutcome {
    Parsed(Value),
    TooLarge,
    Invalid,
}

fn is_json_ct(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase() == "application/json")
        .unwrap_or(false)
}

fn parse_body_40mb(headers: &HeaderMap, bytes: &[u8]) -> BigOutcome {
    if !is_json_ct(headers) {
        return BigOutcome::Parsed(Value::Object(Map::new()));
    }
    if bytes.len() > 40 * 1024 * 1024 {
        return BigOutcome::TooLarge;
    }
    if bytes.is_empty() {
        return BigOutcome::Parsed(Value::Object(Map::new()));
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(v) if v.is_object() => BigOutcome::Parsed(v),
        // strict:true allows arrays; they destructure to nothing useful.
        Ok(v) if v.is_array() => BigOutcome::Parsed(Value::Object(Map::new())),
        _ => BigOutcome::Invalid,
    }
}

fn owns_manabox(st: &AppState) -> bool {
    st.owns("manabox_backup") && st.db.is_some()
}

pub async fn handle_preview(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::POST || !owns_manabox(&st) {
        return crate::proxy(State(st), req).await;
    }
    let (parts, bytes, oversize) = read_body(req, crate::collection_writes::BODY_MANABOX).await;
    let headers = parts.headers.clone();
    let user = match authed(&st, &headers, false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    // bigJson (40mb, server.js:464) mounts AFTER the auth gate: oversize
    // JSON answers the parser's 413 only once authenticateToken passed.
    // Non-JSON content-types skip the parser entirely — empty body is the
    // equivalent (content-length was cleared with the abandoned stream).
    if oversize && is_json_ct(&headers) {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Upload too large. Try exporting/importing in smaller batches.");
    }
    let body = match parse_body_40mb(&headers, &bytes) {
        BigOutcome::TooLarge => {
            return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Upload too large. Try exporting/importing in smaller batches.")
        }
        BigOutcome::Invalid => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
        BigOutcome::Parsed(v) => v,
    };
    let include_lists = match body.get("includeLists") {
        Some(v) => !js_truthy_strict_ne_false(v),
        None => true,
    };
    match build_plan(&st, user.id, body.get("csv").cloned().unwrap_or(Value::Null), include_lists).await {
        BuildOutcome::BadRequest(e) => json_response(StatusCode::BAD_REQUEST, &err_body(&e)),
        BuildOutcome::ServerError(e) => {
            tracing::error!("[manabox-sync] preview {e}");
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &err_body("Could not read the ManaBox export."),
            )
        }
        BuildOutcome::Plan(plan) => {
            let md = report(&plan, false, body.get("fileName"));
            let mut m = Map::new();
            m.insert("summary".into(), plan.summary.value());
            m.insert("report".into(), Value::String(md));
            json_response(StatusCode::OK, &Value::Object(m))
        }
    }
}

/// `req.body?.includeLists !== false` — everything but literal `false`
/// (and absent) keeps lists on.
fn js_truthy_strict_ne_false(v: &Value) -> bool {
    v == &Value::Bool(false)
}

pub async fn handle_apply(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::POST || !owns_manabox(&st) {
        return crate::proxy(State(st), req).await;
    }
    let (parts, bytes, oversize) = read_body(req, crate::collection_writes::BODY_MANABOX).await;
    let headers = parts.headers.clone();
    let user = match authed(&st, &headers, false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    // bigJson (40mb, server.js:464) mounts AFTER the auth gate: oversize
    // JSON answers the parser's 413 only once authenticateToken passed.
    // Non-JSON content-types skip the parser entirely — empty body is the
    // equivalent (content-length was cleared with the abandoned stream).
    if oversize && is_json_ct(&headers) {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Upload too large. Try exporting/importing in smaller batches.");
    }
    let body = match parse_body_40mb(&headers, &bytes) {
        BigOutcome::TooLarge => {
            return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Upload too large. Try exporting/importing in smaller batches.")
        }
        BigOutcome::Invalid => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
        BigOutcome::Parsed(v) => v,
    };
    let include_lists = match body.get("includeLists") {
        Some(v) => !js_truthy_strict_ne_false(v),
        None => true,
    };
    let plan = match build_plan(&st, user.id, body.get("csv").cloned().unwrap_or(Value::Null), include_lists).await {
        BuildOutcome::BadRequest(e) => return json_response(StatusCode::BAD_REQUEST, &err_body(&e)),
        BuildOutcome::ServerError(e) => {
            tracing::error!("[manabox-sync] apply {e}");
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, &err_body("Sync failed; nothing was changed."));
        }
        BuildOutcome::Plan(p) => p,
    };
    if let Some(eb) = body.get("expectBefore").filter(|v| !v.is_null()) {
        let n = js_number(eb);
        if !(n == plan.summary.before as f64) {
            return json_response(
                StatusCode::CONFLICT,
                &err_body("The collection changed since the preview. Preview again."),
            );
        }
    }
    let md = report(&plan, true, body.get("fileName"));
    let summary_json = serde_json::to_string(&plan.summary.value()).unwrap_or_else(|_| "{}".into());
    let pool = st.db.as_ref().unwrap().clone();
    let uid = user.id;
    // Review exec-sync M2: thousands of synchronous writes run off the runtime.
    let (applied, plan, md) = match tokio::task::spawn_blocking(move || {
        let r = match pool.get() {
            Ok(conn) => apply_plan(&conn, uid, &plan, &summary_json, &md),
            Err(e) => Err(e.to_string()),
        };
        (r, plan, md)
    })
    .await
    {
        Ok(t) => t,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    };
    if let Err(e) = applied {
        tracing::error!("[manabox-sync] apply {e}");
        return json_response(StatusCode::INTERNAL_SERVER_ERROR, &err_body("Sync failed; nothing was changed."));
    }
    let mut m = Map::new();
    m.insert("summary".into(), plan.summary.value());
    m.insert("report".into(), Value::String(md));
    let resp = json_response(StatusCode::OK, &Value::Object(m));
    stats::on_proxied_write(&st, &Method::POST, "/api/manabox-sync/apply", &headers);
    resp
}

fn apply_plan(conn: &Connection, uid: i64, plan: &Plan, summary_json: &str, md: &str) -> Result<(), String> {
    let mut tx = Tx::begin(conn).map_err(|e| format!("{e:?}"))?;
    let r = (|| -> Result<(), String> {
        for r in &plan.coll.remove {
            conn.execute(
                "DELETE FROM collection WHERE id = ? AND user_id = ?",
                q(&[SqlValue::Integer(r.id), SqlValue::Integer(uid)]),
            )
            .map_err(|e| e.to_string())?;
        }
        for (id, d) in &plan.coll.redate {
            conn.execute(
                "UPDATE collection SET added_at = ? WHERE id = ? AND user_id = ?",
                q(&[SqlValue::Text(d.clone()), SqlValue::Integer(*id), SqlValue::Integer(uid)]),
            )
            .map_err(|e| e.to_string())?;
        }
        for a in &plan.coll.add {
            conn.execute(
                "INSERT INTO collection (user_id, card_id, quantity, condition, printing, language, purchase_price, added_at, source)
                 VALUES (?, ?, 1, ?, ?, ?, ?, COALESCE(?, CURRENT_TIMESTAMP), 'manabox')",
                q(&[
                    SqlValue::Integer(uid),
                    SqlValue::Text(a.card_id.clone()),
                    SqlValue::Text(a.condition.clone()),
                    SqlValue::Text(a.printing.clone()),
                    SqlValue::Text(a.language.clone()),
                    a.purchase_price.map(SqlValue::Real).unwrap_or(SqlValue::Null),
                    a.added_at.as_ref().map(|s| SqlValue::Text(s.clone())).unwrap_or(SqlValue::Null),
                ]),
            )
            .map_err(|e| e.to_string())?;
        }
        for (name, cards) in &plan.lists.create {
            conn.execute(
                "INSERT INTO card_lists (user_id, name, description, source) VALUES (?, ?, 'Synced from ManaBox', 'manabox')",
                q(&[SqlValue::Integer(uid), SqlValue::Text(name.clone())]),
            )
            .map_err(|e| e.to_string())?;
            let list_id = conn.last_insert_rowid();
            for c in cards {
                conn.execute(
                    "INSERT INTO list_cards (list_id, card_id, quantity) VALUES (?, ?, ?)",
                    q(&[SqlValue::Integer(list_id), SqlValue::Text(c.card_id.clone()), SqlValue::Integer(c.quantity)]),
                )
                .map_err(|e| e.to_string())?;
            }
        }
        for (id, _, cards) in &plan.lists.update {
            conn.execute("DELETE FROM list_cards WHERE list_id = ?", q(&[SqlValue::Integer(*id)]))
                .map_err(|e| e.to_string())?;
            for c in cards {
                conn.execute(
                    "INSERT INTO list_cards (list_id, card_id, quantity) VALUES (?, ?, ?)",
                    q(&[SqlValue::Integer(*id), SqlValue::Text(c.card_id.clone()), SqlValue::Integer(c.quantity)]),
                )
                .map_err(|e| e.to_string())?;
            }
        }
        conn.execute(
            "INSERT INTO manabox_sync_runs (user_id, summary, report) VALUES (?, ?, ?)",
            q(&[SqlValue::Integer(uid), SqlValue::Text(summary_json.to_string()), SqlValue::Text(md.to_string())]),
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })();
    match r {
        Ok(()) => {
            tx.commit().map_err(|e| format!("{e:?}"))?;
            Ok(())
        }
        Err(e) => {
            tx.rollback();
            Err(e)
        }
    }
}

pub async fn handle_runs(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::GET || !owns_manabox(&st) {
        return crate::proxy(State(st), req).await;
    }
    let (parts, _body) = req.into_parts();
    let headers = parts.headers;
    let user = match authed(&st, &headers, true) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    let conn = match st.db.as_ref().unwrap().get() {
        Ok(c) => c,
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    };
    let mut stmt = match conn.prepare(
        "SELECT id, created_at, summary FROM manabox_sync_runs WHERE user_id = ? ORDER BY id DESC LIMIT 20",
    ) {
        Ok(s) => s,
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    };
    let params = vec![SqlValue::Integer(user.id)];
    let rows = stmt.query_map(q(&params), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    });
    let out: Vec<Value> = match rows {
        Ok(rows) => {
            let mut v = Vec::new();
            for row in rows {
                let Ok((id, created_at, summary)) = row else {
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
                };
                // JSON.parse(r.summary) — Node throws on bad JSON (unhandled
                // rejection); the port answers the global handler's 500.
                // A NULL column is JSON.parse(null) -> null in Node, so the
                // text defaults to "null", not "".
                let Ok(parsed) = serde_json::from_str::<Value>(&summary.unwrap_or_else(|| "null".into())) else {
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
                };
                let mut m = Map::new();
                m.insert("id".into(), Value::from(id));
                m.insert("created_at".into(), created_at.map(Value::String).unwrap_or(Value::Null));
                m.insert("summary".into(), parsed);
                v.push(Value::Object(m));
            }
            v
        }
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    };
    json_response(StatusCode::OK, &Value::Array(out))
}

pub async fn handle_report(State(st): State<AppState>, Path(id): Path<String>, req: Request) -> Response {
    if req.method() != Method::GET || !owns_manabox(&st) {
        return crate::proxy(State(st), req).await;
    }
    let (parts, _body) = req.into_parts();
    let headers = parts.headers;
    let user = match authed(&st, &headers, true) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    let conn = match st.db.as_ref().unwrap().get() {
        Ok(c) => c,
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    };
    // Node binds req.params.id as TEXT; SQLite applies INTEGER affinity to
    // the column comparison, matching this exactly.
    let got: Option<String> = conn
        .query_row(
            "SELECT report FROM manabox_sync_runs WHERE id = ? AND user_id = ?",
            q(&[SqlValue::Text(id.clone()), SqlValue::Integer(user.id)]),
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten();
    let Some(report) = got else {
        return json_response(StatusCode::NOT_FOUND, &err_body("Not found"));
    };
    // res.type('text/markdown').set(Content-Disposition).send(string):
    // 'text/markdown; charset=utf-8' + attachment filename over the raw text.
    let name = format!("manabox-sync-{id}.md");
    let cd = format!("attachment; filename=\"{name}\"");
    let mut resp = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/markdown; charset=utf-8"))],
        axum::body::Body::from(report),
    )
        .into_response();
    if let Ok(v) = HeaderValue::from_str(&cd) {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    resp
}

fn err_body(msg: &str) -> Value {
    let mut m = Map::new();
    m.insert("error".into(), Value::String(msg.to_string()));
    Value::Object(m)
}

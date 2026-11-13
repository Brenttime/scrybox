//! Rust port of the Moxfield sync (Lane B unit 3a): `backend/src/moxfieldSync.js`
//! (engine), `backend/src/moxfieldScheduler.js` (20-second tick + boot
//! catch-up) and `backend/src/routes/moxfield.js` (the seven /api/moxfield
//! endpoints). Gated by `RUST_ROUTES=moxfield`.
//!
//! One row per tracked Moxfield author, one row per mirrored deck
//! (`moxfield_decks`), and a `decks` row carrying `moxfield_public_id` once a
//! deck is imported. The two cadences ride `app_settings`. Allocation-guarded
//! sections share the process-wide allocation mutex + a BEGIN IMMEDIATE
//! transaction, exactly like the collection writes — a mirror delete or
//! re-link that races a deck check-out must surface ALLOCATION_CONFLICT
//! instead of corrupting inventory.
//!
//! Every upstream call goes through the exec-curl client in moxfield_api.rs
//! and is GET-only; the Scryfall backfill leg rides the shared paced queue.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use axum::{
    extract::{Path, Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
};
use rusqlite::types::{ToSqlOutput, Value as SqlValue};
use rusqlite::{Connection, ToSql};
use serde_json::{Map, Value};

use crate::auth::{authenticate, bearer_token};
use crate::card_cache::{cache_normalized_cards, card_key_sql};
use crate::card_norm::{normalize_card, scryfall_uuid};
use crate::collection_writes::{alloc_lock, node_exact, parse_body, BodyOutcome, Tx};
use crate::mfx_payload::{
    board_counts, extract_deck_cards, extract_maybe_cards, js_truthy, js_to_string, mfx_format_label,
    scrybox_card_id, synthesize_moxfield_card, target_size_for_format, BoardEntry,
};
use crate::moxfield_api::MoxfieldError;
use crate::scryfall::ScryOpts;
use crate::AppState;

use super::cards_tags::{decode_uri_component, json_error, json_response, read_body};

/// Route-facing error classification, mirroring what routes/moxfield.js
/// probes on the Node error object: `error.code === 'ALLOCATION_CONFLICT'`,
/// `error instanceof MoxfieldError` (+ its `.status`), or a plain message.
#[derive(Clone, Debug)]
pub struct SyncError {
    pub message: String,
    pub kind: ErrKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ErrKind {
    Plain,
    /// MoxfieldError with its HTTP status (0 = no status).
    Mox(u16),
    Conflict,
}

impl SyncError {
    fn plain(message: impl Into<String>) -> Self {
        Self { message: message.into(), kind: ErrKind::Plain }
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self { message: message.into(), kind: ErrKind::Conflict }
    }
    fn from_mox(e: MoxfieldError) -> Self {
        Self { message: e.message, kind: ErrKind::Mox(e.status) }
    }
}

impl From<String> for SyncError {
    fn from(s: String) -> Self {
        Self::plain(s)
    }
}

// ---------------------------------------------------------------------------
// Parameter binding — node-sqlite3 binds JS values; we bind serde Values
// with the same coercion (undefined/Null → NULL, objects → NULL, bools → 0/1).
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum P {
    Val(Value),
    Int(i64),
    OptInt(Option<i64>),
}

impl From<i64> for P {
    fn from(v: i64) -> Self {
        P::Int(v)
    }
}
impl From<Option<i64>> for P {
    fn from(v: Option<i64>) -> Self {
        P::OptInt(v)
    }
}
impl From<&str> for P {
    fn from(v: &str) -> Self {
        P::Val(Value::String(v.to_string()))
    }
}
impl From<String> for P {
    fn from(v: String) -> Self {
        P::Val(Value::String(v))
    }
}
impl From<Value> for P {
    fn from(v: Value) -> Self {
        P::Val(v)
    }
}
impl From<&Value> for P {
    fn from(v: &Value) -> Self {
        P::Val(v.clone())
    }
}
impl From<Option<&Value>> for P {
    fn from(v: Option<&Value>) -> Self {
        P::Val(v.cloned().unwrap_or(Value::Null))
    }
}

impl ToSql for P {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        let owned = match self {
            P::Int(i) => SqlValue::Integer(*i),
            P::OptInt(None) => SqlValue::Null,
            P::OptInt(Some(i)) => SqlValue::Integer(*i),
            P::Val(Value::Null) => SqlValue::Null,
            P::Val(Value::Bool(b)) => SqlValue::Integer(*b as i64),
            P::Val(Value::Number(n)) => match n.as_i64() {
                Some(i) => SqlValue::Integer(i),
                None => SqlValue::Real(n.as_f64().unwrap_or(0.0)),
            },
            P::Val(Value::String(s)) => SqlValue::Text(s.clone()),
            // node-sqlite3 binds unsupported shapes as NULL.
            _ => SqlValue::Null,
        };
        Ok(ToSqlOutput::Owned(owned))
    }
}

fn params_of(list: Vec<P>) -> Vec<P> {
    list
}

fn q_params(list: &[P]) -> Vec<&dyn ToSql> {
    list.iter().map(|p| p as &dyn ToSql).collect()
}

// ---------------------------------------------------------------------------
// Small row helpers
// ---------------------------------------------------------------------------

fn row_val<'a>(row: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    row.get(key).filter(|v| !v.is_null())
}

fn row_str(row: &Map<String, Value>, key: &str) -> Option<String> {
    match row.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(js_to_string(other)),
    }
}

fn row_i64(row: &Map<String, Value>, key: &str) -> Option<i64> {
    match row.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.parse().ok(),
        Some(Value::Bool(b)) => Some(*b as i64),
        _ => None,
    }
}

/// `SELECT ... FROM moxfield_authors` as ordered maps (column order follows
/// the SELECT list, matching what res.json serialises).
fn query_rows(conn: &Connection, sql: &str, args: &[P]) -> Result<Vec<Map<String, Value>>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(q_params(args)), |r| {
            let mut m = Map::new();
            for (i, name) in cols.iter().enumerate() {
                let v: rusqlite::types::Value = r.get(i)?;
                m.insert(
                    name.clone(),
                    match v {
                        rusqlite::types::Value::Null => Value::Null,
                        rusqlite::types::Value::Integer(i) => Value::from(i),
                        rusqlite::types::Value::Real(f) => serde_json::Number::from_f64(f)
                            .map(Value::Number)
                            .unwrap_or(Value::Null),
                        rusqlite::types::Value::Text(t) => Value::String(t),
                        rusqlite::types::Value::Blob(_) => Value::Null,
                    },
                );
            }
            Ok(m)
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

fn query_one(conn: &Connection, sql: &str, args: &[P]) -> Result<Option<Map<String, Value>>, String> {
    Ok(query_rows(conn, sql, args)?.into_iter().next())
}

fn exec(conn: &Connection, sql: &str, args: &[P]) -> Result<usize, String> {
    conn.execute(sql, rusqlite::params_from_iter(q_params(args)))
        .map_err(|e| e.to_string())
}

/// JS `a || b` over raw Values: returns `a` when truthy, else `b`.
fn js_or<'a>(a: Option<&'a Value>, b: &'a Value) -> &'a Value {
    match a {
        Some(v) if js_truthy(Some(v)) => v,
        _ => b,
    }
}

// ---------------------------------------------------------------------------
// Scryfall backfill for cards not yet in card_cache (backfillMissingCards)
// ---------------------------------------------------------------------------

async fn backfill_missing_cards(st: &AppState, entries: &[BoardEntry]) -> Result<(), SyncError> {
    let mut missing: Vec<(String, Value)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        for e in entries {
            let id = scrybox_card_id(&e.card);
            if seen.insert(id.clone()) {
                // Existence check — Node's `db.get('SELECT id …')` row
                // truthiness. Read the TEXT id as text: casting to i64 would
                // always fail (Err → 0) and re-fetch every cached card.
                let found: Option<String> = conn
                    .query_row("SELECT id FROM card_cache WHERE id = ?", rusqlite::params_from_iter(q_params(&params_of(vec![id.clone().into()]))), |r| r.get(0))
                    .ok()
                    .flatten();
                if found.is_none() {
                    missing.push((id, e.card.clone()));
                }
            }
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    // bulkFetchByIdentifier in 75-identifier batches. Rows carry only {id},
    // so a non-uuid id falls to {name:''} exactly like Node.
    let mut fresh: Vec<Map<String, Value>> = Vec::new();
    for chunk in missing.chunks(75) {
        let identifiers: Vec<Value> = chunk
            .iter()
            .map(|(id, _)| match scryfall_uuid(Some(id)) {
                Some(uuid) => serde_json::json!({ "id": uuid }),
                None => serde_json::json!({ "name": "" }),
            })
            .collect();
        let resp = st
            .scry
            .post_retried(
                "/cards/collection",
                Value::Object({
                    let mut m = Map::new();
                    m.insert("identifiers".into(), Value::Array(identifiers));
                    m
                }),
                &ScryOpts::default(),
                3,
            )
            .await
            .map_err(|e| SyncError::plain(e.to_string()))?;
        if let Some(data) = resp.as_ref().and_then(|v| v.get("data")).and_then(|v| v.as_array()) {
            for raw in data {
                if let Some(norm) = normalize_card(raw, None).as_object().cloned() {
                    fresh.push(norm);
                }
            }
        }
    }
    if !fresh.is_empty() {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        cache_normalized_cards(&conn, &fresh).map_err(|e| SyncError::plain(e.to_string()))?;
    }
    // Still missing means Scryfall itself has no record — synthesize from
    // the Moxfield card block (first entry per id, entry order).
    let mut synthesized: Vec<Map<String, Value>> = Vec::new();
    {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        for (id, card) in &missing {
            // Same TEXT-id caveat as the first existence check above:
            // get::<i64> on the TEXT id column always errors, which would
            // mark EVERY card still-missing and clobber the Scryfall row
            // cache_normalized_cards just wrote with the synthesized
            // minimal one. Node checks row truthiness — mirror that.
            let exists = conn
                .query_row("SELECT id FROM card_cache WHERE id = ?", rusqlite::params_from_iter(q_params(&params_of(vec![id.clone().into()]))), |r| {
                    r.get::<_, Option<String>>(0)
                })
                .ok()
                .flatten()
                .is_some();
            if !exists {
                synthesized.push(synthesize_moxfield_card(id, card));
            }
        }
    }
    if !synthesized.is_empty() {
        let names: Vec<String> = synthesized.iter().filter_map(|c| c.get("name").and_then(|v| v.as_str()).map(|s| s.to_string())).collect();
        tracing::warn!(
            "Moxfield sync: {} card(s) unknown to Scryfall — synthesized from Moxfield data: {}",
            synthesized.len(),
            names.join(", ")
        );
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        cache_normalized_cards(&conn, &synthesized).map_err(|e| SyncError::plain(e.to_string()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Local deck mirroring
// ---------------------------------------------------------------------------

/// Flag deck rows whose logical card is in `commander_keys`; clear the rest.
/// Always runs over EVERY deck_cards row — the `IS NOT ?` guard keeps the
/// write count honest while still normalising NULL rows to 0.
fn mark_deck_commanders(
    conn: &Connection,
    deck_id: i64,
    commander_keys: &HashSet<String>,
) -> Result<(), SyncError> {
    let rows = query_rows(
        conn,
        &format!(
            "SELECT dc.card_id, {} AS card_key
    FROM deck_cards dc LEFT JOIN card_cache cc ON cc.id = dc.card_id
    WHERE dc.deck_id = ?",
            card_key_sql("cc")
        ),
        &params_of(vec![deck_id.into()]),
    )
    .map_err(SyncError::plain)?;
    for row in rows {
        let key = row_str(&row, "card_key").filter(|k| !k.is_empty());
        let flag: i64 = match &key {
            Some(k) if commander_keys.contains(k) => 1,
            _ => 0,
        };
        let card_id = row_val(&row, "card_id").cloned().unwrap_or(Value::Null);
        exec(
            conn,
            "UPDATE deck_cards SET is_commander = ? WHERE deck_id = ? AND card_id = ? AND is_commander IS NOT ?",
            &params_of(vec![flag.into(), deck_id.into(), card_id.into(), flag.into()]),
        )
        .map_err(SyncError::plain)?;
    }
    Ok(())
}

/// Replace a deck's maybeboard rows. deck_maybe_cards is display-only: it
/// never feeds totals, stats, value or checkout allocation, so it is
/// refreshed even for checked-out decks.
fn replace_maybeboard(
    conn: &Connection,
    deck_id: i64,
    maybe_entries: &[BoardEntry],
) -> Result<(), SyncError> {
    // Sum duplicate ids, first-appearance order (JS Map semantics).
    let mut order: Vec<String> = Vec::new();
    let mut qty: HashMap<String, i64> = HashMap::new();
    for e in maybe_entries {
        let id = scrybox_card_id(&e.card);
        qty.entry(id.clone()).and_modify(|q| *q += e.quantity).or_insert_with(|| {
            order.push(id.clone());
            e.quantity
        });
    }
    exec(conn, "DELETE FROM deck_maybe_cards WHERE deck_id = ?", &params_of(vec![deck_id.into()]))
        .map_err(SyncError::plain)?;
    for card_id in order {
        let quantity = qty[&card_id];
        exec(
            conn,
            "INSERT INTO deck_maybe_cards (deck_id, card_id, quantity)
       SELECT ?, ?, ? WHERE EXISTS (SELECT 1 FROM card_cache WHERE id = ?)",
            &params_of(vec![deck_id.into(), card_id.clone().into(), quantity.into(), card_id.into()]),
        )
        .map_err(SyncError::plain)?;
    }
    Ok(())
}

/// Create the local mirror deck for a summary (or details-derived summary) if
/// one does not already exist. Returns the local deck id. An existing row is
/// returned UNTOUCHED — renames flow through upsertTrackingRow's UPDATE.
fn ensure_local_deck(
    conn: &Connection,
    author: &Map<String, Value>,
    summary: &Map<String, Value>,
) -> Result<i64, SyncError> {
    let user_id = row_i64(author, "user_id");
    let existing = query_one(
        conn,
        "SELECT id FROM decks WHERE user_id = ? AND moxfield_public_id = ?",
        &params_of(vec![user_id.into(), js_or(summary.get("publicId"), &Value::Null).clone().into()]),
    )
    .map_err(SyncError::plain)?;
    if let Some(existing) = existing {
        return Ok(row_i64(&existing, "id").unwrap_or(0));
    }
    let public_id = js_or(summary.get("publicId"), &Value::Null).clone();
    let name = js_or(summary.get("name"), &public_id).clone();
    let description = js_or(summary.get("description"), &Value::String(String::new())).clone();
    let format = summary.get("format");
    exec(
        conn,
        "INSERT INTO decks (name, description, format, category, accent_color, target_size, user_id, source, moxfield_public_id)
     VALUES (?, ?, ?, 'Moxfield Sync', '#22d3ee', ?, ?, 'moxfield', ?)",
        &params_of(vec![
            name.clone().into(),
            description.into(),
            Value::String(mfx_format_label(format)).into(),
            target_size_for_format(format).into(),
            user_id.into(),
            public_id.clone().into(),
        ]),
    )
    .map_err(SyncError::plain)?;
    let last_id = conn.last_insert_rowid();
    tracing::info!(
        "Moxfield sync: created local deck #{} \"{}\" for {}",
        last_id,
        js_to_string(&name),
        row_str(author, "moxfield_user").unwrap_or_default()
    );
    Ok(last_id)
}

/// Keep the tracking row for a deck in step with the remote summary, and link
/// the local deck to it. Only creates the local mirror when the deck is
/// enabled — an unchecked deck is tracked (so re-enabling is instant) but
/// never imported. Returns None when the author vanished mid-sync.
async fn upsert_tracking_row(
    st: &AppState,
    author: &Map<String, Value>,
    summary: &Value,
) -> Result<Option<Map<String, Value>>, SyncError> {
    let author_id = row_i64(author, "id");
    let author_user_id = row_i64(author, "user_id");
    let _guard = alloc_lock().lock().await;
    let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
    let mut tx = Tx::begin(&conn).map_err(|e| SyncError::plain(format!("{e:?}")))?;

    // Discovery is remote work and may finish after the author was removed.
    // Revalidate lifecycle ownership inside the same lock/transaction used by
    // teardown before creating or linking a local mirror.
    let live_author = query_one(
        &conn,
        "SELECT * FROM moxfield_authors WHERE id = ? AND user_id = ?",
        &params_of(vec![author_id.into(), author_user_id.into()]),
    )
    .map_err(SyncError::plain)?;
    let live_author = match live_author {
        Some(a) => a,
        None => {
            tx.rollback();
            return Ok(None);
        }
    };

    let public_id = summary.get("publicId").cloned().unwrap_or(Value::Null);
    let null = Value::Null;
    let empty = Value::String(String::new());
    // 10 placeholders, 9 Node params: the 10th (bindarr_deck_id) gets
    // undefined → NULL on insert, and COALESCE keeps the live value on update.
    // commander_count is bound NULL and NEVER updated here.
    exec(
        &conn,
        "INSERT INTO moxfield_decks
         (author_id, public_id, name, format, mainboard_count, sideboard_count, maybeboard_count, commander_count, last_updated_at, bindarr_deck_id)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(author_id, public_id) DO UPDATE SET
         name = excluded.name,
         format = excluded.format,
         mainboard_count = excluded.mainboard_count,
         sideboard_count = excluded.sideboard_count,
         maybeboard_count = excluded.maybeboard_count,
         last_updated_at = excluded.last_updated_at,
         bindarr_deck_id = COALESCE(excluded.bindarr_deck_id, moxfield_decks.bindarr_deck_id)",
        &params_of(vec![
            row_i64(&live_author, "id").into(),
            public_id.clone().into(),
            js_or(summary.get("name"), &public_id).clone().into(),
            js_or(summary.get("format"), &null).clone().into(),
            count_param(summary.get("mainboardCount")),
            count_param(summary.get("sideboardCount")),
            count_param(summary.get("maybeboardCount")),
            null.clone().into(),
            js_or(summary.get("lastUpdatedAtUtc"), &null).clone().into(),
            null.clone().into(),
        ]),
    )
    .map_err(SyncError::plain)?;
    let mut row = query_one(
        &conn,
        "SELECT * FROM moxfield_decks WHERE author_id = ? AND public_id = ?",
        &params_of(vec![row_i64(&live_author, "id").into(), public_id.clone().into()]),
    )
    .map_err(SyncError::plain)?
    .ok_or_else(|| SyncError::plain("Moxfield tracking row disappeared"))?;

    // The caller's enabled snapshot may be stale after remote discovery. The
    // tracking row inside this transaction is authoritative: a concurrent
    // disable must never be undone by late deck-list work.
    // JS: !row.bindarr_deck_id && row.enabled !== 0 — NULL enabled !== 0.
    if !js_truthy(row.get("bindarr_deck_id")) && row.get("enabled") != Some(&Value::from(0)) {
        // ensureLocalDeck reads publicId/name/description/format off the
        // summary object.
        let mut summary_like = Map::new();
        for k in ["publicId", "name", "description", "format"] {
            if let Some(v) = summary.get(k) {
                summary_like.insert(k.to_string(), v.clone());
            }
        }
        let deck_id = ensure_local_deck(&conn, &live_author, &summary_like)?;
        let changes = exec(
            &conn,
            "UPDATE moxfield_decks SET bindarr_deck_id = ? WHERE id = ?",
            &params_of(vec![deck_id.into(), row_i64(&row, "id").into()]),
        )
        .map_err(SyncError::plain)?;
        if changes != 1 {
            tx.rollback();
            return Err(SyncError::plain("Moxfield tracking row disappeared while linking its mirror"));
        }
        row.insert("bindarr_deck_id".into(), Value::from(deck_id));
    }
    // Renames on Moxfield propagate to the mirror.
    exec(
        &conn,
        "UPDATE decks SET name = ?, description = ? WHERE id = ? AND moxfield_public_id = ? AND source = 'moxfield'",
        &params_of(vec![
            js_or(summary.get("name"), &public_id).clone().into(),
            js_or(summary.get("description"), &empty).clone().into(),
            row_val(&row, "bindarr_deck_id").cloned().unwrap_or(Value::Null).into(),
            public_id.into(),
        ]),
    )
    .map_err(SyncError::plain)?;
    tx.commit().map_err(|e| SyncError::plain(format!("{e:?}")))?;
    Ok(Some(row))
}

/// `summary.X != null ? parseInt(summary.X, 10) : null` — NaN binds NULL,
/// exactly like node-sqlite3 binding a NaN.
fn count_param(v: Option<&Value>) -> P {
    match v {
        None | Some(Value::Null) => P::OptInt(None),
        Some(v) => P::OptInt(crate::mfx_payload::js_parse_int(Some(v))),
    }
}

/// Retire a tracked deck: drop the local mirror and its tracking row. Cards
/// stay in card_cache (they may be referenced by other decks and by price
/// history).
async fn retire_deck(st: &AppState, author_id: Option<i64>, public_id: &str) -> Result<bool, SyncError> {
    let _guard = alloc_lock().lock().await;
    let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
    let mut tx = Tx::begin(&conn).map_err(|e| SyncError::plain(format!("{e:?}")))?;
    let row = query_one(
        &conn,
        "SELECT md.bindarr_deck_id, d.checked_out
      FROM moxfield_decks md
      LEFT JOIN decks d ON d.id = md.bindarr_deck_id
      WHERE md.author_id = ? AND md.public_id = ?",
        &params_of(vec![author_id.into(), public_id.into()]),
    )
    .map_err(SyncError::plain)?;
    let row = match row {
        Some(r) => r,
        None => {
            tx.commit().map_err(|e| SyncError::plain(format!("{e:?}")))?;
            return Ok(false);
        }
    };
    if js_truthy(row.get("checked_out")) {
        tx.rollback();
        return Err(SyncError::conflict("Check this deck in before removing its Moxfield mirror"));
    }
    if let Some(bind) = row_val(&row, "bindarr_deck_id") {
        let bind = bind.clone();
        exec(&conn, "DELETE FROM deck_cards WHERE deck_id = ?", &params_of(vec![bind.clone().into()])).map_err(SyncError::plain)?;
        exec(&conn, "DELETE FROM deck_maybe_cards WHERE deck_id = ?", &params_of(vec![bind.clone().into()])).map_err(SyncError::plain)?;
        exec(&conn, "DELETE FROM decks WHERE id = ? AND source = 'moxfield'", &params_of(vec![bind.into()])).map_err(SyncError::plain)?;
    }
    exec(
        &conn,
        "DELETE FROM moxfield_decks WHERE author_id = ? AND public_id = ?",
        &params_of(vec![author_id.into(), public_id.into()]),
    )
    .map_err(SyncError::plain)?;
    tx.commit().map_err(|e| SyncError::plain(format!("{e:?}")))?;
    tracing::info!("Moxfield sync: retired deck \"{public_id}\" (removed on Moxfield)");
    Ok(true)
}

// ---------------------------------------------------------------------------
// Decklist sync (the slow job)
// ---------------------------------------------------------------------------

/// Re-validate the author on Moxfield and reconcile the list of tracked
/// decks: create mirrors for new decks (and pull their contents right away),
/// retire deleted ones, refresh the stamp on every tracked row.
pub async fn sync_decklist(st: &AppState, author_id: Option<i64>, user: Option<i64>) -> Result<Map<String, Value>, SyncError> {
    let author = {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        query_one(&conn, "SELECT * FROM moxfield_authors WHERE id = ?", &params_of(vec![author_id.into()]))
            .map_err(SyncError::plain)?
    };
    let author = match author {
        Some(a) => a,
        None => return Err(SyncError::plain("Moxfield author not found")),
    };
    // Number(author.user_id) !== Number(user.id) — numeric compare; a
    // non-numeric author.user_id is NaN and never matches.
    if let Some(uid) = user {
        let same = crate::mfx_payload::js_number(author.get("user_id").unwrap_or(&Value::Null))
            .map(|n| n == uid as f64)
            .unwrap_or(false);
        if !same {
            return Err(SyncError::plain("Moxfield author not found"));
        }
    }

    // Resolve the canonical username (and pick up profile name changes).
    let api = st.mfx.as_ref();
    let user_name_for_lookup = row_str(&author, "moxfield_user").unwrap_or_default();
    let user_info = api.get_user(&user_name_for_lookup).await.map_err(SyncError::from_mox)?;
    {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        exec(
            &conn,
            "UPDATE moxfield_authors SET display_name = ?, profile_image_url = ?, last_error = NULL WHERE id = ?",
            &params_of(vec![
                Value::String(user_info.display_name.clone()).into(),
                user_info
                    .profile_image_url
                    .as_ref()
                    .map(|s| Value::String(s.clone()))
                    .unwrap_or(Value::Null)
                    .into(),
                row_i64(&author, "id").into(),
            ]),
        )
        .map_err(SyncError::plain)?;
    }

    let summaries = api.get_author_deck_summaries(&user_info.user_name).await.map_err(SyncError::from_mox)?;
    let mut created = 0i64;
    let mut kept = 0i64;

    for summary in &summaries {
        if !js_truthy(summary.get("publicId")) {
            continue;
        }
        let public_id = row_str_of(summary.get("publicId"));
        let was_tracked = {
            let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
            query_one(
                &conn,
                "SELECT id FROM moxfield_decks WHERE author_id = ? AND public_id = ?",
                &params_of(vec![row_i64(&author, "id").into(), public_id.clone().into()]),
            )
            .map_err(SyncError::plain)?
            .is_some()
        };
        let live_tracking = upsert_tracking_row(st, &author, summary).await?;
        let live_tracking = match live_tracking {
            Some(t) => t,
            None => {
                let mut m = Map::new();
                m.insert("ok".into(), Value::from(true));
                m.insert("author".into(), Value::String(user_info.user_name.clone()));
                m.insert("stale".into(), Value::from(true));
                m.insert("reason".into(), Value::String("author-removed-during-sync".into()));
                m.insert("decks_on_moxfield".into(), Value::from(summaries.len() as i64));
                m.insert("decks_created".into(), Value::from(created));
                m.insert("decks_kept".into(), Value::from(kept));
                m.insert("decks_removed".into(), Value::from(0));
                return Ok(m);
            }
        };
        if was_tracked {
            kept += 1;
        } else if live_tracking.get("enabled") != Some(&Value::from(0)) {
            created += 1;
            // Brand-new mirror: pull its contents right away rather than
            // waiting for the fast job to notice the unsynced stamp.
            if let Err(err) = pull_deck_content(st, &author, &public_id, None).await {
                let name_for_log = js_to_string(summary.get("name").unwrap_or(&Value::Null));
                tracing::warn!(
                    "Moxfield sync: initial content pull for \"{}\" failed: {}",
                    name_for_log,
                    err.message
                );
                let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
                exec(
                    &conn,
                    "UPDATE moxfield_decks SET last_error = ? WHERE author_id = ? AND public_id = ?",
                    &params_of(vec![
                        Value::String(err.message.clone()).into(),
                        row_i64(&author, "id").into(),
                        public_id.clone().into(),
                    ]),
                )
                .map_err(SyncError::plain)?;
            }
        }
    }

    // Decks that vanished on Moxfield.
    let mut removed = 0i64;
    let tracked = {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        query_rows(
            &conn,
            "SELECT public_id FROM moxfield_decks WHERE author_id = ?",
            &params_of(vec![row_i64(&author, "id").into()]),
        )
        .map_err(SyncError::plain)?
    };
    // JS Set(summaries.map(s => s.publicId)) compared with ===: only string
    // publicIds can ever equal a TEXT row value.
    let remote_ids: HashSet<String> = summaries
        .iter()
        .filter_map(|s| s.get("publicId").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();
    for row in &tracked {
        let pid = row_str(row, "public_id").unwrap_or_default();
        if !remote_ids.contains(&pid) {
            if retire_deck(st, row_i64(&author, "id"), &pid).await? {
                removed += 1;
            }
        }
    }

    {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        exec(
            &conn,
            "UPDATE moxfield_authors SET last_decklist_sync_at = CURRENT_TIMESTAMP WHERE id = ?",
            &params_of(vec![row_i64(&author, "id").into()]),
        )
        .map_err(SyncError::plain)?;
    }
    let mut m = Map::new();
    m.insert("ok".into(), Value::from(true));
    m.insert("author".into(), Value::String(user_info.user_name));
    m.insert("decks_on_moxfield".into(), Value::from(summaries.len() as i64));
    m.insert("decks_created".into(), Value::from(created));
    m.insert("decks_kept".into(), Value::from(kept));
    m.insert("decks_removed".into(), Value::from(removed));
    Ok(m)
}

fn row_str_of(v: Option<&Value>) -> String {
    match v {
        Some(v) if !v.is_null() => js_to_string(v),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Content sync (the fast job)
// ---------------------------------------------------------------------------

/// The per-minute check for one author. One summary call carries every
/// deck's lastUpdatedAtUtc; each tracked deck whose stamp moved (or has never
/// been mirrored) gets its full contents pulled and mirrored. Decks that
/// vanished are retired.
pub async fn run_content_sync(st: &AppState, author_id: Option<i64>, user: Option<i64>) -> Result<Map<String, Value>, SyncError> {
    let author = {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        query_one(&conn, "SELECT * FROM moxfield_authors WHERE id = ?", &params_of(vec![author_id.into()]))
            .map_err(SyncError::plain)?
    };
    let author = match author {
        Some(a) => a,
        None => return Err(SyncError::plain("Moxfield author not found")),
    };
    if let Some(uid) = user {
        let same = crate::mfx_payload::js_number(author.get("user_id").unwrap_or(&Value::Null))
            .map(|n| n == uid as f64)
            .unwrap_or(false);
        if !same {
            return Err(SyncError::plain("Moxfield author not found"));
        }
    }
    // No ENABLED tracked decks (author just added, decklist sync pending, or
    // every deck unchecked): nothing to check, and NO network call.
    {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        let have = query_one(
            &conn,
            "SELECT COUNT(*) AS c FROM moxfield_decks WHERE author_id = ? AND enabled = 1",
            &params_of(vec![row_i64(&author, "id").into()]),
        )
        .map_err(SyncError::plain)?;
        if !have.map(|h| row_i64(&h, "c").unwrap_or(0) != 0).unwrap_or(false) {
            let mut m = Map::new();
            m.insert("ok".into(), Value::from(true));
            m.insert("checked".into(), Value::from(0));
            m.insert("updated".into(), Value::from(0));
            m.insert("skipped".into(), Value::from(0));
            return Ok(m);
        }
    }

    let api = st.mfx.as_ref();
    let summaries = api
        .get_author_deck_summaries(&row_str(&author, "moxfield_user").unwrap_or_default())
        .await
        .map_err(SyncError::from_mox)?;
    let mut by_public_id: HashMap<String, &Value> = HashMap::new();
    for s in &summaries {
        // new Map(summaries.map(s => [s.publicId, s])) — last duplicate wins;
        // non-string keys can never match a TEXT row.public_id.
        if let Some(pid) = s.get("publicId").and_then(|v| v.as_str()) {
            by_public_id.insert(pid.to_string(), s);
        }
    }
    let tracked = {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        query_rows(
            &conn,
            "SELECT md.*, d.checked_out AS mirror_checked_out
    FROM moxfield_decks md
    LEFT JOIN decks d ON d.id = md.bindarr_deck_id
    WHERE md.author_id = ?",
            &params_of(vec![row_i64(&author, "id").into()]),
        )
        .map_err(SyncError::plain)?
    };

    let mut updated = 0i64;
    let mut removed = 0i64;
    let mut skipped = 0i64;
    for row in &tracked {
        let pid = row_str(row, "public_id").unwrap_or_default();
        let summary: Option<&Value> = by_public_id.get(&pid).copied();
        let summary = match summary {
            None => {
                if retire_deck(st, row_i64(&author, "id"), &pid).await? {
                    removed += 1;
                }
                continue;
            }
            Some(s) => s,
        };
        let stamp = js_or(summary.get("lastUpdatedAtUtc"), &Value::Null).clone();
        let refresh = |conn: &Connection, stamp: &Value| -> Result<(), SyncError> {
            exec(
                conn,
                "UPDATE moxfield_decks SET last_updated_at = ?, name = ?, format = ?,
                mainboard_count = ?, sideboard_count = ?, maybeboard_count = ?
         WHERE id = ?",
                &params_of(vec![
                    stamp.clone().into(),
                    js_or(summary.get("name"), &row_val(row, "name").cloned().unwrap_or(Value::Null)).clone().into(),
                    js_or(summary.get("format"), &row_val(row, "format").cloned().unwrap_or(Value::Null)).clone().into(),
                    count_param(summary.get("mainboardCount")),
                    count_param(summary.get("sideboardCount")),
                    count_param(summary.get("maybeboardCount")),
                    row_i64(row, "id").into(),
                ]),
            )
            .map_err(SyncError::plain)?;
            Ok(())
        };
        // Unchecked deck: keep its listing row fresh (name, counts, remote
        // stamp) but never pull contents or create a mirror. JS === 0 strict.
        if row.get("enabled") == Some(&Value::from(0)) {
            skipped += 1;
            let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
            refresh(&conn, &stamp)?;
            continue;
        }
        // Stamp moved since we last mirrored this deck (or never mirrored it).
        let last_synced = row.get("last_synced_updated_at");
        let needs_pull = !js_truthy(last_synced) || stamp != last_synced.cloned().unwrap_or(Value::Null);
        // A Built (checked-out) mirror refuses content changes, so its synced
        // stamp never advances and it used to be re-pulled on every 20s tick.
        // Pull it once per Moxfield change (that pull still mirrors the
        // maybeboard), then skip it silently while the stamp is unchanged.
        let built_and_seen = js_truthy(row.get("mirror_checked_out"))
            && stamp == row.get("last_updated_at").cloned().unwrap_or(Value::Null);
        if needs_pull && !built_and_seen {
            match pull_deck_content(st, &author, &pid, Some(row.clone())).await {
                Ok(_) => updated += 1,
                Err(err) => {
                    if err.kind != ErrKind::Conflict {
                        tracing::warn!(
                            "Moxfield sync: content pull failed for \"{}\": {}",
                            row_str(row, "name").unwrap_or_default(),
                            err.message
                        );
                    }
                    let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
                    exec(
                        &conn,
                        "UPDATE moxfield_decks SET last_error = ? WHERE id = ?",
                        &params_of(vec![Value::String(err.message.clone()).into(), row_i64(row, "id").into()]),
                    )
                    .map_err(SyncError::plain)?;
                }
            }
        }
        // Refresh the stored stamp (and the summary fields) even when no pull
        // happened, so the decklist job's view stays in step.
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        refresh(&conn, &stamp)?;
    }

    // Mark that we did a check just now, whether or not anything moved.
    {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        exec(
            &conn,
            "UPDATE moxfield_authors SET last_content_check_at = CURRENT_TIMESTAMP WHERE id = ?",
            &params_of(vec![row_i64(&author, "id").into()]),
        )
        .map_err(SyncError::plain)?;
    }
    let mut m = Map::new();
    m.insert("ok".into(), Value::from(true));
    m.insert("checked".into(), Value::from(tracked.len() as i64));
    m.insert("updated".into(), Value::from(updated));
    m.insert("removed".into(), Value::from(removed));
    m.insert("skipped".into(), Value::from(skipped));
    Ok(m)
}

// ---------------------------------------------------------------------------
// One deck's contents
// ---------------------------------------------------------------------------

/// Pull one deck's full contents from Moxfield and mirror onto the local deck.
pub async fn pull_deck_content(
    st: &AppState,
    author: &Map<String, Value>,
    public_id: &str,
    known_row: Option<Map<String, Value>>,
) -> Result<Map<String, Value>, SyncError> {
    let author_id = row_i64(author, "id");
    let mut row: Option<Map<String, Value>> = match known_row {
        Some(r) => Some(r),
        None => {
            let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
            query_one(
                &conn,
                "SELECT * FROM moxfield_decks WHERE author_id = ? AND public_id = ?",
                &params_of(vec![author_id.into(), public_id.into()]),
            )
            .map_err(SyncError::plain)?
        }
    };
    let row = match &mut row {
        Some(r) => r,
        None => {
            return Err(SyncError::plain(format!(
                "Deck {} is not tracked for {}",
                public_id,
                row_str(author, "moxfield_user").unwrap_or_default()
            )))
        }
    };

    let api = st.mfx.as_ref();
    let details = api.get_deck_details(public_id).await.map_err(SyncError::from_mox)?;
    let entries = extract_deck_cards(&details);
    let maybe_entries = extract_maybe_cards(&details);

    // Fetch/cache provider metadata BEFORE taking the allocation lock;
    // network I/O must never stall checkouts. From the first mirror-deck read
    // through the final reconciliation write, however, checkout and sync are
    // one ordered operation.
    backfill_missing_cards(st, &entries.iter().cloned().chain(maybe_entries.iter().cloned()).collect::<Vec<_>>()).await?;

    let _guard = alloc_lock().lock().await;
    let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
    let mut tx = Tx::begin(&conn).map_err(|e| SyncError::plain(format!("{e:?}")))?;

    // A content pull may spend seconds in remote/cache I/O. Re-read tracking
    // state only after acquiring the same lock used by disable/remove/retire
    // so stale work can never recreate a mirror that was paused or deleted
    // while the request was in flight.
    let live_row = query_one(
        &conn,
        "SELECT md.*
      FROM moxfield_decks md
      JOIN moxfield_authors ma ON ma.id = md.author_id
      WHERE md.author_id = ? AND md.public_id = ? AND ma.user_id = ?",
        &params_of(vec![author_id.into(), public_id.into(), row_i64(author, "user_id").into()]),
    )
    .map_err(SyncError::plain)?;
    let live_row = match live_row {
        Some(r) if js_truthy(r.get("enabled")) => r,
        _ => {
            tx.commit().map_err(|e| SyncError::plain(format!("{e:?}")))?;
            let mut m = Map::new();
            m.insert("ok".into(), Value::from(true));
            m.insert("skipped".into(), Value::from(true));
            m.insert("public_id".into(), Value::String(public_id.to_string()));
            m.insert("reason".into(), Value::String("tracking-disabled-or-removed".into()));
            return Ok(m);
        }
    };
    *row = live_row;

    // A stored bindarr_deck_id can dangle if the mirror deck was deleted
    // through the generic deck-delete (which doesn't clear the Moxfield
    // pointer). Validate the pointer; if it's gone, re-mint the mirror.
    let mut target_deck_id: Option<Value> = row_val(row, "bindarr_deck_id").cloned();
    if let Some(target) = target_deck_id.clone() {
        let alive = query_one(
            &conn,
            "SELECT id, checked_out FROM decks WHERE id = ? AND source = 'moxfield'",
            &params_of(vec![target.into()]),
        )
        .map_err(SyncError::plain)?;
        if let Some(alive) = &alive {
            if js_truthy(alive.get("checked_out")) {
                // Main/side boards stay frozen while built; the maybeboard
                // still syncs.
                let deck_id = row_i64(alive, "id").unwrap_or(0);
                replace_maybeboard(&conn, deck_id, &maybe_entries)?;
                tx.commit().map_err(|e| SyncError::plain(format!("{e:?}")))?;
                return Err(SyncError::conflict("Check this deck in before syncing Moxfield changes"));
            }
        } else {
            target_deck_id = None;
            exec(
                &conn,
                "UPDATE moxfield_decks SET bindarr_deck_id = NULL WHERE id = ?",
                &params_of(vec![row_i64(row, "id").into()]),
            )
            .map_err(SyncError::plain)?;
        }
    }
    let target_deck_id: i64 = match target_deck_id {
        Some(v) => js_to_string(&v).parse::<i64>().map_err(|_| SyncError::plain("bindarr_deck_id is not a deck id"))?,
        None => {
            let mut synth = Map::new();
            synth.insert("publicId".into(), Value::String(public_id.to_string()));
            synth.insert("name".into(), js_or(details.get("name"), &row_val(row, "name").cloned().unwrap_or(Value::Null)).clone());
            synth.insert(
                "description".into(),
                js_or(details.get("description"), &Value::String(String::new())).clone(),
            );
            synth.insert("format".into(), js_or(details.get("format"), &row_val(row, "format").cloned().unwrap_or(Value::Null)).clone());
            let id = ensure_local_deck(&conn, author, &synth)?;
            exec(
                &conn,
                "UPDATE moxfield_decks SET bindarr_deck_id = ? WHERE id = ?",
                &params_of(vec![id.into(), row_i64(row, "id").into()]),
            )
            .map_err(SyncError::plain)?;
            id
        }
    };

    // Reconcile quantities by logical card identity. Moxfield can name more
    // than one printing of the same game card; retain one representative id
    // and sum those quantities instead of creating parallel deck rows.
    let mut raw_order: Vec<String> = Vec::new();
    let mut raw_desired: HashMap<String, i64> = HashMap::new();
    for e in &entries {
        let id = scrybox_card_id(&e.card);
        raw_desired.entry(id.clone()).and_modify(|q| *q += e.quantity).or_insert_with(|| {
            raw_order.push(id.clone());
            e.quantity
        });
    }
    let mut keys_by_id: HashMap<String, Option<String>> = HashMap::new();
    if !raw_order.is_empty() {
        let placeholders = vec!["?"; raw_order.len()].join(",");
        let args: Vec<P> = raw_order.iter().map(|id| P::Val(Value::String(id.clone()))).collect();
        let cached = query_rows(
            &conn,
            &format!(
                "SELECT id, {} AS card_key
       FROM card_cache WHERE id IN ({})",
                card_key_sql("card_cache"),
                placeholders
            ),
            &args,
        )
        .map_err(SyncError::plain)?;
        for c in cached {
            keys_by_id.insert(row_str(&c, "id").unwrap_or_default(), row_str(&c, "card_key").filter(|k| !k.is_empty()));
        }
    }
    // logical card key -> representative id + summed quantity (insertion order)
    let mut desired: Vec<(String, String, i64)> = Vec::new();
    let mut desired_index: HashMap<String, usize> = HashMap::new();
    for id in &raw_order {
        let quantity = raw_desired[id];
        let key = keys_by_id.get(id).and_then(|k| k.clone());
        let key = match key {
            Some(k) => k,
            None => {
                tx.rollback();
                return Err(SyncError::plain(format!("Card cache metadata is incomplete for {id}")));
            }
        };
        match desired_index.get(&key) {
            Some(&i) => desired[i].2 += quantity,
            None => {
                desired_index.insert(key.clone(), desired.len());
                desired.push((key, id.clone(), quantity));
            }
        }
    }

    let current = query_rows(
        &conn,
        &format!(
            "SELECT dc.card_id, {} AS card_key
    FROM deck_cards dc
    LEFT JOIN card_cache current_cc ON current_cc.id = dc.card_id
    WHERE dc.deck_id = ?
    ORDER BY dc.card_id
  ",
            card_key_sql("current_cc")
        ),
        &params_of(vec![target_deck_id.into()]),
    )
    .map_err(SyncError::plain)?;
    let mut current_by_key: HashMap<String, Vec<String>> = HashMap::new();
    for c in current {
        let card_id = row_str(&c, "card_id").unwrap_or_default();
        let key = row_str(&c, "card_key").filter(|k| !k.is_empty());
        match &key {
            Some(k) if desired_index.contains_key(k) => {
                current_by_key.entry(k.clone()).or_default().push(card_id);
            }
            _ => {
                // Not desired (or its cache row lost its identity): remove,
                // guarded against a concurrent check-out.
                let changes = exec(
                    &conn,
                    "DELETE FROM deck_cards
        WHERE deck_id = ? AND card_id = ?
          AND EXISTS (SELECT 1 FROM decks WHERE id = ? AND checked_out = 0)
      ",
                    &params_of(vec![target_deck_id.into(), Value::String(card_id).into(), target_deck_id.into()]),
                )
                .map_err(SyncError::plain)?;
                if changes == 0 {
                    tx.rollback();
                    return Err(SyncError::plain("Deck was checked out while Moxfield changes were syncing"));
                }
            }
        }
    }

    let checkout_guard_failed: SyncError =
        SyncError::plain("Deck was checked out while Moxfield changes were syncing");
    for (key, wanted, quantity) in &desired {
        let existing_ids: Vec<String> = current_by_key.get(key).cloned().unwrap_or_default();
        if !existing_ids.is_empty() {
            let representative = if existing_ids.iter().any(|i| i == wanted) {
                wanted.clone()
            } else {
                existing_ids[0].clone()
            };
            let mut args: Vec<P> = vec![
                Value::String(representative).into(),
                (*quantity).into(),
                target_deck_id.into(),
            ];
            for id in &existing_ids {
                args.push(Value::String(id.clone()).into());
            }
            args.push(target_deck_id.into());
            let placeholders = vec!["?"; existing_ids.len()].join(",");
            let changes = exec(
                &conn,
                &format!(
                    "UPDATE deck_cards
        SET quantity = CASE WHEN card_id = ? THEN ? ELSE 0 END
        WHERE deck_id = ?
          AND card_id IN ({})
          AND EXISTS (SELECT 1 FROM decks WHERE id = ? AND checked_out = 0)
      ",
                    placeholders
                ),
                &args,
            )
            .map_err(SyncError::plain)?;
            if changes == 0 {
                tx.rollback();
                return Err(checkout_guard_failed);
            }
            let mut del_args: Vec<P> = vec![target_deck_id.into()];
            for id in &existing_ids {
                del_args.push(Value::String(id.clone()).into());
            }
            let placeholders = vec!["?"; existing_ids.len()].join(",");
            exec(
                &conn,
                &format!(
                    "DELETE FROM deck_cards
         WHERE deck_id = ? AND quantity <= 0
           AND card_id IN ({})",
                    placeholders
                ),
                &del_args,
            )
            .map_err(SyncError::plain)?;
        } else {
            let changes = exec(
                &conn,
                "INSERT INTO deck_cards (deck_id, card_id, quantity)
        SELECT ?, ?, ?
        WHERE EXISTS (SELECT 1 FROM decks WHERE id = ? AND checked_out = 0)
      ",
                &params_of(vec![
                    target_deck_id.into(),
                    Value::String(wanted.clone()).into(),
                    (*quantity).into(),
                    target_deck_id.into(),
                ]),
            )
            .map_err(SyncError::plain)?;
            if changes == 0 {
                tx.rollback();
                return Err(checkout_guard_failed);
            }
        }
    }

    // Record which rows are the deck's commander(s), by logical card so a
    // reprint representative still carries the flag.
    let mut commander_keys: HashSet<String> = HashSet::new();
    for e in &entries {
        if e.board == "commanders" {
            if let Some(Some(k)) = keys_by_id.get(&scrybox_card_id(&e.card)) {
                commander_keys.insert(k.clone());
            }
        }
    }
    mark_deck_commanders(&conn, target_deck_id, &commander_keys)?;

    // Maybeboard: replaced wholesale (its own table, never in deck stats).
    replace_maybeboard(&conn, target_deck_id, &maybe_entries)?;

    // Refresh the tracking row: the stamp we just mirrored is now "current".
    let stamp = js_or(details.get("lastUpdatedAtUtc"), &Value::Null).clone();
    let board_count = |board: &str| -> Value {
        match details.get("boards").and_then(|b| b.get(board)) {
            Some(b) if js_truthy(Some(b)) => b.get("count").cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        }
    };
    exec(
        &conn,
        "UPDATE moxfield_decks SET
        last_synced_updated_at = ?, last_content_sync_at = CURRENT_TIMESTAMP, last_error = NULL,
        name = ?, format = ?,
        mainboard_count = ?, sideboard_count = ?, maybeboard_count = ?, commander_count = ?
     WHERE id = ?",
        &params_of(vec![
            stamp.into(),
            js_or(details.get("name"), &row_val(row, "name").cloned().unwrap_or(Value::Null)).clone().into(),
            js_or(details.get("format"), &row_val(row, "format").cloned().unwrap_or(Value::Null)).clone().into(),
            board_count("mainboard").into(),
            board_count("sideboard").into(),
            board_count("maybeboard").into(),
            board_count("commanders").into(),
            row_i64(row, "id").into(),
        ]),
    )
    .map_err(SyncError::plain)?;
    // Name drift between the summary list and the details payload.
    if js_truthy(details.get("name")) {
        exec(
            &conn,
            "UPDATE decks SET name = ? WHERE id = ? AND source = 'moxfield'",
            &params_of(vec![details.get("name").cloned().unwrap_or(Value::Null).into(), target_deck_id.into()]),
        )
        .map_err(SyncError::plain)?;
    }

    tx.commit().map_err(|e| SyncError::plain(format!("{e:?}")))?;

    let counts = board_counts(&entries);
    let mut m = Map::new();
    m.insert("ok".into(), Value::from(true));
    m.insert("public_id".into(), Value::String(public_id.to_string()));
    m.insert(
        "deck".into(),
        js_or(details.get("name"), &row_val(row, "name").cloned().unwrap_or(Value::Null)).clone(),
    );
    m.insert("mainboard".into(), Value::from(counts[0]));
    m.insert("sideboard".into(), Value::from(counts[1]));
    m.insert("commanders".into(), Value::from(counts[3]));
    Ok(m)
}

// ---------------------------------------------------------------------------
// Per-deck enable switch + forced pulls
// ---------------------------------------------------------------------------

/// Flip one deck's import switch. Disabling removes the local mirror (and its
/// card quantities) but keeps the tracking row — the deck stays listed in the
/// UI and re-enabling re-imports it on the next content check, with no state
/// to reconstruct. Enabling works even for a deck that has never been
/// imported.
pub async fn set_deck_enabled(st: &AppState, user_id: i64, public_id: &str, enabled: bool) -> Result<Map<String, Value>, SyncError> {
    let row = {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        query_one(
            &conn,
            "SELECT md.*, a.moxfield_user AS author_user
     FROM moxfield_decks md
     JOIN moxfield_authors a ON a.id = md.author_id
     WHERE md.public_id = ? AND a.user_id = ?",
            &params_of(vec![public_id.into(), user_id.into()]),
        )
        .map_err(SyncError::plain)?
    };
    let row = match row {
        Some(r) => r,
        None => return Err(SyncError::plain("Moxfield deck not found for this user")),
    };

    if enabled {
        {
            let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
            exec(
                &conn,
                "UPDATE moxfield_decks SET enabled = 1, last_error = NULL WHERE id = ?",
                &params_of(vec![row_i64(&row, "id").into()]),
            )
            .map_err(SyncError::plain)?;
        }
        // Imported already: just re-armed. Not imported yet (or re-imported
        // after a disable): pull it now so the user sees the result
        // immediately instead of on the next clock.
        if js_truthy(row.get("bindarr_deck_id")) {
            let mut m = Map::new();
            m.insert("ok".into(), Value::from(true));
            m.insert("enabled".into(), Value::from(true));
            m.insert("imported".into(), Value::from(true));
            return Ok(m);
        }
        let author = {
            let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
            query_one(
                &conn,
                "SELECT * FROM moxfield_authors WHERE id = ?",
                &params_of(vec![row_i64(&row, "author_id").into()]),
            )
            .map_err(SyncError::plain)?
        };
        match author {
            Some(author) => {
                if let Err(err) = pull_deck_content(st, &author, public_id, Some(row.clone())).await {
                    let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
                    exec(
                        &conn,
                        "UPDATE moxfield_decks SET last_error = ? WHERE id = ?",
                        &params_of(vec![Value::String(err.message.clone()).into(), row_i64(&row, "id").into()]),
                    )
                    .map_err(SyncError::plain)?;
                }
            }
            None => {
                // Node would TypeError reading author.moxfield_user off
                // undefined; the FK makes this unreachable in practice.
                let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
                exec(
                    &conn,
                    "UPDATE moxfield_decks SET last_error = ? WHERE id = ?",
                    &params_of(vec![
                        Value::String("Cannot read properties of undefined (reading 'id')".into()).into(),
                        row_i64(&row, "id").into(),
                    ]),
                )
                .map_err(SyncError::plain)?;
            }
        }
        let fresh = {
            let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
            query_one(
                &conn,
                "SELECT bindarr_deck_id FROM moxfield_decks WHERE id = ?",
                &params_of(vec![row_i64(&row, "id").into()]),
            )
            .map_err(SyncError::plain)?
        };
        let mut m = Map::new();
        m.insert("ok".into(), Value::from(true));
        m.insert("enabled".into(), Value::from(true));
        m.insert("imported".into(), Value::from(js_truthy(fresh.as_ref().and_then(|f| f.get("bindarr_deck_id")))));
        return Ok(m);
    }

    let _guard = alloc_lock().lock().await;
    let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
    let mut tx = Tx::begin(&conn).map_err(|e| SyncError::plain(format!("{e:?}")))?;
    let current = query_one(
        &conn,
        "SELECT md.bindarr_deck_id, d.checked_out
      FROM moxfield_decks md
      LEFT JOIN decks d ON d.id = md.bindarr_deck_id
      WHERE md.id = ?",
        &params_of(vec![row_i64(&row, "id").into()]),
    )
    .map_err(SyncError::plain)?;
    let current = match current {
        Some(c) => c,
        None => {
            tx.rollback();
            return Err(SyncError::plain("Moxfield deck not found for this user"));
        }
    };
    if js_truthy(current.get("checked_out")) {
        tx.rollback();
        return Err(SyncError::conflict("Check this deck in before disabling its Moxfield mirror"));
    }
    if let Some(bind) = row_val(&current, "bindarr_deck_id") {
        let bind = bind.clone();
        exec(&conn, "DELETE FROM deck_cards WHERE deck_id = ?", &params_of(vec![bind.clone().into()])).map_err(SyncError::plain)?;
        exec(&conn, "DELETE FROM deck_maybe_cards WHERE deck_id = ?", &params_of(vec![bind.clone().into()])).map_err(SyncError::plain)?;
        exec(&conn, "DELETE FROM decks WHERE id = ? AND source = 'moxfield'", &params_of(vec![bind.into()])).map_err(SyncError::plain)?;
        exec(
            &conn,
            "UPDATE moxfield_decks SET enabled = 0, bindarr_deck_id = NULL, last_synced_updated_at = NULL, last_error = NULL WHERE id = ?",
            &params_of(vec![row_i64(&row, "id").into()]),
        )
        .map_err(SyncError::plain)?;
    } else {
        exec(
            &conn,
            "UPDATE moxfield_decks SET enabled = 0, last_error = NULL WHERE id = ?",
            &params_of(vec![row_i64(&row, "id").into()]),
        )
        .map_err(SyncError::plain)?;
    }
    tx.commit().map_err(|e| SyncError::plain(format!("{e:?}")))?;
    tracing::info!("Moxfield sync: deck \"{}\" import disabled (mirror removed)", row_str(&row, "name").unwrap_or_default());
    let mut m = Map::new();
    m.insert("ok".into(), Value::from(true));
    m.insert("enabled".into(), Value::from(false));
    Ok(m)
}

/// Re-pull one specific deck's contents for a user, regardless of its stamp.
/// Finds the tracking row under that user's authors (a public id belongs to
/// one author on Moxfield, so this is unique once found).
pub async fn pull_deck_content_by_public_id(st: &AppState, user_id: i64, public_id: &str) -> Result<Map<String, Value>, SyncError> {
    let row = {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        query_one(
            &conn,
            "SELECT md.* FROM moxfield_decks md
     JOIN moxfield_authors a ON a.id = md.author_id
     WHERE md.public_id = ? AND a.user_id = ?",
            &params_of(vec![public_id.into(), user_id.into()]),
        )
        .map_err(SyncError::plain)?
    };
    let row = match row {
        Some(r) => r,
        None => return Err(SyncError::plain("Moxfield deck not found for this user")),
    };
    let author = {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        query_one(
            &conn,
            "SELECT * FROM moxfield_authors WHERE id = ?",
            &params_of(vec![row_i64(&row, "author_id").into()]),
        )
        .map_err(SyncError::plain)?
    };
    let author = match author {
        Some(a) => a,
        // Node would TypeError on undefined.moxfield_user; FK-backed so this
        // is practically unreachable.
        None => return Err(SyncError::plain("Cannot read properties of undefined (reading 'id')")),
    };
    pull_deck_content(st, &author, public_id, Some(row)).await
}

// ---------------------------------------------------------------------------
// Author management
// ---------------------------------------------------------------------------

/// Track a new Moxfield author for this user, then run the first decklist
/// sync so the decks appear immediately.
pub async fn add_author(st: &AppState, user_id: i64, username: &Value) -> Result<Map<String, Value>, SyncError> {
    // String(username || '').trim()
    let clean = match username {
        Value::Null => String::new(),
        v if js_truthy(Some(v)) => js_to_string(v),
        _ => String::new(),
    }
    .trim()
    .to_string();
    if clean.is_empty() {
        return Err(SyncError::plain("Moxfield username is required"));
    }
    // Validate up-front so the UI gets a real 404 instead of a
    // silently-broken row.
    let user_info = st.mfx.get_user(&clean).await.map_err(SyncError::from_mox)?;
    let existing = {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        query_one(
            &conn,
            "SELECT id FROM moxfield_authors WHERE user_id = ? AND LOWER(moxfield_user) = LOWER(?)",
            &params_of(vec![user_id.into(), Value::String(user_info.user_name.clone()).into()]),
        )
        .map_err(SyncError::plain)?
    };
    if let Some(existing) = existing {
        let existing_id = row_i64(&existing, "id");
        let report = sync_decklist(st, existing_id, Some(user_id)).await?;
        let mut m = Map::new();
        m.insert("author".into(), Value::from(existing_id));
        m.insert("already_tracked".into(), Value::from(true));
        for (k, v) in report {
            m.insert(k, v); // spread overwrites author (in place) with the canonical name
        }
        return Ok(m);
    }
    let new_id = {
        let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
        exec(
            &conn,
            "INSERT INTO moxfield_authors (user_id, moxfield_user, display_name, profile_image_url) VALUES (?, ?, ?, ?)",
            &params_of(vec![
                user_id.into(),
                Value::String(user_info.user_name.clone()).into(),
                Value::String(user_info.display_name.clone()).into(),
                user_info
                    .profile_image_url
                    .as_ref()
                    .map(|s| Value::String(s.clone()))
                    .unwrap_or(Value::Null)
                    .into(),
            ]),
        )
        .map_err(SyncError::plain)?;
        conn.last_insert_rowid()
    };
    let report = sync_decklist(st, Some(new_id), Some(user_id)).await?;
    let mut m = Map::new();
    m.insert("author".into(), Value::from(new_id));
    for (k, v) in report {
        m.insert(k, v);
    }
    Ok(m)
}

/// Stop tracking an author: remove every mirrored deck this account produced.
pub async fn remove_author(st: &AppState, user_id: i64, author_id: Option<i64>) -> Result<Map<String, Value>, SyncError> {
    let _guard = alloc_lock().lock().await;
    let conn = st.db.as_ref().unwrap().get().map_err(|e| SyncError::plain(e.to_string()))?;
    let mut tx = Tx::begin(&conn).map_err(|e| SyncError::plain(format!("{e:?}")))?;
    let author = query_one(
        &conn,
        "SELECT * FROM moxfield_authors WHERE id = ? AND user_id = ?",
        // parseInt NaN → NULL binding → no row → not-found, like Node.
        &params_of(vec![P::OptInt(author_id), user_id.into()]),
    )
    .map_err(SyncError::plain)?;
    let author = match author {
        Some(a) => a,
        None => {
            tx.rollback();
            return Err(SyncError::plain("Moxfield author not found"));
        }
    };
    let tracked = query_rows(
        &conn,
        "SELECT md.bindarr_deck_id, d.checked_out
      FROM moxfield_decks md
      LEFT JOIN decks d ON d.id = md.bindarr_deck_id
      WHERE md.author_id = ?
    ",
        &params_of(vec![row_i64(&author, "id").into()]),
    )
    .map_err(SyncError::plain)?;
    if tracked.iter().any(|r| js_truthy(r.get("checked_out"))) {
        tx.rollback();
        return Err(SyncError::conflict("Check in every mirrored deck before removing this Moxfield author"));
    }
    for row in &tracked {
        if let Some(bind) = row_val(row, "bindarr_deck_id") {
            let bind = bind.clone();
            exec(&conn, "DELETE FROM deck_cards WHERE deck_id = ?", &params_of(vec![bind.clone().into()])).map_err(SyncError::plain)?;
            exec(&conn, "DELETE FROM deck_maybe_cards WHERE deck_id = ?", &params_of(vec![bind.clone().into()])).map_err(SyncError::plain)?;
            exec(&conn, "DELETE FROM decks WHERE id = ? AND source = 'moxfield'", &params_of(vec![bind.into()])).map_err(SyncError::plain)?;
        }
    }
    // Node relies on ON DELETE CASCADE, but the Rust pool never turns
    // foreign_keys on — delete the tracked rows explicitly, same tx.
    exec(&conn, "DELETE FROM moxfield_decks WHERE author_id = ?", &params_of(vec![row_i64(&author, "id").into()]))
        .map_err(SyncError::plain)?;
    exec(&conn, "DELETE FROM moxfield_authors WHERE id = ?", &params_of(vec![row_i64(&author, "id").into()]))
        .map_err(SyncError::plain)?;
    tx.commit().map_err(|e| SyncError::plain(format!("{e:?}")))?;
    let mut m = Map::new();
    m.insert("ok".into(), Value::from(true));
    m.insert("removed_decks".into(), Value::from(tracked.iter().filter(|r| js_truthy(r.get("bindarr_deck_id"))).count() as i64));
    Ok(m)
}

/// Status for the UI: every author this user tracks, with per-deck freshness.
pub fn get_status(conn: &Connection, user_id: i64) -> Result<Vec<Map<String, Value>>, String> {
    let mut authors = query_rows(
        conn,
        "SELECT a.id, a.moxfield_user, a.display_name, a.profile_image_url,
            a.last_decklist_sync_at, a.last_content_check_at, a.last_error, a.created_at
     FROM moxfield_authors a WHERE a.user_id = ? ORDER BY a.created_at DESC",
        &params_of(vec![user_id.into()]),
    )?;
    for a in authors.iter_mut() {
        // The aggregate describes what is actually being synced; unchecked
        // decks are shown in the deck list but don't count against the "up to
        // date" bar.
        let agg = query_one(
            conn,
            "SELECT COUNT(*) AS total,
              SUM(CASE WHEN last_synced_updated_at IS NOT NULL AND last_synced_updated_at = last_updated_at THEN 1 ELSE 0 END) AS current,
              SUM(CASE WHEN last_error IS NOT NULL THEN 1 ELSE 0 END) AS with_errors,
              MAX(last_content_sync_at) AS last_content_sync
       FROM moxfield_decks WHERE author_id = ? AND enabled = 1",
            &params_of(vec![row_i64(a, "id").into()]),
        )?;
        let total = agg.as_ref().and_then(|x| row_i64(x, "total")).unwrap_or(0);
        let current = agg.as_ref().and_then(|x| row_i64(x, "current")).unwrap_or(0);
        let with_errors = agg.as_ref().and_then(|x| row_i64(x, "with_errors")).unwrap_or(0);
        let last_content_sync = agg.as_ref().and_then(|x| x.get("last_content_sync")).cloned().unwrap_or(Value::Null);
        a.insert("total_decks".into(), Value::from(total));
        // JS `agg && agg.current ? agg.current : 0` — 0/NULL collapse to 0.
        a.insert("current_decks".into(), Value::from(if current != 0 { current } else { 0 }));
        a.insert("error_decks".into(), Value::from(if with_errors != 0 { with_errors } else { 0 }));
        a.insert("last_content_sync".into(), last_content_sync);
        let tracked = query_one(
            conn,
            "SELECT COUNT(*) AS c FROM moxfield_decks WHERE author_id = ?",
            &params_of(vec![row_i64(a, "id").into()]),
        )?;
        a.insert("tracked_decks".into(), Value::from(tracked.and_then(|t| row_i64(&t, "c")).unwrap_or(0)));

        // EVERY tracked deck (no enabled filter), newest Moxfield change first.
        let decks = query_rows(
            conn,
            "SELECT md.public_id, md.name, md.format, md.mainboard_count, md.sideboard_count,
              md.last_updated_at, md.last_synced_updated_at, md.last_content_sync_at, md.last_error,
              md.enabled
       FROM moxfield_decks md WHERE md.author_id = ? ORDER BY md.last_updated_at DESC",
            &params_of(vec![row_i64(a, "id").into()]),
        )?;
        let decks: Vec<Map<String, Value>> = decks
            .into_iter()
            .map(|mut d| {
                // "Allocated cards" = the cards in this deck's main slot, as
                // Moxfield reports it (includes the commander for EDH). The
                // target is the format's main-slot size.
                let synced = d.get("last_synced_updated_at").cloned().unwrap_or(Value::Null);
                let last_updated = d.get("last_updated_at").cloned().unwrap_or(Value::Null);
                let is_current = js_truthy(Some(&synced)) && synced == last_updated;
                let card_count = d.get("mainboard_count").cloned().unwrap_or(Value::Null);
                let card_target = target_size_for_format(d.get("format"));
                // `{...d, enabled: d.enabled !== 0, current: ..., card_count,
                // card_target}` — enabled keeps its SELECT position; current/
                // card_count/card_target append after it.
                d.insert("enabled".into(), Value::from(d.get("enabled") != Some(&Value::from(0))));
                d.insert("current".into(), Value::from(is_current));
                d.insert("card_count".into(), card_count);
                d.insert("card_target".into(), Value::from(card_target));
                d
            })
            .collect();
        a.insert("decks".into(), Value::Array(decks.into_iter().map(Value::Object).collect()));
    }
    Ok(authors)
}

// ---------------------------------------------------------------------------
// Scheduler — port of moxfieldScheduler.js (20s tick + boot catch-up)
// ---------------------------------------------------------------------------

const TICK_SECS: u64 = 20;
const DEFAULT_DECKLIST_MIN: i64 = 60;
const DEFAULT_CONTENT_MIN: i64 = 1;
const MIN_INTERVAL_MIN: i64 = 1;
const MAX_INTERVAL_MIN: i64 = 24 * 60;

/// `clampInterval` — parseInt, finite, >= 1, capped at a day.
pub fn clamp_interval(value: Option<&Value>, fallback: i64) -> i64 {
    match crate::mfx_payload::js_parse_int(value) {
        Some(n) if n >= MIN_INTERVAL_MIN => n.min(MAX_INTERVAL_MIN),
        _ => fallback,
    }
}

fn parse_sqlite_utc(value: Option<&str>) -> i64 {
    // SQLite CURRENT_TIMESTAMP is 'YYYY-MM-DD HH:MM:SS' in UTC; Node's
    // Date.parse of the T/Z-normalised form, NaN → 0.
    let Some(v) = value.filter(|s| !s.is_empty()) else { return 0 };
    let iso = format!("{}{}", v.replace(' ', "T"), if v.contains('Z') { "" } else { "Z" });
    chrono::DateTime::parse_from_rfc3339(&iso)
        .map(|d| d.timestamp_millis())
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(iso.trim_end_matches('Z'), "%Y-%m-%dT%H:%M:%S")
                .map(|d| d.and_utc().timestamp_millis())
        })
        .unwrap_or(0)
}

struct SchedState {
    last_decklist_at: HashMap<i64, i64>,
    last_content_at: HashMap<i64, i64>,
    in_flight: HashSet<i64>,
}

fn sched_state() -> &'static Mutex<SchedState> {
    static S: OnceLock<Mutex<SchedState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(SchedState {
        last_decklist_at: HashMap::new(),
        last_content_at: HashMap::new(),
        in_flight: HashSet::new(),
    }))
}

/// Poison-tolerant lock: a panic elsewhere must not wedge the scheduler.
fn sched() -> std::sync::MutexGuard<'static, SchedState> {
    sched_state().lock().unwrap_or_else(|e| e.into_inner())
}

/// RAII marker for `in_flight`: removes the author on drop, so an error or
/// a panic mid-sync never leaves the author skipped forever.
pub struct InFlightGuard(i64);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        sched().in_flight.remove(&self.0);
    }
}

/// Mark `author_id` in flight unless it already is.
pub fn try_in_flight(author_id: i64) -> Option<InFlightGuard> {
    if sched().in_flight.insert(author_id) {
        Some(InFlightGuard(author_id))
    } else {
        None
    }
}

/// Wait until no scheduler/boot pass is running for `author_id`, then mark
/// it in flight. Manual sync routes use this (wait-then-run keeps the Node
/// response shape — no new status code).
pub async fn wait_in_flight(author_id: i64) -> InFlightGuard {
    loop {
        if let Some(g) = try_in_flight(author_id) {
            return g;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn read_intervals(conn: &Connection) -> (i64, i64) {
    let row = query_one(
        conn,
        "SELECT moxfield_decklist_interval_min, moxfield_content_interval_min FROM app_settings WHERE id = 1",
        &[],
    )
    .ok()
    .flatten();
    (
        clamp_interval(row.as_ref().and_then(|r| r.get("moxfield_decklist_interval_min")), DEFAULT_DECKLIST_MIN),
        clamp_interval(row.as_ref().and_then(|r| r.get("moxfield_content_interval_min")), DEFAULT_CONTENT_MIN),
    )
}

pub async fn tick(st: &AppState) {
    let Some(pool) = st.db.as_ref() else { return };
    let authors = {
        let conn = match pool.get() {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!("Moxfield scheduler: no DB connection ({err})");
                return;
            }
        };
        match query_rows(
            &conn,
            "SELECT id, user_id, moxfield_user FROM moxfield_authors ORDER BY id",
            &[],
        ) {
            Ok(a) => a,
            Err(err) => {
                tracing::warn!("Moxfield scheduler: could not list authors ({err})");
                return;
            }
        }
    };
    if authors.is_empty() {
        return; // nobody tracks Moxfield — nothing to do
    }
    let (decklist_min, content_min) = {
        let conn = match pool.get() {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!("Moxfield scheduler: no DB connection ({err})");
                return;
            }
        };
        read_intervals(&conn)
    };
    let now = now_ms();

    for author in &authors {
        let author_id = row_i64(author, "id").unwrap_or(0);
        if sched().in_flight.contains(&author_id) {
            continue; // one sync per author at a time
        }
        let (last_deck, last_content) = {
            let s = sched();
            (
                s.last_decklist_at.get(&author_id).copied().unwrap_or(0),
                s.last_content_at.get(&author_id).copied().unwrap_or(0),
            )
        };
        let decklist_due = now - last_deck >= decklist_min * 60 * 1000;
        let content_due = now - last_content >= content_min * 60 * 1000;
        // Only one of the two should run per tick: the slower one wins when
        // both are due (a decklist pass already refreshes every stamp).
        let (job, mark_decklist) = if decklist_due {
            ("decklist", true)
        } else if content_due {
            ("content", false)
        } else {
            continue;
        };
        let Some(_in_flight) = try_in_flight(author_id) else { continue };
        {
            let mut s = sched();
            if mark_decklist {
                s.last_decklist_at.insert(author_id, now);
            } else {
                s.last_content_at.insert(author_id, now);
            }
        }
        let uid = row_i64(author, "user_id");
        let muser = row_str(author, "moxfield_user").unwrap_or_default();
        let result = if job == "decklist" {
            sync_decklist(st, Some(author_id), uid)
                .await
                .map(|report| {
                    tracing::info!(
                        "Moxfield sync: decklist for {muser} — {} new, {} removed, {} total",
                        report.get("decks_created").and_then(|v| v.as_i64()).unwrap_or(0),
                        report.get("decks_removed").and_then(|v| v.as_i64()).unwrap_or(0),
                        report.get("decks_on_moxfield").and_then(|v| v.as_i64()).unwrap_or(0),
                    );
                })
        } else {
            run_content_sync(st, Some(author_id), uid)
                .await
                .map(|report| {
                    let updated = report.get("updated").and_then(|v| v.as_i64()).unwrap_or(0);
                    if updated > 0 {
                        let checked = report.get("checked").and_then(|v| v.as_i64()).unwrap_or(0);
                        tracing::info!("Moxfield sync: {updated} of {checked} decks for {muser} changed on Moxfield — mirrored");
                    }
                })
        };
        if let Err(err) = result {
            // Record it on the author row so the UI can show what went
            // wrong, and back off this tick only — the next due tick retries.
            tracing::warn!("Moxfield sync ({job}) for {muser} failed: {}", err.message);
            if let Ok(conn) = pool.get() {
                let _ = exec(
                    &conn,
                    "UPDATE moxfield_authors SET last_error = ? WHERE id = ?",
                    &params_of(vec![Value::String(err.message).into(), author_id.into()]),
                );
            }
        }
    }
}

async fn boot_catch_up(st: AppState) {
    let Some(pool) = st.db.clone() else { return };
    let authors = {
        let conn = match pool.get() {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!("Moxfield sync boot catch-up failed: {err}");
                return;
            }
        };
        match query_rows(
            &conn,
            "SELECT a.id, a.user_id, a.moxfield_user, a.last_decklist_sync_at
     FROM moxfield_authors a",
            &[],
        ) {
            Ok(a) => a,
            Err(err) => {
                tracing::warn!("Moxfield sync boot catch-up failed: {err}");
                return;
            }
        }
    };
    let decklist_min = {
        let conn = match pool.get() {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!("Moxfield sync boot catch-up failed: {err}");
                return;
            }
        };
        read_intervals(&conn).0
    };
    let now = now_ms();
    for a in authors {
        // A never-synced author (NULL stamp) or one whose last decklist is
        // older than the interval runs the full decklist, which pulls new
        // decks' contents on the way.
        let author_id = row_i64(&a, "id").unwrap_or(0);
        let last = parse_sqlite_utc(row_str(&a, "last_decklist_sync_at").as_deref());
        let decklist_stale = last == 0 || now - last >= decklist_min * 60 * 1000;
        if !decklist_stale {
            continue;
        }
        if let Some(in_flight) = try_in_flight(author_id) {
            sched().last_decklist_at.insert(author_id, now);
            let st = st.clone();
            let uid = row_i64(&a, "user_id");
            let muser = row_str(&a, "moxfield_user").unwrap_or_default();
            // Fire-and-forget per author, like Node's .then/.catch chain.
            tokio::spawn(async move {
                let _in_flight = in_flight;
                match sync_decklist(&st, Some(author_id), uid).await {
                    Ok(_) => tracing::info!("Moxfield sync: boot decklist for {muser} complete"),
                    Err(err) => {
                        tracing::warn!("Moxfield sync: boot decklist for {muser} failed: {}", err.message)
                    }
                }
            });
        }
    }
}

pub fn spawn_scheduler(st: &AppState) {
    if st.db.is_none() {
        tracing::warn!("Moxfield scheduler not started: no database");
        return;
    }
    // Node's startMoxfieldScheduler fires boot_catch_up fire-and-forget and
    // arms the 20s interval immediately: two independent tasks, and the
    // interval's FIRST fire is one full tick after start (setInterval).
    let st_boot = st.clone();
    tokio::spawn(async move {
        boot_catch_up(st_boot).await;
    });
    let st = st.clone();
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(TICK_SECS));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        timer.tick().await; // consume the immediate first poll
        loop {
            // Own task per tick: a panic loses that tick only, like a
            // rejected promise under Node's setInterval.
            let st_tick = st.clone();
            if let Err(e) = tokio::spawn(async move { tick(&st_tick).await }).await {
                tracing::error!("Moxfield scheduler tick panicked: {e}");
            }
            timer.tick().await;
        }
    });
}

// ---------------------------------------------------------------------------
// Routes — port of routes/moxfield.js
// ---------------------------------------------------------------------------

/// `parseInt(req.params.id, 10)` — prefix parse; 'abc' → NaN → upstream the
/// NaN binds NULL, no row matches, and the not-found path answers, exactly
/// like Node passing NaN to the query.
fn js_int_param(raw: &str) -> Option<i64> {
    crate::mfx_payload::js_parse_int(Some(&Value::String(raw.to_string())))
}

fn is_not_found(msg: &str) -> bool {
    msg.to_lowercase().contains("not found")
}

async fn auth_gate(st: &AppState, headers: &HeaderMap, is_get: bool) -> Result<i64, Response> {
    let pool = st.db.as_ref().unwrap();
    let token = bearer_token(headers).map(|s| s.to_string());
    let auth = authenticate(pool, token.as_deref(), is_get).map_err(|res| res)?;
    Ok(auth.id)
}

pub async fn handle_status(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::GET || !st.owns("moxfield") || st.db.is_none() {
        return crate::proxy(State(st), req).await;
    }
    let (parts, _body) = req.into_parts();
    let user_id = match auth_gate(&st, &parts.headers, true).await {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    let authors = {
        let conn = match st.db.as_ref().unwrap().get() {
            Ok(c) => c,
            Err(_) => return json_response(StatusCode::INTERNAL_SERVER_ERROR, &err_body("Failed to load Moxfield sync status")),
        };
        match get_status(&conn, user_id) {
            Ok(a) => a,
            Err(_) => return json_response(StatusCode::INTERNAL_SERVER_ERROR, &err_body("Failed to load Moxfield sync status")),
        }
    };
    // Raw app_settings column values, no coercion: NULL columns answer null,
    // and 60/1 default ONLY when the row itself is missing (routes/moxfield.js:19-27).
    let settings = {
        let conn = st.db.as_ref().unwrap().get().ok();
        conn.and_then(|c| {
            query_one(
                &c,
                "SELECT moxfield_decklist_interval_min, moxfield_content_interval_min FROM app_settings WHERE id = 1",
                &[],
            )
            .ok()
            .flatten()
        })
    };
    let (decklist_min, content_min) = match &settings {
        Some(s) => (
            s.get("moxfield_decklist_interval_min").cloned().unwrap_or(Value::from(60)),
            s.get("moxfield_content_interval_min").cloned().unwrap_or(Value::from(1)),
        ),
        None => (Value::from(60), Value::from(1)),
    };
    let mut intervals = Map::new();
    intervals.insert("decklist_min".into(), decklist_min);
    intervals.insert("content_min".into(), content_min);
    let mut body = Map::new();
    body.insert("authors".into(), Value::Array(authors.into_iter().map(Value::Object).collect()));
    body.insert("intervals".into(), Value::Object(intervals));
    json_response(StatusCode::OK, &Value::Object(body))
}

fn err_body(msg: &str) -> Value {
    let mut m = Map::new();
    m.insert("error".into(), Value::String(msg.to_string()));
    Value::Object(m)
}

fn err_detail_body(msg: &str, detail: &str) -> Value {
    let mut m = Map::new();
    m.insert("error".into(), Value::String(msg.to_string()));
    m.insert("detail".into(), Value::String(detail.to_string()));
    Value::Object(m)
}

pub async fn handle_add_author(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::POST || !st.owns("moxfield") || st.db.is_none() {
        return crate::proxy(State(st), req).await;
    }
    let (parts, bytes, oversize) = read_body(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    let headers = parts.headers.clone();
    let body = match parse_body(&headers, &bytes) {
        BodyOutcome::TooLarge => {
            return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Upload too large. Try exporting/importing in smaller batches.")
        }
        BodyOutcome::Invalid => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        }
        BodyOutcome::Parsed(v) => v,
    };
    let user_id = match auth_gate(&st, &headers, false).await {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    // req.body.username — the RAW value; addAuthor does String(username||'').
    let username = body.get("username").cloned().unwrap_or(Value::Null);
    match add_author(&st, user_id, &username).await {
        Ok(report) => json_response(StatusCode::CREATED, &Value::Object(report)),
        Err(err) => {
            // routes/moxfield.js:42-49 — MoxfieldError OR /Moxfield has no
            // user/ → 404, then /Moxfield username is required/ → 400, else
            // 500 with detail. (ALLOCATION_CONFLICT is NOT a MoxfieldError —
            // Node falls through to the 500 branch for it.)
            if matches!(err.kind, ErrKind::Mox(_)) || err.message.contains("Moxfield has no user") {
                return json_response(StatusCode::NOT_FOUND, &err_body(&err.message));
            }
            if err.message.contains("Moxfield username is required") {
                return json_response(StatusCode::BAD_REQUEST, &err_body(&err.message));
            }
            tracing::error!("moxfield add-author failed: {}", err.message);
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &err_detail_body("Failed to add Moxfield author", &err.message),
            )
        }
    }
}

pub async fn handle_delete_author(State(st): State<AppState>, Path(id): Path<String>, req: Request) -> Response {
    if req.method() != Method::DELETE || !st.owns("moxfield") || st.db.is_none() {
        return crate::proxy(State(st), req).await;
    }
    let (parts, _body) = req.into_parts();
    let user_id = match auth_gate(&st, &parts.headers, false).await {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    match remove_author(&st, user_id, js_int_param(&id)).await {
        Ok(report) => json_response(StatusCode::OK, &Value::Object(report)),
        Err(err) => {
            // routes/moxfield.js:59-62 — 409 ONLY for ALLOCATION_CONFLICT,
            // /not found/i → 404, else 500 WITHOUT detail.
            if err.kind == ErrKind::Conflict {
                return json_response(StatusCode::CONFLICT, &err_body(&err.message));
            }
            if is_not_found(&err.message) {
                return json_response(StatusCode::NOT_FOUND, &err_body(&err.message));
            }
            tracing::error!("moxfield remove-author failed: {}", err.message);
            json_response(StatusCode::INTERNAL_SERVER_ERROR, &err_body("Failed to remove Moxfield author"))
        }
    }
}

async fn sync_author_route(
    st: &AppState,
    req: Request,
    job: &str,
    id: String,
) -> Response {
    if !st.owns("moxfield") || st.db.is_none() {
        return crate::proxy(State(st.clone()), req).await;
    }
    let (parts, _body) = req.into_parts();
    let user_id = match auth_gate(&st, &parts.headers, false).await {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    let author_id = js_int_param(&id);
    // Share the scheduler's in_flight set: wait for a running tick/boot pass
    // for this author instead of racing it (same response as Node).
    let _in_flight = match author_id {
        Some(a) => Some(wait_in_flight(a).await),
        None => None,
    };
    let result = if job == "decklist" {
        sync_decklist(st, author_id, Some(user_id)).await
    } else {
        run_content_sync(st, author_id, Some(user_id)).await
    };
    match result {
        Ok(report) => json_response(StatusCode::OK, &Value::Object(report)),
        Err(err) => {
            // routes/moxfield.js:72-78/87-93 — /not found/i first, then
            // MoxfieldError (404 → 404 else 502), then 500 + detail.
            if is_not_found(&err.message) {
                return json_response(StatusCode::NOT_FOUND, &err_body(&err.message));
            }
            if let ErrKind::Mox(status) = err.kind {
                let code = if status == 404 { StatusCode::NOT_FOUND } else { StatusCode::BAD_GATEWAY };
                return json_response(code, &err_body(&err.message));
            }
            tracing::error!("moxfield {job} sync failed: {}", err.message);
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &err_detail_body(
                    if job == "decklist" { "Decklist sync failed" } else { "Content sync failed" },
                    &err.message,
                ),
            )
        }
    }
}

pub async fn handle_sync_decklist(State(st): State<AppState>, Path(id): Path<String>, req: Request) -> Response {
    if req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    sync_author_route(&st, req, "decklist", id).await
}

pub async fn handle_sync_contents(State(st): State<AppState>, Path(id): Path<String>, req: Request) -> Response {
    if req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    sync_author_route(&st, req, "content", id).await
}

pub async fn handle_deck_sync(
    State(st): State<AppState>,
    Path(public_id): Path<String>,
    req: Request,
) -> Response {
    if req.method() != Method::POST || !st.owns("moxfield") || st.db.is_none() {
        return crate::proxy(State(st), req).await;
    }
    let public_id = match decode_uri_component(&public_id) {
        Some(p) => p,
        None => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    };
    let (parts, _body) = req.into_parts();
    let user_id = match auth_gate(&st, &parts.headers, false).await {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    match pull_deck_content_by_public_id(&st, user_id, &public_id).await {
        Ok(report) => json_response(StatusCode::OK, &Value::Object(report)),
        Err(err) => {
            // routes/moxfield.js:103-111 — 409 ONLY for ALLOCATION_CONFLICT;
            // /not found|not tracked/i → 404; MoxfieldError 404→404 else 502.
            if err.kind == ErrKind::Conflict {
                return json_response(StatusCode::CONFLICT, &err_body(&err.message));
            }
            let lower = err.message.to_lowercase();
            if lower.contains("not found") || lower.contains("not tracked") {
                return json_response(StatusCode::NOT_FOUND, &err_body(&err.message));
            }
            if let ErrKind::Mox(status) = err.kind {
                let code = if status == 404 { StatusCode::NOT_FOUND } else { StatusCode::BAD_GATEWAY };
                return json_response(code, &err_body(&err.message));
            }
            tracing::error!("moxfield deck sync failed: {}", err.message);
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &err_detail_body("Deck sync failed", &err.message),
            )
        }
    }
}

pub async fn handle_deck_update(
    State(st): State<AppState>,
    Path(public_id): Path<String>,
    req: Request,
) -> Response {
    if req.method() != Method::PUT || !st.owns("moxfield") || st.db.is_none() {
        return crate::proxy(State(st), req).await;
    }
    let (parts, bytes, oversize) = read_body(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    let headers = parts.headers.clone();
    let body = match parse_body(&headers, &bytes) {
        BodyOutcome::TooLarge => {
            return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Upload too large. Try exporting/importing in smaller batches.")
        }
        BodyOutcome::Invalid => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        }
        BodyOutcome::Parsed(v) => v,
    };
    // `const enabled = req.body.enabled === true;` — strict boolean true.
    let enabled = body.get("enabled") == Some(&Value::Bool(true));
    let public_id = match decode_uri_component(&public_id) {
        Some(p) => p,
        None => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    };
    let user_id = match auth_gate(&st, &headers, false).await {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    match set_deck_enabled(&st, user_id, &public_id, enabled).await {
        Ok(report) => json_response(StatusCode::OK, &Value::Object(report)),
        Err(err) => {
            // routes/moxfield.js:123-126 — 409 ONLY for ALLOCATION_CONFLICT;
            // /not found/i → 404; else 500 + detail (no MoxfieldError branch).
            if err.kind == ErrKind::Conflict {
                return json_response(StatusCode::CONFLICT, &err_body(&err.message));
            }
            if is_not_found(&err.message) {
                return json_response(StatusCode::NOT_FOUND, &err_body(&err.message));
            }
            tracing::error!("moxfield deck update failed: {}", err.message);
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &err_detail_body("Failed to update Moxfield deck", &err.message),
            )
        }
    }
}

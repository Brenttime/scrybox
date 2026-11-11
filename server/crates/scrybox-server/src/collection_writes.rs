//! Native port of the `backend/src/routes/collection.js` WRITE routes:
//! `GET /collection/cardlist`, `PUT /collection/:id`, `DELETE /collection/:id`
//! and `POST /collection/bulk`. Mounted under the `collection_writes` name in
//! RUST_ROUTES; anything on those paths the port does not own (methods, and
//! the PUT card_id branch when the target printing is not in card_cache —
//! Node would hydrate it from Scryfall, which Rust never does) is forwarded
//! to Node through the existing `crate::proxy` path so upstream stays
//! canonical there.
//!
//! Every status, body and message was written against the actual Node source
//! (routes/collection.js, utils/collectionHelpers.js, utils/deckRules.js,
//! utils/splitPrice.js, utils/cardIdentity.js, shared/cardListText.js) — the
//! brief's prose is secondary to that source wherever they disagree (see the
//! module tests; deliberate deviations are documented on the functions).

use std::sync::OnceLock;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::extract::Request;
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use serde_json::{json, Map, Value};

use crate::auth::{authenticate, bearer_token};
use crate::collection::{body_etag, etag_matches, json_num, sqlite_value, truthy, js_parse_int};
use crate::db::DbPool;
use crate::stats;
use crate::AppState;

// ---------------------------------------------------------------------------
// Constants mirroring routes/collection.js
// ---------------------------------------------------------------------------

/// routes/collection.js `PRINTING_VALUES` (also the collection.printing CHECK).
const PRINTING_VALUES: &[&str] = &["Normal", "Holofoil"];
/// routes/collection.js `BULK_ACTIONS` (line 852 — favorite is NOT in it).
const BULK_ACTIONS: &[&str] = &[
    "delete", "trade", "untrade", "condition", "printing", "purchase_split", "add_to_deck",
];
/// routes/collection.js `BULK_CONDITIONS`.
const BULK_CONDITIONS: &[&str] = &[
    "Near Mint", "Lightly Played", "Moderately Played", "Heavily Played", "Damaged",
];
/// routes/collection.js `BULK_PRINTINGS`.
const BULK_PRINTINGS: &[&str] = &["Normal", "Holofoil"];
/// deckRules.js `isBasicLand` canonical basic names.
const BASIC_NAMES: &[&str] = &["Plains", "Island", "Swamp", "Mountain", "Forest", "Wastes"];

/// Node's withAllocationLock is a per-process FIFO mutex. Rust mirrors it with
/// one async mutex around every allocation-guarded section (plus BEGIN
/// IMMEDIATE for cross-process serialisation against Node's own writes).
fn alloc_lock() -> &'static tokio::sync::Mutex<()> {
    static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

// ---------------------------------------------------------------------------
// Response helpers (same Node-exact shape as auth_routes.rs: res.json emits
// application/json; charset=utf-8 + the Express default weak body ETag)
// ---------------------------------------------------------------------------

fn json_response(status: StatusCode, value: &Value) -> Response {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    (
        status,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8")),
            (header::ETAG, HeaderValue::from_str(&body_etag(&body)).unwrap()),
        ],
        Body::from(body),
    )
        .into_response()
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    json_response(status, &json!({ "error": msg }))
}

/// Re-stamp an error Response produced elsewhere (auth.rs builds axum::Json
/// bodies without the Express charset/ETag) into Node's exact res.json form,
/// keeping the JSON body byte-identical.
pub(crate) async fn node_exact(res: Response) -> Response {
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap_or_default();
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => json_response(status, &v),
        Err(_) => (status, Body::from(bytes)).into_response(),
    }
}

/// routes/collection.js:695/816/834/840 — PUT and DELETE share it.
const ENTRY_NOT_FOUND: &str = "Collection entry not found";
/// routes/collection.js:820 — ALLOCATION_CONFLICT.
const ALLOCATION_CONFLICT: &str = "Check in the deck using this card before reducing the owned quantity.";

// ---------------------------------------------------------------------------
// JS semantics helpers
// ---------------------------------------------------------------------------

/// JS `String(value)` for the JSON values that can arrive in a request body
/// (or come out of SQLite through `sqlite_value`).
fn js_to_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "null".to_string(), // `null` prints as null; undefined handled by callers passing None
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|e| match e {
                // Array.prototype.toString = join(","): null/undefined render
                // empty, nested arrays recurse, objects render as [object Object].
                Value::Null => String::new(),
                Value::Array(inner) => js_array_join(inner),
                _ => js_to_string(Some(e)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

fn js_array_join(a: &[Value]) -> String {
    a.iter()
        .map(|e| match e {
            Value::Null => String::new(),
            Value::Array(inner) => js_array_join(inner),
            _ => js_to_string(Some(e)),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// JS `Number(value)` for comparisons (`Number(owned_qty) - Number(x) <
/// Number(locked_qty)` and friends).
fn js_to_number(v: Option<&Value>) -> f64 {
    match v {
        None | Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(other) => {
            let s = js_to_string(Some(other)).trim().to_string();
            if s.is_empty() {
                0.0
            } else {
                s.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
    }
}

/// `Math.round` (closest integer, ties toward +Infinity), NaN/Infinity
/// passed through.
fn math_round(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() {
        return x;
    }
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

/// JS `parseInt(String(v), 10)` (NaN modelled as None).
fn js_parse_int_v(v: Option<&Value>) -> Option<i64> {
    js_parse_int(&js_to_string(v))
}

/// JS `parseFloat(String(s))` — longest valid numeric prefix, leading
/// whitespace and sign allowed, "Infinity" recognised.
fn js_parse_float(s: &str) -> f64 {
    let t = s.trim_start_matches(|c: char| c.is_whitespace());
    let (sign, rest) = match t.as_bytes().first() {
        Some(b'+') => (1.0, &t[1..]),
        Some(b'-') => (-1.0, &t[1..]),
        _ => (1.0, t),
    };
    if let Some(stripped) = rest.strip_prefix("Infinity") {
        let _ = stripped;
        return sign * f64::INFINITY;
    }
    let b = rest.as_bytes();
    let mut i = 0usize;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i;
    if i < b.len() && b[i] == b'.' {
        let mut j = i + 1;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        let frac_digits = j - (i + 1);
        if int_digits == 0 && frac_digits == 0 {
            return f64::NAN;
        }
        i = j;
    } else if int_digits == 0 {
        return f64::NAN;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let exp_start = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            i = j;
        }
    }
    sign * rest[..i].parse::<f64>().unwrap_or(f64::NAN)
}

/// `(total).toFixed(2)` for the purchase_split message. total >= 0 (or -0) is
/// validated before this runs; per the spec ladder it reduces to the closest
/// cent with ties toward +Infinity, i.e. Math.round(total*100) (brief +
/// ECMA-262 agree here).
fn js_fixed2(total: f64) -> String {
    if total.is_nan() {
        return "NaN".to_string();
    }
    if total.is_infinite() {
        // Only +Infinity survives the total >= 0 gate (-Infinity fails).
        return "Infinity".to_string();
    }
    let c = math_round(total * 100.0);
    if c == 0.0 {
        return "0.00".to_string(); // also the -0 case
    }
    if c.abs() >= 9.007_199_254_740_992e15 {
        return format!("{total:.2}"); // unrealistic money; keep digits
    }
    let ci = c as i64;
    format!("{}.{:02}", ci / 100, ci % 100)
}

/// Bind a JSON body value to SQLite the way Node's sqlite3 driver binds the
/// JS value. Objects/arrays are rejected: node-sqlite3 throws on binding
/// them, which the route's catch turns into the route's 500 — reproduced via
/// `WriteError::Bind`.
fn json_to_sql(v: Option<&Value>) -> Result<SqlValue, WriteError> {
    Ok(match v {
        None | Some(Value::Null) => SqlValue::Null,
        Some(Value::Bool(b)) => SqlValue::Integer(if *b { 1 } else { 0 }),
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) => SqlValue::Integer(i),
            None => SqlValue::Real(n.as_f64().unwrap_or(f64::NAN)),
        },
        Some(Value::String(s)) => SqlValue::Text(s.clone()),
        Some(_) => return Err(WriteError::Bind),
    })
}

fn json_to_sql_value(v: &Value) -> SqlValue {
    // For values read back out of SQLite (already normalised JSON) — no bind
    // errors possible.
    json_to_sql(Some(v)).unwrap_or(SqlValue::Null)
}

/// Row-map value with a JSON null default (rows come from SQLite, so they
/// can never contain the object/array forms `json_to_sql` rejects).
fn dv(m: &Map<String, Value>, k: &str) -> Value {
    m.get(k).cloned().unwrap_or(Value::Null)
}

/// JS truthiness over `Option<&Value>` (collection::truthy takes &Value).
fn tr(v: Option<&Value>) -> bool {
    v.is_some_and(|x| truthy(x))
}

// ---------------------------------------------------------------------------
// SQL fragments — twins of utils/cardIdentity.js
// ---------------------------------------------------------------------------

fn sql_card_key(alias: &str) -> String {
    format!("LOWER(TRIM(COALESCE({alias}.name, '')))")
}

fn sql_is_basic_land(alias: &str) -> String {
    let subtypes = format!("LOWER(COALESCE({alias}.subtypes, ''))");
    let is_land =
        format!("(LOWER(COALESCE({alias}.supertype, '')) = 'land' OR INSTR({subtypes}, '\"land\"') > 0)");
    let is_basic_type = format!(
        "({key} IN ('plains', 'island', 'swamp', 'mountain', 'forest', 'wastes') OR INSTR({subtypes}, '\"basic\"') > 0)",
        key = sql_card_key(alias)
    );
    format!("({is_land} AND {is_basic_type})")
}

// ---------------------------------------------------------------------------
// DB plumbing
// ---------------------------------------------------------------------------

enum WriteError {
    Sql(rusqlite::Error),
    /// node-sqlite3 refuses to bind an object/array — same 500 as its throw.
    Bind,
}

impl From<rusqlite::Error> for WriteError {
    fn from(e: rusqlite::Error) -> Self {
        WriteError::Sql(e)
    }
}

impl std::fmt::Debug for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Sql(e) => write!(f, "sql: {e}"),
            WriteError::Bind => write!(f, "unsupported bind value"),
        }
    }
}

fn get_row_map(conn: &Connection, sql: &str, params: &[SqlValue]) -> Result<Option<Map<String, Value>>, WriteError> {
    let mut stmt = conn.prepare_cached(sql)?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    match rows.next()? {
        None => Ok(None),
        Some(r) => {
            let mut m = Map::with_capacity(cols.len());
            for (i, c) in cols.iter().enumerate() {
                m.insert(c.clone(), sqlite_value(r, i)?);
            }
            Ok(Some(m))
        }
    }
}

fn get_rows_map(conn: &Connection, sql: &str, params: &[SqlValue]) -> Result<Vec<Map<String, Value>>, WriteError> {
    let mut stmt = conn.prepare_cached(sql)?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let mut m = Map::with_capacity(cols.len());
        for (i, c) in cols.iter().enumerate() {
            m.insert(c.clone(), sqlite_value(r, i)?);
        }
        out.push(m);
    }
    Ok(out)
}

fn get_i64(conn: &Connection, sql: &str, params: &[SqlValue]) -> Result<Option<i64>, WriteError> {
    Ok(get_row_map(conn, sql, params)?
        .and_then(|m| m.values().next().cloned())
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))))
}

/// A BEGIN IMMEDIATE transaction with guaranteed finalisation. Node's
/// withAllocationLock is only an in-process mutex (no DB transaction); Rust
/// additionally needs a real write transaction so the multi-statement
/// allocate/read-check-write sequences are atomic and serialise against
/// Node's own writes on the shared WAL file.
struct Tx<'a> {
    conn: &'a Connection,
    done: bool,
}

impl<'a> Tx<'a> {
    fn begin(conn: &'a Connection) -> Result<Self, WriteError> {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(Self { conn, done: false })
    }
    fn commit(&mut self) -> Result<(), WriteError> {
        self.done = true;
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }
    fn rollback(&mut self) {
        if !self.done {
            let _ = self.conn.execute_batch("ROLLBACK");
            self.done = true;
        }
    }
}

impl Drop for Tx<'_> {
    fn drop(&mut self) {
        self.rollback();
    }
}

/// logicalInventoryStatus(database, userId, cardId) — collectionHelpers.js,
/// with sqlCardKey/sqlIsBasicLand expanded as in cardIdentity.js.
fn logical_inventory_status(
    conn: &Connection,
    user_id: i64,
    card_id: &Value,
) -> Result<Option<Map<String, Value>>, WriteError> {
    let sql = format!(
        "SELECT
           target.id AS card_id,
           {key} AS card_key,
           CASE WHEN {basic} THEN 1 ELSE 0 END AS is_basic_land,
           (
             SELECT COALESCE(SUM(c.quantity), 0)
             FROM collection c
             JOIN card_cache owned_cc ON owned_cc.id = c.card_id
             WHERE c.user_id = ?1
               AND c.quantity > 0
               AND {owned_key} = {key}
           ) AS owned_qty,
           (
             SELECT COALESCE(SUM(dc.quantity), 0)
             FROM deck_cards dc
             JOIN card_cache locked_cc ON locked_cc.id = dc.card_id
             JOIN decks d ON d.id = dc.deck_id
             WHERE d.user_id = ?2
               AND d.checked_out = 1
               AND dc.quantity > 0
               AND NOT {locked_basic}
               AND {locked_key} = {key}
           ) AS locked_qty
         FROM card_cache target
         WHERE target.id = ?3"
    ,
        key = sql_card_key("target"),
        basic = sql_is_basic_land("target"),
        owned_key = sql_card_key("owned_cc"),
        locked_basic = sql_is_basic_land("locked_cc"),
        locked_key = sql_card_key("locked_cc"),
    );
    get_row_map(conn, &sql, &[SqlValue::Integer(user_id), SqlValue::Integer(user_id), json_to_sql_value(card_id)])
}

/// setStackQuantity(database, userId, entryId, target) — collectionHelpers.js
/// lines 134-174, ported exactly: grow by inserting single-copy rows that
/// mirror the edited row; shrink the siblings (ORDER BY id DESC, newest
/// first) before touching the edited row; finally pin the edited row to
/// `target`. NOTE: the brief described a MIN_STACK_QTY/stackable variant —
/// that does NOT exist in the Node source of this repo, which wins.
fn set_stack_quantity(
    conn: &Connection,
    user_id: i64,
    entry_id: &Value,
    target: i64,
) -> Result<(), WriteError> {
    let row = match get_row_map(
        conn,
        "SELECT * FROM collection WHERE id = ? AND user_id = ?",
        &[json_to_sql_value(entry_id), SqlValue::Integer(user_id)],
    )? {
        Some(r) => r,
        None => return Ok(()), // Node returns 0 (no-op)
    };
    let siblings = get_rows_map(
        conn,
        "SELECT id, quantity FROM collection
         WHERE user_id = ?1 AND card_id = ?2 AND condition = ?3 AND printing = ?4
           AND language = ?5 AND id != ?6 AND quantity > 0
           AND COALESCE(is_proxy, 0) = ?7
         ORDER BY id DESC",
        &[
            SqlValue::Integer(user_id),
            json_to_sql_value(&dv(&row, "card_id")),
            json_to_sql_value(&dv(&row, "condition")),
            json_to_sql_value(&dv(&row, "printing")),
            json_to_sql_value(&dv(&row, "language")),
            json_to_sql_value(entry_id),
            SqlValue::Integer(if tr(row.get("is_proxy")) { 1 } else { 0 }),
        ],
    )?;

    let mut current: f64 = js_to_number(row.get("quantity")).max(0.0);
    for s in &siblings {
        current += js_to_number(s.get("quantity")).max(0.0);
    }

    // Grow: one single-card row per missing copy, mirroring the edited row.
    while current < target as f64 {
        conn.execute(
            "INSERT INTO collection (
               card_id, user_id, quantity, condition, printing, language, purchase_price,
               is_trade, favorite, is_proxy
             ) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params_from_iter(vec![
                json_to_sql(row.get("card_id"))?,
                SqlValue::Integer(user_id),
                json_to_sql(row.get("condition"))?,
                json_to_sql(row.get("printing"))?,
                json_to_sql(row.get("language"))?,
                json_to_sql(row.get("purchase_price"))?,
                json_to_sql(row.get("is_trade"))?,
                json_to_sql(row.get("favorite"))?,
                SqlValue::Integer(if tr(row.get("is_proxy")) { 1 } else { 0 }),
            ]),
        )?;
        current += 1.0;
    }

    // Shrink: trim candidates (newest first) first, edited row last.
    for s in &siblings {
        if current <= target as f64 {
            break;
        }
        let have = js_to_number(s.get("quantity")).max(0.0);
        let drop = have.min(current - target as f64);
        current -= drop;
        if drop >= have {
            conn.execute(
                "DELETE FROM collection WHERE id = ? AND user_id = ?",
                rusqlite::params_from_iter(vec![json_to_sql(s.get("id"))?, SqlValue::Integer(user_id)]),
            )?;
        } else {
            conn.execute(
                "UPDATE collection SET quantity = quantity - ? WHERE id = ? AND user_id = ?",
                rusqlite::params_from_iter(vec![
                    SqlValue::Real(drop),
                    json_to_sql(s.get("id"))?,
                    SqlValue::Integer(user_id),
                ]),
            )?;
        }
    }

    if current > target as f64 {
        conn.execute(
            "UPDATE collection SET quantity = ? WHERE id = ? AND user_id = ?",
            rusqlite::params_from_iter(vec![SqlValue::Integer(target), json_to_sql_value(entry_id), SqlValue::Integer(user_id)]),
        )?;
    }
    Ok(())
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

// ---------------------------------------------------------------------------
// Body parsing — normalJson (express.json {limit:'1mb', strict:true}) which
// server.js mounts globally ahead of the auth gate for non-large paths, so
// parse failures outrank auth errors exactly as in Node.
// ---------------------------------------------------------------------------

pub(crate) enum BodyOutcome {
    Parsed(Value),
    /// entity.parse.failed → the global error handler's 500.
    Invalid,
    /// entity.too.large → the global handler's 413.
    TooLarge,
}

fn is_json_ct(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            let main = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            main == "application/json"
        })
        .unwrap_or(false)
}

pub(crate) fn parse_body(headers: &HeaderMap, bytes: &[u8]) -> BodyOutcome {
    if !is_json_ct(headers) {
        // body-parser leaves req.body undefined; Node's route would throw on
        // destructuring it (unhandled rejection). The port treats it as `{}`,
        // matching the auth_routes convention — documented deviation.
        return BodyOutcome::Parsed(Value::Object(Map::new()));
    }
    if bytes.len() > 1024 * 1024 {
        return BodyOutcome::TooLarge;
    }
    if bytes.is_empty() {
        return BodyOutcome::Parsed(Value::Object(Map::new()));
    }
    match serde_json::from_slice::<Value>(bytes) {
        // express.json({strict:true}): only objects (and arrays — which
        // destructure to nothing useful, like auth_routes) are legal.
        Ok(v) if v.is_object() => BodyOutcome::Parsed(v),
        Ok(v) if v.is_array() => BodyOutcome::Parsed(Value::Object(Map::new())),
        _ => BodyOutcome::Invalid,
    }
}

fn too_large() -> Response {
    json_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "Upload too large. Try exporting/importing in smaller batches.",
    )
}

fn internal_error() -> Response {
    json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
}

// ---------------------------------------------------------------------------
// Router entries
// ---------------------------------------------------------------------------

async fn read_parts(req: Request) -> (axum::http::request::Parts, Vec<u8>) {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default().to_vec();
    (parts, bytes)
}

fn rebuild(parts: axum::http::request::Parts, bytes: &[u8]) -> Request {
    Request::from_parts(parts, Body::from(bytes.to_vec()))
}

/// `GET /collection/cardlist` — routes/collection.js:349.
pub async fn handle_cardlist(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::GET {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes) = read_parts(req).await;
    let headers = parts.headers.clone();
    let uri = parts.uri.clone();

    match parse_body(&headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(_) => {}
    }

    let token = bearer_token(&headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), true) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };

    match cardlist(&pool, user.id, &uri, &headers) {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(error = ?e, "cardlist failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to build card list")
        }
    }
}

/// shared/cardListText.js `norm`: String(v == null ? '' : v).trim().
fn norm(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(x) => js_to_string(Some(x)).trim().to_string(),
    }
}

fn cardlist(pool: &DbPool, user_id: i64, uri: &Uri, headers: &HeaderMap) -> Result<Response, WriteError> {
    let conn = pool.get()?;
    // Node: `Number(unresolved && unresolved.count) > 0` — count is always
    // numeric from COUNT(*).
    let count = get_i64(
        &conn,
        "SELECT COUNT(*) AS count
         FROM collection c
         LEFT JOIN card_cache cc ON cc.id = c.card_id
         WHERE c.user_id = ?1 AND c.quantity > 0 AND cc.id IS NULL",
        &[SqlValue::Integer(user_id)],
    )?
    .unwrap_or(0);
    if count > 0 {
        return Ok(json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The collection contains cards whose details are unavailable. Repair their metadata before exporting.",
        ));
    }
    // Node: req.query.style === 'detailed' ? 'detailed' : 'plain'.
    let style = uri
        .query()
        .map(|q| {
            form_urlencoded::parse(q.as_bytes())
                .find(|(k, _)| k == "style")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let detailed = style == "detailed";

    let rows = get_rows_map(
        &conn,
        "SELECT c.quantity, cc.name, cc.set_id, cc.number
         FROM collection c
         JOIN card_cache cc ON c.card_id = cc.id
         WHERE c.user_id = ?1 AND c.quantity > 0
         ORDER BY c.added_at DESC, c.id DESC",
        &[SqlValue::Integer(user_id)],
    )?;

    // shared/cardListText.js buildCardListText, over exactly the selected
    // columns (no set_code/collector_number on these rows).
    let mut lines: Vec<String> = Vec::with_capacity(rows.len());
    for r in &rows {
        let qty = js_parse_int_v(r.get("quantity")).unwrap_or(1);
        if qty <= 0 {
            continue;
        }
        let name = norm(r.get("name"));
        let line = if detailed {
            let set = r
                .get("set_id")
                .map(|v| if tr(Some(v)) { js_to_string(Some(v)).trim().to_uppercase() } else { String::new() })
                .unwrap_or_default();
            let num = norm(r.get("number"));
            let mut l = name;
            if !set.is_empty() {
                l.push_str(&format!(" ({set})"));
            }
            if !num.is_empty() {
                l.push_str(&format!(" {num}"));
            }
            format!("{qty} {l}")
        } else {
            format!("{qty} {name}")
        };
        lines.push(line);
    }
    let text = lines.join("\n");
    let bytes = text.into_bytes();
    let tag = body_etag(&bytes);

    // Express send(): a matching If-None-Match answers 304, and express
    // strips Content-Type/Content-Length on 304 while keeping the ETag.
    let inm = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    if inm.is_some_and(|inm| etag_matches(Some(inm), &tag)) {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, HeaderValue::from_str(&tag).unwrap())],
        )
            .into_response());
    }
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8")),
            (header::ETAG, HeaderValue::from_str(&tag).unwrap()),
        ],
        Body::from(bytes),
    )
        .into_response())
}

/// `PUT`/`DELETE` `/collection/:id` — routes/collection.js:686 / :829.
pub async fn handle_entry(Path(id): Path<String>, State(st): State<AppState>, req: Request) -> Response {
    let method = req.method().clone();
    if method != Method::PUT && method != Method::DELETE {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes) = read_parts(req).await;
    let headers = parts.headers.clone();

    let body = match parse_body(&headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(b) => b,
    };

    let token = bearer_token(&headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };

    let resp = if method == Method::PUT {
        match put_entry(&pool, &id, user.id, &body).await {
            PutOutcome::Response(r) => r,
            // Rust does not port Node's Scryfall hydration (cardApi.js): hand
            // the untouched original request to Node so its own
            // getCardById/cache-retry path answers (and performs the write).
            PutOutcome::Forward => {
                return crate::proxy(State(st), rebuild(parts, &bytes)).await;
            }
        }
    } else {
        delete_entry(&pool, &id, user.id).await
    };

    // Node bumps the user's stats generation via the invalidateOnWrite
    // middleware on every authenticated write; mirror it for the native
    // success responses (400/404/409 skips are documented deviations).
    if resp.status().is_success() {
        stats::on_proxied_write(&st, &method, format!("/api/collection/{id}").as_str(), &headers);
    }
    resp
}

/// `POST /collection/bulk` — routes/collection.js:856.
pub async fn handle_bulk(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let (parts, bytes) = read_parts(req).await;
    let headers = parts.headers.clone();

    let body = match parse_body(&headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(b) => b,
    };

    let token = bearer_token(&headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };

    let resp = match bulk(&pool, user.id, &body).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = ?e, "bulk action failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Bulk action failed")
        }
    };
    if resp.status().is_success() {
        stats::on_proxied_write(&st, &Method::POST, "/api/collection/bulk", &headers);
    }
    let _ = parts;
    resp
}

// ---------------------------------------------------------------------------
// PUT /collection/:id
// ---------------------------------------------------------------------------

enum PutOutcome {
    Response(Response),
    Forward,
}

async fn put_entry(pool: &DbPool, id: &str, user_id: i64, body: &Value) -> PutOutcome {
    match put_inner(pool, id, user_id, body).await {
        Ok(PutOutcome::Response(r)) => PutOutcome::Response(r),
        Ok(PutOutcome::Forward) => PutOutcome::Forward,
        Err(e) => {
            tracing::error!(error = ?e, "Failed to update entry");
            PutOutcome::Response(json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update entry"))
        }
    }
}

fn defined<'a>(body: &'a Value, key: &str) -> Option<&'a Value> {
    // `x !== undefined` after JSON destructure: the key being present is
    // exactly "defined" (a JSON null IS defined in Node's check).
    body.get(key)
}

async fn put_inner(pool: &DbPool, id: &str, user_id: i64, body: &Value) -> Result<PutOutcome, WriteError> {
    let conn = pool.get()?;

    // Entry lookup — SELECT *, id bound as the raw TEXT route param exactly
    // like Node (INTEGER affinity does the numeric comparison).
    let id_param = SqlValue::Text(id.to_string());
    let entry = match get_row_map(
        &conn,
        "SELECT * FROM collection WHERE id = ?1 AND user_id = ?2",
        &[id_param, SqlValue::Integer(user_id)],
    )? {
        Some(e) => e,
        None => return Ok(PutOutcome::Response(json_error(StatusCode::NOT_FOUND, ENTRY_NOT_FOUND))),
    };

    // Printing change: the new printing must be a different row of the same
    // game card (canonical English name). BEFORE the body-printing
    // validation, exactly as Node orders it.
    let new_card_id: Option<String> = match defined(body, "card_id") {
        Some(v) if !v.is_null() => {
            let s = js_to_string(Some(v));
            if s != js_to_string(entry.get("card_id")) {
                Some(s)
            } else {
                None
            }
        }
        _ => None,
    };

    // Node's `updates`/`params` arrays: card_id is pushed by the printing
    // branch (collection.js:718), the rest below in Node's order.
    let mut updates: Vec<(&str, Value)> = Vec::new();

    if let Some(nc) = &new_card_id {
        // cardApi.hydrate is a no-op in Node; cache miss ⇒ Node fetches from
        // Scryfall — not portable here, so forward the whole request.
        let target = get_row_map(
            &conn,
            "SELECT * FROM card_cache WHERE id = ?1",
            &[SqlValue::Text(nc.clone())],
        )?;
        let Some(target) = target else { return Ok(PutOutcome::Forward) };

        let current_name = get_row_map(
            &conn,
            "SELECT name FROM card_cache WHERE id = ?1",
            &[json_to_sql(entry.get("card_id"))?],
        )?
        .and_then(|r| r.get("name").cloned());
        let norm = |v: Option<&Value>| js_to_string(v).trim().to_string();
        if norm(current_name.as_ref()) != norm(target.get("name")) {
            return Ok(PutOutcome::Response(json_error(
                StatusCode::BAD_REQUEST,
                "A printing change must stay on the same card.",
            )));
        }
        // Node collection.js:718 — updates.push('card_id = ?'); params.push(newCardId)
        updates.push(("card_id", Value::String(nc.clone())));
    }

    // quantity !== undefined ? Math.max(1, parseInt(quantity, 10) || 1) : null
    let requested_qty: Option<i64> = if defined(body, "quantity").is_some() {
        let parsed = js_parse_int_v(defined(body, "quantity"));
        let x = match parsed {
            None | Some(0) => 1,
            Some(p) => p,
        };
        Some(x.max(1))
    } else {
        None
    };

    if let Some(v) = defined(body, "condition") {
        updates.push(("condition", v.clone()));
    }
    if let Some(v) = defined(body, "printing") {
        let ok = PRINTING_VALUES.iter().any(|p| Some(*p) == v.as_str());
        if !ok {
            return Ok(PutOutcome::Response(json_error(
                StatusCode::BAD_REQUEST,
                &format!("Invalid printing: {}", js_to_string(Some(v))),
            )));
        }
        updates.push(("printing", v.clone()));
    }
    if let Some(v) = defined(body, "language") {
        updates.push(("language", v.clone()));
    }
    if let Some(v) = defined(body, "purchase_price") {
        updates.push(("purchase_price", v.clone()));
    }
    if let Some(v) = defined(body, "is_trade") {
        updates.push(("is_trade", Value::Bool(tr(Some(v)))));
    }
    if let Some(v) = defined(body, "notes") {
        updates.push(("notes", v.clone()));
    }
    if let Some(v) = defined(body, "is_proxy") {
        updates.push(("is_proxy", Value::Bool(tr(Some(v)))));
    }

    let touches_physical_stack = defined(body, "quantity").is_some()
        || new_card_id.is_some()
        || defined(body, "condition").is_some()
        || defined(body, "printing").is_some()
        || defined(body, "language").is_some()
        || defined(body, "is_proxy").is_some();

    let run_update = |conn: &Connection, current_entry: &Map<String, Value>| -> Result<(), WriteError> {
        if updates.is_empty() {
            return Ok(());
        }
        let cols: Vec<String> = updates.iter().map(|(c, _)| format!("{c} = ?")).collect();
        let mut params: Vec<SqlValue> = updates
            .iter()
            .map(|(_, v)| json_to_sql(Some(v)))
            .collect::<Result<_, _>>()?;
        params.push(json_to_sql_value(current_entry.get("id").unwrap_or(&Value::Null)));
        params.push(SqlValue::Integer(user_id));
        conn.execute(
            &format!("UPDATE collection SET {} WHERE id = ? AND user_id = ?", cols.join(", ")),
            rusqlite::params_from_iter(params.iter()),
        )?;
        Ok(())
    };

    if !touches_physical_stack {
        // Node: saveEntry() without the allocation lock — the entry read up
        // front, a plain UPDATE, no quantity reconciliation.
        run_update(&conn, &entry)?;
        return Ok(PutOutcome::Response(json_response(
            StatusCode::OK,
            &json!({ "message": "Collection entry updated successfully" }),
        )));
    }

    // withAllocationLock(saveEntry): in-process mutex + BEGIN IMMEDIATE.
    let _guard = alloc_lock().lock().await;
    let mut tx = Tx::begin(&conn)?;

    let current_entry = match get_row_map(
        &conn,
        "SELECT * FROM collection WHERE id = ?1 AND user_id = ?2",
        &[SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)],
    )? {
        Some(c) => c,
        None => {
            tx.rollback();
            return Ok(PutOutcome::Response(json_error(
                StatusCode::NOT_FOUND,
                "Collection entry not found",
            )));
        }
    };

    if let Some(tq) = requested_qty {
        // cardIdForStack = newCardId || currentEntry.card_id
        let card_id_for_stack: Value = match &new_card_id {
            Some(s) => Value::String(s.clone()),
            None => current_entry.get("card_id").cloned().unwrap_or(Value::Null),
        };
        let target_condition = defined(body, "condition")
            .cloned()
            .unwrap_or_else(|| current_entry.get("condition").cloned().unwrap_or(Value::Null));
        let target_printing = defined(body, "printing")
            .cloned()
            .unwrap_or_else(|| current_entry.get("printing").cloned().unwrap_or(Value::Null));
        let target_language = defined(body, "language")
            .cloned()
            .unwrap_or_else(|| current_entry.get("language").cloned().unwrap_or(Value::Null));
        let target_proxy = match defined(body, "is_proxy") {
            Some(v) => tr(Some(v)),
            None => tr(current_entry.get("is_proxy")),
        };

        let siblings = get_rows_map(
            &conn,
            "SELECT quantity FROM collection
             WHERE user_id = ?1 AND card_id = ?2 AND condition = ?3 AND printing = ?4
               AND language = ?5 AND id != ?6 AND quantity > 0
               AND COALESCE(is_proxy, 0) = ?7",
            &[
                SqlValue::Integer(user_id),
                json_to_sql(Some(&card_id_for_stack))?,
                json_to_sql(Some(&target_condition))?,
                json_to_sql(Some(&target_printing))?,
                json_to_sql(Some(&target_language))?,
                json_to_sql_value(entry.get("id").unwrap_or(&Value::Null)),
                SqlValue::Integer(if target_proxy { 1 } else { 0 }),
            ],
        )?;

        let mut projected = js_to_number(current_entry.get("quantity")).max(0.0);
        for s in &siblings {
            projected += js_to_number(s.get("quantity")).max(0.0);
        }
        let reduction = (projected - tq as f64).max(0.0);
        if reduction > 0.0 {
            let status = logical_inventory_status(&conn, user_id, current_entry.get("card_id").unwrap_or(&Value::Null))?;
            let conflict = match status {
                None => true,
                Some(s) => {
                    js_to_number(s.get("owned_qty")) - reduction < js_to_number(s.get("locked_qty"))
                }
            };
            if conflict {
                tx.rollback();
                return Ok(PutOutcome::Response(json_error(StatusCode::CONFLICT, ALLOCATION_CONFLICT)));
            }
        }
    }

    // run_update binds current_entry.id exactly like Node binds entry.id
    // (both are the same integer row id).
    run_update(&conn, &current_entry)?;
    if let Some(tq) = requested_qty {
        set_stack_quantity(&conn, user_id, entry.get("id").unwrap_or(&Value::Null), tq)?;
    }
    tx.commit()?;

    Ok(PutOutcome::Response(json_response(
        StatusCode::OK,
        &json!({ "message": "Collection entry updated successfully" }),
    )))
}

// ---------------------------------------------------------------------------
// DELETE /collection/:id
// ---------------------------------------------------------------------------

async fn delete_entry(pool: &DbPool, id: &str, user_id: i64) -> Response {
    match delete_inner(pool, id, user_id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = ?e, "Failed to remove card");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to remove card")
        }
    }
}

async fn delete_inner(pool: &DbPool, id: &str, user_id: i64) -> Result<Response, WriteError> {
    let _guard = alloc_lock().lock().await;
    let conn = pool.get()?;
    let mut tx = Tx::begin(&conn)?;

    let entry = match get_row_map(
        &conn,
        "SELECT id, card_id, quantity FROM collection WHERE id = ?1 AND user_id = ?2",
        &[SqlValue::Text(id.to_string()), SqlValue::Integer(user_id)],
    )? {
        Some(e) => e,
        None => {
            tx.rollback();
            return Ok(json_error(StatusCode::NOT_FOUND, ENTRY_NOT_FOUND));
        }
    };

    let status = logical_inventory_status(&conn, user_id, entry.get("card_id").unwrap_or(&Value::Null))?;
    let conflict = match &status {
        None => true,
        Some(s) => {
            js_to_number(s.get("owned_qty")) - js_to_number(entry.get("quantity"))
                < js_to_number(s.get("locked_qty"))
        }
    };
    if conflict {
        tx.rollback();
        // routes/collection.js:837 — DELETE's own 409 string.
        return Ok(json_error(
            StatusCode::CONFLICT,
            "Check in the deck using this card before removing it.",
        ));
    }

    let changes = conn.execute(
        "DELETE FROM collection WHERE id = ? AND user_id = ?",
        rusqlite::params_from_iter(vec![
            json_to_sql(entry.get("id"))?,
            SqlValue::Integer(user_id),
        ]),
    )?;
    tx.commit()?;
    if changes == 0 {
        return Ok(json_error(StatusCode::NOT_FOUND, ENTRY_NOT_FOUND));
    }
    Ok(json_response(StatusCode::OK, &json!({ "message": "Card removed from collection" })))
}

// ---------------------------------------------------------------------------
// POST /collection/bulk
// ---------------------------------------------------------------------------

async fn bulk(pool: &DbPool, user_id: i64, body: &Value) -> Result<Response, WriteError> {
    // const { entry_ids = [], action, value } = req.body — a missing key
    // takes the [] default; an explicit null fails Array.isArray.
    let entry_ids_val = defined(body, "entry_ids").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
    let entry_ids: Vec<Value> = match &entry_ids_val {
        Value::Array(a) if !a.is_empty() => a.clone(),
        _ => return Ok(json_error(StatusCode::BAD_REQUEST, "entry_ids is required")),
    };
    let action = match defined(body, "action").and_then(Value::as_str) {
        Some(a) if BULK_ACTIONS.contains(&a) => a.to_string(),
        _ => return Ok(json_error(StatusCode::BAD_REQUEST, "Invalid action")),
    };
    let value = defined(body, "value").cloned();

    // entry_ids.map(n => parseInt(n, 10)).filter(Number.isInteger)
    let ids: Vec<i64> = entry_ids.iter().filter_map(|n| js_parse_int_v(Some(n))).collect();
    if ids.is_empty() {
        return Ok(json_error(StatusCode::BAD_REQUEST, "No valid entry_ids"));
    }
    let ph = placeholders(ids.len());
    let id_params: Vec<SqlValue> = ids.iter().map(|i| SqlValue::Integer(*i)).collect();

    match action.as_str() {
        "add_to_deck" => bulk_add_to_deck(pool, user_id, value.as_ref(), &id_params, &ph),
        "delete" => bulk_delete(pool, user_id, &id_params, &ph).await,
        "trade" | "untrade" => {
            let conn = pool.get()?;
            let set = if action == "trade" { 1 } else { 0 };
            let mut params = vec![SqlValue::Integer(set)];
            params.extend(id_params.iter().cloned());
            params.push(SqlValue::Integer(user_id));
            let changes = conn.execute(
                &format!("UPDATE collection SET is_trade = ? WHERE id IN ({ph}) AND user_id = ?"),
                rusqlite::params_from_iter(params.iter()),
            )?;
            Ok(json_response(
                StatusCode::OK,
                &json!({ "message": format!("Updated {changes} card(s)"), "affected": changes }),
            ))
        }
        "condition" | "printing" => {
            let allowed: &[&str] = if action == "condition" { BULK_CONDITIONS } else { BULK_PRINTINGS };
            let ok = value.as_ref().and_then(Value::as_str).is_some_and(|v| allowed.contains(&v));
            if !ok {
                return Ok(json_error(StatusCode::BAD_REQUEST, &format!("Invalid {action}")));
            }
            // withAllocationLock around the single stack-key UPDATE.
            let _guard = alloc_lock().lock().await;
            let conn = pool.get()?;
            let mut params = vec![json_to_sql(value.as_ref())?];
            params.extend(id_params.iter().cloned());
            params.push(SqlValue::Integer(user_id));
            let changes = conn.execute(
                &format!("UPDATE collection SET {action} = ? WHERE id IN ({ph}) AND user_id = ?"),
                rusqlite::params_from_iter(params.iter()),
            )?;
            Ok(json_response(
                StatusCode::OK,
                &json!({ "message": format!("Set {action} on {changes} card(s)"), "affected": changes }),
            ))
        }
        "purchase_split" => bulk_purchase_split(pool, user_id, value.as_ref(), &id_params, &ph),
        // Every BULK_ACTION has a branch above; anything reaching here would
        // be one Node itself has no branch for → falls to Unknown action.
        _ => Ok(json_error(StatusCode::BAD_REQUEST, "Unknown action")),
    }
}

async fn bulk_delete(pool: &DbPool, user_id: i64, id_params: &[SqlValue], ph: &str) -> Result<Response, WriteError> {
    let _guard = alloc_lock().lock().await;
    let conn = pool.get()?;
    let mut tx = Tx::begin(&conn)?;
    let mut params: Vec<SqlValue> = id_params.to_vec();
    params.push(SqlValue::Integer(user_id));
    let removals = get_rows_map(
        &conn,
        &format!(
            "SELECT MIN(c.card_id) AS card_id, SUM(c.quantity) AS remove_qty
             FROM collection c
             JOIN card_cache selected_cc ON selected_cc.id = c.card_id
             WHERE c.id IN ({ph}) AND c.user_id = ? AND c.quantity > 0
             GROUP BY {}",
            sql_card_key("selected_cc")
        ),
        &params,
    )?;
    for removal in &removals {
        let status = logical_inventory_status(&conn, user_id, removal.get("card_id").unwrap_or(&Value::Null))?;
        let conflict = match &status {
            None => true,
            Some(s) => {
                js_to_number(s.get("owned_qty")) - js_to_number(removal.get("remove_qty"))
                    < js_to_number(s.get("locked_qty"))
            }
        };
        if conflict {
            tx.rollback();
            return Ok(json_error(
                StatusCode::CONFLICT,
                "Check in the deck using the selected card before removing it.",
            ));
        }
    }
    let changes = conn.execute(
        &format!("DELETE FROM collection WHERE id IN ({ph}) AND user_id = ?"),
        rusqlite::params_from_iter(params.iter()),
    )?;
    tx.commit()?;
    Ok(json_response(
        StatusCode::OK,
        &json!({ "message": format!("Deleted {changes} card(s)"), "affected": changes }),
    ))
}

/// utils/splitPrice.js splitPrice(prices, total, method) — verbatim, integer
/// cents with the remainder landing on the last card.
fn split_price(prices: &[f64], total: f64, method: &str) -> Vec<f64> {
    let n = prices.len();
    if n == 0 {
        return Vec::new();
    }
    let cents = math_round(total * 100.0);
    let sum: f64 = prices.iter().map(|p| if p.is_nan() || *p == 0.0 { 0.0 } else { *p }).sum();
    let weighted = method == "weighted" && sum > 0.0;
    let mut allocated = 0.0f64;
    prices
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i == n - 1 {
                return (cents - allocated) / 100.0;
            }
            let share = if weighted {
                math_round(cents * (if p.is_nan() { 0.0 } else { *p }) / sum)
            } else {
                math_round(cents / n as f64)
            };
            allocated += share;
            share / 100.0
        })
        .collect()
}

fn bulk_purchase_split(
    pool: &DbPool,
    user_id: i64,
    value: Option<&Value>,
    id_params: &[SqlValue],
    ph: &str,
) -> Result<Response, WriteError> {
    // const total = parseFloat(value && value.total);
    let total_str = match value {
        Some(v) if tr(Some(v)) && v.is_object() => match v.get("total") {
            Some(t) => js_to_string(Some(t)),
            None => "undefined".to_string(),
        },
        // Falsy value, or a primitive without a `total` property.
        _ => "undefined".to_string(),
    };
    let total = js_parse_float(&total_str);
    if !(total >= 0.0) {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "total must be a non-negative number",
        ));
    }
    let method = match value {
        Some(v) if tr(Some(v)) && v.is_object() && v.get("method") == Some(&Value::String("equal".into())) => {
            "equal"
        }
        _ => "weighted",
    };

    let conn = pool.get()?;
    let mut params: Vec<SqlValue> = id_params.to_vec();
    params.push(SqlValue::Integer(user_id));
    let rows = get_rows_map(
        &conn,
        &format!(
            "SELECT c.id, COALESCE(cc.price_trend, 0) AS price FROM collection c
             LEFT JOIN card_cache cc ON cc.id = c.card_id
             WHERE c.id IN ({ph}) AND c.user_id = ?"
        ),
        &params,
    )?;
    if rows.is_empty() {
        return Ok(json_error(StatusCode::BAD_REQUEST, "No valid entries"));
    }
    // r.price || 0 — falsy (null/0/''/NaN) prices count as 0.
    let prices: Vec<f64> = rows
        .iter()
        .map(|r| {
            let p = r.get("price");
            if tr(p) {
                js_to_number(p)
            } else {
                0.0
            }
        })
        .collect();
    let sum: f64 = prices.iter().sum();
    let weighted = method == "weighted" && sum > 0.0;
    let shares = split_price(&prices, total, method);
    for (i, row) in rows.iter().enumerate() {
        conn.execute(
            "UPDATE collection SET purchase_price = ? WHERE id = ? AND user_id = ?",
            rusqlite::params_from_iter(vec![
                SqlValue::Real(shares[i]),
                json_to_sql(row.get("id"))?,
                SqlValue::Integer(user_id),
            ]),
        )?;
    }
    Ok(json_response(
        StatusCode::OK,
        &json!({
            "message": format!(
                "Split ${} across {} card(s) ({})",
                js_fixed2(total),
                rows.len(),
                if weighted { "by value" } else { "evenly" }
            ),
            "affected": rows.len(),
        }),
    ))
}

/// deckRules.js isBasicLand(card).
fn is_basic_land(card: &Map<String, Value>) -> bool {
    let subs: Vec<Value> = match card.get("subtypes") {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) if !s.is_empty() => serde_json::from_str::<Value>(s)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let has = |needle: &str| subs.iter().any(|s| s == &Value::String(needle.into()));
    let supertype_land = card.get("supertype") == Some(&Value::String("Land".into()));
    let name = js_to_string(card.get("name"));
    (has("Land") || supertype_land)
        && (has("Basic") || BASIC_NAMES.contains(&name.as_str()))
}

/// deckRules.js validateDeckAddition — returns Err(message) for the reject
/// strings. deckId is unused by the Node implementation; kept out.
fn validate_deck_addition(
    conn: &Connection,
    user_id: i64,
    card_id: &Value,
    new_qty: f64,
) -> Result<Result<(), String>, WriteError> {
    // const qty = parseInt(newQty, 10);
    let qty_str = js_to_string(Some(&json_num(new_qty)));
    let qty = match js_parse_int(&qty_str) {
        Some(q) if q >= 0 => q,
        _ => return Ok(Err("Invalid quantity".to_string())),
    };
    let card = match get_row_map(
        conn,
        "SELECT id, name, supertype, subtypes FROM card_cache WHERE id = ?1",
        &[json_to_sql_value(card_id)],
    )? {
        Some(c) => c,
        None => return Ok(Err("Card not found".to_string())),
    };
    let name = card.get("name").cloned().unwrap_or(Value::Null);
    let owned_row = get_row_map(
        conn,
        &format!(
            "SELECT COALESCE(SUM(c.quantity), 0) AS owned
             FROM collection c
             JOIN card_cache owned_cc ON owned_cc.id = c.card_id
             WHERE c.user_id = ?1 AND c.quantity > 0
               AND {} = LOWER(TRIM(?2))",
            sql_card_key("owned_cc")
        ),
        &[SqlValue::Integer(user_id), json_to_sql_value(&name)],
    )?;
    let owned: Value = owned_row.and_then(|r| r.get("owned").cloned()).unwrap_or(Value::from(0));
    let owned_num = js_to_number(Some(&owned));
    if (qty as f64) > owned_num {
        let unit = if owned.as_f64() == Some(1.0) { "copy" } else { "copies" };
        let owned_display = js_to_string(Some(&owned));
        return Ok(Err(format!(
            "You only own {owned_display} {unit} of {}.",
            js_to_string(Some(&name))
        )));
    }
    if !is_basic_land(&card) && qty > 4 {
        return Ok(Err(format!(
            "Cannot have more than 4 copies of {}.",
            js_to_string(Some(&name))
        )));
    }
    Ok(Ok(()))
}

fn bulk_add_to_deck(
    pool: &DbPool,
    user_id: i64,
    value: Option<&Value>,
    id_params: &[SqlValue],
    ph: &str,
) -> Result<Response, WriteError> {
    // const deckId = parseInt(value, 10); if (!deckId) 400
    let deck_id = match js_parse_int_v(value) {
        Some(d) if d != 0 => d,
        _ => return Ok(json_error(StatusCode::BAD_REQUEST, "Invalid deck_id")),
    };
    let conn = pool.get()?;
    let deck = get_row_map(
        &conn,
        "SELECT id, checked_out FROM decks WHERE id = ? AND user_id = ?",
        &[SqlValue::Integer(deck_id), SqlValue::Integer(user_id)],
    )?;
    let Some(deck) = deck else {
        return Ok(json_error(StatusCode::NOT_FOUND, "Deck not found"));
    };
    if tr(deck.get("checked_out")) {
        return Ok(json_error(
            StatusCode::CONFLICT,
            "Check this deck in before changing its cards.",
        ));
    }

    let mut params: Vec<SqlValue> = id_params.to_vec();
    params.push(SqlValue::Integer(user_id));
    let rows = get_rows_map(
        &conn,
        &format!(
            "SELECT MIN(c.card_id) AS card_id, SUM(c.quantity) AS total_qty
             FROM collection c
             JOIN card_cache selected_cc ON selected_cc.id = c.card_id
             WHERE c.id IN ({ph}) AND c.user_id = ? AND c.quantity > 0
             GROUP BY {}",
            sql_card_key("selected_cc")
        ),
        &params,
    )?;

    let ek = sql_card_key("existing_cc");
    let tk = sql_card_key("target_cc");

    let mut added: f64 = 0.0;
    let mut rejected: Vec<String> = Vec::new();
    for row in &rows {
        let row_card_id = row.get("card_id").cloned().unwrap_or(Value::Null);
        let equivalent = get_row_map(
            &conn,
            &format!(
                "SELECT dc.card_id
                 FROM deck_cards dc
                 JOIN card_cache existing_cc ON existing_cc.id = dc.card_id
                 JOIN card_cache target_cc ON target_cc.id = ?1
                 WHERE dc.deck_id = ?2
                   AND {ek} = {tk}
                 ORDER BY (dc.card_id = ?3) DESC, dc.card_id
                 LIMIT 1"
            ),
            &[
                json_to_sql_value(&row_card_id),
                SqlValue::Integer(deck_id),
                json_to_sql_value(&row_card_id),
            ],
        )?
        .and_then(|r| r.get("card_id").cloned());
        let effective = equivalent.clone().unwrap_or_else(|| row_card_id.clone());

        let existing = get_row_map(
            &conn,
            &format!(
                "SELECT COALESCE(SUM(dc.quantity), 0) AS quantity
                 FROM deck_cards dc
                 JOIN card_cache existing_cc ON existing_cc.id = dc.card_id
                 JOIN card_cache target_cc ON target_cc.id = ?1
                 WHERE dc.deck_id = ?2
                   AND {ek} = {tk}"
            ),
            &[json_to_sql_value(&effective), SqlValue::Integer(deck_id)],
        )?;
        let current = existing
            .as_ref()
            .map(|e| js_to_number(e.get("quantity")))
            .unwrap_or(0.0);
        let total_qty = js_to_number(row.get("total_qty"));
        let new_qty = current + total_qty;

        if let Err(msg) = validate_deck_addition(&conn, user_id, &effective, new_qty)? {
            rejected.push(msg);
            continue;
        }

        let conflict = json_error(
            StatusCode::CONFLICT,
            "The deck was checked out before all selected cards could be added.",
        );
        if equivalent.is_some() {
            // Collapse legacy alt-printing rows of this game card into the
            // one row that carries the new quantity (then drop the zeros).
            let changes = conn.execute(
                &format!(
                    "UPDATE deck_cards
                     SET quantity = CASE WHEN card_id = ?1 THEN ?2 ELSE 0 END
                     WHERE deck_id = ?3
                       AND EXISTS (
                         SELECT 1 FROM decks guarded_deck
                         WHERE guarded_deck.id = deck_cards.deck_id
                           AND guarded_deck.user_id = ?4
                           AND guarded_deck.checked_out = 0
                       )
                       AND card_id IN (
                         SELECT existing_cc.id
                         FROM card_cache existing_cc
                         JOIN card_cache target_cc ON target_cc.id = ?5
                         WHERE {ek} = {tk}
                       )"
                ),
                rusqlite::params_from_iter(vec![
                    json_to_sql_value(&effective),
                    json_to_sql(Some(&json_num(new_qty)))?,
                    SqlValue::Integer(deck_id),
                    SqlValue::Integer(user_id),
                    json_to_sql_value(&effective),
                ]),
            )?;
            if changes == 0 {
                return Ok(conflict);
            }
            conn.execute(
                "DELETE FROM deck_cards WHERE deck_id = ? AND quantity <= 0",
                rusqlite::params_from_iter(vec![SqlValue::Integer(deck_id)]),
            )?;
        } else {
            let changes = conn.execute(
                "INSERT INTO deck_cards (deck_id, card_id, quantity)
                 SELECT ?1, ?2, ?3
                 WHERE EXISTS (
                   SELECT 1 FROM decks guarded_deck
                   WHERE guarded_deck.id = ?4
                     AND guarded_deck.user_id = ?5
                     AND guarded_deck.checked_out = 0
                 )",
                rusqlite::params_from_iter(vec![
                    SqlValue::Integer(deck_id),
                    json_to_sql_value(&effective),
                    json_to_sql(Some(&json_num(new_qty)))?,
                    SqlValue::Integer(deck_id),
                    SqlValue::Integer(user_id),
                ]),
            )?;
            if changes == 0 {
                return Ok(conflict);
            }
        }
        added += total_qty;
    }

    let added_display = js_to_string(Some(&json_num(added)));
    let msg = if !rejected.is_empty() {
        if added > 0.0 {
            format!("Added {added_display} card(s). {}", rejected[0])
        } else {
            rejected[0].clone()
        }
    } else {
        format!("Added {added_display} card(s) to deck")
    };
    Ok(json_response(
        StatusCode::OK,
        &json!({ "message": msg, "affected": json_num(added), "rejected": rejected.len() }),
    ))
}

// ---------------------------------------------------------------------------
// POST /collection and POST /collection/bulk-add (collection_adds group)
// ---------------------------------------------------------------------------
//
// Native port of routes/collection.js `addCardToCollection` (+ its two
// routes) and `bulkAddToCollection`. The Scryfall-touching legs — the
// `cardApi.getCardById` cache-miss fetch and the `getPrintingInLang`
// network fetch behind `printingInLanguage` — are never attempted here:
// whenever the plan would need them (card or localized printing absent
// from card_cache), the whole request forwards to Node untouched, exactly
// like the PUT card_id branch above. Scryfall is lane B's scryfallApi
// territory; this module stays read-DB-only + INSERT.
//
// Two documented deviations (both only reachable on DB failure, which
// Node's separate autocommit statements would half-apply anyway):
//   - inserts + recordPrice share one BEGIN IMMEDIATE transaction, so a
//     mid-add DB error rolls the whole add back instead of leaving rows;
//   - bulk-add plans every entry (validation + cache probes) before any
//     write, so a Forward decision cannot strand already-inserted cards.

/// JS falsiness of a body value: `!card_id` — a missing key (undefined),
/// null, false, 0/NaN and "" are falsy; every array/object is truthy.
fn js_falsy(v: Option<&Value>) -> bool {
    match v {
        None => true,
        Some(v) => !truthy(v),
    }
}

/// Input for `languages.toName`/`resolve` from a JSON/sqlite value: JS
/// String()-coerces inside resolve(), and null/undefined take the falsy
/// DEFAULT (English) path, modelled as None.
fn lang_input(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(js_to_string(Some(other))),
    }
}

/// Read-only phase of one `addCardToCollection` call: everything up to (but
/// not including) the INSERTs, decided from the merged body. `Reject` is the
/// only caller-visible AddCardError the native path can reach (both throws
/// in addCardToCollection are 400; the 404 requires the Scryfall fetch,
/// which is a `Forward` here).
enum AddPlan {
    Ready {
        card_id: SqlValue,
        count: i64,
        stack: bool,
        condition: Value,
        printing: Value,
        language: Value,
        purchase_price: Value,
        is_trade: bool,
        price: Value,
    },
    Reject(String),
    Forward,
}

/// Pure-read twin of addCardToCollection's pre-insert block (hydration is a
/// no-op since v3). Node-strict order: card_id → printing → cache lookup →
/// printingInLanguage → quantity/stack/pricing defaults.
fn plan_add(conn: &Connection, merged: &Map<String, Value>) -> Result<AddPlan, WriteError> {
    let card_id_v = merged.get("card_id");
    if js_falsy(card_id_v) {
        return Ok(AddPlan::Reject("card_id is required".into()));
    }
    let printing_default = Value::String("Normal".into());
    let printing = merged.get("printing").unwrap_or(&printing_default);
    // Array.prototype.includes is strict ===: only exact strings match.
    if !PRINTING_VALUES.iter().any(|p| Some(*p) == printing.as_str()) {
        return Ok(AddPlan::Reject(format!(
            "Invalid printing: {}",
            js_to_string(Some(printing))
        )));
    }

    let mut row = match get_row_map(
        conn,
        "SELECT * FROM card_cache WHERE id = ?1",
        &[json_to_sql(card_id_v)?],
    )? {
        Some(r) => r,
        // Node would cardApi.getCardById() → Scryfall. Not Rust's to do.
        None => return Ok(AddPlan::Forward),
    };

    let language_default = Value::String("English".into());
    let language = merged.get("language").unwrap_or(&language_default);
    let mut card_id_bind = json_to_sql(card_id_v)?;

    // cardApi.printingInLanguage(card, language) — null keeps the picked row.
    let card_lang = row.get("language").cloned().unwrap_or(Value::Null);
    if crate::search::to_name(lang_input(Some(&card_lang)).as_deref())
        != crate::search::to_name(lang_input(Some(language)).as_deref())
    {
        let (_code, scry) = crate::search::lang_codes(lang_input(Some(language)).as_deref());
        // getPrintingInLang early-outs on scryfall code 'en', empty set or
        // empty number (all keep the picked row without touching the DB).
        if scry != "en" {
            let set_raw = match row.get("set_id") {
                Some(v) if truthy(v) => js_to_string(Some(v)),
                _ => String::new(),
            };
            let set = set_raw
                .strip_prefix("mtg-")
                .unwrap_or(&set_raw)
                .to_lowercase();
            let number = row.get("number").cloned();
            if !set.is_empty() && !js_falsy(number.as_ref()) {
                match get_row_map(
                    conn,
                    "SELECT * FROM card_cache WHERE set_id = ?1 AND number = ?2 AND language = ?3 LIMIT 1",
                    &[
                        SqlValue::Text(set),
                        json_to_sql(number.as_ref())?,
                        SqlValue::Text(crate::search::to_name(Some(&scry))),
                    ],
                )? {
                    Some(loc) => {
                        // Node: card = localized; cardId = localized.id.
                        card_id_bind = match loc.get("id") {
                            Some(v) => json_to_sql_value(v),
                            None => SqlValue::Null,
                        };
                        row = loc;
                    }
                    // Network fetch + cacheCards — Node's job.
                    None => return Ok(AddPlan::Forward),
                }
            }
        }
    }

    // count = Math.max(1, parseInt(quantity, 10) || 1) — undefined/NaN/0
    // all land on 1.
    let count = match merged.get("quantity") {
        None => 1,
        Some(v) => js_parse_int_v(Some(v)).unwrap_or(1).max(1),
    };
    let stack = merged.get("stackable").map(truthy).unwrap_or(false);
    let purchase_price = match merged.get("purchase_price") {
        Some(v) if truthy(v) => v.clone(),
        _ => Value::from(0),
    };
    Ok(AddPlan::Ready {
        card_id: card_id_bind,
        count,
        stack,
        condition: merged
            .get("condition")
            .cloned()
            .unwrap_or_else(|| Value::String("Near Mint".into())),
        printing: printing.clone(),
        language: language.clone(),
        purchase_price,
        is_trade: tr(merged.get("is_trade")),
        price: row.get("price_trend").cloned().unwrap_or(Value::Null),
    })
}

/// priceHelpers.recordPrice — the movement-guarded INSERT (not the brief's
/// older date-keyed OR-REPLACE; the source is canonical). Returns whether a
/// row landed.
fn record_price(conn: &Connection, card_id: &SqlValue, price: &Value) -> Result<bool, WriteError> {
    let p = js_to_number(Some(price));
    let card_empty = match card_id {
        SqlValue::Null => true,
        SqlValue::Text(s) => s.is_empty(),
        _ => false,
    };
    if card_empty || !(p > 0.0) {
        return Ok(false);
    }
    let changed = conn.execute(
        "WITH latest(price, recorded_at) AS (
           SELECT price, recorded_at
           FROM price_history
           WHERE card_id = ?1
           ORDER BY recorded_at DESC, rowid DESC
           LIMIT 1
         ), candidate(ts) AS (SELECT strftime('%Y-%m-%d %H:%M:%f', 'now'))
         INSERT INTO price_history (card_id, price, recorded_at)
         SELECT ?1, ?2, CASE
           WHEN (SELECT recorded_at FROM latest) IS NULL
             OR (SELECT recorded_at FROM latest) < candidate.ts THEN candidate.ts
           ELSE strftime('%Y-%m-%d %H:%M:%f', (SELECT recorded_at FROM latest), '+0.001 seconds')
         END
         FROM candidate
         WHERE NOT EXISTS (SELECT 1 FROM latest WHERE price = ?2)",
        rusqlite::params![card_id, p],
    )?;
    Ok(changed == 1)
}

/// Writes the planned copies (stack = one qty-N row; else one qty-1 row per
/// copy, matching Node's sequential INSERTs) and records the price, in one
/// BEGIN IMMEDIATE transaction so the revision triggers fire on commit like
/// every other native write group.
fn execute_add(conn: &Connection, user_id: i64, plan: &AddPlan) -> Result<(i64, Vec<i64>), WriteError> {
    let AddPlan::Ready { card_id, count, stack, condition, printing, language, purchase_price, is_trade, price } = plan else {
        return Ok((0, Vec::new()));
    };
    let mut tx = Tx::begin(conn)?;
    let mut ids: Vec<i64> = Vec::new();
    let quantities: Vec<i64> = if *stack { vec![*count] } else { vec![1; *count as usize] };
    let mut last = 0i64;
    for q in quantities {
        conn.execute(
            "INSERT INTO collection (
               card_id, user_id, quantity, condition, printing, language, purchase_price,
               is_trade
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                card_id,
                user_id,
                q,
                json_to_sql_value(condition),
                json_to_sql_value(printing),
                json_to_sql_value(language),
                json_to_sql_value(purchase_price),
                if *is_trade { 1 } else { 0 },
            ],
        )?;
        last = conn.last_insert_rowid();
        ids.push(last);
    }
    record_price(conn, card_id, price)?;
    tx.commit()?;
    Ok((last, ids))
}

/// `POST /collection` — routes/collection.js route 3.
pub async fn handle_add(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st), req).await;
    };
    let (parts, bytes) = read_parts(req).await;
    let headers = parts.headers.clone();
    let body = match parse_body(&headers, &bytes) {
        BodyOutcome::Parsed(b) => b,
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::TooLarge => return too_large(),
    };
    let token = bearer_token(&headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    // express.json hands a non-object body to destructuring as values that
    // never carry card_id; `{}` models both cases (parse_body already maps
    // arrays there).
    let empty = Map::new();
    let m = body.as_object().unwrap_or(&empty);
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error=?e, "add card pool failure");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to add card");
        }
    };
    let plan = match plan_add(&conn, m) {
        Ok(AddPlan::Reject(msg)) => return json_error(StatusCode::BAD_REQUEST, &msg),
        Ok(AddPlan::Forward) => return crate::proxy(State(st), rebuild(parts, &bytes)).await,
        Ok(p @ AddPlan::Ready { .. }) => p,
        Err(e) => {
            tracing::error!(error=?e, "add card failed");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to add card");
        }
    };
    let resp = match execute_add(&conn, user.id, &plan) {
        Ok((last, ids)) => json_response(
            StatusCode::OK,
            &json!({ "message": "Card added to collection", "id": last, "ids": ids }),
        ),
        Err(e) => {
            tracing::error!(error=?e, "add card failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to add card")
        }
    };
    if resp.status().is_success() {
        stats::on_proxied_write(&st, &Method::POST, "/api/collection", &headers);
    }
    resp
}

/// One pass-1 outcome per bulk-add entry. `Failed` carries the per-entry
/// report body; `Forward` (any of them) forwards the WHOLE request, since
/// Node must author the Scryfall fetches (and their side effects) itself.
enum BulkEntry {
    Ready { plan: AddPlan, card_id: Option<Value>, index: usize },
    Failed { error: String, card_id: Option<Value>, index: usize },
    Forward,
}

/// routes/collection.js bulkAddToCollection + its route (BULK_ADD_MAX=250).
pub async fn handle_bulk_add(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st), req).await;
    };
    let (parts, bytes) = read_parts(req).await;
    let headers = parts.headers.clone();
    let body = match parse_body(&headers, &bytes) {
        BodyOutcome::Parsed(b) => b,
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::TooLarge => return too_large(),
    };
    let token = bearer_token(&headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    // `const { card_ids = [], ...shared } = req.body` — null/non-array and
    // a missing key both take the falsy 400 branch.
    let body_map = body.as_object().cloned();
    let card_ids: Vec<Value> = match body_map.as_ref().and_then(|m| m.get("card_ids")) {
        Some(Value::Array(a)) => a.clone(),
        _ => return json_error(StatusCode::BAD_REQUEST, "card_ids is required"),
    };
    if card_ids.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "card_ids is required");
    }
    if card_ids.len() > 250 {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Cannot add more than 250 cards at once.",
        );
    }
    let mut shared = body_map.unwrap_or_default();
    shared.remove("card_ids");

    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error=?e, "bulk add pool failure");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
        }
    };

    // Pass 1 — pure reads: the {...shared, ...perCard, card_id} merge and
    // every validation/probe, before any write can commit.
    let mut outcomes: Vec<BulkEntry> = Vec::with_capacity(card_ids.len());
    for (index, entry) in card_ids.iter().enumerate() {
        // `entry && typeof entry === 'object'` — null is NOT keyed; arrays
        // are (and carry no card_id, like any other object).
        let keyed = matches!(entry, Value::Object(_) | Value::Array(_));
        let mut merged = shared.clone();
        let card_id: Option<Value> = if keyed {
            let obj = entry.as_object();
            let cid = obj.and_then(|o| o.get("card_id")).cloned();
            if let Some(o) = obj {
                for k in ["quantity", "purchase_price", "condition", "printing"] {
                    if let Some(v) = o.get(k) {
                        // perCard only overrides when the value is neither
                        // null nor stringifies to ''.
                        if !v.is_null() && js_to_string(Some(v)) != "" {
                            merged.insert(k.into(), v.clone());
                        }
                    }
                }
            }
            cid
        } else {
            Some(entry.clone())
        };
        if let Some(v) = card_id.clone() {
            merged.insert("card_id".into(), v);
        }
        outcomes.push(match plan_add(&conn, &merged) {
            Ok(p @ AddPlan::Ready { .. }) => BulkEntry::Ready { plan: p, card_id, index },
            Ok(AddPlan::Reject(msg)) => BulkEntry::Failed { error: msg, card_id, index },
            Ok(AddPlan::Forward) => BulkEntry::Forward,
            // Non-AddCardError: Node console.errors and reports the generic
            // per-card message.
            Err(e) => {
                tracing::error!(error=?e, "bulk add card failed");
                BulkEntry::Failed { error: "Failed to add card".into(), card_id, index }
            }
        });
    }
    if outcomes.iter().any(|o| matches!(o, BulkEntry::Forward)) {
        return crate::proxy(State(st), rebuild(parts, &bytes)).await;
    }

    // Pass 2 — sequential writes in request order, per-entry transactions
    // (a throwing entry costs Node exactly that one entry: its INSERT
    // autocommits never ran; Rust rolls back only that entry's tx).
    let mut added: Vec<Value> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    let fail_obj = |card_id: &Option<Value>, error: &str, index: usize| {
        let mut m = Map::new();
        if let Some(v) = card_id {
            m.insert("card_id".into(), v.clone());
        }
        m.insert("error".into(), json!(error));
        m.insert("index".into(), json!(index));
        Value::Object(m)
    };
    for outcome in outcomes {
        match outcome {
            BulkEntry::Ready { plan, card_id, index } => match execute_add(&conn, user.id, &plan) {
                Ok((id, ids)) => {
                    let mut m = Map::new();
                    if let Some(v) = card_id {
                        m.insert("card_id".into(), v.clone());
                    }
                    m.insert("id".into(), json!(id));
                    m.insert("ids".into(), json!(ids));
                    m.insert("index".into(), json!(index));
                    added.push(Value::Object(m));
                }
                Err(e) => {
                    tracing::error!(error=?e, "bulk add card failed");
                    failed.push(fail_obj(&card_id, "Failed to add card", index));
                }
            },
            BulkEntry::Failed { error, card_id, index } => {
                failed.push(fail_obj(&card_id, &error, index));
            }
            BulkEntry::Forward => unreachable!("handled by the pre-write forward"),
        }
    }

    let qty = match shared.get("quantity") {
        Some(v) => js_parse_int_v(Some(v)).unwrap_or(1).max(1),
        None => 1,
    };
    let status = if !failed.is_empty() && added.is_empty() {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    };
    let message = if !failed.is_empty() {
        format!(
            "Added {} of {} cards; {} failed.",
            added.len(),
            card_ids.len(),
            failed.len()
        )
    } else {
        format!(
            "Added {} card{}{} to collection.",
            added.len(),
            if added.len() == 1 { "" } else { "s" },
            if qty > 1 { format!(" (x{qty} each)") } else { String::new() }
        )
    };
    let mut out = Map::new();
    out.insert("message".into(), json!(message));
    out.insert("added".into(), json!(added.len()));
    out.insert("entries".into(), Value::Array(added));
    out.insert("failed".into(), Value::Array(failed));
    let resp = json_response(status, &Value::Object(out));
    if resp.status().is_success() {
        stats::on_proxied_write(&st, &Method::POST, "/api/collection/bulk-add", &headers);
    }
    resp
}

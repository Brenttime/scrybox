//! Lane B unit 4 (iter-12): native port of `backend/src/routes/admin.js`
//! `POST /seed-cards` — the admin dev-seed route, behind
//! `RUST_ROUTES=seed_cards`. Everything else on the path (non-POST, kill
//! switch off, body-parser strict failures) forwards to Node untouched.
//!
//! Flow (admin.js:19-119, pinned line-by-line):
//!   1. MOCK_POOL = concat(getCardsBySet('lea'), getCardsBySet('mh3')),
//!      each a single fetchFromScryfall(`set:<code>`) + normalizeCard +
//!      cacheCards; failures yield [] (getCardsBySet's own catch), with a
//!      500 ms pause after each set (Scryfall queue courtesy, admin.js:26).
//!   2. Empty pool → fallback `SELECT * FROM card_cache LIMIT 500` through
//!      parseCardRow; still empty → 502.
//!   3. withAllocationLock + withDedicatedTransaction (BEGIN IMMEDIATE):
//!      logicalInventoryStatus guard over every seed-set stack the user
//!      owns (409 ALLOCATION_CONFLICT message), bulk DELETE of the user's
//!      seed-set rows, then 168 + 40 random INSERTs.
//!   4. 200 {message: 'Successfully seeded a large test collection: N
//!      cards for admin user.'} where N = Σ qty (∈ [208, 416], random).
//!
//! Randomness: Node draws Math.random() per field. Byte-identity across
//! runtimes is impossible by design (like the share-token writers); the
//! parity case pins the message SHAPE, the row count (208), the allowed
//! value domains and the price ranges instead. Our draws are splitmix64 —
//! seeded from the clock, independent stream from Node, same distribution.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Map, Value};

use crate::auth::{authenticate, bearer_token};
use crate::card_cache::cache_normalized_cards;
use crate::collection_writes::{
    alloc_lock, get_rows_map, json_error, json_response, logical_inventory_status, node_exact, Tx,
};
use crate::decks_reads::{parse_card_row, sql_card_key};
use crate::scryfall::ScryOpts;
use crate::search::to_name;
use crate::sidecar::{encode_uri_component, normalize_card};
use crate::AppState;

/// admin.js:22 `MTG_SEED_SETS`.
const MTG_SEED_SETS: &[&str] = &["lea", "mh3"];
/// admin.js:73 `conditions` (four, no Damaged).
const CONDITIONS: [&str; 4] = ["Near Mint", "Lightly Played", "Moderately Played", "Heavily Played"];
/// admin.js:74 `languages` — three English slots, one Japanese.
const LANGUAGES: [&str; 4] = ["English", "English", "English", "Japanese"];

// ---------------------------------------------------------------------------
// RNG (Math.random() twin: [0,1) uniform; draws below use floor(rand*n))
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new() -> Self {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mut r = Rng(t ^ ((std::process::id() as u64) << 32));
        r.next_u64();
        r
    }
    fn next_u64(&mut self) -> u64 {
        // splitmix64 finalizer over a golden-ratio counter.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Math.random(): 53-bit [0,1).
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
    }
    /// index = Math.floor(Math.random() * n).
    fn below(&mut self, n: usize) -> usize {
        ((self.next_f64() * n as f64).floor() as usize).min(n - 1)
    }
}

/// `parseFloat((Math.random() * maxPrice).toFixed(2))` — ECMAScript
/// toFixed(2): nearest hundredth, exact ties toward the LARGER n; the
/// parseFloat round-trip then yields the closest double to that literal,
/// which f64 division reproduces for these magnitudes.
fn to_fixed_2_parse(x: f64) -> f64 {
    if !x.is_finite() {
        return f64::NAN;
    }
    let mut n = (x * 100.0).floor();
    let d_low = x - n / 100.0;
    let d_high = (n + 1.0) / 100.0 - x;
    if d_high <= d_low {
        n += 1.0;
    }
    n / 100.0
}

// ---------------------------------------------------------------------------
// getCardsBySet / fetchFromScryfall (scryfallApi.js twins, seed call shape)
// ---------------------------------------------------------------------------

/// `fetchFromScryfall(q)` with lang undefined: langSearch resolves the
/// DEFAULT language → scryfall 'en' → NO lang param, the query goes through
/// verbatim. `scryGet` (plain GET, not retried): 429 retries here are the
/// caller's own 3-attempt loop (scryfallApi.js:485-502); anything else
/// throws out.
async fn fetch_from_scryfall(st: &AppState, q: &str) -> Result<Vec<Value>, crate::scryfall::ScryError> {
    let url = format!("/cards/search?q={}", encode_uri_component(q));
    for i in 0..3 {
        match st.scry.get(&url, &ScryOpts::default()).await {
            // (resp.data && resp.data.data) || []
            Ok(v) => {
                return Ok(v
                    .as_ref()
                    .and_then(|d| d.get("data"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default());
            }
            Err(e) => {
                if e.is_status(429) && i < 2 {
                    continue;
                }
                return Err(e);
            }
        }
    }
    unreachable!("the loop returns on every non-continue path")
}

/// `getCardsBySet(setCode)` (scryfallApi.js:887): one search page, all
/// cards normalized with lang undefined (so each row's language rides
/// raw.lang, English fallback = toName(undefined)) and cached. Any failure
/// — transport, HTTP, cacheCards itself (its throw lands in this catch) —
/// answers [].
async fn get_cards_by_set(st: &AppState, set_code: &str) -> Vec<Map<String, Value>> {
    tracing::info!("Querying Scryfall for full set: {set_code}");
    let raw = match fetch_from_scryfall(st, &format!("set:{set_code}")).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Error fetching MTG set {set_code} from Scryfall: {}", e.message);
            return Vec::new();
        }
    };
    // normalizeCard(c) with lang undefined → the None branch uses the
    // display name directly, so pass toName(undefined) = DEFAULT name.
    let lang_name = to_name(None);
    let cards: Vec<Map<String, Value>> = raw.iter().map(|c| normalize_card(c, &lang_name)).collect();
    if !cards.is_empty() {
        let cached = st
            .db
            .as_ref()
            .and_then(|p| p.get().ok())
            .and_then(|conn| cache_normalized_cards(&conn, &cards).map(|_| ()).ok());
        if cached.is_none() {
            // cacheCards threw → Node's catch → [].
            tracing::error!("Error fetching MTG set {set_code} from Scryfall: cacheCards failed");
            return Vec::new();
        }
    }
    cards
}

// ---------------------------------------------------------------------------
// Route handler
// ---------------------------------------------------------------------------

pub async fn handle(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("seed_cards") || req.method() != Method::POST {
        // Kill switch off, or a method Node's router.post does not own —
        // Node (and its 404/global-error chain) stays canonical.
        return crate::proxy(State(st), req).await;
    }

    // server.js chain: normalJson (express.json strict, 1 mb) runs BEFORE
    // the admin auth gate. The seed route never reads the body, but a
    // strict-parse/oversize failure is Node's page to render → Forward.
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return crate::proxy(State(st), Request::from_parts(parts, Body::empty())).await,
    };
    if crate::cardscan::parse_outer_json(&parts.headers, &bytes).is_err() {
        return crate::proxy(
            State(st),
            Request::from_parts(parts, Body::from(bytes.to_vec())),
        )
        .await;
    }
    let req = Request::from_parts(parts, Body::from(bytes.to_vec()));

    // router.use(authenticateToken, requireAdmin) — 401/Invalid come from
    // auth.rs (byte-pinned). An API key on a POST never reaches the admin
    // checks: authenticateToken itself answers 403 'API keys are read-only'
    // (middleware/auth.js:30-33 → auth.rs is_get gate), so pass is_get=false.
    let pool = st.db.clone().unwrap();
    let headers = req.headers().clone();
    let token = bearer_token(&headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), false) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    if user.role.as_deref() != Some("admin") {
        return json_error(
            StatusCode::FORBIDDEN,
            "Access denied. Administrator privileges required.",
        );
    }

    match seed(&st, &pool, user.id).await {
        Ok(res) => res,
        Err(SeedError::Conflict) => json_error(
            StatusCode::CONFLICT,
            "Check in decks using these cards before replacing the seed collection.",
        ),
        Err(SeedError::EmptyPool) => json_error(
            StatusCode::BAD_GATEWAY,
            "Could not fetch seed card data from the card APIs, and local cache is empty. Try again shortly.",
        ),
        Err(SeedError::Failed(e)) => {
            tracing::error!("SEEDING ERROR: {e:?}");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to seed test cards")
        }
    }
}

enum SeedError {
    /// admin.js ALLOCATION_CONFLICT → 409 {error: message}.
    Conflict,
    /// API + cache both empty → 502 (admin.js:40).
    EmptyPool,
    /// Everything else lands in admin.js's catch → 500.
    Failed(String),
}

impl From<rusqlite::Error> for SeedError {
    fn from(e: rusqlite::Error) -> Self {
        SeedError::Failed(format!("sql: {e}"))
    }
}

impl From<crate::collection_writes::WriteError> for SeedError {
    fn from(e: crate::collection_writes::WriteError) -> Self {
        SeedError::Failed(format!("sql: {e:?}"))
    }
}

async fn seed(st: &AppState, pool: &crate::db::DbPool, user_id: i64) -> Result<Response, SeedError> {
    // 1. Build MOCK_POOL: the two seed sets, 500 ms apart (admin.js:23-30).
    let mut mock_pool: Vec<Map<String, Value>> = Vec::new();
    for set_code in MTG_SEED_SETS {
        mock_pool.extend(get_cards_by_set(st, set_code).await);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // 2. Fallback: whatever the cache already knows (admin.js:31-42).
    let conn = pool.get().map_err(|e| SeedError::Failed(format!("pool: {e}")))?;
    if mock_pool.is_empty() {
        let cached = get_rows_map(&conn, "SELECT * FROM card_cache LIMIT 500", &[])?;
        if cached.is_empty() {
            return Err(SeedError::EmptyPool);
        }
        for r in &cached {
            // parseCardRow's JSON.parse throws on corrupt arrays → outer
            // catch → 500.
            mock_pool.push(parse_card_row(r).map_err(|_| SeedError::Failed("parseCardRow".into()))?);
        }
    }

    // 3. seedSetIds = distinct set_id, first-seen order (JS Set).
    let mut seed_set_ids: Vec<Option<String>> = Vec::new();
    for c in &mock_pool {
        let id = match c.get("set_id") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Null) | None => None,
            Some(other) => Some(other.to_string()),
        };
        if !seed_set_ids.contains(&id) {
            seed_set_ids.push(id);
        }
    }
    let ph = seed_set_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let mut set_params: Vec<SqlValue> = seed_set_ids
        .iter()
        .map(|s| match s {
            Some(s) => SqlValue::Text(s.clone()),
            None => SqlValue::Null,
        })
        .collect();

    // 4. withAllocationLock + withDedicatedTransaction (BEGIN IMMEDIATE).
    let _guard = alloc_lock().lock().await;
    let mut tx = Tx::begin(&conn).map_err(|e| SeedError::Failed(format!("{e:?}")))?;

    let mut removal_params: Vec<SqlValue> = vec![SqlValue::Integer(user_id)];
    removal_params.extend(set_params.iter().cloned());
    let removals = get_rows_map(
        &conn,
        &format!(
            "SELECT MIN(c.card_id) AS card_id, SUM(c.quantity) AS remove_qty
             FROM collection c
             JOIN card_cache removed_cc ON removed_cc.id = c.card_id
             WHERE c.user_id = ? AND c.quantity > 0
               AND removed_cc.set_id IN ({ph})
             GROUP BY {}",
            sql_card_key("removed_cc")
        ),
        &removal_params,
    )?;
    for removal in &removals {
        let status = logical_inventory_status(
            &conn,
            user_id,
            removal.get("card_id").unwrap_or(&Value::Null),
        )?;
        let conflict = match &status {
            None => true,
            Some(s) => {
                js_number_of(s.get("owned_qty")) - js_number_of(removal.get("remove_qty"))
                    < js_number_of(s.get("locked_qty"))
            }
        };
        if conflict {
            tx.rollback();
            return Err(SeedError::Conflict);
        }
    }

    let mut delete_params: Vec<SqlValue> = vec![SqlValue::Integer(user_id)];
    delete_params.extend(set_params.drain(..));
    conn.execute(
        &format!(
            "DELETE FROM collection WHERE user_id = ? AND card_id IN (
               SELECT id FROM card_cache WHERE set_id IN ({ph})
             )"
        ),
        rusqlite::params_from_iter(delete_params.iter()),
    )?;

    // 5. 168 assorted cards up to $10, then 40 cheap fillers up to $5.
    let mut rng = Rng::new();
    let mut added_count: i64 = 0;
    let n = mock_pool.len();
    for max_price in std::iter::repeat(10.0).take(168).chain(std::iter::repeat(5.0).take(40)) {
        let card = &mock_pool[rng.below(n)];
        // printsForCard(card): 'Normal' if price_normal>0, 'Holofoil' if
        // price_holofoil>0 (push order matters — randomEntry indexes it),
        // ['Normal'] when neither is priced.
        let mut prints: Vec<&'static str> = Vec::with_capacity(2);
        if js_number_of(card.get("price_normal")) > 0.0 {
            prints.push("Normal");
        }
        if js_number_of(card.get("price_holofoil")) > 0.0 {
            prints.push("Holofoil");
        }
        if prints.is_empty() {
            prints.push("Normal");
        }
        let print = prints[rng.below(prints.len())];
        let qty = rng.below(2) as i64 + 1;
        let purchase_price = to_fixed_2_parse(rng.next_f64() * max_price);
        let card_id = match card.get("id") {
            Some(Value::String(s)) => SqlValue::Text(s.clone()),
            Some(Value::Null) | None => SqlValue::Null,
            Some(other) => SqlValue::Text(other.to_string()),
        };
        conn.execute(
            "INSERT INTO collection (card_id, quantity, condition, printing, language, purchase_price, user_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                card_id,
                qty,
                CONDITIONS[rng.below(4)],
                print,
                LANGUAGES[rng.below(4)],
                purchase_price,
                user_id,
            ],
        )?;
        added_count += qty;
    }

    tx.commit().map_err(|e| SeedError::Failed(format!("{e:?}")))?;
    Ok(json_response(
        StatusCode::OK,
        &json!({ "message": format!("Successfully seeded a large test collection: {added_count} cards for admin user.") }),
    ))
}

/// `Number(x)` for the SQL aggregates this route compares (all REAL/INTEGER
/// or NULL — better-sqlite3 never hands these ports `undefined`).
fn js_number_of(v: Option<&Value>) -> f64 {
    match v {
        None | Some(Value::Null) => 0.0,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::Bool(true)) => 1.0,
        Some(Value::Bool(false)) => 0.0,
        Some(Value::String(s)) => {
            if s.trim().is_empty() {
                0.0 // Number('') === 0 (parseFloat would say NaN)
            } else {
                crate::sidecar::js_parse_float(s).unwrap_or(f64::NAN)
            }
        }
        Some(_) => f64::NAN,
    }
}

// ---------------------------------------------------------------------------
// Unit tests (pure helpers; the route suite lives in tests/seed_cards_route.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_fixed_2_parse_matches_js_tofixed() {
        // Expected values emitted by node: parseFloat((x).toFixed(2)).
        for (x, want) in [
            (0.0f64, 0.0f64),
            (0.004, 0.0),
            (0.005, 0.01),
            (0.015, 0.01), // double(0.015) is BELOW 0.015 → nearest is 0.01
            (1.005, 1.0),   // binary 1.00499999999999989 → nearest is 1.00
            (2.675, 2.67),  // the classic: 2.67499999999999982 → 2.67
            (3.14159, 3.14),
            (9.999, 10.0),
            (9.994, 9.99),
            (9.995, 9.99),  // double(9.995) below exact → 9.99, not 10.0
            (4.999999999999, 5.0),
        ] {
            assert_eq!(to_fixed_2_parse(x), want, "toFixed({x})");
        }
    }

    #[test]
    fn rng_below_stays_in_domain() {
        let mut r = Rng::new();
        for _ in 0..10_000 {
            let i = r.below(4);
            assert!(i < 4);
            assert!((0.0..1.0).contains(&r.next_f64()));
        }
    }
}

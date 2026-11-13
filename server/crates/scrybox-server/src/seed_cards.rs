//! Port of POST /api/admin/seed-cards — `backend/src/routes/admin.js:19`
//! (Lane A iter-49, RUST_ROUTES=`seed_cards`; the last Lane-A route).
//!
//! Middleware order mirrored from server.js + admin.js:17 (probe-pinned the
//! same way as the admin_users port, iter-40): the GLOBAL normalJson
//! (server.js:205-211, 1 MiB, strict) runs ABOVE authenticateToken — a
//! malformed-JSON POST answers the global handler's 500 and an oversize one
//! its 413 with no token at all; then authenticateToken (POST kills the
//! api-key leg via is_get=false); then the router-level requireAdmin twin
//! (`settings::admin_gate`); then the handler. Node's statsCache
//! .invalidateOnWrite sits on the bare '/api' mount BELOW server.js's
//! '/api/admin' mount — it never sees admin-surface writes; the
//! on_proxied_write call inside authorize is Rust-side cache hygiene only.
//!
//! Handler twin (admin.js:19-119):
//! - MTG_SEED_SETS = ['lea','mh3'], each via getCardsBySet
//!   (scryfallApi.js:887): fetchFromScryfall(`set:${code}`) — ONE
//!   /cards/search page (pagination deliberately skipped), 3 attempts, only
//!   429 re-throws the loop, any other throw breaks it; getCardsBySet
//!   swallows the throw (logs, returns []). normalizeCard(raw) (no lang →
//!   languages.resolve(undefined) → English) then cacheCards when non-empty;
//!   a cacheCards throw is swallowed by the SAME catch → the set yields [].
//!   A 500 ms pause follows every set call (inside Node's try; the call
//!   never rejects, so it always runs).
//! - Empty pool fallback: SELECT * FROM card_cache LIMIT 500 through the
//!   shared db client (connection failures throw past getCardsBySet's catch
//!   into the route catch → 500, matching Node); rows go through parseCardRow
//!   (a throw is a route-catch 500); zero rows → 502 verbatim message.
//! - seedSetIds: first-appearance-dedupe of MOCK_POOL.map(c => c.set_id)
//!   (JS Set preserves insertion order).
//! - withAllocationLock (collectionHelpers.js:14 — in-process FIFO; Rust
//!   additionally takes a real BEGIN IMMEDIATE like every other writer port)
//!   around db.withDedicatedTransaction: removals GROUP BY sqlCardKey →
//!   logicalInventoryStatus guard per group → ALLOCATION_CONFLICT aborts the
//!   whole transaction (409, verbatim message); DELETE by set membership;
//!   168 inserts priced ≤ $10 + 40 ≤ $5. addedCount sums qty, NEVER row
//!   count — qty is 1|2 so the response count is random by design (parity
//!   scrubs the number and asserts it equals SUM(quantity) via read-back).
//! - Math.random legs: floor(rand*len) picks; purchase_price =
//!   parseFloat((rand*max).toFixed(2)) — {:.2}+parse is exact (no double is
//!   an exact .005 tie, so JS toFixed's tie-to-larger and Rust's
//!   round-half-even agree on every reachable input).
//! - res.json {message} (ETag via json_response; POST never revalidates).
//! - Route catch: code ALLOCATION_CONFLICT → 409 {error: message};
//!   anything else → 500 {error: 'Failed to seed test cards'}.
//! - Non-POST methods: router.use(auth, admin) runs but no route matches, so
//!   Node's own chain answers (401/403 as gated, then the final-handler
//!   404 HTML) — Rust proxies them verbatim instead of re-gating.

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value};

use crate::admin_users::authorize;
use crate::auth;
use crate::card_cache::cache_normalized_cards;
use crate::card_norm::normalize_card;
use crate::collection_writes::{
    alloc_lock, internal_error, json_error, json_response, parse_body, read_parts, rebuild,
    too_large, BodyOutcome, Tx,
};
use crate::db::DbPool;
use crate::decks_reads::{parse_card_row, rows_map};
use crate::scryfall::ScryOpts;
use crate::AppState;

/// `/api/admin/seed-cards` — POST only; every other method rides the
/// catch-all proxy verbatim (Node's router.use auth + final-handler 404).
pub async fn route_seed_cards(State(st): State<AppState>, req: Request) -> Response {
    if *req.method() != Method::POST {
        return crate::proxy(State(st), req).await;
    }
    handle_post(State(st), req).await
}

fn seed_error() -> Response {
    tracing::error!("SEEDING ERROR");
    json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to seed test cards")
}

/// JS `x > 0` on the price columns (null/undefined → false; numeric strings
/// coerce; everything else NaN → false).
fn gt0(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Number(n)) => n.as_f64().map(|f| f > 0.0).unwrap_or(false),
        Some(Value::String(s)) => s.trim().parse::<f64>().map(|f| f > 0.0).unwrap_or(false),
        Some(Value::Bool(b)) => *b, // true → 1 > 0
        _ => false,
    }
}

/// JS `Number(v)` on SQL aggregates (always numeric here; null → 0).
fn num_of(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
        Some(Value::Bool(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        _ => 0.0,
    }
}

/// Uniform [0,1) from the OS RNG (53-bit — same granularity as Math.random).
fn js_random() -> f64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("getrandom: OS RNG unavailable");
    let v = u64::from_be_bytes(b) >> 11;
    (v as f64) / ((1u64 << 53) as f64)
}

/// `getCardsBySet(setCode)` — scryfallApi.js:887. Never Errs: every failure
/// (fetch throw past the 429-only retry, cacheCards throw) lands in Node's
/// catch which returns [] for that set.
async fn get_cards_by_set(st: &AppState, pool: &DbPool, url: &str, set_code: &str) -> Vec<Value> {
    // fetchFromScryfall(q, undefined, retries=3): only 429 continues the
    // loop; any other throw exits with the error (Node rethrows → catch).
    let mut body: Option<Value> = None;
    for i in 0..3u32 {
        match st.scry.get(url, &ScryOpts::default()).await {
            Ok(Some(v)) => {
                body = Some(v);
                break;
            }
            // Ok(None) = 2xx without a JSON object — axios hands back
            // `undefined` data and `(resp.data && resp.data.data) || []`
            // lands on []. Treat it as the empty answer, not a throw.
            Ok(None) => {
                body = Some(Value::Null);
                break;
            }
            Err(e) => {
                if e.is_status(429) && i + 1 < 3 {
                    continue;
                }
                tracing::error!("Error fetching MTG set {set_code} from Scryfall: {e}");
                return Vec::new();
            }
        }
    }
    let raws: Vec<Value> = body
        .as_ref()
        .and_then(|b| b.get("data"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let cards: Vec<Value> = raws.iter().map(|r| normalize_card(r, None)).collect();
    if !cards.is_empty() {
        // cacheCards (db.run on the shared client) — a throw lands in the
        // SAME catch as the fetch error, so the set yields [] even though
        // the cards were fetched (faithful: cards are lost).
        let maps: Vec<Map<String, Value>> = cards.iter().filter_map(Value::as_object).cloned().collect();
        match pool.get() {
            Ok(conn) => {
                if let Err(e) = cache_normalized_cards(&conn, &maps) {
                    tracing::error!("Error fetching MTG set {set_code} from Scryfall: {e}");
                    return Vec::new();
                }
            }
            Err(e) => {
                tracing::error!("Error fetching MTG set {set_code} from Scryfall: {e}");
                return Vec::new();
            }
        }
    }
    cards
}

async fn handle_post(State(st): State<AppState>, req: Request) -> Response {
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    // Global normalJson above auth: oversize → 413, malformed → generic 500,
    // both with NO token. The handler ignores the parsed body entirely
    // (arrays/objects included — Node never touches req.body here).
    let (parts, bytes) = read_parts(req).await;
    match parse_body(&parts.headers, &bytes) {
        BodyOutcome::TooLarge => return too_large(),
        BodyOutcome::Invalid => return internal_error(),
        BodyOutcome::Parsed(_) => {}
    }
    let headers = parts.headers.clone();
    let method = parts.method.clone();
    let _req2 = rebuild(parts, &bytes);
    let user: auth::AuthUser = match authorize(&st, &pool, &headers, &method, false, "/api/admin/seed-cards").await {
        Ok(u) => u,
        Err(res) => return res,
    };
    let user_id = user.id;

    // --- route try { } catch → 500 'Failed to seed test cards' ---
    let mut mock_pool: Vec<Value> = Vec::new();
    for set_code in ["lea", "mh3"] {
        let url = format!("/cards/search?q=set%3A{set_code}");
        mock_pool.extend(get_cards_by_set(&st, &pool, &url, set_code).await);
        // Scryfall strictly requires 50-100ms between requests.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let fallback_conn = pool.get().map_err(|e| {
        tracing::error!(error = %e, "seed_cards pool");
    });
    let conn = match fallback_conn {
        Ok(c) => c,
        Err(()) => return seed_error(),
    };

    if mock_pool.is_empty() {
        let cached = match rows_map(&conn, "SELECT * FROM card_cache LIMIT 500", &[]) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = ?e, "seed_cards cache fallback");
                return seed_error();
            }
        };
        if !cached.is_empty() {
            tracing::info!("Seed: APIs failed, falling back to {} locally cached cards.", cached.len());
            for r in &cached {
                match parse_card_row(r) {
                    Ok(m) => mock_pool.push(Value::Object(m)),
                    Err(()) => {
                        tracing::error!("seed_cards parseCardRow");
                        return seed_error();
                    }
                }
            }
        } else {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "Could not fetch seed card data from the card APIs, and local cache is empty. Try again shortly.",
            );
        }
    }

    // [...new Set(MOCK_POOL.map(c => c.set_id))] — first-appearance order.
    let mut seed_set_ids: Vec<String> = Vec::new();
    for c in &mock_pool {
        let sid = match c.get("set_id") {
            Some(Value::String(s)) => s.clone(),
            other => crate::collection_writes::js_to_string(other),
        };
        if !seed_set_ids.iter().any(|s| *s == sid) {
            seed_set_ids.push(sid);
        }
    }
    let placeholders = seed_set_ids
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let mut set_params: Vec<SqlValue> = seed_set_ids.iter().map(|s| SqlValue::Text(s.clone())).collect();
    set_params.insert(0, SqlValue::Integer(user_id));

    let _guard = alloc_lock().lock().await;
    let mut tx = match Tx::begin(&conn) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = ?e, "seed_cards begin");
            return seed_error();
        }
    };

    // Removals + allocation guard (admin.js:48-64).
    let removals_sql = format!(
        "SELECT MIN(c.card_id) AS card_id, SUM(c.quantity) AS remove_qty\n         FROM collection c\n         JOIN card_cache removed_cc ON removed_cc.id = c.card_id\n         WHERE c.user_id = ? AND c.quantity > 0\n           AND removed_cc.set_id IN ({})\n         GROUP BY {}",
        placeholders,
        crate::card_cache::card_key_sql("removed_cc")
    );
    let removals = match rows_map(&conn, &removals_sql, &set_params) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = ?e, "seed_cards removals");
            tx.rollback();
            return seed_error();
        }
    };
    for removal in &removals {
        let card_id = removal.get("card_id").cloned().unwrap_or(Value::Null);
        let status = match crate::collection_writes::logical_inventory_status(&conn, user_id, &card_id) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = ?e, "seed_cards inventory status");
                tx.rollback();
                return seed_error();
            }
        };
        let conflict = match &status {
            None => true,
            Some(s) => {
                num_of(s.get("owned_qty")) - num_of(removal.get("remove_qty")) < num_of(s.get("locked_qty"))
            }
        };
        if conflict {
            tx.rollback();
            return json_error(
                StatusCode::CONFLICT,
                "Check in decks using these cards before replacing the seed collection.",
            );
        }
    }

    match conn.execute(
        &format!(
            "DELETE FROM collection WHERE user_id = ? AND card_id IN (\n           SELECT id FROM card_cache WHERE set_id IN ({})\n         )",
            placeholders
        ),
        rusqlite::params_from_iter(set_params.iter()),
    ) {
        Ok(_) => {}
        Err(e) => {
            tracing::error!(error = %e, "seed_cards delete");
            tx.rollback();
            return seed_error();
        }
    }

    const CONDITIONS: [&str; 4] = ["Near Mint", "Lightly Played", "Moderately Played", "Heavily Played"];
    const LANGS: [&str; 4] = ["English", "English", "English", "Japanese"];

    let pool_len = mock_pool.len();
    let mut added_count: i64 = 0;
    let mut failed = false;
    for max_price in (0..168).map(|_| 10.0f64).chain((0..40).map(|_| 5.0f64)) {
        let card = &mock_pool[(js_random() * pool_len as f64).floor() as usize % pool_len];
        let card_id = card.get("id").cloned().unwrap_or(Value::Null);
        let card_id_s = match &card_id {
            Value::String(s) => s.clone(),
            other => crate::collection_writes::js_to_string(Some(other)),
        };
        // printsForCard: USD-gated printing options, default ['Normal'].
        let prints: Vec<&str> = {
            let mut p = Vec::new();
            if gt0(card.get("price_normal")) {
                p.push("Normal");
            }
            if gt0(card.get("price_holofoil")) {
                p.push("Holofoil");
            }
            if p.is_empty() {
                p.push("Normal");
            }
            p
        };
        let print = prints[(js_random() * prints.len() as f64).floor() as usize % prints.len()];
        let condition = CONDITIONS[(js_random() * 4.0).floor() as usize % 4];
        let language = LANGS[(js_random() * 4.0).floor() as usize % 4];
        let qty = (js_random() * 2.0).floor() as i64 + 1;
        let purchase_price = format!("{:.2}", js_random() * max_price).parse::<f64>().unwrap_or(0.0);
        match conn.execute(
            "INSERT INTO collection (card_id, quantity, condition, printing, language, purchase_price, user_id)\n          VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![card_id_s, qty, condition, print, language, purchase_price, user_id],
        ) {
            Ok(_) => added_count += qty,
            Err(e) => {
                tracing::error!(error = %e, "seed_cards insert");
                failed = true;
                break;
            }
        }
    }
    if failed {
        tx.rollback();
        return seed_error();
    }
    if let Err(e) = tx.commit() {
        tracing::error!(error = ?e, "seed_cards commit");
        return seed_error();
    }

    json_response(
        StatusCode::OK,
        &Value::Object(Map::from_iter([(
            "message".to_string(),
            Value::String(format!(
                "Successfully seeded a large test collection: {added_count} cards for admin user."
            )),
        )])),
    )
}

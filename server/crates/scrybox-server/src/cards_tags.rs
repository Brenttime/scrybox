//! Native `/api/cards/:id/oracle`, `/api/cards/:id/oracle-tags`,
//! `POST /api/cards/role-tags`, `POST /api/cards/tag-stats` — ports of
//! `routes/stats.js` (Lane B unit 2). Rules text comes from the shared
//! `card_cache` oracle columns; the legacy-row fetch leg rides the same
//! paced Scryfall client as the sets tree. Tag views answer the closure
//! tables the daily Oracle Tags job maintains.

use std::collections::{HashMap, HashSet};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Map, Value};

use crate::auth::{authenticate, bearer_token};
use crate::card_norm::normalize_card;
use crate::collection_writes::{node_exact, parse_body, BodyOutcome};
use crate::scryfall::ScryOpts;
use crate::AppState;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub(crate) fn json_response(status: StatusCode, v: &Value) -> Response {
    // Same shape every other ported group uses: Express res.json() bytes
    // (application/json; charset=utf-8, no ETag surprises).
    let bytes = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    (
        status,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            ),
        ],
        Body::from(bytes),
    )
        .into_response()
}

pub(crate) fn json_error(status: StatusCode, msg: &str) -> Response {
    json_response(status, &Value::Object({
        let mut m = Map::new();
        m.insert("error".into(), Value::String(msg.to_string()));
        m
    }))
}

/// `encodeURIComponent` (unreserved set A-Za-z0-9-_.!~*'()).
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric()
            || matches!(c, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')')
        {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `decodeURIComponent` for path params: returns None on malformed % escapes
/// (Node's Express router throws URIError there and the global error handler
/// answers 500 {error:'Internal server error'}).
pub(crate) fn decode_uri_component(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub(crate) fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    let q = query?;
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next()? == key {
            let raw = it.next().unwrap_or("");
            return Some(decode_uri_component(raw).unwrap_or_else(|| raw.to_string()));
        }
    }
    None
}

fn body_strings(body: &Value, key: &str) -> Vec<String> {
    // `[...new Set(req.body?.k.filter(x => typeof x === 'string'))]` — dedup
    // preserving first-seen order.
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    if let Some(arr) = body.get(key).and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str() {
                if seen.insert(s.to_string()) {
                    out.push(s.to_string());
                }
            }
        }
    }
    out
}

fn active_generation(conn: &rusqlite::Connection) -> Result<Option<i64>, String> {
    let mut stmt = conn
        .prepare("SELECT id FROM oracle_tag_generations WHERE active = 1")
        .map_err(|e| e.to_string())?;
    let mut rows = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .map_err(|e| e.to_string())?;
    match rows.next() {
        Some(Ok(id)) => Ok(Some(id)),
        Some(Err(e)) => Err(e.to_string()),
        None => Ok(None),
    }
}

/// Capped sibling of `collection_writes::read_parts` (REVIEW-auth.md H1):
/// streams at most `cap+1` bytes, reports oversize instead of buffering the
/// whole stream, and clears `content-length` on the oversize parts.
pub(crate) async fn read_body(req: Request, cap: usize) -> (axum::http::request::Parts, Vec<u8>, bool) {
    let (parts, body) = req.into_parts();
    match axum::body::to_bytes(body, cap.saturating_add(1)).await {
        Ok(b) => (parts, b.to_vec(), false),
        Err(_) => {
            let mut parts = parts;
            parts.headers.remove(axum::http::header::CONTENT_LENGTH);
            (parts, Vec::new(), true)
        }
    }
}

// ---------------------------------------------------------------------------
// GET /api/cards/:id/oracle — scryfallApi.getOracleText + routes/stats.js:537
// ---------------------------------------------------------------------------

fn oracle_shape(oracle_text: Option<String>, oracle_faces: Option<String>) -> Value {
    // shape(r): faces = parse(r.oracle_faces) || null on parse failure;
    // { oracle_text: r.oracle_text || '', faces }
    let faces = match &oracle_faces {
        Some(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        None => Value::Null,
    };
    let mut m = Map::new();
    m.insert(
        "oracle_text".into(),
        Value::String(oracle_text.unwrap_or_default()),
    );
    m.insert("faces".into(), faces);
    Value::Object(m)
}

/// getOracleText, phase 1: cached-row answer or "needs fetch". None ⇒ 404.
fn oracle_cached(conn: &rusqlite::Connection, card_id: &str) -> Option<Result<Value, ()>> {
    let row: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT id, oracle_text, oracle_faces FROM card_cache WHERE id = ?",
            [card_id],
            |r| Ok((r.get(1)?, r.get(2)?)),
        )
        .ok();
    if let Some((oracle_text, oracle_faces)) = &row {
        if oracle_text.is_some() {
            return Some(Ok(oracle_shape(oracle_text.clone(), oracle_faces.clone())));
        }
    }
    if !card_id.starts_with("mtg-") {
        return row.map(|_| Ok(oracle_shape(None, None)));
    }
    Some(Err(())) // legacy row (or missing) — caller fetches from Scryfall
}

/// getOracleText, phase 3: normalize the fetched card, backfill only the
/// rules-text columns (a full re-cache would also overwrite the row's
/// prices/art outside the normal refresh path — Node comment), answer.
fn oracle_after_fetch(
    conn: &rusqlite::Connection,
    card_id: &str,
    data: &Value,
) -> Value {
    let norm = normalize_card(data, None);
    let text = norm
        .get("oracle_text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let faces = norm.get("oracle_faces").cloned().unwrap_or(Value::Null);
    let exists: Option<i64> = conn
        .query_row("SELECT 1 FROM card_cache WHERE id = ?", [card_id], |_| Ok(1))
        .ok();
    if exists.is_some() {
        let faces_json = if faces.is_null() {
            None
        } else {
            Some(serde_json::to_string(&faces).unwrap_or_default())
        };
        let _ = conn.execute(
            "UPDATE card_cache SET oracle_text = ?, oracle_faces = ? WHERE id = ?",
            rusqlite::params![text, faces_json, card_id],
        );
    }
    let mut m = Map::new();
    m.insert("oracle_text".into(), Value::String(text));
    m.insert("faces".into(), faces);
    Value::Object(m)
}

pub async fn handle_oracle(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != axum::http::Method::GET || !st.owns("cards_tags") {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st.clone()), req).await;
    };
    let path = req.uri().path().to_string();
    let raw_id = match path
        .strip_prefix("/api/cards/")
        .and_then(|p| p.strip_suffix("/oracle"))
    {
        Some(id) if !id.is_empty() && !id.contains('/') => id.to_string(),
        _ => return crate::proxy(State(st.clone()), req).await,
    };
    let Some(card_id) = decode_uri_component(&raw_id) else {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
    };
    let headers = req.headers().clone();
    // authenticateToken: Bearer session; API keys answer GETs.
    let token = bearer_token(&headers).map(|s| s.to_string());
    if let Err(res) = authenticate(&pool, token.as_deref(), true) {
        return node_exact(res).await;
    }
    let scry = st.scry.clone();
    // Phase 1: cached answer or fetch decision (conn dropped before await —
    // rusqlite connections are not Send).
    let cached = (|| -> Result<Option<Result<Value, ()>>, Response> {
        let conn = pool.get().map_err(|_| {
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        })?;
        Ok(oracle_cached(&conn, &card_id))
    })();
    let cached = match cached {
        Ok(c) => c,
        Err(res) => return res,
    };
    match cached {
        Some(Ok(v)) => return json_response(StatusCode::OK, &v),
        Some(Err(())) => {}
        None => return json_error(StatusCode::NOT_FOUND, "Card not found"),
    }
    // Phase 2: legacy-row fetch leg through the shared paced client.
    let uuid = encode_uri_component(card_id.strip_prefix("mtg-").unwrap_or(&card_id));
    let fetched = scry
        .get_retried(&format!("/cards/{uuid}"), &ScryOpts::default(), 4)
        .await;
    let data = match fetched {
        // Node: resp = axios response, `resp.data` is the parsed body — the
        // client's Ok(Some(v)) already IS resp.data (no /sets-style envelope).
        Ok(Some(card)) => card,
        // Node's `!resp || !resp.data` falsy guard (Ok(None) = non-JSON body).
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "Card not found"),
        // scryGetRetried throws; the route catches 404 -> null (404 "Card
        // not found"), any other error -> 502.
        Err(e) if e.is_status(404) => return json_error(StatusCode::NOT_FOUND, "Card not found"),
        Err(e) => {
            tracing::warn!(error = ?e, "oracle text lookup failed");
            return json_error(StatusCode::BAD_GATEWAY, "Could not load rules text");
        }
    };
    // Phase 3: normalize + rules-text-only backfill.
    let out = (|| -> Result<Value, Response> {
        let conn = pool.get().map_err(|_| {
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        })?;
        Ok(oracle_after_fetch(&conn, &card_id, &data))
    })();
    match out {
        Ok(v) => json_response(StatusCode::OK, &v),
        Err(res) => res,
    }
}

// ---------------------------------------------------------------------------
// GET /api/cards/:id/oracle-tags — utils/cardOracleTags.js
// ---------------------------------------------------------------------------

fn row_str(r: &rusqlite::Row, i: usize) -> Result<Option<String>, rusqlite::Error> {
    r.get::<_, Option<String>>(i)
}

fn oracle_tags_query(
    conn: &rusqlite::Connection,
    card_id: &str,
    fallback: Option<&str>,
) -> Result<Value, String> {
    let Some(gen) = active_generation(conn)? else {
        let mut m = Map::new();
        m.insert("ready".into(), Value::Bool(false));
        m.insert("oracle_id".into(), Value::Null);
        m.insert("tags".into(), Value::Array(vec![]));
        m.insert("inherited".into(), Value::Array(vec![]));
        return Ok(Value::Object(m));
    };
    let row: Option<Option<String>> = conn
        .query_row(
            "SELECT oracle_id FROM card_cache WHERE id = ?",
            [card_id],
            |r| r.get::<_, Option<String>>(0).map(Some),
        )
        .ok()
        .flatten();
    let oracle_id = row
        .flatten()
        .filter(|s| !s.is_empty())
        .or_else(|| fallback.map(|s| s.to_string()));
    let Some(oracle_id) = oracle_id else {
        let mut m = Map::new();
        m.insert("ready".into(), Value::Bool(true));
        m.insert("oracle_id".into(), Value::Null);
        m.insert("tags".into(), Value::Array(vec![]));
        m.insert("inherited".into(), Value::Array(vec![]));
        return Ok(Value::Object(m));
    };

    // Direct tags: ORDER BY t.slug; strip(tag_id) keeps {slug,label,description}.
    let mut tags: Vec<Value> = Vec::new();
    let mut direct: HashSet<String> = HashSet::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT t.tag_id, t.slug, t.label, t.description
    FROM oracle_tag_assignments a
    JOIN oracle_tags t ON t.generation_id = a.generation_id AND t.tag_id = a.tag_id
    WHERE a.generation_id = ? AND a.oracle_id = ?
    ORDER BY t.slug
  ",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![gen, oracle_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    row_str(r, 1)?,
                    row_str(r, 2)?,
                    row_str(r, 3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for r in rows {
            let (tag_id, slug, label, description) = r.map_err(|e| e.to_string())?;
            direct.insert(tag_id);
            let mut m = Map::new();
            m.insert("slug".into(), slug.map(Value::String).unwrap_or(Value::Null));
            m.insert("label".into(), label.map(Value::String).unwrap_or(Value::Null));
            m.insert(
                "description".into(),
                description.map(Value::String).unwrap_or(Value::Null),
            );
            tags.push(Value::Object(m));
        }
    }

    // Inherited: closure edges minus self rows, grouped by ancestor tag_id in
    // row order (ORDER BY anc.slug, d.slug), `via` lists deduped per tag.
    struct Inherited {
        slug: Option<String>,
        label: Option<String>,
        description: Option<String>,
        via: Vec<String>,
    }
    let mut inherited: Vec<(String, Inherited)> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT anc.tag_id, anc.slug, anc.label, anc.description, d.slug AS via
    FROM oracle_tag_assignments a
    JOIN oracle_tag_closure c ON c.generation_id = a.generation_id AND c.descendant_tag_id = a.tag_id
    JOIN oracle_tags anc ON anc.generation_id = c.generation_id AND anc.tag_id = c.ancestor_tag_id
    JOIN oracle_tags d ON d.generation_id = a.generation_id AND d.tag_id = a.tag_id
    WHERE a.generation_id = ? AND a.oracle_id = ? AND c.ancestor_tag_id != c.descendant_tag_id
    ORDER BY anc.slug, d.slug
  ",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![gen, oracle_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    row_str(r, 1)?,
                    row_str(r, 2)?,
                    row_str(r, 3)?,
                    row_str(r, 4)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for r in rows {
            let (tag_id, slug, label, description, via) = r.map_err(|e| e.to_string())?;
            if direct.contains(&tag_id) {
                continue;
            }
            let pos = match inherited.iter().position(|(id, _)| *id == tag_id) {
                Some(p) => p,
                None => {
                    inherited.push((
                        tag_id,
                        Inherited {
                            slug,
                            label,
                            description,
                            via: Vec::new(),
                        },
                    ));
                    inherited.len() - 1
                }
            };
            if let Some(v) = via {
                if !inherited[pos].1.via.iter().any(|x| x == &v) {
                    inherited[pos].1.via.push(v);
                }
            }
        }
    }

    let inherited_vals: Vec<Value> = inherited
        .into_iter()
        .map(|(_, it)| {
            let mut m = Map::new();
            m.insert("slug".into(), it.slug.map(Value::String).unwrap_or(Value::Null));
            m.insert(
                "label".into(),
                it.label.map(Value::String).unwrap_or(Value::Null),
            );
            m.insert(
                "description".into(),
                it.description.map(Value::String).unwrap_or(Value::Null),
            );
            m.insert(
                "via".into(),
                Value::Array(it.via.into_iter().map(Value::String).collect()),
            );
            Value::Object(m)
        })
        .collect();

    let mut m = Map::new();
    m.insert("ready".into(), Value::Bool(true));
    m.insert("oracle_id".into(), Value::String(oracle_id));
    m.insert("tags".into(), Value::Array(tags));
    m.insert("inherited".into(), Value::Array(inherited_vals));
    Ok(Value::Object(m))
}

pub async fn handle_oracle_tags(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != axum::http::Method::GET || !st.owns("cards_tags") {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st.clone()), req).await;
    };
    let path = req.uri().path().to_string();
    let raw_id = match path
        .strip_prefix("/api/cards/")
        .and_then(|p| p.strip_suffix("/oracle-tags"))
    {
        Some(id) if !id.is_empty() && !id.contains('/') => id.to_string(),
        _ => return crate::proxy(State(st.clone()), req).await,
    };
    let Some(card_id) = decode_uri_component(&raw_id) else {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
    };
    let headers = req.headers().clone();
    let query = req.uri().query().map(|s| s.to_string());
    let token = bearer_token(&headers).map(|s| s.to_string());
    if let Err(res) = authenticate(&pool, token.as_deref(), true) {
        return node_exact(res).await;
    }
    let fallback = query_param(query.as_deref(), "oracle_id");
    let result = (|| -> Result<Value, String> {
        let conn = pool.get().map_err(|e| format!("read pool: {e}"))?;
        oracle_tags_query(&conn, &card_id, fallback.as_deref())
    })();
    match result {
        Ok(v) => json_response(StatusCode::OK, &v),
        Err(e) => {
            tracing::error!(error = %e, "oracle tags lookup failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not load tags")
        }
    }
}

// ---------------------------------------------------------------------------
// POST /api/cards/role-tags — routes/stats.js:560
// ---------------------------------------------------------------------------

fn role_tags(conn: &rusqlite::Connection, ids: &[String], slugs: &[String]) -> Result<Value, String> {
    let Some(gen) = active_generation(conn)? else {
        let mut m = Map::new();
        m.insert("ready".into(), Value::Bool(false));
        m.insert("tags".into(), Value::Object(Map::new()));
        return Ok(Value::Object(m));
    };
    let sql = format!(
        "
      SELECT DISTINCT cc.id AS card_id, anc.slug AS slug
      FROM card_cache cc
      JOIN oracle_tag_assignments a ON a.generation_id = ? AND a.oracle_id = cc.oracle_id
      JOIN oracle_tag_closure c ON c.generation_id = a.generation_id AND c.descendant_tag_id = a.tag_id
      JOIN oracle_tags anc ON anc.generation_id = c.generation_id AND anc.tag_id = c.ancestor_tag_id
      WHERE cc.id IN ({})
        AND anc.slug IN ({})
    ",
        vec!["?"; ids.len()].join(","),
        vec!["?"; slugs.len()].join(",")
    );
    let mut params: Vec<String> = Vec::with_capacity(1 + ids.len() + slugs.len());
    for id in ids {
        params.push(id.clone());
    }
    for s in slugs {
        params.push(s.clone());
    }
    // rusqlite params are positional; generation id goes first.
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let mut all: Vec<rusqlite::types::Value> = vec![rusqlite::types::Value::Integer(gen)];
    all.extend(params.into_iter().map(rusqlite::types::Value::Text));
    let rows = stmt
        .query_map(rusqlite::params_from_iter(all), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?;
    // tags[card_id] ||= []; push slug — insertion-ordered card map.
    let mut tags: Map<String, Value> = Map::new();
    for r in rows {
        let (card_id, slug) = r.map_err(|e| e.to_string())?;
        tags.entry(card_id)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .unwrap()
            .push(Value::String(slug));
    }
    let mut out = Map::new();
    out.insert("ready".into(), Value::Bool(true));
    out.insert("tags".into(), Value::Object(tags));
    Ok(Value::Object(out))
}

pub async fn handle_role_tags(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != axum::http::Method::POST || !st.owns("cards_tags") {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st.clone()), req).await;
    };
    // Node order: express.json parse errors outrank everything (global
    // normalJson mounts ahead of authenticateToken), then the GLOBAL
    // app.use('/api', authenticateToken) gate (server.js:430) runs BEFORE the
    // route body — a no-token request answers 401 even with a bad-shape body,
    // and an api key on POST answers 403 (read-only rule) — and only then the
    // route's 400 shape check.
    let (parts, bytes, oversize) = read_body(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    let headers = parts.headers.clone();
    let body = match parse_body(&headers, &bytes) {
        BodyOutcome::TooLarge => {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Upload too large. Try exporting/importing in smaller batches.",
            )
        }
        BodyOutcome::Invalid => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        }
        BodyOutcome::Parsed(v) => v,
    };
    let token = bearer_token(&headers).map(|s| s.to_string());
    if let Err(res) = authenticate(&pool, token.as_deref(), false) {
        return node_exact(res).await;
    }
    let ids = body_strings(&body, "ids");
    let slugs = body_strings(&body, "slugs");
    if ids.is_empty() || slugs.is_empty() || ids.len() > 1000 || slugs.len() > 100 {
        return json_error(
            StatusCode::BAD_REQUEST,
            "ids (1-1000) and slugs (1-100) are required",
        );
    }
    let result = (|| -> Result<Value, String> {
        let conn = pool.get().map_err(|e| format!("read pool: {e}"))?;
        role_tags(&conn, &ids, &slugs)
    })();
    match result {
        Ok(v) => json_response(StatusCode::OK, &v),
        Err(e) => {
            tracing::error!(error = %e, "role tags lookup failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not load tags")
        }
    }
}

// ---------------------------------------------------------------------------
// POST /api/cards/tag-stats — routes/stats.js:592
// ---------------------------------------------------------------------------

/// routes/stats.js TAG_NOISE: `^(cycle|synergy-|...|name-|has-|mentions-|
/// references-)` plus the `.*-art$` alternative (suffix match).
const TAG_NOISE_PREFIXES: &[&str] = &[
    "cycle",
    "synergy-",
    "card-names",
    "alliteration",
    "flavors-of",
    "staple-with",
    "cheaper-than",
    "triggered-ability",
    "activated-ability",
    "static-ability",
    "tap-",
    "discount-",
    "reprint",
    "single-target",
    "multi-target",
    "art-",
    "pun",
    "reference",
    "joke",
    "flavor",
    "vanilla",
    "french-vanilla",
    "keyword-",
    "mechanic-",
    "cast-trigger",
    "etb-trigger",
    "death-trigger",
    "virtual-",
    "unique-type-line",
    "type-line-",
    "name-",
    "has-",
    "mentions-",
    "references-",
];

fn is_noise_slug(slug: &str) -> bool {
    TAG_NOISE_PREFIXES.iter().any(|p| slug.starts_with(*p)) || slug.ends_with("-art")
}

fn js_locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    // Node sorts the tie-break with String.localeCompare under the default
    // (root) ICU collation. The slug alphabet here is [a-z0-9-_]; verified
    // empirically (bench/oracle-slug-order check) that ICU root order agrees
    // with byte order on that alphabet, so this is plain byte compare.
    a.as_bytes().cmp(b.as_bytes())
}

fn tag_stats(conn: &rusqlite::Connection, ids: &[String]) -> Result<Value, String> {
    let Some(gen) = active_generation(conn)? else {
        let mut m = Map::new();
        m.insert("ready".into(), Value::Bool(false));
        m.insert("tags".into(), Value::Array(vec![]));
        return Ok(Value::Object(m));
    };
    let sql = format!(
        "
      SELECT anc.slug AS slug, anc.label AS label, cc.id AS card_id
      FROM card_cache cc
      JOIN oracle_tag_assignments a ON a.generation_id = ? AND a.oracle_id = cc.oracle_id
      JOIN oracle_tag_closure c ON c.generation_id = a.generation_id AND c.descendant_tag_id = a.tag_id
      JOIN oracle_tags anc ON anc.generation_id = c.generation_id AND anc.tag_id = c.ancestor_tag_id
      WHERE cc.id IN ({})
    ",
        vec!["?"; ids.len()].join(",")
    );
    let mut all: Vec<rusqlite::types::Value> = vec![rusqlite::types::Value::Integer(gen)];
    all.extend(ids.iter().cloned().map(rusqlite::types::Value::Text));
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(all), |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    struct Bucket {
        label: String,
        cards: Vec<String>,
    }
    let mut order: Vec<String> = Vec::new();
    let mut by_slug: HashMap<String, Bucket> = HashMap::new();
    for r in rows {
        let (slug, label, card_id) = r.map_err(|e| e.to_string())?;
        let Some(slug) = slug else { continue }; // anc.slug NULL: Map key 'undefined' — no deck rows carry it
        if is_noise_slug(&slug) {
            continue;
        }
        let label = label.filter(|l| !l.is_empty()).unwrap_or_else(|| slug.clone());
        let bucket = by_slug.entry(slug.clone()).or_insert_with(|| {
            order.push(slug.clone());
            Bucket {
                label,
                cards: Vec::new(),
            }
        });
        if !bucket.cards.iter().any(|c| c == &card_id) {
            bucket.cards.push(card_id);
        }
    }
    let mut scored: Vec<(String, Bucket)> = order
        .into_iter()
        .filter_map(|s| by_slug.remove(&s).map(|b| (s, b)))
        .filter(|(_, b)| b.cards.len() >= 2)
        .collect();
    scored.sort_by(|(sa, a), (sb, b)| {
        b.cards
            .len()
            .cmp(&a.cards.len())
            .then_with(|| js_locale_cmp(sa, sb))
    });
    let tags: Vec<Value> = scored
        .into_iter()
        .take(40)
        .map(|(slug, b)| {
            let mut m = Map::new();
            m.insert("slug".into(), Value::String(slug));
            m.insert("label".into(), Value::String(b.label));
            m.insert(
                "cards".into(),
                Value::Array(b.cards.into_iter().map(Value::String).collect()),
            );
            Value::Object(m)
        })
        .collect();
    let mut out = Map::new();
    out.insert("ready".into(), Value::Bool(true));
    out.insert("tags".into(), Value::Array(tags));
    Ok(Value::Object(out))
}

pub async fn handle_tag_stats(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != axum::http::Method::POST || !st.owns("cards_tags") {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st.clone()), req).await;
    };
    // Same global middleware order as role-tags: parse errors, then the
    // /api authenticateToken gate, then the route's 400 shape check.
    let (parts, bytes, oversize) = read_body(req, crate::collection_writes::BODY_NORMAL).await;
    if oversize && crate::collection_writes::is_json_ct(&parts.headers) {
        return crate::collection_writes::too_large();
    }
    let headers = parts.headers.clone();
    let body = match parse_body(&headers, &bytes) {
        BodyOutcome::TooLarge => {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Upload too large. Try exporting/importing in smaller batches.",
            )
        }
        BodyOutcome::Invalid => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
        }
        BodyOutcome::Parsed(v) => v,
    };
    let token = bearer_token(&headers).map(|s| s.to_string());
    if let Err(res) = authenticate(&pool, token.as_deref(), false) {
        return node_exact(res).await;
    }
    let ids = body_strings(&body, "ids");
    if ids.is_empty() || ids.len() > 1000 {
        return json_error(StatusCode::BAD_REQUEST, "ids (1-1000) are required");
    }
    let result = (|| -> Result<Value, String> {
        let conn = pool.get().map_err(|e| format!("read pool: {e}"))?;
        tag_stats(&conn, &ids)
    })();
    match result {
        Ok(v) => json_response(StatusCode::OK, &v),
        Err(e) => {
            tracing::error!(error = %e, "tag stats failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not load tags")
        }
    }
}

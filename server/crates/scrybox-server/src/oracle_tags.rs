//! Oracle Tags maintenance twin of `backend/src/oracleTags.js`
//! (`runMaintenance` + `startOracleTagsService`): daily-gated, generation-
//! swapped import of Scryfall's `oracle_tags` bulk file and the one-time
//! `card_cache.oracle_id` / `scryfall_search_eligible` backfill (Default
//! Cards bulk scan, then exact-identifier residual lookups), followed by a
//! best-effort `PRAGMA wal_checkpoint(TRUNCATE)`.
//!
//! Scheduling (Node): first run 30 s after boot, then hourly; the refresh
//! itself is gated to one successful check per day by
//! `app_settings.oracle_tags_checked_at`, so the hourly poll self-heals a
//! failed download without hammering Scryfall. The server.js boot catch-up
//! also awaits one `runMaintenance` before the price sweep — the Rust jobs
//! thread runs it in the same order (see scry_jobs::boot_catchup).
//!
//! Bulk files are streamed to a temp file (never held in memory: Default
//! Cards is ~500 MB) and read line by line, gunzipping when the payload
//! starts with the gzip magic (Node `maybeGunzip`).

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};

use http_body_util::BodyExt;
use hyper_util::client::legacy::connect::HttpConnector;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::Value;

use crate::card_norm::{default_search_eligible, normalize_card, oracle_id_for_card, scryfall_uuid};
use crate::scryfall::{Scry, ScryOpts};

pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;
pub const MAINTENANCE_POLL_MS: u64 = 60 * 60 * 1000;
pub const START_DELAY_MS: u64 = 30 * 1000;
const USER_AGENT: &str = "Scrybox/1.0 (+https://github.com/Brenttime/scrybox)";
const COLLECTION_BATCH: usize = 75;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

type Https = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<HttpConnector>,
    http_body_util::Empty<bytes::Bytes>,
>;

/// Plain (unqueued) HTTP for the bulk endpoints — Node uses its own axios
/// instance here, not the paced Scryfall queue. `base` is the API base
/// (`SCRYFALL_BASE_URL`, default https://api.scryfall.com); Node exempts
/// this job from LOCAL_SCRYFALL because the sidecar has no /bulk-data.
pub struct BulkHttp {
    base: String,
    client: Https,
}

impl BulkHttp {
    pub fn new(base: &str) -> Self {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("native roots")
            .https_or_http()
            .enable_http1()
            .build();
        let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
        Self { base: base.trim_end_matches('/').to_string(), client }
    }

    pub fn from_scry(scry: &Scry) -> Self {
        Self::new(scry.base_url())
    }

    async fn get_resp(&self, url: &str) -> Result<hyper::Response<hyper::body::Incoming>, String> {
        let mut url = url.to_string();
        // axios follows redirects (bulk download URIs may 302 to a CDN).
        for _ in 0..5 {
            let req = hyper::Request::get(&url)
                .header("User-Agent", USER_AGENT)
                .header("Accept", "application/json")
                .body(http_body_util::Empty::new())
                .map_err(|e| e.to_string())?;
            let resp = tokio::time::timeout(std::time::Duration::from_secs(120), self.client.request(req))
                .await
                .map_err(|_| format!("timeout of 120000ms exceeded ({url})"))?
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            if status.is_redirection() {
                if let Some(loc) = resp.headers().get("location").and_then(|v| v.to_str().ok()) {
                    url = if loc.starts_with("http") { loc.to_string() } else { format!("{}{}", self.base, loc) };
                    continue;
                }
            }
            if !status.is_success() {
                return Err(format!("Request failed with status code {}", status.as_u16()));
            }
            return Ok(resp);
        }
        Err("too many redirects".into())
    }

    async fn get_json(&self, path: &str) -> Result<Value, String> {
        let resp = self.get_resp(&format!("{}{}", self.base, path)).await?;
        let body = resp.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        serde_json::from_slice(&body).map_err(|e| e.to_string())
    }

    /// Stream `url` to an anonymous temp file; returns it rewound.
    async fn download(&self, url: &str) -> Result<std::fs::File, String> {
        let resp = self.get_resp(url).await?;
        let mut file = tempfile_in_tmp()?;
        let mut body = resp.into_body();
        while let Some(frame) = tokio::time::timeout(std::time::Duration::from_secs(120), body.frame())
            .await
            .map_err(|_| "download stalled (120 s)".to_string())?
        {
            let frame = frame.map_err(|e| e.to_string())?;
            if let Some(d) = frame.data_ref() {
                file.write_all(d).map_err(|e| e.to_string())?;
            }
        }
        use std::io::Seek;
        file.flush().map_err(|e| e.to_string())?;
        file.seek(std::io::SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        Ok(file)
    }
}

fn tempfile_in_tmp() -> Result<std::fs::File, String> {
    let dir = std::env::temp_dir();
    let name = format!("scrybox-bulk-{}-{}", std::process::id(), now_ms());
    let path = dir.join(name);
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("temp file: {e}"))?;
    // Unlink now: the open handle keeps the data until the File drops.
    let _ = std::fs::remove_file(&path);
    Ok(f)
}

/// Node `bulkMetadata`.
pub fn bulk_metadata(body: &Value, ty: &str) -> Result<(String, Option<String>), String> {
    let value = match (body.get("data"), body.get("type")) {
        (Some(d), None) if !d.is_null() => d,
        _ => body,
    };
    if value.get("type").and_then(|v| v.as_str()) != Some(ty) {
        return Err(format!("Scryfall bulk-data/{ty} returned an unexpected object"));
    }
    let url = value
        .get("jsonl_download_uri")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| value.get("download_uri").and_then(|v| v.as_str()).filter(|s| !s.is_empty()))
        .ok_or_else(|| format!("Scryfall bulk-data/{ty} has no JSONL download URI"))?;
    let updated = value.get("updated_at").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from);
    Ok((url.to_string(), updated))
}

/// Node `maybeGunzip` + `streamJsonl`: line iterator over JSON objects,
/// tolerating a JSON-array file (`[`, `]`, trailing commas).
pub fn jsonl_objects<R: Read + 'static>(mut input: R) -> impl Iterator<Item = Result<Value, String>> {
    let mut head = [0u8; 2];
    let mut n = 0;
    while n < 2 {
        match input.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => break,
        }
    }
    let rebuilt = std::io::Cursor::new(head[..n].to_vec()).chain(input);
    let reader: Box<dyn Read> = if n >= 2 && head[0] == 0x1f && head[1] == 0x8b {
        Box::new(flate2::read::MultiGzDecoder::new(rebuilt))
    } else {
        Box::new(rebuilt)
    };
    let mut lines = BufReader::with_capacity(1 << 16, reader).lines();
    let mut line_no = 0usize;
    std::iter::from_fn(move || loop {
        let line = match lines.next()? {
            Ok(l) => l,
            Err(e) => return Some(Err(e.to_string())),
        };
        line_no += 1;
        let t = line.trim();
        let t = t.strip_suffix(',').unwrap_or(t);
        if t.is_empty() || t == "[" || t == "]" {
            continue;
        }
        return Some(serde_json::from_str(t).map_err(|e| format!("Invalid JSONL at line {line_no}: {e}")));
    })
}

pub struct TagRow {
    pub id: String,
    pub slug: String,
    pub label: Option<String>,
    pub description: Option<String>,
    pub aliases: Vec<String>,
}

pub struct TagData {
    pub tags: Vec<TagRow>,
    pub edges: Vec<(String, String)>,
    pub assignments: Vec<(String, String)>,
    pub closure: Vec<(String, String)>,
}

fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn opt_str(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(x) => Some(js_string(x)),
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        _ => true,
    }
}

/// Node `collectTagData`: tags keyed by id (last wins, first-seen order),
/// insertion-ordered edge/assignment sets, dangling edges dropped, BFS
/// closure per ancestor (self included).
pub fn collect_tag_data<I: Iterator<Item = Result<Value, String>>>(objects: I) -> Result<TagData, String> {
    let mut order: Vec<String> = Vec::new();
    let mut tags: HashMap<String, TagRow> = HashMap::new();
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut edge_seen: HashSet<(String, String)> = HashSet::new();
    let mut assignments: Vec<(String, String)> = Vec::new();
    let mut asg_seen: HashSet<(String, String)> = HashSet::new();
    for raw in objects {
        let raw = raw?;
        if raw.get("type").and_then(|v| v.as_str()) != Some("oracle") {
            continue;
        }
        let (Some(id), Some(slug)) = (raw.get("id").filter(|v| truthy(v)), raw.get("slug").filter(|v| truthy(v))) else {
            continue;
        };
        let id = js_string(id);
        let slug = js_string(slug);
        let mut aliases = vec![slug.trim().to_lowercase()];
        if let Some(arr) = raw.get("aliases").and_then(|v| v.as_array()) {
            for a in arr {
                let s = if a.is_null() { String::new() } else { js_string(a) };
                let clean = s.trim().to_lowercase();
                if !clean.is_empty() && !aliases.contains(&clean) {
                    aliases.push(clean);
                }
            }
        }
        let row = TagRow {
            id: id.clone(),
            slug,
            label: opt_str(raw.get("label")),
            description: opt_str(raw.get("description")),
            aliases,
        };
        if !tags.contains_key(&id) {
            order.push(id.clone());
        }
        tags.insert(id.clone(), row);
        for p in raw.get("parent_ids").and_then(|v| v.as_array()).into_iter().flatten() {
            if truthy(p) {
                let e = (js_string(p), id.clone());
                if edge_seen.insert(e.clone()) {
                    edges.push(e);
                }
            }
        }
        for c in raw.get("child_ids").and_then(|v| v.as_array()).into_iter().flatten() {
            if truthy(c) {
                let e = (id.clone(), js_string(c));
                if edge_seen.insert(e.clone()) {
                    edges.push(e);
                }
            }
        }
        for t in raw.get("taggings").and_then(|v| v.as_array()).into_iter().flatten() {
            if let Some(o) = t.get("oracle_id").filter(|v| truthy(v)) {
                let a = (id.clone(), js_string(o));
                if asg_seen.insert(a.clone()) {
                    assignments.push(a);
                }
            }
        }
    }
    if tags.is_empty() {
        return Err("Oracle Tags stream contained no usable oracle tags".into());
    }
    let mut children: HashMap<&str, Vec<&str>> = order.iter().map(|id| (id.as_str(), Vec::new())).collect();
    let mut clean_edges = Vec::new();
    for (p, c) in &edges {
        if !tags.contains_key(p) || !tags.contains_key(c) {
            continue;
        }
        // INFALLIBLE: children is seeded from every id in order, and p is a tags key (checked above)
        let list = children.get_mut(p.as_str()).expect("parent known");
        if !list.contains(&c.as_str()) {
            list.push(c.as_str());
        }
        clean_edges.push((p.clone(), c.clone()));
    }
    let mut closure = Vec::new();
    for anc in &order {
        let mut seen: HashSet<&str> = HashSet::from([anc.as_str()]);
        let mut queue: Vec<&str> = vec![anc.as_str()];
        let mut head = 0;
        while head < queue.len() {
            let cur = queue[head];
            head += 1;
            closure.push((anc.clone(), cur.to_string()));
            for ch in children.get(cur).into_iter().flatten() {
                if seen.insert(ch) {
                    queue.push(ch);
                }
            }
        }
    }
    // INFALLIBLE: order holds each tag id exactly once, so every remove() hits
    let tags = order.into_iter().map(|id| tags.remove(&id).expect("ordered tag")).collect();
    Ok(TagData { tags, edges: clean_edges, assignments, closure })
}

/// Node `stageMappedRows`: 6000-row writer transactions of 150-row INSERTs.
fn stage_rows(conn: &Connection, prefix: &str, rows: &[Vec<rusqlite::types::Value>]) -> rusqlite::Result<()> {
    for tx_chunk in rows.chunks(6000) {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let res = (|| {
            for chunk in tx_chunk.chunks(150) {
                let cols = chunk[0].len();
                let ph = format!("({})", vec!["?"; cols].join(", "));
                let sql = format!("{prefix} {}", vec![ph.as_str(); chunk.len()].join(", "));
                conn.execute(&sql, params_from_iter(chunk.iter().flatten()))?;
            }
            Ok(())
        })();
        match res {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
    }
    Ok(())
}

fn delete_inactive_generation(conn: &Connection, generation_id: i64) -> rusqlite::Result<()> {
    const BATCH: i64 = 2000;
    for table in [
        "oracle_tag_aliases",
        "oracle_tag_edges",
        "oracle_tag_assignments",
        "oracle_tag_closure",
        "oracle_tags",
    ] {
        loop {
            let n = conn.execute(
                &format!(
                    "DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE generation_id = ? LIMIT ?)"
                ),
                params![generation_id, BATCH],
            )?;
            if (n as i64) < BATCH {
                break;
            }
        }
    }
    conn.execute("DELETE FROM oracle_tag_generations WHERE id = ? AND active = 0", params![generation_id])?;
    Ok(())
}

fn reap_inactive_generations(conn: &Connection) -> rusqlite::Result<()> {
    let ids: Vec<i64> = conn
        .prepare("SELECT id FROM oracle_tag_generations WHERE active = 0")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for id in ids {
        delete_inactive_generation(conn, id)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportResult {
    pub generation_id: i64,
    pub tags: usize,
    pub assignments: usize,
}

/// Node `importTagObjects`.
pub fn import_tag_data(conn: &Connection, data: &TagData, source_updated_at: Option<&str>) -> Result<ImportResult, String> {
    use rusqlite::types::Value as V;
    conn.execute(
        "INSERT INTO oracle_tag_generations (source_updated_at, imported_at, active) VALUES (?, ?, 0)",
        params![source_updated_at.unwrap_or(""), now_ms()],
    )
    .map_err(|e| e.to_string())?;
    let gid = conn.last_insert_rowid();
    let staged = (|| -> rusqlite::Result<()> {
        let g = V::Integer(gid);
        let s = |x: &str| V::Text(x.to_string());
        let o = |x: &Option<String>| x.as_ref().map(|v| V::Text(v.clone())).unwrap_or(V::Null);
        let rows: Vec<Vec<V>> = data
            .tags
            .iter()
            .map(|t| vec![g.clone(), s(&t.id), s(&t.slug), o(&t.label), o(&t.description)])
            .collect();
        stage_rows(conn, "INSERT INTO oracle_tags (generation_id, tag_id, slug, label, description) VALUES", &rows)?;
        let rows: Vec<Vec<V>> = data
            .tags
            .iter()
            .flat_map(|t| t.aliases.iter().map(move |a| (t, a)))
            .map(|(t, a)| vec![g.clone(), s(&t.id), s(a)])
            .collect();
        stage_rows(conn, "INSERT INTO oracle_tag_aliases (generation_id, tag_id, alias) VALUES", &rows)?;
        let pair = |v: &[(String, String)]| -> Vec<Vec<V>> { v.iter().map(|(a, b)| vec![g.clone(), s(a), s(b)]).collect() };
        stage_rows(
            conn,
            "INSERT INTO oracle_tag_edges (generation_id, parent_tag_id, child_tag_id) VALUES",
            &pair(&data.edges),
        )?;
        stage_rows(
            conn,
            "INSERT INTO oracle_tag_assignments (generation_id, tag_id, oracle_id) VALUES",
            &pair(&data.assignments),
        )?;
        stage_rows(
            conn,
            "INSERT INTO oracle_tag_closure (generation_id, ancestor_tag_id, descendant_tag_id) VALUES",
            &pair(&data.closure),
        )?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let swap = conn
            .execute("UPDATE oracle_tag_generations SET active = 0 WHERE active = 1", [])
            .and_then(|_| conn.execute("UPDATE oracle_tag_generations SET active = 1 WHERE id = ?", params![gid]));
        match swap {
            Ok(_) => conn.execute_batch("COMMIT"),
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    })();
    if let Err(e) = staged {
        if let Err(c) = delete_inactive_generation(conn, gid) {
            tracing::error!("Oracle Tags failed-generation cleanup failed: {c}");
        }
        return Err(e.to_string());
    }
    if let Err(e) = reap_inactive_generations(conn) {
        tracing::error!("Oracle Tags inactive-generation cleanup failed: {e}");
    }
    Ok(ImportResult { generation_id: gid, tags: data.tags.len(), assignments: data.assignments.len() })
}

#[derive(Debug, Clone, PartialEq)]
pub enum TagsStatus {
    NotDue,
    Current,
    Refreshed(ImportResult),
}

/// Node `refreshOracleTags`.
pub async fn refresh_oracle_tags(conn: &Connection, http: &BulkHttp, now: i64, force: bool) -> Result<TagsStatus, String> {
    let last: i64 = conn
        .query_row("SELECT oracle_tags_checked_at FROM app_settings WHERE id = 1", [], |r| {
            r.get::<_, Option<f64>>(0)
        })
        .optional()
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|f| f as i64)
        .unwrap_or(0);
    if !force && now - last < DAY_MS {
        return Ok(TagsStatus::NotDue);
    }
    let meta = http.get_json("/bulk-data/oracle_tags").await?;
    let (url, updated) = bulk_metadata(&meta, "oracle_tags")?;
    let active: Option<Option<String>> = conn
        .query_row("SELECT source_updated_at FROM oracle_tag_generations WHERE active = 1", [], |r| r.get(0))
        .optional()
        .map_err(|e| e.to_string())?;
    if let (Some(Some(a)), Some(u)) = (&active, &updated) {
        if a == u {
            conn.execute("UPDATE app_settings SET oracle_tags_checked_at = ? WHERE id = 1", params![now])
                .map_err(|e| e.to_string())?;
            return Ok(TagsStatus::Current);
        }
    }
    let file = http.download(&url).await?;
    let data = collect_tag_data(jsonl_objects(file))?;
    let res = import_tag_data(conn, &data, updated.as_deref())?;
    conn.execute("UPDATE app_settings SET oracle_tags_checked_at = ? WHERE id = 1", params![now])
        .map_err(|e| e.to_string())?;
    Ok(TagsStatus::Refreshed(res))
}

/// (id, oracle_id, eligible) — Node `updateOracleIds` (250-row CASE batches).
fn update_oracle_ids(conn: &Connection, pairs: &[(String, Option<String>, Option<i64>)]) -> rusqlite::Result<()> {
    use rusqlite::types::Value as V;
    let clean: Vec<_> = pairs.iter().filter(|(id, o, e)| !id.is_empty() && (o.is_some() || e.is_some())).collect();
    for chunk in clean.chunks(250) {
        let oc = vec!["WHEN ? THEN COALESCE(?, oracle_id)"; chunk.len()].join(" ");
        let ec = vec!["WHEN ? THEN COALESCE(?, scryfall_search_eligible)"; chunk.len()].join(" ");
        let ids = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "UPDATE card_cache SET oracle_id = CASE id {oc} ELSE oracle_id END, \
             scryfall_search_eligible = CASE id {ec} ELSE scryfall_search_eligible END WHERE id IN ({ids})"
        );
        let mut p: Vec<V> = Vec::with_capacity(chunk.len() * 5);
        for (id, o, _) in chunk {
            p.push(V::Text(id.clone()));
            p.push(o.clone().map(V::Text).unwrap_or(V::Null));
        }
        for (id, _, e) in chunk {
            p.push(V::Text(id.clone()));
            p.push(e.map(V::Integer).unwrap_or(V::Null));
        }
        for (id, _, _) in chunk {
            p.push(V::Text(id.clone()));
        }
        conn.execute(&sql, params_from_iter(p))?;
    }
    Ok(())
}

struct Unresolved {
    id: String,
    set_id: String,
    number: String,
    name: String,
}

fn unresolved_rows(conn: &Connection) -> rusqlite::Result<Vec<Unresolved>> {
    conn.prepare(
        "SELECT id, set_id, number, name FROM card_cache WHERE id LIKE 'mtg-%' \
         AND (oracle_id IS NULL OR scryfall_search_eligible IS NULL)",
    )?
    .query_map([], |r| {
        Ok(Unresolved {
            id: r.get(0)?,
            set_id: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            number: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            name: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
        })
    })?
    .collect()
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BackfillResult {
    pub bulk: usize,
    pub fallback: usize,
    pub unresolved: i64,
}

/// Node `backfillOracleIds`.
pub async fn backfill_oracle_ids(
    conn: &Connection,
    http: &BulkHttp,
    scry: &Scry,
    now: i64,
    force: bool,
) -> Result<BackfillResult, String> {
    let done: Option<f64> = conn
        .query_row("SELECT oracle_id_backfill_completed_at FROM app_settings WHERE id = 1", [], |r| r.get(0))
        .optional()
        .map_err(|e| e.to_string())?
        .flatten();
    let bulk_done = !force && done.map(|v| v != 0.0).unwrap_or(false);
    let unresolved = unresolved_rows(conn).map_err(|e| e.to_string())?;
    if unresolved.is_empty() {
        conn.execute("UPDATE app_settings SET oracle_id_backfill_completed_at = ? WHERE id = 1", params![now])
            .map_err(|e| e.to_string())?;
        return Ok(BackfillResult::default());
    }
    let mut bulk = 0usize;
    if !bulk_done {
        let mut wanted: HashSet<String> = unresolved.iter().map(|r| r.id.clone()).collect();
        let meta = http.get_json("/bulk-data/default_cards").await?;
        let (url, _) = bulk_metadata(&meta, "default_cards")?;
        let file = http.download(&url).await?;
        let mut buf: Vec<(String, Option<String>, Option<i64>)> = Vec::new();
        for raw in jsonl_objects(file) {
            let raw = raw?;
            let Some(oid) = oracle_id_for_card(&raw) else { continue };
            let Some(rid) = raw.get("id").filter(|v| truthy(v)).map(js_string) else { continue };
            if raw.get("lang").filter(|v| truthy(v)).map(js_string).is_some_and(|l| l != "en") {
                continue;
            }
            let id = format!("mtg-{rid}");
            if !wanted.remove(&id) {
                continue;
            }
            buf.push((id, Some(oid), default_search_eligible(&raw)));
            if buf.len() >= 500 {
                update_oracle_ids(conn, &buf).map_err(|e| e.to_string())?;
                bulk += buf.len();
                buf.clear();
            }
        }
        if !buf.is_empty() {
            update_oracle_ids(conn, &buf).map_err(|e| e.to_string())?;
            bulk += buf.len();
        }
        conn.execute("UPDATE app_settings SET oracle_id_backfill_completed_at = ? WHERE id = 1", params![now])
            .map_err(|e| e.to_string())?;
    }
    let unresolved = unresolved_rows(conn).map_err(|e| e.to_string())?;
    let mut fallback = 0usize;
    if !unresolved.is_empty() {
        let pairs = fetch_by_identifier(scry, &unresolved).await?;
        update_oracle_ids(conn, &pairs).map_err(|e| e.to_string())?;
        fallback = pairs.len();
    }
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM card_cache WHERE id LIKE 'mtg-%' AND (oracle_id IS NULL OR scryfall_search_eligible IS NULL)",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(BackfillResult { bulk, fallback, unresolved: remaining })
}

/// scryfallApi.bulkFetchByIdentifier (default opts: sidecar-eligible) reduced
/// to the oracle-id pairs backfillOracleIds keeps.
async fn fetch_by_identifier(
    scry: &Scry,
    rows: &[Unresolved],
) -> Result<Vec<(String, Option<String>, Option<i64>)>, String> {
    let mut out = Vec::new();
    for chunk in rows.chunks(COLLECTION_BATCH) {
        let mut by_key: HashMap<String, &Unresolved> = HashMap::new();
        let idents: Vec<Value> = chunk
            .iter()
            .map(|r| {
                if let Some(u) = scryfall_uuid(Some(&r.id)) {
                    by_key.insert(format!("id:{}", u.to_lowercase()), r);
                    serde_json::json!({ "id": u })
                } else if !r.set_id.is_empty() && !r.number.is_empty() {
                    let s = r.set_id.to_lowercase();
                    by_key.insert(format!("sn:{s}|{}", r.number.to_lowercase()), r);
                    serde_json::json!({ "set": s, "collector_number": r.number })
                } else {
                    by_key.insert(format!("n:{}", r.name.to_lowercase()), r);
                    serde_json::json!({ "name": r.name })
                }
            })
            .collect();
        let resp = scry
            .post_retried("/cards/collection", serde_json::json!({ "identifiers": idents }), &ScryOpts::default(), 4)
            .await
            .map_err(|e| e.to_string())?;
        for raw in resp.as_ref().and_then(|v| v.get("data")).and_then(|v| v.as_array()).into_iter().flatten() {
            let norm = normalize_card(raw, None);
            let g = |k: &str| norm.get(k).map(js_string).unwrap_or_default().to_lowercase();
            let row = by_key
                .get(&format!("id:{}", raw.get("id").map(js_string).unwrap_or_default().to_lowercase()))
                .or_else(|| by_key.get(&format!("sn:{}|{}", g("set_id"), g("number"))))
                .or_else(|| by_key.get(&format!("n:{}", g("name"))));
            let Some(row) = row else { continue };
            let Some(oid) = oracle_id_for_card(&norm).or_else(|| oracle_id_for_card(raw)) else { continue };
            let elig = match norm.get("scryfall_search_eligible") {
                Some(v) if !v.is_null() => v.as_i64(),
                _ => default_search_eligible(&norm).or_else(|| default_search_eligible(raw)),
            };
            out.push((row.id.clone(), Some(oid), elig));
        }
    }
    Ok(out)
}

/// Node `checkpointWal`.
pub fn checkpoint_wal(conn: &Connection) -> bool {
    match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get::<_, i64>(0)) {
        Ok(busy) if busy > 0 => {
            tracing::warn!("Oracle Tags WAL truncate deferred because a reader is active.");
            false
        }
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("Oracle Tags WAL checkpoint failed: {e}");
            false
        }
    }
}

#[derive(Debug)]
pub struct MaintenanceResult {
    pub tags: Result<TagsStatus, String>,
    pub backfill: Result<BackfillResult, String>,
}

/// Node `runMaintenance` (callers serialize via the jobs-thread guard).
pub async fn run_maintenance(conn: &Connection, http: &BulkHttp, scry: &Scry) -> MaintenanceResult {
    let now = now_ms();
    let tags = refresh_oracle_tags(conn, http, now, false).await;
    if let Err(e) = &tags {
        tracing::error!("Oracle Tags refresh failed: {e}");
    } else if let Ok(TagsStatus::Refreshed(r)) = &tags {
        tracing::info!("Oracle Tags refreshed: {} tags, {} assignments (generation {}).", r.tags, r.assignments, r.generation_id);
    }
    let backfill = backfill_oracle_ids(conn, http, scry, now, false).await;
    if let Err(e) = &backfill {
        tracing::error!("oracle_id backfill failed: {e}");
    }
    checkpoint_wal(conn);
    MaintenanceResult { tags, backfill }
}

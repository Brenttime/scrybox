//! Rust port of the backup job (Lane B unit 3b): `backend/src/backup.js` +
//! the three admin backup endpoints in `backend/src/routes/admin.js`. Gated
//! by `RUST_ROUTES=manabox_backup`.
//!
//! VACUUM INTO writes a consistent snapshot of the live DB (incl. WAL) to a
//! new file with no locking issues, even while the server is serving
//! requests. KEEP_LAST prunes the newest-N; the auto-backup timer mirrors
//! Node's setInterval twin (BACKUP_INTERVAL_HOURS, default 24, 0 disables).

use std::path::{Path, PathBuf};

use axum::{
    extract::{Path as AxPath, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};

use crate::auth::{authenticate, bearer_token};
use crate::collection_writes::{node_exact, parse_body, BodyOutcome};
use crate::mfx_payload::js_parse_int;
use crate::AppState;

use super::cards_tags::{json_error, json_response, read_body};

// ---------------------------------------------------------------------------
// backup.js
// ---------------------------------------------------------------------------

/// BACKUP_DIR = path.join(dirname(db.dbPath), 'backups').
pub fn backup_dir(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("backups")
}

/// KEEP_LAST = parseInt(process.env.BACKUP_KEEP_LAST, 10) || 10.
pub fn keep_last() -> i64 {
    js_parse_int(
        std::env::var("BACKUP_KEEP_LAST").ok().map(serde_json::Value::String).as_ref(),
    )
    .filter(|v| *v != 0)
    .unwrap_or(10)
}

#[derive(Clone, Debug)]
pub struct BackupMeta {
    pub file: String,
    pub size: u64,
    pub created_at: String,
}

fn js_iso(ms: i64) -> String {
    // new Date(ms).toISOString()
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| ((d.as_nanos() + 500_000) / 1_000_000) as i64)
        .unwrap_or(0)
}

/// listBackups(): readdir *.bak, stat, sort by created_at desc (JS
/// localeCompare on ISO strings == code-point order for this alphabet).
pub fn list_backups(dir: &Path) -> Vec<BackupMeta> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<BackupMeta> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".bak") {
                return None;
            }
            let st = std::fs::metadata(e.path()).ok()?;
            let ms = st
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| ((d.as_nanos() + 500_000) / 1_000_000) as i64)
                .unwrap_or(0);
            Some(BackupMeta { file: name, size: st.len(), created_at: js_iso(ms) })
        })
        .collect();
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out
}

/// createBackup(): mkdir -p, VACUUM INTO a stamped file, prune to KEEP_LAST,
/// return the new file's meta.
pub fn create_backup(db_path: &Path, conn: &rusqlite::Connection) -> Result<BackupMeta, String> {
    let dir = backup_dir(db_path);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // new Date().toISOString().replace(/[:.]/g, '-')
    let stamp = js_iso(now_ms()).replace([':', '.'], "-");
    // path.basename(db.dbPath, '.db') — strip one trailing .db.
    let base = db_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "db".to_string());
    let base = base.strip_suffix(".db").unwrap_or(&base).to_string();
    let dest = dir.join(format!("{base}.{stamp}.bak"));
    conn.execute("VACUUM INTO ?", rusqlite::params![dest.to_string_lossy().to_string()])
        .map_err(|e| e.to_string())?;
    let keep = keep_last();
    let all = list_backups(&dir);
    // listBackups().slice(KEEP_LAST): JS slice semantics (negative counts from
    // the end; clamp; nothing when >= len).
    let start = if keep < 0 {
        (all.len() as i64 + keep).max(0)
    } else {
        keep.min(all.len() as i64)
    } as usize;
    for b in all.iter().skip(start) {
        let _ = std::fs::remove_file(dir.join(&b.file));
    }
    let st = std::fs::metadata(&dest).map_err(|e| e.to_string())?;
    let ms = st
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| ((d.as_nanos() + 500_000) / 1_000_000) as i64)
        .unwrap_or(0);
    Ok(BackupMeta {
        file: dest.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        size: st.len(),
        created_at: js_iso(ms),
    })
}

/// startAutoBackup() twin: interval from BACKUP_INTERVAL_HOURS (default 24,
/// parseFloat semantics; 0/NaN/negative disables). Returns false when off.
pub fn spawn_auto_backup(st: &AppState) -> bool {
    let hours: f64 = match std::env::var("BACKUP_INTERVAL_HOURS") {
        Err(_) => 24.0,
        Ok(v) => js_parse_float_env(&v),
    };
    if !(hours > 0.0) {
        tracing::info!("Auto-backup disabled (BACKUP_INTERVAL_HOURS=0).");
        return false;
    }
    tracing::info!("Auto-backup enabled: every {hours}h, keeping newest {}.", keep_last());
    let st = st.clone();
    let Some(db_path) = st.cfg.db_path.clone() else {
        tracing::warn!("Auto-backup enabled but SCRYBOX_DB_PATH missing; timer not started");
        return false;
    };
    tokio::spawn(async move {
        // BACKUP_INTERVAL_HOURS=Infinity: Node's setInterval(Infinity) simply
        // never fires; cap instead of panicking Duration::from_secs_f64.
        let every = std::time::Duration::from_secs_f64((hours * 3600.0).min(1e15));
        loop {
            tokio::time::sleep(every).await;
            let st2 = st.clone();
            let path = db_path.clone();
            let res = tokio::task::spawn_blocking(move || {
                let conn = st2.db.as_ref().and_then(|p| p.get().ok());
                match conn {
                    Some(c) => create_backup(&path, &c).map(|b| b.file).map_err(|e| e),
                    None => Err("no db".to_string()),
                }
            })
            .await;
            match res {
                Ok(Ok(file)) => tracing::info!("Auto-backup created: {file}"),
                Ok(Err(e)) => tracing::error!("Auto-backup failed: {e}"),
                Err(e) => tracing::error!("Auto-backup failed: {e}"),
            }
        }
    });
    true
}

fn js_parse_float_env(s: &str) -> f64 {
    // parseFloat(trimmed numeric prefix); 'abc' -> NaN.
    let t = s.trim();
    let b = t.as_bytes();
    let mut end = 0usize;
    if end < b.len() && (b[end] == b'+' || b[end] == b'-') {
        end += 1;
    }
    let d0 = end;
    while end < b.len() && (b[end] as char).is_ascii_digit() {
        end += 1;
    }
    if end < b.len() && b[end] == b'.' {
        let dot = end;
        end += 1;
        let d1 = end;
        while end < b.len() && (b[end] as char).is_ascii_digit() {
            end += 1;
        }
        if end == d1 {
            end = dot;
        }
    }
    let numeric = end > d0 || end > 0;
    if numeric {
        if end > 0 {
            if let Ok(f) = t[..end].parse::<f64>() {
                return f;
            }
        }
        if let Ok(f) = t[..end.max(1)].parse::<f64>() {
            return f;
        }
    }
    if t.eq_ignore_ascii_case("infinity") || t.starts_with("Infinity") {
        return f64::INFINITY;
    }
    f64::NAN
}

// ---------------------------------------------------------------------------
// routes/admin.js — the three /api/admin/backups* handlers
// ---------------------------------------------------------------------------

fn err_body(msg: &str) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    m.insert("error".into(), serde_json::Value::String(msg.to_string()));
    serde_json::Value::Object(m)
}

/// router.use(authenticateToken, requireAdmin) behind the shared /api gate.
fn admin_gate(user: &crate::auth::AuthUser) -> Option<Response> {
    if user.via_api_key {
        return Some(json_error(
            StatusCode::FORBIDDEN,
            "API keys cannot access admin endpoints. Use a login session.",
        ));
    }
    if user.role.as_deref() != Some("admin") {
        return Some(json_error(
            StatusCode::FORBIDDEN,
            "Access denied. Administrator privileges required.",
        ));
    }
    None
}

fn owns_backup(st: &AppState) -> bool {
    st.owns("manabox_backup") && st.db.is_some() && st.cfg.db_path.is_some()
}

/// GET + POST /api/admin/backups — one path, two methods; anything else
/// proxies (Node's router 404s those and the SPA catch-all skips /api).
pub async fn handle_backups_collection(State(st): State<AppState>, req: Request) -> Response {
    let method = req.method().clone();
    if (!matches!(method, Method::GET | Method::POST)) || !owns_backup(&st) {
        return crate::proxy(State(st), req).await;
    }
    let (parts, bytes) = read_body(req).await;
    let headers = parts.headers.clone();
    if method == Method::POST {
        // normalJson runs BEFORE the admin router's auth in Node: parse
        // failures outrank auth, same ladder as the collection writes.
        match parse_body(&headers, &bytes) {
            BodyOutcome::TooLarge => {
                return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Upload too large. Try exporting/importing in smaller batches.")
            }
            BodyOutcome::Invalid => {
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
            }
            BodyOutcome::Parsed(_) => {}
        }
    }
    let token = bearer_token(&headers).map(|s| s.to_string());
    let is_get = method == Method::GET;
    let user = match authenticate(st.db.as_ref().unwrap(), token.as_deref(), is_get) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    if let Some(res) = admin_gate(&user) {
        return res;
    }
    let db_path = st.cfg.db_path.clone().unwrap();
    if is_get {
        let dir = backup_dir(&db_path);
        let backups: Vec<serde_json::Value> = list_backups(&dir)
            .into_iter()
            .map(|b| {
                let mut m = serde_json::Map::new();
                m.insert("file".into(), serde_json::Value::String(b.file));
                m.insert("size".into(), serde_json::Value::from(b.size));
                m.insert("created_at".into(), serde_json::Value::String(b.created_at));
                serde_json::Value::Object(m)
            })
            .collect();
        let mut body = serde_json::Map::new();
        body.insert("dir".into(), serde_json::Value::String(dir.to_string_lossy().to_string()));
        body.insert("backups".into(), serde_json::Value::Array(backups));
        return json_response(StatusCode::OK, &serde_json::Value::Object(body));
    }
    // POST createBackup
    let conn = match st.db.as_ref().unwrap().get() {
        Ok(c) => c,
        Err(_) => {
            tracing::error!("BACKUP ERROR: pool exhaustion");
            return backup_failed("pool exhaustion");
        }
    };
    match create_backup(&db_path, &conn) {
        Ok(meta) => {
            let mut m = serde_json::Map::new();
            m.insert("file".into(), serde_json::Value::String(meta.file));
            m.insert("size".into(), serde_json::Value::from(meta.size));
            m.insert("created_at".into(), serde_json::Value::String(meta.created_at));
            let resp = json_response(StatusCode::CREATED, &serde_json::Value::Object(m));
            crate::stats::on_proxied_write(&st, &Method::POST, "/api/admin/backups", &headers);
            resp
        }
        Err(e) => {
            tracing::error!("BACKUP ERROR: {e}");
            backup_failed(&e)
        }
    }
}

fn backup_failed(message: &str) -> Response {
    let mut m = serde_json::Map::new();
    m.insert("error".into(), serde_json::Value::String("Backup failed".to_string()));
    m.insert("message".into(), serde_json::Value::String(message.to_string()));
    json_response(StatusCode::INTERNAL_SERVER_ERROR, &serde_json::Value::Object(m))
}

/// GET /api/admin/backups/:file/download
pub async fn handle_backup_download(State(st): State<AppState>, AxPath(file): AxPath<String>, req: Request) -> Response {
    if req.method() != Method::GET || !owns_backup(&st) {
        return crate::proxy(State(st), req).await;
    }
    let (parts, _body) = req.into_parts();
    let headers = parts.headers;
    let token = bearer_token(&headers).map(|s| s.to_string());
    let user = match authenticate(st.db.as_ref().unwrap(), token.as_deref(), true) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    if let Some(res) = admin_gate(&user) {
        return res;
    }
    // path.basename strips any traversal; .bak whitelist + existence gate.
    let name = Path::new(&file)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // path.basename('.')/'/'-style inputs yield None/empty; also handle
    // trailing-slash names Node's basename would strip to ''.
    if !name.ends_with(".bak") {
        return json_response(StatusCode::NOT_FOUND, &err_body("Backup not found"));
    }
    let full = backup_dir(&st.cfg.db_path.clone().unwrap()).join(&name);
    if !full.exists() {
        return json_response(StatusCode::NOT_FOUND, &err_body("Backup not found"));
    }
    let bytes = match std::fs::read(&full) {
        Ok(b) => b,
        Err(_) => return json_response(StatusCode::NOT_FOUND, &err_body("Backup not found")),
    };
    // res.download(full): attachment disposition with the basename +
    // octet-stream (mime has no type for .bak), Content-Length by hyper.
    let cd = format!("attachment; filename=\"{name}\"");
    let mut resp = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"))],
        axum::body::Body::from(bytes),
    )
        .into_response();
    if let Ok(v) = HeaderValue::from_str(&cd) {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    resp
}

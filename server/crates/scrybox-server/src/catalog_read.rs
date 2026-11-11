//! Stateless catalog reads (Lane B unit 2b): `GET /api/admin/catalogs/languages`
//! and `GET /api/scan-sets` — the `catalog.listLanguages` + `catalog.setCounts`
//! + `cvScan.builtLangs` half of `catalog.js`/`cvScan.js`.
//!
//! The build-control routes (GET /catalogs, GET /catalogs/progress, POST
//! /catalogs/build, POST /catalogs/stop) stay on Node: the build engine keeps
//! its `current` job in the Node process (ONNX embed phase is Lane B unit 6),
//! so a Rust-served progress view would contradict a running Node build.
//! These two routes read only SQLite + the catalog meta files, so they are
//! deterministic across processes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use axum::{
    extract::{Request, State},
    response::Response,
};
use serde_json::{Map, Value};

use crate::auth::{authenticate, bearer_token};
use crate::cards_tags::{json_error, json_response, query_param};
use crate::collection_writes::node_exact;
use crate::AppState;

// ---------------------------------------------------------------------------
// utils/languages.js twin (shared/languages.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct LangEntry {
    code: String,
    name: String,
    scryfall: Option<String>,
}

fn load_languages(st: &AppState) -> Option<Vec<LangEntry>> {
    let file = st.shared_dir.as_ref()?.join("languages.json");
    let text = std::fs::read_to_string(file).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let arr = v.as_array()?;
    let out = arr
        .iter()
        .filter_map(|e| {
            Some(LangEntry {
                code: e.get("code")?.as_str()?.to_string(),
                name: e.get("name")?.as_str()?.to_string(),
                scryfall: e.get("scryfall").and_then(|s| s.as_str()).map(String::from),
            })
        })
        .collect::<Vec<_>>();
    // Node's require() would fail the module on a malformed table; treat an
    // empty/garbage parse as "cannot answer" and let the caller proxy.
    if out.is_empty() {
        return None;
    }
    Some(out)
}

fn resolve_lang<'a>(langs: &'a [LangEntry], input: &str) -> &'a LangEntry {
    // resolve(): trim + lowercase, then byCode, byName (name lowercased),
    // byProvider (Scryfall's own spelling); unknown falls back to the first
    // entry (English) rather than throwing.
    let lower = input.trim().to_lowercase();
    langs
        .iter()
        .find(|l| l.code == lower)
        .or_else(|| langs.iter().find(|l| l.name.to_lowercase() == lower))
        .or_else(|| {
            langs
                .iter()
                .find(|l| l.scryfall.as_deref().map(|s| s.to_lowercase()) == Some(lower.clone()))
        })
        .unwrap_or(&langs[0])
}

/// `languages.toName(input)` — display name; anything unknown is English.
fn to_name(langs: &[LangEntry], input: Option<&str>) -> String {
    resolve_lang(langs, input.unwrap_or("")).name.clone()
}

// ---------------------------------------------------------------------------
// catalog.js / cvScan.js file helpers (MODEL_DIR half)
// ---------------------------------------------------------------------------

fn model_dir(st: &AppState) -> Option<PathBuf> {
    st.model_dir.clone()
}

/// catalog.js/cvScan.js `suffix` — English keeps the bare filename.
fn suffix(lang: &str) -> String {
    if lang.is_empty() || lang == "en" || lang == "English" {
        String::new()
    } else {
        format!("-{}", lang.to_lowercase())
    }
}

fn local_bin(model: &Path, game: &str, lang: &str) -> PathBuf {
    model.join(format!("milo-{game}{}-local.bin", suffix(lang)))
}

fn local_meta(model: &Path, game: &str, lang: &str) -> PathBuf {
    model.join(format!("milo-{game}{}-local.json", suffix(lang)))
}

/// cvScan.hasLocal — a locally built catalog needs both halves.
fn has_local(model: &Path, game: &str, lang: &str) -> bool {
    local_bin(model, game, lang).exists() && local_meta(model, game, lang).exists()
}

/// cvScan.isBuilt — models present AND (this language, or the English
/// fallback, or the published .npz catalog).
fn is_built(model: &Path, game: &str, lang: &str) -> bool {
    if !model.join("cornelius.onnx").exists() || !model.join("milo.onnx").exists() {
        return false;
    }
    if has_local(model, game, lang) {
        return true;
    }
    has_local(model, game, "English") || model.join(format!("milo-{game}.npz")).exists()
}

/// catalog.embeddedIds — `new Set(JSON.parse(text).ids)` inside a try/catch:
/// parse OK with ids missing/null is an EMPTY set (still `local`), a string
/// iterates its chars, a non-iterable throws (=> None), parse failure => None.
fn embedded_ids(meta: &Path) -> Option<HashSet<String>> {
    let text = std::fs::read_to_string(meta).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    match v.get("ids") {
        None | Some(Value::Null) => Some(Default::default()),
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|x| match x {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => String::new(),
                })
                .collect(),
        ),
        Some(Value::String(s)) => Some(s.chars().map(|c| c.to_string()).collect()),
        Some(_) => None,
    }
}

/// catalog.listLanguages `built.rows` — `JSON.parse(text).ids.length`:
/// array -> len, string -> char count, missing/other -> null (throw).
fn meta_rows(meta: &Path) -> Option<usize> {
    let text = std::fs::read_to_string(meta).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    match v.get("ids") {
        Some(Value::Array(items)) => Some(items.len()),
        Some(Value::String(s)) => Some(s.chars().count()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// GET /api/admin/catalogs/languages
// ---------------------------------------------------------------------------

/// `router.use(authenticateToken, requireAdmin)` from routes/admin.js —
/// after the shared auth gate, an API key is refused on the admin surface
/// and the role must be admin.
fn admin_gate(user: &crate::auth::AuthUser) -> Option<Response> {
    if user.via_api_key {
        return Some(json_error(
            axum::http::StatusCode::FORBIDDEN,
            "API keys cannot access admin endpoints. Use a login session.",
        ));
    }
    if user.role.as_deref() != Some("admin") {
        return Some(json_error(
            axum::http::StatusCode::FORBIDDEN,
            "Access denied. Administrator privileges required.",
        ));
    }
    None
}

pub async fn handle_catalog_languages(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != axum::http::Method::GET || !st.owns("catalog_art") {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st.clone()), req).await;
    };
    // No languages table => the route cannot answer like Node; proxy.
    let Some(langs) = load_languages(&st) else {
        return crate::proxy(State(st.clone()), req).await;
    };
    // Node: catalog.listLanguages(String(req.query.game || 'mtg')) — but the
    // whole router sits behind authenticateToken + requireAdmin first.
    let headers = req.headers().clone();
    let query = req.uri().query().map(|s| s.to_string());
    let token = bearer_token(&headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), true) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    if let Some(res) = admin_gate(&user) {
        return res;
    }
    let game_raw = query_param(query.as_deref(), "game").unwrap_or_default();
    let game = if game_raw.is_empty() { "mtg".to_string() } else { game_raw };
    if game != "mtg" {
        return json_response(axum::http::StatusCode::OK, &Value::Object({
            let mut m = Map::new();
            m.insert("languages".into(), Value::Array(vec![]));
            m
        }));
    }
    let model = match model_dir(&st) {
        Some(m) => m,
        None => return crate::proxy(State(st.clone()), req).await,
    };

    let result: Result<Vec<Value>, String> = (|| -> Result<Vec<Value>, String> {
        let conn = pool.get().map_err(|e| format!("read pool: {e}"))?;
        // catalog.listLanguages / list share this aggregate.
        let mut stmt = conn
            .prepare_cached(
                "SELECT language, COUNT(*) cached, \
                 SUM(CASE WHEN image_url IS NOT NULL AND image_url != '' THEN 1 ELSE 0 END) withArt \
                 FROM card_cache GROUP BY language",
            )
            .map_err(|e| e.to_string())?;
        let rows: Vec<(Option<String>, i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        // `r.language || 'English'` — NULL and '' are both falsy in JS.
        let have: HashMap<String, (i64, i64)> = rows
            .into_iter()
            .map(|(l, c, a)| {
                let key = match l.as_deref() {
                    None | Some("") => "English".to_string(),
                    Some(s) => s.to_string(),
                };
                (key, (c, a))
            })
            .collect();

        let mut out = Vec::new();
        for l in &langs {
            if l.code == "en" {
                continue; // English is a row of its own on /catalogs
            }
            let name = to_name(&langs, Some(&l.code));
            let (cached, with_art) = have.get(&name).copied().unwrap_or((0, 0));
            let built = meta_rows(&local_meta(&model, "mtg", &name))
                .map(|rows| Value::Object({
                    let mut m = Map::new();
                    m.insert("rows".into(), Value::from(rows as i64));
                    m
                }));
            let mut o = Map::new();
            o.insert("game".into(), Value::String("mtg".into()));
            o.insert("lang".into(), Value::String(name));
            o.insert("code".into(), Value::String(l.code.clone()));
            o.insert("cached".into(), Value::from(cached));
            o.insert("withArt".into(), Value::from(with_art));
            o.insert("built".into(), built.unwrap_or(Value::Null));
            out.push(Value::Object(o));
        }
        Ok(out)
    })();

    match result {
        Ok(list) => {
            let mut m = Map::new();
            m.insert("languages".into(), Value::Array(list));
            json_response(axum::http::StatusCode::OK, &Value::Object(m))
        }
        Err(e) => {
            tracing::error!(error = %e, "catalog languages listing failed");
            json_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Could not list languages")
        }
    }
}

// ---------------------------------------------------------------------------
// GET /api/scan-sets  (routes/collection.js 1b, behind the global /api gate)
// ---------------------------------------------------------------------------

pub async fn handle_scan_sets(State(st): State<AppState>, req: Request) -> Response {
    if req.method() != axum::http::Method::GET || !st.owns("catalog_art") {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else {
        return crate::proxy(State(st.clone()), req).await;
    };
    let Some(langs) = load_languages(&st) else {
        return crate::proxy(State(st.clone()), req).await;
    };
    let Some(model) = model_dir(&st) else {
        return crate::proxy(State(st.clone()), req).await;
    };
    let headers = req.headers().clone();
    let query = req.uri().query().map(|s| s.to_string());
    let token = bearer_token(&headers).map(|s| s.to_string());
    let user = match authenticate(&pool, token.as_deref(), true) {
        Ok(u) => u,
        Err(res) => return node_exact(res).await,
    };
    let _ = user; // global gate only — scan-sets is not admin-only

    let lang = to_name(&langs, query_param(query.as_deref(), "lang").as_deref());

    let result: Result<Value, String> = (|| -> Result<Value, String> {
        let conn = pool.get().map_err(|e| format!("read pool: {e}"))?;
        // catalog.setCounts — one pass over card_cache, counts keyed by the
        // lowercased set code (what the scan filter matches on), insertion
        // order = row order (same unindexed scan Node performs).
        let embedded = embedded_ids(&local_meta(&model, "mtg", &lang));
        let mut stmt = conn
            .prepare_cached(
                "SELECT id, LOWER(set_id) sid FROM card_cache \
                 WHERE language = ?1 AND set_id IS NOT NULL AND set_id != '' \
                   AND image_url IS NOT NULL AND image_url != ''",
            )
            .map_err(|e| e.to_string())?;
        let rows: Vec<(String, String)> = stmt
            .query_map([&lang], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        let mut sets: Map<String, Value> = Map::new();
        for (id, sid) in rows {
            let e = sets
                .entry(sid)
                .or_insert_with(|| Value::Object({
                    let mut m = Map::new();
                    m.insert("cached".into(), Value::from(0i64));
                    m.insert("embedded".into(), Value::from(0i64));
                    m
                }));
            let m = e.as_object_mut().unwrap();
            m.insert("cached".into(), Value::from(m["cached"].as_i64().unwrap() + 1));
            if embedded.as_ref().is_some_and(|s| s.contains(&id)) {
                m.insert("embedded".into(), Value::from(m["embedded"].as_i64().unwrap() + 1));
            }
        }
        let local = embedded.is_some();
        let published = !local && is_built(&model, "mtg", &lang);

        // cvScan.builtLangs — languages with a catalog OF THEIR OWN.
        // English also answers from a published .npz, which is not a local
        // build, so it rides at the front when no English local pair exists.
        let names: Vec<String> = langs.iter().map(|l| l.name.clone()).collect();
        let mut built_langs: Vec<Value> = names
            .into_iter()
            .filter(|n| has_local(&model, "mtg", n))
            .map(Value::String)
            .collect();
        if !built_langs.iter().any(|v| v.as_str() == Some("English"))
            && model.join("milo-mtg.npz").exists()
        {
            built_langs.insert(0, Value::String("English".into()));
        }

        let mut o = Map::new();
        o.insert("game".into(), Value::String("mtg".into()));
        o.insert("lang".into(), Value::String(lang));
        o.insert("sets".into(), Value::Object(sets));
        o.insert("local".into(), Value::Bool(local));
        o.insert("published".into(), Value::Bool(published));
        o.insert("builtLangs".into(), Value::Array(built_langs));
        Ok(Value::Object(o))
    })();

    match result {
        Ok(v) => json_response(axum::http::StatusCode::OK, &v),
        Err(e) => {
            tracing::error!(error = %e, "scan-sets failed");
            json_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Could not read catalog set counts")
        }
    }
}

//! LANE A — iter-36: rules group (route switch `rules`).
//! Port of `backend/src/routes/rules.js`:
//!
//!   GET /api/rules            cached JSON (refetched weekly), ?refresh=1
//!   GET /api/rules/keywords   CR 701/702 keyword fold + mined reminder text
//!
//! Both sit behind the global `app.use('/api', authenticateToken)` mount
//! (server.js:454, routers mount at 480): the handlers self-check
//! group+method and forward everything else, so unknown shapes never 405
//! locally — Node's router 404s them, through the proxy.
//!
//! Byte-parity surface (BRIEF rule 1): `res.json` body shape + key ORDER.
//! The cache doc is `{fetched, crUrl, crVersion, commanderUrl, rules}`
//! (build() literal order, which is also the file's round-trip order); a
//! failed refetch with a good copy answers `{...mem, stale: err.message}` —
//! stale APPENDS unless the cached doc already had one (JS spread keeps the
//! original slot). Rule rows keep their build order: Commander rows first,
//! then Comprehensive (`{src,id,section,text}`, headers add `header:true`
//! last), then Glossary. Keywords answer `{fetched, crVersion, keywords}`
//! with rows `{name,id,kind,reminder,summary,cards,rules}`.
//!
//! Cache sharing: CACHE = RULES_CACHE env || dirname(DB)/mtg-rules.json —
//! the SAME bind-mounted file the Node container reads at
//! /app/database/mtg-rules.json, so both processes share one cache exactly
//! like two Node processes would (each with its own process `mem` memo,
//! each re-building independently once it goes >7 days old).
//!
//! Upstream legs run through the same axios-shaped error ladder the precons
//! port pinned (module-local instance, timeout 30000ms → 'timeout of
//! 30000ms exceeded', non-2xx → 'Request failed with status code N',
//! transport → '{url}: {err}'), plus axios' default redirect-following
//! (maxRedirects 5 → 'Exceeded maxRedirects...' on the 6th hop). URLs are
//! AppState fields snapshotted at boot from the real constants; tests point
//! them at a mock origin (same pattern as `mtgjson_base`).
//!
//! JS-semantics twins reused, never re-implemented: `precons::js_trim`
//! (String.prototype.trim), `decks_reads::root_collator` (localeCompare on
//! the keyword sort), `lists_reads::qs_get` ('refresh' in req.query, incl.
//! `refresh[k]=`/`refresh[]=` forms → key present), `collection::{body_etag,
//! etag_matches}` (Express default weak body ETag + conditional-GET 304).
//! Backreference-free scans replace the two JS regexes the regex crate
//! cannot express (`</\1>` close-tag matching in parseCommander is a manual
//! leftmost scan with the same semantics).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::Response,
};
use http_body_util::BodyExt;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use regex::Regex;
use serde_json::{Map, Value};

use crate::poison::LockExt;
use crate::auth::{authenticate, bearer_token};
use crate::collection::{body_etag, etag_matches};
use crate::decks_reads::root_collator;
use crate::lists_reads::{qs_get, QsVal};
use crate::precons::js_trim;
use crate::AppState;

/// rules.js MAX_AGE — refetch once Date.now() - fetched >= it.
const MAX_AGE: i64 = 7 * 86400 * 1000;
/// rules.js axios per-request timeout.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// axios follow-redirects default maxRedirects.
const MAX_REDIRECTS: usize = 5;

/// rules.js index page URL (hard-coded in the Node twin).
pub const DEFAULT_INDEX_URL: &str = "https://magic.wizards.com/en/rules";
/// rules.js mtgcommander.net rules page (hard-coded in the Node twin).
pub const DEFAULT_COMMANDER_URL: &str = "https://mtgcommander.net/index.php/rules/";
/// rules.js media.wizards.com link prefix the index page is scanned for.
pub const DEFAULT_CR_BASE: &str = "https://media.wizards.com/";

// ---------------------------------------------------------------------------
// HTTP: fetchText twin (axios get + responseType arraybuffer + decode)
// ---------------------------------------------------------------------------

type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, String>;

fn https_client() -> &'static HttpsClient {
    static C: OnceLock<HttpsClient> = OnceLock::new();
    C.get_or_init(|| {
        let mut https = HttpConnector::new();
        https.set_nodelay(true);
        https.enforce_http(false);
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .unwrap()
            .https_or_http()
            .enable_http1()
            .build();
        Client::builder(TokioExecutor::new()).build(https)
    })
}

/// `decode(buf)`: utf8 (Node's lossy toString), U+FFFD anywhere → latin1 of
/// the raw bytes, else strip a leading BOM.
pub fn decode(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.contains('\u{FFFD}') {
        bytes.iter().map(|b| *b as char).collect()
    } else {
        s.strip_prefix('\u{FEFF}').unwrap_or(&s).to_string()
    }
}

/// `fetchText(url)` — UA header, arraybuffer, 30 s timeout, redirect follow.
/// Err string === the axios rejection `err.message` the route templates.
async fn fetch_text(url: &str) -> Result<String, String> {
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let req = hyper::Request::get(&url)
            .header("user-agent", HeaderValue::from_static("Scrybox/1.0 (rules reference)"))
            .body(String::new())
            .map_err(|e| format!("{url}: {e}"))?;
        let res = tokio::time::timeout(FETCH_TIMEOUT, https_client().request(req))
            .await
            .map_err(|_| "timeout of 30000ms exceeded".to_string())?
            .map_err(|e| format!("{url}: {e}"))?;
        let status = res.status().as_u16();
        if (300..400).contains(&status) {
            // axios follows up to maxRedirects 3xx hops; the 6th throws.
            let loc = res
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let Some(loc) = loc else {
                // 3xx without Location: axios treats it like any non-2xx.
                return Err(format!("Request failed with status code {status}"));
            };
            url = match req_uri_join(&url, &loc) {
                Ok(u) => u,
                Err(e) => return Err(e),
            };
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(format!("Request failed with status code {status}"));
        }
        let body = collect_body(res, &url).await?;
        return Ok(decode(&body));
    }
    Err("Exceeded maxRedirects. Probably stuck in a redirect loop".to_string())
}

async fn collect_body(res: hyper::Response<hyper::body::Incoming>, url: &str) -> Result<Vec<u8>, String> {
    BodyExt::collect(res.into_body())
        .await
        .map(|b| b.to_bytes().to_vec())
        .map_err(|e| format!("{url}: {e}"))
}

/// Resolve a Location header against the request URL (absolute passthrough;
/// protocol-relative and root-relative get the origin prefix). Enough of
/// WHATWG URL for server-issued redirects; malformed targets produce the
/// axios request-error shape `{url}: {reason}`.
fn req_uri_join(base: &str, loc: &str) -> Result<String, String> {
    let loc = loc.trim();
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return Ok(loc.to_string());
    }
    let scheme_end = base.find("://").ok_or_else(|| format!("{base}: invalid base url"))?;
    let origin = &base[..scheme_end + 3 + base[scheme_end + 3..].find('/').map(|i| i + 1).unwrap_or(base.len())];
    if let Some(rest) = loc.strip_prefix("//") {
        let scheme = &base[..scheme_end];
        return Ok(format!("{scheme}//{rest}"));
    }
    if let Some(rest) = loc.strip_prefix('/') {
        return Ok(format!("{origin}{rest}"));
    }
    // relative to the base's directory
    let dir = match base.rfind('/') {
        Some(i) if i + 1 < base.len() => &base[..i + 1],
        _ => origin,
    };
    Ok(format!("{dir}{loc}"))
}

// ---------------------------------------------------------------------------
// parseCR / unescape / parseCommander (rules.js, byte-faithful folds)
// ---------------------------------------------------------------------------

fn rule_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // JS ^(\d{3}\.\d+[a-z]?)\.?\s+(.*)$ — \d/\s are the ASCII/ES sets:
    // [0-9] and WhiteSpace+LineTerminator (\u0085, \u00a0, \uFEFF …).
    R.get_or_init(|| Regex::new(r"^([0-9]{3}\.[0-9]+[a-z]?)\.?[\s\u{feff}]+(.*)$").unwrap())
}
fn sec3_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[0-9]{3}\.[\s\u{feff}]").unwrap())
}
fn sec1_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[0-9]\.[\s\u{feff}]").unwrap())
}
fn entry_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?s)<div class="entry-content[^>]*>(.*?)</article"#).unwrap())
}
fn open_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"<(h[1-6]|p|li)[^>]*>").unwrap())
}
fn tag_strip_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"<[^>]+>").unwrap())
}
fn ws_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[\s\u{feff}]+").unwrap())
}
fn dec_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"&#([0-9]+);").unwrap())
}
fn hex_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)&#x([0-9a-f]+);").unwrap())
}
fn general_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)^(General|Keyword)").unwrap())
}
fn head_id_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^70[12]\.[0-9]+$").unwrap())
}
fn digits8_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[0-9]{8}").unwrap())
}
fn rem_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?:^|\n)([A-Z][^\n(]{0,60}?) \(([^()]{8,400})\)").unwrap())
}

fn obj(pairs: &[(&str, Value)]) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    Value::Object(m)
}

/// JS String() over a JSON value (only the arms this module can observe).
fn js_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a.iter().map(|x| js_str(Some(x))).collect::<Vec<_>>().join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// `String(x)` / template formatting for a JS number (RangeError messages).
fn js_number_string(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
        return format!("{}", f as i64);
    }
    format!("{f}")
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `parseCR(txt)` — see rules.js. Returns rows in build order.
pub fn parse_cr(txt: &str) -> Vec<Value> {
    let normalized = txt.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<String> = normalized.split('\n').map(|l| js_trim(l).to_string()).collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.as_str() == "1. Game Concepts")
        .map(|(i, _)| i)
        .collect();
    let mut i = if starts.len() > 1 { starts[1] } else { 0 };
    let mut section = String::new();
    let mut gloss = false;
    let mut out: Vec<Value> = Vec::new();
    while i < lines.len() {
        let l = &lines[i];
        if l.is_empty() {
            i += 1;
            continue;
        }
        if l == "Glossary" {
            gloss = true;
            section = "Glossary".to_string();
            i += 1;
            continue;
        }
        if l == "Credits" && gloss {
            break;
        }
        if gloss {
            let mut body: Vec<String> = Vec::new();
            let mut j = i + 1;
            while j < lines.len() && !lines[j].is_empty() {
                body.push(lines[j].clone());
                j += 1;
            }
            out.push(obj(&[
                ("src", Value::String("Glossary".into())),
                ("id", Value::String(l.clone())),
                ("section", Value::String("Glossary".into())),
                ("text", Value::String(body.join(" "))),
            ]));
            i = j;
            continue;
        }
        if let Some(c) = rule_re().captures(l) {
            out.push(obj(&[
                ("src", Value::String("Comprehensive".into())),
                ("id", Value::String(c[1].to_string())),
                ("section", Value::String(section.clone())),
                ("text", Value::String(c[2].to_string())),
            ]));
        } else if sec3_re().is_match(l) {
            section = l.clone();
            let head = l.split('.').next().unwrap_or("").to_string();
            out.push(obj(&[
                ("src", Value::String("Comprehensive".into())),
                ("id", Value::String(head)),
                ("section", Value::String(l.clone())),
                ("text", Value::String(l.clone())),
                ("header", Value::Bool(true)),
            ]));
        } else if sec1_re().is_match(l) {
            section = l.clone();
        } else if l.starts_with("Example:") && !out.is_empty() {
            if let Some(last) = out.last_mut() {
                if let Some(t) = last.get_mut("text").and_then(|t| t.as_str()).map(|t| t.to_string()) {
                    last["text"] = Value::String(format!("{t}\n{l}"));
                }
            }
        }
        i += 1;
    }
    out
}

/// `String.fromCodePoint(n)` twin; Err is the RangeError message Node's
/// unescape callback would throw (propagated to the route's 502 text).
fn from_code_point(n: f64) -> Result<String, String> {
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
        return Err(format!("Invalid code point {}", js_number_string(n)));
    }
    let cp = n as u32;
    if cp > 0x10FFFF || (0xD800..=0xDFFF).contains(&cp) {
        return Err(format!("Invalid code point {}", js_number_string(n)));
    }
    Ok(char::from_u32(cp).map(|c| c.to_string()).unwrap_or_default())
}

/// `unescape(s)` — sequential replace chain, &amp; LAST (so '&amp;lt;'
/// stays literal, exactly like Node).
pub fn unescape(s: &str) -> Result<String, String> {
    // Numeric passes splice manually (the regex facade's replace closure
    // cannot fail; from_code_point can).
    let mut out = String::new();
    let mut last = 0usize;
    for c in dec_re().captures_iter(s) {
        let whole = c.get(0).unwrap();
        out.push_str(&s[last..whole.start()]);
        out.push_str(&from_code_point(js_number_f64(&c[1])?)?);
        last = whole.end();
    }
    out.push_str(&s[last..]);
    let src = out;
    let mut out = String::new();
    let mut last = 0usize;
    for c in hex_re().captures_iter(&src) {
        let whole = c.get(0).unwrap();
        out.push_str(&src[last..whole.start()]);
        // parseInt(n, 16)
        let mut acc: f64 = 0.0;
        for ch in c[1].chars() {
            let d = ch.to_digit(16).ok_or_else(|| "Invalid code point NaN".to_string())?;
            acc = acc * 16.0 + d as f64;
        }
        out.push_str(&from_code_point(acc)?);
        last = whole.end();
    }
    out.push_str(&src[last..]);
    let mut s2 = out;
    for (from, to) in [
        ("&nbsp;", " "),
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&rsquo;", "'"),
        ("&lsquo;", "'"),
        ("&rdquo;", "\""),
        ("&ldquo;", "\""),
        ("&ndash;", "-"),
        ("&mdash;", "-"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&amp;", "&"),
    ] {
        s2 = s2.replace(from, to);
    }
    Ok(s2)
}

fn js_number_f64(s: &str) -> Result<f64, String> {
    s.parse::<f64>().map_err(|_| "Invalid code point NaN".to_string())
}

/// `parseCommander(page)`. The JS scan `/<(h[1-6]|p|li)[^>]*>([\s\S]*?)<\/\1>/g`
/// uses a backreference the regex crate lacks; this scan reproduces its
/// leftmost-first semantics: leftmost open tag, first SAME-name close after
/// it; an open tag with no close advances the cursor by ONE and rescans.
pub fn parse_commander(page: &str) -> Result<Vec<Value>, String> {
    let body: &str = match entry_re().captures(page) {
        Some(c) => c.get(1).map(|m| m.as_str()).unwrap_or(page),
        None => page,
    };
    let mut out: Vec<Value> = Vec::new();
    let mut section = "Commander".to_string();
    let mut n: i64 = 0;
    let mut pos = 0usize;
    loop {
        let Some(caps) = open_re().captures(&body[pos..]) else { break };
        let m = caps.get(0).unwrap();
        let s = pos + m.start();
        let tag_end = pos + m.end();
        let tag = caps.get(1).map(|g| g.as_str().to_string()).unwrap_or_default();
        let close = format!("</{tag}>");
        match body[tag_end..].find(&close) {
            Some(rel) => {
                let ce = tag_end + rel;
                let content = &body[tag_end..ce];
                let stripped = tag_strip_re().replace_all(content, "");
                let text = js_trim(&ws_re().replace_all(&unescape(&stripped)?, " ")).to_string();
                if !text.is_empty() {
                    if tag.starts_with('h') {
                        section = text;
                    } else {
                        n += 1;
                        out.push(obj(&[
                            ("src", Value::String("Commander".into())),
                            ("id", Value::String(format!("CMD {n}"))),
                            ("section", Value::String(section.clone())),
                            ("text", Value::String(text)),
                        ]));
                    }
                }
                pos = ce + close.len();
            }
            None => pos = s + 1,
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// build / getRules (rules.js module state)
// ---------------------------------------------------------------------------

use crate::util::now_ms;

fn cr_link_re(base: &str) -> Regex {
    // JS /https:\/\/media\.wizards\.com\/[^"']*?\.txt/g with the base swapped
    // for the AppState field (test hook); leftmost-first, non-overlapping.
    Regex::new(&format!(r#"{}[^"']*?\.txt"#, regex::escape(base))).unwrap()
}

/// `build()` — the three live legs + sanity gate. Err = `err.message`.
pub async fn build(st: &AppState) -> Result<Value, String> {
    let idx = fetch_text(&st.rules_index_url).await?;
    let urls: Vec<String> = cr_link_re(&st.rules_cr_base)
        .find_iter(&idx)
        .map(|m| m.as_str().to_string())
        .collect();
    if urls.is_empty() {
        return Err("could not find Comprehensive Rules .txt link".to_string());
    }
    let cr_url = urls[0].replace(' ', "%20");
    let cr_txt = fetch_text(&cr_url).await?;
    let cr = parse_cr(&cr_txt);
    let cmd_txt = fetch_text(&st.rules_commander_url).await?;
    let cmd = parse_commander(&cmd_txt)?;
    if cr.len() < 1000 || cmd.len() < 5 {
        return Err(format!("parse sanity failed cr={} cmd={}", cr.len(), cmd.len()));
    }
    let ver = digits8_re().find(&urls[0]).map(|m| m.as_str().to_string()).unwrap_or_default();
    let mut rules: Vec<Value> = cmd;
    rules.extend(cr);
    let mut m = Map::new();
    m.insert("fetched".into(), Value::Number(now_ms().into()));
    m.insert("crUrl".into(), Value::String(cr_url));
    m.insert("crVersion".into(), Value::String(ver));
    m.insert("commanderUrl".into(), Value::String(st.rules_commander_url.clone()));
    m.insert("rules".into(), Value::Array(rules));
    Ok(Value::Object(m))
}

/// `Date.now() - mem.fetched < MAX_AGE` with JS numeric coercion of the
/// stored field (anything non-numeric → NaN → false).
fn fresh(m: &Value) -> bool {
    let fetched = match m.get("fetched") {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => {
            let t = js_trim(s);
            if t.is_empty() {
                Some(0.0)
            } else {
                t.parse::<f64>().ok()
            }
        }
        Some(Value::Bool(true)) => Some(1.0),
        Some(Value::Bool(false)) | Some(Value::Null) => Some(0.0),
        _ => None,
    };
    match fetched {
        Some(f) => (now_ms() as f64) - f < MAX_AGE as f64,
        None => false,
    }
}

/// `readFileSync(CACHE,'utf8') + JSON.parse` in a try/catch → mem|null.
fn read_cache(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    let raw = String::from_utf8_lossy(&bytes);
    serde_json::from_str::<Value>(&raw).ok()
}

/// `getRules(force)`. On success mem + CACHE file are updated (the file is
/// the shared bind-mounted copy; Node's twin writeFileSync's the same file
/// non-atomically — we write bytes in one call, best-effort like its
/// try/catch). On failure with a good copy: `{...mem, stale: err.message}`.
pub async fn get_rules(st: &AppState, force: bool) -> Result<Value, String> {
    {
        let mut mem = st.rules_mem.guard();
        if mem.is_none() {
            *mem = read_cache(&st.rules_cache_path);
        }
        if !force {
            if let Some(m) = mem.as_ref() {
                if fresh(m) {
                    return Ok(m.clone());
                }
            }
        }
    }
    // inflight twin: concurrent stale callers share ONE build. The lock makes
    // that explicit; after taking it, a non-force caller whose mem was just
    // refreshed by the previous awaiter returns without a second fetch
    // (Node: it was awaiting the SAME promise). A force caller still
    // rebuilds — Node's refresh always creates a fresh build once inflight
    // has cleared.
    let _g = st.rules_build.lock().await;
    if !force {
        let mem = st.rules_mem.guard();
        if let Some(m) = mem.as_ref() {
            if fresh(m) {
                return Ok(m.clone());
            }
        }
    }
    match build(st).await {
        Ok(built) => {
            *st.rules_mem.guard() = Some(built.clone());
            let _ = std::fs::write(&st.rules_cache_path, serde_json::to_vec(&built).unwrap_or_default());
            Ok(built)
        }
        Err(msg) => {
            let mem = st.rules_mem.guard().clone();
            match mem {
                Some(m) => {
                    let mut map = m.as_object().cloned().unwrap_or_default();
                    map.insert("stale".into(), Value::String(msg));
                    Ok(Value::Object(map))
                }
                None => Err(msg),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// keyword mining (rules.js mineReminders / buildKeywords)
// ---------------------------------------------------------------------------

/// `norm = s => s.toLowerCase().replace(/[’']/g, "'").trim()`
fn norm(s: &str) -> String {
    js_trim(&s.to_lowercase().replace(['’', '\''], "'")).to_string()
}

fn split_words(s: &str) -> Vec<&str> {
    s.split(|c: char| c.is_whitespace() || c == '\u{feff}')
        .filter(|w| !w.is_empty())
        .collect()
}

/// `mineReminders(names)` — the card_cache LIKE '%(%' scan. Returns
/// key -> (winning reminder, card count). The win is
/// `[...m.entries()].sort((a,b)=>b[1]-a[1])[0][0]`: descending count, STABLE,
/// so among ties the FIRST-OBSERVED reminder wins (Vec keeps insertion).
pub fn mine_reminders(st: &AppState, names: &[String]) -> Result<HashMap<String, (String, i64)>, String> {
    let pool = st.db.as_ref().ok_or_else(|| "SqliteError: no database configured".to_string())?;
    let want: HashSet<String> = names.iter().map(|n| norm(n)).collect();
    let mut tally: Vec<(String, Vec<(String, i64)>)> = Vec::new();
    let mut tally_idx: HashMap<String, usize> = HashMap::new();
    let mut cards: HashMap<String, i64> = HashMap::new();
    let conn = pool.get().map_err(|e| format!("SqliteError: {e}"))?;
    let mut stmt = conn
        .prepare("SELECT oracle_text FROM card_cache WHERE oracle_text LIKE '%(%'")
        .map_err(|e| format!("SqliteError: {e}"))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, Option<String>>(0))
        .map_err(|e| format!("SqliteError: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("SqliteError: {e}"))?;
    for row in rows {
        // LIKE '%(%' can never return NULL; a None is skipped rather than
        // reproducing Node's null.matchAll TypeError (unreachable state).
        let Some(t) = row else { continue };
        let mut seen: HashSet<String> = HashSet::new();
        for caps in rem_re().captures_iter(&t) {
            let pre = norm(&caps[1]).replace(['—', '–', '-'], " ");
            let words = split_words(&pre);
            let mut key = String::new();
            for n in (1..=words.len().min(5)).rev() {
                let k = words[..n].join(" ");
                if want.contains(&k) {
                    key = k;
                    break;
                }
            }
            if key.is_empty() {
                continue;
            }
            let rem = caps[2].to_string();
            let idx = *tally_idx.entry(key.clone()).or_insert_with(|| {
                tally.push((key.clone(), Vec::new()));
                tally.len() - 1
            });
            let entries = &mut tally[idx].1;
            match entries.iter_mut().find(|(r, _)| *r == rem) {
                Some((_, c)) => *c += 1,
                None => entries.push((rem, 1)),
            }
            if seen.insert(key.clone()) {
                *cards.entry(key).or_insert(0) += 1;
            }
        }
    }
    let mut out = HashMap::new();
    for (key, mut entries) in tally {
        entries.sort_by(|a, b| b.1.cmp(&a.1)); // stable, ties keep first-observed
        let reminder = entries[0].0.clone();
        let n_cards = *cards.get(&key).unwrap_or(&0);
        out.insert(key, (reminder, n_cards));
    }
    Ok(out)
}

/// `buildKeywords(data)` — head filter, per-head sub-rules, mined reminder
/// fold, localeCompare sort. `data` is the mem object getRules answered with
/// (Node's twin re-enters getRules(false), which returns that same object).
pub fn build_keywords(st: &AppState, data: &Value) -> Result<Value, String> {
    let rules = data
        .get("rules")
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("Cannot read properties of {} (reading 'filter')", js_typeof(data.get("rules"))))?;
    let cr: Vec<&Value> = rules
        .iter()
        .filter(|r| r.get("src").and_then(|s| s.as_str()) == Some("Comprehensive"))
        .collect();
    let heads: Vec<&Value> = cr
        .iter()
        .copied()
        .filter(|r| {
            head_id_re().is_match(&js_str(r.get("id")))
                && !matches!(r.get("id"), Some(Value::String(s)) if s == "701.1" || s == "702.1")
                && match r.get("text") {
                    Some(Value::String(t)) => utf16_len(t) < 60 && !general_re().is_match(t),
                    // .length on a number/object is undefined → gate false;
                    // missing text throws like Node.
                    Some(_) => false,
                    None => panic_unless(false),
                }
        })
        .collect();
    let names: Vec<String> = heads.iter().map(|h| js_str(h.get("text"))).collect();
    let mined = mine_reminders(st, &names)?;
    let mut keywords: Vec<Value> = Vec::new();
    for h in &heads {
        let hid = js_str(h.get("id"));
        let sub_re = Regex::new(&format!("^{}[a-z]$", regex::escape(&hid))).unwrap();
        let sub: Vec<Value> = cr
            .iter()
            .filter(|r| sub_re.is_match(&js_str(r.get("id"))))
            .map(|r| {
                obj(&[
                    ("id", r.get("id").cloned().unwrap_or(Value::Null)),
                    ("text", r.get("text").cloned().unwrap_or(Value::String(String::new()))),
                ])
            })
            .collect();
        let name = js_str(h.get("text"));
        let m = mined.get(&norm(&name));
        obj_push(
            &mut keywords,
            &[
                ("name", Value::String(name.clone())),
                ("id", h.get("id").cloned().unwrap_or(Value::Null)),
                (
                    "kind",
                    Value::String(if hid.starts_with("701") { "action".to_string() } else { "ability".to_string() }),
                ),
                (
                    "reminder",
                    match m.map(|(r, _)| r.clone()).filter(|r| !r.is_empty()) {
                        Some(r) => Value::String(r),
                        None => Value::Null,
                    },
                ),
                (
                    "summary",
                    sub.first()
                        .map(|s| s.get("text").cloned().unwrap_or(Value::String(String::new())))
                        .unwrap_or(Value::String(String::new())),
                ),
                ("cards", Value::Number(m.map(|(_, c)| *c).unwrap_or(0).into())),
                ("rules", Value::Array(sub)),
            ],
        );
    }
    let coll = root_collator();
    keywords.sort_by(|a, b| {
        let an = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let bn = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        coll.compare(an, bn).into()
    });
    let mut m = Map::new();
    m.insert("fetched".into(), data.get("fetched").cloned().unwrap_or(Value::Null));
    m.insert("crVersion".into(), data.get("crVersion").cloned().unwrap_or(Value::Null));
    m.insert("keywords".into(), Value::Array(keywords));
    Ok(Value::Object(m))
}

fn obj_push(v: &mut Vec<Value>, pairs: &[(&str, Value)]) {
    let mut m = Map::new();
    for (k, val) in pairs {
        m.insert((*k).to_string(), val.clone());
    }
    v.push(Value::Object(m));
}

fn js_typeof(v: Option<&Value>) -> &'static str {
    match v {
        None | Some(Value::Null) => "undefined_or_object_placeholder",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) | Some(Value::Object(_)) => "object",
    }
}

fn panic_unless(_c: bool) -> bool {
    // Node throws TypeError reading 'length' of undefined inside the heads
    // filter — unreachable with our cache docs (every row carries text).
    false
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// npm `fresh` twin (same helper shared.rs pinned): a request Cache-Control
/// with the standalone `no-cache` directive disables the ETag 304.
fn fresh_allows_304(headers: &HeaderMap) -> bool {
    match headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()) {
        Some(cc) => !cc.split(',').any(|d| d.trim() == "no-cache"),
        None => true,
    }
}

fn json_ok(raw: Vec<u8>, inm: Option<&str>, cache_control: bool) -> Response {
    let tag = body_etag(&raw);
    if cache_control && etag_matches(inm, &tag) {
        // Express res.send conditional GET: 304 + the tag, no entity headers.
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, tag)
            .body(Body::empty())
            .unwrap();
    }
    let mut b = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::ETAG, tag);
    if cache_control {
        b = b.header(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=3600"));
    }
    b.body(Body::from(raw)).unwrap()
}

fn json_error(status: StatusCode, msg: &str, cache_control: bool) -> Response {
    let mut m = Map::new();
    m.insert("error".into(), Value::String(msg.to_string()));
    let raw = serde_json::to_vec(&Value::Object(m)).unwrap();
    // res.status(N).json still runs res.send — Express stamps its default
    // body ETag on error bodies too.
    let tag = body_etag(&raw);
    let mut b = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::ETAG, tag);
    if cache_control {
        b = b.header(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=3600"));
    }
    b.body(Body::from(raw)).unwrap()
}

fn has_refresh(query: Option<&str>) -> bool {
    // `'refresh' in req.query` — scalar, array (refresh[], refresh[0]) and
    // object (refresh[k]) forms all key the presence.
    !matches!(qs_get(query, "refresh"), QsVal::Missing)
}

fn inm_header(headers: &HeaderMap) -> Option<String> {
    headers.get("if-none-match").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
}

/// GET /api/rules — routes/rules.js GET /.
pub async fn route_rules(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("rules") || req.method() != Method::GET {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let headers = req.headers().clone();
    let query = req.uri().query().map(str::to_string);
    let token = bearer_token(&headers).map(str::to_string);
    if let Err(res) = authenticate(&pool, token.as_deref(), true) {
        return res;
    }
    let force = has_refresh(query.as_deref());
    // Node order: res.set(Cache-Control) runs BEFORE the awaited getRules,
    // so even the 502 carries it.
    let inm = inm_header(&headers).filter(|_| !force && fresh_allows_304(&headers));
    match get_rules(&st, force).await {
        Ok(v) => json_ok(serde_json::to_vec(&v).unwrap(), inm.as_deref(), true),
        Err(msg) => json_error(
            StatusCode::BAD_GATEWAY,
            &format!("Could not load rules: {msg}"),
            true,
        ),
    }
}

/// GET /api/rules/keywords — routes/rules.js GET /keywords.
pub async fn route_keywords(State(st): State<AppState>, req: Request) -> Response {
    if !st.owns("rules") || req.method() != Method::GET {
        return crate::proxy(State(st), req).await;
    }
    let Some(pool) = st.db.clone() else { return crate::proxy(State(st), req).await };
    let headers = req.headers().clone();
    let query = req.uri().query().map(str::to_string);
    let token = bearer_token(&headers).map(str::to_string);
    if let Err(res) = authenticate(&pool, token.as_deref(), true) {
        return res;
    }
    let force = has_refresh(query.as_deref());
    let data = match get_rules(&st, false).await {
        Ok(d) => d,
        // Throws BEFORE res.set runs on the Node side → no Cache-Control.
        Err(msg) => return json_error(
            StatusCode::BAD_GATEWAY,
            &format!("Could not load keywords: {msg}"),
            false,
        ),
    };
    let need = {
        let kw = st.kw_mem.guard();
        !force
            && kw.as_ref().map(|k| k.get("fetched") == data.get("fetched")).unwrap_or(false)
    };
    let kw = if need {
        st.kw_mem.guard().clone().unwrap()
    } else {
        match build_keywords(&st, &data) {
            Ok(k) => {
                *st.kw_mem.guard() = Some(k.clone());
                k
            }
            // buildKeywords threw AFTER res.set — the 502 carries the header.
            Err(msg) => return json_error(
                StatusCode::BAD_GATEWAY,
                &format!("Could not load keywords: {msg}"),
                true,
            ),
        }
    };
    let inm = inm_header(&headers).filter(|_| !force && fresh_allows_304(&headers));
    json_ok(serde_json::to_vec(&kw).unwrap(), inm.as_deref(), true)
}

/// rules.js CACHE default: `__dirname/../../../database/mtg-rules.json` →
/// inside the container that is /app/database — dirname(DB) for the host
/// twin (same bind-mounted file, precons_index_path pattern).
pub fn default_cache_path(db_path: Option<&PathBuf>) -> PathBuf {
    db_path
        .and_then(|p| p.parent())
        .map(|d| d.join("mtg-rules.json"))
        .unwrap_or_else(|| PathBuf::from("mtg-rules.json"))
}

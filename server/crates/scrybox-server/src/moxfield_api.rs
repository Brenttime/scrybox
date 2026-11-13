//! Rust port of `backend/src/moxfieldApi.js` (Lane B unit 3a).
//!
//! Moxfield front-ends its own JSON API behind a Cloudflare edge that
//! fingerprint-blocks non-browser TLS stacks — Node's axios AND plain
//! `curl --tlsv1.2` both get 403 `fingerprint_blocked`. The proven path is a
//! Chrome-shaped TLS handshake: exec `/usr/local/bin/curl-impersonate` when
//! the image ships it, else system `curl` (a recent LibreSSL/BoringSSL curl
//! passes for the read-only endpoints). Everything here is GET-only, per the
//! lane rule: the sync only ever reads.
//!
//! Byte-parity notes (moxfieldApi.js is the spec):
//! - Endpoints: getUser → GET /v2/users/search-sfw; deck summaries → GET
//!   /v2/decks/search-sfw with the exact param order/spellings of lines
//!   145-155 (sortType=Updated, fmt=, includePinned=true, showIllegal=true).
//! - Headers exactly Node's set AND insertion order (Mac Chrome/124 UA,
//!   sec-ch-ua trio, Referer, Accept).
//! - TLS_ARGS is Node's two flags only, and only passed when the chosen
//!   binary's name contains `curl-impersonate` (Node's rule).
//! - execFile semantics: maxBuffer 10MB stdout cap, kill at (30+5)s, and a
//!   non-zero curl exit is an error (`Could not reach Moxfield: …`, status 0)
//!   even though curl without -f exits 0 on HTTP errors only.
//! - 404/403 reject before the 2xx check; a non-JSON body branches on
//!   403/404/other; an empty body resolves to `{}`.

use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;

use crate::mfx_payload::js_parse_int;

pub const BASE_URL: &str = "https://api2.moxfield.com";
const TIMEOUT_S: u64 = 30;

/// HEADERS from moxfieldApi.js:40-47 — order matters (curl emits -H in argv
/// order and Cloudflare fingerprints header order).
const REQUEST_HEADERS: [(&str, &str); 6] = [
    ("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36"),
    ("sec-ch-ua", "\"Chromium\";v=\"124\", \"Google Chrome\";v=\"124\", \"Not-A.Brand\";v=\"99\""),
    ("sec-ch-ua-mobile", "?0"),
    ("sec-ch-ua-platform", "\"macOS\""),
    ("Referer", "https://www.moxfield.com/"),
    ("Accept", "application/json"),
];

/// Chrome 124 ciphersuites and key groups, as curl-impersonate's
/// curl_chrome124 wrapper passes them — the exact two flags of
/// moxfieldApi.js:33-38.
const TLS_ARGS: [&str; 4] = [
    "--ciphers",
    "TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:ECDHE-RSA-AES128-SHA:ECDHE-RSA-AES256-SHA:AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA:AES256-SHA",
    "--curves",
    "X25519Kyber768Draft00:X25519:P-256:P-384",
];

const BLOCKED_403: &str = "Moxfield blocked the request (rate limit or Cloudflare). It usually clears on its own; try again in a minute.";
const NOT_FOUND_404: &str = "Moxfield returned 404 (deck or user not found)";

/// execFile({ maxBuffer }) — stdout beyond 10MB is an execFile error.
const MAX_STDOUT_BYTES: usize = 10 * 1024 * 1024;
/// Only the first 200 chars of stderr are ever used.
const MAX_STDERR_BYTES: usize = 4096;

/// A failed Moxfield call, with the HTTP status the sync maps to a route
/// response (0 for transport/parse failures with no status, like Node's
/// `error.status || 0`).
#[derive(Clone, Debug)]
pub struct MoxfieldError {
    pub message: String,
    pub status: u16,
}

impl MoxfieldError {
    fn new(message: impl Into<String>, status: u16) -> Self {
        Self { message: message.into(), status }
    }
}

/// Where the curl binary and the API base come from. `from_env` mirrors the
/// Node module's require-time constants: MOXFIELD_API_BASE (tests), then
/// MOXFIELD_CURL_BIN, /usr/local/bin/curl-impersonate, and the repo-bundled
/// `bin/curl-impersonate` (Node's `__dirname/../..` = repo root; the crate
/// dir is `<repo>/server/crates/scrybox-server`), else plain `curl`.
#[derive(Clone, Debug)]
pub struct MfxApi {
    pub base_url: String,
    pub curl_bin: String,
}

impl MfxApi {
    /// One instance per process, like Node's module-level CURL_BIN/BASE_URL
    /// captured at require time.
    pub fn global() -> &'static MfxApi {
        static G: std::sync::OnceLock<MfxApi> = std::sync::OnceLock::new();
        G.get_or_init(MfxApi::from_env)
    }

    pub fn from_env() -> Self {
        let base_url = std::env::var("MOXFIELD_API_BASE")
            .unwrap_or_else(|_| BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let repo_bin = format!(
            "{}/../../../bin/curl-impersonate",
            env!("CARGO_MANIFEST_DIR")
        );
        let candidates: Vec<String> = std::env::var("MOXFIELD_CURL_BIN")
            .ok()
            .filter(|b| !b.is_empty())
            .into_iter()
            .chain([
                "/usr/local/bin/curl-impersonate".to_string(),
                repo_bin,
            ])
            .collect();
        let curl_bin = candidates
            .iter()
            .find(|p| std::path::Path::new(p).exists())
            .cloned()
            .unwrap_or_else(|| "curl".to_string());
        Self { base_url, curl_bin }
    }

    fn uses_impersonate(&self) -> bool {
        self.curl_bin.contains("curl-impersonate")
    }

    /// GET `path` with `params` (insertion order, URLSearchParams-style
    /// encoding) and return the parsed JSON body. Mirrors httpGet() lines
    /// 76-114 including the reject order (404, then 403, then 2xx, else).
    pub async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value, MoxfieldError> {
        let url = self.build_url(path, params);
        let mut args: Vec<String> = vec![
            "-sS".into(),
            "-X".into(),
            "GET".into(),
            "--max-time".into(),
            TIMEOUT_S.to_string(),
            "-w".into(),
            "\n%{http_code}".into(),
        ];
        if self.uses_impersonate() {
            args.extend(TLS_ARGS.iter().map(|s| s.to_string()));
        }
        for (k, v) in REQUEST_HEADERS {
            args.push("-H".into());
            args.push(format!("{k}: {v}"));
        }
        args.push(url);

        // execFile(CURL_BIN, args, { maxBuffer: 10MB, timeout: 35s }).
        let spawned = tokio::process::Command::new(&self.curl_bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let (stdout, stderr, exec_err) = match spawned {
            Ok(mut child) => {
                // Stream both pipes with caps: stdout over maxBuffer kills the
                // child right away (Node's execFile), stderr keeps 4 KB.
                use tokio::io::AsyncReadExt;
                let mut out_pipe = child.stdout.take();
                let mut err_pipe = child.stderr.take();
                let read_out = async {
                    let mut buf = Vec::new();
                    if let Some(p) = out_pipe.as_mut() {
                        let _ = p.take(MAX_STDOUT_BYTES as u64 + 1).read_to_end(&mut buf).await;
                    }
                    buf
                };
                let read_err = async {
                    let mut buf = Vec::new();
                    if let Some(p) = err_pipe.as_mut() {
                        let mut chunk = [0u8; 4096];
                        loop {
                            match p.read(&mut chunk).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    let room = MAX_STDERR_BYTES.saturating_sub(buf.len());
                                    buf.extend_from_slice(&chunk[..n.min(room)]);
                                }
                            }
                        }
                    }
                    buf
                };
                let waited = tokio::time::timeout(Duration::from_secs(TIMEOUT_S + 5), async {
                    let out_fut = read_out;
                    tokio::pin!(out_fut);
                    let err_fut = read_err;
                    tokio::pin!(err_fut);
                    let mut err_buf: Option<Vec<u8>> = None;
                    let out = loop {
                        tokio::select! {
                            o = &mut out_fut => break o,
                            e = &mut err_fut, if err_buf.is_none() => err_buf = Some(e),
                        }
                    };
                    if out.len() > MAX_STDOUT_BYTES {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        return Ok((out, Vec::new(), None));
                    }
                    let err = match err_buf {
                        Some(e) => e,
                        None => err_fut.await,
                    };
                    child.wait().await.map(|st| (out, err, Some(st)))
                })
                .await;
                match waited {
                    Ok(Ok((out, err, status))) => {
                        let stderr = String::from_utf8_lossy(&err).to_string();
                        match status {
                            // maxBuffer exceeded → err, stderr empty in Node.
                            None => (String::new(), String::new(), Some("maxBuffer exceeded".to_string())),
                            Some(st) if !st.success() => (
                                // execFile errors on non-zero exit codes.
                                String::new(),
                                stderr,
                                Some(format!("Command failed: {} {}", self.curl_bin, args.join(" "))),
                            ),
                            Some(_) => (String::from_utf8_lossy(&out).to_string(), stderr, None),
                        }
                    }
                    Ok(Err(e)) => (String::new(), String::new(), Some(e.to_string())),
                    Err(_) => {
                        // timeout: execFile kills the child; err message form
                        let _ = child.start_kill();
                        (String::new(), String::new(), Some("spawnSync /bin/sh ETIMEDOUT".to_string()))
                    }
                }
            }
            Err(e) => (String::new(), String::new(), Some(e.to_string())),
        };
        if let Some(err) = exec_err {
            return Err(MoxfieldError::new(
                format!("Could not reach Moxfield: {}", truncate200(&stderr, &err)),
                0,
            ));
        }

        // lastIndexOf('\n') semantics exactly: body = everything before the
        // LAST newline (slice(0, -1) when there is none — curl's -w always
        // appends one, but Node's slice behaviour is mirrored regardless),
        // status = parseInt(after, 10) || 0 (NaN → 0).
        let (body, status_src) = match stdout.rfind('\n') {
            Some(nl) => (&stdout[..nl], &stdout[nl + 1..]),
            None => (&stdout[..stdout.len().saturating_sub(1)], &stdout[..]),
        };
        let status: u16 = js_parse_int(Some(&Value::String(status_src.to_string())))
            .filter(|n| (0..=u16::MAX as i64).contains(n))
            .map(|n| n as u16)
            .unwrap_or(0);

        let data: Result<Value, ()> = if body.is_empty() {
            // `data = body ? JSON.parse(body) : {}` — empty resolves to {}.
            Ok(Value::Object(serde_json::Map::new()))
        } else {
            serde_json::from_str(body).map_err(|_| ())
        };
        match data {
            Ok(v) => {
                // Reject order of Node lines 108-111: 404, then 403, then
                // 2xx, else. (Later rejects on an already-settled promise are
                // no-ops in Node, so first-match wins here too.)
                if status == 404 {
                    Err(MoxfieldError::new(NOT_FOUND_404, 404))
                } else if status == 403 {
                    Err(MoxfieldError::new(BLOCKED_403, 403))
                } else if (200..300).contains(&status) {
                    Ok(v)
                } else {
                    Err(MoxfieldError::new(
                        format!("Moxfield request failed with HTTP {status}"),
                        status,
                    ))
                }
            }
            Err(_) => {
                // Non-JSON body: challenge/interstitial classification.
                if status == 403 {
                    Err(MoxfieldError::new(BLOCKED_403, 403))
                } else if status == 404 {
                    Err(MoxfieldError::new(NOT_FOUND_404, 404))
                } else {
                    Err(MoxfieldError::new(
                        format!("Moxfield returned non-JSON (HTTP {status})"),
                        status,
                    ))
                }
            }
        }
    }

    /// `new URL(path, BASE_URL)` + searchParams.set in insertion order.
    fn build_url(&self, path: &str, params: &[(&str, String)]) -> String {
        let mut url = format!("{}{}", self.base_url, path);
        if !params.is_empty() {
            let q: Vec<String> = params
                .iter()
                .map(|(k, v)| format!("{}={}", encode_param(k), encode_param(v)))
                .collect();
            url.push('?');
            url.push_str(&q.join("&"));
        }
        url
    }

    /// The full deck: boards.mainboard/sideboard/maybeboard/commanders as maps
    /// of { cardId: { quantity, card } }.
    pub async fn get_deck_details(&self, public_id: &str) -> Result<Value, MoxfieldError> {
        self.get(&format!("/v3/decks/all/{}", encode_uri_component(public_id)), &[]).await
    }

    /// Resolve a username to Moxfield's canonical one (case-insensitive
    /// match). GET /v2/users/search-sfw {filter,pageNumber,pageSize}; the
    /// exact-lowercase match, else entries[0, picked ONCE — all three return
    /// fields come from that same entry.
    pub async fn get_user(&self, username: &str) -> Result<MfxUserInfo, MoxfieldError> {
        let params = [
            ("filter", username.to_string()),
            ("pageNumber", "1".to_string()),
            ("pageSize", "10".to_string()),
        ];
        let data = self.get("/v2/users/search-sfw", &params).await?;
        let entries: Vec<Value> = data
            .get("data")
            .filter(|v| v.is_array())
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        // entries.find(e => String(e.userName || '').toLowerCase() ===
        //                  String(username).toLowerCase()) || entries[0]
        let lower = username.to_lowercase();
        let match_once = entries
            .iter()
            .find(|u| {
                // String(e.userName || '').toLowerCase()
                let un = match u.get("userName") {
                    Some(v) if crate::mfx_payload::js_truthy(Some(v)) => crate::mfx_payload::js_to_string(v),
                    _ => String::new(),
                };
                un.to_lowercase() == lower
            })
            .or_else(|| entries.first());
        // `if (!match || !match.userName)` — falsy userName (missing, null,
        // "", 0, false) rejects, exactly like Node.
        let matched = match match_once {
            Some(m) if crate::mfx_payload::js_truthy(m.get("userName")) => m,
            _ => {
                return Err(MoxfieldError::new(
                    format!("Moxfield has no user named \"{username}\""),
                    404,
                ))
            }
        };
        // All three fields come from the ONE picked entry.
        // displayName = match.displayName || match.userName: a truthy
        // non-string displayName passes through (as_str falls back to
        // userName — documented approximation for non-string displayNames).
        let user_name = matched
            .get("userName")
            .map(|v| crate::mfx_payload::js_to_string(v))
            .unwrap_or_default();
        let display_name = match matched.get("displayName") {
            Some(v) if crate::mfx_payload::js_truthy(Some(v)) => match v.as_str() {
                Some(s) => s.to_string(),
                None => crate::mfx_payload::js_to_string(v),
            },
            _ => user_name.clone(),
        };
        let profile_image_url = match matched.get("profileImageUrl") {
            Some(v) if crate::mfx_payload::js_truthy(Some(v)) => Some(match v.as_str() {
                Some(s) => s.to_string(),
                None => crate::mfx_payload::js_to_string(v),
            }),
            _ => None,
        };
        Ok(MfxUserInfo {
            user_name,
            display_name,
            profile_image_url,
        })
    }

    /// Follow /v2/decks/search-sfw pages until totalPages, returning every
    /// summary row (moxfieldApi.js:140-163).
    pub async fn get_author_deck_summaries(
        &self,
        user_name: &str,
    ) -> Result<Vec<Value>, MoxfieldError> {
        self.get_author_deck_summaries_with_page_size(user_name, 100).await
    }

    pub async fn get_author_deck_summaries_with_page_size(
        &self,
        user_name: &str,
        page_size: i64,
    ) -> Result<Vec<Value>, MoxfieldError> {
        let mut all: Vec<Value> = Vec::new();
        let mut page = 1i64;
        let mut total_pages = 1i64;
        while page <= total_pages {
            let params = [
                ("authorUserNames", user_name.to_string()),
                ("pageNumber", page.to_string()),
                ("pageSize", page_size.to_string()),
                ("sortType", "Updated".to_string()),
                ("sortDirection", "Descending".to_string()),
                ("filter", String::new()),
                ("fmt", String::new()),
                ("includePinned", "true".to_string()),
                ("showIllegal", "true".to_string()),
            ];
            let data = self.get("/v2/decks/search-sfw", &params).await?;
            let rows: Vec<Value> = data
                .get("data")
                .filter(|v| v.is_array())
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            all.extend(rows.iter().cloned());
            // totalPages = parseInt(data.totalPages, 10) || page — NaN/0
            // collapses to the current page, so this is the last page.
            total_pages = js_parse_int(data.get("totalPages")).unwrap_or(page);
            if rows.is_empty() {
                break;
            }
            page += 1;
        }
        Ok(all)
    }
}

/// getUser's resolved shape.
#[derive(Clone, Debug)]
pub struct MfxUserInfo {
    pub user_name: String,
    pub display_name: String,
    pub profile_image_url: Option<String>,
}

fn truncate200(stderr: &str, err: &str) -> String {
    // Node: String(stderr || err).slice(0, 200)
    let s = if !stderr.is_empty() { stderr } else { err };
    s.chars().take(200).collect()
}

/// JS `encodeURIComponent`.
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

/// WHATWG urlencoded form component (URLSearchParams): alphanumerics and
/// `*-._` pass through, space becomes `+`, everything else is %XX.
fn encode_param(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '*' | '-' | '.' | '_') {
            out.push(c);
        } else if c == ' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

//! Rust port of the Scryfall HTTP layer in `backend/src/scryfallApi.js`
//! (Lane B unit 1: client, rate-limited queue, 429 cooldown, retry ladder,
//! 404 breaker).
//!
//! One global serialized queue paces every upstream request, exactly like
//! the Node `scryfallQueue` chain: Scryfall publishes HARD per-endpoint
//! limits (`/docs/api/rate-limits`) — 2/second on
//! `/cards/(search|named|random|collection)`, 10/minute on `/cards/manifest`,
//! 10/second on everything else — and per-caller delays that cannot see each
//! other are what earned this app 429s before the shared queue existed.
//! A 429 arms a queue-wide cooldown (`Retry-After` header, else 60 s),
//! because backing off only the offending request keeps the penalty alive.
//!
//! The local sidecar (`LOCAL_SCRYFALL_ENABLED`) is NOT paced: the gaps and
//! the cooldown exist only because Scryfall demands them, and local answers
//! must never queue behind a 60 s upstream freeze. Same rule as
//! `isLocalScryfallTarget` in Node: evaluated on the EFFECTIVE url, and
//! `upstream: true` callers (TCGplayer-priced endpoints) always keep the
//! limiter.
//!
//! `SCRYFALL_BASE_URL` overrides the upstream base. It exists so tests and
//! the DB-diff harness can face a fixture server with the limiter ACTIVE
//! (LOCAL_SCRYFALL mode skips pacing and would test nothing); production
//! never sets it.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use http_body_util::BodyExt;
use hyper::{
    header::{ACCEPT, CONTENT_TYPE, RETRY_AFTER, USER_AGENT},
    Method, Request,
};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::connect::HttpConnector;
use serde_json::Value;

pub const UPSTREAM_BASE: &str = "https://api.scryfall.com";
/// axios client timeout in the Node client (scryfallApi.js).
const REQUEST_TIMEOUT: Duration = Duration::from_millis(6000);
/// Node's SCRYFALL_DEFAULT_COOLDOWN_MS — what a 429 without Retry-After costs.
pub const DEFAULT_COOLDOWN_MS: i64 = 60_000;
/// Bucket keys mirror Node's (the limiter keys a bucket by the regex source
/// string that owns it).
pub const CARD_ENDPOINTS_KEY: &str = r"^\/cards\/(search|named|random|collection)\b";
pub const MANIFEST_KEY: &str = r"^\/cards\/manifest\b";

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Env-derived configuration (mirrors the Node constants).
#[derive(Clone, Debug)]
pub struct ScryEnv {
    pub local: bool,
    pub local_url: String,
    pub base_url: String,
    pub gap_scale: f64,
}

impl ScryEnv {
    pub fn from_env() -> Self {
        Self {
            local: std::env::var("LOCAL_SCRYFALL_ENABLED").as_deref() == Ok("true"),
            local_url: std::env::var("LOCAL_SCRYFALL_URL")
                .unwrap_or_else(|_| "http://scryfall-local:8420".into())
                .trim_end_matches('/')
                .to_string(),
            base_url: std::env::var("SCRYFALL_BASE_URL")
                .unwrap_or_else(|_| UPSTREAM_BASE.to_string())
                .trim_end_matches('/')
                .to_string(),
            gap_scale: std::env::var("SCRYFALL_GAP_SCALE")
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|v: &f64| v.is_finite())
                .unwrap_or(1.0),
        }
    }

    /// (2/s-bucket gap, manifest gap, global floor) in ms, scaled by
    /// SCRYFALL_GAP_SCALE — the same scale Node's e2e suite uses.
    pub fn gaps_ms(&self) -> (u64, u64, u64) {
        let s = self.gap_scale;
        ((500.0 * s) as u64, (10_000.0 * s) as u64, (100.0 * s) as u64)
    }
}

/// True for /cards/(search|named|random|collection) paths — the 2/second
/// bucket. Node's regex is `^\/cards\/(search|named|random|collection)\b`;
/// \b means the next char is not [A-Za-z0-9_].
pub fn is_card_endpoint_path(path: &str) -> bool {
    const NAMES: [&str; 4] = ["search", "named", "random", "collection"];
    let Some(rest) = path.strip_prefix("/cards/") else { return false };
    NAMES.iter().any(|n| {
        rest.starts_with(n)
            && match rest.as_bytes().get(n.len()) {
                None => true,
                Some(c) => !(c.is_ascii_alphanumeric() || *c == b'_'),
            }
    })
}

/// Path portion of an absolute URL (Node `endpointGap` reads path only).
fn url_path(url: &str) -> String {
    match url.split_once("://") {
        Some((_, rest)) => match rest.find('/') {
            Some(i) => {
                let p = &rest[i..];
                p.split('?').next().unwrap_or(p).to_string()
            }
            None => String::new(),
        },
        None => url.split('?').next().unwrap_or(url).to_string(),
    }
}

/// Which limiter bucket a path falls in, with the scaled gap.
pub fn endpoint_gap(path: &str, gaps: (u64, u64, u64)) -> (&'static str, u64) {
    if is_card_endpoint_path(path) {
        (CARD_ENDPOINTS_KEY, gaps.0)
    } else if path.starts_with("/cards/manifest") {
        (MANIFEST_KEY, gaps.1)
    } else {
        ("default", 100)
    }
}

/// Pure wait computation (Node `waitFor`): the request's endpoint gap, the
/// global floor, and any active 429 cooldown — whichever is longest.
/// Exposed for tests with an injected clock.
pub fn wait_ms(
    now: i64,
    last_all: i64,
    last_endpoint: i64,
    cooldown_until: i64,
    gap: u64,
    floor: u64,
) -> i64 {
    [
        cooldown_until - now,
        gap as i64 - (now - last_endpoint),
        floor as i64 - (now - last_all),
        0,
    ]
    .into_iter()
    .max()
    .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct ScryError {
    pub status: Option<u16>,
    pub message: String,
    /// Retry-After seconds carried out of a 429 response for the cooldown.
    pub retry_after_secs: Option<i64>,
}

impl ScryError {
    pub fn is_status(&self, code: u16) -> bool {
        self.status == Some(code)
    }
    fn transport(message: impl Into<String>) -> Self {
        Self { status: None, message: message.into(), retry_after_secs: None }
    }
}

impl std::fmt::Display for ScryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(s) => write!(f, "{s}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

/// Per-request options — the port of the Node `config` bits that steer the
/// limiter path.
#[derive(Clone, Debug, Default)]
pub struct ScryOpts {
    /// Pin to api.scryfall.com even with the sidecar on (TCGplayer prices).
    pub upstream: bool,
    /// Apply the 404 breaker to this call (404 is an answer: cache it
    /// negatively so repeats don't re-pay upstream).
    pub breaker: bool,
}

/// 404 breaker TTL (ms). Node caches negative answers per-caller (runWalk's
/// 404 cache, priceEstimates' askedOracle); this is the shared-client form
/// of the same protection.
const BREAKER_TTL_MS: i64 = 10 * 60 * 1000;

struct LimiterState {
    last_all: i64,
    last_by_endpoint: HashMap<String, i64>,
    cooldown_until: i64,
}

/// Node `noteRateLimit`: a 429 pauses EVERY request for Retry-After
/// (seconds, else 60 s), extending the window, never shortening it.
fn note_rate_limit(state: &mut LimiterState, retry_after_secs: Option<i64>) {
    let wait = retry_after_secs.map(|s| s.max(0) * 1000).unwrap_or(DEFAULT_COOLDOWN_MS);
    let until = now_ms() + wait;
    if until > state.cooldown_until {
        state.cooldown_until = until;
        tracing::warn!("Scryfall rate-limited us — pausing all Scryfall traffic for {}s", wait / 1000);
    }
}

type Https =
    hyper_util::client::legacy::Client<hyper_rustls::HttpsConnector<HttpConnector>, String>;

/// The shared client. Cheap to clone (Arc interior); one per process so the
/// queue is global, exactly like the module-level `scryfallQueue`.
#[derive(Clone)]
pub struct Scry {
    env: ScryEnv,
    inner: Arc<Inner>,
}

struct Inner {
    client: Https,
    /// One global queue: the mutex serializes paced requests (wait + fetch
    /// under the lock), matching the Node promise-chain.
    queue: tokio::sync::Mutex<LimiterState>,
    breaker: Mutex<HashMap<String, i64>>,
    /// priceEstimates.js `askedOracle`: floorless oracle ids already asked
    /// this process, so a never-priced card is not re-asked every sweep.
    asked_oracle: Mutex<HashSet<String>>,
}

impl Scry {
    pub fn new(env: ScryEnv) -> Self {
        let https = HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("native roots")
            .https_or_http()
            .enable_http1()
            .build();
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .pool_idle_timeout(Duration::from_secs(30))
                .build(https);
        Self {
            env,
            inner: Arc::new(Inner {
                client,
                queue: tokio::sync::Mutex::new(LimiterState {
                    last_all: 0,
                    last_by_endpoint: HashMap::new(),
                    cooldown_until: 0,
                }),
                breaker: Mutex::new(HashMap::new()),
                asked_oracle: Mutex::new(HashSet::new()),
            }),
        }
    }

    pub fn env(&self) -> &ScryEnv {
        &self.env
    }

    /// Node `scryUrl`: rewrite an absolute api.scryfall.com URL onto the
    /// sidecar when LOCAL_SCRYFALL is on.
    pub fn scry_url(&self, u: &str) -> String {
        if self.env.local {
            u.replacen(UPSTREAM_BASE, &self.env.local_url, 1)
        } else {
            u.to_string()
        }
    }

    /// Resolve a caller URL + opts to the effective absolute URL that hits
    /// the wire and whether the limiter applies (Node `scryRequest` +
    /// `isLocalScryfallTarget`, evaluated on the EFFECTIVE url).
    fn resolve(&self, url: &str, opts: &ScryOpts) -> (String, bool) {
        if opts.upstream {
            // Node pins baseURL to api.scryfall.com for upstream calls; the
            // base override (tests/harness only) moves that pin too.
            let abs = if url.starts_with("http://") || url.starts_with("https://") {
                url.replacen(UPSTREAM_BASE, &self.env.base_url, 1)
            } else {
                format!("{}{}", self.env.base_url, url)
            };
            return (abs, true);
        }
        let abs = if url.starts_with("http://") || url.starts_with("https://") {
            self.scry_url(url)
        } else {
            let base = if self.env.local { &self.env.local_url } else { &self.env.base_url };
            format!("{base}{url}")
        };
        // Pacing only applies to what still faces upstream: a sidecar swap
        // onto a local host skips it (the queue would freeze local searches
        // behind 60 s upstream cooldowns); anything not swapped keeps it.
        let paced = if self.env.local { !host_is_local(&abs) } else { true };
        (abs, paced)
    }

    /// GET / POST through the shared queue, returning the parsed JSON body
    /// (None when the body is not JSON — Node callers guard `resp.data`).
    pub async fn get(&self, url: &str, opts: &ScryOpts) -> Result<Option<Value>, ScryError> {
        self.request(Method::GET, url, None, opts).await
    }
    pub async fn post(
        &self,
        url: &str,
        body: Value,
        opts: &ScryOpts,
    ) -> Result<Option<Value>, ScryError> {
        self.request(Method::POST, url, Some(body), opts).await
    }

    /// GET that treats 404 as an empty answer (getOracleText/getCardById
    /// semantics): Ok(None), no retry, and — with `breaker` — repeats of the
    /// same URL inside the TTL never touch the wire again.
    pub async fn get_opt(&self, url: &str, opts: &ScryOpts) -> Result<Option<Value>, ScryError> {
        let (abs, _) = self.resolve(url, opts);
        if opts.breaker {
            let hit = self
                .inner
                .breaker
                .lock()
                .unwrap()
                .get(&abs)
                .map(|expiry| *expiry > now_ms())
                .unwrap_or(false);
            if hit {
                return Ok(None);
            }
        }
        match self.request(Method::GET, url, None, opts).await {
            Ok(v) => {
                if opts.breaker {
                    self.inner.breaker.lock().unwrap().remove(&abs);
                }
                Ok(v)
            }
            Err(e) if e.is_status(404) => {
                if opts.breaker {
                    self.inner.breaker.lock().unwrap().insert(abs, now_ms() + BREAKER_TTL_MS);
                }
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Node `scryGetRetried` / `scryPostRetried`: re-queue on 429 up to
    /// `retries` attempts. The wait itself is the shared cooldown; a retry
    /// just lines up behind it again.
    pub async fn get_retried(
        &self,
        url: &str,
        opts: &ScryOpts,
        retries: u32,
    ) -> Result<Option<Value>, ScryError> {
        let mut last: Option<ScryError> = None;
        for i in 0..retries {
            match self.get(url, opts).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if e.is_status(429) && i + 1 < retries {
                        last = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| ScryError::transport("retry loop exited")))
    }

    pub async fn post_retried(
        &self,
        url: &str,
        body: Value,
        opts: &ScryOpts,
        retries: u32,
    ) -> Result<Option<Value>, ScryError> {
        let mut last: Option<ScryError> = None;
        for i in 0..retries {
            match self.post(url, body.clone(), opts).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if e.is_status(429) && i + 1 < retries {
                        last = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| ScryError::transport("retry loop exited")))
    }

    async fn request(
        &self,
        method: Method,
        url: &str,
        body: Option<Value>,
        opts: &ScryOpts,
    ) -> Result<Option<Value>, ScryError> {
        let (abs, paced) = self.resolve(url, opts);
        if !paced {
            return self.send(method, &abs, body).await;
        }
        // Serialize through the global queue, waiting out the endpoint gap /
        // floor / cooldown before stamping the clocks. The loop rechecks
        // after every sleep: a 429 can arm the cooldown while this request
        // sits queued (Node's `for (let w = waitFor(url); ...)` recheck).
        let mut state = self.inner.queue.lock().await;
        loop {
            let gaps = self.env.gaps_ms();
            let path = url_path(&abs);
            let (key, gap) = endpoint_gap(&path, gaps);
            let now = now_ms();
            let wait = wait_ms(
                now,
                state.last_all,
                *state.last_by_endpoint.get(key).unwrap_or(&0),
                state.cooldown_until,
                gap,
                gaps.2,
            );
            if wait <= 0 {
                break;
            }
            drop(state);
            tokio::time::sleep(Duration::from_millis(wait.min(5_000) as u64)).await;
            state = self.inner.queue.lock().await;
        }
        let now = now_ms();
        let (key, _) = endpoint_gap(&url_path(&abs), self.env.gaps_ms());
        state.last_all = now;
        state.last_by_endpoint.insert(key.to_string(), now);
        let r = self.send(method.clone(), &abs, body.clone()).await;
        if let Err(e) = &r {
            if e.is_status(429) {
                note_rate_limit(&mut state, e.retry_after_secs);
            }
        }
        r
    }

    async fn send(
        &self,
        method: Method,
        abs: &str,
        body: Option<Value>,
    ) -> Result<Option<Value>, ScryError> {
        let uri = abs.parse::<hyper::Uri>().map_err(|e| ScryError::transport(format!("bad url {abs}: {e}")))?;
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(USER_AGENT, "Scrybox/1.0")
            .header(ACCEPT, "application/json");
        let req = match body {
            Some(b) => {
                builder = builder.header(CONTENT_TYPE, "application/json");
                builder.body(b.to_string())
            }
            None => builder.body(String::new()),
        }
        .map_err(|e| ScryError::transport(format!("request build: {e}")))?;

        let res = tokio::time::timeout(REQUEST_TIMEOUT, self.inner.client.request(req))
            .await
            .map_err(|_| ScryError::transport(format!("{abs}: timeout")))?
            .map_err(|e| ScryError::transport(format!("{abs}: {e}")))?;

        let status = res.status().as_u16();
        let retry_after_secs = if status == 429 {
            res.headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<i64>().ok())
        } else {
            None
        };
        let bytes = res
            .into_body()
            .collect()
            .await
            .map_err(|e| ScryError { status: Some(status), message: format!("body: {e}"), retry_after_secs })?
            .to_bytes();
        if status >= 400 {
            return Err(ScryError {
                status: Some(status),
                message: format!("{abs} -> {status}"),
                retry_after_secs,
            });
        }
        let text = String::from_utf8_lossy(&bytes[..]);
        Ok(serde_json::from_str::<Value>(&text).ok())
    }

    /// 429 cooldown state for tests/health: `Some(until_ms)` while cooling.
    pub async fn cooldown_until(&self) -> Option<i64> {
        let s = self.inner.queue.lock().await;
        (s.cooldown_until > now_ms()).then_some(s.cooldown_until)
    }

    /// True when the breaker currently holds a negative entry for a URL
    /// (effective form). Test hook.
    pub async fn breaker_hit(&self, url: &str, opts: &ScryOpts) -> bool {
        let (abs, _) = self.resolve(url, opts);
        self.inner
            .breaker
            .lock()
            .unwrap()
            .get(&abs)
            .map(|expiry| *expiry > now_ms())
            .unwrap_or(false)
    }

    pub fn asked_oracle_taken(&self, oid: &str) -> bool {
        self.inner.asked_oracle.lock().unwrap().contains(oid)
    }
    pub fn mark_asked_oracle(&self, oid: &str) {
        self.inner.asked_oracle.lock().unwrap().insert(oid.to_string());
    }
}

fn host_is_local(abs: &str) -> bool {
    let Some((_, rest)) = abs.split_once("://") else { return false };
    let authority = rest.split('/').next().unwrap_or("");
    let host = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    host != "api.scryfall.com"
}

//! Client-IP resolution + the authLimiter twin.
//!
//! Node runs behind `TRUST_PROXY` (server.js) so `req.ip` — which
//! express-rate-limit keys on — is the real client, taken from
//! `X-Forwarded-For` exactly as `proxy-addr` does: walk hops from the socket
//! outward (socket, then XFF right-to-left), skip trusted hops, the first
//! untrusted one is the client. The Rust-native limiter must make the same
//! decision or rate limiting silently collapses to one bucket behind the
//! Rust proxy (BRIEF: "rate limits stay real").

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;

use axum::http::HeaderMap;
use crate::poison::LockExt;

/// Opaque trust-rule detail of `Trust::Rules` (built only by
/// `Trust::from_env_value`); public only because the variant field is.
#[derive(Clone, Debug)]
pub enum TrustRule {
    Loopback,
    UniqueLocal,
    LinkLocal,
    Net(IpAddr, u8),
}

#[derive(Clone, Debug)]
pub enum Trust {
    /// TRUST_PROXY unset: never believe XFF.
    None,
    /// TRUST_PROXY=true: every hop is trusted (req.ip = leftmost XFF entry).
    All,
    /// TRUST_PROXY=<n>: the n rightmost proxy hops are trusted.
    Hops(usize),
    /// TRUST_PROXY="loopback, uniquelocal" (or CIDR list).
    Rules(Vec<TrustRule>),
}

impl Trust {
    /// Same parsing ladder as backend/src/server.js:91-94.
    pub fn from_env_value(v: &str) -> Trust {
        let v = v.trim();
        if v == "true" {
            return Trust::All;
        }
        if let Ok(hops) = v.parse::<usize>() {
            // parse once, use the value (REVIEW errors-maint L3).
            return Trust::Hops(hops);
        }
        let mut rules = Vec::new();
        for token in v.split(',').map(|t| t.trim()).filter(|t| !t.is_empty()) {
            match token {
                "loopback" => rules.push(TrustRule::Loopback),
                "uniquelocal" | "private" => rules.push(TrustRule::UniqueLocal),
                "linklocal" => rules.push(TrustRule::LinkLocal),
                other => {
                    if let Some((ip, bits)) = parse_cidr(other) {
                        rules.push(TrustRule::Net(ip, bits));
                    }
                    // Unknown token: Express silently ignores a trust entry
                    // it cannot parse (proxy-addr throws only on require,
                    // and app.set would crash boot — matching that is out
                    // of scope; ignoring is the safe direction).
                }
            }
        }
        Trust::Rules(rules)
    }

    fn trusts(&self, ip: &IpAddr) -> bool {
        match self {
            Trust::None => false,
            Trust::All => true,
            Trust::Hops(_) => false, // handled positionally in client_ip
            Trust::Rules(rules) => rules.iter().any(|r| match r {
                TrustRule::Loopback => ip.is_loopback(),
                TrustRule::UniqueLocal => is_uniquelocal(ip),
                TrustRule::LinkLocal => is_linklocal(ip),
                TrustRule::Net(net, bits) => in_net(ip, net, *bits),
            }),
        }
    }
}

fn parse_cidr(s: &str) -> Option<(IpAddr, u8)> {
    let (addr, bits) = match s.split_once('/') {
        Some((a, b)) => (a.parse::<IpAddr>().ok()?, b.parse::<u8>().ok()?),
        None => (s.parse::<IpAddr>().ok()?, if s.contains(':') { 128 } else { 32 }),
    };
    let max = if addr.is_ipv6() { 128 } else { 32 };
    if bits > max {
        return None;
    }
    Some((addr, bits))
}

fn masked(ip: &IpAddr) -> u128 {
    match ip {
        IpAddr::V4(v4) => u128::from(u32::from(*v4)),
        IpAddr::V6(v6) => u128::from(*v6),
    }
}

fn in_net(ip: &IpAddr, net: &IpAddr, bits: u8) -> bool {
    // v4 and v6 never trust each other's rules (proxy-addr keeps the
    // families separate).
    if ip.is_ipv4() != net.is_ipv4() {
        return false;
    }
    let width: u8 = if ip.is_ipv4() { 32 } else { 128 };
    if bits > width {
        return false;
    }
    let shift = (width - bits) as u32;
    (masked(ip) >> shift) == (masked(net) >> shift)
}

fn is_uniquelocal(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 10
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments()[0];
            s & 0xfe00 == 0xfc00
        }
    }
}

fn is_linklocal(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 169 && v4.octets()[1] == 254,
        IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 == 0xfe80,
    }
}

fn xff_values(headers: &HeaderMap) -> Vec<String> {
    // Node's http joins repeated XFF headers with ", "; proxy-addr splits
    // on ',' and trims.
    let joined: String = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    joined.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// express-rate-limit v8 `ipKeyGenerator`: IPv6 clients bucket per /64 so a
/// customer's rotating addresses share one window.
fn key_for(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            let segs = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}", segs[0], segs[1], segs[2], segs[3])
        }
    }
}

/// Resolve the limiter key exactly as Express `req.ip` would under the
/// configured TRUST_PROXY value.
pub fn client_ip_key(peer: Option<IpAddr>, headers: &HeaderMap, trust: &Trust) -> String {
    let socket = peer.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let xff = xff_values(headers);
    match trust {
        Trust::None => key_for(socket),
        Trust::All => match xff.first() {
            // trust-proxy-true: req.ip is the leftmost XFF token VERBATIM —
            // proxy-addr 2.0.7 keeps unparseable entries in the chain
            // (verified live: XFF 'garbage, 10.0.0.9' -> req.ip 'garbage'),
            // so an unparseable first hop is the bucket key, not the socket.
            Some(entry) => match entry.parse::<IpAddr>().ok() {
                Some(ip) => key_for(ip),
                None => entry.clone(),
            },
            None => key_for(socket),
        },
        Trust::Hops(n) => {
            if *n == 0 || xff.is_empty() {
                return key_for(socket);
            }
            // trust n proxies: the socket plus the n-1 rightmost XFF entries
            // are the proxies; the client is the entry just left of them.
            let idx = xff.len().saturating_sub(*n);
            // proxy-addr returns an unparseable chosen entry verbatim.
            match xff.get(idx) {
                Some(entry) => match entry.parse::<IpAddr>() {
                    Ok(ip) => key_for(ip),
                    Err(_) => entry.clone(),
                },
                None => key_for(socket),
            }
        }
        Trust::Rules(_) => {
            // proxy-addr's forwarded(): the XFF chain is consulted only when
            // the SOCKET itself is trusted. An untrusted socket ignores XFF
            // entirely — otherwise any direct (non-proxy) client could mint
            // fresh limiter buckets by spoofing the header.
            if xff.is_empty() || !trust.trusts(&socket) {
                return key_for(socket);
            }
            for entry in xff.iter().rev() {
                match entry.parse::<IpAddr>() {
                    Ok(ip) if trust.trusts(&ip) => continue,
                    Ok(ip) => return key_for(ip),
                    // Non-parseable hop: proxy-addr stops the trusted walk
                    // there — this entry is the client (live oracle:
                    // 'garbage, 10.0.0.9' -> req.ip 'garbage').
                    Err(_) => return entry.clone(),
                }
            }
            // Every XFF hop trusted (socket already checked): proxy-addr's
            // forwarded() hands back the whole chain and req.ip takes the
            // FURTHEST entry — the LEFTMOST XFF token, not the socket
            // (verified live against proxy-addr 2.0.7 via express req.ip,
            // LANE-B iter-19: '10.0.0.9, 127.0.0.1' from 127.0.0.1 ->
            // req.ip 10.0.0.9; single hop '10.0.0.9' -> 10.0.0.9).
            // Returning the socket here collapsed an entire trusted subnet
            // of clients behind a loopback front into one limiter bucket.
            // The walk above guarantees xff[0] parses.
            match xff.first().and_then(|s| s.parse::<IpAddr>().ok()) {
                Some(ip) => key_for(ip),
                None => key_for(socket),
            }
        }
    }
}

/// express-rate-limit v8 MemoryStore twin: fixed window anchored at the
/// first request in it; `limit` requests per `window_ms` per key; headers
/// follow the draft-7 set the Node process emits (verified against :3312:
/// RateLimit-Policy/Limit/Remaining/Reset, Retry-After on 429).
pub struct FixedWindowLimiter {
    window_ms: u64,
    limit: u64,
    buckets: Mutex<Buckets>,
}

/// Bucket map plus an adaptive prune threshold: expired windows are swept
/// only when the map outgrows `high_water`, after which the threshold is
/// reset to twice the surviving size — amortised O(1) per hit instead of
/// an O(n) scan under the mutex on every request once the map is large.
struct Buckets {
    map: HashMap<String, Bucket>,
    high_water: usize,
}

const PRUNE_FLOOR: usize = 4096;

struct Bucket {
    reset_ms: u64,
    total: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Verdict {
    pub blocked: bool,
    pub remaining: u64,
    pub reset_secs: u64,
}

impl FixedWindowLimiter {
    pub fn new(limit: u64, window_ms: u64) -> Self {
        Self {
            window_ms,
            limit,
            buckets: Mutex::new(Buckets { map: HashMap::new(), high_water: PRUNE_FLOOR }),
        }
    }

    pub fn hit(&self, key: &str, now_ms: u64) -> Verdict {
        let mut guard = self.buckets.guard();
        let bs = &mut *guard;
        // Prune expired windows so a key spray cannot grow the map without
        // bound (Node's MemoryStore expires keys per window; dropping an
        // expired bucket is behaviourally identical since a fresh hit
        // would reset it anyway). Amortised: see `Buckets`.
        if bs.map.len() > bs.high_water {
            bs.map.retain(|_, v| v.reset_ms > now_ms);
            bs.high_water = (bs.map.len() * 2).max(PRUNE_FLOOR);
        }
        let entry = bs.map.entry(key.to_string()).or_insert(Bucket {
            reset_ms: now_ms + self.window_ms,
            total: 0,
        });
        if entry.reset_ms <= now_ms {
            entry.reset_ms = now_ms + self.window_ms;
            entry.total = 0;
        }
        entry.total += 1;
        let total = entry.total;
        let reset_ms = entry.reset_ms;
        // v8 prunes lazily; the entry may live past its window in the map
        // but a fresh request must start a fresh window, hence the reset
        // check above.
        Verdict {
            blocked: total > self.limit,
            remaining: self.limit.saturating_sub(total),
            reset_secs: reset_ms.saturating_sub(now_ms).div_ceil(1000).max(1),
        }
    }
}

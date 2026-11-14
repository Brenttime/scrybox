//! Primitives the auth port needs that Node gets from `crypto`.
//!
//! Implemented in-tree (on top of `sha2`/`getrandom`, both already in the
//! tree) rather than pulling `pbkdf2`/`hmac`/`rand`/`uuid` crates: the image
//! size budget (Gate D) and a small supply chain are BRIEF goals, and every
//! function here is ~20 lines against an RFC.

use base64::Engine;
use sha2::{Digest, Sha256, Sha512};

/// `crypto.randomBytes(n)`. Panics only if the OS RNG itself fails, which on
/// Linux (getrandom syscall) means the kernel is broken — same failure mode
/// as Node throwing at the call site.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("getrandom: OS RNG unavailable");
    b
}

pub fn random_hex(n_bytes: usize) -> String {
    hex_lower(&random_bytes(n_bytes))
}

pub fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Strict lowercase-or-uppercase hex decode; `None` on any non-hex input.
/// (Node's `Buffer.from(h,'hex')` is lenient and truncates at the first bad
/// nibble; every hash we verify is one we minted ourselves, so strictness
/// only rejects garbage that would fail the comparison anyway.)
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    for pair in b.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// RFC 4122 v4 UUID, lowercase — byte-identical shape to the `uuid` package
/// Node uses for share tokens.
pub fn uuid_v4() -> String {
    let b = random_bytes(16);
    let mut v = [0u8; 16];
    v.copy_from_slice(&b);
    v[6] = (v[6] & 0x0f) | 0x40; // version 4
    v[8] = (v[8] & 0x3f) | 0x80; // variant 10x
    let h = hex_lower(&v);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn hmac_block(digest_block_len: usize, key: &[u8], msg: &[u8], out_len: usize, hash: fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let mut k = key.to_vec();
    if k.len() > digest_block_len {
        k = hash(&k);
    }
    k.resize(digest_block_len, 0);
    let ipad: Vec<u8> = k.iter().map(|x| x ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|x| x ^ 0x5c).collect();
    let mut inner = ipad;
    inner.extend_from_slice(msg);
    let inner = hash(&inner);
    let mut outer = opad;
    outer.extend_from_slice(&inner);
    let mut r = hash(&outer);
    r.truncate(out_len);
    r
}

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    hmac_block(64, key, msg, 32, |b| {
        let mut h = Sha256::new();
        h.update(b);
        h.finalize().to_vec()
    })
}

fn sha512(b: &[u8]) -> Vec<u8> {
    let mut h = Sha512::new();
    h.update(b);
    h.finalize().to_vec()
}

pub fn hmac_sha512(key: &[u8], msg: &[u8]) -> Vec<u8> {
    hmac_block(128, key, msg, 64, sha512)
}

/// PBKDF2-HMAC-SHA512, dkLen 64 — exactly what `crypto.pbkdf2Sync(pw, salt,
/// iters, 64, 'sha512')` returns (single output block, F() only).
pub fn pbkdf2_sha512(pw: &[u8], salt: &[u8], iters: u32) -> [u8; 64] {
    let mut block = [0u8; 4];
    block.copy_from_slice(&1u32.to_be_bytes()); // block index 1
    let mut u = hmac_sha512(pw, &[salt, &block].concat());
    let mut t = u.clone();
    for _ in 1..iters {
        u = hmac_sha512(pw, &u);
        for (t, u) in t.iter_mut().zip(u.iter()) {
            *t ^= u;
        }
    }
    let mut out = [0u8; 64];
    out.copy_from_slice(&t);
    out
}

/// `crypto.timingSafeEqual` (equal-length inputs checked by the caller,
/// like Node does).
pub fn timing_safe_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// `verifyPassword` from backend/src/utils/authHelpers.js, byte-exact:
/// `iters:salt:hash` (or legacy `salt:hash` @ 10000 iterations), pbkdf2
/// 64-byte sha512, timing-safe compare. CPU-heavy (~100 ms at 210k iters) —
/// callers must run it on the blocking pool.
///
/// `Err(())` mirrors Node's `pbkdf2Sync` throwing (iteration count NaN,
/// < 1 or > INT32_MAX) — callers map it to their 500 arm, as Node's
/// try/catch does. Never runs PBKDF2 with a garbage count.
pub fn verify_password_checked(password: &str, stored_hash: &str) -> Result<bool, ()> {
    if stored_hash.is_empty() {
        return Ok(false);
    }
    let parts: Vec<&str> = stored_hash.split(':').collect();
    let (iterations, salt, hash) = match parts.len() {
        3 => match js_parse_int(parts[0]) {
            Some(n) if (1..=i32::MAX as i64).contains(&n) => (n as u32, parts[1], parts[2]),
            _ => return Err(()),
        },
        2 => (10000, parts[0], parts[1]),
        _ => return Ok(false),
    };
    Ok(verify_with(password, salt, hash, iterations))
}

/// Boolean convenience over [`verify_password_checked`] (a Node throw
/// counts as "not verified").
pub fn verify_password(password: &str, stored_hash: &str) -> bool {
    verify_password_checked(password, stored_hash).unwrap_or(false)
}

/// `parseInt(s, 10)`: leading whitespace, optional sign, longest digit
/// prefix; `None` = NaN. Saturates magnitude-then-sign (overflow lands on
/// ±i64::MAX) — shared implementation in `crate::util`.
use crate::util::js_parse_int_unicode_sat as js_parse_int;

fn verify_with(password: &str, salt: &str, hash: &str, iterations: u32) -> bool {
    // Node's Buffer.from(hash,'hex') — see hex_decode.
    let Some(stored) = hex_decode(hash) else { return false };
    let verify = pbkdf2_sha512(password.as_bytes(), salt.as_bytes(), iterations);
    if stored.len() != verify.len() {
        return false;
    }
    timing_safe_equal(&stored, &verify)
}

/// `hashPassword` from backend/src/db.js (PBKDF2_ITERATIONS = 210000,
/// 16-byte hex salt). Also blocking-pool work.
pub fn hash_password(password: &str) -> String {
    const PBKDF2_ITERATIONS: u32 = 210_000;
    let salt = random_hex(16);
    let hash = hex_lower(&pbkdf2_sha512(password.as_bytes(), salt.as_bytes(), PBKDF2_ITERATIONS));
    format!("{PBKDF2_ITERATIONS}:{salt}:{hash}")
}

pub fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Node's `Buffer.from(s, 'base64url')` is lenient (padding optional, both
/// alphabets accepted). Mirror that: try URL-safe first, then standard.
pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    if let Ok(v) = e.decode(s) {
        return Some(v);
    }
    let e = base64::engine::general_purpose::URL_SAFE;
    if let Ok(v) = e.decode(s) {
        return Some(v);
    }
    let e = base64::engine::general_purpose::STANDARD_NO_PAD;
    if let Ok(v) = e.decode(s) {
        return Some(v);
    }
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// `crypto.createHash('sha256').update(s).digest()` (raw bytes).
pub fn sha256(bytes: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().to_vec()
}

/// JavaScript `encodeURIComponent` byte-for-byte: unreserved
/// `A-Za-z0-9-_.!~*'()` pass through, every other UTF-8 byte becomes %XX
/// with UPPERCASE hex. (form_urlencoded differs on `!*'()` and space→'+',
/// so it cannot be used where Node used encodeURIComponent.)
pub fn encode_uri_component(s: &str) -> String {
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

/// WHATWG URLSearchParams application/x-www-form-urlencoded value encoding
/// (what `url.searchParams.set` and `new URLSearchParams().toString()`
/// produce): unreserved `*-._` and alphanumerics pass, space becomes `+`,
/// everything else percent-encoded uppercase.
pub fn form_urlencode(s: &str) -> String {
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

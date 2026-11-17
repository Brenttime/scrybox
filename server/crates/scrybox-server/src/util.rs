//! Shared, tested primitives (LANE-B iter-41 maintainability consolidation).
//!
//! One home for the helpers that used to be copy-pasted per module:
//!
//!  * Clock: [`now_ms`] / [`now_ms_u64`] — `Date.now()` as i64/u64. A clock
//!    before the UNIX epoch degrades to 0 (the majority semantics of the
//!    previous 15 per-module copies, which disagreed on `unwrap` vs
//!    `unwrap_or(0)`; 0 never panics a request). `backup::now_ms` stays
//!    local on purpose: it deliberately ROUNDS nanoseconds.
//!
//!  * JS `parseInt(x, 10)` — four call-site semantics, all preserved exactly,
//!    built from one tested core. The variants differ on two axes: the
//!    whitespace set trimmed before the sign, and the overflow model.
//!    Call sites pick the wrapper that matches what they had before; do NOT
//!    "unify" them further without a Node-parity check.
//!
//!  * Error/response builders for the two byte-shapes the port emits for
//!    `{ "error": ... }` bodies. A third shape (CT-only, no ETag, in
//!    `cards_tags::json_response`) stays local to cards_tags — it is used
//!    behind the cvscan limit-header path and nowhere else.
//!      - [`json_error`] — Express `res.json` WITHOUT the body ETag:
//!        `content-type: application/json; charset=utf-8` + `axum::Json`.
//!      - [`json_error_express`] / [`json_response`] — Express `res.json`
//!        WITH the Express default weak body ETag.

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

/// `Date.now()` as i64 milliseconds since the epoch; 0 on a pre-epoch clock.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// [`now_ms`] for the call sites whose bindings are u64 (auth/session/cardscan/
/// cvscan bookkeeping).
pub fn now_ms_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// JS parseInt(x, 10)
// ---------------------------------------------------------------------------

/// Which leading whitespace the parse strips before the optional sign.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ws {
    /// The six ASCII whitespace bytes (space, \t, \n, \v, \f, \r) — the
    /// collection/search/export copies.
    Ascii,
    /// Rust `str::trim_start()` (Unicode White_Space) — the auth_support copy.
    Unicode,
    /// Unicode White_Space plus U+FEFF BOM — the mfx_payload copy (closest to
    /// the JS spec, kept distinct because it is what the Moxfield/Manabox
    /// paths were validated with).
    UnicodeBom,
}

/// What happens when the digit run exceeds i64. JS returns a non-integer
/// float (modelled as NaN/None); the query-param copies saturate instead —
/// with two DIFFERENT saturation shapes, preserved verbatim.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ov {
    /// Overflow -> i64::MIN (negative) / i64::MAX (positive): signed saturate.
    SaturateSigned,
    /// Magnitude saturates at i64::MAX, THEN the sign applies: a negative
    /// overflow yields -i64::MAX, not i64::MIN (the auth_support fold).
    SaturateMag,
    /// Overflow is NaN -> None (the mfx_payload / export copies).
    NaN,
}

/// One parseInt core parameterised on (whitespace set, overflow model).
/// Leading whitespace, optional sign, longest ASCII digit run, stop at the
/// first non-digit; None when the digit run is empty (NaN).
fn parse_int_core(s: &str, ws: Ws, ov: Ov) -> Option<i64> {
    let t = match ws {
        Ws::Ascii => {
            let b = s.as_bytes();
            let mut i = 0;
            while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r') {
                i += 1;
            }
            &s[i..]
        }
        Ws::Unicode => s.trim_start(),
        Ws::UnicodeBom => s.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}'),
    };
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'+') => (false, &t[1..]),
        Some(b'-') => (true, &t[1..]),
        _ => (false, t),
    };
    let end = digits.bytes().take_while(u8::is_ascii_digit).count();
    if end == 0 {
        return None; // NaN
    }
    let run = &digits[..end]; // ASCII digits only — byte-slice is safe
    match ov {
        Ov::NaN => run.parse::<i64>().ok().map(|v| if neg { -v } else { v }),
        Ov::SaturateSigned => Some(match run.parse::<i64>() {
            Ok(v) => {
                if neg {
                    -v
                } else {
                    v
                }
            }
            Err(_) => {
                if neg {
                    i64::MIN
                } else {
                    i64::MAX
                }
            }
        }),
        Ov::SaturateMag => {
            let v = run.parse::<i64>().unwrap_or(i64::MAX);
            Some(if neg { -v } else { v })
        }
    }
}

/// JS `parseInt(x, 10)` for query params: ASCII whitespace, signed saturate
/// (overflow clamps to i64::MIN/MAX). The canonical str-level copy.
pub(crate) fn js_parse_int(s: &str) -> Option<i64> {
    parse_int_core(s, Ws::Ascii, Ov::SaturateSigned)
}

/// [`js_parse_int`] but overflow is NaN -> None (the export CSV path).
pub(crate) fn js_parse_int_nonsat(s: &str) -> Option<i64> {
    parse_int_core(s, Ws::Ascii, Ov::NaN)
}

/// Unicode whitespace + BOM trim, overflow NaN -> None (the mfx_payload core,
/// fed by its `String(v)` pre-pass).
pub(crate) fn js_parse_int_unicode(s: &str) -> Option<i64> {
    parse_int_core(s, Ws::UnicodeBom, Ov::NaN)
}

/// `str::trim_start()` whitespace, magnitude-then-sign saturate: a negative
/// overflow is `-i64::MAX`, NOT `i64::MIN` (the auth_support pbkdf2/secret
/// parse — verified against the old fold on 2^64+ strings).
pub(crate) fn js_parse_int_unicode_sat(s: &str) -> Option<i64> {
    parse_int_core(s, Ws::Unicode, Ov::SaturateMag)
}

// ---------------------------------------------------------------------------
// Error / JSON response builders
// ---------------------------------------------------------------------------

/// Node `res.json` exact bytes: `application/json; charset=utf-8` plus the
/// Express default weak body ETag (`W/"<len>-<base64 sha1[..27]>"`).
pub(crate) fn json_response(status: StatusCode, value: &Value) -> Response {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    (
        status,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8")),
            (header::ETAG, HeaderValue::from_str(&crate::collection::body_etag(&body)).unwrap()),
        ],
        Body::from(body),
    )
        .into_response()
}

/// `res.json({ error: msg })` with the Express body ETag (the
/// collection_writes/auth_routes byte-shape).
pub(crate) fn json_error_express(status: StatusCode, msg: &str) -> Response {
    json_response(status, &json!({ "error": msg }))
}

/// `{ "error": msg }` with `application/json; charset=utf-8` via axum Json —
/// NO ETag (the shape the auth/stats/search/shared group emits, byte-checked
/// against Node on their routes' parity runs).
pub(crate) fn json_error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"))],
        axum::Json(json!({ "error": msg })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(res: &Response, name: &'static str) -> Option<String> {
        res.headers().get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
    }
    async fn body(res: Response) -> String {
        String::from_utf8(axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec())
            .unwrap()
    }

    #[test]
    fn now_ms_sane_and_agree() {
        let i = now_ms();
        let u = now_ms_u64();
        // Any time after 2025-01-01; guards against unit slips (seconds/µs).
        assert!(i >= 1_735_689_600_000 && i < 4_000_000_000_000, "now_ms = {i}");
        assert!((i as u64).abs_diff(u) < 60_000, "i64/u64 twins drifted: {i} vs {u}");
    }

    // -- shared parseInt semantics across every wrapper ---------------------

    #[test]
    fn parse_int_common_cases_all_wrappers() {
        let wrappers: [(&str, fn(&str) -> Option<i64>); 4] = [
            ("js_parse_int", js_parse_int),
            ("nonsat", js_parse_int_nonsat),
            ("unicode", js_parse_int_unicode),
            ("unicode_sat", js_parse_int_unicode_sat),
        ];
        for (name, f) in wrappers {
            assert_eq!(f("42"), Some(42), "{name} 42");
            assert_eq!(f("  +7x"), Some(7), "{name} +7x stops at non-digit");
            assert_eq!(f("\t -12abc"), Some(-12), "{name} -12abc");
            assert_eq!(f("0x10"), Some(0), "{name} stops at x");
            assert_eq!(f("1e3"), Some(1), "{name} stops at e");
            assert_eq!(f(".5"), None, "{name} .5 NaN");
            assert_eq!(f(""), None, "{name} empty NaN");
            assert_eq!(f("   "), None, "{name} ws-only NaN");
            assert_eq!(f("+"), None, "{name} bare sign NaN");
            assert_eq!(f("9223372036854775807"), Some(i64::MAX), "{name} i64::MAX");
            // -i64::MIN (9223372036854775808) is wrapper-specific — see the
            // per-overflow-model tests below; do not pin it here.
        }
    }

    // -- per-wrapper overflow models -----------------------------------------

    #[test]
    fn parse_int_overflow_signed_saturate() {
        // collection/search: negative overflow lands on i64::MIN exactly.
        assert_eq!(js_parse_int("99999999999999999999"), Some(i64::MAX));
        assert_eq!(js_parse_int("-99999999999999999999"), Some(i64::MIN));
    }

    #[test]
    fn parse_int_overflow_mag_saturate() {
        // auth_support fold: magnitude saturates, THEN the sign applies.
        assert_eq!(js_parse_int_unicode_sat("99999999999999999999"), Some(i64::MAX));
        assert_eq!(js_parse_int_unicode_sat("-99999999999999999999"), Some(-i64::MAX));
        assert_ne!(js_parse_int_unicode_sat("-99999999999999999999"), Some(i64::MIN));
    }

    #[test]
    fn parse_int_overflow_nan() {
        // mfx_payload / export: overflow is NaN.
        assert_eq!(js_parse_int_unicode("99999999999999999999"), None);
        assert_eq!(js_parse_int_unicode("-99999999999999999999"), None);
        assert_eq!(js_parse_int_nonsat("99999999999999999999"), None);
        assert_eq!(js_parse_int_nonsat("-99999999999999999999"), None);
    }

    // -- per-wrapper whitespace sets -----------------------------------------

    #[test]
    fn parse_int_whitespace_sets() {
        // ASCII vertical tab / form feed only in the ASCII-arm wrappers.
        assert_eq!(js_parse_int("\u{0b}\u{0c}7"), Some(7));
        assert_eq!(js_parse_int_nonsat("\u{0b}\u{0c}7"), Some(7));
        // NBSP (U+00A0, Unicode White_Space) is NOT stripped by the ASCII arm.
        assert_eq!(js_parse_int("\u{a0}5"), None);
        assert_eq!(js_parse_int_nonsat("\u{a0}5"), None);
        assert_eq!(js_parse_int_unicode("\u{a0}5"), Some(5));
        assert_eq!(js_parse_int_unicode_sat("\u{a0}5"), Some(5));
        // BOM (U+FEFF, NOT White_Space) only the mfx arm strips it.
        assert_eq!(js_parse_int("\u{feff}5"), None);
        assert_eq!(js_parse_int_unicode("\u{feff}5"), Some(5));
        assert_eq!(js_parse_int_unicode_sat("\u{feff}5"), None);
    }

    // -- response shapes -------------------------------------------------------

    #[tokio::test]
    async fn json_error_shape_no_etag() {
        let res = json_error(StatusCode::BAD_REQUEST, "bad thing");
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            hdr(&res, "content-type").as_deref(),
            Some("application/json; charset=utf-8")
        );
        assert!(res.headers().get("etag").is_none(), "axum-Json shape must not carry an ETag");
        assert_eq!(body(res).await, r#"{"error":"bad thing"}"#);
    }

    #[tokio::test]
    async fn json_error_express_shape_with_body_etag() {
        let res = json_error_express(StatusCode::NOT_FOUND, "nope");
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            hdr(&res, "content-type").as_deref(),
            Some("application/json; charset=utf-8")
        );
        let bytes = br#"{"error":"nope"}"#.to_vec();
        assert_eq!(
            hdr(&res, "etag").as_deref(),
            Some(crate::collection::body_etag(&bytes).as_str()),
            "ETag must be the Express weak body hash of the exact bytes"
        );
        assert_eq!(body(res).await, r#"{"error":"nope"}"#);
    }

    #[tokio::test]
    async fn json_response_serialises_like_serde() {
        // Key order follows the Value (serde_json Map default = BTreeMap order
        // unless preserving) — pin the shape downstream code relies on.
        let v = json!({ "b": 1, "a": "x" });
        let res = json_response(StatusCode::OK, &v);
        assert_eq!(res.status(), StatusCode::OK);
        let text = body(res).await;
        assert!(text.starts_with('{') && text.ends_with('}'), "{text}");
        assert!(text.contains("\"a\":\"x\"") && text.contains("\"b\":1"));
    }
}

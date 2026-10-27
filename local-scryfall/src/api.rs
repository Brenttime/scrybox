//! axum handlers. Responses are built by string assembly over pre-serialized
//! card JSON — zero re-serialization per request.

use crate::eval::eval;
use crate::index::prefix_edit_dist;
use crate::index::Index;
use crate::query::parse;
use axum::extract::{Json, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

pub struct AppState {
    pub ix: Index,
    pub started: Instant,
}

pub type SharedState = Arc<AppState>;

fn json_response(body: String, status: StatusCode) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8")),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response()
}

fn err(status: StatusCode, msg: &str) -> Response {
    json_response(format!("{{\"status_code\":{},\"details\":{}}}", status.as_u16(), json_str(msg)), status)
}

pub(crate) fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

const SUGGEST_TOKENS: &[&str] = &[
    "c:", "c>", "c<", "c=", "c!", "ci:", "t:", "o:", "n:", "m:", "a:", "cmc=", "cmc>=", "cmc<=",
    "pow=", "tou=", "loy=", "r:", "s:", "f:", "k:", "is:", "is:legendary", "is:spell",
    "is:permanent", "is:creature", "is:basic", "is:snow", "is:vanilla", "is:multicolored",
    "is:token", "is:dfc", "layout:", "keyword:", "not ", "or ", "(", ")",
];

/// Walk the AST for a unique: filter (last one wins, like Scryfall).
fn collect_unique(nodes: &[crate::query::Node], out: &mut Option<u8>) {
    use crate::query::{Filter, Node};
    for n in nodes {
        match n {
            Node::And(v) | Node::Or(v) => collect_unique(v, out),
            Node::Not(b) => collect_unique(std::slice::from_ref(b), out),
            Node::Filter(Filter::Unique(m)) => *out = Some(*m),
            _ => {}
        }
    }
}

/// Percent-encode a query-string component (form style, space → %20). Only
/// needed for echoing params back into self/next_page links.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Scryfall-shaped absolute link for a paged search response, built from the
/// request's Host so links point back at whichever origin reached us (direct
/// or through the Scrybox /scryfall-portal proxy). Callers that follow
/// pagination links (cardSets.js fetchMtgSet) then work unchanged.
fn page_link(headers: &HeaderMap, p: &HashMap<String, String>, next_key: &str, next_val: usize) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8420");
    let mut parts: Vec<String> = Vec::new();
    for (k, v) in p {
        if k == "page" || k == "offset" || k == next_key {
            continue;
        }
        parts.push(format!("{}={}", urlencode(k), urlencode(v)));
    }
    parts.push(format!("{next_key}={next_val}"));
    format!("http://{host}/cards/search?{}", parts.join("&"))
}

pub async fn search(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Query(p): Query<HashMap<String, String>>,
) -> Response {
    let t0 = Instant::now();
    let q = p.get("q").cloned().unwrap_or_default();
    let per: usize = p
        .get("limit")
        .or(p.get("page_size"))
        .or(p.get("per_page"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(75)
        .clamp(1, 500);
    let page: u32 = p
        .get("page")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .max(1);
    let offset: usize = p
        .get("offset")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or((page as usize - 1) * per);
    let order = p
        .get("order")
        .or(p.get("sort"))
        .map(|s| s.as_str())
        .unwrap_or("name");
    let desc = p.get("dir").map(|d| d == "desc").unwrap_or(false);

    let ix = &st.ix;
    let (nodes, mut warnings) = crate::query::parse_with(&q, Some(&ix.kw_enum));
    let mut bm = eval(&nodes, ix, &mut warnings);
    // Wave-4 reject parity: filters/parse arms that mirror upstream's hard
    // 400s (unknown k: enums, id: junk letters, frame:/lang:/r:/has: bad
    // values, m:{K}, `!(`, c:=) push a \u{1}-sentinel warning; turn that into
    // the same 400 Scryfall returns. Wave-5 item 1: the body code is
    // "bad_request" — verified against the gambit_w5 cached upstream error
    // bodies for the 65 THEIRS-REJECT rows (5 carry the full body:
    // code="bad_request", details="All of your terms were ignored."; the
    // other 60 are _status=400). Upstream never emits invalid_search_syntax
    // on /cards/search.
    if let Some(pos) = warnings.iter().position(|x| x.starts_with('\u{1}')) {
        let msg = warnings.remove(pos).trim_start_matches('\u{1}').to_string();
        warnings.retain(|x| !x.starts_with('\u{1}'));
        return json_response(
            format!(
                "{{\"object\":\"error\",\"code\":\"bad_request\",\"details\":{},\"status_code\":400}}",
                json_str(&msg)
            ),
            StatusCode::BAD_REQUEST,
        );
    }
    // Scryfall default search dedups to one printing per oracle in BOTH
    // total_cards and data (unique:cards). unique:prints disables dedup;
    // unique:art dedups per (oracle, illustration). otag:/id: queries were
    // already oracle-deduplicated via their gated bitmaps — same rule now
    // applies uniformly.
    //
    // THE DEFAULT-SEARCH POOL GATE (wave3, evidence-derived): upstream hides
    // oracles whose only printings live in hidden set_types/layouts/set-codes
    // (token sheets, memorabilia, promo boxes, minigames, art series, …).
    // Proven by the t:creature full-set diff: ours 20214 vs upstream 18859;
    // 1312 of 1368 extras are pool-invisible (759 token-only oracles, rest
    // hidden-set printings); masking with the calibrated id-pool bitmap
    // predicts 18899 (+0.22%). Skip the gate for include:extras/all, explicit
    // extras targeting (t:scheme, is:token, …) and set-targeted queries —
    // upstream auto-shows those — and for otag: queries (their bitmap is
    // gated by the otag calibration already guarding the guardrails).
    let mut unique: Option<u8> = None;
    collect_unique(&nodes, &mut unique);
    let art_mode = unique == Some(2);
    let dedup = unique != Some(1);
    if dedup && !art_mode && !crate::eval::uses_otag(&nodes) && !crate::eval::wants_extras(&nodes)
        && !crate::eval::uses_print_mode(&nodes)
    {
        bm &= &ix.id_vis_bits;
    }

    let total: u64 = if dedup {
        let mut seen: std::collections::HashSet<u64> =
            std::collections::HashSet::with_capacity(bm.len() as usize);
        if art_mode {
            for i in bm.iter() {
                let row = ix.card_oracle[i as usize] as u64;
                seen.insert((row << 32) | ix.cards[i as usize].illustration as u64);
            }
        } else {
            for i in bm.iter() {
                seen.insert(ix.card_oracle[i as usize] as u64);
            }
        }
        seen.len() as u64
    } else {
        bm.len()
    };

    let ord: &Vec<u32> = match order {
        "cmc" | "mana" | "mv" => &ix.orders.cmc,
        "rarity" => &ix.orders.rarity,
        "set" => &ix.orders.set,
        "released" | "date" => &ix.orders.released,
        _ => &ix.orders.name,
    };

    let mut ids: Vec<u32> = Vec::with_capacity(per);
    let mut skipped = 0usize;
    let seen_rows: Option<std::collections::HashSet<u64>> =
        dedup.then(|| std::collections::HashSet::with_capacity(per * 2));
    let mut seen_rows = seen_rows;
    let iter: Box<dyn Iterator<Item = u32>> =
        if desc { Box::new(ord.iter().rev().copied()) } else { Box::new(ord.iter().copied()) };
    for i in iter {
        if bm.contains(i) {
            if let Some(rows) = &mut seen_rows {
                let row = ix.card_oracle[i as usize] as u64;
                let key = if art_mode {
                    (row << 32) | ix.cards[i as usize].illustration as u64
                } else {
                    row
                };
                if !rows.insert(key) {
                    continue; // another printing of an already-listed oracle
                }
            }
            if skipped < offset {
                skipped += 1;
                continue;
            }
            ids.push(i);
            if ids.len() == per {
                break;
            }
        }
    }
    let has_more = ((offset + ids.len()) as u64) < total;

    // Scryfall puts its pagination cursor in `next_page`; callers that loop on
    // it (cardSets.js fetchMtgSet) stop after one page without it. Follow the
    // request's own paging style: offset in, offset out; otherwise page+1.
    let (self_link, next_link) = if p.contains_key("offset") {
        (
            page_link(&headers, &p, "offset", offset),
            has_more.then(|| page_link(&headers, &p, "offset", offset + ids.len())),
        )
    } else {
        (
            page_link(&headers, &p, "page", page as usize),
            has_more.then(|| page_link(&headers, &p, "page", page as usize + 1)),
        )
    };

    let joined: String = ids
        .iter()
        .map(|&i| &*ix.jsons[i as usize])
        .collect::<Vec<&str>>()
        .join(",");

    let warnings_json = serde_json::to_string(&warnings).unwrap_or_else(|_| "[]".into());
    // Envelope parity with Scryfall's /cards/search: object/type/context list
    // markers plus self/next_page links (additive for existing consumers).
    let body = format!(
        "{{\"object\":\"list\",\"type\":\"card\",\"context\":\"search\",\"data\":[{j}],\"cards\":[{j}],\"total_cards\":{total},\"total\":{total},\"page\":{page},\"page_size\":{per},\"per_page\":{per},\"offset\":{offset},\"has_more\":{has_more},\"self\":{self_j},\"next_page\":{next_j},\"took_ms\":{took},\"warnings\":{warnings_json}}}",
        j = joined,
        self_j = json_str(&self_link),
        next_j = match &next_link { Some(u) => json_str(u), None => "null".into() },
        took = t0.elapsed().as_micros() as f64 / 1000.0,
    );
    json_response(body, StatusCode::OK)
}

pub async fn suggest(
    State(st): State<SharedState>,
    Query(p): Query<HashMap<String, String>>,
) -> Response {
    use rayon::prelude::*;
    use std::collections::HashSet;
    let q = p.get("q").cloned().unwrap_or_default().trim().to_lowercase();
    let limit: usize = p
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(12)
        .clamp(1, 50);
    let ix = &st.ix;

    let mk_item = |i: u32| -> String {
        let c = &ix.cards[i as usize];
        let mut j = String::with_capacity(512);
        j.push_str("{\"name\":");
        j.push_str(&json_str(ix.s(c.name)));
        j.push_str(",\"set\":");
        j.push_str(&json_str(ix.s(c.set)));
        j.push_str(",\"type_line\":");
        j.push_str(&json_str(ix.s(c.type_line)));
        j.push_str(",\"image_small\":");
        j.push_str(&json_str(ix.s(c.image_small)));
        j.push_str(",\"id\":");
        j.push_str(&json_str(ix.s(c.id)));
        j.push('}');
        j
    };

    let mut items: Vec<String> = Vec::new();
    let mut last_name: u32 = u32::MAX;
    if !q.is_empty() {
        let start = ix
            .prefix
            .binary_search_by(|&(off, _)| ix.s(off).cmp(q.as_str()))
            .unwrap_or_else(|pp| pp);
        for &(off, i) in &ix.prefix[start..] {
            if !ix.s(off).starts_with(q.as_str()) {
                break;
            }
            let c = &ix.cards[i as usize];
            if c.name == last_name {
                continue; // dedupe reprints
            }
            last_name = c.name;
            items.push(mk_item(i));
            if items.len() >= limit {
                break;
            }
        }
    }

    // Scryfall-style fuzzy fallback when prefix matching is thin (e.g. "darc" -> Dark Ritual)
    if !q.is_empty() && items.len() < limit.min(4) && q.len() >= 3 {
        let cand = ix.fuzzy_name_cards(&q);
        let ids: Vec<u32> = cand.iter().collect();
        let hits: Vec<(u8, u32)> = ids
            .into_par_iter()
            .filter_map(|i| {
                let mut best = u8::MAX;
                crate::index::for_each_token(ix.name_lc(i), 1, &mut |t| {
                    if let Some(d) = prefix_edit_dist(&q, t) {
                        if d < best {
                            best = d;
                        }
                    }
                });
                (best != u8::MAX).then_some((best, i))
            })
            .collect();
        let mut hits = hits;
        hits.sort_unstable(); // distance first, then id
        let mut seen: HashSet<u32> = HashSet::new();
        for (_, i) in hits {
            if items.len() >= limit {
                break;
            }
            let name_off = ix.cards[i as usize].name;
            if !seen.insert(name_off) {
                continue;
            }
            items.push(mk_item(i));
        }
    }

    // token completions when the query ends with a partial key
    let mut tokens: Vec<&str> = Vec::new();
    if let Some(last) = q.split_whitespace().last() {
        if last.contains(':') || last.starts_with("is:") || last.starts_with("c") {
            tokens = SUGGEST_TOKENS
                .iter()
                .copied()
                .filter(|t| t.starts_with(last) && *t != last)
                .take(8)
                .collect();
        }
    }
    let tokens_json = tokens
        .iter()
        .map(|t| json_str(t))
        .collect::<Vec<_>>()
        .join(",");
    let body = format!(
        "{{\"suggestions\":[{}],\"tokens\":[{}]}}",
        items.join(","),
        tokens_json
    );
    json_response(body, StatusCode::OK)
}

pub async fn card(State(st): State<SharedState>, Path(id): Path<String>) -> Response {
    match lookup_card(&st.ix, &id) {
        Some(i) => json_response(st.ix.jsons[i].to_string(), StatusCode::OK),
        None => err(StatusCode::NOT_FOUND, &format!("card not found: {id}")),
    }
}

/// Resolve `id` by Scryfall uuid, `set/collector_number` (trailing `/lang`
/// ignored — this dump is English), or exact case-insensitive name.
fn lookup_card(ix: &Index, id: &str) -> Option<usize> {
    let id = id.trim_end_matches('/');
    // 1) by scryfall uuid
    if let Some(&i) = ix.id_map.get(id) {
        return Some(i as usize);
    }
    // 2) by set/collector_number e.g. "m19/295" (and "lea/1/en" from
    // getPrintingInLang — the third segment is the language we don't have)
    let segs: Vec<&str> = id.split('/').collect();
    if (segs.len() == 2 || segs.len() == 3) && !segs[0].is_empty() && !segs[1].is_empty() {
        if let Some(bm) = ix.set_idx.get(segs[0].to_lowercase().as_str()) {
            for i in bm.iter() {
                if ix.s(ix.cards[i as usize].collector_number) == segs[1] {
                    return Some(i as usize);
                }
            }
        }
    }
    // 3) exact name match (case-insensitive)
    let want = id.to_lowercase();
    ix.orders
        .name
        .iter()
        .find(|&&i| ix.name_lc(i) == want.as_str())
        .map(|&i| i as usize)
}

/// POST /cards/collection — Scryfall's bulk identifier lookup (≤75 per
/// request). The backend leans on this for price refresh and limited-mode
/// matching, so the local sidecar answers it or those features break under
/// LOCAL_SCRYFALL_ENABLED. Same shape: {object:"list", data:[…], not_found:[…]}.
pub async fn collection(State(st): State<SharedState>, Json(body): Json<serde_json::Value>) -> Response {
    let ix = &st.ix;
    let identifiers: Vec<serde_json::Value> = body
        .get("identifiers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if identifiers.len() > 75 {
        return err(StatusCode::BAD_REQUEST, "identifiers cannot contain more than 75 elements");
    }
    let mut data: Vec<String> = Vec::with_capacity(identifiers.len());
    let mut not_found: Vec<String> = Vec::new();
    for ident in identifiers {
        let key = if let Some(id) = ident.get("id").and_then(|v| v.as_str()) {
            id.to_string()
        } else if let (Some(set), Some(num)) = (
            ident.get("set").and_then(|v| v.as_str()),
            ident.get("collector_number").and_then(|v| v.as_str()),
        ) {
            format!("{set}/{num}")
        } else if let Some(name) = ident.get("name").and_then(|v| v.as_str()) {
            name.to_string()
        } else {
            not_found.push(ident.to_string());
            continue;
        };
        match lookup_card(ix, &key) {
            Some(i) => data.push(ix.jsons[i].to_string()),
            None => not_found.push(ident.to_string()),
        }
    }
    json_response(
        format!(
            "{{\"object\":\"list\",\"uri\":null,\"total_cards\":{},\"has_more\":false,\"data\":[{}],\"not_found\":[{}]}}",
            data.len(),
            data.join(","),
            not_found.join(",")
        ),
        StatusCode::OK,
    )
}

/// GET /api/sets — startup-cached set summaries (see Index::sets).
/// Query params:
///   q=<name/code substring>
///   type=<set_type>[,<set_type>…]   (multi allowed, e.g. type=box,core)
///   order=date|set|block|cards       (default: date desc)
/// Response keeps the historical fields (code,name,set_type,released_at,
/// card_count,sample_image) and adds block/parent_set/icon_svg_uri/
/// printed_size/digital/foil_only/nonfoil_only from the vendored Scryfall
/// set metadata (additive only).
pub async fn sets(
    State(st): State<SharedState>,
    Query(p): Query<HashMap<String, String>>,
) -> Response {
    sets_response(&st, &p, false)
}

/// Scryfall-compatible alias for `GET /sets` (the path the backend hits when
/// LOCAL_SCRYFALL_ENABLED swaps the baseURL). Content-negotiated: Scrybox
/// sends `Accept: application/json` and gets the Scryfall /sets envelope
/// (object:"list", data[] with per-record object:"set"); a browser
/// navigating to /sets keeps getting the 301 to the sets SPA.
pub async fn sets_list_scryfall(
    State(st): State<SharedState>,
    headers: HeaderMap,
    Query(p): Query<HashMap<String, String>>,
) -> Response {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if accept.contains("application/json") {
        sets_response(&st, &p, true)
    } else {
        (
            StatusCode::MOVED_PERMANENTLY,
            [(header::LOCATION, "/sets.html")],
        )
            .into_response()
    }
}

fn sets_response(st: &SharedState, p: &HashMap<String, String>, scryfall: bool) -> Response {
    let t0 = Instant::now();
    let ix = &st.ix;
    let typ = p.get("type").cloned().unwrap_or_default();
    let types: Vec<String> = typ
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let q = p.get("q").map(|s| s.to_lowercase()).unwrap_or_default();
    let order = p.get("order").map(|s| s.as_str()).unwrap_or("date");

    let mut rows: Vec<&crate::index::SetEntry> = ix
        .sets
        .iter()
        .filter(|e| {
            if !types.is_empty() && !types.iter().any(|t| ix.s(e.set_type) == *t) {
                return false;
            }
            if !q.is_empty()
                && !ix.s(e.name).to_lowercase().contains(&q)
                && !ix.s(e.code).to_lowercase().contains(&q)
            {
                return false;
            }
            true
        })
        .collect();

    let has_blocks = ix.sets.iter().any(|e| e.block_rank != u32::MAX);
    match order {
        "set" | "name" => rows.sort_unstable_by_key(|e| ix.s(e.name).to_lowercase()),
        "cards" => rows.sort_unstable_by(|a, b| {
            b.card_count
                .cmp(&a.card_count)
                .then_with(|| b.released.cmp(&a.released))
                .then_with(|| ix.s(a.name).cmp(&ix.s(b.name)))
        }),
        "block" | "group" => {
            if has_blocks {
                // Blocks in vendored (chronological) order first — sets within a
                // block by release date asc; unblocked sets after, newest first.
                rows.sort_unstable_by(|a, b| {
                    let ua = a.block_rank == u32::MAX;
                    let ub = b.block_rank == u32::MAX;
                    match (ua, ub) {
                        (true, true) => b
                            .released
                            .cmp(&a.released)
                            .then_with(|| ix.s(a.name).cmp(&ix.s(b.name))),
                        (true, false) => std::cmp::Ordering::Greater,
                        (false, true) => std::cmp::Ordering::Less,
                        (false, false) => a
                            .block_rank
                            .cmp(&b.block_rank)
                            .then_with(|| a.released.cmp(&b.released))
                            .then_with(|| ix.s(a.name).cmp(&ix.s(b.name))),
                    }
                });
            } else {
                rows.sort_unstable_by(|a, b| {
                    ix.s(a.set_type)
                        .cmp(&ix.s(b.set_type))
                        .then_with(|| b.released.cmp(&a.released))
                        .then_with(|| ix.s(a.name).cmp(&ix.s(b.name)))
                });
            }
        }
        _ => rows.sort_unstable_by(|a, b| {
            b.released
                .cmp(&a.released)
                .then_with(|| ix.s(a.name).cmp(&ix.s(b.name)))
        }),
    }

    let mut body = String::with_capacity(rows.len() * 320 + 64);
    // Same records under both envelopes; only the wrapper differs (additive
    // fields are harmless to the portal, which reads `.sets`).
    if scryfall {
        body.push_str("{\"object\":\"list\",\"data\":[");
    } else {
        body.push_str("{\"sets\":[");
    }
    for (n, e) in rows.iter().enumerate() {
        if n > 0 {
            body.push(',');
        }
        body.push_str("{\"object\":\"set\",\"code\":");
        body.push_str(&json_str(ix.s(e.code)));
        body.push_str(",\"name\":");
        body.push_str(&json_str(ix.s(e.name)));
        body.push_str(",\"set_type\":");
        body.push_str(&json_str(ix.s(e.set_type)));
        body.push_str(",\"released_at\":");
        if e.released == 0 {
            body.push_str("null");
        } else {
            let d = e.released.to_string();
            body.push_str(&format!("\"{}-{}-{}\"", &d[0..4], &d[4..6], &d[6..8]));
        }
        body.push_str(",\"card_count\":");
        body.push_str(&e.card_count.to_string());
        body.push_str(",\"sample_image\":");
        if e.sample_image == u32::MAX {
            body.push_str("null");
        } else {
            body.push_str(&json_str(ix.s(e.sample_image)));
        }
        // --- additive fields (vendored Scryfall set metadata) ---
        body.push_str(",\"block\":");
        if e.block == u32::MAX {
            body.push_str("null");
        } else {
            body.push_str(&json_str(ix.s(e.block)));
        }
        body.push_str(",\"parent_set\":");
        if e.parent_set == u32::MAX {
            body.push_str("null");
        } else {
            body.push_str(&json_str(ix.s(e.parent_set)));
        }
        // Scryfall's /sets shape names the parent's code `parent_set_code`;
        // the backend's listAllSets filters on it to skip child sets.
        body.push_str(",\"parent_set_code\":");
        if e.parent_set == u32::MAX {
            body.push_str("null");
        } else {
            body.push_str(&json_str(ix.s(e.parent_set)));
        }
        body.push_str(",\"icon_svg_uri\":");
        if e.icon == u32::MAX {
            body.push_str("null");
        } else {
            body.push_str(&json_str(ix.s(e.icon)));
        }
        body.push_str(",\"printed_size\":");
        if e.printed_size == u32::MAX {
            body.push_str("null");
        } else {
            body.push_str(&e.printed_size.to_string());
        }
        body.push_str(&format!(
            ",\"digital\":{},\"foil_only\":{},\"nonfoil_only\":{}",
            e.digital, e.foil_only, e.nonfoil_only
        ));
        body.push('}');
    }
    let tail = if scryfall {
        // Scryfall's /sets is single-page: has_more is always false.
        format!(
            "],\"has_more\":false,\"total\":{},\"took_ms\":{:.3}}}",
            rows.len(),
            t0.elapsed().as_micros() as f64 / 1000.0
        )
    } else {
        format!(
            "],\"total\":{},\"took_ms\":{:.3}}}",
            rows.len(),
            t0.elapsed().as_micros() as f64 / 1000.0
        )
    };
    body.push_str(&tail);
    json_response(body, StatusCode::OK)
}

/// GET /api/set/{code} — detail envelope for one set (record + block +
/// parent_set + type counts), pre-serialized once at startup
/// (Index::load_set_meta). 404 mirrors Scryfall's error shape.
pub async fn set_detail(State(st): State<SharedState>, Path(code): Path<String>) -> Response {
    let want = code.trim().to_lowercase();
    if want.is_empty() || want.len() > 32 {
        return json_response(
            format!(
                "{{\"object\":\"error\",\"code\":\"not_found\",\"status_code\":404,\"details\":\"Set {want} not found\"}}"
            ),
            StatusCode::NOT_FOUND,
        );
    }
    let ix = &st.ix;
    for e in ix.sets.iter() {
        if ix.s(e.code).eq_ignore_ascii_case(&want) {
            if let Some(j) = ix.set_detail_json.get(&e.code) {
                return json_response(j.to_string(), StatusCode::OK);
            }
            break;
        }
    }
    json_response(
        format!(
            "{{\"object\":\"error\",\"code\":\"not_found\",\"status_code\":404,\"details\":\"Set {want} not found\"}}"
        ),
        StatusCode::NOT_FOUND,
    )
}

pub async fn health(State(st): State<SharedState>) -> Response {
    let ix = &st.ix;
    let rss_mb = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<f64>().ok())
                .map(|kb| (kb / 1024.0) as usize)
        })
        .unwrap_or(0);
    let body = format!(
        "{{\"status\":\"ok\",\"cards\":{},\"unique_strings\":{},\"index_build_ms\":{},\"index_parse_and_build_ms\":{},\"uptime_s\":{},\"rss_mb\":{},\"formats\":{},\"server\":\"scryfall-api/local\"}}",
        ix.cards.len(),
        ix.strings.len(),
        ix.build_ms,
        ix.build_ms,
        st.started.elapsed().as_secs(),
        rss_mb,
        crate::index::NFORMATS,
    );
    json_response(body, StatusCode::OK)
}

//! AST evaluation -> RoaringBitmap of card ids.

use crate::index::{
    Card, Index, NFORMATS, F2_HIDDEN, F_BOOSTER_FLAG, F_HAS_IND, F_HAS_TEXT, F_HAS_WM, F_HIRES,
    F_SPOTLIGHT, FIN_ETCHED, FIN_FOIL, FIN_NONFOIL,
};
use crate::query::{
    Cmp, Filter, Node, NumField, SubOp,
    HAS_ALCHEMY, HAS_BOOSTER, HAS_ETCHED, HAS_FLAVOR, HAS_FOIL, HAS_HIRES, HAS_INDICATOR,
    HAS_NONFOIL, HAS_SPOTLIGHT, HAS_TEXT, HAS_VIDEO, HAS_WATERMARK,
};
use rayon::prelude::*;
use roaring::RoaringBitmap;

pub fn eval(nodes: &[Node], ix: &Index, warnings: &mut Vec<String>) -> RoaringBitmap {
    if nodes.is_empty() {
        return ix.universe.clone();
    }
    let mut parts: Vec<RoaringBitmap> = nodes.iter().map(|n| eval_node(n, ix, warnings)).collect();
    if parts.len() == 1 {
        return parts.pop().unwrap();
    }
    parts.sort_by_key(|b| b.len());
    let mut it = parts.into_iter();
    let mut acc = it.next().unwrap();
    for p in it {
        acc &= &p;
        if acc.is_empty() {
            break;
        }
    }
    acc
}

fn scan(ix: &Index, pred: impl Sync + Fn(u32) -> bool) -> RoaringBitmap {
    let v: Vec<u32> = (0..ix.cards.len() as u32)
        .into_par_iter()
        .filter(|&i| pred(i))
        .collect();
    RoaringBitmap::from_iter(v)
}

/// True if the AST contains any otag: filter. otag queries are reported
/// oracle-deduplicated (one representative printing per oracle), matching
/// scryfall.com's default-search total_cards semantics.
pub fn uses_otag(nodes: &[Node]) -> bool {
    nodes.iter().any(node_has_otag)
}

fn node_has_otag(n: &Node) -> bool {
    match n {
        Node::And(v) | Node::Or(v) => v.iter().any(node_has_otag),
        Node::Not(b) => node_has_otag(b),
        Node::Filter(Filter::OracleTag(_)) => true,
        _ => false,
    }
}

/// True if the AST contains any id: filter. Scryfall counts id: results as
/// DISTINCT ORACLES (verified: total_cards("id:wubrg") == total_cards("*") ==
/// 33816 == unique-card pool), so these queries report oracle-deduplicated
/// totals with one representative printing per oracle, like otag: queries.
pub fn uses_id(nodes: &[Node]) -> bool {
    nodes.iter().any(node_has_id)
}

fn node_has_id(n: &Node) -> bool {
    match n {
        Node::And(v) | Node::Or(v) => v.iter().any(node_has_id),
        Node::Not(b) => node_has_id(b),
        Node::Filter(Filter::Id(_)) => true,
        _ => false,
    }
}

fn eval_node(node: &Node, ix: &Index, w: &mut Vec<String>) -> RoaringBitmap {
    match node {
        Node::All => ix.universe.clone(),
        Node::And(parts) => {
            let mut bm: Vec<RoaringBitmap> =
                parts.iter().map(|p| eval_node(p, ix, w)).collect();
            bm.sort_by_key(|b| b.len());
            let mut it = bm.into_iter();
            match it.next() {
                None => ix.universe.clone(),
                Some(mut acc) => {
                    for p in it {
                        acc &= &p;
                    }
                    acc
                }
            }
        }
        Node::Or(parts) => {
            let mut acc = RoaringBitmap::new();
            for p in parts {
                acc |= &eval_node(p, ix, w);
            }
            acc
        }
        Node::Not(inner) => {
            let a = eval_node(inner, ix, w);
            let mut r = ix.universe.clone();
            r -= &a;
            r
        }
        Node::Text(word) => eval_text(word, ix),
        Node::Phrase(phrase) => eval_phrase(phrase, ix),
        Node::Filter(f) => eval_filter(f, ix, w),
    }
}

/// Free-text single word: Scryfall's bare-word search matches NAMES only
/// (probed: "power" == name~power; oracle/type matches are key: filters).
fn eval_text(word: &str, ix: &Index) -> RoaringBitmap {
    let mut bm = RoaringBitmap::new();
    if word.len() >= 2 {
        if let Some(b) = ix.name_tokens.get(word) {
            bm |= b;
        }
        if bm.len() < 2048 {
            let pfx = ix.prefix_hits(word, 2048);
            bm |= &pfx;
        }
        // Scryfall-like fuzzy matching as a FALLBACK only: plain-word tokens
        // with no solid exact/prefix hit ("lotsu" -> lotus, "lite" -> lightning,
        // "planezwalker" -> planeswalker). Exact/prefix hits always win.
        if bm.len() < 24 && word.len() >= 3 {
            bm |= &ix.fuzzy_name_cards(word);
        }
        if !bm.is_empty() {
            return bm;
        }
    }
    // fallback: substring on name
    let q = word.to_string();
    scan(ix, |i| ix.name_lc(i).contains(&q))
}

fn eval_phrase(phrase: &str, ix: &Index) -> RoaringBitmap {
    let q = phrase.to_string();
    let words: Vec<&str> = crate::index::tokenize(phrase);
    if !words.is_empty() {
        // candidate AND per name tokens: a phrase inside a name implies every
        // word is a name token.
        let and_field = |map: &std::collections::HashMap<String, RoaringBitmap>| -> Option<RoaringBitmap> {
            let mut acc: Option<RoaringBitmap> = None;
            for w in &words {
                let b = map.get(*w)?;
                acc = Some(match acc {
                    None => b.clone(),
                    Some(mut a) => {
                        a &= b;
                        a
                    }
                });
                if acc.as_ref().map_or(false, |a| a.is_empty()) {
                    return Some(RoaringBitmap::new());
                }
            }
            acc
        };
        let cand = match and_field(&ix.name_tokens) {
            Some(b) => b,
            None => RoaringBitmap::new(),
        };
        if !cand.is_empty() {
            let ids: Vec<u32> = cand.iter().collect();
            let kept: Vec<u32> = ids
                .into_par_iter()
                .filter(|&i| ix.name_lc(i).contains(&q))
                .collect();
            if !kept.is_empty() {
                return RoaringBitmap::from_iter(kept);
            }
        }
        // Name miss: quoted phrases also search the rules text / type line
        // (tests.sh contract: "deals 3 damage" -> Lightning Bolt). Live
        // Scryfall is stricter (names only) — this is a deliberate legacy
        // superset, documented in README.
        if let Some(c) = and_field(&ix.text_tokens) {
            if !c.is_empty() {
                let ids: Vec<u32> = c.iter().collect();
                let kept: Vec<u32> = ids
                    .into_par_iter()
                    .filter(|&i| ix.oracle_lc(i).contains(&q) || ix.type_lc(i).contains(&q))
                    .collect();
                return RoaringBitmap::from_iter(kept);
            }
        }
        return RoaringBitmap::new();
    }
    scan(ix, |i| ix.name_lc(i).contains(&q) || ix.oracle_lc(i).contains(&q))
}

fn prefix_union(_ix: &Index, map: &std::collections::HashMap<String, RoaringBitmap>, w: &str, cap: usize) -> RoaringBitmap {
    // words sorted? HashMap — iterate keys with starts_with (small maps, fine at ms scale)
    let mut bm = RoaringBitmap::new();
    if w.len() < 2 {
        return bm;
    }
    let mut count = 0u64;
    for (k, v) in map.iter() {
        if k.len() > w.len() && k.starts_with(w) {
            bm |= v;
            count += v.len();
            if count > cap as u64 {
                break;
            }
        }
    }
    bm
}

fn eval_filter(f: &Filter, ix: &Index, w: &mut Vec<String>) -> RoaringBitmap {
    match f {
        Filter::Colors { mask, op, identity } => {
            let (bits, subsets, exacts) = if *identity {
                (&ix.ci_bits, &ix.ci_subsets, &ix.ci_exacts)
            } else {
                (&ix.color_bits, &ix.subsets, &ix.exacts)
            };
            match op {
                SubOp::Exact => exacts[*mask as usize].clone(),
                SubOp::Subset => subsets[*mask as usize].clone(),
                SubOp::Include => {
                    if *mask == 0 {
                        return exacts[0].clone();
                    }
                    let mut bm: Option<RoaringBitmap> = None;
                    for b in 0..5 {
                        if mask & (1 << b) != 0 {
                            bm = Some(match bm {
                                None => bits[b].clone(),
                                Some(mut a) => {
                                    a &= &bits[b];
                                    a
                                }
                            });
                        }
                    }
                    bm.unwrap_or_else(|| ix.universe.clone())
                }
                SubOp::Exclude => {
                    let mut a = RoaringBitmap::new();
                    for b in 0..5 {
                        if mask & (1 << b) != 0 {
                            a |= &bits[b];
                        }
                    }
                    let mut r = ix.universe.clone();
                    r -= &a;
                    r
                }
            }
        }
        Filter::Type(v) => {
            if v.contains(' ') {
                let q = v.replace(' ', "");
                scan(ix, |i| ix.type_lc(i).replace(' ', "").contains(&q))
            } else {
                let mut bm = RoaringBitmap::new();
                if let Some(b) = ix.type_tokens.get(v.as_str()) {
                    bm |= b;
                }
                // Scryfall treats t:legend as t:legendary (supertype alias).
                if v == "legend" || v == "legendar" {
                    if let Some(b) = ix.type_tokens.get("legendary") {
                        bm |= b;
                    }
                }
                if bm.is_empty() {
                    bm = prefix_union(ix, &ix.type_tokens, v, 8192);
                }
                if bm.is_empty() {
                    let q = v.clone();
                    bm = scan(ix, |i| ix.type_lc(i).contains(&q));
                }
                bm
            }
        }
        Filter::Oracle(v) => {
            if v.contains(' ') {
                eval_phrase(v, ix) // oracle phrases: substring scan
            } else {
                // oracle-text-only index: reminder text stripped + light
                // stemming (calibrated vs live o:draw behaviour).
                let mut bm = RoaringBitmap::new();
                if let Some(b) = ix.otext_tokens.get(v.as_str()) {
                    bm |= b;
                }
                if bm.is_empty() {
                    let q = v.clone();
                    bm = scan(ix, |i| ix.oracle_lc(i).contains(&q));
                }
                bm
            }
        }
        Filter::Name(v) => {
            if !v.contains(' ') {
                let mut bm = RoaringBitmap::new();
                if let Some(b) = ix.name_tokens.get(v.as_str()) {
                    bm |= b;
                }
                if !bm.is_empty() {
                    return bm;
                }
            }
            let q = v.clone();
            scan(ix, |i| ix.name_lc(i).contains(&q))
        }
        Filter::Mana(v) => {
            let q = v.replace(['{', '}'], "").to_lowercase();
            scan(ix, |i| ix.mana_norm(i).contains(&q))
        }
        Filter::Artist(v) => {
            let q = v.clone();
            scan(ix, |i| ix.artist_lc(i).contains(&q))
        }
        Filter::Cmp { field, op, val } => {
            let v = *val;
            if v.is_nan() && *field != NumField::Pt {
                w.push("ignored numeric filter with non-numeric value".into());
                return ix.universe.clone();
            }
            let o = *op;
            if *field == NumField::Pt {
                // p/t "either" semantics: pow:X matches power OR toughness
                return scan(ix, |i| {
                    let c = &ix.cards[i as usize];
                    cmp_f(c.power, o, v) || cmp_f(c.toughness, o, v)
                });
            }
            let f = *field;
            scan(ix, |i| {
                let x = num_field(ix, i as usize, f);
                !x.is_nan() && cmp_f(x, o, v)
            })
        }
        Filter::CmpRange { field, lo, hi } => {
            let (lo, hi) = (*lo, *hi);
            let f = *field;
            scan(ix, |i| {
                let x = num_field(ix, i as usize, f);
                !x.is_nan() && x >= lo && x <= hi
            })
        }
        Filter::CrossCmp { a, op, b } => {
            let (a, o, b) = (*a, *op, *b);
            scan(ix, |i| {
                let va = num_field(ix, i as usize, a);
                let vb = num_field(ix, i as usize, b);
                !va.is_nan() && !vb.is_nan() && cmp_f(va, o, vb)
            })
        }
        Filter::Rarity(r) => {
            if *r as usize >= 8 {
                RoaringBitmap::new()
            } else {
                ix.rarity_idx[*r as usize].clone()
            }
        }
        Filter::Set(v) => {
            if let Some(b) = ix.set_idx.get(v.as_str()) {
                return b.clone();
            }
            // code prefix (e.g. s:fin)
            let mut bm = RoaringBitmap::new();
            for (k, b) in ix.set_idx.iter() {
                if k.starts_with(v.as_str()) {
                    bm |= b;
                }
            }
            if !bm.is_empty() {
                return bm;
            }
            // set name substring
            let q = v.clone();
            scan(ix, |i| ix.set_name_lc(i).contains(&q))
        }
        Filter::Format(f) => {
            if *f < NFORMATS {
                ix.format_legal[*f].clone()
            } else {
                RoaringBitmap::new()
            }
        }
        Filter::Keyword(v) => {
            let mut bm = RoaringBitmap::new();
            if let Some(b) = ix.keyword_tokens.get(v.as_str()) {
                bm |= b;
            }
            if bm.is_empty() {
                bm = prefix_union(ix, &ix.keyword_tokens, v, 4096);
            }
            if bm.is_empty() {
                let q = v.clone();
                bm = scan(ix, |i| ix.s(ix.cards[i as usize].keywords).contains(&q));
            }
            if bm.is_empty() {
                // bulk `keywords` coverage is sparse; fall back to keyword
                // words appearing in oracle text / type line (reminder text)
                if let Some(b) = ix.text_tokens.get(v.as_str()) {
                    bm |= b;
                }
                if let Some(b) = ix.type_tokens.get(v.as_str()) {
                    bm |= b;
                }
            }
            bm
        }
        Filter::Is(tag) => eval_is(tag, ix, w),
        // oracle tags: transitive closure over the tagger dump, restricted to
        // oracles visible in default search (calibrated visibility rule).
        // Unknown values -> warning + empty set; NEVER falls back to text search.
        Filter::OracleTag(v) => ix.otag_bitmap(v, w),
        // id: — "card name abbreviation" (docs lie; proven via live full-set
        // diffs): oracle color_identity ⊆ value letters; "c" = colorless.
        // Case-insensitive, order/dupes irrelevant (id:rb == id:BR == id:rrb).
        // Values mixing 'c' with colours (id:rc) or foreign letters (id:jb,
        // id:zz) make Scryfall drop the whole term with 400 "All of your
        // terms were ignored" — we mirror: warning + 0 results. Counts for
        // id: queries are oracle-deduplicated (see api.rs dedup + uses_id).
        Filter::Id(v) => {
            if let Some(rest) = v.strip_prefix('\u{0}') {
                // parsed from the non-operator `cardid:` key (Scryfall 400s it)
                w.push(format!(
                    "\"{rest}\" is not a Scryfall operator (cardid: does not exist; use id:) — all terms ignored, 0 results"
                ));
                return RoaringBitmap::new();
            }
            let mut norm: String = v.trim().chars().filter(|c| !c.is_whitespace()).collect();
            // operator forms: id<=xyz (subset, the default), id>=xyz
            // (superset), id=xyz (exact).
            let op = if let Some(r) = norm.strip_prefix("<=") {
                norm = r.to_string();
                0
            } else if let Some(r) = norm.strip_prefix(">=") {
                norm = r.to_string();
                1
            } else if let Some(r) = norm.strip_prefix("==").or(norm.strip_prefix('=')) {
                norm = r.to_string();
                2
            } else {
                0
            };
            if norm.is_empty() {
                w.push("id: with no value — 0 results".into());
                return RoaringBitmap::new();
            }
            if norm == "c" {
                return ix.id_bitmap(0);
            }
            // single digit 0-5: exactly that many colours in the identity
            // (live-verified: id:0==id:c==2977, id:1==24935, id:2==4845,
            // id:3==903, id:4==23, id:5==133; 6-9 / multi-digit => 400).
            if norm.len() == 1 && norm.as_bytes()[0].is_ascii_digit() {
                let n = (norm.as_bytes()[0] - b'0') as u32;
                if n > 5 {
                    w.push(format!(
                        "unknown id \"{v}\" — id: digits mean \"exactly N colours in the colour identity\" and only 0-5 exist; all terms ignored, 0 results"
                    ));
                    return RoaringBitmap::new();
                }
                let mut hits: Vec<u32> = Vec::with_capacity(4096);
                for (i, &row) in ix.card_oracle.iter().enumerate() {
                    let r = row as usize;
                    if ix.oracle_id_vis[r] && ix.oracle_ci[r].count_ones() == n {
                        hits.push(i as u32);
                    }
                }
                return RoaringBitmap::from_iter(hits);
            }
            let mut m = 0u8;
            if !norm.chars().all(|c| matches!(c, 'w' | 'u' | 'b' | 'r' | 'g' | 'c')) {
                // colour nicknames: id<=esper, id=jund ...
                if let Some(nm) = crate::query::color_nickname(&norm) {
                    m = nm;
                } else {
                    w.push(format!(
                        "unknown id \"{v}\" — id: values are colour abbreviations: 'c' (colorless) or letters from w/u/b/r/g; card colour identity must be a subset of the letters given; all terms ignored, 0 results"
                    ));
                    return RoaringBitmap::new();
                }
            } else {
            for ch in norm.chars() {
                match ch {
                    'w' => m |= 1,
                    'u' => m |= 2,
                    'b' => m |= 4,
                    'r' => m |= 8,
                    'g' => m |= 16,
                    _ => {
                        w.push(format!(
                            "unknown id \"{v}\" — id: values are colour abbreviations: 'c' (colorless) or letters from w/u/b/r/g; card colour identity must be a subset of the letters given; all terms ignored, 0 results"
                        ));
                        return RoaringBitmap::new();
                    }
                }
            }
            }
            match op {
                0 => ix.id_bitmap(m),
                1 => {
                    // id>=xyz: colour identity is a SUPERSET of the letters
                    let mut hits: Vec<u32> = Vec::with_capacity(4096);
                    for (i, &row) in ix.card_oracle.iter().enumerate() {
                        let r = row as usize;
                        if ix.oracle_id_vis[r] && (ix.oracle_ci[r] & m) == m {
                            hits.push(i as u32);
                        }
                    }
                    RoaringBitmap::from_iter(hits)
                }
                _ => {
                    // id=xyz: exact colour identity
                    ix.ci_exacts
                        .get(m as usize)
                        .cloned()
                        .unwrap_or_default()
                }
            }
        }
        // card-level tags live in the online tagger DB only.
        Filter::CardTag(v) => {
            w.push(format!(
                "card-level tags (tag:{v}) require the live tagger API — not supported offline; use otag: for oracle tags"
            ));
            RoaringBitmap::new()
        }
        Filter::Layout(v) => ix
            .layout_idx
            .get(v.as_str())
            .cloned()
            .unwrap_or_else(|| {
                w.push(format!("unknown layout: {v}"));
                RoaringBitmap::new()
            }),
        Filter::InField { kind, value } => {
            let q = match (kind, value.as_str()) {
                (0, "no-border") | (0, "noborder") => "borderless",
                (1, "futureseries") | (1, "future-series") => "future",
                (_, s) => s,
            }
            .to_string();
            let m = match kind {
                0 => &ix.border_idx,
                1 => &ix.frame_idx,
                2 => &ix.stamp_idx,
                3 => &ix.ffx_idx,
                _ => &ix.ptype_idx,
            };
            if let Some(b) = m.get(q.as_str()) {
                return b.clone();
            }
            // frame: values that are actually frame *effects* (tombstone,
            // legendary, inverted, ...) resolve against the ffx index.
            if *kind == 1 {
                if let Some(b) = ix.ffx_idx.get(q.as_str()) {
                    return b.clone();
                }
            }
            w.push(format!("unknown value '{value}' (field {kind})"));
            RoaringBitmap::new()
        }
        Filter::Text(v) => {
            // unknown filter key: Scryfall ignores unknown keys entirely
            // (probe: march:of time == *) — neutral term, match-all.
            let _ = v;
            ix.universe.clone()
        }
        // ---- parity additions (Oct 2026) ----
        Filter::Unique(_) => ix.universe.clone(), // handled by api-layer dedup
        Filter::Warn0(msg) => {
            w.push(msg.clone());
            RoaringBitmap::new()
        }
        Filter::NameExact(v) => {
            let q = v.clone();
            scan(ix, |i| ix.name_lc(i) == q.as_str())
        }
        Filter::Regex { field, pat } => {
            // Mana symbols in patterns ({T}, {W}) are literal braces; a raw
            // regex parse fails on them, so retry with braces escaped.
            let re = match regex::Regex::new(&format!("(?i){pat}"))
                .or_else(|_| {
                    let esc = pat.replace('{', "\\{").replace('}', "\\}");
                    regex::Regex::new(&format!("(?i){esc}"))
                }) {
                Ok(r) => r,
                Err(e) => {
                    w.push(format!("invalid regex: {e}"));
                    return RoaringBitmap::new();
                }
            };
            let f = *field;
            scan(ix, |i| match f {
                0 => re.is_match(&ix.name_lc(i)),
                1 => re.is_match(&ix.oracle_lc(i)),
                _ => re.is_match(&ix.type_lc(i)),
            })
        }
        Filter::ColorSpecial { identity, kind } => {
            let (id, k) = (*identity, *kind);
            scan(ix, move |i| {
                let c = &ix.cards[i as usize];
                let mask = if id { ix.oracle_ci[ix.card_oracle[i as usize] as usize] } else { c.colors };
                match k {
                    0 => mask.count_ones() >= 2,
                    1 => mask == 0,
                    _ => mask == 31,
                }
            })
        }
        Filter::Mana2 { need, gc, op } => {
            let (need, gc, op) = (need.clone(), *gc, *op);
            scan(ix, move |i| {
                let cost = ix.mana_norm(i);
                let (cc, pip) = mana_pips(cost);
                for (cmask, cnt) in &need {
                    if *cnt == 0 {
                        continue;
                    }
                    if cmask.count_ones() == 1 {
                        let b = cmask.trailing_zeros() as usize;
                        if pip[b] < *cnt {
                            return false;
                        }
                    } else {
                        // gate/hybrid req: card must cover every color once+ total
                        if !(0..5).all(|b| cmask & (1 << b) == 0 || pip[b] >= 1)
                            || (0..5).filter(|b| cmask & (1 << b) != 0).map(|b| pip[b]).sum::<u8>()
                                < *cnt
                        {
                            return false;
                        }
                    }
                }
                let total_needed: u8 = need.iter().map(|(_, c)| c).sum::<u8>() + gc;
                let total_have: u8 = cc + (0..5).map(|b| pip[b]).sum::<u8>();
                match op {
                    Cmp::Eq | Cmp::Ge => total_have >= total_needed,
                    Cmp::Le => total_have <= total_needed,
                    Cmp::Gt => total_have > total_needed,
                    Cmp::Lt => total_have < total_needed,
                    Cmp::Ne => total_have != total_needed,
                }
            })
        }
        Filter::ManaExact { syms, need_x } => {
            let (syms, need_x) = (syms.clone(), *need_x);
            scan(ix, move |i| {
                let raw = ix.s(ix.cards[i as usize].mana_cost).to_ascii_lowercase();
                let has = |want: &str| {
                    raw.split('{').any(|seg| seg.trim_end_matches('}') == want)
                };
                syms.iter().all(|s| has(s)) && (!need_x || has("x"))
            })
        }
        Filter::ManaCover { need, gc, op } => {
            // Containment semantics calibrated against upstream m: oracle
            // totals: card's PURE coloured pips must satisfy each required
            // colour count; hybrid/phyrexian symbols never satisfy a spec
            // colour; generic compare by op (m: / m>= -> >=, m> -> >, ...).
            let (need, gc, op) = (need.clone(), *gc, *op);
            scan(ix, move |i| {
                let raw = ix.s(ix.cards[i as usize].mana_cost).to_ascii_lowercase();
                let mut d = [0u8; 5];
                let mut cc = 0u8;
                for seg in raw.split('{') {
                    let body = seg.trim_end_matches('}');
                    if body.is_empty() {
                        continue;
                    }
                    if body.chars().all(|c| c.is_ascii_digit()) {
                        cc += body.parse::<u8>().unwrap_or(0);
                        continue;
                    }
                    if body.len() == 1 {
                        match body.as_bytes()[0] {
                            b'w' => d[0] += 1,
                            b'u' => d[1] += 1,
                            b'b' => d[2] += 1,
                            b'r' => d[3] += 1,
                            b'g' => d[4] += 1,
                            _ => {}
                        }
                    }
                }
                // Aggregate required counts per colour first: spec "GGU"
                // arrives as [(g,1),(g,1),(u,1)] and must demand TWO greens,
                // not one green checked twice.
                let mut req = [0u8; 5];
                for (m, c) in &need {
                    if m.count_ones() == 1 {
                        req[m.trailing_zeros() as usize] += c;
                    }
                }
                for b in 0..5 {
                    if d[b] < req[b] {
                        return false;
                    }
                }
                match op {
                    Cmp::Eq | Cmp::Ge => cc >= gc,
                    Cmp::Gt => cc > gc,
                    Cmp::Le => cc <= gc,
                    Cmp::Lt => cc < gc,
                    Cmp::Ne => cc != gc,
                }
            })
        }
        Filter::Devotion { reqs, total } => {
            let (reqs, total) = (reqs.clone(), *total);
            // bare numeric devotion (devotion>=5 / devotion:5): upstream treats
            // any threshold as a has-coloured-pips gate (proven: 0 and 99 give
            // the same total), so filter to cards with >=1 coloured pip.
            if reqs.is_empty() && total.is_some() {
                return scan(ix, |i| (0..5).map(|b| ix.pip_b(i, b)).sum::<u8>() >= 1);
            }
            scan(ix, move |i| {
                for (mask, need) in &reqs {
                    if (0..5).any(|b| mask & (1 << b) != 0)
                        && !(0..5).all(|b| mask & (1 << b) == 0 || ix.pip_b(i, b) >= *need)
                    {
                        return false;
                    }
                }
                if let Some(t) = total {
                    if (0..5).map(|b| ix.pip_b(i, b)).sum::<u8>() < t {
                        return false;
                    }
                }
                true
            })
        }
        Filter::HasField(kind) => {
            let k = *kind;
            scan(ix, move |i| {
                let c = &ix.cards[i as usize];
                match k {
                    HAS_WATERMARK => c.flags & F_HAS_WM != 0,
                    HAS_INDICATOR => c.flags & F_HAS_IND != 0,
                    HAS_FOIL => c.finishes & FIN_FOIL != 0,
                    HAS_ETCHED => c.finishes & FIN_ETCHED != 0,
                    HAS_NONFOIL => c.finishes & FIN_NONFOIL != 0,
                    HAS_HIRES => c.flags & F_HIRES != 0,
                    HAS_VIDEO => false,
                    HAS_BOOSTER => c.flags & F_BOOSTER_FLAG != 0,
                    HAS_SPOTLIGHT => c.flags & F_SPOTLIGHT != 0,
                    HAS_TEXT => c.flags & F_HAS_TEXT != 0,
                    HAS_FLAVOR => !ix.strings[c.flavor as usize].is_empty(),
                    HAS_ALCHEMY => &*ix.strings[c.set_type as usize] == "alchemy",
                    _ => false,
                }
            })
        }
        Filter::InVal(v) => {
            let q = v.clone();
            let mut rows: Vec<u32> = Vec::new();
            for m in [&ix.in_sets, &ix.in_set_types, &ix.in_games, &ix.in_langs, &ix.in_rarities] {
                if let Some(b) = m.get(q.as_str()) {
                    rows.extend(b.iter());
                }
            }
            if rows.is_empty() {
                // set names (in:allies)
                for code in ix.set_name_matches(&q) {
                    if let Some(b) = ix.in_sets.get(code.as_str()) {
                        rows.extend(b.iter());
                    }
                }
            }
            if rows.is_empty() {
                w.push(format!("unknown in: value '{v}'"));
                return RoaringBitmap::new();
            }
            let mut bm = RoaringBitmap::new();
            for r in rows {
                if let Some(p) = ix.oracle_prints.get(r as usize) {
                    bm |= p;
                }
            }
            bm
        }
        Filter::SetType(v) => {
            let q = match v.as_str() {
                "draftinnovation" | "draft_innovation" => "draft_innovation",
                "collector_central" | "collectorcentral" => "collector_central",
                s => s,
            }
            .to_string();
            ix.settype_idx.get(q.as_str()).cloned().unwrap_or_else(|| {
                w.push(format!("unknown set type '{v}'"));
                RoaringBitmap::new()
            })
        }
        Filter::Games(v) => {
            let q = match v.as_str() {
                "mtgo" | "mtgonline" | "mtg_online" => "mtgo",
                s => s,
            }
            .to_string();
            match q.as_str() {
                "paper" | "mtgo" | "arena" => ix
                    .games_idx
                    .get(q.as_str())
                    .cloned()
                    .unwrap_or_else(RoaringBitmap::new),
                "mtg" | "mtg_gold" | "gold" => {
                    let mut bm = RoaringBitmap::new();
                    for fi in [6usize, 7, 9] {
                        bm |= &ix.format_legal[fi];
                    }
                    if let Some(p) = ix.games_idx.get("paper") {
                        bm &= p;
                    }
                    bm
                }
                _ => {
                    w.push(format!("unknown game '{v}'"));
                    RoaringBitmap::new()
                }
            }
        }
        Filter::Lang(v) => {
            let q = match v.as_str() {
                "english" => "en",
                "spanish" => "es",
                "german" => "de",
                "french" => "fr",
                "italian" => "it",
                "japanese" => "ja",
                "portuguese" => "pt",
                "korean" => "ko",
                "russian" => "ru",
                "chinese_simplified" | "simplified_chinese" | "zh_hans" => "zhs",
                "chinese_traditional" | "traditional_chinese" | "zh_hant" => "zht",
                "any" | "all" => return ix.universe.clone(),
                s => s,
            };
            match ix.lang_idx.get(q) {
                Some(b) => b.clone(),
                None => {
                    w.push(format!("unknown lang '{v}'"));
                    RoaringBitmap::new()
                }
            }
        }
        Filter::Block(v) => {
            let q = v.replace(' ', "");
            if let Some(b) = ix.block_idx.get(q.as_str()) {
                return b.clone();
            }
            for (k, b) in &ix.block_idx {
                if k.replace(' ', "") == q {
                    return b.clone();
                }
            }
            w.push(format!("unknown block '{v}'"));
            RoaringBitmap::new()
        }
        Filter::Group(v) => ix.group_idx.get(v.as_str()).cloned().unwrap_or_else(|| {
            w.push(format!("unknown group '{v}'"));
            RoaringBitmap::new()
        }),
        Filter::Banned(fi) => ix.format_banned[*fi].clone(),
        Filter::Restricted(fi) => ix.format_restricted[*fi].clone(),
        Filter::Watermark(v) => {
            let q = v.clone();
            scan(ix, move |i| {
                let c = &ix.cards[i as usize];
                &*ix.strings[c.watermark as usize] == q.as_str()
            })
        }
        Filter::Produces(kind) => {
            let k = *kind;
            if k & 128 != 0 {
                // exact colour set (produces=wu): those colours and no others,
                // colourless production ignored.
                let kk = k & 31;
                return scan(ix, move |i| {
                    let pr = ix.cards[i as usize].produced & 31;
                    pr == kk
                });
            }
            if k & 64 != 0 {
                return scan(ix, |i| ix.cards[i as usize].produced != 0);
            }
            if k & 32 != 0 {
                return scan(ix, |i| ix.cards[i as usize].produced & 32 != 0);
            }
            scan(ix, move |i| {
                let pr = ix.cards[i as usize].produced;
                pr & k == k
            })
        }
        Filter::Parity(even) => {
            let e = *even;
            scan(ix, move |i| {
                let cmc = ix.cards[i as usize].cmc;
                !cmc.is_nan() && cmc.fract() == 0.0 && ((cmc as u32) % 2 == 0) == e
            })
        }
        Filter::NotVal(v) => match v.as_str() {
            "reprint" | "reprints" => {
                scan(ix, |i| {
                    let c = &ix.cards[i as usize];
                    let st = &ix.oracle_stats[ix.card_oracle[i as usize] as usize];
                    st.visible && !st.reprint && c.flags2 & F2_HIDDEN == 0
                })
            }
            "universesbeyond" | "universes_beyond" | "beyond" => scan(ix, |i| {
                ix.strings[ix.cards[i as usize].ptypes as usize]
                    .split(',')
                    .all(|t| t != "universesbeyond")
            }),
            other => {
                w.push(format!("unknown not: value '{other}' (ignored)"));
                ix.universe.clone()
            }
        },
        Filter::NewKind(mask) => {
            let m = *mask;
            scan(ix, move |i| ix.cards[i as usize].new_flags & m == m)
        }
        Filter::Released(op, v) => {
            let (o, q) = (*op, v.clone());
            let th: Option<u32> = if q == "now" || q == "today" {
                Some(ix.today)
            } else if q == "yesterday" {
                Some(ix.today.saturating_sub(1))
            } else if let Some(x) = q.strip_suffix(" years ago").or(q.strip_suffix(" year ago")) {
                let n: u32 = x.trim().parse().unwrap_or(0);
                let y = (ix.today / 10_000).saturating_sub(n);
                Some(y * 10_000 + ix.today % 10_000)
            } else if q.len() == 4 && q.chars().all(|c| c.is_ascii_digit()) {
                let y: u32 = q.parse().unwrap();
                match o {
                    Cmp::Lt => Some(y * 10_000),
                    Cmp::Gt | Cmp::Ge => Some(y * 10_000 + 12_31),
                    _ => Some(y * 10_000 + 6_15),
                }
            } else if q.matches(|c: char| c == '-').count() == 2 {
                let digits: String = q.chars().filter(|c| c.is_ascii_digit()).collect();
                digits.parse().ok()
            } else if (q.len() == 3 || q.len() == 4) && q.chars().all(|c| c.is_ascii_alphanumeric())
            {
                ix.set_idx
                    .get(q.as_str())
                    .and_then(|b| b.iter().next())
                    .map(|i| ix.cards[i as usize].released)
            } else {
                None
            };
            match th {
                None => {
                    w.push(format!("unknown released: value '{v}'"));
                    RoaringBitmap::new()
                }
                Some(t) => scan(ix, move |i: u32| -> bool {
                    let r = ix.cards[i as usize].released;
                    if r == 0 {
                        return false;
                    }
                    match o {
                        Cmp::Eq => r / 10_000 == t / 10_000 && t % 10_000 != 0 && (t % 10_000) < 100
                            || (t % 10_000 >= 100 && r == t)
                            || (t % 10_000 == 0 && r / 10_000 == t / 10_000),
                        Cmp::Ne => r != t,
                        Cmp::Lt => r < t,
                        Cmp::Le => r <= t,
                        Cmp::Gt => r > t,
                        Cmp::Ge => r >= t,
                    }
                }),
            }
        }
        Filter::PriceExists(kind) => {
            let k = *kind as usize;
            scan(ix, move |i| {
                let p = ix.cards[i as usize].prices[k];
                !p.is_nan() && p > 0.0
            })
        }
        Filter::FlavorText(v) => {
            let q = v.clone();
            scan(ix, move |i| {
                let c = &ix.cards[i as usize];
                ix.strings[c.flavor as usize].contains(q.as_str())
            })
        }
        Filter::FullOracle(v) => {
            let q = v.clone();
            scan(ix, move |i| ix.oracle_lc(i).contains(q.as_str()))
        }
    }
}

fn num_field(ix: &Index, i: usize, f: NumField) -> f32 {
    let c = &ix.cards[i];
    match f {
        NumField::Cmc => c.cmc,
        NumField::Power => c.power,
        NumField::Toughness => c.toughness,
        NumField::Loyalty => c.loyalty,
        NumField::Defense => c.defense,
        NumField::Pt => f32::NAN, // handled by the Cmp arm directly
        NumField::Colors => c.colors.count_ones() as f32,
        NumField::PriceUsd => {
            // TCGplayer unified listings: when a printing has no non-foil
            // price, Scryfall's usd filter uses the foil price (verified on
            // live diffs: e.g. 7ED Abyssal Specter usd=null foil=17.88 still
            // matches usd>5). Without this, usd>5 lands ~24% below upstream.
            let u = c.prices[0];
            if u.is_nan() {
                c.prices[1]
            } else {
                u
            }
        }
        NumField::PriceUsdFoil => c.prices[1],
        NumField::PriceTix => c.prices[2],
        NumField::PriceEur => c.prices[3],
        NumField::PriceEurFoil => c.prices[4],
        NumField::Edhrec => {
            if c.edhrec == u32::MAX {
                f32::NAN
            } else {
                c.edhrec as f32
            }
        }
        NumField::Prints => ix.oracle_stats[ix.card_oracle[i] as usize].n_prints as f32,
        NumField::Sets => ix.oracle_stats[ix.card_oracle[i] as usize].n_sets as f32,
        NumField::PaperPrints => ix.oracle_stats[ix.card_oracle[i] as usize].n_paper as f32,
        NumField::PaperSets => ix.oracle_stats[ix.card_oracle[i] as usize].n_paper_sets as f32,
        NumField::Artists => ix.oracle_stats[ix.card_oracle[i] as usize].n_artists as f32,
        NumField::Illustrations => ix.oracle_stats[ix.card_oracle[i] as usize].n_illus as f32,
        NumField::Year => {
            if c.released == 0 {
                f32::NAN
            } else {
                (c.released / 10_000) as f32
            }
        }
        NumField::Date => {
            if c.released == 0 {
                f32::NAN
            } else {
                c.released as f32
            }
        }
        NumField::Cn => {
            if c.cn_val == u32::MAX {
                f32::NAN
            } else {
                c.cn_val as f32
            }
        }
    }
}


#[inline]
/// Parse a normalized mana cost string ({W}{U} stripped to "wu") into
/// (generic pips, per-color pip counts). Hybrid/gate slashes count for both sides.
fn mana_pips(cost: &str) -> (u8, [u8; 5]) {
    let mut gc = 0u8;
    let mut pip = [0u8; 5];
    let mut num = String::new();
    let mut run = String::new();
    let mut flush = |run: &mut String, pip: &mut [u8; 5]| {
        if run.is_empty() {
            return;
        }
        let mut mask = 0u8;
        for ch in run.chars() {
            match ch {
                'w' => mask |= 1,
                'u' => mask |= 2,
                'b' => mask |= 4,
                'r' => mask |= 8,
                'g' => mask |= 16,
                _ => {}
            }
        }
        if mask != 0 {
            for b in 0..5 {
                if mask & (1 << b) != 0 {
                    pip[b] += 1;
                }
            }
        }
        run.clear();
    };
    for ch in cost.chars() {
        if ch.is_ascii_digit() {
            flush(&mut run, &mut pip);
            num.push(ch);
        } else if ch.is_alphabetic() {
            if !num.is_empty() {
                gc += num.parse::<u8>().unwrap_or(0);
                num.clear();
            }
            run.push(ch.to_ascii_lowercase());
        } else {
            // '/', '{', '}' or space: a separator inside or between symbols.
            // Do NOT flush on '/' so "w/u" stays one symbol; flush on braces/space.
            if ch != '/' {
                flush(&mut run, &mut pip);
            }
            if !num.is_empty() {
                gc += num.parse::<u8>().unwrap_or(0);
                num.clear();
            }
        }
    }
    flush(&mut run, &mut pip);
    if !num.is_empty() {
        gc += num.parse::<u8>().unwrap_or(0);
    }
    (gc, pip)
}

fn cmp_f(x: f32, op: Cmp, v: f32) -> bool {
    match op {
        Cmp::Eq => x == v,
        Cmp::Ne => x != v,
        Cmp::Lt => x < v,
        Cmp::Le => x <= v,
        Cmp::Gt => x > v,
        Cmp::Ge => x >= v,
    }
}

fn eval_is(tag: &str, ix: &Index, w: &mut Vec<String>) -> RoaringBitmap {
    use crate::index::*;
    let t = match tag {
        "doublefaced" | "double-faced" => "dfc",
        "full-art" => "fullart",
        "artseries" => "art_series",
        "playtest" => "emergency",
        "cmdr" => "commander",
        "story_spotlight" => "spotlight",
        "flyer" => "flying",
        "chase" => "planechase",
        "canopy" => "canopyland",
        other => other,
    };
    let flag = match t {
        "land" => Some(F_LAND),
        "creature" => Some(F_CREATURE),
        "legendary" | "legend" => Some(F_LEGENDARY),
        "basic" => Some(F_BASIC),
        "snow" => Some(F_SNOW),
        "multicolored" => Some(F_MULTICOLORED),
        "colorless" => Some(F_COLORLESS),
        "fullart" => Some(F_FULLART),
        "textless" => Some(F_TEXTLESS),
        "digital" => Some(F_DIGITAL),
        "promo" => Some(F_PROMO),
        "oversized" => Some(F_OVERSIZED),
        "artifact" => Some(F_ARTIFACT),
        "enchantment" => Some(F_ENCHANTMENT),
        "planeswalker" => Some(F_PLANESWALKER),
        "battle" => Some(F_BATTLE),
        "instant" => Some(F_INSTANT),
        "sorcery" => Some(F_SORCERY),
        "reserved" => Some(F_RESERVED),
        "gamechanger" => Some(F_GAMECHANGER),
        "spotlight" => Some(F_SPOTLIGHT),
        "booster" => Some(F_BOOSTER_FLAG),
        "preview" | "scryfallpreview" => Some(F_PREVIEW),
        _ => None,
    };
    if let Some(f) = flag {
        return scan(ix, |i| ix.cards[i as usize].flags & f != 0);
    }
    // keyword/type-token based tags
    let tok = |name: &str| -> RoaringBitmap {
        ix.type_tokens
            .get(name)
            .or_else(|| ix.text_tokens.get(name))
            .cloned()
            .unwrap_or_default()
    };
    // sheets that are neither castable spells nor permanents on Scryfall's
    // default search (token sheets, art series, planar/scheme/vanguard…).
    let non_aux = |c: &crate::index::Card| {
        c.flags2 & F2_TOKEN == 0
            && !matches!(
                &*ix.strings[c.layout as usize],
                "token"
                    | "art_series"
                    | "emblem"
                    | "planar"
                    | "scheme"
                    | "vanguard"
                    | "phenomenon"
                    | "double_faced_token"
                    | "contraption"
                    | "battle" // never a real layout; defensive
                    | "conspiracy"
            )
    };
    match t {
        "spell" => {
            // castable types only (F_SPELL), non-aux printings
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                c.flags & F_SPELL != 0 && non_aux(c)
            });
        }
        "permanent" => {
            // F_PERMANENT excludes instant/sorcery types and non-permanent
            // types at load; drop the funny/aux sheets like upstream does.
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                c.flags & F_PERMANENT != 0 && non_aux(c)
            });
        }
        "etched" => return scan(ix, |i| ix.cards[i as usize].finishes & FIN_ETCHED != 0),
        "flying" => return tok("flying"),
        "crew" => return tok("crew"),
        "mutant" => return tok("mutant"),
        "conjure" => return tok("conjure"),
        "ice" => return tok("ice"),
        "tile" => return tok("tile"),
        "lesson" => return tok("lesson"),
        "miracle" => return tok("miracle"),
        "changeling" => return tok("changeling"),
        "spree" => return tok("spree"),
        "saga" => return tok("saga"),
        "contraption" => return tok("contraption"),
        // ---- calibrated is: predicates (unique:cards diffs vs live Scryfall) ----
        "vanilla" => {
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                c.flags2 & F2_TOKEN == 0 && c.flags2 & F2_VANILLA != 0
            });
        }
        "dfc" => {
            // transform + modal_dfc + double-sided tokens + double-sided art
            // series (names carry "//"); levelers/splits/flips/meld are NOT
            // dfc upstream (proven: is:dfc list diff, 2925/2925 exact).
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                let lay = &*ix.strings[c.layout as usize];
                matches!(lay, "transform" | "modal_dfc" | "double_faced_token")
                    || (lay == "art_series" && c.flags2 & F2_NAME_SLASH != 0)
            });
        }
        "bear" => {
            // 2/2 creature with mana value 2, any face counts (transform
            // backs included); tokens excluded.
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                c.flags2 & F2_TOKEN == 0 && c.flags2 & F2_BEAR != 0
            });
        }
        "tricolor" => return scan(ix, |i| ix.cards[i as usize].colors.count_ones() == 3),
        "fourcolor" | "four-color" => {
            return scan(ix, |i| ix.cards[i as usize].colors.count_ones() == 4)
        }
        "fivecolor" | "five-color" | "rainbow" => {
            return scan(ix, |i| ix.cards[i as usize].colors.count_ones() == 5)
        }
        "hybrid" => {
            return scan(ix, |i| {
                let m = ix.mana_norm(i);
                m.contains('/') && !m.contains("/p")
            });
        }
        "phyrexian" => return scan(ix, |i| ix.mana_norm(i).contains("/p")),
        "mirror" => return scan(ix, |i| ix.oracle_lc(i).contains("mirror")),
        "historic" => {
            // legendary/artifact/planeswalker/saga (any face, incl. the
            // 'Historic' type word on newer prints); Arena-irrelevant print
            // families (funny/alchemy/custom reprints/memorabilia/promo) are
            // not historic upstream (8143/8188 with the rest of this rule).
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                if c.flags2 & (F2_TOKEN | F2_HIDDEN) != 0 {
                    return false;
                }
                let st = &*ix.strings[c.set_type as usize];
                if matches!(st, "funny" | "alchemy" | "masters" | "memorabilia" | "promo") {
                    return false;
                }
                c.flags2 & F2_HIST_WORD != 0
                    || c.flags & (F_LEGENDARY | F_ARTIFACT | F_PLANESWALKER) != 0
                    || ix.type_lc(i).contains("saga")
            });
        }
        "manland" => {
            if let Some(names) = ix.land_cycles.get("manland") {
                return scan(ix, |i| {
                    let c = &ix.cards[i as usize];
                    if c.flags2 & (F2_TOKEN | F2_LAND_MAIN) != F2_LAND_MAIN {
                        return false;
                    }
                    let n = ix.name_lc(i);
                    names.iter().any(|x| x == &n) && ix.oracle_lc(i).contains("creature")
                });
            }
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                c.flags & F_LAND != 0 && c.flags & F_CREATURE != 0
            });
        }
        "frenchvanilla" | "french-vanilla" => {
            // main-face creature whose text is only keyword abilities
            // (reminder text stripped); non-empty text (else is:vanilla).
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                if c.flags2 & (F2_TOKEN | F2_CRE_MAIN | F2_MAIN_TEXT)
                    != (F2_CRE_MAIN | F2_MAIN_TEXT)
                {
                    return false;
                }
                if c.flags2 & F2_PW_MAIN != 0 {
                    return false;
                }
                c.flags2 & F2_FRENCH != 0
            });
        }
        "commander" => {
            // "can be your commander": legendary creature/vehicle/spacecraft/
            // Background/planeswalker (main face legendary) or explicit
            // 'can be your commander' text; meld results and reversible cards
            // excluded; legality NOT part of the tag (Lurrus is:commander).
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                if c.flags2 & (F2_TOKEN | F2_HIDDEN) != 0 {
                    return false;
                }
                let lay = &*ix.strings[c.layout as usize];
                if lay == "reversible_card" {
                    return false;
                }
                if lay == "meld" && c.flags2 & F2_MELDTEXT == 0 {
                    return false;
                }
                if c.flags2 & F2_CBC != 0 {
                    return true;
                }
                c.flags2 & F2_LEG_MAIN != 0
                    && (c.flags2 & (F2_CRE_MAIN | F2_VEH_MAIN) != 0
                        || c.flags2 & (F2_PW_ANY | F2_BG_ANY) != 0)
            });
        }
        "oathbreaker" => {
            if let Some(names) = ix.land_cycles.get("oathbreaker") {
                return scan(ix, |i| {
                    let n = ix.name_lc(i);
                    names.iter().any(|x| x == &n)
                });
            }
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                ix.oracle_lc(i).contains("oathbreaker")
                    && (c.flags & F_PLANESWALKER != 0
                        || &*ix.strings[ix.cards[i as usize].layout as usize] == "vanguard")
            });
        }
        "brawler" => {
            // brawl-legal (per printing) legendary creature or planeswalker
            // or can-be-your-commander text (proven vs is:brawler=2418).
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                if c.flags2 & (F2_TOKEN | F2_HIDDEN) != 0 {
                    return false;
                }
                let lay = &*ix.strings[c.layout as usize];
                if lay == "reversible_card" {
                    return false;
                }
                let legal_brawl = ((c.legal >> (14 * 2)) & 3) as u32 == L_LEGAL;
                if !legal_brawl {
                    return false;
                }
                if c.flags2 & F2_CBC != 0 {
                    return true;
                }
                (c.flags2 & F2_LEG_MAIN != 0 && c.flags2 & (F2_CRE_MAIN | F2_VEH_MAIN) != 0)
                    || c.flags2 & F2_PW_ANY != 0
            });
        }
        "duelcommander" | "duel-commander" => {
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                if c.flags2 & (F2_TOKEN | F2_HIDDEN) != 0 {
                    return false;
                }
                let legal_duel = ((c.legal >> (18 * 2)) & 3) as u32 == L_LEGAL;
                if !legal_duel {
                    return false;
                }
                if c.flags2 & F2_CBC != 0 {
                    return true;
                }
                c.flags2 & F2_LEG_MAIN != 0
                    && (c.flags2 & (F2_CRE_MAIN | F2_VEH_MAIN) != 0
                        || c.flags2 & F2_PW_ANY != 0)
            });
        }
        "partner" => {
            // partner-family abilities (keyword tag or canonical phrasings);
            // Doctor Who "two commanders" cards count (proven vs 228).
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                if c.flags2 & (F2_TOKEN | F2_HIDDEN) != 0 {
                    return false;
                }
                let kw = ix.s(c.keywords).to_lowercase();
                if kw.contains("partner") {
                    return true;
                }
                let at = ix.oracle_lc(i);
                at.contains("partner with")
                    || at.contains("choose a partner")
                    || at.contains("two commanders")
                    || at.contains("second commander")
                    || at.contains("can have a partner")
                    || at.contains("as an additional commander")
            });
        }
        "companion" => {
            // Ikoria companion keyword on expansion/core prints only (10/10
            // exact; masters/funny "companion" prints are not tagged).
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                c.flags2 & F2_TOKEN == 0 && c.flags2 & F2_COMPANION != 0
            });
        }
        "party" => {
            // creature that can be cleric/warrior/rogue/wizard, including
            // changeling text ("is all creature types") — proven vs 3880.
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                c.flags2 & F2_TOKEN == 0
                    && c.flags2 & F2_CRE_MAIN != 0
                    && (c.flags2 & (F2_CLASS_PARTY | F2_ALLCT) != 0)
            });
        }
        "outlaw" => {
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                c.flags2 & F2_TOKEN == 0
                    && c.flags2 & F2_CRE_MAIN != 0
                    && (c.flags2 & (F2_CLASS_OUTLAW | F2_ALLCT) != 0)
            });
        }
        "modal" => return scan(ix, |i| ix.oracle_lc(i).contains("choose one")),
        "datestamped" | "buyabox" | "boosterfun" | "fnm" | "judgegift" | "prerelease"
        | "inset" | "promopack" | "league" | "boxtopper" | "openrunner" | "bundle"
        | "planeswalkerdeck" | "emergency" | "stamped" | "textlesspromo" => {
            let q = t.trim_end_matches('s').to_string();
            let q2 = t.to_string();
            return ix
                .ptype_idx
                .get(q.as_str())
                .or_else(|| ix.ptype_idx.get(q2.as_str()))
                .cloned()
                .unwrap_or_else(|| {
                    w.push(format!("unknown is:{tag} (ignored)"));
                    RoaringBitmap::new()
                });
        }
        "planechase" => {
            let mut bm = ix.settype_idx.get("planechase").cloned().unwrap_or_default();
            if let Some(l) = ix.layout_idx.get("planar") {
                bm |= l;
            }
            return bm;
        }
        "alchemy" => {
            // set_type alchemy printings, tokens and hidden custom sets out.
            // Known residual: upstream (855) additionally drops ~140 non-Arena
            // frame reprints (power-nine etc.) that bulk set_type still marks
            // alchemy; that pool membership is not derivable from this dump.
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                if c.flags2 & (F2_TOKEN | F2_HIDDEN) != 0 {
                    return false;
                }
                &*ix.strings[c.set_type as usize] == "alchemy"
            });
        }
        "funny" => return ix.settype_idx.get("funny").cloned().unwrap_or_else(RoaringBitmap::new),
        "arena" => return ix.games_idx.get("arena").cloned().unwrap_or_else(RoaringBitmap::new),
        "mtgo" => return ix.games_idx.get("mtgo").cloned().unwrap_or_else(RoaringBitmap::new),
        "paper" => return ix.games_idx.get("paper").cloned().unwrap_or_else(RoaringBitmap::new),
        "old" => return scan(ix, |i| matches!(ix.cards[i as usize].frame, FR_1993 | FR_1997)),
        "new" => {
            return scan(ix, |i| {
                matches!(ix.cards[i as usize].frame, FR_2003 | FR_2015 | FR_FUTURE)
            })
        }
        "borderless" => return scan(ix, |i| ix.cards[i as usize].border == BD_BORDERLESS),
        "colorshifted" => {
            return scan(ix, |i| {
                ix.cards[i as usize].ffx
                    & (1 << FFX_LIST.iter().position(|e| *e == "colorshifted").unwrap_or(13))
                    != 0
            })
        }
        "unique" => {
            // visible oracle with NO reprint-flagged non-token printing.
            // Upstream full list: 16168 (rule reproduces 16949 pre-visibility
            // gate; residual is stale-dump reprint drift, see calibration).
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                let st = &ix.oracle_stats[ix.card_oracle[i as usize] as usize];
                st.visible && !st.reprint && c.flags2 & F2_HIDDEN == 0
            });
        }
        "reprint" | "reprints" => {
            // visible oracle with >=1 reprint-flagged non-token printing.
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                let st = &ix.oracle_stats[ix.card_oracle[i as usize] as usize];
                st.visible && st.reprint && c.flags2 & F2_HIDDEN == 0
            });
        }
        "tranche" => return scan(ix, |i| !ix.cards[i as usize].defense.is_nan()),
        "video" => return RoaringBitmap::new(),
        "stamp" => return scan(ix, |i| ix.cards[i as usize].stamp != ST_NONE),
        "convert" => return scan(ix, |i| ix.oracle_lc(i).contains("convert")),
        "coordinate" => return scan(ix, |i| ix.oracle_lc(i).contains("coordinate")),
        // land cycles: exact name sets loaded from data/land_cycles.json
        "dual" | "fetchland" | "shockland" | "pathway" | "painland" | "bounceland" | "karoo"
        | "triland" | "bikeland" | "checkland" | "fastland" | "scryland" | "surveilland"
        | "canopyland" | "tricycleland" | "shadowland" => {
            match ix.land_cycles.get(t) {
                Some(names) => {
                    return scan(ix, |i| {
                        let n = ix.name_lc(i);
                        names.iter().any(|x| x == &n)
                    })
                }
                None => {
                    w.push(format!("unknown is:{tag} (ignored)"));
                    return RoaringBitmap::new();
                }
            }
        }
        _ => {}
    }
    // layout tags with token/custom exclusions + meld/split subdivisions
    // (calibrated against upstream lists; tokens never carry these tags).
    {
        let c0 = |i: u32| &ix.cards[i as usize];
        let layis = |i: u32, l: &str| &*ix.strings[c0(i).layout as usize] == l;
        match t {
            "transform" | "tdfc" => {
                return scan(ix, |i| {
                    layis(i, "transform") && c0(i).flags2 & (F2_TOKEN | F2_HIDDEN) == 0
                })
            }
            "mdfc" => {
                return scan(ix, |i| {
                    layis(i, "modal_dfc") && c0(i).flags2 & (F2_TOKEN | F2_HIDDEN) == 0
                })
            }
            "flip" => {
                return scan(ix, |i| {
                    layis(i, "flip") && c0(i).flags2 & (F2_TOKEN | F2_HIDDEN) == 0
                })
            }
            "leveler" => {
                return scan(ix, |i| {
                    layis(i, "leveler") && c0(i).flags2 & (F2_TOKEN | F2_HIDDEN) == 0
                })
            }
            "meld" => return scan(ix, |i| layis(i, "meld") && c0(i).flags2 & F2_HIDDEN == 0),
            "meldpart" => {
                return scan(ix, |i| {
                    layis(i, "meld")
                        && c0(i).flags2 & F2_MELDTEXT != 0
                        && c0(i).flags2 & F2_HIDDEN == 0
                })
            }
            "meldresult" => {
                return scan(ix, |i| {
                    layis(i, "meld")
                        && c0(i).flags2 & F2_MELDTEXT == 0
                        && c0(i).flags2 & F2_HIDDEN == 0
                })
            }
            "split" => {
                return scan(ix, |i| {
                    layis(i, "split") && c0(i).flags2 & (F2_TOKEN | F2_HIDDEN) == 0
                })
            }
            _ => {}
        }
    }
    // layout-based tags
    let layout = match t {
        "token" => "token",
        "double_faced_token" | "dfctoken" => "double_faced_token",
        "emblem" => "emblem",
        "scheme" => "scheme",
        "plane" => "planar",
        "phenomenon" => "phenomenon",
        "vanguard" => "vanguard",
        "art_series" => "art_series",
        "adventure" => "adventure",
        "class" => "class",
        "leveler" => "leveler",
        "mutate" => "mutate",
        "prototype" => "prototype",
        "case" => "case",
        "host" => "host",
        "prepare" => "prepare",
        "attract" | "attraction" => "attraction",
        "snapshot" => "snapshot",
        "transform" | "tdfc" => "transform",
        "mdfc" => "modal_dfc",
        "split" => "split",
        "flip" => "flip",
        "meld" | "meldpart" | "meldresult" => "meld",
        other => {
            w.push(format!("unknown is:{other} (ignored)"));
            return RoaringBitmap::new();
        }
    };
    ix.layout_idx
        .get(layout)
        .cloned()
        .unwrap_or_else(RoaringBitmap::new)
}

//! AST evaluation -> RoaringBitmap of card ids.

use crate::index::{
    Card, Index, NFORMATS, F2_HIDDEN, F2_NAME_SLASH, F2_TOKEN, F_BOOSTER_FLAG, F_FULLART,
    F_HAS_IND, F_HAS_TEXT,
    F_HAS_WM, F_HIRES, F_PROMO, F_SPOTLIGHT, F_TEXTLESS, FIN_ETCHED, FIN_FOIL, FIN_NONFOIL, STAR,
};
use crate::query::{
    Cmp, Filter, Mp, Node, NumField, SubOp,
    HAS_ALCHEMY, HAS_ARTIST, HAS_ARTSERIES, HAS_BOOSTER, HAS_ETCHED, HAS_EXTART, HAS_FLAVOR,
    HAS_FOIL, HAS_FULLART, HAS_HIRES, HAS_INDICATOR, HAS_NONFOIL, HAS_PROMO, HAS_REPRINT,
    HAS_SPOTLIGHT, HAS_TEXT, HAS_TEXTLESS, HAS_UNIQUE, HAS_VIDEO, HAS_WATERMARK,
};
use rayon::prelude::*;
use roaring::RoaringBitmap;

use crate::prints::{self, eff_price};

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

/// Parity w6a: accent fold for artist matching. The bulk dump stores most
/// artist names ASCII-folded already ("Zoltan Boros", "Gabor Szikszai") but
/// keeps the odd rare char ("Volkan Baǵa" with U+0135), while upstream
/// artist: matching is accent-insensitive ("volkan baga" == 255). Small
/// explicit table — only what MTG artist credits actually contain.
pub fn fold_artist(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'á'|'à'|'â'|'ä'|'ã'|'å'|'ā'|'ă'|'ą' => 'a',
            'é'|'è'|'ê'|'ë'|'ē'|'ě'|'ė'|'ę' => 'e',
            'í'|'ì'|'î'|'ï'|'ī'|'į'|'ı' => 'i',
            'ó'|'ò'|'ô'|'ö'|'õ'|'ø'|'ō'|'ő' => 'o',
            'ú'|'ù'|'û'|'ü'|'ū'|'ů'|'ű' => 'u',
            'ý'|'ÿ' => 'y',
            'ç'|'ć'|'č'|'ĉ' => 'c',
            'đ' => 'd',
            'ğ'|'ǵ'|'ĝ'|'ģ'|'ǥ' => 'g',
            'ĥ'|'ħ' => 'h',
            'ĵ' => 'j',
            'ķ'|'ĸ' => 'k',
            'ł'|'ĺ'|'ľ'|'ļ' => 'l',
            'ñ'|'ń'|'ŉ'|'ņ' => 'n',
            'ŕ'|'ř'|'ŗ' => 'r',
            'ś'|'š'|'ŝ'|'ş' => 's',
            'ţ'|'ť'|'ŧ' => 't',
            'ŵ' => 'w',
            'ź'|'ž'|'ż' => 'z',
            'æ' => 'a',
            'œ' => 'o',
            'ß' => 's',
            'þ' => 't',
            'ð' => 'd',
            'ŋ' => 'n',
            _ => c,
        })
        .collect()
}

fn scan(ix: &Index, pred: impl Sync + Fn(u32) -> bool) -> RoaringBitmap {
    let v: Vec<u32> = (0..ix.cards.len() as u32)
        .into_par_iter()
        .filter(|&i| pred(i))
        .collect();
    RoaringBitmap::from_iter(v)
}

/// Parity w6a: NumField/PriceExists kind code for the price fields
/// (0 usd, 1 usd_foil, 2 tix, 3 eur, 4 eur_foil — the side-index
/// effective-price numbering), None for everything else.
#[inline]
fn price_kind(f: NumField) -> Option<u8> {
    match f {
        NumField::PriceUsd => Some(0),
        NumField::PriceUsdFoil => Some(1),
        NumField::PriceTix => Some(2),
        NumField::PriceEur => Some(3),
        NumField::PriceEurFoil => Some(4),
        NumField::PriceUsdEtched => Some(5),
        _ => None,
    }
}

/// Parity w6a: ∃-over-all-printings price test against the all_cards side
/// index (effective price incl. foil/etched fallbacks). No-op when the side
/// index is not loaded — predicates keep the default-cards baseline.
#[inline]
fn side_price_any(ix: &Index, card: u32, kind: u8, test: impl Fn(f32) -> bool) -> bool {
    // tix (kind 2) never resolves through the side index: MTGO printings sit
    // under the SAME oracle as paper prints, and upstream only shows a tix
    // price when the DISPLAYED printing is the MTGO one (w6a calibration:
    // side-fallback tix pushed tix>1 to 3207 vs upstream 260).
    if kind == 2 {
        return false;
    }
    // Foil/etched figures legitimately live on variant sheets, so variation
    // rows may feed kinds 1/4/5; the plain USD/EUR comparators stay inside
    // the english non-promo non-token non-variation pool (w6a calibration:
    // unfiltered side rescue pushed frame:2015 usd>1 to 8735 vs 8165).
    let allow_variation = matches!(kind, 1 | 4 | 5);
    let block = prints::SR_PROMO | prints::SR_TOKEN | prints::SR_NONEN
        | if allow_variation { 0 } else { prints::SR_VARIATION };
    for p in prints::recs(ix, card) {
        if p.flags & block != 0 {
            continue;
        }
        let x = eff_price(p, kind);
        if !x.is_nan() && test(x) {
            return true;
        }
    }
    false
}

/// cn pair-comparison for one printing (raw collector number + numeric
/// prefix). Shared by the Card path and the w6a side-record path.
fn cn_match(op: Cmp, num: u32, full: Option<&str>, raw: &str, cn_val: u32) -> bool {
    let flat = raw.trim_end_matches('★').trim_end_matches('*').to_lowercase();
    let digits: String = flat.chars().take_while(|x| x.is_ascii_digit()).collect();
    match (&full, op) {
        // Parity w6a (data-driven from cached upstream page-1 oracle ids):
        // an alpha-suffixed '=' matches the exact string OR the bare-numeric
        // twin — upstream cn=12a returns prints whose collector number is
        // literally '12' (numeric part equal, print carries no suffix).
        (Some(f), Cmp::Eq) => {
            if flat.as_str() == *f {
                return true;
            }
            let qn: String = f.chars().take_while(|x| x.is_ascii_digit()).collect();
            !qn.is_empty() && digits == qn && flat.len() == digits.len()
        }
        (Some(f), Cmp::Ne) => flat.as_str() != *f,
        (None, Cmp::Eq) => !digits.is_empty() && cn_val == num,
        (None, Cmp::Ne) => digits.is_empty() || cn_val != num,
        (Some(f), _) => {
            if digits.is_empty() {
                return false; // alpha-only print never passes
            }
            let qs = f.trim_end_matches('★').trim_end_matches('*');
            match op {
                Cmp::Lt => cn_val < num || (cn_val == num && flat.as_str() < qs),
                Cmp::Le => cn_val < num || (cn_val == num && flat.as_str() <= qs),
                Cmp::Gt => cn_val > num || (cn_val == num && flat.as_str() > qs),
                Cmp::Ge => cn_val > num || (cn_val == num && flat.as_str() >= qs),
                _ => unreachable!(),
            }
        }
        (None, _) => {
            if digits.is_empty() {
                // pure-alpha bonus printing: sorts high, matches
                // lower-bound comparisons only.
                return matches!(op, Cmp::Ge | Cmp::Gt);
            }
            if flat.len() > digits.len() {
                return false; // suffixed prints are NULL for ranges
            }
            match op {
                Cmp::Lt => cn_val < num,
                Cmp::Le => cn_val <= num,
                Cmp::Gt => cn_val > num,
                Cmp::Ge => cn_val >= num,
                _ => unreachable!(),
            }
        }
    }
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

/// True when the query explicitly targets "extras" content (tokens, art
/// sheets, planar/scheme/vanguard sheets, promo-type printings) or broadens
/// visibility with include:extras/all. Upstream auto-shows such content even
/// though the default unique-card pool hides it (live-probed: t:scheme/t:plane
/// return the sheets, is:token returns tokens, plain text search does NOT).
/// api.rs skips the default-search pool gate for these queries.
pub fn wants_extras(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| node_wants_extras(n, false))
}

fn node_wants_extras(n: &Node, in_not: bool) -> bool {
    match n {
        Node::And(v) | Node::Or(v) => v.iter().any(|x| node_wants_extras(x, in_not)),
        Node::Not(b) => node_wants_extras(b, true),
        Node::Filter(f) => filter_wants_extras(f, in_not),
        _ => false,
    }
}

/// parity w5 §9 (borrow report): NEGATIVE print-attribute terms (-is:fnm,
/// -is:judge_gift, …) are oracle predicates over the VISIBLE pool upstream
/// (−is:fnm 33,827 = pool), unlike the positives which search the hidden
/// promo printings themselves (extras semantics, no dedup). Inside NOT(),
/// these tags therefore do NOT unlock extras.
const PRINT_ATTR_NEG: &[&str] = &[
    "datestamped", "buyabox", "boosterfun", "fnm", "judgegift", "judge_gift", "prerelease",
    "inset", "promopack", "league", "openrunner", "bundle", "emergency", "stamped",
    "textlesspromo", "serialized", "invitational", "gameday",
];

const EXTRAS_TYPE_WORDS: &[&str] = &[
    "token", "tokens", "scheme", "schemes", "vanguard", "vanguards", "phenomenon",
    "phenomena", "plane", "planes", "planar", "emblem", "emblems", "contraption",
    "contraptions", "conspiracy", "conspiracies", "attraction", "attractions", "art",
    "art series", "art_series", "artseries", "sticker", "stickers",
];
const EXTRAS_IS_WORDS: &[&str] = &[
    "token", "tokens", "double_faced_token", "doublefacedtoken", "dfctoken", "dfc",
    "doublefaced", "double-faced", "modal", "modal_dfc", "funny", "emblem",
    "emblems", "scheme", "schemes", "plane", "planes", "planar", "phenomenon",
    "phenomena", "vanguard", "vanguards", "contraption", "contraptions", "artseries",
    "art_series", "planeswalkerdeck", "boxtopper", "boxtoppers", "globalillustrator",
    "globalillustrators", "starter", "starters", "datestamped", "buyabox", "boosterfun",
    "fnm", "judgegift", "judge_gift", "prerelease", "inset", "promopack", "league",
    "openrunner", "bundle", "emergency", "stamped", "textlesspromo", "serialized",
    "invitational",
];
const EXTRAS_LAYOUTS: &[&str] = &[
    "token", "double_faced_token", "emblem", "planar", "scheme", "vanguard",
    "phenomenon", "contraption", "art_series", "front_card",
];

fn filter_wants_extras(f: &Filter, in_not: bool) -> bool {
    match f {
        Filter::IncludeVal(v) => v == "extras" || v == "all",
        // explicit set/collection targeting shows the printings searched
        // (live-probed: promo/expansion-hidden sets still return results).
        Filter::Set(_) | Filter::InVal(_) | Filter::SetType(_) => true,
        // printing-attribute targeting (artist, collector number, release
        // date, grouping, name/name-regex): upstream searches the printings
        // themselves and surfaces hidden-set work (probed: a:"proce" 134,
        // g:fin 779, t:creature cn>=400, name:/\bizzet\b/ 21 all match the
        // UNGATED totals; gating them regresses green rows).
        Filter::Artist(_) | Filter::Group(_) | Filter::Name(_) | Filter::NameExact(_)
        | Filter::Released(..) | Filter::CrossCmp { .. } => true,
        Filter::Cmp { field, .. } | Filter::CmpRange { field, .. } => {
            matches!(field, crate::query::NumField::Cn | crate::query::NumField::Date)
        }
        Filter::Layout(v) => EXTRAS_LAYOUTS.contains(&v.as_str()),
        Filter::Regex { field, .. } => *field == 2 || *field == 0, // t~/re/, name~/re/
        Filter::Type(v) => v.split_whitespace().any(|w| EXTRAS_TYPE_WORDS.contains(&w)),
        Filter::Is(v) => {
            EXTRAS_IS_WORDS.contains(&v.as_str())
                && !(in_not && PRINT_ATTR_NEG.contains(&v.as_str()))
        }
        // Parity w6a: has:artseries targets the hidden art-series printings
        // themselves — same extras semantics as is:artseries (golden 2,352;
        // without the gate exemption the id_vis pool mask zeros the scan).
        Filter::HasField(k) => *k == HAS_ARTSERIES,
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
        Node::TextKw(word) => eval_text_kw(word, ix),
        Node::Phrase(phrase) => eval_phrase(phrase, ix),
        Node::Filter(f) => eval_filter(f, ix, w),
    }
}

/// Free-text single word: Scryfall's bare-word search matches NAMES only
/// (probed: "power" == name~power; oracle/type matches are key: filters).
fn eval_text(word: &str, ix: &Index) -> RoaringBitmap {
    // token maps key on for_each_token output, which drops apostrophes
    // ("doctor's" -> key "doctor"); the raw query token still carries them,
    // so fold apostrophes before any map lookup (upstream text search
    // matches Doctor's-companion cards for the bare words).
    let stripped: String;
    let word = if word.contains('\'') {
        stripped = word.replace('\'', "");
        stripped.as_str()
    } else {
        word
    };
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
        // Upstream bare-word text is a NAME-SUBSTRING match, not token-only
        // (golden keyword-family residues: 'Up' after keyword:Level drop =
        // 333 names containing "up" incl. Abrupt Decay; 'strike' = 93 incl.
        // Sunstriker; 'night' = 332 incl. Nightmare). Union the substring
        // scan on top of exact/prefix instead of early-returning.
        bm |= &scan(ix, |i| ix.name_lc(i).contains(word));
        if !bm.is_empty() {
            return bm;
        }
    }
    // fallback: substring on name
    let q = word.to_string();
    scan(ix, |i| ix.name_lc(i).contains(&q))
}

/// STRICT free-word path (keyword-split residuals and `not:legal:X` trailing
/// words). Empirically calibrated against cached theirs (w6b): the upstream
/// answer for these trailing words is plain name-substring — adding the
/// exact-oracle-token OR (parent's original spec) overshoots every small
/// total (gigas 3 vs theirs 2, agenda 16 vs 1), so it is left out.
fn eval_text_kw(word: &str, ix: &Index) -> RoaringBitmap {
    let q = word.to_string();
    scan(ix, |i| ix.name_lc(i).contains(q.as_str()))
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
        Filter::CnCmp { op, num, full } => {
            // Parity w5 §1 (borrow-plan): per-printing collector-number
            // compare with (numeric_prefix, alpha_suffix) PAIR ordering —
            // "12b" > "12" > "9". Quantifier model validated on the bulk
            // dump against cached upstream answers: pure-numeric query
            // values use ∃-over-printings (suffixed prints are NULL for
            // range ops; alpha-only bonus prints satisfy >=/>), while an
            // alpha-suffixed query value ("12a") uses ∀-over-printings —
            // golden cn>12a 24987 EXACT, cn>=12a 24989 vs 25064 (−0.3%).
            // '=' keeps exact-string semantics (cn=12a). Pool visibility is
            // enforced uniformly by the api layer's id_vis_bits gate.
            let op = *op;
            let num = *num;
            let full = full.clone();
            let print_ok = {
                let full = full.clone();
                move |c: &Card| -> bool {
                    cn_match(
                        op,
                        num,
                        full.as_deref(),
                        &ix.strings[c.collector_number as usize],
                        c.cn_val,
                    )
                }
            };
            if full.is_some() && matches!(op, Cmp::Gt | Cmp::Ge) {
                // ∀-over-printings: oracle matches only when EVERY printing
                // satisfies the pair comparison. Parity w6a calibration: the
                // upper-bound alpha queries are ∀ upstream (golden cn>12a
                // 24987 vs 25018 with default prints; adding side prints
                // moved it away from golden), while < / <= with an alpha
                // suffix are ∃ upstream (golden cn<12a 3082: page-1 members
                // like ['178','171','2','319'] carry only ONE low print —
                // ∀ over the oracle's printings would exclude them).
                let mut bm = RoaringBitmap::new();
                for pr in ix.oracle_prints.iter() {
                    let mut all = true;
                    for i in pr.iter() {
                        if !print_ok(&ix.cards[i as usize]) {
                            all = false;
                            break;
                        }
                    }
                    if all && !pr.is_empty() {
                        bm |= pr;
                    }
                }
                return bm;
            }
            // Parity w6a: ∃-over-printings also quantifies over the
            // all_cards side records — variant sub-numbers (12a), bonus
            // sheets and gated prints now exist for the oracle. The
            // ∀-path above stays default-cards-only (w5 goldens validated
            // there).
            let alpha = full.is_some();
            let side = ix.side.loaded && alpha;
            scan(ix, move |i| {
                let c = &ix.cards[i as usize];
                // Pool visibility per candidate print: token sheets and
                // hidden rows do not satisfy an alpha-suffixed lower-bound
                // upstream (golden cn<12a 3082 sits well below the fully
                // ungated ∃ at 4201).
                let pool_ok = !alpha
                    || (c.flags2 & (F2_TOKEN | F2_HIDDEN) == 0);
                let own = pool_ok && print_ok(c);
                if !side {
                    return own;
                }
                if alpha && op == Cmp::Eq {
                    // cn=12a: keep the w5 exact-string OR bare-numeric-twin
                    // model (387 vs golden 290, DRIFT either way). w6a
                    // experiment note: all_cards holds only 8 literal '12a'
                    // rows (variants ride †/★/letter sheets elsewhere), so
                    // requiring a side exact-match collapsed 387->3 — the
                    // side index adds NOTHING for suffixed equality.
                    return own;
                }
                own || prints::recs(ix, i).iter().any(|p| {
                    // only prints from upstream's default search pool
                    // feed cn: quantification — variant sheets, promos,
                    // tokens and foreign rows stay out (golden calibration:
                    // including them pushed cn<12a to 4202 vs 3082).
                    p.flags
                        & (prints::SR_PROMO
                            | prints::SR_VARIATION
                            | prints::SR_TOKEN
                            | prints::SR_NONEN)
                        == 0
                        && cn_match(
                            op,
                            num,
                            full.as_deref(),
                            &ix.side.strings[p.cn as usize],
                            p.cn_val,
                        )
                })
            })
        }
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
                    if !*identity {
                        // bare c:/c> = ANY-FACE superset (see Index::color_incl)
                        return ix.color_incl[*mask as usize].clone();
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
                // phrases: contiguous substring on reminder-stripped oracle
                // text (whitespace-normalised). Golden: o:"forest walk" = 0
                // (cards print "forestwalk" as ONE word) — exact phrase, not
                // an AND of words.
                let q: String = v.split_whitespace().collect::<Vec<_>>().join(" ");
                scan(ix, |i| {
                    let t: String = ix.otext_lc(i).split_whitespace().collect::<Vec<_>>().join(" ");
                    t.contains(q.as_str())
                })
            } else {
                // Upstream o: matches the word as a SUBSTRING of oracle words
                // (tap -> taps/tapped/untap; angel -> archangel), on text with
                // (…) reminder stripped. Golden: oracle:tap ours(tokens)=1029
                // vs upstream 3770; substring scan lands ~3.7k.
                let mut bm = RoaringBitmap::new();
                if v.len() >= 2 {
                    if let Some(b) = ix.otext_tokens.get(v.as_str()) {
                        bm |= b;
                    }
                }
                let q = v.clone();
                let mut hits: Vec<u32> = ix
                    .otext_lc
                    .par_iter()
                    .enumerate()
                    .filter(|(_, &off)| ix.s(off).contains(q.as_str()))
                    .map(|(i, _)| i as u32)
                    .collect();
                bm.extend(hits.drain(..));
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
            // Parity w6a: artist: is ∃-over-printings AND accent-insensitive
            // (upstream artist:"volkan baga" = 255 incl. the 'Volkan Baǵa'
            // prints that only exist outside the default dump). Query is
            // folded, side strings are stored folded; the per-card mirror is
            // folded on the fly (µs-scale table map).
            let q = crate::eval::fold_artist(&v.to_lowercase());
            let side = ix.side.loaded;
            scan(ix, |i| {
                fold_artist(ix.artist_lc(i)).contains(&q)
                    || (side
                        && prints::recs(ix, i)
                            .iter()
                            .any(|p| ix.side.strings[p.artist as usize].contains(&q)))
            })
        }
        Filter::Cmp { field, op, val } => {
            let v = *val;
            if v.is_nan() {
                if matches!(
                    *field,
                    NumField::Pt
                        | NumField::Power
                        | NumField::Toughness
                        | NumField::Loyalty
                        | NumField::Defense
                ) {
                    // non-numeric p/t/loyalty/defense value (pow=3+, tou=2+…):
                    // upstream 404s — mirror with an empty result instead of
                    // leaking the pool through the ignored-filter universe.
                    return RoaringBitmap::new();
                }
                w.push("ignored numeric filter with non-numeric value".into());
                return ix.universe.clone();
            }
            let o = *op;
            // parity w5 §6: pow/tou/loyalty/defense comparisons satisfy on the
            // top-level value OR any card_faces[] value (packed), '*'
            // satisfies every inequality; pt: keeps "either" semantics.
            match *field {
                NumField::Pt => scan(ix, move |i| {
                    let c = &ix.cards[i as usize];
                    num_any(c.power, c.face_pow, o, v) || num_any(c.toughness, c.face_tou, o, v)
                }),
                NumField::Power => scan(ix, move |i| {
                    let c = &ix.cards[i as usize];
                    num_any(c.power, c.face_pow, o, v)
                }),
                NumField::Toughness => scan(ix, move |i| {
                    let c = &ix.cards[i as usize];
                    num_any(c.toughness, c.face_tou, o, v)
                }),
                NumField::Loyalty => scan(ix, move |i| {
                    let c = &ix.cards[i as usize];
                    num_any(c.loyalty, c.face_loy, o, v)
                }),
                NumField::Defense => scan(ix, move |i| {
                    let c = &ix.cards[i as usize];
                    num_any(c.defense, c.face_def, o, v)
                }),
                _ => {
                    let f = *field;
                    // Parity w6a: price comparisons are ∃-over-printings —
                    // own printing OR any all_cards side print (variant/
                    // promo/foreign rows carry prices the default dump
                    // lacks). Eq and Ne are deliberately NOT extended:
                    // goldens usd=5 (61 overshoot with ∃) and Ne trivially
                    // match everything once any print of any language is
                    // considered; own-value semantics stay closer upstream.
                    let rescue_high = matches!(o, Cmp::Gt | Cmp::Ge) && v >= 2.0;
                    let pk = if ix.side.loaded && !matches!(o, Cmp::Ne | Cmp::Eq) {
                        match price_kind(f) {
                            // usd_etched has no own-row slot: always side.
                            Some(5) => Some(5),
                            // other kinds: side fallback is off unless
                            // explicitly enabled (see prints::side_prices_enabled)
                            // — EXCEPT high-band lower-bound queries (v>=2),
                            // where genuine special-drop printings missing
                            // from default-cards are real matches (w5 DRIFT
                            // usd>5 6125→6264 vs 6229; low-band 0.25/1 rows
                            // stay exactly w5 because v<2 never rescues).
                            Some(k) if prints::side_prices_enabled() => Some(k),
                            Some(k @ (0 | 1 | 3 | 4)) if rescue_high => Some(k),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    scan(ix, move |i| {
                        let x = num_field(ix, i as usize, f);
                        if !x.is_nan() {
                            // Own printing carries the price field: it is
                            // authoritative (w5 model). w6a calibration:
                            // letting side prints rescue a FAILING own price
                            // pushed usd>0.25 to 21365 vs upstream 21115 —
                            // upstream prices the DISPLAYED printing.
                            return cmp_f(x, o, v);
                        }
                        match pk {
                            Some(k) => side_price_any(ix, i, k, |y| cmp_f(y, o, v)),
                            None => false,
                        }
                    })
                }
            }
        }
        Filter::CmpRange { field, lo, hi } => {
            let (lo, hi) = (*lo, *hi);
            let f = *field;
            let pk = if ix.side.loaded {
                match price_kind(f) {
                    Some(5) => Some(5),
                    Some(k) if prints::side_prices_enabled() => Some(k),
                    _ => None,
                }
            } else {
                None
            };
            scan(ix, |i| {
                let x = num_field(ix, i as usize, f);
                if !x.is_nan() {
                    return x >= lo && x <= hi;
                }
                match pk {
                    Some(k) => side_price_any(ix, i, k, |y| y >= lo && y <= hi),
                    None => false,
                }
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
            } else if *r == 6 && ix.side.loaded {
                // Parity w6a: rarity:timeshifted — the timeshifted prints
                // (Time Shift-style special drops) exist only in the
                // all_cards bulk: default-cards gave 0 vs upstream 347.
                let bit = 1u8 << 6;
                scan(ix, |i| {
                    ix.cards[i as usize].rarity == 6
                        || ix.side.rar_or[ix.card_oracle[i as usize] as usize] & bit != 0
                })
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
            // EXACT keyword-phrase semantics (golden: keyword:Double = 138
            // "Double" keyword, not 288 word-token union; keyword:Companion
            // = 10, keyword:Void = 15). Hit on the full-phrase index wins
            // unconditionally; the word/prefix/fuzzy chain below is only the
            // legacy path for offline parse() callers without the enum.
            if let Some(b) = ix.keyword_phrase.get(v.as_str()) {
                return b.clone();
            }
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
                // Hyphen/apostrophe keyword enums (Jump-start, Doctor's
                // companion, Hero's Reward, Power-up, Web-slinging,
                // Council's dilemma) are valid upstream (live-probed
                // 200/13, 200/25, 200/37, ...) but the raw value never
                // matches the word-keyed token maps — the tokenizer keeps
                // the hyphen/apostrophe while the maps split on them. Last
                // resort: normalized-phrase scan over oracle text, type
                // line and the keywords blob ('' and '-' fold to spaces so
                // "Doctor's companion" matches the reminder text).
                let norm_kw = |s: &str| -> String {
                    s.to_lowercase()
                        .chars()
                        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
                        .collect::<String>()
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                let target = norm_kw(&v);
                if !target.is_empty() && target.contains(' ') {
                    bm = scan(ix, |i| {
                        let c = &ix.cards[i as usize];
                        norm_kw(&ix.s(c.oracle_text)).contains(&target)
                            || norm_kw(&ix.s(c.type_line)).contains(&target)
                            || norm_kw(&ix.s(c.keywords)).contains(&target)
                    });
                    if !bm.is_empty() {
                        return bm;
                    }
                }
                if !v.contains(' ') {
                    // single unknown word: before rejecting, allow the
                    // reminder-text fallback — apostrophe/hyphen keywords
                    // (Doctor's companion, Jump-start) arrive word-broken
                    // from the tokenizer but ARE valid upstream enums.
                    let in_text = ix.text_tokens.contains_key(v.as_str())
                        || ix.type_tokens.contains_key(v.as_str());
                    if !in_text {
                        // upstream 400s unknown single-word keyword enums
                        // (k:destroy / keyword:angel) — mirror
                        w.push(format!("\u{1}\"{v}\" is not a valid keyword"));
                        return bm;
                    }
                }
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
            } else if let Some(r) = norm.strip_prefix('<') {
                norm = r.to_string();
                3
            } else if let Some(r) = norm.strip_prefix('>') {
                norm = r.to_string();
                4
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
                        "\u{1}invalid id \"{v}\" — id: values are colour abbreviations: 'c' (colorless) or letters from w/u/b/r/g"
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
                1 | 4 => {
                    // id>=xyz: colour identity is a SUPERSET of the letters;
                    // id>xyz: PROPER superset (live-probed: id>bg t:instant
                    // 17 vs id>=bg 47 — the 30 exactly-bg instants drop out).
                    let strict = op == 4;
                    let mut hits: Vec<u32> = Vec::with_capacity(4096);
                    for (i, &row) in ix.card_oracle.iter().enumerate() {
                        let r = row as usize;
                        let ci = ix.oracle_ci[r];
                        if ix.oracle_id_vis[r] && (ci & m) == m && (!strict || ci != m) {
                            hits.push(i as u32);
                        }
                    }
                    RoaringBitmap::from_iter(hits)
                }
                3 => {
                    // id<xyz: PROPER subset (live-probed: id<esper t:instant
                    // 2363 vs id<=esper 2372 — the 9 exactly-esper drop out).
                    let mut hits: Vec<u32> = Vec::with_capacity(4096);
                    for (i, &row) in ix.card_oracle.iter().enumerate() {
                        let r = row as usize;
                        let ci = ix.oracle_ci[r];
                        if ix.oracle_id_vis[r] && (ci & !m) == 0 && ci != m {
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
        // art:/atag:/arttag: are Tagger *illustration* tags (art:squirrel =
        // art that contains a squirrel). Not artist aliases (probed upstream:
        // art:squirrel==arttag:squirrel==199, artist:"Larry Elmore"==14,
        // arttag:"Larry Elmore"==0). The illustration-tag dump is not bundled.
        Filter::ArtTag(v) => {
            w.push(format!(
                "illustration tags (art:/atag:/arttag:{v}) require the live Tagger API — not supported offline; to search by illustrator use artist: — 0 results"
            ));
            RoaringBitmap::new()
        }
        // include:extras/all — neutral term; api.rs skips the default-search
        // pool gate when it sees this.
        Filter::IncludeVal(_) => ix.universe.clone(),
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
                // frame:retro (cached theirs 7289): bulk has no frame
                // "retro" value; the retro-frame printings are exactly the
                // frame_effects "inverted" population (Kamigawa-style
                // retro reprints) — resolve against the ffx index.
                (1, "retro") => "inverted",
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
        Filter::ManaSyms(parts) => {
            // parity w5 §6 (borrow report L73–79): containment counts the
            // UNION of mana_cost and every card_faces[].mana_cost per print;
            // spec parts must each appear (no adjacency requirement —
            // golden m:{2}{W} sim 3,731 vs 3,697 needed union+independent,
            // adjacency starved to 3,308), generic parts compare as the
            // card's MAX generic pip, and hybrid/phyrexian/gold segments
            // contribute nothing to plain colour needs (they match only
            // when spelled explicitly — sim overshoot 686 proved this).
            let parts = parts.clone();
            scan(ix, move |i| {
                let raw = ix.cost_all(i).to_ascii_lowercase();
                let chars: Vec<char> = raw.chars().collect();
                let mut segs: Vec<String> = Vec::new();
                let mut k = 0usize;
                while k < chars.len() {
                    if chars[k] == '{' {
                        match chars[k + 1..].iter().position(|&x| x == '}') {
                            Some(e) => {
                                segs.push(chars[k + 1..k + 1 + e].iter().collect());
                                k += e + 2;
                            }
                            None => break,
                        }
                    } else {
                        k += 1;
                    }
                }
                if segs.is_empty() {
                    return parts.is_empty();
                }
                let mut pips = [0u8; 5];
                let mut maxg = 0u32;
                for s in &segs {
                    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
                        maxg = maxg.max(s.parse::<u32>().unwrap_or(0));
                    } else if s.len() == 1 {
                        if let Some(b) = "wubrg".find(s.as_str()) {
                            pips[b] += 1;
                        }
                    }
                }
                let mut need = [0u8; 5];
                for p in &parts {
                    match p {
                        Mp::Lit(s) => {
                            if s.len() == 1 {
                                if let Some(b) = "wubrg".find(s.as_str()) {
                                    need[b] += 1;
                                } else if !segs.iter().any(|x| x == s) {
                                    return false;
                                }
                            } else if !segs.iter().any(|x| x == s) {
                                return false;
                            }
                        }
                        Mp::Pip(m) => {
                            if m.count_ones() == 1 {
                                need[m.trailing_zeros() as usize] += 1;
                            } else {
                                for b in 0..5 {
                                    if m & (1 << b) != 0 && pips[b] < 1 {
                                        return false;
                                    }
                                }
                            }
                        }
                        Mp::GenericMin(n) => {
                            if maxg < *n {
                                return false;
                            }
                        }
                        Mp::AnyCost => {}
                    }
                }
                for b in 0..5 {
                    if need[b] > pips[b] {
                        return false;
                    }
                }
                true
            })
        }
        Filter::Fatal400(msg) => {
            // sentinel-prefixed warning: api.rs turns this into a 400
            w.push(format!("\u{1}{msg}"));
            RoaringBitmap::new()
        }
        Filter::Ignored(msg) => {
            // unreachable: parse() prunes these (upstream drop-semantics).
            // Defensive fallback: neutral element for AND conjunctions.
            w.push(format!("invalid expression \"{msg}\" was ignored"));
            ix.universe.clone()
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
                // Parity w6a: finish predicates are ∃-over-printings —
                // etched/foil variants live only in the all_cards bulk; fx
                // is the oracle's OR-ed finishes across ALL printings.
                let fx = if ix.side.loaded {
                    ix.side.fin_or[ix.card_oracle[i as usize] as usize]
                } else {
                    0
                };
                match k {
                    HAS_WATERMARK => c.flags & F_HAS_WM != 0,
                    HAS_INDICATOR => c.flags & F_HAS_IND != 0,
                    HAS_FOIL => c.finishes & FIN_FOIL != 0 || fx & FIN_FOIL != 0,
                    HAS_ETCHED => c.finishes & FIN_ETCHED != 0 || fx & FIN_ETCHED != 0,
                    HAS_NONFOIL => c.finishes & FIN_NONFOIL != 0,
                    HAS_HIRES => c.flags & F_HIRES != 0,
                    HAS_VIDEO => false,
                    HAS_BOOSTER => c.flags & F_BOOSTER_FLAG != 0,
                    HAS_SPOTLIGHT => c.flags & F_SPOTLIGHT != 0,
                    HAS_TEXT => c.flags & F_HAS_TEXT != 0,
                    HAS_FLAVOR => !ix.strings[c.flavor as usize].is_empty(),
                    HAS_ALCHEMY => &*ix.strings[c.set_type as usize] == "alchemy",
                    // parity w5 §4 (borrow-plan): these arms previously fell
                    // through to a catch-all binding (the HAS_* constants
                    // were never imported — the arms compiled as wildcard
                    // patterns, making every one of them return the reprint
                    // bitmap: the flat 17,602 for has:artist/textless/…).
                    // Rules now follow the validated print-attribute spec.
                    HAS_REPRINT => c.reprint,
                    HAS_UNIQUE => {
                        // has:unique == is:unique: visible oracle never
                        // flagged reprint (mirrors the is:unique arm).
                        let r = ix.card_oracle[i as usize] as usize;
                        let st = &ix.oracle_stats[r];
                        st.visible && !st.reprint && c.flags2 & F2_HIDDEN == 0
                    }
                    HAS_ARTIST => !ix.strings[c.artist as usize].is_empty(),
                    HAS_PROMO => c.flags & F_PROMO != 0,
                    HAS_ARTSERIES => {
                        // Parity w6a: upstream has:artseries returns the
                        // art-series prints themselves (golden 2,352 = the
                        // default dump's own double-sided art rows; the is:
                        // term path validated that exact number behind the
                        // NAME_SLASH condition — art rows ARE hidden-flagged
                        // so the F2_HIDDEN guard may not apply to them).
                        // The old oracle_stats.has_artseries route never
                        // fired: art prints carry their OWN oracle_id.
                        let r = ix.card_oracle[i as usize] as usize;
                        if &*ix.strings[c.layout as usize] == "art_series" {
                            return c.flags2 & F2_NAME_SLASH != 0;
                        }
                        ix.side.art_or.get(r).copied().unwrap_or(0) != 0
                            && c.flags2 & F2_HIDDEN == 0
                    }
                    HAS_TEXTLESS => c.flags & F_TEXTLESS != 0,
                    HAS_FULLART => c.flags & F_FULLART != 0,
                    HAS_EXTART => {
                        c.ffx
                            & (1u32 << crate::index::FFX_LIST
                                .iter()
                                .position(|x| *x == "extendedart")
                                .unwrap())
                            != 0
                    }
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
                "chinese_simplified" | "simplified_chinese" | "zh_hans" | "zh" => "zhs",
                "chinese_traditional" | "traditional_chinese" | "zh_hant" => "zht",
                "any" | "all" => return ix.universe.clone(),
                s => s,
            };
            // Wave-5 item 3: prefer the full-bulk oracle side table
            // (default-cards is English-slimmed; upstream lang:ja = 30,853
            // cards vs 421 from the dump alone). en falls through to the
            // print-level index (side table deliberately skips en).
            if let Some(b) = ix.oracle_lang_idx.get(q) {
                return b.clone();
            }
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
            // Upstream resolves unknown block values against member-SET
            // names (cached theirs: b:legacy -> 621 = Urza block, matched
            // via set "Urza's Legacy"). OR every block that owns a set whose
            // name has the value as an alphanumeric word.
            let mut acc = RoaringBitmap::new();
            let mut found = false;
            for (setname, blk) in &ix.block_setname {
                let hit = setname.as_ref() == q.as_str()
                    || setname
                        .split(|c: char| !c.is_alphanumeric())
                        .any(|t| t == q.as_str());
                if hit {
                    if let Some(b) = ix.block_idx.get(&**blk) {
                        acc |= b;
                        found = true;
                    }
                }
            }
            if found {
                return acc;
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
                // upstream not:reprint is a PER-PRINT predicate: the
                // query's AND must land on the same printing (golden:
                // 'frame:2015 not:reprint' 19792 vs 25197 when the two
                // terms are evaluated per-oracle separately; 'not:reprint
                // game:paper' 33127 vs oracle-complement 15491). A card
                // matches when it has >=1 non-token/non-hidden/non-aux
                // printing WITHOUT the bulk reprint flag.
                scan(ix, |i| {
                    let c = &ix.cards[i as usize];
                    !c.reprint
                        && c.flags2 & (F2_TOKEN | F2_HIDDEN) == 0
                        && !crate::index::is_aux_layout(&ix.strings[c.layout as usize])
                        && !crate::index::is_aux_set_type(&ix.strings[c.set_type as usize])
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
            let sk = *kind;
            let side = ix.side.loaded;
            scan(ix, move |i| {
                // kind 5 = usd_etched: the slim dump has no such price slot
                // — presence resolves exclusively over the side index.
                let own = k < 5 && {
                    let p = ix.cards[i as usize].prices[k];
                    !p.is_nan() && p > 0.0
                };
                if own {
                    return true;
                }
                // Parity w6a: presence of a price on ANY printing counts for
                // usd_etched only (no own slot exists); other kinds keep the
                // w5 own-row semantics behind SCRYFALL_SIDE_PRICES=1 (side
                // rescue inflated usd: to 32922 vs upstream 21259).
                (sk == 5 || prints::side_prices_enabled())
                    && side_price_any(ix, i, sk, |y| y > 0.0)
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
        NumField::PriceEur => {
            // parity w5 §7 (borrow-plan): cardmarket unified listings — a
            // print with no non-foil eur price is priced by its eur_foil,
            // mirroring the usd fallback above (golden eur>=5 4,689; our
            // own-value-only count was 3,478). tix has no *_foil field in
            // the bulk dump, so PriceTix keeps own-value semantics.
            let e = c.prices[3];
            if e.is_nan() {
                c.prices[4]
            } else {
                e
            }
        }
        NumField::PriceEurFoil => c.prices[4],
        // Parity w6a: the slim dump has no etched price; usd_etched compares
        // resolve exclusively via the all_cards side index (see price_kind).
        NumField::PriceUsdEtched => f32::NAN,
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

/// parity w5 (borrow-plan §6): does any packed face value satisfy the
/// comparison? Codes: 0 absent, 1..=254 value+1, 255 = '*'. The wildcard
/// satisfies every INEQUALITY (golden pow<=3 14,450 vs 14,431 etc.) but never
/// plain equality — pow=3 must not swallow the ~200 '*' creatures.
fn face_pack_any(pack: u64, op: Cmp, v: f32) -> bool {
    let mut p = pack;
    for _ in 0..8 {
        let code = (p & 0xFF) as u8;
        p >>= 8;
        if code == 0 {
            continue;
        }
        if code == 255 {
            // '*' sorts above every number: it satisfies upper bounds
            // (pow<=3 golden 14450 vs 14431) but NEVER lower bounds —
            // battery-confirmed: pow>1 14,833 vs 14,515 when star matched.
            if matches!(op, Cmp::Lt | Cmp::Le) {
                return true;
            }
            continue;
        }
        if cmp_f((code - 1) as f32, op, v) {
            return true;
        }
    }
    false
}
fn num_any(top: f32, pack: u64, op: Cmp, v: f32) -> bool {
    if !top.is_nan() {
        if top == STAR {
            if matches!(op, Cmp::Lt | Cmp::Le) {
                return true;
            }
        } else if cmp_f(top, op, v) {
            return true;
        }
    }
    face_pack_any(pack, op, v)
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
        "full-art" | "full" => "fullart",
        "artseries" => "art_series",
        "playtest" => "emergency",
        "cmdr" => "commander",
        "story_spotlight" => "spotlight",
        "flyer" => "flying",
        "chase" => "planechase",
        "canopy" => "canopyland",
        "highres" => "hires",
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
        "hires" => Some(F_HIRES),
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
        // Parity w6a: is:/bare finishes terms mirror the has: arms — ∃ over
        // the oracle's ALL printings via the all_cards side index.
        "etched" => return scan(ix, |i| {
            let fx = if ix.side.loaded {
                ix.side.fin_or[ix.card_oracle[i as usize] as usize]
            } else {
                0
            };
            ix.cards[i as usize].finishes & FIN_ETCHED != 0 || fx & FIN_ETCHED != 0
        }),
        // finishes (docs: "Each card is available in non-foil, in foil, or in
        // both"; is:foil is:nonfoil = prints available in both).
        "foil" => return scan(ix, |i| {
            let fx = if ix.side.loaded {
                ix.side.fin_or[ix.card_oracle[i as usize] as usize]
            } else {
                0
            };
            ix.cards[i as usize].finishes & FIN_FOIL != 0 || fx & FIN_FOIL != 0
        }),
        "nonfoil" => return scan(ix, |i| ix.cards[i as usize].finishes & FIN_NONFOIL != 0),
        // is:glossy: upstream's glossy-finish (non-foil shiny) classifier has
        // no representation in the bulk dump (finishes ∈ foil/nonfoil/etched
        // only) — keeps the LOUD unknown-is warning below rather than a
        // silent/wrong match. (upstream is:glossy = 7 cards)
        // frame treatments (index.rs parse_line: atypical fit note). is:default
        // = has at least one non-atypical printing (print-level predicate;
        // upstream: is:default is:atypical == 0 per-print, both can hold at
        // oracle level).
        // parity w5 §9: is:atypical = the printing carries ANY frame_effects
        // (borrow report: ∃ any frame_effects → 10,341 vs 10,562 golden; the
        // old bulk `atypical` bool only flagged 8,300).
        "atypical" => return scan(ix, |i| ix.cards[i as usize].ffx != 0),
        "default" => return scan(ix, |i| !ix.cards[i as usize].atypical),
        "masterpiece" => return scan(ix, |i| ix.cards[i as usize].masterpiece),
        // Universes Beyond prints (bulk promo_types carries 'universesbeyond').
        "universesbeyond" | "universes_beyond" => {
            return scan(ix, |i| {
                ix.strings[ix.cards[i as usize].ptypes as usize]
                    .split(',')
                    .any(|x| x == "universesbeyond")
            });
        }
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
            // series (names carry "//") + battles (double-faced: creature
            // front / battle back). levelers/splits/flips/meld are NOT
            // dfc upstream (proven: is:dfc list diff).
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                let lay = &*ix.strings[c.layout as usize];
                matches!(
                    lay,
                    "transform" | "modal_dfc" | "double_faced_token" | "battle" | "prepare"
                ) || (lay == "art_series" && c.flags2 & F2_NAME_SLASH != 0)
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
            // 'Historic' type word on newer prints). Oracle-set diff vs live
            // Scryfall (2026-10-26, ours_cw/ishist fetches): upstream's 8190
            // CONTAINS every funny/acorn legendary-artifact (Un-sets, Under-
            // down, BG3/acorn promos) — our previous exclusion of "funny" |
            // "promo" removed 460 rows it should not (extras=0 => strict
            // subset). "alchemy" | "masters" | "memorabilia" printings are
            // genuinely absent from upstream's set (no such sets in the
            // missing diff) so those exclusions stay.
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                if c.flags2 & F2_HIDDEN != 0 {
                    return false;
                }
                let st = &*ix.strings[c.set_type as usize];
                if matches!(st, "memorabilia") {
                    // sets_catalog probe (2026-10-26): upstream's 8190 has
                    // ZERO memorabilia cards but DOES include every
                    // alchemy (172) and masters (cmm/2xm/me*/tsr…) printing,
                    // so only memorabilia is excluded.
                    return false;
                }
                // (hbg 'Agent of…' extras: stale dump rows — upstream now
                // 404s those exact cards; an all_parts/combo_piece engine
                // rule over-fired on 21k legit cards, so this is DATA, not
                // engine, drift.)
                // artifact-family tokens (Contraptions/Attractions printed
                // on token sheets) count as historic upstream — other token
                // cards stay excluded. The artifact must be on the MAIN
                // face, else DFC token sides (q07 Goblin // Artifact-Blood)
                // leak in as extras.
                if c.flags2 & F2_TOKEN != 0 {
                    let main = ix.type_lc(i).split("//").next().unwrap_or("").to_string();
                    if !main.contains("artifact") {
                        return false;
                    }
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
            // Background (main face legendary) or explicit 'can be your
            // commander' text; meld results and reversible cards excluded;
            // legality NOT part of the tag (Lurrus is:commander).
            // PLANESWALKERS are only commanders with the explicit CBC text —
            // live-probed: t:planeswalker is:commander = 45 upstream; famous
            // walkers (Jace Beleren, Nicol Bolas, Basri) are 404/0 there.
            // Digital-only (paper-less, e.g. hbg-style Alchemy) printings are
            // not commander-able — live-probed: is:commander is:alchemy = 128
            // upstream vs 225 ours pre-fix; Lukamina, Scorpion Form hidden.
            return scan(ix, |i| {
                let c = &ix.cards[i as usize];
                if c.flags2 & (F2_TOKEN | F2_HIDDEN) != 0 {
                    return false;
                }
                if c.flags & F_DIGITAL != 0 && c.games & G_PAPER == 0 {
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
                        || c.flags2 & F2_BG_ANY != 0)
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
        "datestamped" | "buyabox" | "boosterfun" | "fnm" | "judgegift" | "judge_gift"
        | "prerelease" | "inset" | "promopack" | "league" | "boxtopper" | "openrunner"
        | "bundle" | "planeswalkerdeck" | "planeswalker_deck" | "emergency" | "stamped"
        | "textlesspromo" | "gameday" | "giftbox" | "glossy" | "intropack" | "intro_pack"
        | "mediainsert" | "media_insert" | "setpromo" | "set_promo" | "arenaleague"
        | "arena_league" | "convention" | "playerrewards" | "player_rewards" | "release"
        | "instore" | "in_store" | "storechampionship" | "store_championship" => {
            // parity w5 §9: bulk promo_types values match upstream totals on
            // the visible pool — gameday 81/81, giftbox 16/16, glossy 7/7,
            // intropack 40/40, mediainsert 595/595 EXACT (probe-verified).
            // Not in EXTRAS_IS_WORDS: these dedup/gate like normal search.
            let q = t.trim_end_matches('s').to_string();
            let q2 = t.to_string();
            let q3 = t.replace('_', "");
            return ix
                .ptype_idx
                .get(q.as_str())
                .or_else(|| ix.ptype_idx.get(q2.as_str()))
                .or_else(|| ix.ptype_idx.get(q3.as_str()))
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
        | "canopyland" | "tricycleland" | "shadowland"
        // w6b: archetype land groups harvested from cached theirs page-1 lists
        // (bondland aliases crowdland/bbdland/battlebondland, tangoland alias
        // battleland are duplicated keys in land_cycles.json).
        | "bondland" | "crowdland" | "bbdland" | "battlebondland" | "creatureland"
        | "filterland" | "gainland" | "slowland" | "storageland" | "tangoland"
        | "battleland" | "canland" => {
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

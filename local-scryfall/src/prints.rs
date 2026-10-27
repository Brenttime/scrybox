//! Parity w6a: all_cards per-printing side index.
//!
//! The main index is built from the default_cards bulk (~119k printings).
//! Variant prints (`variation:true`), promo finishes, etched/foil price rows,
//! foreign-language prints and rarity:timeshifted special drops exist ONLY in
//! the all_cards bulk (~550k rows, every card in every language). This module
//! loads that bulk as a compact side index: one [`PrintRec`] per printing,
//! bucketed under the MAIN index's dense oracle rows (printings whose oracle
//! id is absent from the default-cards oracle table are dropped — the
//! visibility gate on RESULTS stays default-cards: a query can only ever
//! return cards that are in the visible pool; the side records let
//! price/cn/artist/rarity/finish predicates quantify over the oracle's FULL
//! printing universe).
//!
//! Memory: PrintRec is a 48-byte POD (f32 prices, interned string offsets);
//! ~550k records ≈ 27 MB + interned strings — negligible against the 597 MB
//! baseline index. Because gz-parsing all_cards costs ~2 minutes of startup,
//! the built table is also cached as a raw binary (`*.sideidx`) keyed to the
//! source size+mtime and reloaded in milliseconds.

use crate::index::{Index, FFX_LIST};
use flate2::read::GzDecoder;
use rayon::prelude::*;
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::time::Instant;

/// Side price field order (DIFFERENT from Card::prices which lacks usd_etched
/// and orders tix before eur): 0 usd, 1 usd_foil, 2 usd_etched, 3 eur,
/// 4 eur_foil, 5 tix.
pub const SP_USD: usize = 0;
pub const SP_USD_FOIL: usize = 1;
pub const SP_USD_ETCHED: usize = 2;
pub const SP_EUR: usize = 3;
pub const SP_EUR_FOIL: usize = 4;
pub const SP_TIX: usize = 5;

/// PrintRec.flags bits.
pub const SR_PROMO: u8 = 1;
/// "variation":true — variant sheets are excluded from upstream's default
/// search pool (they do not feed cn:/quantifiers there).
pub const SR_VARIATION: u8 = 2;
/// layout == "token"
pub const SR_TOKEN: u8 = 4;
/// lang != "en" — foreign printings stay out of the default search universe.
pub const SR_NONEN: u8 = 8;
/// Parity w7 (borrow A1): preview:true on this printing. Upstream hides
/// printings whose release date is still in the future AND carry the preview
/// flag from date:/year: quantification (and the pool gate is silent about
/// them for everything else).
pub const SR_PREV: u8 = 16;
/// Parity w7 (borrow A5): printing's set code is in the default-pool hidden
/// set list (index::is_hidden_set_code). Side prints matching a cn: numeric
/// range only count when they pass the same hidden set/set_type gate the
/// default-cards leg applies (w7 replay cn>100 23,208 vs 23,492).
pub const SR_HIDSET: u8 = 32;
/// Parity w7 (borrow A5): set_type ∈ OTAG_HIDDEN_SET_TYPES ∪ ID_EXTRA_*.
pub const SR_HIDST: u8 = 64;
/// Parity w7 (borrow A4): layout == "modal_dfc" on this printing. Feeds the
/// print-count mode of LAYOUT-level negatives (-is:mdfc): upstream answers
/// with DISTINCT ORACLES over the UNGATED allprints table (38,951 vs 38,899
/// golden) — oracles having zero modal_dfc printings anywhere.
pub const SR_MDFC: u8 = 128;

/// w6a calibration finding: the side-index price FALLBACK for usd/eur/foil
/// comparisons systematically overshoots upstream. Upstream prices the
/// DISPLAYED printing — which is exactly the default-cards row the slim dump
/// already carries — so the w5 own-row model sat within 1% (CLOSE) on every
/// battery price row, while ∃-over-all-printings added 1–7% false matches
/// (usd>1 11287→11258 vs 11173; frame:2015 usd>1 +526; tix>1 3207 vs 260 —
/// MTGO prints share oracles with paper ones). Per the wave brief ("family
/// regresses >2 rows and unfixed in 2 builds → gate behind a flag off +
/// document"), the fallback stays behind SCRYFALL_SIDE_PRICES=1.
/// usd_etched (kind 5) is exempt: the slim dump has no such slot, so it can
/// ONLY resolve through the side index (no baseline to regress).
pub fn side_prices_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SCRYFALL_SIDE_PRICES").is_ok_and(|v| v == "1"))
}

/// One compact record per printing from the all_cards bulk, including
/// gated/hidden/variant/foreign prints. All offsets index SideIdx tables.
#[derive(Clone, Copy, Default)]
pub struct PrintRec {
    /// usd, usd_foil, usd_etched, eur, eur_foil, tix (NAN = absent).
    pub prices: [f32; 6],
    /// raw collector_number string offset (SideIdx::strings).
    pub cn: u32,
    /// numeric prefix (u32::MAX when alpha-only), same parse as Card::cn_val.
    pub cn_val: u32,
    /// lowercased artist string offset (SideIdx::strings).
    pub artist: u32,
    /// frame_effects bitset over index::FFX_LIST.
    pub ffx: u32,
    /// index into SideIdx::langs.
    pub lang: u8,
    /// FIN_NONFOIL|FIN_FOIL|FIN_ETCHED bitset (index.rs constants).
    pub finishes: u8,
    /// rarity rank (index::rarity_rank; RARITY_OTHER = 7).
    pub rarity: u8,
    /// border code (index::BD_*).
    pub border: u8,
    /// SR_* flags (promo, variation, token, nonen, prev, hidset, hidst, mdfc).
    pub flags: u8,
    /// Parity w7 (borrow A1): released_at as YYYYMMDD, 0 = absent. Feeds the
    /// ∃-over-prints date:/year: model (pool-gated, preview&&future rows
    /// excluded — see eval's NumField::Date/Year arm).
    pub released: u32,
}

/// The side index, hanging off Index::side. `start`/`prints` give the
/// printings of oracle row `r` as `prints[start[r]..start[r+1]]`; the
/// `*_or` vectors are per-oracle OR-reductions for the cheap existential
/// predicates (has:foil/etched, rarity:timeshifted).
#[derive(Default)]
pub struct SideIdx {
    pub loaded: bool,
    pub prints: Vec<PrintRec>,
    pub start: Vec<u32>,
    pub strings: Vec<Box<str>>,
    pub langs: Vec<Box<str>>,
    /// OR of PrintRec::finishes across the oracle's printings.
    pub fin_or: Vec<u8>,
    /// bit r set ⟺ the oracle has a printing of rarity r (r < 8).
    pub rar_or: Vec<u8>,
    /// 1 ⟺ the oracle has a promo printing.
    pub promo_or: Vec<u8>,
    /// 1 ⟺ the oracle has an art_series printing (has:artseries — those
    /// rows are default-cards-omitted; golden upstream 2,352).
    pub art_or: Vec<u8>,
    /// Parity w7 (borrow A4): 1 ⟺ the oracle has a modal_dfc printing (any
    /// printing in the ungated allprints table — incl. token/hidden/foreign
    /// rows). Drives -is:mdfc print-count mode.
    pub mdfc_or: Vec<u8>,
}

#[inline]
/// Side printings of the oracle that card `card` belongs to (empty when the
/// side index is not loaded — every repointed predicate degrades exactly to
/// its baseline behaviour then).
pub fn recs<'a>(ix: &'a Index, card: u32) -> &'a [PrintRec] {
    let s = &ix.side;
    if !s.loaded {
        return &[];
    }
    let r = ix.card_oracle[card as usize] as usize;
    if r + 1 >= s.start.len() {
        return &[];
    }
    &s.prints[s.start[r] as usize..s.start[r + 1] as usize]
}

/// Effective price of a side print for comparison field `kind` (0 usd,
/// 1 usd_foil, 2 tix, 3 eur, 4 eur_foil — the NumField/PriceExists kind
/// order). Mirrors the Card-side fallbacks in eval::num_field: usd falls
/// back to usd_foil then usd_etched, eur to eur_foil; NAN never matches.
#[inline]
pub fn eff_price(p: &PrintRec, kind: u8) -> f32 {
    match kind {
        0 => {
            let a = p.prices[SP_USD];
            if !a.is_nan() {
                return a;
            }
            let b = p.prices[SP_USD_FOIL];
            if !b.is_nan() {
                return b;
            }
            p.prices[SP_USD_ETCHED]
        }
        1 => p.prices[SP_USD_FOIL],
        2 => p.prices[SP_TIX],
        3 => {
            let a = p.prices[SP_EUR];
            if !a.is_nan() {
                return a;
            }
            p.prices[SP_EUR_FOIL]
        }
        4 => p.prices[SP_EUR_FOIL],
        5 => p.prices[SP_USD_ETCHED],
        _ => f32::NAN,
    }
}

// ---------------------------------------------------------------- parsing --

struct RawSide {
    row: u32,
    prices: [f32; 6],
    cn: String,
    artist: String,
    lang: String,
    ffx: u32,
    finishes: u8,
    rarity: u8,
    border: u8,
    flags: u8,
    released: u32,
    art_series: bool,
}

/// "2026-01-01" -> 20260101 (0 on absent/malformed) — w7 side released_at.
fn parse_ymd(s: &str) -> u32 {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return 0;
    }
    let d = |i: usize| -> Option<u32> {
        if b[i].is_ascii_digit() && b[i + 1].is_ascii_digit() {
            Some(((b[i] - b'0') as u32) * 10 + (b[i + 1] - b'0') as u32)
        } else {
            None
        }
    };
    match (d(0), d(2), d(5), d(8)) {
        (Some(y0), Some(y1), Some(m), Some(dd)) => Some((y0 * 100 + y1) * 10000 + m * 100 + dd),
        _ => None,
    }
    .unwrap_or(0)
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn parse_side_line(line: &str, intern: &HashMap<Box<str>, u32>) -> Option<RawSide> {
    let v: Value = serde_json::from_str(line).ok()?;
    let oid = v.get("oracle_id").and_then(|x| x.as_str()).unwrap_or("");
    // Visibility gate on RESULTS stays default-cards: printings whose oracle
    // never appears in the visible pool can't surface, drop them early.
    let row = *intern.get(oid)?;

    let mut prices = [f32::NAN; 6];
    if let Some(pr) = v.get("prices") {
        for (i, k) in ["usd", "usd_foil", "usd_etched", "eur", "eur_foil", "tix"]
            .iter()
            .enumerate()
        {
            match pr.get(*k) {
                Some(Value::String(s)) => {
                    if let Ok(x) = s.parse::<f32>() {
                        prices[i] = x;
                    }
                }
                Some(Value::Number(n)) => {
                    if let Some(x) = n.as_f64() {
                        prices[i] = x as f32;
                    }
                }
                _ => {}
            }
        }
    }

    let mut finishes = 0u8;
    if v.get("nonfoil").and_then(|x| x.as_bool()).unwrap_or(false) {
        finishes |= crate::index::FIN_NONFOIL;
    }
    if v.get("foil").and_then(|x| x.as_bool()).unwrap_or(false) {
        finishes |= crate::index::FIN_FOIL;
    }
    if let Some(a) = v.get("finishes").and_then(|x| x.as_array()) {
        for x in a {
            match x.as_str().unwrap_or("") {
                "etched" => finishes |= crate::index::FIN_ETCHED,
                "foil" => finishes |= crate::index::FIN_FOIL,
                "nonfoil" => finishes |= crate::index::FIN_NONFOIL,
                _ => {}
            }
        }
    }

    let mut ffx = 0u32;
    if let Some(a) = v.get("frame_effects").and_then(|x| x.as_array()) {
        for x in a {
            if let Some(t) = x.as_str() {
                if let Some(p) = FFX_LIST.iter().position(|e| *e == t) {
                    ffx |= 1u32 << p;
                }
            }
        }
    }

    let border = match str_of(&v, "border_color") {
        "white" => crate::index::BD_WHITE,
        "silver" => crate::index::BD_SILVER,
        "gold" => crate::index::BD_GOLD,
        "borderless" => crate::index::BD_BORDERLESS,
        "yellow" => crate::index::BD_YELLOW,
        _ => crate::index::BD_BLACK,
    };

    let mut flags = 0u8;
    if v.get("promo").and_then(|x| x.as_bool()).unwrap_or(false) {
        flags |= SR_PROMO;
    }
    if v.get("variation").and_then(|x| x.as_bool()).unwrap_or(false) {
        flags |= SR_VARIATION;
    }
    let layout = str_of(&v, "layout");
    if layout == "token" {
        flags |= SR_TOKEN;
    }
    // w7 (borrow A1): bulk 'preview' is an object on previewed prints —
    // see index::json_is_preview (as_bool alone missed every one).
    if crate::index::json_is_preview(v.get("preview")) {
        flags |= SR_PREV;
    }
    // w7 (borrow A5): hidden set / hidden set_type on this printing — the
    // same row gate the default-cards leg applies (index lists).
    if crate::index::is_pool_hidden_set(str_of(&v, "set")) {
        flags |= SR_HIDSET;
    }
    if crate::index::is_hidden_set_type(str_of(&v, "set_type")) {
        flags |= SR_HIDST;
    }
    // w7 (borrow A4): modal_dfc printings feed the -is:mdfc oracle reduction.
    if layout == "modal_dfc" {
        flags |= SR_MDFC;
    }

    Some(RawSide {
        row,
        prices,
        cn: str_of(&v, "collector_number").to_string(),
        // stored accent-folded + lowercased: upstream artist: matching is
        // accent-insensitive and the side artist strings exist ONLY for that
        // comparison (see eval::fold_artist, Volkan Baǵa golden).
        artist: crate::eval::fold_artist(&str_of(&v, "artist").to_lowercase()),
        lang: {
            let l = str_of(&v, "lang").to_lowercase();
            if l.is_empty() {
                "en".to_string()
            } else {
                if l != "en" {
                    flags |= SR_NONEN;
                }
                l
            }
        },
        ffx,
        finishes,
        rarity: crate::index::rarity_rank(str_of(&v, "rarity")),
        border,
        flags,
        released: parse_ymd(str_of(&v, "released_at")),
        art_series: layout == "art_series",
    })
}

// -------------------------------------------------------------- the build --

/// Load all_cards.jsonl.gz into `ix.side`. Returns true on success.
pub fn load_side_gz(ix: &mut Index, src: &Path) -> bool {
    let t0 = Instant::now();
    let file = match File::open(src) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("side index: cannot open {}: {}", src.display(), e);
            return false;
        }
    };
    let dec = GzDecoder::new(file);
    let mut reader = BufReader::with_capacity(1 << 20, dec);
    let intern = &ix.oracle_intern;

    // 1. chunked parallel parse (same shape as index::load)
    let mut raws: Vec<RawSide> = Vec::with_capacity(600_000);
    loop {
        let mut chunk: Vec<String> = Vec::with_capacity(4096);
        let mut eof = false;
        for _ in 0..4096 {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(_) => {
                    if !line.trim_end().is_empty() {
                        chunk.push(line);
                    }
                }
                Err(e) => {
                    eprintln!("side index: read error: {e}");
                    return false;
                }
            }
        }
        let parsed: Vec<RawSide> =
            chunk.par_iter().filter_map(|l| parse_side_line(l, intern)).collect();
        raws.extend(parsed);
        if eof {
            break;
        }
    }
    let parse_ms = t0.elapsed().as_millis();

    // 2. intern cn/artist/lang strings (parallel keys, serial intern)
    let mut interned: HashMap<&str, u32> = HashMap::with_capacity(200_000);
    let mut strings: Vec<Box<str>> = Vec::with_capacity(200_000);
    fn intern_with<'a>(
        s: &'a str,
        interned: &mut HashMap<&'a str, u32>,
        strings: &mut Vec<Box<str>>,
    ) -> u32 {
        if let Some(&o) = interned.get(s) {
            return o;
        }
        let o = strings.len() as u32;
        strings.push(s.into());
        interned.insert(s, o);
        o
    }

    let n_or = ix.oracle_id_off.len();
    // counting sort by oracle row → contiguous per-oracle ranges
    let mut cnt = vec![0u32; n_or];
    for r in &raws {
        cnt[r.row as usize] += 1;
    }
    let mut start = vec![0u32; n_or + 1];
    for r in 0..n_or {
        start[r + 1] = start[r] + cnt[r];
    }
    let mut pos = start[..n_or].to_vec();
    let mut prints = vec![PrintRec::default(); raws.len()];
    let mut fin_or = vec![0u8; n_or];
    let mut rar_or = vec![0u8; n_or];
    let mut promo_or = vec![0u8; n_or];
    let mut art_or = vec![0u8; n_or];
    let mut mdfc_or = vec![0u8; n_or];
    let mut langs: Vec<Box<str>> = Vec::new();
    let mut lang_int: HashMap<&str, u8> = HashMap::new();
    for (i, r) in raws.iter().enumerate() {
        // raws order matters for stable cn/artist intern; keep index-stable
        let cn = intern_with(&r.cn, &mut interned, &mut strings);
        let artist = intern_with(&r.artist, &mut interned, &mut strings);
        let lg = match lang_int.get(r.lang.as_str()) {
            Some(&x) => x,
            None => {
                let x = langs.len() as u8;
                if x < 255 {
                    langs.push(r.lang.as_str().into());
                    lang_int.insert(r.lang.as_str(), x);
                }
                x.min(255)
            }
        };
        let p = &mut pos[r.row as usize];
        prints[*p as usize] = PrintRec {
            prices: r.prices,
            cn,
            cn_val: crate::index::parse_cn(&r.cn),
            artist,
            ffx: r.ffx,
            lang: lg,
            finishes: r.finishes,
            rarity: r.rarity,
            border: r.border,
            flags: r.flags,
            released: r.released,
        };
        *p += 1;
        fin_or[r.row as usize] |= r.finishes;
        if r.rarity < 8 {
            rar_or[r.row as usize] |= 1u8 << r.rarity;
        }
        if r.flags & SR_PROMO != 0 {
            promo_or[r.row as usize] = 1;
        }
        if r.art_series {
            art_or[r.row as usize] = 1;
        }
        if r.flags & SR_MDFC != 0 {
            mdfc_or[r.row as usize] = 1;
        }
        let _ = i;
    }
    drop(interned);
    drop(raws);

    ix.side = SideIdx {
        loaded: true,
        prints,
        start,
        strings,
        langs,
        fin_or,
        rar_or,
        promo_or,
        art_or,
        mdfc_or,
    };
    eprintln!(
        "side index: {} printings over {} oracles in {} ms ({} strings)",
        ix.side.prints.len(),
        n_or,
        t0.elapsed().as_millis(),
        ix.side.strings.len(),
    );
    let _ = parse_ms;
    true
}

// ----------------------------------------------------------------- cache --
// Raw little-endian dump of the built table; keyed to the gz size+mtime so a
// refreshed bulk automatically invalidates it.

const CACHE_MAGIC: &[u8; 8] = b"SPSIDE05";

fn put_u32(b: &mut Vec<u8>, x: u32) {
    b.extend_from_slice(&x.to_le_bytes());
}
fn put_str(b: &mut Vec<u8>, s: &str) {
    put_u32(b, s.len() as u32);
    b.extend_from_slice(s.as_bytes());
}
fn get_u32(b: &[u8], p: &mut usize) -> u32 {
    let x = u32::from_le_bytes(b[*p..*p + 4].try_into().unwrap());
    *p += 4;
    x
}
fn get_str(b: &[u8], p: &mut usize) -> String {
    let n = get_u32(b, p) as usize;
    let s = String::from_utf8_lossy(&b[*p..*p + n]).into_owned();
    *p += n;
    s
}

pub fn write_side_cache(ix: &Index, cache: &Path, src: &Path) -> bool {
    let md = match src.metadata() {
        Ok(m) => m,
        Err(_) => return false,
    };
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut b: Vec<u8> = Vec::with_capacity(ix.side.prints.len() * 56 + 1024);
    b.extend_from_slice(CACHE_MAGIC);
    b.extend_from_slice(&md.len().to_le_bytes());
    b.extend_from_slice(&mtime.to_le_bytes());
    put_u32(&mut b, ix.side.strings.len() as u32);
    for s in &ix.side.strings {
        put_str(&mut b, s);
    }
    put_u32(&mut b, ix.side.langs.len() as u32);
    for s in &ix.side.langs {
        put_str(&mut b, s);
    }
    put_u32(&mut b, ix.side.start.len() as u32);
    for x in &ix.side.start {
        put_u32(&mut b, *x);
    }
    put_u32(&mut b, ix.side.prints.len() as u32);
    for p in &ix.side.prints {
        for f in &p.prices {
            b.extend_from_slice(&f.to_le_bytes());
        }
        put_u32(&mut b, p.cn);
        put_u32(&mut b, p.cn_val);
        put_u32(&mut b, p.artist);
        put_u32(&mut b, p.ffx);
        b.push(p.lang);
        b.push(p.finishes);
        b.push(p.rarity);
        b.push(p.border);
        b.push(p.flags);
        put_u32(&mut b, p.released);
    }
    for (v, name) in [(&ix.side.fin_or, "fin"), (&ix.side.rar_or, "rar"), (&ix.side.promo_or, "promo"), (&ix.side.art_or, "art"), (&ix.side.mdfc_or, "mdfc")] {
        put_u32(&mut b, v.len() as u32);
        b.extend_from_slice(v);
        let _ = name;
    }
    let tmp = cache.with_extension("tmp");
    if File::create(&tmp).and_then(|mut f| f.write_all(&b)).is_err() {
        return false;
    }
    std::fs::rename(&tmp, cache).is_ok()
}

pub fn load_side_cache(ix: &mut Index, cache: &Path, src: &Path) -> bool {
    let md = match src.metadata() {
        Ok(m) => m,
        Err(_) => return false,
    };
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut f = match File::open(cache) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut b = Vec::new();
    if f.read_to_end(&mut b).is_err() || b.len() < 24 || &b[..8] != CACHE_MAGIC {
        return false;
    }
    if u64::from_le_bytes(b[8..16].try_into().unwrap()) != md.len()
        || u64::from_le_bytes(b[16..24].try_into().unwrap()) != mtime
    {
        eprintln!("side index: cache stale for {}", src.display());
        return false;
    }
    let t0 = Instant::now();
    let mut p = 24usize;
    let n = get_u32(&b, &mut p) as usize;
    let mut strings: Vec<Box<str>> = Vec::with_capacity(n);
    for _ in 0..n {
        strings.push(get_str(&b, &mut p).into());
    }
    let n = get_u32(&b, &mut p) as usize;
    let mut langs: Vec<Box<str>> = Vec::with_capacity(n);
    for _ in 0..n {
        langs.push(get_str(&b, &mut p).into());
    }
    let n = get_u32(&b, &mut p) as usize;
    let mut start: Vec<u32> = Vec::with_capacity(n);
    for _ in 0..n {
        start.push(get_u32(&b, &mut p));
    }
    let n = get_u32(&b, &mut p) as usize;
    let mut prints: Vec<PrintRec> = Vec::with_capacity(n);
    for _ in 0..n {
        let mut prices = [f32::NAN; 6];
        for slot in prices.iter_mut() {
            *slot = f32::from_le_bytes(b[p..p + 4].try_into().unwrap());
            p += 4;
        }
        let cn = get_u32(&b, &mut p);
        let cn_val = get_u32(&b, &mut p);
        let artist = get_u32(&b, &mut p);
        let ffx = get_u32(&b, &mut p);
        let (lang, finishes, rarity, border, flags) =
            (b[p], b[p + 1], b[p + 2], b[p + 3], b[p + 4]);
        p += 5;
        let released = get_u32(&b, &mut p);
        prints.push(PrintRec { prices, cn, cn_val, artist, ffx, lang, finishes, rarity, border, flags, released });
    }
    let vec_u8 = |p: &mut usize| -> Vec<u8> {
        let n = get_u32(&b, p) as usize;
        let v = b[*p..*p + n].to_vec();
        *p += n;
        v
    };
    let fin_or = vec_u8(&mut p);
    let rar_or = vec_u8(&mut p);
    let promo_or = vec_u8(&mut p);
    let art_or = vec_u8(&mut p);
    let mdfc_or = vec_u8(&mut p);
    ix.side = SideIdx { loaded: true, prints, start, strings, langs, fin_or, rar_or, promo_or, art_or, mdfc_or };
    eprintln!(
        "side index (cache): {} printings loaded in {} ms",
        ix.side.prints.len(),
        t0.elapsed().as_millis()
    );
    true
}

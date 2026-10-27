//! Memory-resident card store + inverted indexes. No SQLite, no disk at query time.

use flate2::read::GzDecoder;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::Instant;

/// Formats tracked in the packed `legalities` field (2 bits each, u64 total).
/// Full Scryfall format set (docs list), so f:/banned:/restricted: mirror upstream.
pub const FORMATS: [&str; 24] = [
    "standard",
    "future",
    "historic",
    "timeless",
    "gladiator",
    "pioneer",
    "modern",
    "legacy",
    "pauper",
    "vintage",
    "penny",
    "commander",
    "oathbreaker",
    "standardbrawl",
    "brawl",
    "competitivebrawl",
    "alchemy",
    "paupercommander",
    "duel",
    "oldschool",
    "premodern",
    "predh",
    "tlr",
    "explorer",
];
pub const NFORMATS: usize = 24;

// status codes stored in 2 bits
pub const L_NOT_LEGAL: u32 = 0;
pub const L_LEGAL: u32 = 1;
pub const L_BANNED: u32 = 2;
pub const L_RESTRICTED: u32 = 3;

// Derived boolean flags (Card.flags) for `is:` filters.
pub const F_LAND: u32 = 1 << 0;
pub const F_SPELL: u32 = 1 << 1; // nonland
pub const F_PERMANENT: u32 = 1 << 2;
pub const F_CREATURE: u32 = 1 << 3;
pub const F_LEGENDARY: u32 = 1 << 4;
pub const F_BASIC: u32 = 1 << 5;
pub const F_SNOW: u32 = 1 << 6;
pub const F_MULTICOLORED: u32 = 1 << 7;
pub const F_VANILLA: u32 = 1 << 8;
pub const F_COLORLESS: u32 = 1 << 9;
pub const F_DFC: u32 = 1 << 10;
pub const F_FULLART: u32 = 1 << 11;
pub const F_TEXTLESS: u32 = 1 << 12;
pub const F_DIGITAL: u32 = 1 << 13;
pub const F_PROMO: u32 = 1 << 14;
pub const F_OVERSIZED: u32 = 1 << 15;
pub const F_ARTIFACT: u32 = 1 << 16;
pub const F_ENCHANTMENT: u32 = 1 << 17;
pub const F_PLANESWALKER: u32 = 1 << 18;
pub const F_BATTLE: u32 = 1 << 19;
pub const F_INSTANT: u32 = 1 << 20;
pub const F_SORCERY: u32 = 1 << 21;
pub const F_RESERVED: u32 = 1 << 22;
pub const F_GAMECHANGER: u32 = 1 << 23;
pub const F_SPOTLIGHT: u32 = 1 << 24;
pub const F_BOOSTER_FLAG: u32 = 1 << 25;
pub const F_HIRES: u32 = 1 << 26;
pub const F_HAS_WM: u32 = 1 << 27;
pub const F_HAS_IND: u32 = 1 << 28;
pub const F_HAS_FACES: u32 = 1 << 29;
pub const F_PREVIEW: u32 = 1 << 30;
pub const F_HAS_TEXT: u32 = 1 << 31;

// Second flag word (u64) — printings' derived predicates, calibrated against
// live Scryfall semantics (unique:cards diffs, Oct 2026). All computed once at
// load time from the raw JSON so eval stays a bitmap/scan over packed bits.
pub const F2_TOKEN: u64 = 1 << 0; // token-like printing (token set/layout or 'Token' in type)
pub const F2_CRE_MAIN: u64 = 1 << 1; // main face type contains 'creature'
pub const F2_VEH_MAIN: u64 = 1 << 2; // main face type contains vehicle/spacecraft
pub const F2_LEG_MAIN: u64 = 1 << 3; // main face type contains 'legendary'
pub const F2_PW_MAIN: u64 = 1 << 4;
pub const F2_LAND_MAIN: u64 = 1 << 5;
pub const F2_PW_ANY: u64 = 1 << 6; // any face type contains 'planeswalker'
pub const F2_BG_ANY: u64 = 1 << 7; // any face type contains 'background'
pub const F2_HIST_WORD: u64 = 1 << 8; // any face type contains 'historic'
pub const F2_CLASS_PARTY: u64 = 1 << 9; // cleric/warrior/rogue/wizard on any face
pub const F2_CLASS_OUTLAW: u64 = 1 << 10; // assassin/pirate/rogue/warlock/bard/mercenary/trader
pub const F2_ALLCT: u64 = 1 << 11; // text says 'all/every creature type(s)' (changeling)
pub const F2_MELDTEXT: u64 = 1 << 12; // text contains 'meld'
pub const F2_CBC: u64 = 1 << 13; // text contains 'can be your commander'
pub const F2_BEAR: u64 = 1 << 14; // some face (or top level) is a 2/2 with mana value 2
pub const F2_VANILLA: u64 = 1 << 15; // main face creature, not PW, main face has no text
pub const F2_MAIN_TEXT: u64 = 1 << 16; // main face text non-empty
pub const F2_NAME_SLASH: u64 = 1 << 17; // name contains "//"
pub const F2_HIDDEN: u64 = 1 << 18; // printing's set is not in Scryfall's default search
pub const F2_COMPANION: u64 = 1 << 19; // 'Companion' keyword on an expansion/core printing
pub const F2_ANGEL: u64 = 1 << 20; // 'angel' on any face type (game:/t: combos)
pub const F2_FRENCH: u64 = 1 << 21; // main text is keyword-abilities-only
/// Parity w7 (borrow A1): preview object with source mentioning Scryfall —
/// is:scryfallpreview additionally requires released_at > today (a preview
/// stops being one at release).
pub const F2_SC_PREVIEW: u64 = 1 << 22;

/// True when oracle text (reminder parens already absent or removed) consists
/// only of keyword abilities — the is:frenchvanilla test (calibrated against
/// upstream's 1094-card list: sentence openers are keywords, ward/cube/prowess
/// triggers, "This creature has/gains <keyword>", protection, cycling).
pub fn keyword_only_text(s: &str) -> bool {
    let s = strip_paren_text(s);
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    for seg in s.split(|c| c == '.' || c == '\n' || c == '•') {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        let low = seg.to_lowercase();
        let low = low.trim_start_matches(|c: char| c.is_whitespace());
        // activation costs "{W}: " prefix a keyword ability (Trample etc.)
        let low = low.trim_start_matches(|c: char| c == '{' || c == '}' || c == ' ' || (c.is_ascii_digit()));
        let low = low.trim_start_matches(':').trim();
        // "{1}{W}: " style needs a second pass after symbols
        let low = low.trim_start_matches(|c: char| c == '{' || c == '}' || c == ' ' || (c.is_ascii_alphabetic() && "wubrg".contains(c)));
        let low = low.trim_start_matches(':').trim();
        let ok = [
            "flying", "first strike", "double strike", "deathtouch", "haste", "hexproof",
            "indestructible", "lifelink", "menace", "reach", "trample", "vigilance",
            "shadow", "fear", "intimidate", "banding", "flash", "defender", "ward",
            "protection", "this creature has", "this creature gains", "this card has",
            "this card gains", "this permanent has", "this permanent gains",
            "cycling", "landwalk", "horsemanship", "buyback", "kicker", "split second",
            "trample, ", "cumulant upkeep", "cubes", "prowess", "endure", "extort",
            "as this creature enters", "when this creature enters",
        ]
        .iter()
        .any(|k| low.starts_with(k));
        if !ok {
            return false;
        }
    }
    true
}

/// Sets whose prints are absent from Scryfall's default-search universe.
/// Proven by full is:unique/is:reprint list coverage (our oracles from these
/// sets appear in neither upstream list) plus 404 name probes; the s:<code>
/// totals upstream returns for unknown codes are fuzzy fallbacks, not proof
/// of visibility. mb2/fra/dsk/hbg residuals are stale-dump drift, not hiding.
pub fn is_hidden_set_code(code: &str) -> bool {
    matches!(code, "unk" | "hho" | "ph18" | "ph19" | "ph21" | "ph22" | "ph23")
}

/// Parity w7 (borrow A5): the FULL hidden-set-code calibration (OTAG list,
/// ~196 codes) used by the cn: numeric row gate — distinct from
/// is_hidden_set_code, the tiny is:unique blocklist above. Mirrors the w7
/// replay's `set in OTAG_HIDDEN_SETS` gate (replay cn>100 23,208 vs 23,492).
pub fn is_pool_hidden_set(code: &str) -> bool {
    OTAG_HIDDEN_SETS.contains(&code)
}

/// Parity w7 (borrow A1): preview predicate shared by the main index and the
/// side prints table. The Scryfall bulk encodes a previewed printing as
/// `"preview": {source, source_uri, previewed_at}` — an object — while
/// unreleased-and-unpreviewed prints carry preview:false or the key is
/// absent. Truthy = the key is present, non-null, and not boolean false.
pub fn json_is_preview(x: Option<&serde_json::Value>) -> bool {
    match x {
        None => false,
        Some(v) => {
            if let Some(b) = v.as_bool() {
                b
            } else if let Some(o) = v.as_object() {
                !o.is_empty()
            } else {
                !v.is_null()
            }
        }
    }
}

/// Parity w7 (borrow A5): set_type hidden from the default-search pool —
/// OTAG_HIDDEN_SET_TYPES ∪ ID_EXTRA_HIDDEN_SET_TYPES (the replay's IOT).
pub fn is_hidden_set_type(st: &str) -> bool {
    OTAG_HIDDEN_SET_TYPES.contains(&st) || ID_EXTRA_HIDDEN_SET_TYPES.contains(&st)
}

/// Layouts that never make an oracle 'visible' for is:unique/is:reprint
/// (extras & memorabilia surfaces; proven against upstream's full lists).
pub fn is_aux_layout(layout: &str) -> bool {
    matches!(
        layout,
        "art_series"
            | "front_card"
            | "album"
            | "vanguard"
            | "planar"
            | "scheme"
            | "emblem"
            | "token"
            | "double_faced_token"
    )
}

/// set_types that never make an oracle 'visible' for is:unique/is:reprint.
pub fn is_aux_set_type(st: &str) -> bool {
    matches!(
        st,
        "memorabilia" | "promo" | "box" | "vanguard" | "archenemy" | "token" | "album"
    )
}


// price indices for Card::prices
pub const P_USD: usize = 0;
pub const P_USD_FOIL: usize = 1;
pub const P_TIX: usize = 2;
pub const P_EUR: usize = 3;
pub const P_EUR_FOIL: usize = 4;

// security_stamp enum (Card::stamp)
pub const ST_NONE: u8 = 0;
pub const ST_OVAL: u8 = 1;
pub const ST_TRIANGLE: u8 = 2;
pub const ST_ARENA: u8 = 3;
pub const ST_ACORN: u8 = 4;
pub const ST_CIRCLE: u8 = 5;
pub const ST_HEART: u8 = 6;
pub const STAMP_NAMES: [&str; 7] =
    ["", "oval", "triangle", "arena", "acorn", "circle", "heart"];

// border_color enum (Card::border)
pub const BD_BLACK: u8 = 0;
pub const BD_WHITE: u8 = 1;
pub const BD_SILVER: u8 = 2;
pub const BD_GOLD: u8 = 3;
pub const BD_BORDERLESS: u8 = 4;
pub const BD_YELLOW: u8 = 5;
pub const BORDER_NAMES: [&str; 6] =
    ["black", "white", "silver", "gold", "borderless", "yellow"];

// frame enum (Card::frame)
pub const FR_2015: u8 = 0;
pub const FR_2003: u8 = 1;
pub const FR_1997: u8 = 2;
pub const FR_1993: u8 = 3;
pub const FR_FUTURE: u8 = 4;
pub const FRAME_NAMES: [&str; 5] = ["2015", "2003", "1997", "1993", "future"];

// games bitmask (Card::games)
pub const G_PAPER: u8 = 1;
pub const G_MTGO: u8 = 2;
pub const G_ARENA: u8 = 4;

// finishes bitmask (Card::finishes)
pub const FIN_NONFOIL: u8 = 1;
pub const FIN_FOIL: u8 = 2;
pub const FIN_ETCHED: u8 = 4;

// produced_mana bitmask (Card::produced): W U B R G + colorless
pub const PR_C: u8 = 32;

// frame_effects bitset (Card::ffx) — interned over this list
pub const FFX_LIST: [&str; 27] = [
    "legendary", "inverted", "extendedart", "showcase", "enchantment", "etched", "sunmoondfc",
    "devoid", "tombstone", "snow", "fullart", "lesson", "companion", "colorshifted",
    "compasslanddfc", "miracle", "fandfc", "mooneldrazidfc", "spree", "draft", "originpwdfc",
    "convertdfc", "shatteredglass", "waxingandwaningmoondfc", "upsidedowndfc", "borderless",
    "textless",
];

// new_flags bits (Card::new_flags): first-in-oracle occurrences
pub const NEW_ART: u8 = 1;
pub const NEW_ARTIST: u8 = 2;
pub const NEW_FLAVOR: u8 = 4;
pub const NEW_FRAME: u8 = 8;
pub const NEW_RARITY: u8 = 16;
pub const NEW_LANGUAGE: u8 = 32;
pub const NEW_PREVIEW: u8 = 64;

/// Rarity ranks.
pub const RARITY_OTHER: u8 = 7;

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Card {
    // string-table offsets
    pub id: u32,
    pub oracle_id: u32,
    pub name: u32,
    pub set: u32,
    pub set_name: u32,
    pub set_type: u32,
    pub collector_number: u32,
    /// collector_number with trailing variant stars stripped; printings are
    /// counted per (set, cn_key) so foil/star twins share one print (proven
    /// against Scryfall: Embermage Goblin prints=1 despite cn "200" + "200★").
    pub cn_key: u32,
    pub type_line: u32,
    pub mana_cost: u32,
    pub oracle_text: u32,
    pub artist: u32,
    pub keywords: u32,
    pub layout: u32,
    pub image_small: u32,
    pub image_normal: u32,
    pub scryfall_uri: u32,
    pub cmc: f32,
    pub power: f32,    // NAN = absent / *
    pub toughness: f32, // NAN = absent / *
    pub loyalty: f32,  // NAN = absent
    pub colors: u8,    // bitmask W U B R G
    pub ci: u8,
    /// per-face colour masks, 5 bits/slot (slot0 = top/front), max 8 faces;
    /// drives any-face c: superset matching (see Index::color_incl).
    pub face_pack: u64,
    pub rarity: u8,
    /// Dense enum code for the card's set_type (see `set_type_code()`), for the
    /// query layer. Groundwork for Scryfall's default search hiding "extras"
    /// sets (token/asset/stock/etc). The readable string stays at `set_type`
    /// (string-table offset).
    pub set_type_code: u8,
    pub legal: u64, // 2 bits per FORMATS entry (23 formats -> needs u64)
    pub flags: u32,
    /// derived predicates (F2_*), computed at load time (see index constants).
    pub flags2: u64,
    /// bulk `reprint` flag for this printing.
    pub reprint: bool,
    /// print-level variant frame treatment (is:atypical / is:default; see
    /// parse_line note).
    pub atypical: bool,
    /// masterpiece-series printing (set_type/ffx/promo-type 'masterpiece').
    pub masterpiece: bool,
    pub cn_val: u32, // numeric prefix of collector number
    pub released: u32, // YYYYMMDD, 0 = unknown
    /// prices: usd, usd_foil, tix, eur, eur_foil — NAN = absent.
    pub prices: [f32; 5],
    /// edhrec_rank; u32::MAX = absent.
    pub edhrec: u32,
    pub lang: u32,         // interned lowercase lang code
    pub flavor: u32,       // interned lowercased flavor text ("" when absent)
    pub watermark: u32,    // interned lowercase watermark ("" = none)
    pub illustration: u32, // interned illustration_id ("" = none)
    pub ptypes: u32,       // interned ",promo,types," joined lowercase ("" = none)
    pub stamp: u8,
    pub border: u8,
    pub frame: u8,
    pub games: u8,     // G_PAPER|G_MTGO|G_ARENA
    pub finishes: u8,  // FIN_NONFOIL|FIN_FOIL|FIN_ETCHED
    pub produced: u8,  // produced_mana bits (WUBRG | PR_C)
    pub ffx: u32,      // frame_effects bitset over FFX_LIST
    pub new_flags: u8, // NEW_* bits (first-in-oracle occurrences), filled post-parse
    pub defense: f32,  // battle defense (NAN = absent)
    /// parity w5 (borrow-plan §6): per-face numeric attributes packed 8 bits
    /// per face (max 8 faces): 0 = absent, 1..=254 = value+1, 255 = '*'.
    /// pow:/tou:/loyalty:/defense: comparisons satisfy on ANY face value.
    pub face_pow: u64,
    pub face_tou: u64,
    pub face_loy: u64,
    pub face_def: u64,
}

/// Pack card_faces numeric attributes into the 4×(8-bit×8) words stored on
/// Card (see face_pow). 0 = absent, 1..=254 = value+1, 255 = '*' wildcard.
pub fn pack_face_nums(faces: &[serde_json::Value]) -> (u64, u64, u64, u64) {
    let code = |f: &serde_json::Value, k: &str| -> u64 {
        match f.get(k) {
            Some(serde_json::Value::String(s)) => {
                let t = s.trim();
                if t.is_empty() {
                    return 0;
                }
                if t == "*" || t == "?" {
                    return 255;
                }
                match t.parse::<f64>() {
                    Ok(x) if x >= 0.0 => ((x as u32).min(253) + 1) as u64,
                    _ => 0,
                }
            }
            Some(serde_json::Value::Number(n)) => match n.as_f64() {
                Some(x) if x >= 0.0 => ((x as u32).min(253) + 1) as u64,
                _ => 0,
            },
            _ => 0,
        }
    };
    let mut pow = 0u64;
    let mut tou = 0u64;
    let mut loy = 0u64;
    let mut def = 0u64;
    for (k, f) in faces.iter().take(8).enumerate() {
        pow |= code(f, "power") << (8 * k);
        tou |= code(f, "toughness") << (8 * k);
        loy |= code(f, "loyalty") << (8 * k);
        def |= code(f, "defense") << (8 * k);
    }
    (pow, tou, loy, def)
}

pub struct Orders {
    pub name: Vec<u32>,
    pub cmc: Vec<u32>,
    pub rarity: Vec<u32>,
    pub set: Vec<u32>,
    pub released: Vec<u32>,
}

/// Aggregated per-oracle stats (built once at load) for reprint/artist operators.
#[derive(Clone, Copy, Default)]
pub struct OracleStats {
    pub n_prints: u16,
    pub n_sets: u16,
    pub n_paper: u16,
    pub n_paper_sets: u16,
    pub n_artists: u16,
    pub n_illus: u16,
    pub games: u8,
    /// any non-token printing carries the bulk `reprint` flag (is:reprint).
    pub reprint: bool,
    /// any non-token printing LACKS the bulk `reprint` flag. Upstream
    /// `not:reprint` is a per-print exists() predicate (its own printing is
    /// non-reprint), NOT the oracle-level complement of is:reprint: upstream
    /// is:reprint(17469)+not:reprint(33826) > pool(3383x) with a huge overlap
    /// (proven vs golden 'not:reprint game:paper' 33127 vs oracle-complement
    /// 15491).
    pub nrp: bool,
    /// every printing of this oracle is token-like (is:unique/is:reprint skip
    /// these; t:creature etc still include tokens, matching upstream).
    pub tok_oracle: bool,
    /// has at least one non-auxiliary (non memorabilia/promo/box/extras-layout/
    /// hidden) printing — upstream only offers is:unique/is:reprint for such
    /// oracles (proven: full 16168/17469 list coverage).
    pub visible: bool,
    /// any printing (gated or not) has layout art_series: upstream
    /// has:art_series = 2352 counts cards whose art got a series print
    /// (the aux prints themselves stay invisible; only the oracle surfaces).
    pub has_artseries: bool,
}

/// Aggregated per-set row for /api/sets (built once at startup, cached).
/// All string fields are offsets into `Index::strings`. `sample_image` is
/// u32::MAX when the set has no card image; `released` is YYYYMMDD (0 = unknown).
///
/// The `block`/`parent_set`/`icon`/`block_rank`/`printed_size`/flag fields are
/// merged in from the vendored `data/blocks.json` + `data/sets-meta.json`
/// (see data/gen_sets_meta.py) by `Index::load_set_meta`; they stay at their
/// sentinel values (u32::MAX / false) when the vendored files are missing.
#[derive(Clone, Copy)]
pub struct SetEntry {
    pub code: u32,
    pub name: u32,
    pub set_type: u32,
    pub released: u32,
    pub card_count: u32,
    pub sample_image: u32,
    pub best_cn: u32,
    /// block name string offset, u32::MAX = unblocked
    pub block: u32,
    /// block index in the vendored blocks.json order (u32::MAX = unblocked)
    pub block_rank: u32,
    /// parent set code string offset (u32::MAX = none; null on this deployment)
    pub parent_set: u32,
    /// icon svg uri string offset (u32::MAX = none)
    pub icon: u32,
    /// printed_size from Scryfall (u32::MAX = unknown)
    pub printed_size: u32,
    pub digital: bool,
    pub foil_only: bool,
    pub nonfoil_only: bool,
    /// type breakdown over the set's printings (see load_set_meta)
    pub n_creatures: u32,
    pub n_noncreatures: u32,
    pub n_lands: u32,
}

pub struct Index {
    pub cards: Vec<Card>,
    pub strings: Vec<Box<str>>,
    /// Pre-serialized compact JSON per card (served verbatim, zero re-serialization).
    pub jsons: Vec<Box<str>>,
    pub id_map: HashMap<Box<str>, u32>,

    // lowercased mirror offsets (into `strings`) for scans
    pub name_lc: Vec<u32>,
    pub type_lc: Vec<u32>,
    pub oracle_lc: Vec<u32>,
    /// reminder-stripped, lowercased oracle text (upstream o: search field)
    pub otext_lc: Vec<u32>,
    /// mana cost incl. DFC face costs joined by " /// " (m:{…} search)
    pub cost_all: Vec<u32>,
    pub artist_lc: Vec<u32>,
    pub set_name_lc: Vec<u32>,
    pub mana_norm: Vec<u32>, // {R}{G} -> "rg"
    /// packed coloured-pip counts, 4 bits per colour (w,u,b,r,g), computed
    /// from the raw mana_cost before brace-stripping (devotion: needs
    /// per-symbol counts; "rg" without braces is ambiguous with {R/G}).
    pub pip: Vec<u32>,

    // inverted indexes
    pub name_tokens: HashMap<String, RoaringBitmap>,
    pub type_tokens: HashMap<String, RoaringBitmap>,
    pub text_tokens: HashMap<String, RoaringBitmap>,
    /// oracle-text-only token index with reminder text stripped and light
    /// stemming (draws/drawn→draw etc); also folds compound keywords
    /// (landcycling→cycling). o:/oracle: filters use this, NOT text_tokens
    /// (which mixes in type-line words and caused o:draw over-matching).
    pub otext_tokens: HashMap<String, RoaringBitmap>,
    pub keyword_tokens: HashMap<String, RoaringBitmap>,
    /// full-keyword-phrase -> print bitmap, EXACT enum semantics (lowercased
    /// phrase keys, gate: non-token/non-hidden/non-aux prints). Upstream
    /// keyword:/kw: matches whole keyword phrases, never word prefixes:
    /// keyword:Double = 138 (the Spider-Man "Double" keyword), not 288
    /// (word-token "double" also inside Double strike / Double team).
    pub keyword_phrase: HashMap<Box<str>, RoaringBitmap>,
    /// lowercased full-phrase keyword enum (closed value space for kw:).
    pub kw_enum: std::collections::HashSet<String>,
    /// (name_lc offset, card idx) sorted by lowercased name — prefix search + suggest.
    pub prefix: Vec<(u32, u32)>,

    pub color_bits: [RoaringBitmap; 5],
    /// print membership for bare-c: INCLUDE semantics: print is in
    /// color_incl[m] iff ANY of its faces (incl. top) has colors ⊇ m.
    /// Upstream golden c:wb 626 / c:rw 688 / c:gu 645 = any-face superset,
    /// not the union-mask contains (639/699/657).
    pub color_incl: [RoaringBitmap; 32],
    pub ci_bits: [RoaringBitmap; 5],
    pub subsets: [RoaringBitmap; 32],   // colors ⊆ m
    pub exacts: [RoaringBitmap; 32],    // colors == m
    pub ci_subsets: [RoaringBitmap; 32],
    pub ci_exacts: [RoaringBitmap; 32],
    pub rarity_idx: [RoaringBitmap; 8],
    pub set_idx: HashMap<Box<str>, RoaringBitmap>,
    pub layout_idx: HashMap<Box<str>, RoaringBitmap>,
    /// name-token id -> token string (candidate pool for the fuzzy word scan)
    pub name_tok_list: Vec<Box<str>>,
    /// first-2-bytes key (b0<<8|b1) -> name-token ids — fuzzy candidate lookup
    pub name_tok_buckets: HashMap<u16, Vec<u32>>,
    /// type-token id -> token string + same first-2-bytes buckets (type fuzzy)
    pub type_tok_list: Vec<Box<str>>,
    pub type_tok_buckets: HashMap<u16, Vec<u32>>,
    pub format_legal: [RoaringBitmap; NFORMATS],
    pub format_banned: [RoaringBitmap; NFORMATS],
    pub format_restricted: [RoaringBitmap; NFORMATS],

    // ---- parity extras (syntax-parity work, Oct 2026) ----
    /// per-oracle aggregate stats for prints=, sets=, paperprints=, artists>, ...
    pub oracle_stats: Vec<OracleStats>,
    /// oracle row -> printing card indices (for oracle-level filters).
    pub oracle_prints: Vec<RoaringBitmap>,
    /// "ever printed in" oracle-row indexes for `in:`.
    pub in_sets: HashMap<Box<str>, RoaringBitmap>,
    pub in_set_types: HashMap<Box<str>, RoaringBitmap>,
    pub in_games: HashMap<Box<str>, RoaringBitmap>,
    pub in_langs: HashMap<Box<str>, RoaringBitmap>,
    /// Wave-5 item 3: lang -> EVERY printing of every oracle that has >=1
    /// print in that language, sourced from the full Scryfall all-cards
    /// bulk (default-cards.jsonl.gz is English-slimmed, so the print-level
    /// lang_idx above only sees ~400 ja prints vs upstream's 30,853 cards).
    /// Built by load_langs() from bulk-langs/lang_map_full.json.gz.
    pub oracle_lang_idx: HashMap<Box<str>, RoaringBitmap>,
    pub in_rarities: HashMap<Box<str>, RoaringBitmap>,
    /// printing-level indexes for st:/game:/border:/frame:/stamp:/framefx-ish.
    pub settype_idx: HashMap<Box<str>, RoaringBitmap>,
    pub border_idx: HashMap<Box<str>, RoaringBitmap>,
    pub frame_idx: HashMap<Box<str>, RoaringBitmap>,
    pub games_idx: HashMap<Box<str>, RoaringBitmap>,
    pub stamp_idx: HashMap<Box<str>, RoaringBitmap>,
    pub ffx_idx: HashMap<Box<str>, RoaringBitmap>,
    pub ptype_idx: HashMap<Box<str>, RoaringBitmap>,
    pub lang_idx: HashMap<Box<str>, RoaringBitmap>,
    /// b: (block name or any member set code) -> printings; built in load_set_meta.
    pub block_idx: HashMap<Box<str>, RoaringBitmap>,
    /// set name (lowercase) -> block name; powers the b: member-set-name
    /// fallback (b:legacy -> "Urza's Legacy" -> block "Urza").
    pub block_setname: HashMap<Box<str>, Box<str>>,
    /// g: (set code/name) -> printings of the set's release group (self, parent,
    /// siblings, children); built in load_set_meta.
    pub group_idx: HashMap<Box<str>, RoaringBitmap>,
    /// today as YYYYMMDD (for released:>now / date>now), captured at load.
    pub today: u32,
    /// land-cycle name sets (is:dual etc), loaded from data/land_cycles.json.
    pub land_cycles: std::collections::HashMap<String, Vec<String>>,

    // oracle table (otag:)
    /// per-card -> oracle row index
    pub card_oracle: Vec<u32>,
    /// oracle row -> interned oracle_id string offset
    pub oracle_id_off: Vec<u32>,
    /// oracle row -> visible in Scryfall default search (calibrated rule)
    pub oracle_vis: Vec<bool>,
    /// oracle row -> union of color_identity over all its bulk printings
    /// (id: matches at ORACLE level: ci(oracle) ⊆ value letters).
    pub oracle_ci: Vec<u8>,
    /// oracle row -> visible in Scryfall's *unique-card pool* (the pool the
    /// `*` query counts: live total_cards(" * ") == 33816 == total_cards("id:wubrg")).
    /// = oracle_vis AND no memorabilia-style extras (see ID_EXTRA_HIDDEN_*).
    pub oracle_id_vis: Vec<bool>,
    /// printings whose oracle is id-pool visible (id_bitmap ANDs with this).
    pub id_vis_bits: RoaringBitmap,
    pub oracle_intern: HashMap<Box<str>, u32>,
    /// Parity w6a: per-printing side index from the all_cards bulk (see
    /// prints.rs). Empty/!loaded when the bulk file is absent — every
    /// predicate then behaves exactly as the default-cards baseline.
    pub side: crate::prints::SideIdx,

    // oracle tags (otag:)
    pub otags: Vec<OTag>,
    /// normalized slug -> tag index
    pub otag_slug: HashMap<Box<str>, u32>,
    /// normalized alias -> tag indices
    pub otag_alias: HashMap<Box<str>, Vec<u32>>,
    /// true once oracle-tags.jsonl.gz was loaded
    pub otag_ready: bool,

    pub orders: Orders,
    /// Aggregated set rows for /api/sets, sorted by released desc (built in load()).
    pub sets: Vec<SetEntry>,
    /// Pre-serialized JSON envelope per set for /api/set/{code}, keyed by the
    /// SetEntry.code string offset (built once by load_set_meta — one pass).
    pub set_detail_json: HashMap<u32, Box<str>>,
    pub universe: RoaringBitmap,
    pub build_ms: u128,
    pub source_bytes: u64,
}

fn color_mask(v: &[String]) -> u8 {
    let mut m = 0u8;
    for c in v {
        match c.as_str() {
            "W" => m |= 1,
            "U" => m |= 2,
            "B" => m |= 4,
            "R" => m |= 8,
            "G" => m |= 16,
            _ => {}
        }
    }
    m
}

pub(crate) fn rarity_rank(s: &str) -> u8 {
    match s {
        "common" => 0,
        "uncommon" => 1,
        "rare" => 2,
        "mythic" | "mythic rare" | "mythic_rare" => 3,
        "special" => 4,
        "bonus" => 5,
        "timeshifted" => 6,
        _ => RARITY_OTHER,
    }
}

/// Set-type enum codes (per-card `Card::set_type_code`). Dense, so the query
/// layer can index `[RoaringBitmap; NSET_TYPES]`-style tables the same way
/// `rarity_idx` works. Covers every set_type Scryfall's /sets dropdown lists
/// plus the hidden "extras" types (asset/stock/gallery) that Scryfall's
/// default search excludes.
pub const ST_ALCHEMY: u8 = 0;
pub const ST_ARCHENEMY: u8 = 1;
pub const ST_ARSENAL: u8 = 2;
pub const ST_BOX: u8 = 3;
pub const ST_COMMANDER: u8 = 4;
pub const ST_CUBE: u8 = 5;
pub const ST_CORE: u8 = 6;
pub const ST_DRAFT_INNOVATION: u8 = 7;
pub const ST_DUEL_DECK: u8 = 8;
pub const ST_ETERNAL: u8 = 9;
pub const ST_EXPANSION: u8 = 10;
pub const ST_FROM_THE_VAULT: u8 = 11;
pub const ST_FUNNY: u8 = 12;
pub const ST_MASTERPIECE: u8 = 13;
pub const ST_MASTERS: u8 = 14;
pub const ST_MEMORABILIA: u8 = 15;
pub const ST_MINIGAME: u8 = 16;
pub const ST_PLANECHASE: u8 = 17;
pub const ST_PREMIUM_DECK: u8 = 18;
pub const ST_PROMO: u8 = 19;
pub const ST_SPELLBOOK: u8 = 20;
pub const ST_STARTER: u8 = 21;
pub const ST_TOKEN: u8 = 22;
pub const ST_TREASURE_CHEST: u8 = 23;
pub const ST_VANGUARD: u8 = 24;
pub const ST_ASSET: u8 = 25;
pub const ST_STOCK: u8 = 26;
pub const ST_GALLERY: u8 = 27;
pub const ST_OTHER: u8 = 28;
pub const NSET_TYPES: usize = 29;

/// Reverse lookup: enum code -> canonical set_type string ("" for ST_OTHER).
pub const SET_TYPE_NAMES: [&str; NSET_TYPES] = [
    "alchemy", "archenemy", "arsenal", "box", "commander", "cube", "core",
    "draft_innovation", "duel_deck", "eternal", "expansion", "from_the_vault", "funny",
    "masterpiece", "masters", "memorabilia", "minigame", "planechase", "premium_deck",
    "promo", "spellbook", "starter", "token", "treasure_chest", "vanguard", "asset",
    "stock", "gallery", "",
];

pub fn set_type_code(s: &str) -> u8 {
    match s {
        "alchemy" => ST_ALCHEMY,
        "archenemy" => ST_ARCHENEMY,
        "arsenal" => ST_ARSENAL,
        "box" => ST_BOX,
        "commander" => ST_COMMANDER,
        "cube" => ST_CUBE,
        "core" => ST_CORE,
        "draft_innovation" => ST_DRAFT_INNOVATION,
        "duel_deck" => ST_DUEL_DECK,
        "eternal" => ST_ETERNAL,
        "expansion" => ST_EXPANSION,
        "from_the_vault" => ST_FROM_THE_VAULT,
        "funny" => ST_FUNNY,
        "masterpiece" => ST_MASTERPIECE,
        "masters" => ST_MASTERS,
        "memorabilia" => ST_MEMORABILIA,
        "minigame" => ST_MINIGAME,
        "planechase" => ST_PLANECHASE,
        "premium_deck" => ST_PREMIUM_DECK,
        "promo" => ST_PROMO,
        "spellbook" => ST_SPELLBOOK,
        "starter" => ST_STARTER,
        "token" => ST_TOKEN,
        "treasure_chest" => ST_TREASURE_CHEST,
        "vanguard" => ST_VANGUARD,
        "asset" => ST_ASSET,
        "stock" => ST_STOCK,
        "gallery" => ST_GALLERY,
        _ => ST_OTHER,
    }
}

// ---------------------------------------------------------------------------
// Oracle tags (otag:) — calibrated "default search" visibility rules.
//
// Calibrated 2026-10 against live scryfall.com otag: result sets (6 tags,
// full paginated oracle-id diffs: ramp/draw/counterspell/mana-rock/
// donate-token/cantrip): an oracle is in default search results iff ANY of
// its bulk printings has a set_type outside OTAG_HIDDEN_SET_TYPES, a layout
// outside OTAG_HIDDEN_LAYOUTS, and a set code outside OTAG_HIDDEN_SETS.
// (over=0 / under=0 on all six with these rules; mb2 + hbg printings are
// NOT hidden despite appearances in early partial fetches.)
// ---------------------------------------------------------------------------

/// set_type values that never make an oracle visible in default search.
pub const OTAG_HIDDEN_SET_TYPES: &[&str] = &[
        "archenemy",
    "duel_deck",
    "promo",
    "token",
    "vanguard",
];

/// layouts that never make an oracle visible, regardless of printing set.
pub const OTAG_HIDDEN_LAYOUTS: &[&str] = &[
        "emblem",
    "planar",
    "scheme",
    "token",
    "vanguard",
];

// ---------------------------------------------------------------------------
// id: (card colour abbreviation) pool model — reverse-engineered from live
// Scryfall full-set diffs (see scratch/id_probe*.py, id_fit2.py, Oct 2026):
//   id:VALUE  ⟺  oracle color_identity ⊆ letters(VALUE)   (case/order/dupes
//   irrelevant; value "c" means colorless identity; any other char outside
//   wubrgc => Scryfall ignores the whole term => 400 "All of your terms were
//   ignored" — we mirror with warning + 0 results).
//   Counts are DISTINCT ORACLES in the visible pool; Scryfall's unique pool
//   hides a few extras beyond the otag model: minigame set_types and
//   art_series/front_card layouts (memorabilia, showcase art, DFT token sides)
//   plus individually-suppressed oracles. Fitted residuals vs live across 22
//   probed values: ≤ +92 (+0.3%), monotone in pool size = blocklist staleness.
// ---------------------------------------------------------------------------

/// set_types hidden from the id:/unique pool BEYOND OTAG_HIDDEN_SET_TYPES.
pub const ID_EXTRA_HIDDEN_SET_TYPES: &[&str] = &["minigame"];

/// layouts hidden from the id:/unique pool BEYOND OTAG_HIDDEN_LAYOUTS.
pub const ID_EXTRA_HIDDEN_LAYOUTS: &[&str] = &["art_series", "front_card"];

/// Oracles Scryfall suppresses from the unique-card pool (blocked content
/// added since the OTAG calibration snapshot + SLX gimmick counters).
pub const ID_BLOCKED_ORACLES: &[&str] = &[
    "a610c77c-fe31-4465-a1c1-392db4ce4ed1", // Cleanse
    "4692740f-be90-459f-8d90-c4ae71771595", // Crusade
    "b18b9869-8490-4875-a5bb-484c3299f2c5", // Jihad
    "2d22d905-2f30-426c-b610-e5579141da10", // Klement, Life Acolyte
    "c93db66e-7096-4085-84f2-990b46b1747b", // Lae'zel, Blessed Warrior
    "49f6e521-0624-4906-b88d-8a52d625e541", // Lulu, Helpful Hollyphant
    "d58fdb22-4ff3-4cd6-91c8-aaaa76f85071", // Rasaad, Radiant Monk
    "b33fc3cd-da49-46d7-9816-a2ea50fba2ce", // Red Mana
    "f55a5b8a-ee73-4248-90c7-d2456928448e", // Sticker sheet
    "f5ad2fc1-79c1-4d37-ad3e-e57e67ce2739", // Storm Counter
];

/// set codes that never make an oracle visible: acorn/comedy products, promo boxes,
/// Mystery Booster 2 test cards, Planechase/Archenemy products, vanguard, token
/// products, and pre-release (alpha/beta/summ) printings.
/// Calibrated against scryfall.com live result counts (see calib notes above).
pub const OTAG_HIDDEN_SETS: &[&str] = &[
        "2ed",
    "4bb",
    "4ed",
    "5ed",
    "6ed",
    "ced",
    "cei",
    "cmb1",
    "cmb2",
    "dci",
    "ddf",
    "ddp",
    "ddr",
    "fbb",
    "h17",
    "hho",
    "lea",
    "leb",
    "mb2",
    "me1",
    "me3",
    "oarc",
    "oe01",
    "ohop",
    "opc2",
    "opca",
    "past",
    "pcel",
    "pf24",
    "pf25",
    "pf26",
    "ph17",
    "ph18",
    "ph19",
    "ph20",
    "ph21",
    "ph22",
    "ph23",
    "phop",
    "phtr",
    "pl23",
    "pl26",
    "plg24",
    "plny",
    "plst",
    "pmic",
    "pmoa",
    "ppc1",
    "prm",
    "psdg",
    "pssc",
    "psus",
    "ptsnc",
    "punk",
    "pvan",
    "sum",
    "t2x2",
    "t2xm",
    "t30a",
    "t40k",
    "tacr",
    "taer",
    "tafc",
    "tafr",
    "takh",
    "tarb",
    "tbfz",
    "tbig",
    "tblb",
    "tblc",
    "tbng",
    "tbro",
    "tbth",
    "tc14",
    "tc15",
    "tc17",
    "tc18",
    "tc19",
    "tc20",
    "tc21",
    "tclb",
    "tcmm",
    "tcmr",
    "tcn2",
    "tdag",
    "tddi",
    "tdft",
    "tdgm",
    "tdmu",
    "tdom",
    "tdrc",
    "tdsc",
    "tdsk",
    "tecc",
    "tecl",
    "teld",
    "temn",
    "teoc",
    "teoe",
    "tfdc",
    "tfdn",
    "tfic",
    "tfin",
    "tfra",
    "tfrc",
    "tfth",
    "tgn3",
    "tgrn",
    "tgtc",
    "thob",
    "thou",
    "thp1",
    "thp2",
    "thp3",
    "tiko",
    "tinr",
    "tkhm",
    "tkld",
    "tktk",
    "tlcc",
    "tlci",
    "tltc",
    "tltr",
    "tm13",
    "tm14",
    "tm15",
    "tm19",
    "tm20",
    "tm21",
    "tm3c",
    "tmed",
    "tmh2",
    "tmh3",
    "tmic",
    "tmid",
    "tmkc",
    "tmkm",
    "tmm2",
    "tmm3",
    "tmoc",
    "tmom",
    "tmsc",
    "tmsh",
    "tncc",
    "tneo",
    "togw",
    "tonc",
    "tone",
    "tori",
    "totc",
    "totj",
    "totp",
    "tpca",
    "tpip",
    "trex",
    "trix",
    "trna",
    "troe",
    "trvr",
    "tscd",
    "tsnc",
    "tsoc",
    "tsoi",
    "tsos",
    "tspm",
    "tstx",
    "ttdc",
    "ttdm",
    "tthb",
    "tths",
    "ttla",
    "ttmc",
    "ttmt",
    "tunf",
    "tust",
    "tvoc",
    "tvow",
    "twar",
    "twho",
    "twoe",
    "txln",
    "unk",
    "wfin",
    "wmkm",
    "wmom",
    "wwoe",
];

/// Oracles Scryfall hides from default search despite normal-looking printings
/// (hbg digital special guests, blocked content). Calibrated per-oracle blocklist.
pub const OTAG_BLOCKED_ORACLES: &[&str] = &[
        "040cbd08-7c8c-46c4-9ab6-d7ec0c43f9d2",
    "051333e4-a826-46e8-a46e-1571078112fc",
    "0c595dc4-3692-424e-8699-9e8e663b32cc",
    "115d153e-4bdc-4fd6-81f8-0d77db7f657a",
    "1551e394-da71-460b-8878-812eb60506a6",
    "189a8465-ad45-43fc-a65e-728930a48524",
    "2807f9a9-34c2-4036-b726-312d2254ee7b",
    "29732de3-d41d-493a-9bad-6d38e8b6ea3f",
    "2d22d905-2f30-426c-b610-e5579141da10",
    "2dfc72ed-d2d4-4f89-a459-39d06839a257",
    "2ec14744-e633-4939-8047-7db447059fe8",
    "34524530-250e-4fab-b363-d2176d0061e0",
    "3695c512-3dc3-4a3d-8eae-33914c0c79db",
    "38528c64-ede7-4b6e-b55a-5a3342e34c68",
    "3953cf2b-f6b6-482f-b730-05185b40eab4", // Alora, Cheerful Swashbuckler,
    "3ed78e19-d512-4fca-83e9-ae68c5a28c07",
    "42d673ed-8398-4290-b43a-1ef753206c6c",
    "4692740f-be90-459f-8d90-c4ae71771595",
    "484e8e49-e604-4721-95a4-dd69eb11d4c6",
    "4b242289-b219-4eb1-b5d3-b92453091986",
    "52f1db63-3954-4f57-b9ea-edb9fc8b207d",
    "56cb1a5d-4256-467c-a185-8f3d31030158",
    "5ca4e8c4-8358-49be-bafb-85849bf9bd01",
    "5d86f79f-9982-4c89-ad8b-4d59fd76c940",
    "5e2a076b-3afd-432b-9246-c6a979c2b61d",
    "7108939e-521f-4a1a-b148-d83e38708a6a",
    "7a24b1ed-962b-42a6-b4dd-2979523050ff",
    "7d7e232f-e4f9-41db-a2d9-cfe88721b7a9",
    "817dda5d-acfd-47f6-ba57-b41b17db4821",
    "854ad486-0c59-4c57-9a76-ab1dff0ff37c",
    "8f788db4-21a0-4bb2-a162-bb4efe8e4a13",
    "94f849a3-39c8-4e9a-b359-cdeea1bf7ee2",
    "9e936885-0786-41b5-933a-d0953e751116",
    "a49f5823-23dd-497a-a557-3477fe764a72",
    "a5452a30-0661-4631-9620-cf669371a7aa",
    "a610c77c-fe31-4465-a1c1-392db4ce4ed1",
    "a65cb73e-d654-4975-8e6d-2842a0bae38d",
    "a74d6cb3-e2e0-4deb-8fce-971a341c1999",
    "ab2eb85c-a7ed-407e-b1b1-210735f8897c",
    "b18b9869-8490-4875-a5bb-484c3299f2c5",
    "b5c21cb0-95fc-4056-9c69-d1eba3bc853b",
    "ba2fdfb6-0a8b-440f-9582-1fa689f96720",
    "ba4721ba-a14d-43f0-9ee8-619e3f93666a",
    "bd276f47-63a1-49e8-9f6e-ed88ef3b9c45",
    "bf15a083-99ba-40e7-8cab-722ef1e39fb6",
    "c03efa9a-8107-4e2b-bf76-4924e491ade7",
    "c1cdb8fa-adae-47c1-a81d-d296590d8280",
    "d92b853b-433b-4c76-9d39-d6796be8adda",
    "d9fdfc38-ef02-4f36-8ad4-6432f2bd7253",
    "dd1960ae-7e67-42aa-a2eb-59205126aa11",
    "e0157b20-172e-4656-a70d-bb00510302fa",
    "e6b3028c-5ad6-4802-a414-3084ec02786e",
    "e8700eb2-881c-45f7-8ff4-760cfa5edea5",
    "f1bcc02f-c57e-4472-b3fa-9869a0a3e9f1",
    "f5f80c00-464b-4758-897a-b0c5ebfc7488",
    "f84bcd39-243d-4531-8320-48d6258d8833",
];

/// Calibrated visibility rule (per oracle, conditions evaluated INDEPENDENTLY
/// across all of the oracle's bulk printings): the oracle is visible iff it
/// has >=1 printing whose set_type is not hidden AND >=1 printing whose layout
/// is not hidden AND >=1 printing whose set code is not hidden.
#[inline]
pub fn otag_good_set_type(set_type: &str) -> bool {
    !OTAG_HIDDEN_SET_TYPES.contains(&set_type)
}

#[inline]
pub fn otag_good_layout(layout: &str) -> bool {
    !OTAG_HIDDEN_LAYOUTS.contains(&layout)
}

#[inline]
pub fn otag_good_set_code(set_code: &str) -> bool {
    !OTAG_HIDDEN_SETS.contains(&set_code)
}

/// id:/unique-pool variants: otag list + the extras proven by the live diffs.
#[inline]
pub fn id_good_set_type(set_type: &str) -> bool {
    otag_good_set_type(set_type) && !ID_EXTRA_HIDDEN_SET_TYPES.contains(&set_type)
}

#[inline]
pub fn id_good_layout(layout: &str) -> bool {
    otag_good_layout(layout) && !ID_EXTRA_HIDDEN_LAYOUTS.contains(&layout)
}

/// otag value normalization: lowercase, spaces/underscores -> hyphens, collapsed.
pub fn normalize_otag_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut prev_hyphen = false;
    for c in v.trim().chars() {
        if c == ' ' || c == '_' || c == '-' {
            if !prev_hyphen && !out.is_empty() {
                out.push('-');
            }
            prev_hyphen = true;
        } else {
            out.extend(c.to_lowercase());
            prev_hyphen = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// One oracle tag: children (hierarchy) + directly tagged oracle rows.
pub struct OTag {
    pub slug: Box<str>,
    pub children: Vec<u32>,
    /// interned oracle-table ROW indices of this tag's direct taggings
    pub oracles: Vec<u32>,
}

fn legality_code(s: &str) -> u32 {
    match s {
        "legal" => L_LEGAL,
        "banned" => L_BANNED,
        "restricted" => L_RESTRICTED,
        _ => L_NOT_LEGAL,
    }
}

fn num_of(v: &Value) -> f32 {
    match v {
        Value::Number(n) => n.as_f64().map(|f| f as f32).unwrap_or(f32::NAN),
        Value::String(s) => s.trim().parse::<f32>().unwrap_or(f32::NAN),
        _ => f32::NAN,
    }
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn parse_released(s: &str) -> u32 {
    // "2024-08-02" -> 20240802
    let b = s.as_bytes();
    if b.len() >= 10 && b[4] == b'-' && b[7] == b'-' {
        let digits: Option<u32> = [0, 1, 2, 3, 5, 6, 8, 9].iter().try_fold(0u32, |acc, &i| {
            b[i].is_ascii_digit().then_some(acc * 10 + (b[i] - b'0') as u32)
        });
        digits.unwrap_or(0)
    } else {
        0
    }
}

pub(crate) fn parse_cn(s: &str) -> u32 {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().unwrap_or(u32::MAX)
}

/// Sentinel for a P/T/loyalty box printed as '*' (distinct from NAN=absent).
/// Upstream treats '*' as a WILDCARD in inequalities: pow<=3 also matches
/// 0/* creatures (golden pow<1 1064 vs numeric-only 807), but pow=3 does not.
pub const STAR: f32 = -3.0;

fn pt_num(v: Option<&serde_json::Value>) -> f32 {
    match v {
        None => f32::NAN,
        Some(x) => match x.as_str() {
            Some(s) if s.trim() == "*" || s.trim() == "?" => STAR,
            _ => num_of(x),
        },
    }
}

/// Owned intermediate record (per line, parsed in parallel).
struct RawC {
    id: String,
    oracle_id: String,
    name: String,
    set: String,
    set_name: String,
    set_type: String,
    collector_number: String,
    cn_key: String,
    type_line: String,
    type_tokens: Vec<String>,
    mana_cost: String,
    /// mana_cost with DFC face costs joined by " /// " when the top-level
    /// cost is empty (this dump stores back/front costs on the faces).
    mana_cost_all: String,
    oracle_text: String,
    artist: String,
    keywords_joined: String,
    layout: String,
    img_small: String,
    img_normal: String,
    uri: String,
    colors: u8,
    ci: u8,
    face_pack: u64,
    rarity: u8,
    cmc: f32,
    power: f32,
    toughness: f32,
    loyalty: f32,
    def: f32,
    legal: u64,
    flags: u32,
    cn_val: u32,
    released: u32,
    prices: [f32; 5],
    edhrec: u32,
    lang: String,
    flavor: String,
    watermark: String,
    illustration: String,
    ptypes: String,
    stamp: u8,
    border: u8,
    frame: u8,
    games: u8,
    finishes: u8,
    produced: u8,
    ffx: u32,
    flags2: u64,
    reprint: bool,
    atypical: bool,
    masterpiece: bool,
    face_pow: u64,
    face_tou: u64,
    face_loy: u64,
    face_def: u64,
}

/// Remove balanced-parenthesis runs (reminder text) from oracle text before
/// indexing, matching Scryfall's oracle-word index behaviour (proven with
/// o:draw: cards whose only 'draw' sits in a (…) reminder are excluded).
pub fn strip_paren_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0u32;
    for ch in s.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ => {
                if depth == 0 {
                    out.push(ch);
                }
            }
        }
    }
    out
}

fn parse_line(line: &str) -> Option<RawC> {
    let v: Value = serde_json::from_str(line).ok()?;

    let mut legal = 0u64;
    if let Some(m) = v.get("legalities").and_then(|x| x.as_object()) {
        for (fi, f) in FORMATS.iter().enumerate() {
            if let Some(st) = m.get(*f).and_then(|x| x.as_str()) {
                legal |= (legality_code(st) as u64) << (fi as u32 * 2);
            }
        }
    }

    // ---- parity fields (prices/edhrec/lang/flavor/watermark/… ) ----
    let mut prices = [f32::NAN; 5];
    if let Some(pr) = v.get("prices") {
        for (i, k) in ["usd", "usd_foil", "tix", "eur", "eur_foil"].iter().enumerate() {
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
    let edhrec = v
        .get("edhrec_rank")
        .and_then(|x| x.as_u64())
        .map(|x| x as u32)
        .unwrap_or(u32::MAX);
    let lang = {
        let l = str_of(&v, "lang").to_lowercase();
        if l.is_empty() { "en".to_string() } else { l }
    };
    let mut flavor = str_of(&v, "flavor_text").to_lowercase();
    if flavor.trim().is_empty() {
        if let Some(faces) = v.get("card_faces").and_then(|x| x.as_array()) {
            let joined: Vec<String> = faces
                .iter()
                .filter_map(|f| f.get("flavor_text").and_then(|x| x.as_str()))
                .filter(|t| !t.trim().is_empty())
                .map(|t| t.to_lowercase())
                .collect();
            flavor = joined.join(" ");
        }
    }
    let mut watermark = str_of(&v, "watermark").to_lowercase();
    // Wave-5 item 2: DFC/modals carry watermark on card_faces[], not top
    // level. Python sim over the bulk: top-level ∪ face-level watermarks =
    // 4,701 distinct oracles — EXACTLY upstream has:watermark (4,701);
    // top-level alone is 4,610, and the engine counted 4,567 after gating.
    if watermark.is_empty() {
        if let Some(faces) = v.get("card_faces").and_then(|x| x.as_array()) {
            for f in faces {
                let fw = f.get("watermark").and_then(|x| x.as_str()).unwrap_or("");
                if !fw.is_empty() {
                    watermark = fw.to_lowercase();
                    break;
                }
            }
        }
    }
    let illustration = str_of(&v, "illustration_id").to_string();
    let ptypes = v
        .get("promo_types")
        .and_then(|x| x.as_array())
        .map(|a| {
            let mut s = String::new();
            for x in a {
                if let Some(t) = x.as_str() {
                    s.push(',');
                    s.push_str(&t.to_lowercase());
                }
            }
            s
        })
        .unwrap_or_default();
    let stamp = match str_of(&v, "security_stamp") {
        "oval" => ST_OVAL,
        "triangle" => ST_TRIANGLE,
        "arena" => ST_ARENA,
        "acorn" => ST_ACORN,
        "circle" => ST_CIRCLE,
        "heart" => ST_HEART,
        _ => ST_NONE,
    };
    let border = match str_of(&v, "border_color") {
        "white" => BD_WHITE,
        "silver" => BD_SILVER,
        "gold" => BD_GOLD,
        "borderless" => BD_BORDERLESS,
        "yellow" => BD_YELLOW,
        _ => BD_BLACK,
    };
    let frame = match str_of(&v, "frame") {
        "2003" => FR_2003,
        "1997" => FR_1997,
        "1993" => FR_1993,
        "future" => FR_FUTURE,
        _ => FR_2015,
    };
    let mut games = 0u8;
    if let Some(a) = v.get("games").and_then(|x| x.as_array()) {
        for x in a {
            match x.as_str().unwrap_or("") {
                "paper" => games |= G_PAPER,
                "mtgo" => games |= G_MTGO,
                "arena" => games |= G_ARENA,
                _ => {}
            }
        }
    }
    let mut finishes = 0u8;
    if v.get("nonfoil").and_then(|x| x.as_bool()).unwrap_or(false) {
        finishes |= FIN_NONFOIL;
    }
    if v.get("foil").and_then(|x| x.as_bool()).unwrap_or(false) {
        finishes |= FIN_FOIL;
    }
    if let Some(a) = v.get("finishes").and_then(|x| x.as_array()) {
        for x in a {
            match x.as_str().unwrap_or("") {
                "etched" => finishes |= FIN_ETCHED,
                "foil" => finishes |= FIN_FOIL,
                "nonfoil" => finishes |= FIN_NONFOIL,
                _ => {}
            }
        }
    }
    let mut produced = 0u8;
    if let Some(a) = v.get("produced_mana").and_then(|x| x.as_array()) {
        for x in a {
            match x.as_str().unwrap_or("") {
                "W" => produced |= 1,
                "U" => produced |= 2,
                "B" => produced |= 4,
                "R" => produced |= 8,
                "G" => produced |= 16,
                "C" => produced |= PR_C,
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

    // images: top-level, else first face that has them (DFCs)
    let mut img_small = String::new();
    let mut img_normal = String::new();
    if let Some(im) = v.get("image_uris").filter(|x| !x.is_null()) {
        img_small = str_of(im, "small").to_string();
        img_normal = str_of(im, "normal").to_string();
    }
    if img_normal.is_empty() {
        if let Some(faces) = v.get("card_faces").and_then(|x| x.as_array()) {
            for f in faces {
                if let Some(im) = f.get("image_uris").filter(|x| !x.is_null()) {
                    let n = str_of(im, "normal");
                    if !n.is_empty() {
                        img_small = str_of(im, "small").to_string();
                        img_normal = n.to_string();
                        break;
                    }
                }
            }
        }
    }

    let type_line = str_of(&v, "type_line").to_string();
    let tl_lc = type_line.to_lowercase();
    let mut type_tokens = Vec::new();
    for_each_token(&tl_lc, 2, &mut |t| type_tokens.push(t.to_string()));

    let mut flags = 0u32;
    // w7: set when the bulk preview object's source mentions Scryfall;
    // consumed once flags2 exists below (F2_SC_PREVIEW).
    let mut scryfall_preview = false;
    let has = |tok: &str| type_tokens.iter().any(|t| t == tok);
    let land = has("land");
    if land {
        flags |= F_LAND;
    }
    // Scryfall semantics: is:permanent = can enter the battlefield =
    // artifact/creature/enchantment/land/planeswalker/battle (any subtype
    // combo: Artifact Land, Creature Land, Host, Kingdom…); instants and
    // sorceries are never permanents. is:spell = castable = same set minus
    // lands, plus instant/sorcery. Conspiracy/Contraption/Vanguard/etc.
    // types are neither.
    let perm = has("artifact") || has("creature") || has("enchantment")
        || has("land") || has("planeswalker") || has("battle");
    let castable = has("artifact") || has("creature") || has("enchantment")
        || has("instant") || has("sorcery") || has("planeswalker") || has("battle");
    if perm && !has("instant") && !has("sorcery") {
        flags |= F_PERMANENT;
    }
    if castable {
        flags |= F_SPELL;
    }
    if has("creature") {
        flags |= F_CREATURE;
    }
    if has("legendary") {
        flags |= F_LEGENDARY;
    }
    if has("basic") {
        flags |= F_BASIC;
    }
    if has("snow") {
        flags |= F_SNOW;
    }
    if has("artifact") {
        flags |= F_ARTIFACT;
    }
    if has("enchantment") {
        flags |= F_ENCHANTMENT;
    }
    if has("planeswalker") {
        flags |= F_PLANESWALKER;
    }
    if has("battle") {
        flags |= F_BATTLE;
    }
    if has("instant") {
        flags |= F_INSTANT;
    }
    if has("sorcery") {
        flags |= F_SORCERY;
    }
    let mut colors = color_mask(
        &v.get("colors")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default(),
    );
    let mut ci = color_mask(
        &v.get("color_identity")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default(),
    );
    // Upstream c:/ci:/is:mono see the card's FULL colour profile — for DFC
    // prints this dump stores only the front face at top level, so union in
    // every face's colors/color_identity (Delver of Secrets is U+R upstream,
    // not U). Golden: c:w ours 7085 vs upstream 7179; ci:gu 873 vs 13276.
    let top_colors = colors;
    if (colors | ci).count_ones() < 5 {
        if let Some(faces) = v.get("card_faces").and_then(|x| x.as_array()) {
            let face_vec = |key: &str| -> Vec<String> {
                faces
                    .iter()
                    .filter_map(|f| f.get(key).and_then(|x| x.as_array()))
                    .flat_map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect::<Vec<String>>()
                    })
                    .collect()
            };
            colors |= color_mask(&face_vec("colors"));
            ci |= color_mask(&face_vec("color_identity"));
        }
    }
    // Per-face colour masks (5 bits/slot, max 8) for ANY-FACE superset
    // matching: upstream bare c:wb wants >=1 face containing both colours,
    // not the union mask (front-W/back-B DFCs over-count: golden c:wb 626
    // vs union 639; c:rw 688 vs 699; c:gu 645 vs 657).
    let mut face_pack: u64 = 0;
    {
        let mut masks: Vec<u8> = vec![top_colors];
        if let Some(faces) = v.get("card_faces").and_then(|x| x.as_array()) {
            for f in faces {
                let fm = f
                    .get("colors")
                    .and_then(|x| x.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                masks.push(color_mask(&fm));
            }
        }
        for (k, m) in masks.iter().take(8).enumerate() {
            face_pack |= (*m as u64 & 31) << (5 * k);
        }
    }
    if colors.count_ones() > 1 {
        flags |= F_MULTICOLORED;
    }
    if colors == 0 {
        flags |= F_COLORLESS;
    }
    let mut oracle_text = str_of(&v, "oracle_text").to_string();
    if oracle_text.trim().is_empty() {
        // DFC prints in this dump put the text on the faces
        if let Some(faces) = v.get("card_faces").and_then(|x| x.as_array()) {
            let joined: Vec<&str> = faces
                .iter()
                .filter_map(|f| f.get("oracle_text").and_then(|x| x.as_str()))
                .filter(|t| !t.trim().is_empty())
                .collect();
            oracle_text = joined.join(" /// ");
        }
    }
    if has("creature")
        && !has("planeswalker")
        && oracle_text.trim().is_empty()
        && (flags & F_LEGENDARY) == 0
    {
        flags |= F_VANILLA;
    }
    if !oracle_text.trim().is_empty() {
        flags |= F_HAS_TEXT;
    }
    if v.get("reserved").and_then(|x| x.as_bool()).unwrap_or(false) {
        flags |= F_RESERVED;
    }
    if v.get("game_changer").and_then(|x| x.as_bool()).unwrap_or(false) {
        flags |= F_GAMECHANGER;
    }
    if v.get("story_spotlight").and_then(|x| x.as_bool()).unwrap_or(false) {
        flags |= F_SPOTLIGHT;
    }
    if v.get("booster").and_then(|x| x.as_bool()).unwrap_or(false) {
        flags |= F_BOOSTER_FLAG;
    }
    if v.get("highres_image").and_then(|x| x.as_bool()).unwrap_or(false) {
        flags |= F_HIRES;
    }
    if !watermark.is_empty() {
        flags |= F_HAS_WM;
    }
    if v.get("color_indicator").is_some() {
        flags |= F_HAS_IND;
    }
    if v.get("card_faces").and_then(|x| x.as_array()).map(|a| !a.is_empty()).unwrap_or(false) {
        flags |= F_HAS_FACES;
    }
    // Parity w7 (borrow A1): bulk 'preview' is an OBJECT ({source,
    // previewed_at,…}) on previewed prints, not a bool — the old as_bool()
    // test never fired, silently disabling preview suppression (validated
    // sim treated a truthy dict as preview). present-but-null/false = not a
    // preview (bulk carries preview:false widely).
    if json_is_preview(v.get("preview")) {
        flags |= F_PREVIEW;
        if v.get("preview")
            .and_then(|p| p.get("source"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_ascii_lowercase().contains("scryfall"))
            .unwrap_or(false)
        {
            scryfall_preview = true;
        }
    }
    let layout = str_of(&v, "layout").to_string();
    if v
        .get("card_faces")
        .and_then(|x| x.as_array())
        .map_or(false, |a| !a.is_empty())
        || matches!(
            layout.as_str(),
            "transform" | "modal_dfc" | "meld" | "double_faced_token" | "flip"
        )
    {
        flags |= F_DFC;
    }
    if v.get("full_art").and_then(|x| x.as_bool()).unwrap_or(false) {
        flags |= F_FULLART;
    }
    if v.get("textless").and_then(|x| x.as_bool()).unwrap_or(false) {
        flags |= F_TEXTLESS;
    }
    if v.get("digital").and_then(|x| x.as_bool()).unwrap_or(false) {
        flags |= F_DIGITAL;
    }
    if v.get("promo").and_then(|x| x.as_bool()).unwrap_or(false) {
        flags |= F_PROMO;
    }
    if v.get("oversized").and_then(|x| x.as_bool()).unwrap_or(false) {
        flags |= F_OVERSIZED;
    }

    let keywords_joined = v
        .get("keywords")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                // "|" keeps multi-word keyword PHRASE boundaries intact
                // ("Double|Double strike" must not re-tokenize as words).
                .collect::<Vec<_>>()
                .join("|")
        })
        .unwrap_or_default();

    // ---- flags2: derived predicates calibrated against live Scryfall ----
    let faces_arr: Vec<serde_json::Value> = v
        .get("card_faces")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    // per-face power/toughness/loyalty/defense packs (see Card::face_pow)
    let fnums = pack_face_nums(&faces_arr);
    let fstr = |f: &serde_json::Value, k: &str| -> String {
        f.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
    };
    let fnum = |f: &serde_json::Value, k: &str| -> Option<f64> {
        match f.get(k) {
            Some(serde_json::Value::String(s)) => s.trim().parse::<f64>().ok(),
            Some(serde_json::Value::Number(n)) => n.as_f64(),
            _ => None,
        }
    };
    let main_tl: String = type_line.split("//").next().unwrap_or("").to_string();
    let mut main_words: Vec<String> = Vec::new();
    for_each_token(&main_tl.to_lowercase(), 2, &mut |t| main_words.push(t.to_string()));
    let has_main = |tok: &str| main_words.iter().any(|t| t == tok);
    let mut any_words: Vec<String> = Vec::new();
    if faces_arr.is_empty() {
        any_words = type_tokens.clone();
    } else {
        for f in &faces_arr {
            for_each_token(&fstr(f, "type_line").to_lowercase(), 2, &mut |t| {
                any_words.push(t.to_string())
            });
        }
    }
    let has_any = |tok: &str| any_words.iter().any(|t| t == tok);
    let main_text: String = if !faces_arr.is_empty() {
        faces_arr.first().map(|f| fstr(f, "oracle_text")).unwrap_or_default()
    } else {
        oracle_text.clone()
    };
    let joined_text = oracle_text.to_lowercase();
    let opt_num = |o: &serde_json::Value, k: &str| -> Option<f64> {
        match o.get(k) {
            Some(serde_json::Value::String(s)) => s.trim().parse::<f64>().ok(),
            Some(serde_json::Value::Number(n)) => n.as_f64(),
            _ => None,
        }
    };
    let mut flags2 = 0u64;
    if scryfall_preview {
        flags2 |= F2_SC_PREVIEW;
    }
    if str_of(&v, "set_type") == "token"
        || matches!(layout.as_str(), "token" | "double_faced_token")
        || type_tokens.iter().any(|t| t == "token")
    {
        flags2 |= F2_TOKEN;
    }
    if has_main("creature") {
        flags2 |= F2_CRE_MAIN;
    }
    if has_main("vehicle") || has_main("spacecraft") {
        flags2 |= F2_VEH_MAIN;
    }
    if has_main("legendary") {
        flags2 |= F2_LEG_MAIN;
    }
    if has_main("planeswalker") {
        flags2 |= F2_PW_MAIN;
    }
    if has_main("land") {
        flags2 |= F2_LAND_MAIN;
    }
    if has_any("planeswalker") {
        flags2 |= F2_PW_ANY;
    }
    if has_any("background") {
        flags2 |= F2_BG_ANY;
    }
    if has_any("historic") {
        flags2 |= F2_HIST_WORD;
    }
    if ["cleric", "warrior", "rogue", "wizard"]
        .iter()
        .any(|c| has_any(c))
    {
        flags2 |= F2_CLASS_PARTY;
    }
    if ["assassin", "pirate", "rogue", "warlock", "bard", "mercenary", "trader"]
        .iter()
        .any(|c| has_any(c))
    {
        flags2 |= F2_CLASS_OUTLAW;
    }
    if joined_text.contains("all creature type") || joined_text.contains("every creature type") {
        flags2 |= F2_ALLCT;
    }
    if joined_text.contains("meld") {
        flags2 |= F2_MELDTEXT;
    }
    if joined_text.contains("can be your commander") {
        flags2 |= F2_CBC;
    }
    let mut pts: Vec<(Option<f64>, Option<f64>, Option<f64>)> = vec![(
        opt_num(&v, "power"),
        opt_num(&v, "toughness"),
        opt_num(&v, "cmc"),
    )];
    for f in &faces_arr {
        pts.push((fnum(f, "power"), fnum(f, "toughness"), fnum(f, "cmc")));
    }
    if pts.iter().any(|(p, t, m)| {
        p == &Some(2.0) && t == &Some(2.0) && m == &Some(2.0)
    }) && (flags2 & (F2_CRE_MAIN | F2_VEH_MAIN) != 0
        || any_words.iter().any(|t| t == "creature"))
    {
        flags2 |= F2_BEAR;
    }
    if !main_text.trim().is_empty() {
        flags2 |= F2_MAIN_TEXT;
    }
    if has_main("creature") && !has_main("planeswalker") && main_text.trim().is_empty() {
        flags2 |= F2_VANILLA;
    }
    if str_of(&v, "name").contains("//") {
        flags2 |= F2_NAME_SLASH;
    }
    if is_hidden_set_code(str_of(&v, "set")) {
        flags2 |= F2_HIDDEN;
    }
    if (str_of(&v, "set_type") == "expansion" || str_of(&v, "set_type") == "core")
        && keywords_joined
            .split_whitespace()
            .any(|k| k.eq_ignore_ascii_case("companion"))
    {
        flags2 |= F2_COMPANION;
    }
    if has_any("angel") {
        flags2 |= F2_ANGEL;
    }
    if (flags2 & (F2_CRE_MAIN | F2_MAIN_TEXT | F2_PW_MAIN)) == (F2_CRE_MAIN | F2_MAIN_TEXT)
        && keyword_only_text(&main_text)
    {
        flags2 |= F2_FRENCH;
    }
    // duplicate the F_PREVIEW semantic fix: 'preview' present-but-false is not
    // a preview card (bulk data carries preview:false on many prints).
    let reprint = v.get("reprint").and_then(|x| x.as_bool()).unwrap_or(false);
    let cn_key = {
        let cn = str_of(&v, "collector_number");
        cn.trim()
            .trim_end_matches(|c: char| {
                c == '\u{2605}' || c == '\u{2606}' || c == '\u{2730}' || c.is_whitespace()
            })
            .to_string()
    };

    // is:atypical / is:masterpiece print-level predicates (fit vs live
    // upstream unique:cards counts: atypical=10562, default=33488,
    // masterpiece=877, etched∩atypical=1018 — see scratch fit_is.py):
    // a print is "atypical" if it carries a variant frame treatment.
    let mut atypical = false;
    let mut masterpiece = false;
    {
        if let Some(a) = v.get("frame_effects").and_then(|x| x.as_array()) {
            for x in a {
                match x.as_str().unwrap_or("") {
                    "inverted" | "extendedart" | "showcase" | "serialized" | "invitational"
                    | "prizemasterpiece" | "colorshifted" => atypical = true,
                    "masterpiece" => {
                        atypical = true;
                        masterpiece = true;
                    }
                    _ => {}
                }
            }
        }
        match str_of(&v, "frame") {
            "showcase" | "textless" | "inverted" | "extendedart" => atypical = true,
            _ => {}
        }
        if v.get("textless").and_then(|x| x.as_bool()).unwrap_or(false) {
            atypical = true;
        }
        if str_of(&v, "set_type") == "masterpiece" {
            masterpiece = true;
        }
        if let Some(a) = v.get("promo_types").and_then(|x| x.as_array()) {
            for x in a {
                if x.as_str().unwrap_or("") == "masterpiece" {
                    masterpiece = true;
                }
            }
        }
    }

    let mc = str_of(&v, "mana_cost").to_string();
    // cost_all = every printed cost of the card (top + each face) in one
    // " /// "-joined string: upstream m:/ManaSyms searches BOTH faces even
    // when the front has a cost (Transform // back-side costs matched m:).
    let mc_all = {
        let mut ps: Vec<String> = Vec::new();
        if !mc.trim().is_empty() {
            ps.push(mc.clone());
        }
        if let Some(faces) = v.get("card_faces").and_then(|x| x.as_array()) {
            for f in faces {
                if let Some(t) = f.get("mana_cost").and_then(|x| x.as_str()) {
                    if !t.trim().is_empty() {
                        ps.push(t.to_string());
                    }
                }
            }
        }
        ps.join(" /// ")
    };
    Some(RawC {
        id: str_of(&v, "id").to_string(),
        oracle_id: str_of(&v, "oracle_id").to_string(),
        name: str_of(&v, "name").to_string(),
        set: str_of(&v, "set").to_string(),
        set_name: str_of(&v, "set_name").to_string(),
        set_type: str_of(&v, "set_type").to_string(),
        collector_number: str_of(&v, "collector_number").to_string(),
        cn_key,
        type_line,
        type_tokens,
        mana_cost: mc.clone(),
        mana_cost_all: mc_all.clone(),
        oracle_text,
        artist: str_of(&v, "artist").to_string(),
        keywords_joined,
        layout,
        img_small,
        img_normal: img_normal.clone(),
        uri: str_of(&v, "scryfall_uri").to_string(),
        colors,
        ci,
        face_pack,
        rarity: rarity_rank(str_of(&v, "rarity")),
        cmc: v.get("cmc").map(num_of).unwrap_or(f32::NAN),
        power: pt_num(v.get("power")),
        toughness: pt_num(v.get("toughness")),
        loyalty: pt_num(v.get("loyalty")),
        def: v
            .get("defense")
            .map(num_of)
            .or_else(|| {
                v.get("card_faces")
                    .and_then(|x| x.as_array())
                    .and_then(|a| a.first())
                    .and_then(|fc| fc.get("defense"))
                    .map(num_of)
            })
            .unwrap_or(f32::NAN),
        legal,
        flags,
        flags2,
        reprint,
        atypical,
        masterpiece,
        face_pow: fnums.0,
        face_tou: fnums.1,
        face_loy: fnums.2,
        face_def: fnums.3,
        cn_val: parse_cn(str_of(&v, "collector_number")),
        released: parse_released(str_of(&v, "released_at")),
        prices,
        edhrec,
        lang,
        flavor,
        watermark,
        illustration: if illustration.is_empty() {
            img_normal.clone()
        } else {
            illustration
        },
        ptypes,
        stamp,
        border,
        frame,
        games,
        finishes,
        produced,
        ffx,
    })
}

/// Split on non-ascii-alphanumeric bytes (input assumed lowercased).
pub fn for_each_token<'a, F>(s: &'a str, min: usize, f: &mut F)
where
    F: FnMut(&'a str),
{
    let b = s.as_bytes();
    let mut start: Option<usize> = None;
    for i in 0..b.len() {
        if b[i].is_ascii_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(st) = start {
            if i - st >= min {
                f(&s[st..i]);
            }
            start = None;
        }
    }
    if let Some(st) = start {
        if b.len() - st >= min {
            f(&s[st..]);
        }
    }
}

pub fn tokenize(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for_each_token(s, 2, &mut |t| out.push(t));
    out
}

fn push_json_str(out: &mut String, s: &str) {
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
}

fn fmt_f32(v: f32) -> String {
    if v == v.floor() {
        format!("{}", v as i64)
    } else {
        format!("{}", v)
    }
}

impl Index {
    pub fn load(path: &Path) -> Index {
        let t0 = Instant::now();
        let file = File::open(path).expect("cannot open cards jsonl.gz");
        let dec = GzDecoder::new(file);
        let mut reader = BufReader::with_capacity(1 << 20, dec);

        // 1. parallel parse of gz lines into owned intermediates
        let mut raws: Vec<RawC> = Vec::with_capacity(120_000);
        let mut source_bytes = 0u64;
        loop {
            let mut chunk: Vec<String> = Vec::with_capacity(2048);
            let mut eof = false;
            for _ in 0..2048 {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(n) => {
                        source_bytes += n as u64;
                        let t = line.trim_end();
                        if !t.is_empty() {
                            chunk.push(line);
                        }
                    }
                    Err(e) => panic!("read error: {e}"),
                }
            }
            let parsed: Vec<RawC> = chunk.par_iter().filter_map(|l| parse_line(l)).collect();
            raws.extend(parsed);
            if eof {
                break;
            }
        }
        let parse_ms = t0.elapsed().as_millis();

        // 2. intern strings, build cards + lc mirrors + pre-serialized JSON
        let mut ix = Index {
            cards: Vec::with_capacity(raws.len()),
            strings: Vec::with_capacity(600_000),
            jsons: Vec::with_capacity(raws.len()),
            id_map: HashMap::with_capacity(raws.len()),
            name_lc: Vec::with_capacity(raws.len()),
            type_lc: Vec::with_capacity(raws.len()),
            oracle_lc: Vec::with_capacity(raws.len()),
            otext_lc: Vec::with_capacity(raws.len()),
            cost_all: Vec::with_capacity(raws.len()),
            artist_lc: Vec::with_capacity(raws.len()),
            set_name_lc: Vec::with_capacity(raws.len()),
            mana_norm: Vec::with_capacity(raws.len()),
            pip: Vec::with_capacity(raws.len()),
            name_tokens: HashMap::new(),
            type_tokens: HashMap::new(),
            text_tokens: HashMap::new(),
            otext_tokens: HashMap::new(),
            keyword_tokens: HashMap::new(),
            keyword_phrase: HashMap::new(),
            kw_enum: std::collections::HashSet::new(),
            prefix: Vec::with_capacity(raws.len()),
            color_bits: Default::default(),
            color_incl: Default::default(),
            ci_bits: Default::default(),
            subsets: Default::default(),
            exacts: Default::default(),
            ci_subsets: Default::default(),
            ci_exacts: Default::default(),
            rarity_idx: Default::default(),
            set_idx: HashMap::new(),
            layout_idx: HashMap::new(),
            name_tok_list: Vec::new(),
            name_tok_buckets: HashMap::new(),
            type_tok_list: Vec::new(),
            type_tok_buckets: HashMap::new(),
            format_legal: Default::default(),
            format_banned: Default::default(),
            format_restricted: Default::default(),
            oracle_stats: Vec::new(),
            oracle_prints: Vec::new(),
            in_sets: HashMap::new(),
            in_set_types: HashMap::new(),
            in_games: HashMap::new(),
            in_langs: HashMap::new(),
            oracle_lang_idx: HashMap::new(),
            in_rarities: HashMap::new(),
            settype_idx: HashMap::new(),
            border_idx: HashMap::new(),
            frame_idx: HashMap::new(),
            games_idx: HashMap::new(),
            stamp_idx: HashMap::new(),
            ffx_idx: HashMap::new(),
            ptype_idx: HashMap::new(),
            lang_idx: HashMap::new(),
            block_idx: HashMap::new(),
            block_setname: HashMap::new(),
            group_idx: HashMap::new(),
            land_cycles: HashMap::new(),
            today: {
                let d = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() / 86_400)
                    .unwrap_or(20_000);
                // civil-from-days (Howard Hinnant) -> YYYYMMDD
                let z = d as i64 + 719_468;
                let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
                let doe = z - era * 146_097;
                let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
                let y = yoe + era * 400;
                let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
                let mp = (5 * doy + 2) / 153;
                let m = if mp < 10 { mp + 3 } else { mp - 9 };
                let y = if m <= 2 { y + 1 } else { y };
                let day = doy - (153 * mp + 2) / 5 + 1;
                (y as u32) * 10_000 + m as u32 * 100 + day as u32
            },
            card_oracle: Vec::with_capacity(raws.len()),
            oracle_id_off: Vec::with_capacity(40_000),
            oracle_vis: Vec::with_capacity(40_000),
            oracle_ci: Vec::new(),
            oracle_id_vis: Vec::new(),
            id_vis_bits: RoaringBitmap::new(),
            oracle_intern: HashMap::with_capacity(40_000),
            side: Default::default(),
            otags: Vec::new(),
            otag_slug: HashMap::new(),
            otag_alias: HashMap::new(),
            otag_ready: false,
            orders: Orders {
                name: vec![],
                cmc: vec![],
                rarity: vec![],
                set: vec![],
                released: vec![],
            },
            sets: Vec::new(),
            set_detail_json: HashMap::new(),
            universe: RoaringBitmap::new(),
            build_ms: 0,
            source_bytes,
        };
        let mut intern: HashMap<Box<str>, u32> = HashMap::with_capacity(300_000);

        for r in &raws {
            let mut s = |v: &str| -> u32 {
                if let Some(&o) = intern.get(v) {
                    return o;
                }
                let b: Box<str> = v.into();
                let off = ix.strings.len() as u32;
                ix.strings.push(b.clone());
                intern.insert(b, off);
                off
            };
            let c = Card {
                id: s(&r.id),
                oracle_id: s(&r.oracle_id),
                name: s(&r.name),
                set: s(&r.set),
                set_name: s(&r.set_name),
                set_type: s(&r.set_type),
                collector_number: s(&r.collector_number),
                cn_key: s(&r.cn_key),
                type_line: s(&r.type_line),
                mana_cost: s(&r.mana_cost),
                oracle_text: s(&r.oracle_text),
                artist: s(&r.artist),
                keywords: s(&r.keywords_joined),
                layout: s(&r.layout),
                image_small: s(&r.img_small),
                image_normal: s(&r.img_normal),
                scryfall_uri: s(&r.uri),
                cmc: r.cmc,
                power: r.power,
                toughness: r.toughness,
                loyalty: r.loyalty,
                defense: r.def,
                colors: r.colors,
                ci: r.ci,
                face_pack: r.face_pack,
                rarity: r.rarity,
                set_type_code: set_type_code(&r.set_type),
                legal: r.legal,
                flags: r.flags,
                flags2: r.flags2,
                reprint: r.reprint,
                atypical: r.atypical,
                masterpiece: r.masterpiece,
                face_pow: r.face_pow,
                face_tou: r.face_tou,
                face_loy: r.face_loy,
                face_def: r.face_def,
                cn_val: r.cn_val,
                released: r.released,
                prices: r.prices,
                edhrec: r.edhrec,
                lang: s(&r.lang),
                flavor: s(&r.flavor),
                watermark: s(&r.watermark),
                illustration: s(&r.illustration),
                ptypes: s(&r.ptypes),
                stamp: r.stamp,
                border: r.border,
                frame: r.frame,
                games: r.games,
                finishes: r.finishes,
                produced: r.produced,
                ffx: r.ffx,
                new_flags: 0,
            };
            let idx = ix.cards.len() as u32;
            ix.cards.push(c);
            ix.id_map.insert(r.id.as_str().into(), idx);

            ix.name_lc.push(s(&r.name.to_lowercase()));
            ix.type_lc.push(s(&r.type_line.to_lowercase()));
            ix.oracle_lc.push(s(&r.oracle_text.to_lowercase()));
            ix.otext_lc
                .push(s(&crate::index::strip_paren_text(&r.oracle_text).to_lowercase()));
            ix.cost_all.push(s(&r.mana_cost_all));
            ix.artist_lc.push(s(&r.artist.to_lowercase()));
            ix.set_name_lc.push(s(&r.set_name.to_lowercase()));
            ix.mana_norm.push(s(
                &r.mana_cost.replace(['{', '}'], "").to_lowercase(),
            ));
            ix.pip.push(Self::pip_pack(&r.mana_cost));

            // pre-serialized compact JSON (served verbatim)
            let mut j = String::with_capacity(1024);
            j.push_str("{\"id\":");
            push_json_str(&mut j, &r.id);
            j.push_str(",\"oracle_id\":");
            push_json_str(&mut j, &r.oracle_id);
            j.push_str(",\"name\":");
            push_json_str(&mut j, &r.name);
            j.push_str(",\"set\":");
            push_json_str(&mut j, &r.set);
            j.push_str(",\"set_name\":");
            push_json_str(&mut j, &r.set_name);
            j.push_str(",\"collector_number\":");
            push_json_str(&mut j, &r.collector_number);
            j.push_str(",\"type_line\":");
            push_json_str(&mut j, &r.type_line);
            j.push_str(",\"mana_cost\":");
            push_json_str(&mut j, &r.mana_cost);
            j.push_str(",\"cmc\":");
            j.push_str(&if r.cmc.is_nan() { "null".into() } else { fmt_f32(r.cmc) });
            j.push_str(",\"colors\":[");
            for (i, letter) in ['W', 'U', 'B', 'R', 'G'].iter().enumerate() {
                if r.colors & (1 << i) != 0 {
                    if j.as_bytes()[j.len() - 1] != b'[' {
                        j.push(',');
                    }
                    j.push('"');
                    j.push(*letter);
                    j.push('"');
                }
            }
            j.push_str("],\"color_identity\":[");
            for (i, letter) in ['W', 'U', 'B', 'R', 'G'].iter().enumerate() {
                if r.ci & (1 << i) != 0 {
                    if j.as_bytes()[j.len() - 1] != b'[' {
                        j.push(',');
                    }
                    j.push('"');
                    j.push(*letter);
                    j.push('"');
                }
            }
            j.push_str("],\"rarity\":");
            push_json_str(&mut j, str_of_rarity(r.rarity));
            j.push_str(",\"oracle_text\":");
            push_json_str(&mut j, &r.oracle_text);
            j.push_str(",\"keywords\":[");
            let mut first = true;
            for kw in r.keywords_joined.split(' ') {
                if !kw.is_empty() {
                    if !first {
                        j.push(',');
                    }
                    first = false;
                    push_json_str(&mut j, kw);
                }
            }
            j.push_str("],\"power\":");
            j.push_str(&if r.power.is_nan() { "null".into() } else if r.power == STAR { "\"*\"".into() } else { format!("\"{}\"", fmt_f32(r.power)) });
            j.push_str(",\"toughness\":");
            j.push_str(&if r.toughness.is_nan() { "null".into() } else if r.toughness == STAR { "\"*\"".into() } else { format!("\"{}\"", fmt_f32(r.toughness)) });
            j.push_str(",\"loyalty\":");
            j.push_str(&if r.loyalty.is_nan() { "null".into() } else if r.loyalty == STAR { "\"*\"".into() } else { fmt_f32(r.loyalty) });
            j.push_str(",\"layout\":");
            push_json_str(&mut j, &r.layout);
            j.push_str(",\"image_uris\":{\"small\":");
            push_json_str(&mut j, &r.img_small);
            j.push_str(",\"normal\":");
            push_json_str(&mut j, &r.img_normal);
            j.push_str("}");
            // flat fields the frontend reads directly
            j.push_str(",\"image_url\":");
            push_json_str(&mut j, &r.img_normal);
            j.push_str(",\"legal\":\"");
            let mut first = true;
            for (fi, f) in FORMATS.iter().enumerate() {
                if ((r.legal >> (fi as u32 * 2)) & 3) as u32 == L_LEGAL {
                    if !first {
                        j.push(',');
                    }
                    first = false;
                    j.push_str(f);
                }
            }
            j.push('"');
            j.push_str(",\"scryfall_uri\":");
            push_json_str(&mut j, &r.uri);
            j.push_str(",\"artist\":");
            push_json_str(&mut j, &r.artist);
            j.push_str(",\"released_at\":");
            j.push_str(&if r.released == 0 {
                "null".to_string()
            } else {
                let d = r.released.to_string();
                format!("\"{}-{}-{}\"", &d[0..4], &d[4..6], &d[6..8])
            });
            j.push_str(",\"legalities\":{");
            for (fi, f) in FORMATS.iter().enumerate() {
                if fi > 0 {
                    j.push(',');
                }
                push_json_str(&mut j, f);
                j.push(':');
                match ((r.legal >> (fi as u32 * 2)) & 3) as u32 {
                    L_LEGAL => j.push_str("\"legal\""),
                    L_BANNED => j.push_str("\"banned\""),
                    L_RESTRICTED => j.push_str("\"restricted\""),
                    _ => j.push_str("\"not_legal\""),
                }
            }
            j.push_str("}}");
            ix.jsons.push(j.into_boxed_str());
        }
        let intern_ms = t0.elapsed().as_millis();
        drop(intern);
        let raws = raws; // keep for token pass below

        // 2.5 oracle table: intern oracle_ids to dense rows, compute default-search
        // visibility (per the calibrated rule — each condition checked INDEPENDENTLY
        // across the oracle's printings), and map each card to its oracle row
        // (otag: filtering + oracle-level dedup).
        // Snapshot the string offsets first so field-level borrows stay disjoint.
        let card_keys: Vec<(u32, u32, u32, u32)> = ix
            .cards
            .iter()
            .map(|c| (c.oracle_id, c.set_type, c.layout, c.set))
            .collect();
        // flags bits: 1=has printing w/ good set_type, 2=has good layout, 4=has good set code
        let mut oracle_flags: Vec<u8> = Vec::with_capacity(ix.oracle_vis.len());
        for (oid, st, lay, set) in card_keys {
            let row = match ix.oracle_intern.get(&*ix.strings[oid as usize]) {
                Some(&r) => r,
                None => {
                    let r = ix.oracle_id_off.len() as u32;
                    ix.oracle_id_off.push(oid);
                    ix.oracle_intern.insert(ix.strings[oid as usize].clone(), r);
                    oracle_flags.push(0);
                    r
                }
            };
            let f = &mut oracle_flags[row as usize];
            if *f & 1 == 0 && otag_good_set_type(&ix.strings[st as usize]) {
                *f |= 1;
            }
            if *f & 2 == 0 && otag_good_layout(&ix.strings[lay as usize]) {
                *f |= 2;
            }
            if *f & 4 == 0 && otag_good_set_code(&ix.strings[set as usize]) {
                *f |= 4;
            }
            // bits 8/16: id:/unique-pool visibility (independent conditions,
            // same shape as otag but with the ID_EXTRA_HIDDEN_* lists added)
            if *f & 8 == 0 && id_good_set_type(&ix.strings[st as usize]) {
                *f |= 8;
            }
            if *f & 16 == 0 && id_good_layout(&ix.strings[lay as usize]) {
                *f |= 16;
            }
            ix.card_oracle.push(row);
        }
        ix.oracle_vis = oracle_flags.iter().map(|f| f & 7 == 7).collect();
        for b in OTAG_BLOCKED_ORACLES {
            if let Some(&r) = ix.oracle_intern.get(*b) {
                ix.oracle_vis[r as usize] = false;
            }
        }
        // id:/unique-card pool: oracle-level color_identity union (id: matches
        // the whole oracle, not one printing's face) + extended visibility.
        ix.oracle_ci = vec![0u8; ix.oracle_id_off.len()];
        for (i, &row) in ix.card_oracle.iter().enumerate() {
            ix.oracle_ci[row as usize] |= ix.cards[i].ci;
        }
        ix.oracle_id_vis = oracle_flags.iter().map(|f| f & 7 == 7 && f & 24 == 24).collect();
        for b in ID_BLOCKED_ORACLES {
            if let Some(&r) = ix.oracle_intern.get(*b) {
                ix.oracle_id_vis[r as usize] = false;
            }
        }
        let mut idb = RoaringBitmap::new();
        for (i, &row) in ix.card_oracle.iter().enumerate() {
            if ix.oracle_id_vis[row as usize] {
                idb.insert(i as u32);
            }
        }
        ix.id_vis_bits = idb;

        // 2.6 oracle aggregates + "ever printed in" oracle-row indexes (in:,
        // prints=, sets=, artists>, new:…) — all built once at load.
        {
            let n_or = ix.oracle_id_off.len();
            ix.oracle_prints = vec![RoaringBitmap::new(); n_or];
            ix.oracle_stats = vec![OracleStats::default(); n_or];
            let mut set_seen: Vec<std::collections::HashSet<u32>> =
                vec![Default::default(); n_or];
            let mut pset_seen: Vec<std::collections::HashSet<u32>> =
                vec![Default::default(); n_or];
            let mut artist_seen: Vec<std::collections::HashSet<u32>> =
                vec![Default::default(); n_or];
            let mut illus_seen: Vec<std::collections::HashSet<u32>> =
                vec![Default::default(); n_or];
            // parity w5 (borrow-plan §8): prints:N counts ALL bulk rows per
            // oracle (Scryfall's prints_search_uri size incl. foil twins and
            // promo/variant prints — gated/(set,cn) dedup over-counted
            // prints=1 by ~260 oracles: 13,579 vs 13,395 golden). Paper
            // prints keep the (set, cn_key) twin-merge below.
            let mut pprint_seen: Vec<std::collections::HashSet<(u32, u32)>> =
                vec![Default::default(); n_or];
            let mut tok_cnt: Vec<u32> = vec![0; n_or];
            let mut vis: Vec<bool> = vec![false; n_or];
            for (i, c) in ix.cards.iter().enumerate() {
                let r = ix.card_oracle[i] as usize;
                let i32 = i as u32;
                ix.oracle_prints[r].insert(i32);
                let st = &mut ix.oracle_stats[r];
                if !vis[r]
                    && c.flags2 & (F2_TOKEN | F2_HIDDEN) == 0
                    && !is_aux_layout(&ix.strings[c.layout as usize])
                    && !is_aux_set_type(&ix.strings[c.set_type as usize])
                {
                    vis[r] = true;
                }
                st.n_prints = st.n_prints.saturating_add(1);
                if c.flags2 & F2_TOKEN == 0 {
                    if c.reprint {
                        st.reprint = true;
                    } else {
                        st.nrp = true;
                    }
                } else {
                    tok_cnt[r] += 1;
                }
                if &*ix.strings[c.layout as usize] == "art_series" {
                    st.has_artseries = true;
                }
                st.games |= c.games;
                if c.games & G_PAPER != 0 {
                    if pprint_seen[r].insert((c.set, c.cn_key)) {
                        st.n_paper = st.n_paper.saturating_add(1);
                    }
                    pset_seen[r].insert(c.set);
                }
                set_seen[r].insert(c.set);
                artist_seen[r].insert(ix.artist_lc[i]);
                illus_seen[r].insert(c.illustration);
                // oracle-row "ever in" indexes
                ix.in_sets
                    .entry(ix.strings[c.set as usize].clone())
                    .or_default()
                    .insert(r as u32);
                ix.in_set_types
                    .entry(ix.strings[c.set_type as usize].clone())
                    .or_default()
                    .insert(r as u32);
                ix.in_langs
                    .entry(ix.strings[c.lang as usize].clone())
                    .or_default()
                    .insert(r as u32);
                ix.in_rarities
                    .entry(str_of_rarity(c.rarity).into())
                    .or_default()
                    .insert(r as u32);
                for (bit, name) in [
                    (G_PAPER, "paper"),
                    (G_MTGO, "mtgo"),
                    (G_ARENA, "arena"),
                ] {
                    if c.games & bit != 0 {
                        ix.in_games
                            .entry(name.into())
                            .or_default()
                            .insert(r as u32);
                    }
                }
            }
            for r in 0..n_or {
                let st = &mut ix.oracle_stats[r];
                st.n_sets = set_seen[r].len() as u16;
                st.n_paper_sets = pset_seen[r].len() as u16;
                st.n_artists = artist_seen[r].len() as u16;
                st.n_illus = illus_seen[r].len() as u16;
                st.tok_oracle = tok_cnt[r] as u64 == ix.oracle_prints[r].len();
                st.visible = vis[r];
            }

            // new: flags — earliest print wins each (artist, illustration,
            // flavor, frame, rarity, language); ties broken by file order.
            for r in 0..n_or {
                let mut rows: Vec<u32> = ix.oracle_prints[r].iter().collect();
                if rows.len() <= 1 {
                    if let Some(&i) = rows.first() {
                        if ix.cards[i as usize].flags & F_PREVIEW != 0 {
                            ix.cards[i as usize].new_flags |= NEW_PREVIEW;
                        }
                    }
                    continue;
                }
                rows.sort_unstable_by_key(|&i| (ix.cards[i as usize].released, i));
                let mut seen_artist: std::collections::HashSet<u32> = Default::default();
                let mut seen_illus: std::collections::HashSet<u32> = Default::default();
                let mut seen_flavor: std::collections::HashSet<u32> = Default::default();
                let mut seen_frame: std::collections::HashSet<u8> = Default::default();
                let mut seen_rarity: std::collections::HashSet<u8> = Default::default();
                let mut seen_lang: std::collections::HashSet<u32> = Default::default();
                for i in rows {
                    let c = &mut ix.cards[i as usize];
                    let mut nf = 0u8;
                    if seen_artist.insert(ix.artist_lc[i as usize]) {
                        nf |= NEW_ARTIST;
                    }
                    if seen_illus.insert(c.illustration) {
                        nf |= NEW_ART;
                    }
                    if seen_flavor.insert(c.flavor) {
                        nf |= NEW_FLAVOR;
                    }
                    if seen_frame.insert(c.frame) {
                        nf |= NEW_FRAME;
                    }
                    if seen_rarity.insert(c.rarity) {
                        nf |= NEW_RARITY;
                    }
                    if seen_lang.insert(c.lang) {
                        nf |= NEW_LANGUAGE;
                    }
                    if c.flags & F_PREVIEW != 0 {
                        nf |= NEW_PREVIEW;
                    }
                    c.new_flags = nf;
                }
            }
        }

        // 3. token inverted indexes (parallel, per-thread maps merged)
        let merge = |mut a: HashMap<String, RoaringBitmap>, b: HashMap<String, RoaringBitmap>| {
            for (k, v) in b {
                a.entry(k).and_modify(|e| *e |= &v).or_insert(v);
            }
            a
        };
        let mk = |get: fn(&RawC) -> String| -> HashMap<String, RoaringBitmap> {
            raws.par_iter()
                .enumerate()
                .fold(HashMap::<String, RoaringBitmap>::new, |mut m, (i, r)| {
                    let lc = get(r).to_lowercase();
                    for_each_token(&lc, 2, &mut |t| {
                        m.entry(t.to_string()).or_default().insert(i as u32);
                    });
                    m
                })
                .reduce(HashMap::<String, RoaringBitmap>::new, merge)
        };
        ix.name_tokens = raws
            .par_iter()
            .enumerate()
            .fold(HashMap::<String, RoaringBitmap>::new, |mut m, (i, r)| {
                let lc = r.name.to_lowercase();
                for_each_token(&lc, 1, &mut |t| {
                    m.entry(t.to_string()).or_default().insert(i as u32);
                });
                m
            })
            .reduce(HashMap::new, merge);
        ix.text_tokens = mk(|r| format!("{} {}", r.type_line, r.oracle_text));
        ix.otext_tokens = raws
            .par_iter()
            .enumerate()
            .fold(HashMap::<String, RoaringBitmap>::new, |mut m, (i, r)| {
                let stripped = strip_paren_text(&r.oracle_text);
                let lc = stripped.to_lowercase();
                let bit = i as u32;
                for_each_token(&lc, 1, &mut |t| {
                    m.entry(t.to_string()).or_default().insert(bit);
                    if t.len() > 4 {
                        // light stemming: draws/drawn/drawing/… → draw
                        if let Some(b) = t.strip_suffix("es") {
                            m.entry(b.to_string()).or_default().insert(bit);
                            m.entry(format!("{b}e")).or_default().insert(bit);
                        } else if let Some(b) = t.strip_suffix('s') {
                            m.entry(b.to_string()).or_default().insert(bit);
                        }
                        if let Some(b) = t.strip_suffix("ing") {
                            m.entry(b.to_string()).or_default().insert(bit);
                            m.entry(format!("{b}e")).or_default().insert(bit);
                        }
                        if let Some(b) = t.strip_suffix("ed") {
                            m.entry(b.to_string()).or_default().insert(bit);
                            m.entry(format!("{b}e")).or_default().insert(bit);
                        }
                        if let Some(b) = t.strip_suffix("wn") {
                            m.entry(b.to_string()).or_default().insert(bit);
                        }
                    }
                    // compound keyword: landcycling/vaultcycling ⇒ cycling
                    if t.len() > 7 && t.ends_with("cycling") {
                        m.entry("cycling".to_string()).or_default().insert(bit);
                    }
                });
                m
            })
            .reduce(HashMap::new, merge);
        ix.type_tokens = mk(|r| r.type_line.clone());
        ix.keyword_tokens = mk(|r| r.keywords_joined.clone());
        // exact keyword-phrase index: gate to prints that actually count for
        // keyword matching upstream (non-token, non-hidden, non-aux print).
        ix.keyword_phrase = (0..raws.len())
            .into_par_iter()
            .fold(HashMap::<Box<str>, RoaringBitmap>::new, |mut m, i| {
                let c = &ix.cards[i];
                if c.flags2 & (F2_TOKEN | F2_HIDDEN) != 0 {
                    return m;
                }
                if is_aux_layout(&ix.strings[c.layout as usize])
                    || is_aux_set_type(&ix.strings[c.set_type as usize])
                {
                    return m;
                }
                for kw in ix.strings[c.keywords as usize].split('|') {
                    if kw.is_empty() {
                        continue;
                    }
                    m.entry(kw.to_lowercase().into_boxed_str())
                        .or_default()
                        .insert(i as u32);
                }
                m
            })
            .reduce(HashMap::new, |mut a, b| {
                for (k, v) in b {
                    *a.entry(k).or_default() |= &v;
                }
                a
            });
        ix.kw_enum = ix.keyword_phrase.keys().map(|k| k.to_string()).collect();

        // prefix table sorted by lowercased name
        let n = ix.cards.len();
        ix.prefix = (0..n as u32)
            .map(|i| (ix.name_lc[i as usize], i))
            .collect();
        ix.prefix.sort_unstable_by_key(|&(off, _)| &ix.strings[off as usize]);

        // fuzzy word-scan: token lists + first-2-byte candidate buckets.
        // Tokens shorter than 3 bytes can never sit within 2 edits of a >=3-char
        // query, so they are left out of the pools.
        {
            let mut bucket = |map: &HashMap<String, RoaringBitmap>,
                              list: &mut Vec<Box<str>>,
                              buckets: &mut HashMap<u16, Vec<u32>>| {
                for tok in map.keys() {
                    let b = tok.as_bytes();
                    if b.len() < 3 {
                        continue;
                    }
                    let id = list.len() as u32;
                    list.push(tok.as_str().into());
                    buckets
                        .entry(((b[0] as u16) << 8) | (b[1] as u16))
                        .or_default()
                        .push(id);
                }
            };
            bucket(
                &ix.name_tokens,
                &mut ix.name_tok_list,
                &mut ix.name_tok_buckets,
            );
            bucket(
                &ix.type_tokens,
                &mut ix.type_tok_list,
                &mut ix.type_tok_buckets,
            );
        }

        // 4. bitmaps
        for (i, c) in ix.cards.iter().enumerate() {
            let i = i as u32;
            ix.universe.insert(i);
            for b in 0..5 {
                if c.colors & (1 << b) != 0 {
                    ix.color_bits[b as usize].insert(i);
                }
                if c.ci & (1 << b) != 0 {
                    ix.ci_bits[b as usize].insert(i);
                }
            }
            for m in 0..32u8 {
                if c.colors & !m == 0 {
                    ix.subsets[m as usize].insert(i);
                }
                if c.ci & !m == 0 {
                    ix.ci_subsets[m as usize].insert(i);
                }
            }
            ix.exacts[c.colors as usize].insert(i);
            ix.ci_exacts[c.ci as usize].insert(i);
            // any-face superset buckets: for each distinct face mask, the
            // print belongs to every sub-mask's include bucket.
            {
                let fp = c.face_pack;
                let mut seen: [u8; 8] = [0xFFu8; 8];
                let mut cnt = 0usize;
                for k in 0..8u32 {
                    let m = ((fp >> (5 * k)) & 31) as u8;
                    if seen[..cnt].contains(&m) {
                        continue;
                    }
                    seen[cnt] = m;
                    cnt += 1;
                    let mut s = m;
                    loop {
                        ix.color_incl[s as usize].insert(i);
                        if s == 0 {
                            break;
                        }
                        s = (s - 1) & m;
                    }
                }
            }
            ix.rarity_idx[c.rarity as usize].insert(i);
            ix.set_idx
                .entry(ix.strings[c.set as usize].to_string().into_boxed_str())
                .or_default()
                .insert(i);
            ix.layout_idx
                .entry(ix.strings[c.layout as usize].clone())
                .or_default()
                .insert(i);
            ix.settype_idx
                .entry(ix.strings[c.set_type as usize].clone())
                .or_default()
                .insert(i);
            ix.border_idx
                .entry(BORDER_NAMES[c.border as usize].into())
                .or_default()
                .insert(i);
            ix.frame_idx
                .entry(FRAME_NAMES[c.frame as usize].into())
                .or_default()
                .insert(i);
            for (bit, name) in [(G_PAPER, "paper"), (G_MTGO, "mtgo"), (G_ARENA, "arena")] {
                if c.games & bit != 0 {
                    ix.games_idx.entry(name.into()).or_default().insert(i);
                }
            }
            if c.stamp != ST_NONE {
                ix.stamp_idx
                    .entry(STAMP_NAMES[c.stamp as usize].into())
                    .or_default()
                    .insert(i);
            }
            if c.ffx != 0 {
                for (p, name) in FFX_LIST.iter().enumerate() {
                    if c.ffx & (1 << p) != 0 {
                        ix.ffx_idx.entry((*name).into()).or_default().insert(i);
                    }
                }
            }
            {
                let pt = &ix.strings[c.ptypes as usize];
                if !pt.is_empty() {
                    for t in pt.split(',') {
                        if !t.is_empty() {
                            ix.ptype_idx.entry(t.into()).or_default().insert(i);
                        }
                    }
                }
                ix.lang_idx
                    .entry(ix.strings[c.lang as usize].clone())
                    .or_default()
                    .insert(i);
            }
            for fi in 0..NFORMATS {
                match ((c.legal >> (fi as u32 * 2)) & 3) as u32 {
                    L_LEGAL => {
                        ix.format_legal[fi].insert(i);
                    }
                    L_BANNED => {
                        ix.format_banned[fi].insert(i);
                    }
                    L_RESTRICTED => {
                        ix.format_restricted[fi].insert(i);
                    }
                    _ => {}
                }
            }
        }

        // 5. sort orders
        let mut by_name: Vec<u32> = (0..n as u32).collect();
        by_name.sort_unstable_by_key(|&i| {
            let c = &ix.cards[i as usize];
            (
                ix.strings[c.name as usize].to_lowercase(),
                ix.strings[c.set as usize].to_string(),
                c.cn_val,
            )
        });
        let mut by_cmc: Vec<u32> = (0..n as u32).collect();
        by_cmc.sort_unstable_by(|&a, &b| {
            let (x, y) = (ix.cards[a as usize].cmc, ix.cards[b as usize].cmc);
            x.partial_cmp(&y)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| ix.strings[ix.cards[a as usize].name as usize]
                    .cmp(&ix.strings[ix.cards[b as usize].name as usize]))
        });
        let mut by_rarity: Vec<u32> = (0..n as u32).collect();
        // Scryfall "rarity" sort: mythic -> rare -> uncommon -> common
        by_rarity.sort_unstable_by_key(|&i| {
            let c = &ix.cards[i as usize];
            (255u16 - c.rarity as u16, &*ix.strings[c.set as usize], c.cn_val)
        });
        let mut by_set: Vec<u32> = (0..n as u32).collect();
        by_set.sort_unstable_by_key(|&i| {
            let c = &ix.cards[i as usize];
            (&*ix.strings[c.set as usize], c.cn_val)
        });
        let mut by_released: Vec<u32> = (0..n as u32).collect();
        by_released.sort_unstable_by(|&a, &b| {
            ix.cards[b as usize]
                .released
                .cmp(&ix.cards[a as usize].released)
                .then_with(|| ix.strings[ix.cards[a as usize].name as usize].cmp(&ix.strings[ix.cards[b as usize].name as usize]))
        });
        ix.orders = Orders {
            name: by_name,
            cmc: by_cmc,
            rarity: by_rarity,
            set: by_set,
            released: by_released,
        };
        let _ = intern_ms;

        // 7. per-set aggregation for /api/sets — computed ONCE at startup, cached.
        // Scryfall's /sets default view includes token sets (its type dropdown lists
        // Token and /sets?type=token is a subset of the 1072 shown), while asset /
        // stock / gallery sets are hidden entirely. Mirror that here.
        {
            let mut map: HashMap<u32, usize> = HashMap::with_capacity(1200);
            let mut sets: Vec<SetEntry> = Vec::with_capacity(1200);
            for c in ix.cards.iter() {
                if ix.strings[c.set as usize].is_empty() {
                    continue;
                }
                if matches!(&*ix.strings[c.set_type as usize], "asset" | "stock" | "gallery") {
                    continue;
                }
                let idx = match map.get(&c.set) {
                    Some(&i) => i,
                    None => {
                        sets.push(SetEntry {
                            code: c.set,
                            name: c.set_name,
                            set_type: c.set_type,
                            released: c.released,
                            card_count: 0,
                            sample_image: u32::MAX,
                            best_cn: u32::MAX,
                            block: u32::MAX,
                            block_rank: u32::MAX,
                            parent_set: u32::MAX,
                            icon: u32::MAX,
                            printed_size: u32::MAX,
                            digital: false,
                            foil_only: false,
                            nonfoil_only: false,
                            n_creatures: 0,
                            n_noncreatures: 0,
                            n_lands: 0,
                        });
                        let i = sets.len() - 1;
                        map.insert(c.set, i);
                        i
                    }
                };
                let e = &mut sets[idx];
                e.card_count += 1;
                // type breakdown over printings: Creature wins over Land
                // (e.g. Crevasse Creature), matching how Scryfall's creature:
                // operator treats the super types.
                let tl = &ix.strings[c.type_line as usize];
                let tl_main = tl.split("//").next().unwrap_or(tl);
                if tl_main.contains("Creature") {
                    e.n_creatures += 1;
                } else if tl_main.contains("Land") {
                    e.n_lands += 1;
                } else {
                    e.n_noncreatures += 1;
                }
                if c.released > e.released {
                    e.released = c.released;
                }
                // sample tile art: the lowest-collector-number card that has an image
                if c.image_small != u32::MAX
                    && !ix.strings[c.image_small as usize].is_empty()
                    && c.cn_val < e.best_cn
                {
                    e.best_cn = c.cn_val;
                    e.sample_image = c.image_small;
                }
            }
            sets.sort_unstable_by(|a, b| {
                b.released.cmp(&a.released).then_with(|| {
                    ix.strings[a.name as usize].cmp(&ix.strings[b.name as usize])
                })
            });
            ix.sets = sets;
        }

        // 6. universe complement helper is caller's job; store build time
        ix.build_ms = t0.elapsed().as_millis() - parse_ms;
        eprintln!(
            "index: {} cards | parse {}ms | build {}ms | strings {} | ~{} MB",
            ix.cards.len(),
            parse_ms,
            ix.build_ms,
            ix.strings.len(),
            ix.estimate_mb()
        );
        ix
    }

    pub fn estimate_mb(&self) -> usize {
        let jsons: usize = self.jsons.iter().map(|j| j.len() + 16).sum();
        let strs: usize = self.strings.iter().map(|s| s.len() + 24).sum();
        let toks: usize = self
            .name_tokens
            .iter()
            .chain(self.text_tokens.iter())
            .chain(self.type_tokens.iter())
            .chain(self.keyword_tokens.iter())
            .map(|(k, v)| k.len() + 48 + v.serialized_size())
            .sum();
        (jsons + strs + toks + std::mem::size_of::<Card>() * self.cards.len()) / (1 << 20)
    }

    /// Wave-5 item 3: load the language side-table
    /// ({oracle_id: [lang codes...]} gzipped JSON, from the FULL
    /// all-cards bulk — default-cards is English-slimmed) and build
    /// oracle_lang_idx: lang -> all printings of oracles with >=1 print
    /// in that lang. `en` is skipped: the print-level lang_idx built from
    /// the dump already answers lang:en (33,909 vs upstream 33,827,
    /// CLOSE). Codes kept verbatim (zhs/zht); `zh` aliases to zhs in the
    /// evaluator. ~1 MB resident (31.8k entries). Returns false when the
    /// file is absent (offline deployments keep the slimmed-dump behavior).
    pub fn load_langs(&mut self, path: &Path) -> bool {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return false,
        };
        let raw: HashMap<String, Vec<String>> =
            match serde_json::from_reader(BufReader::new(GzDecoder::new(file))) {
                Ok(r) => r,
                Err(_) => return false,
            };
        // lang -> oracle rows that have that language
        let mut by_lang: HashMap<Box<str>, Vec<u32>> = HashMap::new();
        for (oid, langs) in &raw {
            let row = match self.oracle_intern.get(oid.as_str()) {
                Some(&r) => r,
                None => continue, // oracle not in the searchable dump
            };
            for l in langs {
                if l == "en" {
                    continue;
                }
                by_lang.entry(l.clone().into()).or_default().push(row);
            }
        }
        for (lang, rows) in by_lang {
            let mut rowbm = RoaringBitmap::new();
            for r in rows {
                rowbm.insert(r);
            }
            // in:<lang> = ever printed in that language. in_langs stores
            // ORACLE ROW ids (InVal spreads them via oracle_prints).
            self.in_langs.insert(lang.clone(), rowbm.clone());
            let mut bm = RoaringBitmap::new();
            for r in rowbm.iter() {
                if let Some(p) = self.oracle_prints.get(r as usize) {
                    bm |= p;
                }
            }
            self.oracle_lang_idx.insert(lang, bm);
        }
        true
    }

    pub fn load_tags(&mut self, path: &Path) -> bool {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return false,
        };
        let dec = GzDecoder::new(file);
        let mut reader = BufReader::with_capacity(1 << 20, dec);
        struct RawT {
            id: String,
            slug: String,
            aliases: Vec<String>,
            children: Vec<String>,
            oracles: Vec<String>,
        }
        let mut raws: Vec<RawT> = Vec::with_capacity(5_000);
        loop {
            let mut lines: Vec<String> = Vec::with_capacity(512);
            let mut eof = false;
            for _ in 0..512 {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(_) => {
                        let t = line.trim_end();
                        if !t.is_empty() {
                            lines.push(line);
                        }
                    }
                    Err(e) => panic!("tag file read error: {e}"),
                }
            }
            for l in &lines {
                let v: Value = match serde_json::from_str(l) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = str_of(&v, "id").to_string();
                if id.is_empty() || str_of(&v, "type") != "oracle" {
                    continue;
                }
                let mut aliases = Vec::new();
                if let Some(a) = v.get("aliases").and_then(|x| x.as_array()) {
                    aliases = a.iter().filter_map(|x| x.as_str().map(String::from)).collect();
                }
                let mut children = Vec::new();
                if let Some(a) = v.get("child_ids").and_then(|x| x.as_array()) {
                    children = a.iter().filter_map(|x| x.as_str().map(String::from)).collect();
                }
                let mut oracles = Vec::new();
                if let Some(a) = v.get("taggings").and_then(|x| x.as_array()) {
                    for g in a {
                        if let Some(o) = g.get("oracle_id").and_then(|x| x.as_str()) {
                            oracles.push(o.to_string());
                        }
                    }
                }
                raws.push(RawT {
                    id,
                    slug: str_of(&v, "slug").to_string(),
                    aliases,
                    children,
                    oracles,
                });
            }
            if eof {
                break;
            }
        }

        // pass 1: tag id -> index
        let mut idx_by_id: HashMap<String, u32> = HashMap::with_capacity(raws.len());
        for (i, r) in raws.iter().enumerate() {
            idx_by_id.insert(r.id.clone(), i as u32);
        }
        // pass 2: build tags + lookup maps (slug/alias stored normalized)
        self.otags.clear();
        self.otag_slug.clear();
        self.otag_alias.clear();
        for r in &raws {
            let mut children: Vec<u32> = Vec::with_capacity(r.children.len());
            for c in &r.children {
                if let Some(&i) = idx_by_id.get(c) {
                    children.push(i);
                }
            }
            let mut oracles: Vec<u32> = Vec::with_capacity(r.oracles.len());
            for o in &r.oracles {
                // keep only oracles present in the card pool
                if let Some(&row) = self.oracle_intern.get(o.as_str()) {
                    oracles.push(row);
                }
            }
            oracles.sort_unstable();
            oracles.dedup();
            let slug = normalize_otag_value(&r.slug);
            let ti = self.otags.len() as u32;
            self.otags.push(OTag {
                slug: slug.clone().into_boxed_str(),
                children,
                oracles,
            });
            self.otag_slug.entry(slug.into_boxed_str()).or_insert(ti);
            for a in &r.aliases {
                let na = normalize_otag_value(a);
                if na.is_empty() {
                    continue;
                }
                self.otag_alias.entry(na.into_boxed_str()).or_default().push(ti);
            }
        }
        self.otag_ready = !self.otags.is_empty();
        self.otag_ready
    }

    /// Merge the build-time-vendored Scryfall set metadata into the startup
    /// set aggregation and pre-serialize the `/api/set/{code}` envelopes.
    ///
    /// The files (`data/blocks.json`, `data/sets-meta.json`) are generated by
    /// `data/gen_sets_meta.py` from ONE fetch of `api.scryfall.com/sets`
    /// (this API deployment 404s on `/blocks`, so blocks are derived from the
    /// list's `block` field). Returns false when both files are missing — then
    /// blocks stay unknown and `order=block` falls back to the legacy
    /// set_type-grouped ordering.
    pub fn load_set_meta(&mut self, blocks_path: &Path, meta_path: &Path) -> bool {
        let meta: HashMap<String, Value> = match std::fs::read_to_string(meta_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
        {
            Some(v) => v,
            None => {
                eprintln!("set meta: {} not found — block/parent/icon fields stay empty", meta_path.display());
                return false;
            }
        };
        let blocks: Vec<Value> = std::fs::read_to_string(blocks_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.get("blocks").and_then(|b| b.as_array().cloned()))
            .unwrap_or_default();

        // code -> (block name, rank in vendored block order)
        let mut block_of: HashMap<String, (String, u32)> = HashMap::new();
        for (rank, b) in blocks.iter().enumerate() {
            let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if let Some(sets) = b.get("sets").and_then(|v| v.as_array()) {
                for s in sets {
                    if let Some(code) = s.get("code").and_then(|v| v.as_str()) {
                        block_of.insert(code.to_lowercase(), (name.clone(), rank as u32));
                    }
                }
            }
        }

        // Pre-intern every string we might attach (done BEFORE touching
        // self.sets so no simultaneous &mut borrow of sets + strings occurs).
        let mut str_cache: HashMap<String, u32> = HashMap::new();
        {
            let intern = |s: &str, ix: &mut Index, cache: &mut HashMap<String, u32>| -> u32 {
                if let Some(&o) = cache.get(s) {
                    return o;
                }
                ix.strings.push(s.to_string().into_boxed_str());
                let o = ix.strings.len() as u32 - 1;
                cache.insert(s.to_string(), o);
                o
            };
            let mut wanted: Vec<&str> = Vec::new();
            for (bn, _) in block_of.values() {
                wanted.push(bn);
            }
            for m in meta.values() {
                for k in ["block", "parent_set", "icon_svg_uri"] {
                    if let Some(s) = m.get(k).and_then(|v| v.as_str()) {
                        if !s.is_empty() {
                            wanted.push(s);
                        }
                    }
                }
            }
            for w in wanted {
                intern(w, self, &mut str_cache);
            }
        }

        // Merge pass: index-based loop; read code/meta first, then write the
        // entry — keeps borrows sequential (interned offsets are cache hits).
        for i in 0..self.sets.len() {
            let code = self.s(self.sets[i].code).to_lowercase();
            let blk = block_of.get(&code).map(|(bn, rank)| (*str_cache.get(bn.as_str()).unwrap_or(&u32::MAX), *rank));
            let m = meta.get(&code);
            let meta_block = m.and_then(|m| m.get("block")).and_then(|v| v.as_str()).and_then(|s| str_cache.get(s).copied());
            let meta_parent = m.and_then(|m| m.get("parent_set")).and_then(|v| v.as_str()).and_then(|s| str_cache.get(s).copied());
            let meta_icon = m.and_then(|m| m.get("icon_svg_uri")).and_then(|v| v.as_str()).and_then(|s| str_cache.get(s).copied());
            let (digital, foil_only, nonfoil_only, printed) = m.map_or((false, false, false, None), |m| {
                (
                    m.get("digital").and_then(|v| v.as_bool()).unwrap_or(false),
                    m.get("foil_only").and_then(|v| v.as_bool()).unwrap_or(false),
                    m.get("nonfoil_only").and_then(|v| v.as_bool()).unwrap_or(false),
                    m.get("printed_size").and_then(|v| v.as_u64()).map(|v| v as u32),
                )
            });
            let e = &mut self.sets[i];
            if let Some((off, rank)) = blk {
                e.block = off;
                e.block_rank = rank;
            } else if e.block == u32::MAX {
                if let Some(off) = meta_block {
                    e.block = off;
                }
            }
            if let Some(off) = meta_parent {
                e.parent_set = off;
            }
            if let Some(off) = meta_icon {
                e.icon = off;
            }
            e.digital = digital;
            e.foil_only = foil_only;
            e.nonfoil_only = nonfoil_only;
            if let Some(ps) = printed {
                e.printed_size = ps;
            }
        }

        // One cached pass: pre-serialize the /api/set/{code} envelopes.
        // (Collect first, insert after — avoids borrowing self inside the loop.)
        let mut out: Vec<(u32, Box<str>)> = Vec::with_capacity(self.sets.len());
        for e in self.sets.iter() {
            let s_of = |off: u32| -> Option<String> {
                if off == u32::MAX {
                    None
                } else {
                    Some(self.s(off).to_string())
                }
            };
            let released = if e.released == 0 {
                "null".to_string()
            } else {
                let d = e.released.to_string();
                format!("\"{}-{}-{}\"", &d[0..4], &d[4..6], &d[6..8])
            };
            let code = self.s(e.code).to_string();
            let mut body = String::with_capacity(512);
            body.push_str("{\"object\":\"set\",\"code\":");
            body.push_str(&crate::api::json_str(&code));
            body.push_str(",\"name\":");
            body.push_str(&crate::api::json_str(self.s(e.name)));
            body.push_str(",\"set_type\":");
            body.push_str(&crate::api::json_str(self.s(e.set_type)));
            body.push_str(",\"released_at\":");
            body.push_str(&released);
            body.push_str(",\"card_count\":");
            body.push_str(&e.card_count.to_string());
            body.push_str(",\"printed_size\":");
            body.push_str(&if e.printed_size == u32::MAX { "null".into() } else { e.printed_size.to_string() });
            body.push_str(",\"sample_image\":");
            body.push_str(&match s_of(e.sample_image) { Some(s) => crate::api::json_str(&s), None => "null".into() });
            body.push_str(",\"icon_svg_uri\":");
            body.push_str(&match s_of(e.icon) { Some(s) => crate::api::json_str(&s), None => "null".into() });
            body.push_str(",\"block\":");
            body.push_str(&match s_of(e.block) { Some(s) => crate::api::json_str(&s), None => "null".into() });
            body.push_str(",\"parent_set\":");
            body.push_str(&match s_of(e.parent_set) { Some(s) => crate::api::json_str(&s), None => "null".into() });
            body.push_str(&format!(
                ",\"digital\":{},\"foil_only\":{},\"nonfoil_only\":{}",
                e.digital, e.foil_only, e.nonfoil_only
            ));
            body.push_str(",\"scryfall_uri\":");
            body.push_str(&crate::api::json_str(&format!("https://scryfall.com/sets/{code}")));
            body.push_str(",\"search_uri\":");
            body.push_str(&crate::api::json_str(&format!(
                "/api/cards/search?q=s%3A{}+include%3Aextras&order=set",
                code
            )));
            body.push_str(&format!(
                ",\"counts\":{{\"creatures\":{},\"noncreatures\":{},\"lands\":{}}}",
                e.n_creatures, e.n_noncreatures, e.n_lands
            ));
            body.push('}');
            out.push((e.code, body.into_boxed_str()));
        }
        self.set_detail_json = out.into_iter().collect();

        // land-cycle name sets (is:dual etc.) — same dir as sets-meta
        if let Some(dir) = meta_path.parent() {
            let lc = dir.join("land_cycles.json");
            if let Ok(txt) = std::fs::read_to_string(&lc) {
                if let Ok(m) = serde_json::from_str::<std::collections::HashMap<String, Vec<String>>>(
                    &txt,
                ) {
                    self.land_cycles = m;
                }
            }
        }

        // b: (block) and g: (group) printing indexes. b:<block name or any
        // member set code> -> all printings of all sets in the block; g:<set
        // code or name> -> printings of the set's release group (self, parent,
        // siblings sharing a parent, children).
        {
            let mut block_bm: HashMap<Box<str>, RoaringBitmap> = HashMap::new();
            let mut parent_of: HashMap<String, String> = HashMap::new();
            let mut children_of: HashMap<String, Vec<String>> = HashMap::new();
            let mut name_of: HashMap<String, String> = HashMap::new();
            for e in &self.sets {
                let code = self.s(e.code).to_lowercase();
                let name = self.s(e.name).to_lowercase();
                name_of.insert(code.clone(), name);
                if let Some(ps) = if e.parent_set == u32::MAX {
                    None
                } else {
                    Some(self.s(e.parent_set))
                } {
                    let p = ps.to_lowercase();
                    if !p.is_empty() {
                        parent_of.insert(code.clone(), p.clone());
                        children_of.entry(p).or_default().push(code.clone());
                    }
                }
            }
            for (code, (bname, _rank)) in &block_of {
                if let Some(bm) = self.set_idx.get(code.as_str()) {
                    block_bm
                        .entry(bname.clone().into())
                        .or_default()
                        .extend(bm.iter());
                }
            }
            // block membership by member set code too (b:mmq)
            let mut by_member: Vec<(String, String)> = Vec::new();
            for (code, (bname, _)) in &block_of {
                by_member.push((code.clone(), bname.clone()));
            }
            for (code, bname) in by_member {
                if let Some(bm) = block_bm.get(bname.as_str()) {
                    block_bm.insert(code.into(), bm.clone());
                }
            }
            // parser lowercases filter values, so expose case-insensitive
            // block-name keys too (b:Urza and b:urza both resolve).
            let name_keys: Vec<String> = block_bm.keys().map(|k| k.to_string()).collect();
            for k in name_keys {
                let lk = k.to_lowercase();
                if lk != k {
                    if let Some(bm) = block_bm.get(k.as_str()) {
                        block_bm.insert(lk.into(), bm.clone());
                    }
                }
            }
            self.block_idx = block_bm;

            // set-name -> block-name map for the b: member-set fallback
            // (b:legacy matches set "Urza's Legacy" -> block "Urza").
            let mut sn: HashMap<Box<str>, Box<str>> = HashMap::new();
            for i in 0..self.sets.len() {
                let e = &self.sets[i];
                if e.block != u32::MAX {
                    let name = self.s(e.name).to_lowercase();
                    let blk = self.s(e.block).to_lowercase();
                    sn.entry(name.into()).or_insert(blk.into());
                }
            }
            self.block_setname = sn;

            let mut group_bm: HashMap<Box<str>, RoaringBitmap> = HashMap::new();
            let codes: Vec<String> = name_of.keys().cloned().collect();
            for code in codes {
                let mut members: Vec<String> = vec![code.clone()];
                if let Some(p) = parent_of.get(&code) {
                    members.push(p.clone());
                    if let Some(sibs) = children_of.get(p) {
                        members.extend(sibs.iter().cloned());
                    }
                }
                if let Some(kids) = children_of.get(&code) {
                    members.extend(kids.iter().cloned());
                }
                let mut bm = RoaringBitmap::new();
                for m in members.iter().filter_map(|c| self.set_idx.get(c.as_str())) {
                    bm.extend(m.iter());
                }
                if let Some(name) = name_of.get(&code) {
                    group_bm.insert(name.clone().into(), bm.clone());
                }
                group_bm.insert(code.into(), bm);
            }
            self.group_idx = group_bm;
        }
        true
    }

    /// Resolve an otag: value to the set of card printings tagged with it
    /// (transitively via children) that are visible in default search.
    /// Unknown values => warning + empty bitmap (never falls back to text).
    pub fn otag_bitmap(&self, value: &str, w: &mut Vec<String>) -> RoaringBitmap {
        if !self.otag_ready {
            w.push("oracle tag data not loaded (oracle-tags.jsonl.gz missing?) — otag: returns no results".into());
            return RoaringBitmap::new();
        }
        let norm = normalize_otag_value(value);
        let mut roots: Vec<u32> = Vec::new();
        if let Some(&i) = self.otag_slug.get(norm.as_str()) {
            roots.push(i);
        }
        if let Some(list) = self.otag_alias.get(norm.as_str()) {
            roots.extend(list.iter().copied());
        }
        roots.sort_unstable();
        roots.dedup();
        if roots.is_empty() {
            w.push(format!(
                "unknown otag \"{value}\" — no oracle tag slug/alias \"{norm}\"; 0 results (otag: never falls back to text search)"
            ));
            return RoaringBitmap::new();
        }
        // DFS over tag hierarchy (cycle-safe), union of taggings
        let mut seen = vec![false; self.otags.len()];
        let mut rows: Vec<u32> = Vec::new();
        let mut stack = roots;
        while let Some(t) = stack.pop() {
            if seen[t as usize] {
                continue;
            }
            seen[t as usize] = true;
            let tag = &self.otags[t as usize];
            rows.extend_from_slice(&tag.oracles);
            stack.extend_from_slice(&tag.children);
        }
        rows.sort_unstable();
        rows.dedup();
        let rowset: std::collections::HashSet<u32> = rows.into_iter().collect();
        let vis = &self.oracle_vis;
        let co = &self.card_oracle;
        let hits: Vec<u32> = (0..self.cards.len() as u32)
            .into_par_iter()
            .filter(|&i| vis[co[i as usize] as usize] && rowset.contains(&co[i as usize]))
            .collect();
        RoaringBitmap::from_iter(hits)
    }

    /// `id:` — Scryfall's "card name abbreviation" operator. Proven semantics
    /// (live full-set diffs, see the ID_EXTRA_HIDDEN_* notes): a card matches
    /// iff its ORACLE-level color_identity is a subset of the value's colour
    /// letters; value "c" matches colorless identities only. Restricted to the
    /// unique-card pool (oracle_id_vis). Result counts must be taken over
    /// DISTINCT ORACLES to match Scryfall's total_cards.
    pub fn id_bitmap(&self, mask: u8) -> RoaringBitmap {
        let mut hits: Vec<u32> = Vec::with_capacity(4096);
        for (i, &row) in self.card_oracle.iter().enumerate() {
            let r = row as usize;
            if self.oracle_id_vis[r] && self.oracle_ci[r] & !mask == 0 {
                hits.push(i as u32);
            }
        }
        RoaringBitmap::from_iter(hits)
    }

    #[inline]
    pub fn s(&self, off: u32) -> &str {
        &self.strings[off as usize]
    }
    #[inline]
    pub fn name_lc(&self, i: u32) -> &str {
        self.s(self.name_lc[i as usize])
    }
    #[inline]
    pub fn type_lc(&self, i: u32) -> &str {
        self.s(self.type_lc[i as usize])
    }
    #[inline]
    pub fn oracle_lc(&self, i: u32) -> &str {
        self.s(self.oracle_lc[i as usize])
    }
    #[inline]
    pub fn otext_lc(&self, i: u32) -> &str {
        self.s(self.otext_lc[i as usize])
    }
    #[inline]
    pub fn cost_all(&self, i: u32) -> &str {
        self.s(self.cost_all[i as usize])
    }
    #[inline]
    pub fn artist_lc(&self, i: u32) -> &str {
        self.s(self.artist_lc[i as usize])
    }
    #[inline]
    pub fn set_name_lc(&self, i: u32) -> &str {
        self.s(self.set_name_lc[i as usize])
    }
    #[inline]
    pub fn mana_norm(&self, i: u32) -> &str {
        self.s(self.mana_norm[i as usize])
    }
    #[inline]
    pub fn pip(&self, i: u32) -> u32 {
        self.pip[i as usize]
    }

    /// coloured pip for colour bit b (0=w..4=g) of card i.
    #[inline]
    pub fn pip_b(&self, i: u32, b: usize) -> u8 {
        ((self.pip[i as usize] >> (b * 4)) & 15) as u8
    }

    /// Pack coloured pip counts from a raw mana_cost ("{W}{U}{W/U}" -> w=2,u=2)
/// into 4 bits per colour (order w,u,b,r,g). Hybrids/phyrexians count 1 pip
/// for each colour letter (matches Scryfall devotion rules).
pub fn pip_pack(mana_cost: &str) -> u32 {
    let mut p = [0u32; 5];
    for grp in mana_cost.split('}') {
        let g = grp.trim_start_matches('{');
        if g.is_empty() {
            continue;
        }
        // a symbol may contain digits (2/G) or hybrid slashes; only the
        // colour letters matter, 'p' (phyrexian marker) does not add a pip.
        for c in g
            .chars()
            .filter(|c| c.is_ascii_alphabetic() && *c != 'p')
            .map(|c| c.to_ascii_lowercase())
        {
            let b = match c {
                'w' => 0,
                'u' => 1,
                'b' => 2,
                'r' => 3,
                'g' => 4,
                _ => continue,
            };
            p[b] += 1;
        }
    }
    let mut out = 0u32;
    for (i, v) in p.iter().enumerate() {
        out |= (*v).min(15) << (i * 4);
    }
    out
}

/// set codes whose lowercased set name contains `q` (for in:/s: by name).
    pub fn set_name_matches(&self, q: &str) -> Vec<String> {
        let mut out = Vec::new();
        if q.len() < 3 {
            return out;
        }
        for e in &self.sets {
            let name = self.s(e.name);
            if name.to_lowercase().contains(q) {
                out.push(self.s(e.code).to_lowercase());
            }
        }
        out
    }

    /// Cards whose NAME tokens fuzzy-match `q` (see [`prefix_edit_dist`]).
    /// Candidates come from the query's first-2-byte token bucket, so a word
    /// scan never touches the full 21k-token vocabulary; verification is exact.
    pub fn fuzzy_name_cards(&self, q: &str) -> RoaringBitmap {
        self.fuzzy_tok_cards(q, &self.name_tok_list, &self.name_tok_buckets, &self.name_tokens)
    }

    /// Cards whose TYPE tokens fuzzy-match `q` — typo'd type words
    /// ("planezwalker" -> "planeswalker") still find the right cards.
    pub fn fuzzy_type_cards(&self, q: &str) -> RoaringBitmap {
        self.fuzzy_tok_cards(q, &self.type_tok_list, &self.type_tok_buckets, &self.type_tokens)
    }

    fn fuzzy_tok_cards(
        &self,
        q: &str,
        list: &[Box<str>],
        buckets: &HashMap<u16, Vec<u32>>,
        map: &HashMap<String, RoaringBitmap>,
    ) -> RoaringBitmap {
        let mut bm = RoaringBitmap::new();
        let qb = q.as_bytes();
        if qb.len() < 3 {
            return bm;
        }
        let key = ((qb[0] as u16) << 8) | (qb[1] as u16);
        let Some(ids) = buckets.get(&key) else {
            return bm;
        };
        for &tid in ids {
            let tok = &list[tid as usize];
            if prefix_edit_dist(q, tok).is_some() {
                if let Some(cards) = map.get(&**tok) {
                    bm |= cards;
                }
            }
        }
        bm
    }

    /// Binary-search the sorted prefix table for `q`, return card ids whose lowercased
    /// name starts with q, up to `cap` (then truncates).
    pub fn prefix_hits(&self, q: &str, cap: usize) -> RoaringBitmap {
        let mut bm = RoaringBitmap::new();
        if q.is_empty() {
            return bm;
        }
        let start = self
            .prefix
            .binary_search_by(|&(off, _)| self.s(off).cmp(q))
            .unwrap_or_else(|p| p);
        let mut count = 0;
        for &(off, i) in &self.prefix[start..] {
            if !self.s(off).starts_with(q) {
                break;
            }
            bm.insert(i);
            count += 1;
            if count >= cap {
                break;
            }
        }
        bm
    }
}

/// Min Damerau-Levenshtein distance between `q` and any *prefix* of token `t`
/// (i.e. the query word may be a typo'd, possibly truncated copy of the token).
/// Some(d) when d is within tolerance: 2 edits for words of 4+ chars, 1 edit
/// for 3-char words (below that fuzzy is pure noise). None otherwise.
pub fn prefix_edit_dist(q: &str, t: &str) -> Option<u8> {
    let qc: Vec<char> = q.chars().collect();
    let n = qc.len();
    if n < 3 {
        return None;
    }
    let maxd: usize = if n >= 4 { 2 } else { 1 };
    let tc: Vec<char> = t.chars().take(n + maxd).collect();
    let m = tc.len();
    if m + maxd < n {
        return None; // token too short to reach q within tolerance
    }
    let jmax = m.min(n + maxd);
    // rolling DP rows: dp[i][j] = edit distance q[..i] -> t[..j]; the answer is
    // min dp[n][j] over prefix lengths j, so we never require consuming all of t.
    let mut prev2 = vec![0u16; jmax + 1]; // dp[i-2]
    let mut prev = vec![0u16; jmax + 1]; // dp[i-1]
    let mut cur = vec![0u16; jmax + 1]; // dp[i]
    for j in 0..=jmax {
        prev[j] = j as u16; // dp[0][j] = j insertions
    }
    for i in 1..=n {
        cur[0] = i as u16; // dp[i][0] = i deletions
        for j in 1..=jmax {
            let cost = if qc[i - 1] == tc[j - 1] { 0 } else { 1 };
            let mut d = prev[j] + 1; // delete from q
            d = d.min(cur[j - 1] + 1); // insert into q
            d = d.min(prev[j - 1] + cost); // substitute / match
            if i >= 2 && j >= 2 && qc[i - 1] == tc[j - 2] && qc[i - 2] == tc[j - 1] {
                d = d.min(prev2[j - 2] + 1); // adjacent transposition
            }
            cur[j] = d;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    let mut best = u16::MAX;
    for j in n.saturating_sub(maxd)..=jmax {
        best = best.min(prev[j]);
    }
    if (best as usize) <= maxd {
        Some(best as u8)
    } else {
        None
    }
}

fn str_of_rarity(r: u8) -> &'static str {
    match r {
        0 => "common",
        1 => "uncommon",
        2 => "rare",
        3 => "mythic",
        4 => "special",
        5 => "bonus",
        6 => "timeshifted",
        _ => "special",
    }
}

#[cfg(test)]
mod tests {
    use super::prefix_edit_dist;

    #[test]
    fn prefix_edit_dist_fuzzy_cases() {
        // the five regression words vs their intended tokens
        assert_eq!(prefix_edit_dist("lotsu", "lotus"), Some(1)); // transposition
        assert_eq!(prefix_edit_dist("lite", "lightning"), Some(2)); // prefix "ligh"
        assert_eq!(prefix_edit_dist("rital", "ritual"), Some(1)); // insert
        assert_eq!(prefix_edit_dist("planezwalker", "planeswalker"), Some(1)); // sub
        assert_eq!(prefix_edit_dist("tasigur", "tasigur"), Some(0));
        // exact prefix is free
        assert_eq!(prefix_edit_dist("dark", "darkness"), Some(0));
        // 3-char words stay at 1 edit; note the comparison is against any
        // PREFIX of the token: "drc" -> "dr" (prefix of druid) is 1 deletion,
        // while "drc" -> "dar"/"dark" needs 2 edits (n=3 caps tolerance at 1).
        assert_eq!(prefix_edit_dist("drc", "druid"), Some(1));
        assert_eq!(prefix_edit_dist("drc", "dark"), None);
        // bare type words must fuzzy-match the type-line vocabulary too
        assert_eq!(prefix_edit_dist("instantt", "instant"), Some(1));
        assert_eq!(prefix_edit_dist("enchanment", "enchantment"), Some(1));
    }

    #[test]
    fn prefix_edit_dist_rejects_far_words() {
        assert_eq!(prefix_edit_dist("mountain", "swamp"), None);
        assert_eq!(prefix_edit_dist("goblin", "elves"), None);
        assert_eq!(prefix_edit_dist("ab", "anything"), None); // q too short for fuzzy
        assert_eq!(prefix_edit_dist("bolt", "lightning"), None); // >2 edits
    }
}

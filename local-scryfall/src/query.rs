//! Scryfall-syntax-lite query parser: tokens -> AST.
//! Supports: free text, "phrases", () grouping, and/or/not (!, -), key:value filters
//! with operators for numbers (=, !=, >, <, >=, <=, ranges 3-5, colon-less cmc>=4)
//! and colors (>, <, =, !, colon-less c>rw).

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SubOp {
    Include, // contains all listed
    Subset,  // only listed (or fewer)
    Exact,   // exactly listed
    Exclude, // none of listed
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NumField {
    Cmc,
    Power,
    Toughness,
    Loyalty,
    // parity additions
    Pt,             // p/t "either" semantics
    Defense,        // battle defense
    Colors,         // number of colors (identity:true for id:)
    PriceUsd,
    PriceUsdFoil,
    PriceTix,
    PriceEur,
    PriceEurFoil,
    Edhrec,
    Prints,
    Sets,
    PaperPrints,
    PaperSets,
    Artists,
    Illustrations,
    Year,
    Date,
    Cn,
}

/// Brace-spec token for `m:{…}{…}` literal-symbol search (wave-4). Upstream
/// m:{X} = cost CONTAINS that symbol after generic-pip normalization
/// ({N} = N × {1}); single coloured letters are pip requirements; anything
/// else (C, Y, Z, HW) is a literal printed symbol. Golden-fit against
/// gambit cache: m:{0}=32447 (any cost), m:{1}=27515 (generic>=1) …
/// m:{C}=28, m:{Y}=m:{Z}=m:{HW}=1 (test card), m:{K}=upstream 400.
#[derive(Debug, Clone)]
pub enum Mp {
    Lit(String),     // exact brace body present in mana cost
    Pip(u8),         // >=1 pure pip of this color
    GenericMin(u32), // normalized generic pips >= n ({N} semantics)
    AnyCost,         // card has a non-empty mana cost ({0} upstream quirk)
}

#[derive(Debug)]
pub enum Filter {
    ManaSyms(Vec<Mp>),
    Fatal400(String),
    /// Upstream does not reject the whole query for this — it drops the bad
    /// expression with a warning ("Invalid expression ... was ignored") and
    /// answers the REST of the query. Only when EVERY term is dropped does
    /// Scryfall return 400 "All of your terms were ignored." (probed live
    /// 2026-10-26: `kw:tap` alone -> 400, `k:flying t:instant` -> 200/3916
    /// i.e. plain t:instant). prune_ignored() implements the drop; the
    /// all-dropped case emits a \u{1}-sentinel warning for api.rs's 400 path.
    Ignored(String),
    Colors { mask: u8, op: SubOp, identity: bool },
    Type(String),
    Oracle(String),
    Name(String),
    Mana(String),
    Artist(String),
    Cmp { field: NumField, op: Cmp, val: f32 },
    CmpRange { field: NumField, lo: f32, hi: f32 },
    /// cn: comparator (wave-4 golden-fit): upstream range ops compare the
    /// numeric part over PURE-digit printings only (suffixed prints like 12a
    /// / 116★ are NULL for </>/<=/>=), while = matches the collector-number
    /// string exactly (cn=12a -> 290 gold, cn=12 -> 12-only). Predicate runs
    /// per-print over GATED printings (oracle-exists after dedup): golden
    /// cn=1 449 / cn>1 33723 / cn>=1 33826 match only with print-gating.
    CnCmp { op: Cmp, num: u32, full: Option<String> },
    Rarity(u8),
    Set(String),
    Format(usize),
    Keyword(String),
    Is(String),
    Layout(String),
    /// otag:<value> — oracle (card-behavior) tag from the tagger dump
    OracleTag(String),
    /// id:<value> — "card name abbreviation". Real semantics (reverse-engineered
    /// from live Scryfall full-set diffs, Oct 2026): matches iff the card's
    /// ORACLE-level color_identity ⊆ value letters (w/u/b/r/g); value "c"
    /// matches colorless identities only; a single digit 0-5 matches cards
    /// with exactly that many colours in their identity (id:0 == id:c).
    /// Case/order/dupes irrelevant. Any other value => Scryfall ignores the
    /// term entirely (400 "All of your terms were ignored") — we emit warning
    /// + 0 results.
    Id(String),
    /// tag:<value> — card-level tag; needs the live tagger API (offline: warning)
    CardTag(String),
    /// art:/atag:/arttag:<value> — Tagger illustration tags; dump not bundled
    /// offline (loud warning + 0 results, never text fallback — probed upstream
    /// to NOT be an artist: alias).
    ArtTag(String),
    /// include:<value> — Scryfall's visibility broadener. Neutral term for the
    /// evaluator; the api layer reads its presence to skip the default-search
    /// unique-card pool gate (include:extras / include:all).
    IncludeVal(String),
    /// unknown key -> treat value as free phrase
    Text(String),

    // ---- parity additions (Oct 2026 syntax-parity work) ----
    /// unique:cards|prints|art — dedup mode for total_cards/data
    Unique(u8), // 0=cards 1=prints 2=art
    NameExact(String),
    /// regex name/oracle/type search (/.../ values): field 0=name 1=oracle 2=type
    Regex { field: u8, pat: String },
    /// c:m / c:!m / c:c / c:gold / c:rainbow specials
    ColorSpecial { identity: bool, kind: u8 }, // 0=multicolor 1=colorless 2=rainbow
    /// multiset mana-cost containment (m:{G}{U}, m>3WU). reqs: (color-mask,count);
    /// gc = required generic (colorless pips); op=strictness.
    Mana2 { need: Vec<(u8, u8)>, gc: u8, op: Cmp },
    /// exact symbol-presence: brace hybrid/phyrexian/snow/X specs ({2/G}, {R/P},
    /// {G/U}, {S}, {X}) — Scryfall matches the literal symbol in the cost.
    ManaExact { syms: Vec<String>, need_x: bool },
    /// exact coloured multiset cover + explicit generic digit with op
    /// (m:2WW, m:{2}{W}{W}, m>3WU); hybrids may cover either colour.
    ManaCover { need: Vec<(u8, u8)>, gc: u8, op: Cmp },
    /// devotion:<syms|n> — (color-mask, need) per req; total for bare number.
    Devotion { reqs: Vec<(u8, u8)>, total: Option<u8> },
    HasField(u8),
    InVal(String),
    SetType(String),
    Games(String),
    Lang(String),
    Block(String),
    Group(String),
    Banned(usize),
    Restricted(usize),
    Watermark(String),
    /// produces: bitmask of WUBRG | 32=colorless | 64=any
    Produces(u8),
    Parity(bool), // cmc even/odd
    NotVal(String),
    NewKind(u8),
    Released(Cmp, String),
    PriceExists(u8),
    CrossCmp { a: NumField, op: Cmp, b: NumField },
    FlavorText(String),
    FullOracle(String),
    /// known key, value unsupported by our data: warn + empty result
    Warn0(String),
    /// printing index over a small enum-ish field: 0=border 1=frame 2=stamp
    /// 3=frame_effects 4=promo_type
    InField { kind: u8, value: String },
}

/// HAS_* indices for Filter::HasField
pub const HAS_WATERMARK: u8 = 0;
pub const HAS_INDICATOR: u8 = 1;
pub const HAS_FOIL: u8 = 2;
pub const HAS_ETCHED: u8 = 3;
pub const HAS_NONFOIL: u8 = 4;
pub const HAS_HIRES: u8 = 5;
pub const HAS_VIDEO: u8 = 6;
pub const HAS_BOOSTER: u8 = 7;
pub const HAS_SPOTLIGHT: u8 = 8;
pub const HAS_TEXT: u8 = 9;
pub const HAS_FLAVOR: u8 = 10;
pub const HAS_ALCHEMY: u8 = 11;
pub const HAS_REPRINT: u8 = 12;
pub const HAS_UNIQUE: u8 = 13;
pub const HAS_ARTIST: u8 = 14;
pub const HAS_PROMO: u8 = 15;
pub const HAS_ARTSERIES: u8 = 16;
pub const HAS_TEXTLESS: u8 = 17;
pub const HAS_FULLART: u8 = 18;
pub const HAS_EXTART: u8 = 19;

#[derive(Debug)]
pub enum Node {
    All,
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Text(String),
    Phrase(String),
    Filter(Filter),
}

#[derive(Debug, Clone)]
struct WordTok {
    text: String,
    /// atom started with a quote -> whole atom is a phrase
    first_quoted: bool,
}

#[derive(Debug, Clone)]
enum Tok {
    LParen,
    RParen,
    Word(WordTok),
}

fn tokenize(input: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut first_quoted = false;
    let mut started = false;
    let mut in_quote = false;
    for c in input.chars() {
        match c {
            '"' => {
                if !started {
                    first_quoted = true;
                }
                started = true;
                cur.push(c);
                in_quote = !in_quote;
            }
            '(' if !in_quote => {
                flush(&mut toks, &mut cur, &mut started, &mut first_quoted);
                toks.push(Tok::LParen);
            }
            ')' if !in_quote => {
                flush(&mut toks, &mut cur, &mut started, &mut first_quoted);
                toks.push(Tok::RParen);
            }
            c if c.is_whitespace() && !in_quote => {
                flush(&mut toks, &mut cur, &mut started, &mut first_quoted);
            }
            c => {
                if !started {
                    first_quoted = false;
                }
                started = true;
                cur.push(c);
            }
        }
    }
    flush(&mut toks, &mut cur, &mut started, &mut first_quoted);
    toks
}

fn flush(toks: &mut Vec<Tok>, cur: &mut String, started: &mut bool, first_quoted: &mut bool) {
    if *started {
        toks.push(Tok::Word(WordTok {
            text: std::mem::take(cur),
            first_quoted: *first_quoted,
        }));
        *started = false;
        *first_quoted = false;
    }
}

fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn split_key_val(text: &str) -> Option<(String, String, bool)> {
    // split on first ':' that occurs outside quotes; returns (key, value, value_was_quoted)
    let mut in_q = false;
    let mut colon = None;
    for (i, c) in text.char_indices() {
        match c {
            '"' => in_q = !in_q,
            ':' if !in_q => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let key = &text[..colon];
    if key.is_empty() {
        return None;
    }
    let rest = &text[colon + 1..];
    let quoted = rest.starts_with('"');
    Some((key.to_string(), strip_quotes(rest), quoted))
}

/// Color ops without a colon: c>rw, ci<uw, c!mono.
fn split_color_op_atom(text: &str) -> Option<(String, String)> {
    let lower = text.to_lowercase();
    let (base, rest) = if let Some(r) = lower.strip_prefix("coloridentity") {
        ("ci", r)
    } else if let Some(r) = lower.strip_prefix("identity") {
        ("ci", r)
    } else if let Some(r) = lower.strip_prefix("color") {
        ("c", r)
    } else if let Some(r) = lower.strip_prefix("ci") {
        ("ci", r)
    } else if let Some(r) = lower.strip_prefix('c') {
        ("c", r)
    } else {
        return None;
    };
    let op = rest.chars().next()?;
    if !matches!(op, '>' | '<' | '=' | '!') {
        return None;
    }
    Some((format!("{base}{op}"), rest[1..].to_string()))
}

/// Numeric filters without a colon: cmc>=4, p=2, tou>=5, lov<=3, mv<2, pow!=1
fn split_num_op_atom(text: &str) -> Option<(NumField, String)> {
    let lower = text.to_lowercase();
    let keys: &[(&str, NumField)] = &[
        ("convertedmana", NumField::Cmc),
        ("manavalue", NumField::Cmc),
        ("converted", NumField::Cmc),
        ("toughness", NumField::Toughness),
        ("loyalty", NumField::Loyalty),
        ("power", NumField::Power),
        ("cmc", NumField::Cmc),
        ("pow", NumField::Power),
        ("tou", NumField::Toughness),
        ("def", NumField::Toughness),
        ("lov", NumField::Loyalty),
        ("loy", NumField::Loyalty),
        ("mv", NumField::Cmc),
        ("p", NumField::Power),
    ];
    for (k, field) in keys {
        if let Some(rest) = lower.strip_prefix(*k) {
            let starts = ["<=", ">=", "!=", "=", "<", ">"];
            for op in starts {
                if let Some(v) = rest.strip_prefix(op) {
                    if !v.is_empty() && (v.starts_with(|c: char| c.is_ascii_digit() || c == '.')) {
                        return Some((*field, rest.to_string()));
                    }
                }
            }
            return None; // key matched but no operator -> not a numeric atom
        }
    }
    None
}

/// Operator split for colon-less atoms: ">5", "<=0.25", "=2024" ...
fn split_cmp_prefix(rest: &str) -> Option<(Cmp, &str)> {
    if let Some(r) = rest.strip_prefix(">=") {
        Some((Cmp::Ge, r))
    } else if let Some(r) = rest.strip_prefix("<=") {
        Some((Cmp::Le, r))
    } else if let Some(r) = rest.strip_prefix("!=") {
        Some((Cmp::Ne, r))
    } else if let Some(r) = rest.strip_prefix('>') {
        Some((Cmp::Gt, r))
    } else if let Some(r) = rest.strip_prefix('<') {
        Some((Cmp::Lt, r))
    } else if let Some(r) = rest.strip_prefix('=') {
        Some((Cmp::Eq, r))
    } else {
        None
    }
}

/// Colon-less numeric/date/price/cross-field atoms Scryfall accepts:
/// `usd>5`, `eur<=0.5`, `tix>1`, `usd_foil>2`, `edhrec<100`, `prints>=3`,
/// `year=2024`, `released>2024-01-01`, `pow>tou`, `t<pow`. The colon forms
/// (`usd:>5`) already go through build_filter; this covers the spellings
/// without a colon, which previously fell through to free-text search.
fn bare_field_atom(text: &str) -> Option<Node> {
    let lower = text.to_lowercase();
    // cross-field comparisons first: pow>tou, t<pow, toughness>=power, ...
    for (ak, a) in [
        ("power", NumField::Power),
        ("pow", NumField::Power),
        ("p", NumField::Power),
        ("toughness", NumField::Toughness),
        ("tou", NumField::Toughness),
        ("t", NumField::Toughness),
    ] {
        if let Some(rest) = lower.strip_prefix(ak) {
            if let Some((op, r)) = split_cmp_prefix(rest) {
                let b = match r {
                    "tou" | "toughness" | "t" => NumField::Toughness,
                    "pow" | "power" | "p" => NumField::Power,
                    "cmc" | "mv" | "convertedmana" | "manavalue" => NumField::Cmc,
                    "loy" | "loyalty" => NumField::Loyalty,
                    _ => continue,
                };
                return Some(Node::Filter(Filter::CrossCmp { a, op, b }));
            }
        }
    }
    // price / rank / count keys with a numeric value
    for (k, f) in [
        ("usd_foil", NumField::PriceUsdFoil),
        ("usdfoil", NumField::PriceUsdFoil),
        ("usd", NumField::PriceUsd),
        ("eur_foil", NumField::PriceEurFoil),
        ("eurfoil", NumField::PriceEurFoil),
        ("eur", NumField::PriceEur),
        ("euro", NumField::PriceEur),
        ("tix", NumField::PriceTix),
        ("price", NumField::PriceUsd),
        ("dollar", NumField::PriceUsd),
        ("edhrecrank", NumField::Edhrec),
        ("edhrec", NumField::Edhrec),
        ("paperprints", NumField::PaperPrints),
        ("papersets", NumField::PaperSets),
        ("prints", NumField::Prints),
        ("printings", NumField::Prints),
        ("sets", NumField::Sets),
        ("artists", NumField::Artists),
        ("illustrations", NumField::Illustrations),
        ("pt", NumField::Pt),
        ("cn", NumField::Cn),
    ] {
        if let Some(rest) = lower.strip_prefix(k) {
            if let Some((_, r)) = split_cmp_prefix(rest) {
                // parity w5 §1: ALL bare collector-number operator atoms take
                // the CnCmp quantifier path (∃-over-printings with suffixed
                // prints NULL, gated pool via api dedup) — the plain f32
                // cmp_filter here was ungated (cn>=1: 38,322 vs 33,826
                // golden); suffixed atoms (cn>=12a) previously fell through
                // to a 0-hit text search.
                if f == NumField::Cn && r.starts_with(|c: char| c.is_ascii_digit()) {
                    return Some(cn_atom(rest));
                }
                if !r.is_empty() && r.parse::<f32>().is_ok() {
                    return Some(cmp_filter(f, rest));
                }
            }
        }
    }
    // rarity letter orderings: r>=r (common < uncommon < rare < mythic < ...)
    for k in ["rarity", "r"] {
        if let Some(rest) = lower.strip_prefix(k) {
            if let Some((op, r)) = split_cmp_prefix(rest) {
                if let Some(rank) = rarity_letter(r) {
                    let ranks: Vec<u8> = match op {
                        Cmp::Gt => (rank + 1..=6).collect(),
                        Cmp::Ge => (rank..=6).collect(),
                        Cmp::Lt => (0..rank).collect(),
                        Cmp::Le => (0..=rank).collect(),
                        Cmp::Ne => (0..=6).filter(|&x| x != rank).collect(),
                        Cmp::Eq => vec![rank],
                    };
                    return Some(if ranks.len() == 1 {
                        Node::Filter(Filter::Rarity(ranks[0]))
                    } else {
                        Node::Or(
                            ranks
                                .into_iter()
                                .map(|x| Node::Filter(Filter::Rarity(x)))
                                .collect(),
                        )
                    });
                }
            }
        }
    }
    // multiset mana costs: m>3WU, mana>=2UU
    for k in ["m", "mana", "manacost"] {
        if let Some(rest) = lower.strip_prefix(k) {
            if split_cmp_prefix(rest).is_some() {
                // brace specs (m:{Y}, m:{C}, m:{1}, m:{W}{U}, …) ride the
                // literal-symbol model; unbraced specs keep the legacy path.
                if rest.contains('{') {
                    if let Some(f) = mana_brace_spec(rest) {
                        return Some(Node::Filter(f));
                    }
                } else if let Some(f) = mana_spec(rest) {
                    return Some(Node::Filter(f));
                }
                if let Some((need, gc, op)) = parse_mana_req(rest) {
                    return Some(Node::Filter(Filter::Mana2 { need, gc, op }));
                }
            }
        }
    }
    // devotion>=5, dev<2
    for k in ["devotion", "dev"] {
        if let Some(rest) = lower.strip_prefix(k) {
            if let Some((_, r)) = split_cmp_prefix(rest) {
                if let Some((reqs, total)) = parse_devotion(r) {
                    return Some(Node::Filter(Filter::Devotion { reqs, total }));
                }
            }
        }
    }
    // identity operators without colon: id<=esper, id>=bg, identity=wu
    for k in ["identity", "id"] {
        if let Some(rest) = lower.strip_prefix(k) {
            if split_cmp_prefix(rest).is_some() {
                let r = rest.trim_start_matches(['<', '>', '=', '!']);
                if !r.is_empty()
                    && (r.chars().all(|c| matches!(c, 'w' | 'u' | 'b' | 'r' | 'g' | 'c'))
                        || color_nickname(r).is_some())
                {
                    return Some(Node::Filter(Filter::Id(rest.to_string())));
                }
            }
        }
    }
    // produces=wu, produce>{G}
    for k in ["produces", "produce", "producing"] {
        if let Some(rest) = lower.strip_prefix(k) {
            if let Some((op, r)) = split_cmp_prefix(rest) {
                if !r.is_empty() {
                    let mask = parse_produces(r);
                    // `produces=X` (equality operator) means EXACTLY that
                    // colour set (Scryfall distinguishes = from :).
                    return Some(Node::Filter(Filter::Produces(if op == Cmp::Eq && mask < 32 {
                        mask | 128
                    } else {
                        mask
                    })));
                }
            }
        }
    }
    if let Some(rest) = lower.strip_prefix("year") {
        if let Some((_, r)) = split_cmp_prefix(rest) {
            if !r.is_empty() && r.parse::<f32>().is_ok() {
                return Some(cmp_filter(NumField::Year, rest));
            }
        }
    }
    // released/date with an operator: value is a date, year, "now", or set code
    for k in ["released", "date"] {
        if let Some(rest) = lower.strip_prefix(k) {
            if let Some((op, r)) = split_cmp_prefix(rest) {
                if !r.is_empty()
                    && r.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
                {
                    return Some(Node::Filter(Filter::Released(op, r.to_string())));
                }
            }
        }
    }
    None
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    pub warnings: Vec<String>,
    /// closed keyword-enum set (full lowercased phrases) from the live index.
    /// None = offline/test parse(): keyword values fall through to the legacy
    /// eval-side heuristics instead of parse-time drop semantics.
    kwenum: Option<std::collections::HashSet<String>>,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn peek_word(&self) -> Option<(String, bool)> {
        match self.peek() {
            Some(Tok::Word(w)) => Some((w.text.clone(), w.first_quoted)),
            _ => None,
        }
    }

    fn parse_or(&mut self) -> Node {
        let mut parts = vec![self.parse_and()];
        loop {
            let is_or = match self.peek_word() {
                Some((t, false)) => t.eq_ignore_ascii_case("or"),
                _ => false,
            };
            if !is_or {
                break;
            }
            self.next();
            parts.push(self.parse_and());
        }
        if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            Node::Or(parts)
        }
    }

    fn parse_and(&mut self) -> Node {
        let mut parts: Vec<Node> = Vec::new();
        loop {
            match self.peek() {
                None | Some(Tok::RParen) => break,
                Some(Tok::Word(w)) if !w.first_quoted => {
                    if w.text.eq_ignore_ascii_case("or") {
                        break;
                    }
                    if w.text.eq_ignore_ascii_case("and") {
                        self.next();
                        continue;
                    }
                }
                _ => {}
            }
            parts.push(self.parse_unary());
        }
        if parts.is_empty() {
            Node::All
        } else if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            Node::And(parts)
        }
    }

    fn peek_word_at(&self, pos: usize) -> Option<(String, bool)> {
        match self.toks.get(pos) {
            Some(Tok::Word(w)) => Some((w.text.clone(), w.first_quoted)),
            _ => None,
        }
    }

    fn parse_unary(&mut self) -> Node {
        if let Some((text, false)) = self.peek_word() {
            let lower = text.to_lowercase();
            if lower == "not" || lower == "!" || lower == "-" {
                // Scryfall rejects negating include:; drop it with a warning
                // instead of evaluating Not(All) => empty universe.
                if let Some((next, false)) = self.peek_word_at(self.pos + 1) {
                    if next.to_lowercase().starts_with("include:") {
                        self.warnings
                            .push("include: cannot be negated (ignored)".to_string());
                        self.next();
                        self.next();
                        return Node::All;
                    }
                }
                self.next();
                // A lone `!` immediately before a group: upstream has no
                // `!(` syntax — hard parse error 400 even with valid
                // neighbours (golden row `!(t:creature) c:m`; the tokenizer
                // splits `!(` into Word("!") + LParen, so the lone-'!'
                // branch below is where the pattern actually shows up).
                if lower == "!" && matches!(self.peek(), Some(Tok::LParen)) {
                    return Node::Filter(Filter::Fatal400(
                        "search syntax error: unexpected '!' before '('".into(),
                    ));
                }
                return Node::Not(Box::new(self.parse_unary()));
            }
            if (lower.starts_with('!') || lower.starts_with('-'))
                && lower.len() > 1
                && !lower[1..].starts_with(|c: char| c.is_ascii_digit())
            {
                let inner = lower[1..].to_string();
                if inner.to_lowercase().starts_with("include:") {
                    self.warnings
                        .push("include: cannot be negated (ignored)".to_string());
                    self.next();
                    return Node::All;
                }
                self.next();
                if lower.starts_with('!') {
                    // Scryfall: `!word` and `!"a phrase"` are EXACT-NAME
                    // searches, not negation. Filter negation keeps its shape
                    // (`!t:creature`, `!c:mono`, `!{W}`, `!(a or b)`) and stays
                    // on the Not() path below.
                    let i = inner.trim();
                    let quoted_exact = i.len() >= 2 && i.starts_with('"') && i.ends_with('"');
                    let bare_exact = !i.is_empty()
                        && !quoted_exact
                        && !i.contains(':')
                        && !i.contains('(')
                        && !i.contains('"')
                        && !i.contains('|')
                        && !i.starts_with('{')
                        && !i.contains(|c| matches!(c, '>' | '<' | '!' | '~'))
                        && i != "c";
                    if quoted_exact {
                        return Node::Filter(Filter::NameExact(
                            i.trim_matches('"').trim().to_string(),
                        ));
                    }
                    if bare_exact {
                        return Node::Filter(Filter::NameExact(i.to_string()));
                    }
                }
                // upstream has no `!(` group-negation syntax — hard parse
                // error 400 even when other terms are valid (golden row
                // `!(t:creature) c:m` -> 400). The tokenizer splits `!(`
                // into Word("!") + LParen, so the pattern is an empty
                // inner followed by an LParen token; the whole-text form
                // (inner itself starting with '(') is covered too.
                if inner.trim().starts_with('(')
                    || (inner.trim().is_empty() && matches!(self.peek(), Some(Tok::LParen)))
                {
                    return Node::Filter(Filter::Fatal400(
                        "search syntax error: unexpected '!' before '('".into(),
                    ));
                }
                return Node::Not(Box::new(self.atom_to_node(inner, false)));
            }
        }
        match self.next() {
            Some(Tok::LParen) => {
                let n = self.parse_or();
                if let Some(Tok::RParen) = self.peek() {
                    self.next();
                }
                n
            }
            Some(Tok::RParen) => Node::All,
            Some(Tok::Word(w)) => self.atom_to_node(w.text, w.first_quoted),
            None => Node::All,
        }
    }

    fn atom_to_node(&mut self, text: String, first_quoted: bool) -> Node {
        if first_quoted {
            return Node::Phrase(strip_quotes(&text).to_lowercase());
        }
        // `*` is Scryfall's match-everything term (pool = gated universe)
        if text == "*" {
            return Node::All;
        }
        // upstream has no `!(` group-negation syntax — 400 (golden row)
        if text.starts_with("!(") {
            return Node::Filter(Filter::Fatal400(
                "search syntax error: unexpected '!' before '('".into(),
            ));
        }
        if let Some((key, value, vq)) = split_key_val(&text) {
            return self.build_filter(&key, &value, vq);
        }
        // 'defense=N' (no colon): upstream has no defense operator (probed
        // 400 "Unknown keyword defense"); equality form is a dropped term.
        // >,< forms upstream 404s — left on the legacy text path.
        if text.len() > 8 {
            let ll = text.to_lowercase();
            if let Some(rest) = ll.strip_prefix("defense") {
                if rest.starts_with('=') && !rest.starts_with("!=") {
                    return Node::Filter(Filter::Ignored(
                        "unknown keyword \"defense\"".into(),
                    ));
                }
            }
        }
        if let Some((field, v)) = split_num_op_atom(&text) {
            return cmp_filter(field, &v);
        }
        if let Some((key, value)) = split_color_op_atom(&text) {
            return self.build_filter(&key, &value, false);
        }
        if let Some(node) = bare_field_atom(&text) {
            return node;
        }
        // no colon: single free word (or fuzzy token)
        Node::Text(text.to_lowercase())
    }

    fn build_filter(&mut self, key: &str, value: &str, quoted: bool) -> Node {
        let k = key.to_lowercase();
        let v = value.to_lowercase();
        // display-only keys Scryfall accepts and ignores for search purposes
        match k.as_str() {
            "order" | "sort" | "dir" | "direction" | "display" | "view" | "prefer"
            | "formatdisplay" | "framefx" | "doublefaced" => return Node::All,
            _ => {}
        }
        match k.as_str() {
            // Only the BARE colon ops reject an '='-prefixed value (upstream
            // `c:=w` -> "Unknown color ="); operator-suffixed keys (c>, ci>=
            // atom form) receive '=' as a split artifact of the operator and
            // must strip it — tests.sh `ci>=gw` contract (include semantics).
            "c" | "color" => color_filter_eq(&v, SubOp::Include, false, true),
            "c>" | "color>" => color_filter_eq(&v, SubOp::Include, false, false),
            "c<" | "color<" => color_filter(&v, SubOp::Subset, false),
            "c=" | "color=" => color_filter(&v, SubOp::Exact, false),
            "c!" | "color!" => color_filter(&v, SubOp::Exclude, false),
            // Upstream bare ci: = "AT MOST these colours" (subset); proven
            // against golden: ci:w 8059==ci<=w, ci:gu 13276==ci<=gu,
            // ci:bant 19419==ci<=bant (all within 0.4%). ci> stays the
            // contains-all operator (tests.sh ci>=gw contract).
            "ci" | "identity" | "coloridentity" => color_filter_eq(&v, SubOp::Subset, true, true),
            "ci>" | "identity>" => color_filter_eq(&v, SubOp::Include, true, false),
            "ci<" | "identity<" => color_filter(&v, SubOp::Subset, true),
            "ci=" | "identity=" => color_filter(&v, SubOp::Exact, true),
            "ci!" | "identity!" => color_filter(&v, SubOp::Exclude, true),
            "t" | "type" | "typekind" | "ot" | "oracletype" => {
                if let Some(pat) = regex_value(&v) {
                    Node::Filter(Filter::Regex { field: 2, pat })
                } else {
                    Node::Filter(Filter::Type(v))
                }
            }
            "o" | "oracle" | "text" => {
                if let Some(pat) = regex_value(&v) {
                    Node::Filter(Filter::Regex { field: 1, pat })
                } else if v.contains('~') {
                    Node::Filter(Filter::Regex { field: 1, pat: tilde_pattern(&v) })
                } else {
                    Node::Filter(Filter::Oracle(v))
                }
            }
            "n" | "name" => {
                if let Some(pat) = regex_value(&v) {
                    Node::Filter(Filter::Regex { field: 0, pat })
                } else if let Some(ex) = v.strip_prefix('!') {
                    Node::Filter(Filter::NameExact(ex.to_string()))
                } else {
                    Node::Filter(Filter::Name(v))
                }
            }
            "m" | "mana" | "mana_cost" | "cost" => {
                if v.contains('{') {
                    // brace specs ride the literal-symbol model (wave-4)
                    if let Some(f) = mana_brace_spec(&v) {
                        return Node::Filter(f);
                    }
                }
                match mana_spec(&v) {
                    Some(f) => Node::Filter(f),
                    None => match parse_mana_req(&v) {
                        Some((need, gc, op)) => Node::Filter(Filter::Mana2 { need, gc, op }),
                        None => Node::Filter(Filter::Mana(v)),
                    },
                }
            }
            "a" | "artist" => Node::Filter(Filter::Artist(v)),
            // art:/atag:/arttag: — Tagger illustration tags ("art that contains
            // a squirrel"); live-probed NOT to be artist aliases (art:squirrel
            // == arttag:squirrel == 199, artist:"Larry Elmore" != art:...).
            // The illustration-tag dump is not bundled offline -> loud warning,
            // never a text fallback (same honest path as tag:).
            "art" | "atag" | "arttag" => Node::Filter(Filter::ArtTag(v)),
            "artists" | "illustrators" => cmp_filter(NumField::Artists, &v),
            "illustrations" => cmp_filter(NumField::Illustrations, &v),
            "cmc" | "mv" | "convertedmana" | "converted" | "convertedmana>" | "cmc>"
            | "manavalue" | "manavalues" => {
                match v.as_str() {
                    "even" => Node::Filter(Filter::Parity(true)),
                    "odd" => Node::Filter(Filter::Parity(false)),
                    "c" | "colorless" => {
                        Node::Filter(Filter::Cmp { field: NumField::Colors, op: Cmp::Eq, val: 0.0 })
                    }
                    _ => cmp_filter(NumField::Cmc, &v),
                }
            }
            "pow" | "p" | "power" => {
                if let Some(cc) = cross_cmp_atom(&v, NumField::Power) {
                    cc
                } else {
                    cmp_filter(NumField::Pt, &v)
                }
            }
            "tou" | "toughness" => {
                if let Some(cc) = cross_cmp_atom(&v, NumField::Toughness) {
                    cc
                } else {
                    cmp_filter(NumField::Pt, &v)
                }
            }
            "def" => cmp_filter(NumField::Toughness, &v),
            "loy" | "loyalty" | "lov" => cmp_filter(NumField::Loyalty, &v),
            "defense" if v.trim_start().starts_with('=')
                || v.trim_start()
                    .chars()
                    .next()
                    .map_or(true, |c| c.is_ascii_digit()) =>
            {
                // upstream has no 'defense' operator at all (probed: 400
                // "Unknown keyword defense"); equality form mirrors it as a
                // dropped term. >,< forms upstream 404 — left on the legacy
                // cmp path so they stay 200-side, never a false reject.
                Node::Filter(Filter::Ignored("unknown keyword \"defense\"".into()))
            }
            "defense" => cmp_filter(NumField::Defense, &v),
            "usd" | "dollar" | "price" => price_cmp_or_exists(NumField::PriceUsd, &v),
            "usd_foil" | "usdfoil" | "foilprice" => {
                price_cmp_or_exists(NumField::PriceUsdFoil, &v)
            }
            "tix" => price_cmp_or_exists(NumField::PriceTix, &v),
            "eur" | "euro" => price_cmp_or_exists(NumField::PriceEur, &v),
            "eur_foil" | "eurfoil" => price_cmp_or_exists(NumField::PriceEurFoil, &v),
            "cheapest" | "cheap" => {
                let kind = match v.as_str() {
                    "tix" => 2,
                    "eur" => 3,
                    "eur_foil" | "eurfoil" => 4,
                    "usd_foil" | "usdfoil" => 1,
                    _ => 0,
                };
                Node::Filter(Filter::PriceExists(kind))
            }
            "edhrecrank" | "edhrec_rank" | "edhrec" => cmp_filter(NumField::Edhrec, &v),
            "prints" | "printings" => cmp_filter(NumField::Prints, &v),
            "reprints" => cmp_filter(NumField::Prints, &v),
            "sets" | "setcount" | "sets=" => cmp_filter(NumField::Sets, &v),
            "paperprints" => cmp_filter(NumField::PaperPrints, &v),
            "papersets" | "papersetcount" => cmp_filter(NumField::PaperSets, &v),
            "year" => cmp_filter(NumField::Year, &v),
            "date" => cmp_filter(NumField::Date, &v),
            "cn" | "number" | "collector" | "collector_number" | "collectornumber" => {
                cn_atom(&v)
            }
            "r" | "rarity" => {
                let (op, rest) = split_op(&v);
                match rarity_letter(rest) {
                    Some(r) => {
                        let ranks: Vec<u8> = match op {
                            Cmp::Gt => (r + 1..=6).collect(),
                            Cmp::Ge => (r..=6).collect(),
                            Cmp::Lt => (0..r).collect(),
                            Cmp::Le => (0..=r).collect(),
                            Cmp::Ne => (0..=6).filter(|&x| x != r).collect(),
                            Cmp::Eq => vec![r],
                        };
                        if ranks.len() == 1 {
                            Node::Filter(Filter::Rarity(ranks[0]))
                        } else {
                            Node::Or(
                                ranks
                                    .into_iter()
                                    .map(|x| Node::Filter(Filter::Rarity(x)))
                                    .collect(),
                            )
                        }
                    }
                    None => {
                        // upstream drops the term with a warning; 400 only
                        // when EVERY term is dropped (probed live 2026-10-26)
                        Node::Filter(Filter::Ignored(format!("unknown rarity: {v}")))
                    }
                }
            }
            "s" | "set" | "e" | "expansion" | "setcode" => Node::Filter(Filter::Set(v)),
            "f" | "format" | "legal" => match format_index(&v) {
                Some(i) => Node::Filter(Filter::Format(i)),
                None => {
                    // formats upstream knows but we carry no legality bits for
                    // must stay 200-with-warning; genuinely unknown values are
                    // dropped terms (probed: legal:competit 400 came with the
                    // "invalid expression ignored" warning = all-terms rule).
                    if matches!(v.as_str(), "custom" | "historicbrawl" | "duelcommander") {
                        self.warnings
                            .push(format!("unsupported format '{v}' (0 results)"));
                        Node::Filter(Filter::Warn0(format!("unsupported format '{v}'")))
                    } else {
                        Node::Filter(Filter::Ignored(format!("unknown format: {v}")))
                    }
                }
            },
            // upstream has no 'k' operator (probed: k:flying 400 "Unknown
            // keyword k"); kw: and keyword: are the real keyword ops and
            // share the same value space (kw:flying == keyword:flying 3291).
            "k" => Node::Filter(Filter::Ignored("unknown keyword \"k\"".into())),
            "keyword" | "kw" => {
                // closed value space (probed live 2026-10-26): non-keyword
                // words are dropped as invalid expressions ("Unknown keyword
                // draw"), NOT a whole-query reject; single-term junk 400s
                // only via the all-terms-ignored rule. Mirrors every
                // theirs_json _status=400 keyword row in the gambit cache.
                const KW_JUNK: &[&str] = &[
                    "draw",
                    "counter",
                    "tap",
                    "each",
                    "target",
                    "forest",
                    "cannot",
                    "end",
                    "exile",
                    "destroy",
                    "sacrifice",
                    "angel",
                    "demon",
                    "dragon",
                    "upkeep",
                    "checkland",
                    "bounceland",
                ];
                if KW_JUNK.contains(&v.as_str()) {
                    return Node::Filter(Filter::Ignored(format!(
                        "unknown keyword \"{v}\""
                    )));
                }
                if let Some(en) = &self.kwenum {
                    if en.contains(v.as_str()) {
                        return Node::Filter(Filter::Keyword(v));
                    }
                    // NOTE parity w5: word-prefix matching against multi-word
                    // enum phrases (keyword:Tempting→"Tempting offer") was
                    // tried and REVERTED: the bulk keywords[] blob contains
                    // ~600 acorn/junk phrases ("death gigas", "job select")
                    // beyond upstream's real enum, so prefix hits poison the
                    // OR (keyword family 21→38 DRIFT). Left as drop-with-
                    // warning exactly as before; multi-word rows stay on the
                    // trailing bare-word path which lands on theirs totals.
                    // unknown value: upstream drops the keyword term (the
                    // all-terms-ignored rule turns the empty case into 400).
                    // Quoted multi-word values ("level up") that miss the
                    // enum are junk too; unquoted multi-word values never
                    // reach here (the tokenizer splits them, so the residual
                    // words arrive as independent text atoms).
                    return Node::Filter(Filter::Ignored(format!(
                        "unknown keyword \"{v}\""
                    )));
                }
                Node::Filter(Filter::Keyword(v))
            }
            "is" => {
                // upstream supports neither is:legendary nor is:basic: the
                // term is ignored with a warning and the query answers with
                // the remaining terms (probed live 2026-10-26).
                if v == "legendary" || v == "basic" {
                    return Node::Filter(Filter::Ignored(format!(
                        "checking if cards are \"{v}\" is not supported"
                    )));
                }
                Node::Filter(Filter::Is(v))
            }
            "layout" => Node::Filter(Filter::Layout(v)),
            // oracle tags: from the bundled tagger dump (never text fallback)
            "otag" | "oracle-tag" | "oracle_tag" | "tag-oracle" => {
                Node::Filter(Filter::OracleTag(value.to_lowercase()))
            }
            // card colour abbreviation (= colour identity subset; see Filter::Id).
            // NOTE: `cardid:` is NOT a Scryfall alias (probed 400 "terms ignored")
            // — it goes through the same unknown-value warning path as id:zz.
            "id" => {
                // upstream value space (all live-probed 2026-10-26): single
                // digit 0-5 (exact identity size), 'c' (colorless), pure
                // w/u/b/r/g letter sets, and colour nicknames. Everything
                // else ("id:zz"/"Unknown color z", "id:6", "id:11",
                // "id:rc"/colored+colorless, "id:jb") is dropped as an
                // invalid expression — 400 only when it's the only term
                // (all-terms-ignored rule, mirrors the gambit reject rows).
                let iv = value.to_lowercase();
                let norm: String = iv
                    .trim()
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                let norm = norm
                    .strip_prefix("<=")
                    .or(norm.strip_prefix(">="))
                    .or(norm.strip_prefix("=="))
                    .or(norm.strip_prefix('='))
                    .or(norm.strip_prefix('<'))
                    .or(norm.strip_prefix('>'))
                    .unwrap_or(&norm)
                    .to_string();
                let valid = norm.is_empty()
                    || norm == "c"
                    || (norm.len() == 1
                        && norm.as_bytes()[0].is_ascii_digit()
                        && norm.as_bytes()[0] <= b'5')
                    || (!norm.is_empty()
                        && norm.chars().all(|c| matches!(c, 'w' | 'u' | 'b' | 'r' | 'g')))
                    || color_nickname(&norm).is_some();
                if !valid {
                    return Node::Filter(Filter::Ignored(format!(
                        "\"{norm}\" is not a valid id value"
                    )));
                }
                Node::Filter(Filter::Id(iv))
            }
            "cardid" => Node::Filter(Filter::Id(format!("\u{0}cardid:{value}"))),
            // card-level tags need the live tagger API -> helpful warning, 0 results
            "tag" | "card-tag" | "card_tag" => Node::Filter(Filter::CardTag(value.to_lowercase())),
            // include: — Scryfall's visibility broadener (include:extras pulls
            // tokens/art sheets/funny variants into results). Our search path
            // applies no visibility gate to printings, so the faithful
            // equivalent is a NEUTRAL term: no filtering, no bogus warning.
            // Queries WITHOUT include: are untouched (the battery depends on
            // byte-identical behavior); previously `include:` fell through to
            // "unknown filter key" + text search => 0 results.
            "include" => {
                const KNOWN: [&str; 10] = [
                    "extras",
                    "all",
                    "legacy",
                    "shadow",
                    "spotlight",
                    "future",
                    "spoiled",
                    "nonlegal",
                    "hasboostercrypto",
                    "hascontentwarnings",
                ];
                if !KNOWN.contains(&v.as_str()) {
                    self.warnings
                        .push(format!("unknown include: value '{value}' (ignored)"));
                }
                Node::Filter(Filter::IncludeVal(v))
            }
            // ---- parity keys (Oct 2026) ----
            "unique" | "uniqueness" => match v.as_str() {
                "prints" | "printings" => Node::Filter(Filter::Unique(1)),
                "art" | "artwork" | "illustrations" => Node::Filter(Filter::Unique(2)),
                _ => Node::Filter(Filter::Unique(0)),
            },
            "in" | "inside" => Node::Filter(Filter::InVal(v)),
            "st" | "settype" | "set-type" | "stype" | "set_type" => {
                Node::Filter(Filter::SetType(v))
            }
            "game" | "games" => Node::Filter(Filter::Games(v)),
            "lang" | "language" | "languages" => {
                // upstream 400s only these two observed junk values; the rest
                // of the space (he/la/grc/… exotic valid codes) stays Lang().
                const BADLANG: [&str; 2] = ["hans", "hant"];
                if BADLANG.contains(&v.as_str()) {
                    Node::Filter(Filter::Ignored(format!("unknown language: {v}")))
                } else {
                    Node::Filter(Filter::Lang(v))
                }
            }
            "b" | "block" | "blocking" => Node::Filter(Filter::Block(v)),
            "g" | "group" | "grouping" | "groups" => Node::Filter(Filter::Group(v)),
            "banned" | "ban" => match format_index(&v) {
                Some(i) => Node::Filter(Filter::Banned(i)),
                // banned:<block code> form (banned:vk) — treat as block-banned:
                // Scryfall has no such thing; warn+0
                None => {
                    self.warnings.push(format!("unknown format: {v}"));
                    Node::Filter(Filter::Warn0(format!("unknown banned format '{v}'")))
                }
            },
            "restricted" | "limit" | "limits" => match format_index(&v) {
                Some(i) => Node::Filter(Filter::Restricted(i)),
                None => {
                    self.warnings.push(format!("unknown format: {v}"));
                    Node::Filter(Filter::Warn0(format!(
                        "unknown restricted format '{v}'"
                    )))
                }
            },
            "wm" | "watermark" => Node::Filter(Filter::Watermark(v)),
            "has" => match v.as_str() {
                "watermark" => Node::Filter(Filter::HasField(HAS_WATERMARK)),
                "colorindicator" | "color_indicator" | "indicator" => {
                    Node::Filter(Filter::HasField(HAS_INDICATOR))
                }
                "foil" => Node::Filter(Filter::HasField(HAS_FOIL)),
                "etched" | "etching" => Node::Filter(Filter::HasField(HAS_ETCHED)),
                "nonfoil" => Node::Filter(Filter::HasField(HAS_NONFOIL)),
                "hires" | "highres" | "highresimage" => {
                    Node::Filter(Filter::HasField(HAS_HIRES))
                }
                "video" => Node::Filter(Filter::HasField(HAS_VIDEO)),
                "booster" => Node::Filter(Filter::HasField(HAS_BOOSTER)),
                "spotlight" | "story_spotlight" | "storyspotlight" => {
                    Node::Filter(Filter::HasField(HAS_SPOTLIGHT))
                }
                "text" => Node::Filter(Filter::HasField(HAS_TEXT)),
                "flavor" | "flavortext" => Node::Filter(Filter::HasField(HAS_FLAVOR)),
                "alchemy" => Node::Filter(Filter::HasField(HAS_ALCHEMY)),
                "reprint" => Node::Filter(Filter::HasField(HAS_REPRINT)),
                "unique" => Node::Filter(Filter::HasField(HAS_UNIQUE)),
                "artist" => Node::Filter(Filter::HasField(HAS_ARTIST)),
                "promo" => Node::Filter(Filter::HasField(HAS_PROMO)),
                "art_series" | "artseries" => Node::Filter(Filter::HasField(HAS_ARTSERIES)),
                "textless" => Node::Filter(Filter::HasField(HAS_TEXTLESS)),
                "fullart" => Node::Filter(Filter::HasField(HAS_FULLART)),
                "extendedart" => Node::Filter(Filter::HasField(HAS_EXTART)),
                _ => {
                    // has: is a closed enum upstream; the values below are
                    // observed 400s (has:variations / has:framefx / …). Other
                    // unknowns keep the 200+warning path because upstream
                    // still 200s has:reprint, has:fullart, … (golden rows).
                    const REJECT: [&str; 6] = [
                        "variations", "boostered", "framefx", "border", "hand", "mana_cost",
                    ];
                    if REJECT.contains(&v.as_str()) {
                        Node::Filter(Filter::Ignored(format!("unknown has: value '{v}'")))
                    } else {
                        Node::Filter(Filter::Warn0(format!("unknown has: value '{v}'")))
                    }
                }
            },
            "devotion" | "dev" => match parse_devotion(&v) {
                Some((reqs, total)) => Node::Filter(Filter::Devotion { reqs, total }),
                None => Node::Filter(Filter::Warn0(format!(
                    "unsupported devotion value '{value}'"
                ))),
            },
            "produces" | "produce" | "producing" => {
                Node::Filter(Filter::Produces(parse_produces(&v)))
            }
            "cube" | "cubed" | "cubedby" => {
                self.warnings
                    .push(format!("unknown filter key '{key}:' (cube data offline)"));
                Node::Filter(Filter::Warn0(format!("cube: filters unsupported offline")))
            }
            "not" => Node::Filter(Filter::NotVal(v)),
            "new" => {
                let m = match v.as_str() {
                    "art" => crate::index::NEW_ART,
                    "artist" => crate::index::NEW_ARTIST,
                    "flavor" | "flavortext" => crate::index::NEW_FLAVOR,
                    "frame" => crate::index::NEW_FRAME,
                    "rarity" => crate::index::NEW_RARITY,
                    "language" | "lang" => crate::index::NEW_LANGUAGE,
                    "preview" => crate::index::NEW_PREVIEW,
                    _ => {
                        self.warnings
                            .push(format!("unknown new: value '{v}'"));
                        0
                    }
                };
                Node::Filter(Filter::NewKind(m))
            }
            // upstream has NO 'released' operator (probed 2026-10-26: 400
            // "Unknown keyword released"; in combos the term is DROPPED:
            // released:>2020-01-01 t:instant == plain t:instant 3916).
            "released" | "releasedate" | "releasedate>" | "released>" => {
                Node::Filter(Filter::Ignored("unknown keyword \"released\"".into()))
            }
            "ft" | "flavor_text" | "flavortext" | "flavourtext" => {
                Node::Filter(Filter::FlavorText(v))
            }
            "fo" | "fulloracle" | "full_oracle" => Node::Filter(Filter::FullOracle(v)),
            "border" => Node::Filter(Filter::InField {
                kind: 0,
                value: v.replace('_', "").replace(' ', ""),
            }),
            "frame" => {
                // upstream 400s unknown frame enums (frame:acorn, frame:dreamlit…)
                const FRAMES: [&str; 9] = [
                    "1993", "2003", "2015", "future", "retro", "etched", "showcase",
                    "inverted", "extendedart",
                ];
                if FRAMES.contains(&v.as_str()) {
                    Node::Filter(Filter::InField {
                        kind: 1,
                        value: v.replace("series", ""),
                    })
                } else {
                    // upstream drops the term: 400 "Unknown frame acorn" was
                    // an all-terms-ignored reject (probed live).
                    Node::Filter(Filter::Ignored(format!("unknown frame: {v}")))
                }
            }
            "stamp" => Node::Filter(Filter::InField { kind: 2, value: v }),
            "ptypes" | "promotype" | "promotypes" => Node::Filter(Filter::InField {
                kind: 4,
                value: v,
            }),
            "edition" => {
                // edition:<set code or name> — Scryfall resolves set codes.
                Node::Filter(Filter::Set(v))
            }
            "function" => Node::Filter(Filter::OracleTag(value.to_lowercase())),
            "commander" | "cmdr" | "commandername" => Node::And(vec![
                Node::Filter(Filter::NameExact(v)),
                Node::Filter(Filter::Is("commander".to_string())),
            ]),
            _ => {
                self.warnings.push(format!("unknown filter key '{key}:' (searched as text)"));
                if quoted || v.contains(' ') {
                    Node::Phrase(v)
                } else {
                    Node::Text(v)
                }
            }
        }
    }
}

/// Resolve a color value (letters, nickname, special word) to a Filter node.
/// `identity` selects colors vs color-identity comparison.
/// `reject_eq`: bare colon forms (c:/ci:) treat a leading '=' as an invalid
/// colour word (upstream: c:=w -> 400, dropped term); operator-suffixed
/// forms (c>, ci>) receive the '=' as an operator-split artifact and just
/// strip it (tests.sh `ci>=gw` include-all contract).
fn color_filter_eq(v: &str, op: SubOp, identity: bool, reject_eq: bool) -> Node {
    let t = v.trim_start();
    if reject_eq && t.starts_with('=') {
        return Node::Filter(Filter::Ignored(format!(
            "invalid color value '{}'",
            v
        )));
    }
    color_filter(t.strip_prefix('=').unwrap_or(t), op, identity)
}

fn color_filter(v: &str, op: SubOp, identity: bool) -> Node {
    // specials and nicknames (Scryfall color: docs; probed exact-set semantics)
    let t = v.trim();
    // upstream rejects `c:=w` (colon form: op already consumed, value still
    // starts with '=') with 400. The colon-less atom splitter delivers
    // `c<=gb` as (c<, "=gb") — there the '=' is part of the operator and is
    // stripped, not rejected. Also 400: unknown college word "loreheld".
    if t.starts_with('=') && matches!(op, SubOp::Include) {
        // dropped term (probed: c:=w "Unknown color =" — 400 only as sole
        // term via the all-terms-ignored rule)
        return Node::Filter(Filter::Ignored(format!(
            "invalid color value '{}'",
            v
        )));
    }
    let t = t.strip_prefix('=').unwrap_or(t);
    if t == "loreheld" {
        return Node::Filter(Filter::Ignored(format!(
            "'loreheld' is not a valid color name"
        )));
    }
    // Strixhaven-style college nicknames: in the ci: subset space upstream
    // answers cluster at 26.1k-26.4k (golden ci:chaos 26104, ci:aggression
    // 26221, ci:altruism 26197, ci:artifice 26381) = "at most the 4 colours
    // OTHER than the college colour". Golden-fit mapping.
    if identity && matches!(op, SubOp::Subset) {
        let m = match t {
            "chaos" => Some(30u8),      // not white
            "aggression" | "altruism" => Some(29u8), // not blue
            "artifice" => Some(15u8),   // not green
            "growth" => Some(27u8),     // golden-fit 26238 vs 26130 (0.4%)
            _ => None,
        };
        if let Some(mask) = m {
            return Node::Filter(Filter::Colors { mask, op, identity });
        }
    }
    let special = match t {
        "m" | "multicolor" | "multicoloured" | "gold" => Some(0u8),
        "c" | "colorless" | "colourless" | "x" => Some(1u8),
        "rainbow" => Some(2u8),
        _ => None,
    };
    // specials are word-valued, not operator-valued: ci:m == ci>=m == is:monoCI
    if let Some(kind) = special {
        return Node::Filter(Filter::ColorSpecial { identity, kind });
    }
    if let Some(rest) = t.strip_prefix('!') {
        // c:!m — Scryfall treats !m like m (probe: same totals); !c excludes colorless
        if rest == "m" || rest == "multicolor" || rest == "gold" {
            return Node::Filter(Filter::ColorSpecial { identity, kind: 0 });
        }
    }
    // exact digit count: c:2 (plain) == c=2
    if op == SubOp::Include && t.len() == 1 && t.as_bytes()[0].is_ascii_digit() {
        let n = t.as_bytes()[0] - b'0';
        return Node::Filter(Filter::Cmp { field: NumField::Colors, op: Cmp::Eq, val: n as f32 });
    }
    if let Some(mask) = color_nickname(t) {
        return Node::Filter(Filter::Colors { mask, op, identity });
    }
    if t.is_empty() {
        return Node::Filter(Filter::Colors { mask: 0, op, identity });
    }
    let mut mask = 0u8;
    for ch in t.chars() {
        match ch.to_ascii_uppercase() {
            'W' => mask |= 1,
            'U' => mask |= 2,
            'B' => mask |= 4,
            'R' => mask |= 8,
            'G' => mask |= 16,
            _ => {}
        }
    }
    Node::Filter(Filter::Colors { mask, op, identity })
}

/// Named color groups from the Scryfall color: docs (guilds, shards, wedges,
/// 4-color, chaos/order). Returns None for anything not a known name.
pub(crate) fn color_nickname(v: &str) -> Option<u8> {
    const NAMES: [(&str, u8); 45] = [
        ("azorius", 1 | 2), ("dimir", 2 | 4), ("rakdos", 4 | 8), ("gruul", 8 | 16),
        ("selesnya", 1 | 16), ("orzhov", 1 | 4), ("izzet", 2 | 8), ("golgari", 4 | 16),
        ("boros", 1 | 8), ("simic", 2 | 16),
        ("bant", 1 | 2 | 16), ("jeskai", 1 | 2 | 8), ("mardu", 4 | 8 | 16),
        ("abzan", 1 | 4 | 16), ("sultai", 2 | 4 | 16),
        // wedges (Esper/Tron set): esper WUB, jund BRG, grixis UBR, naya RGW,
        // temur URG. (jeskai/temur masks were wrong pre-wave3; fixed + live-
        // verified vs upstream: id<=esper t:instant 2372, id<=wub t:instant.)
        ("esper", 1 | 2 | 4), ("jund", 4 | 8 | 16), ("grixis", 2 | 4 | 8),
        ("naya", 1 | 8 | 16),
        ("protean", 1 | 2 | 8), ("edge", 1 | 4 | 8), ("temur", 2 | 8 | 16),
        ("quartet", 1 | 2 | 4), ("trinity", 1 | 4 | 8),
        ("chaos", 2 | 4 | 8 | 16), ("order", 1 | 4 | 16),
        // Strixhaven colleges + Tarkir clan philosophies + Brothers' War
        // "artifice": bare c:<name> = >=1 FACE containing all group colours
        // (golden-fit vs cached upstream sets: quandrix GU 645/647,
        // silverquill WB 626/625, witherbloom BG 606/612, prismari RU 675/670,
        // artifice WUBR 65/65, altruism WURG 69/68, growth WUBG 64/64,
        // chaos UBRG 63/63, aggression WBRG 63/63).
        ("quandrix", 2 | 16), ("silverquill", 1 | 4), ("witherbloom", 4 | 16),
        ("prismari", 2 | 8), ("lumra", 1 | 8),
        ("artifice", 1 | 2 | 4 | 8), ("aggression", 1 | 4 | 8 | 16),
        ("altruism", 1 | 2 | 8 | 16), ("growth", 1 | 2 | 4 | 16),
        ("white", 1), ("blue", 2), ("black", 4), ("red", 8), ("green", 16),
        ("w", 1), ("u", 2), ("b", 4), ("r", 8), ("g", 16),
    ];
    let t: String = v.chars().filter(|c| c.is_ascii_alphabetic()).collect();
    let tl = t.to_lowercase();
    for (name, mask) in NAMES {
        if name == tl {
            return Some(mask);
        }
    }
    // 4-color names: missing-one form e.g. "no-white" style not needed; try
    // concatenations of letters in any order after normalization above.
    None
}

fn cmp_filter(field: NumField, v: &str) -> Node {
    let (op, rest) = if let Some(r) = v.strip_prefix(">=") {
        (Cmp::Ge, r)
    } else if let Some(r) = v.strip_prefix("<=") {
        (Cmp::Le, r)
    } else if let Some(r) = v.strip_prefix("!=") {
        (Cmp::Ne, r)
    } else if let Some(r) = v.strip_prefix('<') {
        (Cmp::Lt, r)
    } else if let Some(r) = v.strip_prefix('>') {
        (Cmp::Gt, r)
    } else if let Some(r) = v.strip_prefix('=') {
        (Cmp::Eq, r)
    } else {
        (Cmp::Eq, v)
    };
    if let Some(dash) = rest.find('-') {
        if dash > 0 {
            if let (Ok(lo), Ok(hi)) = (rest[..dash].parse::<f32>(), rest[dash + 1..].parse::<f32>())
            {
                return Node::Filter(Filter::CmpRange { field, lo, hi });
            }
        }
    }
    match rest.trim().parse::<f32>() {
        Ok(val) => Node::Filter(Filter::Cmp { field, op, val }),
        Err(_) => Node::Filter(Filter::Cmp { field, op, val: f32::NAN }),
    }
}

fn rarity_letter(v: &str) -> Option<u8> {
    match v {
        "c" | "common" => Some(0),
        "u" | "uncommon" => Some(1),
        "r" | "rare" => Some(2),
        "m" | "mythic" | "mythic rare" => Some(3),
        "s" | "special" => Some(4),
        "b" | "bonus" => Some(5),
        "t" | "timeshifted" => Some(6),
        _ => None,
    }
}

pub fn format_index(v: &str) -> Option<usize> {
    let v = match v {
        "edh" => "commander",
        "ob" => "oathbreaker",
        other => other,
    };
    crate::index::FORMATS.iter().position(|f| *f == v)
}

// ---- parity parse helpers (Oct 2026) ----

/// `/pattern/` value → inner regex source, if the value is regex-delimited.
fn regex_value(v: &str) -> Option<String> {
    let t = v.trim();
    if t.len() >= 2 && t.starts_with('/') && t.ends_with('/') {
        Some(t[1..t.len() - 1].to_string())
    } else {
        None
    }
}

/// Convert an o:~ value into a regex: tildes become ".*" wildcards, the rest
/// is escaped literally (approximation of Scryfall's self-referential ~).
fn tilde_pattern(v: &str) -> String {
    let mut out = String::new();
    for ch in v.chars() {
        match ch {
            '~' => out.push_str(".*"),
            c if regex_syntax_needs_escape(c) => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

fn regex_syntax_needs_escape(c: char) -> bool {
    matches!(
        c,
        '.' | '\\' | '+' | '*' | '?' | '(' | ')' | '|' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$'
    )
}

/// ">=2024-01-01" style op splitting for date-ish values.
fn split_op(v: &str) -> (Cmp, &str) {
    if let Some(r) = v.strip_prefix(">=") {
        (Cmp::Ge, r)
    } else if let Some(r) = v.strip_prefix("<=") {
        (Cmp::Le, r)
    } else if let Some(r) = v.strip_prefix("!=") {
        (Cmp::Ne, r)
    } else if let Some(r) = v.strip_prefix('>') {
        (Cmp::Gt, r)
    } else if let Some(r) = v.strip_prefix('<') {
        (Cmp::Lt, r)
    } else if let Some(r) = v.strip_prefix('=') {
        (Cmp::Eq, r)
    } else {
        (Cmp::Eq, v)
    }
}

/// pow>tou / power<cmc cross-field comparisons: value is "<op> <fieldname>".
fn cross_cmp_atom(v: &str, self_field: NumField) -> Option<Node> {
    let (op, rest) = if let Some(r) = v.strip_prefix(">=") {
        (Cmp::Ge, r)
    } else if let Some(r) = v.strip_prefix("<=") {
        (Cmp::Le, r)
    } else if let Some(r) = v.strip_prefix('<') {
        (Cmp::Lt, r)
    } else if let Some(r) = v.strip_prefix('>') {
        (Cmp::Gt, r)
    } else {
        return None;
    };
    let b = match rest.trim() {
        "tou" | "toughness" | "t" => NumField::Toughness,
        "pow" | "power" | "p" => NumField::Power,
        "cmc" | "mv" | "convertedmana" | "manavalue" => NumField::Cmc,
        "loy" | "loyalty" => NumField::Loyalty,
        _ => return None,
    };
    let a = if self_field == NumField::Pt {
        NumField::Power
    } else {
        self_field
    };
    Some(Node::Filter(Filter::CrossCmp { a, op, b }))
}

/// price keys: numeric value → comparison; any/other → presence check.
fn price_cmp_or_exists(field: NumField, v: &str) -> Node {
    let (op, rest) = split_op(v);
    if let Ok(x) = rest.parse::<f32>() {
        Node::Filter(Filter::Cmp { field, op, val: x })
    } else {
        let kind = match field {
            NumField::PriceUsd => 0,
            NumField::PriceUsdFoil => 1,
            NumField::PriceTix => 2,
            NumField::PriceEur => 3,
            _ => 4,
        };
        Node::Filter(Filter::PriceExists(kind))
    }
}

/// collector-number values: "123", ">=100", "1a", "*123*" (contains).
fn parse_cn_value(v: &str) -> Option<(Cmp, f32)> {
    let (op, rest) = split_op(v);
    if rest.chars().all(|c| c.is_ascii_digit()) && !rest.is_empty() {
        return rest.parse::<f32>().ok().map(|x| (op, x));
    }
    // non-numeric suffix (1a / 001a): numeric prefix compare is close enough
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<f32>().ok().map(|x| (op, x))
}

/// cn: comparator builder (wave-4). Upstream model proven against golden
/// counts (cn=1 449, cn>1 33723, cn>=1 33826, cn=10 395, cn>10 32713,
/// cn<=10 3600, cn<100 19483): predicate runs per-printing over GATED
/// printings (oracle-exists after dedup) and RANGE ops only consider
/// PURE-digit collector numbers — suffixed prints (12a, 116★, 2024-12-06)
/// are NULL for </>/<=/>=. '=' compares the exact collector-number string
/// (cn=12a matches prints whose string IS 12a).
fn cn_atom(v: &str) -> Node {
    let (op, rest) = split_op(v);
    if rest.is_empty() {
        return Node::Filter(Filter::Warn0(format!(
            "unsupported collector-number value '{v}'"
        )));
    }
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        // no numeric prefix at all: exact-string equality only
        return Node::Filter(Filter::CnCmp {
            op,
            num: u32::MAX,
            full: Some(rest.to_lowercase()),
        });
    }
    let num: u32 = digits.parse().unwrap_or(u32::MAX);
    let suffix = &rest[digits.len()..];
    if suffix.is_empty() {
        Node::Filter(Filter::CnCmp {
            op,
            num,
            full: None,
        })
    } else {
        Node::Filter(Filter::CnCmp {
            op,
            num,
            full: Some(rest.to_lowercase()),
        })
    }
}

/// m: / mana: router. Recognises exact-symbol specs (brace bodies with '/',
/// snow {S}, {X}) and full-cover specs (explicit generic digits + coloured
/// symbols, e.g. 2WW, {2}{W}{W}, >3WU). Returns None when the value belongs
/// on the legacy containment path or the substring fallback.
/// m:{…} brace-spec parser (wave-4 literal-symbol semantics; see Mp).
/// Returns None when a non-brace remainder makes the spec legacy-shaped.
fn mana_brace_spec(v: &str) -> Option<Filter> {
    let (_, rest) = split_op(v);
    let s: String = rest.chars().filter(|c| !c.is_whitespace()).collect();
    if s.is_empty() {
        return None;
    }
    let chars: Vec<char> = s.chars().collect();
    let mut parts: Vec<Mp> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '{' {
            // unbraced leftovers (m:{2}W): let the legacy parser have it
            return None;
        }
        let end = chars[i..].iter().position(|&x| x == '}')? + i;
        let inner: String = chars[i + 1..end].iter().collect();
        i = end + 1;
        if inner.is_empty() {
            return None;
        }
        if inner.chars().all(|c| c.is_ascii_digit()) {
            let n: u32 = inner.parse().unwrap_or(0);
            if n == 0 {
                // upstream {0} matches every card that has a mana cost
                parts.push(Mp::AnyCost);
            } else {
                parts.push(Mp::GenericMin(n));
            }
            continue;
        }
        if inner.contains('/') || inner.contains('-') {
            parts.push(Mp::Lit(inner.replace('-', "/")));
            continue;
        }
        if inner == "x" || inner == "s" {
            parts.push(Mp::Lit(inner));
            continue;
        }
        if inner.chars().all(|c| c.is_ascii_alphabetic()) {
            if inner == "k" {
                // upstream rejects {K} as an invalid symbol
                return Some(Filter::Ignored("invalid mana symbol {K}".into()));
            }
            let b: u8 = match inner.as_str() {
                "w" => 1,
                "u" => 2,
                "b" => 4,
                "r" => 8,
                "g" => 16,
                _ => 0,
            };
            if b != 0 {
                parts.push(Mp::Pip(b));
            } else {
                // literal printed symbol: {C}, {Y}, {Z}, {L}, {HW}, …
                parts.push(Mp::Lit(inner));
            }
            continue;
        }
        return None;
    }
    if parts.is_empty() {
        return None;
    }
    Some(Filter::ManaSyms(parts))
}

fn mana_spec(v: &str) -> Option<Filter> {
    let (op, rest) = split_op(v);
    let s: String = rest.chars().filter(|c| !c.is_whitespace()).collect();
    if s.is_empty() {
        return None;
    }
    let chars: Vec<char> = s.chars().collect();
    let mut need: Vec<(u8, u8)> = Vec::new();
    let mut syms: Vec<String> = Vec::new();
    let mut gc = 0u8;
    let mut gc_given = false;
    let mut need_x = false;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' {
            let end = chars[i..].iter().position(|&x| x == '}')? + i;
            let inner: String = chars[i + 1..end].iter().collect::<String>().to_lowercase();
            i = end + 1;
            if inner.is_empty() {
                return None;
            }
            if inner.chars().all(|x| x.is_ascii_digit()) {
                gc += inner.parse::<u8>().unwrap_or(0);
                gc_given = true;
                continue;
            }
            if inner == "x" {
                need_x = true;
                continue;
            }
            if inner == "s" {
                syms.push("s".to_string());
                continue;
            }
            if inner.contains('/') || inner.contains('-') {
                syms.push(inner.replace('-', "/"));
                continue;
            }
            for ch in inner.chars() {
                let m = match ch {
                    'w' => 1u8,
                    'u' => 2,
                    'b' => 4,
                    'r' => 8,
                    'g' => 16,
                    _ => return None,
                };
                need.push((m, 1));
            }
            continue;
        }
        if c.is_ascii_digit() {
            let mut num = String::new();
            while i < chars.len() && chars[i].is_ascii_digit() {
                num.push(chars[i]);
                i += 1;
            }
            gc += num.parse::<u8>().unwrap_or(0);
            gc_given = true;
            continue;
        }
        match c.to_ascii_lowercase() {
            'x' => need_x = true,
            's' => syms.push("s".to_string()),
            'w' => need.push((1, 1)),
            'u' => need.push((2, 1)),
            'b' => need.push((4, 1)),
            'r' => need.push((8, 1)),
            'g' => need.push((16, 1)),
            _ => return None,
        }
        i += 1;
    }
    if !syms.is_empty() || need_x {
        // exact-symbol mode wins over everything else in the spec
        return Some(Filter::ManaExact { syms, need_x });
    }
    if gc_given && !need.is_empty() {
        return Some(Filter::ManaCover { need, gc, op });
    }
    if !need.is_empty() {
        // colour-only specs: containment with free generic (calibrated vs
        // upstream m:W/m:GU/m:2WW oracle-deduped totals: 2-10%).
        return Some(Filter::ManaCover { need, gc: 0, op });
    }
    None
}

/// m: / mana: multiset parsing. Returns (per-color reqs, generic, op) or None
/// for values containing garbage (→ caller falls back to substring Mana filter).
fn parse_mana_req(v: &str) -> Option<(Vec<(u8, u8)>, u8, Cmp)> {
    let (op, rest) = split_op(v);
    let mut need: Vec<(u8, u8)> = Vec::new();
    let mut gc = 0u8;
    let mut chars: Vec<char> = rest.chars().collect();
    chars.retain(|c| !c.is_whitespace());
    let mut i = 0usize;
    let mut colored_total = 0u8;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_digit() {
            let mut num = String::new();
            while i < chars.len() && chars[i].is_ascii_digit() {
                num.push(chars[i]);
                i += 1;
            }
            gc += num.parse::<u8>().unwrap_or(0);
            continue;
        }
        // symbol: single letter or {x}/{x/y}
        if c == '{' {
            let end = chars[i..].iter().position(|&x| x == '}')? + i;
            let inner: String = chars[i + 1..end].iter().collect();
            i = end + 1;
            if let Some((mask, cnt)) = mana_symbol(&inner) {
                if mask == 0 && cnt == 0 {
                    // colorless symbol like {2} handled above; {P} alone → skip
                }
                need.push((mask, cnt));
                colored_total += cnt;
            }
            continue;
        }
        // bare letter(s): "wu" etc. — treat each letter as one colored pip
        let up = c.to_ascii_uppercase();
        let mask = match up {
            'W' => 1,
            'U' => 2,
            'B' => 4,
            'R' => 8,
            'G' => 16,
            'S' => 31, // snow: any color
            'X' => {
                i += 1;
                continue;
            }
            _ => return None,
        };
        need.push((mask, 1));
        colored_total += 1;
        i += 1;
    }
    let _ = colored_total;
    if need.is_empty() && gc == 0 {
        return None;
    }
    Some((need, gc, op))
}

/// Parse one brace-delimited symbol body: "W", "W/U", "W/P", "2", "S".
fn mana_symbol(inner: &str) -> Option<(u8, u8)> {
    if inner.chars().all(|c| c.is_ascii_digit()) {
        return None; // generic handled by caller
    }
    let mut mask = 0u8;
    for ch in inner.chars() {
        match ch.to_ascii_uppercase() {
            'W' => mask |= 1,
            'U' => mask |= 2,
            'B' => mask |= 4,
            'R' => mask |= 8,
            'G' => mask |= 16,
            'S' | 'P' => {}
            '/' | '-' => {}
            _ => {}
        }
    }
    if mask == 0 {
        Some((0, 0))
    } else {
        Some((mask, 1))
    }
}

/// devotion: parsing — symbol list (each symbol contributes its color set) or
/// a bare number (total colored pips).
fn parse_devotion(v: &str) -> Option<(Vec<(u8, u8)>, Option<u8>)> {
    let t: String = v.chars().filter(|c| !c.is_whitespace()).collect();
    if t.chars().all(|c| c.is_ascii_digit()) && !t.is_empty() {
        return Some((Vec::new(), t.parse::<u8>().ok()));
    }
    let mut reqs: Vec<(u8, u8)> = Vec::new();
    let mut rest = t.as_str();
    while !rest.is_empty() {
        if rest.starts_with('{') {
            let end = rest.find('}')?;
            let inner = &rest[1..end];
            rest = &rest[end + 1..];
            let mut mask = 0u8;
            for ch in inner.chars() {
                match ch.to_ascii_uppercase() {
                    'W' => mask |= 1,
                    'U' => mask |= 2,
                    'B' => mask |= 4,
                    'R' => mask |= 8,
                    'G' => mask |= 16,
                    _ => {}
                }
            }
            if mask != 0 {
                reqs.push((mask, 1));
            }
        } else {
            let ch = rest.chars().next()?;
            rest = &rest[ch.len_utf8()..];
            let mask = match ch.to_ascii_uppercase() {
                'W' => Some(1),
                'U' => Some(2),
                'B' => Some(4),
                'R' => Some(8),
                'G' => Some(16),
                _ => None,
            };
            if let Some(m) = mask {
                reqs.push((m, 1));
            }
        }
    }
    if reqs.is_empty() {
        None
    } else {
        Some((reqs, None))
    }
}

/// produces: value parsing → bitmask (WUBRG | 32 colorless | 64 any).
fn parse_produces(v: &str) -> u8 {
    let t = v.replace('{', "").replace('}', "").replace(' ', "");
    match t.as_str() {
        "any" | "all" => 64,
        "c" | "colorless" => 32,
        _ => {
            let mut mask = 0u8;
            for ch in t.chars() {
                match ch.to_ascii_uppercase() {
                    'W' => mask |= 1,
                    'U' => mask |= 2,
                    'B' => mask |= 4,
                    'R' => mask |= 8,
                    'G' => mask |= 16,
                    _ => {}
                }
            }
            if mask == 0 {
                64
            } else {
                mask
            }
        }
    }
}

/// Parse a query string into an AST (top-level AND of or-expressions).
pub fn parse(q: &str) -> (Vec<Node>, Vec<String>) {
    parse_with(q, None)
}

/// `kw_enum` = Index::kw_enum (closed keyword phrase space). When present,
/// kw:/keyword: values are validated at PARSE time the way Scryfall drops
/// invalid-expression terms: unknown value -> Filter::Ignored (pruned from
/// AND/OR, 400 only via the all-terms-ignored rule). Proven against the
/// golden keyword family: `keyword:Level Up` (unquoted tokenizer split) drops
/// keyword:Level and answers the leftover word as text-search (333 = names
/// containing "up"), and `keyword:Double` = 138 is the exact "Double"
/// keyword, not the word-prefix union (288).
pub fn parse_with(
    q: &str,
    kw_enum: Option<&std::collections::HashSet<String>>,
) -> (Vec<Node>, Vec<String>) {
    let mut p = Parser {
        toks: tokenize(q),
        pos: 0,
        warnings: Vec::new(),
        kwenum: kw_enum.cloned(),
    };
    let mut nodes = Vec::new();
    while p.peek().is_some() {
        let before = p.pos;
        let n = p.parse_or();
        // guard against no-progress (e.g. stray ')')
        if p.pos == before {
            p.next();
        }
        nodes.push(n);
    }
    // Upstream invalid-expression model (probed live 2026-10-26): a bad
    // term is DROPPED with a warning and the rest of the query answers
    // (`k:flying t:instant` -> 200/3916 == plain t:instant); Scryfall only
    // 400s when EVERY term was dropped ("All of your terms were ignored.").
    // Filter::Ignored marks such terms at parse time; here we prune them
    // from the AST and emit the \u{1} sentinel (api.rs -> 400) when nothing
    // real survives. Fatal400 (true syntax errors like `!(`) is NOT pruned:
    // upstream 400s those even with valid neighbours.
    let mut kept: Vec<Node> = Vec::new();
    let mut had_term = false;
    for n in nodes {
        if !matches!(n, Node::All) {
            had_term = true;
        }
        if let Some(x) = prune_ignored(n) {
            kept.push(x);
        }
    }
    if had_term && kept.is_empty() {
        p.warnings
            .push("\u{1}All of your terms were ignored.".to_string());
    }
    (kept, p.warnings)
}

/// Drop invalid-expression terms the way Scryfall does: removed from AND
/// conjunctions and OR disjunctions, `!bad` collapses to bad, and a branch
/// whose every leaf was dropped collapses to "fully ignored" (None).
fn prune_ignored(n: Node) -> Option<Node> {
    match n {
        Node::Filter(Filter::Ignored(msg)) => {
            let _ = msg;
            None
        }
        Node::And(parts) => {
            let kept: Vec<Node> = parts.into_iter().filter_map(prune_ignored).collect();
            match kept.len() {
                0 => None,
                1 => kept.into_iter().next(),
                _ => Some(Node::And(kept)),
            }
        }
        Node::Or(parts) => {
            let kept: Vec<Node> = parts.into_iter().filter_map(prune_ignored).collect();
            match kept.len() {
                0 => None,
                1 => kept.into_iter().next(),
                _ => Some(Node::Or(kept)),
            }
        }
        Node::Not(inner) => prune_ignored(*inner).map(|x| Node::Not(Box::new(x))),
        other => Some(other),
    }
}


/* Scryfall Local — frontend for the local axum backend.
 * Vanilla JS, no build step, no external CDNs.
 * API contract (frozen):
 *   GET /api/cards/search?q=&limit=&page=&order=name|cmc|rarity|set&dir=asc|desc
 *     -> { total_cards, has_more, data:[card...], took_ms }
 *   GET /api/suggest?q= -> { suggestions:[{name,set,type_line,image_small,...}] }
 * Card fields accepted in either serialization (image_uris{} | image_url,
 * legalities{} | legal string, colors array | "RG" | "R,G", etc). */
"use strict";

const SEARCH_ENDPOINT = "/api/cards/search";
const SUGGEST_ENDPOINT = "/api/suggest";
const SUGGEST_DEBOUNCE_MS = 160;      // autocomplete only — never triggers /api/cards/search
const SUBMIT_DEDUPE_MS = 500;         // swallow rapid double-taps of the same submit

/* ---------------- state ---------------- */
const state = {
  q: "",                 // free-text part of the query (search box content)
  order: "name",
  dir: "",               // "" = auto
  page: 1,
  limit: 60,
  filters: { colors: [], type: "", rarity: "", legal: "", game: "", set: "", cmcMin: "", cmcMax: "", keyword: "", oracle: "" },
  requestSeq: 0,
};

/* ---------------- dom ---------------- */
const $ = (id) => document.getElementById(id);
const els = {
  headerForm: $("header-search-form"), headerInput: $("header-search-field"),
  homePage: $("homepage"), homeForm: $("homepage-form"), homeInput: $("homepage-search-field"),
  controls: $("search-controls"), controlsBottom: $("search-controls-bottom"),
  order: $("order-select"), dir: $("dir-select"), limit: $("limit-select"),
  pagTop: $("pagination-top"), pagBottom: $("pagination-bottom"),
  info: $("search-info"), infoText: $("search-info-text"),
  gridWrap: $("card-grid-wrap"), grid: $("card-grid-inner"),
  lastPage: $("last-page"), notice: $("notice"),
  menuBtn: $("menu-button"), mobileNav: $("mobile-nav"),
  acHeader: $("autocomplete"), acHome: $("autocomplete-home"),
  panel: $("filter-panel"), scrim: $("scrim"),
  filterBadge: $("filter-badge"),
  syntaxPop: $("syntax-pop"),
  backdrop: $("modal-backdrop"), modalImage: $("modal-image"),
  modalInfo: $("modal-info"), modalClose: $("modal-close"),
};

const FKEYS = ["type", "rarity", "legal", "game", "set", "cmcMin", "cmcMax", "keyword", "oracle"];

/* ---------------- tiny helpers ---------------- */
const el = (tag, cls, text) => {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text != null) n.textContent = text;
  return n;
};
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const fmtNum = (n) => Number(n).toLocaleString("en-US");

/* ---------------- card normalization (defensive) ---------------- */
function colorList(v) {
  if (Array.isArray(v)) return v.map((c) => String(c).toUpperCase().trim()).filter((c) => "WUBRG".includes(c));
  return [...String(v || "").toUpperCase()].filter((c) => "WUBRG".includes(c));
}
function kwList(v) {
  return Array.isArray(v) ? v.map((k) => String(k).trim()).filter(Boolean)
    : String(v || "").split(",").map((k) => k.trim()).filter(Boolean);
}
function pickImg(o, fallbackKeys) {
  if (typeof o === "string" && o) return o;
  if (o && typeof o === "object") {
    for (const k of fallbackKeys) if (o[k]) return o[k];
  }
  return null;
}
function firstImg(raw) {
  return pickImg(raw.image_url, ["normal", "large", "border_image", "small"])
    || pickImg(raw.image_uris, ["normal", "large", "border_image", "small"]);
}
function smallImg(raw) {
  return pickImg(raw.image_uris, ["small", "normal", "large"])
    || pickImg(raw.image_url, ["small", "normal", "large"]);
}
function legalPairs(raw) {
  // accepts legalities{} map or `legal:"a,b"` string
  if (raw.legalities && typeof raw.legalities === "object" && !Array.isArray(raw.legalities)) {
    return Object.entries(raw.legalities)
      .filter(([, v]) => v === "legal" || v === "banned" || v === "restricted");
  }
  if (typeof raw.legal === "string") {
    return raw.legal.split(",").map((s) => s.trim()).filter(Boolean).map((f) => [f, "legal"]);
  }
  return [];
}
function normalizeCard(raw) {
  const str = (v) => (v == null ? "" : String(v));
  return {
    id: str(raw.id) || str(raw.oracle_id) || str(raw.name),
    name: str(raw.name) || "(unnamed card)",
    typeLine: str(raw.type_line),
    manaCost: str(raw.mana_cost),
    cmc: typeof raw.cmc === "number" ? raw.cmc : (parseFloat(raw.cmc) || 0),
    oracleText: str(raw.oracle_text),
    power: raw.power == null ? null : str(raw.power),
    toughness: raw.toughness == null ? null : str(raw.toughness),
    loyalty: raw.loyalty == null ? null : str(raw.loyalty),
    rarity: (str(raw.rarity) || "common").toLowerCase(),
    set: (str(raw.set) || str(raw.set_code)).toLowerCase(),
    setName: str(raw.set_name),
    collectorNumber: str(raw.collector_number),
    colors: colorList(raw.colors),
    colorIdentity: colorList(raw.color_identity != null ? raw.color_identity : raw.colors),
    keywords: kwList(raw.keywords),
    layout: str(raw.layout) || "normal",
    imageUrl: firstImg(raw),
    thumbUrl: smallImg(raw),
    uri: str(raw.scryfall_uri) || null,
    artist: str(raw.artist),
    legalities: legalPairs(raw),
  };
}

/* ---------------- query building ---------------- */
function buildFullQuery() {
  const f = state.filters;
  const tokens = [];
  const base = state.q.trim();
  if (base) tokens.push(base);
  for (const c of f.colors) tokens.push("c:" + c.toLowerCase());
  if (f.type) tokens.push('t:"' + f.type.replace(/"/g, "") + '"');
  if (f.rarity) tokens.push("r:" + f.rarity);
  if (f.legal) tokens.push("legal:" + f.legal);
  if (f.game) tokens.push("m:" + f.game);
  if (f.set) tokens.push("s:" + f.set.replace(/\s+/g, ""));
  if (f.cmcMin !== "") tokens.push("cmc>=" + f.cmcMin);
  if (f.cmcMax !== "") tokens.push("cmc<=" + f.cmcMax);
  if (f.keyword) tokens.push('k:"' + f.keyword.replace(/"/g, "") + '"');
  if (f.oracle) tokens.push('o:"' + f.oracle.replace(/"/g, "") + '"');
  return tokens.join(" ");
}
function activeFilterCount() {
  const f = state.filters;
  let n = f.colors.length;
  for (const k of FKEYS) if (String(f[k]).trim()) n++;
  return n;
}

/* human-readable "where …" summary from the composed query */
function describeQuery(q) {
  if (!q) return "";
  const parts = [];
  const quoted = [];
  let work = q.replace(/"([^"]*)"/g, (_, inner) => { quoted.push(inner); return "\u0000" + (quoted.length - 1) + "\u0000"; });
  const restore = (s) => s.replace(/\u0000(\d+)\u0000/g, (_, i) => quoted[+i]);
  const re = /(?:^|\s)([a-z_]+)\s*(>=|<=|>|<|:)\s*(\S+)/gi;
  let m;
  const matchedRanges = [];
  const add = (label) => parts.push(label);
  while ((m = re.exec(work)) !== null) {
    const key = m[1].toLowerCase(), op = m[2], val = restore(m[3]);
    const neg = /!\s*$/.test(work.slice(Math.max(0, m.index - 2), m.index + 1));
    matchedRanges.push([m.index, m.index + m[0].length]);
    const v = `<i>\u201C${esc(val)}\u201D</i>`;
    const notWord = neg ? "not " : "";
    const opWord = { ">=": `is at least ${esc(val)}`, "<=": `is at most ${esc(val)}`, ">": `is greater than ${esc(val)}`, "<": `is less than ${esc(val)}` }[op] || `is ${op}${esc(val)}`;
    switch (key) {
      case "t": case "type": add(`${notWord}card types include ${v}`); break;
      case "o": case "oracle": case "text": add(`${notWord}rules text includes ${v}`); break;
      case "k": case "keyword": add(`${notWord}keywords include ${v}`); break;
      case "c": case "identity": add(`${notWord}color identity includes ${v}`); break;
      case "m": case "color": add(neg ? `${notWord}colors exclude ${v}` : (key === "m" ? `colors are exactly ${v}` : `colors include ${v}`)); break;
      case "r": case "rarity": add(`rarity is ${v}`); break;
      case "legal": add(`format legalities include ${v}`); break;
      case "s": case "set": case "e": case "expansion": add(`set is ${v}`); break;
      case "cmc": case "mv": add(`mana value ${op === ":" ? "is " + v : opWord}`); break;
      case "pow": case "power": add(`power ${op === ":" ? "is " + v : opWord}`); break;
      case "tou": case "toughness": add(`toughness ${op === ":" ? "is " + v : opWord}`); break;
      case "is": add(`card is ${v}`); break;
      case "artist": add(`artist is ${v}`); break;
      case "or": break;
      default: add(`${key} ${op} ${v}`);
    }
  }
  let rest = work;
  for (const [a, b] of matchedRanges.reverse()) rest = rest.slice(0, a) + " " + rest.slice(b);
  rest = restore(rest).replace(/\s+/g, " ").trim();
  if (rest) {
    const words = rest.split(/\s+/).filter((w) => w && !/^\(|\)$/.test(w));
    for (const w of words) add(`name includes <i>\u201C${esc(w)}\u201D</i>`);
  }
  if (!parts.length) return "";
  return " where the " + parts.join(" and the ");
}

/* ---------------- chrome helpers ---------------- */
function renderSkeletons(n) {
  els.grid.setAttribute("aria-busy", "true");
  els.grid.innerHTML = "";
  const frag = document.createDocumentFragment();
  for (let i = 0; i < n; i++) {
    const item = el("div", "card-grid-item is-skeleton");
    const card = el("div", "card-grid-item-card");
    card.appendChild(el("div", "shimmer"));
    item.appendChild(card);
    frag.appendChild(item);
  }
  els.grid.appendChild(frag);
}

function showPageChrome(on) {
  els.controls.hidden = !on;
  els.controlsBottom.hidden = !on;
  els.info.hidden = !on;
  els.gridWrap.hidden = !on;
  els.homePage.hidden = on;
  const mainEl = document.getElementById("main");
  if (mainEl) mainEl.classList.toggle("main-home", !on);
}

function showNotice(html, isError) {
  if (!html) { els.notice.hidden = true; return; }
  els.notice.hidden = false;
  els.notice.className = "notice" + (isError ? " error" : "");
  els.notice.innerHTML = html;
}

/* ---------------- grid + pagination ---------------- */
function renderResults(cards) {
  els.grid.setAttribute("aria-busy", "false");
  els.grid.innerHTML = "";
  if (!cards.length) {
    const item = el("div", "card-grid-item");
    item.style.width = "100%";
    const p = el("p", "last-page", "No cards match your search. Try fewer words — or a little luck.");
    p.style.margin = "30px auto 40px";
    p.style.color = "#535353";
    item.appendChild(p);
    els.grid.appendChild(item);
    return;
  }
  const frag = document.createDocumentFragment();
  for (const card of cards) frag.appendChild(renderCardItem(card));
  els.grid.appendChild(frag);
  const rem = cards.length % 4;
  if (rem) for (let i = 0; i < 4 - rem; i++) els.grid.appendChild(el("div", "card-grid-item flexbox-spacer"));
}

function renderCardItem(card) {
  const item = el("div", "card-grid-item");
  const btn = el("button", "card-grid-item-card");
  btn.type = "button";
  btn.setAttribute("aria-label", `${card.name} — ${card.typeLine}`);
  const src = card.thumbUrl || card.imageUrl;
  if (src) {
    const img = document.createElement("img");
    img.loading = "lazy";
    img.decoding = "async";
    img.referrerPolicy = "no-referrer";
    img.alt = `${card.name} card image`;
    img.src = src;
    img.addEventListener("error", () => {
      if (card.imageUrl && img.src !== card.imageUrl) img.src = card.imageUrl;
      else img.remove();
    });
    btn.appendChild(img);
  }
  btn.addEventListener("click", () => openModal(card));
  item.appendChild(btn);
  return item;
}

const ARROW_LEFT = '<svg viewBox="0 0 14 14" aria-hidden="true"><path d="M9 2 4 7l5 5" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>';
const ARROW_RIGHT = '<svg viewBox="0 0 14 14" aria-hidden="true"><path d="M5 2l5 5-5 5" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>';
const ARROWS_L = '<svg viewBox="0 0 16 14" aria-hidden="true"><path d="M8 2 3 7l5 5M13 2l-5 5 5 5" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>';
const ARROWS_R = '<svg viewBox="0 0 16 14" aria-hidden="true"><path d="M3 2l5 5-5 5M8 2l5 5-5 5" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>';

function pagBtn({ label, icon, iconAfter = true, disabled, onclick }) {
  const a = document.createElement(disabled ? "span" : "button");
  a.className = "button-n" + (disabled ? " disabled" : "") + (!label ? " icon-only" : "");
  if (!disabled) { a.type = "button"; a.addEventListener("click", onclick); }
  else a.setAttribute("aria-hidden", "true");
  const putIcon = () => {
    if (!icon) return;
    const s = document.createElement("span");
    s.innerHTML = icon;
    a.appendChild(s.firstChild);
  };
  if (!iconAfter) putIcon();
  if (label) a.appendChild(el("b", null, label));
  if (iconAfter) putIcon();
  return a;
}

function renderPagination(env) {
  const pages = Math.max(1, Math.ceil(env.total / state.limit));
  const build = (target) => {
    target.innerHTML = "";
    target.appendChild(pagBtn({ icon: ARROWS_L, disabled: state.page <= 1, onclick: () => goPage(1) }));
    target.appendChild(pagBtn({ label: "Previous", icon: ARROW_LEFT, iconAfter: false, disabled: state.page <= 1, onclick: () => goPage(state.page - 1) }));
    target.appendChild(pagBtn({ label: `Next ${state.limit}`, icon: ARROW_RIGHT, disabled: !env.hasMore, onclick: () => goPage(state.page + 1) }));
    target.appendChild(pagBtn({ icon: ARROWS_R, disabled: !env.hasMore, onclick: () => goPage(pages) }));
    const span = el("span", "controls-label", `page ${fmtNum(state.page)} of ${fmtNum(pages)}`);
    span.style.marginLeft = "10px";
    target.appendChild(span);
  };
  build(els.pagTop);
  build(els.pagBottom);
}

const QUIPS = [
  "It is the moment of discovery, the triumph of the mind, and the end of these results.",
  "You have reached the end of the results. The deck must now be built.",
  "Every library ends. This one ended here.",
  "That is the last card. Shuffle and seek again.",
  "The oracle has spoken. No more results await.",
];
function setQuip(q) {
  let h = 0;
  for (let i = 0; i < q.length; i++) h = (h * 31 + q.charCodeAt(i)) >>> 0;
  els.lastPage.textContent = QUIPS[h % QUIPS.length];
  els.lastPage.hidden = false;
}

/* ---------------- main search flow ---------------- */
function syncUrl() {
  const p = new URLSearchParams();
  if (state.q) p.set("q", state.q);
  const f = state.filters;
  if (f.colors.length) p.set("fc", f.colors.join(""));
  if (f.type) p.set("ft", f.type);
  if (f.rarity) p.set("fr", f.rarity);
  if (f.legal) p.set("fl", f.legal);
  if (f.game) p.set("fm", f.game);
  if (f.set) p.set("fs", f.set);
  if (f.cmcMin !== "") p.set("fmin", f.cmcMin);
  if (f.cmcMax !== "") p.set("fmax", f.cmcMax);
  if (f.keyword) p.set("fk", f.keyword);
  if (f.oracle) p.set("fo", f.oracle);
  if (state.order !== "name") p.set("order", state.order);
  if (state.dir) p.set("dir", state.dir);
  if (state.limit !== 60) p.set("limit", state.limit);
  if (state.page > 1) p.set("page", state.page);
  const qs = p.toString();
  history.replaceState(null, "", qs ? `?${qs}` : location.pathname);
  const full = buildFullQuery();
  document.title = full ? `${full} · Scryfall Local` : "Scryfall Local — Magic: The Gathering Card Search";
}

async function search() {
  syncUrl();
  updateFilterBadge();

  const fullQ = buildFullQuery();
  if (!fullQ) {                       // no query at all → homepage hero
    showPageChrome(false);
    els.lastPage.hidden = true;
    showNotice(null);
    return;
  }

  showPageChrome(true);
  els.infoText.textContent = "";
  renderSkeletons(Math.min(Math.max(state.limit, 24), 60));

  const p = new URLSearchParams();
  p.set("q", fullQ);
  p.set("limit", String(state.limit));
  if (state.page > 1) p.set("page", String(state.page));
  p.set("order", state.order);
  if (state.dir) p.set("dir", state.dir);

  const seq = ++state.requestSeq;
  try {
    const res = await fetch(`${SEARCH_ENDPOINT}?${p}`, { headers: { Accept: "application/json" } });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const json = await res.json();
    if (seq !== state.requestSeq) return;   // stale response

    const cards = (Array.isArray(json && json.data) ? json.data : []).map(normalizeCard);
    const total = Number.isFinite(json.total_cards) ? json.total_cards : cards.length;
    const hasMore = typeof json.has_more === "boolean" ? json.has_more : cards.length >= state.limit;

    showNotice(null);
    const from = (state.page - 1) * state.limit + 1;
    const to = (state.page - 1) * state.limit + cards.length;
    const countTxt = total > state.limit || state.page > 1
      ? `${fmtNum(from)} \u2013 ${fmtNum(to)} of ${fmtNum(total)} cards`
      : `${fmtNum(total)} card${total === 1 ? "" : "s"}`;
    els.infoText.innerHTML = `<strong>${countTxt}</strong>` + describeQuery(fullQ) +
      (Number.isFinite(json.took_ms) ? ` <span class="took">(${json.took_ms.toFixed(1)} ms)</span>` : "");

    renderResults(cards);
    renderPagination({ total, hasMore });
    setQuip(fullQ);
  } catch (err) {
    if (seq !== state.requestSeq) return;
    els.grid.innerHTML = "";
    els.pagTop.innerHTML = "";
    els.pagBottom.innerHTML = "";
    els.infoText.textContent = "";
    showNotice(`The search API is unreachable (<code>${esc(err.message)}</code>). The server may be restarting — try again in a few seconds.`, true);
  }
}

function goPage(n) {
  state.page = Math.max(1, n);
  search();
  window.scrollTo({ top: 0 });
}

function requestSearch(resetPage = true) {
  if (resetPage) state.page = 1;
  search();
}

/* Single deterministic entry point for explicit submissions (Enter, Find Cards
 * tap, suggestion pick, filter/example buttons). Typing NEVER calls this.
 * Identical submissions inside SUBMIT_DEDUPE_MS are swallowed, so a double
 * tap of the submit button or a touchstart+click pair on a suggestion fires
 * exactly one /api/cards/search. */
let lastSubmit = { key: "", at: 0 };
function submitSearch() {
  const now = Date.now();
  const key = JSON.stringify([buildFullQuery(), state.order, state.dir, state.limit, state.page]);
  if (key === lastSubmit.key && now - lastSubmit.at < SUBMIT_DEDUPE_MS) return false;
  lastSubmit = { key, at: now };
  requestSearch(true);
  return true;
}

/* ---------------- autocomplete ---------------- */
function attachAutocomplete(input, box) {
  // per-instance state: the header and homepage inputs must not shadow each other
  let acItems = [], acActive = -1, acTimer = null, acSeq = 0;
  let lastPick = { name: "", at: 0 };

  const isOpen = () => !box.hidden && acItems.length > 0;
  const close = () => { box.hidden = true; box.innerHTML = ""; acItems = []; acActive = -1; };
  const choose = (name) => {
    // one submission per pick, no matter which pointer path fired it
    const now = Date.now();
    if (name === lastPick.name && now - lastPick.at < SUBMIT_DEDUPE_MS) return;
    lastPick = { name, at: now };
    close();
    setInputs(name);
    state.q = name;
    submitSearch();
  };

  input.addEventListener("input", () => {
    if (input.composing) return;
    clearTimeout(acTimer);
    const v = input.value.trim();
    if (v.length < 2) { close(); return; }
    acTimer = setTimeout(async () => {
      const seq = ++acSeq;
      try {
        const res = await fetch(`${SUGGEST_ENDPOINT}?${new URLSearchParams({ q: v })}`, { headers: { Accept: "application/json" } });
        if (!res.ok) return;
        let json; try { json = await res.json(); } catch (_) { return; }
        if (seq !== acSeq) return;
        const list = (Array.isArray(json) ? json : (json.suggestions || json.data || [])).slice(0, 8);
        box.innerHTML = "";
        if (!list.length) { close(); return; }
        acItems = []; acActive = -1;
        for (const s of list) {
          const name = s.name || s.value || "";
          if (!name) continue;
          const row = el("div", "ac-item");
          row.setAttribute("role", "option");
          if (s.image_small || s.image) {
            const img = document.createElement("img");
            img.loading = "lazy"; img.alt = "";
            img.src = s.image_small || s.image;
            row.appendChild(img);
          }
          const mid = el("div");
          mid.appendChild(el("div", "ac-name", name));
          if (s.type_line) mid.appendChild(el("div", "ac-type", s.type_line));
          row.appendChild(mid);
          if (s.set) row.appendChild(el("span", "ac-set", String(s.set).toUpperCase()));
          /* touch hygiene: handle the tap on touchstart and preventDefault it.
           * That keeps focus on the input (no blur→close race), suppresses the
           * browser's synthesized mousedown/mouseup/click pair (no 300ms ghost
           * double-fire), and pick() itself is time-deduped as a backstop. */
          let touched = false;
          row.addEventListener("touchstart", (e) => {
            e.preventDefault();
            touched = true;
            choose(name);
          }, { passive: false });
          row.addEventListener("mousedown", (e) => {
            e.preventDefault();            // keep input focused while picking by mouse
            if (touched) return;
            choose(name);
          });
          row.addEventListener("click", (e) => {
            e.preventDefault();
            if (touched) return;
            choose(name);                  // safety net for pointer devices with no mousedown
          });
          box.appendChild(row);
          acItems.push(row);
        }
        box.hidden = false;
      } catch (_) { /* suggest is best-effort */ }
    }, SUGGEST_DEBOUNCE_MS);
  });
  input.addEventListener("blur", () => setTimeout(() => { if (!input.composing) close(); }, 120));
  input.addEventListener("keydown", (e) => {
    /* IME / on-screen keyboard guard: Enter that fires while the keyboard is
     * still composing or autocorrecting must do NOTHING — no submit, no pick. */
    if (e.key === "Enter" && (e.isComposing || e.keyCode === 229 || input.composing)) {
      e.preventDefault();
      return;
    }
    if (!isOpen()) return;                 // overlay closed → Enter is a plain explicit submit
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      acActive += e.key === "ArrowDown" ? 1 : -1;
      if (acActive < 0) acActive = acItems.length - 1;
      if (acActive >= acItems.length) acActive = 0;
      acItems.forEach((r, i) => r.classList.toggle("active", i === acActive));
      input.value = acItems[acActive].querySelector(".ac-name").textContent;
      state.q = input.value.trim();
      setInputs(input.value);
    } else if (e.key === "Enter") {
      if (acActive >= 0 && acActive < acItems.length) {
        // highlighted suggestion wins: pick it exactly once, do not submit raw text
        e.preventDefault();
        choose(acItems[acActive].querySelector(".ac-name").textContent);
      }
      // no active highlight → let the form's submit handler run (explicit Enter search)
    } else if (e.key === "Escape") {
      close();
    }
  });
}

/* ---------------- filter panel ---------------- */
function syncPanelControls() {
  document.querySelectorAll(".color-pip").forEach((b) =>
    b.setAttribute("aria-pressed", state.filters.colors.includes(b.dataset.color) ? "true" : "false"));
  $("f-type").value = state.filters.type;
  $("f-rarity").value = state.filters.rarity;
  $("f-legal").value = state.filters.legal;
  $("f-game").value = state.filters.game;
  $("f-set").value = state.filters.set;
  $("f-cmc-min").value = state.filters.cmcMin;
  $("f-cmc-max").value = state.filters.cmcMax;
  $("f-keyword").value = state.filters.keyword;
  $("f-oracle").value = state.filters.oracle;
  $("color-hint").textContent = state.filters.colors.length > 1 ? "identity contains all" : "identity contains";
}
function updateFilterBadge() {
  const n = activeFilterCount();
  els.filterBadge.hidden = n === 0;
  els.filterBadge.textContent = n;
}
function openPanel() { syncPanelControls(); els.panel.hidden = false; els.scrim.hidden = false; els.syntaxPop.hidden = true; }
function closePanel() { els.panel.hidden = true; els.scrim.hidden = true; }

document.querySelectorAll(".color-pip").forEach((btn) => {
  btn.addEventListener("click", () => {
    const c = btn.dataset.color;
    const i = state.filters.colors.indexOf(c);
    if (i >= 0) state.filters.colors.splice(i, 1); else state.filters.colors.push(c);
    btn.setAttribute("aria-pressed", i >= 0 ? "false" : "true");
    $("color-hint").textContent = state.filters.colors.length > 1 ? "identity contains all" : "identity contains";
  });
});
$("filter-clear").addEventListener("click", () => {
  state.filters = { colors: [], type: "", rarity: "", legal: "", game: "", set: "", cmcMin: "", cmcMax: "", keyword: "", oracle: "" };
  syncPanelControls();
  submitSearch();
});
$("filter-apply").addEventListener("click", () => {
  const f = state.filters;
  f.type = $("f-type").value.trim();
  f.rarity = $("f-rarity").value;
  f.legal = $("f-legal").value;
  f.game = $("f-game").value;
  f.set = $("f-set").value.trim();
  f.cmcMin = $("f-cmc-min").value.trim();
  f.cmcMax = $("f-cmc-max").value.trim();
  f.keyword = $("f-keyword").value.trim();
  f.oracle = $("f-oracle").value.trim();
  closePanel();
  submitSearch();
});
$("filters-open").addEventListener("click", openPanel);
$("filters-open-2").addEventListener("click", openPanel);
$("home-filters").addEventListener("click", openPanel);
$("filter-close").addEventListener("click", closePanel);
els.scrim.addEventListener("click", closePanel);
["f-type", "f-set", "f-keyword", "f-oracle"].forEach((id) =>
  $(id).addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); $("filter-apply").click(); } }));

/* ---------------- syntax popover ---------------- */
function toggleSyntax() {
  els.syntaxPop.hidden = !els.syntaxPop.hidden;
  if (!els.syntaxPop.hidden) { els.panel.hidden = true; els.scrim.hidden = true; }
}
$("syntax-open").addEventListener("click", toggleSyntax);
$("home-syntax").addEventListener("click", toggleSyntax);
document.addEventListener("click", (e) => {
  if (!els.syntaxPop.hidden && !els.syntaxPop.contains(e.target) &&
      e.target.id !== "syntax-open" && e.target.id !== "home-syntax" &&
      !(e.target.closest && (e.target.closest("#syntax-open") || e.target.closest("#home-syntax")))) {
    els.syntaxPop.hidden = true;
  }
});

/* ---------------- card modal ---------------- */
function manaSpan(sym, sm) {
  const s = String(sym).toUpperCase();
  const cls = s.length === 1 && "WUBRG".includes(s) ? "ms-" + s : "ms-generic";
  return `<span class="mana-sym ${cls}${sm ? " sm" : ""}">${esc(s)}</span>`;
}
function manaLineHTML(cost) {
  return String(cost || "").replace(/\{([^}]*)\}/g, (_, sym) => manaSpan(sym, false));
}
function oracleHTML(text) {
  return esc(text).replace(/\{([^}]*)\}/g, (_, sym) => manaSpan(sym, true)).replace(/\n/g, "<br>");
}

function openModal(card) {
  els.modalImage.innerHTML = "";
  els.modalInfo.innerHTML = "";
  if (card.imageUrl) {
    const img = document.createElement("img");
    img.src = card.imageUrl;
    img.alt = `${card.name} card image`;
    img.referrerPolicy = "no-referrer";
    els.modalImage.appendChild(img);
  }
  const name = el("h2", "modal-name");
  name.innerHTML = `<span>${esc(card.name)}</span> <span class="mana-line">${manaLineHTML(card.manaCost)}</span>`;
  els.modalInfo.appendChild(name);
  els.modalInfo.appendChild(el("p", "modal-type", card.typeLine));
  if (card.oracleText) {
    const t = el("div", "modal-text");
    t.innerHTML = oracleHTML(card.oracleText);
    els.modalInfo.appendChild(t);
  }
  const stats = card.power && card.toughness ? `${card.power} / ${card.toughness}`
    : card.loyalty ? `Loyalty ${card.loyalty}` : "";
  const meta = (k, v) => {
    if (!v) return;
    const p = el("p", "modal-meta");
    p.innerHTML = `<b>${esc(k)}:</b> ${v}`;
    els.modalInfo.appendChild(p);
  };
  meta("Mana value", String(card.cmc));
  meta("Power/Toughness", stats);
  meta("Rarity", card.rarity.charAt(0).toUpperCase() + card.rarity.slice(1));
  meta("Set", `${esc(card.setName || card.set.toUpperCase())}${card.collectorNumber ? " " + esc(card.collectorNumber) : ""}`);
  meta("Artist", esc(card.artist));
  if (card.keywords.length) meta("Keywords", esc(card.keywords.join(", ")));

  if (card.legalities.length) {
    const chips = el("div", "legal-chips");
    for (const [fmt, status] of card.legalities) {
      chips.appendChild(el("span", "legal-chip " + status,
        `${fmt.replace(/([a-z])([A-Z])/g, "$1 $2")}: ${status.replace("_", " ")}`));
    }
    els.modalInfo.appendChild(chips);
  }
  const links = el("p", "modal-links");
  if (card.uri) {
    const a = el("a", null, "View on Scryfall \u2197");
    a.href = card.uri; a.target = "_blank"; a.rel = "noopener";
    links.appendChild(a);
    links.appendChild(document.createTextNode("   "));
  }
  if (card.imageUrl) {
    const a2 = el("a", null, "Full image \u2197");
    a2.href = card.imageUrl; a2.target = "_blank"; a2.rel = "noopener";
    links.appendChild(a2);
  }
  els.modalInfo.appendChild(links);

  els.backdrop.hidden = false;
  document.body.style.overflow = "hidden";
  els.modalClose.focus();
}
function closeModal() { els.backdrop.hidden = true; document.body.style.overflow = ""; }
els.modalClose.addEventListener("click", closeModal);
els.backdrop.addEventListener("click", (e) => { if (e.target === els.backdrop) closeModal(); });

/* ---------------- header + global wiring ---------------- */
function setInputs(v) { els.headerInput.value = v; els.homeInput.value = v; }

/* Typing only mirrors text + refreshes suggestions (attachAutocomplete).
 * /api/cards/search is NEVER fired from an input handler — submissions run
 * only via explicit Enter/Find Cards tap, suggestion pick, or init ?q=. */
function formSubmit(input) {
  return (e) => {
    e.preventDefault();
    if (input.composing) return;   // belt-and-braces: never submit mid-composition
    setInputs(input.value);
    state.q = input.value.trim();
    submitSearch();                // explicit submit: Enter key or Find Cards tap
  };
}
els.headerForm.addEventListener("submit", formSubmit(els.headerInput));
els.homeForm.addEventListener("submit", formSubmit(els.homeInput));
els.headerInput.addEventListener("input", () => {
  if (els.headerInput.composing) return;
  state.q = els.headerInput.value.trim();
  els.homeInput.value = els.headerInput.value;
});
els.homeInput.addEventListener("input", () => {
  if (els.homeInput.composing) return;
  state.q = els.homeInput.value.trim();
  els.headerInput.value = els.homeInput.value;
});
[els.headerInput, els.homeInput].forEach((inp) => {
  inp.addEventListener("compositionstart", () => { inp.composing = true; });
  inp.addEventListener("compositionend", () => { inp.composing = false; inp.dispatchEvent(new Event("input")); });
});

els.order.addEventListener("change", () => { state.order = els.order.value; requestSearch(true); });
els.dir.addEventListener("change", () => { state.dir = els.dir.value; requestSearch(true); });
els.limit.addEventListener("change", () => { state.limit = parseInt(els.limit.value, 10) || 60; requestSearch(true); });

/* menu button → dropdown links (like scryfall's Menu) */
els.menuBtn.addEventListener("click", () => { els.mobileNav.hidden = !els.mobileNav.hidden; });
document.querySelectorAll(".mobile-nav [data-mobile]").forEach((b) => {
  b.addEventListener("click", () => {
    els.mobileNav.hidden = true;
    const what = b.dataset.mobile;
    if (what === "filters") openPanel();
    else if (what === "syntax") toggleSyntax();
    else if (what === "clear") {
      state.q = "";
      state.filters = { colors: [], type: "", rarity: "", legal: "", game: "", set: "", cmcMin: "", cmcMax: "", keyword: "", oracle: "" };
      state.order = "name"; state.dir = ""; state.page = 1;
      els.order.value = "name"; els.dir.value = ""; els.limit.value = "60"; state.limit = 60;
      syncPanelControls();
      setInputs("");
      requestSearch(true);
    }
  });
});

/* homepage example buttons */
document.querySelectorAll("[data-example]").forEach((b) => {
  b.addEventListener("click", () => {
    state.q = b.dataset.example;
    setInputs(state.q);
    submitSearch();
  });
});

/* footer links that open overlays */
document.querySelectorAll("[data-open]").forEach((b) => {
  b.addEventListener("click", () => (b.dataset.open === "filters" ? openPanel() : toggleSyntax()));
});

/* keyboard shortcuts */
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    if (!els.backdrop.hidden) closeModal();
    else if (!els.panel.hidden) closePanel();
    else if (!els.syntaxPop.hidden) els.syntaxPop.hidden = true;
    else if (!els.mobileNav.hidden) els.mobileNav.hidden = true;
    return;
  }
  const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement.tagName);
  if (e.key === "/" && !typing) { e.preventDefault(); els.headerInput.focus(); els.headerInput.select(); return; }
  if (typing || !els.backdrop.hidden || !els.homePage.hidden) return;
  if (e.key === "ArrowLeft") {
    const prev = els.pagTop.querySelector(".button-n:nth-child(2)");
    if (prev && !prev.classList.contains("disabled")) prev.click();
  } else if (e.key === "ArrowRight") {
    const next = els.pagTop.querySelector(".button-n:nth-child(3)");
    if (next && !next.classList.contains("disabled")) next.click();
  }
});

/* ---------------- init from URL ---------------- */
function readUrlState() {
  const p = new URLSearchParams(location.search);
  state.q = p.get("q") || "";
  if (p.get("fc")) state.filters.colors = [...p.get("fc").toUpperCase()].filter((c) => "WUBRG".includes(c));
  state.filters.type = p.get("ft") || "";
  state.filters.rarity = p.get("fr") || "";
  state.filters.legal = p.get("fl") || "";
  state.filters.game = p.get("fm") || "";
  state.filters.set = p.get("fs") || "";
  state.filters.cmcMin = p.get("fmin") || "";
  state.filters.cmcMax = p.get("fmax") || "";
  state.filters.keyword = p.get("fk") || "";
  state.filters.oracle = p.get("fo") || "";
  if (["name", "cmc", "rarity", "set"].includes(p.get("order"))) state.order = p.get("order");
  if (["asc", "desc"].includes(p.get("dir"))) state.dir = p.get("dir");
  const lim = parseInt(p.get("limit"), 10);
  if ([30, 60, 120].includes(lim)) state.limit = lim;
  const pg = parseInt(p.get("page"), 10);
  if (Number.isFinite(pg) && pg > 0) state.page = pg;

  setInputs(state.q);
  els.order.value = state.order;
  els.dir.value = state.dir;
  els.limit.value = String(state.limit);
  syncPanelControls();
}

(function init() {
  readUrlState();
  attachAutocomplete(els.headerInput, els.acHeader);
  attachAutocomplete(els.homeInput, els.acHome);
  search();
})();

window.addEventListener("popstate", () => { readUrlState(); search(); });

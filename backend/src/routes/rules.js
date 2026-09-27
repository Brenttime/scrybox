// MTG rules reference: Comprehensive Rules + Commander rules, searchable in the
// Rules view.
//
//   GET /api/rules            cached JSON (refetched weekly)
//   GET /api/rules?refresh=1  force refetch
//
// Sources: the CR .txt linked from magic.wizards.com/en/rules and the official
// rules page on mtgcommander.net. Parsed once and cached next to the DB so a
// failed fetch falls back to the last good copy.
const express = require('express');
const fs = require('fs');
const path = require('path');
const axios = require('axios');

const router = express.Router();
const CACHE = process.env.RULES_CACHE || path.join(__dirname, '../../../database/mtg-rules.json');
const MAX_AGE = 7 * 86400 * 1000;
const UA = { 'User-Agent': 'Scrybox/1.0 (rules reference)' };
let mem = null;
let inflight = null;

function decode(buf) {
  const s = buf.toString('utf8');
  // Wizards' .txt has shipped as cp1252 before; fall back when UTF-8 is broken.
  return s.includes('\uFFFD') ? buf.toString('latin1') : s.replace(/^\uFEFF/, '');
}

async function fetchText(url) {
  const r = await axios.get(url, { headers: UA, responseType: 'arraybuffer', timeout: 30000 });
  return decode(Buffer.from(r.data));
}

function parseCR(txt) {
  const lines = txt.replace(/\r\n?/g, '\n').split('\n').map(l => l.trim());
  const out = [];
  const starts = lines.reduce((a, l, i) => (l === '1. Game Concepts' ? [...a, i] : a), []);
  let i = starts.length > 1 ? starts[1] : 0;
  let section = '';
  let gloss = false;
  while (i < lines.length) {
    const l = lines[i];
    if (!l) { i++; continue; }
    if (l === 'Glossary') { gloss = true; section = 'Glossary'; i++; continue; }
    if (l === 'Credits' && gloss) break;
    if (gloss) {
      const body = [];
      let j = i + 1;
      while (j < lines.length && lines[j]) body.push(lines[j++]);
      out.push({ src: 'Glossary', id: l, section: 'Glossary', text: body.join(' ') });
      i = j; continue;
    }
    let m;
    if ((m = l.match(/^(\d{3}\.\d+[a-z]?)\.?\s+(.*)$/))) {
      out.push({ src: 'Comprehensive', id: m[1], section, text: m[2] });
    } else if (/^\d{3}\.\s/.test(l)) {
      section = l;
      out.push({ src: 'Comprehensive', id: l.split('.')[0], section: l, text: l, header: true });
    } else if (/^\d\.\s/.test(l)) {
      section = l;
    } else if (l.startsWith('Example:') && out.length) {
      out[out.length - 1].text += '\n' + l;
    }
    i++;
  }
  return out;
}

const unescape = s => s
  .replace(/&#(\d+);/g, (_, n) => String.fromCodePoint(+n))
  .replace(/&#x([0-9a-f]+);/gi, (_, n) => String.fromCodePoint(parseInt(n, 16)))
  .replace(/&nbsp;/g, ' ').replace(/&quot;/g, '"').replace(/&apos;|&rsquo;|&lsquo;/g, "'")
  .replace(/&rdquo;|&ldquo;/g, '"').replace(/&ndash;/g, '-').replace(/&mdash;/g, '-')
  .replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&');

function parseCommander(page) {
  const m = page.match(/<div class="entry-content[^>]*>([\s\S]*?)<\/article/);
  const body = m ? m[1] : page;
  const out = [];
  let section = 'Commander';
  let n = 0;
  const re = /<(h[1-6]|p|li)[^>]*>([\s\S]*?)<\/\1>/g;
  let t;
  while ((t = re.exec(body))) {
    const text = unescape(t[2].replace(/<[^>]+>/g, '')).replace(/\s+/g, ' ').trim();
    if (!text) continue;
    if (t[1].startsWith('h')) { section = text; continue; }
    out.push({ src: 'Commander', id: `CMD ${++n}`, section, text });
  }
  return out;
}

async function build() {
  const idx = await fetchText('https://magic.wizards.com/en/rules');
  const urls = idx.match(/https:\/\/media\.wizards\.com\/[^"']*?\.txt/g);
  if (!urls) throw new Error('could not find Comprehensive Rules .txt link');
  const crUrl = urls[0].replace(/ /g, '%20');
  const cr = parseCR(await fetchText(crUrl));
  const cmdUrl = 'https://mtgcommander.net/index.php/rules/';
  const cmd = parseCommander(await fetchText(cmdUrl));
  if (cr.length < 1000 || cmd.length < 5) throw new Error(`parse sanity failed cr=${cr.length} cmd=${cmd.length}`);
  const ver = urls[0].match(/(\d{8})/);
  return { fetched: Date.now(), crUrl, crVersion: ver ? ver[1] : '', commanderUrl: cmdUrl, rules: [...cmd, ...cr] };
}

async function getRules(force) {
  if (!mem) { try { mem = JSON.parse(fs.readFileSync(CACHE, 'utf8')); } catch { mem = null; } }
  if (mem && !force && Date.now() - mem.fetched < MAX_AGE) return mem;
  inflight = inflight || build().finally(() => { inflight = null; });
  try {
    mem = await inflight;
    try { fs.writeFileSync(CACHE, JSON.stringify(mem)); } catch { /* cache is best-effort */ }
    return mem;
  } catch (err) {
    if (mem) return { ...mem, stale: err.message };
    throw err;
  }
}


// ---------------------------------------------------------------- keywords
// Every keyword action (CR 701) and keyword ability (CR 702) with its rules
// and the reminder text printed on real cards. Reminder text isn't in the CR;
// it's mined from card_cache.oracle_text ("Flying (This creature can't be
// blocked...)") and the most common wording wins. Keywords with a number or
// cost ("Ward {2}", "Toxic 1") get that normalised to N so they group.
let kwMem = null;
const norm = s => s.toLowerCase().replace(/[’']/g, "'").trim();

async function mineReminders(names) {
  const db = require('../db');
  const want = new Map(names.map(n => [norm(n), n]));
  const tally = new Map(); // key -> Map(reminder -> count)
  const cards = new Map();  // key -> count of cards
  const rows = await db.all("SELECT oracle_text FROM card_cache WHERE oracle_text LIKE '%(%'");
  // A reminder is "<Keyword>[ cost/N] (<text>)" at the start of a line. Take the
  // words before the paren and back off one word at a time until they name a
  // keyword, so "Cumulative upkeep {1}" and "Flying" both resolve.
  const re = /(?:^|\n)([A-Z][^\n(]{0,60}?) \(([^()]{8,400})\)/g;
  for (const { oracle_text: t } of rows) {
    const seen = new Set();
    for (const m of t.matchAll(re)) {
      const words = norm(m[1]).replace(/[—–-]/g, ' ').split(/\s+/).filter(Boolean);
      let key = '';
      for (let n = Math.min(words.length, 5); n > 0 && !key; n--) {
        const k = words.slice(0, n).join(' ');
        if (want.has(k)) key = k;
      }
      if (!key) continue;
      const rem = m[2];
      if (!tally.has(key)) tally.set(key, new Map());
      tally.get(key).set(rem, (tally.get(key).get(rem) || 0) + 1);
      if (!seen.has(key)) { seen.add(key); cards.set(key, (cards.get(key) || 0) + 1); }
    }
  }
  const out = {};
  for (const [key, m] of tally) {
    const top = [...m.entries()].sort((a, b) => b[1] - a[1])[0][0];
    out[key] = { reminder: top, cards: cards.get(key) || 0 };
  }
  return out;
}

async function buildKeywords() {
  const data = await getRules(false);
  const cr = data.rules.filter(r => r.src === 'Comprehensive');
  const heads = cr.filter(r => /^70[12]\.\d+$/.test(r.id) && !['701.1', '702.1'].includes(r.id)
    && r.text.length < 60 && !/^(General|Keyword)/i.test(r.text));
  const mined = await mineReminders(heads.map(h => h.text));
  const keywords = heads.map(h => {
    const sub = cr.filter(r => new RegExp(`^${h.id.replace('.', '\\.')}[a-z]$`).test(r.id)).map(r => ({ id: r.id, text: r.text }));
    const m = mined[norm(h.text)] || {};
    return {
      name: h.text, id: h.id,
      kind: h.id.startsWith('701') ? 'action' : 'ability',
      reminder: m.reminder || null, summary: sub[0] ? sub[0].text : '', cards: m.cards || 0, rules: sub,
    };
  }).sort((a, b) => a.name.localeCompare(b.name));
  return { fetched: data.fetched, crVersion: data.crVersion, keywords };
}

router.get('/keywords', async (req, res) => {
  try {
    const data = await getRules(false);
    if (!kwMem || kwMem.fetched !== data.fetched || 'refresh' in req.query) kwMem = await buildKeywords();
    res.set('Cache-Control', 'private, max-age=3600');
    res.json(kwMem);
  } catch (err) {
    res.status(502).json({ error: `Could not load keywords: ${err.message}` });
  }
});

router.get('/', async (req, res) => {
  try {
    res.set('Cache-Control', 'private, max-age=3600');
    res.json(await getRules('refresh' in req.query));
  } catch (err) {
    res.status(502).json({ error: `Could not load rules: ${err.message}` });
  }
});

module.exports = router;
module.exports.parseCR = parseCR;
module.exports.parseCommander = parseCommander;
module.exports.mineReminders = mineReminders;

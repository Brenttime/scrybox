// Catalog builds: the one job that makes scanning work.
//
// A catalog is one language. Building it has two phases, and the first is the
// one that was missing for years:
//
//   1. CACHE — walk every set Scryfall lists and pull its cards into
//      card_cache. This used to happen only as a side effect of building an ORB
//      scan index, so a set nobody indexed, searched or browsed simply was not
//      there.
//   2. EMBED — run every cached card's artwork through milo and write the
//      embedding table the scanner searches.
//
// Phase 2 can only ever be as complete as phase 1, which is why they are one job
// rather than two buttons. Both resume: phase 1 is idempotent, and phase 2 keeps
// every embedding whose image_url has not changed.
const fs = require('fs');
const path = require('path');
const sharp = require('sharp');
const ort = require('onnxruntime-node');
const db = require('./db');
const cardSets = require('./cardSets');
const languages = require('./utils/languages');
const cvScan = require('./cvScan');

const MODEL_DIR = process.env.CV_MODEL_DIR || path.join(__dirname, '..', 'data', 'models');
const SIZE = 448;
const MEAN = [0.485, 0.456, 0.406];
const STD = [0.229, 0.224, 0.225];

const suffix = (lang) => (!lang || lang === 'en' || lang === 'English' ? '' : `-${String(lang).toLowerCase()}`);
const binPath = (game, lang) => path.join(MODEL_DIR, `milo-${game}${suffix(lang)}-local.bin`);
const metaPath = (game, lang) => path.join(MODEL_DIR, `milo-${game}${suffix(lang)}-local.json`);

// One build at a time. Two concurrent builds would fight over the same provider
// rate limits and the same single-threaded ONNX session, and finish later than
// running them in sequence.
let current = null;

const state = () => (current ? {
  game: current.game, lang: current.lang, phase: current.phase,
  done: current.done, total: current.total, message: current.message,
  // So the panel can say WHICH sets are building rather than implying the whole game.
  sets: current.sets,
  startedAt: current.startedAt, cancelled: current.cancelled,
} : null);

function stop() {
  if (!current) return false;
  current.cancelled = true;
  current.message = 'stopping…';
  return true;
}

// What exists, and how complete it is. The counts come from card_cache and the
// set catalogue, so the UI can say "9,604 of 20,460 cards" rather than only
// "built" — a catalog can be perfectly built and still cover a third of the game.
// How many sets exist that this install has no cards for at all — a newly released
// set, in other words. The weekly refresh keeps the set list current (server.js
// calls fetchAndCacheSets with force), so a release surfaces here on its own.
//
// The comparison has to be done in the SAME id namespace as card_cache: the
// `sets` table stores ids prefixed ("mtg-fdn") while card_cache stores the bare
// Scryfall code ("fdn"). Comparing them raw reports every set as new; the first
// version of this function did exactly that and claimed 1047 of 1047.
async function newSetCount(game, lang = 'English') {
  // This install is MTG-only: the game column went away with the other games, so
  // every table this reads is MTG by definition. Asking for another game has no
  // rows to count -- return 0 rather than fall through and report MTG's gaps
  // under someone else's name.
  if (game !== 'mtg') return 0;
  try {
    // Sets a build already found to have no usable data upstream. Counting them as
    // "not built yet" told the user to build sets that cannot be built, so the
    // number could never drop and the weekly auto-update would have rebuilt
    // forever.
    const gaps = new Set((await db.all(
      `SELECT set_id FROM set_data_gaps WHERE language = ?`, [lang]
    ).catch(() => [])).map(r => String(r.set_id).toLowerCase()));

    // Scoped to the language, or a Spanish catalog would be measured against the
    // ENGLISH cache and report whatever English happens to be missing: measured
    // 98 for both mtg/English and mtg/Spanish while the Spanish cache held 1,205
    // cards against English's 103,656.
    //
    // One pass over each table rather than a correlated NOT EXISTS per set: the id
    // namespaces differ (the sets table prefixes 'mtg-', card_cache holds the bare
    // Scryfall code), so neither side can use idx_card_cache_set_num and the old
    // form re-scanned the WHOLE of card_cache once per set -- 9.6s measured against
    // 1,047 sets and 126k rows, on the single sqlite3 connection every other
    // request queues behind. Same answer, one indexed read.
    const cached = new Set((await db.all(
      `SELECT DISTINCT LOWER(set_id) sid FROM card_cache WHERE language = ?`,
      [lang]
    ).catch(() => [])).map((r) => r.sid));
    const rows = await db.all(`SELECT id FROM sets WHERE COALESCE(total, 0) > 0`);
    const bare = (id) => String(id).toLowerCase().replace(/^mtg-/, '');
    return rows.filter((r) =>
      !gaps.has(bare(r.id))
      && !cached.has(String(r.id).toLowerCase())
      && !cached.has(bare(r.id))).length;
  } catch {
    return null;
  }
}

async function list() {
  const out = [];
  for (const game of ['mtg']) {
    const langs = await db.all(
      `SELECT language, COUNT(*) cached,
              SUM(CASE WHEN image_url IS NOT NULL AND image_url != '' THEN 1 ELSE 0 END) withArt
         FROM card_cache GROUP BY language`
    );
    // Always offer English even with an empty cache — that is exactly the state a
    // fresh install is in, and it is the case the build button exists for.
    if (!langs.some(l => l.language === 'English')) langs.unshift({ language: 'English', cached: 0, withArt: 0 });
    const claimed = await db.get(`SELECT SUM(total) t FROM sets`);
    for (const l of langs) {
      const lang = l.language || 'English';
      let built = null;
      try {
        if (fs.existsSync(metaPath(game, lang))) {
          const meta = JSON.parse(fs.readFileSync(metaPath(game, lang), 'utf8'));
          built = {
            rows: meta.ids.length,
            builtAt: meta.builtAt,
            bytes: fs.statSync(binPath(game, lang)).size,
          };
        }
      } catch { built = null; }
      out.push({
        game, lang,
        cached: l.cached, withArt: l.withArt,
        // A denominator, because "built, 3,297 cards" reads as complete and is not.
        // The `sets` table is Scryfall-derived, so it matches card_cache.
        claimed: lang === 'English' ? (claimed?.t || 0) : null,
        built,
        // Sets that exist and have NOTHING cached — which is what a newly released
        // set looks like. The panel's other warning compares cached against
        // embedded, so it stays silent for exactly this case: a new set is not in
        // card_cache at all yet, and until someone rebuilds, scanning one of its
        // cards returns the nearest wrong card at a confident-looking score.
        //
        // Only for a catalog that EXISTS: a language with nothing built has nothing
        // to have fallen behind, and asking anyway cost a provider round-trip per
        // language on a list() the panel polls every second during a build.
        newSets: built ? await newSetCount(game, lang) : null,
        // The published fallback still answers when nothing local is built.
        published: !built && cvScan.isBuilt(game, lang),
      });
    }
  }
  return out;
}


// Per-set counts for one (game, language): how many cards are cached, and how many
// of those are actually IN the catalog the scanner searches.
//
// This exists because "which sets can I scan?" had no answer anywhere. Two
// consequences, both silent before:
//
//   · The set filter lists the `sets` table, whose ids are the prefixed
//     "mtg-<code>" form while card_cache holds the bare Scryfall code — 51 of
//     172 cached set ids were not in that table at all. Picking one of those
//     matched zero catalog rows, and cvScan fails OPEN, so the user got a
//     full unscoped scan with no indication the filter had done nothing.
//   · A set can be cached but not embedded (a build stopped, or a set released
//     since), and scanning it returns the nearest wrong card rather than nothing.
//
// Keyed by lowercased set_id, which is what the scan filter matches on.
// /scan-sets is polled by the scanner's set picker. Each call used to scan
// card_cache and synchronously read + JSON.parse the catalog meta file. Both
// halves are memoized: the parsed id Set on the meta file's (mtime, size), and
// the per-set counts on that plus collection_read_revision (bumped by triggers
// on every card_cache write). `published` is still checked live (a cheap stat).
const embeddedMemo = new Map(); // metaPath -> { stamp, ids }
const setCountMemo = new Map(); // `${game}|${lang}` -> { key, sets, local }

function metaStamp(file) {
  try {
    const st = fs.statSync(file);
    return `${st.mtimeMs}:${st.size}`;
  } catch {
    return null;
  }
}

function embeddedIds(file, stamp) {
  if (!stamp) return null;
  const hit = embeddedMemo.get(file);
  if (hit && hit.stamp === stamp) return hit.ids;
  let ids = null;
  try { ids = new Set(JSON.parse(fs.readFileSync(file, 'utf8')).ids); } catch { ids = null; }
  embeddedMemo.set(file, { stamp, ids });
  return ids;
}

async function cardCacheRevision() {
  try {
    const row = await db.get('SELECT revision FROM collection_read_revision WHERE id = 1');
    return row ? row.revision : null;
  } catch {
    return null;
  }
}

async function setCounts(game, lang = 'English') {
  const file = metaPath(game, lang);
  const stamp = metaStamp(file);
  const rev = await cardCacheRevision();
  const memoKey = `${game}|${lang}`;
  const key = rev == null ? null : `${rev}|${stamp}`;
  const memo = setCountMemo.get(memoKey);
  let sets;
  let embedded;
  if (key && memo && memo.key === key) {
    ({ sets, embedded } = memo);
  } else {
    const rows = await db.all(
      `SELECT id, LOWER(set_id) sid FROM card_cache
        WHERE language = ? AND set_id IS NOT NULL AND set_id != ''
          AND image_url IS NOT NULL AND image_url != ''`,
      [lang]
    );
    embedded = embeddedIds(file, stamp);

    sets = {};
    for (const r of rows) {
      const e = sets[r.sid] || (sets[r.sid] = { cached: 0, embedded: 0 });
      e.cached++;
      if (embedded && embedded.has(r.id)) e.embedded++;
    }
    // Only keep it if nothing wrote card_cache while we were reading.
    if (key && (await cardCacheRevision()) === rev) setCountMemo.set(memoKey, { key, sets, embedded });
  }
  return {
    game, lang,
    // A copy, so a caller mutating the response can't poison the memo.
    sets: Object.fromEntries(Object.entries(sets).map(([k, v]) => [k, { ...v }])),
    // Whether `embedded` means anything. A published .npz catalog is keyed by
    // provider ids rather than card_cache ids, so it cannot be counted per set —
    // say so instead of reporting zeros that read as "nothing is built".
    local: !!embedded,
    published: !embedded && cvScan.isBuilt(game, lang),
  };
}

function toTensor(rgb) {
  const plane = SIZE * SIZE;
  const x = new Float32Array(3 * plane);
  for (let p = 0; p < plane; p++) {
    x[p] = (rgb[p * 3] / 255 - MEAN[0]) / STD[0];
    x[plane + p] = (rgb[p * 3 + 1] / 255 - MEAN[1]) / STD[1];
    x[2 * plane + p] = (rgb[p * 3 + 2] / 255 - MEAN[2]) / STD[2];
  }
  return new ort.Tensor('float32', x, [1, 3, SIZE, SIZE]);
}

// --- phase 1 -----------------------------------------------------------------
async function cachePhase(job) {
  // A scoped build walks only the sets asked for. Everything else about the job is
  // identical — same fetch, same cache writes — so "the sets in front of me" and
  // "the whole game" are one code path with a different list, not two builders.
  const sets = job.sets && job.sets.length
    ? job.sets
    : await cardSets.listAllSets(job.lang);
  job.phase = 'cache';
  job.total = sets.length;
  job.done = 0;
  let cards = 0, failed = 0, gaps = 0;
  for (const set of sets) {
    if (job.cancelled) return { cards, failed, gaps, stopped: true };
    job.message = `${set}`;
    try {
      const got = await cardSets.cacheSetCards(set, job.lang);
      cards += got.length;
      // Data appeared for a set previously recorded as empty: forget the gap so it
      // counts as a normal set again.
      await db.run(`DELETE FROM set_data_gaps WHERE language = ? AND set_id = ?`,
        [job.lang, String(set).toLowerCase()]).catch(() => {});
    } catch (e) {
      // A set with no cards in this language is an expected gap, not a failure —
      // provider coverage is patchy per language and treating gaps as errors
      // would abort every non-English build partway through.
      if (!e.absent) failed++;
      else {
        gaps++;
        // Remember it, so the panel stops telling the user to build a set the
        // provider cannot serve and the auto-update stops chasing it.
        await db.run(
          `INSERT OR REPLACE INTO set_data_gaps (language, set_id, reason) VALUES (?, ?, ?)`,
          [job.lang, String(set).toLowerCase(), e.message.slice(0, 200)]
        ).catch(() => {});
      }
    }
    job.done++;
  }
  return { cards, failed, gaps, stopped: false };
}

// Which of the previous catalog's cards a SCOPED build must carry forward: every
// one it did not just re-embed.
//
// This is what makes per-set builds additive. embedPhase writes exactly the rows
// it embedded, so without this, building Bloomburrow would delete the Foundations
// vectors built last week — the catalog is one file per (game, language), not one
// per set.
//
// Deliberately NOT applied to an unscoped build: that walks every row for the
// language, so a card in `prev` and absent from `embeddedIds` is one that left
// card_cache. Keeping it would leave a vector that can win a scan and then resolve
// to no card at all.
function keptFromPrev(prev, embeddedIds) {
  const embedded = new Set(embeddedIds);
  const out = [];
  for (const [id, vec] of prev.vecs) {
    if (embedded.has(id)) continue;
    out.push({ id, vec, src: prev.srcs.get(id) });
  }
  return out;
}

// --- phase 2 -----------------------------------------------------------------
async function embedPhase(job) {
  // A scoped build embeds only the chosen sets' cards. The catalog it writes is
  // still the whole table, because the vectors for every OTHER set are MERGED back
  // in below — without that, "build Bloomburrow" would silently delete the
  // Foundations vectors built last week, since this phase writes exactly the rows
  // it embedded.
  const scoped = job.sets && job.sets.length;
  const rows = await db.all(
    `SELECT id, image_url FROM card_cache
      WHERE language = ? AND image_url IS NOT NULL AND image_url != ''
        ${scoped ? `AND LOWER(set_id) IN (${job.sets.map(() => '?').join(',')})` : ''}
      ORDER BY id`,
    scoped ? [job.lang, ...job.sets.map(s => String(s).toLowerCase())] : [job.lang]
  );
  job.phase = 'embed';
  job.total = rows.length;
  job.done = 0;
  if (!rows.length) return { built: 0, reused: 0, failed: 0, wrote: false };

  // Resume: keep every embedding whose source image has not changed.
  let prev = null;
  try {
    if (fs.existsSync(metaPath(job.game, job.lang))) {
      const meta = JSON.parse(fs.readFileSync(metaPath(job.game, job.lang), 'utf8'));
      const buf = fs.readFileSync(binPath(job.game, job.lang));
      const vecs = new Map();
      for (let i = 0; i < meta.ids.length; i++) {
        vecs.set(meta.ids[i], new Float32Array(buf.buffer, buf.byteOffset + i * meta.dim * 4, meta.dim));
      }
      prev = { vecs, srcs: new Map(Object.entries(meta.srcs || {})) };
    }
  } catch { prev = null; }

  const session = await ort.InferenceSession.create(path.join(MODEL_DIR, 'milo.onnx'), {
    intraOpNumThreads: 1, interOpNumThreads: 1, executionMode: 'sequential',
  });

  const ids = [], vecs = [], srcs = {};
  let built = 0, reused = 0, failed = 0;
  const queue = rows.slice();
  const inflight = new Map();
  const CONCURRENCY = 8;

  // The image the SCANNER should match against. Scryfall's url is a full-size
  // render; nothing to swap.
  const embedUrl = (row) => row.image_url;

  // Scryfall's image CDN rejects a request with no User-Agent — 400, not 403, which
  // reads like a bad URL. Version comes from package.json rather than a literal.
  const HEADERS = {
    'User-Agent': `Scrybox/${require('../package.json').version} (+https://github.com/Brenttime/scrybox)`,
    Accept: 'image/*',
  };
  const fetchOne = async (row) => {
    const res = await fetch(embedUrl(row), {
      signal: AbortSignal.timeout(30000),
      headers: HEADERS,
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    return Buffer.from(await res.arrayBuffer());
  };
  // Download ahead while the CPU embeds: the network is the slow half and the
  // model is single-threaded, so overlapping them is most of the wall clock.
  const pump = () => {
    while (inflight.size < CONCURRENCY && queue.length && !job.cancelled) {
      const row = queue.shift();
      // Resume compares the url that was EMBEDDED, not the one card_cache holds:
      // raising the resolution above has to invalidate every vector built from the
      // old one, and a row whose art was re-uploaded still has to rebuild.
      if (prev && prev.vecs.has(row.id) && prev.srcs.get(row.id) === embedUrl(row)) {
        ids.push(row.id); vecs.push(prev.vecs.get(row.id)); srcs[row.id] = embedUrl(row);
        reused++; job.done++;
        continue;
      }
      inflight.set(row.id, fetchOne(row).then(buf => ({ row, buf }), err => ({ row, err })));
    }
  };

  pump();
  while (inflight.size) {
    const settled = await Promise.race(inflight.values());
    inflight.delete(settled.row.id);
    if (settled.err) failed++;
    else {
      try {
        const { data } = await sharp(settled.buf).resize(SIZE, SIZE, { fit: 'fill' })
          .removeAlpha().raw().toBuffer({ resolveWithObject: true });
        const out = await session.run({ image: toTensor(data) });
        vecs.push(out.embedding.data);
        ids.push(settled.row.id);
        srcs[settled.row.id] = embedUrl(settled.row);
        built++;
      } catch { failed++; }
    }
    job.done++;
    job.message = `${built + reused}/${rows.length}`;
    pump();
  }

  if (scoped && prev) {
    for (const { id, vec, src } of keptFromPrev(prev, ids)) {
      ids.push(id); vecs.push(vec);
      if (src) srcs[id] = src;
      reused++;
    }
  }

  // A cancelled build still writes what it has: the partial catalog is valid and
  // resuming later reuses all of it. Refusing to write would throw away the work.
  if (!vecs.length) return { built, reused, failed, wrote: false };
  const dim = vecs[0].length;
  const bin = Buffer.allocUnsafe(vecs.length * dim * 4);
  vecs.forEach((v, i) => Buffer.from(v.buffer, v.byteOffset, dim * 4).copy(bin, i * dim * 4));
  fs.mkdirSync(MODEL_DIR, { recursive: true });
  // Write-then-rename: an interrupted write must not leave a truncated catalog
  // that loads as garbage.
  fs.writeFileSync(binPath(job.game, job.lang) + '.tmp', bin);
  fs.writeFileSync(metaPath(job.game, job.lang) + '.tmp', JSON.stringify({
    dim, ids, srcs, game: job.game, lang: job.lang,
    model: 'milo1', builtAt: new Date().toISOString(),
  }));
  fs.renameSync(binPath(job.game, job.lang) + '.tmp', binPath(job.game, job.lang));
  fs.renameSync(metaPath(job.game, job.lang) + '.tmp', metaPath(job.game, job.lang));
  cvScan.reload(job.game, job.lang);
  return { built, reused, failed, wrote: true, rows: ids.length };
}

// Which non-English languages a catalog could be built for, and how much is
// already cached for each. For MTG every language is available (Scryfall serves
// them all), so this is just the card_cache counts per language — no provider
// round-trip needed. A language with nothing cached yet is still a candidate:
// building starts from an empty cache and fetches from the provider, so hiding
// the unbuilt ones would make every non-English catalog unreachable on a fresh
// install.
async function listLanguages(game = 'mtg') {
  if (game !== 'mtg') return [];
  const rows = await db.all(
    `SELECT language, COUNT(*) cached,
            SUM(CASE WHEN image_url IS NOT NULL AND image_url != '' THEN 1 ELSE 0 END) withArt
       FROM card_cache GROUP BY language`
  );
  const have = new Map(rows.map(r => [r.language || 'English', r]));
  const out = [];
  for (const l of languages.LANGUAGES) {
    if (languages.isEnglish(l.code)) continue;   // English is a row of its own
    const name = languages.toName(l.code);
    const h = have.get(name) || { cached: 0, withArt: 0 };
    let built = null;
    try {
      if (fs.existsSync(metaPath(game, name))) {
        built = { rows: JSON.parse(fs.readFileSync(metaPath(game, name), 'utf8')).ids.length };
      }
    } catch { built = null; }
    out.push({ game, lang: name, code: l.code, cached: h.cached, withArt: h.withArt, built });
  }
  return out;
}

function start(game, lang = 'English', opts = {}) {
  if (current) throw new Error('a catalog build is already running');
  if (game !== 'mtg') throw new Error(`unknown game ${game}`);
  const job = {
    game, lang: languages.toName(lang) || 'English',
    // Which sets this build covers, or empty for the whole game. Lowercased once
    // here so neither phase has to think about case: cachePhase passes them to the
    // provider and embedPhase matches them against LOWER(set_id).
    sets: (opts.sets || []).map(s => String(s).trim().toLowerCase()).filter(Boolean),
    phase: 'starting', done: 0, total: 0, message: '',
    startedAt: Date.now(), cancelled: false,
  };
  current = job;
  (async () => {
    try {
      const cached = opts.skipCache ? { cards: 0, failed: 0 } : await cachePhase(job);
      const embedded = job.cancelled && !opts.keepGoing ? null : await embedPhase(job);
      job.phase = job.cancelled ? 'cancelled' : 'done';
      // Say what was skipped and why, or a build that could not touch 46 listed sets
      // reports success and leaves the user wondering why the coverage did not move.
      const skipped = cached.gaps ? `, ${cached.gaps} set(s) skipped — no card data upstream` : '';
      job.message = embedded
        ? `${embedded.rows || 0} cards embedded (${embedded.built} new, ${embedded.reused} reused, ${embedded.failed} failed)${skipped}`
        : `cached ${cached.cards} cards${skipped}`;
      console.log(`catalog: ${job.game}/${job.lang} ${job.phase} — ${job.message}`);
    } catch (e) {
      job.phase = 'error';
      job.message = e.message;
      console.error(`catalog: ${job.game}/${job.lang} failed:`, e.message);
    } finally {
      job.finishedAt = Date.now();
      last = state();
      current = null;
    }
  })();
  return state();
}

let last = null;
const lastResult = () => last;

module.exports = { list, setCounts, keptFromPrev, start, stop, state, lastResult, listLanguages, binPath, metaPath, newSetCount };

// Local Scryfall Oracle Tags index and one-time oracle_id backfill.
//
// Both jobs are background maintenance: server startup only schedules them. Tag
// refreshes are generation-swapped, so a failed download/import never replaces
// the last complete generation that collection searches can use.
const axios = require('axios');
const readline = require('readline');
const zlib = require('zlib');
const { Readable } = require('stream');
const db = require('./db');
const { oracleIdForCard, defaultSearchEligible } = require('./utils/oracleId');

const DAY_MS = 24 * 60 * 60 * 1000;
const MAINTENANCE_POLL_MS = 60 * 60 * 1000;
const START_DELAY_MS = 30 * 1000;
const BULK_ENDPOINT = 'https://api.scryfall.com/bulk-data';
const USER_AGENT = 'Scrybox/1.0 (+https://github.com/Brenttime/scrybox)';
const LOCAL_COLLECTION_OPERATORS = new Set([
  // Keep this deliberately narrower than the SQL compiler. Scryfall implicitly
  // includes extras when a query explicitly targets certain names, layouts,
  // types, sets, collector numbers, or `is:` categories. Our local index stores
  // only default-search eligibility, not enough metadata to reproduce that
  // inference exactly, so those combinations must retain live fallback.
  // Canonical names (shared/scryfallQuery.js folds c:/id:/r:/cmc: into these).
  'color', 'identity', 'rarity', 'lang', 'mv', 'otag',
  // Display directives never filter a row.
  'include', 'unique', 'order', 'sort', 'direction', 'prefer', 'display',
]);

let maintenancePromise = null;
let startupTimer = null;
let intervalTimer = null;

function makeHttp() {
  return axios.create({
    timeout: 120000,
    headers: { 'User-Agent': USER_AGENT, 'Accept': 'application/json' },
  });
}

function bulkMetadata(body, type) {
  const value = body && body.data && !body.type ? body.data : body;
  if (!value || value.type !== type) {
    throw new Error(`Scryfall bulk-data/${type} returned an unexpected object`);
  }
  const url = value.jsonl_download_uri || value.download_uri;
  if (!url) throw new Error(`Scryfall bulk-data/${type} has no JSONL download URI`);
  return { url, updatedAt: value.updated_at || null };
}

async function maybeGunzip(stream) {
  const iterator = stream[Symbol.asyncIterator]();
  const first = await iterator.next();
  const head = first.done ? Buffer.alloc(0) : Buffer.from(first.value);
  const rebuilt = Readable.from((async function* () {
    if (head.length) yield head;
    if (!first.done) {
      for (;;) {
        const next = await iterator.next();
        if (next.done) break;
        yield next.value;
      }
    }
  })(), { objectMode: false });
  return head.length >= 2 && head[0] === 0x1f && head[1] === 0x8b
    ? rebuilt.pipe(zlib.createGunzip())
    : rebuilt;
}

async function* streamJsonl(http, url) {
  const response = await http.get(url, {
    responseType: 'stream',
    timeout: 120000,
    decompress: false,
  });
  const input = await maybeGunzip(response.data);
  const lines = readline.createInterface({ input, crlfDelay: Infinity });
  let lineNumber = 0;
  for await (const line of lines) {
    lineNumber += 1;
    const text = line.trim().replace(/,$/, '');
    if (!text || text === '[' || text === ']') continue;
    try {
      yield JSON.parse(text);
    } catch (error) {
      throw new Error(`Invalid JSONL at line ${lineNumber}: ${error.message}`);
    }
  }
}

function normalizeAlias(value) {
  return String(value || '').trim().toLowerCase();
}

async function collectTagData(objects) {
  const tags = new Map();
  const edges = new Set();
  const assignments = new Set();

  for await (const raw of objects) {
    if (!raw || raw.type !== 'oracle' || !raw.id || !raw.slug) continue;
    const tag = {
      id: String(raw.id),
      slug: String(raw.slug),
      label: raw.label == null ? null : String(raw.label),
      description: raw.description == null ? null : String(raw.description),
      aliases: new Set([normalizeAlias(raw.slug)]),
    };
    for (const alias of raw.aliases || []) {
      const clean = normalizeAlias(alias);
      if (clean) tag.aliases.add(clean);
    }
    tags.set(tag.id, tag);
    for (const parent of raw.parent_ids || []) {
      if (parent) edges.add(`${String(parent)}\u0000${tag.id}`);
    }
    for (const child of raw.child_ids || []) {
      if (child) edges.add(`${tag.id}\u0000${String(child)}`);
    }
    for (const tagging of raw.taggings || []) {
      if (tagging && tagging.oracle_id) {
        assignments.add(`${tag.id}\u0000${String(tagging.oracle_id)}`);
      }
    }
  }

  if (!tags.size) throw new Error('Oracle Tags stream contained no usable oracle tags');

  // Ignore dangling hierarchy references rather than letting one upstream typo
  // invalidate the complete assignment index. Stable UUIDs remain authoritative.
  const children = new Map([...tags.keys()].map(id => [id, new Set()]));
  const cleanEdges = [];
  for (const edge of edges) {
    const [parent, child] = edge.split('\u0000');
    if (!tags.has(parent) || !tags.has(child)) continue;
    children.get(parent).add(child);
    cleanEdges.push([parent, child]);
  }

  // Materialize transitive descendants once per daily import. Collection queries
  // then need indexed joins rather than a recursive walk for every owned row.
  const closure = [];
  for (const ancestor of tags.keys()) {
    const seen = new Set([ancestor]);
    const queue = [ancestor];
    for (let head = 0; head < queue.length; head += 1) {
      const current = queue[head];
      closure.push([ancestor, current]);
      for (const child of children.get(current) || []) {
        if (!seen.has(child)) {
          seen.add(child);
          queue.push(child);
        }
      }
    }
  }

  return {
    tags: [...tags.values()],
    edges: cleanEdges,
    assignments: [...assignments].map(value => value.split('\u0000')),
    closure,
  };
}

async function insertBatches(client, prefix, rows, columnsPerRow, batchSize = 150) {
  for (let i = 0; i < rows.length; i += batchSize) {
    const chunk = rows.slice(i, i + batchSize);
    const placeholders = `(${new Array(columnsPerRow).fill('?').join(', ')})`;
    await client.run(
      `${prefix} ${chunk.map(() => placeholders).join(', ')}`,
      chunk.flat(),
    );
  }
}

async function insertMappedBatches(client, prefix, rows, mapper, columnsPerRow, batchSize = 150) {
  for (let i = 0; i < rows.length; i += batchSize) {
    const chunk = rows.slice(i, i + batchSize).map(mapper);
    const placeholders = `(${new Array(columnsPerRow).fill('?').join(', ')})`;
    await client.run(
      `${prefix} ${chunk.map(() => placeholders).join(', ')}`,
      chunk.flat(),
    );
  }
}

async function stageMappedRows(database, prefix, rows, mapper, columnsPerRow, transactionRows = 6000) {
  for (let i = 0; i < rows.length; i += transactionRows) {
    const transactionChunk = rows.slice(i, i + transactionRows);
    await database.withDedicatedTransaction(tx => (
      insertMappedBatches(tx, prefix, transactionChunk, mapper, columnsPerRow)
    ));
  }
}

// Inactive generations are never queryable. Delete their large child tables in
// short autocommit batches so a daily refresh does not hold SQLite's sole writer
// lock while hundreds of thousands of assignments are staged or reaped.
async function deleteInactiveGeneration(database, generationId, batchSize = 2000) {
  const tables = [
    'oracle_tag_aliases', 'oracle_tag_edges', 'oracle_tag_assignments',
    'oracle_tag_closure', 'oracle_tags',
  ];
  for (const table of tables) {
    for (;;) {
      const deleted = await database.run(
        `DELETE FROM ${table}
         WHERE rowid IN (
           SELECT rowid FROM ${table} WHERE generation_id = ? LIMIT ?
         )`,
        [generationId, batchSize],
      );
      if (!deleted || deleted.changes < batchSize) break;
    }
  }
  await database.run(
    'DELETE FROM oracle_tag_generations WHERE id = ? AND active = 0',
    [generationId],
  );
}

async function reapInactiveGenerations(database, exceptId = null) {
  const rows = await database.all(
    `SELECT id FROM oracle_tag_generations
     WHERE active = 0 ${exceptId == null ? '' : 'AND id <> ?'}`,
    exceptId == null ? [] : [exceptId],
  );
  for (const row of rows) await deleteInactiveGeneration(database, row.id);
}

async function importTagObjects(objects, sourceUpdatedAt, opts = {}) {
  const database = opts.dbClient || db;
  const data = await collectTagData(objects);
  let generationId = null;
  try {
    // Every staging statement is its own short transaction. The previous active
    // generation remains complete and readable throughout this phase.
    const created = await database.run(
      `INSERT INTO oracle_tag_generations (source_updated_at, imported_at, active)
       VALUES (?, ?, 0)`,
      [sourceUpdatedAt || '', Date.now()],
    );
    generationId = created.lastID;

    // Stage in bounded writer transactions. Six thousand rows keeps each lock
    // short while avoiding the WAL amplification of one transaction per 150-row
    // INSERT statement.
    await stageMappedRows(database,
      `INSERT INTO oracle_tags (generation_id, tag_id, slug, label, description) VALUES`,
      data.tags, tag => [generationId, tag.id, tag.slug, tag.label, tag.description], 5);

    const aliases = [];
    for (const tag of data.tags) {
      for (const alias of tag.aliases) aliases.push([generationId, tag.id, alias]);
    }
    await stageMappedRows(database,
      `INSERT INTO oracle_tag_aliases (generation_id, tag_id, alias) VALUES`, aliases, row => row, 3);
    await stageMappedRows(database,
      `INSERT INTO oracle_tag_edges (generation_id, parent_tag_id, child_tag_id) VALUES`,
      data.edges, ([parent, child]) => [generationId, parent, child], 3);
    await stageMappedRows(database,
      `INSERT INTO oracle_tag_assignments (generation_id, tag_id, oracle_id) VALUES`,
      data.assignments, ([tagId, oracleId]) => [generationId, tagId, oracleId], 3);
    await stageMappedRows(database,
      `INSERT INTO oracle_tag_closure (generation_id, ancestor_tag_id, descendant_tag_id) VALUES`,
      data.closure, ([ancestor, descendant]) => [generationId, ancestor, descendant], 3);

    // Only the pointer swap needs a writer-owning transaction; it contains two
    // tiny updates rather than the entire import and cleanup.
    await database.withDedicatedTransaction(async (tx) => {
      await tx.run('UPDATE oracle_tag_generations SET active = 0 WHERE active = 1');
      await tx.run('UPDATE oracle_tag_generations SET active = 1 WHERE id = ?', [generationId]);
    });

    // Cleanup is recoverable and deliberately not part of activation. A busy or
    // interrupted reaper leaves harmless inactive rows for the next refresh.
    try {
      await reapInactiveGenerations(database);
    } catch (error) {
      console.error('Oracle Tags inactive-generation cleanup failed:', error.message);
    }
    return {
      generationId,
      tags: data.tags.length,
      assignments: data.assignments.length,
      sourceUpdatedAt: sourceUpdatedAt || '',
    };
  } catch (error) {
    if (generationId != null) {
      try { await deleteInactiveGeneration(database, generationId); } catch (cleanupError) {
        console.error('Oracle Tags failed-generation cleanup failed:', cleanupError.message);
      }
    }
    throw error;
  }
}

async function refreshOracleTags(opts = {}) {
  const database = opts.dbClient || db;
  const http = opts.http || makeHttp();
  const now = opts.now || Date.now();
  const force = !!opts.force;
  const state = await database.get(
    'SELECT oracle_tags_checked_at FROM app_settings WHERE id = 1',
  );
  const lastChecked = Number(state && state.oracle_tags_checked_at) || 0;
  if (!force && now - lastChecked < DAY_MS) return { status: 'not-due' };

  const metadataResponse = await http.get(`${BULK_ENDPOINT}/oracle_tags`);
  const metadata = bulkMetadata(metadataResponse.data, 'oracle_tags');
  const active = await database.get(
    'SELECT source_updated_at FROM oracle_tag_generations WHERE active = 1',
  );
  if (active && metadata.updatedAt && active.source_updated_at === metadata.updatedAt) {
    await database.run(
      'UPDATE app_settings SET oracle_tags_checked_at = ? WHERE id = 1', [now],
    );
    return { status: 'current', sourceUpdatedAt: metadata.updatedAt };
  }
  const result = await importTagObjects(streamJsonl(http, metadata.url), metadata.updatedAt, { dbClient: database });
  // A transient download, parse, or SQLITE_BUSY failure must be retried by the
  // hourly maintenance poll. Only a verified current/imported snapshot advances
  // the daily success gate.
  await database.run(
    'UPDATE app_settings SET oracle_tags_checked_at = ? WHERE id = 1', [now],
  );
  return { status: 'refreshed', ...result };
}

async function updateOracleIds(database, pairs) {
  const clean = pairs.filter(([id, oracleId, eligible]) => id && (oracleId || eligible != null));
  for (let i = 0; i < clean.length; i += 250) {
    const chunk = clean.slice(i, i + 250);
    const oracleCases = chunk.map(() => 'WHEN ? THEN COALESCE(?, oracle_id)').join(' ');
    const eligibleCases = chunk.map(() => 'WHEN ? THEN COALESCE(?, scryfall_search_eligible)').join(' ');
    const ids = chunk.map(() => '?').join(', ');
    await database.run(
      `UPDATE card_cache SET
         oracle_id = CASE id ${oracleCases} ELSE oracle_id END,
         scryfall_search_eligible = CASE id ${eligibleCases} ELSE scryfall_search_eligible END
       WHERE id IN (${ids})`,
      [
        ...chunk.flatMap(([id, oracleId]) => [id, oracleId || null]),
        ...chunk.flatMap(([id, , eligible]) => [id, eligible == null ? null : eligible]),
        ...chunk.map(([id]) => id),
      ],
    );
  }
}

async function backfillOracleIds(opts = {}) {
  const database = opts.dbClient || db;
  const http = opts.http || makeHttp();
  const complete = await database.get(
    'SELECT oracle_id_backfill_completed_at FROM app_settings WHERE id = 1',
  );
  const bulkAlreadyCompleted = !opts.force
    && !!(complete && complete.oracle_id_backfill_completed_at);
  let unresolved = await database.all(
    `SELECT id, set_id, number, name FROM card_cache
     WHERE id LIKE 'mtg-%'
       AND (oracle_id IS NULL OR scryfall_search_eligible IS NULL)`,
  );
  if (!unresolved.length) {
    await database.run(
      'UPDATE app_settings SET oracle_id_backfill_completed_at = ? WHERE id = 1',
      [opts.now || Date.now()],
    );
    return { status: 'backfilled', bulk: 0, fallback: 0, unresolved: 0 };
  }

  let bulkCount = 0;
  if (!bulkAlreadyCompleted) {
    const wanted = new Set(unresolved.map(row => row.id));
    const metadataResponse = await http.get(`${BULK_ENDPOINT}/default_cards`);
    const metadata = bulkMetadata(metadataResponse.data, 'default_cards');
    let buffered = [];
    for await (const raw of streamJsonl(http, metadata.url)) {
      // The Default Cards bulk is English, but keep the gate explicit: an exact
      // printing id must never give a localized row another printing's oracle id.
      const oracleId = oracleIdForCard(raw);
      if (!raw || !raw.id || !oracleId || (raw.lang && raw.lang !== 'en')) continue;
      const id = `mtg-${raw.id}`;
      if (!wanted.has(id)) continue;
      buffered.push([id, oracleId, defaultSearchEligible(raw)]);
      wanted.delete(id);
      if (buffered.length >= 500) {
        await updateOracleIds(database, buffered);
        bulkCount += buffered.length;
        buffered = [];
      }
    }
    if (buffered.length) {
      await updateOracleIds(database, buffered);
      bulkCount += buffered.length;
    }
    // The expensive full bulk scan completed. Residual exact-id lookups may be
    // retried hourly without downloading it again.
    await database.run(
      'UPDATE app_settings SET oracle_id_backfill_completed_at = ? WHERE id = 1',
      [opts.now || Date.now()],
    );
  }

  unresolved = await database.all(
    `SELECT id, set_id, number, name FROM card_cache
     WHERE id LIKE 'mtg-%'
       AND (oracle_id IS NULL OR scryfall_search_eligible IS NULL)`,
  );
  let fallbackCount = 0;
  if (unresolved.length) {
    const bulkFetch = opts.bulkFetchByIdentifier
      || require('./scryfallApi').bulkFetchByIdentifier;
    const fetched = await bulkFetch(unresolved);
    const pairs = [];
    for (const pair of fetched.pairs || []) {
      const oracleId = oracleIdForCard(pair.card);
      if (pair.row && oracleId) {
        pairs.push([
          pair.row.id || pair.row.card_id,
          oracleId,
          pair.card.scryfall_search_eligible == null
            ? defaultSearchEligible(pair.card)
            : pair.card.scryfall_search_eligible,
        ]);
      }
    }
    await updateOracleIds(database, pairs);
    fallbackCount = pairs.length;
  }

  const remaining = await database.get(
    `SELECT COUNT(*) AS n FROM card_cache
     WHERE id LIKE 'mtg-%'
       AND (oracle_id IS NULL OR scryfall_search_eligible IS NULL)`,
  );
  return {
    status: 'backfilled', bulk: bulkCount, fallback: fallbackCount,
    unresolved: Number(remaining && remaining.n) || 0,
  };
}

async function isReady(database = db, userId = null) {
  const active = await database.get(
    'SELECT id FROM oracle_tag_generations WHERE active = 1 LIMIT 1',
  );
  if (!active) return false;
  const ownerClause = userId == null
    ? ''
    : `AND EXISTS (
         SELECT 1 FROM collection c
         WHERE c.card_id = card_cache.id AND c.user_id = ? AND c.quantity > 0
       )`;
  const missing = await database.get(
    `SELECT 1 AS missing FROM card_cache
     WHERE id LIKE 'mtg-%'
       AND (oracle_id IS NULL OR scryfall_search_eligible IS NULL)
       ${ownerClause} LIMIT 1`,
    userId == null ? [] : [userId],
  );
  return !missing;
}

function supportsLocalCollectionQuery(analysis) {
  return !!analysis
    && analysis.operators.includes('otag')
    && analysis.operators.every(op => LOCAL_COLLECTION_OPERATORS.has(op));
}

async function checkpointWal(database = db) {
  try {
    const checkpoint = await database.get('PRAGMA wal_checkpoint(TRUNCATE)');
    if (checkpoint && Number(checkpoint.busy) > 0) {
      console.warn('Oracle Tags WAL truncate deferred because a reader is active.');
      return false;
    }
    return true;
  } catch (error) {
    console.warn('Oracle Tags WAL checkpoint failed:', error.message);
    return false;
  }
}

async function runMaintenance(opts = {}) {
  if (maintenancePromise) return maintenancePromise;
  maintenancePromise = (async () => {
    const results = {};
    try {
      results.tags = await refreshOracleTags(opts);
    } catch (error) {
      console.error('Oracle Tags refresh failed:', error.message);
      results.tags = { status: 'failed', error };
    }
    try {
      results.backfill = await backfillOracleIds(opts);
    } catch (error) {
      console.error('oracle_id backfill failed:', error.message);
      results.backfill = { status: 'failed', error };
    }
    // Bulk staging can temporarily grow a WAL even though all frames have been
    // checkpointed. Shrink that high-water file before the price sweep begins;
    // a concurrent reader merely makes this best-effort and maintenance remains
    // successful.
    await checkpointWal(opts.dbClient || db);
    return results;
  })().finally(() => { maintenancePromise = null; });
  return maintenancePromise;
}

function startOracleTagsService(opts = {}) {
  if (startupTimer || intervalTimer) return;
  const delay = opts.startDelayMs == null ? START_DELAY_MS : opts.startDelayMs;
  startupTimer = setTimeout(() => {
    startupTimer = null;
    runMaintenance(opts);
  }, delay);
  if (startupTimer.unref) startupTimer.unref();
  // Poll hourly, while refreshOracleTags itself keeps successful checks daily.
  // Failed network/import attempts therefore self-heal without a restart or a
  // 24-hour blind spot.
  intervalTimer = setInterval(() => runMaintenance(opts), MAINTENANCE_POLL_MS);
  if (intervalTimer.unref) intervalTimer.unref();
}

function stopOracleTagsService() {
  if (startupTimer) clearTimeout(startupTimer);
  if (intervalTimer) clearInterval(intervalTimer);
  startupTimer = null;
  intervalTimer = null;
}

module.exports = {
  DAY_MS,
  MAINTENANCE_POLL_MS,
  makeHttp,
  bulkMetadata,
  streamJsonl,
  collectTagData,
  importTagObjects,
  refreshOracleTags,
  backfillOracleIds,
  isReady,
  supportsLocalCollectionQuery,
  checkpointWal,
  runMaintenance,
  startOracleTagsService,
  stopOracleTagsService,
};

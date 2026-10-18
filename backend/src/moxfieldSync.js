// Moxfield sync engine: mirrors an author's public Moxfield decks into Scrybox.
//
// Two cadences, driven by moxfieldScheduler.js on its 20s tick:
//
//   syncDecklist(author) — the SLOW job (default: every hour, configurable).
//     Re-validates the author on Moxfield, re-lists their public decks, creates
//     the local mirror deck for anything new (and pulls its contents right
//     away), and retires anything the author deleted.
//
//   runContentSync(author) — the FAST job (default: every minute, configurable).
//     Fetches the author's deck summary list — one request carries every
//     deck's lastUpdatedAtUtc stamp — and compares it against the stamps it
//     last mirrored. A deck whose stamp moved gets its full contents pulled
//     from /v3/decks/all and mirrored onto the local deck. No stamp movement,
//     no network beyond that one summary call: the per-minute check is cheap by
//     construction.
//
// Mapping rule: Moxfield supplies exact Scryfall printing ids for cache lookup,
// but deck identity is the normalized canonical English card name. Reprints are
// collapsed before reconciliation; the retained id is only representative art.
//
// Ownership: mirrored decks are owned by the user who added the author, carry
// `source='moxfield'` plus the Moxfield public id, and are reconciled exactly
// on each content pull. Whatever a local user edits on a synced deck is a
// departure the next content sync overwrites — that is the feature: the local
// deck is a live mirror, not a copy. Hand-made decks (source='manual') are
// never touched.
const db = require('./db');
const moxfieldApi = require('./moxfieldApi');
const { bulkFetchByIdentifier, cacheCards } = require('./scryfallApi');
const { sqlCardKey } = require('./utils/cardIdentity');
const { withAllocationLock } = require('./utils/collectionHelpers');

// Flag deck rows whose logical card is in `commanderKeys`; clear the rest.
async function markDeckCommanders(client, deckId, commanderKeys) {
  const rows = await client.all(`
    SELECT dc.card_id, ${sqlCardKey('cc')} AS card_key
    FROM deck_cards dc LEFT JOIN card_cache cc ON cc.id = dc.card_id
    WHERE dc.deck_id = ?`, [deckId]);
  for (const row of rows) {
    const flag = row.card_key && commanderKeys.has(row.card_key) ? 1 : 0;
    await client.run(`UPDATE deck_cards SET is_commander = ? WHERE deck_id = ? AND card_id = ? AND is_commander IS NOT ?`,
      [flag, deckId, row.card_id, flag]);
  }
}

// Replace a deck's maybeboard rows. deck_maybe_cards is display-only: it never
// feeds totals, stats, value or checkout allocation, so it is refreshed even
// for checked-out decks.
async function replaceMaybeboard(tx, deckId, maybeEntries) {
  const qty = new Map();
  for (const e of maybeEntries) {
    const id = scryboxCardId(e.card);
    qty.set(id, (qty.get(id) || 0) + e.quantity);
  }
  await tx.run(`DELETE FROM deck_maybe_cards WHERE deck_id = ?`, [deckId]);
  for (const [cardId, quantity] of qty) {
    await tx.run(
      `INSERT INTO deck_maybe_cards (deck_id, card_id, quantity)
       SELECT ?, ?, ? WHERE EXISTS (SELECT 1 FROM card_cache WHERE id = ?)`,
      [deckId, cardId, quantity, cardId]
    );
  }
}

function allocationConflict(message) {
  const error = new Error(message);
  error.code = 'ALLOCATION_CONFLICT';
  return error;
}
const {
  MIRROR_BOARDS,
  extractDeckCards,
  extractMaybeCards,
  boardCounts,
  scryboxCardId,
  mfxFormatLabel,
  targetSizeForFormat,
  synthesizeMoxfieldCard
} = require('./utils/mfxPayload');

// ---------------------------------------------------------------------------
// Scryfall backfill for cards not yet in card_cache
// ---------------------------------------------------------------------------

// Moxfield hands us complete card data, but Scrybox's card_cache is what every
// downstream read joins against (art, prices, rarity, marketplace links — and
// the MTG price sweep). For cards we do not already hold, resolve them against
// Scryfall by id — the same batched, queue-limited lookup the import path uses.
//
// Some cards (sealed-product printings like Mystery Booster) have a
// scryfall_id Scryfall does not know: the id lookup 404s for them. Those are
// synthesized into card_cache from Moxfield's own card block so the deck never
// ends up with a dangling reference (price_source='moxfield' marks the row).
async function backfillMissingCards(entries) {
  const ids = [...new Set(entries.map(e => scryboxCardId(e.card)))];
  const missing = [];
  for (const id of ids) {
    const cached = await db.get(`SELECT id FROM card_cache WHERE id = ?`, [id]);
    if (!cached) missing.push(id);
  }
  if (missing.length === 0) return;

  const { cards, notFound } = await bulkFetchByIdentifier(missing.map(id => ({ id })));
  if (cards.length > 0) await cacheCards(cards);

  // Anything still absent after the batched lookup is a card Scryfall has no
  // record for — synthesize it from the Moxfield payload instead of leaving a
  // dangling deck_card -> card_cache reference.
  const stillMissing = new Set();
  for (const id of missing) {
    const cached = await db.get(`SELECT id FROM card_cache WHERE id = ?`, [id]);
    if (!cached) stillMissing.add(id);
  }
  if (stillMissing.size > 0) {
    const seen = new Set();
    const synthesized = [];
    for (const e of entries) {
      const id = scryboxCardId(e.card);
      if (stillMissing.has(id) && !seen.has(id)) {
        seen.add(id);
        synthesized.push(synthesizeMoxfieldCard(id, e.card));
      }
    }
    if (synthesized.length > 0) {
      await cacheCards(synthesized);
      console.warn(`Moxfield sync: ${synthesized.length} card(s) unknown to Scryfall — synthesized from Moxfield data: ${synthesized.map(c => c.name).join(', ')}`);
    }
  }
}

// ---------------------------------------------------------------------------
// Local deck mirroring
// ---------------------------------------------------------------------------

// Create the local mirror deck for a summary (or details-derived summary) if
// one does not already exist. Returns the local deck id.
async function ensureLocalDeck(author, summary, dbClient = db) {
  const existing = await dbClient.get(
    `SELECT id FROM decks WHERE user_id = ? AND moxfield_public_id = ?`,
    [author.user_id, summary.publicId]
  );
  if (existing) return existing.id;

  const result = await dbClient.run(
    `INSERT INTO decks (name, description, format, category, accent_color, target_size, user_id, source, moxfield_public_id)
     VALUES (?, ?, ?, 'Moxfield Sync', '#22d3ee', ?, ?, 'moxfield', ?)`,
    [
      summary.name || summary.publicId,
      summary.description || '',
      mfxFormatLabel(summary.format),
      targetSizeForFormat(summary.format),
      author.user_id,
      summary.publicId
    ]
  );
  console.log(`Moxfield sync: created local deck #${result.lastID} "${summary.name}" for ${author.moxfield_user}`);
  return result.lastID;
}

// Keep the tracking row for a deck in step with the remote summary, and link
// the local deck to it. Only creates the local mirror when the deck is
// enabled — an unchecked deck is tracked (so re-enabling is instant) but
// never imported.
async function upsertTrackingRow(author, summary) {
  return withAllocationLock(async () => db.withDedicatedTransaction(async tx => {
    // Discovery is remote work and may finish after the author was removed.
    // Revalidate lifecycle ownership inside the same lock/transaction used by
    // teardown before creating or linking a local mirror.
    const liveAuthor = await tx.get(
      `SELECT * FROM moxfield_authors WHERE id = ? AND user_id = ?`,
      [author.id, author.user_id]
    );
    if (!liveAuthor) return null;

    await tx.run(
      `INSERT INTO moxfield_decks
         (author_id, public_id, name, format, mainboard_count, sideboard_count, maybeboard_count, commander_count, last_updated_at, bindarr_deck_id)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(author_id, public_id) DO UPDATE SET
         name = excluded.name,
         format = excluded.format,
         mainboard_count = excluded.mainboard_count,
         sideboard_count = excluded.sideboard_count,
         maybeboard_count = excluded.maybeboard_count,
         last_updated_at = excluded.last_updated_at,
         bindarr_deck_id = COALESCE(excluded.bindarr_deck_id, moxfield_decks.bindarr_deck_id)`,
      [
        liveAuthor.id,
        summary.publicId,
        summary.name || summary.publicId,
        summary.format || null,
        summary.mainboardCount != null ? parseInt(summary.mainboardCount, 10) : null,
        summary.sideboardCount != null ? parseInt(summary.sideboardCount, 10) : null,
        summary.maybeboardCount != null ? parseInt(summary.maybeboardCount, 10) : null,
        null,
        summary.lastUpdatedAtUtc || null
      ]
    );
    const row = await tx.get(
      `SELECT * FROM moxfield_decks WHERE author_id = ? AND public_id = ?`,
      [liveAuthor.id, summary.publicId]
    );
    // The caller's enabled snapshot may be stale after remote discovery. The
    // tracking row inside this transaction is authoritative: a concurrent
    // disable must never be undone by late deck-list work.
    if (!row.bindarr_deck_id && row.enabled !== 0) {
      const deckId = await ensureLocalDeck(liveAuthor, summary, tx);
      const linked = await tx.run(
        `UPDATE moxfield_decks SET bindarr_deck_id = ? WHERE id = ?`,
        [deckId, row.id]
      );
      if (linked.changes !== 1) {
        throw new Error('Moxfield tracking row disappeared while linking its mirror');
      }
      row.bindarr_deck_id = deckId;
    }
    // Renames on Moxfield propagate to the mirror.
    await tx.run(
      `UPDATE decks SET name = ?, description = ? WHERE id = ? AND moxfield_public_id = ? AND source = 'moxfield'`,
      [summary.name || summary.publicId, summary.description || '', row.bindarr_deck_id, summary.publicId]
    );
    return row;
  }));
}

// Retire a tracked deck: drop the local mirror and its tracking row. Cards stay
// in card_cache (they may be referenced by other decks and by price history).
async function retireDeck(author, publicId) {
  return withAllocationLock(async () => db.withDedicatedTransaction(async tx => {
    const row = await tx.get(`
      SELECT md.bindarr_deck_id, d.checked_out
      FROM moxfield_decks md
      LEFT JOIN decks d ON d.id = md.bindarr_deck_id
      WHERE md.author_id = ? AND md.public_id = ?
    `, [author.id, publicId]);
    if (!row) return false;
    if (row.checked_out) throw allocationConflict('Check this deck in before removing its Moxfield mirror');
    if (row.bindarr_deck_id) {
      await tx.run(`DELETE FROM deck_cards WHERE deck_id = ?`, [row.bindarr_deck_id]);
      await tx.run(`DELETE FROM deck_maybe_cards WHERE deck_id = ?`, [row.bindarr_deck_id]);
      await tx.run(`DELETE FROM decks WHERE id = ? AND source = 'moxfield'`, [row.bindarr_deck_id]);
    }
    await tx.run(`DELETE FROM moxfield_decks WHERE author_id = ? AND public_id = ?`, [author.id, publicId]);
    console.log(`Moxfield sync: retired deck "${publicId}" (removed on Moxfield)`);
    return true;
  }));
}

// ---------------------------------------------------------------------------
// Decklist sync (the slow job)
// ---------------------------------------------------------------------------

// Re-validate the author on Moxfield and reconcile the list of tracked decks:
// create mirrors for new decks (and pull their contents immediately), retire
// deleted ones, refresh the stamp on every tracked row.
async function syncDecklist(authorId, { user } = {}) {
  const author = await db.get(`SELECT * FROM moxfield_authors WHERE id = ?`, [authorId]);
  if (!author || (user && Number(author.user_id) !== Number(user.id))) {
    throw new Error('Moxfield author not found');
  }

  // Resolve the canonical username (and pick up profile name changes).
  const userInfo = await moxfieldApi.getUser(author.moxfield_user);
  await db.run(`UPDATE moxfield_authors SET display_name = ?, profile_image_url = ?, last_error = NULL WHERE id = ?`,
    [userInfo.displayName, userInfo.profileImageUrl, author.id]);

  const summaries = await moxfieldApi.getAuthorDeckSummaries(userInfo.userName);
  let created = 0;
  let kept = 0;

  for (const summary of summaries) {
    if (!summary.publicId) continue;
    const wasTracked = await db.get(`SELECT id FROM moxfield_decks WHERE author_id = ? AND public_id = ?`,
      [author.id, summary.publicId]);
    const liveTracking = await upsertTrackingRow(author, summary);
    if (!liveTracking) {
      return {
        ok: true,
        author: userInfo.userName,
        stale: true,
        reason: 'author-removed-during-sync',
        decks_on_moxfield: summaries.length,
        decks_created: created,
        decks_kept: kept,
        decks_removed: 0
      };
    }
    if (wasTracked) {
      kept += 1;
    } else if (liveTracking.enabled !== 0) {
      created += 1;
      // Brand-new mirror: pull its contents right away rather than waiting for
      // the fast job to notice the unsynced stamp.
      try {
        await pullDeckContent(author, summary.publicId);
      } catch (err) {
        console.warn(`Moxfield sync: initial content pull for "${summary.name}" failed: ${err.message}`);
        await db.run(`UPDATE moxfield_decks SET last_error = ? WHERE author_id = ? AND public_id = ?`,
          [err.message, author.id, summary.publicId]);
      }
    }
  }

  // Decks that vanished on Moxfield.
  let removed = 0;
  const tracked = await db.all(`SELECT public_id FROM moxfield_decks WHERE author_id = ?`, [author.id]);
  const remoteIds = new Set(summaries.map(s => s.publicId));
  for (const row of tracked) {
    if (!remoteIds.has(row.public_id)) {
      if (await retireDeck(author, row.public_id)) removed += 1;
    }
  }

  await db.run(`UPDATE moxfield_authors SET last_decklist_sync_at = CURRENT_TIMESTAMP WHERE id = ?`, [author.id]);
  return {
    ok: true,
    author: userInfo.userName,
    decks_on_moxfield: summaries.length,
    decks_created: created,
    decks_kept: kept,
    decks_removed: removed
  };
}

// ---------------------------------------------------------------------------
// Content sync (the fast job)
// ---------------------------------------------------------------------------

// The per-minute check for one author. One summary call carries every deck's
// lastUpdatedAtUtc; each tracked deck whose stamp moved (or has never been
// mirrored) gets its full contents pulled and mirrored. Decks that vanished
// are retired. When nothing changed, this job made exactly ONE Moxfield
// request and touched only its own tracking rows.
async function runContentSync(authorId, { user } = {}) {
  const author = await db.get(`SELECT * FROM moxfield_authors WHERE id = ?`, [authorId]);
  if (!author || (user && Number(author.user_id) !== Number(user.id))) {
    throw new Error('Moxfield author not found');
  }
  // No ENABLED tracked decks yet (author just added, decklist sync pending, or
  // every deck unchecked): nothing to check. Unchecked decks cost no Moxfield
  // requests.
  const haveDecks = await db.get(`SELECT COUNT(*) AS c FROM moxfield_decks WHERE author_id = ? AND enabled = 1`, [author.id]);
  if (!haveDecks || haveDecks.c === 0) return { ok: true, checked: 0, updated: 0, skipped: 0 };

  const summaries = await moxfieldApi.getAuthorDeckSummaries(author.moxfield_user);
  const byPublicId = new Map(summaries.map(s => [s.publicId, s]));
  const tracked = await db.all(`SELECT * FROM moxfield_decks WHERE author_id = ?`, [author.id]);

  let updated = 0;
  let removed = 0;
  let skipped = 0;
  for (const row of tracked) {
    const summary = byPublicId.get(row.public_id);
    if (!summary) {
      if (await retireDeck(author, row.public_id)) removed += 1;
      continue;
    }
    // Unchecked deck: keep its listing row fresh (name, counts, remote stamp)
    // but never pull contents or create a mirror.
    if (row.enabled === 0) {
      skipped += 1;
      await db.run(
        `UPDATE moxfield_decks SET last_updated_at = ?, name = ?, format = ?,
                mainboard_count = ?, sideboard_count = ?, maybeboard_count = ?
         WHERE id = ?`,
        [
          summary.lastUpdatedAtUtc || null,
          summary.name || row.name,
          summary.format || row.format,
          summary.mainboardCount != null ? parseInt(summary.mainboardCount, 10) : null,
          summary.sideboardCount != null ? parseInt(summary.sideboardCount, 10) : null,
          summary.maybeboardCount != null ? parseInt(summary.maybeboardCount, 10) : null,
          row.id
        ]
      );
      continue;
    }
    // Stamp moved since we last mirrored this deck (or never mirrored it).
    const stamp = summary.lastUpdatedAtUtc || null;
    const needsPull = !row.last_synced_updated_at || stamp !== row.last_synced_updated_at;
    if (needsPull) {
      try {
        await pullDeckContent(author, row.public_id, row);
        updated += 1;
      } catch (err) {
        console.warn(`Moxfield sync: content pull failed for "${row.name}": ${err.message}`);
        await db.run(`UPDATE moxfield_decks SET last_error = ? WHERE id = ?`, [err.message, row.id]);
      }
    }
    // Refresh the stored stamp (and the summary fields) even when no pull
    // happened, so the decklist job's view stays in step.
    await db.run(
      `UPDATE moxfield_decks SET last_updated_at = ?, name = ?, format = ?,
              mainboard_count = ?, sideboard_count = ?, maybeboard_count = ?
       WHERE id = ?`,
      [
        stamp,
        summary.name || row.name,
        summary.format || row.format,
        summary.mainboardCount != null ? parseInt(summary.mainboardCount, 10) : null,
        summary.sideboardCount != null ? parseInt(summary.sideboardCount, 10) : null,
        summary.maybeboardCount != null ? parseInt(summary.maybeboardCount, 10) : null,
        row.id
      ]
    );
  }

  // Mark that we did a check just now, whether or not anything moved — the
  // author-level "last content check" timestamp is what tells a person the
  // per-minute job is alive. (Per-deck last_content_sync_at only advances on
  // an actual pull.)
  await db.run(`UPDATE moxfield_authors SET last_content_check_at = CURRENT_TIMESTAMP WHERE id = ?`, [author.id]);

  return { ok: true, checked: tracked.length, updated, removed, skipped };
}

// Pull one deck's full contents from Moxfield and mirror onto the local deck.
async function pullDeckContent(author, publicId, knownRow = null) {
  let row = knownRow || await db.get(`SELECT * FROM moxfield_decks WHERE author_id = ? AND public_id = ?`,
    [author.id, publicId]);
  if (!row) throw new Error(`Deck ${publicId} is not tracked for ${author.moxfield_user}`);

  const details = await moxfieldApi.getDeckDetails(publicId);
  const entries = extractDeckCards(details);
  const maybeEntries = extractMaybeCards(details);

  // Fetch/cache provider metadata before taking the allocation lock; network I/O
  // must never stall checkouts. From the first mirror-deck read through the final
  // reconciliation write, however, checkout and sync are one ordered operation.
  await backfillMissingCards([...entries, ...maybeEntries]);
  const result = await withAllocationLock(async () => db.withDedicatedTransaction(async tx => {
    // A content pull may spend seconds in remote/cache I/O. Re-read tracking
    // state only after acquiring the same lock used by disable/remove/retire so
    // stale work can never recreate a mirror that was paused or deleted while
    // the request was in flight.
    const liveRow = await tx.get(`
      SELECT md.*
      FROM moxfield_decks md
      JOIN moxfield_authors ma ON ma.id = md.author_id
      WHERE md.author_id = ? AND md.public_id = ? AND ma.user_id = ?
    `, [author.id, publicId, author.user_id]);
    if (!liveRow || !liveRow.enabled) {
      return { ok: true, skipped: true, public_id: publicId, reason: 'tracking-disabled-or-removed' };
    }
    row = liveRow;

  // A stored bindarr_deck_id can dangle if the mirror deck was deleted through
  // the generic deck-delete (which doesn't clear the Moxfield pointer). Writing
  // card rows into a nonexistent deck id would leave orphaned data and a bar
  // that reads 0. Validate the pointer; if it's gone, re-mint the mirror.
  let targetDeckId = row.bindarr_deck_id;
  if (targetDeckId) {
    const alive = await tx.get(
      `SELECT id, checked_out FROM decks WHERE id = ? AND source = 'moxfield'`, [targetDeckId]
    );
    if (alive && alive.checked_out) {
      // Main/side boards stay frozen while built; the maybeboard still syncs.
      await replaceMaybeboard(tx, targetDeckId, maybeEntries);
      return { conflict: true };
    }
    if (!alive) {
      targetDeckId = null;
      await tx.run(`UPDATE moxfield_decks SET bindarr_deck_id = NULL WHERE id = ?`, [row.id]);
    }
  }
  if (!targetDeckId) {
    targetDeckId = await ensureLocalDeck(author, {
      publicId,
      name: details.name || row.name,
      description: details.description || '',
      format: details.format || row.format
    }, tx);
    await tx.run(`UPDATE moxfield_decks SET bindarr_deck_id = ? WHERE id = ?`, [targetDeckId, row.id]);
  }

  // 1. Card metadata was backfilled before taking the allocation lock.

  // 2. Reconcile quantities by logical card identity. Moxfield can name more
  // than one printing of the same game card; retain one representative id and
  // sum those quantities instead of creating parallel deck rows.
  const rawDesired = new Map();
  for (const e of entries) {
    const id = scryboxCardId(e.card);
    rawDesired.set(id, (rawDesired.get(id) || 0) + e.quantity);
  }
  const desiredIds = [...rawDesired.keys()];
  const cached = desiredIds.length
    ? await tx.all(
      `SELECT id, ${sqlCardKey('card_cache')} AS card_key
       FROM card_cache WHERE id IN (${desiredIds.map(() => '?').join(',')})`,
      desiredIds
    )
    : [];
  const keysById = new Map(cached.map(card => [card.id, card.card_key]));
  const desired = new Map(); // logical card key -> representative id + summed quantity
  for (const [id, quantity] of rawDesired) {
    const key = keysById.get(id);
    if (!key) throw new Error(`Card cache metadata is incomplete for ${id}`);
    const existing = desired.get(key);
    if (existing) existing.quantity += quantity;
    else desired.set(key, { cardId: id, quantity });
  }

  const current = await tx.all(`
    SELECT dc.card_id, ${sqlCardKey('current_cc')} AS card_key
    FROM deck_cards dc
    LEFT JOIN card_cache current_cc ON current_cc.id = dc.card_id
    WHERE dc.deck_id = ?
    ORDER BY dc.card_id
  `, [targetDeckId]);
  const currentByKey = new Map();
  for (const row of current) {
    if (!row.card_key || !desired.has(row.card_key)) {
      const removed = await tx.run(`
        DELETE FROM deck_cards
        WHERE deck_id = ? AND card_id = ?
          AND EXISTS (SELECT 1 FROM decks WHERE id = ? AND checked_out = 0)
      `, [targetDeckId, row.card_id, targetDeckId]);
      if (!removed.changes) throw new Error('Deck was checked out while Moxfield changes were syncing');
      continue;
    }
    if (!currentByKey.has(row.card_key)) currentByKey.set(row.card_key, []);
    currentByKey.get(row.card_key).push(row.card_id);
  }

  for (const [key, wanted] of desired) {
    const existingIds = currentByKey.get(key) || [];
    if (existingIds.length > 0) {
      const representative = existingIds.includes(wanted.cardId) ? wanted.cardId : existingIds[0];
      const updated = await tx.run(`
        UPDATE deck_cards
        SET quantity = CASE WHEN card_id = ? THEN ? ELSE 0 END
        WHERE deck_id = ?
          AND card_id IN (${existingIds.map(() => '?').join(',')})
          AND EXISTS (SELECT 1 FROM decks WHERE id = ? AND checked_out = 0)
      `, [representative, wanted.quantity, targetDeckId, ...existingIds, targetDeckId]);
      if (!updated.changes) throw new Error('Deck was checked out while Moxfield changes were syncing');
      await tx.run(
        `DELETE FROM deck_cards
         WHERE deck_id = ? AND quantity <= 0
           AND card_id IN (${existingIds.map(() => '?').join(',')})`,
        [targetDeckId, ...existingIds]
      );
    } else {
      const inserted = await tx.run(`
        INSERT INTO deck_cards (deck_id, card_id, quantity)
        SELECT ?, ?, ?
        WHERE EXISTS (SELECT 1 FROM decks WHERE id = ? AND checked_out = 0)
      `, [targetDeckId, wanted.cardId, wanted.quantity, targetDeckId]);
      if (!inserted.changes) throw new Error('Deck was checked out while Moxfield changes were syncing');
    }
  }

  // 2b. Record which rows are the deck's commander(s), by logical card so a
  // reprint representative still carries the flag.
  const commanderKeys = new Set(entries
    .filter(e => e.board === 'commanders')
    .map(e => keysById.get(scryboxCardId(e.card)))
    .filter(Boolean));
  await markDeckCommanders(tx, targetDeckId, commanderKeys);

  // 2c. Maybeboard: replaced wholesale (its own table, never in deck stats).
  await replaceMaybeboard(tx, targetDeckId, maybeEntries);

  // 3. Refresh the tracking row: the stamp we just mirrored is now "current".
  const stamp = details.lastUpdatedAtUtc || null;
  await tx.run(
    `UPDATE moxfield_decks SET
        last_synced_updated_at = ?, last_content_sync_at = CURRENT_TIMESTAMP, last_error = NULL,
        name = ?, format = ?,
        mainboard_count = ?, sideboard_count = ?, maybeboard_count = ?, commander_count = ?
     WHERE id = ?`,
    [
      stamp,
      details.name || row.name,
      details.format || row.format,
      details.boards && details.boards.mainboard ? details.boards.mainboard.count : null,
      details.boards && details.boards.sideboard ? details.boards.sideboard.count : null,
      details.boards && details.boards.maybeboard ? details.boards.maybeboard.count : null,
      details.boards && details.boards.commanders ? details.boards.commanders.count : null,
      row.id
    ]
  );
  // Name drift between the summary list and the details payload.
  if (details.name) {
    await tx.run(`UPDATE decks SET name = ? WHERE id = ? AND source = 'moxfield'`, [details.name, targetDeckId]);
  }

  const counts = boardCounts(entries);
  return {
    ok: true,
    public_id: publicId,
    deck: details.name || row.name,
    mainboard: counts.mainboard,
    sideboard: counts.sideboard,
    commanders: counts.commanders
  };
  }));
  if (result && result.conflict) throw allocationConflict('Check this deck in before syncing Moxfield changes');
  return result;
}

// Flip one deck's import switch. Disabling removes the local mirror (and its
// card quantities) but keeps the tracking row — the deck stays listed in the
// UI and re-enabling re-imports it on the next content check, with no state to
// reconstruct. Enabling works even for a deck that has never been imported.
async function setDeckEnabled(userId, publicId, enabled) {
  const row = await db.get(
    `SELECT md.*, a.moxfield_user AS author_user
     FROM moxfield_decks md
     JOIN moxfield_authors a ON a.id = md.author_id
     WHERE md.public_id = ? AND a.user_id = ?`,
    [publicId, userId]
  );
  if (!row) throw new Error('Moxfield deck not found for this user');

  if (enabled) {
    await db.run(`UPDATE moxfield_decks SET enabled = 1, last_error = NULL WHERE id = ?`, [row.id]);
    // Imported already: just re-armed. Not imported yet (or re-imported after
    // a disable): pull it now so the user sees the result immediately instead
    // of on the next clock.
    if (row.bindarr_deck_id) {
      return { ok: true, enabled: true, imported: true };
    }
    const author = await db.get(`SELECT * FROM moxfield_authors WHERE id = ?`, [row.author_id]);
    try {
      await pullDeckContent(author, publicId, row);
    } catch (err) {
      await db.run(`UPDATE moxfield_decks SET last_error = ? WHERE id = ?`, [err.message, row.id]);
    }
    const fresh = await db.get(`SELECT bindarr_deck_id FROM moxfield_decks WHERE id = ?`, [row.id]);
    return { ok: true, enabled: true, imported: !!fresh.bindarr_deck_id };
  }

  return withAllocationLock(async () => db.withDedicatedTransaction(async tx => {
    const current = await tx.get(`
      SELECT md.bindarr_deck_id, d.checked_out
      FROM moxfield_decks md
      LEFT JOIN decks d ON d.id = md.bindarr_deck_id
      WHERE md.id = ?
    `, [row.id]);
    if (!current) throw new Error('Moxfield deck not found for this user');
    if (current.checked_out) {
      const conflict = new Error('Check this deck in before disabling its Moxfield mirror');
      conflict.code = 'ALLOCATION_CONFLICT';
      throw conflict;
    }
    if (current.bindarr_deck_id) {
      await tx.run(`DELETE FROM deck_cards WHERE deck_id = ?`, [current.bindarr_deck_id]);
      await tx.run(`DELETE FROM deck_maybe_cards WHERE deck_id = ?`, [current.bindarr_deck_id]);
      await tx.run(`DELETE FROM decks WHERE id = ? AND source = 'moxfield'`, [current.bindarr_deck_id]);
      await tx.run(`UPDATE moxfield_decks SET enabled = 0, bindarr_deck_id = NULL, last_synced_updated_at = NULL, last_error = NULL WHERE id = ?`, [row.id]);
    } else {
      await tx.run(`UPDATE moxfield_decks SET enabled = 0, last_error = NULL WHERE id = ?`, [row.id]);
    }
    console.log(`Moxfield sync: deck "${row.name}" import disabled (mirror removed)`);
    return { ok: true, enabled: false };
  }));
}

// Re-pull one specific deck's contents for a user, regardless of its stamp.
// Finds the tracking row under that user's authors (a public id belongs to one
// author on Moxfield, so this is unique once found).
async function pullDeckContentByPublicId(userId, publicId) {
  const row = await db.get(
    `SELECT md.* FROM moxfield_decks md
     JOIN moxfield_authors a ON a.id = md.author_id
     WHERE md.public_id = ? AND a.user_id = ?`,
    [publicId, userId]
  );
  if (!row) throw new Error('Moxfield deck not found for this user');
  const author = await db.get(`SELECT * FROM moxfield_authors WHERE id = ?`, [row.author_id]);
  return pullDeckContent(author, publicId, row);
}

// ---------------------------------------------------------------------------
// Author management
// ---------------------------------------------------------------------------

// Track a new Moxfield author for this user, then run the first decklist sync
// so the decks appear immediately.
async function addAuthor(userId, username) {
  const clean = String(username || '').trim();
  if (!clean) throw new Error('Moxfield username is required');
  // Validate up-front so the UI gets a real 404 instead of a silently-broken row.
  const userInfo = await moxfieldApi.getUser(clean);
  const existing = await db.get(
    `SELECT id FROM moxfield_authors WHERE user_id = ? AND LOWER(moxfield_user) = LOWER(?)`,
    [userId, userInfo.userName]
  );
  if (existing) {
    const report = await syncDecklist(existing.id, { user: { id: userId } });
    return { author: existing.id, already_tracked: true, ...report };
  }
  const result = await db.run(
    `INSERT INTO moxfield_authors (user_id, moxfield_user, display_name, profile_image_url) VALUES (?, ?, ?, ?)`,
    [userId, userInfo.userName, userInfo.displayName, userInfo.profileImageUrl]
  );
  const report = await syncDecklist(result.lastID, { user: { id: userId } });
  return { author: result.lastID, ...report };
}

// Stop tracking an author: remove every mirrored deck this account produced.
async function removeAuthor(userId, authorId) {
  return withAllocationLock(async () => db.withDedicatedTransaction(async tx => {
    const author = await tx.get(`SELECT * FROM moxfield_authors WHERE id = ? AND user_id = ?`, [authorId, userId]);
    if (!author) throw new Error('Moxfield author not found');
    const tracked = await tx.all(`
      SELECT md.bindarr_deck_id, d.checked_out
      FROM moxfield_decks md
      LEFT JOIN decks d ON d.id = md.bindarr_deck_id
      WHERE md.author_id = ?
    `, [author.id]);
    if (tracked.some(row => row.checked_out)) {
      const conflict = new Error('Check in every mirrored deck before removing this Moxfield author');
      conflict.code = 'ALLOCATION_CONFLICT';
      throw conflict;
    }
    for (const row of tracked) {
      if (row.bindarr_deck_id) {
        await tx.run(`DELETE FROM deck_cards WHERE deck_id = ?`, [row.bindarr_deck_id]);
        await tx.run(`DELETE FROM deck_maybe_cards WHERE deck_id = ?`, [row.bindarr_deck_id]);
        await tx.run(`DELETE FROM decks WHERE id = ? AND source = 'moxfield'`, [row.bindarr_deck_id]);
      }
    }
    // CASCADE removes the moxfield_decks rows.
    await tx.run(`DELETE FROM moxfield_authors WHERE id = ?`, [author.id]);
    return { ok: true, removed_decks: tracked.filter(r => r.bindarr_deck_id).length };
  }));
}

// Status for the UI: every author this user tracks, with per-deck freshness.
async function getStatus(userId) {
  const authors = await db.all(
    `SELECT a.id, a.moxfield_user, a.display_name, a.profile_image_url,
            a.last_decklist_sync_at, a.last_content_check_at, a.last_error, a.created_at
     FROM moxfield_authors a WHERE a.user_id = ? ORDER BY a.created_at DESC`,
    [userId]
  );
  for (const a of authors) {
    // The aggregate describes what is actually being synced; unchecked decks
    // are shown in the deck list but don't count against the "up to date" bar.
    const agg = await db.get(
      `SELECT COUNT(*) AS total,
              SUM(CASE WHEN last_synced_updated_at IS NOT NULL AND last_synced_updated_at = last_updated_at THEN 1 ELSE 0 END) AS current,
              SUM(CASE WHEN last_error IS NOT NULL THEN 1 ELSE 0 END) AS with_errors,
              MAX(last_content_sync_at) AS last_content_sync
       FROM moxfield_decks WHERE author_id = ? AND enabled = 1`,
      [a.id]
    );
    a.total_decks = agg ? agg.total : 0;
    a.current_decks = agg && agg.current ? agg.current : 0;
    a.error_decks = agg && agg.with_errors ? agg.with_errors : 0;
    a.last_content_sync = agg ? agg.last_content_sync : null;
    a.tracked_decks = (await db.get(
      `SELECT COUNT(*) AS c FROM moxfield_decks WHERE author_id = ?`, [a.id]
    ) || { c: 0 }).c;

    const decks = await db.all(
      `SELECT md.public_id, md.name, md.format, md.mainboard_count, md.sideboard_count,
              md.last_updated_at, md.last_synced_updated_at, md.last_content_sync_at, md.last_error,
              md.enabled
       FROM moxfield_decks md WHERE md.author_id = ? ORDER BY md.last_updated_at DESC`,
      [a.id]
    );
    a.decks = decks.map(d => {
      // "Allocated cards" = the cards in this deck's main slot, as Moxfield
      // reports it (includes the commander for EDH, so a full commander deck
      // reads 100). The target is the format's main-slot size: 100 for
      // commander, 75 for constructed formats. A deck that hits its target is
      // "complete"; anything under it is short cards.
      const count = d.mainboard_count != null ? d.mainboard_count : null;
      const target = targetSizeForFormat(d.format);
      return {
        ...d,
        enabled: d.enabled !== 0,
        current: !!(d.last_synced_updated_at && d.last_synced_updated_at === d.last_updated_at),
        card_count: count,
        card_target: target
      };
    });
  }
  return { authors };
}

module.exports = {
  addAuthor,
  removeAuthor,
  getStatus,
  syncDecklist,
  runContentSync,
  setDeckEnabled,
  pullDeckContent,
  pullDeckContentByPublicId,
  // Re-exported from utils/mfxPayload so callers can import everything from one place.
  extractDeckCards,
  extractMaybeCards,
  boardCounts,
  scryboxCardId,
  mfxFormatLabel,
  targetSizeForFormat,
  MIRROR_BOARDS, markDeckCommanders };

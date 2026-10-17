const path = require('path');
const fs = require('fs');
const os = require('os');
const assert = require('assert');
const { spawn } = require('child_process');

// Isolated temp DB and unique port
const tmpDb = path.join(os.tmpdir(), `scrybox-lists-test-${process.pid}.db`);
process.env.DB_PATH = tmpDb;
const port = '3014';

const projectRoot = path.join(__dirname, '../../../');
const db = require('../../src/db');

async function waitForServer(port) {
  const url = `http://localhost:${port}/api/health`;
  for (let i = 0; i < 150; i++) {
    try {
      const res = await fetch(url);
      if (res.ok) return;
    } catch (e) {
      // retry
    }
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  throw new Error(`Server on port ${port} did not start in time`);
}

async function waitForDatabase() {
  for (let i = 0; i < 150; i++) {
    try {
      const admin = await db.get(`SELECT id FROM users WHERE username = ?`, ['admin']);
      if (admin) return admin.id;
    } catch (e) {
      // retry
    }
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  throw new Error('Database did not initialize in time');
}

async function runTests() {
  const server = spawn('node', ['-r', path.join(__dirname, 'scryfall-mock.js'), path.join(projectRoot, 'backend/src/server.js')], {
    env: { ...process.env, PORT: port, DB_PATH: tmpDb }
  });
  server.stderr.on('data', (d) => process.stderr.write(d.toString()));

  try {
    await waitForServer(port);
    const adminId = await waitForDatabase();

    // Authenticated via a planted session, like the other e2e suites.
    const token = 'lists-test-token';
    const expiresAt = new Date();
    expiresAt.setDate(expiresAt.getDate() + 1);
    await db.run(
      `INSERT OR REPLACE INTO sessions (token, user_id, expires_at) VALUES (?, ?, ?)`,
      [token, adminId, expiresAt.toISOString()]
    );
    const authHeaders = { 'Authorization': `Bearer ${token}` };
    const json = { 'Content-Type': 'application/json', 'Authorization': `Bearer ${token}` };

    // Seed card cache + a couple of owned copies (so the missing/owned math
    // in the detail response is testable).
    await db.run(`INSERT OR IGNORE INTO card_cache (id, name, set_id, number) VALUES (?, ?, ?, ?)`,
      ['ls-c1', 'Lightning Bolt', 'jud', '124']);
    await db.run(`INSERT OR IGNORE INTO card_cache (id, name, set_id, number) VALUES (?, ?, ?, ?)`,
      ['ls-c2', "Gaea's Cradle", 'jup', '431']);
    await db.run(`INSERT OR IGNORE INTO card_cache (id, name, set_id, number) VALUES (?, ?, ?, ?)`,
      ['ls-c3', 'Mox Diamond', 'alpha', '101']);
    await db.run(`INSERT INTO collection (card_id, quantity, user_id) VALUES (?, ?, ?)`,
      ['ls-c1', 2, adminId]);

    // --- Auth gate: /api/lists sits behind the gate like every /api route ---
    const resAuth = await fetch(`http://localhost:${port}/api/lists`);
    assert.strictEqual(resAuth.status, 401, 'lists require auth');

    // --- Create a list from pasted text (ManaBox/MTGA shape) ---
    const resC = await fetch(`http://localhost:${port}/api/lists`, {
      method: 'POST',
      headers: json,
      body: JSON.stringify({
        name: 'Buy list',
        game: 'mtg',
        accent_color: '#ef4444',
        icon: 'crown',
        list_text: '4 Lightning Bolt\n2 Gaea\'s Cradle (JUP) 431\n1 Mox Diamond\n1 Card That Is Not Cached',
      })
    });
    assert.strictEqual(resC.status, 201, 'list creation');
    const created = await resC.json();
    assert.strictEqual(created.matched, 3, 'three lines matched');
    assert.deepStrictEqual(created.unmatched, ['Card That Is Not Cached'], 'unmatched reported');
    const listId = created.id;

    // --- Bare names (no qty), "2x", headers, and a Scryfall-only card ---
    const resB = await fetch(`http://localhost:${port}/api/lists`, {
      method: 'POST', headers: json,
      body: JSON.stringify({ name: 'Bare names', list_text: 'Commander\nLightning Bolt\n2x Mox Diamond\nBlack Lotus\nDelver of Secrets\nNot A Real Card' })
    });
    assert.strictEqual(resB.status, 201);
    const bare = await resB.json();
    assert.strictEqual(bare.lines, 5, 'header skipped, five card lines');
    assert.strictEqual(bare.matched, 4, 'bare names, 2x, uncached Scryfall card and DFC front face all match');
    assert.deepStrictEqual(bare.unmatched, ['Not A Real Card']);
    await fetch(`http://localhost:${port}/api/lists/${bare.id}`, { method: 'DELETE', headers: authHeaders });

    // --- List index: counts + accent + game ---
    const resL = await fetch(`http://localhost:${port}/api/lists`, { headers: authHeaders });
    assert.strictEqual(resL.status, 200);
    const lists = await resL.json();
    assert.strictEqual(lists.length, 1);
    assert.strictEqual(lists[0].id, listId);
    assert.strictEqual(lists[0].name, 'Buy list');
    assert.strictEqual(lists[0].total_card_types, 3);
    assert.strictEqual(lists[0].total_cards, 7, '4+2+1');
    assert.strictEqual(lists[0].accent_color, '#ef4444');
    assert.strictEqual(lists[0].icon, 'crown');

    // --- Detail: cards + owned_qty (Lightning Bolt: want 4, own 2) ---
    const resD = await fetch(`http://localhost:${port}/api/lists/${listId}`, { headers: authHeaders });
    assert.strictEqual(resD.status, 200);
    const detail = await resD.json();
    assert.strictEqual(detail.cards.length, 3);
    const bolt = detail.cards.find(c => c.id === 'ls-c1');
    assert.strictEqual(bolt.quantity, 4);
    assert.strictEqual(bolt.owned_qty, 2, 'owned copies surfaced for missing math');
    const cradle = detail.cards.find(c => c.id === 'ls-c2');
    assert.strictEqual(cradle.owned_qty, 0);

    // --- Cardlist export matches the collection export pattern ---
    const resP = await fetch(`http://localhost:${port}/api/lists/${listId}/cardlist`, { headers: authHeaders });
    assert.strictEqual(resP.status, 200);
    assert.strictEqual(resP.headers.get('content-type'), 'text/plain; charset=utf-8');
    const plain = (await resP.text()).trim().split('\n');
    assert.deepStrictEqual(plain, [
      "2 Gaea's Cradle",
      '4 Lightning Bolt',
      '1 Mox Diamond',
    ], 'plain export is name+qty only, sorted by name');

    const resDP = await fetch(`http://localhost:${port}/api/lists/${listId}/cardlist?style=detailed`, { headers: authHeaders });
    assert.strictEqual(resDP.status, 200);
    const detailed = (await resDP.text()).trim().split('\n');
    assert.ok(detailed.includes('4 Lightning Bolt (JUD) 124'), 'set code uppercased + collector number');
    assert.ok(detailed.includes("2 Gaea's Cradle (JUP) 431"));
    const manaboxPattern = /^\d+ .+( \([A-Z0-9]{2,13}\) \d+(\/\/\d+)?)?$/;
    detailed.forEach(line => assert.ok(manaboxPattern.test(line), `not in ManaBox pattern: ${line}`));

    // --- Quantity upserts ---
    const resU1 = await fetch(`http://localhost:${port}/api/lists/${listId}/cards`, {
      method: 'POST', headers: json,
      body: JSON.stringify({ card_id: 'ls-c2', quantity: 5 })
    });
    assert.strictEqual(resU1.status, 200, 'set quantity');
    const resU2 = await fetch(`http://localhost:${port}/api/lists/${listId}/cards`, {
      method: 'POST', headers: json,
      body: JSON.stringify({ card_id: 'ls-c3', quantity: 9 })
    });
    assert.strictEqual(resU2.status, 200, 'set quantity on existing card');
    const resD2 = await fetch(`http://localhost:${port}/api/lists/${listId}`, { headers: authHeaders });
    const detail2 = await resD2.json();
    assert.strictEqual(detail2.cards.find(c => c.id === 'ls-c2').quantity, 5);
    assert.strictEqual(detail2.cards.find(c => c.id === 'ls-c3').quantity, 9);

    // Removing via quantity 0 happens client-side; server removes via DELETE:
    const resR = await fetch(`http://localhost:${port}/api/lists/${listId}/cards/ls-c3`, { method: 'DELETE', headers: authHeaders });
    assert.strictEqual(resR.status, 200);
    const resD3 = await fetch(`http://localhost:${port}/api/lists/${listId}`, { headers: authHeaders });
    const detail3 = await resD3.json();
    assert.strictEqual(detail3.cards.length, 2, 'card removed');

    // --- Rename ---
    const resRn = await fetch(`http://localhost:${port}/api/lists/${listId}`, {
      method: 'PUT', headers: json,
      body: JSON.stringify({ name: 'DDT wants', description: 'draft day', accent_color: '#a855f7' })
    });
    assert.strictEqual(resRn.status, 200);
    const resL2 = await fetch(`http://localhost:${port}/api/lists`, { headers: authHeaders });
    const lists2 = await resL2.json();
    assert.strictEqual(lists2[0].name, 'DDT wants');
    assert.strictEqual(lists2[0].description, 'draft day');

    // --- 404s / validation ---
    assert.strictEqual((await fetch(`http://localhost:${port}/api/lists/99999`, { headers: authHeaders })).status, 404);
    assert.strictEqual((await fetch(`http://localhost:${port}/api/lists`, {
      method: 'POST', headers: json, body: JSON.stringify({ name: '  ' })
    })).status, 400, 'blank name rejected');

    // --- Ownership isolation: a second user sees nothing ---
    const member = await db.get(`SELECT id FROM users WHERE username = 'member'`);
    let memberToken = null;
    if (member) {
      memberToken = 'lists-member-token';
      await db.run(
        `INSERT OR REPLACE INTO sessions (token, user_id, expires_at) VALUES (?, ?, ?)`,
        [memberToken, member.id, expiresAt.toISOString()]
      );
      const resM = await fetch(`http://localhost:${port}/api/lists`, { headers: { 'Authorization': `Bearer ${memberToken}` } });
      assert.strictEqual((await resM.json()).length, 0, 'member sees no lists');
      assert.strictEqual((await fetch(`http://localhost:${port}/api/lists/${listId}`, {
        headers: { 'Authorization': `Bearer ${memberToken}` }
      })).status, 404, 'member cannot read another user\'s list');
      assert.strictEqual((await fetch(`http://localhost:${port}/api/lists/${listId}`, {
        method: 'DELETE', headers: { 'Authorization': `Bearer ${memberToken}` }
      })).status, 404, 'member cannot delete another user\'s list');
    }

    // --- Remove list cards from collection (trade pile) ---
    {
      const r = await fetch(`http://localhost:${port}/api/lists`, { method: 'POST', headers: json,
        body: JSON.stringify({ name: 'Trade pile' }) });
      const pile = (await r.json()).id;
      await db.run(`INSERT INTO collection (card_id, quantity, user_id) VALUES (?, ?, ?)`, ['ls-c2', 1, adminId]);
      await db.run(`INSERT INTO list_cards (list_id, card_id, quantity) VALUES (?, ?, ?)`, [pile, 'ls-c2', 3]);
      await db.run(`INSERT INTO list_cards (list_id, card_id, quantity) VALUES (?, ?, ?)`, [pile, 'ls-c3', 1]);
      const url = `http://localhost:${port}/api/lists/${pile}/remove-from-collection`;
      const dry = await (await fetch(url, { method: 'POST', headers: json, body: JSON.stringify({ dry_run: true }) })).json();
      assert.strictEqual(dry.requested, 4);
      assert.strictEqual(dry.removed, 1, 'only 1 cradle owned, no mox');
      assert.strictEqual(dry.short.length, 2);
      assert.strictEqual((await db.get(`SELECT COUNT(*) n FROM collection WHERE card_id='ls-c2'`)).n, 1, 'dry run is read-only');
      const done = await (await fetch(url, { method: 'POST', headers: json, body: '{}' })).json();
      assert.strictEqual(done.removed, 1);
      assert.strictEqual((await db.get(`SELECT COUNT(*) n FROM collection WHERE card_id='ls-c2'`)).n, 0, 'cradle removed');
      assert.strictEqual((await db.get(`SELECT SUM(quantity) q FROM collection WHERE card_id='ls-c1'`)).q, 2, 'bolts untouched');
      assert.strictEqual((await db.get(`SELECT COUNT(*) n FROM list_cards WHERE list_id=?`, [pile])).n, 2, 'list kept');
    }

    // --- Delete cascade ---
    const resDel = await fetch(`http://localhost:${port}/api/lists/${listId}`, { method: 'DELETE', headers: authHeaders });
    assert.strictEqual(resDel.status, 200);
    const remaining = await db.all(`SELECT * FROM list_cards WHERE list_id = ?`, [listId]);
    assert.strictEqual(remaining.length, 0, 'list_cards cascaded');

    console.log('PASS: lists e2e');
  } finally {
    try { server.kill('SIGKILL'); } catch {}
    try {
      await new Promise(resolve => {
        db.dbConnection.close(() => resolve());
      });
    } catch {}
    for (const suffix of ['', '-wal', '-shm']) {
      try { fs.unlinkSync(tmpDb + suffix); } catch {}
    }
  }
}

runTests()
  .then(() => process.exit(0))
  .catch(err => { console.error(err); process.exit(1); });

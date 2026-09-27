// Keyword glossary: reminder text mined from card_cache.oracle_text.
// Run: `node test/keywords.test.js`
const assert = require('assert');
const os = require('os');
const path = require('path');
const fs = require('fs');

process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-keywords-${process.pid}.db`);
const db = require('../src/db');
const { mineReminders } = require('../src/routes/rules');

const cards = [
  ['a', 'Flying (This creature can\'t be blocked except by creatures with flying or reach.)'],
  ['b', 'Flying (This creature can\'t be blocked except by creatures with flying or reach.)\nTrample'],
  ['c', 'Flying (Old wording.)'],
  ['d', 'Ward {2} (Whenever this creature becomes the target of a spell or ability an opponent controls, counter it unless that player pays {2}.)'],
  ['e', 'Cumulative upkeep {1} (At the beginning of your upkeep, put an age counter on this permanent, then sacrifice it unless you pay its upkeep cost for each age counter on it.)'],
  ['f', 'When this enters, scry 2. (Look at the top two cards...)'],
];

async function main() {
  await db.initDb();
  for (const [id, text] of cards) {
    await db.run('INSERT INTO card_cache (id, name, oracle_text) VALUES (?, ?, ?)', [`kw-${id}`, `Card ${id}`, text]);
  }
  const m = await mineReminders(['Flying', 'Ward', 'Cumulative Upkeep', 'Scry', 'Trample']);
  assert.strictEqual(m.flying.reminder, "This creature can't be blocked except by creatures with flying or reach.", 'most common wording wins');
  assert.strictEqual(m.flying.cards, 3);
  assert.match(m.ward.reminder, /counter it unless that player pays \{2\}/, 'costed keyword keeps its cost');
  assert.match(m['cumulative upkeep'].reminder, /age counter/, 'multi-word keyword resolves');
  assert.strictEqual(m.trample, undefined, 'no reminder when none printed');
  assert.strictEqual(m.scry, undefined, 'reminder must follow the keyword at line start');
  console.log('keywords: ok');
}

main().then(() => { try { fs.unlinkSync(process.env.DB_PATH); } catch {} process.exit(0); })
  .catch(e => { console.error(e); try { fs.unlinkSync(process.env.DB_PATH); } catch {} process.exit(1); });

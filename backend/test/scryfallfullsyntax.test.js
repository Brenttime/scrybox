// Full Scryfall grammar + Tagger (otag:/atag:) coverage for the shared parser,
// and JS-vs-SQL parity for every locally answerable operator on the row shape
// the app actually stores (supertype 'MTG', type words in `subtypes`, colors in
// `types`, identity in `color_identity`, display-word rarities).
// Run: `node test/scryfallfullsyntax.test.js`
const assert = require('assert');
const os = require('os');
const path = require('path');

process.env.SCRYFALL_GAP_SCALE = '0';
process.env.DB_PATH = path.join(os.tmpdir(), `scrybox-fullsyntax-${process.pid}.db`);
const db = require('../src/db');
const rawSql = require('../src/utils/rawQuerySql');
const { analyze, matches, toScryfall, QuerySyntaxError, compileQuery } = require('../../shared/scryfallQuery.js');

const CARDS = [
  { id: 'mtg-t1', name: 'Llanowar Elves', subtypes: ['Creature', 'Elf', 'Druid'], types: ['Green'], color_identity: ['Green'], rarity: 'Common', set_id: 'lea', number: '210', cmc: 1, language: 'English' },
  { id: 'mtg-t2', name: 'Lightning Bolt', subtypes: ['Instant'], types: ['Red'], color_identity: ['Red'], rarity: 'Common', set_id: 'm10', number: '146', cmc: 1, language: 'English' },
  { id: 'mtg-t3', name: 'Kenrith, the Returned King', subtypes: ['Legendary', 'Creature', 'Human', 'Noble'], types: ['White'], color_identity: ['Black', 'Green', 'Red', 'Blue', 'White'], rarity: 'Mythic', set_id: 'eld', number: '303', cmc: 5, language: 'English' },
  { id: 'mtg-t4', name: 'Shivan Reef', subtypes: ['Land'], types: [], color_identity: ['Red', 'Blue'], rarity: 'Rare', set_id: 'dmu', number: '255', cmc: 0, language: 'English' },
  { id: 'mtg-t5', name: 'Sol Ring', subtypes: ['Artifact'], types: [], color_identity: [], rarity: 'Uncommon', set_id: 'c21', number: '263', cmc: 1, language: 'English' },
  { id: 'mtg-t6', name: 'Forest', subtypes: ['Basic', 'Land', 'Forest'], types: [], color_identity: ['Green'], rarity: 'Common', set_id: 'lea', number: '294', cmc: 0, language: 'English' },
  { id: 'mtg-t7', name: 'Fire // Ice', subtypes: ['Instant', 'Instant'], types: ['Red', 'Blue'], color_identity: ['Red', 'Blue'], rarity: 'Uncommon', set_id: 'apc', number: '128', cmc: 4, language: 'English' },
  { id: 'mtg-t8', name: 'Azorius Signet', subtypes: ['Artifact'], types: [], color_identity: ['White', 'Blue'], rarity: 'Special', set_id: 'tsr', number: '400', cmc: 2, language: 'Japanese', printed_name: 'アゾリウスの印鑑' },
  { id: 'mtg-t9', name: 'Dack\'s Duplicate', subtypes: ['Creature', 'Shapeshifter'], types: ['Blue', 'Red'], color_identity: ['Blue', 'Red'], rarity: 'Rare', set_id: 'cns', number: '51a', cmc: 4, language: 'English' },
];
const STORED = CARDS.map(c => ({ ...c, supertype: 'MTG' }));

const jsNames = (q) => STORED.filter(c => matches(c, q)).map(c => c.name).sort();

// ---- 1. Grammar: everything Scryfall documents parses.
const PARSES = [
  'c:wu', 'c>=wu', 'c=wu', 'c<=wu', 'c!=g', 'c:m', 'c=2', 'c:esper', 'c:azorius', 'c:quandrix', 'c:colorless',
  'id:g', 'id<=esper', 'ci:c', 'commander:wu', 'id>=2',
  't:"legendary creature"', 't=creature', 't:elf -t:druid',
  'o:"draw a card"', 'o:/^\\{T\\}:/', 'fo:flying', 'kw:"first strike"', 'm:{G}{G}', 'm>={2}{U}', 'devotion:{g}{g}{g}', 'produces:g',
  'mv=even', 'mv:odd', 'cmc>=3', 'manavalue<2', 'pow>=5', 'tou<pow', 'pt>=10', 'loy=3', 'def:4',
  'r>rare', 'r:s', 'r:b', 'r>=u', 'rarity!=common',
  's:lea', 'e:m10', 'edition:eld', 'cn>100', 'cn:51a', 'b:ktk', 'st:core', 'in:lea', 'cube:vintage',
  'f:edh', 'legal:modern', 'banned:modern', 'restricted:vintage',
  'usd>5', 'eur<1', 'tix>=0.5',
  'a:"rk post"', 'artists>1', 'ft:"sorrow"', 'wm:orzhov', 'border:black', 'frame:2015', 'stamp:oval', 'game:paper',
  'year>=2024', 'date>=2024-01-01', 'new:art', 'prints>50', 'sets>20', 'papersets<3', 'illustrations>3', 'lore:urza',
  'is:foil', 'is:commander', 'not:reprint', 'has:watermark', 'is:land', 'not:creature', 'is:basic-land',
  'layout:split', 'oracleid:4457ed35-7c10-48c8-9776-456485fdf070',
  'otag:ramp', 'oracletag:removal', 'function:card-draw', 'atag:dragon', 'art:squirrel', 'arttag:"sword"',
  '!"lightning bolt"', '!fire', 'name:/^light/', '/bolt$/', 'name=bolt',
  'include:extras', 'unique:prints', 'order:usd', 'direction:desc', 'prefer:newest', 'display:text', 'lang:any', 'lang:ja',
  '(t:elf or t:goblin) c:g', '-(r:c or r:u)', 't:elf and c:g', 'bolt or shock', '-"lightning"',
  'subtype:elf', 'somefuturekeyword:x',
];
for (const q of PARSES) {
  assert.doesNotThrow(() => analyze(q), `${q} should parse`);
}

// ---- 2. Genuine syntax errors are named.
for (const bad of ['(c:g', 'c:g)', '-', 'or', 'and', 't:elf and', '"unterminated', 'o:/unterminated', 'c:yore', 'mv:abc', 'name:', 'set>lea']) {
  assert.throws(() => analyze(bad), (e) => e instanceof QuerySyntaxError, `${bad} must be a QuerySyntaxError`);
}

// ---- 3. Routing: local vs catalog.
const LOCAL = ['c>=wu', 'id<=esper', 't:elf c:g', 'r>rare', 'mv=even', 'cmc>=3', 's:lea cn>100', '!"lightning bolt"', 'lang:ja', 'is:land', 'not:creature', 'unique:prints t:elf', 'c:m', 'c=2'];
for (const q of LOCAL) assert.strictEqual(analyze(q).mode, 'local', `${q} is local`);
const CATALOG = ['otag:ramp', 'atag:dragon', 'art:squirrel', 'function:card-draw', 'o:"draw a card"', 'm:{G}', 'f:edh', 'usd>5', 'a:"rk post"', 'is:foil', 'not:reprint', 'name:/^light/', 'subtype:elf', 'pow>=5'];
for (const q of CATALOG) assert.strictEqual(analyze(q).mode, 'catalog', `${q} is catalog`);

// Printing-level terms intersect the collection by printing id.
assert.strictEqual(analyze('a:"rk post"').printing, true);
assert.strictEqual(analyze('atag:dragon').printing, true);
assert.strictEqual(analyze('otag:ramp').printing, false);
assert.strictEqual(analyze('o:flying f:edh').printing, false);

// ---- 4. Semantics on stored rows (verified against api.scryfall.com behavior).
assert.deepStrictEqual(jsNames('c:wu'), []);
assert.deepStrictEqual(jsNames('c>=ur'), ["Dack's Duplicate", 'Fire // Ice']);
assert.deepStrictEqual(jsNames('c=r'), ['Lightning Bolt']);
assert.deepStrictEqual(jsNames('c:m'), ["Dack's Duplicate", 'Fire // Ice']);
assert.deepStrictEqual(jsNames('c=2'), ["Dack's Duplicate", 'Fire // Ice']);
assert.deepStrictEqual(jsNames('c:c'), ['Azorius Signet', 'Forest', 'Shivan Reef', 'Sol Ring'], 'c:c = no colors');
assert.deepStrictEqual(jsNames('id:g'), ['Forest', 'Llanowar Elves', 'Sol Ring'], 'id:g = identity fits in green');
assert.deepStrictEqual(jsNames('id:izzet'), ["Dack's Duplicate", 'Fire // Ice', 'Lightning Bolt', 'Shivan Reef', 'Sol Ring']);
assert.deepStrictEqual(jsNames('id=wubrg'), ['Kenrith, the Returned King']);
assert.deepStrictEqual(jsNames('id:azorius'), ['Azorius Signet', 'Sol Ring']);
assert.deepStrictEqual(jsNames('t:elf'), ['Llanowar Elves']);
assert.deepStrictEqual(jsNames('t:"legendary creature"'), ['Kenrith, the Returned King']);
assert.deepStrictEqual(jsNames('is:land'), ['Forest', 'Shivan Reef']);
assert.deepStrictEqual(jsNames('is:basic-land'), ['Forest']);
assert.deepStrictEqual(jsNames('not:land t:instant'), ['Fire // Ice', 'Lightning Bolt']);
assert.deepStrictEqual(jsNames('r>rare'), ['Azorius Signet', 'Kenrith, the Returned King'], 'special and mythic rank above rare');
assert.deepStrictEqual(jsNames('r:s'), ['Azorius Signet']);
assert.deepStrictEqual(jsNames('r<=u t:instant'), ['Fire // Ice', 'Lightning Bolt']);
assert.deepStrictEqual(jsNames('mv=even'), ['Azorius Signet', "Dack's Duplicate", 'Fire // Ice', 'Forest', 'Shivan Reef']);
assert.deepStrictEqual(jsNames('cmc>=4'), ["Dack's Duplicate", 'Fire // Ice', 'Kenrith, the Returned King']);
assert.deepStrictEqual(jsNames('s:lea cn>250'), ['Forest']);
assert.deepStrictEqual(jsNames('cn:51a'), ["Dack's Duplicate"]);
assert.deepStrictEqual(jsNames('cn>=50 cn<52'), ["Dack's Duplicate"], 'numeric prefix of 51a');
assert.deepStrictEqual(jsNames('!fire'), ['Fire // Ice'], 'exact name matches a split face');
assert.deepStrictEqual(jsNames('!"lightning bolt"'), ['Lightning Bolt']);
assert.deepStrictEqual(jsNames('!lightning'), [], 'exact is not partial');
assert.deepStrictEqual(jsNames('lang:ja'), ['Azorius Signet']);
assert.deepStrictEqual(jsNames('lang:any t:artifact'), ['Azorius Signet', 'Sol Ring']);
assert.deepStrictEqual(jsNames('アゾリウス'), ['Azorius Signet']);
assert.deepStrictEqual(jsNames('t:elf and c:g'), ['Llanowar Elves']);
assert.deepStrictEqual(jsNames('(t:elf or t:instant) -c:r'), ['Llanowar Elves']);
assert.deepStrictEqual(jsNames('t:elf unique:prints order:name'), ['Llanowar Elves'], 'directives never filter');
assert.throws(() => compileQuery('otag:ramp'), QuerySyntaxError, 'the browser evaluator never guesses catalog terms');

// ---- 5. Upstream serialization: Scryfall receives real Scryfall syntax.
assert.strictEqual(toScryfall('is:land c:g'), 't:land c:g');
assert.strictEqual(toScryfall('is:basic-land'), 't:"basic land"');
assert.strictEqual(toScryfall('not:creature otag:ramp'), '-t:creature otag:ramp');
assert.strictEqual(toScryfall('function:ramp art:dragon'), 'function:ramp art:dragon', 'typed aliases are kept verbatim');
assert.strictEqual(toScryfall('o:"draw a card" -(r:c or r:u)'), 'o:"draw a card" -(r:c or r:u)');
assert.strictEqual(toScryfall('(t:elf or t:goblin) c>=g'), '(t:elf or t:goblin) c>=g');
assert.strictEqual(toScryfall('t:elf and c:g'), 't:elf c:g');
assert.strictEqual(toScryfall('!"lightning bolt"'), '!"lightning bolt"');
assert.strictEqual(toScryfall('name:/^light/ o:/\\{T\\}/'), 'name:/^light/ o:/\\{T\\}/');
assert.strictEqual(toScryfall('m:{2}{U}{U} pow>=tou'), 'm:{2}{U}{U} pow>=tou');
assert.strictEqual(toScryfall('a:"rk post" unique:prints'), 'a:"rk post" unique:prints');
assert.strictEqual(toScryfall('-is:foil'), '-is:foil');
assert.strictEqual(toScryfall('bolt or shock'), 'bolt or shock');

// ---- 6. SQL parity: the database answers exactly what the JS evaluator does.
async function main() {
  await db.initDb();
  for (const c of STORED) {
    await db.run(
      `INSERT INTO card_cache (id, oracle_id, scryfall_search_eligible, name, printed_name, supertype, subtypes, types, rarity,
        set_id, set_name, number, image_url, cmc, color_identity, language, last_updated)
       VALUES (?, ?, 1, ?, ?, 'MTG', ?, ?, ?, ?, ?, ?, '', ?, ?, ?, CURRENT_TIMESTAMP)`,
      [c.id, `oracle-${c.id}`, c.name, c.printed_name || null, JSON.stringify(c.subtypes), JSON.stringify(c.types), c.rarity,
        c.set_id, c.set_id.toUpperCase(), c.number, c.cmc, JSON.stringify(c.color_identity), c.language],
    );
  }
  const PARITY = [
    ...LOCAL, 'c:wu', 'c>=ur', 'c=r', 'c:c', 'c!=r', 'c<ur', 'c>r', 'id:g', 'id:izzet', 'id=wubrg', 'id:azorius', 'id>=2', 'id<2',
    't:"legendary creature"', 'is:basic-land', 'not:land t:instant', 'r:s', 'r<=u t:instant', 'r!=common', 'rarity:mythic',
    's:lea cn>250', 'cn:51a', 'cn>=50 cn<52', '!fire', '!lightning', 'lang:any t:artifact', 'アゾリウス',
    't:elf and c:g', '(t:elf or t:instant) -c:r', '-(r:c or r:u)', 'mv:odd', 'mv!=1', 'bolt or elves', 'name!=bolt', 's!=lea',
  ];
  for (const q of PARITY) {
    const { ast } = analyze(q);
    const { whereSql, params } = rawSql.compileWhere(ast);
    const rows = await db.all(`SELECT cc.name FROM card_cache cc WHERE 1 ${whereSql}`, params);
    assert.deepStrictEqual(rows.map(r => r.name).sort(), jsNames(q), `${q}: SQL and JS must agree`);
  }
  console.log(`scryfallfullsyntax.test.js: all assertions passed (${PARSES.length} grammar forms, ${PARITY.length} SQL parity queries)`);
}

main().then(() => process.exit(0)).catch((err) => { console.error(err); process.exit(1); });

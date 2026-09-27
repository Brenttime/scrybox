// The shared Scryfall-syntax evaluator (shared/scryfallQuery.js), which the
// collection screen uses to filter the cards the user already owns. No
// framework — plain node + assert. Run: `node test/scryfallquery.test.js`
const assert = require('assert');
const { QuerySyntaxError, compileQuery, matches } = require('../../shared/scryfallQuery.js');

const LAND = {
  name: 'Foret', supertype: 'land', subtypes: ['Basic', 'Land'], types: [],
  rarity: 'basic', set_id: 'lea', number: '35', cmc: null, color_identity: [], language: 'English',
};
const BOLT = {
  name: 'Lightning Bolt', supertype: '', subtypes: ['Instant'], types: ['Red'],
  rarity: 'common', set_id: 'lea', number: '56', cmc: 1, color_identity: ['Red'], language: 'English',
};
const BOLT_JA = { ...BOLT, language: 'Japanese', printed_name: '稲妻' };
const SPLIT = {
  name: 'The Legend of Yangchen // Avatar Yangchen', supertype: '', subtypes: ['Legendary', 'Creature', 'Human', 'Wizard'],
  types: ['White', 'Blue'], rarity: 'mythic rare', set_id: 'tla', number: '27', cmc: 2,
  color_identity: ['White', 'Blue'], language: 'English',
};
const SECRET = {
  name: 'Xyzzy', supertype: '', subtypes: ['Creature', 'Beast'], types: ['Green'],
  rarity: 'secret rare', set_id: 'mh3', number: '288', cmc: 3, color_identity: ['Green'], language: 'English',
};
const FOLIO = {
  name: 'Folio', supertype: '', subtypes: ['Enchantment'], types: [],
  rarity: 'common', set_id: 'mh2', number: '285', cmc: 0, color_identity: [], language: 'English',
};
const ALL = [LAND, BOLT, BOLT_JA, SPLIT, SECRET, FOLIO];
const names = (q) => ALL.filter(c => matches(c, q)).map(c => `${c.name}|${c.language}`);

function only(...namesMatch) {
  return new Set(namesMatch);
}

// --- plain words: partial name match against the ENGLISH name and the
// --- printed name, both languages, in the same collection row.
assert.deepStrictEqual(new Set(names('bolt')), only('Lightning Bolt|English', 'Lightning Bolt|Japanese'), 'word matches both printings');
assert.deepStrictEqual(new Set(names('稲妻')), only('Lightning Bolt|Japanese'), 'printed_name is searchable');
assert.strictEqual(names('nonexistentname').length, 0);

// --- quoted phrases (and the name: operator, which must agree with them).
assert.deepStrictEqual(new Set(names('"legend of yangchen"')), only('The Legend of Yangchen // Avatar Yangchen|English'));
assert.deepStrictEqual(new Set(names('name:legend')), only('The Legend of Yangchen // Avatar Yangchen|English'), 'name: behaves like a quoted phrase');

// --- is: / type:
assert.deepStrictEqual(new Set(names('is:land')), only('Foret|English'));
assert.deepStrictEqual(new Set(names('is:basic-land')), only('Foret|English'), 'is:basic-land reads the type line as "basic land"');
assert.deepStrictEqual(new Set(names('type:creature')), only('The Legend of Yangchen // Avatar Yangchen|English', 'Xyzzy|English'));
assert.deepStrictEqual(new Set(names('type:legendary')), only('The Legend of Yangchen // Avatar Yangchen|English'));

// --- color / c: (multi-color rows match either)
assert.deepStrictEqual(new Set(names('color:g')), only('Xyzzy|English'));
assert.deepStrictEqual(new Set(names('c:r or c:w')), only('Lightning Bolt|English', 'Lightning Bolt|Japanese', 'The Legend of Yangchen // Avatar Yangchen|English'));
assert.deepStrictEqual(new Set(names('colorless')), only('Foret|English', 'Folio|English'));
assert.deepStrictEqual(new Set(names('color:c')), only('Foret|English', 'Folio|English'), 'color:c means colorless like Scryfall');

// --- rarity: (letter aliases + full words, incl. mythic/secret two-word rarities)
assert.deepStrictEqual(new Set(names('r:m')), only('The Legend of Yangchen // Avatar Yangchen|English'), 'r:m = mythic, not a prefix game');
assert.deepStrictEqual(new Set(names('rarity:rare')), only('Xyzzy|English'), 'r:rare is exactly rare (Scryfall: mythics excluded); secret rare ranks as rare');
assert.deepStrictEqual(new Set(names('r>=rare')), only('The Legend of Yangchen // Avatar Yangchen|English', 'Xyzzy|English'), 'rarity comparators');
assert.deepStrictEqual(new Set(names('rarity:secret')), only('Xyzzy|English'), 'secret rare matches rarity:secret');
assert.deepStrictEqual(new Set(names('r:c')), only('Lightning Bolt|English', 'Lightning Bolt|Japanese', 'Folio|English'));

// --- set: / number:
assert.deepStrictEqual(new Set(names('set:lea')), only('Foret|English', 'Lightning Bolt|English', 'Lightning Bolt|Japanese'));
assert.deepStrictEqual(new Set(names('number:35')), only('Foret|English'));
assert.deepStrictEqual(new Set(names('number:035')), only('Foret|English'), 'leading-zero tolerance both directions');
assert.deepStrictEqual(new Set(names('number:288')), only('Xyzzy|English'));

// --- lang: / language:
assert.deepStrictEqual(new Set(names('lang:ja')), only('Lightning Bolt|Japanese'), 'code form');
assert.deepStrictEqual(new Set(names('language:japanese')), only('Lightning Bolt|Japanese'), 'name form');
assert.deepStrictEqual(new Set(names('lang:zhs')), new Set(), 'unknown-language spelling parses fine, matches nothing');

// --- mv: / cmc: (m: is Scryfall's MANA COST operator and routes to the catalog)
assert.deepStrictEqual(new Set(names('mv:1')), only('Lightning Bolt|English', 'Lightning Bolt|Japanese'));
assert.deepStrictEqual(new Set(names('cmc:0')), only('Folio|English'));
assert.deepStrictEqual(new Set(names('cmc>=1')), only('Lightning Bolt|English', 'Lightning Bolt|Japanese', 'The Legend of Yangchen // Avatar Yangchen|English', 'Xyzzy|English'), 'cards without cmc never match; comparators work');

// --- composition: implicit AND, or, negation, groups
assert.deepStrictEqual(new Set(names('is:land rarity:basic')), only('Foret|English'));
assert.deepStrictEqual(new Set(names('r:c set:lea')), only('Lightning Bolt|English', 'Lightning Bolt|Japanese'), 'AND across operators');
assert.deepStrictEqual(new Set(names('set:lea -c:r')), only('Foret|English'), 'negation excludes');
assert.deepStrictEqual(new Set(names('(is:land or is:creature) r:u')), only(), 'AND binds tighter than or');
assert.deepStrictEqual(new Set(names('r:c or r:m')), only('Lightning Bolt|English', 'Lightning Bolt|Japanese', 'Folio|English', 'The Legend of Yangchen // Avatar Yangchen|English'));
assert.deepStrictEqual(new Set(names('-(r:c)')), only('Foret|English', 'The Legend of Yangchen // Avatar Yangchen|English', 'Xyzzy|English'), 'negated group');
assert.deepStrictEqual(new Set(names('-is:creature is:legendary')), new Set(), 'negation applies inside the AND');
assert.deepStrictEqual(new Set(names('type:legendary -c:g')), only('The Legend of Yangchen // Avatar Yangchen|English'), 'negated operator value');

// --- errors: unknown operators and unparseable shapes are NAMED, not silent.
for (const bad of ['bogus:1', 'name:', 'is:', '(r:c', 'r:c)', '-', '"unterminated', 'or']) {
  assert.throws(
    () => compileQuery(bad),
    (err) => err instanceof QuerySyntaxError,
    `expected QuerySyntaxError for ${JSON.stringify(bad)}`
  );
}
assert.throws(() => matches(BOLT, ''), (err) => err instanceof QuerySyntaxError, 'empty query');

// --- compileQuery is stable: the same string returns an equivalent predicate.
const pred1 = compileQuery('r:c set:lea');
const pred2 = compileQuery('set:lea r:c');
assert.strictEqual(pred1(BOLT), pred2(BOLT), 'AND is order-independent');

console.log('scryfallquery.test.js: all assertions passed');

// Pasted list text -> { qty, name, setRef } lines. Accepts the shapes people
// actually paste: "4 Lightning Bolt", "4x Lightning Bolt", "Lightning Bolt"
// (qty 1), and an optional "(SET) 123" tail. Blank lines, "//" / "#" comments
// and section headers ("Deck", "Sideboard:", "Commander") are ignored.
const HEADER = /^(deck|main ?deck|maindeck|sideboard|commanders?|companion|maybeboard|tokens?|about)\s*:?\s*$/i;

function parseListLine(raw) {
  const line = String(raw || '').replace(/\s+/g, ' ').trim();
  if (!line || line.startsWith('//') || line.startsWith('#') || HEADER.test(line)) return null;
  let qty = 1;
  let rest = line;
  const q = line.match(/^(\d+)\s*x?\s+(.+)$/i);
  if (q) {
    qty = Math.max(1, parseInt(q[1], 10) || 1);
    rest = q[2].trim();
  }
  let setRef = '';
  let set = '';
  let number = '';
  const ref = rest.match(/\s*\(([A-Za-z0-9]{2,13})\)\s*([A-Za-z0-9-]+(?:\/\/\d+)?)?\s*(\*[A-Z]\*)?\s*$/i);
  if (ref) {
    setRef = ref[0].trim();
    set = ref[1].toLowerCase();
    number = ref[2] || '';
    rest = rest.slice(0, rest.length - ref[0].length).trim();
  }
  if (!rest) return null;
  return { qty, name: rest, setRef, set, number };
}

function parseListText(text) {
  return String(text || '').split(/\r?\n/).map(parseListLine).filter(Boolean);
}

// Cache lookup by full name or by either face of a double-faced card.
async function findCachedCard(db, name) {
  return db.get(
    `SELECT id FROM card_cache
     WHERE LOWER(name) = LOWER(?) OR LOWER(name) LIKE LOWER(?) OR LOWER(name) LIKE LOWER(?)
     ORDER BY CASE WHEN LOWER(name) = LOWER(?) THEN 0 ELSE 1 END
     LIMIT 1`,
    [name, `${name} // %`, `% // ${name}`, name]
  );
}

module.exports = { parseListLine, parseListText, findCachedCard };

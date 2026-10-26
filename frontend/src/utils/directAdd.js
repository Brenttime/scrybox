// Add Cards search box: typed shortcuts that add a card on Enter instead of
// searching. Pure so it can be tested without a DOM.
//
//   ltr 246            -> set LTR, collector number 246
//   2 sol ring foil    -> 2 copies of Sol Ring, foil
//   sol ring cmm       -> Sol Ring from CMM (trailing known set code)
//   3 ltr 246 lp jp    -> qty, condition and language tokens anywhere
//
// Anything that looks like Scryfall syntax (t:creature, c<=2, -is:digital ...)
// or a bare name without a shortcut token is NOT a direct add: Enter searches.
const SYNTAX_RE = /(^|\s)(?:-|\(|"|!|\/|(?:or|and)(?=\s|$)|[a-z_]+(?:!=|<=|>=|[:=<>]))/i;

const FINISH = { foil: 'Holofoil', etched: 'Holofoil', nonfoil: 'Normal' };
const CONDITION = {
  nm: 'Near Mint', lp: 'Lightly Played', mp: 'Moderately Played', hp: 'Heavily Played', dmg: 'Damaged',
};
const LANG = {
  en: 'English', jp: 'Japanese', ja: 'Japanese', de: 'German', fr: 'French', es: 'Spanish', sp: 'Spanish',
  it: 'Italian', pt: 'Portuguese', ko: 'Korean', kr: 'Korean', ru: 'Russian',
  zht: 'Chinese (Traditional)', ct: 'Chinese (Traditional)', zhs: 'Chinese (Simplified)', cs: 'Chinese (Simplified)',
};
const SET_RE = /^[a-z0-9]{2,6}$/i;
const NUMBER_RE = /^\d{1,4}[a-z★†]?$/i;
const QTY_RE = /^(\d{1,3})x?$/i;

// knownSets: Set (or array) of lower-case set codes. Without it a bare
// `<code> <number>` still parses (the pattern is unambiguous), but a trailing
// set code after a name needs to be a known set — "sol ring" must not read
// "ring" as a set.
export function parseDirectAdd(input, knownSets) {
  const text = String(input || '').trim();
  if (!text || SYNTAX_RE.test(text)) return null;
  const sets = knownSets instanceof Set ? knownSets : new Set((knownSets || []).map(s => String(s).toLowerCase()));
  let tokens = text.split(/\s+/);
  let qty = 1;
  let qtyGiven = false;
  const q = tokens[0].match(QTY_RE);
  // A leading number is a quantity only when more follows and the next token
  // is not itself a collector number ("2 246" means nothing).
  if (q && tokens.length > 1 && !(tokens.length === 2 && NUMBER_RE.test(tokens[1]))) {
    qty = Math.max(1, parseInt(q[1], 10));
    qtyGiven = true;
    tokens = tokens.slice(1);
  }
  let printing = null;
  let condition = null;
  let language = null;
  // Modifier tokens are peeled from the END only, so a name like "Foil" or
  // "Lp" inside a card name is never eaten mid-string.
  for (;;) {
    if (tokens.length < 2) break;
    const last = tokens[tokens.length - 1].toLowerCase();
    if (!printing && FINISH[last]) { printing = FINISH[last]; tokens.pop(); continue; }
    if (!condition && CONDITION[last]) { condition = CONDITION[last]; tokens.pop(); continue; }
    if (!language && LANG[last] && last !== 'en' && !(tokens.length === 2 && NUMBER_RE.test(tokens[0]))) {
      language = LANG[last]; tokens.pop(); continue;
    }
    break;
  }
  const modifiers = { qty, printing, condition, language };
  // `<set> <number>`
  if (tokens.length === 2 && SET_RE.test(tokens[0]) && /[a-z]/i.test(tokens[0]) && NUMBER_RE.test(tokens[1])
      && (sets.size === 0 || sets.has(tokens[0].toLowerCase()))) {
    return { kind: 'number', set: tokens[0].toLowerCase(), number: tokens[1].toLowerCase(), ...modifiers };
  }
  let set = null;
  if (tokens.length >= 2) {
    const last = tokens[tokens.length - 1].toLowerCase();
    if (sets.has(last)) { set = last; tokens.pop(); }
  }
  const name = tokens.join(' ').trim();
  if (!name || name.length < 2) return null;
  // A plain name is a search; it takes a shortcut token to become an add.
  if (!set && !qtyGiven && !printing && !condition && !language) return null;
  return { kind: 'name', name, set, ...modifiers };
}

// The Scryfall query that finds the printing(s) a direct add means.
export function directAddQuery(d) {
  if (!d) return '';
  if (d.kind === 'number') return `set:${d.set} cn:${d.number}`;
  const safe = d.name.replace(/"/g, '');
  return `!"${safe}"${d.set ? ` set:${d.set}` : ''} unique:prints -is:digital`;
}

// Pick the finish a one-tap add should record: the requested one when the
// printing exists in it, otherwise whatever the printing does come in.
export function pickPrinting(card, wanted = 'Normal') {
  const f = Array.isArray(card?.finishes) ? card.finishes : null;
  if (!f || f.length === 0) return wanted;
  if (wanted === 'Normal') return f.includes('nonfoil') ? 'Normal' : 'Holofoil';
  return (f.includes('foil') || f.includes('etched')) ? 'Holofoil' : 'Normal';
}

// Newest printing first; ties keep the server order.
export function sortByRelease(cards) {
  return cards
    .map((c, i) => [c, i])
    .sort((a, b) => String(b[0].released_at || '').localeCompare(String(a[0].released_at || '')) || a[1] - b[1])
    .map(([c]) => c);
}

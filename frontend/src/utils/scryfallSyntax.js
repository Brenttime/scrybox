// One box speaks two languages: a plain card name/number/set, or Scryfall
// syntax. It looks like syntax when it carries an operator token (set:lea,
// cmc>=3, c!=g, otag:ramp), a quoted phrase, an exact-name "!", a /regex/,
// a parenthesized group, a bare "or"/"and", or a leading "-". Real card names
// never contain those shapes, so detection cannot misfire on a name. A string
// that LOOKS like syntax but does not parse is reported by the shared parser.
//
// Shared by every surface that takes the box (collection filter, Add Cards,
// deck builder, lists), so all of them agree on what "this is a query" means.
const SCRYFALL_SYNTAX_RE = /(^|\s)(?:-|\(|"|!|\/|(?:or|and)(?=\s|$)|[a-z_]+(?:!=|<=|>=|[:=<>]))/i;

export function looksLikeSyntax(value) {
  return SCRYFALL_SYNTAX_RE.test(String(value || '').trim());
}

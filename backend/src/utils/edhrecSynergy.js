// EDHREC commander-page helpers for the deck Synergy map.
// json.edhrec.com sends no CORS headers, so the server fetches the page and
// hands the browser only the cards whose synergy clears the threshold
// (Omenpath's rule: synergy >= 0.35).
const SYNERGY_THRESHOLD = 0.35;

// "Atraxa, Praetors' Voice" -> "atraxa-praetors-voice" (EDHREC page slug).
function commanderSlug(name) {
  if (!name) return '';
  let s = String(name).split(' // ')[0].toLowerCase();
  try { s = s.normalize('NFD').replace(/\p{Diacritic}+/gu, ''); } catch { /* old runtimes */ }
  s = s.replace(/['’]/g, '').replace(/[^a-z0-9\s]/g, ' ').replace(/\s+/g, ' ').trim();
  return s.replace(/ /g, '-');
}

// Pull { name, synergy } pairs (synergy >= threshold) from an EDHREC commander JSON.
function synergyCards(data, threshold = SYNERGY_THRESHOLD) {
  const lists = data?.container?.json_dict?.cardlists;
  if (!Array.isArray(lists)) return [];
  const best = new Map();
  for (const list of lists) {
    if (!list || !Array.isArray(list.cardviews)) continue;
    for (const c of list.cardviews) {
      if (!c || typeof c !== 'object' || !c.name) continue;
      const syn = typeof c.synergy === 'string' ? parseFloat(c.synergy) : c.synergy;
      if (typeof syn !== 'number' || !Number.isFinite(syn) || syn < threshold) continue;
      const prev = best.get(c.name);
      if (prev === undefined || syn > prev) best.set(c.name, syn);
    }
  }
  return [...best.entries()]
    .map(([name, synergy]) => ({ name, synergy: Math.round(synergy * 100) / 100 }))
    .sort((a, b) => b.synergy - a.synergy || a.name.localeCompare(b.name));
}

module.exports = { SYNERGY_THRESHOLD, commanderSlug, synergyCards };

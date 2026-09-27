// Where the user last was, per account. The app has no router: a refresh
// throws the SPA away and App boots on Dashboard by default, which teleports
// you out of whatever you were doing. Remembering the tab — and the deck or
// list you were standing inside — lets the boot put you back.
//
// Keyed per user so a shared device does not cross places. The id of the open
// deck/list lives in its own key: those views rewrite it every time they open
// something, while the tab entry is only rewritten when tabs switch, and a
// tab entry saved before the last open would otherwise be stale.

const deckKey = 'bindarr_last_deck';
const listKey = 'bindarr_last_list';

// Keyed by USERNAME, not the raw bindarr_user blob: the stored user JSON gets
// rewritten (share token rotation, profile edits) and the key must survive that.
// 'anon' never happens while logged in — App clears the entry at logout.
function viewKey() {
  let who = 'anon';
  try {
    who = (JSON.parse(localStorage.getItem('bindarr_user')) || {}).username || 'anon';
  } catch { /* missing/corrupt user JSON: anon bucket */ }
  return 'scrybox_view_' + who;
}

// Storage is best-effort everywhere here: a full or blocked localStorage must
// never break navigation, it only costs the convenience.
function save(raw) {
  try { localStorage.setItem(viewKey(), raw); } catch { /* private mode, quota */ }
}

function readRaw() {
  try { return localStorage.getItem(viewKey()); } catch { return null; }
}

// The tab itself, for App's boot state. Anything unrecognized lands on
// Dashboard, so a renamed tab or garbage entry cannot crash the first render.
export function getRememberedTab() {
  const raw = readRaw();
  if (!raw) return 'dashboard';
  let tab = raw;
  try { tab = (JSON.parse(raw).tab || raw); } catch { /* plain string */ }
  const known = ['dashboard', 'add-cards', 'collection', 'deckbuilder', 'lists', 'limited', 'rules', 'keywords', 'settings'];
  return known.includes(tab) ? tab : 'dashboard';
}

// Full record for the deep views (deck/list id). Returns {tab, deckId?, listId?}.
export function getRememberedView() {
  const raw = readRaw();
  const base = { tab: getRememberedTab() };
  try {
    const parsed = JSON.parse(raw);
    if (parsed && parsed.tab === 'deckbuilder' && parsed.deckId) base.deckId = String(parsed.deckId);
    if (parsed && parsed.tab === 'lists' && parsed.listId) base.listId = String(parsed.listId);
  } catch { /* plain tab string: no deep id */ }
  return base;
}

// Tab switch: the open deck/list is only restored across a REFRESH. Moving
// Lists -> Deck Builder -> Lists should land on the index, not reopen the last
// list, so every deliberate tab change forgets both open ids first.
export function forgetOpenSubviews() {
  try {
    localStorage.setItem(deckKey, '');
    localStorage.setItem(listKey, '');
  } catch { /* best effort */ }
}

// Called on every tab switch and at login. Deck/list entries carry the id the
// sub-view last opened so a refresh mid-edit reopens the same deck, not just
// the same tab.
export function rememberView(tab) {
  if (tab === 'deckbuilder' || tab === 'lists') {
    const idKey = tab === 'deckbuilder' ? deckKey : listKey;
    let openId = null;
    try { openId = localStorage.getItem(idKey) || null; } catch { /* best effort */ }
    const entry = { tab };
    if (openId) entry[tab === 'deckbuilder' ? 'deckId' : 'listId'] = openId;
    save(JSON.stringify(entry));
    return;
  }
  save(tab);
}

// Logout / share route: forget this account's place. Takes the username while
// the caller still knows it — at logout the user has usually been removed from
// storage already. No username: only the CURRENT user's key goes; a stale
// other-user entry must survive so logging back in restores that account's place.
export function clearRememberedView(username) {
  try {
    if (username) localStorage.removeItem('scrybox_view_' + username);
    else localStorage.removeItem(viewKey());
  } catch { /* best effort */ }
}

// Sub-view open/close bookkeeping (DeckBuilder / Lists). The open id lands in
// its own key AND re-writes the tab entry immediately: tab switches are the
// only other save point, so opening a deck and refreshing without touching the
// nav would otherwise restore a tab entry snapshotted before the deck opened.
export function rememberOpen(tab, id) {
  const idKey = tab === 'deckbuilder' ? deckKey : listKey;
  try { localStorage.setItem(idKey, String(id)); } catch { /* best effort */ }
  if (getRememberedTab() === tab) rememberView(tab);
}

export function clearOpen(tab) {
  const idKey = tab === 'deckbuilder' ? deckKey : listKey;
  try { localStorage.setItem(idKey, ''); } catch { /* best effort */ }
  if (getRememberedTab() === tab) rememberView(tab);
}

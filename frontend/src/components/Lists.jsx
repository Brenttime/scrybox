import { useState, useEffect, useRef } from 'react';
import { looksLikeSyntax } from '../utils/scryfallSyntax';
import {
  Plus, Trash2, X, ChevronLeft, Search, ListChecks, Copy, Pencil,
  Layers, Minus, ShoppingCart, Wand2, DollarSign, PackageMinus, PackagePlus,
  Star, Heart, Gift, Flame, Crown, Gem, Swords, Shield, Sparkles, Bookmark, Trophy, Skull, Zap, Target, Package, Coins,
} from 'lucide-react';
import OverflowMenu from './OverflowMenu';
import CardImage from './CardImage';
import { useBackGuard } from '../utils/useBackGuard';
import { getRememberedView, rememberOpen, clearOpen } from '../utils/viewMemory';
import { displayName, setReference } from '../utils/languages';
import { useT } from '../utils/i18n';
import { cardKey, findSameCard } from '../utils/cardIdentity';
import { buildManapoolUrl } from '../utils/manapoolUrl';
import { buildTcgMassEntryUrl } from '../utils/tcgMassEntryUrl';
import { deckMinimumValueText, deckMinimumValueHint } from '../utils/deckMinimumValue';
import { priceText } from '../utils/formatPrice';

export const LIST_ICONS = [
  ['list-checks', ListChecks], ['star', Star], ['heart', Heart], ['shopping-cart', ShoppingCart],
  ['gift', Gift], ['flame', Flame], ['crown', Crown], ['gem', Gem], ['swords', Swords],
  ['shield', Shield], ['sparkles', Sparkles], ['wand', Wand2], ['bookmark', Bookmark],
  ['trophy', Trophy], ['skull', Skull], ['zap', Zap], ['target', Target], ['package', Package],
  ['coins', Coins], ['layers', Layers],
];
const ListIcon = ({ name, ...props }) => {
  const Icon = (LIST_ICONS.find(([k]) => k === name) || LIST_ICONS[0])[1];
  return <Icon {...props} />;
};
const IconPicker = ({ value, onChange, accent, t }) => (
  <div className="form-group">
    <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', fontWeight: 600 }}>{t('lists.icon')}</label>
    <div className="list-icon-picker" role="radiogroup" aria-label={t('lists.icon')} style={{ display: 'flex', gap: '0.35rem', flexWrap: 'wrap', paddingTop: '0.2rem' }}>
      {LIST_ICONS.map(([key, Icon]) => {
        const on = (value || 'list-checks') === key;
        return (
          <button key={key} type="button" role="radio" aria-checked={on} title={key} aria-label={key} data-icon={key}
            onClick={() => onChange(key)}
            style={{ width: '32px', height: '32px', borderRadius: '8px', display: 'flex', alignItems: 'center', justifyContent: 'center', cursor: 'pointer', padding: 0,
              background: on ? `${accent}33` : 'rgba(0,0,0,0.3)', border: on ? `1.5px solid ${accent}` : '1.5px solid rgba(255,255,255,0.12)', color: on ? accent : 'var(--text-secondary)' }}>
            <Icon size={16} />
          </button>
        );
      })}
    </div>
  </div>
);

const ACCENTS = [
  { name: 'Emerald', hex: '#10b981' },
  { name: 'Gold', hex: '#eab308' },
  { name: 'Blue', hex: '#3b82f6' },
  { name: 'Red', hex: '#ef4444' },
  { name: 'Purple', hex: '#a855f7' },
  { name: 'Slate', hex: '#64748b' },
  { name: 'Pink', hex: '#ec4899' },
  { name: 'Orange', hex: '#f97316' },
];

function Lists({ showToast, handoff, onHandoffDone }) {
  const { t } = useT();

  // Menu-item style shared by the header's overflow menu (mirrors the deck editor).
  const menuItem = { display: 'flex', alignItems: 'center', gap: '0.5rem', width: '100%', padding: '0.5rem 0.65rem', borderRadius: '8px', background: 'none', border: 'none', color: 'var(--text-primary)', fontSize: '0.82rem', cursor: 'pointer', textAlign: 'left' };

  // View state: 'list' (all lists) or 'detail' (one list's cards)
  const [lists, setLists] = useState([]);
  const [loading, setLoading] = useState(true);
  const [activeList, setActiveList] = useState(null);
  const [listDetail, setListDetail] = useState(null);

  // List filters
  const [searchTerm, setSearchTerm] = useState('');


  // Create modal
  const [showCreate, setShowCreate] = useState(false);
  const [newName, setNewName] = useState('');
  const [newDesc, setNewDesc] = useState('');

  const [newAccent, setNewAccent] = useState('#10b981');
  const [newIcon, setNewIcon] = useState('list-checks');
  const [importText, setImportText] = useState('');
  const [creating, setCreating] = useState(false);

  // Edit modal
  const [showEdit, setShowEdit] = useState(false);
  const [editName, setEditName] = useState('');
  const [editDesc, setEditDesc] = useState('');
  const [editAccent, setEditAccent] = useState('#10b981');
  const [editIcon, setEditIcon] = useState('list-checks');

  // Buy modal ("shop cart"): the plain list text is fetched when the modal
  // opens rather than on header click, so no half-fetched tab is ever opened.
  const [showBuy, setShowBuy] = useState(false);
  const [buyText, setBuyText] = useState('');
  const [buyLoading, setBuyLoading] = useState(false);
  // false (fine) | 'empty' (list has no cards) | 'failed' (request broke)
  const [buyError, setBuyError] = useState('');

  // Copy modal: exported text per style, lazy-fetched on first tab activation
  // and cached keyed by style; loadList() drops the cache like buyText's.
  const [showCopy, setShowCopy] = useState(false);
  const [copyStyle, setCopyStyle] = useState('plain');
  const [copyTexts, setCopyTexts] = useState({});
  const [copyLoading, setCopyLoading] = useState(false);
  const [copyError, setCopyError] = useState('');
  const copyInFlight = useRef({});

  // Card search inside detail view
  const [searchQuery, setSearchQuery] = useState('');
  const [searchResults, setSearchResults] = useState([]);
  const [searching, setSearching] = useState(false);

  const searchDebounce = useRef(null);
  const pendingEnterSearch = useRef(null);
  const searchRequestRef = useRef(0);
  const lastSearchedQueryRef = useRef('');

  // Detail view card filter: 'all' | 'missing' | 'owned'
  const [cardFilter, setCardFilter] = useState('all');

  // True while an add/qty write is in flight (prevents clobbering upserts)
  const [savingCard, setSavingCard] = useState(false);

  // True while the cheapest-printings rewrite is in flight
  const [movingCheapest, setMovingCheapest] = useState(false);

  useBackGuard(showCreate, () => setShowCreate(false));
  useBackGuard(showEdit, () => setShowEdit(false));
  useBackGuard(showBuy, () => setShowBuy(false));
  useBackGuard(showCopy, () => setShowCopy(false));
  useBackGuard(!!activeList, () => leaveList());

  // Leaving a list clears BOTH detail states and the remembered open id —
  // missing one of them is how a back gesture used to leave the next refresh
  // restoring a list the user had already walked out of.
  const leaveList = () => {
    setActiveList(null);
    setListDetail(null);
    clearOpen('lists');
  };

  const fetchLists = async () => {
    try {
      setLoading(true);
      const res = await fetch('/api/lists');
      if (res.ok) {
        const rows = await res.json();
        setLists(rows);
        return rows;
      }
    } catch (err) {
      console.error(err);
      showToast(t('lists.errLoad'));
    } finally {
      setLoading(false);
    }
    return [];
  };

  useEffect(() => {
    // Load the overview first, then reopen the remembered list (refresh
    // mid-edit should land back in it). The overview rows are the account's
    // current truth: a stale id from a deleted list or another account quietly
    // stops at the overview instead of a broken detail pane. activeList gets
    // the overview entry merged with the detail response — the detail view
    // reads name/description/accent_color off activeList, which the overview
    // row has but a bare openList call never relied on being absent.
    (async () => {
      try {
        const rows = await fetchLists();
        const remembered = getRememberedView();
        if (remembered.tab !== 'lists' || !remembered.listId) return;
        const entry = rows.find(l => String(l.id) === remembered.listId);
        if (!entry) { clearOpen('lists'); return; }
        await openList(entry);
      } catch { /* restore is best-effort; overview already shows */ }
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const openCreate = () => {
    setNewName('');
    setNewDesc('');
    setNewAccent('#10b981');
    setNewIcon('list-checks');
    setImportText('');
    setShowCreate(true);
  };

  const openEdit = () => {
    setEditName(activeList.name);
    setEditDesc(activeList.description || '');
    setEditAccent(activeList.accent_color || '#10b981');
    setEditIcon(activeList.icon || 'list-checks');
    setShowEdit(true);
  };

  const handleCreate = async (e) => {
    e.preventDefault();
    if (!newName.trim() || creating) return;
    setCreating(true);
    try {
      const res = await fetch('/api/lists', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          name: newName,
          description: newDesc,
          accent_color: newAccent,
          icon: newIcon,
          list_text: importText,
        }),
      });
      if (res.ok) {
        const data = await res.json().catch(() => ({}));
        if (data.unmatched && data.unmatched.length > 0) {
          showToast(t('lists.importedSummary', {
            matched: data.matched,
            unmatched: t('lists.unmatched', {
              count: data.unmatched.length,
              names: data.unmatched.slice(0, 3).join(', ') + (data.unmatched.length > 3 ? '…' : ''),
            }),
          }));
        } else {
          showToast(t('lists.created'));
        }
        setShowCreate(false);
        fetchLists();
      } else {
        const data = await res.json().catch(() => ({}));
        showToast(data.error || t('lists.errCreate'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('lists.errCreate'));
    } finally {
      setCreating(false);
    }
  };

  const handleUpdate = async (e) => {
    e.preventDefault();
    if (!editName.trim()) return;
    try {
      const res = await fetch(`/api/lists/${activeList.id}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: editName, description: editDesc, accent_color: editAccent, icon: editIcon }),
      });
      if (res.ok) {
        showToast(t('lists.listUpdated'));
        setShowEdit(false);
        fetchLists();
        loadList(activeList.id);
      } else {
        showToast(t('lists.errUpdate'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('lists.errUpdate'));
    }
  };

  const handleDelete = async () => {
    if (!window.confirm(t('lists.confirmDelete', { name: activeList.name }))) return;
    try {
      const res = await fetch(`/api/lists/${activeList.id}`, { method: 'DELETE' });
      if (res.ok) {
        showToast(t('lists.deleted'));
        leaveList();
        fetchLists();
      } else {
        showToast(t('lists.errDelete'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('lists.errDelete'));
    }
  };

  // Add every card on the list to the collection at its list quantity (e.g. a
  // trade or purchase that arrived). Uses the collection bulk-add route, <=250 entries per call.
  const registerInCollection = async () => {
    const rows = (listDetail?.cards || []).filter(c => c.quantity > 0);
    const total = rows.reduce((n, c) => n + c.quantity, 0);
    if (!total) { showToast(t('lists.registerNone')); return; }
    if (!window.confirm(t('lists.registerConfirm', { count: total, name: activeList.name }))) return;
    let added = 0, failed = 0;
    try {
      for (let i = 0; i < rows.length; i += 250) {
        const chunk = rows.slice(i, i + 250);
        const res = await fetch('/api/collection/bulk-add', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ card_ids: chunk.map(c => ({ card_id: c.id, quantity: c.quantity })) }) });
        const body = await res.json().catch(() => ({}));
        const ok = new Set((body.entries || []).map(e => e.card_id));
        added += chunk.filter(c => ok.has(c.id)).reduce((n, c) => n + c.quantity, 0);
        failed += Array.isArray(body.failed) ? body.failed.length : (res.ok ? 0 : chunk.length);
      }
      showToast(failed ? t('lists.registerPartial', { count: added, failed }) : t('lists.registerDone', { count: added }));
      await Promise.all([loadList(activeList.id), fetchLists()]);
    } catch (err) {
      console.error(err);
      showToast(t('lists.registerError'));
    }
  };

  const pullFromCollection = async () => {
    const url = `/api/lists/${activeList.id}/remove-from-collection`;
    const post = (dry) => fetch(url, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ dry_run: dry }) });
    try {
      const pre = await post(true);
      if (!pre.ok) { showToast(t('lists.pullError')); return; }
      const plan = await pre.json();
      if (!plan.removed) { showToast(t('lists.pullNone')); return; }
      const shortNote = plan.short.length ? '\n\n' + t('lists.pullShort', { count: plan.requested - plan.removed }) : '';
      if (!window.confirm(t('lists.pullConfirm', { count: plan.removed, name: activeList.name }) + shortNote)) return;
      const res = await post(false);
      if (!res.ok) { showToast(t('lists.pullError')); return; }
      const done = await res.json();
      showToast(t('lists.pullDone', { count: done.removed }));
      await Promise.all([loadList(activeList.id), fetchLists()]);
    } catch (err) {
      console.error(err);
      showToast(t('lists.pullError'));
    }
  };

  const loadList = async (listId) => {
    try {
      const res = await fetch(`/api/lists/${listId}`);
      if (res.ok) {
        setListDetail(await res.json());
        // Which list is open is now this id — reloads after edits re-assert it,
        // which is exactly right: the open list never changed.
        rememberOpen('lists', listId);
        // The buy modal caches the exported text; any detail reload (list
        // switch, qty edits, reshuffles) can change it, so drop the cache and
        // let the next modal open refetch it lazily. Same goes for the copy
        // modal's per-style cache.
        setBuyText('');
        setBuyError(false);
        setCopyTexts({});
      } else {
        showToast(t('lists.errLoad'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('lists.errLoad'));
    }
  };

  const openList = async (list) => {
    setActiveList(list);
    setCardFilter('all');
    setSearchQuery('');
    setSearchResults([]);
    await loadList(list.id);
  };

  // Handoff from the deck builder's "what's missing" panel: prefill the
  // create form with the shortfall instead of firing a blind create; the
  // create modal's own save path stays the single owner of list creation.
  useEffect(() => {
    if (!handoff) return;
    if (handoff.createMissing) {
      setNewName(handoff.createMissing.name || '');
      setNewDesc('');
      setNewAccent('#10b981');
      setImportText(handoff.createMissing.text || '');
      setShowCreate(true);
    }
    onHandoffDone?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [handoff]);

  // --- Card search (debounced as the user types) ---
  const doSearch = async (query) => {
    const normalized = query.trim();
    const request = searchRequestRef.current + 1;
    searchRequestRef.current = request;
    lastSearchedQueryRef.current = normalized;
    let cards = [];
    try {
      setSearching(true);
      const field = looksLikeSyntax(normalized) ? 'q' : 'name';
      const res = await fetch(`/api/search?${field}=${encodeURIComponent(normalized)}&limit=24${field === 'q' ? '&scope=internet' : ''}`);
      if (request !== searchRequestRef.current) return cards;
      if (res.ok) {
        const found = await res.json();
        const byCardName = new Map();
        for (const card of found) {
          const key = cardKey(card);
          if (!byCardName.has(key)) byCardName.set(key, card);
        }
        cards = Array.from(byCardName.values());
        setSearchResults(cards);
      } else if (res.status === 429) {
        showToast(t('lists.errLoad'));
      } else {
        setSearchResults([]);
      }
    } catch (err) {
      console.error(err);
      if (request === searchRequestRef.current) setSearchResults([]);
    } finally {
      if (request === searchRequestRef.current) setSearching(false);
    }
    if (
      request === searchRequestRef.current &&
      pendingEnterSearch.current === normalized
    ) {
      pendingEnterSearch.current = null;
      if (cards.length === 1) await addCard(cards[0]);
    }
    return cards;
  };

  const handleSearchChange = (value) => {
    setSearchQuery(value);
    if (searchDebounce.current) clearTimeout(searchDebounce.current);
    if (!value.trim()) { setSearchResults([]); return; }
    searchDebounce.current = setTimeout(() => doSearch(value), 350);
  };

  const handleSearchKeyDown = async (e) => {
    if (e.key !== 'Enter' || savingCard) return;
    e.preventDefault();
    const query = searchQuery.trim();
    if (!query) return;
    if (searchDebounce.current) {
      clearTimeout(searchDebounce.current);
      searchDebounce.current = null;
    }
    if (
      !searching &&
      lastSearchedQueryRef.current === query &&
      searchResults.length === 1
    ) {
      await addCard(searchResults[0]);
      return;
    }
    if (searching) {
      pendingEnterSearch.current = query;
      return;
    }
    pendingEnterSearch.current = null;
    const found = await doSearch(searchQuery);
    if (found.length === 1) await addCard(found[0]);
  };

  // --- Card quantity management inside the open list ---
  const setQty = async (cardId, qty) => {
    if (savingCard) return;
    if (!Number.isFinite(qty) || qty < 0) return;
    if (qty === 0) { await removeCard(cardId); return; }
    setSavingCard(true);
    try {
      const res = await fetch(`/api/lists/${activeList.id}/cards`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ card_id: cardId, quantity: qty }),
      });
      if (res.ok) await loadList(activeList.id);
      else showToast(t('lists.errCard'));
    } catch (err) {
      console.error(err);
      showToast(t('lists.errCard'));
    } finally {
      setSavingCard(false);
    }
  };

  const addCard = async (card) => {
    const existing = findSameCard(listDetail?.cards, card);
    await setQty(existing?.id || card.id, (existing?.quantity || 0) + 1);
    if (!existing) showToast(t('lists.addedCard', { name: displayName(card) }));
  };

  const removeCard = async (cardId) => {
    if (savingCard) return;
    setSavingCard(true);
    try {
      const res = await fetch(`/api/lists/${activeList.id}/cards/${cardId}`, { method: 'DELETE' });
      if (res.ok) {
        showToast(t('lists.cardRemoved'));
        await loadList(activeList.id);
      }
    } catch (err) {
      console.error(err);
    } finally {
      setSavingCard(false);
    }
  };

  // Rewrites the list: every logical card moves to its cheapest known USD
  // printing, sibling-printing rows fold into it. Deliberately loud and
  // confirm-gated — this is a one-way reshape of the saved list.
  const moveToCheapestPrintings = async () => {
    if (movingCheapest || savingCard) return;
    const message = t('lists.confirmCheapest', { name: activeList.name });
    if (!window.confirm(message)) return;
    setMovingCheapest(true);
    try {
      const res = await fetch(`/api/lists/${activeList.id}/cheapest-printings`, { method: 'PUT' });
      if (res.ok) {
        const body = await res.json();
        showToast(t('lists.cheapestDone', { count: Number(body.moved) || 0 }));
        await Promise.all([loadList(activeList.id), fetchLists()]);
      } else {
        const data = await res.json().catch(() => ({}));
        showToast(data.error || t('lists.cheapestFailed'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('lists.cheapestFailed'));
    } finally {
      setMovingCheapest(false);
    }
  };

  // --- Copy modal: lazy per-style export text (same shapes as the cardlist) ---
  // Each style is fetched on first activation and cached keyed by style
  // (loadList drops the cache when the detail reloads). In-flight refs keep a
  // impatient tab click from firing the same request twice.
  const fetchCopyStyle = async (style) => {
    if (copyTexts[style] || copyInFlight.current[style]) return;
    copyInFlight.current[style] = true;
    setCopyLoading(true);
    setCopyError('');
    try {
      const res = await fetch(`/api/lists/${activeList.id}/cardlist?style=${style}`);
      if (!res.ok) throw new Error('export failed');
      const text = await res.text();
      if (text.trim()) setCopyTexts(prev => ({ ...prev, [style]: text }));
      else setCopyError('empty');
    } catch (err) {
      console.error(err);
      setCopyError('failed');
    } finally {
      copyInFlight.current[style] = false;
      // Spinner stays up while any style is still in flight — otherwise a
      // fast plain response would clear the flag while detailed is pending,
      // leaving the modal with neither text nor loading hint.
      setCopyLoading(Object.values(copyInFlight.current).some(Boolean));
    }
  };

  const openCopy = () => {
    setCopyStyle('plain');
    setCopyError('');
    setShowCopy(true);
    fetchCopyStyle('plain');
  };

  const selectCopyStyle = (style) => {
    setCopyStyle(style);
    setCopyError('');
    fetchCopyStyle(style);
  };

  const copyActiveText = copyTexts[copyStyle] || '';
  const copyLineCount = copyActiveText ? copyActiveText.split('\n').filter(l => l.trim()).length : 0;

  // The modal also shows the text in a selectable textarea, so a denied
  // clipboard still leaves the list copyable by hand.
  const copyActiveList = () => {
    copyToClipboard(copyActiveText);
    showToast(t('lists.exportCopied'));
  };

  // --- Buy modal ("shop cart") ---
  // Clipboard write with the legacy textarea fallback. Non-fatal by design:
  // every flow that uses it also leaves the text visible in the modal, so a
  // denied clipboard degrades the UX without breaking the purchase.
  const copyToClipboard = async (text) => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      try {
        const ta = document.createElement('textarea');
        ta.value = text;
        document.body.appendChild(ta);
        ta.select();
        document.execCommand('copy');
        ta.remove();
      } catch { /* clipboard unavailable; the text stays selectable in the modal */ }
    }
  };

  // Fetch the plain list when the modal opens (not on header click): an
  // in-flight fetch never races a user activation, so no tab is opened before
  // its payload exists. Cached afterwards; switching lists invalidates it.
  const openBuy = async () => {
    setShowBuy(true);
    setBuyError(false);
    if (buyText) return;
    setBuyLoading(true);
    try {
      const res = await fetch(`/api/lists/${activeList.id}/cardlist?style=plain`);
      if (!res.ok) throw new Error('export failed');
      const text = await res.text();
      if (text.trim()) setBuyText(text);
      else setBuyError('empty');
    } catch (err) {
      console.error(err);
      setBuyError('failed');
    } finally {
      setBuyLoading(false);
    }
  };

  const buyLineCount = buyText ? buyText.split('\n').filter(l => l.trim()).length : 0;

  // ManaPool prefill deep link: /add-deck reads the base64 `deck` param and
  // drops it straight into its Mass Entry paste box (verified live). The open
  // must stay SYNCHRONOUS inside the click handler — the user activation does
  // not survive an await — and window.open returns null with these features
  // even when the tab opens, so the return value is deliberately ignored.
  const buyManapool = () => {
    const url = buildManapoolUrl(buyText.split('\n'));
    if (!url) { showToast(t('lists.exportEmpty')); return; }
    window.open(url, '_blank', 'noopener,noreferrer');
    setShowBuy(false);
  };

  // TCGplayer prefilled Mass Entry: the `c=` param is exactly what their own
  // "Create a Shareable Link" button emits — qty + name segments joined by
  // `||` (see utils/tcgMassEntryUrl.js). Unknown names still land as
  // "not found" rows rather than vanishing. The list is also left on the
  // clipboard as a belt-and-braces fallback for the rare overlong link that
  // browsers refuse to navigate; the modal text remains selectable too.
  const buyTcgplayer = () => {
    const url = buildTcgMassEntryUrl(buyText.split('\n'));
    if (!url) { showToast(t('lists.exportEmpty')); return; }
    // Chrome caps navigable URLs at 2MB and TCGplayer's own frontend gives
    // up past ~8KB of query. Past that threshold, degrade to the copy+paste
    // flow deliberately: the plain page opens, the list is on the clipboard,
    // and the toast says so — instead of a link that dies mid-navigation.
    const tooLong = url.length > 8000;
    if (tooLong) copyToClipboard(buyText);
    window.open(tooLong ? 'https://www.tcgplayer.com/massentry?productline=Magic' : url, '_blank', 'noopener,noreferrer');
    setShowBuy(false);
    if (tooLong) showToast(t('lists.buyTcgCopied'));
  };

  const copyBuyList = () => {
    copyToClipboard(buyText);
    showToast(t('lists.exportCopied'));
  };

  // --- Derived data ---
  const filteredLists = lists.filter(l => {
    if (searchTerm && !l.name.toLowerCase().includes(searchTerm.toLowerCase())) return false;
    return true;
  });

  const cards = (listDetail?.cards || []).filter(c => {
    if (cardFilter === 'missing') return (c.owned_qty || 0) < c.quantity;
    if (cardFilter === 'owned') return (c.owned_qty || 0) > 0;
    return true;
  });

  // Escape closes the copy and buy modals (the overlay itself is never
  // focused). The effect sits above the list-view early return so hook order
  // stays stable.
  useEffect(() => {
    if (!showCopy && !showBuy) return;
    const onKey = (e) => { if (e.key === 'Escape') { setShowCopy(false); setShowBuy(false); } };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [showCopy, showBuy]);

  // ============================ LIST VIEW ============================
  if (!activeList) {
    return (
      <div style={{ display: 'flex', flexDirection: 'column', gap: '1.25rem' }}>
        <div className="glass-panel" style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: '1rem', padding: '1.25rem 1.5rem', background: 'linear-gradient(135deg, rgba(16, 185, 129, 0.12), rgba(15, 23, 42, 0.8))', border: '1px solid rgba(16,185,129,0.25)' }}>
          <div>
            <h2 style={{ fontSize: '1.4rem', color: 'var(--text-strong)', fontWeight: 800, display: 'flex', alignItems: 'center', gap: '0.5rem', margin: 0 }}>
              <ListChecks size={22} style={{ color: 'var(--accent-green, #10b981)' }} />
              {t('nav.lists')}
            </h2>
            <p style={{ color: 'var(--text-secondary)', fontSize: '0.85rem', marginTop: '0.25rem' }}>{t('lists.subtitle')}</p>
          </div>
          <button className="btn btn-primary" onClick={openCreate}
            style={{ padding: '0.6rem 1.25rem', fontSize: '0.9rem', fontWeight: 700, display: 'flex', alignItems: 'center', gap: '0.5rem' }}>
            <Plus size={18} /> {t('lists.newList')}
          </button>
        </div>

        <div className="glass-panel" style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: '0.75rem', padding: '1rem 1.25rem' }}>
          <div style={{ position: 'relative', flex: '1 1 240px', minWidth: '220px' }}>
            <Search size={16} style={{ position: 'absolute', left: '0.75rem', top: '50%', transform: 'translateY(-50%)', color: 'var(--text-muted)' }} />
            <input type="text" className="input-control" placeholder={t('deck.filterPlaceholder')}
              value={searchTerm} onChange={e => setSearchTerm(e.target.value)}
              style={{ paddingLeft: '2.25rem', width: '100%', fontSize: '0.85rem' }} />
          </div>
        </div>

        {loading ? (
          <div style={{ display: 'flex', justifyContent: 'center', padding: '3rem' }}><div className="spinner" /></div>
        ) : filteredLists.length === 0 ? (
          <div className="glass-panel" style={{ textAlign: 'center', padding: '3.5rem 1.5rem', color: 'var(--text-secondary)' }}>
            <ListChecks size={40} style={{ opacity: 0.35, marginBottom: '0.75rem' }} />
            <div style={{ fontWeight: 700, color: 'var(--text-strong)' }}>{t('lists.empty')}</div>
            <div style={{ fontSize: '0.85rem', marginTop: '0.25rem' }}>{t('lists.emptyHint')}</div>
          </div>
        ) : (
          <div className="list-tiles" style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(280px, 1fr))', gap: '1rem' }}>
            {filteredLists.map(list => {
              const accent = list.accent_color || '#10b981';
              return (
                <div key={list.id} className="glass-panel list-tile"
                  style={{ '--list-accent': accent,
                    display: 'flex', flexDirection: 'column', gap: '0.75rem', padding: '1.25rem',
                    position: 'relative', overflow: 'hidden', cursor: 'pointer',
                    border: `1px solid ${accent}40`,
                    background: `linear-gradient(145deg, ${accent}12, rgba(15,23,42,0.65))`,
                    transition: 'all 0.2s cubic-bezier(0.16, 1, 0.3, 1)',
                  }}
                  onClick={() => openList(list)}
                  onMouseEnter={e => { e.currentTarget.style.transform = 'translateY(-3px)'; e.currentTarget.style.boxShadow = `0 12px 30px ${accent}25`; }}
                  onMouseLeave={e => { e.currentTarget.style.transform = 'none'; e.currentTarget.style.boxShadow = 'none'; }}
                >
                  <div className="list-tile-accent" style={{ position: 'absolute', top: 0, left: 0, right: 0, height: '3px', background: `linear-gradient(90deg, ${accent}, ${accent}cc)` }}>
                    <ListIcon name={list.icon} size={18} className="list-tile-accent-icon" aria-hidden="true" />
                  </div>
                  <div className="list-tile-head" style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', gap: '0.5rem' }}>
                    <div className="list-tile-title" style={{ minWidth: 0, display: 'flex', gap: '0.6rem', alignItems: 'flex-start' }}>
                      <div style={{ minWidth: 0 }}>
                      <div style={{ fontSize: '1.05rem', fontWeight: 800, color: 'var(--text-strong)', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>{list.name}</div>
                      {list.description && <div style={{ fontSize: '0.78rem', color: 'var(--text-secondary)', marginTop: '0.2rem', display: '-webkit-box', WebkitLineClamp: 2, WebkitBoxOrient: 'vertical', overflow: 'hidden' }}>{list.description}</div>}
                      </div>
                    </div>
                    <button className="btn btn-danger btn-icon-only list-tile-delete" title={t('lists.deleteList')} aria-label={t('lists.deleteList')}
                      onClick={e => { e.stopPropagation(); window.confirm(t('lists.confirmDelete', { name: list.name })) && fetch(`/api/lists/${list.id}`, { method: 'DELETE' }).then(() => { showToast(t('lists.deleted')); fetchLists(); }); }}
                      style={{ width: '1.6rem', height: '1.6rem', padding: 0, display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
                      <Trash2 size={14} />
                    </button>
                  </div>
                  <div className="list-tile-stats" style={{ display: 'flex', gap: '0.75rem', fontSize: '0.78rem', color: 'var(--text-secondary)' }}>
                    <span style={{ display: 'flex', alignItems: 'center', gap: '0.3rem' }}>
                      <Layers size={13} /> {t('lists.cards', { count: list.total_card_types || 0 })}
                    </span>
                    {/* deckMinimumValueText already carries the currency symbol —
                        a DollarSign icon beside it rendered a duplicated "$ $12.35". */}
                    <span className="list-tile-value" title={deckMinimumValueHint(list, t)}>
                      {deckMinimumValueText(list)}
                    </span>
                  </div>
                </div>
              );
            })}
          </div>
        )}

        {/* Create modal */}
        {showCreate && (
          <div className="modal-overlay" style={{ position: 'fixed', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: 'rgba(0,0,0,0.75)', backdropFilter: 'blur(6px)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 999 }}>
            <div className="glass-panel" style={{ maxWidth: '460px', width: '100%', maxHeight: '90vh', overflowY: 'auto', padding: '1.75rem', position: 'relative', border: '1px solid rgba(255,255,255,0.15)' }}>
              <button className="btn btn-secondary btn-icon-only" onClick={() => setShowCreate(false)}
                style={{ position: 'absolute', top: '0.75rem', right: '0.75rem' }}><X size={16} /></button>
              <h3 style={{ fontSize: '1.1rem', fontWeight: 800, color: 'var(--text-strong)', margin: '0 0 1rem' }}>{t('lists.newList')}</h3>
              <form onSubmit={handleCreate} style={{ display: 'flex', flexDirection: 'column', gap: '0.9rem' }}>
                <div className="form-group">
                  <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', fontWeight: 600 }}>{t('lists.name')}</label>
                  <input className="input-control" value={newName} onChange={e => setNewName(e.target.value)}
                    placeholder={t('lists.namePlaceholder')} autoFocus required style={{ width: '100%' }} />
                </div>
                <div className="form-group">
                  <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', fontWeight: 600 }}>{t('lists.descPlaceholder')}</label>
                  <input className="input-control" value={newDesc} onChange={e => setNewDesc(e.target.value)}
                    placeholder={t('lists.descPlaceholder')} style={{ width: '100%' }} />
                </div>
                <div style={{ display: 'flex', gap: '0.75rem' }}>
                  <div className="form-group" style={{ flex: 1 }}>
                    <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', fontWeight: 600 }}>{t('deck.accentColor')}</label>
                    <div style={{ display: 'flex', gap: '0.4rem', flexWrap: 'wrap', paddingTop: '0.2rem' }}>
                      {ACCENTS.map(a => (
                        <button key={a.hex} type="button" title={a.name}
                          onClick={() => setNewAccent(a.hex)}
                          style={{ width: '22px', height: '22px', borderRadius: '50%', background: a.hex, border: newAccent === a.hex ? '2px solid #fff' : '2px solid transparent', cursor: 'pointer' }} />
                      ))}
                    </div>
                  </div>
                </div>
                <IconPicker value={newIcon} onChange={setNewIcon} accent={newAccent} t={t} />
                <div className="form-group">
                  <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', fontWeight: 600 }}>{t('lists.importOptional')}</label>
                  <textarea className="input-control" value={importText} onChange={e => setImportText(e.target.value)}
                    placeholder={t('lists.importPlaceholder')} rows={5}
                    style={{ width: '100%', fontFamily: 'monospace', fontSize: '0.8rem', resize: 'vertical' }} />
                  <div style={{ fontSize: '0.72rem', color: 'var(--text-muted)', marginTop: '0.3rem' }}>{t('lists.importHint')}</div>
                </div>
                <button className="btn btn-primary" type="submit" disabled={creating || !newName.trim()}
                  style={{ fontWeight: 700, opacity: creating ? 0.6 : 1 }}>
                  {t('lists.create')}
                </button>
              </form>
            </div>
          </div>
        )}
      </div>
    );
  }

  // ============================ DETAIL VIEW ============================
  const accent = activeList.accent_color || '#10b981';
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: '1.25rem' }}>
      <div className="glass-panel" style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: '1rem', padding: '1.25rem 1.5rem', border: `1px solid ${accent}40`, background: `linear-gradient(135deg, ${accent}14, rgba(15,23,42,0.8))` }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: '0.9rem', minWidth: 0 }}>
          <button className="btn btn-secondary btn-icon-only" onClick={leaveList} title={t('nav.dashboard')}><ChevronLeft size={16} /></button>
          <div style={{ minWidth: 0 }}>
            <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', minWidth: 0 }}>
              <ListIcon name={activeList.icon} size={20} className="list-detail-icon" style={{ color: accent, flexShrink: 0 }} />
              <h2 style={{ fontSize: '1.3rem', fontWeight: 800, color: 'var(--text-strong)', margin: 0, whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>{activeList.name}</h2>
              <button className="icon-btn-ghost" onClick={openEdit} title={t('lists.editList')} aria-label={t('lists.editList')}><Pencil size={15} /></button>
            </div>
            {activeList.description && <div style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>{activeList.description}</div>}
          </div>
        </div>
        <div className="list-editor-header-actions" style={{ display: 'flex', gap: '0.5rem', flexWrap: 'wrap', alignItems: 'center' }}>
          <button className="btn btn-secondary btn-icon-only" onClick={openCopy}
            title={t('lists.copyList')} aria-label={t('lists.copyList')}
            style={{ display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
            <Copy size={16} />
          </button>
<button className="btn btn-primary btn-icon-only" onClick={openBuy}
            title={t('lists.buyList')} aria-label={t('lists.buyList')}
            style={{ display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
            <ShoppingCart size={16} />
          </button>
          <button className="btn btn-secondary" onClick={moveToCheapestPrintings}
            disabled={movingCheapest || savingCard} title={t('lists.cheapestTitle')}
            style={{ fontSize: '0.8rem', display: 'flex', alignItems: 'center', gap: '0.4rem', opacity: (movingCheapest || savingCard) ? 0.6 : 1 }}>
            <Wand2 size={14} /> {movingCheapest ? t('lists.cheapestWorking') : t('lists.cheapestButton')}
          </button>
          <OverflowMenu label={t('lists.moreActions')}>
            <button role="menuitem" style={menuItem} onClick={registerInCollection}>
              <PackagePlus size={14} /> {t('lists.registerButton')}
            </button>
            <button role="menuitem" style={{ ...menuItem, color: 'var(--accent-red)' }} onClick={pullFromCollection}>
              <PackageMinus size={14} /> {t('lists.pullButton')}
            </button>
            <button role="menuitem" style={{ ...menuItem, color: 'var(--accent-red)' }} onClick={handleDelete}>
              <Trash2 size={14} /> {t('lists.deleteList')}
            </button>
          </OverflowMenu>
        </div>
      </div>

      {/* Value: the list's own printings vs the cheapest-printings floor,
          same two axes (and "+" honesty rule) as the deck detail. */}
      {listDetail && (() => {
        const currentUnpriced = Number(listDetail.current_unpriced_cards) || 0;
        const currentText = `${priceText(Number(listDetail.current_printing_value) || 0, listDetail.minimum_value_currency || 'USD')}${currentUnpriced > 0 ? '+' : ''}`;
        const currentHint = currentUnpriced === 1
          ? t('lists.valueCurrentIncompleteOne')
          : currentUnpriced > 0
            ? t('lists.valueCurrentIncomplete', { count: currentUnpriced })
            : t('lists.valueCurrentComplete');
        // A sliver, not a card: one thin inline strip, both totals side by side.
        const item = { display: 'inline-flex', alignItems: 'baseline', gap: '0.35rem', whiteSpace: 'nowrap' };
        return (
          <div className="glass-panel list-value-panel" role="group" aria-label={t('deck.valueTitle')}
            style={{ display: 'flex', alignItems: 'center', flexWrap: 'wrap', gap: '0.35rem 1rem', padding: '0.35rem 0.9rem', fontSize: '0.75rem', color: 'var(--text-secondary)' }}>
            <DollarSign size={12} style={{ color: 'var(--accent-yellow)', flexShrink: 0 }} aria-hidden="true" />
            <span style={item} title={currentHint} tabIndex={0} aria-label={`${t('deck.valueCurrent')}: ${currentText}. ${currentHint}`}>
              {t('deck.valueCurrent')}
              <strong className="list-value-current" style={{ color: 'var(--text-strong)' }}>{currentText}</strong>
              {currentUnpriced > 0 && <small style={{ color: 'var(--text-muted)' }}>({t('deck.unpricedCount', { count: currentUnpriced })})</small>}
            </span>
            <span style={item} title={deckMinimumValueHint(listDetail, t)} tabIndex={0}
              aria-label={`${t('deck.valueCheapest')}: ${deckMinimumValueText(listDetail)}. ${deckMinimumValueHint(listDetail, t)}`}>
              {t('deck.valueCheapest')}
              <strong className="list-value-cheapest" style={{ color: 'var(--accent-yellow)' }}>{deckMinimumValueText(listDetail)}</strong>
            </span>
          </div>
        );
      })()}

      {/* Card search to add */}
      <div className="glass-panel" style={{ display: 'flex', flexDirection: 'column', gap: '0.75rem', padding: '1rem 1.25rem' }}>
        <div style={{ display: 'flex', gap: '0.75rem', flexWrap: 'wrap', alignItems: 'center' }}>
          <div style={{ position: 'relative', flex: '1 1 240px', minWidth: '220px' }}>
            <Search size={16} style={{ position: 'absolute', left: '0.75rem', top: '50%', transform: 'translateY(-50%)', color: 'var(--text-muted)' }} />
            <input type="text" className="input-control" placeholder={t('lists.searchPlaceholder')}
              value={searchQuery} onChange={e => handleSearchChange(e.target.value)} onKeyDown={handleSearchKeyDown}
              style={{ paddingLeft: '2.25rem', width: '100%', fontSize: '0.85rem' }} />
            {searching && <Search size={14} style={{ position: 'absolute', right: '0.75rem', top: '50%', transform: 'translateY(-50%)', color: 'var(--text-muted)', animation: 'spin 1s linear infinite' }} />}
          </div>
        </div>
        {searchResults.length > 0 && (
          <div style={{ display: 'flex', flexDirection: 'column', gap: '0.4rem', maxHeight: '320px', overflowY: 'auto' }}>
            {searchResults.map(card => (
              <div key={card.id} style={{ display: 'flex', alignItems: 'center', gap: '0.75rem', padding: '0.4rem 0.6rem', borderRadius: 'var(--radius-sm)', background: 'rgba(255,255,255,0.04)' }}>
                <CardImage card={card} alt={displayName(card)}
                  style={{ width: '34px', height: '48px', borderRadius: '4px', objectFit: 'cover', flexShrink: 0 }} />
                <div style={{ minWidth: 0, flex: 1 }}>
                  <div style={{ fontSize: '0.85rem', fontWeight: 700, color: 'var(--text-strong)', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>{displayName(card)}</div>
                  <div style={{ fontSize: '0.7rem', color: 'var(--text-muted)' }}>{setReference(card) || ''}</div>
                </div>
                <button className="btn btn-primary" onClick={() => addCard(card)}
                  style={{ fontSize: '0.75rem', padding: '0.3rem 0.7rem', display: 'flex', alignItems: 'center', gap: '0.3rem', whiteSpace: 'nowrap' }}>
                  <Plus size={13} /> {t('lists.addCard')}
                </button>
              </div>
            ))}
          </div>
        )}
        {searchQuery.trim() && !searching && searchResults.length === 0 && (
          <div style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', textAlign: 'center', padding: '0.5rem' }}>{t('lists.noSearchResults')}</div>
        )}
      </div>

      {/* Filter chips */}
      <div className="glass-panel" style={{ display: 'flex', gap: '0.5rem', padding: '0.6rem 0.9rem', flexWrap: 'wrap' }}>
        {[['all', t('lists.allCards')], ['missing', t('lists.missingOnly')], ['owned', t('lists.ownedOnly')]].map(([val, label]) => (
          <button key={val} type="button"
            className={`sub-nav-tab ${cardFilter === val ? 'active' : ''}`}
            onClick={() => setCardFilter(val)}
            style={{ padding: '0.35rem 0.9rem', fontSize: '0.78rem', fontWeight: 600, borderRadius: 'var(--radius-sm)' }}>
            {label}
          </button>
        ))}
      </div>

      {/* Cards table */}
      {!listDetail ? (
        <div style={{ display: 'flex', justifyContent: 'center', padding: '3rem' }}><div className="spinner" /></div>
      ) : cards.length === 0 ? (
        <div className="glass-panel" style={{ textAlign: 'center', padding: '3rem 1.5rem', color: 'var(--text-secondary)' }}>
          <ListChecks size={40} style={{ opacity: 0.35, marginBottom: '0.75rem' }} />
          <div style={{ fontWeight: 700, color: 'var(--text-strong)' }}>{t('lists.emptyList')}</div>
          <div style={{ fontSize: '0.85rem', marginTop: '0.25rem' }}>{t('lists.emptyListHint')}</div>
        </div>
      ) : (
        <div className="glass-panel list-cards-panel" style={{ overflowX: 'auto', padding: 0 }}>
          <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: '0.85rem' }}>
            <thead>
              <tr style={{ borderBottom: '1px solid rgba(255,255,255,0.1)' }}>
                <th style={{ textAlign: 'left', padding: '0.7rem 1rem', color: 'var(--text-muted)', fontSize: '0.72rem', textTransform: 'uppercase', letterSpacing: '0.05em' }}>{t('lists.cardCol')}</th>
                <th style={{ textAlign: 'left', padding: '0.7rem 0.5rem', color: 'var(--text-muted)', fontSize: '0.72rem', textTransform: 'uppercase', letterSpacing: '0.05em' }}>{t('lists.qty')}</th>
                <th style={{ textAlign: 'left', padding: '0.7rem 0.5rem', color: 'var(--text-muted)', fontSize: '0.72rem', textTransform: 'uppercase', letterSpacing: '0.05em' }}>{t('lists.owned')}</th>
                <th style={{ padding: '0.7rem 1rem' }} />
              </tr>
            </thead>
            <tbody>
              {cards.map(card => {
                const missing = Math.max(0, card.quantity - (card.owned_qty || 0));
                return (
                  <tr key={card.id} style={{ borderBottom: '1px solid rgba(255,255,255,0.05)' }}>
                    <td style={{ padding: '0.5rem 1rem' }}>
                      <div style={{ display: 'flex', alignItems: 'center', gap: '0.75rem' }}>
                        <CardImage card={card} alt={displayName(card)}
                          style={{ width: '38px', height: '54px', borderRadius: '5px', objectFit: 'cover', flexShrink: 0 }} />
                        <div style={{ minWidth: 0 }}>
                          <div style={{ fontWeight: 700, color: 'var(--text-strong)' }}>{displayName(card)}</div>
                          <div style={{ fontSize: '0.72rem', color: 'var(--text-muted)' }}>
                            {setReference(card) || ''}
                            {card.price_trend > 0 && <span style={{ marginLeft: '0.5rem' }}>${card.price_trend.toFixed(2)}</span>}
                          </div>
                        </div>
                      </div>
                    </td>
                    <td style={{ padding: '0.5rem 0.5rem' }}>
                      <div style={{ display: 'inline-flex', alignItems: 'center', gap: '0.3rem' }}>
                        <button className="btn btn-secondary btn-icon-only" onClick={() => setQty(card.id, card.quantity - 1)}
                          style={{ width: '22px', height: '22px', padding: 0, display: 'flex', alignItems: 'center', justifyContent: 'center' }}><Minus size={13} /></button>
                        <input type="number" min="0" value={card.quantity}
                          onChange={e => setQty(card.id, parseInt(e.target.value, 10) || 0)}
                          style={{ width: '52px', textAlign: 'center', fontSize: '0.85rem', fontWeight: 700, padding: '0.2rem', borderRadius: 'var(--radius-sm)', background: 'rgba(0,0,0,0.3)', color: 'var(--text-strong)', border: '1px solid rgba(255,255,255,0.15)' }} />
                        <button className="btn btn-secondary btn-icon-only" onClick={() => setQty(card.id, card.quantity + 1)}
                          style={{ width: '22px', height: '22px', padding: 0, display: 'flex', alignItems: 'center', justifyContent: 'center' }}><Plus size={13} /></button>
                      </div>
                    </td>
                    <td style={{ padding: '0.5rem 0.5rem' }}>
                      {missing > 0 ? (
                        <span style={{ color: '#f87171', fontWeight: 700, fontSize: '0.8rem' }}>{t('lists.stillMissing', { count: missing })}</span>
                      ) : (card.owned_qty > 0 ? (
                        <span style={{ color: '#34d399', fontWeight: 700, fontSize: '0.8rem' }}>{t('lists.fullyOwned')}</span>
                      ) : (
                        <span style={{ color: 'var(--text-muted)', fontSize: '0.8rem' }}>0</span>
                      ))}
                    </td>
                    <td style={{ padding: '0.5rem 1rem', textAlign: 'right' }}>
                      <button className="btn btn-secondary btn-icon-only" onClick={() => removeCard(card.id)} title={t('lists.removeCard')}
                        style={{ width: '26px', height: '26px', padding: 0, display: 'flex', alignItems: 'center', justifyContent: 'center' }}><Trash2 size={14} /></button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {/* Edit modal */}
      {showEdit && (
        <div className="modal-overlay" style={{ position: 'fixed', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: 'rgba(0,0,0,0.75)', backdropFilter: 'blur(6px)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 999 }}>
          <div className="glass-panel" style={{ maxWidth: '420px', width: '100%', padding: '1.75rem', position: 'relative', border: '1px solid rgba(255,255,255,0.15)' }}>
            <button className="btn btn-secondary btn-icon-only" onClick={() => setShowEdit(false)}
              style={{ position: 'absolute', top: '0.75rem', right: '0.75rem' }}><X size={16} /></button>
            <h3 style={{ fontSize: '1.1rem', fontWeight: 800, color: 'var(--text-strong)', margin: '0 0 1rem' }}>{t('lists.editList')}</h3>
            <form onSubmit={handleUpdate} style={{ display: 'flex', flexDirection: 'column', gap: '0.9rem' }}>
              <div className="form-group">
                <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', fontWeight: 600 }}>{t('lists.name')}</label>
                <input className="input-control" value={editName} onChange={e => setEditName(e.target.value)} required style={{ width: '100%' }} />
              </div>
              <div className="form-group">
                <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', fontWeight: 600 }}>{t('lists.descPlaceholder')}</label>
                <input className="input-control" value={editDesc} onChange={e => setEditDesc(e.target.value)} style={{ width: '100%' }} />
              </div>
              <div className="form-group">
                <label style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', fontWeight: 600 }}>{t('deck.accentColor')}</label>
                <div style={{ display: 'flex', gap: '0.4rem', flexWrap: 'wrap', paddingTop: '0.2rem' }}>
                  {ACCENTS.map(a => (
                    <button key={a.hex} type="button" title={a.name}
                      onClick={() => setEditAccent(a.hex)}
                      style={{ width: '22px', height: '22px', borderRadius: '50%', background: a.hex, border: editAccent === a.hex ? '2px solid #fff' : '2px solid transparent', cursor: 'pointer' }} />
                  ))}
                </div>
              </div>
              <IconPicker value={editIcon} onChange={setEditIcon} accent={editAccent} t={t} />
              <div style={{ display: 'flex', gap: '0.5rem', justifyContent: 'flex-end' }}>
                <button type="button" className="btn btn-secondary" onClick={() => setShowEdit(false)}>{t('lists.cancel')}</button>
                <button type="submit" className="btn btn-primary" style={{ fontWeight: 700 }}>{t('lists.save')}</button>
              </div>
            </form>
          </div>
        </div>
      )}

      {/* Buy modal — pick where to buy the list */}
      {showBuy && (
        <div className="modal-overlay" onClick={e => { if (e.target === e.currentTarget) setShowBuy(false); }}
          style={{ position: 'fixed', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: 'rgba(0,0,0,0.75)', backdropFilter: 'blur(6px)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 999 }}>
          <div className="glass-panel" style={{ maxWidth: '520px', width: '100%', maxHeight: '90vh', overflowY: 'auto', padding: '1.75rem', position: 'relative', border: '1px solid rgba(255,255,255,0.15)' }}>
            <button className="btn btn-secondary btn-icon-only" onClick={() => setShowBuy(false)}
              aria-label={t('common.close')} title={t('common.close')}
              style={{ position: 'absolute', top: '0.75rem', right: '0.75rem', zIndex: 1 }}><X size={16} /></button>
            <h3 style={{ fontSize: '1.1rem', fontWeight: 800, color: 'var(--text-strong)', margin: '0 0 0.35rem', paddingRight: '2.25rem' }}>{t('lists.buyList')}</h3>
            <div style={{ fontSize: '0.85rem', color: 'var(--text-secondary)', marginBottom: '1rem', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>{activeList.name}</div>

            {/* Provider choices — flat bordered cards, no glass, wrap on narrow screens */}
            <div style={{ display: 'flex', gap: '0.75rem', flexWrap: 'wrap' }}>
              {[
                { label: t('lists.buyOnManapool'), hint: buyText ? t('lists.buyManaPoolHint', { count: buyLineCount }) : '', onClick: buyManapool, accent: accent },
                { label: t('lists.buyTcg'), hint: buyText ? t('lists.buyTcgHint', { count: buyLineCount }) : '', onClick: buyTcgplayer, accent: '#ff2b03' },
              ].map(card => (
                <button key={card.label} type="button" onClick={card.onClick}
                  disabled={buyLoading || buyError || !buyText}
                  style={{ flex: '1 1 200px', minWidth: '200px', display: 'flex', flexDirection: 'column', gap: '0.35rem', padding: '0.9rem 1rem', borderRadius: 'var(--radius-sm)', textAlign: 'left', cursor: (buyLoading || buyError || !buyText) ? 'not-allowed' : 'pointer', opacity: (buyLoading || buyError || !buyText) ? 0.5 : 1, background: 'rgba(0,0,0,0.3)', border: `1.5px solid ${card.accent}66`, color: 'var(--text-primary)', fontSize: '0.8rem' }}>
                  <span style={{ fontWeight: 800, color: 'var(--text-strong)', fontSize: '0.9rem' }}>{card.label}</span>
                  <span style={{ color: 'var(--text-secondary)', lineHeight: 1.4 }}>{card.hint}</span>
                </button>
              ))}
            </div>

            {buyLoading && (
              <div style={{ fontSize: '0.8rem', color: 'var(--text-muted)', marginTop: '0.9rem' }}>{t('lists.buyLoading')}</div>
            )}
            {buyError && (
              <div style={{ fontSize: '0.8rem', color: 'var(--accent-red)', marginTop: '0.9rem' }}>
                {buyError === 'empty' ? t('lists.exportEmpty') : t('lists.errExport')}
              </div>
            )}

            {/* The plain text itself: always selectable, so the flow survives a
                broken clipboard (and lets a detail edit ship its own way). */}
            {buyText && !buyLoading && (
              <div style={{ marginTop: '0.9rem', display: 'flex', flexDirection: 'column', gap: '0.5rem' }}>
                <textarea readOnly value={buyText} rows={6} aria-label={t('lists.copyList')}
                  onFocus={e => e.target.select()}
                  style={{ width: '100%', fontFamily: 'monospace', fontSize: '0.75rem', padding: '0.6rem', borderRadius: 'var(--radius-sm)', background: 'rgba(0,0,0,0.3)', color: 'var(--text-secondary)', border: '1px solid rgba(255,255,255,0.15)', resize: 'vertical', boxSizing: 'border-box' }} />
                <div style={{ display: 'flex', alignItems: 'center', gap: '0.75rem', flexWrap: 'wrap' }}>
                  <button type="button" className="btn btn-secondary" onClick={copyBuyList}
                    style={{ fontSize: '0.8rem', display: 'flex', alignItems: 'center', gap: '0.4rem' }}>
                    <Copy size={14} /> {t('lists.copyList')}
                  </button>
                  <span style={{ fontSize: '0.75rem', color: 'var(--text-muted)' }}>{t('lists.buyLineCount', { count: buyLineCount })}</span>
                </div>
              </div>
            )}
          </div>
        </div>
      )}

      {/* Copy modal — one entry point, two export shapes as tabs */}
      {showCopy && (
        <div className="modal-overlay" onClick={e => { if (e.target === e.currentTarget) setShowCopy(false); }}
          style={{ position: 'fixed', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: 'rgba(0,0,0,0.75)', backdropFilter: 'blur(6px)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 999 }}>
          <div className="glass-panel" style={{ maxWidth: '520px', width: '100%', maxHeight: '90vh', overflowY: 'auto', padding: '1.75rem', position: 'relative', border: '1px solid rgba(255,255,255,0.15)' }}>
            <button className="btn btn-secondary btn-icon-only" onClick={() => setShowCopy(false)}
              style={{ position: 'absolute', top: '0.75rem', right: '0.75rem' }}><X size={16} /></button>
            <h3 style={{ fontSize: '1.1rem', fontWeight: 800, color: 'var(--text-strong)', margin: '0 0 0.35rem' }}>{t('lists.copyListModal')}</h3>
            <div style={{ fontSize: '0.85rem', color: 'var(--text-secondary)', marginBottom: '1rem', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>{activeList.name}</div>

            {/* Format tabs — same flat bordered cards as the buy modal's providers.
                 The hint line mirrors the provider cards: a line count once
                 that style's text is cached, blank until then. */}
            <div style={{ display: 'flex', gap: '0.75rem', flexWrap: 'wrap' }}>
              {[
                { style: 'plain', label: t('lists.copyStylePlain'), accent: accent },
                { style: 'detailed', label: t('lists.copyStyleDetailed'), accent: accent },
              ].map(tab => (
                <button key={tab.style} type="button" onClick={() => selectCopyStyle(tab.style)}
                  aria-pressed={copyStyle === tab.style}
                  style={{ flex: '1 1 200px', minWidth: '200px', display: 'flex', flexDirection: 'column', gap: '0.35rem', padding: '0.9rem 1rem', borderRadius: 'var(--radius-sm)', textAlign: 'left', cursor: 'pointer', background: copyStyle === tab.style ? `${tab.accent}1a` : 'rgba(0,0,0,0.3)', border: `1.5px solid ${copyStyle === tab.style ? `${tab.accent}66` : 'rgba(255,255,255,0.15)'}`, color: 'var(--text-primary)', fontSize: '0.8rem' }}>
                  <span style={{ fontWeight: 800, color: 'var(--text-strong)', fontSize: '0.9rem' }}>{tab.label}</span>
                  <span style={{ color: 'var(--text-secondary)', lineHeight: 1.4 }}>{copyTexts[tab.style] ? t('lists.buyLineCount', { count: copyTexts[tab.style].split('\n').filter(l => l.trim()).length }) : ''}</span>
                </button>
              ))}
            </div>

            {copyLoading && (
              <div style={{ fontSize: '0.8rem', color: 'var(--text-muted)', marginTop: '0.9rem' }}>{t('lists.buyLoading')}</div>
            )}
            {copyError && !copyLoading && (
              <div style={{ fontSize: '0.8rem', color: 'var(--accent-red)', marginTop: '0.9rem' }}>
                {copyError === 'empty' ? t('lists.exportEmpty') : t('lists.errExport')}
              </div>
            )}

            {/* The text of the active tab: always selectable, so the flow
                survives a broken clipboard. */}
            {copyActiveText && !copyLoading && (
              <div style={{ marginTop: '0.9rem', display: 'flex', flexDirection: 'column', gap: '0.5rem' }}>
                <textarea readOnly value={copyActiveText} rows={6} aria-label={t('lists.copyList')}
                  onFocus={e => e.target.select()}
                  style={{ width: '100%', fontFamily: 'monospace', fontSize: '0.75rem', padding: '0.6rem', borderRadius: 'var(--radius-sm)', background: 'rgba(0,0,0,0.3)', color: 'var(--text-secondary)', border: '1px solid rgba(255,255,255,0.15)', resize: 'vertical', boxSizing: 'border-box' }} />
                <div style={{ display: 'flex', alignItems: 'center', gap: '0.75rem', flexWrap: 'wrap' }}>
                  <button type="button" className="btn btn-primary" onClick={copyActiveList}
                    style={{ fontSize: '0.8rem', display: 'flex', alignItems: 'center', gap: '0.4rem' }}>
                    <Copy size={14} /> {t('lists.copyList')}
                  </button>
                  <span style={{ fontSize: '0.75rem', color: 'var(--text-muted)' }}>{t('lists.buyLineCount', { count: copyLineCount })}</span>
                </div>
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

export default Lists;

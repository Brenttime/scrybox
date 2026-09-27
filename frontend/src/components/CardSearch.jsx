import { useState, useEffect, useMemo, useRef } from 'react';
import { Search, Plus, X, ShieldAlert, Check, MousePointerClick, Zap, Undo2, Maximize2, Braces, SlidersHorizontal } from 'lucide-react';
import confetti from 'canvas-confetti';
import { priceText } from '../utils/formatPrice';
import { resolveCardPrice } from '../utils/resolveCardPrice';
import { isPremiumRarity } from '../utils/cardRarity';
import { getPrintingLabel } from '../utils/cardPrinting';
import CardEntryFields from './CardEntryFields';
import CardImageZoom from './CardImageZoom';
import { useMultiSelect } from '../utils/useMultiSelect';
import { CONDITIONS, PRINTING_OPTIONS } from '../utils/cardOptions';
import { LANGUAGES, langName, isEnglish, displayName, translatedName, setReference, setCode } from '../utils/languages';
import CardImage from './CardImage';
import LoadMore from './LoadMore';
import MultiSelectDropdown from './MultiSelectDropdown';
import ColorPicker from './ColorPicker';
import { CARD_TYPES, SUPERTYPES, MANA_VALUES, RARITY_ORDER, mvBucket, cardTypes, cardSupertypes, cardSubtypes, uniqueSorted, sortRarities, matchMtgFilters } from '../utils/mtgFilters';
import { useT } from '../utils/i18n';
import { adjustOwnedQuantityByName, cardKey } from '../utils/cardIdentity';
import { looksLikeSyntax } from '../utils/scryfallSyntax';

// Search failures worth explaining in-page rather than only as a toast. `keyHint`
// marks the ones a user API key actually fixes; an upstream 5xx does not. Title
// and body are looked up as searchErr.<code>.title / .body.
//
// The box speaks two languages — a plain card name, or Scryfall syntax — and
// the detection is the very rule the collection's filter bar uses, imported
// from utils/scryfallSyntax.js so the two surfaces can never disagree about
// what counts as a query. A string that looks like syntax but does not parse
// comes back from the server as INVALID_QUERY, which the banner names.
function CardSearch({ onAddSuccess, showToast }) {
  const { t } = useT();
  const [numberQuery, setNumberQuery] = useState('');
  const [setCodeQuery, setSetCodeQuery] = useState('');
  // Which language's printings to search. Magic comes from Scryfall in every
  // language.
  const [searchLang, setSearchLang] = useState('en');
  // Unified search box (Moxfield-style): plain names and Scryfall syntax live
  // in the SAME field. `searchText` is what is in the box; whether it is a
  // name or a query is decided on submit (looksLikeSyntax) — no toggle to
  // remember, no mode to get wrong.
  const [searchText, setSearchText] = useState('');
  const [cards, setCards] = useState([]);
  const [loading, setLoading] = useState(false);
  const [searching, setSearching] = useState(false);
  const [searchError, setSearchError] = useState(null);

  // Paging. A full page back means there is probably another one; `total` is the
  // provider's real match count when it reports one (cache hits don't).
  const [pageSize, setPageSize] = useState(() => parseInt(localStorage.getItem('search_page_size'), 10) || 60);
  const [page, setPage] = useState(1);
  const [hasMore, setHasMore] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [total, setTotal] = useState(null);
  // Where the answer came from ('cache' = instant local hit, 'scryfall' = live
  // API) — read off the X-Source header so the results header can say so.
  const [source, setSource] = useState(null);

  // Multi-select for bulk add — the same hook, gesture and visuals the
  // collection uses, so selecting works identically on both screens. Only the
  // action differs: bulk ADD here, bulk edit there (so runBulk goes unused).
  const {
    selectMode, setSelectMode, selectedIds, setSelectedIds, selectAt,
    clearSelection, exitSelectMode, pressHandlers, longPressFired,
  } = useMultiSelect({ showToast });
  const [bulkAdding, setBulkAdding] = useState(false);

  // Set-code autocomplete, sourced from the sets already cached in the DB.
  const [knownSets, setKnownSets] = useState([]);

  // Rapid add: set code stays pinned, type a collector number, press Enter, the
  // card goes straight in. `rapidLog` is the running receipt with undo.
  const [rapidMode, setRapidMode] = useState(false);
  const [rapidNumber, setRapidNumber] = useState('');
  const [rapidBusy, setRapidBusy] = useState(false);
  const [rapidLog, setRapidLog] = useState([]);
  const rapidInputRef = useRef(null);

  // Filter states
  const [filterRarity, setFilterRarity] = useState([]);
  const [filterType, setFilterType] = useState([]);
  const [filterSubtype, setFilterSubtype] = useState([]);
  const [filterSupertype, setFilterSupertype] = useState([]);
  const [filterColors, setFilterColors] = useState([]);
  const [colorMode, setColorMode] = useState('any');
  const [filterIdentity, setFilterIdentity] = useState([]);
  const [filterMv, setFilterMv] = useState([]);
  const [showFilters, setShowFilters] = useState(false);
  const [sortBy, setSortBy] = useState('relevance');

  // Drawer states
  const [selectedCard, setSelectedCard] = useState(null);
  const [isDrawerOpen, setIsDrawerOpen] = useState(false);
  const [isFullScreen, setIsFullScreen] = useState(false);
  
  // Form states
  const [quantity, setQuantity] = useState(1);
  const [condition, setCondition] = useState('Near Mint');
  const [printing, setPrinting] = useState('Normal');
  const [language, setLanguage] = useState('English');
  const [purchasePrice, setPurchasePrice] = useState(0);

  // Set codes for the autocomplete, sourced from the sets already cached in the
  // DB. MTG ids are stored prefixed ("mtg-ltr"); the search wants the bare code.
  useEffect(() => {
    let cancelled = false;
    fetch(`/api/sets?lang=${encodeURIComponent(searchLang)}`)
      .then(r => (r.ok ? r.json() : []))
      .then(rows => {
        if (cancelled) return;
        const seen = new Set();
        setKnownSets(rows
          .map(s => ({ code: String(s.id || '').replace(/^mtg-/, ''), name: s.name }))
          .filter(s => s.code && !seen.has(s.code) && seen.add(s.code))
          .reverse()); // newest first — that is what people are adding
      })
      .catch(() => {});
    return () => { cancelled = true; };
  }, [searchLang]);

  // pageNum > 1 appends to the existing results instead of replacing them.
  // The unified box decides its own routing: anything that looks like Scryfall
  // syntax goes as the raw `q` parameter (name/number/set stay out of it so
  // nothing can muddy the operator string); everything else is a plain name.
  const runSearch = async (pageNum, size = pageSize) => {
    const text = searchText.trim();
    // A search is a raw query, a card name, or the number/set fields —
    // number + set alone is a legitimate fast path (rapid add without the
    // rapid panel).
    if (!text && !numberQuery && !setCodeQuery) return;
    const isSyntax = looksLikeSyntax(text);
    const append = pageNum > 1;
    if (append) setLoadingMore(true); else setLoading(true);
    setSearchError(null);
    if (!append) {
      setSearching(true);
      clearFilters();
      setSortBy('relevance');
      clearSelection();
      setTotal(null);
      setSource(null);
    }
    try {
      const params = new URLSearchParams();
      if (isSyntax) {
        params.append('q', text);
      } else {
        if (text) params.append('name', text);
        if (numberQuery) params.append('number', numberQuery);
        if (setCodeQuery) params.append('set', setCodeQuery);
      }
      params.append('scope', 'internet');
      params.append('lang', searchLang);
      params.append('page', pageNum);
      params.append('limit', size);

      const response = await fetch(`/api/search?${params.toString()}`);
      if (response.ok) {
        const data = await response.json();
        const reported = parseInt(response.headers.get('X-Total-Count'), 10);
        if (Number.isFinite(reported)) setTotal(reported);
        const src = response.headers.get('X-Source');
        if (src) setSource(src);
        setHasMore(data.length >= size);
        setPage(pageNum);
        // Paging shifts the exact-match head off later pages, so the same
        // printing can come back twice — keep the first copy.
        setCards(prev => {
          if (!append) return data;
          const seen = new Set(prev.map(c => c.id));
          return [...prev, ...data.filter(c => !seen.has(c.id))];
        });
        // Exactly one match means the search already identified the card (set +
        // number usually does). Skip the "click the only result" step.
        if (!append && data.length === 1 && !selectMode) openQuickAdd(data[0]);
      } else {
        const errData = await response.json().catch(() => ({}));
        let errKey = null;
        if (response.status === 403 || errData.error === 'Invalid API Key') {
          errKey = 'invalid-key';
        } else if (response.status === 429 || errData.error === 'Rate limit exceeded') {
          errKey = 'rate-limit';
        } else if (response.status === 400 && errData.error === 'INVALID_QUERY') {
          errKey = 'invalid-query';
        } else if (response.status === 503) {
          errKey = 'upstream';
        }
        if (errKey) setSearchError(errKey);
        // The banner already explains an invalid query; a toast on top of it
        // is just noise.
        if (errKey !== 'invalid-query') showToast(errData.error || t('search.errRequest'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('search.errApi'));
    } finally {
      setLoading(false);
      setLoadingMore(false);
    }
  };

  const handleSearch = (e) => {
    if (e) e.preventDefault();
    // runSearch knows what is a valid search (raw query, name, or
    // number/set fields) — an empty box simply no-ops.
    runSearch(1);
  };

  // Live search while typing, Moxfield-style: the box fires ~450ms after the
  // user stops typing (plain names only — a query mid-operator, "set:le",
  // would just 404 or misroute, so syntax waits for Enter). Every keystroke
  // after that re-fires, and the AbortController cancels the in-flight request
  // so a slow answer for "so" cannot land on top of the results for "sol".
  // Cache-sourced answers (local mode) are instant, so the debounce cost is
  // only felt on live Scryfall queries, where it also protects the 2/sec
  // rate limit from one query typed too eagerly.
  const abortRef = useRef(null);
  const debounceRef = useRef(null);
  const liveText = searchText.trim();
  const liveLang = searchLang;
  useEffect(() => {
    if (!liveText || looksLikeSyntax(liveText) || liveText.length < 2) {
      return; // live search is for plain names; syntax fires on Enter
    }
    clearTimeout(debounceRef.current);
    debounceRef.current = setTimeout(() => {
      abortRef.current?.abort();
      const controller = new AbortController();
      abortRef.current = controller;
      const doLive = async () => {
        try {
          const params = new URLSearchParams();
          params.append('name', liveText);
          if (numberQuery) params.append('number', numberQuery);
          if (setCodeQuery) params.append('set', setCodeQuery);
          params.append('scope', 'internet');
          params.append('lang', liveLang);
          params.append('page', '1');
          params.append('limit', String(pageSize));
          const res = await fetch(`/api/search?${params.toString()}`, { signal: controller.signal });
          if (controller.signal.aborted) return;
          if (res.ok) {
            const data = await res.json();
            const reported = parseInt(res.headers.get('X-Total-Count'), 10);
            if (Number.isFinite(reported)) setTotal(reported);
            const src = res.headers.get('X-Source');
            if (src) setSource(src);
            // Merge, never clobber: a shorter answer must not shrink a longer
            // visible list (a typo mid-typing is still typed text).
            setCards(prev => {
              if (data.length <= prev.length && prev.length > 0) return prev;
              const seen = new Set(prev.map(c => c.id));
              return [...data, ...prev.filter(c => !seen.has(c.id))];
            });
          }
        } catch (err) {
          if (err.name === 'AbortError') return;
          console.error('live search failed', err);
        }
      };
      doLive();
    }, 450);
    return () => clearTimeout(debounceRef.current);
  }, [liveText, liveLang, numberQuery, setCodeQuery, pageSize]);

  useEffect(() => () => {
    abortRef.current?.abort();
    clearTimeout(debounceRef.current);
  }, []);

  const changePageSize = (size) => {
    setPageSize(size);
    localStorage.setItem('search_page_size', String(size));
    if (searching) runSearch(1, size);
  };

  // Magic facets, offered only for values present in the loaded results.
  const uniqueRarities = useMemo(() => sortRarities(Array.from(new Set(cards.map(c => c.rarity).filter(Boolean)))), [cards]);
  const uniqueTypes = useMemo(() => CARD_TYPES.filter(ty => cards.some(c => cardTypes(c).includes(ty))), [cards]);
  const uniqueSupertypes = useMemo(() => SUPERTYPES.filter(st => cards.some(c => cardSupertypes(c).includes(st))), [cards]);
  const uniqueSubtypes = useMemo(() => uniqueSorted(cards, cardSubtypes), [cards]);
  const uniqueMvs = useMemo(() => MANA_VALUES.filter(b => cards.some(c => mvBucket(c.cmc) === b)), [cards]);

  const activeFilterCount = [filterColors, filterIdentity, filterType, filterSubtype, filterSupertype, filterMv, filterRarity]
    .filter(v => v.length > 0).length;
  function clearFilters() {
    setFilterColors([]); setColorMode('any'); setFilterIdentity([]);
    setFilterType([]); setFilterSubtype([]); setFilterSupertype([]); setFilterMv([]); setFilterRarity([]);
  }

  // Apply filters and sorting
  const filteredAndSortedCards = useMemo(() => {
    let result = cards.filter(c => matchMtgFilters(c, {
      colors: filterColors, colorMode, identity: filterIdentity,
      cardTypes: filterType, subtypes: filterSubtype, supertypes: filterSupertype,
      manaValues: filterMv, rarities: filterRarity,
    }));

    // Apply sorting
    if (sortBy === 'name-asc') {
      result.sort((a, b) => a.name.localeCompare(b.name));
    } else if (sortBy === 'name-desc') {
      result.sort((a, b) => b.name.localeCompare(a.name));
    } else if (sortBy === 'price-asc') {
      result.sort((a, b) => (a.price_trend || 0) - (b.price_trend || 0));
    } else if (sortBy === 'price-desc') {
      result.sort((a, b) => (b.price_trend || 0) - (a.price_trend || 0));
    } else if (sortBy === 'number-asc') {
      result.sort((a, b) => {
        const numA = parseInt(a.number, 10);
        const numB = parseInt(b.number, 10);
        if (!isNaN(numA) && !isNaN(numB)) return numA - numB;
        return a.number.localeCompare(b.number);
      });
    } else if (sortBy === 'mv-asc') {
      result.sort((a, b) => (a.cmc ?? 99) - (b.cmc ?? 99) || a.name.localeCompare(b.name));
    } else if (sortBy === 'mv-desc') {
      result.sort((a, b) => (b.cmc ?? -1) - (a.cmc ?? -1) || a.name.localeCompare(b.name));
    } else if (sortBy === 'rarity-desc') {
      result.sort((a, b) => RARITY_ORDER.indexOf(b.rarity) - RARITY_ORDER.indexOf(a.rarity) || a.name.localeCompare(b.name));
    } else if (sortBy === 'number-desc') {
      result.sort((a, b) => {
        const numA = parseInt(a.number, 10);
        const numB = parseInt(b.number, 10);
        if (!isNaN(numA) && !isNaN(numB)) return numB - numA;
        return b.number.localeCompare(a.number);
      });
    }

    return result;
  }, [cards, filterColors, colorMode, filterIdentity, filterType, filterSubtype, filterSupertype, filterMv, filterRarity, sortBy]);

  // Tap: swallowed if a long-press just armed selection; otherwise toggle (in
  // select mode) or open Quick Add. Mirrors CollectionList.activateCard.
  const handleCardClick = (card, event) => {
    if (longPressFired.current) { longPressFired.current = false; return; }
    if (selectMode) selectAt(card.id, filteredAndSortedCards.map(c => c.id), event?.shiftKey);
    else openQuickAdd(card);
  };

  const handleBulkAdd = async () => {
    const selectedCards = filteredAndSortedCards.filter(c => selectedIds.has(c.id));
    const ids = selectedCards.map(c => c.id);
    if (ids.length === 0) { showToast(t('search.errNoneSelected')); return; }
    setBulkAdding(true);
    try {
      const response = await fetch('/api/collection/bulk-add', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          card_ids: ids,
          quantity: parseInt(quantity, 10) || 1,
          condition,
          printing,
          language,
          purchase_price: parseFloat(purchasePrice) || 0
        })
      });
      const data = await response.json().catch(() => ({}));
      if (response.ok) {
        showToast(data.message || t('search.addedCards', { count: ids.length }));
        // Owned badges are logical-card totals. Adding two selected reprints of
        // one name raises every displayed printing by two copies.
        const added = parseInt(quantity, 10) || 1;
        const deltaByName = new Map();
        for (const card of selectedCards) {
          const key = cardKey(card);
          deltaByName.set(key, (deltaByName.get(key) || 0) + added);
        }
        setCards(prev => prev.map(card => {
          const delta = deltaByName.get(cardKey(card)) || 0;
          return delta ? { ...card, owned_qty: (card.owned_qty || 0) + delta } : card;
        }));
        exitSelectMode();
        onAddSuccess();
      } else {
        showToast(data.error || t('search.errBulkAdd'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('search.errAddCards'));
    } finally {
      setBulkAdding(false);
    }
  };

  // One card straight into the collection, no drawer. Stacked on purpose: one
  // Enter press becomes exactly one row, so undo removes exactly what it added
  // (an unstacked qty-3 add would leave two orphan copies behind).
  const addCardNow = async (card) => {
    const response = await fetch('/api/collection', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        card_id: card.id,
        quantity: parseInt(quantity, 10) || 1,
        condition,
        printing,
        language,
        purchase_price: parseFloat(purchasePrice) || 0,
        stackable: true
      })
    });
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(data.error || t('search.errAddCard'));
    return data;
  };

  // Enter in the rapid field: look the number up in the pinned set and add it.
  // One unambiguous match adds immediately; anything else falls back to the
  // normal result grid rather than guessing which printing was meant.
  const handleRapidAdd = async () => {
    const number = rapidNumber.trim();
    if (!number || rapidBusy) return;
    if (!setCodeQuery.trim()) { showToast(t('search.errNoSetCode')); return; }
    setRapidBusy(true);
    try {
      const params = new URLSearchParams({
        number, set: setCodeQuery, scope: 'internet', lang: searchLang, page: '1', limit: '10'
      });
      const res = await fetch(`/api/search?${params.toString()}`);
      if (!res.ok) {
        const err = await res.json().catch(() => ({}));
        showToast(err.error || t('search.errLookup'));
        return;
      }
      const matches = await res.json();
      const exact = matches.filter(c => String(c.number) === number || parseInt(c.number, 10) === parseInt(number, 10));
      const hit = exact.length === 1 ? exact[0] : (matches.length === 1 ? matches[0] : null);

      if (!hit) {
        if (matches.length === 0) {
          showToast(t('search.errNoSuchNumber', { number, set: setCodeQuery.toUpperCase() }));
        } else {
          // Ambiguous: show them and let the user pick, keeping the number typed.
          setCards(matches);
          setSearching(true);
          showToast(t('search.pickPrinting', { count: matches.length, number }));
        }
        return;
      }

      const result = await addCardNow(hit);
      setRapidLog(prev => [{ entryId: result.id, card: hit, qty: parseInt(quantity, 10) || 1 }, ...prev].slice(0, 25));
      setRapidNumber('');
      // Keep every displayed printing's logical owned badge in sync.
      setCards(prev => adjustOwnedQuantityByName(
        prev,
        hit,
        parseInt(quantity, 10) || 1
      ));
      onAddSuccess();
    } catch (err) {
      console.error(err);
      showToast(err.message || t('search.errAddCardGeneric'));
    } finally {
      setRapidBusy(false);
      // Focus never leaves the field, so the next number can just be typed.
      rapidInputRef.current?.focus();
    }
  };

  const undoRapidAdd = async (entry) => {
    try {
      const res = await fetch(`/api/collection/${entry.entryId}`, { method: 'DELETE' });
      if (!res.ok) { showToast(t('search.errUndo')); return; }
      setRapidLog(prev => prev.filter(e => e.entryId !== entry.entryId));
      setCards(prev => adjustOwnedQuantityByName(prev, entry.card, -entry.qty));
      showToast(t('search.removed', { name: displayName(entry.card) }));
      onAddSuccess();
    } catch (err) {
      console.error(err);
      showToast(t('search.errUndoGeneric'));
    }
  };

  const openQuickAdd = (card) => {
    setSelectedCard(card);
    setPurchasePrice(0); // Default to 0 purchase spend
    // The card itself knows which printing it is, so the copy is recorded in that
    // language rather than defaulting to English and needing a manual correction.
    setLanguage(card.language || langName(searchLang));
    // Rarity does not imply finish; default to the nonfoil copy and let the
    // collector choose Foil when the physical card is actually foil.
    setPrinting('Normal');

    setIsDrawerOpen(true);
  };

  const closeDrawer = () => {
    setIsDrawerOpen(false);
    setIsFullScreen(false);
    setSelectedCard(null);
    setQuantity(1);
    setCondition('Near Mint');
    setPrinting('Normal');
    // Back to the searched language, not hard-coded English: someone adding a run
    // of Japanese cards should not have to re-pick it for every card.
    setLanguage(langName(searchLang));
    setPurchasePrice(0);
  };

  const handleSubmit = async (e) => {
    e.preventDefault();
    if (!selectedCard) return;

    try {
      const response = await fetch('/api/collection', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          card_id: selectedCard.id,
          quantity: parseInt(quantity, 10),
          condition,
          printing,
          language,
          purchase_price: parseFloat(purchasePrice) || 0
        })
      });

      if (response.ok) {
        showToast(t('search.addedToCollection', { name: displayName(selectedCard) }));
        setCards(prev => adjustOwnedQuantityByName(
          prev,
          selectedCard,
          parseInt(quantity, 10) || 1
        ));
        
        // Trigger confetti for rare/valuable cards!
        const rarity = (selectedCard.rarity || '').toLowerCase();
        const price = selectedCard.price_trend || 0;
        if (isPremiumRarity(rarity) || price > 10) {
          confetti({
            particleCount: 150,
            spread: 80,
            origin: { y: 0.6 }
          });
        }

        onAddSuccess(); // Update stats
        closeDrawer();
      } else {
        // Prefer the route's specific validation message to a generic save error.
        const body = await response.json().catch(() => null);
        showToast(body?.error || t('search.errAddDb'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('search.errSave'));
    }
  };

  // Helper to determine location type layout guidance
  return (
    <div>
      {/* Search Header Panel */}
      <div className="glass-panel" style={{ marginBottom: '2rem' }}>
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: '0.75rem', marginBottom: '1rem' }}>
          <h2 style={{ fontSize: '1.25rem', margin: 0, color: 'var(--text-strong)' }}>{t('search.title')}</h2>
        </div>
        <form onSubmit={handleSearch} style={{ display: 'grid', gridTemplateColumns: '1fr', gap: '1rem' }}>
          <div style={{ display: 'grid', gridTemplateColumns: '1fr', gap: '0.75rem' }}>
            <div style={{ display: 'flex', flexDirection: 'column', gap: '0.35rem' }}>
              <label style={{ fontSize: '0.75rem', fontWeight: 600, color: 'var(--text-secondary)' }}>{t('search.cardName')}</label>
              <div style={{ position: 'relative' }}>
                <input
                  type="text"
                  className="input-control"
                  placeholder={t('search.namePlaceholderMtg')}
                  value={searchText}
                  onChange={(e) => setSearchText(e.target.value)}
                  style={{
                    width: '100%',
                    paddingLeft: '2.5rem',
                    // Mono when it is a query, not a name — the query is a string
                    // with operators in it, and the typeface says so at a glance.
                    fontFamily: looksLikeSyntax(searchText.trim()) ? 'var(--font-mono, monospace)' : undefined,
                  }}
                />
                {looksLikeSyntax(searchText.trim())
                  ? <Braces size={16} style={{ position: 'absolute', left: '1rem', top: '50%', transform: 'translateY(-50%)', color: 'var(--text-muted)' }} />
                  : <Search size={16} style={{ position: 'absolute', left: '1rem', top: '50%', transform: 'translateY(-50%)', color: 'var(--text-muted)' }} />}
              </div>
              <p style={{ fontSize: '0.7rem', color: 'var(--text-muted)', margin: 0, lineHeight: 1.4 }}>{t('search.scryfallHint')}</p>
            </div>
          </div>

          {/* auto-fit rather than a fixed 2 columns: language made this row three
              fields wide, and they have to stay usable on a phone. In
              Scryfall-syntax mode only the language field remains (a language
              picker still makes sense — a Japanese card queried by English
              operator syntax is a normal request). */}
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(150px, 1fr))', gap: '0.75rem' }}>
            <div style={{ display: 'flex', flexDirection: 'column', gap: '0.35rem' }}>
              {/* The language of the cards being searched for, not the app's. */}
              <label style={{ fontSize: '0.75rem', fontWeight: 600, color: 'var(--text-secondary)' }}>{t('search.language')}</label>
              <select
                className="select-control"
                value={searchLang}
                onChange={(e) => {
                  const code = e.target.value;
                  setSearchLang(code);
                  // The copy being added is almost always in the language just
                  // searched, so make that the entry default instead of English.
                  setLanguage(langName(code));
                  // Set codes do not carry across languages (JP has sets the West
                  // never got), so a stale code would search a set that is not there.
                  setSetCodeQuery('');
                }}
              >
                {LANGUAGES.map(l => <option key={l.code} value={l.code}>{l.name}</option>)}
              </select>
            </div>
            {/* Plain-name fields. A Scryfall-syntax box ignores them (the query
                string is the whole search), so they hide out of the way. */}
            {!looksLikeSyntax(searchText.trim()) && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: '0.35rem' }}>
              <label style={{ fontSize: '0.75rem', fontWeight: 600, color: 'var(--text-secondary)' }}>{t('search.cardNumber')}</label>
              <input
                type="text"
                className="input-control"
                placeholder={t('search.numberPlaceholder')}
                value={numberQuery}
                onChange={(e) => setNumberQuery(e.target.value)}
              />
            </div>
            )}
            {!looksLikeSyntax(searchText.trim()) && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: '0.35rem' }}>
              <label style={{ fontSize: '0.75rem', fontWeight: 600, color: 'var(--text-secondary)' }}>{t('search.sets')}</label>
              <input
                type="text"
                className="input-control"
                list="known-set-codes"
                placeholder={t('search.setsPlaceholderMtg')}
                value={setCodeQuery}
                onChange={(e) => setSetCodeQuery(e.target.value)}
              />
              {/* Native datalist: free typeahead over every known set, no
                  dropdown component and no extra dependency. */}
              <datalist id="known-set-codes">
                {knownSets.map(s => <option key={s.code} value={s.code}>{s.name}</option>)}
              </datalist>
            </div>
            )}
          </div>

          <div style={{ display: 'flex', gap: '0.75rem', marginTop: '0.5rem', flexWrap: 'wrap' }}>
            <button type="submit" className="btn btn-primary" style={{ flex: '1 1 220px' }}>
              <Search size={18} />
              {t('search.submit')}
            </button>
            <button
              type="button"
              className={`btn ${rapidMode ? 'btn-primary' : 'btn-secondary'}`}
              onClick={() => {
                const next = !rapidMode;
                setRapidMode(next);
                if (next) setTimeout(() => rapidInputRef.current?.focus(), 0);
              }}
              title={t('search.rapidHint')}
              style={{ flex: '0 1 auto' }}
            >
              <Zap size={18} />
              {t(rapidMode ? 'search.rapidOn' : 'search.rapid')}
            </button>
          </div>
        </form>
      </div>

      {/* Rapid add: type a number, press Enter, next. */}
      {rapidMode && (
        <div className="glass-panel" style={{ marginBottom: '1.5rem', padding: '1rem', display: 'flex', flexDirection: 'column', gap: '0.75rem', borderLeft: '4px solid var(--accent-yellow)' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', flexWrap: 'wrap' }}>
            <Zap size={18} style={{ color: 'var(--accent-yellow)' }} />
            <strong style={{ color: 'var(--text-strong)', fontSize: '0.95rem' }}>
              {setCodeQuery ? t('search.rapidToSet', { set: setCodeQuery.toUpperCase() }) : t('search.rapid')}
            </strong>
            <span style={{ fontSize: '0.75rem', color: 'var(--text-muted)' }}>
              {t(setCodeQuery ? 'search.rapidReady' : 'search.rapidNeedsSet')}
            </span>
          </div>

          <div style={{ display: 'flex', gap: '0.5rem', alignItems: 'center', flexWrap: 'wrap' }}>
            <input
              ref={rapidInputRef}
              type="text"
              inputMode="numeric"
              className="input-control"
              placeholder={t('search.rapidNumberPlaceholder')}
              value={rapidNumber}
              // Never disabled mid-add: disabling blurs the field, and the
              // refocus would land on a still-disabled element, forcing a click
              // back in for every card. Re-entry is guarded in the handler.
              disabled={!setCodeQuery}
              onChange={(e) => setRapidNumber(e.target.value)}
              onKeyDown={(e) => { if (e.key === 'Enter') { e.preventDefault(); handleRapidAdd(); } }}
              style={{ flex: '1 1 180px', fontSize: '1.1rem', fontWeight: 700 }}
            />
            <select className="select-control" value={condition} onChange={(e) => setCondition(e.target.value)} style={{ fontSize: '0.75rem', maxWidth: '150px' }}>
              {CONDITIONS.map(c => <option key={c} value={c}>{c}</option>)}
            </select>
            <select className="select-control" value={printing} onChange={(e) => setPrinting(e.target.value)} style={{ fontSize: '0.75rem', maxWidth: '150px' }}>
              {PRINTING_OPTIONS.map(p => <option key={p.value} value={p.value}>{p.label}</option>)}
            </select>
            <input
              type="number"
              min="1"
              className="input-control"
              value={quantity}
              onChange={(e) => setQuantity(e.target.value)}
              title={t('search.copiesPerEnter')}
              style={{ width: '80px', fontSize: '0.75rem' }}
            />
            {rapidBusy && <span style={{ fontSize: '0.75rem', color: 'var(--text-muted)' }}>{t('search.adding')}</span>}
          </div>

          {rapidLog.length > 0 && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: '0.35rem', maxHeight: '220px', overflowY: 'auto' }}>
              <div style={{ fontSize: '0.7rem', fontWeight: 700, color: 'var(--text-muted)', textTransform: 'uppercase', letterSpacing: '0.03em' }}>
                {t('search.addedThisSession', { count: rapidLog.length })}
              </div>
              {rapidLog.map(entry => (
                <div key={entry.entryId} style={{ display: 'flex', alignItems: 'center', gap: '0.6rem', background: 'rgba(255,255,255,0.02)', padding: '0.35rem 0.6rem', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border-glass)' }}>
                  <CardImage card={entry.card} alt="" style={{ width: '28px', borderRadius: '3px' }} />
                  <span style={{ fontSize: '0.8rem', color: 'var(--text-strong)', fontWeight: 600, flex: 1, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    #{entry.card.number} {displayName(entry.card)}{entry.qty > 1 ? ` ×${entry.qty}` : ''}
                  </span>
                  <button
                    type="button"
                    className="btn btn-secondary"
                    style={{ fontSize: '0.7rem', padding: '0.2rem 0.5rem' }}
                    onClick={() => undoRapidAdd(entry)}
                  >
                    <Undo2 size={12} /> {t('search.undo')}
                  </button>
                </div>
              ))}
            </div>
          )}
        </div>
      )}

      {searchError && (
        <div className="glass-panel" style={{ borderLeft: '4px solid var(--accent-red)', background: 'rgba(239, 68, 68, 0.08)', padding: '1.25rem', marginBottom: '1.5rem', display: 'flex', flexDirection: 'column', gap: '0.5rem' }}>
          <h3 style={{ fontSize: '0.95rem', fontWeight: 700, color: 'var(--accent-red)', display: 'flex', alignItems: 'center', gap: '0.5rem', margin: 0 }}>
            <ShieldAlert size={18} />
            {t(`searchErr.${searchError}.title`)}
          </h3>
          <p style={{ fontSize: '0.85rem', color: 'var(--text-secondary)', margin: 0, lineHeight: 1.4 }}>
            {t(`searchErr.${searchError}.body`)}
          </p>
        </div>
      )}

      {/* Loading state */}
      {loading && <div className="spinner"></div>}

      {/* Filters and Sorting Panel */}
      {!loading && cards.length > 0 && (
        <div className="glass-panel" style={{ marginBottom: '1.5rem', padding: '1rem' }}>
          <div style={{ display: 'flex', gap: '0.5rem', alignItems: 'flex-end', flexWrap: 'wrap' }}>
            <div className="search-field" style={{ flex: '1 1 150px' }}>
              <span>{t('search.sortBy')}</span>
              <select className="select-control" value={sortBy} onChange={e => setSortBy(e.target.value)}>
                {['relevance', 'name-asc', 'name-desc', 'price-desc', 'price-asc', 'mv-asc', 'mv-desc', 'rarity-desc', 'number-asc', 'number-desc']
                  .map(key => <option key={key} value={key}>{t(`search.sort.${key}`)}</option>)}
              </select>
            </div>
            <div className="search-field" style={{ flex: '0 1 110px' }}>
              <span>{t('search.cardsPerPage')}</span>
              <select className="select-control" value={pageSize} onChange={e => changePageSize(parseInt(e.target.value, 10))}>
                {[30, 60, 120, 250].map(n => <option key={n} value={n}>{n}</option>)}
              </select>
            </div>
            <button
              type="button"
              className={`btn ${showFilters ? 'btn-primary' : 'btn-secondary'}`}
              onClick={() => setShowFilters(v => !v)}
              aria-expanded={showFilters}
              style={{ height: '40px', display: 'inline-flex', alignItems: 'center', gap: '0.4rem', whiteSpace: 'nowrap' }}
            >
              <SlidersHorizontal size={15} /> {t('collection.filters')}
              {activeFilterCount > 0 && <span className="filter-count">{activeFilterCount}</span>}
            </button>
          </div>

          {showFilters && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: '0.75rem', marginTop: '0.75rem', paddingTop: '0.75rem', borderTop: '1px solid var(--border-glass)' }}>
              <div className="mtg-color-row">
                <div className="search-field">
                  <span>{t('mtg.color')}</span>
                  <ColorPicker label={t('mtg.color')} value={filterColors} onChange={setFilterColors} mode={colorMode} onModeChange={setColorMode} />
                </div>
                <div className="search-field">
                  <span>{t('mtg.identity')}</span>
                  <ColorPicker label={t('mtg.identity')} value={filterIdentity} onChange={setFilterIdentity} withColorless={false} />
                </div>
              </div>
              <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(140px, 1fr))', gap: '0.75rem' }}>
                <div className="search-field"><span>{t('mtg.cardType')}</span>
                  <MultiSelectDropdown label={t('mtg.cardType')} allLabel={t('collection.allTypes')} value={filterType} onChange={setFilterType}
                    options={uniqueTypes.map(v => ({ value: v, label: v }))} />
                </div>
                <div className="search-field"><span>{t('mtg.subtype')}</span>
                  <MultiSelectDropdown label={t('mtg.subtype')} allLabel={t('mtg.allSubtypes')} value={filterSubtype} onChange={setFilterSubtype}
                    options={uniqueSubtypes.map(v => ({ value: v, label: v }))} />
                </div>
                {uniqueSupertypes.length > 0 && (
                  <div className="search-field"><span>{t('collection.fSupertype')}</span>
                    <MultiSelectDropdown label={t('collection.fSupertype')} allLabel={t('collection.allSupertypes')} value={filterSupertype} onChange={setFilterSupertype}
                      options={uniqueSupertypes.map(v => ({ value: v, label: v }))} />
                  </div>
                )}
                <div className="search-field"><span>{t('collection.fManaValue')}</span>
                  <MultiSelectDropdown label={t('collection.fManaValue')} allLabel={t('collection.allManaValues')} value={filterMv} onChange={setFilterMv}
                    options={uniqueMvs.map(v => ({ value: v, label: v }))} />
                </div>
                <div className="search-field"><span>{t('collection.fRarity')}</span>
                  <MultiSelectDropdown label={t('collection.fRarity')} allLabel={t('collection.allRarities')} value={filterRarity} onChange={setFilterRarity}
                    options={uniqueRarities.map(v => ({ value: v, label: v }))} />
                </div>
              </div>
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', gap: '0.5rem', flexWrap: 'wrap' }}>
                <span style={{ fontSize: '0.72rem', color: 'var(--text-muted)' }}>{t('mtg.loadedOnly')}</span>
                {activeFilterCount > 0 && (
                  <button type="button" className="btn btn-secondary" onClick={clearFilters} style={{ fontSize: '0.72rem', padding: '0.3rem 0.7rem', display: 'inline-flex', alignItems: 'center', gap: '0.3rem' }}>
                    <X size={13} /> {t('collection.clearFilters')}
                  </button>
                )}
              </div>
            </div>
          )}
          <div style={{ marginTop: '0.75rem', display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: '0.5rem' }}>
            <span style={{ fontSize: '0.75rem', color: 'var(--text-muted)', display: 'inline-flex', alignItems: 'center', gap: '0.5rem', flexWrap: 'wrap' }}>
              {source && (
                <span
                  title={source === 'cache' ? t('search.sourceCacheTitle') : t('search.sourceLiveTitle')}
                  style={{
                    fontSize: '0.65rem',
                    fontWeight: 700,
                    letterSpacing: '0.03em',
                    textTransform: 'uppercase',
                    padding: '0.15rem 0.5rem',
                    borderRadius: '999px',
                    border: '1px solid var(--border-glass)',
                    background: source === 'cache' ? 'rgba(34, 197, 94, 0.12)' : 'rgba(59, 130, 246, 0.12)',
                    color: source === 'cache' ? 'var(--accent-green, #4ade80)' : 'var(--accent-blue, #60a5fa)',
                  }}
                >
                  {source === 'cache' ? t('search.sourceCache') : t('search.sourceLive')}
                </span>
              )}
              {t('search.showingMatches', { shown: filteredAndSortedCards.length, count: total != null ? total : cards.length })}
              {total != null && cards.length < total ? ` ${t('search.loadedSuffix', { loaded: cards.length })}` : ''}
            </span>
            {/* Same control, label and icon as the collection's select toggle. */}
            <button
              type="button"
              className={`btn ${selectMode ? 'btn-primary' : 'btn-secondary'}`}
              onClick={() => (selectMode ? exitSelectMode() : setSelectMode(true))}
              style={{ fontSize: '0.8rem', padding: '0.4rem 0.9rem', display: 'inline-flex', alignItems: 'center', gap: '0.35rem' }}
              title={t('collection.selectHint')}
            >
              <MousePointerClick size={14} />
              {t(selectMode ? 'bulk.done' : 'collection.select')}
            </button>
          </div>
        </div>
      )}

      {/* Bulk add bar — sticky single row, matching the collection's bulk bar. */}
      {selectMode && (
        <div className="glass-panel" style={{ marginBottom: '1rem', padding: '0.75rem 1rem', display: 'flex', alignItems: 'center', gap: '0.5rem', flexWrap: 'wrap', position: 'sticky', top: '0.5rem', zIndex: 30 }}>
          <span style={{ fontWeight: 800, color: 'var(--text-strong)', fontSize: '0.85rem' }}>{t('bulk.selected', { count: selectedIds.size })}</span>
          <button className="btn btn-secondary" style={{ fontSize: '0.72rem', padding: '0.3rem 0.6rem' }} onClick={() => setSelectedIds(new Set(filteredAndSortedCards.map(c => c.id)))}>{t('bulk.selectAll', { count: filteredAndSortedCards.length })}</button>
          <button className="btn btn-secondary" style={{ fontSize: '0.72rem', padding: '0.3rem 0.6rem' }} onClick={clearSelection}>{t('bulk.clear')}</button>
          <div style={{ width: '1px', height: '22px', background: 'var(--border-glass)' }} />
          <select className="select-control" value={condition} onChange={(e) => setCondition(e.target.value)} style={{ fontSize: '0.72rem', maxWidth: '150px', padding: '0.3rem 0.4rem' }}>
            {CONDITIONS.map(c => <option key={c} value={c}>{c}</option>)}
          </select>
          <select className="select-control" value={printing} onChange={(e) => setPrinting(e.target.value)} style={{ fontSize: '0.72rem', maxWidth: '150px', padding: '0.3rem 0.4rem' }}>
            {PRINTING_OPTIONS.map(p => <option key={p.value} value={p.value}>{p.label}</option>)}
          </select>
          <input
            type="number"
            min="1"
            className="input-control"
            value={quantity}
            onChange={(e) => setQuantity(e.target.value)}
            title={t('search.copiesEachSelected')}
            style={{ fontSize: '0.72rem', width: '70px', padding: '0.3rem 0.4rem' }}
          />
          <button
            className="btn btn-primary"
            style={{ fontSize: '0.72rem', padding: '0.3rem 0.6rem' }}
            disabled={bulkAdding || selectedIds.size === 0}
            onClick={handleBulkAdd}
          >
            {bulkAdding ? t('search.adding') : t('search.addN', { count: selectedIds.size })}
          </button>
          <button className="btn btn-secondary" style={{ fontSize: '0.72rem', padding: '0.3rem 0.6rem', marginLeft: 'auto' }} onClick={exitSelectMode}>{t('bulk.done')}</button>
        </div>
      )}

      {/* Search Results Grid */}
      {!loading && cards.length > 0 && filteredAndSortedCards.length > 0 && (
        <div className="card-grid">
          {filteredAndSortedCards.map((card) => {
            const isSelected = selectedIds.has(card.id);
            return (
              <div
                key={card.id}
                className="tcg-card"
                style={{ cursor: 'pointer', touchAction: 'pan-y' }}
                onClick={(e) => handleCardClick(card, e)}
                {...pressHandlers(card.id)}
              >
                <div className="tcg-card-inner" style={isSelected ? { outline: '3px solid var(--accent-red)', outlineOffset: '2px' } : undefined}>
                  {/* Same check bubble the collection uses for selection. */}
                  {selectMode && (
                    <div style={{ position: 'absolute', top: '6px', right: '6px', zIndex: 20, width: '22px', height: '22px', borderRadius: '50%', background: isSelected ? 'var(--accent-red)' : 'rgba(0,0,0,0.6)', border: '2px solid #fff', display: 'flex', alignItems: 'center', justifyContent: 'center', color: 'var(--text-strong)', fontSize: '0.8rem', fontWeight: 900 }}>{isSelected ? '✓' : ''}</div>
                  )}
                  <CardImage card={card} className="tcg-card-image" loading="lazy" draggable={false} />
                  {/* Already-owned count, so a set browse doesn't invite
                      re-adding what the user already has. */}
                  {card.owned_qty > 0 && (
                    <div style={{ position: 'absolute', top: '8px', left: '8px', background: 'var(--accent-green, #22c55e)', color: '#04210f', padding: '2px 6px', borderRadius: '4px', display: 'flex', alignItems: 'center', gap: '3px', fontSize: '0.65rem', fontWeight: 800 }}>
                      <Check size={10} /> {card.owned_qty}
                    </div>
                  )}
                  {!selectMode && (
                    <div style={{ position: 'absolute', bottom: '8px', right: '8px', background: 'rgba(0,0,0,0.85)', padding: '2px 6px', borderRadius: '4px', border: '1px solid var(--border-glass-hover)', display: 'flex', alignItems: 'center', gap: '4px' }}>
                      <Plus size={10} style={{ color: 'var(--accent-red)' }} />
                      <span style={{ fontSize: '0.65rem', fontWeight: 700 }}>{t('search.quickAdd')}</span>
                    </div>
                  )}
                </div>
                <div className="tcg-card-info">
                  <div className="tcg-card-name">{displayName(card)}</div>
                  {/* Second line only when the localized name needs help: the
                      English name where a provider gives us one (Magic always
                      does), otherwise the set code — language-independent, and the
                      only handle you have on a card whose name you can't read. */}
                  {translatedName(card) ? (
                    <div style={{ fontSize: '0.68rem', color: 'var(--text-secondary)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                      {translatedName(card)}
                    </div>
                  ) : !isEnglish(card.language) && setReference(card) ? (
                    <div style={{ fontSize: '0.68rem', color: 'var(--text-muted)', fontFamily: 'monospace', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                      {setReference(card)}
                    </div>
                  ) : null}
                  <div className="tcg-card-meta">
                    <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', maxWidth: '70%' }}>{card.set_name}</span>
                    <span className="tcg-card-price">{priceText(card.price_trend, card.price_currency)}</span>
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      )}

      {/* Load More */}
      {!loading && hasMore && cards.length > 0 && (
        <LoadMore busy={loadingMore} onLoad={() => runSearch(page + 1)}>
          {loadingMore ? t('common.loading') : t('search.loadMore', { count: pageSize })}
        </LoadMore>
      )}

      {/* Filtered Empty State */}
      {!loading && cards.length > 0 && filteredAndSortedCards.length === 0 && (
        <div className="glass-panel" style={{ textAlign: 'center', color: 'var(--text-secondary)', padding: '3rem 1.5rem', marginBottom: '2rem' }}>
          <p>{t('search.noFilterMatches')}</p>
        </div>
      )}

      {/* Empty State */}
      {!loading && searching && !searchError && cards.length === 0 && (
        <div className="glass-panel" style={{ textAlign: 'center', color: 'var(--text-secondary)', padding: '3rem 1.5rem' }}>
          <p>{t('search.noQueryMatches')}</p>
        </div>
      )}

      {/* Drawer Dialog Backdrop */}
      <div className={`drawer-backdrop ${isDrawerOpen ? 'open' : ''}`} onClick={closeDrawer}></div>

      {/* Quick Add Drawer Sheet */}
      <div className={`quick-add-drawer ${isDrawerOpen ? 'open' : ''}`}>
        {selectedCard && (
          <div style={{ display: 'flex', flexDirection: 'column', gap: '1.25rem' }}>
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start' }}>
              <div>
                <h3 style={{ color: 'var(--text-strong)', fontSize: '1.25rem' }}>{t('search.addCardTitle')}</h3>
                <p style={{ color: 'var(--text-secondary)', fontSize: '0.85rem' }}>
                  {displayName(selectedCard)}
                  {translatedName(selectedCard) && <span style={{ color: 'var(--text-muted)' }}> ({translatedName(selectedCard)})</span>}
                  {' '}({selectedCard.set_name}
                  {/* Code only where the set name isn't readable to an English speaker. */}
                  {!isEnglish(selectedCard.language) && setCode(selectedCard) ? ` / ${setCode(selectedCard)}` : ''}
                  {' • '}#{selectedCard.number})
                </p>
              </div>
              <button className="btn btn-secondary btn-icon-only modal-close" onClick={closeDrawer} style={{ borderRadius: '50%' }}>
                <X size={18} />
              </button>
            </div>

            <div style={{ display: 'flex', gap: '1rem', alignItems: 'center', background: 'rgba(255, 255, 255, 0.02)', padding: '1rem', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border-glass)' }}>
              {/* Tap the art to enlarge, same as the collection inspector. */}
              <div
                onClick={() => setIsFullScreen(true)}
                title={t('inspector.zoomHint')}
                style={{ position: 'relative', flexShrink: 0, cursor: 'pointer', lineHeight: 0 }}
              >
                <CardImage card={selectedCard} style={{ width: '80px', aspectRatio: 0.718, objectFit: 'cover', borderRadius: 'var(--radius-sm)', boxShadow: '0 4px 10px rgba(0,0,0,0.3)' }} />
                <div style={{
                  position: 'absolute', bottom: '4px', right: '4px',
                  background: 'rgba(0,0,0,0.65)', backdropFilter: 'blur(6px)',
                  padding: '2px 4px', borderRadius: '4px', color: '#fff',
                  display: 'flex', alignItems: 'center', pointerEvents: 'none',
                  border: '1px solid rgba(255,255,255,0.15)'
                }}>
                  <Maximize2 size={11} />
                </div>
              </div>
              <div>
                <div style={{ fontSize: '0.75rem', color: 'var(--text-muted)' }}>{t('search.tcgMarketPrice', { printing: getPrintingLabel(printing) })}</div>
                <div style={{ fontSize: '1.8rem', fontWeight: 800, color: 'var(--accent-yellow)' }}>{priceText(resolveCardPrice(selectedCard, printing), selectedCard.price_currency)}</div>
                <div style={{ fontSize: '0.75rem', color: 'var(--text-secondary)' }}>{t('search.rarityLabel')} <span style={{ color: 'var(--text-strong)', fontWeight: 600 }}>{selectedCard.rarity}</span></div>
              </div>
            </div>

            <form onSubmit={handleSubmit}>
              <CardEntryFields
                quantity={quantity} purchasePrice={purchasePrice} condition={condition} printing={printing} language={language}
                onQuantity={setQuantity} onPurchasePrice={setPurchasePrice} onCondition={setCondition} onPrinting={setPrinting} onLanguage={setLanguage}
              />



              <div style={{ display: 'flex', gap: '0.75rem', marginTop: '1.5rem' }}>
                <button type="button" className="btn btn-secondary" onClick={closeDrawer} style={{ flex: 1 }}>{t('common.cancel')}</button>
                <button type="submit" className="btn btn-primary" style={{ flex: 2 }}>{t('search.addToCollection')}</button>
              </div>
            </form>
          </div>
        )}
      </div>

      {/* Outside the drawer on purpose: .quick-add-drawer is transformed, and a
          transformed ancestor becomes the containing block for position:fixed,
          which would trap this overlay inside the drawer instead of the page. */}
      {isFullScreen && selectedCard && (
        <CardImageZoom card={selectedCard} onClose={() => setIsFullScreen(false)} />
      )}
    </div>
  );
}

export default CardSearch;

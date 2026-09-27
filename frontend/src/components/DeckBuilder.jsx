import { useState, useEffect, useRef, useMemo } from 'react';
import { looksLikeSyntax } from '../utils/scryfallSyntax';
import { Plus, Trash2, X, ChevronLeft, Play, BarChart2, Search, LogOut, PackageCheck, LayoutGrid, List, ClipboardList, PackagePlus, Download, Upload, Eye, Filter, Layers, ListChecks, Copy, Gamepad2, SlidersHorizontal, FolderPlus, FileText, Globe, PackageOpen, DollarSign, ExternalLink, ShoppingCart } from 'lucide-react';
import { ResponsiveContainer, PieChart, Pie, Cell, BarChart, Bar, XAxis, YAxis, Tooltip } from 'recharts';
import { shuffleArray } from '../utils/shuffle';
import { displayName } from '../utils/languages';
import CheckoutWizardModal from './CheckoutWizardModal';
import OverflowMenu from './OverflowMenu';
import AddDeckChoiceModal from './AddDeckChoiceModal';
import PreconSearchModal from './PreconSearchModal';
import { useBackGuard } from '../utils/useBackGuard';
import { getRememberedView, rememberOpen, clearOpen } from '../utils/viewMemory';
import { buildDeckExport, parseDeckLine, missingEntries } from '../utils/deckText';
import CardImage from './CardImage';
import { useT } from '../utils/i18n';
import { canRegisterDeckInCollection, deckRegistrationCardCount } from '../utils/deckCollectionRegistration';
import { cardKey, findSameCard } from '../utils/cardIdentity';
import { deckMinimumValueHint, deckMinimumValueText, deckUnpricedCountText } from '../utils/deckMinimumValue';
import { buildCardListText } from '../utils/cardList';
import { buildManapoolUrl } from '../utils/manapoolUrl';
import { buildTcgMassEntryUrl } from '../utils/tcgMassEntryUrl';
import { priceText } from '../utils/formatPrice';
import { deriveDeckRenderData, isBasicLand } from '../utils/deckRenderData';

const EMPTY_DECK_CARDS = [];

function DeckBuilder({ showToast, onNavigate }) {
  const { t } = useT();
  const [decks, setDecks] = useState([]);
  const [activeDeck, setActiveDeck] = useState(null);
  const [loading, setLoading] = useState(true);
  const [viewMode, setViewMode] = useState('list'); // 'list' or 'detail'
  // Mobile-only editor tabs (CSS hides the inactive panes at <=768px).
  const [editorTab, setEditorTab] = useState('cards'); // 'cards' | 'add' | 'stats'
  
  // Deck View & Display Modes
  // Grid by default; the user's last explicit choice is remembered.
  const [cardDisplayMode, setCardDisplayModeState] = useState(() => {
    try { return localStorage.getItem('bindarr.deckCardDisplayMode') === 'list' ? 'list' : 'grid'; } catch { return 'grid'; }
  }); // 'list' | 'grid'
  const setCardDisplayMode = (mode) => {
    setCardDisplayModeState(mode);
    try { localStorage.setItem('bindarr.deckCardDisplayMode', mode); } catch { /* storage unavailable */ }
  };
  const [previewCard, setPreviewCard] = useState(null);

  // Deck Creation States & Constants
  const MTG_FORMATS = ['Commander / EDH', 'Standard', 'Modern', 'Pioneer', 'Legacy', 'Vintage', 'Pauper'];
  const DECK_CATEGORIES = ['Competitive', 'Casual', 'Tournament', 'Theorycraft', 'Proxy', 'Trade'];
  const DECK_ACCENT_COLORS = [
    { name: 'Gold', hex: '#eab308' },
    { name: 'Red', hex: '#ef4444' },
    { name: 'Blue', hex: '#3b82f6' },
    { name: 'Green', hex: '#10b981' },
    { name: 'Purple', hex: '#a855f7' },
    { name: 'Slate', hex: '#64748b' },
    { name: 'Pink', hex: '#ec4899' },
    { name: 'Orange', hex: '#f97316' },
  ];

  const [showAddDeckModal, setShowAddDeckModal] = useState(false);
  const addDeckButtonRef = useRef(null);
  const [showCreateModal, setShowCreateModal] = useState(false);
  const [showPreconModal, setShowPreconModal] = useState(false);
  const [newDeckName, setNewDeckName] = useState('');
  const [newDeckDesc, setNewDeckDesc] = useState('');
  const [newDeckFormat, setNewDeckFormat] = useState('Commander / EDH');
  const [newDeckCategory, setNewDeckCategory] = useState('Competitive');
  const [newDeckAccentColor, setNewDeckAccentColor] = useState('#eab308');
  const [newDeckTargetSize, setNewDeckTargetSize] = useState(100);
  const [newDeckImportText, setNewDeckImportText] = useState('');
  const [showImportDecklistArea, setShowImportDecklistArea] = useState(false);
  
  // Card Search States inside editor
  const [searchQuery, setSearchQuery] = useState('');
  const [searchResults, setSearchResults] = useState([]);
  const [searching, setSearching] = useState(false);

  // Deck Selection Menu Controls
  const [deckSearchTerm, setDeckSearchTerm] = useState('');
  const [deckStatusFilter, setDeckStatusFilter] = useState('all'); // 'all' | 'ready' | 'in_progress' | 'in_play'
  const [deckSortBy, setDeckSortBy] = useState('created_desc'); // 'created_desc' | 'created_asc' | 'name_asc' | 'cards_desc'
  const [deckSelectionViewMode, setDeckSelectionViewMode] = useState('grid'); // 'grid' | 'table'

  // Draw Simulator States
  const [showSimulator, setShowSimulator] = useState(false);
  const [missingOpen, setMissingOpen] = useState(false);

  const [missingListName, setMissingListName] = useState('');
  const [simulatorDeck, setSimulatorDeck] = useState([]);
  const [hand, setHand] = useState([]);
  const [mulliganCount, setMulliganCount] = useState(0);

  // Import / Export Modals
  const [showImportModal, setShowImportModal] = useState(false);
  const [showExportModal, setShowExportModal] = useState(false);
  const [exportFormat, setExportFormat] = useState(null); // null = mtga
  const [importText, setImportText] = useState('');
  const [importComparison, setImportComparison] = useState(null);
  const [comparingImport, setComparingImport] = useState(false);

  // Checkout States
  const [checkingOut, setCheckingOut] = useState(false);
  const [registeringDeck, setRegisteringDeck] = useState(false);
  // The header's overflow (⋯) menu lives in OverflowMenu: a portaled panel
  // that escapes the glass-panel stacking trap. Its open state is internal.

  const [showCheckoutModal, setShowCheckoutModal] = useState(false);
  const [checkoutLocations, setCheckoutLocations] = useState([]);
  const [checkoutMode, setCheckoutMode] = useState('checkout'); // 'checkout' | 'checkin'
  const [checkoutDeckId, setCheckoutDeckId] = useState(null); // deck the open modal acts on

  // True while an add/qty write is in flight. Blocks overlapping clicks that
  // would otherwise each compute a new quantity from the same stale render and
  // clobber one another (last-writer-wins on the server upsert).
  const [savingCard, setSavingCard] = useState(false);

  // Deck buy modal (same shape as the Lists buy modal). The detail response
  // already carries every card, so the plain buylist is rebuilt from
  // activeDeck.cards on every open — synchronous, and a card add/remove
  // (which refetches the detail) can never leave stale lines behind.
  const [showBuy, setShowBuy] = useState(false);
  const [buyText, setBuyText] = useState('');

  useBackGuard(showCreateModal, () => setShowCreateModal(false));
  useBackGuard(showSimulator, () => setShowSimulator(false));
  useBackGuard(missingOpen, () => setMissingOpen(false));

  // Leaving the detail view has to clear BOTH halves of the view state. The two
  // render blocks are gated independently (`viewMode === 'list'` for the deck
  // list, `viewMode === 'detail' && activeDeck` for the editor), so clearing
  // only one of them shows neither: with activeDeck nulled but viewMode still
  // 'detail' the list is suppressed by viewMode and the editor is suppressed by
  // the empty deck, leaving a blank pane. That is exactly what browser Back did,
  // because the guard below only reset activeDeck. One helper, always both.
  const closeDeck = () => {
    setMissingOpen(false);
    setShowBuy(false);
    setBuyText('');
    setActiveDeck(null);
    setViewMode('list');
    clearOpen('deckbuilder');
    fetchDecks();
  };

  useBackGuard(!!activeDeck, closeDeck);
  useBackGuard(showBuy, () => setShowBuy(false));

  // Escape closes the deck buy modal (the overlay itself is never focused).
  // The effect sits above the list-view early return so hook order stays
  // stable — same rule as the Lists buy/copy modal.
  useEffect(() => {
    if (!showBuy) return;
    const onKey = (e) => { if (e.key === 'Escape') setShowBuy(false); };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [showBuy]);

  useEffect(() => {
    // A refresh inside a deck should land back inside it: fetch the list first
    // (its rows carry the checkout badges the editor shows), then reopen the
    // deck the memory names. A dead id (deleted elsewhere) quietly stops at the
    // deck list — loadDeckDetails shows its own error toast for real failures,
    // so route the reopen through it once the rows are in.
    (async () => {
      const rows = await fetchDecks();
      const remembered = getRememberedView();
      if (remembered.tab !== 'deckbuilder' || !remembered.deckId) return;
      if (!rows.some(d => String(d.id) === remembered.deckId)) {
        clearOpen('deckbuilder');
        return;
      }
      loadDeckDetails(Number(remembered.deckId) || remembered.deckId, rows);
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const fetchDecks = async () => {
    try {
      setLoading(true);
      const response = await fetch('/api/decks');
      if (response.ok) {
        const data = await response.json();
        setDecks(data);
        return data;
      }
    } catch (err) {
      console.error(err);
      showToast(t('deck.errLoadDecks'));
    } finally {
      setLoading(false);
    }
    return [];
  };

  const handleCreateDeck = async (e) => {
    e.preventDefault();
    if (!newDeckName.trim()) return;

    try {
      const response = await fetch('/api/decks', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ 
          name: newDeckName, 
          description: newDeckDesc, 
          format: newDeckFormat,
          category: newDeckCategory,
          accent_color: newDeckAccentColor,
          target_size: newDeckTargetSize,
          decklist_text: newDeckImportText
        })
      });

      if (response.ok) {
        showToast(t('deck.created'));
        setNewDeckName('');
        setNewDeckDesc('');
        setNewDeckFormat('Commander / EDH');
        setNewDeckCategory('Competitive');
        setNewDeckAccentColor('#eab308');
        setNewDeckTargetSize(100);
        setNewDeckImportText('');
        setShowImportDecklistArea(false);
        setShowCreateModal(false);
        fetchDecks();
      } else {
        showToast(t('deck.errCreate'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('deck.errCreateGeneric'));
    }
  };

  // sourceRows lets the boot-time restore pass the rows IT fetched: the closure
  // around loadDeckDetails at mount still sees the empty decks state, and the
  // checkout badges come from those rows.
  const loadDeckDetails = async (deckId, sourceRows = decks) => {
    try {
      setLoading(true);
      const response = await fetch(`/api/decks/${deckId}`);
      if (response.ok) {
        const data = await response.json();
        // Also get checkout status from deck list
        const deckMeta = sourceRows.find(d => d.id === deckId);
        setActiveDeck({
          ...data,
          checked_out: deckMeta?.checked_out || 0,
          checked_out_at: deckMeta?.checked_out_at || null,
          // Commander art for the header banner (same one-pass value the deck
          // grid uses; GET /api/decks/:id does not carry it).
          commander_name: data.commander_name ?? deckMeta?.commander_name ?? null,
          commander_image_url: data.commander_image_url ?? deckMeta?.commander_image_url ?? null,
          commander_card_id: data.commander_card_id ?? deckMeta?.commander_card_id ?? null,
        });
        setViewMode('detail');
        rememberOpen('deckbuilder', deckId);
      }
    } catch (err) {
      console.error(err);
      showToast(t('deck.errLoadDetails'));
    } finally {
      setLoading(false);
    }
  };

  const handleAddCardToDeck = async (card) => {
    if (!activeDeck || savingCard) return;

    // Reprints share one deck quantity. The chosen art can differ from the row
    // already in the deck, but the + button still increments the same game card.
    const existing = findSameCard(activeDeck.cards, card);
    const newQty = existing ? existing.quantity + 1 : 1;

    setSavingCard(true);
    try {
      const response = await fetch(`/api/decks/${activeDeck.id}/cards`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ card_id: card.id, quantity: newQty })
      });

      if (response.ok) {
        showToast(t('deck.addedCard', { name: displayName(card) }));
        // Refresh details locally
        await loadDeckDetails(activeDeck.id);
      } else {
        const data = await response.json().catch(() => ({}));
        showToast(data.error || 'Failed to add card.');
      }
    } catch (err) {
      console.error(err);
      showToast(t('search.errAddCard'));
    } finally {
      setSavingCard(false);
    }
  };

  const handleUpdateCardQty = async (cardId, newQty) => {
    if (!activeDeck || savingCard) return;

    // Guard against NaN/garbage from a manual quantity input before it reaches
    // the server as an invalid quantity.
    if (!Number.isFinite(newQty)) return;

    if (newQty <= 0) {
      handleRemoveCard(cardId);
      return;
    }

    // Check limits on increment
    const card = activeDeck.cards.find(c => c.id === cardId);
    if (card && newQty > card.quantity) {
      if (newQty > (card.owned_qty || 0)) {
        showToast(t('deck.errOwnedLimit', { count: card.owned_qty, name: displayName(card) }));
        return;
      }
      
      if (!isBasicLand(card) && (countsByName.get(cardKey(card)) || 0) >= 4) {
        showToast(t('deck.errCopyLimit', { count: 4, name: displayName(card) }));
        return;
      }
    }

    setSavingCard(true);
    try {
      const response = await fetch(`/api/decks/${activeDeck.id}/cards`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ card_id: cardId, quantity: newQty })
      });

      if (response.ok) {
        await loadDeckDetails(activeDeck.id);
      } else {
        showToast(t('deck.errQuantity'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('deck.errQuantity'));
    } finally {
      setSavingCard(false);
    }
  };

  const handleRemoveCard = async (cardId) => {
    if (!activeDeck) return;

    try {
      const response = await fetch(`/api/decks/${activeDeck.id}/cards/${cardId}`, {
        method: 'DELETE'
      });

      if (response.ok) {
        showToast(t('deck.cardRemoved'));
        loadDeckDetails(activeDeck.id);
      } else {
        showToast(t('deck.errRemoveCard'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('deck.errRemoveCard'));
    }
  };

  const handleDeleteDeck = async (deckId, name) => {
    if (!window.confirm(t('deck.confirmDelete', { name }))) return;

    try {
      const response = await fetch(`/api/decks/${deckId}`, {
        method: 'DELETE'
      });

      if (response.ok) {
        showToast(t('deck.deleted'));
        // A deleted deck must not linger as the remembered open one — a
        // restore of a dead id just lands back on the deck list anyway.
        clearOpen('deckbuilder');
        // Delete is reached from inside the deck editor now; the deleted deck has
        // to close itself, or the editor stays open on a row that no longer exists.
        if (activeDeck && activeDeck.id === deckId) {
          closeDeck();
        } else {
          fetchDecks();
        }
      } else {
        const body = await response.json().catch(() => ({}));
        showToast(body.error || t('deck.errDelete'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('deck.errDelete'));
    }
  };

  // Collection rows (one per owned copy) -> the search-result shape the picker
  // renders: printing id as `id`, owned_qty summed per game card.
  const ownedRowsToSearchCards = (rows) => {
    const totals = new Map();
    for (const row of rows) totals.set(cardKey(row), (totals.get(cardKey(row)) || 0) + (Number(row.quantity) || 0));
    return rows.map(row => ({ ...row, id: row.card_id || row.id, owned_qty: totals.get(cardKey(row)) || 0 }));
  };

  const handleSearchCards = async (e) => {
    if (e) e.preventDefault();
    // "Browse Collection" used to sit beside the search box and dumped every card
    // owned through GET /api/collection. It is gone: searching is the only way
    // in, so an empty box no-ops instead of flooding the pane with the library.
    if (!searchQuery.trim()) return;
    try {
      setSearching(true);
      // Scryfall syntax (t:elf c:g, otag:ramp, o:"draw a card") searches the
      // collection through the same resolver as the Collection tab; a plain
      // string stays a name search.
      const text = searchQuery.trim();
      const isSyntax = looksLikeSyntax(text);
      const response = await fetch(isSyntax
        ? `/api/search?q=${encodeURIComponent(text)}&scope=collection`
        : `/api/search?name=${encodeURIComponent(text)}&scope=collection`);
      if (response.ok) {
        let data = await response.json();
        if (isSyntax) data = ownedRowsToSearchCards(data);
        // Search returns each owned printing for art/collection display. A deck
        // picker collapses those to one logical card; owned_qty is already the
        // all-printings total on every row.
        const byCardName = new Map();
        for (const card of data) {
          const key = cardKey(card);
          if (!byCardName.has(key)) byCardName.set(key, card);
        }
        setSearchResults(Array.from(byCardName.values()));
      } else {
        showToast(t(response.status === 429 ? 'deck.errRateLimit' : 'deck.errSearch'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('deck.errSearch'));
    } finally {
      setSearching(false);
    }
  };

  // --- CHECKOUT / RETURN ---
  const handleRegisterInCollection = async () => {
    if (!canRegisterDeckInCollection(activeDeck) || registeringDeck || checkingOut) return;

    const deckId = activeDeck.id;
    const deckName = activeDeck.name;
    const cardCount = deckRegistrationCardCount(activeDeck);
    if (!window.confirm(t('deck.confirmRegisterCollection', { count: cardCount, name: deckName }))) return;

    setRegisteringDeck(true);
    try {
      const res = await fetch(`/api/decks/${deckId}/register-collection`, { method: 'POST' });
      const data = await res.json().catch(() => ({}));
      if (!res.ok) {
        showToast(data.error || t('deck.errRegisterCollection'));
        return;
      }

      showToast(t('deck.registeredCollection', { count: data.added ?? cardCount, name: deckName }));
      await loadDeckDetails(deckId);
    } catch (err) {
      console.error(err);
      showToast(t('deck.errRegisterCollection'));
    } finally {
      setRegisteringDeck(false);
    }
  };

  const handleCheckout = async (deck = null) => {
    const targetDeck = deck || activeDeck;
    if (!targetDeck) return;
    try {
      setCheckingOut(true);
      const res = await fetch(`/api/decks/${targetDeck.id}/checkout`, { method: 'PUT' });
      if (res.ok) {
        showToast(t('deck.checkedOut', { name: targetDeck.name }));
        if (activeDeck && activeDeck.id === targetDeck.id) {
          setActiveDeck(prev => ({ ...prev, checked_out: 1, checked_out_at: new Date().toISOString() }));
        }
        fetchDecks();

        const locRes = await fetch(`/api/decks/${targetDeck.id}/locations`);
        if (locRes.ok) {
          const locData = await locRes.json();
          setCheckoutLocations(locData);
          setCheckoutMode('checkout');
          setCheckoutDeckId(targetDeck.id);
          setShowCheckoutModal(true);
        }
      } else {
        const errData = await res.json().catch(() => null);
        if (errData && errData.details && errData.details.length > 0) {
          showToast(t('deck.errCheckout', { detail: errData.details[0], extra: errData.details.length > 1 ? t('deck.andMore', { count: errData.details.length - 1 }) : '' }));
        } else {
          showToast(errData?.error || 'Failed to check out deck.');
        }
      }
    } catch (err) {
      console.error(err);
      showToast(t('deck.errCheckoutGeneric'));
    } finally {
      setCheckingOut(false);
    }
  };

  const handleReturn = async (deck = null) => {
    const targetDeck = deck || activeDeck;
    if (!targetDeck) return;
    try {
      setCheckingOut(true);
      const locRes = await fetch(`/api/decks/${targetDeck.id}/locations`);
      const locData = locRes.ok ? await locRes.json() : null;
      const res = await fetch(`/api/decks/${targetDeck.id}/return`, { method: 'PUT' });
      if (res.ok) {
        showToast(t('deck.returned', { name: targetDeck.name }));
        if (activeDeck && activeDeck.id === targetDeck.id) {
          setActiveDeck(prev => ({ ...prev, checked_out: 0, checked_out_at: null }));
        }
        fetchDecks();
        if (locData) {
          setCheckoutLocations(locData);
          setCheckoutMode('checkin');
          setCheckoutDeckId(targetDeck.id);
          setShowCheckoutModal(true);
        }
      } else {
        showToast(t('deck.errReturn'));
      }
    } catch (err) {
      console.error(err);
      showToast(t('deck.errReturnGeneric'));
    } finally {
      setCheckingOut(false);
    }
  };

  // Closing the guide via X / back = cancel: revert the toggle we just committed
  // by calling the opposite endpoint. (Done button keeps the status.)
  const handleCheckoutCancel = async () => {
    const id = checkoutDeckId;
    setShowCheckoutModal(false);
    if (!id) return;
    const undo = checkoutMode === 'checkout' ? 'return' : 'checkout';
    try {
      const res = await fetch(`/api/decks/${id}/${undo}`, { method: 'PUT' });
      if (!res.ok) { showToast(t('deck.errUndo')); return; }
      if (activeDeck && activeDeck.id === id) {
        const back = checkoutMode === 'checkout';
        setActiveDeck(prev => ({ ...prev, checked_out: back ? 0 : 1, checked_out_at: back ? null : new Date().toISOString() }));
      }
      fetchDecks();
      showToast(t(checkoutMode === 'checkout' ? 'deck.checkoutCanceled' : 'deck.returnCanceled'));
    } catch (err) {
      console.error(err);
      showToast(t('deck.errUndo'));
    }
  };

  // --- DRAW SIMULATOR LOGIC ---
  const startSimulator = () => {
    if (!activeDeck || activeDeck.cards.length === 0) {
      showToast(t('deck.errEmptyDeck'));
      return;
    }

    // Expand cards into full array based on quantities
    const fullDeck = [];
    activeDeck.cards.forEach(c => {
      for (let i = 0; i < c.quantity; i++) {
        fullDeck.push({ ...c });
      }
    });

    const shuffled = shuffleArray(fullDeck);
    setSimulatorDeck(shuffled);
    setHand(shuffled.slice(0, 7));
    setMulliganCount(0);
    setShowSimulator(true);
  };

  const handleMulligan = () => {
    const shuffled = shuffleArray(simulatorDeck);
    const nextMulligan = mulliganCount + 1;
    const drawCount = Math.max(1, 7 - nextMulligan);
    setSimulatorDeck(shuffled);
    setHand(shuffled.slice(0, drawCount));
    setMulliganCount(nextMulligan);
  };

  const handleDrawCard = () => {
    // Hand grows by drawing from the rest of the deck.
    const nextIndex = hand.length;
    if (nextIndex >= simulatorDeck.length) {
      showToast(t('deck.errNoCardsLeft'));
      return;
    }
    setHand([...hand, simulatorDeck[nextIndex]]);
  };

  // --- EXPORT & IMPORT LOGIC ---
  const effectiveExportFormat = exportFormat || 'mtga';

  const handleExportDeckText = () => {
    if (!activeDeck) return '';
    return buildDeckExport(activeDeck.cards, effectiveExportFormat);
  };

  const handleCopyExportText = () => {
    const text = handleExportDeckText();
    navigator.clipboard.writeText(text)
      .then(() => showToast(t('deck.copied')))
      .catch(() => showToast(t('deck.errCopy')));
  };

  // "What's missing" compares the deck against the owned collection (shared
  // missingEntries math) and opens a panel listing the shortfall. From there
  // the user can copy the list or hand it to the Lists tab as a prefilled
  // create form — one click from "missing" to a shopping list.
  const missingRows = () => missingEntries(activeDeck?.cards || []);

  const openMissing = () => {
    if (!activeDeck) return;
    setMissingListName(t('deck.missingListDefault', { name: activeDeck.name || t('deck.untitled') }));
    setMissingOpen(true);
  };

  const closeMissing = () => setMissingOpen(false);

  const missingLines = (rows) => rows.map((r) => `${r.need - r.have} ${r.name}`).join('\n');

  const copyMissing = () => {
    const rows = missingRows();
    if (!rows.length) { showToast(t('deck.nothingToBuy')); return; }
    navigator.clipboard.writeText(missingLines(rows))
      .then(() => showToast(t('deck.missingCopied')))
      .catch(() => showToast(t('deck.errCopy')));
  };

  const saveMissingAsList = () => {
    const rows = missingRows();
    if (!rows.length) { showToast(t('deck.nothingToBuy')); return; }
    if (!onNavigate) return;
    const name = missingListName.trim() || t('deck.missingListDefault', { name: activeDeck?.name || '' });
    onNavigate('lists', { createMissing: { name, text: missingLines(rows) } });
    setMissingOpen(false);
  };

  // Copy the buylist and open TCGplayer Mass Entry — user pastes (their mass
  // entry page has no documented prefill URL param, so clipboard + open is the
  // reliable path).
  const handleOpenMassEntry = () => {
    const text = buildDeckExport(activeDeck?.cards, 'buylist');
    if (!text) { showToast(t('deck.nothingToBuy')); return; }
    navigator.clipboard.writeText(text).catch(() => {});
    window.open('https://www.tcgplayer.com/massentry?productline=Magic', '_blank', 'noopener');
    showToast(t('deck.buylistCopied'));
  };

  // --- DECK BUY MODAL (mirrors the Lists buy modal) ---
  // Deck rows arrive from the detail endpoint already carrying quantity/name,
  // so the plain list builds synchronously — same text shape as
  // GET /api/lists/:id/cardlist?style=plain (shared/cardListText.js), via the
  // frontend twin in utils/cardList.
  const openBuy = () => {
    setShowBuy(true);
    setBuyText(buildCardListText(activeDeck?.cards || [], 'plain'));
  };

  const buyLineCount = buyText ? buyText.split('\n').filter(l => l.trim()).length : 0;

  // ManaPool prefill deep link + TCGplayer prefilled Mass Entry: identical
  // mechanics to Lists.jsx — the window.open stays synchronous inside the
  // click handler (user activation doesn't survive an await), and an overlong
  // TCG link degrades to copy+paste rather than dying mid-navigation.
  const buyManapool = () => {
    const url = buildManapoolUrl(buyText.split('\n'));
    if (!url) { showToast(t('deck.buyEmpty')); return; }
    window.open(url, '_blank', 'noopener,noreferrer');
    setShowBuy(false);
  };

  const buyTcgplayer = () => {
    const url = buildTcgMassEntryUrl(buyText.split('\n'));
    if (!url) { showToast(t('deck.buyEmpty')); return; }
    const tooLong = url.length > 8000;
    if (tooLong) copyToClipboard(buyText);
    window.open(tooLong ? 'https://www.tcgplayer.com/massentry?productline=Magic' : url, '_blank', 'noopener,noreferrer');
    setShowBuy(false);
    if (tooLong) showToast(t('deck.buyTcgCopied'));
  };

  const copyBuyList = () => {
    copyToClipboard(buyText);
    showToast(t('deck.buyCopied'));
  };

  // Clipboard with a document.execCommand fallback; every flow that uses it
  // also leaves the text visible in the modal, so a denied clipboard degrades
  // the UX without breaking the purchase (same rule as Lists.jsx).
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

  const handleCompareImport = async () => {
    if (!importText.trim() || !activeDeck) return;
    setComparingImport(true);
    const lines = importText.split('\n').map(l => l.trim()).filter(Boolean);
    const results = [];

    for (const line of lines) {
      const parsed = parseDeckLine(line);
      if (!parsed) continue;
      const { qty, name: rawName } = parsed;

      try {
        const res = await fetch(`/api/search?name=${encodeURIComponent(rawName)}&scope=collection`);
        if (res.ok) {
          const cards = await res.json();
          if (cards.length > 0) {
            const card = cards[0];
            const owned = card.owned_qty || 0;
            const inDeck = countsByName.get(cardKey(card)) || 0;
            results.push({
              rawName,
              requestedQty: qty,
              ownedQty: owned,
              inDeckQty: inDeck,
              card: card,
              status: owned >= qty ? 'full' : owned > 0 ? 'partial' : 'missing'
            });
          } else {
            results.push({
              rawName,
              requestedQty: qty,
              ownedQty: 0,
              inDeckQty: 0,
              card: null,
              status: 'missing'
            });
          }
        }
      } catch (err) {
        console.error(err);
      }
    }
    setImportComparison(results);
    setComparingImport(false);
  };

  const handleImportDeck = async () => {
    if (!activeDeck) return;
    const itemsToImport = importComparison
      ? importComparison.filter(item => item.card && item.ownedQty > 0)
      : [];

    if (itemsToImport.length === 0 && !importText.trim()) return;

    let addedCount = 0;
    
    if (importComparison) {
      for (const item of itemsToImport) {
        try {
          const addQty = Math.min(item.requestedQty, item.ownedQty);
          await fetch(`/api/decks/${activeDeck.id}/cards`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ card_id: item.card.id, quantity: addQty })
          });
          addedCount++;
        } catch (err) {
          console.error(err);
        }
      }
    } else {
      const lines = importText.split('\n').map(l => l.trim()).filter(Boolean);
      for (const line of lines) {
        const parsed = parseDeckLine(line);
        if (!parsed) continue;
        const { qty, name: rawName } = parsed;

        try {
          const res = await fetch(`/api/search?name=${encodeURIComponent(rawName)}&scope=collection`);
          if (res.ok) {
            const cards = await res.json();
            if (cards.length > 0) {
              await fetch(`/api/decks/${activeDeck.id}/cards`, {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ card_id: cards[0].id, quantity: qty })
              });
              addedCount++;
            }
          }
        } catch (err) {
          console.error(err);
        }
      }
    }

    if (addedCount > 0) {
      showToast(t('deck.imported', { count: addedCount }));
      await loadDeckDetails(activeDeck.id);
      setImportText('');
      setImportComparison(null);
      setShowImportModal(false);
    } else {
      showToast(t('deck.errNoMatches'));
    }
  };

  const deckCards = activeDeck?.cards || EMPTY_DECK_CARDS;
  const deckDerived = useMemo(() => deriveDeckRenderData(deckCards), [deckCards]);
  const {
    countsByName,
    deckGroups,
    totalDeckCardsCount,
    supertypeData,
    manaCurveData,
    colorLandData,
  } = deckDerived;
  const targetDeckCardsCount = activeDeck?.target_size || 60;

  // One source of truth for which half of the component paints. Deriving it
  // from BOTH pieces of view state (instead of gating the two blocks on
  // independent conditions) makes the desync that blanked the pane unrepresentable:
  // with no deck loaded there is nothing to edit, so the list always shows, and
  // 'detail' without an active deck -- a failed fetch, a 422 unresolved-cards
  // deck, the pre-import hop from the precon modal -- can never render an empty
  // editor while the list is suppressed.
  // The editor header decides its actions from the deck's state, not from a
  // fixed button row. Every verb here moves cards across one of two boundaries:
  //   missing -> shopping list   (openMissing: copy the shortfall / make a list)
  //   deck    -> collection      (register: mint the whole deck as owned copies)
  //   collection <-> deck        (checkout/return: reserve the copies for play)
  // Only one of them is ever the primary, so the filled button always answers
  // "what do I do with this deck right now?".
  const missingCount = useMemo(() => missingEntries(deckCards).length, [deckCards]);
  const canRegister = canRegisterDeckInCollection(activeDeck);
  const isOut = !!activeDeck?.checked_out;
  const isBuilding = !isOut && missingCount > 0;
  const busy = checkingOut || registeringDeck;
  const deckStatus = isOut
    ? t('deck.statusOut')
    : (missingCount > 0
      ? t('deck.statusMissing', { count: missingCount })
      : t('deck.statusComplete'));
  const statusColor = isOut ? '#eab308' : (missingCount > 0 ? 'var(--accent-red)' : 'var(--success)');
  const btnStack = { display: 'flex', alignItems: 'center', gap: '0.4rem' };
  const quietBtn = { ...btnStack, border: '1px solid var(--border-glass)', color: 'var(--text-secondary)' };
  const moreItem = { display: 'flex', alignItems: 'center', gap: '0.5rem', width: '100%', padding: '0.5rem 0.65rem', borderRadius: '8px', background: 'none', border: 'none', color: 'var(--text-primary)', fontSize: '0.82rem', cursor: 'pointer', textAlign: 'left' };
  // Per-card value: the deck's own printing (same current_price the Value
  // panel sums), one copy. Em dash when the printing has no USD price.
  const cardPriceLabel = (card) => (card.current_price === null || card.current_price === undefined)
    ? '—'
    : priceText(Number(card.current_price), 'USD');

  const detailOpen = viewMode === 'detail' && !!activeDeck;

  // Value-section numbers for the open deck. Null-safe: the deck list renders
  // with no active deck and must not touch these. The current-printings total
  // gets the same "+" honesty rule as the cheapest floor — unpriced copies
  // make the shown number a floor, not a full price.
  const currentUnpricedCards = Number(activeDeck?.current_unpriced_cards) || 0;
  const currentValueText = activeDeck
    ? `${priceText(Number(activeDeck.current_printing_value) || 0, activeDeck.minimum_value_currency || 'USD')}${currentUnpricedCards > 0 ? '+' : ''}`
    : '';
  const currentValueHint = currentUnpricedCards === 1
    ? t('deck.valueCurrentIncompleteOne')
    : currentUnpricedCards > 0
      ? t('deck.valueCurrentIncomplete', { count: currentUnpricedCards })
      : t('deck.valueCurrentComplete');

  const PIE_COLORS = ['#3b82f6', '#10b981', '#f59e0b', '#ef4444', '#8b5cf6', '#ec4899', '#14b8a6', '#64748b'];

  const renderSourceBadge = (source) => {
    if (source !== 'precon' && source !== 'moxfield') return null;
    const isPrecon = source === 'precon';
    const Icon = isPrecon ? PackageOpen : Globe;
    return (
      <span className={`deck-source-badge deck-source-${source}`}>
        <Icon size={10} />
        {t(isPrecon ? 'deck.sourcePrecon' : 'deck.sourceMoxfield')}
      </span>
    );
  };

  // A Moxfield-mirrored deck keeps its remote public id (set by the sync
  // scheduler), so an opened deck can deep-link back to the same page on
  // moxfield.com — the exact href the Moxfield sync panel uses. Hand-made and
  // precon decks have no public id, so this renders nothing for them.
  const moxfieldDeckUrl = (deck) =>
    deck && deck.source === 'moxfield' && deck.moxfield_public_id
      ? `https://moxfield.com/decks/${encodeURIComponent(deck.moxfield_public_id)}`
      : null;

  // --- SELECTION MENU METRICS & FILTERING ---
  const filteredDecks = decks.filter(deck => {
    const q = deckSearchTerm.trim().toLowerCase();
    const matchesSearch = !q ||
      deck.name.toLowerCase().includes(q) ||
      (deck.description && deck.description.toLowerCase().includes(q));

    let matchesStatus = true;
    if (deckStatusFilter === 'ready') matchesStatus = deck.total_cards === (deck.target_size || 60);
    else if (deckStatusFilter === 'in_progress') matchesStatus = (deck.total_cards || 0) < (deck.target_size || 60);
    else if (deckStatusFilter === 'in_play') matchesStatus = !!deck.checked_out;

    return matchesSearch && matchesStatus;
  }).sort((a, b) => {
    if (deckSortBy === 'name_asc') return a.name.localeCompare(b.name);
    if (deckSortBy === 'cards_desc') return (b.total_cards || 0) - (a.total_cards || 0);
    if (deckSortBy === 'created_asc') return new Date(a.created_at) - new Date(b.created_at);
    return new Date(b.created_at) - new Date(a.created_at);
  });

  return (
    <div style={{ maxWidth: '1200px', margin: '0 auto', display: 'flex', flexDirection: 'column', gap: '1.5rem' }}>
      
      {/* 1. SELECTION MENU VIEW OF ALL DECKS */}
      {!detailOpen && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: '1.25rem' }}>
          
          {/* Top Banner Header & Primary Action */}
          <div className="glass-panel deck-vault-header" style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', flexWrap: 'nowrap', gap: '1rem', padding: '1.25rem 1.5rem', background: 'linear-gradient(135deg, rgba(30, 41, 59, 0.7), rgba(15, 23, 42, 0.8))', border: '1px solid rgba(255, 255, 255, 0.1)' }}>
            <div style={{ minWidth: 0 }}>
              <h2 style={{ fontSize: '1.4rem', color: 'var(--text-strong)', fontWeight: 800, display: 'flex', alignItems: 'center', gap: '0.5rem', margin: 0 }}>
                <Layers size={22} style={{ color: 'var(--accent-yellow)' }} />
                {t('deck.vaultTitle')}
              </h2>
              <p className="deck-vault-counts" style={{ color: 'var(--text-muted)', fontSize: '0.8rem', marginTop: '0.2rem' }}>
                {t('deck.vaultCounts', { count: decks.length, out: decks.filter(d => d.checked_out).length })}
              </p>
            </div>
            <div style={{ display: 'flex', gap: '0.6rem', flexShrink: 0, marginLeft: 'auto' }}>
              <button 
                ref={addDeckButtonRef}
                className="btn btn-primary" 
                onClick={() => setShowAddDeckModal(true)}
                style={{ padding: '0.6rem 1.25rem', fontSize: '0.9rem', fontWeight: 700, display: 'flex', alignItems: 'center', gap: '0.5rem', boxShadow: '0 4px 14px rgba(234, 179, 8, 0.25)' }}
              >
                <Plus size={18} /> {t('deck.addDeck')}
              </button>
            </div>
          </div>

          {/* Search, Filters, Sorting & View Toolbar */}
          <div className="glass-panel" style={{ display: 'flex', flexDirection: 'column', gap: '0.75rem', padding: '1rem 1.25rem' }}>
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: '0.75rem' }}>
              
              {/* Search input */}
              <div style={{ position: 'relative', flex: '1 1 240px', minWidth: '220px' }}>
                <Search size={16} style={{ position: 'absolute', left: '0.75rem', top: '50%', transform: 'translateY(-50%)', color: 'var(--text-muted)' }} />
                <input
                  type="text"
                  className="input-control"
                  placeholder={t('deck.filterPlaceholder')}
                  value={deckSearchTerm}
                  onChange={e => setDeckSearchTerm(e.target.value)}
                  style={{ paddingLeft: '2.25rem', width: '100%', fontSize: '0.85rem' }}
                />
                {deckSearchTerm && (
                  <button
                    className="btn btn-secondary btn-icon-only"
                    onClick={() => setDeckSearchTerm('')}
                    style={{ position: 'absolute', right: '0.4rem', top: '50%', transform: 'translateY(-50%)', width: '20px', height: '20px', padding: 0, fontSize: '0.7rem' }}
                  >
                    <X size={12} />
                  </button>
                )}
              </div>

            </div>

            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: '0.75rem', paddingTop: '0.5rem', borderTop: '1px solid var(--border-glass)' }}>
              
              <div style={{ display: 'flex', alignItems: 'center', gap: '0.75rem', flexWrap: 'wrap' }}>
                {/* Status Filter */}
                <div style={{ display: 'flex', alignItems: 'center', gap: '0.4rem' }}>
                  <Filter size={14} style={{ color: 'var(--text-muted)' }} />
                  <select
                    className="select-control"
                    value={deckStatusFilter}
                    onChange={e => setDeckStatusFilter(e.target.value)}
                    style={{ padding: '0.3rem 0.6rem', fontSize: '0.75rem', height: 'auto' }}
                  >
                    <option value="all">{t('deck.allStatuses')}</option>
                    <option value="ready">{t('deck.statusBattleReady')}</option>
                    <option value="in_progress">{t('deck.statusBuildingCount')}</option>
                    <option value="in_play">{t('deck.statusInPlayEmoji')}</option>
                  </select>
                </div>

                {/* Sort Order */}
                <div style={{ display: 'flex', alignItems: 'center', gap: '0.4rem' }}>
                  <SlidersHorizontal size={14} style={{ color: 'var(--text-muted)' }} />
                  <select
                    className="select-control"
                    value={deckSortBy}
                    onChange={e => setDeckSortBy(e.target.value)}
                    style={{ padding: '0.3rem 0.6rem', fontSize: '0.75rem', height: 'auto' }}
                  >
                    <option value="created_desc">{t('deck.sortNewest')}</option>
                    <option value="created_asc">{t('deck.sortOldest')}</option>
                    <option value="name_asc">{t('collection.sort.name-asc')}</option>
                    <option value="cards_desc">{t('deck.sortMostCards')}</option>
                  </select>
                </div>
              </div>

              {/* View Mode Toggle: Grid vs Table */}
              <div style={{ display: 'flex', alignItems: 'center', gap: '3px', background: 'rgba(0,0,0,0.3)', padding: '2px', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border-glass)' }}>
                <button
                  type="button"
                  className={`btn ${deckSelectionViewMode === 'grid' ? 'btn-primary' : 'btn-secondary'}`}
                  style={{ padding: '0.25rem 0.55rem', fontSize: '0.75rem', display: 'flex', alignItems: 'center', gap: '4px' }}
                  onClick={() => setDeckSelectionViewMode('grid')}
                  title={t('deck.gridView')}
                >
                  <LayoutGrid size={13} /> Grid
                </button>
                <button
                  type="button"
                  className={`btn ${deckSelectionViewMode === 'table' ? 'btn-primary' : 'btn-secondary'}`}
                  style={{ padding: '0.25rem 0.55rem', fontSize: '0.75rem', display: 'flex', alignItems: 'center', gap: '4px' }}
                  onClick={() => setDeckSelectionViewMode('table')}
                  title={t('deck.tableView')}
                >
                  <List size={13} /> Table
                </button>
              </div>

            </div>
          </div>

          {/* Decks Display Section */}
          {loading ? (
            <div className="spinner" style={{ margin: '3rem auto' }}></div>
          ) : filteredDecks.length === 0 ? (
            <div className="glass-panel" style={{ textAlign: 'center', padding: '3.5rem 1.5rem', color: 'var(--text-secondary)' }}>
              <Layers size={36} style={{ color: 'var(--text-muted)', marginBottom: '0.75rem', opacity: 0.5 }} />
              <h3 style={{ color: 'var(--text-strong)', fontSize: '1.05rem', marginBottom: '0.25rem' }}>{t('deck.noMatches')}</h3>
              <p style={{ fontSize: '0.85rem' }}>{t('deck.noMatchesHint')}</p>
              {(deckSearchTerm || deckStatusFilter !== 'all') && (
                <button
                  className="btn btn-secondary"
                  style={{ marginTop: '1rem', fontSize: '0.8rem' }}
                  onClick={() => { setDeckSearchTerm(''); setDeckStatusFilter('all'); }}
                >
                  {t('deck.clearFilters')}
                </button>
              )}
            </div>
          ) : deckSelectionViewMode === 'grid' ? (
            /* --- GRID VIEW --- */
            <div className="deck-tiles" style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(320px, 1fr))', gap: '1.25rem' }}>
              {filteredDecks.map(deck => {
                const targetSize = deck.target_size || 100;
                const totalCards = deck.total_cards || 0;
                const isComplete = totalCards >= targetSize;
                const percent = Math.min(100, Math.round((totalCards / targetSize) * 100));
                const accentColor = deck.accent_color || '#ef4444';
                // ManaBox-style: the commander's art is the tile. Without a
                // commander the card keeps the old accent-gradient look.
                const commanderArt = deck.commander_image_url || null;
                // ManaBox tiles crop to the illustration; Scryfall serves the same
                // crop at art_crop/, falling to the full face if a print lacks it.
                const commanderCrop = commanderArt
                  ? commanderArt.replace('/normal/front/', '/art_crop/front/') : null;

                return (
                  <div
                    key={deck.id}
                    className={`glass-panel deck-tile${commanderArt ? ' has-art' : ''}${deck.checked_out ? ' is-out' : ''}`}
                    style={{
                      display: 'flex',
                      flexDirection: 'column',
                      justifyContent: 'space-between',
                      gap: '1rem',
                      padding: commanderArt ? 0 : '1.25rem',
                      border: deck.checked_out
                        ? '1px solid rgba(234,179,8,0.5)'
                        : `1px solid ${accentColor}40`,
                      position: 'relative',
                      overflow: 'hidden',
                      cursor: 'pointer',
                      transition: 'all 0.2s cubic-bezier(0.16, 1, 0.3, 1)',
                      background: 'linear-gradient(145deg, rgba(211,32,42,0.06), rgba(15,23,42,0.65))'
                    }}
                    onClick={() => loadDeckDetails(deck.id)}
                    onMouseEnter={e => {
                      e.currentTarget.style.transform = 'translateY(-3px)';
                      e.currentTarget.style.boxShadow = `0 12px 30px ${accentColor}25`;
                    }}
                    onMouseLeave={e => {
                      e.currentTarget.style.transform = 'none';
                      e.currentTarget.style.boxShadow = 'none';
                    }}
                  >
                    {/* Top Accent Line */}
                    <div className="deck-tile-accent" style={{
                      position: 'absolute', top: 0, left: 0, right: 0, height: '3px',
                      background: deck.checked_out
                        ? 'linear-gradient(90deg, #eab308, #f59e0b)'
                        : `linear-gradient(90deg, ${accentColor}, ${accentColor}cc)`
                    }} />

                    {/* Commander Art Banner */}
                    {commanderArt ? (
                      <div className="deck-tile-art" style={{ position: 'relative', marginTop: '3px' }}>
                        <CardImage
                          // card-shaped object so CardImage resolves contributed
                          // art from the cache id before falling to the URL.
                          card={{ id: deck.commander_card_id, name: deck.commander_name }}
                          src={commanderCrop || commanderArt}
                          fallbackSrc={commanderCrop && commanderCrop !== commanderArt ? commanderArt : undefined}
                          loading="lazy"
                          decoding="async"
                          style={{ width: '100%', height: '170px', display: 'block', objectFit: 'cover' }}
                        />
                        <div className="deck-tile-scrim" style={{
                          position: 'absolute', top: 0, left: 0, right: 0, bottom: 0,
                          background: 'linear-gradient(to bottom, rgba(15,23,42,0.35) 0%, rgba(15,23,42,0) 35%, rgba(15,23,42,0.85) 100%)',
                          pointerEvents: 'none'
                        }} />
                        <h3 className="deck-tile-name" style={{
                          position: 'absolute',
                          left: '1rem',
                          right: '1rem',
                          bottom: '0.6rem',
                          margin: 0,
                          color: '#fff',
                          fontSize: '1.15rem',
                          fontWeight: 800,
                          letterSpacing: '-0.01em',
                          textShadow: '0 1px 10px rgba(0,0,0,0.8)'
                        }}>
                          {deck.name}
                        </h3>
                      </div>
                    ) : null}

                    <div className="deck-tile-body" style={commanderArt
                      ? { display: 'flex', flexDirection: 'column', gap: '1rem', justifyContent: 'space-between', flex: 1, padding: '0 1.25rem 1.25rem' }
                      : { display: 'contents' }}>
                    {/* In Play Banner */}
                    {deck.checked_out ? (
                      <div style={{
                        marginTop: '4px',
                        background: 'linear-gradient(90deg, rgba(234,179,8,0.9), rgba(245,158,11,0.85))',
                        padding: '4px 10px',
                        borderRadius: '4px',
                        display: 'flex',
                        alignItems: 'center',
                        gap: '6px',
                        fontSize: '0.65rem',
                        fontWeight: 800,
                        color: '#000',
                        letterSpacing: '0.06em',
                        textTransform: 'uppercase'
                      }}>
                        <Gamepad2 size={12} />
                        <span>{t('deck.inPlay')}</span>
                      </div>
                    ) : null}

                    <div>
                      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', gap: '0.5rem' }}>
                        <div style={{ display: 'flex', flexDirection: 'column', gap: '0.3rem' }}>
                          <div style={{ display: 'flex', alignItems: 'center', gap: '0.4rem', flexWrap: 'wrap' }}>
                            {/* With an art banner the name lives on the scrim;
                                rendering it twice reads as a label, not a title. */}
                            {!commanderArt && (
                              <h3 style={{ color: 'var(--text-strong)', fontSize: '1.15rem', fontWeight: 800, margin: 0, letterSpacing: '-0.01em' }}>
                                {deck.name}
                              </h3>
                            )}

                            {deck.format && (
                              <span style={{
                                fontSize: '0.6rem',
                                fontWeight: 700,
                                padding: '0.1rem 0.4rem',
                                borderRadius: '4px',
                                background: 'rgba(255,255,255,0.06)',
                                color: 'var(--text-secondary)',
                                border: '1px solid var(--border-glass)'
                              }}>
                                {deck.format}
                              </span>
                            )}

                            {deck.category && deck.source !== 'precon' && (
                              <span style={{
                                fontSize: '0.6rem',
                                fontWeight: 700,
                                padding: '0.1rem 0.4rem',
                                borderRadius: '4px',
                                background: 'rgba(59, 130, 246, 0.12)',
                                color: '#60a5fa',
                                border: '1px solid rgba(59, 130, 246, 0.25)'
                              }}>
                                {deck.category}
                              </span>
                            )}
                            {renderSourceBadge(deck.source)}
                          </div>
                        </div>

                        {/* Status Badge: only Building (incomplete) or Built (checked out). */}
                        {(deck.checked_out || !isComplete) && (
                        <span className={`deck-tile-status ${deck.checked_out ? 'ready' : 'building'}`} style={{
                          fontSize: '0.7rem',
                          fontWeight: 700,
                          padding: '0.2rem 0.5rem',
                          borderRadius: '12px',
                          backgroundColor: deck.checked_out ? 'rgba(74, 222, 128, 0.15)' : 'rgba(59, 130, 246, 0.15)',
                          color: deck.checked_out ? '#4ade80' : '#60a5fa',
                          border: deck.checked_out ? '1px solid rgba(74, 222, 128, 0.3)' : '1px solid rgba(59, 130, 246, 0.3)',
                          whiteSpace: 'nowrap'
                        }}>
                          {t(deck.checked_out ? 'deck.statusBuilt' : 'deck.statusBuilding')}
                        </span>
                        )}
                      </div>

                      <p className="deck-tile-desc" style={{ color: 'var(--text-secondary)', fontSize: '0.82rem', marginTop: '0.6rem', minHeight: '34px', lineHeight: '1.4' }}>
                        {deck.description || ''}
                      </p>
                    </div>

                    {/* Progress Bar & Details */}
                    <div className="deck-tile-stats" style={{ display: 'flex', flexDirection: 'column', gap: '0.4rem', background: 'rgba(0,0,0,0.2)', padding: '0.6rem 0.75rem', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border-glass)' }}>
                      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', fontSize: '0.75rem' }}>
                        <span style={{ color: 'var(--text-secondary)', fontWeight: 600 }}>{t('deck.cardCapacity')}</span>
                        <span style={{ color: isComplete ? '#4ade80' : 'var(--text-strong)', fontWeight: 700 }}>
                          {totalCards} / {targetSize} Cards ({percent}%)
                        </span>
                      </div>
                      {Number(deck.missing_card_types) > 0 ? (
                        <div className="deck-tile-missing" style={{ fontSize: '0.72rem', fontWeight: 750, color: '#f87171' }}>
                          {t('deck.statusMissing', { count: Number(deck.missing_card_types) })}
                        </div>
                      ) : totalCards > 0 ? (
                        <div className="deck-tile-owned" style={{ fontSize: '0.72rem', fontWeight: 750, color: 'var(--success)' }}>
                          {t('deck.allOwned')}
                        </div>
                      ) : null}
                      <div className="deck-tile-prog" style={{ width: '100%', height: '6px', background: 'rgba(255,255,255,0.08)', borderRadius: '3px', overflow: 'hidden' }}>
                        <div className={`deck-tile-prog-fill${isComplete ? ' complete' : ''}`} style={{
                          height: '100%',
                          width: `${percent}%`,
                          background: isComplete
                            ? 'linear-gradient(90deg, #4ade80, #22c55e)'
                            : 'linear-gradient(90deg, #facc15, #eab308)',
                          borderRadius: '3px',
                          transition: 'width 0.3s ease'
                        }} />
                      </div>
                      <div
                        title={deckMinimumValueHint(deck, t)}
                        tabIndex={0}
                        aria-label={`${t('deck.minimumValue')}: ${deckMinimumValueText(deck)}. ${deckMinimumValueHint(deck, t)}`}
                        style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', paddingTop: '0.35rem', borderTop: '1px solid var(--border-glass)', fontSize: '0.75rem' }}
                      >
                        <span style={{ color: 'var(--text-secondary)', fontWeight: 600, display: 'inline-flex', alignItems: 'center', gap: '0.25rem' }}>
                          <DollarSign size={12} /> {t('deck.minimumValue')}
                        </span>
                        <span style={{ color: 'var(--text-secondary)', fontWeight: 700, display: 'flex', flexDirection: 'column', alignItems: 'flex-end' }}>
                          {deckMinimumValueText(deck)}
                          {Number(deck.unpriced_cards) > 0 && (
                            <small style={{ color: 'var(--text-muted)', fontWeight: 600 }}>{deckUnpricedCountText(deck, t)}</small>
                          )}
                        </span>
                      </div>
                    </div>

                    </div>

                  </div>
                );
              })}
            </div>
          ) : (
            /* --- TABLE VIEW --- */
            <div className="glass-panel" style={{ overflowX: 'auto', padding: 0 }}>
              <table style={{ width: '100%', borderCollapse: 'collapse', textAlign: 'left', fontSize: '0.85rem' }}>
                <thead>
                  <tr style={{ borderBottom: '1px solid var(--border-glass)', background: 'rgba(0,0,0,0.2)', color: 'var(--text-secondary)', fontSize: '0.75rem', textTransform: 'uppercase', letterSpacing: '0.04em' }}>
                    {/* No Format column at all (Brent, 2026-09-17): the deck list is
                        not a format browser. The name cell carries the accent swatch.
                        Consequence, stated straight: the table now shows a deck's format
                        nowhere, and the editor header does not show it either. The Grid
                        view's cards still badge their format, so Table users can switch
                        views to read it. */}
                    <th style={{ padding: '0.75rem 1rem' }}>{t('deck.colNameDesc')}</th>
                    <th style={{ padding: '0.75rem 1rem' }}>{t('deck.colCapacity')}</th>
                    <th style={{ padding: '0.75rem 1rem' }}>{t('deck.minimumValue')}</th>
                    <th style={{ padding: '0.75rem 1rem' }}>{t('admin.colStatus')}</th>
                    <th style={{ padding: '0.75rem 1rem' }}>{t('admin.colCreated')}</th>
                    {/* No Actions column: a row is clicked, not operated on. Checkout /
                        Return / Delete live in the deck editor you land in by clicking. */}
                  </tr>
                </thead>
                <tbody>
                  {filteredDecks.map(deck => {
                    const targetSize = deck.target_size || 60;
                    const totalCards = deck.total_cards || 0;
                    const isComplete = totalCards >= targetSize;
                    const percent = Math.min(100, Math.round((totalCards / targetSize) * 100));
                    const accentColor = deck.accent_color || '#ef4444';

                    return (
                      <tr
                        key={deck.id}
                        style={{ borderBottom: '1px solid var(--border-glass)', cursor: 'pointer', transition: 'background 0.15s' }}
                        onClick={() => loadDeckDetails(deck.id)}
                        onMouseEnter={e => e.currentTarget.style.background = 'rgba(255,255,255,0.03)'}
                        onMouseLeave={e => e.currentTarget.style.background = 'transparent'}
                      >
                        <td style={{ padding: '0.75rem 1rem' }}>
                          <div style={{ display: 'flex', alignItems: 'center', gap: '6px', flexWrap: 'wrap' }}>
                            {/* The accent swatch survives here: it is the deck's colour,
                                not its format, and the table would otherwise have no
                                trace of accent_color at all. */}
                            <span style={{ width: '8px', height: '8px', borderRadius: '50%', backgroundColor: accentColor, display: 'inline-block', flexShrink: 0 }} />
                            <span style={{ fontWeight: 700, color: 'var(--text-strong)' }}>{deck.name}</span>
                            {renderSourceBadge(deck.source)}
                            {/* A precon is a printed product, not a play-style build: its
                                category tag never renders, even for rows imported before
                                the import stopped stamping one. */}
                            {deck.category && deck.source !== 'precon' && (
                              <span style={{ fontSize: '0.6rem', fontWeight: 700, padding: '1px 6px', borderRadius: '4px', background: 'rgba(59,130,246,0.12)', color: '#60a5fa', border: '1px solid rgba(59,130,246,0.25)' }}>
                                {deck.category}
                              </span>
                            )}
                          </div>
                          <div style={{ fontSize: '0.75rem', color: 'var(--text-secondary)', marginTop: '2px' }}>
                            {deck.description || 'No description'}
                          </div>
                        </td>
                        <td style={{ padding: '0.75rem 1rem', width: '160px' }}>
                          <div style={{ display: 'flex', flexDirection: 'column', gap: '3px' }}>
                            <div style={{ fontSize: '0.75rem', fontWeight: 700, color: isComplete ? '#4ade80' : 'var(--text-strong)' }}>
                              {totalCards} / {targetSize} Cards
                            </div>
                            <div style={{ width: '100%', height: '4px', background: 'rgba(255,255,255,0.1)', borderRadius: '2px', overflow: 'hidden' }}>
                              <div style={{ height: '100%', width: `${percent}%`, background: isComplete ? '#4ade80' : '#3b82f6' }} />
                            </div>
                          </div>
                        </td>
                        <td
                          title={deckMinimumValueHint(deck, t)}
                          tabIndex={0}
                          aria-label={`${t('deck.minimumValue')}: ${deckMinimumValueText(deck)}. ${deckMinimumValueHint(deck, t)}`}
                          style={{ padding: '0.75rem 1rem', color: 'var(--accent-yellow)', fontWeight: 800, whiteSpace: 'nowrap' }}
                        >
                          <div>{deckMinimumValueText(deck)}</div>
                          {Number(deck.unpriced_cards) > 0 && (
                            <small style={{ color: 'var(--text-muted)', fontWeight: 600 }}>{deckUnpricedCountText(deck, t)}</small>
                          )}
                        </td>
                        <td style={{ padding: '0.75rem 1rem' }}>
                          {deck.checked_out ? (
                            <span style={{ fontSize: '0.7rem', fontWeight: 800, padding: '2px 8px', borderRadius: '10px', background: 'rgba(234,179,8,0.15)', color: '#eab308', border: '1px solid rgba(234,179,8,0.4)', display: 'inline-flex', alignItems: 'center', gap: '4px' }}>
                              <Gamepad2 size={11} /> {t('deck.inPlay')}
                            </span>
                          ) : isComplete ? (
                            <span style={{ fontSize: '0.7rem', fontWeight: 800, padding: '2px 8px', borderRadius: '10px', background: 'rgba(74, 222, 128, 0.15)', color: '#4ade80', border: '1px solid rgba(74, 222, 128, 0.3)' }}>
                              {t('deck.statusReady')}
                            </span>
                          ) : (
                            <span style={{ fontSize: '0.7rem', fontWeight: 800, padding: '2px 8px', borderRadius: '10px', background: 'rgba(255,255,255,0.05)', color: 'var(--text-secondary)', border: '1px solid var(--border-glass)' }}>
                              {t('deck.statusBuilding')}
                            </span>
                          )}
                        </td>
                        <td style={{ padding: '0.75rem 1rem', fontSize: '0.75rem', color: 'var(--text-muted)' }}>
                          {new Date(deck.created_at).toLocaleDateString()}
                        </td>
                        {/* Actions column removed by design: the row itself opens the
                            deck, and checkout/return/delete live in the deck editor. */}
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}

        </div>
      )}

      {/* 2. DECK EDITOR / DETAIL VIEW */}
      {viewMode === 'detail' && activeDeck && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: '1.5rem' }}>
          {/* Header */}
          <div className={`glass-panel deck-editor-header${activeDeck.commander_image_url ? ' has-art' : ''}`} style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', flexWrap: 'wrap', gap: '1rem', position: 'relative', overflow: 'visible' }}>
            {/* Commander art banner. Hidden by default (index.css); themes that
                want a hero header (ManaBox) reveal it. Decorative only. */}
            {activeDeck.commander_image_url && (
              <div className="deck-editor-header-art" aria-hidden="true">
                <CardImage
                  card={{ id: activeDeck.commander_card_id, name: activeDeck.commander_name }}
                  src={activeDeck.commander_image_url.replace('/normal/front/', '/art_crop/front/')}
                  fallbackSrc={activeDeck.commander_image_url}
                  loading="lazy"
                  decoding="async"
                  alt=""
                />
              </div>
            )}

            {/* Checked out banner */}
            {activeDeck.checked_out ? (
              <div style={{
                position: 'absolute',
                top: 0, left: 0, right: 0,
                height: '4px',
                background: 'linear-gradient(90deg, #eab308, #f59e0b, #eab308)',
                borderRadius: 'var(--radius-md) var(--radius-md) 0 0',
                backgroundSize: '200% auto',
                animation: 'shimmer-gold 2s linear infinite'
              }} />
            ) : null}

            <div className="deck-editor-header-id" style={{ display: 'flex', alignItems: 'flex-start', gap: '0.75rem', minWidth: 0 }}>
              <button className="btn btn-secondary btn-icon-only" onClick={closeDeck} aria-label={t('deck.backToDecks')} style={{ borderRadius: '50%', flex: 'none' }}>
                <ChevronLeft size={16} />
              </button>
              <div style={{ minWidth: 0 }}>
                <h2 style={{ fontSize: '1.2rem', color: 'var(--text-strong)', display: 'flex', alignItems: 'center', gap: '8px', flexWrap: 'wrap', margin: 0 }}>
                  {activeDeck.name}
                  {renderSourceBadge(activeDeck.source)}
                  <span style={{ fontSize: '0.78rem', color: totalDeckCardsCount === targetDeckCardsCount ? 'var(--success)' : 'var(--accent-yellow)', fontWeight: 600 }}>
                    ({totalDeckCardsCount}/{targetDeckCardsCount} cards)
                  </span>
                  <span
                    title={deckMinimumValueHint(activeDeck, t)}
                    tabIndex={0}
                    aria-label={`${t('deck.minimumValue')}: ${deckMinimumValueText(activeDeck)}. ${deckMinimumValueHint(activeDeck, t)}`}
                    style={{ fontSize: '0.75rem', color: 'var(--accent-yellow)', fontWeight: 800, display: 'inline-flex', alignItems: 'center', gap: '2px' }}
                  >
                    <DollarSign size={13} /> {t('deck.minimumValue')}: {deckMinimumValueText(activeDeck)}
                    {Number(activeDeck.unpriced_cards) > 0 && (
                      <small style={{ color: 'var(--text-muted)', fontWeight: 600 }}>({deckUnpricedCountText(activeDeck, t)})</small>
                    )}
                  </span>
                </h2>
                <p style={{ display: 'flex', alignItems: 'center', gap: '6px', fontSize: '0.82rem', margin: '4px 0 0', color: 'var(--text-secondary)', flexWrap: 'wrap', minWidth: 0 }}>
                  <span aria-hidden={'true'} style={{ width: '7px', height: '7px', borderRadius: '50%', background: statusColor, flex: 'none' }} />
                  <span style={{ color: statusColor, fontWeight: 650 }}>{deckStatus}</span>
                  {!!activeDeck.description && (
                    <span style={{ color: 'var(--text-muted)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', flex: '1 1 auto', minWidth: 0 }}>
                      {activeDeck.description}
                    </span>
                  )}
                  {!!isOut && activeDeck.checked_out_at && (
                    <span style={{ color: 'var(--text-muted)', flex: 'none' }}>
                      {t('deck.checkedOutSince', { when: new Date(activeDeck.checked_out_at).toLocaleString() })}
                    </span>
                  )}
                </p>
              </div>

            {/* The deck's state picks the one filled verb; the other card-movement
                verbs stay quiet outlines so the row never reshuffles under a click.
                Register is hidden while a deck is out because the endpoint refuses it
                then. Housekeeping sits behind one overflow so only the three verbs
                that move cards across a boundary compete for attention. */}
            <div className="deck-editor-header-actions" style={{ display: 'flex', gap: '0.5rem', alignItems: 'center', position: 'relative', flexWrap: 'wrap', justifyContent: 'flex-end', flex: '0 1 auto', minWidth: 0 }}>
              {!isOut && isBuilding && (
                <button
                  className="btn btn-primary"
                  onClick={openMissing}
                  disabled={busy}
                  title={t('deck.missingHint')}
                  style={btnStack}
                >
                  <ClipboardList size={14} /> {t('deck.exportMissingCount', { count: missingCount })}
                </button>
              )}
              {!isOut && (
                <button
                  className={'btn ' + (isBuilding ? 'btn-secondary' : 'btn-primary')}
                  onClick={() => handleCheckout(activeDeck)}
                  disabled={busy}
                  title={t('deck.checkoutHint')}
                  style={isBuilding ? quietBtn : btnStack}
                >
                  <LogOut size={14} /> {t('deck.checkoutAction')}
                </button>
              )}
              {isOut && (
                <button
                  className="btn btn-primary"
                  onClick={() => handleReturn(activeDeck)}
                  disabled={busy}
                  title={t('deck.returnHint')}
                  style={btnStack}
                >
                  <PackageCheck size={14} /> {t('deck.returnAction')}
                </button>
              )}
              {!isOut && canRegister && (
                <button
                  className="btn btn-secondary"
                  onClick={handleRegisterInCollection}
                  disabled={busy}
                  title={t('deck.registerCollectionHint')}
                  style={{ ...btnStack, color: '#4ade80', borderColor: 'rgba(74, 222, 128, 0.38)' }}
                >
                  <PackagePlus size={14} /> {registeringDeck ? t('deck.registeringCollection') : t('deck.registerCollection')}
                </button>
              )}
              <OverflowMenu label={t('deck.moreActions')}>
                <button role="menuitem" style={moreItem} onClick={() => { startSimulator(); }}>
                  <Play size={14} /> {t('deck.drawSimulator')}
                </button>
                <div style={{ height: '1px', background: 'var(--border-glass)', margin: '0.25rem 0.35rem' }} />
                <button role="menuitem" style={moreItem} onClick={() => setShowExportModal(true)}>
                  <Download size={14} /> {t('deck.exportDeckList')}
                </button>
                <button role="menuitem" style={moreItem} onClick={() => setShowImportModal(true)}>
                  <Upload size={14} /> {t('deck.importDeckList')}
                </button>
                {moxfieldDeckUrl(activeDeck) && (
                  <a role="menuitem" style={{ ...moreItem, textDecoration: 'none' }}
                    href={moxfieldDeckUrl(activeDeck)} target="_blank" rel="noreferrer">
                    <ExternalLink size={14} /> {t('mfx.openOnMoxfield')}
                  </a>
                )}
                <div style={{ height: '1px', background: 'var(--border-glass)', margin: '0.25rem 0.35rem' }} />
                <button
                  role="menuitem"
                  style={{ ...moreItem, color: 'var(--accent-red)', cursor: isOut ? 'not-allowed' : 'pointer', opacity: isOut ? 0.45 : 1 }}
                  disabled={isOut}
                  title={isOut ? t('deck.deleteBlockedWhileOut') : t('deck.deleteDeckHint')}
                  onClick={() => handleDeleteDeck(activeDeck.id, activeDeck.name)}
                >
                  <Trash2 size={14} /> {t('deck.deleteDeck')}
                </button>
              </OverflowMenu>
            </div>
          </div>
        </div>

          {/* Checked out info banner */}
          {!!activeDeck.checked_out && (
            <div style={{
              background: 'rgba(234,179,8,0.06)',
              border: '1px solid rgba(234,179,8,0.25)',
              borderRadius: 'var(--radius-md)',
              padding: '0.85rem 1.25rem',
              display: 'flex',
              alignItems: 'center',
              gap: '0.75rem',
              fontSize: '0.85rem',
              color: '#eab308'
            }}>
              <span style={{ fontSize: '1.25rem' }}>🎮</span>
              <div>
                <strong>{t('deck.checkedOutBanner')}</strong>
                <div style={{ fontSize: '0.75rem', color: 'var(--text-secondary)', marginTop: '2px' }}>
                  {t('deck.checkedOutHint')}
                </div>
              </div>
            </div>
          )}

          <div className="deck-editor-tabs" role="tablist">
            {[['cards', t('deck.tabCards')], ['add', t('deck.tabAdd')], ['stats', t('deck.tabStats')]].map(([k, label]) => (
              <button key={k} role="tab" type="button" aria-selected={editorTab === k}
                className={`deck-editor-tab${editorTab === k ? ' active' : ''}`}
                onClick={() => setEditorTab(k)}>{label}</button>
            ))}
          </div>

          <div className="deck-editor-body" data-tab={editorTab} style={{ display: 'grid', gridTemplateColumns: '1fr', gap: '1.5rem', alignItems: 'start' }}>
            <div style={{ display: 'flex', flexDirection: 'row', flexWrap: 'wrap', gap: '1.5rem' }}>
              
              {/* Left Column: Deck Card List */}
              <div className="deck-editor-main" style={{ flex: '2 1 500px', display: 'flex', flexDirection: 'column', gap: '1.5rem' }}>
                
                {/* Search & Quick Add to Deck */}
                <div className="glass-panel deck-pane-add">
                  <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '0.75rem', gap: '0.5rem', flexWrap: 'wrap' }}>
                    <h3 style={{ fontSize: '0.95rem', color: 'var(--text-strong)', margin: 0 }}>{t('deck.addCardsTitle')}</h3>
                  </div>
                  <form onSubmit={handleSearchCards} style={{ display: 'flex', gap: '0.5rem' }}>
                    <input
                      type="text"
                      className="input-control"
                      placeholder={t('deck.searchPlaceholder')}
                      value={searchQuery}
                      onChange={(e) => setSearchQuery(e.target.value)}
                      style={{ flex: 1 }}
                    />
                    <button type="submit" className="btn btn-primary" style={{ padding: '0.5rem 1rem' }} title={t('shared.search')}>
                      <Search size={16} />
                    </button>
                  </form>

                  {/* Search Results list */}
                  {searching ? (
                    <div className="spinner" style={{ margin: '1rem auto' }}></div>
                  ) : searchResults.length > 0 && (
                    <div style={{ display: 'flex', flexDirection: 'column', gap: '0.4rem', marginTop: '1rem', maxHeight: '240px', overflowY: 'auto', background: 'rgba(0,0,0,0.15)', padding: '0.5rem', borderRadius: 'var(--radius-sm)' }}>
                      {searchResults.map(card => {
                          const qtyInDeck = countsByName.get(cardKey(card)) || 0;
                          const ownedQty = card.owned_qty || 0;
                          const isAtMaxOwned = qtyInDeck >= ownedQty;
                          const isAtRuleMax = !isBasicLand(card) && qtyInDeck >= 4;
                          const disabledAdd = savingCard || isAtMaxOwned || isAtRuleMax;

                          return (
                            <div key={card.id} style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '0.35rem 0.5rem', background: 'rgba(255,255,255,0.02)', borderRadius: '4px', border: '1px solid var(--border-glass)' }}>
                              <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', cursor: 'pointer' }} onClick={() => setPreviewCard(card)}>
                                <CardImage card={card} loading="lazy" decoding="async" style={{ width: '24px', height: '33px', objectFit: 'cover', borderRadius: '2px' }} />
                                <div style={{ display: 'flex', flexDirection: 'column' }}>
                                  <span style={{ fontSize: '0.8rem', color: 'var(--text-strong)' }}>{displayName(card)} ({card.set_name} • #{card.number})</span>
                                  <span style={{ fontSize: '0.65rem', color: isAtMaxOwned ? 'var(--accent-red)' : 'var(--text-secondary)' }}>Owned: {ownedQty} | In Deck: {qtyInDeck}</span>
                                </div>
                              </div>
                              <div style={{ display: 'flex', alignItems: 'center', gap: '0.35rem' }}>
                                <button className="btn btn-secondary btn-icon-only" style={{ padding: '0.2rem' }} onClick={() => setPreviewCard(card)} title={t('deck.previewArt')}>
                                  <Eye size={12} />
                                </button>
                                <button className="btn btn-primary btn-icon-only" style={{ padding: '0.2rem' }} disabled={disabledAdd} onClick={() => handleAddCardToDeck(card)} title={isAtRuleMax ? "4-copy limit reached" : isAtMaxOwned ? "Not enough owned copies" : "Add to deck"}>
                                  <Plus size={12} />
                                </button>
                              </div>
                            </div>
                          );
                      })}
                    </div>
                  )}
                </div>

                {/* Deck Cards Header & Display Mode Toggle */}
                <div className="glass-panel deck-pane-cards" style={{ display: 'flex', flexDirection: 'column', gap: '1rem' }}>
                  <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: '0.5rem' }}>
                    <h3 style={{ fontSize: '1rem', color: 'var(--text-strong)', borderLeft: '3px solid var(--accent-red)', paddingLeft: '0.5rem', margin: 0 }}>
                      Deck Cards ({totalDeckCardsCount} / {targetDeckCardsCount})
                    </h3>
                    <div style={{ display: 'flex', alignItems: 'center', gap: '4px', background: 'rgba(0,0,0,0.2)', padding: '2px', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border-glass)' }}>
                      <button
                        type="button"
                        className={`btn ${cardDisplayMode === 'list' ? 'btn-primary' : 'btn-secondary'}`}
                        style={{ padding: '0.25rem 0.5rem', fontSize: '0.75rem', display: 'flex', alignItems: 'center', gap: '4px' }}
                        onClick={() => setCardDisplayMode('list')}
                      >
                        <List size={12} /> List
                      </button>
                      <button
                        type="button"
                        className={`btn ${cardDisplayMode === 'grid' ? 'btn-primary' : 'btn-secondary'}`}
                        style={{ padding: '0.25rem 0.5rem', fontSize: '0.75rem', display: 'flex', alignItems: 'center', gap: '4px' }}
                        onClick={() => setCardDisplayMode('grid')}
                      >
                        <LayoutGrid size={12} /> Grid
                      </button>
                    </div>
                  </div>
                  
                  {activeDeck.cards.length === 0 ? (
                    <p style={{ fontSize: '0.85rem', color: 'var(--text-secondary)', textAlign: 'center', padding: '2rem 0' }}>{t('deck.emptyDeck')}</p>
                  ) : (
                    deckGroups.map(group => {
                      const { name: supertype, cards: list, count: sum } = group;

                      return (
                        <div key={supertype} style={{ display: 'flex', flexDirection: 'column', gap: '0.5rem' }}>
                          <h4 style={{ fontSize: '0.85rem', color: 'var(--text-secondary)', borderBottom: '1px solid var(--border-glass)', paddingBottom: '0.25rem', display: 'flex', justifyContent: 'space-between' }}>
                            <span>{supertype}s</span>
                            <span style={{ color: 'var(--text-strong)', fontWeight: 600 }}>{sum}</span>
                          </h4>

                          {/* 1. COMPACT LIST VIEW */}
                          {cardDisplayMode === 'list' && (
                            <div style={{ display: 'flex', flexDirection: 'column', gap: '0.4rem' }}>
                              {list.map(card => (
                                <div key={card.id} style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '0.5rem 0.75rem', background: 'rgba(255,255,255,0.01)', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border-glass)' }}>
                                  <div style={{ display: 'flex', alignItems: 'center', gap: '0.75rem', cursor: 'pointer' }} onClick={() => setPreviewCard(card)}>
                                    <CardImage card={card} loading="lazy" decoding="async" style={{ width: '32px', height: '44px', objectFit: 'cover', borderRadius: '2px' }} />
                                    <div style={{ minWidth: 0 }}>
                                      <div style={{ fontSize: '0.85rem', fontWeight: 700, color: 'var(--text-strong)' }}>{displayName(card)}</div>
                                      <div style={{ fontSize: '0.7rem', color: 'var(--text-secondary)' }}>{card.set_name} • #{card.number}</div>
                                      {(card.owned_qty || 0) < card.quantity && !isBasicLand(card) && (
                                        <div className="deck-missing-note" title={t('deck.rowMissingTitle', { need: card.quantity, have: card.owned_qty || 0 })}>
                                          {t('deck.rowMissing', { count: card.quantity - (card.owned_qty || 0) })}
                                        </div>
                                      )}
                                    </div>
                                  </div>

                                  <div style={{ display: 'flex', alignItems: 'center', gap: '0.75rem' }}>
                                    <span className="deck-card-price" title={t('deck.cardPriceTitle')}>{cardPriceLabel(card)}</span>
                                    <div style={{ display: 'flex', alignItems: 'center', gap: '4px', background: 'rgba(0,0,0,0.2)', padding: '2px', borderRadius: '4px', border: '1px solid var(--border-glass)' }}>
                                      <button
                                        className={`btn ${card.quantity === 1 ? 'btn-danger' : 'btn-secondary'} btn-icon-only`}
                                        style={{ width: '22px', height: '22px', padding: 0, display: 'flex', alignItems: 'center', justifyContent: 'center' }}
                                        disabled={savingCard}
                                        onClick={() => handleUpdateCardQty(card.id, card.quantity - 1)}
                                        title={t(card.quantity === 1 ? 'deck.removeFromDeck' : 'deck.decreaseQty')}
                                      >
                                        {card.quantity === 1 ? <Trash2 size={11} /> : '-'}
                                      </button>
                                      <span style={{ padding: '0 0.4rem', fontSize: '0.85rem', fontWeight: 700, minWidth: '18px', textAlign: 'center', color: 'var(--text-strong)' }}>{card.quantity}</span>
                                      <button
                                        className="btn btn-secondary btn-icon-only"
                                        style={{ width: '22px', height: '22px', padding: 0 }}
                                        disabled={savingCard || card.quantity >= (card.owned_qty || 0) || (!isBasicLand(card) && (countsByName.get(cardKey(card)) || 0) >= 4)}
                                        onClick={() => handleUpdateCardQty(card.id, card.quantity + 1)}
                                      >
                                        +
                                      </button>
                                    </div>
                                  </div>
                                </div>
                              ))}
                            </div>
                          )}

                          {/* 2. VISUAL CARD GRID VIEW */}
                          {cardDisplayMode === 'grid' && (
                            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(110px, 1fr))', gap: '0.75rem' }}>
                              {list.map(card => (
                                <div key={card.id} style={{ position: 'relative', borderRadius: '6px', overflow: 'hidden', border: '1px solid var(--border-glass)', background: 'rgba(0,0,0,0.3)', display: 'flex', flexDirection: 'column', transition: 'transform 0.15s' }}>
                                  <div className="deck-grid-card" style={{ position: 'relative', width: '100%', aspectRatio: '488 / 680', overflow: 'hidden', cursor: 'pointer' }} onClick={() => setPreviewCard(card)}>
                                    <CardImage card={card} loading="lazy" decoding="async" style={{ position: 'absolute', inset: 0, display: 'block', width: '100%', height: '100%', objectFit: 'cover' }} />
                                    <span style={{ position: 'absolute', top: '4px', right: '4px', background: 'rgba(0,0,0,0.85)', color: 'var(--accent-yellow)', fontSize: '0.75rem', fontWeight: 800, padding: '1px 6px', borderRadius: '10px', border: '1px solid var(--accent-yellow)' }}>
                                      x{card.quantity}
                                    </span>
                                    <span className="deck-card-price-chip" title={t('deck.cardPriceTitle')}>{cardPriceLabel(card)}</span>
                                    {(card.owned_qty || 0) < card.quantity && !isBasicLand(card) && (
                                      <span className="deck-missing-chip" title={t('deck.rowMissingTitle', { need: card.quantity, have: card.owned_qty || 0 })}>
                                        +{card.quantity - (card.owned_qty || 0)}
                                      </span>
                                    )}
                                  </div>
                                  <div style={{ padding: '4px', display: 'flex', justifyContent: 'center', background: 'rgba(0,0,0,0.5)' }}>
                                    <div style={{ display: 'flex', gap: '2px' }}>
                                      <button className={`btn ${card.quantity === 1 ? 'btn-danger' : 'btn-secondary'} btn-icon-only`} style={{ width: '20px', height: '20px', fontSize: '0.7rem', padding: 0, display: 'flex', alignItems: 'center', justifyContent: 'center' }} disabled={savingCard} onClick={() => handleUpdateCardQty(card.id, card.quantity - 1)} title={t(card.quantity === 1 ? 'deck.removeFromDeck' : 'deck.decreaseQty')}>
                                        {card.quantity === 1 ? <Trash2 size={10} /> : '-'}
                                      </button>
                                      <button className="btn btn-secondary btn-icon-only" style={{ width: '20px', height: '20px', fontSize: '0.7rem', padding: 0 }} disabled={savingCard || card.quantity >= (card.owned_qty || 0) || (!isBasicLand(card) && (countsByName.get(cardKey(card)) || 0) >= 4)} onClick={() => handleUpdateCardQty(card.id, card.quantity + 1)}>+</button>
                                    </div>
                                  </div>
                                </div>
                              ))}
                            </div>
                          )}

                        </div>
                      );
                    })
                  )}
                </div>
              </div>

              {/* Right Column: Statistics, Mana Curve & Deck Health */}
              <div className="deck-pane-stats" style={{ flex: '1 1 320px', display: 'flex', flexDirection: 'column', gap: '1.5rem' }}>

                {/* Value: what the deck's OWN printings cost vs the cheapest
                    printings floor. Two axes answer two different questions —
                    sell-this-deck (current) vs build-it-cheap (floor); the
                    printings the deck actually holds are one representative
                    per logical card, so an unpriced representative hides a
                    priced reprint. Buy sends the whole deck to ManaPool or
                    TCGplayer exactly like a list does. */}
                <div className="glass-panel" style={{ display: 'flex', flexDirection: 'column', gap: '0.5rem' }}>
                  <h3 style={{ fontSize: '0.95rem', color: 'var(--text-strong)', margin: 0, display: 'flex', alignItems: 'center', gap: '6px' }}>
                    <DollarSign size={14} style={{ color: 'var(--accent-yellow)' }} /> {t('deck.valueTitle')}
                    <button className="btn btn-primary btn-icon-only" onClick={openBuy}
                      title={t('deck.buyDeck')} aria-label={t('deck.buyDeck')}
                      style={{ marginLeft: 'auto', display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
                      <ShoppingCart size={16} />
                    </button>
                  </h3>
                  <div style={{ display: 'flex', flexDirection: 'column', gap: '0.4rem', fontSize: '0.8rem' }}>
                    <div style={{ display: 'flex', justifyContent: 'space-between', color: 'var(--text-secondary)' }}>
                      <span title={currentValueHint} tabIndex={0}
                        aria-label={`${t('deck.valueCurrent')}: ${currentValueText}. ${currentValueHint}`}
                        style={{ display: 'inline-flex', alignItems: 'center', gap: '4px' }}>
                        {t('deck.valueCurrent')}
                        {Number(activeDeck?.current_unpriced_cards) > 0 && (
                          <small style={{ color: 'var(--text-muted)', fontWeight: 600 }}>({t('deck.unpricedCount', { count: currentUnpricedCards })})</small>
                        )}
                      </span>
                      <strong style={{ color: 'var(--text-strong)' }}>{currentValueText}</strong>
                    </div>
                    <div style={{ display: 'flex', justifyContent: 'space-between', color: 'var(--text-secondary)' }}>
                      <span title={deckMinimumValueHint(activeDeck, t)} tabIndex={0}
                        aria-label={`${t('deck.valueCheapest')}: ${deckMinimumValueText(activeDeck)}. ${deckMinimumValueHint(activeDeck, t)}`}>
                        {t('deck.valueCheapest')}
                      </span>
                      <strong style={{ color: 'var(--accent-yellow)' }}>{deckMinimumValueText(activeDeck)}</strong>
                    </div>
                  </div>
                </div>
                
                {/* Moxfield: mirrored decks get their own one-action section —
                    just the exit to the upstream listing, promoted out of the
                    (now gone) Deck Health panel. Unmirrored decks have nothing
                    to point at, so the section doesn't render for them. */}
                {moxfieldDeckUrl(activeDeck) && (
                  <div className="glass-panel" style={{ display: 'flex', flexDirection: 'column', gap: '0.6rem' }}>
                    <h3 style={{ fontSize: '0.95rem', color: 'var(--text-strong)', margin: 0, display: 'flex', alignItems: 'center', gap: '6px' }}>
                      <ExternalLink size={14} style={{ color: '#3b82f6' }} /> {t('deck.moxfieldTitle')}
                    </h3>
                    <a
                      className="btn btn-secondary"
                      href={moxfieldDeckUrl(activeDeck)}
                      target="_blank"
                      rel="noreferrer"
                      style={{ justifyContent: 'center', fontSize: '0.8rem', padding: '0.5rem 0.9rem' }}
                    >
                      <ExternalLink size={13} /> {t('mfx.openOnMoxfield')}
                    </a>
                  </div>
                )}

                {/* Bar Chart: Mana Cost Curve */}
                {manaCurveData.some(d => d.count > 0) && (
                  <div className="glass-panel" style={{ display: 'flex', flexDirection: 'column', gap: '0.5rem' }}>
                    <h3 style={{ fontSize: '0.95rem', color: 'var(--text-strong)', marginBottom: '0.5rem', display: 'flex', alignItems: 'center', gap: '6px' }}>
                      <BarChart2 size={14} style={{ color: '#3b82f6' }} /> Mana Cost Curve
                    </h3>
                    <div style={{ width: '100%', height: '180px' }}>
                      <ResponsiveContainer width="100%" height="100%">
                        <BarChart data={manaCurveData} margin={{ top: 10, right: 10, left: -20, bottom: 5 }}>
                          <XAxis dataKey="cost" stroke="var(--text-muted)" fontSize={10} tickLine={false} />
                          <YAxis stroke="var(--text-muted)" fontSize={10} tickLine={false} />
                          <Tooltip contentStyle={{ background: 'rgba(0,0,0,0.8)', border: '1px solid var(--border-glass)', borderRadius: '4px', fontSize: '0.8rem', color: 'var(--text-strong)' }} />
                          <Bar dataKey="count" fill="#3b82f6" radius={[4, 4, 0, 0]} />
                        </BarChart>
                      </ResponsiveContainer>
                    </div>
                  </div>
                )}

                {/* Pie Chart: Supertypes */}
                {supertypeData.length > 0 && (
                  <div className="glass-panel" style={{ display: 'flex', flexDirection: 'column', gap: '0.5rem' }}>
                    <h3 style={{ fontSize: '0.95rem', color: 'var(--text-strong)', marginBottom: '0.5rem', display: 'flex', alignItems: 'center', gap: '6px' }}>
                      <BarChart2 size={14} style={{ color: 'var(--accent-red)' }} /> Supertype Breakdown
                    </h3>
                    <div style={{ width: '100%', height: '180px' }}>
                      <ResponsiveContainer width="100%" height="100%">
                        <PieChart>
                          <Pie
                            data={supertypeData}
                            cx="50%"
                            cy="50%"
                            innerRadius={50}
                            outerRadius={70}
                            paddingAngle={3}
                            dataKey="value"
                          >
                            {supertypeData.map((entry, index) => (
                              <Cell key={`cell-${index}`} fill={PIE_COLORS[index % PIE_COLORS.length]} />
                            ))}
                          </Pie>
                          <Tooltip contentStyle={{ background: 'rgba(0,0,0,0.8)', border: '1px solid var(--border-glass)', borderRadius: '4px', fontSize: '0.8rem', color: 'var(--text-strong)' }} />
                        </PieChart>
                      </ResponsiveContainer>
                    </div>
                    {/* Legend */}
                    <div style={{ display: 'flex', justifyContent: 'center', gap: '1rem', flexWrap: 'wrap', marginTop: '0.25rem' }}>
                      {supertypeData.map((d, index) => (
                        <div key={d.name} style={{ display: 'flex', alignItems: 'center', gap: '4px', fontSize: '0.75rem' }}>
                          <div style={{ width: '10px', height: '10px', borderRadius: '50%', backgroundColor: PIE_COLORS[index % PIE_COLORS.length] }}></div>
                          <span style={{ color: 'var(--text-secondary)' }}>{d.name}: <strong>{d.value}</strong></span>
                        </div>
                      ))}
                    </div>
                  </div>
                )}

                {/* Bar Chart: Color & Land Distribution */}
                {colorLandData.length > 0 && (
                  <div className="glass-panel" style={{ display: 'flex', flexDirection: 'column', gap: '0.5rem' }}>
                    <h3 style={{ fontSize: '0.95rem', color: 'var(--text-strong)', marginBottom: '0.5rem', display: 'flex', alignItems: 'center', gap: '6px' }}>
                      <BarChart2 size={14} style={{ color: 'var(--accent-yellow)' }} /> {t('deck.colorLandDist')}
                    </h3>
                    <div style={{ width: '100%', height: '220px' }}>
                      <ResponsiveContainer width="100%" height="100%">
                        <BarChart data={colorLandData} margin={{ top: 10, right: 10, left: -20, bottom: 5 }}>
                          <XAxis dataKey="name" stroke="var(--text-muted)" fontSize={10} tickLine={false} />
                          <YAxis stroke="var(--text-muted)" fontSize={10} tickLine={false} />
                          <Tooltip contentStyle={{ background: 'rgba(0,0,0,0.8)', border: '1px solid var(--border-glass)', borderRadius: '4px', fontSize: '0.8rem', color: 'var(--text-strong)' }} />
                          <Bar dataKey="value" fill="var(--accent-yellow)" radius={[4, 4, 0, 0]}>
                            {colorLandData.map((entry, idx) => {
                              const colorMap = {
                                'White': '#fef08a', 'Blue': '#3b82f6', 'Black': '#475569', 'Red': '#ef4444', 'Green': '#10b981', 'Colorless': '#cbd5e1',
                                'Land (Plains)': '#fef08a', 'Land (Island)': '#60a5fa', 'Land (Swamp)': '#475569', 'Land (Mountain)': '#f87171', 'Land (Forest)': '#4ade80', 'Land (Nonbasic)': '#d97706'
                              };
                              return <Cell key={`cell-${idx}`} fill={colorMap[entry.name] || 'var(--accent-yellow)'} />;
                            })}
                          </Bar>
                        </BarChart>
                      </ResponsiveContainer>
                    </div>
                  </div>
                )}
              </div>

            </div>
          </div>
        </div>
      )}

      {/* --- POPUPS & MODALS --- */}

      {/* Deck buy modal — same flow as the Lists buy modal: provider cards for
          ManaPool + TCGplayer prefilled Mass Entry, the plain list selectable
          as a copy fallback, Escape and an overlay click close it. */}
      {showBuy && activeDeck && (
        <div className="modal-overlay" onClick={e => { if (e.target === e.currentTarget) setShowBuy(false); }}
          style={{ position: 'fixed', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: 'rgba(0,0,0,0.75)', backdropFilter: 'blur(6px)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 999 }}>
          <div className="glass-panel" style={{ maxWidth: '520px', width: '100%', maxHeight: '90vh', overflowY: 'auto', padding: '1.75rem', position: 'relative', border: '1px solid rgba(255,255,255,0.15)' }}>
            <button className="btn btn-secondary btn-icon-only" onClick={() => setShowBuy(false)}
              aria-label={t('common.close')} title={t('common.close')}
              style={{ position: 'absolute', top: '0.75rem', right: '0.75rem', zIndex: 1 }}><X size={16} /></button>
            <h3 style={{ fontSize: '1.1rem', fontWeight: 800, color: 'var(--text-strong)', margin: '0 0 0.35rem', paddingRight: '2.25rem' }}>{t('deck.buyDeck')}</h3>
            <div style={{ fontSize: '0.85rem', color: 'var(--text-secondary)', marginBottom: '1rem', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>{activeDeck.name}</div>

            {/* Provider choices — flat bordered cards, no glass, wrap on narrow screens */}
            <div style={{ display: 'flex', gap: '0.75rem', flexWrap: 'wrap' }}>
              {[
                { label: t('lists.buyOnManapool'), hint: buyText ? t('lists.buyManaPoolHint', { count: buyLineCount }) : '', onClick: buyManapool, accent: activeDeck.accent_color || '#eab308' },
                { label: t('lists.buyTcg'), hint: buyText ? t('lists.buyTcgHint', { count: buyLineCount }) : '', onClick: buyTcgplayer, accent: '#ff2b03' },
              ].map(card => (
                <button key={card.label} type="button" onClick={card.onClick}
                  disabled={!buyText}
                  style={{ flex: '1 1 200px', minWidth: '200px', display: 'flex', flexDirection: 'column', gap: '0.35rem', padding: '0.9rem 1rem', borderRadius: 'var(--radius-sm)', textAlign: 'left', cursor: !buyText ? 'not-allowed' : 'pointer', opacity: !buyText ? 0.5 : 1, background: 'rgba(0,0,0,0.3)', border: `1.5px solid ${card.accent}66`, color: 'var(--text-primary)', fontSize: '0.8rem' }}>
                  <span style={{ fontWeight: 800, color: 'var(--text-strong)', fontSize: '0.9rem' }}>{card.label}</span>
                  <span style={{ color: 'var(--text-secondary)', lineHeight: 1.4 }}>{card.hint}</span>
                </button>
              ))}
            </div>

            {!buyText && (
              <div style={{ fontSize: '0.8rem', color: 'var(--accent-red)', marginTop: '0.9rem' }}>{t('deck.buyEmpty')}</div>
            )}

            {/* The plain text itself: always selectable, so the flow survives a
                broken clipboard. */}
            {buyText && (
              <div style={{ marginTop: '0.9rem', display: 'flex', flexDirection: 'column', gap: '0.5rem' }}>
                <textarea readOnly value={buyText} rows={6} aria-label={t('deck.buyDeck')}
                  onFocus={e => e.target.select()}
                  style={{ width: '100%', fontFamily: 'monospace', fontSize: '0.75rem', padding: '0.6rem', borderRadius: 'var(--radius-sm)', background: 'rgba(0,0,0,0.3)', color: 'var(--text-secondary)', border: '1px solid rgba(255,255,255,0.15)', resize: 'vertical', boxSizing: 'border-box' }} />
                <div style={{ display: 'flex', alignItems: 'center', gap: '0.75rem', flexWrap: 'wrap' }}>
                  <button type="button" className="btn btn-secondary" onClick={copyBuyList}
                    style={{ fontSize: '0.8rem', display: 'flex', alignItems: 'center', gap: '0.4rem' }}>
                    <Copy size={14} /> {t('deck.buyCopy')}
                  </button>
                  <span style={{ fontSize: '0.75rem', color: 'var(--text-muted)' }}>{t('lists.buyLineCount', { count: buyLineCount })}</span>
                </div>
              </div>
            )}
          </div>
        </div>
      )}

      {/* One entry point for every way a deck can enter the vault. */}
      {showAddDeckModal && (
        <AddDeckChoiceModal
          open={showAddDeckModal}
          onClose={() => setShowAddDeckModal(false)}
          onCustom={() => {
            setShowAddDeckModal(false);
            setShowCreateModal(true);
          }}
          onPrecon={() => {
            setShowAddDeckModal(false);
            setShowPreconModal(true);
          }}
          onMoxfield={() => {
            setShowAddDeckModal(false);
            onNavigate?.('settings', 'moxfield');
          }}
          returnFocusRef={addDeckButtonRef}
        />
      )}

      {/* A. Create Deck Modal */}
      {showCreateModal && (
        <div className="modal-overlay" style={{ position: 'fixed', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: 'rgba(0,0,0,0.75)', backdropFilter: 'blur(6px)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 999 }}>
          <div className="glass-panel" style={{ maxWidth: '480px', width: '100%', maxHeight: '90vh', overflowY: 'auto', overscrollBehavior: 'contain', padding: '1.75rem', position: 'relative', border: '1px solid rgba(255,255,255,0.15)' }}>
            <button className="btn btn-secondary btn-icon-only" onClick={() => setShowCreateModal(false)} style={{ position: 'absolute', top: '1rem', right: '1rem', borderRadius: '50%' }}>
              <X size={16} />
            </button>

            <h3 style={{ fontSize: '1.25rem', color: 'var(--text-strong)', fontWeight: 800, marginBottom: '0.25rem', display: 'flex', alignItems: 'center', gap: '8px' }}>
              <FolderPlus size={20} style={{ color: 'var(--accent-yellow)' }} />
              {t('deck.createTitle')}
            </h3>
            <p style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', marginBottom: '1.25rem' }}>
              {t('deck.createSubtitle')}
            </p>

            <form onSubmit={handleCreateDeck} style={{ display: 'flex', flexDirection: 'column', gap: '1.1rem', maxHeight: '80vh', overflowY: 'auto', paddingRight: '0.25rem' }}>
              
              {/* Format & Target Size Row */}
              <div style={{ display: 'grid', gridTemplateColumns: '2fr 1fr', gap: '0.75rem' }}>
                <div className="form-group">
                  <label style={{ fontSize: '0.8rem', fontWeight: 700, color: 'var(--text-strong)', marginBottom: '0.3rem', display: 'block' }}>{t('deck.format')}</label>
                  <select
                    className="input-control"
                    value={newDeckFormat}
                    onChange={(e) => {
                      const selectedFmt = e.target.value;
                      setNewDeckFormat(selectedFmt);
                      if (selectedFmt.includes('Commander')) setNewDeckTargetSize(100);
                      else if (selectedFmt.includes('Standard') || selectedFmt.includes('Modern') || selectedFmt.includes('Pioneer')) setNewDeckTargetSize(60);
                    }}
                    style={{ fontSize: '0.85rem' }}
                  >
                    {(MTG_FORMATS).map(fmt => (
                      <option key={fmt} value={fmt} style={{ background: '#1e293b', color: '#fff' }}>{fmt}</option>
                    ))}
                  </select>
                </div>

                <div className="form-group">
                  <label style={{ fontSize: '0.8rem', fontWeight: 700, color: 'var(--text-strong)', marginBottom: '0.3rem', display: 'block' }}>{t('deck.targetSize')}</label>
                  <input
                    type="number"
                    min="1"
                    max="300"
                    className="input-control"
                    value={newDeckTargetSize}
                    onChange={(e) => setNewDeckTargetSize(parseInt(e.target.value, 10) || 60)}
                    style={{ fontSize: '0.85rem' }}
                  />
                </div>
              </div>

              {/* Deck Name */}
              <div className="form-group">
                <label style={{ fontSize: '0.8rem', fontWeight: 700, color: 'var(--text-strong)', marginBottom: '0.3rem', display: 'block' }}>{t('deck.deckName')}</label>
                <input 
                  type="text" 
                  className="input-control" 
                  placeholder={t('deck.namePlaceholder')} 
                  value={newDeckName} 
                  onChange={(e) => setNewDeckName(e.target.value)}
                  required 
                  autoFocus
                />
              </div>

              {/* Category Pills */}
              <div className="form-group">
                <label style={{ fontSize: '0.8rem', fontWeight: 700, color: 'var(--text-strong)', marginBottom: '0.4rem', display: 'block' }}>{t('deck.category')}</label>
                <div style={{ display: 'flex', flexWrap: 'wrap', gap: '0.4rem' }}>
                  {DECK_CATEGORIES.map(cat => {
                    const isSelected = newDeckCategory === cat;
                    return (
                      <button
                        key={cat}
                        type="button"
                        onClick={() => setNewDeckCategory(cat)}
                        style={{
                          fontSize: '0.75rem',
                          fontWeight: 700,
                          padding: '0.3rem 0.65rem',
                          borderRadius: '12px',
                          border: isSelected ? '1px solid var(--accent-yellow)' : '1px solid var(--border-glass)',
                          background: isSelected ? 'rgba(234, 179, 8, 0.2)' : 'rgba(0,0,0,0.2)',
                          color: isSelected ? 'var(--accent-yellow)' : 'var(--text-secondary)',
                          cursor: 'pointer',
                          transition: 'all 0.15s'
                        }}
                      >
                        {cat}
                      </button>
                    );
                  })}
                </div>
              </div>

              {/* Deck Accent Color */}
              <div className="form-group">
                <label style={{ fontSize: '0.8rem', fontWeight: 700, color: 'var(--text-strong)', marginBottom: '0.4rem', display: 'block' }}>{t('deck.accentColor')}</label>
                <div style={{ display: 'flex', itemsAlign: 'center', gap: '0.5rem', flexWrap: 'wrap' }}>
                  {DECK_ACCENT_COLORS.map(c => {
                    const isSelected = newDeckAccentColor === c.hex;
                    return (
                      <div
                        key={c.hex}
                        onClick={() => setNewDeckAccentColor(c.hex)}
                        title={c.name}
                        style={{
                          width: '26px',
                          height: '26px',
                          borderRadius: '50%',
                          backgroundColor: c.hex,
                          cursor: 'pointer',
                          border: isSelected ? '2px solid #ffffff' : '2px solid transparent',
                          boxShadow: isSelected ? `0 0 10px ${c.hex}` : 'none',
                          transform: isSelected ? 'scale(1.15)' : 'scale(1)',
                          transition: 'all 0.15s'
                        }}
                      />
                    );
                  })}
                </div>
              </div>

              {/* Description (Optional) */}
              <div className="form-group">
                <label style={{ fontSize: '0.8rem', fontWeight: 700, color: 'var(--text-strong)', marginBottom: '0.3rem', display: 'block' }}>{t('deck.descriptionOptional')}</label>
                <textarea
                  className="input-control"
                  style={{ minHeight: '65px', resize: 'vertical', fontSize: '0.85rem' }}
                  placeholder={t('deck.notesPlaceholder')}
                  value={newDeckDesc}
                  onChange={(e) => setNewDeckDesc(e.target.value)}
                />
              </div>

              {/* Quick Decklist Importer Toggle */}
              <div>
                <button
                  type="button"
                  onClick={() => setShowImportDecklistArea(!showImportDecklistArea)}
                  style={{
                    background: 'none',
                    border: 'none',
                    color: 'var(--accent-yellow)',
                    fontSize: '0.8rem',
                    fontWeight: 700,
                    cursor: 'pointer',
                    display: 'flex',
                    alignItems: 'center',
                    gap: '4px',
                    padding: 0
                  }}
                >
                  <FileText size={14} />
                  {showImportDecklistArea ? t('deck.hideQuickImport') : t('deck.showQuickImport')}
                </button>

                {showImportDecklistArea && (
                  <div style={{ marginTop: '0.5rem' }}>
                    <textarea
                      className="input-control"
                      style={{ minHeight: '90px', fontFamily: 'monospace', fontSize: '0.8rem', whiteSpace: 'pre' }}
                      placeholder={t('deck.pasteDecklistPlaceholder')}
                      value={newDeckImportText}
                      onChange={(e) => setNewDeckImportText(e.target.value)}
                    />
                    <span style={{ fontSize: '0.65rem', color: 'var(--text-muted)', display: 'block', marginTop: '0.2rem' }}>
                      {t('deck.importOnCreateHint')}
                    </span>
                  </div>
                )}
              </div>

              <div style={{ display: 'flex', gap: '0.5rem', marginTop: '0.5rem' }}>
                <button type="button" className="btn btn-secondary" style={{ flex: 1 }} onClick={() => setShowCreateModal(false)}>{t('common.cancel')}</button>
                <button type="submit" className="btn btn-primary" style={{ flex: 2, fontWeight: 700 }}>{t('deck.createDeck')}</button>
              </div>
            </form>
          </div>
        </div>
      )}

      {/* B. Draw Hand Simulator Modal */}
      {showSimulator && (
        <div className="modal-overlay" style={{ position: 'fixed', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: 'rgba(0,0,0,0.85)', backdropFilter: 'blur(5px)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 999 }}>
          <div className="glass-panel" style={{ maxWidth: '1000px', width: '100%', maxHeight: '90vh', overflowY: 'auto', overscrollBehavior: 'contain', padding: '2rem', display: 'flex', flexDirection: 'column', gap: '1.5rem', position: 'relative' }}>
            <button className="btn btn-secondary btn-icon-only" onClick={() => setShowSimulator(false)} style={{ position: 'absolute', top: '1rem', right: '1rem', borderRadius: '50%' }}>
              <X size={16} />
            </button>

            <div>
              <h3 style={{ fontSize: '1.25rem', color: 'var(--text-strong)', margin: 0 }}>{t('deck.handSimulator')}</h3>
              <p style={{ color: 'var(--text-secondary)', fontSize: '0.8rem', marginTop: '0.2rem' }}>
                {t('deck.mulliganCountText', { mulligans: mulliganCount, handSize: hand.length })}
              </p>
            </div>

            {/* Hand Area */}
            <div style={{ 
              background: 'rgba(0,0,0,0.4)', 
              minHeight: '220px', 
              borderRadius: 'var(--radius-md)', 
              border: '1px solid var(--border-glass)', 
              display: 'flex', 
              flexWrap: 'wrap', 
              justifyContent: 'center', 
              alignItems: 'center', 
              gap: '1rem', 
              padding: '1.5rem' 
            }}>
              {hand.length === 0 ? (
                <div style={{ color: 'var(--text-muted)', fontSize: '0.9rem' }}>{t('deck.noCardsDrawn')}</div>
              ) : (
                hand.map((card, idx) => (
                  <div key={idx} style={{ 
                    width: '130px', 
                    aspectRatio: 0.718, 
                    borderRadius: '8px', 
                    overflow: 'hidden', 
                    boxShadow: '0 4px 10px rgba(0,0,0,0.5)',
                    animation: 'draw-card-anim 0.3s ease-out forwards',
                    border: '1px solid var(--border-glass)',
                    position: 'relative',
                    cursor: 'pointer'
                  }} onClick={() => setPreviewCard(card)}>
                    <CardImage card={card} style={{ width: '100%', height: '100%', objectFit: 'cover' }} />
                  </div>
                ))
              )}
            </div>

            {/* Control buttons */}
            <div style={{ display: 'flex', justifyContent: 'center', gap: '0.75rem', flexWrap: 'wrap' }}>
              <button className="btn btn-secondary" onClick={startSimulator} style={{ display: 'flex', alignItems: 'center', gap: '4px' }}>
                {t('deck.reshuffle')}
              </button>
              <button 
                className="btn btn-secondary" 
                onClick={handleMulligan} 
                style={{ display: 'flex', alignItems: 'center', gap: '4px' }}
                disabled={hand.length === 0}
              >
                {t('deck.mulliganDraw', { count: Math.max(1, 7 - (mulliganCount + 1)) })}
              </button>
              <button 
                className="btn btn-primary" 
                onClick={handleDrawCard} 
                style={{ display: 'flex', alignItems: 'center', gap: '4px' }}
                disabled={hand.length >= simulatorDeck.length}
              >
                {t('deck.drawOne')}
              </button>
            </div>

            <style>{`
              @keyframes draw-card-anim {
                from { transform: translateY(30px) scale(0.85); opacity: 0; }
                to { transform: translateY(0) scale(1); opacity: 1; }
              }
              @keyframes shimmer-gold {
                0% { background-position: 0% center; }
                100% { background-position: 200% center; }
              }
            `}</style>
          </div>
        </div>
      )}

      {/* C. Export Modal */}
      {/* What's missing: deck requirement vs owned collection, with copy and
          create-list-from-shortfall exits riding the shared deficit math. */}
      {missingOpen && (
        <div className="modal-overlay" onClick={closeMissing}
          style={{ position: 'fixed', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: 'rgba(0,0,0,0.7)', backdropFilter: 'blur(5px)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 999 }}>
          <div className="glass-panel" onClick={(e) => e.stopPropagation()}
            style={{ maxWidth: '560px', width: '100%', maxHeight: '80vh', overflowY: 'auto', overscrollBehavior: 'contain', padding: '1.6rem', position: 'relative' }}>
            <button className="btn btn-secondary btn-icon-only" onClick={closeMissing} title={t('common.close')}
              style={{ position: 'absolute', top: '1rem', right: '1rem', borderRadius: '50%' }}>
              <X size={16} />
            </button>
            <h3 style={{ fontSize: '1.2rem', color: 'var(--text-strong)', marginBottom: '0.4rem' }}>{t('deck.exportMissing')}</h3>
            <p style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', marginBottom: '1rem' }}>{t('deck.missingHint')}</p>
            {missingRows().length === 0 ? (
              <p style={{ fontSize: '0.85rem', color: 'var(--text-secondary)', padding: '1rem 0' }}>{t('deck.nothingToBuy')}</p>
            ) : (
              <>
                <div style={{ border: '1px solid var(--border)', borderRadius: '10px', padding: '0.3rem 0.7rem', marginBottom: '1rem', maxHeight: '40vh', overflowY: 'auto', overscrollBehavior: 'contain' }}>
                  {missingRows().map((r, i) => (
                    <div key={`${r.name}-${r.card_id ?? r.scryfall_id ?? 'x'}-${i}`} style={{ display: 'flex', alignItems: 'center', gap: '0.6rem', padding: '0.4rem 0', borderBottom: '1px solid var(--border)' }}>
                      <span style={{ fontSize: '0.78rem', fontWeight: 700, color: 'var(--accent)', minWidth: '3rem' }}>+{r.need - r.have}</span>
                      <span style={{ fontSize: '0.85rem', color: 'var(--text-strong)', flex: 1 }}>{r.name}</span>
                      <span style={{ fontSize: '0.72rem', color: 'var(--text-muted)', whiteSpace: 'nowrap' }}>{r.have}/{r.need}</span>
                    </div>
                  ))}
                </div>
                <label style={{ display: 'block', fontSize: '0.8rem', color: 'var(--text-secondary)', marginBottom: '0.35rem' }}>{t('lists.name')}</label>
                <input className="input-control" value={missingListName} onChange={(e) => setMissingListName(e.target.value)}
                  style={{ width: '100%', fontSize: '0.85rem', marginBottom: '1rem' }} />
                <div style={{ display: 'flex', gap: '0.5rem', justifyContent: 'flex-end' }}>
                  <button className="btn btn-secondary" onClick={closeMissing}>{t('common.close')}</button>
                  <button className="btn btn-secondary" onClick={copyMissing} style={{ display: 'flex', alignItems: 'center', gap: '0.35rem' }}>
                    <Copy size={14} /> {t('deck.copyClipboard')}
                  </button>
                  <button className="btn btn-primary" onClick={saveMissingAsList} style={{ display: 'flex', alignItems: 'center', gap: '0.35rem' }}>
                    <ListChecks size={14} /> {t('deck.createMissing')}
                  </button>
                </div>
              </>
            )}
          </div>
        </div>
      )}

      {showExportModal && (
        <div className="modal-overlay" style={{ position: 'fixed', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: 'rgba(0,0,0,0.7)', backdropFilter: 'blur(5px)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 999 }}>
          <div className="glass-panel" style={{ maxWidth: '500px', width: '100%', maxHeight: '90vh', overflowY: 'auto', overscrollBehavior: 'contain', padding: '1.75rem', position: 'relative' }}>
            <button className="btn btn-secondary btn-icon-only" onClick={() => setShowExportModal(false)} style={{ position: 'absolute', top: '1rem', right: '1rem', borderRadius: '50%' }}>
              <X size={16} />
            </button>
            <h3 style={{ fontSize: '1.2rem', color: 'var(--text-strong)', marginBottom: '0.5rem' }}>{t('deck.exportTitle')}</h3>
            <p style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', marginBottom: '1rem' }}>{t('deck.exportHintBody')}</p>
            <select
              className="input-control"
              style={{ width: '100%', marginBottom: '1rem', fontSize: '0.85rem' }}
              value={effectiveExportFormat}
              onChange={e => setExportFormat(e.target.value)}
            >
              <option value="mtga">{t('deck.formatMtga')}</option>
              <option value="plain">{t('deck.formatPlain')}</option>
              <option value="buylist">{t('deck.formatBuylist')}</option>
            </select>
            {effectiveExportFormat === 'buylist' && (
              <p style={{ fontSize: '0.75rem', color: 'var(--text-secondary)', marginTop: '-0.5rem', marginBottom: '1rem' }}>
                {t('deck.buylistHint')}
              </p>
            )}
            <textarea
              readOnly
              className="input-control"
              style={{ width: '100%', height: '220px', fontFamily: 'monospace', fontSize: '0.8rem', resize: 'vertical' }}
              value={handleExportDeckText()}
            />
            <div style={{ display: 'flex', gap: '0.5rem', marginTop: '1rem' }}>
              <button className="btn btn-secondary" style={{ flex: 1 }} onClick={() => setShowExportModal(false)}>{t('common.close')}</button>
              <button className="btn btn-primary" style={{ flex: 1 }} onClick={handleCopyExportText}>{t('deck.copyClipboard')}</button>
              {effectiveExportFormat === 'buylist' && (
                <button className="btn btn-primary" style={{ flex: 1 }} onClick={handleOpenMassEntry}>{t('deck.copyOpenTcg')}</button>
              )}
            </div>
          </div>
        </div>
      )}

      {/* D. Import Modal with Collection Comparison */}
      {showImportModal && (
        <div className="modal-overlay" style={{ position: 'fixed', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: 'rgba(0,0,0,0.7)', backdropFilter: 'blur(5px)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 999 }}>
          <div className="glass-panel" style={{ maxWidth: '600px', width: '100%', padding: '1.75rem', position: 'relative', maxHeight: '85vh', display: 'flex', flexDirection: 'column' }}>
            <button className="btn btn-secondary btn-icon-only" onClick={() => { setShowImportModal(false); setImportComparison(null); }} style={{ position: 'absolute', top: '1rem', right: '1rem', borderRadius: '50%' }}>
              <X size={16} />
            </button>
            <h3 style={{ fontSize: '1.2rem', color: 'var(--text-strong)', marginBottom: '0.5rem' }}>{t('deck.importTitle')}</h3>
            <p style={{ fontSize: '0.8rem', color: 'var(--text-secondary)', marginBottom: '1rem' }}>{t('deck.pasteDecklistHint')}</p>
            
            <textarea
              className="input-control"
              style={{ width: '100%', minHeight: '120px', maxHeight: '180px', fontFamily: 'monospace', fontSize: '0.8rem', resize: 'vertical' }}
              placeholder={'4 Lightning Bolt\n1 Shock\n1 Fetch Land'}
              value={importText}
              onChange={e => { setImportText(e.target.value); setImportComparison(null); }}
            />

            {/* Comparison results table */}
            {comparingImport ? (
              <div style={{ padding: '1.5rem', textAlign: 'center' }}>
                <div className="spinner" style={{ margin: '0 auto 0.5rem auto' }}></div>
                <span style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>{t('deck.comparing')}</span>
              </div>
            ) : importComparison && (
              <div style={{ marginTop: '1rem', display: 'flex', flexDirection: 'column', gap: '0.5rem', flex: 1, overflowY: 'auto' }}>
                <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
                  <span>{t('deck.availabilityBreakdown')}</span>
                  <span style={{ color: 'var(--accent-yellow)', fontWeight: 700 }}>
                    {t('deck.fullyOwnedCards', { owned: importComparison.filter(i => i.status === 'full').length, total: importComparison.length })}
                  </span>
                </div>
                <div style={{ background: 'rgba(0,0,0,0.2)', border: '1px solid var(--border-glass)', borderRadius: 'var(--radius-sm)', padding: '0.5rem', display: 'flex', flexDirection: 'column', gap: '0.4rem', maxHeight: '180px', overflowY: 'auto' }}>
                  {importComparison.map((item, idx) => (
                    <div key={idx} style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', fontSize: '0.75rem', padding: '0.25rem 0.5rem', background: 'rgba(255,255,255,0.02)', borderRadius: '4px' }}>
                      <span style={{ color: 'var(--text-strong)', fontWeight: 600 }}>{item.rawName}</span>
                      <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem' }}>
                        <span style={{ color: 'var(--text-secondary)' }}>{t('deck.reqQuantity', { count: item.requestedQty })}</span>
                        <span style={{
                          padding: '2px 6px',
                          borderRadius: '10px',
                          fontWeight: 700,
                          fontSize: '0.65rem',
                          background: item.status === 'full' ? 'rgba(74, 222, 128, 0.15)' : item.status === 'partial' ? 'rgba(234, 179, 8, 0.15)' : 'rgba(239, 68, 68, 0.15)',
                          color: item.status === 'full' ? 'var(--success)' : item.status === 'partial' ? 'var(--accent-yellow)' : 'var(--accent-red)',
                          border: item.status === 'full' ? '1px solid rgba(74, 222, 128, 0.3)' : item.status === 'partial' ? '1px solid rgba(234, 179, 8, 0.3)' : '1px solid rgba(239, 68, 68, 0.3)'
                        }}>
                          {item.status === 'full' ? `Owned (${item.ownedQty})` : item.status === 'partial' ? `Partial (${item.ownedQty}/${item.requestedQty})` : `Missing (0)`}
                        </span>
                      </div>
                    </div>
                  ))}
                </div>
              </div>
            )}

            <div style={{ display: 'flex', gap: '0.5rem', marginTop: '1rem' }}>
              <button className="btn btn-secondary" style={{ flex: 1 }} onClick={() => { setShowImportModal(false); setImportComparison(null); }}>{t('common.cancel')}</button>
              {!importComparison ? (
                <button className="btn btn-primary" style={{ flex: 1 }} onClick={handleCompareImport} disabled={!importText.trim()}>{t('deck.compare')}</button>
              ) : (
                <button className="btn btn-primary" style={{ flex: 1 }} onClick={handleImportDeck}>{t('deck.importMatched')}</button>
              )}
            </div>
          </div>
        </div>
      )}

      {/* E. High-Res Card Art Preview Popover */}
      {previewCard && (
        <div className="modal-overlay" style={{ position: 'fixed', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: 'rgba(0,0,0,0.8)', backdropFilter: 'blur(4px)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 1000 }} onClick={() => setPreviewCard(null)}>
          <div className="glass-panel" style={{ maxWidth: '340px', padding: '1rem', position: 'relative', textAlign: 'center', animation: 'draw-card-anim 0.25s ease-out forwards' }} onClick={e => e.stopPropagation()}>
            <button className="btn btn-secondary btn-icon-only" onClick={() => setPreviewCard(null)} style={{ position: 'absolute', top: '0.5rem', right: '0.5rem', borderRadius: '50%', zIndex: 10 }}>
              <X size={16} />
            </button>
            <CardImage
              card={previewCard}
              style={{ width: '100%', borderRadius: '10px', boxShadow: '0 8px 24px rgba(0,0,0,0.6)' }}
            />
            <h4 style={{ color: 'var(--text-strong)', margin: '0.75rem 0 0.25rem 0', fontSize: '1rem' }}>{displayName(previewCard)}</h4>
            <p style={{ color: 'var(--text-secondary)', margin: 0, fontSize: '0.75rem' }}>
              {previewCard.set_name} • #{previewCard.number} ({previewCard.rarity || 'Common'})
            </p>
          </div>
        </div>
      )}

      {/* Checkout Coverage Modal */}
      {showCheckoutModal && (
        <CheckoutWizardModal
          locationsData={checkoutLocations}
          mode={checkoutMode}
          onCancel={handleCheckoutCancel}
          onClose={() => setShowCheckoutModal(false)}
        />
      )}

      {/* Precon Search + Import Modal */}
      {showPreconModal && (
        <PreconSearchModal
          open={showPreconModal}
          onClose={() => setShowPreconModal(false)}
          onImported={(deckId) => {
            setShowPreconModal(false);
            setViewMode('detail');
            loadDeckDetails(deckId);
            fetchDecks();
          }}
          showToast={showToast}
          returnFocusRef={addDeckButtonRef}
        />
      )}

    </div>
  );
}

export default DeckBuilder;

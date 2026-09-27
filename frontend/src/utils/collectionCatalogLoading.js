import { fetchWithRetry } from './fetchWithRetry.js';

export const CATALOG_INITIAL_PAGE_SIZE = 96;
export const CATALOG_BACKGROUND_PAGE_SIZE = 2000;
export const CATALOG_BACKGROUND_CONCURRENCY = 2;
export const FAST_CATALOG_DEBOUNCE_MS = 135;
export const LIVE_CATALOG_DEBOUNCE_MS = 450;

export async function mapWithConcurrency(items, concurrency, mapper) {
  const results = new Array(items.length);
  let nextIndex = 0;
  let firstError = null;
  const worker = async () => {
    while (!firstError && nextIndex < items.length) {
      const index = nextIndex;
      nextIndex += 1;
      try {
        results[index] = await mapper(items[index], index);
      } catch (error) {
        firstError ||= error;
      }
    }
  };
  await Promise.all(Array.from(
    { length: Math.min(Math.max(1, concurrency), items.length) },
    () => worker(),
  ));
  if (firstError) throw firstError;
  return results;
}

const LOCAL_OPERATORS = new Set([
  // Keep in lockstep with backend/oracleTags.js. Operators that can make
  // Scryfall implicitly include extras must use the live path.
  'color', 'identity', 'rarity', 'lang', 'mv',
  'include', 'unique', 'order', 'sort', 'direction', 'prefer', 'display',
]);

export function catalogDebounceMs(operators = []) {
  return operators.every(operator => operator === 'otag' || LOCAL_OPERATORS.has(operator))
    ? FAST_CATALOG_DEBOUNCE_MS
    : LIVE_CATALOG_DEBOUNCE_MS;
}

export function catalogRowsForQuery(state, queryKey) {
  return state.queryKey === queryKey ? state.rows : null;
}

export function reconcileCollectionHydration(mode, key, current) {
  const state = current || { key: null, status: 'idle' };
  if (mode === 'catalog') {
    if (state.status === 'loading' || state.status === 'partial') {
      return { action: 'abort', state: { ...state, status: 'aborted' } };
    }
    return { action: 'none', state };
  }

  const needsHydration = state.key !== key
    || ['idle', 'aborted', 'partial', 'error'].includes(state.status);
  if (!needsHydration) return { action: 'none', state };
  return { action: 'start', state: { key, status: 'loading' } };
}

export function abortMatchingCollectionHydration(current, { key, generation, controller }) {
  if (!controller
    || current?.key !== key
    || current?.generation !== generation
    || current?.controller !== controller
    || !['loading', 'partial'].includes(current.status)) {
    return current;
  }
  return { ...current, status: 'aborted' };
}

export class CatalogLoadError extends Error {
  constructor(message, { stage, status = null, payload = null, cause = null } = {}) {
    super(message, cause ? { cause } : undefined);
    this.name = 'CatalogLoadError';
    this.stage = stage;
    this.status = status;
    this.payload = payload;
  }
}

function abortError(signal) {
  return signal?.reason || new DOMException('Aborted', 'AbortError');
}

function ensureActive(signal, isCurrent) {
  if (signal?.aborted || !isCurrent()) throw abortError(signal);
}

function buildUrl(query, page, limit, includeTotal, snapshot = null) {
  const params = new URLSearchParams({
    scope: 'collection',
    q: query,
    page: String(page),
    limit: String(limit),
    count: includeTotal ? '1' : '0',
  });
  if (snapshot) params.set('snapshot', snapshot);
  return `/api/search?${params.toString()}`;
}

async function requestPage({ query, page, limit, stage, signal, request, isCurrent, includeTotal = false, snapshot = null }) {
  ensureActive(signal, isCurrent);
  let response;
  try {
    response = await request(buildUrl(query, page, limit, includeTotal, snapshot), { signal });
  } catch (error) {
    if (error?.name === 'AbortError' || signal?.aborted || !isCurrent()) throw abortError(signal);
    throw new CatalogLoadError('Catalog page request failed', { stage, cause: error });
  }
  ensureActive(signal, isCurrent);

  if (!response.ok) {
    const payload = await response.json().catch(() => ({}));
    ensureActive(signal, isCurrent);
    throw new CatalogLoadError(`HTTP ${response.status}`, {
      stage,
      status: response.status,
      payload,
    });
  }

  const rows = await response.json();
  ensureActive(signal, isCurrent);
  const responseSnapshot = response.headers.get('X-Catalog-Snapshot');
  if (!responseSnapshot || (snapshot && responseSnapshot !== snapshot)) {
    throw new CatalogLoadError('Catalog snapshot changed', {
      stage,
      status: 409,
      payload: { error: 'CATALOG_SNAPSHOT_CHANGED' },
    });
  }
  return {
    rows: Array.isArray(rows) ? rows : [],
    complete: response.headers.get('X-Catalog-Complete') !== '0',
    cacheStatus: response.headers.get('X-Catalog-Cache'),
    totalHeader: response.headers.get('X-Total-Count'),
    snapshot: responseSnapshot,
  };
}

export function dedupeCatalogPages(pages) {
  const seen = new Set();
  const rows = [];
  for (const page of pages) {
    for (const row of page) {
      const id = row?.entry_id;
      if (seen.has(id)) continue;
      seen.add(id);
      rows.push(row);
    }
  }
  return rows;
}

export async function loadCatalogCollection({
  query,
  signal,
  isCurrent = () => true,
  request = fetchWithRetry,
  onFirstPage = () => {},
  onIncomplete = () => {},
  snapshotRestarts = 1,
}) {
  const first = await requestPage({
    query,
    page: 1,
    limit: CATALOG_INITIAL_PAGE_SIZE,
    stage: 'first',
    signal,
    request,
    isCurrent,
    includeTotal: true,
  });
  const parsedTotal = Number.parseInt(first.totalHeader, 10);
  const total = Number.isFinite(parsedTotal) ? parsedTotal : first.rows.length;
  const needsBackground = first.rows.length < total;
  const firstState = {
    rows: first.rows,
    total,
    incomplete: needsBackground || !first.complete,
    cacheStatus: first.cacheStatus,
  };
  ensureActive(signal, isCurrent);
  onFirstPage(firstState);

  if (!needsBackground) return firstState;

  // Paging is 1-based over the requested limit. Re-fetching page 1 at the
  // larger size is intentional: it covers rows 97..2000 without relying on an
  // offset parameter, and stable entry_id de-duplication removes the overlap.
  const pageCount = Math.ceil(total / CATALOG_BACKGROUND_PAGE_SIZE);
  const pages = new Array(pageCount);
  try {
    const pageIndexes = Array.from({ length: pageCount }, (_, index) => index);
    const loadedPages = await mapWithConcurrency(
      pageIndexes,
      CATALOG_BACKGROUND_CONCURRENCY,
      index => requestPage({
          query,
          page: index + 1,
          limit: CATALOG_BACKGROUND_PAGE_SIZE,
          stage: 'background',
          signal,
          request,
          isCurrent,
          includeTotal: false,
          snapshot: first.snapshot,
        }),
    );
    loadedPages.forEach((page, index) => { pages[index] = page; });
  } catch (error) {
    if (error?.name === 'AbortError' || signal?.aborted || !isCurrent()) throw abortError(signal);
    const failure = error instanceof CatalogLoadError
      ? error
      : new CatalogLoadError('Catalog background hydration failed', { stage: 'background', cause: error });
    if (failure.status === 409 && snapshotRestarts > 0) {
      ensureActive(signal, isCurrent);
      return loadCatalogCollection({
        query, signal, isCurrent, request, onFirstPage, onIncomplete,
        snapshotRestarts: snapshotRestarts - 1,
      });
    }
    ensureActive(signal, isCurrent);
    onIncomplete({ ...firstState, incomplete: true, error: failure });
    throw failure;
  }

  ensureActive(signal, isCurrent);
  const rows = dedupeCatalogPages([first.rows, ...pages.map(page => page.rows)]);
  const result = {
    rows,
    total,
    incomplete: !first.complete
      || pages.some(page => !page.complete)
      || rows.length !== total,
    cacheStatus: first.cacheStatus,
  };
  ensureActive(signal, isCurrent);
  return result;
}

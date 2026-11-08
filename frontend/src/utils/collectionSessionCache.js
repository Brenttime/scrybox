const ORDINARY_MODE = 'ordinary';
const DEFAULT_MAX_ENTRIES = 6;

function touch(map, key, value) {
  map.delete(key);
  map.set(key, value);
}

export function makeCollectionSessionQuery({ authKey, revision, tradeOnly = false }) {
  if (!authKey) throw new Error('Collection cache requires an auth session key');
  return Object.freeze({
    authKey,
    mode: ORDINARY_MODE,
    revision: String(revision),
    tradeOnly: Boolean(tradeOnly),
    queryKey: `${ORDINARY_MODE}|${String(revision)}|${tradeOnly ? 'trade' : 'all'}`,
  });
}

export function createCollectionSessionCache({ maxEntries = DEFAULT_MAX_ENTRIES } = {}) {
  if (!Number.isInteger(maxEntries) || maxEntries < 1) {
    throw new Error('maxEntries must be a positive integer');
  }

  const authScopes = new Map();
  const entries = new Map();
  let nextScopeId = 1;
  let nextGeneration = 1;

  const removeScope = (authKey, scope) => {
    authScopes.delete(authKey);
    for (const [entryKey, entry] of entries) {
      if (entry.scopeId === scope.id) entries.delete(entryKey);
    }
  };

  const evict = () => {
    while (entries.size > maxEntries) entries.delete(entries.keys().next().value);
    while (authScopes.size > maxEntries) {
      const [authKey, scope] = authScopes.entries().next().value;
      removeScope(authKey, scope);
    }
  };

  const getScope = (query, create = false) => {
    if (!query || query.mode !== ORDINARY_MODE || !query.authKey || !query.queryKey) return null;
    let scope = authScopes.get(query.authKey);
    if (!scope && create) {
      scope = {
        id: nextScopeId++,
        sets: [],
        setsReady: false,
        currentGeneration: 0,
        currentQueryKey: null,
      };
      authScopes.set(query.authKey, scope);
      evict();
    } else if (scope) {
      touch(authScopes, query.authKey, scope);
    }
    return scope;
  };

  const entryKeyFor = (scope, query) => `${scope.id}|${query.queryKey}`;

  return {
    get size() {
      return entries.size;
    },

    begin(query) {
      const scope = getScope(query, true);
      const generation = nextGeneration++;
      scope.currentGeneration = generation;
      scope.currentQueryKey = query.queryKey;
      return Object.freeze({
        authKey: query.authKey,
        scopeId: scope.id,
        queryKey: query.queryKey,
        generation,
      });
    },

    write(lease, value) {
      const scope = authScopes.get(lease?.authKey);
      if (!scope
        || scope.id !== lease.scopeId
        || scope.currentGeneration !== lease.generation
        || scope.currentQueryKey !== lease.queryKey) {
        return false;
      }

      const rows = Array.isArray(value?.rows) ? value.rows : [];
      const parsedTotal = Number.parseInt(value?.total, 10);
      const total = Math.max(rows.length, Number.isFinite(parsedTotal) ? parsedTotal : rows.length);
      const requestedStatus = value?.status || 'partial';
      const rowIds = rows.map(row => row?.entry_id);
      const hasCompleteRows = rows.length === total
        && rowIds.every(id => id !== null && id !== undefined)
        && new Set(rowIds).size === rows.length;
      const status = requestedStatus === 'complete' && !hasCompleteRows
        ? 'partial'
        : requestedStatus;
      const entry = {
        scopeId: scope.id,
        queryKey: lease.queryKey,
        rows,
        total,
        status,
        complete: status === 'complete',
        error: value?.error || null,
      };
      touch(entries, `${scope.id}|${lease.queryKey}`, entry);
      touch(authScopes, lease.authKey, scope);
      evict();
      return true;
    },

    read(query) {
      const scope = getScope(query);
      if (!scope) return null;
      const entryKey = entryKeyFor(scope, query);
      const entry = entries.get(entryKey);
      if (!entry) return null;
      touch(entries, entryKey, entry);
      return { ...entry, sets: scope.sets, setsReady: scope.setsReady };
    },

    readSets(query) {
      const scope = getScope(query);
      return scope ? { sets: scope.sets, setsReady: scope.setsReady } : null;
    },

    setSets(query, sets) {
      const scope = getScope(query, true);
      scope.sets = Array.isArray(sets) ? sets : [];
      scope.setsReady = true;
      touch(authScopes, query.authKey, scope);
      evict();
    },

    // In-place edit of a cached entry's rows (e.g. one card edited in the
    // details sheet) without starting a new hydration generation.
    patch(query, fn) {
      const scope = getScope(query);
      if (!scope) return false;
      const entry = entries.get(entryKeyFor(scope, query));
      if (!entry) return false;
      const rows = fn(entry.rows);
      if (!Array.isArray(rows)) return false;
      entry.total = Math.max(rows.length, entry.total + (rows.length - entry.rows.length));
      entry.rows = rows;
      return true;
    },

    invalidate(query) {
      const scope = getScope(query);
      if (!scope) return;
      entries.delete(entryKeyFor(scope, query));
      if (scope.currentQueryKey === query.queryKey) {
        scope.currentGeneration = nextGeneration++;
      }
    },

    invalidateAuth(authKey) {
      const scope = authScopes.get(authKey);
      if (scope) removeScope(authKey, scope);
    },

    clear() {
      authScopes.clear();
      entries.clear();
    },
  };
}

export function waitForCollectionIdle({ signal, timeout = 250, fallbackDelay = 32 } = {}) {
  if (signal?.aborted) return Promise.resolve(false);
  return new Promise(resolve => {
    let settled = false;
    let handle;
    const usesIdleCallback = typeof globalThis.requestIdleCallback === 'function';
    const finish = (active) => {
      if (settled) return;
      settled = true;
      signal?.removeEventListener('abort', onAbort);
      resolve(active);
    };
    const onAbort = () => {
      if (usesIdleCallback && typeof globalThis.cancelIdleCallback === 'function') {
        globalThis.cancelIdleCallback(handle);
      } else {
        clearTimeout(handle);
      }
      finish(false);
    };
    signal?.addEventListener('abort', onAbort, { once: true });
    handle = usesIdleCallback
      ? globalThis.requestIdleCallback(() => finish(!signal?.aborted), { timeout })
      : setTimeout(() => finish(!signal?.aborted), fallbackDelay);
  });
}

export const collectionSessionCache = createCollectionSessionCache();

// On a slow link the 2000-row background pages (~175 KB gzip each, two in
// flight) shared bandwidth with the first screen of card images: on an
// emulated Fast 3G phone the visible cards took ~7 s to appear while the
// catalog downloaded. Wait (bounded) for the images that are on screen right
// now before starting the background pages. Fast networks finish this in a
// frame; the timeout keeps filters/totals from waiting forever.
export function waitForFirstScreenImages({
  signal,
  timeout = 4000,
  images = typeof document !== 'undefined' ? Array.from(document.images) : [],
  viewportHeight = typeof window !== 'undefined' ? window.innerHeight : 0,
} = {}) {
  if (signal?.aborted) return Promise.resolve(false);
  const pending = images.filter(img => {
    if (!img || img.complete) return false;
    const rect = typeof img.getBoundingClientRect === 'function' ? img.getBoundingClientRect() : null;
    return !rect || (rect.bottom > 0 && rect.top < viewportHeight);
  });
  if (!pending.length) return Promise.resolve(true);
  return new Promise(resolve => {
    let left = pending.length;
    let settled = false;
    const cleanups = [];
    const finish = (active) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      cleanups.forEach(fn => fn());
      signal?.removeEventListener('abort', onAbort);
      resolve(active);
    };
    const onAbort = () => finish(false);
    const timer = setTimeout(() => finish(!signal?.aborted), timeout);
    signal?.addEventListener('abort', onAbort, { once: true });
    for (const img of pending) {
      const done = () => { left -= 1; if (left <= 0) finish(!signal?.aborted); };
      img.addEventListener('load', done, { once: true });
      img.addEventListener('error', done, { once: true });
      cleanups.push(() => {
        img.removeEventListener('load', done);
        img.removeEventListener('error', done);
      });
    }
  });
}

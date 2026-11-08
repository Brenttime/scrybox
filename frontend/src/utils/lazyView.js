import { lazy } from 'react';

// View components are code-split so heavy deps load on demand.
// A deploy replaces the hashed chunk files. A tab opened before the deploy
// still asks for the old names and gets "Failed to fetch dynamically imported
// module". Reload once to pick up the new index.html (guarded so a real
// outage can't loop), and retry once first for a flaky network.
// Shared by App.jsx (views) and AddCards (its per-mode panes).
const CHUNK_RELOAD_KEY = 'scrybox.chunkReloadAt';
export function isChunkError(err) {
  return /dynamically imported module|Importing a module script failed|error loading dynamically imported|Unable to preload CSS/i.test(String(err?.message || err));
}
export function reloadForNewDeploy() {
  const last = Number(sessionStorage.getItem(CHUNK_RELOAD_KEY) || 0);
  if (Date.now() - last < 30000) return false;
  sessionStorage.setItem(CHUNK_RELOAD_KEY, String(Date.now()));
  window.location.reload();
  return true;
}
export const lazyView = (load) => lazy(() => load().catch(err => {
  if (!isChunkError(err)) throw err;
  return new Promise(r => setTimeout(r, 400)).then(load).catch(err2 => {
    if (isChunkError(err2) && reloadForNewDeploy()) return new Promise(() => {});
    throw err2;
  });
}));

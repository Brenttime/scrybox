import { useEffect, useState } from 'react';

// Where search/add surfaces send cards: 'collection' or a list id (string).
// The choice is remembered across visits; a deleted list falls back to the
// collection once the list index loads.
const KEY = 'scrybox_add_dest';

export const COLLECTION = 'collection';

export function useAddDestination() {
  const [lists, setLists] = useState([]);
  const [dest, setDestState] = useState(() => {
    try { return localStorage.getItem(KEY) || COLLECTION; } catch { return COLLECTION; }
  });
  useEffect(() => {
    let cancelled = false;
    fetch('/api/lists')
      .then(r => (r.ok ? r.json() : []))
      .then(d => {
        if (cancelled) return;
        const rows = Array.isArray(d) ? d : (d.lists || []);
        setLists(rows);
        setDestState(cur => (cur === COLLECTION || rows.some(l => String(l.id) === cur) ? cur : COLLECTION));
      })
      .catch(() => {});
    return () => { cancelled = true; };
  }, []);
  const setDest = (v) => {
    const next = String(v || COLLECTION);
    setDestState(next);
    try { localStorage.setItem(KEY, next); } catch { /* private mode */ }
  };
  const list = dest === COLLECTION ? null : lists.find(l => String(l.id) === dest) || null;
  return { dest: list ? dest : COLLECTION, setDest, lists, list };
}

// Increment a list row (printing-equivalent rows merge server-side).
export async function addToList(listId, cardId, quantity) {
  const res = await fetch(`/api/lists/${encodeURIComponent(listId)}/cards/bulk`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ cards: [{ card_id: cardId, quantity }] }),
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok || !data.added) throw new Error(data.error || 'add failed');
}

// Exact inverse of addToList: decrements by the same quantity.
export async function undoListAdd(listId, cardId, quantity) {
  const res = await fetch(`/api/lists/${encodeURIComponent(listId)}/cards/bulk-remove`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ cards: [{ card_id: cardId, quantity }] }),
  });
  if (!res.ok) throw new Error('undo failed');
}

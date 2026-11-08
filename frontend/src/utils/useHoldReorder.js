import { useEffect, useRef, useState } from 'react';
import { lockGesture, unlockGesture } from './gestureLock.js';

// Press-and-hold, then drag to reorder a grid/stack of tiles (touch + mouse).
// The hold (450 ms, cancelled by >10 px of movement) keeps normal taps,
// scrolling and swipe-to-delete working. Slots are measured once when the
// drag starts; the dragged tile follows the finger while the others reflow.
const HOLD_MS = 450;
const SLOP = 10;

export function moveItem(items, from, to) {
  if (from === to || from < 0 || to < 0) return items;
  const next = items.slice();
  const [it] = next.splice(from, 1);
  next.splice(to, 0, it);
  return next;
}

export function nearestSlot(slots, x, y) {
  let best = -1;
  let bestD = Infinity;
  slots.forEach((r, i) => {
    const d = (r.left + r.width / 2 - x) ** 2 + (r.top + r.height / 2 - y) ** 2;
    if (d < bestD) { bestD = d; best = i; }
  });
  return best;
}

export function useHoldReorder({ items, getId, setItems, onCommit, enabled = true }) {
  const els = useRef(new Map());
  const g = useRef(null);
  const latest = useRef(items);
  latest.current = items;
  const [dragId, setDragId] = useState(null);
  const [offset, setOffset] = useState({ x: 0, y: 0 });

  // While dragging, a non-passive touchmove blocks the page from scrolling.
  useEffect(() => {
    if (dragId == null) return undefined;
    const stop = (e) => e.preventDefault();
    lockGesture('hold-reorder');
    document.addEventListener('touchmove', stop, { passive: false });
    return () => {
      document.removeEventListener('touchmove', stop);
      unlockGesture('hold-reorder');
    };
  }, [dragId]);

  const cancel = () => {
    if (g.current?.timer) clearTimeout(g.current.timer);
    g.current = null;
  };

  const start = (id, e) => {
    if (!enabled || (e.pointerType === 'mouse' && e.button !== 0)) return;
    cancel();
    const el = els.current.get(id);
    const state = { id, pid: e.pointerId, sx: e.clientX, sy: e.clientY, x: e.clientX, y: e.clientY, armed: false };
    state.timer = setTimeout(() => {
      const list = latest.current;
      const slots = list.map(it => els.current.get(getId(it))?.getBoundingClientRect()).filter(Boolean);
      if (!el || slots.length !== list.length) { cancel(); return; }
      const r = el.getBoundingClientRect();
      lockGesture('hold-reorder');
      Object.assign(state, { armed: true, slots, gx: state.x - r.left, gy: state.y - r.top });
      try { el.setPointerCapture?.(state.pid); } catch { /* pointer already gone */ }
      navigator.vibrate?.(10);
      setDragId(id);
      setOffset({ x: 0, y: 0 });
    }, HOLD_MS);
    g.current = state;
  };

  const move = (e) => {
    const s = g.current;
    if (!s || s.pid !== e.pointerId) return;
    s.x = e.clientX; s.y = e.clientY;
    if (!s.armed) {
      if (Math.abs(s.x - s.sx) > SLOP || Math.abs(s.y - s.sy) > SLOP) cancel();
      return;
    }
    e.preventDefault();
    const list = latest.current;
    const from = list.findIndex(it => getId(it) === s.id);
    const to = nearestSlot(s.slots, s.x, s.y);
    const next = moveItem(list, from, to);
    if (next !== list) setItems(next);
    const slot = s.slots[to >= 0 ? to : from];
    setOffset({ x: s.x - s.gx - slot.left, y: s.y - s.gy - slot.top });
  };

  const end = (e) => {
    const s = g.current;
    if (!s || s.pid !== e.pointerId) return;
    cancel();
    if (!s.armed) return;
    // Swallow the click that follows the release so the tile doesn't open.
    const swallow = (ev) => { ev.stopPropagation(); ev.preventDefault(); };
    window.addEventListener('click', swallow, { capture: true, once: true });
    setTimeout(() => window.removeEventListener('click', swallow, { capture: true }), 300);
    setDragId(null);
    setOffset({ x: 0, y: 0 });
    onCommit(latest.current);
  };

  const bind = (id) => ({
    ref: (el) => { if (el) els.current.set(id, el); else els.current.delete(id); },
    onPointerDown: (e) => start(id, e),
    onPointerMove: move,
    onPointerUp: end,
    onPointerCancel: (e) => { if (!g.current?.armed) cancel(); else end(e); },
    onContextMenu: (e) => { if (g.current) e.preventDefault(); },
    className: `hold-reorder${dragId === id ? ' is-dragging' : ''}`,
    style: dragId === id ? { transform: `translate(${offset.x}px, ${offset.y}px) scale(1.03)` } : undefined,
  });

  return { bind, dragging: dragId != null };
}

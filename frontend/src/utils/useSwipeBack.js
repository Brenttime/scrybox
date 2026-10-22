import { useEffect, useRef } from 'react';

// Swipe right to go back a level (open deck/list -> that tab's index).
// Installed PWAs (iOS standalone) have no browser edge-swipe, so we do it
// ourselves with touch events. A small chevron disc follows the finger and
// commits once the drag passes TRIGGER px.
//
// Skips: drags that start inside position:fixed layers (modals, sheets, bars),
// inputs, or horizontal scrollers that can still scroll left; mostly-vertical
// drags (axis lock at LOCK px). In regular iOS Safari the first EDGE px are left
// to the native back swipe, which already pops our useBackGuard entry.

const LOCK = 10;
const TRIGGER = 90;
const EDGE = 24;

const isIOSBrowserTab = () => {
  if (typeof window === 'undefined') return false;
  const ios = /iP(hone|ad|od)/.test(navigator.userAgent)
    || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
  const standalone = window.navigator.standalone
    || window.matchMedia?.('(display-mode: standalone)').matches;
  return ios && !standalone;
};

function blocked(target) {
  for (let el = target; el && el !== document.body; el = el.parentElement) {
    if (el.matches?.('input, textarea, select, [contenteditable="true"], [data-no-swipe-back]')) return true;
    const cs = getComputedStyle(el);
    // The app shell itself is fixed; only fixed layers *inside* <main>'s
    // ancestors chain beyond it (modals, sheets, bars) block the gesture.
    if (cs.position === 'fixed' && !el.querySelector('main')) return true;
    if ((cs.overflowX === 'auto' || cs.overflowX === 'scroll') && el.scrollWidth > el.clientWidth && el.scrollLeft > 0) return true;
  }
  return false;
}

function makeIndicator() {
  const el = document.createElement('div');
  el.className = 'swipe-back-indicator';
  el.setAttribute('aria-hidden', 'true');
  el.innerHTML = '<svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="m15 18-6-6 6-6"/></svg>';
  document.body.appendChild(el);
  return el;
}

export function useSwipeBack(enabled, onBack) {
  const onBackRef = useRef(onBack);
  onBackRef.current = onBack;

  useEffect(() => {
    if (!enabled) return undefined;
    const skipEdge = isIOSBrowserTab();
    let s = null; // { x, y, mode: null|'h'|'v', dx }
    let ind = null;

    const show = (dx) => {
      if (!ind) ind = makeIndicator();
      const p = Math.min(dx / TRIGGER, 1);
      ind.style.opacity = String(p);
      ind.style.transform = `translate(${Math.min(dx, TRIGGER) * 0.6 - 44}px, -50%) scale(${0.7 + 0.3 * p})`;
      ind.classList.toggle('ready', dx >= TRIGGER);
    };
    const hide = () => { if (ind) { ind.remove(); ind = null; } };

    const start = (e) => {
      if (e.touches.length !== 1) { s = null; return; }
      const t = e.touches[0];
      if (skipEdge && t.clientX < EDGE) { s = null; return; }
      if (blocked(e.target)) { s = null; return; }
      s = { x: t.clientX, y: t.clientY, mode: null, dx: 0 };
    };
    const move = (e) => {
      if (!s) return;
      const t = e.touches[0];
      const dx = t.clientX - s.x;
      const dy = t.clientY - s.y;
      if (!s.mode) {
        if (Math.abs(dx) < LOCK && Math.abs(dy) < LOCK) return;
        s.mode = dx > 0 && Math.abs(dx) > Math.abs(dy) * 1.5 ? 'h' : 'v';
      }
      if (s.mode !== 'h') return;
      s.dx = Math.max(0, dx);
      show(s.dx);
    };
    const end = () => {
      const fire = s && s.mode === 'h' && s.dx >= TRIGGER;
      s = null;
      hide();
      if (fire) onBackRef.current?.();
    };

    document.addEventListener('touchstart', start, { passive: true });
    document.addEventListener('touchmove', move, { passive: true });
    document.addEventListener('touchend', end);
    document.addEventListener('touchcancel', end);
    return () => {
      document.removeEventListener('touchstart', start);
      document.removeEventListener('touchmove', move);
      document.removeEventListener('touchend', end);
      document.removeEventListener('touchcancel', end);
      hide();
    };
  }, [enabled]);
}

import { useEffect, useRef } from 'react';

// Swipe right to go back a level (open deck/list -> that tab's index), the way
// an iOS navigation stack does it: the page tracks the finger 1:1 as a card
// with a shadow over a dimmed backdrop. Let go past ~1/3 of the width (or with
// a quick flick) and it glides off to the right, then the index eases in from
// the left with a slight parallax. Otherwise it springs back.
//
// Installed PWAs (iOS standalone) have no browser edge-swipe, so we do it
// ourselves. Skips drags that start inside fixed layers (modals, sheets, bars;
// the fixed app shell itself is ignored), inputs, horizontal scrollers that can
// still scroll left, and mostly-vertical drags. In regular iOS Safari the first
// EDGE px are left to the native back swipe.

const LOCK = 8;
const EDGE = 24;
const COMMIT_FRACTION = 0.33;
const FLICK_VELOCITY = 0.45; // px/ms
const EASE = 'cubic-bezier(0.22, 0.8, 0.24, 1)';

const isIOSBrowserTab = () => {
  if (typeof window === 'undefined') return false;
  const ios = /iP(hone|ad|od)/.test(navigator.userAgent)
    || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
  const standalone = window.navigator.standalone
    || window.matchMedia?.('(display-mode: standalone)').matches;
  return ios && !standalone;
};

const reducedMotion = () => window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;

function blocked(target) {
  for (let el = target; el && el !== document.body; el = el.parentElement) {
    if (el.matches?.('input, textarea, select, [contenteditable="true"], [data-no-swipe-back]')) return true;
    const cs = getComputedStyle(el);
    if (cs.position === 'fixed' && !el.querySelector('main')) return true;
    if ((cs.overflowX === 'auto' || cs.overflowX === 'scroll') && el.scrollWidth > el.clientWidth && el.scrollLeft > 0) return true;
  }
  return false;
}

const page = () => document.querySelector('main .view-transition');

export function useSwipeBack(enabled, onBack) {
  const onBackRef = useRef(onBack);
  onBackRef.current = onBack;

  useEffect(() => {
    if (!enabled) return undefined;
    const skipEdge = isIOSBrowserTab();
    let s = null;
    let shade = null;
    let busy = false;

    // The shade lives next to the page inside the fixed app shell, so it shares
    // its stacking context and sits just under the sliding card.
    const setShade = (p, el) => {
      if (!shade) {
        shade = document.createElement('div');
        shade.className = 'swipe-back-shade';
        el.parentElement.insertBefore(shade, el);
      }
      shade.style.opacity = String(0.5 * (1 - p));
    };
    const dropShade = () => { if (shade) { shade.remove(); shade = null; } };
    const clear = (el) => {
      if (el) {
        el.classList.remove('swipe-back-card');
        el.style.transform = '';
        el.style.transition = '';
        el.style.opacity = '';
      }
      dropShade();
    };
    const after = (el, ms, fn) => {
      let done = false;
      const go = () => { if (!done) { done = true; el.removeEventListener('transitionend', go); fn(); } };
      el.addEventListener('transitionend', go);
      setTimeout(go, ms + 60);
    };

    const start = (e) => {
      if (busy || e.touches.length !== 1) { s = null; return; }
      const t = e.touches[0];
      if (skipEdge && t.clientX < EDGE) { s = null; return; }
      if (blocked(e.target)) { s = null; return; }
      s = { x: t.clientX, y: t.clientY, mode: null, dx: 0, lastX: t.clientX, lastT: e.timeStamp, v: 0, el: null };
    };

    const move = (e) => {
      if (!s) return;
      const t = e.touches[0];
      const dx = t.clientX - s.x;
      const dy = t.clientY - s.y;
      if (!s.mode) {
        if (Math.abs(dx) < LOCK && Math.abs(dy) < LOCK) return;
        s.mode = dx > 0 && Math.abs(dx) > Math.abs(dy) * 1.3 ? 'h' : 'v';
        if (s.mode === 'h') {
          s.el = page();
          if (!s.el) { s = null; return; }
          s.x = t.clientX; // track from here so the page doesn't jump
          s.el.classList.add('swipe-back-card');
          s.el.style.transition = 'none';
        }
      }
      if (s.mode !== 'h') return;
      if (e.cancelable) e.preventDefault(); // no vertical scroll mid-swipe
      const dt = e.timeStamp - s.lastT;
      if (dt > 0) s.v = 0.7 * ((t.clientX - s.lastX) / dt) + 0.3 * s.v;
      s.lastX = t.clientX; s.lastT = e.timeStamp;
      s.dx = Math.max(0, t.clientX - s.x);
      s.el.style.transform = `translate3d(${s.dx}px,0,0)`;
      setShade(s.dx / window.innerWidth, s.el);
    };

    const end = () => {
      const st = s;
      s = null;
      if (!st || st.mode !== 'h' || !st.el) return;
      const el = st.el;
      const w = window.innerWidth;
      const commit = st.dx > w * COMMIT_FRACTION || (st.v > FLICK_VELOCITY && st.dx > 30);
      if (reducedMotion()) { clear(el); if (commit) onBackRef.current?.(); return; }
      busy = true;
      if (!commit) {
        const ms = 260;
        el.style.transition = `transform ${ms}ms ${EASE}`;
        if (shade) { shade.style.transition = `opacity ${ms}ms ${EASE}`; shade.style.opacity = '0.5'; }
        el.style.transform = 'translate3d(0,0,0)';
        after(el, ms, () => { clear(el); busy = false; });
        return;
      }
      // Finish at the release speed, clamped to stay snappy.
      const ms = Math.round(Math.min(300, Math.max(160, (w - st.dx) / Math.max(st.v, 1.2))));
      el.style.transition = `transform ${ms}ms ${EASE}`;
      if (shade) { shade.style.transition = `opacity ${ms}ms ${EASE}`; shade.style.opacity = '0'; }
      el.style.transform = `translate3d(${w}px,0,0)`;
      after(el, ms, () => {
        // Swap to the index while off-screen, then ease it in from the left.
        dropShade();
        el.style.transition = 'none';
        el.style.transform = `translate3d(${-w * 0.28}px,0,0)`;
        el.style.opacity = '0.6';
        onBackRef.current?.();
        requestAnimationFrame(() => requestAnimationFrame(() => {
          el.style.transition = `transform 300ms ${EASE}, opacity 300ms ${EASE}`;
          el.style.transform = 'translate3d(0,0,0)';
          el.style.opacity = '1';
          after(el, 300, () => { clear(el); busy = false; });
        }));
      });
    };

    document.addEventListener('touchstart', start, { passive: true });
    document.addEventListener('touchmove', move, { passive: false });
    document.addEventListener('touchend', end);
    document.addEventListener('touchcancel', end);
    return () => {
      document.removeEventListener('touchstart', start);
      document.removeEventListener('touchmove', move);
      document.removeEventListener('touchend', end);
      document.removeEventListener('touchcancel', end);
      // onBack flips `enabled` off mid-animation: the queued ease-in still
      // runs and cleans up after itself, so only reset when idle.
      if (!busy) clear(null);
    };
  }, [enabled]);
}

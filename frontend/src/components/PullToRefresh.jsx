import { useEffect, useRef, useState } from 'react';
import { RotateCw } from 'lucide-react';
import { getScrollTop, scrollRoot } from '../utils/scrollRoot';

// Pull-to-refresh for the whole app. Browsers' own version is off here
// (overscroll-behavior: none keeps the header from rubber-banding) and an
// installed PWA never has one, so it is done by hand: at the very top of the
// page, drag down past THRESHOLD and let go to reload. The app remembers the
// open tab, so a reload lands back where you were.
//
// A pull that starts inside a modal/drawer (anything position:fixed) or inside
// an inner scroller that is not at its own top is ignored, so scrolling a list
// back up never triggers it.
const THRESHOLD = 80;
const MAX = 130;

function pullAllowed(target) {
  if (getScrollTop() > 0) return false;
  const shell = scrollRoot();
  for (let el = target; el && el !== document.body && el !== shell; el = el.parentElement) {
    const cs = getComputedStyle(el);
    if (cs.position === 'fixed') return false;
    if (el.scrollTop > 0 && /(auto|scroll)/.test(cs.overflowY)) return false;
  }
  return true;
}

export default function PullToRefresh() {
  const [pull, setPull] = useState(0);
  const [refreshing, setRefreshing] = useState(false);
  const start = useRef(null);
  const pullRef = useRef(0);

  useEffect(() => {
    const onStart = (e) => {
      if (e.touches.length !== 1 || !pullAllowed(e.target)) { start.current = null; return; }
      start.current = { y: e.touches[0].clientY, x: e.touches[0].clientX };
    };
    const onMove = (e) => {
      if (!start.current) return;
      const dy = e.touches[0].clientY - start.current.y;
      const dx = Math.abs(e.touches[0].clientX - start.current.x);
      if (dy <= 0 || dx > dy || getScrollTop() > 0) { pullRef.current = 0; setPull(0); return; }
      // Resistance so it feels like a rubber band, not a 1:1 drag.
      const p = Math.min(MAX, dy * 0.5);
      pullRef.current = p;
      setPull(p);
      if (e.cancelable && p > 8) e.preventDefault();
    };
    const onEnd = () => {
      if (!start.current) return;
      start.current = null;
      if (pullRef.current >= THRESHOLD) {
        setRefreshing(true);
        setPull(THRESHOLD);
        setTimeout(() => window.location.reload(), 150);
      } else {
        setPull(0);
      }
      pullRef.current = 0;
    };
    window.addEventListener('touchstart', onStart, { passive: true });
    window.addEventListener('touchmove', onMove, { passive: false });
    window.addEventListener('touchend', onEnd, { passive: true });
    window.addEventListener('touchcancel', onEnd, { passive: true });
    return () => {
      window.removeEventListener('touchstart', onStart);
      window.removeEventListener('touchmove', onMove);
      window.removeEventListener('touchend', onEnd);
      window.removeEventListener('touchcancel', onEnd);
    };
  }, []);

  if (!pull && !refreshing) return null;
  const ready = pull >= THRESHOLD;
  return (
    <div
      className={`ptr-indicator${ready ? ' ptr-ready' : ''}${refreshing ? ' ptr-spinning' : ''}`}
      style={{ transform: `translate(-50%, ${pull - 44}px)`, opacity: Math.min(1, pull / 40) }}
      aria-hidden="true"
    >
      <RotateCw size={20} style={{ transform: refreshing ? undefined : `rotate(${pull * 3}deg)` }} />
    </div>
  );
}

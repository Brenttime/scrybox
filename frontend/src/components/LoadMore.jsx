import { useEffect, useRef } from 'react';
import { scrollRoot, isShell } from '../utils/scrollRoot';

// A "load more" button that also fires by itself when you scroll down to it,
// so reaching the end of a list just keeps going. The button stays for
// keyboard users and as a fallback where IntersectionObserver is missing.
// `busy` blocks re-entry while a page is loading; the observer re-arms after.
export default function LoadMore({ onLoad, busy, children, style }) {
  const ref = useRef(null);
  const cb = useRef(onLoad);
  cb.current = onLoad;

  useEffect(() => {
    const el = ref.current;
    if (!el || busy || typeof IntersectionObserver === 'undefined') return undefined;
    const io = new IntersectionObserver(entries => {
      if (entries.some(e => e.isIntersecting)) { io.disconnect(); cb.current(); }
    }, { root: (() => { const r = scrollRoot(); return isShell(r) ? r : null; })(), rootMargin: '0px 0px 400px 0px' });
    io.observe(el);
    return () => io.disconnect();
  }, [busy]);

  return (
    <div ref={ref} style={{ display: 'flex', justifyContent: 'center', margin: '1.5rem 0', ...style }}>
      <button type="button" className="btn btn-secondary" disabled={busy} onClick={() => onLoad()}>
        {children}
      </button>
    </div>
  );
}

// Dual-screen support (Surface Duo / Duo 2, and other devices that report
// viewport segments). One screen: nothing changes; the regular phone layout
// already fits. Spanned across both screens side by side: the app runs as a
// phone on the LEFT screen and the RIGHT screen hosts sheets and dialogs
// (card details etc.) that are portaled to <body>, so nothing sits under the
// hinge.
//
// How: the viewport is ~1114px wide when spanned, so the CSS width media
// queries would pick the desktop layout. We re-evaluate every width-only
// media rule against the LEFT segment's width (CSSOM mediaText rewrite,
// originals kept for restore), confine #root to the left segment (its
// transform makes it the containing block for the app's own fixed bars),
// and move body-level overlays to the right segment via CSS variables.

const orig = new WeakMap();

export function evalWidthQuery(text, width) {
  const t = text.replace(/\s+/g, ' ').trim();
  let m = t.match(/^\(max-width: ?(\d+)px\)$/) || t.match(/^\(width <= ?(\d+)px\)$/);
  if (m) return width <= Number(m[1]);
  m = t.match(/^\(min-width: ?(\d+)px\)$/) || t.match(/^\(width >= ?(\d+)px\)$/);
  if (m) return width >= Number(m[1]);
  m = t.match(/^\(width < ?(\d+)px\)$/);
  if (m) return width < Number(m[1]);
  m = t.match(/^\(width > ?(\d+)px\)$/);
  if (m) return width > Number(m[1]);
  return null;
}

// Two side-by-side segments -> { left, hinge } in CSS px; otherwise null.
export function sideBySide(segments) {
  if (!segments || segments.length !== 2) return null;
  const [a, b] = [...segments].sort((x, y) => x.left - y.left);
  if (Math.abs(a.top - b.top) > 1 || b.left <= a.left) return null;
  return { left: Math.round(a.width), hinge: Math.max(0, Math.round(b.left - a.right)) };
}

function eachMediaRule(fn) {
  for (const sheet of document.styleSheets) {
    let rules;
    try { rules = sheet.cssRules; } catch { continue; }
    for (const r of rules) if (typeof CSSMediaRule !== 'undefined' && r instanceof CSSMediaRule) fn(r);
  }
}

function rewrite(width) {
  eachMediaRule((r) => {
    const base = orig.has(r) ? orig.get(r) : r.media.mediaText;
    const hit = evalWidthQuery(base, width);
    if (hit === null) return;
    if (!orig.has(r)) orig.set(r, base);
    const next = hit ? 'all' : 'not all';
    if (r.media.mediaText !== next) r.media.mediaText = next;
  });
}

function restore() {
  eachMediaRule((r) => {
    if (orig.has(r)) { r.media.mediaText = orig.get(r); orig.delete(r); }
  });
}

export function installDualScreen(win = window) {
  if (!win?.document) return () => {};
  const doc = win.document;
  let active = null;
  const segments = () => win.viewport?.segments || win.visualViewport?.segments || null;

  const apply = () => {
    const s = sideBySide(segments());
    const key = s ? `${s.left}/${s.hinge}` : null;
    if (key === active) { if (s) rewrite(s.left); return; }
    active = key;
    const html = doc.documentElement;
    if (s) {
      html.style.setProperty('--duo-left-w', `${s.left}px`);
      html.style.setProperty('--duo-right-x', `${s.left + s.hinge}px`);
      html.classList.add('duo-span');
      rewrite(s.left);
    } else {
      html.classList.remove('duo-span');
      html.style.removeProperty('--duo-left-w');
      html.style.removeProperty('--duo-right-x');
      restore();
    }
    win.dispatchEvent(new Event('resize'));
  };

  apply();
  win.addEventListener('resize', apply);
  // Lazy route chunks add stylesheets later; rewrite those too.
  const mo = new MutationObserver(() => { if (active) apply(); });
  mo.observe(doc.head, { childList: true });
  return () => { win.removeEventListener('resize', apply); mo.disconnect(); };
}

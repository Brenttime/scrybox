import { useSwipeBack } from '../utils/useSwipeBack';

// One swipe-to-close for every menu, sheet, modal and drawer: the same
// right-swipe the card details modal uses, applied to whichever overlay is on
// top. Card details keep their own hook (.ci-overlay is skipped here).
const LAYERS = '.fs-sheet-backdrop, .modal-overlay:not(.ci-overlay), .quick-add-drawer.open';

const topLayer = () => {
  const all = document.querySelectorAll(LAYERS);
  return all.length ? all[all.length - 1] : null;
};

// Close it the way a tap would: an explicit close button, else the backdrop.
function closeLayer(el) {
  if (!el) return;
  if (el.matches('.quick-add-drawer')) {
    const backdrop = el.previousElementSibling?.matches('.drawer-backdrop') ? el.previousElementSibling : null;
    (backdrop || el.querySelector('.modal-close, [aria-label="Close"], [aria-label="Cancel"]'))?.click();
  } else if (el.matches('.modal-overlay')) {
    const btn = el.querySelector(':scope > * > .modal-close, :scope > .modal-close');
    if (btn) btn.click(); else el.click();
  } else {
    el.click();
  }
  // A layer that refuses to close (e.g. busy) must not stay slid off-screen.
  setTimeout(() => {
    if (el.isConnected) {
      el.classList.remove('swipe-back-card');
      el.style.transition = '';
      el.style.transform = '';
    }
  }, 350);
}

const OPTS = { el: topLayer };

export default function SwipeToCloseLayers() {
  useSwipeBack(true, () => closeLayer(topLayer()), OPTS);
  return null;
}

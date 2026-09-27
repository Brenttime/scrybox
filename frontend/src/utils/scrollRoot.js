// The element that actually scrolls the page.
//
// On phones (<=768px) the app is one fixed full-screen frame (.app-container,
// see index.css "app shell"): the header and tab bar never move because the
// document itself never scrolls; only the frame's contents do. iOS placed
// position:fixed/sticky bars against a stale viewport during toolbar resize
// and rubber-banding, which let both bars drift. On desktop the window scrolls
// as before.
export function scrollRoot() {
  if (typeof document === 'undefined') return null;
  const shell = document.querySelector('.app-container');
  if (shell && getComputedStyle(shell).overflowY !== 'visible' && shell.scrollHeight > 0
      && getComputedStyle(shell).position === 'fixed') return shell;
  return document.scrollingElement || document.documentElement;
}
export const isShell = (el) => el && el !== document.scrollingElement && el !== document.documentElement;
export const getScrollTop = () => scrollRoot()?.scrollTop || 0;
export const getViewportHeight = () => { const r = scrollRoot(); return isShell(r) ? r.clientHeight : window.innerHeight; };
export const getScrollHeight = () => scrollRoot()?.scrollHeight || 0;
// Document-space top of an element, relative to the scroller's content box.
export const offsetInScroller = (el) => {
  const r = scrollRoot();
  const top = el.getBoundingClientRect().top;
  return isShell(r) ? top - r.getBoundingClientRect().top + r.scrollTop : top + window.scrollY;
};
export function scrollToTop(y = 0) {
  const r = scrollRoot();
  if (isShell(r)) r.scrollTop = y; else window.scrollTo(window.scrollX, y);
}

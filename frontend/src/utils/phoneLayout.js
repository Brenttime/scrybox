// Mirrors the CSS phone-layout media query in index.css: narrow screens, plus
// touch screens up to 940px (iPhone Duo unfolded upright is 669 wide).
export const PHONE_QUERY = '(max-width: 768px), (pointer: coarse) and (max-width: 940px)';

// iPhone Duo (and any short, wide touch screen): iOS 27 moves bars to a
// vertical column on the right edge. Measured CSS viewports (WebKit, Safari
// 27.1; Home Screen app in brackets): closed 382x580 [466x657], closed
// sideways 594x368 [678x445], open sideways 951x585 [951x648]. Open upright
// (669x867) keeps the bottom bar, as iOS does. Mirrored in theme-manabox.css.
export const SIDE_RAIL_QUERY = [
  '(pointer: coarse) and (max-width: 560px) and (min-aspect-ratio: 3/5)',
  '(pointer: coarse) and (max-width: 940px) and (max-height: 500px) and (orientation: landscape)',
  '(pointer: coarse) and (min-width: 941px) and (max-width: 1023px) and (max-height: 760px)',
].join(', ');

// The open Duo held sideways: one touch canvas with the hinge at x = 50%.
export const FOLD_SPREAD_QUERY = '(pointer: coarse) and (min-width: 941px) and (max-width: 1023px) and (max-height: 760px)';

export function isFoldSpread(win = typeof window !== 'undefined' ? window : null) {
  return !!win?.matchMedia?.(FOLD_SPREAD_QUERY).matches;
}

export function isPhoneLayout(win = typeof window !== 'undefined' ? window : null) {
  return !!win?.matchMedia?.(PHONE_QUERY).matches;
}

// Width to feed width-threshold layout math (e.g. collection grid gaps, which
// switch at 769px): a phone-layout viewport counts as at most 768px wide.
export function layoutViewportWidth(win = typeof window !== 'undefined' ? window : null) {
  if (!win) return 0;
  return isPhoneLayout(win) ? Math.min(win.innerWidth, 768) : win.innerWidth;
}

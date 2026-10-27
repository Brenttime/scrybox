// Mirrors the CSS phone-layout media query in index.css: narrow screens, plus
// touch screens up to 940px (iPhone Duo unfolded is 626 x 890 / 890 x 626).
export const PHONE_QUERY = '(max-width: 768px), (pointer: coarse) and (max-width: 940px)';

export function isPhoneLayout(win = typeof window !== 'undefined' ? window : null) {
  return !!win?.matchMedia?.(PHONE_QUERY).matches;
}

// Width to feed width-threshold layout math (e.g. collection grid gaps, which
// switch at 769px): a phone-layout viewport counts as at most 768px wide.
export function layoutViewportWidth(win = typeof window !== 'undefined' ? window : null) {
  if (!win) return 0;
  return isPhoneLayout(win) ? Math.min(win.innerWidth, 768) : win.innerWidth;
}

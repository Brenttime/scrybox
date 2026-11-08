// Native shell setup (Capacitor). On Android the WebView renders edge-to-edge
// under the status bar and does NOT populate env(safe-area-inset-top), so the
// app collided with the status icons. Reserve space for the bar (overlay:false)
// and tint it to match the active theme. No-op on web; iOS uses the CSS
// safe-area insets and ignores the Android-only calls (errors swallowed).
// The plugin is imported dynamically so the web build never downloads it.
import { isNative } from './apiBase.js';

const BG = { manabox: '#121417' };

function applyStatusBar({ StatusBar, Style }) {
  const theme = document.documentElement.getAttribute('data-theme') || 'manabox';
  StatusBar.setOverlaysWebView({ overlay: false }).catch(() => {});
  StatusBar.setStyle({ style: Style.Dark }).catch(() => {});
  StatusBar.setBackgroundColor({ color: BG[theme] || BG.manabox }).catch(() => {});
}

if (isNative) {
  import('@capacitor/status-bar').then((plugin) => {
    applyStatusBar(plugin);
    // Re-tint when Settings toggles data-theme on <html>.
    new MutationObserver(() => applyStatusBar(plugin)).observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['data-theme'],
    });
  }).catch(() => {});
}

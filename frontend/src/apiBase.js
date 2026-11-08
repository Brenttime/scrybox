// Web build is served same-origin as the API, so relative `/api/...` paths work
// untouched. A native app (Capacitor) has no server origin of its own, and each
// user points it at their OWN self-hosted Scrybox instance, so we store that URL
// and prepend it to every `/api` request. CapacitorHttp (see capacitor.config)
// routes these through native HTTP, sidestepping WebView CORS entirely.

const KEY = 'bindarr_server';

// The native shell injects `window.Capacitor` (its bridge) before any page
// script runs, so we can ask it directly. Importing @capacitor/core here would
// put it in the entry chunk of every web load, where it always answers false.
export const isNative = typeof window !== 'undefined' && !!window.Capacitor?.isNativePlatform?.();
export const getServerUrl = () => localStorage.getItem(KEY) || '';
export const setServerUrl = (url) =>
  localStorage.setItem(KEY, url.trim().replace(/\/+$/, ''));
export const needsServerUrl = () => isNative && !getServerUrl();

if (isNative) {
  const orig = window.fetch.bind(window);
  window.fetch = (input, init) => {
    if (typeof input === 'string' && input.startsWith('/api')) {
      input = getServerUrl() + input;
    }
    return orig(input, init);
  };
}

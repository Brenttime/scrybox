// Which Cross-Origin-Embedder-Policy a request gets (see server.js).
// WebKit (every iOS browser; desktop Safari) has no 'credentialless', only
// 'require-corp'. iOS Chrome/Firefox/Edge are WebKit too (CriOS/FxiOS/EdgiOS).
function isWebKitOnly(ua = '') {
  if (!/AppleWebKit\//.test(ua)) return false;
  if (/iPhone|iPad|iPod/.test(ua)) return true;                         // all iOS browsers
  if (/(Chrome|Chromium|Edg|OPR|Android)\//.test(ua)) return false;    // Blink
  return /Version\/[\d.]+.*Safari\//.test(ua);                          // desktop Safari (iPadOS desktop UA too)
}

function coepFor(ua = '', env = process.env) {
  if (env.SCAN_ISOLATION === '0') return null;
  if (isWebKitOnly(ua)) return env.SCAN_ISOLATION_WEBKIT === '0' ? null : 'require-corp';
  return 'credentialless';
}

module.exports = { coepFor, isWebKitOnly };

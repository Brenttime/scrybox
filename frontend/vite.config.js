import { readFileSync } from 'node:fs'
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import basicSsl from '@vitejs/plugin-basic-ssl'

// Bake the version into the bundle. Settings used to learn its own version from
// /api/settings/version, so any hiccup on that call left it showing "Version …"
// — the app couldn't state what build it was, which is exactly what you need
// when filing a bug. The release workflow bumps this file, so it's the truth.
const pkg = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8'))

// https://vitejs.dev/config/
export default defineConfig({
  // Demo build is served from https://<user>.github.io/scrybox/, so assets need
  // that sub-path prefix. Every other build (web/mobile) stays root-relative.
  base: process.env.VITE_DEMO ? '/scrybox/' : '/',
  plugins: [react(), basicSsl()],
  // Matches how the app already reads build-time config (VITE_DEMO), so this
  // needs no new global and no eslint exception.
  define: {
    'import.meta.env.VITE_APP_VERSION': JSON.stringify(pkg.version),
    // Build stamp for scan telemetry: which bundle produced a record.
    'import.meta.env.VITE_SCAN_BUILD': JSON.stringify(process.env.SCAN_BUILD || new Date().toISOString().replace(/[-:]/g, '').slice(0, 13)),
  },
  // onnxruntime-web ships each entry point twice: a "bundle" build that hands its
  // wasm to the bundler as an asset, and an extern build that fetches the wasm at
  // runtime. The default is the bundle one, which emitted a second copy of the
  // 13.5 MB binary into dist/assets — never fetched, because detectWorker points
  // ort.env.wasm.wasmPaths at /ort/, where scripts/copy-ort.mjs stages the file the
  // server actually serves. This condition selects the extern build so there is one
  // copy of the wasm in a build instead of two.
  resolve: {
    conditions: ['onnxruntime-web-use-extern-wasm', 'module', 'browser', 'development|production'],
  },
  // Ship source maps so a minified production error (e.g. a device-only crash in
  // the Android WebView) maps back to real file:line via chrome://inspect. Repo
  // is public, so exposing sources costs nothing.
  build: {
    sourcemap: true,
  },
  server: {
    port: 5173,
    host: true,
    proxy: {
      '/api': {
        target: 'http://localhost:3001',
        changeOrigin: true,
      },
      // The in-browser detector fetches cornelius from /models and its wasm from
      // /ort. Unproxied, vite's SPA catch-all answers both with index.html, the
      // worker's content-type guard throws, and detection silently falls back to
      // the pure-JS contour detector — which is ~1 frame per second, so the green
      // outline crawls in dev and nowhere else.
      '/models': { target: 'http://localhost:3001', changeOrigin: true },
      '/ort': { target: 'http://localhost:3001', changeOrigin: true },
      // On-device Scan Cards models + index (backend CLIENT_SCAN_DIR).
      '/scan-assets': { target: 'http://localhost:3001', changeOrigin: true },
    }
  }
})

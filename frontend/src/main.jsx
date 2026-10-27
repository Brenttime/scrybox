import React from 'react'
import ReactDOM from 'react-dom/client'
import './apiBase.js' // installs native fetch shim before any request
import './nativeInit.js' // Capacitor status-bar setup (no-op on web)
import App from './App.jsx'
import { I18nProvider } from './utils/i18n.jsx'
import './index.css'
import './theme-manabox.css' // opt-in skin; every rule scoped to [data-theme="manabox"]
import { installDualScreen } from './utils/dualScreen.js'

// Demo build (GitHub Pages): install the fixture-backed fetch shim and seed a
// fake session BEFORE first render. Guard is a static env check, so a normal
// build tree-shakes the whole ./demo chunk out.
if (import.meta.env.VITE_DEMO) {
  await import('./demo/install.js')
}

// Surface Duo spanned across both screens: app on the left, dialogs on the right.
installDualScreen()

ReactDOM.createRoot(document.getElementById('root')).render(
  <React.StrictMode>
    <I18nProvider>
      <App />
    </I18nProvider>
  </React.StrictMode>,
)

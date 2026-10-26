// "Connect" for the marketplace credential panels: a visibility launcher,
// not a credential channel.
//
// Two facts decide the design:
//   - Scrybox cannot inject anything into a manapool.com page.
//     A popup has no cross-origin scripting access, period.
//   - localStorage is partitioned per SITE FOR TOP FRAME. A manapool.com popup
//     opened by Scrybox does NOT share a readable store with the Scrybox tab
//     (opaque/second-site origins), so the old injected-script + poll design
//     could never deliver a credential. It always timed out.
//
// What Connect honestly does now: open, from the click itself (so popup
// blockers are satisfied by real user activation), the page where each
// credential is DISPLAYED to the signed-in user:
//   manapool  -> the profile's Account tab, which shows the API access token
//                (Creator Studio cannot display it; the profile tab can).
// The user copies the value and pastes it into the field beside the button.
// Nothing is captured, transmitted, or polled; there is never a credential in
// Scrybox's memory. The tab is verified open in the same tick as the call --
// nothing may sit behind an await before window.open -- so callers can toast
// success only for real launches and errors only for real failures.

const CONNECT_TARGETS = {
  manapool: 'https://manapool.com/profile?tab=account',
};

const FEATURES = 'popup,width=980,height=760';

// openFn is injectable for tests; the default is the real thing.
function start(source, openFn = null) {
  const url = CONNECT_TARGETS[source];
  if (!url) return Promise.reject(new Error('UNKNOWN_SOURCE'));
  const open = openFn || ((u, f) => window.open(u, 'bindarrMarketConnect', f));
  const tab = open(url, FEATURES);           // synchronous: keeps user activation
  if (!tab || !tab.location) {
    return Promise.reject(new Error('POPUP_BLOCKED'));
  }
  return Promise.resolve({ opened: true });
}

export { start as startMarketRelay, CONNECT_TARGETS };

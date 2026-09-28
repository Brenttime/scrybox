// Scanner model start with a one-thread fallback (review iOS-COEP S1).
// A threaded start that fails can leave ORT aborted inside that worker, so the
// retry is a NEW worker at one thread, remembered for the session. Pure: the
// worker plumbing is injected, so this is testable without a browser.
//   loadOnce(threads) -> Promise<{ready, error, threads?, info?, loadMs?}>
//   retire(reason)    -> end the current worker (a fresh one is made on demand)
export function startLoad({ loadOnce, retire, override, state }) {
  return (async () => {
    let r = await loadOnce(state.forceOneThread ? '1' : override);
    const tried = r.threads ?? r.info?.threads ?? null;
    if (!r.ready && !state.forceOneThread && tried !== 1) {
      state.forceOneThread = true;
      state.threadFallback = r.error || 'failed';
      retire('threaded start failed');
      r = await loadOnce('1');
    }
    // The retry after the cooldown must get a fresh worker too.
    if (!r.ready) retire('load failed');
    const info = r.info && (state.threadFallback ? { ...r.info, threadFallback: state.threadFallback } : r.info);
    const error = r.ready ? r.error : [r.error, state.threadFallback && `after threaded start failed: ${state.threadFallback}`].filter(Boolean).join('; ');
    return { ok: !!r.ready, loadMs: r.loadMs, error, info };
  })();
}

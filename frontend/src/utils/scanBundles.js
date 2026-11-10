// Which on-device reader bundle (models + card index) reads the next frame,
// and how a newer server bundle replaces it without a page reload.
//
// Pure: loading, manifest fetch, session release and cache pruning are
// injected, so the worker (clientScanWorker.js) and the tests share it.
//
//   bundle = { manifest: {index, rec, dict}, info, reader, busy, retired, ... }
//
// Camera start calls check(): the server manifest is compared by content hash
// with the bundle that will read next. Same -> fresh (no request at all within
// freshMs of the last match, e.g. right after the first load). Different ->
// the new bundle is built in the background, bounded by updateMs (its fetches
// are aborted), while the current one keeps reading; it takes over at the next
// probe -> read transaction (begin()). A newer check for another manifest
// supersedes an older refresh; a superseded, timed-out or late bundle is
// released, never installed (review R1-B2/B3). A failed refresh keeps the
// current bundle and its cached files (pruning runs only after a swap).
export const assetsKey = (m) => (m ? `${m.index}|${m.rec}|${m.dict}` : '');

export function createBundles({ loadBundle, fetchManifest, release, prune = () => {}, now = () => Date.now(), updateMs = 90000, freshMs = 15000, setTimer = setTimeout, clearTimer = clearTimeout }) {
  let active = null, pending = null, tx = null, checkedAt = -Infinity, update = null;

  function retire(b) {
    if (!b || b.retired) return;
    b.retired = true;
    if (!b.busy) release(b);
  }

  return {
    get active() { return active; },
    get pending() { return pending; },
    // The first load's bundle.
    install(b) { active = b; checkedAt = now(); prune([b]); return b; },
    // A probe starts a transaction; a refreshed bundle takes over only here.
    begin() {
      if (pending) { retire(active); active = pending; pending = null; prune([active]); }
      tx = active;
      return active;
    },
    // The bundle the current transaction's read must use.
    current() { return tx || this.begin(); },
    async use(b, fn) {
      b.busy++;
      try { return await fn(b.reader); } finally {
        if (--b.busy === 0 && b.retired) release(b);
      }
    },
    async check() {
      const cur = pending || active;
      if (!cur) return { loaded: false };
      if (now() - checkedAt < freshMs) return { fresh: true, info: cur.info };
      const manifest = await fetchManifest();
      const key = assetsKey(manifest);
      if (assetsKey(cur.manifest) === key) { checkedAt = now(); return { fresh: true, info: cur.info }; }
      if (!update || update.key !== key) {
        update?.ctrl.abort();
        const ctrl = new AbortController();
        const timer = setTimer(() => ctrl.abort(), updateMs);
        const mine = { key, ctrl };
        mine.promise = Promise.resolve().then(() => loadBundle(manifest, ctrl.signal)).then((next) => {
          if (update !== mine || ctrl.signal.aborted) { retire(next); throw new Error('refresh superseded'); }
          retire(pending);
          pending = next;
          checkedAt = now();
          return next;
        }).finally(() => { clearTimer(timer); if (update === mine) update = null; });
        update = mine;
      }
      const next = await update.promise;
      if (assetsKey(next.manifest) !== key) return { error: 'refresh superseded' };
      return { updated: true, from: cur.info?.index, info: next.info, loadMs: next.loadMs };
    },
  };
}

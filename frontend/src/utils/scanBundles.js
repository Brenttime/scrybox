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
// released, never installed (review R1-B2/B3, R2-B2/B3 below). A failed refresh keeps the
// current bundle and its cached files (pruning runs only after a swap).
export const assetsKey = (m) => (m ? `${m.index}|${m.rec}|${m.dict}` : '');

// Review R2: every check gets a generation BEFORE its manifest request, and
// only the newest check may select a target (B3). The manifest fetch, its
// body and the wait for a build are raced against a logical deadline, so a
// check settles even if work ignores its AbortSignal; a build that misses its
// deadline is detached from the refresh slot (a same-key retry starts its own
// build) and, should it ever complete, the bundle it produced is released by
// that build, never installed (B2). A swap waits (boundedly) while the active
// reader owes the verification of a first-look guess (S3).
const timeoutError = () => new Error('refresh timed out');
const supersededError = () => new Error('refresh superseded');

export function createBundles({ loadBundle, fetchManifest, release, prune = () => {}, now = () => Date.now(), updateMs = 90000, freshMs = 15000, maxVerifyDefer = 2, setTimer = setTimeout, clearTimer = clearTimeout }) {
  let active = null, pending = null, tx = null, checkedAt = -Infinity, update = null, gen = 0;

  function retire(b) {
    if (!b || b.retired) return;
    b.retired = true;
    if (!b.busy) release(b);
  }

  // A deadline that rejects; settle() stops it.
  function deadline(onFire) {
    let reject;
    const promise = new Promise((_, rej) => { reject = rej; });
    promise.catch(() => {});
    const timer = setTimer(() => { onFire?.(); reject(timeoutError()); }, updateMs);
    return { promise, settle: () => clearTimer(timer) };
  }

  // Abandon a build: abort its work, free the slot. Its bundle, if it ever
  // arrives, is released by the build itself (it owns it until adopted).
  function cancel(u) {
    if (!u || u.cancelled) return;
    u.cancelled = true;
    u.ctrl.abort();
    u.dl.settle();
    u.fail(supersededError());
    if (update === u) update = null;
  }

  function build(manifest, key) {
    const ctrl = new AbortController();
    const u = { key, ctrl, cancelled: false };
    let fail;
    const detached = new Promise((_, rej) => { fail = rej; });
    detached.catch(() => {});
    u.fail = fail;
    u.dl = deadline(() => { if (!u.cancelled) { u.cancelled = true; ctrl.abort(); fail(timeoutError()); if (update === u) update = null; } });
    const work = Promise.resolve().then(() => loadBundle(manifest, ctrl.signal)).then((next) => {
      u.dl.settle();
      if (u.cancelled || ctrl.signal.aborted) { retire(next); throw supersededError(); }
      if (update === u) update = null;
      retire(pending);
      pending = next;
      checkedAt = now();
      return next;
    }, (err) => { u.dl.settle(); if (update === u) update = null; throw err; });
    work.catch(() => {});
    u.promise = Promise.race([work, detached]);
    u.promise.catch(() => {});
    return u;
  }

  const fresh = (b) => { checkedAt = now(); return { fresh: true, info: b.info }; };

  return {
    get active() { return active; },
    get pending() { return pending; },
    // The first load's bundle.
    install(b) { active = b; checkedAt = now(); prune([b]); return b; },
    // A probe starts a transaction; a refreshed bundle takes over only here,
    // and not while the active reader still owes a guess verification.
    begin() {
      if (pending && active?.verifyPending && (active.verifyDefers || 0) < maxVerifyDefer) {
        active.verifyDefers = (active.verifyDefers || 0) + 1;
      } else if (pending) { retire(active); active = pending; pending = null; prune([active]); }
      tx = active;
      return active;
    },
    // The bundle the current transaction's read must use.
    current() { return tx || this.begin(); },
    // Reader outcomes the swap boundary depends on (R2-S3).
    noteRead(b, out) {
      if (!b) return;
      b.verifyPending = !!out?.results?.some(r => r?.guess_first);
      if (!b.verifyPending) b.verifyDefers = 0;
    },
    noteProbe(b, quad) { if (b && !quad) { b.verifyPending = false; b.verifyDefers = 0; } },
    noteReset(b) { if (b) { b.verifyPending = false; b.verifyDefers = 0; } },
    async use(b, fn) {
      b.busy++;
      try { return await fn(b.reader); } finally {
        if (--b.busy === 0 && b.retired) release(b);
      }
    },
    async check() {
      if (!(pending || active)) return { loaded: false };
      if (now() - checkedAt < freshMs) return { fresh: true, info: (pending || active).info };
      const my = ++gen;
      const from = (pending || active).info?.index;
      const ctrl = new AbortController();
      const dl = deadline(() => ctrl.abort());
      try {
        // fetchManifest bounds the body (res.json) with the same signal; the
        // race bounds it even if it does not.
        const manifest = await Promise.race([Promise.resolve().then(() => fetchManifest(ctrl.signal)), dl.promise]);
        if (my !== gen) throw supersededError();
        const key = assetsKey(manifest);
        // Live state, re-read after the await.
        if (active && assetsKey(active.manifest) === key) {
          cancel(update);
          if (pending) { retire(pending); pending = null; }
          return fresh(active);
        }
        if (pending && assetsKey(pending.manifest) === key) { cancel(update); return fresh(pending); }
        if (!update || update.key !== key) { cancel(update); update = build(manifest, key); }
        const u = update;
        const next = await Promise.race([u.promise, dl.promise]);
        if (my !== gen || next.retired) throw supersededError();
        return { updated: true, from, info: next.info, loadMs: next.loadMs };
      } finally { dl.settle(); }
    },
  };
}

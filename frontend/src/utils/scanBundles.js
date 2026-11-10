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

// Review R2/R3: every check gets a generation BEFORE its manifest request,
// and only the newest check may select a target (B3). Each check has ONE
// end-to-end deadline, started before its manifest request and covering the
// build it waits for; the build has no deadline of its own (R3-B1). A build
// is owned by the newest check that selected it: when that check expires,
// fails or selects something else, the build is cancelled (its signal
// aborted) and, should its work ever complete, the bundle is released by the
// build, never installed. An older check that expires does not cancel a build
// a newer live check still needs. A build that completes while a newer check
// is still waiting for its manifest is held until that manifest decides
// (same target -> installed, otherwise released). A same-target retry after
// an expiry starts a fresh build. A swap waits (boundedly) while the active
// reader owes the verification of a first-look guess (S3).
const timeoutError = () => new Error('refresh timed out');
const supersededError = () => new Error('refresh superseded');

export function createBundles({ loadBundle, fetchManifest, release, prune = () => {}, now = () => Date.now(), updateMs = 90000, freshMs = 15000, maxVerifyDefer = 2, setTimer = setTimeout, clearTimer = clearTimeout }) {
  let active = null, pending = null, tx = null, checkedAt = -Infinity, update = null, gen = 0;
  let deciding = 0;   // generation of the newest check while it awaits its manifest

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

  // Abandon a build: abort its work, free the slot, release a held result.
  function cancel(u, err = supersededError()) {
    if (!u || u.cancelled || u.adopted) return;
    u.cancelled = true;
    u.ctrl.abort();
    if (u.result) retire(u.result);
    u.reject(err);
    if (update === u) update = null;
  }

  // Install a finished build, only while its owner is the newest live check.
  function adopt(u) {
    if (!u || !u.result || u.cancelled || u.adopted || u.owner !== gen || deciding) return;
    u.adopted = true;
    if (update === u) update = null;
    retire(pending);
    pending = u.result;
    checkedAt = now();
    u.resolve(u.result);
  }

  function build(manifest, key, owner) {
    const ctrl = new AbortController();
    const u = { key, ctrl, owner, cancelled: false, adopted: false, result: null };
    u.promise = new Promise((res, rej) => { u.resolve = res; u.reject = rej; });
    u.promise.catch(() => {});
    Promise.resolve().then(() => loadBundle(manifest, ctrl.signal)).then((next) => {
      if (u.cancelled || ctrl.signal.aborted) { retire(next); return; }
      u.result = next;
      adopt(u);
    }, (err) => { if (!u.cancelled) { u.cancelled = true; if (update === u) update = null; u.reject(err); } });
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
    // Reader outcomes the swap boundary depends on (R2-S3, R3-S3). The
    // obligation is the reader's own verify state (out.verify_pending), not
    // the absence of a guess in this output: a title miss or a duplicate
    // frame verifies nothing and keeps it. It clears once verified, on a
    // different card or no card, or at the maxVerifyDefer cap; the deferral
    // count is not reset while the same obligation persists.
    noteRead(b, out) {
      if (!b) return;
      b.verifyPending = !!out?.verify_pending || !!out?.results?.some(r => r?.guess_first);
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
      deciding = my;
      const from = (pending || active).info?.index;
      const ctrl = new AbortController();
      const dl = deadline(() => ctrl.abort());
      let ok = false;
      try {
        // fetchManifest bounds the body (res.json) with the same signal; the
        // race bounds it even if it does not.
        const manifest = await Promise.race([Promise.resolve().then(() => fetchManifest(ctrl.signal)), dl.promise]);
        if (my !== gen) throw supersededError();
        deciding = 0;
        const key = assetsKey(manifest);
        // Live state, re-read after the await.
        if (active && assetsKey(active.manifest) === key) {
          cancel(update);
          if (pending) { retire(pending); pending = null; }
          ok = true;
          return fresh(active);
        }
        if (pending && assetsKey(pending.manifest) === key) { cancel(update); ok = true; return fresh(pending); }
        if (update && update.key === key) update.owner = my;
        else { cancel(update); update = build(manifest, key, my); }
        const u = update;
        adopt(u);                       // a result held for this decision
        const next = await Promise.race([u.promise, dl.promise]);
        if (next.retired || (next !== pending && next !== active)) throw supersededError();
        ok = true;
        return { updated: true, from, info: next.info, loadMs: next.loadMs };
      } finally {
        dl.settle();
        // Only the newest check governs the build; on its expiry or failure
        // the build it owns goes with it (R3-B1).
        if (my === gen) {
          deciding = 0;
          if (!ok) cancel(update, timeoutError());
        }
      }
    },
  };
}

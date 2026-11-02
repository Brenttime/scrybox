import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import {
  ANCHOR_CONVERGENCE_MAX_FRAMES,
  ANCHOR_CONVERGENCE_STABLE_FRAMES,
  advanceAnchorConvergence,
  anchorConvergenceFinished,
  buildVirtualWindow,
  computeVirtualGeometry,
  findVisibleCollectionAnchor,
  measuredAnchorScrollTarget,
  resolvePendingAnchorIndex,
  translateAnchorViewportOffset,
  virtualTableRowCount,
  virtualTableRowIndex,
  virtualWindowsMatch,
} from './collectionVirtualization.js';

const collectionSource = readFileSync(
  new URL('../components/CollectionList.jsx', import.meta.url),
  'utf8',
);
const collectionCss = readFileSync(new URL('../index.css', import.meta.url), 'utf8');

const mountedCount = virtualWindow => virtualWindow.endIndex - virtualWindow.startIndex;

function assertWindowBounds(itemCount, width, viewportWidth, viewportHeight, gallery) {
  const geometry = computeVirtualGeometry(itemCount, width, viewportWidth, gallery);
  const offsets = [0, geometry.totalSize / 2, geometry.totalSize];
  return offsets.map(scrollTop => {
    const window = buildVirtualWindow(itemCount, geometry, scrollTop, viewportHeight);
    const visibleRows = Math.ceil(viewportHeight / geometry.rowStride);
    const maximumRows = visibleRows + 9;
    assert.ok(mountedCount(window) <= maximumRows * geometry.columns,
      'the mounted range must stay bounded by viewport rows plus overscan');
    assert.ok(window.startIndex >= 0 && window.endIndex <= itemCount);
    return window;
  });
}

test('collection virtualization stays viewport-bounded at top, middle, and end', () => {
  const desktopGallery = assertWindowBounds(10_000, 1272, 1440, 900, true);
  const desktopList = assertWindowBounds(10_000, 1272, 1440, 900, false);
  const mobileGallery = assertWindowBounds(10_000, 343, 375, 667, true);
  const mobileList = assertWindowBounds(10_000, 343, 375, 667, false);

  for (const windows of [desktopGallery, desktopList, mobileGallery, mobileList]) {
    assert.equal(windows[0].startIndex, 0, 'the first logical item must be reachable');
    assert.equal(windows.at(-1).endIndex, 10_000, 'the final logical item must be reachable');
    assert.ok(windows[1].startIndex > 0 && windows[1].endIndex < 10_000,
      'a middle viewport must not retain either collection end');
  }

  // These are intentionally only wiring guards; geometry behavior is exercised above.
  assert.match(collectionSource, /data-collection-virtual-spacer/);
  assert.match(collectionSource, /ResizeObserver/);
  assert.doesNotMatch(collectionSource, /visibleCount|LOAD_MORE_RENDER_COUNT|sentinelRef/);
});

test('view anchors preserve clipped progress through repeated round trips', () => {
  assert.equal(translateAnchorViewportOffset(24, 360, 82), 24);
  assert.equal(translateAnchorViewportOffset(0, 360, 82), 0);

  const galleryHeight = 306.21875;
  const listHeight = 82;
  const originalGalleryOffset = -139.4375;
  let galleryOffset = originalGalleryOffset;
  for (let roundTrip = 0; roundTrip < 20; roundTrip += 1) {
    const listOffset = translateAnchorViewportOffset(
      galleryOffset,
      galleryHeight,
      listHeight,
      900,
      1,
    );
    assert.ok(listOffset > -listHeight && listOffset < 0,
      'the clipped entry must remain positively visible in list mode');
    galleryOffset = translateAnchorViewportOffset(
      listOffset,
      listHeight,
      galleryHeight,
      900,
      1,
    );
    assert.ok(galleryOffset + galleryHeight >= 1,
      'the clipped entry must retain a device pixel in gallery mode');
  }
  assert.ok(Math.abs(galleryOffset - originalGalleryOffset) < 1e-9,
    'stable measured geometry must not accumulate round-trip drift');
});

test('view switches select an actually intersecting source entry', () => {
  const element = (entryId, top, height) => ({
    dataset: { collectionEntryId: entryId },
    getBoundingClientRect: () => ({ top, bottom: top + height, height }),
  });
  const root = {
    querySelectorAll: () => [
      element('calculated-but-clipped', -343.75, 343),
      element('measured-visible', -0.75, 343),
      element('later-in-row', -0.75, 343),
    ],
  };

  assert.deepEqual(findVisibleCollectionAnchor(root, 900), {
    entryId: 'measured-visible',
    viewportOffset: -0.75,
    entryHeight: 343,
  });
  assert.match(collectionSource, /data-collection-entry-id=\{item\.entry_id\}/);
});

test('bounded Collection cards cannot defer geometry realization after anchor restoration', () => {
  const sourceTop = -139.4375;
  const sourceHeight = 306.21875;
  const placeholderHeight = 320;
  const placeholderTop = -196.03125;
  const realizedTop = -223.625;
  const desiredPlaceholderTop = translateAnchorViewportOffset(
    sourceTop,
    sourceHeight,
    placeholderHeight,
    900,
    1,
  );
  const scrollTop = 303_356;
  const target = measuredAnchorScrollTarget(
    scrollTop,
    placeholderTop,
    desiredPlaceholderTop,
    605_185,
  );
  const immediateTop = placeholderTop - (target - scrollTop);
  const nextFrameTop = immediateTop + (realizedTop - placeholderTop);

  assert.ok(Math.abs(immediateTop - desiredPlaceholderTop) < 1e-9,
    'the synchronous placeholder measurement appears exactly restored');
  assert.ok(Math.abs(nextFrameTop - immediateTop) > 20,
    'realizing different CSS geometry reproduces post-paint drift at unchanged scrollY');
  assert.doesNotMatch(collectionCss, /\bcontent-visibility\s*:|\bcontain-intrinsic-size\s*:/,
    'the already-bounded Collection window must not introduce deferred card geometry');
});

test('measured correction removes calculated row-stride miss', () => {
  const sourceTop = -67.140625;
  const sourceHeight = 82;
  const destinationHeight = 343.34375;
  const calculatedDestinationTop = -344.09375;
  const desiredTop = translateAnchorViewportOffset(
    sourceTop,
    sourceHeight,
    destinationHeight,
    900,
    1,
  );
  const currentScrollTop = 303_356;
  const targetScrollTop = measuredAnchorScrollTarget(
    currentScrollTop,
    calculatedDestinationTop,
    desiredTop,
    605_185,
  );
  const correctedTop = calculatedDestinationTop - (targetScrollTop - currentScrollTop);

  assert.ok(calculatedDestinationTop + destinationHeight < 0);
  assert.ok(Math.abs(correctedTop - desiredTop) < 1e-9);
  assert.ok(correctedTop + destinationHeight >= 1);
});

test('near-end restoration waits for destination spacer geometry to commit', () => {
  const itemCount = 10_000;
  const viewportHeight = 667;
  const rootTop = 240;
  const provisionalGeometry = computeVirtualGeometry(itemCount, 331, 375, true);
  const destinationGeometry = computeVirtualGeometry(itemCount, 343, 375, true);
  const anchorIndex = 9_989;
  const targetLocalTop = Math.floor(anchorIndex / destinationGeometry.columns)
    * destinationGeometry.rowStride + 120;
  const provisionalWindow = buildVirtualWindow(
    itemCount,
    provisionalGeometry,
    targetLocalTop,
    viewportHeight,
  );
  const destinationWindow = buildVirtualWindow(
    itemCount,
    destinationGeometry,
    targetLocalTop,
    viewportHeight,
  );
  const targetScrollTop = rootTop + targetLocalTop;
  const clampToCommittedSpacer = (requested, committed) => Math.min(
    requested,
    rootTop + committed.totalSize - viewportHeight,
  );
  const prematurelySettled = clampToCommittedSpacer(targetScrollTop, provisionalWindow);

  assert.ok(prematurelySettled < targetScrollTop - 20_000,
    'the stale mobile spacer must reproduce a material near-end clamp');

  let committedWindow = provisionalWindow;
  let settledScrollTop = prematurelySettled;
  const restoreAfterCommit = () => {
    if (!virtualWindowsMatch(committedWindow, destinationWindow)) {
      committedWindow = destinationWindow;
      return false;
    }
    settledScrollTop = clampToCommittedSpacer(targetScrollTop, committedWindow);
    return Math.abs(settledScrollTop - targetScrollTop) < 1;
  };

  assert.equal(restoreAfterCommit(), false);
  assert.equal(settledScrollTop, prematurelySettled);
  assert.equal(restoreAfterCommit(), true);
  assert.equal(settledScrollTop, targetScrollTop);
});

test('anchor convergence survives delayed scroll-range contraction after synchronous success', () => {
  const measurement = (overrides = {}) => ({
    scrollHeight: 410_506,
    maximumScrollTop: 409_606,
    rootDocumentTop: 478.875,
    anchorViewportTop: 75.703125,
    scrollTop: 409_808,
    corrected: false,
    hasPositiveIntersection: true,
    ...overrides,
  });
  let convergence = advanceAnchorConvergence(null, measurement());
  assert.equal(convergence.settled, false,
    'one successful post-paint sample cannot clear a synchronously corrected anchor');
  assert.equal(convergence.stableFrames, 1);

  convergence = advanceAnchorConvergence(convergence, measurement({
    scrollHeight: 410_504,
    maximumScrollTop: 409_604,
    rootDocumentTop: 477.875,
    scrollTop: 409_806,
    corrected: true,
  }));
  assert.equal(convergence.settled, false,
    'a delayed virtual/layout commit resets convergence and keeps the anchor pending');
  assert.equal(convergence.stableFrames, 0);

  const stableSample = measurement({
    scrollHeight: 410_504,
    maximumScrollTop: 409_604,
    rootDocumentTop: 477.875,
    scrollTop: 409_806,
  });
  convergence = advanceAnchorConvergence(convergence, stableSample);
  assert.equal(convergence.settled, false);
  convergence = advanceAnchorConvergence(convergence, stableSample);
  assert.equal(convergence.settled, true,
    'pending clears only after the corrected scroll range is stable across paints');
  assert.equal(convergence.stableFrames, ANCHOR_CONVERGENCE_STABLE_FRAMES);
  assert.ok(convergence.frameCount < ANCHOR_CONVERGENCE_MAX_FRAMES);

  assert.match(collectionSource, /advanceAnchorConvergence\(anchor\.convergence/);
  assert.match(collectionSource, /pendingViewAnchorRef\.current !== anchor/,
    'stale frame callbacks must be fenced to the exact pending anchor object');
  assert.match(collectionSource, /cancelAnimationFrame\(anchorVerificationFrameRef\.current\)/,
    'pending verification frames must be cancelled on replacement and unmount');
  assert.match(collectionCss, /\.collection-virtual-list-panel[\s\S]*?transition-property:/,
    'the gallery spacer must not animate layout-affecting border width when reused as a panel');
});

test('pending anchors fail closed when their display dataset is replaced or identity disappears', () => {
  const original = [{ entry_id: '04999' }, { entry_id: '05000' }, { entry_id: '05001' }];
  const anchor = { entryId: '05000', index: 1, displayCards: original };
  assert.equal(resolvePendingAnchorIndex(anchor, original), 1);

  const sameLengthReplacement = original.map(item => ({ ...item }));
  assert.equal(resolvePendingAnchorIndex(anchor, sameLengthReplacement), -1,
    'a replacement generation invalidates the anchor even when its rows and length match');

  original.splice(1, 1);
  assert.equal(resolvePendingAnchorIndex(anchor, original), -1,
    'a removed card cannot fall back to a position and resurrect later');
});

test('anchor convergence exhaustion is terminal rather than latent pending state', () => {
  const unstable = {
    scrollHeight: 10_000,
    maximumScrollTop: 9_000,
    rootDocumentTop: 100,
    anchorViewportTop: 10,
    scrollTop: 5_000,
    corrected: true,
    hasPositiveIntersection: true,
  };
  let convergence = null;
  for (let frame = 0; frame < ANCHOR_CONVERGENCE_MAX_FRAMES; frame += 1) {
    convergence = advanceAnchorConvergence(convergence, {
      ...unstable,
      scrollHeight: unstable.scrollHeight - frame,
    });
  }
  assert.equal(convergence.settled, false);
  assert.equal(convergence.exhausted, true);
  assert.equal(anchorConvergenceFinished(convergence), true,
    'an exhausted verifier must clear instead of leaving a future scroll trigger');
  assert.equal(anchorConvergenceFinished({ ...convergence, settled: true, exhausted: false }), true);
});

test('virtual table metadata describes filtered logical rows, not mounted spacers', () => {
  for (const itemCount of [10_000, 5_000]) {
    assert.equal(virtualTableRowCount(itemCount), itemCount + 1,
      'aria-rowcount includes the header row');
    for (const startIndex of [0, Math.floor(itemCount / 2), itemCount - 20]) {
      const indices = Array.from({ length: 20 }, (_, mountedOffset) => (
        virtualTableRowIndex(startIndex, mountedOffset)
      ));
      assert.equal(indices[0], startIndex + 2,
        'the first data row follows the header at logical row 1');
      assert.equal(indices.at(-1), startIndex + 21);
      assert.ok(indices.every((value, index) => index === 0 || value === indices[index - 1] + 1),
        'mounted data-row indices must be monotonic and contiguous');
    }
  }

  // Native table markup is preserved; these guards prove metadata is wired to live counts/ranges.
  assert.match(collectionSource, /<table[\s\S]*?aria-rowcount=\{virtualTableRowCount\(displayCards\.length\)\}/);
  assert.match(collectionSource, /<tr aria-rowindex=\{1\}>/);
  assert.match(collectionSource,
    /aria-rowindex=\{virtualTableRowIndex\(virtualWindow\.startIndex, virtualIndex\)\}/);
  assert.match(collectionSource, /collection-virtual-list-spacer" aria-hidden="true"/);
});

test('fold spread rounds gallery columns down to an even count', () => {
  assert.equal(computeVirtualGeometry(100, 600, 951, true).columns, 3);
  assert.equal(computeVirtualGeometry(100, 600, 951, true, true).columns, 2);
  assert.equal(computeVirtualGeometry(100, 990, 951, true, true).columns, 4);
  assert.equal(computeVirtualGeometry(100, 300, 466, true, true).columns, 2);
});

export const INITIAL_RENDER_COUNT = 96;
export const VIRTUAL_OVERSCAN_ROWS = 4;
export const GALLERY_CARD_INFO_HEIGHT = 49;
export const LIST_ROW_HEIGHT = 82;
export const ANCHOR_CONVERGENCE_STABLE_FRAMES = 2;
export const ANCHOR_CONVERGENCE_MAX_FRAMES = 12;

export function computeVirtualRange(itemCount, columns, rowStride, scrollTop, viewportHeight) {
  const safeColumns = Math.max(1, columns);
  const rowCount = Math.ceil(itemCount / safeColumns);
  if (rowCount === 0) {
    return { startIndex: 0, endIndex: 0, startRow: 0, endRow: 0, rowCount: 0 };
  }

  const firstVisibleRow = Math.floor(Math.max(0, scrollTop) / rowStride);
  const lastVisibleRow = Math.ceil((Math.max(0, scrollTop) + viewportHeight) / rowStride);
  const startRow = Math.max(0, Math.min(rowCount - 1, firstVisibleRow - VIRTUAL_OVERSCAN_ROWS));
  const endRow = Math.min(rowCount, Math.max(startRow + 1, lastVisibleRow + VIRTUAL_OVERSCAN_ROWS));

  return {
    startIndex: startRow * safeColumns,
    endIndex: Math.min(itemCount, endRow * safeColumns),
    startRow,
    endRow,
    rowCount,
  };
}

// evenColumns: on a fold spread an odd column count puts a card on the hinge,
// so round down to an even count (Apple HIG, Designing for iPhone Duo).
export function computeVirtualGeometry(itemCount, width, viewportWidth, gallery, evenColumns = false) {
  const gap = gallery ? (viewportWidth >= 769 ? 20 : 12) : 0;
  const minCardWidth = viewportWidth >= 769 ? 180 : 130;
  let columns = gallery
    ? Math.max(1, Math.floor((width + gap) / (minCardWidth + gap)))
    : 1;
  if (evenColumns && columns > 2 && columns % 2) columns -= 1;
  const cardWidth = gallery ? (width - gap * (columns - 1)) / columns : width;
  const rowHeight = gallery ? (cardWidth / 0.718) + GALLERY_CARD_INFO_HEIGHT : LIST_ROW_HEIGHT;
  const rowStride = rowHeight + gap;
  const rowCount = Math.ceil(itemCount / columns);

  return {
    columns,
    rowStride,
    gap,
    rowCount,
    totalSize: Math.max(0, rowCount * rowStride - gap),
  };
}

// A negative viewport offset means the anchor row is clipped above the
// viewport. Preserve progress through the row while retaining at least one
// device pixel of positive intersection in the destination layout.
export function translateAnchorViewportOffset(
  viewportOffset,
  sourceEntryHeight,
  targetEntryHeight,
  viewportHeight = Number.POSITIVE_INFINITY,
  devicePixelRatio = 1,
) {
  const translated = viewportOffset < 0 && sourceEntryHeight > 0 && targetEntryHeight > 0
    ? (viewportOffset / sourceEntryHeight) * targetEntryHeight
    : viewportOffset;
  const visiblePixel = 1 / Math.max(1, devicePixelRatio || 1);
  const minimumOffset = targetEntryHeight > 0
    ? -Math.max(0, targetEntryHeight - visiblePixel)
    : translated;
  const maximumOffset = Number.isFinite(viewportHeight)
    ? Math.max(0, viewportHeight - visiblePixel)
    : translated;
  return Math.min(maximumOffset, Math.max(minimumOffset, translated));
}

export function findVisibleCollectionAnchor(root, viewportHeight) {
  let anchor = null;
  root.querySelectorAll('[data-collection-entry-id]').forEach((element) => {
    const rect = element.getBoundingClientRect();
    if (rect.bottom <= 0 || rect.top >= viewportHeight) return;
    if (!anchor || rect.top < anchor.viewportOffset - 0.5) {
      anchor = {
        entryId: element.dataset.collectionEntryId,
        viewportOffset: rect.top,
        entryHeight: rect.height,
      };
    }
  });
  return anchor;
}

export function measuredAnchorScrollTarget(
  currentScrollTop,
  anchorViewportTop,
  desiredViewportOffset,
  maximumScrollTop,
) {
  return Math.min(
    Math.max(0, maximumScrollTop),
    Math.max(0, currentScrollTop + anchorViewportTop - desiredViewportOffset),
  );
}

// A synchronous scroll is only provisional: the scroll event can commit a new
// virtual window on the next frame and other layout changes can contract the
// document range after that. Require two matching post-paint measurements so
// an apparently-correct synchronous rect cannot clear its anchor too early.
export function advanceAnchorConvergence(previous, sample, tolerance = 1) {
  const frameCount = (previous?.frameCount || 0) + 1;
  const priorSample = previous?.sample;
  const geometryStable = priorSample
    && Math.abs(priorSample.scrollHeight - sample.scrollHeight) <= tolerance
    && Math.abs(priorSample.maximumScrollTop - sample.maximumScrollTop) <= tolerance
    && Math.abs(priorSample.rootDocumentTop - sample.rootDocumentTop) <= tolerance
    && Math.abs(priorSample.anchorViewportTop - sample.anchorViewportTop) <= tolerance
    && Math.abs(priorSample.scrollTop - sample.scrollTop) <= tolerance;
  const stableFrames = sample.corrected || !sample.hasPositiveIntersection
    ? 0
    : geometryStable ? (previous?.stableFrames || 0) + 1 : 1;

  return {
    frameCount,
    stableFrames,
    sample,
    settled: stableFrames >= ANCHOR_CONVERGENCE_STABLE_FRAMES,
    exhausted: frameCount >= ANCHOR_CONVERGENCE_MAX_FRAMES,
  };
}

// Pending anchors are valid only for the exact derived dataset captured by the
// view switch. A filter, sort, hydration commit, or selection-mode change can
// replace the rows while keeping the same length; object identity makes that
// replacement explicit and prevents a removed card from resurrecting later.
export function resolvePendingAnchorIndex(anchor, displayCards) {
  if (!anchor || anchor.displayCards !== displayCards) return -1;
  return displayCards.findIndex(item => String(item?.entry_id) === String(anchor.entryId));
}

export function anchorConvergenceFinished(convergence) {
  return Boolean(convergence?.settled || convergence?.exhausted);
}

export function virtualWindowsMatch(current, expected) {
  return current.startIndex === expected.startIndex
    && current.endIndex === expected.endIndex
    && current.startRow === expected.startRow
    && current.endRow === expected.endRow
    && current.rowCount === expected.rowCount
    && current.columns === expected.columns
    && Math.abs(current.rowStride - expected.rowStride) < 0.5
    && Math.abs(current.gap - expected.gap) < 0.5
    && Math.abs(current.totalSize - expected.totalSize) < 0.5;
}

export function buildVirtualWindow(itemCount, geometry, scrollTop, viewportHeight) {
  const range = computeVirtualRange(
    itemCount,
    geometry.columns,
    geometry.rowStride,
    scrollTop,
    viewportHeight,
  );
  return { ...range, ...geometry };
}

// ARIA row indices include the table header at logical row 1. Virtual spacer
// rows are aria-hidden and therefore never consume a logical row index.
export function virtualTableRowCount(itemCount) {
  return itemCount + 1;
}

export function virtualTableRowIndex(windowStartIndex, mountedRowOffset) {
  return windowStartIndex + mountedRowOffset + 2;
}

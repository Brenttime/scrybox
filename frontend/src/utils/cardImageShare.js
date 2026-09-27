// Copy / share a card image from the full-screen viewer.
//
// Clipboard: browsers only accept image/png in ClipboardItem, and card art is
// JPEG, so the image is redrawn to a canvas and re-encoded. Safari (iOS) also
// requires the ClipboardItem to be constructed synchronously inside the tap
// handler, so it is handed a PROMISE of the blob rather than awaiting first.
// Scryfall's CDN and our own /api/card-art both allow cross-origin reads, so the
// canvas is never tainted.
//
// Fallbacks, in order: clipboard -> native share sheet (phones) -> download.

function loadImage(src) {
  return new Promise((resolve, reject) => {
    const img = new Image();
    img.crossOrigin = 'anonymous';
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error(`Could not load ${src}`));
    img.src = src;
  });
}

export async function cardPngBlob(src) {
  const img = await loadImage(src);
  const canvas = document.createElement('canvas');
  canvas.width = img.naturalWidth;
  canvas.height = img.naturalHeight;
  canvas.getContext('2d').drawImage(img, 0, 0);
  return new Promise((resolve, reject) => {
    canvas.toBlob(b => (b ? resolve(b) : reject(new Error('PNG encode failed'))), 'image/png');
  });
}

export function fileNameFor(name) {
  const base = String(name || 'card').replace(/[^\w\s-]+/g, '').trim().replace(/\s+/g, '-').toLowerCase();
  return `${base || 'card'}.png`;
}

export function canShareFiles() {
  try {
    return typeof navigator !== 'undefined' && !!navigator.canShare
      && navigator.canShare({ files: [new File([new Blob()], 'x.png', { type: 'image/png' })] });
  } catch {
    return false;
  }
}

function download(blob, name) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = fileNameFor(name);
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export async function shareCardImage(src, name) {
  const blob = await cardPngBlob(src);
  const file = new File([blob], fileNameFor(name), { type: 'image/png' });
  if (navigator.canShare && navigator.canShare({ files: [file] })) {
    await navigator.share({ files: [file], title: name || undefined });
    return 'shared';
  }
  download(blob, name);
  return 'saved';
}

// Returns 'copied' | 'shared' | 'saved'.
export async function copyCardImage(src, name) {
  const blobPromise = cardPngBlob(src);
  if (typeof window !== 'undefined' && window.ClipboardItem && navigator.clipboard?.write) {
    try {
      await navigator.clipboard.write([new window.ClipboardItem({ 'image/png': blobPromise })]);
      return 'copied';
    } catch (err) {
      console.warn('Clipboard image write refused, falling back:', err?.message);
    }
  }
  // No clipboard image support (non-HTTPS LAN, older browsers): share or save.
  return shareCardImage(src, name);
}

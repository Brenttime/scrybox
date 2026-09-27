// Pixel work for the in-browser card reader. Pure typed-array code, so the SAME
// functions run in the scan worker and in the Node validation harness
// (backend/scripts/client-scan-replay.mjs).
//
// Strips are sampled straight out of the camera frame through the card's
// homography at the resolution the recognizer wants, instead of first warping
// the whole card and cropping. The recognizer only ever sees 48 px high lines,
// so a full-card warp would be a 1-2 MB intermediate that is almost entirely
// thrown away.
import { getPerspectiveTransform, orderQuad } from '../imgproc.mjs';

export const REC_H = 48;

// Card portrait ratio. Card-space coordinates below are fractions of this box,
// matching the server's crops (fractions of the dewarped card).
export const CARD_W = 500;
export const CARD_H = 700;

// Portrait-ordered [TL, TR, BR, BL] like the server's _order_card_quad: a
// landscape quad is rotated a quarter turn so "top" is a short edge.
export function portraitQuad(pts) {
  const q = orderQuad(pts.map(p => ({ x: p.x, y: p.y })));
  const d = (a, b) => Math.hypot(a.x - b.x, a.y - b.y);
  const w = Math.max(d(q[0], q[1]), d(q[3], q[2]));
  const h = Math.max(d(q[1], q[2]), d(q[0], q[3]));
  return w > h ? [q[3], q[0], q[1], q[2]] : q;
}

// Cornelius returns the card's true outline. The sidecar's detector returns a
// looser quad — measured over 150 saved phone frames (median, in card-edge
// fractions): 6.1% left, 5.7% right, 3.6% top, 7.9% bottom — and every strip
// fraction it uses (title 0.025-0.100, footer 0.90/0.92, retro 0.855...) was
// tuned on crops of that size. Growing the tight quad by the same margins lets
// those fractions carry over unchanged instead of being re-derived.
export const SERVER_PAD = { l: 0.061, r: 0.057, t: 0.036, b: 0.079 };

export function padQuad(quad, pad = SERVER_PAD) {
  const m = cardToFrame(quad);
  const pts = [
    [-pad.l * CARD_W, -pad.t * CARD_H], [(1 + pad.r) * CARD_W, -pad.t * CARD_H],
    [(1 + pad.r) * CARD_W, (1 + pad.b) * CARD_H], [-pad.l * CARD_W, (1 + pad.b) * CARD_H],
  ];
  return pts.map(([x, y]) => { const [px, py] = project(m, x, y); return { x: px, y: py }; });
}

// Homography card-space (CARD_W x CARD_H) -> frame pixels.
export function cardToFrame(quad) {
  return getPerspectiveTransform(
    [{ x: 0, y: 0 }, { x: CARD_W, y: 0 }, { x: CARD_W, y: CARD_H }, { x: 0, y: CARD_H }],
    quad,
  );
}

function project(m, x, y) {
  const den = m[6] * x + m[7] * y + 1;
  return [(m[0] * x + m[1] * y + m[2]) / den, (m[3] * x + m[4] * y + m[5]) / den];
}

// Source pixels covered by one card-space unit near (x, y): >1 means the
// frame has more detail than card space, so sampling must average.
function scaleAt(m, x, y) {
  const [ax, ay] = project(m, x, y);
  const [bx, by] = project(m, x + 1, y);
  const [cx, cy] = project(m, x, y + 1);
  return Math.max(Math.hypot(bx - ax, by - ay), Math.hypot(cx - ax, cy - ay));
}

// Sample card-space rect [x0,x1]x[y0,y1] (fractions) into an RGB strip of
// height outH, width preserving the card-space aspect (server: target_w =
// strip.width * target_h / strip.height). Bilinear, supersampled when the frame
// is finer than the output so a 4K frame does not alias.
export function sampleStrip(rgba, w, h, m, x0, x1, y0, y1, outH = REC_H) {
  const cx0 = x0 * CARD_W, cx1 = x1 * CARD_W, cy0 = y0 * CARD_H, cy1 = Math.min(0.99, y1) * CARD_H;
  const outW = Math.max(16, Math.round((cx1 - cx0) * outH / (cy1 - cy0)));
  const sx = (cx1 - cx0) / outW, sy = (cy1 - cy0) / outH;
  const density = scaleAt(m, (cx0 + cx1) / 2, (cy0 + cy1) / 2) * Math.max(sx, sy);
  const ss = Math.max(1, Math.min(4, Math.round(density)));
  const out = new Uint8Array(outW * outH * 3);
  const acc = [0, 0, 0];
  for (let oy = 0; oy < outH; oy++) {
    for (let ox = 0; ox < outW; ox++) {
      acc[0] = acc[1] = acc[2] = 0;
      for (let j = 0; j < ss; j++) {
        const cy = cy0 + (oy + (j + 0.5) / ss) * sy;
        for (let i = 0; i < ss; i++) {
          const cx = cx0 + (ox + (i + 0.5) / ss) * sx;
          let [px, py] = project(m, cx, cy);
          px -= 0.5; py -= 0.5;
          if (px < 0) px = 0; else if (px > w - 1) px = w - 1;
          if (py < 0) py = 0; else if (py > h - 1) py = h - 1;
          const xi = px | 0, yi = py | 0;
          const x1i = xi + 1 < w ? xi + 1 : xi, y1i = yi + 1 < h ? yi + 1 : yi;
          const fx = px - xi, fy = py - yi;
          const i00 = (yi * w + xi) << 2, i10 = (yi * w + x1i) << 2;
          const i01 = (y1i * w + xi) << 2, i11 = (y1i * w + x1i) << 2;
          for (let c = 0; c < 3; c++) {
            acc[c] += (rgba[i00 + c] * (1 - fx) + rgba[i10 + c] * fx) * (1 - fy)
              + (rgba[i01 + c] * (1 - fx) + rgba[i11 + c] * fx) * fy;
          }
        }
      }
      const o = (oy * outW + ox) * 3, n = ss * ss;
      out[o] = acc[0] / n + 0.5; out[o + 1] = acc[1] / n + 0.5; out[o + 2] = acc[2] / n + 0.5;
    }
  }
  autocontrast(out);
  return { data: out, w: outW, h: outH };
}

// PIL ImageOps.autocontrast(cutoff=0) on an RGB image: stretch each channel
// independently so its darkest value is 0 and its brightest 255.
export function autocontrast(rgb) {
  for (let c = 0; c < 3; c++) {
    let lo = 255, hi = 0;
    for (let i = c; i < rgb.length; i += 3) { const v = rgb[i]; if (v < lo) lo = v; if (v > hi) hi = v; }
    if (hi <= lo) continue;
    const scale = 255 / (hi - lo);
    for (let i = c; i < rgb.length; i += 3) rgb[i] = Math.min(255, Math.max(0, Math.round((rgb[i] - lo) * scale)));
  }
  return rgb;
}

// 12x8 art-box grayscale signature, cosine-compared to skip OCR for a card
// already identified (the server's _IDENTITY_CACHE idea, client side).
export function artSignature(rgba, w, h, m) {
  const out = new Float32Array(96);
  let mean = 0;
  for (let gy = 0; gy < 8; gy++) {
    for (let gx = 0; gx < 12; gx++) {
      const [px, py] = project(m, (0.08 + 0.84 * (gx + 0.5) / 12) * CARD_W, (0.12 + 0.42 * (gy + 0.5) / 8) * CARD_H);
      const xi = Math.max(0, Math.min(w - 1, px | 0)), yi = Math.max(0, Math.min(h - 1, py | 0));
      const i = (yi * w + xi) << 2;
      const v = rgba[i] * 0.299 + rgba[i + 1] * 0.587 + rgba[i + 2] * 0.114;
      out[gy * 12 + gx] = v; mean += v;
    }
  }
  mean /= 96;
  for (let i = 0; i < 96; i++) out[i] -= mean;
  return out;
}

export function cosine(a, b) {
  if (!a || !b || a.length !== b.length) return -1;
  let ab = 0, aa = 0, bb = 0;
  for (let i = 0; i < a.length; i++) { ab += a[i] * b[i]; aa += a[i] * a[i]; bb += b[i] * b[i]; }
  return aa > 0 && bb > 0 ? ab / Math.sqrt(aa * bb) : -1;
}

// Card-crop focus (Laplacian variance of the title band, frame pixels) — the
// same kind of number as the server's box_sharpness, used for the blur gate.
export function titleSharpness(rgba, w, h, m) {
  const G = 64, R = 12;
  const g = new Float32Array(G * R);
  for (let y = 0; y < R; y++) {
    for (let x = 0; x < G; x++) {
      const [px, py] = project(m, (0.05 + 0.75 * x / (G - 1)) * CARD_W, (0.03 + 0.08 * y / (R - 1)) * CARD_H);
      const xi = Math.max(0, Math.min(w - 1, px | 0)), yi = Math.max(0, Math.min(h - 1, py | 0));
      const i = (yi * w + xi) << 2;
      g[y * G + x] = rgba[i] * 0.299 + rgba[i + 1] * 0.587 + rgba[i + 2] * 0.114;
    }
  }
  let s = 0, n = 0;
  for (let y = 1; y < R - 1; y++) {
    for (let x = 1; x < G - 1; x++) {
      const i = y * G + x;
      const l = 4 * g[i] - g[i - 1] - g[i + 1] - g[i - G] - g[i + G];
      s += l * l; n++;
    }
  }
  return s / n;
}

// Separable Lanczos-3 resize (RGBA or RGB in, RGB out), matching sharp's
// default filter. The corner model was validated on sharp-resized 384px
// inputs; the browser's canvas drawImage downscale (bilinear, sparse) moves
// the predicted corners a few px, which on the 61 saved Windows frames cost
// 7 of 11 proven printings (sharp 11, canvas-equivalent bilinear 4, this 11).
function lanczosWeights(inN, outN) {
  const scale = inN / outN, support = 3 * Math.max(1, scale), ws = [];
  const L = (x) => { if (x === 0) return 1; if (Math.abs(x) >= 3) return 0; const px = Math.PI * x; return 3 * Math.sin(px) * Math.sin(px / 3) / (px * px); };
  for (let i = 0; i < outN; i++) {
    const c = (i + 0.5) * scale - 0.5, lo = Math.max(0, Math.ceil(c - support)), hi = Math.min(inN - 1, Math.floor(c + support));
    const w = []; let sum = 0;
    for (let j = lo; j <= hi; j++) { const v = L((j - c) / Math.max(1, scale)); w.push(v); sum += v; }
    ws.push({ lo, w: Float32Array.from(w, v => v / sum) });
  }
  return ws;
}
// Weights and the intermediate buffer depend only on the dimensions, which are
// fixed for a camera session: computed once, reused (same values, same order).
const lzCache = new Map();
export function resizeLanczos3(src, W, H, ch, w, h) {
  const key = `${W}x${H}>${w}x${h}`;
  let c = lzCache.get(key);
  if (!c) { if (lzCache.size > 4) lzCache.clear(); c = { wx: lanczosWeights(W, w), wy: lanczosWeights(H, h), tmp: new Float32Array(H * w * 3) }; lzCache.set(key, c); }
  const { wx, wy, tmp } = c;
  for (let y = 0; y < H; y++) for (let x = 0; x < w; x++) {
    const { lo, w: k } = wx[x]; let r = 0, g = 0, b = 0;
    for (let t = 0; t < k.length; t++) { const p = (y * W + lo + t) * ch; r += src[p] * k[t]; g += src[p + 1] * k[t]; b += src[p + 2] * k[t]; }
    const o = (y * w + x) * 3; tmp[o] = r; tmp[o + 1] = g; tmp[o + 2] = b;
  }
  const out = new Uint8Array(w * h * 3);
  for (let y = 0; y < h; y++) { const { lo, w: k } = wy[y];
    for (let x = 0; x < w; x++) { let r = 0, g = 0, b = 0;
      for (let t = 0; t < k.length; t++) { const p = ((lo + t) * w + x) * 3; r += tmp[p] * k[t]; g += tmp[p + 1] * k[t]; b += tmp[p + 2] * k[t]; }
      const o = (y * w + x) * 3; out[o] = Math.max(0, Math.min(255, Math.round(r))); out[o + 1] = Math.max(0, Math.min(255, Math.round(g))); out[o + 2] = Math.max(0, Math.min(255, Math.round(b))); } }
  return out;
}

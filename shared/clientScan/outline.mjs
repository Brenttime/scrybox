// Outer-paper outline: a cheap second opinion on where the card is.
//
// The corner model (cornelius) sometimes boxes only the illustration of a
// borderless card printed on cream paper (Secret Lair "The Zeta Set": on the
// Oboro fixture it returns [902,347,271,493] while the paper spans about
// x744-1180, y240-847), so every title strip lands inside the art. The
// classic segmentation detector (shared/cardDetectPure.mjs) keys on the
// paper's own boundary and finds the whole card. It runs here on the same
// 384px copy the corner model already got (no extra capture, no zoom, no
// request), resampled to the frame's aspect at OUTLINE_W wide, without the
// warp. A result is only an outline candidate: the caller decides whether to
// read it, and never lets it replace a proven read.
import { createDetector } from '../cardDetectPure.mjs';
import { portraitQuad } from './imaging.mjs';

export const OUTLINE_W = 320;
let det = null;
let buf = null;

// Bilinear resample of the (possibly squashed) small copy to OUTLINE_W wide at
// the frame's aspect ratio, RGBA out.
function resample(src, sw, sh, ch, ow, oh) {
  const n = ow * oh * 4;
  if (!buf || buf.length !== n) buf = new Uint8ClampedArray(n);
  const fx = sw / ow, fy = sh / oh;
  for (let y = 0; y < oh; y++) {
    const sy = Math.min(sh - 1, Math.max(0, (y + 0.5) * fy - 0.5));
    const y0 = Math.floor(sy), y1 = Math.min(sh - 1, y0 + 1), wy = sy - y0;
    for (let x = 0; x < ow; x++) {
      const sx = Math.min(sw - 1, Math.max(0, (x + 0.5) * fx - 0.5));
      const x0 = Math.floor(sx), x1 = Math.min(sw - 1, x0 + 1), wx = sx - x0;
      const o = (y * ow + x) * 4;
      for (let c = 0; c < 3; c++) {
        const a = src[(y0 * sw + x0) * ch + c], b = src[(y0 * sw + x1) * ch + c];
        const d = src[(y1 * sw + x0) * ch + c], e = src[(y1 * sw + x1) * ch + c];
        buf[o + c] = (a * (1 - wx) + b * wx) * (1 - wy) + (d * (1 - wx) + e * wx) * wy;
      }
      buf[o + 3] = 255;
    }
  }
  return buf;
}

// Outline of the card's paper in frame pixels ([TL, TR, BR, BL], portrait),
// or null. `small` = sw x sh pixels (RGBA or RGB) of the whole frame.
export function paperOutline(small, sw, sh, ch, frameW, frameH) {
  if (!small || !frameW || !frameH) return null;
  det ||= createDetector();
  const ow = OUTLINE_W, oh = Math.max(32, Math.round((OUTLINE_W * frameH) / frameW));
  const img = resample(small, sw, sh, ch, ow, oh);
  const r = det.detectCard(img, ow, oh, { warp: false });
  if (!r?.quad) return null;
  const kx = frameW / ow, ky = frameH / oh;
  return { quad: portraitQuad(r.quad.map(p => ({ x: p.x * kx, y: p.y * ky }))), pick: r.pick };
}

// Geometry helpers for the caller's acceptance checks.
export function quadArea(q) {
  let a = 0;
  for (let i = 0; i < q.length; i++) { const p = q[i], n = q[(i + 1) % q.length]; a += p.x * n.y - n.x * p.y; }
  return Math.abs(a) / 2;
}
export function pointInQuad(q, x, y) {
  let sign = 0;
  for (let i = 0; i < 4; i++) {
    const a = q[i], b = q[(i + 1) % 4];
    const s = Math.sign((b.x - a.x) * (y - a.y) - (b.y - a.y) * (x - a.x));
    if (s === 0) continue;
    if (sign && s !== sign) return false;
    sign = s;
  }
  return true;
}

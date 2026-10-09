// The List / Mystery Booster 2 reprint stamp (Scryfall 'plst').
// A reprint-sheet card prints the ORIGINAL card's footer; the only physical
// difference is a small planeswalker stamp in the bottom-left corner. It is
// the same mark on every sheet card, whatever the set, so this detects the
// stamp, never a set.
//
// Template and masks were measured from 67 Scryfall original/plst image pairs
// (mean absolute difference of the bottom-left corner, card space 500x700):
// the stamp sits at x 12-32, y 655-676. Pairs scored: stamped min 0.57 / 0.52,
// unstamped max 0.52 / 0.64 (NCC / masked contrast) at full resolution; webcam
// frames from 2026-10-07 scored 0.69-0.79 / 1.19-1.61 (stamped) vs <= 0.48 /
// 0.65 (unstamped). The decision needs BOTH measures to agree, with a dead
// zone between: anything in it is 'unknown' and the user still picks.
import { sampleStrip } from './imaging.mjs';

// Corner crop in card fractions: x 0-0.16, y 0.857-1.0. sampleStrip clamps the
// bottom edge to 0.99, so the 100 px tall crop is 86 px wide (about 1.08 crop
// px per card unit). The template was cut from the same crop geometry (the
// Python harness resampled with the same clamp), and the +-12 px search
// absorbs the residual scale and quad error (review R1 nit).
export const STAMP_RECT = [0, 0.16, 0.857, 1.0];
const CROP_H = 100;
const Y0 = 600;   // card-space row of crop row 0
// Template: mean |plst - original| over the 67 pairs, rows 650-681, cols 6-39.
const T_ROW = 50, T_COL = 6;
const TEMPLATE = [[1,1,0,1,1,1,1,1,1,1,1,1,0,0,1,1,1,2,6,10,8,7,12,11,11,11,11,9,4,1,1,1,2,2],[1,1,1,1,1,1,1,1,1,1,0,0,1,1,1,1,1,3,9,13,12,11,14,10,11,11,11,11,10,4,1,1,2,2],[1,1,1,1,1,1,1,1,1,1,1,1,1,1,2,2,2,6,23,30,27,24,28,18,13,11,11,11,11,9,4,2,2,2],[1,1,1,1,1,1,1,1,1,1,1,1,1,2,1,3,3,6,26,33,28,28,35,31,28,20,13,11,10,11,8,3,2,2],[1,0,1,1,1,1,1,1,1,1,1,1,1,4,2,2,3,6,23,31,27,27,38,34,30,30,25,15,11,12,11,6,1,2],[1,0,0,0,1,1,1,1,1,1,1,1,4,6,4,2,3,10,24,33,29,28,40,34,28,31,32,28,17,12,12,10,3,2],[1,0,0,0,1,1,1,1,1,1,2,2,10,14,6,3,4,20,28,31,33,33,37,29,26,32,33,33,29,18,13,12,6,2],[1,0,0,0,1,1,1,1,1,1,3,3,16,22,8,4,6,39,34,28,31,37,36,26,25,30,31,31,35,29,14,13,9,2],[1,1,0,1,1,1,1,2,2,6,7,5,21,33,13,5,14,60,38,22,35,42,36,26,27,30,30,29,36,36,23,13,11,3],[1,1,1,1,1,1,2,3,2,17,16,4,33,44,33,21,32,86,48,23,47,45,34,24,34,30,30,26,33,37,32,17,12,4],[1,1,0,1,1,2,6,5,3,30,27,8,44,57,57,42,56,109,57,31,70,46,29,21,38,28,29,26,31,35,34,22,14,5],[1,1,1,1,2,5,18,9,6,44,38,14,65,65,86,69,86,129,65,53,97,49,37,32,41,27,25,33,27,33,34,27,13,5],[1,1,1,1,2,7,32,17,11,56,46,28,94,78,111,97,120,141,83,91,125,56,51,53,42,24,19,35,24,32,31,29,14,5],[1,1,1,1,3,9,43,26,19,65,52,40,120,99,124,131,165,155,105,127,139,67,77,75,40,30,25,37,26,32,29,29,16,5],[1,1,1,1,3,12,53,37,30,69,64,72,149,149,153,178,220,173,149,173,152,97,103,90,37,34,34,37,27,31,29,30,18,5],[1,1,1,0,4,15,59,60,58,68,74,99,168,192,177,200,235,195,180,196,164,126,122,99,34,34,35,20,32,30,28,30,17,5],[1,1,1,1,2,16,57,68,69,69,72,85,145,200,172,191,230,213,193,198,165,126,110,73,32,26,29,24,35,30,29,31,18,6],[1,1,1,1,2,5,33,56,65,69,70,77,91,157,186,196,209,223,199,173,150,108,58,54,33,25,36,24,32,30,30,30,17,4],[1,1,1,1,1,2,3,21,49,62,68,72,89,118,148,198,204,186,162,144,108,62,56,46,34,42,37,22,33,32,34,33,17,5],[1,1,1,1,1,1,2,5,10,44,63,71,76,100,118,161,184,143,135,121,67,62,58,39,35,29,16,23,30,26,28,28,14,3],[1,1,1,1,1,1,0,1,4,10,47,65,71,70,93,106,139,134,114,78,62,58,50,33,23,16,25,26,23,24,23,25,12,3],[1,1,1,1,1,1,0,1,2,3,20,57,69,73,69,46,88,133,96,44,58,55,40,26,20,28,26,24,25,25,27,28,15,4],[1,1,1,1,1,1,1,1,1,2,2,41,66,71,53,21,41,123,72,38,56,53,29,23,28,24,22,23,23,24,26,26,13,4],[1,1,1,1,1,1,2,1,1,1,3,21,60,66,31,10,11,78,53,34,55,51,15,26,21,18,17,18,18,17,18,19,7,2],[1,1,1,1,1,1,1,2,1,1,2,8,51,56,17,6,2,26,31,36,47,35,24,26,21,20,19,20,19,17,19,19,6,2],[1,1,1,1,1,1,1,1,1,1,2,2,36,43,9,4,2,11,13,30,38,19,25,25,23,23,20,20,21,20,20,19,7,2],[1,1,1,1,1,1,1,1,1,1,1,1,16,22,5,1,1,4,6,37,41,9,8,5,5,6,5,5,5,6,6,4,2,1],[1,1,1,1,1,1,1,1,1,1,1,2,3,7,3,1,1,2,4,37,36,5,2,2,2,2,2,2,2,2,2,2,1,1],[1,1,1,1,1,1,1,1,1,1,1,1,1,2,1,1,1,2,2,24,22,2,1,1,1,1,1,1,1,1,1,1,1,1],[1,1,1,1,1,1,1,1,1,1,1,1,1,2,1,1,1,1,2,11,11,1,1,1,1,1,1,1,1,1,1,1,1,1],[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,3,3,1,2,1,1,1,1,1,1,1,1,1,1,1],[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1]];
// Stamp pixels and a ring around them (row, colStart, colEnd), crop coords.
const MASK = [[55,28,29],[58,23,24],[58,27,28],[59,19,20],[59,23,25],[59,26,28],[60,18,25],[60,26,28],[61,15,16],[61,18,28],[61,30,31],[62,15,17],[62,18,31],[63,12,13],[63,15,31],[64,12,13],[64,15,30],[65,12,30],[66,12,30],[67,13,30],[68,14,30],[68,31,32],[69,15,29],[70,16,29],[71,17,29],[72,17,21],[72,22,25],[72,26,28],[73,18,20],[73,23,25],[73,26,28],[74,18,20],[74,26,27],[75,19,20],[76,26,27]];
const BAND = [[49,28,29],[50,27,30],[51,26,31],[52,23,24],[52,25,28],[52,29,32],[53,19,20],[53,22,27],[53,30,33],[54,18,26],[54,31,34],[55,15,16],[55,17,23],[55,24,25],[55,32,35],[56,14,19],[56,20,22],[56,25,26],[56,31,34],[57,12,18],[57,30,33],[58,11,15],[58,16,17],[58,31,34],[59,10,14],[59,32,35],[60,9,12],[60,33,36],[61,8,11],[61,34,37],[62,7,10],[62,34,37],[63,6,9],[63,34,37],[64,6,9],[64,33,36],[65,6,9],[65,33,36],[66,6,9],[66,33,36],[67,7,10],[67,34,37],[68,8,11],[68,35,38],[69,9,12],[69,34,37],[70,10,13],[70,33,36],[71,11,14],[71,32,35],[72,11,14],[72,31,34],[73,12,15],[73,31,34],[74,12,15],[74,30,33],[75,13,16],[75,29,32],[76,14,17],[76,22,23],[76,30,33],[77,15,18],[77,21,24],[77,29,32],[78,16,19],[78,20,25],[78,28,31],[79,17,22],[79,23,26],[79,27,30],[80,18,21],[80,24,29],[81,19,20],[81,25,28],[82,26,27]];
const SHIFT = 12;   // search +-12 px for quad error
export const STAMP_PRESENT = Object.freeze({ ncc: 0.60, contrast: 0.90 });
export const STAMP_ABSENT = Object.freeze({ ncc: 0.50, contrast: 0.80 });

const TH = TEMPLATE.length, TW = TEMPLATE[0].length;
const TFLAT = (() => {
  const f = new Float32Array(TH * TW);
  let s = 0; for (let r = 0; r < TH; r++) for (let c = 0; c < TW; c++) s += (f[r * TW + c] = TEMPLATE[r][c]);
  const mean = s / f.length; let v = 0;
  for (let i = 0; i < f.length; i++) { f[i] -= mean; v += f[i] * f[i]; }
  const sd = Math.sqrt(v / f.length);
  for (let i = 0; i < f.length; i++) f[i] /= sd;
  return f;
})();
const pts = (runs) => runs.flatMap(([r, a, b]) => Array.from({ length: b - a }, (_, i) => [r, a + i]));
const MASK_PTS = pts(MASK), BAND_PTS = pts(BAND);

function gray(strip) {
  const n = strip.w * strip.h, g = new Float32Array(n), d = strip.data;
  for (let i = 0; i < n; i++) g[i] = 0.299 * d[3 * i] + 0.587 * d[3 * i + 1] + 0.114 * d[3 * i + 2];
  return g;
}

// Pure scoring of the gray corner crop (exported for tests).
export function stampScores(g, W, H) {
  let ncc = -1, contrast = -Infinity;
  for (let dy = -SHIFT; dy <= SHIFT; dy++) {
    for (let dx = -SHIFT; dx <= SHIFT; dx++) {
      const y = T_ROW + dy, x = T_COL + dx;
      if (y >= 0 && x >= 0 && y + TH <= H && x + TW <= W) {
        let s = 0, s2 = 0;
        for (let r = 0; r < TH; r++) for (let c = 0; c < TW; c++) { const v = g[(y + r) * W + x + c]; s += v; s2 += v * v; }
        const n = TH * TW, mean = s / n, sd = Math.sqrt(Math.max(0, s2 / n - mean * mean));
        if (sd >= 4) {
          let acc = 0;
          for (let r = 0; r < TH; r++) for (let c = 0; c < TW; c++) acc += (g[(y + r) * W + x + c] - mean) / sd * TFLAT[r * TW + c];
          ncc = Math.max(ncc, acc / n);
        }
      }
      let ok = true, a = 0, a2 = 0, b = 0, b2 = 0;
      for (const [r, c] of MASK_PTS) { const yy = r + dy, xx = c + dx; if (yy < 0 || xx < 0 || yy >= H || xx >= W) { ok = false; break; } const v = g[yy * W + xx]; a += v; a2 += v * v; }
      if (!ok) continue;
      for (const [r, c] of BAND_PTS) { const yy = r + dy, xx = c + dx; if (yy < 0 || xx < 0 || yy >= H || xx >= W) { ok = false; break; } const v = g[yy * W + xx]; b += v; b2 += v * v; }
      if (!ok) continue;
      const na = MASK_PTS.length, nb = BAND_PTS.length;
      const ma = a / na, mb = b / nb;
      const sa = Math.sqrt(Math.max(0, a2 / na - ma * ma)), sb = Math.sqrt(Math.max(0, b2 / nb - mb * mb));
      contrast = Math.max(contrast, (ma - mb) / (sa + sb + 8));
    }
  }
  return { ncc, contrast };
}

// 'absent' is only claimed on a crop with real edge detail: on a blurred
// crop (Gaussian sigma 6 in card units) 3 of 67 stamped references scored
// below the absent thresholds; with this floor none did (blur 0-6 sweep).
// 'present' never fired on an unstamped reference at any blur.
export const STAMP_SHARP_MIN = 25;
export function stampVerdict({ ncc, contrast, sharp = Infinity }) {
  if (ncc >= STAMP_PRESENT.ncc && contrast >= STAMP_PRESENT.contrast) return 'present';
  if (ncc < STAMP_ABSENT.ncc && contrast < STAMP_ABSENT.contrast && sharp >= STAMP_SHARP_MIN) return 'absent';
  return 'unknown';
}

// 99.5th percentile of |horizontal| and |vertical| neighbour differences.
export function cropSharpness(g, W, H) {
  const hist = new Uint32Array(256); let n = 0;
  for (let y = 0; y < H; y++) for (let x = 0; x < W; x++) {
    const v = g[y * W + x];
    if (x + 1 < W) { hist[Math.min(255, Math.abs(g[y * W + x + 1] - v) | 0)]++; n++; }
    if (y + 1 < H) { hist[Math.min(255, Math.abs(g[(y + 1) * W + x] - v) | 0)]++; n++; }
  }
  let k = Math.ceil(n * 0.005), d = 255;
  for (; d > 0; d--) { k -= hist[d]; if (k <= 0) break; }
  return d;
}

// m: homography of the card's TRUE outline (unpadded quad) into the frame.
export function readListStamp(rgba, w, h, m) {
  const strip = sampleStrip(rgba, w, h, m, STAMP_RECT[0], STAMP_RECT[1], STAMP_RECT[2], STAMP_RECT[3], CROP_H);
  const g = gray(strip);
  const s = { ...stampScores(g, strip.w, strip.h), sharp: cropSharpness(g, strip.w, strip.h) };
  return { ...s, verdict: stampVerdict(s) };
}

// Echotype icon generator: the app's dotted globe on a black macOS tile.
// Dot geometry is the "searching" globe of thinking-orbs by Jakub Antalik, MIT License
// (https://github.com/Jakubantalik/thinking-orbs, vendored at ../src/vendor/thinking-orbs.js).
// Deterministic: a fixed frame, no randomness, no dependencies beyond Node's stdlib.
//
//   node app/scripts/icons.mjs      (from the repo root; macOS for iconutil)
//
// Writes app/src-tauri/icons/* (every PNG at its own size, icon.icns, icon.ico), app/src/icon.svg
// and .github/assets/icon.png. The menu-bar glyphs are drawn at runtime in src-tauri/src/tray.rs.
import { MODE_FRAMES } from '../src/vendor/thinking-orbs.js';
import { deflateSync } from 'node:zlib';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const app = join(dirname(fileURLToPath(import.meta.url)), '..');
const icons = join(app, 'src-tauri/icons');
const T = 1.2; // the frozen frame: yaw 0.6 rad, tilt ~24 degrees

// Fewer, larger dots as the icon shrinks, so it is drawn for its size rather than downscaled.
function variant(px) {
  if (px <= 20) return { rings: 6, s: 1 };
  if (px <= 40) return { rings: 8, s: 1 };
  if (px <= 80) return { rings: 9, s: 2 };
  if (px <= 160) return { latRings: 11, lonDensity: 28, rMin: 0.008, rSpan: 0.038 };
  return { latRings: 15, lonDensity: 38, rMin: 0.005, rSpan: 0.03 };
}

// The tile on Apple's grid (824 of 1024, corner 185.4), snapped to whole pixels when small.
function tile(px) {
  const x0 = px < 128 ? Math.floor((px * 100) / 1024) : (px * 100) / 1024;
  const w = px - 2 * x0;
  return { x0, w, r: (w * 185.4) / 824, c: px / 2 };
}

// Globe dots in pixel space: {x, y, r, grey 0..1}, back to front.
function globe(px) {
  const v = variant(px);
  const t = tile(px);
  const R = t.w * 0.39; // sphere diameter ~78% of the tile
  const S = (2 * R) / 0.82; // the engine fits the sphere to 82% of its frame
  const off = t.c - S / 2;
  if (v.rings) return pixelGlobe(t, R, v.rings, v.s);
  const { dots } = MODE_FRAMES.globe(S, T, { latRings: v.latRings, lonDensity: v.lonDensity, rBoost: 0, dimBase: 1 });
  return dots.map((d) => {
    const depth = (d.z + 1) / 2;
    return { x: d.x + off, y: d.y + off, r: R * (v.rMin + v.rSpan * depth), grey: (1 - d.white) * (0.3 + 0.7 * depth ** 1.5), z: d.z };
  });
}

// Too small for the lattice to survive pixel snapping: the same tilted globe drawn as pixel art,
// in dots of s x s px. Each latitude ring keeps only its front arc, sampled every 2s px across so
// the dots stay evenly spaced, with the engine's projection (makeProj's tilt) and ink ramp
// (inkFar 0.62, inkSpan 0.54), its contrast stretched so depth survives at this size.
function pixelGlobe(t, R, rings, s) {
  const tilt = 0.4 + 0.06 * Math.sin(T * 0.35), st = Math.sin(tilt), ct = Math.cos(tilt);
  const dots = [];
  for (let k = 1; k < rings; k++) {
    const lat = -Math.PI / 2 + (k / rings) * Math.PI, cl = Math.cos(lat), sl = Math.sin(lat);
        for (let x = s * (0.5 + (k % 2)); x < t.c * 2; x += 2 * s) {
      const u = (x - t.c) / R;
      if (Math.abs(u) > cl) continue;
      const z1 = Math.sqrt(cl * cl - u * u), z = sl * st + z1 * ct;
      const y = Math.floor(t.c - (sl * ct - z1 * st) * R) + s / 2; // whole pixels: crisp at s = 1 and 2
      if (z < 0) continue;
      const depth = (z + 1) / 2;
      dots.push({ x, y, r: 0.62 * s, grey: Math.min(1, 1 - (0.62 - 0.54 * depth) * 1.6), z });
    }
  }
  return dots.sort((a, b) => a.z - b.z);
}

// Signed distance to the rounded tile (negative inside).
function sdTile(t, x, y) {
  const h = t.w / 2 - t.r;
  const qx = Math.abs(x - t.c) - h, qy = Math.abs(y - t.c) - h;
  return Math.hypot(Math.max(qx, 0), Math.max(qy, 0)) + Math.min(Math.max(qx, qy), 0) - t.r;
}

// RGBA raster: drop shadow, near-black tile with a faint rim, then the dots. 4x4 supersampling.
function render(px) {
  const t = tile(px);
  const img = new Float32Array(px * px * 4); // premultiplied
  const over = (i, rgb, a) => {
    for (let k = 0; k < 3; k++) img[i + k] = rgb[k] * a + img[i + k] * (1 - a);
    img[i + 3] = a + img[i + 3] * (1 - a);
  };
  const rim = Math.max(1, px / 512);
  const ss = (fn, x, y) => {
    let s = 0;
    for (let j = 0; j < 4; j++) for (let i = 0; i < 4; i++) s += fn(x + (i + 0.5) / 4, y + (j + 0.5) / 4);
    return s / 16;
  };
  for (let y = 0; y < px; y++) {
    for (let x = 0; x < px; x++) {
      const i = (y * px + x) * 4;
      // Shadow: offset down, soft, only where the margin allows.
      const ds = sdTile(t, x + 0.5, y + 0.5 - px * 0.012);
      const sh = 0.3 * Math.min(1, Math.max(0, 1 - (ds + px * 0.004) / (px * 0.03)));
      if (sh > 0) over(i, [0, 0, 0], sh);
      const cov = ss((a, b) => (sdTile(t, a, b) <= 0 ? 1 : 0), x, y);
      if (!cov) continue;
      const g = (0x0b / 255) * (1 - (y - t.x0) / t.w); // #0B0B0C at the top to #000 at the bottom
      over(i, [g, g, g * 1.04], cov);
      const edge = ss((a, b) => { const d = sdTile(t, a, b); return d <= 0 && d > -rim ? 1 : 0; }, x, y);
      if (edge) over(i, [1, 1, 1], edge * (px <= 40 ? 0.2 : 0.13));
    }
  }
  for (const d of globe(px)) {
    for (let y = Math.max(0, Math.floor(d.y - d.r)); y <= Math.min(px - 1, Math.ceil(d.y + d.r)); y++) {
      for (let x = Math.max(0, Math.floor(d.x - d.r)); x <= Math.min(px - 1, Math.ceil(d.x + d.r)); x++) {
        const cov = ss((a, b) => (Math.hypot(a - d.x, b - d.y) <= d.r ? 1 : 0), x, y);
        if (cov) over((y * px + x) * 4, [d.grey, d.grey, d.grey], cov);
      }
    }
  }
  const out = Buffer.alloc(px * px * 4);
  for (let i = 0; i < img.length; i += 4) {
    const a = img[i + 3];
    for (let k = 0; k < 3; k++) out[i + k] = a ? Math.round((img[i + k] / a) * 255) : 0;
    out[i + 3] = Math.round(a * 255);
  }
  return out;
}

const crcTable = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});
const crc = (b) => { let c = ~0; for (const x of b) c = crcTable[(c ^ x) & 255] ^ (c >>> 8); return ~c >>> 0; };
function chunk(type, data) {
  const len = Buffer.alloc(4), sum = Buffer.alloc(4), td = Buffer.concat([Buffer.from(type), data]);
  len.writeUInt32BE(data.length);
  sum.writeUInt32BE(crc(td));
  return Buffer.concat([len, td, sum]);
}
function png(px) {
  const rgba = render(px);
  const raw = Buffer.alloc(px * (px * 4 + 1));
  for (let y = 0; y < px; y++) rgba.copy(raw, y * (px * 4 + 1) + 1, y * px * 4, (y + 1) * px * 4);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(px, 0);
  ihdr.writeUInt32BE(px, 4);
  ihdr.set([8, 6, 0, 0, 0], 8);
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ihdr), chunk('IDAT', deflateSync(raw, { level: 9 })), chunk('IEND', Buffer.alloc(0))]);
}

function svg() {
  const t = tile(1024), f = (n) => +n.toFixed(2);
  const dots = globe(1024).map((d) => { const g = Math.round(d.grey * 255).toString(16).padStart(2, '0'); return `<circle cx="${f(d.x)}" cy="${f(d.y)}" r="${f(d.r)}" fill="#${g}${g}${g}"/>`; });
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="1024" height="1024">
<!-- Echotype: a dotted globe (thinking-orbs by Jakub Antalik, MIT). Generated by app/scripts/icons.mjs. -->
<defs><linearGradient id="bg" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#0b0b0c"/><stop offset="1" stop-color="#000"/></linearGradient>
<filter id="shadow" x="-10%" y="-10%" width="120%" height="125%"><feDropShadow dx="0" dy="12" stdDeviation="12" flood-color="#000" flood-opacity="0.3"/></filter></defs>
<rect x="${t.x0}" y="${t.x0}" width="${t.w}" height="${t.w}" rx="${t.r}" filter="url(#shadow)"/>
<rect x="${t.x0}" y="${t.x0}" width="${t.w}" height="${t.w}" rx="${t.r}" fill="url(#bg)"/>
<rect x="${t.x0 + 1}" y="${t.x0 + 1}" width="${t.w - 2}" height="${t.w - 2}" rx="${t.r - 1}" fill="none" stroke="#fff" stroke-opacity="0.13" stroke-width="2"/>
${dots.join('\n')}
</svg>
`;
}

const pngs = {
  '32x32.png': 32, '64x64.png': 64, '128x128.png': 128, '128x128@2x.png': 256, 'icon.png': 512,
  'Square30x30Logo.png': 30, 'Square44x44Logo.png': 44, 'Square71x71Logo.png': 71, 'Square89x89Logo.png': 89,
  'Square107x107Logo.png': 107, 'Square142x142Logo.png': 142, 'Square150x150Logo.png': 150,
  'Square284x284Logo.png': 284, 'Square310x310Logo.png': 310, 'StoreLogo.png': 50,
};
const cache = new Map();
const get = (px) => cache.get(px) ?? cache.set(px, png(px)).get(px);
for (const [name, px] of Object.entries(pngs)) writeFileSync(join(icons, name), get(px));
writeFileSync(join(app, '../.github/assets/icon.png'), get(256));
writeFileSync(join(app, 'src/icon.svg'), svg());

// .icns through iconutil, from an iconset of per-size renders.
const tmp = mkdtempSync(join(tmpdir(), 'echotype-icons-'));
const set = join(tmp, 'icon.iconset');
mkdirSync(set);
for (const s of [16, 32, 128, 256, 512]) {
  writeFileSync(join(set, `icon_${s}x${s}.png`), get(s));
  writeFileSync(join(set, `icon_${s}x${s}@2x.png`), get(s * 2));
}
execFileSync('iconutil', ['-c', 'icns', set, '-o', join(icons, 'icon.icns')]);
rmSync(tmp, { recursive: true });

// .ico: a directory of PNG entries (Vista and later read PNG inside ICO).
const sizes = [16, 24, 32, 48, 64, 256];
const head = Buffer.alloc(6 + 16 * sizes.length);
head.writeUInt16LE(1, 2);
head.writeUInt16LE(sizes.length, 4);
let at = head.length;
sizes.forEach((s, k) => {
  const e = 6 + 16 * k, len = get(s).length;
  head[e] = s & 255; head[e + 1] = s & 255; // 256 is stored as 0
  head.writeUInt16LE(1, e + 4);
  head.writeUInt16LE(32, e + 6);
  head.writeUInt32LE(len, e + 8);
  head.writeUInt32LE(at, e + 12);
  at += len;
});
writeFileSync(join(icons, 'icon.ico'), Buffer.concat([head, ...sizes.map(get)]));
console.log(`wrote ${Object.keys(pngs).length} PNGs, icon.icns, icon.ico, src/icon.svg, .github/assets/icon.png`);

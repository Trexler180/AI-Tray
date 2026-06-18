// Generates a 1024x1024 source icon (no external deps) — a usage "gauge" ring.
// Run: node scripts/gen-icon.mjs  ->  src-tauri/icons/source.png
// Then: npx tauri icon src-tauri/icons/source.png
import { deflateSync } from "node:zlib";
import { writeFileSync, mkdirSync } from "node:fs";

const S = 1024;
const cx = S / 2,
  cy = S / 2;
const rOuter = 410,
  rInner = 290;
const usedFrac = 0.72; // teal arc portion

const buf = Buffer.alloc(S * S * 4);

function set(x, y, r, g, b, a = 255) {
  const i = (y * S + x) * 4;
  buf[i] = r;
  buf[i + 1] = g;
  buf[i + 2] = b;
  buf[i + 3] = a;
}
const lerp = (a, b, t) => Math.round(a + (b - a) * t);

for (let y = 0; y < S; y++) {
  for (let x = 0; x < S; x++) {
    const dx = x - cx,
      dy = y - cy;
    const dist = Math.hypot(dx, dy);

    // Rounded-square dark background with vertical gradient.
    const t = y / S;
    let r = lerp(0x16, 0x0c, t),
      g = lerp(0x1b, 0x0f, t),
      b = lerp(0x24, 0x15, t),
      a = 255;

    // Corner rounding for the source square.
    const m = 60;
    const inCorner =
      (x < m && y < m && Math.hypot(x - m, y - m) > m) ||
      (x > S - m && y < m && Math.hypot(x - (S - m), y - m) > m) ||
      (x < m && y > S - m && Math.hypot(x - m, y - (S - m)) > m) ||
      (x > S - m && y > S - m && Math.hypot(x - (S - m), y - (S - m)) > m);
    if (inCorner) a = 0;

    if (dist >= rInner && dist <= rOuter && a !== 0) {
      // angle: 0 at top, clockwise 0..1
      let ang = Math.atan2(dx, -dy) / (2 * Math.PI);
      if (ang < 0) ang += 1;
      if (ang <= usedFrac) {
        // teal gradient along the arc
        const k = ang / usedFrac;
        r = lerp(0x0e, 0x2d, k);
        g = lerp(0xa5, 0xd4, k);
        b = lerp(0xa3, 0xbf, k);
      } else {
        // dim track
        r = 0x2a;
        g = 0x30;
        b = 0x3c;
      }
    }
    set(x, y, r, g, b, a);
  }
}

// ---- encode PNG ----
function crc32(buf) {
  let c = ~0;
  for (let i = 0; i < buf.length; i++) {
    c ^= buf[i];
    for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1));
  }
  return (~c) >>> 0;
}
function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const t = Buffer.from(type, "ascii");
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(Buffer.concat([t, data])), 0);
  return Buffer.concat([len, t, data, crc]);
}

const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(S, 0);
ihdr.writeUInt32BE(S, 4);
ihdr[8] = 8; // bit depth
ihdr[9] = 6; // RGBA
// rest 0

const raw = Buffer.alloc(S * (S * 4 + 1));
for (let y = 0; y < S; y++) {
  raw[y * (S * 4 + 1)] = 0; // filter none
  buf.copy(raw, y * (S * 4 + 1) + 1, y * S * 4, (y + 1) * S * 4);
}
const idat = deflateSync(raw, { level: 9 });

const png = Buffer.concat([
  Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
  chunk("IHDR", ihdr),
  chunk("IDAT", idat),
  chunk("IEND", Buffer.alloc(0)),
]);

mkdirSync("src-tauri/icons", { recursive: true });
writeFileSync("src-tauri/icons/source.png", png);
console.log("wrote src-tauri/icons/source.png (" + png.length + " bytes)");

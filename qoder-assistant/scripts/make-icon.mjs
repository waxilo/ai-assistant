// 纯 Node 生成一张 1024x1024 的 PNG 图标（蓝色圆角方块 + 白色对勾），
// 不依赖任何第三方库，使用 zlib 编码。产物供 `tauri icon` 转换为各平台图标套件。
import zlib from "node:zlib";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const SIZE = 1024;

// ---- CRC32（PNG 校验用） ----
const crcTable = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();
function crc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = crcTable[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}
function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const typeBuf = Buffer.from(type, "ascii");
  const body = Buffer.concat([typeBuf, data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([len, body, crc]);
}

// ---- 像素绘制 ----
const px = Buffer.alloc(SIZE * SIZE * 4);
function set(x, y, r, g, b, a) {
  const i = (y * SIZE + x) * 4;
  px[i] = r;
  px[i + 1] = g;
  px[i + 2] = b;
  px[i + 3] = a;
}
const lerp = (a, b, t) => Math.round(a + (b - a) * t);

function inRoundRect(x, y, m, rad) {
  const X = x - m;
  const Y = y - m;
  const W = SIZE - 2 * m;
  const H = SIZE - 2 * m;
  if (X < 0 || Y < 0 || X > W || Y > H) return false;
  const cx = Math.min(Math.max(X, rad), W - rad);
  const cy = Math.min(Math.max(Y, rad), H - rad);
  const dx = X - cx;
  const dy = Y - cy;
  return dx * dx + dy * dy <= rad * rad;
}
function distToSeg(px0, py0, ax, ay, bx, by) {
  const dx = bx - ax;
  const dy = by - ay;
  const len2 = dx * dx + dy * dy;
  let t = len2 ? ((px0 - ax) * dx + (py0 - ay) * dy) / len2 : 0;
  t = Math.min(Math.max(t, 0), 1);
  const cx = ax + t * dx;
  const cy = ay + t * dy;
  return Math.hypot(px0 - cx, py0 - cy);
}

const m = 96;
const rad = 220;
// 对勾端点（相对坐标 0..1）
const p1 = [0.3, 0.54];
const p2 = [0.45, 0.69];
const p3 = [0.74, 0.36];
const thick = 64;

for (let y = 0; y < SIZE; y++) {
  for (let x = 0; x < SIZE; x++) {
    const t = x / SIZE;
    let r = lerp(71, 47, t);
    let g = lerp(118, 87, t);
    let b = lerp(255, 214, t);
    let a = 0;
    if (inRoundRect(x, y, m, rad)) a = 255;

    // 白色对勾覆盖
    if (a === 255) {
      const d1 = distToSeg(x, y, p1[0] * SIZE, p1[1] * SIZE, p2[0] * SIZE, p2[1] * SIZE);
      const d2 = distToSeg(x, y, p2[0] * SIZE, p2[1] * SIZE, p3[0] * SIZE, p3[1] * SIZE);
      if (d1 <= thick / 2 || d2 <= thick / 2) {
        r = 255;
        g = 255;
        b = 255;
      }
    }
    set(x, y, r, g, b, a);
  }
}

// ---- 编码 PNG（RGBA, 每行前加 filter byte 0） ----
const raw = Buffer.alloc((SIZE * 4 + 1) * SIZE);
for (let y = 0; y < SIZE; y++) {
  raw[y * (SIZE * 4 + 1)] = 0;
  px.copy(raw, y * (SIZE * 4 + 1) + 1, y * SIZE * 4, (y + 1) * SIZE * 4);
}
const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(SIZE, 0);
ihdr.writeUInt32BE(SIZE, 4);
ihdr[8] = 8; // bit depth
ihdr[9] = 6; // color type RGBA
ihdr[10] = 0;
ihdr[11] = 0;
ihdr[12] = 0;
const idat = zlib.deflateSync(raw, { level: 9 });
const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk("IHDR", ihdr),
  chunk("IDAT", idat),
  chunk("IEND", Buffer.alloc(0)),
]);

const out = path.resolve(__dirname, "../src-tauri/icons/icon-source.png");
fs.mkdirSync(path.dirname(out), { recursive: true });
fs.writeFileSync(out, png);
console.log("wrote", out, png.length, "bytes");

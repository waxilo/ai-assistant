// 纯 Node 生成一张 1024x1024 的 PNG 图标（蓝色圆角方块 + 白色字母 Q），
// 不依赖任何第三方库，使用 zlib 编码。产物供 `tauri icon` 转换为各平台图标套件。
//
// # 为什么是「有向距离场」而不是逐像素的 if
//
// 上一版画对勾时用的是「点到线段的距离 <= 半宽」——边缘只有 0/1 两档，等于没有抗锯齿。
// 一条斜线还看不出来，一个圆环在 1024 图上就会是肉眼可见的锯齿。这里改成：
// 每个形状给出**有向距离**（内部为负），再用 `clamp(0.5 - d, 0, 1)` 当覆盖率，
// 边缘自然获得一个像素的过渡。顺带好处是形状之间可以直接 `min()` 做并集
//（Q 的环与尾巴不是两个独立图形，而是一个字形）。
//
// # 为什么是几何画法而不是渲染字体
//
// 渲字体要装 canvas / 依赖系统字体文件，两者都会让这个脚本不再「零依赖、结果可复现」。
// 而「环 + 尾巴」本来就是字母 Q 的几何本形，在小尺寸下反而比一个字体的衬线细节更清楚。
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

// ---- 距离场 ----

/** 覆盖率：距边缘 1px 内线性过渡，即最朴素也最不容易出错的抗锯齿 */
const cover = (d) => Math.min(Math.max(0.5 - d, 0), 1);

/** 圆角矩形的有向距离（内部为负）。`x/y` 相对矩形左上角 */
function sdRoundRect(x, y, w, h, rad) {
  // 把坐标折到第一象限，四角就变成同一个圆心距问题
  const qx = Math.abs(x - w / 2) - (w / 2 - rad);
  const qy = Math.abs(y - h / 2) - (h / 2 - rad);
  const out = Math.hypot(Math.max(qx, 0), Math.max(qy, 0));
  return out + Math.min(Math.max(qx, qy), 0) - rad;
}

/** 圆环的有向距离：环心距与中径之差，再减去半宽 */
function sdRing(x, y, cx, cy, mid, half) {
  return Math.abs(Math.hypot(x - cx, y - cy) - mid) - half;
}

/** 线段的有向距离（平头端帽 —— 圆头会让笔画的起止端鼓出来） */
function sdSegment(x, y, ax, ay, bx, by) {
  const dx = bx - ax;
  const dy = by - ay;
  const len2 = dx * dx + dy * dy;
  let t = len2 ? ((x - ax) * dx + (y - ay) * dy) / len2 : 0;
  t = Math.min(Math.max(t, 0), 1);
  return Math.hypot(x - (ax + t * dx), y - (ay + t * dy));
}

const lerp = (a, b, t) => Math.round(a + (b - a) * t);

// ---- 版面 ----
//
// 数字都是「相对 1024 画布」定的，改任何一个都要连带看其余几个：
// 底座留白 96 ⇒ 蓝方块 832 见方。环外径 270 ⇒ 字形占边长 65%，
// 四周留白 146 对称（环心因此取正中的 512 —— 尾巴沿 45° 探出后的横向落点
// 242 仍小于环半径 270，所以**字形外接框就是环的外接框**，不需要为尾巴做偏心）。
//
// ⚠️ 这个尾巴试错过两轮，两条边界都记在这里，别再走一遍：
//   1. 探出太短（=环的宽度）→ 只是环边缘鼓起一个包，不像 Q；
//   2. 探出太长又太细（长度 ≫ 宽度）→ 直接变成**放大镜**。
// 正解是第三条：**尾巴要能看见地穿过环内的空心**（从半径 70 一直伸到 312），
// 看得到「一根横杠压在环里」，才是字形而不是外挂。
const MARGIN = 96;
const RADIUS = 220;
const CX = SIZE / 2;
const CY = SIZE / 2;
const RING_OUTER = 270;
const STROKE = 96;
/** 环的中径：外径往回缩半个笔画宽 */
const RING_MID = RING_OUTER - STROKE / 2;

// 尾巴：沿 45° 穿过环的空心后探出环外。比环略细，否则两者糊成一块。
const TAIL_THICK = 86;
const TAIL_A = 70;
const TAIL_B = 312;
const D = Math.SQRT1_2; // cos45° = sin45°

// 蓝底渐变：与应用内 .logo 的那条 135° 渐变同色（#5b86ff → #2f57d6）
const FROM = [0x5b, 0x86, 0xff];
const TO = [0x2f, 0x57, 0xd6];

// ---- 逐像素合成 ----
const px = Buffer.alloc(SIZE * SIZE * 4);
for (let y = 0; y < SIZE; y++) {
  for (let x = 0; x < SIZE; x++) {
    // 蓝底：135° 即左上→右下，用 (x+y) 当参数
    const t = (x + y) / (2 * SIZE);
    const r = lerp(FROM[0], TO[0], t);
    const g = lerp(FROM[1], TO[1], t);
    const b = lerp(FROM[2], TO[2], t);

    // 圆角矩形的距离函数以矩形左上角为原点，这里把坐标挪到留白之后
    const inside = cover(
      sdRoundRect(x - MARGIN, y - MARGIN, SIZE - 2 * MARGIN, SIZE - 2 * MARGIN, RADIUS)
    );

    const q = cover(
      Math.min(
        sdRing(x, y, CX, CY, RING_MID, STROKE / 2),
        sdSegment(x, y, CX + TAIL_A * D, CY + TAIL_A * D, CX + TAIL_B * D, CY + TAIL_B * D) -
          TAIL_THICK / 2
      )
    ) * inside; // 白色只在蓝底之内（否则会在圆角外留下一圈白边）

    const i = (y * SIZE + x) * 4;
    px[i] = lerp(r, 255, q);
    px[i + 1] = lerp(g, 255, q);
    px[i + 2] = lerp(b, 255, q);
    px[i + 3] = Math.round(inside * 255);
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

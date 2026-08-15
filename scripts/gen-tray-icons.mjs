/**
 * 生成托盘状态图标（32x32 RGBA PNG，零第三方依赖：node:zlib + 手写 CRC32）。
 * 输出到 src-tauri/icons/tray/，由 Rust 编译期 include_bytes 嵌入：
 *   tray-idle.png   品牌蓝圆（服务未就绪/引导中）
 *   tray-ready.png  蓝圆 + 右下绿点（服务就绪）
 *   tray-error.png  蓝圆 + 右下红点（启动失败）
 *   tray-off.png    灰圆（桥断/连接中）
 * 用法：node scripts/gen-tray-icons.mjs
 */

import { deflateSync } from "node:zlib";
import { mkdirSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const SIZE = 32; // 输出尺寸
const SS = 2; // 2x 超采样抗锯齿
const N = SIZE * SS; // 采样网格

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = join(root, "src-tauri", "icons", "tray");

// ---- 像素生成 ----
function makePixels(bg, dot) {
  const px = new Uint8Array(SIZE * SIZE * 4); // RGBA
  const cx = (N - 1) / 2;
  const cy = (N - 1) / 2;
  const radius = 13 * SS;
  const dotX = 21.5 * SS, dotY = 21.5 * SS, dotR = 4.5 * SS;
  for (let y = 0; y < SIZE; y++) {
    for (let x = 0; x < SIZE; x++) {
      let r = 0, g = 0, b = 0, a = 0;
      for (let sy = 0; sy < SS; sy++) {
        for (let sx = 0; sx < SS; sx++) {
          const u = x * SS + sx + 0.5;
          const v = y * SS + sy + 0.5;
          const dbg = Math.hypot(u - cx, v - cy) <= radius;
          const ddot = Math.hypot(u - dotX, v - dotY) <= dotR;
          if (dbg || ddot) {
            const c = ddot ? dot : bg;
            r += c[0]; g += c[1]; b += c[2]; a += 255;
          }
        }
      }
      const n = SS * SS;
      const i = (y * SIZE + x) * 4;
      px[i] = Math.round(r / n);
      px[i + 1] = Math.round(g / n);
      px[i + 2] = Math.round(b / n);
      px[i + 3] = Math.round(a / n);
    }
  }
  return px;
}

// ---- 最小 PNG 编码器 ----
const CRC_TABLE = (() => {
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
  for (const byte of buf) c = CRC_TABLE[(c ^ byte) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}
function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}
function encodePng(px, w, h) {
  // 每行前置 filter byte 0
  const raw = Buffer.alloc(h * (1 + w * 4));
  for (let y = 0; y < h; y++) {
    raw[y * (1 + w * 4)] = 0;
    Buffer.from(px.buffer, y * w * 4, w * 4).copy(raw, y * (1 + w * 4) + 1);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// ---- 生成 ----
const BLUE = [0x4d, 0x7c, 0xfe];
const GRAY = [0x94, 0xa3, 0xb8];
const GREEN = [0x22, 0xc5, 0x5e];
const RED = [0xef, 0x44, 0x44];
const SLATE = [0x64, 0x74, 0x8b];

const variants = [
  ["tray-idle.png", BLUE, null],
  ["tray-ready.png", BLUE, GREEN],
  ["tray-error.png", BLUE, RED],
  ["tray-off.png", GRAY, SLATE],
];

mkdirSync(outDir, { recursive: true });
for (const [file, bg, dot] of variants) {
  const px = makePixels(bg, dot ?? [0, 0, 0]);
  const png = encodePng(px, SIZE, SIZE);
  writeFileSync(join(outDir, file), png);
  console.log(file, png.length, "bytes");
}
console.log("tray icons written to", outDir);

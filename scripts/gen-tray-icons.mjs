/**
 * 生成托盘状态图标（32x32 RGBA PNG，零第三方依赖：node:zlib + 手写 CRC32）。
 * 输出到 src-tauri/icons/tray/，由 Rust 编译期 include_bytes 嵌入：
 *   tray-idle.png   品牌蓝圆（服务未就绪/引导中）
 *   tray-ready.png  蓝圆 + 右下绿点（服务就绪）
 *   tray-error.png  蓝圆 + 右下红点（启动失败）
 *   tray-off.png    灰圆（桥断/连接中）
 * 用法：node scripts/gen-tray-icons.mjs
 */

import { deflateSync, inflateSync } from "node:zlib";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
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

// ---- 最小 PNG 解码器（含全部 5 种 filter） ----
function paeth(a, b, c) {
  const p = a + b - c;
  const pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
  return pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
}
function decodePng(buf) {
  let off = 8, idat = [], w = 0, h = 0, bitDepth = 0, colorType = 0;
  while (off + 12 <= buf.length) {
    const len = buf.readUInt32BE(off);
    const type = buf.toString("ascii", off + 4, off + 8);
    if (type === "IHDR") {
      w = buf.readUInt32BE(off + 8);
      h = buf.readUInt32BE(off + 12);
      bitDepth = buf[off + 16];
      colorType = buf[off + 17];
    } else if (type === "IDAT") {
      idat.push(buf.slice(off + 8, off + 8 + len));
    } else if (type === "IEND") break;
    off += 12 + len;
  }
  if (bitDepth !== 8 || (colorType !== 6 && colorType !== 2)) {
    throw new Error("只支持 8bit RGBA/RGB PNG");
  }
  const bpp = colorType === 6 ? 4 : 3;
  const raw = inflateSync(Buffer.concat(idat));
  const stride = w * bpp;
  const out = Buffer.alloc(h * stride);
  for (let y = 0; y < h; y++) {
    const filter = raw[y * (stride + 1)];
    const row = y * (stride + 1) + 1;
    for (let x = 0; x < stride; x++) {
      const rawByte = raw[row + x];
      const left = x >= bpp ? out[y * stride + x - bpp] : 0;
      const up = y > 0 ? out[(y - 1) * stride + x] : 0;
      const upLeft = y > 0 && x >= bpp ? out[(y - 1) * stride + x - bpp] : 0;
      let val;
      switch (filter) {
        case 0: val = rawByte; break;
        case 1: val = rawByte + left; break;
        case 2: val = rawByte + up; break;
        case 3: val = rawByte + ((left + up) >> 1); break;
        case 4: val = rawByte + paeth(left, up, upLeft); break;
        default: throw new Error("未知 filter " + filter);
      }
      out[y * stride + x] = val & 0xff;
    }
  }
  // RGB → RGBA
  if (colorType === 2) {
    const rgba = Buffer.alloc(w * h * 4);
    for (let i = 0; i < w * h; i++) {
      rgba[i * 4] = out[i * 3];
      rgba[i * 4 + 1] = out[i * 3 + 1];
      rgba[i * 4 + 2] = out[i * 3 + 2];
      rgba[i * 4 + 3] = 255;
    }
    return { width: w, height: h, pixels: rgba };
  }
  return { width: w, height: h, pixels: out };
}

// 在底图上叠加右下角状态点（2x 超采样抗锯齿）
function overlayDot(pixels, w, h, dot, dotX, dotY, dotR) {
  const out = Buffer.from(pixels);
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      let hit = false, r = 0, g = 0, b = 0, a = 0;
      for (let sy = 0; sy < 2; sy++) {
        for (let sx = 0; sx < 2; sx++) {
          const u = x * 2 + sx + 0.5;
          const v = y * 2 + sy + 0.5;
          if (Math.hypot(u - dotX, v - dotY) <= dotR) {
            hit = true; r += dot[0]; g += dot[1]; b += dot[2]; a += 255;
          }
        }
      }
      if (hit) {
        const i = (y * w + x) * 4;
        out[i] = Math.round(r / 4);
        out[i + 1] = Math.round(g / 4);
        out[i + 2] = Math.round(b / 4);
        out[i + 3] = Math.round(a / 4);
      }
    }
  }
  return out;
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

// ---- 生成：以软件图标（src-tauri/icons/32x32.png）为底图，叠加状态角标 ----
const GREEN = [0x22, 0xc5, 0x5e];
const RED = [0xef, 0x44, 0x44];
const SLATE = [0x64, 0x74, 0x8b];

const base = decodePng(readFileSync(join(root, "src-tauri", "icons", "32x32.png")));
const W = base.width;
const H = base.height;

const variants = [
  ["tray-idle.png", null],
  ["tray-ready.png", GREEN],
  ["tray-error.png", RED],
  ["tray-off.png", SLATE],
];

mkdirSync(outDir, { recursive: true });
for (const [file, dot] of variants) {
  let px = base.pixels;
  if (dot) {
    // 右下角状态点（2x 超采样坐标）
    px = overlayDot(px, W, H, dot, (W - 5.5) * 2, (H - 5.5) * 2, 4 * 2);
  }
  writeFileSync(join(outDir, file), encodePng(px, W, H));
  console.log(file, "32x32 底图 +", dot ? "角标" : "原样");
}
console.log("tray icons written to", outDir);
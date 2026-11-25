// Encoder of the zstd image type (molcajete_protocol::zimage) for tests:
// indexed pixels for up to 256 colours, RGB otherwise, in one Zstandard
// frame. Needs node:zlib with Zstandard (Node 22.15 or later).
import zlib from "node:zlib";

export const ZSTD_IMAGE = "image/x-molcajete-zstd-v1";

/** An image from `pixel(x, y)`, which returns `[r, g, b]`. */
export function zstdImage(width, height, pixel) {
  const colors = new Map();
  const pixels = [];
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const [r, g, b] = pixel(x, y);
      const key = (r << 16) | (g << 8) | b;
      if (!colors.has(key)) colors.set(key, colors.size);
      pixels.push(key);
    }
  }
  const size = Buffer.alloc(4);
  size.writeUInt16LE(width, 0);
  size.writeUInt16LE(height, 2);
  const rgb = (key) => [key >> 16, (key >> 8) & 0xff, key & 0xff];
  if (colors.size > 256) {
    const raw = Buffer.from(pixels.flatMap(rgb));
    return Buffer.concat([Buffer.from([1]), size, zlib.zstdCompressSync(raw)]);
  }
  const bits = colors.size <= 2 ? 1 : colors.size <= 4 ? 2 : colors.size <= 16 ? 4 : 8;
  const rowBytes = Math.ceil((width * bits) / 8);
  const raw = Buffer.alloc(rowBytes * height);
  pixels.forEach((key, i) => {
    const x = i % width;
    const y = (i - x) / width;
    const shift = 8 - bits * ((x % (8 / bits)) + 1);
    raw[y * rowBytes + Math.floor((x * bits) / 8)] |= colors.get(key) << shift;
  });
  return Buffer.concat([
    Buffer.from([0]),
    size,
    Buffer.from([colors.size - 1]),
    Buffer.from([...colors.keys()].flatMap(rgb)),
    zlib.zstdCompressSync(raw),
  ]);
}

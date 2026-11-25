// Pixel-level tests of the WebGL2 renderer.
//
// The wasm client is loaded from web/public/pkg into headless Chromium (SwiftShader),
// Guacamole instructions are fed directly (no server), and layer / canvas
// pixels are read back and compared with the values the HTML-canvas based
// reference client would produce.
//
//   scripts/build-web.sh && (cd tests/browser && npm install && npm test)
//
// Set CHROME=/path/to/chrome to choose the browser binary.
import { chromium } from "playwright-core";
import { readFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { ZSTD_IMAGE, zstdImage } from "./zstd-image.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const web = path.join(root, "web/public");

function findChrome() {
  if (process.env.CHROME) return process.env.CHROME;
  const cache = path.join(process.env.HOME ?? "", ".cache/ms-playwright");
  for (const dir of ["chromium-1228/chrome-linux64/chrome", "chromium-1228/chrome-linux/chrome"]) {
    const candidate = path.join(cache, dir);
    if (existsSync(candidate)) return candidate;
  }
  return undefined;
}

const browser = await chromium.launch({
  executablePath: findChrome(),
  args: ["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"],
});
const page = await browser.newPage({ viewport: { width: 400, height: 300 } });
page.on("pageerror", (e) => console.error("[pageerror]", e.message));
page.on("console", (m) => m.type() === "error" && console.error("[console]", m.text()));

// Serve web/public/ from a fake origin so no HTTP server is needed.
await page.route("http://guac.test/**", async (route) => {
  const url = new URL(route.request().url());
  if (url.pathname === "/") {
    return route.fulfill({
      contentType: "text/html",
      body: `<!doctype html><div id="c" style="width:200px;height:200px;position:relative"></div>
<script type="module">
import init, { MolcajeteClient } from "./pkg/molcajete_client.js";
await init();
window.MolcajeteClient = MolcajeteClient;
window.ready = true;
</script>`,
    });
  }
  const file = path.join(web, url.pathname);
  const type = file.endsWith(".wasm") ? "application/wasm" : "text/javascript";
  return route.fulfill({ contentType: type, body: await readFile(file) });
});
await page.goto("http://guac.test/");
await page.waitForFunction(() => window.ready === true);

// Helpers available inside the page.
await page.evaluate(() => {
  const encode = (v) => {
    const s = String(v);
    return `${[...s].length}.${s}`;
  };
  window.ins = (op, ...args) => [op, ...args].map(encode).join(",") + ";";
  window.fresh = () => {
    window.client?.free();
    document.getElementById("c").replaceChildren();
    window.client = new window.MolcajeteClient(document.getElementById("c"));
    return true;
  };
  window.feed = (...instructions) => window.client.debug_feed(instructions.join(""));
  window.px = (layer, x, y) => Array.from(window.client.debug_layer_pixel(layer, x, y) ?? []);
  window.cpx = (x, y) => Array.from(window.client.debug_canvas_pixel(x, y) ?? []).slice(0, 3);
  window.settle = async () => {
    for (let i = 0; i < 200 && window.client.debug_queued() > 0; i++) await new Promise((r) => setTimeout(r, 10));
  };
  window.pngBase64 = async (w, h, pixels) => {
    const canvas = new OffscreenCanvas(w, h);
    const ctx = canvas.getContext("2d");
    const data = ctx.createImageData(w, h);
    data.data.set(pixels);
    ctx.putImageData(data, 0, 0);
    const blob = await canvas.convertToBlob({ type: "image/png" });
    const bytes = new Uint8Array(await blob.arrayBuffer());
    let bin = "";
    for (const b of bytes) bin += String.fromCharCode(b);
    return btoa(bin);
  };
  window.jpegBase64 = async (w, h, color) => {
    const canvas = new OffscreenCanvas(w, h);
    const ctx = canvas.getContext("2d");
    ctx.fillStyle = color;
    ctx.fillRect(0, 0, w, h);
    const blob = await canvas.convertToBlob({ type: "image/jpeg", quality: 0.95 });
    const bytes = new Uint8Array(await blob.arrayBuffer());
    let bin = "";
    for (const b of bytes) bin += String.fromCharCode(b);
    return btoa(bin);
  };
});

let failures = 0;
let passed = 0;

function near(actual, expected, tolerance = 2) {
  return (
    Array.isArray(actual) &&
    actual.length === expected.length &&
    actual.every((v, i) => Math.abs(v - expected[i]) <= tolerance)
  );
}

async function test(name, body) {
  await page.evaluate(() => window.fresh());
  try {
    await body();
    passed++;
    console.log(`ok   ${name}`);
  } catch (err) {
    failures++;
    console.log(`FAIL ${name}\n     ${err.message}`);
  }
}

async function expectPixel(what, script, expected, tolerance) {
  const actual = await page.evaluate(script);
  if (!near(actual, expected, tolerance)) {
    throw new Error(`${what}: expected [${expected}] got [${actual}]`);
  }
}

const RED = [255, 0, 0, 255];
const CLEAR = [0, 0, 0, 0];

await test("cfill source-over (opaque and translucent)", async () => {
  await page.evaluate(() =>
    feed(ins("size", 0, 64, 64), ins("rect", 0, 8, 8, 16, 16), ins("cfill", 14, 0, 255, 0, 0, 255), ins("sync", 1)),
  );
  await expectPixel("inside", () => px(0, 10, 10), RED);
  await expectPixel("outside", () => px(0, 4, 4), CLEAR);
  await page.evaluate(() => feed(ins("rect", 0, 8, 8, 16, 16), ins("cfill", 14, 0, 0, 0, 255, 128), ins("sync", 2)));
  await expectPixel("blended", () => px(0, 10, 10), [127, 0, 128, 255]);
});

await test("copy (SRC) clears outside the shape like canvas", async () => {
  await page.evaluate(() =>
    feed(
      ins("size", 0, 64, 64),
      ins("rect", 0, 0, 0, 64, 64), ins("cfill", 14, 0, 255, 0, 0, 255),
      ins("rect", 0, 10, 10, 4, 4), ins("cfill", 12, 0, 0, 255, 0, 255),
      ins("sync", 1),
    ),
  );
  await expectPixel("inside", () => px(0, 11, 11), [0, 255, 0, 255]);
  await expectPixel("outside cleared", () => px(0, 40, 40), CLEAR);
});

await test("SRC with multiple rectangles (mask path)", async () => {
  await page.evaluate(() =>
    feed(
      ins("size", 0, 64, 64),
      ins("rect", 0, 0, 0, 64, 64), ins("cfill", 14, 0, 255, 0, 0, 255),
      ins("rect", 0, 10, 10, 4, 4), ins("rect", 0, 30, 30, 4, 4), ins("cfill", 12, 0, 0, 0, 255, 96),
      ins("sync", 1),
    ),
  );
  await expectPixel("first rect", () => px(0, 11, 11), [0, 0, 96, 96]);
  await expectPixel("second rect", () => px(0, 31, 31), [0, 0, 96, 96]);
  await expectPixel("outside cleared", () => px(0, 50, 50), CLEAR);
});

await test("destination-out (ROUT) erases", async () => {
  await page.evaluate(() =>
    feed(
      ins("size", 0, 64, 64),
      ins("rect", 0, 0, 0, 64, 64), ins("cfill", 14, 0, 255, 0, 0, 255),
      ins("rect", 0, 10, 10, 10, 10), ins("cfill", 2, 0, 0, 0, 0, 255),
      ins("sync", 1),
    ),
  );
  await expectPixel("erased", () => px(0, 12, 12), CLEAR);
  await expectPixel("kept", () => px(0, 40, 40), RED);
});

await test("destination-atop with transparent color clears a buffer (guacd idiom)", async () => {
  await page.evaluate(() =>
    feed(
      ins("rect", -3, 0, 0, 11, 16), ins("cfill", 14, -3, 255, 0, 0, 255),
      ins("rect", -3, 0, 0, 11, 16), ins("cfill", 9, -3, 0, 0, 0, 0),
      ins("sync", 1),
    ),
  );
  const size = await page.evaluate(() => Array.from(client.debug_layer_size(-3)));
  if (size[0] !== 11 || size[1] !== 16) throw new Error(`buffer size ${size}`);
  await expectPixel("cleared", () => px(-3, 5, 5), CLEAR);
});

await test("overlapping copy within one layer (scrolling)", async () => {
  await page.evaluate(() =>
    feed(
      ins("size", 0, 64, 64),
      ins("rect", 0, 0, 0, 32, 64), ins("cfill", 14, 0, 255, 0, 0, 255),
      ins("rect", 0, 32, 0, 32, 64), ins("cfill", 14, 0, 0, 0, 255, 255),
      ins("copy", 0, 0, 0, 48, 64, 14, 0, 16, 0),
      ins("sync", 1),
    ),
  );
  await expectPixel("untouched", () => px(0, 8, 5), RED);
  await expectPixel("shifted red", () => px(0, 47, 5), RED);
  await expectPixel("shifted blue", () => px(0, 48, 5), [0, 0, 255, 255]);
});

await test("copy between layers honours the composite mode", async () => {
  await page.evaluate(() =>
    feed(
      ins("size", 0, 32, 32),
      ins("rect", 0, 0, 0, 32, 32), ins("cfill", 14, 0, 255, 0, 0, 255),
      ins("rect", -1, 0, 0, 8, 8), ins("cfill", 14, -1, 0, 0, 255, 128),
      ins("copy", -1, 0, 0, 8, 8, 14, 0, 4, 4),
      ins("sync", 1),
    ),
  );
  await expectPixel("blended copy", () => px(0, 5, 5), [127, 0, 128, 255]);
});

await test("transfer XOR and NOT_SRC", async () => {
  await page.evaluate(() =>
    feed(
      ins("size", 0, 16, 16),
      ins("rect", 0, 0, 0, 16, 16), ins("cfill", 14, 0, 0x0f, 0x0f, 0x55, 255),
      ins("rect", -1, 0, 0, 4, 4), ins("cfill", 14, -1, 0xf0, 0x0f, 0xaa, 255),
      ins("transfer", -1, 0, 0, 4, 4, 6, 0, 0, 0),
      ins("transfer", -1, 0, 0, 4, 4, 12, 0, 8, 8),
      ins("sync", 1),
    ),
  );
  await expectPixel("xor", () => px(0, 1, 1), [0xff, 0x00, 0xff, 255]);
  await expectPixel("not src", () => px(0, 9, 9), [0x0f, 0xf0, 0x55, 255]);
  await expectPixel("outside", () => px(0, 6, 6), [0x0f, 0x0f, 0x55, 255]);
});

await test("transfer SRC replaces pixels including alpha", async () => {
  await page.evaluate(() =>
    feed(
      ins("size", 0, 16, 16),
      ins("rect", 0, 0, 0, 16, 16), ins("cfill", 14, 0, 255, 0, 0, 255),
      ins("rect", -1, 0, 0, 4, 4), ins("cfill", 14, -1, 0, 0, 255, 128),
      ins("transfer", -1, 0, 0, 4, 4, 3, 0, 2, 2),
      ins("sync", 1),
    ),
  );
  await expectPixel("replaced", () => px(0, 3, 3), [0, 0, 128, 128]);
});

await test("PNG stream with alpha is premultiplied correctly", async () => {
  await page.evaluate(async () => {
    const png = await pngBase64(2, 1, [255, 0, 0, 255, 0, 255, 0, 128]);
    feed(
      ins("size", 0, 16, 16),
      ins("rect", 0, 0, 0, 16, 16), ins("cfill", 14, 0, 0, 0, 255, 255),
      ins("img", 1, 14, 0, "image/png", 4, 4),
      ins("blob", 1, png.slice(0, 8)), ins("blob", 1, png.slice(8)), ins("end", 1),
      ins("sync", 1),
    );
    await settle();
  });
  await expectPixel("opaque pixel", () => px(0, 4, 4), RED);
  await expectPixel("translucent pixel", () => px(0, 5, 4), [0, 128, 127, 255]);
  await expectPixel("untouched", () => px(0, 8, 8), [0, 0, 255, 255]);
});

await test("JPEG stream (direct texture upload path)", async () => {
  await page.evaluate(async () => {
    const jpeg = await jpegBase64(8, 8, "rgb(40, 160, 220)");
    feed(ins("size", 0, 32, 32), ins("img", 3, 14, 0, "image/jpeg", 8, 8), ins("blob", 3, jpeg), ins("end", 3), ins("sync", 1));
    await settle();
  });
  await expectPixel("jpeg color", () => px(0, 11, 11), [40, 160, 220, 255], 8);
  await expectPixel("outside", () => px(0, 2, 2), CLEAR);
});

await test("frames are applied atomically after images decode", async () => {
  const before = await page.evaluate(async () => {
    const png = await pngBase64(1, 1, [0, 255, 0, 255]);
    feed(
      ins("size", 0, 8, 8),
      ins("img", 1, 14, 0, "image/png", 0, 0), ins("blob", 1, png), ins("end", 1),
      ins("rect", 0, 4, 4, 2, 2), ins("cfill", 14, 0, 255, 0, 0, 255),
    );
    // No sync yet: nothing may be drawn.
    const early = px(0, 4, 4);
    feed(ins("sync", 1));
    await settle();
    return early;
  });
  if (before.length && before.some((v) => v !== 0)) throw new Error(`drawn before sync: ${before}`);
  await expectPixel("image", () => px(0, 0, 0), [0, 255, 0, 255]);
  await expectPixel("rect", () => px(0, 4, 4), RED);
});

await test("buffers grow automatically", async () => {
  await page.evaluate(() => feed(ins("rect", -5, 100, 50, 10, 10), ins("cfill", 14, -5, 255, 0, 0, 255), ins("sync", 1)));
  const size = await page.evaluate(() => Array.from(client.debug_layer_size(-5)));
  if (size[0] !== 110 || size[1] !== 60) throw new Error(`buffer size ${size}`);
  await expectPixel("drawn", () => px(-5, 105, 55), RED);
});

await test("layer tree composite: move, z-order and shade", async () => {
  await page.evaluate(() =>
    feed(
      ins("size", 0, 64, 64),
      ins("rect", 0, 0, 0, 64, 64), ins("cfill", 14, 0, 255, 0, 0, 255),
      ins("size", 1, 16, 16), ins("rect", 1, 0, 0, 16, 16), ins("cfill", 14, 1, 0, 0, 255, 255),
      ins("move", 1, 0, 20, 20, 1), ins("shade", 1, 128),
      ins("size", 2, 8, 8), ins("rect", 2, 0, 0, 8, 8), ins("cfill", 14, 2, 0, 255, 0, 255),
      ins("move", 2, 1, 12, 12, 0),
      ins("sync", 1),
    ),
  );
  await expectPixel("base", () => cpx(10, 10), [255, 0, 0]);
  await expectPixel("half-transparent child", () => cpx(25, 25), [127, 0, 128]);
  // Layer 2 is inside layer 1 (inherits its opacity) and clipped to it.
  await expectPixel("grandchild", () => cpx(33, 33), [127, 128, 0]);
  await expectPixel("grandchild clipped", () => cpx(38, 38), [255, 0, 0]);
  await page.evaluate(() => feed(ins("dispose", 1), ins("sync", 2)));
  await expectPixel("after dispose", () => cpx(25, 25), [255, 0, 0]);
});

await test("arc fill (tessellated path)", async () => {
  await page.evaluate(() =>
    feed(ins("size", 0, 64, 64), ins("arc", 0, 32, 32, 20, 0, 6.283185307, 0), ins("cfill", 14, 0, 0, 255, 0, 255), ins("sync", 1)),
  );
  await expectPixel("centre", () => px(0, 32, 32), [0, 255, 0, 255]);
  await expectPixel("near edge", () => px(0, 32, 14), [0, 255, 0, 255]);
  await expectPixel("outside", () => px(0, 32, 9), CLEAR);
  await expectPixel("corner", () => px(0, 14, 14), CLEAR);
});

await test("rectangular clip, then reset", async () => {
  await page.evaluate(() =>
    feed(
      ins("size", 0, 64, 64),
      ins("rect", 0, 10, 10, 10, 10), ins("clip", 0),
      ins("rect", 0, 0, 0, 64, 64), ins("cfill", 14, 0, 255, 0, 0, 255),
      ins("sync", 1),
    ),
  );
  await expectPixel("inside clip", () => px(0, 15, 15), RED);
  await expectPixel("outside clip", () => px(0, 5, 5), CLEAR);
  await page.evaluate(() => feed(ins("reset", 0), ins("rect", 0, 0, 0, 64, 64), ins("cfill", 14, 0, 0, 0, 255, 255), ins("sync", 2)));
  await expectPixel("after reset", () => px(0, 5, 5), [0, 0, 255, 255]);
});

await test("circular clip (mask) with push/pop", async () => {
  await page.evaluate(() =>
    feed(
      ins("size", 0, 64, 64),
      ins("push", 0),
      ins("arc", 0, 32, 32, 10, 0, 6.283185307, 0), ins("clip", 0),
      ins("rect", 0, 0, 0, 64, 64), ins("cfill", 14, 0, 255, 0, 0, 255),
      ins("pop", 0),
      ins("rect", 0, 0, 0, 4, 4), ins("cfill", 14, 0, 0, 255, 0, 255),
      ins("sync", 1),
    ),
  );
  await expectPixel("inside circle", () => px(0, 32, 32), RED);
  await expectPixel("outside circle", () => px(0, 50, 50), CLEAR);
  await expectPixel("after pop", () => px(0, 1, 1), [0, 255, 0, 255]);
});

await test("lfill repeats a buffer as a pattern", async () => {
  await page.evaluate(() =>
    feed(
      ins("size", 0, 16, 16),
      ins("size", -2, 2, 1),
      ins("rect", -2, 0, 0, 1, 1), ins("cfill", 14, -2, 255, 0, 0, 255),
      ins("rect", -2, 1, 0, 1, 1), ins("cfill", 14, -2, 0, 0, 255, 255),
      ins("rect", 0, 0, 0, 8, 8), ins("lfill", 14, 0, -2),
      ins("sync", 1),
    ),
  );
  await expectPixel("x=0", () => px(0, 0, 3), RED);
  await expectPixel("x=1", () => px(0, 1, 3), [0, 0, 255, 255]);
  await expectPixel("x=6 (repeat)", () => px(0, 6, 5), RED);
  await expectPixel("outside", () => px(0, 10, 10), CLEAR);
});

await test("stroke a line", async () => {
  await page.evaluate(() =>
    feed(
      ins("size", 0, 32, 32),
      ins("start", 0, 2, 16), ins("line", 0, 30, 16),
      ins("cstroke", 14, 0, 0, 1, 4, 255, 255, 0, 255),
      ins("sync", 1),
    ),
  );
  await expectPixel("on line", () => px(0, 16, 16), [255, 255, 0, 255]);
  await expectPixel("off line", () => px(0, 16, 24), CLEAR);
});

await test("transform affects subsequent drawing", async () => {
  await page.evaluate(() =>
    feed(
      ins("size", 0, 32, 32),
      ins("transform", 0, 1, 0, 0, 1, 10, 10),
      ins("rect", 0, 0, 0, 4, 4), ins("cfill", 14, 0, 255, 0, 0, 255),
      ins("identity", 0),
      ins("rect", 0, 0, 0, 2, 2), ins("cfill", 14, 0, 0, 0, 255, 255),
      ins("sync", 1),
    ),
  );
  await expectPixel("translated", () => px(0, 11, 11), RED);
  await expectPixel("identity", () => px(0, 1, 1), [0, 0, 255, 255]);
  await expectPixel("untouched", () => px(0, 5, 5), CLEAR);
});

await test("resize keeps existing content", async () => {
  await page.evaluate(() =>
    feed(
      ins("size", 0, 32, 32),
      ins("rect", 0, 0, 0, 32, 32), ins("cfill", 14, 0, 255, 0, 0, 255),
      ins("size", 0, 200, 150),
      ins("sync", 1),
    ),
  );
  await expectPixel("kept", () => px(0, 20, 20), RED);
  await expectPixel("new area", () => px(0, 100, 100), CLEAR);
});

await test("cursor instruction produces a CSS cursor", async () => {
  const cursor = await page.evaluate(async () => {
    feed(
      ins("size", 0, 32, 32),
      ins("rect", -2, 0, 0, 4, 4), ins("cfill", 14, -2, 255, 255, 255, 255),
      ins("cursor", 1, 1, -2, 0, 0, 4, 4),
      ins("sync", 1),
    );
    // The cursor image is read back asynchronously (PBO + fence).
    const canvas = document.querySelector("canvas");
    for (let i = 0; i < 100 && !canvas.style.cursor.startsWith("url("); i++) {
      await new Promise((r) => requestAnimationFrame(r));
    }
    return canvas.style.cursor;
  });
  if (!cursor.startsWith('url("data:image/png')) throw new Error(`cursor: ${cursor.slice(0, 40)}`);
  if (!cursor.includes(" 1 1")) throw new Error(`hotspot missing: ${cursor.slice(-30)}`);
});

// ---------------------------------------------------------------------------
// zstd images (molcajete_protocol::zimage): decoded in wasm when their frame
// is drawn, and uploaded from wasm memory.
// ---------------------------------------------------------------------------

const b64 = (buffer) => buffer.toString("base64");
const GREEN = [0, 255, 0, 255];
const solid = (color) => () => color.slice(0, 3);
/** `img` + `blob`s (cut at `chunk` base64 characters) + `end`. */
const zstdStream = (stream, mode, layer, x, y, image, chunk = 4096) => {
  const data = b64(image);
  const blobs = [];
  for (let i = 0; i < data.length; i += chunk) blobs.push(["blob", stream, data.slice(i, i + chunk)]);
  return [["img", stream, mode, layer, ZSTD_IMAGE, x, y], ...blobs, ["end", stream]];
};
/** Feeds instructions given as arrays and waits for the queue to drain. */
const feedAll = (instructions) =>
  page.evaluate(async (list) => {
    feed(...list.map((i) => ins(...i)));
    await settle();
  }, instructions);

await test("zstd image: indexed and RGB pixels are exact", async () => {
  // 2, 3, 9 and 200 colours take 1, 2, 4 and 8 bits per index; the widths
  // leave padding at the end of every row.
  const shade = (n) => (x, y) => {
    const c = (x * 7 + y * 13) % n;
    return [c, 255 - c, (c * 31) & 0xff];
  };
  const many = (x, y) => [x * 9, y * 11, (x * y) & 0xff];
  await feedAll([
    ["size", 0, 128, 64],
    ...zstdStream(1, 14, 0, 0, 0, zstdImage(9, 5, shade(2))),
    ...zstdStream(2, 14, 0, 16, 0, zstdImage(7, 5, shade(3))),
    ...zstdStream(3, 14, 0, 32, 0, zstdImage(13, 5, shade(9))),
    ...zstdStream(4, 14, 0, 48, 0, zstdImage(33, 9, shade(200))),
    // More than 256 colours: RGB, in several blobs.
    ...zstdStream(5, 14, 0, 0, 16, zstdImage(28, 23, many), 100),
    ["sync", 1],
  ]);
  const cases = [[0, 2, 9, 5], [16, 3, 7, 5], [32, 9, 13, 5], [48, 200, 33, 9]];
  for (const [left, n, w, h] of cases) {
    for (const [x, y] of [[0, 0], [w - 1, 0], [0, h - 1], [w - 1, h - 1], [w >> 1, h >> 1]]) {
      await expectPixel(`${n} colours at ${x},${y}`, `px(0, ${left + x}, ${y})`, [...shade(n)(x, y), 255], 0);
    }
  }
  for (const [x, y] of [[0, 0], [27, 0], [0, 22], [27, 22], [13, 7]]) {
    await expectPixel(`RGB at ${x},${y}`, `px(0, ${x}, ${16 + y})`, [...many(x, y), 255], 0);
  }
  await expectPixel("outside", () => px(0, 100, 40), CLEAR);
});

await test("zstd image through a clip and a transform", async () => {
  await feedAll([
    ["size", 0, 64, 64],
    ["rect", 0, 0, 0, 64, 64], ["cfill", 14, 0, 0, 0, 255, 255],
    ["push", 0],
    ["rect", 0, 10, 10, 4, 4], ["clip", 0],
    ...zstdStream(1, 14, 0, 8, 8, zstdImage(8, 8, solid(RED))),
    ["pop", 0],
    ["transform", 0, 1, 0, 0, 1, 20, 30],
    ...zstdStream(2, 14, 0, 0, 0, zstdImage(8, 8, solid(GREEN))),
    ["sync", 1],
  ]);
  await expectPixel("inside the clip", () => px(0, 11, 11), RED);
  await expectPixel("image outside the clip", () => px(0, 9, 9), [0, 0, 255, 255]);
  await expectPixel("translated image", () => px(0, 23, 33), GREEN);
  await expectPixel("where it would be untranslated", () => px(0, 3, 3), [0, 0, 255, 255]);
});

await test("zstd image with SRC, ROVER and into a buffer", async () => {
  await feedAll([
    ["size", 0, 32, 32],
    ["rect", 0, 0, 0, 32, 32], ["cfill", 14, 0, 255, 0, 0, 255],
    // SRC replaces the whole layer: what the image does not cover is cleared.
    ...zstdStream(1, 12, 0, 4, 4, zstdImage(4, 4, solid(GREEN))),
    // ROVER draws behind what is there.
    ["size", 1, 32, 32],
    ["rect", 1, 0, 0, 8, 8], ["cfill", 14, 1, 255, 0, 0, 255],
    ...zstdStream(2, 11, 1, 0, 0, zstdImage(16, 16, solid(GREEN))),
    // A buffer grows to fit an image.
    ...zstdStream(3, 12, -3, 0, 0, zstdImage(40, 24, solid([1, 2, 3]))),
    ["sync", 1],
  ]);
  await expectPixel("SRC image", () => px(0, 5, 5), GREEN);
  await expectPixel("SRC clears the rest", () => px(0, 20, 20), CLEAR);
  await expectPixel("ROVER keeps what was drawn", () => px(1, 4, 4), RED);
  await expectPixel("ROVER fills the rest", () => px(1, 12, 12), GREEN);
  await expectPixel("ROVER outside the image", () => px(1, 20, 20), CLEAR);
  await expectPixel("buffer", () => px(-3, 39, 23), [1, 2, 3, 255]);
  const size = await page.evaluate(() => Array.from(client.debug_layer_size(-3)));
  if (size.join() !== "40,24") throw new Error(`buffer size ${size}`);
});

await test("zstd, PNG and JPEG in one frame are drawn in order", async () => {
  // The zstd images are ready as soon as they have arrived, the others
  // only after their decoders: each must still cover what came before it.
  const [red, white, blue] = await page.evaluate(async () => [
    await pngBase64(16, 16, new Array(256).fill([255, 0, 0, 255]).flat()),
    await pngBase64(8, 8, new Array(64).fill([255, 255, 255, 255]).flat()),
    await jpegBase64(16, 16, "rgb(0, 0, 255)"),
  ]);
  await feedAll([
    ["size", 0, 64, 64],
    ["img", 1, 14, 0, "image/png", 0, 0], ["blob", 1, red], ["end", 1],
    ...zstdStream(2, 14, 0, 8, 0, zstdImage(16, 16, solid(GREEN))),
    ["img", 3, 14, 0, "image/jpeg", 16, 0], ["blob", 3, blue], ["end", 3],
    ...zstdStream(4, 14, 0, 28, 0, zstdImage(8, 16, solid([255, 255, 0]))),
    ["img", 5, 14, 0, "image/png", 30, 4], ["blob", 5, white], ["end", 5],
    ["sync", 1],
    // The next frame is complete at once, and must still come second.
    ...zstdStream(6, 14, 0, 0, 8, zstdImage(4, 4, solid([9, 9, 9]))),
    ["sync", 2],
  ]);
  await expectPixel("PNG", () => px(0, 2, 2), RED);
  await expectPixel("zstd over PNG", () => px(0, 12, 2), GREEN);
  await expectPixel("JPEG over zstd", () => px(0, 20, 2), [0, 0, 255, 255], 8);
  await expectPixel("zstd over JPEG", () => px(0, 29, 2), [255, 255, 0, 255]);
  await expectPixel("PNG over zstd", () => px(0, 32, 6), [255, 255, 255, 255]);
  await expectPixel("second frame over the first", () => px(0, 2, 10), [9, 9, 9, 255]);
  await expectPixel("first frame beside it", () => px(0, 6, 10), RED);
});

await test("damaged zstd image is skipped and does not hold its frame", async () => {
  const good = zstdImage(8, 8, solid(GREEN));
  const cut = good.subarray(0, good.length - 3);
  const noise = Buffer.from(Array.from({ length: 64 }, (_, i) => (i * 37) & 0xff));
  await feedAll([
    ["size", 0, 32, 32],
    ["rect", 0, 0, 0, 32, 32], ["cfill", 14, 0, 0, 0, 255, 255],
    ...zstdStream(1, 14, 0, 0, 0, cut),
    ...zstdStream(2, 14, 0, 8, 0, noise),
    ...zstdStream(3, 14, 0, 16, 0, Buffer.alloc(0)),
    ...zstdStream(4, 14, 0, 24, 0, good),
    ["sync", 1],
  ]);
  const queued = await page.evaluate(() => client.debug_queued());
  if (queued !== 0) throw new Error(`${queued} operations still queued`);
  for (const x of [2, 10, 18]) await expectPixel(`damaged image at ${x}`, `px(0, ${x}, 2)`, [0, 0, 255, 255]);
  await expectPixel("the image after them", () => px(0, 26, 2), GREEN);
});

await browser.close();
console.log(`\n${passed} passed, ${failures} failed`);
process.exit(failures ? 1 : 0);

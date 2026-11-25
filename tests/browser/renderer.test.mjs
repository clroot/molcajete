// Pixel-level tests of the WebGL2 renderer.
//
// The wasm client is loaded from web/pkg into headless Chromium (SwiftShader),
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

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const web = path.join(root, "web");

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

// Serve web/ from a fake origin so no HTTP server is needed.
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

await browser.close();
console.log(`\n${passed} passed, ${failures} failed`);
process.exit(failures ? 1 : 0);

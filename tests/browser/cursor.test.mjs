// The size of the remote pointer as a CSS cursor, in worker mode and in
// main-thread mode: one pixel of its image per pixel of the remote
// screen, whatever the number of remote pixels per CSS pixel.
//
//   scripts/build-web.sh && (cd tests/browser && npm install && node cursor.test.mjs)
//
// Set CHROME=/path/to/chrome to choose the browser binary.
import { chromium } from "playwright-core";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";
import path from "node:path";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const web = path.join(root, "web/public");

const encode = (v) => `${[...String(v)].length}.${v}`;
const ins = (op, ...args) => [op, ...args].map(encode).join(",") + ";";

const browser = await chromium.launch({
  executablePath: process.env.CHROME,
  args: ["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"],
});

/** Opens a session in a 200×200 container on a screen of `dpr` device pixels per CSS pixel. */
async function open(mode, dpr, { imageSet = true } = {}) {
  const page = await browser.newPage({ viewport: { width: 400, height: 300 }, deviceScaleFactor: dpr });
  page.on("pageerror", (e) => console.error("[pageerror]", e.message));
  if (!imageSet) {
    // What a browser without `image-set()` in `cursor` does: the value is
    // not supported, and setting it leaves the property as it was.
    await page.addInitScript(() => {
      const dense = (value) => String(value).startsWith("image-set(");
      const supports = CSS.supports.bind(CSS);
      CSS.supports = (property, value) => !(property === "cursor" && dense(value)) && supports(property, value);
      const setProperty = CSSStyleDeclaration.prototype.setProperty;
      CSSStyleDeclaration.prototype.setProperty = function (property, value, ...rest) {
        if (property === "cursor" && dense(value)) return;
        setProperty.call(this, property, value, ...rest);
      };
      const cursor = Object.getOwnPropertyDescriptor(CSSStyleDeclaration.prototype, "cursor");
      if (cursor?.set) {
        Object.defineProperty(CSSStyleDeclaration.prototype, "cursor", {
          ...cursor,
          set(value) {
            if (!dense(value)) cursor.set.call(this, value);
          },
        });
      }
    });
  }
  await page.route("http://guac.test/**", async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === "/") {
      return route.fulfill({
        contentType: "text/html",
        body: `<!doctype html><div id="c" style="width:200px;height:200px;position:relative"></div>
<script type="module">
import { MolcajeteSession } from "./molcajete.js";
window.session = await MolcajeteSession.create(document.getElementById("c"), { worker: ${mode === "worker"} });
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
  assert.equal(await page.evaluate(() => window.session.mode), mode);
  return page;
}

const feed = (page, ...instructions) =>
  page.evaluate((data) => window.session.call("debug_feed", data), instructions.join(""));

/** A display of `size` pixels each way with an opaque 8×6 pointer, hotspot at 4,2. */
const withPointer = (size, frame) => [
  ins("size", 0, size, size),
  ins("size", -1, 8, 6),
  ins("rect", -1, 0, 0, 8, 6),
  ins("cfill", 14, -1, 255, 255, 255, 255),
  ins("cursor", 4, 2, -1, 0, 0, 8, 6),
  ins("sync", frame),
];

/** The canvas's cursor once it matches (its image is read back asynchronously). */
async function cursor(page, pattern) {
  let value = "";
  for (let i = 0; i < 200; i++) {
    value = await page.evaluate(() => document.querySelector("canvas").style.cursor);
    if (pattern.test(value)) return value;
    await page.waitForTimeout(25);
  }
  return value;
}

/** Width and height of the PNG in a cursor value. */
function imageSize(value) {
  const png = Buffer.from(value.match(/data:image\/png;base64,([A-Za-z0-9+/=]+)/)[1], "base64");
  return [png.readUInt32BE(16), png.readUInt32BE(20)];
}

const PLAIN = /^url\("data:image\/png[^"]*"\) (\d+) (\d+), auto$/;
const DENSE = /^image-set\(url\("data:image\/png[^"]*"\) ([\d.]+)x\) (\d+) (\d+), auto$/;

for (const mode of ["worker", "main"]) {
  // One remote pixel per CSS pixel: the image as it is.
  {
    const page = await open(mode, 1);
    await feed(page, ...withPointer(200, 1));
    const value = await cursor(page, PLAIN);
    assert.deepEqual(value.match(PLAIN)?.slice(1), ["4", "2"], `${mode}: plain cursor, got ${value.slice(-40)}`);
    assert.deepEqual(imageSize(value), [8, 6], mode);

    // The display grows to twice the container and is scaled to fit: the
    // pointer it already had is shown at two pixels per CSS pixel.
    await feed(page, ins("size", 0, 400, 400), ins("sync", 2));
    const scaled = await cursor(page, DENSE);
    assert.deepEqual(scaled.match(DENSE)?.slice(1), ["2", "2", "1"], `${mode}: after scaling, got ${scaled.slice(-40)}`);
    assert.deepEqual(imageSize(scaled), [8, 6], mode);
    await page.close();
  }

  // Two device pixels per CSS pixel, the display at device resolution.
  {
    const page = await open(mode, 2);
    await page.evaluate(() => window.session.set_scale_mode("native"));
    await feed(page, ...withPointer(400, 1));
    const value = await cursor(page, DENSE);
    assert.deepEqual(value.match(DENSE)?.slice(1), ["2", "2", "1"], `${mode}: dense cursor, got ${value.slice(-40)}`);
    assert.deepEqual(imageSize(value), [8, 6], mode);

    await page.close();
  }

  // A browser that takes no resolution in a cursor gets the image shrunk
  // to its size in CSS pixels.
  {
    const page = await open(mode, 2, { imageSet: false });
    await page.evaluate(() => window.session.set_scale_mode("native"));
    await feed(page, ...withPointer(400, 1));
    const value = await cursor(page, PLAIN);
    assert.deepEqual(value.match(PLAIN)?.slice(1), ["2", "1"], `${mode}: shrunk cursor, got ${value.slice(-40)}`);
    assert.deepEqual(imageSize(value), [4, 3], mode);
    await page.close();
  }
  console.log(`ok - ${mode} mode: the pointer has one image pixel per remote pixel`);
}

await browser.close();

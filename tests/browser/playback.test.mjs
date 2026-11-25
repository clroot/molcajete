// Playback of session recordings (web/public/molcajete-player.js) in
// headless Chromium: a recording with known frames is loaded into the
// player in worker and main-thread mode and the screen is checked after
// seeking forward, backward and to the end, and after playing.
//
// Seeking decodes PNG images in wasm (crate::png) while playing leaves them
// to the browser; an image with transparency drawn OVER must come out the
// same both ways.
//
//   node playback.test.mjs        (CHROME=/path/to/chrome to choose the browser)
import { chromium } from "playwright-core";
import { readFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";
import path from "node:path";
import zlib from "node:zlib";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const web = path.join(root, "web/public");

function pngChunk(type, data) {
  const body = Buffer.concat([Buffer.from(type, "latin1"), data]);
  const out = Buffer.alloc(body.length + 8);
  out.writeUInt32BE(data.length, 0);
  body.copy(out, 4);
  out.writeUInt32BE(zlib.crc32(body), body.length + 4);
  return out;
}

/** An RGBA PNG of one colour. */
function png(width, height, rgba) {
  const raw = Buffer.alloc(height * (1 + width * 4));
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) raw.set(rgba, y * (1 + width * 4) + 1 + x * 4);
  }
  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0);
  header.writeUInt32BE(height, 4);
  header.set([8, 6, 0, 0, 0], 8);
  return Buffer.concat([
    Buffer.from("\x89PNG\r\n\x1a\n", "latin1"),
    pngChunk("IHDR", header),
    pngChunk("IDAT", zlib.deflateSync(raw)),
    pngChunk("IEND", Buffer.alloc(0)),
  ]);
}

const ins = (...args) => args.map((a) => `${String(a).length}.${a}`).join(",") + ";";
const fill = (r, g, b) => ins("rect", 0, 0, 0, 100, 100) + ins("cfill", 12, 0, r, g, b, 255);
const image = (stream, mode, x, y, data) =>
  ins("img", stream, mode, 0, "image/png", x, y) + ins("blob", stream, data.toString("base64")) + ins("end", stream);

// Unix milliseconds, as guacd and the gateway write them.
const T = 1_795_600_000_000;
const recording = [
  ins("size", 0, 100, 100) + fill(255, 0, 0) + ins("sync", T),
  fill(0, 255, 0) + ins("sync", T + 1000),
  // The user's mouse, timed by the gateway.
  ins("mouse", 20, 20, 0, T + 1500),
  // An opaque blue square, and white at half opacity over the green.
  image(1, 14, 0, 0, png(10, 10, [0, 0, 255, 255])) + image(2, 14, 50, 50, png(10, 10, [255, 255, 255, 128])) + ins("sync", T + 2000),
  ins("sync", T + 3000),
  // How a session ends; the player keeps the screen.
  ins("error", "Aborted", 519) + ins("disconnect"),
].join("");

const PAGE = (worker) => `<!doctype html><body style="margin:0"><div id="d" style="width:100px;height:100px;position:relative"></div>
<script type="module">
import { MolcajeteSession } from "/molcajete.js";
const session = await MolcajeteSession.create(document.getElementById("d"), { playback: true, worker: ${worker} });
window.session = session;
window.waiters = [];
window.errors = [];
session.set_event_handler((type, detail) => {
  if (type === "error") window.errors.push(detail);
  if (type !== "playback") return;
  window.playerStatus = detail;
  for (const w of window.waiters.splice(0)) w(detail);
});
window.until = (states) => new Promise((resolve) => {
  const check = (d) => (states.includes(d.state) ? resolve(d) : window.waiters.push(check));
  window.waiters.push(check);
});
window.ready = true;
</script>`;

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

for (const worker of [true, false]) {
  const mode = worker ? "worker" : "main";
  const page = await browser.newPage({ viewport: { width: 100, height: 100 } });
  page.on("pageerror", (e) => console.error("[pageerror]", e.message));
  page.on("console", (m) => m.type() === "error" && console.error("[console]", m.text()));
  await page.route("http://guac.test/**", async (route) => {
    const p = new URL(route.request().url()).pathname;
    if (p === "/") return route.fulfill({ contentType: "text/html", body: PAGE(worker) });
    if (p === "/recording.guac") return route.fulfill({ contentType: "application/octet-stream", body: recording });
    const f = path.join(web, p);
    return route.fulfill({ contentType: f.endsWith(".wasm") ? "application/wasm" : "text/javascript", body: await readFile(f) });
  });
  await page.goto("http://guac.test/");
  await page.waitForFunction(() => window.ready === true);

  const run = (command, ...args) =>
    page.evaluate(
      async ([command, args]) => {
        const done = window.until(command === "load" ? ["ready", "error"] : command === "play" ? ["ended"] : ["paused", "ended"]);
        window.session.playback(command, ...args);
        return done;
      },
      [command, args],
    );
  const pixel = (x, y) => page.evaluate(async ([x, y]) => Array.from(await window.session.request("debug_canvas_pixel", x, y)), [x, y]);
  const near = (actual, expected, what) =>
    assert.ok(actual.every((v, i) => Math.abs(v - expected[i]) <= 2), `${mode}: ${what}: ${actual} ≠ ${expected}`);

  const loaded = await run("load", "/recording.guac");
  assert.equal(loaded.state, "ready", `${mode}: loads`);
  assert.equal(loaded.duration, 3000, `${mode}: duration from the first to the last time`);
  assert.equal(loaded.start, T, `${mode}: start`);
  near(await pixel(50, 50), [255, 0, 0, 255], "the first frame is shown once loaded");

  await run("seek", 2500);
  near(await pixel(5, 5), [0, 0, 255, 255], "seek forward: the opaque image");
  near(await pixel(80, 80), [0, 255, 0, 255], "seek forward: the fill");
  const blended = await pixel(55, 55);
  near(blended, [128, 255, 128, 255], "seek forward: half-transparent white over green");

  await run("seek", 500);
  near(await pixel(5, 5), [255, 0, 0, 255], "seek backward starts over");
  await run("seek", 1200);
  near(await pixel(5, 5), [0, 255, 0, 255], "seek forward again");

  // Played at 8×, the images go through the browser's decoder.
  await page.evaluate(() => window.session.playback("speed", 8));
  const ended = await run("play");
  assert.equal(ended.time, 3000, `${mode}: plays to the end`);
  near(await pixel(5, 5), [0, 0, 255, 255], "played: the opaque image");
  assert.deepEqual(await pixel(55, 55), blended, `${mode}: decoded in wasm and by the browser alike`);
  assert.deepEqual(await page.evaluate(() => window.errors), [], `${mode}: the recorded end is no error`);

  // Playing again from the end starts over.
  await run("seek", 3000);
  near(await pixel(55, 55), blended, "seek to the end");
  await page.close();
  console.log(`ok - playback (${mode})`);
}
await browser.close();

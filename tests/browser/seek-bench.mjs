// Seeking through a long session recording in the player of the web UI
// (web/public/molcajete-player.js, in the worker as on the playback page).
//
//   node seek-bench.mjs <recording.guac> [repeat] [rounds]
//
// The recording (Guacamole's format, as the gateway writes it) is repeated
// `repeat` times one after the other, its times shifted, to make a long one:
// a 15 s recording repeated 240 times is an hour. The bench loads it, seeks
// to the end, back to the start, to the middle and to the end again, and
// reports how long each took. Since every repetition ends like the original,
// the screen after seeking to the end must match the original's end; the
// pixels of both are compared. CHROME / CHROME_ARGS select the browser.
import { chromium } from "playwright-core";
import { readFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import http from "node:http";

const [file, repeat = "240", rounds = "1"] = process.argv.slice(2);
if (!file) {
  console.error("usage: node seek-bench.mjs <recording.guac> [repeat] [rounds]");
  process.exit(2);
}
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const web = path.join(root, "web/public");

/** Times of `sync`, the user's `mouse` (4 arguments) and `key` (3), shifted by `offset`. */
function shift(text, offset) {
  return text.replace(/(\d+)\.(sync|mouse|key),((?:\d+\.[^,;]*,)*?)(\d+)\.(\d+)([,;])/g, (whole, oplen, op, middle, len, time, end) => {
    const args = middle ? middle.split(",").length - 1 : 0;
    const timed = (op === "sync" && args === 0) || (op === "mouse" && args === 3) || (op === "key" && args === 2);
    if (!timed) return whole;
    const shifted = String(Number(time) + offset);
    return `${oplen}.${op},${middle}${shifted.length}.${shifted}${end}`;
  });
}

const original = await readFile(file, "latin1");
const times = [...original.matchAll(/4\.sync,\d+\.(\d+)/g)].map((m) => Number(m[1]));
const span = times[times.length - 1] - times[0] + 1000;
const parts = [];
for (let i = 0; i < Number(repeat); i++) parts.push(i === 0 ? original : shift(original, i * span));
const long = Buffer.from(parts.join(""), "latin1");
const short = Buffer.from(original, "latin1");
console.log(
  `recording: ${(short.length / 1e6).toFixed(1)} MB, ${(span / 1000).toFixed(1)} s; ` +
    `repeated ${repeat}×: ${(long.length / 1e6).toFixed(0)} MB, ${((span * Number(repeat)) / 60000).toFixed(1)} min`,
);

function findChrome() {
  if (process.env.CHROME) return process.env.CHROME;
  const cache = path.join(process.env.HOME ?? "", ".cache/ms-playwright");
  const candidate = path.join(cache, "chromium-1228/chrome-linux64/chrome");
  return existsSync(candidate) ? candidate : undefined;
}

const browser = await chromium.launch({
  executablePath: findChrome(),
  args: (process.env.CHROME_ARGS ?? "--use-angle=swiftshader --enable-unsafe-swiftshader --ignore-gpu-blocklist").split(" "),
});

// A real HTTP server: recordings of hundreds of megabytes are too much for
// Playwright's request interception. localhost is a secure context.
let recording = Buffer.alloc(0);
const server = http.createServer(async (request, response) => {
  const p = new URL(request.url, "http://localhost").pathname;
  if (p === "/") {
    response.writeHead(200, { "content-type": "text/html" });
    response.end(PAGE);
  } else if (p === "/recording.guac") {
    response.writeHead(200, { "content-type": "application/octet-stream", "content-length": recording.length });
    response.end(recording);
  } else {
    const f = path.join(web, path.normalize(p));
    if (!f.startsWith(web) || !existsSync(f)) {
      response.writeHead(404);
      response.end();
      return;
    }
    response.writeHead(200, { "content-type": f.endsWith(".wasm") ? "application/wasm" : "text/javascript" });
    response.end(await readFile(f));
  }
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const origin = `http://localhost:${server.address().port}/`;

const PAGE = `<!doctype html><body style="margin:0"><div id="d" style="width:1280px;height:800px;position:relative"></div>
<script type="module">
import { MolcajeteSession } from "/molcajete.js";
const session = await MolcajeteSession.create(document.getElementById("d"), { playback: true });
window.session = session;
window.waiters = [];
session.set_event_handler((type, detail) => {
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

async function open(data) {
  recording = data;
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  page.on("pageerror", (e) => console.error("[pageerror]", e.message));
  page.on("console", (m) => m.type() === "error" && console.error("[console]", m.text()));
  await page.goto(origin);
  await page.waitForFunction(() => window.ready === true);
  return page;
}

/** Runs `command` in the player and waits for it to finish; returns the milliseconds taken. */
async function timed(page, command, ...args) {
  return page.evaluate(
    async ([command, args]) => {
      const done = window.until(command === "load" ? ["ready", "error"] : ["paused", "ended"]);
      const start = performance.now();
      window.session.playback(command, ...args);
      const status = await done;
      if (status.state === "error") throw new Error(status.message);
      return performance.now() - start;
    },
    [command, args],
  );
}

/** A coarse fingerprint of the screen: the pixels of a 32×20 grid. */
async function screen(page) {
  await page.waitForTimeout(200);
  const pixels = [];
  for (let y = 10; y < 800; y += 40) {
    for (let x = 10; x < 1280; x += 40) pixels.push((await page.evaluate(([x, y]) => window.session.request("debug_canvas_pixel", x, y), [x, y])) ?? []);
  }
  return pixels.map((p) => p.join(",")).join(" ");
}

const reference = await open(short);
await timed(reference, "load", "/recording.guac");
const duration = await reference.evaluate(() => window.playerStatus.duration);
await timed(reference, "seek", duration);
const expected = await screen(reference);
await reference.close();

const results = [];
for (let round = 0; round < Number(rounds); round++) {
  const page = await open(long);
  const load = await timed(page, "load", "/recording.guac");
  const end = await page.evaluate(() => window.playerStatus.duration);
  const toEnd = await timed(page, "seek", end);
  const same = (await screen(page)) === expected;
  const toStart = await timed(page, "seek", 0);
  const toMiddle = await timed(page, "seek", end / 2);
  const toEndAgain = await timed(page, "seek", end);
  results.push({
    "load + index s": +(load / 1000).toFixed(2),
    "seek to end s": +(toEnd / 1000).toFixed(2),
    "end matches": same,
    "back to start s": +(toStart / 1000).toFixed(2),
    "to middle s": +(toMiddle / 1000).toFixed(2),
    "middle to end s": +(toEndAgain / 1000).toFixed(2),
  });
  await page.close();
}
console.table(results);
await browser.close();
server.close();
if (results.some((r) => !r["end matches"])) process.exit(1);

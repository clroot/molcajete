// How long a frame waits between being applied and being shown, in worker
// and in main-thread mode, with the canvas drawn in the next animation frame
// ("raf", what the client does) and drawn as soon as the frame is applied
// ("early", `debug_set_early_composite`).
//
//   scripts/build-web.sh && (cd tests/browser && npm install && node present-bench.mjs [seconds])
//
// Frames that each repaint the whole display in a colour that encodes their
// number are fed at several rates, with nothing else changing on the page:
//
//   single   one frame about every 110 ms, at a random point of the refresh
//            interval (typing)
//   29/s, 57/s  a steady stream slightly slower than half and all of a 60 Hz
//            refresh, so that frames arrive at every point of the interval
//   burst    about 220 frames per second, more than the display can show
//
// and timed in three ways:
//
//   client ms   the client's own figure (`framePresentMsAvg` / `Max` of its
//               `stats`): from the frame being applied to `composite` returning.
//   screen ms   from feeding the frame to the first frame of the browser's
//               compositor that shows it (CDP screencast, identified by its
//               colour). This is what the user waits for.
//   pushed ms   worker mode, single frames: from feeding the frame until the
//               page sees it in the canvas element, which is when the
//               worker's OffscreenCanvas handed it to the compositor.
//
// The run fails if the canvas is composited more often than animation frames
// run, or more than once for a single frame.
//
// Set CHROME=/path/to/chrome to choose the browser binary, CHROME_ARGS for
// its GPU backend and HEADED=1 to run in a window on the real display.
// Headless Chromium has no display: its refresh is a 60 Hz timer, and
// "screen" is when the compositor produced the frame, not when a panel lit
// up. Use a full Chrome build (new headless mode); the old headless shell
// starts a frame whenever one is asked for, so nothing ever waits in it.
import { chromium } from "playwright-core";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

const seconds = Number(process.argv[2] ?? 4);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const web = path.join(root, "web/public");
const [W, H] = [320, 240];
/** Frame numbers are written into the colour modulo this. */
const CODES = 512;

const html = `<!doctype html><body style="margin:0;background:#000">
<div id="d" style="width:${W}px;height:${H}px;position:relative"></div>
<script type="module">
import { MolcajeteSession } from "/molcajete.js";
const params = new URLSearchParams(location.search);
const session = await MolcajeteSession.create(document.getElementById("d"), { worker: params.get("mode") === "worker" });
window.stats = [];
session.set_event_handler((type, detail) => {
  if (type === "stats") window.stats.push({ at: performance.now(), ...detail });
});
await session.request("debug_set_early_composite", params.get("composite") === "early");
const epoch = () => performance.timeOrigin + performance.now();
const encode = (v) => { const s = String(v); return s.length + "." + s; };
const ins = (...args) => args.map(encode).join(",") + ";";
const color = (n) => [16 + 32 * (n & 7), 16 + 32 * ((n >> 3) & 7), 16 + 32 * ((n >> 6) & 7)];
const frame = (n) => ins("rect", 0, 0, 0, ${W}, ${H}) + ins("cfill", 14, 0, ...color(n), 255) + ins("sync", n + 1);
session.call("debug_feed", ins("size", 0, ${W}, ${H}) + frame(0));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let next = 1;
/** Feeds one frame; returns its number and when it was fed (ms since the epoch). */
const feed = () => {
  const n = next++;
  session.call("debug_feed", frame(n));
  return { n, at: epoch() };
};

/**
 * Feeds a frame every \`gap()\` ms for \`ms\`. Resolves with the frames, the
 * animation frames per second meanwhile and the client's stats of that time.
 */
window.run = async (ms, gap, countFrames) => {
  await sleep(1100);
  const from = performance.now();
  let rafs = 0, counting = countFrames;
  const count = () => { rafs++; if (counting) requestAnimationFrame(count); };
  if (counting) requestAnimationFrame(count);
  const fed = [];
  for (let due = from; due - from < ms; due += gap()) {
    const wait = due - performance.now();
    if (wait > 0) await sleep(wait);
    fed.push(feed());
  }
  counting = false;
  const rafPerSecond = (rafs * 1000) / (performance.now() - from);
  await sleep(1100);
  return { fed, rafPerSecond, from, stats: window.stats.filter((s) => s.at > from) };
};

// Worker mode: the canvas element shows what the worker last pushed.
const probe = new OffscreenCanvas(1, 1).getContext("2d", { willReadFrequently: true });
const turn = () => new Promise((r) => { const c = new MessageChannel(); c.port1.onmessage = r; c.port2.postMessage(0); });
/** Feeds one frame and resolves with how long it took to reach the canvas element. */
window.feedAndWatch = async () => {
  const { n, at } = feed();
  const want = color(n);
  for (;;) {
    probe.drawImage(document.querySelector("canvas.guac-display"), 0, 0, 1, 1, 0, 0, 1, 1);
    const got = probe.getImageData(0, 0, 1, 1).data;
    if (want.every((v, i) => Math.abs(v - got[i]) < 12)) return epoch() - at;
    if (epoch() - at > 1000) return NaN;
    await turn();
  }
};
window.ready = true;
</script>`;

// Reads the colour of screencast frames.
const decoderHtml = `<!doctype html><script>
window.decode = async (frames) => {
  const ctx = new OffscreenCanvas(1, 1).getContext("2d", { willReadFrequently: true });
  const out = [];
  for (const data of frames) {
    const bitmap = await createImageBitmap(await (await fetch("data:image/png;base64," + data)).blob());
    ctx.drawImage(bitmap, bitmap.width >> 1, bitmap.height >> 1, 1, 1, 0, 0, 1, 1);
    out.push(Array.from(ctx.getImageData(0, 0, 1, 1).data.slice(0, 3)));
    bitmap.close();
  }
  return out;
};
</script>`;

const browser = await chromium.launch({
  executablePath: process.env.CHROME,
  headless: !process.env.HEADED,
  args: (process.env.CHROME_ARGS ?? "--use-angle=swiftshader --enable-unsafe-swiftshader --ignore-gpu-blocklist").split(" "),
});
const decoder = await browser.newPage();
await decoder.setContent(decoderHtml);

function spread(values) {
  const v = values.filter(Number.isFinite).sort((a, b) => a - b);
  if (!v.length) return "-";
  const at = (q) => v[Math.min(v.length - 1, Math.floor(q * v.length))];
  const avg = v.reduce((a, b) => a + b, 0) / v.length;
  return `${avg.toFixed(1)} / ${at(0.5).toFixed(1)} / ${at(0.95).toFixed(1)} / ${v.at(-1).toFixed(1)}`;
}

/** Composites counted by the client's `stats` events of a run. */
function composites({ stats, from }) {
  let total = 0;
  let last = from;
  for (const s of stats) {
    total += s.compositesPerSecond * ((s.at - last) / 1000);
    last = s.at;
  }
  return total;
}

function clientWait({ stats }) {
  const drawn = stats.filter((s) => s.framePresentMsAvg != null);
  const frames = drawn.reduce((n, s) => n + s.framesPerSecond, 0);
  if (!frames) return "-";
  const avg = drawn.reduce((n, s) => n + s.framePresentMsAvg * s.framesPerSecond, 0) / frames;
  return `${avg.toFixed(2)} / ${Math.max(...drawn.map((s) => s.framePresentMsMax)).toFixed(2)}`;
}

async function measure(mode, composite) {
  const page = await browser.newPage({ viewport: { width: W, height: H } });
  page.on("pageerror", (e) => console.error(`[${mode}/${composite}] ${e.message}`));
  await page.route("http://localhost:9/**", async (route) => {
    const p = new URL(route.request().url()).pathname;
    if (p === "/") return route.fulfill({ contentType: "text/html", body: html });
    const file = path.join(web, p);
    return route.fulfill({ contentType: file.endsWith(".wasm") ? "application/wasm" : "text/javascript", body: await readFile(file) });
  });
  await page.goto(`http://localhost:9/?mode=${mode}&composite=${composite}`);
  await page.waitForFunction(() => window.ready === true);
  await page.bringToFront();

  const cdp = await page.context().newCDPSession(page);
  let casts = [];
  cdp.on("Page.screencastFrame", ({ data, metadata, sessionId }) => {
    casts.push({ at: metadata.timestamp * 1000, data });
    cdp.send("Page.screencastFrameAck", { sessionId }).catch(() => {});
  });
  await cdp.send("Page.startScreencast", { format: "png", everyNthFrame: 1 });

  /** Feeds frames and times each one against the screencast. */
  async function run(ms, gap, countFrames = false) {
    casts = [];
    const result = await page.evaluate(`window.run(${ms}, ${gap}, ${countFrames})`);
    const shown = casts;
    const colors = await decoder.evaluate((frames) => window.decode(frames), shown.map((c) => c.data));
    // Frame numbers only grow, so the colour's number can be unwrapped.
    let last = result.fed[0].n - 1;
    const numbered = shown.map((c, i) => {
      const [r, g, b] = colors[i].map((v) => Math.round((v - 16) / 32) & 7);
      const ahead = (((r | (g << 3) | (b << 6)) - last) % CODES + CODES) % CODES;
      // Anything else on screen (the page before its first frame) is not
      // one of the colours, or reads as a frame far ahead.
      const off = colors[i].map((v) => (((v - 16) % 32) + 32) % 32);
      if (ahead > CODES / 2 || off.some((v) => v > 6 && v < 26)) return null;
      last += ahead;
      return { n: last, at: c.at };
    }).filter(Boolean);
    result.screen = result.fed.map((f) => (numbered.find((c) => c.n >= f.n)?.at ?? NaN) - f.at);
    return result;
  }

  const rows = [];
  const problems = [];
  const row = (load, r, extra = {}) =>
    rows.push({
      mode,
      composite,
      load,
      "client ms avg / max": clientWait(r),
      "screen ms avg / p50 / p95 / max": spread(r.screen),
      "composites/s": +(composites(r) / ((r.fed.at(-1).at - r.fed[0].at) / 1000 || 1)).toFixed(1),
      ...extra,
    });

  const single = await run(seconds * 1000, "() => 100 + Math.random() * 20");
  const pushed = [];
  if (mode === "worker") {
    for (let i = 0; i < single.fed.length; i++) {
      await page.waitForTimeout(100 + Math.random() * 20);
      pushed.push(await page.evaluate(() => window.feedAndWatch()));
    }
  }
  row("single", single, { "pushed ms avg / p50 / p95 / max": spread(pushed) });
  const drawn = composites(single);
  if (drawn > single.fed.length * 1.03 + 1) {
    problems.push(`${drawn.toFixed(0)} composites for ${single.fed.length} single frames`);
  }

  row("29/s", await run(seconds * 1000, "() => 1000 / 29"));
  row("57/s", await run(seconds * 1000, "() => 1000 / 57"));

  const burst = await run(seconds * 1000, "() => 2", true);
  const peak = Math.max(...burst.stats.map((s) => s.compositesPerSecond));
  row("burst", burst, { "raf/s": +burst.rafPerSecond.toFixed(1), "peak composites/s": +peak.toFixed(1) });
  // One second of stats may hold an animation frame more than the average.
  if (peak > burst.rafPerSecond * 1.05 + 1) {
    problems.push(`${peak.toFixed(1)} composites/s with ${burst.rafPerSecond.toFixed(1)} animation frames/s`);
  }
  await cdp.send("Page.stopScreencast");
  await page.close();
  return { rows, problems };
}

const rows = [];
let failed = false;
for (const mode of (process.env.MODES ?? "worker,main").split(",")) {
  for (const composite of ["raf", "early"]) {
    const result = await measure(mode, composite);
    rows.push(...result.rows);
    for (const problem of result.problems) {
      failed = true;
      console.error(`FAIL ${mode}/${composite}: ${problem}`);
    }
  }
}
await browser.close();
console.table(rows);
process.exit(failed ? 1 : 0);

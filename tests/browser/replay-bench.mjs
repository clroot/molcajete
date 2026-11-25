// Deterministic client benchmark: replays a recorded Guacamole stream into
// the stock guacamole-common-js client and into the wasm/WebGL2 client with
// the original timing, and compares main-thread cost.
//
//   GUAC_COMMON_JS=/path/to/guacamole-common-js/src/main/webapp/modules \
//   node replay-bench.mjs recording.json [rounds]
//
// A recording is a JSON array of {t: ms since start, data: "<instructions>"}
// (one entry per WebSocket message). Unlike a live session, both clients see
// exactly the same instructions at the same times, independent of server-side
// flow control. CHROME / CHROME_ARGS select the browser and GPU backend.
import { chromium } from "playwright-core";
import { readFile, readdir } from "node:fs/promises";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const [recordingPath, rounds = "2"] = process.argv.slice(2);
const modules = process.env.GUAC_COMMON_JS;
if (!recordingPath || !modules || !existsSync(modules)) {
  console.error("usage: GUAC_COMMON_JS=<guacamole-common-js modules dir> node replay-bench.mjs <recording.json> [rounds]");
  process.exit(2);
}
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
// Cut the recording after its last complete frame (`sync`), so that neither
// client waits forever for an image stream that was still in flight.
const recording = (() => {
  return readFile(recordingPath, "utf8").then((text) => {
    const messages = JSON.parse(text);
    for (let i = messages.length - 1; i >= 0; i--) {
      const at = messages[i].data.search(/4\.sync,\d+\.\d+(,\d+\.\d+)?;(?![\s\S]*4\.sync,)/);
      if (at >= 0) {
        const end = messages[i].data.indexOf(";", at) + 1;
        return JSON.stringify([...messages.slice(0, i), { ...messages[i], data: messages[i].data.slice(0, end) }]);
      }
    }
    return text;
  });
})();
const files = (await readdir(modules)).filter((f) => f.endsWith(".js")).sort();
const stockJs = (
  await Promise.all(["Namespace.js", ...files.filter((f) => f !== "Namespace.js")].map((f) => readFile(path.join(modules, f), "utf8")))
).join("\n");

// Shared replay driver: feeds each message at its recorded time, then waits
// until the client has drawn everything. Exposes window.done/window.frames.
const driver = `
window.frames_ = 0;
async function replay(feed, idle) {
  const messages = await (await fetch("/recording.json")).json();
  const start = performance.now();
  for (const m of messages) {
    const wait = start + m.t - performance.now();
    if (wait > 0) await new Promise((r) => setTimeout(r, wait));
    feed(m.data);
  }
  while (!(await idle())) await new Promise((r) => setTimeout(r, 5));
  await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
  window.elapsed = performance.now() - start;
  window.done = true;
}`;

const pages = {
  worker: `<!doctype html><body style="margin:0;background:#000"><div id="d" style="width:1280px;height:800px;position:relative"></div>
<script type="module">${driver}
import { MolcajeteSession } from "/molcajete.js";
const session = await MolcajeteSession.create(document.getElementById("d"), { worker: true });
window.session = session;
replay((data) => session.call("debug_feed", data), async () => (await session.request("debug_queued")) === 0);
</script>`,
  stock: `<!doctype html><body style="margin:0;background:#000"><div id="d"></div>
<script src="/guacamole-common.js"></script><script>${driver}
const tunnel = new Guacamole.Tunnel();
tunnel.connect = () => {};
tunnel.sendMessage = () => {};
const client = new Guacamole.Client(tunnel);
document.getElementById("d").appendChild(client.getDisplay().getElement());
client.connect("");
const parser = new Guacamole.Parser();
parser.oninstruction = (opcode, args) => { if (opcode) tunnel.oninstruction(opcode, args); };
let flushed = true;
client.onsync = () => { window.frames_++; };
replay((data) => parser.receive(data), () => {
  // Display.flush() calls back once every queued frame has been drawn.
  if (flushed) { flushed = false; client.getDisplay().flush(() => { window.idle_ = true; }); }
  return window.idle_ === true;
});
</script>`,
  wasm: (staged) => `<!doctype html><body style="margin:0;background:#000"><div id="d" style="width:1280px;height:800px"></div>
<script type="module">${driver}
import init, { MolcajeteClient } from "/pkg/molcajete_client.js";
await init();
const client = new MolcajeteClient(document.getElementById("d"));
window.client = client;
client.debug_set_staged_uploads(${staged});
client.set_event_handler((type, detail) => { if (type === "stats") window.frames_ += Math.round(detail.framesPerSecond); });
replay((data) => client.debug_feed(data), () => client.debug_queued() === 0);
</script>`,
};

async function run(kind) {
  const cache = path.join(process.env.HOME ?? "", ".cache/ms-playwright/chromium-1228/chrome-linux64/chrome");
  const browser = await chromium.launch({
    executablePath: process.env.CHROME ?? (existsSync(cache) ? cache : undefined),
    args: (process.env.CHROME_ARGS ?? "--use-angle=swiftshader --enable-unsafe-swiftshader --ignore-gpu-blocklist").split(" "),
  });
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  page.on("pageerror", (e) => console.error(`[${kind}] ${e.message}`));
  await page.route("http://localhost:9/**", async (route) => {
    const p = new URL(route.request().url()).pathname;
    if (p === "/") {
      const body = kind === "stock" ? pages.stock : kind === "wasm-worker" ? pages.worker : pages.wasm(kind === "wasm-staged");
      return route.fulfill({ contentType: "text/html", body });
    }
    if (p === "/recording.json") return route.fulfill({ contentType: "application/json", body: await recording });
    if (p === "/guacamole-common.js") return route.fulfill({ contentType: "text/javascript", body: stockJs });
    const file = path.join(root, "web", p);
    const type = file.endsWith(".wasm") ? "application/wasm" : "text/javascript";
    return route.fulfill({ contentType: type, body: await readFile(file) });
  });
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("Performance.enable");
  const metrics = async () => Object.fromEntries((await cdp.send("Performance.getMetrics")).metrics.map((m) => [m.name, m.value]));
  const m0 = await metrics();
  if (process.env.PROFILE) {
    await cdp.send("Profiler.enable");
    await cdp.send("Profiler.setSamplingInterval", { interval: 200 });
    await cdp.send("Profiler.start");
  }
  await page.goto("http://localhost:9/"); // localhost is a secure context (WebCodecs)
  await page.waitForFunction(() => window.done === true, null, { timeout: 120_000, polling: 100 });
  const m1 = await metrics();
  if (process.env.PROFILE) {
    const { profile } = await cdp.send("Profiler.stop");
    const byId = new Map(profile.nodes.map((n) => [n.id, n]));
    const self = new Map();
    profile.samples.forEach((id, i) => {
      const cf = byId.get(id).callFrame;
      const key = `${cf.functionName || "(anonymous)"} ${cf.url.split("/").pop()}`;
      self.set(key, (self.get(key) ?? 0) + (profile.timeDeltas[i] ?? 0) / 1000);
    });
    console.log(`-- ${kind} top self time (ms)`);
    for (const [k, v] of [...self].sort((a, b) => b[1] - a[1]).slice(0, 10)) console.log(`  ${v.toFixed(0).padStart(7)}  ${k}`);
  }
  const elapsed = await page.evaluate(() => window.elapsed);
  if (process.env.SCREENSHOTS) await page.screenshot({ path: path.join(process.env.SCREENSHOTS, `${kind}.png`) });
  await browser.close();
  const task = (m1.TaskDuration - m0.TaskDuration) * 1000;
  const script = (m1.ScriptDuration - m0.ScriptDuration) * 1000;
  return {
    client: kind,
    "replay s": +(elapsed / 1000).toFixed(2),
    "main thread ms": +task.toFixed(0),
    "script ms": +script.toFixed(0),
    "main thread %": +((task / elapsed) * 100).toFixed(1),
  };
}

const results = [];
for (let i = 0; i < Number(rounds); i++) {
  for (const kind of (process.env.CLIENTS ?? "stock,wasm-staged,wasm-worker").split(",")) {
    results.push(await run(kind));
  }
}
console.table(results);

// Audio of the page-side client (web/public/molcajete.js): streams play in
// one AudioContext on the page in worker mode and in main-thread mode, a
// context handed to `create` during a click is used, and the page is told
// while sound waits for a gesture (`audio` events).
//
// Headless Chromium lets every AudioContext run, so the page gets the
// autoplay policy of a regular browser window: a context made outside a user
// gesture starts suspended, and resumes only within one. A stand-in tunnel
// opens a PCM stream and sends one second of sound.
//
//   SKIP_UI=1 scripts/build-web.sh && (cd tests/browser && npm ci && node audio.test.mjs)
//
// Set CHROME=/path/to/chrome to choose the browser binary.
import { chromium } from "playwright-core";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const web = path.join(root, "web/public");

const enc = (v) => `${[...String(v)].length}.${v}`;
const ins = (op, ...args) => [op, ...args].map(enc).join(",") + ";";
// One second of a quiet 440 Hz tone, 16-bit stereo at 44.1 kHz.
const pcm = Buffer.alloc(44100 * 4);
for (let i = 0; i < 44100; i++) {
  const v = Math.round(Math.sin((2 * Math.PI * 440 * i) / 44100) * 1000);
  pcm.writeInt16LE(v, i * 4);
  pcm.writeInt16LE(v, i * 4 + 2);
}
const messages = [
  ins("", "4f0c1b9e-0000-4000-8000-000000000000"),
  ins("audio", 1, "audio/L16;rate=44100,channels=2"),
  // Blobs of at most 6048 bytes, as a daemon sends them.
  ...Array.from({ length: Math.ceil(pcm.length / 4536) }, (_, i) =>
    ins("blob", 1, pcm.subarray(i * 4536, (i + 1) * 4536).toString("base64")),
  ),
  ins("sync", 1),
];

/**
 * Web Audio as a browser with an autoplay policy allows it: running only when
 * made or resumed while a trusted press, click, touch or key event is being
 * handled. (\`navigator.userActivation\` cannot tell: Playwright's
 * \`evaluate\` counts as a gesture.)
 */
const autoplayPolicy = `{
  let inGesture = false;
  for (const type of ["pointerdown", "pointerup", "touchend", "click", "keydown"]) {
    window.addEventListener(type, (e) => {
      if (!e.isTrusted) return;
      inGesture = true;
      setTimeout(() => (inGesture = false));
    }, { capture: true });
  }
  const Native = window.AudioContext;
  window.AudioContext = class extends Native {
    constructor(...args) {
      super(...args);
      if (!inGesture) super.suspend();
    }
    resume() {
      return inGesture ? super.resume() : Promise.resolve();
    }
  };
}`;

/** A WebSocket that opens and then sends `messages` (text protocol). */
const standIn = (messagesJson) => `self.WebSocket = class {
  static OPEN = 1;
  readyState = 0;
  bufferedAmount = 0;
  protocol = "guacamole";
  constructor() {
    setTimeout(() => {
      this.readyState = 1;
      this.onopen?.(new Event("open"));
      for (const data of ${messagesJson}) this.onmessage?.(new MessageEvent("message", { data }));
    }, 0);
  }
  send() {}
  close() {}
};
`;

const browser = await chromium.launch({
  executablePath: process.env.CHROME,
  args: ["--use-angle=swiftshader", "--enable-unsafe-swiftshader"],
});

/**
 * Opens a session page in `mode`. With `clickFirst`, the page's
 * AudioContext is made in a click before the client exists and handed to
 * `create`, as the connection list does.
 */
async function open(mode, clickFirst) {
  const page = await browser.newPage({ viewport: { width: 400, height: 300 } });
  page.on("pageerror", (e) => console.error("[pageerror]", e.message));
  const json = JSON.stringify(messages);
  await page.addInitScript(standIn(json));
  await page.addInitScript(autoplayPolicy);
  await page.route("http://guac.test/**", async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === "/") {
      return route.fulfill({
        contentType: "text/html",
        body: `<!doctype html><button id="open" style="position:fixed;right:0;bottom:0">open</button>
<div id="c" style="width:200px;height:200px;position:relative"></div>
<script type="module">
import { MolcajeteSession } from "./molcajete.js";
window.events = [];
window.start = async (audioContext) => {
  window.context = audioContext;
  window.session = await MolcajeteSession.create(document.getElementById("c"), { worker: ${mode === "worker"}, audioContext });
  window.session.set_event_handler((type, detail) => window.events.push([type, detail]));
  window.session.connect("ws://guac.test/tunnel");
};
document.getElementById("open").onclick = () => {
  window.clicked = new AudioContext();
  window.buffers = 0;
  const createBuffer = window.clicked.createBuffer.bind(window.clicked);
  window.clicked.createBuffer = (...args) => (window.buffers++, createBuffer(...args));
};
window.ready = true;
</script>`,
      });
    }
    if (url.pathname === "/molcajete-worker.js") {
      const source = await readFile(path.join(web, url.pathname), "utf8");
      return route.fulfill({ contentType: "text/javascript", body: standIn(json) + source });
    }
    const file = path.join(web, url.pathname);
    const type = file.endsWith(".wasm") ? "application/wasm" : "text/javascript";
    return route.fulfill({ contentType: type, body: await readFile(file) });
  });
  await page.goto("http://guac.test/");
  await page.waitForFunction(() => window.ready === true);
  if (clickFirst) {
    await page.click("#open");
    await page.evaluate(() => window.start(window.clicked));
  } else {
    await page.evaluate(() => window.start(undefined));
  }
  return page;
}

const audioEvents = (page) => page.evaluate(() => window.events.filter(([t]) => t === "audio").map(([, d]) => d.blocked));
const audioTypes = (page) =>
  page.evaluate(() => window.events.map(([t]) => t).filter((t) => t.startsWith("audio-")));

for (const mode of ["worker", "main"]) {
  // No gesture before the stream: the page hears that sound is held back,
  // and the next click anywhere on the page lets it play.
  const page = await open(mode, false);
  await page.waitForFunction(() => window.events.some(([t]) => t === "audio"));
  assert.deepEqual(await audioEvents(page), [true], `${mode}: sound waits for a gesture`);
  assert.deepEqual(await audioTypes(page), [], `${mode}: stream events stay inside the client`);
  await page.mouse.click(390, 290);
  await page.waitForFunction(() => window.events.filter(([t]) => t === "audio").length === 2);
  assert.deepEqual(await audioEvents(page), [true, false], `${mode}: a click turns sound on`);
  await page.close();

  // The context made by the click that opened the session runs from the
  // start: the stream plays at once and nothing is reported.
  const clicked = await open(mode, true);
  await clicked.waitForTimeout(1000);
  assert.equal(await clicked.evaluate(() => window.context.state), "running", `${mode}: the click's context runs`);
  assert.ok(await clicked.evaluate(() => window.buffers > 0), `${mode}: the stream plays in the context it was given`);
  assert.deepEqual(await audioEvents(clicked), [], `${mode}: no hint when sound plays`);
  await clicked.close();
  console.log(`${mode}: ok`);
}

await browser.close();

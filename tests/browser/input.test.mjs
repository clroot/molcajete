// Keyboard and IME handling of the page-side client (web/public/molcajete.js),
// and the keys and held modifiers a page sends for an on-screen keyboard, in
// worker mode and in main-thread mode.
//
// Headless Chromium has no system IME, so the events a Korean IME produces are
// driven over CDP: `keydown` with key "Process" plus composition updates when
// the IME composes, or the bare jamo as `key` when the field takes no IME.
// The test records what reaches the tunnel (main-thread mode) or the worker.
//
//   scripts/build-web.sh && (cd tests/browser && npm install && node input.test.mjs)
//
// Set CHROME=/path/to/chrome to choose the browser binary.
import { chromium } from "playwright-core";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const web = path.join(root, "web/public");

const HANGUL = 0xff31;
const CAPS_LOCK = 0xffe5;
const CTRL = 0xffe3;
const ALT = 0xffe9;
const TAB = 0xff09;
const tap = (keysym) => [`${keysym} down`, `${keysym} up`];
const typed = (text) => [...text].flatMap((c) => tap(c.codePointAt(0)));
const hangul = (c) => 0x01000000 | c.codePointAt(0);

const browser = await chromium.launch({
  executablePath: process.env.CHROME,
  args: ["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"],
});

/** Opens a session with a stand-in tunnel and returns helpers to type into it. */
async function open(mode) {
  const page = await browser.newPage({ viewport: { width: 400, height: 300 } });
  page.on("pageerror", (e) => console.error("[pageerror]", e.message));
  await page.addInitScript(() => {
    window.sent = [];
    const keyIns = /3\.key,\d+\.(\d+),1\.([01]);/g;
    // Main-thread mode: `key` instructions on the tunnel.
    window.WebSocket = class {
      static OPEN = 1;
      readyState = 0;
      bufferedAmount = 0;
      constructor() {
        setTimeout(() => {
          this.readyState = 1;
          this.onopen?.(new Event("open"));
        }, 0);
      }
      send(data) {
        for (const [, keysym, pressed] of data.matchAll(keyIns))
          window.sent.push(`${keysym} ${pressed === "1" ? "down" : "up"}`);
      }
      close() {}
    };
  });
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
    // The worker has no stand-in WebSocket: it reports the keys it would send.
    if (url.pathname === "/molcajete-worker.js") {
      const source = await readFile(path.join(web, url.pathname), "utf8");
      return route.fulfill({
        contentType: "text/javascript",
        body: `self.WebSocket = class {
  static OPEN = 1;
  readyState = 0;
  bufferedAmount = 0;
  constructor() { setTimeout(() => { this.readyState = 1; this.onopen?.(new Event("open")); }, 0); }
  send(data) {
    for (const [, keysym, pressed] of data.matchAll(/3\\.key,\\d+\\.(\\d+),1\\.([01]);/g))
      self.postMessage({ type: "event", event: "test-key", detail: keysym + (pressed === "1" ? " down" : " up") });
  }
  close() {}
};
${source}`,
      });
    }
    const file = path.join(web, url.pathname);
    const type = file.endsWith(".wasm") ? "application/wasm" : "text/javascript";
    return route.fulfill({ contentType: type, body: await readFile(file) });
  });
  await page.goto("http://guac.test/");
  await page.waitForFunction(() => window.ready === true);
  await page.evaluate(async () => {
    window.session.set_event_handler((type, detail) => {
      if (type === "test-key") window.sent.push(detail);
      if (type === "modifiers") window.modifiers = detail;
    });
    window.session.connect("ws://guac.test/tunnel");
    await new Promise((r) => setTimeout(r, 300));
    window.session.focus();
  });
  const cdp = await page.context().newCDPSession(page);

  /** Runs `actions` and returns the key events sent meanwhile. */
  const sent = async (actions) => {
    await page.evaluate(() => (window.sent = []));
    await actions();
    await page.waitForTimeout(100);
    return page.evaluate(() => window.sent);
  };
  const key = (type, key, code, modifiers = 0) =>
    cdp.send("Input.dispatchKeyEvent", { type, key, code, modifiers, windowsVirtualKeyCode: 229 });
  /** A key press an active IME takes: the composition becomes `composing`. */
  const compose = async (code, composing, commit) => {
    await key("rawKeyDown", "Process", code);
    if (commit) await cdp.send("Input.insertText", { text: commit });
    await cdp.send("Input.imeSetComposition", { text: composing, selectionStart: 1, selectionEnd: 1 });
    await key("keyUp", "Process", code);
  };
  /** A full press of a key that no IME takes. */
  const press = async (keyValue, code, modifiers = 0) => {
    await key("rawKeyDown", keyValue, code, modifiers);
    await key("keyUp", keyValue, code, modifiers);
  };
  return { page, cdp, sent, key, compose, press };
}

for (const mode of ["worker", "main"]) {
  const { page, cdp, sent, key, compose, press } = await open(mode);
  const remote = (enabled) => page.evaluate((e) => window.session.set_remote_composition(e), enabled);

  // Local composition (the default): nothing until a syllable is finished.
  assert.deepEqual(await sent(() => compose("KeyG", "ㅎ")), [], `${mode}: ㅎ is composed locally`);
  assert.deepEqual(await sent(() => compose("KeyK", "하")), []);
  assert.deepEqual(await sent(() => compose("KeyS", "한")), []);
  assert.deepEqual(await sent(() => compose("KeyR", "ㄱ", "한")), tap(hangul("한")), `${mode}: 한 once finished`);
  assert.deepEqual(await sent(() => cdp.send("Input.insertText", { text: "ㄱ" })), tap(hangul("ㄱ")));
  assert.deepEqual(await sent(() => press("a", "KeyA")), typed("a"));

  // Remote composition: every key goes out as it is pressed.
  await remote(true);
  assert.equal(await page.evaluate(() => document.querySelector("textarea").readOnly), true);
  // Whether the browser reports the jamo (no IME on the field) ...
  assert.deepEqual(await sent(() => press("ㅎ", "KeyG")), typed("g"), `${mode}: ㅎ is sent as the G key`);
  assert.deepEqual(await sent(() => press("ㅏ", "KeyK")), typed("k"));
  assert.deepEqual(await sent(() => press("ㄴ", "KeyS")), typed("s"));
  // ... or an IME took the key anyway.
  assert.deepEqual(
    await sent(async () => {
      await key("rawKeyDown", "Process", "KeyR");
      await key("keyUp", "Process", "KeyR");
    }),
    typed("r"),
    `${mode}: a key taken by the IME is sent by position`,
  );
  // Shift picks the double consonants on the remote side.
  assert.deepEqual(
    await sent(async () => {
      await key("rawKeyDown", "Shift", "ShiftLeft", 8);
      await press("ㅃ", "KeyQ", 8);
      await key("keyUp", "Shift", "ShiftLeft");
    }),
    ["65505 down", ...typed("Q"), "65505 up"],
  );
  assert.deepEqual(await sent(() => press("a", "KeyA")), typed("a"));
  assert.deepEqual(await sent(() => press("HangulMode", "Lang1")), tap(HANGUL));
  // Caps Lock is the Hangul/English key on a Mac only.
  const isMac = await page.evaluate(() => /mac/i.test(navigator.userAgentData?.platform ?? navigator.platform));
  assert.deepEqual(await sent(() => press("CapsLock", "CapsLock")), tap(isMac ? HANGUL : CAPS_LOCK));

  // Back to local composition.
  await remote(false);
  assert.equal(await page.evaluate(() => document.querySelector("textarea").readOnly), false);
  assert.deepEqual(await sent(() => compose("KeyG", "ㅎ")), []);
  assert.deepEqual(await sent(() => cdp.send("Input.insertText", { text: "ㅎ" })), tap(hangul("ㅎ")));

  // Modifiers held from the page (the terminal key bar on a phone): for the
  // next key, whether it is typed or comes from a button, or until turned off.
  const hold = (name, how) => page.evaluate(([n, h]) => window.session.hold_modifier(n, h), [name, how]);
  const pressKey = (keysym) => page.evaluate((k) => window.session.press_key(k), keysym);
  const held = () => page.evaluate(() => window.modifiers);
  assert.deepEqual(await sent(() => hold("ctrl", "once")), [`${CTRL} down`], `${mode}: Ctrl is held`);
  assert.deepEqual(await held(), { ctrl: "once", alt: "off" });
  assert.deepEqual(
    await sent(() => press("c", "KeyC")),
    ["99 down", `${CTRL} up`, "99 up"],
    `${mode}: Ctrl is let go after the key it was held for`,
  );
  assert.deepEqual(await held(), { ctrl: "off", alt: "off" });
  // Text from an on-screen keyboard: the modifier goes with its first character.
  await hold("ctrl", "once");
  assert.deepEqual(
    await sent(() => cdp.send("Input.insertText", { text: "ab" })),
    [...typed("a"), `${CTRL} up`, ...typed("b")],
    `${mode}: Ctrl applies to the first character of typed text`,
  );
  // A key of the bar takes the held modifier with it as well.
  await hold("ctrl", "once");
  assert.deepEqual(await sent(() => pressKey(TAB)), [...tap(TAB), `${CTRL} up`]);
  // A locked modifier stays down until it is turned off.
  assert.deepEqual(await sent(() => hold("alt", "once")), [`${ALT} down`]);
  assert.deepEqual(await sent(() => hold("alt", "locked")), [], `${mode}: locking sends nothing more`);
  assert.deepEqual(await sent(() => pressKey(TAB)), tap(TAB));
  assert.deepEqual(await sent(() => press("b", "KeyB")), typed("b"));
  assert.deepEqual(await held(), { ctrl: "off", alt: "locked" });
  assert.deepEqual(await sent(() => hold("alt", "off")), [`${ALT} up`]);
  assert.deepEqual(await held(), { ctrl: "off", alt: "off" });
  assert.deepEqual(await sent(() => hold("shift", "once")), [], `${mode}: only Ctrl and Alt can be held`);

  console.log(`ok - ${mode} mode`);
  await page.close();
}
await browser.close();

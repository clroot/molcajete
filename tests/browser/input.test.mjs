// Keyboard and IME handling of the page-side client (web/public/molcajete.js),
// the keys and held modifiers a page sends for an on-screen keyboard, and
// touch in terminals, in worker mode and in main-thread mode.
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
// Bits of the button mask of a `mouse` instruction.
const LEFT = 1;
const WHEEL_DOWN = 16;
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
    window.mice = [];
    window.zooms = [];
    const keyIns = /3\.key,\d+\.(\d+),1\.([01]);/g;
    const mouseIns = /5\.mouse,\d+\.\d+,\d+\.\d+,\d+\.(\d+);/g;
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
        // The button mask of each `mouse` instruction.
        for (const [, mask] of data.matchAll(mouseIns)) window.mice.push(Number(mask));
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
    for (const [, mask] of data.matchAll(/5\\.mouse,\\d+\\.\\d+,\\d+\\.\\d+,\\d+\\.(\\d+);/g))
      self.postMessage({ type: "event", event: "test-mouse", detail: Number(mask) });
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
      if (type === "test-mouse") window.mice.push(detail);
      if (type === "zoom") window.zooms.push(detail.delta);
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

  // Ctrl follows the flags of the key events (CDP's modifier 2 is Ctrl). A
  // stray `keyup` while Ctrl is held, as iPadOS Safari sends now and then:
  // the next key still reports Ctrl, so Ctrl goes down again before it.
  assert.deepEqual(
    await sent(async () => {
      await key("rawKeyDown", "Control", "ControlLeft", 2);
      await press("d", "KeyD", 2);
      await key("keyUp", "Control", "ControlLeft");
      await press("d", "KeyD", 2);
      await key("keyUp", "Control", "ControlLeft");
    }),
    [`${CTRL} down`, ...typed("d"), `${CTRL} up`, `${CTRL} down`, ...typed("d"), `${CTRL} up`],
    `${mode}: Ctrl is pressed again for a key that reports it`,
  );
  // A lost `keyup`: the next key no longer reports Ctrl, so Ctrl goes up first.
  assert.deepEqual(
    await sent(async () => {
      await key("rawKeyDown", "Control", "ControlLeft", 2);
      await press("a", "KeyA");
    }),
    [`${CTRL} down`, `${CTRL} up`, ...typed("a")],
    `${mode}: Ctrl is let go for a key that does not report it`,
  );

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

  // Touch in terminals: a drag scrolls, a finger that rests first selects, a
  // tap clicks and takes the keyboard, two fingers change the text size.
  const terminal = (enabled) => page.evaluate((e) => window.session.set_terminal_shortcuts(e), enabled);
  const touch = (type, ...points) =>
    cdp.send("Input.dispatchTouchEvent", { type, touchPoints: points.map(([x, y], id) => ({ x, y, id })) });
  /** Runs `actions` and returns the button masks sent meanwhile. */
  const mice = async (actions) => {
    await page.evaluate(() => (window.mice = []));
    await actions();
    await page.waitForTimeout(100);
    return page.evaluate(() => window.mice);
  };
  const drag = async () => {
    await touch("touchStart", [100, 150]);
    for (const y of [120, 90, 60, 30]) await touch("touchMove", [100, y]);
    await touch("touchEnd");
  };
  // A display for the fingers to land on: the canvas has no size without one.
  await page.evaluate(() => window.session.call("debug_feed", "4.size,1.0,3.200,3.200;4.sync,1.1;"));
  await page.waitForFunction(() => document.querySelector("canvas").getBoundingClientRect().height === 200);
  await terminal(true);
  assert.deepEqual(
    await mice(drag),
    // The pointer goes to where the drag began, then the wheel turns there.
    [0, WHEEL_DOWN, 0, WHEEL_DOWN, 0],
    `${mode}: a drag of 120 px turns the wheel twice and holds no button`,
  );
  assert.deepEqual(
    await mice(async () => {
      await touch("touchStart", [100, 100]);
      await page.waitForTimeout(650);
      await touch("touchMove", [140, 100]);
      await touch("touchEnd");
    }),
    [LEFT, LEFT, 0],
    `${mode}: a finger that rests holds the button until it is lifted`,
  );
  await page.evaluate(() => document.activeElement.blur());
  assert.deepEqual(
    await mice(async () => {
      await touch("touchStart", [100, 100]);
      await touch("touchEnd");
    }),
    [LEFT, 0],
    `${mode}: a tap clicks`,
  );
  assert.equal(await page.evaluate(() => document.activeElement.tagName), "TEXTAREA", `${mode}: a tap takes the keyboard`);
  assert.deepEqual(
    await mice(async () => {
      await touch("touchStart", [100, 100]);
      await touch("touchStart", [100, 100], [100, 140]);
      // Each step is a change of the fingers' distance by 15%.
      await touch("touchMove", [100, 100], [100, 150]);
      await touch("touchMove", [100, 100], [100, 140]);
      await touch("touchEnd");
    }),
    [],
    `${mode}: two fingers press nothing`,
  );
  assert.deepEqual(await page.evaluate(() => window.zooms), [1, -1], `${mode}: a pinch out and in`);
  // Anywhere else a finger stays the mouse it was: the button is down while it drags.
  await terminal(false);
  assert.deepEqual(
    await mice(drag),
    [LEFT, LEFT, LEFT, LEFT, LEFT, 0],
    `${mode}: outside terminals a drag is a drag`,
  );

  console.log(`ok - ${mode} mode`);
  await page.close();
}
await browser.close();

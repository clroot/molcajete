// How the wasm client reports the end of a tunnel, which the page uses to
// reconnect by itself and get a held session back (GH-145), in worker mode
// and in main-thread mode.
//
// A stand-in gateway scripts its tunnels in order:
//
// 1. draws a red frame, then drops the connection without a close frame:
//    the client reports an `error` and `interrupted`, and the red picture
//    stays on the canvas;
// 2. waits before its first message: the red picture is still there, and
//    goes once the tunnel answers; then draws blue and sends `disconnect`,
//    which ends the session without `interrupted`;
// 3. sends an `error` (SESSION_CONFLICT, as molcajeted does when another
//    window takes the session over): no `interrupted` either.
//
//   SKIP_UI=1 scripts/build-web.sh && (cd tests/browser && npm ci && node tunnel.test.mjs)
//
// Set CHROME=/path/to/chrome to choose the browser binary.
import { chromium } from "playwright-core";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const web = path.join(root, "web/public");

const el = (v) => `${[...String(v)].length}.${v}`;
const ins = (opcode, ...args) => [opcode, ...args].map(el).join(",") + ";";
const frame = (r, g, b, ts) =>
  ins("size", 0, 64, 48) + ins("rect", 0, 0, 0, 64, 48) + ins("cfill", 14, 0, r, g, b, 255) + ins("sync", ts);

function wsFrame(opcode, payload) {
  if (payload.length < 126) return Buffer.concat([Buffer.from([0x80 | opcode, payload.length]), payload]);
  const header = Buffer.from([0x80 | opcode, 126, 0, 0]);
  header.writeUInt16BE(payload.length, 2);
  return Buffer.concat([header, payload]);
}

const UUID = ins("", "00000000-0000-0000-0000-000000000000");
const text = (socket, data) => socket.write(wsFrame(1, Buffer.from(data)));

/** Tunnels opened so far, and what the next one does. */
const tunnels = [];
const scripts = [
  (socket) => {
    text(socket, UUID + frame(255, 0, 0, 1));
    setTimeout(() => socket.destroy(), 300);
  },
  (socket, tunnel) => {
    tunnel.answer = () => text(socket, UUID);
    tunnel.draw = () => text(socket, frame(0, 0, 255, 2));
    tunnel.end = () => text(socket, ins("disconnect"));
  },
  (socket) => text(socket, UUID + ins("error", "The session was opened in another window", 521)),
];

const server = createServer(async (request, response) => {
  const url = new URL(request.url, "http://localhost");
  if (url.pathname === "/") {
    const worker = url.searchParams.get("mode") === "worker";
    response.writeHead(200, { "content-type": "text/html" });
    response.end(`<!doctype html><div id="c" style="width:64px;height:48px;position:relative"></div>
<script type="module">
import { MolcajeteSession } from "./molcajete.js";
window.events = [];
window.session = await MolcajeteSession.create(document.getElementById("c"), { worker: ${worker} });
window.session.set_event_handler((type, detail) => {
  if (type === "interrupted") window.events.push("interrupted " + detail.code);
  if (type === "error") window.events.push("error " + detail.code);
  if (type === "state" && detail === "disconnected") window.events.push("disconnected");
});
window.session.set_scale_mode("native");
window.connectTunnel = () => window.session.connect(window.session.tunnel_url("ws://" + location.host + "/websocket-tunnel", "test"));
window.ready = true;
</script>`);
    return;
  }
  try {
    const file = path.join(web, path.normalize(url.pathname));
    const body = await readFile(file);
    response.writeHead(200, { "content-type": file.endsWith(".wasm") ? "application/wasm" : "text/javascript" });
    response.end(body);
  } catch {
    response.writeHead(404).end();
  }
});

server.on("upgrade", (request, socket) => {
  const accept = createHash("sha1")
    .update(request.headers["sec-websocket-key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")
    .digest("base64");
  socket.write(
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n" +
      `Sec-WebSocket-Accept: ${accept}\r\nSec-WebSocket-Protocol: guacamole\r\n\r\n`,
  );
  socket.on("error", () => {});
  const tunnel = {};
  tunnels.push(tunnel);
  scripts[(tunnels.length - 1) % scripts.length](socket, tunnel);
});

await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const origin = `http://localhost:${server.address().port}`;

const browser = await chromium.launch({
  executablePath: process.env.CHROME,
  args: ["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"],
});

for (const mode of ["worker", "main"]) {
  tunnels.length = 0;
  const page = await browser.newPage({ viewport: { width: 200, height: 200 } });
  page.on("pageerror", (e) => console.error("[pageerror]", e.message));
  await page.goto(`${origin}/?mode=${mode}`);
  await page.waitForFunction(() => window.ready === true);
  const events = () => page.evaluate(() => window.events);
  const until = async (what, check) => {
    for (let i = 0; i < 200; i++) {
      if (await check()) return;
      await page.waitForTimeout(25);
    }
    assert.fail(`${mode}: ${what} (pixel ${await pixel()}, size ${await size()}, events ${await events()})`);
  };
  const pixel = () =>
    page.evaluate(async () => Array.from(await window.session.request("debug_canvas_pixel", 10, 10)));
  const size = () => page.evaluate(async () => Array.from(await window.session.request("debug_layer_size", 0)));

  // 1. Dropped: reported as interrupted, the picture stays.
  await page.evaluate(() => window.connectTunnel());
  await until("the red frame", async () => (await pixel()).join() === "255,0,0,255");
  await until("the drop", async () => (await events()).includes("disconnected"));
  const dropped = await events();
  assert.ok(
    dropped.some((e) => e.startsWith("interrupted ")),
    `${mode}: a dropped tunnel is interrupted: ${dropped}`,
  );
  assert.deepEqual(await pixel(), [255, 0, 0, 255], `${mode}: the last picture stays`);

  // 2. Reconnecting: the picture stays until the tunnel answers.
  await page.evaluate(() => (window.events = []));
  await page.evaluate(() => window.connectTunnel());
  await until("the second tunnel", async () => tunnels.length === 2 && tunnels[1].answer);
  await page.waitForTimeout(200);
  assert.deepEqual(await pixel(), [255, 0, 0, 255], `${mode}: the picture stays while connecting`);
  tunnels[1].answer();
  await until("the display cleared", async () => (await size()).join() === "0,0");
  tunnels[1].draw();
  await until("the blue frame", async () => (await pixel()).join() === "0,0,255,255");
  tunnels[1].end();
  await until("the end", async () => (await events()).includes("disconnected"));
  await page.waitForTimeout(200);
  assert.deepEqual(await events(), ["disconnected"], `${mode}: a session the server ends is not interrupted`);

  // 3. Taken over elsewhere: an error, not interrupted.
  await page.evaluate(() => (window.events = []));
  await page.evaluate(() => window.connectTunnel());
  await until("the error", async () => (await events()).includes("disconnected"));
  await page.waitForTimeout(200);
  assert.deepEqual(await events(), ["error 521", "disconnected"], `${mode}: an error is not interrupted`);
  await page.close();
  console.log(`${mode} mode: ok`);
}

await browser.close();
server.close();
console.log("tunnel tests passed");

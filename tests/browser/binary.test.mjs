// The binary tunnel (molcajete_protocol::binary) in the wasm client, over a
// real WebSocket, in worker mode and in main-thread mode.
//
// A stand-in gateway serves the client and a tunnel that sends one frame:
// PNG images (one larger than a record, one with transparency), a JPEG, PCM
// audio and an H.264 video stream. A client that offers the subprotocol
// `molcajete-binary-v1` gets the stream data as raw bytes in binary messages;
// a client told to stay on the text protocol gets the same frame as base64
// `blob` instructions. Both must draw the same pixels.
//
//   SKIP_UI=1 scripts/build-web.sh && (cd tests/browser && npm ci && node binary.test.mjs)
//
// Set CHROME=/path/to/chrome to choose the browser binary. The video part
// needs `ffmpeg` with libx264 in PATH and is skipped without it.
import { chromium } from "playwright-core";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { fileURLToPath } from "node:url";
import path from "node:path";
import zlib from "node:zlib";
import { encode, frame, splitH264 } from "./video-stream.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const web = path.join(root, "web/public");
const BINARY = "molcajete-binary-v1";

// ---------------------------------------------------------------------------
// Test content
// ---------------------------------------------------------------------------

function pngChunk(type, data) {
  const body = Buffer.concat([Buffer.from(type, "latin1"), data]);
  const out = Buffer.alloc(body.length + 8);
  out.writeUInt32BE(data.length, 0);
  body.copy(out, 4);
  out.writeUInt32BE(zlib.crc32(body), body.length + 4);
  return out;
}

/** A PNG from rows of `channels` bytes per pixel (colour type 2, 3 or 6). */
function png(width, height, colorType, channels, pixel, extra = []) {
  const raw = Buffer.alloc(height * (1 + width * channels));
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) raw.set(pixel(x, y), y * (1 + width * channels) + 1 + x * channels);
  }
  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0);
  header.writeUInt32BE(height, 4);
  header.set([8, colorType, 0, 0, 0], 8);
  return Buffer.concat([
    Buffer.from("\x89PNG\r\n\x1a\n", "latin1"),
    pngChunk("IHDR", header),
    ...extra,
    pngChunk("IDAT", zlib.deflateSync(raw)),
    pngChunk("IEND", Buffer.alloc(0)),
  ]);
}

/** Noise that does not compress, so the image spans several records. */
const noise = (x, y) => {
  let h = Math.imul(x + 1, 0x9e3779b1) ^ Math.imul(y + 1, 0x85ebca6b);
  h = Math.imul(h ^ (h >>> 15), 0x2c1b3c6d);
  h ^= h >>> 13;
  return [h & 0xff, (h >>> 8) & 0xff, (h >>> 16) & 0xff];
};
const NOISE = { x: 0, y: 0, width: 256, height: 128 };
const noisePng = png(NOISE.width, NOISE.height, 2, 3, noise);
assert.ok(noisePng.length > 64 * 1024, "the noise image is larger than a record");

const greenPng = png(16, 16, 2, 3, () => [0, 255, 0]);

// Palette image with a full palette and a transparency chunk: index 0 is
// transparent (left half), index 1 opaque yellow (right half).
const palette = Buffer.alloc(768);
palette.set([255, 255, 0], 3);
const alphaPng = png(16, 16, 3, 1, (x) => [x < 8 ? 0 : 1], [
  pngChunk("PLTE", palette),
  pngChunk("tRNS", Buffer.from([0, 255])),
]);

// 16x16 red, from `ffmpeg -f lavfi -i color=red:size=16x16 -frames:v 1 -f mjpeg`.
const redJpeg = Buffer.from(
  "/9j/4AAQSkZJRgABAgAAAQABAAD//gAQTGF2YzYzLjI0LjEwMAD/2wBDAAgKCgsKCw0NDQ0NDRAPEBAQEBAQEBAQEBASEhIVFRUSEhIQEBISFBQVFRcXFxUVFRUXFxkZGR4eHBwjIyQrKzP/xABMAAEBAAAAAAAAAAAAAAAAAAAABgEBAQAAAAAAAAAAAAAAAAAABgcQAQAAAAAAAAAAAAAAAAAAAAARAQAAAAAAAAAAAAAAAAAAAAD/wAARCAAQABADASIAAhEAAxEA/9oADAMBAAIRAxEAPwCLAE1/f//Z",
  "base64",
);

/** H.264 frames (red, then green) in molcajeted's framing, or null without ffmpeg. */
function videoFrames() {
  try {
    const units = splitH264(encode("h264", ["red", "0x00ff00"]));
    return units.map((unit, i) => frame(unit, i * 100, i === 0));
  } catch {
    return null;
  }
}
const video = videoFrames();
if (!video) console.log("skip video: ffmpeg with libx264 unavailable");

const enc = (v) => `${[...String(v)].length}.${v}`;
const ins = (op, ...args) => [op, ...args].map(enc).join(",") + ";";

/**
 * The frame as a list of parts: `{text}` for instructions and
 * `{stream, data}` for stream data, each `data` one blob.
 */
function content() {
  const parts = [];
  const text = (...t) => parts.push({ text: t.join("") });
  const blob = (stream, data) => parts.push({ stream, data });
  const image = (stream, mimetype, x, y, data) => {
    text(ins("img", stream, 14, 0, mimetype, x, y));
    blob(stream, data);
    text(ins("end", stream));
  };
  text(ins("size", 0, 320, 200), ins("rect", 0, 0, 0, 320, 200), ins("cfill", 14, 0, 0, 0, 255, 255));
  // An audio stream is open while images arrive in the same messages: its
  // data is handed to the page (worker mode) without taking theirs along.
  text(ins("audio", 2, "audio/L16;rate=44100,channels=2"));
  text(ins("img", 1, 14, 0, "image/png", NOISE.x, NOISE.y));
  blob(1, noisePng);
  blob(2, Buffer.alloc(4 * 441));
  text(ins("end", 1));
  image(3, "image/png", 270, 10, greenPng);
  blob(2, Buffer.alloc(4 * 441));
  image(4, "image/jpeg", 270, 40, redJpeg);
  image(5, "image/png", 270, 70, alphaPng);
  text(ins("end", 2));
  if (video) {
    text(ins("size", 1, 64, 64), ins("move", 1, 0, 0, 136, 1), ins("video", 7, 1, "video/h264"));
    // Two frames in one blob, then the rest in pieces that cut through
    // frames, then a frame on its own.
    const last = video.at(-1);
    const middle = Buffer.concat(video.slice(2, -1));
    blob(7, Buffer.concat(video.slice(0, 2)));
    for (let i = 0; i < middle.length; i += 700) blob(7, middle.subarray(i, i + 700));
    blob(7, last);
    text(ins("end", 7));
  }
  text(ins("sync", 1));
  return parts;
}

// ---------------------------------------------------------------------------
// Stand-in gateway
// ---------------------------------------------------------------------------

/** Largest data of one blob record here, so the noise image takes several. */
const RECORD_DATA = 16 * 1024;
/** Messages are filled up to about this size, at record boundaries. */
const MESSAGE_BYTES = 24 * 1024;

function record(tag, ...body) {
  const header = Buffer.alloc(5);
  header[0] = tag;
  header.writeUInt32LE(
    body.reduce((n, b) => n + b.length, 0),
    1,
  );
  return Buffer.concat([header, ...body]);
}

/** Binary messages: whole records, text and blobs interleaved. */
function binaryMessages(parts) {
  const records = [];
  for (const part of parts) {
    if (part.text !== undefined) {
      records.push(record(0, Buffer.from(part.text)));
      continue;
    }
    const stream = Buffer.alloc(4);
    stream.writeUInt32LE(part.stream);
    for (let i = 0; i < part.data.length; i += RECORD_DATA) {
      records.push(record(1, stream, part.data.subarray(i, i + RECORD_DATA)));
    }
  }
  // A record of a kind no client knows yet must be skipped.
  records.splice(3, 0, record(0x7f, Buffer.from("future")));
  const messages = [[]];
  let size = 0;
  for (const r of records) {
    if (size > 0 && size + r.length > MESSAGE_BYTES) {
      messages.push([]);
      size = 0;
    }
    messages.at(-1).push(r);
    size += r.length;
  }
  return messages.map((m) => Buffer.concat(m));
}

/** Text messages: the same frame as stock Guacamole instructions. */
function textMessages(parts) {
  let all = "";
  for (const part of parts) {
    if (part.text !== undefined) all += part.text;
    else {
      for (let i = 0; i < part.data.length; i += 6048) {
        all += ins("blob", part.stream, part.data.subarray(i, i + 6048).toString("base64"));
      }
    }
  }
  // Whole instructions per message, like the gateway.
  const messages = [];
  let current = "";
  for (const instruction of all.match(/[^;]*;/g)) {
    if (current.length + instruction.length > MESSAGE_BYTES && current) {
      messages.push(current);
      current = "";
    }
    current += instruction;
  }
  messages.push(current);
  return messages;
}

function wsFrame(opcode, payload) {
  const length = payload.length;
  let header;
  if (length < 126) header = Buffer.from([0x80 | opcode, length]);
  else if (length < 65536) {
    header = Buffer.alloc(4);
    header[0] = 0x80 | opcode;
    header[1] = 126;
    header.writeUInt16BE(length, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x80 | opcode;
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(length), 2);
  }
  return Buffer.concat([header, payload]);
}

/** Calls `onText` for every (masked, unfragmented) text frame of a client. */
function readClientFrames(socket, onText) {
  let buffer = Buffer.alloc(0);
  socket.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    for (;;) {
      if (buffer.length < 6) return;
      let length = buffer[1] & 0x7f;
      let offset = 2;
      if (length === 126) {
        length = buffer.readUInt16BE(2);
        offset = 4;
      } else if (length === 127) {
        length = Number(buffer.readBigUInt64BE(2));
        offset = 10;
      }
      if (buffer.length < offset + 4 + length) return;
      const mask = buffer.subarray(offset, offset + 4);
      const payload = Buffer.from(buffer.subarray(offset + 4, offset + 4 + length));
      for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i & 3];
      if ((buffer[0] & 0x0f) === 1) onText(payload.toString());
      buffer = buffer.subarray(offset + 4 + length);
    }
  });
}

/** What the tunnels opened so far did, newest last. */
const tunnels = [];

const server = createServer(async (request, response) => {
  const url = new URL(request.url, "http://localhost");
  if (url.pathname === "/") {
    const worker = url.searchParams.get("mode") === "worker";
    const binary = url.searchParams.get("tunnel") === "binary";
    response.writeHead(200, { "content-type": "text/html" });
    response.end(`<!doctype html><div id="c" style="width:360px;height:240px;position:relative"></div>
<script type="module">
import { MolcajeteSession } from "./molcajete.js";
window.states = [];
window.session = await MolcajeteSession.create(document.getElementById("c"), { worker: ${worker}, binary: ${binary} });
window.session.set_event_handler((type, detail) => {
  if (type === "state") window.states.push(detail);
  if (type === "error") window.states.push("error: " + detail.message);
});
window.session.set_scale_mode("native");
window.session.connect(window.session.tunnel_url("ws://" + location.host + "/websocket-tunnel", "test"));
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
  const offered = (request.headers["sec-websocket-protocol"] ?? "").split(",").map((p) => p.trim());
  const selected = offered.includes(BINARY) ? BINARY : "guacamole";
  const accept = createHash("sha1")
    .update(request.headers["sec-websocket-key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")
    .digest("base64");
  socket.write(
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n" +
      `Sec-WebSocket-Accept: ${accept}\r\nSec-WebSocket-Protocol: ${selected}\r\n\r\n`,
  );
  const tunnel = { offered, selected, bytes: 0, messages: 0, received: "" };
  tunnel.synced = new Promise((resolve) => {
    readClientFrames(socket, (text) => {
      tunnel.received += text;
      if (text.includes(ins("sync", 1))) resolve();
    });
  });
  tunnels.push(tunnel);
  const send = (opcode, payload) => {
    tunnel.bytes += payload.length;
    tunnel.messages += 1;
    socket.write(wsFrame(opcode, payload));
  };
  // The tunnel's own messages are text under both subprotocols.
  socket.write(wsFrame(1, Buffer.from(ins("", "00000000-0000-0000-0000-000000000000"))));
  const parts = content();
  if (selected === BINARY) for (const message of binaryMessages(parts)) send(2, message);
  else for (const message of textMessages(parts)) send(1, Buffer.from(message));
  socket.on("error", () => {});
});

await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const origin = `http://localhost:${server.address().port}`;

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

const browser = await chromium.launch({
  executablePath: process.env.CHROME,
  args: ["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"],
});

const near = (actual, expected, tolerance) => actual.every((v, i) => Math.abs(v - expected[i]) <= tolerance);
const results = {};

for (const mode of ["worker", "main"]) {
  for (const tunnelKind of ["binary", "text"]) {
    const name = `${mode} mode, ${tunnelKind} tunnel`;
    const page = await browser.newPage({ viewport: { width: 400, height: 300 } });
    const errors = [];
    page.on("pageerror", (e) => errors.push(e.message));
    page.on("console", (m) => {
      if (m.type() === "error" || m.type() === "warning") errors.push(m.text());
    });
    await page.goto(`${origin}/?mode=${mode}&tunnel=${tunnelKind}`);
    await page.waitForFunction(() => window.ready === true);
    const tunnel = await (async () => {
      for (let i = 0; i < 200 && tunnels.length === Object.keys(results).length; i++) await page.waitForTimeout(25);
      return tunnels.at(-1);
    })();
    assert.equal(tunnels.length, Object.keys(results).length + 1, `${name}: the tunnel was opened`);
    assert.equal(await page.evaluate(() => window.session.mode), mode);

    // What was offered and selected.
    const expected = tunnelKind === "binary" ? BINARY : "guacamole";
    assert.deepEqual(tunnel.offered, tunnelKind === "binary" ? [BINARY, "guacamole"] : ["guacamole"], name);
    assert.equal(tunnel.selected, expected);
    assert.equal(await page.evaluate(() => window.session.request("tunnel_protocol")), expected, name);

    // The client acknowledges the frame once everything in it is drawn.
    await Promise.race([
      tunnel.synced,
      new Promise((_, reject) => setTimeout(() => reject(new Error(`${name}: frame not acknowledged`)), 15000)),
    ]);
    const pixel = (x, y) =>
      page.evaluate(async ([x, y]) => Array.from(await window.session.request("debug_canvas_pixel", x, y)), [x, y]);
    assert.equal(await page.evaluate(() => window.session.request("debug_queued")), 0, `${name}: nothing left queued`);
    const size = await page.evaluate(async () => Array.from(await window.session.request("debug_layer_size", 0)));
    assert.deepEqual(size, [320, 200], `${name}: display size`);

    // The large PNG, bit for bit (it arrived in several blobs).
    for (const [x, y] of [[0, 0], [255, 0], [0, 127], [255, 127], [97, 61], [200, 100]]) {
      assert.deepEqual(await pixel(NOISE.x + x, NOISE.y + y), [...noise(x, y), 255], `${name}: noise pixel ${x},${y}`);
    }
    assert.deepEqual(await pixel(278, 18), [0, 255, 0, 255], `${name}: small PNG`);
    assert.ok(near(await pixel(278, 48), [255, 0, 0, 255], 12), `${name}: JPEG is red, got ${await pixel(278, 48)}`);
    // Transparent half shows the blue background, opaque half is yellow.
    assert.deepEqual(await pixel(273, 78), [0, 0, 255, 255], `${name}: transparent PNG pixels`);
    assert.deepEqual(await pixel(282, 78), [255, 255, 0, 255], `${name}: opaque PNG pixels`);
    assert.deepEqual(await pixel(300, 150), [0, 0, 255, 255], `${name}: background`);
    if (video) {
      const [r, g, b] = await pixel(32, 168);
      assert.ok(g > 200 && r < 60 && b < 60, `${name}: last video frame is green, got ${[r, g, b]}`);
    }
    assert.deepEqual(await page.evaluate(() => window.states.filter((s) => s.startsWith("error"))), [], name);
    assert.deepEqual(errors, [], `${name}: no errors or warnings in the page`);

    results[name] = { bytes: tunnel.bytes, messages: tunnel.messages };
    console.log(`ok - ${name}: ${tunnel.bytes} bytes in ${tunnel.messages} messages`);
    await page.close();
  }
}

const binaryBytes = results["worker mode, binary tunnel"].bytes;
const textBytes = results["worker mode, text tunnel"].bytes;
assert.ok(binaryBytes < textBytes * 0.8, "raw stream data takes at least a fifth less than base64");
console.log(`binary tunnel: ${binaryBytes} bytes, text tunnel: ${textBytes} bytes (${((binaryBytes / textBytes - 1) * 100).toFixed(1)}%)`);

await browser.close();
server.close();
server.closeAllConnections?.();

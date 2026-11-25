// Video stream decoding in the wasm client: real H.264 / AV1 bitstreams
// produced by ffmpeg are framed like molcajeted does (molcajete_protocol::video),
// fed as a `video` stream into a layer, and the composited canvas is checked.
//
//   scripts/build-web.sh && (cd tests/browser && node video.test.mjs)
//
// Needs `ffmpeg` with libx264 / libsvtav1 in PATH; missing encoders are skipped.
import { chromium } from "playwright-core";
import { execFileSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");

/** Encodes `colors` (one second each at 10 fps) as an elementary stream. */
function encode(codec, colors) {
  const inputs = colors.flatMap((c) => ["-f", "lavfi", "-i", `color=${c}:size=64x64:rate=10:duration=0.3`]);
  const concat = colors.map((_, i) => `[${i}:v]`).join("") + `concat=n=${colors.length}:v=1:a=0[v]`;
  const common = ["-hide_banner", "-loglevel", "error", ...inputs, "-filter_complex", concat, "-map", "[v]", "-pix_fmt", "yuv420p"];
  const args = {
    h264: [...common, "-c:v", "libx264", "-bf", "0", "-x264-params", "aud=1", "-f", "h264", "-"],
    av1: [...common, "-c:v", "libsvtav1", "-preset", "12", "-f", "obu", "-"],
  }[codec];
  return execFileSync("ffmpeg", args, { maxBuffer: 1 << 26, stdio: ["ignore", "pipe", "ignore"] });
}

/** Splits an Annex B stream at access unit delimiters (NAL type 9). */
function splitH264(buf) {
  const starts = [];
  for (let i = 0; i + 4 < buf.length; i++) {
    if (buf[i] === 0 && buf[i + 1] === 0 && buf[i + 2] === 0 && buf[i + 3] === 1 && (buf[i + 4] & 0x1f) === 9) starts.push(i);
  }
  return starts.map((s, k) => buf.subarray(s, starts[k + 1] ?? buf.length));
}

/** Splits AV1 low-overhead OBUs into temporal units (OBU type 2 starts one). */
function splitAv1(buf) {
  const units = [];
  let i = 0;
  let start = 0;
  while (i < buf.length) {
    const header = buf[i];
    const type = (header >> 3) & 0xf;
    const hasExt = (header >> 2) & 1;
    let j = i + 1 + hasExt;
    let size = 0;
    for (let shift = 0; ; shift += 7) {
      const b = buf[j++];
      size |= (b & 0x7f) << shift;
      if (!(b & 0x80)) break;
    }
    if (type === 2 && i > start) {
      units.push(buf.subarray(start, i));
      start = i;
    }
    i = j + size;
  }
  units.push(buf.subarray(start));
  return units;
}

/** molcajete_protocol::video framing. */
function frame(payload, timestampMs, key) {
  const out = Buffer.alloc(9 + payload.length);
  out.writeUInt32BE(payload.length, 0);
  out.writeUInt32BE(timestampMs, 4);
  out[8] = key ? 1 : 0;
  payload.copy(out, 9);
  return out;
}

const cache = path.join(process.env.HOME ?? "", ".cache/ms-playwright/chromium-1228/chrome-linux64/chrome");
const browser = await chromium.launch({
  executablePath: process.env.CHROME ?? (existsSync(cache) ? cache : undefined),
  args: ["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"],
});
const page = await browser.newPage({ viewport: { width: 400, height: 300 } });
page.on("pageerror", (e) => console.error("[pageerror]", e.message));
await page.route("http://localhost:9/**", async (route) => {
  const p = new URL(route.request().url()).pathname;
  if (p === "/") {
    return route.fulfill({
      contentType: "text/html",
      body: `<!doctype html><div id="c" style="width:200px;height:200px;position:relative"></div>
<script type="module">
import init, { MolcajeteClient } from "./pkg/molcajete_client.js";
await init();
window.client = new MolcajeteClient(document.getElementById("c"));
window.ready = true;
</script>`,
    });
  }
  const file = path.join(root, "web", p);
  return route.fulfill({ contentType: file.endsWith(".wasm") ? "application/wasm" : "text/javascript", body: await readFile(file) });
});
await page.goto("http://localhost:9/");
await page.waitForFunction(() => window.ready === true);

let failures = 0;
for (const codec of ["h264", "av1"]) {
  let stream;
  try {
    stream = encode(codec, ["red", "0x00ff00"]);
  } catch (err) {
    console.log(`skip ${codec}: ffmpeg encoder unavailable`);
    continue;
  }
  const units = codec === "h264" ? splitH264(stream) : splitAv1(stream);
  const framed = Buffer.concat(units.map((u, i) => frame(u, i * 100, i === 0)));
  const result = await page.evaluate(
    async ({ codec, b64, count }) => {
      const enc = (v) => `${[...String(v)].length}.${v}`;
      const ins = (op, ...a) => [op, ...a].map(enc).join(",") + ";";
      const client = window.client;
      client.debug_feed(ins("size", 0, 100, 100) + ins("rect", 0, 0, 0, 100, 100) + ins("cfill", 14, 0, 0, 0, 255, 255));
      client.debug_feed(ins("size", 1, 64, 64) + ins("move", 1, 0, 10, 10, 1));
      client.debug_feed(ins("video", 7, 1, `video/${codec}`));
      // Split the stream across blobs at arbitrary points.
      for (let i = 0; i < b64.length; i += 1000) client.debug_feed(ins("blob", 7, b64.slice(i, i + 1000)));
      client.debug_feed(ins("end", 7) + ins("sync", 1));
      const settle = async () => {
        for (let i = 0; i < 300 && client.debug_queued() > 0; i++) await new Promise((r) => setTimeout(r, 10));
      };
      await settle();
      const queued = client.debug_queued();
      const inside = Array.from(client.debug_canvas_pixel(40, 40));
      const outside = Array.from(client.debug_canvas_pixel(90, 90));
      // Leaving video mode: the layer goes away and the default layer shows.
      client.debug_feed(ins("dispose", 1) + ins("sync", 2));
      await settle();
      return { queued, inside, outside, disposed: Array.from(client.debug_canvas_pixel(40, 40)), count };
    },
    // Base64 in 4-char groups so every 1000-char blob decodes on its own.
    { codec, b64: framed.toString("base64"), count: units.length },
  );
  const [r, g, b] = result.inside;
  const ok = result.queued === 0 && g > 200 && r < 60 && b < 60 && result.outside[2] > 200 && result.disposed[2] > 200;
  console.log(
    `${ok ? "ok  " : "FAIL"} ${codec}: ${result.count} frames, last frame pixel [${result.inside.slice(0, 3)}], ` +
      `outside [${result.outside.slice(0, 3)}], after dispose [${result.disposed.slice(0, 3)}]`,
  );
  if (!ok) failures++;
}
await browser.close();
process.exit(failures ? 1 : 0);

// Video stream decoding in the wasm client: real H.264 / AV1 bitstreams
// produced by ffmpeg are framed like molcajeted does (molcajete_protocol::video),
// fed as a `video` stream into a layer, and the composited canvas is checked.
//
//   scripts/build-web.sh && (cd tests/browser && node video.test.mjs)
//
// Needs `ffmpeg` with libx264 / libsvtav1 in PATH; missing encoders are skipped.
import { chromium } from "playwright-core";
import { readFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { encode, frame, splitAv1, splitH264 } from "./video-stream.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");

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
  const file = path.join(root, "web/public", p);
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

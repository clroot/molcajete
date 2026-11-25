// Compares the main-thread cost of the stock guacamole-common-js client and
// the wasm/WebGL2 client on the same live workload.
//
//   GUAC_COMMON_JS=/path/to/guacamole-client/guacamole-common-js/src/main/webapp/modules \
//   node bench.mjs [connection-id] [seconds] [rounds]
//
// Requirements: web/public/pkg built (scripts/build-web.sh), target/debug/molcajete-gateway
// or target/release/molcajete-gateway built, gateway.toml with the connection, and
// something animating on the remote desktop (e.g. a scrolling terminal).
// A temporary gateway is started on a free port with its own static dir.
import { chromium } from "playwright-core";
import { spawn } from "node:child_process";
import { cp, mkdtemp, readFile, readdir, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import path from "node:path";

const [connection = "vnc", seconds = "10", rounds = "2"] = process.argv.slice(2);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const modules = process.env.GUAC_COMMON_JS;
if (!modules || !existsSync(modules)) {
  console.error("Set GUAC_COMMON_JS to guacamole-common-js/src/main/webapp/modules of an Apache Guacamole checkout.");
  process.exit(2);
}

const freePort = () =>
  new Promise((resolve) => {
    const srv = createServer().listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });

// Static dir: both clients plus the wasm package.
const dir = await mkdtemp(path.join(tmpdir(), "guac-bench-"));
const files = (await readdir(modules)).filter((f) => f.endsWith(".js")).sort();
const ordered = ["Namespace.js", ...files.filter((f) => f !== "Namespace.js")];
const stockJs = (await Promise.all(ordered.map((f) => readFile(path.join(modules, f), "utf8")))).join("\n");
await writeFile(path.join(dir, "guacamole-common.js"), stockJs);
await cp(path.join(root, "web/public/pkg"), path.join(dir, "pkg"), { recursive: true });

const port = await freePort();
const host = `127.0.0.1:${port}`;
await writeFile(
  path.join(dir, "stock.html"),
  `<!doctype html><body style="margin:0;background:#000"><div id="d"></div>
<script src="/guacamole-common.js"></script><script>
const client = new Guacamole.Client(new Guacamole.WebSocketTunnel("ws://${host}/websocket-tunnel"));
document.getElementById("d").appendChild(client.getDisplay().getElement());
window.frames_ = 0;
client.onsync = () => window.frames_++;
client.connect("GUAC_ID=${connection}&GUAC_WIDTH=1280&GUAC_HEIGHT=800&GUAC_DPI=96&GUAC_IMAGE=image/png&GUAC_IMAGE=image/jpeg&GUAC_IMAGE=image/webp");
</script>`,
);
await writeFile(
  path.join(dir, "wasm.html"),
  `<!doctype html><body style="margin:0;background:#000"><div id="d" style="width:1280px;height:800px"></div>
<script type="module">
import init, { MolcajeteClient } from "/pkg/molcajete_client.js";
await init();
const client = new MolcajeteClient(document.getElementById("d"));
window.frames_ = 0;
client.set_event_handler((type, detail) => {
  if (type === "stats") window.frames_ += Math.round(detail.framesPerSecond);
  if (type === "state" || type === "error") console.error(type, JSON.stringify(detail));
});
client.connect("ws://${host}/websocket-tunnel?connection=${connection}&width=1280&height=800&dpi=96&image=image%2Fpng&image=image%2Fjpeg&image=image%2Fwebp");
</script>`,
);

// The tunnel needs a session: sign in as a bench administrator, created in
// a throwaway SQLite database next to the static files.
const benchUser = { username: "bench", password: "bench-password" };
const config = (await readFile(path.join(root, "gateway.toml"), "utf8"))
  .replace(/^listen\s*=.*$/m, `listen = "${host}"`)
  .replace(/^static_dir\s*=.*$/m, `static_dir = ${JSON.stringify(dir)}`)
  .replace(/^\[\[users\]\][\s\S]*?(?=^\[)/gm, "")
  .concat(`\n[[users]]\nusername = "${benchUser.username}"\npassword = "${benchUser.password}"\n`);
await writeFile(path.join(dir, "gateway.toml"), config);
const binary = ["release", "debug"].map((p) => path.join(root, "target", p, "molcajete-gateway")).find(existsSync);
const gateway = spawn(binary, ["--config", path.join(dir, "gateway.toml"), "--data-dir", dir], { stdio: "ignore" });
await new Promise((r) => setTimeout(r, 1000));
const login = await fetch(`http://${host}/api/auth/login`, {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify(benchUser),
});
if (!login.ok) throw new Error(`bench login failed: HTTP ${login.status}`);
const sessionCookie = login.headers.get("set-cookie").split(";")[0].split("=");

async function measure(kind) {
  const cache = path.join(process.env.HOME ?? "", ".cache/ms-playwright/chromium-1228/chrome-linux64/chrome");
  const browser = await chromium.launch({
    executablePath: process.env.CHROME ?? (existsSync(cache) ? cache : undefined),
    args: (process.env.CHROME_ARGS ?? "--use-angle=swiftshader --enable-unsafe-swiftshader --ignore-gpu-blocklist").split(" "),
  });
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  await page.context().addCookies([{ name: sessionCookie[0], value: sessionCookie[1], url: `http://${host}` }]);
  page.on("console", (m) => m.type() === "error" && console.error(`[${kind}] ${m.text()}`));
  page.on("pageerror", (e) => console.error(`[${kind}] ${e.message}`));
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("Performance.enable");
  await page.goto(`http://${host}/${kind}.html`);
  await page.waitForTimeout(3000);
  const metrics = async () =>
    Object.fromEntries((await cdp.send("Performance.getMetrics")).metrics.map((m) => [m.name, m.value]));
  const f0 = await page.evaluate(() => window.frames_);
  const m0 = await metrics();
  await page.waitForTimeout(Number(seconds) * 1000);
  const m1 = await metrics();
  const f1 = await page.evaluate(() => window.frames_);
  await browser.close();
  const frames = f1 - f0;
  const task = (m1.TaskDuration - m0.TaskDuration) * 1000;
  return {
    client: kind,
    "frames/s": +(frames / seconds).toFixed(1),
    "main thread ms/s": +(task / seconds).toFixed(1),
    "script ms/s": +(((m1.ScriptDuration - m0.ScriptDuration) * 1000) / seconds).toFixed(1),
    "ms/frame": +(task / Math.max(frames, 1)).toFixed(2),
  };
}

try {
  const results = [];
  for (let i = 0; i < Number(rounds); i++) {
    results.push(await measure("stock"));
    results.push(await measure("wasm"));
  }
  console.table(results);
} finally {
  gateway.kill();
}

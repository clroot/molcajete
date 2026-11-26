// File transfer end to end: a gateway, molcajeted and an SSH server with
// SFTP, driven through MolcajeteSession (the embedding API) in headless
// Chromium, in worker mode and in main-thread mode.
//
// - Uploads a file, lists its folder, downloads it through memory (a Blob
//   saved as a browser download) and to a file (File System Access, here a
//   file in the origin private file system standing in for the save
//   dialog), and compares the bytes.
// - With a connection that blocks both directions, both are refused.
// - TUNNEL=text runs it on the text protocol instead of the binary tunnel.
// - LARGE=<GiB> also downloads and uploads a file of that size and reports
//   the browser's memory and the session's frame rate meanwhile.
//
// Needs the SSH test target (docker/compose.yml `ssh`, guac/guac on
// 127.0.0.1:2222), molcajeted, and a gateway with two connections:
//
//   ssh      enable-sftp=true
//   ssh-ro   enable-sftp=true, sftp-disable-upload=true, sftp-disable-download=true
//
//   GATEWAY=http://127.0.0.1:18099 USER=admin PASSWORD=change-me node files.e2e.mjs
//
// The large run creates /config/large.bin in the SSH container
// (SSH_CONTAINER, default gh53-ssh-1) and a file of the same size in the
// system's temporary directory, and removes both.
import { chromium } from "playwright-core";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { createReadStream, createWriteStream } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const GATEWAY = process.env.GATEWAY ?? "http://127.0.0.1:18099";
const USER = process.env.GATEWAY_USER ?? "admin";
const PASSWORD = process.env.GATEWAY_PASSWORD ?? "change-me";
const LARGE_GIB = Number(process.env.LARGE ?? 0);
const SSH_CONTAINER = process.env.SSH_CONTAINER ?? "gh53-ssh-1";
const modes = (process.env.MODES ?? "worker,main").split(",");
// TUNNEL=text keeps the tunnel on the Guacamole text protocol (base64 blobs).
const TEXT = process.env.TUNNEL === "text";

const sha256 = (data) => createHash("sha256").update(data).digest("hex");
async function sha256File(file) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest("hex");
}

/** Resident memory of the browser's processes together, in MiB. */
function browserRss() {
  const out = execFileSync("ps", ["-axo", "rss=,command="], { encoding: "utf8" });
  let kib = 0;
  for (const line of out.split("\n")) {
    if (/chrome-headless-shell|molcajete-e2e-profile-/.test(line)) {
      kib += Number(line.trim().split(/\s+/, 1)[0]) || 0;
    }
  }
  return Math.round(kib / 1024);
}

// A profile on disk: an incognito-like context keeps the origin private file
// system in memory, with a small quota.
const profile = await mkdtemp(path.join(os.tmpdir(), "molcajete-e2e-profile-"));
const context = await chromium.launchPersistentContext(profile, {
  executablePath: process.env.CHROME,
  acceptDownloads: true,
  // CHROME_ARGS adds flags, e.g. --unsafely-treat-insecure-origin-as-secure=<GATEWAY>
  // for a gateway on another host, where the page is not a secure context.
  args: [
    "--use-angle=swiftshader",
    "--enable-unsafe-swiftshader",
    "--ignore-gpu-blocklist",
    ...(process.env.CHROME_ARGS ?? "").split(" ").filter(Boolean),
  ],
});
const page = context.pages()[0] ?? (await context.newPage());
page.on("pageerror", (err) => console.error("page error:", err));
await page.goto(`${GATEWAY}/`);
const login = await page.evaluate(
  async ([username, password]) =>
    (await fetch("api/auth/login", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ username, password }),
    })).status,
  [USER, PASSWORD],
);
assert.equal(login, 200, "sign-in");

/** Opens a session in the page; resolves to its filesystem. */
async function open(connection, mode) {
  return page.evaluate(
    async ([connection, worker, binary]) => {
      document.body.innerHTML = "";
      const container = document.createElement("div");
      Object.assign(container.style, { position: "fixed", inset: "0" });
      document.body.append(container);
      const { MolcajeteSession } = await import(new URL("molcajete.js", document.baseURI).href);
      window.__session?.free();
      const session = await MolcajeteSession.create(container, { worker, binary });
      window.__session = session;
      window.__events = [];
      window.__stats = [];
      const filesystem = new Promise((resolve, reject) => {
        session.set_event_handler((type, detail) => {
          if (type === "filesystem") resolve(detail);
          else if (type === "error") reject(new Error(detail.message));
          else if (type === "transfer") window.__events.push(detail);
          else if (type === "stats") window.__stats.push(detail);
        });
      });
      const ws = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/websocket-tunnel`;
      session.connect(session.tunnel_url(ws, connection));
      return filesystem;
    },
    [connection, mode === "worker", !TEXT],
  );
}

/** Waits for a transfer to finish; returns its last event. */
async function finished(id, timeout = 120_000) {
  const handle = await page.waitForFunction(
    (id) => window.__events.find((e) => e.id === id && e.state !== "active"),
    id,
    { timeout, polling: 100 },
  );
  return handle.jsonValue();
}

/** Puts a local file into a file input of the page; returns nothing, the File is window.__file. */
async function pickFile(file) {
  await page.evaluate(() => {
    const input = document.createElement("input");
    input.type = "file";
    input.id = "pick";
    document.body.append(input);
  });
  await page.setInputFiles("#pick", file);
  await page.evaluate(() => {
    window.__file = document.getElementById("pick").files[0];
    document.getElementById("pick").remove();
  });
}

/** Makes the save dialog answer with a file in the origin private file system. */
async function saveToPrivateFiles() {
  await page.evaluate(() => {
    window.showSaveFilePicker = async ({ suggestedName }) =>
      (await navigator.storage.getDirectory()).getFileHandle(suggestedName, { create: true });
  });
}

const results = [];
const tmp = os.tmpdir();

for (const mode of modes) {
  // ---- Upload, list, download -------------------------------------------
  const fs = await open("ssh", mode);
  assert.deepEqual([fs.canDownload, fs.canUpload], [true, true], `${mode}: filesystem ${JSON.stringify(fs)}`);
  const data = randomBytes(3 * 1024 * 1024 + 12345);
  const local = path.join(tmp, `molcajete-e2e-${mode}.bin`);
  await writeFile(local, data);
  await pickFile(local);
  const remote = `/tmp/e2e-${mode}.bin`;
  const uploadId = await page.evaluate(
    ([object, path]) => window.__session.upload(window.__file, { object, path }),
    [fs.object, remote],
  );
  const upload = await finished(uploadId);
  assert.equal(upload.state, "done", `${mode}: upload ${JSON.stringify(upload)}`);

  const entries = await page.evaluate(([object]) => window.__session.list(object, "/tmp"), [fs.object]);
  const entry = entries.find((e) => e.path === remote);
  assert.deepEqual(entry, { path: remote, name: path.basename(remote), type: "file", size: data.length });

  // Through memory: a Blob the page saves as a download.
  await page.evaluate(() => delete window.showSaveFilePicker);
  const [download] = await Promise.all([
    page.waitForEvent("download"),
    page.evaluate(([object, path]) => window.__session.download(object, path), [fs.object, remote]),
  ]);
  const saved = path.join(tmp, `molcajete-e2e-${mode}-download.bin`);
  await download.saveAs(saved);
  assert.equal(sha256(await readFile(saved)), sha256(data), `${mode}: blob download`);
  await rm(saved);

  // To a file, as it arrives.
  await saveToPrivateFiles();
  const id = await page.evaluate(
    ([object, path]) => window.__session.download(object, path, { name: "e2e.bin" }),
    [fs.object, remote],
  );
  const streamed = await finished(id);
  assert.equal(streamed.state, "done", `${mode}: file download ${JSON.stringify(streamed)}`);
  const written = await page.evaluate(async () => {
    const file = await (await (await navigator.storage.getDirectory()).getFileHandle("e2e.bin")).getFile();
    const digest = await crypto.subtle.digest("SHA-256", await file.arrayBuffer());
    return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
  });
  assert.equal(written, sha256(data), `${mode}: file download contents`);
  await rm(local);

  // ---- Blocked directions ----------------------------------------------
  const ro = await open("ssh-ro", mode);
  assert.deepEqual([ro.canDownload, ro.canUpload], [false, false], `${mode}: read-only flags`);
  await pickFile(path.join(import.meta.dirname, "package.json"));
  const refusedUpload = await finished(
    await page.evaluate(([object]) => window.__session.upload(window.__file, { object, path: "/tmp/no.json" }), [
      ro.object,
    ]),
  );
  assert.equal(refusedUpload.state, "failed", `${mode}: blocked upload`);
  const refusedDownload = await finished(
    await page.evaluate(([object, path]) => window.__session.download(object, path), [ro.object, remote]),
  );
  assert.equal(refusedDownload.state, "failed", `${mode}: blocked download`);
  results.push(`${mode}: upload, list, blob and file downloads, blocked directions ok`);
}

// ---- Large files ----------------------------------------------------------
if (LARGE_GIB > 0) {
  const size = LARGE_GIB * 1024 ** 3;
  const mode = modes[0];
  console.log(`creating ${LARGE_GIB} GiB in ${SSH_CONTAINER}...`);
  execFileSync("docker", [
    "exec", SSH_CONTAINER, "sh", "-c",
    `head -c ${size} /dev/urandom > /config/large.bin && chown 1000:1000 /config/large.bin`,
  ]);
  const remoteHash = execFileSync("docker", ["exec", SSH_CONTAINER, "sha256sum", "/config/large.bin"], {
    encoding: "utf8",
  }).split(" ")[0];

  const fs = await open("ssh", mode);
  // Keep the terminal drawing during the transfers: a clock every 100 ms.
  await page.keyboard.type("while true; do date +%T.%N; sleep 0.1; done\n");
  await saveToPrivateFiles();

  const watch = async (id) => {
    const start = Date.now();
    const before = browserRss();
    let peak = before;
    let state;
    while (!(state = await page.evaluate((id) => window.__events.find((e) => e.id === id && e.state !== "active"), id))) {
      peak = Math.max(peak, browserRss());
      await new Promise((r) => setTimeout(r, 1000));
    }
    const seconds = (Date.now() - start) / 1000;
    const stats = await page.evaluate(() => window.__stats.splice(0));
    const fps = stats.map((s) => s.framesPerSecond);
    return { state, seconds, before, peak, fps };
  };

  await page.evaluate(() => (window.__stats = []));
  const downloadId = await page.evaluate(
    ([object]) => window.__session.download(object, "/config/large.bin", { name: "large.bin" }),
    [fs.object],
  );
  const down = await watch(downloadId);
  assert.equal(down.state.state, "done", JSON.stringify(down.state));
  const downloadedSize = await page.evaluate(
    async () => (await (await (await navigator.storage.getDirectory()).getFileHandle("large.bin")).getFile()).size,
  );
  assert.equal(downloadedSize, size);
  await page.evaluate(async () => (await navigator.storage.getDirectory()).removeEntry("large.bin"));

  const local = path.join(tmp, "molcajete-e2e-large.bin");
  console.log(`creating ${LARGE_GIB} GiB locally...`);
  await new Promise((resolve, reject) => {
    const out = createWriteStream(local);
    let left = size;
    const chunk = randomBytes(16 * 1024 * 1024);
    const write = () => {
      while (left > 0) {
        const piece = left >= chunk.length ? chunk : chunk.subarray(0, left);
        left -= piece.length;
        if (!out.write(piece)) return out.once("drain", write);
      }
      out.end(resolve);
    };
    out.on("error", reject);
    write();
  });
  const localHash = await sha256File(local);
  await pickFile(local);
  const uploadId = await page.evaluate(
    ([object]) => window.__session.upload(window.__file, { object, path: "/config/uploaded.bin" }),
    [fs.object],
  );
  const up = await watch(uploadId);
  assert.equal(up.state.state, "done", JSON.stringify(up.state));
  const uploadedHash = execFileSync("docker", ["exec", SSH_CONTAINER, "sha256sum", "/config/uploaded.bin"], {
    encoding: "utf8",
  }).split(" ")[0];
  assert.equal(uploadedHash, localHash, "large upload contents");
  await rm(local);
  execFileSync("docker", ["exec", SSH_CONTAINER, "rm", "-f", "/config/large.bin", "/config/uploaded.bin"]);
  void remoteHash;

  const summary = (what, r) => {
    const fps = r.fps.length ? r.fps : [0];
    const median = [...fps].sort((a, b) => a - b)[Math.floor(fps.length / 2)];
    return (
      `${what} ${LARGE_GIB} GiB (${mode}): ${r.seconds.toFixed(0)} s, ` +
      `${((size / r.seconds) / 1024 ** 2).toFixed(0)} MiB/s, browser RSS ${r.before} MiB before, ` +
      `${r.peak} MiB at most; terminal frames/s median ${median.toFixed(1)}, min ${Math.min(...fps).toFixed(1)}`
    );
  };
  results.push(summary("download", down), summary("upload", up));
}

await context.close();
await rm(profile, { recursive: true, force: true });
for (const line of results) console.log(line);

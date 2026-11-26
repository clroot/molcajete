// RDP drive redirection end to end: a gateway, molcajeted and an xrdp
// desktop that mounts the redirected drive with FUSE, driven through
// MolcajeteSession in headless Chromium.
//
// - A file uploaded in the browser is read by the remote desktop through
//   the drive, and its bytes match.
// - Files the remote desktop writes, renames and puts in a new folder
//   through the drive are listed and downloaded in the browser, and their
//   bytes match.
//
// Needs an xrdp container with FUSE (RDP_CONTAINER, default gh98-rdp-fuse):
//
//   docker run -d --name gh98-rdp-fuse --network docker_default \
//     --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined \
//     -p 127.0.0.1:13390:3389 scottyhardy/docker-remote-desktop:latest
//   docker exec gh98-rdp-fuse apt-get install -y fuse   # fusermount
//
// and a gateway with a connection to it (CONNECTION, default rdp-drive):
//
//   rdp-drive   username=ubuntu password=ubuntu security=any ignore-cert=true
//               enable-drive=true drive-name=molcajete drive-path=<dir> create-drive-path=true
//
//   GATEWAY=http://127.0.0.1:18099 node drive.e2e.mjs
import { chromium } from "playwright-core";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const GATEWAY = process.env.GATEWAY ?? "http://127.0.0.1:18099";
const USER = process.env.GATEWAY_USER ?? "admin";
const PASSWORD = process.env.GATEWAY_PASSWORD ?? "change-me";
const CONNECTION = process.env.CONNECTION ?? "rdp-drive";
const RDP_CONTAINER = process.env.RDP_CONTAINER ?? "gh98-rdp-fuse";
const DRIVE_NAME = process.env.DRIVE_NAME ?? "molcajete";
// When the browser runs on Windows and docker in WSL, `wsl.exe -e docker`
// (`-e`: no shell in between, which would expand `$HOME` and split the script).
const DOCKER = (process.env.DOCKER ?? "docker").split(" ");

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

/** Runs `script` as the desktop's user in the xrdp container. */
function remote(script) {
  return execFileSync(DOCKER[0], [...DOCKER.slice(1), "exec", "-u", "ubuntu", RDP_CONTAINER, "sh", "-c", script], {
    encoding: "utf8",
  }).trim();
}

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

const fs = await page.evaluate(async (connection) => {
  const container = document.createElement("div");
  Object.assign(container.style, { position: "fixed", inset: "0" });
  document.body.append(container);
  const { MolcajeteSession } = await import(new URL("molcajete.js", document.baseURI).href);
  const session = await MolcajeteSession.create(container, { worker: true, binary: true });
  window.__session = session;
  window.__events = [];
  const filesystem = new Promise((resolve, reject) => {
    session.set_event_handler((type, detail) => {
      if (type === "filesystem") resolve(detail);
      else if (type === "error") reject(new Error(detail.message));
      else if (type === "transfer") window.__events.push(detail);
    });
  });
  const ws = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/websocket-tunnel`;
  session.connect(session.tunnel_url(ws, connection));
  return filesystem;
}, CONNECTION);
assert.deepEqual([fs.canDownload, fs.canUpload], [true, true], `filesystem ${JSON.stringify(fs)}`);

// xrdp mounts the drive once the desktop session has started, under the
// name RDPDR announces: a DOS name of 8 bytes with its NUL, so 7 characters.
const mount = `$HOME/thinclient_drives/${DRIVE_NAME.slice(0, 7)}`;
const deadline = Date.now() + 90_000;
for (;;) {
  try {
    remote(`ls ${mount} >/dev/null`);
    break;
  } catch {
    assert.ok(Date.now() < deadline, `the drive did not show up at ${mount}`);
    await new Promise((r) => setTimeout(r, 1000));
  }
}

async function finished(id) {
  const handle = await page.waitForFunction(
    (id) => window.__events.find((e) => e.id === id && e.state !== "active"),
    id,
    { timeout: 120_000, polling: 100 },
  );
  return handle.jsonValue();
}

// ---- Browser to remote ------------------------------------------------------
const data = randomBytes(2 * 1024 * 1024 + 4321);
const local = path.join(os.tmpdir(), "molcajete-drive-e2e.bin");
await writeFile(local, data);
await page.evaluate(() => {
  const input = document.createElement("input");
  input.type = "file";
  input.id = "pick";
  document.body.append(input);
});
await page.setInputFiles("#pick", local);
const uploadId = await page.evaluate(
  ([object]) => window.__session.upload(document.getElementById("pick").files[0], { object, path: "/from-browser.bin" }),
  [fs.object],
);
const upload = await finished(uploadId);
assert.equal(upload.state, "done", `upload ${JSON.stringify(upload)}`);
assert.equal(remote(`sha256sum ${mount}/from-browser.bin`).split(" ")[0], sha256(data), "the remote side reads the upload");
await rm(local);

// ---- Remote to browser ------------------------------------------------------
remote(
  `head -c 1234567 /dev/urandom > ${mount}/draft.tmp && mv ${mount}/draft.tmp ${mount}/from-server.bin` +
    ` && mkdir ${mount}/made-remotely && printf 'nested' > ${mount}/made-remotely/note.txt`,
);
const serverHash = remote(`sha256sum ${mount}/from-server.bin`).split(" ")[0];

const top = await page.evaluate(([object]) => window.__session.list(object, "/"), [fs.object]);
const names = top.map((e) => `${e.name}:${e.type}`).sort();
assert.deepEqual(
  names.filter((n) => !n.startsWith("from-browser")),
  ["from-server.bin:file", "made-remotely:directory"],
  JSON.stringify(top),
);
assert.equal(top.find((e) => e.name === "from-server.bin").size, 1234567);
const nested = await page.evaluate(([object]) => window.__session.list(object, "/made-remotely"), [fs.object]);
assert.deepEqual(nested.map((e) => [e.name, e.size]), [["note.txt", 6]]);

// Through a Blob saved as a download, not a save dialog.
await page.evaluate(() => delete window.showSaveFilePicker);
const [download] = await Promise.all([
  page.waitForEvent("download"),
  page.evaluate(([object]) => window.__session.download(object, "/from-server.bin"), [fs.object]),
]);
const saved = path.join(os.tmpdir(), "molcajete-drive-e2e-download.bin");
await download.saveAs(saved);
assert.equal(sha256(await readFile(saved)), serverHash, "the browser downloads what the remote side wrote");
await rm(saved);

console.log("drive: upload read remotely; remote write, rename and folder listed and downloaded ok");
await context.close();
await rm(profile, { recursive: true, force: true });

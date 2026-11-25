// Minimal UI shell around the wasm client: connection list, session view,
// side menu (Ctrl+Alt+Shift), clipboard and credential prompts.
import { MolcajeteSession } from "./molcajete.js";

// `?main` runs the client on the main thread instead of a worker.
const forceMainThread = new URLSearchParams(location.search).has("main");

const $ = (id) => document.getElementById(id);

const views = { home: $("home"), session: $("session") };
let client = null;
let currentConnection = null;
let connectionNames = new Map();

function show(view) {
  for (const [name, el] of Object.entries(views)) el.hidden = name !== view;
}

function setOverlay({ title, detail = "", busy = false, canReconnect = false } = {}) {
  const overlay = $("overlay");
  if (!title) {
    overlay.hidden = true;
    return;
  }
  overlay.hidden = false;
  $("overlay-title").textContent = title;
  $("overlay-detail").textContent = detail;
  $("overlay-spinner").hidden = !busy;
  $("overlay-reconnect").hidden = !canReconnect;
}

async function loadConnections() {
  const list = $("connections");
  list.replaceChildren();
  try {
    const response = await fetch("api/connections");
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const connections = await response.json();
    connectionNames = new Map(connections.map((c) => [c.id, c.name]));
    $("home-message").textContent = connections.length ? "" : "No connections are configured.";
    for (const c of connections) {
      const item = document.createElement("li");
      const link = document.createElement("a");
      link.href = `#/connect/${encodeURIComponent(c.id)}`;
      const badge = document.createElement("span");
      badge.className = "protocol";
      badge.textContent = c.protocol;
      const name = document.createElement("span");
      name.textContent = c.name;
      link.append(badge, name);
      item.append(link);
      list.append(item);
    }
  } catch (err) {
    $("home-message").textContent = `Could not load connections (${err.message}).`;
  }
}

function tunnelBase() {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  const path = location.pathname.replace(/[^/]*$/, "");
  return `${scheme}//${location.host}${path}websocket-tunnel`;
}

function formatStats(s) {
  const kb = (s.bytesPerSecond / 1024).toFixed(1);
  return [
    `renderer  WebGL2 (${client?.mode ?? "?"} thread)`,
    `display   ${s.displayWidth}×${s.displayHeight}`,
    `frames    ${s.framesPerSecond.toFixed(1)}/s`,
    `paints    ${s.compositesPerSecond.toFixed(1)}/s`,
    `draws     ${s.drawCallsPerSecond.toFixed(0)}/s`,
    `instr     ${s.instructionsPerSecond.toFixed(0)}/s`,
    `recv      ${kb} KiB/s`,
    `lag       ${s.frameLagMs.toFixed(1)} ms`,
    `rtt       ${s.rttMs == null ? "-" : s.rttMs.toFixed(0) + " ms"}`,
    `queued    ${s.queuedOps}`,
  ].join("\n");
}

function askRequired(names) {
  const form = $("required");
  const fields = $("required-fields");
  fields.replaceChildren();
  for (const name of names) {
    const label = document.createElement("label");
    label.className = "field";
    const caption = document.createElement("span");
    caption.textContent = name;
    const input = document.createElement("input");
    input.name = name;
    input.type = /password|passphrase/.test(name) ? "password" : "text";
    input.autocomplete = "off";
    label.append(caption, input);
    fields.append(label);
  }
  form.hidden = false;
  fields.querySelector("input")?.focus();
  form.onsubmit = (event) => {
    event.preventDefault();
    for (const input of fields.querySelectorAll("input")) client.send_argv(input.name, input.value);
    form.hidden = true;
    client.focus();
  };
}

function handleEvent(type, detail) {
  switch (type) {
    case "state":
      if (detail === "connecting" || detail === "waiting") {
        setOverlay({ title: "Connecting…", detail: connectionNames.get(currentConnection) ?? "", busy: true });
      } else if (detail === "connected") {
        setOverlay();
      } else if (detail === "disconnected" && $("overlay").hidden) {
        setOverlay({ title: "Disconnected", canReconnect: true });
      }
      break;
    case "error":
      setOverlay({ title: "Connection error", detail: `${detail.message} (${detail.name})`, canReconnect: true });
      break;
    case "name":
      document.title = `${detail} · molcajete`;
      $("menu-title").textContent = detail;
      break;
    case "clipboard":
      $("clipboard").value = detail;
      navigator.clipboard?.writeText(detail).catch(() => {});
      break;
    case "required":
      askRequired(detail);
      break;
    case "unstable":
      document.body.classList.toggle("unstable", detail);
      break;
    case "stats":
      if (!$("stats").hidden) $("stats").textContent = formatStats(detail);
      break;
  }
}

async function startSession(id) {
  currentConnection = id;
  show("session");
  $("menu").hidden = true;
  $("required").hidden = true;
  if (!client) {
    client = await MolcajeteSession.create($("display"), { worker: forceMainThread ? false : undefined });
    client.set_event_handler(handleEvent);
    client.set_scale_mode($("scale-mode").value);
  }
  setOverlay({ title: "Connecting…", busy: true });
  client.connect(client.tunnel_url(tunnelBase(), id));
}

function endSession() {
  client?.disconnect();
  currentConnection = null;
  document.title = "molcajete";
  show("home");
  loadConnections();
}

function route() {
  const match = location.hash.match(/^#\/connect\/(.+)$/);
  if (match) startSession(decodeURIComponent(match[1]));
  else endSession();
}

function toggleMenu(open = $("menu").hidden) {
  $("menu").hidden = !open;
  if (open) {
    client?.release_keys();
    $("clipboard").focus();
  } else {
    client?.focus();
  }
}

// Ctrl+Alt+Shift toggles the menu (same shortcut as Apache Guacamole).
const held = new Set();
document.addEventListener("keydown", (e) => {
  held.add(e.key);
  if (held.has("Control") && held.has("Alt") && held.has("Shift") && !views.session.hidden) {
    held.clear();
    toggleMenu();
  }
}, true);
document.addEventListener("keyup", (e) => held.delete(e.key), true);
window.addEventListener("blur", () => held.clear());

$("clipboard").addEventListener("input", () => client?.send_clipboard($("clipboard").value));
$("scale-mode").addEventListener("change", (e) => client?.set_scale_mode(e.target.value));
$("show-stats").addEventListener("change", (e) => ($("stats").hidden = !e.target.checked));
$("cad").addEventListener("click", () => client?.send_ctrl_alt_del());
$("disconnect").addEventListener("click", () => (location.hash = ""));
$("overlay-home").addEventListener("click", () => (location.hash = ""));
$("overlay-reconnect").addEventListener("click", () => currentConnection && startSession(currentConnection));
window.addEventListener("hashchange", route);

route();

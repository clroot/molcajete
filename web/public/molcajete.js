// Page-side API of the molcajete client.
//
// By default the client runs in a dedicated worker (molcajete-worker.js) and
// renders into an OffscreenCanvas, so protocol processing, image decoding
// and GPU uploads never block the page. Browsers without OffscreenCanvas
// fall back to running the same wasm client on the main thread.
//
//   const session = await MolcajeteSession.create(container);
//   session.set_event_handler((type, detail) => { ... });
//   session.connect(session.tunnel_url("wss://host/websocket-tunnel", "my-connection"));
import init, { MolcajeteClient } from "./pkg/molcajete_client.js";

const supportsWorker = () =>
  typeof OffscreenCanvas !== "undefined" &&
  typeof HTMLCanvasElement.prototype.transferControlToOffscreen === "function" &&
  typeof OffscreenCanvas.prototype.getContext === "function";

/** Builds a CSS cursor value from un-premultiplied RGBA pixels. */
function cssCursor({ hotspotX, hotspotY, width, height, rgba }) {
  if (!width || !height || !rgba.some((v, i) => i % 4 === 3 && v !== 0)) return "none";
  const canvas = document.createElement("canvas");
  canvas.width = width;
  canvas.height = height;
  canvas.getContext("2d").putImageData(new ImageData(new Uint8ClampedArray(rgba), width, height), 0, 0);
  return `url(${canvas.toDataURL()}) ${hotspotX} ${hotspotY}, auto`;
}

/** Video mimetypes (molcajete `video` streams) this browser can decode. */
async function supportedVideo() {
  if (typeof VideoDecoder === "undefined") return [];
  const candidates = [
    ["video/av1", "av01.0.08M.08"],
    ["video/h265", "hvc1.1.6.L120.90"],
    ["video/h264", "avc1.64002A"],
  ];
  const supported = [];
  for (const [mimetype, codec] of candidates) {
    try {
      if ((await VideoDecoder.isConfigSupported({ codec, optimizeForLatency: true })).supported) supported.push(mimetype);
    } catch {
      // Unknown codec string: not supported.
    }
  }
  return supported;
}

/** Plays raw PCM (`audio/L8`, `audio/L16`) forwarded by the worker. */
class PcmPlayer {
  // Packets scheduled further ahead are dropped to bound latency.
  static MAX_LATENCY = 0.3;
  static MIN_LATENCY = 0.05;

  constructor(context, { rate, channels, bytesPerSample }) {
    Object.assign(this, { context, rate, channels, bytesPerSample, next: 0 });
  }

  play(bytes) {
    const ctx = this.context;
    if (ctx.state !== "running") return;
    const frameBytes = this.bytesPerSample * this.channels;
    const frames = Math.floor(bytes.byteLength / frameBytes);
    if (!frames) return;
    const now = ctx.currentTime;
    if (this.next < now + PcmPlayer.MIN_LATENCY / 2) this.next = now + PcmPlayer.MIN_LATENCY;
    if (this.next > now + PcmPlayer.MAX_LATENCY) return;
    const buffer = ctx.createBuffer(this.channels, frames, this.rate);
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    for (let ch = 0; ch < this.channels; ch++) {
      const out = buffer.getChannelData(ch);
      for (let i = 0; i < frames; i++) {
        const offset = (i * this.channels + ch) * this.bytesPerSample;
        out[i] = this.bytesPerSample === 2 ? view.getInt16(offset, true) / 32768 : view.getInt8(offset) / 128;
      }
    }
    const source = ctx.createBufferSource();
    source.buffer = buffer;
    source.connect(ctx.destination);
    source.start(this.next);
    this.next += frames / this.rate;
  }
}

/** macOS switches input sources with Caps Lock, so it toggles the remote one there. */
function isMac() {
  const platform = navigator.userAgentData?.platform ?? navigator.platform ?? "";
  return /mac/i.test(platform);
}

function timezone() {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone;
  } catch {
    return undefined;
  }
}

export class MolcajeteSession {
  /**
   * @param {HTMLElement} container element the display is placed in
   * @param {{worker?: boolean, binary?: boolean, zstd?: boolean}} options
   *   `worker: false` forces main-thread mode; `binary: false` keeps the
   *   tunnel on the Guacamole text protocol instead of offering the gateway
   *   raw image, video and audio data; `zstd: false` does not announce the
   *   zstd image type, so that lossless updates arrive as PNG
   */
  static async create(container, options = {}) {
    const useWorker = options.worker ?? supportsWorker();
    const session = new MolcajeteSession(container, useWorker);
    session.#video = await supportedVideo();
    if (useWorker) await session.#startWorker();
    else await session.#startMain();
    if (options.binary === false) session.call("set_binary_tunnel", false);
    if (options.zstd === false) {
      session.#zstd = false;
      session.call("set_zstd_images", false);
    }
    return session;
  }

  #container;
  #handler = null;
  #worker = null;
  #client = null; // main-thread mode
  #canvas = null;
  #sink = null;
  #listeners = [];
  #observer = null;
  #requests = new Map();
  #nextRequest = 1;
  #video = [];
  /** Whether the zstd image type is announced (see `create`). */
  #zstd = true;
  #audioContext = null;
  #players = new Map();
  #remoteComposition = false;

  /** "worker" or "main". */
  mode;

  /** Video mimetypes offered to the server. */
  get video() {
    return [...this.#video];
  }

  constructor(container, useWorker) {
    this.#container = container;
    this.mode = useWorker ? "worker" : "main";
  }

  async #startMain() {
    await init();
    this.#client = new MolcajeteClient(this.#container);
    this.#client.set_event_handler((type, detail) => this.#handler?.(type, detail));
  }

  async #startWorker() {
    const canvas = document.createElement("canvas");
    canvas.className = "guac-display";
    Object.assign(canvas.style, { display: "block", touchAction: "none", outline: "none" });
    const sink = document.createElement("textarea");
    sink.className = "guac-input-sink";
    for (const attr of ["autocapitalize", "autocomplete", "autocorrect"]) sink.setAttribute(attr, "off");
    sink.spellcheck = false;
    sink.setAttribute("aria-label", "Remote keyboard input");
    Object.assign(sink.style, {
      position: "absolute", left: "0", top: "0", width: "1px", height: "1px", opacity: "0",
      border: "0", padding: "0", resize: "none", overflow: "hidden", pointerEvents: "none",
    });
    this.#container.append(canvas, sink);
    this.#canvas = canvas;
    this.#sink = sink;

    const worker = new Worker(new URL("./molcajete-worker.js", import.meta.url), { type: "module" });
    this.#worker = worker;
    const ready = new Promise((resolve) => {
      worker.onmessage = ({ data }) => {
        if (data.type === "ready") {
          // The worker says whether its client decodes zstd images. A
          // worker of an earlier version does not say, and must not be
          // sent a type it would drop.
          if (data.zstd !== true) this.#zstd = false;
          resolve();
        }
        else if (data.type === "event") this.#onEvent(data.event, data.detail);
        else if (data.type === "result") {
          const request = this.#requests.get(data.id);
          this.#requests.delete(data.id);
          if (data.error) request?.reject(new Error(data.error));
          else request?.resolve(data.result);
        }
      };
    });
    const rect = this.#container.getBoundingClientRect();
    const offscreen = canvas.transferControlToOffscreen();
    worker.postMessage(
      {
        type: "init",
        canvas: offscreen,
        width: rect.width,
        height: rect.height,
        dpr: devicePixelRatio || 1,
        audio: typeof AudioContext !== "undefined",
      },
      [offscreen],
    );
    this.#installInput();
    await ready;
  }

  #onEvent(type, detail) {
    if (type === "layout") {
      this.#canvas.style.width = `${detail.width}px`;
      this.#canvas.style.height = `${detail.height}px`;
      return;
    }
    if (type === "cursor") {
      this.#canvas.style.cursor = cssCursor(detail);
      return;
    }
    // Audio forwarded by the worker is played here and not reported.
    if (type === "audio-start") {
      this.#audioContext ??= new AudioContext();
      this.#players.set(detail.stream, new PcmPlayer(this.#audioContext, detail));
      return;
    }
    if (type === "audio-data") {
      this.#players.get(detail.stream)?.play(detail.data);
      return;
    }
    if (type === "audio-end") {
      this.#players.delete(detail.stream);
      return;
    }
    if (type === "state" && detail === "connected") this.focus();
    this.#handler?.(type, detail);
  }

  /** Browsers only start audio after a user gesture. */
  #resumeAudio() {
    if (this.#audioContext?.state === "suspended") this.#audioContext.resume();
  }

  #post(method, ...args) {
    this.#worker?.postMessage({ type: "call", method, args });
  }

  #listen(target, type, handler, options = {}) {
    target.addEventListener(type, handler, options);
    this.#listeners.push(() => target.removeEventListener(type, handler, options));
  }

  #installInput() {
    const canvas = this.#canvas;
    const sink = this.#sink;
    const position = (e) => {
      const r = canvas.getBoundingClientRect();
      return [e.clientX - r.left, e.clientY - r.top];
    };
    const pointer = (e) => this.#post("pointer", ...position(e), e.buttons);
    this.#listen(canvas, "pointermove", pointer, { passive: true });
    this.#listen(canvas, "pointerup", pointer, { passive: true });
    this.#listen(canvas, "pointercancel", pointer, { passive: true });
    this.#listen(canvas, "pointerdown", (e) => {
      e.preventDefault();
      this.#resumeAudio();
      canvas.setPointerCapture(e.pointerId);
      this.focus();
      pointer(e);
    });
    this.#listen(canvas, "wheel", (e) => {
      e.preventDefault();
      this.#post("wheel", e.deltaY, e.deltaMode);
    }, { passive: false });
    this.#listen(canvas, "contextmenu", (e) => e.preventDefault());

    // Keys being composed by an IME (or dead keys) are left to the browser;
    // the composed text is typed when the composition ends. With remote
    // composition every key is forwarded and the remote IME composes.
    this.#listen(sink, "keydown", (e) => {
      this.#resumeAudio();
      const composing = e.isComposing || e.key === "Process";
      if (e.key === "Dead" || (composing && !this.#remoteComposition)) return;
      e.preventDefault();
      this.#post("key_down", e.key, e.code, e.location, composing, e.getModifierState("CapsLock"));
    });
    this.#listen(sink, "keyup", (e) => {
      if ((e.isComposing || e.key === "Process") && !this.#remoteComposition) return;
      e.preventDefault();
      this.#post("key_up", e.key, e.code, e.location, e.getModifierState("CapsLock"));
    });
    this.#listen(sink, "compositionend", (e) => {
      if (e.data && !this.#remoteComposition) this.#post("type_text", e.data);
      sink.value = "";
    });
    this.#listen(sink, "input", (e) => {
      if (e.isComposing) return;
      if (e.inputType === "insertText" && e.data && !this.#remoteComposition) this.#post("type_text", e.data);
      if (e.inputType !== "insertCompositionText") sink.value = "";
    });
    this.#listen(sink, "blur", () => this.#post("release_keys"));

    this.#observer = new ResizeObserver(() => {
      const r = this.#container.getBoundingClientRect();
      this.#post("set_viewport", r.width, r.height, devicePixelRatio || 1);
    });
    this.#observer.observe(this.#container);
  }

  /** Calls a client method in the worker and resolves with its result. */
  request(method, ...args) {
    if (this.#client) return Promise.resolve(this.#client[method](...args));
    const id = this.#nextRequest++;
    return new Promise((resolve, reject) => {
      this.#requests.set(id, { resolve, reject });
      this.#worker.postMessage({ type: "call", method, args, id });
    });
  }

  /** Calls a client method without waiting for a result. */
  call(method, ...args) {
    if (this.#client) this.#client[method](...args);
    else this.#post(method, ...args);
  }

  set_event_handler(handler) {
    this.#handler = handler;
  }

  /** Tunnel URL with display size, DPI, timezone and supported formats. */
  tunnel_url(base, connection) {
    const video = this.#video.map((m) => `&video=${encodeURIComponent(m)}`).join("");
    if (this.#client) return this.#client.tunnel_url(base, connection) + video;
    const rect = this.#container.getBoundingClientRect();
    const dpr = devicePixelRatio || 1;
    const params = new URLSearchParams({
      connection,
      width: String(Math.max(64, Math.round(rect.width * dpr))),
      height: String(Math.max(64, Math.round(rect.height * dpr))),
      dpi: String(Math.round(96 * dpr)),
    });
    for (const image of ["image/png", "image/jpeg", "image/webp"]) params.append("image", image);
    // Decoded by the wasm client itself (molcajete_protocol::zimage).
    if (this.#zstd) params.append("image", "image/x-molcajete-zstd-v1");
    // Audio is decoded in the worker and played on this page.
    if (typeof AudioContext !== "undefined") {
      for (const audio of ["audio/L8", "audio/L16"]) params.append("audio", audio);
    }
    for (const mimetype of this.#video) params.append("video", mimetype);
    const tz = timezone();
    if (tz) params.set("timezone", tz);
    return `${base}${base.includes("?") ? "&" : "?"}${params}`;
  }

  connect(url) {
    this.call("connect", url);
  }

  disconnect() {
    this.call("disconnect");
  }

  focus() {
    if (this.#client) this.#client.focus();
    else this.#sink?.focus();
  }

  /**
   * Lets the remote host's IME compose text (Hangul, …): keys are sent as
   * they are pressed, by physical position, instead of the text composed by
   * the local IME. On macOS, Caps Lock becomes the Hangul/English key.
   */
  set_remote_composition(enabled) {
    this.#remoteComposition = enabled;
    if (this.#sink) {
      // No input method starts on a read-only field; it still gets key events.
      this.#sink.readOnly = enabled;
      this.#sink.value = "";
    }
    this.call("set_remote_composition", enabled, isMac());
  }

  send_clipboard(text) {
    this.call("send_clipboard", text);
  }

  send_argv(name, value) {
    this.call("send_argv", name, value);
  }

  send_ctrl_alt_del() {
    this.call("send_ctrl_alt_del");
  }

  set_scale_mode(mode) {
    this.call("set_scale_mode", mode);
  }

  set_auto_resize(enabled) {
    this.call("set_auto_resize", enabled);
  }

  release_keys() {
    this.call("release_keys");
  }

  /** Disconnects and releases all resources. */
  free() {
    this.disconnect();
    for (const remove of this.#listeners.splice(0)) remove();
    this.#observer?.disconnect();
    this.#worker?.terminate();
    this.#canvas?.remove();
    this.#sink?.remove();
    this.#client?.free();
    this.#client = null;
  }
}

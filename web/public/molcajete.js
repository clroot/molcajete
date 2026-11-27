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
import { RecordingPlayer } from "./molcajete-player.js";
import { FileTransfers } from "./molcajete-files.js";

/** Saves a Blob the browser holds as a download. */
function saveBlob(blob, name) {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  link.rel = "noopener";
  link.style.display = "none";
  document.body.append(link);
  link.click();
  link.remove();
  // The download has started from the URL; it may go once it did.
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
}

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

/**
 * The modifier flags of a key event as the bits `key_down` and `key_up`
 * take, which keep Ctrl, Alt, Shift and Meta in step with the keyboard.
 */
function modifierFlags(e) {
  return (
    (e.ctrlKey ? 1 : 0) |
    (e.altKey ? 2 : 0) |
    (e.shiftKey ? 4 : 0) |
    (e.metaKey ? 8 : 0) |
    (e.getModifierState("AltGraph") ? 16 : 0)
  );
}

/** Link schemes the page opens; the daemon offers no others either. */
function isOpenableLink(url) {
  return typeof url === "string" && /^(https?:\/\/|mailto:)./i.test(url);
}

/**
 * Writes text to the local clipboard from within a key or pointer event,
 * which counts as a user gesture. Resolves to whether it worked.
 */
function writeClipboard(text) {
  if (navigator.clipboard?.writeText) {
    try {
      return navigator.clipboard.writeText(text).then(() => true, () => false);
    } catch {
      // Fall through to the copy command.
    }
  }
  // Pages served over plain HTTP have no Clipboard API.
  const onCopy = (e) => {
    e.clipboardData.setData("text/plain", text);
    e.preventDefault();
  };
  document.addEventListener("copy", onCopy, { capture: true });
  let copied = false;
  try {
    copied = document.execCommand("copy");
  } catch {
    copied = false;
  } finally {
    document.removeEventListener("copy", onCopy, { capture: true });
  }
  return Promise.resolve(copied);
}

/** How long a finger rests before it selects instead of scrolling. */
const LONG_PRESS_MS = 500;
/** How far a finger may wander (CSS pixels) and still be a tap or a long press. */
const TOUCH_SLOP = 10;
/** How much two fingers move apart or together for one step of text size. */
const PINCH_STEP = 1.15;

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
   *   zstd image type, so that lossless updates arrive as PNG;
   *   `playback: true` makes a player of session recordings (see `playback`),
   *   which takes no input; `audioContext` is the context audio streams play
   *   in, best created during the click that opened the session so that the
   *   browser lets it run (one is made at the first stream otherwise)
   */
  static async create(container, options = {}) {
    const useWorker = options.worker ?? supportsWorker();
    const session = new MolcajeteSession(container, useWorker);
    session.#playback = options.playback === true;
    session.#audioContext = options.audioContext ?? null;
    session.#video = await supportedVideo();
    if (useWorker) await session.#startWorker();
    else await session.#startMain();
    session.#installTerminalShortcuts();
    session.#installTerminalTouch();
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
  /** Where audio streams play, in either mode (see `#onAudio`). */
  #audioContext = null;
  #players = new Map();
  /** What the last `audio` event said: a stream is open but the browser holds the sound back. */
  #audioBlocked = false;
  #remoteComposition = false;
  /** Terminal copy, paste and link shortcuts are on (see `set_terminal_shortcuts`). */
  #terminal = false;
  /** Text the terminal selected last, until it says nothing is selected. */
  #selection = null;
  /** Target of the link under the pointer, as the terminal reported it. */
  #link = null;
  /** Pointer whose events are kept from the remote side after it opened a link. */
  #heldPointer = null;
  #playback = false;
  #player = null; // main-thread mode
  #files = null; // main-thread mode
  #nextTransfer = 1;

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
    // Audio plays on this page in both modes, in one AudioContext.
    this.#client.set_audio_forwarding(typeof AudioContext !== "undefined");
    this.#files = new FileTransfers(this.#client, (type, detail) => this.#onFileEvent(type, detail));
    this.#client.set_event_handler((type, detail) => {
      if (this.#files.onEvent(type, detail)) return;
      if (this.#onAudio(type, detail)) return;
      this.#track(type, detail);
      this.#handler?.(type, detail);
    });
    this.#installAudioUnlock();
    if (typeof this.#client.set_page_visible === "function") this.#watchVisibility();
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
    let visibility = false;
    const ready = new Promise((resolve) => {
      worker.onmessage = ({ data }) => {
        if (data.type === "ready") {
          // The worker says whether its client decodes zstd images. A
          // worker of an earlier version does not say, and must not be
          // sent a type it would drop.
          if (data.zstd !== true) this.#zstd = false;
          // Likewise whether it can be told that the page is hidden.
          visibility = data.visibility === true;
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
    this.#installAudioUnlock();
    await ready;
    if (visibility) this.#watchVisibility();
  }

  #onEvent(type, detail) {
    // A recording shows the recorded pointer itself.
    if (this.#playback && type === "cursor") return;
    if (type === "layout") {
      this.#canvas.style.width = `${detail.width}px`;
      this.#canvas.style.height = `${detail.height}px`;
      return;
    }
    if (type === "cursor") {
      this.#canvas.style.cursor = cssCursor(detail);
      return;
    }
    if (this.#onAudio(type, detail)) return;
    if (type === "transfer" || type === "filesystem" || type === "filesystem-removed") {
      this.#onFileEvent(type, detail);
      return;
    }
    if (type === "state" && detail === "connected") this.focus();
    this.#track(type, detail);
    this.#handler?.(type, detail);
  }

  /** Keeps what the terminal shortcuts need from the client's events. */
  #track(type, detail) {
    if (type === "clipboard") this.#selection = detail;
    else if (type === "selection" && !detail) this.#selection = null;
    else if (type === "link") this.#link = detail;
    else if (type === "state" && detail === "connecting") {
      this.#selection = null;
      this.#link = null;
    }
  }

  /**
   * Copy, paste and link shortcuts of terminals. They listen on the
   * container in the capture phase, ahead of the client's own listeners on
   * the canvas and the input sink (in either mode), and keep the keys they
   * handle from the remote side.
   *
   * - Copy (⌘C on macOS, Ctrl+Shift+C elsewhere) writes the last selection
   *   to the local clipboard within the key event, so it counts as a user
   *   gesture. Without a selection, Ctrl+Shift+C goes to the remote side.
   * - Paste (⌘V, Ctrl+Shift+V) is left to the browser, which then fires
   *   `paste`; its text is sent, followed by the terminal's paste keys.
   * - ⌘-click (Ctrl-click elsewhere) on a link the terminal reported opens
   *   it within the click, so popup blockers let it through.
   */
  #installTerminalShortcuts() {
    const container = this.#container;
    const mac = isMac();
    const isShortcut = (e, letter) =>
      !e.altKey &&
      (mac ? e.metaKey && !e.ctrlKey && !e.shiftKey : e.ctrlKey && e.shiftKey && !e.metaKey) &&
      (e.key.toLowerCase() === letter || e.code === `Key${letter.toUpperCase()}`);
    const capture = { capture: true };

    this.#listen(container, "keydown", (e) => {
      if (!this.#terminal || e.isComposing) return;
      if (isShortcut(e, "c")) {
        const text = this.#selection;
        if (!text && !mac) return;
        e.preventDefault();
        e.stopPropagation();
        if (text) writeClipboard(text).then((ok) => this.#handler?.("copy", { ok }));
      } else if (isShortcut(e, "v")) {
        // Not prevented: the browser pastes, firing `paste` below.
        e.stopPropagation();
      }
    }, capture);
    this.#listen(container, "paste", (e) => {
      if (!this.#terminal) return;
      e.preventDefault();
      e.stopPropagation();
      const text = e.clipboardData?.getData("text/plain");
      if (text) this.call("paste_text", text);
    }, capture);

    this.#listen(container, "pointerdown", (e) => {
      const opens = mac ? e.metaKey : e.ctrlKey;
      if (!this.#terminal || e.button !== 0 || !opens || !isOpenableLink(this.#link)) return;
      e.preventDefault();
      e.stopPropagation();
      this.#heldPointer = e.pointerId;
      this.focus();
      window.open(this.#link, "_blank", "noopener,noreferrer");
    }, capture);
    for (const type of ["pointermove", "pointerup", "pointercancel"]) {
      this.#listen(container, type, (e) => {
        if (e.pointerId !== this.#heldPointer) return;
        // The press never reached the remote side; neither does the rest.
        e.stopPropagation();
        if (type !== "pointermove") this.#heldPointer = null;
      }, capture);
    }
  }

  /**
   * Touch in terminals. A finger sent as a mouse with its button held can
   * only select text, and a phone has no wheel to scroll with, so the
   * gestures are told apart here, ahead of the client's own listeners (in
   * either mode), and reach the remote side as what a mouse would do:
   *
   * - A drag up or down scrolls: its distance goes out as wheel movement,
   *   so the history scrolls, and so do full-screen applications.
   * - A finger that rests first selects: the button goes down where it
   *   rested and up where it is lifted.
   * - A tap focuses the input, which opens the on-screen keyboard, and
   *   clicks.
   * - Two fingers moving apart or together are reported as `zoom` events
   *   (`{delta: 1}` or `{delta: -1}`) for the page to change the text size.
   *
   * A mouse and a pen are not touched by this, nor is any pointer outside
   * terminals.
   */
  #installTerminalTouch() {
    if (this.#playback) return;
    const container = this.#container;
    const capture = { capture: true };
    /** Where each finger on the display is, by pointer id. */
    const touches = new Map();
    /**
     * What the fingers are doing: the first one is `pending` until it moves
     * (`scroll`) or rests (`select`); two are a `pinch`; `over` once that
     * ended while a finger is still down.
     */
    let gesture = null;

    const position = (e) => {
      const canvas = this.#canvas ?? container.querySelector("canvas") ?? container;
      const r = canvas.getBoundingClientRect();
      return { x: e.clientX - r.left, y: e.clientY - r.top };
    };
    const pointer = ({ x, y }, buttons) => this.call("pointer", x, y, buttons);
    const spread = () => {
      const [a, b] = [...touches.values()];
      return Math.hypot(a.x - b.x, a.y - b.y);
    };
    /** Ends what the first finger was doing; the button does not stay down. */
    const stop = () => {
      clearTimeout(gesture?.timer);
      if (gesture?.kind === "select") pointer(gesture.at, 0);
    };
    const handles = (e) => this.#terminal && e.pointerType === "touch";

    this.#listen(container, "pointerdown", (e) => {
      if (!handles(e)) return;
      e.preventDefault();
      e.stopPropagation();
      this.#resumeAudio();
      const at = position(e);
      touches.set(e.pointerId, at);
      if (touches.size === 1) {
        const first = { kind: "pending", id: e.pointerId, start: at, at, timer: 0 };
        first.timer = setTimeout(() => {
          if (gesture !== first || first.kind !== "pending") return;
          first.kind = "select";
          navigator.vibrate?.(10);
          pointer(first.start, 1);
        }, LONG_PRESS_MS);
        gesture = first;
      } else if (touches.size === 2) {
        stop();
        gesture = { kind: "pinch", spread: spread() };
      }
    }, capture);

    this.#listen(container, "pointermove", (e) => {
      if (!handles(e) || !touches.has(e.pointerId)) return;
      e.stopPropagation();
      const at = position(e);
      touches.set(e.pointerId, at);
      if (!gesture) return;
      if (gesture.kind === "pinch") {
        if (touches.size < 2 || !gesture.spread) return;
        const ratio = spread() / gesture.spread;
        if (ratio < PINCH_STEP && ratio > 1 / PINCH_STEP) return;
        gesture.spread = spread();
        this.#handler?.("zoom", { delta: ratio > 1 ? 1 : -1 });
        return;
      }
      if (e.pointerId !== gesture.id) return;
      const { start } = gesture;
      if (gesture.kind === "pending" && Math.hypot(at.x - start.x, at.y - start.y) > TOUCH_SLOP) {
        clearTimeout(gesture.timer);
        gesture.kind = "scroll";
        // Applications that read the mouse are told where the wheel turns.
        pointer(start, 0);
      }
      // The text follows the finger: moving it up shows what comes after.
      if (gesture.kind === "scroll") this.call("wheel", gesture.at.y - at.y, 0);
      else if (gesture.kind === "select") pointer(at, 1);
      gesture.at = at;
    }, capture);

    for (const type of ["pointerup", "pointercancel"]) {
      this.#listen(container, type, (e) => {
        if (!handles(e) || !touches.has(e.pointerId)) return;
        e.stopPropagation();
        touches.delete(e.pointerId);
        const ended = gesture;
        if (!ended) return;
        const isFirst = e.pointerId === ended.id;
        if (ended.kind !== "pinch" && ended.kind !== "over" && !isFirst) return;
        stop();
        // A finger left over from a pinch does not start to scroll.
        gesture = touches.size > 0 ? { kind: "over" } : null;
        if (ended.kind === "pending" && type === "pointerup") {
          this.focus();
          pointer(ended.at, 1);
          pointer(ended.at, 0);
        }
      }, capture);
    }
  }

  /**
   * Plays the audio streams the client forwards, and reports with `audio`
   * events whether one is open while the browser holds the sound back.
   * Returns whether the event was about audio.
   */
  #onAudio(type, detail) {
    if (type === "audio-start") {
      if (!this.#audioContext) {
        this.#audioContext = new AudioContext();
        this.#watchAudioContext();
      }
      this.#players.set(detail.stream, new PcmPlayer(this.#audioContext, detail));
    } else if (type === "audio-data") {
      this.#players.get(detail.stream)?.play(detail.data);
    } else if (type === "audio-end") {
      this.#players.delete(detail.stream);
    } else {
      // A tunnel that closes ends its streams without `audio-end`.
      if (type === "state" && (detail === "connecting" || detail === "disconnected")) this.#players.clear();
      if (type === "state") this.#reportAudio();
      return false;
    }
    this.#reportAudio();
    return true;
  }

  #reportAudio() {
    const blocked = this.#players.size > 0 && this.#audioContext?.state !== "running";
    if (blocked === this.#audioBlocked) return;
    this.#audioBlocked = blocked;
    this.#handler?.("audio", { blocked });
  }

  #watchAudioContext() {
    this.#listen(this.#audioContext, "statechange", () => this.#reportAudio());
  }

  /**
   * Browsers only let audio play after a user gesture: any press, click,
   * touch or key on the page resumes the context, even before a stream
   * starts, so that the first sound is not lost.
   */
  #installAudioUnlock() {
    if (this.#audioContext) this.#watchAudioContext();
    const resume = () => this.#resumeAudio();
    for (const type of ["pointerdown", "pointerup", "touchend", "click", "keydown"]) {
      this.#listen(window, type, resume, { capture: true, passive: true });
    }
  }

  #resumeAudio() {
    if (this.#audioContext?.state === "suspended") this.#audioContext.resume().catch(() => {});
  }

  #post(method, ...args) {
    this.#worker?.postMessage({ type: "call", method, args });
  }

  #listen(target, type, handler, options = {}) {
    target.addEventListener(type, handler, options);
    this.#listeners.push(() => target.removeEventListener(type, handler, options));
  }

  #installInput() {
    this.#observeSize();
    if (this.#playback) return;
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
      this.#post("key_down", e.key, e.code, e.location, composing, e.getModifierState("CapsLock"), modifierFlags(e));
    });
    this.#listen(sink, "keyup", (e) => {
      if ((e.isComposing || e.key === "Process") && !this.#remoteComposition) return;
      e.preventDefault();
      this.#post("key_up", e.key, e.code, e.location, e.getModifierState("CapsLock"), modifierFlags(e));
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
  }

  /**
   * Tells the client while the page is hidden: the browser may suspend it
   * then, and that time is not the client's frame lag.
   */
  #watchVisibility() {
    const report = () => this.call("set_page_visible", document.visibilityState !== "hidden");
    this.#listen(document, "visibilitychange", report);
    if (document.visibilityState === "hidden") report();
  }

  #observeSize() {
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

  /**
   * Turns on the copy, paste and link shortcuts of terminals (SSH, telnet,
   * Kubernetes). Other protocols get those keys and clicks as they are.
   */
  set_terminal_shortcuts(enabled) {
    this.#terminal = enabled;
    this.#heldPointer = null;
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

  /**
   * Presses and releases a key the page offers as a button, because an
   * on-screen keyboard has no Tab, Esc or arrow keys. It goes out with the
   * modifiers held by `hold_modifier`.
   */
  press_key(keysym) {
    this.call("press_key", keysym);
  }

  /**
   * Holds `"ctrl"` or `"alt"` as if its key were down: `"once"` until the
   * next key has gone out, from a button or from the keyboard, `"locked"`
   * until it is turned `"off"`. Every change, the release after a key
   * included, arrives as a `modifiers` event (`{ctrl, alt}`).
   */
  hold_modifier(name, hold) {
    this.call("hold_modifier", name, hold);
  }

  /** Pastes text into a terminal, as its paste shortcut does. */
  paste_text(text) {
    this.call("paste_text", text);
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

  /**
   * Controls the player of session recordings (created with `playback: true`):
   * `load(url)`, `play()`, `pause()`, `seek(ms)`, `speed(x)`, `close()`. Its
   * state arrives as `playback` events (see molcajete-player.js).
   */
  playback(command, ...args) {
    if (!this.#client) {
      this.#post(`player_${command}`, ...args);
      return;
    }
    this.#player ??= new RecordingPlayer(this.#client, (type, detail) => this.#handler?.(type, detail));
    this.#player[command](...args);
  }

  #onFileEvent(type, detail) {
    if (type === "transfer" && detail.blob) {
      const { blob, ...rest } = detail;
      saveBlob(blob, detail.name);
      this.#handler?.(type, rest);
      return;
    }
    this.#handler?.(type, detail);
  }

  /**
   * How downloads reach the disk in this browser: `"file"`, written to a
   * file the user picks as the data arrives (File System Access), or
   * `"blob"`, collected in memory and saved at the end, which a large file
   * may not fit in (see `BLOB_WARNING_BYTES`).
   */
  static get downloadMethod() {
    return typeof window !== "undefined" && typeof window.showSaveFilePicker === "function" ? "file" : "blob";
  }

  /** Downloads above this size are worth a warning when they go through memory. */
  static BLOB_WARNING_BYTES = 1024 * 1024 * 1024;

  /**
   * Lists the directory `path` of the filesystem `object` (from a
   * `filesystem` event). Resolves to entries `{path, name, type, size}`,
   * `type` being "directory" or "file".
   */
  list(object, path) {
    if (this.#client) return this.#files.list(object, path);
    return this.request("files_list", object, path);
  }

  /**
   * Uploads a File: into the filesystem `object` at `path`, or without
   * them into the server's upload directory (where a file dropped on the
   * session goes). Returns the transfer's id; progress arrives as
   * `transfer` events.
   */
  upload(file, { object = null, path = null } = {}) {
    const id = `t${this.#nextTransfer++}`;
    if (this.#client) this.#files.upload(id, file, object, path);
    else this.#post("files_upload", id, file, object, path);
    return id;
  }

  /**
   * Downloads `path` of the filesystem `object`. Call it from the click
   * that asks for it: with File System Access the browser asks where to
   * save first, which needs the click. Resolves to the transfer's id, or
   * null when the user cancelled the save dialog.
   */
  async download(object, path, { name, size = null } = {}) {
    const suggestedName = name ?? path.split("/").filter(Boolean).pop() ?? "download";
    let sink = { blob: true };
    if (MolcajeteSession.downloadMethod === "file") {
      try {
        sink = { handle: await window.showSaveFilePicker({ suggestedName }) };
      } catch (err) {
        if (err?.name === "AbortError") return null;
        // Not allowed here (no user gesture, a sandboxed frame): collect it in memory.
      }
    }
    const id = `t${this.#nextTransfer++}`;
    if (this.#client) this.#files.download(id, object, path, sink, size);
    else this.#post("files_download", id, object, path, sink, size);
    return id;
  }

  /** Stops a transfer. An upload stops where it is; what was sent stays on the server. */
  cancel_transfer(id) {
    if (this.#client) this.#files.cancel(id);
    else this.#post("files_cancel", id);
  }

  /** Disconnects and releases all resources. */
  free() {
    this.#player?.close();
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

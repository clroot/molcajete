// Plays a session recording (Guacamole's format) in a molcajete client,
// paced by the times its frames carry. Runs wherever the client runs: in
// the worker (molcajete-worker.js) or, without OffscreenCanvas, on the page.
//
// Seeking replays the recording from its start (or from where playback is,
// when seeking forward) with drawing to the canvas turned off: every frame
// on the way is applied to the layers, none is shown, and the result is
// drawn once at the end.
//
// Events, through `emit("playback", detail)`:
//   {state: "loading", loaded, total}
//   {state: "ready" | "playing" | "paused" | "seeking" | "ended",
//    time, duration, start, speed}
//   {state: "error", message}

/** Bytes fed to the client per step while seeking. */
const SEEK_STEP_BYTES = 4 << 20;
/** Frames allowed to wait for their images while seeking. */
const SEEK_BACKLOG = 64;
/** Bytes fed per tick while playing; more waits for the next tick. */
const PLAY_STEP_BYTES = 16 << 20;

/** Lets other tasks (image decoders) run. */
const yieldTask = () =>
  new Promise((resolve) => {
    const channel = new MessageChannel();
    channel.port1.onmessage = resolve;
    channel.port2.postMessage(0);
  });

const nextFrame = (callback) =>
  typeof requestAnimationFrame === "function" ? requestAnimationFrame(callback) : setTimeout(callback, 16);

export class RecordingPlayer {
  #client;
  #emit;
  #duration = 0;
  #start = 0;
  #speed = 1;
  #playing = false;
  #ended = false;
  /** Playback position at `#since` (performance.now()) while playing. */
  #anchor = 0;
  #since = 0;
  #time = 0;
  /** Increases with every seek, so that an older one gives up. */
  #seek = 0;
  #seeking = false;
  /** Whether to play once the seek in progress is done. */
  #resume = false;
  #ticking = false;
  #load = null;

  constructor(client, emit) {
    this.#client = client;
    this.#emit = emit;
  }

  #status(state) {
    this.#emit("playback", {
      state,
      time: this.#time,
      duration: this.#duration,
      start: this.#start,
      speed: this.#speed,
    });
  }

  /** Fetches and loads the recording at `url`, showing its first frame. */
  async load(url) {
    this.#load?.abort();
    const abort = new AbortController();
    this.#load = abort;
    try {
      const response = await fetch(url, { credentials: "same-origin", signal: abort.signal });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const total = Number(response.headers.get("content-length")) || 0;
      const bytes = await readAll(response, total, (loaded) => this.#emit("playback", { state: "loading", loaded, total }));
      if (abort.signal.aborted) return;
      this.#playing = false;
      this.#duration = this.#client.load_recording(bytes);
      this.#start = this.#client.playback_start();
      this.#time = 0;
      this.#ended = false;
      await this.seek(0);
      this.#status("ready");
    } catch (err) {
      if (!abort.signal.aborted) this.#emit("playback", { state: "error", message: String(err?.message ?? err) });
    }
  }

  play() {
    if (this.#seeking) {
      this.#resume = true;
      return;
    }
    if (this.#playing) return;
    if (this.#ended || this.#time >= this.#duration) {
      this.#ended = false;
      this.seek(0).then(() => this.play());
      return;
    }
    this.#playing = true;
    this.#anchor = this.#time;
    this.#since = performance.now();
    this.#status("playing");
    this.#tick();
  }

  pause() {
    this.#resume = false;
    if (!this.#playing) return;
    this.#time = this.#now();
    this.#playing = false;
    this.#status("paused");
  }

  /** Playback speed, 1 = as recorded. */
  speed(speed) {
    const position = this.#playing ? this.#now() : this.#time;
    this.#speed = Math.min(Math.max(Number(speed) || 1, 0.25), 64);
    this.#anchor = position;
    this.#since = performance.now();
    this.#status(this.#playing ? "playing" : "paused");
  }

  /** Jumps to `time` milliseconds from the start without drawing what lies between. */
  async seek(time) {
    const id = ++this.#seek;
    const target = Math.min(Math.max(Number(time) || 0, 0), this.#duration);
    if (!this.#seeking) this.#resume = this.#playing;
    this.#playing = false;
    this.#seeking = true;
    this.#time = target;
    this.#status("seeking");
    const client = this.#client;
    client.set_presenting(false);
    try {
      while (!client.playback_advance(target, SEEK_STEP_BYTES)) {
        while (client.playback_pending() > SEEK_BACKLOG) await yieldTask();
        if (id !== this.#seek) return;
      }
      while (client.playback_pending() > 0) {
        await yieldTask();
        if (id !== this.#seek) return;
      }
    } finally {
      if (id === this.#seek) {
        client.set_presenting(true);
        this.#seeking = false;
      }
    }
    this.#ended = target >= this.#duration;
    if (this.#resume && !this.#ended) this.play();
    else this.#status(this.#ended ? "ended" : "paused");
  }

  /** Releases the recording. */
  close() {
    this.#load?.abort();
    this.#seek++;
    this.#playing = false;
    this.#client.unload_recording();
  }

  #now() {
    return Math.min(this.#anchor + (performance.now() - this.#since) * this.#speed, this.#duration);
  }

  #tick() {
    if (this.#ticking) return;
    this.#ticking = true;
    let reported = 0;
    const step = () => {
      if (!this.#playing) {
        this.#ticking = false;
        return;
      }
      this.#time = this.#now();
      this.#client.playback_advance(this.#time, PLAY_STEP_BYTES);
      const now = performance.now();
      if (now - reported >= 250) {
        reported = now;
        this.#status("playing");
      }
      if (this.#time >= this.#duration) {
        this.#playing = false;
        this.#ticking = false;
        this.#ended = true;
        this.#status("ended");
        return;
      }
      nextFrame(step);
    };
    nextFrame(step);
  }
}

/** The whole body, reporting progress, into one array. */
async function readAll(response, total, progress) {
  const reader = response.body.getReader();
  let buffer = new Uint8Array(total || 1 << 20);
  let length = 0;
  let reported = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    if (length + value.length > buffer.length) {
      const grown = new Uint8Array(Math.max(buffer.length * 2, length + value.length));
      grown.set(buffer.subarray(0, length));
      buffer = grown;
    }
    buffer.set(value, length);
    length += value.length;
    if (length - reported >= 1 << 20) {
      reported = length;
      progress(length);
    }
  }
  return buffer.subarray(0, length);
}

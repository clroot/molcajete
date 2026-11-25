// Sends the client's once-per-second `stats` events to the gateway in
// batches, so that browser-side performance (decode, draw, frame lag) can be
// analysed next to the server metrics. Only active when the gateway runs
// with `--client-reports`.

const FLUSH_INTERVAL_MS = 10_000;
const MAX_SAMPLES = 60;

let enabled = null;

/** Whether the gateway accepts reports (asked once per page). */
export function clientReportsEnabled() {
  enabled ??= fetch("api/settings")
    .then((r) => (r.ok ? r.json() : {}))
    .then((s) => s.clientReports === true)
    .catch(() => false);
  return enabled;
}

function randomId() {
  if (crypto.randomUUID) return crypto.randomUUID();
  // randomUUID needs a secure context; plain-HTTP LAN access falls back here.
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

export class SessionReporter {
  /** Id shared with the tunnel URL (`session=`) to correlate logs. */
  session = randomId();
  #connection;
  #mode;
  #samples = [];
  #timer;
  #active = true;

  constructor(connection, mode) {
    this.#connection = connection;
    this.#mode = mode;
    this.#timer = setInterval(() => this.flush(), FLUSH_INTERVAL_MS);
  }

  add(stats) {
    if (!this.#active) return;
    this.#samples.push(stats);
    if (this.#samples.length >= MAX_SAMPLES) this.flush();
  }

  /** Sends pending samples; `beacon` survives page unload. */
  flush({ beacon = false } = {}) {
    if (!this.#active || this.#samples.length === 0) return;
    const body = JSON.stringify({
      session: this.session,
      connection: this.#connection,
      mode: this.#mode,
      samples: this.#samples,
    });
    this.#samples = [];
    if (beacon) {
      navigator.sendBeacon?.("api/telemetry/client", new Blob([body], { type: "application/json" }));
      return;
    }
    fetch("api/telemetry/client", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body,
      keepalive: true,
    })
      .then((r) => {
        // Reports were turned off on the gateway: stop trying.
        if (r.status === 404) this.stop({ flush: false });
      })
      .catch(() => {});
  }

  stop({ flush = true } = {}) {
    if (flush) this.flush({ beacon: true });
    this.#active = false;
    clearInterval(this.#timer);
  }
}

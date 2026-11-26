// Dedicated worker hosting the whole client: WebSocket tunnel, protocol
// parsing, image decoding and WebGL2 rendering into an OffscreenCanvas.
// The page (see molcajete.js) forwards input; nothing heavy runs on the main thread.
//
// Messages in:  {type: "init", canvas, width, height, dpr}
//               {type: "call", method, args, id?}  (files_* for molcajete-files.js)
// Messages out: {type: "ready", zstd, visibility}
//               {type: "event", event, detail}
//               {type: "result", id, result, error?}
import init, { MolcajeteClient } from "./pkg/molcajete_client.js";
import { RecordingPlayer } from "./molcajete-player.js";
import { FileTransfers } from "./molcajete-files.js";

let client = null;
let player = null;
let files = null;
const emit = (event, detail, transfer = []) => self.postMessage({ type: "event", event, detail }, transfer);
const pending = [];

function dispatch({ method, args = [], id }) {
  let result;
  let error;
  try {
    if (method.startsWith("files_")) {
      // File transfer: files_list(object, path), files_upload(id, file,
      // object, path), files_download(id, object, path, sink, size),
      // files_cancel(id). A listing answers with its entries.
      const promise = files[method.slice("files_".length)](...args);
      if (promise instanceof Promise && id !== undefined) {
        promise.then(
          (value) => self.postMessage({ type: "result", id, result: value }),
          (err) => self.postMessage({ type: "result", id, error: String(err?.message ?? err) }),
        );
        return;
      }
      result = promise instanceof Promise ? undefined : promise;
    } else if (method.startsWith("player_")) {
      // Session recordings: player_load(url), player_play(), player_pause(),
      // player_seek(ms), player_speed(x), player_close().
      player ??= new RecordingPlayer(client, emit);
      result = player[method.slice("player_".length)](...args);
      if (result instanceof Promise) result = undefined;
    } else if (method === "connect_to") {
      const [base, connection] = args;
      result = client.connect(client.tunnel_url(base, connection));
    } else {
      result = client[method](...args);
    }
  } catch (err) {
    error = String(err?.message ?? err);
    self.postMessage({ type: "event", event: "error", detail: { code: 0x0200, name: "CLIENT_ERROR", message: error } });
  }
  if (id !== undefined) {
    if (result instanceof Uint8Array) result = Array.from(result);
    self.postMessage({ type: "result", id, result, error });
  }
}

self.onmessage = async ({ data }) => {
  if (data.type === "init") {
    await init();
    client = MolcajeteClient.new_offscreen(data.canvas);
    // File data stays in the worker; the page gets progress, and a Blob
    // (cloned by reference) for downloads collected in memory.
    files = new FileTransfers(client, emit);
    client.set_event_handler((event, detail) => {
      if (files.onEvent(event, detail)) return;
      const transfer =
        event === "cursor" ? [detail.rgba.buffer] : event === "audio-data" ? [detail.data.buffer] : [];
      self.postMessage({ type: "event", event, detail }, transfer);
    });
    client.set_viewport(data.width, data.height, data.dpr);
    // Web Audio only exists on the page: let the page play audio streams.
    client.set_audio_forwarding(data.audio === true);
    self.postMessage({
      type: "ready",
      zstd: typeof client.set_zstd_images === "function",
      visibility: typeof client.set_page_visible === "function",
    });
    for (const message of pending.splice(0)) dispatch(message);
    return;
  }
  if (!client) {
    pending.push(data);
    return;
  }
  dispatch(data);
};

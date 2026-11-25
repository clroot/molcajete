// Dedicated worker hosting the whole client: WebSocket tunnel, protocol
// parsing, image decoding and WebGL2 rendering into an OffscreenCanvas.
// The page (see molcajete.js) forwards input; nothing heavy runs on the main thread.
//
// Messages in:  {type: "init", canvas, width, height, dpr}
//               {type: "call", method, args, id?}
// Messages out: {type: "ready", zstd}
//               {type: "event", event, detail}
//               {type: "result", id, result, error?}
import init, { MolcajeteClient } from "./pkg/molcajete_client.js";

let client = null;
const pending = [];

function dispatch({ method, args = [], id }) {
  let result;
  let error;
  try {
    if (method === "connect_to") {
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
    client.set_event_handler((event, detail) => {
      const transfer =
        event === "cursor" ? [detail.rgba.buffer] : event === "audio-data" ? [detail.data.buffer] : [];
      self.postMessage({ type: "event", event, detail }, transfer);
    });
    client.set_viewport(data.width, data.height, data.dpr);
    // Web Audio only exists on the page: let the page play audio streams.
    client.set_audio_forwarding(data.audio === true);
    self.postMessage({ type: "ready", zstd: typeof client.set_zstd_images === "function" });
    for (const message of pending.splice(0)) dispatch(message);
    return;
  }
  if (!client) {
    pending.push(data);
    return;
  }
  dispatch(data);
};

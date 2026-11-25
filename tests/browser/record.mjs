// Records a live Guacamole session (WebSocket messages and arrival times) for
// replay-bench.mjs. Replies to `sync` so that the server keeps sending frames.
//
//   node record.mjs "ws://127.0.0.1:8080/websocket-tunnel?connection=vnc&width=1280&height=800&image=image%2Fpng&image=image%2Fjpeg" 12 out.json
import { writeFileSync } from "node:fs";

const [url, seconds = "10", out = "recording.json"] = process.argv.slice(2);
if (!url) {
  console.error("usage: node record.mjs <tunnel url> [seconds] [out.json]");
  process.exit(2);
}
const ws = new WebSocket(url, "guacamole");
const start = Date.now();
const messages = [];
ws.onmessage = (event) => {
  messages.push({ t: Date.now() - start, data: event.data });
  for (const m of event.data.matchAll(/(?:^|;)4\.sync,(\d+\.\d+)/g)) ws.send(`4.sync,${m[1]};`);
};
ws.onclose = (event) => {
  if (Date.now() - start < Number(seconds) * 1000) console.error(`closed early: ${event.code} ${event.reason}`);
};
setTimeout(() => {
  ws.close();
  writeFileSync(out, JSON.stringify(messages));
  const bytes = messages.reduce((n, m) => n + m.data.length, 0);
  console.log(`${messages.length} messages, ${(bytes / 1048576).toFixed(2)} MiB -> ${out}`);
  process.exit(0);
}, Number(seconds) * 1000);

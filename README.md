<p align="center">
  <img src="docs/logo/molcajete-mark.svg" alt="molcajete logo" width="160">
</p>

# molcajete

**molcajete** (the stone mortar guacamole is made in) is a Rust
reimplementation of [Apache Guacamole](https://guacamole.apache.org/),
the clientless remote desktop gateway, with a **GPU-accelerated browser client**
written in Rust and compiled to WebAssembly.

| Component | Replaces | Status |
|---|---|---|
| `molcajete-protocol` | protocol code in libguac / guacamole-common / guacamole-common-js | complete: streaming parser, encoder, typed instructions, video framing |
| `molcajete-client` (wasm) | `guacamole-common-js` (`Guacamole.Client`, `Display`, `Keyboard`, `Mouse`, …) | WebGL2 renderer, WebCodecs video, input, clipboard, audio, recording playback |
| `molcajete-gateway` | the Guacamole web application (`guacamole-common`, JDBC authentication, session recording) | WebSocket tunnel, web UI, users, groups, connections and grants in PostgreSQL, MySQL or SQLite, single sign-on, session recordings, active sessions and connection history, audit log, API tokens and one-time session links |
| `molcajete-daemon` (`molcajeted`) | `guacd` + libguac + protocol plugins | VNC, SSH, RDP (incl. audio); video mode (AV1/H.265/H.264) for busy areas |

The gateway is wire-compatible with the stock `guacd`, and accepts the same
tunnel parameters as the stock JavaScript client, so the pieces can be mixed
with an existing Guacamole deployment: the gateway can serve some connections
through stock guacd and others through molcajeted.

## Why a new client?

The reference client draws every layer into its own HTML `<canvas>` and lets
CSS composite them; raster operations read pixels back with `getImageData`,
and everything runs on the page's main thread. `molcajete-client` instead runs the
whole client — WebSocket, parser, image decoding, rendering — **inside a Web
Worker**, rendering into an `OffscreenCanvas` with WebGL2:

- the page only forwards input and applies layout/cursor updates, so its
  responsiveness no longer depends on how much the remote screen changes
  (3–6× less main-thread time than the reference client when replaying the
  same recorded session, see [docs/architecture.md](docs/architecture.md));
- instructions are parsed in wasm and applied one complete frame (`sync`) at a time;
- images are decoded with WebCodecs `ImageDecoder` and uploaded straight into
  GPU textures (opaque images go directly into the layer texture);
- `copy`, `cfill`, Porter–Duff channel masks, `transfer` raster operations,
  clipping and paths all run on the GPU; nothing is read back except the
  cursor image;
- the layer tree (move, shade, distort, nested opacity groups) is composited
  into a single low-latency (`desynchronized`) canvas once per animation frame;
- with molcajeted, areas playing video are streamed as AV1, H.265 or H.264
  (whatever the browser can decode) and decoded with WebCodecs
  `VideoDecoder`, typically in hardware: 1/6–1/15 of the bandwidth of sending
  them as images. Once the area calms down it is refreshed losslessly.

## Features

- **Remote desktops and terminals.** VNC, RDP and SSH through molcajeted or
  stock guacd; Telnet and Kubernetes through stock guacd. RDP desktops can
  follow the size of the browser window and take a display scale. The SSH
  terminal is drawn by molcajeted with bundled DejaVu Sans Mono and a
  NanumGothicCoding fallback, so Korean/CJK text works without fonts
  installed (more with the `font-path` parameter), and has copy and paste
  shortcuts and clickable links.
- **File transfer.** SFTP in SSH sessions and a shared drive in RDP
  sessions, with guacd's parameters: drag files onto the session to upload
  them, browse and download in the session menu's file panel. Large files
  stream in pieces both ways, and Chrome and Edge write downloads straight
  to disk; per-connection switches block either direction, and molcajeted
  can limit the file size.
- **Video mode.** molcajeted sends busy, colourful areas (video playback,
  animations, 3D) as a video stream, encoded by hardware encoders through
  `ffmpeg` (NVENC, QSV, VA-API, AMF), VideoToolbox on macOS, or software
  encoders. Clients that cannot decode video, and lossless or SSH sessions,
  get images.
- **Leaner transport.** Between our client, the gateway and molcajeted,
  image, video and audio data travel as raw bytes instead of base64 (about
  25% fewer bytes), and lossless updates as zstd images the client decodes
  in wasm. Each is negotiated, so stock clients and stock guacd keep working.
- **Users, roles, groups and folders.** Users and user groups with three
  roles (user, connection manager, administrator); connections in folders,
  granted to users or groups, directly or by folder.
- **Single sign-on** through any OpenID Connect provider (Keycloak,
  Authentik, Entra ID, …), with roles and group memberships that can follow
  the provider's groups.
- **Session recordings** in Guacamole's format, for stock guacd and
  molcajeted alike: every session of a connection, or the spans a user
  starts and stops from the session menu. Played back in the browser with
  seeking, or turned into video with `guacenc`.
- **Active sessions and connection history**: who is connected to what
  right now, with a button to end a session; sessions end by themselves
  when their user is disabled or loses access; past sessions with their
  duration and how they ended, exported as CSV.
- **Audit log** of sign-ins and administrative changes: who changed what,
  from where, field by field with secrets redacted; filtered and exported
  as CSV in the web UI, and written to the log for a SIEM.
- **Observability.** JSON logs, Prometheus metrics for every stage of the
  frame pipeline, CPU profiles for Pyroscope, performance reports from the
  browser, and a Grafana dashboard.

## Quick start

Requirements: Rust (stable) with the `wasm32-unknown-unknown` target,
`wasm-bindgen-cli` matching the `wasm-bindgen` version in `Cargo.lock`,
[bun](https://bun.sh) for the web UI, and Docker for the test targets.

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.130

# Test targets: stock guacd, an X11 desktop over VNC, an SSH server and an
# xrdp desktop (published on 127.0.0.1:3390)
docker compose -f docker/compose.yml up -d --build

# Build the wasm client and the web UI into web/dist, then start the gateway
scripts/build-web.sh
cp gateway.example.toml gateway.toml
cargo run -p molcajete-gateway -- --config gateway.toml
# open http://127.0.0.1:8080 and sign in as admin / change-me
```

The gateway keeps its users and connections in SQLite in the current
directory unless `[database]` says otherwise. The connections in
`gateway.example.toml` are imported on the first start; after that, manage
them in the web UI.

To serve a connection with molcajeted instead of the stock guacd, run it and
point the connection at it (*Advanced → guacd host / port* in the web UI, or
`guacd = …` in `gateway.toml`):

```sh
cargo run -p molcajete-daemon -- -l 4823
```

`gateway.example.toml` defines `vnc-rs`, `ssh-rs` and `rdp-rs` this way, next
to the stock-guacd `vnc`, `ssh` and `rdp` connections.

For containers, Kubernetes and running molcajeted natively on macOS, see
[docs/deployment.md](docs/deployment.md).

## In a session

Press **Ctrl+Alt+Shift** for the menu (files, clipboard, scaling,
Ctrl+Alt+Del, performance statistics, disconnect). Drop files on the
session to upload them when the connection has SFTP or a drive. Append `?main` to the URL to run the
client on the main thread instead of a worker, `?text` to keep the tunnel on
the Guacamole text protocol (base64), and `?png` to receive lossless updates
as PNG instead of zstd images.

## Embedding

Another application can put a molcajete session in its own pages without
its users signing in to molcajete: its server asks the gateway for a
one-time session link with an [API token](docs/administration.md#api-tokens)
and hands the link to the browser.

1. Once: create a user for the application (a plain user granted the
   connections it may open is enough), sign in as it and create an API token
   under *API tokens*. Keep the token on the application's server.
2. For each session, on the server:

   ```sh
   curl -H "Authorization: Bearer $MOLCAJETE_TOKEN" -H 'Content-Type: application/json' \
     -d '{}' https://molcajete.example.com/api/connections/office-pc/tickets
   # {"url": "https://molcajete.example.com/#/ticket/3f9c…", "ticket": "3f9c…", "expiresAt": …}
   ```

3. In the page, within a minute (the link works once and expires after
   `[api_tokens] ticket_ttl_seconds`):

   ```html
   <iframe src="https://molcajete.example.com/#/ticket/3f9c…"
           allow="clipboard-read; clipboard-write; fullscreen" width="1280" height="800"></iframe>
   ```

The session runs as the token's owner and appears in the audit log
(`ticket.create`, `ticket.use`) and in recordings like any other. See
[one-time session links](docs/administration.md#one-time-session-links).

To run the client in your own page instead of an iframe, serve
`molcajete.js`, `molcajete-worker.js`, `molcajete-player.js` and `pkg/` from
`web/dist` on your own origin (browsers load module workers only from the
page's origin) and pass the `ticket` to the tunnel:

```js
import { MolcajeteSession } from "./molcajete.js";
const session = await MolcajeteSession.create(document.getElementById("display"));
session.set_event_handler((type, detail) => console.log(type, detail));
// `ticket` from the server; it chooses the connection, so the second argument is ignored.
const url = session.tunnel_url("wss://molcajete.example.com/websocket-tunnel", "office-pc");
session.connect(`${url}&ticket=${encodeURIComponent(ticket)}`);

// File transfer: `filesystem` events announce what can be browsed.
const entries = await session.list(filesystem.object, "/");
session.upload(file, { object: filesystem.object, path: "/home/me/report.pdf" });
await session.download(filesystem.object, "/home/me/data.csv"); // from a click
// progress arrives as `transfer` events
```

A tunnel opened with a ticket needs no cookie and may come from another
origin; without one, the tunnel takes the molcajete session cookie and only
from the gateway's own origin.

## Documentation

- [docs/administration.md](docs/administration.md): users and passwords,
  roles, groups, connections, file transfer, folders and grants, single
  sign-on, session recordings, active sessions and the connection history,
  the audit log, API tokens and one-time session links, the JSON API.
- [docs/deployment.md](docs/deployment.md): images, Docker Compose,
  Kubernetes, molcajeted on macOS, the configuration reference, reverse
  proxies, upgrading.
- [docs/observability.md](docs/observability.md): logs, metrics, profiles,
  browser reports and finding bottlenecks.
- [docs/architecture.md](docs/architecture.md): Apache Guacamole's design
  and molcajete's, component by component, with measurements.

## Development

The web UI (`web/`) is React with the [Astryx](https://astryx.atmeta.com)
design system. For UI work, run `SKIP_UI=1 scripts/build-web.sh` once and
`bun run dev` in `web/`; it proxies the API and the tunnel to a gateway on
127.0.0.1:8080. `bun run check` in `web/` runs the type check, ESLint,
Prettier and the unit tests (`bun run format` fixes formatting).

## Tests

```sh
cargo test --workspace                       # unit tests (protocol, gateway, guacd, client logic)
scripts/build-web.sh                         # then: renderer pixel, keyboard/IME, binary tunnel and playback tests
(cd tests/browser && npm install && npm test)
(cd tests/browser && node video.test.mjs)    # video decoding (needs ffmpeg with libx264/libsvtav1)
(cd tests/browser && node files.e2e.mjs)     # file transfer against a running gateway (see the file)
scripts/fuzz.sh 60                           # each fuzz target for 60 s (needs nightly and cargo-fuzz)
```

The fuzz targets in `fuzz/` feed arbitrary bytes to every parser that reads
an untrusted party: the instruction parser, binary records, zstd images and
video frames, and in molcajeted the handshake and client input, the VNC
client and the SSH terminal; a weekly GitHub Actions job runs them
([architecture](docs/architecture.md#71-tests)).

The browser tests feed Guacamole instructions directly into the wasm client in
headless Chromium and compare layer/canvas pixels with what the HTML canvas
reference implementation produces (composite modes including the "unbounded"
ones, raster operations, overlapping copies, PNG alpha, clipping, paths,
patterns, layer trees and frame atomicity). `playback.test.mjs` plays a
recording with known frames and checks seeking and playing.

Benchmarks in `tests/browser`:

- `replay-bench.mjs` replays a recorded session into the stock
  guacamole-common-js client and into molcajete-client (main thread and
  worker) with identical timing and reports main-thread cost; set
  `GUAC_COMMON_JS` to the `guacamole-common-js/src/main/webapp/modules`
  directory of an Apache Guacamole checkout.
- `seek-bench.mjs <file.guac> [repeat]` repeats a real recording into an
  hour and times seeking through it.

## License

Licensed under the [Apache License 2.0](LICENSE); see [NOTICE](NOTICE).
Keysym tables and protocol behaviour are derived from Apache Guacamole
(Apache-2.0). Bundled fonts keep their own licenses:
DejaVu Sans Mono (`crates/daemon/assets/fonts/LICENSE`) and NanumGothicCoding
(SIL Open Font License 1.1, `crates/daemon/assets/fonts/OFL-NanumGothicCoding.txt`).

Video encoders are not part of this project: molcajeted runs an `ffmpeg`
found on the system as a separate process and links none of its libraries.
H.264 and H.265 are covered by patent pools; if you distribute binaries or
images that include encoders (ffmpeg, or the optional OpenH264 build), check
the codec licensing that applies to you.

Apache, Apache Guacamole and Guacamole are trademarks of The Apache Software
Foundation. molcajete is an independent project, not affiliated with or
endorsed by The Apache Software Foundation.

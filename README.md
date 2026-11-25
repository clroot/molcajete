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
| `molcajete-client` (wasm) | `guacamole-common-js` (`Guacamole.Client`, `Display`, `Keyboard`, `Mouse`, …) | WebGL2 renderer, WebCodecs video, input, clipboard, audio |
| `molcajete-gateway` | the Java web application's tunnel (`guacamole-common`) | WebSocket tunnel, web UI, users/connections/permissions in PostgreSQL, MySQL or SQLite |
| `molcajete-daemon` (`molcajeted`) | `guacd` + libguac + protocol plugins | VNC, SSH, RDP (incl. audio); video mode (AV1/H.265/H.264) for busy areas |

The gateway is wire-compatible with the stock `guacd`, and accepts the same
tunnel parameters as the stock JavaScript client, so the pieces can be mixed
with an existing Guacamole deployment.

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

See [docs/architecture.md](docs/architecture.md) for the analysis of Apache
Guacamole and the design of each component.

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

### Users and connections

Like Guacamole's JDBC authentication, the gateway keeps users, groups,
connections, folders, their parameters and permissions in a database,
managed from the web UI (*Connections*, *Users* and *Groups*).

- **Roles.** Each user and each group has one of three roles; a user's
  role is the highest of their own and their groups':
  - *User* opens the connections granted to them.
  - *Connection manager* also opens every connection, creates, edits,
    moves and deletes connections and folders, and grants them to users
    and groups. Users, groups and roles are out of reach.
  - *Administrator* does everything, including users, groups and roles.
    At least one enabled administrator always remains.
- **Groups.** Connections and folders can be granted to a group instead of
  to each member. Removing someone from a group takes away what the group
  gave them and leaves their own grants.
- **Folders.** Connections live in a tree of folders. A grant on a folder
  covers every connection below it, in subfolders too, so moving a
  connection into or out of a folder changes who can open it. Deleting a
  folder moves what it holds up one level.

A user can open the union of the connections granted to them, to their
groups, and below the folders granted to either; the tunnel refuses any
other connection as unknown. Grants are set from the user or group drawer,
or from a connection's or folder's *Access…* action.

```toml
[database]
url = "postgres://molcajete:secret@db/molcajete"   # or mysql://… / mariadb://…
```

Without `[database]` (or `MOLCAJETE_DATABASE_URL`) the gateway uses SQLite in
`server.data_dir` (`MOLCAJETE_DATA_DIR`, `/var/lib/molcajete` in the image).
The schema is created and migrated on startup.

`[[users]]` and `[[connections]]` in `gateway.toml` are imported once, when
the name or id is not in the database yet. With no user at all, an `admin`
account is created and its random password is logged. To recover access:

```sh
echo 'new-password' | molcajete-gateway --config gateway.toml set-password admin
```

People can also sign in through an OpenID Connect identity provider
(Keycloak, Authentik, Entra ID, …) with an `[oidc]` section: users are created
on their first sign-in, their roles and user groups can follow the provider's
groups, and the password form can be turned off. See [single sign-on](docs/deployment.md#single-sign-on-openid-connect).

The web UI (`web/`) is React with the [Astryx](https://astryx.atmeta.com)
design system. For UI work, run `SKIP_UI=1 scripts/build-web.sh` once and
`bun run dev` in `web/`; it proxies the API and the tunnel to a gateway on
127.0.0.1:8080. `bun run check` in `web/` runs the type check, ESLint,
Prettier and the unit tests (`bun run format` fixes formatting).

To use the Rust daemon instead of the stock guacd for a connection, run
`cargo run -p molcajete-daemon -- -l 4823` and point the connection at it:

```toml
[[connections]]
id = "vnc-rs"
name = "VNC via molcajeted"
protocol = "vnc"
guacd = { host = "127.0.0.1", port = 4823 }
[connections.parameters]
hostname = "127.0.0.1"
port = "5900"
password = "secret"
```

`gateway.example.toml` also defines `ssh-rs` and `rdp-rs` (molcajeted) next to
the stock-guacd `ssh` and `rdp` connections. The SSH terminal is rendered by
molcajeted itself with bundled DejaVu Sans Mono and a built-in NanumGothicCoding
fallback, so Korean/CJK text works without any fonts installed; more fonts
can be supplied with the `font-path` parameter.

### Video mode

molcajeted sends busy, colourful areas (video playback, animations, 3D) as a
video stream into a separate layer, using the first codec of `--video`
(default `av1,h265,h264`) that both the browser can decode and the server can
encode. Encoders come from `ffmpeg` (found in `PATH`, or `--ffmpeg <path>`),
which runs as a child process: hardware encoders (NVENC, QSV, VA-API, AMF)
when a trial encode works, otherwise libsvtav1, libx265 and libx264.
Nothing is linked, so GPL encoders are fine. On macOS, H.265 and H.264 are
encoded in hardware with VideoToolbox, called directly rather than through
ffmpeg. Areas too large for software
AV1 in real time fall back to H.265/H.264. `--features openh264` adds a
built-in H.264 encoder (needs a C++ compiler). `--video none` always sends
images. Lossless (`force-lossless`) and SSH sessions never use video, and
neither do clients that don't announce `video/av1`, `video/h265` or
`video/h264` (such as the stock JavaScript client).

In a session, press **Ctrl+Alt+Shift** for the menu (clipboard, scaling,
Ctrl+Alt+Del, performance statistics, disconnect). Append `?main` to the URL
to run the client on the main thread instead of a worker, `?text` to keep
the tunnel on the Guacamole text protocol (base64) instead of raw image, video
and audio data, and `?png` to receive lossless updates as PNG instead of
zstd images.

Embedding the client in another page:

```js
import { MolcajeteSession } from "./molcajete.js";
const session = await MolcajeteSession.create(document.getElementById("display"));
session.set_event_handler((type, detail) => console.log(type, detail));
session.connect(session.tunnel_url("wss://example.com/websocket-tunnel", "vnc"));
```

## Deployment and observability

`Dockerfile` builds two images: `--target gateway` (with the wasm client)
and `--target daemon` (molcajeted with ffmpeg). `deploy/` has a Docker
Compose file, Kubernetes manifests, a reverse-proxy example and a Grafana
Alloy configuration.

Both binaries can write JSON logs (`--log-format json`). With
`--telemetry-listen`, they also serve Prometheus metrics for every stage of
the frame pipeline, and `--profiling` adds pprof CPU profiles for Pyroscope.
The gateway can also collect performance reports from the browser
(`--client-reports`). See [docs/deployment.md](docs/deployment.md).

## Tests

```sh
cargo test --workspace                       # unit tests (protocol, gateway, guacd, client logic)
scripts/build-web.sh                         # then: renderer pixel, keyboard/IME and binary tunnel tests
(cd tests/browser && npm install && npm test)
(cd tests/browser && node video.test.mjs)    # video decoding (needs ffmpeg with libx264/libsvtav1)
```

The browser tests feed Guacamole instructions directly into the wasm client in
headless Chromium and compare layer/canvas pixels with what the HTML canvas
reference implementation produces (composite modes including the "unbounded"
ones, raster operations, overlapping copies, PNG alpha, clipping, paths,
patterns, layer trees and frame atomicity).

`tests/browser/replay-bench.mjs` replays a recorded session into the stock
guacamole-common-js client and into molcajete-client (main thread and worker) with
identical timing and reports main-thread cost; set `GUAC_COMMON_JS` to the
`guacamole-common-js/src/main/webapp/modules` directory of an Apache Guacamole
checkout.

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

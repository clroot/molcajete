# molcajete: analysis and design

This document analyses the structure of Apache Guacamole (server `guacamole-server`, client `guacamole-client`, main branch as of 2026-11) and describes the design of each molcajete component as it is now: what it does, why it is built that way, and what was measured.

Operational detail lives elsewhere:

- [README](../README.md): what molcajete is and a quick start
- [deployment.md](deployment.md): running the components, the configuration reference, upgrading and how mixed versions behave
- [administration.md](administration.md): users, roles, groups, folders, single sign-on and recordings from an administrator's point of view
- [observability.md](observability.md): metrics, logs, profiling and finding bottlenecks

## 1. Apache Guacamole

```
browser ── HTTP/WebSocket ──▶ web application (Java) ── TCP 4822 ──▶ guacd (C) ──▶ VNC/RDP/SSH/Telnet/K8s
 guacamole-common-js          guacamole + guacamole-common         libguac + protocol plugins
```

| Component | Language | Role |
|---|---|---|
| guacd | C | Forks a process per connection and loads a protocol plugin (`libguac-client-vnc/rdp/ssh/...`), which translates the remote protocol into the Guacamole protocol. |
| libguac | C | Protocol serialization, sockets, user/stream management, the server-side display (change tracking, PNG/JPEG/WebP encoding, frame synchronization), recording |
| guacamole-common | Java | The guacd connection (handshake in `ConfiguredGuacamoleSocket`) and the HTTP/WebSocket tunnels |
| guacamole (web app) | Java + AngularJS | Authentication, connection management, REST API, UI |
| guacamole-common-js | JavaScript | Browser client: parser, tunnel, display, keyboard/mouse/touch, audio, streams |

### 1.1 The Guacamole protocol

- Format: `LENGTH.VALUE` elements joined by `,` and terminated by `;`. Example: `4.size,1.0,4.1024,3.768;`
- `LENGTH` counts **Unicode code points**, not bytes, in all three implementations (libguac, Java, JS).
- Limits: 8192 code points per instruction, 128 elements, 5 digits of length (libguac `parser-constants.h`, Java `GuacamoleParser`). One `blob` carries at most 6048 bytes of raw data.
- Versions: `VERSION_1_0_0`, `1_1_0` (timezone, handshake in any order), `1_3_0` (`required`), `1_5_0` (`name`, `msg`). guacd 1.6 also uses `VERSION_1_5_0`.

Handshake (web app ↔ guacd):

```
select,vnc → args,VERSION_1_5_0,hostname,port,... → size/audio/video/image/timezone/name → connect,VERSION_1_5_0,values... → ready,$connectionID
```

Main instruction groups:

| Group | Instructions |
|---|---|
| Drawing | `size` `move` `shade` `distort` `dispose` `copy` `transfer` `rect` `arc` `curve` `start` `line` `close` `clip` `cfill` `cstroke` `lfill` `lstroke` `push` `pop` `reset` `identity` `transform` `set` `cursor` |
| Images | `img` (stream) + `blob` + `end`, legacy `png`/`jpeg` (inline base64) |
| Streams/objects | `ack` `audio` `video` `clipboard` `file` `pipe` `argv` `body` `filesystem` `get` `put` `undefine` |
| Control | `sync` (end of frame + flow control) `mouse` `key` `size` `touch` `nop` `error` `disconnect` `name` `required` `msg` `log` |

`sync` marks a frame boundary. Once the client has drawn the frame it sends `sync` back with the same timestamp; guacd lowers the frame rate as these replies are delayed (processing lag).

### 1.2 WebSocket tunnel contract (`GuacamoleWebSocketTunnelEndpoint`)

- Subprotocol `guacamole`, text messages only. Each message holds complete instructions only.
- The first message is `0.,36.<tunnel UUID>;` (an internal instruction with an empty opcode).
- The client sends `0.,4.ping,<ts>;` and the server echoes it; it never reaches guacd.
- On termination the WebSocket close reason carries the Guacamole status code (decimal).

### 1.3 Rendering in the stock JS client, and its limits

From an analysis of `Guacamole.Display` / `Guacamole.Layer`:

- Each layer is a `<canvas>` (2D context) wrapped in a `div`; the layer tree is composited by CSS `transform`/`opacity`/`z-index`.
- Canvas sizes are rounded up to multiples of 64 px (`CANVAS_SIZE_FACTOR`). Buffers (negative indices) grow automatically to fit what is drawn.
- Channel masks map onto `globalCompositeOperation` (0x1 destination-in … 0xE source-over, 0xF lighter). `copy`/`source-in`/`source-out`/`destination-in`/`destination-atop` are, by the canvas specification, **unbounded**: they also clear outside the drawn shape. The guacd 1.6 terminal relies on this, drawing the selection with `rect`+`cfill(SRC)` and thereby erasing the previous selection.
- `transfer` (ROP) computes pixels one by one on the CPU via `getImageData`/`putImageData`. On a GPU canvas every call reads back from the GPU and stalls the pipeline.
- Images are decoded with `createImageBitmap`, but the result is drawn again into a 2D canvas with `drawImage`.
- When `sync` arrives, all operations of the frame run together once they are ready (atomic frames).

Known bug: setting a cursor of the same size draws it over the previous one (`setCursor` copies instead of clearing).

### 1.4 What guacd actually sends

Going through the whole guacd source, screen updates consist almost entirely of `img` (PNG/JPEG/WebP) + `copy` + `rect`/`cfill` + `size`/`move`/`shade`/`dispose` + `cursor` + `sync`. Path instructions (`arc`/`curve`/`clip`/`cstroke`, ...) exist in the protocol but no guacd plugin uses them. The composite modes in use are OVER, SRC (cursor, terminal selection), ROUT (clearing transparent layers) and RATOP (emptying buffers with a transparent colour).

## 2. molcajete at a glance

```
browser                                  gateway (Rust, axum)                     molcajeted (Rust, tokio)
┌───────────────────────────────┐        ┌──────────────────────────────┐        ┌─────────────────────────────┐
│ web UI (React + Astryx)       │  HTTP  │ JSON API, static files       │        │ handshake, one task per     │
│ molcajete.js  ── page side    │◀──────▶│ auth: sessions, OIDC         │        │ connection                  │
│ molcajete-worker.js           │        │ authorization: roles, grants │  TCP   │ server-side display, diff,  │
│   molcajete-client (wasm):    │  WS    │ tunnel relay ────────────────┼───────▶│ encoding, pacing, video     │──▶ VNC / SSH / RDP
│   tunnel, parser, WebGL2,     │◀──────▶│ recording, retention         │  4822  │                             │
│   WebCodecs, input            │        │ SQLite / PostgreSQL / MySQL  │        │ (or stock guacd)            │
└───────────────────────────────┘        └──────────────────────────────┘        └─────────────────────────────┘
```

```
crates/
  protocol    molcajete-protocol: parser, encoder, status codes, typed instructions, video framing,
              binary record framing, zstd image format, apply-time reports (no platform dependencies)
  client      molcajete-client: the wasm client (WebGL2 renderer, WebCodecs, input, audio, tunnel, playback)
  gateway     molcajete-gateway: web server, API, authentication, WebSocket ↔ guacd relay, recording
  daemon      molcajete-daemon, binary molcajeted: a guacd replacement for VNC, SSH and RDP
  telemetry   molcajete-telemetry: logging, Prometheus metrics, CPU profiling (gateway and molcajeted)
web/          React + Astryx UI; web/public holds the client glue (molcajete.js, molcajete-worker.js,
              molcajete-player.js) next to the built wasm package (pkg/)
tests/browser headless Chromium tests and benchmarks
docker/       test targets (guacd 1.6, X11 over VNC, OpenSSH, xrdp on 127.0.0.1:3390, Keycloak,
              the same targets behind SSH jump hosts)
deploy/       Compose, Kubernetes, launchd and reverse proxy files (see deployment.md)
```

Every component speaks the Guacamole protocol to its neighbours, so each can be replaced by its stock counterpart:

| Browser | Gateway | Daemon |
|---|---|---|
| molcajete-client (wasm) | molcajete-gateway | stock guacd or molcajeted |
| stock guacamole-common-js | molcajete-gateway (accepts the `GUAC_*` tunnel parameters) | stock guacd or molcajeted |
| stock web app | Java web app | molcajeted (compatible handshake and parameter names) |

Between molcajete components a few extensions are used. Each is offered in a way a stock peer ignores, and each falls back to plain Guacamole when the other side does not take it:

| Extension | How it is offered | Who acts on it | Without it |
|---|---|---|---|
| Binary tunnel (section 3.3, 5.3) | WebSocket subprotocol `molcajete-binary-v1`; connection parameter `molcajete-binary` listed last in `args` | gateway, molcajeted | base64 `blob` instructions in text messages |
| zstd images (section 3.5) | mimetype `image/x-molcajete-zstd-v1` in the handshake's `image` | molcajeted | PNG |
| Video streams (section 6.5) | `video/av1`, `video/h265`, `video/h264` in the handshake's `video` | molcajeted | images only |
| Apply-time reports (section 3.6) | `apply-us` as an extra argument of `sync` | molcajete client | pacing by round trip |
| `display-control` (section 4.8) | instruction sent by molcajeted's RDP sessions | molcajete web UI | menu items stay enabled; changes apply at the next connection |
| `link`, `selection` (section 4.7) | instructions sent by molcajeted's SSH terminal | molcajete web UI | no link opening; copy shortcut uses the last selection |
| Per-browser display parameters (section 4.8) | `param.dpi`, `param.resize-method`, `param.font-size` in the tunnel URL | gateway | the connection's own values |
| SSH jump hosts (section 6.9) | `jump-hosts` and `jump-host-*` connection parameters, listed in molcajeted's `args` | molcajeted | the daemon connects to the server directly |
| File transfer details (section 6.11) | two extra arguments of `filesystem` (downloads, uploads allowed), a `get-error` instruction, `size=` as a parameter of the listing's and the download's mimetype | molcajete web client | the panel offers both directions, a refused `get` stays unanswered, no sizes or progress totals |
| `max-file-size` (section 6.11) | connection parameter listed in molcajeted's `args` | molcajeted | no limit |
| Full-screen video (section 6.5) | `video-mode,<auto\|full-screen>` sent by molcajeted's sessions that stream video; the client answers with an `argv` stream `video-mode` | molcajete web UI, molcajeted | no switch in the session menu; busy areas only |

How each extension behaves across component versions is tabulated in [Mixed versions](deployment.md#mixed-versions) in deployment.md.

## 3. Protocol crate (`molcajete-protocol`)

The crate is shared by the daemon, the gateway and the wasm client, so it has no platform-specific dependencies and builds for `wasm32-unknown-unknown`.

### 3.1 Parser and encoder

- `Parser` is incremental and byte-oriented. Data may arrive split anywhere (a TCP read, a WebSocket message), even inside a UTF-8 sequence; the parser keeps its scanning state so that each byte is looked at once. ASCII runs are skipped in bulk, which keeps base64 blobs cheap.
- `Parser::feed` appends bytes; `Parser::feed_with` lets the caller write straight into the parser's buffer (the wasm client copies a binary message's instruction text this way, section 4.6).
- `RawInstruction` is a zero-copy view into the parser buffer (`opcode`, `arg`, `as_bytes`). The gateway uses it to find instruction boundaries and forwards the bytes unchanged.
- `Limits::STRICT` matches libguac (8192 code points, here as 32768 bytes; 128 elements; 5 length digits) and is used for untrusted input: what the browser sends to the gateway, and what a client sends to molcajeted. `Limits::LENIENT` (16 MiB, 4096 elements) is for data from a trusted server.
- `Instruction` / `InstructionBuilder` encode instructions, counting element lengths in code points (`element_length`).
- `Status` holds the Guacamole status codes; `ProtocolVersion` the handshake versions.

### 3.2 Typed instructions

`server::ServerInstruction<'a>` decodes server → client instructions into an enum, borrowing string arguments (a base64 blob is never copied). It covers the drawing, image, stream and control instructions of section 1.1 plus molcajete's own `display-control`, `link` and `selection`. Decoding failures are `DecodeError` values naming the opcode and argument.

### 3.3 Binary record framing (`binary`)

In the Guacamole protocol stream data is base64 inside `blob` instructions inside text WebSocket messages: 4 bytes on the wire for every 3 of an image, a video frame or an audio packet. Between molcajete components the server → client stream is a sequence of records instead:

```
record = tag (1 byte)  length (4 bytes, unsigned, little endian)  body (length bytes)

tag 0x00  instructions   body = Guacamole protocol text (UTF-8), whole instructions
tag 0x01  blob           body = stream index (4 bytes, unsigned, little endian) + raw data
                         same meaning as blob,<stream>,<base64 of data>;
other     skipped by readers (room for later additions)
```

- The instruction records, concatenated, are the ordinary instruction stream without its `blob` instructions; a blob record applies at its position in that stream. `img`, `video`, `audio`, `end` and `ack` are unchanged.
- Consecutive blob records of a stream are equivalent to one with the data joined. Writers keep a record within 64 KiB (`MAX_WRITTEN_RECORD`), so an image up to that size is one blob instead of one per 6048 bytes, and larger data is split (`RecordWriter::blob_aligned` splits PCM only between whole sample frames). Readers accept bodies up to 16 MiB (`MAX_RECORD_BODY`).
- On TCP the records follow each other in the byte stream, starting right after the `;` of `ready`. On the WebSocket every binary message holds whole records.
- `RecordWriter` builds records (molcajeted, and the gateway when it converts a stock guacd's text), `RecordReader` cuts a byte stream split anywhere into records (the gateway's TCP side), and `records` iterates over one WebSocket message.
- Only the server → client direction uses records. What the client sends (input, acknowledgements, clipboard and `argv` streams) is small and stays text.

**Why records, and not binary messages for blobs only.** Carrying each blob as its own binary WebSocket message between text messages would have been the smallest change, but a text message cannot continue across a blob: a frame with *n* images would take about 2*n* messages where the gateway sent one or two, and each message is a task in the browser. With records one message still carries a whole batch (up to about 64 KiB), the gateway forwards molcajeted's records without re-encoding them, and the client finds each part from its 5-byte header instead of scanning. The record size is capped because the client treats 15 s without a message as a dead tunnel and a message only counts once it has arrived completely: bounded messages keep arriving on a slow link. A binary encoding of the instructions themselves was not worth it: outside the blobs the stream is a few percent of the bytes, and keeping it as text keeps one parser and one encoder for both modes.

### 3.4 Video framing (`video`)

A `video` stream carries this project's framing: per frame a 9-byte header (`FRAME_HEADER_LEN`: length, timestamp in milliseconds, key frame flag) followed by one H.264/H.265 access unit in Annex B form or one AV1 temporal unit of OBUs. molcajeted writes it with `encode_frame`; the client reassembles frames across blob boundaries with `FrameReader`. `VideoCodec` maps the mimetypes `video/av1`, `video/h265` and `video/h264`.

### 3.5 zstd images (`zimage`)

A lossless image type that the wasm client decodes itself (mimetype `image/x-molcajete-zstd-v1`). An image is indexed or RGB pixels, as they are, in one Zstandard frame:

```
u8      kind: 0 = indexed, 1 = RGB
u16 LE  width  (1 or more)
u16 LE  height (1 or more)
indexed only:
  u8    number of colours - 1
  [..]  palette: 3 bytes (R, G, B) per colour
[..]    one Zstandard frame (RFC 8878) holding
          indexed: the rows of palette indices, each index taking 1 bit for up to
                   2 colours, 2 bits up to 4, 4 bits up to 16 and 8 bits otherwise,
                   first pixel in the highest bits, every row padded to a whole
                   byte (as in PNG)
          RGB:     width * height * 3 bytes, rows top to bottom
```

- Every pixel is opaque; there is no alpha channel, no row filter and no interlacing. The `img` instruction says where the image goes and with which composite mode, as for any other type.
- An image has at most 2^25 pixels (`MAX_PIXELS`; an 8K screen has 33.2 million).
- The frame uses no dictionary and is the only one. It decompresses to exactly the number of bytes the header implies. Its window is at most 8 MiB (`MAX_WINDOW`) unless it is a single-segment frame, so a decoder can check before allocating that it needs no more than the image and 8 MiB.
- An index without a palette entry is black.
- A decoder refuses anything else, and a refused image is skipped like an image the browser cannot decode: its frame is still applied. A change to the format is a new mimetype.

`zimage::Decoder` checks the header and the frame header itself and leaves the Zstandard frame to `ruzstd`, a pure Rust decoder, so the crate still builds for wasm. It never panics and never produces more or fewer pixels than announced; tests feed it images cut at every length, every byte replaced, bytes removed and inserted, and random data. The encoder needs libzstd and lives in molcajeted (`display/encode.rs`).

**Why this encoding.** The candidates were compared on the images of two recorded RDP sessions (a list being scrolled: 1194 images, 72.6 megapixels, 95% of them with at most 256 colours; pages being opened: 49 images, 14.1 megapixels, 57% of the pixels in two windows with more colours) and on the images `display::bench` produces (text whose glyphs are noise, which is pessimistic). Bytes are relative to the PNG molcajeted sends; encoding was timed natively on an Apple M4, decoding in wasm without SIMD in headless Chromium 1228 on the same machine, warm, per megapixel.

| Candidate | Bytes: list | Bytes: pages | Bytes: bench `page` | Bytes: bench `scatter` | Encode ms/MP (list / bench `page`) | Decode in wasm ms/MP (list / bench `page`) | Decoder in the wasm file |
|---|---|---|---|---|---|---|---|
| PNG, DEFLATE level 4 (the baseline) | 100% (8602 kB) | 100% (565 kB) | 100% (6134 kB) | 100% (2533 kB) | 4.96 / 6.82 | `ImageDecoder`: 1.38 / 0.88 one at a time, 0.67 / 0.27 eight at a time | none |
| QOI | 240% | 1024% | 239% | 247% | 1.05 / 1.22 | 0.72 / 0.75 | 4 kB |
| indexed or RGB + LZ4 | 128% | 122% | 165% | 180% | 1.46 / 2.15 | 0.74 / 0.84 | 9 kB |
| indexed or RGB + zstd level 1 | 100% | 73% | 98% | 101% | 1.55 / 2.43 | 2.54 / 3.14 | 45 kB |
| **indexed or RGB + zstd level 3** | **96%** | **65%** | **94%** | **101%** | **1.64 / 2.55** | **2.10 / 2.90** | **45 kB** |
| indexed or RGB + zstd level 5 | 94% | 62% | 92% | 98% | 3.18 / 3.92 | not measured | 45 kB |
| indexed or RGB + DEFLATE level 4 | 98% | 86% | 100% | 98% | 4.03 / 6.64 | 1.31 / 1.35 | 20 kB |
| RGB rows with PNG's adaptive filter + zstd level 1 (no palette) | 225% | 150% | 164% | 163% | 8.83 / 8.11 | 6.46 / 6.05 | 45 kB |
| PNG decoded in wasm (`png` crate) | 100% | 100% | 100% | 100% | as PNG | 1.30 / 0.98 | 110 kB |

- **QOI and LZ4** have no entropy coder. They decode fastest, but take 22% to ten times more bytes than PNG, and the bytes of a large frame are what a session waits for longest.
- **Row filters** make screen content larger under an LZ coder: an exact repetition of a glyph is no longer one after filtering. They only pay for photographs, which stay PNG (section 6.2).
- **The palette** makes the difference on text: the same pixels as RGB take half as many bytes again.
- **Zstandard against DEFLATE** over the same pixels: about the same bytes at 40% of the encoding time, against a decoder that is slower in wasm (`ruzstd` 350 to 500 MB/s, `miniz_oxide` about 750 MB/s) and 25 kB larger. Level 3 costs 5% more time than level 1 for 4% fewer bytes (11% on the pages).
- **Decoding PNG in wasm** would have served stock guacd too, with no new format, but saves molcajeted nothing. The client does decode PNG in wasm, but only while seeking through a recording (section 4.10).
- **Alpha** was left out: molcajeted sends transparent pixels only in cursors and in regions a remote protocol reports as transparent, which stay PNG.

The comparison tool depends on recordings of a private desktop and is not in the repository; `display::bench` and `display::bench::zstd_images` reproduce the synthetic part.

### 3.6 Apply-time reports (`apply_time`)

From the reply to `sync` alone a server learns only the whole round trip: the network both ways, the frame's data arriving, and the client's decoding and drawing. Pacing by that sends fewer frames the farther away the client is, although a window of unacknowledged frames already keeps the server from running ahead. A molcajete client therefore says how long a frame kept it busy:

```
server -> client   sync,<timestamp>,<frames>,apply-us;
client -> server   sync,<timestamp>,<frames>,<microseconds>;
```

- The time runs from the moment the frame was complete (its `sync` arrived) or, if later, the moment the frame before it was applied, to the moment it was applied. Waiting for its images to decode counts; waiting behind earlier frames does not.
- There is no handshake: molcajeted asks in every `sync` (`apply_time::sync_request`), and only a client that understands it answers (`apply_time::sync_reply`). A `sync` that does not ask gets the stock reply, so stock guacd sees nothing new; guacamole-common-js ignores the extra argument and its reply carries no time. The gateway passes both directions through unchanged.

How molcajeted uses the reports is in section 6.4.

## 4. Client (`molcajete-client` and `web/public`)

The client is a Rust crate compiled to wasm (`MolcajeteClient`), plus three JavaScript files: `molcajete.js` (`MolcajeteSession`, the page side), `molcajete-worker.js` (hosts the wasm client in a worker) and `molcajete-player.js` (`RecordingPlayer`). The React session page (`web/src/session`) uses `MolcajeteSession` through `useMolcajeteClient`.

### 4.1 Worker architecture

By default the whole client (WebSocket tunnel, parser, image decoding, WebGL2 rendering, compositing) runs in a dedicated worker. The visible canvas is an `OffscreenCanvas` handed over with `transferControlToOffscreen()`. Only this stays on the main thread:

- receiving pointer, wheel, keyboard and IME events (on a hidden `textarea`, the input sink) and posting them to the worker
- applying the worker's `layout` (canvas CSS size) and `cursor` (RGBA turned into a CSS cursor) events to the DOM
- playing audio (Web Audio does not exist in workers) and the UI itself

Browsers without OffscreenCanvas run the same wasm client on the main thread (`MolcajeteClient::new` instead of `new_offscreen`); `?main` on the session page forces this. The reason for the split is the measurement in section 7.2: in Chromium, uploading CPU-backed images into WebGL is synchronous on the calling thread, so a main-thread WebGL client was slower than the stock 2D-canvas client, while the worker cut main-thread time to a third to a sixth of it.

All browser callbacks go through one `with_inner` borrow of the client state, so re-entrant calls from JavaScript never meet an active borrow. While connected the client keeps itself alive, so the session survives the JavaScript handle being garbage-collected.

### 4.2 GPU renderer (`gl`)

**Layers are textures.** Each layer and buffer is an RGBA8 texture (premultiplied alpha) with its own framebuffer. Allocations grow in steps of 64 px (`ALLOC_GRANULARITY`), and resizing moves the old contents with `blitFramebuffer`. Textures are stored top row first, so inside a framebuffer `gl_FragCoord.xy` is the layer pixel, and the Y axis is flipped only when compositing onto the canvas. Nothing is read back to the CPU except the cursor.

| Instruction | Implementation |
|---|---|
| `rect`+`cfill` | Non-overlapping rectangles are drawn as quads with vertex colours in a single draw call. |
| Channel masks | `blendFunc` setups for the premultiplied Porter–Duff modes |
| Unbounded modes (SRC/IN/OUT/RIN/RATOP, `is_unbounded`) | For a single rectangle, the outside is cleared with at most 4 scissor+clear calls. Otherwise a coverage mask drives a "clear outside" pass followed by a "draw inside" pass. |
| `copy` | Between layers the source texture is sampled directly. Within a layer (scrolling) the region is first copied into a scratch texture with `copyTexSubImage2D`. |
| `transfer` | ROP shader: applies the 4-bit truth table with integer bit operations to un-premultiplied 8-bit values. SRC uses `blitFramebuffer` (`Renderer::put`), ignoring clip and composite mode like `putImageData`. |
| `img` | Section 4.4 |
| `video` | Section 4.5 |
| Paths (`arc`/`curve`/`line`/`clip`/`cstroke`/`lfill`) | Arcs and curves are flattened into polylines; fills (nonzero) and strokes are tessellated with lyon (`display/path.rs`). Coverage is drawn into an R8 mask that a shader samples. |
| `clip` | Scissor for axis-aligned rectangles, otherwise a per-layer R8 clip mask (intersections by multiplicative blending) |
| `lfill`/`lstroke` | Pattern shader repeating the source surface |
| `push`/`pop`/`reset`/`transform` | Per-layer drawing state stack (transform, clip, composite mode, miter limit) |
| `move`/`shade`/`distort`/`dispose` | Layer tree properties, applied when compositing |
| `cursor` | Copied into a cursor texture and read back **asynchronously** with a PBO and a fence (`Readback`), so the pipeline never stalls. The CSS cursor is updated only when the pixels changed, and the readback is skipped when the source did not change. The server's `mouse` position is drawn as a software cursor. |

**Compositing.** Once per animation frame the layer tree is walked and each layer drawn with its transform (`translate`, then `distort` about its centre), opacity and the parent's clip. A translucent layer with children is first rendered offscreen and blended with the group opacity, like CSS `opacity`. The canvas is created with `desynchronized: true` (low latency), and linear filtering is used only for non-integer scale factors.

### 4.3 Frames

Instructions are queued as `Op`s, and a frame (everything up to `sync`) runs at once when all of its images are ready, as in `Guacamole.Display`. Intermediate states never reach the screen. Each queued frame counts its pending images (`display/frames.rs`), so the queue is not rescanned after every decoded image. When a frame has run, `sync` is sent back, with the apply time when the server asked for it (section 3.6), and the next `requestAnimationFrame` composites once.

The display also measures where a frame's time goes: when it starts arriving (`display/arrival.rs`, starting at the first instruction that draws on a visible layer), how long it waits for images, and how long an applied frame waits to be presented (`display/present.rs`). These go out as once-per-second `stats` events; see [observability.md](observability.md).

### 4.4 Images

A frame may mix three ways of getting pixels, and the drawing order is that of the instructions in every mix.

**PNG, JPEG, WebP: the browser's decoder** (`image.rs`). The encoded image goes to WebCodecs `ImageDecoder` (no colour-space conversion) as a `Uint8Array`: on the binary tunnel a view of the received message when the image arrived in one blob (no copy, no pass through wasm memory), otherwise one joined array. Without `ImageDecoder`, a Blob and `createImageBitmap` are used; per image the Blob alone cost 80 ms per second of a busy session in Chromium, which is why the stock client also prefers `ImageDecoder`. The first 2 KiB (`OPACITY_PREFIX_BYTES`) enter wasm to read the PNG (colour type, tRNS) or WebP (VP8/VP8L/VP8X) header: opaque images without transform or clip are uploaded straight into the layer texture, the rest go through a reused upload texture and are blended. Decoded images are first drawn into an accelerated 2D `OffscreenCanvas` (deferred to the GPU process) and uploaded from there, so the WebGL upload is a GPU-to-GPU copy rather than a synchronous copy on the drawing thread.

**zstd images: decoded in wasm when the frame is drawn.** A zstd image (section 3.5) is kept as received (`QueuedImage::Zstd`) in the place its `img` takes in the frame, and counts as ready as soon as its `end` arrives, so a frame of them waits for nothing. When the frame runs, the image is decoded into a pixel buffer reused from image to image and uploaded with `texSubImage2D` from a view of the wasm memory, into the layer texture for SRC and OVER without transform or clip, through the upload texture otherwise. No decoder object, no promise, no staging canvas. The worker announces the type only when its wasm can decode it (`set_zstd_images`; `?png` on the session page turns it off).

**PNG decoded in wasm while seeking** (`png.rs`, `QueuedImage::Png`). While nothing is presented (section 4.10), PNG images bypass the browser: the `png` crate decodes and premultiplies them in wasm and they are uploaded like zstd images. Through `ImageDecoder` and the staging canvas an image costs about 120 µs on an Apple M4, most of it the staging `drawImage`; decoded in wasm about 30 µs, most of it `texSubImage2D`. Live sessions keep the browser's decoder, which runs off the drawing thread.

### 4.5 Video (`video.rs`)

The client advertises in the handshake only the codecs `VideoDecoder.isConfigSupported` accepts (`video/av1`, `video/h265`, `video/h264`). A `video` stream is unpacked with the framing of section 3.4 into a WebCodecs `VideoDecoder` (`optimizeForLatency`, hardware decoding where available). Each frame takes an image slot in the frame queue, like `img`, filled by the decoder's `VideoFrame`, so video stays in step with `sync` and with the other drawing. Decoded frames are uploaded to the layer texture on the GPU. A decoder error fails the pending slots (the frames are still applied) and is reported to the server with an `ack` error, after which the server sends images.

On the binary tunnel a frame that lies whole within a blob (molcajeted starts each frame in a new record) goes to `EncodedVideoChunk` as a view of the message; a larger frame is reassembled by `FrameReader`.

The decoder is configured with a fixed, generous codec string per codec (`codec_string`), not one derived from the stream's SPS or sequence header.

### 4.6 Tunnel (`tunnel.rs`)

The client offers the subprotocols `molcajete-binary-v1, guacamole` (`set_binary_tunnel(false)` or `?text` offers only `guacamole`). Which one the gateway chose only shows in the messages that arrive, so both kinds are accepted at any time; the tunnel's own messages (UUID, ping replies) are always text.

A binary message is an `ArrayBuffer`. The client reads each record header through a typed-array view, copies instruction text straight into the parser buffer (`Parser::feed_with`), and keeps stream data as views of the message: images as in section 4.4, video as in section 4.5, and PCM copied once, into a buffer of its own for the page (transferred from the worker in worker mode) (transferring the message would detach the image views). The text path (string → parser → base64 decoder) stays for stock gateways and for recordings.

Keep-alive: `nop` every 5 s, a tunnel ping every second (its round trip is reported in `stats`), and the tunnel counts as dead after 15 s without a message.

### 4.7 Input, clipboard and terminals

**Keyboard** (`input`). The keysym is derived from `KeyboardEvent.key`/`code`/`location` with tables ported from guacamole-common-js. The keysym sent on press is remembered per physical key (`code`) and sent again on release, so releasing Shift first does not leave an `A` pressed. Keys are released when the input sink loses focus (`release_keys`). Ctrl+Alt+Shift opens the session menu (`useMenuShortcut`), as in Guacamole.

**IME.** By default composition is left to the browser, and the `compositionend` result is sent as Unicode keysyms (`0x01000000 | code point`), pressed and released.

**Remote composition** (`set_remote_composition`, session menu, remembered per connection in the browser). The remote host's IME composes instead, as with native RDP and VNC clients. The input sink becomes read-only so that the browser starts no composition, and a key the local input source claims (`key` is `Process` or a Hangul jamo) is sent as the US-layout character of its physical position (`code`, with Shift), so every key press reaches the remote side at once. `Lang1`/`HangulMode` are sent as `Hangul` (0xFF31); on macOS, which switches input sources with Caps Lock, a Caps Lock press that does not change the lock state is sent as `Hangul` too. molcajeted's RDP side sends `Hangul` and `Hangul_Hanja` as scancodes 0x72 and 0x71, and `server-layout=ko-kr-qwerty` sets the input language to Korean (0x412). SSH has no remote IME and always composes in the browser.

**Mouse.** Pointer Events with pointer capture. Wheel deltas accumulate in 53 px steps (as in the stock client).

**Keys from the page.** The on-screen keyboard of a phone has no Tab, Esc, Ctrl, Alt or arrow keys, so the page can send them itself: `press_key(keysym)` presses and releases a key, and `hold_modifier(name, hold)` holds `ctrl` or `alt` as if its key were down, `once` until the next key has gone out or `locked` until it is turned `off` (`input::HeldModifiers`). The next key is whichever comes first: a `keydown`, a `press_key`, or the first character of text typed through `type_text`, which is how an on-screen keyboard's letters arrive; the client releases the modifier right after that key's press. Every change is reported as a `modifiers` event (`{ctrl, alt}`), a disconnect releases both, and `paste_text` does not press a Ctrl that is held this way a second time.

**Clipboard.** Both directions as UTF-8 `clipboard` streams in 3 KiB blobs (`send_clipboard`). The daemon enforces a connection's `read-only`, `disable-copy` and `disable-paste`; like guacd, a read-only connection takes no clipboard from the browser either, so `read-only` implies `disable-paste`; the connection list gives the session page these three flags (`readOnly`, `disableCopy`, `disablePaste`) so that it shows a *Read-only* badge and the session menu only offers the clipboard directions that work. `required` requests (missing credentials) are answered with `argv` streams (`send_argv`), which also carry display parameters during a session (section 4.8).

**Terminals.** SSH sessions are drawn on the server (section 6.7), so the browser has no text to select, copy or paste. For terminal protocols the page turns on terminal shortcuts (`set_terminal_shortcuts`): `molcajete.js` listens on the container in the capture phase, ahead of the client's own listeners in either mode, and keeps the keys it handles from the remote side.

- *Copy.* Releasing a selection makes the daemon send its text as a `clipboard` stream (as guacd does); the page writes it to the local clipboard. That write happens after a round trip, outside the user gesture, so the shortcut (⌘C on macOS, Ctrl+Shift+C elsewhere; Ctrl+C stays `^C`) writes the last selection again within the key event. A click that clears the selection makes molcajeted send `selection,0`, so the shortcut does not copy text that is no longer selected.
- *Paste.* ⌘V and Ctrl+Shift+V are neither forwarded nor prevented, so the browser fires `paste`, whose text is read without the Clipboard API's permission prompt. The client sends it as a `clipboard` stream followed by Ctrl+Shift+V (the paste keys of molcajeted and guacd), pressing Ctrl and Shift only when not already held (`paste_text`). Instructions keep their order on the tunnel, so the daemon has the text before the keys.
- *Links.* molcajeted reports the link under the pointer with `link,<url>`, or `link` without an argument when there is none. The page shows the target, and ⌘-click (Ctrl-click elsewhere) opens it with `noopener,noreferrer` within the click, so popup blockers let it through; the click does not reach the remote side.
- *Touch.* A finger sent as a mouse with its button held could only select, and a phone has no wheel. With terminal shortcuts on, `molcajete.js` therefore handles pointers of type `touch` itself, in the same capture phase: a drag goes out as wheel movement of the same distance (so the history scrolls, and `less` or an application that reads the mouse too), a finger that rests for 500 ms selects (the button goes down where it rested and up where it is lifted), a tap focuses the input sink, which opens the on-screen keyboard, and clicks, and two fingers moving apart or together are reported as `zoom` events (`{delta: 1}` or `{delta: -1}` for each 15 % of change), which the page turns into the text size (section 4.8). A mouse, a pen and touch in other sessions are left as they were.
- *Key bar* (`TerminalKeyBar`). On a device whose main pointer is coarse (or where the session menu's *Show terminal keys* says so, preference `terminalKeys`), a terminal session that is not read-only has a bar under the terminal: Esc, Tab, Ctrl, the arrows (repeating while held), Alt, paste, `|` `/` `-` `~`, Home, End, PgUp, PgDn in a strip that scrolls sideways, and the button of the session menu, which replaces the floating one. Ctrl and Alt cycle through `once`, `locked` and `off` and show what the client's `modifiers` events say. The bar prevents `mousedown`'s default, so its buttons never take focus from the input sink and the keyboard stays open. Paste reads `navigator.clipboard` within the tap and falls back to the text of the menu's Clipboard field. A tapped link is shown over the bar with a button that opens it, and a selection the browser refuses to copy outside a gesture gets a toast with a *Copy* button.
- *The keyboard's room.* Browsers on phones keep the layout viewport when the on-screen keyboard opens and shrink only the visual one. While the bar shows, the display and the bar are laid out in `window.visualViewport` (`useVisibleFrame`), so the container gets shorter, the client sends `size`, and the terminal ends above the bar with fewer rows instead of lying under the keyboard.

### 4.8 Display size and scale

The client scales the remote display into its container: `Fit` shrinks to fit and never enlarges, `Native` shows one remote pixel per device pixel (`set_scale_mode`). With auto-resize (`set_auto_resize`) it sends `size` when the container resizes, and the current size when auto-resize is turned on.

For RDP the browser also decides whether the desktop takes the window's size (`resize-method=display-update`) and its scale (`dpi`, 96 per 100 %; without one the DPI is `96 × devicePixelRatio` of the connecting browser). Both are connection parameters with guacd's names and meanings. The session menu changes them for one connection in one browser (preferences `remoteScale.<id>` and `resizeToWindow.<id>` in `localStorage`):

- At connect the page adds `param.dpi=<dpi>` and `param.resize-method=display-update` to the tunnel URL (`tunnelOverrides`). The gateway replaces the connection's parameters with these for that tunnel only; an empty value removes the parameter, so "Automatic" makes the DPI follow the browser even when the connection has a fixed one. No other parameter can be set this way (`valid_override`). The connection list returns `dpi` and `resizeMethod` so that the menu can show what is in effect.
- During the session the page sends a new value as an `argv` stream, the protocol's way to change a parameter while connected. molcajeted applies `dpi` and `resize-method` immediately (section 6.8); stock guacd does not, so with it a change takes effect at the next connection.
- The text size of a terminal works the same way: the session menu's *Text size* (6 to 18 pt; preference `textSize.<id>`) and a pinch on the terminal send `font-size` as `argv`, which molcajeted's SSH terminal (section 6.7) and guacd apply at once, and the page adds `param.font-size` at connect; the gateway accepts 4 to 72 pt. The connection list returns `fontSize` for the menu to start from. For a terminal the menu shows this *Terminal* group instead of *Display*: a terminal follows the window, so *Fit* has nothing to do.
- molcajeted sends `display-control,0` when its RDP server cannot apply changes and `display-control,1` when it can; the session menu then disables "Remote scale" and "Resize to window", and "Fit" stays.
- A session that streams video (VNC or RDP with molcajeted, a codec in common, not `force-lossless`) sends `video-mode,auto` with its first frame, and the session menu then shows "Stream the whole screen as video". Turning it on or off sends `video-mode` as an `argv` stream (`full-screen` or `auto`); molcajeted answers with the mode now in use, and with `video-mode,` (empty) once video is off for the session. The choice is not remembered: every session starts with busy areas. A user who joined a session (section 6.10) has the switch for their own display.

### 4.9 Audio

Raw PCM (`audio/L8`, `audio/L16`) is played with Web Audio, scheduled 50–300 ms ahead. In both modes the client hands PCM to the page (`set_audio_forwarding`; `audio-start`/`audio-data`/`audio-end` events, buffers transferred without copying from the worker), and the page's `PcmPlayer` plays every stream in one `AudioContext`. The wasm client's own player (`audio.rs`) is only used when forwarding is off.

Browsers create an `AudioContext` suspended unless the page is handling a user gesture, and a stream's first sound arrives long after the click that opened the session. The web UI therefore makes the page's context in that click (a connection's link or *Connect*, and *Reconnect*; `session/audio.ts`) and hands it to `MolcajeteSession.create` (`audioContext`). Any press, click, touch or key on the page resumes it. When a stream is open while the context is still suspended, as in a session opened from a bookmark or a new tab, the client reports `audio` events (`{blocked}`) and the session shows *Click or press a key to turn on sound* until a gesture lets the sound through.

### 4.10 Playback of recordings

The same client plays recordings made by the gateway (section 5.6) or by guacd. `MolcajeteSession.create(container, {playback: true})` makes a player that takes no input; `RecordingPlayer` (`molcajete-player.js`) runs where the client runs (the worker, or the page without OffscreenCanvas).

- **Loading.** The page fetches the file with progress; the client copies it into wasm memory and indexes it (`recording::Index`, `load_recording`): for every `sync`, and every `mouse` and `key` with a time, where it ends and its time from the start, kept non-decreasing. A 330 MB recording loads and is indexed in about 0.3 s.
- **Playing.** Playing to a moment (`playback_advance`) feeds the client everything up to the last indexed point at or before it, paced by `requestAnimationFrame`. The recorded `mouse` positions the software cursor, so the pointer is visible without a frame from the server. The page offers 1×, 2×, 4× and 8×. The recorded `error` and `disconnect` at the end leave the display as it is. Audio is not played back.
- **Seeking** turns drawing to the canvas off (`set_presenting(false)`), feeds 4 MiB at a time while up to 64 frames wait for their images, and draws the result once: frames on the way are applied to the layers and never presented, and PNG images are decoded in wasm (section 4.4). A moment before what was played starts over from an empty display; seeking forward continues from the current position.

Measurements are in section 7.7.

### 4.11 File transfer (`molcajete-files.js`)

The wasm client handles the protocol side only: it reports filesystem objects (`filesystem`, `undefine`, `get-error` events), sends `get`, hands every incoming `body` and `file` stream to JavaScript as `stream-start`, `stream-data` (each blob in a buffer of its own) and `stream-end`, and leaves acknowledging them to JavaScript (`ack_stream`). Uploads are output streams opened with `put` (into an object, at a path) or `file` (the server's upload directory) on indexes 32–63, apart from the clipboard's and `argv`'s, which start at 0 and are freed right after `end` (libguac accepts indexes below 64); their acknowledgements come back as `stream-ack`.

`molcajete-files.js` does the transfers where the client runs: in the worker, so in worker mode file data never reaches the page, or on the page in main-thread mode. `MolcajeteSession` exposes it (`list`, `upload`, `download`, `cancel_transfer`, and `filesystem` and `transfer` events).

- **Uploads** read the `File` in 1 MiB slices (`File.slice`) and send them as blobs of 16 KiB (21848 bytes of base64, within guacd's instruction limit), with at most 16 unacknowledged. A slow disk or link holds the reading back, a large file is never in memory as a whole, and input instructions never queue behind more than 256 KiB of file data in the tunnel. Two uploads run at once; the rest wait.
- **Downloads** go to a `FileSystemWritableFileStream` of the file the user picked with `showSaveFilePicker`, which the page asks for within the click and posts to the worker (Chromium). Elsewhere they are collected into a `Blob`, folded into a new Blob every 64 MiB so that the browser may move it to disk, and saved through a link at the end; the web UI warns before doing that for files over 1 GiB. Each blob is acknowledged at once while less than 8 MiB waits to be written, later otherwise, so the server never runs further ahead of the disk than that. Data is written in pieces of about 1 MiB.
- **Listings** are the stream index (`application/vnd.glyptodon.guacamole.stream-index+json`): a JSON object from path to mimetype, directories having the index's mimetype. molcajeted adds the size as a mimetype parameter (`application/octet-stream; size=123`), which stock clients ignore.
- **Errors.** molcajeted answers a `get` it cannot serve with `get-error`, and says in `filesystem` whether downloads and uploads are allowed; the panel then offers only what works. Stock guacd sends neither: a refused download is never answered.
- **Cancelling** a download acknowledges it with an error, which ends it on both daemons. An upload is cancelled with `end`, the only way the protocol has; what was sent stays on the server.

Measurements are in section 7.10. A Service Worker that serves the incoming data as a download response would let Firefox and Safari stream to disk as well; it is not implemented.

## 5. Gateway (`molcajete-gateway`)

The gateway is an axum server. It serves the web UI (`web/dist`), the JSON API under `/api` (authentication, connections, folders, users, groups, live sessions and history, recordings, audit log, settings, client reports), the tunnel at `/websocket-tunnel` and `/healthz`. Its own configuration is a TOML file (`gateway.example.toml`); everything managed through the UI is in the database. Configuration keys are listed in [deployment.md](deployment.md).

### 5.1 Tunnel (`tunnel.rs`, `guacd.rs`)

The tunnel follows the stock contract of section 1.2, so guacamole-common-js works against it too.

1. The upgrade request must come from the gateway's own origin and carry a valid session; the query string (`TunnelRequest::from_query`) accepts the short names of the molcajete client (`connection`, `width`, `image`, ...) and the stock `GUAC_*` names.
2. The connection is looked up for the user (section 5.3). An unknown or forbidden connection closes the socket with `RESOURCE_NOT_FOUND`.
3. The connection's stored credentials are decrypted (section 5.5), and the gateway performs the handshake with the connection's guacd (its own host and port, or the default) (`guacd::connect`), answering the highest common protocol version. A failed handshake closes the WebSocket with the mapped status code in the close reason, like the Java web app.
4. The established tunnel registers in the live-session registry (section 5.9), which can end it.
5. `relay` pumps both directions until either side ends or the session is ended:
   - daemon → browser: whole instructions (or records) are batched into one message of up to 64 KiB, sent as soon as the next read would block. Three framings exist (`Framing`), see section 5.2.
   - browser → daemon: text only, parsed with `Limits::STRICT` and forwarded verbatim. Internal pings are answered by the gateway and never reach the daemon.

The handshake, the framing and the tunnel's end are logged (`tunnel established` with `framing`, `tunnel closed` with byte counts); see [observability.md](observability.md).

### 5.2 Binary tunnel negotiation

Each leg is negotiated on its own and falls back to the text protocol when the peer does not know the extension:

- **Browser ⇄ gateway.** The client offers `molcajete-binary-v1, guacamole`. The gateway selects the first it supports; with `[server] binary_tunnel = false`, or an older gateway, it selects `guacamole`. The stock client offers only `guacamole` and gets exactly the stock tunnel.
- **Gateway ⇄ daemon.** molcajeted lists the parameter `molcajete-binary` last in `args`. A gateway whose browser selected the binary subprotocol answers `1` in `connect`, and molcajeted switches to records after `ready`. In every other case the gateway sends an empty value (a connection parameter of that name in the database is never passed on), as does the stock web app for a parameter it has no setting for. Stock guacd never lists the parameter and is sent nothing it does not know.

| Browser | Daemon | Browser ⇄ gateway | Gateway ⇄ daemon | `framing` |
|---|---|---|---|---|
| molcajete-client | molcajeted | binary (records forwarded) | records | `binary` |
| molcajete-client | stock guacd | binary (gateway decodes base64 into blob records) | text | `transcoded` |
| stock guacamole-common-js | molcajeted | text | text | `text` |
| stock guacamole-common-js | stock guacd | text | text | `text` |
| stock web app (Java) | molcajeted | — | text | — |

molcajeted logs `binary` in `connection established`.

**Compression.** WebSocket `permessage-deflate` is not used and must stay that way for these payloads, which are already compressed. tungstenite (under axum 0.8) does not implement the extension, so the gateway never answers the browser's offer with `Sec-WebSocket-Extensions`; a test checks this for every tunnel mode.

### 5.3 Authentication

**Sessions** (`auth.rs`, `db/sessions.rs`). A login sets an opaque random token in an `HttpOnly`, `SameSite=Strict` cookie (`molcajete_session`, `Secure` behind HTTPS); the database stores only its SHA-256. Requests that change state must be JSON and, like the tunnel, come from the gateway's own origin when the browser says where they come from (`same_origin`). API responses are `no-store`.

**Passwords.** Argon2id hashes, computed off the async workers. A login for an unknown username is verified against a dummy hash, so the response time does not reveal which usernames exist. Users created through single sign-on store `!`, which no password matches. At start the gateway imports `[[users]]` (as administrators) and `[[connections]]` from its configuration file and, when there are no users at all, creates `admin` with a random password it logs (`bootstrap.rs`).

**Client addresses** (`proxy.rs`). `X-Real-IP`, else the rightmost `X-Forwarded-For` address that is not itself a trusted proxy, is believed only when the TCP peer is in `[server] trusted_proxies`; otherwise the peer is the client. The `Client` extractor gives the address (for throttling) and its display form (`ClientAddress`, for logs and the audit log).

**Throttling** (`throttle.rs`). Failed password sign-ins are counted in memory in sliding windows per client address (IPv6 per /64) and per username; past a limit `POST /api/auth/login` answers `429` with `Retry-After` before the database or Argon2 is touched. Refused attempts are not counted, so a lockout ends when the window passes. A success forgets only that address's failures for that username, and marks the address as known for the user, which exempts it from the username limit for `known_address_days`: a distributed guess is stopped without locking the user out of their usual addresses. Argon2 verifications (sign-in and password change) take a slot of a semaphore sized to the CPUs; waiting longer than `verification_wait_secs` gives `503`. The single sign-on callback is not throttled: it checks no password, and each callback needs a sign-in started from the same browser. Counters are per replica; a shared store would be needed to throttle across replicas.

**OpenID Connect** (`oidc/mod.rs`). The authorization code flow with PKCE:

```
GET /api/auth/oidc/login     random state, nonce and PKCE verifier
                             → molcajete_oidc_login (keyed by the state's hash, used once, 10 minutes)
                             → state also in an HttpOnly cookie (molcajete_oidc_state)
                             → redirect to the provider
GET /api/auth/oidc/callback  state must match the cookie; exchange the code;
                             verify the ID token (signature, issuer, audience, expiry, nonce, access token hash);
                             find or create the user; issue the usual session cookie
```

- A user is found by the token's `iss` and `sub` (`molcajete_user_identity`). On the first sign-in of an identity the user with the same username is linked, unless it is already linked to another identity at that provider, or a user is created when `auto_create_users` allows it.
- A changed username at the provider renames the user when the new name is free.
- On every sign-in the provider's groups (`groups_claim`) can set group memberships (`sync_groups`, optionally `create_groups` and a `group_prefix`) and the user's own role (`admin_groups`, `connection_manager_groups`; `sync_role` also lowers it). A change that would leave no enabled administrator is skipped with a warning.
- The ID token is kept with the session for RP-initiated logout.

How to set this up is in [administration.md](administration.md).

**API tokens** (`tokens.rs`, `db/tokens.rs`). A token is `mlc_` and 32 random bytes in hex; the database keeps its SHA-256, its first 12 characters, its owner, an expiry and an optional address allowlist (`molcajete_api_token`). A middleware in front of the API authenticates `Authorization: Bearer` requests before anything else: an unknown, expired or revoked token or a disabled owner is 401, an address outside the allowlist (by the same rules as the logged client address) or a request that needs a person (`tokens::restricted`: creating tokens, changing the password, changing or deleting the owner's own user) is 403. Otherwise it drops the `Cookie` header and attaches `TokenAuth` (the token and the owner, loaded now) to the request; the extractors take the owner from there, `same_origin_guard` lets such requests through (a cross-site page cannot set `Authorization` without a CORS preflight, which the gateway never answers), and `ClientAddress` names the token so audit entries show it. `last_used_at` is written at most once a minute, and `token.use` is audited only for the first use, a new address or a use after an hour, with a conditional update so concurrent requests and replicas record it once.

**One-time session links** (`tickets.rs`). `POST /api/connections/{id}/tickets` stores the SHA-256 of a random ticket with the caller, the connection, the token that made it (`ON DELETE CASCADE`, so revoking the token drops its tickets) and display overrides (`molcajete_session_ticket`). The tunnel's `ticket=` replaces the cookie: the ticket row is read and then deleted, and only the request whose `DELETE` affected the row goes on, so a ticket opens one tunnel across all replicas. The user and their access to the connection are checked again at that moment. Such a tunnel skips the same-origin check, which exists to protect the ambient cookie; a ticket is not ambient. The web UI's `#/ticket/<ticket>` page reads `GET /api/tickets/{ticket}` (which does not use the ticket) for the connection's name and protocol and opens the session without a sign-in.

### 5.4 Authorization

**Roles** (`Role`): `user` opens the connections granted to them; `connection_manager` also sees and opens every connection, manages connections and folders and grants them; `admin` can do everything, including users, groups and roles. Roles are stored per user and per group, and a user's effective role is the highest of their own and their groups'. Request extractors enforce them: `CurrentUser` (signed in or an API token's owner, else 401), `ConnectionManager` and `AdminUser` (else 403). The database refuses changes that would leave no enabled administrator (`DbError::LastAdmin`).

**Grants.** Connections live in a tree of folders (`molcajete_connection_group`). A connection or a folder can be granted to a user or a group; a folder grant covers everything below it. One table per (principal, target) pair, as in Guacamole's JDBC schema:

| | connection | folder |
|---|---|---|
| user | `molcajete_connection_permission` | `molcajete_connection_group_permission` |
| group | `molcajete_group_connection_permission` | `molcajete_group_connection_group_permission` |

**The reachability query** (`db/grants.rs`, `REACHABLE`). The connections a user may open are found with one recursive query that every backend runs:

```sql
WITH RECURSIVE
  my_groups (group_id)         -- the user's groups
  granted_folders (id)         -- folders granted to the user or to one of those groups
  folders (id)                 -- those folders and every folder below them (recursive)
  reachable (connection_id)    -- connections granted to the user or their groups,
                               -- plus connections whose parent is in folders
```

The tunnel checks one connection against it (`connection_for_user`) and the connection list is filtered by it (`connection_summaries`); connection managers and administrators skip it.

### 5.5 Database and migrations (`db/`)

One code path serves SQLite, PostgreSQL and MySQL/MariaDB through sqlx's `Any` driver. Queries are written with `?` placeholders and rewritten to `$n` for PostgreSQL (`Database::sql`); every integer column is `BIGINT` and ids are strings, so rows decode the same way everywhere. Migrations are per dialect in `crates/gateway/migrations/{sqlite,postgres,mysql}` and run at start:

| Migration | Adds |
|---|---|
| `20261125000000_init` | users, connections and their parameters, user grants, login sessions |
| `20261126000000_groups_roles_folders` | groups and members, roles, folders, group and folder grants |
| `20261126000100_recordings` | connection attributes, the recording index |
| `20261126000200_oidc` | identities at providers, pending sign-ins |
| `20261127000000_audit_log` | the audit log |
| `20261127000200_connection_history` | the connection history |

Connection attributes (`molcajete_connection_attribute`) hold molcajete's own per-connection switches (`record-sessions`, `record-keys`, `record-exclude-mouse`, `record-molcajete-formats`); unlike parameters they never reach the daemon. Upgrading is described in [deployment.md](deployment.md#upgrading).

**Connection credentials** (`secrets.rs`). The parameters that `sensitive::is_secret` calls secrets (section 5.7: `password`, `passphrase`, `private-key`, their `sftp-`, `gateway-` and `jump-host-` variants, `client-key`; `web/src/connections/secrets.ts` mirrors it and a test keeps the two lists equal) are stored encrypted, so a copy of the database, or a backup, does not give away the credentials of every remote machine.

- *Format.* `enc:v1:<key id>:<base64 of nonce, ciphertext and tag>`. `v1` is AES-256-GCM with a random 96-bit nonce, and the connection id and parameter name as associated data, so a value moved to another row does not decrypt. The key id is the start of a SHA-256 of the key: with several keys configured during a rotation it picks the right one, and with a wrong key the gateway says which key it needs instead of sending garbage to the remote server.
- *Why AES-256-GCM.* It is what reviewers expect, CPUs accelerate it, and RustCrypto's `aes-gcm` is pure Rust (no OpenSSL). Random 96-bit nonces stay safe up to about 2³² values per key, which a gateway does not come near; XChaCha20-Poly1305's longer nonce buys nothing here.
- *Key.* From `MOLCAJETE_SECRET_KEY`, `[secrets] key_file` or `key`, otherwise `<data_dir>/secret.key`, generated on the first start unless the database already holds encrypted values. Retired keys (`previous_key_files`) only decrypt.
- *Where values are encrypted and decrypted.* `Database` holds the keys: inserting a sensitive parameter encrypts it, and reading a connection (`Database::connection`) leaves credentials out and names them in `stored_secrets`, which is what the API returns, to administrators too. Only the tunnel calls `Database::connection_secrets`, right before the guacd handshake. Updating a connection keeps stored credentials it does not mention unless `clear_secrets` names them, so the write-only form and scripts that read and write back a connection do not lose them.
- *Data migration.* At every start `seal_plaintext_secrets` encrypts plain values under a sensitive name in one transaction (left by an older gateway, or under a name added to the list later) and fails if a value is under a key that is not configured. It is Rust rather than SQL because SQL has neither the key nor the cipher. `rotate-key` (`rotate_secrets`) re-encrypts every value not under the current key, also in one transaction.

### 5.6 Recording

**Why in the gateway.** guacd records in the daemon (`recording-path`), teeing its socket into a file. molcajete records in the gateway (`recording.rs`): the gateway sees the stream of stock guacd and molcajeted alike, so one implementation covers both; it owns the database the recordings are indexed in; and molcajeted may run on another host whose disks the gateway and its API cannot reach (for example natively on a Mac for VideoToolbox while the gateway runs in Kubernetes). Stock `recording-*` parameters still go to stock guacd, which records as before.

**Format.** The file is what guacd writes: the daemon → client instruction stream after the handshake, as Guacamole text, with the user's input in between as guacd's recording code writes it: `mouse,x,y,mask,timestamp` and `key,keysym,pressed,timestamp`. guacenc, the stock player (`SessionRecording`) and molcajete's player read it.

- The tunnel's own instructions are left out.
- Blob records of the binary tunnel are written back as base64 `blob` instructions of at most 6048 bytes, as libguac cuts them, whatever records the data came in (`Encoder` holds the rest of a blob until the stream's next data), so a session gives the same file on either tunnel. Input that arrives while the rest of a blob is held is written after that blob, as guacd only ever writes input between whole blobs; its time is still when it arrived. For this the instruction records from molcajeted are cut into instructions.
- Times never go back. guacenc fills the time between two timestamps with frames and, given an earlier one, loops about four billion times. Frames carry the daemon's clock and input is seen on the gateway's, possibly on different hosts, so input is timed on the daemon's clock as of the last `sync` and never before the latest time written; a `sync` timed before input written ahead of it is moved up to that time.
- Mouse by default, keys only on request (`record-exclude-mouse`, `record-keys`, like guacd's `recording-exclude-mouse` and `recording-include-keys`).
- No file transfer: `filesystem`, `undefine`, `get-error` and the `body` and `file` streams with their blobs are left out, so neither downloaded files nor folder listings end up in a file every administrator can read. guacd's recordings lack them too, as it sends them to one user rather than to the session it records. Uploads never were recorded (only `mouse` and `key` are taken from the browser).

**Compatibility over fidelity.** guacenc skips `image/x-molcajete-zstd-v1` images and every video stream, and the stock player draws neither. A recorded session therefore does not announce them (`stock_formats_only` drops them from the handshake), and molcajeted sends PNG and JPEG as it would to a stock client. The cost is the bandwidth and client time those formats save, for recorded connections only. A connection with `record-molcajete-formats` keeps them, and its recordings play in molcajete only.

**Writing, index and retention.** A recording starts after a successful handshake; the file and its row in `molcajete_recording` are created first, and a session whose file cannot be created is refused (`SERVER_ERROR`). Both directions of the relay encode into one `Encoder` (a mutex never held across an await); before each message to the browser and after each message from it the encoded text goes through a bounded channel to a writer thread, so a slow disk slows the tunnel down instead of losing data. When the tunnel closes, the row gets its end time and size; a row left open by a crash is completed from its file at the next start. Retention runs at start and every 10 minutes: by end time (`max_age_days`) and, newest first, deleting everything older than where the sizes add up past `max_total_size`. Every row names its session (`session_id`, the tunnel id), so a session can have several recordings; history rows list them by it, besides their own `recording_id`.

**On demand** (`sessions/record.rs`). *Record sessions* is decided when the tunnel opens, and its recording is teed from the owner's stream. A recording started from the session menu instead needs the screen as it is, which the gateway does not keep (it only relays). Of the three ways considered, asking the daemon to resend the display (stock guacd has no such instruction, and the owner's negotiated formats would stay) and keeping a screen model in the gateway (layers, buffers, every drawing instruction) were ruled out. The recorder joins the session like a share link: its own read-only `select,<guacd_id>` connection to the same daemon, which sends a joining user the whole display first (guacd replays its layers; molcajeted publishes a snapshot, section 6.10). Its handshake announces what the owner's browser announced (kept in `SessionStart::client`) minus molcajeted's formats unless `record-molcajete-formats` is set, so the owner's tunnel keeps its own. The recorder answers every `sync` with a `sync`, as a client must: guacd disconnects a user it hears nothing from, and molcajeted paces each joined user by its acknowledgements. The `mouse` instructions the daemon sends a joined user (the others' pointers) are left out (`Encoder::without_daemon_pointer`); the owner's mouse and keys come from their tunnel as before, which reads the session's `record::Slot` for each message from the browser, so a recording can start or stop at any time. The slot holds the recording that runs and the ids of those made; `POST` reserves it before joining, so two starts cannot race. A recording stops when its owner stops it (the request waits until the file and row are complete), when the owner's `Registration` is dropped, or when the daemon closes the recorder's connection; the recorder then sends `disconnect`, completes the row and writes `recording.stop`. Its row has `started_by`, and the recording endpoints let that user open it; administrators open every recording, and only they delete.

### 5.7 Audit log (`audit.rs`, `sensitive.rs`, `db/audit.rs`)

Sign-ins, sign-outs and every change made through the API, the configuration file or `set-password` are recorded in `molcajete_audit_log`. A handler describes what happened and records it once the change has succeeded:

```rust
audit::record(
    &state.db,
    Entry::new(action::USER_UPDATE)      // one of audit::action
        .by(&admin)                      // or .by_name(typed) / nothing for the gateway
        .from(&address)                  // the ClientAddress extractor, or .source("command line")
        .target(target::USER, &user.id, &user.username)
        .changes(audit::diff(&before, &after))
        .detail("reason", "wrong_password"),
)
.await;
```

- **Diffs.** `audit::diff` compares two JSON snapshots: nested objects flatten to dotted keys (`parameters.hostname`), lists of strings become `added`/`removed`, and a missing field equals `null`. Handlers snapshot what an administrator thinks of as the record (for a user: username, role, disabled, groups by name, grants), so one request that touches several tables gives one entry.
- **Redaction.** `record` passes every entry through `sensitive::redact`, which replaces the value under any secret key at any depth with `[redacted]`. `sensitive::is_secret` is the gateway's one list of secret names (the parameters the web UI edits as passwords or keys, Guacamole's `sftp-`/`gateway-` variants, and the user's password), for anything else that must keep secrets out of logs or responses; connection parameters it names are also encrypted at rest (section 5.5). Connection entries never see a credential's value to begin with: `api::audit_view` puts `stored` or `replaced` in its place.
- **Not in the change's transaction.** One request is several repository calls with their own transactions (a user, then its groups, then its grants), so the entry is written right after them. `record` never fails the request: it writes the log line (target `audit`) first, then the row; a failed insert is logged and counted (`molcajete_gateway_audit_write_failures_total`).
- **Order and paging.** Entry ids are UUIDv7, increasing within the process, so entries of the same millisecond keep their order under `ORDER BY occurred_at DESC, entry_id DESC`. `GET /api/audit` pages with `LIMIT`/`OFFSET` and fetches one row more to answer `hasMore`; the action filter matches an action or everything below a prefix (`action = ? OR action LIKE 'prefix.%'`), and only names of lowercase letters, digits, `_` and `.` are accepted. CSV export streams the same query in pages of 1000.
- **Retention.** A task deletes entries older than `[audit] retention_days` at start and every hour.

Action names are part of the interface (filters, SIEM rules); new ones go into `audit::action::ALL` and [administration.md](administration.md#actions).

### 5.8 Client reports

The page posts the client's once-per-second `stats` to `/api/telemetry/client` (`client_report.rs`) when reports are enabled. The gateway records them in histograms labelled by connection, client mode and kind of client, and logs a summary per batch naming the tunnel and molcajeted's connection id. This is the only view into the browser side of the pipeline; see [observability.md](observability.md).

### 5.9 Live sessions and history (`sessions/`, `db/history.rs`)

Every established tunnel registers in `sessions::Registry` (in `AppState`), in memory, for as long as it relays. `Registry::register` returns a `Registration` that removes the entry when dropped, so a tunnel cannot leave a stale entry however it ends. An entry holds what the tunnel knew at the end of the handshake (`SessionStart`: user, connection, protocol, peer, the guacd host and port, guacd's connection id from `ready`, the recording id), its start time, byte counters the relay adds to (`Traffic`, atomics) and the latest round trip from the page's client reports.

**Ending.** Each entry has a `watch` channel. `Registry::end(id, Ending)` sets it once; `relay` selects on `Registration::ended()` next to both directions, so the tunnel closes at once with `EndReason::status()` (`SESSION_CLOSED` for an administrator, `CLIENT_FORBIDDEN` for policy) and the guacd socket is dropped. The page maps those two statuses, when they arrive as the tunnel's close reason, to *Ended by an administrator* and *Access removed*.

**Policy.** `sessions::policy::enforce` runs after every handler that can take a connection away (users, groups, access, moves and deletes, single sign-on). It re-checks every live session with `connection_for_user` (section 5.4) and ends those that fail, recording `session.end` in the audit log. Checking all live sessions is simpler and safer than deriving who a change affects through groups and folders, and there are few. A tunnel still in its handshake during the change is not registered yet and is not checked.

**History.** When the relay returns, `sessions::history::record` writes the session to `molcajete_connection_history` (status, reason, who ended it, bytes), and a task deletes rows older than `[history] retention_days` at start and every hour. Startup needs no repair: no tunnel outlives the process.

**Joining a live session** (`sessions/share.rs`). Sharing and shadowing look a session up in the registry: a share link by its token (`Registry::link_by_token`), a watched session by its tunnel id. The `LiveSession` has the owner, the connection, `guacd` (host and port of the daemon serving it) and `guacd_id`. The joining tunnel (`tunnel::serve_join`) opens its own guacd connection with `select,<guacd_id>` on that daemon and only `read-only` among the parameters (`true` for watch-only links and administrators), so stock guacd and molcajeted both enforce it; there is no recording and no history row for it, only the audit entry. It registers with `SessionStart::joined` (owner's tunnel id, `JoinVia::Link(id)` or `JoinVia::Shadow`, read-only). `by_guacd_id` returns only owners, since joined tunnels get the same id from `ready`.

- **Links** are kept in the registry next to the sessions: id, owner's tunnel id, SHA-256 of a 256-bit random token, read-only, expiry. The token is returned once. Keeping them in memory ties their lifetime to the session's, which a table could only imitate, and costs nothing at restart since no session outlives the gateway either. A link is the grant: the tunnel request needs only a signed-in user, and `policy::enforce` does not check a joined session against connection grants (only that its user exists and is enabled, and that a watching administrator still is one).
- **Ending.** `Registry::end(id)` ends the session and everyone joined to it (`EndReason::OwnerLeft`, closing with `SUCCESS`); so does the owner's `Registration` being dropped, which also removes the session's links. Stock guacd keeps joined users connected after the owner leaves, so the gateway must do this itself. A joined tunnel registering after its owner's entry is gone ends at once. `revoke_link` removes the link and ends the sessions joined through it (`LinkRevoked`, `CLIENT_FORBIDDEN`).
- **The page** names its tunnel with `page=<random id>`; the session's user manages sharing under `/api/my-sessions/{page}` (`Registry::by_page`), which avoids passing the tunnel id from the wasm client to the page. The page polls it every 4 s while connected to list who joined.

## 6. molcajeted (`molcajete-daemon`)

### 6.1 Session model

molcajeted listens on port 4822 like guacd and runs one tokio task per connection, where guacd forks a process.

- **Handshake** (`handshake.rs`). Compatible with guacd: `args` advertises `VERSION_1_5_0` and the argument names of guacd's plugins for `vnc`, `ssh` and `rdp`, so a connection configured for guacd works unchanged, followed by the jump host parameters (section 6.9); `molcajete-binary` is appended last (section 5.2). `select,$<connection id>` joins a running connection (section 6.10), and other protocols get `UNSUPPORTED`. Client input is parsed with `Limits::STRICT`.
- **Output** (`io.rs`). Everything sent to the client goes through one `Output`: instructions are appended as text, stream data with `Output::blob`, which writes base64 `blob` instructions of at most 6048 bytes for a stock client and blob records for a molcajete gateway. Display, video, audio and clipboard code is the same for both.
- **Input** (`session.rs`). A reader task decodes client instructions into `ClientEvent`s (mouse, key, size, clipboard, `argv`, sync replies with their arrival time and apply time, stream acks), reassembling inbound `clipboard` and `argv` streams. A client may have 64 such streams open at once (libguac's limit) with at most 16 MiB of data buffered across them; a blob beyond that closes its stream with `CLIENT_OVERRUN`. In sessions with file transfer, `put`, `file` and `get` become file events and upload data is passed on as it arrives rather than collected (section 6.11).
- **Loop.** Each protocol runs one `select!` loop over remote updates, client input, the frame timer, the idle timer (`Display::idle_deadline`, for ending an idle video and refining lossy areas without new changes) and a 5 s keep-alive. The frame is built inside `block_in_place` on the connection's task.

### 6.2 Display: diffing and encoding

The server-side display (`display/`) is shared by all three protocols. It keeps two copies of the default layer:

- `pixels`: the screen as the remote protocol reports it (`put`, `put_image`, `fill`, `copy`)
- `sent`: a copy of what the client's layer holds, built by replaying every instruction molcajeted sends (fills, images, copies)

Because the instructions sent are replayed on the copy, every optimisation corrects itself: a wrong guess shows up as a difference in the next comparison and is sent as pixels. A VNC CopyRect only needs the same `copy` applied to the copy, without flushing the source area first. A client simulator that replays the output, and a randomized test of 40 seeds of 60 random steps each (`fuzz_client_always_matches_framebuffer`), check that the client always ends up equal to the framebuffer.

A flush (`Display::flush`) runs these steps over the 64×64 tiles touched since the last frame:

1. **Compare once.** The touched tiles are merged into regions, and the changed column range of every row of a region (first and last differing pixel) is found with one comparison of the two buffers (`RegionDiff`). The later steps read those ranges instead of comparing again, and compare only inside a row's range where they need to know more.
2. **Scroll detection.** Row hashes find vertical moves, sent as `copy` (at least 8 rows).
3. **Move detection in any direction** (dragged windows). Changed regions are grouped by distance (at most 4 tiles apart) and each group is searched on its own. Hashes of the changed 64×64 blocks are looked up while a 64×64 window slides over the client copy, using a 2D rolling hash with two odd multipliers that subtracts the rows and columns leaving the window. Areas up to 512 pixels per axis are searched completely; larger ones in samples of 127×127 pixels every 384 pixels, each sample holding one window position for every way a block can be offset against the tile grid, after which the displacements found are tried on every changed block. Blocks moved by the same offset become one `copy`, front of the movement first. guacd 1.6 slides with multiplier 62 and no subtraction, which leaves part of the block out of the hash and produces many false matches.
   Every copy is checked against the framebuffer before it is sent, so afterwards its rows can only still differ outside it, and only those columns are compared again.
4. **Remaining differences** are cut into changed row bands and column runs within them. A client is slowed by the number of images in a frame more than by their size, so rectangles are joined into their bounding box wherever one image is estimated to be cheaper than two: an image costs 1 KB (`IMAGE_OVERHEAD_BYTES`: headers, the `img`/`blob`/`end` instructions and a decode on the client), the unchanged pixels a join adds 1 byte per 8 pixels, or per 128 in rows that are one colour or repeat the row above. Lines of text that changed over the same columns share an image, since the leading between them is flat. Joined images stay below one megapixel so that a large update is still decoded as several images in parallel.
5. **Encoding** chooses per rectangle (`display/encode.rs`):

| Region | Sent as |
|---|---|
| one colour | `rect` + `cfill` |
| opaque, up to 256 colours, client takes zstd images | zstd image, indexed |
| opaque, more colours, at least 3 of 4 pixels equal to their left neighbour (sampled every fourth row), client takes zstd images | zstd image, RGB (level 3) |
| up to 256 colours, otherwise | palette PNG (1/2/4/8 bit) |
| more colours, otherwise (photographs, gradients) | RGB PNG (DEFLATE level 4, adaptive filter) |
| any transparent pixel; the cursor; larger than the zstd type allows | PNG |
| large, colourful and changing often, client takes JPEG, session not `force-lossless` | JPEG, re-sent losslessly by this table once the area calms down |

The share of repeated pixels separates screen content from photographs well: it is 94 to 97% in windows and pages with more than 256 colours in recorded sessions and 3 to 15% in photographs. Without a row filter Zstandard needs 22 to 74% more bytes than PNG for a photograph and 37 to 45% fewer for those windows, so the rule keeps PNG exactly where its filters earn something. The check costs one look at a quarter of the rows of an image that already failed to fit a palette (0.17 ms on a 5120×512 photograph against 83 ms to encode it as PNG, `display::bench::zstd_images`).

Lossy refinement: each tile has a heat (about updates per second, decaying). Lossy tiles are re-sent losslessly once their heat falls below 0.25; the idle timer makes this happen without a new change. Adjacent cool tiles go out together, in images of up to 512×512 pixels encoded side by side, and at most 4 megapixels per flush: a screen that was busy as a whole (a video that stopped) cools down as a whole, and what is left follows in the next idle flushes. On the 5K benchmark (`display::bench::refine`, 5 encoding threads) refining a whole screen sent as JPEG took one flush of 290 ms with 3360 PNG images (108 ms as zstd images) when every tile was encoded on its own, and takes four flushes of at most 39 ms with 100 images.

### 6.3 Parallel encoding

The rectangles of a frame are encoded side by side on a thread pool shared by all sessions (`display/pool.rs`, rayon): half the available cores, at most 8, set with `--encode-threads` (1 encodes on the session's thread). Regions do not overlap, so encoding only reads the framebuffer; the instructions are then written and the copy updated one rectangle after the other in the original order, so the output is the same instruction for instruction as encoding them one by one. Lossy refinement encodes on the pool too. Every pool thread keeps its own zstd context. The pool is bounded so that one busy session cannot take every core.

The frame is still built on the connection's task, so input, audio and remote updates wait until it is done, only for a shorter time (section 7.5).

### 6.4 Frame pacing and flow control

molcajeted paces frames by what the client can take (`display/pacing.rs`). Remote updates keep arriving between frames and accumulate as differences from the copy, so short-lived intermediate states are never sent. The first change after a quiet period goes out immediately, so input stays responsive.

**The client's need.** The next frame waits for the client's interval, at least the profile's floor and at most 500 ms:

- A client that reports apply times (section 3.6): the median of its last 8 reports (the lower one of an even count), counting only those within 1 s before the last frame. A single slow frame does not move a median, and after a pause the interval starts afresh. The round trip is left to the window of unacknowledged frames.
- Any other client: the exponentially smoothed time until its `sync` reply (the processing lag), counted up to when the reply arrived rather than when the session loop got to it.

**Profiles** (`Pacing`, chosen with `Display::with_pacing`; all protocols ask `Display::next_frame_at`):

| | `Pacing::DESKTOP` (RDP, VNC) | `Pacing::TERMINAL` (SSH) |
|---|---|---|
| Floor | 16 ms (about 60 fps) | 33 ms (about 30 fps) |
| After a frame over 32 KiB | 66 ms, or less once the client has acknowledged it; a client whose acknowledgements take 33 ms or less (smoothed) is sent the next frame while one such frame is still unacknowledged | at least 66 ms (about 15 fps), however fast the acknowledgement |
| Byte budget | 8 MiB/s: a quarter second's worth may go at once, beyond that frames are spaced at the budget's rate (at most 500 ms apart) | none |

On a 4K or 5K screen nearly every frame of scrolling or dragging is larger than 32 KiB, and a fixed pause held those to 15 fps on links that take 60; for desktops the pause therefore lasts only until the link has delivered and the client applied the frame. For terminals more frames under a flood of output show nothing more, so the pause bounds bandwidth (section 7.4). Waiting for every acknowledgement still cost a frame's worth of time on the largest screens: 4K video frames are all over 32 KiB (52 kB on average), and with 21 ms to encode one and 17 ms until its acknowledgement, 26 frames per second went out for 28 from the server. A client that acknowledges quickly has a link and a decoder with room, so one heavy frame may be on its way when the next follows; a client that takes longer than 33 ms gets them one at a time as before (GH-120). The intervals count from when building the last frame began (the flush), so the floor bounds the frame rate whatever a frame takes to encode: a 4K video frame takes 21 ms, and 16 ms counted from the end of that held a desktop to 27 frames per second (GH-120). A video stream has no floor of its own (GH-116).

**Flow control.** Each protocol keeps a window of unacknowledged frames (`MAX_UNACKED_FRAMES`): RDP and VNC send while at most 2 are pending (so up to 3 are in flight), SSH stops at 2. With the window full no update requests go to the remote server; they resume when replies arrive, or with the next 5 s keep-alive. RDP additionally sends a frame at least every 100 ms while PDUs keep arriving (`rdp/pacing.rs`). Where the server ends a frame of its own (the graphics pipeline, Frame Markers), the pending frame goes out at once if the pacing allows, without waiting for the socket to fall silent: on a 2732×1822 desktop one frame of a video arrives as about 18 PDUs and the next follows immediately, and frames that waited for a pause went out by the 100 ms rule, 12 per second for 22 from the server (GH-118). A frame in which everything came whole (graphics pipeline frames, frames a server marked, the cursor) also does not wait behind the PDUs that are ready when it becomes due; only one that may be a part of an update does, as bitmap updates of a server that marks no frames are (GH-120).

**The server's frames (RDP).** A screen update that takes the server longer than our frame interval to send would reach the client in halves, new above and old below. A server that sends surface commands marks its frames with Frame Markers. IronRDP handles them itself and reports nothing about them, but it answers every frame end with a Frame Acknowledge, the only response a fast-path update ever produces, and molcajeted takes that as the end of the frame. Once a server has ended a frame it is known to mark them: from then on graphics that arrive open a frame, and the pending frame waits for its end, at most the same 100 ms, counted from the first change. The cursor is not part of a frame. `molcajeted_remote_frames_total` counts the ends; it stays at zero for a server that sends classic bitmap updates, which carry no marker, and nothing changes for those. Frames of the graphics pipeline (section 6.8) need no hold: its compositor hands over nothing of a frame before the server ended it.

### 6.5 Video mode

Areas with a lot of motion (video playback, animation, 3D) are expensive even as JPEG. molcajeted finds them and sends them as a video stream into a separate layer, using the protocol's `video` instruction unchanged with the framing of section 3.4. Stock clients do not advertise these mimetypes, so they keep getting images.

**Negotiation.** The server picks, in the operator's order (`--video av1,h265,h264`, `none` to disable), the first codec the client advertised and an encoder can produce (`session::enable_video`). Video is not used for `force-lossless` sessions or for SSH.

**Finding the area** (`display/video.rs`). Each 64×64 tile's motion score grows by one per frame in which it had to be sent as pixels and halves every second. Tiles scoring 8 or more are busy: at 30 fps this takes about 0.3 s, at 15 fps about 0.6 s, and fewer than 6 updates per second never get there. Scrolling and window moves are handled by `copy` first, so they are not mistaken for video. Of the busy tiles only the largest group of adjoining ones is looked at, so that something else moving on the screen (a taskbar animation under a player) neither stretches the area nor thins it out. Video starts when the bounding box of that group has at least 6 tiles, is at least 192 pixels (3 tiles) wide and high, busy tiles cover at least half of it, and it holds more than 256 colours. A strip such as a taskbar animation is cheap as images, and hardware encoders refuse frames that flat (VideoToolbox fails on 1216×96).

**Large areas refreshed slowly.** A remote server sends a large picture only a few times per second: Windows over RDP refreshed a 7.3 MP player on a 5120×2776 desktop about 2.4 times per second, in pieces. Sent that often a tile settles at a score near 3.5 and never reaches 8, while every refresh is megapixels of JPEG (up to 37 images per second in that session, which Safari was up to 4 s behind on). An area of 128 busy tiles or more (about 0.5 MP) therefore counts tiles from a score of 2, which about 1.4 updates per second hold, and becomes a video once it has met the conditions above for 1 s without a pause. The second keeps out a page that is painted a few times while it loads: three paints score 3 for a moment and 1.5 a second later. Smaller areas keep the score of 8, so a clock or a progress bar stays lossless. This changes how the area is sent (one video frame instead of dozens of images, at the video's bitrate), not how often the remote server refreshes it.

**The video layer.** Layer 1 is sized to the area (rounded up to multiples of 16) and placed over it, and a `video` stream is opened on it. The frame given to the encoder (the padding repeating the last column and row) is kept between frames and updated only in the tiles touched since the last flush, one pixel row at a time, so a small change costs a few rows and a frame in which the same pixels were drawn again is not encoded. The first frame is a key frame carrying the codec configuration. Tiles inside the area are left out of the image diff.

**Keeping the default layer honest.** While the video plays, the video area of `sent` is not touched, so the copy keeps matching what the default layer holds beneath the video. When the area has not changed for 1 s, the layer is removed (`end`, `dispose`) and in the same frame the area is compared with the copy and updated losslessly, so no lossy content stays on screen and nothing stale shows through.

**Moving and growing areas.** When the busy area extends beyond the video, the video restarts over the new area once it has extended beyond it for 8 frames and then not grown for 10 more (flickering edges do not count as growth), or after 60 frames even if it keeps growing; meanwhile the part outside goes as images. The new area covers everything the busy area covered meanwhile and, where it grew from the old video, is extended by as much again on those sides, so an opening or resizing window restarts the stream once or twice instead of every few frames (simulating a window growing from 832×960 to 1280×2176 over 3 s: 2 starts instead of 8).

**Codecs and bitrate.** The target is area × 30 fps × 0.1 bits per pixel for H.264, 0.7× that for H.265 and 0.6× for AV1, between 300 kbps and 16 Mbps. Rate control spends that per frame at a nominal 30 frames per second, so content that changes more slowly uses fewer bits per second and content the remote server updates faster uses more. Frames are not held to that rate: they are paced like any other frame and follow the remote server's own. A limit of 30 per second was tried and merged frames: a Windows host playing a 30 fps video ends 34 to 38 frames per second (the video's frames and smaller updates), and whenever two of them fell into one interval a frame of the video was shown a step late (GH-112, GH-116). `molcajeted_frame_interval_seconds` shows how evenly frames follow each other. Software encoders are slow on large frames, so an area beyond what the best backend encodes in real time uses the next codec (`realtime_pixels`: SVT-AV1 up to 1280×736, about 25 ms per 720p frame; x265 up to 1920×1088). Hardware encoders and software H.264 have no limit.

**Frames the encoder leaves out.** VideoToolbox with low-latency rate control holds the bitrate by skipping frames when the content costs more than it allows: a terminal repainting random colours 20 times per second lost four frames in five at the bitrate of its area. A skipped frame is not a failure: the stream goes on with the next frame and no key frame is forced (`molcajeted_video_frames_skipped_total`). When a quarter or more of 30 frames were skipped, the bitrate is doubled, up to three times and never beyond 16 Mbps: the stream ends and a new one starts on the same layer with a new encoder and a key frame (`molcajeted_video_bitrate_raises_total`). The raise lasts as long as that video.

**Encoders** (`video/`). Backends in order of preference for a codec:

1. hardware: VideoToolbox on macOS (H.265, H.264), and through ffmpeg NVENC, QSV, VA-API and AMF when `ffmpeg -encoders` lists them
2. software through ffmpeg: libsvtav1, libx265, libx264
3. the built-in OpenH264 encoder (cargo feature `openh264`, needs a C++ compiler)

Each candidate must pass a trial encode at start. All backends emit one access unit per frame, without lookahead or reordering.

- **Colour conversion** (`video/yuv.rs`). molcajeted converts RGBA to 4:2:0 itself for every backend, BT.709 limited range (signalled in the bitstream), with the same fixed-point arithmetic everywhere; on aarch64 with NEON 16 pixels at a time, bit-identical to the scalar code that handles the rest of a row and other architectures. Output is planar (`yuv420p`) or NV12 depending on the encoder.
- **ffmpeg** (`video/ffmpeg.rs`) runs as a child process per encoder, driven over pipes and never linked, so the GPL x264/x265 pose no licensing problem. Raw 4:2:0 frames go to stdin; packets come back framed by the AVI muxer (`video/avi.rs`), because with a raw elementary stream the end of a frame is only known when the next one starts, which would add a frame of delay. ffmpeg cannot be asked for a key frame over a raw pipe, so a forced key frame restarts the process.
- **VideoToolbox** (`video/videotoolbox.rs`) is called directly instead of through ffmpeg's `*_videotoolbox`, because ffmpeg does not wait for VideoToolbox's asynchronous output and each packet would come one frame late. After each frame `VTCompressionSessionCompleteFrames` waits for its packet. The conversion writes straight into the planes of the NV12 pixel buffer taken from the session's pool. Length-prefixed NAL units are converted to Annex B with the parameter sets in front of key frames. The session requires the hardware encoder.

**Failures.** An encoder error ends video mode and falls back to images; after three failures the session stops trying. A client rejecting the stream or reporting a decoder error with `ack` turns video off for the session.

**Full-screen video.** For a screen that moves most of the time (games, 3D viewports, CAD, video editing, a browser playing full-screen video), busy areas grow, shrink and restart with the content, the rest of the screen goes as images, and every pause sends the area losslessly and waits for the thresholds again. The user can switch the session to the whole screen from the session menu (section 4.8, `Display::set_video_mode`). The video then covers the display (rounded up to multiples of 16), without the tile, side, density, colour or sustained-motion conditions:

- It starts with the first frame in which at least 6 tiles differ from what the client shows (compared against the copy), so a caret or a clock does not start it, and a screen that moves when the switch is turned on starts it in the next frame.
- It restarts only when the display is resized, never because the busy area moved. Nothing goes through the image diff, scroll or move detection while it plays: the encoder's motion search covers scrolling and moves. The frame given to the encoder is still updated tile by tile, and a frame without a changed tile is not encoded.
- It ends after 1 s without a frame that changed 6 tiles or more. Smaller changes during that second still go into the video, but do not keep it going, so a blinking caret does not keep a settled screen lossy. As for any video, the screen is then sent losslessly in the same frame, and the next motion restarts the stream with a key frame. Keeping the stream open and letting the encoder refine the still picture would save those key frames, at the cost of a lossy screen; it is not done.
- It falls back to busy areas, and logs `full-screen video falls back to busy areas` with the reason, when no codec encodes the display in real time (`realtime_pixels`: software AV1 and H.265 on a large display; a later resize to a size that fits goes full screen again), or when an encoder fails (a hardware encoder refusing the frame size, for example), after which the switch shows off again. `force-lossless`, SSH and clients without a codec in common get no switch at all.

Limits: the bitrate is clamped like any video's, so a 5120×2880 screen gets 16 Mbps, about a third of the per-pixel budget of a small area. The encoder input is 4:2:0, which blurs coloured and thin text while the screen moves. Hardware encoders have maximum frame sizes (often around 4096 for H.264), and consumer GPUs limit the encoding sessions that run at once, which caps how many full-screen sessions one host serves. Frames follow the remote server's, as for busy areas. A user who joined gets their own encoder as before, in the mode they chose. Recordings keep announcing stock formats unless `record-molcajete-formats` is set, so they get images.

### 6.6 VNC (`vnc/`)

An RFB client of its own (`vnc/rfb`): RFB 3.3/3.7/3.8, None and VNC authentication, Raw/CopyRect/Hextile/ZRLE, the Cursor/DesktopSize/ExtendedDesktopSize/LastRect pseudo-encodings, clipboard in both directions and remote resize. It always asks for 32 bpp little-endian true colour, so every decoder handles one pixel layout. CopyRect becomes a `copy` on both the display and its client copy. A server is not trusted with memory: its framebuffer may have at most 8192 × 8192 pixels (the largest display a client may ask for, in any shape), which bounds every rectangle, and pixel data, ZRLE payloads and clipboard text are read into buffers that grow as the data arrives rather than to the length the server announces. Parameters (`hostname`, `encodings`, `read-only`, `disable-copy`, `force-lossless`, ...) follow guacd's VNC plugin.

### 6.7 SSH (`ssh/`)

- **Connection** with russh: password, private key with passphrase, keyboard-interactive, asking with `required` when a value is missing, and `host-key` pinning.
- **Terminal.** alacritty_terminal keeps an xterm-compatible screen (256 colours and true colour, alternate screen, scrollback, bracketed paste, SGR mouse). The default terminal type is `xterm-256color`.
- **Rendering.** Cells are rasterised on the server with ab_glyph, and only the row spans that differ from the previous frame go into the display; scroll detection and encoding are up to the shared display, so any Guacamole client works. DejaVu Sans Mono is bundled, with the bundled NanumGothicCoding as fallback for Hangul, Han and Kana; `font-path` and system fonts are further fallbacks. Double-width and box-drawing characters are drawn to the cell grid.
- **Selection and clipboard.** Drag and double-click select; releasing sends the text as a `clipboard` stream; a click that clears the selection sends `selection,0`. Ctrl+Shift+V and right-click paste. Wheel scrolls back.
- **Size.** The terminal has as many cells as fit the display. A `size` from the client is always followed by the end of a frame, also when the resize changed no pixel (a terminal that only got shorter below its last line), because the client applies a new size at the end of a frame. `font-size` sent as `argv` during the session rescales the loaded faces (`Fonts::set_size`), keeps the display's size and gives the terminal the cells that fit now.
- **Links** (`ssh/link.rs`). The cell under the pointer is checked for an OSC 8 hyperlink or an `http://`, `https://` or `mailto:` URL in the line's text, followed across wrapped lines (at most 16 each way) and without trailing punctuation or unbalanced closing brackets. Links are looked for when the pointer moves, a modifier changes and before every frame; not while the application takes mouse events (Shift bypasses it) or a button is held. The link is underlined, the pointer becomes a hand, and the client gets `link,<url>` or a bare `link`.

Parameter names match guacd's SSH plugin.

### 6.8 RDP (`rdp/`)

- **Protocol** through IronRDP (connector, session, graphics, egfx, cliprdr, displaycontrol, rdpsnd, rdpdr) with rustls. Security: TLS and NLA (CredSSP/NTLM); certificates are verified unless `ignore-cert` is set. IronRDP does not implement standard RDP security (RC4), so `security=rdp` (and `vmconnect`) are refused.
- **Graphics.** Two paths lead into the display: the graphics pipeline with servers that have it (next item), bitmap updates with the others (xrdp) and when the pipeline is not offered. Bitmap updates go into the display per changed rectangle, copied once straight from IronRDP's decoded image: `Display::put_image` takes a `SourceImage` (data, stride, `ChannelOrder`) and writes opaque RGBA in the same pass (section 7.8). Pointer bitmaps become the cursor. IronRDP does not pass drawing orders such as ScrBlt through, so RDP gets no `copy` hints from the server; the shared display's scroll and move detection find them.
- **Graphics pipeline** (`rdp/gfx.rs`, [MS-RDPEGFX]). Over bitmap updates a Windows host repaints a large changing area in pieces and only a few times per second (a 7.3 MP video area on a 5120×2776 desktop: about 2.4 times per second, 3.3–4.2 MB/s of planar bitmaps, the pieces spread over several of our frames). The pipeline is a dynamic channel on which the server draws into surfaces with newer codecs (H.264 as AVC420 and AVC444, progressive RemoteFX, ClearCodec, planar) and marks where each frame ends. It is offered to every server unless `disable-gfx` is set (guacd's name), the session is `force-lossless` (these codecs are lossy) or a `color-depth` below 32 was asked for (the pipeline has no other). A server without it never opens the channel and sends bitmap updates as before.
  - *Who does what.* IronRDP's `GraphicsPipelineClient` advertises the capability sets (versions 10.7 down to 8), decodes every codec except H.264, keeps the surfaces and the bitmap cache, applies fills, copies and cache commands, places each surface where it is mapped on the output, and acknowledges every frame right after decoding it (`queueDepth` unavailable), which is what makes the server send the next one. `GraphicsChannel` wraps that client as the channel. IronRDP's active stage would otherwise find the client by its type and copy every frame into its decoded image, from where the session would copy it again; wrapped, the session takes the changed output regions itself and writes them into the display with `Display::put`, one copy.
  - *Frames.* The client releases a frame's regions only when the server ends the frame, so the display never holds half of one, and each release marks the pending frame dirty for pacing (section 6.4). A new output size (`ResetGraphics`) resizes the display if the reactivation sequence has not already done so; the display keeps its content until the server has redrawn it.
  - *H.264* (`video/decode.rs`). IronRDP leaves the decoder to its user: VideoToolbox on macOS (`video/videotoolbox/decode.rs`), OpenH264 through IronRDP's wrapper with the cargo feature `openh264` elsewhere. The decoder returns the picture's planes, IronRDP converts them with the pipeline's full-range BT.709 matrix and combines the two pictures of AVC444. Each access unit is decoded synchronously, since AVC444 pairs every picture with the sub-stream it came from; the VideoToolbox session is created from the stream's SPS and PPS, replaced when they change, and asks for no output format, so the samples come back as they are in the stream. Without a decoder only version 8 is advertised and the server uses the other codecs.
  - *Where it runs.* The channel hands every message to a thread of its own (`rdp-gfx`), which runs the client: a full-screen AVC444 frame takes 17 ms to decode at 3840×1919, and on the session loop that came on top of encoding the frame for the browser (16 ms as video), so the loop was busy for the whole time between two frames of the server and sent about 16 per second. The thread sends back, in the order of the messages, the replies for the server (the session writes them, the acknowledgement of each frame among them) and the frames a message completed (the session draws them and ends its own frame there, section 6.4). Results that are ready together are taken together, so frames the session could not keep up with are merged, not queued. A channel the server closes and opens again is a new one with a new thread; what the old one still had to say is dropped.
  - *Cost.* IronRDP converts a decoded picture to RGBA one pixel at a time: 5 ms for 1920×1080 and 37 ms for 5120×2776 on an Apple M4, whatever part of the picture changed. Its compositor holds at most 256 MiB of surfaces and cache, about four surfaces of that size.
  - *Seeing it.* `RDP graphics pipeline in use` is logged once with the version and whether H.264 was negotiated, `RDP graphics pipeline summary` at the end with the frames, the decode time per frame and the pixels decoded per codec; the metrics are `molcajeted_rdp_gfx_*` (observability.md).
- **No bulk compression.** IronRDP's reactivation sequence rejects compressed Share Data PDUs, which xrdp sends after a resize once compression is negotiated, and the bulk decompressor is private to IronRDP's active stage, so compression is not negotiated.
- **Input.** Modifiers, function keys and shortcuts are sent as scancodes, other characters (Hangul included) as Unicode events; Hangul and Hanja as scancodes 0x72 and 0x71 (section 4.7). Mouse and text clipboard in both directions (UTF-16).
- **Display size and scale.** Display Control ([MS-RDPEDISP]) is offered to every server. A new size (`size` while resizing follows the window) and a new scale (`argv` `dpi`) both go out as a monitor layout: the size the desktop has or is about to get, with `DesktopScaleFactor` from the DPI (100–500 %) and `DeviceScaleFactor` at Windows' plateaus 100, 140 and 180 %. Requests are applied after 500 ms without another one. The server opens the channel when it likes, and "not yet" and "never" look the same, so the channel counts as missing when the server did not join `drdynvc`, or 10 s after the session became active (`display_control::WAIT`); molcajeted then sends `display-control,0`, and `display-control,1` if the channel opens later. Windows applies a new scale to applications that support per-monitor DPI; others keep their size until they restart.
- **Audio** (`rdp/audio.rs`). The RDPSND static channel and the `AUDIO_PLAYBACK_DVC` dynamic channel are registered, together with a device redirection channel (RDPDR), because Windows only starts RDPSND once RDPDR has completed its capability exchange; without a drive (section 6.11) it redirects nothing. Among the server's formats only decodable ones are accepted (PCM 8/16-bit, G.711, IMA/MS ADPCM), preferring 16-bit, 44.1 or 48 kHz, stereo. Sound goes out right away, independent of frame pacing, as `audio/L16;rate=R,channels=C` (8-bit if the client only takes L8). Without client audio or with `disable-audio` the channels are not created, so the remote session plays nothing. A rejected stream stops audio for the session.

### 6.9 SSH jump hosts (`jump/`)

Any of the three protocols can reach its server through one to four SSH jump hosts (`jump-hosts`). `Chain::connect` opens a TCP connection to the first hop, runs an SSH session over it (russh, with the SSH plugin's handshake, host key pinning and authentication from `ssh/client.rs`), and asks that hop for a `direct-tcpip` channel to the next one; the next SSH session runs inside that channel, and so on. The last hop's channel goes to the target. Without jump hosts it is a plain TCP connection.

- **One stream type.** `Upstream` is either the TCP socket or the last channel, holding the SSH session of every hop for as long as it lives. The RFB client takes it split into boxed halves, the RDP connector wraps it in `CountingStream` and TLS, and the SSH plugin runs its own handshake over it, so no protocol code knows whether a tunnel is in between. Through a tunnel the RDP client address sent to the server is unspecified, since the TCP connection to the target belongs to the last hop.
- **Parameters.** Shared values (`jump-host-username`, `-password`, `-private-key`, `-passphrase`, `-host-key`) apply to every hop without its own; `jump-host-<n>-password`, `-private-key`, `-passphrase` and `-host-key` apply to hop `n` only, and a hop's own password or key replaces both shared ones. Every name is in `args` for every protocol (`jump::ARGS`), so the per-hop names are fixed in number: four hops.
- **Missing values.** A hop without a username or credentials is asked for with `required` before connecting: `jump-host-username`, and `jump-host-password` (one hop) or `jump-host-<n>-password` (several), together with the SSH target's own `username`/`password` in an SSH session.
- **Errors** name the hop, as `jump host <n> (<user>@<host>:<port>)`: unreachable (`UPSTREAM_NOT_FOUND`), a host key that does not match its pin (`UPSTREAM_ERROR`), a failed login (`CLIENT_UNAUTHORIZED`), or a hop that cannot open a channel to the next hop or to the target, with the reason it gave (`UPSTREAM_NOT_FOUND`).
- Each hop sends an SSH keepalive every 30 s. A chain is opened per session; sharing an open chain between sessions to the same network is not implemented.

### 6.10 Joining a connection (`share.rs`)

`select,$<connection id>` joins a connection molcajeted is serving, as with guacd. The handshake offers the owner's parameter names; of the joining user's values only `read-only` is used, and `ready` answers the connection's id. Unknown ids get `RESOURCE_NOT_FOUND`.

**One display per user.** A joined user runs in its own task with its own `Display`: its own client copy (`sent`), frame pacing, window of unacknowledged frames, idle refinement and video state. A slow user is therefore paced by its own acknowledgements and never holds up the owner or other users; guacd instead broadcasts one frame to all users and paces by the slowest.

**The shared screen.** Each connection has a `Shared` in a process-wide map (weak references, removed when the owner's session ends). While someone has joined, the owner's `Display::flush` first copies the tiles it is about to compare into the shared screen and sets a bit per tile for every joined user; the cursor goes along with `set_cursor`. Each user's task copies its marked tiles into its own framebuffer (`put_image`) when woken and sends a frame when its pacing allows. The marks coalesce, so a user who falls behind costs one bit per tile, not a queue, and gets the newest pixels when it catches up. With nobody joined nothing is copied and the shared screen is freed; the cost while sharing is one extra copy of the changed tiles per owner frame and a framebuffer per connection plus one per user.

**The first frame.** A new user needs the whole screen even when nothing changes. It sets a snapshot flag and pushes an empty `sync` into the owner's input channel; every protocol hands `sync` to `Display::ack`, which ignores a timestamp it never sent, and there publishes the whole framebuffer when the flag is set. The owner's next flush does the same, for a user who joined before the display existed.

**Input.** Mouse, keys and clipboard of users who are not read-only are pushed into the owner's input channel (`Input::from_viewer`) and handled by the protocol like the owner's own; the clipboard is re-sent as a stream of the user's own (negative stream ids), whose acknowledgements went to that user. `size` and `argv` stay with the user: the owner decides the remote size and settings. A connection that is itself read-only drops everyone's input.

**Video** follows the owner's setup: if the owner's display may use video, each user gets video mode with its own encoder and codec from its own handshake. The simplest correct choice, at the cost of one encoder per user watching a busy area (VideoToolbox and hardware encoders have a limited number of sessions; past it the user falls back to images). Sharing one stream would require every user to decode the same codec at the same pace.

**Hooks.** The protocols do not know about joins: a task-local (`share::current`) set around the owner's protocol run lets `Display::with_protocol` and `session::spawn_reader` attach the display and the input channel. When the owner's session ends, every joined user gets `disconnect`.

Not supported: audio and the remote clipboard for joined users, showing other users' pointers, and frames for joined users more often than the owner's display builds them (the shared screen is updated at the owner's flush).

### 6.11 File transfer (`files/`)

A session with file transfer exposes one Guacamole filesystem object (index 0). Its streams are served by a task of its own (`files/task.rs`), which the session loop feeds through a bounded channel and whose replies it writes like any other output, so disk and SFTP round trips never hold up frames or input. The task:

- answers `get` on a directory with a `body` stream of the JSON index, and on a file with the file, reporting what fails with `get-error` (missing, permission denied, downloads disabled, over `max-file-size`, too many transfers);
- keeps up to 512 KiB of a download unacknowledged, in blobs of one instruction (6048 bytes) or one record (64 KiB) each, so that each blob gets one `ack`, as stock clients send them. The first `ack` accepts the stream. A fast link stays busy, and on a slow one screen updates do not queue behind more than that; a stock client that acknowledges every blob works unchanged;
- writes an upload's blob before acknowledging it, so the client's window of unacknowledged blobs is the only buffering. An upload that ends early (bad data, over `max-file-size`, a full queue) or is still open when the session ends is deleted.

Paths from the client are split on `/` and `\`; `..` and NUL are refused rather than resolved, so no path leaves the root.

**SSH: SFTP over the terminal's connection** (`enable-sftp`, `sftp-root-directory`, `sftp-disable-download`, `sftp-disable-upload`, guacd's names). Once authenticated, molcajeted opens a second session channel on the same russh connection, requests the `sftp` subsystem and runs russh-sftp over it (pipelined reads and writes). The root only sets where browsing starts, as in guacd: the server's permissions decide the rest. `file` uploads go to the home directory if it lies below the root, otherwise to the root. A server without SFTP leaves the session without a filesystem.

**RDP: the drive** (`enable-drive`, `drive-path`, `create-drive-path`, `drive-name`, `disable-download`, `disable-upload`). The directory is opened (and with `create-drive-path` its last component created, mode 0700) before connecting, and announced on RDPDR as a filesystem device served by IronRDP's native backend for the platform (`ironrdp-rdpdr-native`). On Linux and macOS, `NixRdpdrBackend` resolves the server's paths one component at a time below the directory without following symbolic links, and answers directory queries only in `FileBothDirectoryInformation`, which Windows uses; `rdp/drive.rs` wraps it to answer the classes xrdp asks for (`FileDirectoryInformation`, and the full and names-only classes). On Windows, `WindowsRdpdrBackend` opens paths relative to the directory's handle without following reparse points (symbolic links, junctions) and answers every class itself; it completes waiting byte-range locks and directory change notifications on worker threads, which the session loop collects every 50 ms (`poll_deferred_messages`), so Explorer on the server refreshes when a file is uploaded. On both, the wrapper turns the `/` separators xrdp sends into `\`. The browser side of the same directory resolves paths one component at a time too and refuses symbolic links; on Windows it also refuses components Windows would not read as a plain name (a drive prefix, `:` streams, device names such as `CON`, trailing dots and spaces), since pushing `C:` onto a path replaces it. A directory that cannot be opened leaves the session without a drive.

`max-file-size` (MiB, a molcajeted extension) applies to both directions; guacd has no such limit.

## 7. Verification

### 7.1 Tests

- **Unit tests.** Protocol: every split point, code point lengths, limits, invalid UTF-8, binary records at every split point, the zstd image decoder on damaged input. Gateway: the guacd handshake against a fake guacd, error mapping, query parsing, tunnels of all four client/daemon combinations through a real WebSocket, migrations and queries on every database backend, the OIDC flow against a fake provider, recording encoding (file transfers left out), retention and permissions. Daemon: the handshake with and without the binary parameter, display merging, copy ordering and blob splitting, the same frame as text and as records, PNG and zstd round trips through the client's decoder, the choice between zstd and PNG, the client simulator and randomized display test, pacing, YUV conversion, the RFB client, RDP settings and keyboard mapping, jump host chains against in-process russh bastions (one and two hops, host key mismatch, failed login, unreachable hops and targets), file transfer against a drive directory (uploads, listings, the download window, refused paths and directions, the size limit, deleting incomplete uploads) and the RDP drive backend's directory classes and separators. Client logic: path tessellation, keysyms, key release pairing, wheel, PCM.
- **Fuzzing** (`fuzz/`, `cargo fuzz` on nightly; `scripts/fuzz.sh [seconds] [target ...]`). Every parser that reads bytes from a party that should not be trusted has a libFuzzer target:

  | Target | Code | Reads | Checks besides not panicking |
  |---|---|---|---|
  | `parser` | `Parser`, `ServerInstruction::decode` | browser → gateway and molcajeted (strict limits), server → client (lenient) | the same bytes whole and cut into pieces give the same instructions and the same end; each instruction survives encoding and parsing again |
  | `records` | `RecordReader`, `records` | molcajeted → gateway, gateway → client | a stream cut into pieces gives the records of the whole message |
  | `zimage` | `zimage::Decoder` | server → client | a decoded image has the announced size, at most `MAX_PIXELS`, is opaque, and decodes the same again with the same decoder |
  | `video_frames` | `video::FrameReader` | server → client | whole and in pieces give the same frames |
  | `handshake` | `handshake::negotiate`, `session::InputDecoder` | client → molcajeted | |
  | `rfb` | `vnc/rfb`: handshake, server messages in every encoding, ZRLE tiles | VNC server → molcajeted | decoded pixels fill their rectangle |
  | `terminal` | `ssh/terminal.rs` (alacritty_terminal), `ssh/render.rs`, links and selection | SSH server → molcajeted | |

  The targets in molcajeted call `molcajete_daemon::fuzzing`, a hidden module of the daemon's library; to read from memory, `GuacReader`, `GuacWriter` and `RfbReader` are generic over their stream. An allocation above 512 MiB counts as a crash, so a length field that allocates before its data arrives is found too. `fuzz/examples/seeds.rs` writes the committed seeds (`fuzz/seeds`, about 100 KB) with the protocol's own encoders; what a run finds goes to `fuzz/corpus`, which is not committed. A crash leaves its input in `fuzz/artifacts/<target>/`; its fix gets a unit test in the crate that owns the code. `.github/workflows/fuzz.yml` runs each target for five minutes every week, keeping the corpus in the Actions cache and uploading crashing inputs. IronRDP's decoders are left to IronRDP's own fuzzing. `.github/workflows/ci.yml` runs formatting, clippy and the unit tests on every pull request.
- **Browser tests** (`tests/browser`, headless Chromium, `npm test`):
  - `renderer.test.mjs`: pixels of the WebGL2 renderer against what the HTML-canvas reference client produces, including zstd images at every index depth, through clips, transforms and composite modes, damaged images, and frames where zstd, PNG and JPEG images overlap.
  - `input.test.mjs`: keyboard and IME handling of the page-side client (Korean composition driven over CDP), keys and held modifiers sent from the page, and touch in terminals (drag, long press, tap and pinch driven over CDP), in worker and main-thread mode.
  - `binary.test.mjs`: a stand-in gateway sends one frame (a PNG larger than a record, a PNG with transparency, a JPEG, zstd images, PCM audio and an H.264 stream cut at arbitrary points) as records to a client offering the binary subprotocol and as base64 to one that does not, in worker and main-thread mode; all four must show the same pixels, and `sync` replies must carry the apply time only when asked.
  - `audio.test.mjs`: PCM streams under a browser's autoplay policy, in worker and main-thread mode: without a gesture the page is told that sound waits and a click lets it play; a context made in the click that opened the session plays the stream at once.
  - `playback.test.mjs`: seeking and playing recordings; a frame decoded by the browser and in wasm must give the same pixels.
  - `video.test.mjs`: real H.264 and AV1 bitstreams made with ffmpeg, framed as molcajeted frames them, checked on the composited canvas, including the return to the default layer after `dispose`. Linux Chromium has no H.265 decoder; Windows Chrome 158 (headless, hardware decoding) decoded all three codecs.
- **End to end.** VNC (x11vnc), SSH (OpenSSH) and RDP (xrdp and Windows 11) through the gateway, with stock guacd 1.6 and molcajeted, in worker and main-thread mode: display, key input with Shift combinations, Hangul, mouse, scrolling, selection and clipboard in both directions, resizing, error mapping (wrong password, closed port, host-key mismatch). On the stock guacd path Hangul on the SSH command line was garbled; molcajeted shows it correctly because of its `xterm-256color` default.
- **Sharing.** Daemon unit tests (`share/tests.rs`): a joined user starts with the whole screen and follows changes, sizes and the cursor pixel for pixel through the client simulator; a user whose client never reads or acknowledges gets exactly its window of frames while the owner builds frames on; read-only users' input is dropped, others' mouse, keys and clipboard reach the owner's input (with the clipboard's acks going to the user); everyone gets `disconnect` when the owner leaves. Gateway tests (`tests/sharing.rs`) against a joinable fake guacd: links, `read-only` in the join, revoking, expiry, shadowing, the owner leaving or being ended, `[sharing]` switches, audit entries. End to end with headless Chrome through the gateway, molcajeted and stock guacd 1.6: SSH, VNC and RDP joined by a second browser show the same screen; a view-only viewer's typing does not reach the shell and an interactive viewer's does (molcajeted and stock guacd); revoking a link, a revoked link and the owner leaving show their messages; an administrator watching from *Active sessions* is announced to the owner. With the owner's SSH session printing continuously (30 frames per second), throttling the viewer to 64 KB/s and 20× CPU slowdown left the owner at 29–30 fps (219 KB/s) while the viewer received 44 KB/s.
- **File transfer** (`tests/browser/files.e2e.mjs`, by hand against a gateway, molcajeted and the SSH target): through `MolcajeteSession` in worker and main-thread mode, on the binary and the text tunnel: upload, listing with sizes, downloads through memory and to a file with the same bytes, blocked directions refused; with `LARGE=<GiB>` the measurements of section 7.10. The RDP drive was checked with an xrdp container given FUSE (`--device /dev/fuse --cap-add SYS_ADMIN`, `fuse` installed): the drive shows in `~/thinclient_drives`, a file uploaded in the browser reads the same there, and files saved, renamed and folders made remotely appear in the directory and can be downloaded; without FUSE xrdp reports the drive (`Detected remote drive`) but cannot mount it. Windows was not tested. Dropping files on the session and the panel were checked in the web UI for SSH and RDP.
- **Jump hosts** (`docker/jump/compose.yml`: targets on an internal network, OpenSSH bastions bridging to it). VNC, SSH and RDP through one and two hops, from the browser through the gateway and with a bare Guacamole client against molcajeted: the session works; a missing jump host password is asked for; a wrong pinned key, a wrong password and an unreachable second hop end the session with an error naming the hop.
- **Audio.** A 440 Hz sine played in an xrdp session arrives as `audio/L16;rate=44100,channels=2` (about 177 KB/s, on par with stock guacd), and the samples the browser schedules are at 440.0 Hz in both client modes.
- **Recordings.** Real VNC, SSH and RDP sessions through stock guacd 1.6 and molcajeted (the latter on the binary tunnel) were encoded with guacenc 1.6 without errors, and their last frames show the session's screen; a session recorded with `record-molcajete-formats` encodes too but is nearly black there.
- **Settling.** After scrolling an xterm continuously and stopping, the client canvas was compared pixel by pixel with the X server's screen (`xwd`). The molcajeted path showed zero differences in every run. The stock guacd 1.6 path, with either client, sometimes left hundreds to thousands of stale pixels where a window had closed; that is guacd's behaviour.

### 7.2 The client against the stock client (`tests/browser/replay-bench.mjs`)

Live comparisons are unreliable: guacd adapts the number of frames to how fast the client answers `sync`, so two clients do not even receive the same input. Instead, 12 seconds of the protocol stream of a real session (an x11vnc desktop with an xterm scrolling continuously) were recorded with timing and replayed into both clients with the same instructions at the same moments. Afterwards the two screens matched except for 15 cursor pixels. The metric is main-thread task time from CDP `Performance.getMetrics`, summed over the 12 s. The replay feeds text messages (base64 PNG), so it predates the binary tunnel and zstd images.

Real GPU (Chromium ANGLE → D3D12, Intel Graphics, WSL2), 3 runs:

| Recording | guacamole-common-js | wasm, main-thread WebGL | **wasm, worker + OffscreenCanvas** |
|---|---|---|---|
| guacd 1.6 (7.7 MiB) | 418 – 1,007 ms | 4,529 – 4,921 ms | **111 – 152 ms** |
| molcajeted (33 MiB) | 806 – 862 ms | 4,206 – 4,713 ms | **179 – 183 ms** (one run 762 ms) |

SwiftShader (software GPU, less noise), 2 runs:

| Recording | guacamole-common-js | wasm, main-thread WebGL | **wasm, worker** |
|---|---|---|---|
| guacd 1.6 | 436 – 438 ms | 639 – 660 ms | **162 – 167 ms** |
| molcajeted | 313 – 318 ms | 531 – 541 ms | **195 – 215 ms** |

The worker figures include the harness posting the recording to the worker; in a session the WebSocket lives in the worker, so the main thread's share is smaller still.

1. The wasm side (parsing, base64, dispatch) is a small part of the total.
2. A Blob and `createImageBitmap` per image cost 80 ms per second for the Blob alone.
3. In Chromium, uploading CPU-held decoded images (`VideoFrame`, `ImageBitmap`) into WebGL is synchronous on the calling thread. On a real GPU that made main-thread WebGL slower than the stock client, whose 2D `drawImage` is deferred to the GPU process.
4. The same work in a worker cuts main-thread time to a third to a sixth of the stock client's and makes the page's responsiveness independent of how much the remote screen changes.

`tests/browser/bench.mjs` makes the same comparison on a live session and is for reference only.

### 7.3 zstd images in the client

Two recordings (the list and the pages of section 3.5) were replayed into the client in headless Chromium 1228 on an Apple M4, once as recorded (PNG) and once with the images converted by molcajeted's rule. GPU (ANGLE on Metal), ranges of 3 or 4 runs:

| Replay | Measure | PNG | zstd |
|---|---|---|---|
| list, `?main`, messages back to back | until every frame is applied | 0.35–0.40 s | 0.26–0.28 s |
| | frame lag (`sync` to applied), average | 3.9–5.7 ms | 0.48–0.50 ms |
| | main thread: tasks, of which script | 355–405 ms, 299–353 ms | 268–280 ms, 50–58 ms |
| pages, `?main`, messages back to back | frame lag, average | 13.4–17.0 ms | 0.72–0.86 ms |
| list, worker, recorded timing (25 s) | frame lag, average | 1.05–1.56 ms | 1.35–1.48 ms |
| pages, worker, recorded timing (20 s) | frame lag, average | 1.91–2.04 ms | 1.43–1.62 ms |
| list, `?main`, recorded timing | main thread: tasks, of which script | 862–927 ms, 514–554 ms | 956–1071 ms, 102–113 ms |

- When frames queue up, a frame of zstd images is applied in a tenth of the time, because nothing in it is waited for.
- At the pace of a session with about three images per frame, the frame lag is the same within the spread: a PNG is back from its decoder after about as long as a zstd image takes to decode.
- Script time falls (to a fifth on the list), but the drawing thread now also carries the pixels to the GPU process, so with `?main` its task time at recorded timing was 10 to 40% higher. In the default worker mode that thread is the worker.
- Without a GPU the upload dominates: on SwiftShader, `texSubImage2D` from memory took about 5.5 ms per megapixel on the calling thread, and the list's backlog took 0.75–0.85 s as zstd against 0.28 s as PNG.

The wasm client grew by 50 kB for the zstd decoder. Not measured: a deployed session, other GPUs and browsers, the worker's own CPU time.

### 7.4 Bandwidth against guacd

An x11vnc desktop with an xterm scrolling continuously was recorded for 12 s at a time, alternating between the two daemons (`tests/browser/record.mjs`). The rows are historical stages of the display pipeline; load varied between measurements, so compare within a row.

| Stage | guacd 1.6 | molcajeted |
|---|---|---|
| First implementation (merged change rectangles, fast PNG) | 7.7 MiB | 33.0 MiB |
| Comparison with the client copy, scroll detection | 2.3 MiB | 9.7 MiB |
| Better PNG compression, frame pacing | 3.0 / 4.7 MiB | **4.3 / 4.2 MiB** |

Window dragging (xterm moved with xdotool, 10 s each, 2 runs): guacd 1.6 sent 0.73 / 1.04 MiB, molcajeted **0.40 / 0.41 MiB** while sending more frames (142 against 88–132). guacd 1.6 uses WebP heavily under this load; molcajeted got there with lossless PNG, and lossless WebP was 18% larger than its PNG on the same data. All settling checks after these optimisations showed zero pixel differences.

A command flooding the terminal (`while :; do ls -la /usr/bin /etc; done`) for 8 s, alternating, with the browser client:

| Connection | guacd 1.6 | molcajeted |
|---|---|---|
| SSH | 7.2 / 7.3 MiB, 13.5 fps | **5.5 / 5.6 MiB, 14 fps** |
| RDP (xrdp) | 8.3 / 8.5 MiB, 15.5 fps | 8.1 / 9.4 MiB, 13–14 fps |

SSH is still paced this way (`Pacing::TERMINAL`). The RDP row was measured with the fixed pause after heavy frames; with `Pacing::DESKTOP` a flooding terminal inside a remote desktop now gets more frames and bytes when the link and the client keep up.

**Binary tunnel on the wire.** `display::bench` runs fixed scenarios on a 5K screen once per output mode (`cargo test --release -p molcajete-daemon display::bench -- --ignored --nocapture --test-threads=1`, the `wire` lines). Bytes and blobs per frame, molcajeted → gateway and, forwarded unchanged, gateway → browser:

| Scenario | Text | Binary | Change |
|---|---|---|---|
| `scroll` (document scrolls) | 44.2 kB in 6.2 blobs | 33.1 kB in 1.0 blobs | −25.1% |
| `drag` (window dragged) | 66.9 kB in 11.4 blobs | 50.3 kB in 4.5 blobs | −24.9% |
| `corners` (two small changes) | 23.3 kB in 4.1 blobs | 17.5 kB in 2.0 blobs | −25.0% |
| `page` (full page of new text) | 1151.1 kB in 149.6 blobs | 861.8 kB in 16.9 blobs | −25.1% |
| `scatter` (40 small changes) | 173.6 kB in 39.0 blobs | 130.7 kB in 33.3 blobs | −24.7% |

The binary stream is 0.2–3% larger than the encoded images themselves. These were measured with PNG images; zstd images shrink the payload further (section 7.5).

### 7.5 Building a frame in molcajeted

`display::bench` on a 5120×2654 framebuffer, ms per frame, after comparing once per flush and encoding side by side (M4 with 10 cores, pool of 5 threads, a busy machine, so indicative only; median of 3 runs):

| Scenario | Flush, PNG client | Flush, zstd client | Pixel phase, PNG / zstd |
|---|---:|---:|---:|
| `page` (full page of new text) | 33.3 | 22.2 | 24.7 / 13.5 |
| `scatter` (40 small changes) | 6.6 | 5.5 | 2.8 / 1.7 |
| `scroll` (document scrolls) | 10.3 | 8.8 | 2.5 / 1.0 |
| `drag` (window dragged over a wallpaper) | 6.4 | 6.2 | 2.0 / 1.8 |
| `corners` (two small changes) | 1.5 | 0.9 | 1.1 / 0.5 |

- Encoding side by side cut the pixel phase of `page` by 55% (PNG) and 37% (zstd), `scatter` by 46% / 26%; scroll and corners send about one image per frame and gain nothing. With 7.5 images per `page` frame the frame is now bound by its largest images.
- Comparing once per flush took 26 to 30% off a scroll frame. Scroll and move detection still take 3 to 4.5 ms of frames in which they find nothing.
- The output is the same instruction for instruction as before both changes (the lossless digests `display::bench` prints match).
- zstd images cost 7.0% fewer image bytes than PNG on `page`, 2.7% on `scroll`, and about the same elsewhere; on `drag` the wallpaper stays PNG.

Earlier, sampling the move search instead of sliding over the bounding box of every change took two small changes in opposite corners of this screen from 77.8 ms to 3.8 ms per flush, and a deployed 5120×2654 RDP session from about 50 ms to 12 ms per flush on average.

### 7.6 Pacing by apply time

The pacing tests run simulated clients against the stock path (no reports) and the reporting path (`cargo test -p molcajete-daemon display::pacing::tests::table -- --ignored --nocapture`, 8 s of continuous changes each). A client reports its time from when a frame reaches it, after half the round trip, and applies frames one after the other:

| Client | fps stock / reporting | most frames unacknowledged | bytes sent |
|---|---|---|---|
| desktop, fast (2 ms round trip + 1 ms to apply, 20 kB frames) | 62.5 / 62.5 | 1 / 1 | 10.00 / 10.00 MB |
| desktop, slow (20 ms replies, 500 ms for every 25th frame, 5 kB frames) | 15.9 / 29.0 | 3 / 3 | 0.64 / 1.16 MB |
| desktop, far (100 ms round trip + 5 ms to apply, 5 kB frames) | 10.0 / 28.8 | 3 / 3 | 0.40 / 1.15 MB |
| SSH flood, 50 kB frames, 1 ms replies | 15.2 / 15.2 | 1 / 1 | 6.10 / 6.10 MB |
| SSH flood, 50 kB frames, 30 ms replies | 15.2 / 15.2 | 1 / 1 | 6.10 / 6.10 MB |
| SSH, light frames, 1 ms replies | 30.4 / 30.4 | 1 / 1 | 0.49 / 0.49 MB |

The slow client can apply at most 28.4 frames per second, so reporting brings it to what it can take while the window holds what is queued at the client to three frames. Before apply-time reports, a slow client on a deployed 2732×1876 session got 1 to 5 frames per second although it needed about 17 ms per frame and the link was nearly idle.

### 7.7 Video mode

A 640×360 video (`mandelbrot`, a zooming fractal) was played on the desktop with ffplay, measuring what the client received over 10 s; 4.5 s after playback ended the canvas was compared with the real screen.

| Path | Images only | H.264 (x264) | H.265 (x265) | AV1 (SVT-AV1) | guacd 1.6 |
|---|---|---|---|---|---|
| VNC, Linux Chromium | 6.36 MiB, 13.6 fps | 1.01 MiB, 15.9 fps | (no decoder) | **0.59 MiB**, 18.1 fps | 4.43 MiB, 12.4 fps |
| RDP (xrdp), Linux Chromium | 7.64 MiB, 16.6 fps | 1.19 MiB, 25.0 fps | (no decoder) | **0.53 MiB**, 20.2 fps | 12.36 MiB, 24.9 fps |
| VNC, Windows Chrome (hardware decoding) | 6.76 MiB, 14.5 fps | 0.79 MiB, 12.7 fps | 0.65 MiB, 12.0 fps | **0.44 MiB**, 10.5 fps | |

- Video mode sends about 1/6 – 1/9 of the images-only volume with H.264 and 1/11 – 1/15 with AV1; against stock guacd 1/7 over VNC and 1/23 over RDP. For a 704×448 area the bitrates were 946 kbps (H.264), 662 kbps (H.265) and 567 kbps (AV1).
- A 1280×720 video exceeds software AV1's real-time limit and was sent as H.264: 15.15 MiB images only, 2.63 MiB as video.
- After playback VNC showed zero pixel differences; for RDP the only difference inside the area was the pointer drawn by x11grab.
- Motion that is not video stays images: `testsrc2` (small movements over solid bars) is too sparse to start video, and a terminal flooding text stays lossless because of the colour condition.
- The Windows Chrome row shows low frame rates, probably from headless window rendering; compare within the row.

Preparing a frame for the hardware encoder (Apple M4, loaded machine, per frame): RGBA → NV12 at 1920×1080 0.72 ms with NEON (1.26 ms before); a frame in which one tile changed is brought up to date in 0.002 ms instead of 0.185 ms; the whole video phase with VideoToolbox H.265 at 1920×1080 takes about 6.2 ms, most of it the hardware encode.

### 7.8 RDP bitmap updates

`rdp::bench::bitmap_updates` puts 64×64 tiles covering a 5120×2654 desktop and 4000 scattered small rectangles through the earlier two-copy path and `put_image` (median of 15 alternating runs, loaded machine):

| Scenario | Two copies | `put_image` |
|---|---|---|
| tiles, RgbA32 | 0.40–0.59 ms/MP | 0.27–0.33 ms/MP |
| scattered, RgbA32 | 1.34–1.56 ms/MP | 0.89–1.10 ms/MP |
| tiles, BgrA32 | 0.89–1.10 ms/MP | 0.28–0.42 ms/MP |
| scattered, BgrA32 | 2.03–2.45 ms/MP | 0.89–1.03 ms/MP |

### 7.9 Seeking in recordings

Seeking to the end of an hour, measured with `tests/browser/seek-bench.mjs` (a recording repeated with shifted times, headless Chromium 1228 on the Apple M4's GPU, worker mode), each check of the final screen equal to the original's:

| Recording (x11vnc, an xterm scrolling without pause, 1280×800) | 1 hour | Seek to the end | Back to the middle | Middle to the end |
|---|---|---|---|---|
| molcajeted, zstd images (`record-molcajete-formats`) | 152 MB | 2.8 s | 1.4 s | 1.4 s |
| molcajeted, PNG (the default for recorded sessions) | 200 MB | 5.0 s | 2.4 s | 2.6 s |
| stock guacd 1.6, PNG and WebP | 327 MB | 16.2 s | 8.0 s | 8.1 s |

This is the worst case: the screen changes 20 times a second for the whole hour. Ordinary sessions change much less (the recorded RDP and SSH sessions of section 7.1 were 5 to 8 kB/s, against 55 to 90 kB/s here) and seek proportionally faster. guacd's recordings are slower because guacd sends larger images, and WebP still goes through the browser. Decoding PNG in wasm grew the client from 754 kB to 875 kB.

### 7.10 File transfer

`tests/browser/files.e2e.mjs` against the gateway, molcajeted and the SSH test target on one Apple M4 (all local, headless Chromium, binary tunnel). The terminal printed the time every 100 ms during the transfers; the browser's memory is the resident size of all its processes.

| | Time | Rate | Browser memory before → at most | Terminal frames/s (median, min) |
|---|---|---|---|---|
| Download 4 GiB, worker, to a file (File System Access) | 36 s | 113 MiB/s | 385 → 527 MiB | 10, 9 |
| Upload 4 GiB, worker | 69 s | 59 MiB/s | 486 → 495 MiB | 10, 7 |
| Download 1 GiB, main thread, to a file | 9 s | 110 MiB/s | 615 → 646 MiB | 10, 9 |
| Upload 1 GiB, main thread | 18 s | 58 MiB/s | 643 → 644 MiB | 10, 9 |

Memory does not grow with the size of the file in either direction, and the terminal keeps drawing its ten updates a second. Uploads are slower than downloads because their data travels as base64 text (section 8, binary tunnel). Firefox and Safari have no `showSaveFilePicker`: a download there is collected in a Blob, which needs memory for the whole file unless the browser moves it to disk; this was not measured.

## 8. Roadmap

Tracked as issues:

- Edge cases older remote-desktop projects have fixed: keyboard layouts, AltGr, dead keys, stuck keys, clipboard fallbacks and more (#63)

Not yet tracked:

1. **molcajeted protocols.** Telnet and Kubernetes (the web UI already treats them as terminals for stock guacd). SSH: agent forwarding, italics and colour emoji. RDP: RemoteApp, receiving EGFX/H.264 and relaying the server's stream as is, AAC/Opus audio, Kerberos, bulk compression once IronRDP can reactivate with it.
2. **Input.** Audio input (microphone); touch and multi-touch for desktops (terminals have their own, section 4.7), an on-screen keyboard for them; function keys in the terminal key bar.
3. **Gateway.** LDAP, TLS to the daemon, an HTTP tunnel for networks without WebSocket, temporary connections (target and credentials) created together with a one-time session link and deleted after the session. A general per-address rate limit for the API and for opening tunnels (only failed sign-ins are limited; the nginx example limits requests and connections per address), failed-sign-in counters shared between replicas, and connection credentials kept in HashiCorp Vault or OpenBao (KV) and referenced from a parameter, instead of encrypted in the database.
4. **Recordings.** Checkpoints (copies of the layers every so often) so that seeking backward does not replay from the start, at the price of GPU memory and of video streams whose decoder state cannot be copied; playing audio; showing the key log of a recording with keys; recording in molcajeted for deployments without the gateway.
5. **Renderer.** Batching solid fills, a WebGPU backend (`copyExternalImageToTexture`/`importExternalTexture` to cut upload costs; only `Renderer` needs replacing), recovering from a lost WebGL context.
6. **molcajeted encoding.** Pacing from a measured link rate and per-connection limits (the byte budget is a fixed 8 MiB/s); building a frame off the connection's task (images are encoded in parallel, but the session loop still waits for the frame); running the video encode alongside the image phase; backing off scroll and move detection where it keeps finding nothing; lossy WebP as in guacd 1.6. zstd images: the cost of one more image was estimated for PNG and not revisited; indexed images could be uploaded as indices and expanded by a shader; a faster Zstandard decoder for wasm than `ruzstd`.
7. **Binary tunnel.** Client → server streams (clipboard, file uploads, and audio input once it exists) are still base64 text, which makes uploads about half as fast as downloads.
8. **Video mode.** NVENC, QSV, VA-API and AMF are wired up but not verified on real hardware (VideoToolbox is verified on Apple M4); forced key frames restart ffmpeg (tens of milliseconds); the client's codec string should be built from the SPS or sequence header; several video areas at once; bitrate adapted to bandwidth.
9. **File transfer.** Streaming downloads to disk in Firefox and Safari through a Service Worker; syncing a local folder picked with `showDirectoryPicker` (Chromium); SFTP for RDP and VNC connections (guacd's `sftp-hostname` and friends); guacd's automatic download of files saved to the drive's `Download` folder and the SSH terminal's download escape codes; deleting, renaming and creating folders from the panel; a separate drive per user (guacd's `${GUAC_USERNAME}` tokens in `drive-path`); per-file audit entries.

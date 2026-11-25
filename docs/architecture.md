# molcajete: analysis and design

This document analyses the structure of Apache Guacamole (server `guacamole-server`, client `guacamole-client`, main branch as of 2026-11) and describes how each molcajete component was designed from that analysis.

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
- Limits: 8192 per instruction, 128 elements, 5 digits of length (libguac `parser-constants.h`, Java `GuacamoleParser`). One blob carries at most 6048 bytes of raw data.
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
- The client sends `0.,4.ping,<ts>;` every 500 ms and the server echoes it; it never reaches guacd.
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

## 2. molcajete

```
crates/
  molcajete-protocol   shared protocol (platform independent): streaming parser, encoder, status codes, typed instructions, video framing
  molcajete-client     wasm client: WebGL2 renderer, WebCodecs video, input, audio, clipboard, WebSocket tunnel
  molcajete-gateway    Rust web gateway: static files, connection list API, WebSocket ↔ guacd relay
  molcajete-daemon     Rust guacd (molcajeted): handshake, server-side display/encoding, video mode, VNC·SSH·RDP (+audio)
  molcajete-telemetry  logging, Prometheus metrics and CPU profiling for the gateway and molcajeted
web/              React + Astryx UI (login, connections, users, session view, Ctrl+Alt+Shift menu);
                  public/ holds the wasm client glue (molcajete.js, worker, pkg/)
tests/browser/    headless Chromium pixel tests
docker/           test targets (guacd 1.6, X11+VNC, OpenSSH, xrdp+XFCE on 127.0.0.1:3390)
deploy/           container deployment (Compose, Kubernetes, reverse proxy, Grafana), see deployment.md
```

Possible combinations:

| Browser | Gateway | Daemon |
|---|---|---|
| molcajete-client (wasm) | molcajete-gateway | stock guacd or molcajeted |
| stock guacamole-common-js | molcajete-gateway (accepts the `GUAC_*` parameters) | stock guacd or molcajeted |
| stock web app | Java web app | molcajeted (compatible handshake) |

### 2.1 molcajete-protocol

- Incremental byte-level parser. It copes with TCP/WebSocket boundaries in the middle of an instruction or a UTF-8 sequence, and keeps its state so that each byte is looked at once. ASCII runs are skipped in bulk (blobs are mostly base64, so this is fast).
- `RawInstruction`: a zero-copy view borrowing the parser buffer. The gateway only uses it to find instruction boundaries and forwards the bytes unchanged.
- `ServerInstruction<'a>`: typed decoding of server → client instructions (string arguments are borrowed).
- Limits: `Limits::STRICT` (same as guacd, for untrusted input) and `Limits::LENIENT`.
- `video`: framing of `video` streams (section 2.5). The server's `encode_frame` and the client's `FrameReader` (reassembles frames regardless of blob boundaries) share this code.

### 2.2 molcajete-gateway

- Built on axum. Serves the JSON API (`/api/auth/*`, `/api/connections`, `/api/users`), `/websocket-tunnel` and the static files (`web/dist`).
- After the WebSocket upgrade it performs the guacd handshake; on failure it closes with the status code in the close reason (like the Java web app).
- guacd → browser: the bytes read are cut into instructions by the parser and batched into one message of up to 64 KiB, sent immediately when the next read would block.
- browser → guacd: validated with `Limits::STRICT` and forwarded verbatim. Internal pings are answered directly.
- Configuration: TOML (`gateway.example.toml`) for the server itself. Users, connections (each may name its own guacd), parameters, permissions and login sessions are stored in PostgreSQL, MySQL/MariaDB or SQLite through sqlx's `Any` driver, with per-dialect migrations in `crates/gateway/migrations/`, like Guacamole's JDBC authentication extensions. Passwords are Argon2id hashes; sessions are cookies whose SHA-256 is stored.

### 2.3 molcajete-client: GPU renderer design

**Threads.** By default the whole client (WebSocket tunnel, parser, image decoding, WebGL2 rendering, compositing) runs inside a Web Worker. The visible canvas is an `OffscreenCanvas` handed to the worker with `transferControlToOffscreen()`. Only three things stay on the main thread:

- receiving pointer/wheel/keyboard/IME events and forwarding them to the worker
- applying the worker's `layout` (canvas CSS size) and `cursor` (RGBA → CSS cursor) events to the DOM
- handling UI events

`web/public/molcajete.js` (`MolcajeteSession`) and `web/public/molcajete-worker.js` split these roles. Browsers without OffscreenCanvas run the same wasm client on the main thread (`?main` forces this). The reason for the split is the measurement in section 3.1.

**Layers are textures.** Each layer and buffer is an RGBA8 texture (premultiplied alpha) with its own FBO. Allocations are rounded up to 64 px like the original, and resizing moves the old contents with `blitFramebuffer`. Textures are stored top row first, so inside an FBO `gl_FragCoord.xy` is the layer pixel coordinate, and the Y axis is flipped only once, when compositing into the canvas.

**Instructions → GPU operations**

| Instruction | Implementation |
|---|---|
| `rect`+`cfill` | Non-overlapping rectangles are drawn as quads with vertex colours in a single draw call (fast path). |
| Channel masks | 11 `blendFunc` setups (premultiplied Porter–Duff) |
| Unbounded modes (SRC/IN/OUT/RIN/RATOP) | For a single rectangle, the outside is cleared with at most 4 scissor+clear calls. Otherwise a coverage mask drives a "clear outside" pass followed by a "draw inside" pass. |
| `copy` | Between layers the source texture is sampled directly. Within a layer (scrolling) the region is first copied into a scratch texture with `copyTexSubImage2D`. |
| `transfer` | ROP shader: applies the 4-bit truth table with integer bit operations to un-premultiplied 8-bit values. SRC uses `blitFramebuffer` (ignores clip and composite mode, like putImageData). |
| `img` | Decoded with WebCodecs `ImageDecoder` (no colour-space conversion) into a `VideoFrame`; without `ImageDecoder`, Blob + `createImageBitmap`. PNG (IHDR colour type, tRNS) and WebP (VP8/VP8L/VP8X) headers tell whether the image is opaque; opaque images without transform or clip are uploaded straight into the layer texture, the rest go through a reused upload texture and are blended. Uploads are first drawn into a 2D `OffscreenCanvas` (deferred to the GPU process) and that canvas is copied with `texSubImage2D`. |
| `video` | Unpacks molcajete framing (a 9-byte header per frame + H.264/H.265 Annex B or AV1 OBUs, `molcajete_protocol::video`) into a WebCodecs `VideoDecoder` (`optimizeForLatency`, hardware decoding where available). Each frame takes an image slot in the frame queue, like `img`, filled by the decoder output (`VideoFrame`), so video follows `sync` order and the other drawing instructions. Decoded frames are uploaded to the texture as they are, on the GPU. Decoder errors are reported to the server with an `ack` error and the server falls back to images. |
| Paths (`arc`/`curve`/`line`/`clip`/`cstroke`/`lfill`) | Arcs and curves are flattened into polylines; fills (nonzero) and strokes are tessellated with lyon. Coverage is drawn into an R8 mask that a shader samples while compositing. |
| `clip` | Scissor for axis-aligned rectangles, otherwise a per-layer R8 clip mask (intersections by multiplicative blending) |
| `lfill`/`lstroke` | Pattern shader (repeats with `mod` over the logical size) |
| `push`/`pop`/`reset`/`transform` | Per-layer drawing state stack (CTM, clip, composite mode, miter limit) |
| `move`/`shade`/`distort`/`dispose` | Layer tree properties, applied when compositing |
| `cursor` | Copied into a cursor texture and read back **asynchronously** with a PBO + fence, so the GPU pipeline never stalls. The CSS cursor (data URL) is updated only when the pixels actually changed, so the local pointer has no lag; when the source layer did not change, the readback is skipped entirely. The server's `mouse` position is drawn as a software cursor. |

**Frames.** Instructions are queued as `Op`s, and once a whole frame up to `sync` is ready (images decoded) it runs at once. When the frame has run, `sync` is sent back and the next `requestAnimationFrame` composites once. Intermediate states never reach the screen.

**Compositing.** The layer tree is walked and each layer drawn with its transform (`translate`, then `distort` about its centre), opacity and the parent's clip (scissor). A translucent layer with children is first rendered as a subtree offscreen and blended with the group opacity, like CSS `opacity`. The canvas is created with `desynchronized: true` (low latency), and linear filtering is used only for non-integer scale factors.

**Input.**
- Keyboard: the keysym is derived from `KeyboardEvent.key`/`code`/`location` (ported tables). The keysym sent on press is remembered per physical key (`code`) and sent again on release, so releasing Shift first does not leave an `A` pressed.
- IME (Korean etc.): composition is left to the browser, and the `compositionend` result is sent as Unicode keysyms (`0x01000000 | code point`) pressed and released.
- Mouse: Pointer Events with pointer capture. Wheel deltas accumulate in 53 px steps (same as the original).

**Other.** Clipboard (both directions, UTF-8, 3 KiB blobs), `required` requests (credential prompt → `argv` streams), `nop` every 5 s, a tunnel ping every second (RTT), disconnect after 15 s of silence, remote resize requests (`size`) when the container resizes, and performance statistics events. Raw PCM audio (`audio/L8`, `audio/L16`) is played with Web Audio. Web Audio is not available in workers, so in worker mode the worker hands PCM to the page (`audio-start`/`audio-data`/`audio-end` events, ArrayBuffers transferred without copying) and the page's `PcmPlayer` schedules it with 50–300 ms of latency. The audio context resumes on the first pointer or key input (autoplay policy). While connected, the client keeps itself alive, so the session survives the JS handle being garbage-collected (wasm-bindgen FinalizationRegistry).

### 2.4 molcajeted

- Built on tokio, one task per connection (the original forks a process). The handshake matches the original (`args` advertises `VERSION_1_5_0`; argument names match the original VNC plugin).
- The server-side display (`display/`) keeps two buffers:
  - `pixels`: the actual screen as reported by the remote protocol
  - `sent`: a copy of the client's layer, built by replaying every instruction we send (fills, images, copies)
- At flush time only the 64×64 tiles touched in this frame are compared between the two:
  1. **Scroll detection**: row hashes find vertical moves, sent as `copy`.
  2. **Move detection in any direction** (dragged windows, ...): hashes of the changed 64×64 cells are looked up while a 64×64 window slides one pixel at a time over the client copy, using a 2D rolling hash (two odd multipliers, subtracting the rows and columns that leave the window). Cells that moved by the same offset are merged into large rectangles and sent as single `copy` instructions, front of the movement first. guacd 1.6 slides with multiplier 62 and no subtraction, which leaves the upper-left half of the block out of the hash and produces many false matches, so that approach was not used.
  3. **Remaining differences**: cut into changed row bands and changed column runs within them, then encoded. Unchanged pixels re-sent by the server cost nothing.
  4. **Encoding**: solid colours as `rect`+`cfill`, up to 256 colours as palette PNG (1/2/4/8 bit), everything else as RGB PNG (Balanced compression + Adaptive filter). Large, colourful areas that change often are sent as JPEG when the client supports it, and those tiles are re-sent as PNG once they calm down (lossless refinement).
- Because the instructions we send are replayed on the copy, wrong guesses correct themselves: a bad `copy` shows up as a difference in the next comparison and is re-sent as pixels. VNC CopyRect only needs the same `copy` applied to the copy, with no need to flush the source area first. This property is checked by a client simulator that replays the instructions and by randomized fuzz tests (1,000 seeds).
- Frame pacing: the send time of every `sync` is recorded, and the exponentially smoothed time until its reply is used as the client's processing lag. The next frame goes out after `max(33 ms, lag)` (at most 500 ms, the same idea as guacd). Remote updates keep arriving meanwhile and accumulate as differences from the copy, so short-lived intermediate states are never sent. The first change after a quiet period is sent immediately, so input stays responsive.
- After a large frame (more than 32 KiB encoded), at least 66 ms (about 15 fps) pass before the next. This bounds bandwidth under continuous floods of output; all three protocols (VNC/SSH/RDP) share this pacing (`Display::next_frame_at`).
- Flow control: with more than 2 frames unacknowledged, no update requests are sent to the remote server (they resume when replies arrive, or forcibly after 5 s).
- VNC: RFB 3.3/3.7/3.8, None/VNC authentication, Raw/CopyRect/Hextile/ZRLE, Cursor/DesktopSize/ExtendedDesktopSize/LastRect pseudo-encodings, clipboard in both directions, remote resize.
- SSH (`ssh/`): connects with russh (password, private key + passphrase, keyboard-interactive, asking with `required` when a value is missing, `host-key` pinning) and keeps an xterm-compatible screen with alacritty_terminal (256 colours/true colour, alternate screen, scrollback, bracketed paste, SGR mouse). Cells are rendered on the server with ab_glyph, and only the row spans that differ from the previous frame go into the display; scroll detection and encoding are then up to the shared display. DejaVu Sans Mono is bundled, with the bundled NanumGothicCoding as fallback for Hangul, Han and Kana; `font-path` and system fonts are further fallbacks. Supports double-width characters, box-drawing characters, drag/double-click selection → client clipboard, Ctrl+Shift+V/right-click paste, wheel scrollback and resizing. Argument names match the guacd SSH plugin.
- RDP (`rdp/`): IronRDP (connector/session/graphics/cliprdr/displaycontrol) with rustls. Security: TLS and NLA (CredSSP/NTLM); certificates are verified unless `ignore-cert` is set. Bitmap updates go into the display per changed rectangle, pointer bitmaps become the cursor. Modifiers, function keys and shortcuts are sent as scancodes, other characters (including Hangul) as Unicode events. Mouse, text clipboard (both directions, UTF-16) and display-update resizing are supported. IronRDP does not implement RC4 standard RDP security, so `security=rdp` is refused.
- RDP audio (`rdp/audio.rs`): registers the RDPSND static channel and the `AUDIO_PLAYBACK_DVC` dynamic channel, and picks among the server's formats only those it can decode (PCM 8/16-bit, G.711, IMA/MS ADPCM), preferring 16-bit, 44.1k, 48k, stereo. Sound is sent right away, independent of frame pacing, as a Guacamole `audio/L16;rate=R,channels=C` stream (reduced to 8-bit if the client only accepts L8). When the client takes no audio or `disable-audio` is set, the channels are not created, so the remote session plays no sound. If the client rejects the stream (`ack` error), audio stops for that session.

### 2.5 Video mode (molcajeted → WebCodecs)

Areas with a lot of motion (video playback, animation, 3D) are expensive even as JPEG. molcajeted finds such areas and sends them as a compressed video stream into a separate layer. It uses the Guacamole protocol's `video` instruction (stream, layer, mimetype) unchanged; the stream content uses this project's framing (`molcajete_protocol::video`). The stock JS client does not advertise our mimetypes such as `video/h264`, so with it everything stays images, as before.

- **Negotiation.** The client advertises in the handshake `video` only the codecs that `VideoDecoder.isConfigSupported` accepts (`video/av1`, `video/h265`, `video/h264`). The server picks, in the operator's order of preference (`--video av1,h265,h264`, `none` to disable), a codec the client can decode and the server can encode. Not used for sessions with `force-lossless` or for SSH.
- **Finding the area.** For every 64×64 tile, the number of frames in which it had to be sent as an image is counted and halved every second. Tiles scoring 8 or more are busy: at 30 fps this takes about 0.3 s, at 15 fps about 0.6 s, and fewer than 6 updates per second never get there. Scrolling and window moves are handled by `copy` first and only the remaining differences count, so they are not mistaken for video. Video mode starts when the bounding box of the busy tiles meets all of:
  - 6 tiles or more
  - busy tiles cover at least half the box
  - more than 256 colours (text and UI are better served by lossless encodings)
- **Video layer.** Layer 1 is sized to the area (rounded up to multiples of 16) and placed over it (`size`, `move`), and a `video` stream is opened on it. Every frame in which the area changed is fed to the encoder, and the result goes out with a 9-byte header (length, timestamp, key frame flag) in blobs of at most 6048 bytes. The first frame is a key frame carrying the codec configuration (SPS/PPS, VPS, AV1 sequence header). Tiles inside the video area are left out of the normal image diff.
- **The default layer's copy is left alone.** While the video plays, the video area of `sent` (the copy of the client's default layer) is not touched, so the copy keeps matching what the default layer really holds beneath the video layer. When the area has not changed for 1 s, the layer is removed with `end` and `dispose`, and in the same frame the whole area is compared with the copy and updated losslessly. No lossy content stays on screen, and no stale content shows through when the layer disappears. When the busy area extends beyond the video area for 8 frames (window moved or resized), the video restarts over the new area. An idle timer per session (`Display::idle_deadline`) makes this clean-up happen even without changes; the same timer drives the lossless refinement of JPEG areas that calmed down, which previously waited for the next change.
- **Codecs and bitrate.** The target bitrate is area × 30 fps × 0.1 bpp for H.264, 0.7× that for H.265 and 0.6× for AV1 (300 kbps to 16 Mbps). Software AV1 (SVT-AV1) takes about 25 ms per 720p frame, so areas larger than about 0.94 MP (1280×736) use the next codec in line (H.265, then H.264). Hardware encoders have no size limit.
- **Encoders (`video/`).** ffmpeg runs as a child process with RGBA piped in. Nothing is linked, so the GPL x264/x265 pose no licensing problem. Output is read as an AVI container: with a raw elementary stream the end of a frame is only known once the next one starts, which would add a frame of delay, whereas this way each frame in yields its access unit right away. At startup `ffmpeg -encoders` is read and each candidate does a 3-frame trial encode; only working ones are kept. Priority: hardware (NVENC/QSV/VA-API/AMF; on macOS VideoToolbox), ffmpeg software (libsvtav1, libx265, libx264), built-in OpenH264 (cargo feature `openh264`, needs a C++ compiler). Colour space is BT.709 limited range, signalled in the bitstream. VideoToolbox is called directly (`video/videotoolbox.rs`) instead of through ffmpeg's `*_videotoolbox`, because ffmpeg does not wait for VideoToolbox's asynchronous output and each packet would come one frame late; after each frame `VTCompressionSessionCompleteFrames` waits for its packet, which is then converted from length-prefixed NAL units to Annex B with the parameter sets in front of key frames.
- **Failures.** An encoder error ends video mode and falls back to images; after three failures the session stops trying. A client rejecting the `video` stream or reporting a decoder error with `ack` turns video off the same way.

## 3. Verification

- Unit tests: protocol (every split point, code point lengths, limits, invalid UTF-8), gateway (fake guacd handshake, error mapping, query parsing), daemon (handshake, display merging/copy ordering/blob splitting, PNG round trips), client logic (path tessellation area, keysyms, key release pairing, wheel, PCM).
- Browser pixel tests (`tests/browser`): 22 scenarios checked against the canvas reference implementation.
- End-to-end in real environments: with guacd 1.6 + x11vnc (xterm/xclock/xeyes), display, key input (Shift combinations) and mouse tracking; with guacd 1.6 + OpenSSH, colours, bold text, Hangul, scroll bars, drag selection and clipboard. The same items were checked on the all-Rust path (wasm client → molcajete-gateway → molcajeted → x11vnc), worker mode included.
- molcajeted SSH: colours (16/256/true colour), bold, Hangul (in output and on the input command line), full-screen apps such as `top`/vi, fast scrolling, drag selection → clipboard, wheel scrollback, resizing, and error mapping (wrong password, closed port, host-key mismatch). On the stock guacd path Hangul on the input command line was garbled; molcajeted displays it correctly because its default terminal type is `xterm-256color`.
- molcajeted RDP: on xrdp+XFCE, the same scenarios as with stock guacd produce the same screen: opening a terminal, Shift-combined special characters, Ctrl+C, wheel scrolling, clipboard in both directions (including Hangul), display-update resizing, and a clean exit on logout.
- Video: `tests/browser/video.test.mjs` takes real H.264/AV1 bitstreams made with ffmpeg, frames them the way molcajeted does, feeds them as a `video` stream, and checks the composited canvas pixels and the return to the default layer after `dispose`. Linux Chromium has no H.265 decoder, so the same page was opened in Windows Chrome 158 (headless, hardware decoding), which decoded all three codecs (H.264/H.265/AV1). That browser decodes H.265 in hardware only (`prefer-software` is not supported).
- Audio: a 440 Hz sine played in an xrdp session is sent by molcajeted as `audio/L16;rate=44100,channels=2` (about 177 KB/s, on par with stock guacd), and the samples the browser schedules in Web Audio are at 440.0 Hz too (worker and main-thread modes).
- Settling test: after scrolling an xterm continuously for a few seconds and stopping the load, the client canvas was compared pixel by pixel with the X server screen (`xwd`). The molcajeted path showed zero differences in 5 runs (in one, the xclock second hand moved between the canvas and X captures). The stock guacd 1.6 path, viewed with either the stock JS client or ours, sometimes left hundreds to thousands of stale pixels where a window had closed. This is guacd's behaviour, not the client's.

### 3.1 Comparison with the stock client (`tests/browser/replay-bench.mjs`)

Live session comparisons are unreliable: guacd adapts the number of frames to how fast the client answers `sync`, so the two clients do not even receive the same input. Instead, 12 seconds of the protocol stream of a real session (an x11vnc desktop with an xterm scrolling continuously) were recorded with timing, and replayed into both clients with **the same instructions at the same moments**. After the replay the two clients' screens matched except for 15 cursor pixels. The metric is main-thread task time from CDP `Performance.getMetrics` (summed over the 12 s). The page must be served from `localhost` (a secure context) for WebCodecs to be enabled.

Real GPU (Chromium ANGLE → D3D12, Intel Graphics, WSL2), 3 runs:

| Recording | guacamole-common-js | wasm, main-thread WebGL | **wasm, Worker + OffscreenCanvas** |
|---|---|---|---|
| guacd 1.6 (7.7 MiB) | 418 – 1,007 ms | 4,529 – 4,921 ms | **111 – 152 ms** |
| molcajeted (33 MiB) | 806 – 862 ms | 4,206 – 4,713 ms | **179 – 183 ms** (one run 762 ms) |

SwiftShader (software GPU, less noise), 2 runs:

| Recording | guacamole-common-js | wasm, main-thread WebGL | **wasm, Worker** |
|---|---|---|---|
| guacd 1.6 | 436 – 438 ms | 639 – 660 ms | **162 – 167 ms** |
| molcajeted | 313 – 318 ms | 531 – 541 ms | **195 – 215 ms** |

The worker figures include the cost of the replay harness passing the recording to the worker with `postMessage`. In a real session the WebSocket lives inside the worker, so the main thread's share is even smaller.

What the measurements showed:
1. The wasm side (parsing, base64, dispatch) is a small part of the total.
2. `new Blob()` + `createImageBitmap()` per image costs 80 ms per second for Blob creation alone. This is also why the original uses WebCodecs `ImageDecoder`.
3. In Chromium, uploading decoded results held in CPU memory (`VideoFrame`/`ImageBitmap`) into WebGL textures happens **synchronously on the main thread**. On a real GPU, waiting on that synchronization made the main-thread WebGL setup slower than the original (whose 2D canvas `drawImage` is deferred to the GPU process).
4. Moving the same work into a worker cuts main-thread cost to 1/3 – 1/6 of the original, and makes the page's input and UI responsiveness independent of how much the remote screen changes. That is hard to achieve with the original's design, which is tied to DOM canvases.

`tests/browser/bench.mjs` makes the same comparison on a live session and, for the reasons above, is for reference only.

### 3.2 molcajeted bandwidth

An x11vnc desktop with an xterm scrolling continuously was recorded for 12 s at a time, alternating between the two daemons in the same period (`tests/browser/record.mjs`).

| Stage | guacd 1.6 | molcajeted |
|---|---|---|
| First implementation (merged change rectangles + PNG Fast) | 7.7 MiB | 33.0 MiB |
| Mirror comparison + scroll detection | 2.3 MiB | 9.7 MiB |
| + PNG Balanced/Adaptive + frame pacing | 3.0 / 4.7 MiB | **4.3 / 4.2 MiB** |

Window dragging (xterm moved continuously with xdotool, 10 s each, alternating, 2 runs): guacd 1.6 sent 0.73 / 1.04 MiB, molcajeted **0.40 / 0.41 MiB**, at less than half the bandwidth while sending more frames (142 vs 88–132).

For SSH and RDP, a command flooding the terminal (`while :; do ls -la /usr/bin /etc; done`) ran for 8 s at a time, alternating, with the real browser client, comparing the bytes and frames the client received.

| Connection | guacd 1.6 | molcajeted |
|---|---|---|
| SSH | 7.2 / 7.3 MiB, 13.5 fps | **5.5 / 5.6 MiB, 14 fps** |
| RDP (xrdp) | 8.3 / 8.5 MiB, 15.5 fps | 8.1 / 9.4 MiB, 13–14 fps |

Over SSH, molcajeted first used its own 60 fps cap and sent 9.1 MiB (24 fps). With the shared pacing (30 fps cap + client lag compensation) and the "wait 66 ms after a large frame (over 32 KiB)" rule it sends about 24% less than guacd at the same frame rate. RDP gets no server-side `copy` hints, because IronRDP does not pass drawing orders such as ScrBlt through, but the shared display's comparison and move detection keep it on par with guacd.

Load intensity varied between measurements, so only rows can be compared with each other. In the end molcajeted is on par with guacd 1.6 and sends slightly fewer bytes per frame (13.4 KB vs 15 KB). guacd 1.6 uses WebP heavily under the same load, while molcajeted gets there with lossless PNG. On the same data, lossless WebP (image-webp) was 18% larger than PNG Balanced.

Accuracy is unchanged: all 3 settling tests after these optimizations showed zero pixel differences from the real X screen.

### 3.3 Video mode bandwidth

A 640×360 video (`mandelbrot`, a zooming fractal) was played on the desktop with ffplay, measuring what the client received over 10 s. 4.5 s after playback ended, the canvas was compared with the real screen (clock corner masked).

| Path | Images only | H.264 (x264) | H.265 (x265) | AV1 (SVT-AV1) | guacd 1.6 |
|---|---|---|---|---|---|
| VNC, Linux Chromium | 6.36 MiB, 13.6 fps | 1.01 MiB, 15.9 fps | (no decoder) | **0.59 MiB**, 18.1 fps | 4.43 MiB, 12.4 fps |
| RDP (xrdp), Linux Chromium | 7.64 MiB, 16.6 fps | 1.19 MiB, 25.0 fps | (no decoder) | **0.53 MiB**, 20.2 fps | 12.36 MiB, 24.9 fps |
| VNC, Windows Chrome (HW decoding) | 6.76 MiB, 14.5 fps | 0.79 MiB, 12.7 fps | 0.65 MiB, 12.0 fps | **0.44 MiB**, 10.5 fps | |

- Video mode sends about 1/6 – 1/9 of the images-only volume with H.264 and about 1/11 – 1/15 with AV1. Compared with stock guacd that is 1/7 over VNC and 1/23 over RDP. For a 704×448 area the bitrates were 946 kbps (H.264), 662 kbps (H.265) and 567 kbps (AV1).
- A 1280×720 video (an area the size of the full screen) exceeds the real-time limit of AV1 and was sent as H.264: 15.15 MiB images only, 2.63 MiB in video mode.
- In the settling comparison after playback, VNC showed zero pixel differences (images-only mode too). For RDP the only difference inside the video area was the mouse pointer drawn by x11grab; the differences outside it come from xrdp's colour conversion and appear with stock guacd as well.
- Motion that is not video stays images. `testsrc2` (small movements over solid bars) is too sparse to enter video mode (4.4 MiB, same as images-only), and a terminal flooding text stays lossless because of the colour condition.
- The Windows Chrome row shows low frame rates, probably due to headless window rendering, so compare only within that row.

## 4. Remaining work (roadmap)

1. molcajeted protocols
   - SSH: SFTP, session recording, agent forwarding, italics/colour emoji
   - RDP: drive redirection, RemoteApp, receiving EGFX/H.264 (relaying the server's H.264 as is), AAC/Opus audio, Kerberos
   - Telnet and Kubernetes
   - NLA has not been verified against a real (Windows) server yet
2. Connection sharing (joining with `select,$ID`), read-only observers, session recording (guacenc compatible) and playback (the client already has a `feed` API)
3. File transfer (`file`/`pipe`/`filesystem` objects), audio input (microphone), touch/multi-touch, on-screen keyboard
4. Gateway: authentication providers (OIDC/LDAP/DB), connection permissions, TLS to guacd, HTTP tunnel (for environments without WebSocket)
5. Renderer: batching solid fills, a WebGPU backend (`copyExternalImageToTexture`/`importExternalTexture` to cut upload costs; only `Renderer` needs replacing), context loss recovery
6. molcajeted encoding: bandwidth-aware frame pacing, parallel encoding, lossy WebP (as in guacd 1.6)
7. Video mode
   - Hardware encoders (NVENC/QSV/VA-API/AMF) are wired up but not verified on real hardware. VideoToolbox is verified on Apple M4.
   - Forced key frames restart ffmpeg, which costs an extra 25–45 ms.
   - Build the client codec string exactly from the SPS/sequence header (currently a generous fixed value).
   - Several video areas at once, bitrate adapted to bandwidth.

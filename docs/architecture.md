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
  molcajete-protocol   shared protocol (platform independent): streaming parser, encoder, status codes, typed instructions, video framing, binary tunnel framing, zstd image format
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

Between molcajete components, stream data (images, video, audio) travels as raw bytes instead of base64 (section 2.6); every combination with a stock component uses the text protocol on that leg. When the browser runs molcajete-client and the daemon is molcajeted, lossless images are zstd images instead of PNG (section 2.7), and frames are paced by the time the client reports it needs to apply them (section 2.9), whatever the gateway.

### 2.1 molcajete-protocol

- Incremental byte-level parser. It copes with TCP/WebSocket boundaries in the middle of an instruction or a UTF-8 sequence, and keeps its state so that each byte is looked at once. ASCII runs are skipped in bulk (blobs are mostly base64, so this is fast).
- `RawInstruction`: a zero-copy view borrowing the parser buffer. The gateway only uses it to find instruction boundaries and forwards the bytes unchanged.
- `ServerInstruction<'a>`: typed decoding of server → client instructions (string arguments are borrowed).
- Limits: `Limits::STRICT` (same as guacd, for untrusted input) and `Limits::LENIENT`.
- `video`: framing of `video` streams (section 2.5). The server's `encode_frame` and the client's `FrameReader` (reassembles frames regardless of blob boundaries) share this code.
- `binary`: the record framing of the binary tunnel (section 2.6): `RecordWriter` (molcajeted, and the gateway when it decodes for a stock guacd), `RecordReader` (the gateway's TCP side, any split of the byte stream) and the constants the client parses a message with.
- `zimage`: the zstd image type (section 2.7): its mimetype, the header molcajeted writes, and the decoder, which the wasm client and the daemon's client simulator share. The decoder is pure Rust (`ruzstd`), so the crate still builds for `wasm32-unknown-unknown`; the encoder needs libzstd and lives in molcajeted.
- `apply_time`: apply-time reports (section 2.9): the argument with which molcajeted's `sync` asks for the time a frame took to apply, and the client's reply carrying it.

### 2.2 molcajete-gateway

- Built on axum. Serves the JSON API (`/api/auth/*`, `/api/connections`, `/api/users`), `/websocket-tunnel` and the static files (`web/dist`).
- After the WebSocket upgrade it performs the guacd handshake; on failure it closes with the status code in the close reason (like the Java web app).
- guacd → browser: the bytes read are cut into instructions by the parser and batched into one message of up to 64 KiB, sent immediately when the next read would block. For a client on the binary subprotocol the messages are binary and made of records instead (section 2.6): records from molcajeted are batched as they are, without looking inside; instructions from a stock guacd are parsed as before and their `blob` data is decoded from base64 on the way.
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
| `img` (zstd image) | Kept as received until its frame is drawn, then decoded in wasm into a pixel buffer that is reused from image to image, and uploaded with `texSubImage2D` from a view of the wasm memory: straight into the layer texture, or into the upload texture when a transform, a clip or a composite mode other than SRC and OVER applies. No decoder object, no promise, no staging canvas (section 2.7). |
| `img` (PNG, JPEG, WebP) | The encoded image goes to the decoder as a `Uint8Array`: on the binary tunnel a view of the received message when the image arrived in one blob (no copy, and no pass through wasm memory), otherwise one joined array. Decoded with WebCodecs `ImageDecoder` (no colour-space conversion) into a `VideoFrame`; without `ImageDecoder`, Blob + `createImageBitmap`. PNG (IHDR colour type, tRNS) and WebP (VP8/VP8L/VP8X) headers tell whether the image is opaque; opaque images without transform or clip are uploaded straight into the layer texture, the rest go through a reused upload texture and are blended. Uploads are first drawn into a 2D `OffscreenCanvas` (deferred to the GPU process) and that canvas is copied with `texSubImage2D`. |
| `video` | Unpacks molcajete framing (a 9-byte header per frame + H.264/H.265 Annex B or AV1 OBUs, `molcajete_protocol::video`) into a WebCodecs `VideoDecoder` (`optimizeForLatency`, hardware decoding where available). Each frame takes an image slot in the frame queue, like `img`, filled by the decoder output (`VideoFrame`), so video follows `sync` order and the other drawing instructions. Decoded frames are uploaded to the texture as they are, on the GPU. Decoder errors are reported to the server with an `ack` error and the server falls back to images. |
| Paths (`arc`/`curve`/`line`/`clip`/`cstroke`/`lfill`) | Arcs and curves are flattened into polylines; fills (nonzero) and strokes are tessellated with lyon. Coverage is drawn into an R8 mask that a shader samples while compositing. |
| `clip` | Scissor for axis-aligned rectangles, otherwise a per-layer R8 clip mask (intersections by multiplicative blending) |
| `lfill`/`lstroke` | Pattern shader (repeats with `mod` over the logical size) |
| `push`/`pop`/`reset`/`transform` | Per-layer drawing state stack (CTM, clip, composite mode, miter limit) |
| `move`/`shade`/`distort`/`dispose` | Layer tree properties, applied when compositing |
| `cursor` | Copied into a cursor texture and read back **asynchronously** with a PBO + fence, so the GPU pipeline never stalls. The CSS cursor (data URL) is updated only when the pixels actually changed, so the local pointer has no lag; when the source layer did not change, the readback is skipped entirely. The server's `mouse` position is drawn as a software cursor. |

**Frames.** Instructions are queued as `Op`s, and once a whole frame up to `sync` is ready (images decoded) it runs at once. A zstd image is ready as soon as its `end` has arrived, so a frame made of them waits for nothing; it takes its place in the queue like any other image, which keeps the drawing order when PNG, JPEG or video frames are mixed in. When the frame has run, `sync` is sent back (with the time the frame took to apply, if the server asked for it, section 2.9) and the next `requestAnimationFrame` composites once. Intermediate states never reach the screen.

**Compositing.** The layer tree is walked and each layer drawn with its transform (`translate`, then `distort` about its centre), opacity and the parent's clip (scissor). A translucent layer with children is first rendered as a subtree offscreen and blended with the group opacity, like CSS `opacity`. The canvas is created with `desynchronized: true` (low latency), and linear filtering is used only for non-integer scale factors.

**Input.**
- Keyboard: the keysym is derived from `KeyboardEvent.key`/`code`/`location` (ported tables). The keysym sent on press is remembered per physical key (`code`) and sent again on release, so releasing Shift first does not leave an `A` pressed.
- IME (Korean etc.): composition is left to the browser, and the `compositionend` result is sent as Unicode keysyms (`0x01000000 | code point`) pressed and released.
- Remote composition (session menu, RDP and VNC, remembered per connection in the browser): the remote host's IME composes instead, as with native RDP and VNC clients. The input field becomes read-only so that the browser starts no composition, and a key the local input source claims (`key` is `Process` or a Hangul jamo) is sent as the US-layout character of its physical position (`code`, with Shift), so every key press reaches the remote side at once. `Lang1`/`HangulMode` are sent as `Hangul` (0xFF31); on macOS, which switches input sources with Caps Lock, a Caps Lock press that does not change the lock state is sent as `Hangul` too. molcajeted sends `Hangul` and `Hangul_Hanja` over RDP as scancodes 0x72 and 0x71, and `server-layout=ko-kr-qwerty` sets the session's input language to Korean (0x412). SSH has no remote IME and always composes in the browser.
- Mouse: Pointer Events with pointer capture. Wheel deltas accumulate in 53 px steps (same as the original).

**Other.** Clipboard (both directions, UTF-8, 3 KiB blobs), `required` requests (credential prompt → `argv` streams), `nop` every 5 s, a tunnel ping every second (RTT), disconnect after 15 s of silence, remote resize requests (`size`) when the container resizes and the desktop follows the window (section 2.8), and performance statistics events. Raw PCM audio (`audio/L8`, `audio/L16`) is played with Web Audio. Web Audio is not available in workers, so in worker mode the worker hands PCM to the page (`audio-start`/`audio-data`/`audio-end` events, ArrayBuffers transferred without copying) and the page's `PcmPlayer` schedules it with 50–300 ms of latency. The audio context resumes on the first pointer or key input (autoplay policy). While connected, the client keeps itself alive, so the session survives the JS handle being garbage-collected (wasm-bindgen FinalizationRegistry).

### 2.4 molcajeted

- Built on tokio, one task per connection (the original forks a process). The handshake matches the original (`args` advertises `VERSION_1_5_0`; argument names match the original VNC plugin), with one more name at the end of `args`, `molcajete-binary`, through which a molcajete gateway asks for raw stream data (section 2.6).
- Everything sent to the client goes through one `Output` (`io.rs`): instructions are appended as text, stream data with `Output::blob`, which writes base64 `blob` instructions of at most 6048 bytes for a stock client and blob records for a molcajete gateway. The protocol code (display, video, audio, clipboard) is the same for both.
- The server-side display (`display/`) keeps two buffers:
  - `pixels`: the actual screen as reported by the remote protocol
  - `sent`: a copy of the client's layer, built by replaying every instruction we send (fills, images, copies)
- At flush time only the 64×64 tiles touched in this frame are compared between the two:
  1. **Scroll detection**: row hashes find vertical moves, sent as `copy`.
  2. **Move detection in any direction** (dragged windows, ...): hashes of the changed 64×64 cells are looked up while a 64×64 window slides one pixel at a time over the client copy, using a 2D rolling hash (two odd multipliers, subtracting the rows and columns that leave the window). Cells that moved by the same offset are merged into large rectangles and sent as single `copy` instructions, front of the movement first. guacd 1.6 slides with multiplier 62 and no subtraction, which leaves the upper-left half of the block out of the hash and produces many false matches, so that approach was not used.
  3. **Remaining differences**: cut into changed row bands and changed column runs within them, then encoded. Unchanged pixels re-sent by the server cost nothing. A client is slowed by the number of images in a frame rather than by their size, so the resulting rectangles are joined into their bounding box wherever one image is estimated to be cheaper than two: an image is taken to cost 1 KB (PNG headers and palette, the `img`/`blob`/`end` instructions, and a decode on the client), the unchanged pixels a join adds 1 byte per 8 pixels, or per 128 pixels in rows that are one colour or repeat the row above. Lines of text that changed over the same columns therefore share an image, since the leading between them is flat, while changes side by side over unchanged text stay separate. Joined images stay below one megapixel so that a large update is still decoded as several images in parallel.
  4. **Encoding**: solid colours as `rect`+`cfill`, up to 256 colours as palette PNG (1/2/4/8 bit), everything else as RGB PNG (DEFLATE level 4 + Adaptive filter). A client that announces the zstd image type gets it in place of PNG for every opaque region that is not a photograph (section 2.7). Large, colourful areas that change often are sent as JPEG when the client supports it, and those tiles are re-sent losslessly once they calm down (lossless refinement).
- Because the instructions we send are replayed on the copy, wrong guesses correct themselves: a bad `copy` shows up as a difference in the next comparison and is re-sent as pixels. VNC CopyRect only needs the same `copy` applied to the copy, with no need to flush the source area first. This property is checked by a client simulator that replays the instructions and by randomized fuzz tests (1,000 seeds).
- Frame pacing: the next frame goes out after `max(floor, what the client needs per frame)` (at most 500 ms, the same idea as guacd), where the floor is 16 ms for RDP and VNC and 33 ms for SSH. A molcajete client reports how long it took to apply each frame, and the median of its recent reports is what it needs (section 2.9); the round trip is left to the window of unacknowledged frames. For any other client the send time of every `sync` is recorded, and the exponentially smoothed time until its reply (the processing lag) stands in for it. Remote updates keep arriving meanwhile and accumulate as differences from the copy, so short-lived intermediate states are never sent. The first change after a quiet period is sent immediately, so input stays responsive.
- Large frames (more than 32 KiB encoded) are paced by one of two profiles (`display/pacing.rs`, chosen with `Display::with_pacing`; all three protocols ask `Display::next_frame_at`):
  - **Terminal (SSH):** at least 66 ms (about 15 fps) pass after a large frame, however fast the client acknowledged it. This bounds bandwidth under continuous floods of output, where more frames show nothing more (section 3.2).
  - **Desktop (RDP, VNC):** the 66 ms only last until the client has acknowledged the large frame, which shows that the link delivered it and the client applied it. On a 4K or 5K screen nearly every frame of scrolling or dragging is larger than 32 KiB, and the fixed pause held those to 15 fps on links and clients that take 60. A link that needs longer than 66 ms per frame is paced as before, by the pause and then by the lag (for a client that reports apply times, by the pause and the window of unacknowledged frames). So that a fast link is not filled just because it can be, a byte budget of 8 MiB/s applies: a quarter of a second's worth may go out at once, beyond that frames are spaced evenly at the budget's rate (never more than 500 ms apart). While a video stream is active (section 2.5) the floor is 33 ms, the frame rate its rate control was set up for.
- Flow control: with more than 2 frames unacknowledged, no update requests are sent to the remote server (they resume when replies arrive, or forcibly after 5 s).
- VNC: RFB 3.3/3.7/3.8, None/VNC authentication, Raw/CopyRect/Hextile/ZRLE, Cursor/DesktopSize/ExtendedDesktopSize/LastRect pseudo-encodings, clipboard in both directions, remote resize.
- SSH (`ssh/`): connects with russh (password, private key + passphrase, keyboard-interactive, asking with `required` when a value is missing, `host-key` pinning) and keeps an xterm-compatible screen with alacritty_terminal (256 colours/true colour, alternate screen, scrollback, bracketed paste, SGR mouse). Cells are rendered on the server with ab_glyph, and only the row spans that differ from the previous frame go into the display; scroll detection and encoding are then up to the shared display. DejaVu Sans Mono is bundled, with the bundled NanumGothicCoding as fallback for Hangul, Han and Kana; `font-path` and system fonts are further fallbacks. Supports double-width characters, box-drawing characters, drag/double-click selection → client clipboard, Ctrl+Shift+V/right-click paste, wheel scrollback and resizing. Argument names match the guacd SSH plugin.
- RDP (`rdp/`): IronRDP (connector/session/graphics/cliprdr/displaycontrol) with rustls. Security: TLS and NLA (CredSSP/NTLM); certificates are verified unless `ignore-cert` is set. Bitmap updates go into the display per changed rectangle, copied once straight from IronRDP's decoded image (`Display::put_image` converts to opaque RGBA in the same pass; `rdp::bench` compares it with the earlier copy into a temporary buffer); pointer bitmaps become the cursor. No bulk compression is negotiated: IronRDP's reactivation sequence (its connection finalization) rejects compressed Share Data PDUs, which xrdp sends after a resize once compression is negotiated. Modifiers, function keys and shortcuts are sent as scancodes, other characters (including Hangul) as Unicode events. Mouse, text clipboard (both directions, UTF-16), display-update resizing and scale changes during the session (section 2.8) are supported. IronRDP does not implement RC4 standard RDP security, so `security=rdp` is refused.
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

### 2.6 Binary tunnel (raw stream data)

In the Guacamole protocol stream data is base64 inside `blob` instructions inside text WebSocket messages: 4 bytes on the wire for every 3 of an image, a video frame or an audio packet, on both legs (molcajeted → gateway → browser). In the browser each blob was copied four times: into a JavaScript string by the browser, into wasm as UTF-8, into the parser buffer, and out of the base64 decoder; the finished image was then copied back out for `ImageDecoder`. Between molcajete components the data now stays raw bytes end to end. Only the server → client direction changes; what the client sends (input, acknowledgements, clipboard and `argv` streams) is small and stays text.

**Records** (`molcajete_protocol::binary`). The server → client stream becomes a sequence of records:

```
record = tag (1 byte)  length (4 bytes, unsigned, little endian)  body (length bytes)

tag 0x00  instructions   body = Guacamole protocol text (UTF-8), whole instructions
tag 0x01  blob           body = stream index (4 bytes, unsigned, little endian) + raw data
                         same meaning as blob,<stream>,<base64 of data>;
other     skipped by readers (room for later additions)
```

- The instruction records, concatenated, are the ordinary instruction stream without its `blob` instructions; a blob record applies at its position in that stream. `img`, `video`, `audio`, `end` and `ack` are unchanged.
- Consecutive blob records of a stream are equivalent to one with the data joined. Writers keep a record within 64 KiB (`MAX_WRITTEN_RECORD`), so an image up to that size is one blob instead of one per 6048 bytes, and larger data is split (PCM only between whole sample frames). Readers accept bodies up to 16 MiB.
- On TCP (molcajeted → gateway) the records follow each other in the byte stream, starting right after the `;` of `ready`. On the WebSocket every binary message holds whole records.

**Negotiation.** Each leg is negotiated on its own, and each falls back to the text protocol when the peer does not know the extension:

- Browser ⇄ gateway: the client offers the WebSocket subprotocols `molcajete-binary-v1, guacamole`. The gateway selects the first one it supports; an older gateway, or one with `[server] binary_tunnel = false`, selects `guacamole`. The stock guacamole-common-js client offers only `guacamole` and gets exactly the stock tunnel. Under the binary subprotocol the data from the server arrives in binary messages; the tunnel's own messages (UUID, ping replies) are still text messages, and the client handles either kind at any time.
- Gateway ⇄ molcajeted: molcajeted lists the connection parameter `molcajete-binary` last in `args`. A gateway whose browser selected the binary subprotocol answers `1` in `connect`, and molcajeted switches to records after `ready`. In every other case the gateway sends an empty value (a connection parameter of that name in the database is ignored), as does the stock web application for a parameter it has no setting for, and molcajeted sends base64 like guacd. Stock guacd never lists the parameter and is sent nothing it does not know. (`video` mimetypes are announced the same way: something only molcajete peers act on.)

| Browser | Daemon | Browser ⇄ gateway | Gateway ⇄ daemon |
|---|---|---|---|
| molcajete-client | molcajeted | binary (records forwarded) | records |
| molcajete-client | stock guacd | binary (gateway decodes base64) | text |
| stock guacamole-common-js | molcajeted | text | text |
| stock guacamole-common-js | stock guacd | text | text |
| stock web application (Java) | molcajeted | — | text |

The gateway logs the mode of every tunnel as `framing` (`binary`, `transcoded`, `text`) in `tunnel established`, and molcajeted logs `binary` in `connection established`. Appending `?text` to the session URL keeps a client on the text protocol, for comparisons.

**Why records, and not binary messages for blobs only.** Carrying each blob as its own binary WebSocket message between text messages would have been the smallest change, but a text message cannot continue across a blob: a frame with *n* images would take about 2*n* messages where the gateway sent one or two, and each message is a task in the browser. With everything in records one message still carries a whole batch (up to about 64 KiB), the gateway forwards molcajeted's records without parsing instructions at all, and the client finds each part from its 5-byte header instead of scanning. The record size is capped, rather than sending an image of any size as one record, because the client treats 15 s without a message as a dead tunnel and a message only counts once it has arrived completely: bounded messages keep arriving on a slow link. A fully binary encoding of the instructions themselves was not worth it: outside the blobs the stream is a few percent of the bytes, and keeping it as text keeps one parser and one instruction encoder for both modes.

**In the client.** A binary message is an `ArrayBuffer`. The client reads each record header through a typed-array view, copies instruction text straight into the parser buffer (`Parser::feed_with`), and keeps stream data as views of the message:

- images: a single-blob image is handed to `ImageDecoder` as that view; an image in several blobs is joined once into a new array. Only the first 2 KiB enter wasm, to tell from the header whether the image is opaque.
- video: frames that lie whole within a blob (molcajeted starts each frame in a new record) go to `EncodedVideoChunk` as views; a frame larger than a record is reassembled by `FrameReader` as before.
- audio: PCM is copied once, into wasm for Web Audio or, in worker mode, into a buffer of its own that is transferred to the page (transferring the message itself would detach the image views).

The worker and main-thread modes share all of this; the text path (`String` → parser → base64 decoder) is still there for stock guacd behind an older gateway and for recordings.

**Compression.** WebSocket `permessage-deflate` is not in use and must stay that way for these payloads, which are already compressed: tungstenite (0.29, under axum 0.8) does not implement the extension, so the gateway never answers the browser's offer with `Sec-WebSocket-Extensions`. A test checks this for every tunnel mode. A reverse proxy passes WebSocket frames through unchanged.

**Effect on the wire.** `display::bench` runs fixed scenarios on a 5K screen through the display twice, once per output mode (`cargo test --release -p molcajete-daemon display::bench -- --ignored --nocapture --test-threads=1`, the `wire` lines). Bytes and blobs per frame, molcajeted → gateway and, forwarded unchanged, gateway → browser:

| Scenario | Text | Binary | Change |
|---|---|---|---|
| `scroll` (document scrolls) | 44.2 kB in 6.2 blobs | 33.1 kB in 1.0 blobs | −25.1% |
| `drag` (window dragged) | 66.9 kB in 11.4 blobs | 50.3 kB in 4.5 blobs | −24.9% |
| `corners` (two small changes) | 23.3 kB in 4.1 blobs | 17.5 kB in 2.0 blobs | −25.0% |
| `page` (full page of new text) | 1151.1 kB in 149.6 blobs | 861.8 kB in 16.9 blobs | −25.1% |
| `scatter` (40 small changes) | 173.6 kB in 39.0 blobs | 130.7 kB in 33.3 blobs | −24.7% |

The binary stream is 0.2–3% larger than the encoded images themselves (record headers and the instructions around them).

### 2.7 zstd images (a lossless image type the client decodes itself)

Every lossless update used to be a PNG. molcajeted paid for DEFLATE on each one, and the client for an `ImageDecoder` per image: a decoder object, a promise, a `VideoFrame` and, for most images, a pass through the staging canvas before the texture upload; a frame could only be applied once all of its images had come back from their decoders. For a client that announces it, molcajeted now sends screen content as a format the client decodes itself, in wasm, while it draws the frame.

**Format** (`molcajete_protocol::zimage`, mimetype `image/x-molcajete-zstd-v1`). An image is indexed or RGB pixels, as they are, in one Zstandard frame:

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
- An image has at most 2^25 pixels (an 8K screen has 33.2 million).
- The frame uses no dictionary and is the only one; nothing follows it. It decompresses to exactly the number of bytes the header implies. Its window is at most 8 MiB, unless it is a single-segment frame, whose window is its content. A decoder can therefore check, before it allocates anything, that it will need no more than the image itself and 8 MiB.
- An index that has no palette entry is black.
- A decoder refuses anything else, and an image it refuses is skipped like an image the browser cannot decode: its frame is still applied. The version is part of the mimetype; a change to the format is a new mimetype, announced next to this one.

The decoder (`zimage::Decoder`) checks the header and the frame header itself and leaves the Zstandard frame to `ruzstd`, a pure Rust decoder, writing into a buffer of the size the header implies. It never panics on any input and never produces more or fewer pixels than announced; tests feed it images cut at every length, every byte replaced, bytes removed and inserted, and random data.

**Negotiation.** The client lists the mimetype in the `image` instruction of the handshake, after the stock ones (the tunnel URL carries it as one more `image=` parameter, which every gateway passes on). molcajeted uses the type only for a client that listed it, so the stock clients and older molcajete clients get PNG as before, and a stock guacd ignores a mimetype it does not know. Nothing depends on the tunnel: the data is an `img` stream like any other, in blob records or in base64 `blob` instructions. `--image-zstd false` turns the type off in a daemon and `?png` in a session.

**Which images.** molcajeted chooses per image (`display/encode.rs`):

| Region | Sent as |
|---|---|
| one colour | `rect` + `cfill`, as before |
| opaque, up to 256 colours | zstd image, indexed |
| opaque, more colours, at least 3 of 4 pixels equal to their left neighbour (in every fourth row) | zstd image, RGB |
| opaque, more colours, otherwise: a photograph, a gradient | PNG (RGB, adaptive filter) |
| any transparent pixel; the cursor | PNG (the format has no alpha) |
| wider or higher than 65535 pixels, or more than 2^25 pixels | PNG |
| large, colourful and changing often | JPEG, as before; refined later by this table |

The share of repeated pixels tells screen content from photographs well: it is 94 to 97% in the windows and pages with more than 256 colours of the recorded sessions, and 3 to 15% in photographs. Without a row filter Zstandard needs 22 to 74% more bytes than PNG for a photograph, and 37 to 45% fewer for those windows, so the rule keeps PNG exactly where its filters earn something. It costs one look at a quarter of the rows of an image that has already failed to fit a palette; `display::bench::zstd_images` measures it on a 5120x512 photograph (0.17 ms, against 83 ms to encode it as PNG).

**In the client.** A zstd image is not decoded when it arrives. Its bytes are queued (`QueuedImage::Zstd`) in the place the `img` instruction takes in its frame, and the slot counts as ready from the moment `end` arrives, so the frame waits for nothing because of it. When the frame is drawn, the image is decoded into a pixel buffer that is reused from image to image and uploaded with `texSubImage2D` from a view of the wasm memory, into the layer texture when it is drawn with SRC or OVER and no transform or clip, and through the upload texture otherwise, exactly like a decoded PNG. The pixels are opaque, so straight and premultiplied alpha are the same and the upload asks the browser for no conversion. PNG, JPEG and video frames of the same frame still wait for their decoders, and the frame with them; the order of drawing is that of the instructions in every mix.

**Why this encoding.** The candidates of GH-24 were compared on the images of two recorded RDP sessions (a list being scrolled: 1194 images, 72.6 megapixels, 95% of them with at most 256 colours; pages being opened: 49 images, 14.1 megapixels, 57% of the pixels in two windows with more colours) and on the images `display::bench` produces (text whose glyphs are noise, which is pessimistic). Bytes are relative to the PNG molcajeted sends; encoding was timed natively on an Apple M4, decoding in wasm without SIMD in headless Chromium 1228 on the same machine, warm, per megapixel.

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

- **QOI and LZ4** have no entropy coder. They decode fastest, but LZ4 takes 22 to 80% more bytes than PNG and QOI two and a half to ten times as many, and the bytes of a large frame are what a session waits for longest (GH-22).
- **Row filters** make screen content larger, not smaller, under an LZ coder: an exact repetition of a glyph is no longer one after filtering. They only pay for photographs, which stay PNG.
- **The palette** is what makes the difference on text: the same pixels as RGB take half as many bytes again. Packing the indices like PNG instead of keeping them as bytes saves 3% at most.
- **Zstandard against DEFLATE** over the same pixels: about the same bytes (3% more on bench `scatter`, a quarter fewer on the recorded pages) at 40% of the encoding time, against a decoder that is slower in wasm (`ruzstd` reaches 350 to 500 MB/s there, `miniz_oxide` about 750 MB/s) and 25 kB larger. Level 3 costs 5% more time than level 1 for 4% fewer bytes (11% fewer on the pages); level 5 one and a half to two times the time of level 3 for another 2 to 3%.
- **Decoding PNG in wasm** would have served stock guacd too, with no new format, but saves molcajeted nothing and adds the most code.
- **Alpha** was left out: molcajeted sends transparent pixels only in cursors and in regions a remote protocol reports as transparent, which are rare and stay PNG, and without alpha the question of premultiplying exactly like the browser's PNG decoder does not arise.

The comparison tool depends on the recordings, which show a private desktop, and is not in the repository; `display::bench` and `display::bench::zstd_images` reproduce the synthetic part.

**Effect in molcajeted** (`display::bench`, a 5K screen, per frame, PNG client → zstd client):

| Scenario | Encoding | Whole flush | Image bytes | zstd images of all images |
|---|---|---|---|---|
| `page` (full page of new text) | 57.3 → 17.9 ms | 76.8 → 37.4 ms | 860.5 → 799.9 kB (−7.0%) | 7.5 of 7.5 |
| `scatter` (40 small changes) | 7.2 → 2.6 ms | 12.5 → 7.9 ms | 128.1 → 127.8 kB | 33.3 of 33.3 |
| `scroll` (document scrolls) | 2.2 → 0.7 ms | 14.3 → 13.5 ms | 33.0 → 32.1 kB (−2.7%) | 1.0 of 1.0 |
| `corners` (two small changes) | 1.1 → 0.4 ms | 1.7 → 1.0 ms | 17.3 → 17.2 kB | 2.0 of 2.0 |
| `drag` (window dragged over a wallpaper) | 1.0 → 0.7 ms | 7.7 → 7.4 ms | 49.0 → 48.4 kB (−1.2%) | 2.5 of 4.5 (the wallpaper stays PNG) |

The wasm client grows by 50 kB (692,392 → 742,844 bytes as built by `scripts/build-web.sh` without `wasm-opt`; 214,863 → 231,640 bytes with `gzip -9`). molcajeted links libzstd (`zstd` crate), compiled from the source bundled with `zstd-sys`.

**Effect in the client.** The two recordings were replayed into the client in headless Chromium 1228 on an Apple M4, once as recorded (PNG) and once with the same images converted by molcajeted's rule (1187 of 1217 and 49 of 68 images become zstd images; the rest are cursors and a few small photographs). The replay feeds text messages (base64), so it shows nothing of the binary tunnel.

On the GPU (ANGLE on Metal), ranges of 3 or 4 runs:

| Replay | Measure | PNG | zstd |
|---|---|---|---|
| list, `?main`, messages fed back to back | until every frame is applied | 0.35–0.40 s | 0.26–0.28 s |
| | frame lag (from `sync` to applied), average | 3.9–5.7 ms | 0.48–0.50 ms |
| | main thread: tasks, of which script | 355–405 ms, 299–353 ms | 268–280 ms, 50–58 ms |
| pages, `?main`, messages fed back to back | frame lag, average | 13.4–17.0 ms | 0.72–0.86 ms |
| | main thread: tasks, of which script | 71–96 ms, 55–74 ms | 92–102 ms, 38–56 ms |
| list, worker, recorded timing (25 s) | frame lag, average | 1.05–1.56 ms | 1.35–1.48 ms |
| | per image | 0.49–0.75 ms from handing it to the decoder to having it | 0.39–0.51 ms decoding |
| list, `?main`, recorded timing | frame lag, average | 1.42–1.51 ms | 1.35–1.55 ms |
| | main thread: tasks, of which script | 862–927 ms, 514–554 ms | 956–1071 ms, 102–113 ms |
| pages, worker, recorded timing (20 s) | frame lag, average | 1.91–2.04 ms | 1.43–1.62 ms |
| pages, `?main`, recorded timing | frame lag, average | 1.74–2.08 ms | 1.34–1.68 ms |
| | main thread: tasks, of which script | 150–189 ms, 70–102 ms | 213–251 ms, 57–73 ms |

- When frames queue up, a frame of zstd images is applied in a tenth of the time, because nothing in it is waited for, and the client gets through the backlog of the list about 40% sooner.
- At the pace of a session with about three images per frame, on an otherwise idle machine, the time from `sync` to applied is the same within the spread of the runs: a PNG is back from its decoder after about as long as a zstd image takes to decode. Per megapixel these decodes were three to four times slower than in the warm measurement above, as everything is on a thread that sleeps between frames.
- The script time of the client falls (to a fifth on the list), but the thread that draws now also carries the pixels to the GPU process: with `?main` the main thread's task time was 10 to 40% higher at recorded timing than with PNG, whose decoding and staging happen elsewhere. In the default worker mode that thread is the worker.
- Without a GPU the upload dominates. On SwiftShader, which the tests use, `texSubImage2D` from memory took about 5.5 ms per megapixel on the calling thread, where the staging canvas costs it nearly nothing, and the backlog of the list took 0.75–0.85 s as zstd images against 0.28 s as PNG; at recorded timing the frame lag was 0.67–0.70 ms against 0.55–0.58 ms.

Not measured: a deployed session (network, a 5K display, other GPUs and browsers), the worker's own CPU time, and the binary tunnel in the replay.

### 2.8 Remote display size and scale (RDP)

The browser decides two things about an RDP desktop: whether it takes the size of the window (`resize-method=display-update`), and its scale (`dpi`, 96 per 100 %; without one the DPI is `96 × devicePixelRatio` of the browser that connects). Both are connection parameters with guacd's names and meanings, set in the connection form; new RDP connections resize to the window.

**Per browser.** The session menu changes them for one connection in one browser (`remoteScale.<id>`, `resizeToWindow.<id>` in `localStorage`), over the connection's values:

- At connect the page adds `param.dpi=<dpi>` and `param.resize-method=display-update` to the tunnel URL. The gateway replaces the connection's parameters with them for that tunnel; an empty value removes the parameter, so "Automatic" makes the DPI follow the browser even when the connection has a fixed one. No other parameter can be set this way, and values the daemons would not read are ignored. The connection list returns `dpi` and `resizeMethod`, so that the menu can show what is in effect without the connection's other parameters.
- During the session the page sends the new value as an `argv` stream, the protocol's way to change a parameter while connected (guacd uses it for credentials and for a terminal's font and colours). `size` stays as it is: a third argument would be read by molcajeted but not by stock guacd, and `argv` also carries `resize-method`. Stock guacd does not apply these two during a session, so with guacd a change takes effect at the next connection.
- The client sends `size` only while the desktop follows the window, and sends the current size when that is turned on.

**molcajeted.** Display Control ([MS-RDPEDISP]) is offered to every server, so that resizing can start during a session. A new size and a new scale both go out as a monitor layout: the size the desktop has or is about to get, with `DesktopScaleFactor` from the DPI (100–500 %) and `DeviceScaleFactor` at Windows' plateaus 100, 140 and 180 % (125 % gives 100, 175 % gives 140, as on Windows itself). Requests are applied after 500 ms without another one.

**Servers without Display Control.** molcajeted sends `display-control,0` when changes cannot be applied, and `display-control,1` when they can; the session menu then disables Remote scale and Resize to window, and Fit stays. The server opens the channel when it likes, and IronRDP cannot tell "not yet" from "never", so the channel counts as missing when the server did not join `drdynvc`, or 10 s after the session became active (checked with the 5 s keepalive). A channel that opens later is reported, and requests made while it was missing are dropped. Stock guacd never sends `display-control`; clients that do not know it ignore it.

Windows applies a new scale to applications that support per-monitor DPI; others keep their size until they restart, and some ask the user to sign out. The menu says so.

### 2.9 Apply-time reports (pacing frames by what the client needs)

molcajeted paces frames by the client (section 2.4). From the reply to `sync` alone it only learns the whole round trip: the network both ways, the frame's data arriving, and the client's decoding and drawing. Pacing by that sent fewer frames the farther away the client was, although the window of unacknowledged frames (`MAX_UNACKED_FRAMES`: 3 for RDP and VNC, 2 for SSH) already keeps the daemon from running ahead of a client. An average of the replies also took a quarter of every slow one, and only replies moved it again: a slow client on a deployed 2732×1876 session got 1 to 5 frames per second while it took about 17 ms per frame and the link was nearly idle (GH-38).

**Format** (`molcajete_protocol::apply_time`). The daemon asks in its `sync`, the client answers in its reply:

```
server -> client   sync,<timestamp>,<frames>,apply-us;
client -> server   sync,<timestamp>,<frames>,<microseconds>;
```

- The time is how long the frame kept the client busy, in whole microseconds: from the moment the frame was complete (its `sync` arrived) or, if later, the frame before it was applied, to the moment it was applied. Waiting for its images to decode counts; waiting behind earlier frames does not, as that is time the earlier frames already reported.
- The client answers a `sync` that did not ask as a stock client does (`sync,<timestamp>`), so stock guacd, which never asks, gets nothing new.

**Negotiation.** None in the handshake: the question is in every `sync`, and only a client that understands it answers. guacamole-common-js reads the timestamp and the frame count of a `sync` and ignores further arguments, and its reply carries no time; molcajeted reads a time only from the third argument of a reply. The gateway passes both directions through unchanged, over the text and the binary tunnel alike, so any gateway version works.

**Pacing** (`display/pacing.rs`). For a client that reports, the interval is the median of the last 8 reported times (the lower one of an even count), counting only those reported within 1 s before the last frame, at least the profile's floor and at most 500 ms. The round trip is left to the frame window. A single slow frame does not move a median, and after a pause of a second the interval starts afresh, without waiting for replies to pull it down. Everything else stays: the pause after a heavy frame (until acknowledged on desktops, fixed on terminals), the byte budget, and the 33 ms floor while a video stream is active. A client that does not report is paced by the smoothed round trip as before.

**Effect.** The pacing unit tests run the same simulated clients against the stock path (no reports) and the reporting path (`cargo test -p molcajete-daemon display::pacing::tests::table -- --ignored --nocapture`, 8 s of continuous changes each). A client reports its time from when a frame reaches it, after half the round trip, and applies frames one after the other:

| Client | fps stock / reporting | most frames unacknowledged | bytes sent |
|---|---|---|---|
| desktop, fast (2 ms round trip + 1 ms to apply, 20 kB frames) | 62.5 / 62.5 | 1 / 1 | 10.00 / 10.00 MB |
| desktop, slow (20 ms replies, 500 ms for every 25th frame, 5 kB frames) | 15.9 / 29.0 | 3 / 3 | 0.64 / 1.16 MB |
| desktop, far (100 ms round trip + 5 ms to apply, 5 kB frames) | 10.0 / 28.8 | 3 / 3 | 0.40 / 1.15 MB |
| SSH flood, 50 kB frames, 1 ms replies | 15.2 / 15.2 | 1 / 1 | 6.10 / 6.10 MB |
| SSH flood, 50 kB frames, 30 ms replies | 15.2 / 15.2 | 1 / 1 | 6.10 / 6.10 MB |
| SSH, light frames, 1 ms replies | 30.4 / 30.4 | 1 / 1 | 0.49 / 0.49 MB |

The slow client can apply at most 28.4 frames per second (24 × 16 ms + 496 ms per 25 frames), so reporting brings it to what it can take; the frame window holds what is queued at the client to three frames. The SSH flood of section 3.2 sends the same bytes, since the pause after heavy frames sets its rate. `molcajeted_client_apply_seconds` records the reported times.

## 3. Verification

- Unit tests: protocol (every split point, code point lengths, limits, invalid UTF-8, binary records at every split point), gateway (fake guacd handshake, error mapping, query parsing, tunnels of all four client/daemon combinations through a real WebSocket), daemon (handshake with and without the binary parameter, display merging/copy ordering/blob splitting, the same frame as text and as records, PNG round trips, zstd image round trips through the client's decoder and the choice between zstd and PNG), client logic (path tessellation area, keysyms, key release pairing, wheel, PCM).
- Binary tunnel in the browser (`tests/browser/binary.test.mjs`): a stand-in gateway sends one frame over a real WebSocket (a PNG larger than a record, a PNG with transparency, a JPEG, PCM audio and an H.264 stream cut at arbitrary points) as records to a client offering the binary subprotocol and as base64 instructions to one that does not, in worker and main-thread mode; all four must show the same pixels. The frame's plain `sync` must get the stock reply, and a following `sync` that asks for the apply time (section 2.9) a reply with it.
- zstd images: the decoder is tested on truncated images at every length, on every byte of an image replaced, on bytes removed and inserted and on random data (it must fail or produce exactly the announced number of opaque pixels, and never panic). In the browser, `renderer.test.mjs` checks the pixels of indexed images at every index depth and of RGB images, the paths through a clip, a transform and the composite modes SRC and ROVER, damaged images, and a frame in which zstd, PNG and JPEG images overlap; `binary.test.mjs` sends a frame with zstd images (one larger than a record, others overlapping a PNG and a JPEG) over both tunnels in both modes and checks that the mimetype is announced, or not when turned off.
- Browser pixel tests (`tests/browser`): 27 scenarios checked against the canvas reference implementation.
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

Over SSH, molcajeted first used its own 60 fps cap and sent 9.1 MiB (24 fps). With the shared pacing (30 fps cap + client lag compensation) and the "wait 66 ms after a large frame (over 32 KiB)" rule it sends about 24% less than guacd at the same frame rate. SSH still paces this way (a unit test replays the flood against the pacing rule, with and without apply-time reports, section 2.9); RDP and VNC have since moved to the desktop profile of section 2.4, so the RDP row shows the earlier, fixed pause and a flooding terminal inside a remote desktop now gets more frames and bytes when the link and the client keep up. RDP gets no server-side `copy` hints, because IronRDP does not pass drawing orders such as ScrBlt through, but the shared display's comparison and move detection keep it on par with guacd.

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
6. molcajeted encoding: frame pacing from a measured link rate and per-connection limits (the byte budget is a fixed 8 MiB/s), parallel encoding, lossy WebP (as in guacd 1.6). zstd images: the cost of one more image (section 2.4) was estimated for PNG and not looked at again; indexed images could be uploaded as indices and expanded by a shader, a quarter of the bytes the drawing thread carries to the GPU now; a faster Zstandard decoder for wasm than `ruzstd`.
7. Binary tunnel: client → server streams (clipboard, and file upload or audio input once they exist) are still base64 text; the gateway forwards records from molcajeted without checking the instructions inside.
8. Video mode
   - Hardware encoders (NVENC/QSV/VA-API/AMF) are wired up but not verified on real hardware. VideoToolbox is verified on Apple M4.
   - Forced key frames restart ffmpeg, which costs an extra 25–45 ms.
   - Build the client codec string exactly from the SPS/sequence header (currently a generous fixed value).
   - Several video areas at once, bitrate adapted to bandwidth.

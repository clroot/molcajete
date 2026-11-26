//! GPU-accelerated Guacamole client for the browser.
//!
//! The protocol is parsed in WebAssembly, images are decoded off the main
//! thread with WebCodecs `ImageDecoder` (zstd images in wasm), and every
//! layer lives in GPU memory as a WebGL2 texture. See `docs/architecture.md` for the overall design.

pub mod audio;
pub mod display;
pub mod geom;
pub mod gl;
pub mod image;
pub mod input;
pub mod png;
pub mod recording;
pub mod scope;

#[cfg(target_arch = "wasm32")]
mod client;
#[cfg(target_arch = "wasm32")]
mod tunnel;
#[cfg(target_arch = "wasm32")]
mod video;

#[cfg(target_arch = "wasm32")]
pub use wasm_api::MolcajeteClient;

#[cfg(target_arch = "wasm32")]
mod wasm_api {
    use wasm_bindgen::prelude::*;
    use web_sys::HtmlElement;

    use crate::client::{Inner, ScaleMode, Shared, with_inner};

    #[wasm_bindgen(start)]
    pub fn start() {
        console_error_panic_hook::set_once();
    }

    /// JavaScript handle of a client bound to a container element.
    ///
    /// Events are delivered to the handler installed with
    /// [`MolcajeteClient::set_event_handler`] as `(type, detail)`:
    /// `state`, `error`, `name`, `clipboard`, `resize`, `required`,
    /// `unstable`, `stats`, `msg`, `focus`; for files `filesystem`,
    /// `undefine`, `get-error`, `stream-start`, `stream-data`,
    /// `stream-end` and `stream-ack`.
    #[wasm_bindgen]
    pub struct MolcajeteClient {
        inner: Shared,
    }

    #[wasm_bindgen]
    impl MolcajeteClient {
        /// Creates the display canvas inside `container`.
        #[wasm_bindgen(constructor)]
        pub fn new(container: HtmlElement) -> Result<MolcajeteClient, JsValue> {
            Ok(MolcajeteClient {
                inner: Inner::create(container)?,
            })
        }

        /// Creates a client rendering into an `OffscreenCanvas` (normally in a
        /// dedicated worker; see `web/molcajete-worker.js`). Input and layout are
        /// then supplied with [`MolcajeteClient::pointer`], [`MolcajeteClient::key_down`],
        /// [`MolcajeteClient::set_viewport`] and friends.
        pub fn new_offscreen(canvas: web_sys::OffscreenCanvas) -> Result<MolcajeteClient, JsValue> {
            Ok(MolcajeteClient {
                inner: Inner::create_offscreen(canvas)?,
            })
        }

        /// Container size in CSS pixels and device pixel ratio.
        pub fn set_viewport(&self, width: f64, height: f64, device_pixel_ratio: f64) {
            self.run(|inner| inner.set_viewport(width, height, device_pixel_ratio));
        }

        /// Pointer position (CSS pixels relative to the canvas) and DOM
        /// `buttons` bitmask.
        pub fn pointer(&self, x: f64, y: f64, buttons: u16) {
            self.run(|inner| inner.pointer(x, y, buttons));
        }

        /// Wheel event (`deltaY`, `deltaMode`).
        pub fn wheel(&self, delta_y: f64, delta_mode: u32) {
            self.run(|inner| inner.wheel(delta_y, delta_mode));
        }

        /// `keydown`; returns whether the default action should be prevented.
        /// `caps_lock` is `getModifierState("CapsLock")` of the event.
        pub fn key_down(
            &self,
            key: &str,
            code: &str,
            location: u32,
            composing: bool,
            caps_lock: bool,
        ) -> bool {
            let mut consumed = false;
            self.run(|inner| consumed = inner.key_down(key, code, location, composing, caps_lock));
            consumed
        }

        /// `keyup`; returns whether the default action should be prevented.
        pub fn key_up(&self, key: &str, code: &str, location: u32, caps_lock: bool) -> bool {
            let mut consumed = false;
            self.run(|inner| consumed = inner.key_up(key, code, location, caps_lock));
            consumed
        }

        /// Lets the remote host's IME compose text (Hangul, …): keys are
        /// sent by physical position as they are pressed instead of sending
        /// the text the local IME composed. With `caps_lock_toggles`, Caps
        /// Lock is sent as the Hangul/English key.
        pub fn set_remote_composition(&self, remote: bool, caps_lock_toggles: bool) {
            self.run(|inner| inner.set_remote_composition(remote, caps_lock_toggles));
        }

        /// Forwards audio streams to the page as `audio-start`, `audio-data`
        /// and `audio-end` events instead of playing them (worker mode).
        pub fn set_audio_forwarding(&self, enabled: bool) {
            self.run(|inner| inner.set_audio_forwarding(enabled));
        }

        /// Whether the page is visible (`document.visibilityState`). The time
        /// it is hidden does not count as frame lag: the browser may suspend
        /// the page, and a worker cannot see that itself.
        pub fn set_page_visible(&self, visible: bool) {
            self.run(|inner| inner.set_page_visible(visible));
        }

        /// Types text (finished IME composition, paste-as-typing).
        pub fn type_text(&self, text: &str) {
            self.run(|inner| inner.type_text(text));
        }

        fn run(&self, f: impl FnOnce(&mut Inner)) {
            with_inner(&std::rc::Rc::downgrade(&self.inner), f);
        }

        pub fn set_event_handler(&self, handler: Option<js_sys::Function>) {
            self.run(|inner| inner.set_handler(handler));
        }

        /// Tunnel URL for `connection`, including display size, DPI,
        /// timezone and supported media types.
        pub fn tunnel_url(&self, base: &str, connection: &str) -> String {
            self.inner.borrow().tunnel_url(base, connection)
        }

        /// Whether to offer the gateway the binary tunnel subprotocol, in
        /// which image, video and audio data arrives as raw bytes instead of
        /// base64 (on by default; takes effect at the next `connect`).
        pub fn set_binary_tunnel(&self, enabled: bool) {
            self.run(|inner| inner.set_binary_tunnel(enabled));
        }

        /// Whether to announce the zstd image type, which molcajeted then
        /// sends instead of PNG (on by default; takes effect at the next
        /// `tunnel_url`).
        pub fn set_zstd_images(&self, enabled: bool) {
            self.run(|inner| inner.set_zstd_images(enabled));
        }

        /// The WebSocket subprotocol the gateway selected: `guacamole`
        /// (text) or `molcajete-binary-v1`. Empty before the tunnel is open.
        pub fn tunnel_protocol(&self) -> String {
            self.inner.borrow().tunnel_protocol()
        }

        /// Opens the tunnel at `url` (see [`MolcajeteClient::tunnel_url`]).
        pub fn connect(&self, url: &str) -> Result<(), JsValue> {
            let mut result = Ok(());
            self.run(|inner| result = inner.connect(url));
            result
        }

        pub fn disconnect(&self) {
            self.run(|inner| inner.disconnect());
        }

        pub fn focus(&self) {
            self.inner.borrow().focus();
        }

        pub fn send_clipboard(&self, text: &str) {
            self.run(|inner| inner.send_clipboard(text));
        }

        /// Pastes text into a terminal (sends it, then Ctrl+Shift+V).
        pub fn paste_text(&self, text: &str) {
            self.run(|inner| inner.paste_text(text));
        }

        /// Provides a value requested by the server via `required`.
        pub fn send_argv(&self, name: &str, value: &str) {
            self.run(|inner| inner.send_argv(name, value));
        }

        pub fn send_key(&self, keysym: u32, pressed: bool) {
            self.run(|inner| inner.send_key(keysym, pressed));
        }

        /// Asks the filesystem object `object` (announced by a `filesystem`
        /// event) for a directory listing or a file at `name`. The answer
        /// arrives as a stream (`stream-start`) or a `get-error` event.
        pub fn get(&self, object: i32, name: &str) {
            self.run(|inner| inner.get(object, name));
        }

        /// Acknowledges an incoming file stream (`stream-start`, then each
        /// `stream-data`); an error `code` stops it.
        pub fn ack_stream(&self, stream: i32, message: &str, code: u16) {
            self.run(|inner| inner.ack_stream(stream, message, code));
        }

        /// Opens an upload into `object` at the path `name`, or with a
        /// negative `object` into the server's upload directory. Returns the
        /// stream (-1 if none is free); the server acknowledges it and each
        /// blob with `stream-ack` events.
        pub fn open_upload(&self, object: i32, mimetype: &str, name: &str) -> i32 {
            let mut stream = -1;
            self.run(|inner| stream = inner.open_upload(object, mimetype, name));
            stream
        }

        pub fn upload_blob(&self, stream: i32, data: &[u8]) {
            self.run(|inner| inner.upload_blob(stream, data));
        }

        pub fn end_upload(&self, stream: i32) {
            self.run(|inner| inner.end_upload(stream));
        }

        /// Presses and releases Ctrl+Alt+Delete.
        pub fn send_ctrl_alt_del(&self) {
            self.run(|inner| {
                for (keysym, pressed) in [
                    (0xFFE3, true),
                    (0xFFE9, true),
                    (0xFFFF, true),
                    (0xFFFF, false),
                    (0xFFE9, false),
                    (0xFFE3, false),
                ] {
                    inner.send_key(keysym, pressed);
                }
            });
        }

        pub fn send_size(&self, width: u32, height: u32) {
            self.run(|inner| inner.send_size(width, height));
        }

        /// `"fit"` (shrink to the container) or `"native"` (1:1 device pixels).
        pub fn set_scale_mode(&self, mode: &str) {
            let mode = if mode == "native" {
                ScaleMode::Native
            } else {
                ScaleMode::Fit
            };
            self.run(|inner| inner.set_scale_mode(mode));
        }

        /// Whether container size changes are sent to the server as `size`.
        /// Turning it on also sends the current size.
        pub fn set_auto_resize(&self, enabled: bool) {
            self.run(|inner| inner.set_auto_resize(enabled));
        }

        pub fn release_keys(&self) {
            self.run(|inner| inner.release_keys());
        }

        pub fn display_width(&self) -> u32 {
            self.inner.borrow().display_size().0
        }

        pub fn display_height(&self) -> u32 {
            self.inner.borrow().display_size().1
        }

        /// Loads a session recording (Guacamole's format) for playback in
        /// place of a session and returns its duration in milliseconds.
        pub fn load_recording(&self, data: Vec<u8>) -> f64 {
            let mut duration = 0.0;
            self.run(|inner| duration = inner.load_recording(data));
            duration
        }

        /// Plays the loaded recording up to `time` milliseconds from its
        /// start, starting over for a time before what was played. Feeds at
        /// most about `max_bytes` per call; returns whether `time` was
        /// reached. Call again (after letting images decode) until it is.
        pub fn playback_advance(&self, time: f64, max_bytes: usize) -> bool {
            let mut done = true;
            self.run(|inner| done = inner.playback_advance(time, max_bytes));
            done
        }

        /// Time of the last instruction played, in milliseconds from the
        /// start of the recording.
        pub fn playback_time(&self) -> f64 {
            self.inner.borrow().playback_time()
        }

        /// Unix milliseconds at which the recording starts.
        pub fn playback_start(&self) -> f64 {
            self.inner.borrow().playback_start()
        }

        /// Frames of the recording fed but not applied yet, because their
        /// images are still being decoded.
        pub fn playback_pending(&self) -> usize {
            self.inner.borrow_mut().display().pending_frames()
        }

        pub fn unload_recording(&self) {
            self.run(|inner| inner.unload_recording());
        }

        /// Whether changes are drawn to the canvas; off while seeking, so
        /// that nothing on the way is drawn.
        pub fn set_presenting(&self, presenting: bool) {
            self.run(|inner| inner.set_presenting(presenting));
        }

        /// Feeds raw protocol text as if received from the server.
        pub fn debug_feed(&self, text: &str) {
            self.run(|inner| inner.feed(text));
        }

        /// Selects how decoded images reach the GPU (benchmarking aid).
        pub fn debug_set_staged_uploads(&self, enabled: bool) {
            self.inner
                .borrow_mut()
                .display()
                .renderer_mut()
                .set_staged_uploads(enabled);
        }

        /// Draws to the canvas as soon as a frame is applied (`true`) or in
        /// the next animation frame (`false`, the default), for comparing
        /// the two (benchmarking aid).
        pub fn debug_set_early_composite(&self, enabled: bool) {
            self.run(|inner| inner.set_early_composite(enabled));
        }

        /// Number of display operations still waiting (e.g. image decodes).
        pub fn debug_queued(&self) -> usize {
            self.inner.borrow_mut().display().queued()
        }

        /// Premultiplied RGBA of a layer pixel, or `undefined`.
        pub fn debug_layer_pixel(&self, layer: i32, x: i32, y: i32) -> Option<Vec<u8>> {
            self.inner
                .borrow_mut()
                .display()
                .debug_layer_pixel(layer, x, y)
        }

        /// `[width, height]` of a layer, or `undefined`.
        pub fn debug_layer_size(&self, layer: i32) -> Option<Vec<u32>> {
            self.inner
                .borrow_mut()
                .display()
                .debug_layer_size(layer)
                .map(|(w, h)| vec![w, h])
        }

        /// Composites and returns the RGBA of a canvas pixel.
        pub fn debug_canvas_pixel(&self, x: i32, y: i32) -> Option<Vec<u8>> {
            self.inner
                .borrow_mut()
                .display()
                .debug_canvas_pixel(x, y)
                .map(|p| p.to_vec())
        }

        pub fn state(&self) -> String {
            format!("{:?}", self.inner.borrow().state()).to_lowercase()
        }
    }
}

//! GPU-accelerated Guacamole client for the browser.
//!
//! The protocol is parsed in WebAssembly, images are decoded off the main
//! thread with `createImageBitmap`, and every layer lives in GPU memory as a
//! WebGL2 texture. See `docs/architecture.md` for the overall design.

pub mod audio;
pub mod display;
pub mod geom;
pub mod gl;
pub mod image;
pub mod input;
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
    /// `unstable`, `stats`, `msg`, `focus`.
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

        /// Provides a value requested by the server via `required`.
        pub fn send_argv(&self, name: &str, value: &str) {
            self.run(|inner| inner.send_argv(name, value));
        }

        pub fn send_key(&self, keysym: u32, pressed: bool) {
            self.run(|inner| inner.send_key(keysym, pressed));
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

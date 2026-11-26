//! Client core: owns the tunnel, the display and the input handlers, and
//! dispatches Guacamole instructions.
//!
//! All browser callbacks (WebSocket, timers, input events, image decoding)
//! funnel through [`with_inner`], which borrows the shared state, runs the
//! handler, and only then calls into JavaScript. This keeps re-entrant calls
//! from user code from ever hitting an active `RefCell` borrow.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use base64::Engine;
use molcajete_protocol::server::ServerInstruction as S;
use molcajete_protocol::{InstructionBuilder, MouseButtons, Parser, Status, apply_time};
use wasm_bindgen::prelude::*;
use wasm_bindgen::{Clamped, JsCast};
use web_sys::{
    AddEventListenerOptions, AudioContext, AudioContextState, CanvasRenderingContext2d,
    CompositionEvent, Event, EventTarget, HtmlCanvasElement, HtmlElement, HtmlTextAreaElement,
    ImageData, InputEvent, KeyboardEvent, PointerEvent, ResizeObserver, WheelEvent,
};

use crate::audio::{RawAudioPlayer, RawFormat};
use crate::display::{DisplayEvent, DrawOp, ImageSlot, ImageState, Op, QueuedImage};
use crate::geom::Rect;
use crate::gl::CanvasTarget;
use crate::input::{self, Composition, HeldModifiers, Hold, Keyboard, WheelAccumulator};
use crate::scope;
use crate::tunnel::{Tunnel, TunnelHandlers, TunnelMessage};
use crate::{display::Display, gl::Renderer};

type ResizeCallback = Closure<dyn FnMut(js_sys::Array)>;

pub type Shared = Rc<RefCell<Inner>>;
pub type WeakShared = Weak<RefCell<Inner>>;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// The lowest output stream index of an upload; libguac accepts up to 63.
const FIRST_UPLOAD_STREAM: i32 = 32;

/// Client connection state, as reported to JavaScript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Connecting,
    /// Tunnel open, waiting for the first frame.
    Waiting,
    Connected,
    Disconnected,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Connecting => "connecting",
            State::Waiting => "waiting",
            State::Connected => "connected",
            State::Disconnected => "disconnected",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaleMode {
    /// Shrink to fit the container, never enlarge.
    Fit,
    /// One remote pixel per device pixel.
    Native,
}

/// Data of a `blob`: base64 text of the instruction, or raw bytes of a
/// binary record, as a view of the received message.
enum BlobData<'a> {
    Base64(&'a str),
    Raw(js_sys::Uint8Array),
}

impl BlobData<'_> {
    fn to_vec(&self) -> Option<Vec<u8>> {
        match self {
            Self::Base64(text) => B64.decode(text.as_bytes()).ok(),
            Self::Raw(view) => Some(view.to_vec()),
        }
    }
}

/// The data of a stream that is used as a whole once it ends.
///
/// Raw blobs are kept as views of the messages they arrived in, so an
/// image that came in one blob reaches the decoder without a copy. Base64
/// blobs are decoded into wasm memory, as is everything in the unlikely
/// case that both kinds arrive on one stream.
#[derive(Default)]
struct StreamData {
    bytes: Vec<u8>,
    views: Vec<js_sys::Uint8Array>,
}

impl StreamData {
    fn push(&mut self, data: BlobData<'_>) {
        match data {
            BlobData::Raw(view) if self.bytes.is_empty() => self.views.push(view),
            BlobData::Raw(view) => self.bytes.extend_from_slice(&view.to_vec()),
            BlobData::Base64(text) => {
                for view in self.views.drain(..) {
                    self.bytes.extend_from_slice(&view.to_vec());
                }
                let _ = B64.decode_vec(text.as_bytes(), &mut self.bytes);
            }
        }
    }

    fn into_vec(mut self) -> Vec<u8> {
        for view in self.views.drain(..) {
            self.bytes.extend_from_slice(&view.to_vec());
        }
        self.bytes
    }

    /// The data as an encoded image for the decoder, and whether its header
    /// says it is opaque (`crate::image::is_opaque`). The array is the view
    /// itself when everything arrived in one raw blob, a new one otherwise.
    fn into_image(mut self, mimetype: &str) -> (js_sys::Uint8Array, bool) {
        let data = match self.views.len() {
            0 => {
                let opaque = crate::image::is_opaque(&self.bytes, mimetype);
                return (js_sys::Uint8Array::from(self.bytes.as_slice()), opaque);
            }
            1 => self.views.remove(0),
            _ => {
                let total = self.views.iter().map(js_sys::Uint8Array::length).sum();
                let joined = js_sys::Uint8Array::new_with_length(total);
                let mut offset = 0;
                for view in &self.views {
                    joined.set(view, offset);
                    offset += view.length();
                }
                joined
            }
        };
        // JPEG needs no look at the data; for the others the start of the
        // image is enough, and only that much is copied into wasm.
        let opaque = mimetype == "image/jpeg" || {
            let prefix = data.length().min(crate::image::OPACITY_PREFIX_BYTES);
            crate::image::is_opaque(&data.subarray(0, prefix).to_vec(), mimetype)
        };
        (data, opaque)
    }
}

enum InStream {
    /// Compressed video decoded with WebCodecs into a layer.
    Video(crate::video::VideoStream),
    /// PCM forwarded to the page (worker mode, where Web Audio is missing).
    AudioForward,
    Image {
        mimetype: String,
        data: StreamData,
        slot: ImageSlot,
    },
    Clipboard {
        data: StreamData,
    },
    Audio,
    /// A file or directory listing (`body`, `file`), handed to the page
    /// as it arrives; the page acknowledges each blob (`ack_stream`).
    File,
}

/// Keeps an event listener registered for as long as it is alive.
struct Listener {
    target: EventTarget,
    kind: &'static str,
    closure: Closure<dyn FnMut(Event)>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self
            .target
            .remove_event_listener_with_callback(self.kind, self.closure.as_ref().unchecked_ref());
    }
}

#[derive(Default)]
struct Counters {
    bytes: u64,
    instructions: u64,
    /// The formats of the images that arrived.
    images: crate::image::Formats,
}

/// DOM parts owned by a main-thread client.
struct Dom {
    container: HtmlElement,
    canvas: HtmlCanvasElement,
    sink: HtmlTextAreaElement,
    listeners: Vec<Listener>,
    resize_observer: Option<(ResizeObserver, ResizeCallback)>,
}

pub struct Inner {
    self_ref: WeakShared,
    /// Strong self-reference held while a connection is active, so that a
    /// session is not torn down just because JavaScript dropped its handle
    /// (wasm-bindgen frees objects from a FinalizationRegistry).
    keepalive: Option<Shared>,
    /// Present when running on the main thread with a DOM; `None` inside a
    /// worker, where input arrives through method calls instead.
    dom: Option<Dom>,
    /// Container size in CSS pixels and the device pixel ratio.
    viewport: (f64, f64),
    dpr: f64,
    display: Display,
    tunnel: Option<Tunnel>,
    parser: Option<Parser>,
    state: State,
    streams: HashMap<i32, InStream>,
    free_out_streams: Vec<i32>,
    next_out_stream: i32,
    /// Output streams of uploads (see [`Inner::open_upload`]).
    uploads: Vec<i32>,
    keyboard: Keyboard,
    /// Ctrl and Alt held from the page (see [`Inner::hold_modifier`]).
    held: HeldModifiers,
    wheel: WheelAccumulator,
    mouse: (i32, i32, MouseButtons),
    last_sync_ack: Option<i64>,
    handler: Option<js_sys::Function>,
    events: Vec<(&'static str, JsValue)>,
    audio_context: Option<AudioContext>,
    audio: HashMap<i32, RawAudioPlayer>,
    /// Forward audio streams to the page instead of playing them here.
    audio_forward: bool,
    /// Offer the binary tunnel subprotocol when connecting.
    binary_tunnel: bool,
    /// Whether the zstd image type is announced to the server.
    zstd_images: bool,
    scale_mode: ScaleMode,
    auto_resize: bool,
    pending_resize: Option<(u32, u32)>,
    /// Draw to the canvas as soon as something changed instead of in the
    /// next animation frame (see [`Inner::set_early_composite`]).
    early_composite: bool,
    /// An animation frame is requested: either to draw in, or, after an
    /// early composite, to mark the end of the refresh interval it used.
    raf_pending: bool,
    raf_id: Option<i32>,
    raf_closure: Option<Closure<dyn FnMut(f64)>>,
    timer: Option<(i32, Closure<dyn FnMut()>)>,
    last_received: f64,
    last_nop: f64,
    ping_sent_at: Option<f64>,
    rtt_ms: Option<f64>,
    unstable: bool,
    /// The display is cleared when the tunnel opened by `connect` answers.
    reset_display: bool,
    last_lag_ms: f64,
    counters: Counters,
    stats_since: f64,
    /// Whether changes are drawn to the canvas (see [`Inner::set_presenting`]).
    presenting: bool,
    /// A recording loaded for playback.
    playback: Option<Playback>,
    /// Instructions are being fed from a recording.
    replaying: bool,
}

/// A session recording being played (see [`Inner::load_recording`]).
struct Playback {
    data: Vec<u8>,
    index: crate::recording::Index,
    /// How many points of the index have been fed.
    fed: usize,
}

fn now() -> f64 {
    js_sys::Date::now()
}

/// Runs `f` with the shared state borrowed, then delivers queued events to
/// the JavaScript handler outside of the borrow.
pub fn with_inner(weak: &WeakShared, f: impl FnOnce(&mut Inner)) {
    let Some(rc) = weak.upgrade() else { return };
    let (handler, events) = {
        let Ok(mut inner) = rc.try_borrow_mut() else {
            return;
        };
        f(&mut inner);
        inner.after_update();
        (inner.handler.clone(), std::mem::take(&mut inner.events))
    };
    if let Some(handler) = handler {
        for (kind, detail) in events {
            if let Err(err) = handler.call2(&JsValue::NULL, &JsValue::from_str(kind), &detail) {
                web_sys::console::error_2(&"guac event handler failed:".into(), &err);
            }
        }
    }
}

fn object(entries: &[(&str, JsValue)]) -> JsValue {
    let obj = js_sys::Object::new();
    for (k, v) in entries {
        let _ = js_sys::Reflect::set(&obj, &JsValue::from_str(k), v);
    }
    obj.into()
}

impl Inner {
    /// Creates a main-thread client that owns a canvas inside `container`.
    pub fn create(container: HtmlElement) -> Result<Shared, JsValue> {
        let document = web_sys::window()
            .and_then(|w| w.document())
            .ok_or("no document")?;
        let canvas: HtmlCanvasElement = document.create_element("canvas")?.dyn_into()?;
        canvas.set_class_name("guac-display");
        let style = canvas.style();
        style.set_property("display", "block")?;
        style.set_property("touch-action", "none")?;
        style.set_property("outline", "none")?;

        // Off-screen text area that receives keyboard focus and IME input.
        let sink: HtmlTextAreaElement = document.create_element("textarea")?.dyn_into()?;
        sink.set_class_name("guac-input-sink");
        for attr in ["autocapitalize", "autocomplete", "autocorrect"] {
            sink.set_attribute(attr, "off")?;
        }
        sink.set_attribute("spellcheck", "false")?;
        sink.set_attribute("aria-label", "Remote keyboard input")?;
        let s = sink.style();
        for (k, v) in [
            ("position", "absolute"),
            ("left", "0"),
            ("top", "0"),
            ("width", "1px"),
            ("height", "1px"),
            ("opacity", "0"),
            ("border", "0"),
            ("padding", "0"),
            ("resize", "none"),
            ("overflow", "hidden"),
            ("pointer-events", "none"),
        ] {
            s.set_property(k, v)?;
        }

        container.append_child(&canvas)?;
        container.append_child(&sink)?;

        let renderer = Renderer::new(CanvasTarget::Element(canvas.clone()))
            .map_err(|e| JsValue::from_str(&e.0))?;
        let dom = Dom {
            container,
            canvas,
            sink,
            listeners: Vec::new(),
            resize_observer: None,
        };
        let shared = Self::new_shared(renderer, Some(dom));
        {
            let mut inner = shared.borrow_mut();
            inner.install_listeners()?;
            inner.install_timer()?;
        }
        Ok(shared)
    }

    /// Creates a client that renders into an `OffscreenCanvas`, typically
    /// inside a dedicated worker. The page forwards input and layout through
    /// [`Inner::pointer`], [`Inner::key_down`], [`Inner::set_viewport`] etc.
    pub fn create_offscreen(canvas: web_sys::OffscreenCanvas) -> Result<Shared, JsValue> {
        let renderer =
            Renderer::new(CanvasTarget::Offscreen(canvas)).map_err(|e| JsValue::from_str(&e.0))?;
        let shared = Self::new_shared(renderer, None);
        shared.borrow_mut().install_timer()?;
        Ok(shared)
    }

    fn new_shared(renderer: Renderer, dom: Option<Dom>) -> Shared {
        Rc::new_cyclic(|weak: &WeakShared| {
            RefCell::new(Inner {
                self_ref: weak.clone(),
                keepalive: None,
                dom,
                viewport: (0.0, 0.0),
                dpr: scope::device_pixel_ratio().unwrap_or(1.0),
                display: Display::new(renderer),
                tunnel: None,
                parser: Some(Parser::new()),
                state: State::Idle,
                streams: HashMap::new(),
                free_out_streams: Vec::new(),
                next_out_stream: 0,
                uploads: Vec::new(),
                keyboard: Keyboard::default(),
                held: HeldModifiers::default(),
                wheel: WheelAccumulator::default(),
                mouse: (0, 0, MouseButtons::default()),
                last_sync_ack: None,
                handler: None,
                events: Vec::new(),
                audio_context: None,
                audio: HashMap::new(),
                audio_forward: false,
                binary_tunnel: true,
                zstd_images: true,
                scale_mode: ScaleMode::Fit,
                auto_resize: true,
                pending_resize: None,
                early_composite: false,
                raf_pending: false,
                raf_id: None,
                raf_closure: None,
                timer: None,
                last_received: now(),
                last_nop: now(),
                ping_sent_at: None,
                rtt_ms: None,
                unstable: false,
                reset_display: false,
                last_lag_ms: 0.0,
                counters: Counters::default(),
                stats_since: now(),
                presenting: true,
                playback: None,
                replaying: false,
            })
        })
    }

    pub fn set_handler(&mut self, handler: Option<js_sys::Function>) {
        self.handler = handler;
    }

    fn emit(&mut self, kind: &'static str, detail: JsValue) {
        self.events.push((kind, detail));
    }

    fn set_state(&mut self, state: State) {
        if self.state != state {
            self.state = state;
            self.emit("state", JsValue::from_str(state.as_str()));
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    // ------------------------------------------------------------------
    // DOM wiring
    // ------------------------------------------------------------------

    fn listen<E: JsCast + 'static>(
        &mut self,
        target: &EventTarget,
        kind: &'static str,
        passive: bool,
        handler: fn(&mut Inner, E),
    ) -> Result<(), JsValue> {
        let weak = self.self_ref.clone();
        let closure = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
            let event: E = event.unchecked_into();
            with_inner(&weak, |inner| handler(inner, event));
        });
        let options = AddEventListenerOptions::new();
        options.set_passive(passive);
        target.add_event_listener_with_callback_and_add_event_listener_options(
            kind,
            closure.as_ref().unchecked_ref(),
            &options,
        )?;
        if let Some(dom) = self.dom.as_mut() {
            dom.listeners.push(Listener {
                target: target.clone(),
                kind,
                closure,
            });
        }
        Ok(())
    }

    fn install_listeners(&mut self) -> Result<(), JsValue> {
        let Some(dom) = self.dom.as_ref() else {
            return Ok(());
        };
        let canvas: EventTarget = dom.canvas.clone().into();
        let sink: EventTarget = dom.sink.clone().into();
        let container = dom.container.clone();
        self.listen::<PointerEvent>(&canvas, "pointermove", true, Inner::on_pointer)?;
        self.listen::<PointerEvent>(&canvas, "pointerdown", false, Inner::on_pointer_down)?;
        self.listen::<PointerEvent>(&canvas, "pointerup", true, Inner::on_pointer)?;
        self.listen::<PointerEvent>(&canvas, "pointercancel", true, Inner::on_pointer)?;
        self.listen::<WheelEvent>(&canvas, "wheel", false, Inner::on_wheel)?;
        self.listen::<Event>(&canvas, "contextmenu", false, |_, e| e.prevent_default())?;
        self.listen::<KeyboardEvent>(&sink, "keydown", false, Inner::on_key_down)?;
        self.listen::<KeyboardEvent>(&sink, "keyup", false, Inner::on_key_up)?;
        self.listen::<CompositionEvent>(&sink, "compositionend", true, Inner::on_composition_end)?;
        self.listen::<InputEvent>(&sink, "input", true, Inner::on_input)?;
        self.listen::<Event>(&sink, "blur", true, |inner, _| inner.release_keys())?;
        self.listen::<Event>(&sink, "focus", true, |inner, _| {
            inner.emit("focus", JsValue::TRUE)
        })?;

        // Track the container size for scaling and remote resizing.
        let weak = self.self_ref.clone();
        let closure = Closure::<dyn FnMut(js_sys::Array)>::new(move |_entries: js_sys::Array| {
            with_inner(&weak, |inner| inner.on_container_resize());
        });
        let observer = ResizeObserver::new(closure.as_ref().unchecked_ref())?;
        observer.observe(&container);
        if let Some(dom) = self.dom.as_mut() {
            dom.resize_observer = Some((observer, closure));
        }
        Ok(())
    }

    /// One-second housekeeping timer (keep-alive, ping, timeouts, stats).
    fn install_timer(&mut self) -> Result<(), JsValue> {
        let weak = self.self_ref.clone();
        let tick = Closure::<dyn FnMut()>::new(move || with_inner(&weak, Inner::on_timer));
        let id = scope::set_interval(tick.as_ref().unchecked_ref(), 1000)?;
        self.timer = Some((id, tick));
        Ok(())
    }

    pub fn focus(&self) {
        if let Some(dom) = &self.dom {
            let _ = dom.sink.focus();
        }
    }

    // ------------------------------------------------------------------
    // Connection lifecycle
    // ------------------------------------------------------------------

    /// Builds the tunnel URL with display and capability parameters.
    pub fn tunnel_url(&self, base: &str, connection: &str) -> String {
        let dpr = self.dpr;
        let (cw, ch) = self.container_size_now();
        let width = ((cw * dpr).round() as u32).max(64);
        let height = ((ch * dpr).round() as u32).max(64);
        let dpi = (96.0 * dpr).round() as u32;
        let timezone =
            js_sys::Intl::DateTimeFormat::new(&js_sys::Array::new(), &js_sys::Object::new())
                .resolved_options()
                .dyn_into::<js_sys::Object>()
                .ok()
                .and_then(|o| js_sys::Reflect::get(&o, &"timeZone".into()).ok())
                .and_then(|v| v.as_string());
        let mut url = format!(
            "{base}{}connection={}&width={width}&height={height}&dpi={dpi}",
            if base.contains('?') { '&' } else { '?' },
            String::from(js_sys::encode_uri_component(connection)),
        );
        // The zstd type is ours: molcajeted sends it to no one else, and a
        // stock guacd ignores what it does not know.
        let zstd = self
            .zstd_images
            .then_some(molcajete_protocol::zimage::MIMETYPE);
        for image in ["image/png", "image/jpeg", "image/webp"]
            .into_iter()
            .chain(zstd)
        {
            url += &format!(
                "&image={}",
                String::from(js_sys::encode_uri_component(image))
            );
        }
        for audio in crate::audio::SUPPORTED_MIMETYPES {
            url += &format!(
                "&audio={}",
                String::from(js_sys::encode_uri_component(audio))
            );
        }
        if let Some(tz) = timezone {
            url += &format!(
                "&timezone={}",
                String::from(js_sys::encode_uri_component(&tz))
            );
        }
        url
    }

    fn container_size_now(&self) -> (f64, f64) {
        match &self.dom {
            Some(dom) => {
                let rect = dom.container.get_bounding_client_rect();
                (rect.width(), rect.height())
            }
            None => self.viewport,
        }
    }

    pub fn connect(&mut self, url: &str) -> Result<(), JsValue> {
        self.disconnect_internal(None);
        // The last picture stays until the gateway answers, so that a
        // reconnect shows it meanwhile.
        self.reset_display = true;
        self.parser = Some(Parser::new());
        self.last_sync_ack = None;
        self.counters = Counters::default();
        self.set_state(State::Connecting);

        let weak_open = self.self_ref.clone();
        let weak_msg = self.self_ref.clone();
        let weak_close = self.self_ref.clone();
        let tunnel = Tunnel::connect(
            url,
            self.binary_tunnel,
            TunnelHandlers {
                on_open: Box::new(move || {
                    with_inner(&weak_open, |inner| {
                        inner.last_received = now();
                        inner.set_state(State::Waiting);
                    })
                }),
                on_message: Box::new(move |message| {
                    with_inner(&weak_msg, |inner| match message {
                        TunnelMessage::Text(text) => inner.receive(text),
                        TunnelMessage::Binary(buffer) => inner.receive_binary(buffer),
                    })
                }),
                on_close: Box::new(move |code, reason| {
                    with_inner(&weak_close, |inner| inner.on_tunnel_closed(code, &reason))
                }),
            },
        )?;
        self.tunnel = Some(tunnel);
        self.last_received = now();
        self.keepalive = self.self_ref.upgrade();
        Ok(())
    }

    fn on_tunnel_closed(&mut self, code: u16, reason: &str) {
        if self.state == State::Disconnected {
            return;
        }
        // The gateway puts the numeric Guacamole status in the close reason.
        let status = Status::parse(reason).unwrap_or_else(|| Status::from_websocket_code(code));
        if status.is_error() {
            self.emit_error(status, &format!("tunnel closed: {}", status.name()));
        }
        self.emit_interrupted(status);
        self.disconnect_internal(None);
    }

    /// The tunnel ended without the server ending the session (no
    /// `disconnect`, no `error`): the page may connect again and get the
    /// same session back, which molcajeted holds for a while.
    fn emit_interrupted(&mut self, status: Status) {
        self.emit(
            "interrupted",
            object(&[("code", JsValue::from(status.code()))]),
        );
    }

    fn emit_error(&mut self, status: Status, message: &str) {
        self.emit(
            "error",
            object(&[
                ("code", JsValue::from(status.code())),
                ("name", JsValue::from_str(status.name())),
                ("message", JsValue::from_str(message)),
            ]),
        );
    }

    fn fail(&mut self, status: Status, message: &str) {
        self.emit_error(status, message);
        self.disconnect_internal(None);
    }

    pub fn disconnect(&mut self) {
        self.disconnect_internal(Some("10.disconnect;"));
    }

    fn disconnect_internal(&mut self, farewell: Option<&str>) {
        let mut releases = self.keyboard.release_all();
        let held = self.held.release_all();
        if !held.is_empty() {
            self.emit_modifiers();
        }
        releases.extend(held);
        if let Some(tunnel) = self.tunnel.take() {
            for (keysym, pressed) in releases {
                tunnel.send(
                    &InstructionBuilder::new("key")
                        .arg(keysym)
                        .arg(pressed as u8)
                        .finish(),
                );
            }
            if let Some(msg) = farewell {
                tunnel.send(msg);
            }
            tunnel.close();
        }
        self.streams.clear();
        self.audio.clear();
        self.free_out_streams.clear();
        self.next_out_stream = 0;
        self.uploads.clear();
        if self.state != State::Idle {
            self.set_state(State::Disconnected);
        }
        // Callers always hold their own strong reference while borrowing, so
        // this never drops `self` in place.
        self.keepalive = None;
    }

    fn send(&self, data: &str) {
        if let Some(t) = &self.tunnel {
            t.send(data);
        }
    }

    fn is_connected(&self) -> bool {
        matches!(self.state, State::Waiting | State::Connected)
    }

    // ------------------------------------------------------------------
    // Instruction handling
    // ------------------------------------------------------------------

    /// Whether to offer the binary tunnel subprotocol (raw stream data) to
    /// the gateway. Applies to the next [`Inner::connect`].
    pub fn set_binary_tunnel(&mut self, enabled: bool) {
        self.binary_tunnel = enabled;
    }

    /// Whether to announce the zstd image type, which molcajeted then uses
    /// for lossless updates instead of PNG. Applies to the next
    /// [`Inner::tunnel_url`].
    pub fn set_zstd_images(&mut self, enabled: bool) {
        self.zstd_images = enabled;
    }

    /// The WebSocket subprotocol in use (empty when not connected).
    pub fn tunnel_protocol(&self) -> String {
        self.tunnel
            .as_ref()
            .map(Tunnel::protocol)
            .unwrap_or_default()
    }

    fn receive(&mut self, text: String) {
        self.last_received = now();
        self.counters.bytes += text.len() as u64;
        let Some(mut parser) = self.parser.take() else {
            return;
        };
        parser.feed(text.as_bytes());
        if self.drain(&mut parser) {
            self.parser = Some(parser);
            self.display.process();
        }
    }

    /// A binary message: whole records (`molcajete_protocol::binary`).
    /// Instruction text is copied into the parser; stream data stays where
    /// it is, as views of `message`.
    fn receive_binary(&mut self, message: js_sys::ArrayBuffer) {
        use molcajete_protocol::binary::{
            BLOB_HEADER_LEN, HEADER_LEN, TAG_BLOB, TAG_INSTRUCTIONS, parse_header,
        };

        self.last_received = now();
        let bytes = js_sys::Uint8Array::new(&message);
        let total = bytes.length() as usize;
        self.counters.bytes += total as u64;
        let Some(mut parser) = self.parser.take() else {
            return;
        };
        let mut offset = 0;
        while offset < total {
            // The header, and with it the stream index of a blob.
            let mut header = [0u8; BLOB_HEADER_LEN];
            let available = (total - offset).min(BLOB_HEADER_LEN);
            bytes
                .subarray(offset as u32, (offset + available) as u32)
                .copy_to(&mut header[..available]);
            // The length is checked against what is left before it is added
            // to anything: on wasm32 a length near 4 GiB would wrap around.
            let record = parse_header(&header[..available])
                .filter(|&(tag, length)| {
                    length <= total - offset - HEADER_LEN
                        && (tag != TAG_BLOB || length >= BLOB_HEADER_LEN - HEADER_LEN)
                })
                .map(|(tag, length)| (tag, offset + HEADER_LEN, offset + HEADER_LEN + length));
            let Some((tag, body, end)) = record else {
                self.fail(Status::SERVER_ERROR, "protocol error: malformed record");
                return;
            };
            match tag {
                TAG_INSTRUCTIONS => {
                    parser.feed_with(end - body, |out| {
                        bytes.subarray(body as u32, end as u32).copy_to(out)
                    });
                    if !self.drain(&mut parser) {
                        return;
                    }
                }
                TAG_BLOB => {
                    let stream =
                        u32::from_le_bytes([header[5], header[6], header[7], header[8]]) as i32;
                    let data = bytes.subarray((offset + BLOB_HEADER_LEN) as u32, end as u32);
                    self.on_blob(stream, BlobData::Raw(data));
                }
                // A kind of record this client does not know.
                _ => {}
            }
            offset = end;
        }
        self.parser = Some(parser);
        self.display.process();
    }

    /// Handles every complete instruction in the parser. Returns `false`
    /// when the connection ended meanwhile (protocol error, `disconnect`).
    fn drain(&mut self, parser: &mut Parser) -> bool {
        loop {
            let raw = match parser.next_raw() {
                Ok(Some(raw)) => raw,
                Ok(None) => break,
                Err(err) => {
                    self.fail(Status::SERVER_ERROR, &format!("protocol error: {err}"));
                    return false;
                }
            };
            self.counters.instructions += 1;
            match raw.opcode() {
                "" => {
                    // Tunnel-internal: UUID announcement or ping reply.
                    if raw.arg(0) == "ping" {
                        if let Some(sent) = self.ping_sent_at.take() {
                            self.rtt_ms = Some(now() - sent);
                        }
                    } else if std::mem::take(&mut self.reset_display) {
                        // The new connection's first message.
                        self.display.reset();
                    }
                    continue;
                }
                "required" => {
                    let names = js_sys::Array::new();
                    for name in raw.args() {
                        names.push(&JsValue::from_str(name));
                    }
                    self.emit("required", names.into());
                    continue;
                }
                _ => {}
            }
            match S::decode(&raw) {
                Ok(ins) => self.handle(ins),
                Err(err) => web_sys::console::warn_1(&err.to_string().into()),
            }
            if self.state == State::Disconnected {
                return false;
            }
        }
        true
    }

    fn enqueue(&mut self, op: DrawOp) {
        self.display.enqueue(Op::Draw(op));
    }

    fn handle(&mut self, ins: S<'_>) {
        match ins {
            S::Arc {
                layer,
                x,
                y,
                radius,
                start,
                end,
                negative,
            } => self.enqueue(DrawOp::Arc {
                layer,
                x,
                y,
                radius,
                start,
                end,
                negative,
            }),
            S::Cfill {
                mode,
                layer,
                r,
                g,
                b,
                a,
            } => self.enqueue(DrawOp::Cfill {
                mode,
                layer,
                color: [r, g, b, a],
            }),
            S::Clip { layer } => self.enqueue(DrawOp::Clip { layer }),
            S::Close { layer } => self.enqueue(DrawOp::Close { layer }),
            S::Copy {
                src,
                sx,
                sy,
                w,
                h,
                mode,
                dst,
                dx,
                dy,
            } => self.enqueue(DrawOp::Copy {
                src,
                rect: Rect::new(sx, sy, w, h),
                mode,
                dst,
                x: dx,
                y: dy,
            }),
            S::Cstroke {
                mode,
                layer,
                cap,
                join,
                thickness,
                r,
                g,
                b,
                a,
            } => self.enqueue(DrawOp::Cstroke {
                mode,
                layer,
                cap,
                join,
                thickness,
                color: [r, g, b, a],
            }),
            S::Cursor {
                hotspot_x,
                hotspot_y,
                src,
                sx,
                sy,
                w,
                h,
            } => self.enqueue(DrawOp::Cursor {
                hotspot_x,
                hotspot_y,
                src,
                rect: Rect::new(sx, sy, w, h),
            }),
            S::Curve {
                layer,
                cp1x,
                cp1y,
                cp2x,
                cp2y,
                x,
                y,
            } => self.enqueue(DrawOp::Curve {
                layer,
                cp1: (cp1x, cp1y),
                cp2: (cp2x, cp2y),
                end: (x, y),
            }),
            S::Dispose { layer } => self.enqueue(DrawOp::Dispose { layer }),
            S::Distort { layer, matrix } => self.enqueue(DrawOp::Distort { layer, matrix }),
            S::Identity { layer } => self.enqueue(DrawOp::Identity { layer }),
            S::Lfill { mode, layer, src } => self.enqueue(DrawOp::Lfill { mode, layer, src }),
            S::Line { layer, x, y } => self.enqueue(DrawOp::Line { layer, x, y }),
            S::Lstroke {
                mode,
                layer,
                cap,
                join,
                thickness,
                src,
            } => self.enqueue(DrawOp::Lstroke {
                mode,
                layer,
                cap,
                join,
                thickness,
                src,
            }),
            S::Move {
                layer,
                parent,
                x,
                y,
                z,
            } => self.enqueue(DrawOp::Move {
                layer,
                parent,
                x,
                y,
                z,
            }),
            S::Pop { layer } => self.enqueue(DrawOp::Pop { layer }),
            S::Push { layer } => self.enqueue(DrawOp::Push { layer }),
            S::Rect { layer, x, y, w, h } => self.enqueue(DrawOp::Rect {
                layer,
                rect: Rect::new(x, y, w, h),
            }),
            S::Reset { layer } => self.enqueue(DrawOp::Reset { layer }),
            S::Set { layer, name, value } => {
                if name == "miter-limit"
                    && let Ok(limit) = value.trim().parse::<f64>()
                {
                    self.enqueue(DrawOp::MiterLimit { layer, limit });
                }
            }
            S::Shade { layer, opacity } => self.enqueue(DrawOp::Shade { layer, opacity }),
            S::Size { layer, w, h } => self.enqueue(DrawOp::Size {
                layer,
                width: w,
                height: h,
            }),
            S::Start { layer, x, y } => self.enqueue(DrawOp::Start { layer, x, y }),
            S::Transfer {
                src,
                sx,
                sy,
                w,
                h,
                function,
                dst,
                dx,
                dy,
            } => self.enqueue(DrawOp::Transfer {
                src,
                rect: Rect::new(sx, sy, w, h),
                function,
                dst,
                x: dx,
                y: dy,
            }),
            S::Transform { layer, matrix } => self.enqueue(DrawOp::Transform { layer, matrix }),

            S::Img {
                stream,
                mode,
                layer,
                mimetype,
                x,
                y,
            } => {
                self.counters.images.add(mimetype);
                let slot = ImageSlot::pending();
                self.display.enqueue(Op::Image {
                    layer,
                    mode,
                    x,
                    y,
                    slot: slot.clone(),
                });
                self.streams.insert(
                    stream,
                    InStream::Image {
                        mimetype: mimetype.to_owned(),
                        data: StreamData::default(),
                        slot,
                    },
                );
            }
            S::Png {
                mode,
                layer,
                x,
                y,
                data,
            }
            | S::Jpeg {
                mode,
                layer,
                x,
                y,
                data,
            } => {
                let mimetype = if matches!(ins, S::Png { .. }) {
                    "image/png"
                } else {
                    "image/jpeg"
                };
                self.counters.images.add(mimetype);
                let slot = ImageSlot::pending();
                self.display.enqueue(Op::Image {
                    layer,
                    mode,
                    x,
                    y,
                    slot: slot.clone(),
                });
                let bytes = B64.decode(data.as_bytes()).unwrap_or_default();
                let opaque = crate::image::is_opaque(&bytes, mimetype);
                let data = js_sys::Uint8Array::from(bytes.as_slice());
                self.decode_image(data, opaque, mimetype, slot);
            }
            S::Blob { stream, data } => self.on_blob(stream, BlobData::Base64(data)),
            S::End { stream } => self.on_end(stream),
            S::Audio { stream, mimetype } => self.on_audio(stream, mimetype),
            S::Clipboard { stream, .. } => {
                self.streams.insert(
                    stream,
                    InStream::Clipboard {
                        data: StreamData::default(),
                    },
                );
            }
            S::File {
                stream,
                mimetype,
                filename,
            } => self.on_file(stream, None, mimetype, filename),
            S::Body {
                object,
                stream,
                mimetype,
                name,
            } => self.on_file(stream, Some(object), mimetype, name),
            S::Pipe { stream, .. } | S::Argv { stream, .. } => {
                self.send_ack(stream, "Unsupported", Status::UNSUPPORTED);
            }
            S::Video {
                stream,
                layer,
                mimetype,
            } => self.on_video(stream, layer, mimetype),
            S::Ack {
                stream,
                status,
                message,
            } if self.uploads.contains(&stream) => {
                if status.is_error() {
                    self.uploads.retain(|&s| s != stream);
                }
                self.emit(
                    "stream-ack",
                    object(&[
                        ("stream", JsValue::from(stream)),
                        ("code", JsValue::from(status.code())),
                        ("message", JsValue::from_str(message)),
                    ]),
                );
            }
            S::Ack { stream, status, .. } => {
                if status.is_error() {
                    self.free_out_stream(stream);
                }
            }
            S::Sync {
                timestamp,
                frames,
                apply_time,
            } => {
                self.display.enqueue(Op::Sync {
                    timestamp,
                    frames,
                    received_at: scope::performance_now(),
                    apply_time,
                });
            }
            S::Mouse { x, y } => self.display.show_software_cursor(x, y),
            S::Name { name } => self.emit("name", JsValue::from_str(name)),
            S::DisplayControl { available } => {
                self.emit("display-control", JsValue::from_bool(available))
            }
            S::VideoMode { mode } => {
                self.emit("video-mode", mode.map_or(JsValue::NULL, JsValue::from_str))
            }
            S::Msg { id } => self.emit("msg", JsValue::from(id)),
            S::Link { url } => self.emit("link", url.map_or(JsValue::NULL, JsValue::from_str)),
            S::Selection { active } => self.emit("selection", JsValue::from_bool(active)),
            // A recording ends where the session did; the player stays.
            S::Error { .. } | S::Disconnect if self.replaying => {}
            S::Error { message, status } => {
                let message = message.to_owned();
                self.fail(status, &message);
            }
            S::Disconnect => self.disconnect_internal(None),
            S::Log { message } => web_sys::console::log_1(&format!("guacd: {message}").into()),
            // Recordings hold no files, and a player offers none.
            S::Filesystem { .. } | S::Undefine { .. } | S::GetError { .. } if self.replaying => {}
            S::Filesystem {
                object: index,
                name,
                download,
                upload,
            } => {
                let flag = |v: Option<bool>| v.map_or(JsValue::NULL, JsValue::from_bool);
                self.emit(
                    "filesystem",
                    object(&[
                        ("object", JsValue::from(index)),
                        ("name", JsValue::from_str(name)),
                        ("download", flag(download)),
                        ("upload", flag(upload)),
                    ]),
                );
            }
            S::Undefine { object: index } => {
                self.emit("undefine", object(&[("object", JsValue::from(index))]));
            }
            S::GetError {
                object: index,
                name,
                status,
                message,
            } => self.emit(
                "get-error",
                object(&[
                    ("object", JsValue::from(index)),
                    ("name", JsValue::from_str(name)),
                    ("code", JsValue::from(status.code())),
                    ("message", JsValue::from_str(message)),
                ]),
            ),
            S::Nest { .. } | S::Nop | S::Ready { .. } | S::Required | S::Unknown { .. } => {}
        }
    }

    fn on_blob(&mut self, stream: i32, data: BlobData<'_>) {
        match self.streams.get_mut(&stream) {
            Some(InStream::Image { data: buf, .. }) | Some(InStream::Clipboard { data: buf }) => {
                buf.push(data);
            }
            Some(InStream::Audio) => {
                if let (Some(pcm), Some(player)) = (data.to_vec(), self.audio.get_mut(&stream)) {
                    player.play(&pcm);
                }
            }
            Some(InStream::AudioForward) => {
                // The page gets a buffer of its own: the worker transfers it,
                // which would detach every other view of a received message.
                let pcm = match data {
                    BlobData::Base64(text) => B64
                        .decode(text.as_bytes())
                        .ok()
                        .map(|pcm| js_sys::Uint8Array::from(pcm.as_slice())),
                    BlobData::Raw(view) => Some(view.slice(0, view.length())),
                };
                if let Some(pcm) = pcm {
                    self.emit(
                        "audio-data",
                        object(&[("stream", JsValue::from(stream)), ("data", pcm.into())]),
                    );
                }
            }
            Some(InStream::File) => {
                // A buffer of its own, which the worker can transfer.
                let data = match data {
                    BlobData::Base64(text) => B64
                        .decode(text.as_bytes())
                        .ok()
                        .map(|bytes| js_sys::Uint8Array::from(bytes.as_slice())),
                    BlobData::Raw(view) => Some(view.slice(0, view.length())),
                };
                if let Some(data) = data {
                    self.emit(
                        "stream-data",
                        object(&[("stream", JsValue::from(stream)), ("data", data.into())]),
                    );
                } else {
                    self.streams.remove(&stream);
                    self.send_ack(stream, "Invalid data", Status::CLIENT_BAD_REQUEST);
                    self.emit(
                        "stream-end",
                        object(&[("stream", JsValue::from(stream)), ("failed", JsValue::TRUE)]),
                    );
                }
            }
            Some(InStream::Video(video)) => {
                let layer = video.layer;
                let result = match data {
                    BlobData::Base64(text) => B64
                        .decode(text.as_bytes())
                        .map_err(|e| e.to_string())
                        .and_then(|bytes| video.push(&bytes)),
                    BlobData::Raw(view) => video.push_view(&view),
                };
                match result {
                    Ok(slots) => {
                        for slot in slots {
                            self.display.enqueue(Op::Image {
                                layer,
                                mode: molcajete_protocol::CompositeMode::OVER,
                                x: 0,
                                y: 0,
                                slot,
                            });
                        }
                    }
                    Err(err) => {
                        web_sys::console::warn_1(&format!("video stream {stream}: {err}").into());
                        self.streams.remove(&stream);
                        self.send_ack(stream, "Video decoding failed", Status::CLIENT_BAD_TYPE);
                    }
                }
            }
            None => {}
        }
    }

    fn on_end(&mut self, stream: i32) {
        match self.streams.remove(&stream) {
            Some(InStream::Image {
                mimetype,
                data,
                slot,
            }) => {
                if mimetype == molcajete_protocol::zimage::MIMETYPE {
                    // Nothing to wait for: the frame decodes it when it is
                    // drawn, in wasm.
                    slot.finish(ImageState::Ready(QueuedImage::Zstd(data.into_vec())));
                    return;
                }
                if mimetype == "image/png" && !self.presenting {
                    // Seeking: nothing is shown, and decoding here costs a
                    // fifth of the browser's decoder and staging canvas.
                    slot.finish(ImageState::Ready(QueuedImage::Png(data.into_vec())));
                    return;
                }
                let (data, opaque) = data.into_image(&mimetype);
                self.decode_image(data, opaque, &mimetype, slot);
            }
            Some(InStream::Clipboard { data }) => {
                let text = String::from_utf8_lossy(&data.into_vec()).into_owned();
                self.emit("clipboard", JsValue::from_str(&text));
            }
            Some(InStream::Audio) => {
                self.audio.remove(&stream);
            }
            Some(InStream::AudioForward) => {
                self.emit("audio-end", object(&[("stream", JsValue::from(stream))]));
            }
            Some(InStream::File) => {
                self.emit("stream-end", object(&[("stream", JsValue::from(stream))]));
            }
            Some(InStream::Video(video)) => video.finish(),
            None => {}
        }
    }

    // ------------------------------------------------------------------
    // Files
    // ------------------------------------------------------------------

    /// A file or listing the server sends: the page decides whether to
    /// take it, and acknowledges its data as it keeps up.
    fn on_file(&mut self, stream: i32, object_index: Option<i32>, mimetype: &str, name: &str) {
        if self.replaying {
            return;
        }
        self.streams.insert(stream, InStream::File);
        self.emit(
            "stream-start",
            object(&[
                ("stream", JsValue::from(stream)),
                ("object", object_index.map_or(JsValue::NULL, JsValue::from)),
                ("mimetype", JsValue::from_str(mimetype)),
                ("name", JsValue::from_str(name)),
            ]),
        );
    }

    /// Acknowledges an incoming file stream: its start, then each blob. An
    /// error code stops it.
    pub fn ack_stream(&mut self, stream: i32, message: &str, code: u16) {
        if !matches!(self.streams.get(&stream), Some(InStream::File)) {
            return;
        }
        let status = Status(code);
        if status.is_error() {
            self.streams.remove(&stream);
        }
        self.send_ack(stream, message, status);
    }

    /// Asks a filesystem object for a directory listing or a file.
    pub fn get(&mut self, object_index: i32, name: &str) {
        if self.is_connected() {
            self.send(
                &InstructionBuilder::new("get")
                    .arg(object_index)
                    .str_arg(name)
                    .finish(),
            );
        }
    }

    /// Opens an upload: `put` into a filesystem object at the path `name`,
    /// or with a negative `object_index`, `file` (the server's upload
    /// directory). Returns the stream, or -1 when no more may be open.
    pub fn open_upload(&mut self, object_index: i32, mimetype: &str, name: &str) -> i32 {
        if !self.is_connected() {
            return -1;
        }
        // libguac accepts stream indexes below 64; the clipboard's and
        // argv's are taken from the bottom, these from the top.
        let Some(stream) = (FIRST_UPLOAD_STREAM..64)
            .rev()
            .find(|s| !self.uploads.contains(s))
        else {
            return -1;
        };
        self.uploads.push(stream);
        let instruction = if object_index >= 0 {
            InstructionBuilder::new("put")
                .arg(object_index)
                .arg(stream)
                .str_arg(mimetype)
                .str_arg(name)
        } else {
            InstructionBuilder::new("file")
                .arg(stream)
                .str_arg(mimetype)
                .str_arg(name)
        };
        self.send(&instruction.finish());
        stream
    }

    /// Sends data of an upload; the server acknowledges it (`stream-ack`).
    pub fn upload_blob(&mut self, stream: i32, data: &[u8]) {
        if self.uploads.contains(&stream) {
            self.send(
                &InstructionBuilder::new("blob")
                    .arg(stream)
                    .str_arg(&B64.encode(data))
                    .finish(),
            );
        }
    }

    /// Ends an upload and frees its stream.
    pub fn end_upload(&mut self, stream: i32) {
        if self.uploads.contains(&stream) {
            self.uploads.retain(|&s| s != stream);
            self.send(&InstructionBuilder::new("end").arg(stream).finish());
        }
    }

    fn on_video(&mut self, stream: i32, layer: i32, mimetype: &str) {
        let codec = molcajete_protocol::video::VideoCodec::from_mimetype(mimetype)
            .filter(|_| crate::video::decoder_available());
        let Some(codec) = codec else {
            self.send_ack(stream, "BAD TYPE", Status::CLIENT_BAD_TYPE);
            return;
        };
        match crate::video::VideoStream::open(codec, stream, layer, self.self_ref.clone()) {
            Ok(video) => {
                self.streams.insert(stream, InStream::Video(video));
                self.send_ack(stream, "OK", Status::SUCCESS);
            }
            Err(err) => {
                web_sys::console::warn_2(&"cannot open video decoder:".into(), &err);
                self.send_ack(stream, "BAD TYPE", Status::CLIENT_BAD_TYPE);
            }
        }
    }

    /// Closes a video stream whose decoder failed and tells the server.
    pub(crate) fn video_failed(&mut self, stream: i32) {
        if matches!(self.streams.get(&stream), Some(InStream::Video(_))) {
            self.streams.remove(&stream);
            self.send_ack(stream, "Video decoding failed", Status::CLIENT_BAD_TYPE);
        }
    }

    pub fn set_page_visible(&mut self, visible: bool) {
        self.display.set_page_visible(visible);
    }

    /// Lets the page play audio for a client that cannot (worker mode).
    pub fn set_audio_forwarding(&mut self, enabled: bool) {
        self.audio_forward = enabled;
    }

    fn on_audio(&mut self, stream: i32, mimetype: &str) {
        let Some(format) = RawFormat::parse(mimetype) else {
            self.send_ack(stream, "BAD TYPE", Status::CLIENT_BAD_TYPE);
            return;
        };
        if self.audio_forward {
            self.streams.insert(stream, InStream::AudioForward);
            self.emit(
                "audio-start",
                object(&[
                    ("stream", JsValue::from(stream)),
                    ("rate", JsValue::from(format.rate)),
                    ("channels", JsValue::from(format.channels)),
                    ("bytesPerSample", JsValue::from(format.bytes_per_sample)),
                ]),
            );
            self.send_ack(stream, "OK", Status::SUCCESS);
            return;
        }
        if self.audio_context.is_none() {
            self.audio_context = AudioContext::new().ok();
        }
        let Some(ctx) = self.audio_context.clone() else {
            self.send_ack(stream, "Audio unavailable", Status::UNSUPPORTED);
            return;
        };
        self.audio.insert(stream, RawAudioPlayer::new(ctx, format));
        self.streams.insert(stream, InStream::Audio);
        self.send_ack(stream, "OK", Status::SUCCESS);
    }

    /// Decodes an image off the main thread. The queued draw stays blocked
    /// until the decoded image is ready.
    fn decode_image(
        &mut self,
        data: js_sys::Uint8Array,
        opaque: bool,
        mimetype: &str,
        slot: ImageSlot,
    ) {
        let weak = self.self_ref.clone();
        let mimetype = mimetype.to_owned();
        let started = scope::performance_now();
        wasm_bindgen_futures::spawn_local(async move {
            slot.finish(match crate::image::decode(data, mimetype, opaque).await {
                Ok(image) => ImageState::Ready(image.into()),
                Err(err) => {
                    web_sys::console::warn_2(&"image decode failed".into(), &err);
                    ImageState::Failed
                }
            });
            let elapsed = scope::performance_now() - started;
            with_inner(&weak, |inner| {
                inner.display.image_decoded(elapsed);
                inner.display.process();
            });
        });
    }

    fn send_ack(&self, stream: i32, message: &str, status: Status) {
        self.send(
            &InstructionBuilder::new("ack")
                .arg(stream)
                .str_arg(message)
                .arg(status.code())
                .finish(),
        );
    }

    fn alloc_out_stream(&mut self) -> i32 {
        self.free_out_streams.pop().unwrap_or_else(|| {
            let i = self.next_out_stream;
            self.next_out_stream += 1;
            i
        })
    }

    fn free_out_stream(&mut self, index: i32) {
        if index < self.next_out_stream && !self.free_out_streams.contains(&index) {
            self.free_out_streams.push(index);
        }
    }

    /// Sends `text` on a new output stream opened by `header`.
    fn send_text_stream(&mut self, header: impl FnOnce(i32) -> String, text: &str) {
        if !self.is_connected() {
            return;
        }
        let index = self.alloc_out_stream();
        self.send(&header(index));
        // 3 KiB of payload → 4 KiB of base64 per blob, well under guacd limits.
        for chunk in text.as_bytes().chunks(3072) {
            self.send(
                &InstructionBuilder::new("blob")
                    .arg(index)
                    .str_arg(&B64.encode(chunk))
                    .finish(),
            );
        }
        self.send(&InstructionBuilder::new("end").arg(index).finish());
        self.free_out_stream(index);
    }

    pub fn send_clipboard(&mut self, text: &str) {
        self.send_text_stream(
            |i| {
                InstructionBuilder::new("clipboard")
                    .arg(i)
                    .str_arg("text/plain")
                    .finish()
            },
            text,
        );
    }

    /// Pastes text into a terminal: sends it as the clipboard, then the
    /// terminal's paste shortcut, Ctrl+Shift+V (molcajeted and guacd paste
    /// on it). Ctrl and Shift are pressed only when not already held, and
    /// only those are released, so keys the user holds stay held.
    pub fn paste_text(&mut self, text: &str) {
        use input::keysym::{CONTROL_L, CONTROL_R, SHIFT_L, SHIFT_R};
        const KEY_V: u32 = 'V' as u32;
        self.send_clipboard(text);
        let mut keys = Vec::new();
        for (left, right) in [(CONTROL_L, CONTROL_R), (SHIFT_L, SHIFT_R)] {
            if !self.keyboard.is_down(&[left, right]) && !self.held.is_held(left) {
                keys.push(left);
            }
        }
        for &keysym in &keys {
            self.send_key(keysym, true);
        }
        self.send_key(KEY_V, true);
        self.send_key(KEY_V, false);
        for &keysym in keys.iter().rev() {
            self.send_key(keysym, false);
        }
    }

    /// Answers a `required` request (e.g. credentials).
    pub fn send_argv(&mut self, name: &str, value: &str) {
        self.send_text_stream(
            |i| {
                InstructionBuilder::new("argv")
                    .arg(i)
                    .str_arg("text/plain")
                    .str_arg(name)
                    .finish()
            },
            value,
        );
    }

    pub fn send_key(&self, keysym: u32, pressed: bool) {
        if self.is_connected() {
            self.send(
                &InstructionBuilder::new("key")
                    .arg(keysym)
                    .arg(pressed as u8)
                    .finish(),
            );
        }
    }

    fn send_mouse(&mut self, x: i32, y: i32, buttons: MouseButtons) {
        self.mouse = (x, y, buttons);
        if self.is_connected() {
            self.send(
                &InstructionBuilder::new("mouse")
                    .arg(x)
                    .arg(y)
                    .arg(buttons.0)
                    .finish(),
            );
        }
    }

    pub fn send_size(&self, width: u32, height: u32) {
        if self.is_connected() {
            self.send(
                &InstructionBuilder::new("size")
                    .arg(width)
                    .arg(height)
                    .finish(),
            );
        }
    }

    // ------------------------------------------------------------------
    // Input
    // ------------------------------------------------------------------

    fn resume_audio(&self) {
        if let Some(ctx) = &self.audio_context
            && ctx.state() == AudioContextState::Suspended
        {
            let _ = ctx.resume();
        }
    }

    /// Pointer position in CSS pixels relative to the canvas.
    fn css_position(&self, event: &web_sys::MouseEvent) -> (f64, f64) {
        match &self.dom {
            Some(dom) => {
                let rect = dom.canvas.get_bounding_client_rect();
                (
                    event.client_x() as f64 - rect.left(),
                    event.client_y() as f64 - rect.top(),
                )
            }
            None => (event.offset_x() as f64, event.offset_y() as f64),
        }
    }

    /// Pointer update with coordinates in CSS pixels relative to the canvas
    /// and DOM `buttons`.
    pub fn pointer(&mut self, css_x: f64, css_y: f64, dom_buttons: u16) {
        let scale = self.display.scale().max(1e-6);
        let max_x = self.display.width().saturating_sub(1) as i32;
        let max_y = self.display.height().saturating_sub(1) as i32;
        let x = ((css_x / scale).floor() as i32).clamp(0, max_x.max(0));
        let y = ((css_y / scale).floor() as i32).clamp(0, max_y.max(0));
        let buttons = input::buttons_from_dom(dom_buttons);
        self.display.hide_software_cursor();
        if (x, y, buttons) != self.mouse {
            self.send_mouse(x, y, buttons);
        }
    }

    fn on_pointer(&mut self, event: PointerEvent) {
        let (x, y) = self.css_position(&event);
        self.pointer(x, y, event.buttons());
    }

    fn on_pointer_down(&mut self, event: PointerEvent) {
        event.prevent_default();
        if let Some(dom) = &self.dom {
            let _ = dom.canvas.set_pointer_capture(event.pointer_id());
        }
        self.focus();
        self.resume_audio();
        self.on_pointer(event);
    }

    /// Mouse wheel with DOM `deltaY` / `deltaMode`.
    pub fn wheel(&mut self, delta_y: f64, delta_mode: u32) {
        let clicks = self.wheel.push(delta_y, delta_mode);
        let (x, y, buttons) = self.mouse;
        let bit = if clicks < 0 {
            MouseButtons::SCROLL_UP
        } else {
            MouseButtons::SCROLL_DOWN
        };
        for _ in 0..clicks.unsigned_abs().min(10) {
            let mut pressed = buttons;
            pressed.set(bit, true);
            self.send_mouse(x, y, pressed);
            self.send_mouse(x, y, buttons);
        }
    }

    fn on_wheel(&mut self, event: WheelEvent) {
        event.prevent_default();
        self.wheel(event.delta_y(), event.delta_mode());
    }

    /// `keydown`; returns whether the event was consumed (and should have
    /// its default action prevented).
    pub fn key_down(
        &mut self,
        key: &str,
        code: &str,
        location: u32,
        composing: bool,
        caps_lock: bool,
    ) -> bool {
        self.resume_audio();
        match self
            .keyboard
            .key_down(key, code, location, composing, caps_lock)
        {
            Some(events) => {
                let mut typed = false;
                for (keysym, pressed) in events {
                    self.send_key(keysym, pressed);
                    typed |= pressed && !input::keysym::is_modifier(keysym);
                }
                if typed {
                    self.release_held_once();
                }
                true
            }
            None => false,
        }
    }

    /// Holds Ctrl or Alt (`name`: `ctrl`, `alt`) as if its key were down,
    /// for devices whose keyboard has neither: `once` until the next key
    /// has gone out, `locked` until it is turned `off`. The page hears of
    /// every change, the release after a key included, as a `modifiers`
    /// event (`{ctrl, alt}`).
    pub fn hold_modifier(&mut self, name: &str, hold: &str) {
        let Some(hold) = Hold::parse(hold) else {
            return;
        };
        // Nothing can be held without a connection to hold it on.
        if hold != Hold::Off && !self.is_connected() {
            return;
        }
        let before = (self.held.ctrl(), self.held.alt());
        let Some(events) = self.held.set(name, hold) else {
            return;
        };
        for (keysym, pressed) in events {
            self.send_key(keysym, pressed);
        }
        if before != (self.held.ctrl(), self.held.alt()) {
            self.emit_modifiers();
        }
    }

    /// Presses and releases a key that the page offers as a button, with
    /// the modifiers held by [`Inner::hold_modifier`].
    pub fn press_key(&mut self, keysym: u32) {
        self.send_key(keysym, true);
        self.send_key(keysym, false);
        self.release_held_once();
    }

    /// Releases the modifiers that were held for the key that just went out.
    fn release_held_once(&mut self) {
        let releases = self.held.after_key();
        if releases.is_empty() {
            return;
        }
        for (keysym, pressed) in releases {
            self.send_key(keysym, pressed);
        }
        self.emit_modifiers();
    }

    fn emit_modifiers(&mut self) {
        let detail = object(&[
            ("ctrl", JsValue::from_str(self.held.ctrl().as_str())),
            ("alt", JsValue::from_str(self.held.alt().as_str())),
        ]);
        self.emit("modifiers", detail);
    }

    pub fn key_up(&mut self, key: &str, code: &str, location: u32, caps_lock: bool) -> bool {
        let events = self.keyboard.key_up(key, code, location, caps_lock);
        // In remote composition every key belongs to the remote host, even
        // one that sends nothing (the release of a Caps Lock toggle).
        let consumed = !events.is_empty() || self.composes_remotely();
        for (keysym, pressed) in events {
            self.send_key(keysym, pressed);
        }
        consumed
    }

    fn on_key_down(&mut self, event: KeyboardEvent) {
        if self.key_down(
            &event.key(),
            &event.code(),
            event.location(),
            event.is_composing(),
            event.get_modifier_state("CapsLock"),
        ) {
            event.prevent_default();
        }
    }

    fn on_key_up(&mut self, event: KeyboardEvent) {
        if self.key_up(
            &event.key(),
            &event.code(),
            event.location(),
            event.get_modifier_state("CapsLock"),
        ) {
            event.prevent_default();
        }
    }

    fn composes_remotely(&self) -> bool {
        self.keyboard.composition() != Composition::Local
    }

    /// Chooses who composes text typed with an input method. With `remote`,
    /// keys are sent by physical position and the remote host's IME composes;
    /// `caps_lock_toggles` makes Caps Lock the Hangul/English key.
    pub fn set_remote_composition(&mut self, remote: bool, caps_lock_toggles: bool) {
        self.release_keys();
        self.keyboard.set_composition(if remote {
            Composition::Remote { caps_lock_toggles }
        } else {
            Composition::Local
        });
        if let Some(dom) = &self.dom {
            // Browsers do not start an input method on a read-only field,
            // but it still takes focus and key events.
            dom.sink.set_read_only(remote);
            dom.sink.set_value("");
        }
    }

    /// Types text (e.g. a finished IME composition) on the remote side.
    pub fn type_text(&mut self, text: &str) {
        // The keys that made up this text were already sent one by one.
        if !self.composes_remotely() {
            for (keysym, pressed) in input::type_text(text) {
                self.send_key(keysym, pressed);
                // A modifier held for one key applies to the first character.
                if !pressed {
                    self.release_held_once();
                }
            }
        }
        if let Some(dom) = &self.dom {
            dom.sink.set_value("");
        }
    }

    fn on_composition_end(&mut self, event: CompositionEvent) {
        if let Some(text) = event.data().filter(|t| !t.is_empty()) {
            self.type_text(&text);
        }
    }

    fn on_input(&mut self, event: InputEvent) {
        if event.is_composing() {
            return;
        }
        if event.input_type() == "insertText"
            && let Some(text) = event.data().filter(|t| !t.is_empty())
        {
            self.type_text(&text);
            return;
        }
        if let Some(dom) = &self.dom
            && !dom.sink.value().is_empty()
            && event.input_type() != "insertCompositionText"
        {
            dom.sink.set_value("");
        }
    }

    pub fn release_keys(&mut self) {
        for (keysym, pressed) in self.keyboard.release_all() {
            self.send_key(keysym, pressed);
        }
    }

    // ------------------------------------------------------------------
    // Layout and rendering
    // ------------------------------------------------------------------

    pub fn set_scale_mode(&mut self, mode: ScaleMode) {
        self.scale_mode = mode;
        self.update_scale();
    }

    /// Whether container size changes are sent to the server. Turning it
    /// on also sends the current size, so that the remote display catches up
    /// with the window.
    pub fn set_auto_resize(&mut self, enabled: bool) {
        let was_enabled = std::mem::replace(&mut self.auto_resize, enabled);
        if !enabled {
            self.pending_resize = None;
        } else if !was_enabled && self.is_connected() {
            self.queue_resize();
        }
    }

    /// Container size (CSS pixels) and device pixel ratio, for clients
    /// without a DOM.
    pub fn set_viewport(&mut self, width: f64, height: f64, dpr: f64) {
        self.viewport = (width, height);
        if dpr > 0.0 {
            self.dpr = dpr;
        }
        self.on_container_resize();
    }

    fn on_container_resize(&mut self) {
        if self.dom.is_some() {
            self.dpr = scope::device_pixel_ratio().unwrap_or(self.dpr);
        }
        let size = self.container_size_now();
        if size == self.viewport && self.dom.is_some() {
            return;
        }
        self.viewport = size;
        self.update_scale();
        if self.auto_resize && self.is_connected() {
            self.queue_resize();
        }
    }

    /// Queues a `size` for the container in device pixels. It is sent from
    /// the timer, so a window drag produces one request.
    fn queue_resize(&mut self) {
        let w = (self.viewport.0 * self.dpr).round() as u32;
        let h = (self.viewport.1 * self.dpr).round() as u32;
        if w >= 64 && h >= 64 {
            self.pending_resize = Some((w, h));
        }
    }

    fn update_scale(&mut self) {
        let dpr = self.dpr;
        let (dw, dh) = (self.display.width() as f64, self.display.height() as f64);
        let (cw, ch) = self.viewport;
        let scale = match self.scale_mode {
            ScaleMode::Native => 1.0 / dpr,
            ScaleMode::Fit if dw > 0.0 && dh > 0.0 && cw > 0.0 && ch > 0.0 => {
                (cw / dw).min(ch / dh).min(1.0)
            }
            ScaleMode::Fit => 1.0,
        };
        self.display.set_scale(scale, dpr);
        let (css_w, css_h) = (dw * scale, dh * scale);
        match &self.dom {
            Some(dom) => {
                let style = dom.canvas.style();
                let _ = style.set_property("width", &format!("{css_w}px"));
                let _ = style.set_property("height", &format!("{css_h}px"));
            }
            None => self.emit(
                "layout",
                object(&[
                    ("width", JsValue::from_f64(css_w)),
                    ("height", JsValue::from_f64(css_h)),
                    ("scale", JsValue::from_f64(scale)),
                ]),
            ),
        }
    }

    fn set_cursor_image(
        &mut self,
        hotspot_x: i32,
        hotspot_y: i32,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) {
        let Some(dom) = &self.dom else {
            // Without a DOM the page applies the cursor itself.
            let pixels = js_sys::Uint8ClampedArray::from(rgba);
            self.emit(
                "cursor",
                object(&[
                    ("hotspotX", JsValue::from(hotspot_x)),
                    ("hotspotY", JsValue::from(hotspot_y)),
                    ("width", JsValue::from(width)),
                    ("height", JsValue::from(height)),
                    ("rgba", pixels.into()),
                ]),
            );
            return;
        };
        let style = dom.canvas.style();
        if width == 0 || height == 0 || rgba.iter().skip(3).step_by(4).all(|&a| a == 0) {
            let _ = style.set_property("cursor", "none");
            return;
        }
        let url = (|| -> Result<String, JsValue> {
            let document = web_sys::window()
                .and_then(|w| w.document())
                .ok_or("no document")?;
            let canvas: HtmlCanvasElement = document.create_element("canvas")?.dyn_into()?;
            canvas.set_width(width);
            canvas.set_height(height);
            let ctx: CanvasRenderingContext2d = canvas
                .get_context("2d")?
                .ok_or("no 2d context")?
                .dyn_into()?;
            let data = ImageData::new_with_u8_clamped_array_and_sh(Clamped(rgba), width, height)?;
            ctx.put_image_data(&data, 0.0, 0.0)?;
            canvas.to_data_url()
        })();
        match url {
            Ok(url) => {
                let _ = style.set_property(
                    "cursor",
                    &format!("url({url}) {hotspot_x} {hotspot_y}, auto"),
                );
            }
            Err(_) => {
                let _ = style.set_property("cursor", "default");
            }
        }
    }

    /// Handles display events and schedules a composite if needed.
    fn after_update(&mut self) {
        for event in self.display.take_events() {
            match event {
                DisplayEvent::Resized { width, height } => {
                    self.update_scale();
                    self.emit(
                        "resize",
                        object(&[
                            ("width", JsValue::from(width)),
                            ("height", JsValue::from(height)),
                        ]),
                    );
                }
                DisplayEvent::FrameComplete {
                    timestamp,
                    frames,
                    lag_ms,
                    apply_ms,
                } => {
                    self.last_lag_ms = lag_ms;
                    if self.last_sync_ack != Some(timestamp) {
                        self.last_sync_ack = Some(timestamp);
                        // A server that asked how long the frame took to
                        // apply (molcajeted) gets that; any other the stock
                        // reply.
                        let reply = match apply_ms {
                            Some(ms) => {
                                let apply = std::time::Duration::try_from_secs_f64(ms / 1000.0)
                                    .unwrap_or_default();
                                apply_time::sync_reply(timestamp, frames, apply)
                            }
                            None => InstructionBuilder::new("sync").arg(timestamp).finish(),
                        };
                        self.send(&reply);
                    }
                    if self.state == State::Waiting {
                        self.set_state(State::Connected);
                        self.focus();
                    }
                }
                DisplayEvent::Cursor {
                    hotspot_x,
                    hotspot_y,
                    width,
                    height,
                    rgba,
                } => {
                    self.set_cursor_image(hotspot_x, hotspot_y, width, height, &rgba);
                }
            }
        }
        if self.presenting && self.display.needs_composite() && !self.raf_pending {
            // With early compositing, nothing has been drawn in this refresh
            // interval yet, so the change need not wait for the animation
            // frame. The frame that is requested then ends the interval:
            // until it runs, further changes wait for it, which keeps the
            // canvas to one composite per display refresh.
            if self.early_composite {
                self.composite();
            }
            self.request_frame();
        }
    }

    fn composite(&mut self) {
        self.display.poll_cursor();
        self.display.composite();
    }

    /// Draws to the canvas as soon as a frame is applied, when the refresh
    /// interval is still unused, instead of in the next animation frame.
    ///
    /// Off by default, and only here to compare the two: Chrome hands a
    /// canvas to its compositor at the next begin-frame however early it was
    /// drawn, in a worker as on the main thread, so drawing early shortens
    /// the time measured here but not the time until the frame is on screen
    /// (`tests/browser/present-bench.mjs`).
    pub fn set_early_composite(&mut self, enabled: bool) {
        self.early_composite = enabled;
    }

    fn request_frame(&mut self) {
        if self.raf_closure.is_none() {
            let weak = self.self_ref.clone();
            self.raf_closure = Some(Closure::<dyn FnMut(f64)>::new(move |_t: f64| {
                with_inner(&weak, |inner| {
                    inner.raf_pending = false;
                    inner.raf_id = None;
                    if !inner.early_composite {
                        inner.composite();
                    } else if inner.display.needs_composite() {
                        // Changes that arrived after this interval's early
                        // composite. Drawing them uses up the interval that
                        // starts now.
                        inner.composite();
                        inner.request_frame();
                    }
                });
            }));
        }
        let closure = self.raf_closure.as_ref().expect("raf closure");
        if let Ok(id) = scope::request_animation_frame(closure.as_ref().unchecked_ref()) {
            self.raf_pending = true;
            self.raf_id = Some(id);
        }
    }

    fn on_timer(&mut self) {
        let t = now();
        if self.is_connected() {
            if let Some((w, h)) = self.pending_resize.take() {
                self.send_size(w, h);
            }
            if t - self.last_nop >= 5000.0 {
                self.last_nop = t;
                self.send("3.nop;");
            }
            if self.ping_sent_at.is_none() {
                self.ping_sent_at = Some(t);
                self.send(
                    &InstructionBuilder::new("")
                        .str_arg("ping")
                        .arg(t as u64)
                        .finish(),
                );
            }
            let silence = t - self.last_received;
            if silence > 15_000.0 {
                self.emit_error(Status::UPSTREAM_TIMEOUT, "connection timed out");
                self.emit_interrupted(Status::UPSTREAM_TIMEOUT);
                self.disconnect_internal(None);
                return;
            }
            let unstable = silence > 1_500.0;
            if unstable != self.unstable {
                self.unstable = unstable;
                self.emit("unstable", JsValue::from_bool(unstable));
            }
        }
        self.emit_stats(t);
    }

    fn emit_stats(&mut self, t: f64) {
        let elapsed = ((t - self.stats_since) / 1000.0).max(1e-3);
        self.stats_since = t;
        let stats = self.display.take_stats();
        let counters = std::mem::take(&mut self.counters);
        let buffered = self.tunnel.as_ref().map_or(0, Tunnel::buffered_amount);
        // Every stream of a session has the codec of its handshake; the
        // order only settles what is shown should that ever change.
        let video_codec = self
            .streams
            .values()
            .filter_map(|stream| match stream {
                InStream::Video(video) => Some(video.codec),
                _ => None,
            })
            .min();
        let image_formats: js_sys::Array = counters.images.names().map(JsValue::from_str).collect();
        self.emit(
            "stats",
            object(&[
                (
                    "framesPerSecond",
                    JsValue::from_f64(stats.frames_applied as f64 / elapsed),
                ),
                (
                    "compositesPerSecond",
                    JsValue::from_f64(stats.composites as f64 / elapsed),
                ),
                (
                    "drawCallsPerSecond",
                    JsValue::from_f64(stats.draw_calls as f64 / elapsed),
                ),
                (
                    "instructionsPerSecond",
                    JsValue::from_f64(counters.instructions as f64 / elapsed),
                ),
                (
                    "bytesPerSecond",
                    JsValue::from_f64(counters.bytes as f64 / elapsed),
                ),
                ("queuedOps", JsValue::from_f64(stats.queued_ops as f64)),
                ("frameLagMs", JsValue::from_f64(self.last_lag_ms)),
                ("frameLagMsAvg", JsValue::from_f64(stats.lag.average_ms())),
                ("frameLagMsMax", JsValue::from_f64(stats.lag.max_ms)),
                (
                    "frameReceiveMsAvg",
                    JsValue::from_f64(stats.receive.average_ms()),
                ),
                ("frameReceiveMsMax", JsValue::from_f64(stats.receive.max_ms)),
                (
                    "imagesPerSecond",
                    JsValue::from_f64(stats.decode.count as f64 / elapsed),
                ),
                (
                    "imageDecodeMsAvg",
                    JsValue::from_f64(stats.decode.average_ms()),
                ),
                ("imageDecodeMsMax", JsValue::from_f64(stats.decode.max_ms)),
                // No frame was drawn to the canvas in this second: no value.
                (
                    "framePresentMsAvg",
                    if stats.present.count > 0 {
                        JsValue::from_f64(stats.present.average_ms())
                    } else {
                        JsValue::NULL
                    },
                ),
                ("framePresentMsMax", JsValue::from_f64(stats.present.max_ms)),
                (
                    "rttMs",
                    self.rtt_ms.map_or(JsValue::NULL, JsValue::from_f64),
                ),
                ("sendBuffered", JsValue::from_f64(buffered as f64)),
                // Null while no video stream is open.
                (
                    "videoCodec",
                    video_codec.map_or(JsValue::NULL, |codec| JsValue::from_str(codec.name())),
                ),
                // Of the images that arrived in this second.
                ("imageFormats", image_formats.into()),
                ("displayWidth", JsValue::from(self.display.width())),
                ("displayHeight", JsValue::from(self.display.height())),
            ]),
        );
    }

    /// Processes instructions as if they had arrived from the server. Used
    /// by tests and benchmarks.
    pub fn feed(&mut self, text: &str) {
        if self.parser.is_none() {
            self.parser = Some(Parser::new());
        }
        let last = self.last_received;
        self.receive(text.to_owned());
        self.last_received = last;
    }

    // ------------------------------------------------------------------
    // Playback of session recordings
    // ------------------------------------------------------------------

    /// Whether changes reach the canvas. Off while seeking, so that the
    /// frames passed on the way are applied to the layers but never drawn;
    /// turning it back on draws the result once.
    pub fn set_presenting(&mut self, presenting: bool) {
        self.presenting = presenting;
        if presenting {
            self.display.invalidate();
        }
    }

    /// Loads a session recording (Guacamole's format) for playback, in
    /// place of any session, and returns its duration in milliseconds.
    /// Nothing is drawn until [`Inner::playback_advance`].
    pub fn load_recording(&mut self, data: Vec<u8>) -> f64 {
        self.disconnect_internal(None);
        self.set_state(State::Idle);
        let index = crate::recording::Index::build(&data);
        let duration = index.duration() as f64;
        self.playback = Some(Playback {
            data,
            index,
            fed: 0,
        });
        self.rewind();
        duration
    }

    /// Back to an empty display before the first instruction.
    fn rewind(&mut self) {
        self.display.reset();
        self.streams.clear();
        self.audio.clear();
        self.parser = Some(Parser::new());
        self.last_sync_ack = None;
        if let Some(playback) = &mut self.playback {
            playback.fed = 0;
        }
    }

    /// Plays the recording up to `time` (milliseconds from its start),
    /// starting over when `time` lies before what was played. At most
    /// about `max_bytes` are fed per call, so that the caller can let the
    /// images decode in between; returns whether `time` was reached.
    pub fn playback_advance(&mut self, time: f64, max_bytes: usize) -> bool {
        let Some(playback) = &self.playback else {
            return true;
        };
        let target = playback.index.points_until(time as i64);
        if target < playback.fed {
            self.rewind();
        }
        let Some(mut playback) = self.playback.take() else {
            return true;
        };
        let points = &playback.index.points;
        let fed = playback.fed;
        if target > fed {
            let from = fed.checked_sub(1).map_or(0, |i| points[i].end);
            // The furthest point within the budget, and at least the next.
            let limit = from.saturating_add(max_bytes);
            let within = points[fed..target].partition_point(|p| p.end <= limit);
            let to = fed + within.max(1);
            let end = points[to - 1].end;
            self.replaying = true;
            self.feed_bytes(&playback.data[from..end]);
            self.replaying = false;
            playback.fed = to;
        }
        let done = playback.fed >= target;
        self.playback = Some(playback);
        done
    }

    /// Time of the last instruction played (milliseconds from the start).
    pub fn playback_time(&self) -> f64 {
        self.playback.as_ref().map_or(0.0, |p| {
            p.fed
                .checked_sub(1)
                .map_or(0.0, |i| p.index.points[i].time as f64)
        })
    }

    /// Unix milliseconds of the start of the recording.
    pub fn playback_start(&self) -> f64 {
        self.playback.as_ref().map_or(0.0, |p| p.index.start as f64)
    }

    /// Drops the loaded recording.
    pub fn unload_recording(&mut self) {
        self.playback = None;
        self.rewind();
    }

    fn feed_bytes(&mut self, bytes: &[u8]) {
        let mut parser = self.parser.take().unwrap_or_default();
        parser.feed(bytes);
        self.drain(&mut parser);
        self.parser = Some(parser);
        self.display.process();
    }

    pub fn display(&mut self) -> &mut Display {
        &mut self.display
    }

    pub fn display_size(&self) -> (u32, u32) {
        (self.display.width(), self.display.height())
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(id) = self.raf_id.take() {
            scope::cancel_animation_frame(id);
        }
        if let Some((id, _)) = self.timer.take() {
            scope::clear_interval(id);
        }
        self.tunnel = None;
        if let Some(mut dom) = self.dom.take() {
            if let Some((observer, _)) = dom.resize_observer.take() {
                observer.disconnect();
            }
            dom.listeners.clear();
            dom.canvas.remove();
            dom.sink.remove();
        }
    }
}

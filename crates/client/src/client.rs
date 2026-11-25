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
use molcajete_protocol::{InstructionBuilder, MouseButtons, Parser, Status};
use wasm_bindgen::prelude::*;
use wasm_bindgen::{Clamped, JsCast};
use web_sys::{
    AddEventListenerOptions, AudioContext, AudioContextState, CanvasRenderingContext2d,
    CompositionEvent, Event, EventTarget, HtmlCanvasElement, HtmlElement, HtmlTextAreaElement,
    ImageData, InputEvent, KeyboardEvent, PointerEvent, ResizeObserver, WheelEvent,
};

use crate::audio::{RawAudioPlayer, RawFormat};
use crate::display::{DisplayEvent, DrawOp, ImageSlot, ImageState, Op};
use crate::geom::Rect;
use crate::gl::CanvasTarget;
use crate::input::{self, Keyboard, WheelAccumulator};
use crate::scope;
use crate::tunnel::{Tunnel, TunnelHandlers};
use crate::{display::Display, gl::Renderer};

type ResizeCallback = Closure<dyn FnMut(js_sys::Array)>;

pub type Shared = Rc<RefCell<Inner>>;
pub type WeakShared = Weak<RefCell<Inner>>;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

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

enum InStream {
    /// Compressed video decoded with WebCodecs into a layer.
    Video(crate::video::VideoStream),
    /// PCM forwarded to the page (worker mode, where Web Audio is missing).
    AudioForward,
    Image {
        mimetype: String,
        data: Vec<u8>,
        slot: ImageSlot,
    },
    Clipboard {
        data: Vec<u8>,
    },
    Audio,
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
    keyboard: Keyboard,
    wheel: WheelAccumulator,
    mouse: (i32, i32, MouseButtons),
    last_sync_ack: Option<i64>,
    handler: Option<js_sys::Function>,
    events: Vec<(&'static str, JsValue)>,
    audio_context: Option<AudioContext>,
    audio: HashMap<i32, RawAudioPlayer>,
    /// Forward audio streams to the page instead of playing them here.
    audio_forward: bool,
    scale_mode: ScaleMode,
    auto_resize: bool,
    pending_resize: Option<(u32, u32)>,
    raf_pending: bool,
    raf_id: Option<i32>,
    raf_closure: Option<Closure<dyn FnMut(f64)>>,
    timer: Option<(i32, Closure<dyn FnMut()>)>,
    last_received: f64,
    last_nop: f64,
    ping_sent_at: Option<f64>,
    rtt_ms: Option<f64>,
    unstable: bool,
    last_lag_ms: f64,
    counters: Counters,
    stats_since: f64,
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
                keyboard: Keyboard::default(),
                wheel: WheelAccumulator::default(),
                mouse: (0, 0, MouseButtons::default()),
                last_sync_ack: None,
                handler: None,
                events: Vec::new(),
                audio_context: None,
                audio: HashMap::new(),
                audio_forward: false,
                scale_mode: ScaleMode::Fit,
                auto_resize: true,
                pending_resize: None,
                raf_pending: false,
                raf_id: None,
                raf_closure: None,
                timer: None,
                last_received: now(),
                last_nop: now(),
                ping_sent_at: None,
                rtt_ms: None,
                unstable: false,
                last_lag_ms: 0.0,
                counters: Counters::default(),
                stats_since: now(),
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
        for image in ["image/png", "image/jpeg", "image/webp"] {
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
        self.display.reset();
        self.parser = Some(Parser::new());
        self.last_sync_ack = None;
        self.counters = Counters::default();
        self.set_state(State::Connecting);

        let weak_open = self.self_ref.clone();
        let weak_msg = self.self_ref.clone();
        let weak_close = self.self_ref.clone();
        let tunnel = Tunnel::connect(
            url,
            TunnelHandlers {
                on_open: Box::new(move || {
                    with_inner(&weak_open, |inner| {
                        inner.last_received = now();
                        inner.set_state(State::Waiting);
                    })
                }),
                on_message: Box::new(move |text| {
                    with_inner(&weak_msg, |inner| inner.receive(text))
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
        self.disconnect_internal(None);
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
        let releases = self.keyboard.release_all();
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

    fn receive(&mut self, text: String) {
        self.last_received = now();
        self.counters.bytes += text.len() as u64;
        let Some(mut parser) = self.parser.take() else {
            return;
        };
        parser.feed(text.as_bytes());
        loop {
            let raw = match parser.next_raw() {
                Ok(Some(raw)) => raw,
                Ok(None) => break,
                Err(err) => {
                    self.fail(Status::SERVER_ERROR, &format!("protocol error: {err}"));
                    return;
                }
            };
            self.counters.instructions += 1;
            match raw.opcode() {
                "" => {
                    // Tunnel-internal: UUID announcement or ping reply.
                    if raw.arg(0) == "ping"
                        && let Some(sent) = self.ping_sent_at.take()
                    {
                        self.rtt_ms = Some(now() - sent);
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
                return;
            }
        }
        self.parser = Some(parser);
        self.display.process();
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
                let slot: ImageSlot = Rc::new(RefCell::new(ImageState::Pending));
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
                        data: Vec::new(),
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
                let slot: ImageSlot = Rc::new(RefCell::new(ImageState::Pending));
                self.display.enqueue(Op::Image {
                    layer,
                    mode,
                    x,
                    y,
                    slot: slot.clone(),
                });
                let bytes = B64.decode(data.as_bytes()).unwrap_or_default();
                self.decode_image(bytes, mimetype, slot);
            }
            S::Blob { stream, data } => self.on_blob(stream, data),
            S::End { stream } => self.on_end(stream),
            S::Audio { stream, mimetype } => self.on_audio(stream, mimetype),
            S::Clipboard { stream, .. } => {
                self.streams
                    .insert(stream, InStream::Clipboard { data: Vec::new() });
            }
            S::File { stream, .. }
            | S::Pipe { stream, .. }
            | S::Argv { stream, .. }
            | S::Body { stream, .. } => {
                self.send_ack(stream, "Unsupported", Status::UNSUPPORTED);
            }
            S::Video {
                stream,
                layer,
                mimetype,
            } => self.on_video(stream, layer, mimetype),
            S::Ack { stream, status, .. } => {
                if status.is_error() {
                    self.free_out_stream(stream);
                }
            }
            S::Sync { timestamp, frames } => {
                self.display.enqueue(Op::Sync {
                    timestamp,
                    frames,
                    received_at: scope::performance_now(),
                });
            }
            S::Mouse { x, y } => self.display.show_software_cursor(x, y),
            S::Name { name } => self.emit("name", JsValue::from_str(name)),
            S::Msg { id } => self.emit("msg", JsValue::from(id)),
            S::Error { message, status } => {
                let message = message.to_owned();
                self.fail(status, &message);
            }
            S::Disconnect => self.disconnect_internal(None),
            S::Log { message } => web_sys::console::log_1(&format!("guacd: {message}").into()),
            S::Filesystem { .. }
            | S::Undefine { .. }
            | S::Nest { .. }
            | S::Nop
            | S::Ready { .. }
            | S::Required
            | S::Unknown { .. } => {}
        }
    }

    fn on_blob(&mut self, stream: i32, data: &str) {
        match self.streams.get_mut(&stream) {
            Some(InStream::Image { data: buf, .. }) | Some(InStream::Clipboard { data: buf }) => {
                let _ = B64.decode_vec(data.as_bytes(), buf);
            }
            Some(InStream::Audio) => {
                if let (Ok(pcm), Some(player)) =
                    (B64.decode(data.as_bytes()), self.audio.get_mut(&stream))
                {
                    player.play(&pcm);
                }
            }
            Some(InStream::AudioForward) => {
                if let Ok(pcm) = B64.decode(data.as_bytes()) {
                    let pcm = js_sys::Uint8Array::from(pcm.as_slice());
                    self.emit(
                        "audio-data",
                        object(&[("stream", JsValue::from(stream)), ("data", pcm.into())]),
                    );
                }
            }
            Some(InStream::Video(video)) => {
                let layer = video.layer;
                let result = B64
                    .decode(data.as_bytes())
                    .map_err(|e| e.to_string())
                    .and_then(|bytes| video.push(&bytes));
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
            }) => self.decode_image(data, &mimetype, slot),
            Some(InStream::Clipboard { data }) => {
                let text = String::from_utf8_lossy(&data).into_owned();
                self.emit("clipboard", JsValue::from_str(&text));
            }
            Some(InStream::Audio) => {
                self.audio.remove(&stream);
            }
            Some(InStream::AudioForward) => {
                self.emit("audio-end", object(&[("stream", JsValue::from(stream))]));
            }
            Some(InStream::Video(video)) => video.finish(),
            None => {}
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
    fn decode_image(&mut self, bytes: Vec<u8>, mimetype: &str, slot: ImageSlot) {
        let weak = self.self_ref.clone();
        let mimetype = mimetype.to_owned();
        let started = scope::performance_now();
        wasm_bindgen_futures::spawn_local(async move {
            *slot.borrow_mut() = match crate::image::decode(bytes, mimetype).await {
                Ok(image) => ImageState::Ready(image),
                Err(err) => {
                    web_sys::console::warn_2(&"image decode failed".into(), &err);
                    ImageState::Failed
                }
            };
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
    pub fn key_down(&mut self, key: &str, code: &str, location: u32, composing: bool) -> bool {
        self.resume_audio();
        match self.keyboard.key_down(key, code, location, composing) {
            Some(events) => {
                for (keysym, pressed) in events {
                    self.send_key(keysym, pressed);
                }
                true
            }
            None => false,
        }
    }

    pub fn key_up(&mut self, key: &str, code: &str, location: u32) -> bool {
        let events = self.keyboard.key_up(key, code, location);
        let consumed = !events.is_empty();
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
        ) {
            event.prevent_default();
        }
    }

    fn on_key_up(&mut self, event: KeyboardEvent) {
        if self.key_up(&event.key(), &event.code(), event.location()) {
            event.prevent_default();
        }
    }

    /// Types text (e.g. a finished IME composition) on the remote side.
    pub fn type_text(&mut self, text: &str) {
        for (keysym, pressed) in input::type_text(text) {
            self.send_key(keysym, pressed);
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

    pub fn set_auto_resize(&mut self, enabled: bool) {
        self.auto_resize = enabled;
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
            let w = (size.0 * self.dpr).round() as u32;
            let h = (size.1 * self.dpr).round() as u32;
            if w >= 64 && h >= 64 {
                // Sent from the timer so a window drag produces one request.
                self.pending_resize = Some((w, h));
            }
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
                    timestamp, lag_ms, ..
                } => {
                    self.last_lag_ms = lag_ms;
                    if self.last_sync_ack != Some(timestamp) {
                        self.last_sync_ack = Some(timestamp);
                        self.send(&InstructionBuilder::new("sync").arg(timestamp).finish());
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
        if self.display.needs_composite() && !self.raf_pending {
            self.request_frame();
        }
    }

    fn request_frame(&mut self) {
        if self.raf_closure.is_none() {
            let weak = self.self_ref.clone();
            self.raf_closure = Some(Closure::<dyn FnMut(f64)>::new(move |_t: f64| {
                with_inner(&weak, |inner| {
                    inner.raf_pending = false;
                    inner.raf_id = None;
                    inner.display.poll_cursor();
                    inner.display.composite();
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
                self.fail(Status::UPSTREAM_TIMEOUT, "connection timed out");
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
                (
                    "rttMs",
                    self.rtt_ms.map_or(JsValue::NULL, JsValue::from_f64),
                ),
                ("sendBuffered", JsValue::from_f64(buffered as f64)),
                ("displayWidth", JsValue::from(self.display.width())),
                ("displayHeight", JsValue::from(self.display.height())),
            ]),
        );
    }

    /// Processes instructions as if they had arrived from the server. Used
    /// by tests and for replaying recordings.
    pub fn feed(&mut self, text: &str) {
        if self.parser.is_none() {
            self.parser = Some(Parser::new());
        }
        let last = self.last_received;
        self.receive(text.to_owned());
        self.last_received = last;
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

//! WebSocket tunnel to the gateway.
//!
//! The subprotocol `guacamole` is the stock tunnel: text messages of
//! instructions. Unless told otherwise the client also offers
//! `molcajete-binary-v1` (`molcajete_protocol::binary`); a gateway that
//! knows it sends what comes from the server as binary messages, with stream
//! data as raw bytes. Which one is in use only shows in the messages that
//! arrive, so both kinds are always accepted.

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{BinaryType, CloseEvent, Event, MessageEvent, WebSocket};

/// A message from the gateway.
pub enum TunnelMessage {
    /// Instructions as text.
    Text(String),
    /// Records (`molcajete_protocol::binary`).
    Binary(js_sys::ArrayBuffer),
}

/// Callbacks invoked by the tunnel.
pub struct TunnelHandlers {
    pub on_open: Box<dyn FnMut()>,
    pub on_message: Box<dyn FnMut(TunnelMessage)>,
    /// (close code, reason)
    pub on_close: Box<dyn FnMut(u16, String)>,
}

pub struct Tunnel {
    socket: WebSocket,
    _on_open: Closure<dyn FnMut(Event)>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(CloseEvent)>,
}

impl Tunnel {
    /// Opens the tunnel. With `binary`, the binary subprotocol is offered
    /// in addition to `guacamole`.
    pub fn connect(url: &str, binary: bool, handlers: TunnelHandlers) -> Result<Tunnel, JsValue> {
        let protocols = js_sys::Array::new();
        if binary {
            protocols.push(&molcajete_protocol::binary::WEBSOCKET_SUBPROTOCOL.into());
        }
        protocols.push(&"guacamole".into());
        let socket = WebSocket::new_with_str_sequence(url, &protocols)?;
        socket.set_binary_type(BinaryType::Arraybuffer);
        let TunnelHandlers {
            mut on_open,
            mut on_message,
            mut on_close,
        } = handlers;

        let open = Closure::<dyn FnMut(Event)>::new(move |_| on_open());
        let message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let data = event.data();
            if let Some(text) = data.as_string() {
                on_message(TunnelMessage::Text(text));
            } else if let Ok(buffer) = data.dyn_into::<js_sys::ArrayBuffer>() {
                on_message(TunnelMessage::Binary(buffer));
            }
        });
        let close = Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
            on_close(event.code(), event.reason());
        });
        socket.set_onopen(Some(open.as_ref().unchecked_ref()));
        socket.set_onmessage(Some(message.as_ref().unchecked_ref()));
        socket.set_onclose(Some(close.as_ref().unchecked_ref()));

        Ok(Tunnel {
            socket,
            _on_open: open,
            _on_message: message,
            _on_close: close,
        })
    }

    /// The subprotocol the gateway selected (empty until the tunnel is
    /// open).
    pub fn protocol(&self) -> String {
        self.socket.protocol()
    }

    pub fn is_open(&self) -> bool {
        self.socket.ready_state() == WebSocket::OPEN
    }

    pub fn send(&self, data: &str) {
        if self.is_open() {
            let _ = self.socket.send_with_str(data);
        }
    }

    /// Bytes queued in the socket but not yet transmitted.
    pub fn buffered_amount(&self) -> u32 {
        self.socket.buffered_amount()
    }

    pub fn close(&self) {
        let _ = self.socket.close();
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.socket.set_onopen(None);
        self.socket.set_onmessage(None);
        self.socket.set_onclose(None);
        let _ = self.socket.close();
    }
}

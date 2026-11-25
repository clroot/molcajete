//! WebSocket tunnel to the gateway (subprotocol `guacamole`).

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{CloseEvent, Event, MessageEvent, WebSocket};

/// Callbacks invoked by the tunnel.
pub struct TunnelHandlers {
    pub on_open: Box<dyn FnMut()>,
    pub on_message: Box<dyn FnMut(String)>,
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
    pub fn connect(url: &str, handlers: TunnelHandlers) -> Result<Tunnel, JsValue> {
        let socket = WebSocket::new_with_str(url, "guacamole")?;
        let TunnelHandlers {
            mut on_open,
            mut on_message,
            mut on_close,
        } = handlers;

        let open = Closure::<dyn FnMut(Event)>::new(move |_| on_open());
        let message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            if let Some(text) = event.data().as_string() {
                on_message(text);
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

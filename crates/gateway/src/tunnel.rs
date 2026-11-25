//! WebSocket tunnel: relays Guacamole instructions between the browser and
//! guacd.
//!
//! Wire behaviour matches guacamole-common's `GuacamoleWebSocketTunnelEndpoint`
//! so that both our wasm client and the stock guacamole-common-js client work:
//!
//! - subprotocol `guacamole`, text messages only;
//! - every message contains whole instructions;
//! - the first message is `0.,36.<tunnel uuid>;`;
//! - internal `0.,4.ping,<ts>;` messages are echoed and never reach guacd;
//! - on termination the close reason carries the numeric Guacamole status.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket};
use futures_util::{SinkExt, StreamExt};
use metrics::{Counter, counter, gauge, histogram};
use molcajete_protocol::{InstructionBuilder, Limits, Parser, Status};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tracing::{Instrument as _, debug, info, info_span, warn};
use uuid::Uuid;

use crate::AppState;
use crate::guacd::{self, ClientInfo, GuacdSession};

/// Upper bound for a single outgoing WebSocket message.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const READ_BUFFER_BYTES: usize = 64 * 1024;

/// Parameters supplied by the browser in the tunnel URL query string.
///
/// Both our own short names and the `GUAC_*` names used by the stock
/// Guacamole web application are accepted.
#[derive(Debug, Clone, Default)]
pub struct TunnelRequest {
    pub connection: Option<String>,
    /// Opaque id chosen by the browser page (`session=`), used to correlate
    /// the tunnel with the client's performance reports.
    pub session: Option<String>,
    /// Address of the browser, as seen by the gateway or reported by a
    /// trusted reverse proxy (`X-Forwarded-For`).
    pub peer: Option<String>,
    pub info: ClientInfo,
}

impl TunnelRequest {
    pub fn from_query(query: &str) -> Self {
        let mut req = TunnelRequest::default();
        let mut audio = Vec::new();
        let mut video = Vec::new();
        let mut image = Vec::new();
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            let value = value.into_owned();
            match key.as_ref() {
                "connection" | "id" | "GUAC_ID" => req.connection = Some(value),
                "session" => req.session = Some(value.chars().take(64).collect()),
                "width" | "GUAC_WIDTH" => {
                    if let Ok(v) = value.parse() {
                        req.info.width = clamp_dimension(v);
                    }
                }
                "height" | "GUAC_HEIGHT" => {
                    if let Ok(v) = value.parse() {
                        req.info.height = clamp_dimension(v);
                    }
                }
                "dpi" | "GUAC_DPI" => {
                    if let Ok(v) = value.parse::<u32>() {
                        req.info.dpi = v.clamp(24, 1200);
                    }
                }
                "audio" | "GUAC_AUDIO" => audio.push(value),
                "video" | "GUAC_VIDEO" => video.push(value),
                "image" | "GUAC_IMAGE" => image.push(value),
                "timezone" | "GUAC_TIMEZONE" => req.info.timezone = Some(value),
                _ => {}
            }
        }
        req.info.audio = audio;
        req.info.video = video;
        if !image.is_empty() {
            req.info.image = image;
        }
        req
    }
}

fn clamp_dimension(v: u32) -> u32 {
    v.clamp(1, 16384)
}

fn close_message(status: Status) -> Message {
    Message::Close(Some(CloseFrame {
        code: status.websocket_close_code(),
        reason: Utf8Bytes::from(status.code().to_string()),
    }))
}

/// Entry point after the WebSocket upgrade.
pub async fn serve(socket: WebSocket, state: Arc<AppState>, request: TunnelRequest) {
    let tunnel_id = Uuid::new_v4();
    let span = info_span!(
        "tunnel",
        %tunnel_id,
        connection = request.connection.as_deref().unwrap_or(""),
        session = request.session.as_deref().unwrap_or(""),
        peer = request.peer.as_deref().unwrap_or(""),
    );
    serve_tunnel(socket, state, request, tunnel_id)
        .instrument(span)
        .await;
}

async fn serve_tunnel(
    mut socket: WebSocket,
    state: Arc<AppState>,
    request: TunnelRequest,
    tunnel_id: Uuid,
) {
    let Some(connection) = request
        .connection
        .as_deref()
        .and_then(|id| state.config.connection(id))
    else {
        warn!(connection = ?request.connection, "tunnel requested for unknown connection");
        counter!("molcajete_gateway_tunnels_rejected_total", "reason" => "unknown_connection")
            .increment(1);
        let _ = socket.send(close_message(Status::RESOURCE_NOT_FOUND)).await;
        return;
    };
    let labels = [
        ("connection", connection.id.clone()),
        ("protocol", connection.protocol.clone()),
    ];

    let guacd_cfg = connection.guacd.as_ref().unwrap_or(&state.config.guacd);
    let parameters: &BTreeMap<String, String> = &connection.parameters;
    info!(
        connection = %connection.id,
        protocol = %connection.protocol,
        width = request.info.width,
        height = request.info.height,
        "opening tunnel"
    );

    let handshake_started = Instant::now();
    let session = match guacd::connect(
        (guacd_cfg.host.as_str(), guacd_cfg.port),
        &connection.protocol,
        parameters,
        &request.info,
        Duration::from_secs(guacd_cfg.handshake_timeout),
    )
    .await
    {
        Ok(session) => session,
        Err(err) => {
            warn!(error = %err, "guacd handshake failed");
            counter!("molcajete_gateway_handshake_failures_total", &labels).increment(1);
            let _ = socket.send(close_message(err.status())).await;
            return;
        }
    };
    let handshake = handshake_started.elapsed();
    histogram!("molcajete_gateway_handshake_seconds", &labels).record(handshake);

    info!(
        guacd_id = %session.id,
        version = %session.version,
        handshake_ms = handshake.as_millis() as u64,
        "tunnel established"
    );
    let active = gauge!("molcajete_gateway_tunnels_active", &labels);
    active.increment(1.0);
    let started = Instant::now();
    let counters = RelayCounters {
        to_browser: counter!(
            "molcajete_gateway_relayed_bytes_total",
            &with(&labels, "direction", "to_browser")
        ),
        to_guacd: counter!(
            "molcajete_gateway_relayed_bytes_total",
            &with(&labels, "direction", "to_guacd")
        ),
    };
    let stats = relay(socket, session, tunnel_id, &counters).await;
    active.decrement(1.0);
    let duration = started.elapsed();
    histogram!("molcajete_gateway_tunnel_duration_seconds", &labels).record(duration);
    counter!(
        "molcajete_gateway_tunnels_total",
        &with(&labels, "status", stats.status.code().to_string())
    )
    .increment(1);
    info!(
        status = %stats.status,
        duration_s = duration.as_secs_f64(),
        bytes_to_browser = stats.to_browser,
        bytes_to_guacd = stats.to_guacd,
        "tunnel closed"
    );
}

fn with(
    labels: &[(&'static str, String); 2],
    key: &'static str,
    value: impl Into<String>,
) -> [(&'static str, String); 3] {
    [labels[0].clone(), labels[1].clone(), (key, value.into())]
}

/// Byte counters of one tunnel (no-ops when metrics are disabled).
pub struct RelayCounters {
    pub to_browser: Counter,
    pub to_guacd: Counter,
}

/// Outcome of [`relay`].
#[derive(Debug, Clone, Copy)]
pub struct RelayStats {
    /// Status reported to the browser.
    pub status: Status,
    pub to_browser: u64,
    pub to_guacd: u64,
}

/// Pumps data in both directions until either side terminates.
pub async fn relay(
    socket: WebSocket,
    session: GuacdSession,
    tunnel_id: Uuid,
    counters: &RelayCounters,
) -> RelayStats {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(32);

    // Single writer task so that both directions (data and ping replies) can
    // send to the browser.
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let closing = matches!(msg, Message::Close(_));
            if ws_tx.send(msg).await.is_err() || closing {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    let GuacdSession { stream, parser, .. } = session;
    let (guacd_rd, guacd_wr) = stream.into_split();

    let uuid = InstructionBuilder::new("")
        .str_arg(&tunnel_id.to_string())
        .finish();
    let mut to_browser = 0u64;
    let mut to_guacd = 0u64;
    let status = if out_tx.send(Message::Text(uuid.into())).await.is_err() {
        Status::SUCCESS
    } else {
        tokio::select! {
            status = guacd_to_browser(guacd_rd, parser, &out_tx, &counters.to_browser, &mut to_browser) => status,
            status = browser_to_guacd(&mut ws_rx, guacd_wr, &out_tx, &counters.to_guacd, &mut to_guacd) => status,
        }
    };

    let _ = out_tx.send(close_message(status)).await;
    drop(out_tx);
    let _ = writer.await;
    RelayStats {
        status,
        to_browser,
        to_guacd,
    }
}

async fn guacd_to_browser(
    mut rd: OwnedReadHalf,
    mut parser: Parser,
    out: &mpsc::Sender<Message>,
    counter: &Counter,
    total: &mut u64,
) -> Status {
    let mut buf = vec![0u8; READ_BUFFER_BYTES];
    let mut message = String::with_capacity(MAX_MESSAGE_BYTES);
    loop {
        // Forward every complete instruction; partial data stays buffered.
        loop {
            match parser.next_raw() {
                Ok(Some(raw)) => {
                    message.push_str(raw.as_str());
                    if message.len() >= MAX_MESSAGE_BYTES {
                        let chunk = std::mem::replace(
                            &mut message,
                            String::with_capacity(MAX_MESSAGE_BYTES),
                        );
                        if out.send(Message::Text(chunk.into())).await.is_err() {
                            return Status::SUCCESS;
                        }
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    warn!(error = %err, "invalid data from guacd");
                    return Status::UPSTREAM_ERROR;
                }
            }
        }
        // Flush before we might block on the next read.
        if !message.is_empty() {
            let chunk = std::mem::replace(&mut message, String::with_capacity(MAX_MESSAGE_BYTES));
            if out.send(Message::Text(chunk.into())).await.is_err() {
                return Status::SUCCESS;
            }
        }
        match rd.read(&mut buf).await {
            Ok(0) => {
                debug!("guacd closed the connection");
                return Status::SUCCESS;
            }
            Ok(n) => {
                counter.increment(n as u64);
                *total += n as u64;
                parser.feed(&buf[..n]);
            }
            Err(err) => {
                debug!(error = %err, "guacd read failed");
                return Status::UPSTREAM_ERROR;
            }
        }
    }
}

async fn browser_to_guacd(
    ws_rx: &mut futures_util::stream::SplitStream<WebSocket>,
    mut wr: OwnedWriteHalf,
    out: &mpsc::Sender<Message>,
    counter: &Counter,
    total: &mut u64,
) -> Status {
    let mut parser = Parser::with_limits(Limits::STRICT);
    let mut forward = Vec::with_capacity(4096);
    while let Some(msg) = ws_rx.next().await {
        let text = match msg {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) => return Status::SUCCESS,
            Ok(_) => continue,
            Err(err) => {
                debug!(error = %err, "websocket receive failed");
                return Status::SUCCESS;
            }
        };
        parser.feed(text.as_bytes());
        forward.clear();
        loop {
            match parser.next_raw() {
                Ok(Some(raw)) if raw.opcode().is_empty() => {
                    // Tunnel-internal instruction: answer pings, drop the rest.
                    if raw.arg(0) == "ping" {
                        let reply = InstructionBuilder::new("")
                            .str_arg("ping")
                            .str_arg(raw.arg(1))
                            .finish();
                        if out.send(Message::Text(reply.into())).await.is_err() {
                            return Status::SUCCESS;
                        }
                    }
                }
                Ok(Some(raw)) => forward.extend_from_slice(raw.as_bytes()),
                Ok(None) => break,
                Err(err) => {
                    warn!(error = %err, "invalid data from browser");
                    return Status::CLIENT_BAD_REQUEST;
                }
            }
        }
        if !forward.is_empty() {
            if wr.write_all(&forward).await.is_err() {
                return Status::UPSTREAM_ERROR;
            }
            counter.increment(forward.len() as u64);
            *total += forward.len() as u64;
        }
    }
    Status::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_short_and_guac_style_queries() {
        let req = TunnelRequest::from_query(
            "connection=vnc1&width=1920&height=1080&dpi=144&image=image%2Fwebp&audio=audio%2FL16&timezone=Asia%2FSeoul",
        );
        assert_eq!(req.connection.as_deref(), Some("vnc1"));
        assert_eq!(
            (req.info.width, req.info.height, req.info.dpi),
            (1920, 1080, 144)
        );
        assert_eq!(req.info.image, ["image/webp"]);
        assert_eq!(req.info.audio, ["audio/L16"]);
        assert_eq!(req.info.timezone.as_deref(), Some("Asia/Seoul"));

        let req = TunnelRequest::from_query(
            "GUAC_ID=x&GUAC_WIDTH=800&GUAC_IMAGE=image/png&GUAC_IMAGE=image/jpeg",
        );
        assert_eq!(req.connection.as_deref(), Some("x"));
        assert_eq!(req.info.width, 800);
        assert_eq!(req.info.image, ["image/png", "image/jpeg"]);
    }
}

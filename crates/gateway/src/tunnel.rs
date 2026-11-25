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
//!
//! A client that also offers the subprotocol `molcajete-binary-v1` gets it
//! instead. What comes from guacd then reaches the browser as binary
//! messages made of whole records (`molcajete_protocol::binary`): stream
//! data as raw bytes, everything else as instruction text. molcajeted sends
//! records itself and they are forwarded as they are; the `blob`
//! instructions of a stock guacd are decoded here. The tunnel's own messages
//! (UUID, ping replies) and everything the browser sends stay text.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use metrics::{Counter, counter, gauge, histogram};
use molcajete_protocol::binary::{self, RecordReader, RecordWriter};
use molcajete_protocol::{InstructionBuilder, Limits, Parser, RawInstruction, Status};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tracing::{Instrument as _, debug, info, info_span, warn};
use uuid::Uuid;

use crate::AppState;
use crate::db::ConnectionRecord;
use crate::guacd::{self, ClientInfo, GuacdSession};

/// WebSocket subprotocol of the stock Guacamole tunnel.
pub const TEXT_SUBPROTOCOL: &str = "guacamole";
pub use binary::WEBSOCKET_SUBPROTOCOL as BINARY_SUBPROTOCOL;

/// Size at which an outgoing WebSocket message is sent without waiting for
/// more. A message ends with a whole instruction or record, so it can be
/// larger by one of those.
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
    /// The signed-in user opening the tunnel. Only used for logs.
    pub user: Option<String>,
    /// The browser selected the binary subprotocol.
    pub binary: bool,
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
pub async fn serve(
    socket: WebSocket,
    state: Arc<AppState>,
    request: TunnelRequest,
    connection: Option<ConnectionRecord>,
) {
    let tunnel_id = Uuid::new_v4();
    let span = info_span!(
        "tunnel",
        %tunnel_id,
        connection = request.connection.as_deref().unwrap_or(""),
        session = request.session.as_deref().unwrap_or(""),
        peer = request.peer.as_deref().unwrap_or(""),
        user = request.user.as_deref().unwrap_or(""),
    );
    serve_tunnel(socket, state, request, connection, tunnel_id)
        .instrument(span)
        .await;
}

async fn serve_tunnel(
    mut socket: WebSocket,
    state: Arc<AppState>,
    request: TunnelRequest,
    connection: Option<ConnectionRecord>,
    tunnel_id: Uuid,
) {
    let Some(connection) = connection else {
        warn!(connection = ?request.connection, "tunnel requested for unknown or forbidden connection");
        counter!("molcajete_gateway_tunnels_rejected_total", "reason" => "unknown_connection")
            .increment(1);
        let _ = socket.send(close_message(Status::RESOURCE_NOT_FOUND)).await;
        return;
    };
    let labels = [
        ("connection", connection.id.clone()),
        ("protocol", connection.protocol.clone()),
    ];

    let defaults = &state.config.guacd;
    let guacd_host = connection.guacd_host.as_deref().unwrap_or(&defaults.host);
    let guacd_port = connection.guacd_port.unwrap_or(defaults.port);
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
        (guacd_host, guacd_port),
        &connection.protocol,
        parameters,
        &request.info,
        request.binary,
        Duration::from_secs(defaults.handshake_timeout),
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
        framing = Framing::of(request.binary, &session).as_str(),
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
    let stats = relay(socket, session, tunnel_id, request.binary, &counters).await;
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

/// How data from guacd reaches the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// Text messages of instructions (stock Guacamole).
    Text,
    /// Binary messages; the daemon sends records and they are forwarded.
    Binary,
    /// Binary messages; the daemon sends text and its blobs are decoded.
    Transcoded,
}

impl Framing {
    fn of(browser_binary: bool, session: &GuacdSession) -> Self {
        match (browser_binary, session.binary) {
            (true, true) => Self::Binary,
            (true, false) => Self::Transcoded,
            // A daemon is only asked for records on behalf of a browser
            // that takes them.
            (false, _) => Self::Text,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Binary => "binary",
            Self::Transcoded => "transcoded",
        }
    }
}

/// Pumps data in both directions until either side terminates.
/// `browser_binary` tells whether the browser selected the binary
/// subprotocol.
pub async fn relay(
    socket: WebSocket,
    session: GuacdSession,
    tunnel_id: Uuid,
    browser_binary: bool,
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

    let framing = Framing::of(browser_binary, &session);
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
        let mut upstream = Upstream {
            rd: guacd_rd,
            out: &out_tx,
            counter: &counters.to_browser,
            total: &mut to_browser,
        };
        let to_browser = async {
            match framing {
                Framing::Text => upstream.text(parser).await,
                Framing::Binary => upstream.records(parser.pending()).await,
                Framing::Transcoded => upstream.transcoded(parser).await,
            }
        };
        tokio::select! {
            status = to_browser => status,
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

/// The guacd → browser direction of a tunnel.
struct Upstream<'a> {
    rd: OwnedReadHalf,
    out: &'a mpsc::Sender<Message>,
    counter: &'a Counter,
    total: &'a mut u64,
}

impl Upstream<'_> {
    /// Reads more from guacd. `Err` carries the status the tunnel ends with.
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Status> {
        match self.rd.read(buf).await {
            Ok(0) => {
                debug!("guacd closed the connection");
                Err(Status::SUCCESS)
            }
            Ok(n) => {
                self.counter.increment(n as u64);
                *self.total += n as u64;
                Ok(n)
            }
            Err(err) => {
                debug!(error = %err, "guacd read failed");
                Err(Status::UPSTREAM_ERROR)
            }
        }
    }

    /// Sends a message to the browser; `Err` when the browser is gone.
    async fn send(&self, message: Message) -> Result<(), Status> {
        self.out.send(message).await.map_err(|_| Status::SUCCESS)
    }

    /// Instructions as text messages (stock Guacamole).
    async fn text(&mut self, mut parser: Parser) -> Status {
        let mut buf = vec![0u8; READ_BUFFER_BYTES];
        let mut message = String::with_capacity(MAX_MESSAGE_BYTES);
        let result: Result<(), Status> = async {
            loop {
                // Forward every complete instruction; partial data stays
                // buffered.
                loop {
                    match parser.next_raw() {
                        Ok(Some(raw)) => {
                            message.push_str(raw.as_str());
                            if message.len() >= MAX_MESSAGE_BYTES {
                                let chunk = std::mem::replace(
                                    &mut message,
                                    String::with_capacity(MAX_MESSAGE_BYTES),
                                );
                                self.send(Message::Text(chunk.into())).await?;
                            }
                        }
                        Ok(None) => break,
                        Err(err) => {
                            warn!(error = %err, "invalid data from guacd");
                            return Err(Status::UPSTREAM_ERROR);
                        }
                    }
                }
                // Flush before we might block on the next read.
                if !message.is_empty() {
                    let chunk =
                        std::mem::replace(&mut message, String::with_capacity(MAX_MESSAGE_BYTES));
                    self.send(Message::Text(chunk.into())).await?;
                }
                let n = self.read(&mut buf).await?;
                parser.feed(&buf[..n]);
            }
        }
        .await;
        result.unwrap_err()
    }

    /// Records from molcajeted, forwarded unchanged in binary messages.
    /// `pending` is what arrived together with the end of the handshake.
    async fn records(&mut self, pending: &[u8]) -> Status {
        let mut buf = vec![0u8; READ_BUFFER_BYTES];
        let mut reader = RecordReader::new();
        reader.feed(pending);
        let mut message = Vec::with_capacity(MAX_MESSAGE_BYTES);
        let result: Result<(), Status> = async {
            loop {
                loop {
                    match reader.next_record() {
                        Ok(Some(record)) => {
                            let bytes = record.as_bytes();
                            if !message.is_empty()
                                && message.len() + bytes.len() > MAX_MESSAGE_BYTES
                            {
                                let chunk = std::mem::replace(
                                    &mut message,
                                    Vec::with_capacity(MAX_MESSAGE_BYTES),
                                );
                                self.send(Message::Binary(chunk.into())).await?;
                            }
                            message.extend_from_slice(bytes);
                        }
                        Ok(None) => break,
                        Err(err) => {
                            warn!(error = %err, "invalid data from guacd");
                            return Err(Status::UPSTREAM_ERROR);
                        }
                    }
                }
                if !message.is_empty() {
                    let chunk =
                        std::mem::replace(&mut message, Vec::with_capacity(MAX_MESSAGE_BYTES));
                    self.send(Message::Binary(chunk.into())).await?;
                }
                let n = self.read(&mut buf).await?;
                reader.feed(&buf[..n]);
            }
        }
        .await;
        result.unwrap_err()
    }

    /// Text from a stock guacd, turned into records: the data of `blob`
    /// instructions is decoded, everything else is passed on as text.
    async fn transcoded(&mut self, mut parser: Parser) -> Status {
        let mut buf = vec![0u8; READ_BUFFER_BYTES];
        let mut message = RecordWriter::new();
        let mut data = Vec::new();
        let result: Result<(), Status> = async {
            loop {
                loop {
                    match parser.next_raw() {
                        Ok(Some(raw)) => {
                            push_record(&mut message, &raw, &mut data);
                            if message.len() >= MAX_MESSAGE_BYTES {
                                self.send(Message::Binary(message.take().into())).await?;
                            }
                        }
                        Ok(None) => break,
                        Err(err) => {
                            warn!(error = %err, "invalid data from guacd");
                            return Err(Status::UPSTREAM_ERROR);
                        }
                    }
                }
                if !message.is_empty() {
                    self.send(Message::Binary(message.take().into())).await?;
                }
                let n = self.read(&mut buf).await?;
                parser.feed(&buf[..n]);
            }
        }
        .await;
        result.unwrap_err()
    }
}

/// Appends an instruction to a binary message: a `blob` as a blob record,
/// anything else (including a blob that is not valid base64, which is left
/// to the client) as text. `scratch` is reused between calls.
fn push_record(message: &mut RecordWriter, raw: &RawInstruction<'_>, scratch: &mut Vec<u8>) {
    if raw.opcode() == "blob"
        && raw.arg_count() == 2
        && let Ok(stream) = raw.arg(0).parse::<i32>()
    {
        scratch.clear();
        let base64 = base64::engine::general_purpose::STANDARD;
        if base64.decode_vec(raw.arg(1), scratch).is_ok() {
            message.blob(stream, scratch);
            return;
        }
    }
    message.instructions(raw.as_bytes());
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

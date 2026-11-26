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
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
use tracing::{Instrument as _, debug, error, info, info_span, warn};
use uuid::Uuid;

use crate::AppState;
use crate::db::{ConnectionRecord, attributes, now_millis};
use crate::guacd::{self, ClientInfo, GuacdSession};
use crate::recording::{self, Origin, RecordedUser, Recorder};
use crate::sessions::{self, SessionStart, Traffic};

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
    /// Opaque id the page gives its tunnel (`page=`), by which its user
    /// shares the session (`sessions::share`).
    pub page: Option<String>,
    /// A share link's token (`join=`): the tunnel joins that link's
    /// session instead of opening a connection.
    pub join: Option<String>,
    /// A live session's tunnel id (`shadow=`): an administrator watches it.
    pub shadow: Option<String>,
    /// A one-time credential's token (`ironrdp=`): the browser runs
    /// IronRDP's client and molcajeted relays RDP (`crate::ironrdp`).
    pub ironrdp: Option<String>,
    /// Address of the browser, as seen by the gateway or reported by a
    /// trusted reverse proxy (`X-Forwarded-For`).
    pub peer: Option<String>,
    /// The signed-in user opening the tunnel, for logs and recordings.
    pub user: Option<RecordedUser>,
    /// The browser selected the binary subprotocol.
    pub binary: bool,
    pub info: ClientInfo,
    /// Connection parameters this browser sets for itself
    /// (`param.<name>=<value>`, see [`valid_override`]). An empty value
    /// removes the connection's own.
    pub overrides: BTreeMap<String, String>,
}

/// Whether a browser may set parameter `name` to `value` for its own
/// session. Only how the remote desktop follows the window and the text
/// size of a terminal can be set, which the session menu changes during a
/// session anyway.
pub fn valid_override(name: &str, value: &str) -> bool {
    match name {
        "dpi" => value.is_empty() || value.parse::<u32>().is_ok_and(|d| (24..=1200).contains(&d)),
        "resize-method" => matches!(value, "" | "display-update"),
        // Points.
        "font-size" => {
            value.is_empty()
                || value
                    .parse::<f32>()
                    .is_ok_and(|s| (4.0..=72.0).contains(&s))
        }
        _ => false,
    }
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
                "page" => req.page = Some(value.chars().take(64).collect()),
                "join" => req.join = Some(value.chars().take(128).collect()),
                "shadow" => req.shadow = Some(value.chars().take(64).collect()),
                "ironrdp" => req.ironrdp = Some(value.chars().take(128).collect()),
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
                key => {
                    if let Some(name) = key.strip_prefix("param.")
                        && valid_override(name, &value)
                    {
                        req.overrides.insert(name.to_owned(), value);
                    }
                }
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

impl TunnelRequest {
    /// The connection's parameters with this browser's overrides applied.
    pub fn parameters(&self, connection: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut parameters = connection.clone();
        for (name, value) in &self.overrides {
            if value.is_empty() {
                parameters.remove(name);
            } else {
                parameters.insert(name.clone(), value.clone());
            }
        }
        parameters
    }
}

fn clamp_dimension(v: u32) -> u32 {
    v.clamp(1, 16384)
}

pub(crate) fn close_message(status: Status) -> Message {
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
        user = request.user.as_ref().map_or("", |u| u.username.as_str()),
    );
    match request.ironrdp.clone() {
        Some(token) => {
            crate::ironrdp::serve(socket, state, request, connection, &token, tunnel_id)
                .instrument(span)
                .await
        }
        None => {
            serve_tunnel(socket, state, request, connection, tunnel_id)
                .instrument(span)
                .await
        }
    }
}

async fn serve_tunnel(
    mut socket: WebSocket,
    state: Arc<AppState>,
    mut request: TunnelRequest,
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
    let handshake_timeout = connection
        .guacd_handshake_timeout
        .unwrap_or(defaults.handshake_timeout);
    // Stored credentials are decrypted here, for the handshake only.
    let mut stored = connection.parameters.clone();
    match state.db.connection_secrets(&connection.id).await {
        Ok(secrets) => stored.extend(secrets),
        Err(err) => {
            error!(connection = %connection.id, error = %err, "reading the connection's credentials failed");
            counter!("molcajete_gateway_tunnels_rejected_total", "reason" => "credentials")
                .increment(1);
            let _ = socket.send(close_message(Status::SERVER_ERROR)).await;
            return;
        }
    }
    let parameters = request.parameters(&stored);
    let record = recording::wanted(&state.config.recordings, &connection);
    if record && !connection.flag(attributes::RECORD_MOLCAJETE_FORMATS) {
        recording::stock_formats_only(&mut request.info);
    }
    info!(
        connection = %connection.id,
        protocol = %connection.protocol,
        width = request.info.width,
        height = request.info.height,
        overrides = ?request.overrides,
        record,
        "opening tunnel"
    );

    let handshake_started = Instant::now();
    let session = match guacd::connect(
        (guacd_host, guacd_port),
        &connection.protocol,
        &parameters,
        &request.info,
        request.binary,
        Duration::from_secs(handshake_timeout),
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

    // A session that is to be recorded does not start unrecorded.
    let recorder = if record {
        let directory = state.config.recordings.directory(&state.config.server);
        let user = request.user.clone().unwrap_or_default();
        let origin = Origin {
            session: tunnel_id,
            user: &user,
            started_by: None,
        };
        match Recorder::start(&state.db, &directory, &connection, origin).await {
            Ok(recorder) => Some(recorder),
            Err(err) => {
                error!(error = %err, dir = %directory.display(), "cannot start the recording; refusing the session");
                counter!("molcajete_gateway_recording_failures_total", &labels).increment(1);
                let _ = socket.send(close_message(Status::SERVER_ERROR)).await;
                return;
            }
        }
    } else {
        None
    };

    info!(
        guacd_id = %session.id,
        version = %session.version,
        framing = Framing::of(request.binary, &session).as_str(),
        handshake_ms = handshake.as_millis() as u64,
        "tunnel established"
    );
    let user = request.user.clone().unwrap_or_default();
    let registration = state.sessions.register(
        tunnel_id,
        SessionStart {
            user_id: user.id,
            username: user.username,
            connection_id: connection.id.clone(),
            connection_name: connection.name.clone(),
            protocol: connection.protocol.clone(),
            peer: request.peer.clone(),
            guacd: (guacd_host.to_owned(), guacd_port),
            guacd_id: session.id.clone(),
            recording_id: recorder.as_ref().map(|r| r.id().to_owned()),
            page_id: request.page.clone(),
            joined: None,
            client: request.info.clone(),
        },
    );
    let session_id = request.session.as_deref().filter(|_| state.client_reports);
    if let Some(session_id) = session_id {
        state
            .tunnels
            .opened(session_id, &connection.id, tunnel_id, &session.id);
    }
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
    let stats = relay(
        socket,
        session,
        tunnel_id,
        request.binary,
        &counters,
        registration.traffic(),
        Recording {
            whole: recorder.as_ref(),
            on_demand: Some(registration.recording()),
        },
        async { registration.ended().await.reason.status() },
    )
    .await;
    if let Some(recorder) = recorder {
        recorder.finish(&state.db).await;
    }
    if let Some(session_id) = session_id {
        state.tunnels.closed(session_id, tunnel_id);
    }
    sessions::history::record(&state.db, &registration, stats.status).await;
    let ending = registration.ending();
    drop(registration);
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
        ended_for = ending.as_ref().map(|e| e.reason.as_str()),
        ended_by = ending.as_ref().and_then(|e| e.by.as_deref()),
        "tunnel closed"
    );
}

/// Entry point after the WebSocket upgrade for a tunnel that joins a live
/// session (`sessions::share`): `join` is what [`sessions::share::resolve`]
/// found, or the status to close with.
pub async fn serve_join(
    socket: WebSocket,
    state: Arc<AppState>,
    request: TunnelRequest,
    user: crate::db::User,
    join: Result<sessions::share::Join, Status>,
) {
    let tunnel_id = Uuid::new_v4();
    let span = info_span!(
        "tunnel",
        %tunnel_id,
        joins = join.as_ref().map_or(String::new(), |j| j.session.id.to_string()),
        peer = request.peer.as_deref().unwrap_or(""),
        user = %user.username,
    );
    serve_joined(socket, state, request, user, join, tunnel_id)
        .instrument(span)
        .await;
}

async fn serve_joined(
    mut socket: WebSocket,
    state: Arc<AppState>,
    request: TunnelRequest,
    user: crate::db::User,
    join: Result<sessions::share::Join, Status>,
    tunnel_id: Uuid,
) {
    let join = match join {
        Ok(join) => join,
        Err(status) => {
            warn!(%status, "refused to join a session");
            counter!("molcajete_gateway_tunnels_rejected_total", "reason" => "join").increment(1);
            let _ = socket.send(close_message(status)).await;
            return;
        }
    };
    let session = &join.session;
    let labels = [
        ("connection", session.connection_id.clone()),
        ("protocol", session.protocol.clone()),
    ];
    // Only `read-only` is the joining user's to set; guacd takes everything
    // else from the session joined.
    let parameters = BTreeMap::from([(
        "read-only".to_owned(),
        if join.joined.read_only { "true" } else { "" }.to_owned(),
    )]);
    let (host, port) = (session.guacd.0.as_str(), session.guacd.1);
    let handshake_started = Instant::now();
    let guacd = match guacd::connect(
        (host, port),
        &session.guacd_id,
        &parameters,
        &request.info,
        request.binary,
        Duration::from_secs(state.config.guacd.handshake_timeout),
    )
    .await
    {
        Ok(guacd) => guacd,
        Err(err) => {
            warn!(error = %err, "joining at guacd failed");
            counter!("molcajete_gateway_handshake_failures_total", &labels).increment(1);
            let _ = socket.send(close_message(err.status())).await;
            return;
        }
    };
    let handshake = handshake_started.elapsed();
    histogram!("molcajete_gateway_handshake_seconds", &labels).record(handshake);
    info!(
        guacd_id = %guacd.id,
        owner = %session.username,
        via = join.joined.via.as_str(),
        read_only = join.joined.read_only,
        framing = Framing::of(request.binary, &guacd).as_str(),
        handshake_ms = handshake.as_millis() as u64,
        "joined a session"
    );
    let registration = state.sessions.register(
        tunnel_id,
        SessionStart {
            user_id: user.id.clone(),
            username: user.username.clone(),
            connection_id: session.connection_id.clone(),
            connection_name: session.connection_name.clone(),
            protocol: session.protocol.clone(),
            peer: request.peer.clone(),
            guacd: session.guacd.clone(),
            guacd_id: guacd.id.clone(),
            recording_id: None,
            page_id: None,
            joined: Some(join.joined),
            client: request.info.clone(),
        },
    );
    let address = crate::auth::ClientAddress(request.peer.clone().unwrap_or_default());
    sessions::share::record_join(&state, &user, &address, &join).await;
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
    let stats = relay(
        socket,
        guacd,
        tunnel_id,
        request.binary,
        &counters,
        registration.traffic(),
        Recording::default(),
        async { registration.ended().await.reason.status() },
    )
    .await;
    let ending = registration.ending();
    drop(registration);
    active.decrement(1.0);
    info!(
        status = %stats.status,
        duration_s = started.elapsed().as_secs_f64(),
        bytes_to_browser = stats.to_browser,
        bytes_to_guacd = stats.to_guacd,
        ended_for = ending.as_ref().map(|e| e.reason.as_str()),
        "left the session"
    );
}

pub(crate) fn with(
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

/// What a tunnel records.
#[derive(Default, Clone, Copy)]
pub struct Recording<'a> {
    /// The recording of *Record sessions*: what the browser is sent, and
    /// its mouse and keys.
    pub whole: Option<&'a Recorder>,
    /// Where a recording started from the session menu runs, which only
    /// takes the browser's mouse and keys from here
    /// ([`sessions::record`]).
    pub on_demand: Option<&'a sessions::record::Slot>,
}

/// Pumps data in both directions until either side terminates or `ended`
/// resolves with the status to close the tunnel with (an administrator
/// ended the session). `browser_binary` tells whether the browser selected
/// the binary subprotocol. Relayed bytes are counted into `traffic`, and
/// recorded as `recording` says.
#[allow(clippy::too_many_arguments)]
pub async fn relay(
    socket: WebSocket,
    session: GuacdSession,
    tunnel_id: Uuid,
    browser_binary: bool,
    counters: &RelayCounters,
    traffic: &Traffic,
    recording: Recording<'_>,
    ended: impl Future<Output = Status>,
) -> RelayStats {
    let recorder = recording.whole;
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
    let status = if out_tx.send(Message::Text(uuid.into())).await.is_err() {
        Status::SUCCESS
    } else {
        let mut upstream = Upstream {
            rd: guacd_rd,
            out: &out_tx,
            counter: &counters.to_browser,
            total: &traffic.to_browser,
            recorder,
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
            status = browser_to_guacd(&mut ws_rx, guacd_wr, &out_tx, &counters.to_guacd, &traffic.to_guacd, recording) => status,
            status = ended => {
                debug!(%status, "session ended by the gateway");
                status
            }
        }
    };

    let _ = out_tx.send(close_message(status)).await;
    drop(out_tx);
    let _ = writer.await;
    RelayStats {
        status,
        to_browser: traffic.to_browser.load(Ordering::Relaxed),
        to_guacd: traffic.to_guacd.load(Ordering::Relaxed),
    }
}

/// The guacd → browser direction of a tunnel.
struct Upstream<'a> {
    rd: OwnedReadHalf,
    out: &'a mpsc::Sender<Message>,
    counter: &'a Counter,
    total: &'a AtomicU64,
    recorder: Option<&'a Recorder>,
}

impl Upstream<'_> {
    fn record(&self, f: impl FnOnce(&mut recording::Encoder)) {
        if let Some(recorder) = self.recorder {
            recorder.record(f);
        }
    }

    /// Reads more from guacd. `Err` carries the status the tunnel ends with.
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Status> {
        match self.rd.read(buf).await {
            Ok(0) => {
                debug!("guacd closed the connection");
                Err(Status::SUCCESS)
            }
            Ok(n) => {
                self.counter.increment(n as u64);
                self.total.fetch_add(n as u64, Ordering::Relaxed);
                Ok(n)
            }
            Err(err) => {
                debug!(error = %err, "guacd read failed");
                Err(Status::UPSTREAM_ERROR)
            }
        }
    }

    /// Sends a message to the browser; `Err` when the browser is gone.
    /// What was recorded up to here is written first.
    async fn send(&self, message: Message) -> Result<(), Status> {
        if let Some(recorder) = self.recorder {
            recorder.flush().await;
        }
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
                            self.record(|e| e.server_instruction(&raw, now_millis()));
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
                            self.record(|e| e.server_record(record.record(), now_millis()));
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
                            self.record(|e| e.server_instruction(&raw, now_millis()));
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
    total: &AtomicU64,
    recording: Recording<'_>,
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
        // A recording started from the menu may start or stop at any time.
        let on_demand = recording.on_demand.and_then(|slot| slot.recorder());
        let recorders = || recording.whole.into_iter().chain(on_demand.as_deref());
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
                Ok(Some(raw)) => {
                    let now = now_millis();
                    for recorder in recorders() {
                        recorder.record(|e| e.client_instruction(&raw, now));
                    }
                    forward.extend_from_slice(raw.as_bytes());
                }
                Ok(None) => break,
                Err(err) => {
                    warn!(error = %err, "invalid data from browser");
                    return Status::CLIENT_BAD_REQUEST;
                }
            }
        }
        for recorder in recorders() {
            recorder.flush().await;
        }
        if !forward.is_empty() {
            if wr.write_all(&forward).await.is_err() {
                return Status::UPSTREAM_ERROR;
            }
            counter.increment(forward.len() as u64);
            total.fetch_add(forward.len() as u64, Ordering::Relaxed);
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

    #[test]
    fn browser_overrides_only_display_parameters() {
        let req = TunnelRequest::from_query(
            "connection=win&param.dpi=144&param.resize-method=&param.hostname=evil\
             &param.security=tls&param.password=x",
        );
        assert_eq!(
            req.overrides,
            BTreeMap::from([
                ("dpi".to_owned(), "144".to_owned()),
                ("resize-method".to_owned(), String::new()),
            ])
        );
        let connection = BTreeMap::from([
            ("hostname".to_owned(), "win".to_owned()),
            ("dpi".to_owned(), "96".to_owned()),
            ("resize-method".to_owned(), "display-update".to_owned()),
        ]);
        assert_eq!(
            req.parameters(&connection),
            BTreeMap::from([
                ("hostname".to_owned(), "win".to_owned()),
                ("dpi".to_owned(), "144".to_owned()),
            ])
        );

        // Values the daemons would not understand are ignored.
        let req = TunnelRequest::from_query(
            "param.dpi=auto&param.dpi=9999&param.resize-method=reconnect",
        );
        assert!(req.overrides.is_empty());
        assert_eq!(req.parameters(&connection), connection);
        // Empty: follow the client.
        let req = TunnelRequest::from_query("param.dpi=");
        assert!(!req.parameters(&connection).contains_key("dpi"));

        // The text size of a terminal, within what can be read.
        let req = TunnelRequest::from_query("param.font-size=9");
        assert_eq!(
            req.parameters(&connection)
                .get("font-size")
                .map(String::as_str),
            Some("9")
        );
        for refused in ["0", "400", "NaN", "big"] {
            let req = TunnelRequest::from_query(&format!("param.font-size={refused}"));
            assert!(req.overrides.is_empty(), "{refused}");
        }
    }
}

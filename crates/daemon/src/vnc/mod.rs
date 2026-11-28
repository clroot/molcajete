//! VNC support: bridges an RFB server to a Guacamole client.

pub mod rfb;

use std::time::Duration;

use molcajete_protocol::Status;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::display::{Display, ImageFormats, Pacing, VIDEO_MODE, VideoMode};
use crate::handshake::{ClientInfo, Params};
use crate::io::{GuacReader, GuacWriter};
use crate::jump::Chain;
use crate::send_error;
use crate::session::{self, ClientEvent, Input, InputDecoder};

/// Connection parameters, named like guacd's VNC plugin so the same
/// configuration works with both daemons.
pub const ARGS: &[&str] = &[
    "hostname",
    "port",
    "read-only",
    "encodings",
    "username",
    "password",
    "swap-red-blue",
    "color-depth",
    "cursor",
    "autoretry",
    "clipboard-encoding",
    "disable-copy",
    "disable-paste",
    "disable-display-resize",
    "force-lossless",
];

/// Frames the client may lag behind before we stop asking for updates.
const MAX_UNACKED_FRAMES: u32 = 2;

fn encoding_number(name: &str) -> Option<i32> {
    Some(match name {
        "raw" => 0,
        "copyrect" => 1,
        "rre" => 2,
        "hextile" => 5,
        "tight" => 7,
        "zrle" => 16,
        _ => return None,
    })
}

struct Settings {
    read_only: bool,
    disable_copy: bool,
    disable_paste: bool,
    disable_resize: bool,
}

impl Settings {
    /// Whether text from the browser's clipboard reaches the server. A
    /// read-only connection takes nothing from the browser, as in guacd.
    fn accepts_paste(&self) -> bool {
        !self.read_only && !self.disable_paste
    }
}

pub async fn run(
    reader: GuacReader,
    mut writer: GuacWriter,
    info: ClientInfo,
    params: Params,
) -> anyhow::Result<()> {
    let host = params.get("hostname").unwrap_or("localhost").to_owned();
    let port = params.parse::<u16>("port").unwrap_or(5900);
    let encodings = params.get("encodings").map(|list| {
        let mut numbers: Vec<i32> = list
            .split_whitespace()
            .filter_map(encoding_number)
            .collect();
        // Pseudo-encodings are always useful.
        numbers.extend([-239, -223, -308, -224]);
        numbers
    });
    let options = rfb::ConnectOptions {
        host: host.clone(),
        port,
        password: params.get("password").map(str::to_owned),
        shared: true,
        encodings,
    };
    let settings = Settings {
        read_only: params.bool("read-only"),
        disable_copy: params.bool("disable-copy"),
        disable_paste: params.bool("disable-paste"),
        disable_resize: params.bool("disable-display-resize"),
    };

    let mut jump = match Chain::from_params(&params) {
        Ok(jump) => jump,
        Err(err) => {
            send_error(&mut writer, &err, Status::CLIENT_BAD_REQUEST).await;
            return Ok(());
        }
    };

    let (mut inputs, client_task) = session::spawn_reader(reader);
    let mut decoder = InputDecoder::default();
    let result = async {
        let missing = jump.missing();
        if !missing.is_empty() && info.version.supports_required() {
            let asked = session::ask_required(&mut inputs, &mut decoder, &mut writer, &missing);
            let Some(answers) = asked.await? else {
                return Ok(());
            };
            for (name, value) in answers {
                jump.set(&name, value);
            }
        }
        run_session(
            &jump,
            options,
            settings,
            &info,
            &params,
            inputs,
            decoder,
            &mut writer,
        )
        .await
    }
    .await;
    client_task.abort();
    result
}

/// Connects (through the jump hosts, if any) and bridges the session.
#[allow(clippy::too_many_arguments)]
async fn run_session(
    jump: &Chain,
    options: rfb::ConnectOptions,
    settings: Settings,
    info: &ClientInfo,
    params: &Params,
    mut inputs: mpsc::Receiver<Input>,
    mut decoder: InputDecoder,
    writer: &mut GuacWriter,
) -> anyhow::Result<()> {
    let (host, port) = (options.host.clone(), options.port);
    let upstream = match jump.connect(&host, port).await {
        Ok(upstream) => upstream,
        Err(err) => {
            warn!(%host, port, error = %err, "VNC connection failed");
            send_error(
                writer,
                &format!("Unable to connect to VNC server: {err}"),
                err.status(),
            )
            .await;
            return Ok(());
        }
    };
    let (mut rfb_reader, mut rfb_writer, server) = match rfb::connect(upstream, &options).await {
        Ok(v) => v,
        Err(err) => {
            warn!(%host, port, error = %err, "VNC connection failed");
            let status = match err {
                rfb::RfbError::AuthFailed(_) => Status::CLIENT_UNAUTHORIZED,
                rfb::RfbError::Io(_) => Status::UPSTREAM_NOT_FOUND,
                _ => Status::UPSTREAM_ERROR,
            };
            send_error(
                writer,
                &format!("Unable to connect to VNC server: {err}"),
                status,
            )
            .await;
            return Ok(());
        }
    };
    info!(%host, port, width = server.width, height = server.height, name = %server.name, "VNC connected");

    let mut display = Display::new(
        ImageFormats::from_mimetypes(&info.image),
        params.bool("force-lossless"),
    )
    .with_protocol("vnc")
    .with_pacing(Pacing::DESKTOP);
    session::enable_video(&mut display, info);
    crate::resume::establish(writer);
    session::send_name(writer.buffer(), &server.name);
    display.resize(server.width as u32, server.height as u32, writer.buffer());
    writer.flush().await?;
    rfb_writer
        .request_update(false, 0, 0, server.width, server.height)
        .await?;

    // Server events and client instructions are read by dedicated tasks.
    let (event_tx, mut events) = mpsc::channel(512);
    let rfb_task = tokio::spawn(async move {
        loop {
            let event = rfb_reader.next_event().await;
            let failed = event.is_err();
            if event_tx.send(event).await.is_err() || failed {
                break;
            }
        }
    });
    let mut requests = Requests::default();
    let mut frame_pending = false;
    // What the client holds down, to let go of when it goes away.
    let mut held = Held::default();
    let frame_timer = tokio::time::sleep(Duration::from_secs(3600));
    tokio::pin!(frame_timer);
    let mut keepalive = tokio::time::interval(Duration::from_secs(5));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let result: anyhow::Result<()> = loop {
        // Housekeeping without new updates: ending an idle video, refining
        // lossy areas that calmed down.
        let idle_at = display.idle_deadline();
        let idle_wait = tokio::time::sleep_until(idle_at.map_or_else(
            || tokio::time::Instant::now() + Duration::from_secs(3600),
            Into::into,
        ));
        tokio::select! {
            event = events.recv() => {
                let event = match event {
                    Some(Ok(event)) => event,
                    Some(Err(err)) => {
                        warn!(error = %err, "VNC connection lost");
                        send_error(writer, "VNC connection lost", Status::UPSTREAM_ERROR).await;
                        break Ok(());
                    }
                    None => break Ok(()),
                };
                match event {
                    rfb::RfbEvent::Rect { x, y, width, height, rgba } => {
                        display.put(x as u32, y as u32, width as u32, height as u32, &rgba);
                    }
                    rfb::RfbEvent::Fill { x, y, width, height, rgba } => {
                        display.fill(x as u32, y as u32, width as u32, height as u32, rgba);
                    }
                    rfb::RfbEvent::CopyRect { src_x, src_y, x, y, width, height } => {
                        display.copy(
                            src_x as u32, src_y as u32, x as u32, y as u32,
                            width as u32, height as u32, writer.buffer(),
                        );
                    }
                    rfb::RfbEvent::Resize { width, height } => {
                        display.resize(width as u32, height as u32, writer.buffer());
                    }
                    rfb::RfbEvent::Cursor { hotspot_x, hotspot_y, width, height, rgba } => {
                        display.set_cursor(
                            hotspot_x as u32, hotspot_y as u32,
                            width as u32, height as u32, &rgba, writer.buffer(),
                        );
                    }
                    rfb::RfbEvent::UpdateEnd => {
                        // Send now if the frame is due, otherwise keep
                        // accumulating changes until it is (frame pacing).
                        frame_pending = true;
                        let due = display.next_frame_at();
                        if tokio::time::Instant::now() >= due.into() {
                            send_frame(&mut display, writer).await?;
                            frame_pending = false;
                        } else {
                            frame_timer.as_mut().reset(due.into());
                        }
                        let ask = requests.update_ended(
                            looked_at(&display),
                            display.unacked_frames() <= MAX_UNACKED_FRAMES,
                        );
                        request_update(&mut rfb_writer, &display, ask).await?;
                    }
                    rfb::RfbEvent::Bell => {}
                    rfb::RfbEvent::ClipboardText(text) => {
                        if !settings.disable_copy {
                            session::send_clipboard(writer.buffer(), &text);
                            writer.flush().await?;
                        }
                    }
                }
            }
            ins = inputs.recv() => {
                let Some(ins) = ins else {
                    debug!("client disconnected");
                    break Ok(());
                };
                match decoder.decode(&ins, writer.buffer()) {
                    Some(ClientEvent::Mouse { x, y, mask }) if !settings.read_only => {
                        let x = x.min(u16::MAX as i32) as u16;
                        let y = y.min(u16::MAX as i32) as u16;
                        held.pointer = (x, y, mask as u8);
                        rfb_writer.pointer(x, y, mask as u8).await?;
                    }
                    Some(ClientEvent::Key { keysym, pressed }) if !settings.read_only => {
                        if pressed {
                            held.keys.insert(keysym);
                        } else {
                            held.keys.remove(&keysym);
                        }
                        rfb_writer.key(keysym, pressed).await?;
                    }
                    Some(ClientEvent::Size { width, height }) if !settings.disable_resize => {
                        let w = width.min(u16::MAX as u32) as u16;
                        let h = height.min(u16::MAX as u32) as u16;
                        let _ = rfb_writer.set_desktop_size(w, h).await?;
                    }
                    Some(ClientEvent::Ack { stream, status }) => {
                        display.stream_ack(stream, status, writer.buffer());
                    }
                    Some(ClientEvent::Sync { timestamp, received }) => {
                        display.ack(timestamp, received);
                        // A delivered frame may let the pending one go
                        // earlier than the timer was set for.
                        if frame_pending {
                            frame_timer.as_mut().reset(display.next_frame_at().into());
                        }
                        // A user who joins wakes the session this way too.
                        let ask = requests.retry(
                            looked_at(&display),
                            display.unacked_frames() <= MAX_UNACKED_FRAMES,
                        );
                        request_update(&mut rfb_writer, &display, ask).await?;
                    }
                    Some(ClientEvent::Clipboard(text)) if settings.accepts_paste() => {
                        rfb_writer.cut_text(&text).await?;
                    }
                    Some(ClientEvent::Argv { name, value }) if name == VIDEO_MODE => {
                        display.set_video_mode(VideoMode::from_param(Some(&value)), writer.buffer());
                    }
                    Some(ClientEvent::Disconnect) => break Ok(()),
                    Some(ClientEvent::Detached) => {
                        for keysym in std::mem::take(&mut held.keys) {
                            rfb_writer.key(keysym, false).await?;
                        }
                        let (x, y, mask) = held.pointer;
                        if mask != 0 {
                            held.pointer.2 = 0;
                            rfb_writer.pointer(x, y, 0).await?;
                        }
                        display.detach();
                    }
                    Some(ClientEvent::Attached(attach))
                        if crate::resume::take_over(writer, &attach).await =>
                    {
                        let info = attach.info();
                        session::send_name(writer.buffer(), &server.name);
                        session::attach_display(&mut display, info, writer.buffer());
                        if !settings.disable_resize {
                            let w = info.width.min(u16::MAX as u32) as u16;
                            let h = info.height.min(u16::MAX as u32) as u16;
                            let _ = rfb_writer.set_desktop_size(w, h).await?;
                        }
                        frame_pending = true;
                        frame_timer.as_mut().reset(tokio::time::Instant::now());
                        let ask = requests.retry(looked_at(&display), true);
                        request_update(&mut rfb_writer, &display, ask).await?;
                    }
                    _ => {}
                }
                if writer.pending() > 0 {
                    writer.flush().await?;
                }
            }
            _ = &mut frame_timer, if frame_pending => {
                send_frame(&mut display, writer).await?;
                frame_pending = false;
            }
            _ = idle_wait, if idle_at.is_some() && !frame_pending => {
                send_frame(&mut display, writer).await?;
            }
            _ = keepalive.tick() => {
                // A stalled client must not stall the session forever.
                let ask = requests.retry(looked_at(&display), true);
                request_update(&mut rfb_writer, &display, ask).await?;
                writer.push("3.nop;");
                writer.flush().await?;
            }
        }
    };

    writer.push("10.disconnect;");
    let _ = writer.flush().await;
    rfb_task.abort();
    result
}

/// Keys and buttons the client holds down.
#[derive(Default)]
struct Held {
    keys: std::collections::HashSet<u32>,
    /// Position and button mask last sent.
    pointer: (u16, u16, u8),
}

/// Encodes pending changes and ends the frame (if there was anything to send).
async fn send_frame(display: &mut Display, writer: &mut GuacWriter) -> std::io::Result<()> {
    let stats = tokio::task::block_in_place(|| display.flush(writer.buffer()));
    // Copies may already be queued from before the flush.
    if writer.pending() > 0 {
        display.sync(writer.buffer());
    }
    writer.flush().await?;
    let lag_ms = display.processing_lag().as_millis() as u64;
    debug!(
        images = stats.images,
        fills = stats.fills,
        copies = stats.copies,
        bytes = stats.bytes,
        lag_ms,
        "frame sent"
    );
    Ok(())
}

/// Whether anybody sees the screen: a client attached or a user who joined.
fn looked_at(display: &Display) -> bool {
    !display.is_detached() || display.has_viewers()
}

/// What to ask the server for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// What changed since the last update.
    Changes,
    /// The whole screen: what the session has is out of date.
    Whole,
}

/// When the session asks the server for the next update.
///
/// After an update it asks at once, unless the client is too far behind
/// (the request is owed until it catches up) or nobody sees the screen. A
/// held session nobody looks at does not ask at all, so the server sends
/// nothing and nothing is decoded for nobody (GH-176); once somebody looks
/// again it asks for the whole screen, since the framebuffer missed what
/// changed meanwhile.
#[derive(Debug, Default)]
struct Requests {
    /// A request held back until the client catches up.
    owed: bool,
    /// Requests stopped while nobody looked.
    paused: bool,
}

impl Requests {
    /// The server ended an update. `may_send`: the client is not too far
    /// behind.
    fn update_ended(&mut self, looked_at: bool, may_send: bool) -> Option<Ask> {
        if !looked_at {
            self.pause();
            None
        } else if may_send {
            Some(Ask::Changes)
        } else {
            self.owed = true;
            None
        }
    }

    /// The client caught up or attached, a user joined, or the keep-alive
    /// ticked: what was held back, if it may go now.
    fn retry(&mut self, looked_at: bool, may_send: bool) -> Option<Ask> {
        if !(self.owed || self.paused) {
            return None;
        }
        if !looked_at {
            self.pause();
            return None;
        }
        if !may_send {
            return None;
        }
        self.owed = false;
        if std::mem::take(&mut self.paused) {
            info!(paused = false, "VNC screen updates");
            Some(Ask::Whole)
        } else {
            Some(Ask::Changes)
        }
    }

    fn pause(&mut self) {
        self.owed = false;
        if !self.paused {
            self.paused = true;
            info!(paused = true, "VNC screen updates");
        }
    }
}

async fn request_update(
    writer: &mut rfb::RfbWriter,
    display: &Display,
    ask: Option<Ask>,
) -> Result<(), rfb::RfbError> {
    let Some(ask) = ask else {
        return Ok(());
    };
    writer
        .request_update(
            ask == Ask::Changes,
            0,
            0,
            display.width().min(u16::MAX as u32) as u16,
            display.height().min(u16::MAX as u32) as u16,
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::{Ask, Requests, Settings};

    fn settings(read_only: bool, disable_paste: bool) -> Settings {
        Settings {
            read_only,
            disable_copy: false,
            disable_paste,
            disable_resize: false,
        }
    }

    #[test]
    fn read_only_takes_no_clipboard() {
        assert!(settings(false, false).accepts_paste());
        assert!(!settings(false, true).accepts_paste());
        assert!(!settings(true, false).accepts_paste());
    }

    #[test]
    fn updates_follow_the_client() {
        let mut requests = Requests::default();
        assert_eq!(requests.update_ended(true, true), Some(Ask::Changes));
        // Too far behind: owed until the client catches up.
        assert_eq!(requests.update_ended(true, false), None);
        assert_eq!(requests.retry(true, false), None);
        assert_eq!(requests.retry(true, true), Some(Ask::Changes));
        assert_eq!(requests.retry(true, true), None, "asked once");
    }

    #[test]
    fn nobody_looking_stops_updates_until_the_whole_screen_is_asked_for() {
        let mut requests = Requests::default();
        assert_eq!(requests.update_ended(false, true), None);
        // The keep-alive while nobody looks.
        assert_eq!(requests.retry(false, true), None);
        // A client attaches or a user joins.
        assert_eq!(requests.retry(true, true), Some(Ask::Whole));
        assert_eq!(requests.update_ended(true, true), Some(Ask::Changes));
    }

    #[test]
    fn a_request_owed_when_the_client_leaves_waits_for_the_next() {
        let mut requests = Requests::default();
        assert_eq!(requests.update_ended(true, false), None);
        assert_eq!(requests.retry(false, true), None);
        assert_eq!(requests.retry(true, true), Some(Ask::Whole));
    }
}

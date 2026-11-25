//! The active session: server graphics, pointer and clipboard go to the
//! Guacamole client; client input goes to the server.

use std::time::{Duration, Instant};

use ironrdp_cliprdr::CliprdrClient;
use ironrdp_connector::ConnectionResult;
use ironrdp_connector::connection_activation::{
    ConnectionActivationFactory, ConnectionActivationState,
};
use ironrdp_core::WriteBuf;
use ironrdp_displaycontrol::pdu::MonitorLayoutEntry;
use ironrdp_dvc::DrdynvcClient;
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_pdu::Action;
use ironrdp_pdu::input::InputEventPdu;
use ironrdp_pdu::input::fast_path::FastPathInputEvent;
use ironrdp_pdu::rdp::capability_sets::InputFlags;
use ironrdp_pdu::rdp::headers::ShareDataPdu;
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::{FramedWrite as _, TokioFramed, single_sequence_step_read, split_tokio_framed};
use molcajete_protocol::Status;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use tokio::time::sleep_until;
use tracing::{debug, info, warn};

use super::audio::AudioOutput;
use super::clipboard::{self, GuacClipboard};
use super::connect::{RdpFramed, RdpStream};
use super::display_control::{self, DisplayControlStatus};
use super::graphics::{Area, Cursor, extract_rgba};
use super::keyboard::Keyboard;
use super::mouse::Mouse;
use super::pacing::FramePacer;
use super::settings::{self, ResizeMethod, Settings};
use super::slow_path;
use crate::display::{Display, ImageFormats, Pacing};
use crate::handshake::ClientInfo;
use crate::io::GuacWriter;
use crate::send_error;
use crate::session::{self, ClientEvent, Input, InputDecoder};

/// Frames the client may lag behind before frames are held back.
const MAX_UNACKED_FRAMES: u32 = 2;
/// Quiet period before a client size change is applied to the session.
const RESIZE_DELAY: Duration = Duration::from_millis(500);
/// Retry interval while the Display Control channel is not open yet.
const RESIZE_RETRY: Duration = Duration::from_secs(1);
const KEEPALIVE: Duration = Duration::from_secs(5);

/// Client input that arrived while the connection was being established.
#[derive(Debug, Default)]
pub struct EarlyInput {
    pub clipboard: Option<String>,
    pub size: Option<(u32, u32)>,
}

enum Flow {
    Continue,
    End,
}

struct PendingResize {
    width: u32,
    height: u32,
    due: tokio::time::Instant,
}

pub struct Session<'a> {
    settings: &'a Settings,
    writer: &'a mut GuacWriter,
    stage: ActiveStage,
    image: DecodedImage,
    rdp_reader: TokioFramed<ReadHalf<RdpStream>>,
    rdp_writer: TokioFramed<WriteHalf<RdpStream>>,
    activation: ConnectionActivationFactory,
    display: Display,
    pacer: FramePacer,
    keyboard: Keyboard,
    mouse: Mouse,
    fastpath_input: bool,
    refresh_rect_support: bool,
    suppress_output_support: bool,
    resize: Option<PendingResize>,
    /// Starts as the connection's `resize-method`; the client can change it
    /// (`argv`).
    resize_method: ResizeMethod,
    /// Requested DPI, from the connection or the client (`argv`).
    dpi: u32,
    /// DPI the client announced in the handshake, for "follow the client".
    client_dpi: u32,
    /// Desktop scale factor the server was last given.
    scale: u32,
    display_control: DisplayControlStatus,
    audio: Option<AudioOutput>,
}

impl<'a> Session<'a> {
    pub fn new(
        settings: &'a Settings,
        info: &ClientInfo,
        writer: &'a mut GuacWriter,
        result: ConnectionResult,
        framed: RdpFramed,
        audio: Option<AudioOutput>,
    ) -> Self {
        let (rdp_reader, rdp_writer) = split_tokio_framed(framed);
        let size = result.desktop_size;
        debug!(input_flags = ?result.input_flags, "server input capabilities");
        let dynamic_channels = result
            .static_channels
            .get_channel_id_by_type::<DrdynvcClient>()
            .is_some();
        let stage = ActiveStageBuilder {
            static_channels: result.static_channels,
            user_channel_id: result.user_channel_id,
            io_channel_id: result.io_channel_id,
            message_channel_id: result.message_channel_id,
            share_id: result.share_id,
            compression_type: result.compression_type,
            enable_server_pointer: result.enable_server_pointer,
            pointer_software_rendering: result.pointer_software_rendering,
        }
        .build();
        Self {
            settings,
            writer,
            stage,
            image: DecodedImage::new(PixelFormat::RgbA32, size.width, size.height),
            rdp_reader,
            rdp_writer,
            activation: result.activation_factory,
            display: {
                let mut display = Display::new(
                    ImageFormats::from_mimetypes(&info.image),
                    settings.force_lossless,
                )
                .with_protocol("rdp")
                .with_pacing(Pacing::DESKTOP);
                session::enable_video(&mut display, info);
                display
            },
            pacer: FramePacer::new(Instant::now()),
            keyboard: Keyboard::new(
                settings.layout,
                result.input_flags.contains(InputFlags::UNICODE),
            ),
            mouse: Mouse::default(),
            fastpath_input: supports_fastpath(result.input_flags),
            refresh_rect_support: result.refresh_rect_support,
            suppress_output_support: result.suppress_output_support,
            resize: None,
            resize_method: settings.resize_method,
            dpi: settings.dpi,
            client_dpi: info.dpi,
            scale: settings.desktop_scale_factor(),
            display_control: DisplayControlStatus::new(dynamic_channels, Instant::now()),
            audio,
        }
    }

    /// Runs the session until either side ends it. Failures on the RDP side
    /// are reported to the client as an upstream error.
    pub async fn run(
        mut self,
        inputs: &mut mpsc::Receiver<Input>,
        decoder: &mut InputDecoder,
        early: EarlyInput,
    ) {
        if let Err(err) = self.event_loop(inputs, decoder, early).await {
            warn!(error = %err, "RDP session ended with an error");
            send_error(self.writer, "RDP connection error", Status::UPSTREAM_ERROR).await;
        }
    }

    async fn event_loop(
        &mut self,
        inputs: &mut mpsc::Receiver<Input>,
        decoder: &mut InputDecoder,
        early: EarlyInput,
    ) -> anyhow::Result<()> {
        info!(
            width = self.image.width(),
            height = self.image.height(),
            fastpath_input = self.fastpath_input,
            "RDP session active"
        );
        session::send_name(self.writer.buffer(), &self.settings.hostname);
        self.display.resize(
            u32::from(self.image.width()),
            u32::from(self.image.height()),
            self.writer.buffer(),
        );
        self.set_cursor(&Cursor::arrow());
        self.update_display_control();
        self.writer.flush().await?;
        if let Some(text) = early.clipboard {
            self.client_clipboard(text).await?;
        }
        if let Some((width, height)) = early.size {
            self.request_resize(width, height);
        }

        let mut keepalive = tokio::time::interval(KEEPALIVE);
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let can_flush =
                self.pacer.is_dirty() && self.display.unacked_frames() <= MAX_UNACKED_FRAMES;
            let flush_at =
                tokio::time::Instant::from_std(self.pacer.earliest(self.display.next_frame_at()));
            let resize_at = self.resize.as_ref().map(|r| r.due);
            // Housekeeping without new updates: ending an idle video,
            // refining lossy areas that calmed down.
            let idle_at = self
                .display
                .idle_deadline()
                .map(tokio::time::Instant::from_std);
            let flow = tokio::select! {
                biased;
                ins = inputs.recv() => match ins {
                    Some(ins) => self.client_instruction(&ins, decoder).await?,
                    None => {
                        debug!("client disconnected");
                        Flow::End
                    }
                },
                pdu = self.rdp_reader.read_pdu() => match pdu {
                    Ok((action, payload)) => self.server_pdu(action, &payload).await?,
                    Err(err) => {
                        if err.kind() == std::io::ErrorKind::UnexpectedEof {
                            info!("RDP server closed the connection");
                        } else {
                            warn!(error = %err, "RDP connection lost");
                            send_error(self.writer, "RDP connection lost", Status::UPSTREAM_ERROR)
                                .await;
                        }
                        Flow::End
                    }
                },
                _ = sleep_until(flush_at), if can_flush => {
                    self.send_frame().await?;
                    Flow::Continue
                }
                _ = sleep_until(resize_at.unwrap_or_else(tokio::time::Instant::now)),
                    if resize_at.is_some() => {
                    self.apply_resize().await?;
                    Flow::Continue
                }
                _ = sleep_until(idle_at.unwrap_or_else(tokio::time::Instant::now)),
                    if idle_at.is_some() && !can_flush => {
                    self.send_frame().await?;
                    Flow::Continue
                }
                _ = keepalive.tick() => {
                    self.update_display_control();
                    // A stalled client must not hold back changes forever.
                    if self.pacer.is_dirty() {
                        self.send_frame().await?;
                    }
                    self.writer.push("3.nop;");
                    Flow::Continue
                }
            };
            if self.writer.pending() > 0 {
                self.writer.flush().await?;
            }
            if matches!(flow, Flow::End) {
                return Ok(());
            }
        }
    }

    /// Encodes pending display changes and ends the frame.
    async fn send_frame(&mut self) -> std::io::Result<()> {
        let display = &mut self.display;
        let out = self.writer.buffer();
        let stats = tokio::task::block_in_place(|| display.flush(out));
        if !out.is_empty() {
            display.sync(out);
        }
        self.writer.flush().await?;
        self.pacer.frame_sent(Instant::now());
        debug!(
            images = stats.images,
            fills = stats.fills,
            bytes = stats.bytes,
            "frame sent"
        );
        Ok(())
    }

    fn set_cursor(&mut self, cursor: &Cursor) {
        self.display.set_cursor(
            cursor.hotspot_x,
            cursor.hotspot_y,
            cursor.width,
            cursor.height,
            &cursor.rgba,
            self.writer.buffer(),
        );
        // Cursor instructions take effect with the next frame.
        self.pacer.mark_dirty(Instant::now());
    }

    async fn write_rdp(&mut self, frame: &[u8]) -> std::io::Result<()> {
        if frame.is_empty() {
            return Ok(());
        }
        self.rdp_writer.write_all(frame).await
    }

    async fn server_pdu(&mut self, action: Action, payload: &[u8]) -> anyhow::Result<Flow> {
        let received_at = self.rdp_reader.last_read_at();
        let outputs = match self
            .stage
            .process(&mut self.image, action, payload, received_at)
        {
            Ok(outputs) => outputs,
            Err(err) => {
                warn!(error = %err, "failed to process RDP data");
                send_error(self.writer, "RDP protocol error", Status::UPSTREAM_ERROR).await;
                return Ok(Flow::End);
            }
        };
        let flow = self.stage_outputs(outputs).await?;
        if let Flow::End = flow {
            return Ok(flow);
        }
        if self.stage.take_bitmap_recovery_request() {
            self.request_redraw().await?;
        }
        self.pump_clipboard().await?;
        // Sound goes out with this iteration's flush, regardless of frames.
        if let Some(audio) = &mut self.audio {
            audio.pump(self.writer.buffer());
        }
        let now = Instant::now();
        if self.pacer.overdue(now, self.display.next_frame_at())
            && self.display.unacked_frames() <= MAX_UNACKED_FRAMES
        {
            self.send_frame().await?;
        }
        Ok(flow)
    }

    async fn stage_outputs(&mut self, outputs: Vec<ActiveStageOutput>) -> anyhow::Result<Flow> {
        for output in outputs {
            match output {
                ActiveStageOutput::ResponseFrame(frame) => self.write_rdp(&frame).await?,
                ActiveStageOutput::GraphicsUpdate(rect) => {
                    let (width, height) = (
                        u32::from(self.image.width()),
                        u32::from(self.image.height()),
                    );
                    if let Some(area) = Area::clipped(&rect, width, height) {
                        let rgba = extract_rgba(
                            self.image.data(),
                            self.image.stride(),
                            self.image.pixel_format(),
                            area,
                        );
                        self.display
                            .put(area.x, area.y, area.width, area.height, &rgba);
                        self.pacer.mark_dirty(Instant::now());
                    }
                }
                ActiveStageOutput::PointerDefault => self.set_cursor(&Cursor::arrow()),
                ActiveStageOutput::PointerHidden => self.set_cursor(&Cursor::hidden()),
                ActiveStageOutput::PointerBitmap(pointer) => match Cursor::from_pointer(&pointer) {
                    Some(cursor) => self.set_cursor(&cursor),
                    None => debug!("ignoring malformed pointer bitmap"),
                },
                ActiveStageOutput::DeactivateAll => {
                    if let Flow::End = self.reactivate().await? {
                        return Ok(Flow::End);
                    }
                }
                ActiveStageOutput::Terminate(reason) => {
                    info!(%reason, "RDP session terminated by the server");
                    return Ok(Flow::End);
                }
                ActiveStageOutput::ServerRedirection(_) => {
                    send_error(
                        self.writer,
                        "The RDP server redirected the session, which is not supported",
                        Status::UPSTREAM_ERROR,
                    )
                    .await;
                    return Ok(Flow::End);
                }
                ActiveStageOutput::LogonErrors(errors) => debug!(?errors, "logon notification"),
                _ => {}
            }
        }
        Ok(Flow::Continue)
    }

    /// Runs the Deactivation-Reactivation Sequence (e.g. after a resize).
    async fn reactivate(&mut self) -> anyhow::Result<Flow> {
        debug!("deactivation-reactivation sequence");
        let mut sequence = self.activation.create();
        let mut buf = WriteBuf::new();
        loop {
            let written =
                single_sequence_step_read(&mut self.rdp_reader, &mut sequence, &mut buf).await?;
            if written.size().is_some() {
                self.rdp_writer.write_all(buf.filled()).await?;
            }
            match sequence.connection_activation_state() {
                ConnectionActivationState::Finalized {
                    desktop_size,
                    share_id,
                    input_flags,
                    static_channel_chunk_size,
                    enable_server_pointer,
                    pointer_software_rendering,
                    refresh_rect_support,
                    suppress_output_support,
                    ..
                } => {
                    if !self.stage.reactivate(
                        sequence.io_channel_id(),
                        sequence.user_channel_id(),
                        share_id,
                        enable_server_pointer,
                        pointer_software_rendering,
                        static_channel_chunk_size,
                    ) {
                        anyhow::bail!("server sent an invalid virtual channel chunk size");
                    }
                    info!(
                        width = desktop_size.width,
                        height = desktop_size.height,
                        "RDP session reactivated"
                    );
                    self.image = DecodedImage::new(
                        PixelFormat::RgbA32,
                        desktop_size.width,
                        desktop_size.height,
                    );
                    self.display.resize(
                        u32::from(desktop_size.width),
                        u32::from(desktop_size.height),
                        self.writer.buffer(),
                    );
                    self.pacer.mark_dirty(Instant::now());
                    self.fastpath_input = supports_fastpath(input_flags);
                    self.keyboard
                        .set_unicode_support(input_flags.contains(InputFlags::UNICODE));
                    self.refresh_rect_support = refresh_rect_support;
                    self.suppress_output_support = suppress_output_support;
                    return Ok(Flow::Continue);
                }
                ConnectionActivationState::Redirected { .. } => {
                    send_error(
                        self.writer,
                        "The RDP server redirected the session, which is not supported",
                        Status::UPSTREAM_ERROR,
                    )
                    .await;
                    return Ok(Flow::End);
                }
                _ => {}
            }
        }
    }

    /// Asks the server to repaint everything after a bitmap was dropped.
    async fn request_redraw(&mut self) -> anyhow::Result<()> {
        let frames = self.stage.request_full_redraw(
            self.image.width(),
            self.image.height(),
            self.refresh_rect_support,
            self.suppress_output_support,
        )?;
        for frame in frames {
            self.write_rdp(&frame).await?;
        }
        Ok(())
    }

    async fn client_instruction(
        &mut self,
        ins: &Input,
        decoder: &mut InputDecoder,
    ) -> anyhow::Result<Flow> {
        match decoder.decode(ins, self.writer.buffer()) {
            Some(event) => self.client_event(event).await,
            None => Ok(Flow::Continue),
        }
    }

    async fn client_event(&mut self, event: ClientEvent) -> anyhow::Result<Flow> {
        let read_only = self.settings.read_only;
        match event {
            ClientEvent::Mouse { x, y, mask } if !read_only => {
                let max_x = i32::from(self.image.width().saturating_sub(1));
                let max_y = i32::from(self.image.height().saturating_sub(1));
                // Both values are clamped into the u16 desktop range.
                let (x, y) = (x.clamp(0, max_x) as u16, y.clamp(0, max_y) as u16);
                let events = self
                    .mouse
                    .update(x, y, mask)
                    .into_iter()
                    .map(FastPathInputEvent::MouseEvent)
                    .collect();
                self.send_input(events).await?;
            }
            ClientEvent::Key { keysym, pressed } if !read_only => {
                let events = self.keyboard.key(keysym, pressed);
                self.send_input(events).await?;
            }
            ClientEvent::Size { width, height } => self.request_resize(width, height),
            ClientEvent::Argv { name, value } => self.client_parameter(&name, &value),
            ClientEvent::Clipboard(text) if !self.settings.disable_paste => {
                self.client_clipboard(text).await?;
            }
            ClientEvent::Sync {
                timestamp,
                received,
            } => self.display.ack(timestamp, received),
            ClientEvent::Ack { stream, status } => {
                let audio = self.audio.as_mut().is_some_and(|a| a.ack(stream, status));
                if !audio {
                    self.display
                        .stream_ack(stream, status, self.writer.buffer());
                }
            }
            ClientEvent::Disconnect => return Ok(Flow::End),
            _ => {}
        }
        Ok(Flow::Continue)
    }

    async fn send_input(&mut self, events: Vec<FastPathInputEvent>) -> anyhow::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        if self.fastpath_input {
            let outputs = self
                .stage
                .process_fastpath_input(&mut self.image, &events)?;
            self.stage_outputs(outputs).await?;
        } else {
            let pdu = InputEventPdu(events.iter().filter_map(slow_path::input_event).collect());
            let mut buf = WriteBuf::new();
            self.stage
                .encode_static(&mut buf, ShareDataPdu::Input(pdu))?;
            self.write_rdp(buf.filled()).await?;
        }
        Ok(())
    }

    fn clipboard_backend(&mut self) -> Option<&mut GuacClipboard> {
        self.stage
            .get_svc_processor_mut::<CliprdrClient>()?
            .downcast_backend_mut::<GuacClipboard>()
    }

    async fn client_clipboard(&mut self, text: String) -> anyhow::Result<()> {
        let text = self.settings.line_endings.apply(&text);
        if let Some(backend) = self.clipboard_backend() {
            backend.set_local_text(text);
        }
        self.pump_clipboard().await
    }

    /// Carries out what the clipboard backend asked for.
    async fn pump_clipboard(&mut self) -> anyhow::Result<()> {
        let Some(cliprdr) = self.stage.get_svc_processor_mut::<CliprdrClient>() else {
            return Ok(());
        };
        let actions = match cliprdr.downcast_backend_mut::<GuacClipboard>() {
            Some(backend) => backend.take_actions(),
            None => return Ok(()),
        };
        if actions.is_empty() {
            return Ok(());
        }
        let mut remote_text = None;
        let messages = match clipboard::messages_for(cliprdr, actions, &mut remote_text) {
            Ok(messages) => messages,
            Err(err) => {
                warn!(error = %err, "clipboard request failed");
                Vec::new()
            }
        };
        for messages in messages {
            let frame = self.stage.process_svc_processor_messages(messages)?;
            self.write_rdp(&frame).await?;
        }
        if let Some(text) = remote_text
            && !self.settings.disable_copy
        {
            session::send_clipboard(self.writer.buffer(), &text);
        }
        Ok(())
    }

    /// A connection parameter changed by the client during the session.
    fn client_parameter(&mut self, name: &str, value: &str) {
        match name {
            "resize-method" => {
                self.resize_method = ResizeMethod::parse(Some(value));
                info!(resize_method = ?self.resize_method, "client changed the resize method");
            }
            "dpi" => {
                let dpi = settings::parse_dpi(Some(value)).unwrap_or(self.client_dpi);
                info!(dpi, "client changed the DPI");
                self.request_scale(dpi);
            }
            _ => debug!(name, "parameter cannot be changed during the session"),
        }
    }

    /// Tells the client whether the display can change during the session,
    /// once that is known or when it changed.
    fn update_display_control(&mut self) {
        let ready = self.stage.display_control_ready();
        if let Some(available) = self.display_control.update(ready, Instant::now()) {
            info!(available, "Display Control availability");
            self.writer.push(&display_control::instruction(available));
        }
    }

    fn current_size(&self) -> (u32, u32) {
        (
            u32::from(self.image.width()),
            u32::from(self.image.height()),
        )
    }

    /// Asks for a new scale at the size the desktop has or is about to get.
    fn request_scale(&mut self, dpi: u32) {
        self.dpi = dpi;
        if self.resize.is_none() && settings::desktop_scale_factor(dpi) == self.scale {
            return;
        }
        let (width, height) = self
            .resize
            .as_ref()
            .map_or_else(|| self.current_size(), |r| (r.width, r.height));
        self.resize = Some(PendingResize {
            width,
            height,
            due: tokio::time::Instant::now() + RESIZE_DELAY,
        });
    }

    fn request_resize(&mut self, width: u32, height: u32) {
        if self.resize_method != ResizeMethod::DisplayUpdate {
            return;
        }
        let (width, height) = MonitorLayoutEntry::adjust_display_size(width, height);
        if self.resize.is_none() && (width, height) == self.current_size() {
            return;
        }
        self.resize = Some(PendingResize {
            width,
            height,
            due: tokio::time::Instant::now() + RESIZE_DELAY,
        });
    }

    /// Sends a pending size or scale change over the Display Control channel.
    async fn apply_resize(&mut self) -> anyhow::Result<()> {
        let Some(pending) = self.resize.take() else {
            return Ok(());
        };
        self.update_display_control();
        match self.stage.display_control_ready() {
            Some(false) if !self.display_control.is_unavailable() => {
                self.resize = Some(PendingResize {
                    due: tokio::time::Instant::now() + RESIZE_RETRY,
                    ..pending
                });
            }
            None | Some(false) => {
                debug!("Display Control channel unavailable; display change ignored");
            }
            Some(true) => {
                let scale = settings::desktop_scale_factor(self.dpi);
                if (pending.width, pending.height) == self.current_size() && scale == self.scale {
                    return Ok(());
                }
                if let Some(frame) =
                    self.stage
                        .encode_resize(pending.width, pending.height, Some(scale), None)
                {
                    debug!(
                        width = pending.width,
                        height = pending.height,
                        scale,
                        "requesting desktop layout"
                    );
                    self.write_rdp(&frame?).await?;
                    self.scale = scale;
                }
            }
        }
        Ok(())
    }
}

fn supports_fastpath(flags: InputFlags) -> bool {
    flags.intersects(InputFlags::FASTPATH_INPUT | InputFlags::FASTPATH_INPUT_2)
}

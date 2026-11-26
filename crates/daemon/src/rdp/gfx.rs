//! Graphics over the graphics pipeline ([MS-RDPEGFX]): the server draws
//! into surfaces with its newer codecs (H.264 as AVC420 and AVC444,
//! progressive RemoteFX, ClearCodec) and says where each frame ends, instead
//! of sending bitmap updates as they come.
//!
//! IronRDP's [`GraphicsPipelineClient`] does the protocol: it advertises the
//! capability sets, decodes every codec, keeps the surfaces and the bitmap
//! cache, applies fills and copies, maps surfaces to the output and
//! acknowledges each frame as soon as it has been decoded, which is what
//! lets the server send the next one. Only H.264 decoding is left to its
//! user (`crate::video::h264_decoder`); without a decoder the capability
//! sets with H.264 are not advertised.
//!
//! [`GraphicsChannel`] wraps that client as the dynamic channel. The
//! wrapping matters: IronRDP's active stage looks for a channel of the
//! client's own type and, if it finds one, copies every frame into its
//! decoded image first, from where the session would copy it again. With the
//! wrapper the session takes the changed output regions itself ([`Frames`])
//! and writes them straight into the display. The regions only become
//! available when the server ends a frame, so the display never holds half
//! of one.
//!
//! Decoding does not run on the session loop. A full-screen AVC444 frame
//! takes 17 ms to decode at 3840×1919, and the loop also encodes the frame
//! for the browser (16 ms as video) and reads the client's input; one after
//! the other they held it for the whole time between two frames of the
//! server. The channel hands every message to a thread of its own, which
//! runs the client ([`Pipeline`]) and sends back what the session has to do
//! with the result ([`Decoded`]): write the replies to the server and draw
//! the frames. Replies and frames keep the order of the messages.
//!
//! The client reports what it does to a handler ([`PipelineEvents`]), which
//! counts the decoded pixels per codec.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use ironrdp_core::impl_as_any;
use ironrdp_dvc::{DvcClientProcessor, DvcMessage, DvcProcessor};
use ironrdp_egfx::client::{BitmapUpdate, GraphicsPipelineClient, GraphicsPipelineHandler};
use ironrdp_egfx::compositor::OutputUpdate;
use ironrdp_egfx::pdu::{
    CacheToSurfacePdu, CapabilitySet, Codec1Type, SolidFillPdu, SurfaceToSurfacePdu,
    WireToSurface2Pdu,
};
use ironrdp_pdu::PduResult;
use metrics::{Counter, Histogram, counter, histogram};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::{info, warn};

use crate::display::Display;
use crate::video::PipelineDecoder;

/// Codecs of the pipeline, as metric labels and indexes into the counts.
const CODECS: [&str; 8] = [
    "avc420",
    "avc444",
    "progressive",
    "remotefx",
    "clearcodec",
    "planar",
    "uncompressed",
    "alpha",
];
const PROGRESSIVE: usize = 2;
const UNCOMPRESSED: usize = 6;

fn codec_index(codec: Codec1Type) -> usize {
    match codec {
        Codec1Type::Avc420 => 0,
        Codec1Type::Avc444 | Codec1Type::Avc444v2 => 1,
        Codec1Type::RemoteFx => 3,
        Codec1Type::ClearCodec => 4,
        Codec1Type::Planar => 5,
        Codec1Type::Uncompressed => UNCOMPRESSED,
        Codec1Type::Alpha => 7,
    }
}

/// The protocol version of a capability set, as it is written.
fn version(capabilities: &CapabilitySet) -> &'static str {
    match capabilities {
        CapabilitySet::V8 { .. } => "8",
        CapabilitySet::V8_1 { .. } => "8.1",
        CapabilitySet::V10 { .. } => "10",
        CapabilitySet::V10_1 => "10.1",
        CapabilitySet::V10_2 { .. } => "10.2",
        CapabilitySet::V10_3 { .. } => "10.3",
        CapabilitySet::V10_4 { .. } => "10.4",
        CapabilitySet::V10_5 { .. } => "10.5",
        CapabilitySet::V10_6 { .. } | CapabilitySet::V10_6Err { .. } => "10.6",
        CapabilitySet::V10_7 { .. } => "10.7",
    }
}

/// What the handler has seen since the channel last looked.
#[derive(Default)]
struct Tally {
    /// Frames the server ended.
    frames: u32,
    /// Decoded pixels per codec, over the whole session.
    pixels: [u64; CODECS.len()],
}

/// The client's handler. Pixels are not its business (the client composes
/// them and the channel takes the result); it counts.
struct PipelineEvents {
    tally: Arc<Mutex<Tally>>,
    pixels: [Counter; CODECS.len()],
    /// The updates that follow are the tiles of a progressive RemoteFX
    /// command, which the client reports as uncompressed. An uncompressed
    /// command right behind a progressive one in the same frame is counted
    /// as progressive too; the client gives no way to tell them apart.
    progressive: bool,
}

impl PipelineEvents {
    fn tally(&self) -> std::sync::MutexGuard<'_, Tally> {
        self.tally.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl GraphicsPipelineHandler for PipelineEvents {
    fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
        let mut codec = codec_index(update.codec_id);
        if codec != UNCOMPRESSED {
            self.progressive = false;
        } else if self.progressive {
            codec = PROGRESSIVE;
        }
        let pixels = u64::from(update.width) * u64::from(update.height);
        self.pixels[codec].increment(pixels);
        self.tally().pixels[codec] += pixels;
    }

    fn on_wire_to_surface2(&mut self, _pdu: &WireToSurface2Pdu) {
        self.progressive = true;
    }

    fn on_solid_fill(&mut self, _pdu: &SolidFillPdu) {
        self.progressive = false;
    }

    fn on_surface_to_surface(&mut self, _pdu: &SurfaceToSurfacePdu) {
        self.progressive = false;
    }

    fn on_cache_to_surface(&mut self, _pdu: &CacheToSurfacePdu) {
        self.progressive = false;
    }

    fn on_frame_complete(&mut self, _frame_id: u32) {
        self.progressive = false;
        self.tally().frames += 1;
    }
}

/// Frames the pipeline completed: what changed on the output, in order.
pub struct Frames {
    /// The server set a new output size (`ResetGraphics`).
    pub resized: Option<(u16, u16)>,
    updates: Vec<OutputUpdate>,
}

impl Frames {
    /// Writes the changed regions into the display. They are in output
    /// coordinates (the surface's position on the output is already
    /// applied) and RGBA, top row first.
    pub fn draw(&self, display: &mut Display) {
        for update in &self.updates {
            let region = &update.region;
            display.put(
                u32::from(region.left),
                u32::from(region.top),
                u32::from(region.right.saturating_sub(region.left)),
                u32::from(region.bottom.saturating_sub(region.top)),
                &update.data,
            );
        }
    }
}

/// The pipeline's client with its bookkeeping: what the decoding thread
/// runs for every message of the channel.
struct Pipeline {
    client: GraphicsPipelineClient,
    tally: Arc<Mutex<Tally>>,
    /// Name of the H.264 decoder, if there is one.
    decoder: Option<&'static str>,
    /// The negotiated capabilities were logged.
    announced: bool,
    /// Time spent on the messages of the frame under way.
    frame_time: Duration,
    frames: Counter,
    decode: Histogram,
    /// Frames and decode times over the session, for the summary.
    total_frames: u64,
    total_time: Duration,
    longest: Duration,
}

impl Pipeline {
    fn new(decoder: Option<PipelineDecoder>) -> Self {
        let tally = Arc::new(Mutex::new(Tally::default()));
        let events = PipelineEvents {
            tally: Arc::clone(&tally),
            pixels: CODECS
                .map(|codec| counter!("molcajeted_rdp_gfx_pixels_total", "codec" => codec)),
            progressive: false,
        };
        let (name, decoder) = match decoder {
            Some(PipelineDecoder { name, decoder }) => (Some(name), Some(decoder)),
            None => (None, None),
        };
        Self {
            client: GraphicsPipelineClient::new(Box::new(events), decoder),
            tally,
            decoder: name,
            announced: false,
            frame_time: Duration::ZERO,
            frames: counter!("molcajeted_rdp_gfx_frames_total"),
            decode: histogram!("molcajeted_rdp_gfx_frame_decode_seconds"),
            total_frames: 0,
            total_time: Duration::ZERO,
            longest: Duration::ZERO,
        }
    }

    /// What changed on the output in the frames completed since the last
    /// call, if anything.
    fn take_frames(&mut self) -> Option<Frames> {
        let frames = Frames {
            resized: self.client.take_output_reset(),
            updates: self.client.drain_output(),
        };
        (frames.resized.is_some() || !frames.updates.is_empty()).then_some(frames)
    }

    /// Logs once which version the server chose and what it may send.
    fn announce(&mut self) {
        if self.announced || !self.client.is_active() {
            return;
        }
        self.announced = true;
        let codecs = self.client.codec_capabilities();
        info!(
            version = self
                .client
                .negotiated_capabilities()
                .map_or("unknown", version),
            avc420 = codecs.avc420,
            avc444 = codecs.avc444,
            h264_decoder = self.decoder.unwrap_or("none"),
            "RDP graphics pipeline in use"
        );
    }

    /// Records the frames that ended in the message just processed.
    fn count_frames(&mut self) {
        let ended = std::mem::take(
            &mut self
                .tally
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .frames,
        );
        if ended == 0 {
            return;
        }
        // Several frames in one message share its time.
        let each = std::mem::take(&mut self.frame_time) / ended;
        for _ in 0..ended {
            self.decode.record(each);
        }
        self.frames.increment(u64::from(ended));
        self.total_frames += u64::from(ended);
        self.total_time += each * ended;
        self.longest = self.longest.max(each);
    }
}

impl Pipeline {
    fn channel_name(&self) -> &str {
        self.client.channel_name()
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.client.start(channel_id)
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let started = Instant::now();
        let replies = self.client.process(channel_id, payload);
        self.frame_time += started.elapsed();
        self.announce();
        self.count_frames();
        replies
    }

    fn close(&mut self, channel_id: u32) {
        self.client.close(channel_id);
    }
}

impl Drop for Pipeline {
    /// Logs the session's totals, like the display does for its own.
    fn drop(&mut self) {
        if self.total_frames == 0 {
            return;
        }
        let pixels = self
            .tally
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pixels;
        info!(
            frames = self.total_frames,
            decode_ms_avg = self.total_time.as_secs_f64() * 1000.0 / self.total_frames as f64,
            decode_ms_max = self.longest.as_secs_f64() * 1000.0,
            dropped_updates = self.client.dropped_updates(),
            avc420_pixels = pixels[0],
            avc444_pixels = pixels[1],
            progressive_pixels = pixels[2],
            remotefx_pixels = pixels[3],
            clearcodec_pixels = pixels[4],
            planar_pixels = pixels[5],
            uncompressed_pixels = pixels[6],
            alpha_pixels = pixels[7],
            "RDP graphics pipeline summary"
        );
    }
}

/// What the decoding thread made of one message of the channel, for the
/// session to act on.
pub struct Decoded {
    /// The channel the replies go to.
    pub channel_id: u32,
    /// The client's replies for the server (the acknowledgement of a frame
    /// among them), or why the message could not be processed.
    pub replies: Result<Vec<DvcMessage>, String>,
    /// The frames the message completed.
    pub frames: Option<Frames>,
}

/// A message of the channel, for the decoding thread.
struct Job {
    channel_id: u32,
    payload: Vec<u8>,
}

/// The graphics pipeline's dynamic channel.
///
/// The server may close the channel and open it again, each time as a new
/// one of these with a thread of its own.
pub struct GraphicsChannel {
    name: String,
    pipeline: Arc<Mutex<Pipeline>>,
    /// Messages for the decoding thread, which ends when this is dropped.
    jobs: mpsc::Sender<Job>,
    /// What the thread made of them, until the session takes it.
    decoded: Option<UnboundedReceiver<Decoded>>,
    /// The server closed the channel: what is still queued is void.
    closed: Arc<AtomicBool>,
}

impl GraphicsChannel {
    pub fn new(decoder: Option<PipelineDecoder>) -> Self {
        let pipeline = Pipeline::new(decoder);
        let name = pipeline.channel_name().to_owned();
        let pipeline = Arc::new(Mutex::new(pipeline));
        let closed = Arc::new(AtomicBool::new(false));
        let (jobs, queue) = mpsc::channel();
        let (results, decoded) = unbounded_channel();
        let worker = {
            let (pipeline, closed) = (Arc::clone(&pipeline), Arc::clone(&closed));
            // What the thread logs belongs to the connection it works for.
            let span = tracing::Span::current();
            move || span.in_scope(|| decode_messages(&queue, &pipeline, &closed, &results))
        };
        if let Err(err) = std::thread::Builder::new()
            .name("rdp-gfx".into())
            .spawn(worker)
        {
            // Without the thread no message is answered and the server
            // stops sending; the session then ends on its own timeouts.
            warn!(error = %err, "cannot start the graphics pipeline's decoding thread");
        }
        Self {
            name,
            pipeline,
            jobs,
            decoded: Some(decoded),
            closed,
        }
    }

    /// What the decoding thread produces, for the session to receive. Given
    /// out once.
    pub fn take_decoded(&mut self) -> Option<UnboundedReceiver<Decoded>> {
        self.decoded.take()
    }

    fn pipeline(&self) -> std::sync::MutexGuard<'_, Pipeline> {
        self.pipeline.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The decoding thread: runs the client for every message, in order, until
/// the channel is dropped or the session no longer listens.
fn decode_messages(
    queue: &mpsc::Receiver<Job>,
    pipeline: &Mutex<Pipeline>,
    closed: &AtomicBool,
    results: &UnboundedSender<Decoded>,
) {
    for Job {
        channel_id,
        payload,
    } in queue
    {
        if closed.load(Ordering::Acquire) {
            continue;
        }
        let mut pipeline = pipeline.lock().unwrap_or_else(PoisonError::into_inner);
        let replies = pipeline
            .process(channel_id, &payload)
            .map_err(|err| format!("{err:#}"));
        let frames = pipeline.take_frames();
        drop(pipeline);
        // A channel closed meanwhile takes no replies any more.
        if closed.load(Ordering::Acquire) {
            continue;
        }
        let decoded = Decoded {
            channel_id,
            replies,
            frames,
        };
        if results.send(decoded).is_err() {
            return;
        }
    }
}

impl_as_any!(GraphicsChannel);

impl DvcProcessor for GraphicsChannel {
    fn channel_name(&self) -> &str {
        &self.name
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.pipeline().start(channel_id)
    }

    /// Queues the message for the decoding thread. Its replies reach the
    /// server through [`Decoded`], not from here.
    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let job = Job {
            channel_id,
            payload: payload.to_vec(),
        };
        // The thread only ends early when the session is gone.
        let _ = self.jobs.send(job);
        Ok(Vec::new())
    }

    fn close(&mut self, channel_id: u32) {
        self.closed.store(true, Ordering::Release);
        self.pipeline().close(channel_id);
    }
}

impl DvcClientProcessor for GraphicsChannel {}

#[cfg(test)]
mod tests {
    use ironrdp_core::{Decode as _, ReadCursor, encode_vec};
    use ironrdp_egfx::pdu::{
        Avc420Region, CacheToSurfacePdu, CapabilitiesConfirmPdu, CapabilitiesV8Flags,
        CapabilitiesV107Flags, Color, CreateSurfacePdu, EndFramePdu, GfxPdu, MapSurfaceToOutputPdu,
        PixelFormat, Point, ResetGraphicsPdu, StartFramePdu, SurfaceToCachePdu, Timestamp,
        WireToSurface1Pdu, encode_avc420_bitmap_stream,
    };
    use ironrdp_graphics::zgfx;
    use ironrdp_pdu::geometry::ExclusiveRectangle;

    use super::*;
    use crate::display::ImageFormats;
    use crate::io::Output;

    const CHANNEL: u32 = 7;
    const SURFACE: u16 = 3;

    /// Sends the server's commands as one channel message and returns the
    /// client's replies.
    fn send(channel: &mut Pipeline, pdus: &[GfxPdu]) -> Vec<GfxPdu> {
        let bytes: Vec<u8> = pdus
            .iter()
            .flat_map(|pdu| encode_vec(pdu).expect("encode"))
            .collect();
        let replies = channel
            .process(CHANNEL, &zgfx::wrap_uncompressed(&bytes))
            .expect("process");
        decode(replies)
    }

    fn decode(messages: Vec<DvcMessage>) -> Vec<GfxPdu> {
        messages
            .iter()
            .map(|message| {
                let bytes = encode_vec(message.as_ref()).expect("encode");
                GfxPdu::decode(&mut ReadCursor::new(&bytes)).expect("decode")
            })
            .collect()
    }

    fn rect(left: u16, top: u16, right: u16, bottom: u16) -> ExclusiveRectangle {
        ExclusiveRectangle {
            left,
            top,
            right,
            bottom,
        }
    }

    /// A channel past the capability exchange, with an output of
    /// `width`×`height` and a `surface.0`×`surface.1` surface mapped at
    /// `origin`.
    fn channel_with_surface(
        decoder: Option<PipelineDecoder>,
        (width, height): (u32, u32),
        surface: (u16, u16),
        origin: (u32, u32),
    ) -> Pipeline {
        let mut channel = Pipeline::new(decoder);
        channel.start(CHANNEL).expect("start");
        let confirmed = CapabilitySet::V10_7 {
            flags: CapabilitiesV107Flags::SMALL_CACHE,
        };
        send(
            &mut channel,
            &[
                GfxPdu::CapabilitiesConfirm(CapabilitiesConfirmPdu::from_typed(&confirmed)),
                GfxPdu::ResetGraphics(ResetGraphicsPdu {
                    width,
                    height,
                    monitors: Vec::new(),
                }),
                GfxPdu::CreateSurface(CreateSurfacePdu {
                    surface_id: SURFACE,
                    width: surface.0,
                    height: surface.1,
                    pixel_format: PixelFormat::XRgb,
                }),
                GfxPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
                    surface_id: SURFACE,
                    output_origin_x: origin.0,
                    output_origin_y: origin.1,
                }),
            ],
        );
        channel
    }

    fn start_frame(frame_id: u32) -> GfxPdu {
        GfxPdu::StartFrame(StartFramePdu {
            timestamp: Timestamp {
                milliseconds: 0,
                seconds: 0,
                minutes: 0,
                hours: 0,
            },
            frame_id,
        })
    }

    /// An uncompressed command painting `area` of the surface in one colour.
    fn paint(area: ExclusiveRectangle, [r, g, b]: [u8; 3]) -> GfxPdu {
        let pixels = usize::from(area.right - area.left) * usize::from(area.bottom - area.top);
        GfxPdu::WireToSurface1(WireToSurface1Pdu {
            surface_id: SURFACE,
            codec_id: Codec1Type::Uncompressed,
            pixel_format: PixelFormat::XRgb,
            destination_rectangle: area,
            // On the wire a pixel is B, G, R, X.
            bitmap_data: [b, g, r, 0].repeat(pixels),
        })
    }

    fn display(width: u32, height: u32) -> Display {
        let mut display = Display::new(ImageFormats::default(), true);
        display.resize(width, height, &mut Output::text());
        display
    }

    fn pixel(display: &Display, x: u32, y: u32) -> [u8; 3] {
        let offset = ((y * display.width() + x) * 4) as usize;
        let px = &display.framebuffer()[offset..offset + 3];
        [px[0], px[1], px[2]]
    }

    fn advertised(channel: &mut Pipeline) -> Vec<CapabilitySet> {
        let replies = decode(channel.start(CHANNEL).expect("start"));
        let [GfxPdu::CapabilitiesAdvertise(advertise)] = replies.as_slice() else {
            panic!("expected a capability advertisement, got {replies:?}");
        };
        advertise
            .0
            .iter()
            .map(|raw| raw.parsed().expect("parse").expect("known version"))
            .collect()
    }

    /// The channel's messages are decoded on its thread: `process` answers
    /// nothing itself, and the replies and frames arrive for the session in
    /// the order of the messages.
    #[test]
    fn the_channel_decodes_on_its_own_thread() {
        let mut channel = GraphicsChannel::new(None);
        assert_eq!(channel.channel_name(), ironrdp_egfx::CHANNEL_NAME);
        let mut decoded = channel.take_decoded().expect("results");
        assert!(channel.take_decoded().is_none(), "given out once");
        channel.start(CHANNEL).expect("start");
        let confirmed = CapabilitySet::V10_7 {
            flags: CapabilitiesV107Flags::SMALL_CACHE,
        };
        let messages: [&[GfxPdu]; 3] = [
            &[
                GfxPdu::CapabilitiesConfirm(CapabilitiesConfirmPdu::from_typed(&confirmed)),
                GfxPdu::ResetGraphics(ResetGraphicsPdu {
                    width: 64,
                    height: 64,
                    monitors: Vec::new(),
                }),
                GfxPdu::CreateSurface(CreateSurfacePdu {
                    surface_id: SURFACE,
                    width: 64,
                    height: 64,
                    pixel_format: PixelFormat::XRgb,
                }),
                GfxPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
                    surface_id: SURFACE,
                    output_origin_x: 0,
                    output_origin_y: 0,
                }),
            ],
            &[start_frame(1), paint(rect(0, 0, 8, 8), [200, 10, 20])],
            &[GfxPdu::EndFrame(EndFramePdu { frame_id: 1 })],
        ];
        for pdus in messages {
            let bytes: Vec<u8> = pdus
                .iter()
                .flat_map(|pdu| encode_vec(pdu).expect("encode"))
                .collect();
            let replies = channel
                .process(CHANNEL, &zgfx::wrap_uncompressed(&bytes))
                .expect("process");
            assert!(replies.is_empty(), "replies come from the thread");
        }
        let mut display = display(64, 64);
        let mut acknowledged = Vec::new();
        for _ in 0..messages.len() {
            let result = decoded.blocking_recv().expect("a result per message");
            assert_eq!(result.channel_id, CHANNEL);
            for reply in decode(result.replies.expect("replies")) {
                if let GfxPdu::FrameAcknowledge(ack) = reply {
                    acknowledged.push(ack.frame_id);
                }
            }
            if let Some(frames) = result.frames {
                // Nothing of the frame before the server ended it. (The
                // output size comes on its own, with the first message.)
                if !frames.updates.is_empty() {
                    assert_eq!(acknowledged, [1]);
                }
                frames.draw(&mut display);
            }
        }
        assert_eq!(acknowledged, [1]);
        assert_eq!(pixel(&display, 4, 4), [200, 10, 20]);
        // The thread ends with the channel, and the session is told.
        drop(channel);
        assert!(decoded.blocking_recv().is_none());
    }

    #[test]
    fn without_a_decoder_no_h264_is_advertised() {
        let mut channel = Pipeline::new(None);
        assert_eq!(channel.channel_name(), ironrdp_egfx::CHANNEL_NAME);
        assert_eq!(
            advertised(&mut channel),
            [CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::SMALL_CACHE
            }]
        );
    }

    #[test]
    fn with_a_decoder_every_version_is_advertised() {
        let Some(decoder) = crate::video::h264_decoder() else {
            return;
        };
        let mut channel = Pipeline::new(Some(decoder));
        let sets = advertised(&mut channel);
        // The server takes the newest it knows; all of 10.x imply AVC444.
        assert_eq!(sets.first().map(version), Some("10.7"));
        assert!(sets.iter().any(|set| version(set) == "8.1"));
        assert_eq!(sets.last().map(version), Some("8"));
    }

    #[test]
    fn a_frame_reaches_the_display_where_its_surface_is_mapped() {
        let mut channel = channel_with_surface(None, (64, 48), (16, 8), (20, 10));
        let mut display = display(64, 48);
        // The size the display already has is reported, nothing drawn yet.
        let reset = channel.take_frames().expect("reset");
        assert_eq!(reset.resized, Some((64, 48)));
        reset.draw(&mut display);

        let replies = send(
            &mut channel,
            &[start_frame(1), paint(rect(4, 2, 8, 4), [200, 100, 50])],
        );
        // Nothing is acknowledged or shown before the frame ends.
        assert!(replies.is_empty());
        assert!(channel.take_frames().is_none());

        let replies = send(
            &mut channel,
            &[GfxPdu::EndFrame(EndFramePdu { frame_id: 1 })],
        );
        let [GfxPdu::FrameAcknowledge(ack)] = replies.as_slice() else {
            panic!("expected an acknowledgement, got {replies:?}");
        };
        assert_eq!((ack.frame_id, ack.total_frames_decoded), (1, 1));

        let frames = channel.take_frames().expect("a frame");
        assert_eq!(frames.resized, None);
        frames.draw(&mut display);
        // Surface (4, 2) is output (24, 12).
        assert_eq!(pixel(&display, 24, 12), [200, 100, 50]);
        assert_eq!(pixel(&display, 27, 13), [200, 100, 50]);
        for (x, y) in [(23, 12), (28, 12), (24, 11), (24, 14), (4, 2)] {
            assert_eq!(pixel(&display, x, y), [0, 0, 0], "({x}, {y})");
        }
        // Taken once.
        assert!(channel.take_frames().is_none());
        assert_eq!(channel.total_frames, 1);
    }

    #[test]
    fn fills_copies_and_the_cache_are_composed_before_the_display() {
        let mut channel = channel_with_surface(None, (64, 48), (32, 16), (8, 4));
        let mut display = display(64, 48);
        let red = Color {
            b: 0,
            g: 0,
            r: 255,
            xa: 0xFF,
        };
        send(
            &mut channel,
            &[
                start_frame(1),
                GfxPdu::SolidFill(SolidFillPdu {
                    surface_id: SURFACE,
                    fill_pixel: red,
                    rectangles: vec![rect(0, 0, 4, 4)],
                }),
                // The red square again at (8, 0), copied within the surface.
                GfxPdu::SurfaceToSurface(SurfaceToSurfacePdu {
                    source_surface_id: SURFACE,
                    destination_surface_id: SURFACE,
                    source_rectangle: rect(0, 0, 4, 4),
                    destination_points: vec![Point { x: 8, y: 0 }],
                }),
                // And once more at (16, 8), through the bitmap cache.
                GfxPdu::SurfaceToCache(SurfaceToCachePdu {
                    surface_id: SURFACE,
                    cache_key: 42,
                    cache_slot: 1,
                    source_rectangle: rect(0, 0, 4, 4),
                }),
                GfxPdu::CacheToSurface(CacheToSurfacePdu {
                    cache_slot: 1,
                    surface_id: SURFACE,
                    destination_points: vec![Point { x: 16, y: 8 }],
                }),
                GfxPdu::EndFrame(EndFramePdu { frame_id: 1 }),
            ],
        );
        channel.take_frames().expect("a frame").draw(&mut display);
        for (x, y) in [(8, 4), (11, 7), (16, 4), (19, 7), (24, 12), (27, 15)] {
            assert_eq!(pixel(&display, x, y), [255, 0, 0], "({x}, {y})");
        }
        for (x, y) in [(12, 4), (20, 4), (8, 8), (28, 12), (0, 0)] {
            assert_eq!(pixel(&display, x, y), [0, 0, 0], "({x}, {y})");
        }
    }

    #[test]
    fn a_surface_that_is_not_on_the_output_shows_nothing() {
        let mut channel = channel_with_surface(None, (64, 48), (16, 8), (0, 0));
        // The mapped surface appears with the first frame, empty as it is.
        send(
            &mut channel,
            &[
                start_frame(1),
                GfxPdu::EndFrame(EndFramePdu { frame_id: 1 }),
            ],
        );
        let mut display = display(64, 48);
        display.fill(0, 0, 64, 48, [7, 7, 7, 255]);
        channel.take_frames().expect("mapping").draw(&mut display);
        assert_eq!(pixel(&display, 15, 7), [0, 0, 0]);
        assert_eq!(pixel(&display, 16, 7), [7, 7, 7]);

        send(
            &mut channel,
            &[
                GfxPdu::CreateSurface(CreateSurfacePdu {
                    surface_id: SURFACE + 1,
                    width: 16,
                    height: 8,
                    pixel_format: PixelFormat::XRgb,
                }),
                start_frame(2),
                GfxPdu::SolidFill(SolidFillPdu {
                    surface_id: SURFACE + 1,
                    fill_pixel: Color {
                        b: 9,
                        g: 9,
                        r: 9,
                        xa: 0xFF,
                    },
                    rectangles: vec![rect(0, 0, 16, 8)],
                }),
                GfxPdu::EndFrame(EndFramePdu { frame_id: 2 }),
            ],
        );
        assert!(channel.take_frames().is_none());
        assert_eq!(channel.total_frames, 2);
    }

    #[test]
    fn a_new_output_size_is_reported_once() {
        let mut channel = channel_with_surface(None, (64, 48), (16, 8), (0, 0));
        channel.take_frames();
        send(
            &mut channel,
            &[GfxPdu::ResetGraphics(ResetGraphicsPdu {
                width: 128,
                height: 96,
                monitors: Vec::new(),
            })],
        );
        let frames = channel.take_frames().expect("the new size");
        assert_eq!(frames.resized, Some((128, 96)));
        assert!(channel.take_frames().is_none());
    }

    #[test]
    fn pixels_are_counted_by_codec() {
        let mut channel = channel_with_surface(None, (64, 48), (16, 8), (0, 0));
        send(
            &mut channel,
            &[
                start_frame(1),
                paint(rect(0, 0, 4, 2), [1, 2, 3]),
                paint(rect(0, 0, 2, 2), [4, 5, 6]),
                GfxPdu::EndFrame(EndFramePdu { frame_id: 1 }),
            ],
        );
        let tally = channel.tally.lock().unwrap();
        assert_eq!(tally.pixels[UNCOMPRESSED], 12);
        assert_eq!(tally.pixels.iter().sum::<u64>(), 12);
    }

    /// An H.264 encoder to make a test stream with, matching the decoder
    /// [`crate::video::h264_decoder`] returns in this build.
    fn h264_encoder(width: u32, height: u32) -> Option<Box<dyn crate::video::VideoEncoder>> {
        use crate::video::{Backend, EncoderConfig, VideoCodec};
        #[cfg(target_os = "macos")]
        let backend = Some(Backend::VideoToolbox(VideoCodec::H264));
        #[cfg(all(not(target_os = "macos"), feature = "openh264"))]
        let backend = Some(Backend::OpenH264);
        #[cfg(all(not(target_os = "macos"), not(feature = "openh264")))]
        let backend: Option<Backend> = None;
        let config = EncoderConfig {
            codec: VideoCodec::H264,
            width,
            height,
            fps: 30,
            bitrate_kbps: 4000,
        };
        Some(backend?.create(&config).expect("H.264 encoder"))
    }

    #[test]
    fn an_avc420_frame_is_decoded_into_the_display() {
        const WIDTH: u16 = 320;
        const HEIGHT: u16 = 240;
        const ORIGIN: (u32, u32) = (32, 16);
        let (Some(decoder), Some(mut encoder)) = (
            crate::video::h264_decoder(),
            h264_encoder(u32::from(WIDTH), u32::from(HEIGHT)),
        ) else {
            return;
        };
        // A grey ramp from left to right.
        let grey = |x: u32| (40 + x / 2) as u8;
        let rgba: Vec<u8> = (0..u32::from(WIDTH) * u32::from(HEIGHT))
            .flat_map(|i| {
                let level = grey(i % u32::from(WIDTH));
                [level, level, level, 255]
            })
            .collect();
        let access_unit = encoder.encode(&rgba, true).expect("encode").data;

        let mut channel = channel_with_surface(Some(decoder), (400, 300), (WIDTH, HEIGHT), ORIGIN);
        let mut display = display(400, 300);
        // Only the right half of the picture is declared as changed.
        let changed = Avc420Region::new(WIDTH / 2, 0, WIDTH, HEIGHT, 22, 100);
        send(
            &mut channel,
            &[
                start_frame(1),
                GfxPdu::WireToSurface1(WireToSurface1Pdu {
                    surface_id: SURFACE,
                    codec_id: Codec1Type::Avc420,
                    pixel_format: PixelFormat::XRgb,
                    destination_rectangle: rect(WIDTH / 2, 0, WIDTH, HEIGHT),
                    bitmap_data: encode_avc420_bitmap_stream(&[changed], &access_unit),
                }),
                GfxPdu::EndFrame(EndFramePdu { frame_id: 1 }),
            ],
        );
        channel.take_frames().expect("a frame").draw(&mut display);

        // The encoders write limited range and the pipeline reads full
        // range, as its servers send it: a grey level comes back as its
        // limited-range code.
        let expected = |x: u32| 16.0 + f64::from(grey(x)) * 219.0 / 255.0;
        for x in (u32::from(WIDTH) / 2 + 8..u32::from(WIDTH) - 8).step_by(24) {
            let [r, g, b] = pixel(&display, ORIGIN.0 + x, ORIGIN.1 + 100);
            for channel in [r, g, b] {
                let off = (f64::from(channel) - expected(x)).abs();
                assert!(off < 6.0, "x {x}: {channel} instead of {:.0}", expected(x));
            }
        }
        // The left half was not in the changed region.
        assert_eq!(pixel(&display, ORIGIN.0 + 80, ORIGIN.1 + 100), [0, 0, 0]);
        assert_eq!(
            channel.tally.lock().unwrap().pixels[0],
            u64::from(WIDTH / 2) * u64::from(HEIGHT)
        );
        assert_eq!(channel.client.dropped_updates(), 0);
    }
}

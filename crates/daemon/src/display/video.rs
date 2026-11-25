//! Video mode for busy regions.
//!
//! Large, colourful areas that change continuously (video playback,
//! animations, 3D views) are expensive as images, even as JPEG. Once such an
//! area is found it is covered by a dedicated layer that receives a
//! compressed video stream (framed as described in `molcajete_protocol::video`),
//! and the area is left out of the normal image diff.
//!
//! The mirror of the default layer is not touched while the video plays, so
//! it keeps describing what the client's default layer holds underneath.
//! When the area calms down (or the busy area moves elsewhere), the video
//! layer is disposed and the default layer is brought up to date losslessly
//! in the same frame: the client never shows stale or lossy content for long.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use molcajete_protocol::video::{VideoCodec, encode_frame};
use molcajete_protocol::{InstructionBuilder, Status};
use tracing::{debug, info, warn};

use super::{BLOB_MAX_BYTES, Display, FlushStats, Rect, TILE};
use crate::video::{EncoderConfig, VideoEncoder, VideoError};

/// Creates encoders for the codec chosen at connection time.
pub type EncoderFactory =
    Box<dyn FnMut(&EncoderConfig) -> Result<Box<dyn VideoEncoder>, VideoError> + Send>;

/// Layer covering the video area (above the default layer).
const VIDEO_LAYER: i32 = 1;
/// Stream carrying the video (image streams use 0..63, clipboard 100).
pub(super) const VIDEO_STREAM: i32 = 101;
/// Motion score at which a tile counts as busy. The score grows by one per
/// frame in which the tile had to be sent as pixels and halves every second,
/// so this is reached after ~0.3 s at 30 fps, ~0.6 s at 15 fps, and never
/// below ~6 updates per second.
pub(super) const MOTION_HOT: f32 = 8.0;
/// Smallest busy area worth a video stream, in tiles (≈ 25k pixels).
const MIN_TILES: usize = 6;
/// Busy tiles must cover at least this share of their bounding box.
const MIN_DENSITY: f32 = 0.5;
/// Areas with fewer distinct colours (text, UI) are left to the lossless
/// encodings, which handle them better.
const MIN_COLORS: usize = 256;
/// The video ends after this long without changes.
const IDLE: Duration = Duration::from_millis(1000);
/// Frames in which the busy area must extend beyond the video before the
/// video is restarted over the new area.
const REGROW_FRAMES: u32 = 8;
/// Encoder errors after which video mode is given up for the session.
const MAX_FAILURES: u32 = 3;
/// Nominal frame rate given to rate control (frames are sent at most this
/// often by the frame pacing; slower content simply uses fewer bits).
const FPS: u32 = 30;
/// Target bits per pixel and frame for H.264; the other codecs reach the
/// same quality with fewer bits.
const H264_BITS_PER_PIXEL: f32 = 0.1;
const MIN_BITRATE_KBPS: u32 = 300;
const MAX_BITRATE_KBPS: u32 = 16_000;

// Encoders and the factory sit in a `Mutex` only to make `Display` `Sync`
// (sessions hold `&Display` across awaits); access is always through
// `&mut self` and `Mutex::get_mut`, which never locks.

pub(super) struct VideoSupport {
    /// Usable codecs, most preferred first, each with the largest frame (in
    /// pixels) its encoder handles in real time.
    codecs: Vec<(VideoCodec, u64)>,
    factory: Mutex<EncoderFactory>,
    failures: u32,
}

impl VideoSupport {
    /// The most preferred codec that can encode `pixels` in real time, or
    /// the fastest one if none can.
    fn codec_for(&self, pixels: u64) -> VideoCodec {
        self.codecs
            .iter()
            .find(|(_, max)| pixels <= *max)
            .or_else(|| self.codecs.iter().max_by_key(|(_, max)| *max))
            .map(|(codec, _)| *codec)
            .expect("at least one codec")
    }
}

pub(super) struct ActiveVideo {
    /// Screen area covered by the video layer (tile-aligned, clipped).
    pub(super) rect: Rect,
    /// Encoded size: `rect` rounded up to whole 16×16 macroblocks (the
    /// layer is as large; the part beyond the screen edge is never visible).
    width: u32,
    height: u32,
    encoder: Mutex<Box<dyn VideoEncoder>>,
    started: Instant,
    last_change: Instant,
    /// Last frame given to the encoder, to skip unchanged frames.
    previous: Vec<u8>,
    /// Consecutive frames in which the busy area extended beyond `rect`.
    outgrown: u32,
    frames: u64,
    bytes: usize,
}

fn bitrate_kbps(codec: VideoCodec, width: u32, height: u32) -> u32 {
    let factor = match codec {
        VideoCodec::H264 => 1.0,
        VideoCodec::H265 => 0.7,
        VideoCodec::Av1 => 0.6,
    };
    let bits = width as f32 * height as f32 * FPS as f32 * H264_BITS_PER_PIXEL * factor;
    ((bits / 1000.0) as u32).clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS)
}

fn contains(outer: &Rect, inner: &Rect) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && outer.right() >= inner.right()
        && outer.bottom() >= inner.bottom()
}

impl Display {
    /// Enables video mode: busy areas are sent as video produced by encoders
    /// from `factory`, using the first of `codecs` (codec, largest frame in
    /// pixels it encodes in real time) that suits the area. Ignored for
    /// lossless sessions.
    pub fn enable_video(&mut self, codecs: Vec<(VideoCodec, u64)>, factory: EncoderFactory) {
        if self.lossless || codecs.is_empty() {
            return;
        }
        self.video_support = Some(VideoSupport {
            codecs,
            factory: Mutex::new(factory),
            failures: 0,
        });
    }

    /// Whether tile (`tx`, `ty`) lies inside the video area.
    pub(super) fn tile_in_video(&self, tx: u32, ty: u32) -> bool {
        self.video.as_ref().is_some_and(|v| {
            let (x, y) = (tx * TILE, ty * TILE);
            x >= v.rect.x && x < v.rect.right() && y >= v.rect.y && y < v.rect.bottom()
        })
    }

    /// When the video should be checked even if nothing else changes (to
    /// end it once the area is idle).
    pub(super) fn video_deadline(&self) -> Option<Instant> {
        self.video.as_ref().map(|v| v.last_change + IDLE)
    }

    /// Handles an `ack` from the client: an error for the video stream means
    /// the client cannot play it, so video mode is turned off for good.
    pub fn stream_ack(&mut self, stream: i32, status: Status, out: &mut String) {
        if stream == VIDEO_STREAM && status.is_error() && self.video.is_some() {
            warn!(
                status = status.code(),
                "client rejected the video stream; video mode disabled"
            );
            self.video_support = None;
            self.stop_video(out);
        }
    }

    /// Bounding box (tile-aligned, clipped) and count of busy tiles.
    fn busy_area(&self) -> Option<(Rect, usize)> {
        let (mut x0, mut y0, mut x1, mut y1, mut count) = (u32::MAX, u32::MAX, 0, 0, 0usize);
        for ty in 0..self.tile_rows {
            for tx in 0..self.tile_cols {
                if self.motion[(ty * self.tile_cols + tx) as usize] >= MOTION_HOT {
                    x0 = x0.min(tx);
                    y0 = y0.min(ty);
                    x1 = x1.max(tx + 1);
                    y1 = y1.max(ty + 1);
                    count += 1;
                }
            }
        }
        if count == 0 {
            return None;
        }
        let rect = Rect::new(x0 * TILE, y0 * TILE, (x1 - x0) * TILE, (y1 - y0) * TILE)
            .clip(self.width, self.height);
        Some((rect, count))
    }

    /// The busy area, if it qualifies for a video stream.
    fn video_candidate(&self) -> Option<Rect> {
        let (rect, count) = self.busy_area()?;
        let tiles = (rect.w.div_ceil(TILE) * rect.h.div_ceil(TILE)) as usize;
        if count < MIN_TILES || (count as f32) < MIN_DENSITY * tiles as f32 {
            return None;
        }
        (self.sampled_colors(&rect) > MIN_COLORS).then_some(rect)
    }

    /// Distinct colours among ~2048 pixels sampled evenly from `rect` (like
    /// `encode::color_complexity`, without copying the area out first).
    fn sampled_colors(&self, rect: &Rect) -> usize {
        let area = rect.area() as usize;
        let step = (area / 2048).max(1);
        let mut seen = std::collections::HashSet::with_capacity(512);
        for i in (0..area).step_by(step) {
            let x = rect.x + (i % rect.w as usize) as u32;
            let y = rect.y + (i / rect.w as usize) as u32;
            let p = self.segment(&self.pixels, y, x, x + 1);
            seen.insert([p[0], p[1], p[2]]);
            if seen.len() > MIN_COLORS {
                break;
            }
        }
        seen.len()
    }

    /// Starts, feeds or stops the video. Runs first in every flush; tiles
    /// inside the video area are then skipped by the image diff.
    pub(super) fn update_video(&mut self, out: &mut String) -> FlushStats {
        if self.video_support.is_none() {
            return FlushStats::default();
        }
        let now = Instant::now();
        let candidate = self.video_candidate();

        let Some(active) = &mut self.video else {
            return match candidate {
                Some(rect) => self.start_video(rect, now, out),
                None => FlushStats::default(),
            };
        };
        if now >= active.last_change + IDLE {
            self.stop_video(out);
            return FlushStats::default();
        }
        let outgrown = candidate.is_some_and(|c| !contains(&active.rect, &c));
        active.outgrown = if outgrown { active.outgrown + 1 } else { 0 };
        if active.outgrown >= REGROW_FRAMES
            && let Some(rect) = candidate
        {
            // The busy area moved or grew: start over where it is now.
            self.stop_video(out);
            return self.start_video(rect, now, out);
        }
        self.video_frame(now, false, out)
    }

    fn start_video(&mut self, rect: Rect, now: Instant, out: &mut String) -> FlushStats {
        let Some(support) = &mut self.video_support else {
            return FlushStats::default();
        };
        let (width, height) = (rect.w.next_multiple_of(16), rect.h.next_multiple_of(16));
        let codec = support.codec_for(width as u64 * height as u64);
        let config = EncoderConfig {
            codec,
            width,
            height,
            fps: FPS,
            bitrate_kbps: bitrate_kbps(codec, width, height),
        };
        let factory = support.factory.get_mut().unwrap_or_else(|e| e.into_inner());
        let encoder = match factory(&config) {
            Ok(encoder) => encoder,
            Err(err) => {
                self.video_failed(&err);
                return FlushStats::default();
            }
        };
        info!(
            codec = ?config.codec,
            x = rect.x,
            y = rect.y,
            width,
            height,
            kbps = config.bitrate_kbps,
            "video mode started"
        );
        *out += &InstructionBuilder::new("size")
            .arg(VIDEO_LAYER)
            .arg(width)
            .arg(height)
            .finish();
        *out += &InstructionBuilder::new("move")
            .arg(VIDEO_LAYER)
            .arg(0)
            .arg(rect.x)
            .arg(rect.y)
            .arg(1)
            .finish();
        *out += &InstructionBuilder::new("video")
            .arg(VIDEO_STREAM)
            .arg(VIDEO_LAYER)
            .str_arg(config.codec.mimetype())
            .finish();
        self.video = Some(ActiveVideo {
            rect,
            width,
            height,
            encoder: Mutex::new(encoder),
            started: now,
            last_change: now,
            previous: Vec::new(),
            outgrown: 0,
            frames: 0,
            bytes: 0,
        });
        self.video_frame(now, true, out)
    }

    /// Ends the video stream and disposes its layer. The default layer
    /// underneath is re-compared in full at this flush.
    pub(super) fn stop_video(&mut self, out: &mut String) {
        let Some(video) = self.video.take() else {
            return;
        };
        debug!(
            frames = video.frames,
            bytes = video.bytes,
            seconds = video.started.elapsed().as_secs_f32(),
            "video mode ended"
        );
        *out += &InstructionBuilder::new("end").arg(VIDEO_STREAM).finish();
        *out += &InstructionBuilder::new("dispose").arg(VIDEO_LAYER).finish();
        let (cols, rows) = self.tiles_of(&video.rect);
        for ty in rows {
            for tx in cols.clone() {
                let i = (ty * self.tile_cols + tx) as usize;
                self.dirty[i] = true;
                // The refresh below is lossless (not JPEG), and the area
                // has to prove busy again before the next video starts.
                self.motion[i] = 0.0;
                self.heat[i] = 0.0;
            }
        }
    }

    fn video_failed(&mut self, err: &VideoError) {
        if let Some(support) = &mut self.video_support {
            support.failures += 1;
            warn!(error = %err, failures = support.failures, "video encoder failed");
            if support.failures >= MAX_FAILURES {
                warn!("video mode disabled after repeated encoder failures");
                self.video_support = None;
            }
        }
    }

    /// Encodes the video area if it changed (always when `key` is set).
    fn video_frame(&mut self, now: Instant, key: bool, out: &mut String) -> FlushStats {
        let Some(video) = &self.video else {
            return FlushStats::default();
        };
        let rect = video.rect;
        let (cols, rows) = self.tiles_of(&rect);
        let mut touched = false;
        for ty in rows.clone() {
            for tx in cols.clone() {
                touched |= self.dirty[(ty * self.tile_cols + tx) as usize];
            }
        }
        if !touched && !key {
            return FlushStats::default();
        }
        let frame = self.video_pixels();
        let video = self.video.as_mut().expect("checked above");
        if !key && frame == video.previous {
            return FlushStats::default();
        }
        // Changed tiles stay busy, which keeps the video going.
        for ty in rows {
            for tx in cols.clone() {
                let i = (ty * self.tile_cols + tx) as usize;
                if self.dirty[i] {
                    self.motion[i] += 1.0;
                }
            }
        }
        let video = self.video.as_mut().expect("checked above");
        let encoder = video.encoder.get_mut().unwrap_or_else(|e| e.into_inner());
        let encoded = match encoder.encode(&frame, key) {
            Ok(encoded) => encoded,
            Err(err) => {
                self.stop_video(out);
                self.video_failed(&err);
                return FlushStats::default();
            }
        };
        video.previous = frame;
        video.last_change = now;
        if encoded.data.is_empty() {
            // The encoder buffers this frame; it comes out with a later one.
            return FlushStats::default();
        }
        let timestamp = now.duration_since(video.started).as_millis() as u32;
        let mut framed = Vec::with_capacity(encoded.data.len() + 9);
        encode_frame(&mut framed, timestamp, encoded.key, &encoded.data);
        video.frames += 1;
        video.bytes += framed.len();
        let b64 = base64::engine::general_purpose::STANDARD;
        for chunk in framed.chunks(BLOB_MAX_BYTES) {
            *out += &InstructionBuilder::new("blob")
                .arg(VIDEO_STREAM)
                .str_arg(&b64.encode(chunk))
                .finish();
        }
        FlushStats {
            images: 1,
            bytes: framed.len(),
            pixels: rect.area(),
            ..FlushStats::default()
        }
    }

    /// RGBA pixels of the video area at the encoded size; the padding
    /// repeats the last column / row.
    fn video_pixels(&self) -> Vec<u8> {
        let video = self.video.as_ref().expect("video active");
        let rect = video.rect;
        let mut data = Vec::with_capacity((video.width * video.height * 4) as usize);
        for y in 0..video.height {
            let row = self.segment(
                &self.pixels,
                rect.y + y.min(rect.h - 1),
                rect.x,
                rect.right(),
            );
            data.extend_from_slice(row);
            for _ in rect.w..video.width {
                data.extend_from_slice(&row[row.len() - 4..]);
            }
        }
        data
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::ImageFormats;
    use crate::video::EncodedFrame;
    use std::sync::{Arc, Mutex};

    /// (width, height, key) of every encoded frame.
    type EncodeLog = Arc<Mutex<Vec<(u32, u32, bool)>>>;

    #[test]
    fn large_areas_use_the_faster_codec() {
        let support = VideoSupport {
            codecs: vec![(VideoCodec::Av1, 1000), (VideoCodec::H265, 5000)],
            factory: Mutex::new(Box::new(|c| Err(VideoError::Unavailable(c.codec)))),
            failures: 0,
        };
        assert_eq!(support.codec_for(1000), VideoCodec::Av1);
        assert_eq!(support.codec_for(1001), VideoCodec::H265);
        // Too large for all: the fastest one.
        assert_eq!(support.codec_for(9000), VideoCodec::H265);
    }

    /// Records what it was asked to encode.
    struct FakeEncoder {
        log: EncodeLog,
        config: EncoderConfig,
    }

    impl VideoEncoder for FakeEncoder {
        fn codec(&self) -> VideoCodec {
            self.config.codec
        }

        fn encode(&mut self, rgba: &[u8], force_key: bool) -> Result<EncodedFrame, VideoError> {
            assert_eq!(
                rgba.len(),
                (self.config.width * self.config.height * 4) as usize
            );
            self.log
                .lock()
                .unwrap()
                .push((self.config.width, self.config.height, force_key));
            Ok(EncodedFrame {
                data: vec![0, 0, 0, 1, 0x65, rgba[0]],
                key: force_key,
            })
        }
    }

    fn display_with_video() -> (Display, EncodeLog) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut display = Display::new(ImageFormats { jpeg: true }, false);
        let factory_log = log.clone();
        display.enable_video(
            vec![(VideoCodec::Av1, 300 * 300), (VideoCodec::H264, u64::MAX)],
            Box::new(move |config| {
                Ok(Box::new(FakeEncoder {
                    log: factory_log.clone(),
                    config: config.clone(),
                }))
            }),
        );
        let mut out = String::new();
        display.resize(640, 480, &mut out);
        display.flush(&mut out);
        (display, log)
    }

    /// Noisy, colourful content that changes every frame.
    fn noise(seed: u32, w: u32, h: u32) -> Vec<u8> {
        let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        (0..w * h)
            .flat_map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let [r, g, b, _] = state.to_le_bytes();
                [r, g, b, 255]
            })
            .collect()
    }

    #[test]
    fn busy_area_switches_to_video_and_back() {
        let (mut display, log) = display_with_video();
        let mut started_at = None;
        for frame in 0..40u32 {
            let mut out = String::new();
            display.put(100, 70, 300, 200, &noise(frame, 300, 200));
            display.flush(&mut out);
            display.sync(&mut out);
            if out.contains("5.video") {
                started_at = Some(frame);
                // The layer covers the tile-aligned busy area.
                assert!(out.contains("4.size,1.1,3.384,3.256;"), "{out}");
                assert!(out.contains("4.move,1.1,1.0,2.64,2.64,1.1;"), "{out}");
                // 384×256 is beyond what AV1 may encode here.
                assert!(out.contains("10.video/h264;"), "{out}");
            }
            if started_at.is_some_and(|s| frame > s) {
                // No images for the video area once the video runs.
                assert!(!out.contains("3.img"), "frame {frame}: {out}");
                assert!(out.contains("4.blob,3.101,"), "frame {frame}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let started_at = started_at.expect("video mode never started");
        assert!(started_at < 20, "started late: {started_at}");
        let encoded = log.lock().unwrap().clone();
        assert!(encoded[0].2, "first frame must be a key frame");
        assert!(encoded[1..].iter().all(|f| !f.2));
        assert_eq!((encoded[0].0, encoded[0].1), (384, 256));

        // Once idle, the video ends and the area is sent losslessly.
        std::thread::sleep(IDLE + Duration::from_millis(50));
        let mut out = String::new();
        display.flush(&mut out);
        assert!(out.contains("3.end,3.101;"), "{out}");
        assert!(out.contains("7.dispose,1.1;"), "{out}");
        assert!(out.contains("9.image/png"), "{out}");
        assert!(!out.contains("10.image/jpeg"), "{out}");
        assert!(display.video.is_none());
        assert_eq!(display.pixels, display.sent);
    }

    #[test]
    fn text_like_content_stays_lossless() {
        let (mut display, log) = display_with_video();
        for frame in 0..30u32 {
            let mut out = String::new();
            // Two-colour content changing every frame (e.g. a terminal).
            let data: Vec<u8> = noise(frame, 300, 200)
                .chunks_exact(4)
                .flat_map(|p| {
                    if p[0] & 1 == 0 {
                        [0, 0, 0, 255]
                    } else {
                        [255; 4]
                    }
                })
                .collect();
            display.put(100, 70, 300, 200, &data);
            display.flush(&mut out);
            display.sync(&mut out);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(display.video.is_none());
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn rejected_stream_disables_video() {
        let (mut display, log) = display_with_video();
        let mut out = String::new();
        for frame in 0..30u32 {
            display.put(0, 0, 256, 256, &noise(frame, 256, 256));
            display.flush(&mut out);
            display.sync(&mut out);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(display.video.is_some());
        let mut out = String::new();
        display.stream_ack(VIDEO_STREAM, Status::CLIENT_BAD_TYPE, &mut out);
        assert!(out.contains("7.dispose,1.1;"));
        assert!(display.video.is_none());
        let before = log.lock().unwrap().len();
        for frame in 30..60u32 {
            display.put(0, 0, 256, 256, &noise(frame, 256, 256));
            display.flush(&mut out);
            display.sync(&mut out);
        }
        assert_eq!(log.lock().unwrap().len(), before);
        assert!(display.video.is_none());
    }
}

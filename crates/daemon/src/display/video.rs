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
//!
//! A user can ask for the whole screen instead ([`VideoMode::FullScreen`],
//! from the session menu): the video then covers the display as soon as it
//! moves, and restarts only when the display is resized.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use molcajete_protocol::video::{VideoCodec, encode_frame};
use molcajete_protocol::{InstructionBuilder, Status};
use tracing::{debug, info, warn};

use super::{Display, FlushStats, Rect, TILE};
use crate::io::Output;
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
/// Motion score at which a tile counts as busy when it is part of a large
/// area. A remote server refreshes a large picture only a few times per
/// second (Windows sent a 7 MP player 2.4 times per second, in pieces), and
/// sent that often a tile settles near 3.5, far below [`MOTION_HOT`], while
/// every refresh costs megapixels of JPEG. This score is held from about 1.4
/// updates per second.
const MOTION_WARM: f32 = 2.0;
/// Busy tiles (≈ 0.5 MP) from which an area counts as large.
const LARGE_TILES: usize = 128;
/// How long a large area must stay busy before its video starts. A page
/// that is painted three times while it loads is as busy for a moment, but
/// not for this long.
const SUSTAINED: Duration = Duration::from_millis(1000);
/// Smallest busy area worth a video stream, in tiles (≈ 25k pixels).
const MIN_TILES: usize = 6;
/// Shortest side of a video. A strip (a taskbar animation, a progress bar)
/// is cheap as images, and hardware encoders refuse frames this flat:
/// VideoToolbox fails on 1216×96.
const MIN_SIDE: u32 = 3 * TILE;
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
/// Frames in which the busy area must not grow any further before the video
/// is restarted over it: a window that is still opening or being resized
/// would otherwise restart the stream every few frames.
const SETTLE_FRAMES: u32 = 10;
/// Frames after which the video is restarted over the busy area even if it
/// keeps growing (meanwhile, the part outside the video goes as images).
const MAX_OUTGROWN_FRAMES: u32 = 60;
/// Encoder errors after which video mode is given up for the session.
const MAX_FAILURES: u32 = 3;
/// Nominal frame rate given to rate control, which spends the bitrate per
/// frame: slower content uses fewer bits per second, and content the remote
/// server updates faster uses more. Frames are not held to this rate. They
/// follow the remote server's own, and a limit at about the rate of the
/// content merged a frame whenever two of the server's fell into one
/// interval, which showed as stutter (GH-116).
pub(super) const FPS: u32 = 30;
/// Target bits per pixel and frame for H.264; the other codecs reach the
/// same quality with fewer bits.
const H264_BITS_PER_PIXEL: f32 = 0.1;
const MIN_BITRATE_KBPS: u32 = 300;
const MAX_BITRATE_KBPS: u32 = 16_000;
/// Frames after which a video looks at how many of them its encoder left
/// out to hold the bitrate.
const SKIP_WINDOW: u32 = 30;
/// Share of skipped frames (one in this many) from which the bitrate is
/// raised: content that costs more than the area's bitrate allows (noise, a
/// terminal repainting colours) otherwise plays at a fraction of its frame
/// rate.
const SKIPPED_SHARE: u32 = 4;
/// The bitrate is doubled at most this many times, to 8 times what the
/// area's size asks for (and never beyond [`MAX_BITRATE_KBPS`]).
const MAX_RAISES: u32 = 3;
/// Tiles that must change in one frame for a full-screen video to start or
/// to count as still moving: a blinking caret or a clock (one or two tiles)
/// neither starts the stream nor keeps a settled screen lossy.
const FULL_SCREEN_MIN_TILES: usize = MIN_TILES;

/// The client parameter (`argv`) that chooses the [`VideoMode`] during a
/// session, and the opcode of the instruction that tells the client which
/// mode is in use. Both are molcajete extensions: only a session with video
/// sends the instruction, and only a client that got it shows the choice.
pub const VIDEO_MODE: &str = "video-mode";

/// How a session finds what to send as video.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoMode {
    /// The largest busy area, once it qualifies (`auto`, the default).
    #[default]
    Auto,
    /// The whole display, as soon as enough of it moves (`full-screen`).
    FullScreen,
}

impl VideoMode {
    /// The mode named by a `video-mode` value; anything else (empty or
    /// unknown) is [`VideoMode::Auto`].
    pub fn from_param(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(v) if v.eq_ignore_ascii_case("full-screen") => Self::FullScreen,
            _ => Self::Auto,
        }
    }

    /// The `video-mode` value, also the `mode` label of the video metrics.
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::FullScreen => "full-screen",
        }
    }
}

// Encoders and the factory sit in a `Mutex` only to make `Display` `Sync`
// (sessions hold `&Display` across awaits); access is always through
// `&mut self` and `Mutex::get_mut`, which never locks.

pub(super) struct VideoSupport {
    /// Usable codecs, most preferred first, each with the largest frame (in
    /// pixels) its encoder handles in real time.
    codecs: Vec<(VideoCodec, u64)>,
    factory: Mutex<EncoderFactory>,
    failures: u32,
    /// Since when a large area has been busy at [`MOTION_WARM`] without a
    /// pause.
    large_since: Option<Instant>,
    /// Whether the user asked for full-screen video. An encoder failure
    /// turns it off again (the client is told).
    full_screen: bool,
    /// Whether the client still has to be told the mode.
    announce: bool,
    /// The display size for which falling back to busy areas was last
    /// logged, so that it is logged once per size.
    too_large: Option<(u32, u32)>,
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

    /// Whether some codec encodes `pixels` in real time.
    fn realtime(&self, pixels: u64) -> bool {
        self.codecs.iter().any(|(_, max)| pixels <= *max)
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
    /// Covers the whole display ([`VideoMode::FullScreen`]).
    full_screen: bool,
    started: Instant,
    last_change: Instant,
    /// The frame given to the encoder (RGBA at the encoded size, the
    /// padding repeating the last column and row). Kept up to date tile by
    /// tile, which also tells whether anything changed.
    frame: Vec<u8>,
    /// Consecutive frames in which the busy area extended beyond `rect`.
    outgrown: u32,
    /// Everything the busy area covered during those frames.
    outgrown_area: Option<Rect>,
    /// Consecutive frames in which `outgrown_area` did not grow.
    settled: u32,
    frames: u64,
    bytes: usize,
    /// How the encoder is set up (the bitrate as raised so far).
    config: EncoderConfig,
    /// Times the bitrate was doubled.
    raises: u32,
    /// Frames given to the encoder since the bitrate was last looked at,
    /// and those of them it left out.
    encoded: u32,
    skipped: u32,
    /// The next frame starts the stream anew.
    key_next: bool,
}

impl ActiveVideo {
    /// Counts a frame the encoder took (`skipped`: and left out); returns
    /// whether the bitrate should be raised, which is looked at once every
    /// [`SKIP_WINDOW`] frames.
    fn starved(&mut self, skipped: bool) -> bool {
        self.encoded += 1;
        self.skipped += u32::from(skipped);
        if self.encoded < SKIP_WINDOW {
            return false;
        }
        let starved = self.skipped * SKIPPED_SHARE >= self.encoded;
        self.encoded = 0;
        self.skipped = 0;
        starved && self.raises < MAX_RAISES && self.config.bitrate_kbps < MAX_BITRATE_KBPS
    }

    /// Records the busy area `candidate` of this frame; returns the area
    /// the video should be restarted over, if it should.
    ///
    /// The busy area has to extend beyond the video for [`REGROW_FRAMES`]
    /// and then stop growing for [`SETTLE_FRAMES`] (or keep extending
    /// beyond it for [`MAX_OUTGROWN_FRAMES`]). When it grew from the
    /// video, the new area is extended by as much again on the sides where
    /// it grew, so that a window that keeps growing needs fewer restarts.
    fn regrow(&mut self, candidate: Option<Rect>, screen: (u32, u32)) -> Option<Rect> {
        let Some(candidate) = candidate.filter(|c| !contains(&self.rect, c)) else {
            self.outgrown = 0;
            self.outgrown_area = None;
            self.settled = 0;
            return None;
        };
        self.outgrown += 1;
        match self.outgrown_area {
            Some(area) if contains(&area, &candidate) => self.settled += 1,
            Some(area) => {
                self.outgrown_area = Some(union(&area, &candidate));
                self.settled = 0;
            }
            None => {
                self.outgrown_area = Some(candidate);
                self.settled = 0;
            }
        }
        let due = (self.outgrown >= REGROW_FRAMES && self.settled >= SETTLE_FRAMES)
            || self.outgrown >= MAX_OUTGROWN_FRAMES;
        if !due {
            return None;
        }
        let area = self.outgrown_area.expect("set above");
        if !overlaps(&self.rect, &area) {
            // The busy area moved elsewhere.
            return Some(area);
        }
        let old = &self.rect;
        let left = area.x.saturating_sub(old.x.saturating_sub(area.x));
        let top = area.y.saturating_sub(old.y.saturating_sub(area.y));
        let right = area.right() + area.right().saturating_sub(old.right());
        let bottom = area.bottom() + area.bottom().saturating_sub(old.bottom());
        let (left, top) = (left / TILE * TILE, top / TILE * TILE);
        Some(Rect::new(left, top, right - left, bottom - top).clip(screen.0, screen.1))
    }
}

/// The smallest rectangle covering both.
fn union(a: &Rect, b: &Rect) -> Rect {
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    Rect::new(
        x,
        y,
        a.right().max(b.right()) - x,
        a.bottom().max(b.bottom()) - y,
    )
}

fn overlaps(a: &Rect, b: &Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
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
    /// Enables video mode: busy areas are sent as video produced by
    /// encoders from `factory`, using the first of `codecs` (codec, largest
    /// frame in pixels it encodes in real time) that suits the area. Ignored
    /// for lossless sessions. The client is told at the next flush that it
    /// may ask for the whole screen ([`Display::set_video_mode`]).
    pub fn enable_video(&mut self, codecs: Vec<(VideoCodec, u64)>, factory: EncoderFactory) {
        if self.lossless || codecs.is_empty() {
            return;
        }
        self.video_support = Some(VideoSupport {
            codecs,
            factory: Mutex::new(factory),
            failures: 0,
            large_since: None,
            full_screen: false,
            announce: true,
            too_large: None,
        });
    }

    /// The video mode in use, if video is enabled.
    pub(crate) fn video_mode(&self) -> Option<VideoMode> {
        self.video_support.as_ref().map(|s| {
            if s.full_screen {
                VideoMode::FullScreen
            } else {
                VideoMode::Auto
            }
        })
    }

    /// Switches between busy areas and the whole screen, as the client
    /// asked (`argv video-mode`), and confirms the mode in use. Ignored
    /// when the session has no video: the client was never offered it.
    ///
    /// A full-screen video starts with the next frame that changes enough
    /// of the screen; one that runs ends at the next flush, which sends the
    /// screen again losslessly.
    pub fn set_video_mode(&mut self, mode: VideoMode, out: &mut Output) {
        let Some(support) = &mut self.video_support else {
            debug!(mode = mode.label(), "video mode asked for without video");
            return;
        };
        support.full_screen = mode == VideoMode::FullScreen;
        support.too_large = None;
        info!(mode = mode.label(), "client changed the video mode");
        self.announce_video_mode(out);
    }

    /// Tells the client the video mode in use.
    fn announce_video_mode(&mut self, out: &mut Output) {
        let Some(mode) = self.video_mode() else {
            return;
        };
        if let Some(support) = &mut self.video_support {
            support.announce = false;
        }
        *out += &InstructionBuilder::new(VIDEO_MODE)
            .str_arg(mode.label())
            .finish();
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
        let full_screen = self.video_support.as_ref().is_some_and(|s| s.full_screen);
        self.video.as_ref().map(|v| {
            if v.full_screen && !full_screen {
                // Full-screen video was turned off: end it now.
                Instant::now()
            } else {
                v.last_change + IDLE
            }
        })
    }

    /// Handles an `ack` from the client: an error for the video stream means
    /// the client cannot play it, so video mode is turned off for good.
    pub fn stream_ack(&mut self, stream: i32, status: Status, out: &mut Output) {
        if stream == VIDEO_STREAM && status.is_error() && self.video.is_some() {
            warn!(
                status = status.code(),
                "client rejected the video stream; video mode disabled"
            );
            self.disable_video(out);
        }
    }

    /// The largest group of adjoining tiles with a motion score of `score`
    /// or more: its bounding box (tile-aligned, clipped) and tile count.
    ///
    /// Only one group is looked at, so that something else moving on the
    /// screen (a taskbar animation under a player) neither stretches the
    /// box nor thins it out.
    fn busy_area(&self, score: f32) -> Option<(Rect, usize)> {
        let (cols, rows) = (self.tile_cols as usize, self.tile_rows as usize);
        let mut seen = vec![false; cols * rows];
        let mut stack = Vec::new();
        // (tile count, first column, first row, end column, end row)
        let mut best: Option<(usize, usize, usize, usize, usize)> = None;
        for start in 0..cols * rows {
            if seen[start] || self.motion[start] < score {
                continue;
            }
            seen[start] = true;
            stack.push(start);
            let (mut count, mut x0, mut y0, mut x1, mut y1) = (0, cols, rows, 0, 0);
            while let Some(i) = stack.pop() {
                let (tx, ty) = (i % cols, i / cols);
                count += 1;
                x0 = x0.min(tx);
                y0 = y0.min(ty);
                x1 = x1.max(tx + 1);
                y1 = y1.max(ty + 1);
                let neighbours = [
                    (tx > 0).then(|| i - 1),
                    (tx + 1 < cols).then(|| i + 1),
                    (ty > 0).then(|| i - cols),
                    (ty + 1 < rows).then(|| i + cols),
                ];
                for n in neighbours.into_iter().flatten() {
                    if !seen[n] && self.motion[n] >= score {
                        seen[n] = true;
                        stack.push(n);
                    }
                }
            }
            if best.is_none_or(|(most, ..)| count > most) {
                best = Some((count, x0, y0, x1, y1));
            }
        }
        let (count, x0, y0, x1, y1) = best?;
        let rect = Rect::new(
            x0 as u32 * TILE,
            y0 as u32 * TILE,
            (x1 - x0) as u32 * TILE,
            (y1 - y0) as u32 * TILE,
        )
        .clip(self.width, self.height);
        Some((rect, count))
    }

    /// The busy area at `score`, if it has `min_tiles` busy tiles and the
    /// shape and content of a video.
    fn video_area(&self, score: f32, min_tiles: usize) -> Option<Rect> {
        let (rect, count) = self.busy_area(score)?;
        let tiles = (rect.w.div_ceil(TILE) * rect.h.div_ceil(TILE)) as usize;
        if count < min_tiles || (count as f32) < MIN_DENSITY * tiles as f32 {
            return None;
        }
        if rect.w < MIN_SIDE || rect.h < MIN_SIDE {
            return None;
        }
        (self.sampled_colors(&rect) > MIN_COLORS).then_some(rect)
    }

    /// The whole display, when the session streams it as one video: the
    /// user asked for it, no encoder failed since, and some codec encodes
    /// the display in real time. A display too large for that falls back to
    /// busy areas until it is resized.
    fn full_screen(&mut self) -> Option<Rect> {
        let (width, height) = (self.width, self.height);
        let support = self.video_support.as_mut()?;
        if !support.full_screen || width == 0 || height == 0 {
            return None;
        }
        let pixels = width.next_multiple_of(16) as u64 * height.next_multiple_of(16) as u64;
        if !support.realtime(pixels) {
            if support.too_large != Some((width, height)) {
                support.too_large = Some((width, height));
                info!(
                    width,
                    height,
                    reason = "no real-time encoder for the display size",
                    "full-screen video falls back to busy areas"
                );
            }
            return None;
        }
        Some(Rect::new(0, 0, width, height))
    }

    /// Whether at least `min` touched tiles differ from what the client
    /// shows, so that a full-screen video is worth starting.
    fn tiles_changed(&self, min: usize) -> bool {
        let mut found = 0;
        for ty in 0..self.tile_rows {
            for tx in 0..self.tile_cols {
                if !self.dirty[(ty * self.tile_cols + tx) as usize] {
                    continue;
                }
                let tile =
                    Rect::new(tx * TILE, ty * TILE, TILE, TILE).clip(self.width, self.height);
                let differs = (tile.y..tile.bottom()).any(|y| {
                    self.segment(&self.pixels, y, tile.x, tile.right())
                        != self.segment(&self.sent, y, tile.x, tile.right())
                });
                if differs {
                    found += 1;
                    if found >= min {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// The busy area, if it qualifies for a video stream: at once when its
    /// tiles are sent many times per second, and after [`SUSTAINED`] when it
    /// is large and sent a few times per second.
    fn video_candidate(&mut self, now: Instant) -> Option<Rect> {
        let large = self.video_area(MOTION_WARM, LARGE_TILES);
        let support = self.video_support.as_mut()?;
        let since = match large {
            Some(_) => Some(*support.large_since.get_or_insert(now)),
            None => support.large_since.take(),
        };
        let sustained = since.is_some_and(|since| now >= since + SUSTAINED);
        // The large area contains the hot one wherever both exist.
        large
            .filter(|_| sustained)
            .or_else(|| self.video_area(MOTION_HOT, MIN_TILES))
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
    pub(super) fn update_video(&mut self, out: &mut Output) -> FlushStats {
        let Some(support) = &self.video_support else {
            return FlushStats::default();
        };
        if support.announce {
            self.announce_video_mode(out);
        }
        let now = Instant::now();
        if let Some(screen) = self.full_screen() {
            return self.update_full_screen(screen, now, out);
        }
        if self.video.as_ref().is_some_and(|v| v.full_screen) {
            // Full-screen video was turned off, or the display is now too
            // large for it: busy areas from here on.
            self.stop_video(out);
            return FlushStats::default();
        }
        let candidate = self.video_candidate(now);

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
        #[cfg(test)]
        let regrow = if crate::video::LEGACY.get() {
            legacy_regrow(active, candidate)
        } else {
            active.regrow(candidate, (self.width, self.height))
        };
        #[cfg(not(test))]
        let regrow = active.regrow(candidate, (self.width, self.height));
        if let Some(rect) = regrow {
            // The busy area moved or grew: start over where it is now.
            self.stop_video(out);
            return self.start_video(rect, now, out);
        }
        self.video_frame(now, false, out)
    }

    /// Starts, feeds or stops a video over the whole display (`screen`).
    /// It starts once [`FULL_SCREEN_MIN_TILES`] tiles differ from what the
    /// client shows (at once after a resize that changed them), and ends after
    /// [`IDLE`] without such a frame, when the screen is sent again
    /// losslessly; smaller changes meanwhile still reach the video.
    fn update_full_screen(&mut self, screen: Rect, now: Instant, out: &mut Output) -> FlushStats {
        match &self.video {
            Some(active) if active.full_screen && active.rect == screen => {
                if now >= active.last_change + IDLE {
                    self.stop_video(out);
                    return FlushStats::default();
                }
                self.video_frame(now, false, out)
            }
            active => {
                if active.is_some() {
                    self.stop_video(out);
                }
                if self.tiles_changed(FULL_SCREEN_MIN_TILES) {
                    self.start_video(screen, now, out)
                } else {
                    FlushStats::default()
                }
            }
        }
    }

    fn start_video(&mut self, rect: Rect, now: Instant, out: &mut Output) -> FlushStats {
        let full_screen = self.full_screen() == Some(rect);
        let Some(support) = &mut self.video_support else {
            return FlushStats::default();
        };
        let mode = if full_screen {
            VideoMode::FullScreen
        } else {
            VideoMode::Auto
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
                self.video_failed(&err, out);
                return FlushStats::default();
            }
        };
        metrics::counter!(
            "molcajeted_video_streams_total",
            "codec" => super::metrics::codec_label(config.codec),
            "mode" => mode.label(),
        )
        .increment(1);
        info!(
            codec = ?config.codec,
            mode = mode.label(),
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
            full_screen,
            started: now,
            last_change: now,
            frame: Vec::new(),
            outgrown: 0,
            outgrown_area: None,
            settled: 0,
            frames: 0,
            bytes: 0,
            config,
            raises: 0,
            encoded: 0,
            skipped: 0,
            key_next: false,
        });
        self.video_frame(now, true, out)
    }

    /// Ends the video stream and disposes its layer. The default layer
    /// underneath is re-compared in full at this flush.
    pub(super) fn stop_video(&mut self, out: &mut Output) {
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

    fn video_failed(&mut self, err: &VideoError, out: &mut Output) {
        if let Some(support) = &mut self.video_support {
            support.failures += 1;
            metrics::counter!("molcajeted_video_failures_total").increment(1);
            warn!(error = %err, failures = support.failures, "video encoder failed");
            let full_screen = support.full_screen;
            support.full_screen = false;
            if full_screen {
                info!(
                    width = self.width,
                    height = self.height,
                    reason = "encoder failed",
                    "full-screen video falls back to busy areas"
                );
            }
            if support.failures >= MAX_FAILURES {
                warn!("video mode disabled after repeated encoder failures");
                self.disable_video(out);
            } else if full_screen {
                self.announce_video_mode(out);
            }
        }
    }

    /// Turns video off for the session and tells the client, which then
    /// stops offering the choice of mode.
    fn disable_video(&mut self, out: &mut Output) {
        self.video_support = None;
        self.stop_video(out);
        *out += &InstructionBuilder::new(VIDEO_MODE).str_arg("").finish();
    }

    /// Encodes the video area if it changed (always when `key` is set).
    fn video_frame(&mut self, now: Instant, key: bool, out: &mut Output) -> FlushStats {
        let Some(mut video) = self.video.take() else {
            return FlushStats::default();
        };
        #[cfg(test)]
        let changed = if crate::video::LEGACY.get() {
            self.legacy_changed_tiles(&mut video, key)
        } else {
            self.changed_tiles(&mut video)
        };
        #[cfg(not(test))]
        let changed = self.changed_tiles(&mut video);
        let key = key || video.key_next;
        video.key_next = false;
        self.video = Some(video);
        if changed.is_empty() && !key {
            return FlushStats::default();
        }
        // A full-screen video goes on while frames change more than a
        // caret or a clock; anything else that changed keeps it going.
        let moving = match &self.video {
            Some(v) if v.full_screen => changed.len() >= FULL_SCREEN_MIN_TILES,
            _ => true,
        };
        // Changed tiles stay busy, which keeps the video going.
        for i in changed {
            self.motion[i] += 1.0;
        }
        let video = self.video.as_mut().expect("put back above");
        let mode = if video.full_screen {
            VideoMode::FullScreen
        } else {
            VideoMode::Auto
        };
        let rect = video.rect;
        let encoder = video.encoder.get_mut().unwrap_or_else(|e| e.into_inner());
        let codec = encoder.codec();
        let frame = &video.frame;
        let (result, encode_time) = super::metrics::timed(|| encoder.encode(frame, key));
        let encoded = match result {
            Ok(encoded) => encoded,
            Err(err) => {
                self.stop_video(out);
                self.video_failed(&err, out);
                return FlushStats::default();
            }
        };
        if moving || key {
            video.last_change = now;
        }
        let starved = video.starved(encoded.data.is_empty());
        if encoded.data.is_empty() {
            // The encoder left this frame out to hold its bitrate; what it
            // shows comes with the next one.
            self.metrics.video_frame_skipped(codec);
            if starved {
                self.raise_bitrate(out);
            }
            return FlushStats::default();
        }
        let timestamp = now.duration_since(video.started).as_millis() as u32;
        let mut framed = Vec::with_capacity(encoded.data.len() + 9);
        encode_frame(&mut framed, timestamp, encoded.key, &encoded.data);
        video.frames += 1;
        video.bytes += framed.len();
        self.metrics
            .video_frame(codec, mode, encoded.key, encode_time, framed.len());
        out.blob(VIDEO_STREAM, &framed);
        if starved {
            self.raise_bitrate(out);
        }
        FlushStats {
            images: 1,
            bytes: framed.len(),
            pixels: rect.area(),
            ..FlushStats::default()
        }
    }

    /// Doubles the bitrate of the video, whose encoder leaves out too many
    /// frames at the present one: the stream ends and a new one starts on
    /// the same layer, with a new encoder and a key frame. The layer keeps
    /// what it shows meanwhile.
    fn raise_bitrate(&mut self, out: &mut Output) {
        let (Some(video), Some(support)) = (&mut self.video, &mut self.video_support) else {
            return;
        };
        // Whatever comes of it, this was one of the attempts.
        video.raises += 1;
        let config = EncoderConfig {
            bitrate_kbps: (video.config.bitrate_kbps * 2).min(MAX_BITRATE_KBPS),
            ..video.config.clone()
        };
        let factory = support.factory.get_mut().unwrap_or_else(|e| e.into_inner());
        let encoder = match factory(&config) {
            Ok(encoder) => encoder,
            Err(err) => {
                // The video goes on as it is.
                debug!(error = %err, kbps = config.bitrate_kbps, "no encoder for a higher bitrate");
                return;
            }
        };
        info!(
            codec = ?config.codec,
            width = config.width,
            height = config.height,
            kbps = config.bitrate_kbps,
            "video bitrate raised"
        );
        metrics::counter!(
            "molcajeted_video_bitrate_raises_total",
            "codec" => super::metrics::codec_label(config.codec),
        )
        .increment(1);
        *out += &InstructionBuilder::new("end").arg(VIDEO_STREAM).finish();
        *out += &InstructionBuilder::new("video")
            .arg(VIDEO_STREAM)
            .arg(VIDEO_LAYER)
            .str_arg(config.codec.mimetype())
            .finish();
        video.encoder = Mutex::new(encoder);
        video.config = config;
        video.key_next = true;
    }

    /// Brings `video.frame` up to date with the framebuffer and returns the
    /// tiles (indices) that changed and were touched since the last flush.
    ///
    /// Only touched tiles can differ, so only they are compared and copied,
    /// a run of adjacent touched tiles one pixel row at a time; once every
    /// tile of a run is known to differ, the rest of it is only copied. A
    /// new video's frame is filled in full.
    fn changed_tiles(&self, video: &mut ActiveVideo) -> Vec<usize> {
        let rect = video.rect;
        let (width, height) = (video.width as usize, video.height as usize);
        let stride = width * 4;
        let fresh = video.frame.is_empty();
        if fresh {
            video.frame = vec![0; stride * height];
        }
        let (cols, rows) = self.tiles_of(&rect);
        let mut changed = Vec::new();
        let mut copied = false;
        let mut differs = Vec::with_capacity(cols.len());
        for ty in rows {
            let row_start = (ty * self.tile_cols) as usize;
            let (y0, y1) = (ty * TILE, ((ty + 1) * TILE).min(rect.bottom()));
            let mut tx = cols.start;
            while tx < cols.end {
                if !fresh && !self.dirty[row_start + tx as usize] {
                    tx += 1;
                    continue;
                }
                let first = tx;
                while tx < cols.end && (fresh || self.dirty[row_start + tx as usize]) {
                    tx += 1;
                }
                let (x0, x1) = (first * TILE, (tx * TILE).min(rect.right()));
                let column = (x0 - rect.x) as usize * 4;
                differs.clear();
                differs.resize((tx - first) as usize, false);
                let mut known = 0;
                for y in y0..y1 {
                    let src = self.segment(&self.pixels, y, x0, x1);
                    let dst =
                        &mut video.frame[(y - rect.y) as usize * stride + column..][..src.len()];
                    if known < differs.len() {
                        if dst == src {
                            continue;
                        }
                        let tiles = src
                            .chunks(TILE as usize * 4)
                            .zip(dst.chunks(TILE as usize * 4));
                        for (seen, (s, d)) in differs.iter_mut().zip(tiles) {
                            if !*seen && s != d {
                                *seen = true;
                                known += 1;
                            }
                        }
                    }
                    dst.copy_from_slice(src);
                }
                copied |= known > 0;
                let run = row_start + first as usize..row_start + tx as usize;
                changed.extend(
                    run.zip(&differs)
                        .filter(|&(i, &seen)| seen && self.dirty[i])
                        .map(|(i, _)| i),
                );
            }
        }
        if copied {
            pad(
                &mut video.frame,
                stride,
                rect.w as usize,
                rect.h as usize,
                height,
            );
        }
        changed
    }

    /// The frame as it was prepared before GH-37 (a full copy of the area,
    /// compared with the previous one), for the benchmarks.
    #[cfg(test)]
    fn legacy_changed_tiles(&self, video: &mut ActiveVideo, key: bool) -> Vec<usize> {
        let rect = video.rect;
        let (cols, rows) = self.tiles_of(&rect);
        let dirty: Vec<usize> = rows
            .flat_map(|ty| {
                cols.clone()
                    .map(move |tx| (ty * self.tile_cols + tx) as usize)
            })
            .filter(|&i| self.dirty[i])
            .collect();
        if dirty.is_empty() && !key {
            return dirty;
        }
        let mut frame = Vec::with_capacity((video.width * video.height * 4) as usize);
        for y in 0..video.height {
            let row = self.segment(
                &self.pixels,
                rect.y + y.min(rect.h - 1),
                rect.x,
                rect.right(),
            );
            frame.extend_from_slice(row);
            for _ in rect.w..video.width {
                frame.extend_from_slice(&row[row.len() - 4..]);
            }
        }
        if frame == video.frame {
            return Vec::new();
        }
        video.frame = frame;
        dirty
    }
}

/// Fills the padding of a frame (rows of `stride` bytes, the picture
/// `w` × `h` pixels in the top left, `height` rows in all) by repeating the
/// last column and the last row.
fn pad(frame: &mut [u8], stride: usize, w: usize, h: usize, height: usize) {
    if stride > w * 4 {
        for row in frame.chunks_exact_mut(stride).take(h) {
            let (picture, padding) = row.split_at_mut(w * 4);
            let last: [u8; 4] = picture[picture.len() - 4..].try_into().expect("4 bytes");
            for px in padding.as_chunks_mut::<4>().0 {
                *px = last;
            }
        }
    }
    let (picture, padding) = frame.split_at_mut(h * stride);
    let last = &picture[picture.len() - stride..];
    for row in padding.chunks_exact_mut(stride).take(height - h) {
        row.copy_from_slice(last);
    }
}

#[cfg(test)]
fn legacy_regrow(active: &mut ActiveVideo, candidate: Option<Rect>) -> Option<Rect> {
    let outgrown = candidate.is_some_and(|c| !contains(&active.rect, &c));
    active.outgrown = if outgrown { active.outgrown + 1 } else { 0 };
    candidate.filter(|_| active.outgrown >= REGROW_FRAMES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{ImageFormats, Pacing};
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
            large_since: None,
            full_screen: false,
            announce: false,
            too_large: None,
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

    /// Leaves out every frame whose index is in `skip`, as an encoder
    /// holding its bitrate does.
    struct SkippingEncoder {
        skip: Vec<u32>,
        frames: u32,
    }

    impl VideoEncoder for SkippingEncoder {
        fn codec(&self) -> VideoCodec {
            VideoCodec::H264
        }

        fn encode(&mut self, rgba: &[u8], force_key: bool) -> Result<EncodedFrame, VideoError> {
            let index = self.frames;
            self.frames += 1;
            let data = if self.skip.contains(&index) {
                Vec::new()
            } else {
                vec![0, 0, 0, 1, 0x65, rgba[0]]
            };
            Ok(EncodedFrame {
                data,
                key: force_key,
            })
        }
    }

    #[test]
    fn a_frame_the_encoder_skips_is_left_out_and_the_video_goes_on() {
        let mut display = Display::new(
            ImageFormats {
                jpeg: true,
                zstd: false,
            },
            false,
        );
        display.enable_video(
            vec![(VideoCodec::H264, u64::MAX)],
            Box::new(|_| {
                Ok(Box::new(SkippingEncoder {
                    skip: vec![2, 3, 6],
                    frames: 0,
                }))
            }),
        );
        let mut out = Output::text();
        display.resize(640, 480, &mut out);
        display.flush(&mut out);
        let (mut starts, mut blobs, mut silent) = (0, 0, 0);
        for frame in 0..40u32 {
            let mut out = Output::text();
            display.put(100, 70, 300, 200, &noise(frame, 300, 200));
            display.flush(&mut out);
            starts += u32::from(out.contains("5.video"));
            assert!(!out.contains("3.end,3.101;"), "frame {frame}: {out}");
            if starts > 0 {
                // Nothing else is sent for the area of a skipped frame.
                assert!(!out.contains("3.img"), "frame {frame}: {out}");
                blobs += u32::from(out.contains("4.blob,3.101,"));
                silent += u32::from(!out.contains("4.blob,3.101,"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(starts, 1);
        assert_eq!(silent, 3);
        assert!(blobs > 10, "video frames: {blobs}");
        assert!(display.video.is_some());
        assert_eq!(display.video_support.as_ref().unwrap().failures, 0);
    }

    /// Leaves out every other frame while its bitrate is below `enough`.
    struct StarvedEncoder {
        config: EncoderConfig,
        enough: u32,
        frames: u32,
    }

    impl VideoEncoder for StarvedEncoder {
        fn codec(&self) -> VideoCodec {
            self.config.codec
        }

        fn encode(&mut self, rgba: &[u8], force_key: bool) -> Result<EncodedFrame, VideoError> {
            self.frames += 1;
            let key = force_key || self.frames == 1;
            let skip = self.config.bitrate_kbps < self.enough && self.frames.is_multiple_of(2);
            Ok(EncodedFrame {
                data: if skip {
                    Vec::new()
                } else {
                    vec![0, 0, 0, 1, 0x65, rgba[0]]
                },
                key: key && !skip,
            })
        }
    }

    /// Plays 150 busy frames through an encoder that needs `enough` kbps;
    /// returns the bitrates it was created with and what was sent.
    fn play_starved(enough: u32) -> (Vec<u32>, String) {
        let created = Arc::new(Mutex::new(Vec::new()));
        let mut display = Display::new(
            ImageFormats {
                jpeg: true,
                zstd: false,
            },
            false,
        );
        let log = created.clone();
        display.enable_video(
            vec![(VideoCodec::H264, u64::MAX)],
            Box::new(move |config| {
                log.lock().unwrap().push(config.bitrate_kbps);
                Ok(Box::new(StarvedEncoder {
                    config: config.clone(),
                    enough,
                    frames: 0,
                }))
            }),
        );
        let mut out = Output::text();
        display.resize(640, 480, &mut out);
        display.flush(&mut out);
        let mut sent = String::new();
        for frame in 0..150u32 {
            let mut out = Output::text();
            display.put(100, 70, 300, 200, &noise(frame, 300, 200));
            display.flush(&mut out);
            sent += &out.to_string();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(display.video.is_some());
        assert_eq!(display.video_support.as_ref().unwrap().failures, 0);
        let created = created.lock().unwrap().clone();
        (created, sent)
    }

    #[test]
    fn the_bitrate_is_raised_while_the_encoder_skips_many_frames() {
        // 384×256 asks for the smallest bitrate.
        let (created, sent) = play_starved(1000);
        assert_eq!(created, [300, 600, 1200]);
        // Each time the stream starts anew on the layer, which stays.
        assert_eq!(sent.matches("5.video,3.101,1.1,").count(), 3, "{sent}");
        assert_eq!(sent.matches("3.end,3.101;").count(), 2);
        assert!(!sent.contains("7.dispose"));
        // The area never goes back to images in between.
        let video = &sent[sent.find("5.video").expect("a video")..];
        assert!(!video.contains("3.img"));
    }

    #[test]
    fn the_bitrate_is_raised_only_so_far() {
        let (created, _) = play_starved(u32::MAX);
        assert_eq!(created, [300, 600, 1200, 2400]);
    }

    fn display_with_video() -> (Display, EncodeLog) {
        display_of(640, 480)
    }

    fn display_of(width: u32, height: u32) -> (Display, EncodeLog) {
        display_in(VideoMode::Auto, width, height)
    }

    fn display_in(mode: VideoMode, width: u32, height: u32) -> (Display, EncodeLog) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut display = Display::new(
            ImageFormats {
                jpeg: true,
                zstd: false,
            },
            false,
        );
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
        let mut out = Output::text();
        display.resize(width, height, &mut out);
        if mode == VideoMode::FullScreen {
            // Something on the screen: the first frame starts the video.
            display.set_video_mode(mode, &mut out);
            display.put(0, 0, width, height, &noise(7, width, height));
        }
        display.flush(&mut out);
        (display, log)
    }

    /// A 1280×800 display showing colourful content, whose tiles under
    /// `rect` (tile-aligned) have the motion score `score`.
    fn display_with_motion(rect: Rect, score: f32) -> Display {
        let (mut display, _log) = display_of(1280, 800);
        display.put(0, 0, 1280, 800, &noise(1, 1280, 800));
        set_motion(&mut display, rect, score);
        display
    }

    fn set_motion(display: &mut Display, rect: Rect, score: f32) {
        let (cols, rows) = display.tiles_of(&rect);
        for ty in rows {
            for tx in cols.clone() {
                display.motion[(ty * display.tile_cols + tx) as usize] = score;
            }
        }
    }

    /// The score a tile settles at when it is sent `per_second` times per
    /// second: one more each time, halved every second.
    fn settled(per_second: f32) -> f32 {
        per_second / std::f32::consts::LN_2
    }

    const SECOND: Duration = Duration::from_secs(1);

    /// What a Windows host does with a large player: every tile a few
    /// times per second, never often enough to be hot.
    #[test]
    fn a_large_area_repainted_slowly_becomes_a_video() {
        let player = Rect::new(64, 64, 1024, 512);
        let score = settled(2.4);
        assert!(score < MOTION_HOT);
        let mut display = display_with_motion(player, score);
        let t0 = Instant::now();
        assert_eq!(display.video_candidate(t0), None);
        assert_eq!(display.video_candidate(t0 + SECOND / 2), None);
        assert_eq!(display.video_candidate(t0 + SECOND), Some(player));
    }

    #[test]
    fn a_small_area_repainted_slowly_stays_images() {
        let mut display = display_with_motion(Rect::new(64, 64, 448, 256), settled(2.4));
        let t0 = Instant::now();
        assert_eq!(display.video_candidate(t0), None);
        assert_eq!(display.video_candidate(t0 + 5 * SECOND), None);
    }

    /// A page that is painted three times while it loads: as busy as a
    /// video for a moment, and a second later it is not.
    #[test]
    fn a_large_area_repainted_briefly_stays_images() {
        let page = Rect::new(64, 64, 1024, 512);
        let mut display = display_with_motion(page, 3.0);
        let t0 = Instant::now();
        assert_eq!(display.video_candidate(t0), None);
        set_motion(&mut display, page, 3.0 / 2.0);
        assert_eq!(display.video_candidate(t0 + SECOND), None);
        // Busy again later: the second counts from then.
        set_motion(&mut display, page, 3.0);
        assert_eq!(display.video_candidate(t0 + 10 * SECOND), None);
        assert_eq!(display.video_candidate(t0 + 11 * SECOND), Some(page));
    }

    #[test]
    fn a_hot_area_becomes_a_video_at_once() {
        let player = Rect::new(64, 64, 320, 256);
        let mut display = display_with_motion(player, MOTION_HOT);
        assert_eq!(display.video_candidate(Instant::now()), Some(player));
    }

    /// Something else that moves (here a strip along the bottom, wider than
    /// the player) is not part of the video and does not keep it from
    /// starting.
    #[test]
    fn the_largest_busy_group_is_the_video() {
        let player = Rect::new(64, 64, 512, 320);
        let mut display = display_with_motion(player, MOTION_HOT);
        set_motion(&mut display, Rect::new(0, 704, 1280, 64), MOTION_HOT);
        assert_eq!(display.video_candidate(Instant::now()), Some(player));
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
    fn video_frames_are_paced_like_any_other_frame() {
        let (display, _log) = display_with_video();
        let mut display = display.with_pacing(Pacing::DESKTOP);
        let mut sent = Instant::now();
        for frame in 0..40u32 {
            let mut out = Output::text();
            display.put(100, 70, 300, 200, &noise(frame, 300, 200));
            display.flush(&mut out);
            display.sync(&mut out);
            sent = Instant::now();
            // The client keeps up: no frame waits for an acknowledgement.
            display.ack(display.last_sync.unwrap(), sent);
        }
        assert!(display.video.is_some());
        // The profile's interval (60 frames per second), not the encoder's
        // nominal rate.
        assert!(display.next_frame_at() <= sent + Duration::from_millis(16));
    }

    #[test]
    fn busy_area_switches_to_video_and_back() {
        let (mut display, log) = display_with_video();
        let mut started_at = None;
        for frame in 0..40u32 {
            let mut out = Output::text();
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
        let mut out = Output::text();
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
            let mut out = Output::text();
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
    fn a_strip_stays_images() {
        let (mut display, log) = display_with_video();
        for frame in 0..30u32 {
            let mut out = Output::text();
            // Eight tiles wide, less than two high: a taskbar animation.
            display.put(64, 320, 512, 96, &noise(frame, 512, 96));
            display.flush(&mut out);
            display.sync(&mut out);
            assert!(!out.contains("5.video"), "frame {frame}: {out}");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(display.video.is_none());
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn rejected_stream_disables_video() {
        let (mut display, log) = display_with_video();
        let mut out = Output::text();
        for frame in 0..30u32 {
            display.put(0, 0, 256, 256, &noise(frame, 256, 256));
            display.flush(&mut out);
            display.sync(&mut out);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(display.video.is_some());
        let mut out = Output::text();
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

    /// The video area copied out in full, with the padding: what the
    /// encoder must be given.
    fn expected_frame(display: &Display) -> Vec<u8> {
        let video = display.video.as_ref().unwrap();
        let rect = video.rect;
        let mut data = Vec::new();
        for y in 0..video.height {
            let row = display.segment(
                &display.pixels,
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

    #[test]
    fn frame_follows_touched_tiles_and_skips_unchanged_ones() {
        let (mut display, log) = display_with_video();
        let mut out = Output::text();
        // The area ends at the screen edge, inside a macroblock: padded.
        display.resize(630, 470, &mut out);
        display.flush(&mut out);
        let rect = Rect::new(384, 320, 246, 150);
        display.put(rect.x, rect.y, rect.w, rect.h, &noise(1, rect.w, rect.h));
        display.flush(&mut out);
        display.start_video(rect, Instant::now(), &mut out);
        display.dirty.fill(false);
        assert_eq!(display.video.as_ref().unwrap().width, 256);
        assert_eq!(display.video.as_ref().unwrap().height, 160);
        assert_eq!(
            display.video.as_ref().unwrap().frame,
            expected_frame(&display)
        );
        assert_eq!(log.lock().unwrap().len(), 1);

        // A small change inside one tile.
        let tile = ((rect.y / TILE + 1) * display.tile_cols + rect.x / TILE + 1) as usize;
        let motion = display.motion.clone();
        display.put(rect.x + 70, rect.y + 70, 10, 10, &noise(2, 10, 10));
        display.flush(&mut out);
        assert_eq!(log.lock().unwrap().len(), 2);
        assert_eq!(
            display.video.as_ref().unwrap().frame,
            expected_frame(&display)
        );
        for (i, (before, after)) in motion.iter().zip(&display.motion).enumerate() {
            assert_eq!(after > before, i == tile, "tile {i}");
        }

        // The same pixels again: touched, but unchanged.
        display.put(rect.x + 70, rect.y + 70, 10, 10, &noise(2, 10, 10));
        display.flush(&mut out);
        assert_eq!(log.lock().unwrap().len(), 2);

        // Two tiles touched, only the right one changed.
        let mut data = Vec::new();
        for y in rect.y..rect.y + 10 {
            data.extend_from_slice(display.segment(&display.pixels, y, rect.x, rect.x + 64));
            data.extend_from_slice(&noise(y, 64, 1));
        }
        let motion = display.motion.clone();
        display.put(rect.x, rect.y, 128, 10, &data);
        display.flush(&mut out);
        assert_eq!(log.lock().unwrap().len(), 3);
        assert_eq!(
            display.video.as_ref().unwrap().frame,
            expected_frame(&display)
        );
        let right = ((rect.y / TILE) * display.tile_cols + rect.x / TILE + 1) as usize;
        for (i, (before, after)) in motion.iter().zip(&display.motion).enumerate() {
            assert_eq!(after > before, i == right, "tile {i}");
        }

        // The bottom right corner: the padding follows.
        display.put(620, 460, 10, 10, &noise(3, 10, 10));
        display.flush(&mut out);
        assert_eq!(log.lock().unwrap().len(), 4);
        assert_eq!(
            display.video.as_ref().unwrap().frame,
            expected_frame(&display)
        );
    }

    fn active(rect: Rect) -> ActiveVideo {
        let config = EncoderConfig {
            codec: VideoCodec::H264,
            width: rect.w.next_multiple_of(16),
            height: rect.h.next_multiple_of(16),
            fps: FPS,
            bitrate_kbps: 1000,
        };
        ActiveVideo {
            rect,
            width: config.width,
            height: config.height,
            config: config.clone(),
            raises: 0,
            encoded: 0,
            skipped: 0,
            key_next: false,
            encoder: Mutex::new(Box::new(FakeEncoder {
                log: EncodeLog::default(),
                config,
            })),
            full_screen: false,
            started: Instant::now(),
            last_change: Instant::now(),
            frame: Vec::new(),
            outgrown: 0,
            outgrown_area: None,
            settled: 0,
            frames: 0,
            bytes: 0,
        }
    }

    const SCREEN: (u32, u32) = (5120, 5120);

    #[test]
    fn video_is_not_restarted_while_the_busy_area_keeps_growing() {
        let mut video = active(Rect::new(640, 640, 640, 640));
        // One more tile row every fourth frame.
        let candidate = |frame: u32| Rect::new(640, 640, 640, 640 + (frame / 4 + 1) * TILE);
        for frame in 1..MAX_OUTGROWN_FRAMES {
            assert_eq!(
                video.regrow(Some(candidate(frame)), SCREEN),
                None,
                "frame {frame}"
            );
        }
        // Not forever: the area so far, extended by as much again below.
        let last = candidate(MAX_OUTGROWN_FRAMES);
        let rect = video.regrow(Some(last), SCREEN).unwrap();
        assert_eq!(rect, Rect::new(640, 640, 640, 2 * last.h - 640));
    }

    #[test]
    fn video_is_restarted_once_the_busy_area_settles() {
        let mut video = active(Rect::new(640, 640, 640, 640));
        // A tile more on the left and below.
        let grown = Rect::new(576, 640, 704, 704);
        for frame in 1..=SETTLE_FRAMES {
            // Edge tiles flicker in and out: still within the grown area.
            let candidate = if frame % 3 == 0 {
                Rect::new(576, 640, 640, 704)
            } else {
                grown
            };
            assert_eq!(video.regrow(Some(candidate), SCREEN), None, "frame {frame}");
        }
        // Extended by one more tile on those sides.
        let restarted = video.regrow(Some(grown), SCREEN);
        assert_eq!(restarted, Some(Rect::new(512, 640, 768, 768)));
    }

    #[test]
    fn busy_area_back_inside_the_video_resets_the_count() {
        let mut video = active(Rect::new(640, 640, 640, 640));
        let grown = Rect::new(640, 640, 704, 640);
        for _ in 0..SETTLE_FRAMES {
            assert_eq!(video.regrow(Some(grown), SCREEN), None);
        }
        assert_eq!(video.regrow(None, SCREEN), None);
        assert_eq!(
            video.regrow(Some(Rect::new(640, 640, 64, 64)), SCREEN),
            None
        );
        for _ in 0..SETTLE_FRAMES {
            assert_eq!(video.regrow(Some(grown), SCREEN), None);
        }
        assert!(video.regrow(Some(grown), SCREEN).is_some());
    }

    #[test]
    fn video_follows_a_busy_area_that_moved() {
        let mut video = active(Rect::new(0, 0, 640, 640));
        let elsewhere = Rect::new(1280, 1280, 640, 640);
        let mut restarted = None;
        for _ in 0..REGROW_FRAMES + SETTLE_FRAMES {
            restarted = restarted.or(video.regrow(Some(elsewhere), SCREEN));
        }
        assert_eq!(restarted, Some(elsewhere));
    }

    /// Starts of the video stream while a busy window grows from 832×960 to
    /// 1280×2176 over three seconds at 30 frames per second, then plays on
    /// for two seconds; frames are flushed like `Display::flush` does
    /// (without the time-based decay, so the run is deterministic).
    fn starts_while_growing(legacy: bool) -> usize {
        let (mut display, _) = display_with_video();
        let mut out = Output::text();
        display.resize(1600, 2400, &mut out);
        display.dirty.fill(false);
        let (x, y) = (64, 64);
        // Alternating content: the same noise, shifted by one pixel.
        let content = noise(1, 1280, 2176 + 1);
        crate::video::LEGACY.set(legacy);
        let mut starts = 0;
        for frame in 0..150u32 {
            let t = frame.min(90) as f32 / 90.0;
            let w = (832.0 + 448.0 * t) as u32 & !1;
            let h = (960.0 + 1216.0 * t) as u32 & !1;
            let shift = (frame % 2) as usize * 4;
            display.put(x, y, w, h, &content[shift..][..(w * h * 4) as usize]);
            let mut out = Output::text();
            display.update_video(&mut out);
            starts += out.matches("5.video,").count();
            // The image diff: tiles outside the video go as images.
            for ty in 0..display.tile_rows {
                for tx in 0..display.tile_cols {
                    let i = (ty * display.tile_cols + tx) as usize;
                    if display.dirty[i] && !display.tile_in_video(tx, ty) {
                        display.motion[i] += 1.0;
                    }
                }
            }
            display.dirty.fill(false);
        }
        crate::video::LEGACY.set(false);
        starts
    }

    /// Content of a playing video: gradients moving at different speeds
    /// over a fixed texture.
    fn movie_frame(w: u32, h: u32, index: u32) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                let texture = ((x * 7) ^ (y * 13)) as u8 & 0x1f;
                [
                    ((x + 6 * index) / 4) as u8 ^ texture,
                    ((y + 3 * index) / 5) as u8,
                    ((x + y + 9 * index) / 3) as u8,
                    255,
                ]
            })
            .collect()
    }

    /// Passes frames on and adds up the time spent in the encoder.
    struct Timed {
        inner: Box<dyn VideoEncoder>,
        spent: Arc<Mutex<Duration>>,
    }

    impl VideoEncoder for Timed {
        fn codec(&self) -> VideoCodec {
            self.inner.codec()
        }

        fn encode(&mut self, rgba: &[u8], force_key: bool) -> Result<EncodedFrame, VideoError> {
            let start = Instant::now();
            let result = self.inner.encode(rgba, force_key);
            *self.spent.lock().unwrap() += start.elapsed();
            result
        }
    }

    /// Time per frame of the video phase of a flush (preparing the frame
    /// and encoding it) with the encoders found here, before GH-37 and now:
    ///
    /// `cargo test --release -p molcajete-daemon display::video::tests::video_phase_speed -- --ignored --nocapture --test-threads=1`
    ///
    /// For each backend and area size it prints the median over the delta
    /// frames and over the key frames of the whole phase, and of the part
    /// spent in `VideoEncoder::encode` (RGBA to YUV conversion included).
    #[test]
    #[ignore = "benchmark"]
    fn video_phase_speed() {
        use crate::video::VideoEncoders;

        let encoders = VideoEncoders::detect(None);
        eprintln!("{}", encoders.describe());
        let wanted = [
            (VideoCodec::H265, "VideoToolbox"),
            (VideoCodec::H264, "VideoToolbox"),
            (VideoCodec::H264, "libx264"),
        ];
        const DELTA: usize = 30;
        const KEYS: usize = 5;
        let median = |mut times: Vec<Duration>| {
            times.sort();
            times[times.len() / 2].as_secs_f64() * 1000.0
        };
        for (codec, name) in wanted {
            let Some(backend) = encoders
                .backends_for(codec)
                .find(|b| b.name().contains(name))
                .cloned()
            else {
                eprintln!("{name} {codec:?}: not available here");
                continue;
            };
            for (w, h) in [(1920, 1080), (1344, 2176)] {
                let movie: Vec<Vec<u8>> = (0..8).map(|i| movie_frame(w, h, i)).collect();
                let mut results = Vec::new();
                for legacy in [true, false] {
                    let spent = Arc::new(Mutex::new(Duration::ZERO));
                    let mut display = Display::new(
                        ImageFormats {
                            jpeg: true,
                            zstd: false,
                        },
                        false,
                    );
                    let (backend, factory_spent) = (backend.clone(), spent.clone());
                    display.enable_video(
                        vec![(codec, u64::MAX)],
                        Box::new(move |config| {
                            Ok(Box::new(Timed {
                                inner: backend.create(config)?,
                                spent: factory_spent.clone(),
                            }))
                        }),
                    );
                    let mut out = Output::text();
                    display.resize(2560, 2304, &mut out);
                    let rect = Rect::new(64, 64, w, h);
                    display.put(rect.x, rect.y, w, h, &movie[0]);
                    display.dirty.fill(false);
                    crate::video::LEGACY.set(legacy);
                    display.start_video(rect, Instant::now(), &mut out);
                    let measure = |display: &mut Display, index: usize, key: bool| {
                        display.put(rect.x, rect.y, w, h, &movie[index % movie.len()]);
                        *spent.lock().unwrap() = Duration::ZERO;
                        let mut out = Output::text();
                        let start = Instant::now();
                        if key {
                            display.video_frame(Instant::now(), true, &mut out);
                        } else {
                            display.update_video(&mut out);
                        }
                        let total = start.elapsed();
                        assert!(display.video.is_some() && !out.is_empty());
                        display.dirty.fill(false);
                        (total, *spent.lock().unwrap())
                    };
                    let delta: Vec<_> = (1..=DELTA)
                        .map(|i| measure(&mut display, i, false))
                        .collect();
                    let keys: Vec<_> = (1..=KEYS).map(|i| measure(&mut display, i, true)).collect();
                    crate::video::LEGACY.set(false);
                    let split = |runs: Vec<(Duration, Duration)>| {
                        let (total, encode): (Vec<_>, Vec<_>) = runs.into_iter().unzip();
                        (median(total), median(encode))
                    };
                    results.push((split(delta), split(keys)));
                }
                let [(old_delta, old_key), (new_delta, new_key)] = results[..] else {
                    unreachable!()
                };
                eprintln!(
                    "{} {codec:?} {w}x{h}: delta frame {:.2} ms (encode {:.2}) before GH-37, {:.2} ms (encode {:.2}) now; \
                     key frame {:.2} ms (encode {:.2}) before, {:.2} ms (encode {:.2}) now",
                    backend.name(),
                    old_delta.0,
                    old_delta.1,
                    new_delta.0,
                    new_delta.1,
                    old_key.0,
                    old_key.1,
                    new_key.0,
                    new_key.1,
                );
            }
        }
    }

    /// Time to bring the frame for the encoder up to date after the whole
    /// area changed, and after one tile changed, before GH-37 and now:
    ///
    /// `cargo test --release -p molcajete-daemon display::video::tests::frame_prep_speed -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore = "benchmark"]
    fn frame_prep_speed() {
        for (w, h) in [(1920, 1080), (1344, 2176)] {
            let movie: Vec<Vec<u8>> = (0..2).map(|i| movie_frame(w, h, i)).collect();
            let (mut display, _) = display_with_video();
            let mut out = Output::text();
            display.resize(2560, 2304, &mut out);
            let rect = Rect::new(64, 64, w, h);
            display.put(rect.x, rect.y, w, h, &movie[0]);
            display.start_video(rect, Instant::now(), &mut out);
            let mut video = display.video.take().unwrap();
            let mut time = |legacy: bool, whole: bool| {
                let mut times: Vec<Duration> = (1..=40)
                    .map(|i| {
                        if whole {
                            display.put(rect.x, rect.y, w, h, &movie[i % 2]);
                        } else {
                            display.put(rect.x + 100, rect.y + 100, 4, 4, &noise(i as u32, 4, 4));
                        }
                        let start = Instant::now();
                        let changed = if legacy {
                            display.legacy_changed_tiles(&mut video, false)
                        } else {
                            display.changed_tiles(&mut video)
                        };
                        let elapsed = start.elapsed();
                        assert!(!changed.is_empty());
                        display.dirty.fill(false);
                        elapsed
                    })
                    .collect();
                times.sort();
                times[times.len() / 2].as_secs_f64() * 1000.0
            };
            let (old_whole, new_whole) = (time(true, true), time(false, true));
            let (old_tile, new_tile) = (time(true, false), time(false, false));
            eprintln!(
                "frame prep {w}x{h}: whole area changed {old_whole:.3} ms before GH-37, {new_whole:.3} ms now; \
                 one tile changed {old_tile:.3} ms before, {new_tile:.3} ms now (median of 40)"
            );
        }
    }

    #[test]
    fn video_mode_parameter() {
        assert_eq!(VideoMode::from_param(None), VideoMode::Auto);
        assert_eq!(VideoMode::from_param(Some("auto")), VideoMode::Auto);
        assert_eq!(VideoMode::from_param(Some("bogus")), VideoMode::Auto);
        assert_eq!(
            VideoMode::from_param(Some(" Full-Screen ")),
            VideoMode::FullScreen
        );
        assert_eq!(VideoMode::FullScreen.label(), "full-screen");
    }

    /// A flush of one frame: (instructions, encoder calls so far).
    fn frame(display: &mut Display, log: &EncodeLog) -> (String, usize) {
        let mut out = Output::text();
        display.flush(&mut out);
        display.sync(&mut out);
        (out.to_string(), log.lock().unwrap().len())
    }

    #[test]
    fn full_screen_video_covers_the_display_from_the_first_frame() {
        let mut out = Output::text();
        let (mut display, log) = display_in(VideoMode::FullScreen, 1, 1);
        display.resize(630, 470, &mut out);
        display.put(0, 0, 630, 470, &noise(1, 630, 470));
        let (out, _) = frame(&mut display, &log);
        assert!(out.contains("4.size,1.1,3.640,3.480;"), "{out}");
        assert!(out.contains("4.move,1.1,1.0,1.0,1.0,1.1;"), "{out}");
        // Not AV1: 640×480 is beyond its real-time limit here.
        assert!(out.contains("10.video/h264;"), "{out}");
        assert!(!out.contains("3.img"), "{out}");
        let video = display.video.as_ref().unwrap();
        assert!(video.full_screen);
        assert_eq!(video.rect, Rect::new(0, 0, 630, 470));
        assert_eq!(log.lock().unwrap()[0], (640, 480, true));

        // Text-like content moving around: still the same stream, no images.
        for i in 0..20u32 {
            let data = vec![if i % 2 == 0 { 0 } else { 255 }; 200 * 30 * 4];
            display.put(10 + 20 * i, 10 + 15 * i, 200, 30, &data);
            let (out, _) = frame(&mut display, &log);
            assert!(!out.contains("5.video"), "frame {i}: {out}");
            assert!(!out.contains("3.img") && !out.contains("4.copy"), "{out}");
        }
        assert_eq!(log.lock().unwrap().len(), 21);
        assert!(log.lock().unwrap()[1..].iter().all(|f| !f.2));
    }

    #[test]
    fn full_screen_video_restarts_on_resize_only() {
        let (mut display, log) = display_in(VideoMode::FullScreen, 640, 480);
        assert!(display.video.as_ref().unwrap().full_screen);
        let mut out = Output::text();
        display.resize(800, 600, &mut out);
        assert!(out.to_string().contains("7.dispose,1.1;"));
        assert!(display.video.is_none());
        let (out, encoded) = frame(&mut display, &log);
        // The resize changed every tile: the new size starts at once.
        assert!(out.contains("4.size,1.1,3.800,3.608;"), "{out}");
        assert_eq!(log.lock().unwrap()[encoded - 1], (800, 608, true));
    }

    #[test]
    fn a_settled_full_screen_ends_losslessly_and_a_caret_does_not_restart_it() {
        let (mut display, log) = display_in(VideoMode::FullScreen, 640, 480);
        display.put(0, 0, 640, 480, &noise(1, 640, 480));
        frame(&mut display, &log);
        // A blinking caret: one tile, every 100 ms. It reaches the video,
        // but does not keep it going.
        let caret = |display: &mut Display, on: bool| {
            display.put(100, 100, 2, 16, &[if on { 255 } else { 0 }; 2 * 16 * 4]);
        };
        let started = Instant::now();
        let mut blink = false;
        while display.video.is_some() {
            assert!(started.elapsed() < 3 * IDLE, "the video never ended");
            blink = !blink;
            caret(&mut display, blink);
            frame(&mut display, &log);
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(started.elapsed() >= IDLE);
        // Ended losslessly: the client shows exactly the framebuffer.
        assert_eq!(display.pixels, display.sent);
        assert!(display.lossy.iter().all(|l| !l));
        // The caret goes on as images, without a new stream.
        let before = log.lock().unwrap().len();
        for _ in 0..5 {
            blink = !blink;
            caret(&mut display, blink);
            let (out, encoded) = frame(&mut display, &log);
            assert!(!out.contains("5.video"), "{out}");
            assert!(out.contains("3.img") || out.contains("5.cfill"), "{out}");
            assert_eq!(encoded, before);
        }
        // Motion over a larger part starts it again, with a key frame.
        display.put(0, 0, 320, 192, &noise(2, 320, 192));
        let (out, encoded) = frame(&mut display, &log);
        assert!(out.contains("5.video"), "{out}");
        assert_eq!(log.lock().unwrap()[encoded - 1], (640, 480, true));
    }

    #[test]
    fn full_screen_falls_back_to_busy_areas_when_too_large() {
        let log = EncodeLog::default();
        let mut display = Display::new(ImageFormats::default(), false);
        let factory_log = log.clone();
        display.enable_video(
            vec![(VideoCodec::Av1, 1280 * 736)],
            Box::new(move |config| {
                Ok(Box::new(FakeEncoder {
                    log: factory_log.clone(),
                    config: config.clone(),
                }))
            }),
        );
        display.set_video_mode(VideoMode::FullScreen, &mut Output::text());
        let mut out = Output::text();
        display.resize(1920, 1080, &mut out);
        let (out, encoded) = frame(&mut display, &log);
        assert!(!out.contains("5.video"), "{out}");
        assert_eq!(encoded, 0);
        // Busy areas still become videos.
        for i in 0..20u32 {
            display.put(64, 64, 320, 256, &noise(i, 320, 256));
            frame(&mut display, &log);
        }
        let video = display.video.as_ref().expect("a video over the busy area");
        assert!(!video.full_screen);
        assert_eq!(video.rect, Rect::new(64, 64, 320, 256));
        // A display the encoder handles goes full screen again.
        let mut out = Output::text();
        display.resize(1280, 720, &mut out);
        let (out, _) = frame(&mut display, &log);
        assert!(out.contains("4.size,1.1,4.1280,3.720;"), "{out}");
        assert!(display.video.as_ref().unwrap().full_screen);
    }

    #[test]
    fn full_screen_falls_back_to_busy_areas_when_the_encoder_fails() {
        let log = EncodeLog::default();
        let mut display = Display::new(ImageFormats::default(), false);
        let factory_log = log.clone();
        display.enable_video(
            vec![(VideoCodec::H264, u64::MAX)],
            Box::new(move |config| {
                if config.width == 640 {
                    // A hardware encoder refusing the frame size.
                    return Err(VideoError::Unavailable(config.codec));
                }
                Ok(Box::new(FakeEncoder {
                    log: factory_log.clone(),
                    config: config.clone(),
                }))
            }),
        );
        display.set_video_mode(VideoMode::FullScreen, &mut Output::text());
        let mut out = Output::text();
        display.resize(640, 480, &mut out);
        display.put(0, 0, 640, 480, &noise(1, 640, 480));
        let (out, _) = frame(&mut display, &log);
        assert!(!out.contains("5.video"), "{out}");
        assert!(out.contains("3.img"), "{out}");
        // The client is told: its menu shows the switch off again.
        assert!(out.contains("10.video-mode,4.auto;"), "{out}");
        assert!(display.video.is_none());
        // A busy area still becomes a video of its own. Its tiles are made
        // hot directly: their score halves every second of wall-clock time,
        // so painting them in a loop made the test depend on how fast the
        // machine encodes the frames. The decay starts over now, and the
        // score is twice what is needed, so a slow machine still gets there.
        let player = Rect::new(64, 64, 320, 256);
        display.last_decay = Instant::now();
        set_motion(&mut display, player, 2.0 * MOTION_HOT);
        display.put(64, 64, 320, 256, &noise(21, 320, 256));
        frame(&mut display, &log);
        let video = display.video.as_ref().expect("busy area video");
        assert!(!video.full_screen);
        assert_eq!(video.rect, player);
        assert_eq!(display.video_mode(), Some(VideoMode::Auto));
    }

    #[test]
    fn lossless_sessions_get_no_full_screen_video() {
        let mut display = Display::new(ImageFormats::default(), true);
        display.enable_video(
            vec![(VideoCodec::H264, u64::MAX)],
            Box::new(|c| Err(VideoError::Unavailable(c.codec))),
        );
        let mut out = Output::text();
        display.set_video_mode(VideoMode::FullScreen, &mut out);
        assert_eq!(display.video_mode(), None);
        assert!(out.is_empty());
    }

    #[test]
    fn the_client_is_told_the_video_mode() {
        let (mut display, log) = display_of(640, 480);
        // `display_of` flushed once: that frame said so.
        let (out, _) = frame(&mut display, &log);
        assert!(!out.contains("10.video-mode"), "{out}");
        let mut out = Output::text();
        display.set_video_mode(VideoMode::FullScreen, &mut out);
        assert_eq!(out.to_string(), "10.video-mode,11.full-screen;");
        // Without video there is nothing to choose.
        let mut images_only = Display::new(ImageFormats::default(), false);
        let mut out = Output::text();
        images_only.resize(64, 64, &mut out);
        images_only.flush(&mut out);
        images_only.set_video_mode(VideoMode::FullScreen, &mut out);
        assert!(!out.to_string().contains("video-mode"), "{out}");
    }

    #[test]
    fn turning_full_screen_off_ends_its_video_losslessly() {
        let (mut display, log) = display_in(VideoMode::FullScreen, 640, 480);
        assert!(display.video.as_ref().unwrap().full_screen);
        let mut out = Output::text();
        display.set_video_mode(VideoMode::Auto, &mut out);
        assert!(display.idle_deadline().unwrap() <= Instant::now() + Duration::from_millis(200));
        let (out, _) = frame(&mut display, &log);
        assert!(out.contains("7.dispose,1.1;"), "{out}");
        assert!(display.video.is_none());
        assert_eq!(display.pixels, display.sent);
        assert!(display.lossy.iter().all(|l| !l));
    }

    #[test]
    fn a_rejected_stream_withdraws_the_choice_of_mode() {
        let (mut display, _log) = display_in(VideoMode::FullScreen, 640, 480);
        let mut out = Output::text();
        display.stream_ack(VIDEO_STREAM, Status::CLIENT_BAD_TYPE, &mut out);
        assert!(out.to_string().ends_with("10.video-mode,0.;"), "{out}");
        assert_eq!(display.video_mode(), None);
    }

    #[test]
    fn growing_window_restarts_the_video_rarely() {
        let before = starts_while_growing(true);
        let now = starts_while_growing(false);
        eprintln!("video starts while a window grows: before GH-37 {before}, now {now}");
        assert!(now <= 2, "{now} starts");
        assert!(now < before, "{now} starts, {before} before GH-37");
    }
}

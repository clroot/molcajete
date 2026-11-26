//! Instrumentation of the display pipeline.
//!
//! Every frame goes through the same steps whatever the remote protocol:
//! diff against the mirror, scroll / move detection, image or video
//! encoding, then the client draws it and acknowledges the `sync`. Each step
//! is timed into a Prometheus histogram (no-ops unless the telemetry listener
//! is enabled), and per-session totals are logged when the display goes
//! away, so a slow session can be found in the logs afterwards.

use std::time::{Duration, Instant};

use metrics::{Counter, Histogram, counter, histogram};
use molcajete_protocol::video::VideoCodec;
use tracing::info;

use super::video::VideoMode;

/// Image encodings timed separately.
#[derive(Clone, Copy)]
pub(super) enum ImageKind {
    Png,
    Jpeg,
    /// `molcajete_protocol::zimage`.
    Zstd,
}

/// Steps of [`super::Display::flush`].
#[derive(Clone, Copy)]
pub(super) enum Phase {
    Video,
    Scroll,
    Moves,
    Pixels,
    Refine,
}

const PHASES: usize = 5;

struct ImageMetrics {
    encode: Histogram,
    size: Histogram,
    bytes: Counter,
    pixels: Counter,
}

impl ImageMetrics {
    fn new(protocol: &'static str, format: &'static str) -> Self {
        let labels = [("protocol", protocol), ("format", format)];
        Self {
            encode: histogram!("molcajeted_image_encode_seconds", &labels),
            size: histogram!("molcajeted_image_encoded_bytes", &labels),
            bytes: counter!("molcajeted_image_bytes_total", &labels),
            pixels: counter!("molcajeted_image_pixels_total", &labels),
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Timing {
    count: u64,
    total: Duration,
    max: Duration,
}

impl Timing {
    fn add(&mut self, d: Duration) {
        self.count += 1;
        self.total += d;
        self.max = self.max.max(d);
    }

    fn avg_ms(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total.as_secs_f64() * 1000.0 / self.count as f64
        }
    }

    fn max_ms(&self) -> f64 {
        self.max.as_secs_f64() * 1000.0
    }
}

/// Frames further apart than this do not belong to one run of frames (the
/// screen stood still in between), and their interval is not recorded.
const FRAME_RUN_GAP: Duration = Duration::from_secs(1);

/// Totals over the life of one display (one session).
#[derive(Default)]
struct Totals {
    flush: Timing,
    phases: [Timing; PHASES],
    scroll_copies: u64,
    move_copies: u64,
    frames: u64,
    frame_bytes: u64,
    frame_images_max: u32,
    ack: Timing,
    ack_wait: Timing,
    png: Timing,
    png_bytes: u64,
    jpeg: Timing,
    jpeg_bytes: u64,
    zstd: Timing,
    zstd_bytes: u64,
    video: Timing,
    video_bytes: u64,
}

pub(super) struct DisplayMetrics {
    protocol: &'static str,
    started: Instant,
    flush: Histogram,
    phases: [Histogram; PHASES],
    /// Copies found by scroll and by move detection.
    scroll_copies: Counter,
    move_copies: Counter,
    frames: Counter,
    frame_bytes: Histogram,
    frame_images: Histogram,
    frame_interval: Histogram,
    client_ack: Histogram,
    client_apply: Histogram,
    ack_wait: Histogram,
    unacked: Histogram,
    png: ImageMetrics,
    jpeg: ImageMetrics,
    zstd: ImageMetrics,
    totals: Totals,
}

impl DisplayMetrics {
    /// Records nothing (until [`super::Display::with_protocol`] is called), so
    /// that no unlabelled series are registered.
    pub(super) fn disabled() -> Self {
        let image = || ImageMetrics {
            encode: Histogram::noop(),
            size: Histogram::noop(),
            bytes: Counter::noop(),
            pixels: Counter::noop(),
        };
        Self {
            protocol: "",
            started: Instant::now(),
            flush: Histogram::noop(),
            phases: std::array::from_fn(|_| Histogram::noop()),
            scroll_copies: Counter::noop(),
            move_copies: Counter::noop(),
            frames: Counter::noop(),
            frame_bytes: Histogram::noop(),
            frame_images: Histogram::noop(),
            frame_interval: Histogram::noop(),
            client_ack: Histogram::noop(),
            client_apply: Histogram::noop(),
            ack_wait: Histogram::noop(),
            unacked: Histogram::noop(),
            png: image(),
            jpeg: image(),
            zstd: image(),
            totals: Totals::default(),
        }
    }

    pub(super) fn new(protocol: &'static str) -> Self {
        let labels = [("protocol", protocol)];
        let phase = |name: &'static str| {
            histogram!(
                "molcajeted_flush_phase_seconds",
                "protocol" => protocol,
                "phase" => name,
            )
        };
        let copies = |name: &'static str| {
            counter!(
                "molcajeted_copies_total",
                "protocol" => protocol,
                "phase" => name,
            )
        };
        Self {
            protocol,
            started: Instant::now(),
            flush: histogram!("molcajeted_frame_flush_seconds", &labels),
            phases: [
                phase("video"),
                phase("scroll"),
                phase("moves"),
                phase("pixels"),
                phase("refine"),
            ],
            scroll_copies: copies("scroll"),
            move_copies: copies("moves"),
            frames: counter!("molcajeted_frames_total", &labels),
            frame_bytes: histogram!("molcajeted_frame_encoded_bytes", &labels),
            frame_images: histogram!("molcajeted_frame_images", &labels),
            frame_interval: histogram!("molcajeted_frame_interval_seconds", &labels),
            client_ack: histogram!("molcajeted_client_ack_seconds", &labels),
            client_apply: histogram!("molcajeted_client_apply_seconds", &labels),
            ack_wait: histogram!("molcajeted_client_ack_wait_seconds", &labels),
            unacked: histogram!("molcajeted_unacked_frames", &labels),
            png: ImageMetrics::new(protocol, "png"),
            jpeg: ImageMetrics::new(protocol, "jpeg"),
            zstd: ImageMetrics::new(protocol, "zstd"),
            totals: Totals::default(),
        }
    }

    pub(super) fn phase(&mut self, phase: Phase, elapsed: Duration) {
        self.phases[phase as usize].record(elapsed);
        self.totals.phases[phase as usize].add(elapsed);
    }

    /// `copies` copies were found by the detection of `phase` (scroll or
    /// moves) in one flush.
    pub(super) fn copies(&mut self, phase: Phase, copies: u32) {
        let (counter, total) = match phase {
            Phase::Scroll => (&self.scroll_copies, &mut self.totals.scroll_copies),
            Phase::Moves => (&self.move_copies, &mut self.totals.move_copies),
            _ => return,
        };
        counter.increment(copies as u64);
        *total += copies as u64;
    }

    /// Total time spent in `phase` so far.
    #[cfg(test)]
    pub(super) fn phase_total(&self, phase: Phase) -> Duration {
        self.totals.phases[phase as usize].total
    }

    /// Lossless images (PNG and zstd) encoded so far as (count, total
    /// time).
    #[cfg(test)]
    pub(super) fn lossless_total(&self) -> (u64, Duration) {
        let t = &self.totals;
        (t.png.count + t.zstd.count, t.png.total + t.zstd.total)
    }

    /// zstd images encoded so far.
    #[cfg(test)]
    pub(super) fn zstd_count(&self) -> u64 {
        self.totals.zstd.count
    }

    /// Longest time an acknowledgement waited for the session so far.
    #[cfg(test)]
    pub(super) fn ack_wait_max(&self) -> Duration {
        self.totals.ack_wait.max
    }

    pub(super) fn flushed(&mut self, elapsed: Duration) {
        self.flush.record(elapsed);
        self.totals.flush.add(elapsed);
    }

    /// A frame of `images` images was closed with `sync`, `interval` after
    /// the frame before it.
    pub(super) fn frame(
        &mut self,
        encoded_bytes: usize,
        images: u32,
        unacked: usize,
        interval: Option<Duration>,
    ) {
        self.frames.increment(1);
        // Within a run of frames only: how evenly they follow each other.
        // A longer gap is the screen standing still.
        if let Some(interval) = interval.filter(|&i| i < FRAME_RUN_GAP) {
            self.frame_interval.record(interval);
        }
        self.frame_bytes.record(encoded_bytes as f64);
        self.frame_images.record(images as f64);
        self.totals.frame_images_max = self.totals.frame_images_max.max(images);
        self.unacked.record(unacked as f64);
        self.totals.frames += 1;
        self.totals.frame_bytes += encoded_bytes as u64;
    }

    /// `elapsed`: from sending a frame to its acknowledgement arriving
    /// (network both ways plus the client's decoding and drawing). `waited`:
    /// how long the acknowledgement then sat unread, because the session
    /// was busy. `apply`: how long the client said it took to apply the
    /// frame, if it reports that.
    pub(super) fn acked(&mut self, elapsed: Duration, waited: Duration, apply: Option<Duration>) {
        if let Some(apply) = apply {
            self.client_apply.record(apply);
        }
        self.client_ack.record(elapsed);
        self.ack_wait.record(waited);
        self.totals.ack.add(elapsed);
        self.totals.ack_wait.add(waited);
    }

    pub(super) fn image(&mut self, kind: ImageKind, elapsed: Duration, bytes: usize, pixels: u64) {
        let (m, timing, total) = match kind {
            ImageKind::Png => (&self.png, &mut self.totals.png, &mut self.totals.png_bytes),
            ImageKind::Jpeg => (
                &self.jpeg,
                &mut self.totals.jpeg,
                &mut self.totals.jpeg_bytes,
            ),
            ImageKind::Zstd => (
                &self.zstd,
                &mut self.totals.zstd,
                &mut self.totals.zstd_bytes,
            ),
        };
        m.encode.record(elapsed);
        m.size.record(bytes as f64);
        m.bytes.increment(bytes as u64);
        m.pixels.increment(pixels);
        timing.add(elapsed);
        *total += bytes as u64;
    }

    pub(super) fn video_frame(
        &mut self,
        codec: VideoCodec,
        mode: VideoMode,
        key: bool,
        elapsed: Duration,
        bytes: usize,
    ) {
        let codec = codec_label(codec);
        let mode = mode.label();
        let key = if key { "true" } else { "false" };
        histogram!("molcajeted_video_encode_seconds", "codec" => codec, "mode" => mode, "key" => key)
            .record(elapsed);
        histogram!("molcajeted_video_frame_bytes", "codec" => codec, "mode" => mode, "key" => key)
            .record(bytes as f64);
        self.totals.video.add(elapsed);
        self.totals.video_bytes += bytes as u64;
    }

    /// A frame the encoder left out to hold its bitrate.
    pub(super) fn video_frame_skipped(&mut self, codec: VideoCodec) {
        counter!("molcajeted_video_frames_skipped_total", "codec" => codec_label(codec))
            .increment(1);
    }

    /// Logs the session totals (called when the display is dropped).
    pub(super) fn summary(&self) {
        let t = &self.totals;
        if t.frames == 0 || self.protocol.is_empty() {
            return;
        }
        let seconds = self.started.elapsed().as_secs_f64();
        info!(
            protocol = self.protocol,
            duration_s = seconds,
            frames = t.frames,
            fps_avg = t.frames as f64 / seconds.max(1e-3),
            encoded_bytes = t.frame_bytes,
            frame_images_max = t.frame_images_max,
            flush_ms_avg = t.flush.avg_ms(),
            flush_ms_max = t.flush.max_ms(),
            scroll_ms_avg = t.phases[Phase::Scroll as usize].avg_ms(),
            moves_ms_avg = t.phases[Phase::Moves as usize].avg_ms(),
            scroll_copies = t.scroll_copies,
            move_copies = t.move_copies,
            pixels_ms_avg = t.phases[Phase::Pixels as usize].avg_ms(),
            client_ack_ms_avg = t.ack.avg_ms(),
            client_ack_ms_max = t.ack.max_ms(),
            client_ack_wait_ms_avg = t.ack_wait.avg_ms(),
            client_ack_wait_ms_max = t.ack_wait.max_ms(),
            png_images = t.png.count,
            png_bytes = t.png_bytes,
            png_ms_avg = t.png.avg_ms(),
            png_ms_max = t.png.max_ms(),
            jpeg_images = t.jpeg.count,
            jpeg_bytes = t.jpeg_bytes,
            jpeg_ms_avg = t.jpeg.avg_ms(),
            jpeg_ms_max = t.jpeg.max_ms(),
            zstd_images = t.zstd.count,
            zstd_bytes = t.zstd_bytes,
            zstd_ms_avg = t.zstd.avg_ms(),
            zstd_ms_max = t.zstd.max_ms(),
            video_frames = t.video.count,
            video_bytes = t.video_bytes,
            video_encode_ms_avg = t.video.avg_ms(),
            video_encode_ms_max = t.video.max_ms(),
            "display summary"
        );
    }
}

pub(super) fn codec_label(codec: VideoCodec) -> &'static str {
    codec.name()
}

/// Measures `f` and returns its result with the elapsed time.
pub(super) fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let value = f();
    (value, start.elapsed())
}

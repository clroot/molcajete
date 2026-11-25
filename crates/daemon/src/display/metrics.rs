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

/// Totals over the life of one display (one session).
#[derive(Default)]
struct Totals {
    flush: Timing,
    phases: [Timing; PHASES],
    frames: u64,
    frame_bytes: u64,
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
    frames: Counter,
    frame_bytes: Histogram,
    client_ack: Histogram,
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
            frames: Counter::noop(),
            frame_bytes: Histogram::noop(),
            client_ack: Histogram::noop(),
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
            frames: counter!("molcajeted_frames_total", &labels),
            frame_bytes: histogram!("molcajeted_frame_encoded_bytes", &labels),
            client_ack: histogram!("molcajeted_client_ack_seconds", &labels),
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

    pub(super) fn flushed(&mut self, elapsed: Duration) {
        self.flush.record(elapsed);
        self.totals.flush.add(elapsed);
    }

    /// A frame was closed with `sync`.
    pub(super) fn frame(&mut self, encoded_bytes: usize, unacked: usize) {
        self.frames.increment(1);
        self.frame_bytes.record(encoded_bytes as f64);
        self.unacked.record(unacked as f64);
        self.totals.frames += 1;
        self.totals.frame_bytes += encoded_bytes as u64;
    }

    /// `elapsed`: from sending a frame to its acknowledgement arriving
    /// (network both ways plus the client's decoding and drawing). `waited`:
    /// how long the acknowledgement then sat unread, because the session
    /// was busy.
    pub(super) fn acked(&mut self, elapsed: Duration, waited: Duration) {
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
        key: bool,
        elapsed: Duration,
        bytes: usize,
    ) {
        let codec = codec_label(codec);
        let key = if key { "true" } else { "false" };
        histogram!("molcajeted_video_encode_seconds", "codec" => codec, "key" => key)
            .record(elapsed);
        histogram!("molcajeted_video_frame_bytes", "codec" => codec, "key" => key)
            .record(bytes as f64);
        self.totals.video.add(elapsed);
        self.totals.video_bytes += bytes as u64;
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
            flush_ms_avg = t.flush.avg_ms(),
            flush_ms_max = t.flush.max_ms(),
            scroll_ms_avg = t.phases[Phase::Scroll as usize].avg_ms(),
            moves_ms_avg = t.phases[Phase::Moves as usize].avg_ms(),
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
    match codec {
        VideoCodec::Av1 => "av1",
        VideoCodec::H265 => "h265",
        VideoCodec::H264 => "h264",
    }
}

/// Measures `f` and returns its result with the elapsed time.
pub(super) fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let value = f();
    (value, start.elapsed())
}

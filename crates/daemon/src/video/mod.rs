//! Low-latency video encoders for busy screen regions.
//!
//! Backends, in order of preference for a codec:
//! 1. hardware encoders: VideoToolbox on macOS (H.265, H.264; see
//!    [`videotoolbox`]), and through ffmpeg NVENC, QSV, VA-API and AMF, when
//!    `ffmpeg -encoders` lists them; each must pass a trial encode;
//! 2. software encoders through ffmpeg: libsvtav1 (AV1), libx265 (H.265),
//!    libx264 (H.264);
//! 3. the built-in OpenH264 encoder (cargo feature `openh264`).
//!
//! ffmpeg runs as a child process per encoder (see [`ffmpeg`]); nothing is
//! linked, so GPL encoders are fine. All backends take RGBA, convert it to
//! BT.709 limited-range 4:2:0 (signalled in the bitstream) and return one
//! access unit / temporal unit per frame, without lookahead or reordering.
//!
//! The other direction, decoding the H.264 an RDP server sends over its
//! graphics pipeline, is in [`decode`].

mod avi;
mod bitstream;
#[cfg(feature = "openh264")]
mod builtin;
mod decode;
mod ffmpeg;
#[cfg(target_os = "macos")]
mod videotoolbox;
mod yuv;

#[cfg(test)]
mod tests;

/// Set on a thread to prepare video frames there as before GH-37, for
/// comparisons in benchmarks and simulations.
#[cfg(test)]
pub(crate) use yuv::reference::LEGACY;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

pub use decode::{PipelineDecoder, h264_decoder};
pub use molcajete_protocol::video::VideoCodec;

#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub codec: VideoCodec,
    /// Width in pixels; must be even (4:2:0).
    pub width: u32,
    /// Height in pixels; must be even (4:2:0).
    pub height: u32,
    /// Nominal frame rate, e.g. 30.
    pub fps: u32,
    /// Target bitrate; encoders may treat it as a cap.
    pub bitrate_kbps: u32,
}

pub struct EncodedFrame {
    /// One access unit (H.264/H.265 Annex B incl. start codes) or one AV1
    /// temporal unit (low-overhead OBUs). Key frames carry the codec
    /// configuration in-band. Empty when the encoder left the frame out to
    /// hold its bitrate: the stream goes on with the next frame.
    pub data: Vec<u8>,
    pub key: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum VideoError {
    #[error("no encoder available for {0:?}")]
    Unavailable(VideoCodec),
    #[error("invalid encoder configuration: {0}")]
    InvalidConfig(String),
    #[error("frame has {actual} bytes, expected {expected}")]
    FrameSize { expected: usize, actual: usize },
    #[error("video encoder I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("video encoder failed: {0}")]
    Encoder(String),
}

pub trait VideoEncoder: Send {
    #[allow(dead_code)] // part of the interface even where callers know the codec
    fn codec(&self) -> VideoCodec;
    /// Encodes one RGBA frame (width*height*4 bytes, top row first) and
    /// returns its encoded form. Low latency: one output per input, no
    /// lookahead, no B-frames/frame reordering. `force_key` requests an IDR /
    /// key frame. The first frame is always a key frame.
    ///
    /// After an error the encoder starts over: the next frame is a key frame.
    fn encode(&mut self, rgba: &[u8], force_key: bool) -> Result<EncodedFrame, VideoError>;
}

/// Checks the size of an RGBA frame against the configuration.
fn check_frame(config: &EncoderConfig, rgba: &[u8]) -> Result<(), VideoError> {
    let expected = config.width as usize * config.height as usize * 4;
    if rgba.len() != expected {
        return Err(VideoError::FrameSize {
            expected,
            actual: rgba.len(),
        });
    }
    Ok(())
}

fn validate(config: &EncoderConfig) -> Result<(), VideoError> {
    let (w, h) = (config.width, config.height);
    if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err(VideoError::InvalidConfig(format!(
            "{w}x{h}: dimensions must be even and non-zero"
        )));
    }
    if config.fps == 0 || config.bitrate_kbps == 0 {
        return Err(VideoError::InvalidConfig(
            "frame rate and bitrate must be non-zero".into(),
        ));
    }
    Ok(())
}

fn codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::Av1 => "AV1",
        VideoCodec::H265 => "H.265",
        VideoCodec::H264 => "H.264",
    }
}

/// One way of encoding a codec.
#[derive(Clone, Debug)]
pub(crate) enum Backend {
    Ffmpeg {
        path: PathBuf,
        profile: &'static ffmpeg::Profile,
    },
    #[cfg(target_os = "macos")]
    VideoToolbox(VideoCodec),
    #[cfg(feature = "openh264")]
    OpenH264,
}

impl Backend {
    pub(crate) fn codec(&self) -> VideoCodec {
        match self {
            Self::Ffmpeg { profile, .. } => profile.codec,
            #[cfg(target_os = "macos")]
            Self::VideoToolbox(codec) => *codec,
            #[cfg(feature = "openh264")]
            Self::OpenH264 => VideoCodec::H264,
        }
    }

    /// Lower is preferred: hardware, ffmpeg software, built-in.
    fn rank(&self) -> u8 {
        match self {
            Self::Ffmpeg { profile, .. } if profile.hardware => 0,
            #[cfg(target_os = "macos")]
            Self::VideoToolbox(_) => 0,
            Self::Ffmpeg { .. } => 1,
            #[cfg(feature = "openh264")]
            Self::OpenH264 => 2,
        }
    }

    pub(crate) fn name(&self) -> String {
        match self {
            Self::Ffmpeg { profile, .. } if profile.hardware => {
                format!("ffmpeg {} (hardware)", profile.name)
            }
            Self::Ffmpeg { profile, .. } => format!("ffmpeg {}", profile.name),
            #[cfg(target_os = "macos")]
            Self::VideoToolbox(_) => "VideoToolbox (hardware)".into(),
            #[cfg(feature = "openh264")]
            Self::OpenH264 => "built-in OpenH264".into(),
        }
    }

    pub(crate) fn create(
        &self,
        config: &EncoderConfig,
    ) -> Result<Box<dyn VideoEncoder>, VideoError> {
        match self {
            Self::Ffmpeg { path, profile } => {
                Ok(Box::new(ffmpeg::FfmpegEncoder::new(path, profile, config)?))
            }
            #[cfg(target_os = "macos")]
            Self::VideoToolbox(_) => Ok(Box::new(videotoolbox::VideoToolboxEncoder::new(config)?)),
            #[cfg(feature = "openh264")]
            Self::OpenH264 => Ok(Box::new(builtin::OpenH264Encoder::new(config)?)),
        }
    }
}

/// Discovers what can be used on this machine (call once at startup).
pub struct VideoEncoders {
    /// Usable backends, best first.
    backends: Vec<Backend>,
    ffmpeg: Option<PathBuf>,
    /// Why ffmpeg could not be used at all.
    ffmpeg_error: Option<String>,
    /// ffmpeg encoders that are listed but failed their trial encode.
    rejected: Vec<(&'static str, String)>,
}

impl VideoEncoders {
    /// `ffmpeg`: explicit ffmpeg binary, or None to look for `ffmpeg` in PATH
    /// (absence is fine).
    ///
    /// Each candidate ffmpeg encoder gets a short trial encode (in parallel),
    /// which takes a fraction of a second.
    pub fn detect(ffmpeg: Option<&Path>) -> Self {
        let mut found = Self {
            backends: Vec::new(),
            ffmpeg: ffmpeg::locate(ffmpeg),
            ffmpeg_error: None,
            rejected: Vec::new(),
        };
        if let Some(path) = &found.ffmpeg {
            match ffmpeg::list_encoders(path) {
                Ok(names) => {
                    let candidates: Vec<&'static ffmpeg::Profile> = ffmpeg::PROFILES
                        .iter()
                        .filter(|profile| names.iter().any(|name| name == profile.name))
                        .collect();
                    let results: Vec<_> = std::thread::scope(|scope| {
                        let probes: Vec<_> = candidates
                            .iter()
                            .map(|&profile| scope.spawn(move || ffmpeg::probe(path, profile)))
                            .collect();
                        probes.into_iter().map(|probe| probe.join()).collect()
                    });
                    for (profile, result) in candidates.into_iter().zip(results) {
                        match result {
                            Ok(Ok(())) => {
                                debug!(encoder = profile.name, "ffmpeg encoder usable");
                                found.backends.push(Backend::Ffmpeg {
                                    path: path.clone(),
                                    profile,
                                });
                            }
                            Ok(Err(err)) => {
                                debug!(encoder = profile.name, error = %err, "ffmpeg encoder unusable");
                                found.rejected.push((profile.name, err.to_string()));
                            }
                            Err(_) => found.rejected.push((profile.name, "probe panicked".into())),
                        }
                    }
                }
                Err(err) => found.ffmpeg_error = Some(err.to_string()),
            }
        }
        #[cfg(target_os = "macos")]
        for codec in [VideoCodec::H265, VideoCodec::H264] {
            match videotoolbox::probe(codec) {
                Ok(()) => found.backends.push(Backend::VideoToolbox(codec)),
                Err(err) => {
                    debug!(?codec, error = %err, "VideoToolbox encoder unusable");
                    let name = match codec {
                        VideoCodec::H264 => "VideoToolbox H.264",
                        _ => "VideoToolbox H.265",
                    };
                    found.rejected.push((name, err.to_string()));
                }
            }
        }
        #[cfg(feature = "openh264")]
        found.backends.push(Backend::OpenH264);
        // Stable: keeps the profile order within a rank.
        found.backends.sort_by_key(Backend::rank);
        found
    }

    /// Codecs that can be encoded, best compression first.
    pub fn available(&self) -> Vec<VideoCodec> {
        VideoCodec::ALL
            .into_iter()
            .filter(|&codec| self.backends.iter().any(|b| b.codec() == codec))
            .collect()
    }

    /// Creates an encoder with the best backend for the codec. Should that
    /// backend fail before its first frame (e.g. a hardware encoder out of
    /// sessions), the encoder moves on to the next one.
    pub fn create(&self, config: &EncoderConfig) -> Result<Box<dyn VideoEncoder>, VideoError> {
        validate(config)?;
        let mut remaining: VecDeque<Backend> = self.backends_for(config.codec).cloned().collect();
        let mut error = VideoError::Unavailable(config.codec);
        while let Some(backend) = remaining.pop_front() {
            match backend.create(config) {
                Ok(encoder) if remaining.is_empty() => return Ok(encoder),
                Ok(encoder) => {
                    return Ok(Box::new(Fallback {
                        current: encoder,
                        remaining,
                        config: config.clone(),
                        started: false,
                    }));
                }
                Err(err) => {
                    warn!(backend = %backend.name(), error = %err, "cannot create video encoder");
                    error = err;
                }
            }
        }
        Err(error)
    }

    /// Largest frame, in pixels, that the best backend for `codec` encodes
    /// in real time (~30 fps for one session). Software AV1 and H.265 are
    /// slow on large frames (SVT-AV1 needs ~25 ms for 720p, x265 ~14 ms);
    /// hardware encoders and software H.264 have no practical limit.
    pub fn realtime_pixels(&self, codec: VideoCodec) -> u64 {
        match self.backends_for(codec).next() {
            None => 0,
            Some(Backend::Ffmpeg { profile, .. }) if profile.hardware => u64::MAX,
            #[cfg(target_os = "macos")]
            Some(Backend::VideoToolbox(_)) => u64::MAX,
            Some(_) => match codec {
                VideoCodec::Av1 => 1280 * 736,
                VideoCodec::H265 => 1920 * 1088,
                VideoCodec::H264 => u64::MAX,
            },
        }
    }

    /// Human-readable description for logs (which backend serves each codec).
    pub fn describe(&self) -> String {
        let codecs: Vec<String> = VideoCodec::ALL
            .into_iter()
            .map(|codec| {
                let names: Vec<String> = self.backends_for(codec).map(Backend::name).collect();
                let names = if names.is_empty() {
                    "unavailable".to_string()
                } else {
                    names.join(", then ")
                };
                format!("{}: {names}", codec_name(codec))
            })
            .collect();
        let mut text = codecs.join("; ");
        match (&self.ffmpeg, &self.ffmpeg_error) {
            (Some(path), None) => text += &format!(" [ffmpeg: {}]", path.display()),
            (Some(path), Some(err)) => {
                text += &format!(" [ffmpeg {} unusable: {err}]", path.display())
            }
            (None, _) => text += " [no ffmpeg]",
        }
        for (name, err) in &self.rejected {
            text += &format!(" [{name} failed its trial: {err}]");
        }
        text
    }

    pub(crate) fn backends_for(&self, codec: VideoCodec) -> impl Iterator<Item = &Backend> {
        self.backends.iter().filter(move |b| b.codec() == codec)
    }
}

/// Switches to the next backend when the current one fails before it has
/// produced anything, which the decoder cannot notice.
struct Fallback {
    current: Box<dyn VideoEncoder>,
    remaining: VecDeque<Backend>,
    config: EncoderConfig,
    started: bool,
}

impl VideoEncoder for Fallback {
    fn codec(&self) -> VideoCodec {
        self.config.codec
    }

    fn encode(&mut self, rgba: &[u8], force_key: bool) -> Result<EncodedFrame, VideoError> {
        let mut result = self.current.encode(rgba, force_key);
        while !self.started {
            let Err(err) = &result else { break };
            if matches!(err, VideoError::FrameSize { .. }) {
                // The caller's mistake: another backend would fail the same.
                break;
            }
            let Some(next) = self.remaining.pop_front() else {
                break;
            };
            warn!(error = %err, next = %next.name(), "video encoder failed; trying the next backend");
            result = next.create(&self.config).and_then(|mut encoder| {
                let frame = encoder.encode(rgba, force_key);
                self.current = encoder;
                frame
            });
        }
        self.started |= result.is_ok();
        result
    }
}

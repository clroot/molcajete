//! Encoders running in an ffmpeg child process.
//!
//! ffmpeg is driven over pipes, never linked, so GPL encoders (libx264,
//! libx265) are fine. Raw 4:2:0 frames go to stdin; the encoded packets come
//! back on stdout, framed by the AVI muxer (see [`super::avi`]) so that each
//! one is complete the moment it is written. Every encoder runs with
//! low-latency settings that emit exactly one packet per frame without
//! delay, and `encode` writes a frame and waits for its packet. An encoder
//! that ever emits a packet late or extra is treated as broken.
//!
//! ffmpeg cannot be asked for a key frame per frame over a raw pipe, so a
//! forced key frame restarts the process: a fresh encoder always begins with
//! one. That costs one encoder start-up (tens of milliseconds) and is only
//! needed when a client joins or recovers.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tracing::debug;

use super::avi::AviReader;
use super::bitstream::{self, InbandConfig};
use super::yuv::{self, ChromaLayout};
use super::{EncodedFrame, EncoderConfig, VideoCodec, VideoEncoder, VideoError, check_frame};

/// Time allowed for the first packet of a process (includes encoder set-up).
const FIRST_PACKET_TIMEOUT: Duration = Duration::from_secs(10);
/// Time allowed for any later packet.
const PACKET_TIMEOUT: Duration = Duration::from_secs(5);
/// Probes give up sooner: an encoder that holds frames back stalls here.
const PROBE_FIRST_PACKET_TIMEOUT: Duration = Duration::from_secs(4);
const PROBE_PACKET_TIMEOUT: Duration = Duration::from_secs(2);
/// Number of stderr lines kept for error messages.
const LOG_LINES: usize = 16;
/// VA-API render node used by the `*_vaapi` encoders.
const VAAPI_DEVICE: &str = "/dev/dri/renderD128";
/// Key frame interval: effectively never (key frames are requested).
const KEYINT: &str = "1000000";

/// How one ffmpeg encoder is driven.
#[derive(Debug)]
pub(crate) struct Profile {
    /// ffmpeg encoder name.
    pub name: &'static str,
    pub codec: VideoCodec,
    pub hardware: bool,
    /// Layout of the frames written to ffmpeg.
    layout: ChromaLayout,
    /// Options before the input (device set-up).
    global_args: fn() -> Vec<String>,
    /// Encoder options for a configuration.
    encoder_args: fn(&EncoderConfig) -> Vec<String>,
    /// Whether the encoder can possibly work here (e.g. its device exists).
    precondition: fn() -> bool,
}

/// Known encoders; within a codec, hardware ones come first.
#[rustfmt::skip]
pub(crate) static PROFILES: &[Profile] = &[
    hardware("av1_nvenc", VideoCodec::Av1, ChromaLayout::Planar, nvenc_args),
    hardware("hevc_nvenc", VideoCodec::H265, ChromaLayout::Planar, nvenc_args),
    hardware("h264_nvenc", VideoCodec::H264, ChromaLayout::Planar, nvenc_args),
    hardware("av1_qsv", VideoCodec::Av1, ChromaLayout::SemiPlanar, qsv_args),
    hardware("hevc_qsv", VideoCodec::H265, ChromaLayout::SemiPlanar, qsv_args),
    hardware("h264_qsv", VideoCodec::H264, ChromaLayout::SemiPlanar, qsv_args),
    vaapi("av1_vaapi", VideoCodec::Av1),
    vaapi("hevc_vaapi", VideoCodec::H265),
    vaapi("h264_vaapi", VideoCodec::H264),
    hardware("av1_amf", VideoCodec::Av1, ChromaLayout::SemiPlanar, amf_args),
    hardware("hevc_amf", VideoCodec::H265, ChromaLayout::SemiPlanar, amf_args),
    hardware("h264_amf", VideoCodec::H264, ChromaLayout::SemiPlanar, amf_args),
    software("libsvtav1", VideoCodec::Av1, svtav1_args),
    software("libx265", VideoCodec::H265, x265_args),
    software("libx264", VideoCodec::H264, x264_args),
];

const fn hardware(
    name: &'static str,
    codec: VideoCodec,
    layout: ChromaLayout,
    encoder_args: fn(&EncoderConfig) -> Vec<String>,
) -> Profile {
    Profile {
        name,
        codec,
        hardware: true,
        layout,
        global_args: Vec::new,
        encoder_args,
        precondition: always,
    }
}

/// VA-API encoders need the render node and upload frames to the GPU.
const fn vaapi(name: &'static str, codec: VideoCodec) -> Profile {
    Profile {
        global_args: vaapi_global_args,
        precondition: vaapi_device_exists,
        ..hardware(name, codec, ChromaLayout::SemiPlanar, vaapi_args)
    }
}

fn always() -> bool {
    true
}

const fn software(
    name: &'static str,
    codec: VideoCodec,
    encoder_args: fn(&EncoderConfig) -> Vec<String>,
) -> Profile {
    Profile {
        hardware: false,
        ..hardware(name, codec, ChromaLayout::Planar, encoder_args)
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

fn kbps(config: &EncoderConfig) -> String {
    format!("{}k", config.bitrate_kbps)
}

/// Encoder threads: one per `pixels_per_thread`, 1 to 8. More only add
/// overhead at these sizes (and several sessions may encode at once).
fn threads(config: &EncoderConfig, pixels_per_thread: usize) -> String {
    let pixels = config.width as usize * config.height as usize;
    (pixels / pixels_per_thread).clamp(1, 8).to_string()
}

/// x264: no lookahead, no B-frames, sliced threads (zerolatency); constant
/// quality capped at the bitrate by a one-second VBV. Two threads are
/// fastest at 720p (each thread is a slice).
#[rustfmt::skip]
fn x264_args(config: &EncoderConfig) -> Vec<String> {
    let rate = kbps(config);
    strings(&[
        "-preset", "ultrafast", "-tune", "zerolatency", "-threads", &threads(config, 400_000),
        "-crf", "23", "-maxrate", &rate, "-bufsize", &rate,
        "-x264-params", "keyint=infinite:repeat-headers=1",
    ])
}

/// x265: zerolatency (no B-frames, no lookahead, one frame thread); VPS,
/// SPS and PPS repeated on every key frame, which are always IDR. A pool of
/// four threads (wavefront rows) is fastest at 720p.
#[rustfmt::skip]
fn x265_args(config: &EncoderConfig) -> Vec<String> {
    let rate = kbps(config);
    let params = format!(
        "keyint=-1:open-gop=0:bframes=0:repeat-headers=1:pools={}:log-level=error",
        threads(config, 230_000)
    );
    strings(&[
        "-preset", "ultrafast", "-tune", "zerolatency",
        "-crf", "28", "-maxrate", &rate, "-bufsize", &rate,
        "-x265-params", &params,
    ])
}

/// SVT-AV1 real-time mode (low-delay prediction, no lookahead) with CBR.
/// Its latency (~17-25 ms at 720p) is pipeline-bound and hardly depends on
/// the preset. Preset 9 costs about as much CPU as 10 (~7 ms per 720p
/// frame) for ~1 dB more; 8 adds another ~1.5 dB for twice the CPU.
/// Below 64×64 SVT-AV1 cannot run its rate control, so tiny frames use
/// constant quality instead.
#[rustfmt::skip]
fn svtav1_args(config: &EncoderConfig) -> Vec<String> {
    let params = if config.width < 64 || config.height < 64 {
        "rtc=1:keyint=-1".to_string()
    } else {
        format!("rtc=1:rc=2:tbr={}:keyint={KEYINT}", config.bitrate_kbps)
    };
    let mut args = strings(&["-preset", "9", "-svtav1-params", &params]);
    if config.width < 64 || config.height < 64 {
        args.extend(strings(&["-crf", "35"]));
    }
    args
}

#[rustfmt::skip]
fn nvenc_args(config: &EncoderConfig) -> Vec<String> {
    let rate = kbps(config);
    strings(&[
        "-preset", "p1", "-tune", "ull", "-zerolatency", "1", "-delay", "0", "-bf", "0",
        "-rc", "cbr", "-b:v", &rate, "-maxrate", &rate, "-bufsize", &rate, "-g", KEYINT,
    ])
}

#[rustfmt::skip]
fn qsv_args(config: &EncoderConfig) -> Vec<String> {
    let rate = kbps(config);
    strings(&[
        "-async_depth", "1", "-bf", "0",
        "-b:v", &rate, "-maxrate", &rate, "-bufsize", &rate, "-g", KEYINT,
    ])
}

fn vaapi_global_args() -> Vec<String> {
    strings(&["-vaapi_device", VAAPI_DEVICE])
}

fn vaapi_device_exists() -> bool {
    Path::new(VAAPI_DEVICE).exists()
}

#[rustfmt::skip]
fn vaapi_args(config: &EncoderConfig) -> Vec<String> {
    let rate = kbps(config);
    strings(&[
        "-vf", "format=nv12,hwupload", "-async_depth", "1", "-bf", "0",
        "-rc_mode", "CBR", "-b:v", &rate, "-maxrate", &rate, "-g", KEYINT,
    ])
}

#[rustfmt::skip]
fn amf_args(config: &EncoderConfig) -> Vec<String> {
    let rate = kbps(config);
    strings(&[
        "-usage", "lowlatency", "-rc", "cbr", "-bf", "0",
        "-b:v", &rate, "-maxrate", &rate, "-bufsize", &rate, "-g", KEYINT,
    ])
}

/// BT.709, limited range: what `yuv` produces, signalled in the bitstream.
#[rustfmt::skip]
const COLOUR_ARGS: [&str; 8] = [
    "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv",
];

/// Full ffmpeg command line for one encoder process.
fn command_args(profile: &Profile, config: &EncoderConfig) -> Vec<String> {
    let mut args = strings(&["-hide_banner", "-nostdin", "-nostats", "-loglevel", "error"]);
    args.extend((profile.global_args)());
    args.extend(strings(&[
        "-f",
        "rawvideo",
        "-pix_fmt",
        profile.layout.ffmpeg_name(),
    ]));
    args.extend([
        "-video_size".to_string(),
        format!("{}x{}", config.width, config.height),
        "-framerate".to_string(),
        config.fps.to_string(),
    ]);
    // Tag the input too, so ffmpeg never converts between colour spaces.
    args.extend(strings(&COLOUR_ARGS));
    args.extend(strings(&["-i", "pipe:0", "-an", "-c:v", profile.name]));
    args.extend((profile.encoder_args)(config));
    args.extend(strings(&COLOUR_ARGS));
    args.extend(strings(&["-flush_packets", "1", "-f", "avi", "pipe:1"]));
    args
}

/// A running ffmpeg and the threads serving its pipes.
struct Process {
    child: Child,
    /// Frames for the stdin thread; dropped to close stdin.
    input: Option<SyncSender<Vec<u8>>>,
    /// Frame buffers handed back by the stdin thread for reuse.
    recycled: Receiver<Vec<u8>>,
    /// Packets parsed by the stdout thread.
    output: Receiver<io::Result<Vec<u8>>>,
    stderr: Option<JoinHandle<()>>,
    /// Last lines ffmpeg wrote to stderr.
    log: Arc<Mutex<VecDeque<String>>>,
    /// Packets received so far.
    packets: u64,
}

impl Process {
    fn spawn(ffmpeg: &Path, args: &[String]) -> Result<Self, VideoError> {
        let mut child = Command::new(ffmpeg)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| VideoError::Encoder(format!("cannot run {}: {e}", ffmpeg.display())))?;
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let started = match pipes {
            (Some(stdin), Some(stdout), Some(stderr)) => start_threads(stdin, stdout, stderr),
            _ => Err(io::Error::other("ffmpeg pipes missing")),
        };
        match started {
            Ok((input, recycled, output, stderr, log)) => Ok(Self {
                child,
                input: Some(input),
                recycled,
                output,
                stderr: Some(stderr),
                log,
                packets: 0,
            }),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(e.into())
            }
        }
    }

    /// Stops the process and describes what went wrong, with ffmpeg's own
    /// last words.
    fn failure(&mut self, what: &str) -> VideoError {
        self.input = None;
        let _ = self.child.kill();
        let status = self.child.wait();
        if let Some(stderr) = self.stderr.take() {
            // ffmpeg is gone, so its stderr ends.
            let _ = stderr.join();
        }
        let log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        let details: Vec<&str> = log
            .iter()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with("Svt[info]"))
            .collect();
        let mut message = what.to_string();
        if let Ok(status) = status
            && status.code().is_some()
        {
            message += &format!(" ({status})");
        }
        if !details.is_empty() {
            message += ": ";
            message += &details.join(" | ");
        }
        VideoError::Encoder(message)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.input = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

type Threads = (
    SyncSender<Vec<u8>>,
    Receiver<Vec<u8>>,
    Receiver<io::Result<Vec<u8>>>,
    JoinHandle<()>,
    Arc<Mutex<VecDeque<String>>>,
);

/// Starts the threads that write frames, read packets and collect stderr.
/// They end on their own once the process is gone.
fn start_threads(
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
) -> io::Result<Threads> {
    let (input, frames) = mpsc::sync_channel::<Vec<u8>>(1);
    let (recycle, recycled) = mpsc::channel();
    thread::Builder::new()
        .name("ffmpeg-stdin".into())
        .spawn(move || {
            for frame in frames {
                if stdin
                    .write_all(&frame)
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    break;
                }
                let _ = recycle.send(frame);
            }
        })?;

    let (packets, output) = mpsc::channel();
    thread::Builder::new()
        .name("ffmpeg-stdout".into())
        .spawn(move || {
            let mut avi = AviReader::new(BufReader::with_capacity(1 << 16, stdout));
            while let Some(packet) = avi.next_packet().transpose() {
                let failed = packet.is_err();
                if packets.send(packet).is_err() || failed {
                    break;
                }
            }
        })?;

    let log = Arc::new(Mutex::new(VecDeque::with_capacity(LOG_LINES)));
    let lines = log.clone();
    let stderr = thread::Builder::new()
        .name("ffmpeg-stderr".into())
        .spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
                let text = String::from_utf8_lossy(&line).trim_end().to_string();
                line.clear();
                debug!(target: "guacd::video::ffmpeg", "{text}");
                let mut lines = lines.lock().unwrap_or_else(PoisonError::into_inner);
                if lines.len() == LOG_LINES {
                    lines.pop_front();
                }
                lines.push_back(text);
            }
        })?;
    Ok((input, recycled, output, stderr, log))
}

/// One encoder instance backed by an ffmpeg process.
pub(crate) struct FfmpegEncoder {
    ffmpeg: PathBuf,
    profile: &'static Profile,
    config: EncoderConfig,
    args: Vec<String>,
    /// `None` after a failure or before a forced key frame: the next frame
    /// starts a new process.
    process: Option<Process>,
    inband: InbandConfig,
    first_timeout: Duration,
    timeout: Duration,
}

impl FfmpegEncoder {
    /// Starts ffmpeg right away (it initialises the encoder on the first
    /// frame), so a missing binary shows up here.
    pub(crate) fn new(
        ffmpeg: &Path,
        profile: &'static Profile,
        config: &EncoderConfig,
    ) -> Result<Self, VideoError> {
        let args = command_args(profile, config);
        let process = Process::spawn(ffmpeg, &args)?;
        Ok(Self {
            ffmpeg: ffmpeg.to_path_buf(),
            profile,
            config: config.clone(),
            args,
            process: Some(process),
            inband: InbandConfig::new(profile.codec),
            first_timeout: FIRST_PACKET_TIMEOUT,
            timeout: PACKET_TIMEOUT,
        })
    }

    fn encode_frame(&mut self, rgba: &[u8]) -> Result<EncodedFrame, VideoError> {
        let process = match self.process.take() {
            Some(process) => process,
            None => Process::spawn(&self.ffmpeg, &self.args)?,
        };
        let process = self.process.insert(process);

        // Anything that arrived since the last packet means the encoder
        // does not emit exactly one packet per frame.
        if process.output.try_recv().is_ok() {
            return Err(process.failure("encoder emitted an unexpected packet"));
        }
        let mut frame = process.recycled.try_recv().unwrap_or_default();
        let (width, height) = (self.config.width as usize, self.config.height as usize);
        yuv::rgba_to_yuv420(rgba, width, height, self.profile.layout, &mut frame);
        let sent = process
            .input
            .as_ref()
            .is_some_and(|input| input.send(frame).is_ok());
        if !sent {
            return Err(process.failure("ffmpeg stopped accepting frames"));
        }

        let timeout = if process.packets == 0 {
            self.first_timeout
        } else {
            self.timeout
        };
        let packet = match process.output.recv_timeout(timeout) {
            Ok(Ok(packet)) => packet,
            Ok(Err(e)) => return Err(process.failure(&format!("reading ffmpeg output: {e}"))),
            Err(RecvTimeoutError::Timeout) => {
                return Err(process.failure(&format!("no packet within {timeout:?}")));
            }
            Err(RecvTimeoutError::Disconnected) => return Err(process.failure("ffmpeg exited")),
        };
        process.packets += 1;

        let codec = self.profile.codec;
        let data = bitstream::normalize(codec, packet);
        let key = bitstream::is_key_frame(codec, &data);
        if process.packets == 1 && !key {
            return Err(process.failure("encoder did not start with a key frame"));
        }
        let data = self.inband.apply(data, key);
        Ok(EncodedFrame { data, key })
    }
}

impl VideoEncoder for FfmpegEncoder {
    fn codec(&self) -> VideoCodec {
        self.profile.codec
    }

    fn encode(&mut self, rgba: &[u8], force_key: bool) -> Result<EncodedFrame, VideoError> {
        check_frame(&self.config, rgba)?;
        if force_key && self.process.as_ref().is_some_and(|p| p.packets > 0) {
            // A fresh encoder starts with a key frame.
            self.process = None;
        }
        let result = self.encode_frame(rgba);
        if result.is_err() {
            // Start over (with a key frame) on the next call.
            self.process = None;
        }
        result
    }
}

/// The ffmpeg binary: `explicit`, or `ffmpeg` in `PATH`.
pub(crate) fn locate(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    let name = format!("ffmpeg{}", std::env::consts::EXE_SUFFIX);
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(&name))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Names of the video encoders `ffmpeg -encoders` lists.
pub(crate) fn list_encoders(ffmpeg: &Path) -> Result<Vec<String>, VideoError> {
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| VideoError::Encoder(format!("cannot run {}: {e}", ffmpeg.display())))?;
    if !output.status.success() {
        return Err(VideoError::Encoder(format!(
            "ffmpeg -encoders: {}",
            output.status
        )));
    }
    Ok(parse_encoders(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_encoders(listing: &str) -> Vec<String> {
    listing
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("---"))
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let flags = fields.next()?;
            let name = fields.next()?;
            flags.starts_with('V').then(|| name.to_string())
        })
        .collect()
}

/// Checks that `profile` really works here: a few small frames must each
/// come back as exactly one packet without delay, the first a key frame
/// carrying the codec configuration.
pub(crate) fn probe(ffmpeg: &Path, profile: &'static Profile) -> Result<(), VideoError> {
    if !(profile.precondition)() {
        return Err(VideoError::Encoder("device not present".into()));
    }
    let config = EncoderConfig {
        codec: profile.codec,
        width: 320,
        height: 240,
        fps: 30,
        bitrate_kbps: 500,
    };
    let mut encoder = FfmpegEncoder::new(ffmpeg, profile, &config)?;
    encoder.first_timeout = PROBE_FIRST_PACKET_TIMEOUT;
    encoder.timeout = PROBE_PACKET_TIMEOUT;
    for index in 0..3u32 {
        let frame: Vec<u8> = (0..config.width * config.height)
            .flat_map(|i| {
                let (x, y) = (i % config.width, i / config.width);
                [
                    (x + 4 * index) as u8,
                    (y + 2 * index) as u8,
                    ((x ^ y) + index) as u8,
                    255,
                ]
            })
            .collect();
        let packet = encoder.encode(&frame, false)?;
        if index == 0 && !bitstream::has_config(profile.codec, &packet.data) {
            return Err(VideoError::Encoder(
                "key frame without in-band configuration".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_encoder_listing() {
        let listing = "Encoders:\n V..... = Video\n A..... = Audio\n .F.... = Frame-level multithreading\n ------\n V....D libx264              libx264 H.264 / AVC (codec h264)\n A....D aac                  AAC (Advanced Audio Coding)\n V..... h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)\n";
        assert_eq!(parse_encoders(listing), ["libx264", "h264_nvenc"]);
        assert!(parse_encoders("garbage").is_empty());
    }

    #[test]
    fn profiles_are_unique_and_hardware_first() {
        for codec in VideoCodec::ALL {
            let of_codec: Vec<_> = PROFILES.iter().filter(|p| p.codec == codec).collect();
            let software = of_codec
                .iter()
                .position(|p| !p.hardware)
                .expect("software fallback");
            assert!(
                of_codec[software..].iter().all(|p| !p.hardware),
                "{codec:?}"
            );
        }
        for (i, p) in PROFILES.iter().enumerate() {
            assert!(
                PROFILES[i + 1..].iter().all(|q| q.name != p.name),
                "{}",
                p.name
            );
        }
    }

    #[test]
    fn command_line_is_low_latency_avi_with_bt709() {
        let profile = PROFILES.iter().find(|p| p.name == "libx264").unwrap();
        let config = EncoderConfig {
            codec: VideoCodec::H264,
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 2500,
        };
        let args = command_args(profile, &config).join(" ");
        assert!(args.contains("-f rawvideo -pix_fmt yuv420p -video_size 1280x720 -framerate 30"));
        assert!(args.contains("-tune zerolatency"));
        assert!(args.contains("-maxrate 2500k"));
        assert!(args.contains("-colorspace bt709"));
        assert!(args.ends_with("-flush_packets 1 -f avi pipe:1"));
    }

    #[test]
    fn small_av1_frames_use_constant_quality() {
        let mut config = EncoderConfig {
            codec: VideoCodec::Av1,
            width: 48,
            height: 32,
            fps: 30,
            bitrate_kbps: 300,
        };
        assert!(svtav1_args(&config).join(" ").contains("-crf"));
        config.width = 640;
        config.height = 352;
        assert!(svtav1_args(&config).join(" ").contains("rc=2:tbr=300"));
    }
}

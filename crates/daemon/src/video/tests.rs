//! End-to-end checks of every backend usable on this machine: encode
//! synthetic content, decode it back with ffmpeg and compare.
//!
//! The 1280×720 runs print encode time and size per frame. For meaningful
//! numbers: `cargo test --release -p molcajete-daemon video:: -- --nocapture --test-threads=1`.
//! With `GUACD_VIDEO_DUMP=<dir>` the encoded streams are also written there.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::*;

const FRAMES: usize = 60;
const FORCED_KEYS: [usize; 2] = [20, 45];
const SIZES: [(u32, u32); 2] = [(640, 352), (1280, 720)];

/// Flat patches (x, y, colour) for checking matrix and range signalling.
const PATCHES: [(u32, u32, [u8; 3]); 6] = [
    (16, 16, [0, 0, 0]),
    (72, 16, [255, 255, 255]),
    (128, 16, [255, 0, 0]),
    (184, 16, [0, 255, 0]),
    (240, 16, [0, 0, 255]),
    (296, 16, [255, 200, 40]),
];
const PATCH: u32 = 48;

fn ffmpeg_here() -> Option<PathBuf> {
    let found = ffmpeg::locate(None);
    if found.is_none() {
        eprintln!("ffmpeg not found; skipping");
    }
    found
}

/// Same rule of thumb as the display code: 0.1 bit per pixel for H.264.
fn bitrate_kbps(codec: VideoCodec, width: u32, height: u32) -> u32 {
    let factor = match codec {
        VideoCodec::H264 => 1.0,
        VideoCodec::H265 => 0.7,
        VideoCodec::Av1 => 0.6,
    };
    let bits = width as f32 * height as f32 * 30.0 * 0.1 * factor;
    ((bits / 1000.0) as u32).clamp(300, 16_000)
}

fn triangle(value: u32, period: u32) -> u8 {
    let p = value % period;
    let half = period / 2;
    let v = if p < half { p } else { period - p };
    (v * 255 / half) as u8
}

/// Moving colour gradients, a band of scrolling text-like glyphs and a row
/// of static flat patches.
fn synthetic_frame(width: u32, height: u32, index: u32) -> Vec<u8> {
    let text_top = height * 5 / 8;
    let text_bottom = (text_top + 24).min(height);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let mut rgb = [
                triangle(x + 6 * index, 2 * width),
                triangle(y + 3 * index, 2 * height),
                triangle(x + y + 9 * index, 512),
            ];
            if (text_top..text_bottom).contains(&y) {
                // 5×7 glyphs in 8×12 cells, scrolling left.
                let sx = x + 2 * index;
                let (cell, col) = (sx / 8, sx % 8);
                let (line, row) = ((y - text_top) / 12, (y - text_top) % 12);
                let glyph = ((u64::from(cell) * 2_654_435_761) ^ (u64::from(line) * 40_503))
                    .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                    >> 20;
                let ink =
                    col < 5 && (2..9).contains(&row) && (glyph >> ((row - 2) * 5 + col)) & 1 == 1;
                rgb = if ink { [20, 20, 30] } else { [240, 240, 235] };
            }
            for &(px, py, colour) in &PATCHES {
                if (px..px + PATCH).contains(&x) && (py..py + PATCH).contains(&y) {
                    rgb = colour;
                }
            }
            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
    }
    rgba
}

/// Decodes a raw stream to RGBA frames with ffmpeg.
fn decode(ffmpeg: &Path, codec: VideoCodec, stream: Vec<u8>) -> Vec<u8> {
    let format = match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::H265 => "hevc",
        VideoCodec::Av1 => "obu",
    };
    let mut child = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            format,
            "-i",
            "pipe:0",
        ])
        .args([
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("run ffmpeg");
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(&stream));
    let mut decoded = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut decoded)
        .unwrap();
    writer.join().unwrap().unwrap();
    assert!(child.wait().unwrap().success(), "ffmpeg failed to decode");
    decoded
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let (mut sum, mut count) = (0u64, 0u64);
    for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        for c in 0..3 {
            let d = pa[c] as i64 - pb[c] as i64;
            sum += (d * d) as u64;
            count += 1;
        }
    }
    if sum == 0 {
        return 99.0;
    }
    10.0 * (255.0f64 * 255.0 / (sum as f64 / count as f64)).log10()
}

/// Average colour of a patch's interior in a decoded frame.
fn patch_colour(frame: &[u8], width: u32, px: u32, py: u32) -> [f64; 3] {
    let mut sum = [0.0; 3];
    let inset = 8;
    let side = PATCH - 2 * inset;
    for y in py + inset..py + inset + side {
        for x in px + inset..px + inset + side {
            let o = ((y * width + x) * 4) as usize;
            for c in 0..3 {
                sum[c] += frame[o + c] as f64;
            }
        }
    }
    sum.map(|s| s / (side * side) as f64)
}

fn check_patches(name: &str, frame: &[u8], width: u32, height: u32) {
    for &(px, py, colour) in &PATCHES {
        if px + PATCH > width || py + PATCH > height {
            continue;
        }
        let got = patch_colour(frame, width, px, py);
        for c in 0..3 {
            assert!(
                (got[c] - colour[c] as f64).abs() <= 8.0,
                "{name}: patch {colour:?} decoded as {got:?} (colour matrix or range mismatch?)"
            );
        }
    }
}

fn avg_ms(times: &[Duration]) -> f64 {
    times.iter().map(Duration::as_secs_f64).sum::<f64>() * 1000.0 / times.len().max(1) as f64
}

/// Encodes `frames` synthetic frames (forcing key frames as given), checks
/// the output and decodes it back; every decoded frame must reach
/// `min_db`. Returns a one-line summary.
fn round_trip(
    ffmpeg: &Path,
    backend: &Backend,
    (width, height): (u32, u32),
    frames: usize,
    forced: &[usize],
    min_db: f64,
) -> String {
    let codec = backend.codec();
    let name = format!("{} {width}x{height}", backend.name());
    let config = EncoderConfig {
        codec,
        width,
        height,
        fps: 30,
        bitrate_kbps: bitrate_kbps(codec, width, height),
    };
    let inputs: Vec<Vec<u8>> = (0..frames)
        .map(|i| synthetic_frame(width, height, i as u32))
        .collect();
    let mut encoder = backend
        .create(&config)
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(encoder.codec(), codec);

    let mut stream = Vec::new();
    let mut offsets = Vec::new();
    let mut times = Vec::new();
    let mut sizes = Vec::new();
    let mut keys = Vec::new();
    for (i, rgba) in inputs.iter().enumerate() {
        let force = forced.contains(&i);
        let start = Instant::now();
        let frame = encoder
            .encode(rgba, force)
            .unwrap_or_else(|e| panic!("{name}: frame {i}: {e}"));
        times.push(start.elapsed());
        assert!(!frame.data.is_empty(), "{name}: frame {i} is empty");
        assert_eq!(
            frame.key,
            bitstream::is_key_frame(codec, &frame.data),
            "{name}: frame {i}"
        );
        if i == 0 || force {
            assert!(frame.key, "{name}: frame {i} should be a key frame");
        }
        if frame.key {
            assert!(
                bitstream::has_config(codec, &frame.data),
                "{name}: key frame {i} lacks in-band configuration"
            );
            keys.push(i);
        }
        offsets.push(stream.len());
        sizes.push(frame.data.len());
        stream.extend_from_slice(&frame.data);
    }
    drop(encoder);
    if let Some(dir) = std::env::var_os("GUACD_VIDEO_DUMP") {
        let file = format!("{}.{}", name.replace(' ', "-"), codec_name(codec));
        std::fs::write(Path::new(&dir).join(file), &stream).unwrap();
    }

    // Decoding everything must give back exactly one picture per frame, in
    // step with the input (a delayed encoder would be off by one).
    let frame_len = (width * height * 4) as usize;
    let decoded = decode(ffmpeg, codec, stream.clone());
    assert_eq!(
        decoded.len(),
        frame_len * frames,
        "{name}: decoded frame count"
    );
    let scores: Vec<f64> = decoded
        .chunks_exact(frame_len)
        .zip(&inputs)
        .map(|(out, input)| psnr(out, input))
        .collect();
    let (worst, min_psnr) = scores
        .iter()
        .copied()
        .enumerate()
        .fold((0, f64::INFINITY), |a, b| if b.1 < a.1 { b } else { a });
    let avg_psnr = scores.iter().sum::<f64>() / scores.len() as f64;
    assert!(min_psnr > min_db, "{name}: PSNR {scores:.1?}");
    check_patches(&name, &decoded[..frame_len], width, height);
    check_patches(&name, &decoded[decoded.len() - frame_len..], width, height);

    // Every forced key frame must be a random access point on its own.
    for &key in forced {
        let decoded = decode(ffmpeg, codec, stream[offsets[key]..].to_vec());
        assert_eq!(
            decoded.len(),
            frame_len * (frames - key),
            "{name}: decoding from frame {key}"
        );
        assert!(
            psnr(&decoded[..frame_len], &inputs[key]) > min_db,
            "{name}: from frame {key}"
        );
    }

    let first = times[0];
    let forced_times: Vec<Duration> = forced.iter().map(|&i| times[i]).collect();
    let steady: Vec<Duration> = (1..frames)
        .filter(|i| !forced.contains(i))
        .map(|i| times[i])
        .collect();
    let mut sorted = steady.clone();
    sorted.sort();
    let p95 = sorted
        .get(sorted.len() * 95 / 100)
        .copied()
        .unwrap_or_default();
    let key_bytes: Vec<usize> = keys.iter().map(|&i| sizes[i]).collect();
    let delta_bytes: Vec<usize> = (0..frames)
        .filter(|i| !keys.contains(i))
        .map(|i| sizes[i])
        .collect();
    format!(
        "{name:<36} {kbps:>5} kbps | encode ms: steady avg {:5.1} p95 {:5.1}, first {:5.1}, forced key {:5.1} | bytes: avg {:6}, delta avg {:6}, key avg {:6} | PSNR avg {avg_psnr:.1} min {min_psnr:.1} dB (frame {worst})",
        avg_ms(&steady),
        p95.as_secs_f64() * 1000.0,
        first.as_secs_f64() * 1000.0,
        avg_ms(&forced_times),
        sizes.iter().sum::<usize>() / frames,
        delta_bytes.iter().sum::<usize>() / delta_bytes.len().max(1),
        key_bytes.iter().sum::<usize>() / key_bytes.len().max(1),
        kbps = config.bitrate_kbps,
    )
}

fn check_codec(codec: VideoCodec) {
    let Some(ffmpeg) = ffmpeg_here() else { return };
    let encoders = VideoEncoders::detect(Some(&ffmpeg));
    let backends: Vec<Backend> = encoders.backends_for(codec).cloned().collect();
    if backends.is_empty() {
        eprintln!("no {codec:?} encoder here: {}", encoders.describe());
        return;
    }
    for backend in &backends {
        for size in SIZES {
            let summary = round_trip(&ffmpeg, backend, size, FRAMES, &FORCED_KEYS, 30.0);
            eprintln!("{summary}");
        }
        // A size that is not a multiple of 16, and a tiny one (where the
        // patch edges dominate and quality settles lower). Hardware
        // encoders have a smallest frame (Quick Sync refuses 48×32), so
        // they get the smallest video the display makes instead: a square
        // of `MIN_SIDE` (display/video.rs), 3 tiles of 64.
        let tiny = if backend.rank() == 0 {
            (192, 192)
        } else {
            (48, 32)
        };
        for size in [(202, 130), tiny] {
            round_trip(&ffmpeg, backend, size, 6, &[3], 25.0);
        }
    }
}

#[test]
fn h264_round_trip() {
    check_codec(VideoCodec::H264);
}

#[test]
fn h265_round_trip() {
    check_codec(VideoCodec::H265);
}

#[test]
fn av1_round_trip() {
    check_codec(VideoCodec::Av1);
}

#[test]
fn detects_listed_software_encoders() {
    let Some(ffmpeg) = ffmpeg_here() else { return };
    let listed = ffmpeg::list_encoders(&ffmpeg).unwrap();
    let encoders = VideoEncoders::detect(Some(&ffmpeg));
    eprintln!("{}", encoders.describe());
    for (name, codec) in [
        ("libx264", VideoCodec::H264),
        ("libx265", VideoCodec::H265),
        ("libsvtav1", VideoCodec::Av1),
    ] {
        if listed.iter().any(|n| n == name) {
            assert!(
                encoders.available().contains(&codec),
                "{name}: {}",
                encoders.describe()
            );
            assert!(encoders.describe().contains(name));
        }
    }
    // Best compression first.
    let available = encoders.available();
    let mut sorted = available.clone();
    sorted.sort();
    assert_eq!(available, sorted);
}

#[test]
fn absent_ffmpeg_is_fine() {
    let encoders = VideoEncoders::detect(Some(Path::new("/nonexistent/ffmpeg")));
    assert!(
        encoders.describe().contains("unusable"),
        "{}",
        encoders.describe()
    );
    // Only the backends that do not need ffmpeg remain.
    assert!(
        encoders
            .backends
            .iter()
            .all(|b| !matches!(b, Backend::Ffmpeg { .. })),
        "{}",
        encoders.describe()
    );
    #[cfg(all(feature = "openh264", not(target_os = "macos")))]
    assert_eq!(encoders.available(), [VideoCodec::H264]);
    #[cfg(not(any(feature = "openh264", target_os = "macos")))]
    assert!(encoders.available().is_empty());
}

#[test]
fn rejects_bad_configs_and_frames() {
    let Some(ffmpeg) = ffmpeg_here() else { return };
    let encoders = VideoEncoders::detect(Some(&ffmpeg));
    for codec in encoders.available() {
        let mut config = EncoderConfig {
            codec,
            width: 63,
            height: 64,
            fps: 30,
            bitrate_kbps: 500,
        };
        assert!(matches!(
            encoders.create(&config),
            Err(VideoError::InvalidConfig(_))
        ));
        config.width = 64;
        config.fps = 0;
        assert!(matches!(
            encoders.create(&config),
            Err(VideoError::InvalidConfig(_))
        ));
        config.fps = 30;
        let mut encoder = encoders.create(&config).unwrap();
        assert_eq!(encoder.codec(), codec);
        assert!(matches!(
            encoder.encode(&[0; 16], false),
            Err(VideoError::FrameSize {
                expected: 16384,
                actual: 16
            })
        ));
        // Still usable, and the first frame is a key frame even if not asked.
        let frame = encoder.encode(&synthetic_frame(64, 64, 0), false).unwrap();
        assert!(frame.key, "{codec:?}");
    }
}

#[test]
fn falls_back_when_a_backend_fails_before_its_first_frame() {
    let Some(ffmpeg) = ffmpeg_here() else { return };
    let listed = ffmpeg::list_encoders(&ffmpeg).unwrap();
    let profile = |name: &str| ffmpeg::PROFILES.iter().find(|p| p.name == name).unwrap();
    if !listed.iter().any(|n| n == "libx264") || listed.iter().any(|n| n == "h264_nvenc") {
        return;
    }
    // h264_nvenc is not in this ffmpeg, so it fails on the first frame;
    // a missing binary fails at creation.
    let encoders = VideoEncoders {
        backends: vec![
            Backend::Ffmpeg {
                path: "/nonexistent/ffmpeg".into(),
                profile: profile("libx264"),
            },
            Backend::Ffmpeg {
                path: ffmpeg.clone(),
                profile: profile("h264_nvenc"),
            },
            Backend::Ffmpeg {
                path: ffmpeg.clone(),
                profile: profile("libx264"),
            },
        ],
        ffmpeg: Some(ffmpeg),
        ffmpeg_error: None,
        rejected: Vec::new(),
    };
    let config = EncoderConfig {
        codec: VideoCodec::H264,
        width: 128,
        height: 96,
        fps: 30,
        bitrate_kbps: 300,
    };
    let mut encoder = encoders.create(&config).unwrap();
    for i in 0..3 {
        let frame = encoder.encode(&synthetic_frame(128, 96, i), false).unwrap();
        assert_eq!(frame.key, i == 0);
    }
}

//! Playback of raw PCM audio streams (`audio/L8`, `audio/L16`) via Web Audio.

use web_sys::{AudioContext, AudioContextState};

/// Audio mimetypes advertised during the handshake.
pub const SUPPORTED_MIMETYPES: [&str; 2] = ["audio/L8", "audio/L16"];

/// Packets scheduled further ahead than this are dropped to bound latency.
const MAX_LATENCY: f64 = 0.3;
/// Initial buffering to absorb network jitter.
const MIN_LATENCY: f64 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawFormat {
    pub bytes_per_sample: u32,
    pub rate: u32,
    pub channels: u32,
}

impl RawFormat {
    /// Parses mimetypes such as `audio/L16;rate=44100,channels=2`.
    pub fn parse(mimetype: &str) -> Option<RawFormat> {
        let (kind, params) = mimetype.split_once(';').unwrap_or((mimetype, ""));
        let bytes_per_sample = match kind.trim() {
            "audio/L8" => 1,
            "audio/L16" => 2,
            _ => return None,
        };
        let mut rate = None;
        let mut channels = 1;
        for param in params.split(',') {
            let Some((name, value)) = param.split_once('=') else {
                continue;
            };
            match name.trim() {
                "rate" => rate = value.trim().parse().ok(),
                "channels" => channels = value.trim().parse().ok()?,
                _ => {}
            }
        }
        let rate = rate?;
        (rate > 0 && (1..=8).contains(&channels)).then_some(RawFormat {
            bytes_per_sample,
            rate,
            channels,
        })
    }

    /// Converts interleaved PCM into one `f32` vector per channel.
    pub fn deinterleave(&self, data: &[u8]) -> Vec<Vec<f32>> {
        let frame_bytes = (self.bytes_per_sample * self.channels) as usize;
        let frames = data.len() / frame_bytes;
        let mut out = vec![Vec::with_capacity(frames); self.channels as usize];
        for frame in data.chunks_exact(frame_bytes) {
            for (ch, sample) in frame
                .chunks_exact(self.bytes_per_sample as usize)
                .enumerate()
            {
                let v = if self.bytes_per_sample == 2 {
                    i16::from_le_bytes([sample[0], sample[1]]) as f32 / 32768.0
                } else {
                    sample[0] as i8 as f32 / 128.0
                };
                out[ch].push(v);
            }
        }
        out
    }
}

pub struct RawAudioPlayer {
    context: AudioContext,
    format: RawFormat,
    next_time: f64,
}

impl RawAudioPlayer {
    pub fn new(context: AudioContext, format: RawFormat) -> Self {
        Self {
            context,
            format,
            next_time: 0.0,
        }
    }

    pub fn play(&mut self, data: &[u8]) {
        if self.context.state() != AudioContextState::Running {
            // Autoplay policy: nothing can play until a user gesture resumes
            // the context, so drop audio instead of building up latency.
            return;
        }
        let channels = self.format.deinterleave(data);
        let frames = channels.first().map_or(0, Vec::len);
        if frames == 0 {
            return;
        }
        let now = self.context.current_time();
        if self.next_time < now + MIN_LATENCY / 2.0 {
            self.next_time = now + MIN_LATENCY;
        }
        if self.next_time > now + MAX_LATENCY {
            return;
        }
        let Ok(buffer) = self.context.create_buffer(
            self.format.channels,
            frames as u32,
            self.format.rate as f32,
        ) else {
            return;
        };
        for (i, samples) in channels.iter().enumerate() {
            let _ = buffer.copy_to_channel(samples, i as i32);
        }
        let Ok(source) = self.context.create_buffer_source() else {
            return;
        };
        source.set_buffer(Some(&buffer));
        if source
            .connect_with_audio_node(&self.context.destination())
            .is_err()
        {
            return;
        }
        let _ = source.start_with_when(self.next_time);
        self.next_time += frames as f64 / self.format.rate as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_raw_formats() {
        assert_eq!(
            RawFormat::parse("audio/L16;rate=44100,channels=2"),
            Some(RawFormat {
                bytes_per_sample: 2,
                rate: 44100,
                channels: 2
            })
        );
        assert_eq!(
            RawFormat::parse("audio/L8;rate=8000"),
            Some(RawFormat {
                bytes_per_sample: 1,
                rate: 8000,
                channels: 1
            })
        );
        assert_eq!(RawFormat::parse("audio/L16"), None);
        assert_eq!(RawFormat::parse("audio/ogg;rate=8000"), None);
    }

    #[test]
    fn deinterleaves_16_bit_stereo() {
        let fmt = RawFormat::parse("audio/L16;rate=8000,channels=2").unwrap();
        let data = [0x00, 0x40, 0x00, 0xC0, 0xFF, 0x7F, 0x00, 0x80];
        let ch = fmt.deinterleave(&data);
        assert_eq!(ch[0], vec![0.5, 32767.0 / 32768.0]);
        assert_eq!(ch[1], vec![-0.5, -1.0]);
    }
}

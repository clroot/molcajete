//! Audio output: sound played in the remote session, received over the
//! RDPSND static virtual channel (or `AUDIO_PLAYBACK_DVC`, which Windows 8
//! and later prefer), forwarded to the Guacamole client as raw PCM.
//!
//! IronRDP's [`Rdpsnd`] negotiates the formats, answers Training PDUs and
//! confirms every wave block as soon as the handler returned, so the server
//! keeps streaming. The handler ([`SoundHandler`]) only queues the wave data;
//! the session loop drains the queue after each server PDU ([`AudioOutput`])
//! and sends it right away as `audio` / `blob` / `end` instructions, without
//! waiting for the display's frame pacing.
//!
//! Every format the client can be offered is decoded to signed 16-bit PCM
//! and sent at the server's sample rate and channel count: Guacamole clients
//! resample raw PCM themselves, so no conversion other than decoding (and
//! narrowing to 8 bits for `audio/L8`-only clients) is needed.

use std::borrow::Cow;
use std::sync::mpsc;
use std::time::Instant;

use base64::Engine as _;
use ironrdp_rdpsnd::client::{Rdpsnd, RdpsndClientHandler, RdpsndDvc, RdpsndDvcState};
use ironrdp_rdpsnd::codec::AudioDecoder;
use ironrdp_rdpsnd::pdu::{AudioFormat, PitchPdu, VolumePdu, WaveFormat};
use molcajete_protocol::{InstructionBuilder, Status};
use tracing::{debug, info, warn};

/// Guacamole stream index of the audio output stream.
const AUDIO_STREAM: i32 = 128;
/// Largest blob payload, in bytes (guacd's `GUAC_PROTOCOL_BLOB_MAX_LENGTH`).
const MAX_BLOB: usize = 6048;
/// Sample rates in order of preference; others rank after these.
const PREFERRED_RATES: [u32; 3] = [44_100, 48_000, 22_050];
/// Formats outside these bounds are not offered to the server.
const MAX_CHANNELS: u16 = 8;
const RATES: std::ops::RangeInclusive<u32> = 8_000..=192_000;

/// Raw PCM encoding understood by the Guacamole client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEncoding {
    /// `audio/L16`: signed 16-bit little-endian samples.
    L16,
    /// `audio/L8`: signed 8-bit samples.
    L8,
}

impl AudioEncoding {
    /// The best encoding among the audio mimetypes the client announced in
    /// the handshake, if any.
    pub fn negotiate(mimetypes: &[String]) -> Option<Self> {
        let offered = |wanted: &str| {
            mimetypes.iter().any(|mimetype| {
                let base = mimetype.split(';').next().unwrap_or_default();
                base.trim().eq_ignore_ascii_case(wanted)
            })
        };
        if offered("audio/L16") {
            Some(Self::L16)
        } else if offered("audio/L8") {
            Some(Self::L8)
        } else {
            None
        }
    }

    fn mimetype(self, format: StreamFormat) -> String {
        let base = match self {
            Self::L16 => "audio/L16",
            Self::L8 => "audio/L8",
        };
        format!("{base};rate={},channels={}", format.rate, format.channels)
    }

    fn bytes_per_sample(self) -> usize {
        match self {
            Self::L16 => 2,
            Self::L8 => 1,
        }
    }

    /// Appends interleaved samples in this encoding.
    fn encode(self, samples: &[i16], out: &mut Vec<u8>) {
        match self {
            Self::L16 => out.extend(samples.iter().flat_map(|s| s.to_le_bytes())),
            Self::L8 => out.extend(samples.iter().map(|s| s.to_be_bytes()[0])),
        }
    }
}

/// Server formats worth offering (those that can be decoded), best first:
/// 16-bit PCM before everything else, then by sample rate, stereo before
/// mono.
fn preferred_formats(server_formats: &[AudioFormat]) -> Vec<AudioFormat> {
    let mut formats: Vec<AudioFormat> = server_formats
        .iter()
        .filter(|format| playable(format))
        .cloned()
        .collect();
    formats.sort_by_key(|format| {
        let lossless = format.format == WaveFormat::PCM && format.bits_per_sample == 16;
        let rate = PREFERRED_RATES
            .iter()
            .position(|&rate| rate == format.n_samples_per_sec)
            .unwrap_or(PREFERRED_RATES.len());
        let channels = match format.n_channels {
            2 => 0,
            1 => 1,
            _ => 2,
        };
        (
            !lossless,
            rate,
            std::cmp::Reverse(format.n_samples_per_sec),
            channels,
        )
    });
    formats
}

fn playable(format: &AudioFormat) -> bool {
    (1..=MAX_CHANNELS).contains(&format.n_channels)
        && RATES.contains(&format.n_samples_per_sec)
        && AudioDecoder::supports(format)
}

/// 16-bit PCM at the preferred rates, stereo and mono.
fn pcm_formats() -> Vec<AudioFormat> {
    PREFERRED_RATES
        .iter()
        .flat_map(|&rate| [2, 1].map(|channels| pcm_format(rate, channels)))
        .collect()
}

fn pcm_format(rate: u32, channels: u16) -> AudioFormat {
    let block_align = channels * 2;
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: channels,
        n_samples_per_sec: rate,
        n_avg_bytes_per_sec: rate * u32::from(block_align),
        n_block_align: block_align,
        bits_per_sample: 16,
        data: None,
    }
}

/// What the channel handlers report to the session.
#[derive(Debug)]
enum SoundEvent {
    Wave {
        format: AudioFormat,
        data: Vec<u8>,
    },
    /// The server stopped playing (or renegotiates the formats).
    Close,
}

/// Creates the session side of the audio output and the sink its channel
/// handlers report to.
pub fn output(encoding: AudioEncoding) -> (SoundSink, AudioOutput) {
    let (sender, events) = mpsc::channel();
    let output = AudioOutput {
        events,
        encoding,
        stream: None,
        refused: false,
        decoder: None,
        samples: Vec::new(),
        bytes: Vec::new(),
    };
    (SoundSink(sender), output)
}

/// Creates the audio channel processors of a connection.
#[derive(Debug, Clone)]
pub struct SoundSink(mpsc::Sender<SoundEvent>);

impl SoundSink {
    /// Processor of the RDPSND static virtual channel.
    pub fn static_channel(&self) -> Rdpsnd {
        Rdpsnd::new(self.handler()).with_clock(clock())
    }

    /// Processor of the `AUDIO_PLAYBACK_DVC` dynamic virtual channel.
    pub fn dynamic_channel(&self) -> RdpsndDvc {
        RdpsndDvcState::new(self.handler())
            .with_clock(clock())
            .channel()
    }

    fn handler(&self) -> Box<dyn RdpsndClientHandler> {
        Box::new(SoundHandler {
            sink: self.0.clone(),
            formats: pcm_formats(),
        })
    }
}

/// Clock measuring how long wave blocks are held before being confirmed.
fn clock() -> ironrdp_rdpsnd::client::Clock {
    let origin = Instant::now();
    Box::new(move || origin.elapsed())
}

/// Queues the wave data of one audio channel for the session.
#[derive(Debug)]
struct SoundHandler {
    sink: mpsc::Sender<SoundEvent>,
    formats: Vec<AudioFormat>,
}

impl RdpsndClientHandler for SoundHandler {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_formats(&self, server_formats: &[AudioFormat]) -> Vec<AudioFormat> {
        let formats = preferred_formats(server_formats);
        if formats.is_empty() {
            warn!(
                ?server_formats,
                "the RDP server offers no playable audio format"
            );
        }
        formats
    }

    fn wave(&mut self, format: &AudioFormat, _timestamp: u32, data: Cow<'_, [u8]>) {
        // The receiver only goes away with the session.
        let _ = self.sink.send(SoundEvent::Wave {
            format: format.clone(),
            data: data.into_owned(),
        });
    }

    // Volume and pitch control are not advertised (no `VOLUME` / `PITCH`
    // flags), so servers apply them to the samples themselves.
    fn set_volume(&mut self, _: VolumePdu) {}

    fn set_pitch(&mut self, _: PitchPdu) {}

    fn close(&mut self) {
        let _ = self.sink.send(SoundEvent::Close);
    }
}

/// Sample rate and channel count of the Guacamole audio stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamFormat {
    rate: u32,
    channels: u16,
}

/// The Guacamole audio stream, fed with the queued wave data.
pub struct AudioOutput {
    events: mpsc::Receiver<SoundEvent>,
    encoding: AudioEncoding,
    /// Format of the open stream, if any.
    stream: Option<StreamFormat>,
    /// The client rejected the stream: nothing more is sent.
    refused: bool,
    /// Decoder of the last wave format.
    decoder: Option<(AudioFormat, AudioDecoder)>,
    samples: Vec<i16>,
    bytes: Vec<u8>,
}

impl AudioOutput {
    /// Sends the audio received since the last call.
    pub fn pump(&mut self, out: &mut String) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                SoundEvent::Wave { format, data } => self.wave(&format, &data, out),
                SoundEvent::Close => self.end(out),
            }
        }
    }

    /// Handles an `ack` from the client. Returns whether it concerned the
    /// audio stream.
    pub fn ack(&mut self, stream: i32, status: Status) -> bool {
        if stream != AUDIO_STREAM {
            return false;
        }
        if status.is_error() && !self.refused {
            warn!(
                status = status.code(),
                "the client refused the audio stream; audio disabled"
            );
            self.refused = true;
            self.stream = None;
        }
        true
    }

    fn wave(&mut self, format: &AudioFormat, data: &[u8], out: &mut String) {
        if self.refused {
            return;
        }
        let channels = usize::from(format.n_channels);
        let decoder = match &mut self.decoder {
            Some((current, decoder)) if current == format => decoder,
            slot => match AudioDecoder::new(format) {
                Ok(decoder) => &mut slot.insert((format.clone(), decoder)).1,
                Err(err) => {
                    debug!(error = %err, ?format, "dropping undecodable wave data");
                    return;
                }
            },
        };
        self.samples.clear();
        if let Err(err) = decoder.decode(data, &mut self.samples) {
            debug!(error = %err, "wave data partially decoded");
        }
        // Whole frames only, so channels never shift between blobs.
        self.samples
            .truncate(self.samples.len() - self.samples.len() % channels);
        if self.samples.is_empty() {
            return;
        }

        let wanted = StreamFormat {
            rate: format.n_samples_per_sec,
            channels: format.n_channels,
        };
        if self.stream != Some(wanted) {
            self.end(out);
            let mimetype = self.encoding.mimetype(wanted);
            info!(%mimetype, "starting audio stream");
            *out += &InstructionBuilder::new("audio")
                .arg(AUDIO_STREAM)
                .str_arg(&mimetype)
                .finish();
            self.stream = Some(wanted);
        }

        self.bytes.clear();
        self.encoding.encode(&self.samples, &mut self.bytes);
        let frame = self.encoding.bytes_per_sample() * channels;
        let mut b64 = String::new();
        for chunk in self.bytes.chunks(MAX_BLOB - MAX_BLOB % frame) {
            b64.clear();
            base64::engine::general_purpose::STANDARD.encode_string(chunk, &mut b64);
            *out += &InstructionBuilder::new("blob")
                .arg(AUDIO_STREAM)
                .str_arg(&b64)
                .finish();
        }
    }

    /// Ends the open stream; the next wave data opens a new one.
    fn end(&mut self, out: &mut String) {
        if self.stream.take().is_some() {
            debug!("ending audio stream");
            *out += &InstructionBuilder::new("end").arg(AUDIO_STREAM).finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_core::{decode, encode_vec};
    use ironrdp_rdpsnd::pdu::{
        ClientAudioOutputPdu, ServerAudioFormatPdu, ServerAudioOutputPdu, TrainingPdu, Version,
        Wave2Pdu,
    };
    use ironrdp_svc::SvcProcessor as _;
    use molcajete_protocol::{Instruction, parse_all};

    use super::*;

    fn format(tag: WaveFormat, rate: u32, channels: u16, bits: u16) -> AudioFormat {
        let block_align = channels * bits.div_ceil(8);
        AudioFormat {
            format: tag,
            n_channels: channels,
            n_samples_per_sec: rate,
            n_avg_bytes_per_sec: rate * u32::from(block_align),
            n_block_align: block_align,
            bits_per_sample: bits,
            data: None,
        }
    }

    fn pcm16_bytes(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    /// Sends wave data through a handler, as the channel processors do.
    fn play(sink: &SoundSink, format: &AudioFormat, data: &[u8]) {
        let mut handler = sink.handler();
        handler.wave(format, 0, Cow::Borrowed(data));
    }

    fn instructions(out: &str) -> Vec<Instruction> {
        parse_all(out).unwrap()
    }

    fn blob_bytes(ins: &Instruction) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(ins.arg(1))
            .unwrap()
    }

    #[test]
    fn negotiates_the_client_encoding() {
        let mimetypes = |list: &[&str]| list.iter().map(|m| m.to_string()).collect::<Vec<_>>();
        assert_eq!(
            AudioEncoding::negotiate(&mimetypes(&["audio/L8", "audio/L16"])),
            Some(AudioEncoding::L16)
        );
        assert_eq!(
            AudioEncoding::negotiate(&mimetypes(&["audio/ogg", "audio/l8;rate=8000"])),
            Some(AudioEncoding::L8)
        );
        assert_eq!(AudioEncoding::negotiate(&mimetypes(&["audio/ogg"])), None);
        assert_eq!(AudioEncoding::negotiate(&[]), None);
        assert_eq!(
            AudioEncoding::L16.mimetype(StreamFormat {
                rate: 44_100,
                channels: 2
            }),
            "audio/L16;rate=44100,channels=2"
        );
    }

    #[test]
    fn prefers_lossless_stereo_at_common_rates() {
        let server = [
            format(WaveFormat::PCM, 22_050, 2, 16),
            format(WaveFormat::GSM610, 44_100, 1, 0), // no decoder
            format(WaveFormat::PCM, 44_100, 1, 16),
            format(WaveFormat::ALAW, 8_000, 1, 8),
            format(WaveFormat::PCM, 11_025, 2, 16),
            format(WaveFormat::PCM, 48_000, 2, 16),
            format(WaveFormat::PCM, 44_100, 2, 16),
            format(WaveFormat::PCM, 44_100, 2, 24), // no decoder
        ];
        let chosen: Vec<_> = preferred_formats(&server)
            .iter()
            .map(|f| (f.format, f.n_samples_per_sec, f.n_channels))
            .collect();
        assert_eq!(
            chosen,
            [
                (WaveFormat::PCM, 44_100, 2),
                (WaveFormat::PCM, 44_100, 1),
                (WaveFormat::PCM, 48_000, 2),
                (WaveFormat::PCM, 22_050, 2),
                (WaveFormat::PCM, 11_025, 2),
                (WaveFormat::ALAW, 8_000, 1),
            ]
        );
        assert!(pcm_formats().iter().all(playable));
        assert_eq!(pcm_formats()[0], format(WaveFormat::PCM, 44_100, 2, 16));
    }

    #[test]
    fn streams_pcm_in_frame_aligned_blobs() {
        let (sink, mut output) = output(AudioEncoding::L16);
        let stereo = format(WaveFormat::PCM, 44_100, 2, 16);
        let samples: Vec<i16> = (0..8000).map(|i| (i * 7) as i16).collect();
        play(&sink, &stereo, &pcm16_bytes(&samples));
        let mut out = String::new();
        output.pump(&mut out);

        let ins = instructions(&out);
        assert_eq!(ins[0].opcode, "audio");
        assert_eq!(ins[0].args, ["128", "audio/L16;rate=44100,channels=2"]);
        let mut received = Vec::new();
        for blob in &ins[1..] {
            assert_eq!(blob.opcode, "blob");
            let bytes = blob_bytes(blob);
            assert!(bytes.len() <= MAX_BLOB && bytes.len().is_multiple_of(4));
            received.extend(bytes);
        }
        assert_eq!(received, pcm16_bytes(&samples));

        // Same format: no new stream.
        out.clear();
        play(&sink, &stereo, &pcm16_bytes(&[1, 2]));
        output.pump(&mut out);
        let ins = instructions(&out);
        assert_eq!(ins.len(), 1);
        assert_eq!(blob_bytes(&ins[0]), pcm16_bytes(&[1, 2]));
    }

    #[test]
    fn format_changes_and_close_reopen_the_stream() {
        let (sink, mut output) = output(AudioEncoding::L16);
        let mut out = String::new();
        play(&sink, &format(WaveFormat::PCM, 44_100, 2, 16), &[0; 8]);
        play(&sink, &format(WaveFormat::PCM, 22_050, 1, 16), &[0; 8]);
        sink.handler().close();
        output.pump(&mut out);
        let opcodes: Vec<_> = instructions(&out)
            .into_iter()
            .map(|i| (i.opcode, i.args.get(1).cloned().unwrap_or_default()))
            .collect();
        assert_eq!(
            opcodes,
            [
                ("audio".into(), "audio/L16;rate=44100,channels=2".into()),
                ("blob".into(), "AAAAAAAAAAA=".into()),
                ("end".into(), String::new()),
                ("audio".into(), "audio/L16;rate=22050,channels=1".into()),
                ("blob".into(), "AAAAAAAAAAA=".into()),
                ("end".into(), String::new()),
            ]
        );
        // A close without an open stream sends nothing.
        out.clear();
        sink.handler().close();
        output.pump(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn other_codecs_are_decoded_and_narrowed_for_l8_clients() {
        let (sink, mut output) = output(AudioEncoding::L8);
        let mut out = String::new();
        // 8-bit PCM is unsigned: 0x80 is silence, 0xFF the maximum.
        play(
            &sink,
            &format(WaveFormat::PCM, 8_000, 1, 8),
            &[0x80, 0xFF, 0x00],
        );
        // A trailing partial frame is dropped.
        play(
            &sink,
            &format(WaveFormat::PCM, 8_000, 2, 16),
            &pcm16_bytes(&[256, -256, 7]),
        );
        output.pump(&mut out);
        let ins = instructions(&out);
        assert_eq!(ins[0].args[1], "audio/L8;rate=8000,channels=1");
        assert_eq!(blob_bytes(&ins[1]), [0x00, 0x7F, 0x80]);
        assert_eq!(ins[2].opcode, "end");
        assert_eq!(ins[3].args[1], "audio/L8;rate=8000,channels=2");
        assert_eq!(blob_bytes(&ins[4]), [0x01, 0xFF]);
    }

    #[test]
    fn a_refused_stream_stops_the_audio() {
        let (sink, mut output) = output(AudioEncoding::L16);
        let stereo = format(WaveFormat::PCM, 44_100, 2, 16);
        let mut out = String::new();
        play(&sink, &stereo, &[0; 4]);
        output.pump(&mut out);
        assert!(output.ack(AUDIO_STREAM, Status::SUCCESS));
        assert!(!output.ack(3, Status::CLIENT_BAD_TYPE));
        out.clear();
        play(&sink, &stereo, &[0; 4]);
        output.pump(&mut out);
        assert_eq!(instructions(&out).len(), 1, "accepted: {out}");

        assert!(output.ack(AUDIO_STREAM, Status::CLIENT_BAD_TYPE));
        out.clear();
        play(&sink, &stereo, &[0; 4]);
        sink.handler().close();
        output.pump(&mut out);
        assert!(out.is_empty(), "refused: {out}");
    }

    fn reply(messages: Vec<ironrdp_svc::SvcMessage>) -> Vec<ClientAudioOutputPdu> {
        messages
            .iter()
            .map(|m| decode(&m.encode_unframed_pdu().unwrap()).unwrap())
            .collect()
    }

    fn server_pdu(pdu: ServerAudioOutputPdu<'_>) -> Vec<u8> {
        encode_vec(&pdu).unwrap()
    }

    #[test]
    fn rdpsnd_negotiates_and_confirms_waves() {
        let (sink, mut output) = output(AudioEncoding::L16);
        let mut channel = sink.static_channel();
        let server_formats = vec![
            format(WaveFormat::PCM, 22_050, 2, 16),
            format(WaveFormat::PCM, 44_100, 2, 16),
        ];
        let replies = reply(
            channel
                .process(&server_pdu(ServerAudioOutputPdu::AudioFormat(
                    ServerAudioFormatPdu {
                        version: Version::V6,
                        formats: server_formats,
                    },
                )))
                .unwrap(),
        );
        let ClientAudioOutputPdu::AudioFormat(client) = &replies[0] else {
            panic!("unexpected reply: {replies:?}");
        };
        assert_eq!(client.formats[0].n_samples_per_sec, 44_100);
        assert_eq!(client.formats.len(), 2);

        let replies = reply(
            channel
                .process(&server_pdu(ServerAudioOutputPdu::Training(TrainingPdu {
                    timestamp: 42,
                    data: vec![0; 16],
                })))
                .unwrap(),
        );
        assert!(matches!(
            &replies[..],
            [ClientAudioOutputPdu::TrainingConfirm(c)] if c.timestamp == 42 && c.pack_size == 16
        ));

        let samples = pcm16_bytes(&[1, -1, 2, -2]);
        let replies = reply(
            channel
                .process(&server_pdu(ServerAudioOutputPdu::Wave2(Wave2Pdu {
                    timestamp: 1000,
                    format_no: 0,
                    block_no: 7,
                    audio_timestamp: 0,
                    data: Cow::Borrowed(&samples),
                })))
                .unwrap(),
        );
        assert!(matches!(
            &replies[..],
            [ClientAudioOutputPdu::WaveConfirm(c)] if c.block_no == 7 && c.timestamp >= 1000
        ));

        let mut out = String::new();
        output.pump(&mut out);
        let ins = instructions(&out);
        assert_eq!(ins[0].args[1], "audio/L16;rate=44100,channels=2");
        assert_eq!(blob_bytes(&ins[1]), samples);
    }
}

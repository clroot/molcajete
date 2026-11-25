//! Building blocks shared by the protocol implementations (VNC, SSH, RDP):
//! reading client instructions in a background task, decoding them into
//! [`ClientEvent`]s (including reassembly of inbound `clipboard` / `argv`
//! streams), and a few common outbound messages.

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use base64::Engine;
use molcajete_protocol::video::VideoCodec;
use molcajete_protocol::{Instruction, InstructionBuilder, Status, apply_time};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::info;

use crate::display::{Display, SyncReply};
use crate::handshake::ClientInfo;
use crate::io::{GuacReader, GuacWriter, Output};
use crate::video::VideoEncoders;

/// Input from the Guacamole client, already decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    /// Pointer position and button mask (1 left, 2 middle, 4 right,
    /// 8 wheel up, 16 wheel down).
    Mouse {
        x: i32,
        y: i32,
        mask: u32,
    },
    /// X11 keysym pressed or released.
    Key {
        keysym: u32,
        pressed: bool,
    },
    /// The client display changed size (pixels).
    Size {
        width: u32,
        height: u32,
    },
    /// Complete clipboard contents sent by the client.
    Clipboard(String),
    /// A value for a parameter requested with `required`.
    Argv {
        name: String,
        value: String,
    },
    /// The client finished drawing the frame with this timestamp.
    /// `received` says when that reached us, which can be well before the
    /// session loop gets to it (while it builds the next frame, for
    /// example), and how long the client took to apply the frame if it
    /// reports that.
    Sync {
        timestamp: i64,
        received: SyncReply,
    },
    /// The client acknowledged (or rejected) data of an outbound stream.
    Ack {
        stream: i32,
        status: Status,
    },
    Disconnect,
}

enum InboundKind {
    Clipboard,
    Argv(String),
}

struct Inbound {
    kind: InboundKind,
    data: Vec<u8>,
}

/// An instruction from the client and when it was read from the socket.
pub struct Input {
    pub instruction: Instruction,
    pub received: Instant,
    /// Passed on from a user who joined the connection (see
    /// [`crate::share`]): what it owes in return (stream acknowledgements)
    /// went to that user already and is not sent to this client.
    pub from_viewer: bool,
}

impl Deref for Input {
    type Target = Instruction;

    fn deref(&self) -> &Instruction {
        &self.instruction
    }
}

/// Most streams a client may have open towards molcajeted at once (libguac
/// allows 64 per user).
const MAX_INBOUND_STREAMS: usize = 64;

/// Most data buffered for a client's open streams together. A clipboard of
/// a few megabytes is plausible; a stream that never ends is not, and would
/// otherwise grow until the daemon runs out of memory.
const MAX_INBOUND_BYTES: usize = 16 * 1024 * 1024;

/// Turns raw client instructions into [`ClientEvent`]s.
#[derive(Default)]
pub struct InputDecoder {
    streams: HashMap<i32, Inbound>,
}

impl InputDecoder {
    /// Decodes one instruction. Acknowledgements owed to the client are
    /// appended to `out`.
    pub fn decode(&mut self, ins: &Input, out: &mut Output) -> Option<ClientEvent> {
        let mut elsewhere;
        let out = if ins.from_viewer {
            elsewhere = Output::text();
            &mut elsewhere
        } else {
            out
        };
        let int = |i: usize| ins.arg(i).trim().parse::<i64>().ok();
        match ins.opcode.as_str() {
            "mouse" => Some(ClientEvent::Mouse {
                x: int(0)?.clamp(0, i32::MAX as i64) as i32,
                y: int(1)?.clamp(0, i32::MAX as i64) as i32,
                mask: int(2).unwrap_or(0).clamp(0, u32::MAX as i64) as u32,
            }),
            "key" => Some(ClientEvent::Key {
                keysym: int(0)?.clamp(0, u32::MAX as i64) as u32,
                pressed: ins.arg(1) == "1",
            }),
            "size" => {
                let (w, h) = (int(0)?, int(1)?);
                (w > 0 && h > 0).then(|| ClientEvent::Size {
                    width: w.min(8192) as u32,
                    height: h.min(8192) as u32,
                })
            }
            "sync" => int(0).map(|timestamp| ClientEvent::Sync {
                timestamp,
                received: SyncReply {
                    at: ins.received,
                    apply: apply_time::parse_reply(ins.arg(2)),
                },
            }),
            "ack" => Some(ClientEvent::Ack {
                stream: int(0)? as i32,
                status: Status::parse(ins.arg(2))?,
            }),
            "disconnect" => Some(ClientEvent::Disconnect),
            "clipboard" => {
                let stream = int(0)? as i32;
                if !self.may_open(stream, out) {
                    return None;
                }
                self.streams.insert(
                    stream,
                    Inbound {
                        kind: InboundKind::Clipboard,
                        data: Vec::new(),
                    },
                );
                ack(out, stream, "OK", Status::SUCCESS);
                None
            }
            "argv" => {
                let stream = int(0)? as i32;
                if !self.may_open(stream, out) {
                    return None;
                }
                let name = ins.arg(2).to_owned();
                self.streams.insert(
                    stream,
                    Inbound {
                        kind: InboundKind::Argv(name),
                        data: Vec::new(),
                    },
                );
                ack(out, stream, "OK", Status::SUCCESS);
                None
            }
            "blob" => {
                let stream = int(0)? as i32;
                let data = ins.arg(1);
                let buffered: usize = self.streams.values().map(|s| s.data.len()).sum();
                // Base64 decodes to at most three bytes for every four.
                if self.streams.contains_key(&stream)
                    && buffered + data.len() / 4 * 3 > MAX_INBOUND_BYTES
                {
                    self.streams.remove(&stream);
                    ack(out, stream, "Too much data", Status::CLIENT_OVERRUN);
                    return None;
                }
                match self.streams.get_mut(&stream) {
                    Some(inbound) => {
                        let b64 = base64::engine::general_purpose::STANDARD;
                        if b64.decode_vec(data.as_bytes(), &mut inbound.data).is_ok() {
                            ack(out, stream, "OK", Status::SUCCESS);
                        } else {
                            ack(out, stream, "Invalid base64", Status::CLIENT_BAD_REQUEST);
                        }
                    }
                    None => ack(out, stream, "No such stream", Status::CLIENT_BAD_REQUEST),
                }
                None
            }
            "end" => {
                let stream = int(0)? as i32;
                let inbound = self.streams.remove(&stream)?;
                let text = String::from_utf8_lossy(&inbound.data).into_owned();
                Some(match inbound.kind {
                    InboundKind::Clipboard => ClientEvent::Clipboard(text),
                    InboundKind::Argv(name) => ClientEvent::Argv { name, value: text },
                })
            }
            "file" | "pipe" | "audio" | "put" => {
                if let Some(stream) = int(0) {
                    ack(out, stream as i32, "Unsupported", Status::UNSUPPORTED);
                }
                None
            }
            _ => None,
        }
    }

    /// Whether the client may open (or reopen) `stream`; acknowledges the
    /// refusal if not.
    fn may_open(&self, stream: i32, out: &mut Output) -> bool {
        if self.streams.len() < MAX_INBOUND_STREAMS || self.streams.contains_key(&stream) {
            return true;
        }
        ack(out, stream, "Too many streams", Status::CLIENT_TOO_MANY);
        false
    }
}

/// Reads client instructions in a background task. The channel closes when
/// the client disconnects.
///
/// In a connection others may join, the channel also carries the input of
/// the joined users who may control it ([`crate::share`]).
pub fn spawn_reader(mut reader: GuacReader) -> (mpsc::Receiver<Input>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Input>(512);
    if let Some(shared) = crate::share::current() {
        shared.set_input(tx.clone());
    }
    let task = tokio::spawn(async move {
        while let Ok(Some(instruction)) = reader.next().await {
            let input = Input {
                instruction,
                received: Instant::now(),
                from_viewer: false,
            };
            if tx.send(input).await.is_err() {
                break;
            }
        }
    });
    (rx, task)
}

pub fn ack(out: &mut Output, stream: i32, message: &str, status: Status) {
    *out += &InstructionBuilder::new("ack")
        .arg(stream)
        .str_arg(message)
        .arg(status.code())
        .finish();
}

/// Stream index used for clipboard data sent to the client.
const CLIPBOARD_STREAM: i32 = 100;

/// Sends text to the client clipboard.
pub fn send_clipboard(out: &mut Output, text: &str) {
    *out += &InstructionBuilder::new("clipboard")
        .arg(CLIPBOARD_STREAM)
        .str_arg("text/plain")
        .finish();
    out.blob(CLIPBOARD_STREAM, text.as_bytes());
    *out += &InstructionBuilder::new("end")
        .arg(CLIPBOARD_STREAM)
        .finish();
}

/// Asks the client for missing parameters (e.g. credentials); the answers
/// arrive as [`ClientEvent::Argv`]. Requires protocol version 1.3.0 or later.
pub fn send_required(out: &mut Output, names: &[&str]) {
    *out += &InstructionBuilder::new("required").args(names).finish();
}

/// Sends `required` for `names` and waits until the client answered every
/// one, keeping the connection alive with `nop`s. Other input is dropped.
/// `None` if the client left.
pub async fn ask_required(
    inputs: &mut mpsc::Receiver<Input>,
    decoder: &mut InputDecoder,
    writer: &mut GuacWriter,
    names: &[&str],
) -> std::io::Result<Option<Vec<(String, String)>>> {
    const KEEPALIVE: Duration = Duration::from_secs(5);
    send_required(writer.buffer(), names);
    writer.flush().await?;
    let mut answers: Vec<(String, String)> = Vec::new();
    let mut keepalive =
        tokio::time::interval_at(tokio::time::Instant::now() + KEEPALIVE, KEEPALIVE);
    while answers.len() < names.len() {
        tokio::select! {
            ins = inputs.recv() => {
                let Some(ins) = ins else { return Ok(None) };
                match decoder.decode(&ins, writer.buffer()) {
                    Some(ClientEvent::Disconnect) => return Ok(None),
                    Some(ClientEvent::Argv { name, value }) if names.contains(&name.as_str()) => {
                        answers.retain(|(n, _)| *n != name);
                        answers.push((name, value));
                    }
                    _ => {}
                }
            }
            _ = keepalive.tick() => writer.push("3.nop;"),
        }
        if writer.pending() > 0 {
            writer.flush().await?;
        }
    }
    Ok(Some(answers))
}

/// Sets the connection name shown by the client.
pub fn send_name(out: &mut Output, name: &str) {
    *out += &InstructionBuilder::new("name").str_arg(name).finish();
}

/// Video encoders found at startup and the codecs the operator allows.
struct VideoSetup {
    encoders: VideoEncoders,
    allowed: Vec<VideoCodec>,
}

static VIDEO: OnceLock<VideoSetup> = OnceLock::new();

/// Registers the video encoders of this machine (call once at startup).
pub fn init_video(encoders: VideoEncoders, allowed: Vec<VideoCodec>) {
    let _ = VIDEO.set(VideoSetup { encoders, allowed });
}

/// Turns on video mode for busy screen areas, with the most efficient codec
/// that the client can decode (from the `video` handshake instruction) and
/// this server can encode.
pub fn enable_video(display: &mut Display, info: &ClientInfo) {
    let Some(setup) = VIDEO.get() else {
        return;
    };
    let offered: Vec<VideoCodec> = info
        .video
        .iter()
        .filter_map(|m| VideoCodec::from_mimetype(m))
        .collect();
    let available = setup.encoders.available();
    // In the operator's order of preference, each with the largest area it
    // can encode in real time.
    let codecs: Vec<(VideoCodec, u64)> = setup
        .allowed
        .iter()
        .filter(|c| offered.contains(c) && available.contains(c))
        .map(|&c| (c, setup.encoders.realtime_pixels(c)))
        .collect();
    if codecs.is_empty() {
        return;
    }
    info!(codecs = ?codecs.iter().map(|c| c.0).collect::<Vec<_>>(), "video mode available");
    let encoders = &setup.encoders;
    display.enable_video(codecs, Box::new(move |config| encoders.create(config)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use molcajete_protocol::parse_all;
    use std::time::Duration;

    fn ins(text: &str) -> Input {
        Input {
            instruction: parse_all(text).unwrap().remove(0),
            received: Instant::now(),
            from_viewer: false,
        }
    }

    #[test]
    fn decodes_input_events() {
        let mut d = InputDecoder::default();
        let mut out = Output::text();
        assert_eq!(
            d.decode(&ins("5.mouse,2.10,2.20,1.5;"), &mut out),
            Some(ClientEvent::Mouse {
                x: 10,
                y: 20,
                mask: 5
            })
        );
        assert_eq!(
            d.decode(&ins("3.key,5.65293,1.1;"), &mut out),
            Some(ClientEvent::Key {
                keysym: 0xFF0D,
                pressed: true
            })
        );
        // Stock clients: the timestamp, perhaps a frame count.
        for text in ["4.sync,3.123;", "4.sync,3.123,1.1;"] {
            let sync = ins(text);
            assert_eq!(
                d.decode(&sync, &mut out),
                Some(ClientEvent::Sync {
                    timestamp: 123,
                    received: sync.received.into(),
                }),
                "{text}"
            );
        }
        // A molcajete client adds the time it took to apply the frame.
        let sync = ins(&apply_time::sync_reply(
            123,
            1,
            Duration::from_micros(17_500),
        ));
        assert_eq!(
            d.decode(&sync, &mut out),
            Some(ClientEvent::Sync {
                timestamp: 123,
                received: SyncReply {
                    at: sync.received,
                    apply: Some(Duration::from_micros(17_500)),
                },
            })
        );
        assert_eq!(d.decode(&ins("4.size,1.0,3.768;"), &mut out), None);
        assert!(out.is_empty());
    }

    #[test]
    fn reassembles_clipboard_and_argv_streams() {
        let mut d = InputDecoder::default();
        let mut out = Output::text();
        assert_eq!(
            d.decode(&ins("9.clipboard,1.3,10.text/plain;"), &mut out),
            None
        );
        assert_eq!(d.decode(&ins("4.blob,1.3,4.7ZWc;"), &mut out), None);
        assert_eq!(d.decode(&ins("4.blob,1.3,4.6riA;"), &mut out), None);
        assert_eq!(
            d.decode(&ins("3.end,1.3;"), &mut out),
            Some(ClientEvent::Clipboard("한글".into()))
        );
        assert_eq!(parse_all(&out).unwrap().len(), 3, "acks: {out}");

        out.clear();
        d.decode(&ins("4.argv,1.4,10.text/plain,8.password;"), &mut out);
        d.decode(&ins("4.blob,1.4,4.c2Vj;"), &mut out);
        assert_eq!(
            d.decode(&ins("3.end,1.4;"), &mut out),
            Some(ClientEvent::Argv {
                name: "password".into(),
                value: "sec".into()
            })
        );
    }

    fn statuses(out: &Output) -> Vec<String> {
        parse_all(out)
            .unwrap()
            .iter()
            .map(|a| a.arg(2).to_owned())
            .collect()
    }

    /// A client cannot make molcajeted buffer more than `MAX_INBOUND_BYTES`
    /// by sending a stream that never ends.
    #[test]
    fn inbound_data_is_bounded() {
        let mut d = InputDecoder::default();
        let mut out = Output::text();
        d.decode(&ins("9.clipboard,1.1,10.text/plain;"), &mut out);
        // 24 KiB of base64 per blob, 18 KiB decoded.
        let blob = format!("4.blob,1.1,24576.{};", "A".repeat(24 * 1024));
        let mut accepted = 0;
        loop {
            out.clear();
            d.decode(&ins(&blob), &mut out);
            if statuses(&out) != ["0"] {
                break;
            }
            accepted += 18 * 1024;
        }
        assert!(accepted <= MAX_INBOUND_BYTES, "{accepted}");
        assert!(accepted + 18 * 1024 > MAX_INBOUND_BYTES, "{accepted}");
        assert_eq!(statuses(&out), [Status::CLIENT_OVERRUN.code().to_string()]);
        // The stream is gone.
        assert_eq!(d.decode(&ins("3.end,1.1;"), &mut out), None);
        // A new one may start.
        out.clear();
        d.decode(&ins("9.clipboard,1.2,10.text/plain;"), &mut out);
        d.decode(&ins("4.blob,1.2,4.aGk=;"), &mut out);
        assert_eq!(statuses(&out), ["0", "0"]);
        assert_eq!(
            d.decode(&ins("3.end,1.2;"), &mut out),
            Some(ClientEvent::Clipboard("hi".into()))
        );
    }

    #[test]
    fn open_inbound_streams_are_bounded() {
        let mut d = InputDecoder::default();
        let mut out = Output::text();
        for stream in 0..MAX_INBOUND_STREAMS {
            let argv = format!(
                "4.argv,{}.{stream},10.text/plain,4.name;",
                stream.to_string().len()
            );
            d.decode(&ins(&argv), &mut out);
        }
        assert!(statuses(&out).iter().all(|status| status == "0"));
        out.clear();
        d.decode(&ins("9.clipboard,3.999,10.text/plain;"), &mut out);
        assert_eq!(statuses(&out), [Status::CLIENT_TOO_MANY.code().to_string()]);
        // Reopening one that is open, or opening one after another ended,
        // is fine.
        out.clear();
        d.decode(&ins("9.clipboard,1.0,10.text/plain;"), &mut out);
        d.decode(&ins("3.end,1.1;"), &mut out);
        d.decode(&ins("9.clipboard,3.999,10.text/plain;"), &mut out);
        assert_eq!(statuses(&out), ["0", "0"]);
    }
}

//! Building blocks shared by the protocol implementations (VNC, SSH, RDP):
//! reading client instructions in a background task, decoding them into
//! [`ClientEvent`]s (including reassembly of inbound `clipboard` / `argv`
//! streams), and a few common outbound messages.

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::OnceLock;
use std::time::Instant;

use base64::Engine;
use molcajete_protocol::video::VideoCodec;
use molcajete_protocol::{Instruction, InstructionBuilder, Status};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::info;

use crate::display::Display;
use crate::handshake::ClientInfo;
use crate::io::GuacReader;
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
    /// The client finished drawing the frame with this timestamp. `received`
    /// is when that reached us, which can be well before the session loop
    /// gets to it (while it builds the next frame, for example).
    Sync {
        timestamp: i64,
        received: Instant,
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
}

impl Deref for Input {
    type Target = Instruction;

    fn deref(&self) -> &Instruction {
        &self.instruction
    }
}

/// Turns raw client instructions into [`ClientEvent`]s.
#[derive(Default)]
pub struct InputDecoder {
    streams: HashMap<i32, Inbound>,
}

impl InputDecoder {
    /// Decodes one instruction. Acknowledgements owed to the client are
    /// appended to `out`.
    pub fn decode(&mut self, ins: &Input, out: &mut String) -> Option<ClientEvent> {
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
                received: ins.received,
            }),
            "ack" => Some(ClientEvent::Ack {
                stream: int(0)? as i32,
                status: Status::parse(ins.arg(2))?,
            }),
            "disconnect" => Some(ClientEvent::Disconnect),
            "clipboard" => {
                let stream = int(0)? as i32;
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
                match self.streams.get_mut(&stream) {
                    Some(inbound) => {
                        let b64 = base64::engine::general_purpose::STANDARD;
                        if b64
                            .decode_vec(ins.arg(1).as_bytes(), &mut inbound.data)
                            .is_ok()
                        {
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
}

/// Reads client instructions in a background task. The channel closes when
/// the client disconnects.
pub fn spawn_reader(mut reader: GuacReader) -> (mpsc::Receiver<Input>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Input>(512);
    let task = tokio::spawn(async move {
        while let Ok(Some(instruction)) = reader.next().await {
            let input = Input {
                instruction,
                received: Instant::now(),
            };
            if tx.send(input).await.is_err() {
                break;
            }
        }
    });
    (rx, task)
}

pub fn ack(out: &mut String, stream: i32, message: &str, status: Status) {
    *out += &InstructionBuilder::new("ack")
        .arg(stream)
        .str_arg(message)
        .arg(status.code())
        .finish();
}

/// Stream index used for clipboard data sent to the client.
const CLIPBOARD_STREAM: i32 = 100;

/// Sends text to the client clipboard.
pub fn send_clipboard(out: &mut String, text: &str) {
    let b64 = base64::engine::general_purpose::STANDARD;
    *out += &InstructionBuilder::new("clipboard")
        .arg(CLIPBOARD_STREAM)
        .str_arg("text/plain")
        .finish();
    for chunk in text.as_bytes().chunks(4096) {
        *out += &InstructionBuilder::new("blob")
            .arg(CLIPBOARD_STREAM)
            .str_arg(&b64.encode(chunk))
            .finish();
    }
    *out += &InstructionBuilder::new("end")
        .arg(CLIPBOARD_STREAM)
        .finish();
}

/// Asks the client for missing parameters (e.g. credentials); the answers
/// arrive as [`ClientEvent::Argv`]. Requires protocol version 1.3.0 or later.
pub fn send_required(out: &mut String, names: &[&str]) {
    *out += &InstructionBuilder::new("required").args(names).finish();
}

/// Sets the connection name shown by the client.
pub fn send_name(out: &mut String, name: &str) {
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

    fn ins(text: &str) -> Input {
        Input {
            instruction: parse_all(text).unwrap().remove(0),
            received: Instant::now(),
        }
    }

    #[test]
    fn decodes_input_events() {
        let mut d = InputDecoder::default();
        let mut out = String::new();
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
        let sync = ins("4.sync,3.123;");
        assert_eq!(
            d.decode(&sync, &mut out),
            Some(ClientEvent::Sync {
                timestamp: 123,
                received: sync.received
            })
        );
        assert_eq!(d.decode(&ins("4.size,1.0,3.768;"), &mut out), None);
        assert!(out.is_empty());
    }

    #[test]
    fn reassembles_clipboard_and_argv_streams() {
        let mut d = InputDecoder::default();
        let mut out = String::new();
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
}

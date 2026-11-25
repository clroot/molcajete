//! Typed view of instructions sent by the server (guacd) to the client.
//!
//! Decoding borrows string arguments from the source instruction, so large
//! payloads such as base64 `blob` data are never copied.

use crate::{
    CompositeMode, Instruction, LineCap, LineJoin, RawInstruction, Status, TransferFunction,
};

pub type LayerIndex = i32;
pub type StreamIndex = i32;
pub type ObjectIndex = i32;

/// Anything that exposes an opcode and positional string arguments.
pub trait InstructionArgs<'a> {
    fn opcode(&self) -> &'a str;
    fn arg_count(&self) -> usize;
    fn arg(&self, index: usize) -> &'a str;
}

impl<'a> InstructionArgs<'a> for RawInstruction<'a> {
    fn opcode(&self) -> &'a str {
        RawInstruction::opcode(self)
    }
    fn arg_count(&self) -> usize {
        RawInstruction::arg_count(self)
    }
    fn arg(&self, index: usize) -> &'a str {
        RawInstruction::arg(self, index)
    }
}

impl<'a> InstructionArgs<'a> for &'a Instruction {
    fn opcode(&self) -> &'a str {
        &self.opcode
    }
    fn arg_count(&self) -> usize {
        self.args.len()
    }
    fn arg(&self, index: usize) -> &'a str {
        self.args.get(index).map(String::as_str).unwrap_or("")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed `{opcode}` instruction: argument {index} ({value:?}) is invalid")]
pub struct DecodeError {
    pub opcode: String,
    pub index: usize,
    pub value: String,
}

/// 2D affine matrix `[a, b, c, d, e, f]` as used by canvas `setTransform`.
pub type Matrix = [f64; 6];

#[derive(Debug, Clone, PartialEq)]
pub enum ServerInstruction<'a> {
    // ---- Drawing -------------------------------------------------------
    Arc {
        layer: LayerIndex,
        x: i32,
        y: i32,
        radius: i32,
        start: f64,
        end: f64,
        negative: bool,
    },
    Cfill {
        mode: CompositeMode,
        layer: LayerIndex,
        r: u8,
        g: u8,
        b: u8,
        a: u8,
    },
    Clip {
        layer: LayerIndex,
    },
    Close {
        layer: LayerIndex,
    },
    Copy {
        src: LayerIndex,
        sx: i32,
        sy: i32,
        w: i32,
        h: i32,
        mode: CompositeMode,
        dst: LayerIndex,
        dx: i32,
        dy: i32,
    },
    Cstroke {
        mode: CompositeMode,
        layer: LayerIndex,
        cap: LineCap,
        join: LineJoin,
        thickness: i32,
        r: u8,
        g: u8,
        b: u8,
        a: u8,
    },
    Cursor {
        hotspot_x: i32,
        hotspot_y: i32,
        src: LayerIndex,
        sx: i32,
        sy: i32,
        w: i32,
        h: i32,
    },
    Curve {
        layer: LayerIndex,
        cp1x: i32,
        cp1y: i32,
        cp2x: i32,
        cp2y: i32,
        x: i32,
        y: i32,
    },
    Dispose {
        layer: LayerIndex,
    },
    Distort {
        layer: LayerIndex,
        matrix: Matrix,
    },
    Identity {
        layer: LayerIndex,
    },
    Lfill {
        mode: CompositeMode,
        layer: LayerIndex,
        src: LayerIndex,
    },
    Line {
        layer: LayerIndex,
        x: i32,
        y: i32,
    },
    Lstroke {
        mode: CompositeMode,
        layer: LayerIndex,
        cap: LineCap,
        join: LineJoin,
        thickness: i32,
        src: LayerIndex,
    },
    Move {
        layer: LayerIndex,
        parent: LayerIndex,
        x: i32,
        y: i32,
        z: i32,
    },
    Pop {
        layer: LayerIndex,
    },
    Push {
        layer: LayerIndex,
    },
    Rect {
        layer: LayerIndex,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    Reset {
        layer: LayerIndex,
    },
    Set {
        layer: LayerIndex,
        name: &'a str,
        value: &'a str,
    },
    Shade {
        layer: LayerIndex,
        opacity: u8,
    },
    Size {
        layer: LayerIndex,
        w: i32,
        h: i32,
    },
    Start {
        layer: LayerIndex,
        x: i32,
        y: i32,
    },
    Transfer {
        src: LayerIndex,
        sx: i32,
        sy: i32,
        w: i32,
        h: i32,
        function: TransferFunction,
        dst: LayerIndex,
        dx: i32,
        dy: i32,
    },
    Transform {
        layer: LayerIndex,
        matrix: Matrix,
    },

    // ---- Images --------------------------------------------------------
    /// Begins an image stream whose data follows in `blob` instructions.
    Img {
        stream: StreamIndex,
        mode: CompositeMode,
        layer: LayerIndex,
        mimetype: &'a str,
        x: i32,
        y: i32,
    },
    /// Deprecated inline base64 PNG.
    Png {
        mode: CompositeMode,
        layer: LayerIndex,
        x: i32,
        y: i32,
        data: &'a str,
    },
    /// Deprecated inline base64 JPEG.
    Jpeg {
        mode: CompositeMode,
        layer: LayerIndex,
        x: i32,
        y: i32,
        data: &'a str,
    },

    // ---- Streams and objects -------------------------------------------
    Ack {
        stream: StreamIndex,
        message: &'a str,
        status: Status,
    },
    Argv {
        stream: StreamIndex,
        mimetype: &'a str,
        name: &'a str,
    },
    Audio {
        stream: StreamIndex,
        mimetype: &'a str,
    },
    Blob {
        stream: StreamIndex,
        data: &'a str,
    },
    Body {
        object: ObjectIndex,
        stream: StreamIndex,
        mimetype: &'a str,
        name: &'a str,
    },
    Clipboard {
        stream: StreamIndex,
        mimetype: &'a str,
    },
    End {
        stream: StreamIndex,
    },
    File {
        stream: StreamIndex,
        mimetype: &'a str,
        filename: &'a str,
    },
    Filesystem {
        object: ObjectIndex,
        name: &'a str,
    },
    Nest {
        index: i32,
        data: &'a str,
    },
    Pipe {
        stream: StreamIndex,
        mimetype: &'a str,
        name: &'a str,
    },
    Undefine {
        object: ObjectIndex,
    },
    Video {
        stream: StreamIndex,
        layer: LayerIndex,
        mimetype: &'a str,
    },

    // ---- Session control -----------------------------------------------
    Disconnect,
    /// Whether the server can change the remote display's size and scale
    /// during the session (molcajeted, RDP). Stock guacd never sends it.
    DisplayControl {
        available: bool,
    },
    Error {
        message: &'a str,
        status: Status,
    },
    Log {
        message: &'a str,
    },
    /// Server-side mouse position (e.g. another user moving the cursor).
    Mouse {
        x: i32,
        y: i32,
    },
    Msg {
        id: i32,
    },
    Name {
        name: &'a str,
    },
    Nop,
    Ready {
        id: &'a str,
    },
    /// Parameters the server needs before it can continue (e.g. credentials).
    Required,
    /// End of a frame. `frames` is the number of logical frames combined.
    /// `apply_time`: the server asks for the time the frame took to apply
    /// (molcajete extension, [`crate::apply_time`]).
    Sync {
        timestamp: i64,
        frames: u32,
        apply_time: bool,
    },
    /// Any opcode this decoder does not know about.
    Unknown {
        opcode: &'a str,
    },
}

struct Decoder<'i, I> {
    ins: &'i I,
}

impl<'a, I: InstructionArgs<'a>> Decoder<'_, I> {
    fn err(&self, index: usize) -> DecodeError {
        DecodeError {
            opcode: self.ins.opcode().to_owned(),
            index,
            value: self.ins.arg(index).to_owned(),
        }
    }

    fn s(&self, index: usize) -> &'a str {
        self.ins.arg(index)
    }

    fn i32(&self, index: usize) -> Result<i32, DecodeError> {
        parse_int(self.ins.arg(index))
            .and_then(|v| i32::try_from(v).ok())
            .ok_or_else(|| self.err(index))
    }

    fn i64(&self, index: usize) -> Result<i64, DecodeError> {
        parse_int(self.ins.arg(index)).ok_or_else(|| self.err(index))
    }

    fn u8(&self, index: usize) -> Result<u8, DecodeError> {
        Ok(self.i32(index)?.clamp(0, 255) as u8)
    }

    fn f64(&self, index: usize) -> Result<f64, DecodeError> {
        self.ins
            .arg(index)
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| self.err(index))
    }

    fn mode(&self, index: usize) -> Result<CompositeMode, DecodeError> {
        Ok(CompositeMode((self.i32(index)? & 0xF) as u8))
    }

    fn matrix(&self, first: usize) -> Result<Matrix, DecodeError> {
        let mut m = [0.0; 6];
        for (i, slot) in m.iter_mut().enumerate() {
            *slot = self.f64(first + i)?;
        }
        Ok(m)
    }

    fn status(&self, index: usize) -> Result<Status, DecodeError> {
        let v = self.i32(index)?;
        u16::try_from(v).map(Status).map_err(|_| self.err(index))
    }
}

/// Integer parsing with the leniency of JavaScript `parseInt` for the cases
/// that matter in practice: surrounding whitespace and a fractional part
/// (`"12.0"`) are tolerated.
fn parse_int(value: &str) -> Option<i64> {
    let value = value.trim();
    if let Ok(v) = value.parse::<i64>() {
        return Some(v);
    }
    let int_part = value.split('.').next()?;
    if int_part.is_empty() || int_part == "-" || int_part == "+" {
        return None;
    }
    int_part.parse::<i64>().ok()
}

impl<'a> ServerInstruction<'a> {
    /// Decodes an instruction received from the server.
    pub fn decode<I: InstructionArgs<'a>>(ins: &I) -> Result<Self, DecodeError> {
        use ServerInstruction as S;
        let d = Decoder { ins };
        Ok(match ins.opcode() {
            "arc" => S::Arc {
                layer: d.i32(0)?,
                x: d.i32(1)?,
                y: d.i32(2)?,
                radius: d.i32(3)?,
                start: d.f64(4)?,
                end: d.f64(5)?,
                negative: d.i32(6)? != 0,
            },
            "cfill" => S::Cfill {
                mode: d.mode(0)?,
                layer: d.i32(1)?,
                r: d.u8(2)?,
                g: d.u8(3)?,
                b: d.u8(4)?,
                a: d.u8(5)?,
            },
            "clip" => S::Clip { layer: d.i32(0)? },
            "close" => S::Close { layer: d.i32(0)? },
            "copy" => S::Copy {
                src: d.i32(0)?,
                sx: d.i32(1)?,
                sy: d.i32(2)?,
                w: d.i32(3)?,
                h: d.i32(4)?,
                mode: d.mode(5)?,
                dst: d.i32(6)?,
                dx: d.i32(7)?,
                dy: d.i32(8)?,
            },
            "cstroke" => S::Cstroke {
                mode: d.mode(0)?,
                layer: d.i32(1)?,
                cap: LineCap::from_code(d.i64(2)?),
                join: LineJoin::from_code(d.i64(3)?),
                thickness: d.i32(4)?,
                r: d.u8(5)?,
                g: d.u8(6)?,
                b: d.u8(7)?,
                a: d.u8(8)?,
            },
            "cursor" => S::Cursor {
                hotspot_x: d.i32(0)?,
                hotspot_y: d.i32(1)?,
                src: d.i32(2)?,
                sx: d.i32(3)?,
                sy: d.i32(4)?,
                w: d.i32(5)?,
                h: d.i32(6)?,
            },
            "curve" => S::Curve {
                layer: d.i32(0)?,
                cp1x: d.i32(1)?,
                cp1y: d.i32(2)?,
                cp2x: d.i32(3)?,
                cp2y: d.i32(4)?,
                x: d.i32(5)?,
                y: d.i32(6)?,
            },
            "dispose" => S::Dispose { layer: d.i32(0)? },
            "distort" => S::Distort {
                layer: d.i32(0)?,
                matrix: d.matrix(1)?,
            },
            "identity" => S::Identity { layer: d.i32(0)? },
            "lfill" => S::Lfill {
                mode: d.mode(0)?,
                layer: d.i32(1)?,
                src: d.i32(2)?,
            },
            "line" => S::Line {
                layer: d.i32(0)?,
                x: d.i32(1)?,
                y: d.i32(2)?,
            },
            "lstroke" => S::Lstroke {
                mode: d.mode(0)?,
                layer: d.i32(1)?,
                cap: LineCap::from_code(d.i64(2)?),
                join: LineJoin::from_code(d.i64(3)?),
                thickness: d.i32(4)?,
                src: d.i32(5)?,
            },
            "move" => S::Move {
                layer: d.i32(0)?,
                parent: d.i32(1)?,
                x: d.i32(2)?,
                y: d.i32(3)?,
                z: d.i32(4)?,
            },
            "pop" => S::Pop { layer: d.i32(0)? },
            "push" => S::Push { layer: d.i32(0)? },
            "rect" => S::Rect {
                layer: d.i32(0)?,
                x: d.i32(1)?,
                y: d.i32(2)?,
                w: d.i32(3)?,
                h: d.i32(4)?,
            },
            "reset" => S::Reset { layer: d.i32(0)? },
            "set" => S::Set {
                layer: d.i32(0)?,
                name: d.s(1),
                value: d.s(2),
            },
            "shade" => S::Shade {
                layer: d.i32(0)?,
                opacity: d.u8(1)?,
            },
            "size" => S::Size {
                layer: d.i32(0)?,
                w: d.i32(1)?,
                h: d.i32(2)?,
            },
            "start" => S::Start {
                layer: d.i32(0)?,
                x: d.i32(1)?,
                y: d.i32(2)?,
            },
            "transfer" => S::Transfer {
                src: d.i32(0)?,
                sx: d.i32(1)?,
                sy: d.i32(2)?,
                w: d.i32(3)?,
                h: d.i32(4)?,
                function: TransferFunction((d.i32(5)? & 0xF) as u8),
                dst: d.i32(6)?,
                dx: d.i32(7)?,
                dy: d.i32(8)?,
            },
            "transform" => S::Transform {
                layer: d.i32(0)?,
                matrix: d.matrix(1)?,
            },
            "img" => S::Img {
                stream: d.i32(0)?,
                mode: d.mode(1)?,
                layer: d.i32(2)?,
                mimetype: d.s(3),
                x: d.i32(4)?,
                y: d.i32(5)?,
            },
            "png" => S::Png {
                mode: d.mode(0)?,
                layer: d.i32(1)?,
                x: d.i32(2)?,
                y: d.i32(3)?,
                data: d.s(4),
            },
            "jpeg" => S::Jpeg {
                mode: d.mode(0)?,
                layer: d.i32(1)?,
                x: d.i32(2)?,
                y: d.i32(3)?,
                data: d.s(4),
            },
            "ack" => S::Ack {
                stream: d.i32(0)?,
                message: d.s(1),
                status: d.status(2)?,
            },
            "argv" => S::Argv {
                stream: d.i32(0)?,
                mimetype: d.s(1),
                name: d.s(2),
            },
            "audio" => S::Audio {
                stream: d.i32(0)?,
                mimetype: d.s(1),
            },
            "blob" => S::Blob {
                stream: d.i32(0)?,
                data: d.s(1),
            },
            "body" => S::Body {
                object: d.i32(0)?,
                stream: d.i32(1)?,
                mimetype: d.s(2),
                name: d.s(3),
            },
            "clipboard" => S::Clipboard {
                stream: d.i32(0)?,
                mimetype: d.s(1),
            },
            "end" => S::End { stream: d.i32(0)? },
            "file" => S::File {
                stream: d.i32(0)?,
                mimetype: d.s(1),
                filename: d.s(2),
            },
            "filesystem" => S::Filesystem {
                object: d.i32(0)?,
                name: d.s(1),
            },
            "nest" => S::Nest {
                index: d.i32(0)?,
                data: d.s(1),
            },
            "pipe" => S::Pipe {
                stream: d.i32(0)?,
                mimetype: d.s(1),
                name: d.s(2),
            },
            "undefine" => S::Undefine { object: d.i32(0)? },
            "video" => S::Video {
                stream: d.i32(0)?,
                layer: d.i32(1)?,
                mimetype: d.s(2),
            },
            "disconnect" => S::Disconnect,
            "display-control" => S::DisplayControl {
                available: d.s(0) == "1",
            },
            "error" => S::Error {
                message: d.s(0),
                // A missing/invalid status is reported as a generic server error.
                status: d.status(1).unwrap_or(Status::SERVER_ERROR),
            },
            "log" => S::Log { message: d.s(0) },
            "mouse" => S::Mouse {
                x: d.i32(0)?,
                y: d.i32(1)?,
            },
            "msg" => S::Msg { id: d.i32(0)? },
            "name" => S::Name { name: d.s(0) },
            "nop" => S::Nop,
            "ready" => S::Ready { id: d.s(0) },
            "required" => S::Required,
            "sync" => S::Sync {
                timestamp: d.i64(0)?,
                frames: if ins.arg_count() > 1 {
                    d.i64(1)?.clamp(0, u32::MAX as i64) as u32
                } else {
                    0
                },
                apply_time: crate::apply_time::is_request(d.s(2)),
            },
            other => S::Unknown { opcode: other },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Parser;

    #[test]
    fn decodes_from_raw_instruction() {
        let mut parser = Parser::new();
        parser.feed(b"3.img,1.3,2.14,1.0,10.image/webp,2.16,3.128;");
        let raw = parser.next_raw().unwrap().unwrap();
        assert_eq!(
            ServerInstruction::decode(&raw).unwrap(),
            ServerInstruction::Img {
                stream: 3,
                mode: CompositeMode::OVER,
                layer: 0,
                mimetype: "image/webp",
                x: 16,
                y: 128,
            }
        );
    }

    #[test]
    fn decodes_sync_with_and_without_frames() {
        let a = Instruction::new("sync", ["1700000000000"]);
        let b = Instruction::new("sync", ["42", "3"]);
        assert_eq!(
            ServerInstruction::decode(&&a).unwrap(),
            ServerInstruction::Sync {
                timestamp: 1_700_000_000_000,
                frames: 0,
                apply_time: false,
            }
        );
        assert_eq!(
            ServerInstruction::decode(&&b).unwrap(),
            ServerInstruction::Sync {
                timestamp: 42,
                frames: 3,
                apply_time: false,
            }
        );
    }

    #[test]
    fn reports_malformed_arguments() {
        let bad = Instruction::new("rect", ["0", "x", "0", "1", "1"]);
        let err = ServerInstruction::decode(&&bad).unwrap_err();
        assert_eq!(err.index, 1);
        assert_eq!(err.value, "x");
    }

    #[test]
    fn lenient_integers() {
        assert_eq!(parse_int(" 12 "), Some(12));
        assert_eq!(parse_int("12.75"), Some(12));
        assert_eq!(parse_int("-3"), Some(-3));
        assert_eq!(parse_int(""), None);
        assert_eq!(parse_int("abc"), None);
    }

    #[test]
    fn display_control() {
        for (arg, available) in [("1", true), ("0", false), ("", false)] {
            let ins = Instruction::new("display-control", [arg]);
            assert_eq!(
                ServerInstruction::decode(&&ins).unwrap(),
                ServerInstruction::DisplayControl { available }
            );
        }
    }

    #[test]
    fn unknown_opcodes_are_preserved() {
        let ins = Instruction::new("usbconnect", ["1"]);
        assert_eq!(
            ServerInstruction::decode(&&ins).unwrap(),
            ServerInstruction::Unknown {
                opcode: "usbconnect"
            }
        );
    }
}

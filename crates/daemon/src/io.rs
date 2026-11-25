//! Buffered reading and writing of Guacamole instructions over TCP.

use base64::Engine as _;
use molcajete_protocol::binary::RecordWriter;
use molcajete_protocol::{Instruction, InstructionBuilder, Limits, Parser};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

pub struct GuacReader {
    socket: OwnedReadHalf,
    parser: Parser,
    buf: Box<[u8]>,
}

impl GuacReader {
    pub fn new(socket: OwnedReadHalf) -> Self {
        Self {
            socket,
            // Data from clients is untrusted: enforce guacd's limits.
            parser: Parser::with_limits(Limits::STRICT),
            buf: vec![0u8; 16 * 1024].into_boxed_slice(),
        }
    }

    /// Next instruction, or `None` at end of stream.
    pub async fn next(&mut self) -> std::io::Result<Option<Instruction>> {
        loop {
            match self.parser.next_instruction() {
                Ok(Some(ins)) => return Ok(Some(ins)),
                Ok(None) => {}
                Err(e) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e));
                }
            }
            let n = self.socket.read(&mut self.buf).await?;
            if n == 0 {
                return Ok(None);
            }
            self.parser.feed(&self.buf[..n]);
        }
    }
}

/// Largest raw chunk carried by one `blob` instruction (same as libguac).
pub const BLOB_MAX_BYTES: usize = 6048;

/// Instructions and stream data on their way to the client.
///
/// Instructions are appended as text (`*out += "..."`). Stream data goes
/// through [`Output::blob`], which writes it the way the client negotiated:
/// as base64 `blob` instructions, like guacd, or as raw bytes in the record
/// framing of `molcajete_protocol::binary`.
#[derive(Debug, Default)]
pub struct Output {
    /// The Guacamole protocol text (text mode only).
    text: String,
    /// The records (binary mode only).
    records: Option<RecordWriter>,
}

impl Output {
    /// Output in the stock Guacamole text protocol.
    pub fn text() -> Self {
        Self::default()
    }

    /// Output as records, for a peer that asked for them in the handshake.
    pub fn binary() -> Self {
        Self {
            text: String::new(),
            records: Some(RecordWriter::new()),
        }
    }

    pub fn is_binary(&self) -> bool {
        self.records.is_some()
    }

    /// Appends whole instructions.
    pub fn push_str(&mut self, encoded: &str) {
        match &mut self.records {
            Some(records) => records.instructions(encoded.as_bytes()),
            None => self.text.push_str(encoded),
        }
    }

    /// Appends data of the open stream `stream`.
    pub fn blob(&mut self, stream: i32, data: &[u8]) {
        self.blob_aligned(stream, data, 1);
    }

    /// Like [`Output::blob`], for data that may only be split at multiples
    /// of `unit` bytes (whole PCM frames).
    pub fn blob_aligned(&mut self, stream: i32, data: &[u8], unit: usize) {
        match &mut self.records {
            Some(records) => records.blob_aligned(stream, data, unit),
            None => {
                let unit = unit.clamp(1, BLOB_MAX_BYTES);
                let mut b64 = String::new();
                for chunk in data.chunks(BLOB_MAX_BYTES - BLOB_MAX_BYTES % unit) {
                    b64.clear();
                    base64::engine::general_purpose::STANDARD.encode_string(chunk, &mut b64);
                    self.text += &InstructionBuilder::new("blob")
                        .arg(stream)
                        .str_arg(&b64)
                        .finish();
                }
            }
        }
    }

    /// Bytes waiting to be written.
    pub fn len(&self) -> usize {
        match &self.records {
            Some(records) => records.len(),
            None => self.text.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes to put on the wire.
    pub fn as_bytes(&self) -> &[u8] {
        match &self.records {
            Some(records) => records.as_bytes(),
            None => self.text.as_bytes(),
        }
    }

    pub fn clear(&mut self) {
        self.text.clear();
        if let Some(records) = &mut self.records {
            records.clear();
        }
    }
}

impl std::ops::AddAssign<&str> for Output {
    fn add_assign(&mut self, encoded: &str) {
        self.push_str(encoded);
    }
}

/// The protocol text of an output in text mode (empty in binary mode), for
/// tests that inspect what was written.
#[cfg(test)]
impl std::ops::Deref for Output {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

#[cfg(test)]
impl std::fmt::Display for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Accumulates encoded instructions and writes them in one go.
pub struct GuacWriter {
    socket: OwnedWriteHalf,
    buf: Output,
    bytes: metrics::Counter,
    /// Time spent in `write_all`: grows when the gateway / browser cannot
    /// keep up (TCP backpressure).
    write_time: metrics::Histogram,
}

impl GuacWriter {
    pub fn new(socket: OwnedWriteHalf) -> Self {
        Self {
            socket,
            buf: Output::text(),
            bytes: metrics::Counter::noop(),
            write_time: metrics::Histogram::noop(),
        }
    }

    /// Starts recording output metrics labelled with `protocol`.
    pub fn record_metrics(&mut self, protocol: &'static str) {
        self.bytes = metrics::counter!("molcajeted_sent_bytes_total", "protocol" => protocol);
        self.write_time =
            metrics::histogram!("molcajeted_socket_write_seconds", "protocol" => protocol);
    }

    /// Switches to the record framing of `molcajete_protocol::binary`.
    /// Everything pushed before must have been flushed.
    pub fn set_binary(&mut self) {
        debug_assert!(self.buf.is_empty());
        self.buf = Output::binary();
    }

    pub fn push(&mut self, encoded: &str) {
        self.buf.push_str(encoded);
    }

    /// Direct access to the pending output (for code that appends many
    /// instructions).
    pub fn buffer(&mut self) -> &mut Output {
        &mut self.buf
    }

    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    pub async fn flush(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            let started = std::time::Instant::now();
            self.socket.write_all(self.buf.as_bytes()).await?;
            self.write_time.record(started.elapsed());
            self.bytes.increment(self.buf.len() as u64);
            self.buf.clear();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use molcajete_protocol::binary::{Record, records};
    use molcajete_protocol::parse_all;

    use super::*;

    fn data() -> Vec<u8> {
        (0..20_000u32).map(|i| (i * 7 + (i >> 8)) as u8).collect()
    }

    #[test]
    fn text_output_sends_blobs_as_base64_instructions() {
        let mut out = Output::text();
        out += "3.img,1.1,2.14,1.0,9.image/png,1.0,1.0;";
        out.blob(1, &data());
        out.push_str("3.end,1.1;");
        assert_eq!(out.len(), out.as_bytes().len());
        let instructions = parse_all(std::str::from_utf8(out.as_bytes()).unwrap()).unwrap();
        assert_eq!(
            instructions.len(),
            2 + data().len().div_ceil(BLOB_MAX_BYTES)
        );
        let mut joined = Vec::new();
        for ins in &instructions[1..instructions.len() - 1] {
            assert_eq!((ins.opcode.as_str(), ins.arg(0)), ("blob", "1"));
            // Within the instruction size limit of guacd and the stock client.
            assert!(ins.encode().len() <= 8192);
            let chunk = base64::engine::general_purpose::STANDARD
                .decode(ins.arg(1))
                .unwrap();
            assert!(chunk.len() <= BLOB_MAX_BYTES);
            joined.extend(chunk);
        }
        assert_eq!(joined, data());
        out.clear();
        assert!(out.is_empty());
    }

    #[test]
    fn binary_output_sends_blobs_as_raw_records() {
        let mut out = Output::binary();
        out += "3.img,1.1,2.14,1.0,9.image/png,1.0,1.0;";
        out.blob(1, &data());
        out.push_str("3.end,1.1;");
        out += "4.sync,1.5;";
        let all: Vec<_> = records(out.as_bytes()).collect::<Result<_, _>>().unwrap();
        assert_eq!(
            all,
            [
                Record::Instructions(b"3.img,1.1,2.14,1.0,9.image/png,1.0,1.0;"),
                Record::Blob {
                    stream: 1,
                    data: &data()
                },
                Record::Instructions(b"3.end,1.1;4.sync,1.5;"),
            ]
        );
        out.clear();
        assert!(out.is_empty());
        out.push_str("3.nop;");
        assert_eq!(out.as_bytes(), b"\0\x06\0\0\x003.nop;");
    }

    #[test]
    fn aligned_blobs_keep_whole_units_in_both_modes() {
        // 16-bit stereo: 4 bytes per frame, more than one blob's worth.
        let pcm = vec![1u8; 4 * 20_000];
        let mut text = Output::text();
        text.blob_aligned(5, &pcm, 4);
        for ins in parse_all(&text).unwrap() {
            let chunk = base64::engine::general_purpose::STANDARD
                .decode(ins.arg(1))
                .unwrap();
            assert!(chunk.len().is_multiple_of(4) && chunk.len() <= BLOB_MAX_BYTES);
        }
        let mut binary = Output::binary();
        binary.blob_aligned(5, &pcm, 4);
        let mut total = 0;
        for record in records(binary.as_bytes()) {
            let Record::Blob { stream: 5, data } = record.unwrap() else {
                panic!("expected blobs of stream 5");
            };
            assert!(data.len().is_multiple_of(4));
            total += data.len();
        }
        assert_eq!(total, pcm.len());
    }
}

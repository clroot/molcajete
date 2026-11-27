//! Buffered reading and writing of Guacamole instructions over TCP.

use base64::Engine as _;
use molcajete_protocol::binary::RecordWriter;
use molcajete_protocol::{Instruction, InstructionBuilder, Limits, Parser};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// Reads instructions from a client. Generic over the input only so that the
/// fuzz targets can feed it from memory.
pub struct GuacReader<R = OwnedReadHalf> {
    socket: R,
    parser: Parser,
    buf: Box<[u8]>,
}

impl<R: AsyncRead + Unpin> GuacReader<R> {
    pub fn new(socket: R) -> Self {
        Self {
            socket,
            // Data from clients is untrusted: enforce guacd's limits.
            parser: Parser::with_limits(Limits::STRICT),
            buf: vec![0u8; 16 * 1024].into_boxed_slice(),
        }
    }

    /// The socket and the bytes read from it that no instruction has
    /// taken yet, for a connection that stops speaking Guacamole.
    pub fn into_parts(self) -> (R, Vec<u8>) {
        (self.socket, self.parser.pending().to_vec())
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

/// A write to the client's socket failed: the client (the gateway, the
/// browser behind it) went away, which ends a session the ordinary way
/// rather than as a failure. Wrapped in the [`std::io::Error`] the writer
/// returns, with the same kind; see [`is_client_gone`].
#[derive(Debug)]
pub struct ClientGone(std::io::Error);

impl std::fmt::Display for ClientGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the client went away: {}", self.0)
    }
}

impl std::error::Error for ClientGone {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Whether `err` comes from a write to a client that went away.
pub fn is_client_gone(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.is::<ClientGone>()
            || cause
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::get_ref)
                .is_some_and(|inner| inner.is::<ClientGone>())
    })
}

/// Accumulates encoded instructions and writes them in one go.
///
/// The writer of a session that may outlive its client
/// ([`crate::resume`]) can lose its socket and get another: while it has
/// none, or once a write failed, everything is dropped instead of failing
/// the session ([`GuacWriter::set_detachable`]).
pub struct GuacWriter<W = OwnedWriteHalf> {
    /// `None` while no client is attached.
    socket: Option<W>,
    buf: Output,
    /// A failed write detaches instead of returning the error.
    detachable: bool,
    bytes: metrics::Counter,
    /// Time spent in `write_all`: grows when the gateway / browser cannot
    /// keep up (TCP backpressure).
    write_time: metrics::Histogram,
}

impl<W: AsyncWrite + Unpin> GuacWriter<W> {
    pub fn new(socket: W) -> Self {
        Self {
            socket: Some(socket),
            buf: Output::text(),
            detachable: false,
            bytes: metrics::Counter::noop(),
            write_time: metrics::Histogram::noop(),
        }
    }

    /// The socket, for a connection that stops speaking Guacamole.
    /// Everything pushed must have been flushed.
    pub fn into_inner(self) -> W {
        debug_assert!(self.buf.is_empty());
        self.socket.expect("a writer that was never detached")
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

    /// From now on a failed write drops the socket (the session goes on
    /// without its client) instead of failing.
    pub fn set_detachable(&mut self) {
        self.detachable = true;
    }

    /// Whether a client is attached (the socket has not failed or been
    /// taken away).
    pub fn is_attached(&self) -> bool {
        self.socket.is_some()
    }

    /// Drops the socket and whatever was waiting for it; output is
    /// discarded until [`GuacWriter::attach`].
    pub fn detach(&mut self) -> Option<W> {
        self.buf.clear();
        self.socket.take()
    }

    /// Writes to `socket` from now on, in the framing that client chose.
    /// Output pending for the previous client is dropped.
    pub fn attach(&mut self, socket: W, binary: bool) {
        self.socket = Some(socket);
        self.buf = if binary {
            Output::binary()
        } else {
            Output::text()
        };
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
        if self.buf.is_empty() {
            return Ok(());
        }
        let Some(socket) = &mut self.socket else {
            // Nobody to send it to.
            self.buf.clear();
            return Ok(());
        };
        let started = std::time::Instant::now();
        match socket.write_all(self.buf.as_bytes()).await {
            Ok(()) => {
                self.write_time.record(started.elapsed());
                self.bytes.increment(self.buf.len() as u64);
                self.buf.clear();
                Ok(())
            }
            Err(err) if self.detachable => {
                tracing::debug!(error = %err, "writing to the client failed; detaching it");
                self.detach();
                Ok(())
            }
            Err(err) => Err(std::io::Error::new(err.kind(), ClientGone(err))),
        }
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

    #[tokio::test]
    async fn a_detachable_writer_survives_its_socket() {
        let (client, server) = tokio::io::duplex(1024);
        let mut writer = GuacWriter::new(server);
        writer.set_detachable();
        drop(client);
        writer.push("3.nop;");
        writer.flush().await.unwrap();
        assert!(!writer.is_attached());
        // Dropped while detached.
        writer.push("3.nop;");
        writer.flush().await.unwrap();
        assert_eq!(writer.pending(), 0);

        let (mut client, server) = tokio::io::duplex(1024);
        writer.attach(server, false);
        writer.push("4.sync,1.1;");
        writer.flush().await.unwrap();
        let mut buf = [0u8; 16];
        let n = tokio::io::AsyncReadExt::read(&mut client, &mut buf)
            .await
            .unwrap();
        assert_eq!(&buf[..n], b"4.sync,1.1;");
    }

    #[tokio::test]
    async fn a_plain_writer_reports_a_failed_write() {
        let (client, server) = tokio::io::duplex(1024);
        let mut writer = GuacWriter::new(server);
        drop(client);
        writer.push("3.nop;");
        assert!(writer.flush().await.is_err());
    }
}

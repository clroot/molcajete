//! Buffered reading and writing of Guacamole instructions over TCP.

use molcajete_protocol::{Instruction, Limits, Parser};
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

/// Accumulates encoded instructions and writes them in one go.
pub struct GuacWriter {
    socket: OwnedWriteHalf,
    buf: String,
}

impl GuacWriter {
    pub fn new(socket: OwnedWriteHalf) -> Self {
        Self {
            socket,
            buf: String::with_capacity(64 * 1024),
        }
    }

    pub fn push(&mut self, encoded: &str) {
        self.buf.push_str(encoded);
    }

    /// Direct access to the pending output (for code that appends many
    /// instructions).
    pub fn buffer(&mut self) -> &mut String {
        &mut self.buf
    }

    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    pub async fn flush(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            self.socket.write_all(self.buf.as_bytes()).await?;
            self.buf.clear();
        }
        Ok(())
    }
}

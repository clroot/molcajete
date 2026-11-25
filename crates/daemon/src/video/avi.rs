//! Streaming reader for the AVI that ffmpeg writes to a pipe.
//!
//! Raw elementary streams (`-f h264`, `-f hevc`, `-f obu`) do not say where
//! an access unit ends until the next one begins, which would delay every
//! frame by one. AVI frames each packet in a chunk with an explicit size and
//! stores it verbatim (Annex B / OBUs with in-band headers, as AVI has no
//! global header), so a packet is complete as soon as its chunk is.
//!
//! On a non-seekable output ffmpeg never goes back to patch sizes, so list
//! sizes are meaningless (`0xffffffff`); the reader therefore descends into
//! every `RIFF`/`LIST` and only trusts the sizes of leaf chunks.

use std::io::{self, Read};

/// Upper bound for one packet (guards against a corrupt stream).
const MAX_PACKET: usize = 64 * 1024 * 1024;

pub(crate) struct AviReader<R> {
    inner: R,
}

impl<R: Read> AviReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Next non-empty video packet, `None` at the end of the stream.
    pub(crate) fn next_packet(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            let mut header = [0u8; 8];
            if !self.read_header(&mut header)? {
                return Ok(None);
            }
            let id = [header[0], header[1], header[2], header[3]];
            let size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
            if &id == b"RIFF" || &id == b"LIST" {
                // Skip the form/list type and continue with its children.
                let mut form = [0u8; 4];
                self.inner.read_exact(&mut form)?;
                continue;
            }
            let padded = (size + (size & 1)) as u64;
            if is_video_chunk(&id) && size > 0 {
                if size > MAX_PACKET {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("AVI chunk of {size} bytes"),
                    ));
                }
                let mut packet = vec![0u8; size];
                self.inner.read_exact(&mut packet)?;
                self.skip(padded - size as u64)?;
                return Ok(Some(packet));
            }
            self.skip(padded)?;
        }
    }

    /// Reads a chunk header; `false` on a clean end of stream.
    fn read_header(&mut self, header: &mut [u8; 8]) -> io::Result<bool> {
        let mut filled = 0;
        while filled < header.len() {
            match self.inner.read(&mut header[filled..]) {
                Ok(0) if filled == 0 => return Ok(false),
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    fn skip(&mut self, len: u64) -> io::Result<()> {
        let skipped = io::copy(&mut (&mut self.inner).take(len), &mut io::sink())?;
        if skipped < len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(())
    }
}

/// `##dc` (compressed video) or `##db` (uncompressed video) chunk ids.
fn is_video_chunk(id: &[u8; 4]) -> bool {
    id[0].is_ascii_digit() && id[1].is_ascii_digit() && (&id[2..] == b"dc" || &id[2..] == b"db")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    fn list(id: &[u8; 4], size: u32, kind: &[u8; 4], children: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(children);
        out
    }

    /// Shaped like ffmpeg's output to a pipe: unpatched list sizes.
    fn sample() -> Vec<u8> {
        let strl = list(
            b"LIST",
            0,
            b"strl",
            &[chunk(b"strh", &[0; 56]), chunk(b"strf", &[1; 40])].concat(),
        );
        let hdrl = list(
            b"LIST",
            0,
            b"hdrl",
            &[chunk(b"avih", &[0; 56]), strl].concat(),
        );
        let movi = list(
            b"LIST",
            u32::MAX,
            b"movi",
            &[
                chunk(b"00dc", b"key"),
                chunk(b"00dc", b""),
                chunk(b"ix00", &[9; 6]),
                chunk(b"00db", b"next"),
            ]
            .concat(),
        );
        let body = [hdrl, chunk(b"JUNK", &[0; 13]), movi].concat();
        list(b"RIFF", 0, b"AVI ", &body)
    }

    #[test]
    fn yields_each_video_chunk() {
        let data = sample();
        let mut reader = AviReader::new(&data[..]);
        assert_eq!(reader.next_packet().unwrap().as_deref(), Some(&b"key"[..]));
        assert_eq!(reader.next_packet().unwrap().as_deref(), Some(&b"next"[..]));
        assert_eq!(reader.next_packet().unwrap(), None);
    }

    #[test]
    fn works_on_a_trickling_stream() {
        /// Hands out one byte per read.
        struct Trickle<'a>(&'a [u8]);
        impl Read for Trickle<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let Some((&first, rest)) = self.0.split_first() else {
                    return Ok(0);
                };
                if buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = first;
                self.0 = rest;
                Ok(1)
            }
        }
        let data = sample();
        let mut reader = AviReader::new(Trickle(&data));
        assert_eq!(reader.next_packet().unwrap().as_deref(), Some(&b"key"[..]));
        assert_eq!(reader.next_packet().unwrap().as_deref(), Some(&b"next"[..]));
        assert_eq!(reader.next_packet().unwrap(), None);
    }

    #[test]
    fn truncated_chunks_are_errors() {
        let data = sample();
        let cut = data.len() - 3;
        let mut reader = AviReader::new(&data[..cut]);
        assert_eq!(reader.next_packet().unwrap().as_deref(), Some(&b"key"[..]));
        assert!(reader.next_packet().is_err());
        let huge = [b"00dc".as_slice(), &u32::MAX.to_le_bytes()].concat();
        assert!(AviReader::new(&huge[..]).next_packet().is_err());
    }
}

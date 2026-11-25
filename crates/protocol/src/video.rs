//! Framing of compressed video carried in Guacamole `video` streams.
//!
//! The Guacamole protocol only says that a `video` stream delivers data of a
//! given mimetype into a layer. molcajete uses it for live, low-latency
//! video of busy screen regions; a stream is the concatenation of
//! (possibly blob-split) frames:
//!
//! ```text
//! u32 BE  payload length
//! u32 BE  timestamp in milliseconds (wrapping)
//! u8      flags: bit 0 = key frame
//! [payload]  one access unit / temporal unit:
//!            H.264 and H.265 as Annex B, AV1 as low-overhead OBUs
//! ```
//!
//! Every stream starts with a key frame that carries the codec configuration
//! in-band (SPS/PPS, VPS/SPS/PPS or the AV1 sequence header), so decoders
//! need no out-of-band description.

/// Mimetypes used in the `video` handshake instruction and `video` streams.
pub const MIMETYPE_H264: &str = "video/h264";
pub const MIMETYPE_H265: &str = "video/h265";
pub const MIMETYPE_AV1: &str = "video/av1";

/// Video codecs, in order of preference (best compression first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VideoCodec {
    Av1,
    H265,
    H264,
}

impl VideoCodec {
    pub const ALL: [VideoCodec; 3] = [VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264];

    pub fn mimetype(self) -> &'static str {
        match self {
            Self::Av1 => MIMETYPE_AV1,
            Self::H265 => MIMETYPE_H265,
            Self::H264 => MIMETYPE_H264,
        }
    }

    pub fn from_mimetype(mimetype: &str) -> Option<Self> {
        match mimetype.split(';').next()?.trim() {
            MIMETYPE_AV1 => Some(Self::Av1),
            MIMETYPE_H265 => Some(Self::H265),
            MIMETYPE_H264 => Some(Self::H264),
            _ => None,
        }
    }
}

/// Size of the per-frame header.
pub const FRAME_HEADER_LEN: usize = 9;

/// Appends one framed access unit to `out`.
pub fn encode_frame(out: &mut Vec<u8>, timestamp_ms: u32, key: bool, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&timestamp_ms.to_be_bytes());
    out.push(key as u8);
    out.extend_from_slice(payload);
}

/// One decoded frame record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub timestamp_ms: u32,
    pub key: bool,
    pub payload: Vec<u8>,
}

/// Reassembles frames from stream data arriving in arbitrary pieces.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    /// Upper bound for a single frame (guards against corrupt streams).
    pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Next complete frame, `Ok(None)` if more data is needed, `Err` if the
    /// stream is corrupt.
    pub fn next_frame(&mut self) -> Result<Option<VideoFrame>, &'static str> {
        if self.buf.len() < FRAME_HEADER_LEN {
            return Ok(None);
        }
        let len = u32::from_be_bytes(self.buf[0..4].try_into().expect("4 bytes")) as usize;
        if len > Self::MAX_FRAME_LEN {
            return Err("video frame too large");
        }
        if self.buf.len() < FRAME_HEADER_LEN + len {
            return Ok(None);
        }
        let timestamp_ms = u32::from_be_bytes(self.buf[4..8].try_into().expect("4 bytes"));
        let key = self.buf[8] & 1 != 0;
        let payload = self.buf[FRAME_HEADER_LEN..FRAME_HEADER_LEN + len].to_vec();
        self.buf.drain(..FRAME_HEADER_LEN + len);
        Ok(Some(VideoFrame {
            timestamp_ms,
            key,
            payload,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_survive_arbitrary_splitting() {
        let mut stream = Vec::new();
        encode_frame(&mut stream, 7, true, b"\x00\x00\x00\x01key");
        encode_frame(&mut stream, 40, false, b"delta-frame");
        for split in 0..=stream.len() {
            let mut reader = FrameReader::default();
            reader.push(&stream[..split]);
            let mut frames = Vec::new();
            while let Some(f) = reader.next_frame().unwrap() {
                frames.push(f);
            }
            reader.push(&stream[split..]);
            while let Some(f) = reader.next_frame().unwrap() {
                frames.push(f);
            }
            assert_eq!(frames.len(), 2, "split {split}");
            assert_eq!(frames[0].timestamp_ms, 7);
            assert!(frames[0].key);
            assert_eq!(frames[1].payload, b"delta-frame");
            assert!(!frames[1].key);
        }
    }

    #[test]
    fn mimetypes_round_trip() {
        for codec in VideoCodec::ALL {
            assert_eq!(VideoCodec::from_mimetype(codec.mimetype()), Some(codec));
        }
        assert_eq!(
            VideoCodec::from_mimetype("video/av1; foo=bar"),
            Some(VideoCodec::Av1)
        );
        assert_eq!(VideoCodec::from_mimetype("video/webm"), None);
    }

    #[test]
    fn rejects_absurd_lengths() {
        let mut reader = FrameReader::default();
        reader.push(&[0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0]);
        assert!(reader.next_frame().is_err());
    }
}

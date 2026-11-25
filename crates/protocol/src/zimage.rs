//! The zstd image type of `img` streams (`image/x-molcajete-zstd-v1`).
//!
//! A lossless image for screen content that a client decodes synchronously,
//! without an image decoder of the browser: indexed or RGB pixels, as they
//! are, in one Zstandard frame. molcajeted sends it only to clients that
//! list the mimetype in the `image` instruction of the handshake.
//!
//! ```text
//! u8      kind: 0 = indexed, 1 = RGB
//! u16 LE  width  (1 or more)
//! u16 LE  height (1 or more)
//! indexed only:
//!   u8    number of colours - 1
//!   [..]  palette: 3 bytes (R, G, B) per colour
//! [..]    one Zstandard frame (RFC 8878) holding
//!           indexed: the rows of palette indices, each index taking 1 bit
//!                    for up to 2 colours, 2 bits up to 4, 4 bits up to 16
//!                    and 8 bits otherwise, first pixel in the highest
//!                    bits, every row padded to a whole byte (as in PNG)
//!           RGB:     width * height * 3 bytes, rows top to bottom
//! ```
//!
//! Every pixel is opaque. An image has at most [`MAX_PIXELS`] pixels. The
//! frame uses no dictionary, decompresses to exactly the size the header
//! implies, and its window is at most [`MAX_WINDOW`] bytes unless it is a
//! single-segment frame of that exact size; nothing follows it. An index
//! with no palette entry is drawn black.

use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};
use ruzstd::io::Read;

/// Mimetype in the `image` handshake instruction and in `img`.
pub const MIMETYPE: &str = "image/x-molcajete-zstd-v1";

/// Most pixels one image may have (an 8K screen is 33.2 million).
pub const MAX_PIXELS: u32 = 1 << 25;

/// Largest window of the Zstandard frame, which bounds what a decoder
/// allocates before it has seen any data.
pub const MAX_WINDOW: u64 = 8 << 20;

const KIND_INDEXED: u8 = 0;
const KIND_RGB: u8 = 1;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Why an image could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    #[error("the image header is incomplete or invalid")]
    Header,
    #[error("the image is larger than the limit")]
    TooLarge,
    #[error("the Zstandard frame header is invalid or asks for too large a window")]
    Frame,
    #[error("the compressed data is damaged")]
    Data,
    #[error("the data does not have the size the header implies")]
    Size,
}

/// Bits per palette index for a palette of `colors` entries.
pub fn index_bits(colors: usize) -> u8 {
    match colors {
        0..=2 => 1,
        3..=4 => 2,
        5..=16 => 4,
        _ => 8,
    }
}

/// Whether an image of this size can be encoded at all.
pub fn fits(width: u32, height: u32) -> bool {
    (1..=u16::MAX as u32).contains(&width)
        && (1..=u16::MAX as u32).contains(&height)
        && width as u64 * height as u64 <= MAX_PIXELS as u64
}

/// Appends everything in front of the Zstandard frame. `palette` holds the
/// RGB triplets of an indexed image (1 to 256 of them) and is `None` for
/// RGB. The size must satisfy [`fits`].
pub fn write_header(out: &mut Vec<u8>, width: u32, height: u32, palette: Option<&[u8]>) {
    debug_assert!(fits(width, height));
    out.push(if palette.is_some() {
        KIND_INDEXED
    } else {
        KIND_RGB
    });
    out.extend_from_slice(&(width as u16).to_le_bytes());
    out.extend_from_slice(&(height as u16).to_le_bytes());
    if let Some(palette) = palette {
        debug_assert!(!palette.is_empty() && palette.len() <= 768 && palette.len() % 3 == 0);
        out.push((palette.len() / 3 - 1) as u8);
        out.extend_from_slice(palette);
    }
}

/// The parts of an encoded image.
struct Parsed<'a> {
    width: usize,
    height: usize,
    /// RGB triplets; `None` for an RGB image.
    palette: Option<&'a [u8]>,
    frame: &'a [u8],
}

impl Parsed<'_> {
    /// Bytes of one row inside the frame.
    fn row_bytes(&self) -> usize {
        match self.palette {
            Some(palette) => (self.width * index_bits(palette.len() / 3) as usize).div_ceil(8),
            None => self.width * 3,
        }
    }
}

fn parse(data: &[u8]) -> Result<Parsed<'_>, ImageError> {
    let (head, rest) = data.split_at_checked(5).ok_or(ImageError::Header)?;
    let width = u16::from_le_bytes([head[1], head[2]]) as u32;
    let height = u16::from_le_bytes([head[3], head[4]]) as u32;
    if width == 0 || height == 0 {
        return Err(ImageError::Header);
    }
    if !fits(width, height) {
        return Err(ImageError::TooLarge);
    }
    let (palette, frame) = match head[0] {
        KIND_INDEXED => {
            let (&last, rest) = rest.split_first().ok_or(ImageError::Header)?;
            let (palette, frame) = rest
                .split_at_checked((last as usize + 1) * 3)
                .ok_or(ImageError::Header)?;
            (Some(palette), frame)
        }
        KIND_RGB => (None, rest),
        _ => return Err(ImageError::Header),
    };
    Ok(Parsed {
        width: width as usize,
        height: height as usize,
        palette,
        frame,
    })
}

/// Size of an image in pixels, from its first bytes.
pub fn dimensions(data: &[u8]) -> Result<(u32, u32), ImageError> {
    parse(data).map(|p| (p.width as u32, p.height as u32))
}

/// Checks the header of the Zstandard frame against the size its content
/// must have, before a decoder allocates a window for it.
fn check_frame(frame: &[u8], content: usize) -> Result<(), ImageError> {
    let err = ImageError::Frame;
    if frame.len() < 6 || frame[..4] != ZSTD_MAGIC {
        return Err(err);
    }
    let descriptor = frame[4];
    let single_segment = descriptor & 0x20 != 0;
    // No dictionary, and the reserved bit is clear.
    if descriptor & 0x03 != 0 || descriptor & 0x08 != 0 {
        return Err(err);
    }
    let mut pos = 5;
    let mut window = 0u64;
    if !single_segment {
        let byte = frame[pos];
        pos += 1;
        let base = 1u64 << (10 + (byte >> 3));
        window = base + (base >> 3) * (byte & 7) as u64;
    }
    let size_bytes = match (descriptor >> 6, single_segment) {
        (0, false) => 0,
        (0, true) => 1,
        (1, _) => 2,
        (2, _) => 4,
        _ => 8,
    };
    let declared = if size_bytes == 0 {
        None
    } else {
        let field = frame.get(pos..pos + size_bytes).ok_or(err)?;
        let mut bytes = [0u8; 8];
        bytes[..size_bytes].copy_from_slice(field);
        let value = u64::from_le_bytes(bytes);
        Some(if size_bytes == 2 { value + 256 } else { value })
    };
    if declared.is_some_and(|size| size != content as u64) {
        return Err(ImageError::Size);
    }
    // A single-segment frame is its own window: exactly the content.
    if single_segment || window <= MAX_WINDOW {
        Ok(())
    } else {
        Err(err)
    }
}

/// Decoder of zstd images. It keeps its buffers, so one decoder should
/// decode all the images of a session.
pub struct Decoder {
    zstd: FrameDecoder,
    /// The decompressed frame: indices or RGB.
    raw: Vec<u8>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            zstd: FrameDecoder::new(),
            raw: Vec::new(),
        }
    }
}

impl Decoder {
    /// Decodes `data` into `rgba` (replacing its content with
    /// width × height opaque RGBA pixels, top row first) and returns the
    /// size of the image. On an error `rgba` holds nothing meaningful.
    ///
    /// Never panics, whatever `data` holds, and writes exactly as many
    /// pixels as the header announces, which is at most [`MAX_PIXELS`].
    pub fn decode(&mut self, data: &[u8], rgba: &mut Vec<u8>) -> Result<(u32, u32), ImageError> {
        let image = parse(data)?;
        let row_bytes = image.row_bytes();
        self.inflate(image.frame, row_bytes * image.height)?;
        // Every pixel is written below (the frame has exactly the size the
        // rows need), so what the buffer held is not cleared first.
        rgba.resize(image.width * image.height * 4, 255);
        let pixels = rgba.as_chunks_mut::<4>().0;
        match image.palette {
            Some(palette) => expand_indexed(&self.raw, row_bytes, palette, image.width, pixels),
            None => {
                for (pixel, rgb) in pixels.iter_mut().zip(self.raw.as_chunks::<3>().0) {
                    *pixel = [rgb[0], rgb[1], rgb[2], 255];
                }
            }
        }
        Ok((image.width as u32, image.height as u32))
    }

    /// Decompresses `frame` into `self.raw`, which must come to `len` bytes.
    fn inflate(&mut self, frame: &[u8], len: usize) -> Result<(), ImageError> {
        check_frame(frame, len)?;
        // Filled completely below, or the image is refused.
        self.raw.resize(len, 0);
        let mut input = frame;
        self.zstd.init(&mut input).map_err(|_| ImageError::Frame)?;
        let mut written = 0;
        loop {
            self.zstd
                .decode_blocks(&mut input, BlockDecodingStrategy::UptoBytes(1 << 20))
                .map_err(|_| ImageError::Data)?;
            written += self
                .zstd
                .read(&mut self.raw[written..])
                .map_err(|_| ImageError::Data)?;
            // More than the header implies. The decoder holds back up to a
            // window of data until the frame ends, which bounds what a
            // frame that never ends can make it buffer.
            if written == len && self.zstd.read(&mut [0]).map_err(|_| ImageError::Data)? != 0 {
                return Err(ImageError::Size);
            }
            if self.zstd.is_finished() {
                break;
            }
        }
        if written != len || !input.is_empty() {
            return Err(ImageError::Size);
        }
        Ok(())
    }
}

fn expand_indexed(
    raw: &[u8],
    row_bytes: usize,
    palette: &[u8],
    width: usize,
    pixels: &mut [[u8; 4]],
) {
    // Indices beyond the palette are black.
    let mut colors = [[0, 0, 0, 255]; 256];
    for (color, rgb) in colors.iter_mut().zip(palette.as_chunks::<3>().0) {
        *color = [rgb[0], rgb[1], rgb[2], 255];
    }
    let bits = index_bits(palette.len() / 3) as usize;
    if bits == 8 {
        for (pixel, &index) in pixels.iter_mut().zip(raw) {
            *pixel = colors[index as usize];
        }
        return;
    }
    let per_byte = 8 / bits;
    let mask = (1u8 << bits) - 1;
    for (row, packed) in pixels
        .chunks_exact_mut(width)
        .zip(raw.chunks_exact(row_bytes))
    {
        for (group, &byte) in row.chunks_mut(per_byte).zip(packed) {
            for (i, pixel) in group.iter_mut().enumerate() {
                let shift = 8 - bits * (i + 1);
                *pixel = colors[((byte >> shift) & mask) as usize];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruzstd::encoding::{CompressionLevel, compress_to_vec};

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u32) -> u32 {
            (self.next() >> 33) as u32 % n
        }
    }

    /// Encodes with the same layout as molcajeted, using ruzstd's own
    /// compressor (molcajeted uses libzstd, tested there).
    fn encode(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut palette: Vec<u8> = Vec::new();
        let mut indices = Vec::new();
        for p in rgba.as_chunks::<4>().0 {
            let found = palette.as_chunks::<3>().0.iter().position(|c| c == &p[..3]);
            let index = found.unwrap_or_else(|| {
                palette.extend_from_slice(&p[..3]);
                palette.len() / 3 - 1
            });
            indices.push(index);
            if palette.len() > 768 {
                break;
            }
        }
        let mut out = Vec::new();
        let raw: Vec<u8> = if palette.len() <= 768 {
            write_header(&mut out, width, height, Some(&palette));
            let bits = index_bits(palette.len() / 3) as usize;
            let row_bytes = (width as usize * bits).div_ceil(8);
            let mut raw = vec![0u8; row_bytes * height as usize];
            for (y, row) in indices.chunks(width as usize).enumerate() {
                for (x, &i) in row.iter().enumerate() {
                    let shift = 8 - bits * (x % (8 / bits) + 1);
                    raw[y * row_bytes + x * bits / 8] |= (i as u8) << shift;
                }
            }
            raw
        } else {
            write_header(&mut out, width, height, None);
            rgba.as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect()
        };
        out.extend_from_slice(&compress_to_vec(&raw[..], CompressionLevel::Fastest));
        out
    }

    /// An image of `colors` colours (any number: more than 256 gives RGB).
    fn image(width: u32, height: u32, colors: u32, seed: u64) -> Vec<u8> {
        let mut rng = Rng(seed | 1);
        (0..width * height)
            .flat_map(|_| {
                let c = rng.below(colors);
                [c as u8, (c >> 8) as u8, (c * 7) as u8, 255]
            })
            .collect()
    }

    #[test]
    fn images_round_trip_at_every_depth_and_shape() {
        let mut decoder = Decoder::default();
        let mut rgba = Vec::new();
        let shapes = [
            (1, 1),
            (1, 9),
            (9, 1),
            (7, 5),
            (8, 3),
            (13, 5),
            (64, 64),
            (301, 77),
        ];
        for (w, h) in shapes {
            for colors in [1, 2, 3, 4, 5, 16, 17, 256, 257, 70_000] {
                let pixels = image(w, h, colors, (w * 31 + h + colors) as u64);
                let encoded = encode(w, h, &pixels);
                assert_eq!(dimensions(&encoded), Ok((w, h)));
                assert_eq!(decoder.decode(&encoded, &mut rgba), Ok((w, h)));
                assert_eq!(rgba, pixels, "{w}x{h}, {colors} colours");
            }
        }
    }

    #[test]
    fn images_larger_than_a_block_round_trip() {
        // More than 128 KiB of indices and of RGB: several zstd blocks.
        let mut decoder = Decoder::default();
        let mut rgba = Vec::new();
        for colors in [200, 1 << 20] {
            let pixels = image(640, 480, colors, 5);
            let encoded = encode(640, 480, &pixels);
            assert!(encoded.len() > 64 * 1024);
            assert_eq!(decoder.decode(&encoded, &mut rgba), Ok((640, 480)));
            assert!(rgba == pixels, "{colors} colours");
        }
    }

    #[test]
    fn an_index_without_a_colour_is_black() {
        // Three colours take 2 bits per index, so index 3 can be written.
        let mut data = Vec::new();
        write_header(&mut data, 4, 1, Some(&[9, 9, 9, 8, 8, 8, 7, 7, 7]));
        data.extend_from_slice(&compress_to_vec(
            &[0b00_01_10_11u8][..],
            CompressionLevel::Fastest,
        ));
        let mut rgba = Vec::new();
        Decoder::default().decode(&data, &mut rgba).unwrap();
        assert_eq!(
            rgba,
            [9, 9, 9, 255, 8, 8, 8, 255, 7, 7, 7, 255, 0, 0, 0, 255]
        );
    }

    #[test]
    fn headers_are_checked() {
        let mut decoder = Decoder::default();
        let mut rgba = Vec::new();
        let good = encode(20, 20, &image(20, 20, 70_000, 1));
        assert!(decoder.decode(&good, &mut rgba).is_ok());
        let mut decode = |data: &[u8]| decoder.decode(data, &mut rgba);

        assert_eq!(decode(&[]), Err(ImageError::Header));
        assert_eq!(decode(&good[..4]), Err(ImageError::Header));
        let mut bad = good.clone();
        bad[0] = 2;
        assert_eq!(decode(&bad), Err(ImageError::Header), "unknown kind");
        for field in [1, 3] {
            let mut bad = good.clone();
            bad[field] = 0;
            assert_eq!(decode(&bad), Err(ImageError::Header), "zero size");
        }
        // 65535 x 65535 pixels are never allocated.
        let mut bad = good.clone();
        bad[1..5].fill(0xff);
        assert_eq!(decode(&bad), Err(ImageError::TooLarge));
        // The size in the header and the data disagree, either way.
        let mut bad = good.clone();
        bad[1] = 21;
        assert_eq!(decode(&bad), Err(ImageError::Size));
        bad[1] = 19;
        assert_eq!(decode(&bad), Err(ImageError::Size));
        // A palette cut short, and an indexed image with nothing after it.
        let indexed = encode(20, 20, &image(20, 20, 200, 1));
        assert_eq!(decode(&indexed[..5]), Err(ImageError::Header));
        assert_eq!(decode(&indexed[..40]), Err(ImageError::Header));
        // Something after the frame.
        let mut bad = good.clone();
        bad.push(0);
        assert!(decode(&bad).is_err());
        let mut twice = good.clone();
        twice.extend_from_slice(&good[5..]);
        assert!(decode(&twice).is_err());
    }

    #[test]
    fn frames_with_a_large_window_or_a_dictionary_are_refused() {
        let frame = |descriptor: u8, rest: &[u8]| {
            let mut data = Vec::new();
            write_header(&mut data, 2, 2, None);
            data.extend_from_slice(&ZSTD_MAGIC);
            data.push(descriptor);
            data.extend_from_slice(rest);
            data
        };
        let mut decoder = Decoder::default();
        let mut rgba = Vec::new();
        let mut decode = |data: &[u8]| decoder.decode(data, &mut rgba);
        // A window of 2^23 bytes is allowed (the data is not).
        assert_eq!(
            decode(&frame(0, &[13 << 3, 0, 0, 0])),
            Err(ImageError::Data)
        );
        // One step more is 9 MiB, and the largest is 3.75 TiB.
        assert_eq!(decode(&frame(0, &[13 << 3 | 1, 0])), Err(ImageError::Frame));
        assert_eq!(decode(&frame(0, &[0xff, 0])), Err(ImageError::Frame));
        // A dictionary, the reserved bit, no magic number.
        assert_eq!(decode(&frame(1, &[0, 0, 0])), Err(ImageError::Frame));
        assert_eq!(decode(&frame(8, &[0, 0, 0])), Err(ImageError::Frame));
        assert_eq!(
            decode(&[1, 2, 0, 2, 0, 1, 2, 3, 4, 5, 6]),
            Err(ImageError::Frame)
        );
        // A single-segment frame that declares another size than 2x2 RGB.
        assert_eq!(decode(&frame(0x20, &[13, 0])), Err(ImageError::Size));
        assert_eq!(
            decode(&frame(0xe0, &[0, 0, 0, 0, 1, 0, 0, 0, 0])),
            Err(ImageError::Size)
        );
        // The declared size is cut off.
        assert_eq!(decode(&frame(0xe0, &[0, 0, 0])), Err(ImageError::Frame));
    }

    /// Damaged images must fail or decode to the announced size; they must
    /// never panic or produce another amount of pixels.
    fn decode_damaged(decoder: &mut Decoder, rgba: &mut Vec<u8>, data: &[u8]) {
        if let Ok((w, h)) = decoder.decode(data, rgba) {
            assert_eq!(rgba.len(), (w * h * 4) as usize);
            assert!(w as u64 * h as u64 <= MAX_PIXELS as u64);
            assert!(rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
        }
    }

    #[test]
    fn truncated_images_fail_cleanly() {
        let mut decoder = Decoder::default();
        let mut rgba = Vec::new();
        for colors in [3, 40, 5000] {
            let encoded = encode(48, 20, &image(48, 20, colors, 9));
            for len in 0..encoded.len() {
                assert!(
                    decoder.decode(&encoded[..len], &mut rgba).is_err(),
                    "cut at {len} of {}",
                    encoded.len()
                );
            }
            // The decoder is still usable.
            assert_eq!(decoder.decode(&encoded, &mut rgba), Ok((48, 20)));
        }
    }

    #[test]
    fn damaged_images_never_panic() {
        let mut decoder = Decoder::default();
        let mut rgba = Vec::new();
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for colors in [2, 12, 200, 9000] {
            let pixels = image(40, 24, colors, 3);
            let encoded = encode(40, 24, &pixels);
            // Every byte replaced by a few other values, one at a time.
            for at in 0..encoded.len() {
                for value in [0, 1, 0x7f, 0x80, 0xff, encoded[at] ^ 0x10, encoded[at] ^ 1] {
                    let mut bad = encoded.clone();
                    bad[at] = value;
                    decode_damaged(&mut decoder, &mut rgba, &bad);
                }
            }
            // Several bytes at once, bytes removed and inserted.
            for _ in 0..4000 {
                let mut bad = encoded.clone();
                for _ in 0..1 + rng.below(4) {
                    let at = rng.below(bad.len() as u32) as usize;
                    match rng.below(3) {
                        0 => bad[at] = rng.next() as u8,
                        1 => {
                            bad.remove(at);
                        }
                        _ => bad.insert(at, rng.next() as u8),
                    }
                    if bad.is_empty() {
                        break;
                    }
                }
                decode_damaged(&mut decoder, &mut rgba, &bad);
            }
            assert_eq!(decoder.decode(&encoded, &mut rgba), Ok((40, 24)));
            assert_eq!(rgba, pixels);
        }
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut decoder = Decoder::default();
        let mut rgba = Vec::new();
        let mut rng = Rng(42);
        for _ in 0..20_000 {
            let len = rng.below(200) as usize;
            let mut data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            // Most of them with a plausible header and frame start, so that
            // the frame decoder is reached.
            if len > 16 && rng.below(4) != 0 {
                data[0] = rng.below(2) as u8;
                data[1..5].copy_from_slice(&[
                    1 + rng.below(40) as u8,
                    0,
                    1 + rng.below(40) as u8,
                    0,
                ]);
                let frame = if data[0] == 0 {
                    data[5] = rng.below(6) as u8;
                    6 + (data[5] as usize + 1) * 3
                } else {
                    5
                };
                if frame + 5 < len {
                    data[frame..frame + 4].copy_from_slice(&ZSTD_MAGIC);
                    data[frame + 4] &= 0xe4;
                }
            }
            decode_damaged(&mut decoder, &mut rgba, &data);
        }
    }
}

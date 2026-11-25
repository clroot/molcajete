//! Image encoding for display updates.
//!
//! Lossless updates use PNG, as an 8-bit palette image when the region has at
//! most 256 colours (typical for UI and text) and RGB otherwise. Clients
//! that announce the zstd image type (`molcajete_protocol::zimage`) get it
//! instead of PNG wherever it is the better choice ([`ZstdEncoder`]).
//! Frequently updated, colourful regions may use JPEG when the client
//! supports it.

use molcajete_protocol::zimage;

/// DEFLATE setting of display updates. On text, level 4 is 4 to 5 times
/// faster than zlib's default (6) for 5% more bytes; level 1 saves another
/// third of the time for 17% more bytes, and fdeflate is 2.5 to 3 times larger
/// (see `display::bench::png_levels`).
const PNG_COMPRESSION: png::DeflateCompression = png::DeflateCompression::Level(4);

/// Pixel region extracted from the framebuffer (RGBA8, tightly packed).
pub struct Region<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba: &'a [u8],
}

/// Returns the colour if every pixel of the region is identical.
pub fn solid_color(region: &Region) -> Option<[u8; 4]> {
    let mut pixels = region.rgba.chunks_exact(4);
    let first = pixels.next()?;
    pixels
        .all(|p| p == first)
        .then(|| [first[0], first[1], first[2], first[3]])
}

/// Builds a palette (RGB triplets) and the index image, or `None` when the
/// region has more than 256 colours.
fn palettize(region: &Region) -> Option<(Vec<u8>, Vec<u8>)> {
    // Open-addressed table of the colours seen so far. Anti-aliased text
    // changes colour at nearly every pixel, so the lookup has to be cheap.
    const SLOTS: usize = 1024;
    const EMPTY: u32 = u32::MAX;
    let mut keys = [EMPTY; SLOTS];
    let mut values = [0u8; SLOTS];
    let mut palette = Vec::with_capacity(256 * 3);
    let mut indices = Vec::with_capacity((region.width * region.height) as usize);
    let mut last = (EMPTY, 0u8);
    for p in region.rgba.as_chunks::<4>().0 {
        let key = u32::from_le_bytes([p[0], p[1], p[2], 0]);
        // Runs of identical pixels are common; skip the lookup for them.
        if key != last.0 {
            let mut slot = (key.wrapping_mul(0x9e37_79b1) >> 22) as usize;
            while keys[slot] != key && keys[slot] != EMPTY {
                slot = (slot + 1) % SLOTS;
            }
            if keys[slot] == EMPTY {
                let colors = palette.len() / 3;
                if colors == 256 {
                    return None;
                }
                keys[slot] = key;
                values[slot] = colors as u8;
                palette.extend_from_slice(&p[..3]);
            }
            last = (key, values[slot]);
        }
        indices.push(last.1);
    }
    Some((palette, indices))
}

fn pack_indices(indices: &[u8], width: u32, height: u32, bits: u8) -> Vec<u8> {
    if bits == 8 {
        return indices.to_vec();
    }
    let per_byte = (8 / bits) as usize;
    let row_bytes = (width as usize).div_ceil(per_byte);
    let mut out = vec![0u8; row_bytes * height as usize];
    for y in 0..height as usize {
        let row = &indices[y * width as usize..(y + 1) * width as usize];
        let dst = &mut out[y * row_bytes..(y + 1) * row_bytes];
        for (x, &i) in row.iter().enumerate() {
            let shift = 8 - bits as usize * (x % per_byte + 1);
            dst[x / per_byte] |= i << shift;
        }
    }
    out
}

/// Lossless PNG encoding. Opaque regions with at most 256 colours become
/// palette images with the smallest bit depth; regions with transparency keep
/// their alpha channel.
pub fn encode_png(region: &Region) -> Result<Vec<u8>, png::EncodingError> {
    encode_png_with(region, PNG_COMPRESSION)
}

/// [`encode_png`] with a given DEFLATE setting.
pub fn encode_png_with(
    region: &Region,
    compression: png::DeflateCompression,
) -> Result<Vec<u8>, png::EncodingError> {
    let mut out = Vec::with_capacity(region.rgba.len() / 4);
    let mut encoder = png::Encoder::new(&mut out, region.width, region.height);
    encoder.set_deflate_compression(compression);
    let opaque = super::is_opaque(region.rgba);
    if !opaque {
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_filter(png::Filter::Adaptive);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(region.rgba)?;
        writer.finish()?;
        return Ok(out);
    }
    if let Some((palette, indices)) = palettize(region) {
        let colors = palette.len() / 3;
        let bits: u8 = match colors {
            0..=2 => 1,
            3..=4 => 2,
            5..=16 => 4,
            _ => 8,
        };
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(match bits {
            1 => png::BitDepth::One,
            2 => png::BitDepth::Two,
            4 => png::BitDepth::Four,
            _ => png::BitDepth::Eight,
        });
        encoder.set_palette(palette);
        // Filtering rarely helps palette data and costs time.
        encoder.set_filter(png::Filter::NoFilter);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&pack_indices(&indices, region.width, region.height, bits))?;
        writer.finish()?;
    } else {
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_filter(png::Filter::Adaptive);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&rgb_of(region))?;
        writer.finish()?;
    }
    Ok(out)
}

fn rgb_of(region: &Region) -> Vec<u8> {
    region
        .rgba
        .chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect()
}

/// Zstandard level of zstd images. On recorded sessions level 3 takes 4 to
/// 11% fewer bytes than level 1 for 5% more time, and level 5 another 2 to
/// 3% for up to twice the time (docs/architecture.md, section 2.7).
const ZSTD_LEVEL: i32 = 3;

/// An RGB region is sent as a zstd image when at least this share of its
/// pixels repeats the pixel to the left, which tells screen content from
/// photographs: 94% and more in the windows and pages of recorded sessions,
/// 3 to 15% in photographs. Zstandard has no row filter, so a photograph
/// takes a quarter to three quarters more bytes than as PNG, while a window
/// takes about 40% fewer.
const FLAT_SHARE: (usize, usize) = (3, 4);

/// [`is_flat`] looks at one row in this many.
const FLAT_ROW_STEP: usize = 4;

/// Whether an RGB region looks like screen content rather than a
/// photograph.
fn is_flat(region: &Region) -> bool {
    let row = region.width as usize * 4;
    let (mut same, mut pairs) = (0usize, 0usize);
    for line in region.rgba.chunks_exact(row).step_by(FLAT_ROW_STEP) {
        let pixels = line.as_chunks::<4>().0;
        same += pixels.windows(2).filter(|p| p[0] == p[1]).count();
        pairs += pixels.len() - 1;
    }
    same * FLAT_SHARE.1 >= pairs * FLAT_SHARE.0
}

/// Encoder of zstd images (`molcajete_protocol::zimage`). A session keeps
/// one, so that the compression context is reused for every image.
pub struct ZstdEncoder {
    context: zstd::bulk::Compressor<'static>,
}

impl ZstdEncoder {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            context: zstd::bulk::Compressor::new(ZSTD_LEVEL)?,
        })
    }

    /// Encodes an opaque region, or returns `None` where PNG is the better
    /// choice: for a photograph ([`FLAT_SHARE`]) and for a size the format
    /// cannot carry. A region of up to 256 colours is always encoded.
    pub fn encode(&mut self, region: &Region) -> Option<Vec<u8>> {
        if !zimage::fits(region.width, region.height) {
            return None;
        }
        let mut out = Vec::with_capacity(region.rgba.len() / 16 + 64);
        let raw = if let Some((palette, indices)) = palettize(region) {
            zimage::write_header(&mut out, region.width, region.height, Some(&palette));
            let bits = zimage::index_bits(palette.len() / 3);
            pack_indices(&indices, region.width, region.height, bits)
        } else if is_flat(region) {
            zimage::write_header(&mut out, region.width, region.height, None);
            rgb_of(region)
        } else {
            return None;
        };
        let header = out.len();
        out.resize(header + zstd::zstd_safe::compress_bound(raw.len()), 0);
        let written = self
            .context
            .compress_to_buffer(&raw, &mut out[header..])
            .ok()?;
        out.truncate(header + written);
        Some(out)
    }
}

/// Lossy JPEG encoding (opaque regions only).
pub fn encode_jpeg(region: &Region, quality: u8) -> Result<Vec<u8>, jpeg_encoder::EncodingError> {
    let mut out = Vec::with_capacity(region.rgba.len() / 8);
    let encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder.encode(
        region.rgba,
        region.width as u16,
        region.height as u16,
        jpeg_encoder::ColorType::Rgba,
    )?;
    Ok(out)
}

/// Rough measure of how "photographic" a region is: the number of distinct
/// colours among sampled pixels.
pub fn color_complexity(region: &Region) -> usize {
    let mut seen = std::collections::HashSet::with_capacity(512);
    let pixels = region.rgba.len() / 4;
    let step = (pixels / 2048).max(1);
    for i in (0..pixels).step_by(step) {
        let p = &region.rgba[i * 4..i * 4 + 3];
        seen.insert([p[0], p[1], p[2]]);
        if seen.len() > 512 {
            break;
        }
    }
    seen.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        // Normalize to RGB.
        let rgb = match info.color_type {
            png::ColorType::Rgb => buf,
            png::ColorType::Rgba => buf
                .chunks_exact(4)
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect(),
            other => panic!("unexpected {other:?}"),
        };
        (info.width, info.height, rgb)
    }

    fn rgb_of(rgba: &[u8]) -> Vec<u8> {
        rgba.chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect()
    }

    #[test]
    fn solid_detection() {
        let px = [7u8, 8, 9, 255].repeat(6);
        assert_eq!(
            solid_color(&Region {
                width: 3,
                height: 2,
                rgba: &px
            }),
            Some([7, 8, 9, 255])
        );
        let mut px2 = px.clone();
        px2[4] = 0;
        assert_eq!(
            solid_color(&Region {
                width: 3,
                height: 2,
                rgba: &px2
            }),
            None
        );
    }

    #[test]
    fn palette_png_round_trips_at_every_depth() {
        for colors in [2usize, 3, 9, 200] {
            let (w, h) = (13u32, 5u32);
            let rgba: Vec<u8> = (0..w * h)
                .flat_map(|i| {
                    let c = (i as usize % colors) as u8;
                    [c, c.wrapping_mul(3), 255 - c, 255]
                })
                .collect();
            let bytes = encode_png(&Region {
                width: w,
                height: h,
                rgba: &rgba,
            })
            .unwrap();
            let (dw, dh, rgb) = decode(&bytes);
            assert_eq!((dw, dh), (w, h));
            assert_eq!(rgb, rgb_of(&rgba), "colors = {colors}");
        }
    }

    #[test]
    fn truecolor_png_round_trips() {
        let (w, h) = (40u32, 20u32);
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                [
                    (i % 251) as u8,
                    (i * 7 % 253) as u8,
                    (i * 13 % 255) as u8,
                    255,
                ]
            })
            .collect();
        let bytes = encode_png(&Region {
            width: w,
            height: h,
            rgba: &rgba,
        })
        .unwrap();
        let (_, _, rgb) = decode(&bytes);
        assert_eq!(rgb, rgb_of(&rgba));
    }

    fn region(width: u32, height: u32, rgba: &[u8]) -> Region<'_> {
        Region {
            width,
            height,
            rgba,
        }
    }

    /// `colors` colours in runs of `run` pixels.
    fn runs(width: u32, height: u32, colors: u32, run: u32) -> Vec<u8> {
        let mut state = 0x9E37_79B9u32;
        let mut color = [0u8; 4];
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for i in 0..width * height {
            if i % run == 0 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let c = state % colors;
                color = [c as u8, (c >> 8) as u8, (c >> 16) as u8, 255];
            }
            rgba.extend_from_slice(&color);
        }
        rgba
    }

    #[test]
    fn zstd_images_round_trip() {
        let mut encoder = ZstdEncoder::new().unwrap();
        let mut decoder = zimage::Decoder::default();
        let mut decoded = Vec::new();
        let shapes = [
            (1, 1),
            (1, 300),
            (300, 1),
            (5, 3),
            (17, 9),
            (640, 480),
            // More than the 2 MiB window of the compression level, as
            // indices and as RGB.
            (2400, 1300),
        ];
        for (w, h) in shapes {
            // 1, 2, 4 and 8 bits per index, then RGB.
            for colors in [1, 2, 4, 16, 200, 1 << 24] {
                for run in [1, 9] {
                    let rgba = runs(w, h, colors, run);
                    let rgb = palettize(&region(w, h, &rgba)).is_none();
                    let encoded = encoder.encode(&region(w, h, &rgba));
                    // Only RGB in which every pixel differs is left to PNG.
                    assert_eq!(
                        encoded.is_none(),
                        rgb && run == 1 && w > 1,
                        "{w}x{h} {colors}"
                    );
                    let Some(encoded) = encoded else { continue };
                    assert_eq!(encoded[0] == 1, rgb, "{w}x{h} {colors}: kind");
                    assert_eq!(decoder.decode(&encoded, &mut decoded), Ok((w, h)));
                    assert!(decoded == rgba, "{w}x{h}, {colors} colours, runs of {run}");
                }
            }
        }
    }

    #[test]
    fn large_zstd_images_span_records() {
        // Text-like noise does not compress below a record (64 KiB).
        let rgba = runs(800, 600, 200, 1);
        let encoded = ZstdEncoder::new().unwrap().encode(&region(800, 600, &rgba));
        let encoded = encoded.expect("indexed");
        assert!(encoded.len() > 64 * 1024, "{} bytes", encoded.len());
        let mut decoded = Vec::new();
        let size = zimage::Decoder::default().decode(&encoded, &mut decoded);
        assert_eq!(size, Ok((800, 600)));
        assert!(decoded == rgba);
    }

    #[test]
    fn sizes_the_format_cannot_carry_are_left_to_png() {
        let mut encoder = ZstdEncoder::new().unwrap();
        // Wider than 16 bits.
        let rgba = [1u8, 2, 3, 255].repeat(70_000);
        assert!(encoder.encode(&region(70_000, 1, &rgba)).is_none());
        assert!(encoder.encode(&region(1, 70_000, &rgba)).is_none());
        assert!(encoder.encode(&region(35_000, 2, &rgba)).is_some());
    }

    #[test]
    fn screen_content_is_told_from_photographs() {
        // Runs of 8 pixels: 7 of 8 pixels repeat their neighbour.
        assert!(is_flat(&region(256, 64, &runs(256, 64, 1 << 24, 8))));
        // Runs of 2: half of them do, which is below three quarters.
        assert!(!is_flat(&region(256, 64, &runs(256, 64, 1 << 24, 2))));
        assert!(!is_flat(&region(256, 64, &runs(256, 64, 1 << 24, 1))));
    }

    #[test]
    fn jpeg_produces_data() {
        let rgba = [200u8, 100, 50, 255].repeat(64 * 64);
        let bytes = encode_jpeg(
            &Region {
                width: 64,
                height: 64,
                rgba: &rgba,
            },
            80,
        )
        .unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
    }
}

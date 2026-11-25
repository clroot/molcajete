//! Image encoding for display updates.
//!
//! Lossless updates use PNG, as an 8-bit palette image when the region has at
//! most 256 colours (typical for UI and text) and RGB otherwise. Frequently
//! updated, colourful regions may use JPEG when the client supports it.

use std::collections::HashMap;

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
    let mut lookup: HashMap<u32, u8> = HashMap::with_capacity(64);
    let mut palette = Vec::with_capacity(256 * 3);
    let mut indices = Vec::with_capacity((region.width * region.height) as usize);
    let mut last: Option<(u32, u8)> = None;
    for p in region.rgba.chunks_exact(4) {
        let key = u32::from_le_bytes([p[0], p[1], p[2], 0]);
        // Runs of identical pixels are common; skip the hash lookup for them.
        let index = match last {
            Some((k, i)) if k == key => i,
            _ => {
                let i = match lookup.get(&key) {
                    Some(&i) => i,
                    None => {
                        if lookup.len() >= 256 {
                            return None;
                        }
                        let i = lookup.len() as u8;
                        lookup.insert(key, i);
                        palette.extend_from_slice(&p[..3]);
                        i
                    }
                };
                last = Some((key, i));
                i
            }
        };
        indices.push(index);
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
    let mut out = Vec::with_capacity(region.rgba.len() / 4);
    let mut encoder = png::Encoder::new(&mut out, region.width, region.height);
    // `Balanced` is ~2.4x smaller than `Fast` on typical desktop/terminal
    // updates at ~0.1 ms per image.
    encoder.set_compression(png::Compression::Balanced);
    let opaque = region.rgba.chunks_exact(4).all(|p| p[3] == 255);
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
        let rgb: Vec<u8> = region
            .rgba
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&rgb)?;
        writer.finish()?;
    }
    Ok(out)
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

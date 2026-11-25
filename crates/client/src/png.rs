//! PNG decoding in wasm, for seeking through recordings.
//!
//! In a session PNG images go to the browser's decoder, off the thread that
//! draws. Seeking applies every image of the recording without showing any
//! of them, and there the round trip through the browser's decoder and the
//! staging canvas costs five times what decoding here and uploading from
//! wasm memory does (`tests/browser/seek-bench.mjs`), as for zstd images.

thread_local! {
    /// Decoded rows of an image that is not RGBA, reused between images.
    static SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Decodes a PNG into premultiplied RGBA (top row first) in `out`, which is
/// reused, and returns its size and whether every pixel is opaque.
pub fn decode_premultiplied(data: &[u8], out: &mut Vec<u8>) -> Result<(u32, u32, bool), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    // Palettes and low bit depths become 8 bits per channel, 16 bits
    // become 8; tRNS becomes an alpha channel.
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let size = reader.output_buffer_size().ok_or("PNG image too large")?;
    let (width, height, color) = {
        let info = reader.info();
        let (color, _) = reader.output_color_type();
        (info.width, info.height, color)
    };
    let pixels = width as usize * height as usize;
    let rgba_len = pixels.checked_mul(4).ok_or("PNG image too large")?;
    out.clear();
    out.resize(rgba_len, 255);
    if color == png::ColorType::Rgba {
        if size != rgba_len {
            return Err("PNG data does not match its size".into());
        }
        reader.next_frame(out).map_err(|e| e.to_string())?;
        return Ok((width, height, premultiply(out)));
    }
    SCRATCH.with_borrow_mut(|raw| {
        raw.clear();
        raw.resize(size, 0);
        let info = reader.next_frame(raw).map_err(|e| e.to_string())?;
        let raw = &raw[..info.buffer_size()];
        let rgba = out.as_chunks_mut::<4>().0;
        let fits = |bytes: usize| raw.len() == pixels * bytes;
        match color {
            png::ColorType::Rgb if fits(3) => {
                for (o, i) in rgba.iter_mut().zip(raw.as_chunks::<3>().0) {
                    o[..3].copy_from_slice(i);
                }
                Ok((width, height, true))
            }
            png::ColorType::Grayscale if fits(1) => {
                for (o, &g) in rgba.iter_mut().zip(raw) {
                    o[..3].fill(g);
                }
                Ok((width, height, true))
            }
            png::ColorType::GrayscaleAlpha if fits(2) => {
                for (o, i) in rgba.iter_mut().zip(raw.as_chunks::<2>().0) {
                    *o = [i[0], i[0], i[0], i[1]];
                }
                Ok((width, height, premultiply(out)))
            }
            _ => Err("PNG data does not match its size".into()),
        }
    })
}

/// Premultiplies RGBA in place; returns whether every pixel was opaque.
fn premultiply(rgba: &mut [u8]) -> bool {
    let mut opaque = true;
    for p in rgba.as_chunks_mut::<4>().0 {
        let a = p[3];
        if a != 255 {
            opaque = false;
            for c in &mut p[..3] {
                *c = ((*c as u32 * a as u32 + 127) / 255) as u8;
            }
        }
    }
    opaque
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(
        color: png::ColorType,
        depth: png::BitDepth,
        w: u32,
        h: u32,
        data: &[u8],
        palette: Option<(&[u8], &[u8])>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, w, h);
            encoder.set_color(color);
            encoder.set_depth(depth);
            if let Some((palette, trns)) = palette {
                encoder.set_palette(palette.to_vec());
                encoder.set_trns(trns.to_vec());
            }
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(data).unwrap();
        }
        out
    }

    #[test]
    fn decodes_every_color_type_premultiplied() {
        let mut out = Vec::new();
        let rgba = encode(
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            2,
            1,
            &[200, 100, 50, 255, 200, 100, 50, 128],
            None,
        );
        assert_eq!(decode_premultiplied(&rgba, &mut out), Ok((2, 1, false)));
        assert_eq!(out, [200, 100, 50, 255, 100, 50, 25, 128]);

        let rgb = encode(
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            1,
            1,
            &[1, 2, 3],
            None,
        );
        assert_eq!(decode_premultiplied(&rgb, &mut out), Ok((1, 1, true)));
        assert_eq!(out, [1, 2, 3, 255]);

        let gray = encode(
            png::ColorType::GrayscaleAlpha,
            png::BitDepth::Eight,
            1,
            1,
            &[100, 0],
            None,
        );
        decode_premultiplied(&gray, &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 0]);

        // A 1-bit palette with a transparent entry, as libguac writes them.
        let indexed = encode(
            png::ColorType::Indexed,
            png::BitDepth::One,
            3,
            1,
            &[0b0100_0000],
            Some((&[10, 20, 30, 40, 50, 60], &[0])),
        );
        assert_eq!(decode_premultiplied(&indexed, &mut out), Ok((3, 1, false)));
        assert_eq!(out, [0, 0, 0, 0, 40, 50, 60, 255, 0, 0, 0, 0]);

        let wide = encode(
            png::ColorType::Grayscale,
            png::BitDepth::Sixteen,
            1,
            1,
            &[0xAB, 0xCD],
            None,
        );
        decode_premultiplied(&wide, &mut out).unwrap();
        assert_eq!(out, [0xAB, 0xAB, 0xAB, 255]);
    }

    #[test]
    fn refuses_damaged_images() {
        let rgb = encode(
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            4,
            4,
            &[7; 48],
            None,
        );
        let mut out = Vec::new();
        // Cut anywhere, it fails or (missing only the end chunk) has every pixel.
        for len in 0..rgb.len() {
            if let Ok(size) = decode_premultiplied(&rgb[..len], &mut out) {
                assert_eq!((size, out.len()), ((4, 4, true), 64), "{len}");
            }
        }
        assert!(decode_premultiplied(&rgb[..rgb.len() / 2], &mut out).is_err());
        assert!(decode_premultiplied(b"not a png", &mut out).is_err());
    }
}

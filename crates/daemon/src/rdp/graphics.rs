//! Pixel and cursor conversions between IronRDP's decoded data and the
//! RGBA buffers of [`crate::display::Display`].

use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_graphics::pointer::DecodedPointer;
use ironrdp_pdu::geometry::InclusiveRectangle;

use crate::display::pointer::PointerImage;
use crate::display::{ChannelOrder, SourceImage};

/// A rectangle in desktop pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Area {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Area {
    /// Converts an inclusive rectangle, clipped to a `width`×`height`
    /// surface. Returns `None` when nothing is left.
    pub fn clipped(rect: &InclusiveRectangle, width: u32, height: u32) -> Option<Self> {
        let left = u32::from(rect.left);
        let top = u32::from(rect.top);
        let right = (u32::from(rect.right) + 1).min(width);
        let bottom = (u32::from(rect.bottom) + 1).min(height);
        (left < right && top < bottom).then(|| Self {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    }
}

/// The part of a decoded image (`stride` bytes per row, pixels in `format`)
/// that starts at the top-left corner of `area`, for
/// [`crate::display::Display::put_image`]. `None` if the format does not
/// have 4 bytes per pixel or `area` starts past the end of `data`.
pub fn source_at(
    data: &[u8],
    stride: usize,
    format: PixelFormat,
    area: Area,
) -> Option<SourceImage<'_>> {
    if format.bytes_per_pixel() != 4 {
        return None;
    }
    let [r, g, b, _] = format.channel_offsets();
    let channels = ChannelOrder::new(r as u8, g as u8, b as u8);
    SourceImage::at(data, stride, channels, area.x, area.y)
}

/// Copies `area` out of a decoded image (`stride` bytes per row, pixels in
/// `format`) as opaque RGBA, top row first: the copy made for every update
/// before [`source_at`], kept as the reference for tests and the benchmark.
#[cfg(test)]
pub fn extract_rgba(data: &[u8], stride: usize, format: PixelFormat, area: Area) -> Vec<u8> {
    let bpp = usize::from(format.bytes_per_pixel());
    let [r, g, b, _] = format.channel_offsets();
    let (x, y) = (area.x as usize, area.y as usize);
    let (w, h) = (area.width as usize, area.height as usize);
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in y..y + h {
        let start = row * stride + x * bpp;
        let Some(src) = data.get(start..start + w * bpp) else {
            break;
        };
        if format == PixelFormat::RgbA32 || format == PixelFormat::RgbX32 {
            rgba.extend_from_slice(src);
            let row_start = rgba.len() - w * 4;
            for alpha in rgba[row_start..].iter_mut().skip(3).step_by(4) {
                *alpha = 0xFF;
            }
        } else {
            for px in src.chunks_exact(bpp) {
                rgba.extend_from_slice(&[px[r], px[g], px[b], 0xFF]);
            }
        }
    }
    rgba
}

/// A cursor image ready for [`crate::display::Display::set_cursor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    pub width: u32,
    pub height: u32,
    /// Straight (non-premultiplied) RGBA, top row first.
    pub rgba: Vec<u8>,
}

impl Cursor {
    /// A fully transparent 1×1 cursor.
    pub fn hidden() -> Self {
        Self {
            hotspot_x: 0,
            hotspot_y: 0,
            width: 1,
            height: 1,
            rgba: vec![0; 4],
        }
    }

    /// Converts a pointer decoded for accelerated rendering (straight RGBA,
    /// top row first). Malformed pointers yield `None`; empty ones are hidden.
    pub fn from_pointer(pointer: &DecodedPointer) -> Option<Self> {
        let (width, height) = (u32::from(pointer.width), u32::from(pointer.height));
        if width == 0 || height == 0 {
            return Some(Self::hidden());
        }
        let len = (width * height * 4) as usize;
        let rgba = pointer.bitmap_data.get(..len)?.to_vec();
        Some(Self {
            hotspot_x: u32::from(pointer.hotspot_x).min(width - 1),
            hotspot_y: u32::from(pointer.hotspot_y).min(height - 1),
            width,
            height,
            rgba,
        })
    }

    /// The standard arrow, shown when the server asks for its default
    /// pointer, at the size it has at a desktop scale of `scale` percent.
    pub fn arrow(scale: u32) -> Self {
        // '#' black, '.' white, ' ' transparent.
        const ARROW: [&str; 19] = [
            "#           ",
            "##          ",
            "#.#         ",
            "#..#        ",
            "#...#       ",
            "#....#      ",
            "#.....#     ",
            "#......#    ",
            "#.......#   ",
            "#........#  ",
            "#.........# ",
            "#......#####",
            "#...#..#    ",
            "#..# #..#   ",
            "#.#  #..#   ",
            "##    #..#  ",
            "#     #..#  ",
            "       #..# ",
            "       ###  ",
        ];
        let rgba = ARROW
            .iter()
            .flat_map(|row| row.bytes())
            .flat_map(|cell| match cell {
                b'#' => [0, 0, 0, 0xFF],
                b'.' => [0xFF, 0xFF, 0xFF, 0xFF],
                _ => [0, 0, 0, 0],
            })
            .collect();
        let arrow = PointerImage {
            hotspot_x: 0,
            hotspot_y: 0,
            width: ARROW[0].len() as u32,
            height: ARROW.len() as u32,
            rgba,
        }
        .for_dpi(scale.saturating_mul(96) / 100);
        Self {
            hotspot_x: arrow.hotspot_x,
            hotspot_y: arrow.hotspot_y,
            width: arrow.width,
            height: arrow.height,
            rgba: arrow.rgba,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: u16, top: u16, right: u16, bottom: u16) -> InclusiveRectangle {
        InclusiveRectangle {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn inclusive_rectangles_are_clipped() {
        assert_eq!(
            Area::clipped(&rect(1, 2, 3, 4), 100, 100),
            Some(Area {
                x: 1,
                y: 2,
                width: 3,
                height: 3
            })
        );
        assert_eq!(
            Area::clipped(&rect(8, 8, 20, 20), 10, 10).map(|a| (a.width, a.height)),
            Some((2, 2))
        );
        assert_eq!(Area::clipped(&rect(10, 0, 12, 1), 10, 10), None);
    }

    /// A 3×2 image in `format` whose pixel (x, y) has color (x, y, 7).
    fn image(format: PixelFormat) -> Vec<u8> {
        let [r, g, b, a] = format.channel_offsets();
        let mut data = vec![0u8; 3 * 2 * 4];
        for y in 0..2 {
            for x in 0..3 {
                let px = &mut data[(y * 3 + x) * 4..][..4];
                px[r] = x as u8;
                px[g] = y as u8;
                px[b] = 7;
                px[a] = 0; // must be ignored
            }
        }
        data
    }

    #[test]
    fn pixels_become_opaque_rgba_in_every_format() {
        let area = Area {
            x: 1,
            y: 0,
            width: 2,
            height: 2,
        };
        let expected = vec![1, 0, 7, 255, 2, 0, 7, 255, 1, 1, 7, 255, 2, 1, 7, 255];
        for format in [
            PixelFormat::RgbA32,
            PixelFormat::RgbX32,
            PixelFormat::BgrA32,
            PixelFormat::BgrX32,
            PixelFormat::ARgb32,
            PixelFormat::XRgb32,
            PixelFormat::ABgr32,
            PixelFormat::XBgr32,
        ] {
            let rgba = extract_rgba(&image(format), 12, format, area);
            assert_eq!(rgba, expected, "{format:?}");
        }
    }

    /// Copying straight from the decoded image gives the framebuffer the
    /// previous `extract_rgba` + `Display::put` gave it, in every format,
    /// with a padded stride and with rectangles that leave the display.
    #[test]
    fn copies_match_extract_and_put() {
        use crate::display::{Display, ImageFormats};
        use crate::io::Output;

        let (width, height, stride) = (37u32, 23u32, 37 * 4 + 12);
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let data: Vec<u8> = (0..stride * height as usize)
            .map(|_| next() as u8)
            .collect();
        let new_display = || {
            let mut display = Display::new(ImageFormats::default(), true);
            // Narrower than the image, so that rectangles get clipped.
            display.resize(width - 5, height - 3, &mut Output::text());
            display
        };
        for format in [
            PixelFormat::RgbA32,
            PixelFormat::RgbX32,
            PixelFormat::BgrA32,
            PixelFormat::BgrX32,
            PixelFormat::ARgb32,
            PixelFormat::XRgb32,
            PixelFormat::ABgr32,
            PixelFormat::XBgr32,
        ] {
            let (mut old, mut new) = (new_display(), new_display());
            for _ in 0..200 {
                let left = (next() % u64::from(width)) as u16;
                let top = (next() % u64::from(height)) as u16;
                let right = left + (next() % 12) as u16;
                let bottom = top + (next() % 12) as u16;
                let Some(area) = Area::clipped(&rect(left, top, right, bottom), width, height)
                else {
                    continue;
                };
                let rgba = extract_rgba(&data, stride, format, area);
                old.put(area.x, area.y, area.width, area.height, &rgba);
                let source = source_at(&data, stride, format, area).unwrap();
                new.put_image(area.x, area.y, area.width, area.height, &source);
            }
            assert!(old.framebuffer() == new.framebuffer(), "{format:?}");
        }
    }

    #[test]
    fn pointers_keep_their_hotspot() {
        let pointer = DecodedPointer {
            width: 2,
            height: 1,
            hotspot_x: 1,
            hotspot_y: 0,
            bitmap_data: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let cursor = Cursor::from_pointer(&pointer).unwrap();
        assert_eq!((cursor.hotspot_x, cursor.hotspot_y), (1, 0));
        assert_eq!((cursor.width, cursor.height), (2, 1));
        assert_eq!(cursor.rgba, pointer.bitmap_data);

        let truncated = DecodedPointer {
            bitmap_data: vec![0; 4],
            ..pointer
        };
        assert_eq!(Cursor::from_pointer(&truncated), None);
        assert_eq!(
            Cursor::from_pointer(&DecodedPointer::new_invisible()),
            Some(Cursor::hidden())
        );
    }

    #[test]
    fn arrow_cursor_is_well_formed() {
        for (scale, size) in [(100, (12, 19)), (200, (24, 38))] {
            let arrow = Cursor::arrow(scale);
            assert_eq!((arrow.width, arrow.height), size, "{scale} %");
            assert_eq!(arrow.rgba.len() as u32, arrow.width * arrow.height * 4);
            // Tip is black and opaque; the far corner is transparent.
            assert_eq!(&arrow.rgba[..4], &[0, 0, 0, 255]);
            assert_eq!(arrow.rgba[arrow.rgba.len() - 1], 0);
        }
    }
}

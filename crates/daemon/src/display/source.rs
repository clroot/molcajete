//! Pixels taken straight from a remote protocol's own image buffer, for
//! [`super::Display::put_image`].
//!
//! A decoder such as IronRDP keeps the whole desktop in a buffer of its own,
//! with a row stride and a channel order of its choosing. Copying a changed
//! rectangle from there into the framebuffer converts it to opaque RGBA in
//! the same pass, so no intermediate copy is made.

/// Order of the colour channels in a 4-byte source pixel, as byte offsets.
/// The fourth byte (alpha or padding) is ignored: copied pixels are opaque.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelOrder {
    red: u8,
    green: u8,
    blue: u8,
}

impl ChannelOrder {
    /// R, G, B, then alpha or padding.
    pub const RGBX: Self = Self::new(0, 1, 2);
    /// B, G, R, then alpha or padding.
    pub const BGRX: Self = Self::new(2, 1, 0);
    /// Alpha or padding, then R, G, B.
    pub const XRGB: Self = Self::new(1, 2, 3);
    /// Alpha or padding, then B, G, R.
    pub const XBGR: Self = Self::new(3, 2, 1);

    /// Channels at the given byte offsets (each below 4).
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        assert!(red < 4 && green < 4 && blue < 4);
        Self { red, green, blue }
    }

    /// Converts one row of 4-byte pixels to opaque RGBA. `src` and `dst`
    /// hold the same number of pixels.
    pub(super) fn convert_row(self, src: &[u8], dst: &mut [u8]) {
        debug_assert_eq!(src.len(), dst.len());
        // The common orders get loops with constant offsets, which the
        // compiler turns into vector shuffles (faster here than a plain
        // copy followed by setting the alpha bytes).
        match self {
            Self::RGBX => shuffle::<0, 1, 2>(src, dst),
            Self::BGRX => shuffle::<2, 1, 0>(src, dst),
            Self::XRGB => shuffle::<1, 2, 3>(src, dst),
            Self::XBGR => shuffle::<3, 2, 1>(src, dst),
            Self { red, green, blue } => {
                let (r, g, b) = (red as usize, green as usize, blue as usize);
                for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
                    d.copy_from_slice(&[s[r], s[g], s[b], 0xFF]);
                }
            }
        }
    }
}

fn shuffle<const R: usize, const G: usize, const B: usize>(src: &[u8], dst: &mut [u8]) {
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        d.copy_from_slice(&[s[R], s[G], s[B], 0xFF]);
    }
}

/// A source image: `data` starts with the pixel that lands on the top-left
/// corner of the destination rectangle, and each following row starts
/// `stride` bytes after the previous one.
#[derive(Clone, Copy, Debug)]
pub struct SourceImage<'a> {
    pub data: &'a [u8],
    pub stride: usize,
    pub channels: ChannelOrder,
}

impl<'a> SourceImage<'a> {
    /// The part of a whole image (`data` holds its top-left pixel first)
    /// that starts at pixel (`x`, `y`). `None` if that lies past the end.
    pub fn at(
        data: &'a [u8],
        stride: usize,
        channels: ChannelOrder,
        x: u32,
        y: u32,
    ) -> Option<Self> {
        let start = (y as usize)
            .checked_mul(stride)?
            .checked_add((x as usize).checked_mul(4)?)?;
        Some(Self {
            data: data.get(start..)?,
            stride,
            channels,
        })
    }

    /// Whether `w`×`h` pixels can be read: rows fit in the stride and the
    /// last row ends within `data`.
    pub(super) fn holds(&self, w: u32, h: u32) -> bool {
        let row = w as usize * 4;
        if h == 0 || w == 0 {
            return true;
        }
        row <= self.stride
            && (h as usize - 1)
                .checked_mul(self.stride)
                .and_then(|last| last.checked_add(row))
                .is_some_and(|end| end <= self.data.len())
    }
}

#[cfg(test)]
impl super::Display {
    /// The framebuffer, for tests and benchmarks outside this module.
    pub(crate) fn framebuffer(&self) -> &[u8] {
        &self.pixels
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{Display, ImageFormats};
    use crate::io::Output;

    fn display(width: u32, height: u32) -> Display {
        let mut display = Display::new(ImageFormats::default(), true);
        display.resize(width, height, &mut Output::text());
        display
    }

    fn pixel(display: &Display, x: u32, y: u32) -> [u8; 4] {
        let i = display.offset(x, y);
        display.pixels[i..i + 4].try_into().unwrap()
    }

    /// A `width`×`height` image with `stride` bytes per row (padding filled
    /// with 0xEE) whose pixel (x, y) has colour (x, y, 7) in `channels`.
    fn image(channels: ChannelOrder, width: u32, height: u32, stride: usize) -> Vec<u8> {
        let mut data = vec![0xEE; stride * height as usize];
        for y in 0..height {
            for x in 0..width {
                let px = &mut data[y as usize * stride + x as usize * 4..][..4];
                px.fill(0x11); // alpha or padding, must be ignored
                px[channels.red as usize] = x as u8;
                px[channels.green as usize] = y as u8;
                px[channels.blue as usize] = 7;
            }
        }
        data
    }

    #[test]
    fn every_channel_order_becomes_opaque_rgba() {
        for channels in [
            ChannelOrder::RGBX,
            ChannelOrder::BGRX,
            ChannelOrder::XRGB,
            ChannelOrder::XBGR,
            ChannelOrder::new(3, 0, 2),
        ] {
            let data = image(channels, 9, 5, 9 * 4);
            let mut d = display(9, 5);
            let source = SourceImage::at(&data, 9 * 4, channels, 0, 0).unwrap();
            d.put_image(0, 0, 9, 5, &source);
            for y in 0..5 {
                for x in 0..9 {
                    assert_eq!(pixel(&d, x, y), [x as u8, y as u8, 7, 255], "{channels:?}");
                }
            }
        }
    }

    #[test]
    fn rows_follow_the_stride() {
        // 6 pixels per row plus 10 bytes of padding; copy 3×2 from (2, 1).
        let stride = 6 * 4 + 10;
        let data = image(ChannelOrder::BGRX, 6, 4, stride);
        let mut d = display(10, 10);
        let source = SourceImage::at(&data, stride, ChannelOrder::BGRX, 2, 1).unwrap();
        d.put_image(5, 6, 3, 2, &source);
        for dy in 0..2 {
            for dx in 0..3 {
                assert_eq!(
                    pixel(&d, 5 + dx, 6 + dy),
                    [2 + dx as u8, 1 + dy as u8, 7, 255]
                );
            }
        }
        // Nothing around it was touched.
        assert_eq!(pixel(&d, 4, 6), [0; 4]);
        assert_eq!(pixel(&d, 8, 6), [0; 4]);
        assert_eq!(pixel(&d, 5, 8), [0; 4]);
    }

    #[test]
    fn rectangles_are_clipped_at_the_display_edge() {
        let data = image(ChannelOrder::RGBX, 8, 8, 32);
        let mut d = display(6, 5);
        let source = SourceImage::at(&data, 32, ChannelOrder::RGBX, 4, 3).unwrap();
        d.put_image(4, 3, 4, 4, &source);
        for y in 3..5 {
            for x in 4..6 {
                assert_eq!(pixel(&d, x, y), [x as u8, y as u8, 7, 255]);
            }
        }
        assert_eq!(pixel(&d, 3, 3), [0; 4]);
        // Entirely outside: nothing happens.
        let before = d.pixels.clone();
        d.put_image(6, 0, 2, 2, &source);
        d.put_image(0, 5, 2, 2, &source);
        assert_eq!(d.pixels, before);
    }

    #[test]
    fn short_sources_are_ignored() {
        let data = image(ChannelOrder::RGBX, 4, 4, 16);
        let mut d = display(8, 8);
        // Only 3 full rows remain after (0, 1).
        let source = SourceImage::at(&data, 16, ChannelOrder::RGBX, 0, 1).unwrap();
        d.put_image(0, 0, 4, 4, &source);
        assert!(d.pixels.iter().all(|&b| b == 0));
        // Rows wider than the stride.
        let narrow = SourceImage {
            data: &data,
            stride: 8,
            channels: ChannelOrder::RGBX,
        };
        d.put_image(0, 0, 4, 2, &narrow);
        assert!(d.pixels.iter().all(|&b| b == 0));
        assert!(SourceImage::at(&data, 16, ChannelOrder::RGBX, 0, 4).is_some());
        assert!(SourceImage::at(&data, 16, ChannelOrder::RGBX, 1, 4).is_none());
        // A clipped rectangle only needs the rows and columns it keeps.
        let mut d = display(2, 2);
        d.put_image(0, 0, 4, 4, &source);
        assert_eq!(pixel(&d, 1, 1), [1, 2, 7, 255]);
    }

    #[test]
    fn updates_mark_their_tiles_dirty() {
        let data = image(ChannelOrder::RGBX, 4, 4, 16);
        let mut d = display(256, 256);
        d.dirty.iter_mut().for_each(|dirty| *dirty = false);
        let source = SourceImage::at(&data, 16, ChannelOrder::RGBX, 0, 0).unwrap();
        d.put_image(130, 70, 4, 4, &source);
        let dirty: Vec<usize> = (0..d.dirty.len()).filter(|&i| d.dirty[i]).collect();
        assert_eq!(dirty, vec![(d.tile_cols + 2) as usize]);
    }
}

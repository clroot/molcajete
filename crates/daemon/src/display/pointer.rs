//! Pointer images the daemon draws itself.
//!
//! The pointers of a remote desktop come from its server, sized for the
//! resolution of the session. The ones drawn here (a terminal's I-beam,
//! the arrow an RDP server asks for as its default) are pixel art for
//! 96 dpi, and a client shows every pointer at one image pixel per pixel
//! of the display. At a higher resolution they are enlarged to match, or
//! they would shrink next to the text they point at.

/// The resolution the built-in pointers are drawn for.
const ART_DPI: u32 = 96;
/// The highest resolution a pointer is enlarged for.
const MAX_DPI: u32 = 480;

/// A pointer image (straight RGBA, top row first) with its hotspot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerImage {
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl PointerImage {
    /// This image, drawn for 96 dpi, at the size it has at `dpi`. Pixels
    /// are repeated, not blended, so the art keeps its hard edges. A
    /// resolution of 96 dpi or less (or none) leaves it as it is.
    pub fn for_dpi(self, dpi: u32) -> Self {
        let dpi = dpi.min(MAX_DPI);
        if dpi <= ART_DPI || self.width == 0 || self.height == 0 {
            return self;
        }
        let scaled = |pixels: u32| (pixels * dpi + ART_DPI / 2) / ART_DPI;
        let (width, height) = (scaled(self.width), scaled(self.height));
        // The source pixel under the centre of each scaled one.
        let source =
            |pixel: u32, limit: u32| (((2 * pixel + 1) * ART_DPI) / (2 * dpi)).min(limit - 1);
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            let row = (source(y, self.height) * self.width * 4) as usize;
            for x in 0..width {
                let i = row + (source(x, self.width) * 4) as usize;
                rgba.extend_from_slice(&self.rgba[i..i + 4]);
            }
        }
        Self {
            hotspot_x: scaled(self.hotspot_x).min(width - 1),
            hotspot_y: scaled(self.hotspot_y).min(height - 1),
            width,
            height,
            rgba,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2×2 image whose pixels are 1, 2 / 3, 4 in every channel.
    fn art() -> PointerImage {
        PointerImage {
            hotspot_x: 1,
            hotspot_y: 1,
            width: 2,
            height: 2,
            rgba: [1u8, 2, 3, 4].iter().flat_map(|&v| [v; 4]).collect(),
        }
    }

    fn values(image: &PointerImage) -> Vec<u8> {
        image.rgba.chunks_exact(4).map(|px| px[0]).collect()
    }

    #[test]
    fn stays_as_drawn_at_96_dpi_or_less() {
        for dpi in [0, 72, 96] {
            assert_eq!(art().for_dpi(dpi), art(), "dpi {dpi}");
        }
    }

    #[test]
    fn doubles_at_192_dpi() {
        let scaled = art().for_dpi(192);
        assert_eq!((scaled.width, scaled.height), (4, 4));
        assert_eq!((scaled.hotspot_x, scaled.hotspot_y), (2, 2));
        #[rustfmt::skip]
        assert_eq!(values(&scaled), [
            1, 1, 2, 2,
            1, 1, 2, 2,
            3, 3, 4, 4,
            3, 3, 4, 4,
        ]);
    }

    #[test]
    fn repeats_pixels_at_a_fractional_scale() {
        let scaled = art().for_dpi(144);
        assert_eq!((scaled.width, scaled.height), (3, 3));
        assert_eq!((scaled.hotspot_x, scaled.hotspot_y), (2, 2));
        assert_eq!(values(&scaled), [1, 2, 2, 3, 4, 4, 3, 4, 4]);
    }

    #[test]
    fn is_not_enlarged_beyond_the_highest_resolution() {
        let scaled = art().for_dpi(100_000);
        assert_eq!((scaled.width, scaled.height), (10, 10));
        assert_eq!(scaled.rgba.len(), 10 * 10 * 4);
    }
}

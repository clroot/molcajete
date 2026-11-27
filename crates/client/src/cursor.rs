//! The remote pointer as a CSS cursor.
//!
//! A browser shows a cursor image that names no resolution at one image
//! pixel per CSS pixel. The remote screen is drawn at `1 / scale` remote
//! pixels per CSS pixel instead (two on a screen with two device pixels
//! per CSS pixel), so the pointer is given that resolution, and its
//! hotspot, which CSS counts in CSS pixels, is divided by it.
//! `molcajete.js` does the same for a client in a worker.

/// Remote pixels per CSS pixel of a display shown at `scale` CSS pixels
/// per remote pixel.
pub fn density(scale: f64) -> f64 {
    if !scale.is_finite() || scale <= 0.0 {
        return 1.0;
    }
    let density = 1.0 / scale;
    // Close enough to one that the image is used as it is.
    if (density - 1.0).abs() < 0.01 {
        1.0
    } else {
        density
    }
}

/// Size and hotspot of a cursor image in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CssCursor {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
}

/// Where a cursor image of `width`×`height` remote pixels lands at
/// `density` remote pixels per CSS pixel. The hotspot stays inside the
/// image: browsers ignore a cursor whose hotspot does not.
pub fn css_cursor(
    width: u32,
    height: u32,
    hotspot_x: i32,
    hotspot_y: i32,
    density: f64,
) -> CssCursor {
    let size = |pixels: u32| ((f64::from(pixels) / density).round() as u32).max(1);
    let (width, height) = (size(width), size(height));
    let spot = |pixels: i32, size: u32| {
        ((f64::from(pixels.max(0)) / density).round() as u32).min(size - 1)
    };
    CssCursor {
        width,
        height,
        hotspot_x: spot(hotspot_x, width),
        hotspot_y: spot(hotspot_y, height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn density_is_the_inverse_of_the_display_scale() {
        assert_eq!(density(0.5), 2.0);
        assert_eq!(density(1.0 / 1.5), 1.5);
        assert_eq!(density(1.0), 1.0);
        assert_eq!(density(0.999), 1.0);
        // Before the first layout.
        assert_eq!(density(0.0), 1.0);
        assert_eq!(density(f64::NAN), 1.0);
    }

    #[test]
    fn a_pointer_for_twice_the_resolution_covers_half_the_css_pixels() {
        // What Windows sends at a desktop scale of 200 %.
        assert_eq!(
            css_cursor(64, 64, 20, 31, 2.0),
            CssCursor {
                width: 32,
                height: 32,
                hotspot_x: 10,
                hotspot_y: 16,
            }
        );
        assert_eq!(
            css_cursor(32, 32, 5, 9, 1.0),
            CssCursor {
                width: 32,
                height: 32,
                hotspot_x: 5,
                hotspot_y: 9,
            }
        );
    }

    #[test]
    fn the_hotspot_stays_inside_the_image() {
        let cursor = css_cursor(3, 1, 2, 0, 2.0);
        assert_eq!((cursor.width, cursor.height), (2, 1));
        assert_eq!((cursor.hotspot_x, cursor.hotspot_y), (1, 0));
        let cursor = css_cursor(1, 1, -4, 9, 4.0);
        assert_eq!((cursor.width, cursor.height), (1, 1));
        assert_eq!((cursor.hotspot_x, cursor.hotspot_y), (0, 0));
    }
}

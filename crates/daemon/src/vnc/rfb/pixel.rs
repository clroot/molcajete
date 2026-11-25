//! The pixel format this client requests and helpers to convert it to RGBA8.
//!
//! The client asks for 32 bits per pixel, depth 24, little-endian true colour with the red,
//! green and blue shifts at 16, 8 and 0. Every pixel therefore arrives as the bytes
//! `B, G, R, X`, and a ZRLE compressed pixel (CPIXEL) as the first three of those bytes.

/// Bytes per pixel on the wire.
pub(super) const BYTES_PER_PIXEL: usize = 4;

/// Bytes per ZRLE CPIXEL for the requested format.
pub(super) const BYTES_PER_CPIXEL: usize = 3;

/// PIXEL_FORMAT structure sent in SetPixelFormat.
pub(super) const PIXEL_FORMAT: [u8; 16] = [
    32, // bits-per-pixel
    24, // depth
    0,  // big-endian-flag
    1,  // true-colour-flag
    0, 255, // red-max
    0, 255, // green-max
    0, 255, // blue-max
    16,  // red-shift
    8,   // green-shift
    0,   // blue-shift
    0, 0, 0, // padding
];

/// Converts a wire pixel (or CPIXEL) starting at `bytes[0]` to opaque RGBA.
#[inline]
pub(super) fn to_rgba(bytes: &[u8]) -> [u8; 4] {
    [bytes[2], bytes[1], bytes[0], 255]
}

/// Converts a buffer of wire pixels to RGBA in place.
pub(super) fn convert_in_place(buffer: &mut [u8]) {
    for pixel in buffer.chunks_exact_mut(BYTES_PER_PIXEL) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
}

/// Returns the colour of `rgba` if every pixel in it has that colour.
pub(super) fn uniform_colour(rgba: &[u8]) -> Option<[u8; 4]> {
    let first: [u8; 4] = rgba.get(..4)?.try_into().ok()?;
    rgba.chunks_exact(4)
        .all(|pixel| pixel == first)
        .then_some(first)
}

/// Fills a rectangle of an RGBA buffer that is `stride` pixels wide.
pub(super) fn fill(
    buffer: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    colour: [u8; 4],
) {
    for row in y..y + height {
        let start = (row * stride + x) * 4;
        for pixel in buffer[start..start + width * 4].chunks_exact_mut(4) {
            pixel.copy_from_slice(&colour);
        }
    }
}

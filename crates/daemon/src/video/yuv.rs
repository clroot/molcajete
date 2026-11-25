//! RGBA → YUV 4:2:0 conversion.
//!
//! All backends use BT.709 coefficients with limited ("TV", 16..235) range
//! and signal exactly that in the bitstream (colour primaries, transfer and
//! matrix BT.709, `video_full_range_flag = 0`), so browsers convert back
//! with the matching matrix. Chroma is the average of each 2×2 block.

/// Memory layout of the chroma samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChromaLayout {
    /// I420 / `yuv420p`: a U plane followed by a V plane.
    Planar,
    /// NV12: one plane of interleaved U/V pairs.
    SemiPlanar,
}

impl ChromaLayout {
    /// The matching ffmpeg `-pix_fmt` name.
    pub(crate) fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::Planar => "yuv420p",
            Self::SemiPlanar => "nv12",
        }
    }
}

// Fixed-point (×2^16) BT.709 coefficients, limited range: luma scaled by
// 219/255, chroma by 224/255. Each chroma row sums to zero so that grey
// maps exactly to 128 (checked by `coefficients_match_bt709`).
const Y_R: i32 = 11_966;
const Y_G: i32 = 40_254;
const Y_B: i32 = 4_064;
const U_R: i32 = -6_596;
const U_G: i32 = -22_188;
const U_B: i32 = 28_784;
const V_R: i32 = 28_784;
const V_G: i32 = -26_145;
const V_B: i32 = -2_639;

const Y_OFFSET: i32 = (16 << 16) + (1 << 15);
/// Chroma is computed from the sum of four pixels: ×2^18 plus rounding.
const C_OFFSET: i32 = (128 << 18) + (1 << 17);

/// Size in bytes of a 4:2:0 frame (both layouts).
pub(crate) fn frame_len(width: usize, height: usize) -> usize {
    width * height + 2 * (width / 2) * (height / 2)
}

#[inline(always)]
fn luma(px: &[u8; 4]) -> u8 {
    let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
    ((Y_R * r + Y_G * g + Y_B * b + Y_OFFSET) >> 16) as u8
}

/// (Cb, Cr) of a 2×2 block from the sums of its four pixels' channels.
#[inline(always)]
fn chroma(r: i32, g: i32, b: i32) -> (u8, u8) {
    let u = (U_R * r + U_G * g + U_B * b + C_OFFSET) >> 18;
    let v = (V_R * r + V_G * g + V_B * b + C_OFFSET) >> 18;
    (u as u8, v as u8)
}

/// One plane of a destination picture: rows of `stride` bytes.
pub(crate) struct Plane<'a> {
    pub(crate) data: &'a mut [u8],
    pub(crate) stride: usize,
}

impl Plane<'_> {
    /// The first `len` bytes of row `y`.
    #[inline(always)]
    fn row(&mut self, y: usize, len: usize) -> &mut [u8] {
        &mut self.data[y * self.stride..][..len]
    }
}

/// Where a 4:2:0 picture goes.
pub(crate) enum Planes<'a> {
    /// Separate U and V planes (I420).
    Planar {
        y: Plane<'a>,
        u: Plane<'a>,
        v: Plane<'a>,
    },
    /// One plane of interleaved U/V pairs (NV12).
    SemiPlanar { y: Plane<'a>, uv: Plane<'a> },
}

/// The chroma row produced by one pair of pixel rows.
enum ChromaRow<'a> {
    Planar(&'a mut [u8], &'a mut [u8]),
    SemiPlanar(&'a mut [u8]),
}

/// Converts the pixels of a row pair from `from` (even) on, one 2×2 block
/// at a time.
fn row_pair_scalar(
    top: &[[u8; 4]],
    bottom: &[[u8; 4]],
    y_top: &mut [u8],
    y_bottom: &mut [u8],
    chroma_row: &mut ChromaRow<'_>,
    from: usize,
) {
    for x in (from..y_top.len()).step_by(2) {
        let (t0, t1, b0, b1) = (&top[x], &top[x + 1], &bottom[x], &bottom[x + 1]);
        y_top[x] = luma(t0);
        y_top[x + 1] = luma(t1);
        y_bottom[x] = luma(b0);
        y_bottom[x + 1] = luma(b1);
        let sum = |c: usize| t0[c] as i32 + t1[c] as i32 + b0[c] as i32 + b1[c] as i32;
        let (u, v) = chroma(sum(0), sum(1), sum(2));
        match chroma_row {
            ChromaRow::Planar(u_row, v_row) => {
                u_row[x / 2] = u;
                v_row[x / 2] = v;
            }
            ChromaRow::SemiPlanar(uv_row) => {
                uv_row[x] = u;
                uv_row[x + 1] = v;
            }
        }
    }
}

/// The same conversion with NEON, 16 pixels of each row at a time, in the
/// same integer arithmetic (so with exactly the same results). Returns how
/// many pixels it converted; the rest is left to [`row_pair_scalar`].
///
/// Chroma is computed in wrapping `u32` arithmetic with the magnitudes of
/// the negative coefficients subtracted: the true sum is never negative
/// (the offset of 128 outweighs the largest negative part), so the result
/// and its logical shift are those of the `i32` computation.
#[cfg(target_arch = "aarch64")]
fn row_pair_neon(
    top: &[[u8; 4]],
    bottom: &[[u8; 4]],
    y_top: &mut [u8],
    y_bottom: &mut [u8],
    chroma_row: &mut ChromaRow<'_>,
) -> usize {
    use std::arch::aarch64::*;

    /// Luma of 16 pixels given as channel vectors.
    #[inline(always)]
    fn luma16(r: uint8x16_t, g: uint8x16_t, b: uint8x16_t) -> uint8x16_t {
        // SAFETY: NEON is always present on aarch64; no memory access.
        unsafe {
            let offset = vdupq_n_u32(Y_OFFSET as u32);
            let half = |r: uint16x8_t, g: uint16x8_t, b: uint16x8_t| {
                let lo = vmlal_n_u16(offset, vget_low_u16(r), Y_R as u16);
                let lo = vmlal_n_u16(lo, vget_low_u16(g), Y_G as u16);
                let lo = vmlal_n_u16(lo, vget_low_u16(b), Y_B as u16);
                let hi = vmlal_high_n_u16(offset, r, Y_R as u16);
                let hi = vmlal_high_n_u16(hi, g, Y_G as u16);
                let hi = vmlal_high_n_u16(hi, b, Y_B as u16);
                vmovn_u16(vcombine_u16(vshrn_n_u32::<16>(lo), vshrn_n_u32::<16>(hi)))
            };
            let low = half(
                vmovl_u8(vget_low_u8(r)),
                vmovl_u8(vget_low_u8(g)),
                vmovl_u8(vget_low_u8(b)),
            );
            let high = half(vmovl_high_u8(r), vmovl_high_u8(g), vmovl_high_u8(b));
            vcombine_u8(low, high)
        }
    }

    /// One chroma component of 8 blocks from their channel sums;
    /// `coefficients` are (R, G, B) as (magnitude, negative).
    #[inline(always)]
    fn chroma8(
        r: uint16x8_t,
        g: uint16x8_t,
        b: uint16x8_t,
        coefficients: [(u16, bool); 3],
    ) -> uint8x8_t {
        // SAFETY: as in `luma16`.
        unsafe {
            let offset = vdupq_n_u32(C_OFFSET as u32);
            let mut lo = offset;
            let mut hi = offset;
            for (channel, (k, negative)) in [r, g, b].into_iter().zip(coefficients) {
                if negative {
                    lo = vmlsl_n_u16(lo, vget_low_u16(channel), k);
                    hi = vmlsl_high_n_u16(hi, channel, k);
                } else {
                    lo = vmlal_n_u16(lo, vget_low_u16(channel), k);
                    hi = vmlal_high_n_u16(hi, channel, k);
                }
            }
            let lo = vmovn_u32(vshrq_n_u32::<18>(lo));
            let hi = vmovn_u32(vshrq_n_u32::<18>(hi));
            vmovn_u16(vcombine_u16(lo, hi))
        }
    }

    const fn coefficient(k: i32) -> (u16, bool) {
        (k.unsigned_abs() as u16, k < 0)
    }
    const U: [(u16, bool); 3] = [coefficient(U_R), coefficient(U_G), coefficient(U_B)];
    const V: [(u16, bool); 3] = [coefficient(V_R), coefficient(V_G), coefficient(V_B)];

    let n = y_top.len() / 16 * 16;
    assert!(top.len() >= n && bottom.len() >= n && y_bottom.len() >= n);
    match chroma_row {
        ChromaRow::Planar(u, v) => assert!(u.len() >= n / 2 && v.len() >= n / 2),
        ChromaRow::SemiPlanar(uv) => assert!(uv.len() >= n),
    }
    for x in (0..n).step_by(16) {
        // SAFETY: NEON is always present on aarch64, and every access is
        // within the lengths checked above.
        unsafe {
            let t = vld4q_u8(top.as_ptr().add(x).cast());
            let b = vld4q_u8(bottom.as_ptr().add(x).cast());
            vst1q_u8(y_top.as_mut_ptr().add(x), luma16(t.0, t.1, t.2));
            vst1q_u8(y_bottom.as_mut_ptr().add(x), luma16(b.0, b.1, b.2));
            // Sums of horizontal pairs, then of the two rows.
            let r = vpadalq_u8(vpaddlq_u8(t.0), b.0);
            let g = vpadalq_u8(vpaddlq_u8(t.1), b.1);
            let bl = vpadalq_u8(vpaddlq_u8(t.2), b.2);
            let u = chroma8(r, g, bl, U);
            let v = chroma8(r, g, bl, V);
            match chroma_row {
                ChromaRow::Planar(u_row, v_row) => {
                    vst1_u8(u_row.as_mut_ptr().add(x / 2), u);
                    vst1_u8(v_row.as_mut_ptr().add(x / 2), v);
                }
                ChromaRow::SemiPlanar(uv_row) => {
                    vst2_u8(uv_row.as_mut_ptr().add(x), uint8x8x2_t(u, v));
                }
            }
        }
    }
    n
}

/// Converts a pair of pixel rows.
fn row_pair(
    top: &[[u8; 4]],
    bottom: &[[u8; 4]],
    y_top: &mut [u8],
    y_bottom: &mut [u8],
    mut chroma_row: ChromaRow<'_>,
) {
    #[cfg(target_arch = "aarch64")]
    let done = row_pair_neon(top, bottom, y_top, y_bottom, &mut chroma_row);
    #[cfg(not(target_arch = "aarch64"))]
    let done = 0;
    row_pair_scalar(top, bottom, y_top, y_bottom, &mut chroma_row, done);
}

/// Converts an RGBA picture of `width` × `height` pixels (rows of
/// `rgba_stride` bytes, top row first, alpha ignored) into `planes`, whose
/// rows must be long enough. `width` and `height` must be even.
pub(crate) fn convert(
    rgba: &[u8],
    rgba_stride: usize,
    width: usize,
    height: usize,
    planes: Planes<'_>,
) {
    assert!(
        width.is_multiple_of(2) && height.is_multiple_of(2),
        "4:2:0 needs even dimensions"
    );
    assert!(rgba_stride >= width * 4, "RGBA stride");
    if width == 0 || height == 0 {
        return;
    }
    assert!(
        rgba.len() >= (height - 1) * rgba_stride + width * 4,
        "RGBA frame size"
    );
    enum Chroma<'a> {
        Planar(Plane<'a>, Plane<'a>),
        SemiPlanar(Plane<'a>),
    }
    let (y, mut chroma) = match planes {
        Planes::Planar { y, u, v } => (y, Chroma::Planar(u, v)),
        Planes::SemiPlanar { y, uv } => (y, Chroma::SemiPlanar(uv)),
    };
    for row in 0..height / 2 {
        let top = rgba[2 * row * rgba_stride..][..width * 4]
            .as_chunks::<4>()
            .0;
        let bottom = rgba[(2 * row + 1) * rgba_stride..][..width * 4]
            .as_chunks::<4>()
            .0;
        // Both luma rows at once: split the plane at the bottom one.
        let (upper, lower) = y.data.split_at_mut((2 * row + 1) * y.stride);
        let y_top = &mut upper[2 * row * y.stride..][..width];
        let y_bottom = &mut lower[..width];
        let chroma_row = match &mut chroma {
            Chroma::Planar(u, v) => ChromaRow::Planar(u.row(row, width / 2), v.row(row, width / 2)),
            Chroma::SemiPlanar(uv) => ChromaRow::SemiPlanar(uv.row(row, width)),
        };
        row_pair(top, bottom, y_top, y_bottom, chroma_row);
    }
}

/// Converts an RGBA frame (top row first, alpha ignored) into `out`, which
/// is resized to [`frame_len`]. `width` and `height` must be even.
pub(crate) fn rgba_to_yuv420(
    rgba: &[u8],
    width: usize,
    height: usize,
    layout: ChromaLayout,
    out: &mut Vec<u8>,
) {
    #[cfg(test)]
    if reference::LEGACY.get() {
        return reference::rgba_to_yuv420(rgba, width, height, layout, out);
    }
    assert_eq!(rgba.len(), width * height * 4, "RGBA frame size");
    let luma_len = width * height;
    out.resize(frame_len(width, height), 0);
    let (y, chroma) = out.split_at_mut(luma_len);
    let y = Plane {
        data: y,
        stride: width,
    };
    let planes = match layout {
        ChromaLayout::Planar => {
            let (u, v) = chroma.split_at_mut(luma_len / 4);
            Planes::Planar {
                y,
                u: Plane {
                    data: u,
                    stride: width / 2,
                },
                v: Plane {
                    data: v,
                    stride: width / 2,
                },
            }
        }
        ChromaLayout::SemiPlanar => Planes::SemiPlanar {
            y,
            uv: Plane {
                data: chroma,
                stride: width,
            },
        },
    };
    convert(rgba, width * 4, width, height, planes);
}

/// The conversion as it was before GH-37, block by block: the reference
/// for the equivalence tests and the "before" of the benchmarks.
#[cfg(test)]
pub(crate) mod reference {
    use super::*;

    thread_local! {
        /// Prepare frames on this thread as before GH-37 (this conversion,
        /// and the copies that went with it), for the benchmarks.
        pub(crate) static LEGACY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[inline(always)]
    fn luma(px: &[u8]) -> u8 {
        let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
        ((Y_R * r + Y_G * g + Y_B * b + Y_OFFSET) >> 16) as u8
    }

    /// Converts one 2×2 block: writes its four luma samples, returns (Cb, Cr).
    #[inline(always)]
    fn block(top: &[u8], bottom: &[u8], y_top: &mut [u8], y_bottom: &mut [u8]) -> (u8, u8) {
        y_top[0] = luma(&top[0..4]);
        y_top[1] = luma(&top[4..8]);
        y_bottom[0] = luma(&bottom[0..4]);
        y_bottom[1] = luma(&bottom[4..8]);
        let r = top[0] as i32 + top[4] as i32 + bottom[0] as i32 + bottom[4] as i32;
        let g = top[1] as i32 + top[5] as i32 + bottom[1] as i32 + bottom[5] as i32;
        let b = top[2] as i32 + top[6] as i32 + bottom[2] as i32 + bottom[6] as i32;
        let u = (U_R * r + U_G * g + U_B * b + C_OFFSET) >> 18;
        let v = (V_R * r + V_G * g + V_B * b + C_OFFSET) >> 18;
        (u as u8, v as u8)
    }

    pub(crate) fn rgba_to_yuv420(
        rgba: &[u8],
        width: usize,
        height: usize,
        layout: ChromaLayout,
        out: &mut Vec<u8>,
    ) {
        assert!(
            width.is_multiple_of(2) && height.is_multiple_of(2),
            "4:2:0 needs even dimensions"
        );
        assert_eq!(rgba.len(), width * height * 4, "RGBA frame size");
        let luma_len = width * height;
        out.resize(frame_len(width, height), 0);
        if luma_len == 0 {
            return;
        }
        let (y_plane, chroma) = out.split_at_mut(luma_len);
        let row_pairs = rgba
            .chunks_exact(width * 8)
            .zip(y_plane.chunks_exact_mut(width * 2));
        match layout {
            ChromaLayout::Planar => {
                let (u_plane, v_plane) = chroma.split_at_mut(luma_len / 4);
                let chroma_rows = u_plane
                    .chunks_exact_mut(width / 2)
                    .zip(v_plane.chunks_exact_mut(width / 2));
                for ((src, dst), (u_row, v_row)) in row_pairs.zip(chroma_rows) {
                    let (top, bottom) = src.split_at(width * 4);
                    let (y_top, y_bottom) = dst.split_at_mut(width);
                    let blocks = top
                        .chunks_exact(8)
                        .zip(bottom.chunks_exact(8))
                        .zip(y_top.chunks_exact_mut(2).zip(y_bottom.chunks_exact_mut(2)));
                    for (((t, b), (yt, yb)), (u, v)) in
                        blocks.zip(u_row.iter_mut().zip(v_row.iter_mut()))
                    {
                        (*u, *v) = block(t, b, yt, yb);
                    }
                }
            }
            ChromaLayout::SemiPlanar => {
                for ((src, dst), uv_row) in row_pairs.zip(chroma.chunks_exact_mut(width)) {
                    let (top, bottom) = src.split_at(width * 4);
                    let (y_top, y_bottom) = dst.split_at_mut(width);
                    let blocks = top
                        .chunks_exact(8)
                        .zip(bottom.chunks_exact(8))
                        .zip(y_top.chunks_exact_mut(2).zip(y_bottom.chunks_exact_mut(2)));
                    for (((t, b), (yt, yb)), uv) in blocks.zip(uv_row.chunks_exact_mut(2)) {
                        (uv[0], uv[1]) = block(t, b, yt, yb);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: usize, height: usize, rgb: [u8; 3]) -> Vec<u8> {
        [rgb[0], rgb[1], rgb[2], 255].repeat(width * height)
    }

    /// Reference conversion in floating point.
    fn reference(rgb: [u8; 3]) -> (f64, f64, f64) {
        let [r, g, b] = rgb.map(|c| c as f64 / 255.0);
        let (kr, kb) = (0.2126, 0.0722);
        let y = kr * r + (1.0 - kr - kb) * g + kb * b;
        let cb = (b - y) / (2.0 * (1.0 - kb));
        let cr = (r - y) / (2.0 * (1.0 - kr));
        (16.0 + 219.0 * y, 128.0 + 224.0 * cb, 128.0 + 224.0 * cr)
    }

    /// `cargo test --release -p molcajete-daemon yuv -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark"]
    fn conversion_speed() {
        for (w, h) in [(1280, 720), (1920, 1080), (1344, 2176)] {
            let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 255) as u8).collect();
            let mut out = Vec::new();
            for layout in [ChromaLayout::Planar, ChromaLayout::SemiPlanar] {
                let per_frame = |convert: fn(&[u8], usize, usize, ChromaLayout, &mut Vec<u8>),
                                 out: &mut Vec<u8>| {
                    let start = std::time::Instant::now();
                    for _ in 0..50 {
                        convert(std::hint::black_box(&rgba), w, h, layout, out);
                    }
                    start.elapsed() / 50
                };
                let before = per_frame(reference::rgba_to_yuv420, &mut out);
                let after = per_frame(rgba_to_yuv420, &mut out);
                eprintln!(
                    "{layout:?} {w}x{h}: before GH-37 {:.3} ms, now {:.3} ms per frame",
                    before.as_secs_f64() * 1000.0,
                    after.as_secs_f64() * 1000.0
                );
            }
        }
    }

    /// Noise and smooth gradients (all channel values, including extremes).
    fn test_picture(w: usize, h: usize, seed: u32) -> Vec<u8> {
        let mut state = seed.wrapping_mul(2_654_435_761) | 1;
        (0..w * h)
            .flat_map(|i| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let (x, y) = (i % w, i / w);
                if (x / 7 + y / 5) % 3 == 0 {
                    state.to_le_bytes()
                } else {
                    [(x * 255 / w) as u8, (y * 255 / h) as u8, (x ^ y) as u8, 255]
                }
            })
            .collect()
    }

    #[test]
    fn matches_the_previous_conversion_exactly() {
        // Widths around the chunk size and its multiples, odd chunk counts.
        for (w, h) in [
            (2, 2),
            (6, 4),
            (62, 2),
            (64, 6),
            (66, 4),
            (130, 10),
            (202, 130),
        ] {
            for seed in 0..3 {
                let rgba = test_picture(w, h, seed);
                for layout in [ChromaLayout::Planar, ChromaLayout::SemiPlanar] {
                    let (mut old, mut new) = (Vec::new(), Vec::new());
                    reference::rgba_to_yuv420(&rgba, w, h, layout, &mut old);
                    rgba_to_yuv420(&rgba, w, h, layout, &mut new);
                    assert_eq!(old, new, "{w}x{h} {layout:?} seed {seed}");
                }
            }
        }
        // Every value of every channel, against flat colours.
        for value in 0..=255u8 {
            for channel in 0..3 {
                let mut px = [0, 0, 0, 255];
                px[channel] = value;
                let rgba = px.repeat(4 * 2);
                let (mut old, mut new) = (Vec::new(), Vec::new());
                reference::rgba_to_yuv420(&rgba, 4, 2, ChromaLayout::SemiPlanar, &mut old);
                rgba_to_yuv420(&rgba, 4, 2, ChromaLayout::SemiPlanar, &mut new);
                assert_eq!(old, new, "channel {channel} = {value}");
            }
        }
    }

    /// The portable path (all of a row where there is no SIMD path, the
    /// last pixels of a row where there is) on whole rows.
    #[test]
    fn scalar_path_matches_the_previous_conversion() {
        let (w, h) = (66, 6);
        let rgba = test_picture(w, h, 3);
        let mut expected = Vec::new();
        reference::rgba_to_yuv420(&rgba, w, h, ChromaLayout::Planar, &mut expected);
        let mut out = vec![0; frame_len(w, h)];
        let (y, chroma) = out.split_at_mut(w * h);
        let (u, v) = chroma.split_at_mut(w * h / 4);
        let pixels = rgba.as_chunks::<4>().0;
        for row in 0..h / 2 {
            let (upper, lower) = y.split_at_mut((2 * row + 1) * w);
            row_pair_scalar(
                &pixels[2 * row * w..][..w],
                &pixels[(2 * row + 1) * w..][..w],
                &mut upper[2 * row * w..],
                &mut lower[..w],
                &mut ChromaRow::Planar(
                    &mut u[row * w / 2..][..w / 2],
                    &mut v[row * w / 2..][..w / 2],
                ),
                0,
            );
        }
        assert_eq!(out, expected);
    }

    #[test]
    fn converts_from_and_into_strided_buffers() {
        let (w, h) = (70, 6);
        let rgba = test_picture(w, h, 7);
        let mut expected = Vec::new();
        rgba_to_yuv420(&rgba, w, h, ChromaLayout::SemiPlanar, &mut expected);

        // The picture inside a wider buffer, into planes with padding.
        let (src_stride, y_stride, uv_stride) = (w * 4 + 36, w + 10, w + 6);
        let mut src = vec![0xaa; src_stride * h];
        for (row, line) in rgba.chunks_exact(w * 4).enumerate() {
            src[row * src_stride..][..w * 4].copy_from_slice(line);
        }
        let mut y = vec![0x55; y_stride * h];
        let mut uv = vec![0x55; uv_stride * h / 2];
        convert(
            &src,
            src_stride,
            w,
            h,
            Planes::SemiPlanar {
                y: Plane {
                    data: &mut y,
                    stride: y_stride,
                },
                uv: Plane {
                    data: &mut uv,
                    stride: uv_stride,
                },
            },
        );
        for row in 0..h {
            assert_eq!(y[row * y_stride..][..w], expected[row * w..][..w]);
            assert!(
                y[row * y_stride + w..][..y_stride - w]
                    .iter()
                    .all(|&b| b == 0x55)
            );
        }
        for row in 0..h / 2 {
            assert_eq!(uv[row * uv_stride..][..w], expected[w * h + row * w..][..w]);
        }
    }

    #[test]
    fn coefficients_match_bt709() {
        let scale = 65536.0;
        let ys = 219.0 / 255.0;
        let cs = 224.0 / 255.0;
        let close = |fixed: i32, exact: f64| (fixed as f64 - exact * scale).abs() <= 1.0;
        assert!(close(Y_R, 0.2126 * ys));
        assert!(close(Y_G, 0.7152 * ys));
        assert!(close(Y_B, 0.0722 * ys));
        assert!(close(U_R, -0.2126 / 1.8556 * cs));
        assert!(close(U_G, -0.7152 / 1.8556 * cs));
        assert!(close(U_B, 0.5 * cs));
        assert!(close(V_R, 0.5 * cs));
        assert!(close(V_G, -0.7152 / 1.5748 * cs));
        assert!(close(V_B, -0.0722 / 1.5748 * cs));
        assert_eq!(U_R + U_G + U_B, 0);
        assert_eq!(V_R + V_G + V_B, 0);
    }

    #[test]
    fn solid_colours_convert_within_rounding() {
        let colours = [
            [0, 0, 0],
            [255, 255, 255],
            [128, 128, 128],
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [12, 200, 77],
        ];
        for rgb in colours {
            let mut out = Vec::new();
            rgba_to_yuv420(&solid(4, 2, rgb), 4, 2, ChromaLayout::Planar, &mut out);
            assert_eq!(out.len(), 8 + 2 + 2);
            let (y, u, v) = reference(rgb);
            for &sample in &out[..8] {
                assert!(
                    (sample as f64 - y).abs() <= 0.51,
                    "{rgb:?}: Y {sample} vs {y}"
                );
            }
            assert!(
                (out[8] as f64 - u).abs() <= 0.51,
                "{rgb:?}: U {} vs {u}",
                out[8]
            );
            assert!(
                (out[10] as f64 - v).abs() <= 0.51,
                "{rgb:?}: V {} vs {v}",
                out[10]
            );
        }
    }

    #[test]
    fn limited_range_extremes() {
        let mut out = Vec::new();
        rgba_to_yuv420(
            &solid(2, 2, [0, 0, 0]),
            2,
            2,
            ChromaLayout::Planar,
            &mut out,
        );
        assert_eq!(out, [16, 16, 16, 16, 128, 128]);
        rgba_to_yuv420(
            &solid(2, 2, [255, 255, 255]),
            2,
            2,
            ChromaLayout::Planar,
            &mut out,
        );
        assert_eq!(out, [235, 235, 235, 235, 128, 128]);
    }

    #[test]
    fn chroma_is_the_block_average_and_layouts_agree() {
        let (w, h) = (6, 4);
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i * 37 % 251) as u8).collect();
        let mut planar = Vec::new();
        let mut nv12 = Vec::new();
        rgba_to_yuv420(&rgba, w, h, ChromaLayout::Planar, &mut planar);
        rgba_to_yuv420(&rgba, w, h, ChromaLayout::SemiPlanar, &mut nv12);
        assert_eq!(planar.len(), frame_len(w, h));
        assert_eq!(planar[..w * h], nv12[..w * h]);
        let chroma = (w / 2) * (h / 2);
        for i in 0..chroma {
            assert_eq!(planar[w * h + i], nv12[w * h + 2 * i], "U {i}");
            assert_eq!(planar[w * h + chroma + i], nv12[w * h + 2 * i + 1], "V {i}");
        }
        // Block (1, 1) covers pixels (2..4, 2..4).
        let px = |x: usize, y: usize| {
            let o = (y * w + x) * 4;
            [rgba[o], rgba[o + 1], rgba[o + 2]].map(|c| c as f64)
        };
        let mut sum = [0.0; 3];
        for (x, y) in [(2, 2), (3, 2), (2, 3), (3, 3)] {
            for (s, c) in sum.iter_mut().zip(px(x, y)) {
                *s += c / 4.0;
            }
        }
        let avg = sum.map(|c| c.round() as u8);
        let (_, u, v) = reference(avg);
        let index = w / 2 + 1;
        assert!((planar[w * h + index] as f64 - u).abs() <= 1.0);
        assert!((planar[w * h + chroma + index] as f64 - v).abs() <= 1.0);
    }
}

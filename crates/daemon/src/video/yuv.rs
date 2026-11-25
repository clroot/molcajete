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

/// Converts an RGBA frame (top row first, alpha ignored) into `out`, which
/// is resized to [`frame_len`]. `width` and `height` must be even.
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
        let (w, h) = (1280, 720);
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 255) as u8).collect();
        let mut out = Vec::new();
        for layout in [ChromaLayout::Planar, ChromaLayout::SemiPlanar] {
            let start = std::time::Instant::now();
            for _ in 0..100 {
                rgba_to_yuv420(std::hint::black_box(&rgba), w, h, layout, &mut out);
            }
            eprintln!("{layout:?} 1280x720: {:?} per frame", start.elapsed() / 100);
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

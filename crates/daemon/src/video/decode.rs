//! The H.264 decoder of the RDP graphics pipeline ([MS-RDPEGFX] AVC420 and
//! AVC444), which IronRDP leaves to its user.
//!
//! - macOS: VideoToolbox (see `videotoolbox/decode.rs`);
//! - elsewhere: OpenH264 with the cargo feature `openh264`, through
//!   IronRDP's own wrapper;
//! - otherwise none, and the pipeline is offered without H.264.

use ironrdp_egfx::decode::H264Decoder;

/// A decoder for one pipeline, with the name it is logged under.
pub struct PipelineDecoder {
    pub name: &'static str,
    pub decoder: Box<dyn H264Decoder>,
}

/// The decoder this build has, if any.
pub fn h264_decoder() -> Option<PipelineDecoder> {
    #[cfg(target_os = "macos")]
    {
        Some(PipelineDecoder {
            name: "VideoToolbox",
            decoder: Box::new(super::videotoolbox::VideoToolboxDecoder::new()),
        })
    }
    #[cfg(all(not(target_os = "macos"), feature = "openh264"))]
    {
        openh264()
    }
    #[cfg(all(not(target_os = "macos"), not(feature = "openh264")))]
    {
        None
    }
}

#[cfg(all(feature = "openh264", any(not(target_os = "macos"), test)))]
fn openh264() -> Option<PipelineDecoder> {
    match ironrdp_egfx::decode::OpenH264Decoder::new() {
        Ok(decoder) => Some(PipelineDecoder {
            name: "OpenH264",
            decoder: Box::new(decoder),
        }),
        Err(err) => {
            tracing::warn!(error = %err, "OpenH264 decoder unavailable");
            None
        }
    }
}

#[cfg(all(test, feature = "openh264"))]
mod tests {
    use super::*;
    use crate::video::builtin::OpenH264Encoder;
    use crate::video::yuv::{self, ChromaLayout};
    use crate::video::{EncoderConfig, VideoCodec, VideoEncoder};

    #[test]
    fn openh264_decodes_what_the_built_in_encoder_produces() {
        let (width, height) = (320u32, 240u32);
        let mut encoder = OpenH264Encoder::new(&EncoderConfig {
            codec: VideoCodec::H264,
            width,
            height,
            fps: 30,
            bitrate_kbps: 2000,
        })
        .expect("OpenH264 encoder");
        let mut decoder = openh264().expect("OpenH264 decoder");
        assert_eq!(decoder.name, "OpenH264");
        assert!(decoder.decoder.supports_yuv420());
        for index in 0..3 {
            let rgba: Vec<u8> = (0..width * height)
                .flat_map(|i| {
                    let (x, y) = (i % width, i / width);
                    [((x + 8 * index) / 2) as u8, (y / 2) as u8, 90, 255]
                })
                .collect();
            let frame = encoder.encode(&rgba, false).expect("encode");
            let picture = decoder.decoder.decode_yuv420(&frame.data).expect("decode");
            assert_eq!(
                (u32::from(picture.width()), u32::from(picture.height())),
                (width, height)
            );
            // The luma that went into the encoder comes back out.
            let (w, h) = (width as usize, height as usize);
            let mut expected = Vec::new();
            yuv::rgba_to_yuv420(&rgba, w, h, ChromaLayout::Planar, &mut expected);
            let total: u64 = (0..h)
                .flat_map(|row| {
                    let decoded = picture.luma_row(row).expect("luma row");
                    decoded
                        .iter()
                        .zip(&expected[row * w..(row + 1) * w])
                        .map(|(a, b)| u64::from(a.abs_diff(*b)))
                })
                .sum();
            let error = total as f64 / (w * h) as f64;
            assert!(error < 3.0, "frame {index}: luma differs by {error:.2}");
        }
    }
}

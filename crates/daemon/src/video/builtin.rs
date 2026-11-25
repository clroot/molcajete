//! Built-in H.264 encoder: OpenH264, compiled into guacd.
//!
//! Real-time camera settings, Constrained Baseline, bitrate rate control
//! without frame skipping, a single slice and no periodic IDR frames. Every
//! IDR frame carries SPS and PPS.

use openh264::OpenH264API;
use openh264::Timestamp;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig as OpenH264Config, FrameRate, IntraFramePeriod,
    Profile, RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::YUVSlices;

use super::bitstream;
use super::yuv::{self, ChromaLayout};
use super::{EncodedFrame, EncoderConfig, VideoCodec, VideoEncoder, VideoError, check_frame};

pub(crate) struct OpenH264Encoder {
    encoder: Encoder,
    config: EncoderConfig,
    /// I420 conversion buffer.
    yuv: Vec<u8>,
    frames: u64,
}

impl OpenH264Encoder {
    pub(crate) fn new(config: &EncoderConfig) -> Result<Self, VideoError> {
        let settings = OpenH264Config::new()
            .usage_type(UsageType::CameraVideoRealTime)
            .profile(Profile::Baseline)
            .complexity(Complexity::Low)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(config.bitrate_kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(config.fps as f32))
            .skip_frames(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            .num_threads(1)
            .vui(VuiConfig::bt709());
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), settings)
            .map_err(|e| VideoError::Encoder(format!("OpenH264: {e}")))?;
        Ok(Self {
            encoder,
            config: config.clone(),
            yuv: Vec::new(),
            frames: 0,
        })
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn encode(&mut self, rgba: &[u8], force_key: bool) -> Result<EncodedFrame, VideoError> {
        check_frame(&self.config, rgba)?;
        let (width, height) = (self.config.width as usize, self.config.height as usize);
        yuv::rgba_to_yuv420(rgba, width, height, ChromaLayout::Planar, &mut self.yuv);
        let (y, chroma) = self.yuv.split_at(width * height);
        let (u, v) = chroma.split_at(chroma.len() / 2);
        let source = YUVSlices::new((y, u, v), (width, height), (width, width / 2, width / 2));

        // The first frame is an IDR anyway (and the encoder is only set up
        // by its first `encode`).
        if force_key && self.frames > 0 {
            self.encoder.force_intra_frame();
        }
        let timestamp = Timestamp::from_millis(self.frames * 1000 / u64::from(self.config.fps));
        let data = self
            .encoder
            .encode_at(&source, timestamp)
            .map_err(|e| VideoError::Encoder(format!("OpenH264: {e}")))?
            .to_vec();
        self.frames += 1;
        if data.is_empty() {
            return Err(VideoError::Encoder("OpenH264 skipped a frame".into()));
        }
        let key = bitstream::is_key_frame(VideoCodec::H264, &data);
        Ok(EncodedFrame { data, key })
    }
}

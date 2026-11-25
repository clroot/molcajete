//! H.264 decoding with VideoToolbox, for the RDP graphics pipeline.
//!
//! The pipeline hands over one access unit at a time and wants its picture
//! back before it goes on (AVC444 pairs each picture with the sub-stream it
//! came from), so frames are decoded synchronously: without the asynchronous
//! flag `VTDecompressionSessionDecodeFrame` returns after the output callback
//! has run, and VideoToolbox emits pictures in decoding order.
//!
//! The session is created from the stream's SPS and PPS and replaced when
//! they change. No output format is requested, so VideoToolbox returns the
//! decoder's own NV12 buffer with the samples as they are in the stream;
//! asking for a format of the other range would rescale them, and the
//! pipeline applies the conversion of [MS-RDPEGFX] to the decoded samples
//! itself, whatever range the stream signals.

use std::ffi::c_void;
use std::ptr;
use std::sync::{Mutex, PoisonError};

use ironrdp_egfx::decode::{DecodedFrame, DecoderError, DecoderResult, H264Decoder, Yuv420Frame};

use super::{
    Boolean, CFAllocatorRef, CFDictionaryRef, CFRelease, CMBlockBufferRef, CMFormatDescriptionRef,
    CMSampleBufferRef, CMTime, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferRef,
    CVPixelBufferUnlockBaseAddress, OSStatus, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use crate::video::bitstream;

type VTDecompressionSessionRef = *mut c_void;

const kCVPixelFormatType_420YpCbCr8BiPlanarFullRange: u32 = u32::from_be_bytes(*b"420f");
const kCVPixelBufferLock_ReadOnly: u64 = 1;

const NAL_SLICE: u8 = 1;
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
/// Bytes of the length in front of each NAL unit given to VideoToolbox.
const LENGTH_PREFIX: usize = 4;

type OutputCallback = extern "C" fn(
    refcon: *mut c_void,
    source_frame_refcon: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    image_buffer: CVPixelBufferRef,
    presentation_time: CMTime,
    duration: CMTime,
);

#[repr(C)]
struct OutputCallbackRecord {
    callback: Option<OutputCallback>,
    refcon: *mut c_void,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    /// Allocator that never frees: the block buffer borrows our memory.
    static kCFAllocatorNull: CFAllocatorRef;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferGetPixelFormatType(buffer: CVPixelBufferRef) -> u32;
    fn CVPixelBufferGetWidth(buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetWidthOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CFAllocatorRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        data_buffer: CMBlockBufferRef,
        format_description: CMFormatDescriptionRef,
        num_samples: isize,
        num_sample_timing_entries: isize,
        sample_timing_array: *const c_void,
        num_sample_size_entries: isize,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        video_format_description: CMFormatDescriptionRef,
        video_decoder_specification: CFDictionaryRef,
        destination_image_buffer_attributes: CFDictionaryRef,
        output_callback: *const OutputCallbackRecord,
        session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionCanAcceptFormatDescription(
        session: VTDecompressionSessionRef,
        format_description: CMFormatDescriptionRef,
    ) -> Boolean;
    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: u32,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
}

fn check(status: OSStatus, what: &str) -> DecoderResult<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(DecoderError::msg(format!(
            "VideoToolbox {what} failed (OSStatus {status})"
        )))
    }
}

/// What the output callback hands back for the frame being decoded.
type Output = Option<DecoderResult<Yuv420Frame>>;

pub(crate) struct VideoToolboxDecoder {
    /// Null until parameter sets have been seen, and after a failure.
    session: VTDecompressionSessionRef,
    /// Describes `sps` and `pps`; null while there is no usable pair.
    format: CMFormatDescriptionRef,
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// Written by the output callback; boxed so its address stays fixed.
    output: Box<Mutex<Output>>,
    /// The access unit as length-prefixed NAL units, reused between frames.
    sample: Vec<u8>,
}

// The session is only used from the thread that owns the decoder; the
// callback only touches `output`, which is behind a mutex.
unsafe impl Send for VideoToolboxDecoder {}

impl VideoToolboxDecoder {
    pub(crate) fn new() -> Self {
        Self {
            session: ptr::null_mut(),
            format: ptr::null(),
            sps: Vec::new(),
            pps: Vec::new(),
            output: Box::new(Mutex::new(None)),
            sample: Vec::new(),
        }
    }

    fn drop_session(&mut self) {
        if !self.session.is_null() {
            unsafe {
                // Waits for pending callbacks, so `output` is no longer used.
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session.cast_const());
            }
            self.session = ptr::null_mut();
        }
    }

    fn drop_format(&mut self) {
        if !self.format.is_null() {
            unsafe { CFRelease(self.format) };
            self.format = ptr::null();
        }
    }

    /// Splits the access unit: parameter sets are remembered, slices go into
    /// `sample`. Returns whether the parameter sets changed.
    fn read_access_unit(&mut self, data: &[u8]) -> bool {
        self.sample.clear();
        let mut changed = false;
        for nal in nal_units(data) {
            let stored = match nal.first().map(|header| header & 0x1f) {
                Some(NAL_SPS) => &mut self.sps,
                Some(NAL_PPS) => &mut self.pps,
                Some(NAL_SLICE..=NAL_IDR) => {
                    // A NAL unit of 4 GiB does not fit in an RDP message.
                    self.sample
                        .extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    self.sample.extend_from_slice(nal);
                    continue;
                }
                // Delimiters and SEI carry nothing the picture needs.
                _ => continue,
            };
            if stored != nal {
                stored.clear();
                stored.extend_from_slice(nal);
                changed = true;
            }
        }
        changed
    }

    /// Makes the session match the current parameter sets.
    fn prepare_session(&mut self, parameters_changed: bool) -> DecoderResult<()> {
        if self.sps.is_empty() || self.pps.is_empty() {
            return Err(DecoderError::msg(
                "H.264 stream has not sent its parameter sets yet",
            ));
        }
        if parameters_changed || self.format.is_null() {
            self.drop_format();
            let pointers = [self.sps.as_ptr(), self.pps.as_ptr()];
            let sizes = [self.sps.len(), self.pps.len()];
            check(
                unsafe {
                    CMVideoFormatDescriptionCreateFromH264ParameterSets(
                        ptr::null(),
                        pointers.len(),
                        pointers.as_ptr(),
                        sizes.as_ptr(),
                        LENGTH_PREFIX as i32,
                        &mut self.format,
                    )
                },
                "format description",
            )?;
            // A new picture size needs a new session; anything less does not.
            if !self.session.is_null()
                && unsafe {
                    VTDecompressionSessionCanAcceptFormatDescription(self.session, self.format)
                } == 0
            {
                self.drop_session();
            }
        }
        if self.session.is_null() {
            let callback = OutputCallbackRecord {
                callback: Some(output_callback),
                refcon: ptr::from_ref::<Mutex<Output>>(&self.output)
                    .cast_mut()
                    .cast(),
            };
            check(
                unsafe {
                    VTDecompressionSessionCreate(
                        ptr::null(),
                        self.format,
                        ptr::null(),
                        ptr::null(),
                        &raw const callback,
                        &mut self.session,
                    )
                },
                "DecompressionSessionCreate",
            )?;
        }
        Ok(())
    }

    /// Decodes `sample` and waits for its picture.
    fn decode_sample(&mut self) -> DecoderResult<Yuv420Frame> {
        let mut block: CMBlockBufferRef = ptr::null_mut();
        let mut sample: CMSampleBufferRef = ptr::null_mut();
        let size = self.sample.len();
        let status = unsafe {
            check(
                CMBlockBufferCreateWithMemoryBlock(
                    ptr::null(),
                    self.sample.as_mut_ptr().cast(),
                    size,
                    kCFAllocatorNull,
                    ptr::null(),
                    0,
                    size,
                    0,
                    &mut block,
                ),
                "block buffer",
            )?;
            let created = check(
                CMSampleBufferCreateReady(
                    ptr::null(),
                    block,
                    self.format,
                    1,
                    0,
                    ptr::null(),
                    1,
                    &raw const size,
                    &mut sample,
                ),
                "sample buffer",
            );
            if let Err(err) = created {
                CFRelease(block.cast_const());
                return Err(err);
            }
            // No flags: decoded before the call returns.
            let status = VTDecompressionSessionDecodeFrame(
                self.session,
                sample,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            );
            CFRelease(sample.cast_const());
            CFRelease(block.cast_const());
            status
        };
        let output = self
            .output
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        check(status, "decoding")?;
        output.unwrap_or_else(|| Err(DecoderError::msg("VideoToolbox returned no picture")))
    }
}

impl Drop for VideoToolboxDecoder {
    fn drop(&mut self) {
        self.drop_session();
        self.drop_format();
    }
}

impl H264Decoder for VideoToolboxDecoder {
    fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame> {
        self.decode_yuv420(data).map(|picture| picture.to_rgba())
    }

    fn decode_yuv420(&mut self, data: &[u8]) -> DecoderResult<Yuv420Frame> {
        let parameters_changed = self.read_access_unit(data);
        if self.sample.is_empty() {
            return Err(DecoderError::msg("H.264 access unit without a picture"));
        }
        self.prepare_session(parameters_changed)?;
        let picture = self.decode_sample();
        if picture.is_err() {
            // A session that failed (the GPU went away, a broken stream)
            // may not recover; the next frame starts a new one.
            self.drop_session();
        }
        picture
    }

    fn supports_yuv420(&self) -> bool {
        true
    }

    fn reset(&mut self) {
        self.drop_session();
        self.drop_format();
        self.sps.clear();
        self.pps.clear();
    }
}

/// NAL units of an access unit, which the pipeline delivers as an Annex B
/// byte stream or, from some servers, with a 4-byte length in front of each.
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    if !ironrdp_egfx::pdu::is_avc_format(data) {
        return bitstream::nal_units(data).map(|(_, nal)| nal).collect();
    }
    let mut units = Vec::new();
    let mut rest = data;
    while let Some((length, tail)) = rest.split_first_chunk::<LENGTH_PREFIX>() {
        let length = u32::from_be_bytes(*length) as usize;
        let Some((nal, tail)) = tail.split_at_checked(length) else {
            break;
        };
        units.push(nal);
        rest = tail;
    }
    units
}

extern "C" fn output_callback(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image_buffer: CVPixelBufferRef,
    _presentation_time: CMTime,
    _duration: CMTime,
) {
    // SAFETY: `refcon` is the decoder's boxed mutex, which outlives the
    // session (the session is invalidated before it is dropped).
    let output = unsafe { &*refcon.cast::<Mutex<Output>>() };
    let result = if status != 0 {
        Err(DecoderError::msg(format!(
            "VideoToolbox decoding failed (OSStatus {status})"
        )))
    } else if image_buffer.is_null() {
        Err(DecoderError::msg("VideoToolbox dropped a frame"))
    } else {
        unsafe { planes(image_buffer) }
    };
    *output.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
}

/// Copies an NV12 pixel buffer into separate planes without row padding.
unsafe fn planes(buffer: CVPixelBufferRef) -> DecoderResult<Yuv420Frame> {
    unsafe {
        let format = CVPixelBufferGetPixelFormatType(buffer);
        if format != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            && format != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
        {
            return Err(DecoderError::msg(format!(
                "VideoToolbox decoded to an unexpected pixel format ({:?})",
                String::from_utf8_lossy(&format.to_be_bytes())
            )));
        }
        let (width, height) = (
            CVPixelBufferGetWidth(buffer),
            CVPixelBufferGetHeight(buffer),
        );
        let (Ok(frame_width), Ok(frame_height)) = (u16::try_from(width), u16::try_from(height))
        else {
            return Err(DecoderError::msg("H.264 picture larger than 65535 pixels"));
        };
        let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
        if CVPixelBufferGetWidthOfPlane(buffer, 1) < chroma_width
            || CVPixelBufferGetHeightOfPlane(buffer, 1) < chroma_height
        {
            return Err(DecoderError::msg("VideoToolbox chroma plane too small"));
        }
        check(
            CVPixelBufferLockBaseAddress(buffer, kCVPixelBufferLock_ReadOnly),
            "locking the picture",
        )?;
        // `rows` rows of `row` bytes out of a plane.
        let plane = |index: usize, rows: usize, row: usize| {
            let base = CVPixelBufferGetBaseAddressOfPlane(buffer, index);
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, index);
            (!base.is_null() && stride >= row && rows > 0).then(|| {
                let data = std::slice::from_raw_parts(base, (rows - 1) * stride + row);
                data.chunks(stride).map(move |line| &line[..row])
            })
        };
        let copied = match (
            plane(0, height, width),
            plane(1, chroma_height, chroma_width * 2),
        ) {
            (Some(luma), Some(chroma)) => {
                let mut y = Vec::with_capacity(width * height);
                luma.for_each(|line| y.extend_from_slice(line));
                let mut u = Vec::with_capacity(chroma_width * chroma_height);
                let mut v = Vec::with_capacity(chroma_width * chroma_height);
                for line in chroma {
                    u.extend(line.iter().step_by(2));
                    v.extend(line.iter().skip(1).step_by(2));
                }
                Some((y, u, v))
            }
            _ => None,
        };
        CVPixelBufferUnlockBaseAddress(buffer, kCVPixelBufferLock_ReadOnly);
        let (y, u, v) =
            copied.ok_or_else(|| DecoderError::msg("VideoToolbox picture has no planes"))?;
        Yuv420Frame::new(frame_width, frame_height, y, width, u, v, chroma_width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::videotoolbox::VideoToolboxEncoder;
    use crate::video::yuv::{self, ChromaLayout};
    use crate::video::{EncoderConfig, VideoCodec, VideoEncoder};

    const WIDTH: usize = 320;
    const HEIGHT: usize = 240;

    /// Smooth gradients that move with `index`.
    fn picture(index: usize) -> Vec<u8> {
        (0..WIDTH * HEIGHT)
            .flat_map(|i| {
                let (x, y) = (i % WIDTH, i / WIDTH);
                [
                    ((x + 4 * index) / 2) as u8,
                    ((y + 2 * index) / 2) as u8,
                    ((x + y) / 4) as u8,
                    255,
                ]
            })
            .collect()
    }

    fn encoder() -> VideoToolboxEncoder {
        VideoToolboxEncoder::new(&EncoderConfig {
            codec: VideoCodec::H264,
            width: WIDTH as u32,
            height: HEIGHT as u32,
            fps: 30,
            bitrate_kbps: 4000,
        })
        .expect("VideoToolbox H.264")
    }

    /// Mean absolute difference between a decoded picture's luma and the
    /// luma the encoder was given for `rgba`.
    fn luma_error(decoded: &Yuv420Frame, rgba: &[u8]) -> f64 {
        let mut expected = Vec::new();
        yuv::rgba_to_yuv420(rgba, WIDTH, HEIGHT, ChromaLayout::Planar, &mut expected);
        let total: u64 = (0..HEIGHT)
            .flat_map(|row| {
                let decoded = decoded.luma_row(row).expect("luma row");
                let expected = &expected[row * WIDTH..(row + 1) * WIDTH];
                decoded
                    .iter()
                    .zip(expected)
                    .map(|(a, b)| u64::from(a.abs_diff(*b)))
            })
            .sum();
        total as f64 / (WIDTH * HEIGHT) as f64
    }

    #[test]
    fn each_access_unit_gives_its_own_picture() {
        let mut encoder = encoder();
        let mut decoder = VideoToolboxDecoder::new();
        assert!(decoder.supports_yuv420());
        // Key frame, then frames that depend on it.
        for index in 0..6 {
            let rgba = picture(index * 8);
            let frame = encoder.encode(&rgba, false).expect("encode");
            let decoded = decoder.decode_yuv420(&frame.data).expect("decode");
            assert_eq!(
                (decoded.width(), decoded.height()),
                (WIDTH as u16, HEIGHT as u16)
            );
            assert_eq!(
                decoded.u_row(HEIGHT / 2 - 1).map(<[u8]>::len),
                Some(WIDTH / 2)
            );
            // A picture delayed by one frame would differ by the motion.
            let error = luma_error(&decoded, &rgba);
            assert!(error < 3.0, "frame {index}: luma differs by {error:.2}");
            let previous = luma_error(&decoded, &picture(index.saturating_sub(1) * 8));
            assert!(index == 0 || error < previous, "frame {index} is late");
        }
    }

    #[test]
    fn length_prefixed_units_and_rgba_output() {
        let mut encoder = encoder();
        let mut decoder = VideoToolboxDecoder::new();
        let rgba = picture(0);
        let frame = encoder.encode(&rgba, false).expect("encode");
        let avc = ironrdp_egfx::pdu::annex_b_to_avc(&frame.data);
        assert!(ironrdp_egfx::pdu::is_avc_format(&avc));
        let decoded = decoder.decode(&avc).expect("decode");
        assert_eq!(
            (decoded.width(), decoded.height()),
            (WIDTH as u32, HEIGHT as u32)
        );
        assert_eq!(decoded.data().len(), WIDTH * HEIGHT * 4);
    }

    #[test]
    fn a_stream_without_parameter_sets_is_refused_until_they_arrive() {
        let mut encoder = encoder();
        let mut decoder = VideoToolboxDecoder::new();
        let key = encoder.encode(&picture(0), false).expect("encode");
        let delta = encoder.encode(&picture(8), false).expect("encode");
        assert!(decoder.decode_yuv420(&delta.data).is_err());
        assert!(decoder.decode_yuv420(&[]).is_err());
        decoder.decode_yuv420(&key.data).expect("key frame");
        decoder.decode_yuv420(&delta.data).expect("delta frame");
        // After a reset the stream starts over with a key frame.
        decoder.reset();
        assert!(decoder.decode_yuv420(&delta.data).is_err());
        decoder.decode_yuv420(&key.data).expect("key frame again");
    }
}

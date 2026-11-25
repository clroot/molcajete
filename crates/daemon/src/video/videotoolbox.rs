//! Hardware H.264 / H.265 encoding with Apple VideoToolbox (macOS only).
//!
//! ffmpeg's `*_videotoolbox` encoders cannot be used through a pipe: ffmpeg
//! never waits for VideoToolbox's asynchronous output, so each packet only
//! comes out after the next frame goes in. Driving VideoToolbox directly,
//! every `encode` submits one frame and then calls
//! `VTCompressionSessionCompleteFrames`, which returns once that frame's
//! output callback has run: one packet per frame, without delay.
//!
//! The session requires the hardware encoder (no silent software fallback),
//! runs in real-time mode without frame reordering, and uses low-latency
//! rate control where the codec supports it. VideoToolbox returns
//! length-prefixed NAL units with the parameter sets in the format
//! description; they are converted to Annex B and every key frame carries
//! SPS/PPS (and VPS for H.265) in-band.

#![allow(non_upper_case_globals, non_camel_case_types)]

mod decode;

pub(crate) use decode::VideoToolboxDecoder;

use std::ffi::c_void;
use std::ptr;
use std::sync::{Mutex, PoisonError};

use super::bitstream;
use super::yuv;
use super::{EncodedFrame, EncoderConfig, VideoCodec, VideoEncoder, VideoError, check_frame};

type OSStatus = i32;
type Boolean = u8;
type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFMutableDictionaryRef = *mut c_void;
type CFArrayRef = *const c_void;
type CFBooleanRef = *const c_void;
type CFNumberRef = *const c_void;
type CFIndex = isize;
type CVPixelBufferRef = *mut c_void;
type CVPixelBufferPoolRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;
type CMFormatDescriptionRef = *const c_void;
type VTCompressionSessionRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const CMTIME_VALID: u32 = 1;
const CMTIME_INVALID: CMTime = CMTime {
    value: 0,
    timescale: 0,
    flags: 0,
    epoch: 0,
};

const kCMVideoCodecType_H264: u32 = u32::from_be_bytes(*b"avc1");
const kCMVideoCodecType_HEVC: u32 = u32::from_be_bytes(*b"hvc1");
/// NV12 with limited ("video") range, what [`yuv`] produces.
const kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange: u32 = u32::from_be_bytes(*b"420v");
const kCFNumberSInt32Type: CFIndex = 3;
const kCFNumberFloat64Type: CFIndex = 6;

type OutputCallback = extern "C" fn(
    output_refcon: *mut c_void,
    source_frame_refcon: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    sample_buffer: CMSampleBufferRef,
);

#[repr(C)]
struct CFDictionaryKeyCallBacks {
    _private: [u8; 0],
}

#[repr(C)]
struct CFDictionaryValueCallBacks {
    _private: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CFBooleanRef;
    static kCFBooleanFalse: CFBooleanRef;
    static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
    static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;

    fn CFRelease(cf: CFTypeRef);
    fn CFDictionaryCreateMutable(
        allocator: CFAllocatorRef,
        capacity: CFIndex,
        key_callbacks: *const CFDictionaryKeyCallBacks,
        value_callbacks: *const CFDictionaryValueCallBacks,
    ) -> CFMutableDictionaryRef;
    fn CFDictionarySetValue(dict: CFMutableDictionaryRef, key: CFTypeRef, value: CFTypeRef);
    fn CFDictionaryGetValue(dict: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    fn CFNumberCreate(
        allocator: CFAllocatorRef,
        the_type: CFIndex,
        value_ptr: *const c_void,
    ) -> CFNumberRef;
    fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: CFIndex) -> *const c_void;
    fn CFBooleanGetValue(boolean: CFBooleanRef) -> Boolean;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCVPixelBufferWidthKey: CFStringRef;
    static kCVPixelBufferHeightKey: CFStringRef;
    static kCVImageBufferColorPrimaries_ITU_R_709_2: CFStringRef;
    static kCVImageBufferTransferFunction_ITU_R_709_2: CFStringRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_709_2: CFStringRef;

    fn CVPixelBufferPoolCreatePixelBuffer(
        allocator: CFAllocatorRef,
        pool: CVPixelBufferPoolRef,
        out: *mut CVPixelBufferRef,
    ) -> i32;
    fn CVPixelBufferLockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(buffer: CVPixelBufferRef, plane: usize) -> *mut u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
    fn CVPixelBufferRelease(buffer: CVPixelBufferRef);
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFStringRef;

    fn CMSampleBufferGetDataBuffer(sample: CMSampleBufferRef) -> CMBlockBufferRef;
    fn CMSampleBufferGetFormatDescription(sample: CMSampleBufferRef) -> CMFormatDescriptionRef;
    fn CMSampleBufferGetSampleAttachmentsArray(
        sample: CMSampleBufferRef,
        create_if_necessary: Boolean,
    ) -> CFArrayRef;
    fn CMBlockBufferGetDataLength(buffer: CMBlockBufferRef) -> usize;
    fn CMBlockBufferCopyDataBytes(
        buffer: CMBlockBufferRef,
        offset: usize,
        length: usize,
        destination: *mut c_void,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        desc: CMFormatDescriptionRef,
        index: usize,
        pointer_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_length_out: *mut i32,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
        desc: CMFormatDescriptionRef,
        index: usize,
        pointer_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_length_out: *mut i32,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFStringRef;
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality: CFStringRef;
    static kVTCompressionPropertyKey_ColorPrimaries: CFStringRef;
    static kVTCompressionPropertyKey_TransferFunction: CFStringRef;
    static kVTCompressionPropertyKey_YCbCrMatrix: CFStringRef;
    static kVTProfileLevel_H264_Main_AutoLevel: CFStringRef;
    static kVTProfileLevel_HEVC_Main_AutoLevel: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;

    fn VTCompressionSessionCreate(
        allocator: CFAllocatorRef,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: CFDictionaryRef,
        source_image_buffer_attributes: CFDictionaryRef,
        compressed_data_allocator: CFAllocatorRef,
        output_callback: Option<OutputCallback>,
        output_callback_refcon: *mut c_void,
        session_out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    fn VTSessionSetProperty(session: CFTypeRef, key: CFStringRef, value: CFTypeRef) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: VTCompressionSessionRef) -> OSStatus;
    fn VTCompressionSessionGetPixelBufferPool(
        session: VTCompressionSessionRef,
    ) -> CVPixelBufferPoolRef;
    fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image_buffer: CVPixelBufferRef,
        presentation_time: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        complete_until: CMTime,
    ) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
}

/// Owns one Core Foundation reference.
struct Cf(CFTypeRef);

impl Cf {
    fn number_i32(value: i32) -> Self {
        Self(unsafe { CFNumberCreate(ptr::null(), kCFNumberSInt32Type, (&raw const value).cast()) })
    }

    fn number_f64(value: f64) -> Self {
        Self(unsafe {
            CFNumberCreate(ptr::null(), kCFNumberFloat64Type, (&raw const value).cast())
        })
    }

    fn dictionary(entries: &[(CFStringRef, CFTypeRef)]) -> Self {
        unsafe {
            let dict = CFDictionaryCreateMutable(
                ptr::null(),
                entries.len() as CFIndex,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            );
            for &(key, value) in entries {
                CFDictionarySetValue(dict, key, value);
            }
            Self(dict)
        }
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

fn check(status: OSStatus, what: &str) -> Result<(), VideoError> {
    if status == 0 {
        Ok(())
    } else {
        Err(VideoError::Encoder(format!(
            "VideoToolbox {what} failed (OSStatus {status})"
        )))
    }
}

/// What the output callback hands back for the frame being encoded.
type Output = Option<Result<Vec<u8>, VideoError>>;

pub(crate) struct VideoToolboxEncoder {
    session: VTCompressionSessionRef,
    /// Written by the output callback; boxed so its address stays fixed.
    output: Box<Mutex<Output>>,
    config: EncoderConfig,
    /// NV12 conversion buffer of the pre-GH-37 path (benchmarks only).
    #[cfg(test)]
    yuv: Vec<u8>,
    frames: i64,
    /// Set after a failure: the next frame must be a key frame.
    restart: bool,
}

// The session is only used from the thread that owns the encoder; the
// callback only touches `output`, which is behind a mutex.
unsafe impl Send for VideoToolboxEncoder {}

impl VideoToolboxEncoder {
    pub(crate) fn new(config: &EncoderConfig) -> Result<Self, VideoError> {
        let codec_type = match config.codec {
            VideoCodec::H264 => kCMVideoCodecType_H264,
            VideoCodec::H265 => kCMVideoCodecType_HEVC,
            VideoCodec::Av1 => return Err(VideoError::Unavailable(VideoCodec::Av1)),
        };
        let output: Box<Mutex<Output>> = Box::new(Mutex::new(None));
        // Low-latency rate control is not offered for every codec on every
        // macOS release; without it the session still works, with the
        // ordinary rate control.
        let session = create_session(config, codec_type, true, &output)
            .or_else(|_| create_session(config, codec_type, false, &output))?;
        let mut encoder = Self {
            session,
            output,
            config: config.clone(),
            #[cfg(test)]
            yuv: Vec::new(),
            frames: 0,
            restart: false,
        };
        encoder.configure()?;
        Ok(encoder)
    }

    fn set(&self, key: CFStringRef, value: CFTypeRef, what: &str) -> Result<(), VideoError> {
        check(
            unsafe { VTSessionSetProperty(self.session, key, value) },
            what,
        )
    }

    fn configure(&mut self) -> Result<(), VideoError> {
        let config = &self.config;
        unsafe {
            self.set(
                kVTCompressionPropertyKey_RealTime,
                kCFBooleanTrue,
                "RealTime",
            )?;
            self.set(
                kVTCompressionPropertyKey_AllowFrameReordering,
                kCFBooleanFalse,
                "AllowFrameReordering",
            )?;
            let profile = match config.codec {
                VideoCodec::H264 => kVTProfileLevel_H264_Main_AutoLevel,
                _ => kVTProfileLevel_HEVC_Main_AutoLevel,
            };
            self.set(
                kVTCompressionPropertyKey_ProfileLevel,
                profile,
                "ProfileLevel",
            )?;
            // Key frames only when asked for.
            let interval = Cf::number_i32(i32::MAX);
            self.set(
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                interval.0,
                "MaxKeyFrameInterval",
            )?;
            let bitrate = Cf::number_i32(config.bitrate_kbps.saturating_mul(1000) as i32);
            self.set(
                kVTCompressionPropertyKey_AverageBitRate,
                bitrate.0,
                "AverageBitRate",
            )?;
            let fps = Cf::number_f64(f64::from(config.fps));
            self.set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                fps.0,
                "ExpectedFrameRate",
            )?;
            // Older releases do not know this one; it is only a hint.
            let _ = self.set(
                kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                kCFBooleanTrue,
                "PrioritizeEncodingSpeedOverQuality",
            );
            // BT.709 limited range, signalled in the VUI like every backend.
            self.set(
                kVTCompressionPropertyKey_ColorPrimaries,
                kCVImageBufferColorPrimaries_ITU_R_709_2,
                "ColorPrimaries",
            )?;
            self.set(
                kVTCompressionPropertyKey_TransferFunction,
                kCVImageBufferTransferFunction_ITU_R_709_2,
                "TransferFunction",
            )?;
            self.set(
                kVTCompressionPropertyKey_YCbCrMatrix,
                kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                "YCbCrMatrix",
            )?;
            check(
                VTCompressionSessionPrepareToEncodeFrames(self.session),
                "PrepareToEncodeFrames",
            )
        }
    }

    /// Converts the RGBA frame straight into the planes of a pixel buffer
    /// from the session's pool (whose row stride may exceed the width).
    fn pixel_buffer(&mut self, rgba: &[u8]) -> Result<CVPixelBufferRef, VideoError> {
        let (width, height) = (self.config.width as usize, self.config.height as usize);
        unsafe {
            let pool = VTCompressionSessionGetPixelBufferPool(self.session);
            if pool.is_null() {
                return Err(VideoError::Encoder(
                    "VideoToolbox has no pixel buffer pool".into(),
                ));
            }
            let mut buffer: CVPixelBufferRef = ptr::null_mut();
            check(
                CVPixelBufferPoolCreatePixelBuffer(ptr::null(), pool, &mut buffer),
                "CVPixelBufferPoolCreatePixelBuffer",
            )?;
            if let Err(err) = check(CVPixelBufferLockBaseAddress(buffer, 0), "lock pixel buffer") {
                CVPixelBufferRelease(buffer);
                return Err(err);
            }
            // Each plane as a slice: `rows` rows of `stride` bytes (the
            // last one only as long as the picture is wide).
            let plane = |index: usize, rows: usize| {
                let base = CVPixelBufferGetBaseAddressOfPlane(buffer, index);
                let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, index);
                (!base.is_null() && stride >= width).then(|| yuv::Plane {
                    data: std::slice::from_raw_parts_mut(base, (rows - 1) * stride + width),
                    stride,
                })
            };
            let (Some(y), Some(uv)) = (plane(0, height), plane(1, height / 2)) else {
                CVPixelBufferUnlockBaseAddress(buffer, 0);
                CVPixelBufferRelease(buffer);
                return Err(VideoError::Encoder(
                    "VideoToolbox pixel buffer is not NV12 of the session's size".into(),
                ));
            };
            #[cfg(test)]
            if yuv::reference::LEGACY.get() {
                self.fill_legacy(rgba, y, uv);
                CVPixelBufferUnlockBaseAddress(buffer, 0);
                return Ok(buffer);
            }
            yuv::convert(
                rgba,
                width * 4,
                width,
                height,
                yuv::Planes::SemiPlanar { y, uv },
            );
            CVPixelBufferUnlockBaseAddress(buffer, 0);
            Ok(buffer)
        }
    }

    /// How the pixel buffer was filled before GH-37: converted into
    /// `self.yuv` first and copied row by row (for the benchmarks).
    #[cfg(test)]
    fn fill_legacy(&mut self, rgba: &[u8], y: yuv::Plane<'_>, uv: yuv::Plane<'_>) {
        let (width, height) = (self.config.width as usize, self.config.height as usize);
        yuv::reference::rgba_to_yuv420(
            rgba,
            width,
            height,
            yuv::ChromaLayout::SemiPlanar,
            &mut self.yuv,
        );
        let (luma, chroma) = self.yuv.split_at(width * height);
        for (plane, data) in [(y, luma), (uv, chroma)] {
            for (row, line) in data.chunks_exact(width).enumerate() {
                plane.data[row * plane.stride..][..width].copy_from_slice(line);
            }
        }
    }

    fn encode_frame(&mut self, rgba: &[u8], force_key: bool) -> Result<Vec<u8>, VideoError> {
        let buffer = self.pixel_buffer(rgba)?;
        let fps = self.config.fps as i32;
        let pts = CMTime {
            value: self.frames,
            timescale: fps,
            flags: CMTIME_VALID,
            epoch: 0,
        };
        let duration = CMTime { value: 1, ..pts };
        let properties = (force_key && self.frames > 0).then(|| unsafe {
            Cf::dictionary(&[(kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue)])
        });
        *self.output.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let status = unsafe {
            let status = VTCompressionSessionEncodeFrame(
                self.session,
                buffer,
                pts,
                duration,
                properties.as_ref().map_or(ptr::null(), |p| p.0),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            CVPixelBufferRelease(buffer);
            status
        };
        check(status, "EncodeFrame")?;
        // Returns once the callback for every submitted frame has run.
        check(
            unsafe { VTCompressionSessionCompleteFrames(self.session, CMTIME_INVALID) },
            "CompleteFrames",
        )?;
        self.frames += 1;
        self.output
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| Err(VideoError::Encoder("VideoToolbox dropped a frame".into())))
    }
}

fn create_session(
    config: &EncoderConfig,
    codec_type: u32,
    low_latency: bool,
    output: &Mutex<Output>,
) -> Result<VTCompressionSessionRef, VideoError> {
    unsafe {
        let mut entries = vec![(
            kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
            kCFBooleanTrue,
        )];
        if low_latency {
            entries.push((
                kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
                kCFBooleanTrue,
            ));
        }
        let specification = Cf::dictionary(&entries);
        let pixel_format = Cf::number_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32);
        let width = Cf::number_i32(config.width as i32);
        let height = Cf::number_i32(config.height as i32);
        let attributes = Cf::dictionary(&[
            (kCVPixelBufferPixelFormatTypeKey, pixel_format.0),
            (kCVPixelBufferWidthKey, width.0),
            (kCVPixelBufferHeightKey, height.0),
        ]);
        let mut session: VTCompressionSessionRef = ptr::null_mut();
        check(
            VTCompressionSessionCreate(
                ptr::null(),
                config.width as i32,
                config.height as i32,
                codec_type,
                specification.0,
                attributes.0,
                ptr::null(),
                Some(output_callback),
                ptr::from_ref(output).cast_mut().cast(),
                &mut session,
            ),
            "CompressionSessionCreate",
        )?;
        Ok(session)
    }
}

extern "C" fn output_callback(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    sample: CMSampleBufferRef,
) {
    // SAFETY: `refcon` is the encoder's boxed mutex, which outlives the
    // session (the session is invalidated before it is dropped).
    let output = unsafe { &*refcon.cast::<Mutex<Output>>() };
    let result = if status != 0 {
        Err(VideoError::Encoder(format!(
            "VideoToolbox encoding failed (OSStatus {status})"
        )))
    } else if sample.is_null() {
        Err(VideoError::Encoder("VideoToolbox dropped a frame".into()))
    } else {
        unsafe { annex_b(sample) }
    };
    *output.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
}

/// Whether the sample is a sync (key) frame: it has no `NotSync` attachment.
unsafe fn is_sync(sample: CMSampleBufferRef) -> bool {
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
        if attachments.is_null() || CFArrayGetCount(attachments) == 0 {
            return true;
        }
        let first = CFArrayGetValueAtIndex(attachments, 0);
        let not_sync = CFDictionaryGetValue(first, kCMSampleAttachmentKey_NotSync);
        not_sync.is_null() || CFBooleanGetValue(not_sync) == 0
    }
}

/// Converts a sample (length-prefixed NAL units) into Annex B, with the
/// format description's parameter sets in front of key frames.
unsafe fn annex_b(sample: CMSampleBufferRef) -> Result<Vec<u8>, VideoError> {
    unsafe {
        let desc = CMSampleBufferGetFormatDescription(sample);
        if desc.is_null() {
            return Err(VideoError::Encoder(
                "VideoToolbox sample without format".into(),
            ));
        }
        let hevc = {
            // H.264 parameter sets fail on an HEVC description and vice versa.
            let mut count = 0;
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                desc,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut count,
                ptr::null_mut(),
            ) != 0
        };
        let get = if hevc {
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex
        } else {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex
        };
        let mut count = 0usize;
        let mut header_len = 0i32;
        check(
            get(
                desc,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut count,
                &mut header_len,
            ),
            "parameter set count",
        )?;
        let header_len = header_len as usize;
        if !(1..=4).contains(&header_len) {
            return Err(VideoError::Encoder(format!(
                "VideoToolbox NAL length size {header_len}"
            )));
        }

        let mut out = Vec::new();
        if is_sync(sample) {
            for index in 0..count {
                let (mut data, mut size) = (ptr::null(), 0usize);
                check(
                    get(
                        desc,
                        index,
                        &mut data,
                        &mut size,
                        ptr::null_mut(),
                        ptr::null_mut(),
                    ),
                    "parameter set",
                )?;
                out.extend_from_slice(&bitstream::START_CODE);
                out.extend_from_slice(std::slice::from_raw_parts(data, size));
            }
        }

        let block = CMSampleBufferGetDataBuffer(sample);
        if block.is_null() {
            return Err(VideoError::Encoder(
                "VideoToolbox sample without data".into(),
            ));
        }
        let len = CMBlockBufferGetDataLength(block);
        let mut data = vec![0u8; len];
        check(
            CMBlockBufferCopyDataBytes(block, 0, len, data.as_mut_ptr().cast()),
            "CMBlockBufferCopyDataBytes",
        )?;
        let mut rest = &data[..];
        while !rest.is_empty() {
            if rest.len() < header_len {
                return Err(VideoError::Encoder(
                    "truncated VideoToolbox NAL unit".into(),
                ));
            }
            let size = rest[..header_len]
                .iter()
                .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
            let unit = rest
                .get(header_len..header_len + size)
                .ok_or_else(|| VideoError::Encoder("truncated VideoToolbox NAL unit".into()))?;
            out.extend_from_slice(&bitstream::START_CODE);
            out.extend_from_slice(unit);
            rest = &rest[header_len + size..];
        }
        Ok(out)
    }
}

impl Drop for VideoToolboxEncoder {
    fn drop(&mut self) {
        unsafe {
            // Waits for pending callbacks, so `output` is no longer used.
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session.cast_const());
        }
    }
}

impl VideoEncoder for VideoToolboxEncoder {
    fn codec(&self) -> VideoCodec {
        self.config.codec
    }

    fn encode(&mut self, rgba: &[u8], force_key: bool) -> Result<EncodedFrame, VideoError> {
        check_frame(&self.config, rgba)?;
        let force_key = force_key || self.restart;
        match self.encode_frame(rgba, force_key) {
            Ok(data) if data.is_empty() => {
                self.restart = true;
                Err(VideoError::Encoder(
                    "VideoToolbox returned an empty frame".into(),
                ))
            }
            Ok(data) => {
                let key = bitstream::is_key_frame(self.config.codec, &data);
                if force_key && !key {
                    self.restart = true;
                    return Err(VideoError::Encoder(
                        "VideoToolbox ignored a key frame request".into(),
                    ));
                }
                self.restart = false;
                Ok(EncodedFrame { data, key })
            }
            Err(err) => {
                self.restart = true;
                Err(err)
            }
        }
    }
}

/// Checks that the hardware encoder for `codec` really works here: a few
/// small frames must each come back as one packet, the first a key frame
/// carrying the codec configuration.
pub(crate) fn probe(codec: VideoCodec) -> Result<(), VideoError> {
    let config = EncoderConfig {
        codec,
        width: 320,
        height: 240,
        fps: 30,
        bitrate_kbps: 500,
    };
    let mut encoder = VideoToolboxEncoder::new(&config)?;
    for index in 0..3u32 {
        let frame: Vec<u8> = (0..config.width * config.height)
            .flat_map(|i| {
                let (x, y) = (i % config.width, i / config.width);
                [
                    (x + 4 * index) as u8,
                    (y + 2 * index) as u8,
                    ((x ^ y) + index) as u8,
                    255,
                ]
            })
            .collect();
        let packet = encoder.encode(&frame, false)?;
        if index == 0 && !(packet.key && bitstream::has_config(codec, &packet.data)) {
            return Err(VideoError::Encoder(
                "first frame is not a key frame with in-band configuration".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn encoder(width: u32, height: u32) -> VideoToolboxEncoder {
        let config = EncoderConfig {
            codec: VideoCodec::H264,
            width,
            height,
            fps: 30,
            bitrate_kbps: 2000,
        };
        VideoToolboxEncoder::new(&config).expect("VideoToolbox H.264")
    }

    fn picture(width: u32, height: u32, seed: u32) -> Vec<u8> {
        (0..width * height)
            .flat_map(|i| {
                let (x, y) = (i % width, i / width);
                let noise = (i ^ seed).wrapping_mul(2_654_435_761) >> 24;
                [(x + seed) as u8, (y * 3) as u8, noise as u8, 255]
            })
            .collect()
    }

    /// The planes of a pixel buffer (Y, then UV), without the row padding.
    fn planes(encoder: &mut VideoToolboxEncoder, rgba: &[u8], legacy: bool) -> Vec<u8> {
        let (width, height) = (
            encoder.config.width as usize,
            encoder.config.height as usize,
        );
        yuv::reference::LEGACY.set(legacy);
        let buffer = encoder.pixel_buffer(rgba);
        yuv::reference::LEGACY.set(false);
        let buffer = buffer.unwrap();
        let mut out = Vec::new();
        unsafe {
            assert_eq!(CVPixelBufferLockBaseAddress(buffer, 0), 0);
            for (plane, rows) in [(0, height), (1, height / 2)] {
                let base = CVPixelBufferGetBaseAddressOfPlane(buffer, plane);
                let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, plane);
                for row in 0..rows {
                    out.extend_from_slice(std::slice::from_raw_parts(
                        base.add(row * stride),
                        width,
                    ));
                }
            }
            CVPixelBufferUnlockBaseAddress(buffer, 0);
            CVPixelBufferRelease(buffer);
        }
        out
    }

    #[test]
    fn pixel_buffer_holds_the_same_nv12_as_before() {
        // Not a multiple of the pool's row alignment.
        let (width, height) = (202, 130);
        let mut encoder = encoder(width, height);
        let rgba = picture(width, height, 5);
        let mut expected = Vec::new();
        yuv::reference::rgba_to_yuv420(
            &rgba,
            width as usize,
            height as usize,
            yuv::ChromaLayout::SemiPlanar,
            &mut expected,
        );
        assert_eq!(planes(&mut encoder, &rgba, true), expected);
        assert_eq!(planes(&mut encoder, &rgba, false), expected);
    }

    /// Time to fill a pixel buffer from RGBA, before GH-37 and now:
    /// `cargo test --release -p molcajete-daemon videotoolbox -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore = "benchmark"]
    fn pixel_buffer_speed() {
        for (width, height) in [(1920, 1088), (1344, 2176)] {
            let mut encoder = encoder(width, height);
            let rgba = picture(width, height, 1);
            let mut time = |legacy: bool| {
                let mut times: Vec<Duration> = (0..30)
                    .map(|_| {
                        yuv::reference::LEGACY.set(legacy);
                        let start = Instant::now();
                        let buffer = encoder.pixel_buffer(std::hint::black_box(&rgba));
                        let elapsed = start.elapsed();
                        yuv::reference::LEGACY.set(false);
                        unsafe { CVPixelBufferRelease(buffer.unwrap()) };
                        elapsed
                    })
                    .collect();
                times.sort();
                times[times.len() / 2].as_secs_f64() * 1000.0
            };
            let before = time(true);
            let after = time(false);
            eprintln!(
                "VideoToolbox pixel buffer {width}x{height}: before GH-37 {before:.3} ms, now {after:.3} ms (median of 30)"
            );
        }
    }
}

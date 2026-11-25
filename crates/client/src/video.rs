//! Playback of `video` streams with WebCodecs `VideoDecoder`.
//!
//! The stream carries framed access units (see `molcajete_protocol::video`). Each
//! frame becomes an image draw in the display queue — exactly like an `img`
//! stream — whose slot is filled when the decoder outputs the frame, so
//! video stays in step with `sync` frames and other drawing.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use molcajete_protocol::video::{FrameReader, VideoCodec};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::client::{WeakShared, with_inner};
use crate::display::{ImageSlot, ImageState};
use crate::image::DecodedImage;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = VideoDecoder)]
    type JsVideoDecoder;

    #[wasm_bindgen(constructor, catch, js_class = "VideoDecoder")]
    fn new(init: &JsValue) -> Result<JsVideoDecoder, JsValue>;

    #[wasm_bindgen(method, catch)]
    fn configure(this: &JsVideoDecoder, config: &JsValue) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch)]
    fn decode(this: &JsVideoDecoder, chunk: &JsValue) -> Result<(), JsValue>;

    #[wasm_bindgen(method)]
    fn flush(this: &JsVideoDecoder) -> js_sys::Promise;

    #[wasm_bindgen(method, catch)]
    fn close(this: &JsVideoDecoder) -> Result<(), JsValue>;

    #[wasm_bindgen(js_name = EncodedVideoChunk)]
    type JsEncodedVideoChunk;

    #[wasm_bindgen(constructor, catch, js_class = "EncodedVideoChunk")]
    fn new(init: &JsValue) -> Result<JsEncodedVideoChunk, JsValue>;
}

/// WebCodecs codec string used to configure the decoder for each codec.
/// Profiles/levels are generous so any stream molcajeted produces fits.
pub fn codec_string(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "avc1.64002A",
        VideoCodec::H265 => "hvc1.1.6.L120.90",
        VideoCodec::Av1 => "av01.0.08M.08",
    }
}

fn set(obj: &js_sys::Object, key: &str, value: &JsValue) {
    let _ = js_sys::Reflect::set(obj, &key.into(), value);
}

fn number(obj: &JsValue, key: &str) -> u32 {
    js_sys::Reflect::get(obj, &key.into())
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0) as u32
}

pub fn decoder_available() -> bool {
    js_sys::Reflect::has(&js_sys::global(), &"VideoDecoder".into()).unwrap_or(false)
}

type Pending = Rc<RefCell<VecDeque<ImageSlot>>>;

fn fail_all(pending: &Pending) {
    for slot in pending.borrow_mut().drain(..) {
        slot.finish(ImageState::Failed);
    }
}

pub struct VideoStream {
    pub layer: i32,
    reader: FrameReader,
    decoder: JsVideoDecoder,
    /// Display slots awaiting decoder output, in decode order.
    pending: Pending,
    _output: Closure<dyn FnMut(JsValue)>,
    _error: Closure<dyn FnMut(JsValue)>,
}

impl VideoStream {
    pub fn open(
        codec: VideoCodec,
        stream: i32,
        layer: i32,
        weak: WeakShared,
    ) -> Result<Self, JsValue> {
        let pending: Pending = Rc::new(RefCell::new(VecDeque::new()));

        let output_pending = pending.clone();
        let output_weak = weak.clone();
        let output = Closure::<dyn FnMut(JsValue)>::new(move |frame: JsValue| {
            let slot = output_pending.borrow_mut().pop_front();
            match slot {
                Some(slot) => {
                    let image = DecodedImage {
                        width: number(&frame, "displayWidth"),
                        height: number(&frame, "displayHeight"),
                        source: frame,
                        opaque: true,
                    };
                    slot.finish(ImageState::Ready(image));
                }
                None => {
                    // More frames than requested: release it immediately.
                    let image = DecodedImage {
                        source: frame,
                        width: 0,
                        height: 0,
                        opaque: true,
                    };
                    image.close();
                }
            }
            with_inner(&output_weak, |inner| inner.display().process());
        });

        let error_pending = pending.clone();
        let error_weak = weak;
        let error = Closure::<dyn FnMut(JsValue)>::new(move |err: JsValue| {
            web_sys::console::warn_2(&"video decoder error:".into(), &err);
            fail_all(&error_pending);
            with_inner(&error_weak, |inner| inner.display().process());
            // Tell the server, which falls back to images. Deferred: closing
            // the stream drops this closure.
            let weak = error_weak.clone();
            wasm_bindgen_futures::spawn_local(async move {
                with_inner(&weak, |inner| inner.video_failed(stream));
            });
        });

        let init = js_sys::Object::new();
        set(&init, "output", output.as_ref());
        set(&init, "error", error.as_ref());
        let decoder = JsVideoDecoder::new(&init)?;
        let config = js_sys::Object::new();
        set(&config, "codec", &codec_string(codec).into());
        set(&config, "optimizeForLatency", &JsValue::TRUE);
        decoder.configure(&config)?;

        Ok(Self {
            layer,
            reader: FrameReader::default(),
            decoder,
            pending,
            _output: output,
            _error: error,
        })
    }

    /// Feeds stream data. Returns one display slot per complete frame, in
    /// order; the caller queues a draw for each.
    pub fn push(&mut self, data: &[u8]) -> Result<Vec<ImageSlot>, String> {
        self.reader.push(data);
        let mut slots = Vec::new();
        while let Some(frame) = self.reader.next_frame().map_err(str::to_owned)? {
            let init = js_sys::Object::new();
            set(
                &init,
                "type",
                &(if frame.key { "key" } else { "delta" }).into(),
            );
            set(
                &init,
                "timestamp",
                &(frame.timestamp_ms as f64 * 1000.0).into(),
            );
            set(
                &init,
                "data",
                &js_sys::Uint8Array::from(frame.payload.as_slice()).into(),
            );
            let chunk = JsEncodedVideoChunk::new(&init).map_err(|e| format!("{e:?}"))?;
            let slot = ImageSlot::pending();
            self.pending.borrow_mut().push_back(slot.clone());
            if let Err(err) = self.decoder.decode(chunk.unchecked_ref()) {
                self.pending.borrow_mut().pop_back();
                slot.finish(ImageState::Failed);
                return Err(format!("{err:?}"));
            }
            slots.push(slot);
        }
        Ok(slots)
    }

    /// Ends the stream: frames still being decoded are delivered, then the
    /// decoder is released.
    pub fn finish(self) {
        let flushed = self.decoder.flush();
        wasm_bindgen_futures::spawn_local(async move {
            let _ = wasm_bindgen_futures::JsFuture::from(flushed).await;
            drop(self);
        });
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        let _ = self.decoder.close();
        fail_all(&self.pending);
    }
}

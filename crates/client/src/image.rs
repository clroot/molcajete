//! Off-main-thread image decoding.
//!
//! WebCodecs `ImageDecoder` is preferred: it decodes straight from the bytes
//! we already have and yields a `VideoFrame` that WebGL can upload directly.
//! Creating a `Blob` per image and calling `createImageBitmap()` costs far
//! more main-thread time in Chromium (the reference client avoids it for the
//! same reason), so that path is only a fallback.

use std::cell::Cell;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

/// A decoded image ready for texture upload (`ImageBitmap` or `VideoFrame`).
pub struct DecodedImage {
    pub source: JsValue,
    pub width: u32,
    pub height: u32,
    /// Known to have no transparent pixels (JPEG).
    pub opaque: bool,
}

impl DecodedImage {
    /// Releases the decoded pixels (`close()` exists on both source types).
    pub fn close(&self) {
        if let Ok(close) = js_sys::Reflect::get(&self.source, &"close".into())
            && let Some(f) = close.dyn_ref::<js_sys::Function>()
        {
            let _ = f.call0(&self.source);
        }
    }
}

/// Which image formats arrived, for the client's stats.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Formats(u8);

impl Formats {
    /// In the order they are reported.
    const NAMES: [&'static str; 5] = ["jpeg", "webp", "png", "zstd", "other"];

    pub fn add(&mut self, mimetype: &str) {
        let index = match mimetype.split(';').next().unwrap_or_default().trim() {
            "image/jpeg" => 0,
            "image/webp" => 1,
            "image/png" => 2,
            molcajete_protocol::zimage::MIMETYPE => 3,
            _ => 4,
        };
        self.0 |= 1 << index;
    }

    pub fn names(self) -> impl Iterator<Item = &'static str> {
        Self::NAMES
            .into_iter()
            .enumerate()
            .filter(move |(index, _)| self.0 & (1 << index) != 0)
            .map(|(_, name)| name)
    }
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = ImageDecoder)]
    type ImageDecoder;

    #[wasm_bindgen(constructor, catch, js_class = "ImageDecoder")]
    fn new(init: &JsValue) -> Result<ImageDecoder, JsValue>;

    #[wasm_bindgen(method)]
    fn decode(this: &ImageDecoder) -> js_sys::Promise;

    #[wasm_bindgen(method)]
    fn close(this: &ImageDecoder);
}

thread_local! {
    static HAS_IMAGE_DECODER: Cell<Option<bool>> = const { Cell::new(None) };
}

fn has_image_decoder() -> bool {
    HAS_IMAGE_DECODER.with(|cached| {
        *cached.get().get_or_insert_with(|| {
            js_sys::Reflect::has(&js_sys::global(), &"ImageDecoder".into()).unwrap_or(false)
        })
    })
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

async fn decode_with_image_decoder(
    data: &js_sys::Uint8Array,
    mimetype: &str,
) -> Result<JsValue, JsValue> {
    let init = js_sys::Object::new();
    set(&init, "type", &mimetype.into());
    set(&init, "data", data);
    // Remote desktop pixels must not be colour managed.
    set(&init, "colorSpaceConversion", &"none".into());
    let decoder = ImageDecoder::new(&init)?;
    let result = wasm_bindgen_futures::JsFuture::from(decoder.decode()).await;
    decoder.close();
    js_sys::Reflect::get(&result?, &"image".into())
}

async fn decode_with_bitmap(data: &js_sys::Uint8Array, mimetype: &str) -> Result<JsValue, JsValue> {
    let parts = js_sys::Array::of1(data);
    let bag = web_sys::BlobPropertyBag::new();
    bag.set_type(mimetype);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &bag)?;
    let options = web_sys::ImageBitmapOptions::new();
    options.set_premultiply_alpha(web_sys::PremultiplyAlpha::Premultiply);
    options.set_color_space_conversion(web_sys::ColorSpaceConversion::None);
    let promise = crate::scope::create_image_bitmap(&blob, &options)?;
    wasm_bindgen_futures::JsFuture::from(promise).await
}

/// How much of the start of an image [`is_opaque`] needs at most to decide
/// in practice: a PNG header with a full palette and its transparency chunk
/// takes about 1.1 KiB.
pub const OPACITY_PREFIX_BYTES: u32 = 2048;

/// Whether an encoded image certainly has no transparency, judged from its
/// header. Opaque images can be uploaded straight into a layer texture.
/// `bytes` may be just the start of the image
/// ([`OPACITY_PREFIX_BYTES`]); if the header does not fit, the image is
/// taken to be transparent, which is always correct.
pub fn is_opaque(bytes: &[u8], mimetype: &str) -> bool {
    match mimetype {
        "image/jpeg" => true,
        "image/png" => png_is_opaque(bytes),
        "image/webp" => webp_is_opaque(bytes),
        _ => false,
    }
}

fn png_is_opaque(bytes: &[u8]) -> bool {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 33 || &bytes[..8] != SIGNATURE || &bytes[12..16] != b"IHDR" {
        return false;
    }
    // Greyscale, RGB and palette images are opaque unless they carry tRNS.
    if !matches!(bytes[25], 0 | 2 | 3) {
        return false;
    }
    let mut pos = 8;
    while pos + 8 <= bytes.len() {
        let len = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        match &bytes[pos + 4..pos + 8] {
            b"tRNS" => return false,
            b"IDAT" | b"IEND" => return true,
            _ => {}
        }
        pos = pos.saturating_add(12).saturating_add(len);
    }
    false
}

fn webp_is_opaque(bytes: &[u8]) -> bool {
    if bytes.len() < 30 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return false;
    }
    match &bytes[12..16] {
        // Simple lossy: no alpha.
        b"VP8 " => true,
        // Simple lossless: `alpha_is_used` hint in the header.
        b"VP8L" => bytes.len() > 24 && bytes[24] & 0x10 == 0,
        // Extended: alpha flag.
        b"VP8X" => bytes[20] & 0x10 == 0,
        _ => false,
    }
}

/// Decodes a complete image. `data` may be a view of a larger buffer (a
/// received WebSocket message); `opaque` is what [`is_opaque`] said.
pub async fn decode(
    data: js_sys::Uint8Array,
    mimetype: String,
    opaque: bool,
) -> Result<DecodedImage, JsValue> {
    if has_image_decoder() {
        match decode_with_image_decoder(&data, &mimetype).await {
            Ok(frame) => {
                return Ok(DecodedImage {
                    width: number(&frame, "displayWidth"),
                    height: number(&frame, "displayHeight"),
                    source: frame,
                    opaque,
                });
            }
            Err(err) => {
                web_sys::console::debug_2(&"ImageDecoder failed, falling back:".into(), &err)
            }
        }
    }
    let bitmap = decode_with_bitmap(&data, &mimetype).await?;
    Ok(DecodedImage {
        width: number(&bitmap, "width"),
        height: number(&bitmap, "height"),
        source: bitmap,
        opaque,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_are_named_once_each_in_a_fixed_order() {
        let mut formats = Formats::default();
        assert_eq!(formats.names().count(), 0);
        formats.add("image/png");
        formats.add(molcajete_protocol::zimage::MIMETYPE);
        formats.add("image/jpeg");
        formats.add("image/png");
        assert_eq!(formats.names().collect::<Vec<_>>(), ["jpeg", "png", "zstd"]);
        formats.add("image/webp; foo=bar");
        formats.add("image/avif");
        assert_eq!(
            formats.names().collect::<Vec<_>>(),
            ["jpeg", "webp", "png", "zstd", "other"]
        );
    }

    fn png(color_type: u8, extra_chunk: Option<&[u8; 4]>) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, color_type, 0, 0, 0]);
        v.extend_from_slice(&[0; 4]); // CRC (not checked)
        if let Some(kind) = extra_chunk {
            v.extend_from_slice(&1u32.to_be_bytes());
            v.extend_from_slice(kind);
            v.extend_from_slice(&[0; 5]);
        }
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(b"IDAT");
        v.extend_from_slice(&[0; 4]);
        v
    }

    #[test]
    fn png_opacity_from_header() {
        assert!(is_opaque(&png(2, None), "image/png"));
        assert!(is_opaque(&png(3, Some(b"PLTE")), "image/png"));
        assert!(!is_opaque(&png(3, Some(b"tRNS")), "image/png"));
        assert!(!is_opaque(&png(6, None), "image/png"));
        assert!(!is_opaque(b"not a png at all, definitely not", "image/png"));
    }

    #[test]
    fn png_opacity_from_a_prefix() {
        // A full palette followed by transparency, then megabytes of data.
        let mut v = png(3, None);
        v.truncate(33);
        v.extend_from_slice(&768u32.to_be_bytes());
        v.extend_from_slice(b"PLTE");
        v.extend_from_slice(&[0; 768 + 4]);
        let palette_end = v.len();
        v.extend_from_slice(&256u32.to_be_bytes());
        v.extend_from_slice(b"tRNS");
        v.extend_from_slice(&[0; 256 + 4]);
        assert!(v.len() + 8 < OPACITY_PREFIX_BYTES as usize);
        let mut opaque = v[..palette_end].to_vec();
        for image in [&mut v, &mut opaque] {
            image.extend_from_slice(&0u32.to_be_bytes());
            image.extend_from_slice(b"IDAT");
            image.resize(1 << 20, 0);
        }
        let prefix = OPACITY_PREFIX_BYTES as usize;
        assert!(!is_opaque(&v[..prefix], "image/png"));
        assert!(is_opaque(&opaque[..prefix], "image/png"));
        // Cut before the first data chunk: not known to be opaque.
        assert!(!is_opaque(&opaque[..palette_end], "image/png"));
    }

    #[test]
    fn webp_opacity_from_header() {
        let mut lossy = b"RIFF\0\0\0\0WEBPVP8 ".to_vec();
        lossy.resize(32, 0);
        assert!(is_opaque(&lossy, "image/webp"));
        let mut extended = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        extended.resize(32, 0);
        extended[20] = 0x10;
        assert!(!is_opaque(&extended, "image/webp"));
        assert!(is_opaque(&[], "image/jpeg"));
    }
}

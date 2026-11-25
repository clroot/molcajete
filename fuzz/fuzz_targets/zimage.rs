//! The zstd image decoder of the wasm client, on images from the server.
//!
//! A decoded image must have exactly the pixels its header announces, at
//! most `MAX_PIXELS`, all opaque, and `dimensions` must agree with it. The
//! same decoder decodes the input twice, as it decodes every image of a
//! session with the buffers of the previous one.

#![no_main]

use libfuzzer_sys::fuzz_target;
use molcajete_protocol::zimage::{self, Decoder, MAX_PIXELS};

fuzz_target!(|data: &[u8]| {
    let mut decoder = Decoder::default();
    let mut rgba = Vec::new();
    let first = decoder.decode(data, &mut rgba);
    if let Ok((width, height)) = first {
        assert!(width >= 1 && height >= 1 && width * height <= MAX_PIXELS);
        assert_eq!(rgba.len(), width as usize * height as usize * 4);
        assert!(rgba.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255));
        assert_eq!(zimage::dimensions(data), Ok((width, height)));
    }
    let pixels = rgba.clone();
    let second = decoder.decode(data, &mut rgba);
    assert_eq!(first, second);
    if second.is_ok() {
        assert_eq!(pixels, rgba);
    }
});

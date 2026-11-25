//! The client's reassembly of video frames (`FrameReader`) from stream data
//! arriving in arbitrary pieces.
//!
//! Input: one byte picking the split, then the stream. Reading it in pieces
//! must give the same frames as reading it in one piece, and every frame
//! must be what its header says.

#![no_main]

use libfuzzer_sys::fuzz_target;
use molcajete_fuzz::pieces;
use molcajete_protocol::video::{FRAME_HEADER_LEN, FrameReader, VideoFrame};

fn read(pieces: &[&[u8]]) -> Vec<(u32, bool, Vec<u8>)> {
    let mut reader = FrameReader::default();
    let mut frames = Vec::new();
    for piece in pieces {
        reader.push(piece);
        loop {
            match reader.next_frame() {
                Ok(Some(VideoFrame {
                    timestamp_ms,
                    key,
                    payload,
                })) => {
                    assert!(payload.len() <= FrameReader::MAX_FRAME_LEN);
                    frames.push((timestamp_ms, key, payload));
                }
                Ok(None) => break,
                Err(_) => return frames,
            }
        }
    }
    frames
}

fuzz_target!(|data: &[u8]| {
    let Some((&seed, stream)) = data.split_first() else {
        return;
    };
    let whole = read(&[stream]);
    assert_eq!(whole, read(&pieces(seed, stream)));
    let consumed: usize = whole
        .iter()
        .map(|(_, _, payload)| FRAME_HEADER_LEN + payload.len())
        .sum();
    assert!(consumed <= stream.len());
});

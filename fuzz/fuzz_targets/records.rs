//! Binary record framing: `RecordReader` on a byte stream cut anywhere (the
//! gateway reading molcajeted) and `records` on a whole message (the client
//! reading a WebSocket message).
//!
//! Input: one byte picking the split, then the stream. Both readers must
//! find the same records.

#![no_main]

use libfuzzer_sys::fuzz_target;
use molcajete_fuzz::pieces;
use molcajete_protocol::binary::{HEADER_LEN, Record, RecordReader, parse_header, records};

/// A record copied out of the buffer it was read from.
#[derive(Debug, PartialEq, Eq)]
enum Owned {
    Instructions(Vec<u8>),
    Blob(i32, Vec<u8>),
    Unknown(u8, Vec<u8>),
}

impl From<Record<'_>> for Owned {
    fn from(record: Record<'_>) -> Self {
        match record {
            Record::Instructions(text) => Self::Instructions(text.to_vec()),
            Record::Blob { stream, data } => Self::Blob(stream, data.to_vec()),
            Record::Unknown { tag, body } => Self::Unknown(tag, body.to_vec()),
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&seed, stream)) = data.split_first() else {
        return;
    };

    let mut reader = RecordReader::new();
    let mut read = Vec::new();
    'pieces: for piece in pieces(seed, stream) {
        reader.feed(piece);
        loop {
            match reader.next_record() {
                Ok(Some(raw)) => {
                    let (_, length) = parse_header(raw.as_bytes()).expect("a whole header");
                    assert_eq!(raw.as_bytes().len(), HEADER_LEN + length);
                    read.push(Owned::from(raw.record()));
                }
                Ok(None) => break,
                Err(_) => break 'pieces,
            }
        }
    }

    let whole: Vec<Owned> = records(stream)
        .map_while(Result::ok)
        .map(Owned::from)
        .collect();
    assert_eq!(read, whole);
});

//! Binary tunnel framing: an extension of the Guacamole protocol, spoken
//! only between molcajete components, that carries stream data (`blob`) as
//! raw bytes instead of base64 text.
//!
//! The server → client direction of a connection becomes a sequence of
//! **records**:
//!
//! ```text
//! record       = tag length body
//! tag          = 1 byte
//! length       = 4 bytes, unsigned, little endian: the size of body
//! body         = `length` bytes
//!
//! tag 0x00     instructions: body is Guacamole protocol text (UTF-8)
//! tag 0x01     blob: body = stream index (4 bytes, unsigned, little endian)
//!              followed by the raw data; same meaning as the instruction
//!              `blob,<stream>,<base64 of data>;`
//! ```
//!
//! - The bodies of the instruction records, concatenated in order, are the
//!   ordinary instruction stream, with every `blob` instruction left out. A
//!   blob record takes effect at its position in that stream and never
//!   appears inside an instruction. Writers put whole instructions in each
//!   instruction record; readers need not rely on it.
//! - Consecutive blob records of one stream are equivalent to one record
//!   with their data joined, so data may be split anywhere (subject to what
//!   the stream's content requires, such as whole PCM frames).
//! - A record with an unknown tag is skipped.
//! - Between gateway and molcajeted (TCP) the records follow each other in
//!   the byte stream, starting right after the `;` of `ready`. On the
//!   WebSocket every binary message holds whole records.
//! - The client → server direction is unchanged: instructions as text.
//!
//! Negotiation is described with [`WEBSOCKET_SUBPROTOCOL`] and
//! [`HANDSHAKE_ARG`].

/// WebSocket subprotocol under which the gateway sends binary messages made
/// of records. A client offers it together with `guacamole`; the gateway
/// selects it when it supports it, and `guacamole` (text only) otherwise.
pub const WEBSOCKET_SUBPROTOCOL: &str = "molcajete-binary-v1";

/// Connection parameter through which gateway and molcajeted agree on
/// records. molcajeted lists it last in `args`; a gateway that wants records
/// answers [`HANDSHAKE_VERSION`] in `connect`, and molcajeted then switches
/// to records right after `ready`. Stock guacd never lists the parameter,
/// and a stock web application sends an empty value for a parameter it has
/// no setting for, so both keep the text protocol.
pub const HANDSHAKE_ARG: &str = "molcajete-binary";

/// The value of [`HANDSHAKE_ARG`] that selects the framing of this module.
pub const HANDSHAKE_VERSION: &str = "1";

pub const TAG_INSTRUCTIONS: u8 = 0;
pub const TAG_BLOB: u8 = 1;

/// Bytes before the body of a record (tag and length).
pub const HEADER_LEN: usize = 5;

/// Bytes before the data of a blob record (header and stream index).
pub const BLOB_HEADER_LEN: usize = HEADER_LEN + 4;

/// Largest record a [`RecordWriter`] produces, header included. Larger
/// stream data is split over several blob records, so that a relay can keep
/// its WebSocket messages around this size: a slow link then still delivers
/// a message every so often, which the client's liveness check counts on.
pub const MAX_WRITTEN_RECORD: usize = 64 * 1024;

/// Largest data of one written blob record.
pub const MAX_BLOB_DATA: usize = MAX_WRITTEN_RECORD - BLOB_HEADER_LEN;

/// Largest body a [`RecordReader`] accepts by default.
pub const MAX_RECORD_BODY: usize = 16 * 1024 * 1024;

/// A decoded record, borrowing the buffer it was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record<'a> {
    /// Guacamole protocol text.
    Instructions(&'a [u8]),
    /// Raw data of a stream.
    Blob { stream: i32, data: &'a [u8] },
    /// A record of a kind this implementation does not know; to be skipped.
    Unknown { tag: u8, body: &'a [u8] },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error("record of {0} bytes exceeds the limit of {1} bytes")]
    TooLong(usize, usize),
    #[error("blob record without a stream index")]
    BlobTooShort,
    #[error("data ends inside a record")]
    Truncated,
}

/// Decodes the header at the start of `data`: the tag and the body length.
/// `None` when fewer than [`HEADER_LEN`] bytes are available.
pub fn parse_header(data: &[u8]) -> Option<(u8, usize)> {
    let header = data.get(..HEADER_LEN)?;
    let length = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
    Some((header[0], length as usize))
}

fn decode(tag: u8, body: &[u8]) -> Result<Record<'_>, RecordError> {
    match tag {
        TAG_INSTRUCTIONS => Ok(Record::Instructions(body)),
        TAG_BLOB => {
            let (stream, data) = body.split_at_checked(4).ok_or(RecordError::BlobTooShort)?;
            let stream = u32::from_le_bytes([stream[0], stream[1], stream[2], stream[3]]);
            Ok(Record::Blob {
                stream: stream as i32,
                data,
            })
        }
        tag => Ok(Record::Unknown { tag, body }),
    }
}

/// Builds a sequence of records in memory.
#[derive(Debug, Default, Clone)]
pub struct RecordWriter {
    buf: Vec<u8>,
    /// Offset of the instruction record still accepting text, if the last
    /// record is one.
    open: Option<usize>,
}

impl RecordWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The records written so far.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.open = None;
    }

    /// Takes the records written so far, leaving the writer empty.
    pub fn take(&mut self) -> Vec<u8> {
        self.open = None;
        std::mem::take(&mut self.buf)
    }

    /// Appends whole instructions. Text added by consecutive calls shares a
    /// record until that record would exceed [`MAX_WRITTEN_RECORD`].
    pub fn instructions(&mut self, text: &[u8]) {
        if text.is_empty() {
            return;
        }
        let start = match self.open {
            Some(start) if self.buf.len() - start + text.len() <= MAX_WRITTEN_RECORD => start,
            _ => {
                let start = self.buf.len();
                self.buf.push(TAG_INSTRUCTIONS);
                self.buf.extend_from_slice(&[0; 4]);
                self.open = Some(start);
                start
            }
        };
        self.buf.extend_from_slice(text);
        let length = (self.buf.len() - start - HEADER_LEN) as u32;
        self.buf[start + 1..start + HEADER_LEN].copy_from_slice(&length.to_le_bytes());
    }

    /// Appends stream data, in as many blob records as [`MAX_BLOB_DATA`]
    /// requires (none for empty data).
    pub fn blob(&mut self, stream: i32, data: &[u8]) {
        self.blob_aligned(stream, data, 1);
    }

    /// Like [`RecordWriter::blob`], but data is only split at multiples of
    /// `unit` bytes (for example whole PCM frames).
    pub fn blob_aligned(&mut self, stream: i32, data: &[u8], unit: usize) {
        let unit = unit.clamp(1, MAX_BLOB_DATA);
        self.open = None;
        for chunk in data.chunks(MAX_BLOB_DATA - MAX_BLOB_DATA % unit) {
            self.buf.push(TAG_BLOB);
            self.buf
                .extend_from_slice(&(chunk.len() as u32 + 4).to_le_bytes());
            self.buf.extend_from_slice(&(stream as u32).to_le_bytes());
            self.buf.extend_from_slice(chunk);
        }
    }
}

/// Incremental reader of a byte stream of records (the TCP connection
/// between gateway and molcajeted). Data may arrive in arbitrary pieces.
pub struct RecordReader {
    buf: Vec<u8>,
    /// Start of the first record not handed out yet.
    start: usize,
    max_body: usize,
}

impl Default for RecordReader {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordReader {
    pub fn new() -> Self {
        Self::with_max_body(MAX_RECORD_BODY)
    }

    pub fn with_max_body(max_body: usize) -> Self {
        Self {
            buf: Vec::with_capacity(MAX_WRITTEN_RECORD),
            start: 0,
            max_body,
        }
    }

    /// Appends received bytes.
    pub fn feed(&mut self, data: &[u8]) {
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(data);
    }

    /// Number of buffered bytes that do not yet form a complete record.
    pub fn pending_len(&self) -> usize {
        self.buf.len() - self.start
    }

    /// The next complete record, `Ok(None)` if more data is needed.
    pub fn next_record(&mut self) -> Result<Option<RawRecord<'_>>, RecordError> {
        let Some((_, length)) = parse_header(&self.buf[self.start..]) else {
            return Ok(None);
        };
        if length > self.max_body {
            return Err(RecordError::TooLong(length, self.max_body));
        }
        let end = self.start + HEADER_LEN + length;
        if self.buf.len() < end {
            return Ok(None);
        }
        let bytes = &self.buf[self.start..end];
        if bytes[0] == TAG_BLOB && length < 4 {
            return Err(RecordError::BlobTooShort);
        }
        self.start = end;
        Ok(Some(RawRecord { bytes }))
    }
}

/// A complete record inside a buffer.
#[derive(Debug, Clone, Copy)]
pub struct RawRecord<'a> {
    bytes: &'a [u8],
}

impl<'a> RawRecord<'a> {
    /// The encoded record, header included, for forwarding it unchanged.
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub fn record(&self) -> Record<'a> {
        decode(self.bytes[0], &self.bytes[HEADER_LEN..])
            .expect("blob records are checked when they are read")
    }
}

/// Iterates over the records of a buffer that holds whole records only (a
/// WebSocket message).
pub fn records(message: &[u8]) -> Records<'_> {
    Records { rest: message }
}

pub struct Records<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Records<'a> {
    type Item = Result<Record<'a>, RecordError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let item = parse_header(self.rest)
            .and_then(|(tag, length)| {
                let end = HEADER_LEN.checked_add(length)?;
                let body = self.rest.get(HEADER_LEN..end)?;
                self.rest = &self.rest[end..];
                Some(decode(tag, body))
            })
            .unwrap_or(Err(RecordError::Truncated));
        if item.is_err() {
            self.rest = &[];
        }
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Instruction text, a blob with every byte value, more text, an
    /// unknown record and a one-byte blob.
    fn sample() -> (Vec<u8>, Vec<Record<'static>>) {
        static DATA: [u8; 256] = {
            let mut data = [0u8; 256];
            let mut i = 0;
            while i < 256 {
                data[i] = i as u8;
                i += 1;
            }
            data
        };
        let mut writer = RecordWriter::new();
        writer.instructions(b"3.img,1.3,2.14,1.0,9.image/png,1.0,1.0;");
        writer.blob(3, &DATA);
        writer.instructions(b"3.end,1.3;");
        writer.instructions("4.name,3.한국어;".as_bytes());
        let mut bytes = writer.take();
        bytes.extend_from_slice(&[0x7F, 2, 0, 0, 0, b'h', b'i']);
        let mut writer = RecordWriter::new();
        writer.blob(i32::MAX, b"z");
        bytes.extend_from_slice(writer.as_bytes());
        let expected = vec![
            Record::Instructions(b"3.img,1.3,2.14,1.0,9.image/png,1.0,1.0;"),
            Record::Blob {
                stream: 3,
                data: &DATA,
            },
            Record::Instructions("3.end,1.3;4.name,3.한국어;".as_bytes()),
            Record::Unknown {
                tag: 0x7F,
                body: b"hi",
            },
        ];
        (bytes, expected)
    }

    fn drain(reader: &mut RecordReader, out: &mut Vec<(u8, Vec<u8>)>) {
        while let Some(raw) = reader.next_record().unwrap() {
            let (tag, length) = parse_header(raw.as_bytes()).unwrap();
            assert_eq!(raw.as_bytes().len(), HEADER_LEN + length);
            out.push((tag, raw.as_bytes()[HEADER_LEN..].to_vec()));
        }
    }

    fn owned(records: &[Record<'_>]) -> Vec<(u8, Vec<u8>)> {
        records
            .iter()
            .map(|record| match *record {
                Record::Instructions(text) => (TAG_INSTRUCTIONS, text.to_vec()),
                Record::Blob { stream, data } => {
                    let mut body = (stream as u32).to_le_bytes().to_vec();
                    body.extend_from_slice(data);
                    (TAG_BLOB, body)
                }
                Record::Unknown { tag, body } => (tag, body.to_vec()),
            })
            .collect()
    }

    #[test]
    fn layout_is_tag_length_body() {
        let mut writer = RecordWriter::new();
        writer.instructions(b"3.nop;");
        writer.blob(0x0102_0304, &[0xFF, 0x00, 0x80]);
        assert_eq!(
            writer.as_bytes(),
            [
                &[0u8, 6, 0, 0, 0][..],
                b"3.nop;",
                &[1, 7, 0, 0, 0, 4, 3, 2, 1, 0xFF, 0x00, 0x80],
            ]
            .concat()
        );
    }

    #[test]
    fn message_iterator_decodes_what_the_writer_wrote() {
        let (bytes, expected) = sample();
        let mut all: Vec<_> = records(&bytes).collect::<Result<_, _>>().unwrap();
        assert_eq!(
            all.pop(),
            Some(Record::Blob {
                stream: i32::MAX,
                data: b"z"
            })
        );
        assert_eq!(all, expected);
    }

    #[test]
    fn reader_survives_a_split_at_every_byte() {
        let (bytes, expected) = sample();
        let mut expected = owned(&expected);
        expected.push((TAG_BLOB, [&i32::MAX.to_le_bytes()[..], b"z"].concat()));
        for split in 0..=bytes.len() {
            let mut reader = RecordReader::new();
            let mut got = Vec::new();
            reader.feed(&bytes[..split]);
            drain(&mut reader, &mut got);
            reader.feed(&bytes[split..]);
            drain(&mut reader, &mut got);
            assert_eq!(got, expected, "split at {split}");
            assert_eq!(reader.pending_len(), 0);
        }
    }

    #[test]
    fn reader_survives_byte_by_byte_feeding() {
        let (bytes, expected) = sample();
        let mut reader = RecordReader::new();
        let mut got = Vec::new();
        for byte in &bytes {
            reader.feed(std::slice::from_ref(byte));
            drain(&mut reader, &mut got);
        }
        assert_eq!(got.len(), expected.len() + 1);
        assert_eq!(got[..expected.len()], owned(&expected));
    }

    #[test]
    fn large_data_is_split_into_bounded_records() {
        let data: Vec<u8> = (0..MAX_BLOB_DATA * 2 + 10).map(|i| i as u8).collect();
        let mut writer = RecordWriter::new();
        writer.blob(9, &data);
        let mut joined = Vec::new();
        let mut count = 0;
        for record in records(writer.as_bytes()) {
            let Record::Blob { stream: 9, data } = record.unwrap() else {
                panic!("expected a blob of stream 9");
            };
            assert!(data.len() + BLOB_HEADER_LEN <= MAX_WRITTEN_RECORD);
            joined.extend_from_slice(data);
            count += 1;
        }
        assert_eq!(count, 3);
        assert_eq!(joined, data);
    }

    #[test]
    fn aligned_data_is_split_at_whole_units() {
        let data = vec![7u8; MAX_BLOB_DATA + 100];
        let mut writer = RecordWriter::new();
        writer.blob_aligned(1, &data, 6);
        let sizes: Vec<usize> = records(writer.as_bytes())
            .map(|record| match record.unwrap() {
                Record::Blob { data, .. } => data.len(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(sizes.iter().sum::<usize>(), data.len());
        assert_eq!(sizes.len(), 2);
        assert_eq!(sizes[0] % 6, 0);
    }

    #[test]
    fn instruction_records_are_bounded_too() {
        let mut writer = RecordWriter::new();
        let instruction = b"4.sync,10.1234567890;";
        let mut expected = Vec::new();
        for _ in 0..(MAX_WRITTEN_RECORD * 3 / instruction.len()) {
            writer.instructions(instruction);
            expected.extend_from_slice(instruction);
        }
        let mut joined = Vec::new();
        let mut count = 0;
        for record in records(writer.as_bytes()) {
            let Record::Instructions(text) = record.unwrap() else {
                panic!("expected instructions");
            };
            assert!(text.len() + HEADER_LEN <= MAX_WRITTEN_RECORD);
            // Whole instructions in every record.
            assert_eq!(text.len() % instruction.len(), 0);
            joined.extend_from_slice(text);
            count += 1;
        }
        assert_eq!(count, 4);
        assert_eq!(joined, expected);
    }

    #[test]
    fn rejects_oversized_and_malformed_records() {
        let mut reader = RecordReader::with_max_body(16);
        reader.feed(&[TAG_INSTRUCTIONS, 17, 0, 0, 0]);
        assert_eq!(
            reader.next_record().unwrap_err(),
            RecordError::TooLong(17, 16)
        );

        let mut reader = RecordReader::new();
        reader.feed(&[TAG_BLOB, 3, 0, 0, 0, 1, 2, 3]);
        assert_eq!(reader.next_record().unwrap_err(), RecordError::BlobTooShort);

        let truncated = [TAG_INSTRUCTIONS, 6, 0, 0, 0, b'3', b'.'];
        assert_eq!(
            records(&truncated).collect::<Vec<_>>(),
            [Err(RecordError::Truncated)]
        );
        assert_eq!(
            records(&[TAG_BLOB, 1, 0, 0, 0, 9]).collect::<Vec<_>>(),
            [Err(RecordError::BlobTooShort)]
        );
    }
}

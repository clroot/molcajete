//! Just enough elementary-stream parsing to classify encoder output: Annex B
//! NAL units (H.264, H.265) and low-overhead AV1 OBUs.
//!
//! Used to tell key frames apart, to check that key frames carry the codec
//! configuration in-band, and to restore it when an encoder leaves it out.

use super::VideoCodec;

/// Four-byte Annex B start code.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

const H264_IDR: u8 = 5;
const H264_SPS: u8 = 7;
const H264_PPS: u8 = 8;
const H264_AUD: u8 = 9;

const H265_IRAP: std::ops::RangeInclusive<u8> = 16..=21;
const H265_VPS: u8 = 32;
const H265_SPS: u8 = 33;
const H265_PPS: u8 = 34;
const H265_AUD: u8 = 35;

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_FRAME: u8 = 6;
/// A complete temporal delimiter OBU (with an empty size field).
const TEMPORAL_DELIMITER: [u8; 2] = [(OBU_TEMPORAL_DELIMITER << 3) | 0b010, 0];

/// Index of the `01` byte that ends the next `00 00 01` at or after `from`.
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 2 < data.len() {
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i + 2] == 1 && data[i] == 0 && data[i + 1] == 0 {
            return Some(i + 2);
        } else {
            i += 1;
        }
    }
    None
}

/// NAL units of an Annex B byte stream, as `(offset, unit)` without start
/// codes or trailing zero bytes.
pub(crate) fn nal_units(data: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    let mut next = find_start_code(data, 0).map(|one| one + 1);
    std::iter::from_fn(move || {
        let start = next?;
        let end = match find_start_code(data, start) {
            Some(one) => {
                next = Some(one + 1);
                one - 2
            }
            None => {
                next = None;
                data.len()
            }
        };
        let mut unit = &data[start..end.max(start)];
        while let [rest @ .., 0] = unit {
            unit = rest;
        }
        Some((start, unit))
    })
}

fn h264_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1f)
}

fn h265_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| (b >> 1) & 0x3f)
}

/// One OBU: its type, all its bytes and its payload.
#[derive(Debug)]
pub(crate) struct Obu<'a> {
    pub kind: u8,
    pub bytes: &'a [u8],
    pub payload: &'a [u8],
}

fn leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &byte) in data.iter().take(8).enumerate() {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// OBUs of a low-overhead AV1 bitstream; stops at the first malformed one.
pub(crate) fn obus(data: &[u8]) -> impl Iterator<Item = Obu<'_>> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let rest = data.get(pos..).filter(|rest| !rest.is_empty())?;
        let header = rest[0];
        let kind = (header >> 3) & 0x0f;
        let mut header_len = if header & 0b100 != 0 { 2 } else { 1 };
        let payload_len = if header & 0b010 != 0 {
            let (size, len) = leb128(rest.get(header_len..)?)?;
            header_len += len;
            usize::try_from(size).ok()?
        } else {
            rest.len().checked_sub(header_len)?
        };
        let total = header_len.checked_add(payload_len)?;
        let bytes = rest.get(..total)?;
        let obu = Obu {
            kind,
            bytes,
            payload: &bytes[header_len..],
        };
        pos += total;
        Some(obu)
    })
}

/// Whether an AV1 temporal unit starts with a key frame.
fn av1_is_key(tu: &[u8]) -> bool {
    let mut reduced_still_picture = false;
    for obu in obus(tu) {
        match obu.kind {
            OBU_SEQUENCE_HEADER => {
                // seq_profile (3 bits), still_picture (1), reduced_still_picture_header (1)
                reduced_still_picture = obu.payload.first().is_some_and(|b| b & 0b0000_1000 != 0);
            }
            OBU_FRAME_HEADER | OBU_FRAME => {
                if reduced_still_picture {
                    return true;
                }
                // show_existing_frame (1 bit), frame_type (2 bits, KEY_FRAME = 0)
                return obu.payload.first().is_some_and(|b| b & 0b1110_0000 == 0);
            }
            _ => {}
        }
    }
    false
}

/// Whether an access unit / temporal unit can start decoding.
pub(crate) fn is_key_frame(codec: VideoCodec, data: &[u8]) -> bool {
    match codec {
        VideoCodec::H264 => nal_units(data).any(|(_, nal)| h264_type(nal) == Some(H264_IDR)),
        VideoCodec::H265 => {
            nal_units(data).any(|(_, nal)| h265_type(nal).is_some_and(|t| H265_IRAP.contains(&t)))
        }
        VideoCodec::Av1 => av1_is_key(data),
    }
}

/// Unit types that make up the in-band codec configuration.
fn config_types(codec: VideoCodec) -> &'static [u8] {
    match codec {
        VideoCodec::H264 => &[H264_SPS, H264_PPS],
        VideoCodec::H265 => &[H265_VPS, H265_SPS, H265_PPS],
        VideoCodec::Av1 => &[OBU_SEQUENCE_HEADER],
    }
}

/// Configuration units of `data` as `(type, bytes)`; the bytes include
/// their start code (Annex B) or OBU header.
fn config_units(codec: VideoCodec, data: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let wanted = config_types(codec);
    match codec {
        VideoCodec::H264 | VideoCodec::H265 => nal_units(data)
            .filter_map(|(_, nal)| {
                let kind = if codec == VideoCodec::H264 {
                    h264_type(nal)
                } else {
                    h265_type(nal)
                }?;
                wanted
                    .contains(&kind)
                    .then(|| (kind, [&START_CODE[..], nal].concat()))
            })
            .collect(),
        VideoCodec::Av1 => obus(data)
            .filter(|obu| wanted.contains(&obu.kind))
            .map(|obu| (obu.kind, obu.bytes.to_vec()))
            .collect(),
    }
}

/// Whether `data` carries the complete codec configuration in-band (SPS+PPS,
/// VPS+SPS+PPS, or an AV1 sequence header).
pub(crate) fn has_config(codec: VideoCodec, data: &[u8]) -> bool {
    let units = config_units(codec, data);
    config_types(codec)
        .iter()
        .all(|kind| units.iter().any(|(k, _)| k == kind))
}

/// Byte offset at which configuration units may be inserted: after a
/// leading access unit / temporal delimiter, else at the start.
fn insert_offset(codec: VideoCodec, data: &[u8]) -> usize {
    match codec {
        VideoCodec::H264 | VideoCodec::H265 => {
            let mut units = nal_units(data);
            match units.next() {
                Some((offset, nal)) => {
                    let aud = if codec == VideoCodec::H264 {
                        H264_AUD
                    } else {
                        H265_AUD
                    };
                    let is_aud = if codec == VideoCodec::H264 {
                        h264_type(nal)
                    } else {
                        h265_type(nal)
                    } == Some(aud);
                    if is_aud { offset + nal.len() } else { 0 }
                }
                None => 0,
            }
        }
        VideoCodec::Av1 => match obus(data).next() {
            Some(obu) if obu.kind == OBU_TEMPORAL_DELIMITER => obu.bytes.len(),
            _ => 0,
        },
    }
}

/// Normalises encoder output into the form the browser expects: makes sure
/// every AV1 temporal unit starts with a temporal delimiter.
pub(crate) fn normalize(codec: VideoCodec, mut data: Vec<u8>) -> Vec<u8> {
    if codec == VideoCodec::Av1
        && obus(&data)
            .next()
            .is_none_or(|obu| obu.kind != OBU_TEMPORAL_DELIMITER)
    {
        data.splice(0..0, TEMPORAL_DELIMITER);
    }
    data
}

/// Remembers the configuration seen on key frames and restores it on key
/// frames that come without (some encoders only emit it once).
pub(crate) struct InbandConfig {
    codec: VideoCodec,
    /// Serialised configuration units, ready to insert.
    units: Vec<u8>,
}

impl InbandConfig {
    pub(crate) fn new(codec: VideoCodec) -> Self {
        Self {
            codec,
            units: Vec::new(),
        }
    }

    /// Returns `data` with in-band configuration if it is a key frame.
    pub(crate) fn apply(&mut self, mut data: Vec<u8>, key: bool) -> Vec<u8> {
        if !key {
            return data;
        }
        if has_config(self.codec, &data) {
            self.units = config_units(self.codec, &data)
                .into_iter()
                .flat_map(|(_, bytes)| bytes)
                .collect();
        } else if !self.units.is_empty() {
            let at = insert_offset(self.codec, &data);
            data.splice(at..at, self.units.iter().copied());
        }
        data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annexb(units: &[&[u8]]) -> Vec<u8> {
        units
            .iter()
            .flat_map(|u| [&START_CODE[..], u].concat())
            .collect()
    }

    #[test]
    fn splits_nal_units_with_both_start_code_lengths() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, // SPS, 4-byte start code
            0, 0, 1, 0x68, 3, // PPS, 3-byte start code
            0, 0, 0, 1, 0x65, 4, 0, 0, 5,
            0, // IDR with an emulation-like pattern and trailing zero
        ];
        let units: Vec<_> = nal_units(&data).collect();
        assert_eq!(
            units,
            [
                (4, &[0x67, 1, 2][..]),
                (10, &[0x68, 3][..]),
                (16, &[0x65, 4, 0, 0, 5][..])
            ]
        );
        assert_eq!(nal_units(&[1, 2, 3]).count(), 0);
        assert_eq!(nal_units(&[]).count(), 0);
    }

    #[test]
    fn classifies_h264() {
        let idr = annexb(&[&[0x09, 0xf0], &[0x67, 0x42], &[0x68, 0xce], &[0x65, 0x88]]);
        assert!(is_key_frame(VideoCodec::H264, &idr));
        assert!(has_config(VideoCodec::H264, &idr));
        let p = annexb(&[&[0x41, 0x9a]]);
        assert!(!is_key_frame(VideoCodec::H264, &p));
        assert!(!has_config(VideoCodec::H264, &p));
    }

    #[test]
    fn classifies_h265() {
        // NAL header: type << 1, then layer/tid byte 0x01.
        let idr = annexb(&[
            &[0x40, 1, 7],
            &[0x42, 1, 8],
            &[0x44, 1, 9],
            &[0x26, 1, 0xaf],
        ]);
        assert!(is_key_frame(VideoCodec::H265, &idr));
        assert!(has_config(VideoCodec::H265, &idr));
        let trail = annexb(&[&[0x02, 1, 0xd0]]);
        assert!(!is_key_frame(VideoCodec::H265, &trail));
        let without_vps = annexb(&[&[0x42, 1, 8], &[0x44, 1, 9], &[0x26, 1, 0xaf]]);
        assert!(!has_config(VideoCodec::H265, &without_vps));
    }

    #[test]
    fn parses_obus_and_classifies_av1() {
        let seq = [0x0a, 0x03, 0x00, 0x00, 0x00];
        let key_frame = [0x32, 0x02, 0x10, 0x00]; // OBU_FRAME, frame_type KEY
        let inter_frame = [0x32, 0x02, 0x30, 0x00]; // frame_type INTER
        let key = [&TEMPORAL_DELIMITER[..], &seq, &key_frame].concat();
        let obu_kinds: Vec<_> = obus(&key)
            .map(|o| (o.kind, o.bytes.len(), o.payload.len()))
            .collect();
        assert_eq!(obu_kinds, [(2, 2, 0), (1, 5, 3), (6, 4, 2)]);
        assert!(is_key_frame(VideoCodec::Av1, &key));
        assert!(has_config(VideoCodec::Av1, &key));
        let inter = [&TEMPORAL_DELIMITER[..], &inter_frame].concat();
        assert!(!is_key_frame(VideoCodec::Av1, &inter));
        // Truncated OBUs end the iteration instead of panicking.
        assert_eq!(obus(&[0x32, 0x05, 0x10]).count(), 0);
        assert_eq!(obus(&[0x32, 0xff]).count(), 0);
    }

    #[test]
    fn normalize_adds_missing_temporal_delimiters() {
        let frame = vec![0x32, 0x02, 0x10, 0x00];
        let fixed = normalize(VideoCodec::Av1, frame.clone());
        assert_eq!(fixed, [&TEMPORAL_DELIMITER[..], &frame].concat());
        assert_eq!(normalize(VideoCodec::Av1, fixed.clone()), fixed);
        assert_eq!(normalize(VideoCodec::H264, frame.clone()), frame);
    }

    #[test]
    fn restores_missing_configuration_after_delimiters() {
        let mut inband = InbandConfig::new(VideoCodec::H264);
        let first = annexb(&[&[0x09, 0xf0], &[0x67, 0x42], &[0x68, 0xce], &[0x65, 0x88]]);
        assert_eq!(inband.apply(first.clone(), true), first);
        let p = annexb(&[&[0x09, 0x30], &[0x41, 0x9a]]);
        assert_eq!(inband.apply(p.clone(), false), p);
        let bare_idr = annexb(&[&[0x09, 0xf0], &[0x65, 0x77]]);
        let restored = inband.apply(bare_idr, true);
        assert_eq!(
            restored,
            annexb(&[&[0x09, 0xf0], &[0x67, 0x42], &[0x68, 0xce], &[0x65, 0x77]])
        );

        let mut inband = InbandConfig::new(VideoCodec::Av1);
        let seq = [0x0a, 0x01, 0x00];
        let frame = [0x32, 0x02, 0x10, 0x00];
        inband.apply([&TEMPORAL_DELIMITER[..], &seq, &frame].concat(), true);
        let restored = inband.apply([&TEMPORAL_DELIMITER[..], &frame].concat(), true);
        assert_eq!(restored, [&TEMPORAL_DELIMITER[..], &seq, &frame].concat());
    }
}

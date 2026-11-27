//! The device redirection channel: IronRDP's `Rdpdr`, with the names of
//! announced drives written the way Windows servers read them.
//!
//! [MS-RDPEFS] 2.2.3.1 asks for a drive's full name in DeviceData as a
//! null-terminated Unicode string, and `ironrdp-rdpdr` writes it so
//! (UTF-16LE). Windows Server reads the field as a byte string and stops at
//! the first zero byte, so `molcajete` showed as `\\tsclient\m`. FreeRDP
//! writes the name one byte per character, which Windows shows in full;
//! the announcement is rewritten the same way, with the name in UTF-8.

use ironrdp_core::impl_as_any;
use ironrdp_pdu::PduResult;
use ironrdp_pdu::gcc::ChannelName;
use ironrdp_rdpdr::Rdpdr;
use ironrdp_svc::{CompressionCondition, SvcClientProcessor, SvcMessage, SvcProcessor};

/// RDPDR_CTYP_CORE, PAKID_CORE_DEVICELIST_ANNOUNCE.
const DEVICE_LIST_ANNOUNCE: [u8; 4] = [0x72, 0x44, 0x41, 0x44];
/// RDPDR_DTYP_FILESYSTEM.
const FILESYSTEM: u32 = 0x08;

#[derive(Debug)]
pub struct RdpdrChannel {
    rdpdr: Rdpdr,
}

impl_as_any!(RdpdrChannel);

impl RdpdrChannel {
    pub fn new(rdpdr: Rdpdr) -> Self {
        Self { rdpdr }
    }

    #[cfg(test)]
    pub fn rdpdr(&self) -> &Rdpdr {
        &self.rdpdr
    }

    pub fn rdpdr_mut(&mut self) -> &mut Rdpdr {
        &mut self.rdpdr
    }
}

impl SvcProcessor for RdpdrChannel {
    fn channel_name(&self) -> ChannelName {
        self.rdpdr.channel_name()
    }

    fn compression_condition(&self) -> CompressionCondition {
        self.rdpdr.compression_condition()
    }

    fn start(&mut self) -> PduResult<Vec<SvcMessage>> {
        Ok(byte_drive_names(self.rdpdr.start()?))
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        Ok(byte_drive_names(self.rdpdr.process(payload)?))
    }
}

impl SvcClientProcessor for RdpdrChannel {}

fn byte_drive_names(messages: Vec<SvcMessage>) -> Vec<SvcMessage> {
    messages
        .into_iter()
        .map(|message| {
            match message
                .encode_unframed_pdu()
                .ok()
                .and_then(|pdu| rewrite_announce(&pdu))
            {
                Some(rewritten) => SvcMessage::from(rewritten),
                None => message,
            }
        })
        .collect()
}

/// The device list announcement `pdu` with each drive's DeviceData as a
/// null-terminated UTF-8 string; `None` if `pdu` is something else or
/// announces no drive.
fn rewrite_announce(pdu: &[u8]) -> Option<Vec<u8>> {
    if pdu.get(..4)? != DEVICE_LIST_ANNOUNCE {
        return None;
    }
    let count = u32_at(pdu, 4)?;
    let mut out = pdu[..8].to_vec();
    let mut rest = &pdu[8..];
    let mut changed = false;
    for _ in 0..count {
        // DeviceType, DeviceId, PreferredDosName (8), DeviceDataLength.
        let device_type = u32_at(rest, 0)?;
        let length = usize::try_from(u32_at(rest, 16)?).ok()?;
        let data = rest.get(20..20 + length)?;
        out.extend_from_slice(&rest[..16]);
        match (device_type == FILESYSTEM)
            .then(|| utf16_name(data))
            .flatten()
        {
            Some(name) => {
                let mut bytes = name.into_bytes();
                bytes.push(0);
                out.extend_from_slice(&u32::try_from(bytes.len()).ok()?.to_le_bytes());
                out.extend_from_slice(&bytes);
                changed = true;
            }
            None => out.extend_from_slice(&rest[16..20 + length]),
        }
        rest = &rest[20 + length..];
    }
    if !rest.is_empty() || !changed {
        return None;
    }
    Some(out)
}

/// The UTF-16LE string `data` holds before its terminating NUL.
fn utf16_name(data: &[u8]) -> Option<String> {
    if !data.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
        .take_while(|&unit| unit != 0)
        .collect();
    String::from_utf16(&units).ok()
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

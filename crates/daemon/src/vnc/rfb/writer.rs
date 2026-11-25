//! Client-to-server messages.

use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;

use crate::jump::UpstreamWriter;

use super::{RfbError, SharedState, lock_shared, pixel};

const SET_PIXEL_FORMAT: u8 = 0;
const SET_ENCODINGS: u8 = 2;
const FRAMEBUFFER_UPDATE_REQUEST: u8 = 3;
const KEY_EVENT: u8 = 4;
const POINTER_EVENT: u8 = 5;
const CLIENT_CUT_TEXT: u8 = 6;
const SET_DESKTOP_SIZE: u8 = 251;

/// Write half of an RFB connection.
pub struct RfbWriter {
    output: UpstreamWriter,
    shared: Arc<Mutex<SharedState>>,
}

impl RfbWriter {
    pub(super) fn new(output: UpstreamWriter, shared: Arc<Mutex<SharedState>>) -> Self {
        Self { output, shared }
    }

    /// Sends a FramebufferUpdateRequest for the given region.
    pub async fn request_update(
        &mut self,
        incremental: bool,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    ) -> Result<(), RfbError> {
        self.send(&encode_update_request(incremental, x, y, width, height))
            .await
    }

    /// Sends a PointerEvent.
    ///
    /// mask bits: 1 left, 2 middle, 4 right, 8 wheel up, 16 wheel down (same as Guacamole's
    /// mouse mask)
    pub async fn pointer(&mut self, x: u16, y: u16, mask: u8) -> Result<(), RfbError> {
        self.send(&encode_pointer(x, y, mask)).await
    }

    /// Sends a KeyEvent for an X11 keysym.
    pub async fn key(&mut self, keysym: u32, down: bool) -> Result<(), RfbError> {
        self.send(&encode_key(keysym, down)).await
    }

    /// ClientCutText; characters outside Latin-1 are replaced with '?'.
    pub async fn cut_text(&mut self, text: &str) -> Result<(), RfbError> {
        self.send(&encode_cut_text(text)?).await
    }

    /// Asks the server to resize the framebuffer with SetDesktopSize (ExtendedDesktopSize).
    ///
    /// Returns `Ok(false)` without sending anything if the server has not announced support.
    /// Support is only known once the reader has processed the server's first
    /// ExtendedDesktopSize rectangle, which servers send in the first FramebufferUpdate after
    /// SetEncodings. The outcome arrives later as an [`super::RfbEvent::Resize`] (on success)
    /// from the reader.
    pub async fn set_desktop_size(&mut self, width: u16, height: u16) -> Result<bool, RfbError> {
        if width == 0 || height == 0 {
            return Err(RfbError::Protocol(format!(
                "invalid desktop size {width}x{height}"
            )));
        }
        let screen = {
            let shared = lock_shared(&self.shared);
            if !shared.extended_desktop_size {
                return Ok(false);
            }
            shared.screen.unwrap_or_default()
        };
        self.send(&encode_set_desktop_size(
            width,
            height,
            screen.id,
            screen.flags,
        ))
        .await?;
        Ok(true)
    }

    async fn send(&mut self, message: &[u8]) -> Result<(), RfbError> {
        self.output.write_all(message).await?;
        Ok(())
    }
}

pub(super) fn encode_set_pixel_format() -> [u8; 20] {
    let mut message = [0u8; 20];
    message[0] = SET_PIXEL_FORMAT;
    message[4..].copy_from_slice(&pixel::PIXEL_FORMAT);
    message
}

pub(super) fn encode_set_encodings(encodings: &[i32]) -> Result<Vec<u8>, RfbError> {
    let count = u16::try_from(encodings.len())
        .map_err(|_| RfbError::Protocol("too many encodings".into()))?;
    let mut message = Vec::with_capacity(4 + 4 * encodings.len());
    message.extend_from_slice(&[SET_ENCODINGS, 0]);
    message.extend_from_slice(&count.to_be_bytes());
    for encoding in encodings {
        message.extend_from_slice(&encoding.to_be_bytes());
    }
    Ok(message)
}

pub(super) fn encode_update_request(
    incremental: bool,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
) -> [u8; 10] {
    let mut message = [0u8; 10];
    message[0] = FRAMEBUFFER_UPDATE_REQUEST;
    message[1] = u8::from(incremental);
    message[2..4].copy_from_slice(&x.to_be_bytes());
    message[4..6].copy_from_slice(&y.to_be_bytes());
    message[6..8].copy_from_slice(&width.to_be_bytes());
    message[8..10].copy_from_slice(&height.to_be_bytes());
    message
}

pub(super) fn encode_pointer(x: u16, y: u16, mask: u8) -> [u8; 6] {
    let mut message = [0u8; 6];
    message[0] = POINTER_EVENT;
    message[1] = mask;
    message[2..4].copy_from_slice(&x.to_be_bytes());
    message[4..6].copy_from_slice(&y.to_be_bytes());
    message
}

pub(super) fn encode_key(keysym: u32, down: bool) -> [u8; 8] {
    let mut message = [0u8; 8];
    message[0] = KEY_EVENT;
    message[1] = u8::from(down);
    message[4..8].copy_from_slice(&keysym.to_be_bytes());
    message
}

pub(super) fn encode_cut_text(text: &str) -> Result<Vec<u8>, RfbError> {
    let latin1: Vec<u8> = text
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect();
    let length = u32::try_from(latin1.len())
        .map_err(|_| RfbError::Protocol("clipboard text too long".into()))?;
    let mut message = Vec::with_capacity(8 + latin1.len());
    message.extend_from_slice(&[CLIENT_CUT_TEXT, 0, 0, 0]);
    message.extend_from_slice(&length.to_be_bytes());
    message.extend_from_slice(&latin1);
    Ok(message)
}

/// SetDesktopSize with a single screen covering the whole framebuffer.
pub(super) fn encode_set_desktop_size(
    width: u16,
    height: u16,
    screen_id: u32,
    screen_flags: u32,
) -> [u8; 24] {
    let mut message = [0u8; 24];
    message[0] = SET_DESKTOP_SIZE;
    message[2..4].copy_from_slice(&width.to_be_bytes());
    message[4..6].copy_from_slice(&height.to_be_bytes());
    message[6] = 1; // number-of-screens
    message[8..12].copy_from_slice(&screen_id.to_be_bytes());
    // Screen x and y (bytes 12..16) stay zero.
    message[16..18].copy_from_slice(&width.to_be_bytes());
    message[18..20].copy_from_slice(&height.to_be_bytes());
    message[20..24].copy_from_slice(&screen_flags.to_be_bytes());
    message
}

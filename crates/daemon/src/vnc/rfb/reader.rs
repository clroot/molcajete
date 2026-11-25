//! Server-to-client messages.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::tcp::OwnedReadHalf;
use tracing::{debug, trace, warn};

use super::pixel::{self, BYTES_PER_PIXEL};
use super::zrle::ZrleDecoder;
use super::{RfbError, RfbEvent, Screen, SharedState, encoding, hextile, lock_shared};

const FRAMEBUFFER_UPDATE: u8 = 0;
const SET_COLOUR_MAP_ENTRIES: u8 = 1;
const BELL: u8 = 2;
const SERVER_CUT_TEXT: u8 = 3;

/// ExtendedDesktopSize reason: the change was requested by this client.
const RESIZE_REQUESTED_BY_US: u16 = 1;

/// Larger clipboard contents are discarded.
const MAX_CUT_TEXT_LEN: usize = 16 * 1024 * 1024;
/// Larger cursor images are discarded.
const MAX_CURSOR_SIZE: u16 = 1024;

/// Read half of an RFB connection, including all decoder state.
pub struct RfbReader {
    input: BufReader<OwnedReadHalf>,
    width: u16,
    height: u16,
    /// Rectangles left in the FramebufferUpdate being processed, if one is in progress.
    rects_remaining: Option<u16>,
    zrle: ZrleDecoder,
    shared: Arc<Mutex<SharedState>>,
}

/// Result of processing one rectangle.
enum RectOutcome {
    Event(RfbEvent),
    /// The rectangle carried nothing to report (e.g. an unchanged size or an empty area).
    Nothing,
    /// LastRect pseudo-encoding: the update ends here.
    LastRect,
}

impl RfbReader {
    pub(super) fn new(
        input: BufReader<OwnedReadHalf>,
        width: u16,
        height: u16,
        shared: Arc<Mutex<SharedState>>,
    ) -> Self {
        Self {
            input,
            width,
            height,
            rects_remaining: None,
            zrle: ZrleDecoder::new(),
            shared,
        }
    }

    /// Current framebuffer size, as announced by ServerInit or the latest resize.
    #[allow(dead_code)] // Not needed by guacd, which follows Resize events instead.
    pub fn framebuffer_size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// Reads server messages until one produces an event.
    ///
    /// Not cancel-safe: dropping the future part-way through a message (for example by losing
    /// a `tokio::select!` race) desynchronises the stream, so call it from a dedicated loop.
    pub async fn next_event(&mut self) -> Result<RfbEvent, RfbError> {
        loop {
            if let Some(remaining) = self.rects_remaining {
                if remaining == 0 {
                    self.rects_remaining = None;
                    return Ok(RfbEvent::UpdateEnd);
                }
                self.rects_remaining = Some(remaining - 1);
                match self.read_rect().await? {
                    RectOutcome::Event(event) => return Ok(event),
                    RectOutcome::Nothing => continue,
                    RectOutcome::LastRect => {
                        self.rects_remaining = None;
                        return Ok(RfbEvent::UpdateEnd);
                    }
                }
            }

            match self.input.read_u8().await? {
                FRAMEBUFFER_UPDATE => {
                    self.skip(1).await?;
                    self.rects_remaining = Some(self.input.read_u16().await?);
                }
                SET_COLOUR_MAP_ENTRIES => {
                    // Irrelevant for the true-colour format this client requests.
                    self.skip(3).await?;
                    let count = self.input.read_u16().await?;
                    self.skip(usize::from(count) * 6).await?;
                }
                BELL => return Ok(RfbEvent::Bell),
                SERVER_CUT_TEXT => {
                    if let Some(text) = self.read_cut_text().await? {
                        return Ok(RfbEvent::ClipboardText(text));
                    }
                }
                other => {
                    return Err(RfbError::Protocol(format!(
                        "unknown server message type {other}"
                    )));
                }
            }
        }
    }

    async fn read_rect(&mut self) -> Result<RectOutcome, RfbError> {
        let x = self.input.read_u16().await?;
        let y = self.input.read_u16().await?;
        let width = self.input.read_u16().await?;
        let height = self.input.read_u16().await?;
        let encoding = self.input.read_i32().await?;
        trace!(x, y, width, height, encoding, "rectangle");

        match encoding {
            encoding::RAW => {
                self.check_bounds(x, y, width, height)?;
                let mut rgba =
                    vec![0u8; usize::from(width) * usize::from(height) * BYTES_PER_PIXEL];
                self.input.read_exact(&mut rgba).await?;
                pixel::convert_in_place(&mut rgba);
                Ok(pixels(x, y, width, height, rgba))
            }
            encoding::COPY_RECT => {
                let src_x = self.input.read_u16().await?;
                let src_y = self.input.read_u16().await?;
                self.check_bounds(x, y, width, height)?;
                self.check_bounds(src_x, src_y, width, height)?;
                if width == 0 || height == 0 {
                    return Ok(RectOutcome::Nothing);
                }
                Ok(RectOutcome::Event(RfbEvent::CopyRect {
                    src_x,
                    src_y,
                    x,
                    y,
                    width,
                    height,
                }))
            }
            encoding::RRE => {
                self.check_bounds(x, y, width, height)?;
                let rgba = self.read_rre(width, height).await?;
                Ok(pixels(x, y, width, height, rgba))
            }
            encoding::HEXTILE => {
                self.check_bounds(x, y, width, height)?;
                let rgba = hextile::decode(&mut self.input, width, height).await?;
                Ok(pixels(x, y, width, height, rgba))
            }
            encoding::ZRLE => {
                self.check_bounds(x, y, width, height)?;
                let len = self.input.read_u32().await? as usize;
                if len > ZrleDecoder::max_compressed_len(width, height) {
                    return Err(RfbError::Protocol(format!(
                        "ZRLE payload of {len} bytes is too large for a {width}x{height} rectangle"
                    )));
                }
                self.input
                    .read_exact(self.zrle.compressed_buffer(len))
                    .await?;
                let rgba = self.zrle.decode(width, height)?;
                Ok(pixels(x, y, width, height, rgba))
            }
            encoding::CURSOR => self.read_cursor(x, y, width, height).await,
            encoding::DESKTOP_SIZE => Ok(self.resize(width, height)),
            encoding::EXTENDED_DESKTOP_SIZE => {
                self.read_extended_desktop_size(x, y, width, height).await
            }
            encoding::LAST_RECT => Ok(RectOutcome::LastRect),
            other => Err(RfbError::Protocol(format!(
                "rectangle uses unsupported encoding {other}"
            ))),
        }
    }

    fn check_bounds(&self, x: u16, y: u16, width: u16, height: u16) -> Result<(), RfbError> {
        let fits = |start: u16, len: u16, limit: u16| {
            u32::from(start) + u32::from(len) <= u32::from(limit)
        };
        if fits(x, width, self.width) && fits(y, height, self.height) {
            Ok(())
        } else {
            Err(RfbError::Protocol(format!(
                "rectangle {width}x{height} at ({x}, {y}) exceeds the {}x{} framebuffer",
                self.width, self.height
            )))
        }
    }

    async fn read_rre(&mut self, width: u16, height: u16) -> Result<Vec<u8>, RfbError> {
        let (width, height) = (usize::from(width), usize::from(height));
        let count = self.input.read_u32().await?;
        let mut background = [0u8; BYTES_PER_PIXEL];
        self.input.read_exact(&mut background).await?;

        let mut rgba = vec![0u8; width * height * 4];
        pixel::fill(
            &mut rgba,
            width,
            0,
            0,
            width,
            height,
            pixel::to_rgba(&background),
        );
        for _ in 0..count {
            let mut entry = [0u8; BYTES_PER_PIXEL + 8];
            self.input.read_exact(&mut entry).await?;
            let field =
                |offset: usize| usize::from(u16::from_be_bytes([entry[offset], entry[offset + 1]]));
            let (sub_x, sub_y, sub_width, sub_height) = (field(4), field(6), field(8), field(10));
            // Subrectangles reaching outside the rectangle are clipped.
            if sub_x >= width || sub_y >= height {
                continue;
            }
            pixel::fill(
                &mut rgba,
                width,
                sub_x,
                sub_y,
                sub_width.min(width - sub_x),
                sub_height.min(height - sub_y),
                pixel::to_rgba(&entry),
            );
        }
        Ok(rgba)
    }

    async fn read_cursor(
        &mut self,
        hotspot_x: u16,
        hotspot_y: u16,
        width: u16,
        height: u16,
    ) -> Result<RectOutcome, RfbError> {
        let (w, h) = (usize::from(width), usize::from(height));
        let mask_stride = w.div_ceil(8);
        let pixel_len = w * h * BYTES_PER_PIXEL;
        let mask_len = mask_stride * h;
        if width > MAX_CURSOR_SIZE || height > MAX_CURSOR_SIZE {
            warn!(width, height, "ignoring oversized cursor image");
            self.skip(pixel_len + mask_len).await?;
            return Ok(RectOutcome::Nothing);
        }

        let mut rgba = vec![0u8; pixel_len];
        self.input.read_exact(&mut rgba).await?;
        pixel::convert_in_place(&mut rgba);
        let mut mask = vec![0u8; mask_len];
        self.input.read_exact(&mut mask).await?;

        // An empty cursor has no rows; chunking by a zero width would panic.
        if pixel_len > 0 {
            for (pixels, mask_row) in rgba
                .chunks_exact_mut(w * 4)
                .zip(mask.chunks_exact(mask_stride))
            {
                for (column, pixel) in pixels.chunks_exact_mut(4).enumerate() {
                    if mask_row[column / 8] & (0x80 >> (column % 8)) == 0 {
                        pixel.fill(0);
                    }
                }
            }
        }
        Ok(RectOutcome::Event(RfbEvent::Cursor {
            hotspot_x,
            hotspot_y,
            width,
            height,
            rgba,
        }))
    }

    async fn read_extended_desktop_size(
        &mut self,
        reason: u16,
        status: u16,
        width: u16,
        height: u16,
    ) -> Result<RectOutcome, RfbError> {
        let count = self.input.read_u8().await?;
        self.skip(3).await?;
        let mut first_screen = None;
        for _ in 0..count {
            let mut entry = [0u8; 16];
            self.input.read_exact(&mut entry).await?;
            first_screen.get_or_insert(Screen {
                id: u32::from_be_bytes([entry[0], entry[1], entry[2], entry[3]]),
                flags: u32::from_be_bytes([entry[12], entry[13], entry[14], entry[15]]),
            });
        }

        // A non-zero status answers a failed SetDesktopSize from this client; the rectangle
        // then does not describe a new layout.
        let rejected = reason == RESIZE_REQUESTED_BY_US && status != 0;
        {
            let mut shared = lock_shared(&self.shared);
            shared.extended_desktop_size = true;
            if !rejected && first_screen.is_some() {
                shared.screen = first_screen;
            }
        }
        if rejected {
            let cause = match status {
                1 => "resize is administratively prohibited",
                2 => "out of resources",
                3 => "invalid screen layout",
                _ => "unknown error",
            };
            warn!(status, cause, "server rejected SetDesktopSize");
            return Ok(RectOutcome::Nothing);
        }
        Ok(self.resize(width, height))
    }

    fn resize(&mut self, width: u16, height: u16) -> RectOutcome {
        if (width, height) == (self.width, self.height) {
            return RectOutcome::Nothing;
        }
        debug!(width, height, "framebuffer resized");
        self.width = width;
        self.height = height;
        RectOutcome::Event(RfbEvent::Resize { width, height })
    }

    /// Reads the rest of a ServerCutText message. Returns `None` for contents that are skipped.
    async fn read_cut_text(&mut self) -> Result<Option<String>, RfbError> {
        self.skip(3).await?;
        let len = self.input.read_i32().await?;
        let Ok(len) = usize::try_from(len) else {
            // Negative lengths belong to the Extended Clipboard extension, which this client
            // never announces.
            warn!("skipping extended clipboard message");
            self.skip(len.unsigned_abs() as usize).await?;
            return Ok(None);
        };
        if len > MAX_CUT_TEXT_LEN {
            warn!(len, "skipping oversized clipboard text");
            self.skip(len).await?;
            return Ok(None);
        }
        let mut latin1 = vec![0u8; len];
        self.input.read_exact(&mut latin1).await?;
        Ok(Some(latin1.into_iter().map(char::from).collect()))
    }

    /// Discards `len` bytes of input.
    async fn skip(&mut self, mut len: usize) -> Result<(), RfbError> {
        let mut scratch = [0u8; 4096];
        while len > 0 {
            let chunk = len.min(scratch.len());
            self.input.read_exact(&mut scratch[..chunk]).await?;
            len -= chunk;
        }
        Ok(())
    }
}

/// Wraps decoded pixels in an event, using [`RfbEvent::Fill`] for single-colour content.
fn pixels(x: u16, y: u16, width: u16, height: u16, rgba: Vec<u8>) -> RectOutcome {
    if rgba.is_empty() {
        return RectOutcome::Nothing;
    }
    RectOutcome::Event(match pixel::uniform_colour(&rgba) {
        Some(colour) => RfbEvent::Fill {
            x,
            y,
            width,
            height,
            rgba: colour,
        },
        None => RfbEvent::Rect {
            x,
            y,
            width,
            height,
            rgba,
        },
    })
}

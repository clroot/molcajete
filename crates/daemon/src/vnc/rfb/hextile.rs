//! Hextile encoding (5): the rectangle is split into 16x16 tiles, each either raw or a
//! background colour overlaid with solid subrectangles.

use tokio::io::{AsyncRead, AsyncReadExt};

use super::RfbError;
use super::pixel::{self, BYTES_PER_PIXEL};

const TILE_SIZE: usize = 16;

const RAW: u8 = 1;
const BACKGROUND_SPECIFIED: u8 = 2;
const FOREGROUND_SPECIFIED: u8 = 4;
const ANY_SUBRECTS: u8 = 8;
const SUBRECTS_COLOURED: u8 = 16;

/// Reads one Hextile-encoded rectangle and returns its RGBA pixels.
pub(super) async fn decode<R>(input: &mut R, width: u16, height: u16) -> Result<Vec<u8>, RfbError>
where
    R: AsyncRead + Unpin,
{
    let (width, height) = (usize::from(width), usize::from(height));
    let mut rgba = vec![0u8; width * height * 4];
    // Background and foreground carry over from tile to tile when not respecified.
    let mut background = [0, 0, 0, 255];
    let mut foreground = [0, 0, 0, 255];
    let mut scratch = Vec::with_capacity(TILE_SIZE * TILE_SIZE * BYTES_PER_PIXEL);

    for tile_y in (0..height).step_by(TILE_SIZE) {
        let tile_height = TILE_SIZE.min(height - tile_y);
        for tile_x in (0..width).step_by(TILE_SIZE) {
            let tile_width = TILE_SIZE.min(width - tile_x);
            let flags = input.read_u8().await?;

            if flags & RAW != 0 {
                scratch.resize(tile_width * tile_height * BYTES_PER_PIXEL, 0);
                input.read_exact(&mut scratch).await?;
                pixel::convert_in_place(&mut scratch);
                for (row, source) in scratch.chunks_exact(tile_width * 4).enumerate() {
                    let start = ((tile_y + row) * width + tile_x) * 4;
                    rgba[start..start + source.len()].copy_from_slice(source);
                }
                continue;
            }

            if flags & BACKGROUND_SPECIFIED != 0 {
                background = read_pixel(input).await?;
            }
            pixel::fill(
                &mut rgba,
                width,
                tile_x,
                tile_y,
                tile_width,
                tile_height,
                background,
            );
            if flags & FOREGROUND_SPECIFIED != 0 {
                foreground = read_pixel(input).await?;
            }
            if flags & ANY_SUBRECTS == 0 {
                continue;
            }

            let count = usize::from(input.read_u8().await?);
            let coloured = flags & SUBRECTS_COLOURED != 0;
            let entry_len = if coloured { BYTES_PER_PIXEL + 2 } else { 2 };
            scratch.resize(count * entry_len, 0);
            input.read_exact(&mut scratch).await?;
            for entry in scratch.chunks_exact(entry_len) {
                let (colour, geometry) = if coloured {
                    (pixel::to_rgba(entry), &entry[BYTES_PER_PIXEL..])
                } else {
                    (foreground, entry)
                };
                let sub_x = usize::from(geometry[0] >> 4);
                let sub_y = usize::from(geometry[0] & 0x0f);
                let sub_width = usize::from(geometry[1] >> 4) + 1;
                let sub_height = usize::from(geometry[1] & 0x0f) + 1;
                // Subrectangles reaching past a partial edge tile are clipped rather than
                // treated as fatal.
                if sub_x >= tile_width || sub_y >= tile_height {
                    continue;
                }
                pixel::fill(
                    &mut rgba,
                    width,
                    tile_x + sub_x,
                    tile_y + sub_y,
                    sub_width.min(tile_width - sub_x),
                    sub_height.min(tile_height - sub_y),
                    colour,
                );
            }
        }
    }
    Ok(rgba)
}

async fn read_pixel<R>(input: &mut R) -> Result<[u8; 4], RfbError>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = [0u8; BYTES_PER_PIXEL];
    input.read_exact(&mut bytes).await?;
    Ok(pixel::to_rgba(&bytes))
}

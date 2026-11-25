//! ZRLE encoding (16): zlib-compressed 64x64 tiles using raw, solid, packed palette, plain RLE
//! or palette RLE subencodings.
//!
//! A single zlib stream spans every ZRLE rectangle of the connection, so the inflater lives as
//! long as the reader.

use flate2::{Decompress, FlushDecompress, Status};
use tracing::warn;

use super::RfbError;
use super::pixel::{self, BYTES_PER_CPIXEL};

const TILE_SIZE: usize = 64;

/// Output space added per inflate call when the buffer runs full.
const INFLATE_CHUNK: usize = 64 * 1024;

pub(super) struct ZrleDecoder {
    inflater: Decompress,
    compressed: Vec<u8>,
    inflated: Vec<u8>,
}

impl ZrleDecoder {
    pub(super) fn new() -> Self {
        Self {
            inflater: Decompress::new(true),
            compressed: Vec::new(),
            inflated: Vec::new(),
        }
    }

    /// Largest zlib payload accepted for a rectangle of the given size.
    pub(super) fn max_compressed_len(width: u16, height: u16) -> usize {
        // Deflate never expands data by more than a few bytes per 64 KiB block, so twice the
        // largest valid tile data plus some slack for flush markers is generous.
        max_inflated_len(width, height) * 2 + INFLATE_CHUNK
    }

    /// The buffer to read the rectangle's zlib payload into.
    pub(super) fn compressed_buffer(&mut self) -> &mut Vec<u8> {
        &mut self.compressed
    }

    /// Inflates the payload stored by [`Self::compressed_buffer`] and decodes it into RGBA.
    pub(super) fn decode(&mut self, width: u16, height: u16) -> Result<Vec<u8>, RfbError> {
        inflate(
            &mut self.inflater,
            &self.compressed,
            &mut self.inflated,
            max_inflated_len(width, height),
        )?;
        decode_tiles(&self.inflated, width, height)
    }
}

/// Upper bound on the inflated size of a valid rectangle. The worst case per pixel is plain RLE
/// with single-pixel runs (CPIXEL plus one length byte); every tile adds a subencoding byte and
/// at most a 127-entry palette.
fn max_inflated_len(width: u16, height: u16) -> usize {
    let (width, height) = (usize::from(width), usize::from(height));
    let tiles = width.div_ceil(TILE_SIZE) * height.div_ceil(TILE_SIZE);
    width * height * (BYTES_PER_CPIXEL + 1) + tiles * (1 + 127 * BYTES_PER_CPIXEL)
}

/// Feeds all of `input` to the persistent inflater and collects everything it produces.
fn inflate(
    inflater: &mut Decompress,
    input: &[u8],
    output: &mut Vec<u8>,
    limit: usize,
) -> Result<(), RfbError> {
    let zlib_error = |err: &dyn std::fmt::Display| RfbError::Protocol(format!("ZRLE zlib: {err}"));
    output.clear();
    let mut consumed = 0;
    loop {
        if output.capacity() - output.len() < INFLATE_CHUNK / 4 {
            output.reserve(INFLATE_CHUNK);
        }
        let (in_before, out_before) = (inflater.total_in(), inflater.total_out());
        let status = inflater
            .decompress_vec(&input[consumed..], output, FlushDecompress::Sync)
            .map_err(|err| zlib_error(&err))?;
        let used = (inflater.total_in() - in_before) as usize;
        let produced = (inflater.total_out() - out_before) as usize;
        consumed += used;

        if output.len() > limit {
            return Err(RfbError::Protocol(
                "ZRLE rectangle inflates beyond its maximum size".into(),
            ));
        }
        // Spare output space after consuming all input means nothing is left pending.
        let output_full = output.len() == output.capacity();
        if consumed == input.len() && !output_full {
            return Ok(());
        }
        if status == Status::StreamEnd {
            return Err(zlib_error(&"stream ended unexpectedly"));
        }
        if used == 0 && produced == 0 && !output_full {
            return Err(zlib_error(&"no progress on corrupt or truncated data"));
        }
    }
}

/// Decodes inflated ZRLE tile data covering a `width` x `height` rectangle.
pub(super) fn decode_tiles(data: &[u8], width: u16, height: u16) -> Result<Vec<u8>, RfbError> {
    let (width, height) = (usize::from(width), usize::from(height));
    let mut rgba = vec![0u8; width * height * 4];
    let mut input = Input { data, pos: 0 };

    for tile_y in (0..height).step_by(TILE_SIZE) {
        for tile_x in (0..width).step_by(TILE_SIZE) {
            let mut tile = Tile {
                rgba: &mut rgba,
                stride: width,
                x: tile_x,
                y: tile_y,
                width: TILE_SIZE.min(width - tile_x),
                height: TILE_SIZE.min(height - tile_y),
            };
            decode_tile(&mut input, &mut tile)?;
        }
    }
    if input.pos != data.len() {
        warn!(
            extra = data.len() - input.pos,
            "ignoring trailing bytes after ZRLE tiles"
        );
    }
    Ok(rgba)
}

fn decode_tile(input: &mut Input<'_>, tile: &mut Tile<'_>) -> Result<(), RfbError> {
    let pixels = tile.width * tile.height;
    match input.u8()? {
        0 => {
            // Raw CPIXELs.
            let bytes = input.take(pixels * BYTES_PER_CPIXEL)?;
            for (row, source) in bytes
                .chunks_exact(tile.width * BYTES_PER_CPIXEL)
                .enumerate()
            {
                for (dest, cpixel) in tile
                    .row_mut(row)
                    .chunks_exact_mut(4)
                    .zip(source.chunks_exact(BYTES_PER_CPIXEL))
                {
                    dest.copy_from_slice(&pixel::to_rgba(cpixel));
                }
            }
        }
        1 => tile.run(0, pixels, input.cpixel()?),
        palette_size @ 2..=16 => {
            let palette = input.palette(usize::from(palette_size))?;
            let bits = match palette_size {
                2 => 1,
                3..=4 => 2,
                _ => 4,
            };
            let row_len = (tile.width * bits).div_ceil(8);
            let mask = (1u8 << bits) - 1;
            for row in 0..tile.height {
                let packed = input.take(row_len)?;
                for (column, dest) in tile.row_mut(row).chunks_exact_mut(4).enumerate() {
                    let bit = column * bits;
                    let index = (packed[bit / 8] >> (8 - bits - bit % 8)) & mask;
                    dest.copy_from_slice(&palette_entry(&palette, index)?);
                }
            }
        }
        128 => {
            // Plain RLE.
            let mut position = 0;
            while position < pixels {
                let colour = input.cpixel()?;
                let length = input.run_length(pixels - position)?;
                tile.run(position, length, colour);
                position += length;
            }
        }
        subencoding @ 130..=255 => {
            let palette = input.palette(usize::from(subencoding - 128))?;
            let mut position = 0;
            while position < pixels {
                let byte = input.u8()?;
                let colour = palette_entry(&palette, byte & 0x7f)?;
                let length = if byte & 0x80 != 0 {
                    input.run_length(pixels - position)?
                } else {
                    1
                };
                tile.run(position, length, colour);
                position += length;
            }
        }
        other => {
            return Err(RfbError::Protocol(format!(
                "invalid ZRLE subencoding {other}"
            )));
        }
    }
    Ok(())
}

fn palette_entry(palette: &[[u8; 4]], index: u8) -> Result<[u8; 4], RfbError> {
    palette.get(usize::from(index)).copied().ok_or_else(|| {
        RfbError::Protocol(format!(
            "ZRLE palette index {index} out of range for {} entries",
            palette.len()
        ))
    })
}

/// Cursor over inflated tile data.
struct Input<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Input<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], RfbError> {
        let bytes = self
            .data
            .get(self.pos..self.pos + len)
            .ok_or_else(|| RfbError::Protocol("ZRLE tile data truncated".into()))?;
        self.pos += len;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, RfbError> {
        Ok(self.take(1)?[0])
    }

    fn cpixel(&mut self) -> Result<[u8; 4], RfbError> {
        Ok(pixel::to_rgba(self.take(BYTES_PER_CPIXEL)?))
    }

    fn palette(&mut self, size: usize) -> Result<Vec<[u8; 4]>, RfbError> {
        Ok(self
            .take(size * BYTES_PER_CPIXEL)?
            .chunks_exact(BYTES_PER_CPIXEL)
            .map(pixel::to_rgba)
            .collect())
    }

    /// Reads an RLE run length (one plus the sum of bytes up to and including the first byte
    /// that is not 255), rejecting runs longer than `remaining` pixels.
    fn run_length(&mut self, remaining: usize) -> Result<usize, RfbError> {
        let mut length = 1;
        loop {
            let byte = self.u8()?;
            length += usize::from(byte);
            if length > remaining {
                return Err(RfbError::Protocol("ZRLE run exceeds its tile".into()));
            }
            if byte != 255 {
                return Ok(length);
            }
        }
    }
}

/// Destination of one tile inside the rectangle's RGBA buffer.
struct Tile<'a> {
    rgba: &'a mut [u8],
    /// Width of the whole rectangle in pixels.
    stride: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl Tile<'_> {
    /// Returns the RGBA bytes of one row of the tile.
    fn row_mut(&mut self, row: usize) -> &mut [u8] {
        let offset = ((self.y + row) * self.stride + self.x) * 4;
        &mut self.rgba[offset..offset + self.width * 4]
    }

    /// Paints `length` pixels starting at tile-relative pixel index `start` (row-major).
    fn run(&mut self, mut start: usize, mut length: usize, colour: [u8; 4]) {
        while length > 0 {
            let (row, column) = (start / self.width, start % self.width);
            let count = length.min(self.width - column);
            let offset = ((self.y + row) * self.stride + self.x + column) * 4;
            for pixel in self.rgba[offset..offset + count * 4].chunks_exact_mut(4) {
                pixel.copy_from_slice(&colour);
            }
            start += count;
            length -= count;
        }
    }
}

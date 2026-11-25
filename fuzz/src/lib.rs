//! Helpers shared by the fuzz targets.

/// Cuts `data` into consecutive pieces whose lengths follow from `seed`, so
/// that one input exercises a parser on a different split of the same bytes
/// for each seed: mostly short pieces (splitting inside length prefixes,
/// headers and UTF-8 sequences), sometimes all that is left.
pub fn pieces(seed: u8, data: &[u8]) -> Vec<&[u8]> {
    let mut state = (u32::from(seed) + 1).wrapping_mul(0x9E37_79B9);
    let mut pieces = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        // xorshift32
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let len = match state % 4 {
            0 => 1,
            1 => (state >> 8) as usize % 8 + 1,
            2 => (state >> 8) as usize % 256 + 1,
            _ => rest.len(),
        };
        let (piece, tail) = rest.split_at(len.min(rest.len()));
        pieces.push(piece);
        rest = tail;
    }
    pieces
}

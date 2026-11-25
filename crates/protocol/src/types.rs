//! Small value types that appear as instruction arguments.

/// Porter-Duff compositing operation ("channel mask") used by drawing
/// instructions such as `img`, `copy` and `cfill`.
///
/// The four bits describe which regions survive:
/// `0x8` = source where destination is empty, `0x4` = source where both
/// overlap, `0x2` = destination where source is empty, `0x1` = destination
/// where both overlap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CompositeMode(pub u8);

impl CompositeMode {
    /// destination-in
    pub const RIN: CompositeMode = CompositeMode(0x1);
    /// destination-out
    pub const ROUT: CompositeMode = CompositeMode(0x2);
    /// source-in
    pub const IN: CompositeMode = CompositeMode(0x4);
    /// source-atop
    pub const ATOP: CompositeMode = CompositeMode(0x6);
    /// source-out
    pub const OUT: CompositeMode = CompositeMode(0x8);
    /// destination-atop
    pub const RATOP: CompositeMode = CompositeMode(0x9);
    /// xor
    pub const XOR: CompositeMode = CompositeMode(0xA);
    /// destination-over
    pub const ROVER: CompositeMode = CompositeMode(0xB);
    /// copy
    pub const SRC: CompositeMode = CompositeMode(0xC);
    /// source-over (the default)
    pub const OVER: CompositeMode = CompositeMode(0xE);
    /// lighter (additive)
    pub const PLUS: CompositeMode = CompositeMode(0xF);

    pub fn parse(value: &str) -> Option<Self> {
        value.parse::<u8>().ok().map(|v| CompositeMode(v & 0xF))
    }
}

impl Default for CompositeMode {
    fn default() -> Self {
        Self::OVER
    }
}

/// Raster operation used by the `transfer` instruction.
///
/// The value is a 4-bit truth table: bit `3 - (src << 1 | dst)` holds the
/// result for the given source/destination bit pair. For example `SRC = 0x3`
/// sets the bits for `(1,1)` and `(1,0)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TransferFunction(pub u8);

impl TransferFunction {
    pub const BLACK: TransferFunction = TransferFunction(0x0);
    pub const AND: TransferFunction = TransferFunction(0x1);
    pub const SRC_AND_NOT_DEST: TransferFunction = TransferFunction(0x2);
    pub const SRC: TransferFunction = TransferFunction(0x3);
    pub const NOT_SRC_AND_DEST: TransferFunction = TransferFunction(0x4);
    pub const DEST: TransferFunction = TransferFunction(0x5);
    pub const XOR: TransferFunction = TransferFunction(0x6);
    pub const OR: TransferFunction = TransferFunction(0x7);
    pub const NOR: TransferFunction = TransferFunction(0x8);
    pub const XNOR: TransferFunction = TransferFunction(0x9);
    pub const NOT_DEST: TransferFunction = TransferFunction(0xA);
    pub const SRC_OR_NOT_DEST: TransferFunction = TransferFunction(0xB);
    pub const NOT_SRC: TransferFunction = TransferFunction(0xC);
    pub const NOT_SRC_OR_DEST: TransferFunction = TransferFunction(0xD);
    pub const NAND: TransferFunction = TransferFunction(0xE);
    pub const WHITE: TransferFunction = TransferFunction(0xF);

    pub fn parse(value: &str) -> Option<Self> {
        value.parse::<u8>().ok().map(|v| TransferFunction(v & 0xF))
    }

    /// Applies the operation bitwise to one 8-bit channel.
    pub fn apply(self, src: u8, dst: u8) -> u8 {
        let f = self.0;
        let mut out = 0u8;
        if f & 0x8 != 0 {
            out |= !src & !dst;
        }
        if f & 0x4 != 0 {
            out |= !src & dst;
        }
        if f & 0x2 != 0 {
            out |= src & !dst;
        }
        if f & 0x1 != 0 {
            out |= src & dst;
        }
        out
    }

    /// Whether the destination alpha is replaced by the source alpha
    /// (only `SRC` and `NOT_SRC` do so in the reference client).
    pub fn copies_alpha(self) -> bool {
        self == Self::SRC || self == Self::NOT_SRC
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum LineCap {
    #[default]
    Butt,
    Round,
    Square,
}

impl LineCap {
    pub fn from_code(code: i64) -> Self {
        match code {
            1 => Self::Round,
            2 => Self::Square,
            _ => Self::Butt,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum LineJoin {
    Bevel,
    #[default]
    Miter,
    Round,
}

impl LineJoin {
    pub fn from_code(code: i64) -> Self {
        match code {
            0 => Self::Bevel,
            2 => Self::Round,
            _ => Self::Miter,
        }
    }
}

/// Button mask of the `mouse` instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct MouseButtons(pub u8);

impl MouseButtons {
    pub const LEFT: u8 = 0x01;
    pub const MIDDLE: u8 = 0x02;
    pub const RIGHT: u8 = 0x04;
    pub const SCROLL_UP: u8 = 0x08;
    pub const SCROLL_DOWN: u8 = 0x10;
    pub const BACK: u8 = 0x20;
    pub const FORWARD: u8 = 0x40;

    pub fn contains(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    pub fn set(&mut self, bit: u8, pressed: bool) {
        if pressed {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_truth_tables() {
        let (s, d) = (0b1100_1010u8, 0b1010_0110u8);
        assert_eq!(TransferFunction::BLACK.apply(s, d), 0);
        assert_eq!(TransferFunction::WHITE.apply(s, d), 0xFF);
        assert_eq!(TransferFunction::SRC.apply(s, d), s);
        assert_eq!(TransferFunction::DEST.apply(s, d), d);
        assert_eq!(TransferFunction::NOT_SRC.apply(s, d), !s);
        assert_eq!(TransferFunction::NOT_DEST.apply(s, d), !d);
        assert_eq!(TransferFunction::AND.apply(s, d), s & d);
        assert_eq!(TransferFunction::NAND.apply(s, d), !(s & d));
        assert_eq!(TransferFunction::OR.apply(s, d), s | d);
        assert_eq!(TransferFunction::NOR.apply(s, d), !(s | d));
        assert_eq!(TransferFunction::XOR.apply(s, d), s ^ d);
        assert_eq!(TransferFunction::XNOR.apply(s, d), !(s ^ d));
        assert_eq!(TransferFunction::NOT_SRC_AND_DEST.apply(s, d), !s & d);
        assert_eq!(TransferFunction::NOT_SRC_OR_DEST.apply(s, d), !s | d);
        assert_eq!(TransferFunction::SRC_AND_NOT_DEST.apply(s, d), s & !d);
        assert_eq!(TransferFunction::SRC_OR_NOT_DEST.apply(s, d), s | !d);
    }
}

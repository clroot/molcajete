//! Terminal colours: guacd's palette and `color-scheme` syntax, and the
//! resolution of cell colours (named, indexed, true colour) to RGB.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// An RGB colour.
pub type Rgb = [u8; 3];

/// The 16 base colours of guacd's initial palette (`terminal/palette.c`).
const BASE16: [Rgb; 16] = [
    [0x00, 0x00, 0x00],
    [0x99, 0x3E, 0x3E],
    [0x3E, 0x99, 0x3E],
    [0x99, 0x99, 0x3E],
    [0x3E, 0x3E, 0x99],
    [0x99, 0x3E, 0x99],
    [0x3E, 0x99, 0x99],
    [0x99, 0x99, 0x99],
    [0x3E, 0x3E, 0x3E],
    [0xFF, 0x67, 0x67],
    [0x67, 0xFF, 0x67],
    [0xFF, 0xFF, 0x67],
    [0x67, 0x67, 0xFF],
    [0xFF, 0x67, 0xFF],
    [0x67, 0xFF, 0xFF],
    [0xFF, 0xFF, 0xFF],
];

/// The xterm 256-colour palette with guacd's first 16 colours.
fn initial_palette() -> [Rgb; 256] {
    const LEVELS: [u8; 6] = [0x00, 0x5F, 0x87, 0xAF, 0xD7, 0xFF];
    let mut palette = [[0u8; 3]; 256];
    palette[..16].copy_from_slice(&BASE16);
    for i in 0..216 {
        palette[16 + i] = [LEVELS[i / 36], LEVELS[(i / 6) % 6], LEVELS[i % 6]];
    }
    for i in 0..24 {
        let v = 0x08 + 10 * i as u8;
        palette[232 + i] = [v, v, v];
    }
    palette
}

/// Default foreground/background and the 256-colour palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColorScheme {
    pub foreground: Rgb,
    pub background: Rgb,
    pub palette: [Rgb; 256],
}

impl Default for ColorScheme {
    /// guacd's default, `gray-black`.
    fn default() -> Self {
        let palette = initial_palette();
        Self {
            foreground: palette[7],
            background: palette[0],
            palette,
        }
    }
}

impl ColorScheme {
    /// Parses a guacd `color-scheme` value: one of the named schemes
    /// (`black-white`, `gray-black`, `green-black`, `white-black`) or a
    /// `;`-separated list of `name: value` pairs where the name is
    /// `foreground`, `background` or `colorN` and the value is `colorN` or an
    /// X11 colour spec such as `rgb:ff/80/00`.
    ///
    /// Invalid entries are skipped; their descriptions are returned so they
    /// can be logged.
    pub fn parse(spec: &str) -> (Self, Vec<String>) {
        let spec = match spec.trim() {
            "gray-black" => "foreground: color7; background: color0",
            "black-white" => "foreground: color0; background: color15",
            "green-black" => "foreground: color2; background: color0",
            "white-black" => "foreground: color15; background: color0",
            other => other,
        };
        let mut scheme = Self::default();
        let mut errors = Vec::new();
        for entry in spec.split(';').map(str::trim).filter(|e| !e.is_empty()) {
            if let Err(err) = scheme.apply(entry) {
                errors.push(format!("{entry:?}: {err}"));
            }
        }
        (scheme, errors)
    }

    fn apply(&mut self, entry: &str) -> Result<(), &'static str> {
        let (name, value) = entry.split_once(':').ok_or("expected `name: value`")?;
        let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
        let color = match parse_palette_ref(value) {
            Some(index) => self.palette[index],
            None => parse_x11_color(value).ok_or("invalid colour value")?,
        };
        match name.as_str() {
            "foreground" => self.foreground = color,
            "background" => self.background = color,
            _ => {
                let index = parse_palette_ref(&name).ok_or("unknown colour name")?;
                self.palette[index] = color;
            }
        }
        Ok(())
    }

    /// Resolves a cell colour, honouring palette changes made by the
    /// application (OSC 4/10/11).
    pub fn resolve(&self, color: Color, overrides: &Colors) -> Rgb {
        match color {
            Color::Spec(rgb) => [rgb.r, rgb.g, rgb.b],
            Color::Indexed(index) => self.indexed(index as usize, overrides),
            Color::Named(named) => self.named(named, overrides),
        }
    }

    fn indexed(&self, index: usize, overrides: &Colors) -> Rgb {
        match overrides[index] {
            Some(rgb) => [rgb.r, rgb.g, rgb.b],
            None => self.palette[index],
        }
    }

    fn named(&self, named: NamedColor, overrides: &Colors) -> Rgb {
        let index = named as usize;
        if index < 16 {
            return self.indexed(index, overrides);
        }
        if let Some(rgb) = overrides[named] {
            return [rgb.r, rgb.g, rgb.b];
        }
        let foreground =
            || overrides[NamedColor::Foreground].map_or(self.foreground, |c| [c.r, c.g, c.b]);
        match named {
            NamedColor::Background => self.background,
            NamedColor::DimForeground => self.dim(foreground()),
            NamedColor::DimBlack
            | NamedColor::DimRed
            | NamedColor::DimGreen
            | NamedColor::DimYellow
            | NamedColor::DimBlue
            | NamedColor::DimMagenta
            | NamedColor::DimCyan
            | NamedColor::DimWhite => {
                let base = index - NamedColor::DimBlack as usize;
                self.dim(self.indexed(base, overrides))
            }
            // Foreground, BrightForeground, Cursor.
            _ => foreground(),
        }
    }

    /// Faint (SGR 2) version of a colour: two thirds of the way from the
    /// background to the colour.
    pub fn dim(&self, color: Rgb) -> Rgb {
        let mut out = [0u8; 3];
        for (i, o) in out.iter_mut().enumerate() {
            let (c, b) = (color[i] as u16, self.background[i] as u16);
            *o = ((c * 2 + b) / 3) as u8;
        }
        out
    }
}

/// `colorN` with N in 0..=255.
fn parse_palette_ref(value: &str) -> Option<usize> {
    let digits = value.trim().strip_prefix("color")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<usize>().ok().filter(|&i| i < 256)
}

/// Parses `rgb:R/G/B` where each component has 1 to 4 hex digits (the
/// subset of XParseColor accepted by guacd).
pub fn parse_x11_color(value: &str) -> Option<Rgb> {
    let rest = value.trim();
    let rest = rest
        .get(..4)
        .filter(|p| p.eq_ignore_ascii_case("rgb:"))
        .map(|_| &rest[4..])?;
    let mut out = [0u8; 3];
    let mut parts = rest.split('/');
    for component in &mut out {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 4 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(part, 16).ok()?;
        let max = (1u32 << (4 * part.len())) - 1;
        *component = ((value * 255 + max / 2) / max) as u8;
    }
    parts.next().is_none().then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::ansi::Rgb as VteRgb;

    #[test]
    fn default_is_gray_black() {
        let scheme = ColorScheme::default();
        assert_eq!(scheme.foreground, [0x99, 0x99, 0x99]);
        assert_eq!(scheme.background, [0, 0, 0]);
        assert_eq!(ColorScheme::parse("gray-black").0, scheme);
        assert_eq!(ColorScheme::parse("").0, scheme);
    }

    #[test]
    fn named_schemes() {
        let (s, errors) = ColorScheme::parse("black-white");
        assert!(errors.is_empty());
        assert_eq!(
            (s.foreground, s.background),
            ([0, 0, 0], [0xFF, 0xFF, 0xFF])
        );
        let s = ColorScheme::parse("green-black").0;
        assert_eq!(
            (s.foreground, s.background),
            ([0x3E, 0x99, 0x3E], [0, 0, 0])
        );
        let s = ColorScheme::parse("white-black").0;
        assert_eq!(
            (s.foreground, s.background),
            ([0xFF, 0xFF, 0xFF], [0, 0, 0])
        );
    }

    #[test]
    fn custom_scheme_syntax() {
        let (s, errors) = ColorScheme::parse(
            "foreground: rgb:FF/FF/00; background: rgb:00/00/80;\
             color9: rgb:f/8/0; color1: color9; color300: rgb:1/2/3; bogus",
        );
        assert_eq!(s.foreground, [0xFF, 0xFF, 0x00]);
        assert_eq!(s.background, [0x00, 0x00, 0x80]);
        assert_eq!(s.palette[9], [0xFF, 0x88, 0x00]);
        assert_eq!(s.palette[1], [0xFF, 0x88, 0x00]);
        assert_eq!(errors.len(), 2, "{errors:?}");
    }

    #[test]
    fn x11_colors() {
        assert_eq!(parse_x11_color("rgb:ff/00/7f"), Some([255, 0, 127]));
        assert_eq!(parse_x11_color("RGB:ffff/0/8000"), Some([255, 0, 128]));
        assert_eq!(parse_x11_color("rgb:ff/00"), None);
        assert_eq!(parse_x11_color("rgb:ff/00/00/00"), None);
        assert_eq!(parse_x11_color("#ff0000"), None);
        assert_eq!(parse_x11_color("rgb:fffff/0/0"), None);
    }

    #[test]
    fn xterm_cube_and_grays() {
        let p = initial_palette();
        assert_eq!(p[16], [0, 0, 0]);
        assert_eq!(p[196], [0xFF, 0, 0]);
        assert_eq!(p[231], [0xFF, 0xFF, 0xFF]);
        assert_eq!(p[232], [8, 8, 8]);
        assert_eq!(p[255], [0xEE, 0xEE, 0xEE]);
    }

    #[test]
    fn resolves_cell_colors() {
        let scheme = ColorScheme::default();
        let mut overrides = Colors::default();
        assert_eq!(
            scheme.resolve(Color::Named(NamedColor::Red), &overrides),
            [0x99, 0x3E, 0x3E]
        );
        assert_eq!(
            scheme.resolve(Color::Indexed(196), &overrides),
            [0xFF, 0, 0]
        );
        assert_eq!(
            scheme.resolve(Color::Spec(VteRgb { r: 1, g: 2, b: 3 }), &overrides),
            [1, 2, 3]
        );
        assert_eq!(
            scheme.resolve(Color::Named(NamedColor::Background), &overrides),
            [0, 0, 0]
        );
        overrides[1] = Some(VteRgb { r: 9, g: 9, b: 9 });
        overrides[NamedColor::Foreground] = Some(VteRgb { r: 7, g: 7, b: 7 });
        assert_eq!(scheme.resolve(Color::Indexed(1), &overrides), [9, 9, 9]);
        assert_eq!(
            scheme.resolve(Color::Named(NamedColor::Foreground), &overrides),
            [7, 7, 7]
        );
        assert_eq!(
            scheme.resolve(Color::Named(NamedColor::DimRed), &overrides),
            [6, 6, 6]
        );
    }
}

//! Draws the terminal into the Guacamole display.
//!
//! Every frame the visible cells are resolved to what they look like
//! (character, colours, decorations, cursor, selection) and compared with
//! what the client already shows; only rows with differences are redrawn,
//! and only the span of cells that changed. The display then turns the
//! changed pixels into drawing instructions.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Indexed;
use alacritty_terminal::index::Point;
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{RenderableContent, RenderableCursor, Term};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};

use super::color::{ColorScheme, Rgb};
use super::font::{CellMetrics, Fonts, Mask};
use super::terminal::Size;
use crate::display::Display;
use crate::display::pointer::PointerImage;

/// How the text cursor is drawn over a cell (a block cursor is drawn by
/// swapping the cell's colours instead).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CursorMark {
    #[default]
    None,
    Underline,
    Beam,
    Hollow,
}

/// Text decorations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Style {
    bold: bool,
    underline: bool,
    double_underline: bool,
    strikeout: bool,
}

/// Everything that determines how one cell looks.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CellView {
    c: char,
    /// Combining characters drawn over `c`.
    combining: Vec<char>,
    fg: Rgb,
    bg: Rgb,
    style: Style,
    /// Cells covered: 1, 2 for wide characters, 0 for the spacer after a
    /// wide character.
    width: u8,
    cursor: CursorMark,
}

impl CellView {
    fn blank(bg: Rgb) -> Self {
        Self {
            c: ' ',
            combining: Vec::new(),
            fg: bg,
            bg,
            style: Style::default(),
            width: 1,
            cursor: CursorMark::None,
        }
    }
}

pub struct Renderer {
    fonts: Fonts,
    scheme: ColorScheme,
    size: Size,
    /// What the client shows for each cell (`None`: unknown, redraw).
    shown: Vec<Option<CellView>>,
    /// Scratch buffer for the next frame.
    next: Vec<CellView>,
    /// Background colour of the area outside the cell grid, once painted.
    margin: Option<(Rgb, u32, u32)>,
    pixels: Vec<u8>,
    /// Cells of the link under the pointer (grid points, inclusive), drawn
    /// underlined.
    link: Option<(Point, Point)>,
}

impl Renderer {
    pub fn new(fonts: Fonts, scheme: ColorScheme) -> Self {
        Self {
            fonts,
            scheme,
            size: Size { cols: 0, rows: 0 },
            shown: Vec::new(),
            next: Vec::new(),
            margin: None,
            pixels: Vec::new(),
            link: None,
        }
    }

    pub fn metrics(&self) -> CellMetrics {
        self.fonts.metrics()
    }

    /// The resolution (pixels per inch) the terminal is drawn for.
    pub fn dpi(&self) -> u32 {
        self.fonts.dpi()
    }

    /// Changes the font size (points). Returns whether the cells changed
    /// size; the grid then has to be laid out again
    /// ([`Renderer::grid_size`], [`Renderer::invalidate`]).
    pub fn set_font_size(&mut self, size_pt: f32) -> bool {
        self.fonts.set_size(size_pt)
    }

    pub fn scheme(&self) -> &ColorScheme {
        &self.scheme
    }

    /// Terminal size (cells) fitting a display of the given pixel size.
    pub fn grid_size(&self, width: u32, height: u32) -> Size {
        let m = self.metrics();
        Size {
            cols: ((width / m.width) as usize).max(2),
            rows: ((height / m.height) as usize).max(1),
        }
    }

    /// Underlines the cells of the link under the pointer from the next
    /// frame on.
    pub fn set_link(&mut self, link: Option<(Point, Point)>) {
        self.link = link;
    }

    /// Forgets what the client shows, forcing a full redraw (after a
    /// resize).
    pub fn invalidate(&mut self, size: Size) {
        self.size = size;
        self.shown = vec![None; size.cols * size.rows];
        self.margin = None;
    }

    /// Draws everything that changed since the last call. Returns the
    /// number of rows redrawn.
    pub fn render<T: EventListener>(&mut self, term: &Term<T>, display: &mut Display) -> usize {
        let RenderableContent {
            display_iter,
            selection,
            cursor,
            display_offset,
            colors,
            ..
        } = term.renderable_content();
        let frame = Frame {
            colors,
            selection,
            cursor,
            link: self.link,
        };
        let default_bg = self
            .scheme
            .resolve(Color::Named(NamedColor::Background), colors);
        self.paint_margins(display, default_bg);

        let Size { cols, rows } = self.size;
        self.next.clear();
        self.next.resize(cols * rows, CellView::blank(default_bg));
        for indexed in display_iter {
            let row = indexed.point.line.0 + display_offset as i32;
            let col = indexed.point.column.0;
            if row < 0 || row as usize >= rows || col >= cols {
                continue;
            }
            self.next[row as usize * cols + col] = cell_view(&self.scheme, &indexed, &frame);
        }

        let mut redrawn = 0;
        for row in 0..rows {
            let line = row * cols..(row + 1) * cols;
            let changed = |col: &usize| {
                self.shown[line.start + col].as_ref() != Some(&self.next[line.start + col])
            };
            let Some(mut start) = (0..cols).find(changed) else {
                continue;
            };
            let mut end = (0..cols).rev().find(changed).unwrap_or(start);
            // Wide characters are drawn whole.
            if start > 0 && self.next[line.start + start].width == 0 {
                start -= 1;
            }
            if self.next[line.start + end].width == 2 && end + 1 < cols {
                end += 1;
            }
            self.draw_span(display, row, start, end);
            for col in start..=end {
                self.shown[line.start + col] = Some(self.next[line.start + col].clone());
            }
            redrawn += 1;
        }
        redrawn
    }

    /// Paints the strips right of and below the cell grid.
    fn paint_margins(&mut self, display: &mut Display, bg: Rgb) {
        let (w, h) = (display.width(), display.height());
        if self.margin == Some((bg, w, h)) {
            return;
        }
        let m = self.metrics();
        let grid_w = (self.size.cols as u32 * m.width).min(w);
        let grid_h = (self.size.rows as u32 * m.height).min(h);
        let color = [bg[0], bg[1], bg[2], 255];
        display.fill(grid_w, 0, w - grid_w, h, color);
        display.fill(0, grid_h, grid_w, h - grid_h, color);
        self.margin = Some((bg, w, h));
    }

    /// Draws cells `start..=end` of a row into the display.
    fn draw_span(&mut self, display: &mut Display, row: usize, start: usize, end: usize) {
        let m = self.metrics();
        let (cw, ch) = (m.width as usize, m.height as usize);
        let span_w = (end - start + 1) * cw;
        self.pixels.clear();
        self.pixels.resize(span_w * ch * 4, 0);
        let base = row * self.size.cols;
        for col in start..=end {
            let view = &self.next[base + col];
            if view.width == 0 {
                continue;
            }
            let cells = (view.width as usize).min(end + 1 - col);
            draw_cell(
                &mut self.fonts,
                view,
                m,
                cells,
                &mut self.pixels,
                span_w,
                (col - start) * cw,
            );
        }
        display.put(
            (start * cw) as u32,
            (row * ch) as u32,
            span_w as u32,
            ch as u32,
            &self.pixels,
        );
    }
}

/// The parts of [`RenderableContent`] needed per cell.
struct Frame<'a> {
    colors: &'a Colors,
    selection: Option<SelectionRange>,
    cursor: RenderableCursor,
    link: Option<(Point, Point)>,
}

/// Bold text in one of the 8 basic colours uses the bright variant (like
/// guacd and xterm).
fn brighten(color: Color) -> Color {
    match color {
        Color::Named(named) if (named as usize) < 8 => Color::Named(named.to_bright()),
        Color::Indexed(i) if i < 8 => Color::Indexed(i + 8),
        other => other,
    }
}

fn cell_view(scheme: &ColorScheme, indexed: &Indexed<&Cell>, frame: &Frame<'_>) -> CellView {
    let cell = indexed.cell;
    let flags = cell.flags;
    let bold = flags.contains(Flags::BOLD);
    let fg = if bold { brighten(cell.fg) } else { cell.fg };
    let mut fg = scheme.resolve(fg, frame.colors);
    let mut bg = scheme.resolve(cell.bg, frame.colors);
    if flags.contains(Flags::DIM) {
        fg = scheme.dim(fg);
    }
    if flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if flags.contains(Flags::HIDDEN) {
        fg = bg;
    }
    let cursor = &frame.cursor;
    let selected = frame
        .selection
        .is_some_and(|s| s.contains_cell(indexed, cursor.point, cursor.shape));
    if selected {
        std::mem::swap(&mut fg, &mut bg);
    }
    let mut mark = CursorMark::None;
    if indexed.point == cursor.point {
        match cursor.shape {
            CursorShape::Block => std::mem::swap(&mut fg, &mut bg),
            CursorShape::Underline => mark = CursorMark::Underline,
            CursorShape::Beam => mark = CursorMark::Beam,
            CursorShape::HollowBlock => mark = CursorMark::Hollow,
            CursorShape::Hidden => {}
        }
    }
    let width = if flags.contains(Flags::WIDE_CHAR) {
        2
    } else if flags.contains(Flags::WIDE_CHAR_SPACER) {
        0
    } else {
        1
    };
    let c = if flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) || cell.c == '\t' {
        ' '
    } else {
        cell.c
    };
    CellView {
        c,
        combining: cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
        fg,
        bg,
        style: Style {
            bold,
            underline: flags.intersects(Flags::ALL_UNDERLINES)
                || frame
                    .link
                    .is_some_and(|(start, end)| (start..=end).contains(&indexed.point)),
            double_underline: flags.contains(Flags::DOUBLE_UNDERLINE),
            strikeout: flags.contains(Flags::STRIKEOUT),
        },
        width,
        cursor: mark,
    }
}

/// Draws one cell (`cells` wide) at pixel column `x0` of a row buffer
/// `stride` pixels wide.
fn draw_cell(
    fonts: &mut Fonts,
    view: &CellView,
    m: CellMetrics,
    cells: usize,
    pixels: &mut [u8],
    stride: usize,
    x0: usize,
) {
    let (w, h) = (m.width as usize * cells, m.height as usize);
    let mut mask = fonts.glyph(view.c, view.style.bold, cells as u8);
    for &c in &view.combining {
        if let Some(mark) = fonts.glyph(c, view.style.bold, cells as u8) {
            let merged = mask.get_or_insert_with(|| mark.clone());
            std::sync::Arc::make_mut(merged).merge(&mark);
        }
    }

    let mut decorations = Mask::new(w as u32, h as u32);
    let line = (h / 16).max(1) as i32;
    let baseline = m.baseline as i32;
    if view.style.underline {
        let y = (baseline + line).min(h as i32 - line);
        decorations.fill(0, y, w as i32, line);
        if view.style.double_underline {
            decorations.fill(0, (y + 2 * line).min(h as i32 - line), w as i32, line);
        }
    }
    if view.style.strikeout {
        decorations.fill(0, baseline * 2 / 3, w as i32, line);
    }
    let thick = (m.width as i32 / 8).max(1);
    match view.cursor {
        CursorMark::None => {}
        CursorMark::Underline => decorations.fill(0, h as i32 - 2 * thick, w as i32, 2 * thick),
        CursorMark::Beam => decorations.fill(0, 0, thick, h as i32),
        CursorMark::Hollow => {
            decorations.fill(0, 0, w as i32, thick);
            decorations.fill(0, h as i32 - thick, w as i32, thick);
            decorations.fill(0, 0, thick, h as i32);
            decorations.fill(w as i32 - thick, 0, thick, h as i32);
        }
    }

    for y in 0..h {
        let row = &mut pixels[(y * stride + x0) * 4..(y * stride + x0 + w) * 4];
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let i = y * w + x;
            let glyph = mask.as_ref().map_or(0, |m| m.data[i]);
            let alpha = glyph.max(decorations.data[i]) as u16;
            for ((out, &f), &b) in px.iter_mut().zip(&view.fg).zip(&view.bg) {
                *out = ((f as u16 * alpha + b as u16 * (255 - alpha) + 127) / 255) as u8;
            }
            px[3] = 255;
        }
    }
}

/// The terminal's mouse pointers, drawn for 96 dpi
/// ([`PointerImage::for_dpi`] enlarges them).
pub struct Pointer;

impl Pointer {
    fn from_art(art: &[&str], hotspot: (u32, u32)) -> PointerImage {
        let height = art.len() as u32;
        let width = art.iter().map(|r| r.len()).max().unwrap_or(0) as u32;
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        for (y, line) in art.iter().enumerate() {
            for (x, b) in line.bytes().enumerate() {
                let color = match b {
                    b'#' => [0, 0, 0, 255],
                    b'.' => [255, 255, 255, 255],
                    _ => continue,
                };
                let i = (y * width as usize + x) * 4;
                rgba[i..i + 4].copy_from_slice(&color);
            }
        }
        PointerImage {
            hotspot_x: hotspot.0,
            hotspot_y: hotspot.1,
            width,
            height,
            rgba,
        }
    }

    /// Text-selection I-beam, shown while the pointer selects text.
    pub fn ibeam() -> PointerImage {
        const ART: &[&str] = &[
            "###.###", "#..#..#", "###.###", "  #.#  ", "  #.#  ", "  #.#  ", "  #.#  ", "  #.#  ",
            "  #.#  ", "  #.#  ", "  #.#  ", "  #.#  ", "  #.#  ", "###.###", "#..#..#", "###.###",
        ];
        Self::from_art(ART, (3, 8))
    }

    /// Pointing hand, shown over a link.
    pub fn hand() -> PointerImage {
        const ART: &[&str] = &[
            "     ##",
            "    #..#",
            "    #..#",
            "    #..#",
            "    #..###",
            "    #..#..###",
            "    #..#..#..##",
            "    #..#..#..#.#",
            "### #..#..#..#..#",
            "#..##...........#",
            "#...#...........#",
            " #..............#",
            "  #.............#",
            "  #.............#",
            "   #...........#",
            "   #...........#",
            "    #.........#",
            "    #.........#",
            "    ###########",
        ];
        Self::from_art(ART, (5, 0))
    }

    /// Arrow, shown while the application receives mouse events.
    pub fn arrow() -> PointerImage {
        const ART: &[&str] = &[
            "#",
            "##",
            "#.#",
            "#..#",
            "#...#",
            "#....#",
            "#.....#",
            "#......#",
            "#.......#",
            "#........#",
            "#.....#####",
            "#..#..#",
            "#.# #..#",
            "##  #..#",
            "#    #..#",
            "     #..#",
            "      ##",
        ];
        Self::from_art(ART, (0, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::ImageFormats;
    use crate::io::Output;
    use crate::ssh::font::FontOptions;
    use crate::ssh::terminal::Terminal;

    fn setup(cols: usize, rows: usize) -> (Renderer, Terminal, Display) {
        let fonts = Fonts::load(&FontOptions {
            name: None,
            size_pt: 12.0,
            dpi: 96,
            search_paths: Vec::new(),
        });
        let mut renderer = Renderer::new(fonts, ColorScheme::default());
        let m = renderer.metrics();
        let mut display = Display::new(ImageFormats::default(), true);
        let (w, h) = (cols as u32 * m.width + 3, rows as u32 * m.height + 5);
        display.resize(w, h, &mut Output::text());
        let size = renderer.grid_size(w, h);
        assert_eq!(size, Size { cols, rows });
        renderer.invalidate(size);
        (renderer, Terminal::new(size, 10), display)
    }

    #[test]
    fn redraws_only_changed_rows() {
        let (mut r, mut t, mut d) = setup(20, 5);
        assert_eq!(
            r.render(t.term(), &mut d),
            5,
            "first frame draws everything"
        );
        assert_eq!(r.render(t.term(), &mut d), 0, "nothing changed");
        t.feed(b"hi");
        // Row 0 gets the text and the moved cursor.
        assert_eq!(r.render(t.term(), &mut d), 1);
        t.feed(b"\r\nthere");
        assert_eq!(r.render(t.term(), &mut d), 2);
        r.invalidate(t.size());
        assert_eq!(r.render(t.term(), &mut d), 5);
    }

    #[test]
    fn resolves_cell_appearance() {
        let (mut r, mut t, mut d) = setup(20, 3);
        t.feed(b"\x1b[1;31mR\x1b[0m\x1b[7mI\x1b[0m\x1b[4mU\x1b[0m");
        t.feed("한".as_bytes());
        r.render(t.term(), &mut d);
        let view = |col: usize| r.next[col].clone();
        let palette = ColorScheme::default().palette;
        // Bold red is drawn bright red.
        assert_eq!(view(0).fg, palette[9]);
        assert!(view(0).style.bold);
        // Inverse swaps the default colours.
        assert_eq!((view(1).fg, view(1).bg), (palette[0], palette[7]));
        assert!(view(2).style.underline);
        assert_eq!((view(3).c, view(3).width, view(4).width), ('한', 2, 0));
        // The block cursor (after the wide char) is inverted.
        assert_eq!((view(5).fg, view(5).bg), (palette[0], palette[7]));
        // Hiding the cursor removes it.
        t.feed(b"\x1b[?25l");
        r.render(t.term(), &mut d);
        assert_eq!(r.next[5].bg, palette[0]);
    }

    #[test]
    fn selection_is_highlighted() {
        let (mut r, mut t, mut d) = setup(20, 3);
        t.feed(b"abc\r\n");
        t.start_selection(
            alacritty_terminal::selection::SelectionType::Simple,
            0,
            0,
            alacritty_terminal::index::Side::Left,
        );
        t.update_selection(1, 0, alacritty_terminal::index::Side::Right);
        r.render(t.term(), &mut d);
        let palette = ColorScheme::default().palette;
        assert_eq!(r.next[0].bg, palette[7]);
        assert_eq!(r.next[1].bg, palette[7]);
        assert_eq!(r.next[2].bg, palette[0]);
    }

    #[test]
    fn link_under_the_pointer_is_underlined() {
        let (mut r, mut t, mut d) = setup(20, 3);
        t.feed(b"abcd");
        let point = |col| {
            Point::new(
                alacritty_terminal::index::Line(0),
                alacritty_terminal::index::Column(col),
            )
        };
        r.set_link(Some((point(1), point(2))));
        assert_eq!(r.render(t.term(), &mut d), 3);
        let underlined: Vec<bool> = (0..4).map(|col| r.next[col].style.underline).collect();
        assert_eq!(underlined, [false, true, true, false]);
        // Taking the underline away redraws the row.
        r.set_link(None);
        assert_eq!(r.render(t.term(), &mut d), 1);
        assert!(!r.next[1].style.underline);
    }

    #[test]
    fn pixels_reach_the_display() {
        let (mut r, mut t, mut d) = setup(4, 2);
        t.feed(b"\x1b[?25l\x1b[42m \x1b[0m");
        r.render(t.term(), &mut d);
        let mut out = Output::text();
        d.flush(&mut out);
        // Cell (0,0) has a green background: its image or fill is sent.
        assert!(out.contains("img") || out.contains("cfill"), "{out}");
    }

    #[test]
    fn pointers_are_well_formed() {
        for dpi in [96, 144, 192] {
            for p in [Pointer::ibeam(), Pointer::arrow(), Pointer::hand()] {
                let p = p.for_dpi(dpi);
                assert_eq!(p.rgba.len(), (p.width * p.height * 4) as usize);
                assert!(p.hotspot_x < p.width && p.hotspot_y < p.height);
            }
        }
        // The I-beam is as tall as it was drawn, per 96 dpi.
        assert_eq!(Pointer::ibeam().height, 16);
        assert_eq!(Pointer::ibeam().for_dpi(192).height, 32);
    }
}

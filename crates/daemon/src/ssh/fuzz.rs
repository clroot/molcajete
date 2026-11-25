//! Entry point of the `terminal` fuzz target.

use std::cell::RefCell;

use alacritty_terminal::index::Side;
use alacritty_terminal::selection::SelectionType;

use super::color::ColorScheme;
use super::font::{FontOptions, Fonts};
use super::render::Renderer;
use super::terminal::{Size, Terminal};
use crate::display::{Display, ImageFormats};
use crate::io::Output;

thread_local! {
    // Loading the fonts takes far longer than a run.
    static RENDERER: RefCell<Option<Renderer>> = const { RefCell::new(None) };
}

/// Treats `data` as what the remote shell prints. The first two bytes give
/// the terminal size; the rest is fed in pieces split at every 0xFF byte (a
/// byte UTF-8 never contains), and after each piece the terminal's events
/// are taken, the screen is rendered and a link is looked for under a cell,
/// as the session does. A selection over the whole screen ends the run.
pub(crate) fn output(data: &[u8]) {
    let [cols, rows, rest @ ..] = data else {
        return;
    };
    let size = Size {
        // At least two columns, like `Renderer::grid_size`: alacritty
        // needs room for a wide character.
        cols: usize::from(cols % 64) + 2,
        rows: usize::from(rows % 32) + 1,
    };
    RENDERER.with_borrow_mut(|renderer| {
        let renderer = renderer.get_or_insert_with(|| {
            Renderer::new(
                Fonts::load(&FontOptions {
                    name: None,
                    size_pt: 6.0,
                    dpi: 96,
                    search_paths: Vec::new(),
                }),
                ColorScheme::default(),
            )
        });
        let m = renderer.metrics();
        let mut display = Display::new(ImageFormats::default(), true);
        display.resize(
            size.cols as u32 * m.width,
            size.rows as u32 * m.height,
            &mut Output::text(),
        );
        renderer.invalidate(size);
        let mut terminal = Terminal::new(size, 100);
        for (i, piece) in rest.split(|&b| b == 0xFF).enumerate() {
            terminal.feed(piece);
            drop(terminal.take_events());
            renderer.render(terminal.term(), &mut display);
            let _ = terminal.link_at(i % size.cols, i % size.rows);
        }
        terminal.start_selection(SelectionType::Semantic, 0, 0, Side::Left);
        terminal.update_selection(size.cols - 1, size.rows - 1, Side::Right);
        let _ = terminal.selection_text();
    });
}

//! The terminal emulator: alacritty's `Term` fed with the remote output,
//! plus scrollback, selection and the events it raises (replies to queries,
//! title and clipboard changes).

use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{self, Term, TermMode, viewport_to_point};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

/// Terminal size in cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Collects events raised by `Term` while it processes output.
#[derive(Clone, Default)]
pub struct EventQueue(Arc<Mutex<Vec<Event>>>);

impl EventListener for EventQueue {
    fn send_event(&self, event: Event) {
        // Only events the session acts on are kept.
        let wanted = matches!(
            event,
            Event::PtyWrite(_)
                | Event::Title(_)
                | Event::ResetTitle
                | Event::ClipboardStore(..)
                | Event::ColorRequest(..)
                | Event::TextAreaSizeRequest(_)
                | Event::Bell
        );
        if wanted && let Ok(mut events) = self.0.lock() {
            events.push(event);
        }
    }
}

pub struct Terminal {
    term: Term<EventQueue>,
    parser: Processor<StdSyncHandler>,
    events: EventQueue,
}

impl Terminal {
    pub fn new(size: Size, scrollback: usize) -> Self {
        let config = term::Config {
            scrolling_history: scrollback,
            ..Default::default()
        };
        let events = EventQueue::default();
        Self {
            term: Term::new(config, &size, events.clone()),
            parser: Processor::new(),
            events,
        }
    }

    pub fn term(&self) -> &Term<EventQueue> {
        &self.term
    }

    pub fn size(&self) -> Size {
        Size {
            cols: self.term.columns(),
            rows: self.term.screen_lines(),
        }
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// Processes output from the remote side.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Events raised since the last call.
    pub fn take_events(&mut self) -> Vec<Event> {
        self.events
            .0
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    }

    /// End of a pending synchronized update (DEC mode 2026), during which
    /// the screen must not be drawn.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// Applies a synchronized update whose deadline passed.
    pub fn finish_sync(&mut self) {
        self.parser.stop_sync(&mut self.term);
    }

    pub fn resize(&mut self, size: Size) {
        self.term.resize(size);
    }

    /// Scrollback position (0: at the bottom).
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Scrolls the view into (positive) or out of (negative) the history.
    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_pages(&mut self, up: bool) {
        self.term
            .scroll_display(if up { Scroll::PageUp } else { Scroll::PageDown });
    }

    pub fn scroll_to_bottom(&mut self) {
        if self.display_offset() != 0 {
            self.term.scroll_display(Scroll::Bottom);
        }
    }

    /// Grid point under a viewport cell.
    fn point(&self, col: usize, row: usize) -> Point {
        let size = self.size();
        let viewport = Point::new(
            row.min(size.rows.saturating_sub(1)),
            Column(col.min(size.cols.saturating_sub(1))),
        );
        viewport_to_point(self.display_offset(), viewport)
    }

    /// The link at a viewport cell, if any.
    pub fn link_at(&self, col: usize, row: usize) -> Option<super::link::Link> {
        super::link::link_at(&self.term, self.point(col, row))
    }

    /// Starts a selection at a viewport cell. `side` tells which half of
    /// the cell the pointer is on.
    pub fn start_selection(&mut self, kind: SelectionType, col: usize, row: usize, side: Side) {
        let point = self.point(col, row);
        self.term.selection = Some(Selection::new(kind, point, side));
    }

    /// Moves the end of the current selection.
    pub fn update_selection(&mut self, col: usize, row: usize, side: Side) {
        let point = self.point(col, row);
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side);
        }
    }

    /// Selected text, if anything is selected.
    pub fn selection_text(&self) -> Option<String> {
        let selection = self.term.selection.as_ref()?;
        if selection.is_empty() {
            return None;
        }
        self.term
            .selection_to_string()
            .filter(|text| !text.is_empty())
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// Text of a viewport row (for tests and diagnostics).
    #[cfg(test)]
    pub fn row_text(&self, row: usize) -> String {
        let line = alacritty_terminal::index::Line(row as i32 - self.display_offset() as i32);
        let cols = self.size().cols;
        self.term
            .bounds_to_string(
                Point::new(line, Column(0)),
                Point::new(line, Column(cols - 1)),
            )
            .trim_end()
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal(cols: usize, rows: usize) -> Terminal {
        Terminal::new(Size { cols, rows }, 100)
    }

    #[test]
    fn processes_output_and_modes() {
        let mut t = terminal(20, 5);
        t.feed(b"hello\r\n\x1b[31mred\x1b[0m");
        assert_eq!(t.row_text(0), "hello");
        assert_eq!(t.row_text(1), "red");
        assert!(!t.mode().contains(TermMode::APP_CURSOR));
        t.feed(b"\x1b[?1h\x1b[?2004h\x1b[?1000;1006h");
        let mode = t.mode();
        assert!(mode.contains(TermMode::APP_CURSOR));
        assert!(mode.contains(TermMode::BRACKETED_PASTE));
        assert!(mode.contains(TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE));
    }

    #[test]
    fn answers_device_queries() {
        let mut t = terminal(20, 5);
        t.feed(b"\x1b[6n\x1b]2;my title\x07");
        let events = t.take_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::PtyWrite(s) if s == "\x1b[1;1R"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Title(s) if s == "my title"))
        );
        assert!(t.take_events().is_empty());
    }

    #[test]
    fn selection_to_text() {
        let mut t = terminal(20, 4);
        t.feed("first line\r\nsecond 한글 line\r\nthird".as_bytes());
        // From the "l" of "line" on row 0 to the end of "한글" on row 1.
        t.start_selection(SelectionType::Simple, 6, 0, Side::Left);
        t.update_selection(10, 1, Side::Right);
        assert_eq!(t.selection_text().as_deref(), Some("line\nsecond 한글"));
        // A click without movement selects nothing.
        t.start_selection(SelectionType::Simple, 3, 2, Side::Left);
        assert_eq!(t.selection_text(), None);
        // Double-click selects a word; triple-click a line.
        t.start_selection(SelectionType::Semantic, 1, 1, Side::Left);
        assert_eq!(t.selection_text().as_deref(), Some("second"));
        t.start_selection(SelectionType::Lines, 0, 2, Side::Left);
        assert_eq!(t.selection_text().as_deref(), Some("third\n"));
        t.clear_selection();
        assert!(t.term().selection.is_none());
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let mut t = terminal(10, 2);
        t.feed("a한b".as_bytes());
        let grid = t.term().grid();
        let row = &grid[alacritty_terminal::index::Line(0)];
        use alacritty_terminal::term::cell::Flags;
        assert_eq!(row[Column(1)].c, '한');
        assert!(row[Column(1)].flags.contains(Flags::WIDE_CHAR));
        assert!(row[Column(2)].flags.contains(Flags::WIDE_CHAR_SPACER));
        assert_eq!(row[Column(3)].c, 'b');
    }

    #[test]
    fn scrollback_and_selection_in_history() {
        let mut t = terminal(10, 3);
        for i in 0..10 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(t.row_text(0), "line8");
        t.scroll(5);
        assert_eq!(t.display_offset(), 5);
        assert_eq!(t.row_text(0), "line3");
        t.start_selection(SelectionType::Lines, 0, 0, Side::Left);
        assert_eq!(t.selection_text().as_deref(), Some("line3\n"));
        t.scroll(100);
        assert_eq!(t.display_offset(), 8);
        t.scroll_to_bottom();
        assert_eq!(t.display_offset(), 0);
    }

    #[test]
    fn resizes() {
        let mut t = terminal(10, 3);
        t.resize(Size { cols: 40, rows: 12 });
        assert_eq!(t.size(), Size { cols: 40, rows: 12 });
    }
}

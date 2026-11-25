//! The interactive part of an SSH session: remote output goes through the
//! terminal emulator and is rendered into the display at a bounded frame
//! rate; client input (keys, mouse, clipboard, resizes) is translated for
//! the remote side or handled locally (scrollback, selection, paste).

use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, WindowSize};
use alacritty_terminal::index::Side;
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb as VteRgb};
use molcajete_protocol::{InstructionBuilder, Status};
use russh::{ChannelMsg, ChannelReadHalf};
use tokio::sync::mpsc;
use tracing::{debug, info};

use super::client::{Outgoing, PtySize};
use super::keys::{self, KeyOptions, Modifiers};
use super::link::{self, Link};
use super::mouse::{self, MouseAction};
use super::render::{Pointer, Renderer};
use super::settings::Settings;
use super::terminal::{Size, Terminal};
use crate::display::Display;
use crate::io::GuacWriter;
use crate::send_error;
use crate::session::{self, ClientEvent, Input, InputDecoder};

/// Frames the client may have in flight (sent, not yet acknowledged);
/// rendering pauses while this many are pending.
const MAX_UNACKED_FRAMES: u32 = 2;
/// Output arriving this soon after the first change joins the same frame.
const COALESCE: Duration = Duration::from_millis(3);
/// Interval of `nop` keepalives (and of forced frames for stalled clients).
pub const KEEPALIVE: Duration = Duration::from_secs(5);
/// Clicks closer than this (at the same cell) count as double/triple.
const MULTI_CLICK: Duration = Duration::from_millis(400);
/// Lines scrolled per wheel step.
const WHEEL_LINES: i32 = 3;

/// Pointer and selection state.
#[derive(Default)]
struct MouseState {
    mask: u32,
    cell: Option<(usize, usize, Side)>,
    selecting: bool,
    last_click: Option<(Instant, usize, usize)>,
    clicks: u8,
    /// Pointer image in use.
    pointer: Option<PointerKind>,
    /// Link under the pointer, as last told to the client.
    link: Option<Link>,
}

/// Pointer images.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PointerKind {
    /// Selecting text.
    Text,
    /// The application gets mouse events.
    Arrow,
    /// Over a link.
    Hand,
}

/// Instruction telling the client that nothing is selected any more, so
/// that a copy shortcut does not copy the previous selection. A selection
/// arrives as `clipboard`. Stock guacd never sends it, and clients that do
/// not know it ignore it.
fn selection_cleared() -> String {
    InstructionBuilder::new("selection").arg(0).finish()
}

pub struct Bridge {
    settings: Settings,
    terminal: Terminal,
    renderer: Renderer,
    display: Display,
    writer: GuacWriter,
    ssh: mpsc::UnboundedSender<Outgoing>,
    mods: Modifiers,
    mouse: MouseState,
    /// Latest clipboard contents received from the client.
    clipboard: String,
    dirty: bool,
    dirty_since: Instant,
}

impl Bridge {
    pub fn new(
        settings: Settings,
        renderer: Renderer,
        display: Display,
        writer: GuacWriter,
        ssh: mpsc::UnboundedSender<Outgoing>,
    ) -> Self {
        let size = renderer.grid_size(display.width(), display.height());
        let mut bridge = Self {
            terminal: Terminal::new(size, settings.scrollback),
            settings,
            renderer,
            display,
            writer,
            ssh,
            mods: Modifiers::default(),
            mouse: MouseState::default(),
            clipboard: String::new(),
            dirty: true,
            dirty_since: Instant::now(),
        };
        bridge.renderer.invalidate(size);
        bridge.set_pointer(PointerKind::Text);
        bridge
    }

    /// Current PTY size for a terminal size.
    pub fn pty_size(renderer: &Renderer, size: Size) -> PtySize {
        let m = renderer.metrics();
        PtySize {
            cols: size.cols as u32,
            rows: size.rows as u32,
            width: size.cols as u32 * m.width,
            height: size.rows as u32 * m.height,
        }
    }

    /// Runs until the shell exits or the client leaves.
    pub async fn run(
        mut self,
        mut channel: ChannelReadHalf,
        mut inputs: mpsc::Receiver<Input>,
        mut decoder: InputDecoder,
    ) -> anyhow::Result<()> {
        let mut keepalive = tokio::time::interval(KEEPALIVE);
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let deadline = self.frame_deadline();
            // Client input first so that it is never starved by a flood of
            // output; frames are rendered between events (see `settle`).
            tokio::select! {
                biased;
                ins = inputs.recv() => {
                    let Some(ins) = ins else {
                        debug!("client disconnected");
                        break;
                    };
                    let event = decoder.decode(&ins, self.writer.buffer());
                    if let Some(event) = event
                        && !self.client_event(event)
                    {
                        break;
                    }
                }
                msg = channel.wait() => match msg {
                    Some(ChannelMsg::Data { data }) => self.output(&data),
                    Some(ChannelMsg::ExtendedData { data, .. }) => self.output(&data),
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        info!(exit_status, "remote command exited");
                    }
                    Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                        info!(signal = ?signal_name, "remote command killed");
                    }
                    Some(ChannelMsg::Failure) => {
                        let message = "The SSH server refused to start the session";
                        send_error(&mut self.writer, message, Status::UPSTREAM_ERROR).await;
                        break;
                    }
                    Some(ChannelMsg::Close) | None => {
                        debug!("SSH channel closed");
                        break;
                    }
                    _ => {}
                },
                _ = sleep_until(deadline), if deadline.is_some() => {}
                _ = keepalive.tick() => {
                    // A stalled client must not freeze the screen forever.
                    if self.dirty && self.display.unacked_frames() >= MAX_UNACKED_FRAMES {
                        self.render_frame();
                    }
                    self.writer.push("3.nop;");
                }
            }
            self.settle().await?;
        }
        // Show the final output (e.g. the shell's goodbye) before leaving.
        if self.dirty {
            self.render_frame();
        }
        self.writer.push("10.disconnect;");
        let _ = self.writer.flush().await;
        Ok(())
    }

    /// Handles terminal events, renders a frame when one is due and sends
    /// pending output to the client.
    async fn settle(&mut self) -> std::io::Result<()> {
        self.terminal_events();
        let now = Instant::now();
        if self.terminal.sync_deadline().is_some_and(|d| now >= d) {
            self.terminal.finish_sync();
            self.mark_dirty();
        }
        if self.frame_deadline().is_some_and(|d| now >= d) {
            self.render_frame();
        }
        if self.writer.pending() > 0 {
            self.writer.flush().await?;
        }
        Ok(())
    }

    /// When the next frame should be drawn, if one is needed and allowed.
    fn frame_deadline(&self) -> Option<Instant> {
        if !self.dirty {
            return None;
        }
        // Wait for the end of a synchronized update (DEC mode 2026).
        if let Some(sync) = self.terminal.sync_deadline() {
            return Some(sync);
        }
        // Wait for the client to catch up; its `sync` reply wakes us.
        if self.display.unacked_frames() >= MAX_UNACKED_FRAMES {
            return None;
        }
        // Shared pacing: at most ~30 fps while output keeps coming, slower when
        // the client lags; the first change after a pause goes out at once.
        Some(
            self.display
                .next_frame_at()
                .max(self.dirty_since + COALESCE),
        )
    }

    fn mark_dirty(&mut self) {
        if !self.dirty {
            self.dirty = true;
            self.dirty_since = Instant::now();
        }
    }

    fn render_frame(&mut self) {
        // Output may have moved or replaced the link under the pointer.
        self.update_link();
        let before = self.writer.pending();
        let (renderer, terminal, display, writer) = (
            &mut self.renderer,
            &self.terminal,
            &mut self.display,
            &mut self.writer,
        );
        let (rows, stats) = tokio::task::block_in_place(|| {
            let rows = renderer.render(terminal.term(), display);
            (rows, display.flush(writer.buffer()))
        });
        if self.writer.pending() > before {
            self.display.sync(self.writer.buffer());
            debug!(
                rows,
                images = stats.images,
                fills = stats.fills,
                bytes = stats.bytes,
                "frame sent"
            );
        }
        self.dirty = false;
    }

    fn output(&mut self, data: &[u8]) {
        self.terminal.feed(data);
        self.mark_dirty();
    }

    fn send(&self, bytes: Vec<u8>) {
        // A closed writer means the channel is going away; the read side
        // reports it.
        let _ = self.ssh.send(Outgoing::Data(bytes));
    }

    /// Acts on events raised by the terminal emulator.
    fn terminal_events(&mut self) {
        for event in self.terminal.take_events() {
            match event {
                Event::PtyWrite(text) => self.send(text.into_bytes()),
                Event::Title(title) => session::send_name(self.writer.buffer(), &title),
                Event::ResetTitle => {
                    session::send_name(self.writer.buffer(), &self.settings.hostname)
                }
                Event::ClipboardStore(_, text) if !self.settings.disable_copy => {
                    session::send_clipboard(self.writer.buffer(), &text);
                }
                Event::ColorRequest(index, format) => {
                    let color = match index {
                        0..=255 => Color::Indexed(index as u8),
                        257 => Color::Named(NamedColor::Background),
                        _ => Color::Named(NamedColor::Foreground),
                    };
                    let [r, g, b] = self
                        .renderer
                        .scheme()
                        .resolve(color, self.terminal.term().colors());
                    self.send(format(VteRgb { r, g, b }).into_bytes());
                }
                Event::TextAreaSizeRequest(format) => {
                    let size = self.terminal.size();
                    let m = self.renderer.metrics();
                    let window = WindowSize {
                        num_lines: size.rows as u16,
                        num_cols: size.cols as u16,
                        cell_width: m.width as u16,
                        cell_height: m.height as u16,
                    };
                    self.send(format(window).into_bytes());
                }
                _ => {}
            }
        }
    }

    /// Handles one client event; returns `false` to end the session.
    fn client_event(&mut self, event: ClientEvent) -> bool {
        match event {
            ClientEvent::Key { keysym, pressed } => self.key(keysym, pressed),
            ClientEvent::Mouse { x, y, mask } => self.pointer(x, y, mask),
            ClientEvent::Size { width, height } => self.resize(width, height),
            ClientEvent::Clipboard(text) if !self.settings.disable_paste => self.clipboard = text,
            ClientEvent::Sync {
                timestamp,
                received,
            } => self.display.ack(timestamp, received),
            ClientEvent::Disconnect => return false,
            _ => {}
        }
        true
    }

    fn key(&mut self, keysym: u32, pressed: bool) {
        if self.mods.update(keysym, pressed) {
            // Shift lets the pointer bypass mouse reporting, links included.
            self.update_link();
            return;
        }
        if !pressed {
            return;
        }
        let mode = self.terminal.mode();
        let alt_screen = mode.contains(TermMode::ALT_SCREEN);
        // Local shortcuts: Shift+PgUp/PgDn scroll the history,
        // Ctrl+Shift+V pastes the clipboard (like guacd).
        if self.mods.shift && !self.mods.ctrl && !alt_screen && matches!(keysym, 0xFF55 | 0xFF56) {
            self.terminal.scroll_pages(keysym == 0xFF55);
            self.mark_dirty();
            return;
        }
        if self.mods.ctrl && self.mods.shift && matches!(keysym, 0x56 | 0x76) {
            self.paste();
            return;
        }
        if self.settings.read_only {
            return;
        }
        let options = KeyOptions {
            backspace: self.settings.backspace,
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            app_keypad: mode.contains(TermMode::APP_KEYPAD),
            linux: self.settings.linux_keys(),
        };
        if let Some(bytes) = keys::encode_key(keysym, self.mods, &options) {
            self.scroll_to_bottom();
            self.send(bytes);
        }
    }

    fn scroll_to_bottom(&mut self) {
        if self.terminal.display_offset() != 0 {
            self.terminal.scroll_to_bottom();
            self.mark_dirty();
        }
    }

    /// Sends the client clipboard to the remote side.
    fn paste(&mut self) {
        if self.settings.read_only || self.settings.disable_paste || self.clipboard.is_empty() {
            return;
        }
        let bracketed = self.terminal.mode().contains(TermMode::BRACKETED_PASTE);
        let data = paste_bytes(&self.clipboard, bracketed);
        self.scroll_to_bottom();
        self.send(data);
    }

    fn pointer(&mut self, x: i32, y: i32, mask: u32) {
        let m = self.renderer.metrics();
        let size = self.terminal.size();
        let (x, y) = (x.max(0) as u32, y.max(0) as u32);
        let col = ((x / m.width) as usize).min(size.cols - 1);
        let row = ((y / m.height) as usize).min(size.rows - 1);
        let side = if x % m.width < m.width / 2 {
            Side::Left
        } else {
            Side::Right
        };
        let pressed = mask & !self.mouse.mask;
        let released = self.mouse.mask & !mask;
        self.mouse.mask = mask;
        let moved = self.mouse.cell.is_none_or(|(c, r, _)| (c, r) != (col, row));
        let side_changed = self.mouse.cell.is_none_or(|(_, _, s)| s != side);
        self.mouse.cell = Some((col, row, side));

        let mode = self.terminal.mode();
        let reporting = self.reporting();
        self.update_link();
        if reporting {
            self.report_mouse(pressed, released, moved, col, row, mode);
            return;
        }

        if pressed & mouse::WHEEL_UP != 0 {
            self.wheel(true);
        }
        if pressed & mouse::WHEEL_DOWN != 0 {
            self.wheel(false);
        }
        if pressed & mouse::LEFT != 0 {
            self.start_selection(col, row, side);
        } else if mask & mouse::LEFT != 0 && self.mouse.selecting && (moved || side_changed) {
            self.terminal.update_selection(col, row, side);
            self.mark_dirty();
        }
        if released & mouse::LEFT != 0 && self.mouse.selecting {
            self.finish_selection();
        }
        // Right or middle click pastes, like guacd.
        if released & (mouse::MIDDLE | mouse::RIGHT) != 0 {
            self.paste();
        }
    }

    fn report_mouse(
        &mut self,
        pressed: u32,
        released: u32,
        moved: bool,
        col: usize,
        row: usize,
        mode: TermMode,
    ) {
        let buttons = [
            mouse::LEFT,
            mouse::MIDDLE,
            mouse::RIGHT,
            mouse::WHEEL_UP,
            mouse::WHEEL_DOWN,
        ];
        let mut reports = Vec::new();
        for button in buttons {
            let Some(code) = mouse::button_code(button) else {
                continue;
            };
            if pressed & button != 0 {
                reports.push(MouseAction::Press(code));
            }
            if released & button != 0 {
                reports.push(MouseAction::Release(code));
            }
        }
        if reports.is_empty() && moved {
            let held = [mouse::LEFT, mouse::MIDDLE, mouse::RIGHT]
                .into_iter()
                .find(|b| self.mouse.mask & b != 0)
                .and_then(mouse::button_code);
            reports.push(MouseAction::Motion(held));
        }
        let bytes: Vec<u8> = reports
            .into_iter()
            .filter_map(|action| mouse::encode_report(action, col, row, self.mods, mode))
            .flatten()
            .collect();
        if !bytes.is_empty() {
            self.send(bytes);
        }
    }

    /// Wheel without mouse reporting: scroll the history, or send cursor
    /// keys to full-screen applications (alternate scroll mode, like
    /// xterm), so `less` and `man` scroll too.
    fn wheel(&mut self, up: bool) {
        let mode = self.terminal.mode();
        if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            if self.settings.read_only {
                return;
            }
            let arrow = match (up, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            self.send(arrow.repeat(WHEEL_LINES as usize));
        } else {
            self.terminal
                .scroll(if up { WHEEL_LINES } else { -WHEEL_LINES });
            self.mark_dirty();
        }
    }

    fn start_selection(&mut self, col: usize, row: usize, side: Side) {
        let now = Instant::now();
        let repeated = self
            .mouse
            .last_click
            .is_some_and(|(t, c, r)| now.duration_since(t) <= MULTI_CLICK && (c, r) == (col, row));
        self.mouse.clicks = if repeated {
            self.mouse.clicks % 3 + 1
        } else {
            1
        };
        self.mouse.last_click = Some((now, col, row));
        let kind = match self.mouse.clicks {
            1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        self.terminal.start_selection(kind, col, row, side);
        self.mouse.selecting = true;
        self.mark_dirty();
    }

    /// Copies the selection to the client clipboard when the button is
    /// released; a plain click clears it.
    fn finish_selection(&mut self) {
        self.mouse.selecting = false;
        match self.terminal.selection_text() {
            Some(text) => {
                if !self.settings.disable_copy {
                    session::send_clipboard(self.writer.buffer(), &text);
                }
            }
            None => {
                self.terminal.clear_selection();
                self.writer.push(&selection_cleared());
                self.mark_dirty();
            }
        }
    }

    /// Whether the application gets mouse events (Shift bypasses it).
    fn reporting(&self) -> bool {
        self.terminal.mode().intersects(TermMode::MOUSE_MODE)
            && !self.mods.shift
            && !self.settings.read_only
    }

    /// Finds the link under the pointer, tells the client when it changed,
    /// underlines it and picks the pointer image. Links are not offered
    /// while the application gets mouse events or a button is held.
    fn update_link(&mut self) {
        let reporting = self.reporting();
        let link = match self.mouse.cell {
            Some((col, row, _)) if !reporting && self.mouse.mask == 0 && !self.mouse.selecting => {
                self.terminal.link_at(col, row)
            }
            _ => None,
        };
        if link != self.mouse.link {
            self.writer.push(&link::instruction(link.as_ref()));
            self.renderer
                .set_link(link.as_ref().map(|link| (link.start, link.end)));
            self.mouse.link = link;
            self.mark_dirty();
        }
        self.set_pointer(if self.mouse.link.is_some() {
            PointerKind::Hand
        } else if reporting {
            PointerKind::Arrow
        } else {
            PointerKind::Text
        });
    }

    fn set_pointer(&mut self, kind: PointerKind) {
        if self.mouse.pointer == Some(kind) {
            return;
        }
        self.mouse.pointer = Some(kind);
        let p = match kind {
            PointerKind::Text => Pointer::ibeam(),
            PointerKind::Arrow => Pointer::arrow(),
            PointerKind::Hand => Pointer::hand(),
        };
        self.display.set_cursor(
            p.hotspot.0,
            p.hotspot.1,
            p.width,
            p.height,
            &p.rgba,
            self.writer.buffer(),
        );
    }

    fn resize(&mut self, width: u32, height: u32) {
        if (width, height) == (self.display.width(), self.display.height()) {
            return;
        }
        self.display.resize(width, height, self.writer.buffer());
        let size = self.renderer.grid_size(width, height);
        if size != self.terminal.size() {
            self.terminal.resize(size);
            let _ = self
                .ssh
                .send(Outgoing::Resize(Self::pty_size(&self.renderer, size)));
        }
        self.renderer.invalidate(size);
        self.mark_dirty();
    }
}

/// Bytes sent for pasted text: bracketed (with ESC removed so the paste
/// cannot end the bracket early) or with newlines turned into carriage
/// returns, as typed.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut out = b"\x1b[200~".to_vec();
        out.extend(text.replace('\x1b', "").bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline.into()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_modes() {
        assert_eq!(paste_bytes("a\nb\r\nc", false), b"a\rb\rc");
        assert_eq!(
            paste_bytes("ls\x1b[201~\n", true),
            b"\x1b[200~ls[201~\n\x1b[201~"
        );
    }
}

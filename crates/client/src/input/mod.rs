//! Keyboard and mouse state tracking. DOM listeners live in `client.rs`;
//! this module only turns browser events into Guacamole key/mouse updates.

pub mod keysym;

use std::collections::HashMap;

use molcajete_protocol::MouseButtons;

/// Tracks pressed keys by physical key (`KeyboardEvent.code`) so that each
/// release reports the same keysym as its press, even if modifiers changed
/// in between (press Shift, press "a" → "A", release Shift, release "a").
#[derive(Default)]
pub struct Keyboard {
    pressed: HashMap<String, u32>,
}

/// A key press (`true`) or release (`false`).
pub type KeyEvent = (u32, bool);

impl Keyboard {
    /// Handles `keydown`. Returns the events to send, or `None` when the
    /// event should be left to the browser (IME composition, dead keys).
    pub fn key_down(
        &mut self,
        key: &str,
        code: &str,
        location: u32,
        composing: bool,
    ) -> Option<Vec<KeyEvent>> {
        if composing || key == "Process" || key == "Dead" {
            return None;
        }
        let keysym = keysym::keysym_for_event(key, code, location)?;
        let id = if code.is_empty() { key } else { code };
        let mut events = Vec::with_capacity(2);
        if let Some(previous) = self.pressed.insert(id.to_owned(), keysym)
            && previous != keysym
        {
            events.push((previous, false));
        }
        events.push((keysym, true));
        Some(events)
    }

    pub fn key_up(&mut self, key: &str, code: &str, location: u32) -> Vec<KeyEvent> {
        let id = if code.is_empty() { key } else { code };
        match self.pressed.remove(id) {
            Some(keysym) => vec![(keysym, false)],
            None => {
                // Release without a matching press (e.g. focus moved in while
                // the key was held). Releasing an unpressed key is harmless.
                keysym::keysym_for_event(key, code, location)
                    .map(|k| vec![(k, false)])
                    .unwrap_or_default()
            }
        }
    }

    /// Releases everything (on blur or disconnect).
    pub fn release_all(&mut self) -> Vec<KeyEvent> {
        self.pressed
            .drain()
            .map(|(_, keysym)| (keysym, false))
            .collect()
    }

    pub fn any_pressed(&self) -> bool {
        !self.pressed.is_empty()
    }
}

/// Events needed to type text (from IME composition) on the remote side.
pub fn type_text(text: &str) -> Vec<KeyEvent> {
    let mut events = Vec::with_capacity(text.len() * 2);
    for c in text.chars() {
        let keysym = match c {
            '\n' | '\r' => Some(0xFF0D),
            '\t' => Some(0xFF09),
            other => keysym::keysym_from_codepoint(other as u32),
        };
        if let Some(k) = keysym {
            events.push((k, true));
            events.push((k, false));
        }
    }
    events
}

/// Accumulates wheel deltas into discrete scroll "clicks".
#[derive(Default)]
pub struct WheelAccumulator {
    delta: f64,
}

impl WheelAccumulator {
    /// Pixels of scrolling that make up one click (as in `Guacamole.Mouse`).
    const THRESHOLD: f64 = 53.0;
    const PIXELS_PER_LINE: f64 = 18.0;
    const PIXELS_PER_PAGE: f64 = Self::PIXELS_PER_LINE * 16.0;

    /// Returns the number of clicks: negative = up, positive = down.
    pub fn push(&mut self, delta_y: f64, delta_mode: u32) -> i32 {
        let px = match delta_mode {
            1 => delta_y * Self::PIXELS_PER_LINE,
            2 => delta_y * Self::PIXELS_PER_PAGE,
            _ => delta_y,
        };
        // A fresh gesture in the opposite direction should react immediately.
        if px.signum() != self.delta.signum() {
            self.delta = 0.0;
        }
        self.delta += px;
        let clicks = (self.delta / Self::THRESHOLD).trunc();
        self.delta -= clicks * Self::THRESHOLD;
        clicks as i32
    }
}

/// Converts DOM `PointerEvent.buttons` to a Guacamole button mask.
pub fn buttons_from_dom(buttons: u16) -> MouseButtons {
    let mut mask = MouseButtons::default();
    mask.set(MouseButtons::LEFT, buttons & 1 != 0);
    mask.set(MouseButtons::RIGHT, buttons & 2 != 0);
    mask.set(MouseButtons::MIDDLE, buttons & 4 != 0);
    mask.set(MouseButtons::BACK, buttons & 8 != 0);
    mask.set(MouseButtons::FORWARD, buttons & 16 != 0);
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_uses_pressed_keysym() {
        let mut kb = Keyboard::default();
        assert_eq!(
            kb.key_down("Shift", "ShiftLeft", 1, false),
            Some(vec![(keysym::SHIFT_L, true)])
        );
        assert_eq!(
            kb.key_down("A", "KeyA", 0, false),
            Some(vec![('A' as u32, true)])
        );
        assert_eq!(
            kb.key_up("Shift", "ShiftLeft", 1),
            vec![(keysym::SHIFT_L, false)]
        );
        // Shift is up now, but the release must still be "A".
        assert_eq!(kb.key_up("a", "KeyA", 0), vec![('A' as u32, false)]);
        assert!(!kb.any_pressed());
    }

    #[test]
    fn composition_is_left_to_the_browser() {
        let mut kb = Keyboard::default();
        assert_eq!(kb.key_down("Process", "KeyG", 0, true), None);
        assert_eq!(kb.key_down("ㅎ", "KeyG", 0, true), None);
    }

    #[test]
    fn typing_text_presses_and_releases() {
        assert_eq!(
            type_text("한a"),
            vec![
                (0x0100_D55C, true),
                (0x0100_D55C, false),
                (0x61, true),
                (0x61, false)
            ]
        );
    }

    #[test]
    fn wheel_accumulates() {
        let mut w = WheelAccumulator::default();
        assert_eq!(w.push(120.0, 0), 2);
        assert_eq!(w.push(-3.0, 1), -1);
        // Tiny trackpad deltas accumulate until they reach one click.
        let mut w = WheelAccumulator::default();
        let total: i32 = (0..13).map(|_| w.push(4.0, 0)).sum();
        assert_eq!(total, 0);
        assert_eq!(w.push(4.0, 0), 1);
    }

    #[test]
    fn dom_buttons() {
        assert_eq!(buttons_from_dom(1).0, MouseButtons::LEFT);
        assert_eq!(buttons_from_dom(2).0, MouseButtons::RIGHT);
        assert_eq!(buttons_from_dom(4).0, MouseButtons::MIDDLE);
    }
}

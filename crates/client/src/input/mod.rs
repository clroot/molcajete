//! Keyboard and mouse state tracking. DOM listeners live in `client.rs`;
//! this module only turns browser events into Guacamole key/mouse updates.

pub mod keysym;

use std::collections::HashMap;

use molcajete_protocol::MouseButtons;

/// Who composes text typed with an input method (Hangul, …).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Composition {
    /// The browser's IME composes; finished text is typed as Unicode keysyms.
    #[default]
    Local,
    /// Keys are sent by their physical position (US layout) as they are
    /// pressed and the remote host's IME composes, as native RDP and VNC
    /// clients do.
    Remote {
        /// Caps Lock switches the remote input mode (Hangul/English) instead
        /// of locking capitals: the key macOS uses to switch input sources.
        caps_lock_toggles: bool,
    },
}

/// Tracks pressed keys by physical key (`KeyboardEvent.code`) so that each
/// release reports the same keysym as its press, even if modifiers changed
/// in between (press Shift, press "a" → "A", release Shift, release "a").
#[derive(Default)]
pub struct Keyboard {
    pressed: HashMap<String, u32>,
    composition: Composition,
    /// Whether capitals are locked, as last reported by the browser.
    caps_locked: bool,
    /// A Caps Lock `keydown` was taken as a toggle; its `keyup` is not one.
    caps_toggle_down: bool,
}

/// A key press (`true`) or release (`false`).
pub type KeyEvent = (u32, bool);

fn tap(keysym: u32) -> Vec<KeyEvent> {
    vec![(keysym, true), (keysym, false)]
}

impl Keyboard {
    pub fn set_composition(&mut self, composition: Composition) {
        self.composition = composition;
        self.caps_toggle_down = false;
    }

    pub fn composition(&self) -> Composition {
        self.composition
    }

    fn caps_lock_toggles(&self) -> bool {
        matches!(
            self.composition,
            Composition::Remote {
                caps_lock_toggles: true
            }
        )
    }

    fn shift_held(&self) -> bool {
        self.pressed
            .values()
            .any(|&k| k == keysym::SHIFT_L || k == keysym::SHIFT_R)
    }

    /// Caps Lock while it toggles the remote input mode. A press that
    /// changes the lock state (a long press on macOS) is still Caps Lock;
    /// any other press is the toggle. Browsers on macOS report only
    /// `keydown` or only `keyup` for some presses, so either counts once.
    fn caps_lock_event(&mut self, down: bool, locked: bool) -> Vec<KeyEvent> {
        if locked != self.caps_locked {
            self.caps_locked = locked;
            self.caps_toggle_down = false;
            return tap(keysym::CAPS_LOCK);
        }
        if down {
            // Auto-repeat must not toggle again.
            let first = !self.caps_toggle_down;
            self.caps_toggle_down = true;
            return if first {
                tap(keysym::HANGUL)
            } else {
                Vec::new()
            };
        }
        if std::mem::take(&mut self.caps_toggle_down) {
            Vec::new()
        } else {
            tap(keysym::HANGUL)
        }
    }

    /// The keysym to send for a key the local input method claimed, when the
    /// remote host composes: the key's US-layout character.
    fn physical_keysym(&self, key: &str, code: &str, composing: bool) -> Option<u32> {
        if self.composition == Composition::Local {
            return None;
        }
        if !(composing || key == "Process" || keysym::is_hangul_key(key)) {
            return None;
        }
        keysym::us_layout_char(code, self.shift_held())
            .map(|c| c as u32)
            .or_else(|| keysym::editing_key_by_code(code))
    }

    /// Handles `keydown`. Returns the events to send, or `None` when the
    /// event should be left to the browser (IME composition, dead keys).
    /// `caps_lock` is the lock state the event reports.
    pub fn key_down(
        &mut self,
        key: &str,
        code: &str,
        location: u32,
        composing: bool,
        caps_lock: bool,
    ) -> Option<Vec<KeyEvent>> {
        if code == "CapsLock" && self.caps_lock_toggles() {
            return Some(self.caps_lock_event(true, caps_lock));
        }
        self.caps_locked = caps_lock;
        let keysym = match self.physical_keysym(key, code, composing) {
            Some(keysym) => keysym,
            None if composing || key == "Process" || key == "Dead" => return None,
            None => keysym::keysym_for_event(key, code, location)?,
        };
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

    pub fn key_up(
        &mut self,
        key: &str,
        code: &str,
        location: u32,
        caps_lock: bool,
    ) -> Vec<KeyEvent> {
        if code == "CapsLock" && self.caps_lock_toggles() {
            return self.caps_lock_event(false, caps_lock);
        }
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
        self.caps_toggle_down = false;
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
            kb.key_down("Shift", "ShiftLeft", 1, false, false),
            Some(vec![(keysym::SHIFT_L, true)])
        );
        assert_eq!(
            kb.key_down("A", "KeyA", 0, false, false),
            Some(vec![('A' as u32, true)])
        );
        assert_eq!(
            kb.key_up("Shift", "ShiftLeft", 1, false),
            vec![(keysym::SHIFT_L, false)]
        );
        // Shift is up now, but the release must still be "A".
        assert_eq!(kb.key_up("a", "KeyA", 0, false), vec![('A' as u32, false)]);
        assert!(!kb.any_pressed());
    }

    #[test]
    fn composition_is_left_to_the_browser() {
        let mut kb = Keyboard::default();
        assert_eq!(kb.key_down("Process", "KeyG", 0, true, false), None);
        assert_eq!(kb.key_down("ㅎ", "KeyG", 0, true, false), None);
    }

    fn remote(caps_lock_toggles: bool) -> Keyboard {
        let mut kb = Keyboard::default();
        kb.set_composition(Composition::Remote { caps_lock_toggles });
        kb
    }

    #[test]
    fn remote_composition_sends_physical_keys() {
        let mut kb = remote(false);
        // A composing IME reports "Process"; an inactive one the jamo.
        assert_eq!(
            kb.key_down("Process", "KeyG", 0, true, false),
            Some(vec![('g' as u32, true)])
        );
        assert_eq!(
            kb.key_up("Process", "KeyG", 0, false),
            vec![('g' as u32, false)]
        );
        assert_eq!(
            kb.key_down("ㅏ", "KeyK", 0, false, false),
            Some(vec![('k' as u32, true)])
        );
        assert_eq!(kb.key_up("ㅏ", "KeyK", 0, false), vec![('k' as u32, false)]);
        // Shift selects the double consonants on the remote side.
        kb.key_down("Shift", "ShiftLeft", 1, false, false);
        assert_eq!(
            kb.key_down("ㅃ", "KeyQ", 0, false, false),
            Some(vec![('Q' as u32, true)])
        );
        kb.key_up("ㅃ", "KeyQ", 0, false);
        kb.key_up("Shift", "ShiftLeft", 1, false);
        // Editing keys claimed by the IME are forwarded too.
        assert_eq!(
            kb.key_down("Process", "Backspace", 0, true, false),
            Some(vec![(0xFF08, true)])
        );
        // Latin text and named keys are unchanged.
        assert_eq!(
            kb.key_down("é", "KeyE", 0, false, false),
            Some(vec![(0xE9, true)])
        );
        assert_eq!(
            kb.key_down("HangulMode", "Lang1", 0, false, false),
            Some(vec![(keysym::HANGUL, true)])
        );
        // Dead keys still compose locally.
        assert_eq!(kb.key_down("Dead", "Quote", 0, false, false), None);
    }

    #[test]
    fn caps_lock_toggles_the_remote_input_mode() {
        let toggle = tap(keysym::HANGUL);
        let caps = tap(keysym::CAPS_LOCK);

        // Press and release both reported: one toggle.
        let mut kb = remote(true);
        assert_eq!(
            kb.key_down("CapsLock", "CapsLock", 0, false, false),
            Some(toggle.clone())
        );
        assert_eq!(
            kb.key_down("CapsLock", "CapsLock", 0, false, false),
            Some(vec![])
        );
        assert_eq!(kb.key_up("CapsLock", "CapsLock", 0, false), vec![]);
        // Only the release reported.
        assert_eq!(kb.key_up("CapsLock", "CapsLock", 0, false), toggle);
        // A press that locks capitals is Caps Lock, and so is its unlock.
        assert_eq!(
            kb.key_down("CapsLock", "CapsLock", 0, false, true),
            Some(caps.clone())
        );
        assert_eq!(kb.key_up("CapsLock", "CapsLock", 0, false), caps);
        assert!(!kb.any_pressed());

        // Capitals already locked when the session started.
        let mut kb = remote(true);
        kb.key_down("A", "KeyA", 0, false, true);
        kb.key_up("A", "KeyA", 0, true);
        assert_eq!(kb.key_up("CapsLock", "CapsLock", 0, false), caps);

        // Without the option, and when composing locally, it stays Caps Lock.
        for mut kb in [remote(false), Keyboard::default()] {
            assert_eq!(
                kb.key_down("CapsLock", "CapsLock", 0, false, true),
                Some(vec![(keysym::CAPS_LOCK, true)])
            );
        }
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

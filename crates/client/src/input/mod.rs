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

/// The modifier flags of a key event (`ctrlKey`, `altKey`, `shiftKey`,
/// `metaKey`, `getModifierState("AltGraph")`) as a bit set.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Modifiers(u32);

impl Modifiers {
    pub const CTRL: u32 = 1;
    pub const ALT: u32 = 2;
    pub const SHIFT: u32 = 4;
    pub const META: u32 = 8;
    pub const ALT_GRAPH: u32 = 16;

    pub fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    fn has(self, bit: u32) -> bool {
        self.0 & bit != 0
    }
}

/// The modifiers kept in step with the event flags: the flag, the code a
/// press that only the flags reported is filed under, and the keysyms of
/// the left and the right key.
const SYNCED_MODIFIERS: [(u32, &str, [u32; 2]); 4] = [
    (
        Modifiers::CTRL,
        "ControlLeft",
        [keysym::CONTROL_L, keysym::CONTROL_R],
    ),
    (Modifiers::ALT, "AltLeft", [keysym::ALT_L, keysym::ALT_R]),
    (
        Modifiers::SHIFT,
        "ShiftLeft",
        [keysym::SHIFT_L, keysym::SHIFT_R],
    ),
    (
        Modifiers::META,
        "MetaLeft",
        [keysym::SUPER_L, keysym::SUPER_R],
    ),
];

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

    /// Brings Ctrl, Alt, Shift and Meta in line with the flags of a key
    /// event, as `Guacamole.Keyboard` does: a modifier the flags report but
    /// no `keydown` pressed is pressed, one pressed that the flags no
    /// longer report is released. A browser sends a modifier's `keydown`
    /// once, so a stray `keyup` (iPadOS Safari sends one now and then while
    /// the key is held) or a lost one would otherwise last until the key is
    /// pressed again. `own` is the keysym of the event's key, whose flag
    /// describes the key itself and is left alone. With AltGr down the
    /// browser also reports Ctrl and Alt (on Windows), which are left alone
    /// too. `None` (a caller that does not pass the flags) changes nothing.
    fn sync_modifiers(&mut self, flags: Option<Modifiers>, own: Option<u32>) -> Vec<KeyEvent> {
        let Some(flags) = flags else {
            return Vec::new();
        };
        let mut events = Vec::new();
        for (bit, code, keysyms) in SYNCED_MODIFIERS {
            if own.is_some_and(|k| keysyms.contains(&k))
                || (flags.has(Modifiers::ALT_GRAPH)
                    && bit & (Modifiers::CTRL | Modifiers::ALT) != 0)
            {
                continue;
            }
            let held: Vec<String> = self
                .pressed
                .iter()
                .filter(|(_, k)| keysyms.contains(k))
                .map(|(id, _)| id.clone())
                .collect();
            if flags.has(bit) && held.is_empty() {
                self.pressed.insert(code.to_owned(), keysyms[0]);
                events.push((keysyms[0], true));
            } else if !flags.has(bit) {
                for id in held {
                    if let Some(k) = self.pressed.remove(&id) {
                        events.push((k, false));
                    }
                }
            }
        }
        events
    }

    /// Handles `keydown`. Returns the events to send, or `None` when the
    /// event should be left to the browser (IME composition, dead keys).
    /// `caps_lock` is the lock state the event reports, `modifiers` its
    /// modifier flags (see [`Keyboard::sync_modifiers`]).
    pub fn key_down(
        &mut self,
        key: &str,
        code: &str,
        location: u32,
        composing: bool,
        caps_lock: bool,
        modifiers: Option<Modifiers>,
    ) -> Option<Vec<KeyEvent>> {
        if code == "CapsLock" && self.caps_lock_toggles() {
            let mut events = self.sync_modifiers(modifiers, None);
            events.extend(self.caps_lock_event(true, caps_lock));
            return Some(events);
        }
        self.caps_locked = caps_lock;
        let keysym = match self.physical_keysym(key, code, composing) {
            Some(keysym) => keysym,
            None if composing || key == "Process" || key == "Dead" => return None,
            None => keysym::keysym_for_event(key, code, location)?,
        };
        let id = if code.is_empty() { key } else { code };
        let mut events = self.sync_modifiers(modifiers, Some(keysym));
        if let Some(previous) = self.pressed.insert(id.to_owned(), keysym)
            && previous != keysym
        {
            events.push((previous, false));
        }
        events.push((keysym, true));
        Some(events)
    }

    /// Handles `keyup`, with the event's lock state and modifier flags as
    /// for [`Keyboard::key_down`].
    pub fn key_up(
        &mut self,
        key: &str,
        code: &str,
        location: u32,
        caps_lock: bool,
        modifiers: Option<Modifiers>,
    ) -> Vec<KeyEvent> {
        if code == "CapsLock" && self.caps_lock_toggles() {
            let mut events = self.sync_modifiers(modifiers, None);
            events.extend(self.caps_lock_event(false, caps_lock));
            return events;
        }
        let id = if code.is_empty() { key } else { code };
        // Release without a matching press (e.g. focus moved in while the
        // key was held). Releasing an unpressed key is harmless.
        let release = self
            .pressed
            .remove(id)
            .or_else(|| keysym::keysym_for_event(key, code, location));
        let mut events = self.sync_modifiers(modifiers, release);
        events.extend(release.map(|k| (k, false)));
        events
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

    /// Whether a key with one of these keysyms is held.
    pub fn is_down(&self, keysyms: &[u32]) -> bool {
        self.pressed.values().any(|k| keysyms.contains(k))
    }
}

/// How long a modifier pressed from the page stays down.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// Not held.
    #[default]
    Off,
    /// Held for the next key, then released.
    Once,
    /// Held until it is turned off.
    Locked,
}

impl Hold {
    pub fn parse(hold: &str) -> Option<Self> {
        Some(match hold {
            "off" => Self::Off,
            "once" => Self::Once,
            "locked" => Self::Locked,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Once => "once",
            Self::Locked => "locked",
        }
    }
}

/// Ctrl and Alt held with a button of the page instead of a key: an
/// on-screen keyboard has neither, so the page holds one for the next key
/// typed, or until it is turned off.
#[derive(Default)]
pub struct HeldModifiers {
    ctrl: Hold,
    alt: Hold,
}

impl HeldModifiers {
    fn slot(&mut self, name: &str) -> Option<(&mut Hold, u32)> {
        match name {
            "ctrl" => Some((&mut self.ctrl, keysym::CONTROL_L)),
            "alt" => Some((&mut self.alt, keysym::ALT_L)),
            _ => None,
        }
    }

    /// Sets how the modifier `name` (`ctrl` or `alt`) is held. Returns
    /// the press or release that makes the remote side follow, or `None`
    /// for a name that is not one of the two.
    pub fn set(&mut self, name: &str, hold: Hold) -> Option<Vec<KeyEvent>> {
        let (slot, keysym) = self.slot(name)?;
        let was_held = std::mem::replace(slot, hold) != Hold::Off;
        let is_held = hold != Hold::Off;
        Some(if was_held == is_held {
            Vec::new()
        } else {
            vec![(keysym, is_held)]
        })
    }

    pub fn ctrl(&self) -> Hold {
        self.ctrl
    }

    pub fn alt(&self) -> Hold {
        self.alt
    }

    /// Whether the modifier with this keysym is held.
    pub fn is_held(&self, keysym: u32) -> bool {
        match keysym {
            keysym::CONTROL_L => self.ctrl != Hold::Off,
            keysym::ALT_L => self.alt != Hold::Off,
            _ => false,
        }
    }

    /// A key went out with the held modifiers: returns the releases of
    /// those that were held for one key.
    pub fn after_key(&mut self) -> Vec<KeyEvent> {
        let mut events = Vec::new();
        for (slot, keysym) in [
            (&mut self.ctrl, keysym::CONTROL_L),
            (&mut self.alt, keysym::ALT_L),
        ] {
            if *slot == Hold::Once {
                *slot = Hold::Off;
                events.push((keysym, false));
            }
        }
        events
    }

    /// Releases both (on disconnect).
    pub fn release_all(&mut self) -> Vec<KeyEvent> {
        let mut events = Vec::new();
        for (slot, keysym) in [
            (&mut self.ctrl, keysym::CONTROL_L),
            (&mut self.alt, keysym::ALT_L),
        ] {
            if std::mem::take(slot) != Hold::Off {
                events.push((keysym, false));
            }
        }
        events
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
            kb.key_down("Shift", "ShiftLeft", 1, false, false, None),
            Some(vec![(keysym::SHIFT_L, true)])
        );
        assert_eq!(
            kb.key_down("A", "KeyA", 0, false, false, None),
            Some(vec![('A' as u32, true)])
        );
        assert_eq!(
            kb.key_up("Shift", "ShiftLeft", 1, false, None),
            vec![(keysym::SHIFT_L, false)]
        );
        // Shift is up now, but the release must still be "A".
        assert_eq!(
            kb.key_up("a", "KeyA", 0, false, None),
            vec![('A' as u32, false)]
        );
        assert!(!kb.any_pressed());
    }

    const CTRL: Option<Modifiers> = Some(Modifiers(Modifiers::CTRL));
    const NONE: Option<Modifiers> = Some(Modifiers(0));

    #[test]
    fn stray_modifier_release_is_undone_by_the_next_key() {
        // The keys of a recorded iPad session: Ctrl went up while held.
        let mut kb = Keyboard::default();
        assert_eq!(
            kb.key_down("Control", "ControlLeft", 1, false, false, CTRL),
            Some(vec![(keysym::CONTROL_L, true)])
        );
        assert_eq!(
            kb.key_down("d", "KeyD", 0, false, false, CTRL),
            Some(vec![('d' as u32, true)])
        );
        assert_eq!(
            kb.key_up("d", "KeyD", 0, false, CTRL),
            vec![('d' as u32, false)]
        );
        assert_eq!(
            kb.key_up("Control", "ControlLeft", 1, false, NONE),
            vec![(keysym::CONTROL_L, false)]
        );
        // The next key still reports Ctrl, so Ctrl goes down again first.
        assert_eq!(
            kb.key_down("d", "KeyD", 0, false, false, CTRL),
            Some(vec![(keysym::CONTROL_L, true), ('d' as u32, true)])
        );
        assert_eq!(
            kb.key_up("d", "KeyD", 0, false, CTRL),
            vec![('d' as u32, false)]
        );
        // The real release lets it go.
        assert_eq!(
            kb.key_up("Control", "ControlLeft", 1, false, NONE),
            vec![(keysym::CONTROL_L, false)]
        );
        assert!(!kb.any_pressed());
    }

    #[test]
    fn lost_modifier_release_is_made_up_by_the_next_key() {
        let mut kb = Keyboard::default();
        kb.key_down("Control", "ControlRight", 2, false, false, CTRL);
        kb.key_down(
            "Shift",
            "ShiftLeft",
            1,
            false,
            false,
            Some(Modifiers(Modifiers::CTRL | Modifiers::SHIFT)),
        );
        // Neither `keyup` came (the page lost focus meanwhile, say).
        assert_eq!(
            kb.key_down("a", "KeyA", 0, false, false, NONE),
            Some(vec![
                (keysym::CONTROL_R, false),
                (keysym::SHIFT_L, false),
                ('a' as u32, true)
            ])
        );
        assert_eq!(
            kb.key_up("a", "KeyA", 0, false, NONE),
            vec![('a' as u32, false)]
        );
        assert!(!kb.any_pressed());
    }

    #[test]
    fn a_modifier_key_is_not_synced_against_its_own_flag() {
        let mut kb = Keyboard::default();
        // `keydown` of Shift reports Shift, its `keyup` does not.
        let shift = Some(Modifiers(Modifiers::SHIFT));
        assert_eq!(
            kb.key_down("Shift", "ShiftLeft", 1, false, false, shift),
            Some(vec![(keysym::SHIFT_L, true)])
        );
        assert_eq!(
            kb.key_up("Shift", "ShiftLeft", 1, false, NONE),
            vec![(keysym::SHIFT_L, false)]
        );
        // A Meta press reported only by the flags is pressed once.
        let meta = Some(Modifiers(Modifiers::META));
        assert_eq!(
            kb.key_down("c", "KeyC", 0, false, false, meta),
            Some(vec![(keysym::SUPER_L, true), ('c' as u32, true)])
        );
        assert_eq!(
            kb.key_up("c", "KeyC", 0, false, meta),
            vec![('c' as u32, false)]
        );
        assert_eq!(
            kb.key_up("Meta", "MetaLeft", 1, false, NONE),
            vec![(keysym::SUPER_L, false)]
        );
        assert!(!kb.any_pressed());
    }

    #[test]
    fn alt_graph_leaves_ctrl_and_alt_alone() {
        // Windows reports AltGr as Ctrl and Alt down.
        let mut kb = Keyboard::default();
        let alt_gr = Some(Modifiers(
            Modifiers::CTRL | Modifiers::ALT | Modifiers::ALT_GRAPH,
        ));
        kb.key_down("Control", "ControlLeft", 1, false, false, alt_gr);
        assert_eq!(
            kb.key_down("AltGraph", "AltRight", 2, false, false, alt_gr),
            Some(vec![(0xFE03, true)])
        );
        assert_eq!(
            kb.key_down("@", "KeyQ", 0, false, false, alt_gr),
            Some(vec![('@' as u32, true)])
        );
    }

    #[test]
    fn caps_lock_toggle_syncs_modifiers_too() {
        let mut kb = remote(true);
        kb.key_down("Control", "ControlLeft", 1, false, false, CTRL);
        assert_eq!(
            kb.key_down("CapsLock", "CapsLock", 0, false, false, NONE),
            Some(vec![
                (keysym::CONTROL_L, false),
                (keysym::HANGUL, true),
                (keysym::HANGUL, false)
            ])
        );
    }

    #[test]
    fn held_modifier_is_released_after_one_key() {
        let mut held = HeldModifiers::default();
        assert_eq!(
            held.set("ctrl", Hold::Once),
            Some(vec![(keysym::CONTROL_L, true)])
        );
        assert!(held.is_held(keysym::CONTROL_L));
        assert_eq!(held.after_key(), vec![(keysym::CONTROL_L, false)]);
        assert_eq!(held.ctrl(), Hold::Off);
        // Nothing is held any more, so the next key releases nothing.
        assert_eq!(held.after_key(), Vec::new());
    }

    #[test]
    fn locked_modifier_stays_until_turned_off() {
        let mut held = HeldModifiers::default();
        assert_eq!(
            held.set("alt", Hold::Once),
            Some(vec![(keysym::ALT_L, true)])
        );
        // Locking a modifier that is already down sends nothing.
        assert_eq!(held.set("alt", Hold::Locked), Some(Vec::new()));
        assert_eq!(held.after_key(), Vec::new());
        assert_eq!(held.alt(), Hold::Locked);
        assert_eq!(
            held.set("alt", Hold::Off),
            Some(vec![(keysym::ALT_L, false)])
        );
        assert_eq!(held.set("shift", Hold::Once), None);
    }

    #[test]
    fn held_modifiers_are_released_together() {
        let mut held = HeldModifiers::default();
        held.set("ctrl", Hold::Locked);
        held.set("alt", Hold::Once);
        assert_eq!(
            held.release_all(),
            vec![(keysym::CONTROL_L, false), (keysym::ALT_L, false)]
        );
        assert_eq!((held.ctrl(), held.alt()), (Hold::Off, Hold::Off));
        assert_eq!(Hold::parse("locked"), Some(Hold::Locked));
        assert_eq!(Hold::parse("held"), None);
        assert_eq!(Hold::Once.as_str(), "once");
    }

    #[test]
    fn tells_held_keys() {
        let mut kb = Keyboard::default();
        kb.key_down("Control", "ControlRight", 2, false, false, None);
        assert!(kb.is_down(&[keysym::CONTROL_L, keysym::CONTROL_R]));
        assert!(!kb.is_down(&[keysym::SHIFT_L, keysym::SHIFT_R]));
        kb.key_up("Control", "ControlRight", 2, false, None);
        assert!(!kb.is_down(&[keysym::CONTROL_L, keysym::CONTROL_R]));
    }

    #[test]
    fn composition_is_left_to_the_browser() {
        let mut kb = Keyboard::default();
        assert_eq!(kb.key_down("Process", "KeyG", 0, true, false, None), None);
        assert_eq!(kb.key_down("ㅎ", "KeyG", 0, true, false, None), None);
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
            kb.key_down("Process", "KeyG", 0, true, false, None),
            Some(vec![('g' as u32, true)])
        );
        assert_eq!(
            kb.key_up("Process", "KeyG", 0, false, None),
            vec![('g' as u32, false)]
        );
        assert_eq!(
            kb.key_down("ㅏ", "KeyK", 0, false, false, None),
            Some(vec![('k' as u32, true)])
        );
        assert_eq!(
            kb.key_up("ㅏ", "KeyK", 0, false, None),
            vec![('k' as u32, false)]
        );
        // Shift selects the double consonants on the remote side.
        kb.key_down("Shift", "ShiftLeft", 1, false, false, None);
        assert_eq!(
            kb.key_down("ㅃ", "KeyQ", 0, false, false, None),
            Some(vec![('Q' as u32, true)])
        );
        kb.key_up("ㅃ", "KeyQ", 0, false, None);
        kb.key_up("Shift", "ShiftLeft", 1, false, None);
        // Editing keys claimed by the IME are forwarded too.
        assert_eq!(
            kb.key_down("Process", "Backspace", 0, true, false, None),
            Some(vec![(0xFF08, true)])
        );
        // Latin text and named keys are unchanged.
        assert_eq!(
            kb.key_down("é", "KeyE", 0, false, false, None),
            Some(vec![(0xE9, true)])
        );
        assert_eq!(
            kb.key_down("HangulMode", "Lang1", 0, false, false, None),
            Some(vec![(keysym::HANGUL, true)])
        );
        // Dead keys still compose locally.
        assert_eq!(kb.key_down("Dead", "Quote", 0, false, false, None), None);
    }

    #[test]
    fn caps_lock_toggles_the_remote_input_mode() {
        let toggle = tap(keysym::HANGUL);
        let caps = tap(keysym::CAPS_LOCK);

        // Press and release both reported: one toggle.
        let mut kb = remote(true);
        assert_eq!(
            kb.key_down("CapsLock", "CapsLock", 0, false, false, None),
            Some(toggle.clone())
        );
        assert_eq!(
            kb.key_down("CapsLock", "CapsLock", 0, false, false, None),
            Some(vec![])
        );
        assert_eq!(kb.key_up("CapsLock", "CapsLock", 0, false, None), vec![]);
        // Only the release reported.
        assert_eq!(kb.key_up("CapsLock", "CapsLock", 0, false, None), toggle);
        // A press that locks capitals is Caps Lock, and so is its unlock.
        assert_eq!(
            kb.key_down("CapsLock", "CapsLock", 0, false, true, None),
            Some(caps.clone())
        );
        assert_eq!(kb.key_up("CapsLock", "CapsLock", 0, false, None), caps);
        assert!(!kb.any_pressed());

        // Capitals already locked when the session started.
        let mut kb = remote(true);
        kb.key_down("A", "KeyA", 0, false, true, None);
        kb.key_up("A", "KeyA", 0, true, None);
        assert_eq!(kb.key_up("CapsLock", "CapsLock", 0, false, None), caps);

        // Without the option, and when composing locally, it stays Caps Lock.
        for mut kb in [remote(false), Keyboard::default()] {
            assert_eq!(
                kb.key_down("CapsLock", "CapsLock", 0, false, true, None),
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

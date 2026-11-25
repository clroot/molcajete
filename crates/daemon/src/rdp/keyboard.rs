//! Translation of X11 keysyms (as sent by Guacamole clients) into RDP
//! keyboard events.
//!
//! RDP keyboards send scancodes, interpreted by the server's keyboard layout.
//! Keys that do not produce text (modifiers, function and navigation keys)
//! always become scancodes. Printable ASCII characters become scancodes of the
//! US layout when the current Shift state produces that very character, or
//! when a shortcut modifier (Ctrl, Alt, Super) is held; otherwise (for
//! example `@` typed with AltGr on a non-US client keyboard, or any non-ASCII
//! character such as Hangul) a Unicode keyboard event is sent instead.
//!
//! Every press is remembered so that its release matches it, even if the
//! modifiers changed in between.

use std::collections::{HashMap, HashSet};

use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};

use super::settings::KeyboardLayout;

/// A Set 1 scancode, possibly with the 0xE0 prefix ("extended").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Scancode {
    pub code: u8,
    pub extended: bool,
}

const fn key(code: u8) -> Scancode {
    Scancode {
        code,
        extended: false,
    }
}

const fn ext(code: u8) -> Scancode {
    Scancode {
        code,
        extended: true,
    }
}

const SHIFT_L: u32 = 0xFFE1;
const SHIFT_R: u32 = 0xFFE2;
const CONTROL_L: u32 = 0xFFE3;
const CONTROL_R: u32 = 0xFFE4;
const META_L: u32 = 0xFFE7;
const META_R: u32 = 0xFFE8;
const ALT_L: u32 = 0xFFE9;
const ALT_R: u32 = 0xFFEA;
const SUPER_L: u32 = 0xFFEB;
const SUPER_R: u32 = 0xFFEC;

const LEFT_SHIFT_SCANCODE: Scancode = key(0x2A);

/// Scancodes of keys that do not produce text.
fn special_scancode(keysym: u32) -> Option<Scancode> {
    Some(match keysym {
        0xFF08 => key(0x0E),          // BackSpace
        0xFF09 | 0xFE20 => key(0x0F), // Tab, ISO_Left_Tab
        0xFF0D => key(0x1C),          // Return
        0xFF14 => key(0x46),          // Scroll_Lock
        // The Korean keys, which reach the remote IME as VK_HANGUL and
        // VK_HANJA when the session uses a Korean keyboard layout.
        0xFF31 => key(0x72),                                    // Hangul
        0xFF34 => key(0x71),                                    // Hangul_Hanja
        0xFF15 => key(0x54),                                    // Sys_Req
        0xFF1B => key(0x01),                                    // Escape
        0xFF50 | 0xFF95 => ext(0x47),                           // Home, KP_Home
        0xFF51 | 0xFF96 => ext(0x4B),                           // Left, KP_Left
        0xFF52 | 0xFF97 => ext(0x48),                           // Up, KP_Up
        0xFF53 | 0xFF98 => ext(0x4D),                           // Right, KP_Right
        0xFF54 | 0xFF99 => ext(0x50),                           // Down, KP_Down
        0xFF55 | 0xFF9A => ext(0x49),                           // Page_Up, KP_Page_Up
        0xFF56 | 0xFF9B => ext(0x51),                           // Page_Down, KP_Page_Down
        0xFF57 | 0xFF9C => ext(0x4F),                           // End, KP_End
        0xFF9D => key(0x4C),                                    // KP_Begin
        0xFF61 => ext(0x37),                                    // Print
        0xFF63 | 0xFF9E => ext(0x52),                           // Insert, KP_Insert
        0xFFFF | 0xFF9F => ext(0x53),                           // Delete, KP_Delete
        0xFF67 => ext(0x5D),                                    // Menu
        0xFF7F => key(0x45),                                    // Num_Lock
        0xFF8D => ext(0x1C),                                    // KP_Enter
        0xFFAA => key(0x37),                                    // KP_Multiply
        0xFFAB => key(0x4E),                                    // KP_Add
        0xFFAD => key(0x4A),                                    // KP_Subtract
        0xFFAF => ext(0x35),                                    // KP_Divide
        0xFFBE..=0xFFC7 => key(0x3B + (keysym - 0xFFBE) as u8), // F1..F10
        0xFFC8 => key(0x57),                                    // F11
        0xFFC9 => key(0x58),                                    // F12
        0xFFCA..=0xFFD4 => key(0x64 + (keysym - 0xFFCA) as u8), // F13..F23
        0xFFD5 => key(0x76),                                    // F24
        SHIFT_L => LEFT_SHIFT_SCANCODE,
        SHIFT_R => key(0x36),
        CONTROL_L => key(0x1D),
        CONTROL_R => ext(0x1D),
        0xFFE5 => key(0x3A), // Caps_Lock
        META_L | SUPER_L => ext(0x5B),
        META_R | SUPER_R => ext(0x5C),
        ALT_L => key(0x38),
        ALT_R => ext(0x38),
        _ => return None,
    })
}

/// The character typed by a keysym, if it is printable.
fn keysym_char(keysym: u32) -> Option<char> {
    let code = match keysym {
        0x20..=0x7E | 0xA0..=0xFF => keysym,
        // Keypad keys that produce text (with Num Lock on).
        0xFFB0..=0xFFB9 => '0' as u32 + (keysym - 0xFFB0),
        0xFFAE => '.' as u32,
        0xFFAC => ',' as u32,
        0xFFBD => '=' as u32,
        0xFF80 => ' ' as u32,
        // Unicode keysyms.
        0x0100_0100..=0x0110_FFFF => keysym - 0x0100_0000,
        _ => return None,
    };
    char::from_u32(code).filter(|c| !c.is_control())
}

/// Position of a printable ASCII character on a US keyboard and whether it
/// needs Shift (`None` when Shift does not matter, i.e. for space).
fn us_scancode(ch: char) -> Option<(Scancode, Option<bool>)> {
    const ROW_DIGITS: &[u8; 10] = b"1234567890";
    const ROW_DIGITS_SHIFTED: &[u8; 10] = b"!@#$%^&*()";
    const ROW_Q: &[u8; 10] = b"qwertyuiop";
    const ROW_A: &[u8; 9] = b"asdfghjkl";
    const ROW_Z: &[u8; 7] = b"zxcvbnm";
    // Punctuation: (unshifted, shifted, scancode).
    const PUNCTUATION: &[(u8, u8, u8)] = &[
        (b'-', b'_', 0x0C),
        (b'=', b'+', 0x0D),
        (b'[', b'{', 0x1A),
        (b']', b'}', 0x1B),
        (b';', b':', 0x27),
        (b'\'', b'"', 0x28),
        (b'`', b'~', 0x29),
        (b'\\', b'|', 0x2B),
        (b',', b'<', 0x33),
        (b'.', b'>', 0x34),
        (b'/', b'?', 0x35),
    ];

    if !ch.is_ascii() {
        return None;
    }
    let byte = ch as u8;
    if byte == b' ' {
        return Some((key(0x39), None));
    }
    let find = |row: &[u8], first: u8, byte: u8| {
        row.iter()
            .position(|&c| c == byte)
            .map(|i| key(first + i as u8))
    };
    let lower = byte.to_ascii_lowercase();
    let shifted = byte.is_ascii_uppercase();
    let letter = find(ROW_Q, 0x10, lower)
        .or_else(|| find(ROW_A, 0x1E, lower))
        .or_else(|| find(ROW_Z, 0x2C, lower));
    if let Some(sc) = letter {
        return Some((sc, Some(shifted)));
    }
    if let Some(sc) = find(ROW_DIGITS, 0x02, byte) {
        return Some((sc, Some(false)));
    }
    if let Some(sc) = find(ROW_DIGITS_SHIFTED, 0x02, byte) {
        return Some((sc, Some(true)));
    }
    PUNCTUATION.iter().find_map(|&(plain, shift, code)| {
        if byte == plain {
            Some((key(code), Some(false)))
        } else if byte == shift {
            Some((key(code), Some(true)))
        } else {
            None
        }
    })
}

/// What was sent for a pressed keysym, so that its release matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    Scancode(Scancode),
    Unicode(char),
}

fn scancode_event(sc: Scancode, pressed: bool) -> FastPathInputEvent {
    let mut flags = KeyboardFlags::empty();
    if sc.extended {
        flags |= KeyboardFlags::EXTENDED;
    }
    if !pressed {
        flags |= KeyboardFlags::RELEASE;
    }
    FastPathInputEvent::KeyboardEvent(flags, sc.code)
}

fn unicode_events(ch: char, pressed: bool, out: &mut Vec<FastPathInputEvent>) {
    let flags = if pressed {
        KeyboardFlags::empty()
    } else {
        KeyboardFlags::RELEASE
    };
    let mut units = [0u16; 2];
    for &unit in ch.encode_utf16(&mut units).iter() {
        out.push(FastPathInputEvent::UnicodeKeyboardEvent(flags, unit));
    }
}

fn sent_events(sent: Sent, pressed: bool, out: &mut Vec<FastPathInputEvent>) {
    match sent {
        Sent::Scancode(sc) => out.push(scancode_event(sc, pressed)),
        Sent::Unicode(ch) => unicode_events(ch, pressed, out),
    }
}

/// Keyboard state of one session.
#[derive(Debug)]
pub struct Keyboard {
    layout: KeyboardLayout,
    /// Whether the server accepts Unicode keyboard events.
    unicode: bool,
    pressed: HashMap<u32, Sent>,
    /// Modifier keysyms currently held.
    modifiers: HashSet<u32>,
}

impl Keyboard {
    pub fn new(layout: KeyboardLayout, unicode: bool) -> Self {
        Self {
            layout,
            unicode,
            pressed: HashMap::new(),
            modifiers: HashSet::new(),
        }
    }

    /// Updates whether the server accepts Unicode events (it may change
    /// after a reactivation).
    pub fn set_unicode_support(&mut self, unicode: bool) {
        self.unicode = unicode;
    }

    fn held(&self, keysyms: &[u32]) -> bool {
        keysyms.iter().any(|k| self.modifiers.contains(k))
    }

    fn shift_held(&self) -> bool {
        self.held(&[SHIFT_L, SHIFT_R])
    }

    /// Ctrl, Alt or Super, but not Ctrl+Alt (which Windows clients send for
    /// AltGr, so characters typed with it must keep their identity).
    fn shortcut_held(&self) -> bool {
        let ctrl = self.held(&[CONTROL_L, CONTROL_R]);
        let alt = self.held(&[ALT_L, ALT_R]);
        let super_ = self.held(&[SUPER_L, SUPER_R, META_L, META_R]);
        (ctrl || alt || super_) && !(ctrl && alt)
    }

    /// Handles a key press or release and returns the events to send.
    pub fn key(&mut self, keysym: u32, pressed: bool) -> Vec<FastPathInputEvent> {
        let mut out = Vec::new();
        if pressed {
            self.press(keysym, &mut out);
        } else if let Some(sent) = self.pressed.remove(&keysym) {
            self.modifiers.remove(&keysym);
            sent_events(sent, false, &mut out);
        }
        out
    }

    fn press(&mut self, keysym: u32, out: &mut Vec<FastPathInputEvent>) {
        let Some((sent, fake_shift)) = self.translate(keysym) else {
            return;
        };
        // A repeated press (auto-repeat) is sent again; if the mapping
        // changed meanwhile, the earlier press is released first.
        if let Some(previous) = self.pressed.insert(keysym, sent)
            && previous != sent
        {
            sent_events(previous, false, out);
        }
        if is_modifier(keysym) {
            self.modifiers.insert(keysym);
        }
        match fake_shift {
            None => sent_events(sent, true, out),
            Some(wanted) => {
                // Temporarily give the server the Shift state the character
                // needs, then restore the state of the client's Shift keys.
                let held = self.held_shift_scancodes();
                let toggles: Vec<(Scancode, bool)> = if wanted {
                    vec![(LEFT_SHIFT_SCANCODE, true)]
                } else {
                    held.into_iter().map(|sc| (sc, false)).collect()
                };
                for &(sc, pressed) in &toggles {
                    out.push(scancode_event(sc, pressed));
                }
                sent_events(sent, true, out);
                for &(sc, pressed) in toggles.iter().rev() {
                    out.push(scancode_event(sc, !pressed));
                }
            }
        }
    }

    fn held_shift_scancodes(&self) -> Vec<Scancode> {
        [SHIFT_L, SHIFT_R]
            .iter()
            .filter_map(|k| match self.pressed.get(k) {
                Some(Sent::Scancode(sc)) => Some(*sc),
                _ => None,
            })
            .collect()
    }

    /// Decides how a pressed keysym is sent. The second value asks for a
    /// synthesized Shift state (only when Unicode events are unavailable).
    fn translate(&self, keysym: u32) -> Option<(Sent, Option<bool>)> {
        if let Some(sc) = special_scancode(keysym) {
            return Some((Sent::Scancode(sc), None));
        }
        let ch = keysym_char(keysym)?;
        let us = match self.layout {
            KeyboardLayout::EnUsQwerty | KeyboardLayout::KoKrQwerty => us_scancode(ch),
            KeyboardLayout::Failsafe => None,
        };
        let Some((sc, needs_shift)) = us else {
            return self.unicode.then_some((Sent::Unicode(ch), None));
        };
        let shift_matches = needs_shift.is_none_or(|needed| needed == self.shift_held());
        if shift_matches || self.shortcut_held() {
            Some((Sent::Scancode(sc), None))
        } else if self.unicode {
            Some((Sent::Unicode(ch), None))
        } else {
            Some((Sent::Scancode(sc), needs_shift))
        }
    }
}

fn is_modifier(keysym: u32) -> bool {
    matches!(
        keysym,
        SHIFT_L
            | SHIFT_R
            | CONTROL_L
            | CONTROL_R
            | META_L
            | META_R
            | ALT_L
            | ALT_R
            | SUPER_L
            | SUPER_R
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> Keyboard {
        Keyboard::new(KeyboardLayout::EnUsQwerty, true)
    }

    fn down(code: u8) -> FastPathInputEvent {
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), code)
    }

    fn up(code: u8) -> FastPathInputEvent {
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, code)
    }

    fn ext_down(code: u8) -> FastPathInputEvent {
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::EXTENDED, code)
    }

    fn uni(unit: u16, pressed: bool) -> FastPathInputEvent {
        let flags = if pressed {
            KeyboardFlags::empty()
        } else {
            KeyboardFlags::RELEASE
        };
        FastPathInputEvent::UnicodeKeyboardEvent(flags, unit)
    }

    #[test]
    fn special_keys_map_to_scancodes() {
        assert_eq!(special_scancode(0xFF0D), Some(key(0x1C)));
        assert_eq!(special_scancode(0xFF8D), Some(ext(0x1C)));
        assert_eq!(special_scancode(0xFFBE), Some(key(0x3B))); // F1
        assert_eq!(special_scancode(0xFFC7), Some(key(0x44))); // F10
        assert_eq!(special_scancode(0xFFC9), Some(key(0x58))); // F12
        assert_eq!(special_scancode(0xFF52), Some(ext(0x48))); // Up
        assert_eq!(special_scancode(0xFFFF), Some(ext(0x53))); // Delete
        assert_eq!(special_scancode(SUPER_L), Some(ext(0x5B)));
        assert_eq!(special_scancode(CONTROL_R), Some(ext(0x1D)));
        assert_eq!(special_scancode(0xFF31), Some(key(0x72))); // Hangul
        assert_eq!(special_scancode(0xFF34), Some(key(0x71))); // Hanja
        // AltGr is not forwarded: the characters it produces are.
        assert_eq!(special_scancode(0xFE03), None);
    }

    #[test]
    fn us_layout_positions() {
        assert_eq!(us_scancode('q'), Some((key(0x10), Some(false))));
        assert_eq!(us_scancode('A'), Some((key(0x1E), Some(true))));
        assert_eq!(us_scancode('m'), Some((key(0x32), Some(false))));
        assert_eq!(us_scancode('1'), Some((key(0x02), Some(false))));
        assert_eq!(us_scancode('0'), Some((key(0x0B), Some(false))));
        assert_eq!(us_scancode('!'), Some((key(0x02), Some(true))));
        assert_eq!(us_scancode(')'), Some((key(0x0B), Some(true))));
        assert_eq!(us_scancode('|'), Some((key(0x2B), Some(true))));
        assert_eq!(us_scancode('/'), Some((key(0x35), Some(false))));
        assert_eq!(us_scancode(' '), Some((key(0x39), None)));
        assert_eq!(us_scancode('é'), None);
    }

    #[test]
    fn keysym_characters() {
        assert_eq!(keysym_char(0x61), Some('a'));
        assert_eq!(keysym_char(0xE9), Some('é'));
        assert_eq!(keysym_char(0x0100_D55C), Some('한'));
        assert_eq!(keysym_char(0x0101_F600), Some('😀'));
        assert_eq!(keysym_char(0xFFB7), Some('7'));
        assert_eq!(keysym_char(0xFF0D), None);
        assert_eq!(keysym_char(0x0100_0007), None);
    }

    #[test]
    fn plain_and_shifted_text_uses_scancodes() {
        let mut k = kb();
        assert_eq!(k.key(0x61, true), vec![down(0x1E)]);
        assert_eq!(k.key(0x61, false), vec![up(0x1E)]);
        assert_eq!(k.key(SHIFT_L, true), vec![down(0x2A)]);
        assert_eq!(k.key(0x21, true), vec![down(0x02)]); // '!'
        assert_eq!(k.key(0x21, false), vec![up(0x02)]);
        assert_eq!(k.key(SHIFT_L, false), vec![up(0x2A)]);
        assert_eq!(k.key(0x20, true), vec![down(0x39)]);
    }

    #[test]
    fn mismatched_shift_state_falls_back_to_unicode() {
        let mut k = kb();
        // '@' without Shift (e.g. AltGr+Q on a German keyboard).
        k.key(0xFE03, true);
        assert_eq!(k.key(0x40, true), vec![uni('@' as u16, true)]);
        assert_eq!(k.key(0x40, false), vec![uni('@' as u16, false)]);
        // Caps Lock on: 'A' without Shift.
        assert_eq!(k.key(0x41, true), vec![uni('A' as u16, true)]);
    }

    #[test]
    fn shortcuts_use_scancodes() {
        let mut k = kb();
        assert_eq!(k.key(CONTROL_L, true), vec![down(0x1D)]);
        // Ctrl+C even when the client reports an upper-case 'C'.
        assert_eq!(k.key(0x43, true), vec![down(0x2E)]);
        assert_eq!(k.key(0x43, false), vec![up(0x2E)]);
        assert_eq!(k.key(CONTROL_L, false), vec![up(0x1D)]);
        // Ctrl+Alt behaves like AltGr: characters keep their identity.
        k.key(CONTROL_L, true);
        k.key(ALT_R, true);
        assert_eq!(k.key(0x40, true), vec![uni('@' as u16, true)]);
    }

    #[test]
    fn non_ascii_text_uses_unicode_events() {
        let mut k = kb();
        assert_eq!(k.key(0x0100_D55C, true), vec![uni(0xD55C, true)]);
        assert_eq!(k.key(0x0100_D55C, false), vec![uni(0xD55C, false)]);
        // Supplementary characters become surrogate pairs.
        assert_eq!(
            k.key(0x0101_F600, true),
            vec![uni(0xD83D, true), uni(0xDE00, true)]
        );
    }

    #[test]
    fn releases_match_presses() {
        let mut k = kb();
        k.key(SHIFT_L, true);
        assert_eq!(k.key(0x41, true), vec![down(0x1E)]); // 'A' as scancode
        k.key(SHIFT_L, false);
        // Released after Shift: still the scancode that was pressed.
        assert_eq!(k.key(0x41, false), vec![up(0x1E)]);
        // Releases of keys that were never pressed are dropped.
        assert!(k.key(0x42, false).is_empty());
        // Navigation keys carry the extended flag in both directions.
        assert_eq!(k.key(0xFF51, true), vec![ext_down(0x4B)]);
        assert_eq!(
            k.key(0xFF51, false),
            vec![FastPathInputEvent::KeyboardEvent(
                KeyboardFlags::EXTENDED | KeyboardFlags::RELEASE,
                0x4B
            )]
        );
    }

    #[test]
    fn auto_repeat_resends_the_press() {
        let mut k = kb();
        assert_eq!(k.key(0x61, true), vec![down(0x1E)]);
        assert_eq!(k.key(0x61, true), vec![down(0x1E)]);
        assert_eq!(k.key(0x61, false), vec![up(0x1E)]);
    }

    #[test]
    fn without_unicode_support_shift_is_synthesized() {
        let mut k = Keyboard::new(KeyboardLayout::EnUsQwerty, false);
        assert_eq!(k.key(0x41, true), vec![down(0x2A), down(0x1E), up(0x2A)]);
        assert_eq!(k.key(0x41, false), vec![up(0x1E)]);
        k.key(SHIFT_L, true);
        assert_eq!(k.key(0x61, true), vec![up(0x2A), down(0x1E), down(0x2A)]);
        // Characters outside the layout cannot be typed at all.
        assert!(k.key(0x0100_D55C, true).is_empty());
    }

    #[test]
    fn korean_layout_types_with_scancodes_for_the_remote_ime() {
        let mut k = Keyboard::new(KeyboardLayout::KoKrQwerty, true);
        // The Hangul key, then "gks": the remote IME composes 한.
        assert_eq!(k.key(0xFF31, true), vec![down(0x72)]);
        assert_eq!(k.key(0xFF31, false), vec![up(0x72)]);
        assert_eq!(k.key(0x67, true), vec![down(0x22)]);
        assert_eq!(k.key(0x6B, true), vec![down(0x25)]);
        assert_eq!(k.key(0x73, true), vec![down(0x1F)]);
        // Text composed on the client still arrives as Unicode.
        assert_eq!(k.key(0x0100_D55C, true), vec![uni(0xD55C, true)]);
    }

    #[test]
    fn failsafe_layout_sends_all_text_as_unicode() {
        let mut k = Keyboard::new(KeyboardLayout::Failsafe, true);
        assert_eq!(k.key(0x61, true), vec![uni('a' as u16, true)]);
        assert_eq!(k.key(0xFF0D, true), vec![down(0x1C)]);
    }
}

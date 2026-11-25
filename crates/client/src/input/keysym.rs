//! Mapping from DOM `KeyboardEvent` properties to X11 keysyms.
//!
//! The tables are derived from `Guacamole.Keyboard` in Apache Guacamole's
//! guacamole-common-js (Copyright The Apache Software Foundation, Apache
//! License 2.0), translated to Rust and modified; see NOTICE. Modern browsers
//! all provide `key`, `code` and `location`, so the legacy `keyCode` /
//! `keyIdentifier` heuristics of the reference client are not needed.

/// DOM key location constants.
pub const LOCATION_STANDARD: u32 = 0;
pub const LOCATION_LEFT: u32 = 1;
pub const LOCATION_RIGHT: u32 = 2;
pub const LOCATION_NUMPAD: u32 = 3;

pub const SHIFT_L: u32 = 0xFFE1;
pub const SHIFT_R: u32 = 0xFFE2;
pub const CONTROL_L: u32 = 0xFFE3;
pub const CONTROL_R: u32 = 0xFFE4;
pub const ALT_L: u32 = 0xFFE9;
pub const ALT_R: u32 = 0xFFEA;
pub const SUPER_L: u32 = 0xFFEB;
pub const SUPER_R: u32 = 0xFFEC;
pub const DELETE: u32 = 0xFFFF;
pub const CAPS_LOCK: u32 = 0xFFE5;
/// Hangul/English toggle (`XK_Hangul`).
pub const HANGUL: u32 = 0xFF31;

/// Named keys (`KeyboardEvent.key`) → keysyms indexed by location
/// (standard, left, right). A single entry applies to every location.
fn named_key(key: &str) -> Option<&'static [u32]> {
    Some(match key {
        "Again" => &[0xFF66],
        "AllCandidates" => &[0xFF3D],
        "Alphanumeric" => &[0xFF30],
        "Alt" => &[ALT_L, ALT_L, ALT_R],
        "Attn" => &[0xFD0E],
        "AltGraph" => &[0xFE03],
        "ArrowDown" | "Down" | "UIKeyInputDownArrow" => &[0xFF54],
        "ArrowLeft" | "Left" | "UIKeyInputLeftArrow" => &[0xFF51],
        "ArrowRight" | "Right" | "UIKeyInputRightArrow" => &[0xFF53],
        "ArrowUp" | "Up" | "UIKeyInputUpArrow" => &[0xFF52],
        "Backspace" => &[0xFF08],
        "CapsLock" => &[0xFFE5],
        "Cancel" => &[0xFF69],
        "Clear" => &[0xFF0B],
        "Convert" => &[0xFF23],
        "Copy" => &[0xFD15],
        "Crsel" | "CrSel" => &[0xFD1C],
        "CodeInput" => &[0xFF37],
        "Compose" => &[0xFF20],
        "Control" => &[CONTROL_L, CONTROL_L, CONTROL_R],
        "ContextMenu" => &[0xFF67],
        "Delete" => &[DELETE],
        "End" => &[0xFF57],
        "Enter" => &[0xFF0D],
        "EraseEof" => &[0xFD06],
        "Escape" | "UIKeyInputEscape" => &[0xFF1B],
        "Execute" => &[0xFF62],
        "Exsel" | "ExSel" => &[0xFD1D],
        "F1" => &[0xFFBE],
        "F2" => &[0xFFBF],
        "F3" => &[0xFFC0],
        "F4" => &[0xFFC1],
        "F5" => &[0xFFC2],
        "F6" => &[0xFFC3],
        "F7" => &[0xFFC4],
        "F8" => &[0xFFC5],
        "F9" => &[0xFFC6],
        "F10" => &[0xFFC7],
        "F11" => &[0xFFC8],
        "F12" => &[0xFFC9],
        "F13" => &[0xFFCA],
        "F14" => &[0xFFCB],
        "F15" => &[0xFFCC],
        "F16" => &[0xFFCD],
        "F17" => &[0xFFCE],
        "F18" => &[0xFFCF],
        "F19" => &[0xFFD0],
        "F20" => &[0xFFD1],
        "F21" => &[0xFFD2],
        "F22" => &[0xFFD3],
        "F23" => &[0xFFD4],
        "F24" => &[0xFFD5],
        "Find" => &[0xFF68],
        "GroupFirst" => &[0xFE0C],
        "GroupLast" => &[0xFE0E],
        "GroupNext" => &[0xFE08],
        "GroupPrevious" => &[0xFE0A],
        "HangulMode" => &[0xFF31],
        "Hankaku" => &[0xFF29],
        "HanjaMode" => &[0xFF34],
        "Help" => &[0xFF6A],
        "Hiragana" | "JapaneseHiragana" => &[0xFF25],
        "HiraganaKatakana" => &[0xFF27],
        "Home" => &[0xFF50],
        "Insert" => &[0xFF63],
        "JapaneseKatakana" | "Katakana" => &[0xFF26],
        "JapaneseRomaji" | "Romaji" => &[0xFF24],
        "JunjaMode" => &[0xFF38],
        "KanaMode" => &[0xFF2D],
        "KanjiMode" => &[0xFF21],
        "Meta" | "OS" | "Super" | "Win" => &[SUPER_L, SUPER_L, SUPER_R],
        "ModeChange" => &[0xFF7E],
        "NonConvert" => &[0xFF22],
        "NumLock" => &[0xFF7F],
        "PageDown" => &[0xFF56],
        "PageUp" => &[0xFF55],
        "Pause" => &[0xFF13],
        "Play" => &[0xFD16],
        "PreviousCandidate" => &[0xFF3E],
        "PrintScreen" => &[0xFF61],
        "Redo" => &[0xFF66],
        "Scroll" | "ScrollLock" => &[0xFF14],
        "Select" => &[0xFF60],
        "Separator" => &[0xFFAC],
        "Shift" => &[SHIFT_L, SHIFT_L, SHIFT_R],
        "SingleCandidate" => &[0xFF3C],
        "Tab" => &[0xFF09],
        "Undo" => &[0xFF65],
        "Zenkaku" => &[0xFF28],
        "ZenkakuHankaku" => &[0xFF2A],
        _ => return None,
    })
}

/// Numeric keypad keys by `KeyboardEvent.code` (used when the key produces a
/// character, i.e. with NumLock on).
fn numpad_code(code: &str) -> Option<u32> {
    Some(match code {
        "Numpad0" => 0xFFB0,
        "Numpad1" => 0xFFB1,
        "Numpad2" => 0xFFB2,
        "Numpad3" => 0xFFB3,
        "Numpad4" => 0xFFB4,
        "Numpad5" => 0xFFB5,
        "Numpad6" => 0xFFB6,
        "Numpad7" => 0xFFB7,
        "Numpad8" => 0xFFB8,
        "Numpad9" => 0xFFB9,
        "NumpadMultiply" => 0xFFAA,
        "NumpadAdd" => 0xFFAB,
        "NumpadComma" => 0xFFAC,
        "NumpadSubtract" => 0xFFAD,
        "NumpadDecimal" => 0xFFAE,
        "NumpadDivide" => 0xFFAF,
        "NumpadEqual" => 0xFFBD,
        "NumpadEnter" => 0xFF8D,
        _ => return None,
    })
}

/// Fallback by physical key when `key` is unusable ("Unidentified").
fn code_fallback(code: &str) -> Option<u32> {
    if let Some(letter) = code.strip_prefix("Key") {
        let c = letter.chars().next()?;
        if letter.len() == 1 && c.is_ascii_uppercase() {
            return Some(c.to_ascii_lowercase() as u32);
        }
    }
    if let Some(digit) = code.strip_prefix("Digit") {
        let c = digit.chars().next()?;
        if digit.len() == 1 && c.is_ascii_digit() {
            return Some(c as u32);
        }
    }
    Some(match code {
        "Space" => 0x20,
        "Minus" => '-' as u32,
        "Equal" => '=' as u32,
        "BracketLeft" => '[' as u32,
        "BracketRight" => ']' as u32,
        "Backslash" => '\\' as u32,
        "Semicolon" => ';' as u32,
        "Quote" => '\'' as u32,
        "Backquote" => '`' as u32,
        "Comma" => ',' as u32,
        "Period" => '.' as u32,
        "Slash" => '/' as u32,
        "Lang1" => 0xFF31, // Hangul
        "Lang2" => 0xFF34, // Hanja
        _ => return numpad_code(code),
    })
}

/// The character a physical key types on a US keyboard, by
/// `KeyboardEvent.code`, with or without Shift.
pub fn us_layout_char(code: &str, shift: bool) -> Option<char> {
    const SHIFTED: &[(char, char)] = &[
        ('1', '!'),
        ('2', '@'),
        ('3', '#'),
        ('4', '$'),
        ('5', '%'),
        ('6', '^'),
        ('7', '&'),
        ('8', '*'),
        ('9', '('),
        ('0', ')'),
        ('-', '_'),
        ('=', '+'),
        ('[', '{'),
        (']', '}'),
        ('\\', '|'),
        (';', ':'),
        ('\'', '"'),
        ('`', '~'),
        (',', '<'),
        ('.', '>'),
        ('/', '?'),
    ];
    let plain = char::from_u32(code_fallback(code)?).filter(|c| c.is_ascii() && !c.is_control())?;
    if !shift {
        return Some(plain);
    }
    if plain.is_ascii_lowercase() {
        return Some(plain.to_ascii_uppercase());
    }
    Some(
        SHIFTED
            .iter()
            .find(|(from, _)| *from == plain)
            .map_or(plain, |(_, to)| *to),
    )
}

/// Keys that an active IME reports as `Process` but that are still worth
/// forwarding by their physical position.
pub fn editing_key_by_code(code: &str) -> Option<u32> {
    Some(match code {
        "Backspace" => 0xFF08,
        "Tab" => 0xFF09,
        "Enter" => 0xFF0D,
        "Escape" => 0xFF1B,
        "NumpadEnter" => 0xFF8D,
        _ => return None,
    })
}

/// Whether `key` is a Hangul jamo or syllable, which is what browsers report
/// for a letter key while a Korean input source is active but not composing.
pub fn is_hangul_key(key: &str) -> bool {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => matches!(
            c as u32,
            0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F | 0xAC00..=0xD7AF | 0xD7B0..=0xD7FF
        ),
        _ => false,
    }
}

fn is_control_character(cp: u32) -> bool {
    cp <= 0x1F || (0x7F..=0x9F).contains(&cp)
}

/// Keysym for a Unicode codepoint.
pub fn keysym_from_codepoint(cp: u32) -> Option<u32> {
    if is_control_character(cp) {
        return Some(0xFF00 | cp);
    }
    match cp {
        0x0000..=0x00FF => Some(cp),
        0x0100..=0x10FFFF => Some(0x0100_0000 | cp),
        _ => None,
    }
}

/// Resolves the keysym for a key event.
pub fn keysym_for_event(key: &str, code: &str, location: u32) -> Option<u32> {
    // Keypad keys that produce characters map to their KP_* keysyms.
    if location == LOCATION_NUMPAD
        && key.chars().count() == 1
        && let Some(ks) = numpad_code(code)
    {
        return Some(ks);
    }
    if key == "Enter" && code == "NumpadEnter" {
        return Some(0xFF8D);
    }

    if let Some(keysyms) = named_key(key) {
        let index = match location {
            LOCATION_LEFT => 1,
            LOCATION_RIGHT => 2,
            _ => 0,
        };
        return Some(*keysyms.get(index).unwrap_or(&keysyms[0]));
    }

    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return keysym_from_codepoint(c as u32);
    }

    // "Unidentified", "Dead", "Process" and friends.
    match key {
        "Dead" | "Process" => None,
        _ => code_fallback(code),
    }
}

/// Whether a keysym is a modifier (never auto-repeated, released on blur).
pub fn is_modifier(keysym: u32) -> bool {
    matches!(keysym, 0xFFE1..=0xFFEE | 0xFE03)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn us_layout_by_physical_key() {
        assert_eq!(us_layout_char("KeyG", false), Some('g'));
        assert_eq!(us_layout_char("KeyQ", true), Some('Q'));
        assert_eq!(us_layout_char("Digit2", true), Some('@'));
        assert_eq!(us_layout_char("Slash", true), Some('?'));
        assert_eq!(us_layout_char("Space", true), Some(' '));
        // Keys that type nothing on their own.
        assert_eq!(us_layout_char("Lang1", false), None);
        assert_eq!(us_layout_char("Numpad1", false), None);
        assert_eq!(us_layout_char("Enter", false), None);
    }

    #[test]
    fn hangul_keys() {
        assert!(is_hangul_key("ㅎ"));
        assert!(is_hangul_key("한"));
        assert!(!is_hangul_key("g"));
        assert!(!is_hangul_key("é"));
        assert!(!is_hangul_key("Process"));
    }

    #[test]
    fn printable_characters() {
        assert_eq!(keysym_for_event("a", "KeyA", 0), Some('a' as u32));
        assert_eq!(keysym_for_event("A", "KeyA", 0), Some('A' as u32));
        assert_eq!(keysym_for_event(" ", "Space", 0), Some(0x20));
        assert_eq!(keysym_for_event("é", "KeyE", 0), Some(0xE9));
        assert_eq!(keysym_for_event("한", "KeyG", 0), Some(0x0100_D55C));
    }

    #[test]
    fn named_keys_and_locations() {
        assert_eq!(keysym_for_event("Enter", "Enter", 0), Some(0xFF0D));
        assert_eq!(keysym_for_event("Shift", "ShiftLeft", 1), Some(SHIFT_L));
        assert_eq!(keysym_for_event("Shift", "ShiftRight", 2), Some(SHIFT_R));
        assert_eq!(
            keysym_for_event("Control", "ControlRight", 2),
            Some(CONTROL_R)
        );
        assert_eq!(keysym_for_event("Meta", "MetaLeft", 1), Some(SUPER_L));
        assert_eq!(keysym_for_event("F5", "F5", 0), Some(0xFFC2));
        assert_eq!(keysym_for_event("HangulMode", "Lang1", 0), Some(0xFF31));
    }

    #[test]
    fn numpad() {
        assert_eq!(keysym_for_event("7", "Numpad7", 3), Some(0xFFB7));
        assert_eq!(keysym_for_event("Home", "Numpad7", 3), Some(0xFF50));
        assert_eq!(keysym_for_event("Enter", "NumpadEnter", 3), Some(0xFF8D));
        assert_eq!(keysym_for_event("+", "NumpadAdd", 3), Some(0xFFAB));
    }

    #[test]
    fn fallbacks() {
        assert_eq!(
            keysym_for_event("Unidentified", "KeyQ", 0),
            Some('q' as u32)
        );
        assert_eq!(keysym_for_event("Dead", "Quote", 0), None);
        assert_eq!(keysym_for_event("Process", "KeyA", 0), None);
    }

    #[test]
    fn control_characters() {
        assert_eq!(keysym_from_codepoint(0x08), Some(0xFF08));
        assert_eq!(keysym_from_codepoint(0x7F), Some(0xFF7F));
    }
}

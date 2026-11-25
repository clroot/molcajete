//! Translation of X11 keysyms (as sent by Guacamole clients) into the bytes
//! a terminal sends to the remote application.

/// Modifier keys currently held down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

impl Modifiers {
    /// Tracks modifier keysyms (0xFFE1–0xFFEA). Returns `true` when the
    /// keysym was a modifier and must not produce any output.
    pub fn update(&mut self, keysym: u32, pressed: bool) -> bool {
        match keysym {
            0xFFE1 | 0xFFE2 => self.shift = pressed,
            0xFFE3 | 0xFFE4 => self.ctrl = pressed,
            0xFFE7 | 0xFFE8 => self.meta = pressed,
            0xFFE9 | 0xFFEA => self.alt = pressed,
            // Caps/Shift lock, Super, Hyper and AltGr (ISO_Level3_Shift)
            // change the keysyms the client sends but produce nothing.
            0xFFE5 | 0xFFE6 | 0xFFEB..=0xFFEE | 0xFE03 => {}
            _ => return false,
        }
        true
    }

    /// Alt and Meta both act as "meta sends escape".
    fn escape_prefix(self) -> bool {
        self.alt || self.meta
    }

    /// xterm's modifier parameter (`CSI 1 ; <m> X`), or `None` when no
    /// modifier applies.
    fn xterm_param(self) -> Option<u8> {
        let m = self.shift as u8 | ((self.escape_prefix() as u8) << 1) | ((self.ctrl as u8) << 2);
        (m != 0).then_some(m + 1)
    }
}

/// Terminal state and settings that influence key encoding.
#[derive(Clone, Copy, Debug)]
pub struct KeyOptions {
    /// Byte sent for BackSpace (guacd's `backspace` parameter, 127 or 8).
    pub backspace: u8,
    /// DECCKM: cursor keys send `ESC O x` instead of `ESC [ x`.
    pub app_cursor: bool,
    /// DECKPAM: the numeric keypad sends application sequences.
    pub app_keypad: bool,
    /// Linux console conventions (`terminal-type` = `linux`) for Home/End
    /// and F1–F5 instead of xterm's.
    pub linux: bool,
}

impl Default for KeyOptions {
    fn default() -> Self {
        Self {
            backspace: 0x7F,
            app_cursor: false,
            app_keypad: false,
            linux: false,
        }
    }
}

/// Unicode scalar typed by a keysym, if it is a character keysym
/// (Latin-1 keysyms are their code point, others are `0x01000000 | cp`).
pub fn keysym_to_char(keysym: u32) -> Option<char> {
    match keysym {
        0x20..=0x7E | 0xA0..=0xFF => char::from_u32(keysym),
        _ if keysym & 0xFF00_0000 == 0x0100_0000 => char::from_u32(keysym & 0x00FF_FFFF),
        _ => None,
    }
}

/// Control code produced by Ctrl+<c>, following xterm/guacd.
fn control_code(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' | 'A'..='Z' => (c as u8) & 0x1F,
        '@' | ' ' | '2' | '`' => 0x00,
        '[' | '3' | '{' => 0x1B,
        '\\' | '4' | '|' => 0x1C,
        ']' | '5' | '}' => 0x1D,
        '^' | '6' | '~' => 0x1E,
        '_' | '7' | '/' | '-' => 0x1F,
        '8' | '?' => 0x7F,
        _ => return None,
    })
}

/// Encodes a key press. Returns `None` for keys that send nothing.
pub fn encode_key(keysym: u32, mods: Modifiers, opts: &KeyOptions) -> Option<Vec<u8>> {
    if let Some(c) = keysym_to_char(keysym) {
        return Some(encode_char(c, mods));
    }
    let csi_tilde = |n: u8| -> Vec<u8> {
        match mods.xterm_param() {
            Some(m) => format!("\x1b[{n};{m}~").into_bytes(),
            None => format!("\x1b[{n}~").into_bytes(),
        }
    };
    // Cursor-style keys: `ESC [ X`, `ESC O X` (application mode) or
    // `ESC [ 1 ; m X` with modifiers.
    let cursor = |x: char| -> Vec<u8> {
        match mods.xterm_param() {
            Some(m) => format!("\x1b[1;{m}{x}").into_bytes(),
            None if opts.app_cursor => format!("\x1bO{x}").into_bytes(),
            None => format!("\x1b[{x}").into_bytes(),
        }
    };
    // F1–F4 use SS3 (`ESC O P`), or `ESC [ 1 ; m P` with modifiers.
    let ss3 = |x: char| -> Vec<u8> {
        match mods.xterm_param() {
            Some(m) => format!("\x1b[1;{m}{x}").into_bytes(),
            None => format!("\x1bO{x}").into_bytes(),
        }
    };
    let with_escape = |bytes: &[u8]| -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        if mods.escape_prefix() {
            out.push(0x1B);
        }
        out.extend_from_slice(bytes);
        out
    };
    let keypad_app = |x: char| format!("\x1bO{x}").into_bytes();

    Some(match keysym {
        // Ctrl+BackSpace sends the other erase character (like xterm).
        0xFF08 if mods.ctrl => with_escape(&[if opts.backspace == 0x7F { 0x08 } else { 0x7F }]),
        0xFF08 => with_escape(&[opts.backspace]),
        0xFF09 if mods.shift => b"\x1b[Z".to_vec(),
        0xFF09 => with_escape(b"\t"),
        0xFE20 => b"\x1b[Z".to_vec(),
        0xFF0A => with_escape(b"\n"),
        0xFF0D => with_escape(b"\r"),
        0xFF1B => with_escape(b"\x1b"),
        0xFF8D if opts.app_keypad => keypad_app('M'),
        0xFF8D => with_escape(b"\r"),

        0xFF50 | 0xFF95 if opts.linux && mods.xterm_param().is_none() => b"\x1b[1~".to_vec(),
        0xFF57 | 0xFF9C if opts.linux && mods.xterm_param().is_none() => b"\x1b[4~".to_vec(),
        0xFF50 | 0xFF95 => cursor('H'),
        0xFF57 | 0xFF9C => cursor('F'),
        0xFF51 | 0xFF96 => cursor('D'),
        0xFF52 | 0xFF97 => cursor('A'),
        0xFF53 | 0xFF98 => cursor('C'),
        0xFF54 | 0xFF99 => cursor('B'),
        0xFF9D => cursor('E'),
        0xFF55 | 0xFF9A => csi_tilde(5),
        0xFF56 | 0xFF9B => csi_tilde(6),
        0xFF63 | 0xFF9E => csi_tilde(2),
        0xFFFF | 0xFF9F => csi_tilde(3),

        // F1–F5 on the Linux console: `ESC [ [ A` … `ESC [ [ E`.
        0xFFBE..=0xFFC2 if opts.linux && mods.xterm_param().is_none() => {
            let letter = b'A' + (keysym - 0xFFBE) as u8;
            vec![0x1B, b'[', b'[', letter]
        }
        0xFFBE => ss3('P'),
        0xFFBF => ss3('Q'),
        0xFFC0 => ss3('R'),
        0xFFC1 => ss3('S'),
        0xFFC2 => csi_tilde(15),
        0xFFC3 => csi_tilde(17),
        0xFFC4 => csi_tilde(18),
        0xFFC5 => csi_tilde(19),
        0xFFC6 => csi_tilde(20),
        0xFFC7 => csi_tilde(21),
        0xFFC8 => csi_tilde(23),
        0xFFC9 => csi_tilde(24),

        // Keypad digits and operators (`*` … `9`).
        0xFFAA..=0xFFB9 => {
            const CHARS: &[u8; 16] = b"*+,-./0123456789";
            const APP: &[u8; 16] = b"jklmnopqrstuvwxy";
            let i = (keysym - 0xFFAA) as usize;
            if opts.app_keypad {
                keypad_app(APP[i] as char)
            } else {
                encode_char(CHARS[i] as char, mods)
            }
        }
        0xFFBD => encode_char('=', mods),
        _ => return None,
    })
}

/// Encodes a typed character, applying Ctrl (control codes) and Alt/Meta
/// (ESC prefix).
fn encode_char(c: char, mods: Modifiers) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    if mods.escape_prefix() {
        out.push(0x1B);
    }
    match control_code(c).filter(|_| mods.ctrl) {
        Some(code) => out.push(code),
        None => {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(keysym: u32, mods: Modifiers, opts: KeyOptions) -> Vec<u8> {
        encode_key(keysym, mods, &opts).unwrap_or_default()
    }

    const NONE: Modifiers = Modifiers {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
    };
    const CTRL: Modifiers = Modifiers { ctrl: true, ..NONE };
    const ALT: Modifiers = Modifiers { alt: true, ..NONE };
    const SHIFT: Modifiers = Modifiers {
        shift: true,
        ..NONE
    };

    #[test]
    fn printable_and_unicode_keysyms() {
        let o = KeyOptions::default();
        assert_eq!(key(0x61, NONE, o), b"a");
        assert_eq!(key(0x41, SHIFT, o), b"A");
        assert_eq!(key(0xE9, NONE, o), "é".as_bytes());
        assert_eq!(key(0x0100_D55C, NONE, o), "한".as_bytes());
        assert_eq!(key(0x0100_20AC, NONE, o), "€".as_bytes());
        assert_eq!(encode_key(0xFFE1, NONE, &o), None);
    }

    #[test]
    fn editing_keys() {
        let o = KeyOptions::default();
        assert_eq!(key(0xFF0D, NONE, o), b"\r");
        assert_eq!(key(0xFF08, NONE, o), b"\x7f");
        let bs8 = KeyOptions { backspace: 8, ..o };
        assert_eq!(key(0xFF08, NONE, bs8), b"\x08");
        assert_eq!(key(0xFF08, CTRL, o), b"\x08");
        assert_eq!(key(0xFF09, NONE, o), b"\t");
        assert_eq!(key(0xFF09, SHIFT, o), b"\x1b[Z");
        assert_eq!(key(0xFF1B, NONE, o), b"\x1b");
        assert_eq!(key(0xFF55, NONE, o), b"\x1b[5~");
        assert_eq!(key(0xFF56, NONE, o), b"\x1b[6~");
        assert_eq!(key(0xFF63, NONE, o), b"\x1b[2~");
        assert_eq!(key(0xFFFF, NONE, o), b"\x1b[3~");
        assert_eq!(key(0xFFFF, CTRL, o), b"\x1b[3;5~");
    }

    #[test]
    fn cursor_keys_honour_application_mode() {
        let normal = KeyOptions::default();
        let app = KeyOptions {
            app_cursor: true,
            ..normal
        };
        assert_eq!(key(0xFF52, NONE, normal), b"\x1b[A");
        assert_eq!(key(0xFF54, NONE, normal), b"\x1b[B");
        assert_eq!(key(0xFF53, NONE, normal), b"\x1b[C");
        assert_eq!(key(0xFF51, NONE, normal), b"\x1b[D");
        assert_eq!(key(0xFF52, NONE, app), b"\x1bOA");
        assert_eq!(key(0xFF50, NONE, normal), b"\x1b[H");
        assert_eq!(key(0xFF57, NONE, app), b"\x1bOF");
        // Modifiers always use the CSI form.
        assert_eq!(key(0xFF53, CTRL, app), b"\x1b[1;5C");
        assert_eq!(key(0xFF51, ALT, normal), b"\x1b[1;3D");
        assert_eq!(key(0xFF52, SHIFT, normal), b"\x1b[1;2A");
    }

    #[test]
    fn function_keys() {
        let o = KeyOptions::default();
        assert_eq!(key(0xFFBE, NONE, o), b"\x1bOP");
        assert_eq!(key(0xFFC1, NONE, o), b"\x1bOS");
        assert_eq!(key(0xFFC2, NONE, o), b"\x1b[15~");
        assert_eq!(key(0xFFC6, NONE, o), b"\x1b[20~");
        assert_eq!(key(0xFFC8, NONE, o), b"\x1b[23~");
        assert_eq!(key(0xFFC9, NONE, o), b"\x1b[24~");
        assert_eq!(key(0xFFBE, SHIFT, o), b"\x1b[1;2P");
        assert_eq!(key(0xFFC9, CTRL, o), b"\x1b[24;5~");
        let linux = KeyOptions { linux: true, ..o };
        assert_eq!(key(0xFFBE, NONE, linux), b"\x1b[[A");
        assert_eq!(key(0xFFC2, NONE, linux), b"\x1b[[E");
        assert_eq!(key(0xFFC3, NONE, linux), b"\x1b[17~");
        assert_eq!(key(0xFF50, NONE, linux), b"\x1b[1~");
        assert_eq!(key(0xFF57, NONE, linux), b"\x1b[4~");
    }

    #[test]
    fn control_and_alt_combinations() {
        let o = KeyOptions::default();
        assert_eq!(key(0x63, CTRL, o), b"\x03");
        assert_eq!(key(0x43, CTRL, o), b"\x03");
        assert_eq!(key(0x40, CTRL, o), b"\x00");
        assert_eq!(key(0x20, CTRL, o), b"\x00");
        assert_eq!(key(0x5B, CTRL, o), b"\x1b");
        assert_eq!(key(0x5C, CTRL, o), b"\x1c");
        assert_eq!(key(0x5D, CTRL, o), b"\x1d");
        assert_eq!(key(0x5E, CTRL, o), b"\x1e");
        assert_eq!(key(0x5F, CTRL, o), b"\x1f");
        assert_eq!(key(0x3F, CTRL, o), b"\x7f");
        assert_eq!(key(0x31, CTRL, o), b"1");
        assert_eq!(key(0x62, ALT, o), b"\x1bb");
        let ctrl_alt = Modifiers { alt: true, ..CTRL };
        assert_eq!(key(0x78, ctrl_alt, o), b"\x1b\x18");
        let meta = Modifiers { meta: true, ..NONE };
        assert_eq!(key(0x66, meta, o), b"\x1bf");
        assert_eq!(key(0xFF0D, ALT, o), b"\x1b\r");
    }

    #[test]
    fn keypad() {
        let o = KeyOptions::default();
        assert_eq!(key(0xFFB5, NONE, o), b"5");
        assert_eq!(key(0xFFAB, NONE, o), b"+");
        assert_eq!(key(0xFF8D, NONE, o), b"\r");
        let app = KeyOptions {
            app_keypad: true,
            ..o
        };
        assert_eq!(key(0xFFB5, NONE, app), b"\x1bOu");
        assert_eq!(key(0xFF8D, NONE, app), b"\x1bOM");
        assert_eq!(key(0xFF97, NONE, o), b"\x1b[A");
    }

    #[test]
    fn tracks_modifiers() {
        let mut mods = Modifiers::default();
        assert!(mods.update(0xFFE3, true));
        assert!(mods.ctrl);
        assert!(mods.update(0xFFEA, true));
        assert!(mods.alt);
        assert!(mods.update(0xFFE3, false));
        assert!(!mods.ctrl);
        assert!(mods.update(0xFFE1, true));
        assert!(mods.shift);
        assert!(!mods.update(0x61, true));
    }
}

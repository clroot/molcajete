//! Mouse reporting (xterm X10/normal, UTF-8 and SGR encodings) for
//! applications that enable mouse tracking.

use alacritty_terminal::term::TermMode;

use super::keys::Modifiers;

/// Guacamole mouse button mask bits.
pub const LEFT: u32 = 1;
pub const MIDDLE: u32 = 2;
pub const RIGHT: u32 = 4;
pub const WHEEL_UP: u32 = 8;
pub const WHEEL_DOWN: u32 = 16;

/// Button number used in reports (before modifiers and motion flags).
pub fn button_code(button: u32) -> Option<u8> {
    Some(match button {
        LEFT => 0,
        MIDDLE => 1,
        RIGHT => 2,
        WHEEL_UP => 64,
        WHEEL_DOWN => 65,
        _ => return None,
    })
}

/// What happened to the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    /// A button was pressed (or the wheel turned).
    Press(u8),
    /// A button was released.
    Release(u8),
    /// The pointer moved, with this button held (`None`: no button).
    Motion(Option<u8>),
}

/// Encodes a mouse report for the given 0-based cell, or `None` when the
/// terminal mode does not ask for it or the position cannot be encoded.
pub fn encode_report(
    action: MouseAction,
    col: usize,
    row: usize,
    mods: Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    let (mut code, pressed) = match action {
        MouseAction::Press(code) => (code, true),
        MouseAction::Release(code) => {
            // Wheel "buttons" have no release events.
            if code >= 64 {
                return None;
            }
            (code, false)
        }
        MouseAction::Motion(button) => {
            let wanted = mode.contains(TermMode::MOUSE_MOTION)
                || (mode.contains(TermMode::MOUSE_DRAG) && button.is_some());
            if !wanted {
                return None;
            }
            (button.unwrap_or(3) + 32, true)
        }
    };
    if mods.shift {
        code += 4;
    }
    if mods.alt || mods.meta {
        code += 8;
    }
    if mods.ctrl {
        code += 16;
    }

    if mode.contains(TermMode::SGR_MOUSE) {
        let suffix = if pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{code};{};{}{suffix}", col + 1, row + 1).into_bytes());
    }

    // Legacy encodings report releases as button 3.
    if !pressed {
        code = (code & !0b11) | 3;
    }
    let mut out = vec![0x1B, b'[', b'M', 32 + code];
    for value in [col, row] {
        let value = value + 1 + 32;
        if mode.contains(TermMode::UTF8_MOUSE) {
            // Two-byte UTF-8 sequences at most (xterm's limit).
            let c = u32::try_from(value)
                .ok()
                .filter(|&v| v < 0x800)
                .and_then(char::from_u32)?;
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        } else {
            out.push(u8::try_from(value).ok()?);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_MODS: Modifiers = Modifiers {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
    };

    #[test]
    fn nothing_without_mouse_mode() {
        assert_eq!(
            encode_report(MouseAction::Press(0), 0, 0, NO_MODS, TermMode::default()),
            None
        );
    }

    #[test]
    fn sgr_reports() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            encode_report(MouseAction::Press(0), 4, 9, NO_MODS, mode).unwrap(),
            b"\x1b[<0;5;10M"
        );
        assert_eq!(
            encode_report(MouseAction::Release(0), 4, 9, NO_MODS, mode).unwrap(),
            b"\x1b[<0;5;10m"
        );
        assert_eq!(
            encode_report(MouseAction::Press(64), 0, 0, NO_MODS, mode).unwrap(),
            b"\x1b[<64;1;1M"
        );
        let ctrl = Modifiers {
            ctrl: true,
            ..NO_MODS
        };
        assert_eq!(
            encode_report(MouseAction::Press(2), 299, 0, ctrl, mode).unwrap(),
            b"\x1b[<18;300;1M"
        );
        // Plain click mode does not report motion.
        assert_eq!(
            encode_report(MouseAction::Motion(Some(0)), 1, 1, NO_MODS, mode),
            None
        );
        let drag = mode | TermMode::MOUSE_DRAG;
        assert_eq!(
            encode_report(MouseAction::Motion(Some(0)), 1, 1, NO_MODS, drag).unwrap(),
            b"\x1b[<32;2;2M"
        );
        assert_eq!(
            encode_report(MouseAction::Motion(None), 1, 1, NO_MODS, drag),
            None
        );
        let any = mode | TermMode::MOUSE_MOTION;
        assert_eq!(
            encode_report(MouseAction::Motion(None), 1, 1, NO_MODS, any).unwrap(),
            b"\x1b[<35;2;2M"
        );
    }

    #[test]
    fn legacy_reports() {
        let mode = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            encode_report(MouseAction::Press(0), 0, 0, NO_MODS, mode).unwrap(),
            b"\x1b[M\x20\x21\x21"
        );
        assert_eq!(
            encode_report(MouseAction::Release(2), 2, 3, NO_MODS, mode).unwrap(),
            b"\x1b[M\x23\x23\x24"
        );
        // Wheel buttons are never released.
        assert_eq!(
            encode_report(MouseAction::Release(65), 2, 3, NO_MODS, mode),
            None
        );
        // Too far right for one byte.
        assert_eq!(
            encode_report(MouseAction::Press(0), 300, 0, NO_MODS, mode),
            None
        );
        let utf8 = mode | TermMode::UTF8_MOUSE;
        let report = encode_report(MouseAction::Press(0), 300, 0, NO_MODS, utf8).unwrap();
        assert_eq!(report, "\x1b[M\u{20}\u{14D}\u{21}".as_bytes());
    }
}

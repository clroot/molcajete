//! Slow-path input, for servers that do not accept fast-path input
//! ([MS-RDPBCGR] 2.2.8.1.1.3).

use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags as FastKeyboardFlags};
use ironrdp_pdu::input::scan_code::KeyboardFlags as ScanCodeFlags;
use ironrdp_pdu::input::unicode::KeyboardFlags as UnicodeFlags;
use ironrdp_pdu::input::{InputEvent, ScanCodePdu, UnicodePdu};

/// Converts a fast-path input event into its slow-path equivalent.
pub fn input_event(event: &FastPathInputEvent) -> Option<InputEvent> {
    Some(match *event {
        FastPathInputEvent::KeyboardEvent(flags, code) => {
            let mut scan_flags = ScanCodeFlags::empty();
            if flags.contains(FastKeyboardFlags::RELEASE) {
                scan_flags |= ScanCodeFlags::RELEASE;
            }
            if flags.contains(FastKeyboardFlags::EXTENDED) {
                scan_flags |= ScanCodeFlags::EXTENDED;
            }
            if flags.contains(FastKeyboardFlags::EXTENDED1) {
                scan_flags |= ScanCodeFlags::EXTENDED_1;
            }
            InputEvent::ScanCode(ScanCodePdu {
                flags: scan_flags,
                key_code: u16::from(code),
            })
        }
        FastPathInputEvent::UnicodeKeyboardEvent(flags, unit) => InputEvent::Unicode(UnicodePdu {
            flags: if flags.contains(FastKeyboardFlags::RELEASE) {
                UnicodeFlags::RELEASE
            } else {
                UnicodeFlags::empty()
            },
            unicode_code: unit,
        }),
        FastPathInputEvent::MouseEvent(pdu) => InputEvent::Mouse(pdu),
        FastPathInputEvent::MouseEventEx(pdu) => InputEvent::MouseX(pdu),
        FastPathInputEvent::MouseEventRel(pdu) => InputEvent::MouseRel(pdu),
        FastPathInputEvent::QoeEvent(_) | FastPathInputEvent::SyncEvent(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use ironrdp_pdu::input::MousePdu;
    use ironrdp_pdu::input::mouse::PointerFlags;

    use super::*;

    #[test]
    fn keyboard_flags_are_translated() {
        let released_extended = FastPathInputEvent::KeyboardEvent(
            FastKeyboardFlags::RELEASE | FastKeyboardFlags::EXTENDED,
            0x4B,
        );
        assert_eq!(
            input_event(&released_extended),
            Some(InputEvent::ScanCode(ScanCodePdu {
                flags: ScanCodeFlags::RELEASE | ScanCodeFlags::EXTENDED,
                key_code: 0x4B,
            }))
        );
        let unicode = FastPathInputEvent::UnicodeKeyboardEvent(FastKeyboardFlags::empty(), 0xD55C);
        assert_eq!(
            input_event(&unicode),
            Some(InputEvent::Unicode(UnicodePdu {
                flags: UnicodeFlags::empty(),
                unicode_code: 0xD55C,
            }))
        );
    }

    #[test]
    fn mouse_events_pass_through() {
        let pdu = MousePdu {
            flags: PointerFlags::MOVE,
            number_of_wheel_rotation_units: 0,
            x_position: 3,
            y_position: 4,
        };
        assert_eq!(
            input_event(&FastPathInputEvent::MouseEvent(pdu)),
            Some(InputEvent::Mouse(pdu))
        );
        assert_eq!(input_event(&FastPathInputEvent::QoeEvent(1)), None);
    }
}

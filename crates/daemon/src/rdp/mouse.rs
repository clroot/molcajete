//! Translation of Guacamole mouse state (position + button mask) into RDP
//! pointer events.

use ironrdp_pdu::input::MousePdu;
use ironrdp_pdu::input::mouse::PointerFlags;

const LEFT: u32 = 1;
const MIDDLE: u32 = 2;
const RIGHT: u32 = 4;
const WHEEL_UP: u32 = 8;
const WHEEL_DOWN: u32 = 16;

/// Rotation units of one wheel notch (`WHEEL_DELTA` on Windows).
const WHEEL_NOTCH: i16 = 120;

const BUTTONS: [(u32, PointerFlags); 3] = [
    (LEFT, PointerFlags::LEFT_BUTTON),
    (MIDDLE, PointerFlags::MIDDLE_BUTTON_OR_WHEEL),
    (RIGHT, PointerFlags::RIGHT_BUTTON),
];

/// Last pointer state sent to the server.
#[derive(Debug, Default)]
pub struct Mouse {
    x: u16,
    y: u16,
    mask: u32,
    /// Whether a position has been sent yet.
    positioned: bool,
}

impl Mouse {
    /// Releases the buttons held, where the pointer is.
    pub fn release_all(&mut self) -> Vec<MousePdu> {
        if self.mask & (LEFT | MIDDLE | RIGHT) == 0 {
            self.mask = 0;
            return Vec::new();
        }
        self.update(self.x, self.y, 0)
    }

    /// Returns the events turning the previous state into the new one: a
    /// move first, then button changes, then wheel notches (sent when the
    /// wheel "button" goes down, as Guacamole clients press and release it).
    pub fn update(&mut self, x: u16, y: u16, mask: u32) -> Vec<MousePdu> {
        let mut events = Vec::new();
        let event = |flags, units| MousePdu {
            flags,
            number_of_wheel_rotation_units: units,
            x_position: x,
            y_position: y,
        };

        if !self.positioned || (x, y) != (self.x, self.y) {
            events.push(event(PointerFlags::MOVE, 0));
        }
        let changed = mask ^ self.mask;
        for (bit, flag) in BUTTONS {
            if changed & bit != 0 {
                let down = if mask & bit != 0 {
                    PointerFlags::DOWN
                } else {
                    PointerFlags::empty()
                };
                events.push(event(flag | down, 0));
            }
        }
        let pressed = changed & mask;
        if pressed & WHEEL_UP != 0 {
            events.push(event(PointerFlags::VERTICAL_WHEEL, WHEEL_NOTCH));
        }
        if pressed & WHEEL_DOWN != 0 {
            events.push(event(PointerFlags::VERTICAL_WHEEL, -WHEEL_NOTCH));
        }

        self.x = x;
        self.y = y;
        self.mask = mask;
        self.positioned = true;
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(events: &[MousePdu]) -> Vec<PointerFlags> {
        events.iter().map(|e| e.flags).collect()
    }

    #[test]
    fn first_event_positions_the_pointer() {
        let mut m = Mouse::default();
        let events = m.update(0, 0, 0);
        assert_eq!(flags(&events), vec![PointerFlags::MOVE]);
        assert!(m.update(0, 0, 0).is_empty());
    }

    #[test]
    fn moves_and_clicks() {
        let mut m = Mouse::default();
        m.update(10, 20, 0);
        let events = m.update(30, 40, LEFT);
        assert_eq!(
            flags(&events),
            vec![
                PointerFlags::MOVE,
                PointerFlags::LEFT_BUTTON | PointerFlags::DOWN
            ]
        );
        assert_eq!((events[1].x_position, events[1].y_position), (30, 40));
        assert_eq!(flags(&m.update(30, 40, 0)), vec![PointerFlags::LEFT_BUTTON]);
        assert_eq!(
            flags(&m.update(30, 40, RIGHT | MIDDLE)),
            vec![
                PointerFlags::MIDDLE_BUTTON_OR_WHEEL | PointerFlags::DOWN,
                PointerFlags::RIGHT_BUTTON | PointerFlags::DOWN
            ]
        );
    }

    #[test]
    fn wheel_notches_are_sent_on_press_only() {
        let mut m = Mouse::default();
        m.update(5, 5, 0);
        let up = m.update(5, 5, WHEEL_UP);
        assert_eq!(flags(&up), vec![PointerFlags::VERTICAL_WHEEL]);
        assert_eq!(up[0].number_of_wheel_rotation_units, 120);
        assert!(m.update(5, 5, 0).is_empty());
        let down = m.update(5, 5, WHEEL_DOWN);
        assert_eq!(down[0].number_of_wheel_rotation_units, -120);
        assert!(MousePdu::WHEEL_ROTATION_RANGE.contains(&down[0].number_of_wheel_rotation_units));
    }

    #[test]
    fn drag_keeps_button_held() {
        let mut m = Mouse::default();
        m.update(0, 0, LEFT);
        let events = m.update(8, 9, LEFT);
        assert_eq!(flags(&events), vec![PointerFlags::MOVE]);
    }
}

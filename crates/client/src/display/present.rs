//! Timing of the wait between applying a frame and drawing it to the canvas.

use super::Span;

/// Frames that have been applied to the layers but not drawn to the canvas
/// yet. Only their number and times are kept, so a display that is not
/// redrawn for a long time (a hidden tab) costs nothing.
#[derive(Default, Clone, Copy, Debug)]
pub(super) struct Unpresented {
    count: u32,
    /// Sum of the times the frames were applied at.
    applied_total: f64,
    first_applied: f64,
}

impl Unpresented {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn add(&mut self, applied_at: f64) {
        if self.count == 0 {
            self.first_applied = applied_at;
        }
        self.count += 1;
        self.applied_total += applied_at;
    }

    /// Records in `waits` how long each frame has waited until `now`, and
    /// forgets the frames.
    pub fn present(&mut self, now: f64, waits: &mut Span) {
        let frames = std::mem::take(self);
        if frames.count == 0 {
            return;
        }
        let total = frames.count as f64 * now - frames.applied_total;
        waits.count += frames.count;
        waits.total_ms += total.max(0.0);
        waits.max_ms = waits.max_ms.max(now - frames.first_applied);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_frame_waits_until_the_composite() {
        let mut frames = Unpresented::default();
        let mut waits = Span::default();
        frames.add(100.0);
        frames.add(104.0);
        frames.add(110.0);
        frames.present(116.0, &mut waits);
        assert_eq!(waits.count, 3);
        assert_eq!(waits.total_ms, 16.0 + 12.0 + 6.0);
        assert_eq!(waits.max_ms, 16.0);
        assert!(frames.is_empty());
    }

    #[test]
    fn a_composite_without_new_frames_records_nothing() {
        let mut frames = Unpresented::default();
        let mut waits = Span::default();
        frames.add(10.0);
        frames.present(12.0, &mut waits);
        frames.present(40.0, &mut waits);
        assert_eq!(waits.count, 1);
        assert_eq!(waits.max_ms, 2.0);
    }

    #[test]
    fn waits_add_to_those_already_recorded() {
        let mut frames = Unpresented::default();
        let mut waits = Span::default();
        waits.add(9.0);
        frames.add(20.0);
        frames.present(23.0, &mut waits);
        assert_eq!(waits.count, 2);
        assert_eq!(waits.average_ms(), 6.0);
        assert_eq!(waits.max_ms, 9.0);
    }
}

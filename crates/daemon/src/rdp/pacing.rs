//! Frame pacing: when accumulated display changes are encoded and ended
//! with a `sync`.
//!
//! A frame is sent once the PDUs that are already available have been
//! processed (the socket would block), no earlier than the display's shared
//! pacing allows (`Display::next_frame_at` with `Pacing::DESKTOP`: up to
//! ~60 fps, less for a lagging client, an unacknowledged heavy frame or a
//! used-up byte budget) and never more often than [`MIN_FRAME_INTERVAL`],
//! which is the same floor counted from when the frame was written out.
//! A server that never pauses (video) still gets a frame every
//! [`MAX_FRAME_DELAY`].

use std::time::{Duration, Instant};

/// Shortest time between two frames (~60 fps).
pub const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// Longest time changes may wait while PDUs keep arriving.
pub const MAX_FRAME_DELAY: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub struct FramePacer {
    /// When the first change of the pending frame was made.
    dirty_since: Option<Instant>,
    last_frame: Instant,
}

impl FramePacer {
    pub fn new(now: Instant) -> Self {
        Self {
            dirty_since: None,
            last_frame: now.checked_sub(MIN_FRAME_INTERVAL).unwrap_or(now),
        }
    }

    pub fn mark_dirty(&mut self, now: Instant) {
        self.dirty_since.get_or_insert(now);
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty_since.is_some()
    }

    /// Earliest time the pending frame may be sent, given the display's
    /// own pacing deadline.
    pub fn earliest(&self, display_due: Instant) -> Instant {
        (self.last_frame + MIN_FRAME_INTERVAL).max(display_due)
    }

    /// Whether a stream of PDUs has delayed the pending frame for too long.
    pub fn overdue(&self, now: Instant, display_due: Instant) -> bool {
        self.dirty_since
            .is_some_and(|since| now.saturating_duration_since(since) >= MAX_FRAME_DELAY)
            && now >= self.earliest(display_due)
    }

    pub fn frame_sent(&mut self, now: Instant) {
        self.dirty_since = None;
        self.last_frame = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A display deadline that never constrains the pacer.
    fn t_any() -> Instant {
        Instant::now() - Duration::from_secs(3600)
    }

    #[test]
    fn first_frame_is_not_delayed() {
        let now = Instant::now();
        let mut pacer = FramePacer::new(now);
        assert!(!pacer.is_dirty());
        pacer.mark_dirty(now);
        assert!(pacer.is_dirty());
        assert!(pacer.earliest(t_any()) <= now);
    }

    #[test]
    fn frames_are_capped_at_60_fps() {
        let t0 = Instant::now();
        let mut pacer = FramePacer::new(t0);
        pacer.frame_sent(t0);
        pacer.mark_dirty(t0 + Duration::from_millis(1));
        assert_eq!(pacer.earliest(t_any()), t0 + MIN_FRAME_INTERVAL);
    }

    #[test]
    fn continuous_updates_eventually_force_a_frame() {
        let t0 = Instant::now();
        let mut pacer = FramePacer::new(t0);
        pacer.frame_sent(t0);
        pacer.mark_dirty(t0 + Duration::from_millis(5));
        assert!(!pacer.overdue(t0 + Duration::from_millis(50), t_any()));
        // Later dirty marks do not postpone the deadline.
        pacer.mark_dirty(t0 + Duration::from_millis(60));
        assert!(pacer.overdue(t0 + Duration::from_millis(105), t_any()));
        pacer.frame_sent(t0 + Duration::from_millis(105));
        assert!(!pacer.is_dirty());
        assert!(!pacer.overdue(t0 + Duration::from_millis(500), t_any()));
    }
}

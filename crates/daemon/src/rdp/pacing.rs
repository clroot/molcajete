//! Frame pacing: when accumulated display changes are encoded and ended
//! with a `sync`.
//!
//! A frame is sent once the PDUs that are already available have been
//! processed (the socket would block), no earlier than the display's shared
//! pacing allows (`Display::next_frame_at` with `Pacing::DESKTOP`: up to
//! ~60 fps, less for a lagging client, an unacknowledged heavy frame or a
//! used-up byte budget) and never more often than [`MIN_FRAME_INTERVAL`].
//! Both intervals count from when building the last frame began, so that
//! they bound the frame rate whatever a frame takes to encode. A frame in
//! which everything came whole ([`FramePacer::pending_is_whole`]) does not
//! wait for the socket: it goes out when it is due.
//! A server that never pauses (video) still gets a frame every
//! [`MAX_FRAME_DELAY`], and one that ends its frames (the graphics pipeline,
//! Frame Marker surface commands) gets one where it ends a frame, as soon as
//! the pacing allows: what is pending then is whole, and its next frame may
//! already be arriving.
//!
//! A server that marks its frames (Frame Marker surface commands) is not
//! cut in the middle of one: while it is between two frame ends, the pending
//! frame waits for the end, at most [`MAX_FRAME_DELAY`]. A screen update
//! that takes the server longer than our frame interval to send would
//! otherwise reach the client in halves, new above and old below.

use std::time::{Duration, Instant};

/// Shortest time between two frames (~60 fps).
pub const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// Longest time changes may wait while PDUs keep arriving, or for the
/// server to end its frame.
pub const MAX_FRAME_DELAY: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub struct FramePacer {
    /// When the first change of the pending frame was made.
    dirty_since: Option<Instant>,
    last_frame: Instant,
    /// Whether the server has ended a frame, which shows that it marks them.
    marks_frames: bool,
    /// Graphics arrived after the server's last frame end: it is inside a
    /// frame.
    in_server_frame: bool,
    /// Graphics arrived from a server that marks no frames: what is pending
    /// may be a part of an update.
    partial: bool,
}

impl FramePacer {
    pub fn new(now: Instant) -> Self {
        Self {
            dirty_since: None,
            last_frame: now.checked_sub(MIN_FRAME_INTERVAL).unwrap_or(now),
            marks_frames: false,
            in_server_frame: false,
            partial: false,
        }
    }

    pub fn mark_dirty(&mut self, now: Instant) {
        self.dirty_since.get_or_insert(now);
    }

    /// The server changed the screen. Unlike other changes (the cursor),
    /// this is part of a frame of a server that marks them.
    pub fn graphics_updated(&mut self, now: Instant) {
        self.mark_dirty(now);
        self.in_server_frame = self.marks_frames;
        self.partial = !self.marks_frames;
    }

    /// The server ended a frame: what it sent so far belongs together.
    pub fn server_frame_ended(&mut self) {
        self.marks_frames = true;
        self.in_server_frame = false;
        self.partial = false;
    }

    /// Whether the server has been seen to mark its frames.
    pub fn marks_frames(&self) -> bool {
        self.marks_frames
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty_since.is_some()
    }

    /// Earliest time the pending frame may be sent, given the display's
    /// own pacing deadline.
    pub fn earliest(&self, display_due: Instant) -> Instant {
        let paced = (self.last_frame + MIN_FRAME_INTERVAL).max(display_due);
        match self.dirty_since {
            Some(since) if self.in_server_frame => paced.max(since + MAX_FRAME_DELAY),
            _ => paced,
        }
    }

    /// Whether everything that is pending came whole: frames the server
    /// ended (the graphics pipeline's, or marked ones) and changes that are
    /// whole in themselves (the cursor). Such a frame can go out when it is
    /// due, whatever the server is sending meanwhile; one that may be a part
    /// of an update waits until the PDUs that are ready have been read.
    pub fn pending_is_whole(&self) -> bool {
        self.is_dirty() && !self.partial && !self.in_server_frame
    }

    /// Whether the pending frame should be sent now that the server has
    /// ended a frame of its own: what is pending is whole, so it need not
    /// wait for the socket to fall silent.
    ///
    /// The server's next frame often follows at once (video on a large
    /// desktop), and waiting for a pause would hold every frame until it is
    /// [`Self::overdue`] and merged with the next ones.
    pub fn due_at_frame_end(&self, now: Instant, display_due: Instant) -> bool {
        self.is_dirty() && !self.in_server_frame && now >= self.earliest(display_due)
    }

    /// Whether a stream of PDUs has delayed the pending frame for too long.
    pub fn overdue(&self, now: Instant, display_due: Instant) -> bool {
        self.dirty_since
            .is_some_and(|since| now.saturating_duration_since(since) >= MAX_FRAME_DELAY)
            && now >= self.earliest(display_due)
    }

    /// The pending frame was sent; building it began at `started`, from
    /// where the shortest interval to the next one counts.
    pub fn frame_sent(&mut self, started: Instant) {
        self.dirty_since = None;
        self.last_frame = started;
        self.partial = false;
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
    fn only_frames_that_came_whole_go_out_while_the_server_sends() {
        let t0 = Instant::now();
        let mut pacer = FramePacer::new(t0);
        assert!(!pacer.pending_is_whole(), "nothing pending");
        // The cursor, or a frame of the graphics pipeline.
        pacer.mark_dirty(t0);
        assert!(pacer.pending_is_whole());
        // A bitmap update of a server that marks no frames.
        pacer.graphics_updated(t0);
        assert!(!pacer.pending_is_whole());
        pacer.frame_sent(t0);
        pacer.mark_dirty(t0);
        assert!(pacer.pending_is_whole());
        // A server that marks its frames: whole from one end to the next
        // frame's first update.
        pacer.server_frame_ended();
        assert!(pacer.pending_is_whole());
        pacer.graphics_updated(t0);
        assert!(!pacer.pending_is_whole());
        pacer.server_frame_ended();
        assert!(pacer.pending_is_whole());
    }

    #[test]
    fn a_frame_is_due_where_the_server_ends_one() {
        let t0 = Instant::now();
        let mut pacer = FramePacer::new(t0);
        pacer.frame_sent(t0);
        // Nothing changed: nothing to send.
        assert!(!pacer.due_at_frame_end(t0 + Duration::from_millis(50), t_any()));
        pacer.graphics_updated(t0 + Duration::from_millis(20));
        pacer.server_frame_ended();
        // Long before the changes are overdue.
        let now = t0 + Duration::from_millis(25);
        assert!(!pacer.overdue(now, t_any()));
        assert!(pacer.due_at_frame_end(now, t_any()));
        // Not sooner than the shortest interval, or than the display allows.
        assert!(!pacer.due_at_frame_end(t0 + Duration::from_millis(10), t_any()));
        assert!(!pacer.due_at_frame_end(now, now + Duration::from_millis(5)));
    }

    #[test]
    fn a_frame_is_not_due_inside_a_server_frame() {
        let t0 = Instant::now();
        let mut pacer = FramePacer::new(t0);
        pacer.frame_sent(t0);
        pacer.server_frame_ended();
        // The server started its next frame.
        pacer.graphics_updated(t0 + Duration::from_millis(20));
        assert!(!pacer.due_at_frame_end(t0 + Duration::from_millis(30), t_any()));
        pacer.server_frame_ended();
        assert!(pacer.due_at_frame_end(t0 + Duration::from_millis(30), t_any()));
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

    #[test]
    fn a_frame_waits_for_the_server_to_end_its_own() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut pacer = FramePacer::new(t0);
        // Until the server has ended a frame, nothing says it marks them.
        pacer.graphics_updated(t0);
        assert!(pacer.earliest(t_any()) <= t0);
        pacer.server_frame_ended();
        pacer.frame_sent(t0);

        // The next frame is under way: it is not sent half-way.
        pacer.graphics_updated(t0 + ms(20));
        assert_eq!(pacer.earliest(t_any()), t0 + ms(20) + MAX_FRAME_DELAY);
        pacer.graphics_updated(t0 + ms(50));
        assert_eq!(pacer.earliest(t_any()), t0 + ms(20) + MAX_FRAME_DELAY);
        // Once it has ended, pacing alone decides.
        pacer.server_frame_ended();
        assert_eq!(pacer.earliest(t_any()), t0 + MIN_FRAME_INTERVAL);
        pacer.frame_sent(t0 + ms(60));

        // The cursor is not part of a frame.
        pacer.mark_dirty(t0 + ms(61));
        assert_eq!(pacer.earliest(t_any()), t0 + ms(60) + MIN_FRAME_INTERVAL);
    }

    #[test]
    fn a_frame_the_server_does_not_end_is_sent_anyway() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut pacer = FramePacer::new(t0);
        pacer.server_frame_ended();
        pacer.graphics_updated(t0);
        assert!(!pacer.overdue(t0 + ms(99), t_any()));
        assert!(pacer.overdue(t0 + ms(100), t_any()));
        pacer.frame_sent(t0 + ms(100));
        // What follows of the same frame waits again, from its own start.
        pacer.graphics_updated(t0 + ms(110));
        assert_eq!(pacer.earliest(t_any()), t0 + ms(110) + MAX_FRAME_DELAY);
    }
}

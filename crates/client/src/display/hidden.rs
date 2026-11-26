//! When the page was hidden (in the background, or its device asleep).
//!
//! A browser may suspend a hidden page altogether. A frame that was
//! received before and applied after has waited for the user, not for the
//! client, so that time is left out of the frame lag.

/// The time the page is hidden since, and the last time it was hidden.
#[derive(Default, Clone, Copy, Debug)]
pub(super) struct Hidden {
    /// Since when the page is hidden, while it is.
    since: Option<f64>,
    /// Start and end of the last time it was hidden, once that is over.
    last: Option<(f64, f64)>,
}

impl Hidden {
    /// The page became visible or hidden at `now`.
    pub fn set_visible(&mut self, visible: bool, now: f64) {
        match (visible, self.since) {
            (false, None) => self.since = Some(now),
            (true, Some(since)) => {
                self.since = None;
                self.last = Some((since, now));
            }
            _ => {}
        }
    }

    /// Milliseconds between `from` and `to` (which is not after now) in
    /// which the page was hidden.
    ///
    /// Only the last two times it was hidden are known; nothing waits
    /// through more of them.
    pub fn within(&self, from: f64, to: f64) -> f64 {
        let overlap = |start: f64, end: f64| (end.min(to) - start.max(from)).max(0.0);
        self.last.map_or(0.0, |(start, end)| overlap(start, end))
            + self.since.map_or(0.0, |since| overlap(since, to))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_never_hidden_takes_nothing_off() {
        assert_eq!(Hidden::default().within(100.0, 160.0), 0.0);
    }

    #[test]
    fn the_hidden_part_of_an_interval_is_counted() {
        let mut hidden = Hidden::default();
        hidden.set_visible(false, 1_000.0);
        hidden.set_visible(true, 61_000.0);
        // Received before the page was hidden, applied after it came back.
        assert_eq!(hidden.within(990.0, 61_020.0), 60_000.0);
        // Entirely before, entirely after, and partly inside.
        assert_eq!(hidden.within(900.0, 950.0), 0.0);
        assert_eq!(hidden.within(61_000.0, 61_050.0), 0.0);
        assert_eq!(hidden.within(60_990.0, 61_050.0), 10.0);
    }

    #[test]
    fn a_page_still_hidden_counts_up_to_the_end_of_the_interval() {
        let mut hidden = Hidden::default();
        hidden.set_visible(false, 1_000.0);
        // The frame is applied before the page is told that it is visible.
        assert_eq!(hidden.within(990.0, 61_020.0), 60_020.0);
    }

    #[test]
    fn repeated_states_do_not_move_the_start() {
        let mut hidden = Hidden::default();
        hidden.set_visible(true, 500.0);
        hidden.set_visible(false, 1_000.0);
        hidden.set_visible(false, 2_000.0);
        hidden.set_visible(true, 3_000.0);
        hidden.set_visible(true, 4_000.0);
        assert_eq!(hidden.within(0.0, 5_000.0), 2_000.0);
    }

    #[test]
    fn hidden_again_after_the_last_time() {
        let mut hidden = Hidden::default();
        hidden.set_visible(false, 1_000.0);
        hidden.set_visible(true, 2_000.0);
        hidden.set_visible(false, 2_500.0);
        assert_eq!(hidden.within(900.0, 3_000.0), 1_500.0);
    }
}

//! Frame pacing: when the next frame may be sent.
//!
//! Every frame ends with a `sync` that the client answers once it has
//! applied the frame, and the next frame waits as long as the client needs
//! for one:
//!
//! - A molcajete client says how long it took to apply the frame in its
//!   reply (`molcajete_protocol::apply_time`). The interval is the median
//!   of the last few of those times, so one slow frame does not hold back
//!   the frames after it, and times older than a second are forgotten, so a
//!   pause lets the interval start afresh. The round trip is left to the
//!   window of unacknowledged frames that the sessions keep
//!   (`MAX_UNACKED_FRAMES`).
//! - A stock client reports nothing. Its frames wait for the *lag*: the
//!   smoothed time until the reply (network both ways plus the client's
//!   decoding and drawing).
//!
//! On top of that, a [`Pacing`] profile says how fast frames may go at best
//! and what happens after a heavy frame:
//!
//! - [`Pacing::DESKTOP`] (RDP, VNC): up to ~60 fps. A heavy frame holds the
//!   next one back only until the client has acknowledged it, so a link with
//!   room is not slowed down, and a byte budget caps what one session may
//!   take from a link that would deliver even more.
//! - [`Pacing::TERMINAL`] (SSH): up to ~30 fps, and a fixed pause after
//!   every heavy frame however fast it was acknowledged. A flood of terminal
//!   output is not worth more frames, only more bytes.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Frames carrying more encoded data than this count as heavy.
const LARGE_FRAME_BYTES: usize = 32 * 1024;
/// Longest pause after a heavy frame (~15 fps, similar to guacd under floods
/// of output).
const LARGE_FRAME_INTERVAL: Duration = Duration::from_millis(66);
/// Upper bound for waiting on a slow client (same as guacd), and for
/// waiting on the byte budget: the screen never stands still for longer.
const MAX_LAG_COMPENSATION: Duration = Duration::from_millis(500);
/// How much of the byte budget may be sent at once, as time at the budget's
/// rate: a short burst (a window opening, a page appearing) is not held back.
const BUDGET_BURST: Duration = Duration::from_millis(250);
/// How many of the client's reported apply times the interval is taken
/// from (their median).
const APPLY_SAMPLES: usize = 8;
/// How long a reported apply time counts. After a pause this long the
/// interval starts afresh, however slow the frames before it were.
const APPLY_MEMORY: Duration = Duration::from_secs(1);

/// A client's `sync` reply as it reached us.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncReply {
    /// When it arrived, which can be well before the session gets to it.
    pub at: Instant,
    /// How long the client took to apply the frame, if it said so
    /// (`molcajete_protocol::apply_time`).
    pub apply: Option<Duration>,
}

impl From<Instant> for SyncReply {
    /// A reply that says nothing more than that the frame was applied.
    fn from(at: Instant) -> Self {
        Self { at, apply: None }
    }
}

/// How fast frames may follow each other at best.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pacing {
    /// Shortest interval between frames while content keeps changing.
    min_interval: Duration,
    /// Whether the pause after a heavy frame ends as soon as the client has
    /// acknowledged that frame (and every frame before it).
    heavy_until_acked: bool,
    /// Encoded bytes per second that may be sent before further frames
    /// wait. Frames above the budget are spaced out evenly rather than sent
    /// in a burst followed by a standstill.
    budget: Option<usize>,
}

impl Pacing {
    /// Remote desktops: scrolling and dragging are worth every frame the
    /// link and the client can take.
    pub const DESKTOP: Self = Self {
        min_interval: Duration::from_millis(16),
        heavy_until_acked: true,
        budget: Some(8 * 1024 * 1024),
    };

    /// Terminals: bandwidth under floods of output stays bounded whatever
    /// the link could carry.
    pub const TERMINAL: Self = Self {
        min_interval: Duration::from_millis(33),
        heavy_until_acked: false,
        budget: None,
    };
}

/// The frames sent to one client and its acknowledgements.
#[derive(Debug)]
pub(super) struct FrameClock {
    pacing: Pacing,
    /// Frames sent and not yet acknowledged: (timestamp, sent at).
    pending: VecDeque<(i64, Instant)>,
    /// Smoothed time the client needs to acknowledge a frame.
    lag: Duration,
    /// The last apply times the client reported, with when each arrived
    /// (newest last).
    applies: VecDeque<(Instant, Duration)>,
    /// The client reports apply times: they set the interval, not the lag.
    reports: bool,
    /// When the last frame was sent, and its encoded size.
    last_frame: Option<(Instant, usize)>,
    /// Bytes sent that the budget has not covered yet, as of the last frame.
    /// It shrinks at the budget's rate.
    backlog: usize,
}

impl FrameClock {
    pub(super) fn new(pacing: Pacing) -> Self {
        Self {
            pacing,
            pending: VecDeque::new(),
            lag: Duration::ZERO,
            applies: VecDeque::with_capacity(APPLY_SAMPLES),
            reports: false,
            last_frame: None,
            backlog: 0,
        }
    }

    pub(super) fn set_pacing(&mut self, pacing: Pacing) {
        self.pacing = pacing;
    }

    /// A frame of `bytes` encoded bytes was ended with `sync(timestamp)`.
    pub(super) fn frame_sent(&mut self, timestamp: i64, now: Instant, bytes: usize) {
        self.pending.push_back((timestamp, now));
        if let Some(budget) = self.pacing.budget {
            let since_last = self.last_frame.map_or(Duration::MAX, |(last, _)| {
                now.saturating_duration_since(last)
            });
            // Beyond this the wait would exceed MAX_LAG_COMPENSATION anyway.
            let most = within(budget, BUDGET_BURST + MAX_LAG_COMPENSATION);
            self.backlog = self
                .backlog
                .saturating_sub(within(budget, since_last))
                .saturating_add(bytes)
                .min(most);
        }
        self.last_frame = Some((now, bytes));
    }

    /// The client's reply to `sync(timestamp)` arrived.
    /// Returns the time that frame took to be acknowledged, if it was still
    /// pending. Frames before it count as acknowledged too.
    pub(super) fn acked(
        &mut self,
        timestamp: i64,
        reply: impl Into<SyncReply>,
    ) -> Option<Duration> {
        let reply = reply.into();
        let received = reply.at;
        let mut acked = None;
        while let Some(&(ts, sent_at)) = self.pending.front() {
            if ts > timestamp {
                break;
            }
            self.pending.pop_front();
            if ts == timestamp {
                let elapsed = received.saturating_duration_since(sent_at);
                self.lag = (self.lag * 3 + elapsed.min(MAX_LAG_COMPENSATION)) / 4;
                acked = Some(elapsed);
                if let Some(apply) = reply.apply {
                    if self.applies.len() == APPLY_SAMPLES {
                        self.applies.pop_front();
                    }
                    self.applies.push_back((received, apply));
                    self.reports = true;
                }
            }
        }
        acked
    }

    pub(super) fn unacked(&self) -> usize {
        self.pending.len()
    }

    pub(super) fn lag(&self) -> Duration {
        self.lag
    }

    /// How long the client needs per frame, as of `at`: the median of the
    /// apply times it reported within [`APPLY_MEMORY`] before `at` (the
    /// lower one of an even count), nothing once they are all older, and the
    /// lag for a client that does not report them.
    fn client_interval(&self, at: Instant) -> Duration {
        if !self.reports {
            return self.lag;
        }
        let mut recent: Vec<Duration> = self
            .applies
            .iter()
            .filter(|&&(received, _)| at.saturating_duration_since(received) < APPLY_MEMORY)
            .map(|&(_, apply)| apply)
            .collect();
        if recent.is_empty() {
            return Duration::ZERO;
        }
        recent.sort_unstable();
        recent[(recent.len() - 1) / 2]
    }

    /// When the next frame may be sent; `None` before the first frame.
    ///
    /// This moves earlier when an acknowledgement arrives (a heavy frame was
    /// delivered), so it has to be asked again after every `acked`.
    ///
    /// `at_least` raises the profile's shortest interval, for content that
    /// must not be sent faster.
    pub(super) fn next_frame_at(&self, at_least: Duration) -> Option<Instant> {
        let (last, bytes) = self.last_frame?;
        let mut interval = self
            .client_interval(last)
            .clamp(self.pacing.min_interval.max(at_least), MAX_LAG_COMPENSATION);
        let delivered = self.pacing.heavy_until_acked && self.pending.is_empty();
        if bytes > LARGE_FRAME_BYTES && !delivered {
            interval = interval.max(LARGE_FRAME_INTERVAL);
        }
        Some(last + interval.max(self.budget_wait()))
    }

    /// How long after the last frame the bytes sent are back within the
    /// budget (at most [`MAX_LAG_COMPENSATION`]).
    fn budget_wait(&self) -> Duration {
        let Some(budget) = self.pacing.budget else {
            return Duration::ZERO;
        };
        let over = self.backlog.saturating_sub(within(budget, BUDGET_BURST));
        Duration::from_secs_f64(over as f64 / budget as f64).min(MAX_LAG_COMPENSATION)
    }
}

/// Bytes a budget of `per_second` covers in `time`.
fn within(per_second: usize, time: Duration) -> usize {
    (per_second as f64 * time.as_secs_f64()).min(usize::MAX as f64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    /// What a client received while the screen changed without pause.
    struct Run {
        /// Send time of every frame, from the start of the run.
        frames: Vec<Duration>,
        frame_bytes: usize,
        /// Most frames unacknowledged at once.
        most_unacked: usize,
    }

    impl Run {
        fn fps(&self, seconds: u32) -> f64 {
            self.frames.len() as f64 / seconds as f64
        }

        fn bytes(&self) -> usize {
            self.frames.len() * self.frame_bytes
        }

        /// Most bytes sent within any one second.
        fn peak_bytes_per_second(&self) -> usize {
            (0..self.frames.len())
                .map(|i| {
                    let end = self.frames[i] + Duration::from_secs(1);
                    self.frames[i..].iter().take_while(|&&t| t < end).count() * self.frame_bytes
                })
                .max()
                .unwrap_or(0)
        }
    }

    /// A simulated client. A frame reaches it after half the round trip, it
    /// applies the frames one after the other, and its reply takes the other
    /// half of the round trip back.
    #[derive(Clone, Copy)]
    struct Client {
        rtt: Duration,
        /// Time to apply the n-th frame (counting from 0).
        apply: fn(usize) -> Duration,
        /// Whether its replies say how long applying took (molcajete
        /// client) or not (stock client).
        reports: bool,
    }

    impl Client {
        /// A stock client whose reply arrives `ack_after` each frame.
        fn acking_after(ack_after: Duration) -> Self {
            Self {
                rtt: ack_after,
                apply: |_| Duration::ZERO,
                reports: false,
            }
        }

        fn reporting(self, reports: bool) -> Self {
            Self { reports, ..self }
        }
    }

    /// Sends a frame of `frame_bytes` whenever pacing and the frame window
    /// (`window` unacknowledged frames, as the sessions enforce it) allow,
    /// for `seconds`, to a stock client whose reply arrives `ack_after` each
    /// frame.
    fn run(
        pacing: Pacing,
        frame_bytes: usize,
        ack_after: Duration,
        window: usize,
        seconds: u32,
    ) -> Run {
        run_client(
            pacing,
            frame_bytes,
            Client::acking_after(ack_after),
            window,
            seconds,
        )
    }

    /// Like [`run`], for any client.
    fn run_client(
        pacing: Pacing,
        frame_bytes: usize,
        client: Client,
        window: usize,
        seconds: u32,
    ) -> Run {
        let start = Instant::now();
        let mut clock = FrameClock::new(pacing);
        let mut acks: VecDeque<(Instant, i64, SyncReply)> = VecDeque::new();
        let mut frames = Vec::new();
        let mut most_unacked = 0;
        // When the client is done with the frames it has.
        let mut client_free = start;
        for tick in 0..seconds * 1000 {
            let now = start + MS * tick;
            while acks.front().is_some_and(|&(at, _, _)| at <= now) {
                let (_, ts, reply) = acks.pop_front().unwrap();
                clock.acked(ts, reply);
            }
            let due = clock
                .next_frame_at(Duration::ZERO)
                .is_none_or(|due| due <= now);
            if due && clock.unacked() < window {
                let ts = i64::from(tick);
                clock.frame_sent(ts, now, frame_bytes);
                most_unacked = most_unacked.max(clock.unacked());
                let arrived = now + client.rtt / 2;
                let started = arrived.max(client_free);
                client_free = started + (client.apply)(frames.len());
                let at = client_free + (client.rtt - client.rtt / 2);
                let apply = client.reports.then(|| client_free - started);
                acks.push_back((at, ts, SyncReply { at, apply }));
                frames.push(now - start);
            }
        }
        Run {
            frames,
            frame_bytes,
            most_unacked,
        }
    }

    #[test]
    fn the_first_frame_is_not_delayed() {
        for pacing in [Pacing::DESKTOP, Pacing::TERMINAL] {
            assert_eq!(FrameClock::new(pacing).next_frame_at(Duration::ZERO), None);
        }
    }

    /// The display hands its frames and acknowledgements to the clock.
    #[test]
    fn a_display_paces_by_its_profile() {
        use super::super::{Display, ImageFormats};
        use crate::io::Output;

        let heavy_frame = |pacing| {
            let mut d = Display::new(
                ImageFormats {
                    jpeg: false,
                    zstd: false,
                },
                true,
            )
            .with_pacing(pacing);
            let mut out = Output::text();
            d.frame_bytes = 100_000;
            d.sync(&mut out);
            let sent = Instant::now();
            assert!(d.next_frame_at() >= sent + 60 * MS);
            d.ack(d.last_sync.unwrap(), sent);
            assert_eq!(d.unacked_frames(), 0);
            (d.next_frame_at(), sent)
        };
        let (due, sent) = heavy_frame(Pacing::DESKTOP);
        assert!(due <= sent + 16 * MS);
        let (due, sent) = heavy_frame(Pacing::TERMINAL);
        assert!(due >= sent + 60 * MS);
    }

    /// The display asks for apply times and paces by the ones it gets.
    #[test]
    fn a_display_asks_for_apply_times() {
        use super::super::{Display, ImageFormats};
        use crate::io::Output;
        use molcajete_protocol::{apply_time, parse_all};

        let mut d = Display::new(
            ImageFormats {
                jpeg: false,
                zstd: false,
            },
            true,
        )
        .with_pacing(Pacing::DESKTOP);
        let mut out = Output::text();
        d.sync(&mut out);
        let sync = &parse_all(&out.to_string()).unwrap()[0];
        assert_eq!(sync.opcode, "sync");
        assert!(apply_time::is_request(sync.arg(2)), "{sync}");
        let sent = Instant::now();
        d.ack(
            d.last_sync.unwrap(),
            SyncReply {
                at: sent + 300 * MS,
                apply: Some(40 * MS),
            },
        );
        // Not the 300 ms of the reply (75 ms smoothed): the 40 ms.
        let due = d.next_frame_at();
        assert!(
            due <= sent + 40 * MS && due >= sent + 30 * MS,
            "{:?}",
            due - sent
        );
    }

    #[test]
    fn heavy_desktop_frames_follow_a_fast_client() {
        // 100 kB per frame (5K scrolling), acknowledged after 12 ms: the
        // fixed pause after heavy frames held this to 15 fps.
        let r = run(Pacing::DESKTOP, 100_000, 12 * MS, 3, 4);
        assert!(r.fps(4) > 50.0, "{} fps", r.fps(4));
    }

    #[test]
    fn a_heavy_desktop_frame_waits_for_its_acknowledgement() {
        let t0 = Instant::now();
        let mut clock = FrameClock::new(Pacing::DESKTOP);
        clock.frame_sent(1, t0, 100_000);
        // Not acknowledged yet: the frame may still be on its way.
        assert_eq!(
            clock.next_frame_at(Duration::ZERO),
            Some(t0 + LARGE_FRAME_INTERVAL)
        );
        clock.acked(1, t0 + 20 * MS);
        // Delivered and applied: only the lag (20 ms / 4 smoothed, so the
        // 16 ms floor) is left.
        assert_eq!(clock.next_frame_at(Duration::ZERO), Some(t0 + 16 * MS));
    }

    #[test]
    fn a_heavy_desktop_frame_is_not_released_by_an_earlier_acknowledgement() {
        let t0 = Instant::now();
        let mut clock = FrameClock::new(Pacing::DESKTOP);
        clock.frame_sent(1, t0, 100);
        clock.frame_sent(2, t0 + 20 * MS, 100_000);
        clock.acked(1, t0 + 25 * MS);
        assert_eq!(
            clock.next_frame_at(Duration::ZERO),
            Some(t0 + 20 * MS + LARGE_FRAME_INTERVAL)
        );
    }

    #[test]
    fn the_caller_can_ask_for_a_longer_interval() {
        let t0 = Instant::now();
        let mut clock = FrameClock::new(Pacing::DESKTOP);
        clock.frame_sent(1, t0, 2_000);
        assert_eq!(clock.next_frame_at(33 * MS), Some(t0 + 33 * MS));
        // It is a floor, not the interval: a heavy frame still waits.
        clock.frame_sent(2, t0, 100_000);
        assert_eq!(
            clock.next_frame_at(33 * MS),
            Some(t0 + LARGE_FRAME_INTERVAL)
        );
    }

    #[test]
    fn small_desktop_frames_do_not_wait_for_acknowledgements() {
        let t0 = Instant::now();
        let mut clock = FrameClock::new(Pacing::DESKTOP);
        clock.frame_sent(1, t0, 2_000);
        assert_eq!(clock.next_frame_at(Duration::ZERO), Some(t0 + 16 * MS));
    }

    #[test]
    fn a_slow_client_slows_desktop_frames_down() {
        // Replies take 120 ms: frames must not go out faster than they are
        // acknowledged, heavy or not.
        for bytes in [2_000, 100_000] {
            let r = run(Pacing::DESKTOP, bytes, 120 * MS, 3, 8);
            assert!(r.fps(8) < 10.0, "{bytes} B: {} fps", r.fps(8));
        }
        // A link slower than the old fixed pause gets no more heavy frames
        // than it did before.
        let r = run(Pacing::DESKTOP, 100_000, 80 * MS, 3, 8);
        assert!(r.fps(8) <= 15.5, "{} fps", r.fps(8));
    }

    #[test]
    fn a_client_that_stops_acknowledging_stops_the_frames() {
        let r = run(Pacing::DESKTOP, 100_000, Duration::from_secs(3600), 3, 4);
        assert_eq!(r.frames.len(), 3, "only the frame window goes out");
    }

    #[test]
    fn the_byte_budget_throttles_a_link_that_would_take_more() {
        let budget = Pacing::DESKTOP.budget.unwrap();
        let burst = within(budget, BUDGET_BURST);
        // 400 kB per frame acknowledged after 5 ms would be 25 MB/s.
        let r = run(Pacing::DESKTOP, 400_000, 5 * MS, 3, 6);
        assert!(
            r.peak_bytes_per_second() <= budget + burst + 400_000,
            "{} B/s",
            r.peak_bytes_per_second()
        );
        assert!(r.bytes() <= 6 * budget + burst + 400_000, "{} B", r.bytes());
        // The budget is used, not undercut.
        assert!(r.bytes() >= 6 * budget - 400_000, "{} B", r.bytes());
        // Once the burst is spent, frames are spaced evenly (400 kB at
        // 8 MiB/s is one every 48 ms), not sent in bursts between pauses.
        let late: Vec<_> = r.frames.iter().filter(|t| t.as_secs() >= 1).collect();
        for pair in late.windows(2) {
            let gap = *pair[1] - *pair[0];
            assert!((47 * MS..=49 * MS).contains(&gap), "{gap:?}");
        }
    }

    #[test]
    fn frames_within_the_byte_budget_are_not_held_back() {
        // 100 kB at 60 fps is 6 MB/s, below the budget.
        let r = run(Pacing::DESKTOP, 100_000, 5 * MS, 3, 6);
        assert!(r.fps(6) > 58.0, "{} fps", r.fps(6));
    }

    #[test]
    fn the_byte_budget_never_holds_a_frame_for_longer_than_a_slow_client_would() {
        let t0 = Instant::now();
        let mut clock = FrameClock::new(Pacing::DESKTOP);
        // Frames far above the whole budget (full repaints).
        clock.frame_sent(1, t0, 64 * 1024 * 1024);
        clock.acked(1, t0 + 5 * MS);
        assert_eq!(
            clock.next_frame_at(Duration::ZERO),
            Some(t0 + MAX_LAG_COMPENSATION)
        );
        let t1 = t0 + MAX_LAG_COMPENSATION;
        clock.frame_sent(2, t1, 64 * 1024 * 1024);
        clock.acked(2, t1 + 5 * MS);
        assert_eq!(
            clock.next_frame_at(Duration::ZERO),
            Some(t1 + MAX_LAG_COMPENSATION)
        );
        // And they leave no debt behind for the small frames after them.
        let t2 = t1 + MAX_LAG_COMPENSATION;
        clock.frame_sent(3, t2, 1_000);
        clock.acked(3, t2 + 5 * MS);
        assert_eq!(clock.next_frame_at(Duration::ZERO), Some(t2 + 16 * MS));
    }

    #[test]
    fn a_quiet_period_restores_the_burst() {
        let budget = Pacing::DESKTOP.budget.unwrap();
        let t0 = Instant::now();
        let mut clock = FrameClock::new(Pacing::DESKTOP);
        clock.frame_sent(1, t0, budget / 2);
        clock.acked(1, t0 + 5 * MS);
        // A quarter of a second's budget is free, the other quarter is not.
        assert_eq!(clock.next_frame_at(Duration::ZERO), Some(t0 + 250 * MS));
        let t1 = t0 + Duration::from_secs(2);
        clock.frame_sent(2, t1, budget / 4);
        clock.acked(2, t1 + 5 * MS);
        assert_eq!(clock.next_frame_at(Duration::ZERO), Some(t1 + 16 * MS));
    }

    /// The case the pause after heavy frames exists for (docs/architecture.md,
    /// "SSH flood"): a command flooding the terminal produces a ~50 kB frame
    /// whenever one is allowed, and the client on a fast link acknowledges
    /// each within a few milliseconds. Without the pause this ran at the
    /// frame rate cap and sent 9.1 MiB in 8 s; with it, 5.5 MiB at 14 fps.
    #[test]
    fn a_terminal_flood_stays_at_15_fps_however_fast_the_client_is() {
        for ack_after in [MS, 5 * MS, 30 * MS] {
            let r = run(Pacing::TERMINAL, 50_000, ack_after, 2, 8);
            // 8 s / 66 ms = 121.2 intervals.
            assert!(r.frames.len() <= 122, "{} frames", r.frames.len());
            assert!(r.bytes() <= 122 * 50_000, "{} B", r.bytes());
            assert!(r.fps(8) > 14.0, "{} fps", r.fps(8));
        }
    }

    #[test]
    fn light_terminal_frames_are_capped_at_30_fps() {
        let r = run(Pacing::TERMINAL, 2_000, MS, 2, 4);
        assert!(r.fps(4) > 29.0 && r.fps(4) < 31.0, "{} fps", r.fps(4));
    }

    #[test]
    fn acknowledgements_of_unknown_or_older_frames_do_not_change_the_lag() {
        let t0 = Instant::now();
        let mut clock = FrameClock::new(Pacing::DESKTOP);
        clock.frame_sent(5, t0, 100);
        clock.frame_sent(6, t0, 100);
        assert_eq!(clock.acked(4, t0 + 300 * MS), None);
        assert_eq!(clock.unacked(), 2);
        // Acknowledging the later frame covers the earlier one.
        assert_eq!(clock.acked(6, t0 + 40 * MS), Some(40 * MS));
        assert_eq!(clock.unacked(), 0);
        assert_eq!(clock.lag(), 10 * MS);
    }

    /// Needs 16 ms per frame and 496 ms for every 25th, over a round trip
    /// of 4 ms: its replies take 20 ms, now and then 500 ms.
    fn stalls_now_and_then(n: usize) -> Duration {
        if n % 25 == 24 { 496 * MS } else { 16 * MS }
    }

    /// The clients the pacing is compared on (`table` prints the numbers).
    /// Each runs once as a stock client (the lag sets the interval, as
    /// before apply times were reported) and once reporting apply times.
    const SCENARIOS: [(&str, Pacing, usize, Client, usize); 6] = [
        (
            "desktop, fast client (3 ms replies)",
            Pacing::DESKTOP,
            20_000,
            Client {
                rtt: Duration::from_millis(2),
                apply: |_| MS,
                reports: false,
            },
            3,
        ),
        (
            "desktop, slow client (20 ms replies, 500 ms every 25th)",
            Pacing::DESKTOP,
            5_000,
            Client {
                rtt: Duration::from_millis(4),
                apply: stalls_now_and_then,
                reports: false,
            },
            3,
        ),
        (
            "desktop, long round trip (100 ms + 5 ms to apply)",
            Pacing::DESKTOP,
            5_000,
            Client {
                rtt: Duration::from_millis(100),
                apply: |_| 5 * MS,
                reports: false,
            },
            3,
        ),
        (
            "SSH flood, 50 kB frames, 1 ms replies",
            Pacing::TERMINAL,
            50_000,
            Client {
                rtt: Duration::ZERO,
                apply: |_| MS,
                reports: false,
            },
            2,
        ),
        (
            "SSH flood, 50 kB frames, 30 ms replies",
            Pacing::TERMINAL,
            50_000,
            Client {
                rtt: Duration::from_millis(20),
                apply: |_| 10 * MS,
                reports: false,
            },
            2,
        ),
        (
            "SSH, light frames, 1 ms replies",
            Pacing::TERMINAL,
            2_000,
            Client {
                rtt: Duration::ZERO,
                apply: |_| MS,
                reports: false,
            },
            2,
        ),
    ];

    const SECONDS: u32 = 8;

    /// The scenario's run with a stock client and with a reporting one.
    fn compare(name: &str) -> (Run, Run) {
        let &(_, pacing, bytes, client, window) =
            SCENARIOS.iter().find(|s| s.0 == name).expect("scenario");
        let stock = run_client(pacing, bytes, client.reporting(false), window, SECONDS);
        let reporting = run_client(pacing, bytes, client.reporting(true), window, SECONDS);
        (stock, reporting)
    }

    /// Prints the comparison of the scenarios:
    /// `cargo test -p molcajete-daemon display::pacing::tests::table -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn table() {
        println!(
            "| Client ({SECONDS} s of changes) | fps stock / reporting | most unacked | bytes sent |"
        );
        println!("|---|---|---|---|");
        for (name, ..) in SCENARIOS {
            let (stock, reporting) = compare(name);
            println!(
                "| {name} | {:.1} / {:.1} | {} / {} | {:.2} / {:.2} MB |",
                stock.fps(SECONDS),
                reporting.fps(SECONDS),
                stock.most_unacked,
                reporting.most_unacked,
                stock.bytes() as f64 / 1e6,
                reporting.bytes() as f64 / 1e6,
            );
        }
    }

    #[test]
    fn a_client_that_reports_apply_times_is_paced_by_them_not_by_its_replies() {
        let (stock, reporting) = compare("desktop, slow client (20 ms replies, 500 ms every 25th)");
        assert!(
            reporting.fps(SECONDS) > 1.5 * stock.fps(SECONDS),
            "{} fps, stock {} fps",
            reporting.fps(SECONDS),
            stock.fps(SECONDS)
        );
        assert!(reporting.most_unacked <= 3);
    }

    #[test]
    fn the_round_trip_is_left_to_the_frame_window() {
        let (stock, reporting) = compare("desktop, long round trip (100 ms + 5 ms to apply)");
        // The lag holds a stock client to one frame per round trip.
        assert!(stock.fps(SECONDS) < 10.5, "{} fps", stock.fps(SECONDS));
        // Three frames per round trip go out to a reporting client.
        assert!(
            reporting.fps(SECONDS) > 25.0,
            "{} fps",
            reporting.fps(SECONDS)
        );
        assert!(reporting.most_unacked <= 3);
    }

    #[test]
    fn a_fast_client_is_not_slowed_down_by_reporting() {
        let (stock, reporting) = compare("desktop, fast client (3 ms replies)");
        assert!(reporting.fps(SECONDS) >= stock.fps(SECONDS));
        assert!(
            reporting.fps(SECONDS) > 58.0,
            "{} fps",
            reporting.fps(SECONDS)
        );
    }

    /// The SSH flood case of docs/architecture.md must not send more.
    #[test]
    fn reporting_does_not_raise_the_bytes_of_a_terminal_flood() {
        for name in [
            "SSH flood, 50 kB frames, 1 ms replies",
            "SSH flood, 50 kB frames, 30 ms replies",
        ] {
            let (stock, reporting) = compare(name);
            assert!(reporting.bytes() <= stock.bytes(), "{name}");
            assert!(reporting.frames.len() <= 122, "{name}");
        }
        let (_, light) = compare("SSH, light frames, 1 ms replies");
        assert!(light.fps(SECONDS) < 31.0, "{} fps", light.fps(SECONDS));
    }

    fn reply(at: Instant, apply: Duration) -> SyncReply {
        SyncReply {
            at,
            apply: Some(apply),
        }
    }

    #[test]
    fn one_slow_frame_does_not_hold_back_the_next() {
        let t0 = Instant::now();
        let mut clock = FrameClock::new(Pacing::DESKTOP);
        let mut t = t0;
        for (ts, apply) in [20, 20, 20, 500].into_iter().enumerate() {
            clock.frame_sent(ts as i64, t, 2_000);
            clock.acked(ts as i64, reply(t + (apply + 5) * MS, apply * MS));
            t += 30 * MS;
        }
        // The median of 20, 20, 20 and 500 ms.
        let last = t - 30 * MS;
        assert_eq!(clock.next_frame_at(Duration::ZERO), Some(last + 20 * MS));
        // The stock path would have taken a quarter of the 500 ms.
        assert!(clock.lag() > 100 * MS);
    }

    #[test]
    fn reported_apply_times_are_forgotten_after_a_pause() {
        let t0 = Instant::now();
        let mut clock = FrameClock::new(Pacing::DESKTOP);
        for ts in 0..4 {
            let t = t0 + 400 * MS * ts;
            clock.frame_sent(i64::from(ts), t, 2_000);
            clock.acked(i64::from(ts), reply(t + 300 * MS, 300 * MS));
        }
        let last = t0 + 1200 * MS;
        assert_eq!(clock.next_frame_at(Duration::ZERO), Some(last + 300 * MS));
        // The screen stood still for a while; the next frames are not
        // spaced by what the client needed before the pause.
        let t = last + 2 * APPLY_MEMORY;
        clock.frame_sent(4, t, 2_000);
        assert_eq!(clock.next_frame_at(Duration::ZERO), Some(t + 16 * MS));
    }

    #[test]
    fn heavy_frames_still_wait_for_their_acknowledgement() {
        let t0 = Instant::now();
        let mut clock = FrameClock::new(Pacing::DESKTOP);
        clock.frame_sent(1, t0, 2_000);
        clock.acked(1, reply(t0 + 10 * MS, MS));
        clock.frame_sent(2, t0 + 16 * MS, 100_000);
        assert_eq!(
            clock.next_frame_at(Duration::ZERO),
            Some(t0 + 16 * MS + LARGE_FRAME_INTERVAL)
        );
    }
}

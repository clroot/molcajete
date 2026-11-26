//! Whether the server can change its display during the session, as far as
//! the client needs to know.
//!
//! The Display Control channel ([MS-RDPEDISP]) is a dynamic channel the
//! server opens when it supports it, at a time of its choosing. A server
//! without it says nothing, so "not open yet" and "never" look the same; the
//! channel is taken as missing once the server has had a while to open it.
//! If it opens after all, the client is told again.

use std::time::{Duration, Instant};

use molcajete_protocol::InstructionBuilder;

/// Time after the session became active by which servers open the channel.
pub const WAIT: Duration = Duration::from_secs(10);

/// Instruction telling the client whether size and scale changes can be
/// applied during the session (`1`) or not (`0`). Stock guacd never sends
/// it, and clients that do not know it ignore it.
pub const OPCODE: &str = "display-control";

pub struct DisplayControlStatus {
    /// The dynamic channel transport (drdynvc) was joined at connect time.
    dynamic_channels: bool,
    give_up_at: Instant,
    reported: Option<bool>,
}

impl DisplayControlStatus {
    pub fn new(dynamic_channels: bool, active_since: Instant) -> Self {
        Self {
            dynamic_channels,
            give_up_at: active_since + WAIT,
            reported: None,
        }
    }

    /// What the client was last told, for a client that attaches later.
    pub fn reported(&self) -> Option<bool> {
        self.reported
    }

    /// Whether the client was told the channel is missing.
    pub fn is_unavailable(&self) -> bool {
        self.reported == Some(false)
    }

    /// Takes the channel's state (`ActiveStage::display_control_ready`) and
    /// returns what to tell the client, when that changed.
    pub fn update(&mut self, ready: Option<bool>, now: Instant) -> Option<bool> {
        let available = match ready {
            Some(true) => true,
            _ if !self.dynamic_channels || ready.is_none() || now >= self.give_up_at => false,
            // Not open yet, and there is still time.
            _ => return None,
        };
        (self.reported != Some(available)).then(|| {
            self.reported = Some(available);
            available
        })
    }
}

pub fn instruction(available: bool) -> String {
    InstructionBuilder::new(OPCODE)
        .arg(u8::from(available))
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_the_channel_before_calling_it_missing() {
        let t0 = Instant::now();
        let mut status = DisplayControlStatus::new(true, t0);
        assert_eq!(status.update(Some(false), t0), None);
        assert_eq!(status.update(Some(false), t0 + WAIT / 2), None);
        assert!(!status.is_unavailable());
        assert_eq!(status.update(Some(false), t0 + WAIT), Some(false));
        assert!(status.is_unavailable());
        // Said once.
        assert_eq!(status.update(Some(false), t0 + WAIT * 2), None);
        // A late channel is reported, once.
        assert_eq!(status.update(Some(true), t0 + WAIT * 3), Some(true));
        assert_eq!(status.update(Some(true), t0 + WAIT * 4), None);
        assert!(!status.is_unavailable());
    }

    #[test]
    fn reports_an_open_channel_right_away() {
        let t0 = Instant::now();
        let mut status = DisplayControlStatus::new(true, t0);
        assert_eq!(status.update(Some(true), t0), Some(true));
    }

    #[test]
    fn without_dynamic_channels_it_is_missing_at_once() {
        let t0 = Instant::now();
        let mut status = DisplayControlStatus::new(false, t0);
        assert_eq!(status.update(Some(false), t0), Some(false));
        let mut unregistered = DisplayControlStatus::new(true, t0);
        assert_eq!(unregistered.update(None, t0), Some(false));
    }

    #[test]
    fn instruction_format() {
        assert_eq!(instruction(false), "15.display-control,1.0;");
        assert_eq!(instruction(true), "15.display-control,1.1;");
    }
}

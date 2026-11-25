//! Apply-time reports: an extension of `sync`, spoken only between
//! molcajeted and molcajete-client, through which the client tells the
//! server how long it needed to apply a frame.
//!
//! The time from sending `sync` to its reply covers the network both ways
//! as well as the client's work. A server that paces frames by that whole
//! time sends fewer frames the longer the round trip, although a window of
//! unacknowledged frames already keeps it from running ahead of the client.
//! With the client's own share it can pace by what the client needs and
//! leave the round trip to that window.
//!
//! ```text
//! server -> client   sync,<timestamp>,<frames>,apply-us;
//! client -> server   sync,<timestamp>,<frames>,<microseconds>;
//! ```
//!
//! - The server asks with [`REQUEST`] as the third argument of its `sync`.
//!   Stock clients read the timestamp and the frame count and ignore the
//!   rest, and reply `sync,<timestamp>` as before. Stock guacd never asks.
//! - A client that knows the extension answers a `sync` that asked with the
//!   frame count it received and the time it spent on the frame, in whole
//!   microseconds: from the moment the frame was complete and the frames
//!   before it had been applied, to the moment it was applied. Waiting
//!   behind earlier frames does not count, waiting for its images to decode
//!   does. A `sync` that did not ask is answered as by a stock client.
//! - A server reads the value only from the third argument of a reply. A
//!   reply without it, or with a value that is not a number, is a plain
//!   acknowledgement.

use std::time::Duration;

use crate::InstructionBuilder;

/// Third argument of a server's `sync` that asks for the apply time.
pub const REQUEST: &str = "apply-us";

/// Longest apply time a reply can report; larger values are cut to it.
pub const MAX: Duration = Duration::from_secs(60);

/// The server's `sync` ending a frame, asking for the apply time.
pub fn sync_request(timestamp: i64, frames: u32) -> String {
    InstructionBuilder::new("sync")
        .arg(timestamp)
        .arg(frames)
        .str_arg(REQUEST)
        .finish()
}

/// Whether the third argument of a server's `sync` asks for the apply time.
pub fn is_request(arg: &str) -> bool {
    arg == REQUEST
}

/// The client's reply to a `sync` that asked: the frame with `timestamp`
/// (`frames` as received) took `apply` to apply.
pub fn sync_reply(timestamp: i64, frames: u32, apply: Duration) -> String {
    let micros = apply.min(MAX).as_micros() as u64;
    InstructionBuilder::new("sync")
        .arg(timestamp)
        .arg(frames)
        .arg(micros)
        .finish()
}

/// The apply time in the third argument of a client's `sync` reply.
pub fn parse_reply(arg: &str) -> Option<Duration> {
    let micros: u64 = arg.trim().parse().ok()?;
    Some(Duration::from_micros(micros).min(MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_all;
    use crate::server::ServerInstruction;

    #[test]
    fn a_request_is_a_sync_with_one_more_argument() {
        let text = sync_request(1_700_000_000_123, 1);
        assert_eq!(text, "4.sync,13.1700000000123,1.1,8.apply-us;");
        let ins = &parse_all(&text).unwrap()[0];
        // What a stock client reads of it is unchanged.
        assert_eq!(
            ServerInstruction::decode(&ins).unwrap(),
            ServerInstruction::Sync {
                timestamp: 1_700_000_000_123,
                frames: 1,
                apply_time: true,
            }
        );
        assert!(is_request(ins.arg(2)));
    }

    #[test]
    fn a_plain_sync_does_not_ask() {
        for text in ["4.sync,2.42;", "4.sync,2.42,1.3;", "4.sync,2.42,1.3,0.;"] {
            let ins = &parse_all(text).unwrap()[0];
            assert!(!is_request(ins.arg(2)), "{text}");
            assert!(matches!(
                ServerInstruction::decode(&ins).unwrap(),
                ServerInstruction::Sync {
                    timestamp: 42,
                    apply_time: false,
                    ..
                }
            ));
        }
    }

    #[test]
    fn replies_round_trip_in_microseconds() {
        let text = sync_reply(42, 1, Duration::from_micros(16_250));
        assert_eq!(text, "4.sync,2.42,1.1,5.16250;");
        let ins = &parse_all(&text).unwrap()[0];
        assert_eq!(ins.opcode, "sync");
        assert_eq!(ins.arg(0), "42");
        assert_eq!(parse_reply(ins.arg(2)), Some(Duration::from_micros(16_250)));
        // Sub-microsecond parts are dropped, long times are cut.
        let ins = &parse_all(&sync_reply(1, 1, Duration::from_nanos(999))).unwrap()[0];
        assert_eq!(parse_reply(ins.arg(2)), Some(Duration::ZERO));
        let ins = &parse_all(&sync_reply(1, 1, Duration::from_secs(3600))).unwrap()[0];
        assert_eq!(parse_reply(ins.arg(2)), Some(MAX));
    }

    #[test]
    fn stock_replies_carry_no_apply_time() {
        for text in ["4.sync,2.42;", "4.sync,2.42,1.1;", "4.sync,2.42,1.1,0.;"] {
            let ins = &parse_all(text).unwrap()[0];
            assert_eq!(parse_reply(ins.arg(2)), None, "{text}");
        }
        for junk in ["-5", "1.5", "apply-us", "99999999999999999999999"] {
            assert_eq!(parse_reply(junk), None, "{junk}");
        }
        assert_eq!(
            parse_reply(&u64::MAX.to_string()),
            Some(MAX),
            "huge but valid"
        );
    }
}

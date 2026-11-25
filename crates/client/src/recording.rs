//! Timing of session recordings (Guacamole's format, as written by guacd
//! and the molcajete gateway) for playback.
//!
//! A recording is the server's instruction stream with the user's `mouse`
//! and `key` instructions in between, which carry the time they happened
//! as their last argument. Frames end with `sync`, whose first argument is
//! the server's time. The index lists, for every instruction with a time,
//! where it ends in the file: playing up to a moment is feeding the client
//! everything up to the last such point at or before it.

use molcajete_protocol::{Limits, Parser};

/// A place in the recording that has a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    /// Offset just after the instruction.
    pub end: usize,
    /// Milliseconds since the first point of the recording. Never less than
    /// the time of the point before (clocks of the daemon and the gateway
    /// may disagree a little).
    pub time: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
    pub points: Vec<Point>,
    /// The absolute time (Unix milliseconds) of the first point.
    pub start: i64,
    /// Offset of the end of the last complete instruction; anything after
    /// it is a cut-off instruction of a recording still being written.
    pub complete: usize,
}

impl Index {
    /// Indexes `data`. Parsing stops at the first malformed instruction,
    /// keeping what came before it.
    pub fn build(data: &[u8]) -> Self {
        let mut parser = Parser::with_limits(Limits::LENIENT);
        let mut index = Index::default();
        let mut offset = 0;
        let mut first = None;
        let mut last = 0;
        // Fed in pieces so that the parser's buffer stays small.
        for chunk in data.chunks(1 << 20) {
            parser.feed(chunk);
            loop {
                let (len, time) = match parser.next_raw() {
                    Ok(Some(raw)) => (raw.as_bytes().len(), time_of(&raw)),
                    Ok(None) => break,
                    Err(_) => return index,
                };
                offset += len;
                index.complete = offset;
                let Some(time) = time else { continue };
                let start = *first.get_or_insert(time);
                last = (time - start).max(last);
                index.points.push(Point {
                    end: offset,
                    time: last,
                });
            }
        }
        index.start = first.unwrap_or(0);
        index
    }

    /// Length of the recording in milliseconds.
    pub fn duration(&self) -> i64 {
        self.points.last().map_or(0, |p| p.time)
    }

    /// How many points lie at or before `time`.
    pub fn points_until(&self, time: i64) -> usize {
        self.points.partition_point(|p| p.time <= time)
    }
}

/// The time an instruction carries: `sync` its first argument, the
/// user's `mouse` (four arguments) and `key` (three) their last.
fn time_of(raw: &molcajete_protocol::RawInstruction<'_>) -> Option<i64> {
    let arg = match (raw.opcode(), raw.arg_count()) {
        ("sync", n) if n >= 1 => raw.arg(0),
        ("mouse", 4) => raw.arg(3),
        ("key", 3) => raw.arg(2),
        _ => return None,
    };
    arg.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_syncs_and_user_input() {
        let text = "4.size,1.0,3.640,3.480;4.sync,4.1000;\
                    5.mouse,2.10,2.20,1.0,4.1250;3.key,2.97,1.1,4.1300;\
                    5.mouse,2.10,2.20;4.sync,4.1200,1.1;4.sync,4.2000;3.nop;4.sy";
        let index = Index::build(text.as_bytes());
        assert_eq!(index.start, 1000);
        let times: Vec<_> = index.points.iter().map(|p| p.time).collect();
        // A sync a little behind the mouse before it keeps the order.
        assert_eq!(times, [0, 250, 300, 300, 1000]);
        let ends: Vec<_> = index.points.iter().map(|p| &text[..p.end]).collect();
        assert!(ends[0].ends_with("4.sync,4.1000;"));
        assert!(ends[1].ends_with("4.1250;"));
        assert!(ends[4].ends_with("4.sync,4.2000;"));
        assert_eq!(index.complete, text.len() - "4.sy".len());
        assert_eq!(index.duration(), 1000);
        assert_eq!(index.points_until(-1), 0);
        assert_eq!(index.points_until(0), 1);
        assert_eq!(index.points_until(300), 4);
        assert_eq!(index.points_until(999), 4);
        assert_eq!(index.points_until(5000), 5);
    }

    #[test]
    fn stops_at_damage_and_survives_empty_input() {
        let index = Index::build(b"4.sync,1.5;x.broken;4.sync,2.10;");
        assert_eq!(index.points.len(), 1);
        assert_eq!(Index::build(b""), Index::default());
    }
}

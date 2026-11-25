//! When a frame starts arriving, for the frame-arrival time (from a frame's
//! first instruction to its `sync`).
//!
//! Not every instruction after a `sync` starts the next frame. A cursor
//! change is sent as soon as it happens and takes effect with the next
//! frame: its image is drawn into an off-screen buffer and the `cursor`
//! instruction then reads it from there. Starting the clock at those
//! instructions would count the whole idle time until the next frame as
//! arrival. The clock therefore starts at the first instruction that draws
//! on a visible layer; instructions drawing into buffers before it count
//! only when that frame turns out to use them, not when a `cursor`
//! instruction takes them.

use super::{DrawOp, Op};

/// What an instruction means for the arrival clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Draws on the screen (the default layer or a visible layer).
    Visible,
    /// Draws into an off-screen buffer (negative layer index).
    Buffer,
    /// Sets the cursor from a buffer.
    Cursor,
    /// The end of the frame.
    Sync,
}

fn layer_kind(layer: i32) -> Kind {
    if layer < 0 {
        Kind::Buffer
    } else {
        Kind::Visible
    }
}

fn kind(op: &Op) -> Kind {
    match op {
        Op::Sync { .. } => Kind::Sync,
        Op::Image { layer, .. } => layer_kind(*layer),
        Op::Draw(op) => match op {
            DrawOp::Cursor { .. } => Kind::Cursor,
            DrawOp::Copy { dst, .. } | DrawOp::Transfer { dst, .. } => layer_kind(*dst),
            DrawOp::Arc { layer, .. }
            | DrawOp::Cfill { layer, .. }
            | DrawOp::Clip { layer }
            | DrawOp::Close { layer }
            | DrawOp::Cstroke { layer, .. }
            | DrawOp::Curve { layer, .. }
            | DrawOp::Dispose { layer }
            | DrawOp::Distort { layer, .. }
            | DrawOp::Identity { layer }
            | DrawOp::Lfill { layer, .. }
            | DrawOp::Line { layer, .. }
            | DrawOp::Lstroke { layer, .. }
            | DrawOp::Move { layer, .. }
            | DrawOp::Pop { layer }
            | DrawOp::Push { layer }
            | DrawOp::Rect { layer, .. }
            | DrawOp::Reset { layer }
            | DrawOp::MiterLimit { layer, .. }
            | DrawOp::Shade { layer, .. }
            | DrawOp::Size { layer, .. }
            | DrawOp::Start { layer, .. }
            | DrawOp::Transform { layer, .. } => layer_kind(*layer),
        },
    }
}

/// Start of the frame being received.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Arrival {
    /// When the frame's first visible instruction (or the buffer drawing
    /// before it) arrived.
    started: Option<f64>,
    /// When the first buffer drawing since the last `sync` or `cursor`
    /// arrived, while no visible instruction has.
    buffered: Option<f64>,
}

impl Arrival {
    /// Notes an instruction other than `sync`, arriving at `now()`.
    pub fn op(&mut self, op: &Op, now: impl FnOnce() -> f64) {
        match kind(op) {
            Kind::Visible => {
                if self.started.is_none() {
                    self.started = Some(self.buffered.take().unwrap_or_else(now));
                }
            }
            Kind::Buffer => {
                if self.started.is_none() && self.buffered.is_none() {
                    self.buffered = Some(now());
                }
            }
            // The buffer drawing so far was the cursor image.
            Kind::Cursor => self.buffered = None,
            Kind::Sync => {}
        }
    }

    /// Ends the frame at its `sync`, received at `received_at`, and returns
    /// how long it took to arrive (0 for a frame with nothing visible).
    pub fn sync(&mut self, received_at: f64) -> f64 {
        self.buffered = None;
        self.started
            .take()
            .map_or(0.0, |started| received_at - started)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Rect;
    use molcajete_protocol::CompositeMode;

    fn rect(layer: i32) -> Op {
        Op::Draw(DrawOp::Rect {
            layer,
            rect: Rect::new(0, 0, 8, 8),
        })
    }

    fn size(layer: i32) -> Op {
        Op::Draw(DrawOp::Size {
            layer,
            width: 32,
            height: 32,
        })
    }

    fn copy(src: i32, dst: i32) -> Op {
        Op::Draw(DrawOp::Copy {
            src,
            rect: Rect::new(0, 0, 8, 8),
            mode: CompositeMode::OVER,
            dst,
            x: 0,
            y: 0,
        })
    }

    fn image(layer: i32) -> Op {
        Op::Image {
            layer,
            mode: CompositeMode::SRC,
            x: 0,
            y: 0,
            slot: super::super::ImageSlot::pending(),
        }
    }

    fn cursor() -> Op {
        Op::Draw(DrawOp::Cursor {
            hotspot_x: 0,
            hotspot_y: 0,
            src: -1,
            rect: Rect::new(0, 0, 32, 32),
        })
    }

    /// Feeds `(time, op)` pairs, then the `sync` at `sync_at`.
    fn frame(arrival: &mut Arrival, ops: &[(f64, Op)], sync_at: f64) -> f64 {
        for (at, op) in ops {
            arrival.op(op, || *at);
        }
        arrival.sync(sync_at)
    }

    #[test]
    fn starts_at_the_first_visible_instruction() {
        let mut arrival = Arrival::default();
        let ms = frame(&mut arrival, &[(100.0, rect(0)), (104.0, rect(1))], 110.0);
        assert_eq!(ms, 10.0);
    }

    #[test]
    fn a_cursor_change_between_frames_does_not_start_one() {
        // The daemon's cursor change: size and image of buffer -1, then
        // `cursor`. The frame that follows a second later takes 5 ms.
        let mut arrival = Arrival::default();
        let ms = frame(
            &mut arrival,
            &[
                (0.0, size(-1)),
                (0.0, image(-1)),
                (0.0, cursor()),
                (1000.0, image(0)),
            ],
            1005.0,
        );
        assert_eq!(ms, 5.0);
    }

    #[test]
    fn a_frame_with_only_a_cursor_change_takes_no_time() {
        let mut arrival = Arrival::default();
        let ms = frame(&mut arrival, &[(0.0, size(-1)), (0.0, cursor())], 1000.0);
        assert_eq!(ms, 0.0);
    }

    #[test]
    fn buffer_drawing_used_by_the_frame_counts() {
        // A glyph cache filled before it is copied to the screen.
        let mut arrival = Arrival::default();
        let ms = frame(
            &mut arrival,
            &[(10.0, rect(-3)), (12.0, rect(-3)), (15.0, copy(-3, 0))],
            20.0,
        );
        assert_eq!(ms, 10.0);
    }

    #[test]
    fn a_cursor_change_inside_a_frame_keeps_its_start() {
        let mut arrival = Arrival::default();
        let ms = frame(
            &mut arrival,
            &[
                (10.0, rect(0)),
                (11.0, size(-1)),
                (11.0, cursor()),
                (12.0, rect(0)),
            ],
            20.0,
        );
        assert_eq!(ms, 10.0);
    }

    #[test]
    fn the_next_frame_starts_afresh() {
        let mut arrival = Arrival::default();
        frame(&mut arrival, &[(0.0, rect(-5))], 1.0);
        let ms = frame(&mut arrival, &[(500.0, rect(0))], 502.0);
        assert_eq!(ms, 2.0);
    }

    #[test]
    fn an_empty_frame_takes_no_time() {
        let mut arrival = Arrival::default();
        assert_eq!(frame(&mut arrival, &[], 10.0), 0.0);
    }
}

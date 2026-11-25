//! Box-drawing (U+2500–U+257F) and block-element (U+2580–U+259F)
//! characters drawn procedurally so that lines join seamlessly across
//! cells regardless of the font's metrics (tmux borders, `mc`, `htop`…).

use super::font::Mask;

/// Weight of one arm of a box-drawing character.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arm {
    None,
    Light,
    Heavy,
    Double,
}

/// Arms in up, right, down, left order.
fn arms(c: char) -> Option<[Arm; 4]> {
    use Arm::{Double as D, Heavy as H, Light as L, None as N};
    Some(match c {
        '─' => [N, L, N, L],
        '━' => [N, H, N, H],
        '│' => [L, N, L, N],
        '┃' => [H, N, H, N],
        '┌' | '╭' => [N, L, L, N],
        '┏' => [N, H, H, N],
        '┐' | '╮' => [N, N, L, L],
        '┓' => [N, N, H, H],
        '└' | '╰' => [L, L, N, N],
        '┗' => [H, H, N, N],
        '┘' | '╯' => [L, N, N, L],
        '┛' => [H, N, N, H],
        '├' => [L, L, L, N],
        '┣' => [H, H, H, N],
        '┤' => [L, N, L, L],
        '┫' => [H, N, H, H],
        '┬' => [N, L, L, L],
        '┳' => [N, H, H, H],
        '┴' => [L, L, N, L],
        '┻' => [H, H, N, H],
        '┼' => [L, L, L, L],
        '╋' => [H, H, H, H],
        '═' => [N, D, N, D],
        '║' => [D, N, D, N],
        '╔' => [N, D, D, N],
        '╗' => [N, N, D, D],
        '╚' => [D, D, N, N],
        '╝' => [D, N, N, D],
        '╠' => [D, D, D, N],
        '╣' => [D, N, D, D],
        '╦' => [N, D, D, D],
        '╩' => [D, D, N, D],
        '╬' => [D, D, D, D],
        '╴' => [N, N, N, L],
        '╵' => [L, N, N, N],
        '╶' => [N, L, N, N],
        '╷' => [N, N, L, N],
        '╸' => [N, N, N, H],
        '╹' => [H, N, N, N],
        '╺' => [N, H, N, N],
        '╻' => [N, N, H, N],
        '╼' => [N, H, N, L],
        '╽' => [L, N, H, N],
        '╾' => [N, L, N, H],
        '╿' => [H, N, L, N],
        _ => return None,
    })
}

/// Whether `c` is drawn by [`render`].
pub fn handles(c: char) -> bool {
    matches!(c, '\u{2580}'..='\u{259F}') || arms(c).is_some()
}

/// Draws `c` into a `width`×`height` coverage mask, or returns `None` when
/// the character is not handled here.
pub fn render(c: char, width: u32, height: u32) -> Option<Mask> {
    let mut mask = Mask::new(width, height);
    if let Some(arms) = arms(c) {
        draw_lines(&mut mask, arms);
    } else {
        draw_block(&mut mask, c)?;
    }
    Some(mask)
}

fn draw_lines(mask: &mut Mask, arms: [Arm; 4]) {
    let (w, h) = (mask.width as i32, mask.height as i32);
    let light = (w.min(h) / 10).max(1);
    let heavy = light * 2 + 1;
    let gap = light.max(w.min(h) / 8).max(1);
    let (cx, cy) = (w / 2, h / 2);
    let [up, right, down, left] = arms;

    // Single (light/heavy) lines: every arm covers the centre box, whose
    // size is the thickest stroke crossing it, so perpendicular arms meet
    // without gaps and collinear arms overlap.
    let single = |arm: Arm| matches!(arm, Arm::Light | Arm::Heavy);
    let thickness = |arm: Arm| if arm == Arm::Heavy { heavy } else { light };
    let widest = |a: Arm, b: Arm| {
        [a, b]
            .into_iter()
            .filter(|&arm| single(arm))
            .map(thickness)
            .max()
            .unwrap_or(0)
    };
    let (vertical_t, horizontal_t) = (widest(up, down), widest(left, right));
    // Start and end of the centre box along one axis.
    let centre = |c: i32, t: i32| {
        let start = c - t / 2;
        (start, start + t)
    };
    for (arm, toward_end) in [(left, false), (right, true)] {
        if single(arm) {
            let t = thickness(arm);
            let (box_start, box_end) = centre(cx, vertical_t.max(t));
            let y = cy - t / 2;
            if toward_end {
                mask.fill(box_start, y, w - box_start, t);
            } else {
                mask.fill(0, y, box_end, t);
            }
        }
    }
    for (arm, toward_end) in [(up, false), (down, true)] {
        if single(arm) {
            let t = thickness(arm);
            let (box_start, box_end) = centre(cy, horizontal_t.max(t));
            let x = cx - t / 2;
            if toward_end {
                mask.fill(x, box_start, t, h - box_start);
            } else {
                mask.fill(x, 0, t, box_end);
            }
        }
    }

    // Double lines: two parallel strokes at ±gap. Each stroke stops at the
    // inner stroke of a perpendicular arm on its side, extends to the outer
    // stroke when only the opposite side has one, and meets its twin in the
    // middle when the line goes straight through.
    let t = light;
    let double = |arm: Arm| arm == Arm::Double;
    // Offset from the centre at which a stroke of an arm starts, given the
    // perpendicular arm on the stroke's side, the perpendicular arm on the
    // other side, and the arm opposite this one.
    let start = |side: Arm, other: Arm, opposite: Arm| -> i32 {
        if double(side) {
            gap
        } else if double(other) {
            -gap
        } else if double(opposite) {
            0
        } else {
            -gap
        }
    };
    if double(right) {
        for (s, side, other) in [(-1, up, down), (1, down, up)] {
            let x = cx + start(side, other, left);
            mask.fill(x, cy + s * gap - t / 2, w - x, t);
        }
    }
    if double(left) {
        for (s, side, other) in [(-1, up, down), (1, down, up)] {
            let x = cx - start(side, other, right);
            mask.fill(0, cy + s * gap - t / 2, x + t, t);
        }
    }
    if double(down) {
        for (s, side, other) in [(-1, left, right), (1, right, left)] {
            let y = cy + start(side, other, up);
            mask.fill(cx + s * gap - t / 2, y, t, h - y);
        }
    }
    if double(up) {
        for (s, side, other) in [(-1, left, right), (1, right, left)] {
            let y = cy - start(side, other, down);
            mask.fill(cx + s * gap - t / 2, 0, t, y + t);
        }
    }
}

/// Block elements and shades.
fn draw_block(mask: &mut Mask, c: char) -> Option<()> {
    let (w, h) = (mask.width as i32, mask.height as i32);
    let eighth_h = |n: i32| (h * n + 4) / 8;
    let eighth_w = |n: i32| (w * n + 4) / 8;
    let (hw, hh) = (w / 2, h / 2);
    match c {
        '▀' => mask.fill(0, 0, w, hh),
        '\u{2581}'..='\u{2588}' => {
            let n = c as i32 - 0x2580;
            let top = h - eighth_h(n);
            mask.fill(0, top, w, h - top);
        }
        '\u{2589}'..='\u{258F}' => {
            let n = 8 - (c as i32 - 0x2588);
            mask.fill(0, 0, eighth_w(n), h);
        }
        '▐' => mask.fill(hw, 0, w - hw, h),
        '░' => mask.data.fill(64),
        '▒' => mask.data.fill(128),
        '▓' => mask.data.fill(192),
        '▔' => mask.fill(0, 0, w, eighth_h(1)),
        '▕' => {
            let x = w - eighth_w(1);
            mask.fill(x, 0, w - x, h);
        }
        '\u{2596}'..='\u{259F}' => {
            // Quadrants as bits: upper-left, upper-right, lower-left,
            // lower-right.
            const QUADRANTS: [u8; 10] = [
                0b0010, 0b0001, 0b1000, 0b1011, 0b1001, 0b1110, 0b1101, 0b0100, 0b0110, 0b0111,
            ];
            let bits = QUADRANTS[(c as u32 - 0x2596) as usize];
            if bits & 0b1000 != 0 {
                mask.fill(0, 0, hw, hh);
            }
            if bits & 0b0100 != 0 {
                mask.fill(hw, 0, w - hw, hh);
            }
            if bits & 0b0010 != 0 {
                mask.fill(0, hh, hw, h - hh);
            }
            if bits & 0b0001 != 0 {
                mask.fill(hw, hh, w - hw, h - hh);
            }
        }
        _ => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn covered(mask: &Mask, x: u32, y: u32) -> bool {
        mask.data[(y * mask.width + x) as usize] > 0
    }

    #[test]
    fn horizontal_line_spans_the_cell() {
        let m = render('─', 10, 20).unwrap();
        assert!(covered(&m, 0, 10) && covered(&m, 9, 10));
        assert!(!covered(&m, 5, 0) && !covered(&m, 5, 19));
    }

    #[test]
    fn corner_joins_both_arms() {
        let m = render('┌', 10, 20).unwrap();
        // Right arm to the border, down arm to the bottom, nothing up/left.
        assert!(covered(&m, 9, 10) && covered(&m, 5, 19));
        assert!(!covered(&m, 0, 10) && !covered(&m, 5, 0));
        assert!(covered(&m, 5, 10));
    }

    #[test]
    fn double_lines_are_two_strokes() {
        let m = render('║', 10, 20).unwrap();
        let row: Vec<bool> = (0..10).map(|x| covered(&m, x, 10)).collect();
        let strokes = row.windows(2).filter(|p| !p[0] && p[1]).count() + row[0] as usize;
        assert_eq!(strokes, 2, "{row:?}");
    }

    #[test]
    fn blocks_and_shades() {
        let m = render('█', 8, 16).unwrap();
        assert!(m.data.iter().all(|&a| a == 255));
        let m = render('▄', 8, 16).unwrap();
        assert!(!covered(&m, 0, 7) && covered(&m, 0, 8));
        let m = render('▌', 8, 16).unwrap();
        assert!(covered(&m, 3, 0) && !covered(&m, 4, 0));
        let m = render('▒', 8, 16).unwrap();
        assert!(m.data.iter().all(|&a| a == 128));
        let m = render('▚', 8, 16).unwrap();
        assert!(covered(&m, 0, 0) && !covered(&m, 7, 0) && covered(&m, 7, 15));
        assert!(render('a', 8, 16).is_none());
        assert!(handles('┼') && handles('▓') && !handles('x'));
    }
}

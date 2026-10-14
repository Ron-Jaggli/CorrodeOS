//! The built-in typeface, as strokes.
//!
//! Each glyph is the path a pen takes, not an outline to fill: the rasteriser
//! gives the path its thickness. That is what lets one small table serve
//! every size and every weight, and it is why the whole of printable ASCII
//! fits in a few kilobytes of text a person can read and edit.
//!
//! Coordinates are whole units on a grid with the origin on the baseline at
//! the glyph's left edge, x to the right and y **up**:
//!
//! ```text
//!  17  ascender (b d f h k l)
//!  16  cap height, digits
//!  12  x-height
//!   0  baseline
//!  -5  descender (g j p q y)
//! ```
//!
//! A path is space-separated commands:
//!
//! * `Mx,y` -- lift the pen and put it down at a point;
//! * `Lx,y` -- draw a straight line to a point;
//! * `Acx,cy x,y` -- draw a quarter turn to `x,y`, rounding the corner at
//!   `cx,cy`. When the corner is square to both ends this is an exact quarter
//!   of an ellipse, so four of them make a true circle.
//!
//! A line from a point to itself is a dot.

/// Units between one glyph's box and the next.
pub(crate) const SPACING: i32 = 5;

pub(crate) struct Glyph {
    /// Width of the box the path is drawn in. The advance is this plus
    /// [`SPACING`].
    pub width: i32,
    pub path: &'static str,
}

const fn g(width: i32, path: &'static str) -> Glyph {
    Glyph { width, path }
}

/// Drawn for anything that is not printable ASCII.
pub(crate) static MISSING: Glyph = g(8, "M0,0 L0,16 L8,16 L8,0 L0,0");

/// `' '` to `'~'`, in order.
static GLYPHS: [Glyph; 95] = [
    g(2, ""),
    g(0, "M0,16 L0,5 M0,0 L0,0"),
    g(4, "M0,16 L0,12 M4,16 L4,12"),
    g(12, "M4,16 L2,0 M10,16 L8,0 M0,11 L12,11 M0,5 L12,5"),
    g(10, "M10,12 A10,16 5,16 A0,16 0,12 A0,8 5,8 A10,8 10,4 A10,0 5,0 A0,0 0,4 M5,18 L5,-2"),
    g(14, "M12,16 L2,0 M3,16 A0,16 0,13 A0,10 3,10 A6,10 6,13 A6,16 3,16 M11,6 A8,6 8,3 A8,0 11,0 A14,0 14,3 A14,6 11,6"),
    g(12, "M12,0 L2,11 A0,13 1,15 A2,16 4,16 A7,16 7,13 A7,11 5,9 L2,7 A0,5 0,4 A0,0 4,0 A8,0 12,6"),
    g(0, "M0,16 L0,12"),
    g(4, "M4,18 A0,14 0,7 A0,0 4,-4"),
    g(4, "M0,18 A4,14 4,7 A4,0 0,-4"),
    g(8, "M4,16 L4,8 M0,14 L8,10 M8,14 L0,10"),
    g(10, "M5,12 L5,2 M0,7 L10,7"),
    g(2, "M2,0 L0,-4"),
    g(7, "M0,7 L7,7"),
    g(0, "M0,0 L0,0"),
    g(7, "M7,18 L0,-2"),
    // 0-9: all one width, so columns of figures line up.
    g(10, "M5,0 A0,0 0,5 L0,11 A0,16 5,16 A10,16 10,11 L10,5 A10,0 5,0"),
    g(10, "M2,12 L6,16 L6,0"),
    g(10, "M0,12 A0,16 5,16 A10,16 10,12 A10,10 8,8 L0,0 L10,0"),
    g(10, "M0,13 A0,16 5,16 A10,16 10,12 A10,8 5,8 L3,8 M5,8 A10,8 10,4 A10,0 5,0 A0,0 0,3"),
    g(10, "M8,0 L8,16 L0,5 L10,5"),
    g(10, "M9,16 L1,16 L1,10 L5,10 A10,10 10,5 A10,0 5,0 A0,0 0,4"),
    g(10, "M10,13 A10,16 5,16 A0,16 0,11 L0,5 A0,0 5,0 A10,0 10,5 A10,10 5,10 A0,10 0,5"),
    g(10, "M0,16 L10,16 L3,0"),
    g(10, "M5,8 A0,8 0,12 A0,16 5,16 A10,16 10,12 A10,8 5,8 A0,8 0,4 A0,0 5,0 A10,0 10,4 A10,8 5,8"),
    g(10, "M0,3 A0,0 5,0 A10,0 10,5 L10,11 A10,16 5,16 A0,16 0,11 A0,6 5,6 A10,6 10,11"),
    g(0, "M0,10 L0,10 M0,0 L0,0"),
    g(2, "M2,10 L2,10 M2,0 L0,-4"),
    g(9, "M9,12 L0,7 L9,2"),
    g(10, "M0,10 L10,10 M0,4 L10,4"),
    g(9, "M0,12 L9,7 L0,2"),
    g(8, "M0,12 A0,16 4,16 A8,16 8,12 A8,9 4,8 L4,5 M4,0 L4,0"),
    g(16, "M11,8 A11,11 8,11 A5,11 5,8 A5,5 8,5 A11,5 11,8 M11,11 L11,7 A11,5 13,5 A16,5 16,9 A16,16 8,16 A0,16 0,8 A0,0 8,0 L12,0"),
    // A-Z
    g(12, "M0,0 L6,16 L12,0 M2,5 L10,5"),
    g(10, "M0,0 L0,16 L5,16 A9,16 9,12 A9,8 5,8 L0,8 M5,8 L6,8 A10,8 10,4 A10,0 6,0 L0,0"),
    g(11, "M11,12 A11,16 6,16 A0,16 0,10 L0,6 A0,0 6,0 A11,0 11,4"),
    g(11, "M0,0 L0,16 L5,16 A11,16 11,10 L11,6 A11,0 5,0 L0,0"),
    g(9, "M9,16 L0,16 L0,0 L9,0 M0,8 L7,8"),
    g(9, "M9,16 L0,16 L0,0 M0,8 L7,8"),
    g(12, "M11,12 A11,16 6,16 A0,16 0,10 L0,6 A0,0 6,0 A12,0 12,6 L12,8 L7,8"),
    g(11, "M0,0 L0,16 M11,0 L11,16 M0,8 L11,8"),
    g(0, "M0,0 L0,16"),
    g(8, "M8,16 L8,5 A8,0 4,0 A0,0 0,4"),
    g(10, "M0,0 L0,16 M10,16 L0,7 M3,10 L10,0"),
    g(9, "M0,16 L0,0 L9,0"),
    g(14, "M0,0 L0,16 L7,4 L14,16 L14,0"),
    g(11, "M0,0 L0,16 L11,0 L11,16"),
    g(12, "M6,0 A0,0 0,6 L0,10 A0,16 6,16 A12,16 12,10 L12,6 A12,0 6,0"),
    g(10, "M0,0 L0,16 L6,16 A10,16 10,12 A10,8 6,8 L0,8"),
    g(12, "M6,0 A0,0 0,6 L0,10 A0,16 6,16 A12,16 12,10 L12,6 A12,0 6,0 M7,4 L12,-1"),
    g(10, "M0,0 L0,16 L6,16 A10,16 10,12 A10,8 6,8 L0,8 M5,8 L10,0"),
    g(10, "M10,12 A10,16 5,16 A0,16 0,12 A0,8 5,8 A10,8 10,4 A10,0 5,0 A0,0 0,4"),
    g(12, "M0,16 L12,16 M6,16 L6,0"),
    g(12, "M0,16 L0,6 A0,0 6,0 A12,0 12,6 L12,16"),
    g(12, "M0,16 L6,0 L12,16"),
    g(16, "M0,16 L4,0 L8,12 L12,0 L16,16"),
    g(11, "M0,0 L11,16 M0,16 L11,0"),
    g(12, "M0,16 L6,8 L12,16 M6,8 L6,0"),
    g(10, "M0,16 L10,16 L0,0 L10,0"),
    g(4, "M4,18 L0,18 L0,-4 L4,-4"),
    g(7, "M0,18 L7,-2"),
    g(4, "M0,18 L4,18 L4,-4 L0,-4"),
    g(8, "M0,10 L4,16 L8,10"),
    g(10, "M0,-3 L10,-3"),
    g(3, "M0,17 L3,14"),
    // a-z
    g(10, "M10,12 L10,0 M10,6 A10,12 5,12 A0,12 0,6 A0,0 5,0 A10,0 10,6"),
    g(10, "M0,17 L0,0 M0,6 A0,12 5,12 A10,12 10,6 A10,0 5,0 A0,0 0,6"),
    g(9, "M9,9 A9,12 5,12 A0,12 0,6 A0,0 5,0 A9,0 9,3"),
    g(10, "M10,17 L10,0 M10,6 A10,12 5,12 A0,12 0,6 A0,0 5,0 A10,0 10,6"),
    g(10, "M0,6 L10,6 A10,12 5,12 A0,12 0,6 A0,0 5,0 A10,0 10,3"),
    g(7, "M7,17 A3,17 3,13 L3,0 M0,12 L7,12"),
    g(10, "M10,12 L10,-1 A10,-5 5,-5 A1,-5 1,-3 M10,6 A10,12 5,12 A0,12 0,6 A0,0 5,0 A10,0 10,6"),
    g(10, "M0,17 L0,0 M0,7 A0,12 5,12 A10,12 10,7 L10,0"),
    g(0, "M0,12 L0,0 M0,16 L0,16"),
    g(4, "M4,12 L4,-1 A4,-5 0,-5 M4,16 L4,16"),
    g(9, "M0,17 L0,0 M9,12 L0,5 M3,7 L9,0"),
    g(0, "M0,17 L0,0"),
    g(16, "M0,12 L0,0 M0,7 A0,12 4,12 A8,12 8,7 L8,0 M8,7 A8,12 12,12 A16,12 16,7 L16,0"),
    g(10, "M0,12 L0,0 M0,7 A0,12 5,12 A10,12 10,7 L10,0"),
    g(10, "M5,0 A0,0 0,6 A0,12 5,12 A10,12 10,6 A10,0 5,0"),
    g(10, "M0,12 L0,-5 M0,6 A0,12 5,12 A10,12 10,6 A10,0 5,0 A0,0 0,6"),
    g(10, "M10,12 L10,-5 M10,6 A10,12 5,12 A0,12 0,6 A0,0 5,0 A10,0 10,6"),
    g(6, "M0,12 L0,0 M0,7 A0,12 6,12"),
    g(8, "M8,9 A8,12 4,12 A0,12 0,9 A0,6 4,6 A8,6 8,3 A8,0 4,0 A0,0 0,3"),
    g(7, "M3,16 L3,4 A3,0 7,0 M0,12 L7,12"),
    g(10, "M0,12 L0,5 A0,0 5,0 A10,0 10,5 M10,12 L10,0"),
    g(10, "M0,12 L5,0 L10,12"),
    g(14, "M0,12 L3,0 L7,10 L11,0 L14,12"),
    g(9, "M0,0 L9,12 M0,12 L9,0"),
    g(10, "M0,12 L5,0 M10,12 L3,-5"),
    g(9, "M0,12 L9,12 L0,0 L9,0"),
    g(5, "M5,18 A2,18 2,15 L2,10 A2,7 0,7 A2,7 2,4 L2,-1 A2,-4 5,-4"),
    g(0, "M0,18 L0,-4"),
    g(5, "M0,18 A3,18 3,15 L3,10 A3,7 5,7 A3,7 3,4 L3,-1 A3,-4 0,-4"),
    g(10, "M0,6 A0,9 2,9 A4,9 5,7 A6,5 8,5 A10,5 10,8"),
];

pub(crate) fn glyph(character: char) -> &'static Glyph {
    match character {
        ' '..='~' => &GLYPHS[character as usize - ' ' as usize],
        _ => &MISSING,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Move(i32, i32),
    Line(i32, i32),
    /// Corner, then end point.
    Arc(i32, i32, i32, i32),
}

/// The commands of a path. Stops at the first thing it cannot read, which the
/// tests establish never happens for a glyph in the table.
pub(crate) struct Ops<'a>(core::str::Split<'a, char>);

impl Glyph {
    pub(crate) fn ops(&self) -> Ops<'static> {
        Ops(self.path.split(' '))
    }
}

fn pair(text: &str) -> Option<(i32, i32)> {
    let (x, y) = text.split_once(',')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

impl Iterator for Ops<'_> {
    type Item = Op;

    fn next(&mut self) -> Option<Op> {
        let token = self.0.next()?;
        let (x, y) = pair(token.get(1..)?)?;
        match token.as_bytes()[0] {
            b'M' => Some(Op::Move(x, y)),
            b'L' => Some(Op::Line(x, y)),
            b'A' => {
                let (ex, ey) = pair(self.0.next()?)?;
                Some(Op::Arc(x, y, ex, ey))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_path_reads_to_its_end_and_stays_in_its_box() {
        for (index, glyph) in GLYPHS.iter().chain([&MISSING]).enumerate() {
            let name = (b' ' + index as u8) as char;
            let commands = glyph
                .path
                .split(' ')
                .filter(|token| token.starts_with(['M', 'L', 'A']))
                .count();
            assert_eq!(glyph.ops().count(), commands, "{name:?} is malformed");

            let mut started = false;
            for op in glyph.ops() {
                let points = match op {
                    Op::Move(x, y) => {
                        started = true;
                        vec![(x, y)]
                    }
                    Op::Line(x, y) => vec![(x, y)],
                    Op::Arc(cx, cy, x, y) => vec![(cx, cy), (x, y)],
                };
                assert!(started, "{name:?} draws before it moves");
                for (x, y) in points {
                    assert!((0..=glyph.width).contains(&x), "{name:?} x={x}");
                    assert!((-5..=18).contains(&y), "{name:?} y={y}");
                }
            }
        }
    }

    #[test]
    fn only_the_space_is_blank() {
        for character in ' '..='~' {
            assert_eq!(glyph(character).path.is_empty(), character == ' ');
        }
        assert!(core::ptr::eq(glyph('\u{e9}'), &MISSING));
    }

    #[test]
    fn figures_share_a_width() {
        assert!(('0'..='9').all(|digit| glyph(digit).width == 10));
    }
}

//! Corrode's text engine.
//!
//! One job: turn a string into **coverage** -- for every pixel, how much of it
//! the letters cover, 0 to 255. It knows nothing about colour, screens or the
//! renderer, and the renderer knows nothing about letters. What crosses
//! between them is a block of bytes, which is the same thing a glyph atlas
//! holds on a GPU and the same thing that would cross an IPC boundary.
//!
//! # Letters are distances
//!
//! A glyph is a pen path ([`font`]), and a pixel is inked according to how far
//! its centre is from that path:
//!
//! ```text
//! coverage = clamp(r + 1/2 - d, 0, 1)
//! ```
//!
//! with `d` the distance to the nearest stroke and `r` half the stroke's
//! thickness. Inside the stroke that is 1, outside it is 0, and across the
//! one-pixel band at the edge it ramps -- which is the anti-aliasing, for
//! free, with no supersampling. Joins and ends come out round because "within
//! `r` of the path" is round there.
//!
//! Size and weight are only numbers in that formula. There are no bitmaps to
//! scale and no hinting tables; a glyph at 11 pixels and at 300 is the same
//! path measured against a different `r`.
//!
//! The distance to a segment `ab` from `p` needs a square root only near its
//! ends. Alongside it, `d = |ap x ab| / |ab|`, and `|ab|` is found once per
//! segment -- so most pixels cost a multiply and a divide.
//!
//! # No floating point, no allocation
//!
//! Positions are 26.6 fixed point (1/64 pixel). The caller supplies the
//! memory, so this runs unchanged in the kernel.
//!
//! # What is missing
//!
//! Printable ASCII only, one line at a time, no kerning, one typeface. Real
//! font files (TrueType outlines) are a different rasteriser feeding the same
//! [`Coverage`], and are what compatibility will eventually need.

#![cfg_attr(not(test), no_std)]

mod font;

use font::{Op, SPACING};

/// Font units per em: a `Style::px(24)` glyph is one pixel per unit.
const EM: i64 = 24;
/// Units reserved above the baseline, and below it.
const ASCENT: i32 = 18;
const DESCENT: i32 = 6;

/// One pixel, in the 1/64ths positions are kept in.
const PX: i64 = 64;

/// How thick the pen is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Light,
    Regular,
    Bold,
}

impl Weight {
    /// Stroke thickness in sixteenths of a font unit.
    const fn thickness(self) -> i64 {
        match self {
            Self::Light => 22,
            Self::Regular => 32,
            Self::Bold => 50,
        }
    }
}

/// How to draw: how big and how heavy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    /// Em height in 1/64 pixel.
    size: i64,
    weight: Weight,
}

impl Style {
    /// Text `px` pixels tall from the top of an ascender to the bottom of a
    /// descender. Clamped to 4..=1024.
    pub const fn px(px: u32) -> Self {
        let px = if px < 4 {
            4
        } else if px > 1024 {
            1024
        } else {
            px
        };
        Self {
            size: px as i64 * PX,
            weight: Weight::Regular,
        }
    }

    pub const fn weight(self, weight: Weight) -> Self {
        Self { weight, ..self }
    }

    /// Font units to 1/64 pixel, rounded.
    const fn scale(&self, units: i32) -> i64 {
        (units as i64 * self.size + EM / 2).div_euclid(EM)
    }

    /// Half the stroke's thickness. Never under half a pixel: thinner than
    /// that and a stroke fades instead of narrowing, and small text greys out.
    const fn radius(&self) -> i64 {
        let radius = self.weight.thickness() * self.size / (EM * 16 * 2);
        if radius < PX / 2 {
            PX / 2
        } else {
            radius
        }
    }

    /// Whole pixels kept clear around the text for the stroke's thickness,
    /// the soft edge beyond it, and the snap to the pixel grid.
    const fn pad(&self) -> i64 {
        (self.radius() + PX - 1) / PX + 2
    }

    /// Segments a quarter turn is drawn with.
    ///
    /// A chord of angle `a` on a circle of radius `R` strays from the arc by
    /// `R a^2 / 8`. The largest bowls are about a quarter of the em across,
    /// so holding that under a hundredth of a pixel takes a number of steps
    /// that grows with the square root of the size.
    const fn arc_steps(&self) -> i64 {
        let steps = isqrt((self.size / PX) as u64) as i64 * 3 + 1;
        if steps < 6 {
            6
        } else {
            steps
        }
    }
}

const fn ceil_px(value: i64) -> i64 {
    (value + PX - 1).div_euclid(PX)
}

/// The block of pixels a string needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub width: u32,
    pub height: u32,
    /// The row of the block the baseline runs through.
    pub baseline: u32,
}

impl Extent {
    pub const fn area(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

/// How much room `text` takes in `style`. [`draw`] stays inside it.
pub fn measure(text: &str, style: Style) -> Extent {
    let units: i32 = text
        .chars()
        .map(|character| font::glyph(character).width + SPACING)
        .sum();
    let pad = style.pad();
    let ascent = ceil_px(style.scale(ASCENT));
    Extent {
        width: (ceil_px(style.scale(units)) + 2 * pad) as u32,
        height: (ascent + ceil_px(style.scale(DESCENT)) + 2 * pad) as u32,
        baseline: (pad + ascent) as u32,
    }
}

/// Caller-supplied bytes to draw coverage into, one per pixel, row by row.
pub struct Coverage<'a> {
    data: &'a mut [u8],
    width: usize,
    height: usize,
}

impl<'a> Coverage<'a> {
    /// Rows of `width` pixels over `data`. Bytes past the last whole row are
    /// left alone.
    pub fn new(data: &'a mut [u8], width: usize) -> Self {
        let height = data.len().checked_div(width).unwrap_or(0);
        Self {
            data,
            width,
            height,
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Ink one stroke from `a` to `b` (1/64 pixel) with half-thickness `r`.
    ///
    /// Strokes combine by taking the larger coverage, not by adding: where
    /// two overlap, the pixel is as covered as the more covering one says.
    fn stroke(&mut self, a: (i64, i64), b: (i64, i64), r: i64) {
        // Only pixels whose centres can be within `r + 1/2` of the segment.
        let reach = r + PX / 2;
        let first = |low: i64| (low - reach - PX / 2).div_euclid(PX).max(0);
        let last = |high: i64, limit: usize| {
            ((high + reach - PX / 2).div_euclid(PX) + 1).min(limit as i64)
        };
        let (x0, x1) = (first(a.0.min(b.0)), last(a.0.max(b.0), self.width));
        let (y0, y1) = (first(a.1.min(b.1)), last(a.1.max(b.1), self.height));

        let ab = (b.0 - a.0, b.1 - a.1);
        let length_squared = ab.0 * ab.0 + ab.1 * ab.1;
        // Kept to 1/256 of a position unit. An arc is drawn as segments about
        // a pixel long, and a whole-unit length would be off by over a
        // percent on those -- an error that lands directly on the distance.
        let length = isqrt((length_squared as u64) << 16) as i64;

        for y in y0..y1 {
            let row = y as usize * self.width;
            let py = y * PX + PX / 2;
            for x in x0..x1 {
                let px = x * PX + PX / 2;
                let ap = (px - a.0, py - a.1);
                let along = ap.0 * ab.0 + ap.1 * ab.1;

                let distance = if length == 0 || along <= 0 {
                    nearest_root(ap.0 * ap.0 + ap.1 * ap.1)
                } else if along >= length_squared {
                    let bp = (px - b.0, py - b.1);
                    nearest_root(bp.0 * bp.0 + bp.1 * bp.1)
                } else {
                    (((ap.0 * ab.1 - ap.1 * ab.0).abs() << 8) + length / 2) / length
                };

                let covered = (reach - distance).clamp(0, PX);
                if covered > 0 {
                    let value = (covered * 255 / PX) as u8;
                    let pixel = &mut self.data[row + x as usize];
                    *pixel = (*pixel).max(value);
                }
            }
        }
    }
}

/// Square root to the nearest whole number, for distances.
fn nearest_root(value: i64) -> i64 {
    ((isqrt((value as u64) << 2) + 1) >> 1) as i64
}

/// Integer square root, rounded down.
const fn isqrt(value: u64) -> u64 {
    let mut rest = value;
    let mut root = 0u64;
    let mut bit = 1u64 << 62;
    while bit > rest {
        bit >>= 2;
    }
    while bit != 0 {
        if rest >= root + bit {
            rest -= root + bit;
            root = (root >> 1) + bit;
        } else {
            root >>= 1;
        }
        bit >>= 2;
    }
    root
}

/// Where each glyph of a string goes: its path's origin, in 1/64 pixel.
///
/// Origins are snapped so that a stem at the glyph's left edge and a stroke
/// on the baseline both run through pixel centres. That is all the hinting
/// there is, and it is most of what makes small text look sharp.
fn place(text: &str, style: Style) -> impl Iterator<Item = (&'static font::Glyph, i64, i64)> + '_ {
    let snap = |value: i64| (value + PX / 2).div_euclid(PX) * PX + PX / 2;
    let extent = measure(text, style);
    let baseline = extent.baseline as i64 * PX + PX / 2;
    let mut pen = style.pad() * PX + style.scale(SPACING / 2);

    text.chars().map(move |character| {
        let glyph = font::glyph(character);
        let origin = snap(pen - PX / 2);
        pen += style.scale(glyph.width + SPACING);
        (glyph, origin, baseline)
    })
}

/// A quarter turn from `from` to `to` around the corner `corner`, as points.
///
/// This is the rational quadratic with weight `sqrt(1/2)` on the corner,
/// which is exactly a quarter of an ellipse when the corner is square:
///
/// ```text
/// P(t) = ((1-t)^2 P0 + 2w t(1-t) P1 + t^2 P2) / ((1-t)^2 + 2w t(1-t) + t^2)
/// ```
fn arc(
    from: (i64, i64),
    corner: (i64, i64),
    to: (i64, i64),
    steps: i64,
) -> impl Iterator<Item = (i64, i64)> {
    /// sqrt(1/2) in Q16.
    const W: i64 = 46341;
    (1..=steps).map(move |step| {
        let t = (step << 16) / steps;
        let u = (1 << 16) - t;
        let (a, c) = ((u * u) >> 16, (t * t) >> 16);
        let b = (2 * W * ((t * u) >> 16)) >> 16;
        let sum = a + b + c;
        (
            (a * from.0 + b * corner.0 + c * to.0) / sum,
            (a * from.1 + b * corner.1 + c * to.1) / sum,
        )
    })
}

/// Draw `text` into `target`, whose top-left corner is the top-left of the
/// block [`measure`] describes. Coverage already there is kept, so strings
/// can be layered into one block. Anything that would fall outside the
/// target is clipped.
pub fn draw(text: &str, style: Style, target: &mut Coverage<'_>) {
    let radius = style.radius();
    let steps = style.arc_steps();

    for (glyph, origin, baseline) in place(text, style) {
        // y is up in a glyph and down on a screen.
        let point = |x: i32, y: i32| (origin + style.scale(x), baseline - style.scale(y));
        let mut pen = (origin, baseline);

        for op in glyph.ops() {
            match op {
                Op::Move(x, y) => pen = point(x, y),
                Op::Line(x, y) => {
                    let to = point(x, y);
                    target.stroke(pen, to, radius);
                    pen = to;
                }
                Op::Arc(cx, cy, x, y) => {
                    for to in arc(pen, point(cx, cy), point(x, y), steps) {
                        target.stroke(pen, to, radius);
                        pen = to;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRINTABLE: &str = " !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~";

    fn render(text: &str, style: Style) -> (Vec<u8>, Extent) {
        let extent = measure(text, style);
        let mut data = vec![0u8; extent.area()];
        draw(
            text,
            style,
            &mut Coverage::new(&mut data, extent.width as usize),
        );
        (data, extent)
    }

    #[test]
    fn isqrt_matches_reference() {
        for value in (0..2_000_000u64).chain([u32::MAX as u64, 1 << 40, (1 << 62) - 1]) {
            let root = isqrt(value);
            assert!(
                root * root <= value && (root + 1) * (root + 1) > value,
                "{value}"
            );
        }
    }

    #[test]
    fn ink_stays_inside_what_was_measured() {
        for px in [4, 9, 16, 31, 72, 200] {
            for weight in [Weight::Light, Weight::Regular, Weight::Bold] {
                let (data, extent) = render(PRINTABLE, Style::px(px).weight(weight));
                let (w, h) = (extent.width as usize, extent.height as usize);
                // Not just in bounds -- the outermost ring of pixels is
                // untouched, so nothing was clipped at the edge either.
                for x in 0..w {
                    assert_eq!(data[x], 0, "{px}px top");
                    assert_eq!(data[(h - 1) * w + x], 0, "{px}px bottom");
                }
                for y in 0..h {
                    assert_eq!(data[y * w], 0, "{px}px left");
                    assert_eq!(data[y * w + w - 1], 0, "{px}px right");
                }
            }
        }
    }

    #[test]
    fn every_character_but_the_space_leaves_ink() {
        for character in PRINTABLE.chars() {
            let mut buffer = [0u8; 4];
            let (data, _) = render(character.encode_utf8(&mut buffer), Style::px(16));
            assert_eq!(
                data.iter().any(|v| *v > 128),
                character != ' ',
                "{character:?}"
            );
        }
    }

    #[test]
    fn a_stem_is_as_thick_as_its_weight_says() {
        // Summing coverage across a stem measures its width to a fraction of
        // a pixel, edges included.
        for (weight, units) in [
            (Weight::Light, 22.0),
            (Weight::Regular, 32.0),
            (Weight::Bold, 50.0),
        ] {
            let style = Style::px(96).weight(weight);
            let (data, extent) = render("I", style);
            let row = (extent.baseline - 20) as usize * extent.width as usize;
            let width: f64 = data[row..row + extent.width as usize]
                .iter()
                .map(|v| *v as f64 / 255.0)
                .sum();
            let expected = units / 16.0 * 96.0 / 24.0;
            assert!(
                (width - expected).abs() < 0.1,
                "{weight:?}: {width} vs {expected}"
            );
        }
    }

    #[test]
    fn small_text_keeps_a_solid_stroke() {
        // At 8 pixels the nominal stroke is a third of a pixel. The floor on
        // the radius, and the snap to pixel centres, keep the stem at full
        // strength instead of a grey smear.
        let (data, extent) = render("l", Style::px(8));
        let row = (extent.baseline - 3) as usize * extent.width as usize;
        assert_eq!(
            data[row..row + extent.width as usize].iter().max(),
            Some(&255)
        );
    }

    #[test]
    fn width_grows_with_the_string_and_the_size() {
        let width = |text, px| measure(text, Style::px(px)).width;
        assert!(width("ab", 16) > width("a", 16));
        assert!(width("a", 32) > width("a", 16));
        assert!(width("m", 16) > width("i", 16));
        assert_eq!(width("0000", 16), width("1111", 16));
    }

    #[test]
    fn a_target_that_is_too_small_clips() {
        let mut data = [0u8; 12];
        draw("CORRODE", Style::px(200), &mut Coverage::new(&mut data, 4));
        draw("x", Style::px(16), &mut Coverage::new(&mut [], 0));
    }

    /// The same picture from first principles, in floating point: every
    /// stroke as exact geometry (arcs as true conics, sampled far finer than
    /// the rasteriser's flats), and coverage from the real distance.
    fn reference(text: &str, style: Style) -> (Vec<f64>, Extent) {
        let extent = measure(text, style);
        let mut segments: Vec<((f64, f64), (f64, f64))> = Vec::new();
        let r = style.radius() as f64 / 64.0;

        for (glyph, origin, baseline) in place(text, style) {
            let size = style.size as f64 / 64.0;
            let point = |x: i32, y: i32| {
                (
                    origin as f64 / 64.0 + x as f64 * size / 24.0,
                    baseline as f64 / 64.0 - y as f64 * size / 24.0,
                )
            };
            let mut pen = (0.0, 0.0);
            for op in glyph.ops() {
                match op {
                    Op::Move(x, y) => pen = point(x, y),
                    Op::Line(x, y) => {
                        segments.push((pen, point(x, y)));
                        pen = point(x, y);
                    }
                    Op::Arc(cx, cy, x, y) => {
                        let (p0, p1, p2) = (pen, point(cx, cy), point(x, y));
                        let w = 0.5f64.sqrt();
                        for step in 1..=400 {
                            let t = step as f64 / 400.0;
                            let (a, b, c) = ((1.0 - t) * (1.0 - t), 2.0 * w * t * (1.0 - t), t * t);
                            let to = (
                                (a * p0.0 + b * p1.0 + c * p2.0) / (a + b + c),
                                (a * p0.1 + b * p1.1 + c * p2.1) / (a + b + c),
                            );
                            segments.push((pen, to));
                            pen = to;
                        }
                    }
                }
            }
        }

        let mut out = vec![0.0; extent.area()];
        for y in 0..extent.height as usize {
            for x in 0..extent.width as usize {
                let p = (x as f64 + 0.5, y as f64 + 0.5);
                let nearest = segments
                    .iter()
                    .map(|(a, b)| {
                        let ab = (b.0 - a.0, b.1 - a.1);
                        let ap = (p.0 - a.0, p.1 - a.1);
                        let length = ab.0 * ab.0 + ab.1 * ab.1;
                        let t = if length == 0.0 {
                            0.0
                        } else {
                            ((ap.0 * ab.0 + ap.1 * ab.1) / length).clamp(0.0, 1.0)
                        };
                        (ap.0 - t * ab.0).hypot(ap.1 - t * ab.1)
                    })
                    .fold(f64::INFINITY, f64::min);
                out[y * extent.width as usize + x] = (r + 0.5 - nearest).clamp(0.0, 1.0);
            }
        }
        (out, extent)
    }

    #[test]
    fn coverage_matches_the_distance_formula() {
        for (text, px) in [("CorrodeOS", 40), ("Sg@8&", 23), ("quartz", 13), ("O", 150)] {
            let style = Style::px(px);
            let (got, _) = render(text, style);
            let (want, _) = reference(text, style);
            let worst = got
                .iter()
                .zip(&want)
                .map(|(got, want)| (*got as f64 / 255.0 - want).abs())
                .fold(0.0, f64::max);
            // A sixty-fourth of a pixel in position is 1.6% in coverage.
            assert!(worst < 0.04, "{text:?} at {px}px is off by {worst}");
        }
    }

    /// `./corrode specimen` runs this to write a sheet of the typeface, for
    /// looking at. It is how the glyph table gets judged by eye.
    #[test]
    #[ignore = "writes a picture; run through ./corrode specimen"]
    fn specimen() {
        let lines: [(&str, Style); 7] = [
            ("CorrodeOS", Style::px(96)),
            ("ABCDEFGHIJKLMNOPQRSTUVWXYZ", Style::px(40)),
            ("abcdefghijklmnopqrstuvwxyz", Style::px(40)),
            (
                "0123456789 !\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~",
                Style::px(32),
            ),
            (
                "The quick brown fox jumps over the lazy dog.",
                Style::px(24).weight(Weight::Bold),
            ),
            (
                "The quick brown fox jumps over the lazy dog. 0123456789",
                Style::px(16),
            ),
            (
                "Sphinx of black quartz, judge my vow: capabilities, not identity.",
                Style::px(12),
            ),
        ];

        let blocks: Vec<_> = lines
            .iter()
            .map(|(text, style)| render(text, *style))
            .collect();
        let width = blocks.iter().map(|(_, e)| e.width as usize).max().unwrap() + 32;
        let height: usize = blocks
            .iter()
            .map(|(_, e)| e.height as usize + 12)
            .sum::<usize>()
            + 32;

        let mut sheet = vec![0u8; width * height];
        let mut top = 16;
        for (data, extent) in &blocks {
            for y in 0..extent.height as usize {
                let from = y * extent.width as usize;
                let to = (top + y) * width + 16;
                sheet[to..to + extent.width as usize]
                    .copy_from_slice(&data[from..from + extent.width as usize]);
            }
            top += extent.height as usize + 12;
        }

        let path = std::env::var("SPECIMEN").unwrap_or_else(|_| "specimen.pgm".into());
        let mut file = format!("P5\n{width} {height}\n255\n").into_bytes();
        file.extend_from_slice(&sheet);
        std::fs::write(path, file).unwrap();
    }
}

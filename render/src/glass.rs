//! Liquid glass: a pane that bends, frosts and lights what is behind it.
//!
//! A port of the optics in Glass HQ's `liquid-glass`
//! (<https://github.com/Glass-HQ/liquid-glass>, MIT, (c) 2026 Glass HQ). There
//! the effect is an SVG filter chain over the page: a displacement map drawn
//! by a WGSL shader, `feGaussianBlur` for the frost, `feComponentTransfer`
//! tables for each material's tone, and an overlay plane for the rim. Here
//! it is the same arithmetic over pixels already on the target, in integers,
//! so it runs in a kernel with no floating point and on the build host alike.
//!
//! What a glass pane does to the pixels behind it, in order:
//!
//! 1. **Frost.** Regular glass blurs its backdrop, then lightens it (darkens
//!    it, in dark appearance) towards a much wider blur. Clear glass does
//!    neither.
//! 2. **Refraction.** Near the edge the pane is a lens: each pixel looks at
//!    the backdrop a little way inward along the edge's normal, by an amount
//!    that falls from `refraction / 2` pixels at the rim to nothing a bezel's
//!    width in. Inside that, the backdrop is seen straight through.
//! 3. **Tone.** A per-material curve: lift the shadows, roll off the
//!    highlights, and for Regular glass push the chroma up a little.
//! 4. **Tint.** Coloured glass passes luminance through the colour rather
//!    than laying the colour over the top.
//! 5. **Rim.** A highlight along the top and bottom edges, and a shade down
//!    the sides.
//!
//! Units: lengths in 256ths of a pixel ([`Q8`]) and fractions in 4096ths
//! ([`ONE`]), multiplied in `i64` so nothing overflows.

use crate::Color;

/// 1.0, for fractions.
pub const ONE: i64 = 1 << 12;
/// One pixel, for lengths.
pub const Q8: i64 = 1 << 8;

/// How wide the lens is: the distance from the edge over which refraction
/// fades out. The library's `inverseHeight` is 0.05, one over twenty pixels.
pub const BEZEL: i64 = 20;

/// Which of the library's two materials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Material {
    /// Sees the backdrop sharp, lifted and bent at the edges.
    Clear,
    /// Frosted: blurred and veiled, for things that must stay legible.
    Regular,
}

/// A glass pane's look. Plain data, like everything a command carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Glass {
    pub material: Material,
    /// Tuned for a dark desktop: darker veil, dimmer rim.
    pub dark: bool,
    /// Corner radius in pixels. Larger than half the short side makes a
    /// capsule.
    pub radius: u16,
    /// How far the edge bends the backdrop: the library's `refraction`,
    /// 60 by default, which shifts the rim's pixels by up to 30.
    pub refraction: u8,
    /// How strongly to frost, as a percentage of the material's own blur.
    /// 100 is the library's look; 0 turns Regular glass's blur off.
    pub frost: u8,
    /// Colour to tint with, and how much of it out of 255. Zero is untinted.
    pub tint: Color,
    pub tint_amount: u8,
    /// Whether to light the rim.
    pub rim: bool,
}

impl Glass {
    pub const fn regular(dark: bool) -> Self {
        Self {
            material: Material::Regular,
            dark,
            radius: 16,
            refraction: 60,
            frost: 100,
            tint: Color::WHITE,
            tint_amount: 0,
            rim: true,
        }
    }

    pub const fn clear(dark: bool) -> Self {
        Self {
            material: Material::Clear,
            ..Self::regular(dark)
        }
    }

    /// The box blur radii, three passes each, that stand in for the
    /// material's two Gaussians: the frost, and the wider fill it is veiled
    /// towards. Zero for no blur.
    ///
    /// Three boxes of width `w` have the variance of a Gaussian with
    /// `sigma^2 = 3 (w^2 - 1) / 12`, so `w = sqrt(4 sigma^2 + 1)`.
    pub fn blur_radii(&self) -> (u32, u32) {
        if self.material == Material::Clear || self.frost == 0 {
            return (0, 0);
        }
        // Sigmas in hundredths of a pixel, from the library's materials.
        let (frost, fill) = if self.dark { (733, 2180) } else { (733, 2300) };
        let radius = |sigma: i64| {
            let sigma = sigma * self.frost as i64 / 100;
            let width = isqrt((4 * sigma * sigma + 100 * 100) as u64) as i64 / 100;
            ((width - 1) / 2).max(0) as u32
        };
        (radius(frost), radius(fill))
    }

    /// How far outside its rectangle a pane reads: three passes of the
    /// widest blur. Whatever is there must be painted before the pane is.
    pub fn reach(&self) -> u32 {
        let (frost, fill) = self.blur_radii();
        3 * frost.max(fill)
    }

    /// The material's tone curve as a lookup table, one entry per channel
    /// value.
    pub fn tone(&self) -> [u8; 256] {
        // The library's tables: 33 points from 0 to 1, read by linear
        // interpolation, as `feFuncR type="table"` does. Fractions in
        // ten-thousandths, to write the library's constants as they are.
        const S: i64 = 10_000;
        let mut table = [0i64; 33];
        for (n, value) in table.iter_mut().enumerate() {
            let v = n as i64 * S / 32;
            let out = match (self.material, self.dark) {
                (Material::Regular, dark) => {
                    let (a, b, q, weight) = if dark {
                        (1050, 8300, -2091, 9000)
                    } else {
                        (3600, 6700, -1113, 8000)
                    };
                    let toned = a + b * v / S + q * v / S * v / S;
                    (weight * toned + (S - weight) * v) / S
                }
                (Material::Clear, dark) => {
                    let (lift, rolloff) = if dark { (1500, 2000) } else { (1800, 0) };
                    let cube = |x: i64| x * x / S * x / S;
                    let tone = v + lift * cube(S - v) / S - rolloff * cube(v) / S;
                    let (tinted, weight) = if dark {
                        (3800 * v / S + 2000, 1400)
                    } else {
                        (7800 * v / S + 2600, 600)
                    };
                    ((S - weight) * tone + weight * tinted) / S
                }
            };
            *value = out.clamp(0, S);
        }
        let mut lut = [0u8; 256];
        for (index, entry) in lut.iter_mut().enumerate() {
            // Where this value falls among the 32 spans, in 255ths of one.
            let at = index as i64 * 32;
            let (k, within) = ((at / 255).min(31) as usize, at - (at / 255).min(31) * 255);
            let value = table[k] + (table[k + 1] - table[k]) * within / 255;
            *entry = ((value * 255 + S / 2) / S) as u8;
        }
        lut
    }

    /// How much Regular glass saturates, in [`ONE`]s: `feColorMatrix
    /// type="saturate"`. One for Clear glass, which leaves chroma alone.
    pub fn chroma(&self) -> i64 {
        match (self.material, self.dark) {
            (Material::Clear, _) => ONE,
            (Material::Regular, false) => 11031 * ONE / 10_000,
            (Material::Regular, true) => 10648 * ONE / 10_000,
        }
    }

    /// The veil Regular glass puts its frost behind: how much of the lighter
    /// (darker) of frost and fill to take, out of 255.
    pub fn fill_opacity(&self) -> u32 {
        if self.dark {
            166 // 0.65
        } else {
            87 // 0.34
        }
    }
}

/// Integer square root, rounded down.
pub fn isqrt(n: u64) -> u64 {
    if n < 2 {
        return n;
    }
    // Newton's method from above: start at a power of two no smaller than
    // the root and every step stays at or above it until it lands.
    let mut x = 1u64 << ((64 - n.leading_zeros()).div_ceil(2));
    loop {
        let y = (x + n / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

fn sat(x: i64) -> i64 {
    x.clamp(0, ONE)
}

fn mul(a: i64, b: i64) -> i64 {
    a * b / ONE
}

/// The square root of a fraction, as a fraction.
fn sqrt1(x: i64) -> i64 {
    isqrt((x.max(0) * ONE) as u64) as i64
}

/// `x` to the power 1.25, for a fraction: `x` times its fourth root.
fn pow125(x: i64) -> i64 {
    mul(x, sqrt1(sqrt1(x)))
}

/// Where a point is relative to a rounded rectangle's edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    /// Signed distance to the edge in [`Q8`] pixels: negative inside.
    pub distance: i64,
    /// The outward normal of the nearest edge, in [`ONE`]s.
    pub normal: (i64, i64),
}

/// The distance field of a `width` by `height` rectangle with corners of
/// `radius`, at `(x, y)` from its top left, all in [`Q8`] pixels.
///
/// The library tessellates a squircle and measures to its segments. A
/// circular corner is the same field to within a pixel at the radii a
/// desktop uses, and is a square root rather than a search.
pub fn field(x: i64, y: i64, width: i64, height: i64, radius: i64) -> Field {
    let (half_w, half_h) = (width / 2, height / 2);
    let radius = radius.clamp(0, half_w.min(half_h));
    let (px, py) = (x - half_w, y - half_h);
    let (sx, sy) = (if px < 0 { -1 } else { 1 }, if py < 0 { -1 } else { 1 });
    let qx = px.abs() - (half_w - radius);
    let qy = py.abs() - (half_h - radius);
    if qx > 0 && qy > 0 {
        let length = isqrt((qx * qx + qy * qy) as u64) as i64;
        let length = length.max(1);
        Field {
            distance: length - radius,
            normal: (sx * qx * ONE / length, sy * qy * ONE / length),
        }
    } else if qx > qy {
        Field {
            distance: qx - radius,
            normal: (sx * ONE, 0),
        }
    } else {
        Field {
            distance: qy - radius,
            normal: (0, sy * ONE),
        }
    }
}

/// How much of a pixel the shape covers, out of 255, a pixel's width of
/// antialiasing straddling the edge.
pub fn coverage(field: &Field) -> u32 {
    ((Q8 / 2 - field.distance).clamp(0, Q8) * 255 / Q8) as u32
}

/// How far, in [`Q8`] pixels, a pixel looks for its backdrop: inward along
/// the normal, most at the rim, none a [`BEZEL`] in. The library's
/// `edgeDisplacement`, with the SVG map's halving folded in.
pub fn displacement(field: &Field, refraction: u8) -> (i64, i64) {
    let depth = sat(-field.distance * ONE / (BEZEL * Q8));
    let edge = ONE - sat(sqrt1(mul(2 * ONE - depth, depth)));
    let amount = edge * refraction as i64 * Q8 / (2 * ONE);
    (
        -field.normal.0 * amount / ONE,
        -field.normal.1 * amount / ONE,
    )
}

/// The rim at a point: how much light to lay over it and how opaque the
/// rim is there, both out of 255. Over a pixel `c` the rim gives
/// `c * (255 - alpha) / 255 + light`.
///
/// The second plane of the library's map shader, at one device pixel per
/// CSS pixel.
pub fn rim(field: &Field, dark: bool) -> (u32, u32) {
    // Fractions throughout; distances turned into them a pixel to one.
    let d = field.distance * ONE / Q8;
    let (nx, ny) = field.normal;
    let mix = |a: i64, b: i64| if dark { b } else { a };

    // cos(1.08) and cos(1.08 * 0.65): how far round from straight up or down
    // the highlight reaches.
    const COS_SPREAD: i64 = 1930; // 0.4713
    const COS_DIFFUSE: i64 = 3127; // 0.7635
    let directional = pow125(sat((ny.abs() - COS_SPREAD) * ONE / (ONE - COS_SPREAD)));
    let diffuse_dir = pow125(sat((ny.abs() - COS_DIFFUSE) * ONE / (ONE - COS_DIFFUSE)));

    let covered = sat(ONE / 2 - d);
    let stroke = mul(sat(d + ONE * 14 / 10), covered);
    let mut specular = mul(directional, stroke);
    specular = specular * ONE / (ONE + (ONE - specular) / 2);
    let fade = ONE - sat(-d * 10 / 45);
    let diffuse = mul(mul(mul(diffuse_dir, 737), mul(fade, fade)), covered); // 0.18
    let highlight = sat(mul(specular, mix(ONE, 3359)) + mul(diffuse, mix(ONE, 1638)));

    // The library's shade is a line straddling the edge, half of it outside
    // the shape. That works where there are pixels a quarter of one either
    // side of the edge; on a screen of whole pixels the outer half lands
    // on no pane at all, so the line comes in by a pixel, onto the last
    // whole pixel inside.
    let adjusted = d + ONE / 2;
    let falloff = ONE - sat(-adjusted * 2);
    let step = if falloff > 0 { ONE } else { 0 };
    let ramp = step + mul(falloff - step, 3072); // mix(step, falloff, 0.75)
    let outline = mul(
        mul(ramp, sat(adjusted + ONE)),
        sat(-adjusted + ONE / 2),
    );
    // cos(0.98): how far round from the side the shade reaches.
    const COS_SIDE: i64 = 2281; // 0.5570
    let weight = |n: i64| mul(sat((n - COS_SIDE) * ONE / (ONE - COS_SIDE)), outline);
    let soften = |w: i64| w * ONE / (ONE + mul(1638, ONE - w));
    let shade = mul(soften(weight(nx)) + soften(weight(-nx)), mix(1966, 2458));

    let alpha = sat(highlight + mul(shade, ONE - highlight));
    let light = mul(mix(ONE, 3359), highlight);
    (
        (light * 255 / ONE) as u32,
        (alpha * 255 / ONE) as u32,
    )
}

/// Pull each channel of `rgb` away from its luminance by `chroma`.
pub fn saturate(rgb: [u32; 3], chroma: i64) -> [u32; 3] {
    if chroma == ONE {
        return rgb;
    }
    // The weights `feColorMatrix type="saturate"` uses.
    let luma = (213 * rgb[0] as i64 + 715 * rgb[1] as i64 + 72 * rgb[2] as i64) / 1000;
    rgb.map(|c| (luma + (c as i64 - luma) * chroma / ONE).clamp(0, 255) as u32)
}

/// Coloured glass: the colour, lit by the backdrop's luminance, mixed in by
/// `amount` out of 255. The library's `tintMarkup`, whose glass body passes
/// 0.78 of the colour (0.68 for Clear) whatever is behind it.
pub fn tint(rgb: [u32; 3], glass: &Glass) -> [u32; 3] {
    if glass.tint_amount == 0 {
        return rgb;
    }
    let body = match glass.material {
        Material::Regular => 199, // 0.78
        Material::Clear => 173,   // 0.68
    };
    let luma = (213 * rgb[0] + 715 * rgb[1] + 72 * rgb[2]) / 1000;
    let lit = body + (255 - body) * luma / 255;
    let colour = [glass.tint.r, glass.tint.g, glass.tint.b].map(|c| c as u32 * lit / 255);
    let amount = glass.tint_amount as u32;
    [0, 1, 2].map(|i| (colour[i] * amount + rgb[i] * (255 - amount)) / 255)
}

/// Box-blur `pixels`, `width` by `height` of `0x00RRGGBB`, in place:
/// across each row and then down each column, each a running sum over
/// `2 * radius + 1` pixels with the edges repeated. `line` is somewhere to
/// keep one row or column while it is rewritten, and must be at least as
/// long as the longer side.
pub fn box_blur(pixels: &mut [u32], width: usize, height: usize, radius: usize, line: &mut [u32]) {
    if radius == 0 || width == 0 || height == 0 {
        return;
    }
    for y in 0..height {
        let row = &mut pixels[y * width..(y + 1) * width];
        line[..width].copy_from_slice(row);
        blur_line(&line[..width], radius, |x, pixel| row[x] = pixel);
    }
    for x in 0..width {
        for y in 0..height {
            line[y] = pixels[y * width + x];
        }
        blur_line(&line[..height], radius, |y, pixel| pixels[y * width + x] = pixel);
    }
}

fn blur_line(line: &[u32], radius: usize, mut put: impl FnMut(usize, u32)) {
    let last = line.len() as isize - 1;
    let at = |i: isize| line[i.clamp(0, last) as usize];
    let span = 2 * radius as u32 + 1;
    let mut sum = [0u32; 3];
    let add = |sum: &mut [u32; 3], pixel: u32, sign: bool| {
        for (channel, total) in sum.iter_mut().enumerate() {
            let value = (pixel >> (16 - 8 * channel)) & 0xff;
            if sign {
                *total += value;
            } else {
                *total -= value;
            }
        }
    };
    for i in -(radius as isize)..=radius as isize {
        add(&mut sum, at(i), true);
    }
    for i in 0..line.len() as isize {
        put(
            i as usize,
            (sum[0] / span) << 16 | (sum[1] / span) << 8 | sum[2] / span,
        );
        add(&mut sum, at(i + radius as isize + 1), true);
        add(&mut sum, at(i - radius as isize), false);
    }
}

/// `0x00RRGGBB` as three channels, and back.
pub fn channels(pixel: u32) -> [u32; 3] {
    [(pixel >> 16) & 0xff, (pixel >> 8) & 0xff, pixel & 0xff]
}

pub fn pack(rgb: [u32; 3]) -> u32 {
    rgb[0].min(255) << 16 | rgb[1].min(255) << 8 | rgb[2].min(255)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The library's float formulas, written out as it has them, to check
    /// the integers against.
    fn edge_displacement(distance: f64, amount: f64, inverse_height: f64) -> f64 {
        let depth = (-distance * inverse_height).clamp(0.0, 1.0);
        amount * (1.0 - ((2.0 - depth) * depth).sqrt().clamp(0.0, 1.0))
    }

    #[test]
    fn isqrt_is_the_floor_of_the_root() {
        for n in (0..100_000u64).chain([u32::MAX as u64, u64::MAX / 3]) {
            let r = isqrt(n);
            assert!(r * r <= n, "{n}");
            assert!((r + 1).checked_mul(r + 1).is_none_or(|sq| sq > n), "{n}");
        }
    }

    #[test]
    fn displacement_follows_the_library_curve() {
        for tenth in 0..=300 {
            let distance = -(tenth as f64) / 10.0;
            let want = edge_displacement(distance, 30.0, 0.05);
            let at = Field {
                distance: (distance * Q8 as f64) as i64,
                normal: (ONE, 0),
            };
            let (dx, dy) = displacement(&at, 60);
            let got = -dx as f64 / Q8 as f64;
            assert!((got - want).abs() < 0.1, "at {distance}: {got} vs {want}");
            assert_eq!(dy, 0);
        }
    }

    #[test]
    fn the_field_is_negative_inside_and_points_out() {
        let (w, h, r) = (100 * Q8, 40 * Q8, 10 * Q8);
        let centre = field(w / 2, h / 2, w, h, r);
        assert_eq!(centre.distance, -20 * Q8);
        let top = field(w / 2, Q8, w, h, r);
        assert_eq!((top.distance, top.normal), (-Q8, (0, -ONE)));
        let right = field(w - Q8, h / 2, w, h, r);
        assert_eq!(right.normal, (ONE, 0));
        // Diagonally out from the top left corner's centre.
        let corner = field(0, 0, w, h, r);
        assert!(corner.distance > 0);
        assert!(corner.normal.0 < 0 && corner.normal.1 < 0);
        assert!((corner.normal.0 - corner.normal.1).abs() < 8);
    }

    #[test]
    fn a_corner_is_round() {
        let (w, h, r) = (64 * Q8, 64 * Q8, 16 * Q8);
        // The corner pixel is outside, the middle of an edge is inside.
        assert_eq!(coverage(&field(Q8 / 2, Q8 / 2, w, h, r)), 0);
        assert_eq!(coverage(&field(32 * Q8, Q8 / 2, w, h, r)), 255);
        // Along the diagonal the edge is at r (1 - 1/sqrt 2) from the corner.
        let edge = 16.0 * (1.0 - core::f64::consts::FRAC_1_SQRT_2);
        let at = ((edge + 1.0) * Q8 as f64) as i64;
        assert!(coverage(&field(at, at, w, h, r)) > 200);
    }

    #[test]
    fn tone_tables_match_the_library() {
        // Regular, light: 0.8 * (0.36 + 0.67 v - 0.1113 v^2) + 0.2 v.
        let regular = Glass::regular(false).tone();
        for (index, value) in regular.iter().enumerate() {
            let v = index as f64 / 255.0;
            let want = (0.8 * (0.36 + 0.67 * v - 0.1113 * v * v) + 0.2 * v).clamp(0.0, 1.0);
            assert!((*value as f64 - want * 255.0).abs() <= 1.5, "{index}");
        }
        // Clear, dark: shadows lifted, highlights rolled off, a pale veil.
        let clear = Glass::clear(true).tone();
        for (index, value) in clear.iter().enumerate() {
            let v = index as f64 / 255.0;
            let tone = v + 0.15 * (1.0 - v).powi(3) - 0.2 * v.powi(3);
            let want = (0.86 * tone + 0.14 * (0.38 * v + 0.2)).clamp(0.0, 1.0);
            assert!((*value as f64 - want * 255.0).abs() <= 1.5, "{index}");
        }
    }

    #[test]
    fn the_rim_lights_the_top_and_shades_the_sides() {
        let (w, h, r) = (200 * Q8, 60 * Q8, 20 * Q8);
        let top = rim(&field(100 * Q8, Q8 / 2, w, h, r), false);
        let side = rim(&field(w - Q8 / 2, 30 * Q8, w, h, r), false);
        let middle = rim(&field(100 * Q8, 30 * Q8, w, h, r), false);
        assert!(top.0 > 128, "top {top:?}");
        assert_eq!(side.0, 0, "side {side:?}");
        assert!(side.1 > 32, "side {side:?}");
        assert_eq!(middle, (0, 0));
    }

    #[test]
    fn blur_keeps_flat_colour_flat_and_spreads_a_spike() {
        let line = &mut [0u32; 16];
        let mut flat = [0x00336699u32; 16 * 8];
        box_blur(&mut flat, 16, 8, 3, line);
        assert!(flat.iter().all(|p| *p == 0x00336699));

        let mut spike = [0u32; 9 * 9];
        spike[4 * 9 + 4] = 0x00ffffff;
        box_blur(&mut spike, 9, 9, 1, line);
        // 255 / 9 in each of the nine pixels around it.
        assert_eq!(spike[4 * 9 + 4], 0x001c1c1c);
        assert_eq!(spike[3 * 9 + 3], 0x001c1c1c);
        assert_eq!(spike[2 * 9 + 2], 0);
    }

    #[test]
    fn regular_glass_frosts_and_clear_glass_does_not() {
        assert_eq!(Glass::clear(false).blur_radii(), (0, 0));
        let (frost, fill) = Glass::regular(false).blur_radii();
        assert!((6..=8).contains(&frost) && (21..=24).contains(&fill));
        let none = Glass {
            frost: 0,
            ..Glass::regular(false)
        };
        assert_eq!(none.reach(), 0);
    }

    #[test]
    fn tint_keeps_its_hue_over_a_light_backdrop() {
        let blue = Glass {
            tint: Color::rgb(0, 122, 255),
            tint_amount: 255,
            ..Glass::regular(false)
        };
        let over_white = tint([255, 255, 255], &blue);
        assert_eq!(over_white, [0, 122, 255]);
        let over_black = tint([0, 0, 0], &blue);
        assert!(over_black[2] > 150 && over_black[0] == 0);
    }
}

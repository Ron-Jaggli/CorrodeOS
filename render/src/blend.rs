//! Alpha blending.
//!
//! Putting a colour `s` over a pixel `d` at opacity `a` in 0..=255 is
//!
//! ```text
//! out = (s·a + d·(255 − a)) / 255        rounded to nearest
//! ```
//!
//! per channel. Two things about that line decide whether a fade is usable on
//! a slow machine, and neither is the multiply.
//!
//! **The divide.** Dividing by 255 is a real division, and shifting by 8
//! instead is the classic shortcut that makes white-over-white come out as
//! 254 -- a fade that never quite arrives, and a seam wherever a faded region
//! meets an untouched one. For any `x` up to 65535,
//!
//! ```text
//! round(x / 255) = (t + (t >> 8)) >> 8      where t = x + 128
//! ```
//!
//! exactly: `1/255 = 1/256 + 1/256² + ...`, and two terms of that series plus
//! the rounding bias are enough at this width. Two shifts and two adds.
//!
//! **Four channels, two multiplies.** A channel's sum never exceeds
//! `255·255 + 128`, which fits in 16 bits. So a 32-bit pixel is split into its
//! even and odd bytes, each pair sitting in the two 16-bit halves of one word,
//! and both halves go through the formula in a single multiply with no carry
//! able to cross between them. Everything that depends only on the colour and
//! the opacity is worked out once per rectangle, leaving one multiply-add per
//! pair per pixel.
//!
//! Blending is done on the stored values, which are gamma-encoded. That is
//! what every desktop compositor does by default and it is slightly wrong --
//! a 50% fade between black and white is darker than the true midpoint.
//! Recorded rather than fixed: linear-light blending costs two table lookups
//! per channel per pixel, on the path that has to stay cheap.

use crate::{Color, PixelFormat};

/// `round(x / 255)`, exact for `x <= 65535`.
pub const fn div255(x: u32) -> u32 {
    let t = x + 128;
    (t + (t >> 8)) >> 8
}

/// One channel: `src` over `dst` at `alpha`.
pub const fn mix(src: u8, dst: u8, alpha: u8) -> u8 {
    let a = alpha as u32;
    div255(src as u32 * a + dst as u32 * (255 - a)) as u8
}

/// The even bytes of a pixel, each in its own 16-bit lane.
const LANES: u32 = 0x00ff_00ff;
/// The rounding bias, once per lane.
const BIAS: u32 = 0x0080_0080;

/// One colour at one opacity, ready to be put over any number of pixels.
#[derive(Clone, Copy, Debug)]
pub struct Blend {
    alpha: u8,
    plan: Plan,
}

#[derive(Clone, Copy, Debug)]
enum Plan {
    /// Every channel is a whole byte, so the pixel is four byte lanes and the
    /// unused one blends along harmlessly. `even` and `odd` hold `s·a + 128`
    /// for each lane, which is the whole of the source's contribution.
    Lanes { even: u32, odd: u32, inverse: u32 },
    /// Channels of other widths or at odd offsets: one at a time.
    Fields { format: PixelFormat, color: Color },
}

impl Blend {
    pub const fn new(format: &PixelFormat, color: Color, alpha: u8) -> Self {
        let plan = if byte_lanes(format) {
            let packed = format.pack(color);
            let a = alpha as u32;
            Plan::Lanes {
                even: (packed & LANES) * a + BIAS,
                odd: ((packed >> 8) & LANES) * a + BIAS,
                inverse: 255 - a,
            }
        } else {
            Plan::Fields {
                format: *format,
                color,
            }
        };
        Self { alpha, plan }
    }

    /// The pixel that results from putting this over `dst`.
    #[inline]
    pub const fn over(&self, dst: u32) -> u32 {
        match self.plan {
            Plan::Lanes { even, odd, inverse } => {
                let even = (dst & LANES) * inverse + even;
                let odd = ((dst >> 8) & LANES) * inverse + odd;
                let even = ((even + ((even >> 8) & LANES)) >> 8) & LANES;
                let odd = ((odd + ((odd >> 8) & LANES)) >> 8) & LANES;
                even | (odd << 8)
            }
            Plan::Fields { format, color } => {
                let r = field(
                    dst,
                    format.red_mask_size,
                    format.red_shift,
                    color.r,
                    self.alpha,
                );
                let g = field(
                    dst,
                    format.green_mask_size,
                    format.green_shift,
                    color.g,
                    self.alpha,
                );
                let b = field(
                    dst,
                    format.blue_mask_size,
                    format.blue_shift,
                    color.b,
                    self.alpha,
                );
                r | g | b
            }
        }
    }
}

const fn byte_lanes(format: &PixelFormat) -> bool {
    format.red_mask_size == 8
        && format.green_mask_size == 8
        && format.blue_mask_size == 8
        && format.red_shift.is_multiple_of(8)
        && format.green_shift.is_multiple_of(8)
        && format.blue_shift.is_multiple_of(8)
}

/// Blend one channel in place, at the depth the format stores it.
const fn field(dst: u32, size: u8, shift: u8, src: u8, alpha: u8) -> u32 {
    let max = (1u32 << size) - 1;
    let d = (dst >> shift) & max;
    let s = (src as u32) >> (8 - size);
    let a = alpha as u32;
    div255(s * a + d * (255 - a)) << shift
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn div255_is_exact() {
        for x in 0..=65535u32 {
            let reference = (x as f64 / 255.0).round() as u32;
            assert_eq!(div255(x), reference, "{x}");
        }
    }

    #[test]
    fn mix_matches_reference_everywhere() {
        // Every source, destination and opacity there is: 16.7 million cases
        // against the formula in floating point.
        for alpha in 0..=255u32 {
            for src in 0..=255u32 {
                for dst in 0..=255u32 {
                    let exact = (src * alpha + dst * (255 - alpha)) as f64 / 255.0;
                    assert_eq!(
                        mix(src as u8, dst as u8, alpha as u8) as f64,
                        exact.round(),
                        "{src} over {dst} at {alpha}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_ends_are_exact() {
        for value in 0..=255u8 {
            for other in [0, 1, 127, 254, 255] {
                assert_eq!(mix(value, other, 255), value);
                assert_eq!(mix(value, other, 0), other);
            }
            // A colour over itself is itself at every opacity, which is what
            // makes a faded region seamless against its surroundings.
            for alpha in 0..=255u8 {
                assert_eq!(mix(value, value, alpha), value);
            }
        }
    }

    /// A cheap deterministic stream of test pixels.
    fn pixels() -> impl Iterator<Item = u32> {
        let mut state = 0x2545_f491u32;
        (0..20_000).map(move |_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        })
    }

    #[test]
    fn lanes_agree_with_one_channel_at_a_time() {
        let bgrx = PixelFormat {
            red_shift: 8,
            green_shift: 16,
            blue_shift: 24,
            ..PixelFormat::XRGB8888
        };
        for format in [PixelFormat::XRGB8888, bgrx] {
            assert!(byte_lanes(&format));
            let color = Color::rgb(0xb7, 0x41, 0x0e);
            for alpha in [0u8, 1, 77, 128, 254, 255] {
                let blend = Blend::new(&format, color, alpha);
                for dst in pixels() {
                    let out = blend.over(dst);
                    let channel = |shift: u8| (out >> shift) as u8;
                    let under = |shift: u8| (dst >> shift) as u8;
                    assert_eq!(
                        channel(format.red_shift),
                        mix(color.r, under(format.red_shift), alpha)
                    );
                    assert_eq!(
                        channel(format.green_shift),
                        mix(color.g, under(format.green_shift), alpha)
                    );
                    assert_eq!(
                        channel(format.blue_shift),
                        mix(color.b, under(format.blue_shift), alpha)
                    );
                }
            }
        }
    }

    #[test]
    fn narrow_channels_blend_at_their_own_depth() {
        // 5-6-5 in a 32-bit word.
        let format = PixelFormat {
            bits_per_pixel: 32,
            red_mask_size: 5,
            red_shift: 11,
            green_mask_size: 6,
            green_shift: 5,
            blue_mask_size: 5,
            blue_shift: 0,
        };
        assert!(!byte_lanes(&format));
        let white = Blend::new(&format, Color::WHITE, 255);
        let nothing = Blend::new(&format, Color::WHITE, 0);
        let half = Blend::new(&format, Color::WHITE, 128);
        for dst in pixels() {
            let dst = dst & 0xffff;
            assert_eq!(white.over(dst), 0xffff);
            assert_eq!(nothing.over(dst), dst);
            // No channel may spill into its neighbour.
            let out = half.over(dst);
            assert_eq!(out >> 16, 0);
            assert!(out & 0x1f >= dst & 0x1f && (out >> 11) >= (dst >> 11));
        }
    }
}

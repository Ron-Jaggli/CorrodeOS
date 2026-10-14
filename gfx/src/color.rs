//! Light, as numbers.
//!
//! A screen stores colours gamma-encoded: 128 is not half as bright as 255,
//! it is about a fifth. Lighting is arithmetic on amounts of light, so the
//! engine works in **linear** values and only encodes on the way out. Getting
//! this wrong is why a lit scene looks muddy and its highlights look chalky.
//!
//! Linear values are Q12 -- 4096 is the brightest thing an ordinary screen can
//! show -- held in 16 bits, so there is room for things sixteen times brighter
//! than that. That headroom is what HDR means here: a lamp and a sheet of
//! paper are different numbers until the very last step, where the tone curve
//! decides how to fit both on the glass. Bloom needs that difference to know
//! what to glow.

use corrode_render::Color;

use crate::math::Fx;

/// White, in linear light.
pub(crate) const ONE: i32 = 4096;

/// The most a channel can hold: just under sixteen times white.
pub(crate) const MAX: i32 = 65535;

// The tables below are worked out by the compiler, in floating point that
// never reaches the machine.

const LN2: f64 = core::f64::consts::LN_2;

const fn ln(x: f64) -> f64 {
    let (mut m, mut e) = (x, 0i32);
    while m < 0.5 {
        m *= 2.0;
        e -= 1;
    }
    while m >= 1.0 {
        m *= 0.5;
        e += 1;
    }
    // ln m = 2 atanh((m - 1) / (m + 1)), and the argument is under a third.
    let t = (m - 1.0) / (m + 1.0);
    let (mut sum, mut term, mut k) = (0.0, t, 1);
    while k < 26 {
        sum += term / k as f64;
        term *= t * t;
        k += 2;
    }
    2.0 * sum + e as f64 * LN2
}

const fn exp(x: f64) -> f64 {
    let (mut r, mut k) = (x, 0i32);
    while r > 0.5 {
        r -= LN2;
        k += 1;
    }
    while r < -0.5 {
        r += LN2;
        k -= 1;
    }
    let (mut sum, mut term, mut n) = (1.0, 1.0, 1);
    while n < 15 {
        term *= r / n as f64;
        sum += term;
        n += 1;
    }
    while k > 0 {
        sum *= 2.0;
        k -= 1;
    }
    while k < 0 {
        sum *= 0.5;
        k += 1;
    }
    sum
}

const fn pow(x: f64, power: f64) -> f64 {
    if x <= 0.0 {
        0.0
    } else {
        exp(power * ln(x))
    }
}

/// The sRGB decoding curve: an encoded 0..1 to linear 0..1.
const fn decode(encoded: f64) -> f64 {
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        pow((encoded + 0.055) / 1.055, 2.4)
    }
}

const fn encode(linear: f64) -> f64 {
    if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * pow(linear, 1.0 / 2.4) - 0.055
    }
}

/// An encoded byte to linear Q12.
#[allow(long_running_const_eval)]
pub(crate) static DECODE: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        table[i] = (decode(i as f64 / 255.0) * ONE as f64 + 0.5) as u16;
        i += 1;
    }
    table
};

/// Linear Q12, up to white, to an encoded byte.
#[allow(long_running_const_eval)]
pub(crate) static ENCODE: [u8; ONE as usize + 1] = {
    let mut table = [0u8; ONE as usize + 1];
    let mut i = 0;
    while i <= ONE as usize {
        table[i] = (encode(i as f64 / ONE as f64) * 255.0 + 0.5) as u8;
        i += 1;
    }
    table
};

/// A colour as linear light, Q12 a channel.
pub(crate) type Linear = [i32; 3];

pub(crate) fn linear(color: Color) -> Linear {
    [color.r, color.g, color.b].map(|c| DECODE[c as usize] as i32)
}

/// A colour's light at `intensity` times its own brightness.
pub(crate) fn light(color: Color, intensity: Fx) -> Linear {
    linear(color).map(|c| ((c as i64 * intensity.0.max(0) as i64) >> 16).min(1 << 24) as i32)
}

/// A packed `0x00RRGGBB` to linear.
#[inline]
pub(crate) fn linear_packed(rgb: u32) -> Linear {
    [
        DECODE[(rgb >> 16) as usize & 0xff] as i32,
        DECODE[(rgb >> 8) as usize & 0xff] as i32,
        DECODE[rgb as usize & 0xff] as i32,
    ]
}

/// Three channels of light in the low 48 bits of a word. The top 16 are left
/// for whoever is using the buffer.
#[inline]
pub(crate) fn pack(c: Linear) -> u64 {
    let clamp = |v: i32| v.clamp(0, MAX) as u64;
    clamp(c[0]) | clamp(c[1]) << 16 | clamp(c[2]) << 32
}

#[inline]
pub(crate) fn unpack(word: u64) -> Linear {
    [
        (word & 0xffff) as i32,
        (word >> 16 & 0xffff) as i32,
        (word >> 32 & 0xffff) as i32,
    ]
}

/// The low 48 bits of a word: its light, without what else is kept there.
pub(crate) const RGB: u64 = 0xffff_ffff_ffff;

/// How bright a colour looks.
#[inline]
pub(crate) fn luminance(c: Linear) -> i32 {
    (c[0] * 54 + c[1] * 183 + c[2] * 19) >> 8
}

/// How a range of light with no upper limit is fitted onto a screen that has
/// one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tonemap {
    /// The filmic curve most engines use: contrast in the middle, and
    /// highlights that roll off towards white instead of hitting it.
    Filmic,
    /// `x / (1 + x)`. Never clips, and never quite reaches white either.
    Reinhard,
    /// No curve: anything brighter than white is white. This is what turning
    /// HDR off looks like.
    Clip,
}

impl Tonemap {
    /// Linear Q12 of any size to linear Q12 no brighter than white.
    #[inline]
    pub(crate) fn apply(self, x: i64) -> i32 {
        let one = ONE as i64;
        let x = x.clamp(0, 1 << 20);
        match self {
            // Narkowicz's fit to the ACES curve:
            // x (2.51 x + 0.03) / (x (2.43 x + 0.59) + 0.14).
            Self::Filmic => {
                let top = x * (251 * x + 3 * one);
                let bottom = x * (243 * x + 59 * one) + 14 * one * one;
                ((top * one + bottom / 2) / bottom).min(one) as i32
            }
            Self::Reinhard => (x * one / (one + x)) as i32,
            Self::Clip => x.min(one) as i32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_are_the_srgb_curve() {
        for i in 0..256 {
            let e = i as f64 / 255.0;
            let exact = if e <= 0.04045 {
                e / 12.92
            } else {
                ((e + 0.055) / 1.055).powf(2.4)
            };
            assert_eq!(DECODE[i], (exact * 4096.0).round() as u16, "decode {i}");
        }
        for i in 0..=4096 {
            let l = i as f64 / 4096.0;
            let exact = if l <= 0.0031308 {
                l * 12.92
            } else {
                1.055 * l.powf(1.0 / 2.4) - 0.055
            };
            assert_eq!(ENCODE[i], (exact * 255.0).round() as u8, "encode {i}");
        }
    }

    #[test]
    fn decoding_then_encoding_is_within_a_step() {
        // Twelve bits are fewer than the curve wants in the darkest corner:
        // a couple of the lowest codes share a linear value.
        for i in 0..256usize {
            let back = ENCODE[DECODE[i] as usize] as i32;
            assert!((back - i as i32).abs() <= 1, "{i} came back as {back}");
        }
    }

    #[test]
    fn the_filmic_curve_matches_its_formula() {
        for x in (0..65536).step_by(37) {
            let v = x as f64 / 4096.0;
            let exact = (v * (2.51 * v + 0.03)) / (v * (2.43 * v + 0.59) + 0.14);
            let got = Tonemap::Filmic.apply(x) as f64 / 4096.0;
            assert!((got - exact.min(1.0)).abs() < 5e-4, "{v}: {got} vs {exact}");
        }
        assert_eq!(Tonemap::Clip.apply(9000), 4096);
        assert_eq!(Tonemap::Reinhard.apply(4096), 2048);
    }

    #[test]
    fn packing_keeps_three_channels_apart() {
        assert_eq!(unpack(pack([1, 2, 3])), [1, 2, 3]);
        assert_eq!(unpack(pack([-5, 70000, 65535])), [0, 65535, 65535]);
        assert_eq!(pack([65535; 3]) >> 48, 0);
    }
}

//! Fixed-point maths for three dimensions.
//!
//! Everything here is integer arithmetic, for the reason `render::anim` gives:
//! the kernel does not save vector state, so a float in ring 0 does not survive
//! a context switch. Positions and lengths are Q16.16 ([`Fx`]) -- 1/65536 of a
//! unit, out to 32768 units either way. Unit vectors that only the engine sees
//! are Q14, so three of them multiplied together still fit a machine word.
//!
//! An [`Angle`] is a fraction of a turn rather than a number of radians. That
//! makes wrapping free, and it makes a rotation a formula of time with no
//! state: [`Angle::spin`] asks the clock, not a counter.

use core::ops::{Add, Neg, Sub};

pub use corrode_render::anim::Fx;

/// A number written the ordinary way: `fx(1.5)`.
///
/// For constants. In a `const` the conversion happens in the compiler and no
/// floating point reaches the machine; called at run time it works, slowly,
/// through the soft-float routines.
pub const fn fx(value: f64) -> Fx {
    let bias = if value < 0.0 { -0.5 } else { 0.5 };
    Fx((value * 65536.0 + bias) as i32)
}

pub(crate) const fn saturate(value: i64) -> i32 {
    if value > i32::MAX as i64 {
        i32::MAX
    } else if value < i32::MIN as i64 {
        i32::MIN
    } else {
        value as i32
    }
}

/// Q16.16 times Q16.16.
#[inline]
pub(crate) const fn mul(a: i32, b: i32) -> i32 {
    saturate((a as i64 * b as i64) >> 16)
}

// ---------------------------------------------------------------------------
// Angles
// ---------------------------------------------------------------------------

/// An angle, as a fraction of a full turn: `1 << 32` would be once round.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Angle(pub u32);

/// pi / 2 in Q30.
const HALF_PI: i64 = 1_686_629_713;

impl Angle {
    pub const ZERO: Self = Self(0);

    pub const fn degrees(degrees: i32) -> Self {
        Self((((degrees as i64) << 32) / 360) as u32)
    }

    pub const fn turns(turns: Fx) -> Self {
        Self(((turns.0 as i64) << 16) as u32)
    }

    pub const fn radians(radians: Fx) -> Self {
        // 2^32 / (2 pi), applied to a Q16.16 value.
        Self(((radians.0 as i64 * 683_565_276) >> 16) as u32)
    }

    /// Once round every `period_ms`, as a function of the clock.
    ///
    /// Nothing is stepped: the same instant always gives the same angle, and
    /// a frame that is late lands exactly where it should have.
    pub const fn spin(now_ns: u64, period_ms: u32) -> Self {
        let period = if period_ms == 0 { 1 } else { period_ms } as u64 * 1_000_000;
        let phase = now_ns % period;
        Self((((phase as u128) << 32) / period as u128) as u32)
    }

    /// Swing between `-amplitude` and `+amplitude`, once every `period_ms`.
    pub const fn sway(now_ns: u64, period_ms: u32, amplitude: Angle) -> Self {
        let (sin, _) = sin_cos_q30(Self::spin(now_ns, period_ms).0);
        Self(((amplitude.0 as i32 as i64 * sin) >> 30) as u32)
    }

    pub const fn plus(self, other: Angle) -> Self {
        Self(self.0.wrapping_add(other.0))
    }

    pub const fn negated(self) -> Self {
        Self(self.0.wrapping_neg())
    }

    /// `(sin, cos)`.
    pub const fn sin_cos(self) -> (Fx, Fx) {
        let (sin, cos) = sin_cos_q30(self.0);
        (Fx(((sin + (1 << 13)) >> 14) as i32), Fx(((cos + (1 << 13)) >> 14) as i32))
    }

    pub const fn sin(self) -> Fx {
        self.sin_cos().0
    }

    pub const fn cos(self) -> Fx {
        self.sin_cos().1
    }
}

/// `(sin, cos)` in Q30 of a fraction of a turn.
///
/// The top two bits pick the quadrant and the rest is a position inside it,
/// where a Taylor series is short. The same construction as `render::anim`,
/// starting from turns so there is no reduction to do.
pub(crate) const fn sin_cos_q30(turn: u32) -> (i64, i64) {
    const fn m(a: i64, b: i64) -> i64 {
        (a * b) >> 30
    }
    let quadrant = turn >> 30;
    let x = m((turn & 0x3fff_ffff) as i64, HALF_PI);
    let x2 = m(x, x);

    let one = 1i64 << 30;
    let mut sin = one;
    let divisors = [156, 110, 72, 42, 20, 6];
    let mut i = 0;
    while i < divisors.len() {
        sin = one - m(x2, sin) / divisors[i];
        i += 1;
    }
    let sin = m(x, sin);

    let mut cos = one;
    let divisors = [182, 132, 90, 56, 30, 12, 2];
    let mut i = 0;
    while i < divisors.len() {
        cos = one - m(x2, cos) / divisors[i];
        i += 1;
    }

    match quadrant {
        0 => (sin, cos),
        1 => (cos, -sin),
        2 => (-sin, -cos),
        _ => (-cos, sin),
    }
}

// ---------------------------------------------------------------------------
// Square roots
// ---------------------------------------------------------------------------

/// Exact integer square root, for building tables at compile time.
pub(crate) const fn isqrt(value: u64) -> u64 {
    let mut root = 0u64;
    let mut bit = 1u64 << 62;
    while bit > value {
        bit >>= 2;
    }
    let mut rest = value;
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

/// `2^16 / sqrt(M)` for `M` in 1..4, by its top bits.
const RSQRT: [u16; 192] = {
    let mut table = [0u16; 192];
    let mut i = 0;
    while i < 192 {
        // M is (i + 64.5) / 64, so the entry is sqrt(2^39 / (2i + 129)).
        table[i] = (isqrt((1u64 << 59) / (2 * i as u64 + 129)) >> 10) as u16;
        i += 1;
    }
    table
};

/// `1 / sqrt(n)` for `n > 0`, as `(y, half)`: `y * 2^(half - 61)`, with `y`
/// in Q30 between a half and one.
///
/// Normalising a vector is the commonest thing a pixel asks for, and a real
/// square root is thirty-two iterations. This is a table lookup and two
/// Newton steps, all multiplies, good to a few parts in a billion.
#[inline]
pub(crate) fn rsqrt(n: u64) -> (i64, u32) {
    let shift = n.leading_zeros() & !1;
    let m = n << shift;
    let mq = (m >> 32) as i64;
    let mut y = (RSQRT[(m >> 56) as usize - 64] as i64) << 14;
    for _ in 0..2 {
        let t = (mq * ((y * y) >> 30)) >> 30;
        y = (y * ((3i64 << 30) - t)) >> 31;
    }
    (y, shift / 2)
}

/// `sqrt(n)`, rounded down to within a few parts in a billion.
#[inline]
pub(crate) fn sqrt(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let shift = n.leading_zeros() & !1;
    let (y, half) = rsqrt(n);
    let mq = ((n << shift) >> 32) as i64;
    ((mq * y) >> (29 + half)) as u64
}

/// Scale `v` to length `2^bits`. A zero vector stays zero.
#[inline]
pub(crate) fn normalize(v: [i32; 3], bits: u32) -> [i32; 3] {
    let square = |c: i32| (c as i64 * c as i64) as u64;
    let n = square(v[0]) + square(v[1]) + square(v[2]);
    if n == 0 {
        return [0; 3];
    }
    let (y, half) = rsqrt(n);
    let shift = 61 - bits - half;
    let round = 1i64 << (shift - 1);
    v.map(|c| ((c as i64 * y + round) >> shift) as i32)
}

#[inline]
pub(crate) fn dot(a: [i32; 3], b: [i32; 3]) -> i64 {
    a[0] as i64 * b[0] as i64 + a[1] as i64 * b[1] as i64 + a[2] as i64 * b[2] as i64
}

/// `log2(x) * 256` for `x >= 1`, straight between powers of two. Out by at
/// most 22 (0.086), which is plenty for choosing a mip level.
#[inline]
pub(crate) fn log2_q8(x: u64) -> i32 {
    let x = x.max(1);
    let top = 63 - x.leading_zeros();
    let fraction = if top >= 8 {
        (x >> (top - 8)) & 0xff
    } else {
        (x << (8 - top)) & 0xff
    };
    (top as i32) * 256 + fraction as i32
}

/// `log2(x) * 256` for a 128-bit `x >= 1`.
pub(crate) fn log2_q8_wide(x: u128) -> i32 {
    let x = x.max(1);
    let top = 127 - x.leading_zeros();
    if top < 64 {
        log2_q8(x as u64)
    } else {
        log2_q8((x >> (top - 63)) as u64) + (top as i32 - 63) * 256
    }
}

// ---------------------------------------------------------------------------
// Vectors
// ---------------------------------------------------------------------------

/// A point or a direction in three dimensions.
///
/// The world is right-handed with `y` up: `x` to the right, `z` towards a
/// viewer who is looking down `-z`. That is what model files use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vec3 {
    pub x: Fx,
    pub y: Fx,
    pub z: Fx,
}

impl Vec3 {
    pub const ZERO: Self = Self::new(0, 0, 0);
    pub const X: Self = Self::new(1, 0, 0);
    pub const Y: Self = Self::new(0, 1, 0);
    pub const Z: Self = Self::new(0, 0, 1);

    /// Whole units.
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self {
            x: Fx::from_int(x),
            y: Fx::from_int(y),
            z: Fx::from_int(z),
        }
    }

    pub const fn fx(x: Fx, y: Fx, z: Fx) -> Self {
        Self { x, y, z }
    }

    /// Written the ordinary way: `Vec3::f(0.0, 1.5, -3.0)`. See [`fx`].
    pub const fn f(x: f64, y: f64, z: f64) -> Self {
        Self {
            x: fx(x),
            y: fx(y),
            z: fx(z),
        }
    }

    pub(crate) const fn raw(self) -> [i32; 3] {
        [self.x.0, self.y.0, self.z.0]
    }

    pub(crate) const fn from_raw(v: [i32; 3]) -> Self {
        Self {
            x: Fx(v[0]),
            y: Fx(v[1]),
            z: Fx(v[2]),
        }
    }

    pub fn dot(self, other: Self) -> Fx {
        Fx(saturate(dot(self.raw(), other.raw()) >> 16))
    }

    pub fn cross(self, other: Self) -> Self {
        let (a, b) = (self.raw(), other.raw());
        let c = |i: usize, j: usize| {
            saturate((a[i] as i64 * b[j] as i64 - a[j] as i64 * b[i] as i64) >> 16)
        };
        Self::from_raw([c(1, 2), c(2, 0), c(0, 1)])
    }

    pub fn scale(self, by: Fx) -> Self {
        Self::from_raw(self.raw().map(|c| mul(c, by.0)))
    }

    pub fn length(self) -> Fx {
        let square = |c: i32| (c as i64 * c as i64) as u64;
        let [x, y, z] = self.raw();
        Fx(saturate(sqrt(square(x) + square(y) + square(z)) as i64))
    }

    /// The same direction, one unit long.
    pub fn normalized(self) -> Self {
        Self::from_raw(normalize(self.raw(), 16))
    }

    /// `self` at `t = 0`, `other` at `t = 1`.
    pub fn lerp(self, other: Self, t: Fx) -> Self {
        self + (other - self).scale(t)
    }
}

impl Add for Vec3 {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        let (a, b) = (self.raw(), other.raw());
        Self::from_raw([0, 1, 2].map(|i| a[i].saturating_add(b[i])))
    }
}

impl Sub for Vec3 {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        let (a, b) = (self.raw(), other.raw());
        Self::from_raw([0, 1, 2].map(|i| a[i].saturating_sub(b[i])))
    }
}

impl Neg for Vec3 {
    type Output = Self;
    fn neg(self) -> Self {
        Self::from_raw(self.raw().map(i32::saturating_neg))
    }
}

// ---------------------------------------------------------------------------
// Transforms
// ---------------------------------------------------------------------------

/// Where something is, which way it faces and how big it is.
///
/// Built outwards from a position: `Transform::at(p).rotate_y(a).scale(s)`
/// places a model at `p`, turned by `a`, `s` times its own size. Each call
/// applies to the model before the ones written to its left, which is the
/// order they read in.
///
/// Like an `Animation`, a `Transform` is plain data with no history. One that
/// moves is one that is built again from the clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transform {
    /// Rows of the linear part, Q16.16.
    pub(crate) m: [[i32; 3]; 3],
    pub(crate) t: [i32; 3],
}

const ONE: i32 = 1 << 16;

impl Transform {
    pub const IDENTITY: Self = Self {
        m: [[ONE, 0, 0], [0, ONE, 0], [0, 0, ONE]],
        t: [0; 3],
    };

    pub const fn at(position: Vec3) -> Self {
        Self {
            m: Self::IDENTITY.m,
            t: position.raw(),
        }
    }

    /// `self` applied after `inner`.
    pub fn after(&self, inner: &Transform) -> Self {
        let mut m = [[0; 3]; 3];
        for (row, out) in m.iter_mut().enumerate() {
            for (col, cell) in out.iter_mut().enumerate() {
                let sum: i64 = (0..3)
                    .map(|k| self.m[row][k] as i64 * inner.m[k][col] as i64)
                    .sum();
                *cell = saturate(sum >> 16);
            }
        }
        Self {
            m,
            t: self.point(inner.t),
        }
    }

    fn with(self, m: [[i32; 3]; 3]) -> Self {
        self.after(&Self { m, t: [0; 3] })
    }

    pub fn rotate_x(self, angle: Angle) -> Self {
        let (s, c) = angle.sin_cos();
        self.with([[ONE, 0, 0], [0, c.0, -s.0], [0, s.0, c.0]])
    }

    pub fn rotate_y(self, angle: Angle) -> Self {
        let (s, c) = angle.sin_cos();
        self.with([[c.0, 0, s.0], [0, ONE, 0], [-s.0, 0, c.0]])
    }

    pub fn rotate_z(self, angle: Angle) -> Self {
        let (s, c) = angle.sin_cos();
        self.with([[c.0, -s.0, 0], [s.0, c.0, 0], [0, 0, ONE]])
    }

    pub fn scale(self, by: Fx) -> Self {
        self.stretch(Vec3::fx(by, by, by))
    }

    /// A different scale along each of the model's own axes.
    pub fn stretch(self, by: Vec3) -> Self {
        self.with([[by.x.0, 0, 0], [0, by.y.0, 0], [0, 0, by.z.0]])
    }

    pub fn translate(self, by: Vec3) -> Self {
        self.after(&Self::at(by))
    }

    /// Where a point ends up.
    #[inline]
    pub(crate) fn point(&self, p: [i32; 3]) -> [i32; 3] {
        [0, 1, 2].map(|row| saturate((dot(self.m[row], p) >> 16) + self.t[row] as i64))
    }

    /// Where a direction ends up: the same, without the translation.
    #[inline]
    pub(crate) fn direction(&self, v: [i32; 3]) -> [i32; 3] {
        [0, 1, 2].map(|row| saturate(dot(self.m[row], v) >> 16))
    }

    pub fn apply(&self, point: Vec3) -> Vec3 {
        Vec3::from_raw(self.point(point.raw()))
    }

    /// The transform that undoes this one, provided it only rotates and
    /// moves -- which is what a camera does.
    pub(crate) fn inverse_rigid(&self) -> Self {
        let mut m = [[0; 3]; 3];
        for (row, out) in m.iter_mut().enumerate() {
            for (col, cell) in out.iter_mut().enumerate() {
                *cell = self.m[col][row];
            }
        }
        let back = Self { m, t: [0; 3] };
        Self {
            m,
            t: back.direction(self.t).map(|c| -c),
        }
    }

    /// The cofactors of the linear part: its rows crossed with each other.
    fn cofactors(&self) -> [[i64; 3]; 3] {
        let cross = |a: [i32; 3], b: [i32; 3]| {
            let c = |i: usize, j: usize| a[i] as i64 * b[j] as i64 - a[j] as i64 * b[i] as i64;
            [c(1, 2), c(2, 0), c(0, 1)]
        };
        [
            cross(self.m[1], self.m[2]),
            cross(self.m[2], self.m[0]),
            cross(self.m[0], self.m[1]),
        ]
    }

    /// Whether this turns things inside out, as a mirror does: a left hand
    /// into a right one, and anticlockwise into clockwise.
    pub(crate) fn mirrors(&self) -> bool {
        let across = self.cofactors()[0];
        let determinant: i128 = (0..3).map(|k| self.m[0][k] as i128 * across[k] as i128).sum();
        determinant < 0
    }

    /// The matrix that carries surface normals.
    ///
    /// Not the linear part itself: stretch a sphere and its normals lean the
    /// other way. It is the cofactor matrix -- turned over if the transform
    /// mirrors, since a mirror reverses which side of a surface is out. Only
    /// the direction of what comes out matters, so it is scaled to keep the
    /// most bits it can.
    pub(crate) fn normals(&self) -> [[i32; 3]; 3] {
        let mut rows = self.cofactors();
        if self.mirrors() {
            rows = rows.map(|row| row.map(|c| -c));
        }
        let largest = rows
            .iter()
            .flatten()
            .map(|c| c.unsigned_abs())
            .max()
            .unwrap_or(0);
        // Bring the largest entry to about 2^24.
        let bits = 64 - largest.leading_zeros() as i32;
        rows.map(|row| {
            row.map(|c| {
                if bits > 24 {
                    (c >> (bits - 24)) as i32
                } else {
                    (c << (24 - bits)) as i32
                }
            })
        })
    }
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real(value: Fx) -> f64 {
        value.0 as f64 / 65536.0
    }

    #[test]
    fn sin_and_cos_match_reference() {
        let mut turn = 0u32;
        loop {
            let angle = turn as f64 / 4294967296.0 * core::f64::consts::TAU;
            let (s, c) = sin_cos_q30(turn);
            assert!((s as f64 / (1u64 << 30) as f64 - angle.sin()).abs() < 1e-8);
            assert!((c as f64 / (1u64 << 30) as f64 - angle.cos()).abs() < 1e-8);
            let Some(next) = turn.checked_add(1_234_567) else {
                break;
            };
            turn = next;
        }
    }

    #[test]
    fn angles_convert() {
        assert_eq!(Angle::degrees(90).0, 1 << 30);
        assert_eq!(Angle::degrees(-90).0, 3 << 30);
        assert_eq!(Angle::turns(fx(0.5)).0, 1 << 31);
        let quarter = Angle::radians(fx(core::f64::consts::FRAC_PI_2));
        assert!((quarter.0 as i64 - (1 << 30)).abs() < 1 << 16);
        assert_eq!(Angle::spin(250_000_000, 1000).0, 1 << 30);
        assert_eq!(Angle::spin(1_250_000_000, 1000).0, 1 << 30);
    }

    #[test]
    fn square_roots_match_reference() {
        let mut n = 1u64;
        while n < u64::MAX / 3 {
            let exact = (n as f64).sqrt();
            let got = sqrt(n) as f64;
            assert!((got - exact).abs() <= 1.0 + exact * 1e-8, "sqrt {n}: {got}");
            assert_eq!(isqrt(n), exact.floor() as u64, "isqrt {n}");
            n = n * 3 + 7;
        }
    }

    #[test]
    fn normalizing_gives_unit_length_in_the_same_direction() {
        let cases = [
            [1, 0, 0],
            [3, -4, 12],
            [65536, 65536, 65536],
            [-70000, 12, 900_000],
            [i32::MAX, i32::MIN + 1, i32::MAX],
            [0, 0, 7],
        ];
        for v in cases {
            let length = v.iter().map(|c| (*c as f64).powi(2)).sum::<f64>().sqrt();
            for bits in [14, 16] {
                let unit = normalize(v, bits);
                for i in 0..3 {
                    let exact = v[i] as f64 / length * (1u32 << bits) as f64;
                    assert!((unit[i] as f64 - exact).abs() < 1.0, "{v:?} {bits}");
                }
            }
        }
        assert_eq!(normalize([0; 3], 16), [0; 3]);
    }

    #[test]
    fn logarithms_are_close_enough_to_pick_a_mip() {
        for x in [1u64, 2, 3, 255, 256, 1000, 123_456_789, u64::MAX] {
            let exact = (x as f64).log2() * 256.0;
            assert!((log2_q8(x) as f64 - exact).abs() < 23.0, "{x}");
        }
        let wide = 3u128 << 100;
        let exact = (wide as f64).log2() * 256.0;
        assert!((log2_q8_wide(wide) as f64 - exact).abs() < 23.0);
    }

    #[test]
    fn vectors_do_what_vectors_do() {
        let a = Vec3::f(1.0, 2.0, 3.0);
        let b = Vec3::f(-2.0, 0.5, 4.0);
        assert!((real(a.dot(b)) - 11.0).abs() < 1e-3);
        let c = a.cross(b);
        assert!((real(c.x) - 6.5).abs() < 1e-3);
        assert!((real(c.y) + 10.0).abs() < 1e-3);
        assert!((real(c.z) - 4.5).abs() < 1e-3);
        assert!((real(Vec3::new(3, 4, 12).length()) - 13.0).abs() < 1e-3);
        assert_eq!(Vec3::X.cross(Vec3::Y), Vec3::Z);
    }

    /// The same chain of transforms in floating point.
    #[test]
    fn transforms_compose_like_matrices() {
        let transform = Transform::at(Vec3::f(1.0, -2.0, 0.5))
            .rotate_y(Angle::degrees(30))
            .rotate_x(Angle::degrees(-50))
            .scale(fx(1.5));
        let got = transform.apply(Vec3::f(0.3, 0.7, -1.1));

        let (x, y, z) = (0.3 * 1.5, 0.7 * 1.5, -1.1 * 1.5);
        let a = (-50.0f64).to_radians();
        let (y, z) = (y * a.cos() - z * a.sin(), y * a.sin() + z * a.cos());
        let b = 30.0f64.to_radians();
        let (x, z) = (x * b.cos() + z * b.sin(), -x * b.sin() + z * b.cos());
        let want = [x + 1.0, y - 2.0, z + 0.5];

        for (got, want) in [got.x, got.y, got.z].into_iter().zip(want) {
            assert!((real(got) - want).abs() < 1e-3, "{} vs {want}", real(got));
        }
    }

    #[test]
    fn a_rigid_transform_inverts() {
        let transform = Transform::at(Vec3::f(4.0, 1.0, -9.0))
            .rotate_z(Angle::degrees(77))
            .rotate_y(Angle::degrees(-140));
        let point = Vec3::f(2.5, -3.0, 6.0);
        let back = transform.inverse_rigid().apply(transform.apply(point));
        for (got, want) in back.raw().into_iter().zip(point.raw()) {
            assert!((got - want).abs() < 40, "{got} vs {want}");
        }
    }

    #[test]
    fn normals_stay_square_to_a_stretched_surface() {
        // A surface along (1, 1, 0) has normal (1, -1, 0). Stretch x by four
        // and the surface runs along (4, 1, 0); its normal is (1, -4, 0).
        let transform = Transform::IDENTITY.stretch(Vec3::new(4, 1, 1));
        let m = transform.normals();
        let n = [0, 1, 2].map(|row| dot(m[row], [ONE, -ONE, 0]));
        let along = transform.direction([ONE, ONE, 0]);
        let across = n[0] * along[0] as i64 + n[1] * along[1] as i64;
        assert_eq!(across, 0);
        assert!(n[0] > 0 && n[1] < 0 && n[2] == 0);
    }

    #[test]
    fn normals_point_out_of_a_mirrored_surface_too() {
        let plain = Transform::IDENTITY.rotate_y(Angle::degrees(40));
        let mirror = plain.stretch(Vec3::new(1, 1, -1));
        assert!(!plain.mirrors() && mirror.mirrors());
        // A surface facing +y still faces +y, in a mirror or out of one.
        for transform in [plain, mirror] {
            let m = transform.normals();
            let n = [0, 1, 2].map(|row| dot(m[row], [0, ONE, 0]));
            assert!(n[1] > 0 && n[0] == 0 && n[2] == 0, "{n:?}");
        }
    }
}

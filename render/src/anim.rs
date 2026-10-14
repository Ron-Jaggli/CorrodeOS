//! Formula-based animation.
//!
//! An [`Animation`] is a closed-form function of time. Nothing steps it: there
//! is no integrator, no per-frame state and no task whose job is to tick.
//! Whoever is about to paint asks "where is this at time `t`" and gets the
//! answer the formula gives, which has three consequences worth having:
//!
//! * **Dropped frames cannot change the motion.** A stepped spring that misses
//!   a frame either takes one large step and goes unstable or runs slow. A
//!   sampled one is simply sampled less often, on exactly the same curve --
//!   which is what a slow machine needs.
//! * **Idle costs nothing.** Every animation knows the instant it ends, so the
//!   painter knows when there is nothing left to paint and can stop waking up.
//! * **It is plain data.** `Animation` is `Copy` and holds no pointers, like a
//!   `Command`. A client can describe a motion once, send it over IPC, and
//!   never be scheduled again while the compositor plays it.
//!
//! # No floating point
//!
//! Everything here is integer arithmetic. The kernel does not save SSE state
//! across a context switch, so a float in ring 0 would be corrupted by the
//! first preemption, and the soft-float fallback is slow exactly where a cheap
//! CPU can least afford it. Public values are Q16.16 ([`Fx`]); the maths inside
//! runs in Q30 so that rounding stays well under one Q16.16 step even after
//! the terms of a spring are multiplied together.

const ONE: i64 = 1 << 30;
const NS_PER_SECOND: u64 = 1_000_000_000;

/// ln 2 in Q30.
const LN2: i64 = 744_261_118;
/// pi / 2 in Q30.
const HALF_PI: i64 = 1_686_629_713;
/// 2^64 / (2 pi): turns per radian, with enough bits that reducing a large
/// angle loses nothing.
const INV_TAU_Q64: u64 = 2_935_890_503_282_001_226;
/// 2 pi in Q16.16.
const TAU_FX: i64 = 411_775;

/// A Q16.16 fixed-point number.
///
/// By convention an animated value is in **device units** -- pixels, or 0-255
/// for opacity -- so that "settled" can mean one fixed thing: within half a
/// unit of the target, the point past which the motion can no longer change
/// what is on screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Fx(pub i32);

impl Fx {
    pub const ZERO: Self = Self(0);
    pub const HALF: Self = Self(1 << 15);
    pub const ONE: Self = Self(1 << 16);

    pub const fn from_int(value: i32) -> Self {
        Self(saturate((value as i64) << 16))
    }

    /// `numerator / denominator`. A zero denominator yields zero rather than a
    /// panic, since this is reachable with values that arrived over IPC.
    pub const fn ratio(numerator: i32, denominator: i32) -> Self {
        if denominator == 0 {
            return Self::ZERO;
        }
        Self(saturate(((numerator as i64) << 16) / denominator as i64))
    }

    /// Nearest integer, halves rounding up.
    pub const fn round(self) -> i32 {
        ((self.0 as i64 + 0x8000) >> 16) as i32
    }
}

/// A rate of change in units per second, Q16.16.
///
/// Wider than [`Fx`] on purpose: a window crossing a 4K screen on a stiff
/// spring peaks well beyond the 32767 units/s an `Fx` could carry, and a
/// velocity clipped on the way into [`Animation::retarget`] is a visible kink.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Velocity(pub i64);

impl Velocity {
    pub const ZERO: Self = Self(0);

    /// Largest magnitude kept, about a million units per second. Bounds the
    /// intermediate products below; nothing on a screen moves this fast.
    const LIMIT: i64 = 1 << 36;

    pub const fn per_second(units: i32) -> Self {
        Self((units as i64) << 16)
    }
}

const fn saturate(value: i64) -> i32 {
    if value > i32::MAX as i64 {
        i32::MAX
    } else if value < i32::MIN as i64 {
        i32::MIN
    } else {
        value as i32
    }
}

// ---------------------------------------------------------------------------
// Q30 arithmetic
// ---------------------------------------------------------------------------

fn mul(a: i64, b: i64) -> i64 {
    ((a as i128 * b as i128) >> 30) as i64
}

/// `a / b`. Only used while setting an animation up, never per sample: a
/// 128-bit divide is a library call, and a multiply is one instruction.
fn div(a: i64, b: i64) -> i64 {
    if b == 0 {
        return 0;
    }
    (((a as i128) << 30) / b as i128) as i64
}

const fn to_q(value: Fx) -> i64 {
    (value.0 as i64) << 14
}

fn to_fx(q: i64) -> Fx {
    Fx(saturate((q + (1 << 13)) >> 14))
}

fn sqrt(q: i64) -> i64 {
    if q <= 0 {
        return 0;
    }
    // Bit-by-bit integer square root of `q << 30`, so the result is Q30.
    let value = (q as u128) << 30;
    let mut root = 0u128;
    let mut bit = 1u128 << 94;
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
    root as i64
}

/// `e^-x` for `x >= 0`.
///
/// Split `x = k ln2 + r`: `e^-r` is a short series because `r < ln 2`, and
/// `2^-k` is a shift.
fn exp_neg(x: i64) -> i64 {
    if x <= 0 {
        return ONE;
    }
    let k = x / LN2;
    if k >= 31 {
        return 0;
    }
    let r = x - k * LN2;

    // Horner form of the Taylor series to r^10; the first dropped term is
    // below 2^-30 for r < ln 2.
    let mut sum = ONE;
    let mut n = 10;
    while n >= 1 {
        sum = ONE - mul(r, sum) / n;
        n -= 1;
    }
    sum >> k
}

/// `(sin, cos)` of an angle in radians.
///
/// The angle is turned into a fraction of a turn first, against a 64-bit
/// reciprocal of 2 pi, so a spring that has been oscillating for minutes still
/// reduces exactly. What is left is a quarter turn, where the series is short.
fn sin_cos(angle: i64) -> (i64, i64) {
    let phase = ((angle as i128 * INV_TAU_Q64 as i128) >> 62) as u64 & 0xffff_ffff;
    let quadrant = phase >> 30;
    let x = mul((phase & 0x3fff_ffff) as i64, HALF_PI);
    let x2 = mul(x, x);

    // Taylor series in Horner form, to x^13 and x^14. A quarter turn is a
    // wide interval for a series, so these run long enough for the first
    // dropped term to be under 2^-30 at pi/2.

    let mut sin = ONE;
    for divisor in [156, 110, 72, 42, 20, 6] {
        sin = ONE - mul(x2, sin) / divisor;
    }
    let sin = mul(x, sin);

    let mut cos = ONE;
    for divisor in [182, 132, 90, 56, 30, 12, 2] {
        cos = ONE - mul(x2, cos) / divisor;
    }

    match quadrant {
        0 => (sin, cos),
        1 => (cos, -sin),
        2 => (-sin, -cos),
        _ => (-cos, sin),
    }
}

/// Nanoseconds to Q30 seconds, without a 128-bit divide.
fn seconds(ns: u64) -> i64 {
    let whole = (ns / NS_PER_SECOND).min(1 << 20);
    let part = ns % NS_PER_SECOND;
    ((whole << 30) + (part << 30) / NS_PER_SECOND) as i64
}

fn nanoseconds(t: i64) -> u64 {
    let t = t.max(0) as u64;
    (t >> 30) * NS_PER_SECOND + (((t & 0x3fff_ffff) * NS_PER_SECOND) >> 30)
}

// ---------------------------------------------------------------------------
// Easing curves
// ---------------------------------------------------------------------------

/// A fixed-duration curve from 0 to 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ease {
    /// `u`
    Linear,
    /// `3u^2 - 2u^3`: zero velocity at both ends.
    Smooth,
    /// `u^3`
    In,
    /// `1 - (1 - u)^3`
    Out,
    /// `4u^3` up to the midpoint, mirrored after it.
    InOut,
    /// A cubic Bezier through (0,0), (x1,y1), (x2,y2), (1,1) -- the CSS
    /// `cubic-bezier()` timing function. `x1` and `x2` are clamped to 0..1 so
    /// the curve is a function of time.
    Bezier { x1: Fx, y1: Fx, x2: Fx, y2: Fx },
}

impl Ease {
    /// CSS `ease`, the curve most toolkits default to.
    pub const STANDARD: Self = Self::Bezier {
        x1: Fx::ratio(1, 4),
        y1: Fx::ratio(1, 10),
        x2: Fx::ratio(1, 4),
        y2: Fx::ONE,
    };

    /// Progress and its derivative at `u` in 0..=1.
    fn eval(self, u: i64) -> (i64, i64) {
        let v = ONE - u;
        match self {
            Self::Linear => (u, ONE),
            Self::Smooth => (mul(mul(u, u), 3 * ONE - 2 * u), 6 * mul(u, v)),
            Self::In => (mul(mul(u, u), u), 3 * mul(u, u)),
            Self::Out => (ONE - mul(mul(v, v), v), 3 * mul(v, v)),
            Self::InOut => {
                if u < ONE / 2 {
                    (4 * mul(mul(u, u), u), 12 * mul(u, u))
                } else {
                    (ONE - 4 * mul(mul(v, v), v), 12 * mul(v, v))
                }
            }
            Self::Bezier { x1, y1, x2, y2 } => bezier(u, to_q(x1), to_q(y1), to_q(x2), to_q(y2)),
        }
    }
}

fn bezier(u: i64, x1: i64, y1: i64, x2: i64, y2: i64) -> (i64, i64) {
    let (x1, x2) = (x1.clamp(0, ONE), x2.clamp(0, ONE));

    // Polynomial form: x(s) = ((ax s + bx) s + cx) s, and likewise y.
    let cx = 3 * x1;
    let bx = 3 * (x2 - x1) - cx;
    let ax = ONE - cx - bx;
    let cy = 3 * y1;
    let by = 3 * (y2 - y1) - cy;
    let ay = ONE - cy - by;

    // Solve x(s) = u. With the control points clamped x is monotonic, so
    // bisection always converges and there is no derivative to go to zero
    // under a Newton step.
    let (mut low, mut high) = (0, ONE);
    for _ in 0..30 {
        let s = (low + high) / 2;
        if mul(mul(mul(ax, s) + bx, s) + cx, s) < u {
            low = s;
        } else {
            high = s;
        }
    }
    let s = (low + high) / 2;

    let y = mul(mul(mul(ay, s) + by, s) + cy, s);
    let dx = (mul(mul(3 * ax, s) + 2 * bx, s) + cx).max(ONE >> 10);
    let dy = mul(mul(3 * ay, s) + 2 * by, s) + cy;
    (y, div(dy, dx))
}

// ---------------------------------------------------------------------------
// Springs
// ---------------------------------------------------------------------------

/// A damped spring: `x'' = -w^2 (x - target) - 2 z w x'`.
///
/// Described by the two numbers that mean something to a person tuning one:
/// how fast (`w`, the undamped angular frequency in rad/s) and how bouncy
/// (`z`, the damping ratio -- below 1 overshoots, 1 is the fastest approach
/// with no overshoot, above 1 is sluggish).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spring {
    omega: Fx,
    zeta: Fx,
}

impl Spring {
    /// Quick, no overshoot. For things that follow input.
    pub const SNAPPY: Self = Self::from_response(250, Fx::ONE);
    /// Unhurried, no overshoot. For things that move a long way.
    pub const SMOOTH: Self = Self::from_response(500, Fx::ONE);
    /// Overshoots and comes back. For things that should feel physical.
    pub const BOUNCY: Self = Self::from_response(450, Fx::ratio(1, 2));

    /// `omega` is clamped to 0.25..=512 rad/s and `zeta` to 1/16..=8.
    ///
    /// The lower bound on `zeta` is what guarantees every spring ends: an
    /// undamped one never settles, and a painter waiting for it would never
    /// get to stop.
    pub const fn new(omega: Fx, zeta: Fx) -> Self {
        Self {
            omega: Fx(clamp(omega.0, 1 << 14, 512 << 16)),
            zeta: Fx(clamp(zeta.0, 1 << 12, 8 << 16)),
        }
    }

    /// A spring whose undamped period is `response_ms`.
    pub const fn from_response(response_ms: u32, zeta: Fx) -> Self {
        let ms = if response_ms == 0 { 1 } else { response_ms };
        Self::new(Fx(saturate(TAU_FX * 1000 / ms as i64)), zeta)
    }

    /// Work out every coefficient the closed form needs, once.
    fn solve(self) -> Solved {
        let w = to_q(self.omega);
        let z = to_q(self.zeta);
        let discriminant = ONE - mul(z, z);

        // Close enough to critical that the difference is under one Q16.16
        // step. Without this band the other two forms divide by a square root
        // that is heading for zero.
        if discriminant.abs() < ONE >> 12 {
            return Solved::Critical { w };
        }

        if discriminant > 0 {
            let s = sqrt(discriminant);
            let wd = mul(w, s);
            let zs = div(z, s);
            Solved::Under {
                decay: mul(z, w),
                wd,
                zs,
                inv_wd: div(ONE, wd),
                w_s: div(w, s),
                bound: sqrt(ONE + mul(zs, zs)),
            }
        } else {
            let s = sqrt(-discriminant);
            Solved::Over {
                slow: mul(w, z - s),
                fast: mul(w, z + s),
                a: div(z + s, 2 * s),
                b: -div(z - s, 2 * s),
                inv_2ws: div(ONE, 2 * mul(w, s)),
                w_2s: div(w, 2 * s),
            }
        }
    }
}

const fn clamp(value: i32, low: i32, high: i32) -> i32 {
    if value < low {
        low
    } else if value > high {
        high
    } else {
        value
    }
}

/// A spring's closed-form solution, as coefficients.
///
/// The equation is linear, so any motion is a sum of two basis responses:
/// `h`, released from one unit away at rest, and `k`, kicked from the target
/// at one unit per second. Position is `target + d0 h(t) + v0 k(t)`. Keeping
/// the two apart is what lets a spring start from any position *and* any
/// velocity with no division by the distance, which would be zero for a kick
/// that starts on the target.
#[derive(Clone, Copy, Debug)]
enum Solved {
    /// `z < 1`. With `E = e^(-z w t)`, `S, C = sin, cos(wd t)`:
    /// `h = E (C + zs S)`, `k = E S / wd`.
    Under {
        decay: i64,
        wd: i64,
        zs: i64,
        inv_wd: i64,
        w_s: i64,
        bound: i64,
    },
    /// `z = 1`. With `E = e^(-w t)`: `h = E (1 + w t)`, `k = E t`.
    Critical { w: i64 },
    /// `z > 1`. Two real decay rates: `h = a e1 + b e2`,
    /// `k = (e1 - e2) / (2 w s)`.
    Over {
        slow: i64,
        fast: i64,
        a: i64,
        b: i64,
        inv_2ws: i64,
        w_2s: i64,
    },
}

/// The two basis responses and their time derivatives.
struct Basis {
    h: i64,
    k: i64,
    dh: i64,
    dk: i64,
}

impl Solved {
    fn basis(&self, t: i64) -> Basis {
        match *self {
            Self::Under {
                decay,
                wd,
                zs,
                inv_wd,
                w_s,
                ..
            } => {
                let e = exp_neg(mul(decay, t));
                let (s, c) = sin_cos(mul(wd, t));
                let es = mul(e, s);
                Basis {
                    h: mul(e, c + mul(zs, s)),
                    k: mul(es, inv_wd),
                    dh: -mul(es, w_s),
                    dk: mul(e, c - mul(zs, s)),
                }
            }
            Self::Critical { w } => {
                let wt = mul(w, t);
                let e = exp_neg(wt);
                Basis {
                    h: mul(e, ONE + wt),
                    k: mul(e, t),
                    dh: -mul(mul(w, wt), e),
                    dk: mul(e, ONE - wt),
                }
            }
            Self::Over {
                slow,
                fast,
                a,
                b,
                inv_2ws,
                w_2s,
            } => {
                let e1 = exp_neg(mul(slow, t));
                let e2 = exp_neg(mul(fast, t));
                Basis {
                    h: mul(a, e1) + mul(b, e2),
                    k: mul(e1 - e2, inv_2ws),
                    dh: -mul(e1 - e2, w_2s),
                    dk: mul(b, e1) + mul(a, e2),
                }
            }
        }
    }

    /// An upper bound on the distance from the target at `t`, for a start
    /// `d0` away moving at `v0`. Decreasing from [`Self::decreasing_from`] on.
    fn envelope(&self, t: i64, d0: i64, v0: i64) -> i64 {
        match *self {
            Self::Under {
                decay,
                inv_wd,
                bound,
                ..
            } => mul(exp_neg(mul(decay, t)), mul(d0, bound) + mul(v0, inv_wd)),
            Self::Critical { w } => {
                let wt = mul(w, t);
                mul(exp_neg(wt), mul(d0, ONE + wt) + mul(v0, t))
            }
            Self::Over {
                slow,
                fast,
                a,
                b,
                inv_2ws,
                ..
            } => {
                let e1 = exp_neg(mul(slow, t));
                let e2 = exp_neg(mul(fast, t));
                mul(d0, mul(a, e1) + mul(b.abs(), e2)) + mul(v0, mul(e1, inv_2ws))
            }
        }
    }

    /// The critical envelope rises before it falls when the start is a kick;
    /// `t e^(-w t)` peaks at `1/w`. The other two fall from the outset.
    fn decreasing_from(&self) -> i64 {
        match *self {
            Self::Critical { w } => div(ONE, w),
            _ => 0,
        }
    }

    /// When the motion is within `rest` of the target for good.
    fn settle(&self, d0: i64, v0: i64, rest: i64) -> i64 {
        let (d0, v0) = (d0.abs(), v0.abs());
        if v0 == 0 && d0 <= rest {
            return 0;
        }

        // Longest animation allowed. The clamps on `Spring` keep every real
        // one far inside this; it is here so the search below is bounded.
        const LIMIT: i64 = 4096 * ONE;

        let mut low = self.decreasing_from();
        if self.envelope(low, d0, v0) <= rest {
            return low;
        }

        // Double outwards until the envelope is inside, then bisect to the
        // millisecond. Both rely only on the envelope decreasing.
        let mut step = ONE / 64;
        let mut high = low + step;
        while high < LIMIT && self.envelope(high, d0, v0) > rest {
            low = high;
            step *= 2;
            high += step;
        }
        while high - low > ONE / 1024 {
            let middle = low + (high - low) / 2;
            if self.envelope(middle, d0, v0) > rest {
                low = middle;
            } else {
                high = middle;
            }
        }
        high
    }
}

// ---------------------------------------------------------------------------
// Animation
// ---------------------------------------------------------------------------

/// One value moving from one place to another, as a function of time.
///
/// Times are nanoseconds on whatever monotonic clock the caller uses; only
/// differences are ever taken.
#[derive(Clone, Copy, Debug)]
pub struct Animation {
    start_ns: u64,
    duration_ns: u64,
    from: Fx,
    to: Fx,
    kind: Kind,
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Ease { ease: Ease, per_second: i64 },
    Spring { v0: i64, solved: Solved },
}

/// An animation's state at one instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub value: Fx,
    pub velocity: Velocity,
    /// The animation has reached its target and will not move again.
    pub done: bool,
}

impl Animation {
    /// How close to the target counts as arrived: half a device unit.
    const REST: i64 = ONE / 2;

    /// Something that is already where it is going.
    pub const fn rest(value: Fx) -> Self {
        Self {
            start_ns: 0,
            duration_ns: 0,
            from: value,
            to: value,
            kind: Kind::Ease {
                ease: Ease::Linear,
                per_second: 0,
            },
        }
    }

    /// Move along `ease` over `duration_ms`.
    pub fn ease(start_ns: u64, duration_ms: u32, from: Fx, to: Fx, ease: Ease) -> Self {
        let duration_ns = duration_ms.max(1) as u64 * 1_000_000;
        Self {
            start_ns,
            duration_ns,
            from,
            to,
            kind: Kind::Ease {
                ease,
                per_second: div(ONE, seconds(duration_ns)),
            },
        }
    }

    /// Release `spring` at `from`, already moving at `velocity`.
    ///
    /// The duration is not chosen: it is when this particular motion comes
    /// within half a unit of `to` and stays there.
    pub fn spring(start_ns: u64, from: Fx, to: Fx, velocity: Velocity, spring: Spring) -> Self {
        let solved = spring.solve();
        let v0 = velocity.0.clamp(-Velocity::LIMIT, Velocity::LIMIT) << 14;
        let settle = solved.settle(to_q(from) - to_q(to), v0, Self::REST);
        Self {
            start_ns,
            duration_ns: nanoseconds(settle),
            from,
            to,
            kind: Kind::Spring { v0, solved },
        }
    }

    /// Where this is at `now_ns`.
    ///
    /// A pure function: the same instant always gives the same answer, in any
    /// order, however many frames were or were not painted in between.
    pub fn sample(&self, now_ns: u64) -> Sample {
        let elapsed = now_ns.saturating_sub(self.start_ns);
        if elapsed >= self.duration_ns {
            return Sample {
                value: self.to,
                velocity: Velocity::ZERO,
                done: true,
            };
        }

        let t = seconds(elapsed);
        let (value, velocity) = match self.kind {
            Kind::Ease { ease, per_second } => {
                let (progress, slope) = ease.eval(mul(t, per_second).clamp(0, ONE));
                let span = to_q(self.to) - to_q(self.from);
                (
                    to_q(self.from) + mul(span, progress),
                    mul(mul(span, slope), per_second),
                )
            }
            Kind::Spring { v0, solved } => {
                let basis = solved.basis(t);
                let d0 = to_q(self.from) - to_q(self.to);
                (
                    to_q(self.to) + mul(d0, basis.h) + mul(v0, basis.k),
                    mul(d0, basis.dh) + mul(v0, basis.dk),
                )
            }
        };

        Sample {
            value: to_fx(value),
            velocity: Velocity((velocity + (1 << 13)) >> 14),
            done: false,
        }
    }

    /// Change destination mid-flight.
    ///
    /// The new spring starts from wherever this is and however fast it is
    /// going, both read off the formula, so the motion bends rather than
    /// jumping or stopping dead. This is the operation a desktop performs
    /// constantly -- a window dragged again before it has settled.
    pub fn retarget(&self, now_ns: u64, to: Fx, spring: Spring) -> Self {
        let now = self.sample(now_ns);
        Self::spring(now_ns, now.value, to, now.velocity, spring)
    }

    /// The instant after which [`Self::sample`] reports `done`.
    pub fn end_ns(&self) -> u64 {
        self.start_ns.saturating_add(self.duration_ns)
    }

    pub const fn target(&self) -> Fx {
        self.to
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(value: f64) -> i64 {
        (value * ONE as f64).round() as i64
    }

    fn f(q: i64) -> f64 {
        q as f64 / ONE as f64
    }

    fn fx(value: f64) -> Fx {
        Fx((value * 65536.0).round() as i32)
    }

    fn real(value: Fx) -> f64 {
        value.0 as f64 / 65536.0
    }

    fn ns(seconds: f64) -> u64 {
        (seconds * 1e9).round() as u64
    }

    #[test]
    fn exp_matches_reference() {
        let mut x = 0.0f64;
        while x < 25.0 {
            let error = (f(exp_neg(q(x))) - (-x).exp()).abs();
            assert!(error < 1e-8, "e^-{x} off by {error}");
            x += 0.00731;
        }
    }

    #[test]
    fn sin_cos_match_reference() {
        let mut angle = -50.0f64;
        while angle < 20_000.0 {
            let (s, c) = sin_cos(q(angle));
            // The input itself is only good to 2^-30, hence the tolerance.
            assert!((f(s) - angle.sin()).abs() < 1e-8, "sin {angle}");
            assert!((f(c) - angle.cos()).abs() < 1e-8, "cos {angle}");
            angle += if angle < 50.0 { 0.0137 } else { 17.31 };
        }
    }

    #[test]
    fn sqrt_matches_reference() {
        for value in [0.0001, 0.25, 0.75, 1.0, 2.0, 63.0, 1000.0] {
            assert!((f(sqrt(q(value))) - f64::sqrt(value)).abs() < 1e-8);
        }
    }

    /// Integrate the spring's differential equation numerically. Independent
    /// of the closed form under test: it knows the physics, not the solution.
    fn integrate(omega: f64, zeta: f64, from: f64, to: f64, v0: f64, t: f64) -> (f64, f64) {
        let accel = |x: f64, v: f64| -omega * omega * (x - to) - 2.0 * zeta * omega * v;
        let steps = (t * 20_000.0).ceil().max(1.0) as usize;
        let dt = t / steps as f64;
        let (mut x, mut v) = (from, v0);
        for _ in 0..steps {
            let (k1x, k1v) = (v, accel(x, v));
            let (k2x, k2v) = (
                v + 0.5 * dt * k1v,
                accel(x + 0.5 * dt * k1x, v + 0.5 * dt * k1v),
            );
            let (k3x, k3v) = (
                v + 0.5 * dt * k2v,
                accel(x + 0.5 * dt * k2x, v + 0.5 * dt * k2v),
            );
            let (k4x, k4v) = (v + dt * k3v, accel(x + dt * k3x, v + dt * k3v));
            x += dt / 6.0 * (k1x + 2.0 * k2x + 2.0 * k3x + k4x);
            v += dt / 6.0 * (k1v + 2.0 * k2v + 2.0 * k3v + k4v);
        }
        (x, v)
    }

    #[test]
    fn spring_matches_the_differential_equation() {
        // (omega, zeta, from, to, initial velocity): all three regimes, both
        // sides of the critical band, with and without a kick.
        let cases = [
            (14.0, 0.5, 0.0, 1000.0, 0.0),
            (14.0, 0.5, 300.0, -200.0, 4000.0),
            (25.0, 0.1, 0.0, 100.0, -900.0),
            (30.0, 0.99, 0.0, 1000.0, 0.0),
            (25.0, 1.0, 0.0, 1000.0, 0.0),
            (25.0, 1.0, 500.0, 500.0, 6000.0),
            (30.0, 1.01, 0.0, 1000.0, 0.0),
            (20.0, 2.5, 1000.0, 0.0, -3000.0),
            (8.0, 7.5, 0.0, 400.0, 0.0),
        ];

        for (omega, zeta, from, to, v0) in cases {
            let animation = Animation::spring(
                0,
                fx(from),
                fx(to),
                Velocity((v0 * 65536.0) as i64),
                Spring::new(fx(omega), fx(zeta)),
            );
            // Integrate the parameters the spring actually holds: Q16.16
            // cannot represent 0.99 exactly, and velocity is sensitive to it.
            let (omega, zeta) = (real(fx(omega)), real(fx(zeta)));
            for step in 1..40 {
                let t = step as f64 * 0.013;
                let sample = animation.sample(ns(t));
                if sample.done {
                    break;
                }
                let (x, v) = integrate(omega, zeta, from, to, v0, t);
                let got_v = sample.velocity.0 as f64 / 65536.0;
                assert!(
                    (real(sample.value) - x).abs() < 0.01,
                    "w={omega} z={zeta} t={t}: position {} vs {x}",
                    real(sample.value)
                );
                assert!(
                    (got_v - v).abs() < 0.01 + v.abs() * 1e-5,
                    "w={omega} z={zeta} t={t}: velocity {got_v} vs {v}"
                );
            }
        }
    }

    #[test]
    fn spring_ends_only_once_it_has_settled() {
        for (omega, zeta, v0) in [
            (14.0, 0.5, 0.0),
            (14.0, 0.0625, 0.0),
            (25.0, 1.0, 0.0),
            (25.0, 1.0, 50_000.0),
            (20.0, 3.0, -2000.0),
            (2.0, 1.0, 0.0),
        ] {
            let animation = Animation::spring(
                0,
                fx(0.0),
                fx(800.0),
                Velocity((v0 * 65536.0) as i64),
                Spring::new(fx(omega), fx(zeta)),
            );
            let end = animation.end_ns() as f64 / 1e9;
            assert!(
                end > 0.0 && end < 4000.0,
                "w={omega} z={zeta}: ends at {end}"
            );

            // From the end onwards the true motion stays inside half a unit,
            // so snapping to the target there is invisible.
            for step in 0..20 {
                let t = end * (1.0 + step as f64 * 0.05);
                let (x, _) = integrate(omega, zeta, 0.0, 800.0, v0, t);
                assert!((x - 800.0).abs() <= 0.5, "w={omega} z={zeta} t={t}: {x}");
            }
            assert!(animation.sample(animation.end_ns()).done);
            assert_eq!(animation.sample(animation.end_ns()).value, fx(800.0));
        }
    }

    #[test]
    fn spring_at_rest_on_target_takes_no_time() {
        let animation = Animation::spring(77, fx(10.0), fx(10.0), Velocity::ZERO, Spring::BOUNCY);
        assert!(animation.sample(77).done);
    }

    #[test]
    fn only_an_underdamped_spring_overshoots() {
        let peak = |spring| {
            let animation = Animation::spring(0, fx(0.0), fx(100.0), Velocity::ZERO, spring);
            (0..3000)
                .map(|ms| animation.sample(ms * 1_000_000).value)
                .max()
                .unwrap()
        };
        assert!(peak(Spring::BOUNCY) > fx(110.0));
        assert!(peak(Spring::SNAPPY) <= fx(100.0));
        assert!(peak(Spring::new(fx(20.0), fx(2.0))) <= fx(100.0));
    }

    #[test]
    fn retarget_keeps_position_and_velocity() {
        let first = Animation::spring(0, fx(0.0), fx(1000.0), Velocity::ZERO, Spring::BOUNCY);
        let at = ns(0.12);
        let before = first.sample(at);
        let second = first.retarget(at, fx(-300.0), Spring::SNAPPY);
        let after = second.sample(at);

        assert!(!before.done && before.velocity.0 > 0);
        assert!((before.value.0 - after.value.0).abs() <= 1);
        assert!((before.velocity.0 - after.velocity.0).abs() <= 4);

        // Still heading the old way a moment later: it bends, it does not jump.
        assert!(second.sample(at + ns(0.005)).value > before.value);
        assert_eq!(second.sample(second.end_ns()).value, fx(-300.0));
    }

    #[test]
    fn sampling_is_stateless() {
        let animation = Animation::spring(0, fx(0.0), fx(640.0), Velocity::ZERO, Spring::BOUNCY);
        let late = animation.sample(ns(0.4));
        let early = animation.sample(ns(0.1));
        assert_ne!(late, early);
        assert_eq!(animation.sample(ns(0.4)), late);
        assert_eq!(animation.sample(ns(0.1)), early);
    }

    #[test]
    fn eases_run_from_zero_to_one() {
        for ease in [
            Ease::Linear,
            Ease::Smooth,
            Ease::In,
            Ease::Out,
            Ease::InOut,
            Ease::STANDARD,
        ] {
            assert!(f(ease.eval(0).0).abs() < 1e-6, "{ease:?} at 0");
            assert!((f(ease.eval(ONE).0) - 1.0).abs() < 1e-6, "{ease:?} at 1");

            // The reported slope is the slope of the curve.
            let mut u = 0.05;
            while u < 0.95 {
                let h = 1e-4;
                let numeric = (f(ease.eval(q(u + h)).0) - f(ease.eval(q(u - h)).0)) / (2.0 * h);
                let slope = f(ease.eval(q(u)).1);
                assert!((slope - numeric).abs() < 1e-3, "{ease:?} at {u}");
                u += 0.05;
            }
        }
    }

    #[test]
    fn bezier_matches_reference() {
        // Solve the same curve in floating point by bisection.
        let reference = |u: f64| {
            // Q16.16 holds 0.1 as 0.09999, and the curve is the one it holds.
            let (x1, y1, x2, y2) = (0.25, real(Fx::ratio(1, 10)), 0.25, 1.0);
            let curve = |a: f64, b: f64, s: f64| {
                3.0 * (1.0 - s).powi(2) * s * a + 3.0 * (1.0 - s) * s * s * b + s.powi(3)
            };
            let (mut low, mut high) = (0.0, 1.0);
            for _ in 0..60 {
                let s = (low + high) / 2.0;
                if curve(x1, x2, s) < u {
                    low = s
                } else {
                    high = s
                }
            }
            curve(y1, y2, low)
        };
        for step in 0..=100 {
            let u = step as f64 / 100.0;
            assert!((f(Ease::STANDARD.eval(q(u)).0) - reference(u)).abs() < 1e-6);
        }
    }

    #[test]
    fn ease_animation_hits_its_endpoints_and_midpoint() {
        let animation = Animation::ease(1_000, 200, fx(10.0), fx(110.0), Ease::Smooth);
        assert_eq!(animation.sample(0).value, fx(10.0));
        assert_eq!(animation.sample(1_000).value, fx(10.0));
        assert!((real(animation.sample(1_000 + ns(0.1)).value) - 60.0).abs() < 0.001);
        // Smoothstep's peak slope is 1.5x the average: 1.5 * 100 / 0.2s.
        let velocity = animation.sample(1_000 + ns(0.1)).velocity.0 as f64 / 65536.0;
        assert!((velocity - 750.0).abs() < 0.01);
        assert!(animation.sample(1_000 + ns(0.2)).done);
        assert_eq!(animation.sample(u64::MAX).value, fx(110.0));
    }

    #[test]
    fn fx_rounds_to_nearest() {
        assert_eq!(fx(2.5).round(), 3);
        assert_eq!(fx(2.49).round(), 2);
        assert_eq!(fx(-2.5).round(), -2);
        assert_eq!(fx(-2.51).round(), -3);
        assert_eq!(Fx::ratio(1, 0), Fx::ZERO);
    }
}

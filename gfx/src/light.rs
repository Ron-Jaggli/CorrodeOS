//! Light on surfaces.
//!
//! The rasteriser left two words a pixel saying what is there. This turns
//! them into how bright it is. Because it works from those words and not
//! from triangles, a scene with a million triangles and one with a dozen
//! cost the same to light.
//!
//! Four things add up to a pixel's light:
//!
//! * **The sun**, by how squarely the surface faces it, and not at all where
//!   the shadow map says something is in the way.
//! * **The sky**, from every direction at once, which is what fills shadows
//!   in. Less of it reaches into corners -- that is ambient occlusion.
//! * **Lamps**, fading with distance.
//! * **Bounce**: light that has already hit one surface and left it again,
//!   carrying its colour. That is global illumination, and here it is
//!   gathered from what is on screen.
//!
//! Everything is in view space: the camera at the origin looking down `z`.

use crate::color::{self, Linear, RGB};
use crate::math::{dot, normalize, Transform};
use crate::raster::{depth_of, normal, z_of, ShadowMap};
use crate::scene::{AmbientOcclusion, GlobalIllumination};

/// One, for a unit vector or a fraction in Q14.
const UNIT: i64 = 1 << 14;

/// Further apart than this -- four thousand units -- two points are treated
/// as this far apart, so that the square of the distance fits.
const FAR: i32 = 1 << 28;

/// `base ^ exponent` for a Q14 `base` between 0 and 1.
#[inline]
pub(crate) fn power(base: i64, mut exponent: u32) -> i64 {
    let (mut result, mut square) = (UNIT, base.clamp(0, UNIT));
    while exponent > 0 {
        if exponent & 1 == 1 {
            result = (result * square) >> 14;
        }
        square = (square * square) >> 14;
        exponent >>= 1;
    }
    result
}

#[inline]
fn blend(a: Linear, b: Linear, t: i64) -> Linear {
    [0, 1, 2].map(|i| (a[i] as i64 + (((b[i] - a[i]) as i64 * t) >> 14)) as i32)
}

/// The camera, as the passes that work on finished pixels need it: enough to
/// turn a pixel and its depth back into a point, and a point into a pixel.
#[derive(Clone, Copy)]
pub(crate) struct Eye {
    pub width: usize,
    pub height: usize,
    /// Focal length in pixels, Q16.16, and its reciprocal in Q24.
    pub focal: i64,
    pub inverse_focal: i64,
    pub near: i32,
    /// Which way is up in the world, as the camera sees it. Q14.
    pub up: [i32; 3],
}

impl Eye {
    pub(crate) fn new(width: usize, height: usize, focal: i64, near: i32, view: &Transform) -> Self {
        Self {
            width,
            height,
            focal,
            inverse_focal: (1i64 << 40) / focal.max(1),
            near,
            up: view.direction([0, 1 << 14, 0]),
        }
    }

    /// The line of sight through the centre of a pixel: `x / z` and `y / z`
    /// along it, Q24.
    #[inline]
    pub(crate) fn ray(&self, x: usize, y: usize) -> [i64; 2] {
        let across = 2 * x as i64 + 1 - self.width as i64;
        let up = self.height as i64 - 1 - 2 * y as i64;
        [(across * self.inverse_focal) >> 1, (up * self.inverse_focal) >> 1]
    }

    /// The view-space point a pixel shows.
    #[inline]
    pub(crate) fn position(&self, x: usize, y: usize, depth: u32) -> [i32; 3] {
        let z = z_of(depth).min(i32::MAX as i64);
        let [rx, ry] = self.ray(x, y);
        [((z * rx) >> 24) as i32, ((z * ry) >> 24) as i32, z as i32]
    }

    /// The direction a pixel looks in, a unit in Q14.
    #[inline]
    pub(crate) fn direction(&self, x: usize, y: usize) -> [i32; 3] {
        let [rx, ry] = self.ray(x, y);
        normalize([(rx >> 4) as i32, (ry >> 4) as i32, 1 << 20], 14)
    }

    /// Which pixel a view-space point falls on, and the depth it would have
    /// there. `None` if it is behind the camera or off the picture.
    #[inline]
    pub(crate) fn project(&self, p: [i32; 3]) -> Option<(usize, usize, u32)> {
        if p[2] < self.near {
            return None;
        }
        let depth = depth_of(p[2] as i64);
        let scale = |c: i32| ((c as i64 * self.focal) as i128 * depth as i128 >> 42) as i64;
        let x = ((self.width as i64) << 15) + scale(p[0]);
        let y = ((self.height as i64) << 15) - scale(p[1]);
        if x < 0 || y < 0 {
            return None;
        }
        let (x, y) = ((x >> 16) as usize, (y >> 16) as usize);
        (x < self.width && y < self.height).then_some((x, y, depth))
    }
}

/// The sky and the sun in it, as light.
#[derive(Clone, Copy)]
pub(crate) struct Heaven {
    pub zenith: Linear,
    pub horizon: Linear,
    pub ground: Linear,
    /// Towards the sun, in view space. Q14.
    pub sun_direction: [i32; 3],
    pub sun: Linear,
    pub up: [i32; 3],
}

impl Heaven {
    /// What is seen looking along `direction`, a view-space unit in Q14.
    pub(crate) fn color(&self, direction: [i32; 3]) -> Linear {
        let height = dot(direction, self.up) >> 14;
        let mut sky = if height >= 0 {
            // Most of the change is near the horizon.
            let below = UNIT - height.min(UNIT);
            blend(self.horizon, self.zenith, UNIT - ((below * below) >> 14))
        } else {
            blend(self.horizon, self.ground, (-height * 8).min(UNIT))
        };

        let toward = dot(direction, self.sun_direction) >> 14;
        if toward > 0 && height > -(UNIT >> 5) {
            // A wide glow, and a disc bright enough to bloom.
            let glow = power(toward, 96) >> 2;
            let disc = (toward - (UNIT - 12)).clamp(0, 8) * (UNIT / 8) * 6;
            for (channel, sun) in sky.iter_mut().zip(self.sun) {
                *channel = (*channel as i64 + ((sun as i64 * (glow + disc)) >> 14)).min(color::MAX as i64) as i32;
            }
        }
        sky
    }

    /// The light arriving on a surface facing `n` from the whole sky above
    /// it and the ground below.
    #[inline]
    fn ambient(&self, n: [i32; 3]) -> Linear {
        let up = ((dot(n, self.up) >> 14) + UNIT) / 2;
        let above = [0, 1, 2].map(|i| (self.zenith[i] + self.horizon[i]) / 2);
        blend(self.ground, above, up.clamp(0, UNIT))
    }
}

/// A lamp, as the camera sees it.
#[derive(Clone, Copy)]
pub(crate) struct Lamp {
    pub position: [i32; 3],
    pub color: Linear,
    /// `2^32 / radius^2`, with the radius squared in Q16.16.
    pub inverse_square: i64,
}

impl Lamp {
    pub(crate) fn new(position: [i32; 3], color: Linear, radius: i32) -> Self {
        let square = ((radius as i64 * radius as i64) >> 16).max(1);
        Self {
            position,
            color,
            inverse_square: (1i64 << 32) / square,
        }
    }
}

/// Everything needed to ask whether a point is in the sun's shadow.
pub(crate) struct Shade<'m> {
    pub map: &'m ShadowMap<'m>,
    /// View space to the sun's view.
    pub to_light: Transform,
    /// Light-space units to 1/256 texel, Q16.16.
    pub scale: i64,
    /// How far a surface is pushed towards the sun before it is tested, and
    /// how far along its own normal. Without these a surface shadows itself
    /// in stripes, because the map's idea of where it is is a texel coarse.
    pub bias: i32,
    pub offset: i32,
}

impl Shade<'_> {
    /// How much of the sun reaches a point: 0 for none, 256 for all of it.
    ///
    /// Sixteen texels are tested and weighted by how close the point is to
    /// each, so the edge of a shadow is a ramp a texel or two wide rather
    /// than a staircase.
    fn lit(&self, p: [i32; 3], n: [i32; 3]) -> i64 {
        let lifted = [0, 1, 2].map(|i| p[i].saturating_add(((n[i] as i64 * self.offset as i64) >> 14) as i32));
        let l = self.to_light.point(lifted);
        let size = self.map.size as i64;
        let on_map = |c: i32| ((c as i64 * self.scale) >> 16) + size * 128 - 128;
        let (tx, ty) = (on_map(l[0]), on_map(l[1]));
        let (x0, y0) = ((tx >> 8) - 1, (ty >> 8) - 1);
        if x0 < 0 || y0 < 0 || x0 + 3 >= size || y0 + 3 >= size {
            return 256;
        }
        let depth = ShadowMap::encode(l[2].saturating_add(self.bias));
        let (fx, fy) = (tx & 0xff, ty & 0xff);
        let across = [256 - fx, 256, 256, fx];
        let down = [256 - fy, 256, 256, fy];

        let mut sum = 0;
        for (j, weight_y) in down.into_iter().enumerate() {
            let mut row = 0;
            for (i, weight_x) in across.into_iter().enumerate() {
                if depth >= self.map.get(x0 as usize + i, y0 as usize + j) {
                    row += weight_x;
                }
            }
            sum += row * weight_y;
        }
        // The weights sum to 768 each way.
        (sum * 256) / (768 * 768)
    }
}

// ---------------------------------------------------------------------------
// Ambient occlusion
// ---------------------------------------------------------------------------

/// A cheap deterministic stream of numbers, for building sample patterns at
/// compile time.
const fn scramble(state: u32) -> u32 {
    let state = state ^ (state << 13);
    let state = state ^ (state >> 17);
    state ^ (state << 5)
}

/// Sixty-four points scattered through a ball of radius one, Q14, none of
/// them too close to the middle.
const BALL: [[i16; 3]; 64] = {
    let mut points = [[0i16; 3]; 64];
    let mut state = 0x9e37_79b9u32;
    let mut count = 0;
    while count < 64 {
        let mut v = [0i32; 3];
        let mut k = 0;
        while k < 3 {
            state = scramble(state);
            v[k] = (state >> 17) as i32 - 16384;
            k += 1;
        }
        let square = v[0] * v[0] + v[1] * v[1] + v[2] * v[2];
        if square <= 16384 * 16384 && square >= 5000 * 5000 {
            points[count] = [v[0] as i16, v[1] as i16, v[2] as i16];
            count += 1;
        }
    }
    points
};

/// Thirty-two points scattered over a disc of radius one, in 1/256.
const DISC: [[i16; 2]; 32] = {
    let mut points = [[0i16; 2]; 32];
    let mut state = 0x85eb_ca6bu32;
    let mut count = 0;
    while count < 32 {
        state = scramble(state);
        let x = (state >> 23) as i32 - 256;
        state = scramble(state);
        let y = (state >> 23) as i32 - 256;
        let square = x * x + y * y;
        if square <= 256 * 256 && square >= 48 * 48 {
            points[count] = [x as i16, y as i16];
            count += 1;
        }
    }
    points
};

/// How many places round each pixel are tested.
const OCCLUSION_SAMPLES: usize = 8;

/// Work out how open to the sky each pixel is: 255 for fully, less in
/// corners and creases. Left in the top byte-and-a-bit of each word of
/// `out`, which is where [`shade`] looks for it.
///
/// For each pixel, eight points are scattered in the half-ball in front of
/// its surface. Each is looked up on screen: if what is drawn there is
/// nearer the camera than the point, the point is inside something, and
/// that much of the sky is blocked.
///
/// Eight points is few, so each pixel of a four-by-four tile uses a
/// different eight and the result is averaged over the tile -- sixty-four
/// points' worth of smoothness for the price of eight.
pub(crate) fn occlusion(
    eye: &Eye,
    shape: &[u64],
    raw: &mut [u64],
    out: &mut [u64],
    settings: &AmbientOcclusion,
) {
    let radius = settings.radius.0.max(1) as i64;
    let strength = settings.strength.0.clamp(0, 1 << 16) as i64;

    for y in 0..eye.height {
        for x in 0..eye.width {
            let index = y * eye.width + x;
            let depth = shape[index] as u32;
            if depth == 0 {
                raw[index] = 255;
                continue;
            }
            let n = normal(shape[index]);
            let p = eye.position(x, y, depth);
            // Start a little off the surface so that it does not shade itself.
            let origin = [0, 1, 2].map(|i| p[i] + ((n[i] as i64 * radius) >> 18) as i32);
            // Nothing much nearer than the pixel itself counts: a railing in
            // front of a far wall does not darken the wall.
            let too_near = depth_of((p[2] as i64 - 2 * radius).max(eye.near as i64));

            let first = ((x & 3) + 4 * (y & 3)) * 4;
            let mut blocked = 0;
            for sample in 0..OCCLUSION_SAMPLES {
                let mut s = BALL[(first + sample) & 63].map(|c| c as i32);
                if dot(s, n) < 0 {
                    s = s.map(|c| -c);
                }
                let q = [0, 1, 2].map(|i| origin[i].saturating_add(((s[i] as i64 * radius) >> 14) as i32));
                let Some((sx, sy, at)) = eye.project(q) else {
                    continue;
                };
                let there = shape[sy * eye.width + sx] as u32;
                if there > at + (at >> 8) && there <= too_near {
                    blocked += 1;
                }
            }
            let dark = (blocked * strength * 255) / (OCCLUSION_SAMPLES as i64 * 65536);
            raw[index] = (255 - dark) as u64;
        }
    }

    // Average over the tile, but only with pixels at about the same depth,
    // so that darkness does not leak across an edge onto something far
    // behind it.
    for y in 0..eye.height {
        for x in 0..eye.width {
            let index = y * eye.width + x;
            let depth = shape[index] as u32 as i64;
            let (mut sum, mut count) = (0, 0);
            if depth != 0 {
                for ty in y.saturating_sub(2)..(y + 2).min(eye.height) {
                    for tx in x.saturating_sub(2)..(x + 2).min(eye.width) {
                        let there = ty * eye.width + tx;
                        if ((shape[there] as u32 as i64) - depth).abs() <= depth >> 4 {
                            sum += raw[there] & 0xff;
                            count += 1;
                        }
                    }
                }
            }
            let open = if count == 0 { 255 } else { sum / count };
            out[index] = open << 48;
        }
    }
}

// ---------------------------------------------------------------------------
// Direct light
// ---------------------------------------------------------------------------

/// Light every pixel.
///
/// `light` arrives holding each pixel's openness to the sky in its top bits
/// (see [`occlusion`]; 255 everywhere if that pass is off) and leaves holding
/// the pixel's colour in linear light.
pub(crate) fn shade(
    eye: &Eye,
    heaven: &Heaven,
    lamps: &[Lamp],
    shadow: Option<&Shade<'_>>,
    surface: &[u64],
    shape: &[u64],
    light: &mut [u64],
) {
    for y in 0..eye.height {
        for x in 0..eye.width {
            let index = y * eye.width + x;
            let depth = shape[index] as u32;
            if depth == 0 {
                light[index] = color::pack(heaven.color(eye.direction(x, y)));
                continue;
            }

            let material = surface[index];
            let albedo = color::linear_packed(material as u32);
            let shine = (material >> 32) as u8 as i64;
            let glow = (material >> 40) as u8 as i64;
            let n = normal(shape[index]);
            let p = eye.position(x, y, depth);
            let to_eye = normalize(p.map(|c| -c), 14);
            let open = (light[index] >> 48) as i64;

            // Light that the surface scatters in its own colour, and light
            // it mirrors in the light's.
            let mut diffuse = heaven.ambient(n).map(|c| c as i64 * open / 255);
            let mut gloss = [0i64; 3];

            let mut add = |toward: [i32; 3], color: Linear, amount: i64| {
                let facing = dot(n, toward) >> 14;
                if facing <= 0 || amount <= 0 {
                    return;
                }
                for i in 0..3 {
                    diffuse[i] += (color[i] as i64 * facing * amount) >> 22;
                }
                if shine > 0 {
                    let half = normalize([0, 1, 2].map(|i| toward[i] + to_eye[i]), 14);
                    let highlight = power(dot(n, half) >> 14, 4 + shine as u32) * shine / 255;
                    for i in 0..3 {
                        gloss[i] += (color[i] as i64 * highlight * amount) >> 22;
                    }
                }
            };

            let sun = match shadow {
                Some(shade) if dot(n, heaven.sun_direction) > 0 => shade.lit(p, n),
                _ => 256,
            };
            add(heaven.sun_direction, heaven.sun, sun);

            for lamp in lamps {
                let away = [0, 1, 2].map(|i| lamp.position[i].saturating_sub(p[i]).clamp(-FAR, FAR));
                let square = dot(away, away) >> 16;
                let fade = (1 << 16) - ((square * lamp.inverse_square) >> 16);
                if fade > 0 {
                    add(normalize(away, 14), lamp.color, (fade * fade) >> 24);
                }
            }

            let lit = [0, 1, 2].map(|i| {
                let scattered = (albedo[i] as i64 * diffuse[i]) >> 12;
                let own = (albedo[i] as i64 * glow) >> 4;
                (scattered + gloss[i] + own).min(color::MAX as i64) as i32
            });
            light[index] = color::pack(lit) | (light[index] & !RGB);
        }
    }
}

// ---------------------------------------------------------------------------
// Bounced light
// ---------------------------------------------------------------------------

const BOUNCE_SAMPLES: usize = 8;

/// Gather light bounced off nearby surfaces, for every second pixel each
/// way.
///
/// For each, eight other pixels round it are visited. Each one is a small
/// patch of lit surface; how much of its light arrives here depends on how
/// squarely the two face each other and how far apart they are. This is the
/// same sum a path tracer estimates, taken only over what the camera can
/// see -- which is its limit: a red wall behind the camera tints nothing.
pub(crate) fn bounce(
    eye: &Eye,
    shape: &[u64],
    light: &[u64],
    out: &mut [u64],
    settings: &GlobalIllumination,
) {
    let radius = settings.radius.0.max(1 << 8) as i64;
    let inverse_square = (1i64 << 32) / ((radius * radius) >> 16).max(1);
    let strength = settings.strength.0.clamp(0, 8 << 16) as i64;

    for y in (0..eye.height).step_by(2) {
        for x in (0..eye.width).step_by(2) {
            let index = y * eye.width + x;
            let depth = shape[index] as u32;
            out[index] = 0;
            if depth == 0 {
                continue;
            }
            let n = normal(shape[index]);
            let p = eye.position(x, y, depth);
            // How many pixels the gathering radius spans at this distance.
            let reach = ((radius * eye.focal) / (p[2] as i64).max(1) >> 16).clamp(6, 96);

            let first = (((x >> 1) & 1) + 2 * ((y >> 1) & 1)) * BOUNCE_SAMPLES;
            let mut sum = [0i64; 3];
            for sample in 0..BOUNCE_SAMPLES {
                let [dx, dy] = DISC[first + sample].map(|c| (c as i64 * reach) >> 8);
                let (sx, sy) = (x as i64 + dx, y as i64 + dy);
                if sx < 0 || sy < 0 || sx >= eye.width as i64 || sy >= eye.height as i64 {
                    continue;
                }
                let there = sy as usize * eye.width + sx as usize;
                let other = shape[there] as u32;
                if other == 0 || there == index {
                    continue;
                }
                let q = eye.position(sx as usize, sy as usize, other);
                let away = [0, 1, 2].map(|i| q[i].saturating_sub(p[i]).clamp(-FAR, FAR));
                let fade = (1 << 16) - (((dot(away, away) >> 16) * inverse_square) >> 16);
                if fade <= 0 {
                    continue;
                }
                let toward = normalize(away, 14);
                let receiving = dot(n, toward) >> 14;
                let sending = -dot(normal(shape[there]), toward) >> 14;
                if receiving <= 0 || sending <= 0 {
                    continue;
                }
                let weight = (receiving * sending * fade) >> 28;
                let sent = color::unpack(light[there]);
                for i in 0..3 {
                    sum[i] += sent[i] as i64 * weight;
                }
            }
            // `weight` and `strength` are both Q16; the two is there because
            // a patch facing this one head on sends twice the average.
            let gathered = sum.map(|c| ((c * strength * 2) >> 32) / BOUNCE_SAMPLES as i64);
            out[index] = color::pack(gathered.map(|c| c.min(color::MAX as i64) as i32));
        }
    }
}

/// Add the light [`bounce`] gathered to the picture, tinted by each
/// surface's own colour.
///
/// Each pixel takes the average of the gathered values around it that lie
/// on about the same depth, which both fills in the pixels that were
/// skipped and smooths over the few samples each one took.
pub(crate) fn bounce_apply(eye: &Eye, surface: &[u64], shape: &[u64], gathered: &[u64], light: &mut [u64]) {
    for y in 0..eye.height {
        for x in 0..eye.width {
            let index = y * eye.width + x;
            let depth = shape[index] as u32 as i64;
            if depth == 0 {
                continue;
            }
            let (cx, cy) = ((x & !1) as i64, (y & !1) as i64);
            let (mut sum, mut count) = ([0i64; 3], 0);
            for ty in [cy - 2, cy, cy + 2] {
                for tx in [cx - 2, cx, cx + 2] {
                    if tx < 0 || ty < 0 || tx >= eye.width as i64 || ty >= eye.height as i64 {
                        continue;
                    }
                    let there = ty as usize * eye.width + tx as usize;
                    if ((shape[there] as u32 as i64) - depth).abs() > depth >> 3 {
                        continue;
                    }
                    let value = color::unpack(gathered[there]);
                    for i in 0..3 {
                        sum[i] += value[i] as i64;
                    }
                    count += 1;
                }
            }
            if count == 0 {
                continue;
            }
            let albedo = color::linear_packed(surface[index] as u32);
            let now = color::unpack(light[index]);
            let lit = [0, 1, 2].map(|i| now[i] + ((albedo[i] as i64 * sum[i] / count) >> 12) as i32);
            light[index] = color::pack(lit) | (light[index] & !RGB);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::Vec3;
    use crate::scene::Camera;

    fn eye() -> Eye {
        let camera = Camera::look_at(Vec3::new(0, 0, 5), Vec3::ZERO);
        Eye::new(320, 200, camera.focal(200), camera.near_plane(), &camera.view())
    }

    #[test]
    fn powers_match_reference() {
        for base in [0.0, 0.3, 0.9, 0.99, 1.0] {
            for exponent in [1u32, 2, 7, 64, 259] {
                let got = power((base * 16384.0) as i64, exponent) as f64 / 16384.0;
                let exact = f64::powi(base, exponent as i32);
                assert!((got - exact).abs() < 2e-3, "{base}^{exponent}: {got} vs {exact}");
            }
        }
    }

    #[test]
    fn a_point_projects_to_the_pixel_it_came_from() {
        let eye = eye();
        for (x, y, z) in [(0, 0, 3.0), (319, 199, 1.0), (160, 100, 40.0), (17, 180, 0.25)] {
            let depth = depth_of((z * 65536.0) as i64);
            let p = eye.position(x, y, depth);
            // Against the pinhole formula, in floating point.
            let f = eye.focal as f64 / 65536.0;
            let back = z_of(depth) as f64 / 65536.0;
            let exact = [(x as f64 + 0.5 - 160.0) / f * back, (100.0 - y as f64 - 0.5) / f * back];
            assert!((p[0] as f64 / 65536.0 - exact[0]).abs() < back * 2e-4);
            assert!((p[1] as f64 / 65536.0 - exact[1]).abs() < back * 2e-4);
            let (sx, sy, there) = eye.project(p).unwrap();
            assert_eq!((sx, sy), (x, y));
            assert!((there as i64 - depth as i64).abs() <= 1 + (depth as i64 >> 20));
        }
        assert!(eye.project([0, 0, -65536]).is_none());
        assert!(eye.project([i32::MAX / 2, 0, 65536]).is_none());
    }

    #[test]
    fn the_sky_is_blue_above_and_brightest_at_the_sun() {
        let heaven = Heaven {
            zenith: [400, 900, 2500],
            horizon: [2000, 2400, 2800],
            ground: [300, 250, 200],
            sun_direction: normalize([1, 1, 1], 14),
            sun: [8000, 7800, 7000],
            up: [0, 1 << 14, 0],
        };
        assert_eq!(heaven.color([0, 1 << 14, 0]), heaven.zenith);
        assert_eq!(heaven.color([0, -(1 << 14), 0]), heaven.ground);
        let level = heaven.color(normalize([1, 0, -1], 14));
        assert!((level[2] - heaven.horizon[2]).abs() < 8);
        let sun = heaven.color(heaven.sun_direction);
        assert!(sun[0] > 4 * color::ONE, "the sun's disc is {sun:?}");

        // A floor is lit by the sky, a ceiling by the ground.
        assert_eq!(heaven.ambient([0, 1 << 14, 0])[2], (2500 + 2800) / 2);
        assert_eq!(heaven.ambient([0, -(1 << 14), 0]), heaven.ground);
    }

    #[test]
    fn the_sample_patterns_are_where_they_should_be() {
        for point in BALL {
            let length = point.iter().map(|c| (*c as f64 / 16384.0).powi(2)).sum::<f64>().sqrt();
            assert!((0.3..=1.0).contains(&length));
        }
        for point in DISC {
            let length = point.iter().map(|c| (*c as f64 / 256.0).powi(2)).sum::<f64>().sqrt();
            assert!((0.18..=1.0).contains(&length));
        }
        // Spread about evenly: the average is near the middle.
        let mean = |axis: usize| BALL.iter().map(|p| p[axis] as i32).sum::<i32>() / 64;
        assert!((0..3).all(|axis| mean(axis).abs() < 3000));
    }
}

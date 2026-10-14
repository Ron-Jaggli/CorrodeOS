//! What happens to a picture after it is lit.
//!
//! Each of these takes a finished picture and makes another. The first four
//! work on light -- linear, with room above white -- because they are things
//! light does: bounce off a shiny floor, smear across a moving shutter,
//! spread through a lens. Then [`resolve`] fits that light onto a screen, and
//! what follows works on the screen's own gamma-encoded pixels, because it is
//! about how the picture looks rather than what the light did.
//!
//! The order is the order a camera does it in: the world, then the lens,
//! then the film.

use crate::color::{self, Linear, Tonemap, RGB};
use crate::light::{power, Eye, Heaven};
use crate::math::{dot, isqrt, normalize, sin_cos_q30, sqrt};
use crate::raster::{normal, velocity};
use crate::scene::{DepthOfField, MotionBlur, Reflections};
use crate::texture::mix;

const UNIT: i64 = 1 << 14;

// ---------------------------------------------------------------------------
// Reflections
// ---------------------------------------------------------------------------

/// How many steps a reflected ray takes before giving up.
const REFLECTION_STEPS: usize = 28;

/// Show, in every surface that reflects, what it reflects.
///
/// From each such pixel a ray leaves in the mirror direction and is walked
/// across the picture, a step at a time, until it passes behind something
/// already drawn. What is drawn there is what the surface reflects. This is
/// ray tracing against the depth buffer instead of the scene, which is why
/// it costs so little and why it can only reflect what is on screen: a ray
/// that leaves the picture, or passes behind something without hitting it,
/// shows the sky instead.
///
/// Surfaces reflect more at a glancing angle than head on -- a lake is a
/// mirror at the far shore and a window at your feet. That is Fresnel's law,
/// here in Schlick's approximation.
pub(crate) fn reflections(
    eye: &Eye,
    heaven: &Heaven,
    surface: &[u64],
    shape: &[u64],
    light: &[u64],
    out: &mut [u64],
    settings: &Reflections,
) {
    let reach = settings.distance.0.max(1 << 12) as i64;

    for y in 0..eye.height {
        for x in 0..eye.width {
            let index = y * eye.width + x;
            out[index] = light[index];
            let depth = shape[index] as u32;
            let mirror = (surface[index] >> 24) as u8 as i64;
            if depth == 0 || mirror == 0 {
                continue;
            }

            let n = normal(shape[index]);
            let p = eye.position(x, y, depth);
            let incoming = normalize(p, 14);
            let facing = (-dot(n, incoming) >> 14).clamp(0, UNIT);
            let away = [0, 1, 2].map(|i| incoming[i] + ((2 * facing * n[i] as i64) >> 14) as i32);

            let head_on = mirror * UNIT / 255;
            let amount = head_on + (((UNIT - head_on) * power(UNIT - facing, 5)) >> 14);

            // Start just off the surface, and take longer strides the
            // further the ray has gone: what is far away is small on screen.
            let first = (p[2] as i64 >> 6).max(1 << 9);
            let lifted = [0, 1, 2].map(|i| p[i] + ((n[i] as i64 * first) >> 15) as i32);
            let along = |t: i64| {
                [0, 1, 2].map(|i| lifted[i].saturating_add(((away[i] as i64 * t) >> 14) as i32))
            };

            // Whether the ray, `t` along, has gone behind what is drawn
            // there -- and not so far behind that it has passed something
            // thin.
            let behind = |t: i64| -> Option<Option<usize>> {
                let (sx, sy, at) = eye.project(along(t))?;
                let there = sy * eye.width + sx;
                let drawn = shape[there] as u32;
                let hit = drawn > at && drawn - at < (at >> 2).max(1);
                Some(hit.then_some(there))
            };

            let (mut t, mut stride, mut before) = (first, first, 0);
            let mut found = None;
            for _ in 0..REFLECTION_STEPS {
                if t > reach {
                    break;
                }
                match behind(t) {
                    None => break,
                    Some(Some(there)) => {
                        // Somewhere between the last step and this one the
                        // ray went in. Halve the gap a few times to find
                        // where.
                        let (mut low, mut high, mut best) = (before, t, there);
                        for _ in 0..5 {
                            let middle = (low + high) / 2;
                            match behind(middle) {
                                Some(Some(nearer)) => (high, best) = (middle, nearer),
                                _ => low = middle,
                            }
                        }
                        found = Some((best, high));
                        break;
                    }
                    Some(None) => {}
                }
                before = t;
                stride += stride >> 2;
                t += stride;
            }

            let sky = heaven.color(normalize(away, 14));
            let reflected = match found {
                Some((there, t)) => {
                    // Fade to sky towards the edge of the picture and the
                    // end of the ray's reach, so reflections do not stop
                    // dead where the information does.
                    let (hx, hy) = (there % eye.width, there / eye.width);
                    let edge = hx.min(eye.width - 1 - hx).min(hy).min(eye.height - 1 - hy) as i64;
                    let near_edge = (edge * UNIT / 24).min(UNIT);
                    let near_end = ((reach - t) * 4 * UNIT / reach).clamp(0, UNIT);
                    let trust = (near_edge * near_end) >> 14;
                    let seen = color::unpack(light[there]);
                    [0, 1, 2].map(|i| sky[i] + (((seen[i] - sky[i]) as i64 * trust) >> 14) as i32)
                }
                None => sky,
            };

            let base = color::unpack(light[index]);
            let shown = [0, 1, 2]
                .map(|i| base[i] + (((reflected[i] - base[i]) as i64 * amount) >> 14) as i32);
            out[index] = color::pack(shown) | (light[index] & !RGB);
        }
    }
}

// ---------------------------------------------------------------------------
// Motion blur
// ---------------------------------------------------------------------------

const MOTION_SAMPLES: i64 = 8;

/// Smear each pixel along the way it moved while the shutter was open.
///
/// The rasteriser recorded, for every pixel, where that piece of surface was
/// on screen at the previous instant. Averaging the picture along that line
/// is what film does when something crosses the frame during an exposure.
pub(crate) fn motion_blur(
    width: usize,
    height: usize,
    surface: &[u64],
    light: &[u64],
    out: &mut [u64],
    settings: &MotionBlur,
) {
    let strength = settings.strength.0.clamp(0, 4 << 16) as i64;
    for y in 0..height {
        for x in 0..width {
            let index = y * width + x;
            // Half pixels in, sixteenths of a pixel out.
            let [vx, vy] = velocity(surface[index]).map(|c| (c as i64 * strength) >> 13);
            if vx.abs() + vy.abs() < 16 {
                out[index] = light[index];
                continue;
            }
            let mut sum = [0i64; 3];
            for sample in 0..MOTION_SAMPLES {
                // Centred on the pixel: from half the motion behind it to
                // half ahead.
                let along = 2 * sample + 1 - MOTION_SAMPLES;
                let sx = (x as i64 + ((vx * along) >> 8)).clamp(0, width as i64 - 1) as usize;
                let sy = (y as i64 + ((vy * along) >> 8)).clamp(0, height as i64 - 1) as usize;
                let seen = color::unpack(light[sy * width + sx]);
                for i in 0..3 {
                    sum[i] += seen[i] as i64;
                }
            }
            out[index] =
                color::pack(sum.map(|c| (c / MOTION_SAMPLES) as i32)) | (light[index] & !RGB);
        }
    }
}

// ---------------------------------------------------------------------------
// Depth of field
// ---------------------------------------------------------------------------

const LENS_SAMPLES: usize = 16;

/// Sixteen points spread evenly over a disc of radius one, in 1/256, each
/// with its distance from the middle.
///
/// The sunflower pattern: each point a fixed fraction of a turn on from the
/// last -- the golden angle -- and far enough out that every ring holds the
/// same area. No two points line up, which is what keeps sixteen samples
/// from looking like sixteen copies.
const LENS: [[i32; 3]; LENS_SAMPLES] = {
    let mut points = [[0; 3]; LENS_SAMPLES];
    let mut k = 0;
    while k < LENS_SAMPLES {
        // sqrt((k + 1/2) / 16), in 1/256.
        let radius = isqrt(((2 * k as u64 + 1) << 16) / 32) as i64;
        let (sin, cos) = sin_cos_q30((k as u32).wrapping_mul(1_640_531_527));
        points[k] = [
            ((radius * cos) >> 30) as i32,
            ((radius * sin) >> 30) as i32,
            radius as i32,
        ];
        k += 1;
    }
    points
};

/// Blur what the lens is not focused on.
///
/// A lens focused at one distance turns a point at any other into a disc --
/// the *circle of confusion* -- whose size grows with how far out of focus
/// the point is. Each pixel here is replaced by an average over the disc it
/// would be spread across.
///
/// Not every neighbour counts. A neighbour contributes only if its own disc
/// is big enough to reach this pixel, so something sharp does not smear into
/// the blur behind it.
pub(crate) fn depth_of_field(
    eye: &Eye,
    shape: &[u64],
    light: &[u64],
    out: &mut [u64],
    settings: &DepthOfField,
) {
    let (width, height) = (eye.width as i64, eye.height as i64);
    let focus = settings.focus.0.max(eye.near) as i64;
    // The disc's radius for something at infinity, in 1/256 pixel.
    let aperture = (settings.aperture.0.clamp(0, 1 << 14) as i64 * height) >> 8;
    // A disc's radius from a depth. `focus / z` is one for what is sharp,
    // nought at infinity, and large close to the lens.
    let confusion = |depth: u32| {
        let ratio = (focus * depth as i64) >> 26;
        ((aperture * (ratio - (1 << 16)).abs()) >> 16).min(aperture * 3)
    };

    for y in 0..eye.height {
        for x in 0..eye.width {
            let index = y * eye.width + x;
            let radius = confusion(shape[index] as u32);
            if radius < 128 {
                out[index] = light[index];
                continue;
            }
            let mut sum = color::unpack(light[index]).map(|c| c as i64 * 256);
            let mut total = 256;
            for [dx, dy, distance] in LENS {
                let sx = x as i64 + ((dx as i64 * radius + (1 << 15)) >> 16);
                let sy = y as i64 + ((dy as i64 * radius + (1 << 15)) >> 16);
                if sx < 0 || sy < 0 || sx >= width || sy >= height {
                    continue;
                }
                let there = sy as usize * eye.width + sx as usize;
                let reach = confusion(shape[there] as u32) - ((distance as i64 * radius) >> 8);
                let weight = (reach + 256).clamp(0, 256);
                if weight > 0 {
                    let seen = color::unpack(light[there]);
                    for i in 0..3 {
                        sum[i] += seen[i] as i64 * weight;
                    }
                    total += weight;
                }
            }
            out[index] = color::pack(sum.map(|c| (c / total) as i32)) | (light[index] & !RGB);
        }
    }
}

// ---------------------------------------------------------------------------
// Bloom
// ---------------------------------------------------------------------------

/// How much smaller than the picture, each way, bloom is worked out at. Glow
/// is soft by nature, so a sixteenth of the pixels is plenty.
pub(crate) const BLOOM_SCALE: usize = 4;

/// Pick out what is brighter than `threshold` (linear Q12), at a quarter
/// size: each output pixel is the average of a four-by-four block with the
/// threshold's worth of brightness taken off.
pub(crate) fn bloom_extract(
    width: usize,
    height: usize,
    light: &[u64],
    out: &mut [u64],
    threshold: i64,
) {
    let (small_width, small_height) = (width / BLOOM_SCALE, height / BLOOM_SCALE);
    for y in 0..small_height {
        for x in 0..small_width {
            let mut sum = [0i64; 3];
            for by in 0..BLOOM_SCALE {
                let row = (y * BLOOM_SCALE + by) * width + x * BLOOM_SCALE;
                for word in &light[row..row + BLOOM_SCALE] {
                    let seen = color::unpack(*word);
                    for i in 0..3 {
                        sum[i] += seen[i] as i64;
                    }
                }
            }
            let mean = sum.map(|c| (c / (BLOOM_SCALE * BLOOM_SCALE) as i64) as i32);
            let brightness = color::luminance(mean) as i64;
            // Dim the whole colour rather than each channel, so that what
            // glows keeps its hue.
            let over = (brightness - threshold).max(0);
            let kept = mean.map(|c| (c as i64 * over / brightness.max(1)) as i32);
            out[y * small_width + x] = color::pack(kept);
        }
    }
}

/// The ninth row of Pascal's triangle: near enough a bell curve, and it sums
/// to 256.
const BELL: [i64; 9] = [1, 8, 28, 56, 70, 56, 28, 8, 1];

/// Spread the bright parts out.
///
/// Three rounds of a nine-tap blur, each with its taps twice as far apart as
/// the last. That reaches as far as one blur sixty-odd taps wide for the
/// price of twenty-seven, and blurring across and then down costs two short
/// passes instead of one square one.
pub(crate) fn bloom_blur(width: usize, height: usize, a: &mut [u64], b: &mut [u64]) {
    let pass = |from: &[u64], to: &mut [u64], step_x: i64, step_y: i64| {
        for y in 0..height as i64 {
            for x in 0..width as i64 {
                let mut sum = [0i64; 3];
                for (tap, weight) in BELL.into_iter().enumerate() {
                    let offset = tap as i64 - 4;
                    let sx = (x + offset * step_x).clamp(0, width as i64 - 1) as usize;
                    let sy = (y + offset * step_y).clamp(0, height as i64 - 1) as usize;
                    let seen = color::unpack(from[sy * width + sx]);
                    for i in 0..3 {
                        sum[i] += seen[i] as i64 * weight;
                    }
                }
                to[y as usize * width + x as usize] =
                    color::pack(sum.map(|c| ((c + 128) >> 8) as i32));
            }
        }
    };
    for spacing in [1, 2, 4] {
        pass(a, b, spacing, 0);
        pass(b, a, 0, spacing);
    }
}

/// Read a quarter-size picture at a full-size pixel, blending the four
/// nearest.
#[inline]
fn bloom_at(small: &[u64], small_width: usize, small_height: usize, x: usize, y: usize) -> Linear {
    // The centre of full-size pixel x, in quarter-size pixels, less the
    // half that puts sample points at centres. 1/256.
    let place = |v: usize, limit: usize| {
        let at =
            ((2 * v as i64 + 1) * 128 / BLOOM_SCALE as i64 - 128).clamp(0, (limit as i64 - 1) << 8);
        let whole = (at >> 8) as usize;
        (whole, (whole + 1).min(limit - 1), at & 0xff)
    };
    let (x0, x1, fx) = place(x, small_width);
    let (y0, y1, fy) = place(y, small_height);
    let read = |x: usize, y: usize| color::unpack(small[y * small_width + x]);
    let (a, b, c, d) = (read(x0, y0), read(x1, y0), read(x0, y1), read(x1, y1));
    [0, 1, 2].map(|i| {
        let top = a[i] as i64 * (256 - fx) + b[i] as i64 * fx;
        let bottom = c[i] as i64 * (256 - fx) + d[i] as i64 * fx;
        ((top * (256 - fy) + bottom * fy) >> 16) as i32
    })
}

// ---------------------------------------------------------------------------
// Onto the screen
// ---------------------------------------------------------------------------

/// The glow to add, and how much of it.
pub(crate) struct Glow<'b> {
    pub small: &'b [u64],
    pub width: usize,
    pub height: usize,
    /// Q16.16.
    pub strength: i64,
}

/// How light becomes a picture.
pub(crate) struct Grade<'b> {
    pub tonemap: Tonemap,
    /// Q16.16.
    pub exposure: i64,
    pub glow: Option<Glow<'b>>,
    /// How far red and blue part at the corner, as a fraction of the
    /// picture's height, Q16.16. Zero for none.
    pub aberration: i64,
    /// How much the corners darken, Q16.16. Zero for none.
    pub vignette: i64,
}

/// One channel of the light at a point between pixels, given in 1/256.
#[inline]
fn channel_at(light: &[u64], width: usize, height: usize, x: i64, y: i64, shift: u32) -> i64 {
    let x = (x - 128).clamp(0, (width as i64 - 1) << 8);
    let y = (y - 128).clamp(0, (height as i64 - 1) << 8);
    let (x0, y0) = ((x >> 8) as usize, (y >> 8) as usize);
    let (x1, y1) = ((x0 + 1).min(width - 1), (y0 + 1).min(height - 1));
    let (fx, fy) = (x & 0xff, y & 0xff);
    let read = |x: usize, y: usize| (light[y * width + x] >> shift & 0xffff) as i64;
    let top = read(x0, y0) * (256 - fx) + read(x1, y0) * fx;
    let bottom = read(x0, y1) * (256 - fx) + read(x1, y1) * fx;
    (top * (256 - fy) + bottom * fy) >> 16
}

/// Fit the light onto a screen.
///
/// This is where HDR ends. Up to here a pixel can be sixteen times brighter
/// than white; here the tone curve decides how that range is shown on glass
/// that stops at white, and the result is gamma-encoded into the `0x00RRGGBB`
/// a framebuffer wants.
///
/// Three things a lens does ride along, because they are a per-pixel
/// multiply or a shifted read and a pass of their own would cost more than
/// they do: glow is added, the corners are darkened, and red and blue are
/// read from slightly further out and further in than green, which is the
/// colour fringing of a simple lens.
pub(crate) fn resolve(
    width: usize,
    height: usize,
    light: &[u64],
    out: &mut [u64],
    grade: &Grade<'_>,
) {
    let (w, h) = (width as i64, height as i64);
    // Distances from the centre are kept doubled, so that pixel centres are
    // whole numbers.
    let corner = (w * w + h * h).max(1);
    let inverse_corner = (1i64 << 32) / corner;
    // How far to shift per doubled pixel of distance from the centre, Q24.
    let fringe = if grade.aberration > 0 {
        (((grade.aberration * h) << 8) / sqrt((corner as u64) << 16).max(1) as i64) << 8
    } else {
        0
    };

    for y in 0..height {
        for x in 0..width {
            let index = y * width + x;
            let (ox, oy) = (2 * x as i64 + 1 - w, 2 * y as i64 + 1 - h);
            let mut seen = color::unpack(light[index]);

            if fringe != 0 {
                // In 1/256 pixel.
                let (dx, dy) = ((ox * fringe) >> 16, (oy * fringe) >> 16);
                let (cx, cy) = ((2 * x as i64 + 1) * 128, (2 * y as i64 + 1) * 128);
                seen[0] = channel_at(light, width, height, cx + dx, cy + dy, 0) as i32;
                seen[2] = channel_at(light, width, height, cx - dx, cy - dy, 32) as i32;
            }

            let mut value = seen.map(|c| c as i64);
            if let Some(glow) = &grade.glow {
                let add = bloom_at(glow.small, glow.width, glow.height, x, y);
                for i in 0..3 {
                    value[i] += (add[i] as i64 * glow.strength) >> 16;
                }
            }

            let mut gain = grade.exposure;
            if grade.vignette != 0 {
                // The square of the distance, as a fraction of the corner's;
                // squared again so that the middle is left alone.
                let out = ((ox * ox + oy * oy) * inverse_corner) >> 16;
                let dark = (grade.vignette * ((out * out) >> 16)) >> 16;
                gain = (gain * ((1 << 16) - dark).max(0)) >> 16;
            }

            let encoded = value.map(|c| {
                let toned = grade.tonemap.apply((c * gain) >> 16);
                color::ENCODE[toned.clamp(0, color::ONE) as usize] as u64
            });
            out[index] = encoded[0] << 16 | encoded[1] << 8 | encoded[2];
        }
    }
}

// ---------------------------------------------------------------------------
// Anti-aliasing and grain
// ---------------------------------------------------------------------------

/// How bright a screen pixel looks, 0 to 255. Weighted towards green, as the
/// eye is.
#[inline]
fn brightness(pixel: u64) -> i32 {
    let p = pixel as u32;
    (((p >> 16) & 0xff) * 2 + ((p >> 8) & 0xff) * 5 + (p & 0xff)) as i32 >> 3
}

/// A screen pixel at a point between pixels, given in 1/256.
#[inline]
fn pixel_at(picture: &[u64], width: usize, height: usize, x: i64, y: i64) -> u32 {
    let x = (x - 128).clamp(0, (width as i64 - 1) << 8);
    let y = (y - 128).clamp(0, (height as i64 - 1) << 8);
    let (x0, y0) = ((x >> 8) as usize, (y >> 8) as usize);
    let (x1, y1) = ((x0 + 1).min(width - 1), (y0 + 1).min(height - 1));
    let (fx, fy) = ((x & 0xff) as u32, (y & 0xff) as u32);
    let read = |x: usize, y: usize| picture[y * width + x] as u32;
    mix(
        mix(read(x0, y0), read(x1, y0), fx),
        mix(read(x0, y1), read(x1, y1), fx),
        fy,
    )
}

/// The smoothed colour of one pixel, or `None` if it is not on an edge.
///
/// This is FXAA: instead of drawing every edge several times over, find the
/// edges in the finished picture and blur each along its own direction. The
/// four diagonal neighbours give the direction -- brightness changing from
/// top to bottom means an edge running across -- and the pixel becomes an
/// average of samples taken along it. A staircase turns into a ramp, and
/// anything that is not an edge is left exactly as it was.
#[inline]
fn smooth(picture: &[u64], width: usize, height: usize, x: usize, y: usize) -> Option<u32> {
    let at = |dx: usize, dy: usize| brightness(picture[(y + dy - 1) * width + x + dx - 1]);
    let (nw, ne, sw, se, middle) = (at(0, 0), at(2, 0), at(0, 2), at(2, 2), at(1, 1));
    let low = nw.min(ne).min(sw).min(se).min(middle);
    let high = nw.max(ne).max(sw).max(se).max(middle);
    if high - low < (high >> 3).max(6) {
        return None;
    }

    let (dx, dy) = (-((nw + ne) - (sw + se)), (nw + sw) - (ne + se));
    // Scale the direction so that its shorter side is about a pixel, which
    // makes a nearly level edge reach a long way along itself; then cap it.
    let reduce = ((nw + ne + sw + se) >> 5).max(2);
    let shortest = dx.abs().min(dy.abs()) + reduce;
    let span = |d: i32| (((d as i64) << 8) / shortest as i64).clamp(-8 << 8, 8 << 8);
    let (dx, dy) = (span(dx), span(dy));

    let (cx, cy) = ((2 * x as i64 + 1) * 128, (2 * y as i64 + 1) * 128);
    let tap = |scale: i64| {
        pixel_at(
            picture,
            width,
            height,
            cx + dx * scale / 6,
            cy + dy * scale / 6,
        )
    };
    let narrow = mix(tap(-1), tap(1), 128);
    let wide = mix(narrow, mix(tap(-3), tap(3), 128), 128);
    // If the wide average has wandered off the edge onto something else,
    // its brightness gives it away.
    let wide_brightness = brightness(wide as u64);
    Some(if wide_brightness < low || wide_brightness > high {
        narrow
    } else {
        wide
    })
}

/// A number that looks random, from a pixel's position and a seed.
#[inline]
fn hash(x: usize, y: usize, seed: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x9e37_79b1) ^ (y as u32).wrapping_mul(0x85eb_ca77) ^ seed;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297a_2d39);
    h ^ (h >> 15)
}

/// Film grain: how strong, Q16.16, and a seed that should change every
/// frame.
#[derive(Clone, Copy)]
pub(crate) struct Grain {
    pub strength: i64,
    pub seed: u32,
}

/// Write the finished picture, smoothing edges and adding grain on the way.
///
/// Grain goes on last so that the smoothing does not smooth it away. It is
/// the same amount on all three channels -- film grain is in brightness,
/// not colour -- and the sum of two random numbers, so that most of it is
/// small and a little is not.
pub(crate) fn finish(
    width: usize,
    height: usize,
    picture: &[u64],
    out: &mut [u32],
    smooth_edges: bool,
    grain: Option<Grain>,
) {
    for y in 0..height {
        let inner_row = y > 0 && y + 1 < height;
        for x in 0..width {
            let index = y * width + x;
            let mut pixel = picture[index] as u32;
            if smooth_edges && inner_row && x > 0 && x + 1 < width {
                if let Some(smoothed) = smooth(picture, width, height, x, y) {
                    pixel = smoothed;
                }
            }
            if let Some(grain) = grain {
                let h = hash(x, y, grain.seed);
                let noise = (h & 0xff) as i64 + ((h >> 8) & 0xff) as i64 - 255;
                let shift = ((noise * grain.strength) >> 16) as i32;
                let channel = |s: u32| (((pixel >> s) & 0xff) as i32 + shift).clamp(0, 255) as u32;
                pixel = channel(16) << 16 | channel(8) << 8 | channel(0);
            }
            out[index] = pixel;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lens_pattern_fills_a_disc_evenly() {
        for (k, [x, y, distance]) in LENS.into_iter().enumerate() {
            let exact = ((k as f64 + 0.5) / 16.0).sqrt() * 256.0;
            assert!((distance as f64 - exact).abs() < 1.5, "ring {k}");
            let length = ((x * x + y * y) as f64).sqrt();
            assert!((length - distance as f64).abs() < 2.0, "point {k}");
        }
        // Balanced: the points do not lean to one side.
        let lean: i32 = LENS.iter().map(|p| p[0]).sum();
        assert!(lean.abs() < 256, "leans {lean}");
    }

    #[test]
    fn the_bell_sums_to_one() {
        assert_eq!(BELL.iter().sum::<i64>(), 256);
    }

    #[test]
    fn blurring_keeps_the_light_it_was_given() {
        // A single bright pixel in the middle of a dark field: however far
        // it is spread, the total is what it started as.
        let (width, height) = (64, 64);
        let mut a = vec![0u64; width * height];
        let mut b = vec![0u64; width * height];
        a[32 * width + 32] = color::pack([60000, 30000, 4096]);
        bloom_blur(width, height, &mut a, &mut b);
        let total = |i: usize| a.iter().map(|w| color::unpack(*w)[i] as i64).sum::<i64>();
        // Each of the six passes rounds every pixel.
        assert!((total(0) - 60000).abs() < 3000, "{}", total(0));
        assert!((total(1) - 30000).abs() < 3000, "{}", total(1));
        // And it has actually spread: the middle is a small part of it.
        assert!(color::unpack(a[32 * width + 32])[0] < 3000);
    }

    #[test]
    fn only_what_is_over_the_threshold_glows() {
        let (width, height) = (8, 4);
        let mut light = vec![color::pack([2000, 2000, 2000]); width * height];
        for y in 0..4 {
            for x in 4..8 {
                light[y * width + x] = color::pack([16384, 8192, 0]);
            }
        }
        let mut out = [0u64; 2];
        bloom_extract(width, height, &light, &mut out, color::ONE as i64);
        assert_eq!(color::unpack(out[0]), [0, 0, 0]);
        let glow = color::unpack(out[1]);
        // Dimmer than it was, and the same hue: red still twice green.
        assert!(glow[0] > 4000 && glow[0] < 16384, "{glow:?}");
        assert!(
            (glow[0] - 2 * glow[1]).abs() <= 2 && glow[2] == 0,
            "{glow:?}"
        );
    }

    fn plain(tonemap: Tonemap) -> Grade<'static> {
        Grade {
            tonemap,
            exposure: 1 << 16,
            glow: None,
            aberration: 0,
            vignette: 0,
        }
    }

    #[test]
    fn white_resolves_to_white_and_black_to_black() {
        let light = [
            color::pack([0, 0, 0]),
            color::pack([color::ONE; 3]),
            color::pack([65535; 3]),
        ];
        let mut out = [0u64; 3];
        resolve(3, 1, &light, &mut out, &plain(Tonemap::Clip));
        assert_eq!(out, [0x000000, 0xffffff, 0xffffff]);
        // The filmic curve keeps something back for what is brighter still.
        resolve(3, 1, &light, &mut out, &plain(Tonemap::Filmic));
        assert_eq!(out[0], 0);
        assert!((out[1] & 0xff) < 0xff && out[2] == 0xffffff, "{out:x?}");
    }

    #[test]
    fn a_vignette_darkens_the_corners_and_not_the_middle() {
        let (width, height) = (33, 33);
        let light = vec![color::pack([2048; 3]); width * height];
        let mut out = vec![0u64; width * height];
        let mut grade = plain(Tonemap::Clip);
        resolve(width, height, &light, &mut out, &grade);
        let flat = out[0];
        grade.vignette = 1 << 16;
        resolve(width, height, &light, &mut out, &grade);
        assert_eq!(out[16 * width + 16], flat);
        assert!(out[0] & 0xff < (flat & 0xff) / 2, "corner is {:#x}", out[0]);
        assert!(out[16] & 0xff > out[0] & 0xff);
    }

    #[test]
    fn fringing_parts_red_from_blue_away_from_the_middle() {
        // A white stripe down a black picture, right of centre. Red is read
        // from further out, so the stripe's red lands nearer the middle than
        // its blue.
        let (width, height) = (64, 8);
        let mut light = vec![0u64; width * height];
        for y in 0..height {
            for x in 52..56 {
                light[y * width + x] = color::pack([color::ONE; 3]);
            }
        }
        let mut out = vec![0u64; width * height];
        let mut grade = plain(Tonemap::Clip);
        grade.aberration = 1 << 15;
        resolve(width, height, &light, &mut out, &grade);
        let row = &out[4 * width..5 * width];
        let first = |shift: u32| row.iter().position(|p| (p >> shift) & 0xff > 40).unwrap();
        assert!(
            first(16) < first(8) && first(8) < first(0),
            "{} {} {}",
            first(16),
            first(8),
            first(0)
        );
        // The middle column is untouched.
        assert_eq!(out[4 * width + 32], 0);
    }

    #[test]
    fn smoothing_softens_a_staircase_and_leaves_flat_colour_alone() {
        let (width, height) = (32, 32);
        // White below a diagonal, black above it.
        let mut picture = vec![0u64; width * height];
        for y in 0..height {
            for x in 0..width {
                if x < y * 3 / 2 {
                    picture[y * width + x] = 0x00ff_ffff;
                }
            }
        }
        let mut out = vec![0u32; width * height];
        finish(width, height, &picture, &mut out, true, None);

        let greys = out
            .iter()
            .filter(|p| **p != 0 && **p != 0x00ff_ffff)
            .count();
        assert!(greys > 20, "only {greys} pixels were blended");
        // Well away from the edge nothing changed.
        assert_eq!(out[2 * width + 28], 0);
        assert_eq!(out[28 * width + 2], 0x00ff_ffff);

        finish(width, height, &picture, &mut out, false, None);
        assert!(out.iter().zip(&picture).all(|(a, b)| *a == *b as u32));
    }

    #[test]
    fn grain_averages_to_nothing_and_changes_with_the_seed() {
        let (width, height) = (64, 64);
        let picture = vec![0x0080_8080u64; width * height];
        let mut one = vec![0u32; width * height];
        let mut two = vec![0u32; width * height];
        let grain = |seed| {
            Some(Grain {
                strength: 1 << 13,
                seed,
            })
        };
        finish(width, height, &picture, &mut one, false, grain(1));
        finish(width, height, &picture, &mut two, false, grain(2));
        assert_ne!(one, two);
        let mean = one.iter().map(|p| (p & 0xff) as i64).sum::<i64>() / (width * height) as i64;
        assert!((mean - 0x80).abs() <= 1, "mean {mean:#x}");
        assert!(one.iter().any(|p| p & 0xff > 0x90) && one.iter().any(|p| p & 0xff < 0x70));
        // The same on every channel.
        assert!(one.iter().all(|p| (p >> 16) & 0xff == p & 0xff));
    }
}

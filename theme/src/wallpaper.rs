//! Wallpapers, drawn from three colours with integer arithmetic.
//!
//! They are computed once, when the desktop starts or the theme changes,
//! into pixels the renderer then shows like any window's. Everything is
//! smooth on purpose: glass shows what is behind it bent and frosted, and
//! the soft shapes are what make that visible.
//!
//! A little ordered dither is added at the end. Gradients this wide over a
//! dark range band visibly in eight bits a channel without it.

use crate::{Color, Theme, Wallpaper};

/// `0x00RRGGBB`.
fn pack(rgb: [i32; 3]) -> u32 {
    let c = |v: i32| v.clamp(0, 255) as u32;
    c(rgb[0]) << 16 | c(rgb[1]) << 8 | c(rgb[2])
}

fn rgb(color: Color) -> [i32; 3] {
    [color.r as i32, color.g as i32, color.b as i32]
}

/// `a` towards `b` by `t` out of 4096.
fn mix(a: [i32; 3], b: [i32; 3], t: i32) -> [i32; 3] {
    let t = t.clamp(0, 4096);
    [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t / 4096)
}

/// A colour darkened (negative) or lightened (positive) by `amount` out of
/// 4096.
fn shade(color: [i32; 3], amount: i32) -> [i32; 3] {
    if amount < 0 {
        mix(color, [0, 0, 0], -amount)
    } else {
        mix(color, [255, 255, 255], amount)
    }
}

/// `sin(turn * 2 pi / 4096)` in 4096ths: Bhaskara's approximation, within a
/// fraction of a percent, which no wallpaper can show.
pub fn sin(turn: i32) -> i32 {
    let t = turn.rem_euclid(4096);
    let (half, sign) = if t < 2048 { (t, 1) } else { (t - 2048, -1) };
    // On a half turn x in 0..=2048 standing for 0..=pi:
    // sin x ~ 16 x (pi - x) / (5 pi^2 - 4 x (pi - x)).
    let x = half as i64;
    let p = 2048i64;
    let top = 16 * x * (p - x);
    let bottom = 5 * p * p - 4 * x * (p - x);
    (sign * (top * 4096 / bottom)) as i32
}

/// A soft round glow: 1 at its centre, falling off as `1 / (1 + d^2/r^2)^2`.
fn glow(dx: i64, dy: i64, radius: i64) -> i32 {
    let r2 = (radius * radius).max(1);
    let q = 4096 + (dx * dx + dy * dy) * 4096 / r2;
    (4096i64 * 4096 / q * 4096 / q) as i32
}

/// Draw `theme`'s wallpaper into `pixels`, `width` by `height`, rows
/// packed.
pub fn paint(theme: &Theme, pixels: &mut [u32], width: u32, height: u32) {
    let (w, h) = (width as i64, height as i64);
    let colors = theme.wallpaper_colors.map(rgb);
    // A dark wallpaper keeps its shapes out of the deep shadows; a light
    // one washes them pale.
    let base = if theme.dark {
        shade(colors[0], -2600)
    } else {
        shade(colors[0], 2400)
    };
    let tone = |c: [i32; 3]| {
        if theme.dark {
            c
        } else {
            shade(c, 1600)
        }
    };

    for y in 0..h {
        for x in 0..w {
            let color = match theme.wallpaper {
                Wallpaper::Gradient => mix(colors[0], colors[1], (y * 4096 / h.max(1)) as i32),
                Wallpaper::Aurora => {
                    // Three glows, wide and overlapping, each pulling the
                    // base towards its colour.
                    let blobs = [
                        (w * 22 / 100, h * 30 / 100, w * 38 / 100, colors[1]),
                        (w * 78 / 100, h * 70 / 100, w * 42 / 100, colors[2]),
                        (w * 58 / 100, h * 8 / 100, w * 26 / 100, colors[0]),
                    ];
                    let mut c = mix(base, shade(colors[0], -1200), (y * 4096 / h.max(1)) as i32);
                    for (cx, cy, r, blob) in blobs {
                        // Squashed vertically, like light through a slot.
                        let g = glow(x - cx, (y - cy) * 3 / 2, r);
                        c = mix(c, tone(blob), g * 7 / 8);
                    }
                    c
                }
                Wallpaper::Ribbons => {
                    // Diagonal bands crossing the screen at a slant, each
                    // with a soft edge and a sheen along it.
                    let mut c = mix(base, shade(base, -1500), (y * 4096 / h.max(1)) as i32);
                    let along = (x * 4096 / w.max(1)) as i32;
                    for (index, (slope, offset, thickness, color)) in [
                        (-38i64, 62i64, 18i64, colors[1]),
                        (-30, 82, 12, colors[2]),
                        (-46, 98, 9, shade(colors[1], 1200)),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let wave = sin(along / 2 + index as i32 * 900) as i64 * h / 4096 / 14;
                        let centre = h * offset / 100 + x * slope * h / (100 * w.max(1)) + wave;
                        let half = h * thickness / 200;
                        let distance = (y - centre).abs();
                        if distance < half + 24 {
                            let edge = ((half + 24 - distance) * 4096 / 24).clamp(0, 4096) as i32;
                            let sheen = (4096 - (distance * 4096 / half.max(1))).clamp(0, 4096) as i32;
                            let lit = shade(tone(color), sheen / 5 - 600);
                            c = mix(c, lit, edge * 9 / 10);
                        }
                    }
                    c
                }
                Wallpaper::Dunes => {
                    // A sky from the third colour down to the second, and
                    // five ridges in front of it, each darker and nearer.
                    let sky = mix(tone(colors[2]), tone(colors[1]), (y * 4096 / h.max(1)) as i32);
                    let mut c = mix(sky, base, 600);
                    let along = (x * 4096 / w.max(1)) as i32;
                    for layer in 0..5i64 {
                        let crest = h * (45 + layer * 10) / 100
                            + (sin(along * (2 + layer as i32) / 3 + layer as i32 * 1300) as i64
                                * h
                                / 4096
                                / (12 + layer * 2))
                            + (sin(along * 5 + layer as i32 * 700) as i64 * h / 4096 / 90);
                        if y >= crest {
                            let depth = (layer * 4096 / 5) as i32;
                            let ridge = mix(tone(colors[1]), base, depth + 600);
                            // A rim of light along each crest.
                            let lip = (4096 - ((y - crest) * 4096 / 6).min(4096)) as i32;
                            c = shade(ridge, lip / 10);
                        }
                    }
                    c
                }
            };
            // Bayer 4x4, a third of a step either way.
            const BAYER: [i32; 16] = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];
            let d = BAYER[((y & 3) * 4 + (x & 3)) as usize] - 8;
            let dithered = color.map(|v| v + d / 6);
            pixels[(y * w + x) as usize] = pack(dithered);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_is_close_enough() {
        for turn in (0..4096).step_by(7) {
            let want = (turn as f64 / 4096.0 * core::f64::consts::TAU).sin() * 4096.0;
            assert!((sin(turn) as f64 - want).abs() < 12.0, "{turn}");
        }
    }

    #[test]
    fn every_wallpaper_paints_every_pixel_and_is_not_flat() {
        for wallpaper in [
            Wallpaper::Aurora,
            Wallpaper::Ribbons,
            Wallpaper::Dunes,
            Wallpaper::Gradient,
        ] {
            for dark in [true, false] {
                let theme = Theme {
                    wallpaper,
                    dark,
                    ..crate::CORRODE
                };
                let (w, h) = (64, 40);
                let mut pixels = vec![0xdead_beefu32; w * h];
                paint(&theme, &mut pixels, w as u32, h as u32);
                assert!(pixels.iter().all(|p| p >> 24 == 0), "{wallpaper:?}");
                let distinct = pixels.iter().collect::<std::collections::BTreeSet<_>>().len();
                assert!(distinct > 20, "{wallpaper:?} has {distinct} colours");
            }
        }
    }
}

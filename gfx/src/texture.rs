//! Textures: pictures wrapped onto surfaces.
//!
//! A [`Texture`] is a block of `0x00RRGGBB` pixels the caller owns, with sides
//! that are powers of two so that wrapping is a mask rather than a divide.
//!
//! # Mip levels
//!
//! A texture seen from far away covers fewer screen pixels than it has texels,
//! and picking one texel per pixel out of many is noise: a tiled floor turns
//! to shimmering static towards the horizon. The cure is to keep the picture
//! at half size, quarter size and so on -- its *mip levels* -- and read from
//! the one whose texels are about a pixel across. The whole chain is a third
//! bigger than the picture alone, and lives in the same slice straight after
//! it.
//!
//! Reads blend the four nearest texels and the two nearest levels, so nothing
//! snaps as a surface moves.

use corrode_render::Color;

/// A picture to wrap onto a surface.
#[derive(Clone, Copy, Debug)]
pub struct Texture<'a> {
    pixels: &'a [u32],
    /// log2 of the width and height of level 0.
    width: u8,
    height: u8,
    levels: u8,
}

const fn log2_exact(side: u32) -> Option<u8> {
    if side == 0 || !side.is_power_of_two() || side > 4096 {
        None
    } else {
        Some(side.trailing_zeros() as u8)
    }
}

impl<'a> Texture<'a> {
    /// How many pixels a `width` x `height` texture needs with all its mip
    /// levels, down to one texel.
    pub const fn len_with_mips(width: u32, height: u32) -> usize {
        let (mut w, mut h, mut total) = (width as usize, height as usize, 0);
        loop {
            total += w * h;
            if w <= 1 && h <= 1 {
                return total;
            }
            w = if w > 1 { w / 2 } else { 1 };
            h = if h > 1 { h / 2 } else { 1 };
        }
    }

    /// Wrap pixels as a texture. Rows run top to bottom.
    ///
    /// If `pixels` is exactly `width * height` long it has no mip levels; if
    /// it is [`Self::len_with_mips`] long, the rest is taken to be the chain
    /// [`build_mips`] writes. Anything else, or a side that is not a power of
    /// two up to 4096, is refused.
    pub fn new(pixels: &'a [u32], width: u32, height: u32) -> Option<Self> {
        let (w, h) = (log2_exact(width)?, log2_exact(height)?);
        let base = width as usize * height as usize;
        let levels = if pixels.len() == base {
            1
        } else if pixels.len() == Self::len_with_mips(width, height) {
            w.max(h) + 1
        } else {
            return None;
        };
        Some(Self {
            pixels,
            width: w,
            height: h,
            levels,
        })
    }

    pub const fn width(&self) -> u32 {
        1 << self.width
    }

    pub const fn height(&self) -> u32 {
        1 << self.height
    }

    /// log2 of the number of texels in level 0.
    pub(crate) const fn log2_area(&self) -> i32 {
        self.width as i32 + self.height as i32
    }

    /// Where a level starts, and log2 of its sides.
    #[inline]
    fn level(&self, level: u32) -> (usize, u32, u32) {
        let (mut w, mut h, mut start) = (self.width as u32, self.height as u32, 0usize);
        for _ in 0..level {
            start += 1usize << (w + h);
            w = w.saturating_sub(1);
            h = h.saturating_sub(1);
        }
        (start, w, h)
    }

    /// Blend the four texels around `(u, v)` on one level.
    ///
    /// `u` and `v` are Q16.16 and wrap: 0 and 1 are the same edge.
    #[inline]
    fn bilinear(&self, level: u32, u: i32, v: i32) -> u32 {
        let (start, w, h) = self.level(level);
        // Texel centres sit at half-integers, so step back half a texel
        // before splitting into a whole part and a blend weight.
        let x = ((u as i64) << w) - 0x8000;
        let y = ((v as i64) << h) - 0x8000;
        let (xm, ym) = ((1usize << w) - 1, (1usize << h) - 1);
        let (x0, y0) = ((x >> 16) as usize & xm, (y >> 16) as usize & ym);
        let (x1, y1) = ((x0 + 1) & xm, (y0 + 1) & ym);
        let (fx, fy) = ((x >> 8) as u32 & 0xff, (y >> 8) as u32 & 0xff);

        let texels = &self.pixels[start..start + (1usize << (w + h))];
        let top = mix(texels[(y0 << w) + x0], texels[(y0 << w) + x1], fx);
        let bottom = mix(texels[(y1 << w) + x0], texels[(y1 << w) + x1], fx);
        mix(top, bottom, fy)
    }

    /// The colour at `(u, v)`, as `0x00RRGGBB`.
    ///
    /// `lod` says how many texels a screen pixel spans there, as a base-2
    /// logarithm times 256: zero or less reads the full-size picture, 256
    /// the half-size one, and values in between blend the two.
    #[inline]
    pub(crate) fn sample(&self, u: i32, v: i32, lod: i32) -> u32 {
        let last = (self.levels as i32 - 1) * 256;
        let lod = lod.clamp(0, last);
        let (level, between) = ((lod >> 8) as u32, lod as u32 & 0xff);
        let near = self.bilinear(level, u, v);
        if between == 0 {
            return near;
        }
        mix(near, self.bilinear(level + 1, u, v), between)
    }
}

/// `a` at `t = 0`, `b` at `t = 256`, on packed pixels: red and blue share one
/// multiply, green gets the other.
#[inline]
pub(crate) fn mix(a: u32, b: u32, t: u32) -> u32 {
    const RB: u32 = 0x00ff_00ff;
    const G: u32 = 0x0000_ff00;
    let s = 256 - t;
    let rb = (((a & RB) * s + (b & RB) * t) >> 8) & RB;
    let g = (((a & G) * s + (b & G) * t) >> 8) & G;
    rb | g
}

pub(crate) const fn pack(color: Color) -> u32 {
    (color.r as u32) << 16 | (color.g as u32) << 8 | color.b as u32
}

/// Fill in the mip levels of a texture whose full-size picture is already in
/// the first `width * height` pixels.
///
/// Each level is the one before it with every two-by-two block averaged.
/// Returns `false`, having written nothing, if `pixels` is not
/// [`Texture::len_with_mips`] long.
pub fn build_mips(pixels: &mut [u32], width: u32, height: u32) -> bool {
    let (Some(_), Some(_)) = (log2_exact(width), log2_exact(height)) else {
        return false;
    };
    if pixels.len() != Texture::len_with_mips(width, height) {
        return false;
    }
    let (mut w, mut h) = (width as usize, height as usize);
    let mut start = 0;
    while w > 1 || h > 1 {
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        let (above, below) = pixels[start..].split_at_mut(w * h);
        for y in 0..nh {
            for x in 0..nw {
                // A level one texel wide or tall has no neighbour that way.
                let (x0, y0) = ((2 * x).min(w - 1), (2 * y).min(h - 1));
                let (x1, y1) = ((2 * x + 1).min(w - 1), (2 * y + 1).min(h - 1));
                let block = [
                    above[y0 * w + x0],
                    above[y0 * w + x1],
                    above[y1 * w + x0],
                    above[y1 * w + x1],
                ];
                let channel = |shift: u32| {
                    let sum: u32 = block.iter().map(|p| (p >> shift) & 0xff).sum();
                    ((sum + 2) / 4) << shift
                };
                below[y * nw + x] = channel(16) | channel(8) | channel(0);
            }
        }
        start += w * h;
        (w, h) = (nw, nh);
    }
    true
}

/// Paint level 0 of a texture from a function of the texel's position.
pub fn paint(
    pixels: &mut [u32],
    width: u32,
    height: u32,
    mut color: impl FnMut(u32, u32) -> Color,
) {
    let rows = pixels.chunks_exact_mut(width.max(1) as usize);
    for (y, row) in rows.take(height as usize).enumerate() {
        for (x, pixel) in row.iter_mut().enumerate() {
            *pixel = pack(color(x as u32, y as u32));
        }
    }
}

/// A checkerboard of `cells` squares a side, with all its mip levels.
///
/// The plain test pattern: it shows at a glance whether a surface is mapped
/// straight, in perspective, and without shimmer.
pub fn checker(pixels: &mut [u32], size: u32, cells: u32, a: Color, b: Color) -> bool {
    if log2_exact(size).is_none() || pixels.len() != Texture::len_with_mips(size, size) {
        return false;
    }
    let cell = (size / cells.max(1)).max(1);
    paint(pixels, size, size, |x, y| {
        if (x / cell + y / cell).is_multiple_of(2) {
            a
        } else {
            b
        }
    });
    build_mips(pixels, size, size)
}

/// Staggered bricks with mortar between them, with all its mip levels.
pub fn bricks(pixels: &mut [u32], size: u32, brick: Color, mortar: Color) -> bool {
    if log2_exact(size).is_none() || pixels.len() != Texture::len_with_mips(size, size) {
        return false;
    }
    let (rows, across) = (8u32, 4u32);
    let (row_height, brick_width) = ((size / rows).max(1), (size / across).max(1));
    let gap = (size / 64).max(1);
    paint(pixels, size, size, |x, y| {
        let row = y / row_height;
        let x = x + (row % 2) * brick_width / 2;
        if y % row_height < gap || x % brick_width < gap {
            return mortar;
        }
        // Each brick a little lighter or darker than its neighbours, by a
        // hash of which brick it is.
        let which = (row * 31 + x / brick_width % across * 17).wrapping_mul(2_654_435_761);
        let shade = 224 + (which >> 27);
        let tone = |c: u8| ((c as u32 * shade) >> 8).min(255) as u8;
        Color::rgb(tone(brick.r), tone(brick.g), tone(brick.b))
    });
    build_mips(pixels, size, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mip_chain_is_a_third_longer() {
        assert_eq!(Texture::len_with_mips(4, 4), 16 + 4 + 1);
        assert_eq!(Texture::len_with_mips(8, 2), 16 + 4 + 2 + 1);
        assert_eq!(Texture::len_with_mips(1, 1), 1);
    }

    #[test]
    fn sizes_that_do_not_fit_are_refused() {
        assert!(Texture::new(&[0; 16], 4, 4).is_some());
        assert!(Texture::new(&[0; 21], 4, 4).is_some());
        assert!(Texture::new(&[0; 20], 4, 4).is_none());
        assert!(Texture::new(&[0; 9], 3, 3).is_none());
        assert!(!build_mips(&mut [0; 16], 4, 4));
    }

    #[test]
    fn a_texel_centre_reads_back_exactly() {
        let mut pixels = [0u32; 16];
        paint(&mut pixels, 4, 4, |x, y| {
            Color::rgb(x as u8 * 60, y as u8 * 60, 7)
        });
        let texture = Texture::new(&pixels, 4, 4).unwrap();
        for y in 0..4 {
            for x in 0..4 {
                let (u, v) = ((x * 2 + 1) << 13, (y * 2 + 1) << 13);
                assert_eq!(texture.sample(u, v, 0), pixels[(y * 4 + x) as usize]);
            }
        }
    }

    #[test]
    fn between_texels_is_between_their_colours() {
        let pixels = [0x000000, 0xffffff, 0x000000, 0xffffff];
        let texture = Texture::new(&pixels, 2, 2).unwrap();
        // Half way between the two columns, on a texel row.
        let middle = texture.sample(1 << 15, 1 << 14, 0);
        assert!((0x7e..=0x81).contains(&(middle & 0xff)), "{middle:#x}");
        // Wrapping: just left of 0 is the right-hand column.
        assert_eq!(texture.sample(-(1 << 14), 1 << 14, 0), 0xffffff);
    }

    #[test]
    fn far_away_a_checkerboard_is_its_average() {
        let mut pixels = [0u32; Texture::len_with_mips(16, 16)];
        assert!(checker(&mut pixels, 16, 8, Color::WHITE, Color::BLACK));
        let texture = Texture::new(&pixels, 16, 16).unwrap();
        // Up close, a square is one colour or the other.
        assert_eq!(texture.sample(1 << 11, 1 << 11, 0), 0xffffff);
        // Level 1 is still a checkerboard, one texel a square. From level 2
        // on each texel covers as much white as black.
        for lod in [512, 768, 4000] {
            let grey = texture.sample(12345, 54321, lod);
            assert_eq!(grey, 0x808080, "lod {lod}");
        }
    }
}

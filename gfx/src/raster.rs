//! Triangles to pixels.
//!
//! This pass decides, for every pixel, which surface is in front and what
//! that surface is like there -- its colour, which way it faces, how far away
//! it is, how it is moving. It does no lighting. That is written down in two
//! words per pixel (the *G-buffer*, see [`pack_surface`] and [`pack_shape`])
//! and every later pass works from those, so the cost of lighting and of
//! every effect is the number of pixels on screen, not the number of
//! triangles behind them.
//!
//! # Depth
//!
//! Depth is stored as `2^26 / z`: larger is nearer, and zero means nothing
//! was drawn there. The reciprocal is the right thing to keep for two
//! reasons. It is what perspective makes linear across a triangle on screen,
//! so it interpolates with an add per pixel and no divide. And it spends its
//! precision where the eye does, on what is close.
//!
//! # Coverage
//!
//! A pixel belongs to a triangle if its centre is inside all three edges.
//! Corners are snapped to 1/256 of a pixel and the edge tests are exact
//! integers, so two triangles sharing an edge agree about every pixel along
//! it: each is drawn by exactly one of them, with no gap and no double
//! cover. An edge that passes exactly through a pixel centre gives it to the
//! triangle on its right or below (the "top-left rule").

use crate::math::{dot, log2_q8, log2_q8_wide, normalize, Transform};
use crate::scene::Item;
use crate::texture::Texture;

/// Fractional bits in a screen coordinate.
const SUB: i32 = 8;
const SUB_ONE: i64 = 1 << SUB;

/// The furthest thing depth can tell from nothing: 32768 units.
const DEPTH_MIN: i64 = 1 << 11;

/// `2^26 / z` for a view-space `z` in Q16.16.
#[inline]
pub(crate) fn depth_of(z: i64) -> u32 {
    ((1i64 << 42) / z.max(DEPTH_MIN)) as u32
}

/// View-space `z`, Q16.16, from a stored depth.
#[inline]
pub(crate) fn z_of(depth: u32) -> i64 {
    (1i64 << 42) / (depth as i64).max(DEPTH_MIN)
}

/// The first word of a pixel: what the surface is made of.
///
/// | bits    | what                                              |
/// |---------|---------------------------------------------------|
/// | 0..24   | its colour, gamma-encoded `RRGGBB`                |
/// | 24..32  | how much it reflects                              |
/// | 32..40  | how shiny it is                                   |
/// | 40..48  | how much light it gives off                       |
/// | 48..64  | how far it moved, in half pixels, `x` then `y`    |
#[inline]
pub(crate) fn pack_surface(rgb: u32, props: u64, velocity: [i32; 2]) -> u64 {
    let v = velocity.map(|c| c.clamp(-127, 127) as i8 as u8 as u64);
    rgb as u64 | props | v[0] << 48 | v[1] << 56
}

/// The material part of [`pack_surface`], which is the same for every pixel
/// of a draw.
pub(crate) fn pack_props(reflect: u8, shine: u8, glow: u8) -> u64 {
    (reflect as u64) << 24 | (shine as u64) << 32 | (glow as u64) << 40
}

/// How far a pixel moved while the shutter was open, in half pixels.
#[inline]
pub(crate) fn velocity(surface: u64) -> [i32; 2] {
    [(surface >> 48) as i8 as i32, (surface >> 56) as i8 as i32]
}

/// The second word of a pixel: where the surface is and which way it faces.
/// Depth in the low half, then the view-space normal at ten bits a
/// component.
#[inline]
pub(crate) fn pack_shape(depth: u32, normal: [i32; 3]) -> u64 {
    let n = normal.map(|c| ((c * 511) >> 14).clamp(-511, 511) as u64 & 0x3ff);
    depth as u64 | n[0] << 32 | n[1] << 42 | n[2] << 52
}

/// The view-space normal of a pixel, Q14.
#[inline]
pub(crate) fn normal(shape: u64) -> [i32; 3] {
    let field = |shift: u32| ((((shape >> shift) as u32) << 22) as i32) >> 22;
    // 511 steps to a unit: near enough 32 to the Q14 step.
    [field(32) * 32, field(42) * 32, field(52) * 32]
}

/// A corner of a triangle as the camera sees it.
#[derive(Clone, Copy, Default)]
pub(crate) struct Corner {
    /// View space: `x` right, `y` up, `z` ahead. Q16.16.
    pub p: [i32; 3],
    /// View-space normal, a unit in Q14.
    pub n: [i32; 3],
    pub uv: [i32; 2],
    /// Where it was, in the view of the camera that was, when the shutter
    /// opened.
    pub q: [i32; 3],
}

/// The camera's projection onto a picture of a given size.
#[derive(Clone, Copy)]
pub(crate) struct Lens {
    pub width: i32,
    pub height: i32,
    /// Focal length in pixels, Q16.16.
    pub focal: i64,
    pub near: i32,
}

impl Lens {
    /// Where a view-space point lands on screen, in 1/256 pixel, and its
    /// depth.
    #[inline]
    fn project(&self, p: [i32; 3]) -> ([i32; 2], u32) {
        let z = p[2].max(self.near) as i64;
        let x = (p[0] as i64 * self.focal / z) >> (16 - SUB);
        let y = (p[1] as i64 * self.focal / z) >> (16 - SUB);
        let limit = 1i64 << 26;
        (
            [
                (self.width as i64 * (SUB_ONE / 2) + x).clamp(-limit, limit) as i32,
                (self.height as i64 * (SUB_ONE / 2) - y).clamp(-limit, limit) as i32,
            ],
            depth_of(z),
        )
    }

    /// How far inside one side of the view a point is; negative is outside.
    /// The five sides are the near plane and the four edges of the picture,
    /// each with a couple of pixels to spare.
    #[inline]
    fn inside(&self, side: usize, p: [i32; 3]) -> i64 {
        let [x, y, z] = p.map(|c| c as i64);
        let half_width = (self.width as i64 / 2 + 2) << 16;
        let half_height = (self.height as i64 / 2 + 2) << 16;
        match side {
            0 => z - self.near as i64,
            1 => z * half_width + x * self.focal,
            2 => z * half_width - x * self.focal,
            3 => z * half_height + y * self.focal,
            _ => z * half_height - y * self.focal,
        }
    }
}

/// The point where an edge crosses a side of the view.
///
/// Always worked out from the end that is inside, whichever way round the
/// edge was given. Two triangles sharing the edge traverse it in opposite
/// directions, and they must cut it at the same point to the last bit or a
/// seam opens between them.
fn cut(inside: &Corner, outside: &Corner, d_in: i64, d_out: i64) -> Corner {
    let span = d_in - d_out;
    let shift = (33 - span.leading_zeros() as i32).max(0);
    let t = (((d_in >> shift) << 30) / (span >> shift).max(1)).clamp(0, 1 << 30);
    let mix = |a: i32, b: i32| (a as i64 + (((b as i64 - a as i64) * t) >> 30)) as i32;
    let mix3 = |a: [i32; 3], b: [i32; 3]| [0, 1, 2].map(|i| mix(a[i], b[i]));
    Corner {
        p: mix3(inside.p, outside.p),
        n: mix3(inside.n, outside.n),
        uv: [0, 1].map(|i| mix(inside.uv[i], outside.uv[i])),
        q: mix3(inside.q, outside.q),
    }
}

/// The most corners a triangle can have after five cuts.
const POLYGON: usize = 9;

/// Trim a triangle to the view. Returns how many corners are left.
fn clip(lens: &Lens, triangle: [Corner; 3], out: &mut [Corner; POLYGON]) -> usize {
    let mut count = 3;
    out[..3].copy_from_slice(&triangle);

    for side in 0..5 {
        let mut distances = [0i64; POLYGON];
        let mut outside = 0;
        for i in 0..count {
            distances[i] = lens.inside(side, out[i].p);
            outside += (distances[i] < 0) as usize;
        }
        if outside == 0 {
            continue;
        }
        if outside == count {
            return 0;
        }

        let source = *out;
        let mut kept = 0;
        for i in 0..count {
            let j = (i + 1) % count;
            let (a, b) = (&source[i], &source[j]);
            let (da, db) = (distances[i], distances[j]);
            if da >= 0 {
                out[kept] = *a;
                kept += 1;
                if db < 0 {
                    out[kept] = cut(a, b, da, db);
                    kept += 1;
                }
            } else if db >= 0 {
                out[kept] = cut(b, a, db, da);
                kept += 1;
            }
        }
        count = kept;
        if count < 3 {
            return 0;
        }
    }
    count
}

/// How one value varies across a triangle on screen: what it is at the first
/// pixel, and how much it changes a pixel across and a pixel down.
#[derive(Clone, Copy)]
struct Plane {
    v: i64,
    dx: i64,
    dy: i64,
}

/// A triangle ready to be filled.
struct Setup {
    /// The pixels its bounding box covers: `x0..x1`, `y0..y1`.
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    /// The three edge tests at the centre of pixel `(x0, y0)`, and how each
    /// changes a pixel across and a pixel down. A pixel is covered when none
    /// is negative.
    w: [i64; 3],
    wx: [i64; 3],
    wy: [i64; 3],
    /// The weights of the second and third corners at pixel `(x0, y0)`, Q52,
    /// and their change per pixel, Q60. The first corner's is what is left.
    start: [i128; 2],
    across: [i128; 2],
    down: [i128; 2],
}

impl Setup {
    /// `s` are the corners in 1/256 pixel, wound so that the area below
    /// comes out positive. `None` if no pixel centre can be inside.
    fn new(s: [[i32; 2]; 3], width: i32, height: i32) -> Option<Self> {
        let [x, y] = [0, 1].map(|axis| [0, 1, 2].map(|i| s[i][axis] as i64));
        let area = (x[1] - x[0]) * (y[2] - y[0]) - (y[1] - y[0]) * (x[2] - x[0]);
        if area <= 0 {
            return None;
        }

        // The first and last pixels whose centres the box reaches.
        let half = SUB_ONE / 2;
        let first = |v: [i64; 3]| (v.into_iter().min().unwrap_or(0) - half + SUB_ONE - 1) >> SUB;
        let last = |v: [i64; 3]| (v.into_iter().max().unwrap_or(0) - half) >> SUB;
        let (x0, y0) = (first(x).max(0), first(y).max(0));
        let (x1, y1) = (
            (last(x) + 1).min(width as i64),
            (last(y) + 1).min(height as i64),
        );
        if x0 >= x1 || y0 >= y1 {
            return None;
        }

        // Edge i is the one opposite corner i, so its test is that corner's
        // weight: zero along the edge, `area` at the corner.
        let (px, py) = (x0 * SUB_ONE + half, y0 * SUB_ONE + half);
        let mut w = [0i64; 3];
        let mut wx = [0i64; 3];
        let mut wy = [0i64; 3];
        let mut steepest = 0;
        for i in 0..3 {
            let (a, b) = ((i + 1) % 3, (i + 2) % 3);
            let (dx, dy) = (x[b] - x[a], y[b] - y[a]);
            w[i] = dx * (py - y[a]) - dy * (px - x[a]);
            wx[i] = -dy * SUB_ONE;
            wy[i] = dx * SUB_ONE;
            steepest = steepest.max(wx[i].abs()).max(wy[i].abs());
        }

        // A sliver less than a thousandth of a pixel thick: its weights
        // change too fast to hold, and it covers nothing worth drawing.
        if steepest > area.saturating_mul(1024) {
            return None;
        }

        let inverse = (1i128 << 80) / area as i128;
        let start = [1, 2].map(|i| (w[i] as i128 * inverse) >> 28);
        let across = [1, 2].map(|i| (wx[i] as i128 * inverse) >> 20);
        let down = [1, 2].map(|i| (wy[i] as i128 * inverse) >> 20);

        // The top-left rule: a pixel centre exactly on an edge belongs to
        // the triangle only if that edge is its top or its left.
        for i in 0..3 {
            let (a, b) = ((i + 1) % 3, (i + 2) % 3);
            let (dx, dy) = (x[b] - x[a], y[b] - y[a]);
            let top_left = dy < 0 || (dy == 0 && dx > 0);
            if !top_left {
                w[i] -= 1;
            }
        }

        Some(Self {
            x0: x0 as i32,
            y0: y0 as i32,
            x1: x1 as i32,
            y1: y1 as i32,
            w,
            wx,
            wy,
            start,
            across,
            down,
        })
    }

    /// How a value that is `a` at each corner varies across the triangle.
    fn plane(&self, a: [i64; 3]) -> Plane {
        let (d1, d2) = ((a[1] - a[0]) as i128, (a[2] - a[0]) as i128);
        let blend = |weights: [i128; 2], shift: u32| ((weights[0] * d1 + weights[1] * d2) >> shift) as i64;
        Plane {
            v: a[0].wrapping_add(blend(self.start, 52)),
            dx: blend(self.across, 60),
            dy: blend(self.down, 60),
        }
    }
}

/// The two words of every pixel.
pub(crate) struct Buffers<'b> {
    pub surface: &'b mut [u64],
    pub shape: &'b mut [u64],
}

/// What is the same for every pixel of one draw.
pub(crate) struct Surface<'a> {
    /// The material's colour, `0x00RRGGBB`.
    pub tint: u32,
    pub texture: Option<&'a Texture<'a>>,
    pub props: u64,
    pub two_sided: bool,
    /// The model reaches the screen through an odd number of mirrors, which
    /// turns anticlockwise into clockwise. The camera is always one: the
    /// world's `z` comes towards the viewer and the view's goes away.
    pub mirrored: bool,
    /// Whether anything is going to read how far pixels moved.
    pub moving: bool,
}

/// Multiply two packed colours, channel by channel.
#[inline]
fn tint(texel: u32, tint: u32) -> u32 {
    if tint == 0x00ff_ffff {
        return texel;
    }
    let channel = |shift: u32| {
        let product = ((texel >> shift) & 0xff) * ((tint >> shift) & 0xff);
        corrode_render::blend::div255(product) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// The part of a texture's mip level that is the same across a whole
/// triangle, as a base-2 logarithm times 256.
///
/// How many texels a screen pixel spans is `rho z / f` when a surface faces
/// the camera -- `rho` texels to a world unit, `z / f` world units to a pixel
/// -- and more than that along the direction it leans away. For a flat
/// triangle the lean is `h / z`, where `h` is how far the camera is from the
/// triangle's plane, so the span along the lean is `rho z^2 / (f h)`. Taking
/// the level for the longer span blurs the other direction; taking the
/// shorter shimmers. This takes the mean of the two logarithms:
///
/// ```text
/// level = log2 rho - log2 f - (log2 h) / 2  +  (3/2) log2 z
/// ```
///
/// Everything but the last term is constant across the triangle, and that
/// term comes straight off the depth with a count of leading zeros.
fn lod_constant(lens: &Lens, texture: &Texture<'_>, c: &[Corner; 3]) -> i32 {
    let edge = |i: usize| [0, 1, 2].map(|k| (c[i].p[k] as i64 - c[0].p[k] as i64) >> 4);
    let (e1, e2) = (edge(1), edge(2));
    let cross = [
        e1[1] * e2[2] - e1[2] * e2[1],
        e1[2] * e2[0] - e1[0] * e2[2],
        e1[0] * e2[1] - e1[1] * e2[0],
    ];
    let square: u128 = cross.iter().map(|c| (*c as i128 * *c as i128) as u128).sum();
    let uv = |i: usize, k: usize| c[i].uv[k] as i64 - c[0].uv[k] as i64;
    let uv_area = (uv(1, 0) * uv(2, 1) - uv(2, 0) * uv(1, 1)).unsigned_abs();
    if square == 0 || uv_area == 0 {
        return 0;
    }
    let toward: i128 = (0..3).map(|k| cross[k] as i128 * (c[0].p[k] as i128 >> 4)).sum();

    // Twice the triangle's area in world units, as a logarithm; then the
    // distance to its plane, the texel density, and the focal length.
    let area = log2_q8_wide(square) / 2 - 24 * 256;
    let distance = log2_q8_wide(toward.unsigned_abs()) - 36 * 256 - area;
    let density = (log2_q8(uv_area) - 32 * 256 + texture.log2_area() * 256 - area) / 2;
    let focal = log2_q8(lens.focal as u64) - 16 * 256;
    density - focal - distance / 2
}

/// Fill one triangle of a clipped polygon into the G-buffer.
fn fill(
    lens: &Lens,
    out: &mut Buffers<'_>,
    surface: &Surface<'_>,
    lod: i32,
    mut c: [Corner; 3],
) -> bool {
    let mut screen = [[0i32; 2]; 3];
    let mut depth = [0u32; 3];
    for i in 0..3 {
        (screen[i], depth[i]) = lens.project(c[i].p);
    }

    // On a screen whose `y` runs downwards, a triangle seen from the front
    // -- corners anticlockwise -- has a negative area by the formula the
    // setup uses. Swap two corners to make it positive; a triangle that is
    // already positive is being seen from behind. A model that has been
    // turned inside out on its way here has it the other way round.
    let [x, y] = [0, 1].map(|axis| [0, 1, 2].map(|i| screen[i][axis] as i64));
    let area = (x[1] - x[0]) * (y[2] - y[0]) - (y[1] - y[0]) * (x[2] - x[0]);
    let back = (area > 0) == surface.mirrored;
    if back && !surface.two_sided {
        return false;
    }
    if area < 0 {
        c.swap(1, 2);
        screen.swap(1, 2);
        depth.swap(1, 2);
    }
    let Some(setup) = Setup::new(screen, lens.width, lens.height) else {
        return false;
    };

    let d = depth.map(|d| d as i64);
    let depth_plane = setup.plane(d.map(|d| d << 16));
    let normal_planes = [0, 1, 2].map(|k| setup.plane([0, 1, 2].map(|i| (c[i].n[k] as i64) << 16)));

    // Texture coordinates do not vary evenly across the screen, but
    // coordinates divided by depth do. Interpolate those and divide back.
    let uv_planes = surface
        .texture
        .map(|_| [0, 1].map(|k| setup.plane([0, 1, 2].map(|i| (c[i].uv[k] as i64 * d[i]) << 8))));

    let motion_planes = if surface.moving && c.iter().all(|c| c.q[2] >= lens.near) {
        let moved = [0, 1, 2].map(|i| {
            let (was, _) = lens.project(c[i].q);
            [screen[i][0] - was[0], screen[i][1] - was[1]]
        });
        Some([0, 1].map(|k| setup.plane([0, 1, 2].map(|i| (moved[i][k] as i64) << 16))))
    } else {
        None
    };

    let width = lens.width as usize;
    let (mut w_row, mut depth_row) = (setup.w, depth_plane.v);
    for y in setup.y0..setup.y1 {
        let rows = (y - setup.y0) as i64;
        let (mut w, mut depth_here) = (w_row, depth_row);
        let base = y as usize * width;
        let mut entered = false;

        for x in setup.x0..setup.x1 {
            if (w[0] | w[1] | w[2]) >= 0 {
                entered = true;
                let depth = (depth_here >> 16).clamp(DEPTH_MIN, u32::MAX as i64) as u32;
                let index = base + x as usize;
                if depth > out.shape[index] as u32 {
                    let columns = (x - setup.x0) as i64;
                    let at = |plane: &Plane| {
                        plane
                            .v
                            .wrapping_add(plane.dy.wrapping_mul(rows))
                            .wrapping_add(plane.dx.wrapping_mul(columns))
                    };

                    let mut n = normalize(normal_planes.map(|p| (at(&p) >> 16) as i32), 14);
                    if back {
                        n = n.map(|c| -c);
                    }

                    let rgb = match (surface.texture, &uv_planes) {
                        (Some(texture), Some(planes)) => {
                            let over = (depth_here >> 8).max(1);
                            let u = (at(&planes[0]) / over) as i32;
                            let v = (at(&planes[1]) / over) as i32;
                            let level = lod + 3 * (26 * 256 - log2_q8(depth as u64)) / 2;
                            tint(texture.sample(u, v, level), surface.tint)
                        }
                        _ => surface.tint,
                    };

                    let moved = match &motion_planes {
                        Some(planes) => [0, 1].map(|k| (at(&planes[k]) >> (16 + SUB - 1)) as i32),
                        None => [0; 2],
                    };

                    out.surface[index] = pack_surface(rgb, surface.props, moved);
                    out.shape[index] = pack_shape(depth, n);
                }
            } else if entered {
                // A triangle is convex: once a row has left it, it does not
                // come back.
                break;
            }
            for i in 0..3 {
                w[i] = w[i].wrapping_add(setup.wx[i]);
            }
            depth_here = depth_here.wrapping_add(depth_plane.dx);
        }

        for i in 0..3 {
            w_row[i] = w_row[i].wrapping_add(setup.wy[i]);
        }
        depth_row = depth_row.wrapping_add(depth_plane.dy);
    }
    true
}

/// Draw one model into the G-buffer. Returns how many triangles made it to
/// the screen.
pub(crate) fn draw(
    lens: &Lens,
    out: &mut Buffers<'_>,
    item: &Item<'_>,
    view: &Transform,
    previous_view: &Transform,
    moving: bool,
) -> u32 {
    let material = &item.material;
    let mut surface = Surface {
        tint: crate::texture::pack(material.color),
        texture: material.texture,
        props: pack_props(material.reflect, material.shine, material.glow),
        two_sided: material.two_sided,
        mirrored: true,
        moving,
    };

    let to_view = view.after(&item.transform);
    let was_view = previous_view.after(&item.previous);
    let normals = to_view.normals();
    surface.mirrored = to_view.mirrors();
    let tiling = material.tiling.0 as i64;

    let corner = |index: u16| {
        let vertex = item.mesh.vertices.get(index as usize)?;
        let position = vertex.position.raw();
        let n = vertex.normal.raw();
        let limit = 1i64 << 22;
        Some(Corner {
            p: to_view.point(position),
            n: normalize(normals.map(|row| (dot(row, n) >> 12) as i32), 14),
            uv: vertex.uv.map(|c| ((c.0 as i64 * tiling) >> 16).clamp(-limit, limit) as i32),
            q: was_view.point(position),
        })
    };

    let mut drawn = 0;
    let mut polygon = [Corner::default(); POLYGON];
    for triangle in item.mesh.indices.chunks_exact(3) {
        let (Some(a), Some(b), Some(c)) = (corner(triangle[0]), corner(triangle[1]), corner(triangle[2]))
        else {
            continue;
        };
        let corners = [a, b, c];
        let count = clip(lens, corners, &mut polygon);
        if count < 3 {
            continue;
        }
        let lod = match surface.texture {
            Some(texture) => lod_constant(lens, texture, &corners),
            None => 0,
        };
        let mut any = false;
        for i in 1..count - 1 {
            any |= fill(lens, out, &surface, lod, [polygon[0], polygon[i], polygon[i + 1]]);
        }
        drawn += any as u32;
    }
    drawn
}

// ---------------------------------------------------------------------------
// Shadow map
// ---------------------------------------------------------------------------

/// The scene's depth as the sun sees it: for each direction, how close to
/// the sun the nearest surface is. Anything further along the same line is
/// in that surface's shadow.
///
/// The sun is far enough away that its rays are parallel, so this view has
/// no perspective. Two texels to a word.
pub(crate) struct ShadowMap<'b> {
    pub texels: &'b mut [u64],
    /// Texels along a side.
    pub size: usize,
}

impl ShadowMap<'_> {
    pub(crate) const fn words(size: usize) -> usize {
        size * size / 2
    }

    pub(crate) fn clear(&mut self) {
        self.texels.fill(0);
    }

    /// A light-space `z` as something that orders the right way unsigned,
    /// with zero meaning nothing.
    #[inline]
    pub(crate) fn encode(z: i32) -> u32 {
        (z as u32 ^ 0x8000_0000).max(1)
    }

    #[inline]
    pub(crate) fn get(&self, x: usize, y: usize) -> u32 {
        let index = y * self.size + x;
        (self.texels[index / 2] >> ((index & 1) * 32)) as u32
    }

    #[inline]
    fn raise(&mut self, index: usize, value: u32) {
        let shift = (index & 1) * 32;
        let word = &mut self.texels[index / 2];
        if value > (*word >> shift) as u32 {
            *word = (*word & !(0xffff_ffffu64 << shift)) | (value as u64) << shift;
        }
    }

    fn fill(&mut self, mut screen: [[i32; 2]; 3], mut depth: [i32; 3]) {
        // A shadow is cast by either side of a surface.
        let [x, y] = [0, 1].map(|axis| [0, 1, 2].map(|i| screen[i][axis] as i64));
        if (x[1] - x[0]) * (y[2] - y[0]) - (y[1] - y[0]) * (x[2] - x[0]) < 0 {
            screen.swap(1, 2);
            depth.swap(1, 2);
        }
        let Some(setup) = Setup::new(screen, self.size as i32, self.size as i32) else {
            return;
        };
        let plane = setup.plane(depth.map(|z| (z as i64) << 8));

        let (mut w_row, mut depth_row) = (setup.w, plane.v);
        for y in setup.y0..setup.y1 {
            let (mut w, mut depth_here) = (w_row, depth_row);
            let base = y as usize * self.size;
            let mut entered = false;
            for x in setup.x0..setup.x1 {
                if (w[0] | w[1] | w[2]) >= 0 {
                    entered = true;
                    let z = (depth_here >> 8).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                    self.raise(base + x as usize, Self::encode(z));
                } else if entered {
                    break;
                }
                for i in 0..3 {
                    w[i] = w[i].wrapping_add(setup.wx[i]);
                }
                depth_here = depth_here.wrapping_add(plane.dx);
            }
            for i in 0..3 {
                w_row[i] = w_row[i].wrapping_add(setup.wy[i]);
            }
            depth_row = depth_row.wrapping_add(plane.dy);
        }
    }

    /// Draw one model as the sun sees it. `to_light` takes world space to
    /// the sun's view, and `scale` light-space units to 1/256 texel, Q16.16.
    pub(crate) fn draw(&mut self, item: &Item<'_>, to_light: &Transform, scale: i64) {
        let transform = to_light.after(&item.transform);
        let centre = self.size as i64 * (SUB_ONE / 2);
        let limit = 1i64 << 26;
        let corner = |index: u16| {
            let vertex = item.mesh.vertices.get(index as usize)?;
            let p = transform.point(vertex.position.raw());
            let on_map = |c: i32| (((c as i64 * scale) >> 16) + centre).clamp(-limit, limit) as i32;
            Some(([on_map(p[0]), on_map(p[1])], p[2]))
        };
        for triangle in item.mesh.indices.chunks_exact(3) {
            let (Some(a), Some(b), Some(c)) = (corner(triangle[0]), corner(triangle[1]), corner(triangle[2]))
            else {
                continue;
            };
            self.fill([a.0, b.0, c.0], [a.1, b.1, c.1]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cheap deterministic stream of numbers.
    struct Random(u32);

    impl Random {
        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0
        }

        fn below(&mut self, limit: u32) -> i32 {
            (self.next() % limit) as i32
        }
    }

    const SIDE: i32 = 48;

    /// Count how many times each pixel is covered by a triangle, either way
    /// round.
    fn cover(counts: &mut [u32], mut s: [[i32; 2]; 3]) {
        let [x, y] = [0, 1].map(|axis| [0, 1, 2].map(|i| s[i][axis] as i64));
        if (x[1] - x[0]) * (y[2] - y[0]) - (y[1] - y[0]) * (x[2] - x[0]) < 0 {
            s.swap(1, 2);
        }
        let Some(setup) = Setup::new(s, SIDE, SIDE) else {
            return;
        };
        for y in setup.y0..setup.y1 {
            for x in setup.x0..setup.x1 {
                let (dx, dy) = ((x - setup.x0) as i64, (y - setup.y0) as i64);
                let inside = (0..3).all(|i| setup.w[i] + setup.wx[i] * dx + setup.wy[i] * dy >= 0);
                counts[(y * SIDE + x) as usize] += inside as u32;
            }
        }
    }

    /// Whether a pixel centre is strictly inside a triangle, in floating
    /// point.
    fn strictly_inside(s: &[[i32; 2]; 3], x: i32, y: i32) -> Option<bool> {
        let p = [(x as f64 + 0.5) * 256.0, (y as f64 + 0.5) * 256.0];
        let edge = |a: [i32; 2], b: [i32; 2]| {
            (b[0] - a[0]) as f64 * (p[1] - a[1] as f64) - (b[1] - a[1]) as f64 * (p[0] - a[0] as f64)
        };
        let e = [edge(s[0], s[1]), edge(s[1], s[2]), edge(s[2], s[0])];
        if e.contains(&0.0) {
            // On an edge: the rule decides, and the other tests check it.
            return None;
        }
        Some(e.iter().all(|v| *v > 0.0) || e.iter().all(|v| *v < 0.0))
    }

    #[test]
    fn coverage_matches_a_point_in_triangle_test() {
        let mut random = Random(0x1234_5678);
        for _ in 0..400 {
            let s = [0; 3].map(|_| [random.below(60 * 256) - 6 * 256, random.below(60 * 256) - 6 * 256]);
            let mut counts = vec![0u32; (SIDE * SIDE) as usize];
            cover(&mut counts, s);
            for y in 0..SIDE {
                for x in 0..SIDE {
                    if let Some(inside) = strictly_inside(&s, x, y) {
                        let got = counts[(y * SIDE + x) as usize];
                        // Slivers are dropped whole, which only ever loses
                        // pixels.
                        assert!(got == inside as u32 || (got == 0 && inside), "{s:?} at {x},{y}");
                    }
                }
            }
        }
    }

    /// The property that matters for a mesh: a fan of triangles round a
    /// shared corner covers every pixel inside it exactly once, including
    /// the ones whose centres land exactly on a shared edge.
    #[test]
    fn triangles_sharing_edges_cover_each_pixel_once() {
        let mut random = Random(0x0bad_cafe);
        for round in 0..300 {
            // Whole and half pixel positions, so that centres on edges and
            // on corners are common rather than vanishingly rare.
            let step = if round % 2 == 0 { 128 } else { 37 };
            let mut point = |low: i32, high: i32| (low + random.below((high - low) as u32)) / step * step;
            let centre = [point(16 * 256, 32 * 256), point(16 * 256, 32 * 256)];
            let rim = [
                [point(0, 12 * 256), point(0, 12 * 256)],
                [point(36 * 256, 48 * 256), point(0, 12 * 256)],
                [point(36 * 256, 48 * 256), point(36 * 256, 48 * 256)],
                [point(0, 12 * 256), point(36 * 256, 48 * 256)],
            ];

            let mut fan = vec![0u32; (SIDE * SIDE) as usize];
            for i in 0..4 {
                cover(&mut fan, [centre, rim[i], rim[(i + 1) % 4]]);
            }
            // The same outline, split along a diagonal instead.
            let mut split = vec![0u32; (SIDE * SIDE) as usize];
            cover(&mut split, [rim[0], rim[1], rim[2]]);
            cover(&mut split, [rim[0], rim[2], rim[3]]);

            assert!(fan.iter().all(|c| *c <= 1), "a pixel was drawn twice");
            assert_eq!(fan, split, "two ways of cutting one shape disagree");
        }
    }

    #[test]
    fn values_interpolate_as_a_plane() {
        let mut random = Random(0x5eed_5eed);
        for _ in 0..200 {
            let mut s = [0; 3].map(|_| [random.below(48 * 256), random.below(48 * 256)]);
            let [x, y] = [0, 1].map(|axis| [0, 1, 2].map(|i| s[i][axis] as f64));
            let area = (x[1] - x[0]) * (y[2] - y[0]) - (y[1] - y[0]) * (x[2] - x[0]);
            if area.abs() < 256.0 * 256.0 * 4.0 {
                continue;
            }
            if area < 0.0 {
                s.swap(1, 2);
            }
            let Some(setup) = Setup::new(s, SIDE, SIDE) else {
                continue;
            };
            let a = [0; 3].map(|_| (random.next() as i64) << 12);
            let plane = setup.plane(a);

            // The plane through the three corners, solved in floating point.
            let [x, y] = [0, 1].map(|axis| [0, 1, 2].map(|i| s[i][axis] as f64 / 256.0));
            let det = (x[1] - x[0]) * (y[2] - y[0]) - (x[2] - x[0]) * (y[1] - y[0]);
            let (a1, a2) = ((a[1] - a[0]) as f64, (a[2] - a[0]) as f64);
            let gx = (a1 * (y[2] - y[0]) - a2 * (y[1] - y[0])) / det;
            let gy = (a2 * (x[1] - x[0]) - a1 * (x[2] - x[0])) / det;

            for (px, py) in [(setup.x0, setup.y0), (setup.x1 - 1, setup.y1 - 1), (0, 47)] {
                let exact = a[0] as f64 + gx * (px as f64 + 0.5 - x[0]) + gy * (py as f64 + 0.5 - y[0]);
                let got = plane.v + plane.dx * (px - setup.x0) as i64 + plane.dy * (py - setup.y0) as i64;
                // Values are 44 bits; ask for agreement to a part in 2^30.
                assert!((got as f64 - exact).abs() < 16384.0, "{s:?}: {got} vs {exact}");
            }
        }
    }

    #[test]
    fn the_g_buffer_round_trips() {
        let surface = pack_surface(0x00ab_cdef, pack_props(1, 2, 3), [-127, 90]);
        assert_eq!(surface as u32 & 0x00ff_ffff, 0x00ab_cdef);
        assert_eq!((surface >> 24) as u8, 1);
        assert_eq!((surface >> 32) as u8, 2);
        assert_eq!((surface >> 40) as u8, 3);
        assert_eq!(velocity(surface), [-127, 90]);
        assert_eq!(velocity(pack_surface(0, 0, [900, -900])), [127, -127]);

        for n in [[16384, 0, 0], [0, -16384, 0], [9459, 9459, -9459], [-3000, 100, 16000]] {
            let shape = pack_shape(0xdead_beef, n);
            assert_eq!(shape as u32, 0xdead_beef);
            let back = normal(shape);
            for i in 0..3 {
                assert!((back[i] - n[i]).abs() <= 64, "{n:?} came back {back:?}");
            }
        }
    }

    #[test]
    fn depth_and_distance_are_reciprocals() {
        for z in [0.1, 1.0, 7.5, 300.0, 20_000.0] {
            let q = (z * 65536.0) as i64;
            let back = z_of(depth_of(q)) as f64 / 65536.0;
            assert!((back - z).abs() < z * z / 5.0e7 + 1e-4, "{z} came back {back}");
        }
        // Nearer is larger, and nothing real is zero.
        assert!(depth_of(1 << 16) > depth_of(2 << 16));
        assert!(depth_of(i32::MAX as i64) > 0);
    }

    /// A view so wide that only the near plane ever cuts anything.
    fn lens() -> Lens {
        Lens {
            width: 1 << 15,
            height: 1 << 15,
            focal: 40 << 16,
            near: 1 << 13,
        }
    }

    #[test]
    fn a_triangle_in_view_is_left_alone_and_one_behind_is_dropped() {
        let at = |x: i32, y: i32, z: i32| Corner {
            p: [x << 14, y << 14, z << 16],
            ..Corner::default()
        };
        let mut polygon = [Corner::default(); POLYGON];
        let lens = lens();
        assert_eq!(clip(&lens, [at(-1, 0, 4), at(1, 0, 4), at(0, 1, 4)], &mut polygon), 3);
        assert_eq!(clip(&lens, [at(-1, 0, -4), at(1, 0, -4), at(0, 1, -4)], &mut polygon), 0);
        // One corner behind the camera: the near plane cuts it to a quad,
        // every corner of which projects.
        let count = clip(&lens, [at(-1, 0, 4), at(1, 0, 4), at(0, 0, -4)], &mut polygon);
        assert_eq!(count, 4);
        assert!(polygon[..count].iter().all(|c| c.p[2] >= lens.near - 1));
    }

    #[test]
    fn a_shared_edge_is_cut_at_one_point_whichever_way_it_runs() {
        let lens = lens();
        let a = Corner {
            p: [123_456, -70_000, 300_000],
            uv: [5, 99_999],
            ..Corner::default()
        };
        let b = Corner {
            p: [-900_001, 41_234, -77_777],
            uv: [70_000, -3],
            ..Corner::default()
        };
        let (da, db) = (lens.inside(0, a.p), lens.inside(0, b.p));
        assert!(da >= 0 && db < 0);
        // The clipper hands `cut` the inside end first, both times.
        let one = cut(&a, &b, da, db);
        let mut polygon = [Corner::default(); POLYGON];
        let third = Corner {
            p: [0, 0, 300_000],
            ..Corner::default()
        };
        clip(&lens, [b, a, third], &mut polygon);
        assert!(polygon.iter().any(|c| c.p == one.p && c.uv == one.uv));
        clip(&lens, [a, b, third], &mut polygon);
        assert!(polygon.iter().any(|c| c.p == one.p && c.uv == one.uv));
    }
}

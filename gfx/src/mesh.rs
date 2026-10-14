//! Models: surfaces made of triangles.
//!
//! A [`Mesh`] is a list of corners and a list of which three make each
//! triangle. The caller owns both, so a model can live in the kernel image,
//! in a buffer a client shared, or in memory filled by [`sphere`] or
//! [`parse_obj`] -- the engine only reads them.
//!
//! A triangle faces whoever sees its corners run anticlockwise, and by
//! default only that side is drawn. That is the convention model files use.

use crate::math::{sin_cos_q30, Fx, Vec3};

/// One corner of a model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vertex {
    pub position: Vec3,
    /// Which way the surface faces here. Shared between the triangles that
    /// meet at a corner, it is what makes a faceted ball look round.
    pub normal: Vec3,
    /// Where on a texture this corner is: (0, 0) top left, (1, 1) bottom
    /// right, and beyond that the texture repeats.
    pub uv: [Fx; 2],
}

impl Vertex {
    pub const ZERO: Self = Self {
        position: Vec3::ZERO,
        normal: Vec3::ZERO,
        uv: [Fx::ZERO; 2],
    };
}

/// A model.
#[derive(Clone, Copy, Debug)]
pub struct Mesh<'a> {
    pub vertices: &'a [Vertex],
    /// Three to a triangle, anticlockwise seen from the front.
    pub indices: &'a [u16],
}

impl<'a> Mesh<'a> {
    pub const fn new(vertices: &'a [Vertex], indices: &'a [u16]) -> Self {
        Self { vertices, indices }
    }

    pub const fn triangles(&self) -> usize {
        self.indices.len() / 3
    }
}

const HALF: i32 = 1 << 15;

/// The six faces of a cube: the way each faces, then the two directions
/// across it, chosen so that the first crossed with the second is the normal.
const FACES: [[[i32; 3]; 3]; 6] = [
    [[1, 0, 0], [0, 1, 0], [0, 0, 1]],
    [[0, 1, 0], [0, 0, 1], [1, 0, 0]],
    [[0, 0, 1], [1, 0, 0], [0, 1, 0]],
    [[-1, 0, 0], [0, 0, 1], [0, 1, 0]],
    [[0, -1, 0], [1, 0, 0], [0, 0, 1]],
    [[0, 0, -1], [0, 1, 0], [1, 0, 0]],
];

const CORNERS: [[i32; 2]; 4] = [[-1, -1], [1, -1], [1, 1], [-1, 1]];

/// One corner of a square facing along `FACES[which]`, half a unit across
/// each way and `lift` halves of a unit out from the origin.
const fn face(which: usize, corner: usize, lift: i32) -> Vertex {
    let [n, a, b] = FACES[which];
    let [s, t] = CORNERS[corner];
    Vertex {
        position: Vec3::fx(
            Fx((n[0] * lift + a[0] * s + b[0] * t) * HALF),
            Fx((n[1] * lift + a[1] * s + b[1] * t) * HALF),
            Fx((n[2] * lift + a[2] * s + b[2] * t) * HALF),
        ),
        normal: Vec3::new(n[0], n[1], n[2]),
        uv: [Fx((s + 1) * HALF), Fx((1 - t) * HALF)],
    }
}

const CUBE_VERTICES: [Vertex; 24] = {
    let mut vertices = [Vertex::ZERO; 24];
    let mut i = 0;
    while i < 24 {
        vertices[i] = face(i / 4, i % 4, 1);
        i += 1;
    }
    vertices
};

const CUBE_INDICES: [u16; 36] = {
    let mut indices = [0u16; 36];
    let mut f = 0;
    while f < 6 {
        let base = f as u16 * 4;
        let quad = [0, 1, 2, 0, 2, 3];
        let mut k = 0;
        while k < 6 {
            indices[f * 6 + k] = base + quad[k];
            k += 1;
        }
        f += 1;
    }
    indices
};

const PLANE_VERTICES: [Vertex; 4] = [face(1, 0, 0), face(1, 1, 0), face(1, 2, 0), face(1, 3, 0)];

/// A cube one unit along each edge, centred on the origin.
pub const CUBE: Mesh<'static> = Mesh::new(&CUBE_VERTICES, &CUBE_INDICES);

/// A square one unit a side, lying flat and facing up, centred on the
/// origin. Scale it up for a floor.
pub const PLANE: Mesh<'static> = Mesh::new(&PLANE_VERTICES, &[0, 1, 2, 0, 2, 3]);

/// How many vertices and indices [`sphere`] needs.
pub const fn sphere_len(segments: u32, rings: u32) -> (usize, usize) {
    (
        ((segments + 1) * (rings + 1)) as usize,
        (segments * rings * 6) as usize,
    )
}

/// A ball one unit across, centred on the origin, with `segments` slices
/// round and `rings` bands from top to bottom.
///
/// Fills the storage given and returns a mesh over it. `None` if either
/// slice is not exactly [`sphere_len`] long, or there are too few slices to
/// enclose anything, or too many corners for 16-bit indices.
pub fn sphere<'a>(
    vertices: &'a mut [Vertex],
    indices: &'a mut [u16],
    segments: u32,
    rings: u32,
) -> Option<Mesh<'a>> {
    let (vertex_count, index_count) = sphere_len(segments, rings);
    if segments < 3 || rings < 2 || vertex_count > 1 << 16 {
        return None;
    }
    if vertices.len() != vertex_count || indices.len() != index_count {
        return None;
    }

    let q16 = |q30: i64| ((q30 + (1 << 13)) >> 14) as i32;
    for ring in 0..=rings {
        // Half a turn from the top pole to the bottom one.
        let (down_sin, down_cos) = sin_cos_q30((((ring as u64) << 31) / rings as u64) as u32);
        for segment in 0..=segments {
            let (round_sin, round_cos) =
                sin_cos_q30((((segment as u64) << 32) / segments as u64) as u32);
            let normal = [
                q16((down_sin * round_cos) >> 30),
                q16(down_cos),
                q16((down_sin * round_sin) >> 30),
            ];
            vertices[(ring * (segments + 1) + segment) as usize] = Vertex {
                position: Vec3::from_raw(normal.map(|c| c / 2)),
                normal: Vec3::from_raw(normal),
                uv: [
                    Fx::ratio(segment as i32, segments as i32),
                    Fx::ratio(ring as i32, rings as i32),
                ],
            };
        }
    }

    let mut next = 0;
    for ring in 0..rings {
        for segment in 0..segments {
            let a = (ring * (segments + 1) + segment) as u16;
            let b = a + segments as u16 + 1;
            // Seen from outside, the next segment is to the left and the
            // next ring is below.
            for index in [a, a + 1, b + 1, a, b + 1, b] {
                indices[next] = index;
                next += 1;
            }
        }
    }
    Some(Mesh { vertices, indices })
}

// ---------------------------------------------------------------------------
// Wavefront OBJ
// ---------------------------------------------------------------------------

/// Somewhere for [`parse_obj`] to keep a file's positions, texture
/// coordinates and normals while it reads the faces that refer to them.
pub struct ObjScratch<'a> {
    pub positions: &'a mut [Vec3],
    pub uvs: &'a mut [[Fx; 2]],
    pub normals: &'a mut [Vec3],
}

/// Why a model file could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjError {
    /// A number that is not a number, or a face that names a corner the file
    /// has not defined. Carries the line, counting from one.
    Malformed(usize),
    /// The file holds more than the storage it was given.
    TooBig,
}

/// A decimal number as Q16.16, without floating point. Exponents are not
/// understood; model files written by people and by exporters do not use them
/// for coordinates.
fn parse_fx(text: &str) -> Option<Fx> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    let mut value: i64 = 0;
    for byte in whole.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = (value * 10 + (byte - b'0') as i64).min(1 << 20);
    }
    // Nine decimal places is more than Q16.16 can tell apart.
    let (mut numerator, mut denominator) = (0i64, 1i64);
    for byte in fraction.bytes().take(9) {
        if !byte.is_ascii_digit() {
            return None;
        }
        numerator = numerator * 10 + (byte - b'0') as i64;
        denominator *= 10;
    }
    let q = (value << 16) + ((numerator << 16) + denominator / 2) / denominator;
    let q = if negative { -q } else { q };
    Some(Fx(q.clamp(i32::MIN as i64, i32::MAX as i64) as i32))
}

/// A one-based index, or a negative one counting back from the newest.
fn resolve(text: &str, count: usize) -> Option<usize> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let mut value = 0usize;
    if digits.is_empty() {
        return None;
    }
    for byte in digits.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as usize)?;
    }
    let index = if negative {
        count.checked_sub(value)?
    } else {
        value.checked_sub(1)?
    };
    (index < count).then_some(index)
}

/// Read a Wavefront OBJ model -- the plain-text format every modelling
/// program can export.
///
/// Understands `v`, `vt`, `vn` and `f`, with faces of any number of corners
/// (they are fanned into triangles); everything else is skipped. A face
/// without normals gets the flat normal of its own plane. Every face corner
/// becomes its own vertex, so `vertices` needs three per triangle.
///
/// There is no filesystem yet, so `text` is whatever the caller has: usually
/// `include_str!`, which puts the file in the kernel image.
pub fn parse_obj<'a>(
    text: &str,
    vertices: &'a mut [Vertex],
    indices: &'a mut [u16],
    scratch: ObjScratch<'_>,
) -> Result<Mesh<'a>, ObjError> {
    let ObjScratch {
        positions: seen_positions,
        uvs: seen_uvs,
        normals: seen_normals,
    } = scratch;
    let (mut positions, mut uvs, mut normals) = (0, 0, 0);
    let (mut vertex_count, mut index_count) = (0usize, 0usize);

    for (row, line) in text.lines().enumerate() {
        let bad = ObjError::Malformed(row + 1);
        let line = line.split('#').next().unwrap_or("");
        let mut words = line.split_ascii_whitespace();
        let Some(keyword) = words.next() else {
            continue;
        };
        let mut number = || words.next().and_then(parse_fx).ok_or(bad);

        match keyword {
            "v" => {
                let slot = seen_positions.get_mut(positions).ok_or(ObjError::TooBig)?;
                *slot = Vec3::fx(number()?, number()?, number()?);
                positions += 1;
            }
            "vn" => {
                let slot = seen_normals.get_mut(normals).ok_or(ObjError::TooBig)?;
                *slot = Vec3::fx(number()?, number()?, number()?);
                normals += 1;
            }
            "vt" => {
                let slot = seen_uvs.get_mut(uvs).ok_or(ObjError::TooBig)?;
                // The format puts v = 0 at the bottom of the picture.
                let (u, v) = (number()?, number()?);
                *slot = [u, Fx(Fx::ONE.0.saturating_sub(v.0))];
                uvs += 1;
            }
            "f" => {
                let first = vertex_count;
                let mut corners = 0;
                for corner in words {
                    let mut parts = corner.split('/');
                    let position = parts
                        .next()
                        .and_then(|p| resolve(p, positions))
                        .ok_or(bad)?;
                    let uv = match parts.next() {
                        None | Some("") => None,
                        Some(t) => Some(resolve(t, uvs).ok_or(bad)?),
                    };
                    let normal = match parts.next() {
                        None | Some("") => None,
                        Some(n) => Some(resolve(n, normals).ok_or(bad)?),
                    };
                    if vertex_count > u16::MAX as usize {
                        return Err(ObjError::TooBig);
                    }
                    let slot = vertices.get_mut(vertex_count).ok_or(ObjError::TooBig)?;
                    *slot = Vertex {
                        position: seen_positions[position],
                        normal: normal.map_or(Vec3::ZERO, |n| seen_normals[n]),
                        uv: uv.map_or([Fx::ZERO; 2], |t| seen_uvs[t]),
                    };
                    vertex_count += 1;
                    corners += 1;
                }
                if corners < 3 {
                    return Err(bad);
                }

                let face = &mut vertices[first..first + corners];
                if face.iter().any(|v| v.normal == Vec3::ZERO) {
                    let flat = (face[1].position - face[0].position)
                        .cross(face[2].position - face[0].position)
                        .normalized();
                    for vertex in face.iter_mut() {
                        vertex.normal = flat;
                    }
                }
                for corner in 1..corners - 1 {
                    for offset in [0, corner, corner + 1] {
                        let slot = indices.get_mut(index_count).ok_or(ObjError::TooBig)?;
                        *slot = (first + offset) as u16;
                        index_count += 1;
                    }
                }
            }
            _ => {}
        }
    }

    Ok(Mesh {
        vertices: &vertices[..vertex_count],
        indices: &indices[..index_count],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real(v: Vec3) -> [f64; 3] {
        v.raw().map(|c| c as f64 / 65536.0)
    }

    /// Every triangle's corners run anticlockwise seen from outside: the
    /// normal they imply points the same way as the normals stored.
    fn faces_outwards(mesh: &Mesh) {
        for triangle in mesh.indices.chunks_exact(3) {
            let [a, b, c] = [0, 1, 2].map(|i| mesh.vertices[triangle[i] as usize]);
            let implied = (b.position - a.position).cross(c.position - a.position);
            if implied == Vec3::ZERO {
                // At a pole a band's quad narrows to a single triangle.
                continue;
            }
            let stored = a.normal + b.normal + c.normal;
            assert!(implied.dot(stored).0 > 0, "{triangle:?}");
        }
    }

    #[test]
    fn the_cube_is_closed_and_faces_outwards() {
        assert_eq!(CUBE.triangles(), 12);
        faces_outwards(&CUBE);
        for vertex in CUBE.vertices {
            assert!(real(vertex.position).iter().all(|c| c.abs() == 0.5));
        }
        faces_outwards(&PLANE);
        assert!(PLANE
            .vertices
            .iter()
            .all(|v| v.normal == Vec3::Y && v.position.y == Fx::ZERO));
    }

    #[test]
    fn the_sphere_is_round_and_faces_outwards() {
        let (vertex_count, index_count) = sphere_len(24, 12);
        let mut vertices = vec![Vertex::ZERO; vertex_count];
        let mut indices = vec![0u16; index_count];
        let mesh = sphere(&mut vertices, &mut indices, 24, 12).unwrap();
        for vertex in mesh.vertices {
            let [x, y, z] = real(vertex.position);
            assert!(((x * x + y * y + z * z).sqrt() - 0.5).abs() < 1e-4);
        }
        faces_outwards(&mesh);
        assert!(sphere(&mut vertices, &mut indices, 24, 11).is_none());
    }

    #[test]
    fn decimals_parse_without_floating_point() {
        for (text, want) in [
            ("1", 1.0),
            ("-0.5", -0.5),
            ("3.14159265", 3.14159265),
            (".25", 0.25),
            ("+7.", 7.0),
            ("-0.000015", -0.000015),
        ] {
            let got = parse_fx(text).unwrap().0 as f64 / 65536.0;
            assert!((got - want).abs() < 1.0 / 65536.0, "{text}: {got}");
        }
        for text in ["", "-", ".", "1e5", "abc", "1.2.3"] {
            assert!(parse_fx(text).is_none(), "{text}");
        }
    }

    const QUAD: &str = "
        # a square, with and without everything
        v -1 0 -1
        v -1 0 1
        v 1 0 1
        v 1 0 -1
        vt 0 0
        vt 1 1
        vn 0 1 0
        f 1/1/1 2/2/1 3/1/1 4/2/1
        f -4 -3 -2
        ";

    #[test]
    fn a_model_file_becomes_triangles() {
        let mut vertices = [Vertex::ZERO; 7];
        let mut indices = [0u16; 9];
        let (mut p, mut t, mut n) = ([Vec3::ZERO; 4], [[Fx::ZERO; 2]; 2], [Vec3::ZERO; 1]);
        let scratch = ObjScratch {
            positions: &mut p,
            uvs: &mut t,
            normals: &mut n,
        };
        let mesh = parse_obj(QUAD, &mut vertices, &mut indices, scratch).unwrap();

        // A quad fans into two triangles; the bare triangle follows.
        assert_eq!(mesh.indices, &[0, 1, 2, 0, 2, 3, 4, 5, 6]);
        assert_eq!(mesh.vertices[2].position, Vec3::new(1, 0, 1));
        // v = 0 is the bottom of the picture in the file, the top here.
        assert_eq!(mesh.vertices[0].uv, [Fx::ZERO, Fx::ONE]);
        // No normal given: the face's own, which for these corners is up.
        let flat = real(mesh.vertices[4].normal);
        assert!(flat[0] == 0.0 && (flat[1] - 1.0).abs() < 1e-4 && flat[2] == 0.0);
        faces_outwards(&mesh);
    }

    #[test]
    fn a_bad_model_file_says_where() {
        let mut vertices = [Vertex::ZERO; 3];
        let mut indices = [0u16; 3];
        let mut run = |text: &str| {
            let (mut p, mut t, mut n) = ([Vec3::ZERO; 3], [[Fx::ZERO; 2]; 1], [Vec3::ZERO; 1]);
            let scratch = ObjScratch {
                positions: &mut p,
                uvs: &mut t,
                normals: &mut n,
            };
            parse_obj(text, &mut vertices, &mut indices, scratch).map(|m| m.triangles())
        };
        assert_eq!(run("v 0 0 0\nv 1 0 x"), Err(ObjError::Malformed(2)));
        assert_eq!(run("v 0 0 0\nf 1 2 3"), Err(ObjError::Malformed(2)));
        assert_eq!(
            run("v 0 0 0\nv 0 0 0\nv 0 0 0\nv 0 0 0"),
            Err(ObjError::TooBig)
        );
        assert_eq!(run("v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3"), Ok(1));
    }
}

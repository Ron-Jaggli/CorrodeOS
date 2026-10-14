//! A scene that uses everything, for looking at.
//!
//! The kernel shows this when booted with the `engine-demo` feature, and
//! `./corrode engine-shot` draws it to files on the build host. It is one
//! scene so that the two can be compared, and so that there is somewhere to
//! see what each setting does: a tiled floor running to the horizon for
//! textures and their mip levels, a mirror ball for reflections, a red ball
//! beside a white pillar for bounced light, a lamp for bloom, something
//! spinning fast for motion blur, and things at several distances for the
//! lens to focus between.
//!
//! Nothing here keeps state. [`Assets::scene`] builds the whole scene from
//! the time it is given, and the camera a shutter-length earlier comes from
//! asking the same function about an earlier time.

use crate::math::{fx, Angle, Fx, Transform, Vec3};
use crate::mesh::{self, Mesh, Vertex};
use crate::scene::{Camera, Material, PointLight, Scene};
use crate::texture::{self, Texture};
use crate::Color;

const SEGMENTS: u32 = 40;
const RINGS: u32 = 20;
const TEXTURE_SIZE: u32 = 256;
const TEXTURE_LEN: usize = Texture::len_with_mips(TEXTURE_SIZE, TEXTURE_SIZE);

/// How long the shutter is open: a forty-eighth of a second, which is what
/// film shot at twenty-four frames a second uses.
const SHUTTER_NS: u64 = 1_000_000_000 / 48;

/// The models and textures the demo scene is made of.
pub struct Assets<'a> {
    ball: Mesh<'a>,
    tiles: Texture<'a>,
    bricks: Texture<'a>,
}

impl<'a> Assets<'a> {
    /// How long each of the three slices [`Assets::new`] fills must be.
    pub const VERTICES: usize = mesh::sphere_len(SEGMENTS, RINGS).0;
    pub const INDICES: usize = mesh::sphere_len(SEGMENTS, RINGS).1;
    pub const TEXELS: usize = 2 * TEXTURE_LEN;

    /// Build the demo's models and textures in the storage given. `None` if
    /// any slice is the wrong length.
    pub fn new(
        vertices: &'a mut [Vertex],
        indices: &'a mut [u16],
        texels: &'a mut [u32],
    ) -> Option<Self> {
        if texels.len() != Self::TEXELS {
            return None;
        }
        let (tiles, bricks) = texels.split_at_mut(TEXTURE_LEN);
        let pale = Color::rgb(212, 208, 198);
        let dark = Color::rgb(58, 62, 70);
        if !texture::checker(tiles, TEXTURE_SIZE, 2, pale, dark) {
            return None;
        }
        if !texture::bricks(
            bricks,
            TEXTURE_SIZE,
            Color::rgb(183, 65, 14),
            Color::rgb(170, 165, 150),
        ) {
            return None;
        }
        Some(Self {
            ball: mesh::sphere(vertices, indices, SEGMENTS, RINGS)?,
            tiles: Texture::new(tiles, TEXTURE_SIZE, TEXTURE_SIZE)?,
            bricks: Texture::new(bricks, TEXTURE_SIZE, TEXTURE_SIZE)?,
        })
    }

    /// Where the camera is at an instant: swinging slowly from side to side
    /// in front of the scene, rising and falling as it goes.
    pub fn camera(now_ns: u64) -> Camera {
        const DISTANCE: Fx = fx(9.5);
        const HEIGHT: Fx = fx(3.0);
        const TARGET: Vec3 = Vec3::f(0.0, 0.9, 0.0);
        let (sin, cos) = Angle::sway(now_ns, 40_000, Angle::degrees(32)).sin_cos();
        let bob = Angle::spin(now_ns, 17_000).sin();
        let eye = Vec3::fx(sin, Fx::ZERO, cos).scale(DISTANCE)
            + Vec3::fx(Fx::ZERO, Fx(HEIGHT.0 + bob.0 / 2), Fx::ZERO);
        Camera::look_at(eye, TARGET).fov(Angle::degrees(58))
    }

    /// The whole scene at an instant.
    pub fn scene(&self, now_ns: u64) -> Scene<'_, 12> {
        let before = now_ns.saturating_sub(SHUTTER_NS);
        let mut scene = Scene::new(Self::camera(now_ns));
        scene.previous_camera = Some(Self::camera(before));

        // A floor to the horizon.
        scene.draw(
            &mesh::PLANE,
            Transform::IDENTITY.scale(const { fx(240.0) }),
            Material::textured(&self.tiles)
                .tiling(const { fx(60.0) })
                .shine(90)
                .reflect(28),
        );

        // A brick block, turning slowly.
        let block = |at: u64| {
            Transform::at(const { Vec3::f(-3.0, 1.0, -0.6) })
                .rotate_y(Angle::spin(at, 24_000))
                .scale(const { fx(2.0) })
        };
        scene.draw_moving(
            &mesh::CUBE,
            block(now_ns),
            block(before),
            Material::textured(&self.bricks),
        );

        // A mirror ball.
        const MIRROR: Vec3 = Vec3::f(0.3, 1.2, -1.4);
        scene.draw(
            &self.ball,
            Transform::at(MIRROR).scale(const { fx(2.4) }),
            Material::color(Color::rgb(235, 235, 240))
                .reflect(235)
                .shine(230),
        );

        // A red ball next to a white pillar, close enough to colour it.
        scene.draw(
            &self.ball,
            Transform::at(const { Vec3::f(2.75, 0.65, 0.9) }).scale(const { fx(1.3) }),
            Material::color(Color::rgb(210, 28, 24)).shine(60),
        );
        scene.draw(
            &mesh::CUBE,
            Transform::at(const { Vec3::f(3.9, 1.9, 0.5) })
                .stretch(const { Vec3::f(0.7, 3.8, 2.4) }),
            Material::color(Color::rgb(232, 230, 224)),
        );

        // A lamp, bobbing: a ball that glows and a light where it is.
        let lamp = {
            let bob = Angle::spin(now_ns, 5_000).sin();
            const REST: Vec3 = Vec3::f(-1.0, 0.75, 2.4);
            REST + Vec3::fx(Fx::ZERO, Fx(bob.0 / 5), Fx::ZERO)
        };
        let warm = Color::rgb(255, 150, 50);
        scene.draw(
            &self.ball,
            Transform::at(lamp).scale(const { fx(0.5) }),
            Material::color(warm).glow(200),
        );
        scene.light(PointLight {
            position: lamp,
            color: warm,
            intensity: const { fx(5.0) },
            radius: const { fx(6.0) },
        });

        // Something small going round fast, for the shutter to smear.
        let flyer = |at: u64| {
            let (sin, cos) = Angle::spin(at, 1_600).sin_cos();
            let orbit = Vec3::fx(sin, Fx::ZERO, cos).scale(const { fx(1.9) });
            Transform::at(MIRROR + Vec3::new(0, 2, 0) + orbit)
                .rotate_y(Angle::spin(at, 700))
                .rotate_x(Angle::spin(at, 1_100))
                .scale(const { fx(0.55) })
        };
        scene.draw_moving(
            &mesh::CUBE,
            flyer(now_ns),
            flyer(before),
            Material::color(Color::rgb(40, 150, 235)).shine(140),
        );

        // Far things, for the lens to leave out of focus.
        for (index, x) in [-9, -3, 4, 10].into_iter().enumerate() {
            let tall = [5, 8, 6, 9][index];
            scene.draw(
                &mesh::CUBE,
                Transform::at(Vec3::new(x, tall / 2, -14 - index as i32 * 3))
                    .rotate_y(Angle::degrees(20 * index as i32))
                    .stretch(Vec3::new(2, tall, 2)),
                Material::color(Color::rgb(150, 160, 175)).shine(30),
            );
        }

        scene
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, Settings};

    fn assets<'a>(
        vertices: &'a mut Vec<Vertex>,
        indices: &'a mut Vec<u16>,
        texels: &'a mut Vec<u32>,
    ) -> Assets<'a> {
        vertices.resize(Assets::VERTICES, Vertex::ZERO);
        indices.resize(Assets::INDICES, 0);
        texels.resize(Assets::TEXELS, 0);
        Assets::new(vertices, indices, texels).unwrap()
    }

    #[test]
    fn the_demo_fits_its_scene_and_draws() {
        let (mut vertices, mut indices, mut texels) = (Vec::new(), Vec::new(), Vec::new());
        let assets = assets(&mut vertices, &mut indices, &mut texels);
        let (width, height) = (160, 100);
        let mut memory = vec![0u64; Engine::words(width, height)];
        let mut out = vec![0u32; width * height];
        let mut engine = Engine::new(width, height, &mut memory).unwrap();
        for now in [0, 3_000_000_000, 17_500_000_000] {
            let scene = assets.scene(now);
            // Everything the scene asks to draw found room.
            assert_eq!(scene.items().count(), 11);
            let stats = engine.render(&scene, &Settings::CINEMATIC, now, &mut out);
            assert!(stats.triangles > 500, "{stats:?}");
            // Not one flat colour.
            assert!(out.iter().any(|p| *p != out[0]));
        }
    }

    /// Draw the demo to files, for looking at. Not a test of anything: run
    /// through `./corrode engine-shot`, which says where to put them.
    #[test]
    #[ignore]
    fn shot() {
        let Ok(directory) = std::env::var("ENGINE_SHOT") else {
            return;
        };
        let (width, height) = (640, 400);
        let (mut vertices, mut indices, mut texels) = (Vec::new(), Vec::new(), Vec::new());
        let assets = assets(&mut vertices, &mut indices, &mut texels);
        let mut memory = vec![0u64; Engine::words(width, height)];
        let mut out = vec![0u32; width * height];
        let mut engine = Engine::new(width, height, &mut memory).unwrap();

        let presets = [
            ("off", Settings::OFF),
            ("low", Settings::LOW),
            ("high", Settings::HIGH),
            ("cinematic", Settings::CINEMATIC),
        ];
        let now = 4_300_000_000;
        for (name, settings) in presets {
            let scene = assets.scene(now);
            let started = std::time::Instant::now();
            let stats = engine.render(&scene, &settings, now, &mut out);
            let took = started.elapsed();
            println!(
                "{name:>10}: {:>6.1} ms, {} triangles",
                took.as_secs_f64() * 1000.0,
                stats.triangles
            );

            let mut file = format!("P6\n{width} {height}\n255\n").into_bytes();
            for pixel in &out {
                file.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8]);
            }
            std::fs::write(format!("{directory}/engine-{name}.ppm"), file).unwrap();
        }
    }
}

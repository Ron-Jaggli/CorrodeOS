//! Corrode's 3D engine.
//!
//! The `render` crate puts rectangles and text on a screen. This one draws
//! worlds: models with textures, lit by a sun and a sky, seen through a
//! camera with a lens. It produces a block of pixels and knows nothing about
//! where they go -- a framebuffer, a window's surface, a file -- in the same
//! way the text engine produces coverage and knows nothing about colour.
//!
//! # Using it
//!
//! ```
//! use corrode_gfx::{mesh, Angle, Camera, Color, Engine, Material, Scene, Settings, Transform, Vec3};
//!
//! // Once: somewhere to work, and somewhere to put the picture.
//! let (width, height) = (160, 100);
//! let mut memory = vec![0u64; Engine::words(width, height)];
//! let mut picture = vec![0u32; width * height];
//! let mut engine = Engine::new(width, height, &mut memory).unwrap();
//!
//! // Every frame: say what the world looks like at this instant...
//! let now_ns = 1_500_000_000;
//! let camera = Camera::look_at(Vec3::new(0, 2, 5), Vec3::ZERO).fov(Angle::degrees(70));
//! let mut scene: Scene<8> = Scene::new(camera);
//! scene.draw(
//!     &mesh::CUBE,
//!     Transform::at(Vec3::new(0, 1, 0)).rotate_y(Angle::spin(now_ns, 4000)),
//!     Material::color(Color::rgb(200, 80, 40)).shine(120),
//! );
//! scene.draw(
//!     &mesh::PLANE,
//!     Transform::IDENTITY.scale(corrode_gfx::fx(20.0)),
//!     Material::color(Color::rgb(120, 120, 120)),
//! );
//!
//! // ...and draw it. `picture` is now `0x00RRGGBB`, row by row.
//! engine.render(&scene, &Settings::HIGH, now_ns, &mut picture);
//! ```
//!
//! Effects are fields of [`Settings`]: `None` is off.
//!
//! # How a frame is made
//!
//! 1. **Geometry.** Every triangle is drawn, but only as far as recording
//!    what is in front at each pixel: colour, direction it faces, depth,
//!    motion. No lighting yet. (`raster`)
//! 2. **Shadow map.** The same triangles as the sun sees them, depth only.
//! 3. **Light.** Each pixel is lit from what step 1 recorded: sun, sky,
//!    lamps, then light bounced off its neighbours. (`light`)
//! 4. **Lens.** Reflections, motion blur, depth of field, bloom. (`post`)
//! 5. **Film.** The tone curve fits the light to the screen; then fringing,
//!    vignette, edge smoothing, grain. (`post`)
//!
//! Steps 3 to 5 cost the same whatever was drawn, which is why the engine
//! takes a resolution at the start and can be run at less than the screen's.
//!
//! # The same rules as the rest of Corrode
//!
//! * **No floating point.** The kernel does not save vector state, so this
//!   is integer arithmetic throughout, checked against floating point by
//!   tests on the build host.
//! * **No allocation.** The engine works in memory it is handed.
//! * **Nothing is stepped.** A [`Scene`] describes one instant and is built
//!   from the clock every frame. Motion blur does not remember the last
//!   frame: it asks the same formulas where things were a moment earlier.
//! * **No GPU required.** This is the reference implementation, on a CPU.
//!   A GPU backend, when there are drivers, replaces the inside of
//!   [`Engine::render`] and nothing that calls it.

#![cfg_attr(not(test), no_std)]

mod color;
pub mod demo;
mod light;
pub mod math;
pub mod mesh;
mod post;
mod raster;
pub mod scene;
pub mod texture;

pub use color::Tonemap;
pub use corrode_render::Color;
pub use math::{fx, Angle, Fx, Transform, Vec3};
pub use mesh::{Mesh, Vertex};
pub use scene::{
    AmbientOcclusion, AntiAliasing, Bloom, Camera, DepthOfField, GlobalIllumination, Material, MotionBlur,
    PointLight, Reflections, Scene, Settings, Shadows, Sky, Sun,
};
pub use texture::Texture;

use light::{Eye, Heaven, Lamp, Shade};
use raster::{Buffers, Lens, ShadowMap};

/// Texels along a side of the shadow map.
const SHADOW_SIZE: usize = 1024;

/// What a frame took.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Triangles that reached the screen, after everything out of view or
    /// facing away was dropped.
    pub triangles: u32,
}

/// The engine: a picture size and the memory to draw pictures that size in.
pub struct Engine<'a> {
    width: usize,
    height: usize,
    /// What each pixel is made of, and where it is. See `raster`.
    surface: &'a mut [u64],
    shape: &'a mut [u64],
    /// The picture, as light; and a second one, because every pass after
    /// lighting reads one picture and writes another.
    light: &'a mut [u64],
    spare: &'a mut [u64],
    /// Two quarter-size pictures for bloom.
    glow: &'a mut [u64],
    glow_spare: &'a mut [u64],
    shadow: &'a mut [u64],
}

impl<'a> Engine<'a> {
    /// How many words [`Engine::new`] needs for a picture this size: about
    /// 33 bytes a pixel, plus four megabytes of shadow map.
    pub const fn words(width: usize, height: usize) -> usize {
        let small = (width / post::BLOOM_SCALE) * (height / post::BLOOM_SCALE);
        4 * width * height + 2 * small + ShadowMap::words(SHADOW_SIZE)
    }

    /// An engine that draws `width` x `height` pictures, working in
    /// `memory`.
    ///
    /// `None` if the picture is smaller than 8 x 8 or larger than 4096 either
    /// way, or `memory` is shorter than [`Engine::words`].
    pub fn new(width: usize, height: usize, memory: &'a mut [u64]) -> Option<Self> {
        if !(8..=4096).contains(&width) || !(8..=4096).contains(&height) {
            return None;
        }
        if memory.len() < Self::words(width, height) {
            return None;
        }
        let pixels = width * height;
        let small = (width / post::BLOOM_SCALE) * (height / post::BLOOM_SCALE);
        let (surface, rest) = memory.split_at_mut(pixels);
        let (shape, rest) = rest.split_at_mut(pixels);
        let (light, rest) = rest.split_at_mut(pixels);
        let (spare, rest) = rest.split_at_mut(pixels);
        let (glow, rest) = rest.split_at_mut(small);
        let (glow_spare, rest) = rest.split_at_mut(small);
        let (shadow, _) = rest.split_at_mut(ShadowMap::words(SHADOW_SIZE));
        Some(Self {
            width,
            height,
            surface,
            shape,
            light,
            spare,
            glow,
            glow_spare,
            shadow,
        })
    }

    pub const fn width(&self) -> usize {
        self.width
    }

    pub const fn height(&self) -> usize {
        self.height
    }

    fn swap(&mut self) {
        core::mem::swap(&mut self.light, &mut self.spare);
    }

    /// Draw `scene` into `out`: `width * height` pixels of `0x00RRGGBB`, row
    /// by row from the top.
    ///
    /// `now_ns` is only used to make film grain different from one frame to
    /// the next. Everything that moves does so because the scene handed in
    /// is different.
    ///
    /// Draws nothing if `out` is too short.
    pub fn render<const N: usize>(
        &mut self,
        scene: &Scene<'_, N>,
        settings: &Settings,
        now_ns: u64,
        out: &mut [u32],
    ) -> Stats {
        let (width, height) = (self.width, self.height);
        let Some(out) = out.get_mut(..width * height) else {
            return Stats::default();
        };

        let camera = &scene.camera;
        let view = camera.view();
        let previous_view = scene.previous_camera.map_or(view, |camera| camera.view());
        let focal = camera.focal(height);
        let near = camera.near_plane();
        let lens = Lens {
            width: width as i32,
            height: height as i32,
            focal,
            near,
        };
        let eye = Eye::new(width, height, focal, near, &view);

        // Geometry.
        self.surface.fill(0);
        self.shape.fill(0);
        let mut stats = Stats::default();
        {
            let mut buffers = Buffers {
                surface: &mut *self.surface,
                shape: &mut *self.shape,
            };
            let moving = settings.motion_blur.is_some();
            for item in scene.items() {
                stats.triangles += raster::draw(&lens, &mut buffers, item, &view, &previous_view, moving);
            }
        }

        // The sun's view of the same geometry.
        let mut toward_sun = scene.sun.direction.normalized();
        if toward_sun == Vec3::ZERO {
            toward_sun = Vec3::Y;
        }
        let mut map = ShadowMap {
            texels: &mut *self.shadow,
            size: SHADOW_SIZE,
        };
        let mut cast = None;
        if let Some(shadows) = &settings.shadows {
            let extent = shadows.extent.0.max(1 << 12) as i64;
            let mut right = Vec3::Y.cross(toward_sun).normalized();
            if right == Vec3::ZERO {
                right = Vec3::X.cross(toward_sun).normalized();
            }
            let up = toward_sun.cross(right);
            let m = [right.raw(), up.raw(), toward_sun.raw()];
            let turned = Transform { m, t: [0; 3] };
            let to_light = Transform {
                m,
                t: turned.direction(camera.target.raw()).map(|c| -c),
            };
            let scale = ((SHADOW_SIZE as i64 * 128) << 16) / extent;

            map.clear();
            for item in scene.items() {
                map.draw(item, &to_light, scale);
            }
            cast = Some((to_light.after(&view.inverse_rigid()), scale, extent));
        }
        let shade = cast.map(|(to_light, scale, extent)| {
            // One texel of the map, in world units.
            let texel = (2 * extent / SHADOW_SIZE as i64) as i32;
            Shade {
                map: &map,
                to_light,
                scale,
                bias: texel + texel / 2,
                offset: texel * 2,
            }
        });

        // Light.
        match &settings.ambient_occlusion {
            Some(occlusion) => light::occlusion(&eye, self.shape, self.spare, self.light, occlusion),
            None => self.light.fill(255 << 48),
        }

        let sky_light = |tint| color::light(tint, scene.sky.intensity);
        let heaven = Heaven {
            zenith: sky_light(scene.sky.zenith),
            horizon: sky_light(scene.sky.horizon),
            ground: sky_light(scene.sky.ground),
            sun_direction: math::normalize(view.direction(toward_sun.raw()), 14),
            sun: color::light(scene.sun.color, scene.sun.intensity),
            up: eye.up,
        };
        let mut lamps = [Lamp::new([0; 3], [0; 3], 1); scene::MAX_LIGHTS];
        let mut lamp_count = 0;
        for lamp in scene.lights.iter().flatten() {
            lamps[lamp_count] = Lamp::new(
                view.point(lamp.position.raw()),
                color::light(lamp.color, lamp.intensity),
                lamp.radius.0.max(1 << 8),
            );
            lamp_count += 1;
        }
        light::shade(
            &eye,
            &heaven,
            &lamps[..lamp_count],
            shade.as_ref(),
            self.surface,
            self.shape,
            self.light,
        );

        if let Some(bounced) = &settings.global_illumination {
            light::bounce(&eye, self.shape, self.light, self.spare, bounced);
            light::bounce_apply(&eye, self.surface, self.shape, self.spare, self.light);
        }

        // Lens.
        if let Some(reflections) = &settings.reflections {
            post::reflections(&eye, &heaven, self.surface, self.shape, self.light, self.spare, reflections);
            self.swap();
        }
        if let Some(blur) = &settings.motion_blur {
            post::motion_blur(width, height, self.surface, self.light, self.spare, blur);
            self.swap();
        }
        if let Some(lens) = &settings.depth_of_field {
            post::depth_of_field(&eye, self.shape, self.light, self.spare, lens);
            self.swap();
        }

        let small = (width / post::BLOOM_SCALE, height / post::BLOOM_SCALE);
        let mut glow = None;
        if let Some(bloom) = &settings.bloom {
            let threshold = (bloom.threshold.0.max(0) as i64 * color::ONE as i64) >> 16;
            post::bloom_extract(width, height, self.light, self.glow, threshold);
            post::bloom_blur(small.0, small.1, self.glow, self.glow_spare);
            glow = Some(post::Glow {
                small: &*self.glow,
                width: small.0,
                height: small.1,
                strength: bloom.strength.0.max(0) as i64,
            });
        }

        // Film.
        let grade = post::Grade {
            tonemap: settings.tonemap,
            exposure: settings.exposure.0.max(0) as i64,
            glow,
            aberration: settings.chromatic_aberration.map_or(0, |amount| amount.0.max(0) as i64),
            vignette: settings.vignette.map_or(0, |amount| amount.0.clamp(0, 1 << 16) as i64),
        };
        post::resolve(width, height, self.light, self.spare, &grade);

        let grain = settings.film_grain.map(|strength| post::Grain {
            strength: strength.0.clamp(0, 1 << 16) as i64,
            // Any number that differs between frames will do.
            seed: (now_ns / 1_000_000) as u32,
        });
        let smooth = settings.anti_aliasing == AntiAliasing::Fxaa;
        post::finish(width, height, self.spare, out, smooth, grain);

        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: usize = 96;
    const HEIGHT: usize = 64;

    /// A cube on a floor, drawn with `settings`.
    fn picture(settings: &Settings, edit: impl FnOnce(&mut Scene<'_, 4>)) -> (Vec<u32>, Stats) {
        let mut memory = vec![0u64; Engine::words(WIDTH, HEIGHT)];
        let mut out = vec![0u32; WIDTH * HEIGHT];
        let mut engine = Engine::new(WIDTH, HEIGHT, &mut memory).unwrap();
        let camera = Camera::look_at(Vec3::f(2.0, 2.5, 5.0), Vec3::f(0.0, 0.5, 0.0));
        let mut scene: Scene<4> = Scene::new(camera);
        scene.draw(
            &mesh::PLANE,
            Transform::IDENTITY.scale(fx(30.0)),
            Material::color(Color::rgb(160, 160, 160)),
        );
        scene.draw(
            &mesh::CUBE,
            Transform::at(Vec3::f(0.0, 0.5, 0.0)),
            Material::color(Color::rgb(200, 40, 40)),
        );
        edit(&mut scene);
        let stats = engine.render(&scene, settings, 0, &mut out);
        (out, stats)
    }

    fn channels(pixel: u32) -> [i32; 3] {
        [(pixel >> 16) as i32 & 0xff, (pixel >> 8) as i32 & 0xff, pixel as i32 & 0xff]
    }

    fn at(picture: &[u32], x: usize, y: usize) -> [i32; 3] {
        channels(picture[y * WIDTH + x])
    }

    #[test]
    fn memory_that_is_too_small_is_refused() {
        let mut memory = vec![0u64; Engine::words(64, 64) - 1];
        assert!(Engine::new(64, 64, &mut memory).is_none());
        let mut memory = vec![0u64; Engine::words(64, 64)];
        assert!(Engine::new(64, 64, &mut memory).is_some());
        assert!(Engine::new(4, 64, &mut memory).is_none());
    }

    #[test]
    fn a_cube_on_a_floor_under_a_sky() {
        let (out, stats) = picture(&Settings::OFF, |_| {});
        // Two triangles of floor, and the three faces of the cube that face
        // the camera.
        assert_eq!(stats.triangles, 2 + 6);
        // Sky at the top: bluer than it is red.
        let sky = at(&out, WIDTH / 2, 1);
        assert!(sky[2] > sky[0], "sky is {sky:?}");
        // The cube in the middle: red.
        let cube = at(&out, WIDTH / 2, HEIGHT / 2);
        assert!(cube[0] > 2 * cube[1] && cube[0] > 60, "cube is {cube:?}");
        // Floor at the bottom: grey.
        let floor = at(&out, 4, HEIGHT - 2);
        assert!((floor[0] - floor[2]).abs() < 40 && floor[0] > 60, "floor is {floor:?}");
    }

    #[test]
    fn the_same_scene_draws_the_same_picture() {
        let (first, _) = picture(&Settings::HIGH, |_| {});
        let (second, _) = picture(&Settings::HIGH, |_| {});
        assert_eq!(first, second);
    }

    #[test]
    fn the_cube_casts_a_shadow_on_the_floor() {
        // Light from the side, so the shadow falls where the camera can see
        // it rather than behind the cube.
        let sideways = |scene: &mut Scene<'_, 4>| scene.sun.direction = Vec3::f(-0.7, 0.6, 0.1);
        let lit = picture(&Settings::OFF, sideways).0;
        let shadowed = picture(
            &Settings {
                shadows: Some(Shadows { extent: fx(8.0) }),
                ..Settings::OFF
            },
            sideways,
        )
        .0;
        // Somewhere on the floor got darker, and nothing got brighter.
        let mut darker = 0;
        for (a, b) in lit.iter().zip(&shadowed) {
            let (a, b) = (channels(*a), channels(*b));
            assert!(b[1] <= a[1] + 1, "{a:?} became {b:?}");
            darker += (b[1] + 20 < a[1]) as usize;
        }
        assert!(darker > 50, "only {darker} pixels are in shadow");
    }

    #[test]
    fn a_wider_field_of_view_makes_things_smaller() {
        let red = |out: &[u32]| {
            out.iter()
                .filter(|p| {
                    let c = channels(**p);
                    c[0] > 2 * c[1] && c[0] > 60
                })
                .count()
        };
        let normal = red(&picture(&Settings::OFF, |_| {}).0);
        let wide = red(&picture(&Settings::OFF, |scene| scene.camera.fov = Angle::degrees(100)).0);
        let narrow = red(&picture(&Settings::OFF, |scene| scene.camera.fov = Angle::degrees(30)).0);
        assert!(wide * 2 < normal && normal * 2 < narrow, "{wide} {normal} {narrow}");
    }

    #[test]
    fn every_effect_changes_the_picture_and_none_breaks_it() {
        let plain = picture(&Settings::OFF, |_| {}).0;
        let with = |settings: Settings| picture(&settings, |_| {}).0;
        let off = Settings::OFF;
        let effects: [(&str, Settings); 9] = [
            ("shadows", Settings { shadows: Settings::LOW.shadows, ..off }),
            ("occlusion", Settings { ambient_occlusion: Settings::HIGH.ambient_occlusion, ..off }),
            ("bounce", Settings { global_illumination: Settings::HIGH.global_illumination, ..off }),
            ("depth of field", Settings { depth_of_field: Settings::CINEMATIC.depth_of_field, ..off }),
            ("bloom", Settings { bloom: Some(Bloom { threshold: fx(0.3), strength: fx(1.0) }), ..off }),
            ("smoothing", Settings { anti_aliasing: AntiAliasing::Fxaa, ..off }),
            ("fringing", Settings { chromatic_aberration: Some(fx(0.02)), ..off }),
            ("vignette", Settings { vignette: Some(fx(0.5)), ..off }),
            ("grain", Settings { film_grain: Some(fx(0.05)), ..off }),
        ];
        for (name, settings) in effects {
            assert_ne!(with(settings), plain, "{name} did nothing");
        }
        // Everything at once still draws a red cube under a blue sky.
        let all = with(Settings::CINEMATIC);
        let cube = at(&all, WIDTH / 2, HEIGHT / 2);
        assert!(cube[0] > 2 * cube[1], "cube is {cube:?}");
    }

    #[test]
    fn a_mirror_shows_what_is_in_front_of_it() {
        // Make the floor a mirror: below the cube, the floor turns red.
        let mirror = |scene: &mut Scene<'_, 4>| {
            scene.items[0].as_mut().unwrap().material.reflect = 255;
        };
        let settings = Settings {
            reflections: Settings::HIGH.reflections,
            ..Settings::OFF
        };
        let plain = picture(&Settings::OFF, mirror).0;
        let reflected = picture(&settings, mirror).0;
        let redder = plain
            .iter()
            .zip(&reflected)
            .filter(|(a, b)| {
                let (a, b) = (channels(**a), channels(**b));
                // Was grey floor, is now distinctly red.
                (a[0] - a[1]).abs() < 30 && b[0] > b[1] + 40
            })
            .count();
        assert!(redder > 30, "only {redder} pixels of floor reflect the cube");
    }

    #[test]
    fn something_that_moved_is_smeared() {
        let settings = Settings {
            motion_blur: Some(MotionBlur { strength: Fx::ONE }),
            ..Settings::OFF
        };
        let still = picture(&settings, |_| {}).0;
        let moving = picture(&settings, |scene| {
            let cube = scene.items[1].as_mut().unwrap();
            cube.previous = Transform::at(Vec3::f(-0.6, 0.5, 0.0));
        })
        .0;
        // Nothing moved: motion blur leaves the picture alone.
        assert_eq!(still, picture(&Settings::OFF, |_| {}).0);
        assert_ne!(still, moving);
    }

    #[test]
    fn a_lamp_lights_what_is_near_it() {
        let dark = |scene: &mut Scene<'_, 4>| {
            scene.sun.intensity = Fx::ZERO;
            scene.sky.intensity = fx(0.05);
        };
        let unlit = picture(&Settings::OFF, dark).0;
        let lit = picture(&Settings::OFF, |scene| {
            dark(scene);
            scene.light(PointLight {
                position: Vec3::f(1.5, 0.5, 1.5),
                color: Color::WHITE,
                intensity: fx(4.0),
                radius: fx(4.0),
            });
        })
        .0;
        let brighter = unlit.iter().zip(&lit).filter(|(a, b)| channels(**b)[1] > channels(**a)[1] + 30).count();
        assert!(brighter > 200, "only {brighter} pixels are lit by the lamp");
    }
}

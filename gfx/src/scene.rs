//! What there is to draw: a camera, lights, and things.
//!
//! A [`Scene`] is a description of one instant. It holds no history and
//! nothing in it moves by being stepped -- a scene that changes is a scene
//! built again from the clock, the same way `render::scene` asks every
//! formula where it is now. Building one is a few dozen words copied; it is
//! meant to be done every frame.

use corrode_render::Color;

use crate::math::{fx, Angle, Fx, Transform, Vec3};
use crate::mesh::Mesh;
use crate::texture::Texture;

/// Where the viewer is and what they see.
#[derive(Clone, Copy, Debug)]
pub struct Camera {
    pub eye: Vec3,
    pub target: Vec3,
    pub up: Vec3,
    /// Field of view, top of the picture to the bottom. Small is a telephoto
    /// lens, large a wide one; 60 to 75 degrees is where games sit.
    pub fov: Angle,
    /// Nothing closer than this is drawn. Never less than 1/32 of a unit.
    pub near: Fx,
}

impl Camera {
    /// A camera at `eye`, looking at `target`, with a 60 degree field of view.
    pub const fn look_at(eye: Vec3, target: Vec3) -> Self {
        Self {
            eye,
            target,
            up: Vec3::Y,
            fov: Angle::degrees(60),
            near: fx(0.1),
        }
    }

    pub const fn fov(mut self, fov: Angle) -> Self {
        self.fov = fov;
        self
    }

    pub const fn near(mut self, near: Fx) -> Self {
        self.near = near;
        self
    }

    /// The world as the camera sees it: `x` to its right, `y` up, and `z`
    /// straight ahead.
    pub(crate) fn view(&self) -> Transform {
        let mut forward = (self.target - self.eye).normalized();
        if forward == Vec3::ZERO {
            forward = -Vec3::Z;
        }
        let mut right = forward.cross(self.up).normalized();
        if right == Vec3::ZERO {
            // Looking straight along `up`; any sideways will do.
            right = forward.cross(Vec3::X).normalized();
        }
        let up = right.cross(forward);
        let m = [right.raw(), up.raw(), forward.raw()];
        let turned = Transform { m, t: [0; 3] };
        Transform {
            m,
            t: turned.direction(self.eye.raw()).map(|c| -c),
        }
    }

    /// The focal length in pixels, Q16.16, for a picture `height` tall: how
    /// far from the eye a screen that size would have to be.
    pub(crate) fn focal(&self, height: usize) -> i64 {
        // Between 1 and 170 degrees, so the tangent below is finite.
        let fov = self.fov.0.clamp(Angle::degrees(1).0, Angle::degrees(170).0);
        let (sin, cos) = crate::math::sin_cos_q30(fov / 2);
        ((height as i64 * cos) << 15) / sin
    }

    pub(crate) fn near_plane(&self) -> i32 {
        self.near.0.max(1 << 11)
    }
}

/// The one light that is far enough away to shine the same way everywhere,
/// and the one that casts shadows.
#[derive(Clone, Copy, Debug)]
pub struct Sun {
    /// From the scene towards the sun.
    pub direction: Vec3,
    pub color: Color,
    /// How bright, where 1 lights a white surface facing it to white. More
    /// than that is what makes highlights that bloom.
    pub intensity: Fx,
}

/// The sky: what is seen where nothing is drawn, what reflects in things, and
/// the light that fills in shadows.
#[derive(Clone, Copy, Debug)]
pub struct Sky {
    pub zenith: Color,
    pub horizon: Color,
    /// Below the horizon, and the colour of light bounced up from the ground.
    pub ground: Color,
    pub intensity: Fx,
}

/// A lamp: light from a point, fading to nothing at `radius`.
#[derive(Clone, Copy, Debug)]
pub struct PointLight {
    pub position: Vec3,
    pub color: Color,
    pub intensity: Fx,
    pub radius: Fx,
}

/// What a surface is made of.
#[derive(Clone, Copy, Debug)]
pub struct Material<'a> {
    /// The surface's own colour, or a tint over its texture.
    pub color: Color,
    pub texture: Option<&'a Texture<'a>>,
    /// How many times the texture repeats across one unit of `uv`.
    pub tiling: Fx,
    /// 0 is matte. Higher is a smaller, sharper, brighter highlight.
    pub shine: u8,
    /// How much of what is around it a surface shows, head on: 0 for none,
    /// 255 for a mirror. Everything reflects more at a glancing angle.
    pub reflect: u8,
    /// Light the surface gives off by itself, in sixteenths of its own
    /// colour's brightness: 16 is a screen at full white, 255 a lamp.
    pub glow: u8,
    /// Draw the back of each triangle as well. For things with no inside,
    /// like a sheet of paper.
    pub two_sided: bool,
}

impl<'a> Material<'a> {
    /// A matte surface of one colour.
    pub const fn color(color: Color) -> Self {
        Self {
            color,
            texture: None,
            tiling: Fx::ONE,
            shine: 0,
            reflect: 0,
            glow: 0,
            two_sided: false,
        }
    }

    /// A matte surface showing a texture.
    pub const fn textured(texture: &'a Texture<'a>) -> Self {
        Self::color(Color::WHITE).texture(texture)
    }

    pub const fn texture(mut self, texture: &'a Texture<'a>) -> Self {
        self.texture = Some(texture);
        self
    }

    pub const fn tiling(mut self, tiling: Fx) -> Self {
        self.tiling = tiling;
        self
    }

    pub const fn shine(mut self, shine: u8) -> Self {
        self.shine = shine;
        self
    }

    pub const fn reflect(mut self, reflect: u8) -> Self {
        self.reflect = reflect;
        self
    }

    pub const fn glow(mut self, glow: u8) -> Self {
        self.glow = glow;
        self
    }

    pub const fn two_sided(mut self) -> Self {
        self.two_sided = true;
        self
    }
}

/// One thing to draw.
#[derive(Clone, Copy)]
pub(crate) struct Item<'a> {
    pub mesh: &'a Mesh<'a>,
    pub transform: Transform,
    /// Where it was when the shutter opened. Motion blur smears from here.
    pub previous: Transform,
    pub material: Material<'a>,
}

/// The most lamps a scene can hold.
pub const MAX_LIGHTS: usize = 8;

/// One instant of a world, with room for `N` things in it.
pub struct Scene<'a, const N: usize> {
    pub camera: Camera,
    /// The camera when the shutter opened, if it was somewhere else. Motion
    /// blur smears the difference. Ask the same formula that placed the
    /// camera where it was a moment earlier; nothing has to remember.
    pub previous_camera: Option<Camera>,
    pub sun: Sun,
    pub sky: Sky,
    pub(crate) lights: [Option<PointLight>; MAX_LIGHTS],
    pub(crate) items: [Option<Item<'a>>; N],
    pub(crate) count: usize,
}

impl<'a, const N: usize> Scene<'a, N> {
    /// An empty world under a clear afternoon sky.
    pub const fn new(camera: Camera) -> Self {
        Self {
            camera,
            previous_camera: None,
            sun: Sun {
                direction: Vec3::f(-0.6, 0.7, 0.3),
                color: Color::rgb(255, 244, 224),
                intensity: fx(1.8),
            },
            sky: Sky {
                zenith: Color::rgb(60, 110, 200),
                horizon: Color::rgb(185, 205, 225),
                ground: Color::rgb(70, 62, 55),
                intensity: Fx::ONE,
            },
            lights: [None; MAX_LIGHTS],
            items: [None; N],
            count: 0,
        }
    }

    /// Put a model in the world. `false` if there is no room for it.
    pub fn draw(
        &mut self,
        mesh: &'a Mesh<'a>,
        transform: Transform,
        material: Material<'a>,
    ) -> bool {
        self.draw_moving(mesh, transform, transform, material)
    }

    /// Put a model in the world that was at `previous` when the shutter
    /// opened, so that motion blur can smear it.
    pub fn draw_moving(
        &mut self,
        mesh: &'a Mesh<'a>,
        transform: Transform,
        previous: Transform,
        material: Material<'a>,
    ) -> bool {
        let Some(slot) = self.items.get_mut(self.count) else {
            return false;
        };
        *slot = Some(Item {
            mesh,
            transform,
            previous,
            material,
        });
        self.count += 1;
        true
    }

    /// Add a lamp. `false` if the scene already has [`MAX_LIGHTS`].
    pub fn light(&mut self, light: PointLight) -> bool {
        match self.lights.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => {
                *slot = Some(light);
                true
            }
            None => false,
        }
    }

    pub(crate) fn items(&self) -> impl Iterator<Item = &Item<'a>> {
        self.items[..self.count].iter().flatten()
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Shadows cast by the sun.
#[derive(Clone, Copy, Debug)]
pub struct Shadows {
    /// How far from the camera's target shadows reach, in world units. The
    /// shadow map is spread over a square twice this wide, so smaller is
    /// sharper.
    pub extent: Fx,
}

/// Darkening where surfaces meet -- corners, creases, the ground under
/// things -- because less of the sky can see in there.
#[derive(Clone, Copy, Debug)]
pub struct AmbientOcclusion {
    /// How far away a surface can be and still shade this one, in world
    /// units.
    pub radius: Fx,
    /// 0 for none, 1 for the full effect.
    pub strength: Fx,
}

/// Light that has bounced once: a red wall tinting the floor beside it, a
/// lamp lighting what is next to it. Gathered from what is on screen.
#[derive(Clone, Copy, Debug)]
pub struct GlobalIllumination {
    /// How far light is gathered from, in world units.
    pub radius: Fx,
    pub strength: Fx,
}

/// Reflections of what is on screen, in surfaces whose material reflects.
/// Where the reflected thing is off screen, the sky shows instead.
#[derive(Clone, Copy, Debug)]
pub struct Reflections {
    /// How far a reflection can reach, in world units.
    pub distance: Fx,
}

/// Smearing of whatever moved while the shutter was open.
#[derive(Clone, Copy, Debug)]
pub struct MotionBlur {
    /// 1 smears the full distance moved since the previous transform; less
    /// is a faster shutter.
    pub strength: Fx,
}

/// A lens that can only focus at one distance.
#[derive(Clone, Copy, Debug)]
pub struct DepthOfField {
    /// How far away is sharp, in world units.
    pub focus: Fx,
    /// How blurred things at infinity get, as a fraction of the picture's
    /// height. 0.01 is gentle; 0.03 is a portrait lens.
    pub aperture: Fx,
}

/// Glow around things brighter than the screen can show.
#[derive(Clone, Copy, Debug)]
pub struct Bloom {
    /// How bright something must be before it glows, where 1 is white.
    pub threshold: Fx,
    pub strength: Fx,
}

/// How jagged edges are smoothed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AntiAliasing {
    Off,
    /// Find edges in the finished picture and blend along them. One pass,
    /// whatever was drawn.
    Fxaa,
}

/// Which effects are on, and how much of each.
///
/// Every field is one effect. `None` turns it off and costs nothing; start
/// from a preset and change what you want:
///
/// ```
/// use corrode_gfx::{fx, Bloom, Settings};
/// let mut settings = Settings::HIGH;
/// settings.film_grain = None;
/// settings.bloom = Some(Bloom { threshold: fx(1.2), strength: fx(0.5) });
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub shadows: Option<Shadows>,
    pub ambient_occlusion: Option<AmbientOcclusion>,
    pub global_illumination: Option<GlobalIllumination>,
    pub reflections: Option<Reflections>,
    pub motion_blur: Option<MotionBlur>,
    pub depth_of_field: Option<DepthOfField>,
    pub bloom: Option<Bloom>,
    /// How the range of light in the scene is fitted to the screen.
    /// [`Tonemap::Clip`](crate::Tonemap::Clip) is HDR off.
    pub tonemap: crate::Tonemap,
    /// Brightness of the whole picture, applied before the tone curve.
    pub exposure: Fx,
    /// Colour fringes towards the corners, as a real lens has: how far red
    /// and blue part at the corner, as a fraction of the picture's height.
    pub chromatic_aberration: Option<Fx>,
    pub anti_aliasing: AntiAliasing,
    /// How much the corners darken: 0 for none, 1 for black.
    pub vignette: Option<Fx>,
    /// Noise that changes every frame, like film: its size where 1 is the
    /// full range of a pixel. 0.03 is visible, 0.1 is heavy.
    pub film_grain: Option<Fx>,
}

impl Settings {
    /// Geometry, textures and plain lighting. Nothing else.
    pub const OFF: Self = Self {
        shadows: None,
        ambient_occlusion: None,
        global_illumination: None,
        reflections: None,
        motion_blur: None,
        depth_of_field: None,
        bloom: None,
        tonemap: crate::Tonemap::Filmic,
        exposure: Fx::ONE,
        chromatic_aberration: None,
        anti_aliasing: AntiAliasing::Off,
        vignette: None,
        film_grain: None,
    };

    /// The effects that cost least for what they add: shadows, smoothed
    /// edges, bloom.
    pub const LOW: Self = Self {
        shadows: Some(Shadows { extent: fx(12.0) }),
        bloom: Some(Bloom {
            threshold: fx(1.0),
            strength: fx(0.35),
        }),
        anti_aliasing: AntiAliasing::Fxaa,
        ..Self::OFF
    };

    /// Everything that makes a scene sit together, without the camera
    /// effects.
    pub const HIGH: Self = Self {
        ambient_occlusion: Some(AmbientOcclusion {
            radius: fx(0.6),
            strength: fx(0.9),
        }),
        global_illumination: Some(GlobalIllumination {
            radius: fx(2.0),
            strength: fx(1.0),
        }),
        reflections: Some(Reflections { distance: fx(20.0) }),
        ..Self::LOW
    };

    /// Everything, with a camera in front of it: blur, a lens, grain.
    pub const CINEMATIC: Self = Self {
        motion_blur: Some(MotionBlur { strength: Fx::ONE }),
        depth_of_field: Some(DepthOfField {
            focus: fx(6.0),
            aperture: fx(0.012),
        }),
        chromatic_aberration: Some(fx(0.004)),
        vignette: Some(fx(0.35)),
        film_grain: Some(fx(0.025)),
        ..Self::HIGH
    };
}

impl Default for Settings {
    fn default() -> Self {
        Self::HIGH
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real(v: [i32; 3]) -> [f64; 3] {
        v.map(|c| c as f64 / 65536.0)
    }

    #[test]
    fn the_camera_looks_down_its_own_z() {
        let camera = Camera::look_at(Vec3::new(0, 0, 5), Vec3::ZERO);
        let view = camera.view();
        // The target is five units straight ahead.
        let [x, y, z] = real(view.point([0; 3]));
        assert!(x.abs() < 1e-3 && y.abs() < 1e-3 && (z - 5.0).abs() < 1e-3);
        // World right is the camera's right, world up its up.
        let [x, y, z] = real(view.point(Vec3::new(1, 2, 0).raw()));
        assert!((x - 1.0).abs() < 1e-3 && (y - 2.0).abs() < 1e-3 && (z - 5.0).abs() < 1e-3);
    }

    #[test]
    fn a_camera_looking_straight_down_still_has_a_right() {
        let camera = Camera::look_at(Vec3::new(0, 9, 0), Vec3::ZERO);
        let view = camera.view();
        let [x, y, z] = real(view.point([0; 3]));
        assert!(x.abs() < 1e-3 && y.abs() < 1e-3 && (z - 9.0).abs() < 1e-3);
    }

    #[test]
    fn focal_length_follows_the_field_of_view() {
        for (degrees, height) in [(60, 400usize), (90, 1080), (20, 600), (120, 300)] {
            let camera = Camera::look_at(Vec3::Z, Vec3::ZERO).fov(Angle::degrees(degrees));
            let exact = height as f64 / 2.0 / ((degrees as f64).to_radians() / 2.0).tan();
            let got = camera.focal(height) as f64 / 65536.0;
            assert!(
                (got - exact).abs() < exact * 1e-5,
                "{degrees}: {got} vs {exact}"
            );
        }
    }

    #[test]
    fn a_full_scene_refuses_more() {
        let mut scene: Scene<2> = Scene::new(Camera::look_at(Vec3::Z, Vec3::ZERO));
        let matte = Material::color(Color::WHITE);
        assert!(scene.draw(&crate::mesh::CUBE, Transform::IDENTITY, matte));
        assert!(scene.draw(&crate::mesh::CUBE, Transform::IDENTITY, matte));
        assert!(!scene.draw(&crate::mesh::CUBE, Transform::IDENTITY, matte));
        assert_eq!(scene.items().count(), 2);
    }
}

//! A scene: what is on screen, back to front, and the least work that keeps
//! the glass matching it.
//!
//! A [`Layer`] is a rectangle of colour whose position, size and opacity are
//! each an [`Animation`] -- a formula, not a value somebody updates. A
//! [`Scene`] is a stack of them. Its one operation, [`Scene::frame`], asks
//! every formula where it is *now*, compares the answers with what was last
//! put on the glass, and repaints the difference.
//!
//! That comparison is the whole scheduling model. The scene does not know or
//! care how long it has been since it was last asked, so there is nothing to
//! keep in step and nothing that must run at any particular rate:
//!
//! * a frame in which nothing moved submits no commands and writes no pixels;
//! * [`Scene::animating_until`] says when the last formula goes quiet, so a
//!   painter can sleep through a still screen rather than poll one;
//! * a client that sets a layer moving is not involved again -- it hands over
//!   the description of a motion, not a stream of positions.
//!
//! # What a repaint costs
//!
//! Each stale rectangle is repainted bottom-up under a scissor. Bottom is not
//! always the background: if some opaque layer covers the whole of the stale
//! rectangle, nothing beneath it can show through, so painting starts there
//! and everything below is skipped. Under a maximised window the wallpaper is
//! never drawn again, however much moves on top of it.
//!
//! # What a layer shows
//!
//! One colour filling its rectangle, one colour pushed through a stencil from
//! the mask atlas -- which is what text is -- or pixels of its own, which is
//! what a window is. Those pixels belong to a client, and a client changing
//! them is the one thing that makes the glass stale without any formula
//! having moved, so it has to be said: [`Scene::damage`]. The scene decides
//! where the pixels go and when; it never looks at them.

use crate::anim::{Animation, Fx};
use crate::damage::Damage;
use crate::{Color, Command, CommandList, Rect, Renderer, SurfaceInfo};

/// Commands recorded before a batch is submitted and recording starts again.
const BATCH: usize = 16;

/// What a layer's colour is applied through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Content {
    /// The whole rectangle.
    Solid,
    /// A block of the mask atlas, its top-left corner at the layer's. The
    /// layer's rectangle crops it, so animating the width wipes it in.
    Mask { src: Rect },
    /// Pixels of its own: the surface the renderer knows as `id`, which is
    /// `width` by `height`, its top-left corner at the layer's. The layer's
    /// rectangle crops it and is never larger than it, so there is no part of
    /// the layer the surface does not cover. The layer's colour is not used.
    ///
    /// The layer must not outlast the surface: the scene takes it that an
    /// opaque one hides what is under it.
    Surface { id: u32, width: u32, height: u32 },
}

/// One rectangle of colour, in motion or not.
///
/// Plain data, like an [`Animation`]: `Copy`, no pointers, so a whole layer
/// can cross an IPC boundary by value.
#[derive(Clone, Copy, Debug)]
pub struct Layer {
    pub x: Animation,
    pub y: Animation,
    pub width: Animation,
    pub height: Animation,
    /// 0 to 255. A spring may carry it past either end; it is clamped when
    /// sampled, so an overshooting fade is merely fully there a little early.
    pub opacity: Animation,
    pub color: Color,
    pub content: Content,
}

impl Layer {
    /// An opaque rectangle that is not going anywhere.
    pub const fn solid(rect: Rect, color: Color) -> Self {
        Self {
            x: Animation::rest(Fx::from_int(rect.x)),
            y: Animation::rest(Fx::from_int(rect.y)),
            width: Animation::rest(Fx::from_int(clamp_size(rect.width))),
            height: Animation::rest(Fx::from_int(clamp_size(rect.height))),
            opacity: Animation::rest(Fx::from_int(255)),
            color,
            content: Content::Solid,
        }
    }

    /// `color` through the block `src` of the mask atlas, with its top-left
    /// corner at `x`, `y`.
    pub const fn mask(x: i32, y: i32, src: Rect, color: Color) -> Self {
        let mut layer = Self::solid(Rect::new(x, y, src.width, src.height), color);
        layer.content = Content::Mask { src };
        layer
    }

    /// The surface `id`, all `width` by `height` of it, with its top-left
    /// corner at `x`, `y`.
    pub const fn surface(x: i32, y: i32, id: u32, width: u32, height: u32) -> Self {
        let mut layer = Self::solid(Rect::new(x, y, width, height), Color::BLACK);
        layer.content = Content::Surface { id, width, height };
        layer
    }

    fn at(&self, now: u64) -> Shown {
        let whole = |animation: &Animation| animation.sample(now).value.round();
        // A surface has no pixels past its own edge to show.
        let (most_wide, most_tall) = match self.content {
            Content::Surface { width, height, .. } => (width, height),
            _ => (u32::MAX, u32::MAX),
        };
        Shown {
            rect: Rect::new(
                whole(&self.x),
                whole(&self.y),
                (whole(&self.width).max(0) as u32).min(most_wide),
                (whole(&self.height).max(0) as u32).min(most_tall),
            ),
            alpha: whole(&self.opacity).clamp(0, 255) as u8,
            color: self.color,
            content: self.content,
        }
    }

    /// The instant the last of this layer's animations ends.
    pub fn end_ns(&self) -> u64 {
        [self.x, self.y, self.width, self.height, self.opacity]
            .iter()
            .map(Animation::end_ns)
            .max()
            .unwrap_or(0)
    }
}

/// An `Fx` holds whole numbers up to 32767, which is also more pixels than
/// any layer has a use for.
const fn clamp_size(size: u32) -> i32 {
    if size > i16::MAX as u32 {
        i16::MAX as i32
    } else {
        size as i32
    }
}

/// The empty rectangle.
const NOWHERE: Rect = Rect::new(0, 0, 0, 0);

/// `rect`, `dx` to the right and `dy` down. A rectangle a client supplied can
/// be anywhere, so this saturates rather than wraps; what is left is clipped
/// by whoever asked.
fn moved(rect: Rect, dx: i32, dy: i32) -> Rect {
    Rect::new(
        rect.x.saturating_add(dx),
        rect.y.saturating_add(dy),
        rect.width,
        rect.height,
    )
}

/// What a layer looked like the last time it was painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shown {
    rect: Rect,
    alpha: u8,
    color: Color,
    content: Content,
}

impl Shown {
    const NOTHING: Self = Self {
        rect: Rect::new(0, 0, 0, 0),
        alpha: 0,
        color: Color::BLACK,
        content: Content::Solid,
    };

    /// Whether every pixel of the rectangle is this layer's and nothing
    /// else. A mask never is: it has holes by design. A surface is, because
    /// its pixels carry no transparency of their own.
    fn opaque(&self) -> bool {
        self.alpha == 255 && !matches!(self.content, Content::Mask { .. })
    }
}

/// Names a layer in the scene that issued it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerId(usize);

/// What one call to [`Scene::frame`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Painted {
    /// Pixels on screen that were out of date. Zero means nothing was
    /// submitted and nothing was presented.
    pub stale: u64,
    /// Pixels written to bring them up to date. More than `stale` where
    /// layers overlap; never more than `stale` times the layers involved.
    pub drawn: u64,
}

/// Up to `N` layers over a background, in fixed storage.
///
/// Layers stack in the order they were added. A removed layer's place is not
/// reused, so an id never comes to mean a different layer.
pub struct Scene<const N: usize> {
    screen: Rect,
    background: Color,
    layers: [Option<Layer>; N],
    shown: [Shown; N],
    /// The part of each layer its client has redrawn since the last frame,
    /// relative to the layer's corner. Empty when it has not.
    redrawn: [Rect; N],
    len: usize,
    painted: bool,
}

impl<const N: usize> Scene<N> {
    pub const fn new(info: &SurfaceInfo, background: Color) -> Self {
        Self {
            screen: Rect::new(0, 0, info.width, info.height),
            background,
            layers: [None; N],
            shown: [Shown::NOTHING; N],
            redrawn: [NOWHERE; N],
            len: 0,
            painted: false,
        }
    }

    pub const fn screen(&self) -> Rect {
        self.screen
    }

    /// Put `layer` on top of everything already here. `None` if full.
    pub fn add(&mut self, layer: Layer) -> Option<LayerId> {
        if self.len == N {
            return None;
        }
        self.layers[self.len] = Some(layer);
        self.len += 1;
        Some(LayerId(self.len - 1))
    }

    /// Take a layer out. The next frame repaints where it was.
    pub fn remove(&mut self, id: LayerId) {
        if let Some(slot) = self.layers.get_mut(id.0) {
            *slot = None;
            self.redrawn[id.0] = NOWHERE;
        }
    }

    /// Say that the pixels behind a layer have changed inside `rect`, which
    /// is relative to the layer's own corner. The next frame repaints that
    /// much of it, wherever the layer is by then.
    ///
    /// This is for a surface whose client has drawn into it. Nothing else
    /// needs it: a layer's place, size, colour and opacity are compared with
    /// what is on the glass every frame, but the scene never reads pixels.
    pub fn damage(&mut self, id: LayerId, rect: Rect) {
        if let Some(Some(_)) = self.layers.get(id.0) {
            self.redrawn[id.0] = self.redrawn[id.0].union(&rect);
        }
    }

    pub fn layer(&self, id: LayerId) -> Option<&Layer> {
        self.layers.get(id.0)?.as_ref()
    }

    /// Change a layer. Nothing needs telling: the next frame sees the
    /// difference between what the layer now says and what is on the glass.
    pub fn layer_mut(&mut self, id: LayerId) -> Option<&mut Layer> {
        self.layers.get_mut(id.0)?.as_mut()
    }

    /// When the last running animation ends, or `None` if `now` is already
    /// past all of them and the screen will stay as it is until told
    /// otherwise.
    pub fn animating_until(&self, now: u64) -> Option<u64> {
        self.layers
            .iter()
            .flatten()
            .map(Layer::end_ns)
            .max()
            .filter(|end| *end > now)
    }

    /// Bring the screen up to date for `now`.
    pub fn frame<R: Renderer>(&mut self, renderer: &mut R, now: u64) -> Result<Painted, R::Error> {
        let mut damage = Damage::new();
        if !self.painted {
            damage.add(self.screen);
        }

        let layers = self.layers.iter().zip(self.redrawn.iter_mut());
        for ((layer, redrawn), shown) in layers.zip(self.shown.iter_mut()) {
            let now_shown = match layer {
                Some(layer) => layer.at(now),
                None => Shown::NOTHING,
            };
            let redrawn = core::mem::replace(redrawn, NOWHERE);
            if now_shown == *shown {
                // In the same place looking the same, as far as the scene can
                // tell -- but its client says otherwise about part of it.
                if now_shown.alpha > 0 {
                    let stale = moved(redrawn, now_shown.rect.x, now_shown.rect.y)
                        .intersect(&now_shown.rect)
                        .and_then(|rect| rect.intersect(&self.screen));
                    if let Some(stale) = stale {
                        damage.add(stale);
                    }
                }
                continue;
            }
            // Where it was has to be repainted as well as where it is.
            // Overshoot can carry either off the edge, hence the clip.
            for state in [*shown, now_shown] {
                if state.alpha > 0 {
                    if let Some(visible) = state.rect.intersect(&self.screen) {
                        damage.add(visible);
                    }
                }
            }
            *shown = now_shown;
        }

        if damage.is_empty() {
            return Ok(Painted::default());
        }
        self.painted = true;

        let mut drawn = 0;
        for stale in damage.rects() {
            drawn += self.paint(renderer, *stale)?;
        }
        renderer.present(&damage)?;

        Ok(Painted {
            stale: damage.area(),
            drawn,
        })
    }

    /// Repaint `stale`, back to front, and return the pixels that took.
    fn paint<R: Renderer>(&self, renderer: &mut R, stale: Rect) -> Result<u64, R::Error> {
        let scissor = Command::Scissor { rect: stale };
        let mut storage = [scissor; BATCH];
        let mut list = CommandList::new(&mut storage);
        list.push(scissor);
        let mut drawn = 0;

        // The topmost opaque layer covering all of `stale` hides everything
        // under it, background included.
        let cover = self
            .shown
            .iter()
            .rposition(|shown| shown.opaque() && shown.rect.contains(&stale));
        let first = cover.unwrap_or(0);
        if cover.is_none() {
            list.push(Command::Clear {
                color: self.background,
            });
            drawn += stale.area();
        }

        for shown in &self.shown[first..] {
            let Some(visible) = shown.rect.intersect(&stale) else {
                continue;
            };
            let command = match (shown.content, shown.alpha) {
                (_, 0) => continue,
                (Content::Solid, 255) => Command::FillRect {
                    rect: shown.rect,
                    color: shown.color,
                },
                (Content::Solid, alpha) => Command::BlendRect {
                    rect: shown.rect,
                    color: shown.color,
                    alpha,
                },
                (Content::Mask { src }, alpha) => Command::Mask {
                    x: shown.rect.x,
                    y: shown.rect.y,
                    // No more of the block than the layer is currently wide
                    // and tall.
                    src: Rect::new(
                        src.x,
                        src.y,
                        src.width.min(shown.rect.width),
                        src.height.min(shown.rect.height),
                    ),
                    color: shown.color,
                    alpha,
                },
                (Content::Surface { id, .. }, alpha) => Command::Image {
                    x: shown.rect.x,
                    y: shown.rect.y,
                    src: Rect::new(0, 0, shown.rect.width, shown.rect.height),
                    surface: id,
                    alpha,
                },
            };
            if !list.push(command) {
                // Full is a normal condition: flush and carry on. The scissor
                // does not outlive a list, so it goes in again.
                renderer.submit(&list)?;
                list.clear();
                list.push(scissor);
                list.push(command);
            }
            drawn += visible.area();
        }

        renderer.submit(&list)?;
        Ok(drawn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anim::{Ease, Spring, Velocity};
    use crate::software::SoftwareRenderer;
    use crate::PixelFormat;

    const INFO: SurfaceInfo = SurfaceInfo {
        width: 64,
        height: 48,
        pitch: 64 * 4,
        format: PixelFormat::XRGB8888,
    };
    const GROUND: Color = Color::rgb(1, 2, 3);
    const MS: u64 = 1_000_000;

    /// A renderer that draws nothing and remembers what it was asked.
    #[derive(Default)]
    struct Recorder {
        commands: Vec<Command>,
        presented: Vec<Rect>,
    }

    impl Renderer for Recorder {
        type Error = ();

        fn surface_info(&self) -> SurfaceInfo {
            INFO
        }

        fn submit(&mut self, list: &CommandList<'_>) -> Result<(), ()> {
            self.commands.extend_from_slice(list.commands());
            Ok(())
        }

        fn upload_mask(&mut self, _: Rect, _: &[u8]) -> Result<(), ()> {
            Ok(())
        }

        fn present(&mut self, damage: &Damage) -> Result<(), ()> {
            self.presented.extend_from_slice(damage.rects());
            Ok(())
        }
    }

    fn fade(now: u64, from: i32, to: i32) -> Animation {
        Animation::ease(now, 100, Fx::from_int(from), Fx::from_int(to), Ease::Linear)
    }

    #[test]
    fn the_first_frame_paints_everything_and_the_second_nothing() {
        let mut scene = Scene::<4>::new(&INFO, GROUND);
        scene.add(Layer::solid(Rect::new(4, 4, 8, 8), Color::WHITE));
        let mut recorder = Recorder::default();

        let first = scene.frame(&mut recorder, 0).unwrap();
        assert_eq!(first.stale, 64 * 48);
        assert_eq!(first.drawn, 64 * 48 + 8 * 8);
        assert_eq!(recorder.presented, [scene.screen()]);

        let mut recorder = Recorder::default();
        assert_eq!(
            scene.frame(&mut recorder, 5 * MS).unwrap(),
            Painted::default()
        );
        assert!(recorder.commands.is_empty() && recorder.presented.is_empty());
    }

    #[test]
    fn a_move_damages_where_it_was_and_where_it_is() {
        let mut scene = Scene::<4>::new(&INFO, GROUND);
        let id = scene
            .add(Layer::solid(Rect::new(0, 0, 4, 4), Color::WHITE))
            .unwrap();
        scene.frame(&mut Recorder::default(), 0).unwrap();

        scene.layer_mut(id).unwrap().x = Animation::rest(Fx::from_int(40));
        let mut recorder = Recorder::default();
        let painted = scene.frame(&mut recorder, MS).unwrap();

        assert_eq!(painted.stale, 32);
        recorder.presented.sort_by_key(|rect| rect.x);
        assert_eq!(
            recorder.presented,
            [Rect::new(0, 0, 4, 4), Rect::new(40, 0, 4, 4)]
        );
    }

    #[test]
    fn a_fade_is_blended_until_it_arrives() {
        let mut scene = Scene::<4>::new(&INFO, GROUND);
        let mut layer = Layer::solid(Rect::new(8, 8, 8, 8), Color::WHITE);
        layer.opacity = fade(0, 0, 255);
        scene.add(layer);

        let blends = |scene: &mut Scene<4>, now| {
            let mut recorder = Recorder::default();
            scene.frame(&mut recorder, now).unwrap();
            recorder
                .commands
                .iter()
                .filter_map(|command| match command {
                    Command::BlendRect { alpha, .. } => Some(*alpha),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        // Invisible, then a fifth of the way, then a plain fill.
        assert_eq!(blends(&mut scene, 0), []);
        assert_eq!(blends(&mut scene, 20 * MS), [51]);
        let mut recorder = Recorder::default();
        scene.frame(&mut recorder, 100 * MS).unwrap();
        assert!(matches!(
            recorder.commands.last(),
            Some(Command::FillRect { .. })
        ));
        assert_eq!(scene.animating_until(100 * MS), None);
    }

    #[test]
    fn nothing_under_an_opaque_cover_is_drawn() {
        let mut scene = Scene::<4>::new(&INFO, GROUND);
        let buried = Color::rgb(200, 0, 0);
        let cover = Color::rgb(0, 200, 0);
        scene.add(Layer::solid(Rect::new(10, 10, 20, 20), buried));
        scene.add(Layer::solid(Rect::new(0, 0, 64, 48), cover));
        let top = scene
            .add(Layer::solid(Rect::new(12, 12, 4, 4), Color::WHITE))
            .unwrap();
        scene.frame(&mut Recorder::default(), 0).unwrap();

        scene.layer_mut(top).unwrap().opacity = fade(0, 255, 0);
        let mut recorder = Recorder::default();
        let painted = scene.frame(&mut recorder, 50 * MS).unwrap();

        // The cover and the fading layer, and no more: neither the background
        // nor the layer underneath can be seen, so neither is touched.
        assert_eq!(painted.stale, 16);
        assert_eq!(painted.drawn, 32);
        assert!(matches!(
            recorder.commands[..],
            [
                Command::Scissor { .. },
                Command::FillRect { color, .. },
                Command::BlendRect { .. }
            ] if color == cover
        ));
    }

    #[test]
    fn a_translucent_cover_hides_nothing() {
        let mut scene = Scene::<4>::new(&INFO, GROUND);
        let mut veil = Layer::solid(Rect::new(0, 0, 64, 48), Color::WHITE);
        veil.opacity = Animation::rest(Fx::from_int(254));
        scene.add(veil);
        let mut recorder = Recorder::default();
        scene.frame(&mut recorder, 0).unwrap();
        assert!(matches!(recorder.commands[1], Command::Clear { .. }));
    }

    #[test]
    fn a_mask_hides_nothing_and_is_cropped_to_its_layer() {
        let mut scene = Scene::<4>::new(&INFO, GROUND);
        let src = Rect::new(8, 0, 64, 48);
        let mut label = Layer::mask(0, 0, src, Color::WHITE);
        label.width = Animation::rest(Fx::from_int(10));
        scene.add(label);

        let mut recorder = Recorder::default();
        scene.frame(&mut recorder, 0).unwrap();
        // Fully opaque and it would cover the screen, but there is
        // background between the letters: the clear still happens.
        assert!(matches!(
            recorder.commands[1..],
            [
                Command::Clear { .. },
                Command::Mask { src, alpha: 255, .. }
            ] if src == Rect::new(8, 0, 10, 48)
        ));
    }

    #[test]
    fn a_surface_is_drawn_whole_and_hides_what_is_under_it() {
        let mut scene = Scene::<4>::new(&INFO, GROUND);
        scene.add(Layer::solid(Rect::new(0, 0, 64, 48), Color::WHITE));
        scene.add(Layer::surface(8, 6, 5, 20, 10));
        let mut recorder = Recorder::default();
        scene.frame(&mut recorder, 0).unwrap();

        let image = recorder.commands.iter().find_map(|command| match *command {
            Command::Image {
                x,
                y,
                src,
                surface,
                alpha,
            } => Some((x, y, src, surface, alpha)),
            _ => None,
        });
        assert_eq!(image, Some((8, 6, Rect::new(0, 0, 20, 10), 5, 255)));

        // Something moving entirely behind it is not drawn at all.
        let mut scene = Scene::<4>::new(&INFO, GROUND);
        let mover = scene
            .add(Layer::solid(Rect::new(10, 8, 4, 4), Color::WHITE))
            .unwrap();
        scene.add(Layer::surface(8, 6, 5, 20, 10));
        scene.frame(&mut Recorder::default(), 0).unwrap();
        scene.layer_mut(mover).unwrap().x = Animation::rest(Fx::from_int(14));
        let mut recorder = Recorder::default();
        scene.frame(&mut recorder, MS).unwrap();
        assert!(recorder
            .commands
            .iter()
            .all(|command| matches!(command, Command::Scissor { .. } | Command::Image { .. })));
    }

    #[test]
    fn a_surface_layer_is_never_larger_than_its_surface() {
        let mut scene = Scene::<4>::new(&INFO, GROUND);
        let id = scene.add(Layer::surface(0, 0, 1, 20, 10)).unwrap();
        // Asked to be bigger than it has pixels for, and then smaller.
        scene.layer_mut(id).unwrap().width = Animation::rest(Fx::from_int(50));
        scene.layer_mut(id).unwrap().height = Animation::rest(Fx::from_int(4));
        let mut recorder = Recorder::default();
        scene.frame(&mut recorder, 0).unwrap();
        assert!(recorder.commands.iter().any(|command| matches!(
            command,
            Command::Image { src, .. } if *src == Rect::new(0, 0, 20, 4)
        )));

        // So the part it does not cover is not taken to be hidden: a change
        // there is painted from the background up.
        let mut recorder = Recorder::default();
        let mut veil = Layer::solid(Rect::new(30, 0, 4, 4), Color::WHITE);
        veil.opacity = Animation::rest(Fx::from_int(128));
        scene.add(veil);
        scene.frame(&mut recorder, MS).unwrap();
        assert!(recorder
            .commands
            .iter()
            .any(|command| matches!(command, Command::Clear { .. })));
    }

    #[test]
    fn what_a_client_redrew_is_repainted_and_nothing_else() {
        let mut scene = Scene::<4>::new(&INFO, GROUND);
        let id = scene.add(Layer::surface(10, 20, 0, 30, 20)).unwrap();
        scene.frame(&mut Recorder::default(), 0).unwrap();

        // Two changes before the next frame are one repaint.
        scene.damage(id, Rect::new(2, 3, 4, 5));
        scene.damage(id, Rect::new(4, 3, 4, 2));
        let mut recorder = Recorder::default();
        let painted = scene.frame(&mut recorder, MS).unwrap();
        assert_eq!(recorder.presented, [Rect::new(12, 23, 6, 5)]);
        assert_eq!((painted.stale, painted.drawn), (30, 30));

        // Said once, repainted once.
        assert_eq!(
            scene.frame(&mut Recorder::default(), 2 * MS).unwrap(),
            Painted::default()
        );

        // A rectangle from a client is clipped to the layer, wherever and
        // however large it claims to be.
        for wild in [
            Rect::new(-5, -5, 10, 10),
            Rect::new(i32::MIN, i32::MIN, u32::MAX, u32::MAX),
            Rect::new(i32::MAX, i32::MAX, u32::MAX, u32::MAX),
            Rect::new(0, 0, u32::MAX, u32::MAX),
        ] {
            scene.damage(id, wild);
            let mut recorder = Recorder::default();
            scene.frame(&mut recorder, 3 * MS).unwrap();
            for rect in &recorder.presented {
                assert!(
                    Rect::new(10, 20, 30, 20).contains(rect),
                    "{wild:?} -> {rect:?}"
                );
            }
        }

        // An invisible layer has nothing on the glass to be out of date, and
        // a layer that is gone has no pixels to speak of.
        scene.layer_mut(id).unwrap().opacity = Animation::rest(Fx::ZERO);
        scene.frame(&mut Recorder::default(), 4 * MS).unwrap();
        scene.damage(id, Rect::new(0, 0, 30, 20));
        assert_eq!(
            scene.frame(&mut Recorder::default(), 5 * MS).unwrap(),
            Painted::default()
        );
        scene.remove(id);
        scene.damage(id, Rect::new(0, 0, 30, 20));
        assert_eq!(
            scene.frame(&mut Recorder::default(), 6 * MS).unwrap(),
            Painted::default()
        );
    }

    #[test]
    fn more_layers_than_one_batch_are_all_drawn() {
        let mut scene = Scene::<40>::new(&INFO, GROUND);
        for i in 0..40 {
            scene.add(Layer::solid(Rect::new(i, 0, 1, 1), Color::WHITE));
        }
        let mut recorder = Recorder::default();
        scene.frame(&mut recorder, 0).unwrap();

        let fills = recorder
            .commands
            .iter()
            .filter(|command| matches!(command, Command::FillRect { .. }))
            .count();
        assert_eq!(fills, 40);
        // Every fill is preceded by a scissor in its own batch.
        for batch in recorder.commands.chunks(BATCH) {
            assert!(matches!(batch[0], Command::Scissor { .. }));
        }
        assert!(scene
            .add(Layer::solid(Rect::new(0, 0, 1, 1), Color::WHITE))
            .is_none());
    }

    #[test]
    fn it_knows_when_it_will_be_still() {
        let mut scene = Scene::<4>::new(&INFO, GROUND);
        let id = scene
            .add(Layer::solid(Rect::new(0, 0, 4, 4), Color::WHITE))
            .unwrap();
        assert_eq!(scene.animating_until(0), None);

        scene.layer_mut(id).unwrap().opacity = fade(10 * MS, 255, 0);
        assert_eq!(scene.animating_until(0), Some(110 * MS));
        assert_eq!(scene.animating_until(110 * MS), None);
    }

    /// A screen and a back buffer the size of [`INFO`].
    struct Glass {
        front: Vec<u32>,
        back: Vec<u32>,
    }

    /// Side lengths of the test surfaces.
    const WINDOW: (u32, u32) = (40, 30);

    /// Two clients' worth of pixels, which every renderer in a test is shown.
    /// Written through the same pointers the renderers read through, the way
    /// a client writes while a compositor reads.
    struct Windows {
        _pixels: [Vec<u32>; 2],
        base: [*mut u32; 2],
    }

    impl Windows {
        fn new(random: &mut Random) -> Self {
            let mut pixels = [(); 2].map(|_| {
                (0..WINDOW.0 * WINDOW.1)
                    .map(|_| random.next() as u32 & 0x00ff_ffff)
                    .collect::<Vec<u32>>()
            });
            let base = [pixels[0].as_mut_ptr(), pixels[1].as_mut_ptr()];
            Self {
                _pixels: pixels,
                base,
            }
        }

        /// Draw something new inside `rect` of one surface.
        fn scribble(&mut self, surface: usize, rect: Rect, random: &mut Random) {
            for y in rect.y as u32..rect.y as u32 + rect.height {
                for x in rect.x as u32..rect.x as u32 + rect.width {
                    // SAFETY: callers keep `rect` inside WINDOW, and the
                    // vector behind `base` is that many pixels.
                    unsafe {
                        self.base[surface]
                            .add((y * WINDOW.0 + x) as usize)
                            .write(random.next() as u32 & 0x00ff_ffff);
                    }
                }
            }
        }
    }

    impl Glass {
        fn new() -> Self {
            let pixels = (INFO.width * INFO.height) as usize;
            Self {
                // Different garbage in each, so anything left unpainted or
                // unpresented shows up as a mismatch.
                front: vec![0xdead_beef; pixels],
                back: vec![0x0bad_f00d; pixels],
            }
        }

        fn renderer(&mut self, windows: &Windows) -> SoftwareRenderer {
            // SAFETY: both vectors are `pitch * height` bytes, separate, and
            // only touched through the renderer while it is in use. Each
            // window is WINDOW pixels of its own and outlives the renderer.
            unsafe {
                let mut renderer =
                    SoftwareRenderer::new(self.front.as_mut_ptr().cast(), INFO).unwrap();
                renderer.attach_back_buffer(self.back.as_mut_ptr().cast());
                for (id, base) in windows.base.iter().enumerate() {
                    renderer
                        .attach_surface(id as u32, *base, WINDOW.0, WINDOW.1)
                        .unwrap();
                }

                // Every renderer gets the same stencils: a fixed scramble
                // with plenty of 0s and 255s among the in-betweens.
                let mut noise = Random(99);
                let coverage: Vec<u8> = (0..ATLAS * ATLAS)
                    .map(|_| match noise.below(4) {
                        0 => 0,
                        1 => 255,
                        _ => noise.next() as u8,
                    })
                    .collect();
                renderer.attach_mask_atlas(vec![0u8; coverage.len()].leak(), ATLAS);
                renderer
                    .upload_mask(Rect::new(0, 0, ATLAS, ATLAS), &coverage)
                    .unwrap();
                renderer
            }
        }
    }

    /// Side of the test atlas, in pixels.
    const ATLAS: u32 = 32;

    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, limit: u64) -> u64 {
            self.next() % limit
        }

        fn between(&mut self, low: i32, high: i32) -> Fx {
            Fx::from_int(low + self.below((high - low) as u64) as i32)
        }
    }

    /// The property everything else here exists to keep: however a scene got
    /// to its present state -- whichever frames were painted or skipped,
    /// whatever was culled, wherever the damage fell -- the glass shows
    /// exactly what painting that state from scratch would.
    #[test]
    fn repainting_only_the_damage_matches_repainting_everything() {
        let mut random = Random(7);
        let mut windows = Windows::new(&mut random);
        let mut glass = Glass::new();
        let mut renderer = glass.renderer(&windows);
        let mut scene = Scene::<12>::new(&INFO, GROUND);
        let mut ids = Vec::new();
        let mut now = 0u64;
        let (mut skipped, mut culled, mut redrawn) = (0, 0, 0);

        for _ in 0..1500 {
            now += random.below(40) * MS;

            match random.below(10) {
                0 if ids.len() < 6 => {
                    let rect = Rect::new(
                        random.between(-20, 70).round(),
                        random.between(-20, 50).round(),
                        random.below(80) as u32,
                        random.below(60) as u32,
                    );
                    let shade = random.next() as u8;
                    let mut layer = Layer::solid(rect, Color::rgb(shade, 90, !shade));
                    if random.below(3) == 0 {
                        // Some of these reach outside the atlas on purpose.
                        let src = Rect::new(
                            random.below(24) as i32,
                            random.below(24) as i32,
                            rect.width,
                            rect.height,
                        );
                        layer.content = Content::Mask { src };
                    } else {
                        // A window, often asked to be larger than it is.
                        layer.content = Content::Surface {
                            id: random.below(2) as u32,
                            width: WINDOW.0,
                            height: WINDOW.1,
                        };
                    }
                    ids.extend(scene.add(layer));
                }
                1 if !ids.is_empty() => {
                    scene.remove(ids.swap_remove(random.below(ids.len() as u64) as usize));
                }
                2..=6 if !ids.is_empty() => {
                    let id = ids[random.below(ids.len() as u64) as usize];
                    let layer = scene.layer_mut(id).unwrap();
                    match random.below(4) {
                        0 => {
                            let to = random.between(-30, 80);
                            layer.x = layer.x.retarget(now, to, Spring::BOUNCY);
                        }
                        1 => {
                            let from = layer.y.sample(now).value;
                            let to = random.between(-30, 60);
                            layer.y = Animation::ease(now, 150, from, to, Ease::STANDARD);
                        }
                        2 => {
                            let to = random.between(0, 90);
                            layer.width = layer.width.retarget(now, to, Spring::SNAPPY);
                        }
                        _ => {
                            let from = layer.opacity.sample(now).value;
                            let to = [0, 255, 255, 120][random.below(4) as usize];
                            layer.opacity = Animation::spring(
                                now,
                                from,
                                Fx::from_int(to),
                                Velocity::ZERO,
                                Spring::BOUNCY,
                            );
                        }
                    }
                }
                // A client draws into its window and says where.
                7..=9 => {
                    let window =
                        ids.iter()
                            .copied()
                            .find_map(|id| match scene.layer(id)?.content {
                                Content::Surface { id: surface, .. } => Some(surface),
                                _ => None,
                            });
                    if let Some(surface) = window {
                        let x = random.below(WINDOW.0 as u64 - 1) as u32;
                        let y = random.below(WINDOW.1 as u64 - 1) as u32;
                        let rect = Rect::new(
                            x as i32,
                            y as i32,
                            1 + random.below((WINDOW.0 - x - 1).min(12) as u64 + 1) as u32,
                            1 + random.below((WINDOW.1 - y - 1).min(12) as u64 + 1) as u32,
                        );
                        windows.scribble(surface as usize, rect, &mut random);
                        // Every layer showing that surface is now out of date.
                        for other in &ids {
                            if let Some(Content::Surface { id: shows, .. }) =
                                scene.layer(*other).map(|layer| layer.content)
                            {
                                if shows == surface {
                                    scene.damage(*other, rect);
                                }
                            }
                        }
                        redrawn += 1;
                    }
                }
                _ => {}
            }

            let painted = scene.frame(&mut renderer, now).unwrap();
            skipped += (painted.stale == 0) as u32;
            culled += (painted.stale > 0 && painted.drawn < painted.stale * 2) as u32;

            // The same layers, in a scene that has never painted anything.
            let mut fresh_glass = Glass::new();
            let mut fresh = Scene::<12>::new(&INFO, GROUND);
            fresh.layers = scene.layers;
            fresh.len = scene.len;
            fresh
                .frame(&mut fresh_glass.renderer(&windows), now)
                .unwrap();

            assert_eq!(glass.front, fresh_glass.front, "at {} ms", now / MS);
        }

        // The run has to have exercised the paths it is vouching for.
        assert!(skipped > 50, "only {skipped} still frames");
        assert!(culled > 50, "only {culled} partial repaints");
        assert!(redrawn > 20, "only {redrawn} client redraws");
    }
}

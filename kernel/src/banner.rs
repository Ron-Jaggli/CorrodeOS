//! A notification on screen: a glass banner that slides in at the top
//! right, under the menu bar, as macOS's do, and slides out again after a
//! few seconds -- or at once, when clicked.
//!
//! The icon is the dock's for the program that sent it, if the dock has
//! one; the text is drawn into two of the window title slots, which the
//! desk lends and takes back.

use alloc::vec::Vec;

use corrode_render::anim::{Animation, Ease, Fx, Spring, Velocity};
use corrode_render::scene::{Layer, LayerId, Scene};
use corrode_render::{Color, Rect};
use corrode_theme::Theme;

const WIDTH: u32 = 340;
const HEIGHT: u32 = 66;
/// From the screen's right edge, and below the menu bar.
const MARGIN: i32 = 12;
const TOP: i32 = 28 + 10;
const PAD: i32 = 12;
/// How long it stays, and how long it takes to go.
const SHOWN_NS: u64 = 4_000_000_000;
const LEAVING_MS: u32 = 280;

pub struct Banner {
    layers: Vec<LayerId>,
    /// The title slots it borrowed, to give back.
    pub slots: [usize; 2],
    /// When it starts to leave, and when it has gone.
    leave_at: u64,
    gone_at: Option<u64>,
    /// How far each layer sits from the pane's left edge.
    offsets: Vec<i32>,
}

/// What a banner shows: the icon's stencils, and the name and text
/// stencils with how wide each came out.
pub struct Content {
    pub icon: Option<(Rect, Option<Rect>, Color)>,
    pub name: (Rect, u32),
    pub text: (Rect, u32),
    pub slots: [usize; 2],
}

impl Banner {
    pub fn show<const N: usize>(
        scene: &mut Scene<N>,
        theme: &Theme,
        content: Content,
        now: u64,
    ) -> Option<Self> {
        let screen = scene.screen();
        let rest = screen.width as i32 - WIDTH as i32 - MARGIN;
        let mut layers = Vec::new();
        let mut offsets = Vec::new();
        let mut add = |scene: &mut Scene<N>, layer: Layer, offset: i32| -> Option<()> {
            layers.push(scene.add(layer)?);
            offsets.push(offset);
            Some(())
        };
        add(scene, crate::input::pane(Rect::new(rest, TOP, WIDTH, HEIGHT), 18, theme), 0)?;
        let mut text_x = PAD;
        if let Some((tile, glyph, color)) = content.icon {
            // Scaled masks there are not, so the icon is as big as the
            // dock's, within reason.
            if tile.height <= HEIGHT - 12 {
                let y = TOP + (HEIGHT as i32 - tile.height as i32) / 2;
                add(scene, Layer::mask(rest + PAD, y, tile, color), PAD)?;
                if let Some(glyph) = glyph {
                    add(scene, Layer::mask(rest + PAD, y, glyph, Color::WHITE), PAD)?;
                }
                text_x = PAD + tile.width as i32 + 12;
            }
        }
        let (name, name_width) = content.name;
        let (text, text_width) = content.text;
        let mut name_layer = Layer::mask(rest + text_x, TOP + 12, name, theme.text());
        name_layer.width = Animation::rest(Fx::from_int(name_width as i32));
        add(scene, name_layer, text_x)?;
        let mut text_layer = Layer::mask(rest + text_x, TOP + 34, text, theme.text());
        text_layer.width = Animation::rest(Fx::from_int(text_width as i32));
        text_layer.opacity = Animation::rest(Fx::from_int(200));
        add(scene, text_layer, text_x)?;

        // In from beyond the right edge, on a spring.
        let start = screen.width as i32 + 8;
        let animate = theme.speed > 0;
        for (id, offset) in layers.iter().zip(&offsets) {
            if let (Some(layer), true) = (scene.layer_mut(*id), animate) {
                layer.x = Animation::spring(
                    now,
                    Fx::from_int(start + offset),
                    Fx::from_int(rest + offset),
                    Velocity::ZERO,
                    Spring::SMOOTH,
                );
            }
        }
        Some(Banner {
            layers,
            slots: content.slots,
            leave_at: now + SHOWN_NS,
            gone_at: None,
            offsets,
        })
    }

    /// Start leaving now, if it has not already.
    pub fn dismiss<const N: usize>(&mut self, scene: &mut Scene<N>, theme: &Theme, now: u64) {
        if self.gone_at.is_some() {
            return;
        }
        let ms = theme.ms(LEAVING_MS);
        let away = scene.screen().width as i32 + 8;
        for (id, offset) in self.layers.iter().zip(&self.offsets) {
            if let Some(layer) = scene.layer_mut(*id) {
                let from = layer.x.sample(now).value;
                layer.x = if ms == 0 {
                    Animation::rest(Fx::from_int(away + offset))
                } else {
                    Animation::ease(now, ms, from, Fx::from_int(away + offset), Ease::In)
                };
            }
        }
        self.gone_at = Some(now + ms as u64 * 1_000_000);
    }

    /// Time passing: leave when its time is up. True once it has gone and
    /// can be taken down.
    pub fn tick<const N: usize>(&mut self, scene: &mut Scene<N>, theme: &Theme, now: u64) -> bool {
        if now >= self.leave_at {
            self.dismiss(scene, theme, now);
        }
        self.gone_at.is_some_and(|at| now >= at)
    }

    pub fn contains(&self, id: LayerId) -> bool {
        self.layers.contains(&id)
    }

    pub fn layers(&self) -> impl Iterator<Item = LayerId> + '_ {
        self.layers.iter().copied()
    }

    pub fn close<const N: usize>(self, scene: &mut Scene<N>) {
        for id in self.layers {
            scene.remove(id);
        }
    }
}

//! The window switcher: Alt+Tab, as macOS's Cmd+Tab.
//!
//! While Alt is held, a glass panel in the middle of the screen shows an
//! icon for each window, most recently used first, with the next one
//! picked. Each Tab picks the one after (Shift+Tab the one before), and
//! letting go of Alt brings the picked window to the front. Escape puts the
//! panel away and changes nothing.
//!
//! The icons are the dock's own stencils; the name under the pick is the
//! window's own title stencil. Nothing new is drawn but the highlight.

use alloc::vec::Vec;

use corrode_render::anim::{Animation, Ease, Fx, Spring};
use corrode_render::scene::{Layer, LayerId, Scene};
use corrode_render::{Color, Rect, Renderer};
use corrode_theme::Theme;

/// The highlight's stencil: in the right half of the atlas, below the
/// dock's labels.
const HIGHLIGHT_AT: (i32, i32) = (512, crate::input::ATLAS_ROW as i32 + 152);
/// Room around each icon, and around them all.
const AROUND: u32 = 14;
const PAD: u32 = 12;
/// Room under the icons for the picked window's name.
const LABEL: u32 = 26;

/// One window in the switcher.
pub struct Entry {
    /// The window's id, which outlives an index into the desk's list.
    pub window: usize,
    /// The dock's stencils for its program: the tile, its glyph and the
    /// tile's colour. None for a program the dock does not know.
    pub icon: Option<(Rect, Option<Rect>, Color)>,
    /// The window's title stencil, if it has one.
    pub title: Option<Rect>,
}

pub struct Switcher {
    entries: Vec<Entry>,
    picked: usize,
    layers: Vec<LayerId>,
    highlight: LayerId,
    label: Option<LayerId>,
    /// Where the first slot starts, and the icons' size.
    left: i32,
    top: i32,
    size: u32,
    text: Color,
}

/// A rounded square `size` across with corners of `radius`, sampled four
/// by four.
fn rounded(size: u32, radius: u32) -> Vec<u8> {
    let (size, radius) = (size as i64, radius as i64);
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    let (w, r) = (size * 4, radius * 4);
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let (qx, qy) = (px * 4 + sx, py * 4 + sy);
                    let dx = (r - qx).max(qx - (w - 1 - r)).max(0);
                    let dy = (r - qy).max(qy - (w - 1 - r)).max(0);
                    hits += (dx * dx + dy * dy <= r * r) as i64;
                }
            }
            coverage[(py * size + px) as usize] = (hits * 255 / 16) as u8;
        }
    }
    coverage
}

impl Switcher {
    /// Put the panel up with `entries`, `picked` chosen.
    pub fn open<R: Renderer, const N: usize>(
        renderer: &mut R,
        scene: &mut Scene<N>,
        theme: &Theme,
        size: u32,
        entries: Vec<Entry>,
        picked: usize,
        now: u64,
    ) -> Option<Self> {
        let slot = size + 2 * AROUND;
        let count = entries.len() as u32;
        let (width, height) = (count * slot + 2 * PAD, slot + 2 * PAD + LABEL);
        let screen = scene.screen();
        let x = (screen.width as i32 - width as i32) / 2;
        let y = (screen.height as i32 - height as i32) / 2;
        let mut layers = Vec::new();

        let mut pane = crate::input::pane(Rect::new(x, y, width, height), 22, theme);
        let ms = theme.ms(120);
        if ms > 0 {
            pane.opacity = Animation::ease(now, ms, Fx::ZERO, Fx::from_int(255), Ease::Out);
        }
        layers.push(scene.add(pane)?);

        let (left, top) = (x + PAD as i32, y + PAD as i32);
        let src = Rect::new(HIGHLIGHT_AT.0, HIGHLIGHT_AT.1, slot, slot);
        // A faint wash over the pane, light on dark and dark on light.
        let (wash, strength) = if theme.dark {
            (Color::WHITE, 0x38)
        } else {
            (Color::BLACK, 0x22)
        };
        let at = left + (picked as u32 * slot) as i32;
        let mut highlight = if renderer.upload_mask(src, &rounded(slot, 14)).is_ok() {
            Layer::mask(at, top, src, wash)
        } else {
            Layer::solid(Rect::new(at, top, slot, slot), wash)
        };
        highlight.opacity = Animation::rest(Fx::from_int(strength));
        let highlight = scene.add(highlight)?;
        layers.push(highlight);

        for (index, entry) in entries.iter().enumerate() {
            let (ix, iy) = (left + (index as u32 * slot + AROUND) as i32, top + AROUND as i32);
            match entry.icon {
                Some((tile, glyph, color)) => {
                    layers.push(scene.add(Layer::mask(ix, iy, tile, color))?);
                    if let Some(glyph) = glyph {
                        layers.push(scene.add(Layer::mask(ix, iy, glyph, Color::WHITE))?);
                    }
                }
                None => {
                    let plain = Layer::solid(Rect::new(ix, iy, size, size), theme.surface());
                    layers.push(scene.add(plain)?);
                }
            }
        }
        let mut switcher = Switcher {
            entries,
            picked,
            layers,
            highlight,
            label: None,
            left,
            top,
            size,
            text: theme.text(),
        };
        switcher.name(scene);
        Some(switcher)
    }

    /// The picked window's name, centred under its icon.
    fn name<const N: usize>(&mut self, scene: &mut Scene<N>) {
        if let Some(label) = self.label.take() {
            scene.remove(label);
            self.layers.retain(|id| *id != label);
        }
        let Some(src) = self.entries.get(self.picked).and_then(|entry| entry.title) else {
            return;
        };
        let slot = self.size + 2 * AROUND;
        let centre = self.left + (self.picked as u32 * slot + slot / 2) as i32;
        let y = self.top + slot as i32 + (LABEL as i32 - src.height as i32) / 2 + 2;
        if let Some(label) = scene.add(Layer::mask(centre - src.width as i32 / 2, y, src, self.text)) {
            self.label = Some(label);
            self.layers.push(label);
        }
    }

    /// Pick the next window, or the one before for `back`, round the ends.
    pub fn step<const N: usize>(&mut self, scene: &mut Scene<N>, back: bool, now: u64) {
        let count = self.entries.len();
        self.picked = if back {
            (self.picked + count - 1) % count
        } else {
            (self.picked + 1) % count
        };
        let slot = self.size + 2 * AROUND;
        let x = Fx::from_int(self.left + (self.picked as u32 * slot) as i32);
        if let Some(layer) = scene.layer_mut(self.highlight) {
            layer.x = layer.x.retarget(now, x, Spring::SNAPPY);
        }
        self.name(scene);
    }

    /// The window picked, by id.
    pub fn picked(&self) -> Option<usize> {
        self.entries.get(self.picked).map(|entry| entry.window)
    }

    /// Every layer, to keep it above the windows.
    pub fn layers(&self) -> impl Iterator<Item = LayerId> + '_ {
        self.layers.iter().copied()
    }

    /// Take the panel down.
    pub fn close<const N: usize>(self, scene: &mut Scene<N>) {
        for id in self.layers {
            scene.remove(id);
        }
    }
}

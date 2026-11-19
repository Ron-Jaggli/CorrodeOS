//! The dock: a pane of glass floating above the bottom of the screen, with
//! an icon for each program the launcher knows.
//!
//! Clicking an icon asks the launcher to start the program -- through the
//! same endpoint the panel holds, so the dock can start nothing the panel
//! could not -- and the icon jumps while it does, on a spring. A dot under
//! an icon says the program has a window open.
//!
//! The icons are drawn here, once, as stencils in the mask atlas: a rounded
//! square in the program's colour (a superellipse, as Apple's icons are)
//! with a white glyph over it.
//!
//! The icon under the pointer lifts a little, and its name shows above it
//! on a pill of glass, as macOS's dock labels its icons.

use alloc::vec::Vec;

use corrode_abi::launcher;
use corrode_render::anim::{Animation, Fx, Spring, Velocity};
use corrode_render::scene::{Layer, LayerId, Scene};
use corrode_render::{Color, Rect, Renderer};
use corrode_text::{Coverage, Style, Weight};
use corrode_theme::Theme;

use crate::println;

/// What a dock icon looks like.
#[derive(Clone, Copy)]
enum Glyph {
    /// A line of text, centred.
    Text(&'static str),
    Gear,
    Folder,
}

/// A program the dock can start.
struct App {
    /// What its window's title says, to know when it is running.
    title: &'static str,
    program: u64,
    color: Color,
    glyph: Glyph,
}

const APPS: [App; 4] = [
    // First, where Finder is.
    App {
        title: "Files",
        program: launcher::FILES,
        color: Color::rgb(0x1e, 0x9b, 0xf0),
        glyph: Glyph::Folder,
    },
    App {
        title: "Terminal",
        program: launcher::TERMINAL,
        color: Color::rgb(0x1d, 0x1f, 0x24),
        glyph: Glyph::Text(">_"),
    },
    App {
        title: "Settings",
        program: launcher::SETTINGS,
        color: Color::rgb(0x7c, 0x7f, 0x88),
        glyph: Glyph::Gear,
    },
    App {
        title: "About",
        program: launcher::ABOUT,
        color: Color::rgb(0x0a, 0x84, 0xff),
        glyph: Glyph::Text("i"),
    },
];

/// Where the dock keeps its stencils: the right half of the atlas, below
/// the pointer's row. The tile, then each glyph, side by side; the running
/// dot below them.
const ATLAS_X: i32 = 512;
const ATLAS_Y: i32 = crate::input::ATLAS_ROW as i32 + 64;
const DOT: u32 = 5;
/// The names' stencils, in a row below the icons'.
const LABEL_Y: i32 = ATLAS_Y + 64;
const LABEL_SLOT: u32 = 128;
const LABEL: Style = Style::px(13);
/// The tile and the glyphs again, at the size they grow to under the
/// pointer, in a row of their own below everything else. A stencil shrunk
/// stays sharp; one stretched goes soft.
const BIG_Y: i32 = crate::input::ATLAS_ROW as i32 + 256;
const BIG_ROWS: u32 = 256;
/// How far a hovered icon rises.
const LIFT: i32 = 6;

/// The dock floats this far above the bottom edge, with this much around
/// its icons.
const GAP: i32 = 10;
const PAD: u32 = 8;

/// How much of the magnification an icon gets by how far it is from the
/// one under the pointer, in percent: all of it, half, a little.
const FALLOFF: [u32; 3] = [100, 50, 15];

struct Icon {
    tile: LayerId,
    glyph: Option<LayerId>,
    dot: Option<LayerId>,
    /// Where the icon rests.
    at: (i32, i32),
    running: bool,
    /// Its name above it on hover: the pill, then the text on it.
    tooltip: Option<[LayerId; 2]>,
    /// Where the pill rests, for it to follow the icon as it grows.
    tip_at: (i32, i32),
    /// The tile's and the glyph's stencils at rest, and at the magnified
    /// size if there was room for them.
    stencils: [Rect; 2],
    big: Option<[Rect; 2]>,
}

pub struct Dock {
    pane: LayerId,
    /// The pane at rest: left, width.
    pane_at: (i32, u32),
    icons: Vec<Icon>,
    size: u32,
    bounce: bool,
    /// [`Theme::magnify`]: 100 when icons only lift.
    magnify: u32,
    /// The icon the pointer is over.
    hovered: Option<usize>,
}

/// How much of each pixel of a `size`-pixel superellipse tile is covered:
/// `|x|^5 + |y|^5 <= r^5`, sampled four by four.
fn tile(size: u32) -> Vec<u8> {
    let size = size as i64;
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    // In eighths of a pixel from the centre.
    let r = size * 4 - 2;
    let r5 = r.pow(5);
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = (px * 8 + sx * 2 + 1 - size * 4).abs();
                    let y = (py * 8 + sy * 2 + 1 - size * 4).abs();
                    hits += (x.pow(5) + y.pow(5) <= r5) as i64;
                }
            }
            coverage[(py * size + px) as usize] = (hits * 255 / 16) as u8;
        }
    }
    coverage
}

/// A cog with eight teeth and a hole, `size` pixels across.
fn gear(size: u32) -> Vec<u8> {
    let size = size as i64;
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    // Radii in eighths of a pixel: the body, the teeth's tips, the hole.
    let (body, tip, hole) = (size * 8 * 24 / 100, size * 8 * 33 / 100, size * 8 * 10 / 100);
    let tooth = size * 8 * 6 / 100;
    // The eight directions, in 4096ths.
    const DIRS: [(i64, i64); 4] = [(4096, 0), (2896, 2896), (0, 4096), (-2896, 2896)];
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = px * 8 + sx * 2 + 1 - size * 4;
                    let y = py * 8 + sy * 2 + 1 - size * 4;
                    let d2 = x * x + y * y;
                    let in_tooth = d2 <= tip * tip
                        && DIRS
                            .iter()
                            .any(|(dx, dy)| (dx * y - dy * x).abs() / 4096 <= tooth);
                    let solid = (d2 <= body * body || in_tooth) && d2 > hole * hole;
                    hits += solid as i64;
                }
            }
            coverage[(py * size + px) as usize] = (hits * 255 / 16) as u8;
        }
    }
    coverage
}

/// A folder: a tab on top of a body, `size` pixels across.
fn folder(size: u32) -> Vec<u8> {
    let size = size as i64;
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    // In eighths of a pixel: the body, and the tab above its left end.
    let (left, right) = (size * 8 * 22 / 100, size * 8 * 78 / 100);
    let (top, bottom) = (size * 8 * 34 / 100, size * 8 * 72 / 100);
    let (tab_right, tab_top) = (size * 8 * 46 / 100, size * 8 * 28 / 100);
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = px * 8 + sx * 2 + 1;
                    let y = py * 8 + sy * 2 + 1;
                    let body = x >= left && x < right && y >= top && y < bottom;
                    let tab = x >= left && x < tab_right && y >= tab_top && y < top;
                    hits += (body || tab) as i64;
                }
            }
            coverage[(py * size + px) as usize] = (hits * 255 / 16) as u8;
        }
    }
    coverage
}

/// Text centred in a `size`-pixel square.
fn text(size: u32, text: &str) -> Vec<u8> {
    let style = Style::px((size * 52 / 100).max(8)).weight(Weight::Bold);
    let extent = corrode_text::measure(text, style);
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    if extent.width > size || extent.height > size {
        return coverage;
    }
    let mut drawn = alloc::vec![0u8; extent.area()];
    corrode_text::draw(text, style, &mut Coverage::new(&mut drawn, extent.width as usize));
    let (left, top) = ((size - extent.width) / 2, (size - extent.height) / 2);
    for y in 0..extent.height {
        for x in 0..extent.width {
            coverage[((top + y) * size + left + x) as usize] =
                drawn[(y * extent.width + x) as usize];
        }
    }
    coverage
}

/// A round dot, `DOT` pixels across.
fn dot() -> Vec<u8> {
    let size = DOT as i64;
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    let r = size * 4;
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = px * 8 + sx * 2 + 1 - size * 4;
                    let y = py * 8 + sy * 2 + 1 - size * 4;
                    hits += (x * x + y * y <= r * r) as i64;
                }
            }
            coverage[(py * size + px) as usize] = (hits * 255 / 16) as u8;
        }
    }
    coverage
}

impl Dock {
    /// Draw the icons into the atlas and put the dock in the scene, or
    /// `None` if the theme has no dock or there is no room for one.
    pub fn new<R: Renderer, const N: usize>(
        renderer: &mut R,
        scene: &mut Scene<N>,
        theme: &Theme,
    ) -> Option<Self> {
        if !theme.dock {
            return None;
        }
        let size = theme.dock_size as u32;
        let screen = scene.screen();
        let count = APPS.len() as u32;
        let (width, height) = (count * (size + PAD) + PAD, size + 2 * PAD);
        let left = (screen.width as i32 - width as i32) / 2;
        let top = screen.height as i32 - height as i32 - GAP;
        let pane = scene.add(crate::input::pane(
            Rect::new(left, top, width, height),
            (height / 2 - 4) as u16,
            theme,
        ))?;

        let tile_src = Rect::new(ATLAS_X, ATLAS_Y, size, size);
        let dot_src = Rect::new(ATLAS_X, ATLAS_Y + size as i32 + 2, DOT, DOT);
        let stencils = renderer.upload_mask(tile_src, &tile(size)).is_ok()
            && renderer.upload_mask(dot_src, &dot()).is_ok();
        // The magnified tile, shared as the small one is, if the row has room
        // for it and a glyph for every icon.
        let magnify = if theme.speed > 0 { theme.magnify.clamp(100, 200) as u32 } else { 100 };
        let big = size * magnify / 100;
        let big_size = (stencils
            && magnify > 100
            && (count + 1) * big <= crate::ATLAS_WIDTH
            && big <= BIG_ROWS)
            .then_some(big)
            .filter(|big| renderer.upload_mask(Rect::new(0, BIG_Y, *big, *big), &tile(*big)).is_ok());

        let mut icons = Vec::new();
        for (index, app) in APPS.iter().enumerate() {
            let at = (left + (PAD + index as u32 * (size + PAD)) as i32, top + PAD as i32);
            let Some(tile) = stencils
                .then(|| scene.add(Layer::scaled_mask(at.0, at.1, tile_src, app.color)))
                .flatten()
            else {
                // Without stencils, a plain square says as much.
                let tile = scene.add(Layer::solid(Rect::new(at.0, at.1, size, size), app.color))?;
                icons.push(Icon {
                    tile,
                    glyph: None,
                    dot: None,
                    at,
                    running: false,
                    tooltip: None,
                    tip_at: at,
                    stencils: [tile_src; 2],
                    big: None,
                });
                continue;
            };
            let glyph_src = Rect::new(
                ATLAS_X + (index as i32 + 1) * size as i32,
                ATLAS_Y,
                size,
                size,
            );
            let coverage = match app.glyph {
                Glyph::Text(label) => text(size, label),
                Glyph::Gear => gear(size),
                Glyph::Folder => folder(size),
            };
            let glyph = renderer
                .upload_mask(glyph_src, &coverage)
                .ok()
                .and_then(|_| scene.add(Layer::scaled_mask(at.0, at.1, glyph_src, Color::WHITE)));
            let mut dot = Layer::mask(
                at.0 + (size - DOT) as i32 / 2,
                top + height as i32 - DOT as i32 - 1,
                dot_src,
                theme.text(),
            );
            dot.opacity = Animation::rest(Fx::ZERO);
            let dot = scene.add(dot);
            let tooltip = Self::tooltip(renderer, scene, theme, index, app.title, at, size);
            let tip_at = tooltip
                .and_then(|[pill, _]| scene.layer(pill))
                .map_or(at, |pill| (pill.x.target().round(), pill.y.target().round()));
            let big = big_size.and_then(|big| {
                let glyph = Rect::new((index as i32 + 1) * big as i32, BIG_Y, big, big);
                let coverage = match app.glyph {
                    Glyph::Text(label) => text(big, label),
                    Glyph::Gear => gear(big),
                    Glyph::Folder => folder(big),
                };
                renderer.upload_mask(glyph, &coverage).ok()?;
                Some([Rect::new(0, BIG_Y, big, big), glyph])
            });
            icons.push(Icon {
                tile,
                glyph,
                dot,
                at,
                running: false,
                tooltip,
                tip_at,
                stencils: [tile_src, glyph_src],
                big,
            });
        }
        Some(Dock {
            pane,
            pane_at: (left, width),
            icons,
            size,
            bounce: theme.bounce && theme.speed > 0,
            // Growing needs animating to look like anything.
            magnify,
            hovered: None,
        })
    }

    /// Icon `index`'s name on a pill above it, hidden until hovered.
    fn tooltip<R: Renderer, const N: usize>(
        renderer: &mut R,
        scene: &mut Scene<N>,
        theme: &Theme,
        index: usize,
        name: &str,
        at: (i32, i32),
        size: u32,
    ) -> Option<[LayerId; 2]> {
        let extent = corrode_text::measure(name, LABEL);
        if extent.width > LABEL_SLOT || extent.height > 20 {
            return None;
        }
        let mut coverage = alloc::vec![0u8; extent.area()];
        corrode_text::draw(name, LABEL, &mut Coverage::new(&mut coverage, extent.width as usize));
        let src = Rect::new(
            ATLAS_X + index as i32 * LABEL_SLOT as i32,
            LABEL_Y,
            extent.width,
            extent.height,
        );
        renderer.upload_mask(src, &coverage).ok()?;
        let (width, height) = (extent.width + 20, extent.height + 8);
        let x = at.0 + (size as i32 - width as i32) / 2;
        let y = at.1 - PAD as i32 - height as i32 - 10;
        let mut pill = crate::input::pane(Rect::new(x, y, width, height), (height / 2) as u16, theme);
        let mut label = Layer::mask(x + 10, y + 4, src, theme.text());
        pill.opacity = Animation::rest(Fx::ZERO);
        label.opacity = Animation::rest(Fx::ZERO);
        Some([scene.add(pill)?, scene.add(label)?])
    }

    /// Where every icon goes with the pointer over `over`: its left, top
    /// and size; and the pane's left and width.
    ///
    /// Unmagnified, the icon under the pointer only lifts. Magnified, it and
    /// its neighbours grow up from the dock's floor, and the row spreads out
    /// from the hovered icon's middle, which stays put: so the icon still
    /// covers wherever the pointer found it, and growing cannot move it out
    /// from under the pointer.
    fn layout(&self, over: Option<usize>) -> (Vec<(i32, i32, u32)>, (i32, u32)) {
        let size = self.size;
        let rest = || self.icons.iter().map(|icon| (icon.at.0, icon.at.1, size)).collect();
        let Some(over) = over.filter(|over| *over < self.icons.len()) else {
            return (rest(), self.pane_at);
        };
        if self.magnify <= 100 {
            let mut icons: Vec<(i32, i32, u32)> = rest();
            icons[over].1 -= LIFT;
            return (icons, self.pane_at);
        }
        let grown = |index: usize| {
            let share = FALLOFF.get(index.abs_diff(over)).copied().unwrap_or(0);
            size + size * (self.magnify - 100) * share / 10_000
        };
        let floor = self.icons[over].at.1 + size as i32;
        let mut icons: Vec<(i32, i32, u32)> = (0..self.icons.len())
            .map(|index| (0, floor - grown(index) as i32, grown(index)))
            .collect();
        let middle = self.icons[over].at.0 + size as i32 / 2;
        icons[over].0 = middle - icons[over].2 as i32 / 2;
        for index in over + 1..icons.len() {
            icons[index].0 = icons[index - 1].0 + (icons[index - 1].2 + PAD) as i32;
        }
        for index in (0..over).rev() {
            icons[index].0 = icons[index + 1].0 - (icons[index].2 + PAD) as i32;
        }
        let left = icons[0].0 - PAD as i32;
        let (last, _, last_size) = icons[icons.len() - 1];
        let right = last + (last_size + PAD) as i32;
        (icons, (left, (right - left) as u32))
    }

    /// The pointer is over icon `over`, or none: lift or grow it and name
    /// it, and put the last one back.
    pub fn hover<const N: usize>(&mut self, scene: &mut Scene<N>, over: Option<usize>, now: u64) {
        if over == self.hovered {
            return;
        }
        let (places, (left, width)) = self.layout(over);
        if let (Some(over), true) = (over, self.magnify > 100) {
            if let (Some(app), Some(place)) = (APPS.get(over), places.get(over)) {
                println!("dock       : {} grows to {} px", app.title, place.2);
            }
        }
        for (icon, (x, y, size)) in self.icons.iter().zip(places) {
            // A growing icon draws from its big stencils, shrunk to fit; one
            // going back to rest from its own.
            let sources = match icon.big {
                Some(big) if size > self.size => big,
                _ => icon.stencils,
            };
            for (id, src) in [Some(icon.tile), icon.glyph].into_iter().zip(sources) {
                let Some(id) = id else { continue };
                if let Some(layer) = scene.layer_mut(id) {
                    if let corrode_render::scene::Content::ScaledMask { src: shown } = &mut layer.content {
                        *shown = src;
                    }
                    layer.x = layer.x.retarget(now, Fx::from_int(x), Spring::SNAPPY);
                    layer.y = layer.y.retarget(now, Fx::from_int(y), Spring::SNAPPY);
                    layer.width = layer.width.retarget(now, Fx::from_int(size as i32), Spring::SNAPPY);
                    layer.height = layer.height.retarget(now, Fx::from_int(size as i32), Spring::SNAPPY);
                }
            }
            // The dot stays under the icon's middle, on the floor.
            if let Some(layer) = icon.dot.and_then(|id| scene.layer_mut(id)) {
                let middle = x + (size - DOT) as i32 / 2;
                layer.x = layer.x.retarget(now, Fx::from_int(middle), Spring::SNAPPY);
            }
            // The name rides above the icon's top, over its middle.
            let (dx, dy) = (x + size as i32 / 2 - (icon.at.0 + self.size as i32 / 2), y - icon.at.1);
            for (id, inset) in icon.tooltip.into_iter().flatten().zip([(0, 0), (10, 4)]) {
                if let Some(layer) = scene.layer_mut(id) {
                    let (x, y) = (icon.tip_at.0 + inset.0 + dx, icon.tip_at.1 + inset.1 + dy);
                    layer.x = layer.x.retarget(now, Fx::from_int(x), Spring::SNAPPY);
                    layer.y = layer.y.retarget(now, Fx::from_int(y), Spring::SNAPPY);
                }
            }
        }
        if let Some(layer) = scene.layer_mut(self.pane) {
            layer.x = layer.x.retarget(now, Fx::from_int(left), Spring::SNAPPY);
            layer.width = layer.width.retarget(now, Fx::from_int(width as i32), Spring::SNAPPY);
        }
        for (index, shown) in [(self.hovered, false), (over, true)] {
            let Some(icon) = index.and_then(|index| self.icons.get(index)) else {
                continue;
            };
            let opacity = Fx::from_int(if shown { 255 } else { 0 });
            for id in icon.tooltip.into_iter().flatten() {
                if let Some(layer) = scene.layer_mut(id) {
                    let from = layer.opacity.sample(now).value;
                    layer.opacity = Animation::ease(now, 120, from, opacity, corrode_render::anim::Ease::Out);
                }
            }
        }
        self.hovered = over;
    }

    /// Every layer of the dock, bottom first.
    pub fn layers(&self) -> impl Iterator<Item = LayerId> + '_ {
        core::iter::once(self.pane).chain(
            self.icons
                .iter()
                .flat_map(|icon| {
                    [Some(icon.tile), icon.glyph, icon.dot]
                        .into_iter()
                        .flatten()
                        .chain(icon.tooltip.into_iter().flatten())
                }),
        )
    }

    /// Which icon `id` is part of, if any. The pane itself is not an icon.
    pub fn icon(&self, id: LayerId) -> Option<usize> {
        self.icons
            .iter()
            .position(|icon| icon.tile == id || icon.glyph == Some(id))
    }

    /// Icon `index`'s stencils, for the switcher to show it too: the tile,
    /// its glyph and the tile's colour. None if the dock fell back to plain
    /// squares, or there is no such icon.
    pub fn stencils(&self, index: usize) -> Option<(Rect, Option<Rect>, Color)> {
        let (icon, app) = (self.icons.get(index)?, APPS.get(index)?);
        icon.dot?;
        let size = self.size;
        let tile = Rect::new(ATLAS_X, ATLAS_Y, size, size);
        let glyph = icon
            .glyph
            .map(|_| Rect::new(ATLAS_X + (index as i32 + 1) * size as i32, ATLAS_Y, size, size));
        Some((tile, glyph, app.color))
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    /// How many programs the dock knows.
    pub fn apps(&self) -> usize {
        self.icons.len()
    }

    /// Icon `index`'s name stencil, the one its tooltip shows.
    pub fn label<const N: usize>(&self, scene: &Scene<N>, index: usize) -> Option<Rect> {
        let [_, text] = self.icons.get(index)?.tooltip?;
        match scene.layer(text)?.content {
            corrode_render::scene::Content::Mask { src } => Some(src),
            _ => None,
        }
    }

    /// What icon `index`'s program titles its windows.
    /// Where icon `index` rests: its top left.
    pub fn at(&self, index: usize) -> Option<(i32, i32)> {
        self.icons.get(index).map(|icon| icon.at)
    }

    /// The launcher's number for icon `index`'s program.
    pub fn program(&self, index: usize) -> u64 {
        APPS.get(index).map_or(0, |app| app.program)
    }

    /// Whether icon `index`'s program has a window open.
    pub fn is_running(&self, index: usize) -> bool {
        self.icons.get(index).is_some_and(|icon| icon.running)
    }

    pub fn title(&self, index: usize) -> &'static str {
        APPS.get(index).map_or("", |app| app.title)
    }

    /// Which icon starts the program that titles its windows `title`.
    pub fn find(&self, title: &str) -> Option<usize> {
        APPS.iter().position(|app| app.title == title)
    }

    /// Whether `id` is the dock at all.
    pub fn contains(&self, id: LayerId) -> bool {
        self.layers().any(|layer| layer == id)
    }

    /// Start icon `index`'s program: jump, and ask the launcher.
    pub fn launch<const N: usize>(&self, index: usize, scene: &mut Scene<N>, now: u64) {
        let (Some(icon), Some(app)) = (self.icons.get(index), APPS.get(index)) else {
            return;
        };
        if self.bounce {
            let height = (self.size * 45 / 100) as i32;
            // It lands where it rests now: lifted or grown, if the pointer
            // is on it.
            let rest = self.layout(self.hovered).0.get(index).map_or(icon.at.1, |place| place.1);
            for id in [Some(icon.tile), icon.glyph].into_iter().flatten() {
                if let Some(layer) = scene.layer_mut(id) {
                    layer.y = Animation::spring(
                        now,
                        Fx::from_int(rest - height),
                        Fx::from_int(rest),
                        Velocity::ZERO,
                        Spring::BOUNCY,
                    );
                }
            }
        }
        let answer = crate::ipc::call(
            crate::LAUNCHER_ENDPOINT,
            crate::ipc::Message {
                label: launcher::LAUNCH,
                words: [app.program, 0, 0, 0],
            },
            None,
            None,
        );
        match answer {
            Ok(answer) if answer.message.words[0] == 0 => {
                println!("dock       : started {}", app.title)
            }
            Ok(answer) => println!(
                "dock       : the launcher would not start {} ({})",
                app.title, answer.message.words[0]
            ),
            Err(error) => println!("dock       : no launcher: {error:?}"),
        }
    }

    /// Light the dots of the programs that have a window: the titles of
    /// every window there is.
    pub fn running<'a, const N: usize>(
        &mut self,
        scene: &mut Scene<N>,
        titles: impl Iterator<Item = &'a str> + Clone,
    ) {
        for (icon, app) in self.icons.iter_mut().zip(APPS.iter()) {
            let running = titles.clone().any(|title| title == app.title);
            if running == icon.running {
                continue;
            }
            icon.running = running;
            if let Some(layer) = icon.dot.and_then(|id| scene.layer_mut(id)) {
                layer.opacity = Animation::rest(Fx::from_int(if running { 255 } else { 0 }));
            }
        }
    }
}
